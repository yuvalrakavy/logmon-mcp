# Multi-Session Architecture — Design Spec

## Purpose

Refactor logmon-mcp from a single-process-per-session model to a daemon + shim architecture, allowing multiple Claude Code sessions to share a single log collector. This is essential because multiple concurrent Claude sessions is the common case during development.

## Problem

Currently each Claude Code session spawns its own `logmon-mcp-server` process. Only the first can bind GELF port 12201 — subsequent sessions fail. Logs go to one session only, and there's no shared state.

## Architecture

Single binary, two modes:

```
Claude A (stdio) ←→ Shim A ──┐
                               │
Claude B (stdio) ←→ Shim B ──┤── Unix Socket / TCP
                               │   (~/.config/logmon/)
Claude C (stdio) ←→ Shim C ──┼──→ Daemon
                                    │
                              ┌─────┴──────────────────────────┐
                              │  Log Receivers                  │
                              │    └─ GelfReceiver (port 12201) │
                              │  Log Buffer (shared)            │
                              │  Seq Counter (persistent)       │
                              │  Pre-trigger Buffer             │
                              │  Session Registry               │
                              │    ├─ "store-debug" (named)     │
                              │    │   filters, triggers, queue │
                              │    ├─ anonymous-uuid-1          │
                              │    │   filters, triggers        │
                              │    └─ ...                       │
                              └────────────────────────────────┘

  App A ──UDP/TCP──→ Daemon port 12201
  App B ──UDP/TCP──→ Daemon port 12201
```

### Daemon

Long-running process that owns all shared state:

- **Log receivers** (GELF for now, extensible via `LogReceiver` trait)
- **Log buffer** (InMemoryStore, shared across all sessions)
- **Sequence counter** (global, persistent across restarts)
- **Pre-trigger buffer** (global, sized to max pre_window across all sessions' triggers)
- **Session registry** (per-session filters, triggers, notification queues)

Started by the first shim (auto-start) or manually via `logmon-mcp-server daemon`.

### Shim

Thin bridge between Claude's MCP (stdio) and the daemon (socket RPC). No business logic. Responsibilities:

1. Auto-start daemon if not running
2. Connect to daemon, establish session (anonymous or named)
3. Translate MCP tool calls → daemon RPC calls
4. Forward daemon notifications → MCP notifications to Claude
5. On disconnect: daemon handles cleanup based on session type

### CLI

```
logmon-mcp-server [--session <name>]                    # shim mode (default)
logmon-mcp-server daemon [--gelf-port 12201] [--buffer-size 10000]  # daemon mode
```

## Session Model

### Two Session Types

| | Anonymous | Named |
|---|---|---|
| Created by | `logmon-mcp-server` (no `--session`) | `logmon-mcp-server --session <name>` |
| Session ID | UUID, assigned by daemon | User-provided name |
| Filters/triggers | Default triggers cloned, lost on disconnect | Default triggers cloned on first connect, persist across disconnects |
| Notification queue | None — dropped on disconnect | Capped queue (default 1000, oldest-dropped-first), drained on reconnect |
| Cleanup | Automatic on disconnect | Explicit via `drop_session` tool, or TTL (default 24h) |

### Session Lifecycle

1. Shim connects → sends `session.start` with optional name
2. Daemon looks up name:
   - Found + disconnected: reconnect, drain queued notifications
   - Found + connected: error (session already in use)
   - Not found + named: create new session with default triggers, mark as persistent
   - Not found + anonymous: create new session with default triggers, mark as ephemeral
3. Normal operation: shim relays MCP tool calls as RPC, daemon evaluates triggers and sends notifications
4. Shim disconnects:
   - Anonymous: remove session from registry
   - Named: mark disconnected, keep state, start queuing notifications
5. Named session TTL expires with no reconnect: auto-cleanup

### Session State

```rust
struct SessionState {
    name: Option<String>,                    // None = anonymous
    triggers: TriggerManager,                // per-session, initialized with defaults
    filters: Vec<BufferFilterEntry>,         // per-session
    notification_queue: VecDeque<PipelineEvent>,  // for disconnected named sessions
    max_queue_size: usize,                   // default 1000
    connected: bool,
    last_seen: Instant,
}
```

### Default Triggers

Each new session starts with cloned defaults:

| ID | Filter | Pre-window | Post-window | Notify-context | Description |
|----|--------|------------|-------------|----------------|-------------|
| 1 | `l>=ERROR` | 500 | 200 | 5 | Error-level log detected |
| 2 | `/panic\|unwrap failed\|stack backtrace/` | 500 | 200 | 5 | Panic or unwrap failure detected |

No global triggers — each session can customize independently.

## Log Processing Flow

For every incoming log message:

1. Assign global seq number (atomic increment)
2. Append to pre-trigger buffer (global)
3. For each connected session + named disconnected sessions:
   a. Evaluate session's triggers — if matched:
      - Flush pre_window entries from pre-trigger buffer into LogStore (skip duplicates)
      - If session connected: send notification to shim → Claude
      - If session disconnected (named): queue notification (cap at max_queue_size)
      - Activate post-trigger window for this session
   b. During active post-trigger window for a session: skip that session's trigger evaluation, decrement counter
4. Evaluate buffer storage: union all sessions' filters. If no session has filters, store everything (implicit ALL). Otherwise, store the log if ANY session's filter matches it. This keeps the store manageable while ensuring no session misses data it asked for.

When a session queries via `get_recent_logs`, its own filters are applied to the stored data as an additional read-time filter. This means a session may see logs that matched another session's filter — this is acceptable since query-time filtering narrows the results.

**`clear_logs`** clears the shared buffer for all sessions — it's a global operation. The tool response should warn that this affects all sessions.

## Daemon ↔ Shim Communication

### Transport

- **Unix systems (macOS, Linux):** Unix domain socket at `~/.config/logmon/logmon.sock`
- **Windows:** TCP on `127.0.0.1:<port>` (configurable, default 12200)
- Detection is compile-time: `#[cfg(unix)]` / `#[cfg(windows)]`
- RPC protocol is identical regardless of transport

### Protocol

JSON-RPC 2.0 over the socket. Each message is newline-delimited JSON.

**Shim → Daemon (requests):**

| RPC Method | MCP Tool | Parameters |
|---|---|---|
| `session.start` | (on connect) | `{ name?: string }` |
| `session.stop` | (on disconnect) | `{}` |
| `session.list` | `get_sessions` | `{}` |
| `session.drop` | `drop_session` | `{ name: string }` |
| `logs.recent` | `get_recent_logs` | `{ count, filter? }` |
| `logs.context` | `get_log_context` | `{ seq?, timestamp?, before, after, window_secs }` |
| `logs.export` | `export_logs` | `{ path, count?, filter?, format }` |
| `logs.clear` | `clear_logs` | `{}` |
| `status.get` | `get_status` | `{}` |
| `filters.list` | `get_filters` | `{}` |
| `filters.add` | `add_filter` | `{ filter, description? }` |
| `filters.edit` | `edit_filter` | `{ id, filter?, description? }` |
| `filters.remove` | `remove_filter` | `{ id }` |
| `triggers.list` | `get_triggers` | `{}` |
| `triggers.add` | `add_trigger` | `{ filter, pre_window?, post_window?, notify_context?, description? }` |
| `triggers.edit` | `edit_trigger` | `{ id, filter?, pre_window?, post_window?, notify_context?, description? }` |
| `triggers.remove` | `remove_trigger` | `{ id }` |

Session ID is implicit — the daemon associates each socket connection with a session.

**Daemon → Shim (notifications):**

Push notifications over the socket when a trigger fires:

```json
{
    "jsonrpc": "2.0",
    "method": "trigger.fired",
    "params": {
        "trigger_id": 2,
        "trigger_description": "Panic or unwrap failure detected",
        "filter": "/panic|unwrap failed|stack backtrace/",
        "matched_entry": { /* LogEntry */ },
        "context_before": [ /* notify_context entries */ ],
        "pre_trigger_flushed": 500,
        "post_window_size": 200,
        "buffer_size": 4523
    }
}
```

On reconnect of a named session, queued notifications are sent immediately.

## Daemon Lifecycle

### Auto-start

1. Shim starts, checks if daemon is running:
   - Read `~/.config/logmon/daemon.pid`
   - If PID file exists and process is alive: connect
   - Otherwise: spawn `logmon-mcp-server daemon` as detached child, wait for socket/port to appear, connect
2. Daemon writes PID file on startup, deletes on clean shutdown

### Shutdown

Daemon shuts down when ALL of:
- No connected sessions
- No named sessions with active triggers
- Idle timeout reached (configurable, default 30 minutes)

### State Persistence

**Directory:** `~/.config/logmon/` (created on first run)

**Files:**
- `daemon.pid` — daemon PID for auto-start detection
- `state.json` — persistent state (written periodically + on shutdown)
- `logmon.sock` — Unix domain socket (Unix only)
- `buffer.json` — optional log buffer export (if `persist_buffer_on_exit` enabled)

**state.json:**
```json
{
    "last_seq": 48523
}
```

Extensible for future persistent state.

**buffer.json (optional):**
- Config option `persist_buffer_on_exit` (default false)
- On shutdown: export buffer to `buffer.json`
- On startup: if file exists, import into buffer, delete file

## LogReceiver Trait

Abstraction for log input protocols. GELF is the first (and currently only) implementation. Designed for future syslog, journald, etc.

```rust
#[async_trait]
trait LogReceiver: Send + Sync {
    /// Start listening, feed logs into the pipeline
    async fn start(config: &ReceiverConfig, pipeline: Arc<LogPipeline>) -> Result<Box<dyn LogReceiver>>
    where Self: Sized;

    /// Human-readable name ("gelf", "syslog", etc.)
    fn name(&self) -> &str;

    /// Addresses/ports this receiver is listening on
    fn listening_on(&self) -> Vec<String>;

    /// Graceful shutdown
    async fn shutdown(self: Box<Self>);
}
```

`GelfReceiver` wraps the existing UDP and TCP listeners. The daemon starts all configured receivers at launch.

`get_status` reports all active receivers and their listening addresses.

## MCP Tools

All existing tools remain unchanged from Claude's perspective. New tools:

**`get_sessions`**
- Parameters: none
- Returns: List of all sessions with name, connected/disconnected, trigger count, filter count, queue size, last seen

**`drop_session`**
- Parameters: `name` (string)
- Returns: Confirmation
- Removes a named session and all its state (triggers, filters, queued notifications)

## Project Structure Changes

```
src/
├── main.rs                  # CLI dispatch: shim mode vs daemon mode
├── config.rs                # Config for both modes (clap subcommands)
├── shim/
│   ├── mod.rs
│   ├── bridge.rs            # MCP stdio ↔ daemon RPC translation
│   └── auto_start.rs        # Daemon detection and auto-start
├── daemon/
│   ├── mod.rs
│   ├── server.rs            # Daemon main loop, socket listener
│   ├── session.rs           # SessionRegistry, SessionState
│   ├── rpc.rs               # JSON-RPC request handling
│   └── persistence.rs       # state.json read/write
├── receiver/
│   ├── mod.rs               # LogReceiver trait
│   └── gelf.rs              # GelfReceiver (wraps existing UDP/TCP)
├── engine/
│   ├── mod.rs
│   ├── pipeline.rs          # LogPipeline (unchanged core logic)
│   ├── pre_buffer.rs        # PreTriggerBuffer (unchanged)
│   └── trigger.rs           # TriggerManager (unchanged, instantiated per-session)
├── filter/
│   ├── mod.rs
│   ├── parser.rs            # unchanged
│   └── matcher.rs           # unchanged
├── store/
│   ├── mod.rs
│   ├── traits.rs            # unchanged
│   └── memory.rs            # unchanged
├── gelf/
│   ├── mod.rs
│   ├── message.rs           # LogEntry, parsing (unchanged)
│   ├── udp.rs               # UDP listener (unchanged, used by GelfReceiver)
│   └── tcp.rs               # TCP listener (unchanged, used by GelfReceiver)
└── mcp/
    ├── mod.rs
    ├── server.rs            # GelfMcpServer (unchanged tool definitions)
    ├── tools_status.rs      # now delegates to daemon via shim
    └── notifications.rs     # forwards daemon notifications to MCP
```

Key changes:
- `shim/` — new, thin bridge
- `daemon/` — new, manages sessions and socket server
- `receiver/` — new, LogReceiver trait + GelfReceiver
- `engine/`, `filter/`, `store/`, `gelf/` — mostly unchanged, moved to daemon context
- `mcp/` — tool handlers now RPC through shim instead of calling pipeline directly

## Future Work (Out of Scope)

- **Launchd/systemd service installation** — `logmon-mcp-server install-service` for always-on daemon
- **Syslog receiver** — `SyslogReceiver` implementing `LogReceiver` trait
- **Persistent storage** — SQLite implementation of LogStore
- **SSE MCP transport** — direct Claude connection to daemon without shim
