# AIexandria v0.5.0

AIexandria now prefers hybrid search when an embedding provider is configured,
reports semantic failures explicitly, and can start your local provider on
demand. Source discovery can also use embeddings to find relevant repositories
before their documents are cached.

## Highlights

- **Unified search defaults:** omitted search mode selects `auto`, using hybrid
  retrieval with a provider and keyword retrieval otherwise. Omit the source to
  search indexed cached documentation globally. Explicit custom-command routes
  retain their query syntax.
- **Semantic source discovery:** rank source names, descriptions and routes with
  keyword, semantic or hybrid search. Metadata embeddings are cached separately
  from document embeddings, so discovery does not download or index every
  source's content. Prepare them with `aiexandria update --discovery --wait`.
- **Clear failures:** semantic and hybrid requests return MCP tool errors when
  the provider fails, and `preparing` while embeddings are incomplete. Request
  `fallback: keyword` explicitly to receive degraded keyword results with a
  warning. Text and structured responses expose the same status and diagnostics.
- **Optional provider startup:** configure `embedding.startup.argv` in your
  local search configuration. AIexandria starts it when needed, shares startup
  across clients and processes, and prevents duplicate managed models. Healthy
  external providers are reused; managed providers outlive individual MCP
  clients.
- **Provider controls:** use `aiexandria embedding start`, `status` and `stop`
  to manage the configured local provider. Startup uses bounded logs and checks
  process identity before stopping a managed provider.

## Install

On Apple Silicon macOS or Linux with Homebrew:

```sh
brew install HelloNoel/tap/aiexandria
```

Homebrew installs Git as a dependency. Ripgrep is optional; install it
separately for custom sources that invoke `rg`. For Intel Macs and Windows, use
a release archive. Intel Homebrew installations can build Git's dependencies
from source.

Manual installations require **Git 2.43.7 or newer** on the MCP client's PATH,
or selected with `--git` / `AIEXANDRIA_GIT`.

| Platform              | Asset                                              |
| --------------------- | -------------------------------------------------- |
| macOS (Apple Silicon) | aiexandria-v0.5.0-aarch64-apple-darwin.tar.gz      |
| macOS (Intel)         | aiexandria-v0.5.0-x86_64-apple-darwin.tar.gz       |
| Linux (ARM64)         | aiexandria-v0.5.0-aarch64-unknown-linux-gnu.tar.gz |
| Linux (x86-64)        | aiexandria-v0.5.0-x86_64-unknown-linux-gnu.tar.gz  |
| Windows (x86-64)      | aiexandria-v0.5.0-x86_64-pc-windows-gnu.zip        |

Verify the archive against `SHA256SUMS` before extracting it. See the
[installation guide](https://hellonoel.github.io/aiexandria/installation.html)
for platform-specific instructions and MCP client setup.

## Upgrade

Restart your MCP client after replacing the binary. Existing registries and
custom-command query languages remain compatible; no Pinakes format change is
required. Ranked passage search still requires an enabled `search.yaml`.

- Clients relying on metadata substring search should use `list_docs`, or
  `deep: true` for legacy substring/document search.
- Clients relying on automatic hybrid fallback must now request
  `fallback: keyword` and handle `status: degraded`. Check MCP `isError` for
  provider failures and follow retry guidance for `preparing` responses.
- Discovery defaults to hybrid when a provider is configured. Set
  `discovery.default_mode: keyword` in `search.yaml`, or request
  `mode: keyword`, to keep keyword discovery. Discovery ranks metadata, not
  document evidence.
- Provider startup is optional and configured only in the local search config.
  Install the provider and model yourself; AIexandria does not download models.
  Listing and keyword search/discovery do not start an embedding provider.

Document/vector caches are preserved when startup or discovery defaults change.
For setup and troubleshooting, see
[ranked search](https://hellonoel.github.io/aiexandria/semantic-search.html),
[source discovery](https://hellonoel.github.io/aiexandria/source-discovery.html)
and [repository updates](https://hellonoel.github.io/aiexandria/updating.html).
