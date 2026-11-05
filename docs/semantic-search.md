# Semantic search

Ranked search is optional in v0.4.0. Enable it with a separate search
configuration; the default setup continues to work without it.

Ranked search finds passages in documentation already cached by AIexandria.
Keyword search uses words; semantic search can find related concepts with
different wording; hybrid search combines both. Results include the original
file, Git revision and line range so your assistant can read the evidence.

Existing Pinakes indexes and custom search commands continue to work. No changes
to the Pinakes format are needed.

## Start with keyword search

Create `~/.config/aiexandria/search.yaml` (or
`$XDG_CONFIG_HOME/aiexandria/search.yaml`):

```yaml
enabled: true
```

On Windows, use `%USERPROFILE%/.config/aiexandria/search.yaml`. You can instead
pass `--search-config /absolute/path/search.yaml` or set
`AIEXANDRIA_SEARCH_CONFIG`. Use the same configuration and cache for the CLI and
MCP server.

Prepare a source from the public collection:

```sh
aiexandria update --index public-docs --path languages --path nodejs --semantic --wait --timeout 5m
```

If you have a custom registry, [add public-docs](adding-documentation.md) first.
Restart your MCP server after changing its search configuration. Your assistant
can now discover sources advertising `keyword` search and request ranked
passages.

Preparing a frequently used source is recommended during setup. To prepare the
entire public collection instead, use
`aiexandria update --all --index public-docs --semantic --wait --timeout 30m`.
Omit `--index public-docs` to include every configured registry root. This can
download much more data, especially for custom-command sources; see
[preparing all sources](updating.md#prepare-all-sources).

## Add a local embedding provider

Install and start an embedding server separately. For example, with
[Ollama](https://ollama.com/), download a model explicitly:

```sh
ollama pull nomic-embed-text
ollama list
```

Add the provider to your search configuration. Replace the revision value with
the model ID reported by `ollama list`:

```yaml
enabled: true
embedding:
  provider: ollama
  endpoint: http://127.0.0.1:11434/api/embed
  model: nomic-embed-text
  revision: "replace-with-installed-model-id"
  query_prefix: "search_query: "
  document_prefix: "search_document: "
  timeout_secs: 30
```

The prefixes above are for `nomic-embed-text`. Follow your model's
recommendations; other models may require different prefixes or none. AIexandria
sends a title/heading and the passage text for document embeddings. It does not
request generated answers, query expansion or reranking.

### Use llama.cpp with EmbeddingGemma

If you already have
[llama-server](https://github.com/ggml-org/llama.cpp/tree/master/tools/server),
you can use it instead of installing Ollama. On macOS, Homebrew provides it
through `brew install llama.cpp`. Download an
[EmbeddingGemma 300M Q8_0 GGUF](https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF)
after reviewing its model license, and save it as
`~/.local/share/aiexandria/models/embeddinggemma.gguf`.

Start the provider in a separate terminal:

```sh
llama-server \
  --model ~/.local/share/aiexandria/models/embeddinggemma.gguf \
  --alias embeddinggemma --embedding --pooling mean \
  --ctx-size 2048 --batch-size 2048 --ubatch-size 2048 \
  --parallel 1 --host 127.0.0.1 --port 8080
```

Leave that process running while using semantic search. AIexandria does not
start or supervise it. These settings use one provider slot and a small context
allocation; increasing the model's maximum context is unnecessary for the
default documentation chunks.

In another terminal, check its health and obtain a stable model revision:

```sh
curl --fail http://127.0.0.1:8080/health
shasum -a 256 ~/.local/share/aiexandria/models/embeddinggemma.gguf
```

Use the checksum in your search configuration:

```yaml
enabled: true
embedding:
  provider: openai_compatible
  endpoint: http://127.0.0.1:8080/v1/embeddings
  model: embeddinggemma
  revision: "replace-with-model-file-sha256"
  query_prefix: "task: search result | query: "
  document_prefix: "title: Documentation | text: "
  timeout_secs: 30
```

Use the full endpoint URL. `revision` is a cache identity you maintain; it does
not select or pin the model on the server. Change it whenever the model weights
or embedding behavior change. Model dimensions are checked, but a replacement
model with the same dimensions cannot be detected automatically.

Re-run the update command and restart MCP with the same configuration. The first
embedding pass can take time; subsequent passes reuse unchanged chunks. The
SQLite database and vectors live under the configured repository cache's
`search/` directory. No model or inference runtime is bundled with AIexandria.

Loopback HTTP(S) endpoints work by default. A remote HTTPS provider requires
`allow_remote: true`; eligible document passages and queries will be sent to
that service. If authentication is needed, set `token_env` to the name of an
environment variable containing its bearer token. Do not put a token in the YAML
or URL. Redirects and credentials in endpoint URLs are rejected.

### Choose a model for your available memory

Start with a small embedding model and use hybrid search for documentation
questions. EmbeddingGemma 300M Q8_0 is the model tested in our local AIexandria
pilot. On a small Node.js documentation evaluation, hybrid search found the
labelled passage in the first five results for 14 of 14 answerable questions,
compared with 11 for semantic search and 10 for keyword search. This supports it
as a starting point, not as a proven best model for every corpus. All three
modes also returned hits for questions the corpus could not answer: always read
the evidence.

Budget for memory left after your editor, browser, containers and any chat model
are running. These are planning estimates for the provider and indexing work,
not measured minimum requirements:

| Installed memory  | Suggested starting point                                                                | Additional working headroom                                       |
| ----------------- | --------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| 8 GiB             | Keyword search if memory is tight; otherwise a 100–300M embedding model                 | About 2–3 GiB for a small provider and indexing                   |
| 16–18 GiB         | EmbeddingGemma 300M Q8_0                                                                | About 2–3 GiB; allow more when comparing a 0.6B model             |
| 24–32 GiB         | The same small models; consider a 4B model only after measuring a retrieval improvement | Roughly 6–10 GiB for a quantized larger-model experiment          |
| 48–64 GiB or more | Small models remain useful; 4B/8B models are optional experiments                       | Roughly 10–16 GiB or more for larger models and provider overhead |

On Apple Silicon, CPU and GPU share memory. Watch Activity Monitor's Memory
Pressure and whether Swap Used keeps growing during indexing. If pressure rises,
reduce provider concurrency or use a smaller model. A GGUF download's size is
only part of its runtime memory cost.

[EmbeddingGemma 2](https://ai.google.dev/gemma/docs/embeddinggemma/model_card_2),
using its small text-only model, is a promising next comparison: Google reports
improved code retrieval and current llama.cpp supports it. It has not yet been
benchmarked with AIexandria.
[Qwen3-Embedding-0.6B](https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF) is
another multilingual candidate, but needs last-token pooling and its recommended
query instruction instead of Gemma's settings. If Ollama is already installed,
`nomic-embed-text` is a convenient alternative for English documentation.
Compare models on your own questions before spending more memory.

Corpus size matters too. AIexandria stores float32 vectors: 100,000 chunks use
about 293 MiB at 768 dimensions or 391 MiB at 1,024 dimensions before text and
database overhead. These are storage figures, not a requirement to keep the
entire database in RAM. Larger vectors also cost more work to search. Limit the
corpus to useful documentation before choosing a larger model.

After changing models, update the prefixes and `revision`, rerun
`update --semantic`, and restart MCP. Vectors from different models cannot be
mixed even when their dimensions match.

## What gets indexed

There are two distinct scopes:

- A **browse source** searches the explicit document entries at that index
  level. It does not include every file in their repositories.
- A **cached repository source** searches eligible available files in one
  repository/ref, reached through an active entry at that index path. It is a
  separate generated source with its own `source_id`. It does not inherit a
  custom command's search language or promise to reproduce its file selection.

Background indexing starts with MCP, runs after reads and periodically checks
cached roots. It never fetches repositories, downloads missing Git blobs or
executes custom commands. Selective Git checkouts therefore produce a partial
corpus: only files already materialized can be indexed. Ordinary navigation or
`update` can prepare more files; later indexing picks them up.

Automatic selection includes Markdown (`.md` and `.markdown`), MDX,
reStructuredText, AsciiDoc, text files and README-like files. Extension matching
is case-insensitive. It skips symlinks and common dependency, build, cache and
tooling directories. Binary and oversized files are skipped. Explicit document
entries can use other text extensions.

Defaults are 2,000 files, 5 MiB per file and 64 MiB of text per source, with
approximately 1,200-byte chunks. Results report `coverage`, including skipped or
missing files and whether a limit was reached. Cached coverage is never a claim
that an entire remote repository has been searched.

## Control the corpus

Use repository-relative globs, globally or for an index/path subtree:

```yaml
enabled: true
exclude: ["**/generated/**", "**/archive/**"]
sources:
  - index: public-docs
    path: [languages, nodejs]
    include: ["doc/api/**/*.md", "README.md"]
    exclude: ["doc/api/deprecations.md"]
  - index: private-archive
    enabled: false
max_files: 2000
max_file_bytes: 5242880
max_corpus_bytes: 67108864
chunk_bytes: 1200
```

A rule matches its path and descendants. More specific rules apply last;
equal-depth rules use YAML order. `include` replaces the inherited include list,
and exclusions accumulate. These filters apply to explicit docs as well as
cached repository sources. Includes can select extra text extensions but do not
override the automatic scanner's excluded directories or symlink rules.
Disabling search for a scope does not disable its existing Pinakes commands or
file reads.

Configuration keys are validated strictly. Changing corpus configuration creates
a separate search database, so independent MCP configurations cannot overwrite
each other's membership. Old configuration databases remain on disk; stop
processes using them before removing the cache's `search/` directory to reclaim
them. Within a configuration, obsolete sources are removed after a complete
cached inventory and unreferenced vectors are collected; SQLite reuses their
space.

## Use the MCP workflow

Discover a topic, use an advertised mode, then read the best passages:

```json
{
  "index": "public-docs",
  "path": ["languages", "nodejs"],
  "query": "How do writable streams signal backpressure?",
  "mode": "hybrid",
  "limit": 5,
  "max_output_bytes": 8192
}
```

That explicit route searches listed documents. To search an automatically
selected cached repository scope, pass its discovered `source_id` instead of
`index` and `path`. The returned `search` object provides the route and a
default mode. Custom-command sources keep their original syntax; select a browse
or cached repository source for ranked search.

`keyword` uses SQLite FTS5/BM25 with word matching. Query words are safely
quoted and combined as alternatives; operators such as AND/OR are not a portable
syntax. `semantic` uses embedding similarity. `hybrid` combines keyword and
vector rankings with reciprocal-rank fusion. Source searches are scoped before
ranking. Global searches rank the eligible union described below. Discovery
still ranks metadata.

The default is five hits and 8,192 output bytes; allowed ranges are 1–20 hits
and 1,024–32,768 bytes. At most two passages per file are returned. `deep: true`
belongs to legacy search and cannot be combined with a ranked mode. Omitting
`mode` preserves existing searches, except that generated cached repository
sources select their advertised default.

Discovery also reports `search_status` with last-indexed coverage and readiness;
it is informational and each search revalidates membership.

Check `mode_used`, `semantic`, `coverage`, `freshness` and `truncated`. Hybrid
search falls back to keyword results while embeddings are pending or
unavailable. Semantic-only requests return a normal `preparing` or `unavailable`
response when they cannot run. If a source has no indexed text, there are no
hits.

Each hit includes `read` (index, path, entry ID), `file`, `revision`, `offset`
and `limit`. Pass those route fields and the top-level `snapshot` token to
`read_file`; expand the line range when necessary. Rankings are candidates, not
confidence estimates. A semantic search may return unrelated passages for a
question the corpus cannot answer; verify the evidence before using it.

## Search globally

Use global search when a question spans repositories or you do not know which
source contains the answer:

```json
{
  "scope": "global",
  "query": "How are failed requests retried?",
  "mode": "hybrid",
  "limit": 5,
  "max_output_bytes": 8192
}
```

This ranks one combined corpus of currently indexed, cached documentation across
enabled registry roots. It never fetches repositories, hydrates missing files,
executes custom commands or builds document embeddings as part of the request.
The background indexer can continue its existing work. With an embedding
provider configured, the default mode is hybrid; otherwise it is keyword.

Narrow the search with plural filters:

```json
{
  "scope": "global",
  "indexes": ["public-docs"],
  "query": "How do streams handle backpressure?",
  "mode": "hybrid"
}
```

You can also pass source_ids containing IDs returned by discovery. These must
identify browse or generated cached repository sources, not custom commands.
Both filters may be combined if the sources belong to the selected indexes; each
list allows up to 64 entries. Omit singular index/path/source_id/command_id and
deep. Global search uses cached_ok; request refreshes through update or a scoped
search instead.

Filters and configured exclusions apply before ranking. Duplicate passages at
the same Git revision, file and line range count once even when several indexes
expose them. Different revisions remain distinct. Results prefer contributions
from different repositories and return at most two passages per file/revision.
Remaining slots can use the same repository when no useful alternatives exist.

Each hit includes source.name and a source.search route for narrowing the next
search. Follow its read route with the response snapshot to read the exact
evidence. Only the requested number of hits is returned; global search does not
list every configured index in the response.

Coverage reports the number of sources and passages searched, sources awaiting a
current index, unavailable metadata/repositories and work limits. It describes
the cached corpus, not all documentation on remote repositories. If some
embeddings are pending, hybrid still combines available vectors with keyword
results from the other eligible documents. The embedded_chunks count records
vectors compared in this query; keyword-only searches do not compare vectors. If
the provider fails, hybrid returns keyword results when the remaining execution
budget permits it.

When no requested sources are cached, global search reports unavailable; it does
not imply that a download is queued. Use discovery to locate documentation that
has not been cached, then prepare that source with update --semantic or ordinary
navigation. Global search revalidates source membership and revisions; outdated
search rows are skipped until reindexed. An empty result with partial coverage
is not proof of absence.

The default work limits are configurable in search.yaml:

```yaml
enabled: true
global_max_sources: 128
global_max_chunks: 50000
global_timeout_secs: 30
```

Changing these retrieval limits does not rebuild the corpus or its embeddings.
Allowed ranges are 1–1024 sources, 1–200000 deduplicated chunks and 1–120
seconds. The timeout covers query embedding and database ranking, including
expensive SQLite operations. refresh_wait_ms separately bounds cached inventory
validation. Hitting a source/chunk limit returns partial coverage; an execution
deadline returns a normal limited response suggesting narrower filters. A limit
can omit the most relevant source, so increase it or narrow the scope before
treating results as exhaustive. Result-count/byte truncation is reported
separately in truncated.

## Updates and concurrency

```sh
# Index only content already available; perform no Git work.
aiexandria update --semantic --cached-only --json

# Prepare one public source, then index its available documentation.
aiexandria update --index public-docs --path languages --path nodejs --semantic --wait --timeout 5m
```

`--semantic` enables the search-index phase, including keyword-only
configurations. `--all` also prepares uncached repositories in the selected
scope. `--cached-only` requires `--semantic` and cannot be combined with
`--force` or `--all`. Without `--semantic`, update retains its existing
repository-only behavior.

Only one bulk update campaign runs per cache. MCP background workers and updates
also share an embedding lock, use batches of up to eight chunks, and yield
between batches while a query is embedding. They reuse completed vectors after
interruption. Queries can run alongside one already-started batch; a request
does not interrupt a provider call in progress.

Update reports distinguish pending embeddings, unavailable metadata levels and
partial sources. `--wait` retries pending work until the campaign deadline; it
does not fetch missing content in cached-only mode or override corpus limits.
`partial` and `preparing` exit with status 1. A campaign deadline is checked
between batches; an in-progress provider request may take up to its configured
timeout to finish. The MCP refresh deadline covers repository/index preparation,
with a separate provider timeout for query embedding.

If hybrid keeps falling back, check the provider endpoint, model and
credentials, then run the update command with `--json`. Failed background
embedding requests back off for 30 seconds. Increase the provider timeout for
slow model startup, or reduce `chunk_bytes` if its input limit is smaller.
Keyword search remains available without a running embedding server.
