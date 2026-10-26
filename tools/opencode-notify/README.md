# OpenCode notifications for Jim

This OpenCode V2 CLI plugin emits a terminal BEL when a root agent execution succeeds, fails, or is interrupted by the user or inactivity. Jim's terminal worker turns the bell into a sidebar unread badge when the project's not visible. No desktop notification or terminal-focus detection is involved inside Jim. Child executions, shutdowns, and superseded executions are ignored; duplicate stop events notify only once per turn.

The plugin runs in the local CLI, so it reaches the correct terminal even when OpenCode uses a shared or remote server. An execution started in this CLI still notifies if you navigate to another OpenCode session before it finishes. Outside Jim, the plugin retains desktop notifications while the terminal is blurred.

Install with `bash scripts/install-opencode-notify.sh`, then restart each OpenCode TUI. The installer preserves unrelated `cli.json` settings, backs up changes, and replaces the existing `./stop-notify` entry to avoid duplicate notifications. The plugin is loaded from this checkout, so keep the checkout in place. No Jim restart is required.

Run tests with `bun test tools/opencode-notify/tui.test.ts`.
