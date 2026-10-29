# Changelog

## 0.3.0 — 2026-10-29

- Add global metadata discovery with bounded results and explicit coverage status.
- Bound search preparation with a refresh deadline and retryable pending status.
- Provide a 20-topic public starter collection and release-specific help when no registry is configured.
- Preserve explicit and existing registry precedence; report invalid configurations.
- Rewrite user guides and move CI, releases, and Pages to GitHub.
- Preserve and re-sign converted SHA-1 history for GitHub compatibility.

Existing custom registries are unchanged. SHA-256 repositories remain unsupported.

## 0.2.0 — 2026-10-27

### Added

- Read files directly from supported repository web URLs with the MCP read_url tool.
- Page document and file reads with offset and limit.
- Enable SSH Git transport support.
- Provide a Linux ARM64 release binary alongside macOS ARM64/Intel, Linux x86-64, and Windows x86-64.

### Fixed

- Prevent search queries from being interpreted as ripgrep options.
- Preserve command restrictions and refresh settings across merges and nested indexes.
- Detect index cycles across lazy navigation and mixed index/merge paths.
- Separate repository caches by exact Git reference and checkout options; publish clones only after successful initialization.
- Resolve historical and abbreviated commit pins, and verify cached pinned checkouts.
- Avoid libgit2's unsafe cleanup path when a submodule clone fails; handle relative local submodule URLs correctly.
- Report missing configured credentials instead of silently falling back.
- Keep command deadlines active while collecting output, manage descendant processes, and bound stdout/stderr without splitting UTF-8 characters.
- Prevent index generation through symlinks and generate unique entry IDs.
- Align runtime validation with the Pinakes specification and schemas.
- Generate a valid Windows file URL in the repository-discovery test fixture.

### Upgrade notes

- Repository caches now use a versioned layout. The first access clones repositories again; old cache directories are retained.
- Validation rejects malformed indexes and registries, including unknown fields, explicit nulls, empty required values, conflicting Git refs, and invalid command settings. Existing files that relied on permissive parsing may need updates.
- Command allow patterns match expanded arguments joined with single spaces, as specified by Pinakes. Patterns that expected shell quoting may need updates.
- Command policies control execution but do not provide OS isolation. Register command-bearing indexes only when you trust their authors and updates, or supply your own OS sandbox.
- Windows builds were exercised under Wine; native Windows verification is still outstanding. SHA-256 Git repositories remain unsupported.
