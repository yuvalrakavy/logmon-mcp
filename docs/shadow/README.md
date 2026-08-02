# logmon

**Real-time logs and traces for AI coding assistants. Stop letting your AI debug blind.**

logmon is a local broker daemon that collects structured logs and OpenTelemetry traces from your running application and exposes them to AI coding assistants over MCP. Instead of reading source code and guessing, your assistant can pull the actual runtime telemetry — what got logged, which span was slow, what fired the error — the same way a human would tail a log.

It's a single binary you start once. Your apps emit GELF or OTLP. Your assistant connects over MCP. Multiple AI sessions, a CLI, and any Rust process linking the SDK can all observe the same stream in parallel.

## Why logmon

- **Built for AI debugging loops.** Triggers, bookmarks, and cursors give the assistant a way to say "show me everything that happened between *before-fix* and *after-fix*, errors only" — without you copy-pasting log snippets into the chat.
- **Logs and traces in one place, correlated.** OTLP receivers ingest both; logs and spans share a `trace_id` so the assistant can pivot from a slow span to its log lines and back.
- **Multi-session by design.** Several Claude Code / Cursor / Windsurf sessions can attach to the same broker simultaneously. Each session has its own triggers and filters; the buffer is shared.
- **Survives reconnects.** Named sessions persist filters, triggers, and bookmarks across daemon restarts. Disconnected sessions queue notifications.
- **Backpressure-aware.** UDP gets an 8 MB receive buffer; OTLP returns 429 / UNAVAILABLE at ~80% channel fill; per-source drop counts surface in `status.get`. A misbehaving producer can't take the broker down.
- **Says what it could not record.** Storage is conditional — a filter held by any session narrows what is kept — so every range read carries a verdict telling absence of *cause* from absence of *recording*. `create_case` freezes a window to disk with that verdict at the top of the document, before anything it qualifies.
- **Same surface from MCP, CLI, and Rust.** The `logmon-mcp` binary doubles as a shell-friendly CLI (`logmon-mcp logs recent --json`). The `logmon-broker-sdk` crate gives Rust consumers a typed client. Other languages can codegen from `crates/protocol/protocol-v1.schema.json`.

## Architecture

```
   Your app(s)              AI assistant sessions          Other clients
                                                       (test harnesses,
                                                        dashboards, CI)
   GELF UDP/TCP  ───┐                │  │  │                   │
   OTLP gRPC/HTTP ──┤                │  │  │                   │
                    │           logmon-mcp (MCP stdio)         │
                    │           logmon-mcp (CLI)               │
                    │           logmon-mcp (CLI)        logmon-broker-sdk
                    ▼                │  │  │                   │
            ┌─────────────────────────────────────────────────────┐
            │                  logmon-broker                      │
            │       long-lived daemon, JSON-RPC over UDS          │
            │   receivers → pipeline → ring buffers (logs+spans)  │
            │   per-session triggers / filters / bookmarks        │
            └─────────────────────────────────────────────────────┘
```

The workspace has four crates that ship as one project:

| Crate | What it is |
|---|---|
| `logmon-broker` (`crates/broker`) | The daemon. Owns the receivers, ring buffers, and the JSON-RPC UDS server. |
| `logmon-mcp` (`crates/mcp`) | Dual-mode binary. As a stdio MCP server it bridges AI clients to the broker. With a subcommand it acts as a CLI that mirrors the MCP surface 1:1. |
| `logmon-broker-sdk` (`crates/sdk`) | Typed Rust client. Talks JSON-RPC against the broker, exposes a typed notification stream, includes a filter-DSL builder and a reconnect state machine. |
| `logmon-broker-protocol` (`crates/protocol`) | The wire types. Drift-guarded JSON Schema at `crates/protocol/protocol-v1.schema.json` for cross-language clients. |

**Domains.** The broker can host multiple isolated **domains** — each a full instance with its own receivers (ports), ring buffers, and per-session triggers/filters, so unrelated log streams never interleave. The `default` domain is the always-on anchor; declare durable ones in `config.json` (see [Configuration](#configuration)) or create ephemeral ones at runtime. A session targets one via `use_domain` (MCP) or the `--domain` flag (CLI). For a per-worktree / per-project setup, set **`LOGMON_DOMAIN`** in the MCP server's env once — **alongside a named `--session`** — so the shim binds that domain at connect and **re-binds it on every reconnect** (durable across daemon restarts). Reconnect-preservation needs a named session: an anonymous session can't resume a restart, so it fails *loud* (never a silent revert to `default`) and the shim is restarted. Every session then auto-scopes with zero per-call ceremony. Create the domain before the shim connects; a missing domain is a loud handshake error, not a silent fallback.

## Installation

### Build

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp.git
cd logmon-mcp
cargo install --path crates/broker
cargo install --path crates/mcp
```

This puts `logmon-broker` and `logmon-mcp` on your PATH (`~/.cargo/bin` by default).

### Run the broker as a service (recommended)

```bash
logmon-broker install-service --scope user
```

This registers a launchd agent on macOS or a systemd user unit on Linux. The broker starts at login and restarts on crash. To remove it: `logmon-broker uninstall-service --scope user`.

If you skip this, the MCP shim auto-starts the broker the first time a client connects.

### Wire up your AI assistant

<details>
<summary><b>Claude Code — plugin install (recommended)</b></summary>

Install the bundled Claude Code plugin from the `claude-tools` marketplace. One marketplace add, one plugin install, and you're done — the MCP server registration and the skill come along automatically:

```
/plugin marketplace add yuvalrakavy/claude-tools
/plugin install logmon-mcp@claude-tools
```

The marketplace lives in [yuvalrakavy/claude-tools](https://github.com/yuvalrakavy/claude-tools) and hosts every Claude Code plugin in this author's stack — you only add it once, then install whichever plugins you want from it.

Prerequisite: the `logmon-mcp` binary must already be on your `PATH` (`cargo install --path crates/broker` then `cargo install --path crates/mcp`, or once published, `cargo install logmon-mcp`). The plugin manifest references the binary; it doesn't bundle it.

To update later: `/plugin marketplace update claude-tools`. To remove: `/plugin uninstall logmon-mcp@claude-tools`.

</details>

<details>
<summary><b>Claude Code — manual MCP registration</b></summary>

If you prefer not to use the plugin system, register the MCP server directly:

```bash
# Global install — available in every project
claude mcp add logmon --scope user -- logmon-mcp

# Or with a named session that persists triggers/filters across reconnects
claude mcp add logmon --scope user -- logmon-mcp --session my-session

# Project-local install
claude mcp add logmon -- logmon-mcp
```

If `logmon-mcp` isn't on your PATH, give the absolute path: `claude mcp add logmon --scope user -- /full/path/to/logmon-mcp`.

</details>

<details>
<summary><b>Cursor</b></summary>

Add to `.cursor/mcp.json` in your project, or `~/.cursor/mcp.json` for global:

```json
{
  "mcpServers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": ["--session", "cursor"]
    }
  }
}
```

</details>

<details>
<summary><b>Windsurf</b></summary>

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": []
    }
  }
}
```

</details>

<details>
<summary><b>VS Code (GitHub Copilot)</b></summary>

Add to your VS Code `settings.json`:

```json
{
  "mcp": {
    "servers": {
      "logmon": {
        "command": "logmon-mcp",
        "args": []
      }
    }
  }
}
```

Or, per-project, `.vscode/mcp.json`:

```json
{
  "servers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": []
    }
  }
}
```

</details>

<details>
<summary><b>Gemini CLI</b></summary>

Add to `~/.gemini/settings.json`:

```json
{
  "mcpServers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": []
    }
  }
}
```

</details>

<details>
<summary><b>OpenAI Codex CLI</b></summary>

Add to `~/.codex/config.json`:

```json
{
  "mcpServers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": []
    }
  }
}
```

</details>

<details>
<summary><b>Any MCP-compatible client</b></summary>

logmon-mcp speaks the standard MCP stdio transport. Configure your client to launch `logmon-mcp` as a stdio server. The only optional argument is `--session <name>` to attach to a persistent named session.

</details>

### Three ways the skill reaches your assistant

`skill/logmon.md` is a structured guide that teaches the assistant how to use the MCP tools above effectively — when to query for logs vs. traces, the filter DSL, the bookmark/cursor model, the trigger flight-recorder pattern, a Claude-Code-only `/logmon` slash-command reference, and a recovery guide for the common failure modes.

Depending on how you installed logmon, the skill reaches the assistant through one of three channels — pick whichever fits:

1. **Claude Code plugin (recommended for Claude Code).** If you installed via `/plugin install logmon-mcp@claude-tools`, the plugin registers the skill alongside the MCP server. Claude Code surfaces it on-demand via the skill's natural-language triggers and exposes the `/logmon-mcp:logmon` slash-namespaced form.
2. **Embedded in the MCP server (every other host).** `logmon-mcp` embeds `skill/logmon.md` at compile time and returns it as the MCP server's `instructions`. Any MCP host that honors server instructions (Cursor, Codex, …) picks it up automatically when the server is registered.
3. **Manual install.** Drop the file into a skills directory for Claude Code if you want to use the skill in a project where the plugin route doesn't apply, or if you want to customize the `/logmon` aliases locally:

   ```bash
   # Per project
   mkdir -p .claude/skills && cp /path/to/logmon-mcp/skill/logmon.md .claude/skills/

   # Or once for all projects on the machine
   mkdir -p ~/.claude/skills && cp /path/to/logmon-mcp/skill/logmon.md ~/.claude/skills/
   ```

   If you tweak the on-disk copy, it takes precedence over the embedded version for hosts that honor it.

## Wire up your application

Point your app's structured-logging output at one of the broker's receivers. GELF is easiest for log-only workloads; OTLP gets you traces too.

### GELF (logs only)

Default port: **12201** (UDP and TCP, same port).

| Language | Library |
|---|---|
| Rust | [`tracing-init`](https://github.com/yuvalrakavy/tracing-init) (sister crate — also handles OTLP), or [`tracing-gelf`](https://crates.io/crates/tracing-gelf) |
| Python | [`pygelf`](https://pypi.org/project/pygelf/) |
| Node.js | [`gelf-pro`](https://www.npmjs.com/package/gelf-pro) |
| Go | [`go-gelf`](https://github.com/Graylog2/go-gelf) |
| Java | Logback `biz.paluch.logging:logstash-gelf` |
| Docker | `--log-driver=gelf --log-opt gelf-address=udp://localhost:12201` |

If you're writing your app in Rust, [`tracing-init`](https://github.com/yuvalrakavy/tracing-init) is the path of least resistance: a single `TracingInit::builder("myapp").init()` wires up both GELF and OTLP, with `logging.toml` auto-discovery if you want config-driven overrides. Its defaults already match logmon's ports, so the typical setup is zero-arg.

### OTLP (logs + traces, correlated by trace_id)

Default ports: **4317** (gRPC), **4318** (HTTP/protobuf).

Configure your OpenTelemetry SDK to export to `http://localhost:4318` or `grpc://localhost:4317`. logmon accepts both logs and traces on either transport. Spans linked to your logs through `trace_id` / `span_id` show up correlated in `get_trace`.

## MCP tool reference

| Tool | Description |
|---|---|
| `get_recent_logs` | Fetch recent logs, optionally filtered or scoped to a `trace_id`. |
| `get_log_context` | Get logs surrounding a specific entry by `seq`. |
| `export_logs` | Save logs to a file (json or text). `from_seq`/`to_seq` bound an **inclusive** window and compose with a bookmark bound. Every reply carries a **`verdict`** — `complete` / `filtered` / `evicted` / `cannot_verify` — saying how much of that window the daemon can vouch for, with `narrowed_by` naming any session filter that was narrowing what got stored, and over which seqs. See [Evidence verdicts](#evidence-verdicts). |
| `export_spans` | The same inclusive seq range over the span ring, for pairing spans with the logs of one window. Reports its **own** retention: the two stores share a seq axis but evict independently, so a complete log window says nothing about whether its spans survived. |
| `create_case` | Capture a window as three files on disk — a markdown document you read to decide whether this is your bug, and two JSONL evidence files you consult once you have decided it is. `dir` is required and must be **absolute**; the anchor is tagged (`{seq}` / `{bookmark}` / `{trace_id}`) rather than sniffed, and an unresolvable one is an error rather than a document with no headline. `data` is `update_domain_data` in the same call; a key with a leading `@` is asserted about **this capture alone** and never enters the domain registry. |
| `update_domain_data` / `get_domain_data` / `remove_domain_data` | A per-domain key/value registry recording what was true of the project while the logs were produced — the commit, the build profile, the scenario. Two timestamps per key, never one: set six days ago and never revisited is a guess, the same value confirmed five minutes ago is evidence. Staleness is reported as **age**, never as a verdict. |
| `clear_logs` | Clear the shared log buffer. |
| `get_recent_traces` | List recent traces with timing and error info. |
| `get_trace` | Full span tree for a trace; `include_logs` (default `true`) interleaves linked logs. |
| `get_trace_summary` | Compact timing breakdown highlighting bottlenecks. |
| `get_slow_spans` | Find slow spans (default `min_duration_ms=100`, `count=20`). With `group_by="name"` the aggregates cover **every** stored span of that name, and `min_duration_ms` becomes a display floor deciding which names appear — so a name can qualify on its `max_ms` while its `avg_ms` sits far below the floor. |
| `get_span_context` | Spans surrounding a given span by `seq`. |
| `get_trace_logs` | All logs linked to a trace. |
| `add_collector` / `list_collectors` / `get_collector` / `remove_collector` | Span time collectors: arm a filter, run the workload, read exact totals, percentiles and self time. At small n the `sampled` block also carries **`durations_ms`** (every retained duration, in arrival order, when complete and ≤50) and **`stddev_ms`** — at three runs the percentiles are order statistics of three numbers, so the durations are what make "are these two arms actually separated?" a computation rather than a judgement. `skip_warmup_ms` reports its own effect as **`excluded_by_warmup`**, and a grouped read reports **`groups_total`** before `top_n` truncation. Needs a **named** session — an anonymous one's identity is a UUID that never returns, so anything it armed would be unreachable after a disconnect. `matched: 0` comes with `zeroed_by` (`snapshot` / `reset` / `edit` / `daemon_restart`, or absent for "no traffic yet"), so an empty window is never ambiguous. |
| `snapshot_collector` / `get_collector_history` | Record a window as a named run and start the next — the between-runs move for a before/after comparison. History carries each run's own definition, and `merge` reports the run-to-run spread so you can tell a real difference from noise. Survives a daemon restart; a run that could not be written reports `durable: false` rather than pretending otherwise. |
| `edit_collector` | Change an armed collector. Description is free; anything structural discards the live window (never the history). Re-pins a collector orphaned by a restart. |
| `diff_collectors` | Subtract two runs and report what moved. Arms are `<collector>`, `<collector>@<label>`, or `<collector>@*` (every recorded run merged — the only shape with a run-to-run floor, so the only one whose deltas can be told from noise). Every row carries **the threshold that was applied**, and estimated percentile rows carry the error on the delta: `α(a+b)/|a−b|`, which reaches ±199% for a 1% change. **Refuses rather than guessing** when the arms are not comparable, and names the flag that would permit it. |
| `document_collectors` | Write the measurement up for a reader who wasn't there: what moved, what to do next, and every caveat beside the number it qualifies. `md` (default), `json`, or `folded` for a flame graph. Returns bytes plus a sidecar — the client writes them. Regeneration is free, so `finding` normally arrives on a second call. |
| `add_collector(threshold=…)` | A rolling guard over `count` / `total_ms` / `avg_ms` / `error_count` / `error_rate_pct`. Evaluated on **span arrival, not a clock**, so an idle collector costs nothing — and so with no traffic a breached threshold neither fires nor clears. A load-time guard, not a liveness check; every report says so. |
| `reset_collector` | Zero a collector and **discard** the run. Prefer `snapshot_collector`. |
| `profile_traces` | The same numbers over spans already in the buffer, without arming anything. |
| `get_filters` / `add_filter` / `edit_filter` / `remove_filter` | Per-session buffer filters. |
| `get_triggers` / `add_trigger` / `edit_trigger` / `remove_trigger` | Per-session triggers. |
| `add_bookmark` / `list_bookmarks` / `remove_bookmark` / `clear_bookmarks` | Bookmarks (also act as cursors via `c>=`). |
| `get_sessions` / `drop_session` | Multi-session inspection. |
| `rename_session` | Rename this session in place — all state (domain binding, triggers, filters, bookmarks) survives. A name held by a *connected* session errors (deliberate: two live clients must not share an identity); a *disconnected* holder is displaced (reported via `displaced_stale_holder`). |
| `get_status` | Daemon uptime, receivers, store stats, per-source drop counts, **`trace_ingest`** (trace-transport loss before any collector saw it — see [Backpressure](#backpressure); its `dropped` is a repeat of two `receiver_drops` fields, so don't sum them), current domain + active filters, and per-listener `receiver_liveness`. Also **`broker_version`** and **`broker_tools`** — the MCP tools a shim of this broker's version exposes, so a client can tell it is out of date; a shim that finds itself short adds a **`shim_note`** naming the missing tools. See [Version skew](#version-skew). |
| `list_domains` / `create_domain` / `delete_domain` | Manage isolated domains (each with its own receivers, buffers, triggers). `list_domains` also reports per-domain liveness (last received / idle / stale) and `bound_sessions` — which sessions are bound to each domain (derived from the session registry; disconnected holders are suffixed). |
| `use_domain` | Bind this session to a domain for subsequent queries + notifications. |
| `clear_domain` | Dispose the bound domain's logs + spans (keeps the domain alive). |

## Filter DSL

Filters are comma-separated qualifier lists. Qualifiers inside one filter are AND-ed; multiple registered filters OR together.

```
l>=ERROR                       all errors and above
fa=mqtt, l>=WARN               warnings+ from the mqtt facility
connection refused, h=myapp    substring match scoped to a host
/panic|unwrap failed/          regex match
b>=before, b<=after, l>=warn   warnings between two bookmarks
c>=poll-1, count=500           drain mode (advances the cursor)
```

### Log selectors

| Selector | Field |
|---|---|
| `m` | message (GELF `short_message`) |
| `fm` | `full_message` |
| `mfm` | message or full_message |
| `h` | host |
| `fa` | facility |
| `fi` | file |
| `ln` | line |
| `l` | level (`>=`, `<=`, `=`; ERROR/WARN/INFO/DEBUG/TRACE) |
| `<name>` | any custom GELF field (`_foo` → `foo`) |

Special tokens: `ALL` matches everything; `NONE` matches nothing.

### Span selectors

Span filters use the same syntax but with span fields. You can't mix log and span selectors in a single filter.

| Selector | Field |
|---|---|
| `sn` | span name |
| `sv` | service name |
| `st` | status (`ok` / `error` / `unset`) |
| `sk` | span kind (`server` / `client` / `producer` / `consumer` / `internal`) |
| `d>=` / `d<=` | duration ms |

### Patterns

- `pattern` — bare text is a case-insensitive substring match against all fields.
- `/regex/` — regex (case-sensitive). Append `/i` for case-insensitive: `/foo/i`.
- `"quoted text"` — literal text. Use this when your value contains commas or `=`.

## Bookmarks and cursors

Bookmarks are named positions in the broker's globally-monotonic `seq` stream. Two interaction patterns share the same storage:

### `b>=` / `b<=` — pure read

Set a bookmark, read records strictly after / before it. The bookmark never moves on its own.

```
add_bookmark("before")
# ... run the operation ...
add_bookmark("after")
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, b<=after, d>=100")
```

### `c>=` — read and advance

Same as `b>=`, but atomically advances the bookmark to the max `seq` returned. Use when you want "what's new since I last checked":

```
# First call auto-creates the bookmark at seq=0 if missing,
# returns everything currently matching, advances the cursor.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)

# Subsequent calls return only records that arrived since the last call.
get_recent_logs(filter="c>=test-run, l>=ERROR", count=500)
```

When a `c>=` qualifier is present, results come back oldest-first within the cursor's window so paginated polls drain monotonically.

`c>=` is allowed in `get_recent_logs`, `export_logs`, and `get_trace_logs`. Other query methods reject it because their results aren't seq-streamable.

### Cross-session

Bookmarks are global, qualified by the creating session (`session/name`). A bare name resolves to the calling session.

- `b>=other-session/before` — pure-read across sessions is allowed.
- `c>=other-session/before` — rejected. Only the owning session can advance its own cursor.

Bookmarks auto-evict when both the log and span buffers have rolled past their `seq`.

`b>=`, `b<=`, and `c>=` are query-only — they're rejected by `add_filter` and `add_trigger`.

## Evidence verdicts

Storage is **conditional**. When any session bound to a domain holds a filter, only
matching entries are kept — so *"nothing appeared before the error"* is ambiguous between
**absence of cause** and **absence of recording**, and only the first is a conclusion.

Nothing a reader can see in the returned entries resolves that. `LogEntry.source` explains
why a **kept** entry was kept; no stored entry can testify about an absent one. Nor can
reading the live filter set at query time: filters are time-extended and that read is a
point. An anonymous session is removed on disconnect and takes its filters with it, so a
query minutes after the run sees nothing and would assert that nothing was narrowing a
window that recorded a tenth of it.

So the daemon keeps a **per-domain epoch log** — the first seq decided under each storage
policy, plus that policy — and `export_logs` reports against it:

| `verdict` | Means |
|---|---|
| `complete` | the window lay wholly inside one unfiltered epoch, nothing was evicted from it, and nothing was capped |
| `filtered` | a session filter was narrowing the store over part of it; **`narrowed_by`** names which filters, over which seqs |
| `evicted` | the ring dropped part of the window; `evicted_before_window` bounds how much |
| `cannot_verify` | no claim is possible — the store is empty, the window predates this daemon run, or the read was capped short of what matched |

Three properties worth knowing:

- **`cannot_verify` is the default when the field is absent**, so a reply from a broker too
  old to send it never reads as a clean bill of health.
- **The epoch records filter *strings*, not a boolean.** `edit_filter` replaces a condition
  in place and never moves a boolean, so a flag-only marker would report the new filter
  string over the old range — wrong information rather than missing information.
- **A disconnected named session's filters still narrow the store**, until the TTL sweep
  retires it. The verdict reflects that, which is precisely the case you cannot see by
  asking what filters *you* hold.
- **An unbounded read is judged over what the store can still answer for**, not over the
  whole seq axis: the window opens at the ring's own floor. Naming a `from_seq` beneath
  that floor is a different question — you asked about records that are gone, and the
  reply says `evicted` rather than reporting a narrower window as if you had asked for it.

A daemon restart forces `cannot_verify` for a window carried over from the previous run:
the epoch log opens at the seq this incarnation started from, so a restored domain cannot
speak for its predecessor's seqs. Nothing detects the restart — the property falls out of
the log being per-process.

## Rendered output

The daemon supplies presentation. A reply carries a **`_display`** string —
already formatted for a reader — whenever the caller asks for one and the daemon
has a renderer for that method. The CLI prints it instead of JSON; the MCP shim
hands it to the agent instead of a pretty-printed envelope.

**You do not ask for it explicitly.** The CLI requests it unless you pass
`--json`; the MCP route requests it unless the reply is being written to a file.
So `logmon-mcp status` is seven readable lines and `logmon-mcp status --json` is
exactly the result it always was, with no rendered field to strip in `jq`.

**Not every method has one, and that is the design.** A method with no renderer
returns its JSON unchanged, which is also what an older broker does — so the two
degrade identically and renderers can land one method at a time.

Which methods render follows one rule: **render where rendering removes noise;
leave a small flat result as JSON.** For an agent — the primary client — a reply
like `{"id": 3, "filter": "l>=ERROR"}` is already ideal: unambiguous,
machine-parseable, cheap. What costs an agent is volume. So:

| reply | rendered | why |
|---|---|---|
| `status.get` | yes | 1,967 bytes → ~300. Most of the difference is a list of 47 tool names an MCP client already holds |
| log / span / trace reads | yes | ~2× denser, and no JSON punctuation to mis-parse |
| list reads | yes | one row per thing; keys stated once rather than per record |
| mutations (`add_filter`, `clear_logs`, …) | no | already small and flat; rendering would save ~20 bytes and risk hiding a field |

A rendering **drops only the record array** — every other key on the result is
stated. That matters more than it sounds: `{"logs": [], "count": 0,
"scanned": 4000}` shown as just `(no logs)` would tell a reader the system was
quiet while four thousand records flowed past. The rendered form says so
outright, and it carries `verdict`, `truncated`, `evicted_before_window` and
`cursor_advanced_to` with it.

Long record lists are **cut at 50 records or 16 KB and say how many were left**,
because `export_logs` defaults to unbounded and a rendering that silently
returned 50 of 1,000 would read as the whole answer.

## Triggers

Triggers watch every incoming log and fire when a match occurs, capturing context:

- `pre_window` — logs captured *before* the matching event (flight recorder).
- `post_window` — logs captured *after* the matching event.
- `notify_context` — how many pre-window entries are inlined into the notification payload.
- `oneshot` — when `true`, the trigger auto-removes after its first match.

Each session automatically gets two triggers on startup: `l>=ERROR` and `mfm=panic`. The pre- and post-trigger captures bypass buffer filters, so context around a fire is never truncated by a narrow filter.

When a trigger fires, the client receives a notification with the matched entry and surrounding context. `get_triggers` reports both `match_count` (times it has fired) and `post_remaining` (entries still to pass before it can fire again — `0` means armed and live). If a trigger looks stuck, check `post_remaining` before suspecting the filter: a non-zero value means it is debounced, not broken.

**Span triggers behave differently.** A trigger whose filter targets span selectors is evaluated on a separate path: it fires on *every* matching span, with no debounce, so its `post_remaining` is always `0` and carries no information. Its `match_count` is counted normally, so it still answers "has this fired, and how often?".

A **log** trigger is **debounced by its own `post_window`**: while it is inside the window opened by its last match it does not fire again, so one burst produces one capture rather than one per entry. The debounce is strictly per trigger — it never suppresses a *different* trigger. That distinction matters when you arm a rare-event trigger alongside a noisy one (say `kind=deadlock` next to the built-in `l>=ERROR` on a busy stream): the noisy trigger firing constantly has no effect on whether the rare one is evaluated. Set `post_window: 0` to disable the debounce and count every match — but understand what it costs before you do.

`post_window` is **one knob doing two jobs**: it debounces the trigger AND it sets how much aftermath is captured. Setting it to `0` gives up the aftermath capture entirely, and removes the only rate limit on firing. A firing entry is expensive — it linearly scans the store, clones up to `pre_window` entries into the notification, and clones that again for delivery: roughly **0.6 µs for a normal entry versus ~200 µs for a firing one** at `pre_window: 500`. An undebounced trigger on a busy stream can therefore cap that domain's processor at a few thousand entries/second. Worse, notifications go through a bounded broadcast channel: if a client can't keep up, fires are **dropped with only a daemon-side warning** — the client is never told it missed them.

So reach for `post_window: 0` on a low-rate signal you must not miss a single instance of. For anything bursty, keep a window and read `match_count` as "captures", not "occurrences".

## Multi-session

- All sessions share the same log and span buffers and the same GELF/OTLP receivers.
- Each session has its own triggers, filters, and bookmarks.
- **Anonymous sessions** (default) get a UUID and clean up on disconnect.
- **Named sessions** (`--session NAME`) persist filters, triggers, and bookmarks across disconnects and across daemon restarts. Notifications are queued while disconnected.

## CLI mode

The same `logmon-mcp` binary is also a shell-friendly CLI. Subcommands mirror the MCP surface 1:1:

```bash
logmon-mcp logs recent --json | jq '.logs[] | select(.level=="Error")'
logmon-mcp bookmarks add release-rc1
logmon-mcp status
```

Global flags:

- `--session NAME` — connect to a named session. CLI mode defaults to `"cli"` so state persists across invocations.
- `--domain NAME` — scope this invocation to an existing domain (queries + `domains clear`); omitted → `default`. Does not persist across invocations. `domains create/delete/list` ignore it.
- `--json` — emit machine-readable JSON instead of human-readable text.

See `crates/mcp/README.md` for the full command reference.

Useful when:

- You're in a subagent that doesn't inherit MCP servers.
- The MCP server disconnected mid-session.
- You want to pipe output through `head`, `jq`, or `grep`.

CLI calls are one-shot: no reconnect, fast-fail with a 5-second call timeout, no auto-start of the broker. Run the broker as a service first.

## Configuration

Config and state live in `~/.config/logmon/` on both macOS and Linux:

| File | Contents |
|---|---|
| `config.json` | Daemon settings: ports, buffer sizes, idle timeout. |
| `state.json` | Persisted state: seq counter, named sessions and their triggers/filters/bookmarks. |
| `logmon.sock` | The JSON-RPC Unix domain socket. |
| `daemon.pid` | PID file. |
| `daemon.log` | Broker log output. |

Defaults:

```json
{
  "gelf_port": 12201,
  "otlp_grpc_port": 4317,
  "otlp_http_port": 4318,
  "buffer_size": 10000,
  "span_buffer_size": 10000,
  "idle_timeout_secs": 1800,
  "max_domains": 32,
  "stale_after_secs": 60
}
```

`max_domains` caps API-created domains (config/`default` don't count). `stale_after_secs` is the idle threshold above which `list_domains` reports a domain `stale` (`idle_secs` is always reported raw, so tune or ignore this to fit your workload's cadence). `session_ttl_secs` (default `86400` = 24 h) is the session TTL: a *disconnected* named session past it is disposed by a periodic sweep (interval TTL/10, clamped 60 s..1 h) — connected sessions never expire, whatever their age. This keeps per-launch generated names (e.g. `MyProject-Main-<uuid8>`) from accumulating forever.

**Config-declared domains.** Add a `domains` array to declare durable, isolated domains — each a full broker instance with its own receivers, buffers, and triggers — re-created on every boot:

```json
{
  "gelf_port": 12201,
  "domains": [
    { "name": "staging", "gelf_port": 12300, "otlp_grpc_port": 0, "otlp_http_port": 0 }
  ]
}
```

Ports are optional (omitted → auto-allocated; `0` → that receiver disabled) — declare explicit ports for a domain an external producer targets by a fixed port. Config domains hold no persisted data (empty buffers, fresh seq each boot); `domains delete` refuses them (edit `config.json`). A malformed or port-clashing entry is skipped with a warning; the daemon still starts. Query one with `logmon-mcp --domain staging logs recent`.

> **Deriving per-track ports?** The OTLP defaults `4317` (gRPC) and `4318` (HTTP) are **adjacent**, so a naive `base + N` stride collides at N≥1 (track 1's gRPC `4318` = track 0's HTTP `4318`). Stride OTLP by **≥2** per track, or use non-adjacent bases. (`gelf 12201+N / otlp_grpc 4317+2N / otlp_http 4318+2N` is collision-free.)

Environment variable overrides:

- `LOGMON_BROKER_BIN` — explicit path to `logmon-broker` (skips PATH lookup).
- `LOGMON_BROKER_SOCKET` — explicit broker socket path. Falls back to
  `$LOGMON_CONFIG_DIR/logmon.sock` if that is set, else `~/.config/logmon/logmon.sock`.
- `LOGMON_CONFIG_DIR` — relocate the whole config **and** state directory: `config.json`,
  `state.json`, `daemon.pid`, `daemon.lock`, `logmon.sock`, `daemon.log`, `collectors/`.
  Read by the **daemon and its clients**, so one variable stands up a second broker beside
  your live one:
  `LOGMON_CONFIG_DIR=/tmp/probe logmon-broker --gelf-port 0 --otlp-grpc-port 0 --otlp-http-port 0`
  and any `logmon-mcp` invocation in the same environment finds it.

  Three things worth knowing before you use it:
  - **It moves state, not ports.** A broker in the new directory reads *that* directory's
    `config.json` — so with no file there it runs on stock defaults (10 000-entry buffers,
    no declared domains, GELF 12201), which will collide with your live daemon. Pass
    explicit ports as above, or put a `config.json` with non-conflicting ports in the new
    directory.
  - **It must be an absolute path.** A relative one is ignored (with a warning in the
    daemon log), because processes share an environment but not a working directory — a
    relative value would point the daemon and its clients at different places.
  - **Auto-start refuses** in a relocated directory that has no `config.json`, rather than
    spawning a broker that would bind the default ports; the error tells you the command
    to run. An empty value is ignored throughout (`VAR= cmd` means "unset for this
    invocation").

  Intended for tests and throwaway instances — the managed service is unaffected, because
  launchd/systemd do not inherit your shell's environment. `LOGMON_BROKER_SOCKET` still
  wins for clients.

## Backpressure

A noisy producer should slow itself down, not take the broker down. Concretely:

- GELF receivers use `try_send` into the pipeline channel — full channel means the entry is dropped at the receiver, not enqueued without bound.
- GELF UDP sets `SO_RCVBUF` to **8 MB** so a slow consumer has a sizeable OS-side cushion before datagrams start falling on the floor.
- OTLP gRPC and OTLP HTTP both check channel fill before consuming a payload. At **≥ 80% full**, gRPC returns `UNAVAILABLE` and HTTP returns `429`. The producer is expected to retry with backoff. The protocol-level rejection *is* the backpressure signal — per-source drop counters aren't bumped, because nothing was silently dropped.
- Per-source drop counts surface in `status.get` under `receiver_drops` (`gelf_udp`, `gelf_tcp`, `otlp_http_logs`, `otlp_http_traces`, `otlp_grpc_logs`, `otlp_grpc_traces`). Healthy operation keeps all six at zero.
- Trace-transport loss surfaces separately under `trace_ingest` (`dropped`, `shed_batches`, `malformed_dropped`) — spans lost before any collector saw them, so non-zero means every span-derived figure is a lower bound. `shed_batches` counts request **bodies** refused with 429/UNAVAILABLE, not spans: the bodies were never parsed, so how many spans they held is unknowable. **`dropped` is not a separate quantity** — it is exactly `receiver_drops.otlp_http_traces + otlp_grpc_traces`, reported again so the three trace figures read as one block, so **adding it to those two double-counts**.

If you're seeing nonzero **drops**, the broker is the bottleneck — bump `buffer_size` /
`span_buffer_size`, or check whether a runaway producer is genuinely outpacing the consumer.
That remedy is for channel-full drops only: a `shed_batches` count means the producer was
told to back off and should retry, and a `malformed_dropped` span was refused for cause (an
unusable trace id) — no buffer size changes either.

## Version skew

The broker and the MCP shim are separate binaries with separate lifetimes: the broker runs
as a long-lived service, while the shim is spawned per client session. The **tool list is
compiled into the shim**, so upgrading the broker alone cannot make a new tool appear —
and for a long time nothing said so. A project once filed a report proposing three
collector features that already shipped, because their shim was several versions behind
and there was no way to tell.

`status.get` now carries two facts, and they are the fix:

```json
"broker_version": "0.9.0",
"broker_tools": ["add_bookmark", "add_collector", …]
```

`broker_tools` is the MCP tool names a shim built at *this broker's* version exposes — tool
names, not RPC method names, because that is the vocabulary a client holds. Compare it
against the tools you actually have; anything listed but absent is out of reach until the
shim is reinstalled.

A shim from 0.9.0 onward does that comparison itself and adds a `shim_note` naming the
gap and the command to fix it. Nothing is added when the sets match.

**Why this lands on `status.get` rather than the handshake:** `get_status` relays the
broker's JSON verbatim and always has, so these fields are rendered by *every shim ever
built*, including ones that predate the feature. That is what makes it reach an
installation already in the field — a stale shim shows the facts after a broker restart
and nothing else, without first performing the upgrade the notice recommends.

To upgrade both:

```bash
cargo install --path crates/broker && cargo install --path crates/mcp
```

then restart the broker service and your MCP client. Reinstalling the binary does not
affect a running process — it keeps the image it started with.

## SDK and cross-language clients

### Rust: `logmon-broker-sdk`

The typed Rust SDK at `crates/sdk` is the canonical client for non-MCP consumers (test harnesses, archival workers, dashboards). It:

- Returns `Result<R, BrokerError>` for every method.
- Auto-discovers the socket at `~/.config/logmon/logmon.sock`.
- Resumes named sessions across daemon restarts with jittered exponential backoff.
- Emits a typed `Notification` enum (`TriggerFired`, `Reconnected`) on a broadcast channel.
- Builds filter strings via a typed `Filter` builder (no manual quoting).

```rust
use logmon_broker_sdk::{Broker, Filter, Level};
use logmon_broker_protocol::LogsRecent;

let broker = Broker::connect()
    .session_name("my-tool")
    .open().await?;

let result = broker.logs_recent(LogsRecent {
    count: Some(50),
    filter: Some(Filter::builder().level_at_least(Level::Error).build()),
    ..Default::default()
}).await?;
```

See [`crates/sdk/README.md`](crates/sdk/README.md) for the full guide.

### Other languages

The wire protocol is JSON-RPC 2.0 over a Unix domain socket (newline-delimited frames, no length prefix), formally defined by [`crates/protocol/protocol-v1.schema.json`](crates/protocol/protocol-v1.schema.json) (JSON Schema 2020-12). The schema is drift-guarded — `cargo xtask verify-schema` fails CI when the committed schema disagrees with the Rust struct definitions. Treat it as the authoritative contract for cross-language codegen.

## Development

```bash
# Build the workspace
cargo build --workspace

# Run the full test suite
cargo test --workspace

# Lint and format checks (CI runs these)
cargo fmt --all --check
cargo clippy --workspace --all-targets

# Regenerate / verify the wire-protocol JSON Schema
cargo xtask verify-schema

# Quick smoke: send a test GELF message to a running broker
./test-gelf.sh           # TCP
./test-gelf.sh 12201 udp # UDP
```

The default workspace members (`crates/broker`, `crates/mcp`) are what `cargo build` and `cargo run` target without `-p`.

## Roadmap

- Hot reload of `config.json` without a restart.
- Span trigger evaluation (currently triggers only watch logs).
- Persistent buffer rotation on disk for crash-survival debugging.
- First-class Windows support (today's TCP fallback works but isn't first-class).
- Additional language SDKs codegen'd from `protocol-v1.schema.json`.

## Contributing

Issues, PRs, and design discussions are welcome. A few ground rules:

- Run `cargo fmt --all`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` before opening a PR.
- If your change touches `crates/protocol/src/methods.rs` or `notifications.rs`, regenerate the schema with `cargo xtask verify-schema` and commit the result.
- Keep new features additive on the wire — the protocol uses additive-field discipline.

## License

MIT. See [LICENSE](LICENSE).
