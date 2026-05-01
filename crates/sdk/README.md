# logmon-broker-sdk

Typed Rust client for the logmon broker daemon.

`logmon-broker-sdk` speaks JSON-RPC 2.0 over a Unix domain socket against `logmon-broker`. It exposes a typed method per RPC, a typed `Notification` enum on a broadcast channel, a builder for the broker's filter DSL, and a reconnection state machine that resumes named sessions across daemon restarts.

This guide is the canonical reference for SDK consumers (test harnesses, archival workers, dashboards). The `logmon-mcp` shim is the first SDK consumer; `store-test` is the second. Anything that needs broker access from Rust without going through MCP belongs here.

> **Note on cursor support.** This guide describes the cursor surface from `docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md`. The cursor implementation lands together with this doc on the `feat/broker-ification` branch; references to `Filter::builder().cursor(...)` and `cursor_advanced_to` reflect the post-implementation state.

---

## Quick start

```toml
# Cargo.toml
[dependencies]
logmon-broker-sdk = { path = "../path/to/logmon-mcp/crates/sdk" }
logmon-broker-protocol = { path = "../path/to/logmon-mcp/crates/protocol" }
tokio = { version = "1", features = ["full"] }
```

```rust
use logmon_broker_sdk::{Broker, Filter, Level, Notification};
use logmon_broker_protocol::{LogsRecent, TriggersAdd};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect — auto-discovers the broker socket at ~/.config/logmon/logmon.sock
    let broker = Broker::connect()
        .session_name("my-tool")          // named session: persists across reconnect
        .client_info(json!({ "name": "my-tool", "version": "0.1.0" }))
        .open()
        .await?;

    // Typed call
    let result = broker.logs_recent(LogsRecent {
        count: Some(20),
        filter: Some(Filter::builder().level_at_least(Level::Error).build()),
        ..Default::default()
    }).await?;
    for entry in &result.logs {
        println!("[{}] {}", entry.level, entry.message);
    }

    // Subscribe to push notifications
    let mut sub = broker.subscribe_notifications();
    let trigger = broker.triggers_add(TriggersAdd {
        filter: "l>=ERROR, fa=mqtt".into(),
        oneshot: true,
        ..Default::default()
    }).await?;

    while let Ok(notif) = sub.recv().await {
        match notif {
            Notification::TriggerFired(payload) if payload.trigger_id == trigger.id => {
                println!("Trigger fired: {}", payload.matched_entry.message);
                break;
            }
            Notification::Reconnected => eprintln!("session resumed"),
            _ => {}
        }
    }

    Ok(())
}
```

### Iterating recent traces

```rust
use logmon_broker_sdk::Broker;
use logmon_broker_protocol::TracesRecent;

let result = broker.traces_recent(TracesRecent {
    count: Some(20),
    ..Default::default()
}).await?;

for t in &result.traces {
    println!(
        "[{}] {} ({}) — {:.1} ms, {} spans{}",
        t.trace_id,
        t.root_span_name,
        t.service_name,
        t.total_duration_ms,
        t.span_count,
        if t.has_errors { " ⚠ errors" } else { "" },
    );
}
```

`traces_recent` returns `Vec<TraceSummary>` — one row per trace, with the root span's name, service, total wall-clock duration, span count, and an `has_errors` flag (boolean — see "Record types" below for the full shape).

### Walking one trace's spans

```rust
use logmon_broker_sdk::Broker;
use logmon_broker_protocol::{TracesGet, SpanEntry};
use std::collections::HashMap;

let result = broker.traces_get(TracesGet {
    trace_id: "0123456789abcdef0123456789abcdef".into(),
    include_logs: Some(false),
    ..Default::default()
}).await?;

// Index spans by their span_id so we can walk parent → child.
let by_id: HashMap<&str, &SpanEntry> = result.spans.iter()
    .map(|s| (s.span_id.as_str(), s))
    .collect();

// Compute depth by walking parent_span_id chain to root.
fn depth_of(span: &SpanEntry, by_id: &HashMap<&str, &SpanEntry>) -> usize {
    let mut depth = 0;
    let mut cur = span;
    while let Some(parent_id) = &cur.parent_span_id {
        let Some(parent) = by_id.get(parent_id.as_str()) else { break };
        depth += 1;
        cur = parent;
    }
    depth
}

for span in &result.spans {
    let indent = "  ".repeat(depth_of(span, &by_id));
    println!(
        "{indent}{} ({}) {:.1} ms",
        span.name,
        span.service_name,
        span.duration_ms,
    );
}
```

`traces_get` returns the full span tree as a flat `Vec<SpanEntry>` plus the trace's linked logs (when `include_logs: true`). Walk the tree by indexing on `span_id` and following `parent_span_id`. Root spans have `parent_span_id: None`.

---

## Connecting

`Broker::connect()` returns a `BrokerBuilder`. Configure it, then call `.open().await`:

```rust
let broker = Broker::connect()
    .socket_path("/path/to/socket".into())  // override default discovery
    .session_name("my-session")              // None = anonymous (default)
    .client_info(json!({ "name": "my-tool" })) // ≤ 4 KB JSON; broker rejects oversized
    .reconnect_max_attempts(10)              // default 10
    .reconnect_initial_backoff(Duration::from_millis(100))  // default 100ms
    .reconnect_max_backoff(Duration::from_secs(30))         // default 30s
    .call_timeout(Duration::from_secs(60))   // default = max_attempts × max_backoff
    .open()
    .await?;
```

### Socket discovery

If `.socket_path()` isn't set, `BrokerBuilder` resolves the socket path in this order:

1. `LOGMON_BROKER_SOCKET` environment variable.
2. `~/.config/logmon/logmon.sock` (default on macOS and Linux; pinned to `.config/logmon/` even on macOS so the SDK and broker agree on every platform).
3. On Windows: `127.0.0.1:12200` TCP fallback.

### Session names

- **Anonymous** (default): broker assigns a UUID. State (triggers, filters, bookmarks) lives only for the connection's lifetime — disconnect drops everything.
- **Named** (`.session_name("..")`): state persists across disconnect and across daemon restart (via `state.json`). The same name reconnects to the same session.

Named sessions are required if you want reconnect-with-resume semantics.

### Reconnection model

The SDK includes a built-in reconnection state machine (`crates/sdk/src/reconnect.rs`). Behavior:

- **EOF on the bridge** (daemon restart, network blip): named sessions transition to `Reconnecting` and retry the handshake with exponential backoff (jittered ±15%, capped at `max_backoff`).
- **Successful resume** (`is_new: false` on the new handshake): emits `Notification::Reconnected` on the broadcast channel, then resumes processing daemon-drained queued notifications.
- **Resurrection** (`is_new: true` on the new handshake — the daemon lost our state): terminal `BrokerError::SessionLost`. No retry.
- **Anonymous session disconnect**: terminal `BrokerError::SessionLost` immediately, no retry attempts (no name to resume by).
- **Exhausted attempts**: terminal `BrokerError::Disconnected`.

In-flight calls during reconnect block on the state-changed signal until either the connection comes back (then proceed) or `call_timeout` expires (then `BrokerError::Disconnected`).

---

## Typed methods

Every JSON-RPC method has a typed `Broker::*` method. Param and result types come from `logmon_broker_protocol`.

| Method | Param type | Result type |
|---|---|---|
| `logs_recent` | `LogsRecent` | `LogsRecentResult` |
| `logs_context` | `LogsContext` | `LogsContextResult` |
| `logs_export` | `LogsExport` | `LogsExportResult` |
| `logs_clear` | `LogsClear` | `LogsClearResult` |
| `filters_list` | `FiltersList` | `FiltersListResult` |
| `filters_add` | `FiltersAdd` | `FiltersAddResult` |
| `filters_edit` | `FiltersEdit` | `FiltersEditResult` |
| `filters_remove` | `FiltersRemove` | `FiltersRemoveResult` |
| `triggers_list` | `TriggersList` | `TriggersListResult` |
| `triggers_add` | `TriggersAdd` | `TriggersAddResult` |
| `triggers_edit` | `TriggersEdit` | `TriggersEditResult` |
| `triggers_remove` | `TriggersRemove` | `TriggersRemoveResult` |
| `traces_recent` | `TracesRecent` | `TracesRecentResult` |
| `traces_get` | `TracesGet` | `TracesGetResult` |
| `traces_summary` | `TracesSummary` | `TracesSummaryResult` |
| `traces_slow` | `TracesSlow` | `TracesSlowResult` |
| `traces_logs` | `TracesLogs` | `TracesLogsResult` |
| `spans_context` | `SpansContext` | `SpansContextResult` |
| `bookmarks_add` | `BookmarksAdd` | `BookmarksAddResult` |
| `bookmarks_list` | `BookmarksList` | `BookmarksListResult` |
| `bookmarks_remove` | `BookmarksRemove` | `BookmarksRemoveResult` |
| `bookmarks_clear` | `BookmarksClear` | `BookmarksClearResult` |
| `session_list` | `SessionList` | `SessionListResult` |
| `session_drop` | `SessionDrop` | `SessionDropResult` |
| `status_get` | `StatusGet` | `StatusGetResult` |

All methods return `Result<R, BrokerError>`.

### Untyped escape hatches

For quick experimentation or when a method isn't yet typed:

```rust
broker.call(method: &str, params: serde_json::Value) -> Result<Value, BrokerError>
broker.call_typed::<P, R>(method: &str, params: P) -> Result<R, BrokerError>
    where P: Serialize, R: DeserializeOwned
```

### Capability discovery

`broker.has_capability("oneshot_triggers")` returns `bool` based on what the daemon advertised in `session.start`. Current capabilities at v1: `bookmarks`, `oneshot_triggers`, `client_info`. Use this to feature-gate code that depends on a specific broker version.

---

## Record types

The SDK returns these as fully-typed Rust structs from `logmon-broker-protocol`. The shapes documented below mirror the wire JSON the broker emits — every field name appears verbatim in `crates/protocol/src/methods.rs` and `crates/protocol/src/notifications.rs`. The JSON Schema at `crates/protocol/protocol-v1.schema.json` is drift-guarded against these definitions and is the canonical wire contract for cross-language clients.

For Rust consumers: `use logmon_broker_protocol::*;` brings everything into scope.

### Logs

```rust
pub enum Level { Trace, Debug, Info, Warn, Error }
// Wire format: variant name as-written ("Info", "Error", ...).

pub enum LogSource { Filter, PreTrigger, PostTrigger }
// Where the entry came from in the pipeline.

pub struct LogEntry {
    pub seq: u64,                            // Monotonic position; cursor-friendly.
    pub timestamp: chrono::DateTime<Utc>,    // ISO 8601 on the wire.
    pub level: Level,
    pub message: String,                     // GELF "short_message".
    pub full_message: Option<String>,        // GELF "full_message" if present.
    pub host: String,                        // GELF "host".
    pub facility: Option<String>,            // GELF "facility".
    pub file: Option<String>,                // GELF "file".
    pub line: Option<u32>,                   // GELF "line".
    pub additional_fields: HashMap<String, serde_json::Value>,
        // Anything carried as `_*` GELF fields lands here with the leading underscore stripped.
    pub trace_id: Option<String>,            // 32-char lowercase hex string on the wire (NOT numeric).
    pub span_id: Option<String>,             // 16-char lowercase hex string on the wire.
    pub matched_filters: Vec<String>,        // Names of filters this entry matched.
    pub source: LogSource,
}
```

### Spans and traces

```rust
pub enum SpanKind {
    Unspecified, Internal, Server, Client, Producer, Consumer
}
// Wire format: snake_case ("unspecified", "server", ...).

pub enum SpanStatus { Unset, Ok, Error(String) }
// Tagged enum on the wire: {"type":"unset"} | {"type":"ok"} | {"type":"error","message":"..."}.
// Distinct from the SDK builder's payload-free `FilterSpanStatus`.

pub struct SpanEvent {
    pub name: String,
    pub timestamp: chrono::DateTime<Utc>,    // ISO 8601.
    pub attributes: HashMap<String, serde_json::Value>,
}

pub struct SpanEntry {
    pub seq: u64,                            // Shared seq counter with logs.
    pub trace_id: String,                    // 32-char lowercase hex.
    pub span_id: String,                     // 16-char lowercase hex.
    pub parent_span_id: Option<String>,      // 16-char lowercase hex; None for root.
    pub start_time: chrono::DateTime<Utc>,   // ISO 8601.
    pub end_time: chrono::DateTime<Utc>,     // ISO 8601.
    pub duration_ms: f64,
    pub name: String,                        // OTel span name.
    pub kind: SpanKind,
    pub service_name: String,                // OTel service.name resource attribute.
    pub status: SpanStatus,
    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<SpanEvent>,
}

pub struct TraceSummary {
    pub trace_id: String,                    // 32-char lowercase hex.
    pub root_span_name: String,
    pub service_name: String,                // Of the root span.
    pub start_time: chrono::DateTime<Utc>,   // ISO 8601.
    pub total_duration_ms: f64,
    pub span_count: u32,
    pub has_errors: bool,                    // True if any span in the trace has SpanStatus::Error.
    pub linked_log_count: u32,               // Logs the broker has correlated to this trace.
}

pub struct TraceSummaryBreakdownEntry {
    pub name: String,                        // Direct child of the root span.
    pub self_time_ms: f64,                   // Time spent in this child only.
    pub total_time_ms: f64,                  // Including descendants.
    pub percentage: f64,                     // Of trace's wall-clock.
    pub is_error: bool,
}

pub struct TracesSlowGroup {
    pub name: String,                        // Span name (the grouping key).
    pub avg_ms: f64,
    pub p95_ms: f64,
    pub count: usize,
}
```

### Bookmarks (post-cursor design)

```rust
pub struct BookmarkInfo {
    pub qualified_name: String,              // "session/bookmark-name". Bare name recoverable by splitting on the last '/'.
    pub seq: u64,                            // Position in the broker's seq stream.
    pub created_at: chrono::DateTime<Utc>,   // ISO 8601; informational, not used for filtering.
    pub description: Option<String>,
}
```

See "Bookmarks and cursors" below for how to use the same `BookmarkInfo` with the `b>=` (pure read) and `c>=` (read-and-advance) DSL operators.

### Filters and triggers

```rust
pub struct FilterInfo {
    pub id: u32,                             // Per-session, monotonic.
    pub filter: String,                      // The DSL string as registered.
    pub description: Option<String>,
}

pub struct TriggerInfo {
    pub id: u32,                             // Per-session, monotonic.
    pub filter: String,                      // The DSL string as registered.
    pub pre_window: u32,                     // Pre-trigger context capacity.
    pub post_window: u32,                    // Post-trigger capture window.
    pub notify_context: u32,                 // How many pre-window entries to include in the notification.
    pub description: Option<String>,
    pub match_count: u64,                    // Lifetime fire count.
    pub oneshot: bool,                       // Auto-removes after the first match if true.
}
```

### Sessions and status

```rust
pub struct SessionInfo {
    pub id: String,                          // UUID for anonymous, name for named sessions.
    pub name: Option<String>,                // None for anonymous sessions.
    pub connected: bool,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub queue_size: usize,                   // Pending notifications buffered while disconnected.
    pub last_seen_secs_ago: u64,
    pub client_info: Option<serde_json::Value>,
        // Caller-supplied identity blob from the most recent session.start.
}

pub struct StoreStats {
    pub total_received: u64,                 // Lifetime ingest count.
    pub total_stored: u64,                   // After filter screening.
    pub malformed_count: u64,                // Receiver-level parse failures.
    pub current_size: usize,                 // Records currently in the ring buffer.
}
```

### Cross-language note

Non-Rust consumers can codegen these same shapes from `crates/protocol/protocol-v1.schema.json` (JSON Schema 2020-12). Every type listed above appears there under `definitions/`. The schema is drift-guarded by `cargo xtask verify-schema`, so it's safe to treat it as the authoritative wire contract — when the schema and the Rust structs disagree, CI fails before a release ships.

---

## Notifications

The broker pushes notifications on JSON-RPC notification frames. The SDK converts them to a typed `Notification` enum and broadcasts them on a `tokio::sync::broadcast` channel:

```rust
pub enum Notification {
    TriggerFired(TriggerFiredPayload),
    Reconnected,
    // #[non_exhaustive]: future variants ship without major-version bump
}
```

Subscribe:

```rust
let mut sub = broker.subscribe_notifications();  // broadcast::Receiver<Notification>

loop {
    match sub.recv().await {
        Ok(Notification::TriggerFired(payload)) => { /* ... */ }
        Ok(Notification::Reconnected) => { /* re-prime any per-connection state */ }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            // We dropped n notifications. Decide whether to refetch state or skip.
            tracing::warn!("notification subscriber lagged by {n}");
        }
        Err(broadcast::error::RecvError::Closed) => break,  // broker dropped
    }
}
```

### TriggerFiredPayload

```rust
pub struct TriggerFiredPayload {
    pub trigger_id: u32,                     // Matches the id returned by triggers_add.
    pub description: Option<String>,         // Mirror of TriggerInfo.description.
    pub filter_string: String,               // The DSL string of the firing trigger.
    pub pre_window: u32,                     // Pre-trigger context capacity (see TriggerInfo).
    pub post_window: u32,                    // Post-trigger capture window.
    pub notify_context: u32,                 // Cap on context_before length.
    pub oneshot: bool,                       // True iff the trigger auto-removed itself on this fire.
    pub matched_entry: LogEntry,             // The log line that matched.
    pub context_before: Vec<LogEntry>,       // Up to notify_context entries that arrived before the match.
}
```

`Reconnected` is emitted *after* a successful handshake but *before* the new bridge processes any daemon-drained queued notifications, so subscribers see `Reconnected` first and any drained `TriggerFired` events second.

Subscribers each get their own `Receiver`. Multiple subscribers see the same events.

---

## Filter builder

The broker's filter DSL is a comma-separated list of qualifiers (AND semantics within a filter). The SDK builder constructs valid filter strings without manual quoting / escaping:

```rust
use logmon_broker_sdk::{Filter, Level, FilterSpanStatus, FilterSpanKind};

// l>=ERROR, fa=mqtt, m=disconnect
let f = Filter::builder()
    .level_at_least(Level::Error)
    .facility("mqtt")
    .message("disconnect")
    .build();
```

### Selector method index

| Builder method | DSL emitted |
|---|---|
| `match_all()` / `match_none()` | `ALL` / `NONE` |
| `level_at_least(L)` / `level_at_most(L)` / `level_eq(L)` | `l>=L` / `l<=L` / `l=L` |
| `pattern(s)` / `pattern_regex(r, ci)` | bare substring or `/r/` (case-insens with `/i` suffix) |
| `message(s)` / `message_regex(r, ci)` | `m=...` |
| `full_message(s)` / `full_message_regex(r, ci)` | `fm=...` |
| `message_or_full(s)` / `message_or_full_regex(r, ci)` | `mfm=...` |
| `host(s)` / `host_regex(r, ci)` | `h=...` |
| `facility(s)` / `facility_regex(r, ci)` | `fa=...` |
| `file(s)` / `file_regex(r, ci)` | `fi=...` |
| `line(n)` | `ln=N` |
| `span_name(s)` / `span_name_regex(r, ci)` | `sn=...` |
| `service(s)` / `service_regex(r, ci)` | `sv=...` |
| `span_status(FilterSpanStatus)` | `st=ok\|error\|unset` |
| `span_kind(FilterSpanKind)` | `sk=server\|client\|producer\|consumer\|internal` |
| `duration_at_least_ms(n)` / `duration_at_most_ms(n)` | `d>=N` / `d<=N` |
| `bookmark_after(name)` / `bookmark_before(name)` | `b>=name` / `b<=name` |
| `cursor(name)` | `c>=name` (read-and-advance — see "Cursors" below) |
| `additional_field(name, value)` / `additional_field_regex(name, r, ci)` | `name=...` (custom GELF fields) |

`Level` covers ERROR/WARN/INFO/DEBUG/TRACE. `FilterSpanStatus` and `FilterSpanKind` are payload-free enums distinct from `protocol::SpanStatus` / `protocol::SpanKind`, which carry payloads — these names are intentionally `Filter`-prefixed to avoid import shadowing.

Quoting (commas, equals, double-quote inside values) is handled by `esc()` automatically; pass values verbatim.

---

## Bookmarks and cursors

Bookmarks are named seq positions in the broker's record stream. Two interaction patterns share the same storage:

### Bookmark — pure read

Mark a position; read records strictly after it. Bookmark never moves on its own.

```rust
broker.bookmarks_add(BookmarksAdd {
    name: "before-deploy".into(),
    description: Some("baseline before rollout".into()),
    ..Default::default()  // start_seq defaults to current; replace defaults to false
}).await?;

// later — get logs that arrived after the bookmark
let result = broker.logs_recent(LogsRecent {
    filter: Some(Filter::builder().bookmark_after("before-deploy").build()),  // "b>=before-deploy"
    count: Some(1000),
    ..Default::default()
}).await?;
```

### Cursor — read-and-advance

Use a bookmark via the `c>=` qualifier to read AND atomically advance the bookmark to the max seq returned. The same bookmark can be referenced with either operator — `b>=` is pure read, `c>=` reads+advances. The bookmark itself has no "this is a cursor" flag.

```rust
// First call — auto-creates the bookmark at seq=0 if it doesn't exist,
// returns everything currently in the buffer matching the filter,
// advances the bookmark to max(returned.seq).
let r1 = broker.logs_recent(LogsRecent {
    filter: Some(Filter::builder().cursor("test-run-abc").build()),  // "c>=test-run-abc"
    count: Some(100),
    ..Default::default()
}).await?;
println!("got {} records, cursor at {:?}", r1.logs.len(), r1.cursor_advanced_to);

// Subsequent call — returns only records with seq > previous max
let r2 = broker.logs_recent(LogsRecent {
    filter: Some(Filter::builder().cursor("test-run-abc").build()),
    count: Some(100),
    ..Default::default()
}).await?;
println!("got {} new records, cursor at {:?}", r2.logs.len(), r2.cursor_advanced_to);
```

#### Result ordering with cursors

When a `c>=` qualifier is present in the filter, `logs.recent`/`logs.export`/`traces.logs` return **oldest-first within the cursor's window**, so paginated polls drain the buffer monotonically. Without `c>=` they return newest-first as today. Combine with `count` to page through a large delta:

```rust
loop {
    let r = broker.logs_recent(LogsRecent {
        filter: Some(Filter::builder().cursor("drain").build()),
        count: Some(500),
        ..Default::default()
    }).await?;
    if r.logs.is_empty() { break; }
    process(&r.logs);
}
```

#### Where `c>=` is permitted

Allowed in: `logs_recent`, `logs_export`, `traces_logs`. Rejected in `logs_context`, `traces_recent`, `traces_summary`, `traces_slow`, `traces_get`, `spans_context` (their results are anchored or aggregated, not seq-streamable). Also rejected in `filters_add` and `triggers_add` — cursor positions don't make sense in long-lived registered filters.

#### `cursor_advanced_to` field

Cursor-permitted result types include `cursor_advanced_to: Option<u64>`:

- `Some(seq)` if the filter contained `c>=` AND at least one record matched (cursor advanced to `seq`).
- `None` if the filter had no `c>=`, or `c>=` matched zero records (cursor unchanged).

To inspect a cursor's current seq without advancing it, call `bookmarks_list()` and find the entry by name.

#### Initial position

| Creation path | Default `seq` | First read returns |
|---|---|---|
| `bookmarks_add(name)` (no `start_seq`) | current seq counter | only records arriving after this call |
| Implicit `c>=name` on missing entry | 0 | all records currently in the buffer + everything after |

To get "stream from now" via the implicit path, call `bookmarks_add(name)` first; the subsequent `c>=name` finds the bookmark already at current-seq and behaves accordingly.

#### Cross-session

Pure-read across sessions is allowed:

```rust
Filter::builder().bookmark_after("other-session/before-deploy").build()  // "b>=other-session/before-deploy"
```

Cross-session **advance** is rejected at the broker — only the owning session can move its own cursor. The SDK builder does not expose a cross-session cursor method to prevent the footgun.

#### Eviction

A bookmark evicts when its `seq` is older than both stores' oldest seq (high-churn workload outpaces an idle cursor). The next `c>=name` reference auto-recreates the entry at seq=0 — the next read returns the entire current buffer rather than a delta. The broker logs at WARN when this happens; bump `buffer_size` to avoid it under known-high-churn workloads.

---

## Errors

```rust
pub enum BrokerError {
    Transport(io::Error),       // connect / write / read I/O failure
    Protocol(String),           // parse error or schema mismatch on the wire
    Method { code: i32, message: String },  // RPC-level error from the broker
    Disconnected,               // bridge dropped + reconnect exhausted attempts
    SessionLost,                // session can't be resumed (anonymous, or daemon lost state)
    // #[non_exhaustive]
}
```

Pattern-match on the variant; in particular, `SessionLost` is terminal (no retry will help — re-`connect()` with a fresh handle).

---

## Test-support harness

Integration tests against a real broker live in `crates/core` under the `test-support` feature. The harness spins up an in-process daemon on a tempdir socket, lets you inject synthetic logs, and exposes a low-level `TestClient`:

```toml
# Cargo.toml of the consuming test crate
[dev-dependencies]
logmon-broker-core = { path = "...", features = ["test-support"] }
logmon-broker-sdk = { path = "..." }
```

```rust
#[tokio::test]
async fn my_test() {
    use logmon_broker_core::test_support::spawn_test_daemon;
    use logmon_broker_core::gelf::message::Level;

    let daemon = spawn_test_daemon().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open().await.unwrap();

    daemon.inject_log(Level::Error, "synthetic failure").await;
    // ...
}
```

The harness handles process lifetime, shutdown, and per-test isolation. See `crates/core/tests/harness_smoke.rs` for the canonical smoke test.

---

## Cross-language clients

The wire protocol is documented in `crates/protocol/protocol-v1.schema.json` (JSON Schema 2020-12). Cross-language clients can codegen from it. The schema is drift-guarded: `cargo xtask verify-schema` fails CI if the committed schema doesn't match what the typed Rust structs would generate.

The protocol is JSON-RPC 2.0 over a Unix domain socket (or TCP `127.0.0.1:12200` on Windows), newline-delimited messages, no length prefix. The first message must be `session.start` with `protocol_version: 1`.

---

## Versioning

`PROTOCOL_VERSION = 1`. Future protocol versions will use additive-field discipline (no field removals at the wire level except for one-time cleanups during major surface changes). The cursor mechanism's removal of `BookmarkInfo.timestamp` is one such one-time cleanup tied to introducing seq-based positions.

The SDK is versioned with the broker — they ship together. Cross-version SDK ↔ broker compatibility within the same major is best-effort but not guaranteed; in practice both are pinned in the same workspace.
