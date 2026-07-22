//! Sandboxed execution of `command` entries.
//!
//! A `CommandPolicyChain` is the *intersection* of all `command_policy` blocks
//! seen on the way down the index hierarchy. A child index can only narrow
//! the allow-regex, never widen it: a command must match the regex of every
//! policy in the chain.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;

use crate::error::{AIexError, Result};
use crate::index::CommandPolicy;

#[derive(Debug, Clone)]
pub struct CommandPolicyChain {
    /// All allow-regexes encountered, top-down. Empty = deny all.
    allows: Vec<CompiledAllow>,
    cwd: Option<PathBuf>,
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
            cwd: None,
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
        if let Some(cwd) = &p.cwd {
            next.cwd = Some(PathBuf::from(cwd));
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
    let expanded: Vec<String> = argv
        .iter()
        .map(|a| a.replace("{query}", query))
        .collect();
    let command_line = shell_words::join(&expanded);
    policy.check(&command_line)?;

    let cwd = policy
        .cwd
        .clone()
        .map(|c| if c.is_absolute() { c } else { repo_dir.join(c) })
        .unwrap_or_else(|| repo_dir.to_path_buf());

    let mut child = std::process::Command::new(&expanded[0])
        .args(&expanded[1..])
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain stdout/stderr on separate threads so a chatty child can't fill
    // the OS pipe buffer and deadlock (it blocks writing, we block waiting).
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout_pipe, &mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr_pipe, &mut buf);
        buf
    });

    let timeout = policy.timeout;
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AIexError::Other(format!(
                        "command timed out after {:?}: {command_line}",
                        timeout
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // Threads finish once the pipes hit EOF (guaranteed after the child exits).
    let stdout_bytes = stdout_reader.join().unwrap_or_default();
    let stderr_bytes = stderr_reader.join().unwrap_or_default();
    let mut out = String::from_utf8_lossy(&stdout_bytes).into_owned();
    const MAX: usize = 256 * 1024;
    if out.len() > MAX {
        out.truncate(MAX);
        out.push_str("\n...[truncated]");
    }
    if !status.success() {
        let exit_code = status.code();
        let allowed = match exit_code {
            Some(c) if success_exit_codes.contains(&c) => true,
            _ => false,
        };
        if !allowed {
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            return Err(AIexError::Other(format!(
                "command failed ({status}): {stderr}"
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                cwd: None,
                timeout_secs: None,
            }))
            .unwrap();
        let child = parent
            .extend(Some(&CommandPolicy {
                allow: Some("docs".into()),
                cwd: None,
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
                cwd: None,
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
