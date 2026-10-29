# Add documentation

AIexandria reads Pinakes indexes: YAML files describing documents, sub-indexes,
and optional search commands in Git repositories. To add your team's repository,
create ~/.config/aiexandria/registry.yaml:

~~~yaml
indexes:
  team:
    description: Team architecture and operations guides
    repo:
      url: https://github.com/YOUR-TEAM/docs.git
      branch: main
      submodules: false
    path: pinakes.yaml
    refresh: 1h
~~~

Replace the URL with your repository and commit a pinakes.yaml there. Restart the
MCP client. For a registry elsewhere, pass --registry /absolute/path/to/registry.yaml.

A custom registry replaces the built-in registry. To keep public docs and help,
add these entries beneath the same indexes mapping:

~~~yaml
  public-docs:
    repo:
      url: https://github.com/HelloNoel/aiexandria-indexes.git
      branch: v1
      submodules: false
    path: pinakes.yaml
    refresh: 24h
  aiexandria:
    repo:
      url: https://github.com/HelloNoel/aiexandria.git
      tag: v0.3.0
      submodules: false
    path: docs/pinakes.yaml
    refresh: never
~~~

## Add an index beside your documentation

Commit this pinakes.yaml at the root of your documentation repository:

~~~yaml
version: 1
name: Team documentation
description: Architecture decisions, deployment procedures, and incident runbooks.
entries:
  - id: deploy
    name: Deploy the service
    description: Releases, rollbacks, and deployment troubleshooting.
    kind: doc
    path: docs/deployment.md
~~~

Paths are relative to the repository root, even when the index is in a subdirectory.
Select files that exist in the configured Git revision. See
[writing useful indexes](writing-indexes.md) for generation and composition.

## Index a repository you do not own

Keep the index in a separate repository. A document can select another repository:

~~~yaml
version: 1
name: Rust learning resources
description: Ownership and borrowing in the Rust Programming Language book.
entries:
  - id: ownership
    name: What is ownership?
    kind: doc
    repo:
      url: https://github.com/rust-lang/book.git
      branch: main
      submodules: false
    path: src/ch04-01-what-is-ownership.md
~~~

Discovery reads your metadata without acquiring that document repository. A read
or deep search acquires it. The [public collection](https://github.com/HelloNoel/aiexandria-indexes)
uses this pattern. Upstream documentation retains its own license.
