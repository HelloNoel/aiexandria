use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use super::{db_error, Hit, Store};
use crate::error::{AIexError, Result};
use crate::search::global::{QueryOptions, QueryResult, Source};
use crate::search::Mode;

impl Store {
    pub fn query_global(
        &self,
        sources: &[Source],
        query: &str,
        mode: Mode,
        vector: Option<(&str, &[f32])>,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        let deadline = options.deadline;
        let result = self.query_global_inner(sources, query, mode, vector, options);
        match result {
            Err(_) if Instant::now() >= deadline => Ok(QueryResult {
                timed_out: true,
                limited: true,
                ..Default::default()
            }),
            other => other,
        }
    }

    fn query_global_inner(
        &self,
        sources: &[Source],
        query: &str,
        mode: Mode,
        vector: Option<(&str, &[f32])>,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        if Instant::now() >= options.deadline {
            return Ok(QueryResult {
                timed_out: true,
                limited: true,
                ..Default::default()
            });
        }
        let mut db = self.connection()?;
        db.busy_timeout(
            options
                .deadline
                .saturating_duration_since(Instant::now())
                .min(std::time::Duration::from_secs(2)),
        )
        .map_err(db_error)?;
        let deadline = options.deadline;
        db.progress_handler(1000, Some(move || Instant::now() >= deadline))
            .map_err(db_error)?;
        // All membership checks, deduplication and rankings share one SQLite snapshot.
        db.execute_batch("BEGIN DEFERRED;
            CREATE TEMP TABLE allowed_docs(scope TEXT, file TEXT, revision TEXT, repository TEXT, PRIMARY KEY(scope,file,revision));
            CREATE TEMP TABLE candidates(id INTEGER PRIMARY KEY, repository TEXT);").map_err(db_error)?;
        let mut result = QueryResult::default();
        let mut descriptors = HashMap::new();
        for source in sources {
            let actual: Option<String> = db
                .query_row(
                    "SELECT generation FROM sources WHERE id=?1",
                    [&source.descriptor.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_error)?;
            if actual.as_deref() != Some(&source.generation) {
                result.changed_sources += 1;
                continue;
            }
            result.sources_searched += 1;
            descriptors.insert(source.descriptor.id.as_str(), &source.descriptor);
            for doc in &source.documents {
                db.execute(
                    "INSERT OR IGNORE INTO allowed_docs VALUES(?1,?2,?3,?4)",
                    params![source.descriptor.id, doc.file, doc.revision, doc.repository],
                )
                .map_err(db_error)?;
            }
        }
        // A commit identifies its tree: aliases/forks with the same revision and
        // passage location are the same evidence. Deduplicate BEFORE top-k ranking.
        db.execute("INSERT INTO candidates SELECT MIN(c.id),MIN(d.repository)
            FROM chunks c JOIN allowed_docs d ON c.scope=d.scope AND c.file=d.file AND c.revision=d.revision
            GROUP BY c.revision,c.file,c.first_line,c.last_line
            ORDER BY c.revision,c.file,c.first_line LIMIT ?1", [(options.max_chunks + 1) as i64]).map_err(db_error)?;
        let count: i64 = db
            .query_row("SELECT count(*) FROM candidates", [], |r| r.get(0))
            .map_err(db_error)?;
        result.limited = count as usize > options.max_chunks;
        if result.limited {
            db.execute(
                "DELETE FROM candidates WHERE id=(SELECT max(id) FROM candidates)",
                [],
            )
            .map_err(db_error)?;
        }
        result.chunks_searched = (count as usize).min(options.max_chunks);
        result.documents_searched = db.query_row("SELECT count(*) FROM (SELECT c.revision,c.file FROM chunks c JOIN candidates e ON c.id=e.id GROUP BY c.revision,c.file)", [], |r| r.get::<_,i64>(0)).map_err(db_error)? as usize;
        let mut lexical = Vec::<i64>::new();
        if mode != Mode::Semantic {
            let words: Vec<_> = query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|s| !s.is_empty())
                .take(32)
                .map(|s| format!("\"{s}\""))
                .collect();
            if !words.is_empty() {
                let mut q = db.prepare("SELECT terms.rowid FROM terms JOIN candidates e ON terms.rowid=e.id WHERE terms MATCH ?1 ORDER BY bm25(terms,3.0,1.0),terms.rowid LIMIT 1000").map_err(db_error)?;
                lexical = q
                    .query_map([words.join(" OR ")], |r| r.get(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db_error)?;
            }
        }
        let mut dense = Vec::<(i64, f64)>::new();
        if let Some((model, vector)) = vector {
            let mut q = db.prepare("SELECT c.id,v.value FROM chunks c JOIN candidates e ON c.id=e.id JOIN vectors v ON c.key=v.key AND v.model=?1").map_err(db_error)?;
            let rows = q
                .query_map([model], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_error)?;
            for row in rows {
                if Instant::now() >= deadline {
                    return Ok(QueryResult {
                        timed_out: true,
                        limited: true,
                        ..result
                    });
                }
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
        }
        result.embedded_chunks = dense.len();
        dense.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        dense.truncate(1000);
        let mut scores = BTreeMap::<i64, f64>::new();
        for (rank, id) in lexical.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (61.0 + rank as f64);
        }
        for (rank, (id, _)) in dense.iter().enumerate() {
            *scores.entry(*id).or_default() += 1.0 / (61.0 + rank as f64);
        }
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut files = HashMap::<(String, String), usize>::new();
        let mut repositories = HashMap::<String, usize>::new();
        let mut deferred = Vec::new();
        let repository_limit = options.limit.div_ceil(2);
        for (id, _) in ranked {
            if Instant::now() >= deadline {
                return Ok(QueryResult {
                    timed_out: true,
                    limited: true,
                    ..result
                });
            }
            let (file,revision,start,end,body,route,scope,repository): (String,String,i64,i64,String,String,String,String) = db.query_row(
                "SELECT c.file,c.revision,c.first_line,c.last_line,c.body,c.route,c.scope,e.repository FROM chunks c JOIN candidates e ON c.id=e.id WHERE c.id=?1", [id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).map_err(db_error)?;
            if start < 1 || end < start {
                return Err(AIexError::Other("invalid cached passage location".into()));
            }
            let key = (revision.clone(), file.clone());
            if files.get(&key).copied().unwrap_or(0) >= 2 {
                continue;
            }
            let descriptor = descriptors[scope.as_str()];
            let hit = Hit {
                file,
                revision,
                offset: start as usize,
                limit: (end - start + 1) as usize,
                excerpt: body,
                read: serde_json::from_str(&route).map_err(|e| AIexError::Other(e.to_string()))?,
            };
            let mut value =
                serde_json::to_value(hit).map_err(|e| AIexError::Other(e.to_string()))?;
            value["source"] = json!({"name":descriptor.name.chars().take(120).collect::<String>(),"search":
                if descriptor.repo.is_some() { json!({"source_id":descriptor.id}) }
                else { json!({"index":descriptor.route.index,"path":descriptor.route.path}) }});
            if repositories.get(&repository).copied().unwrap_or(0) >= repository_limit {
                deferred.push((value, key));
                continue;
            }
            *repositories.entry(repository).or_default() += 1;
            *files.entry(key).or_default() += 1;
            result.hits.push(value);
            if result.hits.len() > options.limit {
                break;
            }
        }
        // Prefer other repositories, but do not discard useful same-repository
        // passages when the query has no matching alternatives.
        for (value, key) in deferred {
            if result.hits.len() > options.limit {
                break;
            }
            if files.get(&key).copied().unwrap_or(0) >= 2 {
                continue;
            }
            *files.entry(key).or_default() += 1;
            result.hits.push(value);
        }
        result.truncated = result.hits.len() > options.limit;
        result.hits.truncate(options.limit);
        Ok(result)
    }
}
