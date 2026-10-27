# AIexandria

LLMs are bad at picking the right docs to consult. AIexandria is an MCP
server that gives them hierarchical, sandboxed access to documentation
hosted in git repos — public or private, with submodules, behind tokens
or SSH agents.

A docs author writes a small `pinakes.yaml` index file in their repo listing
documents, sub-indexes, and sandboxed search commands. Referencing a repository
grants repo-root-sandboxed read access to its checkout; document entries curate
navigation and are not a per-file allow-list. AIexandria clones the repo on
demand, refreshes it lazily, and exposes six MCP tools: `list_indexes`,
`list_docs`, `read_doc`, `read_file`, `read_url`, and `search_docs`.

This site documents how to install AIexandria, write or generate an index,
and wire the server into an MCP client like Claude Code.
