//! §8: threshold triggers over a rolling bucket ring advanced by span arrival.
//!
//! The load-bearing property is the one §8 states rather than discovers: **with
//! no traffic a breached threshold neither fires nor clears.** Several tests
//! below exist only to pin that, because it is the behaviour someone will
//! mistake for a bug.

use super::*;

const MS: i64 = 1;

fn th(metric: Metric, op: Op, value: f64, window_ms: u64) -> Threshold {
    Threshold {
        metric,
        group: None,
        op,
        value,
        window_ms,
    }
}

/// Feed `n` spans of `each_ms`, one per millisecond starting at `from_ms`.
fn feed(s: &ThresholdState, from_ms: i64, n: usize, each_ms: i64, errors: bool) {
    for i in 0..n as i64 {
        s.record(
            from_ms + i * MS,
            Duration::Ok(each_ms * 1_000_000),
            errors,
            None,
        );
    }
}

// ---------------------------------------------------------------------------
// The ring
// ---------------------------------------------------------------------------

#[test]
fn a_count_threshold_breaches_when_the_window_fills_and_clears_as_it_rolls_off() {
    let s = ThresholdState::new(th(Metric::Count, Op::Gt, 20.0, 1600));
    assert!(!s.report().breached, "nothing has arrived");
    assert!(
        s.report().last_value.is_none(),
        "and there is no value yet — unknown, not zero"
    );

    // 25 spans inside one window: over the limit of 20.
    feed(&s, 0, 25, 1, false);
    let r = s.report();
    assert!(r.breached, "25 > 20");
    assert_eq!(r.fires, 1);
    assert_eq!(r.last_value, Some(25.0));

    // Move a full window past them. The old buckets age out, so the window is
    // down to the newly arriving spans only.
    feed(&s, 10_000, 3, 1, false);
    let r = s.report();
    assert!(!r.breached, "the old spans are outside the window now");
    assert_eq!(r.last_value, Some(3.0));
    assert_eq!(r.fires, 1, "a clear does not decrement the fire count");
}

#[test]
fn fires_counts_transitions_not_evaluations() {
    // "Did this happen" is a question about transitions. A threshold breached
    // for a whole run has fired once; counting evaluations would report
    // thousands and answer a question nobody asked.
    let s = ThresholdState::new(th(Metric::Count, Op::Gte, 5.0, 1600));
    feed(&s, 0, 50, 1, false);
    assert!(s.report().breached);
    assert_eq!(s.report().fires, 1, "one continuous breach is one fire");

    // Roll off, then breach again.
    feed(&s, 10_000, 1, 1, false);
    assert!(!s.report().breached);
    feed(&s, 10_001, 10, 1, false);
    assert_eq!(s.report().fires, 2, "a second crossing is a second fire");
}

#[test]
fn an_avg_ms_threshold_uses_the_windowed_average() {
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Gt, 50.0, 1600));
    feed(&s, 0, 10, 10, false);
    assert!(!s.report().breached, "10ms average is under 50");
    assert_eq!(s.report().last_value, Some(10.0));

    // A burst of slow spans in a fresh window.
    feed(&s, 10_000, 10, 100, false);
    assert!(s.report().breached, "100ms average is over 50");
    assert_eq!(s.report().last_value, Some(100.0));
}

#[test]
fn an_average_over_an_empty_window_is_undefined_and_holds_the_previous_verdict() {
    // Reporting zero would make `avg_ms < 5` fire on an empty window, which is
    // the opposite of a load-time guard.
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Lt, 5.0, 1600));
    assert!(!s.report().breached);
    assert!(!s.report().evaluated, "no evaluation has happened");

    // A span far in the future: its own bucket is the only live one, so the
    // window is non-empty and the average is real.
    feed(&s, 100_000, 1, 100, false);
    assert!(s.report().evaluated);
    assert_eq!(s.report().last_value, Some(100.0));
    assert!(!s.report().breached, "100ms is not < 5ms");
}

#[test]
fn an_error_rate_threshold_is_a_percentage_of_the_window() {
    let s = ThresholdState::new(th(Metric::ErrorRatePct, Op::Gte, 25.0, 1600));
    // 3 clean, 1 error: 25%.
    feed(&s, 0, 3, 10, false);
    feed(&s, 3, 1, 10, true);
    assert_eq!(s.report().last_value, Some(25.0));
    assert!(s.report().breached, "25 >= 25");

    // A fresh window with one error in ten: 10%.
    feed(&s, 10_000, 9, 10, false);
    feed(&s, 10_009, 1, 10, true);
    assert_eq!(s.report().last_value, Some(10.0));
    assert!(!s.report().breached);
}

#[test]
fn an_error_count_threshold_counts_only_errors() {
    let s = ThresholdState::new(th(Metric::ErrorCount, Op::Gt, 2.0, 1600));
    feed(&s, 0, 50, 10, false);
    assert!(!s.report().breached, "50 clean spans are not 3 errors");
    feed(&s, 60, 3, 10, true);
    assert!(s.report().breached);
    assert_eq!(s.report().last_value, Some(3.0));
}

#[test]
fn a_span_older_than_the_window_is_dropped_rather_than_folded_into_the_oldest_bucket() {
    // Attributing late-arriving work to the current window would report a breach
    // the window did not contain.
    let s = ThresholdState::new(th(Metric::Count, Op::Gte, 10.0, 1600));
    feed(&s, 100_000, 2, 10, false);
    assert_eq!(s.report().last_value, Some(2.0));

    // Twenty spans from long ago: outside the ring, so they change nothing.
    feed(&s, 0, 20, 10, false);
    assert_eq!(
        s.report().last_value,
        Some(2.0),
        "ancient spans must not fill the current window"
    );
    assert!(!s.report().breached);
}

#[test]
fn no_traffic_neither_fires_nor_clears_a_breached_threshold() {
    // §8, stated rather than discovered. This is the property most likely to be
    // mistaken for a bug, so it is pinned in both directions.
    let s = ThresholdState::new(th(Metric::Count, Op::Gt, 5.0, 1600));
    feed(&s, 0, 10, 10, false);
    assert!(s.report().breached);

    // Time passing is not an input. Reading it a thousand times changes nothing.
    for _ in 0..1000 {
        assert!(s.report().breached, "the verdict is held, not recomputed");
    }
    assert_eq!(s.report().fires, 1);

    // And the report says so, so a stuck `true` on a finished run is
    // distinguishable from a defect.
    assert!(s.report().note.contains("no traffic"));
    assert!(s.report().note.contains("neither fires nor clears"));
}

#[test]
fn a_group_scoped_threshold_ignores_other_groups_entirely() {
    let mut def = th(Metric::Count, Op::Gte, 3.0, 1600);
    def.group = Some("enabled".into());
    let s = ThresholdState::new(def);

    // A flood in another group: not counted, and — importantly — it does not
    // advance the ring either, so it cannot age the watched group's window out.
    for i in 0..1000i64 {
        s.record(i, Duration::Ok(1_000_000), false, Some("disabled"));
    }
    assert!(!s.report().evaluated, "nothing in the watched group yet");

    for i in 0..2i64 {
        s.record(i, Duration::Ok(1_000_000), false, Some("enabled"));
    }
    assert!(!s.report().breached, "2 < 3");
    s.record(2, Duration::Ok(1_000_000), false, Some("enabled"));
    assert!(s.report().breached, "the third one crosses it");
}

#[test]
fn a_span_with_no_group_never_matches_a_group_scoped_threshold() {
    let mut def = th(Metric::Count, Op::Gte, 1.0, 1600);
    def.group = Some("enabled".into());
    let s = ThresholdState::new(def);
    for i in 0..10i64 {
        s.record(i, Duration::Ok(1_000_000), false, None);
    }
    assert!(!s.report().evaluated);
}

#[test]
fn a_malformed_or_negative_duration_counts_toward_count_but_not_toward_total() {
    // The same rule the exact tier uses: an unusable duration is still a matched
    // span, and silently treating it as zero would drag an average down.
    let s = ThresholdState::new(th(Metric::TotalMs, Op::Gt, 0.0, 1600));
    for i in 0..5i64 {
        s.record(i, Duration::Negative(-1_000_000), false, None);
    }
    assert_eq!(s.report().last_value, Some(0.0), "no duration accumulated");
    assert!(!s.report().breached);

    let c = ThresholdState::new(th(Metric::Count, Op::Gte, 5.0, 1600));
    for i in 0..5i64 {
        c.record(i, Duration::Negative(-1_000_000), false, None);
    }
    assert!(c.report().breached, "but they are still matched spans");
}

#[test]
fn a_negative_producer_clock_does_not_panic_or_wrap() {
    // The producer's clock arrives over a network. A negative millisecond is
    // nonsense, and the arithmetic must not be the thing that discovers it.
    let s = ThresholdState::new(th(Metric::Count, Op::Gte, 1.0, 1600));
    s.record(i64::MIN, Duration::Ok(1_000_000), false, None);
    s.record(-5000, Duration::Ok(1_000_000), false, None);
    s.record(i64::MAX, Duration::Ok(1_000_000), false, None);
    // No assertion about the verdict: the point is that none of these panic and
    // none of them wrap into a bucket they do not belong in.
    let _ = s.report();
}

#[test]
fn the_window_is_divided_into_buckets_so_it_rolls_gradually() {
    // A single-slot window would clear all at once; the ring exists so the
    // window slides.
    let s = ThresholdState::new(th(Metric::Count, Op::Gte, 16.0, 1600));
    // One span per bucket, across the whole ring.
    let bucket_ms = 1600 / BUCKETS as i64;
    for i in 0..BUCKETS as i64 {
        s.record(i * bucket_ms, Duration::Ok(1_000_000), false, None);
    }
    assert_eq!(s.report().last_value, Some(BUCKETS as f64));

    // Advance by one bucket: the oldest falls off, so the count drops by one
    // rather than to zero.
    s.record(
        BUCKETS as i64 * bucket_ms,
        Duration::Ok(1_000_000),
        false,
        None,
    );
    assert_eq!(
        s.report().last_value,
        Some(BUCKETS as f64),
        "one arrived, one aged out"
    );
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

#[test]
fn a_percentile_metric_is_refused_with_the_design_reason_not_a_shrug() {
    // "Why not p95" is the first thing anyone asks, and the answer is a bounded
    // memory budget rather than an oversight.
    for m in ["p95_ms", "p99", "p50_ms"] {
        let e = Metric::parse(m).expect_err("refused");
        assert!(e.contains("sketch per bucket"), "{e}");
        assert!(e.contains("bounded"), "{e}");
        assert!(e.contains("avg_ms"), "and names the alternative: {e}");
        assert!(e.contains("collectors.get"), "{e}");
    }
    let e = Metric::parse("nonsense").expect_err("refused");
    assert!(e.contains("expected"), "{e}");
}

#[test]
fn a_non_finite_threshold_value_is_refused() {
    for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let e = th(Metric::Count, Op::Gt, v, 1000)
            .validate(&[])
            .expect_err("refused");
        assert!(e.contains("finite"), "{e}");
    }
}

#[test]
fn a_window_outside_the_permitted_range_is_refused_with_both_bounds() {
    for w in [0, 1, MIN_WINDOW_MS - 1, MAX_WINDOW_MS + 1, u64::MAX] {
        let e = th(Metric::Count, Op::Gt, 1.0, w)
            .validate(&[])
            .expect_err("refused");
        assert!(e.contains(&MIN_WINDOW_MS.to_string()), "{e}");
        assert!(e.contains(&MAX_WINDOW_MS.to_string()), "{e}");
    }
    assert!(th(Metric::Count, Op::Gt, 1.0, MIN_WINDOW_MS)
        .validate(&[])
        .is_ok());
    assert!(th(Metric::Count, Op::Gt, 1.0, MAX_WINDOW_MS)
        .validate(&[])
        .is_ok());
}

#[test]
fn a_group_scoped_threshold_on_a_collector_with_no_group_keys_is_refused() {
    let mut def = th(Metric::Count, Op::Gt, 1.0, 1000);
    def.group = Some("enabled".into());
    let e = def.validate(&[]).expect_err("refused");
    assert!(e.contains("no group_keys"), "{e}");
    assert!(e.contains("drop `group`"), "and what to do: {e}");

    assert!(def.validate(&["cache.enabled".to_string()]).is_ok());
}

#[test]
fn an_out_of_range_error_rate_is_refused_because_it_can_never_be_crossed() {
    for v in [-1.0, 101.0, 1000.0] {
        let e = th(Metric::ErrorRatePct, Op::Gt, v, 1000)
            .validate(&[])
            .expect_err("refused");
        assert!(e.contains("percentage"), "{e}");
    }
    assert!(th(Metric::ErrorRatePct, Op::Gt, 0.0, 1000)
        .validate(&[])
        .is_ok());
    assert!(th(Metric::ErrorRatePct, Op::Gte, 100.0, 1000)
        .validate(&[])
        .is_ok());
}

#[test]
fn a_downward_threshold_is_permitted_and_warned_about() {
    // Legal, and the single most likely misunderstanding: it detects a drop
    // WHILE traffic continues, and does nothing at all if traffic stops.
    for op in [Op::Lt, Op::Lte] {
        let w = th(Metric::Count, op, 10.0, 1000)
            .validate(&[])
            .expect("permitted");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("will NOT fire if traffic stops"), "{}", w[0]);
        assert!(w[0].contains("not a liveness check"), "{}", w[0]);
    }
    // And an upward one is not warned about, so the warning means something.
    for op in [Op::Gt, Op::Gte] {
        assert!(th(Metric::Count, op, 10.0, 1000)
            .validate(&[])
            .unwrap()
            .is_empty());
    }
}

#[test]
fn a_total_ms_threshold_warns_that_summed_work_can_exceed_the_window() {
    let w = th(Metric::TotalMs, Op::Gt, 100.0, 1000)
        .validate(&[])
        .expect("permitted");
    assert!(
        w.iter()
            .any(|x| x.contains("summed work, not elapsed time")),
        "{w:?}"
    );
    assert!(w.iter().any(|x| x.contains("concurrency")), "{w:?}");
}

#[test]
fn ops_and_metrics_round_trip_through_their_string_forms() {
    for (s, op) in [
        ("gt", Op::Gt),
        ("gte", Op::Gte),
        ("lt", Op::Lt),
        ("lte", Op::Lte),
    ] {
        assert_eq!(Op::parse(s).unwrap(), op);
        assert_eq!(op.as_str(), s);
    }
    // The symbolic spellings are accepted but not canonical.
    assert_eq!(Op::parse(">=").unwrap(), Op::Gte);
    assert!(Op::parse("=").is_err());

    for m in [
        Metric::Count,
        Metric::TotalMs,
        Metric::AvgMs,
        Metric::ErrorCount,
        Metric::ErrorRatePct,
    ] {
        assert_eq!(Metric::parse(m.as_str()).unwrap(), m);
    }
}

// ---------------------------------------------------------------------------
// Concurrency (A4's shape, applied here)
// ---------------------------------------------------------------------------

#[test]
fn concurrent_recording_neither_panics_nor_loses_the_breach() {
    use std::sync::Arc;
    let s = Arc::new(ThresholdState::new(th(Metric::Count, Op::Gte, 100.0, 1600)));
    let mut hs = Vec::new();
    for t in 0..8u64 {
        let s = s.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..200i64 {
                // All within one window, so every thread contributes to the
                // same buckets and they contend on the same slots.
                s.record(
                    (t as i64 * 7 + i) % 100,
                    Duration::Ok(1_000_000),
                    i % 10 == 0,
                    None,
                );
            }
        }));
    }
    for h in hs {
        h.join().expect("no panic");
    }
    let r = s.report();
    assert!(r.breached, "1600 spans across a 100ms span of buckets");
    assert!(r.fires >= 1);
    // The count is not asserted exactly: slot reclamation races are benign by
    // design and can drop a handful. What must hold is that the verdict is
    // right and nothing panicked or wrapped.
    assert!(r.last_value.unwrap() >= 100.0, "{:?}", r.last_value);
}

// ---------------------------------------------------------------------------
// Found by the pre-merge gate
// ---------------------------------------------------------------------------

#[test]
fn an_average_divides_by_the_spans_that_contributed_not_by_every_matched_span() {
    // The mistake `ExactStats::well_formed_count` exists to prevent, made here:
    // dividing by `count` understates the average by exactly the proportion of
    // unusable input — quietly, and worst on the dirtiest data. And
    // `Duration::Negative` is routine under `tokio::spawn` instrumentation and
    // cross-process clock skew, not exotic.
    //
    // Four spans of 200ms plus one with end < start. The honest average is 200;
    // dividing by 5 gives 160 and a `> 180` guard silently does not fire.
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Gt, 180.0, 1600));
    for i in 0..4i64 {
        s.record(i, Duration::Ok(200 * 1_000_000), false, None);
    }
    s.record(4, Duration::Negative(-1_000_000), false, None);

    assert_eq!(
        s.report().last_value,
        Some(200.0),
        "800ms over the FOUR spans that contributed it, not over all five"
    );
    assert!(s.report().breached, "so the guard fires, as it should");

    // And the same collector's exact tier agrees, which is the point: one
    // `collectors.get` response must not show `exact.avg_ms: 200` beside a
    // `> 180` guard reporting 160 and `breached: false`.
    let mut e = crate::collector::exact::ExactStats::new();
    for _ in 0..4 {
        e.record(Duration::Ok(200 * 1_000_000), false);
    }
    e.record(Duration::Negative(-1_000_000), false);
    assert_eq!(e.avg_ns().map(|v| v / 1e6), Some(200.0), "the two agree");
}

#[test]
fn a_malformed_duration_still_counts_toward_count_and_the_error_rate() {
    // The other side of the fix: excluding unusable durations from the average
    // must not exclude the spans from the population.
    let c = ThresholdState::new(th(Metric::Count, Op::Gte, 5.0, 1600));
    for i in 0..5i64 {
        c.record(i, Duration::Malformed, false, None);
    }
    assert!(
        c.report().breached,
        "five matched spans are five matched spans"
    );

    let r = ThresholdState::new(th(Metric::ErrorRatePct, Op::Gte, 40.0, 1600));
    for i in 0..3i64 {
        r.record(i, Duration::Malformed, i < 2, None);
    }
    let got = r.report().last_value.expect("evaluated");
    // 1e-3, not 1e-9: `last_value` is stored as an integer thousandth of the
    // metric's unit, so it is quantised to 0.001 by construction.
    assert!(
        (got - 200.0 / 3.0).abs() < 1e-3,
        "two errors in three spans is 66.7%, whether or not their durations were \
         usable: got {got}"
    );
    assert!(r.report().breached);
}

#[test]
fn an_average_over_a_window_of_only_unusable_durations_is_undefined() {
    // Not zero. With no span contributing to the sum there is no average, and
    // reporting 0.0 would make `avg_ms < 5` fire on a window of pure clock skew.
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Lt, 5.0, 1600));
    for i in 0..10i64 {
        s.record(i, Duration::Negative(-1_000_000), false, None);
    }
    assert!(!s.report().evaluated, "nothing evaluable arrived");
    assert!(!s.report().breached);
    assert!(s.report().last_value.is_none());
}

#[test]
fn clear_discards_the_window_and_the_verdict_reached_over_it() {
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Gt, 50.0, 1600));
    feed(&s, 0, 10, 200, false);
    let before = s.report();
    assert!(before.breached && before.fires == 1);
    assert_eq!(before.last_value, Some(200.0));

    s.clear();
    let after = s.report();
    assert!(!after.breached, "the verdict went with the data");
    assert_eq!(after.fires, 0, "and so did the transition count");
    assert!(
        after.last_value.is_none(),
        "reporting a value computed from discarded data is worse than reporting none"
    );
    assert!(!after.evaluated);

    // And the ring is genuinely empty, not merely reported so: one span in a
    // fresh window must read as one span.
    feed(&s, 100_000, 1, 10, false);
    assert_eq!(s.report().last_value, Some(10.0));
}

#[test]
fn the_evaluated_window_is_never_narrower_than_the_one_declared() {
    // It used to be narrower by `window_ms % BUCKETS` — an ABSOLUTE error, so at
    // `window_ms: 31` the real window was 16 ms: a 48% shortfall against a doc
    // promising "at most 1/16". Rounding up overshoots instead, which is the
    // conservative direction for a guard.
    for w in [MIN_WINDOW_MS, 17, 31, 100, 1000, 5000, MAX_WINDOW_MS] {
        let s = ThresholdState::new(th(Metric::Count, Op::Gte, 1.0, w));
        let effective = s.effective_window_ms();
        assert!(
            effective >= w,
            "window_ms {w} evaluated over only {effective}ms"
        );
        // And never wildly wider: at most one bucket over.
        assert!(
            effective < w + w.div_ceil(BUCKETS as u64) + BUCKETS as u64,
            "window_ms {w} overshot to {effective}ms"
        );
    }
}

#[test]
fn a_span_exactly_a_window_old_is_dropped_and_one_just_inside_is_kept() {
    // The boundary of the staleness check, on the slot-COLLISION path the older
    // test missed: an epoch a whole ring cycle back maps to the same slot as a
    // live one, so if it were accepted it would reclaim that slot and delete
    // real in-window data.
    let window = 1600u64;
    let s = ThresholdState::new(th(Metric::Count, Op::Gte, 2.0, window));
    let bucket = (window.div_ceil(BUCKETS as u64)) as i64;

    // Establish a window at a high epoch.
    let base = 1_000_000i64;
    s.record(base, Duration::Ok(1_000_000), false, None);
    assert_eq!(s.report().last_value, Some(1.0));

    // Exactly one ring cycle back: same slot, and outside the window.
    s.record(
        base - bucket * BUCKETS as i64,
        Duration::Ok(1_000_000),
        false,
        None,
    );
    assert_eq!(
        s.report().last_value,
        Some(1.0),
        "a span a full cycle old must neither count nor clear the slot it aliases"
    );

    // One bucket back is inside the window and does count.
    s.record(base - bucket, Duration::Ok(1_000_000), false, None);
    assert_eq!(s.report().last_value, Some(2.0));
}

#[test]
fn the_verdict_and_the_value_it_came_from_are_read_as_one() {
    // They were three independent atomics, so a reader could pair a `breached`
    // from one evaluation with a `last_value` from another and print a verdict
    // that does not follow from the number beside it.
    let s = ThresholdState::new(th(Metric::AvgMs, Op::Gt, 50.0, 1600));
    feed(&s, 0, 5, 100, false);

    // The property, stated as an invariant a reader can check: whenever a value
    // is reported, applying the declared op to it reproduces `breached`.
    for _ in 0..100 {
        let r = s.report();
        if let Some(v) = r.last_value {
            assert_eq!(
                r.breached,
                v > r.value,
                "the verdict must follow from the value printed beside it: {r:?}"
            );
        }
    }
}

#[test]
fn a_group_scoped_threshold_on_several_group_keys_warns_that_only_the_first_counts() {
    // Silent non-matching is indistinguishable from no traffic, and this is the
    // one in-range misconfiguration that produces it.
    let mut def = th(Metric::Count, Op::Gt, 1.0, 1000);
    def.group = Some("us-east".into());
    let w = def
        .validate(&["service.name".to_string(), "region".to_string()])
        .expect("permitted");
    assert!(
        w.iter().any(|x| x.contains("FIRST declared group key")),
        "{w:?}"
    );
    assert!(w.iter().any(|x| x.contains("service.name")), "{w:?}");

    // One key: no warning, so the warning means something.
    assert!(def
        .validate(&["region".to_string()])
        .unwrap()
        .iter()
        .all(|x| !x.contains("FIRST declared")));
}
