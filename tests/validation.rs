#[test]
fn validates_the_shared_schema_corpus() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/validation.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let body = serde_json::to_string(&case["value"]).unwrap();
        let mut submodules = Vec::new();
        let actual = if case["kind"] == "index" {
            let parsed = aiexandria::index::Index::parse(&body, case["name"].as_str().unwrap());
            if let Ok(index) = &parsed {
                submodules.extend(index.entries.iter().filter_map(|entry| {
                    use aiexandria::index::EntryKind;
                    match &entry.kind {
                        EntryKind::Doc { repo, .. }
                        | EntryKind::Index { repo, .. }
                        | EntryKind::Merge { repo, .. }
                        | EntryKind::Command { repo, .. } => {
                            repo.as_ref().map(|repo| repo.submodules)
                        }
                    }
                }));
            }
            parsed.is_ok()
        } else {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("registry.yaml");
            std::fs::write(&path, body).unwrap();
            let parsed = aiexandria::registry::RegistryConfig::load_from(&path);
            if let Ok(registry) = &parsed {
                submodules.extend(registry.indexes.values().map(|entry| entry.repo.submodules));
            }
            parsed.is_ok()
        };
        assert_eq!(actual, case["valid"].as_bool().unwrap(), "{}", case["name"]);
        if let Some(expected) = case.get("expected_submodules") {
            assert_eq!(&serde_json::json!(submodules), expected, "{}", case["name"]);
        }
    }
}
