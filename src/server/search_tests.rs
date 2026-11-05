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
        Self::with_files(body, &[])
    }

    fn with_files(body: &str, files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("repo");
        let mut all = vec![("pinakes.yaml", body)];
        all.extend_from_slice(files);
        let origin = repo(&remote, &all);
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
        expire(dir);
        let lock = crate::cache::lock_file(&dir.parent().unwrap().join("job.lock")).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            lock.lock().unwrap();
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

fn assert_pending(result: &CallToolResult) {
    assert_eq!(
        result.structured_content.as_ref().unwrap()["status"],
        "preparing"
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["retry_after_ms"],
        2000
    );
}
fn expire(dir: &Path) {
    let path = dir.parent().unwrap().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    state["checked_ms"] = json!(0);
    std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
}

#[tokio::test]
async fn deadline_keeps_runtime_responsive_and_never_runs_a_late_command() {
    let fixture = Fixture::new(COMMAND);
    let gate = Gate::hold(&fixture.cached);
    let request = json!({"index":"docs", "command_id":"search", "query":"x", "freshness":"require_fresh", "refresh_wait_ms":60});
    let start = Instant::now();
    let (result, heartbeat) = tokio::join!(fixture.search(request.clone()), async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        start.elapsed()
    });
    assert_pending(&result.unwrap());
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(
        heartbeat < Duration::from_millis(100),
        "async runtime was blocked: {heartbeat:?}"
    );
    assert_pending(&fixture.search(request).await.unwrap());
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
    assert!(
        !fixture.cached.join("marker/.git").exists(),
        "command modified the published snapshot"
    );
}

#[tokio::test]
async fn failed_refresh_does_not_silently_search_cached_metadata() {
    let fixture = Fixture::new(COMMAND);
    expire(&fixture.cached);
    std::fs::rename(&fixture.remote, fixture.remote.with_file_name("offline")).unwrap();
    let error = fixture
        .search(json!({"index":"docs", "query":"Search", "freshness":"require_fresh"}))
        .await
        .unwrap();
    assert_eq!(error.is_error, Some(true));
    let message = &error.content[0].as_text().unwrap().text;
    assert!(!message.is_empty());
    let structured = error.structured_content.as_ref().unwrap();
    assert_eq!(structured["status"], "failed");
    assert_eq!(structured["message"], *message);
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
        .search(json!({"index":"docs", "command_id":"search", "query":"x", "freshness":"require_fresh", "refresh_wait_ms":60}))
        .await
        .unwrap();
    assert_pending(&command);
    // An unavailable content repository is not reported as a successful empty deep search.
    let deep = fixture
        .search(json!({"index":"docs", "deep":true, "query":"needle", "freshness":"require_fresh", "refresh_wait_ms":60}))
        .await
        .unwrap();
    assert_pending(&deep);
    fixture
        .search(json!({"index":"docs", "query":"Doc", "freshness":"require_fresh", "refresh_wait_ms":60}))
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
        .search(json!({"index":"docs", "deep":true, "query":"needle", "freshness":"require_fresh", "refresh_wait_ms":250}))
        .await
        .unwrap();
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
            .search(json!({"index":"docs", "query":"x", "freshness":"require_fresh", "refresh_wait_ms":wait}))
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
    expire(&fixture.cached);
    expire(&pinned_cache);
    std::fs::rename(&fixture.remote, fixture.remote.with_file_name("offline")).unwrap();
    for name in ["never", "pinned"] {
        let request = Parameters(
            serde_json::from_value(json!({"index":name, "query":"Search", "freshness":"require_fresh", "refresh_wait_ms":100}))
                .unwrap(),
        );
        server.search_docs(request).await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn command_execution_has_its_own_timeout_after_preparation() {
    let fixture = Fixture::new("version: 1\nname: Root\ncommand_policy: {allow: '^sh -c .+$', timeout_secs: 4}\nentries:\n - {id: slow, name: Slow, kind: command, argv: [sh, -c, 'sleep 1.1; printf finished']}\n");
    let start = Instant::now();
    let result = fixture
        .search(json!({"index":"docs", "command_id":"slow", "query":"x", "refresh_wait_ms":1000}))
        .await
        .unwrap();
    assert_eq!(
        result.structured_content.as_ref().unwrap()["status"],
        "ready"
    );
    assert!(start.elapsed() > Duration::from_millis(1100));
    assert!(serde_json::to_string(&result).unwrap().contains("finished"));
}

const RANKED: &str = "version: 1\nname: Tree documentation\nentries:\n - {id: guide, name: Trees, kind: doc, path: guide.md}\n - {id: custom, name: External search, kind: command, argv: [never-execute, '{query}']}\n";

#[tokio::test]
async fn ranked_discovery_search_and_snapshot_read_preserve_legacy_search() {
    let mut fixture = Fixture::with_files(
        RANKED,
        &[
            ("guide.md", "# Trees\nSquirrels gather acorns.\n"),
            (
                "extra.md",
                "# Databases\nTransactions preserve consistency.\n",
            ),
        ],
    );
    fixture.server = fixture
        .server
        .with_search(crate::search::config::SearchConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
    let service = fixture.server.inner.search.as_ref().unwrap();
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty());
    let discovery = fixture
        .server
        .discover_sources(Parameters(
            serde_json::from_value(json!({"query":"Tree", "limit":10})).unwrap(),
        ))
        .await
        .unwrap();
    let wire = serde_json::to_value(discovery).unwrap();
    let discovery: Value =
        serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
    let source = discovery["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source_id"].as_str().unwrap().starts_with("cached:"))
        .unwrap();
    assert_eq!(source["search_modes"], json!(["keyword"]));
    assert_eq!(source["search_status"]["keyword"], "ready");
    assert_eq!(source["search_status"]["coverage"]["indexed_files"], 2);
    let source_id = source["source_id"].as_str().unwrap();
    let result = fixture.search(json!({"source_id":source_id, "query":"transactions", "limit":1, "max_output_bytes":2048})).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let value = result.structured_content.unwrap();
    assert_eq!(value["mode_used"], "keyword");
    assert_eq!(value["hits"][0]["file"], "extra.md");
    assert!(serde_json::to_vec(&value).unwrap().len() <= 2048);
    let hit = &value["hits"][0];
    assert_eq!(hit["revision"].as_str().unwrap().len(), 40);
    let mut read = hit["read"].clone();
    for key in ["file", "offset", "limit"] {
        read[key] = hit[key].clone();
    }
    read["snapshot"] = value["snapshot"].clone();
    let read = fixture
        .server
        .read_file(Parameters(serde_json::from_value(read).unwrap()))
        .await
        .unwrap();
    assert!(serde_json::to_string(&read)
        .unwrap()
        .contains("Transactions preserve consistency"));
    let curated = fixture
        .search(json!({"index":"docs", "query":"transactions", "mode":"keyword"}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert!(
        curated["hits"].as_array().unwrap().is_empty(),
        "curated scope must not expand to the whole repository"
    );
    let legacy = fixture
        .search(json!({"index":"docs", "query":"Squirrels", "deep":true}))
        .await
        .unwrap();
    assert!(serde_json::to_string(&legacy)
        .unwrap()
        .contains("Squirrels"));
    assert!(legacy
        .structured_content
        .unwrap()
        .get("mode_used")
        .is_none());
    let fallback = fixture
        .search(json!({"index":"docs", "query":"Squirrels", "mode":"hybrid"}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(fallback["mode_used"], "keyword");
    assert_eq!(fallback["semantic"]["status"], "not_configured");
}

#[tokio::test]
async fn ranked_requests_validate_bounds_scopes_and_refresh_deadlines() {
    let mut fixture = Fixture::with_files(RANKED, &[("guide.md", "Squirrels gather acorns.\n")]);
    assert!(fixture
        .search(json!({"index":"docs", "query":"Squirrels", "mode":"keyword"}))
        .await
        .is_err());
    fixture.server = fixture
        .server
        .with_search(crate::search::config::SearchConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
    for invalid in [
        json!({"limit":0}),
        json!({"limit":21}),
        json!({"max_output_bytes":1023}),
        json!({"max_output_bytes":32769}),
        json!({"deep":true}),
        json!({"query":" "}),
        json!({"refresh_wait_ms":0}),
        json!({"command_id":"custom"}),
        json!({"source_id":"cached:missing"}),
    ] {
        let mut request = json!({"index":"docs", "query":"Squirrels", "mode":"keyword"});
        request
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        assert!(fixture.search(request.clone()).await.is_err(), "{request}");
    }
    let service = fixture.server.inner.search.as_ref().unwrap();
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let source = service.sources().pop().unwrap();
    let gate = Gate::hold(&fixture.cached);
    let started = Instant::now();
    let result = fixture.search(json!({"source_id":source.id, "query":"Squirrels", "mode":"keyword", "freshness":"require_fresh", "refresh_wait_ms":60})).await.unwrap();
    assert_pending(&result);
    let result = fixture.search(json!({"index":"docs", "query":"Squirrels", "mode":"keyword", "freshness":"require_fresh", "refresh_wait_ms":60})).await.unwrap();
    assert_pending(&result);
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(gate);
    fixture.finish_refresh(&fixture.origin).await;
}

#[tokio::test]
async fn global_mcp_search_filters_and_reads_snapshots_across_repositories() {
    let mut fixture = Fixture::with_files(
        RANKED,
        &[
            ("guide.md", "# Trees\nAcorns and consistency.\n"),
            ("extra.md", "# Extra\nAdditional consistency evidence.\n"),
        ],
    );
    let other = Fixture::with_files(
        RANKED,
        &[(
            "guide.md",
            "# Transactions\nTransactions preserve consistency.\n",
        )],
    );
    let cache = (*fixture.server.inner.cache).clone();
    cache.ensure(&other.origin).unwrap();
    let mut registry = (*fixture.server.inner.registry).clone();
    registry
        .indexes
        .insert("alias".into(), registry.indexes["docs"].clone());
    let mut second = registry.indexes["docs"].clone();
    second.repo = other.origin.clone();
    registry.indexes.insert("second".into(), second);
    fixture.server = AIexandriaServer::new(registry, cache)
        .with_search(crate::search::config::SearchConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
    let service = fixture.server.inner.search.as_ref().unwrap();
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let result = fixture
        .search(json!({"scope":"global", "query":"consistency", "limit":5}))
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let value = result.structured_content.unwrap();
    assert_eq!(value["scope"], "global");
    assert_eq!(value["mode_used"], "keyword");
    assert_eq!(value["hits"].as_array().unwrap().len(), 3);
    for hit in value["hits"].as_array().unwrap() {
        let mut read = hit["read"].clone();
        for key in ["file", "offset", "limit"] {
            read[key] = hit[key].clone();
        }
        read["snapshot"] = value["snapshot"].clone();
        let read = fixture
            .server
            .read_file(Parameters(serde_json::from_value(read).unwrap()))
            .await
            .unwrap();
        assert!(serde_json::to_string(&read)
            .unwrap()
            .contains("consistency"));
    }
    let result = fixture.search(json!({"scope":"global", "indexes":["second"], "query":"consistency", "max_output_bytes":1024})).await.unwrap();
    let body = serde_json::to_value(&result).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.len() <= 1024);
    let value = result.structured_content.unwrap();
    assert!(value["hits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["read"]["index"] == "second"));
    let source = service
        .sources()
        .into_iter()
        .find(|s| s.route.index == "docs")
        .unwrap();
    let filtered = fixture
        .search(json!({"scope":"global", "source_ids":[source.id], "query":"consistency"}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(filtered["hits"].as_array().unwrap().len(), 2);
    assert!(filtered["hits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|h| h["read"]["index"] == "docs"));
    let discovery: Value = serde_json::from_str(
        &fixture
            .server
            .inner
            .discovery
            .discover(serde_json::from_value(json!({"query":"Tree", "limit":20})).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    let browse = discovery["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "browse" && s["search"]["index"] == "docs")
        .unwrap();
    let filtered = fixture
        .search(
            json!({"scope":"global", "source_ids":[browse["source_id"]], "query":"consistency"}),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(
        filtered["hits"].as_array().unwrap().len(),
        1,
        "browse filter must retain curated scope"
    );
    for invalid in [
        json!({"index":"docs"}),
        json!({"source_id":source.id}),
        json!({"path":["x"]}),
        json!({"deep":true}),
        json!({"freshness":"require_fresh"}),
        json!({"indexes":["unknown"]}),
        json!({"source_ids":["unknown"]}),
        json!({"limit":0}),
    ] {
        let mut request = json!({"scope":"global","query":"consistency"});
        request
            .as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        assert!(fixture.search(request).await.is_err());
    }
    assert!(fixture
        .search(json!({"indexes":["docs"],"query":"consistency"}))
        .await
        .is_err());
}
