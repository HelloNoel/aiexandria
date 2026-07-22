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

/// A reference to a git repository at a particular ref. Index files use this both
/// when they live in another repo and when their entries point at content in another repo.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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

/// Restricts what shell commands `EntryKind::Command` may run. Inherited down
/// the index hierarchy: a parent's allow regex is intersected with each child's.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct CommandPolicy {
    /// A regex that the *full command line* (after argv flattening) must match.
    /// Defaults to denying everything when not set anywhere in the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<String>,

    /// Working directory for the command. Defaults to the repo's local checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

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
    pub name: String,

    /// Free-text description shown to the LLM so it can pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Index {
    /// Schema version for forward compat.
    #[serde(default = "default_version")]
    pub version: u32,

    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Command sandboxing policy applied to every `command` descendant of this index.
    /// Children may further restrict but cannot widen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_policy: Option<CommandPolicy>,

    #[serde(default)]
    pub entries: Vec<Entry>,
}

fn default_version() -> u32 {
    1
}

impl Index {
    pub fn parse(yaml: &str, source: &str) -> crate::error::Result<Self> {
        serde_yaml::from_str(yaml).map_err(|source_err| crate::error::AIexError::Yaml {
            path: source.to_string(),
            source: source_err,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_index() {
        let y = r#"
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
  - name: shared
    kind: merge
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
}
