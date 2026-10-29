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
reads to listed files. read_url can acquire an unregistered public repository
from a supported file URL.

## Format

[Pinakes](https://github.com/HelloNoel/pinakes) defines the portable index format:
Git references, entry kinds, paths, and intersecting command policies. Discovery
ranking, defaults, cache, MCP names, and deadlines are AIexandria behavior, not
requirements imposed on every Pinakes consumer.
