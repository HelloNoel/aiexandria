//! Top-level registry of named indexes that the MCP server exposes to the LLM.
//!
//! Loaded from a config YAML like:
//! ```yaml
//! indexes:
//!   rust-stdlib:
//!     repo:
//!       url: https://github.com/rust-lang/rust.git
//!       branch: master
//!     path: docs/index.yaml
//!     description: "Rust standard library docs"
//!   internal:
//!     repo:
//!       url: git@github.example.com:platform/docs.git
//!       branch: main
//!       token_env: PLATFORM_DOCS_TOKEN
//!     path: pinakes.yaml
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{AIexError, Result};
use crate::index::GitRef;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConfig {
    #[serde(deserialize_with = "deserialize_unique_indexes")]
    pub indexes: BTreeMap<String, RegistryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEntry {
    pub repo: GitRef,
    /// Path of the root index YAML inside the repo.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-entry override of the cache refresh interval. Accepts humantime
    /// strings like "5m", "1h", "24h"; or "never" to disable refresh for this
    /// entry. If unset, the cache's default policy applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<RefreshSetting>,
}

/// Wrapper so YAML can express either `never` or a humantime duration string.
#[derive(Debug, Clone)]
pub enum RefreshSetting {
    Never,
    Every(Duration),
}

impl Serialize for RefreshSetting {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            RefreshSetting::Never => s.serialize_str("never"),
            RefreshSetting::Every(d) => s.serialize_str(&humantime::format_duration(*d).to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for RefreshSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.eq_ignore_ascii_case("never") {
            return Ok(RefreshSetting::Never);
        }
        humantime::parse_duration(&s)
            .map(RefreshSetting::Every)
            .map_err(serde::de::Error::custom)
    }
}

impl RegistryConfig {
    /// Default path: `$XDG_CONFIG_HOME/aiexandria/registry.yaml` if set,
    /// else `~/.config/aiexandria/registry.yaml`.
    pub fn default_path() -> Option<std::path::PathBuf> {
        crate::config_dir().map(|d| d.join("registry.yaml"))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)?;
        let registry: Self = serde_yaml::from_str(&body).map_err(|e| AIexError::Yaml {
            path: path.display().to_string(),
            source: e,
        })?;
        for (name, entry) in &registry.indexes {
            entry.repo.validate().map_err(|error| {
                AIexError::Other(format!("invalid registry entry '{name}': {error}"))
            })?;
        }
        Ok(registry)
    }
}

fn deserialize_unique_indexes<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, RegistryEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UniqueIndexesVisitor;

    impl<'de> Visitor<'de> for UniqueIndexesVisitor {
        type Value = BTreeMap<String, RegistryEntry>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a mapping of unique registry index names")
        }

        fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut indexes = BTreeMap::new();
            while let Some(name) = map.next_key::<String>()? {
                if indexes.contains_key(&name) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate registry index name '{name}'"
                    )));
                }
                let entry = map.next_value()?;
                indexes.insert(name, entry);
            }
            Ok(indexes)
        }
    }

    deserializer.deserialize_map(UniqueIndexesVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn load(body: &str) -> Result<RegistryConfig> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.yaml");
        std::fs::write(&path, body).unwrap();
        RegistryConfig::load_from(&path)
    }

    #[test]
    fn rejects_duplicate_names_unknown_fields_and_invalid_refs() {
        let entry =
            "repo: { url: https://example.com/docs.git, branch: main }\n    path: pinakes.yaml";
        let duplicate = format!("indexes:\n  docs:\n    {entry}\n  docs:\n    {entry}\n");
        assert!(load(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate registry index name 'docs'"));

        let unknown = format!("indexes:\n  docs:\n    {entry}\n    typo: true\n");
        assert!(load(&unknown)
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        let refs = "indexes:\n  docs:\n    repo: { url: https://example.com/docs.git, branch: main, tag: v1 }\n    path: pinakes.yaml\n";
        assert!(load(refs).unwrap_err().to_string().contains("at most one"));
    }
}
