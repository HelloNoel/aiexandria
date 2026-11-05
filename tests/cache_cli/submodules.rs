use super::*;

#[test]
fn submodules_are_opt_in_and_legacy_cache_options_keep_their_meaning() {
    let parent = Fixture::new("sha1", true);
    let child = Fixture::new("sha1", true);
    git(
        &parent.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child.repo.to_str().unwrap(),
            "deps/docs",
        ],
    );
    git(&parent.repo, &["commit", "-am", "nested documentation"]);
    let omitted: GitRef = serde_json::from_value(serde_json::json!({
        "url":parent.origin.url, "branch":"main"
    }))
    .unwrap();
    assert!(!omitted.submodules);
    assert!(!GitRef::default().submodules);
    let disabled = parent.request().ensure(&omitted).unwrap();
    assert!(disabled.join("doc.md").exists());
    assert!(!disabled.join("deps/docs/doc.md").exists());
    let disabled_state = disabled.parent().unwrap().join("state.json");
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(disabled_state).unwrap()).unwrap();
    assert_eq!(state["origin"]["submodules"], false);

    let enabled_ref = GitRef {
        submodules: true,
        ..omitted.clone()
    };
    let enabled = parent.request().ensure(&enabled_ref).unwrap();
    assert_ne!(enabled, disabled);
    assert_eq!(
        std::fs::read_to_string(enabled.join("deps/docs/doc.md")).unwrap(),
        "old evidence\n"
    );
    // Cache records before this revision omitted true, not false.
    let enabled_state = enabled.parent().unwrap().join("state.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&enabled_state).unwrap()).unwrap();
    assert_eq!(legacy["origin"]["submodules"], true);
    legacy["origin"]
        .as_object_mut()
        .unwrap()
        .remove("submodules");
    std::fs::write(&enabled_state, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let origins = parent.cache.cached_origins();
    assert!(origins.contains(&enabled_ref));
    assert!(origins.contains(&omitted));
    assert!(parent.cache.state(&enabled_ref).unwrap().origin.submodules);
    assert_eq!(
        parent
            .cache
            .cached_snapshot(&enabled_ref, RefreshPolicy::Never)
            .unwrap(),
        enabled
    );
    // Explicit options are retained when exporting repository references, too.
    for reference in [omitted, enabled_ref] {
        let encoded = serde_json::to_value(&reference).unwrap();
        assert_eq!(encoded["submodules"], reference.submodules);
        assert_eq!(
            serde_json::from_value::<GitRef>(encoded).unwrap(),
            reference
        );
    }
}

#[test]
fn remote_generation_skips_submodules_unless_requested() {
    let f = Fixture::new("sha1", true);
    let commit = git(&f.repo, &["rev-parse", "HEAD"]);
    git(
        &f.repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{commit},docs/missing-module"),
        ],
    );
    git(&f.repo, &["commit", "-m", "gitlink without module URL"]);
    let generate = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_aiexandria"))
            .arg("--cache")
            .arg(f.cache.root())
            .args([
                "generate",
                "--repo",
                &f.origin.url,
                "--branch",
                "main",
                "--no-search",
                "--no-nested",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    for args in [vec![], vec!["--no-submodules"]] {
        let output = generate(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        aiexandria::index::Index::parse(&String::from_utf8(output.stdout).unwrap(), "generated")
            .unwrap();
    }
    let enabled = generate(&["--submodules"]);
    assert!(!enabled.status.success());
    assert!(String::from_utf8_lossy(&enabled.stderr).contains("No url found for submodule path"));
    assert_eq!(
        generate(&["--submodules", "--no-submodules"]).status.code(),
        Some(2)
    );
}
