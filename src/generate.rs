//! Generate a `pinakes.yaml` index for a checked-out repo.
//!
//! Walks the repo, treating each directory as an `index` entry and each
//! markdown-ish file as a `doc` entry. Submodules become `index` entries that
//! point at `pinakes.yaml` inside the submodule (generated lazily, or on
//! the fly here).
//!
//! This is intentionally a *starting point* for repo authors — not a final
//! product. Users should curate the output, drop entries they don't want, add
//! `description:` fields to help the LLM choose, and add `command:` entries
//! for things like ripgrep.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AIexError, Result};

/// Conventional filename for a pinakes index. The resolver doesn't require
/// this name — paths come from the registry or parent entries — but the
/// generator writes it and cross-repo/submodule entries reference it.
pub const INDEX_FILENAME: &str = "pinakes.yaml";

/// Files we treat as documentation. mdx is included for Docusaurus repos.
const DOC_EXTENSIONS: &[&str] = &["md", "mdx", "rst", "txt", "adoc"];

/// Directories we never descend into.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".docusaurus",
    "__pycache__",
    ".venv",
    "venv",
    "coverage",
];

#[derive(Debug, Clone)]
pub struct GenerateOptions {
    /// Generate sub-indexes for each non-empty doc directory instead of
    /// flattening everything into the top-level index.
    pub recursive: bool,
    /// Also write generated indexes into nested directories. Without this we
    /// only print/return the top-level index for the root.
    pub write_nested: bool,
    /// Add a default `command_policy` block with a ripgrep allow-regex and a
    /// `command` entry for full-text search.
    pub include_search_command: bool,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            write_nested: true,
            include_search_command: true,
        }
    }
}

/// Inspect a checked-out repo and produce a `pinakes.yaml` for its root
/// (and, if `write_nested`, for each nested directory). Returns the YAML body
/// for the root index; caller decides whether to write it.
pub fn generate_index(repo_root: &Path, opts: &GenerateOptions) -> Result<String> {
    let canonical_root = repo_root.canonicalize()?;
    let repo_root = canonical_root.as_path();
    let root_index = build_index(repo_root, repo_root, opts)?;
    let yaml = serde_yaml::to_string(&root_index)
        .map_err(|e| AIexError::Other(format!("serializing generated index: {e}")))?;
    crate::index::Index::parse(&yaml, "generated root")?;
    if opts.write_nested && opts.recursive {
        write_nested_indexes(repo_root, repo_root, opts)?;
    }
    Ok(yaml)
}

fn build_index(repo_root: &Path, dir: &Path, opts: &GenerateOptions) -> Result<GeneratedIndex> {
    let rel = dir.strip_prefix(repo_root).unwrap_or(dir);
    let name = if rel.as_os_str().is_empty() {
        repo_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "index".into())
    } else {
        rel.to_string_lossy().into_owned()
    };

    let mut entries: Vec<GeneratedEntry> = Vec::new();
    let submodules = read_submodules(repo_root)?;

    let mut children: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        children.push(entry.path());
    }
    children.sort();

    for path in children {
        if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
            continue;
        }
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if file_name.starts_with('.') && file_name != ".gitmodules" {
            continue;
        }
        if SKIP_DIRS.contains(&file_name) {
            continue;
        }
        let rel_to_root = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            // Submodule? Reference it as a sub-index.
            if let Some(url) = submodules.get(rel_to_root.as_str()) {
                entries.push(GeneratedEntry {
                    id: Some(slugify(file_name)),
                    name: file_name.to_string(),
                    description: Some(format!("Submodule: {url}")),
                    kind: "index".into(),
                    // Path is relative to the *submodule's* repo root, not ours.
                    path: INDEX_FILENAME.into(),
                    repo: Some(GeneratedRepo {
                        url: url.clone(),
                        branch: None,
                    }),
                    argv: None,
                    success_exit_codes: None,
                });
                continue;
            }
            if !opts.recursive {
                continue;
            }
            // Skip empty dirs.
            if !directory_has_docs(&path) {
                continue;
            }
            entries.push(GeneratedEntry {
                id: Some(slugify(file_name)),
                name: file_name.to_string(),
                description: None,
                kind: "index".into(),
                path: format!("{rel_to_root}/{INDEX_FILENAME}"),
                repo: None,
                argv: None,
                success_exit_codes: None,
            });
        } else if is_doc_file(&path) {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file_name);
            entries.push(GeneratedEntry {
                id: Some(slugify(stem)),
                name: prettify(stem),
                description: None,
                kind: "doc".into(),
                path: rel_to_root,
                repo: None,
                argv: None,
                success_exit_codes: None,
            });
        }
    }

    let mut used_ids = HashSet::new();
    if opts.include_search_command && rel.as_os_str().is_empty() {
        used_ids.insert("search".to_string());
    }
    for entry in &mut entries {
        let base = entry
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or("entry")
            .to_string();
        let mut id = base.clone();
        let mut suffix = 2;
        while !used_ids.insert(id.clone()) {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        entry.id = Some(id);
    }
    let mut command_policy = None;
    if opts.include_search_command && rel.as_os_str().is_empty() {
        // Only attach a policy + search command at the root.
        command_policy = Some(GeneratedPolicy {
            allow: r#"^rg --max-count 20 -- .+ [.]$"#.into(),
            timeout_secs: 15,
        });
        entries.push(GeneratedEntry {
            id: Some("search".into()),
            name: "Full-text search".into(),
            description: Some("Search this directory recursively with ripgrep. Query is a Rust regular expression; words and spaces are literal, | means OR. No Boolean AND/OR operators.".into()),
            kind: "command".into(),
            path: String::new(),
            repo: None,
            argv: Some(vec![
                "rg".into(),
                "--max-count".into(),
                "20".into(),
                "--".into(),
                "{query}".into(),
                ".".into(),
            ]),
            success_exit_codes: Some(vec![0, 1]),
        });
    }

    Ok(GeneratedIndex {
        version: 1,
        name,
        description: None,
        command_policy,
        entries,
    })
}

fn write_nested_indexes(repo_root: &Path, dir: &Path, opts: &GenerateOptions) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !path.is_dir() || file_name.starts_with('.') || SKIP_DIRS.contains(&file_name) {
            continue;
        }
        // Don't walk into submodules; they manage their own indexes.
        let rel_to_root = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let submodules = read_submodules(repo_root)?;
        if submodules.contains_key(rel_to_root.as_str()) {
            continue;
        }
        if !directory_has_docs(&path) {
            continue;
        }
        let nested = build_index(repo_root, &path, opts)?;
        let body = serde_yaml::to_string(&nested)
            .map_err(|e| AIexError::Other(format!("serializing nested index: {e}")))?;
        crate::index::Index::parse(&body, &path.display().to_string())?;
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(repo_root) {
            return Err(AIexError::Other(
                "generated index directory escapes repository".into(),
            ));
        }
        let destination = canonical.join(INDEX_FILENAME);
        if std::fs::symlink_metadata(&destination).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(AIexError::Other(format!(
                "refusing to overwrite index symlink: {}",
                destination.display()
            )));
        }
        let mut output = tempfile::NamedTempFile::new_in(&canonical)?;
        output.write_all(body.as_bytes())?;
        output
            .persist(&destination)
            .map_err(|e| AIexError::Io(e.error))?;
        write_nested_indexes(repo_root, &path, opts)?;
    }
    Ok(())
}

fn read_submodules(repo_root: &Path) -> Result<BTreeMap<String, String>> {
    let path = repo_root.join(".gitmodules");
    let mut out = BTreeMap::new();
    if !path.exists() {
        return Ok(out);
    }
    if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(AIexError::Other(
            "refusing to read a symlinked .gitmodules".into(),
        ));
    }
    let body = std::fs::read_to_string(&path)?;
    let mut current_path: Option<String> = None;
    let mut current_url: Option<String> = None;
    let flush =
        |p: &mut Option<String>, u: &mut Option<String>, out: &mut BTreeMap<String, String>| {
            if let (Some(p), Some(u)) = (p.take(), u.take()) {
                out.insert(p, u);
            }
        };
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut current_path, &mut current_url, &mut out);
        } else if let Some(rest) = line.strip_prefix("path") {
            if let Some(v) = rest.split('=').nth(1) {
                current_path = Some(v.trim().to_string());
            }
        } else if let Some(rest) = line.strip_prefix("url") {
            if let Some(v) = rest.split('=').nth(1) {
                current_url = Some(v.trim().to_string());
            }
        }
    }
    flush(&mut current_path, &mut current_url, &mut out);
    Ok(out)
}

fn directory_has_docs(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        if e.file_type().map_or(true, |t| t.is_symlink()) {
            continue;
        }
        let p = e.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }
        if p.is_file() && is_doc_file(&p) {
            return true;
        }
        if p.is_dir() && directory_has_docs(&p) {
            return true;
        }
    }
    false
}

fn is_doc_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| DOC_EXTENSIONS.iter().any(|d| d.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

fn slugify(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_string()
}

fn prettify(stem: &str) -> String {
    // "0-understand-providers" -> "Understand providers"
    let stripped = stem.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '_');
    let cleaned = stripped.replace(['-', '_'], " ");
    let mut out = String::new();
    let mut chars = cleaned.chars();
    if let Some(c) = chars.next() {
        out.push(c.to_ascii_uppercase());
        out.extend(chars);
    }
    if out.is_empty() {
        return stem.to_string();
    }
    out
}

#[derive(Serialize)]
struct GeneratedIndex {
    version: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_policy: Option<GeneratedPolicy>,
    entries: Vec<GeneratedEntry>,
}

#[derive(Serialize)]
struct GeneratedPolicy {
    allow: String,
    timeout_secs: u64,
}

#[derive(Serialize)]
struct GeneratedEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<GeneratedRepo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success_exit_codes: Option<Vec<i32>>,
}

#[derive(Serialize)]
struct GeneratedRepo {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn generates_doc_and_nested_indexes() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("README.md"), "# r");
        write(&root.join("docs/getting-started.md"), "# gs");
        write(&root.join("docs/api/users.md"), "# u");

        let yaml = generate_index(root, &GenerateOptions::default()).unwrap();
        assert!(yaml.contains("readme"));
        assert!(yaml.contains("kind: index"));
        // nested index for docs/ written
        assert!(root.join("docs/pinakes.yaml").exists());
        assert!(root.join("docs/api/pinakes.yaml").exists());
    }

    #[test]
    fn submodule_dirs_become_index_entries_with_repo_url() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("README.md"), "# r");
        write(
            &root.join(".gitmodules"),
            r#"
[submodule "ext/foo"]
    path = ext/foo
    url = https://example.com/foo.git
"#,
        );
        std::fs::create_dir_all(root.join("ext/foo")).unwrap();
        write(&root.join("ext/foo/README.md"), "# foo");

        generate_index(root, &GenerateOptions::default()).unwrap();
        // The submodule lives at ext/foo, so the entry appears in ext/pinakes.yaml.
        let nested = std::fs::read_to_string(root.join("ext/pinakes.yaml")).unwrap();
        assert!(
            nested.contains("Submodule: https://example.com/foo.git"),
            "got: {nested}"
        );
        assert!(nested.contains("pinakes.yaml"));
    }
}
