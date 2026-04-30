use logmon_broker_core::store::memory::InMemoryStore;
use logmon_broker_core::store::traits::LogStore;
use logmon_broker_core::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: None, file: None, line: None,
        additional_fields: HashMap::new(),
        trace_id: None, span_id: None,
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_append_and_recent() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "first"));
    store.append(make_entry(2, Level::Info, "second"));
    let recent = store.recent(10, None);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].seq, 2); // newest first
    assert_eq!(recent[1].seq, 1);
}

#[test]
fn test_max_capacity() {
    let store = InMemoryStore::new(3);
    for i in 1..=5 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    assert_eq!(store.len(), 3);
    let recent = store.recent(10, None);
    assert_eq!(recent[0].seq, 5);
    assert_eq!(recent[2].seq, 3); // oldest surviving
}

#[test]
fn test_contains_seq() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(42, Level::Info, "hello"));
    assert!(store.contains_seq(42));
    assert!(!store.contains_seq(99));
}

#[test]
fn test_context_by_seq() {
    let store = InMemoryStore::new(100);
    for i in 1..=10 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let ctx = store.context_by_seq(5, 2, 2);
    assert_eq!(ctx.len(), 5); // seq 3,4,5,6,7
    assert_eq!(ctx[0].seq, 3);
    assert_eq!(ctx[4].seq, 7);
}

#[test]
fn test_context_by_seq_at_edges() {
    let store = InMemoryStore::new(100);
    for i in 1..=5 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    // At the beginning
    let ctx = store.context_by_seq(1, 5, 1);
    assert_eq!(ctx[0].seq, 1); // can't go before first
    // At the end
    let ctx = store.context_by_seq(5, 1, 5);
    assert_eq!(ctx.last().unwrap().seq, 5); // can't go past last
}

#[test]
fn test_clear() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "hello"));
    assert_eq!(store.len(), 1);
    store.clear();
    assert_eq!(store.len(), 0);
}

#[test]
fn test_recent_with_filter() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "good"));
    store.append(make_entry(2, Level::Error, "bad"));
    store.append(make_entry(3, Level::Info, "good again"));

    use logmon_broker_core::filter::parser::parse_filter;
    let filter = parse_filter("l>=ERROR").unwrap();
    let recent = store.recent(10, Some(&filter));
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].seq, 2);
}

#[test]
fn test_recent_with_count_limit() {
    let store = InMemoryStore::new(100);
    for i in 1..=10 {
        store.append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let recent = store.recent(3, None);
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].seq, 10); // newest
    assert_eq!(recent[2].seq, 8);
}

#[test]
fn test_stats() {
    let store = InMemoryStore::new(100);
    store.append(make_entry(1, Level::Info, "hello"));
    store.append(make_entry(2, Level::Info, "world"));
    let stats = store.stats();
    assert_eq!(stats.total_stored, 2);
}

#[test]
fn test_logs_by_trace_id() {
    let store = InMemoryStore::new(100);
    let trace = 0xabc123_u128;

    let mut e1 = make_entry(1, Level::Info, "first");
    e1.trace_id = Some(trace);
    store.append(e1);

    let mut e2 = make_entry(2, Level::Info, "second");
    e2.trace_id = Some(trace);
    store.append(e2);

    let e3 = make_entry(3, Level::Info, "unrelated");
    store.append(e3);

    let traced = store.logs_by_trace_id(trace);
    assert_eq!(traced.len(), 2);
    assert_eq!(store.count_by_trace_id(trace), 2);
    assert_eq!(store.count_by_trace_id(0xdead_u128), 0);
}

#[test]
fn test_trace_id_index_eviction() {
    let store = InMemoryStore::new(3);
    let trace = 0xabc_u128;

    for i in 1..=4 {
        let mut e = make_entry(i, Level::Info, &format!("msg {i}"));
        e.trace_id = Some(trace);
        store.append(e);
    }

    // Entry with seq=1 evicted, only 3 remain
    let traced = store.logs_by_trace_id(trace);
    assert_eq!(traced.len(), 3);
}
