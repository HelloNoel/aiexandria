# AIexandria — next release (draft)

Searches and reads now stay usable while repositories refresh. This release also adds manual and scheduled updates, selective downloads, and support for SHA-256 repositories through Git CLI.

## Highlights

- **Search during refresh:** return a complete cached snapshot immediately while a due refresh runs in the background. Missing content returns a normal preparing response with a retry delay.
- **Prepare ahead of time:** aiexandria update refreshes metadata and cached sources, or prewarms one search route. Concurrent updaters and MCP requests share repository jobs, and interactive requests can promote queued work.
- **Consistent evidence:** pass the snapshot token from a search to a follow-up read to keep the same revision. Tokens expire after 15 minutes; current routes and permissions are still checked.
- **Smaller downloads:** explicit document and index paths use partial clone and selective checkout where supported. Commands and required submodules retain complete checkouts. Commands use prepared independent copies.
- **SHA-256 repositories and pins:** the Git CLI replaces libgit2 for runtime repository access. Full commit IDs and unambiguous prefixes work for both SHA-1 and SHA-256. Resolved pins stay fixed during forced updates and selective downloads.

## Upgrade

Install Git 2.43.7 or newer before installing this build. Ensure Git is on the MCP client's PATH, or select it with --git / AIEXANDRIA_GIT. Repository operations use normal Git HTTPS/SSH configuration, with explicit AIexandria credentials taking precedence. Prompts, hooks, and checkout filters are disabled.

The new cache layout leaves older caches available for rollback. Prepare frequently used sources before restarting the MCP client:

~~~sh
aiexandria update --index YOUR_INDEX --path ENTRY_ID --command-id COMMAND_ID
~~~

See the [update guide](docs/updating.md) for command options and cron, systemd, and launchd examples. No daemon or schedule is installed automatically.

Discovery still searches metadata; deep search covers listed documents at the selected level. Custom commands retain their own query syntax and output format. Windows runtime validation and final version/asset naming must be reviewed before publishing this draft.
