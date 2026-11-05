use super::*;

fn collection(config: SearchConfig) -> (Fixture, Fixture, SearchService) {
    let first = Fixture::new();
    let second = Fixture::new();
    std::fs::write(
        second.repo.join("guide.md"),
        "# Replication\nTransactions preserve consistency across replicas.\n",
    )
    .unwrap();
    std::fs::write(
        second.repo.join("database.md"),
        "# Consensus\nA quorum agrees on transactions and consistency.\n",
    )
    .unwrap();
    git(&second.repo, &["add", "."]);
    git(&second.repo, &["commit", "-m", "different evidence"]);
    first.warm();
    first
        .cache
        .ensure_fresh_until(
            &second.origin,
            RefreshPolicy::Never,
            Instant::now() + Duration::from_secs(60),
        )
        .unwrap_or_else(|error| {
            panic!(
                "second repository fixture did not finish: {error}; state={}",
                serde_json::to_string(&first.cache.state(&second.origin)).unwrap()
            )
        });
    let entry = |origin: &GitRef| RegistryEntry {
        repo: origin.clone(),
        path: "pinakes.yaml".into(),
        description: None,
        refresh: Some(crate::registry::RefreshSetting::Never),
    };
    let registry = Arc::new(RegistryConfig {
        indexes: std::collections::BTreeMap::from([
            ("first".into(), entry(&first.origin)),
            ("alias".into(), entry(&first.origin)),
            ("second".into(), entry(&second.origin)),
        ]),
    });
    let service = SearchService::new(config, registry, first.cache.clone()).unwrap();
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    (first, second, service)
}

fn prepare(
    service: &SearchService,
    indexes: &[&str],
    sources: &[String],
) -> (global::Prepared, RepoCache) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let cache = service
        .inner
        .cache
        .session(Freshness::CachedOk, deadline)
        .cached_only();
    let indexes: Vec<_> = indexes.iter().map(|s| s.to_string()).collect();
    let filters: Vec<_> = sources
        .iter()
        .map(|id| {
            let descriptor = service.inner.store.descriptor(id).unwrap().unwrap();
            global::SourceFilter {
                id: id.clone(),
                index: descriptor.route.index,
                path: descriptor.route.path,
            }
        })
        .collect();
    let prepared = service
        .prepare_global(&cache, &indexes, &filters, deadline)
        .unwrap();
    (prepared, cache)
}

#[test]
fn global_union_deduplicates_aliases_before_ranking_and_filters_sources() {
    let (first, second, service) = collection(SearchConfig {
        enabled: true,
        ..Default::default()
    });
    std::fs::rename(&first.repo, first.temp.path().join("offline")).unwrap();
    std::fs::rename(&second.repo, second.temp.path().join("offline")).unwrap();
    let (prepared, cache) = prepare(&service, &[], &[]);
    let result = prepared
        .execute("transactions consistency", Mode::Keyword, 5)
        .unwrap();
    assert_eq!(result["status"], "ready");
    assert_eq!(
        result["coverage"]["documents_searched"], 4,
        "curated/cache/registry aliases must share evidence"
    );
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    let revisions: std::collections::HashSet<_> = hits
        .iter()
        .map(|h| h["revision"].as_str().unwrap())
        .collect();
    assert_eq!(
        revisions.len(),
        2,
        "both repositories should contribute evidence"
    );
    let identities: std::collections::HashSet<_> = hits
        .iter()
        .map(|h| {
            (
                h["revision"].to_string(),
                h["file"].to_string(),
                h["offset"].to_string(),
            )
        })
        .collect();
    assert_eq!(identities.len(), hits.len());
    let snapshot = cache.response_metadata()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    for hit in hits {
        assert!(hit["source"]["search"].is_object());
        let root = &service.inner.registry.indexes[hit["read"]["index"].as_str().unwrap()];
        let followup = service.inner.cache.session(
            Freshness::CachedOk,
            Instant::now() + Duration::from_secs(10),
        );
        followup.apply_snapshot(&root.repo, &snapshot).unwrap();
        let (dir, _) = followup
            .ensure_path(
                &root.repo,
                RefreshPolicy::Never,
                hit["file"].as_str().unwrap(),
            )
            .unwrap();
        assert_eq!(followup.snapshot_revision(&dir).unwrap(), hit["revision"]);
        assert!(followup
            .read_file(&dir, hit["file"].as_str().unwrap())
            .unwrap()
            .contains("consistency"));
    }
    let (restricted, _) = prepare(&service, &["second"], &[]);
    assert!(
        restricted.execute("acorns", Mode::Keyword, 5).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let source = service
        .sources()
        .into_iter()
        .find(|s| s.route.index == "first")
        .unwrap();
    let (restricted, _) = prepare(&service, &["first"], &[source.id]);
    let hits = restricted.execute("acorns", Mode::Keyword, 5).unwrap();
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1);
    assert_eq!(hits["hits"][0]["read"]["index"], "first");
}

#[test]
fn global_query_embeds_once_and_keeps_keyword_fallback() {
    let provider = MockProvider::new();
    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        embedding: Some(provider.config()),
        ..Default::default()
    });
    let (prepared, _) = prepare(&service, &[], &[]);
    let before = provider.requests.lock().unwrap().len();
    let result = prepared.execute("woodland", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["mode_used"], "hybrid");
    assert_eq!(result["semantic"]["status"], "ready");
    assert_eq!(provider.requests.lock().unwrap().len(), before + 1);
    provider.fail.store(true, Ordering::Relaxed);
    let fallback = prepared.execute("transactions", Mode::Hybrid, 5).unwrap();
    assert_eq!(fallback["mode_used"], "keyword");
    assert_eq!(fallback["semantic"]["status"], "unavailable");
    assert!(!fallback["hits"].as_array().unwrap().is_empty());
    let semantic = prepared.execute("woodland", Mode::Semantic, 5).unwrap();
    assert_eq!(semantic["status"], "unavailable");
    assert!(semantic["mode_used"].is_null());
    assert!(semantic["hits"].as_array().unwrap().is_empty());
}

#[test]
fn global_reports_uncached_roots_and_never_uses_obsolete_membership() {
    let (first, _second, service) = collection(SearchConfig {
        enabled: true,
        ..Default::default()
    });
    let (old, old_cache) = prepare(&service, &[], &[]);
    let result = old.execute("acorns", Mode::Keyword, 5).unwrap();
    let snapshot = old_cache.response_metadata()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    let revision = result["hits"][0]["revision"].as_str().unwrap().to_owned();
    std::fs::write(
        first.repo.join("guide.md"),
        "# Oceans\nOtters swim in rivers.\n",
    )
    .unwrap();
    git(&first.repo, &["add", "."]);
    git(&first.repo, &["commit", "-m", "replace"]);
    first.refresh();
    let (pending, _) = prepare(&service, &[], &[]);
    let result = pending.execute("acorns", Mode::Keyword, 5).unwrap();
    assert!(result["coverage"]["partial"].as_bool().unwrap());
    assert!(result["coverage"]["pending_sources"].as_u64().unwrap() > 0);
    assert!(result["hits"].as_array().unwrap().is_empty());
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let raced = old.execute("acorns", Mode::Keyword, 5).unwrap();
    assert!(raced["hits"].as_array().unwrap().is_empty());
    assert!(raced["coverage"]["pending_sources"].as_u64().unwrap() > 0);
    let followup = service.inner.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    followup.apply_snapshot(&first.origin, &snapshot).unwrap();
    let (dir, _) = followup
        .ensure_path(&first.origin, RefreshPolicy::Never, "guide.md")
        .unwrap();
    assert_eq!(followup.snapshot_revision(&dir).unwrap(), revision);
    assert!(followup
        .read_file(&dir, "guide.md")
        .unwrap()
        .contains("acorns"));

    let cold = Fixture::new();
    let service = cold.service(None);
    let (prepared, _) = prepare(&service, &[], &[]);
    let result = prepared.execute("acorns", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["status"], "unavailable");
    assert!(result.get("retry_after_ms").is_none());
    assert_eq!(result["coverage"]["unavailable_levels"], 1);
    assert!(!cold.cache.path_for(&cold.origin).exists());
}

#[test]
fn global_bounds_work_and_does_not_search_disabled_sources() {
    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        global_max_chunks: 1,
        ..Default::default()
    });
    let (prepared, _) = prepare(&service, &[], &[]);
    let result = prepared
        .execute("transactions squirrels", Mode::Keyword, 5)
        .unwrap();
    assert_eq!(result["coverage"]["chunks_searched"], 1);
    assert_eq!(result["coverage"]["limited"], true);
    let bounded = bounded(result, 1024).unwrap();
    assert!(bounded.len() <= 1024);
    let expired = service
        .inner
        .store
        .query_global(
            &prepared.sources,
            "transactions",
            Mode::Keyword,
            None,
            global::QueryOptions {
                limit: 5,
                max_chunks: 10,
                deadline: Instant::now(),
            },
        )
        .unwrap();
    assert!(expired.timed_out);

    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        global_max_sources: 1,
        ..Default::default()
    });
    let (prepared, _) = prepare(&service, &[], &[]);
    let result = prepared.execute("transactions", Mode::Keyword, 5).unwrap();
    assert_eq!(result["coverage"]["sources_searched"], 1);
    assert_eq!(result["coverage"]["limited"], true);

    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        sources: vec![
            config::SourceRule {
                index: "first".into(),
                enabled: Some(false),
                ..Default::default()
            },
            config::SourceRule {
                index: "alias".into(),
                enabled: Some(false),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let (prepared, _) = prepare(&service, &[], &[]);
    assert!(
        prepared.execute("acorns", Mode::Keyword, 5).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn slow_global_provider_leaves_time_for_keyword_fallback() {
    let provider = MockProvider::new();
    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        embedding: Some(provider.config()),
        global_timeout_secs: 1,
        ..Default::default()
    });
    let (prepared, _) = prepare(&service, &[], &[]);
    provider.delay_ms.store(1500, Ordering::Relaxed);
    let start = Instant::now();
    let result = prepared.execute("transactions", Mode::Hybrid, 5).unwrap();
    assert!(start.elapsed() < Duration::from_millis(1400));
    assert_eq!(result["status"], "ready");
    assert_eq!(result["mode_used"], "keyword");
    assert!(!result["hits"].as_array().unwrap().is_empty());
}

#[test]
fn global_hybrid_keeps_ready_vectors_while_another_source_is_preparing() {
    let provider = MockProvider::new();
    let (first, second, service) = collection(SearchConfig {
        enabled: true,
        embedding: Some(provider.config()),
        ..Default::default()
    });
    std::fs::write(
        second.repo.join("guide.md"),
        "# Changed\nReplicated transactions use a consensus protocol.\n",
    )
    .unwrap();
    git(&second.repo, &["add", "."]);
    git(&second.repo, &["commit", "-m", "advance"]);
    first
        .cache
        .update(
            &second.origin,
            RefreshPolicy::Never,
            Selection::all(),
            Some(crate::cache::now_ms()),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
    let cache = first.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    let inventory = corpus::inventory(
        &cache,
        &service.inner.registry,
        Instant::now() + Duration::from_secs(10),
        Some("second"),
        &[],
    );
    for source in inventory.sources {
        service
            .prepare(source, &cache, Instant::now() + Duration::from_secs(10))
            .unwrap();
    }
    let (prepared, _) = prepare(&service, &[], &[]);
    let result = prepared.execute("transactions", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["mode_used"], "hybrid");
    assert_eq!(result["semantic"]["status"], "partial");
    assert_eq!(result["coverage"]["partial"], true);
    assert!(
        result["coverage"]["embedded_chunks"].as_u64().unwrap()
            < result["coverage"]["chunks_searched"].as_u64().unwrap()
    );
    assert!(
        result["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["excerpt"]
                .as_str()
                .unwrap()
                .contains("consensus protocol")),
        "new lexical evidence must remain available"
    );
}

#[test]
fn global_source_filters_do_not_count_unrelated_uncached_metadata() {
    let (first, _second, service) = collection(SearchConfig {
        enabled: true,
        ..Default::default()
    });
    let unrelated = Fixture::new();
    let mut body = std::fs::read_to_string(first.repo.join("pinakes.yaml")).unwrap();
    body.push_str(&format!("- id: unrelated\n  name: Unrelated\n  kind: index\n  path: pinakes.yaml\n  repo: {{url: '{}', branch: main}}\n", unrelated.origin.url));
    std::fs::write(first.repo.join("pinakes.yaml"), body).unwrap();
    git(&first.repo, &["add", "."]);
    git(&first.repo, &["commit", "-m", "add uncached child"]);
    first.refresh();
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let (all, _) = prepare(&service, &["first"], &[]);
    assert!(
        all.execute("acorns", Mode::Keyword, 5).unwrap()["coverage"]["unavailable_levels"]
            .as_u64()
            .unwrap()
            > 0
    );
    let source = service
        .sources()
        .into_iter()
        .find(|s| s.route.index == "first")
        .unwrap();
    let (filtered, _) = prepare(&service, &["first"], &[source.id]);
    let result = filtered.execute("acorns", Mode::Keyword, 5).unwrap();
    assert_eq!(result["coverage"]["unavailable_levels"], 0);
    assert_eq!(result["coverage"]["partial"], false);
    assert!(!first.cache.path_for(&unrelated.origin).exists());
}

#[test]
fn global_budget_changes_reuse_existing_index_and_embeddings() {
    let provider = MockProvider::new();
    let (_first, _second, service) = collection(SearchConfig {
        enabled: true,
        embedding: Some(provider.config()),
        ..Default::default()
    });
    let mut config = service.inner.config.clone();
    config.global_max_chunks = 1;
    config.global_timeout_secs = 5;
    let bounded = SearchService::new(
        config,
        service.inner.registry.clone(),
        service.inner.cache.clone(),
    )
    .unwrap();
    let before = provider.requests.lock().unwrap().len();
    let report = bounded
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(report.changed_sources, 0);
    assert_eq!(report.embedded_chunks, 0);
    assert_eq!(provider.requests.lock().unwrap().len(), before);
    let (prepared, _) = prepare(&bounded, &[], &[]);
    assert_eq!(
        prepared.execute("woodland", Mode::Hybrid, 5).unwrap()["coverage"]["chunks_searched"],
        1
    );
}
