# AIexandria

AIexandria connects AI assistants to documentation in Git repositories through
MCP. Find relevant sources, search for passages, and read the original text with
file, line and revision references.

**[Get started](https://hellonoel.github.io/aiexandria/getting-started.html)** ·
[Download v0.4.0](https://github.com/HelloNoel/aiexandria/releases/tag/v0.4.0) ·
[User guide](docs/introduction.md)

Install **Git 2.43.7 or newer** and AIexandria, then connect your MCP client.
With no registry configured, AIexandria provides a curated
[public collection](https://github.com/HelloNoel/aiexandria-indexes) covering 20
programming and infrastructure topics, plus help for the installed release. No
account, API token, ripgrep or embedding service is needed for this setup.

For your own documentation, register a Pinakes index alongside the docs or
maintain one in a separate repository. Sources can span public and private
repositories, branches, tags and pinned commits, including SHA-256 repositories.

- **Find sources:** global metadata discovery returns a small ranked selection
  from nested indexes.
- **Search and read:** use existing source searches, or enable
  [ranked search](docs/semantic-search.md) for keyword, semantic and hybrid
  passages. Global ranked search combines indexed cached documentation across
  sources with bounded results and explicit coverage.
- **Keep searches available:** read cached snapshots while due refreshes run in
  the background. Use [aiexandria update](docs/updating.md) to prepare sources
  ahead of time or on a schedule.
- **Compose collections:** combine team indexes while preserving repository
  references, command policies and each custom search command's query syntax.

Discovery searches metadata. Global passage search is optional and covers only
indexed cached content; it does not download every repository on demand.
Semantic search uses an embedding provider you configure separately. No models
or schedules are installed automatically. The starter collection includes
selected pages, not complete upstream manuals, and a first visit can require a
download.

**Upgrading from v0.3.0:** Git is now a runtime dependency and must be available
to the MCP process. Existing registries and custom searches continue to work.
See the [release notes](release-notes.md) for cache and search setup details.

See [adding documentation](docs/adding-documentation.md),
[configuration](docs/configuration.md), and
[writing indexes](docs/writing-indexes.md).

Referenced repositories are read-access boundaries; listed documents curate
navigation rather than restricting individual files. Custom commands run with
the server's permissions and require trusted authors or your own OS isolation.

AIexandria implements [Pinakes](https://github.com/HelloNoel/pinakes). See
[CONTRIBUTING.md](CONTRIBUTING.md) for development. Licensed under
[Apache-2.0](LICENSE).
