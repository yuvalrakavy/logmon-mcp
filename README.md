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

## Installation

### Build

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp.git
cd logmon-mcp
cargo install --path crates/broker --path crates/mcp
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

Install the bundled Claude Code plugin from the `mcps` marketplace hosted in this repo. One marketplace add, one plugin install, and you're done — the MCP server registration and the skill come along automatically:

```
/plugin marketplace add yuvalrakavy/logmon-mcp
/plugin install logmon-mcp@mcps
```

The marketplace alias is `mcps` (plugin-agnostic, so additional plugins can join the same marketplace later). The repo it currently lives in is `yuvalrakavy/logmon-mcp`; if the marketplace later migrates to its own repo, the alias stays the same and you'll only re-run the `marketplace add` step.

Prerequisite: the `logmon-mcp` binary must already be on your `PATH` (`cargo install --path crates/broker --path crates/mcp` or, once published, `cargo install logmon-mcp`). The plugin manifest references the binary; it doesn't bundle it.

To update later: `/plugin marketplace update mcps`. To remove: `/plugin uninstall logmon-mcp@mcps`.

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

1. **Claude Code plugin (recommended for Claude Code).** If you installed via `/plugin install logmon-mcp@mcps`, the plugin registers the skill alongside the MCP server. Claude Code surfaces it on-demand via the skill's natural-language triggers and exposes the `/logmon-mcp:logmon` slash-namespaced form.
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
| `export_logs` | Save logs to a file (json or text). |
| `clear_logs` | Clear the shared log buffer. |
| `get_recent_traces` | List recent traces with timing and error info. |
| `get_trace` | Full span tree for a trace; `include_logs` (default `true`) interleaves linked logs. |
| `get_trace_summary` | Compact timing breakdown highlighting bottlenecks. |
| `get_slow_spans` | Find slow spans (default `min_duration_ms=100`, `count=20`); aggregate by name with `group_by="name"`. |
| `get_span_context` | Spans surrounding a given span by `seq`. |
| `get_trace_logs` | All logs linked to a trace. |
| `get_filters` / `add_filter` / `edit_filter` / `remove_filter` | Per-session buffer filters. |
| `get_triggers` / `add_trigger` / `edit_trigger` / `remove_trigger` | Per-session triggers. |
| `add_bookmark` / `list_bookmarks` / `remove_bookmark` / `clear_bookmarks` | Bookmarks (also act as cursors via `c>=`). |
| `get_sessions` / `drop_session` | Multi-session inspection. |
| `get_status` | Daemon uptime, receivers, store stats, per-source drop counts. |

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

## Triggers

Triggers watch every incoming log and fire when a match occurs, capturing context:

- `pre_window` — logs captured *before* the matching event (flight recorder).
- `post_window` — logs captured *after* the matching event.
- `notify_context` — how many pre-window entries are inlined into the notification payload.
- `oneshot` — when `true`, the trigger auto-removes after its first match.

Each session automatically gets two triggers on startup: `l>=ERROR` and `mfm=panic`. The pre- and post-trigger captures bypass buffer filters, so context around a fire is never truncated by a narrow filter.

When a trigger fires, the client receives a notification with the matched entry and surrounding context. The trigger fire count is visible in `get_triggers`.

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
  "idle_timeout_secs": 1800
}
```

Environment variable overrides:

- `LOGMON_BROKER_BIN` — explicit path to `logmon-broker` (skips PATH lookup).
- `LOGMON_BROKER_SOCKET` — explicit broker socket path. Defaults to `~/.config/logmon/logmon.sock`.

## Backpressure

A noisy producer should slow itself down, not take the broker down. Concretely:

- GELF receivers use `try_send` into the pipeline channel — full channel means the entry is dropped at the receiver, not enqueued without bound.
- GELF UDP sets `SO_RCVBUF` to **8 MB** so a slow consumer has a sizeable OS-side cushion before datagrams start falling on the floor.
- OTLP gRPC and OTLP HTTP both check channel fill before consuming a payload. At **≥ 80% full**, gRPC returns `UNAVAILABLE` and HTTP returns `429`. The producer is expected to retry with backoff. The protocol-level rejection *is* the backpressure signal — per-source drop counters aren't bumped, because nothing was silently dropped.
- Per-source drop counts surface in `status.get` under `receiver_drops` (`gelf_udp`, `gelf_tcp`, `otlp_http_logs`, `otlp_http_traces`, `otlp_grpc_logs`, `otlp_grpc_traces`). Healthy operation keeps all six at zero.

If you're seeing nonzero drops, the broker is the bottleneck — bump `buffer_size` / `span_buffer_size`, or check whether a runaway producer is genuinely outpacing the consumer.

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
