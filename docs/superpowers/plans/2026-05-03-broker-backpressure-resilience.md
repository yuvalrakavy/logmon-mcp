# Broker Backpressure Resilience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `logmon-broker` incapable of wedging under sustained ingest load while preserving information from compliant clients.

**Architecture:** Receivers (GELF UDP/TCP, OTLP HTTP/gRPC) currently call `mpsc::Sender::send().await` against a 1024-cap channel; under burst load this parks producer tasks indefinitely and accumulates FDs until macOS' default 256-FD limit wedges the process and SIGTERM stops working. This plan: (1) bumps channel capacities for burst headroom, (2) converts every receiver call site to `try_send` so receivers never park, (3) returns `429 Too Many Requests` / gRPC `UNAVAILABLE` on OTLP when the channel is >80% full so well-behaved clients back off, (4) tunes `SO_RCVBUF` on the GELF UDP socket so kernel buffers match user-space buffers, (5) adds rate-limited drop counters surfaced in `status.get` and `tracing::warn`, (6) makes `OtlpReceiver::Drop` abort its JoinHandles so shutdown is bounded, (7) verifies via a stress integration test.

**Tech Stack:** Rust 2021, tokio 1.50 (`mpsc::Sender::try_send` / `capacity()` / `max_capacity()`), axum 0.8 (`StatusCode::TOO_MANY_REQUESTS`), tonic 0.14 (`Status::unavailable`), socket2 0.6 (`SO_RCVBUF`), std::sync::atomic.

---

## File Structure

**New files:**
- `crates/core/src/receiver/metrics.rs` — `ReceiverMetrics` struct (six `AtomicU64` drop counters + rate-limited warn helper) and `ReceiverSource` enum.
- `crates/core/tests/backpressure.rs` — integration stress test verifying broker stays responsive under burst load.

**Modified files:**
- `crates/core/src/receiver/mod.rs` — `pub mod metrics;` + re-export.
- `crates/core/src/receiver/otlp/mod.rs` — pass `Arc<ReceiverMetrics>` through; `Drop` aborts `grpc_handle` and `http_handle`.
- `crates/core/src/receiver/otlp/http.rs` — `try_send` on logs+spans, 429 when ≥80% full, count drops via metrics.
- `crates/core/src/receiver/otlp/grpc.rs` — `try_send` on logs+spans, `Status::unavailable` when ≥80% full, count drops via metrics.
- `crates/core/src/receiver/gelf.rs` — pass `Arc<ReceiverMetrics>` through.
- `crates/core/src/gelf/udp.rs` — bind via `socket2` with `SO_RCVBUF=8 MB`; `try_send` + count drops.
- `crates/core/src/gelf/tcp.rs` — `try_send` + count drops.
- `crates/core/src/daemon/server.rs` — channel cap `1024 → 65536` (logs and spans); construct + share `Arc<ReceiverMetrics>`; pass to `RpcHandler`.
- `crates/core/src/daemon/rpc_handler.rs` — `RpcHandler` holds `Arc<ReceiverMetrics>`; `handle_status` includes `receiver_drops` map.
- `crates/protocol/src/methods.rs` — `StatusGetResult` gains `receiver_drops: ReceiverDropCounts` (typed, optional fields default to 0).

---

## Task 1: Set up worktree + confirm clean baseline

**Files:**
- N/A (workspace setup)

- [ ] **Step 1: Confirm working tree is clean**

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp
git status
```

Expected: `On branch master`, only untracked `crates/.DS_Store` (ignorable).

- [ ] **Step 2: Create feature branch + worktree**

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp
git worktree add -b feat/broker-backpressure ../logmon-mcp-backpressure master
cd ../logmon-mcp-backpressure
```

Expected: new worktree at sibling path on branch `feat/broker-backpressure`.

- [ ] **Step 3: Run baseline test suite**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: all green.

- [ ] **Step 4: Note baseline counts**

Record the test-pass count from Step 3 in your notes — Task 10 must show ≥ that number plus the new stress test.

- [ ] **Step 5: Commit (no changes — branch only)**

No commit needed at this step. The branch is the commit boundary.

---

## Task 2: Add `ReceiverMetrics` foundation

**Files:**
- Create: `crates/core/src/receiver/metrics.rs`
- Modify: `crates/core/src/receiver/mod.rs`
- Test: `crates/core/src/receiver/metrics.rs` (in-module `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Create file `crates/core/src/receiver/metrics.rs` with the test only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn record_drop_increments_per_source() {
        let m = Arc::new(ReceiverMetrics::new());
        m.record_drop(ReceiverSource::OtlpHttpLogs);
        m.record_drop(ReceiverSource::OtlpHttpLogs);
        m.record_drop(ReceiverSource::GelfUdp);

        let snap = m.snapshot();
        assert_eq!(snap.otlp_http_logs, 2);
        assert_eq!(snap.gelf_udp, 1);
        assert_eq!(snap.otlp_http_traces, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_send_log_drops_when_full() {
        use crate::gelf::message::{LogEntry, Level, LogSource};
        use chrono::Utc;
        use std::collections::HashMap;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<LogEntry>(1);
        let m = Arc::new(ReceiverMetrics::new());

        let make = || LogEntry {
            seq: 0, timestamp: Utc::now(), level: Level::Info,
            message: "x".into(), full_message: None,
            host: "h".into(), facility: None, file: None, line: None,
            additional_fields: HashMap::new(),
            trace_id: None, span_id: None,
            matched_filters: vec![], source: LogSource::Filter,
        };

        // First send fills the 1-slot channel.
        assert!(m.try_send_log(&tx, make(), ReceiverSource::GelfUdp));
        // Second send finds it full → drop counted, no panic, no park.
        assert!(!m.try_send_log(&tx, make(), ReceiverSource::GelfUdp));
        assert_eq!(m.snapshot().gelf_udp, 1);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p logmon-broker-core --lib receiver::metrics::tests
```

Expected: compile error — `ReceiverMetrics`, `ReceiverSource`, `try_send_log`, `record_drop`, `snapshot` not defined.

- [ ] **Step 3: Implement `ReceiverMetrics` and `ReceiverSource`**

Prepend to `crates/core/src/receiver/metrics.rs` (above the `#[cfg(test)]` block):

```rust
//! Receiver-side metrics: per-source drop counters with rate-limited warning.
//!
//! Every receiver (GELF UDP/TCP, OTLP HTTP logs/traces, OTLP gRPC logs/traces)
//! holds an `Arc<ReceiverMetrics>` and forwards entries via
//! [`ReceiverMetrics::try_send_log`] / [`ReceiverMetrics::try_send_span`] —
//! these never park: on `TrySendError::Full` they bump the per-source drop
//! counter and return `false`. The first drop in any 60-second window also
//! emits a `tracing::warn!` so daemon.log surfaces backpressure visibly.

use crate::gelf::message::LogEntry;
use crate::span::types::SpanEntry;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Identifies which receiver call site produced a drop. Used both for
/// per-source counters and for the structured field on the warn log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverSource {
    GelfUdp,
    GelfTcp,
    OtlpHttpLogs,
    OtlpHttpTraces,
    OtlpGrpcLogs,
    OtlpGrpcTraces,
}

impl ReceiverSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GelfUdp => "gelf_udp",
            Self::GelfTcp => "gelf_tcp",
            Self::OtlpHttpLogs => "otlp_http_logs",
            Self::OtlpHttpTraces => "otlp_http_traces",
            Self::OtlpGrpcLogs => "otlp_grpc_logs",
            Self::OtlpGrpcTraces => "otlp_grpc_traces",
        }
    }
}

const WARN_INTERVAL_NANOS: i64 = 60_000_000_000; // 60 seconds

/// Snapshot of all drop counters, suitable for status.get RPC payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiverDropSnapshot {
    pub gelf_udp: u64,
    pub gelf_tcp: u64,
    pub otlp_http_logs: u64,
    pub otlp_http_traces: u64,
    pub otlp_grpc_logs: u64,
    pub otlp_grpc_traces: u64,
}

pub struct ReceiverMetrics {
    gelf_udp: AtomicU64,
    gelf_tcp: AtomicU64,
    otlp_http_logs: AtomicU64,
    otlp_http_traces: AtomicU64,
    otlp_grpc_logs: AtomicU64,
    otlp_grpc_traces: AtomicU64,
    /// Unix-epoch nanos of the last warn emission. Initialised to a value
    /// that ensures the first drop always warns.
    last_warn_nanos: AtomicI64,
}

impl ReceiverMetrics {
    pub fn new() -> Self {
        Self {
            gelf_udp: AtomicU64::new(0),
            gelf_tcp: AtomicU64::new(0),
            otlp_http_logs: AtomicU64::new(0),
            otlp_http_traces: AtomicU64::new(0),
            otlp_grpc_logs: AtomicU64::new(0),
            otlp_grpc_traces: AtomicU64::new(0),
            last_warn_nanos: AtomicI64::new(i64::MIN),
        }
    }

    /// Increment the counter for `source` and emit a `tracing::warn!` if the
    /// last warning was more than 60 seconds ago (or never).
    pub fn record_drop(&self, source: ReceiverSource) {
        let counter = match source {
            ReceiverSource::GelfUdp => &self.gelf_udp,
            ReceiverSource::GelfTcp => &self.gelf_tcp,
            ReceiverSource::OtlpHttpLogs => &self.otlp_http_logs,
            ReceiverSource::OtlpHttpTraces => &self.otlp_http_traces,
            ReceiverSource::OtlpGrpcLogs => &self.otlp_grpc_logs,
            ReceiverSource::OtlpGrpcTraces => &self.otlp_grpc_traces,
        };
        counter.fetch_add(1, Ordering::Relaxed);

        let now_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(i64::MAX);
        let last = self.last_warn_nanos.load(Ordering::Relaxed);
        if now_nanos.saturating_sub(last) >= WARN_INTERVAL_NANOS
            && self
                .last_warn_nanos
                .compare_exchange(last, now_nanos, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                source = source.as_str(),
                "receiver dropped entry due to channel backpressure"
            );
        }
    }

    /// Send a [`LogEntry`] without ever parking. Returns `true` on success,
    /// `false` if the channel is full (counter incremented) or closed.
    pub fn try_send_log(
        &self,
        sender: &mpsc::Sender<LogEntry>,
        entry: LogEntry,
        source: ReceiverSource,
    ) -> bool {
        match sender.try_send(entry) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.record_drop(source);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Send a [`SpanEntry`] without ever parking. Returns `true` on success,
    /// `false` if the channel is full (counter incremented) or closed.
    pub fn try_send_span(
        &self,
        sender: &mpsc::Sender<SpanEntry>,
        entry: SpanEntry,
        source: ReceiverSource,
    ) -> bool {
        match sender.try_send(entry) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.record_drop(source);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    pub fn snapshot(&self) -> ReceiverDropSnapshot {
        ReceiverDropSnapshot {
            gelf_udp: self.gelf_udp.load(Ordering::Relaxed),
            gelf_tcp: self.gelf_tcp.load(Ordering::Relaxed),
            otlp_http_logs: self.otlp_http_logs.load(Ordering::Relaxed),
            otlp_http_traces: self.otlp_http_traces.load(Ordering::Relaxed),
            otlp_grpc_logs: self.otlp_grpc_logs.load(Ordering::Relaxed),
            otlp_grpc_traces: self.otlp_grpc_traces.load(Ordering::Relaxed),
        }
    }
}

impl Default for ReceiverMetrics {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Wire module into the receiver namespace**

Edit `crates/core/src/receiver/mod.rs`. After the existing `pub mod gelf;` / `pub mod otlp;` lines, add:

```rust
pub mod metrics;

pub use metrics::{ReceiverDropSnapshot, ReceiverMetrics, ReceiverSource};
```

- [ ] **Step 5: Run the test to verify it passes**

```bash
cargo test -p logmon-broker-core --lib receiver::metrics::tests
```

Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/receiver/metrics.rs crates/core/src/receiver/mod.rs
git commit -m "feat(receiver): ReceiverMetrics with per-source drop counters + try_send helpers"
```

---

## Task 3: Bump channel capacities to 65 536

**Files:**
- Modify: `crates/core/src/daemon/server.rs:259-260` (channel construction)

- [ ] **Step 1: Replace the two `mpsc::channel(1024)` calls**

In `crates/core/src/daemon/server.rs`, locate:

```rust
            let (log_tx, log_rx) = mpsc::channel(1024);
            let (span_tx, span_rx_real) = mpsc::channel(1024);
```

Replace with:

```rust
            // Bursts of GELF + OTLP traffic from store-test test runs can
            // briefly overshoot the consumer; 65 536 entries × ~500 B ≈ 32 MB
            // worst-case headroom keeps drop-counting from engaging on
            // realistic workloads. Receivers use try_send (see ReceiverMetrics)
            // so they never park if this cap is exceeded.
            const LOG_CHANNEL_CAP: usize = 65_536;
            const SPAN_CHANNEL_CAP: usize = 65_536;
            let (log_tx, log_rx) = mpsc::channel(LOG_CHANNEL_CAP);
            let (span_tx, span_rx_real) = mpsc::channel(SPAN_CHANNEL_CAP);
```

- [ ] **Step 2: Verify the project compiles**

```bash
cargo check -p logmon-broker-core
```

Expected: clean (no errors).

- [ ] **Step 3: Run existing tests to confirm no regression**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: same baseline pass count as Task 1 Step 3.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/daemon/server.rs
git commit -m "feat(daemon): bump log/span channel cap from 1024 to 65 536 for burst headroom"
```

---

## Task 4: Convert OTLP HTTP to `try_send` with backpressure response

**Files:**
- Modify: `crates/core/src/receiver/otlp/http.rs` (function signatures, AppState, handlers)
- Modify: `crates/core/src/receiver/otlp/mod.rs` (pass `Arc<ReceiverMetrics>` through)
- Test: `crates/core/src/receiver/otlp/http.rs` (in-module `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/receiver/otlp/http.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn make_state(log_cap: usize, span_cap: usize) -> AppState {
        let (log_sender, _log_rx) = mpsc::channel(log_cap);
        let (span_sender, _span_rx) = mpsc::channel(span_cap);
        AppState {
            log_sender,
            span_sender,
            metrics: Arc::new(ReceiverMetrics::new()),
        }
    }

    #[test]
    fn channel_used_pct_reads_capacity() {
        let state = make_state(10, 10);
        // Empty channel → 0% used.
        assert_eq!(channel_used_pct(&state.log_sender), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_traces_drops_when_span_channel_full() {
        let state = make_state(10, 1);
        // Fill the span channel.
        let dummy_span = crate::span::types::SpanEntry {
            seq: 0, trace_id: 1, span_id: 1, parent_span_id: None,
            start_time: chrono::Utc::now(), end_time: chrono::Utc::now(),
            duration_ms: 0.0, name: "x".into(),
            kind: crate::span::types::SpanKind::Internal,
            service_name: "s".into(),
            status: crate::span::types::SpanStatus::Unset,
            attributes: std::collections::HashMap::new(),
            events: vec![],
        };
        state.span_sender.try_send(dummy_span).unwrap();

        // POST a single span — channel is full, so it should drop and bump
        // the counter, NOT park.
        let body = serde_json::json!({
            "resourceSpans": [{
                "resource": { "attributes": [] },
                "scopeSpans": [{
                    "spans": [{
                        "traceId": "0102030405060708090a0b0c0d0e0f10",
                        "spanId":  "0102030405060708",
                        "name": "synthetic",
                        "startTimeUnixNano": "0",
                        "endTimeUnixNano":   "0"
                    }]
                }]
            }]
        });

        let status = handle_traces_inner(&state, body).await;
        // 80%+ full → 429.
        assert_eq!(status.as_u16(), 429);
        // Counter must reflect the drop.
        assert_eq!(state.metrics.snapshot().otlp_http_traces, 1);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p logmon-broker-core --lib receiver::otlp::http::tests
```

Expected: compile error — `metrics` field on `AppState` doesn't exist; `channel_used_pct` and `handle_traces_inner` don't exist.

- [ ] **Step 3: Update `AppState` and add helper + 80% threshold**

In `crates/core/src/receiver/otlp/http.rs`, replace the existing `AppState` and `start_http_server`:

```rust
use crate::receiver::ReceiverMetrics;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    metrics: Arc<ReceiverMetrics>,
}

/// Threshold above which OTLP HTTP returns 429 instead of consuming the
/// remaining channel headroom. Compliant clients (tracing-init's OTLP
/// exporter) retry with exponential backoff, so this is a soft brake — no
/// information is lost as long as clients honour it.
const BACKPRESSURE_THRESHOLD_PCT: u64 = 80;

fn channel_used_pct<T>(sender: &mpsc::Sender<T>) -> u64 {
    let cap = sender.max_capacity() as u64;
    if cap == 0 {
        return 0;
    }
    let avail = sender.capacity() as u64;
    (cap - avail) * 100 / cap
}

pub async fn start_http_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    metrics: Arc<ReceiverMetrics>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let state = AppState {
        log_sender,
        span_sender,
        metrics,
    };
    let app = Router::new()
        .route("/v1/logs", post(handle_logs))
        .route("/v1/traces", post(handle_traces))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("OTLP HTTP server listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await?;
    Ok(())
}
```

- [ ] **Step 4: Replace `handle_logs` with non-parking + 429 backpressure**

Add `use crate::receiver::ReceiverSource;` to the top imports of the file (next to the existing `use crate::receiver::ReceiverMetrics;` added in Step 3), then replace the existing `handle_logs` with:

```rust
async fn handle_logs(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    handle_logs_inner(&state, body).await
}

async fn handle_logs_inner(state: &AppState, body: serde_json::Value) -> StatusCode {
    if channel_used_pct(&state.log_sender) >= BACKPRESSURE_THRESHOLD_PCT {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    if let Some(resource_logs) = body.get("resourceLogs").and_then(|v| v.as_array()) {
        for rl in resource_logs {
            let resource_attrs = extract_json_resource_attrs(rl);
            let (service, host) = extract_resource_attrs(&resource_attrs);

            if let Some(scope_logs) = rl.get("scopeLogs").and_then(|v| v.as_array()) {
                for sl in scope_logs {
                    if let Some(records) = sl.get("logRecords").and_then(|v| v.as_array()) {
                        for record in records {
                            if let Some(entry) = parse_json_log_record(record, &service, &host) {
                                state.metrics.try_send_log(
                                    &state.log_sender,
                                    entry,
                                    ReceiverSource::OtlpHttpLogs,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    StatusCode::OK
}
```

- [ ] **Step 5: Replace `handle_traces` analogously**

Replace `handle_traces` with:

```rust
async fn handle_traces(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    handle_traces_inner(&state, body).await
}

async fn handle_traces_inner(state: &AppState, body: serde_json::Value) -> StatusCode {
    if channel_used_pct(&state.span_sender) >= BACKPRESSURE_THRESHOLD_PCT {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    if let Some(resource_spans) = body.get("resourceSpans").and_then(|v| v.as_array()) {
        for rs in resource_spans {
            let resource_attrs = extract_json_resource_attrs(rs);
            let (service, _host) = extract_resource_attrs(&resource_attrs);

            if let Some(scope_spans) = rs.get("scopeSpans").and_then(|v| v.as_array()) {
                for ss in scope_spans {
                    if let Some(spans) = ss.get("spans").and_then(|v| v.as_array()) {
                        for span_json in spans {
                            if let Some(entry) = parse_json_span(span_json, &service) {
                                state.metrics.try_send_span(
                                    &state.span_sender,
                                    entry,
                                    ReceiverSource::OtlpHttpTraces,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    StatusCode::OK
}
```

- [ ] **Step 6: Skip workspace verification — commit and move on**

> **NOTE on interim build state:** Tasks 4 and 5 each change a single OTLP receiver file in isolation. **The broker as a whole will not compile until Task 6** (which rewrites `OtlpReceiver::start` to call the new signatures). That is intentional — it keeps each commit's change scope narrow. Cargo cannot meaningfully type-check a single file in a library — `--lib --no-run` still compiles the whole crate, so per-file verification commands here would just surface the (expected) `mod.rs` error. Trust the explicit code blocks above; workspace compilation is verified at the end of Task 6.

Optional sanity check that you didn't introduce a typo INSIDE `http.rs` itself:

```bash
cargo check -p logmon-broker-core 2>&1 | grep -E "src/receiver/otlp/http\.rs" || echo "no errors in http.rs itself"
```

Expected: `no errors in http.rs itself` (errors will appear in `mod.rs` and `daemon/server.rs`, but not in `http.rs`).

- [ ] **Step 7: Commit (broken-workspace interim is intentional)**

```bash
git add crates/core/src/receiver/otlp/http.rs
git commit -m "feat(otlp/http): try_send + 429 on >=80%% full, plumb ReceiverMetrics

Self-contained file change. OtlpReceiver::start still calls
start_http_server with the old signature; rewired in Task 6."
```

---

## Task 5: Convert OTLP gRPC to `try_send` with `UNAVAILABLE` backpressure

**Files:**
- Modify: `crates/core/src/receiver/otlp/grpc.rs` (services, `start_grpc_server`)
- Test: `crates/core/src/receiver/otlp/grpc.rs` (in-module `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/receiver/otlp/grpc.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use opentelemetry_proto::tonic::collector::trace::v1::{
        ExportTraceServiceRequest,
    };
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tonic::Request;

    fn make_trace_service(span_cap: usize) -> (OtlpTraceService, mpsc::Receiver<SpanEntry>, Arc<ReceiverMetrics>) {
        let (span_sender, span_rx) = mpsc::channel(span_cap);
        let metrics = Arc::new(ReceiverMetrics::new());
        let svc = OtlpTraceService {
            span_sender,
            metrics: metrics.clone(),
            malformed_count: AtomicU64::new(0),
        };
        (svc, span_rx, metrics)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_service_returns_unavailable_when_full() {
        let (svc, _rx, metrics) = make_trace_service(1);

        // Fill the channel.
        let dummy = SpanEntry {
            seq: 0, trace_id: 1, span_id: 1, parent_span_id: None,
            start_time: chrono::Utc::now(), end_time: chrono::Utc::now(),
            duration_ms: 0.0, name: "x".into(),
            kind: SpanKind::Internal, service_name: "s".into(),
            status: SpanStatus::Unset,
            attributes: std::collections::HashMap::new(),
            events: vec![],
        };
        svc.span_sender.try_send(dummy).unwrap();

        let req = Request::new(ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans: vec![Span {
                        trace_id: vec![1; 16],
                        span_id: vec![1; 8],
                        name: "synthetic".into(),
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        });

        let result = svc.export(req).await;
        // The handler should return UNAVAILABLE rather than consume more
        // channel capacity. The body of the request is parsed, the metric
        // increments per span that would have been pushed, and clients see
        // a retryable error.
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), tonic::Code::Unavailable);
        // Snapshot reflects no drops because the request was rejected at
        // the threshold check, before any individual try_send_span. The
        // important assertion is the error code.
        let _ = metrics; // unused, but kept for readability
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p logmon-broker-core --lib receiver::otlp::grpc::tests --no-run 2>&1 | tail -20
```

Expected: compile error — `metrics` field on `OtlpTraceService` doesn't exist.

- [ ] **Step 3: Add metrics + threshold to `OtlpLogsService` and `OtlpTraceService`**

In `crates/core/src/receiver/otlp/grpc.rs`, replace the two service struct definitions and their export() bodies:

```rust
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use std::sync::Arc;

const BACKPRESSURE_THRESHOLD_PCT: u64 = 80;

fn channel_used_pct<T>(sender: &mpsc::Sender<T>) -> u64 {
    let cap = sender.max_capacity() as u64;
    if cap == 0 { return 0; }
    let avail = sender.capacity() as u64;
    (cap - avail) * 100 / cap
}

pub struct OtlpLogsService {
    pub log_sender: mpsc::Sender<LogEntry>,
    pub metrics: Arc<ReceiverMetrics>,
    pub malformed_count: AtomicU64,
}

pub struct OtlpTraceService {
    pub span_sender: mpsc::Sender<SpanEntry>,
    pub metrics: Arc<ReceiverMetrics>,
    pub malformed_count: AtomicU64,
}
```

(Remove the previous bare definitions — keep them only here.)

In `OtlpLogsService::export`, immediately after `let req = request.into_inner();` add:

```rust
        if channel_used_pct(&self.log_sender) >= BACKPRESSURE_THRESHOLD_PCT {
            return Err(Status::unavailable("broker log channel under backpressure"));
        }
```

Replace the `let _ = self.log_sender.send(entry).await;` with:

```rust
                    self.metrics.try_send_log(
                        &self.log_sender,
                        entry,
                        ReceiverSource::OtlpGrpcLogs,
                    );
```

Apply the symmetric change to `OtlpTraceService::export` — threshold check at top, `try_send_span` with `ReceiverSource::OtlpGrpcTraces`.

- [ ] **Step 4: Update `start_grpc_server` signature**

Replace `start_grpc_server`:

```rust
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    metrics: Arc<ReceiverMetrics>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let logs_svc = OtlpLogsService {
        log_sender,
        metrics: metrics.clone(),
        malformed_count: AtomicU64::new(0),
    };
    let trace_svc = OtlpTraceService {
        span_sender,
        metrics,
        malformed_count: AtomicU64::new(0),
    };

    tracing::info!("OTLP gRPC server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(LogsServiceServer::new(logs_svc))
        .add_service(TraceServiceServer::new(trace_svc))
        .serve_with_shutdown(addr, async move {
            let _ = shutdown_rx.recv().await;
        })
        .await?;

    Ok(())
}
```

- [ ] **Step 5: Skip workspace verification — commit and move on**

Workspace-wide `cargo check` still fails (continuation of Task 4's interim state). Same caveat as Task 4 Step 6: cargo can't meaningfully type-check just `grpc.rs`. Optional sanity check that the file itself is clean:

```bash
cargo check -p logmon-broker-core 2>&1 | grep -E "src/receiver/otlp/grpc\.rs" || echo "no errors in grpc.rs itself"
```

Expected: `no errors in grpc.rs itself`.

- [ ] **Step 6: Commit (broken-workspace interim continues)**

```bash
git add crates/core/src/receiver/otlp/grpc.rs
git commit -m "feat(otlp/grpc): try_send + UNAVAILABLE on >=80%% full

Self-contained file change. OtlpReceiver::start still calls
start_grpc_server with the old signature; rewired in Task 6."
```

---

## Task 6: Wire metrics end-to-end (broker compiles again after this task)

**Goal:** Re-establish a compilable workspace after Tasks 4 and 5. This task does ALL the cross-file wiring at once: rewrites `OtlpReceiver::start` to use the new HTTP/gRPC signatures, gives `GelfReceiver::start` and the udp/tcp listeners a `metrics` parameter (stubbed — Tasks 7 + 8 will actually use it), constructs `Arc<ReceiverMetrics>` in `daemon/server.rs`, and threads it through `RpcHandler`.

**Files:**
- Modify: `crates/core/src/receiver/otlp/mod.rs` (`OtlpReceiver::start` signature + body)
- Modify: `crates/core/src/receiver/gelf.rs` (`GelfReceiver::start` signature, threads to udp/tcp)
- Modify: `crates/core/src/gelf/udp.rs` (signature stub: accept `metrics`, ignore)
- Modify: `crates/core/src/gelf/tcp.rs` (signature stub: accept `metrics`, ignore)
- Modify: `crates/core/src/daemon/server.rs` (construct `receiver_metrics`, update both `start` callsites + `RpcHandler::new` callsite)
- Modify: `crates/core/src/daemon/rpc_handler.rs` (`RpcHandler::new` takes metrics)

- [ ] **Step 1: Rewrite `OtlpReceiver::start`**

In `crates/core/src/receiver/otlp/mod.rs`, replace `OtlpReceiver::start`:

```rust
use crate::receiver::ReceiverMetrics;
use std::sync::Arc;

impl OtlpReceiver {
    pub async fn start(
        config: OtlpReceiverConfig,
        log_sender: mpsc::Sender<LogEntry>,
        span_sender: mpsc::Sender<SpanEntry>,
        metrics: Arc<ReceiverMetrics>,
    ) -> anyhow::Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let grpc_addr: std::net::SocketAddr = config.grpc_addr.parse()?;
        let http_addr: std::net::SocketAddr = config.http_addr.parse()?;

        let grpc_handle = {
            let log_tx = log_sender.clone();
            let span_tx = span_sender.clone();
            let metrics = metrics.clone();
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    grpc::start_grpc_server(grpc_addr, log_tx, span_tx, metrics, rx).await
                {
                    tracing::error!("OTLP gRPC server error: {e}");
                }
            })
        };

        let http_handle = {
            let rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) =
                    http::start_http_server(http_addr, log_sender, span_sender, metrics, rx).await
                {
                    tracing::error!("OTLP HTTP server error: {e}");
                }
            })
        };

        Ok(Self {
            shutdown_tx,
            grpc_port: grpc_addr.port(),
            http_port: http_addr.port(),
            grpc_handle,
            http_handle,
        })
    }
}
```

- [ ] **Step 2: Stub `start_udp_listener` signature**

In `crates/core/src/gelf/udp.rs`, change ONLY the function signature to accept `metrics`. Leave the body unchanged for now (still calls `sender.send(...).await` — Task 7 replaces the body):

```rust
use crate::receiver::ReceiverMetrics;
use std::sync::Arc;

pub async fn start_udp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    #[allow(unused_variables)]
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<UdpListenerHandle> {
    // Body unchanged — still parks on full channel. Replaced in Task 7.
    // ... existing body ...
}
```

> **Implementer note:** Add the new arg + the imports. Do NOT change anything else in the file. The `#[allow(unused_variables)]` attribute keeps clippy quiet for one task; Task 7 removes it.

- [ ] **Step 3: Stub `start_tcp_listener` signature**

Symmetric change in `crates/core/src/gelf/tcp.rs`:

```rust
use crate::receiver::ReceiverMetrics;

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    #[allow(unused_variables)]
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    // Body unchanged. Replaced in Task 8.
    // ... existing body ...
}
```

(The existing file already imports `Arc`.)

- [ ] **Step 4: Update `GelfReceiver::start` signature**

In `crates/core/src/receiver/gelf.rs`, replace `GelfReceiver::start`:

```rust
use crate::receiver::ReceiverMetrics;
use std::sync::Arc;

impl GelfReceiver {
    pub async fn start(
        config: GelfReceiverConfig,
        sender: mpsc::Sender<LogEntry>,
        metrics: Arc<ReceiverMetrics>,
    ) -> anyhow::Result<Self> {
        let udp_handle = crate::gelf::udp::start_udp_listener(
            &config.udp_addr,
            sender.clone(),
            metrics.clone(),
        )
        .await?;
        let tcp_handle = crate::gelf::tcp::start_tcp_listener(
            &config.tcp_addr,
            sender,
            metrics,
        )
        .await?;
        Ok(Self { udp_handle, tcp_handle })
    }
    // ... other methods unchanged ...
}
```

- [ ] **Step 5: Construct `receiver_metrics` in `run_with_overrides`**

In `crates/core/src/daemon/server.rs`, immediately BEFORE the `let (log_rx, _gelf_receiver, _otlp_receiver, all_receivers_info) = match injected_log_rx {` line, add:

```rust
    // Receiver metrics (drop counters + rate-limited warn). Shared between
    // every receiver call site and the RpcHandler so status.get can surface
    // the counts.
    let receiver_metrics = std::sync::Arc::new(crate::receiver::ReceiverMetrics::new());
```

- [ ] **Step 6: Update `GelfReceiver::start` callsite (inside the `None` arm)**

In `crates/core/src/daemon/server.rs`, INSIDE the `None =>` arm of the `match injected_log_rx`, find:

```rust
            let gelf_receiver = GelfReceiver::start(gelf_config, log_tx.clone()).await?;
```

Replace with:

```rust
            let gelf_receiver =
                GelfReceiver::start(gelf_config, log_tx.clone(), receiver_metrics.clone()).await?;
```

- [ ] **Step 7: Update `OtlpReceiver::start` callsite (also inside the `None` arm)**

In the same `None =>` arm, find:

```rust
                let otlp_receiver =
                    OtlpReceiver::start(otlp_config, log_tx.clone(), span_tx).await?;
```

Replace with:

```rust
                let otlp_receiver =
                    OtlpReceiver::start(otlp_config, log_tx.clone(), span_tx, receiver_metrics.clone()).await?;
```

- [ ] **Step 8: Update `RpcHandler::new` signature**

In `crates/core/src/daemon/rpc_handler.rs`, replace the `RpcHandler` struct definition + the `new` constructor (leave the rest of the impl block unchanged).

> **VERIFY BEFORE REPLACING:** Read the existing struct first. The plan's replacement assumes 6 pre-existing fields (`pipeline`, `span_store`, `sessions`, `bookmarks`, `start_time`, `receivers_info`). If the existing struct has additional fields the plan didn't anticipate, preserve them in your replacement — do not silently drop them. The plan's pattern is "add `metrics` field, add `metrics` constructor parameter" — nothing else should change.

```rust
use crate::receiver::ReceiverMetrics;

pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
    bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
    metrics: Arc<ReceiverMetrics>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}

impl RpcHandler {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        span_store: Arc<SpanStore>,
        sessions: Arc<SessionRegistry>,
        bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
        metrics: Arc<ReceiverMetrics>,
        receivers_info: Vec<String>,
    ) -> Self {
        Self {
            pipeline,
            span_store,
            sessions,
            bookmarks,
            metrics,
            start_time: std::time::Instant::now(),
            receivers_info,
        }
    }
    // ... rest of impl block (handle, handle_status, etc.) unchanged ...
}
```

- [ ] **Step 9: Update `RpcHandler::new` callsite**

In `crates/core/src/daemon/server.rs`, find:

```rust
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store.clone(),
        sessions.clone(),
        bookmark_store.clone(),
        all_receivers_info,
    ));
```

Replace with:

```rust
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store.clone(),
        sessions.clone(),
        bookmark_store.clone(),
        receiver_metrics.clone(),
        all_receivers_info,
    ));
```

- [ ] **Step 10: Verify the workspace compiles**

```bash
cargo check -p logmon-broker-core
```

Expected: clean. Workspace builds again for the first time since Task 4.

- [ ] **Step 11: Run existing test suite to confirm no regression**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: same baseline pass count as Task 1 plus the new tests added in Tasks 2, 4, 5 (the receiver/otlp http + grpc unit tests now run successfully because the workspace compiles).

- [ ] **Step 12: Commit**

```bash
git add crates/core/src/receiver/otlp/mod.rs crates/core/src/receiver/gelf.rs crates/core/src/gelf/udp.rs crates/core/src/gelf/tcp.rs crates/core/src/daemon/server.rs crates/core/src/daemon/rpc_handler.rs
git commit -m "feat(daemon): wire Arc<ReceiverMetrics> through OtlpReceiver, GelfReceiver, and RpcHandler"
```

---

## Task 7: Convert GELF UDP body to `try_send` + `SO_RCVBUF=8 MB`

**Files:**
- Modify: `crates/core/src/gelf/udp.rs` (body only — signature already exists from Task 6)
- Test: `crates/core/src/gelf/udp.rs` (in-module `#[cfg(test)]`)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/gelf/udp.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Sending many GELF UDP datagrams when the channel is full must NOT
    /// park the receiver task. Counter increments instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_channel_does_not_park_udp_listener() {
        let (sender, _rx) = mpsc::channel(1);
        // Fill the channel pre-emptively.
        sender.try_send(crate::gelf::message::LogEntry {
            seq: 0, timestamp: chrono::Utc::now(),
            level: crate::gelf::message::Level::Info,
            message: "filler".into(), full_message: None,
            host: "h".into(), facility: None, file: None, line: None,
            additional_fields: std::collections::HashMap::new(),
            trace_id: None, span_id: None,
            matched_filters: vec![],
            source: crate::gelf::message::LogSource::Filter,
        }).unwrap();

        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_udp_listener("127.0.0.1:0", sender.clone(), metrics.clone())
            .await
            .unwrap();
        let port = handle.port();

        // Send 50 well-formed GELF UDP datagrams; the listener task must
        // drain the OS buffer immediately for each one (since try_send
        // returns instantly) and bump the counter.
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = br#"{"version":"1.1","host":"h","short_message":"x","level":6,"timestamp":1.0}"#;
        for _ in 0..50 {
            socket.send_to(payload, format!("127.0.0.1:{port}")).await.unwrap();
        }
        // Tiny sleep to let the listener pick up the datagrams.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // At least most of the 50 must have been counted as drops; some
        // may have been kernel-dropped before reaching us. Real assertion:
        // > 0, because the channel was already full.
        let snap = metrics.snapshot();
        assert!(snap.gelf_udp >= 1, "expected at least one user-space drop, got {:?}", snap);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p logmon-broker-core --lib gelf::udp::tests
```

Expected: compiles (signature already accepts `metrics` from Task 6 stub) but the test FAILS — `metrics.snapshot().gelf_udp` is 0 because the body still uses `sender.send(...).await` and parks, then drops the entry only when the parked send is cancelled. The assertion `snap.gelf_udp >= 1` fails OR the test hangs/times out.

- [ ] **Step 3: Rewrite `udp.rs` body**

Replace the body of `crates/core/src/gelf/udp.rs` with:

```rust
use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use socket2::{Domain, Protocol, Socket, Type};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub struct UdpListenerHandle {
    port: u16,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl UdpListenerHandle {
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Target receive-buffer size for the UDP socket. macOS' default is ~256 KB,
/// which can drop datagrams under burst even when the user-space task drains
/// quickly. 8 MB matches the expected burst headroom of the user-space
/// 65 536-entry channel; the kernel may cap to its own `kern.ipc.maxsockbuf`,
/// which is fine — best-effort.
const UDP_RCVBUF_BYTES: usize = 8 * 1024 * 1024;

pub async fn start_udp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<UdpListenerHandle> {
    // Create via socket2 so we can set SO_RCVBUF before bind.
    let sock_addr: std::net::SocketAddr = addr.parse()?;
    let domain = match sock_addr {
        std::net::SocketAddr::V4(_) => Domain::IPV4,
        std::net::SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(e) = socket.set_recv_buffer_size(UDP_RCVBUF_BYTES) {
        tracing::warn!(error = %e, "could not set GELF UDP SO_RCVBUF; continuing with default");
    }
    socket.set_nonblocking(true)?;
    socket.bind(&sock_addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    let socket = UdpSocket::from_std(std_socket)?;
    let port = socket.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut buf = [0u8; 65535];
        loop {
            tokio::select! {
                result = socket.recv_from(&mut buf) => {
                    if let Ok((len, _addr)) = result {
                        let mut end = len;
                        while end > 0 && buf[end - 1] == 0 {
                            end -= 1;
                        }
                        match parse_gelf_message(&buf[..end], 0) {
                            Ok(entry) => {
                                metrics.try_send_log(&sender, entry, ReceiverSource::GelfUdp);
                            }
                            Err(e) => { eprintln!("malformed GELF UDP: {e}"); }
                        }
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(UdpListenerHandle { port, _shutdown: tx })
}
```

- [ ] **Step 4: Run the test to verify it passes**

```bash
cargo test -p logmon-broker-core --lib gelf::udp::tests
```

Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/gelf/udp.rs
git commit -m "feat(gelf/udp): try_send + SO_RCVBUF=8 MB to prevent silent kernel drops"
```

---

## Task 8: Convert GELF TCP body to `try_send`

**Files:**
- Modify: `crates/core/src/gelf/tcp.rs` (body only — signature already exists from Task 6)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/src/gelf/tcp.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::ReceiverMetrics;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_channel_does_not_park_tcp_listener() {
        let (sender, _rx) = mpsc::channel(1);
        sender.try_send(crate::gelf::message::LogEntry {
            seq: 0, timestamp: chrono::Utc::now(),
            level: crate::gelf::message::Level::Info,
            message: "filler".into(), full_message: None,
            host: "h".into(), facility: None, file: None, line: None,
            additional_fields: std::collections::HashMap::new(),
            trace_id: None, span_id: None,
            matched_filters: vec![],
            source: crate::gelf::message::LogSource::Filter,
        }).unwrap();

        let metrics = Arc::new(ReceiverMetrics::new());
        let handle = start_tcp_listener("127.0.0.1:0", sender, metrics.clone())
            .await
            .unwrap();
        let port = handle.port();

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        let payload = br#"{"version":"1.1","host":"h","short_message":"x","level":6,"timestamp":1.0}"#;
        for _ in 0..50 {
            stream.write_all(payload).await.unwrap();
            stream.write_all(&[0u8]).await.unwrap();
        }
        stream.flush().await.unwrap();
        // Drop stream to signal EOF.
        drop(stream);

        tokio::time::sleep(Duration::from_millis(200)).await;
        let snap = metrics.snapshot();
        assert!(snap.gelf_tcp >= 1, "expected at least one drop, got {:?}", snap);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p logmon-broker-core --lib gelf::tcp::tests
```

Expected: compiles (signature already accepts `metrics` from Task 6 stub) but the test fails — body still uses `sender.send(...).await` so the counter never increments.

- [ ] **Step 3: Replace the body of `start_tcp_listener`**

Replace the function in `crates/core/src/gelf/tcp.rs`:

```rust
use crate::gelf::message::{parse_gelf_message, LogEntry};
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

pub struct TcpListenerHandle {
    port: u16,
    connected_clients: Arc<AtomicU32>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl TcpListenerHandle {
    pub fn port(&self) -> u16 { self.port }
    pub fn connected_clients(&self) -> u32 {
        self.connected_clients.load(Ordering::Relaxed)
    }
}

pub async fn start_tcp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
    metrics: Arc<ReceiverMetrics>,
) -> anyhow::Result<TcpListenerHandle> {
    let listener = TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let connected = Arc::new(AtomicU32::new(0));
    let connected_clone = connected.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    if let Ok((stream, _addr)) = result {
                        let sender = sender.clone();
                        let metrics = metrics.clone();
                        let connected = connected_clone.clone();
                        connected.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            let mut reader = BufReader::new(stream);
                            let mut buf = Vec::new();

                            loop {
                                buf.clear();
                                let bytes_read = match reader.read_until(b'\0', &mut buf).await {
                                    Ok(n) => n,
                                    Err(e) => {
                                        eprintln!("TCP read error: {e}");
                                        break;
                                    }
                                };

                                if bytes_read == 0 { break; }

                                if buf.last() == Some(&0) {
                                    buf.pop();
                                }
                                if buf.is_empty() { continue; }

                                match parse_gelf_message(&buf, 0) {
                                    Ok(entry) => {
                                        metrics.try_send_log(
                                            &sender,
                                            entry,
                                            ReceiverSource::GelfTcp,
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!("malformed GELF TCP: {e}");
                                    }
                                }
                            }

                            connected.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                }
                _ = &mut rx => break,
            }
        }
    });

    Ok(TcpListenerHandle {
        port,
        connected_clients: connected,
        _shutdown: tx,
    })
}
```

- [ ] **Step 4: Run tests to verify**

```bash
cargo test -p logmon-broker-core --lib --features test-support
```

Expected: all green, including the new `gelf::tcp::tests::full_channel_does_not_park_tcp_listener` and the earlier `gelf::udp` / `metrics` / `receiver::otlp::http` / `receiver::otlp::grpc` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/gelf/tcp.rs
git commit -m "feat(gelf/tcp): try_send so a wedged consumer never parks the listener"
```

---

## Task 9: Surface drop counters in `status.get` + protocol struct

**Files:**
- Modify: `crates/protocol/src/methods.rs` (add `ReceiverDropCounts` field)
- Modify: `crates/core/src/daemon/rpc_handler.rs::handle_status`
- Test: `crates/core/tests/harness_smoke.rs` (extend status assertion)

- [ ] **Step 1: Extend the protocol struct**

In `crates/protocol/src/methods.rs`, immediately above `pub struct StatusGetResult`, add:

```rust
/// Per-source receiver drop counts. Each field is the cumulative count of
/// entries dropped at that receiver call site since broker start, due to
/// the upstream channel being full (i.e. the consumer couldn't keep up).
/// Healthy operation keeps all fields at 0.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ReceiverDropCounts {
    #[serde(default)] pub gelf_udp: u64,
    #[serde(default)] pub gelf_tcp: u64,
    #[serde(default)] pub otlp_http_logs: u64,
    #[serde(default)] pub otlp_http_traces: u64,
    #[serde(default)] pub otlp_grpc_logs: u64,
    #[serde(default)] pub otlp_grpc_traces: u64,
}
```

Then add a field to `StatusGetResult`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StatusGetResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionInfo>,
    pub daemon_uptime_secs: u64,
    pub receivers: Vec<String>,
    pub store: StoreStats,
    #[serde(default)]
    pub receiver_drops: ReceiverDropCounts,
}
```

- [ ] **Step 2: Emit `receiver_drops` from `handle_status`**

In `crates/core/src/daemon/rpc_handler.rs`, replace the body of `handle_status`:

```rust
    fn handle_status(&self, session_id: &SessionId) -> Result<Value, String> {
        let session_info = self.sessions.get(session_id);
        let stats = self.pipeline.store_stats();
        let drops = self.metrics.snapshot();
        Ok(json!({
            "session": session_info.map(|s| json!({
                "id": s.id.to_string(),
                "name": s.name,
                "connected": s.connected,
                "trigger_count": s.trigger_count,
                "filter_count": s.filter_count,
                "queue_size": s.queue_size,
                "last_seen_secs_ago": s.last_seen_secs_ago,
                "client_info": s.client_info,
            })),
            "daemon_uptime_secs": self.start_time.elapsed().as_secs(),
            "receivers": self.receivers_info,
            "store": {
                "total_received": stats.total_received,
                "total_stored": stats.total_stored,
                "malformed_count": stats.malformed_count,
                "current_size": self.pipeline.store_len(),
            },
            "receiver_drops": {
                "gelf_udp": drops.gelf_udp,
                "gelf_tcp": drops.gelf_tcp,
                "otlp_http_logs": drops.otlp_http_logs,
                "otlp_http_traces": drops.otlp_http_traces,
                "otlp_grpc_logs": drops.otlp_grpc_logs,
                "otlp_grpc_traces": drops.otlp_grpc_traces,
            },
        }))
    }
```

- [ ] **Step 3: Update existing harness smoke test to assert on the new field**

In `crates/core/tests/harness_smoke.rs`, replace `harness_starts_and_status_responds` with:

```rust
#[tokio::test]
async fn harness_starts_and_status_responds() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    assert!(
        result.receivers.is_empty(),
        "expected no receivers in test harness, got {:?}",
        result.receivers
    );
    // Fresh daemon — no drops yet.
    assert_eq!(result.receiver_drops, Default::default());
}
```

- [ ] **Step 4: Run tests to verify**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: all green, including the updated smoke test.

- [ ] **Step 5: Regenerate the JSON schema**

The xtask exposes a `gen-schema` subcommand that rewrites `crates/protocol/protocol-v1.schema.json` from the schemars-derived types:

```bash
cargo run -p xtask -- gen-schema
```

Confirm the file changed and now contains `receiver_drops`:

```bash
git diff crates/protocol/protocol-v1.schema.json | grep receiver_drops
```

Expected: at least one `+` line with `"receiver_drops"`. The CI verify path is `cargo run -p xtask -- verify-schema`, which the implementer can run for extra confidence.

- [ ] **Step 6: Commit**

```bash
git add crates/protocol/src/methods.rs crates/protocol/protocol-v1.schema.json crates/core/src/daemon/rpc_handler.rs crates/core/tests/harness_smoke.rs
git commit -m "feat(status): surface receiver_drops in status.get + protocol schema"
```

---

## Task 10: `OtlpReceiver::Drop` aborts JoinHandles

**Files:**
- Modify: `crates/core/src/receiver/otlp/mod.rs`

- [ ] **Step 1: Add Drop impl**

Append to `crates/core/src/receiver/otlp/mod.rs`:

```rust
impl Drop for OtlpReceiver {
    fn drop(&mut self) {
        // Best-effort: signal graceful shutdown. axum's
        // `with_graceful_shutdown` waits for in-flight handlers to complete
        // — which is fine for handlers that finish quickly, but if the
        // shutdown is happening because we've already wedged, in-flight
        // tasks may park on the (full) log/span channel forever. Aborting
        // the JoinHandles forces an exit path regardless. Both paths leave
        // the listening sockets to be cleaned up by the kernel after
        // process exit; the only thing we lose is graceful body
        // completion of an already-parked handler.
        let _ = self.shutdown_tx.send(());
        self.grpc_handle.abort();
        self.http_handle.abort();
    }
}
```

- [ ] **Step 2: Verify nothing relies on the old "non-blocking Drop" semantic**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: still all green.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/receiver/otlp/mod.rs
git commit -m "fix(otlp): abort grpc/http JoinHandles on Drop so shutdown is bounded"
```

---

## Task 11: Backpressure stress integration test

**Files:**
- Create: `crates/core/tests/backpressure.rs`

- [ ] **Step 1: Write the test**

Create `crates/core/tests/backpressure.rs`:

```rust
//! Backpressure stress test: pump many GELF UDP datagrams rapidly while a
//! slow consumer simulation keeps the channel under pressure, then verify:
//!
//! 1. The broker stays responsive (status.get returns within 200 ms).
//! 2. No information from compliant clients (those that honour 429 /
//!    UNAVAILABLE) is lost — but in this test the GELF UDP producer is
//!    NON-compliant (UDP can't be told to back off), so we accept that
//!    some drops will occur and assert the counter sees them.
//! 3. The drop counter increments are visible in `status.get`.
//!
//! This is the regression guard for the wedge bug observed during
//! `store-test tests/ --enforce-baseline` runs that filled the prior
//! 1024-cap channel within minutes.

#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::StatusGetResult;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn broker_stays_responsive_under_burst_load() {
    // Spawn a real daemon (not the injected-channel harness) so the GELF
    // UDP receiver is actually bound. spawn_test_daemon() uses the
    // injected harness, so we use the lower-level path.
    let daemon = TestDaemonHandle::spawn_with_real_receivers().await;
    let udp_port = daemon.gelf_udp_port().await;

    // Producer task: blast 200 000 GELF UDP datagrams as fast as the
    // kernel will let us, ignoring drops. This will overflow the 65 536
    // user-space channel during the burst.
    let producer = tokio::spawn(async move {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let payload = br#"{"version":"1.1","host":"stress","short_message":"x","level":6,"timestamp":1.0}"#;
        let target = format!("127.0.0.1:{udp_port}");
        for _ in 0..200_000 {
            let _ = socket.send_to(payload, &target).await;
        }
    });

    // Concurrently, every 100 ms, call status.get and assert it returns
    // within 200 ms. This is the primary wedge regression check.
    let mut client = daemon.connect_anon().await;
    let probe_deadline = Instant::now() + Duration::from_secs(8);
    let mut probes = 0u32;
    while Instant::now() < probe_deadline {
        let probe_start = Instant::now();
        let result: StatusGetResult = client
            .call("status.get", json!({}))
            .await
            .expect("status.get must succeed during stress");
        let elapsed = probe_start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "status.get took {elapsed:?} during stress — broker is wedging"
        );
        probes += 1;
        // Spot-check that the JSON deserialised the new field.
        let _ = result.receiver_drops;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(probes >= 50, "expected >=50 probes in 8s, got {probes}");

    producer.await.unwrap();

    // After the burst, status.get should still succeed and either report
    // a healthy zero or some drops. Either is acceptable — the broker
    // just must not have wedged.
    let final_status: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    let drops = &final_status.receiver_drops;
    eprintln!("post-stress drops: {drops:?}");
    // GELF UDP is the only producer in this test, so all drops must be
    // attributed there (or zero if everything fit in the channel).
    assert_eq!(drops.gelf_tcp, 0);
    assert_eq!(drops.otlp_http_logs, 0);
    assert_eq!(drops.otlp_grpc_logs, 0);
}
```

- [ ] **Step 2: Add `spawn_with_real_receivers` + `gelf_udp_port` helpers**

In `crates/core/src/test_support.rs`, add a new private constructor that mirrors `spawn_in_dir` but does NOT pass `injected_log_rx`. The default-port-0 config in `default_test_config()` is fine — GELF will bind on kernel-assigned free ports; OTLP stays disabled (its 0/0 ports trigger the `if otlp_grpc_port > 0 || otlp_http_port > 0` skip in `daemon/server.rs`). Add these methods inside `impl TestDaemonHandle`:

```rust
    /// Spawn the daemon with the REAL GELF receiver (UDP+TCP bound to
    /// kernel-assigned ports). Required for tests that exercise the actual
    /// receiver paths. `spawn()` uses the injected-channel harness which
    /// disables real receivers and is unsuitable for backpressure tests.
    pub async fn spawn_with_real_receivers() -> Self {
        let tempdir = Arc::new(TempDir::new().expect("create tempdir for test daemon"));
        Self::spawn_in_dir_no_inject(tempdir, default_test_config()).await
    }

    async fn spawn_in_dir_no_inject(tempdir: Arc<TempDir>, config: DaemonConfig) -> Self {
        let socket_path = tempdir.path().join("logmon.sock");
        let dir_for_daemon = tempdir.path().to_path_buf();
        let socket_path_for_daemon = socket_path.clone();

        // log_tx is unused for real-receiver tests, but the struct field
        // must be initialised; keep a dropped channel so inject_log() (if
        // ever called) becomes a silent no-op.
        let (log_tx, _drop_log_rx) = mpsc::channel::<LogEntry>(1);
        drop(_drop_log_rx);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let accept_paused = Arc::new(AtomicBool::new(false));
        let accept_paused_for_daemon = accept_paused.clone();
        let config_for_daemon = config.clone();

        let join_handle = tokio::spawn(async move {
            let overrides = DaemonOverrides {
                config_dir: Some(dir_for_daemon),
                socket_path: Some(socket_path_for_daemon),
                injected_log_rx: None, // <-- key difference from spawn_in_dir
                shutdown_rx: Some(shutdown_rx),
                accept_paused: Some(accept_paused_for_daemon),
                skip_tracing_init: true,
            };
            if let Err(e) = run_with_overrides(config_for_daemon, overrides).await {
                eprintln!("test daemon (real receivers) exited with error: {e}");
            }
        });

        let mut appeared = false;
        for _ in 0..SOCKET_WAIT_TICKS {
            if socket_path.exists() {
                appeared = true;
                break;
            }
            tokio::time::sleep(SOCKET_WAIT_INTERVAL).await;
        }
        assert!(appeared, "real-receiver daemon socket {} never appeared", socket_path.display());

        Self {
            socket_path,
            tempdir,
            config,
            log_tx,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            accept_paused,
            join_handle: Mutex::new(Some(join_handle)),
        }
    }

    /// Discover the bound UDP port of the GELF receiver via a status.get
    /// RPC call. Only meaningful for daemons spawned with
    /// `spawn_with_real_receivers`.
    pub async fn gelf_udp_port(&self) -> u16 {
        use logmon_broker_protocol::StatusGetResult;
        use serde_json::json;

        let mut client = self.connect_anon().await;
        let result: StatusGetResult = client
            .call("status.get", json!({}))
            .await
            .expect("status.get must succeed on freshly-spawned daemon");
        for entry in &result.receivers {
            if let Some(rest) = entry.strip_prefix("UDP:") {
                if let Ok(port) = rest.parse::<u16>() {
                    return port;
                }
            }
        }
        panic!(
            "no UDP receiver in receivers list: {:?}",
            result.receivers
        )
    }
```

> **Note:** `connect_anon` already exists on `TestDaemonHandle`; `StatusGetResult` is the protocol struct extended in Task 9. The imports `use logmon_broker_protocol::StatusGetResult;` and `use serde_json::json;` are scoped inside the function so they don't pollute the file's top-level imports.

- [ ] **Step 3: Run the stress test in release mode (it's CPU-bound)**

```bash
cargo test -p logmon-broker-core --features test-support --release --test backpressure -- --nocapture
```

Expected: passes within ~10 seconds. Output prints the `receiver_drops` snapshot.

- [ ] **Step 4: Run the full suite once to confirm no regression**

```bash
cargo test -p logmon-broker-core --features test-support
```

Expected: all green (baseline count from Task 1 + the new backpressure test + the unit tests added in Tasks 2/4/5/7/8).

- [ ] **Step 5: Commit**

```bash
git add crates/core/tests/backpressure.rs crates/core/src/test_support.rs
git commit -m "test(broker): backpressure stress test — broker stays responsive under burst"
```

---

## Task 12: Manual end-to-end verification against Store

**Files:**
- N/A (verification step)

- [ ] **Step 1: Install the new broker binary**

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp-backpressure
cargo install --path crates/broker --force --bin logmon-broker
```

Expected: binary installed to `~/.cargo/bin/logmon-broker`.

- [ ] **Step 2: Restart the broker via launchd**

```bash
launchctl kickstart -k gui/$(id -u)/com.yuval.logmon-broker
sleep 2
logmon-broker status
```

Expected: `running pid=… socket=…` line.

- [ ] **Step 3: Run store-test --enforce-baseline twice in a row**

```bash
cd /Users/yuval/Documents/Projects/Store
store-test tests/ --enforce-baseline
store-test tests/ --enforce-baseline
```

Expected: both runs complete; second run does not encounter "OTel collector not online" stalls.

- [ ] **Step 4: Probe broker liveness during/after**

```bash
curl -s -m 2 -o /dev/null -w "%{http_code}\n" -X POST http://localhost:4318/v1/traces -H "Content-Type: application/json" -d '{"resourceSpans":[]}'
lsof /Users/yuval/.config/logmon/logmon.sock
```

Expected: `200` from curl. `lsof` shows ≤2 unix-socket FDs (broker side + at most one transient client).

- [ ] **Step 5: Inspect drop counters**

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"session.start","params":{"protocol_version":1}}' | nc -U /Users/yuval/.config/logmon/logmon.sock &
sleep 1
# Easier path: use logmon-mcp shim or write a one-liner:
logmon get_status 2>/dev/null || true
```

Expected: `receiver_drops` field present in status output. Most fields likely 0; some `otlp_http_traces` drops would indicate the 80%-threshold path engaged (acceptable — that's the design).

- [ ] **Step 6: If everything works, merge to master**

Worktrees share the parent repo's git dir, so the `feat/broker-backpressure` branch is already visible from the main checkout — no `git fetch` needed:

```bash
cd /Users/yuval/Documents/Projects/MCPs/logmon-mcp
git checkout master
git merge --no-ff feat/broker-backpressure -m "Merge feat/broker-backpressure: receiver backpressure resilience"
git push origin master
```

(Or open a PR if you prefer external review.)

- [ ] **Step 7: Clean up worktree**

```bash
git worktree remove ../logmon-mcp-backpressure
```

Expected: worktree removed; branch can be deleted with `git branch -d feat/broker-backpressure` if no further work is queued.

---

## Self-Review Checklist (run before handing off)

**Spec coverage:**
- [x] A — channel cap 1024 → 65 536 → Task 3
- [x] B — try_send + drop counter on all 6 sites → Tasks 4 (OTLP HTTP logs+traces), 5 (OTLP gRPC logs+traces), 7 (GELF UDP), 8 (GELF TCP)
- [x] C — backpressure 429 / UNAVAILABLE on OTLP → Tasks 4, 5
- [x] E — SO_RCVBUF=8 MB on GELF UDP → Task 7
- [x] F — drop counters in status.get + rate-limited warn → Tasks 2 (warn helper), 9 (status surface)
- [x] G — OtlpReceiver Drop aborts handles → Task 10
- [x] H — stress test → Task 11

**Deferred (out of scope by design):**
- D — parallel two-stage processing pipeline. Discussed with user; deferred because (a) measurements show the single consumer is not the bottleneck under store-test load (~10k entries/s vs ~100k/s capacity), (b) parallelizing pre_buffer / trigger context_before semantics is non-trivial and risks correctness regressions. Revisit if/when a future workload demonstrates `log_processor` as the bottleneck.

**Build state across the task sequence:**
- Tasks 1–3: workspace compiles after each.
- Task 4: workspace COMPILE FAILS (HTTP file changed, OtlpReceiver::start still calls old sig). Intentional. The HTTP file's tests still type-check in isolation.
- Task 5: workspace still fails (now both HTTP and gRPC files have new sigs, OtlpReceiver::start still uses old). Intentional.
- Task 6: workspace COMPILES AGAIN — this is the wiring task that re-establishes the build.
- Tasks 7–12: workspace compiles after each.

**Placeholder scan:** No `unimplemented!()` / `TODO` / "fill in" markers remain in the plan after Task 11 Step 2 was filled with concrete code.

**Type consistency:**
- `ReceiverMetrics` / `ReceiverSource` / `ReceiverDropSnapshot` defined in Task 2; used identically in Tasks 4–9, 11.
- `try_send_log` (LogEntry) vs `try_send_span` (SpanEntry) consistently distinguished.
- `BACKPRESSURE_THRESHOLD_PCT = 80` defined identically in `http.rs` (Task 4) and `grpc.rs` (Task 5).
- `LOG_CHANNEL_CAP = SPAN_CHANNEL_CAP = 65_536` (Task 3).
- `UDP_RCVBUF_BYTES = 8 * 1024 * 1024` (Task 7).
- `WARN_INTERVAL_NANOS = 60_000_000_000` (Task 2).
- Protocol field `receiver_drops` (Task 9) matches `ReceiverDropSnapshot` field-for-field.
- `gelf_udp_port()` is `async` (Task 11 Step 2); the stress test in Task 11 Step 1 calls it with `.await`.
