//! Real cross-process tests: CLI instances race to start one supervised model.
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_aiexandria");

struct Fixture {
    temp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    endpoint: String,
}

impl Fixture {
    fn new(delay_ms: u64) -> Self {
        Self::with_listener(delay_ms).0
    }

    fn with_listener(delay_ms: u64) -> (Self, TcpListener) {
        let temp = tempfile::tempdir().unwrap();
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}/v1/embeddings");
        let state = temp.path().join("state");
        let config = temp.path().join("search.yaml");
        std::fs::write(
            temp.path().join("fixture.json"),
            json!({"port":port,"delay_ms":delay_ms}).to_string(),
        )
        .unwrap();
        let yaml = json!({"enabled":true,"embedding":{
            "provider":"openai_compatible","endpoint":endpoint,"model":"fixture","revision":"v1","timeout_secs":10,
            "startup":{"argv":[std::env::current_exe().unwrap(),"--exact","mock_provider_process","--nocapture"],"timeout_secs":20}
        }});
        std::fs::write(&config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
        (
            Self {
                temp,
                config,
                state,
                endpoint,
            },
            reservation,
        )
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .arg("--search-config")
            .arg(&self.config)
            .env("AIEXANDRIA_EMBEDDING_STATE", &self.state)
            .env("AIEXANDRIA_CACHE", self.temp.path().join("cache"))
            .env("AIEXANDRIA_PROVIDER_FIXTURE", self.temp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn cli(&self, action: &str) -> Output {
        wait(
            self.command().args(["embedding", action]).spawn().unwrap(),
            Duration::from_secs(30),
        )
    }

    fn started(&self) -> usize {
        std::fs::read_to_string(self.temp.path().join("starts.txt"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn status(&self) -> Value {
        success(self.cli("status"))
    }

    fn probe(&self) -> Value {
        ureq::post(&self.endpoint)
            .send_json(json!({"model":"fixture","input":["test"]}))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli("stop");
    }
}

fn wait(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // A detached process retaining an inherited pipe writer can keep output
    // collection blocked even after the CLI exits. Bound that wait as well.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = rx
        .recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1)),
        )
        .expect("CLI output pipes remained open after process exit")
        .unwrap();
    assert!(
        !timed_out,
        "CLI timed out: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn concurrent_cli_starts_share_one_provider_and_survive_client_exit() {
    let fixture = Fixture::new(600);
    let callers: Vec<_> = (0..6)
        .map(|i| {
            fixture
                .command()
                .args(["embedding", "start"])
                // Different repo caches must still share provider ownership.
                .arg("--cache")
                .arg(fixture.temp.path().join(format!("cache-{i}")))
                .spawn()
                .unwrap()
        })
        .collect();
    for caller in callers {
        let result = success(wait(caller, Duration::from_secs(30)));
        assert_eq!(result["status"], "ready");
        assert_eq!(result["managed"], true);
    }
    assert_eq!(
        fixture.started(),
        1,
        "concurrent requests launched duplicate models"
    );
    assert_eq!(fixture.probe()["model"], "fixture");
    assert_eq!(success(fixture.cli("start"))["status"], "ready");
    assert_eq!(fixture.started(), 1);
    let status = fixture.status();
    let log = Path::new(status["log"].as_str().unwrap());
    assert!(std::fs::metadata(log).unwrap().len() <= 1024 * 1024 + 4096);
    assert!(
        log.with_extension("log.1").exists(),
        "provider logs must rotate"
    );
    assert_eq!(success(fixture.cli("stop"))["status"], "stopped");
    assert_eq!(fixture.status()["managed"], false);

    // Simulate leftover state with an unrelated PID. No signal is ever sent
    // using this field, and an unlocked stale record does not block startup.
    let state_path = log.parent().unwrap().join("state.json");
    let mut stale: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    stale["status"] = json!("ready");
    stale["pid"] = json!(std::process::id());
    std::fs::write(state_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert_eq!(success(fixture.cli("start"))["status"], "ready");
    assert_eq!(fixture.started(), 2);
}

#[test]
fn existing_external_provider_is_reused_and_never_stopped() {
    let fixture = Fixture::new(0);
    let mut external = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mock_provider_process", "--nocapture"])
        .env("AIEXANDRIA_PROVIDER_FIXTURE", fixture.temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if fixture.status()["status"] == "ready" {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(success(fixture.cli("start"))["managed"], false);
        assert_eq!(success(fixture.cli("stop"))["status"], "not_managed");
        assert_eq!(fixture.probe()["model"], "fixture");
        assert_eq!(fixture.started(), 1);
    });
    let _ = external.kill();
    let _ = external.wait();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[test]
fn unresponsive_external_provider_is_not_replaced() {
    // Keep the port open without responding to HTTP. A timeout is not evidence
    // that the port is unused and must never authorize launching another model.
    let (fixture, _listener) = Fixture::with_listener(0);
    let result = fixture.cli("start");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("exceeded the request timeout"), "{stderr}");
    assert_eq!(fixture.started(), 0);
}

#[test]
fn failed_startup_is_reported_and_releases_ownership() {
    let fixture = Fixture::new(0);
    let mut config: Value =
        serde_yaml::from_str(&std::fs::read_to_string(&fixture.config).unwrap()).unwrap();
    config["embedding"]["startup"]["argv"] =
        json!([fixture.temp.path().join("nonexistent-model-executable")]);
    std::fs::write(&fixture.config, serde_yaml::to_string(&config).unwrap()).unwrap();
    let failed = fixture.cli("start");
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("execute embedding.startup.argv"),
        "{stderr}"
    );
    let status = fixture.status();
    assert_eq!(status["managed"], false);
    assert_eq!(status["startup"]["status"], "failed");
    assert_eq!(status["startup"]["error"]["code"], "provider_start_failed");
    assert_eq!(fixture.started(), 0);
}

#[test]
fn conflicting_model_configuration_cannot_spawn_a_second_model() {
    let fixture = Fixture::new(0);
    success(fixture.cli("start"));
    let mut config: Value =
        serde_yaml::from_str(&std::fs::read_to_string(&fixture.config).unwrap()).unwrap();
    config["embedding"]["revision"] = json!("different-weights");
    let conflicting = fixture.temp.path().join("conflicting.yaml");
    std::fs::write(&conflicting, serde_yaml::to_string(&config).unwrap()).unwrap();
    let result = wait(
        Command::new(BIN)
            .arg("--search-config")
            .arg(conflicting)
            .env("AIEXANDRIA_EMBEDDING_STATE", &fixture.state)
            .args(["embedding", "start"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(30),
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("different model or revision"));
    assert_eq!(fixture.started(), 1);
    // Operators can stop the owned endpoint after editing model settings or
    // disabling search; stopping does not require the old model configuration.
    config["enabled"] = json!(false);
    std::fs::write(&fixture.config, serde_yaml::to_string(&config).unwrap()).unwrap();
    assert_eq!(success(fixture.cli("stop"))["status"], "stopped");
}

struct Mcp {
    child: Child,
    input: std::process::ChildStdin,
    responses: std::sync::mpsc::Receiver<Value>,
    id: u64,
}

#[test]
fn cancelling_a_start_request_does_not_cancel_shared_startup() {
    let fixture = Fixture::new(800);
    let mut first = fixture
        .command()
        .args(["embedding", "start"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.started() == 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    first.kill().unwrap();
    first.wait().unwrap();
    assert_eq!(success(fixture.cli("start"))["status"], "ready");
    assert_eq!(fixture.started(), 1);
}

#[test]
fn startup_timeout_and_child_exit_release_lifetime_lock() {
    for exit in [false, true] {
        let fixture = Fixture::new(10_000);
        let path = fixture.temp.path().join("fixture.json");
        let mut model: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        model["exit"] = json!(exit);
        std::fs::write(&path, serde_json::to_vec(&model).unwrap()).unwrap();
        let mut config: Value =
            serde_yaml::from_str(&std::fs::read_to_string(&fixture.config).unwrap()).unwrap();
        config["embedding"]["startup"]["timeout_secs"] = json!(if exit { 20 } else { 3 });
        std::fs::write(&fixture.config, serde_yaml::to_string(&config).unwrap()).unwrap();
        assert!(!fixture.cli("start").status.success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = fixture.status();
            if status["managed"] == false && status["startup"]["status"] == "failed" {
                assert_eq!(
                    status["startup"]["error"]["code"],
                    if exit {
                        "provider_exited"
                    } else {
                        "provider_start_timeout"
                    }
                );
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            fixture.started() <= 1,
            "a failed child must not restart repeatedly"
        );
    }
}

impl Mcp {
    fn new(fixture: &Fixture, registry: &Path) -> Self {
        let mut child = fixture
            .command()
            .arg("--registry")
            .arg(registry)
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, responses) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(response) = serde_json::from_str(&line) {
                    if tx.send(response).is_err() {
                        break;
                    }
                }
            }
        });
        let mut peer = Self {
            child,
            input,
            responses,
            id: 0,
        };
        peer.rpc("initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"embedding-test","version":"1"}}));
        writeln!(
            peer.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})
        )
        .unwrap();
        peer.input.flush().unwrap();
        peer
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let value = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("MCP response deadline");
            if value["id"] != self.id {
                continue;
            }
            assert!(value.get("error").is_none(), "{value}");
            return value["result"].clone();
        }
    }

    fn search(&mut self, mode: &str, fallback: &str) -> Value {
        self.rpc(
            "tools/call",
            json!({"name":"search_docs","arguments":{
                "index":"docs","query":"certificate provisioning", "mode":mode,"fallback":fallback
            }}),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn ranked_value(result: &Value) -> &Value {
    let structured = &result["structuredContent"];
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        structured, &text,
        "text-only and structured-only clients must see the same result"
    );
    structured
}

#[test]
fn mcp_demand_starts_provider_and_failures_are_visible_in_both_representations() {
    let fixture = Fixture::new(300);
    let repo = fixture.temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("pinakes.yaml"), "version: 1\nname: Certificates\nentries:\n- {id: guide, name: Certificate provisioning, kind: doc, path: guide.md}\n").unwrap();
    std::fs::write(
        repo.join("guide.md"),
        "# Certificate provisioning\nConsumers provision certificates with the platform CLI.\n",
    )
    .unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["add", "."],
        vec![
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        let result = Command::new("git")
            .current_dir(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let registry = fixture.temp.path().join("registry.yaml");
    let config = json!({"indexes":{"docs":{"repo":{"url":url::Url::from_directory_path(&repo).unwrap().as_str(),"branch":"main"},"path":"pinakes.yaml","refresh":"never"}}});
    std::fs::write(&registry, serde_yaml::to_string(&config).unwrap()).unwrap();
    let update = fixture
        .command()
        .arg("--registry")
        .arg(&registry)
        .args(["update", "--all", "--wait", "--json"])
        .output()
        .unwrap();
    success(update);
    let mut first = Mcp::new(&fixture, &registry);
    let mut second = Mcp::new(&fixture, &registry);
    first.rpc("tools/list", json!({}));
    first.rpc(
        "tools/call",
        json!({"name":"discover_sources","arguments":{"query":"Certificates","mode":"keyword"}}),
    );
    assert_eq!(
        ranked_value(&first.search("keyword", "none"))["status"],
        "ready"
    );
    assert_eq!(
        fixture.started(),
        0,
        "listing, keyword discovery and keyword search must not load a model"
    );
    // Discovery and document search share the same endpoint owner.
    first.rpc(
        "tools/call",
        json!({"name":"discover_sources","arguments":{"query":"Certificates"}}),
    );
    first.search("auto", "none");
    second.search("hybrid", "none");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = first.search("auto", "none");
        let value = ranked_value(&result);
        if value["status"] == "ready" {
            assert_eq!(value["mode_requested"], "auto");
            assert_eq!(value["mode_resolved"], "hybrid");
            assert_eq!(value["mode_used"], "hybrid");
            assert!(!value["hits"].as_array().unwrap().is_empty());
            break;
        }
        assert!(Instant::now() < deadline, "{value}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(fixture.started(), 1);
    let discovery = first.rpc(
        "tools/call",
        json!({"name":"discover_sources","arguments":{"query":"Certificates"}}),
    );
    assert_eq!(ranked_value(&discovery)["mode_used"], "hybrid");
    drop(first);
    assert_eq!(
        ranked_value(&second.search("semantic", "none"))["mode_used"],
        "semantic"
    );
    drop(second);
    success(fixture.cli("stop"));
    let mut config: Value =
        serde_yaml::from_str(&std::fs::read_to_string(&fixture.config).unwrap()).unwrap();
    config["embedding"]
        .as_object_mut()
        .unwrap()
        .remove("startup");
    std::fs::write(&fixture.config, serde_yaml::to_string(&config).unwrap()).unwrap();
    let mut offline = Mcp::new(&fixture, &registry);
    for fallback in ["none", "keyword"] {
        let deadline = Instant::now() + Duration::from_secs(20);
        let response = loop {
            let response = offline.rpc(
                "tools/call",
                json!({"name":"discover_sources","arguments":{
                    "query":"Certificates","mode":"hybrid","fallback":fallback
                }}),
            );
            let value = ranked_value(&response);
            if value["status"] != "preparing" {
                break response;
            }
            assert!(Instant::now() < deadline, "{value}");
            std::thread::sleep(Duration::from_millis(100));
        };
        let value = ranked_value(&response);
        assert_eq!(response["isError"], fallback == "none");
        assert_eq!(
            value["status"],
            if fallback == "none" {
                "unavailable"
            } else {
                "degraded"
            }
        );
    }
    for mode in ["auto", "hybrid", "semantic"] {
        let result = offline.search(mode, "none");
        assert_eq!(result["isError"], true);
        let value = ranked_value(&result);
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["error"]["code"], "provider_unreachable");
        assert!(value.get("hits").is_none());
        assert!(value["mode_used"].is_null());
    }
    let result = offline.search("hybrid", "keyword");
    assert_eq!(result["isError"], false);
    let value = ranked_value(&result);
    assert_eq!(value["status"], "degraded");
    assert_eq!(value["mode_used"], "keyword");
    assert_eq!(value["warnings"][0]["code"], "provider_unreachable");
    assert!(!value["hits"].as_array().unwrap().is_empty());
    for fallback in ["none", "keyword"] {
        let result = offline.rpc(
            "tools/call",
            json!({"name":"search_docs","arguments":{
                "query":"certificate provisioning", "mode":"hybrid", "fallback":fallback
            }}),
        );
        let value = ranked_value(&result);
        assert_eq!(value["scope"], "global");
        assert_eq!(result["isError"], fallback == "none");
        assert_eq!(
            value["status"],
            if fallback == "none" {
                "unavailable"
            } else {
                "degraded"
            }
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_supervisor_crash_reaps_the_owned_model() {
    let fixture = Fixture::new(0);
    let started = success(fixture.cli("start"));
    let directory = Path::new(started["log"].as_str().unwrap())
        .parent()
        .unwrap();
    success(fixture.cli("stop"));
    let mut supervisor = Command::new(BIN)
        .arg("__embedding-supervisor")
        .arg(directory.join("request.json"))
        .env("AIEXANDRIA_EMBEDDING_STATE", &fixture.state)
        .env("AIEXANDRIA_PROVIDER_FIXTURE", fixture.temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while fixture.status()["status"] != "ready" {
        if Instant::now() >= deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            panic!("direct supervisor did not start");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    supervisor.kill().unwrap();
    supervisor.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fixture.status();
        if status["managed"] == false && status["status"] == "unavailable" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor crash left a running model: {status}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(success(fixture.cli("start"))["status"], "ready");
    assert_eq!(fixture.started(), 3);
}

// The integration test executable doubles as a portable model-process fixture.
// It is a no-op during ordinary cargo test, and only the dedicated child runs it.
#[test]
fn mock_provider_process() {
    let Some(root) = std::env::var_os("AIEXANDRIA_PROVIDER_FIXTURE").map(PathBuf::from) else {
        return;
    };
    let config: Value =
        serde_json::from_slice(&std::fs::read(root.join("fixture.json")).unwrap()).unwrap();
    let mut started = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("starts.txt"))
        .unwrap();
    writeln!(started, "{}", std::process::id()).unwrap();
    if config["exit"] == true {
        return;
    }
    // Fill both pipes before binding. A supervisor that does not drain logs
    // concurrently would deadlock startup, and an unbounded logger grows forever.
    let noise = vec![b'x'; 600 * 1024];
    for _ in 0..2 {
        std::io::stdout().write_all(&noise).unwrap();
    }
    std::io::stderr().write_all(&noise).unwrap();
    std::thread::sleep(Duration::from_millis(config["delay_ms"].as_u64().unwrap()));
    let listener =
        TcpListener::bind(("127.0.0.1", config["port"].as_u64().unwrap() as u16)).unwrap();
    for stream in listener.incoming() {
        let mut stream = stream.unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut length = 0;
        let mut chunked = false;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            if line.to_lowercase().starts_with("transfer-encoding:")
                && line.to_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
        let mut bytes = Vec::new();
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
                let mut chunk = vec![0; size + 2];
                if reader.read_exact(&mut chunk).is_err() {
                    break;
                }
                bytes.extend_from_slice(&chunk[..size]);
            }
        } else {
            bytes.resize(length, 0);
            if reader.read_exact(&mut bytes).is_err() {
                continue;
            }
        }
        let Ok(request) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(inputs) = request["input"].as_array() else {
            continue;
        };
        let data: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(i, _)| json!({"index":i,"embedding":[1.0,0.0]}))
            .collect();
        let body = json!({"model":"fixture","data":data}).to_string();
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    }
}
