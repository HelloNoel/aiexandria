# Getting started

This guide takes you from installation to reading a real document with AIexandria v0.3.0. The public starter collection requires no Git hosting account or search service.

## 1. Install

Download your platform's archive from the [v0.3.0 release](https://github.com/HelloNoel/aiexandria/releases/tag/v0.3.0), extract it, and put the executable on PATH. See [installation](installation.md) for platform-specific commands and checksum verification.

~~~sh
aiexandria --version
~~~

The output should report 0.3.0. Older binaries do not include the discovery tool or default collection described here.

## 2. Connect your assistant

Choose your MCP client. These commands use an executable already on PATH.

**Codex:**

~~~sh
codex mcp add AIexandria -- aiexandria
~~~

**Claude Code:**

~~~sh
claude mcp add AIexandria --scope user -- aiexandria
~~~

For desktop clients or a command-not-found error, use the executable's absolute path. [Client configuration](installation.md#connect-an-mcp-client) includes configuration-file examples. Restart the client; use /mcp in Codex or Claude Code to inspect the connection.

## 3. Read a document

Ask your assistant:

> Use AIexandria to open its getting-started guide and tell me which version it describes. Cite the document.

A precise tool-level check uses read_doc with:

~~~json
{"index":"aiexandria","entry_id":"getting-started","offset":1,"limit":40}
~~~

It should return this guide and identify v0.3.0. The first call may wait while AIexandria acquires its help repository.

Then try:

> Use AIexandria to find Rust documentation about ownership and borrowing. Read a relevant passage and explain the rule with a citation.

The assistant should discover a source, search or browse its selected pages, and read relevant text. The default registry contains public-docs and aiexandria. It does not clone all 20 upstream repositories at startup.

## 4. Make it your own

[Add your documentation](adding-documentation.md) when you are ready. An explicit registry, or an existing registry at the default location, replaces the built-in defaults. Existing installations keep their own collection.

A successful connection does not mean every source is cached. On a slow first search, refresh_pending means acquisition is still running; retry after the suggested delay. See [troubleshooting](configuration.md#common-problems).
