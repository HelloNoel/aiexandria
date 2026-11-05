# Changelog

## 0.4.0 — 2026-11-05

- Include document, listing, and legacy search output in structured MCP
  responses so clients receive content alongside status metadata. Preserve error
  messages in structured failures.
- Accept Git version strings with Apple and Windows vendor labels while
  retaining the Git 2.43.7 minimum.
- Convert canonical Windows cache paths for Git when cloning repositories.
- Keep cache line endings independent of the caller's Git configuration.

- Recognize `.markdown` files, including uppercase extensions, in automatic
  search indexing and generated indexes.

- Adopt Pinakes revision 2026-11-02: omitted `repo.submodules` now defaults to
  `false` in indexes and registries. Preserve explicit options in old cache
  records. Remote generation now requires `--submodules` to initialize nested
  repositories.

- Continue indexing available sources after a repository acquisition fails, and
  identify the affected source in update errors.

- Add `update --all` to prepare uncached sources across configured indexes or a
  selected subtree, optionally with `--semantic`; ordinary updates retain their
  existing scope.

- Add global ranked search across indexed cached documentation, with
  index/source filters, deduplicated passages, per-hit source routes, explicit
  coverage and bounded work.

- Add opt-in ranked passage search over cached documentation: SQLite keyword
  search, local embedding providers, semantic and hybrid modes, bounded results
  and revision-bound reads.
- Discover generated cached repository sources and index available documentation
  in the background or with update --semantic [--cached-only]; preserve custom
  commands and legacy searches.

- Use the Git CLI (2.43.7 or newer) for repository access; support SHA-1 and
  SHA-256 branches, tags, and commit pins.
- Resolve commit pins as object IDs, reject non-commit objects, and retain
  resolved pins during forced updates and selective hydration. See the
  [Pinakes changelog](https://github.com/HelloNoel/pinakes/blob/main/CHANGELOG.md)
  for format compatibility.
- Keep cached searches and reads available while refreshes run in the
  background. Return normal preparing responses for cold acquisitions and
  snapshot tokens for consistent follow-up reads.
- Add aiexandria update with shared process locks, bounded acquisition
  concurrency, interactive priority, and manual/scheduled prewarming.
- Fetch and materialize explicit document paths selectively. Preserve full
  command/submodule checkouts and run commands in independent copies.

### Upgrade notes

- Add explicit `submodules: true` to indexes and registries that rely on nested
  documentation before upgrading. Producers targeting older consumers should
  continue to emit an explicit boolean.

- Install Git 2.43.7 or newer and make it available to the MCP process, or
  select it with --git / AIEXANDRIA_GIT. Release binaries now use the Git CLI
  instead of libgit2.
- Existing registries and custom-command query languages are preserved. Ranked
  search requires an enabled search.yaml; no model or embedding service is
  installed automatically.
- The cache layout changes. Frequently used sources can be prepared with
  aiexandria update; older caches remain available for rollback.
- Global ranked search covers indexed cached documentation only. Missing
  metadata, pending embeddings and work limits are reported explicitly.

## 0.3.0 — 2026-10-29

- Add global metadata discovery with bounded results and explicit coverage
  status.
- Bound search preparation with a refresh deadline and retryable pending status.
- Provide a 20-topic public starter collection and release-specific help when no
  registry is configured.
- Preserve explicit and existing registry precedence; report invalid
  configurations.
- Rewrite user guides and move CI, releases, and Pages to GitHub.
- Preserve and re-sign converted SHA-1 history for GitHub compatibility.

Existing custom registries are unchanged. SHA-256 repositories remain
unsupported.

## 0.2.0 — 2026-10-27

### Added

- Read files directly from supported repository web URLs with the MCP read_url
  tool.
- Page document and file reads with offset and limit.
- Enable SSH Git transport support.
- Provide a Linux ARM64 release binary alongside macOS ARM64/Intel, Linux
  x86-64, and Windows x86-64.

### Fixed

- Prevent search queries from being interpreted as ripgrep options.
- Preserve command restrictions and refresh settings across merges and nested
  indexes.
- Detect index cycles across lazy navigation and mixed index/merge paths.
- Separate repository caches by exact Git reference and checkout options;
  publish clones only after successful initialization.
- Resolve historical and abbreviated commit pins, and verify cached pinned
  checkouts.
- Avoid libgit2's unsafe cleanup path when a submodule clone fails; handle
  relative local submodule URLs correctly.
- Report missing configured credentials instead of silently falling back.
- Keep command deadlines active while collecting output, manage descendant
  processes, and bound stdout/stderr without splitting UTF-8 characters.
- Prevent index generation through symlinks and generate unique entry IDs.
- Align runtime validation with the Pinakes specification and schemas.
- Generate a valid Windows file URL in the repository-discovery test fixture.

### Upgrade notes

- Repository caches now use a versioned layout. The first access clones
  repositories again; old cache directories are retained.
- Validation rejects malformed indexes and registries, including unknown fields,
  explicit nulls, empty required values, conflicting Git refs, and invalid
  command settings. Existing files that relied on permissive parsing may need
  updates.
- Command allow patterns match expanded arguments joined with single spaces, as
  specified by Pinakes. Patterns that expected shell quoting may need updates.
- Command policies control execution but do not provide OS isolation. Register
  command-bearing indexes only when you trust their authors and updates, or
  supply your own OS sandbox.
- Windows builds were exercised under Wine; native Windows verification is still
  outstanding. SHA-256 Git repositories remain unsupported.
