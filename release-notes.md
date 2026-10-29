# AIexandria v0.3.0

AIexandria now starts with a public documentation collection and can discover
relevant sources across your configured indexes. The project has moved to GitHub.

## Highlights

- **Useful defaults:** 20 programming, framework, tooling, and infrastructure
  topics, plus help matched to the installed release. Existing custom registries
  remain authoritative; add the public collection explicitly if desired.
- **Source discovery:** select sources through bounded metadata results before
  searching and reading. Different sources can keep their own search implementations.
- **Search refresh deadlines:** one configurable preparation deadline, with
  explicit pending/failed status and background acquisition shared across retries.
- **User guides:** installation, MCP clients, adding documentation, index authoring,
  and troubleshooting, with a separate implementer-focused Pinakes specification.
- **GitHub migration:** preserved histories converted from SHA-256 to SHA-1 and
  signed anew. Native SHA-256 support remains pending upstream Rust/libgit2 support.

Discovery is available starting in v0.3.0. It ranks metadata, not document contents.
Deep search covers listed documents at the selected level. Content-search output
has no shared byte budget; use focused queries and bounded read excerpts.

## Install

Download your archive, extract it, and put aiexandria on your PATH.

| Platform | Asset |
| --- | --- |
| macOS Apple Silicon | aiexandria-v0.3.0-aarch64-apple-darwin.tar.gz |
| macOS Intel | aiexandria-v0.3.0-x86_64-apple-darwin.tar.gz |
| Linux ARM64 | aiexandria-v0.3.0-aarch64-unknown-linux-gnu.tar.gz |
| Linux x86-64 | aiexandria-v0.3.0-x86_64-unknown-linux-gnu.tar.gz |
| Windows x86-64 | aiexandria-v0.3.0-x86_64-pc-windows-gnu.zip |

Compare your archive's SHA-256 with its line in SHA256SUMS. If you download all
archives, run shasum -a 256 -c SHA256SUMS (or sha256sum -c SHA256SUMS on Linux).
Linux builds require glibc 2.35 or newer. macOS binaries are unsigned; after
verifying your download, clear quarantine if needed:

~~~sh
xattr -d com.apple.quarantine aiexandria
~~~

Or build with a Rust toolchain:

~~~sh
git clone https://github.com/HelloNoel/aiexandria.git
cd aiexandria
git checkout v0.3.0
cargo install --path . --locked
~~~

## Getting started

See the [getting-started guide](https://hellonoel.github.io/aiexandria/getting-started.html)
to connect your MCP client and read a document from the default collection.
