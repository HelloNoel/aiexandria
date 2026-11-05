use std::time::Duration;

use serde_json::{json, Value};

use super::config::{EmbeddingConfig, Provider};
use crate::error::{AIexError, Result};

impl EmbeddingConfig {
    pub fn identity(&self) -> String {
        super::chunk::digest(
            serde_json::to_vec(self).expect("serializable embedding configuration"),
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
        if texts.is_empty() {
            return Ok(Vec::new());
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
                    AIexError::Other(format!("embedding credential variable {env} is missing"))
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
        let mut response = request.send_json(body)
            .map_err(|_| AIexError::Other("embedding request failed; check the endpoint, model, credentials, and input limits".into()))?;
        let value: Value = response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json()
            .map_err(|_| AIexError::Other("invalid or oversized embedding response".into()))?;
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
