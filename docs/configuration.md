# Configuration and troubleshooting

## Registry selection

AIexandria uses the first applicable registry:

1. --registry PATH, or AIEXANDRIA_REGISTRY when the option is omitted.
2. $XDG_CONFIG_HOME/aiexandria/registry.yaml, otherwise ~/.config/aiexandria/registry.yaml.
3. The built-in public collection and help for the installed release.

A missing explicitly requested file, invalid existing file, or broken symlink is
an error. No configuration file is created automatically. Existing custom
registries are never silently combined with the built-in collection.

## Authentication

Public defaults need no token. For private HTTPS repositories, reference an
environment variable in a central auth file:

~~~yaml
hosts:
  github.com:
    token_env: TEAM_DOCS_TOKEN
    username: git
~~~

Save it as $XDG_CONFIG_HOME/aiexandria/auth.yaml or ~/.config/aiexandria/auth.yaml,
or choose a file with --auth / AIEXANDRIA_AUTH. Ensure the MCP client passes the
variable to the server. A repository's token_env can override the host mapping.
Never commit tokens.

SSH repositories use your SSH agent. Load the key for the intended account. Git
access uses libgit2; command-line Git may have a different authentication setup.

## Cache and refresh

Choose a cache with --cache PATH or AIEXANDRIA_CACHE. Defaults:

| System | Directory |
| --- | --- |
| Linux | $XDG_CACHE_HOME/aiexandria/repos, otherwise ~/.cache/aiexandria/repos |
| macOS | ~/Library/Caches/aiexandria/repos |
| Windows | %LOCALAPPDATA%/aiexandria/repos |

Repositories are cloned lazily. Discovery acquires repositories containing index
metadata, but not those referenced only by documents or commands. The first read
or deep search can still need a large clone.

Set refresh: 1h, 24h, or never on a registry entry. It overrides --refresh /
AIEXANDRIA_REFRESH, whose default is never. Public defaults refresh after 24h;
release help uses never. Commit pins are immutable. Refresh happens when next
needed after the interval, not by periodic background polling.

## Common problems

**Server not found:** use an absolute executable path and check it with --version.
Restart the client after configuration changes or replacing the executable.

**Discovery incomplete:** retry after catalog acquisition finishes. Check reported
failures before interpreting an empty response as absence of documentation.

**Search returns refresh_pending:** cloning or refresh exceeded its preparation
deadline. Work continues in the background. Retry after the indicated delay, or
set refresh_wait_ms up to 30000. A refresh_failed error means acquisition failed;
check connectivity, refs, and credentials. Command execution has a separate timeout.
Read tools have their own acquisition behavior and can use cached content after
refresh failures; do not assume every read fetched the latest text.

**Search finds nothing:** built-in search uses a case-insensitive substring, not
Boolean expressions. Select the right source, try a distinctive term, and enable
deep: true to search listed current-level documents. Commands can use other query
languages; follow their descriptions.

**SHA-256 repository fails:** this release's libgit2 dependency does not support
SHA-256 Git repositories. Use a SHA-1 source for now. Native support remains
planned when supported Rust bindings become available. Pinakes v1 commit pins
use SHA-1 IDs (7–40 hexadecimal characters).

Set RUST_LOG=aiexandria=debug for diagnostic logs on stderr. Review logs for
private repository information before sharing them.
