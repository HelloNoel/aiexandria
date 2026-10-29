# AIexandria

AIexandria connects AI assistants to documentation in Git repositories through MCP. It finds relevant sources, searches selected documents, and reads the original text with file and line references.

**[Get started](https://hellonoel.github.io/aiexandria/getting-started.html)** · [Download v0.3.0](https://github.com/HelloNoel/aiexandria/releases/tag/v0.3.0) · [User guide](docs/introduction.md)

Install the binary and connect your MCP client. With no registry configured, AIexandria provides a curated [public collection](https://github.com/HelloNoel/aiexandria-indexes) covering 20 programming and infrastructure topics, plus help for your installed release. No account, API token, ripgrep, or embedding service is needed for that setup.

For your own documentation, register a Pinakes index alongside your docs or maintain one in a separate repository. Sources can span public and private repositories, branches, tags, and pinned commits. Different sources can provide their own search commands.

- **Find sources:** global metadata discovery returns a small ranked selection, including nested indexes.
- **Search and read:** search the selected source and retrieve relevant line ranges as evidence.
- **Refresh lazily:** acquire repositories on demand and refresh according to your settings.
- **Compose collections:** combine team indexes while preserving repository references and command policies.

Discovery searches metadata, not every document. The starter collection includes selected pages, not complete upstream manuals. First use of a source can take time while its repository is cloned.

See [adding documentation](docs/adding-documentation.md), [configuration](docs/configuration.md), and [writing indexes](docs/writing-indexes.md). Referenced repositories are read-access boundaries; listed documents curate navigation rather than restricting individual files. Custom commands run with the server's permissions and require trusted authors or your own OS isolation.

AIexandria implements [Pinakes](https://github.com/HelloNoel/pinakes). See [CONTRIBUTING.md](CONTRIBUTING.md) for development. Native SHA-256 repository support remains planned.

Licensed under [Apache-2.0](LICENSE).
