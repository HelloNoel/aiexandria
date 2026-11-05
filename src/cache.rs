//! Published Git snapshots shared by MCP and update processes.
use crate::auth::AuthConfig;
use crate::error::{AIexError, Result};
use crate::git_cli::GitCli;
use crate::index::GitRef;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
mod build;
mod cleanup;
mod scheduler;
mod views;
pub use views::CommandView;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RefreshPolicy {
    #[default]
    Never,
    Every(Duration),
}
impl RefreshPolicy {
    pub fn from_duration(d: Duration) -> Self {
        if d.is_zero() {
            Self::Never
        } else {
            Self::Every(d)
        }
    }
    fn is_due(self, checked: u64) -> bool {
        match self {
            Self::Never => false,
            Self::Every(d) => now_ms().saturating_sub(checked) >= d.as_millis() as u64,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    #[default]
    CachedOk,
    RequireFresh,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Selection {
    pub all: bool,
    pub paths: BTreeSet<String>,
}
impl Selection {
    pub fn all() -> Self {
        Self {
            all: true,
            paths: BTreeSet::new(),
        }
    }
    pub fn path(path: &str) -> Result<Self> {
        validate_path(path)?;
        Ok(Self {
            all: false,
            paths: BTreeSet::from([path.into()]),
        })
    }
    fn covers(&self, other: &Self) -> bool {
        self.all || (!other.all && other.paths.is_subset(&self.paths))
    }
    fn extend(&mut self, other: &Self) {
        self.all |= other.all;
        self.paths.extend(other.paths.iter().cloned());
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub revision: String,
    pub selection: Selection,
    pub created_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheState {
    pub origin: GitRef,
    pub current: Option<String>,
    pub checked_ms: u64,
    #[serde(default)]
    pub attempted_ms: u64,
    pub snapshots: Vec<Snapshot>,
    pub failure: Option<String>,
    pub retry_ms: u64,
    pub failures: u32,
}
impl CacheState {
    fn new(origin: &GitRef) -> Self {
        Self {
            origin: origin.clone(),
            current: None,
            checked_ms: 0,
            attempted_ms: 0,
            snapshots: vec![],
            failure: None,
            retry_ms: 0,
            failures: 0,
        }
    }
    fn current(&self) -> Option<&Snapshot> {
        self.snapshots
            .iter()
            .find(|s| Some(&s.name) == self.current.as_ref())
    }
}
#[derive(Debug)]
struct Lease {
    snapshot: Snapshot,
    dir: PathBuf,
    _lock: File,
    checked_ms: u64,
    stale: bool,
    failure: Option<String>,
}
#[derive(Debug, Default)]
struct Session {
    leases: BTreeMap<PathBuf, Lease>,
    requested: BTreeMap<PathBuf, String>,
}
#[derive(Clone)]
pub struct RepoCache {
    root: PathBuf,
    auth: AuthConfig,
    default_refresh: RefreshPolicy,
    pub git: GitCli,
    freshness: Freshness,
    deadline: Option<Instant>,
    session: Option<Arc<Mutex<Session>>>,
    background: bool,
    force_since: Option<u64>,
    cache_only: bool,
}
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub(crate) fn lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}
pub(crate) fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut file, value).map_err(|e| AIexError::Other(e.to_string()))?;
    file.flush()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn jobs() -> &'static Mutex<HashSet<PathBuf>> {
    static JOBS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}
impl RepoCache {
    pub fn new(root: PathBuf, auth: AuthConfig) -> Self {
        let _ = std::fs::create_dir_all(&root);
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            auth,
            default_refresh: RefreshPolicy::Never,
            git: GitCli::default(),
            freshness: Freshness::CachedOk,
            deadline: None,
            session: None,
            background: false,
            force_since: None,
            cache_only: false,
        }
    }
    pub fn with_default_refresh(mut self, policy: RefreshPolicy) -> Self {
        self.default_refresh = policy;
        self
    }
    pub fn with_git(mut self, git: GitCli) -> Self {
        self.git = git;
        self
    }
    pub fn default_refresh(&self) -> RefreshPolicy {
        self.default_refresh
    }
    pub fn default_root() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| ".".into())
            .join("aiexandria/repos")
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Use only published snapshots. Never fetch, hydrate, or enqueue Git work.
    pub fn cached_only(&self) -> Self {
        let mut cache = self.clone();
        cache.cache_only = true;
        cache
    }
    /// Acquire a lease on the available selection without widening its checkout.
    pub fn cached_snapshot(&self, origin: &GitRef, policy: RefreshPolicy) -> Result<PathBuf> {
        self.cached_only()
            .acquire(origin, policy, Selection::default(), false, None)
            .map(|(dir, _)| dir)
    }
    pub fn snapshot_revision(&self, dir: &Path) -> Result<String> {
        let head = std::fs::read_to_string(dir.join(".git/HEAD"))?;
        let head = head.trim();
        if !matches!(head.len(), 40 | 64) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AIexError::Other(
                "not a published repository snapshot".into(),
            ));
        }
        Ok(head.into())
    }
    pub fn session(&self, freshness: Freshness, deadline: Instant) -> Self {
        let mut cache = self.clone();
        cache.freshness = freshness;
        cache.deadline = Some(deadline);
        cache.session = Some(Arc::new(Mutex::new(Session::default())));
        cache
    }
    pub(crate) fn background_session(&self, deadline: Instant) -> Self {
        let mut cache = self.session(Freshness::RequireFresh, deadline);
        cache.background = true;
        cache
    }
    pub fn for_update(&self, deadline: Instant, force_since: Option<u64>) -> Self {
        let mut cache = self.session(Freshness::RequireFresh, deadline);
        cache.git = self.git.independent();
        cache.background = true;
        cache.force_since = force_since;
        cache
    }
    pub fn ensure(&self, origin: &GitRef) -> Result<PathBuf> {
        self.ensure_with(origin, self.default_refresh)
    }
    pub fn ensure_with(&self, origin: &GitRef, policy: RefreshPolicy) -> Result<PathBuf> {
        self.ensure_with_status(origin, policy).map(|v| v.0)
    }
    pub(crate) fn ensure_with_status(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
    ) -> Result<(PathBuf, bool)> {
        self.acquire(origin, policy, Selection::all(), false, None)
    }
    pub fn ensure_path(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        path: &str,
    ) -> Result<(PathBuf, bool)> {
        self.acquire(origin, policy, Selection::path(path)?, false, None)
    }
    pub fn ensure_paths(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        paths: impl IntoIterator<Item = String>,
    ) -> Result<PathBuf> {
        let mut selection = Selection::default();
        for path in paths {
            selection.extend(&Selection::path(&path)?);
        }
        self.acquire(origin, policy, selection, false, None)
            .map(|v| v.0)
    }
    #[cfg(test)]
    pub(crate) fn ensure_fresh_until(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        deadline: Instant,
    ) -> Result<PathBuf> {
        self.session(Freshness::RequireFresh, deadline)
            .ensure_with(origin, policy)
    }
    pub fn update(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        selection: Selection,
        force_since: Option<u64>,
        deadline: Instant,
    ) -> Result<PathBuf> {
        let mut cache = self.clone();
        cache.session = None;
        cache.deadline = Some(deadline);
        cache.freshness = Freshness::RequireFresh;
        cache
            .acquire(origin, policy, selection, true, force_since)
            .map(|v| v.0)
    }
    pub(crate) fn path_for(&self, origin: &GitRef) -> PathBuf {
        let mut identity = serde_json::to_vec(&(
            &origin.url,
            &origin.branch,
            &origin.tag,
            &origin.commit,
            origin.submodules,
            &origin.token_env,
            self.auth.hosts.iter().collect::<BTreeMap<_, _>>(),
            self.auth
                .resolve(&origin.url, origin.token_env.as_deref())
                .ok()
                .flatten()
                .map(|t| (t.username, t.token)),
        ))
        .unwrap();
        // Submodules can use a different host. Partition their environment-based
        // credential profiles too, without persisting any credential values.
        let environment: BTreeMap<_, _> = self
            .auth
            .hosts
            .iter()
            .filter_map(|(host, auth)| match &auth.source {
                crate::auth::TokenSource::Env { token_env } => {
                    Some((host, std::env::var(token_env).ok()))
                }
                crate::auth::TokenSource::Inline { .. } => None,
            })
            .collect();
        if !environment.is_empty() {
            identity.extend(serde_json::to_vec(&environment).unwrap());
        }
        self.root
            .join("v3")
            .join(hex::encode(Sha256::digest(identity)))
    }
    pub fn state(&self, origin: &GitRef) -> Option<CacheState> {
        read_state(&self.path_for(origin))
    }
    pub fn cached_origins(&self) -> Vec<GitRef> {
        std::fs::read_dir(self.root.join("v3"))
            .into_iter()
            .flatten()
            .filter_map(|d| d.ok().and_then(|d| read_state(&d.path())).map(|s| s.origin))
            .collect()
    }
    fn acquire(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        selection: Selection,
        background: bool,
        force_since: Option<u64>,
    ) -> Result<(PathBuf, bool)> {
        let background = background || self.background;
        let force_since = force_since.or(self.force_since);
        origin.validate()?;
        self.auth
            .resolve(&origin.url, origin.token_env.as_deref())?;
        let base = self.path_for(origin);
        if self.cache_only && !base.is_dir() {
            return Err(AIexError::RefreshPending);
        }
        std::fs::create_dir_all(&base)?;
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
        let mut revision = None;
        if let Some(session) = &self.session {
            let s = session.lock().unwrap();
            if let Some(lease) = s.leases.get(&base) {
                if lease.snapshot.selection.covers(&selection) {
                    return Ok((lease.dir.clone(), lease.stale));
                }
                revision = Some(lease.snapshot.revision.clone());
            } else {
                revision = s.requested.get(&base).cloned();
            }
        }
        let requested_ms = now_ms();
        let mut demand = None;
        loop {
            let state = read_state(&base).unwrap_or_else(|| CacheState::new(origin));
            let wanted = revision
                .as_deref()
                .or_else(|| state.current().map(|s| s.revision.as_str()));
            let ready = state.snapshots.iter().rev().find(|s| {
                Some(s.revision.as_str()) == wanted
                    && s.selection.covers(&selection)
                    && intact(&base, s)
            });
            let due = origin.commit.is_none()
                && ((policy.is_due(state.checked_ms) && state.checked_ms < requested_ms)
                    || force_since.is_some_and(|t| state.checked_ms < t));
            if let Some(snapshot) = ready {
                let stale = due || state.failure.is_some();
                if !due || revision.is_some() || self.freshness == Freshness::CachedOk {
                    if due && !self.cache_only && now_ms() >= state.retry_ms {
                        self.start_job(
                            origin,
                            policy,
                            selection.clone(),
                            revision.clone(),
                            force_since,
                            &base,
                        );
                    }
                    let dir = base.join(&snapshot.name);
                    if dir.is_dir() {
                        if let Some(session) = &self.session {
                            let lock = lock_file(&base.join(format!("{}.lease", snapshot.name)))?;
                            lock.lock_shared()?;
                            session.lock().unwrap().leases.insert(
                                base.clone(),
                                Lease {
                                    snapshot: snapshot.clone(),
                                    dir: dir.clone(),
                                    _lock: lock,
                                    checked_ms: state.checked_ms,
                                    stale,
                                    failure: state.failure.clone(),
                                },
                            );
                        }
                        return Ok((dir, stale));
                    }
                }
            }
            if self.cache_only {
                return Err(AIexError::RefreshPending);
            }
            if Instant::now() >= deadline
                || self.git.cancelled.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AIexError::RefreshPending);
            }
            if state.retry_ms > now_ms() && force_since.is_none_or(|t| state.attempted_ms >= t) {
                return Err(AIexError::RefreshFailed(
                    state
                        .failure
                        .unwrap_or_else(|| "acquisition is backing off".into()),
                ));
            }
            if !background && demand.is_none() {
                let file = lock_file(&base.join("interactive.lock"))?;
                file.lock_shared()?;
                demand = Some(file);
            }
            self.start_job(
                origin,
                policy,
                selection.clone(),
                revision.clone(),
                force_since,
                &base,
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn start_job(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        selection: Selection,
        revision: Option<String>,
        force_since: Option<u64>,
        base: &Path,
    ) {
        {
            let mut jobs = jobs().lock().unwrap();
            if jobs.len() >= 128 || !jobs.insert(base.to_path_buf()) {
                return;
            }
        }
        let cache = self.clone();
        let origin = origin.clone();
        let base = base.to_path_buf();
        let worker = cache.git.worker();
        std::thread::spawn(move || {
            let _worker = worker;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                cache.work(&origin, policy, selection, revision, force_since, &base)
            }));
            if let Err(error) = result
                .unwrap_or_else(|_| Err(AIexError::Other("repository worker panicked".into())))
            {
                tracing::warn!(%error, "repository acquisition failed");
            }
            jobs().lock().unwrap().remove(&base);
        });
    }
    fn work(
        &self,
        origin: &GitRef,
        policy: RefreshPolicy,
        mut selection: Selection,
        revision: Option<String>,
        force_since: Option<u64>,
        base: &Path,
    ) -> Result<()> {
        let Some(_owner) = try_lock(&base.join("job.lock"))? else {
            return Ok(());
        };
        let mut state = read_state(base).unwrap_or_else(|| CacheState::new(origin));
        if state.retry_ms > now_ms() && force_since.is_none_or(|t| state.attempted_ms >= t) {
            return Ok(());
        }
        let previous = revision
            .as_ref()
            .and_then(|r| state.snapshots.iter().rev().find(|s| &s.revision == r))
            .or_else(|| state.current())
            .cloned();
        let due = origin.commit.is_none()
            && (policy.is_due(state.checked_ms)
                || force_since.is_some_and(|t| state.checked_ms < t));
        if previous
            .as_ref()
            .is_some_and(|s| s.selection.covers(&selection) && intact(base, s))
            && (!due || revision.is_some())
        {
            return Ok(());
        }
        if let Some(previous) = &previous {
            selection.extend(&previous.selection);
        }
        let deadline = Instant::now() + self.git.timeout;
        let _slot = self.admit(base, deadline)?;
        let mut worker = self.clone();
        worker
            .git
            .held_locks
            .extend([Arc::new(_owner), Arc::new(_slot)]);
        state.attempted_ms = now_ms();
        let result = worker.build(
            origin,
            previous.as_ref().map(|s| base.join(&s.name)),
            &selection,
            build::Target {
                revision: revision.as_deref().or_else(|| {
                    origin
                        .commit
                        .as_ref()
                        .and_then(|_| previous.as_ref().map(|s| s.revision.as_str()))
                }),
                due,
            },
            base,
            deadline,
        );
        match result {
            Ok(snapshot) => {
                if revision.is_none()
                    || state
                        .current()
                        .is_some_and(|s| s.revision == snapshot.revision)
                {
                    state.current = Some(snapshot.name.clone());
                }
                if revision.is_none() && (due || previous.is_none()) {
                    state.checked_ms = now_ms();
                }
                if !state.snapshots.iter().any(|s| s.name == snapshot.name) {
                    state.snapshots.push(snapshot);
                }
                state.failure = None;
                state.retry_ms = 0;
                state.failures = 0;
                atomic_json(&base.join("state.json"), &state)?;
                if let Err(error) = self.cleanup(base, &mut state) {
                    tracing::warn!(%error, "snapshot cleanup deferred");
                }
            }
            Err(error) => {
                state.failures = state.failures.saturating_add(1);
                state.retry_ms = now_ms()
                    + (5000u64.saturating_mul(1u64 << state.failures.saturating_sub(1).min(4)))
                        .min(60000)
                    + now_ms() % 1000;
                state.failure = Some(error.to_string());
                atomic_json(&base.join("state.json"), &state)?;
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn read_file(&self, repo_root: &Path, rel_path: &str) -> Result<String> {
        validate_path(rel_path)?;
        let root = repo_root.canonicalize()?;
        let path = repo_root
            .join(rel_path)
            .canonicalize()
            .map_err(|_| AIexError::Other(format!("file not found in checkout: {rel_path}")))?;
        if !path.starts_with(root) {
            return Err(AIexError::Other("path escapes repo root".into()));
        }
        Ok(std::fs::read_to_string(path)?)
    }
    pub fn apply_snapshot(&self, origin: &GitRef, token: &str) -> Result<()> {
        if token.is_empty()
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(AIexError::Other("invalid snapshot token".into()));
        }
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.root.join("sessions").join(token))
                .map_err(|_| AIexError::Other("snapshot expired; repeat search".into()))?,
        )
        .map_err(|e| AIexError::Other(e.to_string()))?;
        if value["expires_ms"].as_u64().unwrap_or(0) < now_ms() {
            return Err(AIexError::Other("snapshot expired; repeat search".into()));
        }
        let base = self.path_for(origin);
        let key = base.file_name().unwrap().to_string_lossy();
        let revision = value["revisions"][key.as_ref()]
            .as_str()
            .ok_or_else(|| AIexError::Other("snapshot does not cover this repository".into()))?;
        if let Some(session) = &self.session {
            let mut s = session.lock().unwrap();
            s.leases.remove(&base);
            s.requested.insert(base, revision.into());
        }
        Ok(())
    }
    pub fn response_metadata(&self) -> serde_json::Value {
        let Some(session) = &self.session else {
            return serde_json::json!({"status":"ready"});
        };
        self.prune_tokens();
        let s = session.lock().unwrap();
        let repos = s.leases.iter().map(|(base, l)| (base.file_name().unwrap().to_string_lossy().into_owned(), serde_json::json!({"revision":l.snapshot.revision,"checked_ms":l.checked_ms,"stale":l.stale,"refresh":if l.failure.is_some(){"failed"}else if l.stale{"pending"}else{"idle"}}))).collect::<BTreeMap<_,_>>();
        let revisions: BTreeMap<_, _> = repos
            .iter()
            .map(|(key, value)| (key.clone(), value["revision"].clone()))
            .collect();
        let token = (|| -> Result<String> {
            let root = self.root.join("sessions");
            std::fs::create_dir_all(&root)?;
            let mut file = tempfile::Builder::new().prefix("read-").tempfile_in(root)?;
            serde_json::to_writer(
                &mut file,
                &serde_json::json!({"expires_ms":now_ms()+900_000,"revisions":revisions}),
            )
            .map_err(|e| AIexError::Other(e.to_string()))?;
            let (_, path) = file.keep().map_err(|e| e.error)?;
            Ok(path.file_name().unwrap().to_string_lossy().into_owned())
        })()
        .ok();
        let count = repos.len();
        let stale = repos.values().filter(|v| v["stale"] == true).count();
        let repos: BTreeMap<_, _> = repos.into_iter().take(8).collect();
        serde_json::json!({"status":"ready","repositories":repos,"repository_count":count,"stale_repositories":stale,"snapshot":token})
    }
}
fn read_state(base: &Path) -> Option<CacheState> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join("state.json")).ok()?).ok()?;
    // Older cache records omitted submodules only when it was true. The new
    // Pinakes default must not reinterpret those already-acquired repositories.
    value
        .get_mut("origin")?
        .as_object_mut()?
        .entry("submodules")
        .or_insert(serde_json::Value::Bool(true));
    serde_json::from_value(value).ok()
}
fn escape_pattern(path: &str) -> String {
    path.chars()
        .flat_map(|c| {
            if "\\*?[]!# ".contains(c) {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}
pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\n', '\r', '\0'])
        || Path::new(path).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(AIexError::Other(
            "path escapes repo root: absolute paths and parent traversal are forbidden".into(),
        ));
    }
    Ok(())
}

fn intact(base: &Path, snapshot: &Snapshot) -> bool {
    std::fs::read_to_string(base.join(&snapshot.name).join(".git/HEAD"))
        .is_ok_and(|head| head.trim() == snapshot.revision)
}
