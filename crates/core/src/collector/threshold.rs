//! Threshold triggers — spec §8.
//!
//! A load-time guard: "tell me if the average reconcile goes over 200 ms across
//! any five-second window". Evaluated against a **rolling bucket ring advanced
//! by span arrival**, not by a timer — which is what keeps the zero-CPU-at-idle
//! contract the rest of this design rests on. An idle collector does no work
//! here at all, because nothing calls in.
//!
//! **Stated rather than discovered: with no traffic, a breached threshold
//! neither fires nor clears.** The ring only advances when a span arrives, so a
//! collector whose traffic stops holds whatever verdict it last reached. That is
//! the honest behaviour for a load-time guard and the wrong behaviour for a
//! liveness check, so it is written down here and reported in the result rather
//! than left for someone to infer from a stuck `breached: true`.
//!
//! Nothing is reused from the log-side trigger machinery (§8). Its debounce is
//! the session storage window, its counter measures *entries*, and span
//! thresholds are not debounced at all — three different meanings for names that
//! look the same.

use crate::collector::exact::Duration;
use std::sync::atomic::{AtomicU64, Ordering};

/// Buckets in the ring. The window is divided into this many slots, so the
/// evaluated window is accurate to within one slot of the declared `window_ms`.
///
/// Sixteen is a deliberate middle: the error is at most 1/16 of the window, and
/// the whole ring is 16 × 24 bytes, which fits in six cache lines and is read in
/// full on every evaluation.
pub const BUCKETS: usize = 16;

/// The smallest window worth evaluating. Below this a single bucket is under a
/// millisecond and the ring is measuring scheduling jitter.
pub const MIN_WINDOW_MS: u64 = 16;

/// The largest. Beyond this the guard is no longer about load — and the ring
/// holds no timestamps, so a window measured in hours would report a breach from
/// work that finished long ago.
pub const MAX_WINDOW_MS: u64 = 600_000;

/// What a threshold watches.
///
/// Every one of these is a sum or a count over the window, computable from three
/// per-bucket integers. **Percentiles are deliberately absent**: a rolling
/// percentile needs a sketch per bucket, and a sketch per bucket per collector
/// is how a design with a bounded per-collector memory budget stops having one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Matched spans in the window.
    Count,
    /// Summed matched duration in the window. Work, not elapsed time.
    TotalMs,
    /// `total_ms / count` over the window.
    AvgMs,
    /// Matched spans whose status was an error.
    ErrorCount,
    /// `error_count / count`, as a percentage.
    ErrorRatePct,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::Count => "count",
            Metric::TotalMs => "total_ms",
            Metric::AvgMs => "avg_ms",
            Metric::ErrorCount => "error_count",
            Metric::ErrorRatePct => "error_rate_pct",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "count" => Metric::Count,
            "total_ms" => Metric::TotalMs,
            "avg_ms" => Metric::AvgMs,
            "error_count" => Metric::ErrorCount,
            "error_rate_pct" => Metric::ErrorRatePct,
            // Named explicitly, because "why not p95" is the first thing anyone
            // asks and the answer is a design constraint rather than an
            // oversight.
            "p50_ms" | "p80_ms" | "p95_ms" | "p99_ms" | "p50" | "p95" | "p99" => {
                return Err(format!(
                    "`{s}` cannot be a threshold metric: a rolling percentile needs a \
                     duration sketch per bucket, and this design's per-collector memory is \
                     bounded precisely so that a collector cannot grow without limit. Use \
                     `avg_ms` for a rolling guard, and read the real percentiles with \
                     collectors.get"
                ))
            }
            other => {
                return Err(format!(
                    "unknown threshold metric `{other}`: expected `count`, `total_ms`, \
                     `avg_ms`, `error_count` or `error_rate_pct`"
                ))
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Gt => "gt",
            Op::Gte => "gte",
            Op::Lt => "lt",
            Op::Lte => "lte",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Ok(match s {
            "gt" | ">" => Op::Gt,
            "gte" | ">=" => Op::Gte,
            "lt" | "<" => Op::Lt,
            "lte" | "<=" => Op::Lte,
            other => {
                return Err(format!(
                    "unknown threshold op `{other}`: expected `gt`, `gte`, `lt` or `lte`"
                ))
            }
        })
    }

    fn holds(self, value: f64, limit: f64) -> bool {
        match self {
            Op::Gt => value > limit,
            Op::Gte => value >= limit,
            Op::Lt => value < limit,
            Op::Lte => value <= limit,
        }
    }

    /// Whether this direction can only be satisfied by an *absence* of traffic.
    ///
    /// `count < 10` is true of an idle collector, but the ring never advances
    /// while idle — so the threshold will not fire, which is the opposite of
    /// what someone writing it probably expects. Warned about at arm time rather
    /// than silently never firing.
    fn is_downward(self) -> bool {
        matches!(self, Op::Lt | Op::Lte)
    }
}

/// A threshold as declared.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    pub metric: Metric,
    /// Restrict to spans in one group value. `None` watches every matched span.
    pub group: Option<String>,
    pub op: Op,
    pub value: f64,
    pub window_ms: u64,
}

impl Threshold {
    /// Admission, on the same terms as everything else here: loud and specific.
    pub fn validate(&self, group_keys: &[String]) -> Result<Vec<String>, String> {
        if !self.value.is_finite() {
            return Err(format!(
                "threshold value must be finite, got `{}`: every comparison against NaN is \
                 false and an infinite limit is either always or never breached",
                self.value
            ));
        }
        if !(MIN_WINDOW_MS..=MAX_WINDOW_MS).contains(&self.window_ms) {
            return Err(format!(
                "window_ms {} is outside {MIN_WINDOW_MS}..={MAX_WINDOW_MS}: below that a \
                 bucket is under a millisecond and the ring measures scheduling jitter, and \
                 above it the ring holds no timestamps so a breach could come from work \
                 that finished long ago",
                self.window_ms
            ));
        }
        if self.group.is_some() && group_keys.is_empty() {
            return Err(
                "a threshold names a `group`, but this collector declares no group_keys, so \
                 there is nothing to match it against. Add group_keys, or drop `group`"
                    .to_string(),
            );
        }
        if matches!(self.metric, Metric::ErrorRatePct) && !(0.0..=100.0).contains(&self.value) {
            return Err(format!(
                "`error_rate_pct` is a percentage, so a limit of {} can never be crossed \
                 from inside 0..=100",
                self.value
            ));
        }

        let mut warnings = Vec::new();
        if self.op.is_downward() {
            // Legal, and the single most likely misunderstanding of this
            // feature. Marked rather than refused: "did the throughput drop
            // while still under load" is a real question, and this answers it.
            warnings.push(format!(
                "`{} {} {}` is a downward threshold, and the rolling window only advances \
                 when a span arrives — so it detects a drop *while traffic continues*, and \
                 will NOT fire if traffic stops altogether. A threshold is a load-time \
                 guard, not a liveness check",
                self.metric.as_str(),
                self.op.as_str(),
                self.value
            ));
        }
        if matches!(self.metric, Metric::TotalMs) {
            warnings.push(
                "`total_ms` over a window is summed work, not elapsed time: under N-way \
                 concurrency it can exceed the window length, so a limit below \
                 `window_ms × concurrency` may be breached by a perfectly healthy system"
                    .to_string(),
            );
        }
        Ok(warnings)
    }
}

/// One bucket's accumulation. Three integers, so a bucket is 24 bytes and the
/// whole ring is read on every evaluation without touching more than a handful
/// of cache lines.
#[derive(Debug, Default)]
struct Bucket {
    /// The ring index this bucket currently represents. Stored so a stale
    /// bucket can be recognised and skipped without clearing it eagerly.
    epoch: AtomicU64,
    count: AtomicU64,
    total_ns: AtomicU64,
    errors: AtomicU64,
}

/// The rolling evaluation state.
///
/// Every field is an atomic and every operation takes `&self`, because ingest
/// holds the collector's read lock and must not need a write one — §3.6's whole
/// point is that the lock which gates ingest is never held for anything but the
/// shortest possible update.
pub struct ThresholdState {
    def: Threshold,
    bucket_ms: u64,
    ring: Vec<Bucket>,
    /// The newest epoch any span has reached. Monotonic.
    head: AtomicU64,
    /// Whether the last evaluation found the threshold breached.
    breached: AtomicU64,
    /// Epoch at which it most recently *became* breached, for reporting.
    breached_at_epoch: AtomicU64,
    /// How many times it has transitioned from clear to breached. A count of
    /// **transitions**, not of evaluations: a threshold breached for a whole run
    /// has fired once, which is what someone asking "did this happen" means.
    fires: AtomicU64,
    /// The value at the most recent evaluation, so a caller can see how close
    /// it is rather than only whether it crossed.
    last_value_milli: AtomicU64,
    last_value_known: AtomicU64,
}

impl ThresholdState {
    pub fn new(def: Threshold) -> Self {
        let bucket_ms = (def.window_ms / BUCKETS as u64).max(1);
        Self {
            def,
            bucket_ms,
            ring: (0..BUCKETS).map(|_| Bucket::default()).collect(),
            head: AtomicU64::new(0),
            breached: AtomicU64::new(0),
            breached_at_epoch: AtomicU64::new(0),
            fires: AtomicU64::new(0),
            last_value_milli: AtomicU64::new(0),
            last_value_known: AtomicU64::new(0),
        }
    }

    pub fn def(&self) -> &Threshold {
        &self.def
    }

    /// Record one matched span and re-evaluate.
    ///
    /// `start_ms` is the producer's clock, the same one every other projection
    /// uses — not the daemon's idea of now. A run whose spans arrive in a burst
    /// after the fact is then evaluated over the window they *happened* in, and
    /// a threshold that fired during the run still reads as fired.
    pub fn record(&self, start_ms: i64, d: Duration, is_error: bool, group: Option<&str>) {
        if let Some(want) = &self.def.group {
            match group {
                Some(g) if g == want => {}
                // Not in the watched group: it did not happen, for this
                // threshold's purposes. Notably this does NOT advance the ring,
                // so a collector matching a million spans in other groups does
                // not age this threshold's window out.
                _ => return,
            }
        }

        let epoch = self.epoch_of(start_ms);
        // A span older than the window is dropped rather than folded into the
        // oldest bucket: attributing late-arriving work to the current window
        // would report a breach that the window did not contain.
        let head = self.head.fetch_max(epoch, Ordering::Relaxed).max(epoch);
        if epoch + BUCKETS as u64 <= head {
            return;
        }

        let slot = &self.ring[(epoch % BUCKETS as u64) as usize];
        // Claim the slot for this epoch if it is stale, resetting it. Two
        // threads can race here; the loser's `compare_exchange` fails and it
        // simply adds to the slot the winner just cleared, which is correct.
        let prev = slot.epoch.load(Ordering::Relaxed);
        if prev != epoch
            && slot
                .epoch
                .compare_exchange(prev, epoch, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            slot.count.store(0, Ordering::Relaxed);
            slot.total_ns.store(0, Ordering::Relaxed);
            slot.errors.store(0, Ordering::Relaxed);
        }

        slot.count.fetch_add(1, Ordering::Relaxed);
        if is_error {
            slot.errors.fetch_add(1, Ordering::Relaxed);
        }
        if let Duration::Ok(ns) = d {
            slot.total_ns.fetch_add(ns as u64, Ordering::Relaxed);
        }

        self.evaluate(head);
    }

    fn epoch_of(&self, start_ms: i64) -> u64 {
        // Negative producer clocks are not worth a branch downstream: clamp to
        // zero, which puts them in the oldest possible window and lets the
        // staleness check above drop them.
        (start_ms.max(0) as u64) / self.bucket_ms
    }

    /// Sum the live buckets and compare.
    fn evaluate(&self, head: u64) {
        let oldest = head.saturating_sub(BUCKETS as u64 - 1);
        let mut count = 0u64;
        let mut total_ns = 0u64;
        let mut errors = 0u64;
        for b in &self.ring {
            let e = b.epoch.load(Ordering::Relaxed);
            // A bucket outside the window is skipped rather than cleared, so an
            // evaluation never writes to a slot another thread may be adding to.
            if e < oldest || e > head {
                continue;
            }
            count += b.count.load(Ordering::Relaxed);
            total_ns += b.total_ns.load(Ordering::Relaxed);
            errors += b.errors.load(Ordering::Relaxed);
        }

        let value = match self.def.metric {
            Metric::Count => Some(count as f64),
            Metric::TotalMs => Some(total_ns as f64 / 1e6),
            // An average over nothing is not zero, and reporting it as zero
            // would make `avg_ms < 5` fire on an empty window.
            Metric::AvgMs => (count > 0).then(|| total_ns as f64 / count as f64 / 1e6),
            Metric::ErrorCount => Some(errors as f64),
            Metric::ErrorRatePct => (count > 0).then(|| errors as f64 / count as f64 * 100.0),
        };
        let Some(value) = value else {
            // Undefined, so neither breached nor cleared. Holding the previous
            // verdict is the same choice the no-traffic case makes, for the same
            // reason: this is a load-time guard, and with no load there is
            // nothing to guard.
            return;
        };

        self.last_value_milli
            .store((value * 1000.0).max(0.0) as u64, Ordering::Relaxed);
        self.last_value_known.store(1, Ordering::Relaxed);

        let now_breached = self.def.op.holds(value, self.def.value);
        let was = self.breached.swap(now_breached as u64, Ordering::Relaxed) == 1;
        if now_breached && !was {
            self.fires.fetch_add(1, Ordering::Relaxed);
            self.breached_at_epoch.store(head, Ordering::Relaxed);
        }
    }

    /// The current verdict, for `collectors.list` and `collectors.get`.
    pub fn report(&self) -> ThresholdReport {
        let known = self.last_value_known.load(Ordering::Relaxed) == 1;
        ThresholdReport {
            metric: self.def.metric.as_str(),
            group: self.def.group.clone(),
            op: self.def.op.as_str(),
            value: self.def.value,
            window_ms: self.def.window_ms,
            breached: self.breached.load(Ordering::Relaxed) == 1,
            fires: self.fires.load(Ordering::Relaxed),
            last_value: known
                .then(|| self.last_value_milli.load(Ordering::Relaxed) as f64 / 1000.0),
            evaluated: known,
            note: NOTE,
        }
    }
}

/// Carried with every report, because it is the one thing about this mechanism
/// that surprises people, and a stuck `breached: true` on a finished run is
/// otherwise indistinguishable from a bug.
pub const NOTE: &str = "the window advances on span arrival, not on a clock — so with no \
                        traffic a breached threshold neither fires nor clears, and the \
                        verdict below is the last one reached while spans were arriving";

#[derive(Debug, Clone)]
pub struct ThresholdReport {
    pub metric: &'static str,
    pub group: Option<String>,
    pub op: &'static str,
    pub value: f64,
    pub window_ms: u64,
    pub breached: bool,
    /// Clear-to-breached transitions, not evaluations.
    pub fires: u64,
    /// The value at the last evaluation. `None` until one has happened —
    /// **unknown, not zero**, because zero is a value `count` can legitimately
    /// hold and `lt` thresholds would read it as a breach.
    pub last_value: Option<f64>,
    pub evaluated: bool,
    pub note: &'static str,
}

#[cfg(test)]
mod tests;
