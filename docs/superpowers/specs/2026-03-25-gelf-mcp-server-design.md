# GELF MCP Server — Design Spec

## Purpose

An MCP server that collects GELF-formatted logs from the Store ecosystem (store_server, ht_server) and makes them accessible to Claude in real-time. Claude receives notifications when interesting events occur and can query the log buffer on demand.

This enables Claude to have direct, continuous access to logs produced by the Store/HT projects during development, without requiring the developer to copy-paste log output.

## Context

- `store_server` and `ht_server` both use the `tracing_init` library, which wraps Rust's `tracing` crate and supports GELF output via `tracing-gelf` over UDP.
- The MCP server acts as a GELF receiver, replacing (or running alongside) a traditional log aggregator like Graylog.
- Primary use case is real-time development debugging. Historical log search will be added later.

## Architecture

Single Rust binary with a storage trait abstraction for future persistence.

### Components

```
┌─────────────────┐     ┌─────────────────┐
│  store_server    │     │   ht_server     │
│  (tracing_init)  │     │  (tracing_init) │
└────────┬────────┘     └────────┬────────┘
         │ UDP GELF                │ UDP GELF
         └──────────┬─────────────┘
                    ▼
         ┌─────────────────────────────────┐
         │      gelf-mcp-server            │
         │                                 │
         │  ┌───────────┐  ┌────────────┐  │
         │  │ UDP GELF  │  │  Trigger    │  │
         │  │ Listener  │─▶│  Engine     │  │
         │  └─────┬─────┘  └─────┬──────┘  │
         │        │              │          │
         │        ▼              │ notify   │
         │  ┌───────────┐       │          │
         │  │  LogStore  │◀──────┘          │
         │  │  (trait)   │                  │
         │  └─────┬─────┘                  │
         │        │                        │
         │  ┌─────▼─────┐                  │
         │  │ InMemory   │  (future:       │
         │  │ RingBuffer │   SQLite impl)  │
         │  └───────────┘                  │
         │                                 │
         │  ┌───────────┐                  │
         │  │ MCP Server │ stdio / SSE     │
         │  │ (Tools +   │◀──── Claude     │
         │  │  Notifs)   │                 │
         │  └───────────┘                  │
         └─────────────────────────────────┘
```

### Component Responsibilities

**UDP GELF Listener**: Binds to a configurable UDP port (default 12201, configurable via `GELF_PORT` env var or CLI arg). Receives GELF JSON messages, parses them into `LogEntry` structs, and forwards to the trigger engine and log store.

**Trigger Engine**: Evaluates every incoming log against all configured triggers, regardless of the buffer filter. When a trigger matches, sends an MCP notification to Claude. If the trigger has `auto_activate` set and the current buffer filter is `NONE`, switches the filter to `ALL`.

**LogStore (trait)**: Abstraction over log storage. v1 implements `InMemoryStore` — a `VecDeque`-based ring buffer behind a `RwLock`. Max entries configurable (default 10,000). The trait allows swapping in SQLite or file-based persistence later.

**MCP Server**: Exposes tools and notifications over stdio transport (also runnable standalone for testing). Handles tool calls from Claude and dispatches trigger notifications.

## Data Model

### LogEntry

Normalized from GELF messages:

```rust
struct LogEntry {
    timestamp: DateTime<Utc>,
    level: Level,                              // ERROR, WARN, INFO, DEBUG, TRACE
    message: String,                           // GELF "short_message"
    full_message: Option<String>,              // GELF "full_message" (stack traces)
    host: String,                              // GELF "host" (store_server vs ht_server)
    facility: Option<String>,                  // GELF "facility" (Rust module path)
    additional_fields: HashMap<String, Value>,  // GELF "_xxx" fields (tracing span data)
}
```

GELF syslog levels are mapped to Rust-style levels: 0-3 → ERROR, 4 → WARN, 5-6 → INFO, 7 → DEBUG.

### LogStore Trait

```rust
trait LogStore: Send + Sync {
    fn append(&self, entry: LogEntry);
    fn recent(&self, count: usize, filter: Option<Filter>) -> Vec<LogEntry>;
    fn context(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry>;
    fn clear(&self);
    fn len(&self) -> usize;
}
```

## Filter DSL

A filter is a comma-separated list of qualifiers. All qualifiers must match (AND semantics). For OR logic, create separate triggers.

### Qualifier Syntax

| Form | Meaning |
|------|---------|
| `<pattern>` | Regex matched against all fields |
| `<selector>=<regex>` | Regex matched against a specific field |

### Selectors

| Selector | Field |
|----------|-------|
| `m` | message (short_message) |
| `fm` | full_message |
| `h` | host |
| `fa` | facility (module path) |
| `<other>` | Custom GELF additional field (`_xxx`) |

### Special Patterns

| Pattern | Meaning |
|---------|---------|
| `ALL` | Matches everything |
| `NONE` | Matches nothing |

### Examples

| Filter | Meaning |
|--------|---------|
| `ALL` | Match all logs |
| `NONE` | Match no logs |
| `"BUG",h=ht_server` | "BUG" in any field AND host is ht_server |
| `fa=mqtt` | Facility contains "mqtt" |
| `"connection refused",h=store_server` | Connection errors from store_server |
| `"panic\|unwrap failed\|stack backtrace"` | Panic-related patterns |

## Buffer Filter

The buffer filter controls which logs are stored. It uses the same DSL as triggers:

- `ALL` — store every log (full collection)
- `NONE` — store nothing (monitoring only, triggers still evaluate)
- Any DSL expression — selective collection

The buffer filter is orthogonal to triggers. Triggers always evaluate every incoming log regardless of the buffer filter.

## Trigger System

### Trigger Definition

```rust
struct Trigger {
    id: u32,                        // auto-assigned
    condition: TriggerCondition,
    auto_activate: bool,            // switch buffer filter from NONE → ALL when fired
    description: Option<String>,    // optional annotation (e.g., "watching MQTT disconnects")
    match_count: u64,               // how many times this trigger has fired
}

enum TriggerCondition {
    LevelAtLeast(Level),
    Filter(ParsedFilter),           // uses the DSL
    LevelAndFilter(Level, ParsedFilter),
}
```

### Default Triggers

Shipped out of the box:

| ID | Condition | Auto-activate | Description |
|----|-----------|---------------|-------------|
| 1 | level >= ERROR | true | Error-level log detected |
| 2 | pattern: `panic\|unwrap failed\|stack backtrace` | true | Panic or unwrap failure detected |

### Trigger Evaluation Flow

For every incoming GELF message:

1. Parse into LogEntry
2. Evaluate all triggers — if matched, send MCP notification; if `auto_activate` and buffer filter is `NONE`, switch to `ALL`
3. Evaluate buffer filter — if matched, append to LogStore

## MCP Tools

### Log Query Tools

**`get_recent_logs`**
- Parameters: `count` (u32, default 100), optional `filter` (DSL string)
- Returns: Array of LogEntry, newest first
- Retrieves the most recent logs from the buffer, optionally filtered

**`get_log_context`**
- Parameters: `timestamp` (ISO 8601), `window_secs` (u32, default 5)
- Returns: Array of LogEntry within the time window around the given timestamp
- Useful for examining what happened around a specific event

### Buffer & Status Tools

**`set_filter`**
- Parameters: `filter` (DSL string: `"ALL"`, `"NONE"`, or any DSL expression)
- Returns: Confirmation + previous filter
- Controls which logs are buffered

**`get_status`**
- Parameters: none
- Returns: Current buffer filter, buffer size, trigger count, listener port, uptime

### Trigger Management Tools

**`get_triggers`**
- Parameters: none
- Returns: List of all triggers with id, filter, auto_activate, description, match count

**`add_trigger`**
- Parameters: `filter` (DSL string), `auto_activate` (bool, default true), optional `description`
- Returns: Created trigger with assigned ID

**`edit_trigger`**
- Parameters: `id` (u32), optional `filter`, optional `auto_activate`, optional `description`
- Returns: Updated trigger definition

**`remove_trigger`**
- Parameters: `id` (u32)
- Returns: Confirmation

## MCP Notifications

When a trigger fires, Claude receives a notification containing:

```json
{
    "trigger_id": 2,
    "trigger_description": "Panic or unwrap failure detected",
    "filter": "panic|unwrap failed|stack backtrace",
    "matched_entry": { /* LogEntry */ },
    "buffer_size": 4523,
    "buffer_filter": "ALL"
}
```

## Configuration

### Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `GELF_PORT` | `12201` | UDP port to listen on |
| `GELF_BUFFER_SIZE` | `10000` | Max log entries in ring buffer |

### CLI Arguments

Same options available as CLI args (take precedence over env vars):

```
gelf-mcp-server [--port 12201] [--buffer-size 10000]
```

### MCP Config (Claude Code settings)

```json
{
  "mcpServers": {
    "gelf-logs": {
      "command": "/path/to/gelf-mcp-server",
      "args": ["--port", "12201"]
    }
  }
}
```

## Deployment

- **Standalone**: Run the binary directly for testing. Listens on UDP and accepts MCP commands via stdio.
- **Claude Code managed**: Configured as an MCP server in Claude Code settings. Launched automatically when Claude starts.

## Project Structure

```
gelf-mcp-server/
├── Cargo.toml
├── src/
│   ├── main.rs              # CLI entry, arg parsing, starts Tokio runtime
│   ├── gelf/
│   │   ├── mod.rs
│   │   ├── listener.rs      # UDP socket, GELF parsing
│   │   └── message.rs       # LogEntry struct, GELF → LogEntry conversion
│   ├── store/
│   │   ├── mod.rs
│   │   ├── traits.rs        # LogStore trait + Filter types
│   │   └── memory.rs        # InMemoryStore (VecDeque ring buffer)
│   ├── filter/
│   │   ├── mod.rs
│   │   ├── parser.rs        # DSL parser (qualifier list → ParsedFilter)
│   │   └── matcher.rs       # Filter evaluation against LogEntry
│   ├── triggers/
│   │   ├── mod.rs
│   │   ├── engine.rs        # Trigger evaluation loop
│   │   └── types.rs         # Trigger, TriggerCondition
│   └── mcp/
│       ├── mod.rs
│       ├── server.rs         # MCP stdio server setup
│       ├── tools.rs          # Tool handlers
│       └── notifications.rs  # Notification dispatch to Claude
```

## Future Work (Out of Scope for v1)

- **Persistent storage**: SQLite implementation of LogStore for historical queries
- **Rate anomaly detection**: Trigger on sudden error rate spikes
- **SSE transport**: For remote/multi-client scenarios
- **Log export**: Dump buffer to file for sharing
