# Reference

## CLI

Run aiexandria --help and aiexandria generate --help for installed options.
Running without a subcommand starts the MCP stdio server.

| Option | Environment variable | Purpose |
| --- | --- | --- |
| --registry PATH | AIEXANDRIA_REGISTRY | Select registry |
| --auth PATH | AIEXANDRIA_AUTH | Select HTTPS credentials |
| --cache PATH | AIEXANDRIA_CACHE | Choose cache |
| --refresh INTERVAL | AIEXANDRIA_REFRESH | Default refresh policy; default never |
| --version | — | Show version |

Options override their environment equivalents; registry entries can override
the global refresh policy. See [configuration](configuration.md).

## MCP tools

| Tool | Use |
| --- | --- |
| discover_sources | Rank metadata across configured index trees |
| list_indexes | List registry roots |
| list_docs | Browse one level or retrieve full source guidance |
| search_docs | Search a discovered source or explicit route |
| read_doc | Read a listed document |
| read_file | Read a repo-relative file using an entry's repository |
| read_url | Read a supported GitHub/GitLab/Codeberg-style file URL |

Complete argument schemas are advertised to the MCP client. Discovery defaults
to 5 results and 8192 bytes; maximums are 20 and 32768. Search accepts either a
source_id or an index/path/command_id route. Do not combine both forms.

Read tools accept 1-based offset and line-count limit. Without them, the whole
file may be returned. Indexed entries select repositories; they do not restrict
reads to listed files. read_url requires the repository to appear in the configured indexes.

## Acquisition controls in the next release

Search, read, and listing tools accept freshness (cached_ok by default, or require_fresh) and refresh_wait_ms (1–30000, default 5000). Expected acquisition delays return status=preparing with a retry delay. Ready responses include a snapshot token and freshness metadata for up to eight repositories, plus total and stale counts. The token covers all repositories used by the request.

read_doc, read_file, and read_url accept snapshot from a search. It lasts 15 minutes and preserves selected revisions while current routes and permissions are rechecked. Expiry requires another search. Multiple repositories do not form one transaction.

The new CLI options are --git PATH / AIEXANDRIA_GIT (Git 2.43.7 or newer), --repo-timeout DURATION / AIEXANDRIA_REPO_TIMEOUT (default 120s), and [aiexandria update](updating.md). These controls are not available in v0.3.0.

## Format

[Pinakes](https://github.com/HelloNoel/pinakes) defines the portable index format:
Git references, entry kinds, paths, and intersecting command policies. Discovery
ranking, defaults, cache, MCP names, and deadlines are AIexandria behavior, not
requirements imposed on every Pinakes consumer.

See the [Pinakes changelog](https://github.com/HelloNoel/pinakes/blob/main/CHANGELOG.md)
for revision history and compatibility requirements. Run `aiexandria --version`
to identify the revision supported by your build.
