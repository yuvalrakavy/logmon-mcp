---
name: logmon
user_invocable: true
description: Use when the user mentions logs, traces, errors, crashes, or performance, or asks what happened at runtime. Use when investigating a flaky test, a slow request, or a panic. Use when the logmon MCP tools (get_recent_logs, get_recent_traces, add_trigger, add_bookmark, …) are available. Skip for static log files on disk, historical archives, or projects with no live telemetry pipeline.
---

# Using the log monitor (logmon)

logmon is a local broker daemon that collects structured logs (GELF over UDP+TCP) and OpenTelemetry traces (OTLP over gRPC+HTTP) from running applications and serves them over a Unix domain socket. You read it via the MCP tools listed below, or via the `logmon-mcp <verb>` CLI.

Use logmon when the user wants to know **what the running program actually did**. Source-level reasoning isn't enough for those questions — open the broker first, then go back to the code.

## When to reach for logmon

Reach for it when any of these are true:

- The user says "logs," "traces," "errors," "panic," "crash," "slow," "timeout," "what happened," "investigate," or "debug at runtime."
- A test just failed and the failure isn't obviously in the test code.
- A user-reported bug describes a behavior, not a code path.
- You're about to insert `println!` / `console.log` / `print` to understand control flow — query logmon first.

## When NOT to reach for logmon

Skip it (and say so) when:

- The user is asking about a `.log` file on disk, or an archived log from yesterday — logmon is in-memory and live only.
- The broker isn't running and the project doesn't ship telemetry. Don't try to invent log lines.
- The question is purely about source structure or types — read code, not logs.

## Tool selection at a glance

| You want to … | Call |
|---|---|
| See the most recent activity | `get_recent_logs` |
| Find errors / panics | `get_recent_logs(filter="l>=ERROR")` or `…mfm=panic` |
| Investigate a known entry's context | `get_log_context(seq=N)` |
| Find a slow request | `get_slow_spans(min_duration_ms=100)` |
| Drill into one request end-to-end | `get_trace(trace_id=…)` |
| Get the timing breakdown of a trace | `get_trace_summary(trace_id=…)` |
| Compare before/after a code change | `add_bookmark` → make change → query with `b>=name` |
| Stream "what's new since I last checked" | `c>=name` filter (cursor — see below) |
| Get notified when X happens later | `add_trigger(filter=…, pre_window=…, post_window=…)` |
| See the daemon's health | `get_status` |

## Quick commands (Claude Code slash-command UX)

If the host is Claude Code, the user can type `/logmon <args>`. Execute the matching action immediately; don't echo the menu. The slash-command `count` defaults below (`50`, `10`) intentionally differ from `get_recent_logs`'s underlying default of `100` — pick a sensible size for the action.

- `/logmon` — `get_recent_logs(count=50)`, summarize.
- `/logmon errors` — `get_recent_logs(filter="l>=ERROR")`, summarize.
- `/logmon warnings` — `get_recent_logs(filter="l>=WARN")`, summarize.
- `/logmon recent [count]` — `get_recent_logs(count=<count>)`, default 50.
- `/logmon status` — `get_status`, report.
- `/logmon clear` — `clear_logs`. Warn the user this affects every session.
- `/logmon fixed` — `get_recent_logs(filter="l>=ERROR", count=10)`. If empty, "looks fixed"; else show.
- `/logmon watch <filter>` — `add_filter(filter=<filter>)`.
- `/logmon unwatch` — `get_filters`, then remove each one.
- `/logmon sessions` — `get_sessions`, summarize.
- `/logmon traces` — `get_recent_traces`, summarize.
- `/logmon slow` — `get_slow_spans` with default threshold, summarize bottlenecks.
- `/logmon trace <trace_id>` — `get_trace(trace_id=…)`.
- `/logmon <DSL expr>` — if the argument contains `=`, `>=`, `/regex/`, or a known selector (`fa=`, `l>=`, `h=`, `m=`, `sn=`, `sv=`, `d>=`), call `get_recent_logs` (log selectors) or `get_slow_spans` (span selectors) with that filter.
- `/logmon help` — print this list. Do **not** call any tools.

On other MCP hosts (Cursor, Windsurf, Codex, etc.) the user invokes via natural language ("show me errors", "what's slow"); the rest of this document applies unchanged.

## CLI fallback

The same operations are available as `logmon-mcp <subcommand>`. Use the CLI when:

- You're inside a subagent — agents spawned via the `Agent` tool don't inherit MCP servers.
- The MCP connection has dropped mid-session.
- You want to pipe through `jq`, `grep`, or `head`.

Mapping is mechanical: `get_recent_logs` ↔ `logmon-mcp logs recent`, `add_bookmark` ↔ `logmon-mcp bookmarks add`. Add `--json` for machine-readable output. CLI invocations default to a named session called `"cli"` so state persists across calls.

## Architecture (one-paragraph version)

`logmon-broker` is a long-running daemon that ingests GELF (UDP/TCP on `12201`) and OTLP (gRPC `4317`, HTTP `4318`), stores logs and spans in in-memory ring buffers, correlates them by `trace_id`, and serves multiple clients over `~/.config/logmon/logmon.sock` via JSON-RPC 2.0. `logmon-mcp` is the thin MCP shim — one per editor session, all sharing the same broker. Each session owns its triggers, filters, and bookmarks; named sessions persist across reconnects and daemon restarts.

## Available tools

### Logs

- **`get_recent_logs(count?, filter?, trace_id?)`** — newest-first by default; **oldest-first** when the filter contains `c>=` (cursor). Default `count=100`.
- **`get_log_context(seq, before?, after?)`** — logs around a specific entry. Use this when you have a `seq` from another query.
- **`export_logs(path, count?, filter?, format?)`** — write matching logs to a file (`json` or `text`).
- **`clear_logs()`** — clear the in-memory buffer. **Shared across all sessions.** Prefer bookmarks for "see only what happens next" — see below.

### Filters (per-session, shape what gets stored)

- **`get_filters` / `add_filter(filter, description?)` / `edit_filter(id, …)` / `remove_filter(id)`** — when any filter exists, only matching records are stored. OR semantics across filters within a session; the union across all sessions is what the broker keeps.

### Triggers (per-session, push notifications)

- **`get_triggers` / `add_trigger(filter, pre_window?, post_window?, notify_context?, oneshot?, description?)` / `edit_trigger(id, …)` / `remove_trigger(id)`**.
- Defaults on every new session: `l>=ERROR` and `mfm=panic`.
- `pre_window` captures **unfiltered** context before the match (flight recorder). `post_window` captures after. `notify_context` is how many of the pre-window entries ride along in the notification.
- `oneshot=true` removes the trigger after the first match — useful for "tell me the next time this happens."

### Bookmarks (named seq positions)

- **`add_bookmark(name, start_seq?, description?, replace?)` / `list_bookmarks(session?)` / `remove_bookmark(name)` / `clear_bookmarks(session?)`**.
- A bookmark is just a `(session, name) → seq` mapping. Two operators use it: `b>=` (pure read) and `c>=` (read-and-advance).

### Sessions

- **`get_sessions` / `drop_session(name)`** — list connected sessions; remove a named session and its state.

### Traces (OTLP)

- **`get_recent_traces(count?, filter?)`** — index page: trace id, root span, total duration, error flag.
- **`get_trace(trace_id, include_logs?, filter?)`** — full span tree + linked logs. `include_logs` defaults to **`true`** — only pass `false` if you specifically want just the spans.
- **`get_trace_summary(trace_id)`** — timing breakdown of the root span's direct children, with percentages.
- **`get_slow_spans(min_duration_ms?, count?, filter?, group_by?)`** — slow individual spans, or aggregates when `group_by="name"`. Defaults: `min_duration_ms=100`, `count=20`.
- **`get_span_context(seq, before?, after?)`** — spans surrounding a given span.
- **`get_trace_logs(trace_id, filter?)`** — only the logs linked to one trace.

### Status

- **`get_status()`** — uptime, receivers, store stats, **`receiver_drops`** counts. Check the drop counts when investigating "missing logs."

## Filter DSL

Comma-separated qualifiers, AND-ed within a filter. Multiple filters on a session OR together.

| Pattern | Meaning |
|---|---|
| `text` | case-insensitive substring against all fields |
| `/regex/` | regex (add `/i` for case-insensitive) |
| `selector=pattern` | match against a specific field |
| `l>=L` / `l<=L` / `l=L` | level filter (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`) |
| `b>=name` / `b<=name` | match records strictly after / before the bookmark's seq |
| `c>=name` | cursor: same as `b>=` but advances the bookmark to the highest returned seq |
| `"quoted"` | literal — use when the value contains commas or `=` |
| `ALL` / `NONE` | match everything / nothing |

Only `>=` and `<=` are accepted for `b`, `d`, and `l`; only `>=` for `c` (`c<=` is rejected by the parser). The level filter additionally allows `l=` for an exact match.

> **Off-by-one note:** despite the `>=` / `<=` syntax, `b>=name` matches records with seq **strictly greater** than the bookmark's seq, and `b<=name` strictly less. The bookmark's own record is never included on either side. Same applies to `c>=`.

**Log selectors:** `m` (message), `fm` (full_message), `mfm` (either), `h` (host), `fa` (facility), `fi` (file), `ln` (line), `l` (level). Any other selector (e.g. `user_id`, `request_id`) is treated as a GELF additional field — drop the leading underscore that GELF uses on the wire (`user_id=42`, not `_user_id=42`).

**Span selectors:** `sn` (span name), `sv` (service), `st` (status: `ok|error|unset`), `sk` (kind: `server|client|producer|consumer|internal`), `d>=` / `d<=` (duration ms).

Log selectors and span selectors **cannot mix in the same filter** — they target different stores. Log filters apply to `get_recent_logs`, `get_log_context`, `export_logs`, `get_trace_logs`; span filters apply to `get_recent_traces`, `get_slow_spans`, `get_span_context`, and to the `filter` argument of `get_trace`.

Examples:

```
l>=ERROR                      all errors and worse
fa=mqtt, l>=WARN              warnings+ from the mqtt facility
connection refused, h=myapp   substring match + host
/panic|unwrap failed/         regex for panics
m="POST /users, 200"          literal — needed because of the comma
user_id=42, l>=WARN           custom GELF field, no underscore prefix
sn=query_database, d>=100     spans named query_database taking ≥100 ms
sv=auth, st=error             error spans from the auth service
b>=before, b<=after           records strictly between two bookmarks
c>=test-run-abc               records since last poll, advances the cursor
```

## Bookmarks: when and how

Use a bookmark instead of `clear_logs` when you want a clear before/after boundary but don't want to lose history.

Reach for them when:

- You're about to start a flaky operation and want to inspect just that range later.
- You're comparing two attempts at the same operation — bookmark each attempt's start.
- You'd otherwise call `clear_logs` to "see only what happens next."

```
add_bookmark("before-deploy")
# … run the operation …
add_bookmark("after-deploy")
get_recent_logs(filter="b>=before-deploy, b<=after-deploy, l>=warn")
get_recent_traces(filter="b>=before-deploy, b<=after-deploy, d>=100")
```

Naming: bookmarks are stored as `{session}/{name}`. Bare `before` in a query resolves to your own session; `other/before` reaches into another session's bookmarks (pure-read across sessions is fine; cross-session **advance** with `c>=` is rejected).

`b>=`, `b<=`, and `c>=` are query-only — rejected by `add_filter` and `add_trigger`.

## Cursors: "what's new since I last checked"

A cursor is a bookmark used with `c>=` instead of `b>=`. Every read with `c>=` atomically advances the bookmark to the highest seq returned, so the next read sees only what's new. No checkpoint state to thread through your own code.

```
# First call — if the bookmark doesn't exist, it's auto-created at seq=0,
# so this returns everything currently in the buffer matching the filter.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)

# Subsequent calls — only the delta since the previous call.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)
```

Results are returned **oldest-first** when `c>=` is present, so a paginated drain stays monotonic.

`c>=` is allowed in `get_recent_logs`, `export_logs`, and `get_trace_logs`. Rejected in `get_log_context`, `get_recent_traces`, `get_trace_summary`, `get_slow_spans`, `get_trace`, and `get_span_context` — their results are anchor-driven or aggregated, not seq-streamable. Only one `c>=` per filter.

To pre-position a cursor at "now" (so the first read returns only future records), call `add_bookmark("name")` first — the default `start_seq` is the current seq counter.

## Triggers vs bookmarks: which one?

| Use a bookmark when… | Use a trigger when… |
|---|---|
| You know roughly when the interesting thing happens and want to query that range later. | You don't know when it'll happen and want to be told. |
| You're doing a manual before/after comparison. | You want pre/post context captured automatically around the event. |
| You're polling regularly (cursor). | You want push notifications. |

A bookmark is passive metadata; a trigger is an active watcher with windowed context capture.

## Worked patterns

### Pattern: debug a specific module

```
result = add_filter(filter="fa=<module>")   # returns the new filter's id
# … ask the user to reproduce …
get_recent_logs(count=100)                  # examine
remove_filter(id=result.id)                  # restore full capture when done
```

(`add_filter`, `add_trigger`, and `add_bookmark` accept either positional or named arguments; this skill uses named for clarity.)

### Pattern: before/after a change

```
add_bookmark("before-change")
# … make the change, restart the service, etc …
get_recent_logs(filter="b>=before-change, l>=warn")
get_recent_traces(filter="b>=before-change, d>=100")
```

### Pattern: catch the next occurrence of a rare event

```
add_trigger(
    filter   = "mfm=connection refused, fa=db",
    pre_window  = 500,
    post_window = 200,
    notify_context = 10,
    oneshot = true,
)
# Continue working. When it fires, you'll be notified with surrounding context.
```

### Pattern: investigate a slow request end-to-end

```
get_slow_spans(min_duration_ms=200, group_by="name")
# ↑ pick a span name that stands out, then find a specific trace:
get_recent_traces(filter="sn=<that-name>, d>=200", count=5)
# ↑ note a trace_id, then:
get_trace_summary(trace_id="<id>")        # where did the time go?
get_trace(trace_id="<id>")                 # full span tree + logs interleaved
```

### Pattern: log + trace correlation in one shot

This pattern only works when the application is exporting OTel traces **and** emitting logs (GELF or otherwise) with a `trace_id` field — e.g. `tracing-init`'s GELF layer, OTel auto-instrumented HTTP middleware, etc. If logs don't carry a trace id, fall back to timestamp-based correlation.

When a user reports "this request was broken" and gives you a `trace_id`:

```
get_trace(trace_id="<id>")     # include_logs defaults to true
# Returns the span tree AND every log line linked to that trace.
# Now you have timing AND log context in one response.
```

When they only give you a timestamp or symptom, find the trace first:

```
get_recent_logs(filter="l>=ERROR, h=<host>")     # find the error log
# Note the trace_id field on the matching entry, then:
get_trace(trace_id="<that_id>")
```

### Pattern: paginated drain of a long burst

```
loop:
    r = get_recent_logs(filter="c>=drain, l>=warn", count=500)
    if r.logs is empty: break
    process(r.logs)
# Cursor auto-advances each call; oldest-first ordering keeps it monotonic.
```

### Pattern: zoom in on the context around an error

```
r = get_recent_logs(filter="l>=ERROR", count=5)   # find the error(s)
# Each entry carries a `seq`. Pick the one you care about:
get_log_context(seq=r.logs[0].seq, before=20, after=10)
# Returns 20 entries before and 10 after, regardless of level/filter —
# the full unfiltered run-up to and recovery from the error.
```

### Pattern: comparing two test attempts

```
add_bookmark("attempt-1-start")
# … run test attempt 1 …
add_bookmark("attempt-1-end")
add_bookmark("attempt-2-start")
# … run test attempt 2 …
add_bookmark("attempt-2-end")

get_recent_logs(filter="b>=attempt-1-start, b<=attempt-1-end")  # 1's logs
get_recent_logs(filter="b>=attempt-2-start, b<=attempt-2-end")  # 2's logs
```

## When things look wrong

### "I'm getting zero logs back"

In order:

1. `get_status` — is the broker even running? Is uptime sensible? Are receivers listed?
2. Does the application emit telemetry yet? Many projects send GELF only after a feature flag flips. Ask the user to trigger an action that should produce a log.
3. Is a filter narrowing the buffer? `get_filters` — if filters exist, the buffer only stores matches. Remove them or widen.
4. Did someone (you, another session) call `clear_logs`? The buffer is shared.
5. Check `receiver_drops` on `get_status`. Non-zero means the receivers couldn't keep up — the user's app is over-producing; suggest bumping `buffer_size` in `~/.config/logmon/config.json`.

### "My cursor returned a huge unexpected flood"

A cursor was idle long enough that its seq fell off the ring buffer. The broker auto-recreated it at `seq=0`, so it returned the entire current buffer. A WARN-level log entry was emitted by the broker noting the rollover. Either poll the cursor more often or raise `buffer_size`.

### "A trigger isn't firing"

1. Triggers evaluate **every** incoming log, regardless of filters. So a filter isn't the cause.
2. Triggers skip evaluation during another trigger's `post_window` to prevent cascading. If two triggers want overlapping events, expect coalescing.
3. CLI mode invocations can't receive trigger fires — the CLI process exits before any log can match. Use the MCP shim or the SDK for that.
4. Confirm the trigger exists: `get_triggers`.
5. Verify the filter actually matches incoming logs by trying the same filter in `get_recent_logs`.

### "The broker isn't running"

If the MCP shim is connected, the broker is running by definition. If you're hitting the CLI and seeing "broker not running":

```
logmon-broker status                       # check
logmon-broker install-service --scope user # install as launchd/systemd, start
```

Don't suggest editing `~/.config/logmon/state.json` or `daemon.pid` by hand unless the user explicitly asks — those are managed.

### "I cleared logs and now I have no context"

`clear_logs` is shared across all sessions and destructive. There's no undo. Going forward, prefer `add_bookmark("checkpoint")` + `b>=checkpoint` for scoped queries — same outcome, no data loss, and other sessions aren't affected.

## Multi-session behavior

- **Shared:** the log/span ring buffers, the receivers, `clear_logs`.
- **Per-session:** triggers, filters, bookmarks, queued notifications.
- **Anonymous sessions** (no `--session` flag): cleaned up on disconnect.
- **Named sessions** (`logmon-mcp --session NAME`): persist across disconnect; trigger fires queue while disconnected and replay on reconnect; state survives daemon restart via `state.json`.

Filters are unioned across sessions (the broker stores anything any session's filters match), so adding a narrow filter in your session doesn't hide records from another session — but if every session has a narrow filter, only the union is stored.

## SDK consumers (non-MCP)

For test harnesses, dashboards, archival workers, anything that isn't an MCP client, point them at the typed Rust SDK at `crates/sdk` (`logmon-broker-sdk`). The SDK speaks the same JSON-RPC protocol, builds filter strings without manual escaping, and includes a reconnect state machine for named sessions. Cross-language clients can codegen from `crates/protocol/protocol-v1.schema.json` (drift-guarded by `cargo xtask verify-schema`).

If a user mentions building a custom integration, redirect them to `crates/sdk/README.md` rather than wrapping the MCP shim.
