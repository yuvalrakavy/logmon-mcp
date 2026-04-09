---
name: logmon
user_invocable: true
description: Use when debugging runtime behavior, investigating errors or crashes, examining log output, or when the user mentions logs, tracing, or asks you to check what happened. Also use when the logmon MCP server tools (get_recent_logs, get_triggers, etc.) are available. Covers any project that uses structured logging (GELF, tracing, syslog).
---

# Using the Log Monitor (logmon)

You have access to a log collector MCP server that receives structured logs from applications in real-time. It runs as a daemon shared across multiple Claude sessions.

## Quick Commands

`/logmon` accepts arguments for common actions. Execute the matching action immediately:

- `/logmon` (no args) — call `get_recent_logs` with count 50 and summarize
- `/logmon errors` — call `get_recent_logs` with filter `l>=ERROR` and summarize findings
- `/logmon warnings` — call `get_recent_logs` with filter `l>=WARN` and summarize findings
- `/logmon recent [count]` — call `get_recent_logs` with the given count (default 50)
- `/logmon status` — call `get_status` and report
- `/logmon clear` — call `clear_logs`
- `/logmon fixed` — call `get_recent_logs` with filter `l>=ERROR`, count 10. If no recent errors, report "looks fixed". If errors persist, show them.
- `/logmon watch <filter>` — call `add_filter` with the given DSL filter expression
- `/logmon unwatch` — call `get_filters`, then remove all filters
- `/logmon sessions` — call `get_sessions` and summarize
- `/logmon traces` — call `get_recent_traces` and summarize
- `/logmon slow` — call `get_slow_spans` with default threshold and summarize bottlenecks
- `/logmon trace <trace_id>` — call `get_trace` for a specific trace ID
- `/logmon <any DSL filter>` — if the argument looks like a filter DSL expression (contains `=`, `>=`, `/regex/`, or known selectors like `fa=`, `l>=`, `h=`, `m=`, `sn=`, `sv=`, `d>=`), call `get_recent_logs` (for log selectors) or `get_slow_spans` (for span selectors) with that filter
- `/logmon help` — print the Quick Commands list above (do NOT call any tools)

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

### Traces

- **get_recent_traces** — List recent traces with timing and error info. Use `count` and `filter` (span DSL).
- **get_trace** — Full trace detail: span tree + linked logs for a given `trace_id`.
- **get_trace_summary** — Compact timing breakdown highlighting bottlenecks for a trace.
- **get_slow_spans** — Find slow spans exceeding a duration threshold. Supports `group_by="name"`.
- **get_span_context** — Get spans surrounding a specific span by `seq` number.
- **get_trace_logs** — Get all logs linked to a trace by `trace_id`.

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
| ------- | ------- |
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

## Bookmarks

Use bookmarks instead of `clear_logs` when you want a clear before/after boundary for an operation but still need the prior history.

**When to reach for bookmarks:**

- Before starting a flaky operation you want to inspect — you can query just that range later instead of wading through everything.
- When comparing two attempts of the same operation — bookmark each attempt's start, then query the two ranges side by side.
- Whenever you'd otherwise reach for `clear_logs` to "see only what happens next" — bookmarks give you the same scoping without losing history.

**Tools:**

- `add_bookmark(name)` — drops a bookmark at the current moment. Pass `replace: true` to overwrite an existing bookmark with the same name.
- `list_bookmarks()` — shows all live bookmarks, newest first. Optional `session` param to filter.
- `remove_bookmark(name)` — removes a bookmark. Bare name = current session; `session/name` for cross-session.

**DSL operators:**

Bookmarks plug into the filter DSL as comparison qualifiers, just like `l>=warn` and `d>=100`:

- `b>=name` — entries at or after the bookmark's timestamp
- `b<=name` — entries at or before
- `b>=other-session/name` — reach into another session's bookmarks

Combine freely:

```
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, d>=100")
get_trace_logs(trace_id="...", filter="b>=before")
```

**Naming and lifetime:**

Bookmarks are global across sessions and stored as `{session_name}/{name}`. Two sessions can both have a bookmark called `before` — they're stored as `A/before` and `B/before` and don't collide. Inside *your* session, a bare `before` means `your-session/before`.

Bookmarks auto-evict when both the log buffer *and* the span buffer have rolled past their timestamp. You cannot end up with a stale bookmark pointing at data that no longer exists.

**Restrictions:**

`b>=` and `b<=` are query-only — they're rejected by `add_filter` and `add_trigger`. Bookmarks are timestamps frozen at creation time, so they don't make sense in long-lived registered filters.

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

### Investigating slowness
1. `get_slow_spans` to find bottleneck spans
2. `get_trace_summary` on the affected trace for timing breakdown
3. `get_trace` for the full span tree with linked logs
4. To compare: `get_recent_traces` to find a fast trace for the same endpoint, compare both summaries

### Following a request through the system
1. Find the request in `get_recent_traces`
2. `get_trace` to see the full span tree with logs interleaved
3. `get_trace_logs` with a filter to focus on specific log types within the trace

### When you receive a trigger notification

The notification includes `context_before` entries and the matched log. Use `get_log_context` with the matched entry's `seq` to get more surrounding context if needed.

## Important Notes

- Triggers always evaluate every incoming log, even when filters are active
- The pre-trigger buffer captures ALL logs regardless of filters — when a trigger fires, you get unfiltered context before the event
- Post-trigger windows also bypass filters, capturing the aftermath
- Logs during a post-trigger window skip trigger evaluation (prevents cascading)
- The daemon listens on GELF UDP/TCP (default port 12201) and OTLP gRPC/HTTP (default ports 4317/4318)
- Applications can send telemetry via GELF or OpenTelemetry (OTLP)
- OTLP provides both logs and traces (spans) — enabling request-level debugging
- Config is stored in `~/.config/logmon/`

## Span Filter DSL

Span filters use the same comma-separated syntax but with span-specific selectors:

| Selector | Field | Example |
| -------- | ----- | ------- |
| `sn` | span name | `sn=query_database` |
| `sv` | service name | `sv=store_server` |
| `st` | status | `st=error` |
| `sk` | span kind | `sk=server` |
| `d>=` / `d<=` | duration (ms) | `d>=100` |

Cannot mix log selectors and span selectors in the same filter.
