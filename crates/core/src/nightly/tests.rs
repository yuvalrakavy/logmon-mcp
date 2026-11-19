use super::{CaptureSnapshot, Selector, Summary};
use crate::cases;
use crate::daemon::domain::{Domain, DomainConfig, DomainId, DomainSource};
use crate::gelf::message::{Level, LogEntry};
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::{Duration, Utc};
use serde_json::json;

fn domain() -> Domain {
    Domain::new(
        DomainConfig {
            name: DomainId::default_domain(),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 100,
            span_buffer_size: 100,
            source: DomainSource::Ephemeral,
        },
        0,
    )
}

fn span() -> SpanEntry {
    let start = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 10,
        span_id: 9,
        parent_span_id: None,
        start_time: start,
        end_time: start + Duration::milliseconds(3),
        duration_ms: 3.0,
        name: "kernel".into(),
        kind: SpanKind::Internal,
        service_name: "store".into(),
        status: SpanStatus::Ok,
        attributes: [("kernel.entry_point".to_owned(), json!("close"))].into(),
        events: vec![],
    }
}

#[test]
fn snapshots_are_selective_survive_ring_eviction_and_round_trip() {
    let d = domain();
    let now = Utc::now();
    for (id, delta, msg) in [
        (Some(10), -4, "target before interval"),
        (Some(11), 0, "peer"),
        (None, 0, "untagged"),
        (Some(11), 2, "exclude newer peer"),
    ] {
        let mut log = LogEntry::synthetic(Level::Info, msg);
        log.trace_id = id;
        log.timestamp = now + Duration::seconds(delta);
        log.seq = d.pipeline.assign_seq();
        d.pipeline.append_to_store(log);
    }
    let mut peer = span();
    peer.trace_id = 11;
    peer.start_time = now - Duration::seconds(2);
    peer.end_time = now + Duration::seconds(2);
    d.span_store.insert(peer);
    let mut snap = CaptureSnapshot::new(
        &d,
        Selector::new("trace_id", "a"),
        now - Duration::seconds(1),
        now,
    );
    d.pipeline.clear_logs();
    let mut late = span();
    late.seq = d.pipeline.assign_seq();
    snap.observe_span(&late);
    snap.observe_span(&late);
    let dir = tempfile::tempdir().unwrap();
    let case = snap
        .write(
            dir.path(),
            &json!({"name":"A","reason":"fail","end":{"message":"assertion\nfailed"}}),
            vec![],
        )
        .unwrap();
    let loaded = cases::load::load(&case).unwrap();
    let messages: Vec<_> = loaded
        .logs
        .unwrap()
        .into_iter()
        .map(|e| e.message)
        .collect();
    assert_eq!(
        messages,
        [
            "target before interval",
            "peer",
            "untagged",
            "assertion\nfailed"
        ]
    );
    assert_eq!(loaded.spans.unwrap().len(), 2);
}

#[test]
fn counts_are_selector_scoped_and_backfill_deduplicates_pending_callbacks() {
    let d = domain();
    let mut event = LogEntry::synthetic(Level::Warn, "warn");
    event.trace_id = Some(10);
    event.seq = d.pipeline.assign_seq();
    d.pipeline.append_to_store(event.clone());
    let mut s = span();
    s.seq = d.span_store.insert(s.clone());
    let mut counts = Summary::new(
        "trace_id",
        "0000000000000000000000000000000a",
        &["kernel.entry_point".to_owned()],
    );
    let coverage = counts.backfill(&d, event.timestamp, s.end_time);
    // The processor had stored these records when the snapshot took its lock,
    // but its callback on the test-log lock was still waiting.
    counts.observe_log(&event);
    counts.observe_span(&s);
    s.trace_id = 12;
    s.seq += 1;
    counts.observe_span(&s);
    assert_eq!(coverage["log_coverage"]["status"], "retained");
    let view = counts.json();
    assert_eq!(view["log_levels"]["WARN"], 1);
    assert_eq!(view["spans"]["kernel"]["count"], 1);
    assert_eq!(view["spans"]["kernel"]["total_duration_ms"], 3.0);
    assert_eq!(
        view["distinct_values"]["kernel.entry_point"]["values"],
        json!(["close"])
    );
}

#[test]
fn oversized_record_fails_explicitly_without_copying_it() {
    let d = domain();
    let now = Utc::now();
    let mut snap = CaptureSnapshot::new(&d, Selector::new("trace_id", "a"), now, now);
    let mut log = LogEntry::synthetic(Level::Info, "");
    log.trace_id = Some(10);
    d.test_log.capture_pool.lock().unwrap().set_available(1024);
    log.message = "x".repeat(2048);
    snap.observe_log(&log);
    assert!(snap.is_over_budget());
    assert_eq!(d.test_log.capture_pool.lock().unwrap().available(), 1024);
    let dir = tempfile::tempdir().unwrap();
    assert!(snap.write(dir.path(), &json!({}), vec![]).is_err());
    assert_eq!(dir.path().read_dir().unwrap().count(), 0);
}

#[test]
fn backfill_uses_independent_event_time_coverage_after_eviction() {
    let d = domain();
    let end = Utc::now();
    let start = end - Duration::seconds(10);
    // Force one eviction in each ring. The log falls inside the interval.
    // The evicted span ended before the execution. Both floors are positive.
    for i in 0..101 {
        let mut l = LogEntry::synthetic(Level::Info, "test");
        l.timestamp = start + Duration::microseconds(i);
        l.seq = d.pipeline.assign_seq();
        d.pipeline.append_to_store(l);
        let mut s = span();
        s.start_time = start - Duration::seconds(2);
        s.end_time = start - Duration::seconds(1);
        d.span_store.insert(s);
    }
    let mut summary = Summary::new("trace_id", "a", &[]);
    let view = summary.backfill(&d, start, end);
    assert_eq!(view["log_coverage"]["status"], "evicted");
    assert_eq!(view["span_coverage"]["status"], "retained");
}

#[test]
fn bounded_schema_and_value_memory_and_generic_backfill_interval() {
    let d = domain();
    let start = Utc::now();
    for delta in [-1, 0, 2] {
        let mut s = span();
        s.start_time = start + Duration::seconds(delta);
        s.end_time = s.start_time;
        s.attributes.insert("group".to_owned(), json!("worker"));
        s.attributes
            .insert("huge".to_owned(), json!("X".repeat(65000)));
        d.span_store.insert(s);
    }
    let mut summary = Summary::new("group", "worker", &["huge".into(), "X".repeat(65000)]);
    summary.backfill(&d, start, start + Duration::seconds(1));
    let view = summary.json();
    assert_eq!(view["omitted_attribute_keys"], 1);
    assert_eq!(view["distinct_values"]["huge"]["truncated"], true);
    assert_eq!(view["spans"]["kernel"]["count"], 1);
    assert!(view.to_string().len() < 1000);
    assert_eq!(d.span_store.total_received(), 3);
    d.span_store.clear();
    assert_eq!(d.span_store.total_received(), 3);
}

#[test]
fn eight_overlapping_large_snapshots_round_trip_after_both_rings_roll() {
    let d = domain();
    let now = Utc::now();
    let original_budget = d.test_log.capture_pool.lock().unwrap().available();
    // Each case covers more than 16 MiB. Eight deep copies would also exceed
    // the entire 128 MiB pool before any case had reached its worker.
    for i in 0..18 {
        let mut entry = LogEntry::synthetic(Level::Info, &format!("shared row {i}"));
        entry.full_message = Some("z".repeat(1024 * 1024));
        entry.seq = d.pipeline.assign_seq();
        entry.timestamp = now;
        d.pipeline.append_to_store(entry);
    }
    let mut event = span();
    event.start_time = now;
    event.end_time = now;
    d.span_store.insert(event);
    let create = || CaptureSnapshot::new(&d, Selector::new("trace_id", "a"), now, now);
    let first = create();
    let after_first = d.test_log.capture_pool.lock().unwrap().available();
    let mut cases = vec![first];
    cases.extend((1..8).map(|_| create()));
    let after_eight = d.test_log.capture_pool.lock().unwrap().available();
    assert!(original_budget - after_first > 16 * 1024 * 1024);
    assert!(after_first - after_eight < 32 * 1024);
    d.pipeline.clear_logs();
    d.span_store.clear();
    let dir = tempfile::tempdir().unwrap();
    for case in cases {
        assert!(!case.is_over_budget());
        let path = case
            .write(
                dir.path(),
                &json!({"name":"burst","reason":"failure"}),
                vec![],
            )
            .unwrap();
        let loaded = cases::load::load(&path).unwrap();
        let logs = loaded.logs.unwrap();
        assert_eq!(logs.len(), 19);
        assert!(logs[..18]
            .iter()
            .all(|l| l.full_message.as_ref().unwrap().len() == 1024 * 1024));
        assert_eq!(loaded.spans.unwrap().len(), 1);
    }
    assert_eq!(
        d.test_log.capture_pool.lock().unwrap().available(),
        original_budget
    );
}

#[test]
fn pending_capture_memory_counts_shared_records_once_after_resize_and_clear() {
    let d = domain();
    let now = Utc::now();
    let mut log = LogEntry::synthetic(Level::Info, &"r".repeat(4096));
    log.trace_id = Some(10);
    log.seq = d.pipeline.assign_seq();
    let log_bytes = super::log_bytes(&log);
    let span = span();
    let span_bytes = super::span_bytes(&span);
    d.pipeline.append_to_store(log);
    d.span_store.insert(span);
    let snap = || {
        CaptureSnapshot::new(
            &d,
            Selector::new("trace_id", "a"),
            now - Duration::seconds(1),
            now + Duration::seconds(1),
        )
    };
    let first = snap();
    let second = snap();
    let pending = || d.test_log.capture_pool.lock().unwrap().exclusive_bytes();
    let indexes = pending();
    assert!(indexes < log_bytes);
    assert_eq!(d.pipeline.store.ring_info().1, log_bytes);
    d.pipeline.store.resize_with(Some(0), None, || {});
    assert_eq!(pending(), indexes + log_bytes);
    d.span_store.clear();
    assert_eq!(pending(), indexes + log_bytes + span_bytes);
    drop(first);
    assert_eq!(pending(), indexes / 2 + log_bytes + span_bytes);
    drop(second);
    assert_eq!(pending(), 0);
}
