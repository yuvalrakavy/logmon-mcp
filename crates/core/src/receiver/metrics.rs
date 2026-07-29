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

/// The two transports a span can arrive on.
///
/// Deliberately narrower than [`ReceiverSource`]: the span-loss counters below
/// exist so a collector can say whether spans went missing during its window,
/// and a GELF burst or a log-side 429 must not be able to reach them. Making
/// that a type rather than a convention means a log call site cannot feed span
/// accounting even by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceTransport {
    OtlpHttp,
    OtlpGrpc,
}

/// The three ways a span is lost before it can reach a collector, summed over
/// both OTLP trace transports.
///
/// They are kept apart because they call for different remedies: `dropped`
/// means the broker's channel was full, `shed_batches` means the broker refused
/// whole request bodies and told the client so, and `malformed` means the spans
/// were unusable on arrival. Only the first was previously counted, which is
/// why a collector could report a clean window through a run that lost most of
/// its spans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceIngestLoss {
    /// Per-span, channel full at `try_send_span`.
    pub dropped: u64,
    /// Whole request bodies rejected with 429 / UNAVAILABLE before any span in
    /// them was parsed. The span count behind these is unknowable — the body
    /// was never read — so this is a count of *batches*, and its name says so.
    pub shed_batches: u64,
    /// Discarded at parse: unusable trace id, unusable span id, or empty name.
    pub malformed: u64,
}

impl TraceIngestLoss {
    /// The loss accumulated between two readings of the same counters.
    ///
    /// `saturating_sub` throughout: the caller is expected to have checked that
    /// both readings came from the same `ReceiverMetrics` instance, but a
    /// wrapped-around subtraction would turn a small bookkeeping mistake into a
    /// vast fabricated loss, which is worse than reporting zero.
    pub fn since(self, baseline: Self) -> Self {
        Self {
            dropped: self.dropped.saturating_sub(baseline.dropped),
            shed_batches: self.shed_batches.saturating_sub(baseline.shed_batches),
            malformed: self.malformed.saturating_sub(baseline.malformed),
        }
    }

    /// Whether anything at all was lost.
    pub fn is_clean(self) -> bool {
        self == Self::default()
    }
}

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
    /// Trace request bodies refused wholesale under backpressure. Counted
    /// separately from the drop counters above and NOT surfaced as a
    /// `receiver_drops` figure, because those two mean different things to a
    /// client: a drop is silent data loss, a shed is a 429 the client saw.
    otlp_http_traces_shed: AtomicU64,
    otlp_grpc_traces_shed: AtomicU64,
    /// Spans discarded at parse time, per transport.
    otlp_http_traces_malformed: AtomicU64,
    otlp_grpc_traces_malformed: AtomicU64,
    /// Unix-epoch nanos of the last warn emission. Initialised to a value
    /// that ensures the first drop always warns.
    last_warn_nanos: AtomicI64,
    /// Unix-epoch nanos of the last SUCCESSFUL forward per source (`i64::MIN` =
    /// never received). Powers per-domain / per-listener liveness (#2).
    gelf_udp_last: AtomicI64,
    gelf_tcp_last: AtomicI64,
    otlp_http_logs_last: AtomicI64,
    otlp_http_traces_last: AtomicI64,
    otlp_grpc_logs_last: AtomicI64,
    otlp_grpc_traces_last: AtomicI64,
}

/// Snapshot of last-received Unix-epoch nanos per source (`None` = never). (#2.)
#[derive(Debug, Clone, Default)]
pub struct ReceiverLivenessNanos {
    pub gelf_udp: Option<i64>,
    pub gelf_tcp: Option<i64>,
    pub otlp_http_logs: Option<i64>,
    pub otlp_http_traces: Option<i64>,
    pub otlp_grpc_logs: Option<i64>,
    pub otlp_grpc_traces: Option<i64>,
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
            otlp_http_traces_shed: AtomicU64::new(0),
            otlp_grpc_traces_shed: AtomicU64::new(0),
            otlp_http_traces_malformed: AtomicU64::new(0),
            otlp_grpc_traces_malformed: AtomicU64::new(0),
            last_warn_nanos: AtomicI64::new(i64::MIN),
            gelf_udp_last: AtomicI64::new(i64::MIN),
            gelf_tcp_last: AtomicI64::new(i64::MIN),
            otlp_http_logs_last: AtomicI64::new(i64::MIN),
            otlp_http_traces_last: AtomicI64::new(i64::MIN),
            otlp_grpc_logs_last: AtomicI64::new(i64::MIN),
            otlp_grpc_traces_last: AtomicI64::new(i64::MIN),
        }
    }

    /// Stamp the last-received time for `source` — called on every successful
    /// forward. Relaxed: liveness is advisory, not a happens-before guard.
    fn record_received(&self, source: ReceiverSource) {
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        let slot = match source {
            ReceiverSource::GelfUdp => &self.gelf_udp_last,
            ReceiverSource::GelfTcp => &self.gelf_tcp_last,
            ReceiverSource::OtlpHttpLogs => &self.otlp_http_logs_last,
            ReceiverSource::OtlpHttpTraces => &self.otlp_http_traces_last,
            ReceiverSource::OtlpGrpcLogs => &self.otlp_grpc_logs_last,
            ReceiverSource::OtlpGrpcTraces => &self.otlp_grpc_traces_last,
        };
        slot.store(now, Ordering::Relaxed);
    }

    /// Per-source last-received snapshot (`None` = never). (#2.)
    pub fn liveness(&self) -> ReceiverLivenessNanos {
        let g = |a: &AtomicI64| match a.load(Ordering::Relaxed) {
            i64::MIN => None,
            v => Some(v),
        };
        ReceiverLivenessNanos {
            gelf_udp: g(&self.gelf_udp_last),
            gelf_tcp: g(&self.gelf_tcp_last),
            otlp_http_logs: g(&self.otlp_http_logs_last),
            otlp_http_traces: g(&self.otlp_http_traces_last),
            otlp_grpc_logs: g(&self.otlp_grpc_logs_last),
            otlp_grpc_traces: g(&self.otlp_grpc_traces_last),
        }
    }

    /// Increment the counter for `source` and emit a `tracing::warn!` if the
    /// last warning was more than 60 seconds ago (or never).
    pub(crate) fn record_drop(&self, source: ReceiverSource) {
        let counter = match source {
            ReceiverSource::GelfUdp => &self.gelf_udp,
            ReceiverSource::GelfTcp => &self.gelf_tcp,
            ReceiverSource::OtlpHttpLogs => &self.otlp_http_logs,
            ReceiverSource::OtlpHttpTraces => &self.otlp_http_traces,
            ReceiverSource::OtlpGrpcLogs => &self.otlp_grpc_logs,
            ReceiverSource::OtlpGrpcTraces => &self.otlp_grpc_traces,
        };
        counter.fetch_add(1, Ordering::Relaxed);

        let now_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        // Relaxed: last_warn_nanos guards nothing else; sole side effect is the warn emission below.
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

    /// Record one trace request body refused wholesale under backpressure.
    ///
    /// Called from the 429 / UNAVAILABLE gate, which returns *before* any span
    /// in the body is parsed — so no per-span counter can ever move for it.
    /// That gate is the primary load-shedding mechanism on these transports and
    /// it engages exactly when load is high, which is the variable under test
    /// in any before/after comparison. Left uncounted, a collector would report
    /// a spotless window through the run that lost the most spans.
    pub fn record_trace_batch_shed(&self, transport: TraceTransport) {
        match transport {
            TraceTransport::OtlpHttp => &self.otlp_http_traces_shed,
            TraceTransport::OtlpGrpc => &self.otlp_grpc_traces_shed,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Record one span discarded at parse time.
    pub fn record_trace_malformed(&self, transport: TraceTransport) {
        match transport {
            TraceTransport::OtlpHttp => &self.otlp_http_traces_malformed,
            TraceTransport::OtlpGrpc => &self.otlp_grpc_traces_malformed,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Span loss across both OTLP trace transports.
    ///
    /// Trace sources only. Including the GELF or OTLP-log counters here would
    /// let a log burst mark a span collector's window untrustworthy on a run
    /// that lost no spans at all.
    pub fn trace_ingest_loss(&self) -> TraceIngestLoss {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TraceIngestLoss {
            dropped: g(&self.otlp_http_traces) + g(&self.otlp_grpc_traces),
            shed_batches: g(&self.otlp_http_traces_shed) + g(&self.otlp_grpc_traces_shed),
            malformed: g(&self.otlp_http_traces_malformed) + g(&self.otlp_grpc_traces_malformed),
        }
    }

    /// Send a [`LogEntry`] without ever parking. Returns `true` on success,
    /// `false` if the channel is full (counter incremented) or closed.
    #[must_use]
    pub fn try_send_log(
        &self,
        sender: &mpsc::Sender<LogEntry>,
        entry: LogEntry,
        source: ReceiverSource,
    ) -> bool {
        match sender.try_send(entry) {
            Ok(()) => {
                self.record_received(source);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.record_drop(source);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Send a [`SpanEntry`] without ever parking. Returns `true` on success,
    /// `false` if the channel is full (counter incremented) or closed.
    #[must_use]
    pub fn try_send_span(
        &self,
        sender: &mpsc::Sender<SpanEntry>,
        entry: SpanEntry,
        source: ReceiverSource,
    ) -> bool {
        match sender.try_send(entry) {
            Ok(()) => {
                self.record_received(source);
                true
            }
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

    #[test]
    fn trace_loss_ignores_every_non_trace_source() {
        // The reason this matters: a GELF burst during a run must not mark a
        // span collector's window as having lost spans. It lost logs.
        let m = ReceiverMetrics::new();
        for source in [
            ReceiverSource::GelfUdp,
            ReceiverSource::GelfTcp,
            ReceiverSource::OtlpHttpLogs,
            ReceiverSource::OtlpGrpcLogs,
        ] {
            m.record_drop(source);
        }
        assert!(
            m.trace_ingest_loss().is_clean(),
            "no span was lost, whatever the log side did"
        );

        m.record_drop(ReceiverSource::OtlpHttpTraces);
        m.record_drop(ReceiverSource::OtlpGrpcTraces);
        assert_eq!(
            m.trace_ingest_loss().dropped,
            2,
            "both trace transports count toward span loss"
        );
    }

    #[test]
    fn shed_and_malformed_are_counted_apart_from_drops_and_from_each_other() {
        let m = ReceiverMetrics::new();
        m.record_trace_batch_shed(TraceTransport::OtlpHttp);
        m.record_trace_batch_shed(TraceTransport::OtlpGrpc);
        m.record_trace_malformed(TraceTransport::OtlpHttp);

        let loss = m.trace_ingest_loss();
        assert_eq!(loss.shed_batches, 2);
        assert_eq!(loss.malformed, 1);
        assert_eq!(loss.dropped, 0);
        // And none of it leaked into the wire-facing drop snapshot, which
        // documents itself as counting only entries the broker silently lost.
        assert_eq!(m.snapshot(), ReceiverDropSnapshot::default());
    }

    #[test]
    fn a_window_delta_never_reports_more_loss_than_happened() {
        let m = ReceiverMetrics::new();
        m.record_drop(ReceiverSource::OtlpHttpTraces);
        let baseline = m.trace_ingest_loss();
        m.record_drop(ReceiverSource::OtlpHttpTraces);
        m.record_trace_batch_shed(TraceTransport::OtlpGrpc);

        let delta = m.trace_ingest_loss().since(baseline);
        assert_eq!(delta.dropped, 1, "only what happened inside the window");
        assert_eq!(delta.shed_batches, 1);

        // A baseline ahead of the reading means the counters were reset under
        // us. Zero is the honest answer; wrapping would fabricate 18 quintillion.
        let stale = TraceIngestLoss {
            dropped: 99,
            shed_batches: 99,
            malformed: 99,
        };
        assert!(m.trace_ingest_loss().since(stale).is_clean());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn try_send_log_drops_when_full() {
        use crate::gelf::message::{Level, LogEntry, LogSource};
        use chrono::Utc;
        use std::collections::HashMap;
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<LogEntry>(1);
        let m = Arc::new(ReceiverMetrics::new());

        let make = || LogEntry {
            seq: 0,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "x".into(),
            full_message: None,
            host: "h".into(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: vec![],
            source: LogSource::Filter,
        };

        // First send fills the 1-slot channel.
        assert!(m.try_send_log(&tx, make(), ReceiverSource::GelfUdp));
        // Second send finds it full → drop counted, no panic, no park.
        assert!(!m.try_send_log(&tx, make(), ReceiverSource::GelfUdp));
        assert_eq!(m.snapshot().gelf_udp, 1);
    }
}
