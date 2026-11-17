//! One completion contract for both scoped and global ranked retrieval.
use serde_json::{json, Value};

use super::{Fallback, Mode};

pub(crate) fn finish(
    mut value: Value,
    requested: Mode,
    resolved: Mode,
    fallback: Fallback,
) -> Value {
    value["mode_requested"] = json!(requested);
    value["mode_resolved"] = json!(resolved);
    if resolved == Mode::Keyword {
        value["semantic"] = if requested == Mode::Auto {
            json!({"status":"not_configured"})
        } else {
            json!({"status":"not_requested"})
        };
        return value;
    }
    if matches!(
        value["semantic"]["status"].as_str(),
        Some("ready" | "empty")
    ) {
        return value;
    }
    let semantic = value["semantic"].clone();
    let preparing = matches!(semantic["status"].as_str(), Some("preparing" | "partial"))
        || semantic["code"] == "provider_starting";
    let code = semantic["code"].as_str().unwrap_or(if preparing {
        "embeddings_pending"
    } else if semantic["status"] == "not_configured" {
        "embedding_not_configured"
    } else {
        "provider_unavailable"
    });
    let reason = semantic["message"].as_str().unwrap_or(if preparing {
        "The requested embedding search is still being prepared."
    } else {
        "Configure a working embedding provider in search.yaml."
    });
    let diagnostic = json!({
        "code": code, "message":reason,
        "retryable": preparing || semantic["retryable"].as_bool().unwrap_or(false),
        "action": if preparing { "Retry after preparation finishes, or explicitly request keyword fallback." } else { "Check aiexandria embedding status, repair or start the provider, and retry; alternatively request fallback: keyword." }
    });
    value["phase"] = json!(if code == "provider_starting" {
        "provider_start"
    } else if preparing {
        "document_embeddings"
    } else {
        "embedding"
    });
    if fallback == Fallback::Keyword {
        value["status"] = json!("degraded");
        value["mode_used"] = json!("keyword");
        value["message"] = json!(format!("Embedding retrieval was not used. Returning explicitly permitted keyword results. {reason}"));
        value["warnings"] = json!([diagnostic]);
    } else {
        value["status"] = json!(if preparing {
            "preparing"
        } else {
            "unavailable"
        });
        value["mode_used"] = Value::Null;
        value["message"] = json!(format!(
            "The requested embedding search did not complete. {reason}"
        ));
        value["error"] = diagnostic;
        value.as_object_mut().unwrap().remove("hits");
        value.as_object_mut().unwrap().remove("truncated");
    }
    if preparing {
        value["retry_after_ms"] = json!(2000);
    }
    value
}
