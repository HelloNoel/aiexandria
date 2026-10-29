# Write useful indexes

Start with a handful of important pages. Group large collections by topic, and
keep entry IDs stable as descriptions improve.

## Generate a starting point

Work in a local checkout so you can review every generated file:

~~~sh
git clone https://github.com/YOUR-TEAM/docs.git team-docs
aiexandria generate --repo ./team-docs --no-search --output ./team-docs/pinakes.yaml
~~~

Generation can write nested indexes inside the checkout. Use --no-nested for one
index. Review files, remove irrelevant pages, and add topic and version descriptions
before committing. Generating from a URL writes nested files into a cache checkout;
use a local checkout for indexes you intend to maintain.

The --no-search option avoids an optional command requiring ripgrep. Built-in
search matches entry metadata; deep: true also searches listed documents at the
selected level. It does not search every repository file.

## Help discovery select a source

Give each topic a precise name and a short description containing its subject,
version or rolling branch, and coverage. For example: “Django 5.2: models,
QuerySets, migrations, views, templates, and deployment.” Include common names
people search for. Avoid generic keyword repetition.

Use kind: index for a navigable child. Use kind: merge to bring another index's
entries into the current level; choose distinct prefixes when IDs could collide.
See the [Pinakes examples](https://github.com/HelloNoel/pinakes/tree/main/examples).

## Add a search command

This optional example uses ripgrep and accepts its no-match exit code:

~~~yaml
version: 1
name: Team documentation
command_policy:
  allow: '^rg '
  timeout_secs: 10
entries:
  - id: search
    name: Search team docs
    description: >-
      Search Markdown with a ripgrep regular expression, not Boolean AND/OR.
      Results are path:line:text.
    kind: command
    argv: [rg, --line-number, --no-heading, --glob, '*.md', --, '{query}', .]
    success_exit_codes: [0, 1]
~~~

Install rg on the server first. Arguments are passed directly without a shell;
the command runs in the selected repository checkout. Describe query syntax and
returned paths so the LLM can search and read relevant excerpts.

Command policy limits allowed commands and execution time; it is not an OS
sandbox. Only enable commands from trusted indexes. Children can narrow inherited
policies, never widen them. Document entries curate navigation; they are not a
per-file access allowlist. Read tools can access other files in the selected repo.

## Validate and maintain

Validate YAML against the [Pinakes schemas](https://github.com/HelloNoel/pinakes/tree/main/schema),
then exercise discovery, reading, and commands through AIexandria. Schema validation
cannot prove remote file existence or command behavior. Check a successful search
and a no-match query. Update descriptions and version labels when sources change.
