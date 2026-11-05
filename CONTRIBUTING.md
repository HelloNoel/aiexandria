# Contributing

User guides belong in docs/. Portable format requirements belong in
[Pinakes](https://github.com/HelloNoel/pinakes). Public source additions belong in
[aiexandria-indexes](https://github.com/HelloNoel/aiexandria-indexes).

Before opening a pull request, run:

~~~sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
mdbook build
~~~

Add focused regression tests for behavior changes. CI covers Linux, macOS, and
Windows. Process and path changes need Windows checks.

## Discovery and cache architecture

catalog.rs builds metadata from index/merge routes without acquiring repositories
referenced only by documents or commands. discovery.rs ranks lexical metadata and
creates bounded temporary pagination snapshots. Default output is 5 results and
8192 bytes. Catalog acquisition has a 1.5-second foreground wait and reports
incomplete coverage explicitly.

Source IDs identify routes, not frozen permissions. Search re-resolves routes
under current definitions and intersected policies. One preparation deadline
(default 5 seconds, maximum 30) covers route resolution and repository acquisition.
Git work is shared across retries; expired preparation does not start a command.
Execution uses its separate inherited timeout.

Discovery ranking and content queries remain separate. Commands define their
query languages and outputs. Full guidance is available through list_docs.

## Ranked search architecture

The optional search subsystem lives in src/search/. AIexandria owns cached corpus membership, Git revision provenance and read routes. SQLite FTS5/BM25 and stored normalized vectors provide keyword, semantic and hybrid retrieval. Embeddings come from a configured HTTP provider; inference runtimes are not part of the server.

Scoped and global requests revalidate current authority before querying a consistent database snapshot. Global search filters and deduplicates the eligible union before ranking, with bounded work and output. It does not acquire missing Git content or synchronously build document embeddings. Preserve the legacy search path when mode is omitted.

Background indexing and update --semantic share embedding locks, incremental vector reuse and query priority. Corpus configuration is separate from Pinakes. Retrieval budget changes should not invalidate embeddings. Test exclusions, stale membership, aliases, partial vectors, deadlines and snapshot-bound follow-up reads when changing this subsystem.

## Current limits and direction

Prioritize useful evidence and easy maintenance. Discovery is lexical, with no general relevance threshold. Ranked passage search has result and byte budgets; legacy custom commands keep their own output limits. Neither semantic ranks nor hybrid scores prove that the corpus answers a question. Exact vector search is bounded, not an approximate index for arbitrary corpus sizes.

Evaluate larger collections and version-sensitive questions before adding ANN, reranking or cross-backend federation. Measure useful reads, output size, incomplete coverage, clone/indexing cost, provider startup and version mismatches. A small local benchmark is not an adoption or general quality result.

Command policies are not OS isolation. The Git CLI backend supports SHA-1 and SHA-256 repositories and commit pins. Keep schemas, shared fixtures and repository-aware validation aligned; record portable format changes in the [Pinakes changelog](https://github.com/HelloNoel/pinakes/blob/main/CHANGELOG.md).

## Releases

Prepare the package/lockfile versions, changelog, release-notes.md, README/download links, installation examples, explicit help refs and Pages dispatch default together. Built-in help automatically targets the package version. Keep the changelog entry marked Unreleased until publication is ready, then record the release date.

Run the checks above and review the diff before committing. Release binaries are built by GitHub Actions when a signed vX.Y.Z tag is pushed; ordinary branch pushes run CI without rebuilding release archives. The tag must match Cargo.toml. The release matrix tests and packages all five targets, with fail-fast disabled so one failure does not cancel the remaining platform checks.

After all builds succeed, the workflow creates a draft release and uploads archives plus SHA256SUMS. Review the notes, checksums and native Windows results before publishing. Publishing deploys the user guide from that tag. Resolve failed platform checks before publishing the draft.

A source build needs Rust 1.89 or newer and a native C toolchain for SQLite/TLS. Tests also use git2 fixtures with vendored OpenSSL, which can require Perl and make. See .github/workflows/release.yml for the Windows GNU toolchain setup.

## Repository backend

Production uses Git CLI (minimum 2.43.7); git2 is a development dependency for older fixtures. cache.rs publishes immutable snapshots. cache/build.rs stages private Git metadata and shares immutable object files; cache/scheduler.rs coordinates four acquisition slots, at most two background, across processes. The update command uses the same queue and repository locks.

Keep repository I/O off the async MCP runtime. Request deadlines stop waiting; backend deadlines bound Git processes. Updaters cancel only their own jobs. Shared leases protect active readers, and follow-up tokens expire after 15 minutes. Commands take exclusive prepared copies; timed-out requests leave copy preparation available for retry. Disk copies have a separate two-worker limit. Commands execute in independent temporary trees: never hardlink mutable command files to published snapshots.

Run tests/cache_cli.rs with both baseline and current Git. It covers SHA-256, selective hydration, snapshot consistency, process coordination, credentials, and cancellation. Some regressions need a local listening socket and process inspection, which may require sandbox permission.
