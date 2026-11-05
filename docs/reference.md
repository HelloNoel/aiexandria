# Reference

## CLI

Running without a subcommand starts the MCP stdio server. Use
`aiexandria --help`, `aiexandria update --help` and `aiexandria generate --help`
for installed options.

| Option                    | Environment variable       | Purpose                                                |
| ------------------------- | -------------------------- | ------------------------------------------------------ |
| `--registry PATH`         | `AIEXANDRIA_REGISTRY`      | Select the registry                                    |
| `--auth PATH`             | `AIEXANDRIA_AUTH`          | Select HTTPS credentials                               |
| `--cache PATH`            | `AIEXANDRIA_CACHE`         | Choose the repository cache                            |
| `--refresh INTERVAL`      | `AIEXANDRIA_REFRESH`       | Default refresh policy; default `never`                |
| `--git PATH`              | `AIEXANDRIA_GIT`           | Select Git 2.43.7 or newer                             |
| `--repo-timeout DURATION` | `AIEXANDRIA_REPO_TIMEOUT`  | Bound a Git operation; default `120s`                  |
| `--search-config PATH`    | `AIEXANDRIA_SEARCH_CONFIG` | Select optional ranked-search configuration            |
| `--version`               | —                          | Show the server version and supported Pinakes revision |

Options override their environment equivalents; registry entries override the
global refresh policy. See [configuration](configuration.md).

[Repository updates](updating.md) can prepare one index/path or refresh cached
sources. Add `--all` to include uncached repositories within the selected scope;
it respects refresh intervals and does not execute custom commands. Add
`--semantic` to update the passage index, and `--cached-only` to skip Git work.
Those last two flags require ranked search to be enabled. `--cached-only` cannot
be combined with `--force` or `--all`.

`generate` also leaves remote submodules uninitialized by default; pass
`--submodules` to opt in. See [writing indexes](writing-indexes.md).

## MCP tools

| Tool               | Use                                                                  |
| ------------------ | -------------------------------------------------------------------- |
| `discover_sources` | Rank metadata across configured index trees                          |
| `list_indexes`     | List registry roots                                                  |
| `list_docs`        | Browse one level or retrieve full source guidance                    |
| `search_docs`      | Search a source, or search globally when ranked search is configured |
| `read_doc`         | Read a listed document                                               |
| `read_file`        | Read a repo-relative file using an entry's repository                |
| `read_url`         | Read a supported GitHub/GitLab/Codeberg-style file URL               |

Complete argument schemas are advertised to the MCP client. Discovery defaults
to five results and 8,192 bytes; maximums are 20 and 32,768.

Successful reads, `list_docs`, and substring/custom-command searches include
their content blocks in `structuredContent.content`, alongside status and
snapshot metadata. The same blocks remain in the MCP `content` array, so clients
can use either representation. Failed acquisition or command responses include
`status: failed` and the error `message` in structured content.

Read tools accept a 1-based `offset` and line-count `limit`. Without them, a
whole file may be returned. Indexed entries select repositories; they do not
restrict reads to listed files. `read_url` requires the repository to appear in
the configured indexes.

## Search modes and scope

A source search accepts either `source_id` or an `index`/`path`/`command_id`
route. Do not combine those forms.

| Request                                 | Behavior                                                                                       |
| --------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Omit `mode` on an existing source route | Preserve substring or custom-command search; `deep: true` reads listed current-level documents |
| `mode: keyword`                         | Rank passages by word matches with SQLite FTS5/BM25                                            |
| `mode: semantic`                        | Rank by embedding similarity; requires an embedding provider                                   |
| `mode: hybrid`                          | Combine keyword and vector rankings, falling back to keyword when needed                       |

The three ranked modes require [search configuration](semantic-search.md). They
accept `limit` (1–20, default 5) and `max_output_bytes` (1024–32768, default
8192). Discovery advertises `search_modes` and readiness. Generated cached
repository sources have a separate `source_id` and select hybrid by default with
a provider, keyword otherwise. Their file scope is independent of custom
commands.

Use `scope: global` to rank the union of eligible indexed cached documentation.
Optional `indexes` and `source_ids` arrays restrict the search, with up to 64
entries each. Global requests cannot be combined with singular source routes,
`deep` or `require_fresh`. They perform no Git work or custom-command execution.
See [global search](semantic-search.md#search-globally) for work limits, missing
coverage and per-hit source routes.

Ranked responses identify `mode_used`, semantic readiness, coverage and
truncation. Use each hit's `read` route, file and line range with the returned
snapshot token to read the evidence. Rankings are candidates, not confidence
estimates.

## Acquisition and snapshots

Search, read and listing tools accept `freshness` (`cached_ok` by default, or
`require_fresh`) and `refresh_wait_ms` (1–30000, default 5000). Expected
acquisition delays return `status: preparing` and a retry delay. Global search
uses `cached_ok`; its preparation budget covers cached inventory validation, and
its query execution has a separate configurable deadline.

Ready responses include a `snapshot` token and freshness metadata. Source
requests list up to eight repositories plus total/stale counts; global searches
return aggregate counts. The token covers all repositories used by the request.
`read_doc`, `read_file` and `read_url` accept it to retain the revision for up
to 15 minutes, while checking current routes and permissions. Snapshots across
different repositories do not form one transaction.

## Format

[Pinakes](https://github.com/HelloNoel/pinakes) defines the portable index
format: Git references, entry kinds, paths and intersecting command policies.
AIexandria supports SHA-1 and SHA-256 commit pins.

Discovery ranking, embedding providers, cache behavior, MCP tools and deadlines
are AIexandria features, not requirements imposed on every Pinakes consumer. See
the
[Pinakes changelog](https://github.com/HelloNoel/pinakes/blob/main/CHANGELOG.md)
for revision history and compatibility requirements. Run `aiexandria --version`
to identify the revision supported by your build.
