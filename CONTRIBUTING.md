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

## Current limits and direction

Prioritize source selection, useful excerpts, and easy maintenance. Discovery is
lexical with no general relevance threshold. Search has no shared output budget;
deep search covers listed current-level documents. Command policy is not OS
isolation. The Git CLI backend supports SHA-1 and SHA-256 repositories and commit
pins. Keep schemas, shared fixtures, and repository-aware validation aligned;
record format changes in the [Pinakes changelog](https://github.com/HelloNoel/pinakes/blob/main/CHANGELOG.md).

Before adding a crawler or vector store, evaluate an adapter to an existing
retriever through a command entry. Measure source-selection queries and real
reads, output size, incomplete coverage, clone cost, and version mismatches.

For a four-week user pilot, collect setup failures, grounded reads, wrong-source
selections, and time to add or repair an index. These are evaluation criteria,
not completed adoption results.

## Releases

Update package/lockfile versions, changelog, installation examples, and help refs
together. Push a signed vX.Y.Z tag to build archives and checksums. Review CI and
draft assets before publishing. The published release deploys the user guide.

## Repository backend

Production uses Git CLI (minimum 2.43.7); git2 is a development dependency for older fixtures. cache.rs publishes immutable snapshots. cache/build.rs stages private Git metadata and shares immutable object files; cache/scheduler.rs coordinates four acquisition slots, at most two background, across processes. The update command uses the same queue and repository locks.

Keep repository I/O off the async MCP runtime. Request deadlines stop waiting; backend deadlines bound Git processes. Updaters cancel only their own jobs. Shared leases protect active readers, and follow-up tokens expire after 15 minutes. Commands take exclusive prepared copies; timed-out requests leave copy preparation available for retry. Disk copies have a separate two-worker limit. Commands execute in independent temporary trees: never hardlink mutable command files to published snapshots.

Run tests/cache_cli.rs with both baseline and current Git. It covers SHA-256, selective hydration, snapshot consistency, process coordination, credentials, and cancellation. Some regressions need a local listening socket and process inspection, which may require sandbox permission.
