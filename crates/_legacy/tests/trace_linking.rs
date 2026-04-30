use logmon_mcp_server::daemon::log_processor::{process_entry, sync_pre_buffer_size};
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::engine::seq_counter::SeqCounter;
use logmon_mcp_server::gelf::message::{Level, LogEntry, LogSource};
use logmon_mcp_server::span::store::SpanStore;
use logmon_mcp_server::span::types::*;

use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq: 0,
        timestamp: Utc::now(),
        level,
        message: msg.to_string(),
        full_message: None,
        host: "test".into(),
        facility: Some("test".into()),
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(),
        source: LogSource::Filter,
        trace_id: None,
        span_id: None,
    }
}

fn make_span(name: &str, trace_id: u128, duration_ms: f64) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id,
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
fn test_logs_and_spans_linked_by_trace_id() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace_id = 0xabc123_u128;

    // Insert a log with trace context
    let mut log = make_entry(Level::Info, "processing request");
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

    // Both share the same global seq counter -- different seq numbers
    assert!(traced_logs[0].seq != traced_spans[0].seq);
}

#[test]
fn test_shared_seq_counter_interleaves() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    sync_pre_buffer_size(&pipeline, &sessions);

    let mut log = make_entry(Level::Info, "first");
    process_entry(&mut log, &pipeline, &sessions);
    let log_seq = log.seq;

    let span = make_span("second", 1, 10.0);
    let span_seq = span_store.insert(span);
    // span_seq should be log_seq + 1
    assert_eq!(span_seq, log_seq + 1);

    let mut log2 = make_entry(Level::Info, "third");
    process_entry(&mut log2, &pipeline, &sessions);
    assert!(log2.seq > log_seq + 1); // seq counter shared
    assert_eq!(log2.seq, span_seq + 1);
}

#[test]
fn test_multiple_logs_and_spans_same_trace() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace_id = 0xfeed_u128;

    // Insert multiple logs and spans for the same trace
    let mut log1 = make_entry(Level::Info, "start");
    log1.trace_id = Some(trace_id);
    process_entry(&mut log1, &pipeline, &sessions);

    let span1 = make_span("span-a", trace_id, 50.0);
    span_store.insert(span1);

    let mut log2 = make_entry(Level::Debug, "middle");
    log2.trace_id = Some(trace_id);
    process_entry(&mut log2, &pipeline, &sessions);

    let span2 = make_span("span-b", trace_id, 25.0);
    span_store.insert(span2);

    let mut log3 = make_entry(Level::Info, "end");
    log3.trace_id = Some(trace_id);
    process_entry(&mut log3, &pipeline, &sessions);

    // All logs for this trace
    let traced_logs = pipeline.logs_by_trace_id(trace_id);
    assert_eq!(traced_logs.len(), 3);

    // All spans for this trace
    let traced_spans = span_store.get_trace(trace_id);
    assert_eq!(traced_spans.len(), 2);

    // All five entries have unique, monotonically increasing seq values
    let mut all_seqs: Vec<u64> = traced_logs.iter().map(|l| l.seq).collect();
    all_seqs.extend(traced_spans.iter().map(|s| s.seq));
    all_seqs.sort();
    assert_eq!(all_seqs.len(), 5);
    for window in all_seqs.windows(2) {
        assert!(window[1] > window[0], "seq values must be strictly increasing");
    }
}

#[test]
fn test_different_traces_are_isolated() {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(100, seq.clone()));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();
    sync_pre_buffer_size(&pipeline, &sessions);

    let trace_a = 0xaaa_u128;
    let trace_b = 0xbbb_u128;

    let mut log_a = make_entry(Level::Info, "trace-a log");
    log_a.trace_id = Some(trace_a);
    process_entry(&mut log_a, &pipeline, &sessions);

    let mut log_b = make_entry(Level::Info, "trace-b log");
    log_b.trace_id = Some(trace_b);
    process_entry(&mut log_b, &pipeline, &sessions);

    let span_a = make_span("span-a", trace_a, 10.0);
    span_store.insert(span_a);

    let span_b = make_span("span-b", trace_b, 20.0);
    span_store.insert(span_b);

    // Each trace sees only its own entries
    assert_eq!(pipeline.logs_by_trace_id(trace_a).len(), 1);
    assert_eq!(pipeline.logs_by_trace_id(trace_b).len(), 1);
    assert_eq!(span_store.get_trace(trace_a).len(), 1);
    assert_eq!(span_store.get_trace(trace_b).len(), 1);

    // Nonexistent trace returns empty
    assert_eq!(pipeline.logs_by_trace_id(0xdead_u128).len(), 0);
    assert_eq!(span_store.get_trace(0xdead_u128).len(), 0);
}
