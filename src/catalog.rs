//! Incremental, metadata-only catalog of registered documentation sources.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cache::{RefreshPolicy, RepoCache};
use crate::command::CommandPolicyChain;
use crate::error::AIexError;
use crate::index::EntryKind;
use crate::registry::{RefreshSetting, RegistryConfig};
use crate::resolver::{descend, LoadedIndex, Resolver, TraversalBudget};

const MAX_WORKERS: usize = 2;
const MAX_SOURCES: usize = 50_000;
const MAX_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_LOADS: usize = 5000;
const BUILD_BUDGET: Duration = Duration::from_secs(60);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Browse,
    Command,
}

/// These are ordinary search_docs arguments; no new authority is granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceRoute {
    pub index: String,
    pub path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
}

#[derive(Debug)]
pub(crate) struct Source {
    pub id: String,
    pub kind: SourceKind,
    pub name: String,
    pub description: String,
    pub description_complete: bool,
    pub terms: BTreeMap<String, f64>,
    pub length: f64,
    pub normalized_name: String,
    pub route: SourceRoute,
}

impl Source {
    pub(crate) fn new(
        kind: SourceKind,
        route: SourceRoute,
        name: &str,
        description: &str,
        context: &str,
        docs: &str,
    ) -> Self {
        let identity =
            serde_json::to_vec(&(kind, &route)).expect("source identity is serializable");
        let name = clip(name, 512);
        let description_complete = description.chars().count() <= 4096;
        let description = clip(description, 4096);
        let mut terms = BTreeMap::new();
        let mut length = 0.0;
        for (field, weight) in [
            (&name, 4.0),
            (&description, 2.0),
            (&clip(context, 2048), 0.8),
            (&clip(docs, 4096), 0.5),
        ] {
            for term in tokenize(field) {
                *terms.entry(term).or_default() += weight;
                length += weight;
            }
        }
        let normalized_name = tokenize(&name).join(" ");
        Self {
            id: format!("src_{}", hex::encode(Sha256::digest(identity))),
            kind,
            name,
            description,
            description_complete,
            terms,
            length,
            normalized_name,
            route,
        }
    }
    // Conservative retained-memory estimate, including tree nodes and string allocations.
    pub(crate) fn bytes(&self) -> usize {
        self.id.len()
            + self.name.len()
            + self.description.len()
            + self.normalized_name.len()
            + self.route.index.len()
            + self.route.path.iter().map(|s| s.len() + 24).sum::<usize>()
            + self.route.command_id.as_ref().map_or(0, String::len)
            + self.terms.keys().map(|s| s.len() + 96).sum::<usize>()
            + 512
    }
}

pub(crate) fn clip(value: &str, chars: usize) -> String {
    value.chars().take(chars).collect()
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CatalogStatus {
    pub complete: bool,
    pub pending_indexes: usize,
    pub failed_nodes: usize,
    pub stale_indexes: usize,
    pub limited_indexes: usize,
}

pub(crate) struct Snapshot {
    pub generation: u64,
    pub sources: Vec<Arc<Source>>,
    pub status: CatalogStatus,
}

#[derive(Default)]
struct RootState {
    sources: BTreeMap<String, Arc<Source>>,
    wanted: bool,
    running: bool,
    waiting: bool,
    failed: usize,
    limited: bool,
    stale: bool,
    checked: Option<Instant>,
    retry_at: Option<Instant>,
}

struct State {
    roots: BTreeMap<String, RootState>,
    workers: usize,
    generation: u64,
    bytes: usize,
}

struct Inner {
    registry: Arc<RegistryConfig>,
    cache: Arc<RepoCache>,
    state: Mutex<State>,
}

#[derive(Clone)]
pub(crate) struct Catalog {
    inner: Arc<Inner>,
}

struct Task {
    parent: Option<(Arc<LoadedIndex>, String)>,
    path: Vec<String>,
    context: String,
}

impl Catalog {
    pub fn new(registry: Arc<RegistryConfig>, cache: Arc<RepoCache>) -> Self {
        let roots = registry
            .indexes
            .keys()
            .map(|name| (name.clone(), RootState::default()))
            .collect();
        Self {
            inner: Arc::new(Inner {
                registry,
                cache,
                state: Mutex::new(State {
                    roots,
                    workers: 0,
                    generation: 0,
                    bytes: 0,
                }),
            }),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.inner.registry.indexes.keys().cloned().collect()
    }

    pub fn request(&self, names: &[String]) {
        {
            let mut state = self.inner.state.lock().expect("catalog poisoned");
            for name in names {
                if let Some(root) = state.roots.get_mut(name) {
                    if !root.running {
                        root.wanted = true;
                    }
                }
            }
        }
        self.kick();
    }

    fn refresh(&self, name: &str) -> RefreshPolicy {
        match &self.inner.registry.indexes[name].refresh {
            Some(RefreshSetting::Never) => RefreshPolicy::Never,
            Some(RefreshSetting::Every(d)) => RefreshPolicy::from_duration(*d),
            None => self.inner.cache.default_refresh(),
        }
    }

    fn due(&self, name: &str, root: &RootState, now: Instant) -> bool {
        if root.running || !root.wanted {
            return false;
        }
        if let Some(retry) = root.retry_at {
            return now >= retry;
        }
        match (root.checked, self.refresh(name)) {
            (None, _) => true,
            (Some(_), RefreshPolicy::Never) => false,
            (Some(last), RefreshPolicy::Every(interval)) => now.duration_since(last) >= interval,
        }
    }

    fn kick(&self) {
        let jobs = {
            let mut state = self.inner.state.lock().expect("catalog poisoned");
            let now = Instant::now();
            let jobs: Vec<String> = state
                .roots
                .iter()
                .filter(|(name, root)| self.due(name, root, now))
                .take(MAX_WORKERS.saturating_sub(state.workers))
                .map(|(name, _)| name.clone())
                .collect();
            for name in &jobs {
                let root = state.roots.get_mut(name).unwrap();
                root.wanted = false;
                root.stale = !root.sources.is_empty();
                root.running = true;
                root.waiting = true;
                root.failed = 0;
                root.limited = false;
            }
            state.workers += jobs.len();
            if !jobs.is_empty() {
                state.generation += 1;
            }
            jobs
        };
        for name in jobs {
            let catalog = self.clone();
            std::thread::spawn(move || {
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| catalog.build(&name)));
                if outcome.is_err() {
                    tracing::error!(index = name, "catalog worker panicked");
                    catalog.finish(&name, BTreeSet::new(), 1, false, false, true);
                }
                catalog.kick();
            });
        }
    }

    fn build(&self, name: &str) {
        let root = &self.inner.registry.indexes[name];
        let budget = TraversalBudget::new(32, MAX_INDEX_LOADS);
        let resolver = Resolver::with_budget(&self.inner.cache, &budget);
        let context = format!("{} {}", name, root.description.as_deref().unwrap_or(""));
        let mut tasks = VecDeque::from([Task {
            parent: None,
            path: vec![],
            context,
        }]);
        let mut enqueued = 1;
        let mut seen = BTreeSet::new();
        let mut failed = 0;
        let mut waiting = false;
        let mut limited = false;
        let start = Instant::now();
        while let Some(task) = tasks.pop_front() {
            if start.elapsed() >= BUILD_BUDGET {
                waiting = true;
                break;
            }
            let loaded = match &task.parent {
                None => resolver.load_index_with_refresh(
                    &root.repo,
                    &root.path,
                    &CommandPolicyChain::default(),
                    self.refresh(name),
                ),
                Some((parent, id)) => descend(&resolver, parent, id),
            };
            let loaded = match loaded {
                Ok(index) => Arc::new(index),
                Err(AIexError::Warming { .. }) => {
                    tasks.push_back(task);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(AIexError::TraversalLimit(_)) => {
                    limited = true;
                    break;
                }
                Err(error) => {
                    failed += 1;
                    tracing::warn!(index = name, path = ?task.path, %error, "catalog index unavailable");
                    continue;
                }
            };
            let description = loaded.description.as_deref().unwrap_or("");
            let context = clip(
                &format!("{} {} {}", task.context, loaded.name, description),
                2048,
            );
            let mut documents = String::new();
            for entry in &loaded.entries {
                if matches!(entry.entry.kind, EntryKind::Doc { .. }) {
                    documents.push_str(&clip(
                        &format!(
                            " {} {}",
                            entry.view.name,
                            entry.view.description.as_deref().unwrap_or("")
                        ),
                        512,
                    ));
                    if documents.len() >= 4096 {
                        break;
                    }
                }
            }
            let mut batch = vec![Source::new(
                SourceKind::Browse,
                SourceRoute {
                    index: name.into(),
                    path: task.path.clone(),
                    command_id: None,
                },
                &loaded.name,
                description,
                &context,
                &documents,
            )];
            for entry in &loaded.entries {
                match &entry.entry.kind {
                    EntryKind::Command { .. } => batch.push(Source::new(
                        SourceKind::Command,
                        SourceRoute {
                            index: name.into(),
                            path: task.path.clone(),
                            command_id: Some(entry.view.id.clone()),
                        },
                        &entry.view.name,
                        entry.view.description.as_deref().unwrap_or(""),
                        &context,
                        "",
                    )),
                    EntryKind::Index { .. } => {
                        if enqueued >= MAX_INDEX_LOADS {
                            limited = true;
                            continue;
                        }
                        enqueued += 1;
                        let mut path = task.path.clone();
                        path.push(entry.view.id.clone());
                        tasks.push_back(Task {
                            parent: Some((loaded.clone(), entry.view.id.clone())),
                            path,
                            context: clip(
                                &format!(
                                    "{} {} {}",
                                    context,
                                    entry.view.name,
                                    entry.view.description.as_deref().unwrap_or("")
                                ),
                                2048,
                            ),
                        });
                    }
                    _ => {}
                }
                if batch.len() >= 256 {
                    seen.extend(batch.iter().map(|source| source.id.clone()));
                    if !self.publish(name, std::mem::take(&mut batch)) {
                        limited = true;
                        break;
                    }
                }
            }
            for source in &batch {
                seen.insert(source.id.clone());
            }
            if !self.publish(name, batch) {
                limited = true;
                break;
            }
            if limited {
                break;
            }
        }
        self.finish(name, seen, failed, waiting, limited, budget.is_stale());
    }

    fn publish(&self, name: &str, batch: Vec<Source>) -> bool {
        let mut state = self.inner.state.lock().expect("catalog poisoned");
        let mut count: usize = state.roots.values().map(|r| r.sources.len()).sum();
        let mut bytes = state.bytes;
        let root = state.roots.get_mut(name).unwrap();
        let mut complete = true;
        for source in batch {
            let previous_bytes = root.sources.get(&source.id).map_or(0, |s| s.bytes());
            let next_bytes = bytes - previous_bytes + source.bytes();
            if next_bytes > MAX_CATALOG_BYTES {
                complete = false;
                break;
            }
            if let Some(previous) = root.sources.get(&source.id) {
                // Never let even a hypothetical digest collision change a route.
                if previous.route != source.route || previous.kind != source.kind {
                    complete = false;
                    break;
                }
            } else {
                if count >= MAX_SOURCES {
                    complete = false;
                    break;
                }
                count += 1;
            }
            root.sources.insert(source.id.clone(), Arc::new(source));
            bytes = next_bytes;
        }
        state.bytes = bytes;
        state.generation += 1;
        complete
    }

    fn finish(
        &self,
        name: &str,
        seen: BTreeSet<String>,
        failed: usize,
        waiting: bool,
        limited: bool,
        stale: bool,
    ) {
        let mut state = self.inner.state.lock().expect("catalog poisoned");
        let root = state.roots.get_mut(name).unwrap();
        if !root.running {
            return;
        }
        root.stale |= stale;
        if failed == 0 && !waiting && !limited && !stale {
            root.stale = false;
            root.sources.retain(|id, _| seen.contains(id));
        }
        root.failed = failed;
        root.waiting = waiting;
        root.limited = limited;
        root.running = false;
        root.checked = Some(Instant::now());
        root.retry_at = if waiting {
            Some(Instant::now() + Duration::from_secs(1))
        } else if failed > 0 || limited || stale {
            Some(Instant::now() + Duration::from_secs(30))
        } else {
            None
        };
        state.bytes = state
            .roots
            .values()
            .flat_map(|r| r.sources.values())
            .map(|s| s.bytes())
            .sum();
        state.workers -= 1;
        state.generation += 1;
    }

    pub fn snapshot(&self, names: &[String]) -> Snapshot {
        let state = self.inner.state.lock().expect("catalog poisoned");
        let mut status = CatalogStatus {
            complete: true,
            ..Default::default()
        };
        let mut sources = Vec::new();
        for name in names {
            let root = &state.roots[name];
            let queued_refresh = self.due(name, root, Instant::now());
            let pending = root.checked.is_none() || root.running || root.waiting || queued_refresh;
            let stale = root.stale || (queued_refresh && !root.sources.is_empty());
            status.pending_indexes += usize::from(pending);
            status.failed_nodes += root.failed;
            status.limited_indexes += usize::from(root.limited);
            status.stale_indexes += usize::from(stale);
            status.complete &= !pending && root.failed == 0 && !root.limited && !stale;
            sources.extend(root.sources.values().cloned());
        }
        Snapshot {
            generation: state.generation,
            sources,
            status,
        }
    }

    pub fn route(&self, id: &str) -> Option<SourceRoute> {
        self.inner
            .state
            .lock()
            .expect("catalog poisoned")
            .roots
            .values()
            .find_map(|root| root.sources.get(id).map(|s| s.route.clone()))
    }
}

/// Unicode words plus identifier boundaries, e.g. resourceGroups and AI_Core.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = c.is_uppercase()
            && i > 0
            && (chars[i - 1].is_lowercase()
                || chars[i - 1].is_numeric()
                || (chars[i - 1].is_uppercase()
                    && chars.get(i + 1).is_some_and(|c| c.is_lowercase())));
        if (!c.is_alphanumeric() || boundary) && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_refresh_is_pending_and_stale_before_a_worker_starts() {
        let temp = tempfile::tempdir().unwrap();
        let registry = RegistryConfig {
            indexes: BTreeMap::from([(
                "docs".into(),
                crate::registry::RegistryEntry {
                    repo: crate::index::GitRef {
                        url: "file:///unused".into(),
                        ..Default::default()
                    },
                    path: "pinakes.yaml".into(),
                    description: None,
                    refresh: Some(RefreshSetting::Every(Duration::from_secs(1))),
                },
            )]),
        };
        let catalog = Catalog::new(
            Arc::new(registry),
            Arc::new(RepoCache::new(
                temp.path().join("cache"),
                crate::auth::AuthConfig::default(),
            )),
        );
        {
            let mut state = catalog.inner.state.lock().unwrap();
            // Both workers are occupied when another root becomes due.
            state.workers = MAX_WORKERS;
            let root = state.roots.get_mut("docs").unwrap();
            root.checked = Some(Instant::now() - Duration::from_secs(2));
            let source = Source::new(
                SourceKind::Browse,
                SourceRoute {
                    index: "docs".into(),
                    path: vec![],
                    command_id: None,
                },
                "Docs",
                "",
                "",
                "",
            );
            root.sources.insert(source.id.clone(), Arc::new(source));
        }
        let names = vec!["docs".into()];
        catalog.request(&names);
        let snapshot = catalog.snapshot(&names);
        assert!(!snapshot.status.complete);
        assert_eq!(snapshot.status.pending_indexes, 1);
        assert_eq!(snapshot.status.stale_indexes, 1);
        assert_eq!(snapshot.sources.len(), 1);
    }
}
