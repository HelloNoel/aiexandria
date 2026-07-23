# Getting started with AIexandria + Claude Code

This walks you through installing AIexandria, publishing an index alongside
some docs, and wiring it into Claude Code as an MCP server.

## 1. Install AIexandria

From source (only option today):

```sh
git clone https://codeberg.org/HelloNoel/AIexandria.git aiexandria
cd aiexandria
cargo install --path .
```

This drops `aiexandria` into `~/.cargo/bin`. Verify:

```sh
aiexandria --help
```

## 2. Publish an index in a docs repo

AIexandria reads a YAML index from any git repo you point it at. You have
two ways to author it.

### Option A: generate it

Point the generator at the repo (local path or git URL):

```sh
aiexandria generate \
    --repo https://github.com/me/my-project \
    --output pinakes.yaml
```

This walks the tree, makes a `doc` entry for every markdown-ish file, an
`index` entry for every non-empty subdirectory (with a nested
`pinakes.yaml` written alongside), and an `index` entry pointing at the
submodule's *own* repo for each git submodule. It also adds a default
ripgrep `command_policy` plus a search command. Curate the output: drop
anything you don't want exposed, and add `description:` fields so the LLM
can pick well.

### Option B: write it by hand

Drop one into the root of a repo you want Claude to be able to read:

```yaml
# pinakes.yaml
version: 1
name: my-project
description: "Internal docs for my-project"

# Allow `command` entries to run ripgrep, scoped to docs/.
command_policy:
  allow: '^rg( -[A-Za-z]+)* [^;&|`$]+ docs/'
  timeout_secs: 10

entries:
  - id: getting-started
    name: Getting started
    description: "Install + run my-project locally"
    kind: doc
    path: docs/getting-started.md

  - id: api
    name: API reference
    description: "REST endpoints and error codes"
    kind: index
    path: docs/api/index.yaml

  - id: search
    name: Full-text search
    description: "Substring search over docs/. Pass a query."
    kind: command
    argv: ["rg", "--max-count", "20", "{query}", "docs/"]
```

Commit and push. AIexandria will clone this repo on demand the first time
Claude reaches into it. If your repo uses git submodules to compose docs
from elsewhere, AIexandria initializes them recursively on clone — your
top-level index can either reference the submodule path directly (as a
`doc`) or use an `index` entry with its own `repo:` block to descend into
the linked repo's `pinakes.yaml`.

The four entry kinds you can use:

| `kind:` | Purpose |
| --- | --- |
| `doc` | A leaf text file Claude can read |
| `index` | A child `pinakes.yaml` (same repo or a different one) |
| `merge` | Inline another index's entries here — for shared/composed indexes |
| `command` | A sandboxed shell command (RAG, ripgrep, ...). `{query}` is replaced with Claude's input |

`command` entries are gated by the `command_policy.allow` regex on the
nearest enclosing index. The chain of regexes from root to leaf is
*intersected* — children narrow but never widen.

## 3. Create the local registry

The registry tells your AIexandria instance which top-level indexes exist.
Create it anywhere you like:

```yaml
# ~/.config/aiexandria/registry.yaml
indexes:
  my-project:
    description: "My internal project docs"
    repo:
      url: git@github.com:me/my-project.git
      branch: main
    path: pinakes.yaml
    refresh: 1h     # re-fetch at most once an hour

  rust-book:
    description: "The Rust Programming Language book"
    repo:
      url: https://github.com/rust-lang/book.git
      branch: main
    path: pinakes.yaml
    refresh: 24h
```

`refresh:` accepts humantime (`5m`, `1h`, `24h`) or `never`. Repos are cloned
lazily on first use and re-fetched lazily after the interval elapses — there's
no background process.

## 4. (Private repos only) Configure auth

For private repos over HTTPS, drop a token config at the default path:

```yaml
# ~/.config/aiexandria/auth.yaml
hosts:
  github.com:
    token_env: GITHUB_TOKEN
  gitlab.example.com:
    token_env: GITLAB_TOKEN
    username: oauth2
```

Then export the token in the environment Claude Code runs in:

```sh
export GITHUB_TOKEN=ghp_xxx
```

For SSH URLs (`git@host:org/repo.git`), AIexandria uses `ssh-agent` — make
sure `ssh-add` has loaded your key in the same shell that launches Claude
Code.

## 5. Wire it into Claude Code

Register AIexandria as an MCP server. The simplest route is the `claude` CLI:

```sh
claude mcp add AIexandria \
    --scope user \
    -- aiexandria --registry ~/.config/aiexandria/registry.yaml --refresh 1h
```

Or add it manually to your settings — `~/.claude.json` (user scope) or
`.mcp.json` in a project:

```json
{
  "mcpServers": {
    "AIexandria": {
      "command": "aiexandria",
      "args": [
        "--registry", "/Users/you/.config/aiexandria/registry.yaml",
        "--refresh", "1h"
      ],
      "env": {
        "GITHUB_TOKEN": "ghp_xxx"
      }
    }
  }
}
```

Restart Claude Code (or run `/mcp` to reload).

## 6. Try it

In a Claude Code session, ask something like:

> Use AIexandria to find the docs for our auth flow.

Claude will call:

1. `list_indexes` — to see what's available
2. `list_docs` with `index: "my-project"` — to see the entries
3. `read_doc` for the relevant leaf, or `search_docs` (optionally `deep: true`,
   or with `command_id: "search"` to run your `rg` command); follow up on a
   search hit with `read_file` to fetch the whole file

You can confirm the server is healthy with `/mcp` inside Claude Code; it
should show `AIexandria` as connected with its tools listed.

## Troubleshooting

- **"unknown index 'foo'"** — the name in your prompt doesn't match a key in
  `registry.yaml`. Check `list_indexes`.
- **"command '...' not allowed by regex ..."** — your `command` entry is
  blocked by the `command_policy.allow` chain. Either widen the regex on the
  index or change the command. Children can only *narrow* the parent's
  regex; they cannot widen it.
- **Auth failures** — confirm the env var is set in the shell that launches
  Claude Code (not just your terminal). For SSH, confirm `ssh-add -l` lists
  your key.
- **Stale docs** — lower the `refresh:` on that registry entry, or delete the
  cache directory: `rm -rf ~/.cache/aiexandria/repos`.
- **Inspect raw MCP traffic** — run AIexandria once by hand with
  `RUST_LOG=debug` and pipe a JSON-RPC `initialize` message into stdin to
  confirm it responds.
