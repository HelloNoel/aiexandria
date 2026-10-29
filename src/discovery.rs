//! Built-in source discovery. Queries rank metadata; they never execute searches.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::RepoCache;
use crate::catalog::{clip, tokenize, Catalog, Source};
pub use crate::catalog::{CatalogStatus, SourceKind, SourceRoute};
use crate::registry::RegistryConfig;

const FOREGROUND_BUDGET: Duration = Duration::from_millis(1500);
const CURSOR_TTL: Duration = Duration::from_secs(300);
const MAX_SNAPSHOTS: usize = 16;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoverSourcesRequest {
    /// Topic or source name to discover; not a Boolean content query. Required unless using cursor.
    pub query: Option<String>,
    /// Limit discovery to these registered indexes. Omit for all indexes.
    pub indexes: Option<Vec<String>>,
    /// Return command sources, browse sources, or both (default).
    pub kinds: Option<Vec<SourceKind>>,
    /// Number of sources per page, 1-20. Default 5.
    pub limit: Option<usize>,
    /// Maximum UTF-8 bytes in the JSON response, 512-32768. Default 8192.
    pub max_output_bytes: Option<usize>,
    /// Continue the same immutable result snapshot; omit query/indexes/kinds when using this.
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveredSource {
    pub source_id: String,
    pub name: String,
    pub kind: SourceKind,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Whether summary contains the complete original source description.
    pub guidance_complete: bool,
    pub query_profile: &'static str,
    /// Existing route arguments accepted by search_docs; source_id can also be used directly.
    pub search: SourceRoute,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryResponse {
    pub sources: Vec<DiscoveredSource>,
    pub catalog: CatalogStatus,
    pub generation: u64,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub matches_limited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

struct QuerySnapshot {
    id: String,
    created: Instant,
    sources: Vec<Arc<Source>>,
    catalog: CatalogStatus,
    generation: u64,
    limited: bool,
    bytes: usize,
}

/// Internal boundary for later ranking implementations, not a public plugin protocol.
trait SourceRanker {
    fn rank(&self, sources: &[Arc<Source>], query: &str) -> Vec<Arc<Source>>;
}

struct BuiltinRanker;

impl SourceRanker for BuiltinRanker {
    fn rank(&self, sources: &[Arc<Source>], query: &str) -> Vec<Arc<Source>> {
        const STOP: &[&str] = &[
            "a", "an", "and", "are", "can", "for", "how", "i", "in", "is", "of", "on", "or", "the",
            "to", "what", "where", "with",
        ];
        let all = tokenize(query);
        let mut terms: BTreeSet<String> = all
            .iter()
            .filter(|t| !STOP.contains(&t.as_str()))
            .cloned()
            .collect();
        if terms.is_empty() {
            terms.extend(all.iter().cloned());
        }
        let phrase = all.join(" ");
        let average =
            (sources.iter().map(|s| s.length).sum::<f64>() / sources.len().max(1) as f64).max(1.0);
        let idf: BTreeMap<_, _> = terms
            .iter()
            .map(|term| {
                let matches = sources
                    .iter()
                    .filter(|source| source.terms.contains_key(term))
                    .count() as f64;
                (
                    term,
                    (1.0 + (sources.len() as f64 - matches + 0.5) / (matches + 0.5)).ln(),
                )
            })
            .collect();
        let mut ranked: Vec<_> = sources
            .iter()
            .filter_map(|source| {
                let counts: BTreeMap<_, _> = terms
                    .iter()
                    .filter_map(|term| source.terms.get(term).map(|tf| (term, tf)))
                    .collect();
                let length = source.length;
                if counts.is_empty() {
                    return None;
                }
                let mut score = counts
                    .iter()
                    .map(|(term, &tf)| {
                        idf[term] * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * length / average))
                    })
                    .sum::<f64>();
                score *= 0.5 + counts.len() as f64 / terms.len().max(1) as f64;
                let name = &source.normalized_name;
                if !phrase.is_empty() && *name == phrase {
                    score += 6.0;
                } else if !phrase.is_empty() && name.contains(&phrase) {
                    score += 2.0;
                }
                Some((source.clone(), score))
            })
            .collect();
        ranked.sort_by(|(a, score_a), (b, score_b)| {
            score_b.total_cmp(score_a).then_with(|| a.id.cmp(&b.id))
        });
        ranked.into_iter().map(|(source, _)| source).collect()
    }
}

pub(crate) struct DiscoveryService {
    catalog: Catalog,
    snapshots: Mutex<VecDeque<Arc<QuerySnapshot>>>,
    ranking_slots: Arc<tokio::sync::Semaphore>,
}

impl DiscoveryService {
    pub fn new(registry: Arc<RegistryConfig>, cache: Arc<RepoCache>) -> Self {
        Self {
            catalog: Catalog::new(registry, cache),
            snapshots: Mutex::new(VecDeque::new()),
            ranking_slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    pub fn route(&self, id: &str) -> Option<SourceRoute> {
        self.catalog.route(id)
    }

    pub async fn discover(&self, req: DiscoverSourcesRequest) -> Result<String, String> {
        let limit = req.limit.unwrap_or(5);
        let bytes = req.max_output_bytes.unwrap_or(8192);
        if !(1..=20).contains(&limit) {
            return Err("limit must be between 1 and 20".into());
        }
        if !(512..=32768).contains(&bytes) {
            return Err("max_output_bytes must be between 512 and 32768".into());
        }
        if let Some(cursor) = req.cursor {
            if req.query.is_some() || req.indexes.is_some() || req.kinds.is_some() {
                return Err(
                    "cursor continues its original query and scope; omit query, indexes, and kinds"
                        .into(),
                );
            }
            let (id, offset) = cursor.rsplit_once(':').ok_or("invalid discovery cursor")?;
            let offset = offset
                .parse::<usize>()
                .map_err(|_| "invalid discovery cursor")?;
            let mut cache = self.snapshots.lock().expect("discovery snapshots poisoned");
            cache.retain(|snapshot| snapshot.created.elapsed() < CURSOR_TTL);
            let snapshot = cache
                .iter()
                .find(|snapshot| snapshot.id == id)
                .ok_or("discovery cursor expired; repeat the query")?;
            if offset >= snapshot.sources.len() {
                return Err("invalid discovery cursor offset".into());
            }
            return render_page(snapshot, offset, limit, bytes);
        }
        let query = req.query.ok_or("query is required without a cursor")?;
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 1024 || tokenize(query).is_empty() {
            return Err("query must contain words and be at most 1024 characters".into());
        }
        let known = self.catalog.names();
        let names: Vec<_> = req
            .indexes
            .unwrap_or_else(|| known.clone())
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if names.is_empty() {
            return Err("select at least one registered index".into());
        }
        for name in &names {
            if !known.contains(name) {
                return Err(format!("unknown index '{name}'"));
            }
        }
        if req.kinds.as_ref().is_some_and(Vec::is_empty) {
            return Err("kinds must not be empty".into());
        }
        self.catalog.request(&names);
        let started = Instant::now();
        let snapshot = loop {
            let snapshot = self.catalog.snapshot(&names);
            if snapshot.status.complete
                || snapshot.status.pending_indexes == 0
                || started.elapsed() >= FOREGROUND_BUDGET
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let candidates: Vec<_> = snapshot
            .sources
            .into_iter()
            .filter(|source| {
                req.kinds
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&source.kind))
            })
            .collect();
        let permit = self
            .ranking_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "discovery is busy; retry shortly")?;
        let query_owned = query.to_string();
        let ranked = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            BuiltinRanker.rank(&candidates, &query_owned)
        })
        .await
        .map_err(|_| "source ranking failed")?;
        let mut retained = Vec::new();
        let mut size = 0;
        let mut limited = false;
        for source in ranked {
            let cost = source.bytes();
            if retained.len() >= 1000 || size + cost > MAX_SNAPSHOT_BYTES {
                limited = true;
                break;
            }
            size += cost;
            retained.push(source);
        }
        let nonce = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let id = hex::encode(Sha256::digest(format!(
            "{}:{nonce}:{query}:{names:?}",
            snapshot.generation
        )));
        let snapshot = Arc::new(QuerySnapshot {
            id,
            created: Instant::now(),
            sources: retained,
            catalog: snapshot.status,
            generation: snapshot.generation,
            limited,
            bytes: size,
        });
        let result = render_page(&snapshot, 0, limit, bytes)?;
        let mut cache = self.snapshots.lock().expect("discovery snapshots poisoned");
        cache.retain(|snapshot| snapshot.created.elapsed() < CURSOR_TTL);
        while !cache.is_empty()
            && (cache.len() >= MAX_SNAPSHOTS
                || cache.iter().map(|s| s.bytes).sum::<usize>() + size > MAX_CACHE_BYTES)
        {
            cache.pop_front();
        }
        cache.push_back(snapshot);
        Ok(result)
    }
}

fn render_page(
    snapshot: &QuerySnapshot,
    offset: usize,
    limit: usize,
    budget: usize,
) -> Result<String, String> {
    let mut response = DiscoveryResponse {
        sources: Vec::new(),
        catalog: snapshot.catalog.clone(),
        generation: snapshot.generation,
        next_cursor: None,
        matches_limited: snapshot.limited,
        retry_after_ms: if snapshot.catalog.pending_indexes > 0 {
            Some(2000)
        } else if snapshot.catalog.failed_nodes > 0
            || snapshot.catalog.limited_indexes > 0
            || snapshot.catalog.stale_indexes > 0
        {
            Some(30_000)
        } else {
            None
        },
    };
    for source in snapshot.sources.iter().skip(offset).take(limit) {
        let summary = clip(&source.description, 240);
        response.sources.push(DiscoveredSource {
            source_id: source.id.clone(),
            name: clip(&source.name, 120),
            kind: source.kind,
            guidance_complete: source.description_complete && summary == source.description,
            summary,
            query_profile: if source.kind == SourceKind::Command {
                "source_defined"
            } else {
                "substring"
            },
            search: source.route.clone(),
        });
        let next = offset + response.sources.len();
        response.next_cursor =
            (next < snapshot.sources.len()).then(|| format!("{}:{next}", snapshot.id));
        // A small budget should drop optional prose before rejecting a usable locator.
        if response.sources.len() == 1 {
            while serde_json::to_vec(&response)
                .map_err(|e| e.to_string())?
                .len()
                > budget
                && !response.sources[0].summary.is_empty()
            {
                let first = &mut response.sources[0];
                first.summary = clip(&first.summary, first.summary.chars().count() / 2);
                first.guidance_complete = false;
            }
            if serde_json::to_vec(&response)
                .map_err(|e| e.to_string())?
                .len()
                > budget
            {
                response.sources[0].name = clip(&response.sources[0].name, 32);
            }
        }
        if serde_json::to_vec(&response)
            .map_err(|e| e.to_string())?
            .len()
            > budget
        {
            response.sources.pop();
            if response.sources.is_empty() {
                return Err(
                    "max_output_bytes is too small for a source locator; increase the budget"
                        .into(),
                );
            }
            break;
        }
    }
    let next = offset + response.sources.len();
    response.next_cursor =
        (next < snapshot.sources.len()).then(|| format!("{}:{next}", snapshot.id));
    let body = serde_json::to_string(&response).map_err(|e| e.to_string())?;
    if body.len() > budget {
        return Err("max_output_bytes is too small for catalog status".into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests;
