//! Auth resolution: env-var-in-index OR central config keyed by host.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{AIexError, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    /// host -> token mapping. host is the URL host (e.g. "github.com",
    /// "gitlab.example.com"). For SSH URLs, host is parsed out of `git@host:org/repo`.
    #[serde(default)]
    pub hosts: HashMap<String, HostAuth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostAuth {
    /// Either an inline token (discouraged) or an env var name to read it from.
    #[serde(flatten)]
    pub source: TokenSource,

    /// Username to send with HTTPS basic auth. GitHub and GitLab accept any
    /// non-empty username when the password is a PAT; defaults to "git".
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenSource {
    Env { token_env: String },
    Inline { token: String },
}

impl AuthConfig {
    pub fn default_path() -> Option<PathBuf> {
        crate::config_dir().map(|d| d.join("auth.yaml"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path)?;
        serde_yaml::from_str(&body).map_err(|e| AIexError::Yaml {
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Look up a token for a repo URL. Order:
    ///   1. explicit `token_env` on the GitRef (if provided)
    ///   2. central config entry matching the URL's host
    pub fn resolve(&self, url: &str, override_env: Option<&str>) -> Result<Option<ResolvedToken>> {
        if let Some(env) = override_env {
            return Ok(std::env::var(env).ok().map(|t| ResolvedToken {
                username: "git".into(),
                token: t,
            }));
        }
        // Local file:// repos don't need auth.
        if url.starts_with("file://") {
            return Ok(None);
        }
        let host = host_of(url)?;
        let Some(entry) = self.hosts.get(&host) else {
            return Ok(None);
        };
        let token = match &entry.source {
            TokenSource::Env { token_env } => match std::env::var(token_env) {
                Ok(t) => t,
                Err(_) => {
                    return Err(AIexError::MissingToken {
                        host,
                        reason: format!("env var {token_env} not set"),
                    })
                }
            },
            TokenSource::Inline { token } => token.clone(),
        };
        Ok(Some(ResolvedToken {
            username: entry.username.clone().unwrap_or_else(|| "git".into()),
            token,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedToken {
    pub username: String,
    pub token: String,
}

fn host_of(url: &str) -> Result<String> {
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, _)) = rest.split_once(':') {
            return Ok(host.to_string());
        }
    }
    let parsed = Url::parse(url).map_err(|e| AIexError::InvalidUrl {
        url: url.to_string(),
        message: e.to_string(),
    })?;
    parsed
        .host_str()
        .map(|s| s.to_string())
        .ok_or_else(|| AIexError::InvalidUrl {
            url: url.to_string(),
            message: "no host".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_host() {
        assert_eq!(
            host_of("git@github.com:foo/bar.git").unwrap(),
            "github.com"
        );
    }

    #[test]
    fn parses_https_host() {
        assert_eq!(
            host_of("https://gitlab.example.com/foo.git").unwrap(),
            "gitlab.example.com"
        );
    }
}
