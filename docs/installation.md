# Installation and MCP clients

## Prerequisites

Install [Git 2.43.7 or newer](https://git-scm.com/install/), then check `git --version`. Git must be available on the MCP process's PATH; GUI clients may use a different PATH from your terminal. You can select an absolute Git path with `--git` or `AIEXANDRIA_GIT`. This is a new runtime requirement in v0.4.0.

The default setup needs no embedding service, database installation or Git hosting account. Ripgrep is only required by sources whose custom commands invoke it. [Semantic search](semantic-search.md) is optional.

## Download a binary

Download one archive and SHA256SUMS from the [v0.4.0 release](https://github.com/HelloNoel/aiexandria/releases/tag/v0.4.0).

| Platform | Archive |
| --- | --- |
| macOS Apple Silicon | aiexandria-v0.4.0-aarch64-apple-darwin.tar.gz |
| macOS Intel | aiexandria-v0.4.0-x86_64-apple-darwin.tar.gz |
| Linux ARM64 | aiexandria-v0.4.0-aarch64-unknown-linux-gnu.tar.gz |
| Linux x86-64 | aiexandria-v0.4.0-x86_64-unknown-linux-gnu.tar.gz |
| Windows x86-64 | aiexandria-v0.4.0-x86_64-pc-windows-gnu.zip |

The word unknown in a Linux filename is part of Rust's target name. GNU Linux builds require glibc; they are not Alpine/musl binaries.

### macOS

Run these commands in your download directory. Substitute x86_64 for aarch64 on Intel Macs:

~~~sh
shasum -a 256 aiexandria-v0.4.0-aarch64-apple-darwin.tar.gz
tar -xzf aiexandria-v0.4.0-aarch64-apple-darwin.tar.gz
mkdir -p ~/.local/bin
mv aiexandria ~/.local/bin/
~/.local/bin/aiexandria --version
~~~

Compare the checksum with that archive's line in SHA256SUMS. Add ~/.local/bin to PATH or use the absolute executable path in your client.

The binary is not notarized. If Gatekeeper blocks a download you have verified:

~~~sh
xattr -d com.apple.quarantine ~/.local/bin/aiexandria
~~~

### Linux

For x86-64, run the following. Substitute aarch64 for x86_64 on ARM64:

~~~sh
sha256sum aiexandria-v0.4.0-x86_64-unknown-linux-gnu.tar.gz
tar -xzf aiexandria-v0.4.0-x86_64-unknown-linux-gnu.tar.gz
mkdir -p ~/.local/bin
mv aiexandria ~/.local/bin/
~/.local/bin/aiexandria --version
~~~

Compare the checksum with SHA256SUMS. Add ~/.local/bin to PATH or use the executable's absolute path.

### Windows

In PowerShell, from your download directory:

~~~powershell
Get-FileHash .\aiexandria-v0.4.0-x86_64-pc-windows-gnu.zip -Algorithm SHA256
Expand-Archive .\aiexandria-v0.4.0-x86_64-pc-windows-gnu.zip -DestinationPath "$env:LOCALAPPDATA\Programs\AIexandria"
& "$env:LOCALAPPDATA\Programs\AIexandria\aiexandria.exe" --version
~~~

Compare the checksum with SHA256SUMS. Use the full executable path in your MCP configuration, or add its directory to your user PATH. Restart clients after changing PATH.

## Connect an MCP client

AIexandria is a local stdio MCP server. Your client starts it; you do not need a background service or HTTP port. Running it by hand without a client waits for MCP input.

### Codex

~~~sh
codex mcp add AIexandria -- aiexandria
~~~

Alternatively, add this to ~/.codex/config.toml:

~~~toml
[mcp_servers.AIexandria]
command = "/absolute/path/to/aiexandria"
~~~

Use codex mcp list and /mcp to inspect the connection. See the [official Codex MCP documentation](https://developers.openai.com/codex/mcp).

### Claude Code

~~~sh
claude mcp add AIexandria --scope user -- aiexandria
~~~

Use /mcp to inspect the connection. See the [Claude Code MCP documentation](https://code.claude.com/docs/en/mcp).

### Other clients

For clients using the common mcpServers JSON format:

~~~json
{
  "mcpServers": {
    "AIexandria": {
      "command": "/absolute/path/to/aiexandria",
      "args": []
    }
  }
}
~~~

On Windows, escape backslashes in JSON, for example C:\\Users\\you\\AppData\\Local\\Programs\\AIexandria\\aiexandria.exe. Use absolute paths rather than ~ in JSON or TOML. Your client may use a different format; choose a local stdio server.

Restart the client after configuration changes or replacing the executable. For a custom registry, add --registry and its absolute path to args; see [configuration](configuration.md).

## Build from source

Install Rust 1.89 or newer, Git 2.43.7 or newer, and native C build tools for the bundled SQLite and TLS dependencies. Windows GNU builds need a compatible MinGW toolchain.

~~~sh
git clone https://github.com/HelloNoel/aiexandria.git
cd aiexandria
git checkout v0.4.0
cargo install --path . --locked
~~~

This installs into Cargo's bin directory. Ensure both AIexandria and Git are available to the MCP client. See [CONTRIBUTING.md](https://github.com/HelloNoel/aiexandria/blob/v0.4.0/CONTRIBUTING.md) for development test dependencies.
