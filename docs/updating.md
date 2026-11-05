# Prepare and update repositories

Use `aiexandria update` to prepare documentation before a session or on a
schedule. Git 2.43.7 or newer is required.

Searches and reads use complete cached snapshots while due refreshes run in the
background. A first visit still needs to download the requested content. Use
update before a session to prepare frequently used sources. These examples use
the built-in public collection; if you have a custom registry,
[add the public-docs entry](adding-documentation.md) first:

```sh
aiexandria update
aiexandria update --index public-docs --path languages --path nodejs
```

The first command refreshes configured index metadata and already-cached
referenced repositories when due. It does not download every document
repository. The second prepares the index chain and selected Node.js
documentation from the public collection. Repeat --path for each nested entry
ID. For a source with a custom search command, add --command-id with its actual
command entry ID; this prepares the complete repository, required submodules,
and a command copy without executing the search. Use the same registry, auth,
cache and Git options as your MCP server.

## Prepare all sources

For initial setup, prepare the sources you expect to use. To include every
referenced repository, even those you have never opened, use `--all`:

```sh
# Prepare the public collection, including uncached documentation.
aiexandria update --all --index public-docs --wait --timeout 30m

# Prepare every configured registry root and build the search index.
# Requires ranked search configured, and its embedding provider running if set.
aiexandria update --all --semantic --wait --timeout 30m
```

`--all` follows nested indexes and respects `--index`, `--path` and
`--command-id`. Document sources still download their declared paths
selectively. Custom-command sources require full checkouts and any required
submodules, but commands are never executed. Search exclusions, file/chunk
limits and traversal limits still apply: completion does not mean every file in
every remote repository was indexed. Check the report for errors or partial
coverage.

This is opt-in because adding a source to an index should not silently turn a
scheduled refresh into a large first-time download. Ordinary `update` keeps
refreshing metadata and already-cached sources; an explicit `--path` still
prepares its subtree. Add `--all` to a schedule only if you want it to acquire
newly added sources too. The existing campaign lock, deadlines and concurrency
limits apply.

`--all` does not force a remote refresh: add `--force` to check sources
immediately regardless of their refresh interval. It does not enable embeddings:
add `--semantic` for that. It cannot be combined with `--cached-only`.

`update --all --dry-run --json` reports the requested scope and cached
repository count without network access. It cannot enumerate repositories hidden
behind uncached metadata or estimate download size.

For regular updates after preparation, use:

```sh
aiexandria update --semantic --wait --timeout 30m
```

Omit `--semantic` if ranked search is disabled. Unchanged passages reuse their
existing embeddings.

## Refresh and coordinate updates

```sh
# Check selected mutable refs now, including those configured with refresh: never.
aiexandria update --index public-docs --force
# Inspect cached inventory without network access; incomplete coverage is explicit.
aiexandria update --dry-run --json
# Wait for another campaign, then complete any work still needed for this scope.
aiexandria update --wait --index public-docs --timeout 5m --json
```

Only one bulk update campaign runs per cache. A duplicate normally returns
status=already_running and exits successfully; that means it skipped, not that
the requested scope is complete. Use --wait when completion matters. JSON
distinguishes completed, already_running, timed_out and failed. Exit 0 means
completed/no work or an explicit already_running skip; 1 means operational
failure or incomplete work; 2 means invalid options/configuration. Report counts
describe checked, unchanged, and skipped work, with errors when applicable.

Searches and updates share acquisitions. There are at most four active
repository jobs across cooperating processes, including at most two background
jobs. An interactive request promotes a queued job and joins its work. It does
not restart an active fetch. Cached reads remain available throughout. Command
copies are prepared in the background and retained for the next request, with at
most two concurrent disk copies. A request timeout stops waiting; a repository
job has its own 120-second limit, configurable with --repo-timeout. The update
command's --timeout bounds its entire campaign and cancels only jobs it owns.

## Schedule an update

No daemon or schedule is installed automatically. Scheduling is optional: choose
an interval suitable for the repositories' existing refresh settings. Use
absolute paths and noninteractive credentials; a scheduler usually has a
different PATH and SSH environment from a terminal.

A cron entry can run every 15 minutes:

```cron
*/15 * * * * /absolute/path/aiexandria --git /absolute/path/git --registry /absolute/path/registry.yaml --cache /absolute/path/cache update --timeout 5m --json >> /absolute/path/update.log 2>&1
```

For a Linux user service, save this as
~/.config/systemd/user/aiexandria-update.service, replacing the paths:

```ini
[Unit]
Description=Update AIexandria documentation
[Service]
Type=oneshot
ExecStart=/absolute/path/aiexandria --git /absolute/path/git --registry /absolute/path/registry.yaml update --timeout 5m --json
```

Save aiexandria-update.timer in the same directory:

```ini
[Unit]
Description=Schedule AIexandria documentation updates
[Timer]
OnBootSec=2m
OnUnitActiveSec=15m
[Install]
WantedBy=timers.target
```

Enable it with systemctl --user daemon-reload and systemctl --user enable --now
aiexandria-update.timer.

On macOS, a launchd agent can use this plist at
~/Library/LaunchAgents/org.aiexandria.update.plist:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>org.aiexandria.update</string>
  <key>ProgramArguments</key><array>
    <string>/absolute/path/aiexandria</string>
    <string>--git</string><string>/absolute/path/git</string>
    <string>--registry</string><string>/absolute/path/registry.yaml</string>
    <string>update</string><string>--timeout</string><string>5m</string>
  </array>
  <key>StartInterval</key><integer>900</integer>
  <key>StandardOutPath</key><string>/absolute/path/update.log</string>
  <key>StandardErrorPath</key><string>/absolute/path/update-error.log</string>
</dict></plist>
```

After editing the paths, load it with launchctl bootstrap gui/$(id -u)
~/Library/LaunchAgents/org.aiexandria.update.plist. Review its logs after the
first run. Windows Task Scheduler can invoke the same update command using your
account and explicit executable paths.

## Index cached documentation

With [ranked search configured](semantic-search.md), add --semantic to update
the passage index after repository preparation. Add --cached-only to skip all
Git work. Both use the same campaign lock as normal updates. Search reports
include pending embeddings, missing metadata and partial coverage; preparing and
partial exit with status 1.

A failed repository acquisition does not prevent the search-index phase from
processing other available sources, provided the campaign has time remaining.
The final status remains `failed` (exit 1), with the affected source identified
in the errors; successful indexing does not clear acquisition failures. Reruns
reuse completed downloads and embeddings.
