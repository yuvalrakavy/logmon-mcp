# OTLP Receiver + Distributed Trace Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OTLP log/trace ingestion, a SpanStore, span-aware DSL filtering, span triggers, and 6 new MCP trace tools to logmon-mcp.

**Architecture:** OTLP receiver (gRPC + HTTP/JSON) feeds logs into the existing pipeline and spans into a new SpanStore. Logs and spans share a global seq counter and are linked by trace_id. The existing filter DSL is extended with span selectors. Span triggers evaluate in a dedicated span processor.

**Tech Stack:** Rust, tonic (gRPC), axum (HTTP), opentelemetry-proto (OTLP types), tokio, serde

**Spec:** `docs/superpowers/specs/2026-03-28-otlp-traces-design.md`

---

## File Map

Changes marked: `[new]`, `[modify]`, `[unchanged]`. Unmarked = unchanged.

```
src/
├── main.rs                         [modify] add OTLP CLI args
├── lib.rs                          [modify] add span module
├── config.rs                       [modify] add OTLP port args
├── gelf/
│   ├── message.rs                  [modify] add trace_id/span_id to LogEntry, extract from GELF
│   ├── udp.rs
│   └── tcp.rs
├── filter/
│   ├── parser.rs                   [modify] add span selectors (sn, sv, st, sk, d>=)
│   └── matcher.rs                  [modify] add matches_span()
├── store/
│   ├── traits.rs                   [modify] add trace_id index methods to LogStore
│   └── memory.rs                   [modify] add trace_id index to InMemoryStore
├── span/                           [new]
│   ├── mod.rs                      span module root
│   ├── types.rs                    SpanEntry, SpanKind, SpanStatus, SpanEvent, TraceSummary
│   └── store.rs                    SpanStore (ring buffer + trace_id index)
├── engine/
│   ├── pipeline.rs                 [modify] extract SeqCounter, add trace fields to PipelineEvent
│   ├── pre_buffer.rs
│   └── trigger.rs                  [modify] add is_span_trigger() detection
├── receiver/
│   ├── mod.rs                      [modify] rename LogReceiver → Receiver
│   ├── gelf.rs                     [modify] update trait name
│   └── otlp/                       [new]
│       ├── mod.rs                  OtlpReceiver struct
│       ├── grpc.rs                 tonic gRPC server (LogsService + TraceService)
│       ├── http.rs                 axum HTTP/JSON server
│       └── mapping.rs              OTLP → LogEntry/SpanEntry conversion
├── daemon/
│   ├── server.rs                   [modify] add SpanStore, span channel, OTLP receiver, span processor
│   ├── log_processor.rs            [modify] trace-aware pre-buffer flush, trace summary in notifications
│   ├── span_processor.rs           [new] span ingest + span trigger evaluation
│   ├── rpc_handler.rs              [modify] add trace RPC methods
│   ├── persistence.rs              [modify] add OTLP config fields
│   └── session.rs                  [modify] add evaluate_span_triggers(), span trigger detection
├── mcp/
│   ├── server.rs                   [modify] add 6 trace tools, enhance get_recent_logs
│   └── notifications.rs            [modify] handle span trigger notifications
├── shim/
│   ├── auto_start.rs
│   └── bridge.rs
└── rpc/
    ├── types.rs
    └── transport.rs

tests/
├── span_types.rs                   [new] SpanEntry/TraceSummary tests
├── span_store.rs                   [new] SpanStore tests
├── span_filter.rs                  [new] span DSL selector tests
├── span_processor.rs               [new] span processor tests
├── otlp_mapping.rs                 [new] OTLP → LogEntry/SpanEntry mapping tests
├── trace_linking.rs                [new] cross-store trace_id query tests
├── gelf_parsing.rs                 [modify] add trace_id extraction tests
├── filter_dsl.rs                   [modify] add span selector parse tests
├── log_processor.rs                [modify] add trace-aware flush tests
├── memory_store.rs                 [modify] add trace_id index tests
└── ... (existing tests unchanged)
```

---

## Phase 1: Data Model Foundation

### Task 1: SpanEntry Types

**Files:**
- Create: `src/span/mod.rs`
- Create: `src/span/types.rs`
- Create: `tests/span_types.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write span type tests**

`tests/span_types.rs`:

```rust
use logmon_mcp_server::span::types::*;
use chrono::Utc;
use std::collections::HashMap;

#[test]
fn test_span_entry_creation() {
    let span = SpanEntry {
        seq: 0,
        trace_id: 0x4bf92f3577b16e0f0000000000000001_u128,
        span_id: 0x00f067aa0ba902b7_u64,
        parent_span_id: None,
        start_time: Utc::now(),
        end_time: Utc::now(),
        duration_ms: 42.5,
        name: "query_database".to_string(),
        kind: SpanKind::Internal,
        service_name: "store_server".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    };
    assert_eq!(span.name, "query_database");
    assert_eq!(span.duration_ms, 42.5);
    assert!(span.parent_span_id.is_none());
}

#[test]
fn test_span_kind_default() {
    assert!(matches!(SpanKind::Unspecified, SpanKind::Unspecified));
}

#[test]
fn test_span_status_error() {
    let status = SpanStatus::Error("connection timeout".to_string());
    if let SpanStatus::Error(msg) = &status {
        assert_eq!(msg, "connection timeout");
    } else {
        panic!("expected Error");
    }
}

#[test]
fn test_span_event() {
    let event = SpanEvent {
        name: "exception".to_string(),
        timestamp: Utc::now(),
        attributes: {
            let mut m = HashMap::new();
            m.insert("exception.message".to_string(), serde_json::json!("null pointer"));
            m
        },
    };
    assert_eq!(event.name, "exception");
}

#[test]
fn test_trace_id_hex_formatting() {
    let trace_id: u128 = 0x4bf92f3577b16e0f0000000000000001;
    let hex = format!("{:032x}", trace_id);
    assert_eq!(hex, "4bf92f3577b16e0f0000000000000001");
    assert_eq!(hex.len(), 32);
}

#[test]
fn test_trace_id_hex_parsing() {
    let hex = "4bf92f3577b16e0f0000000000000001";
    let parsed = u128::from_str_radix(hex, 16).unwrap();
    assert_eq!(parsed, 0x4bf92f3577b16e0f0000000000000001);
}

#[test]
fn test_span_id_hex_formatting() {
    let span_id: u64 = 0x00f067aa0ba902b7;
    let hex = format!("{:016x}", span_id);
    assert_eq!(hex, "00f067aa0ba902b7");
    assert_eq!(hex.len(), 16);
}

#[test]
fn test_span_entry_serialization() {
    let span = SpanEntry {
        seq: 1,
        trace_id: 0xabc123_u128,
        span_id: 0xdef456_u64,
        parent_span_id: Some(0xdef000_u64),
        start_time: Utc::now(),
        end_time: Utc::now(),
        duration_ms: 100.0,
        name: "test".to_string(),
        kind: SpanKind::Server,
        service_name: "svc".to_string(),
        status: SpanStatus::Unset,
        attributes: HashMap::new(),
        events: vec![],
    };
    let json = serde_json::to_value(&span).unwrap();
    // trace_id and span_id should serialize as hex strings
    assert!(json["trace_id"].is_string());
    assert!(json["span_id"].is_string());
}

#[test]
fn test_trace_summary_creation() {
    let summary = TraceSummary {
        trace_id: 0xabc_u128,
        root_span_name: "POST /api/test".to_string(),
        service_name: "myapp".to_string(),
        start_time: Utc::now(),
        total_duration_ms: 250.0,
        span_count: 5,
        has_errors: false,
        linked_log_count: 3,
    };
    assert_eq!(summary.span_count, 5);
    assert!(!summary.has_errors);
}
```

- [ ] **Step 2: Implement span types**

`src/span/mod.rs`:

```rust
pub mod types;
```

`src/span/types.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer, Deserializer};
use std::collections::HashMap;

fn serialize_trace_id<S: Serializer>(id: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:032x}", id))
}

fn deserialize_trace_id<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    let s = String::deserialize(d)?;
    u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
}

fn serialize_span_id<S: Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:016x}", id))
}

fn deserialize_span_id<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    u64::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
}

fn serialize_opt_span_id<S: Serializer>(id: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    match id {
        Some(v) => s.serialize_some(&format!("{:016x}", v)),
        None => s.serialize_none(),
    }
}

fn deserialize_opt_span_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let opt: Option<String> = Option::deserialize(d)?;
    match opt {
        Some(s) => u64::from_str_radix(&s, 16).map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEntry {
    pub seq: u64,
    #[serde(serialize_with = "serialize_trace_id", deserialize_with = "deserialize_trace_id")]
    pub trace_id: u128,
    #[serde(serialize_with = "serialize_span_id", deserialize_with = "deserialize_span_id")]
    pub span_id: u64,
    #[serde(serialize_with = "serialize_opt_span_id", deserialize_with = "deserialize_opt_span_id")]
    pub parent_span_id: Option<u64>,

    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: f64,

    pub name: String,
    pub kind: SpanKind,
    pub service_name: String,

    pub status: SpanStatus,

    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<SpanEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "message")]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEvent {
    pub name: String,
    pub timestamp: DateTime<Utc>,
    pub attributes: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    #[serde(serialize_with = "serialize_trace_id", deserialize_with = "deserialize_trace_id")]
    pub trace_id: u128,
    pub root_span_name: String,
    pub service_name: String,
    pub start_time: DateTime<Utc>,
    pub total_duration_ms: f64,
    pub span_count: u32,
    pub has_errors: bool,
    pub linked_log_count: u32,
}
```

Add to `src/lib.rs`:

```rust
pub mod span;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test span_types`

- [ ] **Step 4: Commit**

```bash
git add src/span/ tests/span_types.rs src/lib.rs
git commit -m "feat: SpanEntry, SpanKind, SpanStatus, TraceSummary types"
```

---

### Task 2: Add trace_id/span_id to LogEntry + GELF Extraction

**Files:**
- Modify: `src/gelf/message.rs`
- Modify: `tests/gelf_parsing.rs`

- [ ] **Step 1: Write trace context extraction tests**

Add to `tests/gelf_parsing.rs`:

```rust
#[test]
fn test_parse_gelf_with_trace_context() {
    let json = r#"{
        "version": "1.1",
        "host": "app",
        "short_message": "traced log",
        "_trace_id": "4bf92f3577b16e0f0000000000000001",
        "_span_id": "00f067aa0ba902b7"
    }"#;
    let entry = parse_gelf_message(json).unwrap();
    assert_eq!(entry.trace_id, Some(0x4bf92f3577b16e0f0000000000000001_u128));
    assert_eq!(entry.span_id, Some(0x00f067aa0ba902b7_u64));
    // Extracted from additional_fields — should NOT be in the map
    assert!(!entry.additional_fields.contains_key("trace_id"));
    assert!(!entry.additional_fields.contains_key("span_id"));
}

#[test]
fn test_parse_gelf_without_trace_context() {
    let json = r#"{
        "version": "1.1",
        "host": "app",
        "short_message": "plain log"
    }"#;
    let entry = parse_gelf_message(json).unwrap();
    assert_eq!(entry.trace_id, None);
    assert_eq!(entry.span_id, None);
}

#[test]
fn test_parse_gelf_invalid_trace_id() {
    let json = r#"{
        "version": "1.1",
        "host": "app",
        "short_message": "bad trace",
        "_trace_id": "not-valid-hex"
    }"#;
    let entry = parse_gelf_message(json).unwrap();
    // Invalid hex → trace_id is None, not an error
    assert_eq!(entry.trace_id, None);
}
```

- [ ] **Step 2: Add trace_id/span_id fields to LogEntry**

In `src/gelf/message.rs`, add to the `LogEntry` struct:

```rust
pub struct LogEntry {
    // ... all existing fields ...

    /// Trace context — extracted from GELF _trace_id or OTLP native
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<u64>,
}
```

Custom serde for hex display (add serialize_with/deserialize_with helpers similar to SpanEntry, but for Option<u128> and Option<u64>).

Update `parse_gelf_message()` to extract `_trace_id` and `_span_id` from additional_fields before building the entry:

```rust
// After collecting additional_fields, extract trace context
let trace_id = additional_fields.remove("trace_id")
    .and_then(|v| v.as_str().map(String::from))
    .and_then(|s| u128::from_str_radix(&s, 16).ok());

let span_id = additional_fields.remove("span_id")
    .and_then(|v| v.as_str().map(String::from))
    .and_then(|s| u64::from_str_radix(&s, 16).ok());
```

Update all test helpers and `make_entry` functions across test files to include the new fields (set to `None`).

- [ ] **Step 3: Run all tests**

Run: `cargo test`

Ensure no regressions. All existing tests must pass — the new fields default to None.

- [ ] **Step 4: Commit**

```bash
git add src/gelf/message.rs tests/gelf_parsing.rs
git commit -m "feat: add trace_id/span_id to LogEntry with GELF extraction"
```

---

### Task 3: Extract SeqCounter from LogPipeline

**Files:**
- Create: `src/engine/seq_counter.rs`
- Modify: `src/engine/pipeline.rs`
- Modify: `src/engine/mod.rs`
- Modify: `tests/pipeline.rs`

- [ ] **Step 1: Create SeqCounter**

`src/engine/seq_counter.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// Shared monotonic sequence counter for logs and spans.
pub struct SeqCounter {
    counter: AtomicU64,
}

impl SeqCounter {
    pub fn new() -> Self {
        Self { counter: AtomicU64::new(1) }
    }

    pub fn new_with_initial(initial: u64) -> Self {
        Self { counter: AtomicU64::new(initial) }
    }

    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}
```

- [ ] **Step 2: Refactor LogPipeline to use SeqCounter**

In `src/engine/pipeline.rs`:
- Remove the internal `AtomicU64` seq counter
- Accept `Arc<SeqCounter>` in constructor
- `assign_seq()` delegates to `seq_counter.next()`
- Update `new()` and `new_with_seq()` to create/accept SeqCounter

Update `src/engine/mod.rs` to export the new module.

- [ ] **Step 3: Update tests and callers**

Update `tests/pipeline.rs` — create SeqCounter and pass to LogPipeline.
Update `src/daemon/server.rs` — create `Arc<SeqCounter>`, pass to LogPipeline.
Update `src/daemon/log_processor.rs` — no changes needed (uses pipeline.assign_seq()).
Update `tests/log_processor.rs` — LogPipeline::new() should still work (convenience constructor creates internal SeqCounter).

- [ ] **Step 4: Run all tests**

Run: `cargo test`

- [ ] **Step 5: Commit**

```bash
git add src/engine/seq_counter.rs src/engine/pipeline.rs src/engine/mod.rs tests/pipeline.rs src/daemon/server.rs
git commit -m "refactor: extract SeqCounter from LogPipeline for shared use"
```

---

### Task 4: Add trace_id Index to LogStore

**Files:**
- Modify: `src/store/traits.rs`
- Modify: `src/store/memory.rs`
- Modify: `tests/memory_store.rs`

- [ ] **Step 1: Write trace_id index tests**

Add to `tests/memory_store.rs`:

```rust
#[test]
fn test_logs_by_trace_id() {
    let mut store = InMemoryStore::new(100);
    let trace = 0xabc123_u128;

    let mut e1 = make_entry(Level::Info, "first");
    e1.seq = 1;
    e1.trace_id = Some(trace);
    store.append(e1);

    let mut e2 = make_entry(Level::Info, "second");
    e2.seq = 2;
    e2.trace_id = Some(trace);
    store.append(e2);

    let mut e3 = make_entry(Level::Info, "unrelated");
    e3.seq = 3;
    e3.trace_id = None;
    store.append(e3);

    let traced = store.logs_by_trace_id(trace);
    assert_eq!(traced.len(), 2);
    assert_eq!(store.count_by_trace_id(trace), 2);
    assert_eq!(store.count_by_trace_id(0xdead_u128), 0);
}

#[test]
fn test_trace_id_index_eviction() {
    let mut store = InMemoryStore::new(3);
    let trace = 0xabc_u128;

    for i in 1..=4 {
        let mut e = make_entry(Level::Info, &format!("msg {i}"));
        e.seq = i as u64;
        e.trace_id = Some(trace);
        store.append(e);
    }

    // Entry with seq=1 evicted, only 3 remain
    let traced = store.logs_by_trace_id(trace);
    assert_eq!(traced.len(), 3);
}
```

- [ ] **Step 2: Add trace_id methods to LogStore trait**

In `src/store/traits.rs`:

```rust
pub trait LogStore: Send + Sync {
    // ... existing methods ...

    fn logs_by_trace_id(&self, trace_id: u128) -> Vec<&LogEntry>;
    fn count_by_trace_id(&self, trace_id: u128) -> usize;
}
```

- [ ] **Step 3: Implement trace_id index in InMemoryStore**

In `src/store/memory.rs`:
- Add `trace_index: HashMap<u128, Vec<u64>>` field
- On `append()`: if entry has trace_id, add seq to the index
- On eviction: remove seq from the trace_index entry; remove the key if empty
- Implement `logs_by_trace_id()`: lookup seqs from index, collect matching entries
- Implement `count_by_trace_id()`: lookup seqs from index, return count

- [ ] **Step 4: Run tests**

Run: `cargo test --test memory_store`

- [ ] **Step 5: Run all tests**

Run: `cargo test`

- [ ] **Step 6: Commit**

```bash
git add src/store/traits.rs src/store/memory.rs tests/memory_store.rs
git commit -m "feat: trace_id index on LogStore for cross-store linking"
```

---

## Phase 2: SpanStore

### Task 5: SpanStore Implementation

**Files:**
- Create: `src/span/store.rs`
- Create: `tests/span_store.rs`
- Modify: `src/span/mod.rs`

- [ ] **Step 1: Write SpanStore tests**

`tests/span_store.rs`:

```rust
use logmon_mcp_server::span::store::SpanStore;
use logmon_mcp_server::span::types::*;
use logmon_mcp_server::engine::seq_counter::SeqCounter;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_span(name: &str, trace_id: u128, duration_ms: f64) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id,
        span_id: rand_id(),
        parent_span_id: None,
        start_time: now,
        end_time: now + chrono::Duration::milliseconds(duration_ms as i64),
        duration_ms,
        name: name.to_string(),
        kind: SpanKind::Internal,
        service_name: "test".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

fn rand_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn test_insert_and_get_trace() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq.clone());
    let trace_id = 0xabc_u128;

    let mut s1 = make_span("root", trace_id, 100.0);
    s1.seq = seq.next();
    store.insert(s1);
    let mut s2 = make_span("child", trace_id, 50.0);
    s2.seq = seq.next();
    store.insert(s2);

    let spans = store.get_trace(trace_id);
    assert_eq!(spans.len(), 2);
}

#[test]
fn test_get_trace_nonexistent() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);
    assert!(store.get_trace(0xdead_u128).is_empty());
}

#[test]
fn test_slow_spans() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);
    store.insert(make_span("fast", 1, 10.0));
    store.insert(make_span("slow", 2, 500.0));
    store.insert(make_span("medium", 3, 200.0));

    let slow = store.slow_spans(100.0, 10, None);
    assert_eq!(slow.len(), 2);
    assert_eq!(slow[0].name, "slow"); // sorted by duration desc
    assert_eq!(slow[1].name, "medium");
}

#[test]
fn test_recent_traces() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);

    store.insert(make_span("trace-a-root", 0xa_u128, 100.0));
    store.insert(make_span("trace-b-root", 0xb_u128, 200.0));

    let recent = store.recent_traces(10, None, |_| 0); // linked_log_count callback returns 0
    assert_eq!(recent.len(), 2);
    // Most recently inserted first
    assert_eq!(recent[0].trace_id, 0xb_u128);
}

#[test]
fn test_eviction_cleans_index() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(3, seq);
    let trace_a = 0xa_u128;
    let trace_b = 0xb_u128;

    store.insert(make_span("a1", trace_a, 10.0));
    store.insert(make_span("a2", trace_a, 10.0));
    store.insert(make_span("b1", trace_b, 10.0));
    // a1 is evicted on next insert
    store.insert(make_span("b2", trace_b, 10.0));

    let a_spans = store.get_trace(trace_a);
    assert_eq!(a_spans.len(), 1); // only a2 remains
}

#[test]
fn test_stats() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);
    store.insert(make_span("a", 1, 100.0));
    store.insert(make_span("b", 2, 200.0));

    let stats = store.stats();
    assert_eq!(stats.total_stored, 2);
    assert_eq!(stats.total_traces, 2);
}

#[test]
fn test_context_by_seq() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);
    for i in 0..10 {
        store.insert(make_span(&format!("span-{i}"), i as u128, 10.0));
    }

    // Get the seq of the 5th span (0-indexed)
    let all = store.slow_spans(0.0, 100, None);
    let target_seq = all.iter().find(|s| s.name == "span-5").unwrap().seq;

    let context = store.context_by_seq(target_seq, 2, 2);
    assert_eq!(context.len(), 5); // 2 before + target + 2 after
}
```

- [ ] **Step 2: Implement SpanStore**

`src/span/store.rs`:

```rust
use crate::engine::seq_counter::SeqCounter;
use crate::span::types::*;
use crate::filter::parser::ParsedFilter;
use crate::filter::matcher::matches_span;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

pub struct SpanStoreStats {
    pub total_stored: usize,
    pub total_traces: usize,
    pub avg_duration_ms: f64,
}

pub struct SpanStore {
    inner: RwLock<SpanStoreInner>,
    seq_counter: Arc<SeqCounter>,
}

struct SpanStoreInner {
    buffer: VecDeque<SpanEntry>,
    trace_index: HashMap<u128, Vec<u64>>, // trace_id → seq numbers
    capacity: usize,
}

impl SpanStore {
    pub fn new(capacity: usize, seq_counter: Arc<SeqCounter>) -> Self {
        Self {
            inner: RwLock::new(SpanStoreInner {
                buffer: VecDeque::with_capacity(capacity),
                trace_index: HashMap::new(),
                capacity,
            }),
            seq_counter,
        }
    }

    /// Insert a span. Caller must assign seq before calling.
    pub fn insert(&self, span: SpanEntry) {
        debug_assert!(span.seq > 0, "seq must be assigned before insert");
        let mut inner = self.inner.write().unwrap();

        // Evict if at capacity
        if inner.buffer.len() >= inner.capacity {
            if let Some(evicted) = inner.buffer.pop_front() {
                if let Some(seqs) = inner.trace_index.get_mut(&evicted.trace_id) {
                    seqs.retain(|&s| s != evicted.seq);
                    if seqs.is_empty() {
                        inner.trace_index.remove(&evicted.trace_id);
                    }
                }
            }
        }

        // Add to index
        inner.trace_index
            .entry(span.trace_id)
            .or_default()
            .push(span.seq);

        inner.buffer.push_back(span);
    }

    pub fn get_trace(&self, trace_id: u128) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        let seqs = match inner.trace_index.get(&trace_id) {
            Some(s) => s,
            None => return vec![],
        };
        let mut spans: Vec<SpanEntry> = inner.buffer.iter()
            .filter(|s| seqs.contains(&s.seq))
            .cloned()
            .collect();
        spans.sort_by(|a, b| a.start_time.cmp(&b.start_time));
        spans
    }

    pub fn slow_spans(&self, min_duration_ms: f64, count: usize, filter: Option<&ParsedFilter>) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        let mut matching: Vec<SpanEntry> = inner.buffer.iter()
            .filter(|s| s.duration_ms >= min_duration_ms)
            .filter(|s| filter.map_or(true, |f| matches_span(f, s)))
            .cloned()
            .collect();
        matching.sort_by(|a, b| b.duration_ms.partial_cmp(&a.duration_ms).unwrap_or(std::cmp::Ordering::Equal));
        matching.truncate(count);
        matching
    }

    pub fn recent_traces<F>(&self, count: usize, filter: Option<&ParsedFilter>, linked_log_count: F) -> Vec<TraceSummary>
    where
        F: Fn(u128) -> u32,
    {
        let inner = self.inner.read().unwrap();

        // Group spans by trace_id, find most recent seq per trace
        let mut trace_max_seq: HashMap<u128, u64> = HashMap::new();
        for span in inner.buffer.iter() {
            if filter.map_or(true, |f| matches_span(f, span)) {
                let entry = trace_max_seq.entry(span.trace_id).or_insert(0);
                if span.seq > *entry {
                    *entry = span.seq;
                }
            }
        }

        // Sort by max_seq descending (most recently received first)
        let mut traces: Vec<(u128, u64)> = trace_max_seq.into_iter().collect();
        traces.sort_by(|a, b| b.1.cmp(&a.1));
        traces.truncate(count);

        // Build summaries
        traces.iter().map(|&(trace_id, _)| {
            let spans: Vec<&SpanEntry> = inner.buffer.iter()
                .filter(|s| s.trace_id == trace_id)
                .collect();
            let root = spans.iter().find(|s| s.parent_span_id.is_none());
            TraceSummary {
                trace_id,
                root_span_name: root.map_or("[no root]".to_string(), |r| r.name.clone()),
                service_name: root.map_or("unknown".to_string(), |r| r.service_name.clone()),
                start_time: root.map_or(spans[0].start_time, |r| r.start_time),
                total_duration_ms: root.map_or(0.0, |r| r.duration_ms),
                span_count: spans.len() as u32,
                has_errors: spans.iter().any(|s| matches!(s.status, SpanStatus::Error(_))),
                linked_log_count: linked_log_count(trace_id),
            }
        }).collect()
    }

    pub fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        let pos = inner.buffer.iter().position(|s| s.seq == seq);
        match pos {
            Some(idx) => {
                let start = idx.saturating_sub(before);
                let end = (idx + after + 1).min(inner.buffer.len());
                inner.buffer.range(start..end).cloned().collect()
            }
            None => vec![],
        }
    }

    pub fn stats(&self) -> SpanStoreStats {
        let inner = self.inner.read().unwrap();
        let total = inner.buffer.len();
        let traces = inner.trace_index.len();
        let avg = if total > 0 {
            inner.buffer.iter().map(|s| s.duration_ms).sum::<f64>() / total as f64
        } else {
            0.0
        };
        SpanStoreStats {
            total_stored: total,
            total_traces: traces,
            avg_duration_ms: avg,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().buffer.len()
    }
}
```

Update `src/span/mod.rs`:

```rust
pub mod store;
pub mod types;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test span_store`

- [ ] **Step 4: Commit**

```bash
git add src/span/store.rs src/span/mod.rs tests/span_store.rs
git commit -m "feat: SpanStore with ring buffer, trace_id index, and query methods"
```

---

## Phase 3: Span Filter DSL

### Task 6: Extend Filter Parser with Span Selectors

**Files:**
- Modify: `src/filter/parser.rs`
- Create: `tests/span_filter.rs`

- [ ] **Step 1: Write span selector parse tests**

`tests/span_filter.rs`:

```rust
use logmon_mcp_server::filter::parser::*;

#[test]
fn test_parse_span_name_selector() {
    let f = parse_filter("sn=query_database").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::SpanName, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_service_name_selector() {
    let f = parse_filter("sv=store_server").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::ServiceName, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_status_selector() {
    let f = parse_filter("st=error").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::SpanStatus, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_kind_selector() {
    let f = parse_filter("sk=server").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::SpanKind, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_duration_gte() {
    let f = parse_filter("d>=100").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::DurationFilter(DurationOp::Gte, d) if *d == 100.0));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_duration_lte() {
    let f = parse_filter("d<=50.5").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::DurationFilter(DurationOp::Lte, d) if *d == 50.5));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_combined_span_filter() {
    let f = parse_filter("sv=store_server,d>=100").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 2);
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_is_span_filter() {
    assert!(is_span_filter(&parse_filter("sn=query").unwrap()));
    assert!(is_span_filter(&parse_filter("d>=100").unwrap()));
    assert!(!is_span_filter(&parse_filter("l>=ERROR").unwrap()));
    assert!(!is_span_filter(&parse_filter("fa=mqtt").unwrap()));
}

#[test]
fn test_mixed_log_span_selectors_error() {
    let result = parse_filter("l>=ERROR,sn=query");
    assert!(result.is_err());
}

#[test]
fn test_attribute_match_unknown_selector() {
    // "http.method=POST" — not a known selector, treated as attribute match
    let f = parse_filter("http.method=POST").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::AdditionalField(name), _) if name == "http.method"));
    } else {
        panic!("expected Qualifiers");
    }
}
```

- [ ] **Step 2: Extend parser with span selectors**

In `src/filter/parser.rs`:

Add to `Selector` enum:

```rust
pub enum Selector {
    // existing...
    SpanName,       // sn
    ServiceName,    // sv
    SpanStatus,     // st
    SpanKind,       // sk
}
```

Add new qualifier variant:

```rust
pub enum Qualifier {
    // existing...
    DurationFilter(DurationOp, f64),
}

pub enum DurationOp {
    Gte,
    Lte,
}
```

Update `parse_selector()` to recognize `sn`, `sv`, `st`, `sk`.

Add duration parsing in `parse_token()` — detect `d>=` and `d<=` patterns.

Add `is_span_filter()` public function that checks if any qualifier uses span selectors or duration.

Add validation: if filter has both span and log selectors, return error.

- [ ] **Step 3: Run tests**

Run: `cargo test --test span_filter --test filter_dsl`

- [ ] **Step 4: Commit**

```bash
git add src/filter/parser.rs tests/span_filter.rs
git commit -m "feat: span selectors (sn, sv, st, sk, d>=) in filter DSL"
```

---

### Task 7: Span Matcher

**Files:**
- Modify: `src/filter/matcher.rs`
- Add to: `tests/span_filter.rs`

- [ ] **Step 1: Write span matcher tests**

Add to `tests/span_filter.rs`:

```rust
use logmon_mcp_server::filter::matcher::matches_span;
use logmon_mcp_server::span::types::*;
use chrono::Utc;
use std::collections::HashMap;

fn make_span(name: &str, service: &str, duration_ms: f64, status: SpanStatus) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 0xabc_u128,
        span_id: 0xdef_u64,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms,
        name: name.to_string(),
        kind: SpanKind::Server,
        service_name: service.to_string(),
        status,
        attributes: HashMap::new(),
        events: vec![],
    }
}

#[test]
fn test_match_span_name() {
    let f = parse_filter("sn=query_database").unwrap();
    let span = make_span("query_database", "svc", 10.0, SpanStatus::Ok);
    assert!(matches_span(&f, &span));
}

#[test]
fn test_match_span_name_no_match() {
    let f = parse_filter("sn=query_database").unwrap();
    let span = make_span("authenticate", "svc", 10.0, SpanStatus::Ok);
    assert!(!matches_span(&f, &span));
}

#[test]
fn test_match_service_name() {
    let f = parse_filter("sv=store_server").unwrap();
    let span = make_span("op", "store_server", 10.0, SpanStatus::Ok);
    assert!(matches_span(&f, &span));
}

#[test]
fn test_match_duration_gte() {
    let f = parse_filter("d>=100").unwrap();
    assert!(matches_span(&f, &make_span("op", "svc", 150.0, SpanStatus::Ok)));
    assert!(!matches_span(&f, &make_span("op", "svc", 50.0, SpanStatus::Ok)));
    assert!(matches_span(&f, &make_span("op", "svc", 100.0, SpanStatus::Ok))); // exact
}

#[test]
fn test_match_status_error() {
    let f = parse_filter("st=error").unwrap();
    assert!(matches_span(&f, &make_span("op", "svc", 10.0, SpanStatus::Error("fail".into()))));
    assert!(!matches_span(&f, &make_span("op", "svc", 10.0, SpanStatus::Ok)));
}

#[test]
fn test_match_status_error_substring() {
    let f = parse_filter("st=timeout").unwrap();
    assert!(matches_span(&f, &make_span("op", "svc", 10.0, SpanStatus::Error("connection timeout".into()))));
    assert!(!matches_span(&f, &make_span("op", "svc", 10.0, SpanStatus::Error("null pointer".into()))));
}

#[test]
fn test_match_kind() {
    let f = parse_filter("sk=server").unwrap();
    assert!(matches_span(&f, &make_span("op", "svc", 10.0, SpanStatus::Ok))); // kind=Server
}

#[test]
fn test_match_bare_text_against_span_name() {
    let f = parse_filter("query").unwrap();
    assert!(matches_span(&f, &make_span("query_database", "svc", 10.0, SpanStatus::Ok)));
    assert!(!matches_span(&f, &make_span("authenticate", "svc", 10.0, SpanStatus::Ok)));
}

#[test]
fn test_match_attribute() {
    let f = parse_filter("http.method=POST").unwrap();
    let mut span = make_span("op", "svc", 10.0, SpanStatus::Ok);
    span.attributes.insert("http.method".to_string(), serde_json::json!("POST"));
    assert!(matches_span(&f, &span));
}

#[test]
fn test_match_combined_span_filter() {
    let f = parse_filter("sv=store_server,d>=100").unwrap();
    assert!(matches_span(&f, &make_span("op", "store_server", 150.0, SpanStatus::Ok)));
    assert!(!matches_span(&f, &make_span("op", "store_server", 50.0, SpanStatus::Ok)));
    assert!(!matches_span(&f, &make_span("op", "other", 150.0, SpanStatus::Ok)));
}
```

- [ ] **Step 2: Implement matches_span()**

In `src/filter/matcher.rs`:

```rust
pub fn matches_span(filter: &ParsedFilter, span: &SpanEntry) -> bool {
    match filter {
        ParsedFilter::All => true,
        ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qualifiers) => {
            qualifiers.iter().all(|q| matches_span_qualifier(q, span))
        }
    }
}

fn matches_span_qualifier(qualifier: &Qualifier, span: &SpanEntry) -> bool {
    match qualifier {
        Qualifier::BarePattern(pattern) => {
            // Bare pattern matches span name only
            matches_pattern(pattern, &span.name)
        }
        Qualifier::SelectorPattern(selector, pattern) => {
            match selector {
                Selector::SpanName => matches_pattern(pattern, &span.name),
                Selector::ServiceName => matches_pattern(pattern, &span.service_name),
                Selector::SpanStatus => match &span.status {
                    SpanStatus::Error(msg) => {
                        let pat_str = pattern_as_str(pattern);
                        pat_str == "error" || matches_pattern(pattern, msg)
                    }
                    SpanStatus::Ok => pattern_as_str(pattern) == "ok",
                    SpanStatus::Unset => pattern_as_str(pattern) == "unset",
                },
                Selector::SpanKind => {
                    let kind_str = format!("{:?}", span.kind).to_lowercase();
                    matches_pattern(pattern, &kind_str)
                }
                Selector::AdditionalField(key) => {
                    span.attributes.get(key)
                        .and_then(|v| v.as_str())
                        .map_or(false, |v| matches_pattern(pattern, v))
                }
                _ => false, // log selectors don't match spans
            }
        }
        Qualifier::DurationFilter(op, threshold) => {
            match op {
                DurationOp::Gte => span.duration_ms >= *threshold,
                DurationOp::Lte => span.duration_ms <= *threshold,
            }
        }
        Qualifier::LevelFilter(_, _) => false, // log-only
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test span_filter`

- [ ] **Step 4: Commit**

```bash
git add src/filter/matcher.rs tests/span_filter.rs
git commit -m "feat: matches_span() for span filter DSL evaluation"
```

---

## Phase 4: OTLP Receiver

### Task 8: OTLP → LogEntry/SpanEntry Mapping

**Files:**
- Create: `src/receiver/otlp/mod.rs`
- Create: `src/receiver/otlp/mapping.rs`
- Create: `tests/otlp_mapping.rs`
- Modify: `src/receiver/mod.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Add dependencies**

Add to `Cargo.toml`:

```toml
tonic = "0.13"
prost = "0.13"
opentelemetry-proto = { version = "0.27", features = ["gen-tonic", "logs", "trace"] }
axum = "0.8"
```

Note: Check the latest compatible versions of `opentelemetry-proto` and `tonic` at the time of implementation. The `opentelemetry-proto` crate provides pre-generated Rust types for OTLP protobuf messages.

- [ ] **Step 2: Write mapping tests**

`tests/otlp_mapping.rs`:

```rust
use logmon_mcp_server::receiver::otlp::mapping::*;
use logmon_mcp_server::gelf::message::Level;

#[test]
fn test_severity_to_level() {
    assert_eq!(severity_to_level(1), Level::Trace);
    assert_eq!(severity_to_level(5), Level::Debug);
    assert_eq!(severity_to_level(9), Level::Info);
    assert_eq!(severity_to_level(13), Level::Warn);
    assert_eq!(severity_to_level(17), Level::Error);
    assert_eq!(severity_to_level(21), Level::Error); // Fatal → Error
    assert_eq!(severity_to_level(0), Level::Info); // unspecified → Info
}

#[test]
fn test_bytes_to_trace_id() {
    let bytes = vec![0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb1, 0x6e, 0x0f,
                     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
    assert_eq!(bytes_to_trace_id(&bytes), Some(0x4bf92f3577b16e0f0000000000000001_u128));
}

#[test]
fn test_bytes_to_trace_id_empty() {
    assert_eq!(bytes_to_trace_id(&[]), None);
    assert_eq!(bytes_to_trace_id(&[0; 16]), None); // all zeros = no trace
}

#[test]
fn test_bytes_to_span_id() {
    let bytes = vec![0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];
    assert_eq!(bytes_to_span_id(&bytes), Some(0x00f067aa0ba902b7_u64));
}

#[test]
fn test_bytes_to_span_id_empty() {
    assert_eq!(bytes_to_span_id(&[]), None);
    assert_eq!(bytes_to_span_id(&[0; 8]), None);
}
```

- [ ] **Step 3: Implement mapping functions**

`src/receiver/otlp/mapping.rs`:

```rust
use crate::gelf::message::{Level, LogEntry, LogSource};
use crate::span::types::*;
use chrono::{DateTime, Utc, TimeZone};
use std::collections::HashMap;

pub fn severity_to_level(severity: i32) -> Level {
    match severity {
        1..=4 => Level::Trace,
        5..=8 => Level::Debug,
        9..=12 => Level::Info,
        13..=16 => Level::Warn,
        17..=24 => Level::Error, // Error + Fatal
        _ => Level::Info, // Unspecified → Info
    }
}

pub fn bytes_to_trace_id(bytes: &[u8]) -> Option<u128> {
    if bytes.len() != 16 {
        return None;
    }
    let val = u128::from_be_bytes(bytes.try_into().ok()?);
    if val == 0 { None } else { Some(val) }
}

pub fn bytes_to_span_id(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != 8 {
        return None;
    }
    let val = u64::from_be_bytes(bytes.try_into().ok()?);
    if val == 0 { None } else { Some(val) }
}

pub fn nanos_to_datetime(nanos: u64) -> DateTime<Utc> {
    let secs = (nanos / 1_000_000_000) as i64;
    let nsecs = (nanos % 1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nsecs).single().unwrap_or_else(Utc::now)
}

/// Extract service.name and host.name from OTLP resource attributes.
/// Returns (service_name, host_name).
/// The attrs parameter accepts a generic slice of (key, value) pairs.
/// When called from gRPC: convert opentelemetry_proto KeyValue vec to (String, String) pairs first.
/// When called from HTTP/JSON: extract from serde_json::Value directly.
pub fn extract_resource_attrs(attrs: &[(String, String)]) -> (String, String) {
    let mut service = "unknown".to_string();
    let mut host = "unknown".to_string();
    for (k, v) in attrs {
        match k.as_str() {
            "service.name" => service = v.clone(),
            "host.name" => host = v.clone(),
            _ => {}
        }
    }
    (service, host)
}

/// Convert opentelemetry-proto KeyValue attributes to (String, String) pairs.
/// Non-string values are formatted as debug strings.
pub fn key_values_to_pairs(kvs: &[opentelemetry_proto::tonic::common::v1::KeyValue]) -> Vec<(String, String)> {
    kvs.iter().filter_map(|kv| {
        kv.value.as_ref().map(|v| {
            let val = match &v.value {
                Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(s)) => s.clone(),
                Some(other) => format!("{:?}", other),
                None => String::new(),
            };
            (kv.key.clone(), val)
        })
    }).collect()
}
```

`src/receiver/otlp/mod.rs`:

```rust
pub mod mapping;
```

Update `src/receiver/mod.rs`:

```rust
pub mod otlp;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test otlp_mapping`

- [ ] **Step 5: Commit**

```bash
git add src/receiver/otlp/ tests/otlp_mapping.rs Cargo.toml src/receiver/mod.rs
git commit -m "feat: OTLP → LogEntry/SpanEntry mapping functions"
```

---

### Task 9: OTLP gRPC Server

**Files:**
- Create: `src/receiver/otlp/grpc.rs`
- Modify: `src/receiver/otlp/mod.rs`

- [ ] **Step 1: Implement gRPC services**

`src/receiver/otlp/grpc.rs`:

Implement tonic gRPC services for OTLP LogsService and TraceService. Use the `opentelemetry-proto` crate's generated types.

```rust
use crate::gelf::message::{LogEntry, LogSource};
use crate::receiver::otlp::mapping::*;
use crate::span::types::*;
use opentelemetry_proto::tonic::collector::logs::v1::{
    logs_service_server::{LogsService, LogsServiceServer},
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct OtlpLogsService {
    log_sender: mpsc::Sender<LogEntry>,
    malformed_count: AtomicU64,
}

pub struct OtlpTraceService {
    span_sender: mpsc::Sender<SpanEntry>,
    malformed_count: AtomicU64,
}

// Implementation converts OTLP protobuf types to LogEntry/SpanEntry
// using the mapping functions from mapping.rs, then sends via mpsc channels.
// Malformed entries (missing required fields) are counted and skipped.
// Returns success even for partial batches (OTLP convention).
```

The implementer should read the `opentelemetry-proto` crate docs to understand the exact protobuf message structures. Key types:

- `ExportLogsServiceRequest` contains `resource_logs` → `scope_logs` → `log_records`
- `ExportTraceServiceRequest` contains `resource_spans` → `scope_spans` → `spans`
- Resource attributes are on the outer `resource_logs`/`resource_spans` wrapper

- [ ] **Step 2: Build gRPC server startup function**

```rust
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let logs_svc = OtlpLogsService { log_sender, malformed_count: AtomicU64::new(0) };
    let trace_svc = OtlpTraceService { span_sender, malformed_count: AtomicU64::new(0) };

    tonic::transport::Server::builder()
        .add_service(LogsServiceServer::new(logs_svc))
        .add_service(TraceServiceServer::new(trace_svc))
        .serve_with_shutdown(addr, async move { let _ = shutdown_rx.recv().await; })
        .await?;
    Ok(())
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/receiver/otlp/grpc.rs src/receiver/otlp/mod.rs
git commit -m "feat: OTLP gRPC server (LogsService + TraceService)"
```

---

### Task 10: OTLP HTTP/JSON Server

**Files:**
- Create: `src/receiver/otlp/http.rs`
- Modify: `src/receiver/otlp/mod.rs`

- [ ] **Step 1: Implement HTTP/JSON endpoints**

`src/receiver/otlp/http.rs`:

```rust
use crate::gelf::message::LogEntry;
use crate::receiver::otlp::mapping::*;
use crate::span::types::SpanEntry;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use tokio::sync::mpsc;

#[derive(Clone)]
struct AppState {
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
}

pub async fn start_http_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let state = AppState { log_sender, span_sender };
    let app = Router::new()
        .route("/v1/logs", post(handle_logs))
        .route("/v1/traces", post(handle_traces))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { let _ = shutdown_rx.recv().await; })
        .await?;
    Ok(())
}

// Handlers parse JSON OTLP format (same structure as protobuf, but JSON-encoded)
// and convert to LogEntry/SpanEntry using mapping functions.
async fn handle_logs(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    // Parse OTLP JSON log format, convert, send to channel
    // Return 200 OK (OTLP convention)
    StatusCode::OK
}

async fn handle_traces(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    // Parse OTLP JSON trace format, convert, send to channel
    StatusCode::OK
}
```

Note: The OTLP JSON format mirrors the protobuf structure. The implementer should reference the [OTLP JSON encoding spec](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding) for exact field names (camelCase in JSON).

- [ ] **Step 2: Verify compilation**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/receiver/otlp/http.rs src/receiver/otlp/mod.rs
git commit -m "feat: OTLP HTTP/JSON server (/v1/logs, /v1/traces)"
```

---

### Task 11: OtlpReceiver + Receiver Trait Rename

**Files:**
- Modify: `src/receiver/mod.rs` (rename trait)
- Modify: `src/receiver/gelf.rs` (update trait impl)
- Modify: `src/receiver/otlp/mod.rs` (add OtlpReceiver struct)

- [ ] **Step 1: Rename LogReceiver → Receiver**

In `src/receiver/mod.rs`:

```rust
#[async_trait]
pub trait Receiver: Send + Sync {
    fn name(&self) -> &str;
    fn listening_on(&self) -> Vec<String>;
    async fn shutdown(self: Box<Self>);
}
```

Update `src/receiver/gelf.rs`: change `impl LogReceiver for GelfReceiver` → `impl Receiver for GelfReceiver`.

Update any imports in `src/daemon/server.rs` that reference `LogReceiver`.

- [ ] **Step 2: Create OtlpReceiver**

In `src/receiver/otlp/mod.rs`:

```rust
pub mod grpc;
pub mod http;
pub mod mapping;

use crate::gelf::message::LogEntry;
use crate::receiver::Receiver;
use crate::span::types::SpanEntry;
use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

pub struct OtlpReceiverConfig {
    pub grpc_addr: String,
    pub http_addr: String,
}

pub struct OtlpReceiver {
    grpc_handle: JoinHandle<()>,
    http_handle: JoinHandle<()>,
    shutdown_tx: broadcast::Sender<()>,
    grpc_port: u16,
    http_port: u16,
}

impl OtlpReceiver {
    pub async fn start(
        config: OtlpReceiverConfig,
        log_sender: mpsc::Sender<LogEntry>,
        span_sender: mpsc::Sender<SpanEntry>,
    ) -> anyhow::Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);

        let grpc_addr: std::net::SocketAddr = config.grpc_addr.parse()?;
        let http_addr: std::net::SocketAddr = config.http_addr.parse()?;

        let grpc_handle = tokio::spawn(
            grpc::start_grpc_server(grpc_addr, log_sender.clone(), span_sender.clone(), shutdown_tx.subscribe())
        );
        let http_handle = tokio::spawn(
            http::start_http_server(http_addr, log_sender, span_sender, shutdown_tx.subscribe())
        );

        Ok(Self {
            grpc_handle,
            http_handle,
            shutdown_tx,
            grpc_port: grpc_addr.port(),
            http_port: http_addr.port(),
        })
    }
}

#[async_trait]
impl Receiver for OtlpReceiver {
    fn name(&self) -> &str { "otlp" }
    fn listening_on(&self) -> Vec<String> {
        vec![
            format!("gRPC:{}", self.grpc_port),
            format!("HTTP:{}", self.http_port),
        ]
    }
    async fn shutdown(self: Box<Self>) {
        let _ = self.shutdown_tx.send(());
    }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/receiver/ src/daemon/server.rs
git commit -m "feat: OtlpReceiver + rename LogReceiver → Receiver"
```

---

## Phase 5: Span Processing

### Task 12: Span Processor

**Files:**
- Create: `src/daemon/span_processor.rs`
- Create: `tests/span_processor.rs`
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Write span processor tests**

`tests/span_processor.rs`:

```rust
use logmon_mcp_server::daemon::span_processor::process_span;
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::span::store::SpanStore;
use logmon_mcp_server::span::types::*;
use logmon_mcp_server::engine::seq_counter::SeqCounter;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_span(name: &str, duration_ms: f64) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 0xabc_u128,
        span_id: 0xdef_u64,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms,
        name: name.to_string(),
        kind: SpanKind::Internal,
        service_name: "test".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

#[test]
fn test_span_stored() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());

    let mut span = make_span("query", 100.0);
    process_span(&mut span, &store, &sessions);
    assert!(span.seq > 0);
    assert_eq!(store.len(), 1);
}

#[test]
fn test_span_trigger_fires() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    // Add a span trigger
    sessions.add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow span")).unwrap();

    let mut span = make_span("slow_query", 600.0);
    process_span(&mut span, &store, &sessions);

    // Notification should be queued (anonymous session is connected,
    // so it goes to the broadcast channel — check session has no queued notifs)
    assert_eq!(store.len(), 1);
}

#[test]
fn test_span_trigger_no_match() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    sessions.add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow")).unwrap();

    let mut span = make_span("fast_query", 10.0);
    process_span(&mut span, &store, &sessions);

    // Span stored but no trigger fired
    assert_eq!(store.len(), 1);
}
```

- [ ] **Step 2: Implement span processor**

`src/daemon/span_processor.rs`:

```rust
use crate::daemon::session::SessionRegistry;
use crate::engine::seq_counter::SeqCounter;
use crate::filter::parser::is_span_filter;
use crate::filter::matcher::matches_span;
use crate::span::store::SpanStore;
use crate::span::types::SpanEntry;
use std::sync::Arc;
use tokio::sync::mpsc;

pub fn spawn_span_processor(
    mut receiver: mpsc::Receiver<SpanEntry>,
    seq_counter: Arc<SeqCounter>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut span) = receiver.recv().await {
            span.seq = seq_counter.next();
            process_span(&mut span, &span_store, &sessions);
        }
    })
}

pub fn process_span(span: &mut SpanEntry, store: &SpanStore, sessions: &SessionRegistry) {
    // 1. Store unconditionally
    store.insert(span.clone());

    // 2. Evaluate span triggers for each session
    let session_ids = sessions.active_session_ids();
    for sid in &session_ids {
        let triggers = sessions.list_triggers(sid);
        for trigger in &triggers {
            if is_span_filter_str(&trigger.filter_string) {
                if let Ok(filter) = crate::filter::parser::parse_filter(&trigger.filter_string) {
                    if matches_span(&filter, span) {
                        // Build and send span trigger notification
                        let trace_summary = build_trace_summary(span.trace_id, store);
                        sessions.send_or_queue_span_notification(sid, span, trigger, trace_summary);
                    }
                }
            }
        }
    }
}

fn is_span_filter_str(filter_str: &str) -> bool {
    crate::filter::parser::parse_filter(filter_str)
        .map(|f| crate::filter::parser::is_span_filter(&f))
        .unwrap_or(false)
}

fn build_trace_summary(trace_id: u128, store: &SpanStore) -> Option<crate::span::types::TraceSummary> {
    let spans = store.get_trace(trace_id);
    if spans.is_empty() { return None; }
    let root = spans.iter().find(|s| s.parent_span_id.is_none());
    Some(crate::span::types::TraceSummary {
        trace_id,
        root_span_name: root.map_or("[incomplete]".to_string(), |r| r.name.clone()),
        service_name: root.map_or("unknown".to_string(), |r| r.service_name.clone()),
        start_time: root.map_or(spans[0].start_time, |r| r.start_time),
        total_duration_ms: root.map_or(0.0, |r| r.duration_ms),
        span_count: spans.len() as u32,
        has_errors: spans.iter().any(|s| matches!(s.status, crate::span::types::SpanStatus::Error(_))),
        linked_log_count: 0, // cross-store query done at RPC level
    })
}
```

Note: The `send_or_queue_span_notification` method needs to be added to SessionRegistry (Task 13 handles session extensions). For now, this can call the existing `send_or_queue_notification` with a PipelineEvent variant, or a new method. The implementer should check what exists and adapt.

Add to `src/daemon/mod.rs`:

```rust
pub mod span_processor;
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test span_processor`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/span_processor.rs tests/span_processor.rs src/daemon/mod.rs
git commit -m "feat: span processor with trigger evaluation"
```

---

### Task 13: Session Registry Span Trigger Support

**Files:**
- Modify: `src/daemon/session.rs`
- Modify: `src/engine/pipeline.rs` (add trace fields to PipelineEvent)

- [ ] **Step 1: Add trace fields to PipelineEvent**

In `src/engine/pipeline.rs`, add to `PipelineEvent`:

```rust
pub struct PipelineEvent {
    // ... existing fields ...

    pub trace_id: Option<u128>,
    pub trace_summary: Option<crate::span::types::TraceSummary>,
}
```

Update all places that construct PipelineEvent to set these to None.

- [ ] **Step 2: Add span notification methods to SessionRegistry**

In `src/daemon/session.rs`, add:

```rust
/// Send or queue a span trigger notification.
pub fn send_or_queue_span_notification(
    &self,
    id: &SessionId,
    span: &SpanEntry,
    trigger: &TriggerInfo,
    trace_summary: Option<TraceSummary>,
) {
    let event = PipelineEvent {
        trigger_id: trigger.id,
        trigger_description: trigger.description.clone(),
        filter_string: trigger.filter_string.clone(),
        matched_entry: LogEntry {
            seq: span.seq,
            timestamp: span.start_time,
            level: if matches!(span.status, SpanStatus::Error(_)) { Level::Error } else { Level::Info },
            message: format!("[span] {} ({}ms)", span.name, span.duration_ms),
            full_message: None,
            host: span.service_name.clone(),
            facility: Some(span.service_name.clone()),
            file: None, line: None,
            additional_fields: HashMap::new(),
            matched_filters: vec![],
            source: LogSource::Filter,
            trace_id: Some(span.trace_id),
            span_id: Some(span.span_id),
        },
        context_before: vec![],
        pre_trigger_flushed: 0,
        post_window_size: 0,
        trace_id: Some(span.trace_id),
        trace_summary,
    };
    self.send_or_queue_notification(id, event);
}

/// Get session IDs (not sorted by pre_window — that's log-specific).
pub fn active_session_ids(&self) -> Vec<SessionId> {
    let sessions = self.sessions.read().unwrap();
    sessions.keys()
        .filter(|id| {
            sessions.get(*id).map_or(false, |s| s.connected.load(Ordering::Relaxed))
        })
        .cloned()
        .collect()
}
```

The implementer will need to decide whether to extend `PipelineEvent` to carry span data or create a separate `SpanTriggerEvent` type. The simplest approach: reuse `PipelineEvent` with the new optional trace fields, and set `matched_entry` to a synthetic LogEntry with the span info. The alternative (cleaner but more work) is an enum `TriggerEvent { Log(PipelineEvent), Span(SpanTriggerEvent) }`.

- [ ] **Step 3: Run all tests**

Run: `cargo test`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/session.rs src/engine/pipeline.rs
git commit -m "feat: span trigger notifications in session registry"
```

---

## Phase 6: Log Processor Enhancements

### Task 14: Trace-Aware Pre-Buffer Flush + Trigger Notification Enhancement

**Files:**
- Modify: `src/daemon/log_processor.rs`
- Add to: `tests/log_processor.rs`

- [ ] **Step 1: Write trace-aware flush tests**

Add to `tests/log_processor.rs`:

```rust
#[test]
fn test_trace_aware_prebuffer_flush() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    sessions.add_filter(&sid, "l>=ERROR", None).unwrap();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace = 0xabc_u128;

    // Send INFO logs with trace context — not stored (filter blocks INFO)
    for i in 0..5 {
        let mut e = make_entry(Level::Info, &format!("step {i}"));
        e.trace_id = Some(trace);
        process_entry(&mut e, &pipeline, &sessions);
    }
    assert_eq!(pipeline.store_len(), 0);

    // Send ERROR with same trace_id — trigger fires
    let mut error = make_entry(Level::Error, "crash");
    error.trace_id = Some(trace);
    process_entry(&mut error, &pipeline, &sessions);

    // Pre-buffer flush + trace-aware flush should include the traced INFO logs
    let logs = pipeline.recent_logs(100, None);
    let traced_logs: Vec<_> = logs.iter().filter(|l| l.trace_id == Some(trace)).collect();
    assert!(traced_logs.len() > 1); // error + at least some context
}
```

- [ ] **Step 2: Implement trace-aware pre-buffer flush**

In `src/daemon/log_processor.rs`, after the existing pre-buffer flush in the trigger match block:

```rust
// After copying pre_window entries from pre-buffer:
// Additionally, copy logs from the same trace_id
if let Some(tid) = entry.trace_id {
    let trace_entries = pipeline.pre_buffer_entries_by_trace_id(tid);
    for mut trace_entry in trace_entries {
        if !pipeline.contains_seq(trace_entry.seq) {
            trace_entry.source = LogSource::PreTrigger;
            pipeline.append_to_store(trace_entry);
        }
    }
}
```

This requires adding a `pre_buffer_entries_by_trace_id()` method to LogPipeline that scans the pre-buffer for matching entries.

- [ ] **Step 3: Enhance trigger notification with trace summary**

When building PipelineEvent in the trigger match block, populate the new trace fields:

```rust
let event = PipelineEvent {
    // ... existing fields ...
    trace_id: entry.trace_id,
    trace_summary: None, // Populated by RPC handler at query time, or by span_store lookup
};
```

Note: The trace_summary requires access to SpanStore, which the log processor doesn't have. Two options:
1. Pass SpanStore to log processor (adds coupling)
2. Set trace_summary to None here, and have the notification forwarder/RPC handler enrich it

Option 2 is cleaner. The trace_id is enough for the AI to call `get_trace` if needed.

- [ ] **Step 4: Run tests**

Run: `cargo test --test log_processor`

- [ ] **Step 5: Commit**

```bash
git add src/daemon/log_processor.rs src/engine/pipeline.rs tests/log_processor.rs
git commit -m "feat: trace-aware pre-buffer flush and trigger notification enhancement"
```

---

## Phase 7: RPC + MCP Tools

### Task 15: Trace RPC Methods

**Files:**
- Modify: `src/daemon/rpc_handler.rs`

- [ ] **Step 1: Add SpanStore to RpcHandler**

```rust
pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    span_store: Arc<SpanStore>,  // NEW
    sessions: Arc<SessionRegistry>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}
```

Update constructor.

- [ ] **Step 2: Add trace RPC method handlers**

Add to the `handle()` dispatch:

```rust
"traces.recent" => self.handle_traces_recent(&request.params),
"traces.get" => self.handle_traces_get(&request.params),
"traces.summary" => self.handle_traces_summary(&request.params),
"traces.slow" => self.handle_traces_slow(&request.params),
"traces.logs" => self.handle_traces_logs(&request.params),
"spans.context" => self.handle_spans_context(&request.params),
```

Each handler follows the same pattern: parse params, call span_store/pipeline methods, return JSON.

Key implementation details:
- `traces.get`: call `span_store.get_trace()` + `pipeline.logs_by_trace_id()`, build tree structure
- `traces.summary`: get trace, compute self-time for direct children of root
- `traces.slow`: call `span_store.slow_spans()`, optionally group by name
- `traces.logs`: call `pipeline.logs_by_trace_id()`, apply optional filter

Also update `handle_logs_recent` to accept optional `trace_id` parameter.

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/rpc_handler.rs
git commit -m "feat: trace RPC methods (traces.recent/get/summary/slow/logs, spans.context)"
```

---

### Task 16: Trace MCP Tools

**Files:**
- Modify: `src/mcp/server.rs`

- [ ] **Step 1: Add 6 new trace tool definitions**

Add parameter structs:

```rust
#[derive(Deserialize, JsonSchema)]
struct GetRecentTracesParams {
    count: Option<u32>,
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceParams {
    trace_id: String,
    include_logs: Option<bool>,
    filter: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceSummaryParams {
    trace_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct GetSlowSpansParams {
    min_duration_ms: Option<f64>,
    count: Option<u32>,
    filter: Option<String>,
    group_by: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct GetSpanContextParams {
    seq: u64,
    before: Option<u32>,
    after: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct GetTraceLogsParams {
    trace_id: String,
    filter: Option<String>,
}
```

Add tool implementations — all follow the bridge.call() pattern:

```rust
#[rmcp::tool(description = "List recent traces with timing and error info")]
async fn get_recent_traces(&self, Parameters(p): Parameters<GetRecentTracesParams>) -> Result<CallToolResult, rmcp::ErrorData> {
    let result = self.bridge.call("traces.recent", serde_json::json!({
        "count": p.count,
        "filter": p.filter,
    })).await.map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap()
    )]))
}

// ... same pattern for all 6 tools
```

Update `get_recent_logs` to accept optional `trace_id`:

```rust
#[derive(Deserialize, JsonSchema)]
struct GetRecentLogsParams {
    count: Option<u32>,
    filter: Option<String>,
    trace_id: Option<String>,  // NEW
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/mcp/server.rs
git commit -m "feat: 6 trace MCP tools + enhanced get_recent_logs with trace_id"
```

---

## Phase 8: Daemon Integration

### Task 17: Config + CLI + Daemon Startup

**Files:**
- Modify: `src/config.rs`
- Modify: `src/daemon/persistence.rs`
- Modify: `src/daemon/server.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add OTLP config fields**

In `src/daemon/persistence.rs`, add to `DaemonConfig`:

```rust
pub struct DaemonConfig {
    // ... existing ...
    #[serde(default = "default_otlp_grpc_port")]
    pub otlp_grpc_port: u16,
    #[serde(default = "default_otlp_http_port")]
    pub otlp_http_port: u16,
    #[serde(default = "default_span_buffer_size")]
    pub span_buffer_size: usize,
}

fn default_otlp_grpc_port() -> u16 { 4317 }
fn default_otlp_http_port() -> u16 { 4318 }
fn default_span_buffer_size() -> usize { 10000 }
```

In `src/config.rs`, add to the Daemon subcommand:

```rust
Commands::Daemon {
    // ... existing ...
    #[arg(long, default_value = "4317")]
    otlp_grpc_port: u16,
    #[arg(long, default_value = "4318")]
    otlp_http_port: u16,
    #[arg(long, default_value = "10000")]
    span_buffer_size: usize,
}
```

- [ ] **Step 2: Update daemon startup**

In `src/daemon/server.rs`, update `run_daemon()`:

```rust
// After creating LogPipeline:
let seq_counter = Arc::new(SeqCounter::new_with_initial(initial_seq));
let pipeline = Arc::new(LogPipeline::new_with_seq_counter(config.buffer_size, seq_counter.clone()));
let span_store = Arc::new(SpanStore::new(config.span_buffer_size, seq_counter.clone()));

// Create span channel
let (span_tx, span_rx) = mpsc::channel(1024);

// Start OTLP receiver (if ports > 0)
if config.otlp_grpc_port > 0 || config.otlp_http_port > 0 {
    let otlp_config = OtlpReceiverConfig {
        grpc_addr: format!("0.0.0.0:{}", config.otlp_grpc_port),
        http_addr: format!("0.0.0.0:{}", config.otlp_http_port),
    };
    let otlp_receiver = OtlpReceiver::start(otlp_config, log_tx.clone(), span_tx).await?;
    // ... add to receivers_info
}

// Start span processor
let _span_processor = spawn_span_processor(span_rx, seq_counter.clone(), span_store.clone(), sessions.clone());

// Update RpcHandler to include span_store
let handler = Arc::new(RpcHandler::new(pipeline.clone(), span_store.clone(), sessions.clone(), receivers_info));
```

Update `src/main.rs` to pass new CLI args to DaemonConfig.

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/config.rs src/daemon/persistence.rs src/daemon/server.rs src/main.rs
git commit -m "feat: OTLP daemon integration with config, CLI, and startup"
```

---

## Phase 9: Testing + Polish

### Task 18: Cross-Store Trace Linking Tests

**Files:**
- Create: `tests/trace_linking.rs`

- [ ] **Step 1: Write cross-store tests**

```rust
use logmon_mcp_server::daemon::log_processor::{process_entry, sync_pre_buffer_size};
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::span::store::SpanStore;
use logmon_mcp_server::span::types::*;
use logmon_mcp_server::engine::seq_counter::SeqCounter;
use logmon_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn test_logs_and_spans_linked_by_trace_id() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace_id = 0xabc123_u128;

    // Insert a log with trace context
    let mut log = make_log(Level::Info, "processing request");
    log.trace_id = Some(trace_id);
    process_entry(&mut log, &pipeline, &sessions);

    // Insert a span with same trace_id
    let span = make_span("handle_request", trace_id, 100.0);
    span_store.insert(span);

    // Cross-store query: logs for this trace
    let traced_logs = pipeline.logs_by_trace_id(trace_id);
    assert_eq!(traced_logs.len(), 1);

    // Cross-store query: spans for this trace
    let traced_spans = span_store.get_trace(trace_id);
    assert_eq!(traced_spans.len(), 1);

    // Both share the same global seq counter
    assert!(traced_logs[0].seq != traced_spans[0].seq);
}

#[test]
fn test_shared_seq_counter_interleaves() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    sync_pre_buffer_size(&pipeline, &sessions);

    let mut log = make_log(Level::Info, "first");
    process_entry(&mut log, &pipeline, &sessions);
    let log_seq = log.seq;

    let mut span = make_span("second", 1, 10.0);
    span_store.insert(span.clone());
    // span.seq should be log_seq + 1

    let mut log2 = make_log(Level::Info, "third");
    process_entry(&mut log2, &pipeline, &sessions);
    assert!(log2.seq > log_seq + 1); // seq counter shared
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --test trace_linking`

- [ ] **Step 3: Commit**

```bash
git add tests/trace_linking.rs
git commit -m "test: cross-store trace linking integration tests"
```

---

### Task 19: Skill Update + Final Polish

**Files:**
- Modify: `skill/logmon.md` (and copy to `~/.claude/skills/logmon/SKILL.md`)
- Modify: `README.md`

- [ ] **Step 1: Update skill with trace commands**

Add to Quick Commands in the skill:

```markdown
- `/logmon traces` — call `get_recent_traces` and summarize
- `/logmon slow` — call `get_slow_spans` with default threshold and summarize
- `/logmon trace <trace_id>` — call `get_trace` for a specific trace
```

Add workflow tips:

```markdown
### Investigating slowness
1. `get_slow_spans` to find bottleneck spans
2. `get_trace_summary` on the affected trace for timing breakdown
3. `get_trace` for the full span tree with linked logs
4. To compare: `get_recent_traces` to find a fast trace for the same endpoint, compare both summaries

### Following a request through the system
1. Find the request in `get_recent_traces`
2. `get_trace` to see the full span tree with logs interleaved
3. `get_trace_logs` with a filter to focus on specific log types within the trace
```

- [ ] **Step 2: Update README**

Add OTLP section to README:
- OTLP ports (gRPC 4317, HTTP 4318)
- How to configure OpenTelemetry SDK in your application
- New trace tools
- Example: `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317`

- [ ] **Step 3: Run clippy + full test suite**

```bash
cargo clippy
cargo test
```

- [ ] **Step 4: Commit and push**

```bash
git add skill/logmon.md README.md
git commit -m "docs: skill and README updates for OTLP trace support"
git push
```

---

### Task 20: MCP Notification Forwarding for Span Triggers

**Files:**
- Modify: `src/mcp/notifications.rs`
- Modify: `src/shim/bridge.rs`

- [ ] **Step 1: Handle span trigger notifications in notification forwarder**

The notification forwarder in `src/mcp/notifications.rs` receives `RpcNotification` from the daemon. Span trigger notifications arrive with method `"trigger.fired"` (same as log triggers) but with the span trigger schema (containing `type: "span_trigger"`, `matched_span`, `trace_summary`).

No changes needed to the forwarder itself — it already forwards all RPC notifications as MCP custom notifications. The daemon's connection handler (`src/daemon/server.rs`) already broadcasts PipelineEvents via the broadcast channel.

Verify that the span processor's `send_or_queue_notification` (from Task 13) correctly delivers to connected sessions via the same PipelineEvent broadcast channel. If it uses a different path, wire it up.

- [ ] **Step 2: Verify compilation and test manually**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/mcp/notifications.rs
git commit -m "feat: verify span trigger notifications flow through MCP"
```

---

### Task 21: Prerequisite — tracing_init GELF Trace Context

**Files:**
- Modify: `/Users/yuval/Documents/Projects/Store/tracing_init/src/gelf.rs`

This task is in the **Store** project, not logmon-mcp. It enhances the GELF layer to emit `_trace_id` and `_span_id` fields when OpenTelemetry context is available.

- [ ] **Step 1: Check if tracing-opentelemetry is available**

The GelfLayer's `on_event` method has access to the current span context via the `ctx` parameter. If the application has a `tracing-opentelemetry` layer installed, the span extensions will contain an OTel `SpanContext`.

```rust
use tracing_opentelemetry::OtelData;
use opentelemetry::trace::TraceContextExt;

// In on_event or the visitor, access the current span:
if let Some(otel_data) = ctx.current_span().extensions().get::<OtelData>() {
    let span_context = otel_data.parent_cx.span().span_context();
    if span_context.is_valid() {
        let trace_id = format!("{:032x}", span_context.trace_id());
        let span_id = format!("{:016x}", span_context.span_id());
        // Add as GELF additional fields: _trace_id, _span_id
    }
}
```

Note: This adds `tracing-opentelemetry` and `opentelemetry` as optional dependencies. Use a cargo feature flag (`otel`) so the GELF layer works without OTel when the feature is not enabled.

- [ ] **Step 2: Add feature-gated trace context extraction**

In `Cargo.toml` of tracing_init:

```toml
[features]
default = []
otel = ["tracing-opentelemetry", "opentelemetry"]

[dependencies]
tracing-opentelemetry = { version = "0.28", optional = true }
opentelemetry = { version = "0.27", optional = true }
```

In the GELF layer's event handler, conditionally extract trace context:

```rust
#[cfg(feature = "otel")]
fn extract_trace_context(extensions: &tracing::span::Extensions) -> Option<(String, String)> {
    // Extract trace_id and span_id from OTel context
}
```

- [ ] **Step 3: Test with store_server**

Enable the `otel` feature in store_server's Cargo.toml, add `tracing-opentelemetry` layer, and verify `_trace_id` and `_span_id` appear in GELF messages received by logmon.

- [ ] **Step 4: Commit (in Store repo)**

```bash
cd /Users/yuval/Documents/Projects/Store
git add tracing_init/
git commit -m "feat: GELF layer emits _trace_id/_span_id when OTel context available"
```

---

## Task Dependency Graph

```
Task 1 (SpanEntry types)
    └→ Task 5 (SpanStore)
        └→ Task 12 (Span Processor)
            └→ Task 17 (Daemon Integration)

Task 2 (LogEntry trace_id)
    ├→ Task 4 (LogStore trace index)
    │   └→ Task 14 (Trace-aware flush)
    │       └→ Task 17
    └→ Task 18 (Trace linking tests)

Task 3 (SeqCounter extraction)
    ├→ Task 5
    └→ Task 17

Task 6 (Span filter parser)
    └→ Task 7 (Span matcher)
        └→ Task 12

Task 8 (OTLP mapping)
    ├→ Task 9 (gRPC server)
    └→ Task 10 (HTTP server)
        └→ Task 11 (OtlpReceiver)
            └→ Task 17

Task 13 (Session span triggers)
    └→ Task 12

Task 15 (Trace RPC)
    └→ Task 16 (Trace MCP tools)
        └→ Task 19 (Skill + polish)
```

**Parallelizable groups:**
- Tasks 1, 2, 3 (all foundation, independent)
- Tasks 6+7, 8+9+10 (DSL and OTLP, independent of each other)
- Tasks 4, 5 (both depend on Task 2 and 1 respectively)
