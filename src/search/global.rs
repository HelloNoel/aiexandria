//! Bounded retrieval over the union of currently authorized, indexed cached sources.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use super::{corpus, Mode, SearchService};
use crate::cache::{lock_file, RepoCache};
use crate::error::{AIexError, Result};

#[derive(Default, Clone, Serialize)]
pub struct Coverage {
    pub indexes: usize,
    pub sources_discovered: usize,
    pub sources_searched: usize,
    pub pending_sources: usize,
    pub unavailable_sources: usize,
    pub unavailable_levels: usize,
    pub uncached_repositories: usize,
    pub partial_sources: usize,
    pub documents_searched: usize,
    pub chunks_searched: usize,
    pub embedded_chunks: usize,
    pub limited: bool,
    pub cached_only: bool,
    pub partial: bool,
}

pub struct SourceFilter {
    pub id: String,
    pub index: String,
    pub path: Vec<String>,
}

pub struct Document {
    pub file: String,
    pub revision: String,
    pub repository: String,
}

pub struct Source {
    pub descriptor: corpus::Descriptor,
    pub generation: String,
    pub documents: Vec<Document>,
}

pub struct Prepared {
    service: SearchService,
    pub(crate) sources: Vec<Source>,
    coverage: Coverage,
}

pub struct QueryOptions {
    pub limit: usize,
    pub max_chunks: usize,
    pub deadline: Instant,
}

#[derive(Default)]
pub struct QueryResult {
    pub hits: Vec<Value>,
    pub sources_searched: usize,
    pub changed_sources: usize,
    pub documents_searched: usize,
    pub chunks_searched: usize,
    pub embedded_chunks: usize,
    pub limited: bool,
    pub timed_out: bool,
    pub truncated: bool,
}

impl SearchService {
    pub fn prepare_global(
        &self,
        cache: &RepoCache,
        indexes: &[String],
        filters: &[SourceFilter],
        deadline: Instant,
    ) -> Result<Prepared> {
        let mut registry = (*self.inner.registry).clone();
        if !indexes.is_empty() {
            registry.indexes.retain(|name, _| indexes.contains(name));
        }
        let cache = cache.cached_only();
        let source_ids: Vec<_> = filters.iter().map(|filter| filter.id.clone()).collect();
        let inventory = if filters.is_empty() {
            corpus::inventory(&cache, &registry, deadline, None, &[])
        } else {
            // Explicit source filters do not scan unrelated sub-indexes, or count
            // their unavailable metadata as gaps in the requested scope.
            let mut inventory = corpus::Inventory::default();
            for filter in filters {
                if Instant::now() >= deadline {
                    inventory.limited = true;
                    break;
                }
                let level = match corpus::level(&cache, &registry, &filter.index, &filter.path) {
                    Ok(level) => level,
                    Err(_) => {
                        inventory.unavailable_levels += 1;
                        continue;
                    }
                };
                let descriptor = if filter.id.starts_with("curated:") {
                    Some(corpus::Descriptor::curated(
                        &filter.index,
                        &filter.path,
                        &level,
                    ))
                } else {
                    self.inner.store.descriptor(&filter.id)?
                };
                if let Some(descriptor) = descriptor {
                    if descriptor.id == filter.id
                        && descriptor.route.index == filter.index
                        && descriptor.route.path == filter.path
                    {
                        inventory.sources.push(descriptor);
                    }
                }
            }
            inventory
        };
        let mut coverage = Coverage {
            indexes: registry.indexes.len(),
            unavailable_levels: inventory.unavailable_levels,
            uncached_repositories: inventory.uncached_repositories,
            limited: inventory.limited,
            cached_only: true,
            ..Default::default()
        };
        let mut sources = Vec::new();
        let mut selected = HashSet::new();
        for descriptor in inventory.sources {
            if !self.supports(&descriptor.route.index, &descriptor.route.path)
                || (!source_ids.is_empty() && !source_ids.contains(&descriptor.id))
            {
                continue;
            }
            selected.insert(descriptor.id.clone());
            coverage.sources_discovered += 1;
            if coverage.sources_discovered > self.inner.config.global_max_sources
                || Instant::now() >= deadline
            {
                coverage.limited = true;
                continue;
            }
            // Re-resolve the current documents and their leased revisions; never trust
            // stored membership as read authority. Do not publish/re-embed on this path.
            match corpus::prepare(&cache, &registry, descriptor, &self.inner.config, deadline) {
                Ok(corpus) => {
                    coverage.partial_sources +=
                        usize::from(corpus.coverage.limited || corpus.coverage.missing_files > 0);
                    if self
                        .inner
                        .store
                        .generation(&corpus.descriptor.id)?
                        .as_deref()
                        != Some(&corpus.generation)
                    {
                        coverage.pending_sources += 1;
                        continue;
                    }
                    sources.push(Source {
                        descriptor: corpus.descriptor,
                        generation: corpus.generation,
                        documents: corpus
                            .documents
                            .into_iter()
                            .map(|doc| Document {
                                file: doc.file,
                                revision: doc.revision,
                                repository: doc.repository,
                            })
                            .collect(),
                    });
                }
                Err(AIexError::RefreshPending) => coverage.pending_sources += 1,
                Err(_) => coverage.unavailable_sources += 1,
            }
        }
        coverage.unavailable_sources += source_ids
            .iter()
            .filter(|id| !selected.contains(*id))
            .count();
        self.wake();
        Ok(Prepared {
            service: self.clone(),
            sources,
            coverage,
        })
    }
}

impl Prepared {
    pub fn execute(&self, query: &str, mode: Mode, limit: usize) -> Result<Value> {
        let inner = &self.service.inner;
        let deadline = Instant::now() + Duration::from_secs(inner.config.global_timeout_secs);
        let mut semantic = json!({"status":"not_requested"});
        let mut vector = None;
        let mut model = String::new();
        if mode != Mode::Keyword {
            semantic = json!({"status":"not_configured"});
            if let Some(provider) = &inner.config.embedding {
                model = provider.identity();
                semantic = json!({"status":"preparing","retry_after_ms":2000});
                if !self.sources.is_empty() {
                    if let Some(dimensions) = inner.store.dimensions(&model)? {
                        let priority = lock_file(&inner.locks.join("query-priority.lock"))?;
                        priority.lock_shared()?;
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        // Keep time for useful keyword fallback when a hybrid provider stalls.
                        let provider_budget = if mode == Mode::Hybrid {
                            remaining.saturating_sub((remaining / 4).min(Duration::from_secs(1)))
                        } else {
                            remaining
                        };
                        match provider.embed_with_timeout(&[query.into()], true, provider_budget) {
                            Ok(mut values) if values[0].len() == dimensions => {
                                vector = values.pop()
                            }
                            Ok(_) => {
                                semantic = json!({"status":"unavailable","message":"Embedding dimensions changed; configure a new model revision."})
                            }
                            Err(error) => {
                                semantic =
                                    json!({"status":"unavailable","message":error.to_string()})
                            }
                        }
                    }
                }
            }
        }
        let result = inner.store.query_global(
            &self.sources,
            query,
            mode,
            vector.as_deref().map(|v| (model.as_str(), v)),
            QueryOptions {
                limit,
                max_chunks: inner.config.global_max_chunks,
                deadline,
            },
        )?;
        let mut coverage = self.coverage.clone();
        coverage.sources_searched = result.sources_searched;
        coverage.pending_sources += result.changed_sources;
        coverage.documents_searched = result.documents_searched;
        coverage.chunks_searched = result.chunks_searched;
        coverage.embedded_chunks = result.embedded_chunks;
        coverage.limited |= result.limited;
        let used = if result.embedded_chunks > 0 && vector.is_some() {
            semantic = json!({"status": if result.embedded_chunks == result.chunks_searched {"ready"} else {"partial"}});
            mode
        } else {
            Mode::Keyword
        };
        coverage.partial = coverage.limited
            || coverage.pending_sources > 0
            || coverage.unavailable_sources > 0
            || coverage.unavailable_levels > 0
            || coverage.uncached_repositories > 0
            || coverage.partial_sources > 0
            || (mode != Mode::Keyword && result.embedded_chunks < result.chunks_searched);
        let mut status = "ready";
        let mut message = None;
        if result.timed_out {
            status = "limited";
            message = Some("Global search reached its work deadline; narrow indexes or source_ids, or increase global_timeout_secs.");
        } else if mode == Mode::Semantic
            && (vector.is_none() || result.embedded_chunks == 0)
            && !self.sources.is_empty()
        {
            status = if semantic["status"] == "preparing" {
                "preparing"
            } else {
                "unavailable"
            };
        } else if coverage.sources_searched == 0 && coverage.partial {
            status = if coverage.limited {
                "limited"
            } else if coverage.pending_sources > 0 {
                "preparing"
            } else {
                "unavailable"
            };
            message = Some("No current indexed sources were searched; prepare documentation with update --semantic, or narrow the requested scope.");
        }
        let mut response = json!({"status":status,"scope":"global","mode_requested":mode,
            "mode_used":if mode == Mode::Semantic && used != Mode::Semantic { None } else { Some(used) },
            "semantic":semantic,"coverage":coverage,"hits":result.hits,"truncated":result.truncated});
        if let Some(message) = message {
            response["message"] = json!(message);
        }
        if status == "preparing" {
            response["retry_after_ms"] = json!(2000);
        }
        Ok(response)
    }
}
