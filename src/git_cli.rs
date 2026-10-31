//! Bounded, noninteractive Git processes. Repository credentials never enter argv.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::auth::AuthConfig;
use crate::error::{AIexError, Result};
use crate::index::GitRef;

#[derive(Clone, Debug)]
pub struct GitCli {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub(crate) cancelled: Arc<AtomicBool>,
    active: Arc<AtomicUsize>,
    workers: Arc<AtomicUsize>,
    pub(crate) held_locks: Vec<Arc<std::fs::File>>,
}

impl Default for GitCli {
    fn default() -> Self {
        Self {
            executable: std::env::var_os("AIEXANDRIA_GIT")
                .map(PathBuf::from)
                .unwrap_or_else(|| "git".into()),
            timeout: Duration::from_secs(120),
            cancelled: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicUsize::new(0)),
            workers: Arc::new(AtomicUsize::new(0)),
            held_locks: vec![],
        }
    }
}

impl GitCli {
    pub(crate) fn independent(&self) -> Self {
        Self {
            executable: self.executable.clone(),
            timeout: self.timeout,
            ..Self::default()
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let end = Instant::now() + Duration::from_secs(2);
        while (self.active.load(Ordering::SeqCst) > 0 || self.workers.load(Ordering::SeqCst) > 0)
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub(crate) fn worker(&self) -> Worker {
        self.workers.fetch_add(1, Ordering::SeqCst);
        Worker(self.workers.clone())
    }
    pub fn check(&self) -> Result<String> {
        let text = self.run(
            None,
            &["--version".into()],
            None,
            &AuthConfig::default(),
            None,
            Instant::now() + self.timeout,
        )?;
        let version = text.trim().strip_prefix("git version ").unwrap_or("");
        let parts: Vec<u32> = version
            .split('.')
            .take(3)
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        if parts.len() < 3 || (parts[0], parts[1], parts[2]) < (2, 43, 7) {
            return Err(AIexError::Git("Git 2.43.7 or newer is required; install Git and check the MCP client's PATH or --git setting".into()));
        }
        Ok(text.trim().into())
    }

    pub fn run(
        &self,
        dir: Option<&Path>,
        args: &[String],
        origin: Option<&GitRef>,
        auth: &AuthConfig,
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<String> {
        let bytes = self.run_bytes(dir, args, origin, auth, input, deadline)?;
        String::from_utf8(bytes).map_err(|_| AIexError::Git("Git returned non-UTF-8 output".into()))
    }

    pub fn run_bytes(
        &self,
        dir: Option<&Path>,
        args: &[String],
        origin: Option<&GitRef>,
        auth: &AuthConfig,
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        self.run_output(dir, args, origin, auth, input, deadline)
            .map(|v| v.0)
    }
    pub(crate) fn run_output(
        &self,
        dir: Option<&Path>,
        args: &[String],
        origin: Option<&GitRef>,
        auth: &AuthConfig,
        input: Option<&[u8]>,
        deadline: Instant,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        if Instant::now() >= deadline || self.cancelled.load(Ordering::SeqCst) {
            return Err(AIexError::RefreshPending);
        }
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.active.clone());
        let token = origin
            .map(|o| auth.resolve(&o.url, o.token_env.as_deref()))
            .transpose()?
            .flatten();
        let hooks = tempfile::tempdir()?;
        let mut command = process_wrap::std::CommandWrap::with_new(&self.executable, |cmd| {
            if let Some(dir) = dir {
                cmd.current_dir(dir);
            }
            // Never inherit a caller's working-repository identity or injected Git config.
            for key in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_PARAMETERS",
            ] {
                cmd.env_remove(key);
            }
            cmd.env("GIT_TERMINAL_PROMPT", "0")
                .env("GCM_INTERACTIVE", "never")
                .env("SSH_ASKPASS_REQUIRE", "never")
                .env("GIT_LFS_SKIP_SMUDGE", "1")
                .env("LC_ALL", "C")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args([
                    "-c",
                    &format!("core.hooksPath={}", hooks.path().display()),
                    "-c",
                    "init.templateDir=",
                    "-c",
                    "maintenance.auto=false",
                    "-c",
                    "gc.auto=0",
                    "-c",
                    "submodule.fetchJobs=1",
                    "-c",
                    "fetch.parallel=1",
                    "-c",
                    "protocol.ext.allow=never",
                ])
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(origin) = origin {
                // A credential helper supplies configured credentials through its private stdout pipe.
                // Scope resets prevent a global helper from taking precedence over explicit credentials.
                if token.is_some() || !auth.hosts.is_empty() {
                    let helper_exe = std::env::var_os("AIEXANDRIA_CREDENTIAL_PROGRAM")
                        .map(PathBuf::from)
                        .or_else(|| std::env::current_exe().ok())
                        .unwrap_or_default();
                    let quoted = helper_exe
                        .to_string_lossy()
                        .replace('\\', "/")
                        .replace('\'', "'\\''");
                    let helper = format!("!'{}' __git-credential", quoted);
                    for host in auth.hosts.keys() {
                        cmd.args([
                            "-c",
                            &format!("credential.https://{host}.helper="),
                            "-c",
                            &format!("credential.https://{host}.helper={helper}"),
                        ]);
                        cmd.args([
                            "-c",
                            &format!("credential.http://{host}.helper="),
                            "-c",
                            &format!("credential.http://{host}.helper={helper}"),
                        ]);
                    }
                    if token.is_some() {
                        cmd.args([
                            "-c",
                            &format!("credential.{}.helper=", origin.url),
                            "-c",
                            &format!("credential.{}.helper={helper}", origin.url),
                        ]);
                    }
                    cmd.args(["-c", "credential.useHttpPath=true"])
                        .env(
                            "AIEXANDRIA_CREDENTIAL_AUTH",
                            serde_json::to_string(auth).unwrap_or_default(),
                        )
                        .env(
                            "AIEXANDRIA_CREDENTIAL_ORIGIN",
                            serde_json::to_string(origin).unwrap_or_default(),
                        );
                }
            }
            cmd.args(args);
            #[cfg(unix)]
            {
                use std::os::unix::{io::AsRawFd, process::CommandExt};
                let fds: Vec<_> = self
                    .held_locks
                    .iter()
                    .map(|file| file.as_raw_fd())
                    .collect();
                // Keep ownership/slot leases alive if the parent dies while Git is writing.
                unsafe {
                    cmd.pre_exec(move || {
                        for fd in &fds {
                            let flags = libc::fcntl(*fd, libc::F_GETFD);
                            if flags == -1
                                || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                            {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        Ok(())
                    });
                }
            }
        });
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::std::JobObject);
        let mut child = command.spawn().map_err(|e| {
            AIexError::Git(format!("cannot start Git: {e}; install Git or set --git"))
        })?;
        let (tx, rx) = mpsc::channel();
        for (which, pipe) in [
            (
                0,
                Box::new(child.stdout().take().unwrap()) as Box<dyn Read + Send>,
            ),
            (
                1,
                Box::new(child.stderr().take().unwrap()) as Box<dyn Read + Send>,
            ),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut data = Vec::new();
                let mut buf = [0; 8192];
                let result = loop {
                    match pipe.read(&mut buf) {
                        Ok(0) => break Ok(data),
                        Ok(n) => {
                            if data.len() + n <= 16 * 1024 * 1024 {
                                data.extend_from_slice(&buf[..n]);
                            } else {
                                break Err(std::io::Error::other("Git output exceeds 16 MiB"));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => break Err(e),
                    }
                };
                let _ = tx.send((which, result));
            });
        }
        if let Some(data) = input {
            let data = data.to_vec();
            let mut pipe = child.stdin().take().unwrap();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
            });
        }
        let mut output = [None, None];
        let mut status = None;
        let limit = deadline.min(Instant::now() + self.timeout);
        let result = loop {
            while let Ok((which, data)) = rx.try_recv() {
                output[which] = Some(data);
            }
            match child.try_wait() {
                Ok(Some(s)) => status = Some(s),
                Ok(None) => {}
                Err(e) => break Err(e.into()),
            }
            if let Some(status) = status {
                if output.iter().all(Option::is_some) {
                    let stdout = match output[0].take().unwrap() {
                        Ok(v) => v,
                        Err(e) => break Err(e.into()),
                    };
                    let stderr = match output[1].take().unwrap() {
                        Ok(v) => v,
                        Err(e) => break Err(e.into()),
                    };
                    if status.success() {
                        break Ok((stdout, stderr));
                    }
                    let mut message = String::from_utf8_lossy(&stderr).into_owned();
                    if let Some(token) = &token {
                        message = message.replace(&token.token, "[redacted]");
                    }
                    for (host, configured) in &auth.hosts {
                        let _ = configured;
                        if let Ok(Some(token)) = auth.resolve(&format!("https://{host}/"), None) {
                            message = message.replace(&token.token, "[redacted]");
                        }
                    }
                    let mut end = 4096.min(message.len());
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                    break Err(AIexError::Git(format!(
                        "Git exited {status}: {}",
                        message.trim()
                    )));
                }
            }
            if Instant::now() >= limit || self.cancelled.load(Ordering::SeqCst) {
                break Err(AIexError::Git(
                    "repository operation timed out or was cancelled".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = child.start_kill();
        let _ = child.wait();
        result
    }
}

/// Git invokes this mode as a credential helper, never as an MCP tool.
pub fn credential_helper(operation: &str) -> Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let auth: AuthConfig =
        serde_json::from_str(&std::env::var("AIEXANDRIA_CREDENTIAL_AUTH").unwrap_or_default())
            .map_err(|_| AIexError::Other("credential context missing".into()))?;
    let origin: GitRef =
        serde_json::from_str(&std::env::var("AIEXANDRIA_CREDENTIAL_ORIGIN").unwrap_or_default())
            .map_err(|_| AIexError::Other("credential origin missing".into()))?;
    let mut input = String::new();
    std::io::stdin().take(65536).read_to_string(&mut input)?;
    let fields: std::collections::HashMap<_, _> =
        input.lines().filter_map(|l| l.split_once('=')).collect();
    let protocol = fields.get("protocol").copied().unwrap_or("");
    if !matches!(protocol, "http" | "https") {
        return Ok(());
    }
    let requested = format!(
        "{}://{}/{}",
        protocol,
        fields.get("host").copied().unwrap_or(""),
        fields.get("path").copied().unwrap_or("")
    );
    let same = requested.trim_end_matches('/') == origin.url.trim_end_matches('/');
    match auth.resolve(
        &requested,
        if same {
            origin.token_env.as_deref()
        } else {
            None
        },
    ) {
        Ok(Some(token))
            if !token.username.contains(['\n', '\r']) && !token.token.contains(['\n', '\r']) =>
        {
            println!("username={}\npassword={}\n", token.username, token.token);
        }
        Ok(None) => {}
        _ => {
            println!("quit=true\n");
        }
    }
    Ok(())
}

pub(crate) struct Worker(Arc<AtomicUsize>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
