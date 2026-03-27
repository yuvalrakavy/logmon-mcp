# logmon-mcp

A log monitoring MCP server that collects structured logs (GELF format) from applications and exposes them to Claude Code via MCP tools. Multiple Claude sessions share a single log collector daemon.

## Architecture

```
Application(s)                Claude Code Sessions
     |                          |           |
  GELF UDP/TCP              Shim (MCP)   Shim (MCP)
     |                          |           |
     v                          v           v
  +------------------------------------------+
  |              logmon daemon               |
  |  GELF Receivers -> Log Processor -> Store|
  |  Per-session: triggers, filters, notifs  |
  +------------------------------------------+
```

- **Daemon**: Long-running process that collects logs, evaluates per-session triggers/filters, stores entries in a ring buffer, and serves RPC requests over a Unix socket.
- **Shim**: Thin MCP bridge that auto-starts the daemon and translates MCP tool calls to daemon RPC. One shim per Claude session, all connected to the same daemon.

## Installation

```bash
# Build
cargo build --release

# Register as MCP server in Claude Code (global)
claude mcp add logmon --scope user -- \
  /path/to/logmon-mcp/target/release/logmon-mcp-server

# With a named session (persists triggers/filters across reconnects)
claude mcp add logmon --scope user -- \
  /path/to/logmon-mcp/target/release/logmon-mcp-server \
  --session my-session
```

The daemon starts automatically when Claude Code connects. No manual setup needed.

## Usage

### Configure your application to send GELF logs

Point your application's GELF output to `localhost:12201` (UDP or TCP). Most logging frameworks support GELF:

- **Rust**: `tracing-gelf`
- **Python**: `pygelf`
- **Node.js**: `gelf-pro`
- **Go**: `go-gelf`
- **Docker**: `--log-driver=gelf --log-opt gelf-address=udp://localhost:12201`

### Use from Claude Code

Once logs are flowing, ask Claude:

- "check the logs"
- "show me recent errors"
- "what happened around the cache error?"
- "add a filter for the mqtt module"
- "set up a trigger for panics"

### MCP Tools

| Tool | Description |
|------|-------------|
| `get_recent_logs` | Fetch recent logs, optionally filtered |
| `get_log_context` | Get logs surrounding a specific entry by seq number |
| `export_logs` | Save logs to a file |
| `clear_logs` | Clear the log buffer |
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

## Triggers

Triggers watch every incoming log and fire when a match occurs, capturing context:

- **pre_window**: Number of logs captured *before* the trigger event (flight recorder pattern)
- **post_window**: Number of logs captured *after* the trigger event
- **notify_context**: Number of context entries included in the notification

Default triggers are created for each session: `l>=ERROR` and `mfm=panic`.

When a trigger fires, Claude receives a notification with the matched entry and surrounding context.

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
# Shim mode (default, used by Claude Code)
logmon-mcp-server [--session <name>]

# Daemon mode (usually auto-started)
logmon-mcp-server daemon [--gelf-port 12201] [--buffer-size 10000]
```

## Testing

```bash
# Run tests
cargo test

# Send test GELF messages
./test-gelf.sh              # TCP (default)
./test-gelf.sh 12201 udp    # UDP
```

## License

MIT
