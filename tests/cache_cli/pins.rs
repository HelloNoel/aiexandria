use super::*;
use aiexandria::cache::Selection;
use std::io::Write;

fn pin(f: &Fixture, commit: &str) -> GitRef {
    GitRef {
        branch: None,
        commit: Some(commit.into()),
        ..f.origin.clone()
    }
}
fn git_input(dir: &Path, args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn full_pins_fetch_unadvertised_commits_in_both_object_formats() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, false);
        git(
            &f.repo,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
        let commit = format!(
            "tree {tree}\nauthor Fixture <fixture@example.invalid> 1 +0000\ncommitter Fixture <fixture@example.invalid> 1 +0000\n\nunadvertised commit\n"
        );
        let oid = git_input(
            &f.repo,
            &["hash-object", "-w", "-t", "commit", "--stdin"],
            &commit,
        );
        assert!(!git(&f.repo, &["rev-list", "--all"]).contains(&oid));
        let dir = f
            .request()
            .ensure_path(&pin(&f, &oid), RefreshPolicy::Never, "doc.md")
            .unwrap()
            .0;
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), oid);
        assert_eq!(git(&dir, &["rev-parse", "FETCH_HEAD"]), oid);
        assert_eq!(f.cache.read_file(&dir, "doc.md").unwrap(), "old evidence\n");
    }
}

#[test]
fn full_and_abbreviated_pins_keep_the_revision_during_force_and_hydration() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, true);
        let oid = git(&f.repo, &["rev-parse", "HEAD"]);
        let mut pinned = Vec::new();
        for input in [
            oid[..7].to_string(),
            oid[..oid.len() - 1].to_string(),
            oid.to_uppercase(),
        ] {
            let origin = pin(&f, &input);
            let request = f.request();
            let dir = request
                .ensure_path(&origin, RefreshPolicy::Never, "doc.md")
                .unwrap()
                .0;
            assert_eq!(git(&dir, &["rev-parse", "HEAD"]), oid);
            assert!(!dir.join("extra.md").exists());
            let token = request.response_metadata()["snapshot"]
                .as_str()
                .unwrap()
                .to_owned();
            let checked = f.cache.state(&origin).unwrap().checked_ms;
            pinned.push((origin, token, checked));
        }
        f.change();
        for (origin, token, checked) in pinned {
            let hydrated = f
                .cache
                .update(
                    &origin,
                    RefreshPolicy::Every(Duration::from_millis(1)),
                    Selection::path("extra.md").unwrap(),
                    Some(u64::MAX),
                    Instant::now() + Duration::from_secs(20),
                )
                .unwrap();
            assert_eq!(git(&hydrated, &["rev-parse", "HEAD"]), oid);
            assert_eq!(
                f.cache.read_file(&hydrated, "extra.md").unwrap(),
                "old extra\n"
            );
            assert_eq!(
                f.cache.state(&origin).unwrap().checked_ms,
                checked,
                "force checked a moving ref for a pin"
            );
            let followup = f.request();
            followup.apply_snapshot(&origin, &token).unwrap();
            let dir = followup
                .ensure_path(&origin, RefreshPolicy::Never, "doc.md")
                .unwrap()
                .0;
            assert_eq!(f.cache.read_file(&dir, "doc.md").unwrap(), "old evidence\n");
        }
    }
}

#[test]
fn rejects_pins_for_the_wrong_object_format_and_non_commit_objects() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, false);
        git(
            &f.repo,
            &[
                "-c",
                "tag.gpgsign=false",
                "tag",
                "-a",
                "release",
                "-m",
                "release",
            ],
        );
        for reference in ["HEAD:doc.md", "HEAD^{tree}", "refs/tags/release"] {
            let object = git(&f.repo, &["rev-parse", reference]);
            let error = f
                .request()
                .ensure_path(&pin(&f, &object), RefreshPolicy::Never, "doc.md")
                .unwrap_err();
            assert!(
                error.to_string().contains("commit object"),
                "{format} {reference}: {error}"
            );
        }
        let missing = pin(&f, "0000000");
        let error = f
            .request()
            .ensure_path(&missing, RefreshPolicy::Never, "doc.md")
            .unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
        if format == "sha1" {
            for length in [41, 64] {
                let origin = pin(&f, &"a".repeat(length));
                assert!(
                    origin.validate().is_ok(),
                    "syntax validation cannot know the repository format"
                );
                let error = f
                    .request()
                    .ensure_path(&origin, RefreshPolicy::Never, "doc.md")
                    .unwrap_err();
                assert!(error.to_string().contains("40-character sha1"), "{error}");
                assert!(f.cache.state(&origin).unwrap().current.is_none());
            }
        }
    }
}

#[test]
fn hexadecimal_branch_names_do_not_override_commit_prefixes() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, true);
        let oid = git(&f.repo, &["rev-parse", "HEAD"]);
        f.change();
        git(&f.repo, &["branch", &oid[..7], "HEAD"]);
        let dir = f
            .request()
            .ensure_path(&pin(&f, &oid[..7]), RefreshPolicy::Never, "doc.md")
            .unwrap()
            .0;
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]), oid);
        assert_eq!(f.cache.read_file(&dir, "doc.md").unwrap(), "old evidence\n");
    }
}

#[test]
fn sha256_root_registry_pins_preserve_inherited_document_references() {
    let f = Fixture::new("sha256", true);
    let oid = git(&f.repo, &["rev-parse", "HEAD"]);
    f.change();
    let registry = f.temp.path().join("pinned-registry.yaml");
    std::fs::write(&registry, format!("indexes:\n  pinned:\n    repo: {{url: '{}', commit: '{}', submodules: false}}\n    path: pinakes.yaml\n    refresh: 1s\n", f.origin.url, oid)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_aiexandria"))
        .arg("--registry")
        .arg(registry)
        .arg("--cache")
        .arg(f.cache.root())
        .args(["update", "--index", "pinned", "--force", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let config: aiexandria::registry::RegistryConfig = serde_yaml::from_str(&format!("indexes:\n  pinned:\n    repo: {{url: '{}', commit: '{}', submodules: false}}\n    path: pinakes.yaml\n", f.origin.url, oid)).unwrap();
    let request = f.request();
    let resolver = aiexandria::resolver::Resolver::new(&request);
    let level = resolver
        .load_index(
            &config.indexes["pinned"].repo,
            "pinakes.yaml",
            &aiexandria::command::CommandPolicyChain::default(),
        )
        .unwrap();
    let entry = level.find("doc").unwrap();
    assert_eq!(entry.origin.commit.as_deref(), Some(oid.as_str()));
    assert_eq!(resolver.read_doc(entry).unwrap(), "old evidence\n");
}

// Find real commit-object collisions in seven hexadecimal characters. Only
// the two colliding objects are written; hashing candidates is entirely local.
fn collision(f: &Fixture, format: &str) -> (String, String, String) {
    use sha2::Digest;
    let tree = git(&f.repo, &["rev-parse", "HEAD^{tree}"]);
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let body = |n| {
        format!("tree {tree}\nauthor Fixture <fixture@example.invalid> 0 +0000\ncommitter Fixture <fixture@example.invalid> 0 +0000\n\nprefix-collision-{n}\n")
    };
    for n in 0..300_000 {
        let value = body(n);
        let oid = if format == "sha1" {
            git2::Oid::hash_object(git2::ObjectType::Commit, value.as_bytes())
                .unwrap()
                .to_string()
        } else {
            let mut hasher = sha2::Sha256::new();
            hasher.update(format!("commit {}\0", value.len()));
            hasher.update(value.as_bytes());
            hex::encode(hasher.finalize())
        };
        if let Some(previous) = seen.insert(oid[..7].into(), n) {
            return (oid[..7].into(), body(previous), value);
        }
    }
    panic!("no seven-character collision found");
}

#[test]
fn ambiguous_prefixes_fail_but_an_already_resolved_pin_keeps_its_identity() {
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format, true);
        let (prefix, a, b) = collision(&f, format);
        let oid_a = git_input(
            &f.repo,
            &["hash-object", "-w", "-t", "commit", "--stdin"],
            &a,
        );
        git(&f.repo, &["update-ref", "refs/heads/collision-a", &oid_a]);
        let origin = pin(&f, &prefix);
        let dir = f
            .request()
            .ensure_path(&origin, RefreshPolicy::Never, "doc.md")
            .unwrap()
            .0;
        // Simulate another object being acquired after the pin was resolved.
        let oid_b = git_input(&dir, &["hash-object", "-w", "-t", "commit", "--stdin"], &b);
        assert_ne!(oid_a, oid_b);
        assert!(oid_b.starts_with(&prefix));
        let expanded = f
            .request()
            .ensure_path(&origin, RefreshPolicy::Never, "extra.md")
            .unwrap()
            .0;
        assert_eq!(git(&expanded, &["rev-parse", "HEAD"]), oid_a);
        assert_eq!(
            f.cache.read_file(&expanded, "extra.md").unwrap(),
            "old extra\n"
        );
        git_input(
            &f.repo,
            &["hash-object", "-w", "-t", "commit", "--stdin"],
            &b,
        );
        git(&f.repo, &["update-ref", "refs/heads/collision-b", &oid_b]);
        let cold = RepoCache::new(f.temp.path().join("ambiguous-cache"), AuthConfig::default())
            .session(
                Freshness::CachedOk,
                Instant::now() + Duration::from_secs(20),
            );
        let error = cold
            .ensure_path(&origin, RefreshPolicy::Never, "doc.md")
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
    }
}
