use thiserror::Error;

#[derive(Debug, Error)]
pub enum AIexError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml parse error in {path}: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    #[error("invalid url {url}: {message}")]
    InvalidUrl { url: String, message: String },

    #[error("index entry '{id}' not found")]
    EntryNotFound { id: String },

    #[error("index '{name}' not registered")]
    IndexNotFound { name: String },

    #[error("auth token unavailable for {host}: {reason}")]
    MissingToken { host: String, reason: String },

    #[error("command '{cmd}' not allowed by regex {pattern}")]
    CommandDenied { cmd: String, pattern: String },

    #[error("invalid regex {pattern}: {source}")]
    BadRegex {
        pattern: String,
        #[source]
        source: regex::Error,
    },

    #[error("merge cycle detected at {repo}#{path}")]
    MergeCycle { repo: String, path: String },

    #[error("repository is still being fetched (cold start); retry in a few seconds: {url}")]
    Warming { url: String },

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AIexError>;

impl From<anyhow::Error> for AIexError {
    fn from(e: anyhow::Error) -> Self {
        AIexError::Other(e.to_string())
    }
}
