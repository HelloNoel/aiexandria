use super::*;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::mpsc;

fn repo(dir: &Path, files: &[(&str, &str)]) -> GitRef {
    std::fs::create_dir_all(dir).unwrap();
    for (path, body) in files {
        std::fs::write(dir.join(path), body).unwrap();
    }
    let repo =
        git2::Repository::init_opts(dir, git2::RepositoryInitOptions::new().initial_head("main"))
            .unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["."], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("test", "test@example.org").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &[])
        .unwrap();
    GitRef {
        url: url::Url::from_directory_path(dir).unwrap().into(),
        branch: Some("main".into()),
        ..Default::default()
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    server: AIexandriaServer,
    origin: GitRef,
    remote: PathBuf,
    cached: PathBuf,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("repo");
        let origin = repo(&remote, &[("pinakes.yaml", body)]);
        let cache = RepoCache::new(
            temp.path().join("cache"),
            crate::auth::AuthConfig::default(),
        );
        let cached = cache.ensure(&origin).unwrap();
        let registry = RegistryConfig {
            indexes: BTreeMap::from([(
                "docs".into(),
                RegistryEntry {
                    repo: origin.clone(),
                    path: "pinakes.yaml".into(),
                    description: None,
                    refresh: Some(RefreshSetting::Every(Duration::from_secs(3600))),
                },
            )]),
        };
        Self {
            _temp: temp,
            server: AIexandriaServer::new(registry, cache),
            origin,
            remote,
            cached,
        }
    }

    async fn search(&self, request: Value) -> Result<CallToolResult, McpError> {
        self.server
            .search_docs(Parameters(serde_json::from_value(request).unwrap()))
            .await
    }

    async fn finish_refresh(&self, origin: &GitRef) {
        let cache = self.server.inner.cache.clone();
        let origin = origin.clone();
        tokio::task::spawn_blocking(move || {
            cache.ensure_fresh_until(
                &origin,
                RefreshPolicy::Every(Duration::from_secs(3600)),
                Instant::now() + Duration::from_secs(2),
            )
        })
        .await
        .unwrap()
        .unwrap();
    }
}

// Delay a real refresh at its checkout lock, without network timing dependencies.
// Auto-release also prevents a broken synchronous implementation from hanging the test suite.
struct Gate {
    release: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Gate {
    fn hold(dir: &Path) -> Self {
        std::fs::remove_file(dir.join(".pinakes-fetched")).unwrap();
        let lock = crate::cache::checkout_lock_for_test(dir);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _guard = lock.lock().unwrap();
            ready_tx.send(()).unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        ready_rx.recv().unwrap();
        Self {
            release: Some(release),
            worker: Some(worker),
        }
    }
}
impl Drop for Gate {
    fn drop(&mut self) {
        let _ = self.release.take().unwrap().send(());
        self.worker.take().unwrap().join().unwrap();
    }
}

const COMMAND: &str = "version: 1\nname: Test\ncommand_policy: {allow: '^git init -- marker$'}\nentries:\n - {id: search, name: Search, kind: command, argv: [git, init, --, marker]}\n";

fn assert_pending(error: &McpError) {
    assert_eq!(error.data.as_ref().unwrap()["status"], "refresh_pending");
    assert_eq!(error.data.as_ref().unwrap()["retry_after_ms"], 1000);
}

#[tokio::test]
async fn deadline_keeps_runtime_responsive_and_never_runs_a_late_command() {
    let fixture = Fixture::new(COMMAND);
    let gate = Gate::hold(&fixture.cached);
    let request = json!({"index":"docs", "command_id":"search", "query":"x", "refresh_wait_ms":60});
    let start = Instant::now();
    let (result, heartbeat) = tokio::join!(fixture.search(request.clone()), async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        start.elapsed()
    });
    assert_pending(&result.unwrap_err());
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(
        heartbeat < Duration::from_millis(100),
        "async runtime was blocked: {heartbeat:?}"
    );
    assert_pending(&fixture.search(request).await.unwrap_err());
    drop(gate);
    fixture.finish_refresh(&fixture.origin).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !fixture.cached.join("marker").exists(),
        "expired request executed a command"
    );
    fixture
        .search(json!({"index":"docs", "command_id":"search", "query":"x"}))
        .await
        .unwrap();
    assert!(fixture.cached.join("marker/.git").exists());
}

#[tokio::test]
async fn failed_refresh_does_not_silently_search_cached_metadata() {
    let fixture = Fixture::new(COMMAND);
    std::fs::remove_file(fixture.cached.join(".pinakes-fetched")).unwrap();
    std::fs::rename(&fixture.remote, fixture.remote.with_file_name("offline")).unwrap();
    let error = fixture
        .search(json!({"index":"docs", "query":"Search"}))
        .await
        .unwrap_err();
    assert_eq!(error.data.unwrap()["status"], "refresh_failed");
    assert!(!fixture.cached.join("marker").exists());
}

#[tokio::test]
async fn referenced_command_and_deep_doc_repositories_share_the_deadline() {
    let external = tempfile::tempdir().unwrap();
    let origin = repo(
        &external.path().join("external"),
        &[("README.md", "needle evidence")],
    );
    let body = format!("version: 1\nname: Test\ncommand_policy: {{allow: '^git init -- marker$'}}\nentries:\n - {{id: search, name: Search, kind: command, argv: [git, init, --, marker], repo: {{url: '{}', branch: main}}}}\n - {{id: doc, name: Doc, kind: doc, path: README.md, repo: {{url: '{}', branch: main}}}}\n", origin.url, origin.url);
    let fixture = Fixture::new(&body);
    let cached = fixture.server.inner.cache.ensure(&origin).unwrap();
    let gate = Gate::hold(&cached);
    let command = fixture
        .search(json!({"index":"docs", "command_id":"search", "query":"x", "refresh_wait_ms":60}))
        .await
        .unwrap_err();
    assert_pending(&command);
    // An unavailable content repository is not reported as a successful empty deep search.
    let deep = fixture
        .search(json!({"index":"docs", "deep":true, "query":"needle", "refresh_wait_ms":60}))
        .await
        .unwrap_err();
    assert_pending(&deep);
    fixture
        .search(json!({"index":"docs", "query":"Doc", "refresh_wait_ms":60}))
        .await
        .unwrap();
    drop(gate);
    fixture.finish_refresh(&origin).await;
    let found = fixture
        .search(json!({"index":"docs", "deep":true, "query":"needle"}))
        .await
        .unwrap();
    assert!(serde_json::to_string(&found)
        .unwrap()
        .contains("needle evidence"));
    assert!(!cached.join("marker").exists());
}

#[tokio::test]
async fn refresh_budget_is_not_restarted_for_a_second_repository() {
    let external = tempfile::tempdir().unwrap();
    let origin = repo(
        &external.path().join("external"),
        &[("README.md", "needle")],
    );
    let body = format!("version: 1\nname: Root\nentries:\n - {{id: doc, name: Doc, kind: doc, path: README.md, repo: {{url: '{}', branch: main}}}}\n", origin.url);
    let fixture = Fixture::new(&body);
    let cached = fixture.server.inner.cache.ensure(&origin).unwrap();
    let first = Gate::hold(&fixture.cached);
    let second = Gate::hold(&cached);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(180));
        drop(first);
    });
    let start = Instant::now();
    let error = fixture
        .search(json!({"index":"docs", "deep":true, "query":"needle", "refresh_wait_ms":250}))
        .await
        .unwrap_err();
    assert_pending(&error);
    assert!(
        start.elapsed() < Duration::from_millis(375),
        "deadline restarted: {:?}",
        start.elapsed()
    );
    drop(second);
    releaser.join().unwrap();
    fixture.finish_refresh(&origin).await;
}

#[tokio::test]
async fn refresh_wait_bounds_are_validated() {
    let fixture = Fixture::new(COMMAND);
    for wait in [0, 30_001] {
        let error = fixture
            .search(json!({"index":"docs", "query":"x", "refresh_wait_ms":wait}))
            .await
            .unwrap_err();
        assert!(error.message.contains("between 1 and 30000"));
    }
}

#[tokio::test]
async fn never_and_pinned_indexes_do_not_wait_for_an_unnecessary_refresh() {
    let fixture = Fixture::new(COMMAND);
    let commit = git2::Repository::open(&fixture.remote)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    let pinned = GitRef {
        branch: None,
        commit: Some(commit),
        ..fixture.origin.clone()
    };
    let pinned_cache = fixture.server.inner.cache.ensure(&pinned).unwrap();
    let entry = |origin: GitRef, refresh| RegistryEntry {
        repo: origin,
        path: "pinakes.yaml".into(),
        description: None,
        refresh: Some(refresh),
    };
    let registry = RegistryConfig {
        indexes: BTreeMap::from([
            (
                "never".into(),
                entry(fixture.origin.clone(), RefreshSetting::Never),
            ),
            (
                "pinned".into(),
                entry(pinned, RefreshSetting::Every(Duration::from_secs(3600))),
            ),
        ]),
    };
    let server = AIexandriaServer::new(registry, (*fixture.server.inner.cache).clone());
    std::fs::remove_file(fixture.cached.join(".pinakes-fetched")).unwrap();
    std::fs::remove_file(pinned_cache.join(".pinakes-fetched")).unwrap();
    std::fs::rename(&fixture.remote, fixture.remote.with_file_name("offline")).unwrap();
    for name in ["never", "pinned"] {
        let request = Parameters(
            serde_json::from_value(json!({"index":name, "query":"Search", "refresh_wait_ms":100}))
                .unwrap(),
        );
        server.search_docs(request).await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn command_execution_has_its_own_timeout_after_preparation() {
    let fixture = Fixture::new("version: 1\nname: Root\ncommand_policy: {allow: '^sh -c .+$', timeout_secs: 1}\nentries:\n - {id: slow, name: Slow, kind: command, argv: [sh, -c, 'sleep 0.15; printf finished']}\n");
    let start = Instant::now();
    let result = fixture
        .search(json!({"index":"docs", "command_id":"slow", "query":"x", "refresh_wait_ms":100}))
        .await
        .unwrap();
    assert!(start.elapsed() > Duration::from_millis(150));
    assert!(serde_json::to_string(&result).unwrap().contains("finished"));
}
