//! End-to-end integration tests for AIexandria.
//!
//! Spins up a local git repo on disk, points AIexandria at it, and exercises
//! the full resolver pipeline (clone -> parse -> walk -> read/run). The MCP
//! wire protocol is the `rmcp` crate's responsibility and isn't re-tested here;
//! we drive the same library types the MCP server uses.

use std::path::Path;

use aiexandria::auth::AuthConfig;
use aiexandria::cache::{RefreshPolicy, RepoCache};
use aiexandria::command::CommandPolicyChain;
use aiexandria::index::GitRef;
use aiexandria::resolver::{descend, Resolver};
use git2::{Repository, Signature};
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// Build a fully-populated docs repo on disk and return its file:// URL.
fn make_docs_repo(dir: &Path) -> String {
    write(
        &dir.join("pinakes.yaml"),
        r#"
version: 1
name: testproj
command_policy:
  allow: '^echo '
  timeout_secs: 5
entries:
  - id: readme
    name: README
    description: Project overview
    kind: doc
    path: README.md
  - id: subindex
    name: Sub-section
    kind: index
    path: docs/sub.yaml
  - id: greet
    name: Greeting
    kind: command
    argv: ["echo", "hello {query}"]
  - id: badcat
    name: Forbidden
    kind: command
    argv: ["cat", "/etc/passwd"]
"#,
    );
    write(&dir.join("README.md"), "# Test\n\nReadme body.\n");
    write(
        &dir.join("docs/sub.yaml"),
        r#"
version: 1
name: sub
entries:
  - id: deep
    name: Deep
    kind: doc
    path: docs/deep.md
"#,
    );
    write(&dir.join("docs/deep.md"), "# Deep\n\nDeeper body.\n");

    let repo = Repository::init_opts(dir, git2::RepositoryInitOptions::new().initial_head("main"))
        .unwrap();
    {
        let mut index = repo.index().unwrap();
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }
    format!("file://{}", dir.display())
}

fn cache(tmp: &Path) -> RepoCache {
    RepoCache::new(tmp.join("cache"), AuthConfig::default())
}

#[test]
fn resolves_root_lists_entries_reads_doc_and_descends() {
    let tmp = TempDir::new().unwrap();
    let url = make_docs_repo(&tmp.path().join("repo"));
    let cache = cache(tmp.path());
    let resolver = Resolver::new(&cache);

    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };
    let root = resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();

    let ids: Vec<_> = root.views().into_iter().map(|v| v.id).collect();
    assert_eq!(ids, vec!["readme", "subindex", "greet", "badcat"]);

    let readme = root.find("readme").unwrap();
    let body = resolver.read_doc(readme).unwrap();
    assert!(body.contains("Readme body."));

    let sub = descend(&resolver, &root, "subindex").unwrap();
    let deep_ids: Vec<_> = sub.views().into_iter().map(|v| v.id).collect();
    assert_eq!(deep_ids, vec!["deep"]);

    let deep = sub.find("deep").unwrap();
    assert!(resolver.read_doc(deep).unwrap().contains("Deeper body."));
}

#[test]
fn rejects_duplicate_ids_after_merge_flattening() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let url = make_docs_repo(&repo_dir);
    write(
        &repo_dir.join("pinakes.yaml"),
        r#"
version: 1
name: duplicates
entries:
  - id: same
    name: Local
    kind: doc
    path: README.md
  - kind: merge
    path: shared.yaml
"#,
    );
    write(
        &repo_dir.join("shared.yaml"),
        r#"
version: 1
name: shared
entries:
  - id: same
    name: Merged
    kind: doc
    path: README.md
"#,
    );
    let repo = Repository::open(&repo_dir).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    let sig = Signature::now("test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "duplicates", &tree, &[&parent])
        .unwrap();

    let cache = cache(tmp.path());
    let resolver = Resolver::new(&cache);
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };
    let error = resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap_err();
    assert!(error.to_string().contains("duplicate entry id 'same'"));
}

#[test]
fn command_policy_allows_matching_and_denies_others() {
    let tmp = TempDir::new().unwrap();
    let url = make_docs_repo(&tmp.path().join("repo"));
    let cache = cache(tmp.path());
    let resolver = Resolver::new(&cache);

    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };
    let root = resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();

    let greet = root.find("greet").unwrap();
    let out = resolver.run(greet, "world").unwrap();
    assert_eq!(out.trim(), "hello world");

    let bad = root.find("badcat").unwrap();
    let err = resolver.run(bad, "").unwrap_err().to_string();
    assert!(err.contains("not allowed by regex"), "got: {err}");
}

#[test]
fn cache_is_reused_across_calls() {
    let tmp = TempDir::new().unwrap();
    let url = make_docs_repo(&tmp.path().join("repo"));
    let cache = cache(tmp.path());
    let resolver = Resolver::new(&cache);
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };

    resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();
    // Locate the freshness sentinel and snapshot its mtime; it must not change
    // on the second call because the default policy is RefreshPolicy::Never.
    let sentinel = find_sentinel(&tmp.path().join("cache"));
    let mtime1 = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();
    let mtime2 = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    assert_eq!(mtime1, mtime2, "second call should hit cache, not re-fetch");
}

#[test]
fn read_file_reads_arbitrary_path_and_rejects_traversal() {
    let tmp = TempDir::new().unwrap();
    let url = make_docs_repo(&tmp.path().join("repo"));
    let cache = cache(tmp.path());
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };
    let dir = cache.ensure(&git_ref).unwrap();

    let body = cache.read_file(&dir, "docs/deep.md").unwrap();
    assert!(body.contains("Deeper body."));

    let err = cache
        .read_file(&dir, "../../etc/passwd")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("escapes repo root") || err.contains("not found"),
        "got: {err}"
    );
}

#[test]
fn concurrent_ensure_serializes() {
    // Two threads racing to clone the same cold cache entry must both succeed —
    // without per-dir locking the second hits `.git/config.lock: File exists`.
    let tmp = TempDir::new().unwrap();
    let url = make_docs_repo(&tmp.path().join("repo"));
    let cache = std::sync::Arc::new(cache(tmp.path()));
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let cache = cache.clone();
            let git_ref = git_ref.clone();
            std::thread::spawn(move || cache.ensure(&git_ref))
        })
        .collect();
    for h in handles {
        h.join()
            .unwrap()
            .expect("concurrent ensure should not race");
    }
}

#[test]
fn cold_clone_of_bogus_repo_surfaces_error_not_warming_forever() {
    // A clone that fails (unreachable URL) should surface a real error once
    // the background job finishes — not hang, and not warm forever. We use a
    // file:// URL to a nonexistent path so it fails fast without network.
    let tmp = TempDir::new().unwrap();
    let cache = cache(tmp.path());
    let git_ref = GitRef {
        url: format!("file://{}/does-not-exist", tmp.path().display()),
        branch: Some("main".into()),
        ..Default::default()
    };

    // Retry across the warming window; the background clone fails quickly, so
    // we should get a concrete error well before this loop exhausts.
    let mut last = String::new();
    for _ in 0..50 {
        match cache.ensure(&git_ref) {
            Ok(_) => panic!("clone of nonexistent repo should not succeed"),
            Err(e) => {
                last = e.to_string();
                if !last.contains("still being fetched") {
                    break; // got the real failure, not a warming signal
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        !last.contains("still being fetched"),
        "should surface the real clone error, got warming: {last}"
    );
}

fn find_sentinel(cache_root: &Path) -> std::path::PathBuf {
    fn walk(p: &Path, out: &mut Option<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).unwrap().flatten() {
            let path = e.path();
            if path.file_name().and_then(|s| s.to_str()) == Some(".pinakes-fetched") {
                *out = Some(path);
                return;
            }
            if path.is_dir() {
                walk(&path, out);
                if out.is_some() {
                    return;
                }
            }
        }
    }
    let mut found = None;
    walk(cache_root, &mut found);
    found.expect("sentinel file not found")
}

/// Drives the refresh code path: with a sub-second interval, a second call
/// after a small sleep should re-fetch and the working tree should reflect a
/// new upstream commit.
#[test]
fn refresh_policy_pulls_new_commits() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let url = make_docs_repo(&repo_dir);
    let cache = RepoCache::new(tmp.path().join("cache"), AuthConfig::default())
        .with_default_refresh(RefreshPolicy::Every(std::time::Duration::from_millis(50)));
    let resolver = Resolver::new(&cache);
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };

    let root = resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();
    assert!(resolver
        .read_doc(root.find("readme").unwrap())
        .unwrap()
        .contains("Readme body."));

    // Mutate upstream and commit again.
    write(&repo_dir.join("README.md"), "# Test\n\nUpdated body.\n");
    let upstream = Repository::open(&repo_dir).unwrap();
    let mut idx = upstream.index().unwrap();
    idx.add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    idx.write().unwrap();
    let tree = upstream.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    let head = upstream.head().unwrap().peel_to_commit().unwrap();
    upstream
        .commit(Some("HEAD"), &sig, &sig, "update", &tree, &[&head])
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(80));

    let root2 = resolver
        .load_index(&git_ref, "pinakes.yaml", &CommandPolicyChain::default())
        .unwrap();
    let body = resolver.read_doc(root2.find("readme").unwrap()).unwrap();
    assert!(body.contains("Updated body."), "got: {body}");
}

/// A repo with a `.gitmodules` entry that has a `path` but no `url` (a broken
/// or placeholder submodule) must still clone cleanly — the URL-less submodule
/// is skipped, not treated as a fatal error.
#[test]
fn urlless_submodule_is_skipped_not_fatal() {
    let tmp = TempDir::new().unwrap();
    let repo_dir = tmp.path().join("repo");
    let url = make_docs_repo(&repo_dir);

    // Add a `.gitmodules` declaring a submodule with a path but no url, and
    // commit it. libgit2 surfaces this as "no URL configured for submodule".
    write(
        &repo_dir.join(".gitmodules"),
        "[submodule \"vendored\"]\n\tpath = vendored\n",
    );
    let upstream = Repository::open(&repo_dir).unwrap();
    let mut idx = upstream.index().unwrap();
    idx.add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    idx.write().unwrap();
    let tree = upstream.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = Signature::now("t", "t@t").unwrap();
    let head = upstream.head().unwrap().peel_to_commit().unwrap();
    upstream
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "add broken submodule",
            &tree,
            &[&head],
        )
        .unwrap();

    let cache = cache(tmp.path());
    let git_ref = GitRef {
        url,
        branch: Some("main".into()),
        ..Default::default()
    };
    // Must not error on the URL-less submodule.
    let dir = cache
        .ensure(&git_ref)
        .expect("clone should skip url-less submodule");
    assert!(cache
        .read_file(&dir, "README.md")
        .unwrap()
        .contains("Readme"));
}
