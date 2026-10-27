#[test]
fn validates_the_shared_schema_corpus() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/validation.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let body = serde_json::to_string(&case["value"]).unwrap();
        let actual = if case["kind"] == "index" {
            aiexandria::index::Index::parse(&body, case["name"].as_str().unwrap()).is_ok()
        } else {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("registry.yaml");
            std::fs::write(&path, body).unwrap();
            aiexandria::registry::RegistryConfig::load_from(&path).is_ok()
        };
        assert_eq!(actual, case["valid"].as_bool().unwrap(), "{}", case["name"]);
    }
}
