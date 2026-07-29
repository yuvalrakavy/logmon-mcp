//! §12: V9 diff bounds using α(a+b)/|a−b|, V19 both floors with one printed
//! threshold, A7 diff level/filter/layout/overflow handling, A8 incomplete side.

use super::*;
use crate::collector::history::{SnapshotPolicy, StoredSnapshot};
use crate::collector::project::Projected;
use crate::collector::sketch::DurationSketch;
use crate::collector::state::{Collector, DEFAULT_MAX_SAMPLE_BYTES};
use crate::filter::parser::parse_filter;
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::TimeZone;

const S: i64 = 1_700_000_000_000_000_000;
const MS: i64 = 1_000_000;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(S + secs * 1_000_000_000)
}

fn def_named(name: &str, filter: &str, level: Level, group_keys: Vec<String>) -> CollectorDef {
    CollectorDef {
        name: name.into(),
        filter_string: filter.into(),
        filter: parse_filter(filter).unwrap(),
        level,
        group_keys,
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: None,
    }
}

fn span(id: u64, parent: Option<u64>, start_ns: i64, dur_ns: i64, name: &str) -> SpanEntry {
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: id,
        parent_span_id: parent,
        start_time: Utc.timestamp_nanos(start_ns),
        end_time: Utc.timestamp_nanos(start_ns + dur_ns),
        duration_ms: 0.0,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

/// An arm over `n` spans of `each_ms`, from a real collector.
fn arm(spec: &str, n: usize, each_ms: i64) -> Arm {
    arm_with(spec, def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
        for i in 0..n {
            c.ingest(&span(i as u64 + 1, None, S, each_ms * MS, "op"));
        }
    })
}

/// Built through `Arm::from_live` rather than by hand, so these tests exercise
/// the constructor the RPC layer uses. A hand-built `Arm` would pass even if
/// that constructor resolved the wrong interner.
fn arm_with(spec: &str, def: CollectorDef, feed: impl FnOnce(&Collector)) -> Arm {
    let c = Collector::new(def, at(0));
    feed(&c);
    let mut arm = Arm::from_live(
        spec.into(),
        &c.snapshot(),
        Some(TraceIngestLoss::default()),
        at(1),
    );
    // The live shape is `Aggregation::Live`; most tests here care about the
    // single-run *property*, not which flavour of single run it is.
    arm.aggregation = Aggregation::SingleRun(spec.into());
    arm
}

fn row<'a>(rows: &'a [Row], metric: &str, tier: Tier) -> &'a Row {
    rows.iter()
        .find(|r| r.metric == metric && r.tier == tier)
        .unwrap_or_else(|| panic!("no {tier:?} row for {metric}"))
}

// ---------------------------------------------------------------------------
// The arithmetic (V9)
// ---------------------------------------------------------------------------

#[test]
fn exact_deltas_are_exact_and_carry_no_measurement_floor() {
    let d = diff(
        arm("a", 100, 10),
        arm("b", 100, 20),
        &DiffOptions::default(),
    )
    .unwrap();

    let count = row(&d.rows, "count", Tier::Exact);
    assert_eq!(count.a, Some(100.0));
    assert_eq!(count.b, Some(100.0));
    assert_eq!(count.delta, Some(0.0));

    let total = row(&d.rows, "total_ms", Tier::Exact);
    assert_eq!(total.a, Some(1000.0));
    assert_eq!(total.b, Some(2000.0));
    assert_eq!(total.delta, Some(1000.0));
    assert_eq!(total.delta_pct, Some(100.0));
    // Single-run arms: no run-to-run spread exists, so the floor is unknown —
    // and unknown must not read as zero, which would license calling any
    // difference significant (§6.5).
    assert!(total.threshold_abs.is_none(), "floor unknown, not zero");
    assert_eq!(total.threshold_basis, ThresholdBasis::Unknown);
    assert!(!total.suppressed, "nothing to suppress against");
    assert!(
        total.notes.iter().any(|n| n.contains("no floor")),
        "and it says so: {:?}",
        total.notes
    );
}

#[test]
fn an_estimated_delta_carries_alpha_a_plus_b_over_a_minus_b_and_not_root_two_alpha() {
    // V9. The bound is deterministic worst-case on a quantised value, so the
    // two arms' errors do NOT combine in quadrature: √2·α is wrong in kind.
    let d = diff(
        arm("a", 200, 100),
        arm("b", 200, 105),
        &DiffOptions::default(),
    )
    .unwrap();
    let p50 = row(&d.rows, "p50_ms", Tier::Estimated);
    let (a, b) = (p50.a.unwrap(), p50.b.unwrap());

    let expected = ALPHA * (a + b) / (a - b).abs() * 100.0;
    let got = p50.error_bound_pct.expect("estimated rows carry the bound");
    assert!(
        (got - expected).abs() < 1e-9,
        "bound is α(a+b)/|a−b|: got {got}, expected {expected}"
    );

    // The magnitude is the point of the correction. √2·α at α = 1 % would be
    // ~1.41 %; the correct bound here is more than an order of magnitude wider,
    // which is why the earlier revision's figure was wrong in kind and not just
    // in rounding.
    let root_two_alpha_pct = 2.0f64.sqrt() * ALPHA * 100.0;
    assert!(
        got > 10.0 * root_two_alpha_pct,
        "the bound must dwarf √2·α ({root_two_alpha_pct:.2}%), not approximate it: {got}"
    );
}

#[test]
fn the_bound_reproduces_the_two_figures_the_spec_quotes() {
    // §5.6 quotes ±39 % for a 5 % change and ±199 % for a 1 % change, at
    // α = 1 %. Checked as pure arithmetic rather than through a sketch, because
    // quantisation moves the inputs and the point here is the formula itself.
    //
    // Both figures are for a *reduction* from 100 — the direction an
    // optimization moves a latency, and the one the design is aimed at.
    let bound = |a: f64, b: f64| ALPHA * (a + b) / (a - b).abs() * 100.0;

    assert!(
        (bound(100.0, 95.0) - 39.0).abs() < 0.5,
        "a 5% reduction carries ±39%: {}",
        bound(100.0, 95.0)
    );
    assert!(
        (bound(100.0, 99.0) - 199.0).abs() < 0.5,
        "a 1% reduction carries ±199%: {}",
        bound(100.0, 99.0)
    );

    // And the resolution floor is where the bound crosses 100 %: below a ~2 %
    // change the error bar is wider than the delta it qualifies.
    assert!(bound(100.0, 98.0) < 100.0, "2% is (just) resolvable");
    assert!(bound(100.0, 99.5) > 100.0, "0.5% is not");
}

#[test]
fn an_estimated_delta_below_the_resolution_floor_is_suppressed_by_the_printed_threshold() {
    // V19's "one printed threshold". The suppression rule and the printed
    // number must be the same thing — an earlier revision struck deltas
    // through using a bound other than the one it displayed.
    let d = diff(
        arm("a", 200, 100),
        arm("b", 200, 100),
        &DiffOptions::default(),
    )
    .unwrap();
    let p50 = row(&d.rows, "p50_ms", Tier::Estimated);
    assert!(p50.suppressed, "a zero delta is below any floor");

    let t = p50
        .threshold_pct
        .expect("a measurement floor always exists");
    assert!(
        (t - 2.0 * ALPHA * 100.0).abs() < 1e-9,
        "threshold is 2α as a percentage of the mean: {t}"
    );
    // Derived from ALPHA, not written down: at α = 1 % this is §5.6's ~2 %.
    assert!((t - 2.0).abs() < 1e-9, "2% at α=1%: {t}");

    // And the printed threshold is the one applied, in the metric's units.
    let abs = p50.threshold_abs.unwrap();
    let mean = (p50.a.unwrap() + p50.b.unwrap()) / 2.0;
    assert!((abs - t / 100.0 * mean).abs() < 1e-9);
    assert!(
        p50.delta.unwrap().abs() <= abs,
        "suppressed because |Δ| ≤ threshold"
    );
}

#[test]
fn the_error_bound_reaching_one_hundred_percent_is_the_same_condition_as_suppression() {
    // The two presentations of the one rule must not be able to disagree: a
    // reader who checks `error_bound_pct >= 100` and a reader who checks
    // `|delta| <= threshold_abs` have to reach the same verdict.
    for each_b in [100, 101, 102, 103, 105, 110, 150] {
        let d = diff(
            arm("a", 200, 100),
            arm("b", 200, each_b),
            &DiffOptions::default(),
        )
        .unwrap();
        let r = row(&d.rows, "p50_ms", Tier::Estimated);
        let by_bound = r.error_bound_pct.map(|e| e >= 100.0).unwrap_or(true);
        assert_eq!(
            by_bound, r.suppressed,
            "at b={each_b}ms the two presentations disagree: bound={:?} suppressed={}",
            r.error_bound_pct, r.suppressed
        );
    }
}

#[test]
fn a_known_run_to_run_floor_binds_when_it_is_wider_than_the_measurement_floor() {
    // V19's second floor. The two are distinct and a result must not conflate
    // them; when both apply, the wider one is what suppresses.
    let mut a = arm("a", 200, 100);
    let mut b = arm("b", 200, 103);
    a.aggregation = Aggregation::Merged(vec!["r1".into(), "r2".into()]);
    b.aggregation = Aggregation::Merged(vec!["r1".into(), "r2".into()]);
    a.floor = Some(noisy_floor(12.0));
    b.floor = Some(noisy_floor(5.0));

    let d = diff(a, b, &DiffOptions::default()).unwrap();
    let p50 = row(&d.rows, "p50_ms", Tier::Estimated);
    assert_eq!(
        p50.threshold_pct,
        Some(12.0),
        "the noisier arm sets the floor"
    );
    assert_eq!(p50.threshold_basis, ThresholdBasis::Binding);
    assert!(p50.suppressed, "a 3% change is inside a 12% floor");

    // And an exact row uses the same floor — it has no measurement error of
    // its own, but scheduling variance applies to it just the same.
    let total = row(&d.rows, "total_ms", Tier::Exact);
    assert_eq!(total.threshold_pct, Some(12.0));
    assert_eq!(total.threshold_basis, ThresholdBasis::RunToRun);
}

fn noisy_floor(cv_pct: f64) -> RunToRunFloor {
    RunToRunFloor {
        metric: "total_ms",
        runs: 3,
        min: 0.0,
        max: 0.0,
        mean: 1.0,
        cv_pct: Some(cv_pct),
    }
}

#[test]
fn an_avg_ms_delta_is_suppressed_when_the_count_moved_materially() {
    // §9.5: an exact-tier number that is invalid as an effect size, because the
    // denominator moved. Reported as a pair of values with no delta rather than
    // omitted, so a reader can still see both.
    let d = diff(
        arm("a", 100, 10),
        arm("b", 200, 10),
        &DiffOptions::default(),
    )
    .unwrap();
    let avg = row(&d.rows, "avg_ms", Tier::Exact);
    assert!(avg.delta.is_none(), "no delta");
    assert_eq!(avg.a, Some(10.0), "but both values are still shown");
    assert_eq!(avg.b, Some(10.0));
    assert!(
        avg.notes.iter().any(|n| n.contains("blends")),
        "and it says why: {:?}",
        avg.notes
    );

    // Immaterial movement still gets a delta: suppressing on one span in a
    // thousand would train the reader to ignore the suppression.
    let d = diff(
        arm("a", 1000, 10),
        arm("b", 1005, 10),
        &DiffOptions::default(),
    )
    .unwrap();
    assert!(row(&d.rows, "avg_ms", Tier::Exact).delta.is_some());
}

// ---------------------------------------------------------------------------
// Refusals and marks (A7)
// ---------------------------------------------------------------------------

#[test]
fn a_mismatched_sketch_layout_is_refused_with_no_override() {
    let mut a = arm("a", 10, 10);
    let b = arm("b", 10, 10);
    // Rebuild the exact tier around a sketch recorded under a different layout,
    // which is what a snapshot written by a build with different parameters
    // would restore to. `SketchLayout` is the recorded fact the refusal rests on.
    let odd = SketchLayout {
        alpha: 0.05,
        ..SketchLayout::CURRENT
    };
    let bytes = a.total.sketch().to_bytes();
    let restored = DurationSketch::from_bytes(&bytes, odd).expect("decode");
    a.total = ExactStats::from_parts(
        a.total.count,
        a.total.total_ns,
        a.total.min_ns,
        a.total.max_ns,
        a.total.error_count,
        a.total.negative_duration_spans,
        a.total.malformed_timestamps,
        a.total.out_of_range_spans,
        restored,
    );

    let err = diff(
        a,
        b,
        &DiffOptions {
            allow: DiffAllowances {
                mismatch: true,
                lossy: true,
                truncated: true,
            },
            ..Default::default()
        },
    )
    .expect_err("refused");
    assert!(matches!(err, DiffError::LayoutMismatch { .. }));
    // No flag permits it, and the message must not imply one exists.
    let msg = err.to_string();
    assert!(msg.contains("no flag"), "{msg}");
}

#[test]
fn a_genuine_filter_difference_blocks_and_names_the_flag() {
    let a = arm_with("a", def_named("c", "sn=get", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "get"));
    });
    let b = arm_with("b", def_named("c", "sn=put", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "put"));
    });

    let err = diff(a, b, &DiffOptions::default()).expect_err("blocked");
    assert!(matches!(err, DiffError::FilterMismatch { .. }));
    assert!(err.to_string().contains("allow_mismatch"), "names the flag");

    // Permitted, and then MARKED — the override does not make the difference
    // stop existing.
    let a = arm_with("a", def_named("c", "sn=get", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "get"));
    });
    let b = arm_with("b", def_named("c", "sn=put", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "put"));
    });
    let d = diff(
        a,
        b,
        &DiffOptions {
            allow: DiffAllowances {
                mismatch: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    let m = d.marks.iter().find(|m| m.kind == "filter").expect("marked");
    assert_eq!(m.overridden_by.as_deref(), Some("allow_mismatch"));
    assert!(!d.trustworthy);
}

#[test]
fn a_spelling_only_filter_difference_marks_rather_than_blocks() {
    // Qualifiers are AND-ed, so their order carries no meaning — two spellings
    // of one population must compare equal, and the difference is still worth
    // saying out loud because a reader comparing the strings by eye will see it.
    let a = arm_with(
        "a",
        def_named("c", "sv=svc, d>=5", Level::Tree, vec![]),
        |c| {
            c.ingest(&span(1, None, S, 10 * MS, "op"));
        },
    );
    let b = arm_with(
        "b",
        def_named("c", "d>=5, sv=svc", Level::Tree, vec![]),
        |c| {
            c.ingest(&span(1, None, S, 10 * MS, "op"));
        },
    );
    let d = diff(a, b, &DiffOptions::default()).expect("not blocked");
    let m = d.marks.iter().find(|m| m.kind == "filter").expect("marked");
    assert!(m.overridden_by.is_none(), "not an override, just a note");
    assert!(m.detail.contains("spelling"), "{}", m.detail);
}

#[test]
fn asymmetric_ingest_loss_blocks_and_symmetric_loss_only_marks() {
    let lossy = TraceIngestLoss {
        dropped: 12,
        shed_batches: 0,
        malformed: 0,
    };

    let mut a = arm("a", 100, 10);
    let b = arm("b", 100, 10);
    a.ingest = Some(lossy);
    let err = diff(a, b, &DiffOptions::default()).expect_err("blocked");
    assert!(matches!(err, DiffError::AsymmetricLoss { .. }));
    assert!(err.to_string().contains("allow_lossy"));

    // Both sides lossy: the delta is between two under-counts, which is worth
    // saying — but it is not the asymmetry the refusal is aimed at.
    let mut a = arm("a", 100, 10);
    let mut b = arm("b", 100, 10);
    a.ingest = Some(lossy);
    b.ingest = Some(lossy);
    let d = diff(a, b, &DiffOptions::default()).expect("not blocked");
    let m = d
        .marks
        .iter()
        .find(|m| m.kind == "ingest_loss")
        .expect("marked");
    assert!(m.detail.contains("both arms"), "{}", m.detail);
    assert!(!d.trustworthy);
}

#[test]
fn unknown_ingest_loss_marks_rather_than_blocks() {
    // `None` is unknown, not zero. Refusing on unknown would make the
    // restart-then-re-pin workflow §7.1 exists to support undiffable, since a
    // restored collector's baseline cannot be reconciled with fresh counters.
    let mut a = arm("a", 100, 10);
    let b = arm("b", 100, 10);
    a.ingest = None;
    let d = diff(a, b, &DiffOptions::default()).expect("not blocked");
    let m = d
        .marks
        .iter()
        .find(|m| m.kind == "ingest_loss")
        .expect("marked");
    assert!(m.detail.contains("unknown"), "{}", m.detail);
    assert!(!d.trustworthy, "but it is not trustworthy either");
}

#[test]
fn both_arms_truncated_is_refused_and_one_truncated_arm_only_scopes() {
    // A8. One truncated side scopes to sample-derived rows; both truncated is
    // refused, because a faster arm reaches the cap later in wall-clock terms
    // and the two cold prefixes cover different slices of work.
    let mut a = arm("a", 100, 10);
    let mut b = arm("b", 100, 10);
    a.sample_complete = false;
    b.sample_complete = false;
    let err = diff(a, b, &DiffOptions::default()).expect_err("refused");
    assert!(matches!(err, DiffError::BothTruncated));
    let msg = err.to_string();
    assert!(msg.contains("allow_truncated"), "names the flag: {msg}");
    assert!(
        msg.contains("total_ms") && msg.contains("unaffected"),
        "and says which rows survive: {msg}"
    );

    let mut a = arm("a", 100, 10);
    let b = arm("b", 100, 20);
    a.sample_complete = false;
    let d = diff(a, b, &DiffOptions::default()).expect("one side scopes, not refuses");
    assert!(d.marks.iter().any(|m| m.kind == "truncated"));
    // Exact and estimated pass; sample-derived rows carry the doubt.
    assert!(row(&d.rows, "total_ms", Tier::Exact).trustworthy);
    assert!(row(&d.rows, "p95_ms", Tier::Estimated).trustworthy);
    assert!(!row(&d.rows, "p95_ms", Tier::Sampled).trustworthy);
}

#[test]
fn a_scalar_arm_is_permitted_because_an_absent_sample_tier_is_not_a_truncated_one() {
    // §5.6 says scalar collectors are explicitly permitted. That falls out of
    // `complete` staying true when there is no tier to truncate — this test
    // exists so a future change to that semantic cannot silently make every
    // scalar diff refuse.
    let a = arm_with("a", def_named("c", "sv=svc", Level::Scalar, vec![]), |c| {
        for i in 0..50 {
            c.ingest(&span(i + 1, None, S, 10 * MS, "op"));
        }
    });
    let b = arm_with("b", def_named("c", "sv=svc", Level::Scalar, vec![]), |c| {
        for i in 0..50 {
            c.ingest(&span(i + 1, None, S, 20 * MS, "op"));
        }
    });
    assert!(!a.truncated() && !b.truncated());

    let d = diff(a, b, &DiffOptions::default()).expect("permitted");
    assert_eq!(row(&d.rows, "total_ms", Tier::Exact).delta, Some(500.0));
    // And the sample-derived rows are absent with a reason rather than zero.
    assert!(d
        .suppressed
        .iter()
        .any(|s| s.field == "sampled" && s.reason.contains("scalar")));
}

#[test]
fn different_levels_compare_at_the_minimum_but_nesting_evidence_survives() {
    // A7 plus §5.6's explicit correction: dropping `nested_matches` along with
    // the level reintroduces the round-1 headline defect through another door.
    // The `tree` arm has nesting; comparing at `timing` must not hide that its
    // `total_ms` double-counts.
    let a = arm_with("a", def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 100 * MS, "outer"));
        c.ingest(&span(2, Some(1), S + 10 * MS, 50 * MS, "inner"));
    });
    let b = arm_with("b", def_named("c", "sv=svc", Level::Timing, vec![]), |c| {
        c.ingest(&span(1, None, S, 100 * MS, "outer"));
        c.ingest(&span(2, Some(1), S + 10 * MS, 50 * MS, "inner"));
    });

    let d = diff(a, b, &DiffOptions::default()).unwrap();
    assert_eq!(d.level, Level::Timing, "min(level)");
    assert!(d.marks.iter().any(|m| m.kind == "level"));

    let nesting = d
        .marks
        .iter()
        .find(|m| m.kind == "nesting")
        .expect("nesting evidence is not discarded with the level");
    assert!(nesting.detail.contains("twice"), "{}", nesting.detail);

    let total = row(&d.rows, "total_ms", Tier::Exact);
    assert!(!total.trustworthy, "the fact attaches to the total_ms row");
    assert!(!d.trustworthy);

    // Self time cannot be compared at `timing` — and the reason names the level
    // rather than leaving the reader to infer it.
    let self_row = row(&d.rows, "self_ms", Tier::Sampled);
    assert!(self_row.delta.is_none());
    assert!(
        self_row.notes.iter().any(|n| n.contains("timing")),
        "{:?}",
        self_row.notes
    );
}

#[test]
fn below_tree_on_both_arms_marks_that_double_counting_is_unknowable() {
    // The other half of the nesting rule: "we looked and found none" and "we
    // cannot look" are different claims, and only the second one is a reason to
    // re-run.
    let a = arm_with("a", def_named("c", "sv=svc", Level::Timing, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "op"));
    });
    let b = arm_with("b", def_named("c", "sv=svc", Level::Timing, vec![]), |c| {
        c.ingest(&span(1, None, S, 20 * MS, "op"));
    });
    let d = diff(a, b, &DiffOptions::default()).unwrap();
    let m = d
        .marks
        .iter()
        .find(|m| m.kind == "nesting")
        .expect("marked");
    assert!(m.detail.contains("not knowable"), "{}", m.detail);
    assert!(!row(&d.rows, "total_ms", Tier::Exact).trustworthy);
}

#[test]
fn nesting_detected_on_neither_arm_at_tree_level_does_not_mark() {
    // Two-way evidence for the rule above: a tree-level arm that genuinely
    // found no nesting must not be marked, or the mark means nothing.
    let d = diff(arm("a", 10, 10), arm("b", 10, 20), &DiffOptions::default()).unwrap();
    assert!(
        !d.marks.iter().any(|m| m.kind == "nesting"),
        "marks: {:?}",
        d.marks
    );
    assert!(row(&d.rows, "total_ms", Tier::Exact).trustworthy);
}

// ---------------------------------------------------------------------------
// Breakdowns
// ---------------------------------------------------------------------------

fn grouped(by: DiffGroupBy) -> DiffOptions {
    DiffOptions {
        group_by: by,
        ..Default::default()
    }
}

#[test]
fn per_name_rows_pair_up_and_one_sided_keys_get_no_invented_zero() {
    let a = arm_with("a", def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 100 * MS, "shared"));
        c.ingest(&span(2, None, S, 50 * MS, "only_a"));
    });
    let b = arm_with("b", def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 40 * MS, "shared"));
        c.ingest(&span(3, None, S, 10 * MS, "only_b"));
    });

    let d = diff(a, b, &grouped(DiffGroupBy::Name)).unwrap();
    let g = |k: &str| d.groups.iter().find(|g| g.key == k).expect(k);

    let shared = g("shared");
    assert_eq!(shared.presence, "both");
    assert_eq!(
        row(&shared.rows, "total_ms", Tier::Exact).delta,
        Some(-60.0)
    );

    // A key on one side only is NOT a delta against zero: it may be missing
    // because nothing matched it, or because a cardinality cap folded it.
    let only_a = g("only_a");
    assert_eq!(only_a.presence, "a_only");
    let r = row(&only_a.rows, "total_ms", Tier::Exact);
    assert_eq!(r.a, Some(50.0));
    assert!(r.b.is_none() && r.delta.is_none(), "no invented zero");
    assert_eq!(g("only_b").presence, "b_only");

    // Ranked by how much the row moved — a breakdown answers "where did the
    // time go", and a row that did not move is not an answer to it.
    assert_eq!(d.groups[0].key, "shared");
}

#[test]
fn overflow_group_rows_are_suppressed_and_counted_never_compared() {
    // §3.3: an overflow row aggregates an arbitrary, arrival-ordered member
    // set, so two arms' overflow rows are different populations sharing a label.
    let cap = crate::collector::intern::DEFAULT_NAME_CAP;
    let mk = |spec: &str, dur: i64| {
        arm_with(spec, def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
            for i in 0..(cap as u64 + 5) {
                c.ingest(&span(i + 1, None, S, dur * MS, &format!("op{i}")));
            }
        })
    };
    let d = diff(mk("a", 10), mk("b", 20), &grouped(DiffGroupBy::Name)).unwrap();

    assert!(
        !d.groups.iter().any(|g| g.key.contains("__overflow__")),
        "never compared"
    );
    assert!(d.overflow_rows_suppressed > 0, "but counted");
    assert!(
        d.marks.iter().any(|m| m.kind == "cardinality"),
        "and marked: {:?}",
        d.marks
    );
}

#[test]
fn per_group_rows_are_suppressed_when_the_arms_declare_different_group_keys() {
    let a = arm_with(
        "a",
        def_named("c", "sv=svc", Level::Tree, vec!["cache".into()]),
        |c| {
            c.ingest(&span(1, None, S, 10 * MS, "op"));
        },
    );
    let b = arm_with(
        "b",
        def_named("c", "sv=svc", Level::Tree, vec!["region".into()]),
        |c| {
            c.ingest(&span(1, None, S, 10 * MS, "op"));
        },
    );
    let d = diff(a, b, &grouped(DiffGroupBy::Group)).unwrap();
    assert!(
        d.groups.is_empty(),
        "a tuple means a different thing on each"
    );
    assert!(d
        .suppressed
        .iter()
        .any(|s| s.field == "groups" && s.reason.contains("group_keys")));
    assert!(d.marks.iter().any(|m| m.kind == "group_keys"));
    // Per-NAME rows are unaffected by a group_keys difference.
    let a = arm_with(
        "a",
        def_named("c", "sv=svc", Level::Tree, vec!["cache".into()]),
        |c| {
            c.ingest(&span(1, None, S, 10 * MS, "op"));
        },
    );
    let b = arm_with(
        "b",
        def_named("c", "sv=svc", Level::Tree, vec!["region".into()]),
        |c| {
            c.ingest(&span(1, None, S, 20 * MS, "op"));
        },
    );
    let d = diff(a, b, &grouped(DiffGroupBy::Name)).unwrap();
    assert_eq!(d.groups.len(), 1);
}

#[test]
fn a_per_group_avg_delta_is_suppressed_on_its_own_count_not_the_overall_one() {
    // The overall counts can be flat while one group's population moves
    // entirely — which is exactly the case where that row's avg would mislead.
    let mk = |spec: &str, hot: usize, cold: usize| {
        arm_with(spec, def_named("c", "sv=svc", Level::Tree, vec![]), |c| {
            let mut id = 0u64;
            for _ in 0..hot {
                id += 1;
                c.ingest(&span(id, None, S, 10 * MS, "hot"));
            }
            for _ in 0..cold {
                id += 1;
                c.ingest(&span(id, None, S, 10 * MS, "cold"));
            }
        })
    };
    // 100 total on both arms, but the split moved wholesale.
    let d = diff(
        mk("a", 20, 80),
        mk("b", 80, 20),
        &grouped(DiffGroupBy::Name),
    )
    .unwrap();
    assert!(
        row(&d.rows, "count", Tier::Exact).delta == Some(0.0),
        "overall count is flat"
    );
    let hot = d.groups.iter().find(|g| g.key == "hot").unwrap();
    let avg = row(&hot.rows, "avg_ms", Tier::Exact);
    assert!(avg.delta.is_none(), "suppressed on this row's own count");
}

// ---------------------------------------------------------------------------
// Aggregation and trust
// ---------------------------------------------------------------------------

#[test]
fn a_single_run_arm_makes_the_result_untrustworthy_on_its_own() {
    // §9.4: `trustworthy: false` whenever any arm is single-run. Nothing else
    // has to be wrong for a two-single-run comparison to be unable to tell a
    // real change from noise.
    let d = diff(
        arm("a", 100, 10),
        arm("b", 100, 20),
        &DiffOptions::default(),
    )
    .unwrap();
    assert!(d.marks.is_empty(), "nothing differs: {:?}", d.marks);
    assert!(!d.trustworthy, "and it is still not trustworthy");

    let mut a = arm("a", 100, 10);
    let mut b = arm("b", 100, 20);
    a.aggregation = Aggregation::Merged(vec!["r1".into(), "r2".into(), "r3".into()]);
    b.aggregation = Aggregation::Merged(vec!["r1".into(), "r2".into(), "r3".into()]);
    a.floor = Some(noisy_floor(1.0));
    b.floor = Some(noisy_floor(1.0));
    let d = diff(a, b, &DiffOptions::default()).unwrap();
    assert!(d.trustworthy, "three runs a side, nothing marked");
}

#[test]
fn aggregation_says_which_runs_a_figure_came_from() {
    assert!(Aggregation::Live.is_single_run());
    assert!(Aggregation::SingleRun("x".into()).is_single_run());
    assert!(
        Aggregation::Merged(vec!["a".into()]).is_single_run(),
        "one is one"
    );
    assert!(!Aggregation::Merged(vec!["a".into(), "b".into()]).is_single_run());

    let s = Aggregation::Merged(vec!["a".into(), "b".into()]).as_str();
    assert!(s.contains("2 runs") && s.contains("a, b"), "{s}");
}

#[test]
fn comparing_two_different_collectors_is_permitted_and_labelled() {
    let a = arm_with("a", def_named("left", "sv=svc", Level::Tree, vec![]), |c| {
        c.ingest(&span(1, None, S, 10 * MS, "op"));
    });
    let b = arm_with(
        "b",
        def_named("right", "sv=svc", Level::Tree, vec![]),
        |c| {
            c.ingest(&span(1, None, S, 20 * MS, "op"));
        },
    );
    let d = diff(a, b, &DiffOptions::default()).expect("permitted");
    let m = d
        .marks
        .iter()
        .find(|m| m.kind == "collector")
        .expect("labelled");
    assert!(
        m.detail.contains("two different collectors"),
        "{}",
        m.detail
    );
}

// ---------------------------------------------------------------------------
// Filter canonicalization
// ---------------------------------------------------------------------------

#[test]
fn canonicalization_is_order_insensitive_and_case_insensitive_on_substrings() {
    let c = |s: &str| canonical_filter(&parse_filter(s).unwrap());
    assert_eq!(c("sv=svc, d>=5"), c("d>=5, sv=svc"), "AND-ed, so unordered");
    assert_eq!(c("sn=GetUser"), c("sn=getuser"), "substrings fold case");
    assert_ne!(c("sn=get"), c("sn=put"));
    assert_eq!(c("ALL"), vec!["ALL".to_string()]);
}

#[test]
fn duration_thresholds_canonicalize_by_bit_pattern_which_is_total_cmp_equality() {
    let c = |s: &str| canonical_filter(&parse_filter(s).unwrap());
    assert_eq!(c("d>=100"), c("d>=100.0"), "same value, same bits");
    assert_ne!(c("d>=100"), c("d>=100.0001"));
    assert_ne!(c("d>=100"), c("d<=100"), "op is part of the identity");

    // The claim the comment rests on: bit equality is exactly total_cmp
    // equality, so canonicalizing on bits IS "thresholds by total_cmp".
    for (x, y) in [
        (1.0f64, 1.0f64),
        (0.0, -0.0),
        (f64::NAN, f64::NAN),
        (1.0, 2.0),
    ] {
        assert_eq!(
            x.to_bits() == y.to_bits(),
            x.total_cmp(&y) == std::cmp::Ordering::Equal,
            "{x} vs {y}"
        );
    }
}

#[test]
fn a_regex_spelled_differently_marks_rather_than_being_normalised() {
    // Deliberate: proving two regexes equivalent is undecidable in general, so
    // a mark that says "these differ in spelling" is honest where a normaliser
    // that got it wrong would not be.
    let c = |s: &str| canonical_filter(&parse_filter(s).unwrap());
    assert_ne!(c("sn=/get.*/"), c("sn=/get.+/"));
}

// ---------------------------------------------------------------------------
// The stored-snapshot path
// ---------------------------------------------------------------------------

/// A recorded run of `n` spans of `each_ms`, with `ingest` loss attributed.
fn stored(label: &str, n: usize, each_ms: i64, ingest: Option<TraceIngestLoss>) -> StoredSnapshot {
    let c = Collector::new(def_named("c", "sv=svc", Level::Tree, vec![]), at(0));
    for i in 0..n {
        c.ingest(&span(i as u64 + 1, None, S, each_ms * MS, "op"));
    }
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    StoredSnapshot::from_view(
        label.into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        ingest,
        projected,
    )
}

#[test]
fn a_merged_arm_is_the_only_shape_that_yields_a_run_to_run_floor() {
    // The reason `@*` exists. With single-run arms on both sides every threshold
    // in the result reads "unknown", so nothing can be called a result.
    let runs = vec![
        stored("r1", 100, 10, Some(TraceIngestLoss::default())),
        stored("r2", 100, 11, Some(TraceIngestLoss::default())),
        stored("r3", 100, 12, Some(TraceIngestLoss::default())),
    ];
    let merged = Arm::merged("c@*".into(), &runs).expect("merged");

    assert_eq!(merged.total.count, 300, "exact scalars are additive");
    let floor = merged.floor.as_ref().expect("three runs have a spread");
    assert_eq!(floor.runs, 3);
    assert!(floor.cv_pct.is_some(), "and a CV");
    assert!(!merged.aggregation.is_single_run());
    assert!(merged.aggregation.as_str().contains("r1, r2, r3"));
}

#[test]
fn a_merged_arm_omits_sample_derived_figures_rather_than_summing_them() {
    // Self time across two runs is not a self time, and a wall union across
    // them is not a union (§6.5).
    let runs = vec![
        stored("r1", 50, 10, Some(TraceIngestLoss::default())),
        stored("r2", 50, 10, Some(TraceIngestLoss::default())),
    ];
    assert!(runs[0].projections.is_some(), "the runs have projections");
    let merged = Arm::merged("c@*".into(), &runs).unwrap();
    assert!(merged.projections.is_none(), "omitted, not summed");
    assert!(merged.paths.is_empty(), "and neither do path lists merge");

    // Which the diff then reports as a suppression rather than a silent gap.
    let d = diff(merged, arm("b", 50, 10), &DiffOptions::default()).unwrap();
    assert!(d
        .suppressed
        .iter()
        .any(|s| s.field == "sampled" && s.reason.contains("projections")));
}

#[test]
fn a_merged_arms_per_axis_rows_are_all_or_nothing() {
    // Merging only the runs that recorded a breakdown would give per-name rows
    // that do not sum to the merged total — breaking A11's reconciliation rule
    // in a way no reader could detect.
    let mut runs = vec![
        stored("r1", 50, 10, Some(TraceIngestLoss::default())),
        stored("r2", 50, 10, Some(TraceIngestLoss::default())),
    ];
    let merged = Arm::merged("c@*".into(), &runs).unwrap();
    let per_name = merged.per_name.as_ref().expect("both runs recorded it");
    assert_eq!(
        per_name.get("op").map(|e| e.count),
        Some(100),
        "and the rows sum to the merged total"
    );

    // One run without it — as a restored-from-disk snapshot is — and the whole
    // breakdown is absent rather than partial.
    runs[1].per_name = None;
    let merged = Arm::merged("c@*".into(), &runs).unwrap();
    assert!(merged.per_name.is_none(), "all or nothing");
}

#[test]
fn merged_ingest_loss_is_summed_and_unknown_on_any_run_makes_it_unknown() {
    let loss = TraceIngestLoss {
        dropped: 5,
        shed_batches: 1,
        malformed: 2,
    };
    let runs = vec![
        stored("r1", 10, 10, Some(loss)),
        stored("r2", 10, 10, Some(loss)),
    ];
    let merged = Arm::merged("c@*".into(), &runs).unwrap();
    let i = merged.ingest.expect("known on both");
    assert_eq!(
        (i.dropped, i.shed_batches, i.malformed),
        (10, 2, 4),
        "summed"
    );

    // One window whose attribution failed makes the total a floor rather than a
    // figure, and a floor presented as a figure is worse than no number.
    let runs = vec![stored("r1", 10, 10, Some(loss)), stored("r2", 10, 10, None)];
    let merged = Arm::merged("c@*".into(), &runs).unwrap();
    assert!(merged.ingest.is_none(), "unknown, not 5");
}

#[test]
fn truncation_anywhere_truncates_the_merged_arm() {
    let mut runs = vec![
        stored("r1", 10, 10, Some(TraceIngestLoss::default())),
        stored("r2", 10, 10, Some(TraceIngestLoss::default())),
    ];
    assert!(!Arm::merged("x".into(), &runs).unwrap().truncated());
    runs[0].sample_complete = false;
    assert!(
        Arm::merged("x".into(), &runs).unwrap().truncated(),
        "a set of prefixes is not a sample of the whole"
    );
}

#[test]
fn merging_no_runs_is_an_error_rather_than_an_empty_arm() {
    assert!(Arm::merged("c@*".into(), &[]).is_err());
}

#[test]
fn the_nesting_verdict_survives_a_merge_even_though_the_projections_do_not() {
    // Found by a test, not by inspection: deriving `nesting` from `projections`
    // made every merged arm read `unknown`, so every multi-run diff advised
    // "re-run at `tree`" to someone whose runs were already at `tree`. A remedy
    // that cannot be acted on teaches the reader to ignore the ones that can.
    let tree_no_nesting = vec![
        stored("r1", 10, 10, Some(TraceIngestLoss::default())),
        stored("r2", 10, 10, Some(TraceIngestLoss::default())),
    ];
    let merged = Arm::merged("c@*".into(), &tree_no_nesting).unwrap();
    assert!(merged.projections.is_none(), "projections do not merge");
    assert_eq!(merged.nesting, "undetected", "but the verdict survives");

    // And a merged diff of two clean tree-level arms carries no nesting mark.
    let other = Arm::merged("d@*".into(), &tree_no_nesting).unwrap();
    let d = diff(merged, other, &DiffOptions::default()).unwrap();
    assert!(
        !d.marks.iter().any(|m| m.kind == "nesting"),
        "marks: {:?}",
        d.marks
    );
}

#[test]
fn the_merged_nesting_verdict_folds_by_severity() {
    // `detected` anywhere means the merged `total_ms` double-counts; one run
    // that could not tell makes the answer unknown rather than clean. Built by
    // hand because the fold is the thing under test, not the ingest path.
    let nested = {
        let c = Collector::new(def_named("c", "sv=svc", Level::Tree, vec![]), at(0));
        c.ingest(&span(1, None, S, 100 * MS, "outer"));
        c.ingest(&span(2, Some(1), S + 10 * MS, 50 * MS, "inner"));
        let view = c.swap(at(60));
        let projected = crate::collector::project::project_for_snapshot(&view);
        StoredSnapshot::from_view(
            "nested".into(),
            None,
            serde_json::Value::Null,
            at(60),
            SnapshotPolicy::default(),
            view,
            Some(TraceIngestLoss::default()),
            projected,
        )
    };
    let flat = stored("flat", 10, 10, Some(TraceIngestLoss::default()));
    let blind = {
        let mut s = stored("blind", 10, 10, Some(TraceIngestLoss::default()));
        // A run recorded below `tree`: it cannot answer the question at all.
        s.def = Arc::new(def_named("c", "sv=svc", Level::Timing, vec![]));
        s
    };

    let v = |runs: Vec<StoredSnapshot>| Arm::merged("c@*".into(), &runs).unwrap().nesting;
    assert_eq!(v(vec![flat.clone(), flat.clone()]), "undetected");
    assert_eq!(v(vec![flat.clone(), blind.clone()]), "unknown");
    assert_eq!(
        v(vec![flat.clone(), nested.clone()]),
        "detected",
        "one nested run is enough"
    );
    assert_eq!(
        v(vec![blind, nested]),
        "detected",
        "detected outranks unknown: the double-count is a fact, not a gap"
    );
}

// ---------------------------------------------------------------------------
// Stored path rows (§9.8)
// ---------------------------------------------------------------------------

#[test]
fn a_snapshot_records_call_paths_because_they_cannot_be_computed_later() {
    // The samples a path is walked from are not retained, so a flame graph not
    // taken at snapshot time can never be taken.
    let c = Collector::new(def_named("c", "sv=svc", Level::Tree, vec![]), at(0));
    c.ingest(&span(1, None, S, 100 * MS, "handler"));
    c.ingest(&span(2, Some(1), S + 10 * MS, 60 * MS, "db"));
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);

    let paths: Vec<&str> = projected.paths.iter().map(|p| p.path.as_str()).collect();
    assert!(
        paths.contains(&"handler > db"),
        "the chain is rendered root first: {paths:?}"
    );
    assert!(paths.contains(&"handler"));
    assert!(!projected.paths_truncated);

    // Self time, not total: `handler` keeps 40 ms of its 100 once `db`'s
    // clipped interval is removed.
    let handler = projected
        .paths
        .iter()
        .find(|p| p.path == "handler")
        .unwrap();
    assert!(
        (handler.self_ms - 40.0).abs() < 0.001,
        "{}",
        handler.self_ms
    );
}

#[test]
fn a_scalar_or_timing_snapshot_records_no_paths_because_it_has_no_parent_identity() {
    for level in [Level::Scalar, Level::Timing] {
        let c = Collector::new(def_named("c", "sv=svc", level, vec![]), at(0));
        c.ingest(&span(1, None, S, 10 * MS, "op"));
        let projected = crate::collector::project::project_for_snapshot(&c.swap(at(60)));
        assert!(projected.paths.is_empty(), "{level:?} retains no identity");
        assert!(!projected.paths_truncated, "absent is not truncated");
    }
}

#[test]
fn a_capped_path_list_says_so_because_a_partial_flame_graph_looks_complete() {
    use crate::collector::project::SNAPSHOT_PATH_ROWS;
    let c = Collector::new(def_named("c", "sv=svc", Level::Tree, vec![]), at(0));
    // Distinct names, so each span is its own path.
    for i in 0..(SNAPSHOT_PATH_ROWS as u64 + 50) {
        c.ingest(&span(i + 1, None, S, 10 * MS, &format!("op{i}")));
    }
    let projected = crate::collector::project::project_for_snapshot(&c.swap(at(60)));
    assert!(projected.paths.len() <= SNAPSHOT_PATH_ROWS);
    assert!(
        projected.paths_truncated,
        "nothing about looking at a flame graph reveals that mass is missing"
    );
}

#[test]
fn path_rows_survive_a_persistence_round_trip() {
    use crate::collector::persist::PersistedSnapshot;
    let s = {
        let c = Collector::new(def_named("c", "sv=svc", Level::Tree, vec![]), at(0));
        c.ingest(&span(1, None, S, 100 * MS, "handler"));
        c.ingest(&span(2, Some(1), S + 10 * MS, 60 * MS, "db"));
        let view = c.swap(at(60));
        let projected = crate::collector::project::project_for_snapshot(&view);
        StoredSnapshot::from_view(
            "r".into(),
            None,
            serde_json::Value::Null,
            at(60),
            SnapshotPolicy::default(),
            view,
            None,
            projected,
        )
    };
    assert_eq!(s.paths.len(), 2);

    let json = serde_json::to_string(&PersistedSnapshot::of(&s)).expect("encode");
    let back: PersistedSnapshot = serde_json::from_str(&json).expect("decode");
    let restored = back.restore("c").expect("restore");
    assert_eq!(restored.paths.len(), 2);
    assert_eq!(restored.paths[0].path, s.paths[0].path);
    assert_eq!(restored.paths_truncated, s.paths_truncated);
}

#[test]
fn a_snapshot_file_written_before_paths_existed_still_loads() {
    // The reason FORMAT_VERSION does not move for this field: the only version
    // gate refuses a file NEWER than this build, so an additive optional field
    // has to read correctly from an older one or the choice was wrong.
    use crate::collector::persist::PersistedSnapshot;
    let s = stored("r", 5, 10, None);
    let mut v = serde_json::to_value(PersistedSnapshot::of(&s)).expect("encode");
    v.as_object_mut().unwrap().remove("paths");
    v.as_object_mut().unwrap().remove("paths_truncated");

    let back: PersistedSnapshot = serde_json::from_value(v).expect("an older file still parses");
    let restored = back.restore("c").expect("restore");
    assert!(restored.paths.is_empty());
    assert!(!restored.paths_truncated);
    assert_eq!(restored.total.count, 5, "and the rest of the run survives");
}

#[test]
fn an_arm_built_from_a_stored_snapshot_reports_the_recorded_definition() {
    // §6.3's whole reason for existing: a diff must not read the LIVE
    // collector's definition and present it as proof of what a recorded run
    // measured. Editing the collector after the snapshot must not move it.
    let c = Collector::new(def_named("c", "sn=old", Level::Tree, vec![]), at(0));
    c.ingest(&span(1, None, S, 10 * MS, "old"));
    let stored = StoredSnapshot::from_view(
        "baseline".into(),
        Some("before".into()),
        serde_json::json!({"git_sha": "abc123"}),
        at(1),
        SnapshotPolicy::default(),
        c.swap(at(1)),
        None,
        Projected::default(),
    );
    assert_eq!(stored.def.filter_string, "sn=old");

    // The live collector moves on; the snapshot does not.
    let edited = c.with_def(def_named("c", "sn=new", Level::Tree, vec![]));
    assert_eq!(edited.def().filter_string, "sn=new");
    assert_eq!(stored.def.filter_string, "sn=old", "recorded, not live");
    assert_eq!(stored.meta["git_sha"], "abc123");
}
