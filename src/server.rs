//! MCP server exposing AIexandria's four tools over stdio:
//!
//!  - `list_indexes`: lists every top-level index registered with this AIexandria instance
//!  - `list_docs`:    lists entries for a given index, optionally walking down a path
//!  - `read_doc`:     reads a single doc entry's contents
//!  - `search_docs`:  substring search over names/descriptions, or runs a `command` entry

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cache::{RefreshPolicy, RepoCache};
use crate::command::CommandPolicyChain;
use crate::registry::{RefreshSetting, RegistryConfig, RegistryEntry};
use crate::resolver::{descend, EntryView, LoadedIndex, Resolver};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListDocsRequest {
    /// Name of the registered top-level index (see `list_indexes`).
    pub index: String,
    /// Slash-separated chain of entry ids to descend into. Empty = root.
    #[serde(default)]
    pub path: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadDocRequest {
    pub index: String,
    /// Slash-separated chain of entry ids to descend into.
    #[serde(default)]
    pub path: Vec<String>,
    /// The id of the doc entry within the deepest level.
    pub entry_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileRequest {
    pub index: String,
    /// Slash-separated chain of entry ids to descend into.
    #[serde(default)]
    pub path: Vec<String>,
    /// Id of an entry at the current level whose repo this file lives in.
    /// Use the same id as the `command_id` you just searched with.
    pub entry_id: String,
    /// Repo-relative path to the file, exactly as it appears in search output.
    pub file: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchDocsRequest {
    pub index: String,
    #[serde(default)]
    pub path: Vec<String>,
    pub query: String,
    /// If set, runs the named `command` entry instead of doing substring search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// When true, also grep the contents of `doc` entries at this level
    /// (capped at 5 snippets per doc). Off by default — costs an N-doc read.
    #[serde(default)]
    pub deep: bool,
}

#[derive(Debug, Serialize)]
struct SearchHit {
    #[serde(flatten)]
    view: EntryView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    snippets: Vec<String>,
}

const MAX_SNIPPETS_PER_DOC: usize = 5;
const MAX_SNIPPET_LEN: usize = 200;

fn find_snippets(body: &str, needle: &str) -> Vec<String> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(needle))
        .take(MAX_SNIPPETS_PER_DOC)
        .map(|(i, line)| {
            let truncated: String = line.chars().take(MAX_SNIPPET_LEN).collect();
            format!("L{}: {}", i + 1, truncated)
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct ListIndexesItem {
    name: String,
    description: Option<String>,
    repo_url: String,
    repo_ref: String,
}

#[derive(Clone)]
pub struct AIexandriaServer {
    inner: Arc<ServerInner>,
    #[allow(dead_code)] // populated and used via the #[tool_handler] macro
    tool_router: ToolRouter<AIexandriaServer>,
}

struct ServerInner {
    registry: RegistryConfig,
    cache: RepoCache,
}

#[tool_router]
impl AIexandriaServer {
    pub fn new(registry: RegistryConfig, cache: RepoCache) -> Self {
        Self {
            inner: Arc::new(ServerInner { registry, cache }),
            tool_router: Self::tool_router(),
        }
    }

    fn refresh_for(&self, entry: &RegistryEntry) -> RefreshPolicy {
        match &entry.refresh {
            None => self.inner.cache.default_refresh(),
            Some(RefreshSetting::Never) => RefreshPolicy::Never,
            Some(RefreshSetting::Every(d)) => RefreshPolicy::Every(*d),
        }
    }

    async fn root_for(&self, name: &str) -> Result<LoadedIndex, McpError> {
        let entry: &RegistryEntry =
            self.inner.registry.indexes.get(name).ok_or_else(|| {
                McpError::invalid_params(format!("unknown index '{name}'"), None)
            })?;
        let refresh = self.refresh_for(entry);
        let resolver = Resolver::new(&self.inner.cache);
        resolver
            .load_index_with_refresh(&entry.repo, &entry.path, &CommandPolicyChain::default(), refresh)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    async fn walk_to(&self, index: &str, path: &[String]) -> Result<LoadedIndex, McpError> {
        let mut current = self.root_for(index).await?;
        for step in path {
            let resolver = Resolver::new(&self.inner.cache);
            current = descend(&resolver, &current, step)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        }
        Ok(current)
    }

    #[tool(description = "List every top-level documentation index registered with this AIexandria instance.")]
    async fn list_indexes(&self) -> Result<CallToolResult, McpError> {
        let items: Vec<ListIndexesItem> = self
            .inner
            .registry
            .indexes
            .iter()
            .map(|(name, e)| ListIndexesItem {
                name: name.clone(),
                description: e.description.clone(),
                repo_url: e.repo.url.clone(),
                repo_ref: e
                    .repo
                    .commit
                    .clone()
                    .or_else(|| e.repo.tag.clone())
                    .or_else(|| e.repo.branch.clone())
                    .unwrap_or_else(|| "HEAD".into()),
            })
            .collect();
        let body = serde_json::to_string_pretty(&items)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "List doc/index/command entries for an index level. Use `path` to descend by entry id.")]
    async fn list_docs(
        &self,
        Parameters(req): Parameters<ListDocsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let level = self.walk_to(&req.index, &req.path).await?;
        let views: Vec<EntryView> = level.views();
        let body = serde_json::to_string_pretty(&views)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "Read the contents of a `doc` entry. Path identifies the parent index level; entry_id identifies the doc.")]
    async fn read_doc(
        &self,
        Parameters(req): Parameters<ReadDocRequest>,
    ) -> Result<CallToolResult, McpError> {
        let level = self.walk_to(&req.index, &req.path).await?;
        let entry = level.find(&req.entry_id).ok_or_else(|| {
            McpError::invalid_params(format!("entry '{}' not found", req.entry_id), None)
        })?;
        let resolver = Resolver::new(&self.inner.cache);
        let body = resolver
            .read_doc(entry)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "Read an arbitrary file from an entry's repo. Use after `search_docs` with a `command_id` (e.g. ripgrep) to fetch the full content of a hit. `file` must be a relative path inside the entry's repo; absolute paths and `..` traversal are rejected.")]
    async fn read_file(
        &self,
        Parameters(req): Parameters<ReadFileRequest>,
    ) -> Result<CallToolResult, McpError> {
        let level = self.walk_to(&req.index, &req.path).await?;
        let entry = level.find(&req.entry_id).ok_or_else(|| {
            McpError::invalid_params(format!("entry '{}' not found", req.entry_id), None)
        })?;
        let dir = self
            .inner
            .cache
            .ensure(&entry.origin)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let body = self
            .inner
            .cache
            .read_file(&dir, &req.file)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "Search docs. With `command_id`, executes that `command` entry with `query`. Without it, performs substring search over names/descriptions at the given path.")]
    async fn search_docs(
        &self,
        Parameters(req): Parameters<SearchDocsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let level = self.walk_to(&req.index, &req.path).await?;
        if let Some(cmd_id) = &req.command_id {
            let entry = level.find(cmd_id).ok_or_else(|| {
                McpError::invalid_params(format!("entry '{cmd_id}' not found"), None)
            })?;
            let resolver = Resolver::new(&self.inner.cache);
            let body = resolver
                .run(entry, &req.query)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(body)]));
        }
        let needle = req.query.to_lowercase();
        let resolver = Resolver::new(&self.inner.cache);
        let hits: Vec<SearchHit> = level
            .entries
            .iter()
            .filter_map(|e| {
                let meta_match = e.view.name.to_lowercase().contains(&needle)
                    || e.view
                        .description
                        .as_deref()
                        .map(|d| d.to_lowercase().contains(&needle))
                        .unwrap_or(false);
                let snippets = if req.deep && e.view.kind == "doc" {
                    match resolver.read_doc(e) {
                        Ok(body) => find_snippets(&body, &needle),
                        Err(err) => {
                            tracing::warn!("deep search skip {}: {err}", e.view.id);
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                if meta_match || !snippets.is_empty() {
                    Some(SearchHit {
                        view: e.view.clone(),
                        snippets,
                    })
                } else {
                    None
                }
            })
            .collect();
        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

#[tool_handler]
impl ServerHandler for AIexandriaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info({
            let mut info = Implementation::from_build_env();
            info.name = "AIexandria".to_string();
            info.title = Some("AIexandria".to_string());
            info.version = env!("CARGO_PKG_VERSION").to_string();
            info
        })
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "AIexandria gives you hierarchical access to documentation hosted in git repos. \
             \
             WHEN TO USE: prefer these tools over Bash/grep/Read whenever the user asks \
             about documentation, setup, configuration, or APIs of a *project* (named or \
             implied) — even if the project name doesn't appear in the local working \
             directory. The registry often covers external repos. Always start with \
             `list_indexes`; only fall back to local file search if no registered index \
             looks relevant. Local source reading is still correct for questions about \
             code in the current working directory. \
             \
             HOW TO USE: `list_indexes` shows registered top-level indexes; `list_docs` \
             drills into one by entry id; `read_doc` fetches a leaf doc; `search_docs` \
             does substring matching over names/descriptions, or — with `deep: true` — \
             also greps doc contents at the current level (5 snippets per doc, capped). \
             With `command_id`, `search_docs` runs that index's sandboxed command entry \
             (e.g. ripgrep) instead of substring search. After a `command_id` search, \
             use `read_file` with the same `entry_id` and a path from the search output \
             to fetch the full file."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_snippets_caps_per_doc_and_truncates_long_lines() {
        let body = "auth here\nnope\nauth again\n";
        let hits = find_snippets(body, "auth");
        assert_eq!(hits, vec!["L1: auth here", "L3: auth again"]);

        let many = (1..=10)
            .map(|_| "needle line")
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(find_snippets(&many, "needle").len(), MAX_SNIPPETS_PER_DOC);

        let long = format!("auth {}", "x".repeat(500));
        let hits = find_snippets(&long, "auth");
        let payload = hits[0].strip_prefix("L1: ").unwrap();
        assert_eq!(payload.chars().count(), MAX_SNIPPET_LEN);
    }
}
