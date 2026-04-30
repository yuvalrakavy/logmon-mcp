# logmon-mcp

A log monitoring MCP server that collects structured logs (GELF format) from applications and exposes them to AI coding assistants via the [Model Context Protocol](https://modelcontextprotocol.io/). Multiple sessions share a single log collector daemon.

Works with any MCP-compatible client: Claude Code, Cursor, Windsurf, VS Code (Copilot), Gemini CLI, OpenAI Codex CLI, and more.

## Architecture

logmon is a **broker** for structured logs and OTLP traces. A long-lived daemon ingests via GELF (UDP+TCP) and OTLP (gRPC+HTTP), and serves multiple clients over a Unix domain socket using JSON-RPC 2.0.

```
Application(s)        AI Sessions / Test Harnesses / Other Clients
     |                        |          |          |
  GELF UDP/TCP           logmon-mcp   logmon-mcp   SDK consumer
  OTLP gRPC/HTTP             |          |          |
     |                       v          v          v
     v                ┌───────────────────────────────┐
                      │       logmon-broker           │
                      │   (long-lived daemon, UDS)    │
                      │  Receivers → Pipeline → Store │
                      │  Per-session triggers/filters │
                      └───────────────────────────────┘
```

**Binaries:**

- `logmon-broker` — the daemon. Run as a system service (launchd / systemd) for always-on availability.
- `logmon-mcp` — MCP shim. Exposes broker tools to Claude Code / Cursor / etc. Auto-starts the broker if no service is installed.

**Other clients:** anything depending on the public `logmon-broker-sdk` Rust crate, or speaking JSON-RPC against the documented protocol (`crates/protocol/protocol-v1.schema.json`).

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (for building from source)
- An application that sends logs in [GELF format](https://go2docs.graylog.org/current/getting_in_log_data/gelf.html) over UDP or TCP

## Installation

### Build from source

```bash
git clone https://github.com/yuvalrakavy/logmon-mcp.git
cd logmon-mcp
cargo build --release
```

The binaries are at `target/release/logmon-broker` and `target/release/logmon-mcp`. You can copy them to a directory on your PATH:

```bash
cp target/release/logmon-broker target/release/logmon-mcp ~/.local/bin/
```

Or install via cargo:

```bash
cargo install --path crates/broker --path crates/mcp
```

### Run the broker as a system service (recommended)

```bash
logmon-broker install-service --scope user
```

Boots the broker via launchd (macOS) or systemd (Linux). Starts at login, restarts on crash. To uninstall: `logmon-broker uninstall-service --scope user`.

### Configure your AI coding assistant

If a system service is installed, the broker is already running. Otherwise the MCP shim auto-starts it on the first client connection. No manual setup needed.

#### Claude Code

```bash
# Register globally (available in all projects)
claude mcp add logmon --scope user -- logmon-mcp

# Or with a named session (persists triggers/filters across reconnects)
claude mcp add logmon --scope user -- logmon-mcp --session my-session

# Or register for current project only
claude mcp add logmon -- logmon-mcp
```

If the binary is not on your PATH, use the full path:

```bash
claude mcp add logmon --scope user -- /path/to/logmon-mcp
```

#### Cursor

Add to your Cursor MCP settings (`.cursor/mcp.json` in your project or `~/.cursor/mcp.json` globally):

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

For a named session:

```json
{
  "mcpServers": {
    "logmon": {
      "command": "logmon-mcp",
      "args": ["--session", "my-session"]
    }
  }
}
```

#### Windsurf

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

#### VS Code (GitHub Copilot)

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

Or add to `.vscode/mcp.json` in your project:

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

#### Gemini CLI

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

#### OpenAI Codex CLI

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

#### Any MCP-compatible client

logmon-mcp uses the standard MCP stdio transport. Configure your client to run `logmon-mcp` as a stdio MCP server. Optional argument: `--session <name>` for persistent sessions.

## Usage

### Configure your application to send GELF logs

Point your application's GELF output to `localhost:12201` (UDP or TCP). Most logging frameworks support GELF:

- **Rust**: `tracing-gelf`
- **Python**: `pygelf`
- **Node.js**: `gelf-pro`
- **Go**: `go-gelf`
- **Java**: Logback `biz.paluch.logging:logstash-gelf`
- **Docker**: `--log-driver=gelf --log-opt gelf-address=udp://localhost:12201`

### Use from your AI assistant

Once logs are flowing, ask your assistant:

- "check the logs"
- "show me recent errors"
- "what happened around the cache error?"
- "add a filter for the mqtt module"
- "set up a trigger for panics"

### MCP Tools

| Tool | Description |
| ---- | ----------- |
| `get_recent_logs` | Fetch recent logs, optionally filtered |
| `get_log_context` | Get logs surrounding a specific entry by seq number |
| `export_logs` | Save logs to a file |
| `clear_logs` | Clear the log buffer |
| `add_bookmark` | Set a named timestamp anchor at the current moment (global, qualified by session name) |
| `list_bookmarks` | List all live bookmarks, newest first |
| `remove_bookmark` | Remove a bookmark (bare name = current session; `session/name` reaches another session) |
| `get_status` | Server status and statistics |
| `get_filters` | List buffer filters for this session |
| `add_filter` | Add a buffer filter (OR semantics across filters) |
| `edit_filter` | Modify a filter |
| `remove_filter` | Remove a filter |
| `get_triggers` | List triggers for this session |
| `add_trigger` | Add a trigger with pre/post capture windows |
| `edit_trigger` | Modify a trigger |
| `remove_trigger` | Remove a trigger |
| `get_sessions` | List all connected sessions |
| `drop_session` | Remove a named session |

## Filter DSL

Filters use comma-separated qualifiers (AND semantics):

```
l>=ERROR                     # all errors and above
fa=mqtt,l>=WARN              # warnings+ from MQTT module
connection refused,h=myapp   # substring match + host filter
/panic|unwrap failed/        # regex match
```

**Selectors:** `m` (message), `fm` (full_message), `mfm` (message or full_message), `h` (host), `fa` (facility), `fi` (file), `ln` (line), `l` (level)

**Special filters:** `ALL` (match everything), `NONE` (match nothing)

### Bookmarks

Bookmarks let you scope queries to a time range without destructively clearing logs. Set one before an operation, another after, and query the range:

```
add_bookmark("before")
# run the operation
add_bookmark("after")
get_recent_logs(filter="b>=before, b<=after, l>=warn")
get_recent_traces(filter="b>=before, b<=after, d>=100")
```

Bookmarks are global across sessions and qualified by the creating session (`session/name`). Bare names in tool calls and DSL expressions resolve to the current session. Bookmarks auto-evict when both the log and span buffers have rolled past their timestamp — they cannot outlive the data they point at.

`b>=` / `b<=` are usable only in query tools, not in registered filters or triggers.

## Triggers

Triggers watch every incoming log and fire when a match occurs, capturing context:

- **pre_window**: Number of logs captured *before* the trigger event (flight recorder pattern)
- **post_window**: Number of logs captured *after* the trigger event
- **notify_context**: Number of context entries included in the notification

Default triggers are created for each session: `l>=ERROR` and `mfm=panic`.

When a trigger fires, the client receives a notification with the matched entry and surrounding context.

## Multi-Session

- All sessions share the same log buffer and GELF receivers
- Each session has its own triggers and filters
- **Anonymous sessions** (default): cleaned up on disconnect
- **Named sessions** (`--session name`): persist across disconnects, queue notifications while disconnected

## Configuration

Config files are stored in `~/.config/logmon/`:

- `config.json` — daemon settings (ports, buffer size, idle timeout)
- `state.json` — persisted state (seq counter, named sessions)

### CLI

```bash
# MCP shim — used by MCP clients
logmon-mcp [--session <name>]

# Broker daemon — usually run as a service, but you can run it directly
logmon-broker [--gelf-port 12201] [--buffer-size 10000]

# Broker subcommands
logmon-broker status                    # query daemon status
logmon-broker install-service [--scope user|system]
logmon-broker uninstall-service [--scope user|system]
```

### Environment variable overrides

- `LOGMON_BROKER_BIN` — explicit path to `logmon-broker` (defeats PATH lookup).
- `LOGMON_BROKER_SOCKET` — explicit broker socket path (used by tests; defaults to `~/.config/logmon/logmon.sock`).

## Testing

```bash
# Run tests
cargo test

# Send test GELF messages
./test-gelf.sh              # TCP (default)
./test-gelf.sh 12201 udp    # UDP
```

## Embedding via SDK

For non-MCP consumers (test harnesses, archival workers, dashboards), use the typed Rust SDK at `crates/sdk` (`logmon-broker-sdk`). It speaks JSON-RPC 2.0 against the broker's UDS, returns typed `Result<R, BrokerError>` for every method, supports named-session reconnection across daemon restarts, and emits typed `Notification` events (TriggerFired, Reconnected) on a broadcast channel.

The first SDK consumer outside this repo is `store-test` — see [`docs/superpowers/specs/2026-04-30-store-test-integration-handoff.md`](docs/superpowers/specs/2026-04-30-store-test-integration-handoff.md) for the integration brief.

## Manual smoke tests

These are not run in CI; verify locally before tagging a release. Each
section is independent — run only what's relevant to your platform.

### Build + binaries

```bash
cargo build --workspace
ls -lh target/debug/logmon-broker target/debug/logmon-mcp
```

Expected: both binaries exist; no warnings.

### Auto-start path (shim spawns broker)

```bash
target/debug/logmon-mcp --session smoke <<'EOF'
EOF
```

Expected: shim exits cleanly; `daemon.pid` and `logmon.sock` created in
`~/.config/logmon/`.

### Status subcommand

```bash
target/debug/logmon-broker status
```

Expected:
- with no daemon running: prints `not running` and exits 1.
- with a daemon running: prints `running pid=<N> socket=<path>` and exits 0.

### Stale-pid recovery

```bash
target/debug/logmon-broker &
PID1=$!
sleep 1
kill -9 $PID1                  # leave stale pid + socket
target/debug/logmon-broker &
PID2=$!
sleep 1
kill -TERM $PID2
wait
```

Expected: second start succeeds; `daemon.log` shows
"removing stale pid file from previous run"; clean exit on SIGTERM.

### Graceful shutdown via SIGTERM and SIGINT

```bash
target/debug/logmon-broker &
sleep 1
kill -TERM $!                  # try -INT too in a separate run
wait
```

Expected: broker exits 0; `logmon.sock` and `daemon.pid` removed;
`daemon.log` records "received SIGTERM" (or "SIGINT").

### System service install (macOS)

```bash
target/debug/logmon-broker install-service --scope user
launchctl print gui/$(id -u)/logmon.broker | head -5
target/debug/logmon-broker status
target/debug/logmon-broker uninstall-service --scope user
```

Expected: install succeeds → launchctl shows the agent → status
reports running → uninstall removes the plist.

### System service install (Linux)

```bash
target/debug/logmon-broker install-service --scope user
systemctl --user status logmon-broker
target/debug/logmon-broker status
target/debug/logmon-broker uninstall-service --scope user
```

Expected: install succeeds → `systemctl status` shows
"active (running)" within 2 s of start (Type=notify) → status reports
running → uninstall removes the unit.

## License

MIT
