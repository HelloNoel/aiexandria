//! YAML index schema.
//!
//! An index is a hierarchical, recursive description of documentation reachable
//! from a git repository. Entries are either:
//!  - `doc`: a leaf pointing at a text file in some repo
//!  - `index`: another index file (same repo or a different one) — resolved on demand
//!  - `command`: a custom retrieval command (e.g. RAG / search) executed in a sandbox
//!  - `merge`: pulls another index in and exposes its entries inline so multiple
//!    contributors can compose without each maintaining a full copy
//!
//! The schema is intentionally permissive: every entry has an optional `id` (used for
//! stable addressing by the LLM) and `description` (used to help the LLM choose).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

pub const SUPPORTED_VERSION: u32 = 1;

/// A reference to a git repository at a particular ref. Index files use this both
/// when they live in another repo and when their entries point at content in another repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRef {
    /// HTTPS or SSH URL of the repository.
    pub url: String,

    /// Branch name. Mutually exclusive with `tag` and `commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,

    /// Optional override of the auth token. If unset, AIexandria falls back to
    /// the central auth config keyed by host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,

    /// Whether to recursively initialize git submodules after clone/refresh.
    /// Defaults to true: many docs repos use submodules to compose docs from
    /// other repos, and AIexandria needs the contents to read them.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub submodules: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Default for GitRef {
    fn default() -> Self {
        Self {
            url: String::new(),
            branch: None,
            tag: None,
            commit: None,
            token_env: None,
            submodules: true,
        }
    }
}

impl GitRef {
    pub fn reference(&self) -> Option<&str> {
        self.commit
            .as_deref()
            .or(self.tag.as_deref())
            .or(self.branch.as_deref())
    }

    pub fn validate(&self) -> crate::error::Result<()> {
        let ref_count = [
            self.branch.is_some(),
            self.tag.is_some(),
            self.commit.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count();
        if ref_count > 1 {
            return Err(crate::error::AIexError::Other(format!(
                "git reference for '{}' must set at most one of branch, tag, or commit",
                self.url
            )));
        }
        if let Some(commit) = &self.commit {
            if !(7..=40).contains(&commit.len())
                || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(crate::error::AIexError::Other(format!(
                    "git commit for '{}' must be a 7-40 character hexadecimal SHA",
                    self.url
                )));
            }
        }
        Ok(())
    }
}

/// Restricts what shell commands `EntryKind::Command` may run. Inherited down
/// the index hierarchy: a parent's allow regex is intersected with each child's.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandPolicy {
    /// A regex that the *full command line* (after argv flattening) must match.
    /// Defaults to denying everything when not set anywhere in the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<String>,

    /// Per-command timeout in seconds. Defaults to 30s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    /// Leaf: a file that should be returned to the LLM verbatim.
    Doc {
        /// Path inside the repo. If `repo` is unset, resolved against the current index's repo.
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<GitRef>,
    },

    /// Reference to another index. Resolved on demand when the LLM descends.
    Index {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<GitRef>,
    },

    /// Inline merge: the referenced index's entries appear as siblings of this entry,
    /// flattened into the parent. Lets multiple users share a partial index without
    /// each maintaining a complete one.
    Merge {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<GitRef>,
        /// Optional prefix prepended to each merged entry's id, to disambiguate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },

    /// Custom retrieval command. The LLM passes a `query` string; the command's stdout
    /// is returned. Useful for RAG, ripgrep, semantic search etc. Subject to `command_policy`.
    Command {
        /// argv-style command. The literal token `{query}` is replaced with the LLM's input.
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<GitRef>,
        /// Exit codes treated as success. Defaults to `[0]`. Set to `[0, 1]` for
        /// grep-family tools (ripgrep, grep, ag) where exit 1 means "no matches".
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        success_exit_codes: Vec<i32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Entry {
    /// Stable identifier used by the LLM to address this entry. Auto-generated from
    /// `name`/path if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Human-readable name shown in listings.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// Free-text description shown to the LLM so it can pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Index {
    /// Schema version for forward compat.
    pub version: u32,

    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Command sandboxing policy applied to every `command` descendant of this index.
    /// Children may further restrict but cannot widen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_policy: Option<CommandPolicy>,

    pub entries: Vec<Entry>,
}

impl Index {
    pub fn parse(yaml: &str, source: &str) -> crate::error::Result<Self> {
        let raw: Value = serde_yaml::from_str(yaml).map_err(|source_err| {
            crate::error::AIexError::Yaml {
                path: source.to_string(),
                source: source_err,
            }
        })?;
        validate_index_shape(&raw, source)?;
        let index: Self = serde_yaml::from_value(raw).map_err(|source_err| {
            crate::error::AIexError::Yaml {
                path: source.to_string(),
                source: source_err,
            }
        })?;
        if index.version != SUPPORTED_VERSION {
            return Err(crate::error::AIexError::Other(format!(
                "unsupported pinakes version {} in {source}; supported version is {SUPPORTED_VERSION}",
                index.version
            )));
        }
        for entry in &index.entries {
            if !matches!(entry.kind, EntryKind::Merge { .. }) && entry.name.is_empty() {
                return Err(crate::error::AIexError::Other(format!(
                    "entry name must not be empty in {source}"
                )));
            }
            if let Some(repo) = entry_repo(&entry.kind) {
                repo.validate()?;
            }
        }
        Ok(index)
    }
}

fn entry_repo(kind: &EntryKind) -> Option<&GitRef> {
    match kind {
        EntryKind::Doc { repo, .. }
        | EntryKind::Index { repo, .. }
        | EntryKind::Merge { repo, .. }
        | EntryKind::Command { repo, .. } => repo.as_ref(),
    }
}

fn validate_index_shape(value: &Value, source: &str) -> crate::error::Result<()> {
    let map = value.as_mapping().ok_or_else(|| {
        crate::error::AIexError::Other(format!("index in {source} must be a mapping"))
    })?;
    reject_unknown_keys(
        map,
        &[
            "version",
            "name",
            "description",
            "command_policy",
            "entries",
        ],
        &format!("index in {source}"),
    )?;
    let entries = map
        .get(Value::String("entries".into()))
        .and_then(Value::as_sequence)
        .ok_or_else(|| {
            crate::error::AIexError::Other(format!(
                "index in {source} must contain an entries list"
            ))
        })?;
    for (position, entry) in entries.iter().enumerate() {
        validate_entry_shape(entry, source, position + 1)?;
    }
    Ok(())
}

fn validate_entry_shape(value: &Value, source: &str, position: usize) -> crate::error::Result<()> {
    let map = value.as_mapping().ok_or_else(|| {
        crate::error::AIexError::Other(format!("entry {position} in {source} must be a mapping"))
    })?;
    let kind = map
        .get(Value::String("kind".into()))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::error::AIexError::Other(format!(
                "entry {position} in {source} must contain a string kind"
            ))
        })?;
    let allowed: &[&str] = match kind {
        "doc" | "index" => &["id", "name", "description", "kind", "path", "repo"],
        "merge" => &["kind", "path", "prefix", "repo"],
        "command" => &[
            "id",
            "name",
            "description",
            "kind",
            "argv",
            "repo",
            "success_exit_codes",
        ],
        _ => {
            return Err(crate::error::AIexError::Other(format!(
                "unknown entry kind '{kind}' at entry {position} in {source}"
            )))
        }
    };
    reject_unknown_keys(map, allowed, &format!("entry {position} in {source}"))
}

fn reject_unknown_keys(map: &Mapping, allowed: &[&str], context: &str) -> crate::error::Result<()> {
    for key in map.keys() {
        let key = key.as_str().ok_or_else(|| {
            crate::error::AIexError::Other(format!("{context} contains a non-string field"))
        })?;
        if !allowed.contains(&key) {
            return Err(crate::error::AIexError::Other(format!(
                "unknown field '{key}' in {context}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_index() {
        let y = r#"
version: 1
name: example
entries:
  - name: README
    kind: doc
    path: README.md
"#;
        let idx = Index::parse(y, "test").unwrap();
        assert_eq!(idx.name, "example");
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn parses_all_entry_kinds() {
        let y = r#"
version: 1
name: full
command_policy:
  allow: "^rg .*"
entries:
  - name: api
    kind: doc
    path: docs/api.md
  - name: subindex
    kind: index
    path: subdir/index.yaml
  - kind: merge
    path: shared.yaml
    repo:
      url: https://github.com/team/shared-docs.git
      branch: main
    prefix: shared/
  - name: search
    kind: command
    argv: ["rg", "{query}", "docs/"]
"#;
        let idx = Index::parse(y, "test").unwrap();
        assert_eq!(idx.entries.len(), 4);
    }

    #[test]
    fn rejects_unknown_versions_fields_and_missing_entries() {
        let unsupported = "version: 99\nname: example\nentries: []\n";
        assert!(Index::parse(unsupported, "test")
            .unwrap_err()
            .to_string()
            .contains("unsupported pinakes version 99"));

        let unknown_top = "version: 1\nname: example\nentry: []\nentries: []\n";
        assert!(Index::parse(unknown_top, "test")
            .unwrap_err()
            .to_string()
            .contains("unknown field 'entry'"));

        let unknown_entry = r#"
version: 1
name: example
entries:
  - name: README
    kind: doc
    path: README.md
    typo: true
"#;
        assert!(Index::parse(unknown_entry, "test")
            .unwrap_err()
            .to_string()
            .contains("unknown field 'typo'"));

        assert!(Index::parse("version: 1\nname: example\n", "test")
            .unwrap_err()
            .to_string()
            .contains("entries list"));
    }

    #[test]
    fn validates_git_refs_and_accepts_nameless_merges() {
        let multiple_refs = r#"
version: 1
name: example
entries:
  - name: README
    kind: doc
    path: README.md
    repo:
      url: https://example.com/docs.git
      branch: main
      tag: v1
"#;
        assert!(Index::parse(multiple_refs, "test")
            .unwrap_err()
            .to_string()
            .contains("at most one"));

        let invalid_commit = r#"
version: 1
name: example
entries:
  - name: README
    kind: doc
    path: README.md
    repo:
      url: https://example.com/docs.git
      commit: not-a-sha
"#;
        assert!(Index::parse(invalid_commit, "test")
            .unwrap_err()
            .to_string()
            .contains("hexadecimal SHA"));

        let merge = r#"
version: 1
name: example
entries:
  - kind: merge
    path: shared.yaml
"#;
        assert!(Index::parse(merge, "test").is_ok());
    }
}
