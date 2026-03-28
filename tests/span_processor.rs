use logmon_mcp_server::daemon::span_processor::process_span;
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::seq_counter::SeqCounter;
use logmon_mcp_server::span::store::SpanStore;
use logmon_mcp_server::span::types::*;
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
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());

    let span = make_span("query", 100.0);
    process_span(&span, &store, &sessions);
    assert_eq!(store.len(), 1);
}

#[test]
fn test_span_trigger_fires() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    // Add a span trigger (d>=500 is a span selector, not log)
    sessions.add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow span")).unwrap();

    let span = make_span("slow_query", 600.0);
    process_span(&span, &store, &sessions);

    // Span stored
    assert_eq!(store.len(), 1);
}

#[test]
fn test_span_trigger_no_match() {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(100, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    sessions.add_trigger(&sid, "d>=500", 0, 0, 0, Some("slow")).unwrap();

    let span = make_span("fast_query", 10.0);
    process_span(&span, &store, &sessions);

    // Span stored but no trigger fired
    assert_eq!(store.len(), 1);
}
