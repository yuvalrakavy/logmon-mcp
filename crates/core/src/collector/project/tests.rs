//! §12: V3 percentiles, V4/V4b self time, V5 wall union, V6 paths, V7 groups,
//! V8 warm-up, V25 zero and negative durations.
//!
//! Every case builds real spans and runs them through `Collector::ingest`, so
//! these test the projection over the data model as it is actually written —
//! not over a hand-built snapshot that could drift from it.

use super::*;
use crate::collector::sample::Level;
use crate::collector::state::{Collector, CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
use crate::filter::parser::parse_filter;
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::TimeZone;
use std::collections::HashMap;

/// A round base so nanosecond offsets in the tests read as themselves.
const S: i64 = 1_700_000_000_000_000_000;
const MS: i64 = 1_000_000;

fn def(level: Level, group_keys: &[&str]) -> CollectorDef {
    CollectorDef {
        name: "c".into(),
        filter_string: "sv=svc".into(),
        filter: parse_filter("sv=svc").unwrap(),
        level,
        group_keys: group_keys.iter().map(|s| s.to_string()).collect(),
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: None,
        threshold: None,
    }
}

/// `span(name, id, parent, start_ns, end_ns)` — offsets from `S`.
fn span(name: &str, id: u64, parent: Option<u64>, start: i64, end: i64) -> SpanEntry {
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: id,
        parent_span_id: parent,
        start_time: Utc.timestamp_nanos(S + start),
        end_time: Utc.timestamp_nanos(S + end),
        duration_ms: 0.0,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

fn now() -> DateTime<Utc> {
    Utc.timestamp_nanos(S)
}

fn read_at() -> DateTime<Utc> {
    Utc.timestamp_nanos(S + 10 * 1_000_000_000)
}

fn run(level: Level, spans: &[SpanEntry], opts: ProfileOptions) -> ProfileResult {
    let c = Collector::new(def(level, &[]), now());
    for s in spans {
        c.ingest(s);
    }
    profile(&c.snapshot(), IngestBasis::Unavailable, &opts, read_at())
}

fn suppression<'a>(r: &'a ProfileResult, field: &str) -> Option<&'a Suppressed> {
    r.suppressed.iter().find(|s| s.field == field)
}

// ---------------------------------------------------------------------------
// Percentiles (V3, §5.7)
// ---------------------------------------------------------------------------

#[test]
fn quantiles_follow_the_lower_convention_against_a_hand_computed_reference() {
    // n = 10, values 1..=10 ns. rank = floor(1 + q(n-1)), 1-indexed.
    // q=0.50 → floor(1 + 4.5) = 5 → 5th smallest = 5.
    // q=0.80 → floor(1 + 7.2) = 8 → 8.
    // q=0.95 → floor(1 + 8.55) = 9 → 9.
    // q=0.99 → floor(1 + 8.91) = 9 → 9.
    let v: Vec<i64> = (1..=10).collect();
    assert_eq!(quantile_sorted(&v, 0.50), Some(5));
    assert_eq!(quantile_sorted(&v, 0.80), Some(8));
    assert_eq!(quantile_sorted(&v, 0.95), Some(9));
    assert_eq!(quantile_sorted(&v, 0.99), Some(9));
    // The extremes are the extremes, not an interpolation past them.
    assert_eq!(quantile_sorted(&v, 0.0), Some(1));
    assert_eq!(quantile_sorted(&v, 1.0), Some(10));
    assert_eq!(quantile_sorted(&[], 0.5), None);
}

#[test]
fn a_single_sample_is_every_percentile() {
    assert_eq!(quantile_sorted(&[42], 0.50), Some(42));
    assert_eq!(quantile_sorted(&[42], 0.99), Some(42));
}

#[test]
fn sampled_percentiles_come_out_in_milliseconds() {
    let spans: Vec<SpanEntry> = (1..=10)
        .map(|i| span("t", i as u64, None, 0, i * MS))
        .collect();
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let s = r.sampled.expect("timing retains samples");
    assert_eq!(s.sample_count, 10);
    assert_eq!(s.p50_ms, Some(5.0));
    assert_eq!(s.p80_ms, Some(8.0));
    assert_eq!(s.p95_ms, Some(9.0));
}

// ---------------------------------------------------------------------------
// Self time (V4, V4b, §5.3)
// ---------------------------------------------------------------------------

#[test]
fn self_time_uses_the_union_of_concurrent_children_not_their_sum() {
    // The motivating case verbatim: a 100 ms parent with two children that run
    // concurrently for 60 ms each. Union = 60, so self = 40. The sum rule
    // would give 100 - 120 = -20, clamp to 0, and report a parent that did no
    // work of its own.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        span("child-a", 2, Some(1), 10 * MS, 70 * MS),
        span("child-b", 3, Some(1), 10 * MS, 70 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.expect("tree retains samples");

    // parent self 40 + child-a 60 + child-b 60 = 160.
    assert_eq!(s.self_ms, Some(160.0));
    assert_eq!(s.nested_matches, 2);
    assert_eq!(r.nesting, "detected");
    assert_eq!(
        s.overlapping_child_ms, 0.0,
        "nothing lay outside the parent"
    );
}

#[test]
fn partially_overlapping_children_union_rather_than_double_count() {
    // [10,60] ∪ [40,90] = [10,90] = 80 ms of a 100 ms parent → self 20.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        span("a", 2, Some(1), 10 * MS, 60 * MS),
        span("b", 3, Some(1), 40 * MS, 90 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    // 20 (parent) + 50 + 50 = 120.
    assert_eq!(s.self_ms, Some(120.0));
}

#[test]
fn a_child_outliving_its_parent_is_clipped_and_the_clipping_is_reported() {
    // Parent [0,100], child [50,150]: 50 ms of the child lies past the parent.
    // Clipped contribution is 50, so parent self = 50 — and the 50 ms that had
    // to be clipped away is an anomaly worth naming, not a rounding detail.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        span("late", 2, Some(1), 50 * MS, 150 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.self_ms, Some(150.0), "parent 50 + child 100");
    assert_eq!(s.overlapping_child_ms, 50.0);
    assert_eq!(s.overlapping_child_spans, 1);
}

#[test]
fn a_child_starting_before_its_parent_is_clipped_at_both_ends() {
    // OTLP permits this under clock skew, which is why both ends are clipped
    // rather than only the end Elastic's timer clips.
    let spans = [
        span("parent", 1, None, 50 * MS, 150 * MS),
        span("early", 2, Some(1), 0, 100 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    // Clipped child is [50,100] = 50 → parent self 50, child self 100.
    assert_eq!(s.self_ms, Some(150.0));
    assert_eq!(s.overlapping_child_ms, 50.0);
}

#[test]
fn a_child_entirely_outside_its_parent_cannot_inflate_self_time() {
    // Every other clip test uses a PARTIAL overlap. A fully disjoint child is
    // the case where dropping `Interval::clip`'s `start < end` guard produces a
    // degenerate interval whose "length" is negative — which, since the clamp
    // only protects the low end, would inflate the parent's self time PAST its
    // own duration. Found by mutation testing: the guard was load-bearing and
    // nothing exercised it.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        // Starts after the parent has already ended.
        span("elsewhere", 2, Some(1), 500 * MS, 600 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();

    // The child contributes nothing to the union, so the parent's self time is
    // its whole duration — never more.
    assert_eq!(
        s.self_ms,
        Some(200.0),
        "parent 100 (all its own) + child 100"
    );
    assert_eq!(s.overlapping_child_ms, 100.0, "all of it was clipped away");
    assert_eq!(s.overlapping_child_spans, 1);
}

#[test]
fn a_fully_nested_child_is_not_counted_as_clipped() {
    // `overlapping_child_spans` must count children that actually needed
    // clipping. Counting every child would make a clean run look anomalous, and
    // the millisecond figure alone cannot contradict it — adding zero leaves the
    // sum correct while the count lies.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        span("inside", 2, Some(1), 10 * MS, 20 * MS),
        span("also-inside", 3, Some(1), 30 * MS, 40 * MS),
    ];
    let s = run(Level::Tree, &spans, ProfileOptions::default())
        .sampled
        .unwrap();
    assert_eq!(s.overlapping_child_ms, 0.0);
    assert_eq!(
        s.overlapping_child_spans, 0,
        "nothing was clipped, so nothing may be reported as clipped"
    );
}

#[test]
fn v4b_one_large_clip_is_distinguishable_from_many_small_ones() {
    // Same total clipped mass, opposite remedies: one badly skewed clock
    // versus a systematic off-by-a-millisecond in the instrumentation.
    let one_big = [
        span("parent", 1, None, 0, 100 * MS),
        span("c", 2, Some(1), 50 * MS, 150 * MS),
    ];
    let mut many_small = vec![span("parent", 1, None, 0, 100 * MS)];
    for i in 0..50 {
        // Each child overruns the parent's end by exactly 1 ms.
        many_small.push(span("c", 10 + i as u64, Some(1), 99 * MS, 101 * MS));
    }

    let a = run(Level::Tree, &one_big, ProfileOptions::default())
        .sampled
        .unwrap();
    let b = run(Level::Tree, &many_small, ProfileOptions::default())
        .sampled
        .unwrap();

    assert_eq!(a.overlapping_child_ms, 50.0);
    assert_eq!(a.overlapping_child_spans, 1);
    assert_eq!(b.overlapping_child_ms, 50.0);
    assert_eq!(
        b.overlapping_child_spans, 50,
        "the millisecond figure alone cannot tell these two runs apart"
    );
}

#[test]
fn self_time_is_suppressed_with_a_reason_when_nothing_nested() {
    // Not zero, and not silently equal to total: with no matched parent
    // anywhere, self time carries no information about the program at all.
    let spans = [
        span("a", 1, None, 0, 10 * MS),
        span("b", 2, None, 20 * MS, 30 * MS),
    ];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.as_ref().unwrap();
    assert_eq!(s.self_ms, None);
    assert_eq!(s.nested_matches, 0);
    assert_eq!(r.nesting, "undetected");
    let sup = suppression(&r, "sampled.self_ms").expect("a reason is given");
    assert!(sup.reason.contains("equal total time"));
    assert!(sup.remedy.is_some(), "and something the caller can do");
}

#[test]
fn an_unmatched_parent_leaves_its_child_a_root_for_self_time() {
    // The child's parent exists in the program but not in the matched set, so
    // there is nothing to attribute the child's time away from — and nothing
    // nested, which is what makes self time uninformative here.
    let spans = [span("child", 2, Some(999), 0, 10 * MS)];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    assert_eq!(r.sampled.as_ref().unwrap().nested_matches, 0);
    assert_eq!(r.nesting, "undetected");
}

#[test]
fn a2_a_self_parenting_span_does_not_subtract_its_own_duration() {
    // Malformed, but it arrives over a network. Left unguarded this reports a
    // span with zero self time and one phantom nested match.
    let spans = [span("loop", 1, Some(1), 0, 10 * MS)];
    let r = run(Level::Tree, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.nested_matches, 0, "a span is not its own child");
    assert_eq!(s.self_ms, None, "so nothing nested, and self is suppressed");
}

#[test]
fn children_are_matched_within_their_trace_not_across_traces() {
    // Span ids are only required to be unique WITHIN a trace, and
    // instrumentation that numbers them sequentially per trace collides across
    // traces constantly. Keyed on span id alone, trace 99's span 1 would adopt
    // trace 7's span 2 as a child and subtract its time.
    let mut a = span("root", 1, None, 0, 100 * MS);
    a.trace_id = 7;
    let mut b = span("root", 1, None, 0, 100 * MS);
    b.trace_id = 99;
    let mut child = span("child", 2, Some(1), 0, 80 * MS);
    child.trace_id = 7;

    let r = run(Level::Tree, &[a, b, child], ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.nested_matches, 1, "one child, in one trace");
    // trace 7: root self 20 + child 80. trace 99: root self 100. Total 200.
    // Cross-trace adoption would subtract the child from BOTH roots → 120.
    assert_eq!(s.self_ms, Some(200.0));
}

#[test]
fn a10_a_truncated_sample_reports_concurrency_over_what_it_actually_holds() {
    use crate::collector::sample::CHUNK_RECORDS;
    // A budget for one chunk, then twice that many spans. The exact tier still
    // covers every span; the sample tier holds a prefix. Dividing the exact
    // total by the prefix's wall union would inflate concurrency by exactly
    // the truncation ratio — worst precisely when the sample is least
    // representative.
    let mut d = def(Level::Timing, &[]);
    d.max_sample_bytes = CHUNK_RECORDS * Level::Timing.bytes_per_record();
    let c = Collector::new(d, now());
    let n = CHUNK_RECORDS * 2;
    for i in 0..n {
        // Strictly serial: each span occupies its own millisecond.
        c.ingest(&span(
            "op",
            i as u64 + 1,
            None,
            i as i64 * MS,
            (i as i64 + 1) * MS,
        ));
    }

    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions::default(),
        read_at(),
    );
    let s = r.sampled.unwrap();
    assert!(!s.complete, "the budget stopped retention");
    assert!(s.sample_count < n as u64);
    assert_eq!(r.matched, n as u64, "the exact tier still saw them all");
    assert_eq!(
        s.achieved_concurrency,
        Some(1.0),
        "serial work reads as serial however much of it was retained"
    );
}

#[test]
fn nesting_is_unknown_rather_than_undetected_below_tree_level() {
    // "undetected" would read as "we looked and found none", which would let a
    // reader infer a flat call structure from a retention setting.
    let spans = [
        span("parent", 1, None, 0, 100 * MS),
        span("child", 2, Some(1), 10 * MS, 20 * MS),
    ];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    assert_eq!(r.nesting, "unknown");
    let sup = suppression(&r, "sampled.self_ms").expect("says why");
    assert!(sup.reason.contains("timing"));
}

// ---------------------------------------------------------------------------
// Wall union and concurrency (V5)
// ---------------------------------------------------------------------------

#[test]
fn union_length_merges_overlapping_touching_and_nested_intervals() {
    assert_eq!(union_len(&mut []), 0);
    assert_eq!(union_len(&mut [(0, 10)]), 10);
    assert_eq!(union_len(&mut [(0, 10), (20, 30)]), 20, "disjoint");
    assert_eq!(union_len(&mut [(0, 10), (5, 15)]), 15, "overlapping");
    assert_eq!(union_len(&mut [(0, 10), (10, 20)]), 20, "touching");
    assert_eq!(union_len(&mut [(0, 100), (10, 20)]), 100, "nested");
    // Out of order input must give the same answer as sorted input.
    assert_eq!(union_len(&mut [(20, 30), (0, 10), (5, 25)]), 30);
}

#[test]
fn achieved_concurrency_reports_parallelism_over_the_sampled_population() {
    // Four spans of 100 ms each, all in flight together: 400 ms of work in
    // 100 ms of wall time.
    let spans: Vec<SpanEntry> = (1..=4)
        .map(|i| span("worker", i, None, 0, 100 * MS))
        .collect();
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.wall_union_ms, Some(100.0));
    assert_eq!(s.achieved_concurrency, Some(4.0));
}

#[test]
fn fully_serial_work_reports_concurrency_of_one() {
    let spans: Vec<SpanEntry> = (0..4)
        .map(|i| span("step", i as u64 + 1, None, i * 100 * MS, (i + 1) * 100 * MS))
        .collect();
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.wall_union_ms, Some(400.0));
    assert_eq!(s.achieved_concurrency, Some(1.0));
}

// ---------------------------------------------------------------------------
// Warm-up (V8, §5.5)
// ---------------------------------------------------------------------------

#[test]
fn warmup_drops_early_samples_and_withholds_both_unwindowed_tiers() {
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
        span("warm", 3, None, 300 * MS, 310 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );

    let s = r.sampled.as_ref().unwrap();
    assert_eq!(s.sample_count, 2, "the first 100 ms of spans are excluded");
    assert_eq!(s.p50_ms, Some(10.0));

    // Both unwindowed tiers must be withheld, not reported beside a windowed
    // one — a reader comparing exact.avg_ms against sampled.p50_ms would be
    // comparing different populations with nothing in the shape to say so.
    assert!(r.exact.is_none());
    assert!(r.estimated.is_none());
    assert!(suppression(&r, "exact").is_some());
    assert!(suppression(&r, "estimated").is_some());
    // `matched` is a raw count and stays honest about the whole run.
    assert_eq!(r.matched, 3);
}

#[test]
fn the_warmup_origin_is_the_earliest_matched_start_not_the_window_start() {
    // Producer clock throughout. If the origin were the collector's arming
    // time, a run whose spans arrive late would have its warm-up cut miss
    // entirely — which is exactly the run most likely to need one.
    let spans = [
        span("a", 1, None, 5_000 * MS, 5_010 * MS),
        span("b", 2, None, 5_050 * MS, 5_060 * MS),
        span("c", 3, None, 5_200 * MS, 5_210 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    assert_eq!(
        r.sampled.unwrap().sample_count,
        1,
        "the cut is 100 ms after the first span, not after arming"
    );
}

// ---------------------------------------------------------------------------
// Groups (V7)
// ---------------------------------------------------------------------------

#[test]
fn group_by_name_ranks_by_total_time_and_carries_both_exact_and_estimated() {
    let spans = [
        span("slow", 1, None, 0, 100 * MS),
        span("fast", 2, None, 0, MS),
        span("fast", 3, None, 0, MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Name,
            ..Default::default()
        },
    );
    assert_eq!(r.grouped_by.as_deref(), Some("name"));
    assert_eq!(r.groups.len(), 2);
    assert_eq!(r.groups[0].key, "slow", "ranked by total time descending");
    assert_eq!(r.groups[0].exact.as_ref().unwrap().total_ms, 100.0);
    assert_eq!(r.groups[1].exact.as_ref().unwrap().count, 2);
    assert_eq!(
        r.groups[0].estimated.as_ref().unwrap().axis,
        "name",
        "the axis is named so a percentile is not read as the collector's"
    );
    assert!(
        r.groups[0].sampled.is_none(),
        "name rows are aggregated at ingest; there is no per-name sample view"
    );
}

#[test]
fn group_by_group_splits_a_boolean_attribute_into_two_arms() {
    // The driving A/B, and the case the span matcher's `.as_str()` view cannot
    // see: an OTLP BoolValue kill-switch.
    let c = Collector::new(def(Level::Timing, &["cache.enabled"]), now());
    for i in 0..4 {
        let mut on = span("op", i * 2 + 1, None, 0, 10 * MS);
        on.attributes
            .insert("cache.enabled".into(), serde_json::json!(true));
        let mut off = span("op", i * 2 + 2, None, 0, 50 * MS);
        off.attributes
            .insert("cache.enabled".into(), serde_json::json!(false));
        c.ingest(&on);
        c.ingest(&off);
    }
    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions {
            group_by: GroupBy::Group,
            ..Default::default()
        },
        read_at(),
    );

    assert_eq!(r.groups.len(), 2);
    assert_eq!(r.groups[0].key, "false", "the slower arm ranks first");
    assert_eq!(r.groups[0].exact.as_ref().unwrap().total_ms, 200.0);
    assert_eq!(r.groups[1].key, "true");
    assert_eq!(r.groups[1].exact.as_ref().unwrap().total_ms, 40.0);
}

#[test]
fn group_by_group_on_a_collector_without_group_keys_says_so() {
    let r = run(
        Level::Timing,
        &[span("a", 1, None, 0, MS)],
        ProfileOptions {
            group_by: GroupBy::Group,
            ..Default::default()
        },
    );
    assert!(r.groups.is_empty());
    let sup = suppression(&r, "groups").expect("a reason, not an empty list");
    assert!(sup.reason.contains("group_keys"));
}

#[test]
fn a_span_missing_the_group_key_is_labelled_absent_rather_than_dropped() {
    let c = Collector::new(def(Level::Timing, &["arm"]), now());
    let mut tagged = span("op", 1, None, 0, 10 * MS);
    tagged
        .attributes
        .insert("arm".into(), serde_json::json!("a"));
    c.ingest(&tagged);
    c.ingest(&span("op", 2, None, 0, 90 * MS));

    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions {
            group_by: GroupBy::Group,
            ..Default::default()
        },
        read_at(),
    );
    let keys: Vec<&str> = r.groups.iter().map(|g| g.key.as_str()).collect();
    assert!(keys.contains(&"__absent__"), "got {keys:?}");
    let sum: u64 = r
        .groups
        .iter()
        .map(|g| g.exact.as_ref().unwrap().count)
        .sum();
    assert_eq!(sum, r.matched, "the breakdown accounts for every match");
}

#[test]
fn group_by_trace_is_refused_below_tree_level_with_a_remedy() {
    let r = run(
        Level::Timing,
        &[span("a", 1, None, 0, MS)],
        ProfileOptions {
            group_by: GroupBy::Trace,
            ..Default::default()
        },
    );
    assert!(r.groups.is_empty());
    let sup = suppression(&r, "groups").expect("named, with the level in it");
    assert!(sup.reason.contains("timing"));
    assert!(sup.remedy.as_deref().unwrap().contains("tree"));
}

#[test]
fn top_n_truncates_the_row_list() {
    let spans: Vec<SpanEntry> = (0..10)
        .map(|i| span(&format!("op{i}"), i as u64 + 1, None, 0, (i + 1) * MS))
        .collect();
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Name,
            top_n: 3,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 3);
    assert_eq!(r.groups[0].key, "op9", "the three largest, in order");
    assert_eq!(r.groups[2].key, "op7");
}

// ---------------------------------------------------------------------------
// Call paths (V6, §5.4)
// ---------------------------------------------------------------------------

#[test]
fn paths_render_root_first_and_carry_self_time() {
    let spans = [
        span("handler", 1, None, 0, 100 * MS),
        span("db", 2, Some(1), 10 * MS, 40 * MS),
        span("db", 3, Some(1), 50 * MS, 60 * MS),
    ];
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Path,
            ..Default::default()
        },
    );
    let keys: Vec<&str> = r.groups.iter().map(|g| g.key.as_str()).collect();
    assert!(keys.contains(&"handler"), "got {keys:?}");
    assert!(keys.contains(&"handler > db"), "got {keys:?}");

    let db = r.groups.iter().find(|g| g.key == "handler > db").unwrap();
    assert_eq!(
        db.sampled.as_ref().unwrap().sample_count,
        2,
        "siblings fold"
    );
    assert_eq!(db.sampled.as_ref().unwrap().self_ms, Some(40.0));

    let handler = r.groups.iter().find(|g| g.key == "handler").unwrap();
    // 100 - union([10,40] ∪ [50,60]) = 100 - 40 = 60.
    assert_eq!(handler.sampled.as_ref().unwrap().self_ms, Some(60.0));
}

#[test]
fn a_path_whose_parent_is_not_matched_is_marked_rather_than_rooted() {
    // Silently rooting this at "orphan" would claim a call structure the data
    // does not support.
    let spans = [span("orphan", 2, Some(999), 0, 10 * MS)];
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Path,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].key, "[?] > orphan");
    assert!(r.groups[0].path_incomplete);
}

#[test]
fn a2_a_parent_cycle_terminates_and_is_marked_incomplete() {
    // 1 → 2 → 1. Without the visited set this walks to the depth cap and
    // renders a path that repeats; without the cap a longer cycle would spin.
    let spans = [
        span("a", 1, Some(2), 0, 100 * MS),
        span("b", 2, Some(1), 0, 100 * MS),
    ];
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Path,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 2);
    for g in &r.groups {
        assert!(g.path_incomplete, "{} must be marked", g.key);
        assert!(g.key.starts_with("[?] > "));
        assert!(
            g.key.matches('>').count() <= 2,
            "the cycle is cut, not walked: {}",
            g.key
        );
    }
}

#[test]
fn a_chain_deeper_than_the_cap_is_cut_and_marked() {
    let n = MAX_PATH_DEPTH + 10;
    let spans: Vec<SpanEntry> = (0..n)
        .map(|i| {
            span(
                &format!("f{i}"),
                i as u64 + 1,
                (i > 0).then_some(i as u64),
                0,
                100 * MS,
            )
        })
        .collect();
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Path,
            top_n: 1000,
            ..Default::default()
        },
    );
    let deepest = r
        .groups
        .iter()
        .max_by_key(|g| g.key.matches('>').count())
        .unwrap();
    assert!(deepest.path_incomplete);
    assert!(
        deepest.key.matches(" > ").count() <= MAX_PATH_DEPTH,
        "walk stopped at the cap"
    );
}

// ---------------------------------------------------------------------------
// Trace rollup
// ---------------------------------------------------------------------------

#[test]
fn group_by_trace_rolls_up_per_trace_without_claiming_an_exact_tier() {
    let mut spans = vec![span("a", 1, None, 0, 10 * MS)];
    let mut other = span("b", 2, None, 0, 40 * MS);
    other.trace_id = 99;
    spans.push(other);

    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Trace,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 2);
    for g in &r.groups {
        assert_eq!(g.key.len(), 32, "trace ids render as 32 hex digits");
        // No per-trace aggregate is kept at ingest, and inventing one from the
        // sample tier would be a different population under truncation.
        assert!(g.exact.is_none());
        assert!(g.estimated.is_none());
        assert!(g.sampled.is_some());
    }
}

// ---------------------------------------------------------------------------
// Levels, window, ingest (V25, §5.1)
// ---------------------------------------------------------------------------

#[test]
fn scalar_reports_the_exact_tier_and_says_why_there_is_no_sample_view() {
    let spans = [span("a", 1, None, 0, 10 * MS)];
    let r = run(Level::Scalar, &spans, ProfileOptions::default());
    assert!(r.sampled.is_none());
    assert_eq!(r.exact.as_ref().unwrap().count, 1);
    assert_eq!(r.exact.as_ref().unwrap().total_ms, 10.0);
    assert_eq!(r.nesting, "unknown");
    let sup = suppression(&r, "sampled").expect("named");
    assert!(sup.reason.contains("scalar"));
}

#[test]
fn v25_zero_duration_spans_are_counted_and_report_as_zero() {
    let spans = [
        span("instant", 1, None, 10 * MS, 10 * MS),
        span("slow", 2, None, 0, 100 * MS),
    ];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let e = r.exact.as_ref().unwrap();
    assert_eq!(e.count, 2);
    assert_eq!(e.min_ms, Some(0.0), "zero is a duration, not an error");
    assert_eq!(e.total_ms, 100.0);
    assert_eq!(r.sampled.as_ref().unwrap().sample_count, 2);
}

#[test]
fn v25_negative_durations_are_excluded_from_the_sum_and_counted() {
    let spans = [
        span("ok", 1, None, 0, 10 * MS),
        span("backwards", 2, None, 50 * MS, 40 * MS),
    ];
    let c = Collector::new(def(Level::Timing, &[]), now());
    for s in &spans {
        c.ingest(s);
    }
    let snap = c.snapshot();
    let r = profile(
        &snap,
        IngestBasis::Same {
            baseline: TraceIngestLoss::default(),
            current: TraceIngestLoss::default(),
        },
        &ProfileOptions::default(),
        read_at(),
    );

    assert_eq!(r.matched, 2, "it matched");
    assert_eq!(
        r.exact.as_ref().unwrap().total_ms,
        10.0,
        "but is not summed"
    );
    assert_eq!(r.exact.as_ref().unwrap().avg_ms, Some(10.0), "nor averaged");
    assert_eq!(r.ingest.as_ref().unwrap().negative_duration_spans, 1);
    assert_eq!(
        r.sampled.as_ref().unwrap().sample_count,
        1,
        "nor retained as a timing"
    );
}

#[test]
fn the_ingest_block_reports_a_window_delta_and_names_its_attribution() {
    let base = TraceIngestLoss {
        dropped: 5,
        shed_batches: 1,
        malformed: 2,
    };
    let now_counters = TraceIngestLoss {
        dropped: 9,
        shed_batches: 4,
        malformed: 2,
    };
    let c = Collector::new(def(Level::Timing, &[]), now());
    c.ingest(&span("a", 1, None, 0, MS));
    let r = profile(
        &c.snapshot(),
        IngestBasis::Same {
            baseline: base,
            current: now_counters,
        },
        &ProfileOptions::default(),
        read_at(),
    );

    let i = r.ingest.expect("same instance, so a delta is meaningful");
    assert_eq!(i.drops_in_window, 4, "only what happened in the window");
    assert_eq!(i.shed_batches, 3);
    assert_eq!(i.malformed_dropped, 0);
    assert_eq!(
        i.attribution, "domain",
        "stated, because the counters are unfiltered and per-domain"
    );
}

#[test]
fn a_recreated_domain_withholds_the_ingest_block_rather_than_guessing() {
    let r = run(
        Level::Timing,
        &[span("a", 1, None, 0, MS)],
        ProfileOptions::default(),
    );
    assert!(r.ingest.is_none());
    assert!(suppression(&r, "ingest").is_some());
}

#[test]
fn v10_wall_ms_runs_from_the_zeroing_not_from_arming() {
    let c = Collector::new(def(Level::Timing, &[]), now());
    c.ingest(&span("a", 1, None, 0, MS));
    let zeroed = Utc.timestamp_nanos(S + 4 * 1_000_000_000);
    c.swap(zeroed);
    c.ingest(&span("b", 2, None, 0, MS));

    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions::default(),
        read_at(),
    );
    // read_at is S+10s, zeroed at S+4s → 6 s of window, not 10.
    assert_eq!(r.window.wall_ms, 6000.0);
    assert_eq!(
        r.window.armed_at,
        Some(now()),
        "arming is history, not state"
    );
    assert_eq!(r.window.zeroed_at, Some(zeroed));
}

#[test]
fn a_clock_that_goes_backwards_reports_a_zero_window_not_a_negative_one() {
    let c = Collector::new(def(Level::Timing, &[]), read_at());
    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions::default(),
        now(),
    );
    assert_eq!(r.window.wall_ms, 0.0);
}

#[test]
fn a_capped_axis_is_flagged_on_the_result() {
    use crate::collector::intern::DEFAULT_NAME_CAP;
    let spans: Vec<SpanEntry> = (0..(DEFAULT_NAME_CAP + 5))
        .map(|i| span(&format!("n{i}"), i as u64 + 1, None, 0, MS))
        .collect();
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    assert!(
        r.cardinality_capped,
        "an __overflow__ row aggregates an arbitrary member set and must be visible"
    );
}

#[test]
fn group_by_parses_only_the_names_it_documents() {
    assert_eq!(GroupBy::parse("name"), Some(GroupBy::Name));
    assert_eq!(GroupBy::parse("path"), Some(GroupBy::Path));
    assert_eq!(GroupBy::parse(""), Some(GroupBy::None));
    assert_eq!(GroupBy::parse("service"), None);
    assert_eq!(GroupBy::parse("NAME"), None);
}

// ---------------------------------------------------------------------------
// What the instrument did to the data (§2.5)
// ---------------------------------------------------------------------------

/// Three spans, cut at 100 ms: one excluded, two kept. The exclusion count and
/// the surviving population must add back up to what was retained, or a reader
/// cannot reconstruct the denominator the numbers were computed over.
#[test]
fn warmup_reports_how_many_spans_it_excluded() {
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
        span("warm", 3, None, 300 * MS, 310 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    assert_eq!(r.excluded_by_warmup, Some(1));
    assert_eq!(r.sampled.as_ref().unwrap().sample_count, 2);
    assert_eq!(
        r.excluded_by_warmup.unwrap() + r.sampled.unwrap().sample_count,
        3,
        "excluded plus surviving must account for every retained span"
    );
}

/// The trap this field exists to close. `absent` means the filter never ran;
/// `0` means it ran and removed nothing. Asserted at the JSON layer too,
/// because a `#[serde(default)]` on the read side would turn one into the
/// other with nothing in the Rust type to show for it.
#[test]
fn no_warmup_leaves_the_exclusion_count_absent_not_zero() {
    let spans = [span("a", 1, None, 0, 10 * MS)];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    assert_eq!(r.excluded_by_warmup, None);

    let v = serde_json::to_value(&r).unwrap();
    assert!(
        v.get("excluded_by_warmup").is_none(),
        "the key must be absent from the wire, not present as 0: {v}"
    );
    // The mirror of `an_empty_window_says_the_zero_means_there_was_nothing_to_cut`:
    // that suppression is gated on a cut having been *asked for*, and without
    // this a read that requested nothing would carry an explanation for it.
    assert!(
        suppression(&r, "excluded_by_warmup").is_none(),
        "nothing was suppressed, because nothing was requested"
    );
}

#[test]
fn a_cut_that_could_not_be_positioned_is_absent_with_a_reason_not_a_zero() {
    // `warmup_cutoff_ns` measures from the earliest retained start, so with
    // nothing retained there is no cut to position. `Some(0)` would be a claim
    // about the run ("warm-up excluded nothing") when the truth is a claim
    // about retention ("we could not look") — and the documented contract for a
    // figure that could not be computed is null plus a `suppressed` entry,
    // because nothing directs a reader to check `suppressed` for a field that
    // already carries a value.
    let r = run(
        Level::Timing,
        &[],
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    assert_eq!(r.excluded_by_warmup, None);
    let s = suppression(&r, "excluded_by_warmup").expect("the absence must be explained");
    assert!(
        s.reason.contains("unknown, not zero"),
        "reason: {}",
        s.reason
    );
}

#[test]
fn a_warmup_that_excludes_everything_reports_the_whole_count() {
    let spans = [
        span("a", 1, None, 0, 10 * MS),
        span("b", 2, None, 5 * MS, 15 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(10_000.0),
            ..Default::default()
        },
    );
    assert_eq!(r.excluded_by_warmup, Some(2));
    assert_eq!(
        r.sampled.unwrap().sample_count,
        0,
        "an empty surviving population must not panic on the percentile path"
    );
}

/// Adversarial (H2): the count is taken once, off the record set every view
/// filters over. Group by trace and by path both re-walk those same records,
/// so a per-view count could drift — this pins all three to one number.
#[test]
fn every_view_reports_the_same_exclusion_count() {
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
        span("warm", 3, None, 300 * MS, 310 * MS),
    ];
    let opts = |g| ProfileOptions {
        skip_warmup_ms: Some(100.0),
        group_by: g,
        ..Default::default()
    };
    let ungrouped = run(Level::Tree, &spans, opts(GroupBy::None));
    let by_trace = run(Level::Tree, &spans, opts(GroupBy::Trace));
    let by_path = run(Level::Tree, &spans, opts(GroupBy::Path));

    assert_eq!(ungrouped.excluded_by_warmup, Some(1));
    assert_eq!(by_trace.excluded_by_warmup, ungrouped.excluded_by_warmup);
    assert_eq!(by_path.excluded_by_warmup, ungrouped.excluded_by_warmup);
}

#[test]
fn groups_total_counts_rows_before_top_n_truncation() {
    let spans = [
        span("a", 1, None, 0, 40 * MS),
        span("b", 2, None, 0, 30 * MS),
        span("c", 3, None, 0, 20 * MS),
        span("d", 4, None, 0, 10 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Name,
            top_n: 2,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 2, "top_n bounds what is returned");
    assert_eq!(
        r.groups_total,
        Some(4),
        "but the reader still has to be able to tell the top 2 of 4 from the top 2 of 2"
    );
}

/// `path_rows` is a SECOND implementation of "count before truncating" — it
/// uses `take`, not `rank_and_truncate` — and the name-axis test above cannot
/// reach it. A mutation moving its count to after the `take` survived the whole
/// suite because every path fixture either never truncated or never looked.
#[test]
fn groups_total_counts_paths_before_truncation_too() {
    let spans = [
        span("handler", 1, None, 0, 100 * MS),
        span("db", 2, Some(1), 10 * MS, 60 * MS),
    ];
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Path,
            top_n: 1,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 1, "top_n bounds what is returned");
    assert_eq!(
        r.groups_total,
        Some(2),
        "two distinct paths exist; truncating to one must not change the count"
    );
}

/// Two-way: the count must not be a constant that only looks right when
/// something was dropped.
#[test]
fn groups_total_equals_the_row_count_when_nothing_was_truncated() {
    let spans = [
        span("a", 1, None, 0, 40 * MS),
        span("b", 2, None, 0, 30 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Name,
            top_n: 20,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 2);
    assert_eq!(r.groups_total, Some(2));
}

/// A grouping that was REFUSED has no denominator. Reporting `0` would read as
/// "this run touched zero traces" when the truth is that trace identity is not
/// retained at this level — the same absent-is-not-zero rule the exclusion
/// count follows, in the same struct.
#[test]
fn a_refused_grouping_has_no_total_rather_than_a_zero_one() {
    let spans = [span("a", 1, None, 0, 10 * MS)];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            group_by: GroupBy::Trace,
            ..Default::default()
        },
    );
    assert!(suppression(&r, "groups").is_some(), "the refusal is stated");
    assert_eq!(r.groups_total, None);
    let v = serde_json::to_value(&r).unwrap();
    assert!(
        v.get("groups_total").is_none(),
        "and absent on the wire, not 0: {v}"
    );
}

/// Convergent finding from two independent gate lenses. Name and group rows are
/// built from the per-axis accumulators, which are written at ingest and have no
/// window — so under a warm-up cut they hand back at full weight exactly the
/// spans the read excluded, while the top-level `exact` tier is withheld for
/// that very reason. `excluded_by_warmup` beside them makes the contradiction
/// arithmetically visible: the cold span dominates the breakdown 25:1 while the
/// headline figures exclude it.
#[test]
fn a_warmup_cut_withholds_the_unwindowed_group_axes() {
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
        span("warm", 3, None, 300 * MS, 310 * MS),
    ];
    let r = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            group_by: GroupBy::Name,
            ..Default::default()
        },
    );
    assert!(
        r.groups.is_empty(),
        "the `cold` row would restore the excluded span at full weight: {:?}",
        r.groups
    );
    assert_eq!(r.groups_total, None);
    let s = suppression(&r, "groups").expect("withholding must be explained");
    assert!(s.reason.contains("unwindowed"), "reason: {}", s.reason);

    // Two-way: the sample-derived axes DO honour the cut, so they must survive.
    let by_path = run(
        Level::Tree,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            group_by: GroupBy::Path,
            ..Default::default()
        },
    );
    assert!(
        !by_path.groups.is_empty(),
        "path rows are projected from the retained records and honour the cut"
    );
}

/// The regression guard for that withholding. A `scalar` collector retains no
/// per-span records, so a warm-up cut can never be positioned and NOTHING is
/// ever excluded — the per-name rows are identical to a read without the option.
/// Keying the withholding on "a cut was requested" rather than "a cut ran"
/// blanked every figure in the response and claimed the rows carried excluded
/// spans, which was false.
#[test]
fn a_cut_that_never_ran_withholds_nothing() {
    let spans = [
        span("a", 1, None, 0, 40 * MS),
        span("b", 2, None, 0, 30 * MS),
    ];
    let r = run(
        Level::Scalar,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            group_by: GroupBy::Name,
            ..Default::default()
        },
    );
    assert_eq!(
        r.groups.len(),
        2,
        "the breakdown is correct and must be served"
    );
    assert_eq!(r.groups_total, Some(2));
    assert!(
        suppression(&r, "groups").is_none(),
        "nothing was excluded, so nothing may claim these rows carry excluded spans"
    );
}

/// `skip_warmup_ms: 0.0` asks for no cut at all — `warmup` filters out anything
/// at or below zero. A mutation keying the withholding off the raw parameter
/// instead survived the whole suite, so the edge gets its own test.
#[test]
fn a_zero_warmup_is_no_cut_and_withholds_nothing() {
    let spans = [span("a", 1, None, 0, 40 * MS)];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(0.0),
            group_by: GroupBy::Name,
            ..Default::default()
        },
    );
    assert_eq!(r.groups.len(), 1);
    assert!(suppression(&r, "groups").is_none());
    assert_eq!(r.excluded_by_warmup, None, "no cut was asked for");
}

/// A collector with no `group_keys` must hear about that, not receive a warm-up
/// explanation for an axis it never had. The withholding check runs after each
/// axis has had its own say for exactly this reason.
#[test]
fn a_missing_group_axis_is_diagnosed_ahead_of_the_warmup_rule() {
    let spans = [span("a", 1, None, 0, 40 * MS)];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(10.0),
            group_by: GroupBy::Group,
            ..Default::default()
        },
    );
    let s = suppression(&r, "groups").expect("the missing axis must be reported");
    assert!(
        s.reason.contains("no group_keys"),
        "the real problem is the absent axis, not the warm-up: {}",
        s.reason
    );
    assert_eq!(
        r.groups_total, None,
        "a refused grouping has no denominator"
    );
}

/// The remedy must be reachable from the level the reader is on. `trace` and
/// `path` honour a cut but need `tree`, so offering them at `timing` sends a
/// reader to a grouping their level refuses.
#[test]
fn the_withholding_remedy_only_offers_axes_the_level_supports() {
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
    ];
    let opts = ProfileOptions {
        skip_warmup_ms: Some(100.0),
        group_by: GroupBy::Name,
        ..Default::default()
    };

    let timing = run(Level::Timing, &spans, opts.clone());
    let r = suppression(&timing, "groups")
        .and_then(|s| s.remedy.clone())
        .expect("withheld with a remedy");
    assert!(
        !r.contains("`trace`, `path`") || r.contains("need level `tree`"),
        "timing cannot group by trace or path, so it must not be told to: {r}"
    );

    let tree = run(Level::Tree, &spans, opts);
    let r = suppression(&tree, "groups")
        .and_then(|s| s.remedy.clone())
        .expect("withheld with a remedy");
    assert!(
        r.contains("`trace`"),
        "at tree those axes DO work and are the better answer: {r}"
    );
}

/// The mirror of the level-aware remedy above, on the exclusion count: a
/// `timing` collector whose window is simply empty must not be told to "read at
/// level `timing` or `tree`" — advice to adopt the level it already has.
#[test]
fn an_empty_window_is_not_told_to_change_to_the_level_it_already_has() {
    let r = run(
        Level::Timing,
        &[],
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    let remedy = suppression(&r, "excluded_by_warmup")
        .and_then(|s| s.remedy.clone())
        .expect("the absence is explained");
    assert!(
        !remedy.contains("level `timing`"),
        "circular: it is already at timing: {remedy}"
    );

    let scalar = run(
        Level::Scalar,
        &[span("a", 1, None, 0, 10 * MS)],
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    let remedy = suppression(&scalar, "excluded_by_warmup")
        .and_then(|s| s.remedy.clone())
        .expect("the absence is explained");
    assert!(
        remedy.contains("`timing`"),
        "but scalar genuinely cannot retain records, so there the level IS the fix: {remedy}"
    );
}

// ---------------------------------------------------------------------------
// Small-n evidence: durations and spread (§2.1)
// ---------------------------------------------------------------------------

/// The load-bearing property. `sampled_view` keeps a sorted copy for the
/// percentiles right beside the arrival-order one, so returning the wrong
/// vector is a one-character mistake that every other assertion here would
/// still pass. A reader can sort these; they cannot unsort them.
#[test]
fn durations_come_back_in_arrival_order_not_sorted() {
    let spans = [
        span("op", 1, None, 0, 30 * MS),
        span("op", 2, None, 100 * MS, 110 * MS),
        span("op", 3, None, 200 * MS, 220 * MS),
    ];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    assert_eq!(
        r.sampled.unwrap().durations_ms,
        Some(vec![30.0, 10.0, 20.0]),
        "arrival order, not ascending"
    );
}

/// Pure arithmetic, checkable by hand: 10, 20, 30 ms has mean 20, squared
/// deviations 100 + 0 + 100, and n-1 = 2 — so exactly 10.0 ms. Pinned as an
/// exact figure rather than a range so that swapping in the population form
/// (which gives 8.165) fails rather than merely shifts.
#[test]
fn stddev_matches_a_hand_computed_reference() {
    let spans = [
        span("op", 1, None, 0, 10 * MS),
        span("op", 2, None, 100 * MS, 120 * MS),
        span("op", 3, None, 200 * MS, 230 * MS),
    ];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let s = r
        .sampled
        .unwrap()
        .stddev_ms
        .expect("three records have spread");
    assert!(
        (s - 10.0).abs() < 1e-9,
        "sample stddev of 10/20/30 ms is exactly 10.0, got {s}"
    );
}

#[test]
fn a_single_record_has_no_spread_rather_than_a_zero_one() {
    // 0.0 would say the measurement was perfectly repeatable. One observation
    // says nothing about repeatability at all.
    let spans = [span("op", 1, None, 0, 10 * MS)];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    let s = r.sampled.unwrap();
    assert_eq!(s.sample_count, 1);
    assert_eq!(s.stddev_ms, None);
    assert_eq!(
        s.durations_ms,
        Some(vec![10.0]),
        "the one duration is still worth listing — it is the spread that is undefined"
    );
}

#[test]
fn stddev_is_computed_over_the_post_warmup_population() {
    // Without the cut the 500 ms cold span dominates; with it, the spread is
    // that of 10 and 30 ms — sqrt(200) ms. A stddev computed before the filter
    // would report a number describing a population the percentiles beside it
    // do not share.
    let spans = [
        span("cold", 1, None, 0, 500 * MS),
        span("warm", 2, None, 200 * MS, 210 * MS),
        span("warm", 3, None, 300 * MS, 330 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    let s = r
        .sampled
        .unwrap()
        .stddev_ms
        .expect("two records have spread");
    assert!(
        (s - 10.0 * 2f64.sqrt()).abs() < 1e-9,
        "spread of the surviving 10/30 ms pair is sqrt(200) = 14.142, got {s}"
    );
}

#[test]
fn the_duration_list_stops_at_the_inline_cap() {
    let build = |n: u64| {
        let spans: Vec<_> = (0..n)
            .map(|i| span("op", i + 1, None, i as i64 * MS, i as i64 * MS + MS))
            .collect();
        run(Level::Timing, &spans, ProfileOptions::default())
    };

    let at_cap = build(MAX_INLINE_DURATIONS);
    let s = at_cap.sampled.as_ref().unwrap();
    assert_eq!(
        s.durations_ms.as_ref().map(|d| d.len()),
        Some(MAX_INLINE_DURATIONS as usize),
        "exactly at the cap is still listed"
    );
    assert!(suppression(&at_cap, "sampled.durations_ms").is_none());

    let over_cap = build(MAX_INLINE_DURATIONS + 1);
    let s = over_cap.sampled.as_ref().unwrap();
    assert_eq!(s.durations_ms, None, "one over the cap is withheld");
    assert!(
        suppression(&over_cap, "sampled.durations_ms").is_some(),
        "and withholding is explained, not silent"
    );
    assert!(
        s.stddev_ms.is_some(),
        "the spread still describes the whole retained set, so it survives the cap"
    );
}

#[test]
fn a_truncated_sample_withholds_its_durations_and_says_why() {
    use crate::collector::sample::CHUNK_RECORDS;
    // A budget for one chunk, then twice that many spans: the retained set is a
    // contiguous prefix of the run. Listing it would invite exactly the
    // per-value reasoning a biased subset cannot support.
    let mut d = def(Level::Timing, &[]);
    d.max_sample_bytes = CHUNK_RECORDS * Level::Timing.bytes_per_record();
    let c = Collector::new(d, now());
    for i in 0..(CHUNK_RECORDS * 2) {
        c.ingest(&span(
            "op",
            i as u64 + 1,
            None,
            i as i64 * MS,
            (i as i64 + 1) * MS,
        ));
    }
    let r = profile(
        &c.snapshot(),
        IngestBasis::Unavailable,
        &ProfileOptions::default(),
        read_at(),
    );

    let s = r.sampled.as_ref().unwrap();
    assert!(!s.complete, "the fixture must actually truncate");
    assert_eq!(s.durations_ms, None);
    let sup = suppression(&r, "sampled.durations_ms").expect("withholding must be explained");
    assert!(sup.reason.contains("prefix"), "reason: {}", sup.reason);
}

/// H7: a `sampled` block recorded before these fields existed — every snapshot
/// on disk today — must read as *not recorded*, never as an empty list or a
/// zero spread.
#[test]
fn a_sampled_block_from_an_older_build_reads_as_absent_not_empty() {
    let legacy = serde_json::json!({
        "complete": true,
        "sample_count": 3,
        "nested_matches": 0,
        "overlapping_child_ms": 0.0,
        "overlapping_child_spans": 0,
        "p50_ms": 10.0,
    });
    let s: ProfileSampled = serde_json::from_value(legacy).expect("older shape still loads");
    assert_eq!(s.sample_count, 3);
    assert_eq!(s.durations_ms, None, "absent, not Some(vec![])");
    assert_eq!(s.stddev_ms, None, "absent, not Some(0.0)");
}

/// The cap's tests reference it symbolically, so they hold for ANY value and a
/// refactor that shrank it tenfold would ship green. The literal also appears in
/// `durations_ms`'s published doc comment, which a schema consumer reads and
/// cannot resolve a Rust constant from — so the two must not drift apart.
#[test]
fn the_inline_cap_is_the_fifty_the_published_docs_promise() {
    assert_eq!(MAX_INLINE_DURATIONS, 50);
}

/// A span starting EXACTLY on the cutoff is kept — the filters use `<`, not
/// `<=`. Every other warm-up fixture places spans strictly inside or strictly
/// outside the cut, so flipping that one operator survived the whole suite while
/// silently breaking the "excluded plus surviving accounts for every retained
/// span" invariant by one.
#[test]
fn a_span_starting_exactly_on_the_cutoff_survives_the_cut() {
    let spans = [
        span("before", 1, None, 0, 10 * MS),
        // The cut lands at the earliest start + 100 ms — precisely this start.
        span("boundary", 2, None, 100 * MS, 110 * MS),
        span("after", 3, None, 200 * MS, 210 * MS),
    ];
    let r = run(
        Level::Timing,
        &spans,
        ProfileOptions {
            skip_warmup_ms: Some(100.0),
            ..Default::default()
        },
    );
    assert_eq!(
        r.excluded_by_warmup,
        Some(1),
        "only the span strictly before the cut is excluded"
    );
    let s = r.sampled.unwrap();
    assert_eq!(s.sample_count, 2, "the boundary span is retained");
    assert_eq!(
        s.durations_ms,
        Some(vec![10.0, 10.0]),
        "and it is the boundary and after spans that survived"
    );
}

#[test]
fn an_ungrouped_read_reports_no_group_total() {
    let spans = [span("a", 1, None, 0, 10 * MS)];
    let r = run(Level::Timing, &spans, ProfileOptions::default());
    assert!(r.grouped_by.is_none());
    assert!(r.groups.is_empty());
    assert_eq!(
        r.groups_total, None,
        "no grouping was asked for, so there is no denominator to report"
    );
}
