//! Policy-controlled execution of `command` entries.
//!
//! A `CommandPolicyChain` is the *intersection* of all `command_policy` blocks
//! seen on the way down the index hierarchy. A child index can only narrow
//! the allow-regex, never widen it: a command must match the regex of every
//! policy in the chain.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;

use crate::error::{AIexError, Result};
use crate::index::CommandPolicy;

#[derive(Debug, Clone)]
pub struct CommandPolicyChain {
    /// All allow-regexes encountered, top-down. Empty = deny all.
    allows: Vec<CompiledAllow>,
    timeout: Duration,
}

#[derive(Debug, Clone)]
struct CompiledAllow {
    pattern: String,
    regex: Regex,
}

impl Default for CommandPolicyChain {
    fn default() -> Self {
        Self {
            allows: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }
}

impl CommandPolicyChain {
    pub fn extend(&self, policy: Option<&CommandPolicy>) -> Result<Self> {
        let mut next = self.clone();
        let Some(p) = policy else {
            return Ok(next);
        };
        if let Some(allow) = &p.allow {
            let regex = Regex::new(allow).map_err(|source| AIexError::BadRegex {
                pattern: allow.clone(),
                source,
            })?;
            next.allows.push(CompiledAllow {
                pattern: allow.clone(),
                regex,
            });
        }
        if let Some(t) = p.timeout_secs {
            next.timeout = Duration::from_secs(t);
        }
        Ok(next)
    }

    fn check(&self, command_line: &str) -> Result<()> {
        if self.allows.is_empty() {
            return Err(AIexError::CommandDenied {
                cmd: command_line.to_string(),
                pattern: "<no allow regex set>".into(),
            });
        }
        for a in &self.allows {
            if !a.regex.is_match(command_line) {
                return Err(AIexError::CommandDenied {
                    cmd: command_line.to_string(),
                    pattern: a.pattern.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Run `argv`, replacing the literal token `{query}` in any arg with `query`.
/// Stdout (UTF-8 lossy) is returned. Truncated to 256KiB. `success_exit_codes`
/// lists exit codes treated as success; empty means `[0]` only.
pub fn run_command(
    policy: &CommandPolicyChain,
    argv: &[String],
    query: &str,
    repo_dir: &Path,
    success_exit_codes: &[i32],
) -> Result<String> {
    if argv.is_empty() {
        return Err(AIexError::Other("empty argv".into()));
    }
    let expanded: Vec<String> = argv.iter().map(|a| a.replace("{query}", query)).collect();
    let command_line = expanded.join(" ");
    policy.check(&command_line)?;
    let mut command = process_wrap::std::CommandWrap::with_new(&expanded[0], |cmd| {
        cmd.args(&expanded[1..])
            .current_dir(repo_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    let mut child = command.spawn()?;
    let stdout_pipe = child.stdout().take().expect("stdout piped");
    let stderr_pipe = child.stderr().take().expect("stderr piped");
    let (stdout_tx, stdout_rx) = std::sync::mpsc::sync_channel(1);
    let (stderr_tx, stderr_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = stdout_tx.send(read_bounded(stdout_pipe));
    });
    std::thread::spawn(move || {
        let _ = stderr_tx.send(read_bounded(stderr_pipe));
    });
    let started = std::time::Instant::now();
    let mut status = None;
    let mut stdout = None;
    let mut stderr = None;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => status = Some(exit),
            Ok(None) => {}
            Err(err) => {
                let _ = child.start_kill();
                let _ = child.wait();
                return Err(err.into());
            }
        }
        if stdout.is_none() {
            stdout = stdout_rx.try_recv().ok();
        }
        if stderr.is_none() {
            stderr = stderr_rx.try_recv().ok();
        }
        if let (Some(status), true, true) = (status, stdout.is_some(), stderr.is_some()) {
            let stdout = stdout.take().unwrap();
            let stderr = stderr.take().unwrap();
            // Commands must not leave background workers running after returning.
            let _ = child.start_kill();
            let stdout = stdout?;
            let stderr = stderr?;
            if !status.success()
                && !status
                    .code()
                    .is_some_and(|code| success_exit_codes.contains(&code))
            {
                return Err(AIexError::Other(format!(
                    "command failed ({status}): {}",
                    render_output(stderr)
                )));
            }
            return Ok(render_output(stdout));
        }
        if started.elapsed() >= policy.timeout {
            let _ = child.start_kill();
            let _ = child.wait();
            // Never wait indefinitely for a detached descendant holding a pipe.
            return Err(AIexError::Other(format!(
                "command timed out after {:?}: {command_line}",
                policy.timeout
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

const MAX_OUTPUT_BYTES: usize = 256 * 1024;
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}
fn read_bounded(mut stream: impl std::io::Read) -> std::io::Result<BoundedOutput> {
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        truncated: false,
    };
    let mut buffer = [0u8; 8192];
    loop {
        let count = match stream.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if count == 0 {
            return Ok(output);
        }
        let retained = count.min(MAX_OUTPUT_BYTES - output.bytes.len());
        output.bytes.extend_from_slice(&buffer[..retained]);
        output.truncated |= retained < count;
    }
}
fn render_output(output: BoundedOutput) -> String {
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    let truncated = output.truncated || text.len() > MAX_OUTPUT_BYTES;
    if text.len() > MAX_OUTPUT_BYTES {
        let mut boundary = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    if truncated {
        text.push_str("\n...[truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_readers_drain_but_retain_only_the_limit() {
        use std::io::Read;
        let output =
            read_bounded(std::io::repeat(255).take((MAX_OUTPUT_BYTES * 8) as u64)).unwrap();
        assert_eq!(output.bytes.len(), MAX_OUTPUT_BYTES);
        assert!(output.truncated);
        let text = render_output(output);
        assert!(text.len() <= MAX_OUTPUT_BYTES + "\n...[truncated]".len());
        assert!(text.ends_with("...[truncated]"));
    }

    #[test]
    fn deny_when_no_policy() {
        let chain = CommandPolicyChain::default();
        assert!(chain.check("rg foo").is_err());
    }

    #[test]
    fn child_can_narrow_not_widen() {
        let parent = CommandPolicyChain::default()
            .extend(Some(&CommandPolicy {
                allow: Some("^rg ".into()),
                timeout_secs: None,
            }))
            .unwrap();
        let child = parent
            .extend(Some(&CommandPolicy {
                allow: Some("docs".into()),
                timeout_secs: None,
            }))
            .unwrap();

        assert!(child.check("rg foo docs/api.md").is_ok());
        // narrowed: must contain "docs"
        assert!(child.check("rg foo src/").is_err());
        // can't widen: child allows "docs" but parent requires "^rg "
        assert!(child.check("cat docs/x").is_err());
    }

    #[test]
    fn success_exit_codes_treats_listed_as_ok() {
        let policy = CommandPolicyChain::default()
            .extend(Some(&CommandPolicy {
                allow: Some("^sh ".into()),
                timeout_secs: None,
            }))
            .unwrap();
        let argv: Vec<String> = vec!["sh".into(), "-c".into(), "exit 1".into()];
        let tmp = std::env::temp_dir();

        // Default: exit 1 is failure.
        assert!(run_command(&policy, &argv, "", &tmp, &[]).is_err());
        // Opted in: exit 1 returns empty stdout.
        let out = run_command(&policy, &argv, "", &tmp, &[0, 1]).unwrap();
        assert_eq!(out, "");
        // Other non-zero exits still fail even with [0, 1].
        let argv_2: Vec<String> = vec!["sh".into(), "-c".into(), "exit 2".into()];
        assert!(run_command(&policy, &argv_2, "", &tmp, &[0, 1]).is_err());
    }
}
