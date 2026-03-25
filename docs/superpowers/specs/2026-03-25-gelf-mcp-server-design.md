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
         ┌───────────────────────────────────────────────┐
         │      gelf-mcp-server                          │
         │                                               │
         │  ┌───────────┐                                │
         │  │   GELF    │                                │
         │  │ Listener  │                                │
         │  └─────┬─────┘                                │
         │        │ every log                            │
         │        ├──────────▶┌────────────┐             │
         │        │           │  Trigger    │             │
         │        │           │  Engine     │── notify Claude
         │        │           └──────┬─────┘             │
         │        │                  │ on match:         │
         │        │                  │ flush ──┐         │
         │        │                  │         │         │
         │        ├──────────▶┌──────▼──────┐  │         │
         │        │           │ Pre-trigger  │  │         │
         │        │           │ Buffer       │──┘         │
         │        │           │ (all logs)   │            │
         │        │           └─────────────┘            │
         │        ▼                   │                  │
         │  ┌───────────┐            │ flush to store   │
         │  │  Buffer    │            │                  │
         │  │  Filter    │            │                  │
         │  └─────┬─────┘            │                  │
         │        │ if matched       │                  │
         │        ▼                  ▼                   │
         │  ┌───────────────────────────┐               │
         │  │  LogStore (trait)          │               │
         │  │  ┌───────────────────┐    │               │
         │  │  │ InMemory RingBuf  │    │               │
         │  │  │ (future: SQLite)  │    │               │
         │  │  └───────────────────┘    │               │
         │  └───────────────────────────┘               │
         │                                               │
         │  ┌───────────┐                                │
         │  │ MCP Server │ stdio / SSE                   │
         │  │ (Tools +   │◀──── Claude                   │
         │  │  Notifs)   │                                │
         │  └───────────┘                                │
         └───────────────────────────────────────────────┘
```

### Component Responsibilities

**GELF Listener**: Listens for GELF messages on both UDP and TCP (default port 12201 for both, configurable). UDP receives individual JSON messages; TCP receives null-byte delimited JSON streams. Both are parsed into `LogEntry` structs and forwarded to the trigger engine and log store. Malformed messages (invalid JSON, missing required fields, wrong types) are counted and logged to stderr — never crash, never store in the log buffer.

**Trigger Engine**: Evaluates every incoming log against all configured triggers, regardless of buffer filters. When a trigger matches: flushes the pre-trigger buffer into the LogStore, sends an MCP notification to Claude, and activates a post-trigger window. During an active post-window, incoming logs skip trigger evaluation entirely and go straight to the store.

**Pre-trigger Buffer**: A ring buffer that captures all incoming logs regardless of filters. Automatically sized to `max(pre_window)` across all triggers. When a specific trigger fires, its `pre_window` worth of entries are flushed to the LogStore.

**Post-trigger Window**: After a trigger fires, the next N logs (per-trigger `post_window`, default 200) are stored unconditionally, bypassing both trigger evaluation and buffer filters. This captures the aftermath of an event and naturally prevents trigger cascading.

**LogStore (trait)**: Abstraction over log storage. v1 implements `InMemoryStore` — a `VecDeque`-based ring buffer behind a `RwLock`. Max entries configurable (default 10,000). The trait allows swapping in SQLite or file-based persistence later.

**MCP Server**: Exposes tools and notifications over stdio transport (also runnable standalone for testing). Handles tool calls from Claude and dispatches trigger notifications.

## Data Model

### LogEntry

Normalized from GELF messages:

```rust
struct LogEntry {
    seq: u64,                                  // monotonically increasing, assigned at ingestion
    timestamp: DateTime<Utc>,
    level: Level,                              // ERROR, WARN, INFO, DEBUG, TRACE
    message: String,                           // GELF "short_message"
    full_message: Option<String>,              // GELF "full_message" (stack traces)
    host: String,                              // GELF "host" (source application)
    facility: Option<String>,                  // GELF "facility" (module path)
    additional_fields: HashMap<String, Value>,  // GELF "_xxx" fields (structured data)
}
```

The `seq` field is a unique, monotonically increasing ID assigned when the message is received. Used for deduplication during pre-trigger buffer flush, referencing specific entries, and ordering.

GELF syslog levels are mapped to Rust-style levels: 0-3 → ERROR, 4 → WARN, 5-6 → INFO, 7 → DEBUG. GELF does not define a TRACE level; the `tracing-gelf` crate maps TRACE to syslog level 7 (DEBUG). To distinguish them, we use the `_level` additional field that `tracing-gelf` includes.

### LogStore Trait

```rust
trait LogStore: Send + Sync {
    fn append(&self, entry: LogEntry);
    fn recent(&self, count: usize, filter: Option<Filter>) -> Vec<LogEntry>;
    fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry>;
    fn context_by_time(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry>;
    fn contains_seq(&self, seq: u64) -> bool;  // for deduplication during pre-trigger flush
    fn clear(&self);
    fn len(&self) -> usize;
    fn stats(&self) -> StoreStats;             // total received, total stored, malformed count
}
```

## Filter DSL

A filter is a comma-separated list of qualifiers. All qualifiers must match (AND semantics). For OR logic, create separate triggers/filters.

### Qualifier Syntax

| Form | Meaning |
|------|---------|
| `<pattern>` | Matched against all fields |
| `<selector>=<pattern>` | Matched against a specific field |
| `l>=<level>` | Log level comparison (see Level Selector) |

Use double quotes to include literal commas or equals signs in patterns: `"key=value"`, `"connection refused, retrying"`.

### Pattern Types

| Form | Meaning | Example |
|------|---------|---------|
| `text` | Substring match (contains), **case-insensitive** | `bug` matches "BUG", "Bug", "bug" |
| `/regex/` | Full regex, case-sensitive by default | `/^ERROR:.*timeout/` |
| `/regex/i` | Full regex, case-insensitive | `/connection (refused\|reset)/i` |

Substring matches are case-insensitive because log messages mix cases constantly. Use regex when case matters.

### Selectors

| Selector | Field |
|----------|-------|
| `m` | message (short_message) |
| `fm` | full_message |
| `mfm` | message or full_message (matches if either contains the pattern) |
| `h` | host |
| `fa` | facility (module path) |
| `l` | level (special: supports `>=`, `<=`, `=` comparisons) |
| `<other>` | Custom GELF additional field (`_xxx`) |

### Level Selector

The `l` selector uses comparison operators instead of pattern matching:

| Form | Meaning |
|------|---------|
| `l>=WARN` | Level is WARN or more severe (WARN, ERROR) |
| `l<=INFO` | Level is INFO or less severe (INFO, DEBUG, TRACE) |
| `l=DEBUG` | Level is exactly DEBUG |

Level names are case-insensitive. Severity order: ERROR > WARN > INFO > DEBUG > TRACE.

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
| `l>=ERROR` | All error-level and above |
| `l>=WARN,h=myapp` | Warnings and errors from myapp |
| `bug,h=myapp` | "bug" (case-insensitive) in any field AND host contains "myapp" |
| `fa=mqtt` | Facility contains "mqtt" |
| `connection refused,h=myapp` | "connection refused" from a specific host |
| `/panic\|unwrap failed\|stack backtrace/` | Regex: panic-related patterns |
| `mfm=/timeout.*retry/i` | Case-insensitive regex on message or full_message |
| `"key=value"` | Literal "key=value" in any field (quoted to avoid selector parsing) |

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
    source: LogSource,             // how this entry entered the store
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

## Event Capture Windows

When a trigger fires, logs are captured in a window around the event, regardless of buffer filters. Both window sizes are per-trigger, allowing Claude to tune them via `add_trigger` / `edit_trigger`.

### Pre-trigger Buffer (Flight Recorder)

A global rolling ring buffer that captures ALL incoming logs regardless of buffer filters. The buffer automatically sizes itself to `max(pre_window)` across all configured triggers. When a specific trigger fires, its `pre_window` entries are flushed into the LogStore at their correct chronological positions, tagged `source: pre-trigger`. Entries already in the LogStore (from buffer filters) are not duplicated. The buffer is cleared after flush and continues recording.

If all triggers have `pre_window: 0`, the pre-trigger buffer is disabled.

### Post-trigger Window

After a trigger fires, the next N logs (per-trigger `post_window`, default 200) are stored unconditionally, bypassing both trigger evaluation and buffer filters. Tagged `source: post-trigger`.

This serves two purposes:
- Captures the aftermath of an event (retries, cascading failures, recovery)
- Prevents trigger flooding — during the post-window, no triggers fire, so a cascading error doesn't produce 50 notifications

If multiple triggers match the same log, the largest `post_window` is used.

## Trigger System

### Trigger Definition

```rust
struct Trigger {
    id: u32,                        // auto-assigned
    condition: ParsedFilter,        // uses the DSL (level conditions via `l>=` selector)
    pre_window: u32,                // records to flush before trigger (default 500)
    post_window: u32,               // records to capture after trigger (default 200)
    notify_context: u32,            // entries to include before matched entry in notification (default 5)
    description: Option<String>,    // optional annotation (e.g., "watching MQTT disconnects")
    match_count: u64,               // how many times this trigger has fired
}
```

No separate `TriggerCondition` enum — the DSL handles everything including level conditions via the `l` selector.

### Default Triggers

Shipped out of the box:

| ID | Filter | Pre-window | Post-window | Notify-context | Description |
|----|--------|------------|-------------|----------------|-------------|
| 1 | `l>=ERROR` | 500 | 200 | 5 | Error-level log detected |
| 2 | `/panic\|unwrap failed\|stack backtrace/` | 500 | 200 | 5 | Panic or unwrap failure detected |

Default triggers are mutable — they can be edited or removed like any other trigger.

### Trigger Evaluation Flow

For every incoming GELF message:

1. Parse into LogEntry
2. If a post-trigger window is active:
   a. Append to LogStore (tagged `source: post-trigger`), decrement window counter
   b. Append to pre-trigger buffer
   c. Skip trigger evaluation and filter evaluation
   d. If window counter reaches 0, resume normal flow
3. Otherwise (normal flow):
   a. Append to pre-trigger buffer
   b. Evaluate all triggers — if matched:
      - Flush trigger's `pre_window` entries from pre-trigger buffer into LogStore (tagged `source: pre-trigger`, skip duplicates)
      - Send MCP notification to Claude
      - Activate post-trigger window (size from trigger's `post_window`)
      - If multiple triggers match same log, use largest `pre_window` for flush and largest `post_window`
   c. Evaluate buffer filters — if no filters defined or any filter matches, append to LogStore (tagged `source: filter`)

## MCP Tools

### Log Query Tools

**`get_recent_logs`**
- Parameters: `count` (u32, default 100), optional `filter` (DSL string)
- Returns: Array of LogEntry, newest first
- Retrieves the most recent logs from the buffer, optionally filtered

**`get_log_context`**
- Parameters: `seq` (u64, a log entry's sequence number), optional `before` (u32, default 10), optional `after` (u32, default 10)
- Returns: Array of LogEntry surrounding the specified entry
- Can also accept `timestamp` (ISO 8601) + `window_secs` (u32, default 5) as an alternative to seq-based lookup

**`export_logs`**
- Parameters: `path` (file path), optional `count` (u32, default all buffered), optional `filter` (DSL string), optional `format` ("json" or "text", default "json")
- Returns: Confirmation + number of entries exported
- Writes matching log entries from the buffer to a file for comparison, sharing, or archival

**`clear_logs`**
- Parameters: none
- Returns: Confirmation + number of entries cleared
- Clears the LogStore buffer. Pre-trigger buffer is unaffected. Useful for starting fresh before a test.

### Status Tool

**`get_status`**
- Parameters: none
- Returns: active filter count, buffer size, trigger count, UDP port, TCP port, connected TCP clients, uptime, pre-trigger buffer size/capacity, active post-window state (remaining records, trigger id), total messages received, total messages stored, malformed message count

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
- Returns: List of all triggers with id, filter, pre_window, post_window, notify_context, description, match count

**`add_trigger`**
- Parameters: `filter` (DSL string), optional `pre_window` (u32, default 500), optional `post_window` (u32, default 200), optional `notify_context` (u32, default 5), optional `description`
- Returns: Created trigger with assigned ID

**`edit_trigger`**
- Parameters: `id` (u32), optional `filter`, optional `pre_window`, optional `post_window`, optional `notify_context`, optional `description`
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
    "filter": "/panic|unwrap failed|stack backtrace/",
    "matched_entry": { /* LogEntry */ },
    "context_before": [ /* notify_context entries preceding the match */ ],
    "pre_trigger_flushed": 500,
    "post_window_size": 200,
    "buffer_size": 4523
}
```

The `context_before` array contains up to `notify_context` log entries immediately preceding the matched entry (from the pre-trigger buffer). This gives Claude enough context to assess severity without an extra tool call.

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

## Limitations

- **Chunked GELF UDP is not supported.** Use TCP for messages exceeding the UDP MTU. Chunked GELF reassembly adds complexity for minimal benefit given TCP support.

## Future Work (Out of Scope for v1)

- **Persistent storage**: SQLite implementation of LogStore for historical queries
- **Rate anomaly detection**: Trigger on sudden error rate spikes
- **SSE transport**: For remote/multi-client scenarios
