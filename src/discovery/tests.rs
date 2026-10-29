use super::*;
use crate::auth::AuthConfig;
use crate::cache::RefreshPolicy;
use crate::command::CommandPolicyChain;
use crate::error::AIexError;
use crate::index::GitRef;
use crate::registry::{RefreshSetting, RegistryEntry};
use crate::resolver::{Resolver, TraversalBudget};
use serde_json::{json, Value};
use std::path::Path;

struct Fixture {
    _temp: tempfile::TempDir,
    repo: git2::Repository,
    registry: Arc<RegistryConfig>,
    cache: Arc<RepoCache>,
    service: DiscoveryService,
}

fn commit(repo: &git2::Repository) {
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
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

impl Fixture {
    fn new(files: &[(&str, &str)], failed_root: bool, refresh: RefreshSetting) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let repo = git2::Repository::init_opts(
            &dir,
            git2::RepositoryInitOptions::new().initial_head("main"),
        )
        .unwrap();
        for (path, body) in files {
            write(&dir, path, body);
        }
        commit(&repo);
        let entry = RegistryEntry {
            repo: GitRef {
                url: url::Url::from_directory_path(&dir).unwrap().into(),
                branch: Some("main".into()),
                ..Default::default()
            },
            path: "pinakes.yaml".into(),
            description: Some("registered catalog".into()),
            refresh: Some(refresh),
        };
        let mut indexes = BTreeMap::from([("docs".into(), entry.clone())]);
        if failed_root {
            indexes.insert(
                "broken".into(),
                RegistryEntry {
                    path: "missing.yaml".into(),
                    ..entry
                },
            );
        }
        let registry = Arc::new(RegistryConfig { indexes });
        let cache = Arc::new(RepoCache::new(
            temp.path().join("cache"),
            AuthConfig::default(),
        ));
        let service = DiscoveryService::new(registry.clone(), cache.clone());
        Self {
            _temp: temp,
            repo,
            registry,
            cache,
            service,
        }
    }

    async fn query(&self, request: Value) -> Value {
        let body = self
            .service
            .discover(serde_json::from_value(request).unwrap())
            .await
            .unwrap();
        serde_json::from_str(&body).unwrap()
    }
}

const TREE: &str = r#"
version: 1
name: Library
description: Documentation library
command_policy: {allow: '^nothing-is-allowed$'}
entries:
  - {id: child, name: Kubernetes, kind: index, path: child.yaml}
  - {kind: merge, path: merge.yaml, prefix: shared}
  - id: external-doc
    name: ZephyrManual
    description: Deployment reference
    kind: doc
    path: unavailable.md
    repo: {url: 'file:///must-not-clone-document-repo'}
  - {id: body, name: General, kind: doc, path: body.md}
"#;
const CHILD: &str = r#"
version: 1
name: Kubernetes
description: Cluster operations
entries:
  - id: kubectl
    name: KubernetesSearch
    description: Exact phrases only; AND is literal text.
    kind: command
    argv: [must-not-run, '{query}']
    repo: {url: 'file:///must-not-clone-command-repo'}
"#;
const MERGE: &str = r#"
version: 1
name: Shared
entries:
  - {id: unix, name: Linux, kind: index, path: linux.yaml}
"#;
const LINUX: &str = "version: 1\nname: Linux\nentries: []\n";

#[tokio::test]
async fn nested_and_merged_routes_use_only_metadata() {
    let fixture = Fixture::new(
        &[
            ("pinakes.yaml", TREE),
            ("child.yaml", CHILD),
            ("merge.yaml", MERGE),
            ("linux.yaml", LINUX),
            ("body.md", "zzbodysecretzz"),
        ],
        false,
        RefreshSetting::Never,
    );
    let response = fixture.query(json!({"query":"KubernetesSearch"})).await;
    assert_eq!(response["catalog"]["complete"], true, "{response}");
    assert_eq!(response["catalog"]["stale_indexes"], 0);
    let source = &response["sources"][0];
    assert_eq!(source["kind"], "command");
    assert_eq!(source["query_profile"], "source_defined");
    assert_eq!(
        source["search"],
        json!({"index":"docs", "path":["child"], "command_id":"kubectl"})
    );
    assert!(fixture
        .service
        .route(source["source_id"].as_str().unwrap())
        .is_some());
    let linux = fixture.query(json!({"query":"Linux"})).await;
    assert_eq!(linux["sources"][0]["search"]["path"], json!(["sharedunix"]));
    let hints = fixture.query(json!({"query":"ZephyrManual"})).await;
    assert_eq!(hints["sources"][0]["kind"], "browse");
    let body = fixture.query(json!({"query":"zzbodysecretzz"})).await;
    assert_eq!(body["sources"], json!([]));
    let commands = fixture
        .query(json!({"query":"Kubernetes", "kinds":["command"], "indexes":["docs"]}))
        .await;
    assert_eq!(commands["sources"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn cycles_and_failed_roots_preserve_healthy_results() {
    let root = "version: 1\nname: Healthy\nentries:\n  - {id: loop, name: Loop, kind: index, path: pinakes.yaml}\n";
    let fixture = Fixture::new(&[("pinakes.yaml", root)], true, RefreshSetting::Never);
    let response = fixture.query(json!({"query":"Healthy"})).await;
    assert_eq!(response["sources"][0]["name"], "Healthy");
    assert_eq!(response["catalog"]["complete"], false);
    assert_eq!(response["catalog"]["failed_nodes"], 2);
    assert_eq!(response["catalog"]["stale_indexes"], 0);
    let scoped = fixture
        .query(json!({"query":"Healthy", "indexes":["docs"]}))
        .await;
    assert_eq!(scoped["catalog"]["failed_nodes"], 1);
}

#[tokio::test]
async fn refresh_removes_sources_but_cursor_keeps_its_snapshot() {
    let root = "version: 1\nname: Topic\nentries:\n  - {id: child, name: Topic child, kind: index, path: child.yaml}\n";
    let fixture = Fixture::new(
        &[
            ("pinakes.yaml", root),
            ("child.yaml", "version: 1\nname: Topic child\nentries: []"),
        ],
        false,
        RefreshSetting::Every(Duration::from_millis(100)),
    );
    let first = fixture.query(json!({"query":"Topic", "limit":1})).await;
    assert_eq!(first["catalog"]["complete"], true);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let root_id = first["sources"][0]["source_id"].clone();
    let dir = fixture.repo.workdir().unwrap();
    write(
        dir,
        "pinakes.yaml",
        "version: 1\nname: Topic\nentries: []\n",
    );
    commit(&fixture.repo);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let updated = fixture.query(json!({"query":"Topic"})).await;
    assert_eq!(updated["catalog"]["complete"], true, "{updated}");
    assert_eq!(updated["sources"].as_array().unwrap().len(), 1);
    assert_eq!(updated["sources"][0]["source_id"], root_id);
    let second = fixture.query(json!({"cursor":cursor})).await;
    assert_eq!(second["generation"], first["generation"]);
    assert_eq!(second["sources"][0]["name"], "Topic child");
    assert!(second["next_cursor"].is_null());
    assert!(fixture
        .service
        .route(second["sources"][0]["source_id"].as_str().unwrap())
        .is_none());
}

#[tokio::test]
async fn never_refresh_and_failed_refresh_status() {
    let fixture = Fixture::new(&[("pinakes.yaml", LINUX)], false, RefreshSetting::Never);
    let first = fixture.query(json!({"query":"Linux"})).await;
    write(fixture.repo.workdir().unwrap(), "pinakes.yaml", "invalid");
    commit(&fixture.repo);
    let again = fixture.query(json!({"query":"Linux"})).await;
    assert_eq!(first["generation"], again["generation"]);
    assert_eq!(again["catalog"]["complete"], true);

    let fixture = Fixture::new(
        &[("pinakes.yaml", LINUX)],
        false,
        RefreshSetting::Every(Duration::from_millis(100)),
    );
    fixture.query(json!({"query":"Linux"})).await;
    write(fixture.repo.workdir().unwrap(), "pinakes.yaml", "invalid");
    commit(&fixture.repo);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let failed = fixture.query(json!({"query":"Linux"})).await;
    assert_eq!(failed["sources"][0]["name"], "Linux");
    assert_eq!(failed["catalog"]["complete"], false);
    assert_eq!(failed["catalog"]["stale_indexes"], 1);
    assert_eq!(failed["catalog"]["failed_nodes"], 1);
}

#[tokio::test]
async fn failed_fetch_reports_stale_cached_metadata() {
    let fixture = Fixture::new(
        &[("pinakes.yaml", LINUX)],
        false,
        RefreshSetting::Every(Duration::from_millis(100)),
    );
    fixture.query(json!({"query":"Linux"})).await;
    let dir = fixture.repo.workdir().unwrap();
    std::fs::rename(dir, dir.with_file_name("offline")).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let response = fixture.query(json!({"query":"Linux"})).await;
    assert_eq!(response["sources"][0]["name"], "Linux");
    assert_eq!(response["catalog"]["complete"], false);
    assert_eq!(response["catalog"]["stale_indexes"], 1);
    assert_eq!(response["retry_after_ms"], 30_000);
}

#[tokio::test]
async fn retained_matches_are_bounded_and_reported() {
    let mut body = "version: 1\nname: Topic\nentries:\n".to_string();
    for i in 0..1050 {
        body.push_str(&format!(
            "  - {{id: c{i}, name: Topic {i}, kind: command, argv: [unused]}}\n"
        ));
    }
    let fixture = Fixture::new(&[("pinakes.yaml", &body)], false, RefreshSetting::Never);
    let response = fixture.query(json!({"query":"Topic", "limit":20})).await;
    assert_eq!(response["catalog"]["complete"], true);
    assert_eq!(response["matches_limited"], true);
    assert!(response["next_cursor"].is_string());
    assert!(response["sources"].as_array().unwrap().len() <= 20);
    let snapshots = fixture.service.snapshots.lock().unwrap();
    assert_eq!(snapshots.back().unwrap().sources.len(), 1000);
}

#[tokio::test]
async fn invalid_requests_and_expired_cursors_are_actionable() {
    let fixture = Fixture::new(&[("pinakes.yaml", LINUX)], false, RefreshSetting::Never);
    for request in [
        json!({}),
        json!({"query":""}),
        json!({"query":"---"}),
        json!({"query":"x", "limit":0}),
        json!({"query":"x", "limit":21}),
        json!({"query":"x", "max_output_bytes":511}),
        json!({"query":"x", "max_output_bytes":32769}),
        json!({"query":"x", "indexes":[]}),
        json!({"query":"x", "indexes":["missing"]}),
        json!({"query":"x", "kinds":[]}),
        json!({"cursor":"bad"}),
        json!({"cursor":"missing:0"}),
        json!({"cursor":"x:0", "query":"x"}),
    ] {
        assert!(
            fixture
                .service
                .discover(serde_json::from_value(request.clone()).unwrap())
                .await
                .is_err(),
            "{request}"
        );
    }
    fixture.query(json!({"query":"Linux"})).await;
    let cursor = {
        let mut snapshots = fixture.service.snapshots.lock().unwrap();
        let snapshot = Arc::get_mut(snapshots.back_mut().unwrap()).unwrap();
        snapshot.created = Instant::now() - CURSOR_TTL;
        format!("{}:0", snapshot.id)
    };
    let error = fixture
        .service
        .discover(serde_json::from_value(json!({"cursor":cursor})).unwrap())
        .await
        .unwrap_err();
    assert!(error.contains("expired"));
}

fn source(name: &str, description: &str, context: &str, docs: &str) -> Arc<Source> {
    Arc::new(Source::new(
        SourceKind::Command,
        SourceRoute {
            index: "docs".into(),
            path: vec![],
            command_id: Some(name.into()),
        },
        name,
        description,
        context,
        docs,
    ))
}

#[test]
fn ranking_handles_exact_names_words_unicode_and_context() {
    let sources = vec![
        source("AI_Core", "authentication tokens", "SAP", ""),
        source("Other", "AI Core deployment", "", ""),
        source("Überblick", "日本語 ガイド", "", ""),
        source("Library", "", "", "UniqueDocumentHint"),
    ];
    assert_eq!(BuiltinRanker.rank(&sources, "AI Core")[0].name, "AI_Core");
    assert_eq!(
        BuiltinRanker.rank(&sources, "how to find SAP authentication")[0].name,
        "AI_Core"
    );
    assert_eq!(
        BuiltinRanker.rank(&sources, "ÜBERBLICK")[0].name,
        "Überblick"
    );
    assert_eq!(BuiltinRanker.rank(&sources, "日本語")[0].name, "Überblick");
    assert_eq!(
        BuiltinRanker.rank(&sources, "Unique Document Hint")[0].name,
        "Library"
    );
    assert!(BuiltinRanker.rank(&sources, "unmatched").is_empty());
    assert_eq!(
        tokenize("HTTPServer AI_Core"),
        vec!["http", "server", "ai", "core"]
    );
}

#[test]
fn byte_bounded_pages_preserve_unicode_and_every_locator() {
    let sources: Vec<_> = (0..12)
        .map(|i| source(&format!("資料{i}"), &"説明🦀".repeat(200), "", ""))
        .collect();
    let snapshot = QuerySnapshot {
        id: "snapshot".into(),
        created: Instant::now(),
        sources,
        catalog: CatalogStatus {
            complete: true,
            ..Default::default()
        },
        generation: 1,
        limited: false,
        bytes: 0,
    };
    let mut offset = 0;
    let mut ids = BTreeSet::new();
    loop {
        let body = render_page(&snapshot, offset, 20, 2048).unwrap();
        assert!(body.len() <= 2048);
        let page: Value = serde_json::from_str(&body).unwrap();
        for source in page["sources"].as_array().unwrap() {
            assert!(ids.insert(source["source_id"].as_str().unwrap().to_owned()));
            assert_eq!(source["guidance_complete"], false);
        }
        let Some(cursor) = page["next_cursor"].as_str() else {
            break;
        };
        offset = cursor.rsplit_once(':').unwrap().1.parse().unwrap();
    }
    assert_eq!(ids.len(), 12);
    let small = render_page(&snapshot, 0, 1, 512).unwrap();
    assert!(small.len() <= 512);
    let huge_locator = QuerySnapshot {
        sources: vec![source(&"x".repeat(4096), "", "", "")],
        ..snapshot
    };
    assert!(render_page(&huge_locator, 0, 1, 512)
        .unwrap_err()
        .contains("max_output_bytes"));
}

#[tokio::test]
async fn traversal_budget_also_limits_merge_chains() {
    let fixture = Fixture::new(
        &[
            (
                "pinakes.yaml",
                "version: 1\nname: Root\nentries:\n - {kind: merge, path: merge.yaml}",
            ),
            ("merge.yaml", "version: 1\nname: Merged\nentries: []"),
        ],
        false,
        RefreshSetting::Never,
    );
    fixture.query(json!({"query":"Root"})).await; // Warm the repository.
    let budget = TraversalBudget::new(32, 1);
    let resolver = Resolver::with_budget(&fixture.cache, &budget);
    let result = resolver.load_index(
        &fixture.registry.indexes["docs"].repo,
        "pinakes.yaml",
        &CommandPolicyChain::default(),
    );
    assert!(matches!(
        result,
        Err(AIexError::TraversalLimit("index loads"))
    ));
    let budget = TraversalBudget::new(1, 100);
    let result = Resolver::with_budget(&fixture.cache, &budget).load_index_with_refresh(
        &fixture.registry.indexes["docs"].repo,
        "pinakes.yaml",
        &CommandPolicyChain::default(),
        RefreshPolicy::Never,
    );
    assert!(matches!(result, Err(AIexError::TraversalLimit("depth"))));
}

#[test]
fn rank_ten_thousand_sources() {
    let sources: Vec<_> = (0..10_000)
        .map(|i| {
            source(
                &format!("Project{i}"),
                "API deployment operations reference",
                "",
                "",
            )
        })
        .collect();
    let start = Instant::now();
    let ranked = BuiltinRanker.rank(&sources, "Project1234");
    assert_eq!(ranked[0].name, "Project1234");
    eprintln!("10,000-source warm ranking: {:?}", start.elapsed());
}
