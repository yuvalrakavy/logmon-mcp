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
    #[must_use]
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
