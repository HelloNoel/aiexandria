//! Optional retrieval over cached documentation, with revision-bound membership.
mod chunk;
pub mod config;
pub(crate) mod corpus;
pub(crate) mod global;
mod provider;
mod store;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache::{lock_file, try_lock, Freshness, RepoCache};
use crate::error::{AIexError, Result};
use crate::registry::RegistryConfig;
use config::SearchConfig;
use corpus::{Corpus, Descriptor};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Keyword,
    Hybrid,
    Semantic,
}

struct Inner {
    config: SearchConfig,
    registry: Arc<RegistryConfig>,
    cache: Arc<RepoCache>,
    store: store::Store,
    locks: PathBuf,
    wake: Mutex<bool>,
    condition: Condvar,
}

#[derive(Clone)]
pub struct SearchService {
    inner: Arc<Inner>,
}

pub struct Prepared {
    service: SearchService,
    corpus: Corpus,
}

#[derive(Debug, Default, Serialize)]
pub struct UpdateReport {
    pub unavailable_levels: usize,
    pub inventory_limited: bool,
    pub partial_sources: usize,
    pub indexed_sources: usize,
    pub changed_sources: usize,
    pub embedded_chunks: usize,
    pub pending_sources: usize,
    pub errors: Vec<String>,
}

impl SearchService {
    pub fn new(
        config: SearchConfig,
        registry: Arc<RegistryConfig>,
        cache: Arc<RepoCache>,
    ) -> Result<Self> {
        config.validate()?;
        // Registries may expose different roots under the same names in one cache.
        // Retrieval budgets do not change document membership or embedding identity.
        let mut corpus_config = serde_json::to_value(&config).unwrap();
        for field in [
            "global_max_sources",
            "global_max_chunks",
            "global_timeout_secs",
        ] {
            corpus_config.as_object_mut().unwrap().remove(field);
        }
        let identity = chunk::digest(serde_json::to_vec(&(&*registry, corpus_config)).unwrap());
        let locks = cache.root().join("search");
        let store = store::Store::open(&locks.join(identity).join("search.sqlite"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                registry,
                cache,
                store,
                locks,
                wake: Mutex::new(true),
                condition: Condvar::new(),
            }),
        })
    }

    /// One lightweight coordinator per MCP process. SQLite and file locks coalesce
    /// work across processes; no daemon or model download is started here.
    pub fn start(&self) {
        let weak = Arc::downgrade(&self.inner);
        std::thread::spawn(move || {
            while let Some(inner) = weak.upgrade() {
                let service = Self { inner };
                if let Err(error) =
                    service.update(None, &[], None, Instant::now() + Duration::from_secs(120))
                {
                    tracing::warn!(%error, "cached search indexing failed");
                }
                let guard = service.inner.wake.lock().unwrap();
                let (mut guard, _) = service
                    .inner
                    .condition
                    .wait_timeout_while(guard, Duration::from_secs(30), |wake| !*wake)
                    .unwrap();
                *guard = false;
                drop(guard);
                // Do not let the background worker keep an otherwise-unused server alive.
                if Arc::strong_count(&service.inner) == 1 {
                    break;
                }
            }
        });
    }

    pub fn wake(&self) {
        *self.inner.wake.lock().unwrap() = true;
        self.inner.condition.notify_one();
    }

    pub fn descriptor(&self, id: &str) -> Result<Descriptor> {
        self.inner.store.descriptor(id)?.ok_or_else(|| {
            AIexError::Other("cached source is unavailable; repeat discovery".into())
        })
    }

    pub fn sources(&self) -> Vec<Descriptor> {
        let cache = self
            .inner
            .cache
            .session(
                Freshness::CachedOk,
                Instant::now() + Duration::from_millis(300),
            )
            .cached_only();
        self.inner
            .store
            .descriptors()
            .unwrap_or_default()
            .into_iter()
            .filter(|source| {
                if !self.supports(&source.route.index, &source.route.path) {
                    return false;
                }
                let Some(expected) = &source.repo else {
                    return false;
                };
                if self
                    .coverage(&source.id)
                    .is_none_or(|coverage| coverage.indexed_files == 0)
                {
                    return false;
                }
                corpus::level(
                    &cache,
                    &self.inner.registry,
                    &source.route.index,
                    &source.route.path,
                )
                .is_ok_and(|level| {
                    level
                        .find(&source.route.entry_id)
                        .is_some_and(|e| &e.origin == expected)
                })
            })
            .collect()
    }

    pub fn has_embeddings(&self) -> bool {
        self.inner.config.embedding.is_some()
    }

    pub fn supports(&self, index: &str, path: &[String]) -> bool {
        self.inner.config.for_source(index, path).enabled
    }

    pub fn coverage(&self, id: &str) -> Option<corpus::Coverage> {
        self.inner.store.coverage(id).ok().flatten()
    }

    /// Informational readiness; search always revalidates current membership.
    pub fn status(&self, id: &str) -> Value {
        let coverage = self.coverage(id);
        let semantic = match &self.inner.config.embedding {
            None => "not_configured",
            Some(_) if coverage.is_none() => "preparing",
            Some(_) if coverage.as_ref().is_some_and(|c| c.indexed_files == 0) => "empty",
            Some(provider) => {
                let model = provider.identity();
                if self
                    .inner
                    .store
                    .failure(id, &model)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    "unavailable"
                } else if self
                    .inner
                    .store
                    .missing(id, &model, 1)
                    .is_ok_and(|v| v.is_empty())
                {
                    "ready"
                } else {
                    "preparing"
                }
            }
        };
        json!({"keyword":if coverage.is_some() {"ready"} else {"preparing"}, "semantic":semantic, "coverage":coverage})
    }

    pub fn prepare(
        &self,
        descriptor: Descriptor,
        cache: &RepoCache,
        deadline: Instant,
    ) -> Result<Prepared> {
        let mut corpus = corpus::prepare(
            cache,
            &self.inner.registry,
            descriptor,
            &self.inner.config,
            deadline,
        )?;
        if Instant::now() >= deadline {
            return Err(AIexError::RefreshPending);
        }
        self.inner
            .store
            .publish(&corpus, self.inner.config.chunk_bytes)?;
        corpus.documents.clear();
        self.wake();
        Ok(Prepared {
            service: self.clone(),
            corpus,
        })
    }

    pub fn update(
        &self,
        index: Option<&str>,
        path: &[String],
        command: Option<&str>,
        deadline: Instant,
    ) -> Result<UpdateReport> {
        let cache = self
            .inner
            .cache
            .session(Freshness::CachedOk, deadline)
            .cached_only();
        let inventory = corpus::inventory(&cache, &self.inner.registry, deadline, index, path);
        let mut report = UpdateReport {
            unavailable_levels: inventory.unavailable_levels,
            inventory_limited: inventory.limited,
            ..Default::default()
        };
        if index.is_none()
            && path.is_empty()
            && inventory.unavailable_levels == 0
            && !inventory.limited
        {
            let active: Vec<_> = inventory
                .sources
                .iter()
                .filter(|s| self.supports(&s.route.index, &s.route.path))
                .map(|s| s.id.clone())
                .collect();
            self.inner.store.prune(&active)?;
        }
        let mut prepared = Vec::new();
        for descriptor in inventory.sources {
            if !self.supports(&descriptor.route.index, &descriptor.route.path) {
                continue;
            }
            if index.is_some_and(|i| descriptor.route.index != i)
                || !descriptor.route.path.starts_with(path)
                || command.is_some_and(|id| {
                    descriptor.repo.is_none()
                        || descriptor.route.path != path
                        || !corpus::level(
                            &cache,
                            &self.inner.registry,
                            &descriptor.route.index,
                            path,
                        )
                        .is_ok_and(|level| {
                            level.find(id).is_some_and(|entry| {
                                matches!(entry.entry.kind, crate::index::EntryKind::Command { .. })
                                    && descriptor.repo.as_ref() == Some(&entry.origin)
                            })
                        })
                })
            {
                continue;
            }
            if Instant::now() >= deadline {
                report.pending_sources += 1;
                continue;
            }
            match corpus::prepare(
                &cache,
                &self.inner.registry,
                descriptor,
                &self.inner.config,
                deadline,
            ) {
                Ok(corpus) => {
                    report.changed_sources += usize::from(
                        self.inner
                            .store
                            .publish(&corpus, self.inner.config.chunk_bytes)?,
                    );
                    report.indexed_sources += 1;
                    report.partial_sources +=
                        usize::from(corpus.coverage.limited || corpus.coverage.missing_files > 0);
                    prepared.push(corpus.descriptor.id);
                }
                Err(AIexError::RefreshPending) => report.pending_sources += 1,
                Err(error) => report.errors.push(error.to_string()),
            }
        }
        let Some(provider) = &self.inner.config.embedding else {
            self.inner.store.collect_vectors(None)?;
            return Ok(report);
        };
        let model = provider.identity();
        let Some(_owner) = try_lock(&self.inner.locks.join("embeddings.lock"))? else {
            report.pending_sources += prepared
                .iter()
                .filter(|id| {
                    self.inner
                        .store
                        .missing(id, &model, 1)
                        .is_ok_and(|v| !v.is_empty())
                })
                .count();
            return Ok(report);
        };
        self.inner.store.collect_vectors(Some(&model))?;
        for id in prepared {
            if let Some(error) = self.inner.store.failure(&id, &model)? {
                report.errors.push(error);
                continue;
            }
            loop {
                let missing = self.inner.store.missing(&id, &model, 8)?;
                if missing.is_empty() {
                    self.inner.store.set_failure(&id, &model, None)?;
                    break;
                }
                if Instant::now() >= deadline {
                    report.pending_sources += 1;
                    break;
                }
                // Queries take a shared priority marker. Yield between small batches.
                let Some(priority) = try_lock(&self.inner.locks.join("query-priority.lock"))?
                else {
                    report.pending_sources += 1;
                    break;
                };
                drop(priority);
                let keys: Vec<_> = missing.iter().map(|(key, _)| key.clone()).collect();
                let inputs: Vec<_> = missing.into_iter().map(|(_, input)| input).collect();
                match provider
                    .embed(&inputs, false)
                    .and_then(|vectors| self.inner.store.insert_vectors(&model, &keys, &vectors))
                {
                    Ok(()) => report.embedded_chunks += keys.len(),
                    Err(error) => {
                        self.inner
                            .store
                            .set_failure(&id, &model, Some(&error.to_string()))?;
                        report.errors.push(error.to_string());
                        break;
                    }
                }
            }
        }
        Ok(report)
    }
}

impl Prepared {
    pub fn execute(&self, query: &str, mode: Mode, limit: usize) -> Result<Value> {
        let inner = &self.service.inner;
        let id = &self.corpus.descriptor.id;
        let mut semantic = json!({"status":"not_configured","message":"Configure an embedding provider in search.yaml to enable semantic retrieval."});
        let mut used = Mode::Keyword;
        let mut embedding = None;
        let mut model = String::new();
        if mode != Mode::Keyword {
            if let Some(provider) = &inner.config.embedding {
                model = provider.identity();
                let missing = !inner.store.missing(id, &model, 1)?.is_empty();
                if missing {
                    semantic = if let Some(error) = inner.store.failure(id, &model)? {
                        json!({"status":"unavailable","message":error})
                    } else {
                        json!({"status":"preparing","retry_after_ms":2000})
                    };
                    self.service.wake();
                } else if self.corpus.coverage.indexed_files > 0 {
                    let priority = lock_file(&inner.locks.join("query-priority.lock"))?;
                    priority.lock_shared()?;
                    match provider.embed(&[query.into()], true) {
                        Ok(mut values)
                            if inner.store.dimensions(&model)? == Some(values[0].len()) =>
                        {
                            embedding = values.pop();
                            used = mode;
                            semantic = json!({"status":"ready"});
                        }
                        Ok(_) => {
                            semantic = json!({"status":"unavailable","message":"Embedding dimensions changed; configure a new model revision."})
                        }
                        Err(error) => {
                            semantic = json!({"status":"unavailable","message":error.to_string()})
                        }
                    }
                }
            }
        }
        if mode == Mode::Semantic && embedding.is_none() && self.corpus.coverage.indexed_files > 0 {
            let status = if semantic["status"] == "preparing" {
                "preparing"
            } else {
                "unavailable"
            };
            let mut response = json!({"status":status,"phase":"semantic_index","semantic":semantic,"coverage":self.corpus.coverage,"hits":[]});
            if status == "preparing" {
                response["retry_after_ms"] = json!(2000);
            }
            return Ok(response);
        }
        let mut hits = inner.store.query(
            &self.corpus,
            query,
            used,
            embedding.as_deref().map(|v| (model.as_str(), v)),
            limit + 1,
        )?;
        let truncated = hits.len() > limit;
        hits.truncate(limit);
        let mut response = json!({"status":"ready","mode_requested":mode,"mode_used":used,"semantic":semantic,"coverage":self.corpus.coverage,"hits":hits,"truncated":truncated});
        if self.corpus.descriptor.repo.is_some() {
            response["source_id"] = json!(id);
        } else {
            response["index"] = json!(self.corpus.descriptor.route.index);
            response["path"] = json!(self.corpus.descriptor.route.path);
        }
        Ok(response)
    }
}

/// Byte budget includes the result metadata and read routes, not just excerpts.
pub fn bounded(mut value: Value, max_bytes: usize) -> Result<String> {
    loop {
        let body = serde_json::to_string(&value).map_err(|e| AIexError::Other(e.to_string()))?;
        if body.len() <= max_bytes {
            return Ok(body);
        }
        value["truncated"] = json!(true);
        let Some(hits) = value.get_mut("hits").and_then(Value::as_array_mut) else {
            return Err(AIexError::Other(
                "search response metadata exceeds output budget".into(),
            ));
        };
        if hits.pop().is_none() {
            return Err(AIexError::Other(
                "search response metadata exceeds output budget".into(),
            ));
        }
    }
}
