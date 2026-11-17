use std::time::Duration;

use serde_json::{json, Value};

use super::config::{EmbeddingConfig, Provider};
use crate::error::{AIexError, Result};

impl EmbeddingConfig {
    pub fn identity(&self) -> String {
        // Preserve the v0.4 field order and identity. Launch settings do not
        // change the vector space or invalidate previously embedded documents.
        let mut config = self.clone();
        config.startup = None;
        super::chunk::digest(
            serde_json::to_vec(&config).expect("serializable embedding configuration"),
        )
    }

    pub fn embed(&self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        self.embed_with_timeout(texts, query, Duration::from_secs(self.timeout_secs))
    }

    pub fn embed_with_timeout(
        &self,
        texts: &[String],
        query: bool,
        timeout: Duration,
    ) -> Result<Vec<Vec<f32>>> {
        let deadline = std::time::Instant::now() + timeout;
        match self.request(texts, query, timeout) {
            Err(AIexError::Embedding { ref code, .. })
                if code == "provider_unreachable" && self.startup.is_some() =>
            {
                super::lifecycle::ensure_started(self, deadline, false)?;
                self.request(
                    texts,
                    query,
                    deadline.saturating_duration_since(std::time::Instant::now()),
                )
            }
            result => result,
        }
    }

    /// A probe never starts another provider, including inside the supervisor.
    pub(crate) fn probe(&self, timeout: Duration) -> Result<usize> {
        self.request(&["Readiness check".into()], true, timeout)
            .map(|vectors| vectors[0].len())
    }

    fn request(&self, texts: &[String], query: bool, timeout: Duration) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if timeout.is_zero() {
            return Err(failure(
                "timeout",
                "Embedding request reached its deadline.",
                true,
            ));
        }
        let prefix = if query {
            &self.query_prefix
        } else {
            &self.document_prefix
        };
        let inputs: Vec<String> = texts.iter().map(|text| format!("{prefix}{text}")).collect();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout.min(Duration::from_secs(self.timeout_secs))))
            .max_redirects(0)
            .build()
            .into();
        let mut request = agent.post(&self.endpoint);
        if let Some(env) = &self.token_env {
            let token = std::env::var(env)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    failure(
                        "authentication_failed",
                        &format!("Embedding credential variable {env} is missing."),
                        false,
                    )
                })?;
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let body = match self.provider {
            Provider::Ollama => {
                json!({"model":self.model,"input":inputs,"truncate":false,"keep_alive":"5m"})
            }
            Provider::OpenaiCompatible => {
                json!({"model":self.model,"input":inputs,"encoding_format":"float"})
            }
        };
        // Never include a provider response body or request text in diagnostics.
        let mut response = request.send_json(body).map_err(request_error)?;
        let value: Value = response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json()
            .map_err(|_| {
                failure(
                    "invalid_embedding_response",
                    "Invalid or oversized embedding response.",
                    false,
                )
            })?;
        if value
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|model| model != self.model)
        {
            return Err(failure(
                "model_mismatch",
                "The provider returned a different model than configured.",
                false,
            ));
        }
        let values: Vec<Value> = match self.provider {
            Provider::Ollama => value
                .get("embeddings")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| {
                    AIexError::Other("embedding response has no embeddings array".into())
                })?,
            Provider::OpenaiCompatible => {
                let data = value.get("data").and_then(Value::as_array).ok_or_else(|| {
                    AIexError::Other("embedding response has no data array".into())
                })?;
                let mut ordered = vec![None; texts.len()];
                for item in data {
                    let index = item
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|i| *i < ordered.len())
                        .ok_or_else(|| {
                            AIexError::Other("invalid embedding response index".into())
                        })?;
                    if ordered[index].is_some() {
                        return Err(AIexError::Other(
                            "duplicate embedding response index".into(),
                        ));
                    }
                    ordered[index] = Some(item.get("embedding").cloned().unwrap_or(Value::Null));
                }
                ordered
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| AIexError::Other("incomplete embedding response".into()))?
            }
        };
        if values.len() != texts.len() {
            return Err(AIexError::Other("embedding response count mismatch".into()));
        }
        let vectors: Vec<Vec<f32>> = values
            .into_iter()
            .map(|v| {
                let vector: Vec<f32> = serde_json::from_value(v)
                    .map_err(|_| AIexError::Other("invalid embedding vector".into()))?;
                normalize(vector)
            })
            .collect::<Result<_>>()?;
        if vectors.iter().any(|v| v.len() != vectors[0].len()) {
            return Err(AIexError::Other("inconsistent embedding dimensions".into()));
        }
        Ok(vectors)
    }
}

pub(crate) fn failure(code: &str, message: &str, retryable: bool) -> AIexError {
    AIexError::Embedding {
        code: code.into(),
        message: message.into(),
        retryable,
    }
}

pub(crate) fn diagnostic(error: &AIexError) -> Value {
    match error {
        AIexError::Embedding {
            code,
            message,
            retryable,
        } => {
            json!({"code":code,"message":message,"retryable":retryable})
        }
        _ => {
            json!({"code":"invalid_embedding_response","message":error.to_string(),"retryable":false})
        }
    }
}

pub(crate) fn cached_diagnostic(message: &str) -> Value {
    serde_json::from_str(message).unwrap_or_else(|_| {
        json!({
            "code":"provider_unavailable", "message":message, "retryable":true
        })
    })
}

fn request_error(error: ureq::Error) -> AIexError {
    use ureq::Error;
    match error {
        Error::Io(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused =>
            failure("provider_unreachable", "The embedding provider refused the connection. Start the provider or configure embedding.startup.", true),
        Error::Timeout(_) => failure("timeout", "The embedding provider exceeded the request timeout.", true),
        Error::StatusCode(401 | 403) => failure("authentication_failed", "The embedding provider rejected authentication; check token_env and credentials.", false),
        Error::StatusCode(404) => failure("model_unavailable", "The embedding endpoint or configured model was not found.", false),
        Error::StatusCode(429) => failure("rate_limited", "The embedding provider rate limit was reached; retry later.", true),
        Error::StatusCode(503) => failure("provider_busy", "The embedding provider is unavailable or still loading; retry later.", true),
        Error::StatusCode(code) => failure("provider_http_error", &format!("The embedding provider returned HTTP {code}; check the endpoint, model and input limits."), code >= 500),
        Error::HostNotFound | Error::ConnectionFailed => failure("provider_connection_failed", "Could not connect to the embedding provider; check its endpoint and network.", true),
        _ => failure("provider_request_failed", "The embedding request failed; check transport, TLS and endpoint configuration.", true),
    }
}

pub fn normalize(vector: Vec<f32>) -> Result<Vec<f32>> {
    if vector.is_empty() || vector.len() > 8192 || vector.iter().any(|v| !v.is_finite()) {
        return Err(AIexError::Other(
            "invalid embedding dimensions or values".into(),
        ));
    }
    let norm = vector
        .iter()
        .map(|v| f64::from(*v).powi(2))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 {
        return Err(AIexError::Other("zero embedding vector".into()));
    }
    Ok(vector
        .into_iter()
        .map(|v| (f64::from(v) / norm) as f32)
        .collect())
}
