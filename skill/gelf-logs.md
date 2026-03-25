---
name: gelf-logs
description: Use when debugging applications that emit GELF logs, when you need to examine runtime behavior, error patterns, or investigate issues using live log data from the GELF MCP server.
---

# Using the GELF Log Collector

You have access to a GELF log collector MCP server that receives structured logs from applications in real-time.

## Available Tools

### Querying Logs
- **get_recent_logs** — Fetch recent logs, optionally filtered. Use `count` (default 100) and `filter` (DSL string).
- **get_log_context** — Get logs around a specific entry by `seq` number or `timestamp`.
- **export_logs** — Save logs to a file for comparison or sharing.
- **clear_logs** — Clear the log buffer (useful before running a test).

### Filtering
- **get_filters** — List active buffer filters.
- **add_filter** — Add a buffer filter. Only matching logs are stored. Use when you want to focus on specific components.
- **remove_filter** — Remove a filter. When no filters exist, all logs are buffered.

### Triggers
- **get_triggers** — List configured triggers.
- **add_trigger** — Add a trigger that notifies you when matching logs arrive. Set `pre_window` and `post_window` for context capture.
- **edit_trigger** — Modify a trigger's filter, windows, or description.
- **remove_trigger** — Remove a trigger.

### Status
- **get_status** — Server status: buffer size, filter/trigger counts, ports, message stats.

## Filter DSL

Filters use a comma-separated qualifier syntax (AND semantics):

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

### When you receive a trigger notification
The notification includes `context_before` entries and the matched log. Use `get_log_context` with the matched entry's `seq` to get more surrounding context if needed.

## Important Notes
- Triggers always evaluate every incoming log, even when filters are active
- The pre-trigger buffer captures ALL logs regardless of filters — when a trigger fires, you get unfiltered context before the event
- Post-trigger windows also bypass filters, capturing the aftermath
- Logs during a post-trigger window skip trigger evaluation (prevents cascading)
