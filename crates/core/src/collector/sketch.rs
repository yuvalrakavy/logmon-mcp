//! The duration sketch — spec §3.5.
//!
//! DDSketch, fed at ingest so percentiles cover every matched span regardless
//! of what the sample tier retained. Everything the spec pins about it lives
//! here: the algorithm, the input unit, the enforced range, and the layout
//! identity that has to travel with the buckets into snapshots and documents.

use sketches_ddsketch::{Config, DDSketch};

/// Relative accuracy. Percentiles read from the sketch are within ±1 % of the
/// true order statistic, and that is the bound reported as `alpha_pct`.
pub const ALPHA: f64 = 0.01;

/// Input unit is **integer nanoseconds**. A millisecond-fed sketch collapses
/// every sub-millisecond span into one bucket with no error guarantee — which
/// is exactly the population a cache creates, so it would destroy the driving
/// measurement.
pub const MIN_NS: f64 = 1.0;
/// One hour. Durations above this are clamped (see [`DurationSketch::record`]).
pub const MAX_NS: f64 = 3.6e12;

/// Set explicitly rather than inherited. DDSketch has no upper bound of its
/// own; its only ceiling is `bin_limit`, and exceeding it **collapses the low
/// end** — silently doing the thing nanosecond input was chosen to prevent.
///
/// 1 ns … 1 h needs `ln(3.6e12) / ln(gamma)` ≈ 1446 keys at α = 0.01, so 2048
/// leaves headroom. Clamping means the span can never exceed that anyway; this
/// is the second of the two guards.
pub const MAX_NUM_BINS: u32 = 2048;

/// What happened to a value offered to the sketch. Returned so the caller can
/// account for the values that did not land in it — a count that silently
/// omits its exclusions is the failure this whole design is built against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// Added normally.
    Added,
    /// `duration <= MIN_NS`. Counted in the sketch's zero bucket and reported
    /// as `0.0`. A legitimate population: a cache hit, or anything below clock
    /// resolution. Not malformed.
    Zero,
    /// Above `MAX_NS`; clamped to it and added.
    Clamped,
    /// Negative. **Not** added. The crate would route it to a negative store
    /// where it participates in `quantile()`, and a negative p50 is not a
    /// percentile anyone wants.
    NegativeRejected,
}

/// The layout identity that travels with the buckets (§3.5, §9.4).
///
/// Recorded by logmon rather than read back from the sketch so it can be
/// persisted and compared. §5.6 refuses to merge or diff across mismatched
/// layouts, and that refusal is only as trustworthy as this record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SketchLayout {
    pub algo: &'static str,
    pub alpha: f64,
    pub unit: &'static str,
    pub min_ns: f64,
    pub max_ns: f64,
    pub max_num_bins: u32,
}

impl SketchLayout {
    pub const CURRENT: SketchLayout = SketchLayout {
        algo: "ddsketch",
        alpha: ALPHA,
        unit: "ns",
        min_ns: MIN_NS,
        max_ns: MAX_NS,
        max_num_bins: MAX_NUM_BINS,
    };
}

/// A duration distribution over matched spans.
#[derive(Clone)]
pub struct DurationSketch {
    inner: DDSketch,
    layout: SketchLayout,
}

impl DurationSketch {
    pub fn new() -> Self {
        Self {
            inner: DDSketch::new(Config::new(ALPHA, MAX_NUM_BINS, MIN_NS)),
            layout: SketchLayout::CURRENT,
        }
    }

    pub fn layout(&self) -> SketchLayout {
        self.layout
    }

    /// Offer one span's duration, in nanoseconds.
    ///
    /// Clamping to the declared range before `add` is what makes the range a
    /// constraint rather than documentation, and is what guarantees the
    /// `bin_limit` low-end collapse can never fire.
    pub fn record(&mut self, duration_ns: i64) -> Recorded {
        if duration_ns < 0 {
            return Recorded::NegativeRejected;
        }
        let v = duration_ns as f64;
        if v <= MIN_NS {
            // The crate routes |v| <= min_possible() to its zero bucket.
            self.inner.add(0.0);
            return Recorded::Zero;
        }
        if v > MAX_NS {
            self.inner.add(MAX_NS);
            return Recorded::Clamped;
        }
        self.inner.add(v);
        Recorded::Added
    }

    /// Quantile in nanoseconds, or `None` when empty.
    ///
    /// `q` is a fraction in `0.0..=1.0`. The crate's rank is
    /// `(q * (n - 1)) as u64` — the paper's lower quantile, ⌊1 + q(n−1)⌋
    /// 1-indexed — which is the convention §5.7 pins for every percentile in
    /// the design, precisely so this call needs no adjustment.
    pub fn quantile_ns(&self, q: f64) -> Option<f64> {
        self.inner.quantile(q).ok().flatten()
    }

    pub fn count(&self) -> usize {
        self.inner.count()
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Exact merge. Fails when the layouts differ — the arithmetic would be
    /// wrong, and the layout is a recorded fact, so this is the one hard
    /// refusal §5.6 keeps.
    pub fn merge(&mut self, other: &DurationSketch) -> Result<(), SketchMergeError> {
        let mismatch = || {
            SketchMergeError::LayoutMismatch(Box::new(LayoutMismatch {
                theirs: other.layout,
                ours: self.layout,
            }))
        };
        if self.layout != other.layout {
            return Err(mismatch());
        }
        self.inner.merge(&other.inner).map_err(|_| mismatch())
    }

    /// Compact binary form for persistence and documents (§10, §9.7).
    /// Deliberately not the serde derive, whose fields are `pub(crate)` and
    /// whose layout changed between 0.2 and 0.4 — persisting it would make the
    /// on-disk format hostage to a 0.x crate's private internals.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.to_java_bytes()
    }
}

impl Default for DurationSketch {
    fn default() -> Self {
        Self::new()
    }
}

/// Which two layouts failed to reconcile. Boxed into the error so the `Ok`
/// path of `merge` does not carry two layouts' worth of dead weight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutMismatch {
    pub theirs: SketchLayout,
    pub ours: SketchLayout,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SketchMergeError {
    LayoutMismatch(Box<LayoutMismatch>),
}

impl std::fmt::Display for SketchMergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SketchMergeError::LayoutMismatch(m) => write!(
                f,
                "sketch layout mismatch: cannot merge {:?} into {:?}",
                m.theirs, m.ours
            ),
        }
    }
}

impl std::error::Error for SketchMergeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact nearest-rank reference, §5.7: rank = floor(q * (n - 1)), 0-indexed
    /// over the sorted sample. Deliberately spelled out rather than reused from
    /// the implementation, so the two can disagree.
    fn exact_quantile(sorted: &[i64], q: f64) -> i64 {
        let rank = (q * (sorted.len() as f64 - 1.0)) as usize;
        sorted[rank]
    }

    #[test]
    fn percentiles_are_within_alpha_of_the_true_order_statistic() {
        // Heavy-tailed: the case where a fixed-width bucketing would drift.
        // Scaled to 1 µs … 100 s, i.e. plausible span durations. Values at or
        // below the 1 ns range floor are deliberately `Zero` (see
        // `zero_duration_spans_are_counted_and_read_as_zero`), so a
        // distribution starting at 1 would be testing the floor, not accuracy.
        let values: Vec<i64> = (1..=10_000)
            .map(|i| (i as i64) * (i as i64) * 1_000)
            .collect();
        let mut s = DurationSketch::new();
        for v in &values {
            assert_eq!(s.record(*v), Recorded::Added);
        }
        let mut sorted = values.clone();
        sorted.sort_unstable();

        for q in [0.5, 0.8, 0.95, 0.99] {
            let got = s.quantile_ns(q).expect("non-empty");
            let want = exact_quantile(&sorted, q) as f64;
            let rel = (got - want).abs() / want;
            assert!(
                rel <= ALPHA,
                "q={q}: sketch {got} vs exact {want}, relative error {rel} exceeds {ALPHA}"
            );
        }
    }

    #[test]
    fn negative_durations_are_rejected_not_absorbed() {
        let mut s = DurationSketch::new();
        assert_eq!(s.record(-1), Recorded::NegativeRejected);
        assert_eq!(s.record(-1_000_000), Recorded::NegativeRejected);
        assert!(
            s.is_empty(),
            "a rejected value must not reach the sketch at all"
        );

        // Control: without the guard the crate would keep them in a negative
        // store and let them come back out of quantile().
        s.record(500_000);
        assert_eq!(s.count(), 1);
        assert!(
            s.quantile_ns(0.0).expect("non-empty") >= 0.0,
            "no negative can surface as a percentile"
        );
    }

    #[test]
    fn zero_duration_spans_are_counted_and_read_as_zero() {
        // A cache hit, or anything below clock resolution. Legitimate, not
        // malformed — and the driving A/B manufactures them by design.
        let mut s = DurationSketch::new();
        assert_eq!(s.record(0), Recorded::Zero);
        assert_eq!(s.record(1), Recorded::Zero, "1 ns is at the range floor");
        assert_eq!(s.count(), 2);
        assert_eq!(s.quantile_ns(0.5), Some(0.0));
    }

    #[test]
    fn out_of_range_values_clamp_and_do_not_collapse_the_low_end() {
        // A6c. The failure this guards: exceeding `bin_limit` makes DDSketch
        // fold the whole low end into one bucket, silently destroying exactly
        // the sub-millisecond resolution the ns unit was chosen for.
        let mut s = DurationSketch::new();
        assert_eq!(s.record(1_000), Recorded::Added); // 1 µs

        // ~54 years, in nanoseconds. Not a hypothetical: the OTLP HTTP path
        // defaults a missing `startTimeUnixNano` to 0, so a malformed span
        // arrives with a duration of the whole Unix epoch. It is also wide
        // enough to matter — the key span from 1 ns to this value exceeds
        // MAX_NUM_BINS, so without the clamp DDSketch folds the low end into a
        // single bucket. A 24-hour value would NOT have exercised that, which
        // is what this test originally used.
        assert_eq!(
            s.record(1_700_000_000_000_000_000),
            Recorded::Clamped,
            "an epoch-wide malformed duration is clamped to the declared max"
        );

        // The 1 µs value must still be readable to within alpha.
        let got = s.quantile_ns(0.0).expect("non-empty");
        let rel = (got - 1_000.0).abs() / 1_000.0;
        assert!(
            rel <= ALPHA,
            "low end survived the out-of-range value: got {got}, want ~1000 (rel {rel})"
        );
    }

    #[test]
    fn merge_is_exact_across_identical_layouts() {
        let mut a = DurationSketch::new();
        let mut b = DurationSketch::new();
        for v in 1..=500 {
            a.record(v * 1_000);
        }
        for v in 501..=1_000 {
            b.record(v * 1_000);
        }
        let mut whole = DurationSketch::new();
        for v in 1..=1_000 {
            whole.record(v * 1_000);
        }

        a.merge(&b).expect("identical layouts merge");
        assert_eq!(a.count(), whole.count());
        for q in [0.5, 0.95] {
            assert_eq!(
                a.quantile_ns(q),
                whole.quantile_ns(q),
                "merged sketch must equal the one-pass sketch at q={q}"
            );
        }
    }

    #[test]
    fn merge_refuses_a_mismatched_layout_record_even_when_the_config_agrees() {
        // The layout RECORD is the load-bearing thing, because it is what
        // survives into snapshots and documents and gets compared across
        // processes — `Config` only exists for two live sketches.
        //
        // So this deliberately gives both sketches an identical `Config` and
        // differing recorded layouts. The crate would happily merge them; only
        // our own check refuses. Written this way because the obvious version
        // (differing `Config`s) passes with our check deleted — the crate
        // refuses those itself, making the test vacuous.
        let mut a = DurationSketch::new();
        a.record(1_000);

        let mut b = DurationSketch {
            inner: DDSketch::new(Config::new(ALPHA, MAX_NUM_BINS, MIN_NS)),
            layout: SketchLayout {
                unit: "us",
                ..SketchLayout::CURRENT
            },
        };
        b.record(2_000);

        let err = a
            .merge(&b)
            .expect_err("a differing layout record must refuse");
        assert!(matches!(err, SketchMergeError::LayoutMismatch(_)));
        assert_eq!(a.count(), 1, "a refused merge must not have absorbed it");
    }

    #[test]
    fn merge_refuses_a_mismatched_config() {
        // Belt and braces: the crate's own guard, which covers the case where
        // an upgrade changed the numeric accuracy.
        let mut a = DurationSketch::new();
        a.record(1_000);

        let mut b = DurationSketch {
            inner: DDSketch::new(Config::new(0.05, MAX_NUM_BINS, MIN_NS)),
            layout: SketchLayout::CURRENT,
        };
        b.record(2_000);

        assert!(a.merge(&b).is_err(), "differing accuracy must refuse");
    }

    #[test]
    fn layout_identity_is_reported_and_stable() {
        let s = DurationSketch::new();
        let l = s.layout();
        assert_eq!(l.algo, "ddsketch");
        assert_eq!(l.unit, "ns");
        assert_eq!(l.alpha, ALPHA);
        assert_eq!(l.min_ns, MIN_NS);
        assert_eq!(l.max_ns, MAX_NS);
        assert_eq!(l.max_num_bins, MAX_NUM_BINS);
    }

    #[test]
    fn declared_range_fits_inside_the_bin_limit() {
        // The derivation in MAX_NUM_BINS' doc comment, checked rather than
        // asserted: fill the full declared range and confirm the occupied key
        // span never approaches the limit (which is what would collapse it).
        let mut s = DurationSketch::new();
        let mut v = MIN_NS as i64 + 1;
        while (v as f64) < MAX_NS {
            s.record(v);
            v = ((v as f64) * 1.5) as i64 + 1;
        }
        s.record(MAX_NS as i64);
        // Occupancy is far below the limit, so no collapse is possible.
        assert!(s.count() > 0);
        assert!(
            s.quantile_ns(0.0).expect("non-empty") <= MIN_NS * 4.0,
            "the smallest recorded value is still near the range floor"
        );
    }
}
