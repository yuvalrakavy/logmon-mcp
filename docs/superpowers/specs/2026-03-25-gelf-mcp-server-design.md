# GELF MCP Server — Design Spec

## Purpose

An MCP server that collects GELF-formatted logs from any application and makes them accessible to Claude in real-time. Claude receives notifications when interesting events occur and can query the log buffer on demand.

This enables Claude to have direct, continuous access to application logs during development, without requiring the developer to copy-paste log output. Any application that emits GELF over UDP (e.g., via Rust's `tracing-gelf`, Python's `pygelf`, Go's `go-gelf`, or any GELF-compatible logging library) can send logs to this server.

## Context

- GELF (Graylog Extended Log Format) is a structured log format supported by many logging libraries across languages.
- The MCP server acts as a GELF receiver, replacing (or running alongside) a traditional log aggregator like Graylog.
- Primary use case is real-time development debugging. Historical log search will be added later.

## Architecture

Single Rust binary with a storage trait abstraction for future persistence.

### Components

```
┌─────────────────┐     ┌─────────────────┐
│   Application A  │     │  Application B  │
│  (GELF logging)  │     │  (GELF logging) │
└────────┬────────┘     └────────┬────────┘
         │ UDP/TCP GELF             │ UDP/TCP GELF
         └──────────┬─────────────┘
                    ▼
         ┌─────────────────────────────────┐
         │      gelf-mcp-server            │
         │                                 │
         │  ┌───────────┐  ┌────────────┐  │
         │  │   GELF    │  │  Trigger    │  │
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

**GELF Listener**: Listens for GELF messages on both UDP and TCP (default port 12201 for both, configurable). UDP receives individual JSON messages; TCP receives null-byte delimited JSON streams. Both are parsed into `LogEntry` structs and forwarded to the trigger engine and log store.

**Trigger Engine**: Evaluates every incoming log against all configured triggers, regardless of buffer filters. When a trigger matches, sends an MCP notification to Claude. If the trigger has `auto_activate` set and filters are defined, clears all filters (restoring implicit `ALL`).

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
    host: String,                              // GELF "host" (source application)
    facility: Option<String>,                  // GELF "facility" (module path)
    additional_fields: HashMap<String, Value>,  // GELF "_xxx" fields (structured data)
}
```

GELF syslog levels are mapped to Rust-style levels: 0-3 → ERROR, 4 → WARN, 5-6 → INFO, 7 → DEBUG. GELF does not define a TRACE level; the `tracing-gelf` crate maps TRACE to syslog level 7 (DEBUG). To distinguish them, we use the `_level` additional field that `tracing-gelf` includes.

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
| `"BUG",h=myapp` | "BUG" in any field AND host is myapp |
| `fa=mqtt` | Facility contains "mqtt" |
| `"connection refused",h=myapp` | Connection errors from a specific host |
| `"panic\|unwrap failed\|stack backtrace"` | Panic-related patterns |

## Buffer Filters

Buffer filters control which logs are stored. They follow the same pattern as triggers — a list of DSL expressions with OR semantics across them, AND within each expression.

### Semantics

- **No filters defined** — everything is buffered (implicit `ALL`)
- **One or more filters defined** — a log is buffered if ANY filter matches; if none match, the log is discarded
- **Matched filter descriptions are attached to the stored LogEntry** — so Claude can see why each log was kept

Buffer filters are orthogonal to triggers. Triggers always evaluate every incoming log regardless of buffer filters.

### Filter Definition

```rust
struct BufferFilter {
    id: u32,                        // auto-assigned
    condition: ParsedFilter,        // uses the DSL
    description: Option<String>,    // annotation (e.g., "MQTT debugging")
}
```

### LogEntry Extension

When a log is buffered, the descriptions of all matching filters are recorded:

```rust
struct LogEntry {
    // ... existing fields ...
    matched_filters: Vec<String>,  // descriptions of filters that matched this entry
}
```

### Filter Management Tools

| Tool | Parameters | Returns |
|------|-----------|---------|
| `get_filters` | none | List of all buffer filters with id, filter, description |
| `add_filter` | `filter` (DSL string), optional `description` | Created filter with assigned ID |
| `edit_filter` | `id`, optional `filter`, optional `description` | Updated filter |
| `remove_filter` | `id` | Confirmation |

To return to buffering everything, remove all filters (restores implicit `ALL` default).

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

Default triggers are mutable — they can be edited or removed like any other trigger.

### Trigger Evaluation Flow

For every incoming GELF message:

1. Parse into LogEntry
2. Evaluate all triggers — if matched, send MCP notification; if `auto_activate` and filters are defined, clear all filters (restoring implicit `ALL` — buffer everything)
3. Evaluate buffer filters — if no filters defined or any filter matches, append to LogStore

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

### Status Tool

**`get_status`**
- Parameters: none
- Returns: Active filter count, buffer size, trigger count, UDP port, TCP port, connected TCP clients, uptime

### Buffer Filter Management Tools

**`get_filters`**
- Parameters: none
- Returns: List of all buffer filters with id, filter, description

**`add_filter`**
- Parameters: `filter` (DSL string), optional `description`
- Returns: Created filter with assigned ID

**`edit_filter`**
- Parameters: `id` (u32), optional `filter`, optional `description`
- Returns: Updated filter definition

**`remove_filter`**
- Parameters: `id` (u32)
- Returns: Confirmation

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
    "active_filters": 0
}
```

## Configuration

### Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `GELF_PORT` | `12201` | Port for both UDP and TCP listeners |
| `GELF_UDP_PORT` | — | Override port for UDP only |
| `GELF_TCP_PORT` | — | Override port for TCP only |
| `GELF_BUFFER_SIZE` | `10000` | Max log entries in ring buffer |

### CLI Arguments

Same options available as CLI args (take precedence over env vars):

```
gelf-mcp-server [--port 12201] [--udp-port 12201] [--tcp-port 12201] [--buffer-size 10000]
```

`--port` sets both UDP and TCP. `--udp-port` and `--tcp-port` override individually.

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
│   │   ├── udp.rs           # UDP listener
│   │   ├── tcp.rs           # TCP listener (null-byte delimited)
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
- **Chunked GELF**: Support reassembly of chunked UDP messages (for payloads exceeding UDP MTU)
