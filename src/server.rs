//! MCP server exposing AIexandria's tools over stdio:
//!
//!  - `list_indexes`: lists every top-level index registered with this AIexandria instance
//!  - `list_docs`:    lists entries for a given index, optionally walking down a path
//!  - `read_doc`:     reads a single doc entry's contents
//!  - `read_file`:    reads an arbitrary file from an indexed repository
//!  - `read_url`:     reads an indexed repository file from a web URL
//!  - `search_docs`:  substring search over names/descriptions, or runs a `command` entry

use std::sync::Arc;

use percent_encoding::percent_decode_str;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::cache::{RefreshPolicy, RepoCache};
use crate::command::CommandPolicyChain;
use crate::index::{EntryKind, GitRef, Index};
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
    /// 1-based line to start from. Omit to start at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Max lines to return. Omit for the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
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
    /// 1-based line to start from. Omit to start at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Max lines to return. Omit for the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Return `body`, or a 1-based line window of it with a `[lines a-b of N]`
/// header when offset/limit is given. No window = raw body (unchanged).
fn window(body: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String, String> {
    if offset.is_none() && limit.is_none() {
        return Ok(body.to_string());
    }
    if offset == Some(0) {
        return Err("offset must be at least 1".into());
    }
    if limit == Some(0) {
        return Err("limit must be at least 1".into());
    }
    let lines: Vec<&str> = body.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok("[lines 0-0 of 0]\n".into());
    }
    let requested = offset.unwrap_or(1);
    if requested > total {
        return Err(format!(
            "offset {requested} exceeds document length of {total} lines"
        ));
    }
    let start = requested - 1;
    let end = limit.map_or(total, |l| start.saturating_add(l).min(total));
    Ok(format!(
        "[lines {}-{} of {}]\n{}",
        start + 1,
        end,
        total,
        lines[start..end].join("\n")
    ))
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadUrlRequest {
    /// A web URL to a file in a repo that appears somewhere in the index,
    /// e.g. `https://github.com/org/repo/blob/main/docs/x.md`.
    pub url: String,
    /// 1-based line to start from. Omit to start at the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Max lines to return. Omit for the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Canonical host/path identity shared by HTTPS, ssh://, and SCP-style remotes.
fn repo_identity(raw: &str) -> Option<String> {
    let (host, path) = if let Ok(url) = url::Url::parse(raw) {
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str()?),
            None => url.host_str()?.to_string(),
        };
        (host, url.path().to_string())
    } else {
        let without_user = raw.rsplit_once('@').map_or(raw, |(_, rest)| rest);
        let (host, path) = without_user.split_once(':')?;
        (host.to_string(), path.to_string())
    };
    let path = path
        .trim_matches('/')
        .trim_end_matches(".git")
        .to_lowercase();
    if path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_lowercase()))
}

/// Split a web file URL into (repo identity, `<ref>/<path>` tail).
fn split_file_url(raw: &str) -> Option<(String, String)> {
    const MARKERS: &[&str] = &[
        "/src/branch/",
        "/src/commit/",
        "/src/tag/",
        "/raw/branch/",
        "/raw/commit/",
        "/raw/tag/",
        "/blob/",
        "/raw/",
    ];
    let url = url::Url::parse(raw).ok()?;
    let path = url.path();
    let (idx, marker) = MARKERS
        .iter()
        .filter_map(|marker| path.find(marker).map(|idx| (idx, *marker)))
        .min_by_key(|(idx, _)| *idx)?;
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str()?),
        None => url.host_str()?.to_string(),
    };
    let repo = format!("{}://{}{}", url.scheme(), host, &path[..idx]);
    let tail = percent_decode_str(&path[idx + marker.len()..])
        .decode_utf8()
        .ok()?
        .into_owned();
    if tail.is_empty() {
        return None;
    }
    Some((repo_identity(&repo)?, tail))
}

/// Remove the link's ref, preferring the indexed ref so branch names may contain `/`.
fn file_path_from_tail(tail: &str, git_ref: &GitRef) -> Option<String> {
    if let Some(reference) = git_ref.reference() {
        if let Some(path) = tail
            .strip_prefix(reference)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return (!path.is_empty()).then(|| path.to_string());
        }
    }
    let (_, path) = tail.split_once('/')?;
    (!path.is_empty()).then(|| path.to_string())
}

fn select_origin(
    candidates: Vec<(GitRef, RefreshPolicy)>,
    tail: &str,
) -> Result<Option<(GitRef, RefreshPolicy)>, String> {
    if candidates.len() <= 1 {
        return Ok(candidates.into_iter().next());
    }
    let indexed_refs = candidates
        .iter()
        .filter_map(|(git_ref, _)| git_ref.reference().map(str::to_string))
        .collect::<Vec<_>>();
    let mut matching = candidates.into_iter().filter(|(git_ref, _)| {
        git_ref.reference().is_some_and(|reference| {
            tail.strip_prefix(reference)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    });
    let selected = matching.next();
    if selected.is_some() && matching.next().is_none() {
        return Ok(selected);
    }
    let refs = if indexed_refs.is_empty() {
        "HEAD".to_string()
    } else {
        indexed_refs.join(", ")
    };
    match selected {
        Some(_) => Err(format!(
            "URL ref is ambiguous because it matches multiple indexed refs: {refs}"
        )),
        None => Err(format!(
            "URL ref does not match any indexed ref; repository is indexed at: {refs}"
        )),
    }
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
        let entry: &RegistryEntry = self
            .inner
            .registry
            .indexes
            .get(name)
            .ok_or_else(|| McpError::invalid_params(format!("unknown index '{name}'"), None))?;
        let refresh = self.refresh_for(entry);
        let resolver = Resolver::new(&self.inner.cache);
        resolver
            .load_index_with_refresh(
                &entry.repo,
                &entry.path,
                &CommandPolicyChain::default(),
                refresh,
            )
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

    /// Find an entry's repo (anywhere in any registered index tree) whose URL
    /// matches `target`. Used by `read_url` to gate reads to indexed repos.
    fn find_origins(&self, target: &str) -> Result<Vec<(GitRef, RefreshPolicy)>, String> {
        fn walk(
            cache: &RepoCache,
            origin: &GitRef,
            path: &str,
            refresh: RefreshPolicy,
            target: &str,
            visited: &mut std::collections::HashSet<String>,
            matches: &mut Vec<GitRef>,
        ) -> crate::error::Result<()> {
            if repo_identity(&origin.url).as_deref() == Some(target) && !matches.contains(origin) {
                matches.push(origin.clone());
            }
            let key = format!(
                "{}#{:?}#{:?}#{:?}#{path}",
                origin.url, origin.branch, origin.tag, origin.commit
            );
            if !visited.insert(key) {
                return Ok(());
            }
            // Inspect references before merge flattening, which can erase the
            // repository of an empty merge or one whose entries override it.
            let dir = cache.ensure_with(origin, refresh)?;
            let body = cache.read_file(&dir, path)?;
            let idx = Index::parse(&body, &format!("{}:{path}", origin.url))?;
            for entry in &idx.entries {
                let entry_origin = match &entry.kind {
                    EntryKind::Doc { repo, .. }
                    | EntryKind::Index { repo, .. }
                    | EntryKind::Merge { repo, .. }
                    | EntryKind::Command { repo, .. } => repo.as_ref().unwrap_or(origin),
                };
                if repo_identity(&entry_origin.url).as_deref() == Some(target)
                    && !matches.contains(entry_origin)
                {
                    matches.push(entry_origin.clone());
                }
            }
            let mut first_error = None;
            for e in &idx.entries {
                if let EntryKind::Index { path, repo } | EntryKind::Merge { path, repo, .. } =
                    &e.kind
                {
                    if let Err(err) = walk(
                        cache,
                        repo.as_ref().unwrap_or(origin),
                        path,
                        refresh,
                        target,
                        visited,
                        matches,
                    ) {
                        first_error.get_or_insert(err);
                    }
                }
            }
            first_error.map_or(Ok(()), Err)
        }
        let mut first_error = None;
        let mut found = Vec::new();
        for entry in self.inner.registry.indexes.values() {
            let mut visited = std::collections::HashSet::new();
            let refresh = self.refresh_for(entry);
            let mut matches = Vec::new();
            if let Err(err) = walk(
                &self.inner.cache,
                &entry.repo,
                &entry.path,
                refresh,
                target,
                &mut visited,
                &mut matches,
            ) {
                first_error.get_or_insert(err.to_string());
            }
            // A failed child must not discard references already discovered,
            // including the registry's root repository. Auth and checkout
            // options do not make aliases of the same repository/ref distinct.
            for origin in matches {
                if !found.iter().any(|(existing, _): &(GitRef, RefreshPolicy)| {
                    repo_identity(&existing.url) == repo_identity(&origin.url)
                        && existing.branch == origin.branch
                        && existing.tag == origin.tag
                        && existing.commit == origin.commit
                }) {
                    found.push((origin, refresh));
                }
            }
        }
        if !found.is_empty() {
            Ok(found)
        } else if let Some(err) = first_error {
            Err(format!("could not search every registered index: {err}"))
        } else {
            Ok(found)
        }
    }

    #[tool(
        description = "List every top-level documentation index registered with this AIexandria instance."
    )]
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
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "List doc/index/command entries for an index level. Use `path` to descend by entry id."
    )]
    async fn list_docs(
        &self,
        Parameters(req): Parameters<ListDocsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let level = self.walk_to(&req.index, &req.path).await?;
        let views: Vec<EntryView> = level.views();
        let body = serde_json::to_string_pretty(&views)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "Read the contents of a `doc` entry. Path identifies the parent index level; entry_id identifies the doc. For large docs, page through with `offset`/`limit` (1-based line range) instead of fetching the whole file."
    )]
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
        let body =
            window(&body, req.offset, req.limit).map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "Read an arbitrary file from an entry's repo. Use after `search_docs` to fetch a hit's full content, or to open a file the user linked: for a GitHub/GHE URL like `.../blob/<ref>/<path>`, pass `<path>` as `file` and the entry whose `repo` matches the URL as `entry_id`. `file` is a repo-relative path; absolute paths and `..` are rejected. Page large files with `offset`/`limit` (1-based line range)."
    )]
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
        let body =
            window(&body, req.offset, req.limit).map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "Read a file directly from a web URL (e.g. one the user pasted) like `https://host/org/repo/blob/<ref>/<path>` — GitHub, Codeberg, Forgejo, and Gitea link forms are understood. The repo must appear somewhere in the index; the file is read from the indexed ref. Use this instead of `search_docs` when you already have a link. Page large files with `offset`/`limit`."
    )]
    async fn read_url(
        &self,
        Parameters(req): Parameters<ReadUrlRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (repo, tail) = split_file_url(&req.url).ok_or_else(|| {
            McpError::invalid_params(
                "unrecognized file URL; expected a .../blob/<ref>/<path> style link".to_string(),
                None,
            )
        })?;
        let candidates = self
            .find_origins(&repo)
            .map_err(|e| McpError::internal_error(e, None))?;
        let (git_ref, refresh) = select_origin(candidates, &tail)
            .map_err(|e| McpError::invalid_params(e, None))?
            .ok_or_else(|| {
                McpError::invalid_params(format!("no repo in the index matches {repo}"), None)
            })?;
        let rel = file_path_from_tail(&tail, &git_ref).ok_or_else(|| {
            McpError::invalid_params(
                "file URL does not contain a path after its git ref".to_string(),
                None,
            )
        })?;
        let dir = self
            .inner
            .cache
            .ensure_with(&git_ref, refresh)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let body = self
            .inner
            .cache
            .read_file(&dir, &rel)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let body =
            window(&body, req.offset, req.limit).map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "Search docs. With `command_id`, runs that `command` entry — consult that entry's own `description` for what it searches and how to phrase the query. As a general fallback, command search matches text literally/by regex (not semantically), so prefer short distinctive keywords or a `term1|term2` alternation over full sentences. Without `command_id`, substring search over entry names/descriptions at the given path (add `deep: true` to also grep doc contents)."
    )]
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
            return Ok(CallToolResult::success(vec![ContentBlock::text(body)]));
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
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }
}

#[tool_handler]
impl ServerHandler for AIexandriaServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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
              to fetch the full file. When the user provides a web file URL, prefer \
              `read_url`; it validates the repository against the index and reads the \
              file from the configured git ref."
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

    #[test]
    fn window_ranges_and_bounds() {
        let body = "a\nb\nc\nd\ne";
        assert_eq!(window(body, None, None).unwrap(), body); // no window = raw
        assert_eq!(
            window(body, Some(2), Some(2)).unwrap(),
            "[lines 2-3 of 5]\nb\nc"
        );
        assert_eq!(
            window(body, Some(4), None).unwrap(),
            "[lines 4-5 of 5]\nd\ne"
        );
        assert_eq!(
            window(body, Some(5), Some(99)).unwrap(),
            "[lines 5-5 of 5]\ne"
        );
        assert_eq!(window("", None, Some(5)).unwrap(), "[lines 0-0 of 0]\n");
        assert!(window(body, Some(0), Some(3)).is_err());
        assert!(window(body, Some(99), Some(3)).is_err());
        assert!(window(body, Some(1), Some(0)).is_err());
    }

    #[test]
    fn split_file_url_handles_forge_link_forms() {
        assert_eq!(
            split_file_url("https://github.com/org/repo/blob/feature/docs/docs/x%20y.md#L20"),
            Some((
                "github.com/org/repo".into(),
                "feature/docs/docs/x y.md".into()
            ))
        );
        assert_eq!(
            split_file_url("https://codeberg.org/u/r/raw/branch/main/f.md?display=source"),
            Some(("codeberg.org/u/r".into(), "main/f.md".into()))
        );
        assert_eq!(split_file_url("https://github.com/org/repo"), None);

        let git_ref = GitRef {
            url: "git@github.com:org/repo.git".into(),
            branch: Some("feature/docs".into()),
            ..GitRef::default()
        };
        assert_eq!(
            repo_identity(&git_ref.url),
            Some("github.com/org/repo".into())
        );
        assert_eq!(
            file_path_from_tail("feature/docs/docs/x y.md", &git_ref),
            Some("docs/x y.md".into())
        );
    }

    #[test]
    fn select_origin_uses_the_url_ref_when_repo_has_multiple_refs() {
        let candidate = |branch: &str| {
            (
                GitRef {
                    url: "git@github.com:org/repo.git".into(),
                    branch: Some(branch.into()),
                    ..GitRef::default()
                },
                RefreshPolicy::Never,
            )
        };
        let candidates = vec![candidate("v1"), candidate("feature/x")];

        let (selected, _) = select_origin(candidates.clone(), "feature/x/docs/file.md")
            .unwrap()
            .unwrap();
        assert_eq!(selected.branch.as_deref(), Some("feature/x"));
        assert_eq!(
            file_path_from_tail("feature/x/docs/file.md", &selected),
            Some("docs/file.md".into())
        );

        let error = select_origin(candidates, "v2/docs/file.md").unwrap_err();
        assert!(error.contains("does not match any indexed ref"));
    }

    #[test]
    fn select_origin_rejects_ambiguous_slash_prefixes() {
        let candidate = |branch: &str| {
            (
                GitRef {
                    url: "https://github.com/org/repo.git".into(),
                    branch: Some(branch.into()),
                    ..GitRef::default()
                },
                RefreshPolicy::Never,
            )
        };
        let candidates = vec![candidate("feature"), candidate("feature/x")];
        let error = select_origin(candidates, "feature/x/docs/file.md").unwrap_err();
        assert!(error.contains("ambiguous"));
    }

    #[test]
    fn find_origins_collects_every_ref_for_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("root");
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(
            repo_dir.join("pinakes.yaml"),
            r#"
version: 1
name: root
entries:
  - name: Version one
    kind: doc
    path: README.md
    repo: { url: https://github.com/org/repo.git, branch: v1 }
  - name: Version two
    kind: doc
    path: README.md
    repo: { url: git@github.com:org/repo.git, branch: v2 }
"#,
        )
        .unwrap();
        let repo = git2::Repository::init_opts(
            &repo_dir,
            git2::RepositoryInitOptions::new().initial_head("main"),
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();

        let registry = RegistryConfig {
            indexes: std::iter::once((
                "root".into(),
                RegistryEntry {
                    repo: GitRef {
                        url: format!("file://{}", repo_dir.display()),
                        branch: Some("main".into()),
                        ..GitRef::default()
                    },
                    path: "pinakes.yaml".into(),
                    description: None,
                    refresh: None,
                },
            ))
            .collect(),
        };
        let cache = RepoCache::new(tmp.path().join("cache"), crate::auth::AuthConfig::default());
        let server = AIexandriaServer::new(registry, cache);

        let found = server.find_origins("github.com/org/repo").unwrap();
        let refs = found
            .iter()
            .filter_map(|(git_ref, _)| git_ref.reference())
            .collect::<Vec<_>>();
        assert_eq!(refs, vec!["v1", "v2"]);
    }
}

#[cfg(test)]
mod url_lookup_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    fn reference(url: &str) -> GitRef {
        GitRef {
            url: url.into(),
            branch: Some("main".into()),
            ..GitRef::default()
        }
    }
    fn cached(root: &std::path::Path, git: &GitRef, yaml: &str) {
        let hash = Sha256::digest(git.url.as_bytes());
        let dir = root.join(hex::encode(&hash[..8])).join("main");
        git2::Repository::init(&dir).unwrap();
        std::fs::write(dir.join("pinakes.yaml"), yaml).unwrap();
    }
    fn server(root: &std::path::Path, yaml: &str) -> AIexandriaServer {
        let git = reference("https://github.com/org/root.git");
        cached(root, &git, yaml);
        let entry = RegistryEntry {
            repo: git,
            path: "pinakes.yaml".into(),
            description: None,
            refresh: Some(RefreshSetting::Never),
        };
        let registry = RegistryConfig {
            indexes: [("root".into(), entry)].into_iter().collect(),
        };
        let cache = RepoCache::new(root.into(), crate::auth::AuthConfig::default())
            .with_default_refresh(RefreshPolicy::Never);
        AIexandriaServer::new(registry, cache)
    }
    #[test]
    fn root_repo_remains_available_with_broken_child() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server(
            tmp.path(),
            r#"version: 1
name: root
entries:
  - name: broken child
    kind: index
    path: missing.yaml
"#,
        );
        let found = s.find_origins("github.com/org/root").unwrap();
        assert_eq!(found.len(), 1);
        assert!(s.find_origins("github.com/org/unknown").is_err());
    }
    #[test]
    fn aliases_of_one_repo_ref_are_not_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server(
            tmp.path(),
            r#"version: 1
name: root
entries:
  - name: https
    kind: doc
    path: README.md
    repo: {url: 'https://github.com/org/docs.git', branch: main}
  - name: ssh
    kind: doc
    path: README.md
    repo: {url: 'git@github.com:org/docs.git', branch: main}
"#,
        );
        let found = s.find_origins("github.com/org/docs").unwrap();
        let selected = select_origin(found, "main/README.md");
        assert!(
            selected.is_ok(),
            "same repo and ref rejected as ambiguous: {selected:?}"
        );
    }
    #[test]
    fn merge_preserves_its_repository_when_entries_do_not_reference_it() {
        for merged in [
            "version: 1\nname: shared\nentries: []\n",
            "version: 1\nname: shared\nentries:\n  - name: external\n    kind: doc\n    path: README.md\n    repo: {url: https://github.com/org/external.git}\n",
        ] {
        let tmp = tempfile::tempdir().unwrap();
        cached(tmp.path(), &reference("https://github.com/org/shared.git"), merged);
        let s = server(tmp.path(), r#"version: 1
name: root
entries:
  - kind: merge
    path: pinakes.yaml
    repo: {url: 'https://github.com/org/shared.git', branch: main}
"#);
        let found = s.find_origins("github.com/org/shared").unwrap();
        assert_eq!(found.len(), 1, "explicit merge repository disappeared after flattening");
        }
    }

    #[test]
    fn unavailable_child_does_not_hide_later_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        cached(
            tmp.path(),
            &reference("https://github.com/org/child.git"),
            r#"version: 1
name: child
entries:
  - name: docs
    kind: doc
    path: README.md
    repo: {url: 'https://github.com/org/docs.git', branch: main}
"#,
        );
        let s = server(
            tmp.path(),
            r#"version: 1
name: root
entries:
  - name: broken child
    kind: index
    path: missing.yaml
  - kind: merge
    path: pinakes.yaml
    repo: {url: 'https://github.com/org/child.git', branch: main}
"#,
        );
        let found = s.find_origins("github.com/org/docs").unwrap();
        assert_eq!(found.len(), 1);
    }
}
