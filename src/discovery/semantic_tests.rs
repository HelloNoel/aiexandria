use super::tests::{commit, write, Fixture};
use super::*;
use crate::registry::RefreshSetting;
use crate::search::config::{EmbeddingConfig, Provider};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;

struct ProviderFixture {
    endpoint: String,
    inputs: Arc<Mutex<Vec<String>>>,
    status: Arc<std::sync::atomic::AtomicU16>,
    delay_ms: Arc<AtomicU64>,
    fail_after_documents: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProviderFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
        let delay_ms = Arc::new(AtomicU64::new(0));
        let fail_after_documents = Arc::new(AtomicU64::new(u64::MAX));
        let stop = Arc::new(AtomicBool::new(false));
        let received = inputs.clone();
        let response_status = status.clone();
        let stopping = stop.clone();
        let delay = delay_ms.clone();
        let fail_after = fail_after_documents.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let mut length = 0;
                let mut chunked = false;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    chunked |= lower.starts_with("transfer-encoding:") && lower.contains("chunked");
                }
                let mut body = Vec::new();
                if chunked {
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        let size = usize::from_str_radix(line.trim(), 16).unwrap();
                        if size == 0 {
                            break;
                        }
                        let mut part = vec![0; size + 2];
                        if reader.read_exact(&mut part).is_err() {
                            break;
                        }
                        body.extend_from_slice(&part[..size]);
                    }
                } else {
                    body.resize(length, 0);
                    if reader.read_exact(&mut body).is_err() {
                        continue;
                    }
                }
                let Ok(request) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                let Some(batch) = request["input"].as_array() else {
                    continue;
                };
                let vectors: Vec<_> = batch
                    .iter()
                    .enumerate()
                    .map(|(i, input)| {
                        let input = input.as_str().unwrap();
                        received.lock().unwrap().push(input.into());
                        let lower = input.to_lowercase();
                        let vector = if lower.contains("certificate") || lower.contains("identity")
                        {
                            vec![1.0, 0.0, 0.0]
                        } else if lower.contains("storage") || lower.contains("blob") {
                            vec![0.0, 1.0, 0.0]
                        } else {
                            vec![0.0, 0.0, 1.0]
                        };
                        json!({"index":i,"embedding":vector})
                    })
                    .collect();
                let body = json!({"model":"fixture","data":vectors}).to_string();
                std::thread::sleep(Duration::from_millis(delay.load(Ordering::Acquire)));
                let document_count = received
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|s: &&String| s.starts_with("D: "))
                    .count() as u64;
                let status = if document_count > fail_after.load(Ordering::Acquire) {
                    503
                } else {
                    response_status.load(Ordering::Acquire)
                };
                let _ = write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self {
            endpoint,
            inputs,
            status,
            delay_ms,
            fail_after_documents,
            stop,
            worker: Some(worker),
        }
    }

    fn config(&self) -> SearchConfig {
        SearchConfig {
            enabled: true,
            embedding: Some(EmbeddingConfig {
                provider: Provider::OpenaiCompatible,
                endpoint: self.endpoint.clone(),
                model: "fixture".into(),
                revision: "v1".into(),
                token_env: None,
                allow_remote: false,
                query_prefix: "Q: ".into(),
                document_prefix: "D: ".into(),
                timeout_secs: 3,
                startup: None,
            }),
            ..Default::default()
        }
    }

    fn document_requests(&self) -> usize {
        self.inputs
            .lock()
            .unwrap()
            .iter()
            .filter(|input| input.starts_with("D: "))
            .count()
    }
}

#[tokio::test]
async fn incomplete_vector_coverage_cannot_be_called_successful_hybrid() {
    let provider = ProviderFixture::new();
    provider.fail_after_documents.store(8, Ordering::Release);
    let entries = (0..12).map(|i| format!("  - {{id: cert-{i}, name: Certificate{i}, description: Certificate provisioning, kind: command, argv: [must-not-run]}}\n")).collect::<String>();
    let root = format!("version: 1\nname: Catalog\nentries:\n{entries}");
    let mut fixture = Fixture::new(&[("pinakes.yaml", &root)], false, RefreshSetting::Never);
    fixture.service.configure(&provider.config());
    let strict = settled(
        &fixture.service,
        json!({"query":"certificate","kinds":["command"]}),
    )
    .await;
    assert_eq!(strict["status"], "unavailable", "{strict}");
    assert_eq!(strict["sources"], json!([]));
    assert_eq!(strict["semantic"]["ready_sources"], 8);
    assert_eq!(strict["semantic"]["total_sources"], 12);
    let degraded = settled(&fixture.service,json!({"query":"certificate","kinds":["command"],"fallback":"keyword","limit":20,"max_output_bytes":32768})).await;
    assert_eq!(degraded["status"], "degraded", "{degraded}");
    assert_eq!(degraded["sources"].as_array().unwrap().len(), 12);
    assert_eq!(degraded["warnings"][0]["code"], "provider_busy");
}

#[tokio::test]
async fn metadata_only_update_prepares_discovery_without_document_indexing() {
    let provider = ProviderFixture::new();
    let fixture = Fixture::new(
        &[("pinakes.yaml", CATALOG), ("body.md", "zzbodysecretzz")],
        false,
        RefreshSetting::Never,
    );
    let result = prepare_metadata(
        (*fixture.registry).clone(),
        (*fixture.cache).clone(),
        provider.config(),
        Some("docs".into()),
        true,
        Duration::from_secs(20),
    )
    .await
    .unwrap();
    assert_eq!(result["status"], "ready", "{result}");
    assert_eq!(result["scope"], "source_metadata");
    assert!(provider.document_requests() >= 3);
    assert!(provider
        .inputs
        .lock()
        .unwrap()
        .iter()
        .all(|text| !text.contains("zzbodysecretzz")));
    assert_eq!(fixture.cache.cached_origins().len(), 1);
    for entry in std::fs::read_dir(fixture.cache.root().join("search")).unwrap() {
        let db = entry.unwrap().path().join("search.sqlite");
        if db.is_file() {
            let db = rusqlite::Connection::open(db).unwrap();
            let chunks: i64 = db
                .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
                .unwrap();
            assert_eq!(chunks, 0, "discovery preparation indexed document bodies");
        }
    }
}

#[tokio::test]
async fn cold_discovery_has_bounded_wait_and_explicit_fallback_while_work_continues() {
    let provider = ProviderFixture::new();
    let mut fixture = Fixture::new(&[("pinakes.yaml", CATALOG)], false, RefreshSetting::Never);
    fixture.service.configure(&provider.config());
    settled(
        &fixture.service,
        json!({"query":"certificate","mode":"keyword"}),
    )
    .await;
    provider.delay_ms.store(1800, Ordering::Release);
    let started = Instant::now();
    let pending: Value = serde_json::from_str(
        &fixture
            .service
            .discover(
                serde_json::from_value(json!({"query":"certificate","kinds":["command"]})).unwrap(),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "foreground discovery blocked on indexing"
    );
    assert_eq!(pending["status"], "preparing", "{pending}");
    assert_eq!(pending["sources"], json!([]));
    let fallback: Value = serde_json::from_str(&fixture.service.discover(serde_json::from_value(json!({"query":"certificate","kinds":["command"],"fallback":"keyword","max_output_bytes":2048})).unwrap()).await.unwrap()).unwrap();
    assert_eq!(fallback["status"], "degraded", "{fallback}");
    assert_eq!(fallback["sources"][0]["name"], "PassVault");
    provider.delay_ms.store(0, Ordering::Release);
    let ready = settled(
        &fixture.service,
        json!({"query":"certificate","kinds":["command"]}),
    )
    .await;
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(provider.document_requests(), 2);
}

impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

const CATALOG: &str = r#"version: 1
name: Library
entries:
  - id: credentials
    name: PassVault
    description: X.509 consumer certificate provisioning
    kind: command
    argv: [must-not-run, '{query}']
    repo: {url: 'file:///must-not-acquire-content'}
  - id: storage
    name: Storage
    description: Blob containers
    kind: command
    argv: [must-not-run, '{query}']
  - id: body
    name: Ordinary document
    kind: doc
    path: body.md
"#;

async fn settled(service: &DiscoveryService, request: Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result: Value = serde_json::from_str(
            &service
                .discover(serde_json::from_value(request.clone()).unwrap())
                .await
                .unwrap(),
        )
        .unwrap();
        if result["status"] != "preparing" && result["catalog"]["pending_indexes"] == 0 {
            return result;
        }
        assert!(Instant::now() < deadline, "{result}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn semantic_discovery_routes_paraphrases_without_reading_bodies_or_commands() {
    let provider = ProviderFixture::new();
    let mut fixture = Fixture::new(
        &[("pinakes.yaml", CATALOG), ("body.md", "zzbodysecretzz")],
        false,
        RefreshSetting::Never,
    );
    fixture.service.configure(&provider.config());
    let keyword = settled(
        &fixture.service,
        json!({"query":"issue client identity","mode":"keyword","kinds":["command"]}),
    )
    .await;
    assert_eq!(keyword["sources"], json!([]));
    assert!(provider.inputs.lock().unwrap().is_empty());
    for mode in ["semantic", "hybrid", "auto"] {
        let result = settled(
            &fixture.service,
            json!({"query":"issue client identity","mode":mode,"kinds":["command"]}),
        )
        .await;
        assert_eq!(result["status"], "ready", "{result}");
        assert_eq!(result["sources"][0]["name"], "PassVault");
        assert_eq!(result["sources"][0]["query_profile"], "source_defined");
        assert_eq!(result["semantic"]["total_sources"], 2);
        assert_eq!(
            result["mode_used"],
            if mode == "auto" { "hybrid" } else { mode }
        );
    }
    assert_eq!(provider.document_requests(), 2);
    assert!(provider
        .inputs
        .lock()
        .unwrap()
        .iter()
        .all(|text| !text.contains("zzbodysecretzz")));
    assert_eq!(fixture.cache.cached_origins().len(), 1);
}

#[tokio::test]
async fn metadata_is_reused_across_clients_and_only_changed_records_are_embedded() {
    let provider = ProviderFixture::new();
    let mut fixture = Fixture::new(
        &[("pinakes.yaml", CATALOG)],
        false,
        RefreshSetting::Every(Duration::from_secs(5)),
    );
    let config = provider.config();
    fixture.service.configure(&config);
    let mut other = DiscoveryService::new(fixture.registry.clone(), fixture.cache.clone());
    other.configure(&config);
    let query = json!({"query":"identity","kinds":["command"]});
    let (first, second) = tokio::join!(
        settled(&fixture.service, query.clone()),
        settled(&other, query.clone())
    );
    assert_eq!(first["metadata_snapshot"], second["metadata_snapshot"]);
    assert_eq!(
        provider.document_requests(),
        2,
        "competing clients duplicated metadata work"
    );
    let db = rusqlite::Connection::open(fixture.service.metadata_directory.join("metadata.sqlite"))
        .unwrap();
    db.execute("UPDATE vectors SET used_ms=1", []).unwrap();
    drop(db);
    write(
        fixture.repo.workdir().unwrap(),
        "pinakes.yaml",
        &CATALOG.replace(
            "X.509 consumer certificate provisioning",
            "X.509 certificate renewal and provisioning",
        ),
    );
    commit(&fixture.repo);
    tokio::time::sleep(Duration::from_millis(5100)).await;
    let status = settled(&fixture.service, query).await;
    assert_eq!(status["status"], "ready");
    assert_ne!(status["metadata_snapshot"], first["metadata_snapshot"]);
    assert_eq!(provider.document_requests(), 3);
}

#[tokio::test]
async fn discovery_failures_fallback_opt_out_and_cursor_modes_are_explicit() {
    let provider = ProviderFixture::new();
    let mut fixture = Fixture::new(&[("pinakes.yaml", CATALOG)], false, RefreshSetting::Never);
    fixture.service.configure(&provider.config());
    let first = settled(
        &fixture.service,
        json!({"query":"certificate","kinds":["command"],"limit":1}),
    )
    .await;
    assert_eq!(first["status"], "ready", "{first}");
    let cursor = first["next_cursor"]
        .as_str()
        .unwrap_or_else(|| panic!("{first}"));
    provider.status.store(503, Ordering::Release);
    let count = provider.inputs.lock().unwrap().len();
    let second = settled(&fixture.service, json!({"cursor":cursor})).await;
    assert_eq!(second["metadata_snapshot"], first["metadata_snapshot"]);
    assert_eq!(second["mode_used"], "hybrid");
    assert_eq!(provider.inputs.lock().unwrap().len(), count);
    assert!(fixture
        .service
        .discover(serde_json::from_value(json!({"cursor":cursor,"mode":"keyword"})).unwrap())
        .await
        .is_err());
    let strict = settled(
        &fixture.service,
        json!({"query":"certificate","kinds":["command"]}),
    )
    .await;
    assert_eq!(strict["status"], "unavailable");
    assert_eq!(strict["error"]["code"], "provider_busy");
    assert_eq!(strict["sources"], json!([]));
    assert!(strict["mode_used"].is_null());
    let fallback = settled(
        &fixture.service,
        json!({"query":"certificate","kinds":["command"],"fallback":"keyword"}),
    )
    .await;
    assert_eq!(fallback["status"], "degraded");
    assert_eq!(fallback["mode_used"], "keyword");
    assert_eq!(fallback["sources"][0]["name"], "PassVault");
    assert_eq!(fallback["warnings"][0]["code"], "provider_busy");
    let mut config = provider.config();
    config.discovery.default_mode = Mode::Keyword;
    fixture.service.configure(&config);
    let count = provider.inputs.lock().unwrap().len();
    let result = settled(&fixture.service, json!({"query":"certificate"})).await;
    assert_eq!(result["mode_used"], "keyword");
    assert_eq!(provider.inputs.lock().unwrap().len(), count);
}

#[tokio::test]
async fn no_provider_keeps_default_discovery_keyword_and_explicit_hybrid_fails() {
    let fixture = Fixture::new(&[("pinakes.yaml", CATALOG)], false, RefreshSetting::Never);
    let default = settled(&fixture.service, json!({"query":"certificate"})).await;
    assert_eq!(default["mode_used"], "keyword");
    assert_eq!(default["semantic"]["status"], "not_configured");
    let hybrid = settled(
        &fixture.service,
        json!({"query":"certificate","mode":"hybrid"}),
    )
    .await;
    assert_eq!(hybrid["status"], "unavailable");
    assert_eq!(hybrid["sources"], json!([]));
    assert_eq!(hybrid["error"]["code"], "embedding_not_configured");
}

#[test]
fn metadata_template_and_fusion_preserve_topic_and_exact_name() {
    let make = |name: &str, description: &str| {
        Arc::new(Source::new(
            SourceKind::Command,
            SourceRoute {
                index: "docs".into(),
                path: vec![],
                command_id: Some(name.into()),
            },
            name,
            description,
            "parent",
            "",
        ))
    };
    let exact = make("Storage", &"資料🦀".repeat(2000));
    assert!(exact.metadata_truncated);
    assert!(exact.embedding_text.len() <= 2000);
    let decoy = make("Credentials", "certificate");
    let fused = metadata::fuse(vec![exact.clone()], vec![decoy, exact.clone()], "Storage");
    assert_eq!(fused[0].id, exact.id);
}
