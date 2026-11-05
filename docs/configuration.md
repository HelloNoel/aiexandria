# Configuration and troubleshooting

AIexandria v0.4.0 requires Git 2.43.7 or newer. Existing registries continue to
work; ranked search is configured separately and is optional.

## Registry selection

AIexandria uses the first applicable registry:

1. --registry PATH, or AIEXANDRIA_REGISTRY when the option is omitted.
2. $XDG_CONFIG_HOME/aiexandria/registry.yaml, otherwise
   ~/.config/aiexandria/registry.yaml.
3. The built-in public collection and help for the installed release.

A missing explicitly requested file, invalid existing file, or broken symlink is
an error. No configuration file is created automatically. Existing custom
registries are never silently combined with the built-in collection.

## Authentication

Public defaults need no token. For private HTTPS repositories, reference an
environment variable in a central auth file:

```yaml
hosts:
  github.com:
    token_env: TEAM_DOCS_TOKEN
    username: git
```

Save it as $XDG_CONFIG_HOME/aiexandria/auth.yaml or
~/.config/aiexandria/auth.yaml, or choose a file with --auth / AIEXANDRIA_AUTH.
Ensure the MCP client passes the variable to the server. A repository's
token_env can override the host mapping. Never commit tokens.

Git CLI handles repository access. SSH uses the identity selected by your SSH
configuration. HTTPS uses a repository's explicit token, then the central host
mapping, then normal Git credential helpers. Missing explicit credentials remain
errors even when content is cached. Prompts are disabled; scheduled jobs need
noninteractive credentials. AIexandria does not change global Git configuration.

## Cache and refresh

Choose a cache with --cache PATH or AIEXANDRIA_CACHE. Defaults:

| System  | Directory                                                             |
| ------- | --------------------------------------------------------------------- |
| Linux   | $XDG_CACHE_HOME/aiexandria/repos, otherwise ~/.cache/aiexandria/repos |
| macOS   | ~/Library/Caches/aiexandria/repos                                     |
| Windows | %LOCALAPPDATA%/aiexandria/repos                                       |

Repositories are cloned lazily. Discovery acquires repositories containing index
metadata, but not those referenced only by documents or commands. The first read
or deep search can still need a large clone.

Set refresh: 1h, 24h, or never on a registry entry. It overrides --refresh /
AIEXANDRIA_REFRESH, whose default is never. Public defaults refresh after 24h;
release help uses never. Commit pins are immutable. Refresh happens when next
needed after the interval, not by periodic background polling.

## Submodule documentation

Submodule downloads are disabled by default. If documentation lives in nested
Git repositories, enable them in the relevant index entry or registry root:

```yaml
repo:
  url: https://github.com/YOUR-TEAM/docs.git
  branch: main
  submodules: true
```

This initializes submodules recursively and can download additional
repositories. It also applies when preparing a custom-command source or running
`update --all`. Entries without their own `repo:` use their enclosing index's
repository settings.

When upgrading from an earlier default, add explicit `submodules: true` to
sources that rely on nested documentation. Explicit `false` remains useful when
sharing an index with older consumers. Existing caches keep their recorded
options; a reference whose effective setting changes uses a separate cache.

## Common problems

**Git reports a submodule without a URL:** the upstream repository contains a
gitlink that has no matching URL in `.gitmodules`. Fix that repository, or set
`submodules: false` in its Pinakes `repo:` blocks if the documentation you need
is stored in the parent repository. This excludes all submodule content for
those references; AIexandria does not silently skip required submodules. An
update reports the affected source as failed while still indexing other
available sources when time remains.

**Server not found:** use an absolute executable path and check it with
--version. Restart the client after configuration changes or replacing the
executable.

**Discovery incomplete:** retry after catalog acquisition finishes. Check
reported failures before interpreting an empty response as absence of
documentation.

**Search returns preparing:** the requested content is not ready yet. This is a
normal tool response; no search ran. Retry after the suggested delay a bounded
number of times, or prewarm the source with [aiexandria update](updating.md).
Cached searches return available content while due refreshes run in the
background. Use freshness=require_fresh only when a fresh scheduled check
matters. refresh_wait_ms accepts 1–30000 milliseconds (default 5000); command
execution has a separate timeout.

Failed acquisitions return explicit tool errors. Cached content remains usable
after a refresh failure, with freshness metadata. Pending preparation is not an
empty search.

**Search finds nothing:** built-in search uses a case-insensitive substring, not
Boolean expressions. Select the right source, try a distinctive term, and enable
deep: true to search listed current-level documents. Commands can use other
query languages; follow their descriptions.

**Git missing or too old:** install Git 2.43.7 or newer and make it available on
the MCP process's PATH, or set --git /absolute/path/to/git / AIEXANDRIA_GIT. A
GUI client's PATH may differ from your terminal's.

**SHA-256 repository fails:** check that AIexandria reports v0.4.0 or newer and
that the MCP process uses Git 2.43.7 or newer. Use a full 64-character SHA-256
commit ID or an unambiguous hexadecimal prefix of at least seven characters.
SHA-1 IDs remain 40 characters, with the same minimum prefix length. A pin must
identify a commit in that repository; branches or tags with hexadecimal names do
not override it. Prefer full IDs when sharing indexes.

Set RUST_LOG=aiexandria=debug for diagnostic logs on stderr. Review logs for
private repository information before sharing them.

Explicit document and index paths use selective checkout and partial clone when
supported by the server. Required submodules and command searches receive
complete checkouts. Hooks and checkout filters, including Git LFS smudging, are
disabled; LFS pointers are not expanded. Commands run in temporary copies so
writes cannot corrupt shared snapshots.

The new cache layout preserves older caches for rollback. Current and previous
snapshots, active reads, and unexpired follow-up tokens are retained; older
unused generations are cleaned during publication. Do not edit cache checkouts
by hand. See [repository updates](updating.md) for prewarming and scheduling.

For ranked searches, also check `mode_used` and corpus coverage. A global search
only covers indexed cached documentation; use discovery and a scoped update to
prepare missing sources. See [semantic search](semantic-search.md) for provider
errors, fallback behavior and work limits.

## Optional ranked search

Use a separate search.yaml configuration for keyword, semantic and hybrid
passage retrieval. It is disabled unless explicitly enabled. See
[semantic search](semantic-search.md) for local providers, corpus filters,
limits and incremental updates. This configuration does not change the Pinakes
format.
