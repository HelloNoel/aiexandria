//! Incremental source-metadata vectors. Membership always comes from the current
//! authorized catalog snapshot, never from whichever vectors happen to exist.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::cache::{now_ms, try_lock};
use crate::catalog::Source;
use crate::error::{AIexError, Result};
use crate::search::config::EmbeddingConfig;
use crate::search::provider::{diagnostic, failure};

const TEMPLATE: &str = "source-metadata-v1";
const BATCH: usize = 8;
const JOB_BUDGET: Duration = Duration::from_secs(60);
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;

pub(super) fn digest(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

pub(super) fn key(source: &Source) -> String {
    digest(format!("{TEMPLATE}\n{}", source.embedding_text))
}

pub(super) struct MetadataIndex {
    path: PathBuf,
    pub provider: EmbeddingConfig,
    model: String,
    running: AtomicBool,
}

impl MetadataIndex {
    /// Constructing discovery does not open the database or contact a provider.
    pub fn new(directory: &Path, provider: EmbeddingConfig) -> Arc<Self> {
        Arc::new(Self {
            path: directory.join("metadata.sqlite"),
            model: provider.identity(),
            provider,
            running: AtomicBool::new(false),
        })
    }

    fn connection(&self) -> Result<Connection> {
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        let db = Connection::open(&self.path).map_err(storage_error)?;
        db.busy_timeout(Duration::from_millis(100))
            .map_err(storage_error)?;
        let version: i64 = db
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(storage_error)?;
        if !matches!(version, 0 | 1) {
            return Err(failure(
                "metadata_cache_version",
                "Unsupported discovery metadata cache version.",
                false,
            ));
        }
        if version == 0 {
            db.execute_batch("
                PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS models(id TEXT PRIMARY KEY, dimensions INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS vectors(model TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, used_ms INTEGER NOT NULL, PRIMARY KEY(model,key));
                CREATE TABLE IF NOT EXISTS failures(model TEXT PRIMARY KEY, diagnostic TEXT NOT NULL, retry_ms INTEGER NOT NULL);
                PRAGMA user_version=1;
            ").map_err(storage_error)?;
        }
        Ok(db)
    }

    pub fn identity(&self, sources: &[Arc<Source>]) -> String {
        let members: BTreeMap<_, _> = sources.iter().map(|s| (&s.id, key(s))).collect();
        digest(serde_json::to_vec(&(&self.model, TEMPLATE, members)).unwrap())
    }

    pub fn ready_count(&self, sources: &[Arc<Source>]) -> Result<usize> {
        let db = self.connection()?;
        let mut statement = db
            .prepare("SELECT 1 FROM vectors WHERE model=?1 AND key=?2")
            .map_err(storage_error)?;
        let mut count = 0;
        for source in sources {
            count += usize::from(
                statement
                    .exists(params![self.model, key(source)])
                    .map_err(storage_error)?,
            );
        }
        Ok(count)
    }

    fn cached_failure(&self) -> Result<Option<Value>> {
        let db = self.connection()?;
        let raw: Option<String> = db
            .query_row(
                "SELECT diagnostic FROM failures WHERE model=?1 AND retry_ms>?2",
                params![self.model, now_ms() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        raw.map(|s| {
            serde_json::from_str(&s).map_err(|_| {
                failure(
                    "metadata_cache_invalid",
                    "Invalid cached discovery diagnostic.",
                    false,
                )
            })
        })
        .transpose()
    }

    fn set_failure(&self, error: Option<&AIexError>) -> Result<()> {
        let db = self.connection()?;
        if let Some(error) = error {
            let detail = diagnostic(error);
            let delay = if detail["code"] == "provider_starting"
                || detail["code"] == "discovery_preparing"
            {
                1000
            } else {
                30_000
            };
            db.execute("INSERT INTO failures(model,diagnostic,retry_ms) VALUES(?1,?2,?3) ON CONFLICT(model) DO UPDATE SET diagnostic=excluded.diagnostic,retry_ms=excluded.retry_ms",
                params![self.model, detail.to_string(), (now_ms() + delay) as i64]).map_err(storage_error)?;
        } else {
            db.execute("DELETE FROM failures WHERE model=?1", [&self.model])
                .map_err(storage_error)?;
        }
        Ok(())
    }

    /// Request work once per process; an OS lock also coalesces different MCP
    /// processes. The job is independent of a caller's wait/cancellation.
    fn request(self: &Arc<Self>, sources: &[Arc<Source>]) {
        if self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let index = self.clone();
        let sources = sources.to_vec();
        std::thread::spawn(move || {
            struct Running(Arc<MetadataIndex>);
            impl Drop for Running {
                fn drop(&mut self) {
                    self.0.running.store(false, Ordering::Release);
                }
            }
            let _running = Running(index.clone());
            if let Err(error) = index.prepare(&sources) {
                let _ = index.set_failure(Some(&error));
            }
        });
    }

    fn prepare(&self, sources: &[Arc<Source>]) -> Result<()> {
        let Some(_worker) = try_lock(&self.path.with_extension("lock"))? else {
            return Ok(());
        };
        let deadline = Instant::now() + JOB_BUDGET;
        let db = self.connection()?;
        // Removed routes cannot be returned: queries provide their own complete
        // membership. Keep unreferenced vectors briefly to reuse identical refreshes.
        let mut inputs = BTreeMap::new();
        for source in sources {
            inputs
                .entry(key(source))
                .or_insert_with(|| source.embedding_text.clone());
        }
        let mut missing = Vec::new();
        {
            let mut touch = db
                .prepare("UPDATE vectors SET used_ms=?3 WHERE model=?1 AND key=?2")
                .map_err(storage_error)?;
            for (key, input) in inputs {
                if touch
                    .execute(params![self.model, key, now_ms() as i64])
                    .map_err(storage_error)?
                    == 0
                {
                    missing.push((key, input));
                }
            }
        }
        // Touch the selected membership before collecting old entries, so a
        // long-idle but still authorized source is not needlessly re-embedded.
        db.execute(
            "DELETE FROM vectors WHERE used_ms<?1",
            [now_ms().saturating_sub(RETENTION_MS) as i64],
        )
        .map_err(storage_error)?;
        drop(db);
        for batch in missing.chunks(BATCH) {
            if Instant::now() >= deadline {
                break;
            }
            let texts: Vec<_> = batch.iter().map(|(_, input)| input.clone()).collect();
            let vectors = self.provider.embed_with_timeout(
                &texts,
                false,
                deadline.saturating_duration_since(Instant::now()),
            )?;
            self.insert(batch, &vectors)?;
        }
        self.set_failure(None)
    }

    fn insert(&self, inputs: &[(String, String)], vectors: &[Vec<f32>]) -> Result<()> {
        let Some(first) = vectors.first() else {
            return Err(failure(
                "invalid_embedding_response",
                "Empty metadata embedding batch.",
                false,
            ));
        };
        if inputs.len() != vectors.len() || vectors.iter().any(|v| v.len() != first.len()) {
            return Err(failure(
                "invalid_embedding_response",
                "Metadata embedding batch dimensions differ.",
                false,
            ));
        }
        let mut db = self.connection()?;
        let tx = db.transaction().map_err(storage_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO models(id,dimensions) VALUES(?1,?2)",
            params![self.model, first.len() as i64],
        )
        .map_err(storage_error)?;
        let expected: i64 = tx
            .query_row(
                "SELECT dimensions FROM models WHERE id=?1",
                [&self.model],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if first.len() as i64 != expected {
            return Err(failure(
                "dimension_mismatch",
                "Embedding dimensions changed; select a new model revision.",
                false,
            ));
        }
        for ((key, _), vector) in inputs.iter().zip(vectors) {
            let bytes: Vec<_> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
            tx.execute("INSERT INTO vectors(model,key,value,used_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(model,key) DO UPDATE SET used_ms=excluded.used_ms", params![self.model, key, bytes, now_ms() as i64]).map_err(storage_error)?;
        }
        tx.commit().map_err(storage_error)
    }

    pub fn ensure(self: &Arc<Self>, sources: &[Arc<Source>], deadline: Instant) -> Result<Value> {
        let total = sources.len();
        let truncated = sources.iter().filter(|s| s.metadata_truncated).count();
        loop {
            let ready = self.ready_count(sources)?;
            if ready == total {
                return Ok(
                    json!({"status":"ready", "ready_sources":ready,"total_sources":total,"truncated_metadata":truncated}),
                );
            }
            if let Some(mut error) = self.cached_failure()? {
                // A provider explicitly started or repaired elsewhere can recover
                // immediately, without waiting for the cached failure to expire.
                if !self.running.load(Ordering::Acquire)
                    && Instant::now() < deadline
                    && self
                        .provider
                        .probe(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(150)),
                        )
                        .is_ok()
                {
                    self.set_failure(None)?;
                } else {
                    if (error["code"] == "provider_starting"
                        || error["code"] == "discovery_preparing")
                        && Instant::now() < deadline
                    {
                        std::thread::sleep(
                            Duration::from_millis(20)
                                .min(deadline.saturating_duration_since(Instant::now())),
                        );
                        continue;
                    }
                    error["status"] = json!(if error["code"] == "discovery_preparing" {
                        "preparing"
                    } else {
                        "unavailable"
                    });
                    error["ready_sources"] = json!(ready);
                    error["total_sources"] = json!(total);
                    error["truncated_metadata"] = json!(truncated);
                    return Ok(error);
                }
            }
            self.request(sources);
            if Instant::now() >= deadline {
                let mut state = json!({"status":"preparing","ready_sources":ready,"total_sources":total,"truncated_metadata":truncated});
                if crate::search::lifecycle::is_starting(&self.provider) {
                    state["code"] = json!("provider_starting");
                    state["message"] = json!(
                        "The local embedding provider is starting; retry after startup completes."
                    );
                }
                return Ok(state);
            }
            std::thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    pub fn rank(
        &self,
        sources: &[Arc<Source>],
        query: &str,
        deadline: Instant,
    ) -> Result<Vec<Arc<Source>>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        if Instant::now() >= deadline {
            return Err(failure(
                "discovery_preparing",
                "Discovery reached its foreground wait budget; retry shortly.",
                true,
            ));
        }
        let query_vector = &self.provider.embed_with_timeout(
            &[query.into()],
            true,
            deadline.saturating_duration_since(Instant::now()),
        )?[0];
        let db = self.connection()?;
        let mut statement = db
            .prepare("SELECT value FROM vectors WHERE model=?1 AND key=?2")
            .map_err(storage_error)?;
        let mut scores = Vec::new();
        // Only one stored vector is materialized at a time, including for large
        // catalogs and high-dimensional models. No out-of-scope candidate scan.
        for source in sources {
            if Instant::now() >= deadline {
                return Err(failure(
                    "discovery_preparing",
                    "Discovery ranking reached its wait budget; narrow the index or kind filters.",
                    true,
                ));
            }
            let bytes: Vec<u8> = statement
                .query_row(params![self.model, key(source)], |r| r.get(0))
                .map_err(storage_error)?;
            if bytes.len() != query_vector.len() * 4 {
                return Err(failure("dimension_mismatch", "Query and source metadata embedding dimensions differ; check the model revision.", false));
            }
            let score: f64 = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .zip(query_vector)
                .map(|(b, q)| f64::from(f32::from_le_bytes(*b)) * f64::from(*q))
                .sum();
            if !score.is_finite() {
                return Err(failure(
                    "invalid_embedding_response",
                    "Invalid cached metadata vector.",
                    false,
                ));
            }
            scores.push((source.clone(), score));
        }
        scores.sort_by(|(a, x), (b, y)| y.total_cmp(x).then_with(|| a.id.cmp(&b.id)));
        // Keep frequently queried vectors alive even when no new metadata needs
        // embedding. Housekeeping contention must not discard a completed rank.
        let keys: Vec<_> = sources.iter().map(|source| key(source)).collect();
        let _ = db.execute("UPDATE vectors SET used_ms=?1 WHERE model=?2 AND used_ms<?3 AND key IN (SELECT value FROM json_each(?4))",
            params![now_ms() as i64, self.model, now_ms().saturating_sub(60_000) as i64, serde_json::to_string(&keys).unwrap()]);
        Ok(scores.into_iter().map(|(source, _)| source).collect())
    }
}

fn storage_error(error: rusqlite::Error) -> AIexError {
    let busy = matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    );
    failure(
        if busy {
            "discovery_preparing"
        } else {
            "metadata_cache_failed"
        },
        if busy {
            "Discovery metadata cache is busy; retry shortly."
        } else {
            "Discovery metadata cache could not be read or updated."
        },
        busy,
    )
}

pub(super) fn fuse(
    lexical: Vec<Arc<Source>>,
    semantic: Vec<Arc<Source>>,
    query: &str,
) -> Vec<Arc<Source>> {
    let mut scores = BTreeMap::new();
    for list in [lexical, semantic] {
        let mut seen = BTreeSet::new();
        for (position, source) in list.into_iter().enumerate() {
            if seen.insert(source.id.clone()) {
                let entry = scores.entry(source.id.clone()).or_insert((source, 0.0));
                entry.1 += 1.0 / (60.0 + position as f64 + 1.0);
            }
        }
    }
    let name = crate::catalog::tokenize(query).join(" ");
    let mut ranked: Vec<_> = scores.into_values().collect();
    ranked.sort_by(|(a, x), (b, y)| {
        (b.normalized_name == name)
            .cmp(&(a.normalized_name == name))
            .then_with(|| y.total_cmp(x))
            .then_with(|| a.id.cmp(&b.id))
    });
    ranked.into_iter().map(|(source, _)| source).collect()
}
