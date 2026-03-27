---
name: logmon
description: Use when debugging runtime behavior, investigating errors or crashes, examining log output, or when the user mentions logs, tracing, or asks you to check what happened. Also use when the logmon MCP server tools (get_recent_logs, get_triggers, etc.) are available. Covers any project that uses structured logging (GELF, tracing, syslog).
---

# Using the Log Monitor (logmon)

You have access to a log collector MCP server that receives structured logs from applications in real-time. It runs as a daemon shared across multiple Claude sessions.

## Architecture

logmon uses a **daemon + shim** architecture:
- **Daemon**: Long-running process that collects GELF logs (UDP/TCP), stores them in a ring buffer, and evaluates triggers/filters per session.
- **Shim**: Thin MCP bridge that connects your Claude session to the daemon. Auto-starts the daemon if not running.

Multiple Claude sessions share the same daemon and log buffer. Each session has its own triggers and filters.

## Available Tools

### Querying Logs
- **get_recent_logs** — Fetch recent logs, optionally filtered. Use `count` (default 100) and `filter` (DSL string).
- **get_log_context** — Get logs around a specific entry by `seq` number.
- **export_logs** — Save logs to a file for comparison or sharing.
- **clear_logs** — Clear the log buffer (affects ALL sessions since buffer is shared).

### Filtering
- **get_filters** — List active buffer filters for this session.
- **add_filter** — Add a buffer filter. Only matching logs are stored. Use when you want to focus on specific components.
- **edit_filter** — Modify a filter's expression or description.
- **remove_filter** — Remove a filter. When no filters exist, all logs are buffered.

### Triggers
- **get_triggers** — List configured triggers for this session.
- **add_trigger** — Add a trigger that notifies you when matching logs arrive. Set `pre_window` and `post_window` for context capture.
- **edit_trigger** — Modify a trigger's filter, windows, or description.
- **remove_trigger** — Remove a trigger.

### Sessions
- **get_sessions** — List all sessions connected to the daemon.
- **drop_session** — Remove a named session and all its triggers/filters.

### Status
- **get_status** — Server status: buffer size, session info, receiver ports, message stats.

## Multi-Session Behavior

- **Shared buffer**: All sessions read from and write to the same log buffer. `clear_logs` clears for everyone.
- **Per-session filters**: Each session's filters determine what gets stored. The union of all sessions' filters is applied (OR semantics across sessions).
- **Per-session triggers**: Each session has its own triggers. Default triggers (ERROR and panic) are created automatically.
- **Named sessions**: Use `--session <name>` when starting logmon for persistent sessions that survive disconnects. Triggers and filters are preserved, and notifications are queued while disconnected.
- **Anonymous sessions**: Default. Cleaned up on disconnect.

## Filter DSL

Filters use a comma-separated qualifier syntax (AND semantics within a filter):

| Pattern | Meaning |
|---------|---------|
| `text` | Case-insensitive substring match against all fields |
| `/regex/` | Regex match (case-sensitive, use `/regex/i` for insensitive) |
| `selector=pattern` | Match against a specific field |
| `l>=LEVEL` | Level comparison (ERROR, WARN, INFO, DEBUG, TRACE) |
| `"quoted"` | Literal text (use for commas or equals in patterns) |

**Selectors:** `m` (message), `fm` (full_message), `mfm` (message or full_message), `h` (host), `fa` (facility), `fi` (file), `ln` (line), `l` (level), or any custom GELF field name.

**Examples:**
- `l>=ERROR` — all errors
- `fa=mqtt,l>=WARN` — warnings+ from MQTT module
- `connection refused,h=myapp` — connection errors from myapp
- `/panic|unwrap failed/` — regex for panics

## Workflow Tips

### Debugging a specific component
1. `add_filter` with `fa=<module>` to focus on that module
2. Ask the user to reproduce the issue
3. `get_recent_logs` to examine what happened
4. `remove_filter` when done to go back to capturing everything

### Before/after comparison
1. `clear_logs` to start fresh
2. `export_logs` with path for "before" state
3. Make changes
4. `export_logs` with a different path for "after" state
5. Compare the two files

### Monitoring across sessions
1. Use `get_sessions` to see what other sessions are connected
2. Named sessions (`--session debug`) let you keep triggers/filters across reconnects
3. Use `drop_session` to clean up stale named sessions

### When you receive a trigger notification
The notification includes `context_before` entries and the matched log. Use `get_log_context` with the matched entry's `seq` to get more surrounding context if needed.

## Important Notes
- Triggers always evaluate every incoming log, even when filters are active
- The pre-trigger buffer captures ALL logs regardless of filters — when a trigger fires, you get unfiltered context before the event
- Post-trigger windows also bypass filters, capturing the aftermath
- Logs during a post-trigger window skip trigger evaluation (prevents cascading)
- The daemon listens on GELF UDP and TCP (default port 12201)
- Config is stored in `~/.config/logmon/`
