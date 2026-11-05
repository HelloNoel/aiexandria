use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{chunk::digest, config::SearchConfig};
use crate::cache::{RefreshPolicy, RepoCache};
use crate::command::CommandPolicyChain;
use crate::error::{AIexError, Result};
use crate::index::{EntryKind, GitRef};
use crate::registry::{RefreshSetting, RegistryConfig};
use crate::resolver::{descend, LoadedIndex, Resolver, TraversalBudget};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub index: String,
    pub path: Vec<String>,
    pub entry_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub id: String,
    pub name: String,
    pub description: String,
    pub route: Route,
    /// None for a curated level; Some for automatically selected repository docs.
    pub repo: Option<GitRef>,
}

impl Descriptor {
    pub fn curated_id(index: &str, path: &[String]) -> String {
        format!(
            "curated:{}",
            digest(serde_json::to_vec(&(index, path)).unwrap())
        )
    }

    pub fn curated(index: &str, path: &[String], level: &LoadedIndex) -> Self {
        Self {
            id: Self::curated_id(index, path),
            name: level.name.clone(),
            description: level.description.clone().unwrap_or_default(),
            route: Route {
                index: index.into(),
                path: path.to_vec(),
                entry_id: String::new(),
            },
            repo: None,
        }
    }
    fn repository(
        index: &str,
        path: &[String],
        id: &str,
        repo: &GitRef,
        level: &LoadedIndex,
    ) -> Self {
        let name = repo
            .url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("repository")
            .trim_end_matches(".git");
        Self {
            id: format!("cached:{}", digest(serde_json::to_vec(&(index, path, repo)).unwrap())),
            name: format!("{name} ({}): cached documentation", repo.commit.as_deref().or(repo.tag.as_deref()).or(repo.branch.as_deref()).unwrap_or("default branch")),
            description: format!("{}. {} Available documentation files selected automatically; independent of custom command scopes.", level.name, level.description.as_deref().unwrap_or("").chars().take(512).collect::<String>()),
            route: Route { index: index.into(), path: path.to_vec(), entry_id: id.into() }, repo: Some(repo.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub missing_files: usize,
    pub limited: bool,
    pub cached_only: bool,
}

pub struct Document {
    pub file: String,
    pub title: String,
    pub body: String,
    pub revision: String,
    pub repository: String,
    pub route: Route,
}

pub struct Corpus {
    pub descriptor: Descriptor,
    pub generation: String,
    pub documents: Vec<Document>,
    pub coverage: Coverage,
}

pub fn level(
    cache: &RepoCache,
    registry: &RegistryConfig,
    index: &str,
    path: &[String],
) -> Result<LoadedIndex> {
    let root = registry
        .indexes
        .get(index)
        .ok_or_else(|| AIexError::IndexNotFound { name: index.into() })?;
    let policy = match root.refresh {
        Some(RefreshSetting::Every(d)) => RefreshPolicy::Every(d),
        Some(RefreshSetting::Never) => RefreshPolicy::Never,
        None => cache.default_refresh(),
    };
    let budget = TraversalBudget::new(64, 512);
    let resolver = Resolver::with_budget(cache, &budget);
    let mut loaded = resolver.load_index_with_refresh(
        &root.repo,
        &root.path,
        &CommandPolicyChain::default(),
        policy,
    )?;
    for step in path {
        loaded = descend(&resolver, &loaded, step)?;
    }
    Ok(loaded)
}

/// Discover only through currently registered cached metadata. No Git jobs start.
#[derive(Default)]
pub struct Inventory {
    pub sources: Vec<Descriptor>,
    pub unavailable_levels: usize,
    pub uncached_repositories: usize,
    pub limited: bool,
}

pub fn inventory(
    cache: &RepoCache,
    registry: &RegistryConfig,
    deadline: Instant,
    selected_index: Option<&str>,
    start_path: &[String],
) -> Inventory {
    let mut inventory = Inventory::default();
    let cache = cache.cached_only();
    let mut found = BTreeMap::new();
    let mut uncached = HashSet::new();
    for index in registry.indexes.keys() {
        if selected_index.is_some_and(|selected| selected != index) {
            continue;
        }
        let mut pending = vec![start_path.to_vec()];
        let mut visited = HashSet::new();
        while let Some(path) = pending.pop() {
            if Instant::now() >= deadline || visited.len() >= 256 || path.len() >= 64 {
                inventory.limited = true;
                break;
            }
            let Ok(loaded) = level(&cache, registry, index, &path) else {
                inventory.unavailable_levels += 1;
                continue;
            };
            let key = format!(
                "{}:{}",
                cache.path_for(&loaded.origin).display(),
                loaded.rel_path
            );
            if !visited.insert(key) {
                continue;
            }
            if loaded
                .entries
                .iter()
                .any(|e| matches!(e.entry.kind, EntryKind::Doc { .. }))
            {
                let source = Descriptor::curated(index, &path, &loaded);
                found.insert(source.id.clone(), source);
            }
            for entry in &loaded.entries {
                match &entry.entry.kind {
                    EntryKind::Index { .. } => {
                        let mut next = path.clone();
                        next.push(entry.view.id.clone());
                        pending.push(next);
                    }
                    EntryKind::Doc { .. } | EntryKind::Command { .. } => {
                        if cache.cached_snapshot(&entry.origin, entry.refresh).is_ok() {
                            let source = Descriptor::repository(
                                index,
                                &path,
                                &entry.view.id,
                                &entry.origin,
                                &loaded,
                            );
                            found.entry(source.id.clone()).or_insert(source);
                        } else {
                            uncached.insert(cache.path_for(&entry.origin));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    inventory.uncached_repositories = uncached.len();
    inventory.sources = found.into_values().collect();
    inventory
}

fn documentation(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(name.as_str(), "readme" | "changelog" | "contributing")
        || matches!(
            path.extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "md" | "markdown" | "mdx" | "rst" | "adoc" | "asciidoc" | "txt"
        )
}

fn excluded_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".local"
            | ".cache"
            | ".claude"
            | ".codex"
            | ".agents"
            | "node_modules"
            | "vendor"
            | "target"
            | "build"
            | "dist"
            | "generated"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

pub fn prepare(
    cache: &RepoCache,
    registry: &RegistryConfig,
    descriptor: Descriptor,
    config: &SearchConfig,
    deadline: Instant,
) -> Result<Corpus> {
    let config = config.for_source(&descriptor.route.index, &descriptor.route.path);
    if !config.enabled {
        return Err(AIexError::Other(
            "ranked search is disabled for this source".into(),
        ));
    }
    let cache = cache.cached_only();
    let loaded = level(
        &cache,
        registry,
        &descriptor.route.index,
        &descriptor.route.path,
    )?;
    let (include, exclude) = config.filters()?;
    let mut candidates = Vec::<(PathBuf, String, String, Route)>::new();
    let mut coverage = Coverage {
        cached_only: true,
        ..Default::default()
    };
    if let Some(expected) = &descriptor.repo {
        let entry = loaded.find(&descriptor.route.entry_id).ok_or_else(|| {
            AIexError::Other("cached source is no longer registered; repeat discovery".into())
        })?;
        if &entry.origin != expected {
            return Err(AIexError::Other(
                "cached source repository changed; repeat discovery".into(),
            ));
        }
        let dir = cache.cached_snapshot(expected, entry.refresh)?;
        let mut pending = vec![dir.clone()];
        let mut examined = 0;
        while let Some(directory) = pending.pop() {
            if Instant::now() >= deadline {
                return Err(AIexError::RefreshPending);
            }
            if examined >= config.max_files.saturating_mul(20) {
                coverage.limited = true;
                break;
            }
            let remaining = config.max_files.saturating_mul(20).saturating_sub(examined);
            let mut entries: Vec<_> = std::fs::read_dir(directory)?
                .take(remaining + 1)
                .collect::<std::io::Result<_>>()?;
            if entries.len() > remaining {
                coverage.limited = true;
                entries.truncate(remaining);
            }
            entries.sort_by_key(|e| e.file_name());
            for item in entries {
                examined += 1;
                let ty = item.file_type()?;
                if ty.is_symlink() {
                    coverage.skipped_files += 1;
                    continue;
                }
                let file = item.path();
                if ty.is_dir() {
                    if !excluded_directory(&item.file_name().to_string_lossy()) {
                        pending.push(file);
                    }
                } else if ty.is_file() {
                    let relative = file
                        .strip_prefix(&dir)
                        .map_err(|_| AIexError::Other("invalid cached document path".into()))?;
                    let Some(relative) = relative.to_str() else {
                        coverage.skipped_files += 1;
                        continue;
                    };
                    let relative = relative.replace('\\', "/");
                    if exclude.is_match(&relative)
                        || (!include.is_empty() && !include.is_match(&relative))
                    {
                        coverage.skipped_files += 1;
                        continue;
                    }
                    if include.is_empty() && !documentation(&file) {
                        continue;
                    }
                    candidates.push((
                        dir.clone(),
                        relative.clone(),
                        relative,
                        descriptor.route.clone(),
                    ));
                    if candidates.len() >= config.max_files {
                        coverage.limited = true;
                        break;
                    }
                }
            }
            if coverage.limited {
                break;
            }
        }
    } else {
        for entry in &loaded.entries {
            if let EntryKind::Doc { path, .. } = &entry.entry.kind {
                if exclude.is_match(path) || (!include.is_empty() && !include.is_match(path)) {
                    coverage.skipped_files += 1;
                    continue;
                }
                match cache.ensure_path(&entry.origin, entry.refresh, path) {
                    Ok((dir, _)) => candidates.push((
                        dir,
                        path.clone(),
                        entry.view.name.clone(),
                        Route {
                            index: descriptor.route.index.clone(),
                            path: descriptor.route.path.clone(),
                            entry_id: entry.view.id.clone(),
                        },
                    )),
                    Err(_) => coverage.missing_files += 1,
                }
            }
        }
    }
    let mut documents = Vec::new();
    let mut bytes = 0;
    let mut identities = Vec::new();
    for (dir, file, title, route) in candidates {
        if Instant::now() >= deadline {
            return Err(AIexError::RefreshPending);
        }
        if documents.len() >= config.max_files {
            coverage.limited = true;
            break;
        }
        let size = std::fs::metadata(dir.join(&file))
            .map(|m| m.len())
            .unwrap_or(u64::MAX);
        if size > config.max_file_bytes as u64 {
            coverage.skipped_files += 1;
            continue;
        }
        if bytes + size as usize > config.max_corpus_bytes {
            coverage.limited = true;
            break;
        }
        let body = match cache.read_file(&dir, &file) {
            Ok(body) if body.len() <= config.max_file_bytes && !body.contains('\0') => body,
            _ => {
                coverage.skipped_files += 1;
                continue;
            }
        };
        if body.trim().is_empty() {
            coverage.skipped_files += 1;
            continue;
        }
        bytes += body.len();
        let revision = cache.snapshot_revision(&dir)?;
        identities.push(serde_json::json!([
            &file,
            &title,
            &route,
            &revision,
            digest(&body)
        ]));
        let origin = &loaded
            .find(&route.entry_id)
            .ok_or_else(|| AIexError::EntryNotFound {
                id: route.entry_id.clone(),
            })?
            .origin;
        let repository = digest(origin.url.trim_end_matches('/').trim_end_matches(".git"));
        documents.push(Document {
            repository,
            file,
            title,
            body,
            revision,
            route,
        });
    }
    coverage.indexed_files = documents.len();
    let generation = digest(
        serde_json::to_vec(&serde_json::json!([
            "corpus-v1",
            config.chunk_bytes,
            &coverage,
            identities
        ]))
        .unwrap(),
    );
    Ok(Corpus {
        descriptor,
        generation,
        documents,
        coverage,
    })
}
