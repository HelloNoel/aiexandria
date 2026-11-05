use super::*;
use aiexandria::registry::RegistryConfig;
use aiexandria::update::UpdateOptions;
use serde_json::Value;

fn register(f: &Fixture, other: Option<&Fixture>) -> PathBuf {
    let path = f.temp.path().join("registry.yaml");
    let mut yaml = format!(
        "indexes:\n  docs:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n",
        f.origin.url
    );
    if let Some(other) = other {
        yaml.push_str(&format!(
            "  other:\n    repo: {{url: '{}', branch: main, submodules: false}}\n    path: pinakes.yaml\n    refresh: never\n",
            other.origin.url
        ));
    }
    std::fs::write(&path, yaml).unwrap();
    path
}

fn update(f: &Fixture, registry: &Path, args: &[&str]) -> std::process::Output {
    let search = f.temp.path().join("search.yaml");
    std::fs::write(&search, "enabled: true\n").unwrap();
    Command::new(env!("CARGO_BIN_EXE_aiexandria"))
        .arg("--registry")
        .arg(registry)
        .arg("--cache")
        .arg(f.cache.root())
        .arg("--search-config")
        .arg(search)
        .args(["update", "--json", "--timeout", "30s"])
        .args(args)
        .output()
        .unwrap()
}

fn completed(output: std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    result
}

#[test]
fn all_prepares_uncached_sources_with_selective_docs_full_commands_and_scope_filters() {
    let root = Fixture::new("sha1", true);
    let doc = Fixture::new("sha1", true);
    let command = Fixture::new("sha1", true);
    let outside = Fixture::new("sha1", true);
    let other = Fixture::new("sha1", true);
    std::fs::write(
        root.repo.join("pinakes.yaml"),
        format!(
            "version: 1\nname: Root\nentries:\n  - {{id: section, name: Section, kind: index, path: section.yaml}}\n  - id: outside\n    name: Outside\n    kind: doc\n    path: doc.md\n    repo: {{url: '{}', branch: main, submodules: false}}\n",
            outside.origin.url
        ),
    ).unwrap();
    std::fs::write(
        root.repo.join("section.yaml"),
        "version: 1\nname: Section\nentries:\n  - {id: nested, name: Nested, kind: index, path: nested.yaml}\n",
    ).unwrap();
    std::fs::write(
        root.repo.join("nested.yaml"),
        format!(
            "version: 1\nname: Nested\ncommand_policy:\n  allow: '^not-an-installed-command$'\nentries:\n  - id: doc\n    name: Doc\n    kind: doc\n    path: doc.md\n    repo: {{url: '{}', branch: main, submodules: false}}\n  - id: command\n    name: Command\n    kind: command\n    argv: [not-an-installed-command]\n    repo: {{url: '{}', branch: main, submodules: false}}\n",
            doc.origin.url, command.origin.url
        ),
    ).unwrap();
    git(&root.repo, &["add", "."]);
    git(&root.repo, &["commit", "-m", "nested sources"]);
    let registry = register(&root, Some(&other));

    let ordinary = completed(update(&root, &registry, &["--index", "docs"]));
    assert_eq!(ordinary["skipped"], 3);
    for fixture in [&doc, &command, &outside, &other] {
        assert!(root.cache.state(&fixture.origin).is_none());
    }

    let scoped = completed(update(
        &root,
        &registry,
        &[
            "--all",
            "--index",
            "docs",
            "--path",
            "section",
            "--semantic",
            "--wait",
        ],
    ));
    assert_eq!(scoped["search"]["partial_sources"], 0);
    assert_eq!(scoped["search"]["pending_sources"], 0);
    let selected = root
        .cache
        .cached_snapshot(&doc.origin, RefreshPolicy::Never)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(selected.join("doc.md")).unwrap(),
        "old evidence\n"
    );
    assert!(!selected.join("extra.md").exists());
    assert!(!selected.join("blob.bin").exists());
    let full = root
        .cache
        .cached_snapshot(&command.origin, RefreshPolicy::Never)
        .unwrap();
    assert!(full.join("extra.md").exists());
    assert!(full.join("blob.bin").exists());
    // The nonexistent command would fail if preparation executed it.
    assert!(root.cache.state(&outside.origin).is_none());
    assert!(root.cache.state(&other.origin).is_none());

    completed(update(&root, &registry, &["--all", "--index", "docs"]));
    assert!(root.cache.state(&outside.origin).is_some());
    assert!(root.cache.state(&other.origin).is_none());
    completed(update(&root, &registry, &["--all"]));
    assert!(root.cache.state(&other.origin).is_some());

    // --all changes acquisition scope, not refresh policy.
    doc.change();
    completed(update(&root, &registry, &["--all"]));
    let still_old = root
        .cache
        .cached_snapshot(&doc.origin, RefreshPolicy::Never)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(still_old.join("doc.md")).unwrap(),
        "old evidence\n"
    );
    completed(update(&root, &registry, &["--all", "--force"]));
    let fresh = root
        .cache
        .cached_snapshot(&doc.origin, RefreshPolicy::Never)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fresh.join("doc.md")).unwrap(),
        "new evidence\n"
    );
}

#[test]
fn all_preview_is_offline_and_conflicts_with_cached_only() {
    let root = Fixture::new("sha1", true);
    let registry = register(&root, None);
    std::fs::rename(&root.repo, root.temp.path().join("offline-remote")).unwrap();
    let preview = update(&root, &registry, &["--all", "--dry-run"]);
    assert!(preview.status.success());
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["status"], "planned");
    assert_eq!(preview["active_scope"]["all"], true);
    assert_eq!(preview["active_scope"]["network"], false);
    assert_eq!(preview["active_scope"]["cached_repositories"], 0);
    assert!(root.cache.state(&root.origin).is_none());
    assert!(!root.cache.root().join("update-scope.json").exists());
    let invalid = update(&root, &registry, &["--all", "--semantic", "--cached-only"]);
    assert_eq!(invalid.status.code(), Some(2));
    let options = UpdateOptions {
        all: true,
        cached_only: true,
        semantic: Some(aiexandria::search::config::SearchConfig::default()),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    assert!(options
        .validate(&RegistryConfig::load_from(&registry).unwrap())
        .is_err());
}

#[test]
fn failed_submodule_acquisition_does_not_skip_indexing_healthy_sources() {
    let root = Fixture::new("sha1", true);
    let healthy = Fixture::new("sha1", true);
    let broken = Fixture::new("sha1", true);
    let commit = git(&broken.repo, &["rev-parse", "HEAD"]);
    git(
        &broken.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},docs/missing-module"),
        ],
    );
    git(
        &broken.repo,
        &["commit", "-m", "gitlink without module URL"],
    );
    std::fs::write(
        root.repo.join("pinakes.yaml"),
        format!(
            "version: 1\nname: Mixed sources\nentries:\n  - id: broken-doc\n    name: Broken\n    kind: doc\n    path: doc.md\n    repo: {{url: '{}', branch: main, submodules: true}}\n  - id: healthy-doc\n    name: Healthy\n    kind: doc\n    path: doc.md\n    repo: {{url: '{}', branch: main, submodules: false}}\n",
            broken.origin.url, healthy.origin.url
        ),
    ).unwrap();
    git(&root.repo, &["add", "."]);
    git(&root.repo, &["commit", "-m", "mixed sources"]);
    let registry = register(&root, None);
    let output = update(&root, &registry, &["--all", "--semantic", "--wait"]);
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "failed");
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report}");
    let error = errors[0].as_str().unwrap();
    assert!(error.contains("Mixed sources / broken-doc"), "{error}");
    assert!(error.contains("No url found for submodule path"), "{error}");
    assert!(report["search"]["indexed_sources"].as_u64().unwrap() > 0);
    assert_eq!(report["search"]["pending_sources"], 0);
    assert_eq!(report["search"]["partial_sources"], 1);
    assert_eq!(report["search"]["errors"], serde_json::json!([]));
    let databases: Vec<_> = std::fs::read_dir(root.cache.root().join("search"))
        .unwrap()
        .map(|entry| entry.unwrap().path().join("search.sqlite"))
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(databases.len(), 1);
    let db = rusqlite::Connection::open(&databases[0]).unwrap();
    let hits: i64 = db
        .query_row(
            "SELECT count(*) FROM terms WHERE terms MATCH 'evidence'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(hits > 0, "healthy documentation was not indexed");
}
