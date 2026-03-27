# OTLP Receiver + Distributed Trace Support

**Date:** 2026-03-28
**Status:** Draft
**Depends on:** Multi-session architecture (2026-03-27)

## Goal

Add OpenTelemetry Protocol (OTLP) support to logmon-mcp, enabling ingestion of structured logs and distributed traces. This transforms logmon from a log viewer into a full observability companion — the AI can see not just *what happened* (logs) but *how requests flowed* (traces) and *where time was spent* (span durations).

## Motivation

GELF provides logs. OTLP provides logs + traces + metrics from a single SDK, available in every major language. For developers starting new projects, OpenTelemetry is the recommended telemetry stack — one instrumentation, maximum observability.

The key value of traces: when an error or slowdown occurs, the AI can show the full request journey — which function called which, how long each step took, and exactly where things went wrong. This replaces hours of log-grepping with a structured, timed call tree.

## Architecture Overview

```
Applications
├── GELF (UDP/TCP :12201)  ──→ GelfReceiver ──→ log channel ──→ Log Processor
│                                                                    │
└── OTLP (gRPC :4317       ──→ OtlpReceiver ─┬→ log channel ──→ Log Processor
          HTTP/JSON :4318)                    │                      │
                                              └→ span channel ──→ Span Processor
                                                                     │
                                                                     v
                                              ┌─────────────────────────────────┐
                                              │         Daemon State            │
                                              │  LogPipeline    SpanStore       │
                                              │  (existing)     (new)           │
                                              │       └── linked via trace_id ──┘
                                              └─────────────────────────────────┘
```

Both receivers feed logs into the existing pipeline (triggers, filters, pre-buffer — all unchanged). The OTLP receiver additionally feeds spans into a new SpanStore. Logs and spans are linked by trace_id.

## Data Model

### SpanEntry

```rust
pub struct SpanEntry {
    pub seq: u64,                          // shared global counter with logs
    pub trace_id: u128,                    // 128-bit, stored as integer, displayed as 32-char hex
    pub span_id: u64,                      // 64-bit, stored as integer, displayed as 16-char hex
    pub parent_span_id: Option<u64>,       // None = root span

    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: f64,                  // precomputed: end - start

    pub name: String,                      // operation name ("query_database", "HTTP POST /api/presets")
    pub kind: SpanKind,                    // Server, Client, Internal, Producer, Consumer
    pub service_name: String,              // from OTLP resource attributes "service.name"

    pub status: SpanStatus,                // Unset, Ok, Error(message)

    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<SpanEvent>,            // exceptions, annotations within the span
}

pub enum SpanKind {
    Unspecified,   // OpenTelemetry default when kind is not set
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
}

pub struct SpanEvent {
    pub name: String,                      // e.g. "exception"
    pub timestamp: DateTime<Utc>,
    pub attributes: HashMap<String, serde_json::Value>,
}
```

### TraceSummary

Compact representation for listing traces:

```rust
pub struct TraceSummary {
    pub trace_id: u128,
    pub root_span_name: String,            // e.g. "POST /api/presets/activate"
    pub service_name: String,
    pub start_time: DateTime<Utc>,
    pub total_duration_ms: f64,            // root span duration
    pub span_count: u32,
    pub has_errors: bool,
    pub linked_log_count: u32,             // logs with this trace_id in LogStore
}
```

For traces with no spans (logs-only, from GELF with trace context), `root_span_name` is `"[logs only]"` and `total_duration_ms` is derived from the time range of linked logs: `max(timestamp) - min(timestamp)`. If only one log exists for the trace, duration is 0.

### LogEntry Changes

Add trace context fields to the existing LogEntry:

```rust
pub struct LogEntry {
    // ... all existing fields unchanged ...

    pub trace_id: Option<u128>,            // None for logs without trace context
    pub span_id: Option<u64>,             // None for logs without trace context
}
```

**Extraction rules:**
- **GELF logs:** if `_trace_id` exists in additional_fields, parse as hex → u128, remove from additional_fields. Same for `_span_id` → u64.
- **OTLP logs:** trace_id and span_id come natively as byte arrays, convert to u128/u64.
- **No trace context:** both fields are None. The log works exactly as before.

## SpanStore

Ring buffer with trace_id index, parallel to the existing LogStore.

```
SpanStore
├── Ring buffer: VecDeque<SpanEntry> (evicts oldest when full)
├── Index: HashMap<u128, Vec<u64>> (trace_id → vec of seq numbers)
├── On insert: add to buffer + update index
├── On eviction: remove from index
├── Capacity: configurable, default 10,000 spans
```

### Access Methods

| Method | Purpose |
| ------ | ------- |
| `insert(span)` | Add span, assign seq, update trace_id index |
| `get_trace(trace_id) -> Vec<SpanEntry>` | All spans for a trace, ordered by start_time |
| `recent_traces(count, filter) -> Vec<TraceSummary>` | Recent unique traces, optionally filtered |
| `slow_spans(min_duration_ms, count, filter) -> Vec<SpanEntry>` | Spans exceeding threshold, sorted by duration desc |
| `context_by_seq(seq, before, after) -> Vec<SpanEntry>` | Spans surrounding a given seq |
| `stats() -> SpanStoreStats` | Total stored, total traces, avg duration |

### LogStore Additions

New index and methods for trace linking:

```rust
// New index: HashMap<u128, Vec<u64>> (trace_id → seq numbers)
// Updated on insert/eviction, same pattern as SpanStore

pub fn logs_by_trace_id(&self, trace_id: u128) -> Vec<&LogEntry>
pub fn count_by_trace_id(&self, trace_id: u128) -> usize
```

## OTLP Receiver

### Transports

| Transport | Port | Protocol |
| --------- | ---- | -------- |
| gRPC | 4317 (default) | Protobuf over HTTP/2 |
| HTTP/JSON | 4318 (default) | JSON over HTTP/1.1 |

Both implement the same OTLP endpoints:
- **LogsService / POST /v1/logs** — receives `ExportLogsServiceRequest`
- **TraceService / POST /v1/traces** — receives `ExportTraceServiceRequest`

### Dependencies

- `tonic` — gRPC server
- `opentelemetry-proto` — published crate with Rust types for OTLP protobuf definitions
- `axum` — HTTP server for the JSON endpoint

### OTLP Log Record → LogEntry Mapping

| OTLP field | LogEntry field |
| ---------- | -------------- |
| `body` (string) | `message` |
| `body` (if long/structured) | `full_message` |
| `severity_number` | `level` (same syslog-style mapping) |
| `time_unix_nano` | `timestamp` |
| `trace_id` (bytes) | `trace_id` (u128) |
| `span_id` (bytes) | `span_id` (u64) |
| `resource.attributes["service.name"]` | `facility` (same semantic: where the log came from) |
| `resource.attributes["host.name"]` | `host` |
| `attributes` (all others) | `additional_fields` |

**Default values:**
- If `resource.attributes["service.name"]` is missing: `facility` = `"unknown"`
- If `resource.attributes["host.name"]` is missing: `host` = `"unknown"`
- If `trace_id` or `span_id` is empty/zero: set to None (log is not part of a trace)
- If `severity_number` is missing: default to Info

### OTLP Error Handling

Malformed OTLP data is handled the same way as malformed GELF:
- Missing required fields: increment malformed counter, skip the entry
- For log records: `body` is required. Everything else has defaults.
- For spans: `trace_id`, `span_id`, and `name` are required. Missing any → skip and count as malformed.
- Partially valid batches: process valid entries, skip malformed ones. Return success to the OTLP exporter (standard OTLP behavior — exporters don't retry on partial failure).

### OTLP Span → SpanEntry Mapping

| OTLP field | SpanEntry field |
| ---------- | --------------- |
| `trace_id` (bytes) | `trace_id` (u128) |
| `span_id` (bytes) | `span_id` (u64) |
| `parent_span_id` (bytes) | `parent_span_id` (Option<u64>) |
| `name` | `name` |
| `kind` | `kind` (SpanKind) |
| `start_time_unix_nano` | `start_time` |
| `end_time_unix_nano` | `end_time` |
| computed | `duration_ms` |
| `status.code` + `status.message` | `status` (SpanStatus) |
| `resource.attributes["service.name"]` | `service_name` |
| `attributes` | `attributes` |
| `events` | `events` (Vec<SpanEvent>) |

**Default values for spans:**
- If `resource.attributes["service.name"]` is missing: `service_name` = `"unknown"`
- If `status` is not set: `SpanStatus::Unset`
- If `kind` is not set: `SpanKind::Unspecified`

### Receiver Structure

```rust
pub struct OtlpReceiver {
    grpc_handle: JoinHandle<()>,           // tonic server
    http_handle: JoinHandle<()>,           // axum server
    shutdown_tx: broadcast::Sender<()>,    // signal both to stop
}

impl OtlpReceiver {
    pub async fn start(
        config: OtlpReceiverConfig,
        log_sender: mpsc::Sender<LogEntry>,
        span_sender: mpsc::Sender<SpanEntry>,
    ) -> anyhow::Result<Self>
}
```

Implements a generalized `Receiver` trait (rename the current `LogReceiver` trait since receivers now handle more than logs). The trait interface is unchanged — `name()`, `listening_on()`, `shutdown()` — only the name changes:
- `name()` → `"otlp"`
- `listening_on()` → `["gRPC:4317", "HTTP:4318"]`
- `shutdown()` → signals both servers to stop

## Log-to-Trace Linking

The link between logs and spans is the `trace_id` field. No explicit linking step — both stores independently index by trace_id, and cross-store queries happen at query time.

### Cross-Store Queries

| Query | Implementation |
| ----- | -------------- |
| "Logs for this trace" | `log_store.logs_by_trace_id(trace_id)` |
| "Trace for this log" | Read `log.trace_id`, then `span_store.get_trace(trace_id)` |
| "Linked log count" | `log_store.count_by_trace_id(trace_id)` |

### Edge Cases

- **Spans arrive before/after their logs:** No problem. Stores are independent. Query returns whatever has arrived.
- **Log has trace_id but no spans exist:** Common with GELF-only setups. Tools return logs grouped by trace_id with "no spans found." Still useful.
- **Spans evicted but logs remain (or vice versa):** Partial view is fine. No error.

### Span Storage Rules

Spans are stored unconditionally — session filters do NOT affect span storage. Rationale: spans are relatively few compared to logs (one span per operation vs potentially many log lines per operation), and filtering spans would break trace completeness. A trace with missing spans is misleading.

Session triggers DO evaluate against spans (if they contain span selectors). But trigger evaluation is for notification, not storage gating.

## Span Filter DSL

Extends the existing filter DSL with span-specific selectors. Same syntax, same parser, new matchers.

### Span Selectors

| Selector | Field | Example |
| -------- | ----- | ------- |
| `sn` | span name | `sn=query_database` |
| `sv` | service name | `sv=store_server` |
| `st` | status | `st=error` |
| `sk` | span kind | `sk=server` |
| `d>=` / `d<=` | duration (ms) | `d>=100` |
| bare text | substring match against span name | `query` |
| `/regex/` | regex against span name | `/copy_from\|query/` |
| `key=value` | attribute match | `http.method=POST` |

**Bare text and regex behavior:** When no selector is specified, bare text and `/regex/` match against the span `name` field only (not all attributes). This differs from log matching where bare patterns match against all fields — span attributes are key-value pairs that don't lend themselves to global text search. Use `key=value` syntax for attribute matching.

### Auto-Detection

When `add_trigger` is called with a filter containing span selectors (`sn`, `sv`, `sk`, `d>=`, `d<=`), the system detects it as a span trigger and evaluates it against spans in the span processor. Filters containing only log selectors (`m`, `fm`, `fa`, `l>=`, etc.) evaluate against logs as before.

If a filter mixes log and span selectors, it's a span trigger (span selectors are the more specific context).

### Matcher Function

```rust
pub fn matches_span(filter: &ParsedFilter, span: &SpanEntry) -> bool
```

Parallel to the existing `matches_entry(filter, entry) -> bool` for logs.

## Span Triggers

Span-based triggers evaluate in the span processor, mirroring how log triggers work in the log processor. They fire when a span matches the trigger's filter.

### How They Work

1. Span arrives in span processor
2. For each session, evaluate span triggers (triggers with span selectors)
3. On match: build notification with span context and send/queue

### Notification Content

When a span trigger fires, the notification includes:
- The matched span (name, duration, status, service)
- The trace summary (root span name, total duration, span count)
- Linked logs from the same trace_id (if any)

This is richer than log trigger notifications because the AI gets the full request context immediately.

### No Pre/Post Window for Spans

Span triggers don't need pre/post windows. Spans are self-contained with timing — there's no equivalent of "capture logs before the error." The trace itself provides all the context. Span triggers fire immediately on match and include the trace summary.

Log triggers and span triggers are completely independent:
- A log post-trigger window does NOT suppress span trigger evaluation
- A span trigger firing does NOT affect log trigger evaluation
- They run in separate processors on separate channels

### Span Trigger Notification Schema

```json
{
  "type": "span_trigger",
  "trigger_id": 3,
  "trigger_description": "Slow database queries",
  "filter_string": "sn=query_database,d>=500",
  "matched_span": {
    "name": "query_database",
    "duration_ms": 612.5,
    "status": "ok",
    "service_name": "store_server",
    "trace_id": "4bf92f3577b16e0f0000000000000001",
    "span_id": "00f067aa0ba902b7"
  },
  "trace_summary": {
    "root_span_name": "POST /api/presets/activate",
    "total_duration_ms": 720.0,
    "span_count": 5,
    "has_errors": false
  },
  "linked_log_count": 3
}
```

The notification does NOT include full linked logs inline (they could be large). The AI can call `get_trace_logs` if it needs them.

### Span Tree Reconstruction

SpanStore returns spans as a flat list sorted by start_time. Tree reconstruction (indenting by parent-child relationships) is done by the RPC handler when formatting `get_trace` responses. Algorithm: build a HashMap<span_id, Vec<children>>, then DFS from root spans (parent_span_id = None).

### Configuration

Same `add_trigger` tool. The filter determines whether it's a log trigger or span trigger:

```
add_trigger(filter="l>=ERROR", ...)           → log trigger (existing)
add_trigger(filter="d>=500", ...)             → span trigger (new)
add_trigger(filter="sv=store_server,d>=500")  → span trigger (new)
```

## New MCP Tools

### Trace Query Tools

#### `get_recent_traces`

List recent traces, newest first.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `count` | u32 | 20 | Max traces to return |
| `filter` | String? | None | Span filter DSL expression |

Returns list of TraceSummary. Traces with only logs (no spans) are included, marked as `[logs only]`.

#### `get_trace`

Full trace detail — span tree + linked logs.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `trace_id` | String | required | 32-char hex trace ID |
| `include_logs` | bool | true | Include linked logs in output |
| `filter` | String? | None | Filter spans within the trace |

Returns span tree (indented by parent-child, sorted by start_time) with linked logs interleaved at their timestamp positions. When filter is set, non-matching spans are omitted but tree structure is preserved.

#### `get_trace_summary`

Compact timing breakdown highlighting bottlenecks.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `trace_id` | String | required | 32-char hex trace ID |

Returns flat breakdown:

```
POST /api/presets/activate (520ms)
  query_database       450ms  87%  <- bottleneck
  serialize_response    12ms   2%
  authenticate_user      5ms   1%
  other                 53ms  10%
```

Automatically identifies the bottleneck span. Highlights error paths.

#### `get_slow_spans`

Find performance bottlenecks.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `min_duration_ms` | f64 | 100 | Duration threshold |
| `count` | u32 | 20 | Max results |
| `filter` | String? | None | Additional span filter |
| `group_by` | String? | None | `"name"` or `"service"` |

Without grouping: individual slow spans with root request context (root span name, total duration, percentage).

With `group_by="name"`:

```
Slow spans by operation (>100ms):
  query_database   avg=320ms  p95=450ms  count=12
  load_schema      avg=150ms  p95=200ms  count=3
```

#### `get_span_context`

Spans surrounding a specific span in time.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `seq` | u64 | required | Span seq number |
| `before` | u32 | 5 | Spans before |
| `after` | u32 | 5 | Spans after |

#### `get_trace_logs`

All logs linked to a trace.

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `trace_id` | String | required | 32-char hex trace ID |
| `filter` | String? | None | Additional log filter DSL |

### Enhanced Existing Tool

#### `get_recent_logs` — new optional parameter

| Parameter | Type | Default | Description |
| --------- | ---- | ------- | ----------- |
| `trace_id` | String? | None | Filter logs by trace ID |

## Trigger Notification Enhancement

When a log trigger fires on a log entry that has a `trace_id`, the notification automatically includes the trace summary (root span name, total duration, span count). The AI gets the full request context without a follow-up tool call.

## Trace-Aware Pre-Buffer Flush

When a log trigger fires and flushes the pre-buffer, also include all logs from the same trace_id as the triggering entry, even if they fall outside the pre_window count. This ensures the AI sees the full request context:

- Pre-buffer flush captures N most recent logs (existing behavior)
- Additionally, scan the store and pre-buffer for logs with the same trace_id as the triggering entry
- Deduplicate (some may already be in the pre-buffer flush)

This handles the case where a request's early logs scrolled out of the pre-window but are still in the buffer.

## Daemon Integration

### Startup Sequence

```
1. Load config, state
2. Create SeqCounter (extracted from LogPipeline)
3. Create LogPipeline, SpanStore, SessionRegistry
4. Create log channel + span channel
5. Start GelfReceiver → log channel
6. Start OtlpReceiver → log channel + span channel
7. Start log processor (unchanged)
8. Start span processor (new — insert + trigger eval)
9. Start RPC listener (extended with SpanStore + trace RPC methods)
```

### Shared Seq Counter

Extract the `AtomicU64` seq counter from LogPipeline into a standalone `SeqCounter` struct shared by both processors. Logs and spans share a single monotonic sequence — this allows interleaving them in a unified timeline.

### Span Processor

```rust
pub fn spawn_span_processor(
    mut receiver: mpsc::Receiver<SpanEntry>,
    seq_counter: Arc<SeqCounter>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut span) = receiver.recv().await {
            span.seq = seq_counter.next();
            span_store.insert(span.clone());

            // Evaluate span triggers for each session
            for sid in sessions.active_session_ids() {
                let matches = sessions.evaluate_span_triggers(&sid, &span);
                for m in matches {
                    // Build notification with trace summary
                    sessions.send_or_queue_span_notification(&sid, &span, &m);
                }
            }
        }
    })
}
```

### New RPC Methods

| RPC method | Handler |
| ---------- | ------- |
| `traces.recent` | span_store.recent_traces() |
| `traces.get` | span_store.get_trace() + log_store.logs_by_trace_id() |
| `traces.summary` | span_store.get_trace() → compute breakdown |
| `traces.slow` | span_store.slow_spans() |
| `traces.logs` | log_store.logs_by_trace_id() |
| `spans.context` | span_store.context_by_seq() |

### What Stays Unchanged

- Log processing pipeline (filters, triggers, pre-buffer, post-window)
- Session registry structure (span triggers are just triggers with span selectors)
- Existing MCP tools (all work exactly as before)
- GELF receiver (no modifications)
- Shim/bridge architecture (new RPC methods are transparent)

## Configuration

### Daemon Config (`~/.config/logmon/config.json`)

```json
{
  "gelf_port": 12201,
  "gelf_udp_port": null,
  "gelf_tcp_port": null,
  "otlp_grpc_port": 4317,
  "otlp_http_port": 4318,
  "buffer_size": 10000,
  "span_buffer_size": 10000,
  "idle_timeout_secs": 1800
}
```

All OTLP fields have defaults. No configuration needed to get started.

### CLI

```bash
logmon-mcp-server daemon \
  --gelf-port 12201 \
  --otlp-grpc-port 4317 \
  --otlp-http-port 4318 \
  --span-buffer-size 10000
```

### Disabling Receivers

Set port to 0 to disable:

```bash
logmon-mcp-server daemon --gelf-port 0            # OTLP only
logmon-mcp-server daemon --otlp-grpc-port 0 \
                         --otlp-http-port 0       # GELF only
```

## Skill Updates

### New `/logmon` Commands

- `/logmon traces` — call `get_recent_traces` and summarize
- `/logmon slow` — call `get_slow_spans` and summarize
- `/logmon trace <trace_id>` — call `get_trace` for a specific trace

### Workflow Tips

Add to skill file:

- "When investigating slowness, start with `get_slow_spans` to find bottlenecks, then `get_trace_summary` to see the full request breakdown."
- "When investigating an error that has a trace_id, use `get_trace` to see the full request journey that led to the error."
- "To compare a slow trace with a normal one, use `get_recent_traces` to find a fast trace for the same endpoint, then compare both with `get_trace_summary`."

## Prerequisite: tracing_init GELF Enhancement

The `tracing_init` crate (`/Users/yuval/Documents/Projects/Store/tracing_init/`) currently does not emit trace context in GELF messages. To enable log-to-trace linking for GELF-based applications:

**Task:** Enhance the GelfLayer in `tracing_init/src/gelf.rs` to extract OpenTelemetry trace context from the current span and include `_trace_id` (32-char hex) and `_span_id` (16-char hex) in GELF additional fields.

This requires the application to have a `tracing-opentelemetry` layer installed alongside the GELF layer. If no OTel context is present, the fields are simply omitted.

## Out of Scope

The following are deferred to future specs:

- **Derived metrics and rate-based triggers** — computed error rates, log volume trends, rate-threshold triggers
- **OTLP metrics ingestion** — ingesting counter/gauge/histogram data
- **Service map** — auto-discovering service topology from cross-service spans
- **Live trace streaming** — watching traces assemble in real-time
- **Persistent storage** — SQLite backend for logs or spans
- **Additional receivers** — syslog, journald, stdout/JSON, file tailing
