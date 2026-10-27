//! Resolves a hierarchical index: walking from a root index into sub-indexes,
//! merge entries, and on-demand into the docs/commands they reference.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache::{RefreshPolicy, RepoCache};
use crate::command::{run_command, CommandPolicyChain};
use crate::error::{AIexError, Result};
use crate::index::{Entry, EntryKind, GitRef, Index};

/// A view of one index level after merges have been flattened. Used by `list_docs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: String,
    /// For `index`/`merge` entries, the path that descendants can list with.
    pub child_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    pub view: EntryView,
    /// Underlying entry plus the index it lives in (for path/command resolution).
    pub entry: Entry,
    pub origin: GitRef,
    pub policy: CommandPolicyChain,
}

pub struct Resolver<'a> {
    pub cache: &'a RepoCache,
}

impl<'a> Resolver<'a> {
    pub fn new(cache: &'a RepoCache) -> Self {
        Self { cache }
    }

    /// Load an index file from a repo + path, with merge entries flattened in-place.
    /// `parent_policy` is the policy chain inherited from above (intersected with this index's).
    /// The cache's default refresh policy applies.
    pub fn load_index(
        &self,
        origin: &GitRef,
        rel_path: &str,
        parent_policy: &CommandPolicyChain,
    ) -> Result<LoadedIndex> {
        let mut visited = HashSet::new();
        self.load_index_inner(
            origin,
            rel_path,
            parent_policy,
            self.cache.default_refresh(),
            &mut visited,
            None,
        )
    }

    /// Like [`load_index`] but applies an explicit refresh policy to the root repo.
    /// Sub-indexes/merges still use the cache's default.
    pub fn load_index_with_refresh(
        &self,
        origin: &GitRef,
        rel_path: &str,
        parent_policy: &CommandPolicyChain,
        refresh: RefreshPolicy,
    ) -> Result<LoadedIndex> {
        let mut visited = HashSet::new();
        self.load_index_inner(origin, rel_path, parent_policy, refresh, &mut visited, None)
    }

    fn load_index_inner(
        &self,
        origin: &GitRef,
        rel_path: &str,
        parent_policy: &CommandPolicyChain,
        refresh: RefreshPolicy,
        visited: &mut HashSet<String>,
        merge_prefix: Option<&str>,
    ) -> Result<LoadedIndex> {
        let key = format!("{}#{}", origin.url, rel_path);
        if !visited.insert(key.clone()) {
            return Err(AIexError::MergeCycle {
                repo: origin.url.clone(),
                path: rel_path.to_string(),
            });
        }

        let repo_dir = self.cache.ensure_with(origin, refresh)?;
        let body = self.cache.read_file(&repo_dir, rel_path)?;
        let index = Index::parse(&body, &format!("{}:{rel_path}", origin.url))?;

        let policy = parent_policy.extend(index.command_policy.as_ref())?;

        let mut resolved: Vec<ResolvedEntry> = Vec::new();
        for entry in &index.entries {
            match &entry.kind {
                EntryKind::Merge { path, repo, prefix } => {
                    let merge_origin = repo.clone().unwrap_or_else(|| origin.clone());
                    let combined_prefix = match (merge_prefix, prefix.as_deref()) {
                        (Some(a), Some(b)) => Some(format!("{a}{b}")),
                        (Some(a), None) => Some(a.to_string()),
                        (None, Some(b)) => Some(b.to_string()),
                        (None, None) => None,
                    };
                    let mut sub = self.load_index_inner(
                        &merge_origin,
                        path,
                        &policy,
                        self.cache.default_refresh(),
                        visited,
                        combined_prefix.as_deref(),
                    )?;
                    resolved.append(&mut sub.entries);
                }
                _ => {
                    let view = build_view(entry, merge_prefix);
                    let entry_origin = entry_repo(&entry.kind).unwrap_or_else(|| origin.clone());
                    resolved.push(ResolvedEntry {
                        view,
                        entry: entry.clone(),
                        origin: entry_origin,
                        policy: policy.clone(),
                    });
                }
            }
        }

        let mut ids = HashSet::new();
        for entry in &resolved {
            if !ids.insert(entry.view.id.clone()) {
                return Err(AIexError::DuplicateId {
                    id: entry.view.id.clone(),
                    location: format!("{}:{rel_path}", origin.url),
                });
            }
        }
        visited.remove(&key);

        Ok(LoadedIndex {
            name: index.name,
            description: index.description,
            origin: origin.clone(),
            rel_path: rel_path.to_string(),
            repo_dir,
            entries: resolved,
            policy,
        })
    }

    /// Read a `doc` entry's content.
    pub fn read_doc(&self, entry: &ResolvedEntry) -> Result<String> {
        match &entry.entry.kind {
            EntryKind::Doc { path, .. } => {
                let dir = self.cache.ensure(&entry.origin)?;
                self.cache.read_file(&dir, path)
            }
            _ => Err(AIexError::Other("not a doc entry".into())),
        }
    }

    /// Run a `command` entry against a query.
    pub fn run(&self, entry: &ResolvedEntry, query: &str) -> Result<String> {
        match &entry.entry.kind {
            EntryKind::Command { argv, success_exit_codes, .. } => {
                let dir = self.cache.ensure(&entry.origin)?;
                run_command(&entry.policy, argv, query, &dir, success_exit_codes)
            }
            _ => Err(AIexError::Other("not a command entry".into())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedIndex {
    pub name: String,
    pub description: Option<String>,
    pub origin: GitRef,
    pub rel_path: String,
    pub repo_dir: PathBuf,
    pub entries: Vec<ResolvedEntry>,
    pub policy: CommandPolicyChain,
}

impl LoadedIndex {
    pub fn views(&self) -> Vec<EntryView> {
        self.entries.iter().map(|e| e.view.clone()).collect()
    }

    pub fn find(&self, id: &str) -> Option<&ResolvedEntry> {
        self.entries.iter().find(|e| e.view.id == id)
    }
}

fn entry_repo(kind: &EntryKind) -> Option<GitRef> {
    match kind {
        EntryKind::Doc { repo, .. } => repo.clone(),
        EntryKind::Index { repo, .. } => repo.clone(),
        EntryKind::Merge { repo, .. } => repo.clone(),
        EntryKind::Command { repo, .. } => repo.clone(),
    }
}

fn build_view(entry: &Entry, merge_prefix: Option<&str>) -> EntryView {
    let raw_id = entry
        .id
        .clone()
        .unwrap_or_else(|| slugify(&entry.name));
    let id = match merge_prefix {
        Some(p) => format!("{p}{raw_id}"),
        None => raw_id,
    };
    let (kind_str, child_path) = match &entry.kind {
        EntryKind::Doc { .. } => ("doc", None),
        EntryKind::Index { path, .. } => ("index", Some(path.clone())),
        EntryKind::Merge { path, .. } => ("merge", Some(path.clone())),
        EntryKind::Command { .. } => ("command", None),
    };
    EntryView {
        id,
        name: entry.name.clone(),
        description: entry.description.clone(),
        kind: kind_str.to_string(),
        child_path,
    }
}

fn slugify(s: &str) -> String {
    let mut slug = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

/// Helper used by the MCP tool: walk into a child index by `child_path` from a
/// previously-loaded LoadedIndex.
pub fn descend<'a>(
    resolver: &'a Resolver<'a>,
    parent: &LoadedIndex,
    id: &str,
) -> Result<LoadedIndex> {
    let entry = parent.find(id).ok_or_else(|| AIexError::EntryNotFound {
        id: id.to_string(),
    })?;
    let path = match &entry.entry.kind {
        EntryKind::Index { path, .. } => path.clone(),
        _ => return Err(AIexError::Other(format!("entry '{id}' is not an index"))),
    };
    resolver.load_index(&entry.origin, &path, &parent.policy)
}

// keep import even if unused depending on call sites
#[allow(dead_code)]
fn _path_marker(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_lowercases_unicode_alphanumerics() {
        assert_eq!(slugify("ÉTÉ Über ΑΒ"), "été-über-αβ");
    }
}
