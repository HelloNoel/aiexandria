//! Foreground updates use the same acquisitions as the MCP server.
use crate::cache::{atomic_json, now_ms, try_lock, RefreshPolicy, RepoCache, Selection};
use crate::command::CommandPolicyChain;
use crate::error::{AIexError, Result};
use crate::index::{EntryKind, GitRef};
use crate::registry::{RefreshSetting, RegistryConfig};
use crate::resolver::{descend, LoadedIndex, Resolver};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    pub index: Option<String>,
    pub path: Vec<String>,
    pub command_id: Option<String>,
    pub force: bool,
    pub wait: bool,
    pub dry_run: bool,
    pub timeout: Duration,
}
#[derive(Debug, Serialize)]
pub struct UpdateReport {
    pub status: String,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
    pub active_scope: Option<serde_json::Value>,
}
impl Default for UpdateReport {
    fn default() -> Self {
        Self {
            status: "completed".into(),
            updated: 0,
            unchanged: 0,
            skipped: 0,
            errors: vec![],
            active_scope: None,
        }
    }
}
impl UpdateOptions {
    pub fn validate(&self, registry: &RegistryConfig) -> Result<()> {
        let options = self;
        if options.index.is_none() && (!options.path.is_empty() || options.command_id.is_some()) {
            return Err(AIexError::Other(
                "--path and --command-id require --index".into(),
            ));
        }
        if options
            .index
            .as_ref()
            .is_some_and(|index| !registry.indexes.contains_key(index))
        {
            return Err(AIexError::Other("unknown index".into()));
        }
        if options.timeout.is_zero() {
            return Err(AIexError::Other("update timeout must be positive".into()));
        }
        Ok(())
    }
}

type Work = (GitRef, RefreshPolicy, Selection);
pub fn run(
    cache: &RepoCache,
    registry: &RegistryConfig,
    options: &UpdateOptions,
) -> Result<UpdateReport> {
    options.validate(registry)?;
    let deadline = Instant::now() + options.timeout;
    let requested_ms = now_ms();
    let scope_file = cache.root().join("update-scope.json");
    if options.dry_run {
        let mut report = UpdateReport {
            status: "planned".into(),
            ..Default::default()
        };
        report.active_scope = Some(
            serde_json::json!({"index":options.index,"path":options.path,"command_id":options.command_id,"cached_repositories":cache.cached_origins().len(),"network":false,"coverage":"cached inventory only; uncached metadata has not been traversed"}),
        );
        return Ok(report);
    }
    let _campaign = loop {
        if let Some(lock) = try_lock(&cache.root().join("update.lock"))? {
            break lock;
        }
        if !options.wait {
            return Ok(UpdateReport {
                status: "already_running".into(),
                active_scope: std::fs::read(&scope_file)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok()),
                ..Default::default()
            });
        }
        if Instant::now() >= deadline {
            return Ok(UpdateReport {
                status: "timed_out".into(),
                ..Default::default()
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    atomic_json(
        &scope_file,
        &serde_json::json!({"index":options.index,"path":options.path,"command_id":options.command_id,"started_ms":now_ms()}),
    )?;
    let cache = cache.for_update(deadline, options.force.then_some(requested_ms));
    let resolver = Resolver::new(&cache);
    let mut report = UpdateReport::default();
    let mut work: BTreeMap<PathBuf, Work> = BTreeMap::new();
    let mut visited = HashSet::new();
    for (name, root) in &registry.indexes {
        if options
            .index
            .as_ref()
            .is_some_and(|selected| selected != name)
        {
            continue;
        }
        let policy = match root.refresh {
            Some(RefreshSetting::Never) => RefreshPolicy::Never,
            Some(RefreshSetting::Every(d)) => RefreshPolicy::Every(d),
            None => cache.default_refresh(),
        };
        let prepared = (|| -> Result<()> {
            let before = cache.state(&root.repo).map(|s| s.checked_ms);
            let mut level = resolver.load_index_with_refresh(
                &root.repo,
                &root.path,
                &CommandPolicyChain::default(),
                policy,
            )?;
            if cache.state(&root.repo).map(|s| s.checked_ms) == before {
                report.unchanged += 1;
            } else {
                report.updated += 1;
            }
            for step in &options.path {
                level = descend(&resolver, &level, step)?;
            }
            if let Some(id) = &options.command_id {
                let entry = level
                    .find(id)
                    .ok_or_else(|| AIexError::EntryNotFound { id: id.clone() })?;
                if !matches!(entry.entry.kind, EntryKind::Command { .. }) {
                    return Err(AIexError::Other("selected entry is not a command".into()));
                }
                work.insert(
                    cache.path_for(&entry.origin),
                    (entry.origin.clone(), entry.refresh, Selection::all()),
                );
            } else {
                collect(
                    &cache,
                    &resolver,
                    level,
                    !options.path.is_empty(),
                    &mut visited,
                    &mut work,
                    &mut report,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            report.errors.push(format!("{name}: {error}"));
        }
    }
    let work = Arc::new(Mutex::new(VecDeque::from_iter(work.into_values())));
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let work = work.clone();
            let outcomes = outcomes.clone();
            let cache = &cache;
            scope.spawn(move || loop {
                if Instant::now() >= deadline {
                    break;
                }
                let Some((origin, policy, selection)) = work.lock().unwrap().pop_front() else {
                    break;
                };
                let before = cache.state(&origin).map(|s| s.checked_ms);
                let complete = selection.all;
                let result = cache
                    .update(
                        &origin,
                        policy,
                        selection,
                        options.force.then_some(requested_ms),
                        deadline,
                    )
                    .and_then(|dir| {
                        if complete {
                            cache.warm_execution_view(&dir)?;
                        }
                        Ok(cache.state(&origin).map(|s| s.checked_ms) != before)
                    });
                outcomes.lock().unwrap().push(result);
            });
        }
    });
    for result in outcomes.lock().unwrap().drain(..) {
        match result {
            Ok(true) => report.updated += 1,
            Ok(false) => report.unchanged += 1,
            Err(error) => report.errors.push(error.to_string()),
        }
    }
    if Instant::now() >= deadline || !work.lock().unwrap().is_empty() {
        report.status = "timed_out".into();
    } else if !report.errors.is_empty() {
        report.status = "failed".into();
    }
    cache.git.cancel();
    Ok(report)
}

fn collect(
    cache: &RepoCache,
    resolver: &Resolver<'_>,
    level: LoadedIndex,
    warm: bool,
    visited: &mut HashSet<String>,
    work: &mut BTreeMap<PathBuf, Work>,
    report: &mut UpdateReport,
) -> Result<()> {
    let key = format!(
        "{}:{}",
        serde_json::to_string(&level.origin).unwrap(),
        level.rel_path
    );
    if !visited.insert(key) {
        return Ok(());
    }
    if visited.len() > 128 {
        return Err(AIexError::TraversalLimit("update metadata nodes"));
    }
    for entry in &level.entries {
        match &entry.entry.kind {
            EntryKind::Index { .. } => collect(
                cache,
                resolver,
                descend(resolver, &level, &entry.view.id)?,
                warm,
                visited,
                work,
                report,
            )?,
            EntryKind::Doc { .. } | EntryKind::Command { .. } => {
                let previous = cache.state(&entry.origin);
                if !warm && previous.is_none() {
                    report.skipped += 1;
                    continue;
                }
                let mut selection = previous
                    .as_ref()
                    .and_then(|s| {
                        s.current
                            .as_ref()
                            .and_then(|n| s.snapshots.iter().find(|v| &v.name == n))
                    })
                    .map(|s| s.selection.clone())
                    .unwrap_or_default();
                match &entry.entry.kind {
                    EntryKind::Doc { path, .. } => {
                        selection.paths.insert(path.clone());
                    }
                    EntryKind::Command { .. } => selection = Selection::all(),
                    _ => unreachable!(),
                }
                let job = work
                    .entry(cache.path_for(&entry.origin))
                    .or_insert_with(|| (entry.origin.clone(), entry.refresh, Selection::default()));
                job.2.all |= selection.all;
                job.2.paths.extend(selection.paths);
            }
            _ => {}
        }
    }
    Ok(())
}
