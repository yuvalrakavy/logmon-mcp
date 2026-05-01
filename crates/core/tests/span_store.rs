use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::*;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_span(name: &str, trace_id: u128, duration_ms: f64) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0, trace_id,
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
    let store = SpanStore::new(100, seq);
    let trace_id = 0xabc_u128;
    store.insert(make_span("root", trace_id, 100.0));
    store.insert(make_span("child", trace_id, 50.0));
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
    assert_eq!(slow[0].name, "slow");
    assert_eq!(slow[1].name, "medium");
}

#[test]
fn test_recent_traces() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(100, seq);
    store.insert(make_span("trace-a-root", 0xa_u128, 100.0));
    store.insert(make_span("trace-b-root", 0xb_u128, 200.0));
    let recent = store.recent_traces(10, None, |_| 0u32);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].trace_id, 0xb_u128);
}

#[test]
fn test_eviction_cleans_index() {
    let seq = Arc::new(SeqCounter::new());
    let store = SpanStore::new(3, seq);
    store.insert(make_span("a1", 0xa_u128, 10.0));
    store.insert(make_span("a2", 0xa_u128, 10.0));
    store.insert(make_span("b1", 0xb_u128, 10.0));
    store.insert(make_span("b2", 0xb_u128, 10.0));
    let a_spans = store.get_trace(0xa_u128);
    assert_eq!(a_spans.len(), 1);
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
    let all = store.slow_spans(0.0, 100, None);
    let target_seq = all.iter().find(|s| s.name == "span-5").unwrap().seq;
    let context = store.context_by_seq(target_seq, 2, 2);
    assert_eq!(context.len(), 5);
}
