//! Shared local provider ownership. Neither a PID file nor a request-local mutex
//! is sufficient: different MCP clients and CLI processes race to start models.
#[cfg(windows)]
mod windows;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::config::{EmbeddingConfig, SearchConfig};
use super::provider::{diagnostic, failure};
use crate::cache::{atomic_json, now_ms, try_lock};
use crate::error::{AIexError, Result};

const POLL: Duration = Duration::from_millis(100);
const PROBE: Duration = Duration::from_secs(1);
const FAILURE_BACKOFF_MS: u64 = 30_000;

#[derive(Serialize, Deserialize)]
struct Record {
    instance: String,
    contract: String,
    status: String,
    changed_ms: u64,
    error: Option<Value>,
}

fn contract(config: &EmbeddingConfig) -> String {
    super::chunk::digest(
        serde_json::to_vec(&(&config.provider, &config.model, &config.revision)).unwrap(),
    )
}

fn directory(config: &EmbeddingConfig) -> Result<PathBuf> {
    let url = url::Url::parse(&config.endpoint).map_err(|_| {
        failure(
            "invalid_configuration",
            "Invalid embedding endpoint.",
            false,
        )
    })?;
    // Share admission for localhost, 127.0.0.1 and ::1, and different API paths.
    // Two configurations of one listening port cannot each own a model.
    let host = url.host_str().unwrap_or("");
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    let key = super::chunk::digest(format!(
        "{}:{}",
        if loopback { "loopback" } else { host },
        url.port_or_known_default().unwrap_or(0)
    ));
    let root = std::env::var_os("AIEXANDRIA_EMBEDDING_STATE")
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|p| p.join("aiexandria/providers")))
        .ok_or_else(|| {
            failure(
                "invalid_configuration",
                "No local provider state directory is available.",
                false,
            )
        })?;
    let path = root.join(key);
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

fn record(dir: &Path) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(dir.join("state.json")).ok()?).ok()
}

fn pending() -> AIexError {
    failure(
        "provider_starting",
        "The embedding provider is starting; no search ran. Retry after startup completes.",
        true,
    )
}

fn recorded_error(state: &Record) -> AIexError {
    let error = state.error.as_ref();
    failure(
        error
            .and_then(|e| e["code"].as_str())
            .unwrap_or("provider_start_failed"),
        error.and_then(|e| e["message"].as_str()).unwrap_or(
            "The embedding provider failed to start; check embedding status and its log.",
        ),
        error.and_then(|e| e["retryable"].as_bool()).unwrap_or(true),
    )
}

fn check_contract(dir: &Path, config: &EmbeddingConfig) -> Result<()> {
    if try_lock(&dir.join("owner.lock"))?.is_none()
        && record(dir).is_some_and(|r| r.contract != contract(config))
    {
        return Err(failure("provider_configuration_conflict", "This local port is already owned by a different model or revision. Stop that managed provider before changing models.", false));
    }
    Ok(())
}

/// A caller's deadline bounds its wait, not the shared startup job's lifetime.
pub fn ensure_started(
    config: &EmbeddingConfig,
    deadline: Instant,
    mut explicit: bool,
) -> Result<()> {
    SearchConfig {
        enabled: true,
        embedding: Some(config.clone()),
        ..Default::default()
    }
    .validate()?;
    let Some(startup) = &config.startup else {
        return config
            .probe(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(PROBE),
            )
            .map(|_| ());
    };
    let dir = directory(config)?;
    loop {
        if Instant::now() >= deadline {
            return Err(pending());
        }
        check_contract(&dir, config)?;
        match config.probe(
            deadline
                .saturating_duration_since(Instant::now())
                .min(PROBE),
        ) {
            Ok(_) => return Ok(()),
            Err(AIexError::Embedding { ref code, .. }) if code == "provider_unreachable" => {}
            Err(error) => {
                // Do not launch a second process just because a live server is
                // loading, busy, misconfigured or rejecting authentication.
                if try_lock(&dir.join("owner.lock"))?.is_some() {
                    return Err(error);
                }
            }
        }
        let Some(_admission) = try_lock(&dir.join("start.lock"))? else {
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            continue;
        };
        check_contract(&dir, config)?;
        if try_lock(&dir.join("owner.lock"))?.is_none() {
            if let Some(state) = record(&dir) {
                if state.status == "failed" {
                    return Err(recorded_error(&state));
                }
            }
            drop(_admission);
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
            continue;
        }
        // Another caller may have finished while we were acquiring admission.
        match config.probe(
            deadline
                .saturating_duration_since(Instant::now())
                .min(PROBE),
        ) {
            Ok(_) => return Ok(()),
            Err(AIexError::Embedding { ref code, .. }) if code == "provider_unreachable" => {}
            Err(error) => return Err(error),
        }
        if !explicit {
            if let Some(state) = record(&dir) {
                if state.status == "failed"
                    && now_ms().saturating_sub(state.changed_ms) < FAILURE_BACKOFF_MS
                {
                    return Err(recorded_error(&state));
                }
            }
        }
        atomic_json(&dir.join("request.json"), config)?;
        // The executable receives no secret values or user queries on argv.
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("__embedding-supervisor")
            .arg(dir.join("request.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A different session survives a terminal or individual MCP exit.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000 | 0x00000200); // no window, new process group
        }
        let mut supervisor = command.spawn().map_err(|_| {
            failure(
                "provider_start_failed",
                "Could not launch the AIexandria embedding supervisor.",
                true,
            )
        })?;
        // Keep admission until the supervisor owns the lifetime lock. Otherwise
        // a second caller could spawn another supervisor in the handoff gap.
        let handoff = Instant::now() + Duration::from_secs(startup.timeout_secs.min(10));
        while try_lock(&dir.join("owner.lock"))?.is_some() {
            if supervisor.try_wait()?.is_some() {
                return Err(record(&dir).map(|s| recorded_error(&s)).unwrap_or_else(|| {
                    failure(
                        "provider_start_failed",
                        "Embedding supervisor exited before acquiring ownership.",
                        true,
                    )
                }));
            }
            if Instant::now() >= handoff {
                let _ = supervisor.kill();
                let _ = supervisor.wait();
                return Err(failure(
                    "provider_start_failed",
                    "Embedding supervisor did not acquire ownership before its deadline.",
                    true,
                ));
            }
            std::thread::sleep(POLL);
        }
        std::thread::spawn(move || {
            let _ = supervisor.wait();
        });
        // Override an earlier failure once; never repeatedly restart a crashing
        // child inside one explicit start request.
        explicit = false;
        drop(_admission);
    }
}

/// Read ownership progress without an HTTP probe on a bounded discovery path.
pub(crate) fn is_starting(config: &EmbeddingConfig) -> bool {
    if config.startup.is_none() {
        return false;
    }
    directory(config).ok().is_some_and(|dir| {
        try_lock(&dir.join("owner.lock")).is_ok_and(|lock| lock.is_none())
            && record(&dir).is_some_and(|state| state.status == "starting")
    })
}

pub fn status(config: &EmbeddingConfig) -> Result<Value> {
    let dir = directory(config)?;
    let owned = try_lock(&dir.join("owner.lock"))?.is_none();
    let state = record(&dir);
    let probe = check_contract(&dir, config).and_then(|()| config.probe(PROBE));
    Ok(json!({
        "status": if probe.is_ok() { "ready" } else if owned && state.as_ref().is_some_and(|r| r.status == "starting") { "starting" } else { "unavailable" },
        "managed": owned,
        "endpoint": config.endpoint,
        "model": config.model,
        "dimensions": probe.as_ref().ok(),
        "error": probe.as_ref().err().map(diagnostic),
        "startup": state,
        "log": dir.join("provider.log"),
    }))
}

pub fn stop(config: &EmbeddingConfig) -> Result<Value> {
    let dir = directory(config)?;
    let admission_deadline = Instant::now() + Duration::from_secs(10);
    let _admission = loop {
        if let Some(lock) = try_lock(&dir.join("start.lock"))? {
            break lock;
        }
        if Instant::now() >= admission_deadline {
            return Err(failure(
                "provider_control_timeout",
                "Timed out waiting for a concurrent provider start request.",
                true,
            ));
        }
        std::thread::sleep(POLL);
    };
    if try_lock(&dir.join("owner.lock"))?.is_some() {
        return Ok(
            json!({"status":"not_managed","message":"No AIexandria-owned provider is running; external services were not stopped."}),
        );
    }
    let state = record(&dir).ok_or_else(|| {
        failure(
            "provider_control_unavailable",
            "The provider owner has not published its state; retry shortly.",
            true,
        )
    })?;
    // Only the owning supervisor interprets this nonce. Never signal a saved PID.
    atomic_json(&dir.join("stop.json"), &state.instance)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while try_lock(&dir.join("owner.lock"))?.is_none() {
        if Instant::now() >= deadline {
            return Err(failure(
                "provider_control_timeout",
                "The managed provider did not stop before the deadline.",
                true,
            ));
        }
        std::thread::sleep(POLL);
    }
    Ok(json!({"status":"stopped"}))
}

/// Internal CLI entry point. Its lifetime, not a request's, owns the process.
pub fn supervise(request: &Path) -> Result<()> {
    let config: EmbeddingConfig =
        serde_json::from_slice(&std::fs::read(request)?).map_err(|_| {
            failure(
                "invalid_configuration",
                "Invalid provider startup request.",
                false,
            )
        })?;
    SearchConfig {
        enabled: true,
        embedding: Some(config.clone()),
        ..Default::default()
    }
    .validate()?;
    let dir = directory(&config)?;
    if request != dir.join("request.json") {
        return Err(failure(
            "invalid_configuration",
            "Provider startup request path does not match its endpoint.",
            false,
        ));
    }
    let Some(owner) = try_lock(&dir.join("owner.lock"))? else {
        return Ok(());
    };
    let mut state = Record {
        instance: super::chunk::digest(format!(
            "{}:{}:{:?}",
            std::process::id(),
            now_ms(),
            Instant::now()
        )),
        contract: contract(&config),
        status: "starting".into(),
        changed_ms: now_ms(),
        error: None,
    };
    atomic_json(&dir.join("state.json"), &state)?;
    let result = run_provider(&config, &dir, &owner, &mut state);
    state.status = if result.is_ok() { "stopped" } else { "failed" }.into();
    state.error = result.as_ref().err().map(diagnostic);
    state.changed_ms = now_ms();
    atomic_json(&dir.join("state.json"), &state)?;
    result
}

fn run_provider(
    config: &EmbeddingConfig,
    dir: &Path,
    owner: &File,
    state: &mut Record,
) -> Result<()> {
    let startup = config.startup.as_ref().ok_or_else(|| {
        failure(
            "invalid_configuration",
            "No embedding.startup command is configured.",
            false,
        )
    })?;
    // Recheck inside the lifetime lock as well. An external server can appear
    // after the caller's probe; never deliberately compete with a live port.
    match config.probe(PROBE) {
        Ok(_) => return Ok(()),
        Err(AIexError::Embedding { ref code, .. }) if code == "provider_unreachable" => {}
        Err(error) => return Err(error),
    }
    let stdin = owner.try_clone()?;
    let mut command = process_wrap::std::CommandWrap::with_new(&startup.argv[0], |cmd| {
        cmd.args(&startup.argv[1..])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(home) = dirs::home_dir() {
            cmd.current_dir(home);
        }
    });
    // On Unix the empty stdin file also retains the lifetime flock across exec,
    // so a supervisor crash cannot admit a second model while this one lives.
    // Windows job ownership kills the process tree if the supervisor exits.
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    let mut child = command.spawn().map_err(|_| {
        failure(
            "provider_start_failed",
            "Could not execute embedding.startup.argv; check the executable and permissions.",
            false,
        )
    })?;
    #[cfg(windows)]
    let _kill_job = match windows::KillJob::attach(&*child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait();
            return Err(failure(
                "provider_start_failed",
                &format!("Could not establish crash-safe Windows process ownership: {error}"),
                false,
            ));
        }
    };
    let log = Arc::new(Mutex::new(RotatingLog {
        path: dir.join("provider.log"),
    }));
    let stdout = child.stdout().take().unwrap();
    let stderr = child.stderr().take().unwrap();
    drain(stdout, log.clone());
    drain(stderr, log);
    let deadline = Instant::now() + Duration::from_secs(startup.timeout_secs);
    let result = (|| loop {
        if child.try_wait()?.is_some() {
            return Err(failure(
                "provider_exited",
                "The embedding process exited; inspect the provider log and startup command.",
                true,
            ));
        }
        if std::fs::read(dir.join("stop.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<String>(&b).ok())
            .as_ref()
            == Some(&state.instance)
        {
            return Ok(());
        }
        if state.status == "starting" {
            if Instant::now() >= deadline {
                return Err(failure("provider_start_timeout", "The embedding API did not become ready within startup.timeout_secs; inspect the provider log.", true));
            }
            match config.probe(PROBE.min(deadline.saturating_duration_since(Instant::now()))) {
                Ok(_) => {
                    state.status = "ready".into();
                    state.changed_ms = now_ms();
                    atomic_json(&dir.join("state.json"), state)?;
                }
                Err(AIexError::Embedding { ref code, .. })
                    if matches!(
                        code.as_str(),
                        "provider_unreachable" | "provider_busy" | "timeout"
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        std::thread::sleep(POLL);
    })();
    let _ = child.start_kill();
    let _ = child.wait();
    result
}

struct RotatingLog {
    path: PathBuf,
}
impl RotatingLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= 1024 * 1024) {
            let previous = self.path.with_extension("log.1");
            if previous.exists() {
                std::fs::remove_file(&previous)?;
            }
            std::fs::rename(&self.path, previous)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&self.path)?.write_all(bytes)
    }
}

fn drain(mut pipe: impl Read + Send + 'static, log: Arc<Mutex<RotatingLog>>) {
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = pipe.read(&mut buffer) {
            if count == 0 {
                break;
            }
            if let Ok(mut log) = log.lock() {
                let _ = log.write(&buffer[..count]);
            }
        }
    });
}
