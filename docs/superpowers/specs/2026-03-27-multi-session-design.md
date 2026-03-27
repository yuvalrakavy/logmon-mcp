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
3. Translate MCP tool calls → daemon RPC calls (resolve relative paths to absolute before sending)
4. Forward daemon notifications → MCP notifications to Claude
5. Detect daemon disconnection → notify Claude, attempt auto-restart and reconnect
6. On disconnect: daemon handles cleanup based on session type

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

1. Shim connects → sends `session.start` with optional name and `protocol_version`
2. Daemon checks protocol version — if mismatch, returns error. Shim should restart the daemon (kill old, start new) and retry.
3. Daemon looks up name:
   - Found + disconnected: reconnect, drain queued notifications
   - Found + connected: error (session already in use)
   - Not found + named: create new session with default triggers, mark as persistent
   - Not found + anonymous: create new session with default triggers, mark as ephemeral
4. Normal operation: shim relays MCP tool calls as RPC, daemon evaluates triggers and sends notifications
5. Shim disconnects:
   - Anonymous: remove session from registry
   - Named: mark disconnected, keep state, start queuing notifications
6. Named session TTL expires with no reconnect: auto-cleanup

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
2. Append to pre-trigger buffer (global, read-only ring buffer — entries are only evicted by capacity, never by flush)
3. For each connected session + named disconnected sessions:
   a. If post-trigger window is active for this session: skip trigger evaluation, decrement counter, continue to next session
   b. Evaluate session's triggers — if matched:
      - Copy pre_window entries from pre-trigger buffer into LogStore (skip duplicates via seq). Note: entries are copied, not removed — other sessions may need the same context.
      - If session connected: send notification to shim → Claude
      - If session disconnected (named): queue notification (cap at max_queue_size)
      - Activate post-trigger window for this session
4. Evaluate buffer storage:
   a. If any session has an active post-trigger window: store unconditionally (post-window logs are valuable context for all sessions)
   b. Otherwise: union all sessions' filters. If no session has filters, store everything (implicit ALL). If any session's filter matches, store the log.

### Query-Time Filtering

Session filters serve as a **default lens** for queries:

- `get_recent_logs()` with no explicit filter → session's own filters are applied (the session sees its configured view)
- `get_recent_logs(filter="l>=ERROR")` with explicit filter → overrides session filters (ad-hoc query across all stored data)
- Session with no filters → sees everything in the store

### Global Operations

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
| `session.start` | (on connect) | `{ name?: string, protocol_version: u32 }` |
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

On reconnect of a named session, queued notifications are drained in chronological order before any new notifications are forwarded. This prevents out-of-order events.

### Shim Reconnection (Daemon Crash/Restart)

If the shim detects a broken socket connection (daemon crashed or was killed):

1. Send MCP notification to Claude: "logmon daemon disconnected, attempting restart"
2. Follow the auto-start procedure (acquire lock, check PID, spawn if needed)
3. Reconnect to daemon with the same session name (if named) or create new anonymous session
4. For named sessions: state is restored from `state.json` — triggers and filters are preserved. Notification queue is lost (not persisted).
5. For anonymous sessions: triggers and filters are lost — Claude is informed via notification
6. Send MCP notification to Claude: "logmon daemon reconnected" (with session state summary)

## Daemon Lifecycle

### Auto-start

1. Shim acquires file lock (`~/.config/logmon/daemon.lock`) to prevent race conditions when multiple shims start simultaneously
2. Read `~/.config/logmon/daemon.pid`:
   - If PID file exists and process is alive: release lock, connect
   - If PID file exists but process is dead (unclean shutdown): delete stale PID file and socket file, continue to step 3
   - If no PID file: continue to step 3
3. Spawn `logmon-mcp-server daemon` as detached child, wait for socket/port to appear (timeout: 10 seconds — if exceeded, report error to Claude and exit)
4. Release lock, connect
5. Daemon startup order: read config → read state → start receivers (bind ports) → only after receivers succeed: write PID file → listen on socket. If receiver fails (e.g., port in use by non-logmon process), daemon exits with clear error before writing PID file.
6. Daemon deletes PID file and socket file on clean shutdown

### Shutdown

Daemon shuts down when ALL of:
- No connected sessions
- No named sessions with active triggers
- Idle timeout reached (configurable, default 30 minutes)

### State Persistence

**Directory:** `~/.config/logmon/` (created on first run)

**Files:**
- `config.json` — daemon configuration (ports, buffer size, options)
- `daemon.pid` — daemon PID for auto-start detection
- `daemon.lock` — file lock for auto-start race prevention
- `daemon.log` — daemon log output (the daemon has no terminal; max size 10MB, rotated)
- `state.json` — persistent state: seq counter + named session definitions (written periodically + on shutdown)
- `logmon.sock` — Unix domain socket (Unix only)
- `buffer.json` — optional log buffer export (if `persist_buffer_on_exit` enabled)

**state.json:**
```json
{
    "last_seq": 48523,
    "named_sessions": {
        "store-debug": {
            "triggers": [ /* trigger definitions */ ],
            "filters": [ /* filter definitions */ ]
        }
    }
}
```

Named sessions (triggers and filters) are persisted so they survive daemon restarts. Notification queues are not persisted — only configuration. On daemon restart, named sessions are restored in disconnected state, ready for reconnection.

**config.json** (daemon configuration):
```json
{
    "gelf_port": 12201,
    "gelf_udp_port": null,
    "gelf_tcp_port": null,
    "buffer_size": 10000,
    "persist_buffer_on_exit": false,
    "idle_timeout_secs": 1800
}
```

The daemon reads this file on startup if it exists — all fields are optional with sensible defaults. CLI args override file values. The shim does not need to know about daemon config — it just connects. This decouples shim configuration (session name) from daemon configuration (ports, buffer sizes).

Similarly, `state.json` is optional on first run — the daemon starts with `last_seq: 0` and no named sessions.

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

All existing tools remain unchanged from Claude's perspective. `get_status` is extended to include: total sessions, connected sessions, current session name/id (so Claude knows its own identity). New tools:

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
│   ├── pipeline.rs          # LogPipeline (refactored: shared infrastructure only)
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
- `shim/` — new, thin bridge. Owns the `mcp/` layer (MCP tool definitions, stdio transport)
- `daemon/` — new, manages sessions, socket server, RPC handlers
- `receiver/` — new, LogReceiver trait + GelfReceiver
- `engine/` — `LogPipeline` refactored to shared infrastructure (store + pre-buffer + seq counter); per-session trigger/filter iteration moves to daemon's log processing loop. `TriggerManager` and `PreTriggerBuffer` unchanged.
- `filter/`, `store/`, `gelf/` — unchanged, used by daemon
- `mcp/` — tool definitions unchanged (same parameters/descriptions), but implementations now serialize to RPC instead of calling pipeline directly

## Implementation Notes

- **Session name validation.** Named session names must match `[a-zA-Z0-9_-]+` (alphanumeric, hyphens, underscores). Empty strings and names containing path separators or special characters are rejected.
- **Trigger IDs are session-scoped.** Session A's trigger 3 and session B's trigger 3 are unrelated. Tools operate on "my session's triggers" implicitly.
- **Pre-trigger buffer resizing.** The daemon recalculates `max(pre_window across all sessions' triggers)` and calls `pre_buffer.resize()` whenever a session is added, removed, or has triggers edited.
- **Socket → session mapping.** The daemon maintains a `HashMap<ConnectionId, SessionId>` to route RPC requests to the correct session. Each socket connection is associated with exactly one session.
- **Path resolution.** The shim resolves relative file paths (e.g., in `export_logs`) to absolute paths before sending to the daemon, since the daemon's CWD differs from Claude's project directory.
- **Post-trigger window and storage.** When any session has an active post-trigger window, the log is stored unconditionally. This means one session's trigger can cause logs to be stored that benefit other sessions — this is intentional and desirable.
- **`drop_session` restrictions.** Only named sessions can be dropped (anonymous have no name to reference). A session cannot drop itself — that's what disconnecting does.
- **Pre-trigger flush optimization.** When multiple sessions' triggers match the same log, process sessions in order of largest `pre_window` first. The larger flush covers the smaller, reducing duplicate seq checks.

## Future Work (Out of Scope)

- **Launchd/systemd service installation** — `logmon-mcp-server install-service` for always-on daemon
- **Syslog receiver** — `SyslogReceiver` implementing `LogReceiver` trait
- **Persistent storage** — SQLite implementation of LogStore
- **SSE MCP transport** — direct Claude connection to daemon without shim
