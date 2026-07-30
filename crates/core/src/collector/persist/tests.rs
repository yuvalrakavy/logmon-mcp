//! §12 V11: definition *and* history survive a restart.
//!
//! Every test writes into its own temp dir. **Nothing here may touch
//! `config_dir()`** — a developer's live daemon reads that path, and a test
//! that wrote there would corrupt a running broker.

use super::*;
use crate::collector::state::{Collector, DEFAULT_MAX_SAMPLE_BYTES};
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::TimeZone;
use std::collections::HashMap;

const S: i64 = 1_700_000_000_000_000_000;
const MS: i64 = 1_000_000;

fn tmp() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(S + secs * 1_000_000_000)
}

fn def() -> CollectorDef {
    CollectorDef {
        name: "perf".into(),
        filter_string: "sv=svc".into(),
        filter: parse_filter("sv=svc").unwrap(),
        level: Level::Tree,
        group_keys: vec!["cache.enabled".into()],
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: Some("the read-through cache".into()),
        threshold: None,
    }
}

fn span(id: u64, dur_ns: i64) -> SpanEntry {
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: id,
        parent_span_id: None,
        start_time: Utc.timestamp_nanos(S),
        end_time: Utc.timestamp_nanos(S + dur_ns),
        duration_ms: 0.0,
        name: "op".into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

/// A collector that ingested `n` spans, snapshotted once.
fn one_run(label: &str, n: usize, each_ms: i64) -> StoredSnapshot {
    let c = Collector::new(def(), at(0));
    for i in 0..n {
        c.ingest(&span(i as u64 + 1, each_ms * MS));
    }
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    StoredSnapshot::from_view(
        label.into(),
        Some("baseline".into()),
        serde_json::json!({ "commit": "abc123" }),
        at(60),
        SnapshotPolicy::default(),
        view,
        Some(TraceIngestLoss {
            dropped: 1,
            shed_batches: 2,
            malformed: 3,
        }),
        projected,
    )
}

fn file_with(snapshots: Vec<StoredSnapshot>) -> PersistedCollector {
    PersistedCollector {
        version: FORMAT_VERSION,
        name: "perf".into(),
        owner: "sess".into(),
        domain: "t3".into(),
        filter: "sv=svc".into(),
        level: "tree".into(),
        group_keys: vec!["cache.enabled".into()],
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: Some("the read-through cache".into()),
        threshold: None,
        armed_at: at(0),
        next_auto_label: snapshots.len() as u64 + 1,
        snapshots_evicted: 0,
        snapshots: snapshots.iter().map(PersistedSnapshot::of).collect(),
    }
}

// ---------------------------------------------------------------------------
// V11: the round trip
// ---------------------------------------------------------------------------

#[test]
fn v11_definition_and_history_both_survive_a_write_and_reload() {
    // The defect this shape exists for: making snapshots write-through while
    // leaving the definition to graceful shutdown produced, after a kill -9,
    // orphan history with nothing to attach it to.
    let d = tmp();
    let file = file_with(vec![one_run("baseline", 10, 10), one_run("cached", 10, 4)]);
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    assert!(out.quarantined.is_empty(), "{:?}", out.quarantined);
    assert_eq!(out.collectors.len(), 1);
    let back = &out.collectors[0];

    // Definition.
    assert_eq!(back.name, "perf");
    assert_eq!(back.filter, "sv=svc");
    assert_eq!(back.level, "tree");
    assert_eq!(back.group_keys, vec!["cache.enabled"]);
    assert_eq!(back.description.as_deref(), Some("the read-through cache"));
    // The pinned domain: without it, session restore hard-codes `default` and
    // every restored collector would silently re-pin.
    assert_eq!(back.domain, "t3");

    // History.
    let (history, errors) = restore_history(back);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(history.len(), 2);
    let first = history.get("baseline").expect("by label");
    assert_eq!(first.total.count, 10);
    assert_eq!(first.total_ms(), 100.0);
    assert_eq!(first.description.as_deref(), Some("baseline"));
    assert_eq!(first.meta["commit"], "abc123");
    assert_eq!(first.def.filter_string, "sv=svc", "its own definition");
}

#[test]
fn a_restored_sketch_still_answers_percentiles() {
    // The scalars are easy; the sketch is the part that has to survive a
    // binary encoding, and without it `estimated` is gone after every restart.
    let d = tmp();
    let c = Collector::new(def(), at(0));
    for i in 1..=100 {
        c.ingest(&span(i, i as i64 * MS));
    }
    let view = c.swap(at(60));
    let live_p95 = view.total.quantile_ns(0.95).expect("live");
    let file = file_with(vec![StoredSnapshot::from_view(
        "r".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        None,
        crate::collector::project::Projected::default(),
    )]);
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    let (history, errors) = restore_history(&out.collectors[0]);
    assert!(errors.is_empty(), "{errors:?}");
    let restored = history.get("r").unwrap();
    let back_p95 = restored.total.quantile_ns(0.95).expect("restored");
    assert!(
        (back_p95 - live_p95).abs() < 1.0,
        "p95 changed across persistence: {live_p95} -> {back_p95}"
    );
    assert_eq!(restored.total.count, 100);
}

#[test]
fn a_nanosecond_total_past_2_to_the_53_survives_the_json_round_trip() {
    // JSON numbers are f64 in most parsers, which silently rounds past 2^53 —
    // about 104 days of summed span time, well inside what a long-running
    // collector reaches. Hence the string.
    let huge: i128 = 9_007_199_254_740_993; // 2^53 + 1
    let stats =
        ExactStats::from_parts(1, huge, Some(1), Some(1), 0, 0, 0, 0, DurationSketch::new());
    let p = PersistedExact::of(&stats);
    let json = serde_json::to_string(&p).unwrap();
    let back: PersistedExact = serde_json::from_str(&json).unwrap();
    assert_eq!(back.restore().unwrap().total_ns, huge);
}

#[test]
fn the_auto_label_counter_survives_so_a_restart_does_not_reissue_snapshot_1() {
    // Otherwise a restarted daemon labels the next run `snapshot-1` when that
    // name already means something else — exactly the ambiguity the
    // never-reuse rule exists to prevent, reintroduced by a restart.
    let d = tmp();
    let mut file = file_with(vec![
        one_run("snapshot-1", 1, 10),
        one_run("snapshot-2", 1, 10),
    ]);
    file.next_auto_label = 3;
    file.snapshots_evicted = 7;
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    let (mut history, _) = restore_history(&out.collectors[0]);
    assert_eq!(history.next_auto(), 3);
    assert_eq!(history.reserve_label(None).unwrap(), "snapshot-3");
    assert_eq!(history.evicted(), 7, "and the eviction count is not lost");
}

// ---------------------------------------------------------------------------
// Failure handling
// ---------------------------------------------------------------------------

#[test]
fn an_unparseable_file_is_quarantined_and_the_others_still_load() {
    let d = tmp();
    save(d.path(), &file_with(vec![one_run("good", 5, 10)])).expect("saved");
    let mut bad = file_with(vec![]);
    bad.name = "broken".into();
    save(d.path(), &bad).expect("saved");
    std::fs::write(collector_path(d.path(), "sess", "broken"), "{ not json").unwrap();

    let out = load_all(d.path());
    assert_eq!(out.collectors.len(), 1, "the good one still loaded");
    assert_eq!(out.collectors[0].name, "perf");
    assert_eq!(out.quarantined.len(), 1);
    assert!(out.quarantined[0].1.contains("unparseable"));
    assert!(
        out.quarantined[0].0.exists(),
        "moved aside, never deleted — it is the only record of those runs"
    );
}

#[test]
fn a_file_from_a_newer_build_is_refused_rather_than_partially_read() {
    // Reading it would drop fields this build does not know, and the next
    // write would persist the truncation — a downgrade turning into silent
    // data loss.
    let d = tmp();
    let mut file = file_with(vec![one_run("r", 1, 10)]);
    file.version = FORMAT_VERSION + 1;
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    assert!(out.collectors.is_empty());
    assert_eq!(out.quarantined.len(), 1);
    assert!(out.quarantined[0].1.contains("newer logmon"));
}

#[test]
fn one_unreadable_snapshot_does_not_condemn_the_rest_of_the_file() {
    let d = tmp();
    let mut file = file_with(vec![one_run("good", 5, 10), one_run("bad", 5, 10)]);
    // Corrupt only the second snapshot's recorded level.
    file.snapshots[1].level = "quantum".into();
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    let (history, errors) = restore_history(&out.collectors[0]);
    assert_eq!(history.len(), 1, "the readable run survived");
    assert!(history.get("good").is_ok());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("bad") && errors[0].contains("quantum"));
}

#[test]
fn a_sketch_layout_this_build_does_not_know_is_refused() {
    // The guard §6.5 needs and that is unreachable in-process, since
    // SketchLayout::CURRENT is a constant. Only a persisted file can carry a
    // layout this build cannot compute with.
    let d = tmp();
    let mut file = file_with(vec![one_run("r", 5, 10)]);
    file.snapshots[0].total.layout.algo = "t-digest".into();
    save(d.path(), &file).expect("saved");

    let out = load_all(d.path());
    let (history, errors) = restore_history(&out.collectors[0]);
    assert!(history.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("layout"), "got: {:?}", errors);
}

#[test]
fn an_empty_or_absent_directory_loads_nothing_and_is_not_an_error() {
    let d = tmp();
    let out = load_all(d.path());
    assert!(out.collectors.is_empty());
    assert!(out.quarantined.is_empty());
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

#[test]
fn two_sessions_may_each_hold_a_collector_of_the_same_name() {
    let a = collector_path(Path::new("/d"), "sess-a", "perf");
    let b = collector_path(Path::new("/d"), "sess-b", "perf");
    assert_ne!(a, b);
    assert!(a.to_string_lossy().contains("collectors/"));
}

#[test]
fn a_session_name_cannot_walk_out_of_the_collectors_directory() {
    // Collector names are validated at add; session ids arrive from a wider
    // surface, so anything unexpected is folded rather than trusted.
    let p = collector_path(Path::new("/d"), "../../etc", "perf");
    let s = p.to_string_lossy();
    assert!(!s.contains(".."), "got: {s}");
    assert!(s.contains("collectors/"));
}

#[test]
fn delete_removes_the_file_and_is_safe_to_repeat() {
    let d = tmp();
    save(d.path(), &file_with(vec![])).expect("saved");
    assert!(delete(d.path(), "sess", "perf"));
    assert!(load_all(d.path()).collectors.is_empty());
    assert!(!delete(d.path(), "sess", "perf"), "already gone");
}
