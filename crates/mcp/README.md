# logmon-mcp

The `logmon-mcp` binary serves two roles:

1. **MCP stdio server** — invoked without subcommands, runs as the MCP shim that Claude Code, Cursor, etc. connect to via `claude mcp add logmon -- logmon-mcp`. This is today's default behavior, unchanged.

2. **CLI tool** — invoked with subcommands, performs broker operations from the shell. Mirrors the MCP tool surface 1:1. Useful when MCP isn't available (subagents, cross-tool consumers, CI scripts), or when you want pipe-friendly output.

Both modes connect to the same broker daemon (`logmon-broker`) over the same Unix domain socket.

## CLI quick reference

```
logmon-mcp [--session NAME] [--json] <COMMAND>
```

Global flags:
- `--session NAME`: connect to a named session. Default for CLI mode is `"cli"` so state persists across invocations.
- `--json`: emit machine-readable JSON. Default is human-readable text.

### Commands

| Group | Verb | Description |
|---|---|---|
| `logs` | `recent` | Fetch recent logs (newest-first; oldest-first when filter contains `c>=`). |
| `logs` | `context` | Fetch logs surrounding a specific seq. |
| `logs` | `export` | Export matching logs (with `--out FILE` to redirect). |
| `logs` | `clear` | Clear the log buffer. |
| `bookmarks` | `add` | Add a bookmark (a named seq position). |
| `bookmarks` | `list` | List bookmarks. |
| `bookmarks` | `remove` | Remove a bookmark by qualified name. |
| `bookmarks` | `clear` | Clear all bookmarks for a session. |
| `triggers` | `add` | Add a trigger (notification fires require an MCP shim subscriber). |
| `triggers` | `list` / `edit` / `remove` | Manage triggers. |
| `filters` | `add` / `list` / `edit` / `remove` | Manage per-session buffer filters. |
| `traces` | `recent` / `get` / `summary` / `slow` / `logs` | Query traces. |
| `spans` | `context` | Fetch spans surrounding a seq. |
| `sessions` | `list` / `drop` | List or drop sessions. |
| `status` | (no verb) | Print broker status. |

Run `logmon-mcp <group> --help` for per-group flag details.

## Notes

- **Triggers don't fire in CLI mode.** A CLI invocation exits before any matching log can fire the trigger. Use the CLI to *manage* triggers; subscribe to fires via the MCP shim or a custom SDK consumer.
- **The CLI is one-shot.** No reconnect, 5-second call timeout. Errors fast if the broker isn't running.
- **No auto-start.** Install the broker as a service: `logmon-broker install-service --scope user`.
