//! The always-exact tier — spec §3.1.
//!
//! Scalars plus the duration sketch, maintained for the collector as a whole,
//! per span name, and per group. Never capped by the sample budget: this is
//! the tier the headline A/B number comes from, so it must stay correct however
//! long a run goes on.

use crate::collector::sketch::{DurationSketch, Recorded, SketchMergeError};
use crate::span::types::{SpanEntry, SpanStatus};

/// How one span's timestamps classified. Computed **once per span**, then
/// applied to every aggregate that span belongs to (collector, name, group) —
/// classifying per aggregate would be both wasteful and a chance for the three
/// to disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duration {
    /// Well formed and non-negative, in nanoseconds.
    Ok(i64),
    /// `end < start`. Normal under `tokio::spawn` instrumentation and cross-
    /// process clock skew — not an error, but not summable either.
    Negative(i64),
    /// Timestamps that cannot be trusted: either endpoint at the Unix epoch
    /// (the OTLP HTTP path defaults a missing `startTimeUnixNano` to 0, which
    /// yields a ~54-year duration), or outside the representable nanosecond
    /// range.
    Malformed,
}

/// Classify a span's duration from its timestamps.
///
/// Derived from `end_time - start_time`, **not** from `duration_ms`: the
/// integer-nanosecond accumulator exists so the total is exactly reproducible
/// regardless of arrival order, and reading it back out of an `f64` would
/// discard that.
pub fn classify(span: &SpanEntry) -> Duration {
    let (Some(start), Some(end)) = (
        span.start_time.timestamp_nanos_opt(),
        span.end_time.timestamp_nanos_opt(),
    ) else {
        return Duration::Malformed;
    };
    if start == 0 || end == 0 {
        return Duration::Malformed;
    }
    let d = end - start;
    if d < 0 {
        Duration::Negative(d)
    } else {
        Duration::Ok(d)
    }
}

pub fn is_error(span: &SpanEntry) -> bool {
    matches!(span.status, SpanStatus::Error(_))
}

/// Exact scalars over a set of matched spans.
#[derive(Clone)]
pub struct ExactStats {
    /// Every matched span, including those excluded from `total_ns`.
    pub count: u64,
    /// Sum over **well-formed, non-negative** durations only.
    ///
    /// `i128` not because negatives are accumulated — they are not — but
    /// because the accumulator must not overflow on adversarial input. A span
    /// with a corrupt-but-nonzero start timestamp passes the malformed check
    /// and can carry a multi-decade duration; `i64` nanoseconds saturates at
    /// 292 years, so a handful of those would wrap. The extra 8 bytes are paid
    /// once per aggregate, against a sketch measured in kilobytes.
    pub total_ns: i128,
    pub min_ns: Option<i64>,
    pub max_ns: Option<i64>,
    pub error_count: u64,
    pub negative_duration_spans: u64,
    pub malformed_timestamps: u64,
    /// Well-formed but longer than the sketch's declared range, so clamped in
    /// the distribution while counting in full toward `total_ns`. Reported so
    /// the two cannot silently disagree.
    pub out_of_range_spans: u64,
    sketch: DurationSketch,
}

impl ExactStats {
    pub fn new() -> Self {
        Self {
            count: 0,
            total_ns: 0,
            min_ns: None,
            max_ns: None,
            error_count: 0,
            negative_duration_spans: 0,
            malformed_timestamps: 0,
            out_of_range_spans: 0,
            sketch: DurationSketch::new(),
        }
    }

    pub fn record(&mut self, d: Duration, is_error: bool) {
        self.count += 1;
        if is_error {
            self.error_count += 1;
        }
        match d {
            Duration::Malformed => {
                self.malformed_timestamps += 1;
            }
            Duration::Negative(_) => {
                self.negative_duration_spans += 1;
            }
            Duration::Ok(ns) => {
                self.total_ns += ns as i128;
                self.min_ns = Some(self.min_ns.map_or(ns, |m| m.min(ns)));
                self.max_ns = Some(self.max_ns.map_or(ns, |m| m.max(ns)));
                if matches!(self.sketch.record(ns), Recorded::Clamped) {
                    self.out_of_range_spans += 1;
                }
            }
        }
    }

    /// The denominator for `avg`. `count` includes spans excluded from the sum,
    /// so dividing by it would understate the average by exactly the proportion
    /// of malformed input — quietly, and worst on the dirtiest data.
    pub fn well_formed_count(&self) -> u64 {
        self.count - self.negative_duration_spans - self.malformed_timestamps
    }

    pub fn avg_ns(&self) -> Option<f64> {
        let n = self.well_formed_count();
        (n > 0).then(|| self.total_ns as f64 / n as f64)
    }

    /// Whole-run percentile, in nanoseconds, at the sketch's stated accuracy.
    pub fn quantile_ns(&self, q: f64) -> Option<f64> {
        self.sketch.quantile_ns(q)
    }

    pub fn sketch(&self) -> &DurationSketch {
        &self.sketch
    }

    /// Additive merge — the property `collectors.history(merge:)` and the
    /// read-path fold both rest on.
    pub fn merge(&mut self, other: &ExactStats) -> Result<(), SketchMergeError> {
        self.sketch.merge(&other.sketch)?;
        self.count += other.count;
        self.total_ns += other.total_ns;
        self.error_count += other.error_count;
        self.negative_duration_spans += other.negative_duration_spans;
        self.malformed_timestamps += other.malformed_timestamps;
        self.out_of_range_spans += other.out_of_range_spans;
        self.min_ns = match (self.min_ns, other.min_ns) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.max_ns = match (self.max_ns, other.max_ns) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        Ok(())
    }
}

impl Default for ExactStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Nanoseconds as milliseconds, for the wire surface.
pub fn ns_to_ms(ns: i128) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::sketch::MAX_NS;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn span_with(start_ns: i64, end_ns: i64, error: bool) -> SpanEntry {
        SpanEntry {
            seq: 0,
            trace_id: 1,
            span_id: 1,
            parent_span_id: None,
            start_time: Utc.timestamp_nanos(start_ns),
            end_time: Utc.timestamp_nanos(end_ns),
            duration_ms: 0.0,
            name: "s".into(),
            kind: crate::span::types::SpanKind::Internal,
            service_name: "svc".into(),
            status: if error {
                SpanStatus::Error("boom".into())
            } else {
                SpanStatus::Ok
            },
            attributes: HashMap::new(),
            events: vec![],
        }
    }

    const S: i64 = 1_700_000_000_000_000_000; // a plausible wall-clock base

    #[test]
    fn classify_reads_timestamps_not_duration_ms() {
        assert_eq!(
            classify(&span_with(S, S + 5_000, false)),
            Duration::Ok(5_000)
        );
        assert_eq!(
            classify(&span_with(S + 5_000, S, false)),
            Duration::Negative(-5_000)
        );
        // The OTLP zero-default: a missing startTimeUnixNano.
        assert_eq!(classify(&span_with(0, S, false)), Duration::Malformed);
        assert_eq!(classify(&span_with(S, 0, false)), Duration::Malformed);
    }

    #[test]
    fn count_includes_every_match_but_the_sum_does_not() {
        let mut e = ExactStats::new();
        e.record(Duration::Ok(1_000), false);
        e.record(Duration::Ok(3_000), false);
        e.record(Duration::Negative(-500), false);
        e.record(Duration::Malformed, false);

        assert_eq!(e.count, 4, "every matched span is counted");
        assert_eq!(e.total_ns, 4_000, "only well-formed durations are summed");
        assert_eq!(e.negative_duration_spans, 1);
        assert_eq!(e.malformed_timestamps, 1);
        assert_eq!(e.well_formed_count(), 2);
        assert_eq!(
            e.avg_ns(),
            Some(2_000.0),
            "avg divides by the well-formed count, not by count"
        );
    }

    #[test]
    fn a_malformed_epoch_span_cannot_poison_the_total() {
        // Without the malformed guard this single span adds ~54 years to the
        // tier the design calls exact.
        let mut e = ExactStats::new();
        e.record(Duration::Ok(1_000), false);
        e.record(classify(&span_with(0, S, false)), false);

        assert_eq!(e.total_ns, 1_000);
        assert_eq!(e.max_ns, Some(1_000), "and it does not become the maximum");
        assert_eq!(e.malformed_timestamps, 1);
    }

    #[test]
    fn min_max_are_absent_until_a_well_formed_span_arrives() {
        let mut e = ExactStats::new();
        assert_eq!(e.min_ns, None);
        e.record(Duration::Negative(-1), false);
        assert_eq!(e.min_ns, None, "a negative is not a minimum");
        assert_eq!(e.avg_ns(), None, "and there is no average yet");
        e.record(Duration::Ok(7), false);
        assert_eq!((e.min_ns, e.max_ns), (Some(7), Some(7)));
    }

    #[test]
    fn out_of_range_spans_are_counted_and_still_summed() {
        let mut e = ExactStats::new();
        let long = MAX_NS as i64 * 2;
        e.record(Duration::Ok(long), false);
        assert_eq!(
            e.total_ns, long as i128,
            "the exact sum keeps the full duration"
        );
        assert_eq!(
            e.out_of_range_spans, 1,
            "while the distribution says it was clamped"
        );
    }

    #[test]
    fn error_count_tracks_status_independently_of_duration() {
        let mut e = ExactStats::new();
        e.record(Duration::Ok(1), true);
        e.record(Duration::Malformed, true);
        e.record(Duration::Ok(1), false);
        assert_eq!(
            e.error_count, 2,
            "errors count even when excluded from sums"
        );
    }

    #[test]
    fn merge_is_additive_across_every_field() {
        let mut a = ExactStats::new();
        let mut b = ExactStats::new();
        let mut whole = ExactStats::new();
        for (i, d) in [
            Duration::Ok(1_000),
            Duration::Ok(9_000),
            Duration::Negative(-1),
            Duration::Malformed,
        ]
        .into_iter()
        .enumerate()
        {
            whole.record(d, i == 0);
            if i < 2 {
                a.record(d, i == 0);
            } else {
                b.record(d, false);
            }
        }

        a.merge(&b).expect("identical layouts");
        assert_eq!(a.count, whole.count);
        assert_eq!(a.total_ns, whole.total_ns);
        assert_eq!(a.min_ns, whole.min_ns);
        assert_eq!(a.max_ns, whole.max_ns);
        assert_eq!(a.error_count, whole.error_count);
        assert_eq!(a.negative_duration_spans, whole.negative_duration_spans);
        assert_eq!(a.malformed_timestamps, whole.malformed_timestamps);
        assert_eq!(a.sketch().count(), whole.sketch().count());
    }

    #[test]
    fn accumulation_is_order_independent() {
        // §4.3: integer-nanosecond accumulation is associative, so the exact
        // tier does not depend on arrival order. An f64 accumulator would make
        // this assertion fail at the bit level.
        let ds = [
            Duration::Ok(1),
            Duration::Ok(1_000_000_007),
            Duration::Ok(3),
            Duration::Ok(999_999_999_999),
        ];
        let mut forward = ExactStats::new();
        for d in ds {
            forward.record(d, false);
        }
        let mut backward = ExactStats::new();
        for d in ds.into_iter().rev() {
            backward.record(d, false);
        }
        assert_eq!(forward.total_ns, backward.total_ns);
        assert_eq!(forward.avg_ns(), backward.avg_ns());
    }
}
