# AIexandria

LLMs are bad at picking the right docs to consult. AIexandria is an MCP
server that gives them hierarchical, sandboxed access to documentation
hosted in git repos — public or private, with submodules, behind tokens
or SSH agents.

A docs author writes a small `pinakes.yaml` index file in their repo
listing the documents, sub-indexes, and sandboxed search commands an LLM
should be allowed to reach. AIexandria clones the repo on demand, refreshes
it lazily, and exposes four MCP tools: `list_indexes`, `list_docs`,
`read_doc`, and `search_docs`.

This site documents how to install AIexandria, write or generate an index,
and wire the server into an MCP client like Claude Code.
