//! Shallow-clone repos to a local cache and re-use them across queries.
//!
//! Cache layout: `<cache_root>/v2/<sha256(repository, ref kind/value, options)>/`. Each identity gets its own
//! checkout so different branches/tags don't fight over a single working tree.
//!
//! Refresh policy: when a repo is already cloned, we re-fetch and fast-forward
//! the working tree if the last fetch was longer than `RefreshPolicy::interval`
//! ago. The freshness sentinel is the mtime of `.pinakes-fetched` in the
//! checkout root (not `.git/FETCH_HEAD`, which git rewrites for unrelated reasons).
//! Pinned refs (`commit:` set) are immutable and never refreshed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use git2::{build::RepoBuilder, Cred, FetchOptions, RemoteCallbacks, Repository};
use sha2::{Digest, Sha256};

use crate::auth::{AuthConfig, ResolvedToken};
use crate::error::{AIexError, Result};
use crate::index::GitRef;

const FRESHNESS_FILE: &str = ".pinakes-fetched";
const READY_FILE: &str = "aiexandria-ready";

/// How long `ensure_with` waits for a cold clone inline before returning
/// [`AIexError::Warming`]. Small repos finish well within this; large ones
/// keep cloning in the background and succeed on a retry.
const INLINE_CLONE_BUDGET: Duration = Duration::from_secs(8);

/// A background clone in progress. `result` is `None` while running, then
/// `Some(Ok)` on success or `Some(Err(message))` on failure.
struct CloneJob {
    result: Mutex<Option<std::result::Result<(), String>>>,
}

/// Per-cache-dir mutex registry. Concurrent `ensure_with` calls for the same
/// repo serialize on the same `Arc<Mutex<()>>` so only one does the slow
/// refresh while the others wait. Keyed by the absolute cache dir.
fn dir_lock(dir: &Path) -> Arc<Mutex<()>> {
    static REGISTRY: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
        OnceLock::new();
    let map = REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut guard = map.lock().expect("dir_lock registry poisoned");
    guard
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Registry of in-flight cold clones, keyed by cache dir. Ensures a repo is
/// only cloned once even if several requests race on a cold cache.
fn clone_registry() -> &'static Mutex<std::collections::HashMap<PathBuf, Arc<CloneJob>>> {
    static REGISTRY: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<CloneJob>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Get (or start) the background clone job for `dir`. The clone runs on a
/// detached thread; callers poll `job.result`.
fn clone_job(dir: &Path, git_ref: &GitRef, auth: &AuthConfig) -> Arc<CloneJob> {
    let mut reg = clone_registry().lock().expect("clone registry poisoned");
    if let Some(job) = reg.get(dir) {
        return job.clone();
    }
    let job = Arc::new(CloneJob {
        result: Mutex::new(None),
    });
    reg.insert(dir.to_path_buf(), job.clone());
    drop(reg);

    let dir = dir.to_path_buf();
    let git_ref = git_ref.clone();
    let auth = auth.clone();
    let job_for_thread = job.clone();
    std::thread::spawn(move || {
        // Serialize with any refresh on the same dir.
        let lock = dir_lock(&dir);
        let _guard = lock.lock().expect("dir lock poisoned");
        let outcome = do_clone(&dir, &git_ref, &auth).map_err(|e| e.to_string());
        *job_for_thread.result.lock().expect("clone job poisoned") = Some(outcome);
    });
    job
}

// Completed jobs remain available to callers that previously stopped waiting.
// Only running jobs count against the limit; no queue of blocked refresh threads.
const MAX_REFRESH_JOBS: usize = 8;

#[derive(Clone)]
struct RefreshFailure {
    message: String,
    requires_auth: bool,
}

struct RefreshJob {
    started: Instant,
    result: Mutex<Option<std::result::Result<(), RefreshFailure>>>,
}

fn refresh_registry() -> &'static Mutex<std::collections::HashMap<PathBuf, Arc<RefreshJob>>> {
    static REGISTRY: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<RefreshJob>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn refresh_job(
    cache: &RepoCache,
    dir: &Path,
    git_ref: &GitRef,
    policy: RefreshPolicy,
) -> Option<Arc<RefreshJob>> {
    let mut jobs = refresh_registry()
        .lock()
        .expect("refresh registry poisoned");
    if let Some(job) = jobs.get(dir) {
        return Some(job.clone());
    }
    if jobs
        .values()
        .filter(|job| job.result.lock().expect("refresh job poisoned").is_none())
        .count()
        >= MAX_REFRESH_JOBS
    {
        return None;
    }
    let job = Arc::new(RefreshJob {
        started: Instant::now(),
        result: Mutex::new(None),
    });
    jobs.insert(dir.to_path_buf(), job.clone());
    drop(jobs);
    let cache = cache.clone();
    let dir = dir.to_path_buf();
    let git_ref = git_ref.clone();
    let worker_job = job.clone();
    let spawned = std::thread::Builder::new()
        .name("repo-refresh".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let lock = dir_lock(&dir);
                let _guard = lock.lock().expect("dir lock poisoned");
                if git_ref.commit.is_none() && cache.refresh_due(&dir, policy) {
                    cache.refresh(&dir, &git_ref)?;
                    std::fs::write(dir.join(FRESHNESS_FILE), b"")?;
                }
                Ok::<(), AIexError>(())
            }))
            .unwrap_or_else(|_| {
                Err(AIexError::Other(
                    "repository refresh worker panicked".into(),
                ))
            })
            .map_err(|error| RefreshFailure {
                requires_auth: matches!(error, AIexError::MissingToken { .. }),
                message: error.to_string(),
            });
            *worker_job.result.lock().expect("refresh job poisoned") = Some(result);
        });
    if let Err(error) = spawned {
        *job.result.lock().expect("refresh job poisoned") = Some(Err(RefreshFailure {
            message: error.to_string(),
            requires_auth: false,
        }));
    }
    Some(job)
}

fn wait_for_refresh(
    dir: &Path,
    git_ref: &GitRef,
    job: Arc<RefreshJob>,
    deadline: Option<Instant>,
) -> Result<bool> {
    loop {
        let result = job.result.lock().expect("refresh job poisoned").clone();
        if let Some(result) = result {
            let mut jobs = refresh_registry()
                .lock()
                .expect("refresh registry poisoned");
            if jobs
                .get(dir)
                .is_some_and(|current| Arc::ptr_eq(current, &job))
            {
                jobs.remove(dir);
            }
            return match result {
                Ok(()) => Ok(false),
                Err(error) if deadline.is_some() || error.requires_auth => {
                    Err(AIexError::RefreshFailed(error.message))
                }
                Err(error) => {
                    tracing::warn!(
                        url = git_ref.url,
                        error = error.message,
                        "refresh failed, keeping cached copy"
                    );
                    Ok(true)
                }
            };
        }
        if deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(AIexError::RefreshPending);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
pub(crate) fn checkout_lock_for_test(dir: &Path) -> Arc<Mutex<()>> {
    dir_lock(dir)
}

/// The actual clone + checkout + submodule init. Blocking; run on a worker
/// thread via [`clone_job`].
fn do_clone(dir: &Path, git_ref: &GitRef, auth: &AuthConfig) -> Result<()> {
    if checkout_ready(dir, git_ref) {
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    let parent = dir
        .parent()
        .ok_or_else(|| AIexError::Other("cache has no parent".into()))?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix("clone-")
        .tempdir_in(parent)?;
    let token = auth.resolve(&git_ref.url, git_ref.token_env.as_deref())?;
    let mut builder = RepoBuilder::new();
    let mut options = FetchOptions::new();
    // Historical pins and tags need more than the default branch's shallow tip.
    if !git_ref.url.starts_with("file://") && git_ref.commit.is_none() && git_ref.tag.is_none() {
        options.depth(1);
    }
    options.remote_callbacks(make_callbacks(token.clone()));
    builder.fetch_options(options);
    if let Some(branch) = &git_ref.branch {
        builder.branch(branch);
    }
    let repo = builder.clone(&git_ref.url, staging.path())?;
    if let Some(commit) = &git_ref.commit {
        let oid = match resolve_commit(&repo, commit) {
            Ok(oid) => oid,
            Err(original) => {
                // Servers may allow fetching a full object ID that no advertised ref reaches.
                if commit.len() != 40 {
                    return Err(original.into());
                }
                let mut fetch = FetchOptions::new();
                fetch.remote_callbacks(make_callbacks(token));
                repo.find_remote("origin")?
                    .fetch(&[commit.as_str()], Some(&mut fetch), None)?;
                resolve_commit(&repo, commit)?
            }
        };
        let object = repo.find_commit(oid)?;
        repo.checkout_tree(object.as_object(), None)?;
        repo.set_head_detached(oid)?;
    } else if let Some(tag) = &git_ref.tag {
        let object = repo
            .revparse_single(&format!("refs/tags/{tag}"))?
            .peel_to_commit()?;
        repo.checkout_tree(object.as_object(), None)?;
        repo.set_head_detached(object.id())?;
    }
    if git_ref.submodules {
        update_submodules(&repo, auth)?;
    }
    std::fs::write(repo.path().join(READY_FILE), b"1")?;
    std::fs::write(staging.path().join(FRESHNESS_FILE), b"")?;
    drop(repo);
    // No request can mistake a partial clone for a usable checkout.
    std::fs::rename(staging.path(), dir)?;
    Ok(())
}

fn resolve_commit(repo: &Repository, value: &str) -> std::result::Result<git2::Oid, git2::Error> {
    let object = repo.find_object_by_prefix(value, Some(git2::ObjectType::Commit))?;
    Ok(object.peel_to_commit()?.id())
}

fn checkout_ready(dir: &Path, git_ref: &GitRef) -> bool {
    if !dir.join(".git").join(READY_FILE).is_file() {
        return false;
    }
    let Ok(repo) = Repository::open(dir) else {
        return false;
    };
    if let Some(pin) = &git_ref.commit {
        let Ok(expected) = resolve_commit(&repo, pin) else {
            return false;
        };
        return repo.head().ok().and_then(|head| head.target()) == Some(expected);
    }
    true
}

fn wait_for_clone(
    dir: &Path,
    git_ref: &GitRef,
    job: Arc<CloneJob>,
    budget: Duration,
) -> Result<PathBuf> {
    let start = std::time::Instant::now();
    loop {
        let result = job.result.lock().expect("clone job poisoned").clone();
        if let Some(result) = result {
            let mut jobs = clone_registry().lock().expect("clone registry poisoned");
            if jobs
                .get(dir)
                .is_some_and(|current| Arc::ptr_eq(current, &job))
            {
                jobs.remove(dir);
            }
            return result.map(|()| dir.to_path_buf()).map_err(AIexError::Other);
        }
        if start.elapsed() >= budget {
            return Err(AIexError::Warming {
                url: git_ref.url.clone(),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How often to re-fetch a cached repo. `Never` disables auto-refresh.
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

    fn is_due(&self, last_fetch: SystemTime, now: SystemTime) -> bool {
        match self {
            RefreshPolicy::Never => false,
            RefreshPolicy::Every(d) => match now.duration_since(last_fetch) {
                Ok(elapsed) => elapsed >= *d,
                Err(_) => false, // clock skew: treat as fresh
            },
        }
    }
}

#[derive(Clone)]
pub struct RepoCache {
    root: PathBuf,
    auth: AuthConfig,
    default_refresh: RefreshPolicy,
}

impl RepoCache {
    pub fn new(root: PathBuf, auth: AuthConfig) -> Self {
        Self {
            root,
            auth,
            default_refresh: RefreshPolicy::Never,
        }
    }

    pub fn with_default_refresh(mut self, policy: RefreshPolicy) -> Self {
        self.default_refresh = policy;
        self
    }

    pub fn default_refresh(&self) -> RefreshPolicy {
        self.default_refresh
    }

    pub fn default_root() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aiexandria")
            .join("repos")
    }

    /// Ensure the repo is checked out, refreshing per the cache's default policy.
    pub fn ensure(&self, git_ref: &GitRef) -> Result<PathBuf> {
        self.ensure_with(git_ref, self.default_refresh)
    }

    /// Ensure the repo is checked out under an explicit refresh policy override.
    ///
    /// A cold clone runs on a background thread so a slow first fetch (large
    /// repo, VPN) doesn't block the caller long enough for an MCP client to
    /// time out. We wait a bounded budget inline; if the clone is still
    /// running, we return [`AIexError::Warming`], which signals the caller to
    /// retry shortly. A warm cache (or a fast clone that finishes within the
    /// budget) returns the path directly.
    pub fn ensure_with(&self, git_ref: &GitRef, policy: RefreshPolicy) -> Result<PathBuf> {
        self.ensure_with_status(git_ref, policy).map(|(dir, _)| dir)
    }

    /// Whether acquisition fell back to a cached checkout after a failed refresh.
    pub(crate) fn ensure_with_status(
        &self,
        git_ref: &GitRef,
        policy: RefreshPolicy,
    ) -> Result<(PathBuf, bool)> {
        self.ensure_until(git_ref, policy, None)
    }

    /// Search preparation requires fresh data and shares one request-wide deadline.
    pub(crate) fn ensure_fresh_until(
        &self,
        git_ref: &GitRef,
        policy: RefreshPolicy,
        deadline: Instant,
    ) -> Result<PathBuf> {
        self.ensure_until(git_ref, policy, Some(deadline))
            .map(|(dir, _)| dir)
    }

    fn ensure_until(
        &self,
        git_ref: &GitRef,
        policy: RefreshPolicy,
        deadline: Option<Instant>,
    ) -> Result<(PathBuf, bool)> {
        let requested = Instant::now();
        git_ref.validate()?;
        self.auth
            .resolve(&git_ref.url, git_ref.token_env.as_deref())?;
        let dir = self.path_for(git_ref);
        loop {
            if deadline.is_some_and(|end| Instant::now() >= end) {
                return Err(AIexError::RefreshPending);
            }
            let clone = clone_registry()
                .lock()
                .expect("clone registry poisoned")
                .get(&dir)
                .cloned();
            if clone.is_some() || !checkout_ready(&dir, git_ref) {
                let job = clone.unwrap_or_else(|| clone_job(&dir, git_ref, &self.auth));
                let budget = deadline.map_or(INLINE_CLONE_BUDGET, |end| {
                    end.saturating_duration_since(Instant::now())
                });
                let cloned =
                    wait_for_clone(&dir, git_ref, job, budget).map_err(|error| match error {
                        AIexError::Warming { .. } if deadline.is_some() => {
                            AIexError::RefreshPending
                        }
                        other => other,
                    })?;
                // A completed clone may have gone unobserved long enough to need a refresh.
                if deadline.is_some() {
                    continue;
                }
                return Ok((cloned, false));
            }
            let existing = refresh_registry()
                .lock()
                .expect("refresh registry poisoned")
                .get(&dir)
                .cloned();
            if existing.is_none() && (git_ref.commit.is_some() || !self.refresh_due(&dir, policy)) {
                return Ok((dir, false));
            }
            if let Some(job) = existing.or_else(|| refresh_job(self, &dir, git_ref, policy)) {
                let predates_request = job.started < requested;
                let stale = wait_for_refresh(&dir, git_ref, job, deadline)?;
                // An old completed job does not suppress a refresh which is now due.
                if !stale
                    && predates_request
                    && git_ref.commit.is_none()
                    && self.refresh_due(&dir, policy)
                {
                    continue;
                }
                return Ok((dir, stale));
            }
            // All refresh slots are occupied. Keep the request's original deadline.
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn refresh_due(&self, dir: &Path, policy: RefreshPolicy) -> bool {
        let last = std::fs::metadata(dir.join(FRESHNESS_FILE))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        policy.is_due(last, SystemTime::now())
    }

    fn refresh(&self, dir: &Path, git_ref: &GitRef) -> Result<()> {
        let token = self
            .auth
            .resolve(&git_ref.url, git_ref.token_env.as_deref())?;
        let repo = Repository::open(dir)?;
        let mut remote = repo.find_remote("origin")?;

        let mut fopts = FetchOptions::new();
        if !git_ref.url.starts_with("file://") {
            fopts.depth(1);
        }
        fopts.remote_callbacks(make_callbacks(token));

        let refspec = git_ref
            .branch
            .as_ref()
            .map(|branch| format!("refs/heads/{branch}"))
            .or_else(|| git_ref.tag.as_ref().map(|tag| format!("refs/tags/{tag}")))
            .unwrap_or_else(|| "HEAD".to_string());
        remote.fetch(&[refspec.as_str()], Some(&mut fopts), None)?;

        // Fast-forward the working tree to the freshly fetched ref.
        let fetch_head = repo.find_reference("FETCH_HEAD")?;
        let target = fetch_head.peel(git2::ObjectType::Commit)?;
        repo.checkout_tree(&target, None)?;
        repo.set_head_detached(target.id())?;
        if git_ref.submodules {
            update_submodules(&repo, &self.auth)?;
        }
        Ok(())
    }

    pub(crate) fn path_for(&self, git_ref: &GitRef) -> PathBuf {
        // Versioned layout intentionally never trusts older, potentially partial checkouts.
        let identity = serde_json::to_vec(&(
            &git_ref.url,
            &git_ref.branch,
            &git_ref.tag,
            &git_ref.commit,
            git_ref.submodules,
        ))
        .expect("serializing cache identity");
        self.root
            .join("v2")
            .join(hex::encode(Sha256::digest(identity)))
    }

    /// Read a file from a checkout, validating the path stays inside the repo.
    pub fn read_file(&self, repo_root: &Path, rel_path: &str) -> Result<String> {
        if rel_path.is_empty()
            || Path::new(rel_path).components().any(|part| {
                matches!(
                    part,
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
        let candidate = repo_root.join(rel_path);
        let canon_root = repo_root.canonicalize()?;
        let canon_candidate = candidate
            .canonicalize()
            .map_err(|_| AIexError::Other(format!("file not found in checkout: {rel_path}")))?;
        if !canon_candidate.starts_with(&canon_root) {
            return Err(AIexError::Other(format!(
                "path escapes repo root: {rel_path}"
            )));
        }
        Ok(std::fs::read_to_string(canon_candidate)?)
    }
}

fn make_callbacks<'a>(token: Option<ResolvedToken>) -> RemoteCallbacks<'a> {
    let mut cb = RemoteCallbacks::new();
    // Load the user's git config once so we can fall back to their configured
    // credential helper (macOS keychain, `gh auth setup-git`, store, …).
    let git_config = git2::Config::open_default().ok();
    cb.credentials(move |url, username_from_url, allowed| {
        // 1. Explicit token (per-entry token_env or central host map).
        if let Some(t) = &token {
            if allowed.is_user_pass_plaintext() {
                return Cred::userpass_plaintext(&t.username, &t.token);
            }
        }
        // 2. SSH agent for git@ URLs.
        if allowed.is_ssh_key() {
            if let Some(u) = username_from_url {
                return Cred::ssh_key_from_agent(u);
            }
        }
        // 3. The system git credential helper — lets us reuse whatever the
        //    user already configured (e.g. `gh auth login` for github.com and
        //    GitHub Enterprise hosts) without a separate token_env.
        if allowed.is_user_pass_plaintext() {
            if let Some(cfg) = &git_config {
                if let Ok(cred) = Cred::credential_helper(cfg, url, username_from_url) {
                    return Ok(cred);
                }
            }
        }
        Cred::default()
    });
    cb
}

/// Initialize and update every submodule, recursively. Each submodule is
/// authenticated against the central auth config keyed by its remote host.
fn update_submodules(repo: &Repository, auth: &AuthConfig) -> Result<()> {
    let submodules = repo.submodules()?;
    for mut sub in submodules {
        // A submodule with no configured URL (a broken or placeholder
        // `.gitmodules` entry) can't be fetched — skip it rather than
        // aborting the whole clone.
        let declared_url = match sub.url() {
            Ok(Some(u)) if !u.is_empty() => u.to_string(),
            _ => {
                tracing::warn!(
                    "submodule {:?} has no URL — skipping",
                    sub.name().unwrap_or("<unnamed>")
                );
                continue;
            }
        };
        sub.init(false)?;
        let key = format!("submodule.{}.url", sub.name().unwrap_or(""));
        let mut config = repo.config()?;
        // libgit2's filesystem-style relative resolution collapses the third
        // slash in file:/// URLs. Preserve the URL when resolving local siblings.
        if declared_url.starts_with("./") || declared_url.starts_with("../") {
            let remote = repo.find_remote("origin")?;
            let parent_url = remote.url()?;
            if parent_url.starts_with("file://") {
                let mut base = url::Url::parse(parent_url)
                    .map_err(|error| AIexError::Other(error.to_string()))?;
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                let resolved = base
                    .join(&declared_url)
                    .map_err(|error| AIexError::Other(error.to_string()))?;
                config.set_str(&key, resolved.as_str())?;
            }
        }
        let url = config.get_string(&key)?;
        let token = if url.starts_with("file://") {
            None
        } else {
            auth.resolve(&url, None)?
        };

        let mut fopts = FetchOptions::new();
        fopts.remote_callbacks(make_callbacks(token.clone()));
        // Initialize separately so update takes its fetch/checkout path. libgit2
        // 1.9's submodule clone failure cleanup uses the relative submodule path
        // against the process CWD and can delete unrelated files there.
        if sub.open().is_err() {
            sub.repo_init(true)?;
            sub.reload(true)?;
        }
        let mut update_opts = git2::SubmoduleUpdateOptions::new();
        update_opts.fetch(fopts);
        sub.update(true, Some(&mut update_opts))?;

        // Recurse into nested submodules.
        let nested_repo = sub.open()?;
        update_submodules(&nested_repo, auth)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_deadline_reuses_the_same_background_job() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote");
        let repo = Repository::init_opts(
            &remote,
            git2::RepositoryInitOptions::new().initial_head("main"),
        )
        .unwrap();
        let commit = |body: &str| {
            std::fs::write(remote.join("README.md"), body).unwrap();
            let mut index = repo.index().unwrap();
            index
                .add_all(["."], git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("test", "test@example.org").unwrap();
            let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "fixture",
                &tree,
                &parent.iter().collect::<Vec<_>>(),
            )
            .unwrap();
        };
        commit("old");
        let origin = GitRef {
            url: url::Url::from_directory_path(&remote).unwrap().into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
        let cached = cache.ensure(&origin).unwrap();
        commit("fresh");
        std::fs::remove_file(cached.join(FRESHNESS_FILE)).unwrap();
        let lock = dir_lock(&cached);
        let guard = lock.lock().unwrap();
        let policy = RefreshPolicy::Every(Duration::from_secs(3600));
        assert!(matches!(
            cache.ensure_fresh_until(&origin, policy, Instant::now() + Duration::from_millis(30)),
            Err(AIexError::RefreshPending)
        ));
        let first = refresh_registry()
            .lock()
            .unwrap()
            .get(&cached)
            .unwrap()
            .clone();
        assert!(matches!(
            cache.ensure_fresh_until(&origin, policy, Instant::now() + Duration::from_millis(30)),
            Err(AIexError::RefreshPending)
        ));
        let second = refresh_registry()
            .lock()
            .unwrap()
            .get(&cached)
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&first, &second));
        drop(guard);
        cache
            .ensure_fresh_until(&origin, policy, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cached.join("README.md")).unwrap(),
            "fresh"
        );
        // A caller can return long after a timed-out job completed. That old
        // completion must not suppress a refresh which has since become due.
        commit("newer");
        std::fs::remove_file(cached.join(FRESHNESS_FILE)).unwrap();
        refresh_registry().lock().unwrap().insert(
            cached.clone(),
            Arc::new(RefreshJob {
                started: Instant::now(),
                result: Mutex::new(Some(Ok(()))),
            }),
        );
        cache
            .ensure_fresh_until(&origin, policy, Instant::now() + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cached.join("README.md")).unwrap(),
            "newer"
        );
    }

    #[test]
    fn cold_clone_uses_the_search_deadline_and_keeps_the_job_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RepoCache::new(temp.path().join("cache"), AuthConfig::default());
        let origin = GitRef {
            url: "file:///unused".into(),
            ..Default::default()
        };
        let dir = cache.path_for(&origin);
        let job = Arc::new(CloneJob {
            result: Mutex::new(None),
        });
        clone_registry()
            .lock()
            .unwrap()
            .insert(dir.clone(), job.clone());
        let start = Instant::now();
        let result = cache.ensure_fresh_until(
            &origin,
            RefreshPolicy::Never,
            start + Duration::from_millis(30),
        );
        let remaining = clone_registry().lock().unwrap().remove(&dir).unwrap();
        assert!(matches!(result, Err(AIexError::RefreshPending)));
        assert!(start.elapsed() < Duration::from_millis(300));
        assert!(Arc::ptr_eq(&job, &remaining));
    }

    #[test]
    fn cache_identity_includes_ref_namespace_and_checkout_options() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = RepoCache::new(tmp.path().into(), AuthConfig::default());
        let branch = GitRef {
            url: "https://example.com/docs.git".into(),
            branch: Some("release".into()),
            ..Default::default()
        };
        let tag = GitRef {
            branch: None,
            tag: branch.branch.clone(),
            ..branch.clone()
        };
        let no_submodules = GitRef {
            submodules: false,
            ..branch.clone()
        };
        assert_ne!(cache.path_for(&branch), cache.path_for(&tag));
        assert_ne!(cache.path_for(&branch), cache.path_for(&no_submodules));
    }

    #[test]
    fn incomplete_job_wait_has_a_deadline_even_when_git_directory_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let reference = GitRef {
            url: "https://example.com/docs.git".into(),
            ..Default::default()
        };
        assert!(!checkout_ready(tmp.path(), &reference));
        let job = Arc::new(CloneJob {
            result: Mutex::new(None),
        });
        let start = std::time::Instant::now();
        assert!(matches!(
            wait_for_clone(tmp.path(), &reference, job, Duration::from_millis(20)),
            Err(AIexError::Warming { .. })
        ));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn submodule_missing_host_credentials_is_not_suppressed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        std::fs::write(
            tmp.path().join(".gitmodules"),
            "[submodule \"docs\"]\npath = docs\nurl = https://example.invalid/docs.git\n",
        )
        .unwrap();
        let mut auth = AuthConfig::default();
        auth.hosts.insert(
            "example.invalid".into(),
            crate::auth::HostAuth {
                source: crate::auth::TokenSource::Env {
                    token_env: "AIEX_MISSING_SUBMODULE_TOKEN_728593".into(),
                },
                username: None,
            },
        );
        assert!(matches!(
            update_submodules(&repo, &auth),
            Err(AIexError::MissingToken { .. })
        ));
    }

    #[test]
    fn never_is_never_due() {
        let now = SystemTime::now();
        assert!(!RefreshPolicy::Never.is_due(now, now));
        assert!(!RefreshPolicy::Never.is_due(SystemTime::UNIX_EPOCH, now));
    }

    #[test]
    fn every_respects_interval() {
        let policy = RefreshPolicy::Every(Duration::from_secs(60));
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(120);
        let fresh = now - Duration::from_secs(10);
        assert!(policy.is_due(stale, now));
        assert!(!policy.is_due(fresh, now));
    }
}
