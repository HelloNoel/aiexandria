//! Shallow-clone repos to a local cache and re-use them across queries.
//!
//! Cache layout: `<cache_root>/<sha256(url)>/<ref>/`. Each (url, ref) pair gets its own
//! checkout so different branches/tags don't fight over a single working tree.
//!
//! Refresh policy: when a repo is already cloned, we re-fetch and fast-forward
//! the working tree if the last fetch was longer than `RefreshPolicy::interval`
//! ago. The freshness sentinel is the mtime of `.pinakes-fetched` in the
//! checkout root (not `.git/FETCH_HEAD`, which git rewrites for unrelated reasons).
//! Pinned refs (`commit:` set) are immutable and never refreshed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use git2::{build::RepoBuilder, Cred, FetchOptions, RemoteCallbacks, Repository};
use sha2::{Digest, Sha256};

use crate::auth::{AuthConfig, ResolvedToken};
use crate::error::{AIexError, Result};
use crate::index::GitRef;

const FRESHNESS_FILE: &str = ".pinakes-fetched";

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

/// The actual clone + checkout + submodule init. Blocking; run on a worker
/// thread via [`clone_job`].
fn do_clone(dir: &Path, git_ref: &GitRef, auth: &AuthConfig) -> Result<()> {
    // The directory may exist from a prior failed clone (e.g. interrupted, or
    // a stale `.git/config.lock` left behind). Wipe and start clean.
    if dir.exists() {
        tracing::warn!(
            "incomplete cache dir at {} — removing and re-cloning",
            dir.display()
        );
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;

    let token = auth.resolve(&git_ref.url, git_ref.token_env.as_deref())?;

    let mut builder = RepoBuilder::new();
    let mut fopts = FetchOptions::new();
    // git2's local transport doesn't support shallow fetch; only ask for
    // depth=1 over real network transports.
    if !git_ref.url.starts_with("file://") {
        fopts.depth(1);
    }
    fopts.remote_callbacks(make_callbacks(token.clone()));
    builder.fetch_options(fopts);

    if let Some(branch) = &git_ref.branch {
        builder.branch(branch);
    } else if let Some(tag) = &git_ref.tag {
        // `branch()` with a tag name works because git2 resolves the ref symbolically.
        builder.branch(tag);
    }

    let repo = builder.clone(&git_ref.url, dir)?;

    if let Some(commit) = &git_ref.commit {
        let oid = git2::Oid::from_str(commit)?;
        let obj = repo.find_object(oid, None)?;
        repo.checkout_tree(&obj, None)?;
        repo.set_head_detached(oid)?;
    }
    if git_ref.submodules {
        update_submodules(&repo, auth)?;
    }
    let _ = std::fs::write(dir.join(FRESHNESS_FILE), b"");
    Ok(())
}

/// How often to re-fetch a cached repo. `Never` disables auto-refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPolicy {
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

impl Default for RefreshPolicy {
    fn default() -> Self {
        RefreshPolicy::Never
    }
}

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
        let dir = self.path_for(git_ref);

        if dir.join(".git").exists() {
            // A finished (or stale) background clone entry can be dropped now.
            clone_registry().lock().expect("clone registry poisoned").remove(&dir);
            let lock = dir_lock(&dir);
            let _guard = lock.lock().expect("dir lock poisoned");
            // Pinned commits are immutable — never refresh.
            if git_ref.commit.is_none() && self.refresh_due(&dir, policy) {
                if let Err(e) = self.refresh(&dir, git_ref) {
                    tracing::warn!(
                        "refresh of {} ({:?}) failed, keeping cached copy: {e}",
                        git_ref.url,
                        git_ref.branch.as_deref().or(git_ref.tag.as_deref())
                    );
                } else {
                    let _ = std::fs::write(dir.join(FRESHNESS_FILE), b"");
                }
            }
            return Ok(dir);
        }

        // Cold clone. Kick it off in the background (deduped per dir) and wait
        // up to INLINE_CLONE_BUDGET for it to finish.
        let job = clone_job(&dir, git_ref, &self.auth);
        let start = SystemTime::now();
        loop {
            {
                let slot = job.result.lock().expect("clone job poisoned");
                if let Some(res) = slot.as_ref() {
                    return match res {
                        Ok(()) => Ok(dir),
                        Err(msg) => {
                            // Drop the failed job so a later call retries cleanly.
                            clone_registry()
                                .lock()
                                .expect("clone registry poisoned")
                                .remove(&dir);
                            Err(AIexError::Other(msg.clone()))
                        }
                    };
                }
            }
            if SystemTime::now()
                .duration_since(start)
                .unwrap_or_default()
                > INLINE_CLONE_BUDGET
            {
                return Err(AIexError::Warming {
                    url: git_ref.url.clone(),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn refresh_due(&self, dir: &Path, policy: RefreshPolicy) -> bool {
        let last = std::fs::metadata(dir.join(FRESHNESS_FILE))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        policy.is_due(last, SystemTime::now())
    }

    fn refresh(&self, dir: &Path, git_ref: &GitRef) -> Result<()> {
        let token = self.auth.resolve(&git_ref.url, git_ref.token_env.as_deref())?;
        let repo = Repository::open(dir)?;
        let mut remote = repo.find_remote("origin")?;

        let mut fopts = FetchOptions::new();
        if !git_ref.url.starts_with("file://") {
            fopts.depth(1);
        }
        fopts.remote_callbacks(make_callbacks(token));

        let refspec = git_ref
            .branch
            .clone()
            .or_else(|| git_ref.tag.clone())
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

    fn path_for(&self, git_ref: &GitRef) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(git_ref.url.as_bytes());
        let url_hash = hex::encode(&hasher.finalize()[..8]);
        let suffix = git_ref
            .commit
            .as_deref()
            .or(git_ref.tag.as_deref())
            .or(git_ref.branch.as_deref())
            .unwrap_or("HEAD");
        let safe_suffix: String = suffix
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.root.join(url_hash).join(safe_suffix)
    }

    /// Read a file from a checkout, validating the path stays inside the repo.
    pub fn read_file(&self, repo_root: &Path, rel_path: &str) -> Result<String> {
        let candidate = repo_root.join(rel_path);
        let canon_root = repo_root.canonicalize()?;
        let canon_candidate = candidate.canonicalize().map_err(|_| AIexError::Other(
            format!("file not found in checkout: {rel_path}"),
        ))?;
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
    let submodules = match repo.submodules() {
        Ok(s) => s,
        Err(_) => return Ok(()), // no .gitmodules => nothing to do
    };
    for mut sub in submodules {
        // A submodule with no configured URL (a broken or placeholder
        // `.gitmodules` entry) can't be fetched — skip it rather than
        // aborting the whole clone.
        let url = match sub.url() {
            Ok(Some(u)) if !u.is_empty() => u.to_string(),
            _ => {
                tracing::warn!(
                    "submodule {:?} has no URL — skipping",
                    sub.name().unwrap_or("<unnamed>")
                );
                continue;
            }
        };
        let token = if url.starts_with("file://") {
            None
        } else {
            auth.resolve(&url, None).unwrap_or(None)
        };

        let mut fopts = FetchOptions::new();
        fopts.remote_callbacks(make_callbacks(token.clone()));
        let mut update_opts = git2::SubmoduleUpdateOptions::new();
        update_opts.fetch(fopts);
        sub.update(true, Some(&mut update_opts))?;

        // Recurse into nested submodules.
        if let Ok(nested_repo) = sub.open() {
            update_submodules(&nested_repo, auth)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
