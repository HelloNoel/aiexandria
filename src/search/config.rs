use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::error::{AIexError, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub enabled: bool,
    pub embedding: Option<EmbeddingConfig>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub sources: Vec<SourceRule>,
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_corpus_bytes: usize,
    pub chunk_bytes: usize,
    pub global_max_sources: usize,
    pub global_max_chunks: usize,
    pub global_timeout_secs: u64,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedding: None,
            include: Vec::new(),
            exclude: Vec::new(),
            sources: Vec::new(),
            max_files: 2000,
            max_file_bytes: 5 * 1024 * 1024,
            max_corpus_bytes: 64 * 1024 * 1024,
            chunk_bytes: 1200,
            global_max_sources: 128,
            global_max_chunks: 50000,
            global_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceRule {
    pub index: String,
    pub path: Vec<String>,
    pub enabled: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub provider: Provider,
    /// Complete embedding endpoint, including /api/embed or /v1/embeddings.
    pub endpoint: String,
    pub model: String,
    /// Identifies pinned model weights when the provider uses mutable names.
    pub revision: String,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub allow_remote: bool,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Ollama,
    OpenaiCompatible,
}

impl SearchConfig {
    pub fn load(path: Option<&Path>) -> Result<Option<Self>> {
        let explicit = path.is_some();
        let path: Option<PathBuf> = path
            .map(Path::to_path_buf)
            .or_else(|| crate::config_dir().map(|p| p.join("search.yaml")));
        let Some(path) = path else { return Ok(None) };
        let body = match std::fs::read_to_string(path) {
            Ok(body) => body,
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let config: Self = serde_yaml::from_str(&body)
            .map_err(|e| AIexError::Other(format!("invalid search configuration: {e}")))?;
        config.validate()?;
        Ok(config.enabled.then_some(config))
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=1024).contains(&self.global_max_sources)
            || !(1..=200000).contains(&self.global_max_chunks)
            || !(1..=120).contains(&self.global_timeout_secs)
        {
            return Err(AIexError::Other(
                "global search limits: sources 1–1024, chunks 1–200000, timeout 1–120 seconds"
                    .into(),
            ));
        }
        if self.sources.len() > 256
            || self
                .sources
                .iter()
                .any(|s| s.index.is_empty() || s.path.iter().any(String::is_empty))
        {
            return Err(AIexError::Other("search source rules require an index and nonempty path components; at most 256 rules are allowed".into()));
        }
        if self.max_files == 0
            || self.max_files > 10_000
            || self.max_file_bytes == 0
            || self.max_file_bytes > 16 * 1024 * 1024
            || self.max_corpus_bytes == 0
            || self.max_corpus_bytes > 256 * 1024 * 1024
            || !(256..=4096).contains(&self.chunk_bytes)
        {
            return Err(AIexError::Other("invalid search limits (files: 1–10000, file bytes: 1–16 MiB, corpus bytes: 1–256 MiB, chunk bytes: 256–4096)".into()));
        }
        self.filters()?;
        for rule in &self.sources {
            let mut config = self.clone();
            config.include = rule.include.clone().unwrap_or_default();
            config.exclude = rule.exclude.clone();
            config.filters()?;
        }
        if let Some(embedding) = &self.embedding {
            let url = url::Url::parse(&embedding.endpoint)
                .map_err(|_| AIexError::Other("invalid embedding endpoint URL".into()))?;
            let local = url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (!local && (!embedding.allow_remote || url.scheme() != "https"))
            {
                return Err(AIexError::Other("embedding endpoints must be loopback HTTP(S), or explicitly enabled remote HTTPS, without URL credentials/query/fragment".into()));
            }
            if embedding.model.trim().is_empty()
                || embedding.revision.trim().is_empty()
                || !(1..=120).contains(&embedding.timeout_secs)
                || embedding.query_prefix.len() > 512
                || embedding.document_prefix.len() > 512
                || embedding.token_env.as_ref().is_some_and(|s| s.is_empty())
            {
                return Err(AIexError::Other("embedding model and revision are required; timeout must be 1–120 seconds and prefixes at most 512 bytes".into()));
            }
        }
        Ok(())
    }

    pub fn for_source(&self, index: &str, path: &[String]) -> Self {
        let mut config = self.clone();
        let mut rules: Vec<_> = self
            .sources
            .iter()
            .filter(|rule| rule.index == index && path.starts_with(&rule.path))
            .collect();
        rules.sort_by_key(|rule| rule.path.len());
        for rule in rules {
            if let Some(enabled) = rule.enabled {
                config.enabled = enabled;
            }
            if let Some(include) = &rule.include {
                config.include = include.clone();
            }
            config.exclude.extend(rule.exclude.iter().cloned());
        }
        config
    }

    pub(crate) fn filters(&self) -> Result<(GlobSet, GlobSet)> {
        fn build(patterns: &[String]) -> Result<GlobSet> {
            let mut set = GlobSetBuilder::new();
            for pattern in patterns {
                set.add(
                    Glob::new(pattern)
                        .map_err(|e| AIexError::Other(format!("invalid corpus glob: {e}")))?,
                );
            }
            set.build().map_err(|e| AIexError::Other(e.to_string()))
        }
        Ok((build(&self.include)?, build(&self.exclude)?))
    }
}
