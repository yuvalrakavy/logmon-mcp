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
- **Measures instead of guessing.** [Span time collectors](#time-profiling-with-collectors) turn "did that get faster?" into a computation: arm a filter, run the workload, read exact totals, percentiles and self time. Snapshot a run, change the code, snapshot again, and `diff_collectors` subtracts them — reporting the run-to-run spread so a real difference can be told from noise, and *refusing* the comparison rather than guessing when the two arms aren't comparable.
- **Says what it could not record.** Storage is conditional — a filter held by any session narrows what is kept — so every range read carries a verdict telling absence of *cause* from absence of *recording*. [`create_case`](#provenance-and-case-documents) freezes a window to disk with that verdict at the top of the document, before anything it qualifies, alongside a per-domain provenance registry saying which commit, in which build, doing what.
- **Same surface from MCP, CLI, and Rust.** The `logmon-mcp` binary doubles as a shell-friendly CLI (`logmon-mcp logs recent --json`), and both surfaces are [built from the broker's own manifest at startup](#how-the-clients-are-built) — a broker that gains a tool gains an MCP tool and a CLI command with no client reinstall. The `logmon-broker-sdk` crate gives Rust consumers a typed client. Other languages can codegen from `crates/protocol/protocol-v1.schema.json`.

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
            │            logmon-broker / logmon-broker-core       │
            │       long-lived daemon, JSON-RPC over UDS          │
            │   receivers → pipeline → ring buffers (logs+spans)  │
            │   per-session triggers / filters / bookmarks        │
            │   collectors · case archive · domain data · render  │
            │   tools.manifest — the surface it teaches clients   │
            └─────────────────────────────────────────────────────┘
```

Five crates ship as one project, plus an `xtask` helper that isn't published:

| Crate | What it is |
|---|---|
| `logmon-broker-core` (`crates/core`) | The engine. Receivers, the ingest pipeline, the log and span stores, the filter engine, triggers, sessions and domains, collectors, the case archive, the `domain_data` registry, and the daemon-side renderers. Every behaviour lives here; the two binaries are thin. |
| `logmon-broker` (`crates/broker`) | The daemon binary. Argument parsing, the service installer (launchd / systemd), and a `main` that hands off to `logmon-broker-core`. |
| `logmon-mcp` (`crates/mcp`) | Dual-mode binary. Bare, it's a stdio MCP server; with a command, a CLI. Neither surface is hand-written — both are assembled at startup from the broker's `tools.manifest` (see below). |
| `logmon-broker-sdk` (`crates/sdk`) | Typed Rust client. Talks JSON-RPC against the broker, exposes a typed notification stream, includes a filter-DSL builder and a reconnect state machine. |
| `logmon-broker-protocol` (`crates/protocol`) | The wire types, and `mcp_tools.rs` — the one place a tool is declared. Drift-guarded JSON Schema at `crates/protocol/protocol-v1.schema.json` for cross-language clients. |

### How the clients are built

**The daemon teaches its clients what it can do.** `logmon-mcp` holds no tool
names, no parameter structs and no command definitions. On startup it calls
`tools.manifest`, and from the reply it assembles both surfaces: the MCP router,
and the CLI's commands, flags, types and accepted values. A broker that gains a
tool gains an MCP tool *and* a CLI command with no rebuild of the client.

Two consequences worth knowing before you install anything:

- **Upgrade the broker before the shim.** The shim requires `tools.manifest` and
  refuses to start without it, so a broker older than the shim leaves you with no
  tools at all rather than a subset. See [Upgrades](#upgrades-and-version-skew).
- **CLI command paths are derived from RPC method names**, never declared —
  `collectors.list` is `logmon-mcp collectors list`, `domain_data.update` is
  `logmon-mcp domain-data update`. A derived path has no second place to disagree
  with itself, which is why `--help` is authoritative and any table of commands
  (including the ones in this file) can lag.

This replaces an earlier arrangement where the tool list was compiled into the
shim. That version could not gain a capability without being reinstalled, and
nothing told anyone when the reinstall was overdue — a project once filed a
report proposing three collector features that had already shipped.

**Domains.** The broker can host multiple isolated **domains** — each a full instance with its own receivers (ports), ring buffers, and per-session triggers/filters, so unrelated log streams never interleave. The `default` domain is the always-on anchor; declare durable ones in `config.json` (see [Configuration](#configuration)) or create ephemeral ones at runtime. A session targets one via `use_domain` (MCP) or the `--domain` flag (CLI). For a per-worktree / per-project setup, set **`LOGMON_DOMAIN`** in the MCP server's env once — **alongside a named `--session`** — so the shim binds that domain at connect and **re-binds it on every reconnect** (durable across daemon restarts). Reconnect-preservation needs a named session: an anonymous session can't resume a restart, so it fails *loud* (never a silent revert to `default`) and the shim is restarted. Every session then auto-scopes with zero per-call ceremony. Create the domain before the shim connects; a missing domain is a loud handshake error, not a silent fallback.

## Installation

### Build

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp.git
cd logmon-mcp
cargo install --path crates/broker --locked   # the daemon, FIRST
cargo install --path crates/mcp --locked      # the MCP server / CLI
```

This puts `logmon-broker` and `logmon-mcp` on your PATH (`~/.cargo/bin` by default).

**The order matters.** `logmon-mcp` builds its whole surface from the broker's
`tools.manifest` and refuses to start without one, so a shim newer than the running
broker has no tools rather than a stale subset. Install the broker, restart it, then
install the shim.

**`--locked` is not optional.** `cargo install` ignores `Cargo.lock` without it and
re-resolves every dependency to the newest semver-compatible release — which is how
a build that passes the test suite fails at install time with a macro error, on the
same commit. The shim's MCP layer is the one that bites: it pins a version whose
derive macros changed shape in a later minor.

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

Prerequisite: the `logmon-mcp` binary must already be on your `PATH` (`cargo install --path crates/broker --locked` then `cargo install --path crates/mcp --locked`, or once published, `cargo install logmon-mcp --locked`). The plugin manifest references the binary; it doesn't bundle it.

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

47 tools, grouped by what they're for. The broker declares them in
`crates/protocol/src/mcp_tools.rs` and serves them over `tools.manifest`, so this
table is a convenience — the tool list your client registered is the truth, and
`get_status` reports the broker's own list as `broker_tools`.

### Logs

| Tool | Description |
|---|---|
| `get_recent_logs` | Fetch recent logs, optionally filtered or scoped to a `trace_id`. |
| `get_log_context` | Get logs surrounding a specific entry by `seq`. |
| `export_logs` | Save logs to a file (json or text). `from_seq`/`to_seq` bound an **inclusive** window and compose with a bookmark bound. Every reply carries a **`verdict`** — `complete` / `filtered` / `evicted` / `cannot_verify` — saying how much of that window the daemon can vouch for, with `narrowed_by` naming any session filter that was narrowing what got stored, and over which seqs. See [Evidence verdicts](#evidence-verdicts). |
| `clear_logs` | Clear the shared log buffer. |

### Traces and spans

| Tool | Description |
|---|---|
| `get_recent_traces` | List recent traces with timing and error info. |
| `get_trace` | Full span tree for a trace; `include_logs` (default `true`) interleaves linked logs. |
| `get_trace_summary` | Compact timing breakdown highlighting bottlenecks. |
| `get_slow_spans` | Find slow spans (default `min_duration_ms=100`, `count=20`). With `group_by="name"` the aggregates cover **every** stored span of that name, and `min_duration_ms` becomes a display floor deciding which names appear — so a name can qualify on its `max_ms` while its `avg_ms` sits far below the floor. |
| `get_span_context` | Spans surrounding a given span by `seq`. |
| `get_trace_logs` | All logs linked to a trace. |
| `export_spans` | The same inclusive seq range as `export_logs`, over the span ring, for pairing spans with the logs of one window. Reports its **own** retention: the two stores share a seq axis but evict independently, so a complete log window says nothing about whether its spans survived. |

### Collectors — measuring time

Full guide: [Time profiling with collectors](#time-profiling-with-collectors).

| Tool | Description |
|---|---|
| `add_collector` / `list_collectors` / `get_collector` / `remove_collector` | Arm a filter, run the workload, read exact totals, percentiles and self time. At small n the `sampled` block also carries **`durations_ms`** (every retained duration, in arrival order, when complete and ≤50) and **`stddev_ms`** — at three runs the percentiles are order statistics of three numbers, so the durations are what make "are these two arms actually separated?" a computation rather than a judgement. `skip_warmup_ms` reports its own effect as **`excluded_by_warmup`**, and a grouped read reports **`groups_total`** before `top_n` truncation. Needs a **named** session — an anonymous one's identity is a UUID that never returns, so anything it armed would be unreachable after a disconnect. `matched: 0` comes with `zeroed_by` (`snapshot` / `reset` / `edit` / `daemon_restart`, or absent for "no traffic yet"), so an empty window is never ambiguous. |
| `snapshot_collector` / `get_collector_history` | Record a window as a named run and start the next — the between-runs move for a before/after comparison. History carries each run's own definition, and `merge` reports the run-to-run spread so you can tell a real difference from noise. Survives a daemon restart; a run that could not be written reports `durable: false` rather than pretending otherwise. |
| `edit_collector` | Change an armed collector. Description is free; anything structural discards the live window (never the history). Re-pins a collector orphaned by a restart. |
| `diff_collectors` | Subtract two runs and report what moved. Arms are `<collector>`, `<collector>@<label>`, or `<collector>@*` (every recorded run merged — the only shape with a run-to-run floor, so the only one whose deltas can be told from noise). Every row carries **the threshold that was applied**, and estimated percentile rows carry the error on the delta: `α(a+b)/|a−b|`, which reaches ±199% for a 1% change. **Refuses rather than guessing** when the arms are not comparable, and names the flag that would permit it. |
| `document_collectors` | Write the measurement up for a reader who wasn't there: what moved, what to do next, and every caveat beside the number it qualifies. `md` (default), `json`, or `folded` for a flame graph. Returns bytes plus a sidecar — the client writes them. Regeneration is free, so `finding` normally arrives on a second call. |
| `add_collector(threshold=…)` | A rolling guard over `count` / `total_ms` / `avg_ms` / `error_count` / `error_rate_pct`. Evaluated on **span arrival, not a clock**, so an idle collector costs nothing — and so with no traffic a breached threshold neither fires nor clears. A load-time guard, not a liveness check; every report says so. |
| `reset_collector` | Zero a collector and **discard** the run. Prefer `snapshot_collector`. |
| `profile_traces` | The same numbers over spans already in the buffer, without arming anything. |

### Cases and provenance

Full guide: [Provenance and case documents](#provenance-and-case-documents).

| Tool | Description |
|---|---|
| `create_case` | Capture a window as three files on disk — a markdown document you read to decide whether this is your bug, and two JSONL evidence files you consult once you have decided it is. `reason` and `dir` are required and `dir` must be **absolute**; the anchor is tagged (`{seq}` / `{bookmark}` / `{trace_id}`) rather than sniffed, and an unresolvable one is an error rather than a document with no headline. `before`/`after` count stored **records**, not seq distances (default 350 each, capped at 5000). `data` is `update_domain_data` in the same call; a key with a leading `@` is asserted about **this capture alone** and never enters the domain registry. |
| `update_domain_data` | A per-domain key/value registry recording what was true of the project while the logs were produced — the commit, the build profile, the scenario. Entries are `{path, value?, ttl?}`: a value **sets**, a key alone **validates** what is already there and never creates. Two timestamps per key, never one: set six days ago and never revisited is a guess, the same value confirmed five minutes ago is evidence. |
| `get_domain_data` | Read it back — each key with its value, when it came into force, when it was last confirmed, and its age. A key with a stated `ttl` also gets an expiry verdict; a key without one gets an age and **no verdict**, deliberately. Also reports which recommended core keys are missing. |
| `remove_domain_data` | Remove keys by prefix, matched on segment boundaries (`/Versions` removes `/Versions/*`; `/Ver` removes nothing). **No undo** — and re-setting a removed key resets when its value came into force, turning a months-old confirmed fact into a fresh-looking one. |

### Filters, triggers, bookmarks

| Tool | Description |
|---|---|
| `get_filters` / `add_filter` / `edit_filter` / `remove_filter` | Per-session buffer filters. |
| `get_triggers` / `add_trigger` / `edit_trigger` / `remove_trigger` | Per-session triggers. |
| `add_bookmark` / `list_bookmarks` / `remove_bookmark` / `clear_bookmarks` | Bookmarks (also act as cursors via `c>=`). |

### Sessions, domains, status

| Tool | Description |
|---|---|
| `get_sessions` / `drop_session` | Multi-session inspection. |
| `rename_session` | Rename this session in place — all state (domain binding, triggers, filters, bookmarks, collectors) survives. A name held by a *connected* session errors (deliberate: two live clients must not share an identity); a *disconnected* holder is displaced (reported via `displaced_stale_holder`). |
| `get_status` | Daemon uptime, receivers, store stats, per-source drop counts, **`trace_ingest`** (trace-transport loss before any collector saw it — see [Backpressure](#backpressure); its `dropped` is a repeat of two `receiver_drops` fields, so don't sum them), current domain + active filters, and per-listener `receiver_liveness`. Also **`broker_version`** and **`broker_tools`** — the tools this broker serves, which is what your client registered from. See [Upgrades](#upgrades-and-version-skew). |
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

## Provenance and case documents

Everything in the buffer is in memory and rolling. A restart, a `clear_logs` from
another session, or simply enough traffic, and the evidence is gone with no error
anywhere. The elusive bugs are exactly the ones you cannot reproduce on demand,
so the moment something looks worth understanding later, freeze it.

Two mechanisms, and the order matters: **provenance first, capture second.** Logs
without provenance are a dump; logs with it are evidence.

### `domain_data` — what was true while these logs were produced

A flat key/value registry per domain. Keys are path-shaped (`/Build/commit`),
values are UTF-8, and every key carries **two** timestamps: when the value came
into force, and when it was last confirmed. That distinction is the whole point —
a value set six days ago and never revisited is a guess; the same value confirmed
five minutes ago is evidence.

```
update_domain_data(entries: [
  {path: "/Build/commit",  value: "<git rev-parse HEAD>"},
  {path: "/Build/profile", value: "release"},
  {path: "/Action",        value: "checkout smoke, 20 iterations", ttl: "30m"},
  {path: "/Env/host"},                        # key-only: confirm, do not restate
])
```

**A value present sets it; a value absent validates it.** A key-only entry moves
only the confirmation time and never creates — recording a key with no value would
be a guess. So anything you did not actually re-derive should go key-only: a `ttl`
runs from the last confirmation, and restating a value you merely assumed buys it a
freshness it has not earned.

| Key | Why |
|---|---|
| `/Build/commit` | The only exact identity of the code. A version string is a label someone maintains; a SHA is what ran. |
| `/Build/profile` | `debug` or `release`. logmon is a timing instrument and the two differ by an order of magnitude. |
| `/Action` | What you were doing, in prose. Without it a reader has logs and no scenario — and a *stale* `/Action` is worse than an absent one, because it reads as fact. |
| `/Versions/<component>` | Which release of *which part* — plural, because the failing one is rarely the one you upgraded. |
| `/Env/host`, `/Env/os`, `/Env/container` | "Only on CI?" — the first question about anything intermittent. |
| `/Data/seed`, `/Data/dataset` | The seed is what turns "fails 1 in 20" into a reproduction, and it's the one most often skipped. |
| `/case-name` | Filename prefix for this project's case documents — a slug, not prose. |

`ttl` (`30s` / `5m` / `2h` / `7d` / `4w`, or `false` to clear one) states how long a
value stays believable. **Leave it off and the reader gets an age and no verdict**,
which is deliberate: without a stated lifetime, logmon will not tell anyone a value
is current. `/logmon/*` is the daemon's own namespace and is rejected on write.

Each entry comes back with an outcome — `created`, `updated`, `validated`,
`rejected` (with a reason), or `unknown` (a key-only entry that found nothing,
with `never_set` or `undetermined` saying which). Create a domain per project:
anything that doesn't is sharing one registry with every other project on the box.

### `create_case` — freezing a window to disk

```
create_case(
  reason:  "checkout hangs at 20/20, reproducibly",   # required
  anchor:  {seq: 41022},                              # or {bookmark: …} / {trace_id: …}
  dir:     "/abs/path/to/docs/cases",                 # required, ABSOLUTE
  prefix:  "checkout-hang",                           # optional
  before:  350, after: 350,                           # stored RECORDS either side
  data:    [{path: "/Env/host",   value: "ci-7"},     # → the domain registry
            {path: "@/Data/seed", value: "8814"}],    # → this document only
)
```

Three files sharing one stem, written by the daemon:

```
checkout-hang-260731-021530.md               ← read this to triage
checkout-hang-260731-021530.logdata.jsonl    ← the log records
checkout-hang-260731-021530.spandata.jsonl   ← the spans
```

**The split is the design.** The document is what you read to decide whether this
is your bug; the logdata is the evidence you consult once you have decided it is.
The daemon writes rather than returning the archive over RPC, because a case runs
to hundreds of kilobytes of JSONL and returning it would put the whole thing in the
model's context — the outcome the split exists to prevent.

The document **leads with what could not be captured**: the [`verdict`](#evidence-verdicts)
for the window, whether the ring had already evicted part of it, whether the spans
survived alongside the logs, and which core provenance keys are missing — all
before anything those facts qualify. The verdict is on the wire too, so the
document's most important correctness property isn't reachable only by parsing
markdown.

Filenames are `<prefix>-<yymmdd>-<hhmmss>[-<id>]` in **UTC**, fixed-width so
lexicographic order is chronological order and `ls` answers "what happened around
then" over an archive nobody has indexed. The prefix comes from the `prefix`
parameter, else `/case-name` in the registry, else the domain name — and a
`/case-name` sent in the *same* call names the **next** capture, since the filename
is claimed before anything durable happens. Nothing is compressed and nothing
indexes the directory: the format is the contract, and whatever walks the archive
owns the querying.

**Four things that bite:**

- **`dir` must be absolute.** The broker runs as a service, so a relative path
  would resolve against *its* working directory. It's rejected, not resolved.
- **The anchor is tagged, not sniffed.** Exactly one of `{seq}`, `{bookmark}`,
  `{trace_id}` — a bookmark named `12345` and a seq are indistinguishable as bare
  strings, and the anchor's message becomes the headline, so a wrong anchor is a
  wrong document. Unresolvable is an error, not an empty headline.
- **`before`/`after` count stored records, not seqs.** One counter feeds both the
  log and span stores, so a 200-*seq* range holds an unpredictable number of logs.
- **`@` scopes a key to this capture.** It reaches the document, keeps its sigil,
  and never enters the registry — otherwise the next case on this domain would
  silently inherit the last one's seed. Plain keys are facts about the *domain*;
  `@` keys are facts about *this incident*. They come back as a `scoped` outcome,
  which is one arm `update_domain_data` alone never emits.

**Capture before you investigate, not after.** The document records both instants
and renders the gap, so a fact recorded twenty minutes later is visibly a fact
about twenty minutes later. That's honest — and it's also weaker evidence than the
same fact recorded at the time.

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

## Time profiling with collectors

A **span time collector** answers "how long does this take, in aggregate" and
"did that change make it faster" — without wrapping anything in `Instant::now()`
or eyeballing durations across a handful of traces. Arm a filter, run the
workload, read the numbers.

```
add_collector(name="lookup", filter="sn=Lookup", description="cache lookup path")
# ... run the workload ...
get_collector(name="lookup")
```

**It cannot be applied retroactively.** A collector only sees spans that arrive
after it is armed, which is the one mistake worth avoiding: if you are about to
change code and want to know whether it got faster, arm *first*. For spans already
in the buffer, `profile_traces` gives the same projection with nothing armed — the
useful pattern is `profile_traces` to find *what* is slow, then arm a collector on
that filter to track it across changes.

Collectors belong to the **session**, so they need a named one (`--session NAME`,
or the MCP shim's `--session`): an anonymous session's identity is a UUID that
never comes back, and anything it armed would be unreachable after a disconnect.

### Reading the result

`exact`, `estimated` and `sampled` are three different populations, not three views
of one number:

| Block | What it covers |
|---|---|
| `exact` | Every matched span, for the collector's whole life. Trust `count`, `total_ms`, `avg_ms`. |
| `estimated` | Percentiles from a sketch over the same population, accurate to ±1%. |
| `sampled` | Exact over the records actually retained — which is everything only while `complete` is `true`. Self time, wall union and call paths live here. |

`level` picks how much is computed: `scalar` (counts and totals), `timing` (adds
percentiles, wall union, warm-up exclusion), or `tree` (adds self time, nesting and
call paths — the default).

Any field that could not be computed comes back `null` with an entry in
`suppressed` saying why, and usually what to change. `null` and `0` are different
claims: `self_ms: null` beside `nested_matches: 0` means the filter matched no
nested spans, not that no time was spent there.

**At small n, read `sampled.durations_ms` rather than the percentiles.** When a
collector matches once per run, three runs give three records and the percentiles
are order statistics of three numbers. `durations_ms` lists every retained duration
in **arrival order** (`[0]` is the first, not the smallest) whenever the sample is
complete and holds at most 50; `stddev_ms` gives the spread from two records up.
Treat `stddev_ms` as a description, not a significance test — separating two
three-run means properly takes roughly 2.3 standard deviations, not one.

**`matched: 0` is ambiguous on its own, so read `zeroed_by` beside it.** Absent
means nothing has emptied the window — no traffic yet. Otherwise it names what did:
`snapshot` (the run was kept), `reset` (discarded), `edit` (the definition changed
under it), or `daemon_restart`.

### A/B comparisons

Two shapes, and the choice is made *before* the run:

1. **One pass, `group_keys`.** Emit a span attribute naming the arm, run both
   interleaved, read `group_by="group"`. Immune to drift between runs, and it
   replaces hand-rolled per-case counters with no change to the code being measured
   beyond emitting the attribute.

   ```
   add_collector(name="cache-ab", filter="sn=Lookup", group_keys=["cache.enabled"])
   get_collector(name="cache-ab", group_by="group")
   ```

2. **Two passes, `snapshot_collector`.** Arm, run A, `snapshot_collector(label="before")`,
   change the code, run B, `snapshot_collector(label="after")`, then
   `diff_collectors(a="cache@*", b="cache@*")` or `get_collector_history(merge=true)`.
   Use it when the arm cannot be an attribute. Both runs are kept, each with the
   definition it was taken under.

`snapshot_collector` — not `reset_collector` — is the between-runs move: reset
zeroes the window and **discards** the run.

**Repeat before you conclude.** Two runs differing by 5% tell you nothing until you
know the run-to-run spread. Take three snapshots of the *same* configuration first
and read the floor from `get_collector_history(merge=true)`; treat differences below
it as noise. A single run reports the spread as *unknown*, which is the honest
answer rather than zero — and it's why `@*` arms (every recorded run merged) are the
only ones whose deltas can be told from noise.

`diff_collectors` spends most of its behaviour on the cases where it **refuses**:
mismatched sketch layouts, or a `@*` arm whose runs carry different definitions
(a structural edit keeps history, so a collector's history can legitimately span
configurations, and summing them would report the spread across configurations as
scheduling variance). A refusal names both runs and the flag that would permit the
comparison. Estimated percentile rows carry the error on the delta —
`α(a+b)/|a−b|`, which reaches ±199% for a 1% change.

`document_collectors` writes the whole thing up for a reader who wasn't there:
what moved, what to do next, and every caveat beside the number it qualifies.
`md` (default), `json`, or `folded` for a flame graph. Regeneration is free and
lossless, so nothing is stored — pass `question` on the first call and `finding`
on a second once you have read it.

### Thresholds

`add_collector(threshold={metric, op, value, window_ms, group?})` arms a rolling
guard over `count` / `total_ms` / `avg_ms` / `error_count` / `error_rate_pct`; read
the verdict back from `list_collectors` or `get_collector`.

**The window advances on span arrival, not on a clock.** That's what makes an idle
collector free, and it has one consequence: with no traffic a breached threshold
neither fires nor clears. It's a load-time guard, not a liveness check — a `lt`
threshold detects a drop *while traffic continues* and does nothing at all if
traffic stops. Every report says so, so a stuck `breached: true` on a finished run
isn't a bug. Percentiles can't be thresholds (a rolling sketch per bucket is
unbounded memory); guard on `avg_ms` and read the real percentiles from
`get_collector`.

### Budget and lifetime

The sample tier is a **daemon-wide reservation**, checked at arm time: 256 MB
total, 64 MB per default-sized collector, so about four fit across every session
and domain at once. `add_collector` refuses with the numbers rather than silently
degrading, and `remove_collector` hands the reservation back — do that when you're
done. `edit_collector` from `tree` down to `timing` buys roughly 2.5× the records
inside the same budget.

**Collectors and their history survive a daemon restart** — both are written
through to `~/.config/logmon/collectors/`. The live window does not, and a restored
collector says so with `zeroed_by: "daemon_restart"`. One armed on an *ephemeral*
domain comes back `orphaned`, since that domain isn't re-created; `edit_collector`
re-pins it. If `snapshot_collector` returns `durable: false`, the run is in the
reply and in memory but not on disk — copy the numbers out now.

Renaming a session keeps its collectors. If the rename displaces a disconnected
session holding the same name, that session's collectors are cleared rather than
inherited, so you never read another conversation's measurements.

## Multi-session

- All sessions share the same log and span buffers and the same GELF/OTLP receivers.
- Each session has its own triggers, filters, bookmarks, and collectors.
- **Anonymous sessions** (default) get a UUID and clean up on disconnect. They cannot hold collectors — a UUID that never returns would make anything armed unreachable after a disconnect.
- **Named sessions** (`--session NAME`) persist filters, triggers, bookmarks and collectors across disconnects and across daemon restarts. Notifications are queued while disconnected.
- A *disconnected* named session is disposed by a periodic sweep once it passes `session_ttl_secs` (default 24 h). Connected sessions never expire.

## CLI mode

The same `logmon-mcp` binary is also a shell-friendly CLI. Command paths are derived
from the broker's RPC method names — `collectors.list` is `collectors list`,
`domain_data.update` is `domain-data update` — and the arguments, their types and
their accepted values come from the same manifest:

```bash
logmon-mcp logs recent --json | jq '.logs[] | select(.level=="Error")'
logmon-mcp bookmarks add release-rc1
logmon-mcp collectors add --name lookup --filter "sn=Lookup"
logmon-mcp collectors snapshot --name lookup --label before
logmon-mcp domain-data get
logmon-mcp status
```

Global flags, which go **before** the command (after it, a flag belongs to the tool):

- `--session NAME` — connect to a named session. CLI mode defaults to `"cli"` so state persists across invocations.
- `--domain NAME` — scope this invocation to an existing domain (queries + `domains clear`); omitted → `default`. Does not persist across invocations. `domains create/delete/list` ignore it.
- `--json` — emit machine-readable JSON instead of the daemon's [rendered form](#rendered-output).

`--help` is authoritative and always current, because it is built from what the broker
just described:

```bash
logmon-mcp --help                  # every group
logmon-mcp collectors --help       # one group's verbs
logmon-mcp cases create --help     # one command's arguments and their accepted values
```

`crates/mcp/README.md` has a command overview, with the same caveat: it is written by
hand and the manifest is not, so a broker that gains a tool gains a command the table
has never heard of.

Useful when:

- You're in a subagent that doesn't inherit MCP servers.
- The MCP server disconnected mid-session.
- You want to pipe output through `head`, `jq`, or `grep`.

CLI calls are one-shot: no reconnect, fast-fail with a 5-second call timeout, no auto-start of the broker. Run the broker as a service first. **Triggers never fire in CLI mode** — the invocation exits before a matching log can arrive, so use the CLI to *manage* triggers and subscribe to fires over MCP or the SDK. **Collectors do work across invocations**, since CLI mode uses a persistent named session: `collectors add` → run your workload → `collectors get` is the intended shape.

## Configuration

Config and state live in `~/.config/logmon/` on both macOS and Linux:

| File | Contents |
|---|---|
| `config.json` | Daemon settings: ports, buffer sizes, idle timeout, declared domains. |
| `state.json` | Persisted state: seq counter, named sessions and their triggers/filters/bookmarks. |
| `collectors/` | Collector definitions and every recorded run, written through per mutation. The live window is not persisted. |
| `domain_data/` | The per-domain provenance registry, one file per domain, off the ingest path. |
| `logmon.sock` | The JSON-RPC Unix domain socket. |
| `daemon.pid` | PID file. |
| `daemon.log` | Broker log output. |

Case documents are **not** here — `create_case` writes them to the absolute `dir` you
name, because they belong beside the project they are evidence about.

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

## Upgrades and version skew

The broker and `logmon-mcp` are separate binaries with separate lifetimes: the broker runs
as a long-lived service, the shim is spawned per client session. **Skew in the tool surface
is gone** — the shim builds its MCP router and its CLI from `tools.manifest` at startup, so
whatever the broker serves is what the client offers, and upgrading the broker alone makes
new tools reachable.

That was not always true. The tool list used to be compiled into the shim, so a new
capability was unusable until the shim was reinstalled and nothing said when that was
overdue: a project once filed a report proposing three collector features that had all
shipped, because their shim was three minor versions behind.

**What replaced it is an ordering requirement.** The shim requires `tools.manifest` and
refuses to start without one, with an error naming the fix. So:

```bash
cargo install --path crates/broker --locked   # 1. broker
# 2. restart the broker service
cargo install --path crates/mcp --locked      # 3. shim
# 4. restart your MCP client
```

A shim newer than the broker has **no tools at all**, not a stale subset — loud, and
deliberately so. Reinstalling a binary never affects a running process; it keeps the image
it started with, so both restarts are load-bearing.

`status.get` still reports what the broker is:

```json
"broker_version": "0.10.0",
"broker_tools": ["add_bookmark", "add_collector", …]
```

`broker_tools` names *tools*, not RPC methods, because that is the vocabulary a client
holds (`traces.slow` is `get_slow_spans`). Since the client registered from that same
manifest the two now agree by construction, which is what makes the list useful for a
different question: whether the broker you are talking to is the one you just installed.
The old `shim_note` — a shim comparing its compiled-in list against the broker's — is gone,
because there is no compiled-in list left to compare.

The **wire** protocol is a separate matter, and it still uses additive-field discipline: an
older broker that omits a field deserializes it as that field's default, and `_display`
degrades to plain JSON in both directions.

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
# Everything CI checks, each step exactly once, with per-step timings
scripts/verify.sh

# Or the pieces:
cargo build --workspace
cargo test --workspace --all-features        # --all-features is load-bearing, see below
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo xtask verify-schema                    # regenerate with `cargo xtask gen-schema`

# Quick smoke: send a test GELF message to a running broker
./test-gelf.sh           # TCP
./test-gelf.sh 12201 udp # UDP
```

**`--all-features` is not optional on the test suite.** A dozen-plus integration files
(`collector_end_to_end`, `boot_resilience`, `domains_binding`, …) are gated behind
`#![cfg(feature = "test-support")]`. A plain `cargo test --workspace` compiles them
**empty** and reports the suite green — which has happened, and is why
`scripts/verify.sh` tallies zero-test suites as well as failures.

The default workspace members (`crates/broker`, `crates/mcp`) are what `cargo build` and
`cargo run` target without `-p`. Most behaviour lives in `crates/core`, so that is
usually where a change and its tests belong.

## Roadmap

- **Watches** — automatic `create_case` on a filter match, so an intermittent failure captures itself. Deliberately deferred from v1: deciding *when* a watch should fire, and how it avoids writing a thousand documents for one burst, is a design problem rather than an implementation one.
- Cross-checking provenance against a snapshot's `meta`, once the comparison can be made without firing on correct usage.
- Hot reload of `config.json` without a restart.
- Persistent buffer rotation on disk for crash-survival debugging.
- First-class Windows support (today's TCP fallback works but isn't first-class).
- Additional language SDKs codegen'd from `protocol-v1.schema.json`.

## Contributing

Issues, PRs, and design discussions are welcome. A few ground rules:

- Run `scripts/verify.sh` before opening a PR. It is `fmt` + `verify-schema` + `clippy` + the full `--all-features` suite, each exactly once.
- If your change touches `crates/protocol/src/methods.rs` or `notifications.rs`, regenerate the schema with `cargo xtask verify-schema` and commit the result.
- A new tool is declared **once**, in `crates/protocol/src/mcp_tools.rs` — name, method, description, and any CLI-only facts. The daemon serves it over `tools.manifest`, and the MCP router and CLI command both fall out of that. There is no client-side list to update.
- Keep new features additive on the wire — the protocol uses additive-field discipline.

## License

MIT. See [LICENSE](LICENSE).
