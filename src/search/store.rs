mod global;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::corpus::{Corpus, Coverage, Descriptor, Route};
use super::{chunk, Mode};
use crate::error::{AIexError, Result};

#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

fn db_error(error: rusqlite::Error) -> AIexError {
    if matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    ) {
        AIexError::RefreshPending
    } else {
        AIexError::Other(format!("search storage: {error}"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub file: String,
    pub revision: String,
    pub offset: usize,
    pub limit: usize,
    pub excerpt: String,
    pub read: Route,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| AIexError::Other("invalid search cache path".into()))?,
        )?;
        let store = Self { path: path.into() };
        let db = store.connection()?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(db_error)?;
        if !matches!(version, 0 | 1) {
            return Err(AIexError::Other("unsupported search cache version".into()));
        }
        db.execute_batch("
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS sources(id TEXT PRIMARY KEY, generation TEXT NOT NULL, descriptor TEXT NOT NULL, coverage TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS chunks(id INTEGER PRIMARY KEY, scope TEXT NOT NULL, key TEXT NOT NULL, file TEXT NOT NULL, title TEXT NOT NULL, first_line INTEGER NOT NULL, last_line INTEGER NOT NULL, body TEXT NOT NULL, input TEXT NOT NULL, route TEXT NOT NULL, revision TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS chunks_scope ON chunks(scope);
            CREATE VIRTUAL TABLE IF NOT EXISTS terms USING fts5(title, body, scope UNINDEXED);
            CREATE TABLE IF NOT EXISTS models(id TEXT PRIMARY KEY, dimensions INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS vectors(model TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(model,key));
            CREATE TABLE IF NOT EXISTS failures(scope TEXT NOT NULL, model TEXT NOT NULL, message TEXT NOT NULL, retry_ms INTEGER NOT NULL, PRIMARY KEY(scope,model));
            PRAGMA user_version=1;
        ").map_err(db_error)?;
        Ok(store)
    }

    fn connection(&self) -> Result<Connection> {
        let db = Connection::open(&self.path).map_err(db_error)?;
        db.busy_timeout(Duration::from_secs(2)).map_err(db_error)?;
        Ok(db)
    }

    pub fn descriptor(&self, id: &str) -> Result<Option<Descriptor>> {
        let db = self.connection()?;
        let raw: Option<String> = db
            .query_row("SELECT descriptor FROM sources WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_error)?;
        raw.map(|s| serde_json::from_str(&s).map_err(|e| AIexError::Other(e.to_string())))
            .transpose()
    }

    pub fn dimensions(&self, model: &str) -> Result<Option<usize>> {
        self.connection()?
            .query_row("SELECT dimensions FROM models WHERE id=?1", [model], |r| {
                r.get::<_, i64>(0)
            })
            .optional()
            .map_err(db_error)
            .map(|v| v.map(|v| v as usize))
    }

    pub fn descriptors(&self) -> Result<Vec<Descriptor>> {
        let db = self.connection()?;
        let mut q = db
            .prepare("SELECT descriptor FROM sources ORDER BY id")
            .map_err(db_error)?;
        let rows = q
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(db_error)?;
        rows.map(|r| {
            serde_json::from_str(&r.map_err(db_error)?).map_err(|e| AIexError::Other(e.to_string()))
        })
        .collect()
    }

    /// Atomically replace membership/FTS rows; content-addressed vectors survive.
    pub fn publish(&self, corpus: &Corpus, chunk_bytes: usize) -> Result<bool> {
        let mut db = self.connection()?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let generation: Option<String> = tx
            .query_row(
                "SELECT generation FROM sources WHERE id=?1",
                [&corpus.descriptor.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let changed = generation.as_deref() != Some(&corpus.generation);
        if changed {
            tx.execute("DELETE FROM terms WHERE scope=?1", [&corpus.descriptor.id])
                .map_err(db_error)?;
            tx.execute("DELETE FROM chunks WHERE scope=?1", [&corpus.descriptor.id])
                .map_err(db_error)?;
            for doc in &corpus.documents {
                for part in chunk::chunks(&doc.title, &doc.body, chunk_bytes) {
                    tx.execute("INSERT INTO chunks(scope,key,file,title,first_line,last_line,body,input,route,revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![
                        corpus.descriptor.id, part.key, doc.file, doc.title, part.start as i64, part.end as i64, part.text, part.input,
                        serde_json::to_string(&doc.route).unwrap(), doc.revision,
                    ]).map_err(db_error)?;
                    tx.execute(
                        "INSERT INTO terms(rowid,title,body,scope) VALUES(?1,?2,?3,?4)",
                        params![
                            tx.last_insert_rowid(),
                            doc.title,
                            part.text,
                            corpus.descriptor.id
                        ],
                    )
                    .map_err(db_error)?;
                }
            }
        }
        tx.execute("INSERT INTO sources(id,generation,descriptor,coverage) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation, descriptor=excluded.descriptor, coverage=excluded.coverage", params![
            corpus.descriptor.id, corpus.generation, serde_json::to_string(&corpus.descriptor).unwrap(), serde_json::to_string(&corpus.coverage).unwrap(),
        ]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(changed)
    }

    /// Remove obsolete memberships only after a complete inventory of active roots.
    pub fn prune(&self, active: &[String]) -> Result<()> {
        let mut db = self.connection()?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(db_error)?;
        tx.execute_batch("CREATE TEMP TABLE active_sources(id TEXT PRIMARY KEY);")
            .map_err(db_error)?;
        for id in active {
            tx.execute("INSERT INTO active_sources VALUES(?1)", [id])
                .map_err(db_error)?;
        }
        tx.execute_batch(
            "
            DELETE FROM terms WHERE scope NOT IN (SELECT id FROM active_sources);
            DELETE FROM chunks WHERE scope NOT IN (SELECT id FROM active_sources);
            DELETE FROM failures WHERE scope NOT IN (SELECT id FROM active_sources);
            DELETE FROM sources WHERE id NOT IN (SELECT id FROM active_sources);
        ",
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    /// Keep only vectors used by current memberships and the configured model.
    /// Deleted SQLite pages are reused by subsequent generations.
    pub fn collect_vectors(&self, model: Option<&str>) -> Result<()> {
        let mut db = self.connection()?;
        let tx = db.transaction().map_err(db_error)?;
        tx.execute(
            "DELETE FROM vectors WHERE model IS NOT ?1 OR key NOT IN (SELECT key FROM chunks)",
            [model],
        )
        .map_err(db_error)?;
        tx.execute("DELETE FROM models WHERE id IS NOT ?1", [model])
            .map_err(db_error)?;
        tx.execute("DELETE FROM failures WHERE model IS NOT ?1", [model])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    pub fn missing(&self, scope: &str, model: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let db = self.connection()?;
        let mut q = db.prepare("SELECT DISTINCT c.key,c.input FROM chunks c LEFT JOIN vectors v ON v.key=c.key AND v.model=?2 WHERE c.scope=?1 AND v.key IS NULL ORDER BY c.key LIMIT ?3").map_err(db_error)?;
        let rows = q
            .query_map(params![scope, model, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error);
        rows
    }

    pub fn insert_vectors(&self, model: &str, keys: &[String], vectors: &[Vec<f32>]) -> Result<()> {
        if keys.is_empty() || keys.len() != vectors.len() {
            return Err(AIexError::Other("invalid embedding batch".into()));
        }
        let dimensions = vectors[0].len();
        if vectors.iter().any(|v| v.len() != dimensions) {
            return Err(AIexError::Other("embedding dimension mismatch".into()));
        }
        let mut db = self.connection()?;
        let tx = db.transaction().map_err(db_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO models(id,dimensions) VALUES(?1,?2)",
            params![model, dimensions as i64],
        )
        .map_err(db_error)?;
        let expected: i64 = tx
            .query_row("SELECT dimensions FROM models WHERE id=?1", [model], |r| {
                r.get(0)
            })
            .map_err(db_error)?;
        if expected != dimensions as i64 {
            return Err(AIexError::Other(
                "embedding dimensions changed; select a new model revision".into(),
            ));
        }
        for (key, vector) in keys.iter().zip(vectors) {
            let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
            tx.execute(
                "INSERT OR IGNORE INTO vectors(model,key,value) VALUES(?1,?2,?3)",
                params![model, key, bytes],
            )
            .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    pub fn failure(&self, scope: &str, model: &str) -> Result<Option<String>> {
        self.connection()?
            .query_row(
                "SELECT message FROM failures WHERE scope=?1 AND model=?2 AND retry_ms>?3",
                params![scope, model, crate::cache::now_ms() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    pub fn set_failure(&self, scope: &str, model: &str, error: Option<&str>) -> Result<()> {
        let db = self.connection()?;
        if let Some(error) = error {
            db.execute("INSERT INTO failures(scope,model,message,retry_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(scope,model) DO UPDATE SET message=excluded.message,retry_ms=excluded.retry_ms", params![scope,model,error,(crate::cache::now_ms()+30_000) as i64]).map_err(db_error)?;
        } else {
            db.execute(
                "DELETE FROM failures WHERE scope=?1 AND model=?2",
                params![scope, model],
            )
            .map_err(db_error)?;
        }
        Ok(())
    }

    /// All candidate selection occurs inside the selected membership generation.
    pub fn query(
        &self,
        corpus: &Corpus,
        query: &str,
        mode: Mode,
        vector: Option<(&str, &[f32])>,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let db = self.connection()?;
        // A read transaction prevents a concurrent publication from mixing rows.
        db.execute_batch("BEGIN DEFERRED").map_err(db_error)?;
        let actual: Option<String> = db
            .query_row(
                "SELECT generation FROM sources WHERE id=?1",
                [&corpus.descriptor.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if actual.as_deref() != Some(&corpus.generation) {
            return Err(AIexError::RefreshPending);
        }
        let mut lexical = Vec::<i64>::new();
        let mut dense = Vec::<(i64, f64)>::new();
        if mode != Mode::Semantic {
            let words: Vec<_> = query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
                .take(32)
                .map(|s| format!("\"{s}\""))
                .collect();
            if !words.is_empty() {
                let mut q = db.prepare("SELECT rowid FROM terms WHERE scope=?1 AND terms MATCH ?2 ORDER BY bm25(terms,3.0,1.0),rowid LIMIT 100").map_err(db_error)?;
                lexical = q
                    .query_map(params![corpus.descriptor.id, words.join(" OR ")], |r| {
                        r.get(0)
                    })
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db_error)?;
            }
        }
        if let Some((model, vector)) = vector {
            let mut q = db.prepare("SELECT c.id,v.value FROM chunks c JOIN vectors v ON v.key=c.key AND v.model=?2 WHERE c.scope=?1").map_err(db_error)?;
            let rows = q
                .query_map(params![corpus.descriptor.id, model], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_error)?;
            for row in rows {
                let (id, bytes) = row.map_err(db_error)?;
                if bytes.len() != vector.len() * 4 {
                    return Err(AIexError::Other(
                        "query/document embedding dimensions differ".into(),
                    ));
                }
                let score: f64 = bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(vector)
                    .map(|(b, q)| f64::from(f32::from_le_bytes(*b)) * f64::from(*q))
                    .sum();
                if !score.is_finite() {
                    return Err(AIexError::Other("invalid cached embedding".into()));
                }
                dense.push((id, score));
            }
            dense.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            dense.truncate(100);
        }
        let mut scores = BTreeMap::<i64, f64>::new();
        for (i, id) in lexical.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (60.0 + i as f64 + 1.0);
        }
        for (i, (id, _)) in dense.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (60.0 + i as f64 + 1.0);
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut hits: Vec<Hit> = Vec::new();
        for (id, _) in ranked {
            let (file, revision, start, end, body, route): (
                String,
                String,
                i64,
                i64,
                String,
                String,
            ) = db
                .query_row(
                    "SELECT file,revision,first_line,last_line,body,route FROM chunks WHERE id=?1",
                    [id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .map_err(db_error)?;
            if start < 1 || end < start {
                return Err(AIexError::Other("invalid cached passage location".into()));
            }
            let (start, end) = (start as usize, end as usize);
            let route: Route =
                serde_json::from_str(&route).map_err(|e| AIexError::Other(e.to_string()))?;
            if hits
                .iter()
                .filter(|h| h.file == file && h.read == route)
                .count()
                >= 2
            {
                continue;
            }
            hits.push(Hit {
                file,
                revision,
                offset: start,
                limit: end - start + 1,
                excerpt: body,
                read: route,
            });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    pub fn generation(&self, id: &str) -> Result<Option<String>> {
        self.connection()?
            .query_row("SELECT generation FROM sources WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_error)
    }

    pub fn coverage(&self, id: &str) -> Result<Option<Coverage>> {
        let raw: Option<String> = self
            .connection()?
            .query_row("SELECT coverage FROM sources WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_error)?;
        raw.map(|s| serde_json::from_str(&s).map_err(|e| AIexError::Other(e.to_string())))
            .transpose()
    }
}
