# AIexandria v0.4.0

Search documentation across repositories, keep using cached evidence during refreshes, and prepare frequently used sources before a session. AIexandria v0.4.0 adds optional ranked passage search and moves repository access to the Git CLI, including support for SHA-256 repositories.

## Highlights

- **Keyword, semantic and hybrid search** — opt-in passage retrieval with bounded results, file/line references and explicit keyword fallback. Use a local Ollama or OpenAI-compatible embedding server; no model is bundled or downloaded automatically.
- **Global ranked search** — search indexed cached documentation across configured indexes, with optional index/source filters, duplicate removal, source routes and coverage reporting. Queries do not fetch repositories or execute custom commands.
- **Search during refresh** — complete cached snapshots remain usable while due refreshes run in the background. Missing content returns a normal preparing response instead of a refresh-timeout tool error.
- **Prepare ahead of time** — `aiexandria update` supports manual and scheduled preparation. MCP requests and concurrent updaters share repository work; interactive requests can promote queued jobs. Add `--semantic` to index available documentation, or `--semantic --cached-only` to skip Git work. Add `--all` to include uncached sources across configured indexes or a selected subtree.
- **Consistent follow-up reads** — pass the search response's snapshot token to read the same revision for up to 15 minutes. Current routes and permissions are still checked.
- **Complete MCP results** — reads, listings and existing search commands include their output in structured responses, so clients such as Claude Code receive document text instead of only status metadata. Failure responses also retain their error messages.
- **Selective downloads and SHA-256 support** — explicit document/index paths use partial clones and selective checkout where supported. SHA-1 and SHA-256 branches, tags and commit pins work through Git. Commands and required submodules retain complete checkouts; commands run in independent copies.

**Submodule migration:** submodules are now disabled by default. Add `submodules: true` to index entries or registry roots that require nested documentation before upgrading. For remote index generation, use `generate --submodules`. Explicit boolean settings retain their meaning.

## Install

**Git 2.43.7 or newer is now required at runtime.** Install Git and ensure it is available on the MCP client's PATH, or select an absolute path with `--git` / `AIEXANDRIA_GIT`.

Download the archive for your platform, extract it and put `aiexandria` on PATH:

| Platform | Asset |
| --- | --- |
| macOS (Apple Silicon) | aiexandria-v0.4.0-aarch64-apple-darwin.tar.gz |
| macOS (Intel) | aiexandria-v0.4.0-x86_64-apple-darwin.tar.gz |
| Linux (ARM64) | aiexandria-v0.4.0-aarch64-unknown-linux-gnu.tar.gz |
| Linux (x86-64) | aiexandria-v0.4.0-x86_64-unknown-linux-gnu.tar.gz |
| Windows (x86-64) | aiexandria-v0.4.0-x86_64-pc-windows-gnu.zip |

Verify your archive against its entry in `SHA256SUMS`. For example, on macOS:

~~~sh
shasum -a 256 aiexandria-v0.4.0-aarch64-apple-darwin.tar.gz
~~~

The macOS binaries are not notarized. If Gatekeeper blocks a download you have verified, clear its quarantine flag after extracting:

~~~sh
xattr -d com.apple.quarantine aiexandria
~~~

See the [installation guide](https://hellonoel.github.io/aiexandria/installation.html) for Linux/Windows verification, source builds and MCP client configuration.

## Upgrade

Existing registries and custom-command query languages continue to work. Ranked search is disabled unless enabled in `search.yaml`; the default public collection still works without an embedding service or hosting account. No Pinakes format change is required for ranked or global search.

The repository cache layout changes; older caches remain available for rollback. Prepare a public source before restarting your MCP client:

~~~sh
aiexandria update --index public-docs --path languages --path nodejs
~~~

If you use a custom registry, add the public collection first or substitute one of your own index routes. See [repository updates](https://hellonoel.github.io/aiexandria/updating.html) for scheduling and concurrency. No daemon or schedule is installed automatically.

## Getting started

Follow the [getting-started guide](https://hellonoel.github.io/aiexandria/getting-started.html) for the default setup, or [enable ranked search](https://hellonoel.github.io/aiexandria/semantic-search.html) for conceptual and cross-repository questions.

Discovery still ranks metadata. Ranked searches cover eligible cached documents, and global results report gaps and work limits. Verify returned passages before using them as evidence.
