use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::auth::AuthConfig;
use crate::cache::{RefreshPolicy, Selection};
use crate::index::GitRef;
use crate::registry::RegistryEntry;
use config::{EmbeddingConfig, Provider};

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Fixture {
    temp: tempfile::TempDir,
    repo: PathBuf,
    origin: GitRef,
    cache: Arc<RepoCache>,
    registry: Arc<RegistryConfig>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_format("sha1")
    }

    fn with_format(format: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("remote");
        std::fs::create_dir(&repo).unwrap();
        git(
            &repo,
            &["init", "-b", "main", &format!("--object-format={format}")],
        );
        std::fs::write(repo.join("pinakes.yaml"), "version: 1\nname: Woodland and storage\nentries:\n- {id: guide, name: guide.md, kind: doc, path: guide.md}\n- {id: command, name: Custom, kind: command, argv: [do-not-execute, '{query}']}\n").unwrap();
        std::fs::write(
            repo.join("guide.md"),
            "# Trees\nSquirrels gather acorns in trees.\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("database.md"),
            "# Transactions\nTransactions preserve database consistency.\n",
        )
        .unwrap();
        std::fs::create_dir(repo.join("node_modules")).unwrap();
        std::fs::write(
            repo.join("node_modules/noise.md"),
            "Do not include dependency noise.\n",
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "fixture"]);
        let origin = GitRef {
            url: url::Url::from_directory_path(&repo).unwrap().into(),
            branch: Some("main".into()),
            submodules: false,
            ..Default::default()
        };
        let cache = Arc::new(RepoCache::new(
            temp.path().join("cache"),
            AuthConfig::default(),
        ));
        let registry = Arc::new(RegistryConfig {
            indexes: std::collections::BTreeMap::from([(
                "public-docs".into(),
                RegistryEntry {
                    repo: origin.clone(),
                    path: "pinakes.yaml".into(),
                    description: None,
                    refresh: Some(crate::registry::RefreshSetting::Never),
                },
            )]),
        });
        Self {
            temp,
            repo,
            origin,
            cache,
            registry,
        }
    }
    fn warm(&self) {
        self.cache
            .ensure_fresh_until(
                &self.origin,
                RefreshPolicy::Never,
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
    }
    fn service(&self, embedding: Option<EmbeddingConfig>) -> SearchService {
        SearchService::new(
            SearchConfig {
                enabled: true,
                embedding,
                ..Default::default()
            },
            self.registry.clone(),
            self.cache.clone(),
        )
        .unwrap()
    }
    fn refresh(&self) {
        self.cache
            .update(
                &self.origin,
                RefreshPolicy::Never,
                Selection::all(),
                Some(crate::cache::now_ms()),
                Instant::now() + Duration::from_secs(60),
            )
            .unwrap();
    }
    fn prepare(&self, service: &SearchService, descriptor: Descriptor) -> (Prepared, RepoCache) {
        let cache = self.cache.session(
            Freshness::CachedOk,
            Instant::now() + Duration::from_secs(20),
        );
        let prepared = service
            .prepare(descriptor, &cache, Instant::now() + Duration::from_secs(20))
            .unwrap();
        (prepared, cache)
    }
}

struct MockProvider {
    endpoint: String,
    stop: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
    mismatch: Arc<AtomicBool>,
    delay_ms: Arc<std::sync::atomic::AtomicU64>,
    requests: Arc<Mutex<Vec<Value>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/api/embed", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let fail = Arc::new(AtomicBool::new(false));
        let mismatch = Arc::new(AtomicBool::new(false));
        let delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let delay = delay_ms.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (done, failing, dimensions, captured) = (
            stop.clone(),
            fail.clone(),
            mismatch.clone(),
            requests.clone(),
        );
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let body = read_request(&mut stream);
                std::thread::sleep(Duration::from_millis(delay.load(Ordering::Relaxed)));
                let inputs = body["input"].as_array().unwrap();
                let vectors: Vec<_> = inputs
                    .iter()
                    .map(|input| {
                        if dimensions.load(Ordering::Relaxed) {
                            return vec![1.0, 0.0, 0.0];
                        }
                        let text = input.as_str().unwrap().to_lowercase();
                        if text.contains("squirrels") || text.contains("woodland") {
                            vec![1.0, 0.0]
                        } else {
                            vec![0.0, 1.0]
                        }
                    })
                    .collect();
                let data: Vec<_> = vectors
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, v)| json!({"index":i,"embedding":v}))
                    .collect();
                let response = json!({"embeddings":vectors,"data":data}).to_string();
                captured.lock().unwrap().push(body);
                let status = if failing.load(Ordering::Relaxed) {
                    "503 Unavailable"
                } else {
                    "200 OK"
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len());
            }
        });
        Self {
            endpoint,
            stop,
            fail,
            mismatch,
            delay_ms,
            requests,
            thread: Some(thread),
        }
    }
    fn config(&self) -> EmbeddingConfig {
        EmbeddingConfig {
            provider: Provider::Ollama,
            endpoint: self.endpoint.clone(),
            model: "fixture".into(),
            revision: "v1".into(),
            token_env: None,
            allow_remote: false,
            query_prefix: "query: ".into(),
            document_prefix: "document: ".into(),
            timeout_secs: 2,
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let joined = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            joined.unwrap();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let lower = line.to_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let mut bytes = Vec::new();
    if chunked {
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let size = usize::from_str_radix(line.trim(), 16).unwrap();
            if size == 0 {
                break;
            }
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            bytes.extend_from_slice(&chunk[..size]);
        }
    } else {
        bytes.resize(length, 0);
        reader.read_exact(&mut bytes).unwrap();
    }
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn cache_only_inventory_never_hydrates_or_starts_git_work() {
    let f = Fixture::new();
    let absent = f
        .cache
        .cached_only()
        .ensure_path(&f.origin, RefreshPolicy::Never, "pinakes.yaml");
    assert!(matches!(absent, Err(AIexError::RefreshPending)));
    assert!(!f.cache.path_for(&f.origin).exists());
    f.cache
        .ensure_path(&f.origin, RefreshPolicy::Never, "pinakes.yaml")
        .unwrap();
    let selected = f.cache.state(&f.origin).unwrap();
    let service = f.service(None);
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty());
    assert!(service.sources().is_empty());
    assert_eq!(f.cache.state(&f.origin).unwrap().current, selected.current);
    let cached = f.cache.cached_only().ensure_path(
        &f.origin,
        RefreshPolicy::Every(Duration::ZERO),
        "database.md",
    );
    assert!(matches!(cached, Err(AIexError::RefreshPending)));
    assert_eq!(f.cache.state(&f.origin).unwrap().current, selected.current);
}

#[test]
fn cached_corpus_indexes_docs_but_never_executes_commands_or_reads_dependencies() {
    let f = Fixture::new();
    f.warm();
    // Make the remote unavailable: cached-only preparation must still work.
    std::fs::rename(&f.repo, f.temp.path().join("offline-remote")).unwrap();
    let service = f.service(None);
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let source = service.sources().pop().unwrap();
    assert_eq!(service.coverage(&source.id).unwrap().indexed_files, 2);
    let (prepared, cache) = f.prepare(&service, source);
    let result = prepared.execute("database", Mode::Keyword, 5).unwrap();
    assert_eq!(result["hits"][0]["file"], "database.md");
    let limited = prepared
        .execute("Squirrels Transactions", Mode::Keyword, 1)
        .unwrap();
    assert_eq!(limited["hits"].as_array().unwrap().len(), 1);
    assert_eq!(limited["truncated"], true);
    assert!(prepared
        .execute("dependency noise", Mode::Keyword, 5)
        .unwrap()["hits"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(cache.response_metadata()["snapshot"].is_string());
    let unavailable = prepared.execute("woodland", Mode::Semantic, 5).unwrap();
    assert_eq!(unavailable["status"], "unavailable");
}

#[test]
fn cached_corpus_indexes_markdown_extensions_but_still_excludes_dependencies() {
    let f = Fixture::new();
    for (path, body) in [
        ("overview.markdown", "Uniquequasar documentation."),
        ("GUIDE.MARKDOWN", "Uniquepulsar documentation."),
        (
            "node_modules/hidden.markdown",
            "Dependencyonly forbidden content.",
        ),
    ] {
        std::fs::write(f.repo.join(path), body).unwrap();
    }
    git(&f.repo, &["add", "."]);
    git(
        &f.repo,
        &["commit", "-m", "Markdown extension documentation"],
    );
    f.warm();
    // All inputs are already cached; finding this extension must not fetch Git.
    std::fs::rename(&f.repo, f.temp.path().join("offline-remote")).unwrap();
    let service = f.service(None);
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let source = service.sources().pop().unwrap();
    assert_eq!(service.coverage(&source.id).unwrap().indexed_files, 4);
    let (prepared, _) = f.prepare(&service, source);
    for (query, file) in [
        ("Uniquequasar", "overview.markdown"),
        ("Uniquepulsar", "GUIDE.MARKDOWN"),
    ] {
        let result = prepared.execute(query, Mode::Keyword, 5).unwrap();
        assert_eq!(result["hits"][0]["file"], file);
    }
    assert!(prepared
        .execute("Dependencyonly", Mode::Keyword, 5)
        .unwrap()["hits"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn hybrid_falls_back_then_reuses_embeddings_and_validates_query_dimensions() {
    let f = Fixture::new();
    f.warm();
    let provider = MockProvider::new();
    let service = f.service(Some(provider.config()));
    let cache = f.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    let level = corpus::level(&cache, &f.registry, "public-docs", &[]).unwrap();
    let descriptor = Descriptor::curated("public-docs", &[], &level);
    let (prepared, _) = f.prepare(&service, descriptor.clone());
    let result = prepared.execute("squirrels", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["mode_used"], "keyword");
    assert_eq!(result["semantic"]["status"], "preparing");
    assert_eq!(
        prepared
            .execute("woodland creatures", Mode::Semantic, 5)
            .unwrap()["status"],
        "preparing"
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let count = provider.requests.lock().unwrap().len();
    assert!(count > 0);
    let unchanged = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(unchanged.changed_sources, 0);
    assert_eq!(unchanged.embedded_chunks, 0);
    assert_eq!(provider.requests.lock().unwrap().len(), count);
    let result = prepared
        .execute("woodland creatures", Mode::Semantic, 5)
        .unwrap();
    assert_eq!(result["mode_used"], "semantic");
    assert_eq!(result["hits"][0]["file"], "guide.md");
    let requests = provider.requests.lock().unwrap();
    assert!(requests.iter().all(|r| r["truncate"] == false));
    assert!(requests[0]["input"][0]
        .as_str()
        .unwrap()
        .starts_with("document: "));
    assert!(requests.last().unwrap()["input"][0]
        .as_str()
        .unwrap()
        .starts_with("query: "));
    drop(requests);
    provider.mismatch.store(true, Ordering::Relaxed);
    let result = prepared.execute("squirrels", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["mode_used"], "keyword");
    assert_eq!(result["semantic"]["status"], "unavailable");
}

#[test]
fn openai_compatible_batches_are_reordered_and_remote_providers_require_opt_in() {
    let provider = MockProvider::new();
    let mut config = provider.config();
    config.provider = Provider::OpenaiCompatible;
    let vectors = config
        .embed(&["squirrels".into(), "database".into()], false)
        .unwrap();
    assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    config.endpoint = "https://embedding.example.invalid/v1/embeddings".into();
    let mut search = SearchConfig {
        enabled: true,
        embedding: Some(config),
        ..Default::default()
    };
    assert!(search.validate().is_err());
    search.embedding.as_mut().unwrap().allow_remote = true;
    search.validate().unwrap();
    search.embedding.as_mut().unwrap().endpoint =
        "http://embedding.example.invalid/v1/embeddings".into();
    assert!(search.validate().is_err());
    assert!(provider::normalize(vec![f32::NAN, 0.0]).is_err());
    assert!(provider::normalize(vec![0.0, 0.0]).is_err());
}

#[test]
fn source_removal_and_refresh_cannot_return_old_or_wrong_revision_hits() {
    let f = Fixture::new();
    f.warm();
    let service = f.service(None);
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let source = service.sources().pop().unwrap();
    let (prepared, cache) = f.prepare(&service, source.clone());
    let result = prepared.execute("squirrels", Mode::Keyword, 5).unwrap();
    let token = cache.response_metadata()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    let old = result["hits"][0]["revision"].as_str().unwrap().to_owned();
    std::fs::write(
        f.repo.join("guide.md"),
        "# New content\nOtters live near rivers.\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "advance"]);
    f.refresh();
    let (updated, _) = f.prepare(&service, source.clone());
    assert!(
        updated.execute("squirrels", Mode::Keyword, 5).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        prepared.execute("squirrels", Mode::Keyword, 5),
        Err(AIexError::RefreshPending)
    ));
    let followup = f.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    followup.apply_snapshot(&f.origin, &token).unwrap();
    let (dir, _) = followup
        .ensure_path(&f.origin, RefreshPolicy::Never, "guide.md")
        .unwrap();
    assert_eq!(followup.snapshot_revision(&dir).unwrap(), old);
    assert!(followup
        .read_file(&dir, "guide.md")
        .unwrap()
        .contains("Squirrels"));
    std::fs::write(
        f.repo.join("pinakes.yaml"),
        "version: 1\nname: Empty\nentries: []\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "remove"]);
    f.refresh();
    assert!(service.sources().is_empty());
    let cache = f.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    assert!(service
        .prepare(source, &cache, Instant::now() + Duration::from_secs(10))
        .is_err());
}

#[test]
fn concurrent_indexers_do_not_duplicate_embedding_batches() {
    let f = Fixture::new();
    f.warm();
    let provider = MockProvider::new();
    let service = f.service(Some(provider.config()));
    let other = f.service(Some(provider.config()));
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            service
                .update(None, &[], None, Instant::now() + Duration::from_secs(10))
                .unwrap()
        });
        let b = scope.spawn(|| {
            other
                .update(None, &[], None, Instant::now() + Duration::from_secs(10))
                .unwrap()
        });
        assert!(a.join().unwrap().errors.is_empty());
        assert!(b.join().unwrap().errors.is_empty());
    });
    let inputs: Vec<String> = provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|r| {
            r["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
        })
        .collect();
    assert!(!inputs.is_empty());
    assert_eq!(
        inputs.len(),
        inputs
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
    );
}

#[test]
fn provider_failure_keeps_keyword_results_available() {
    let f = Fixture::new();
    f.warm();
    let provider = MockProvider::new();
    provider.fail.store(true, Ordering::Relaxed);
    let service = f.service(Some(provider.config()));
    let report = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(!report.errors.is_empty());
    let (prepared, _) = f.prepare(&service, service.sources().pop().unwrap());
    let result = prepared.execute("database", Mode::Hybrid, 5).unwrap();
    assert_eq!(result["mode_used"], "keyword");
    assert_eq!(result["semantic"]["status"], "unavailable");
    assert!(!result["hits"].as_array().unwrap().is_empty());
}

#[test]
fn chunks_and_output_budgets_preserve_unicode_and_original_line_numbers() {
    let body = format!("# Heading\n{}\nFinal line.\n", "界".repeat(1000));
    let chunks = chunk::chunks("Document", &body, 256);
    assert!(chunks
        .iter()
        .all(|c| c.text.len() <= 257 && c.start >= 1 && c.end >= c.start));
    assert!(chunks.iter().any(|c| c.start == 2 && c.end == 2));
    assert!(chunks.last().unwrap().text.contains("Final line."));
    assert_ne!(
        chunk::chunks("Other title", &body, 256)[0].key,
        chunks[0].key
    );
    let value = json!({"status":"ready","hits":[{"excerpt":"界".repeat(500)},{"excerpt":"a".repeat(500)}],"truncated":false});
    let bounded = bounded(value, 1024).unwrap();
    assert!(bounded.len() <= 1024);
    assert_eq!(
        serde_json::from_str::<Value>(&bounded).unwrap()["truncated"],
        true
    );
}

#[test]
fn filters_apply_to_curated_and_cached_scopes_without_changing_other_services() {
    let f = Fixture::new();
    f.warm();
    let full = f.service(None);
    full.update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let filtered = SearchService::new(
        SearchConfig {
            enabled: true,
            sources: vec![config::SourceRule {
                index: "public-docs".into(),
                include: Some(vec!["database.md".into()]),
                ..Default::default()
            }],
            ..Default::default()
        },
        f.registry.clone(),
        f.cache.clone(),
    )
    .unwrap();
    let report = filtered
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.errors.is_empty());
    let source = filtered.sources().pop().unwrap();
    assert_eq!(filtered.coverage(&source.id).unwrap().indexed_files, 1);
    let (prepared, _) = f.prepare(&filtered, source);
    assert!(
        prepared.execute("squirrels", Mode::Keyword, 5).unwrap()["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (prepared, _) = f.prepare(&full, full.sources().pop().unwrap());
    assert_eq!(
        prepared.execute("squirrels", Mode::Keyword, 5).unwrap()["hits"][0]["file"],
        "guide.md"
    );
    let cache = f.cache.cached_only();
    let level = corpus::level(&cache, &f.registry, "public-docs", &[]).unwrap();
    let (prepared, _) = f.prepare(&filtered, Descriptor::curated("public-docs", &[], &level));
    assert_eq!(prepared.corpus.coverage.indexed_files, 0);
    let config = SearchConfig {
        enabled: true,
        sources: vec![
            config::SourceRule {
                index: "public-docs".into(),
                enabled: Some(false),
                exclude: vec!["secrets/**".into()],
                ..Default::default()
            },
            config::SourceRule {
                index: "public-docs".into(),
                path: vec!["languages".into()],
                enabled: Some(true),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    config.validate().unwrap();
    assert!(!config.for_source("public-docs", &[]).enabled);
    let child = config.for_source("public-docs", &["languages".into(), "nodejs".into()]);
    assert!(child.enabled);
    assert!(child.filters().unwrap().1.is_match("secrets/key.md"));
}

#[test]
fn reports_incomplete_inventory_limits_and_scoped_command_selection() {
    let f = Fixture::new();
    let service = f.service(None);
    let cold = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert_eq!(cold.unavailable_levels, 1);
    assert_eq!(cold.indexed_sources, 0);
    assert!(!f.cache.path_for(&f.origin).exists());
    f.warm();
    let command = service
        .update(
            Some("public-docs"),
            &[],
            Some("command"),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
    assert_eq!(
        command.indexed_sources, 1,
        "a doc anchor must not hide a command's cached repository"
    );
    let limited = SearchService::new(
        SearchConfig {
            enabled: true,
            max_corpus_bytes: 4,
            ..Default::default()
        },
        f.registry.clone(),
        f.cache.clone(),
    )
    .unwrap();
    let report = limited
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(report.partial_sources > 0);
    assert_eq!(report.unavailable_levels, 0);
    let deadline = service.update(None, &[], None, Instant::now()).unwrap();
    assert!(deadline.inventory_limited);
}

#[test]
fn removed_memberships_and_vectors_are_collected_and_queries_have_priority() {
    let f = Fixture::new();
    f.warm();
    let provider = MockProvider::new();
    let service = f.service(Some(provider.config()));
    let priority = lock_file(&service.inner.locks.join("query-priority.lock")).unwrap();
    priority.lock_shared().unwrap();
    let deferred = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(deferred.pending_sources > 0);
    assert_eq!(deferred.embedded_chunks, 0);
    assert!(provider.requests.lock().unwrap().is_empty());
    drop(priority);
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let source = service.sources().pop().unwrap();
    let original = std::fs::read_to_string(f.repo.join("pinakes.yaml")).unwrap();
    std::fs::write(
        f.repo.join("pinakes.yaml"),
        "version: 1\nname: Empty\nentries: []\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "remove"]);
    f.refresh();
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(service
        .inner
        .store
        .descriptor(&source.id)
        .unwrap()
        .is_none());
    let before = provider.requests.lock().unwrap().len();
    std::fs::write(f.repo.join("pinakes.yaml"), original).unwrap();
    git(&f.repo, &["add", "."]);
    git(&f.repo, &["commit", "-m", "restore"]);
    f.refresh();
    let restored = service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    assert!(
        restored.embedded_chunks > 0,
        "removed vectors should have been collected"
    );
    assert!(provider.requests.lock().unwrap().len() > before);
}

#[test]
fn ranked_corpus_accepts_sha256_commit_pins() {
    let mut f = Fixture::with_format("sha256");
    let revision = git(&f.repo, &["rev-parse", "HEAD"]);
    f.origin.branch = None;
    f.origin.commit = Some(revision.clone());
    Arc::make_mut(&mut f.registry)
        .indexes
        .get_mut("public-docs")
        .unwrap()
        .repo = f.origin.clone();
    f.warm();
    let service = f.service(None);
    service
        .update(None, &[], None, Instant::now() + Duration::from_secs(10))
        .unwrap();
    let (prepared, cache) = f.prepare(&service, service.sources().pop().unwrap());
    let value = prepared.execute("Squirrels", Mode::Keyword, 5).unwrap();
    assert_eq!(value["hits"][0]["revision"], revision);
    assert_eq!(revision.len(), 64);
    let snapshot = cache.response_metadata()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    let followup = f.cache.session(
        Freshness::CachedOk,
        Instant::now() + Duration::from_secs(10),
    );
    followup.apply_snapshot(&f.origin, &snapshot).unwrap();
    let (dir, _) = followup
        .ensure_path(&f.origin, RefreshPolicy::Never, "guide.md")
        .unwrap();
    assert_eq!(followup.snapshot_revision(&dir).unwrap(), revision);
    assert!(followup
        .read_file(&dir, "guide.md")
        .unwrap()
        .contains("Squirrels"));
}

#[path = "global_tests.rs"]
mod global_tests;
