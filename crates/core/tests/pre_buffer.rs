use chrono::Utc;
use logmon_broker_core::engine::pre_buffer::PreTriggerBuffer;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use std::collections::HashMap;

fn make_entry(seq: u64) -> LogEntry {
    LogEntry {
        seq,
        timestamp: Utc::now(),
        level: Level::Info,
        message: format!("msg {seq}"),
        full_message: None,
        host: "test".into(),
        facility: None,
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        trace_id: None,
        span_id: None,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

#[test]
fn test_append_and_flush() {
    let buf = PreTriggerBuffer::new(5);
    for i in 1..=3 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3);
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 1);
    assert_eq!(flushed[2].seq, 3);
}

#[test]
fn test_flush_respects_pre_window() {
    let buf = PreTriggerBuffer::new(10);
    for i in 1..=10 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3); // only last 3
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 8);
    assert_eq!(flushed[2].seq, 10);
}

#[test]
fn test_ring_buffer_eviction() {
    let buf = PreTriggerBuffer::new(3);
    for i in 1..=5 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(10); // ask for more than capacity
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 3); // oldest surviving
}

#[test]
fn test_flush_removes_only_flushed_entries() {
    let buf = PreTriggerBuffer::new(10);
    for i in 1..=10 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(3); // flush last 3 (8, 9, 10)
    assert_eq!(flushed.len(), 3);
    assert_eq!(flushed[0].seq, 8);
    // Remaining entries should still be there
    let flushed2 = buf.flush(10);
    assert_eq!(flushed2.len(), 7); // entries 1-7
}

#[test]
fn test_resize() {
    let buf = PreTriggerBuffer::new(3);
    buf.resize(5);
    for i in 1..=5 {
        buf.append(make_entry(i));
    }
    assert_eq!(buf.flush(10).len(), 5);
}

#[test]
fn test_disabled_when_zero() {
    let buf = PreTriggerBuffer::new(0);
    buf.append(make_entry(1));
    assert_eq!(buf.flush(10).len(), 0);
}

#[test]
fn test_flush_returns_chronological_order() {
    let buf = PreTriggerBuffer::new(5);
    for i in 1..=5 {
        buf.append(make_entry(i));
    }
    let flushed = buf.flush(5);
    for i in 0..flushed.len() - 1 {
        assert!(flushed[i].seq < flushed[i + 1].seq);
    }
}
