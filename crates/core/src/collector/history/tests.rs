//! §12: V14–V17 snapshot/history/merge/descriptions, A10 truncation not
//! laundered into a snapshot, A12 history eviction.

use super::*;
use crate::collector::state::{Collector, DEFAULT_MAX_SAMPLE_BYTES};
use crate::filter::parser::parse_filter;
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::TimeZone;
use std::collections::HashMap;

const S: i64 = 1_700_000_000_000_000_000;
const MS: i64 = 1_000_000;

fn def(level: Level) -> CollectorDef {
    CollectorDef {
        name: "c".into(),
        filter_string: "sv=svc".into(),
        filter: parse_filter("sv=svc").unwrap(),
        level,
        group_keys: vec![],
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: None,
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

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(S + secs * 1_000_000_000)
}

/// A snapshot of a collector that ingested `n` spans of `each_ms`.
fn taken(label: &str, n: usize, each_ms: i64, when: DateTime<Utc>) -> StoredSnapshot {
    let c = Collector::new(def(Level::Tree), at(0));
    for i in 0..n {
        c.ingest(&span(i as u64 + 1, each_ms * MS));
    }
    StoredSnapshot::from_view(
        label.into(),
        None,
        serde_json::Value::Null,
        when,
        SnapshotPolicy::default(),
        c.swap(when),
        None,
        Projected::default(),
    )
}

// ---------------------------------------------------------------------------
// Labels (V14)
// ---------------------------------------------------------------------------

#[test]
fn auto_labels_are_sequential_and_never_reuse_a_number_after_eviction() {
    // A12 plus the label rule. `snapshot-3` naming two different runs over a
    // collector's life would make every note that mentions it ambiguous.
    let mut h = History::with_max(2);
    for i in 0..4 {
        let l = h.reserve_label(None).expect("auto label");
        assert_eq!(l, format!("snapshot-{}", i + 1));
        h.push(taken(&l, 1, 10, at(i as i64)));
    }
    assert_eq!(h.len(), 2, "capped");
    assert_eq!(h.evicted(), 2, "and it says how many went");
    let labels: Vec<&str> = h.all().iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, vec!["snapshot-3", "snapshot-4"], "oldest evicted");

    // The next auto label continues past the evicted numbers.
    assert_eq!(h.reserve_label(None).unwrap(), "snapshot-5");
}

#[test]
fn an_auto_label_skips_a_number_a_caller_took_by_hand() {
    let mut h = History::new();
    let manual = h.reserve_label(Some("snapshot-1")).expect("explicit");
    h.push(taken(&manual, 1, 10, at(0)));
    assert_eq!(
        h.reserve_label(None).unwrap(),
        "snapshot-2",
        "auto generation must not collide with an explicit label"
    );
}

#[test]
fn a_duplicate_label_is_refused_so_a_reference_is_never_ambiguous() {
    let mut h = History::new();
    h.push(taken("baseline", 1, 10, at(0)));
    assert_eq!(
        h.reserve_label(Some("baseline")),
        Err(HistoryError::DuplicateLabel("baseline".into()))
    );
}

#[test]
fn labels_reject_characters_that_would_break_the_collector_at_label_syntax() {
    let mut h = History::new();
    for bad in ["with@sign", "with/slash", "", "with space"] {
        assert!(
            h.reserve_label(Some(bad)).is_err(),
            "`{bad}` must be refused"
        );
    }
    for good in ["baseline", "v1.2.3", "run_4", "with-dash"] {
        assert!(is_valid_label(good), "`{good}` must be accepted");
    }
}

#[test]
fn an_unknown_label_lists_the_ones_that_exist() {
    let mut h = History::new();
    h.push(taken("baseline", 1, 10, at(0)));
    h.push(taken("cached", 1, 10, at(1)));
    let err = h.get("cachd").unwrap_err().to_string();
    assert!(
        err.contains("baseline") && err.contains("cached"),
        "got: {err}"
    );
}

#[test]
fn an_unknown_label_on_an_empty_history_says_there_are_none() {
    let h = History::new();
    let err = h.get("anything").unwrap_err().to_string();
    assert!(err.contains("none yet"), "got: {err}");
}

// ---------------------------------------------------------------------------
// What a snapshot records (V15, V17, §6.3)
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_carries_its_own_definition_not_the_live_one() {
    // The case this exists for: the collector is edited between runs. A
    // comparison that read the CURRENT definition would present it as proof of
    // what the recorded run measured.
    let c = Collector::new(def(Level::Tree), at(0));
    c.ingest(&span(1, 10 * MS));
    let snap = StoredSnapshot::from_view(
        "baseline".into(),
        Some("before the cache".into()),
        serde_json::json!({ "commit": "abc123" }),
        at(60),
        SnapshotPolicy::default(),
        c.swap(at(60)),
        None,
        Projected::default(),
    );

    assert_eq!(snap.def.filter_string, "sv=svc");
    assert_eq!(snap.def.level, Level::Tree);
    assert_eq!(snap.description.as_deref(), Some("before the cache"));
    assert_eq!(snap.meta["commit"], "abc123");
    assert_eq!(snap.total.count, 1);
    assert_eq!(snap.total_ms(), 10.0);
}

#[test]
fn v10_a_snapshots_wall_ms_covers_its_own_window_not_the_collectors_life() {
    let c = Collector::new(def(Level::Timing), at(0));
    c.ingest(&span(1, 10 * MS));
    // First window: 0s to 60s.
    let first = StoredSnapshot::from_view(
        "a".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        c.swap(at(60)),
        None,
        Projected::default(),
    );
    assert_eq!(first.wall_ms, 60_000.0);

    // Second window starts at the zeroing, so it is 30s, not 90.
    c.ingest(&span(2, 10 * MS));
    let second = StoredSnapshot::from_view(
        "b".into(),
        None,
        serde_json::Value::Null,
        at(90),
        SnapshotPolicy::default(),
        c.swap(at(90)),
        None,
        Projected::default(),
    );
    assert_eq!(second.wall_ms, 30_000.0);
}

#[test]
fn a10_truncation_travels_with_the_snapshot() {
    // A snapshot must not launder a truncated run into a clean-looking record:
    // the whole point of history is comparing runs, and a prefix is a
    // different population from a complete one.
    use crate::collector::sample::CHUNK_RECORDS;
    let mut d = def(Level::Timing);
    d.max_sample_bytes = CHUNK_RECORDS * Level::Timing.bytes_per_record();
    let c = Collector::new(d, at(0));
    for i in 0..(CHUNK_RECORDS * 2) {
        c.ingest(&span(i as u64 + 1, 10 * MS));
    }
    let snap = StoredSnapshot::from_view(
        "truncated".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        c.swap(at(60)),
        None,
        Projected::default(),
    );
    assert!(!snap.sample_complete, "the record says the sample was cut");
    assert_eq!(
        snap.total.count,
        (CHUNK_RECORDS * 2) as u64,
        "while the exact tier still covers every span"
    );
}

#[test]
fn a_policy_can_drop_the_per_axis_breakdowns_but_never_the_core() {
    let c = Collector::new(def(Level::Timing), at(0));
    c.ingest(&span(1, 10 * MS));
    let snap = StoredSnapshot::from_view(
        "lean".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy {
            per_name: false,
            per_group: false,
            projections: false,
        },
        c.swap(at(60)),
        None,
        Projected::default(),
    );
    assert!(snap.per_name.is_none());
    assert!(snap.per_group.is_none());
    // The core is not optional: without it a later reader has to take these
    // from the live collector and call them evidence.
    assert_eq!(snap.total.count, 1);
    assert_eq!(snap.def.filter_string, "sv=svc");
}

#[test]
fn a_policy_asking_for_projections_below_timing_is_an_error() {
    let p = SnapshotPolicy::default();
    let err = p.validate(Level::Scalar).unwrap_err();
    assert!(
        err.contains("scalar") && err.contains("projections"),
        "got: {err}"
    );
    assert!(p.validate(Level::Timing).is_ok());
    assert!(p.validate(Level::Tree).is_ok());
    // Not asking for them is fine at any level.
    let lean = SnapshotPolicy {
        projections: false,
        ..Default::default()
    };
    assert!(lean.validate(Level::Scalar).is_ok());
}

// ---------------------------------------------------------------------------
// Merging (V16, §6.5)
// ---------------------------------------------------------------------------

#[test]
fn merging_repeats_adds_the_exact_scalars() {
    let runs = vec![
        taken("r1", 10, 10, at(10)),
        taken("r2", 10, 10, at(20)),
        taken("r3", 10, 10, at(30)),
    ];
    let merged = merge(&runs).expect("same layout");
    assert_eq!(merged.count, 30);
    assert_eq!(ns_to_ms(merged.total_ns), 300.0);
    assert_eq!(merged.avg_ns().unwrap() / 1_000_000.0, 10.0);
    // The merged sketch answers over the combined population.
    assert!(merged.quantile_ns(0.5).is_some());
}

#[test]
fn merging_nothing_is_an_error_not_an_empty_result() {
    assert!(matches!(merge(&[]), Err(MergeError::Empty)));
}

#[test]
fn a_single_snapshot_merges_to_itself() {
    let runs = vec![taken("only", 5, 20, at(10))];
    let merged = merge(&runs).unwrap();
    assert_eq!(merged.count, 5);
    assert_eq!(ns_to_ms(merged.total_ns), 100.0);
}

// ---------------------------------------------------------------------------
// The run-to-run floor (§6.5)
// ---------------------------------------------------------------------------

#[test]
fn the_floor_reports_spread_across_repeats() {
    // Three runs at 100, 110, 120 ms total.
    let runs = vec![
        taken("r1", 10, 10, at(10)),
        taken("r2", 11, 10, at(20)),
        taken("r3", 12, 10, at(30)),
    ];
    let f = RunToRunFloor::over_total_ms(&runs).expect("three runs");
    assert_eq!(f.runs, 3);
    assert_eq!(f.min, 100.0);
    assert_eq!(f.max, 120.0);
    assert_eq!(f.mean, 110.0);
    // Sample sd of {100,110,120} is 10, so CV is 9.09%.
    let cv = f.cv_pct.expect("three runs give a spread");
    assert!((cv - 9.0909).abs() < 0.001, "got {cv}");
}

#[test]
fn a_single_run_reports_the_floor_as_unknown_never_as_zero() {
    // Zero would license calling any difference significant.
    let runs = vec![taken("only", 10, 10, at(10))];
    let f = RunToRunFloor::over_total_ms(&runs).expect("one run still has a value");
    assert_eq!(f.runs, 1);
    assert_eq!(f.mean, 100.0);
    assert_eq!(f.cv_pct, None, "unknown, and the shape says so");
}

#[test]
fn identical_repeats_report_a_zero_floor_which_is_a_real_measurement() {
    // Distinct from the single-run case: here we DID look and found no spread.
    let runs = vec![taken("r1", 10, 10, at(10)), taken("r2", 10, 10, at(20))];
    let f = RunToRunFloor::over_total_ms(&runs).unwrap();
    assert_eq!(f.cv_pct, Some(0.0));
}

#[test]
fn the_floor_states_what_it_does_not_cover() {
    // A bare spread invites being read as a total error bar.
    assert!(RunToRunFloor::CAVEAT.contains("ingest loss"));
    assert!(RunToRunFloor::CAVEAT.contains("truncation"));
}
