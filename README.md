# AIexandria

An MCP server that gives LLMs hierarchical, sandboxed access to documentation
hosted in git repositories.

## Why

LLMs are bad at picking the right docs to consult. AIexandria lets a docs author
publish a YAML index alongside their docs in git — flat lists, nested
sub-indexes, merges of indexes from teammates, and even custom retrieval
commands (RAG, ripgrep, ...). The LLM walks the tree via a handful of MCP
tools and only fetches the leaves it actually needs.

## Concepts

An **index** is a YAML file with `entries`, each a `kind`:

- `doc` — a leaf text file
- `index` — a child index, resolved on demand
- `merge` — pulls another index's entries inline so multiple contributors
  can compose an index without each maintaining a complete one
- `command` — a sandboxed shell command (e.g. `rg "{query}" docs/`) for
  custom retrieval; `{query}` is substituted from the LLM's input

A `command_policy.allow` regex on an index gates what `command` entries
inside it may execute. The chain of allow-regexes from root to leaf is
*intersected* — children may narrow but never widen.

External indexes are addressed via a `repo:` block (url + branch/tag/commit).
Submodules are followed by default — when AIexandria clones or refreshes a
repo it recursively initializes git submodules, so a docs site that composes
content from multiple repos via submodules just works.

Auth tokens may be provided per-entry (`token_env`) or centrally
(`~/.config/aiexandria/auth.yaml` keyed by host); for submodule remotes the
central config is used automatically.

## Generating an index

If you don't want to write `pinakes.yaml` by hand, point the generator at
a checked-out repo or a git URL:

```sh
aiexandria generate --repo https://github.com/your/docs-repo --output pinakes.yaml
```

It walks the tree, treats markdown-ish files as `doc` entries and
subdirectories as `index` entries, turns submodule paths into `index`
entries that point at the submodule's *own* repo, and (with the default
`--no-nested` off) writes nested `pinakes.yaml` files into each
subdirectory. Treat the output as a starting point — add `description:`
fields to help the LLM pick, and drop entries that do not help navigation.
Referencing a repo authorizes repo-root-sandboxed reads from that checkout;
`doc` entries curate the navigation tree rather than form a file allow-list.

See [examples/crossplane-provider-docs.yaml](examples/crossplane-provider-docs.yaml)
for a real-world example produced this way.

## Running

See the [getting-started guide](https://hellonoel.codeberg.page/AIexandria/getting-started.html)
(or the [source](docs/getting-started.md)) for an end-to-end walkthrough that
installs AIexandria, publishes an index, and wires it into Claude Code as an
MCP server.

Quick version:

```sh
cargo install --path .
aiexandria \
    --registry ~/.config/aiexandria/registry.yaml \
    --refresh 1h
```

## Cache & refresh

Repos are cloned lazily into `~/.cache/aiexandria/repos/<sha8(url)>/<ref>/` the
first time a tool call touches them. They're refreshed *lazily* too: on each
request we look at the sentinel mtime in the checkout and only `git fetch` if
the configured interval has elapsed. There's no background scheduler.

- Global default via `--refresh` / `AIEXANDRIA_REFRESH` (humantime, e.g. `5m`,
  `1h`, `24h`, or `never` — the default).
- Per-entry override via `refresh:` in the registry YAML (see
  [examples/registry.example.yaml](examples/registry.example.yaml)).
- Pinned `commit:` refs are treated as immutable and never refreshed.

## MCP tools

- `list_indexes` — enumerate top-level indexes
- `list_docs` — list entries at a given path inside an index
- `read_doc` — fetch a `doc` entry's contents
- `read_file` — fetch any file from an entry's repo by path (e.g. to follow
  up on a `search_docs` hit); sandboxed to the repo root
- `read_url` — fetch an indexed repository file directly from a GitHub,
  Codeberg, Forgejo, or Gitea URL
- `search_docs` — substring search over names/descriptions (pass `deep: true`
  to also grep doc contents), or run a `command` entry with a query

## Layout

- [src/lib.rs](src/lib.rs) — public crate entrypoint
- [src/index.rs](src/index.rs) — YAML index schema
- [src/resolver.rs](src/resolver.rs) — recursive walk + merge flattening
- [src/cache.rs](src/cache.rs) — git2 shallow-clone cache + submodule init
- [src/auth.rs](src/auth.rs) — central + per-entry token resolution
- [src/command.rs](src/command.rs) — sandboxed `command` entry execution
- [src/generate.rs](src/generate.rs) — `aiexandria generate` index generator
- [src/registry.rs](src/registry.rs) — top-level registry config
- [src/server.rs](src/server.rs) — MCP server (rmcp)
- [src/main.rs](src/main.rs) — binary entrypoint (server + generate subcommand)
- [examples/](examples/) — sample registry, auth, and index files

## License

Apache-2.0. See [LICENSE](LICENSE).
