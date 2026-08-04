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
- `--domain NAME`: scope this invocation to an existing domain — queries and `domains clear` target it. Omitted → `default`. Does not persist across invocations (each one is reset); `domains create/delete/list` ignore it.
- `--json`: emit machine-readable JSON. Default is human-readable text.

### Commands

| Group | Verb | Description |
|---|---|---|
| `logs` | `recent` | Fetch recent logs (newest-first; oldest-first when filter contains `c>=`). |
| `logs` | `context` | Fetch logs surrounding a specific seq. |
| `logs` | `export` | Export matching logs (`--path FILE`, or `--path -` for stdout). |
| `logs` | `clear` | Clear the log buffer. |
| `bookmarks` | `add` | Add a bookmark (a named seq position). |
| `bookmarks` | `list` | List bookmarks. |
| `bookmarks` | `remove` | Remove a bookmark by qualified name. |
| `bookmarks` | `clear` | Clear all bookmarks for a session. |
| `triggers` | `add` | Add a trigger (notification fires require an MCP shim subscriber). |
| `triggers` | `list` / `edit` / `remove` | Manage triggers. |
| `filters` | `add` / `list` / `edit` / `remove` | Manage per-session buffer filters. |
| `traces` | `recent` / `get` / `summary` / `slow` / `logs` | Query traces. With `slow --group-by name` the aggregates cover every matching span of that name; `--min-duration-ms` is then a display floor deciding which names appear. |
| `spans` | `context` | Fetch spans surrounding a seq. |
| `collectors` | `add` | Arm a span time collector (`--filter`, `--level`, repeatable `--group-keys`, `--description`). `--threshold.metric/.op/.value/.window-ms` arms a rolling guard. |
| `collectors` | `list` / `get` / `remove` | Read totals, percentiles and self time; release the budget. `get --snapshot LABEL` reads a recorded run. |
| `collectors` | `snapshot` / `history` | Record a window as a named run and start the next; list the runs. `history --merge` adds them up and reports the run-to-run spread. |
| `collectors` | `edit` | Change an armed collector. `--description` is free; anything else discards the live window (never the history). |
| `collectors` | `reset` | Zero and **discard** the run. Prefer `snapshot`. |
| `collectors` | `diff` | Subtract two arms and report what moved. An arm is `<collector>`, `<collector>@<label>`, or `<collector>@*` (every recorded run merged). `--allow-mismatch` / `--allow-lossy` / `--allow-truncated` permit the comparisons it otherwise blocks. |
| `collectors` | `document` | Write the measurement up: `--format md|json|folded`, `--path` to write it (and its sidecar) to disk, `--question` / `--finding` to make it triageable later. |
| `traces` | `profile` | Same numbers over spans already buffered, without arming anything. |
| `session` | `list` / `drop` | List or drop sessions. (Renaming the *current* session is an MCP-mode tool — `rename_session` — not a CLI verb: the CLI's per-invocation session has nothing durable to rename.) |
| `domains` | `create` / `delete` / `list` / `clear` | Manage isolated domains (each with its own buffers, receivers, triggers). |
| `cases` | `create` | Freeze a window to disk as one `<stem>.case.zip` — document, both evidence files, machine-readable provenance. `--dir` must be **absolute** (the broker is a service; a relative path resolves against *its* cwd). `--separate` writes loose files, `--uncompressed` stores rather than deflates, `--omit-logdata` / `--omit-spandata` leave that evidence out. |
| `cases` | `load` | Read one back into a sealed **postmortem** domain, so every read verb answers about it via `--domain`. Nothing arrives on it: collectors, triggers, filters and clears are refused by name, and reads of evidence the capture *omitted* are refused rather than answered emptily. |
| `status` | (no verb) | Print broker status (incl. `current_domain` + `active_filters`). Reports the broker's version alongside this CLI's. |

Run `logmon-mcp <group> --help` for a group's verbs, and
`logmon-mcp <group> <verb> --help` for a command's arguments, their types and
their accepted values.

**The table above is a convenience, not the source of truth.** Commands are
built at runtime from the broker's `tools.manifest`: the command paths are
derived from its RPC method names, and the arguments, types and accepted values
come from its schema. A broker that gains a tool gains a command with no
reinstall of this binary — so `--help` is authoritative and this table can lag.

## Notes

- **Triggers don't fire in CLI mode.** A CLI invocation exits before any matching log can fire the trigger. Use the CLI to *manage* triggers; subscribe to fires via the MCP shim or a custom SDK consumer.
- **Collectors do work across CLI invocations**, unlike triggers. They belong to the session, and CLI mode uses a persistent named session (`"cli"`), so `collectors add` → run your workload → `collectors get` is the intended shape. **They survive a daemon restart** — the definition and every recorded run are written through to `~/.config/logmon/collectors/`, though the live window is not (a partial measurement interrupted by a restart would report a `wall_ms` spanning the outage). An idle-TTL sweep of the session still disposes of them. Use `--session NAME` if you want a collector kept apart from other CLI work.
- **The CLI is one-shot.** No reconnect, 5-second call timeout. Errors fast if the broker isn't running.
- **Domains in CLI mode.** There is no sticky `domains use` verb. The CLI connects with a persistent named session, so each invocation is *explicitly* scoped by `--domain NAME` (queries + `domains clear`) and reset to `default` when the flag is omitted — a prior `--domain` never silently carries over. `domains create/delete/list` are domain-agnostic and ignore `--domain`. MCP mode, being long-lived, additionally has the sticky `use_domain` tool.
- **No auto-start.** Install the broker as a service: `logmon-broker install-service --scope user`.
