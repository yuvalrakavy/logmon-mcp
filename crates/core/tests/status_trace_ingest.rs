//! `status.get`'s `trace_ingest` counters (Wave: trace-side loss surfaced).
//!
//! Trace-side loss counters (`ReceiverMetrics::trace_ingest_loss`) existed
//! before this and were already consumed by `collectors.*` / `traces.profile`
//! (see `ProfileIngest`), but were never surfaced on `status.get` itself. This
//! file exercises the new `trace_ingest` field there: that it starts at zero,
//! that its three counters read correctly and independently, the true
//! relationship between `trace_ingest.dropped` and `receiver_drops` (they
//! turn out to share the underlying atomics for the OTLP trace sources — see
//! the second test), and that the field is scoped to the session's bound
//! domain exactly like `receiver_drops` is.
//!
//! Exercises `RpcHandler::handle` directly (Harness/`make_domain` pattern
//! borrowed from `collectors_rpc.rs`) rather than a spawned daemon, so this
//! carries no `test-support` feature gate.

use logmon_broker_core::collector::registry::CollectorRegistry;
use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::receiver::{ReceiverMetrics, ReceiverSource, TraceTransport};
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::{RpcRequest, StatusGetResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

struct Harness {
    handler: Arc<RpcHandler>,
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
}

fn make_domain(name: &str) -> Arc<Domain> {
    let seq = Arc::new(SeqCounter::new());
    Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::new(name).unwrap_or_else(|_| DomainId::default_domain()),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 1000,
            span_buffer_size: 1000,
            source: DomainSource::Config,
        },
        Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone())),
        Arc::new(SpanStore::new(1000, seq)),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ))
}

fn harness() -> Harness {
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(make_domain("default"));
    let sessions = Arc::new(SessionRegistry::new());
    let collectors = Arc::new(CollectorRegistry::new());
    let handler = Arc::new(RpcHandler::new(
        domains.clone(),
        sessions.clone(),
        collectors,
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 1000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    ));
    Harness {
        handler,
        domains,
        sessions,
    }
}

impl Harness {
    fn call(&self, sid: &SessionId, method: &str, params: Value) -> Result<Value, String> {
        let req = RpcRequest::new(1, method, params);
        let resp = self.handler.handle(sid, &req);
        match resp.error {
            Some(e) => Err(e.message),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        }
    }

    fn status(&self, sid: &SessionId) -> StatusGetResult {
        let v = self
            .call(sid, "status.get", json!({}))
            .expect("status.get should succeed");
        serde_json::from_value(v).expect("status.get result deserializes as StatusGetResult")
    }

    /// The live `ReceiverMetrics` backing `domain`, fetched back out of the
    /// registry — the SAME `Arc` `handle_status` reads, so bumping counters
    /// through it is visible to the next `status.get` on a session bound
    /// there.
    fn metrics(&self, domain: &str) -> Arc<ReceiverMetrics> {
        let id = DomainId::new(domain).expect("valid domain name");
        self.domains
            .get(&id)
            .unwrap_or_else(|| panic!("domain {domain:?} exists"))
            .metrics
            .clone()
    }
}

fn blank_span() -> SpanEntry {
    let now = chrono::Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 1,
        span_id: 1,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms: 0.0,
        name: "x".into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

/// Fill a capacity-1 span channel and overflow it once, bumping the
/// channel-full drop counter for `source` by exactly one.
///
/// This is the only way to move `dropped` from an integration test:
/// `ReceiverMetrics::record_drop` (the thing that actually increments the
/// counter) is `pub(crate)`, not `pub`, so it is invisible outside the
/// `logmon-broker-core` crate. `try_send_span` is the real `pub` entry point
/// every OTLP trace receiver goes through, so driving it through a full
/// channel exercises the exact production code path instead of poking at
/// internals.
fn overflow_one_span_drop(metrics: &ReceiverMetrics, source: ReceiverSource) {
    let (tx, _rx) = tokio::sync::mpsc::channel::<SpanEntry>(1);
    assert!(
        metrics.try_send_span(&tx, blank_span(), source),
        "first send fills the 1-slot channel"
    );
    assert!(
        !metrics.try_send_span(&tx, blank_span(), source),
        "second send finds it full — this is the channel-full drop path"
    );
}

// ---------------------------------------------------------------------------
// (a) Fresh daemon: all three counters start at zero.
// ---------------------------------------------------------------------------

#[test]
fn status_get_reports_zero_trace_ingest_on_a_fresh_daemon() {
    let h = harness();
    let sid = h.sessions.create_named("A").expect("session");

    let status = h.status(&sid);
    assert_eq!(status.trace_ingest.dropped, 0);
    assert_eq!(status.trace_ingest.shed_batches, 0);
    assert_eq!(status.trace_ingest.malformed_dropped, 0);
}

// ---------------------------------------------------------------------------
// (b) The three counters read correctly, and the true relationship to
//     receiver_drops: shed_batches / malformed_dropped never move it, but
//     dropped does — because it is sourced from the SAME atomics.
// ---------------------------------------------------------------------------

#[test]
fn shed_and_malformed_never_move_receiver_drops_but_dropped_shares_its_atomics() {
    let h = harness();
    let sid = h.sessions.create_named("A").expect("session");
    let metrics = h.metrics("default");

    // Stage 1: shed batches + malformed spans only. Neither is a
    // channel-full drop, so receiver_drops (which documents itself as
    // counting only SILENT channel-full loss) must not move at all.
    metrics.record_trace_batch_shed(TraceTransport::OtlpHttp);
    metrics.record_trace_batch_shed(TraceTransport::OtlpHttp);
    metrics.record_trace_batch_shed(TraceTransport::OtlpGrpc);
    metrics.record_trace_malformed(TraceTransport::OtlpHttp);

    let status = h.status(&sid);
    assert_eq!(status.trace_ingest.dropped, 0);
    assert_eq!(status.trace_ingest.shed_batches, 3, "2 http + 1 grpc");
    assert_eq!(status.trace_ingest.malformed_dropped, 1);
    assert_eq!(
        status.receiver_drops.otlp_http_traces, 0,
        "shed batches are refused loudly (429/UNAVAILABLE) before parsing — \
         not a receiver_drops event"
    );
    assert_eq!(status.receiver_drops.otlp_grpc_traces, 0);
    assert_eq!(
        status.receiver_drops.gelf_udp, 0,
        "trace-only bumps must not leak into unrelated sources either"
    );
    assert_eq!(status.receiver_drops.gelf_tcp, 0);
    assert_eq!(status.receiver_drops.otlp_http_logs, 0);
    assert_eq!(status.receiver_drops.otlp_grpc_logs, 0);

    // Stage 2: now the channel-full path, which IS a receiver_drops event.
    //
    // Finding that contradicts a naive "trace_ingest and receiver_drops are
    // fully independent" assumption: `TraceIngestLoss::dropped` is computed
    // (metrics.rs:264) as `otlp_http_traces + otlp_grpc_traces` — literally
    // the same two atomics that `ReceiverMetrics::snapshot()` reads into
    // `receiver_drops.otlp_http_traces` / `.otlp_grpc_traces`
    // (rpc_handler.rs `handle_status`). There is no separate counter for
    // "trace channel-full drops" — `dropped` and those two `receiver_drops`
    // fields are two different projections of the SAME underlying state. So
    // bumping `dropped` via the real `try_send_span` full-channel path is
    // expected, not a bug, to move `receiver_drops` in lockstep. What stays
    // separate is `shed_batches` / `malformed_dropped` (their own atomics,
    // asserted untouched above and again below) — that is the part of
    // "receiver_drops must not change" that is actually true.
    overflow_one_span_drop(&metrics, ReceiverSource::OtlpHttpTraces);
    overflow_one_span_drop(&metrics, ReceiverSource::OtlpGrpcTraces);

    let status = h.status(&sid);
    assert_eq!(status.trace_ingest.dropped, 2, "one per transport");
    assert_eq!(
        status.trace_ingest.shed_batches, 3,
        "unaffected by the dropped bump"
    );
    assert_eq!(
        status.trace_ingest.malformed_dropped, 1,
        "unaffected by the dropped bump"
    );
    assert_eq!(
        status.receiver_drops.otlp_http_traces, 1,
        "moved: the SAME atomic feeds trace_ingest.dropped and this field"
    );
    assert_eq!(
        status.receiver_drops.otlp_grpc_traces, 1,
        "moved: the SAME atomic feeds trace_ingest.dropped and this field"
    );
    // Still untouched: gelf/log-side sources share nothing with trace loss.
    assert_eq!(status.receiver_drops.gelf_udp, 0);
    assert_eq!(status.receiver_drops.gelf_tcp, 0);
    assert_eq!(status.receiver_drops.otlp_http_logs, 0);
    assert_eq!(status.receiver_drops.otlp_grpc_logs, 0);
}

// ---------------------------------------------------------------------------
// (c) Scope: trace_ingest follows the session's domain binding, exactly like
//     receiver_drops does — a session bound elsewhere sees zeros even while
//     the default domain it is NOT bound to is actively losing spans.
// ---------------------------------------------------------------------------

#[test]
fn trace_ingest_follows_the_session_domain_binding_not_daemon_wide_totals() {
    let h = harness();
    h.domains.insert(make_domain("other"));

    let default_sid = h.sessions.create_named("on-default").expect("session");
    let other_sid = h.sessions.create_named("on-other").expect("session");
    h.call(&other_sid, "domains.use", json!({ "name": "other" }))
        .expect("bind on-other's session to the other domain");

    // Bump ONLY the default domain's counters — `other` never touched.
    let default_metrics = h.metrics("default");
    default_metrics.record_trace_batch_shed(TraceTransport::OtlpHttp);
    default_metrics.record_trace_malformed(TraceTransport::OtlpGrpc);
    overflow_one_span_drop(&default_metrics, ReceiverSource::OtlpHttpTraces);

    // Sanity check first: the session still bound to `default` DOES see the
    // loss. Without this half of the test, a `trace_ingest` that was wired
    // to always report zero would pass the "other domain sees zero" check
    // below for the wrong reason.
    let default_status = h.status(&default_sid);
    assert_eq!(default_status.trace_ingest.dropped, 1);
    assert_eq!(default_status.trace_ingest.shed_batches, 1);
    assert_eq!(default_status.trace_ingest.malformed_dropped, 1);

    // The session bound to `other` sees none of it: the field follows the
    // binding, exactly like receiver_drops (§7).
    let other_status = h.status(&other_sid);
    assert_eq!(other_status.trace_ingest.dropped, 0);
    assert_eq!(other_status.trace_ingest.shed_batches, 0);
    assert_eq!(other_status.trace_ingest.malformed_dropped, 0);
    assert_eq!(other_status.receiver_drops.otlp_http_traces, 0);
}
