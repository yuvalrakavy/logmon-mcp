use chrono::Utc;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::filter::parser::parse_filter;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use std::collections::HashMap;

fn make_entry(seq: u64, level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq,
        timestamp: Utc::now(),
        level,
        message: msg.to_string(),
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
fn test_seq_counter() {
    let pipeline = LogPipeline::new(1000);
    assert_eq!(pipeline.assign_seq(), 1);
    assert_eq!(pipeline.assign_seq(), 2);
    assert_eq!(pipeline.assign_seq(), 3);
}

#[test]
fn test_new_with_seq() {
    let pipeline = LogPipeline::new_with_seq(1000, 5000);
    assert_eq!(pipeline.assign_seq(), 5001);
}

#[test]
fn test_store_and_query() {
    let pipeline = LogPipeline::new(1000);
    pipeline.append_to_store(make_entry(1, Level::Info, "hello"));
    pipeline.append_to_store(make_entry(2, Level::Error, "bad"));
    assert_eq!(pipeline.store_len(), 2);

    let all = pipeline.recent_logs(10, None, false);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].seq, 2); // newest first

    let filter = parse_filter("l>=ERROR").unwrap();
    let errors = pipeline.recent_logs(10, Some(&filter), false);
    assert_eq!(errors.len(), 1);
}

#[test]
fn test_contains_seq() {
    let pipeline = LogPipeline::new(1000);
    pipeline.append_to_store(make_entry(42, Level::Info, "hello"));
    assert!(pipeline.contains_seq(42));
    assert!(!pipeline.contains_seq(99));
}

#[test]
fn test_pre_buffer() {
    let pipeline = LogPipeline::new(1000);
    pipeline.resize_pre_buffer(5);
    for i in 1..=3 {
        pipeline.pre_buffer_append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let copied = pipeline.pre_buffer_copy(2);
    assert_eq!(copied.len(), 2);
    assert_eq!(copied[0].seq, 2);
    assert_eq!(copied[1].seq, 3);
}

#[test]
fn test_clear_logs() {
    let pipeline = LogPipeline::new(1000);
    pipeline.append_to_store(make_entry(1, Level::Info, "hello"));
    let cleared = pipeline.clear_logs();
    assert_eq!(cleared, 1);
    assert_eq!(pipeline.store_len(), 0);
}

#[test]
fn test_context_by_seq() {
    let pipeline = LogPipeline::new(1000);
    for i in 1..=10 {
        pipeline.append_to_store(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let ctx = pipeline.context_by_seq(5, 2, 2);
    assert_eq!(ctx.len(), 5);
    assert_eq!(ctx[0].seq, 3);
    assert_eq!(ctx[4].seq, 7);
}

#[test]
fn test_increment_malformed() {
    let pipeline = LogPipeline::new(1000);
    pipeline.increment_malformed();
    pipeline.increment_malformed();
    let stats = pipeline.store_stats();
    assert_eq!(stats.malformed_count, 2);
}

/// A `recent` reply's floor and bounds describe the instant of its query (gh #24). Read
/// afterwards, an eviction in between moved the floor past a record the reply still returned —
/// `logs.export` then graded the window `evicted` over a record it handed out. A writer churns
/// a 4-record ring while the reader checks every reply: no returned record lies below the
/// reply's floor, and every one is inside the reply's bounds.
#[test]
fn a_recent_reply_never_returns_a_record_below_its_own_floor() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let pipeline = Arc::new(LogPipeline::new(4));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (p, stop) = (pipeline.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut seq = 0;
            while !stop.load(Ordering::Relaxed) {
                seq += 1;
                p.append_to_store(make_entry(seq, Level::Info, "x"));
            }
        })
    };
    let mut checked = 0;
    for _ in 0..200_000 {
        let (entries, stats) = pipeline.recent_logs_with_stats(4, None, true);
        for e in &entries {
            assert!(
                e.seq >= stats.lost_below,
                "seq {} returned with floor {}",
                e.seq,
                stats.lost_below
            );
            assert!(
                stats.buffer_oldest_seq.is_some_and(|o| e.seq >= o)
                    && stats.buffer_newest_seq.is_some_and(|n| e.seq <= n),
                "seq {} outside the reply's bounds {:?}..{:?}",
                e.seq,
                stats.buffer_oldest_seq,
                stats.buffer_newest_seq
            );
            checked += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(checked > 0, "the reader saw records");
}
