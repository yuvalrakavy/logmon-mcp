//! Read-time projections — spec §5.
//!
//! Everything here reads a [`CollectorSnapshot`] and computes; nothing here
//! touches a lock or a live collector. That separation is the point of §3.6:
//! ingest decides as little as possible, and every question a consumer might
//! ask is answered from retained records at read time.
//!
//! The three result categories are not interchangeable views of one number.
//! `exact` covers every matched span for as long as the collector lives.
//! `estimated` covers the same population with a bounded relative error.
//! `sampled` is *exact over the records it retained*, which is the whole
//! population only while `complete` is true. A consumer that treats them as
//! redundant will pick the wrong one exactly when they disagree, which is when
//! it matters.

use crate::collector::exact::{ns_to_ms, ExactStats};
use crate::collector::intern::{Interner, OVERFLOW_LABEL};
use crate::collector::sample::SampleSnapshot;
use crate::collector::sketch::ALPHA;
use crate::collector::state::CollectorSnapshot;
use crate::receiver::TraceIngestLoss;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::{
    ProfileEstimated, ProfileExact, ProfileGroup, ProfileIngest, ProfileResult, ProfileSampled,
    ProfileWindow, Suppressed,
};
use std::collections::HashMap;

/// The percentiles every profile reports, as fractions.
pub const PERCENTILES: [f64; 4] = [0.50, 0.80, 0.95, 0.99];

/// How many retained durations a `sampled` block will list inline.
///
/// The cap exists because the list is unbounded otherwise — a busy collector
/// retains tens of thousands of records, and a reader who wants those wants an
/// export, not a profile response. Below it, at the sizes where percentiles of
/// three values say nothing useful, the raw durations are the only thing that
/// answers "are these two arms actually separated?".
pub const MAX_INLINE_DURATIONS: u64 = 50;

/// Ancestor-walk depth cap for call paths (§5.4). Beyond this the path is
/// reported truncated rather than walked further — a cap plus the visited set
/// is what makes a malformed parent cycle terminate.
pub const MAX_PATH_DEPTH: usize = 64;

/// What a profile breaks down by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// No breakdown; the overall figures only.
    None,
    /// Span name.
    Name,
    /// The collector's declared `group_keys` tuple.
    Group,
    /// Trace id. Tree level only.
    Trace,
    /// Call path within the matched set. Tree level only.
    Path,
}

impl GroupBy {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupBy::None => "none",
            GroupBy::Name => "name",
            GroupBy::Group => "group",
            GroupBy::Trace => "trace",
            GroupBy::Path => "path",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" | "" => GroupBy::None,
            "name" => GroupBy::Name,
            "group" => GroupBy::Group,
            "trace" => GroupBy::Trace,
            "path" => GroupBy::Path,
            _ => return None,
        })
    }

    /// Whether the rows come from the sample tier rather than the exact tier.
    /// Trace and path rows have no exact counterpart: the exact tier keeps no
    /// per-trace or per-path aggregate, by design — those are unbounded axes.
    fn is_sample_derived(self) -> bool {
        matches!(self, GroupBy::Trace | GroupBy::Path)
    }
}

/// Everything a caller can vary at read time.
#[derive(Debug, Clone)]
pub struct ProfileOptions {
    pub group_by: GroupBy,
    /// Drop samples starting within N ms of the earliest matched start (§5.5).
    pub skip_warmup_ms: Option<f64>,
    /// Rows returned, ranked by total time descending.
    pub top_n: usize,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        Self {
            group_by: GroupBy::None,
            skip_warmup_ms: None,
            top_n: 20,
        }
    }
}

/// Where the ingest-loss figures come from, and whether they can be trusted.
///
/// The collector holds the domain's `Arc<ReceiverMetrics>` from the moment it
/// was armed. If the domain has since been deleted and recreated, its counters
/// restarted at zero and a delta against the old baseline is meaningless — so
/// the identity check is pointer equality against the domain's *current*
/// metrics, not a name comparison.
#[derive(Debug, Clone, Copy)]
pub enum IngestBasis {
    /// Baseline and current reading from the same counters.
    Same {
        baseline: TraceIngestLoss,
        current: TraceIngestLoss,
    },
    /// The pinned domain is gone, or was replaced under the collector.
    Unavailable,
    /// There is no window to attribute loss to — an ad-hoc query over spans
    /// that arrived before it was asked.
    ///
    /// Distinct from `Unavailable` because the reasons read as claims about
    /// the user's system. Telling someone their domain "is gone or was
    /// recreated" when they simply ran a one-off profile is a false statement,
    /// and the whole point of the suppression channel is that a reader can
    /// trust what it says.
    NoWindow,
}

/// Build the profile.
pub fn profile(
    snap: &CollectorSnapshot,
    ingest: IngestBasis,
    opts: &ProfileOptions,
    read_at: DateTime<Utc>,
) -> ProfileResult {
    let mut suppressed: Vec<Suppressed> = Vec::new();
    let level = snap.def.level;

    // Warm-up cuts the sample tier only. Both unwindowed tiers must then be
    // withheld rather than reported beside a windowed one: a reader comparing
    // `exact.avg_ms` against `sampled.p50_ms` would be comparing two different
    // populations and nothing in the shape would say so.
    let warmup = opts.skip_warmup_ms.filter(|ms| *ms > 0.0);
    let cut_ns = warmup.and_then(|ms| warmup_cutoff_ns(&snap.samples, ms));

    // Counted ONCE, here, off the same `samples.records()` that every filtering
    // view walks (`sampled`, `groups: trace`, `groups: path`). Counting inside
    // each view would let three numbers describing one filter disagree, and a
    // reader has no way to tell which of them the headline figures used.
    //
    // `None` when no cut was requested — never `Some(0)`. Absent says the
    // filter did not run; zero says it ran and removed nothing. Reporting the
    // second for the first is the failure this field exists to prevent.
    let excluded_by_warmup = warmup.map(|_| match cut_ns {
        Some(c) => snap.samples.records().filter(|r| r.start_ns < c).count() as u64,
        None => 0,
    });
    if warmup.is_some() && cut_ns.is_none() {
        suppressed.push(Suppressed {
            field: "excluded_by_warmup".into(),
            reason: "the cut is measured from the earliest retained span, and this \
                     window retains none — so the zero means there was nothing to \
                     cut, not that the warm-up period was empty"
                .into(),
            remedy: Some("read a window that retained samples, at level `timing` or `tree`".into()),
        });
    }

    let exact = if warmup.is_some() {
        suppressed.push(Suppressed {
            field: "exact".into(),
            reason: "the exact tier is unwindowed, so it still covers the warm-up \
                     period that was excluded from the sample tier"
                .into(),
            remedy: Some(
                "read without skip_warmup_ms, or reset the collector after warm-up".into(),
            ),
        });
        None
    } else {
        Some(exact_view(&snap.total))
    };
    let estimated = if warmup.is_some() {
        suppressed.push(Suppressed {
            field: "estimated".into(),
            reason: "the sketch is unwindowed, for the same reason as `exact`".into(),
            remedy: Some("use `sampled` percentiles, which honour the warm-up cut".into()),
        });
        None
    } else {
        Some(estimated_view(&snap.total, "collector"))
    };

    let sampled = if level.has_samples() {
        Some(sampled_view(
            &snap.samples,
            cut_ns,
            level.has_tree(),
            &mut suppressed,
        ))
    } else {
        suppressed.push(Suppressed {
            field: "sampled".into(),
            reason: format!("level `{}` retains no per-span records", level.as_str()),
            remedy: Some("define the collector at level `timing` or `tree`".into()),
        });
        None
    };

    let nesting = if !level.has_tree() {
        // Not "undetected": the question was never asked. Reporting the same
        // word for "we looked and found none" and "we cannot look" would let a
        // reader conclude a flat call structure from a retention setting.
        "unknown"
    } else if sampled.as_ref().is_some_and(|s| s.nested_matches > 0) {
        "detected"
    } else {
        "undetected"
    };

    let (grouped_by, groups, groups_total) = if opts.group_by == GroupBy::None {
        (None, Vec::new(), 0)
    } else {
        let (rows, total) = group_rows(snap, opts, cut_ns, &mut suppressed);
        (Some(opts.group_by.as_str().to_string()), rows, total)
    };

    ProfileResult {
        collector: Some(snap.def.name.clone()),
        // The live window, not a recorded one.
        snapshot: None,
        description: snap.def.description.clone(),
        filter: snap.def.filter_string.clone(),
        level: level.as_str().to_string(),
        matched: snap.total.count,
        nesting: nesting.to_string(),
        window: window_view(snap, read_at),
        ingest: ingest_view(snap, ingest, &mut suppressed),
        exact,
        estimated,
        sampled,
        grouped_by,
        groups,
        groups_total,
        excluded_by_warmup,
        cardinality_capped: snap.cardinality_capped(),
        suppressed,
        // Filled by the RPC layer for `traces.profile`, whose filter has no
        // arm-time moment at which to report admission warnings. A collector
        // reports them from `collectors.add` instead, which is when the caller
        // can still cheaply change their mind.
        warnings: Vec::new(),
        // Also filled by the RPC layer, and only for a live read: the rolling
        // window is state on the `Collector`, which this function does not see —
        // it projects a `CollectorSnapshot`, and a snapshot has no rolling
        // window by design.
        threshold: None,
    }
}

/// Everything a snapshot must project at the moment it is taken.
///
/// One bundle rather than several arguments, because they share one reason for
/// existing: the per-span records these are derived from are not retained
/// (§6.3), so a projection not taken here can never be taken later. Adding a
/// projection to the snapshot means adding a field here, which is the point —
/// the alternative is discovering months later that the number you want was
/// computable exactly once and nobody computed it.
#[derive(Debug, Clone, Default)]
pub struct Projected {
    pub sampled: Option<ProfileSampled>,
    pub paths: Vec<logmon_broker_protocol::PathRow>,
    pub paths_truncated: bool,
}

/// Project everything a snapshot records, outside the collector lock.
pub fn project_for_snapshot(snap: &CollectorSnapshot) -> Projected {
    let (paths, paths_truncated) = project_paths(snap);
    Projected {
        sampled: project_samples(snap),
        paths,
        paths_truncated,
    }
}

/// Just the sample-derived figures, for a snapshot to record at the moment it
/// is taken.
///
/// A snapshot does not retain the raw samples — at 64 MiB a collector, fifty
/// of them would be a different product — so a projection not computed here
/// can never be computed later. Suppression reasons are discarded rather than
/// surfaced: the snapshot's own `level` and `nested_matches` already say why a
/// field is absent, and a stored reason would go stale against a collector
/// whose definition later changed.
pub fn project_samples(snap: &CollectorSnapshot) -> Option<ProfileSampled> {
    let level = snap.def.level;
    if !level.has_samples() {
        return None;
    }
    let mut discard = Vec::new();
    Some(sampled_view(
        &snap.samples,
        None,
        level.has_tree(),
        &mut discard,
    ))
}

fn window_view(snap: &CollectorSnapshot, read_at: DateTime<Utc>) -> ProfileWindow {
    let start = snap.window_start();
    let wall_ms = (read_at - start).num_microseconds().unwrap_or(0) as f64 / 1000.0;
    ProfileWindow {
        armed_at: Some(snap.armed_at),
        zeroed_at: snap.zeroed_at,
        read_at: Some(read_at),
        // Never negative: a clock that went backwards between arming and
        // reading should read as a zero-length window, not a negative one.
        wall_ms: wall_ms.max(0.0),
    }
}

fn ingest_view(
    snap: &CollectorSnapshot,
    basis: IngestBasis,
    suppressed: &mut Vec<Suppressed>,
) -> Option<ProfileIngest> {
    match basis {
        IngestBasis::Unavailable => {
            suppressed.push(Suppressed {
                field: "ingest".into(),
                reason: "the pinned domain is gone or was recreated, so its transport \
                         counters no longer share an origin with this collector's baseline"
                    .into(),
                remedy: None,
            });
            None
        }
        IngestBasis::NoWindow => {
            suppressed.push(Suppressed {
                field: "ingest".into(),
                reason: "span loss is measured as a delta across a collector's window, and \
                         an ad-hoc profile has no window — these spans arrived before it \
                         was asked for"
                    .into(),
                remedy: Some(
                    "arm a collector before the run if you need to know whether spans went \
                     missing while it was measured"
                        .into(),
                ),
            });
            None
        }
        IngestBasis::Same { baseline, current } => {
            let d = current.since(baseline);
            Some(ProfileIngest {
                drops_in_window: d.dropped,
                shed_batches: d.shed_batches,
                malformed_dropped: d.malformed,
                malformed_timestamps: snap.total.malformed_timestamps,
                negative_duration_spans: snap.total.negative_duration_spans,
                // Stated, not implied. The counters are per-domain and
                // unfiltered, so most of what they count may be spans this
                // filter would never have matched. Sound as a trigger for
                // distrusting `matched`; wrong as a count of lost matches.
                attribution: "domain".into(),
            })
        }
    }
}

fn exact_view(e: &ExactStats) -> ProfileExact {
    ProfileExact {
        count: e.count,
        total_ms: ns_to_ms(e.total_ns),
        avg_ms: e.avg_ns().map(|ns| ns / 1_000_000.0),
        min_ms: e.min_ns.map(|ns| ns as f64 / 1_000_000.0),
        max_ms: e.max_ns.map(|ns| ns as f64 / 1_000_000.0),
        error_count: e.error_count,
        out_of_range_spans: e.out_of_range_spans,
    }
}

fn estimated_view(e: &ExactStats, axis: &str) -> ProfileEstimated {
    let q = |p: f64| e.quantile_ns(p).map(|ns| ns / 1_000_000.0);
    ProfileEstimated {
        axis: axis.to_string(),
        alpha_pct: ALPHA * 100.0,
        p50_ms: q(PERCENTILES[0]),
        p80_ms: q(PERCENTILES[1]),
        p95_ms: q(PERCENTILES[2]),
        p99_ms: q(PERCENTILES[3]),
    }
}

// ---------------------------------------------------------------------------
// Sample-tier projections
// ---------------------------------------------------------------------------

/// One retained span, reduced to what every projection here needs.
#[derive(Debug, Clone, Copy)]
struct Interval {
    start: i64,
    end: i64,
}

impl Interval {
    fn len(self) -> i64 {
        self.end - self.start
    }

    /// This interval confined to `bounds`, or `None` when they do not overlap.
    fn clip(self, bounds: Interval) -> Option<Interval> {
        let start = self.start.max(bounds.start);
        let end = self.end.min(bounds.end);
        (start < end).then_some(Interval { start, end })
    }
}

/// Earliest retained start plus `ms`, or `None` when nothing is retained.
fn warmup_cutoff_ns(samples: &SampleSnapshot, ms: f64) -> Option<i64> {
    // The producer's clock throughout: the origin is the earliest matched
    // start, not the daemon's idea of when the window opened. A run whose
    // spans arrive late would otherwise have its whole warm-up cut missed.
    let min_start = samples.records().map(|r| r.start_ns).min()?;
    // Saturating, because `ms` arrives unclamped from an RPC parameter. A plain
    // `+` overflows on a large value, and the release profile has no overflow
    // checks — so it would wrap to a large NEGATIVE cutoff, which excludes
    // nothing and turns the warm-up cut into a silent no-op. Saturating instead
    // excludes everything: visibly wrong beats invisibly absent.
    let offset_ns = (ms * 1_000_000.0).min(i64::MAX as f64) as i64;
    Some(min_start.saturating_add(offset_ns))
}

fn sampled_view(
    samples: &SampleSnapshot,
    cut_ns: Option<i64>,
    tree: bool,
    suppressed: &mut Vec<Suppressed>,
) -> ProfileSampled {
    let mut durations: Vec<i64> = Vec::new();
    let mut intervals: Vec<(i64, i64)> = Vec::new();
    // (trace_id, span_id) — not span_id alone. Span ids are only required to
    // be unique within a trace, and instrumentation that assigns them
    // sequentially per trace collides constantly across traces, which would
    // graft one trace's children onto another trace's parent.
    let mut by_id: HashMap<(u128, u64), usize> = HashMap::new();
    let mut parents: Vec<u64> = Vec::new();
    let mut traces: Vec<u128> = Vec::new();

    for r in samples.records() {
        if cut_ns.is_some_and(|c| r.start_ns < c) {
            continue;
        }
        let idx = durations.len();
        durations.push(r.end_ns - r.start_ns);
        intervals.push((r.start_ns, r.end_ns));
        if tree {
            by_id.insert((r.trace_id, r.span_id), idx);
            parents.push(r.parent_span_id);
            traces.push(r.trace_id);
        }
    }

    let sample_count = durations.len() as u64;
    let total_ns: i128 = durations.iter().map(|d| *d as i128).sum();
    let wall_union_ns = union_len(&mut intervals);

    let mut sorted = durations.clone();
    sorted.sort_unstable();
    let pct = |q: f64| quantile_sorted(&sorted, q).map(|ns| ns as f64 / 1_000_000.0);

    // Small-n evidence (§2.1). At three records the percentiles are order
    // statistics of three numbers and carry nothing; the durations themselves
    // are what turn "do these arms overlap?" from a judgement by eye into a
    // computation.
    let durations_ms = if !samples.complete {
        suppressed.push(Suppressed {
            field: "sampled.durations_ms".into(),
            reason: "the sample budget stopped retention, so these records are a \
                     contiguous prefix of the run rather than all of it — listing \
                     them would invite per-value reasoning a biased subset cannot \
                     support"
                .into(),
            remedy: Some("raise max_sample_bytes, or narrow the filter".into()),
        });
        None
    } else if sample_count > MAX_INLINE_DURATIONS {
        suppressed.push(Suppressed {
            field: "sampled.durations_ms".into(),
            reason: format!(
                "{sample_count} retained records is above the {MAX_INLINE_DURATIONS}-record \
                 inline cap"
            ),
            remedy: Some(
                "read the percentiles and stddev_ms, which describe the whole retained set".into(),
            ),
        });
        None
    } else {
        Some(durations.iter().map(|d| ns_to_ms(*d as i128)).collect())
    };

    let mut out = ProfileSampled {
        complete: samples.complete,
        sample_count,
        stddev_ms: sample_stddev_ms(&durations),
        durations_ms,
        self_ms: None,
        nested_matches: 0,
        overlapping_child_ms: 0.0,
        overlapping_child_spans: 0,
        wall_union_ms: (sample_count > 0).then(|| ns_to_ms(wall_union_ns)),
        // Deliberately the SAMPLED total over the sampled union, not the exact
        // total. Under truncation the exact tier covers the whole run while
        // the union covers a prefix, and dividing one by the other would
        // report a concurrency figure inflated by exactly the truncation
        // ratio — highest precisely when the sample is least representative.
        achieved_concurrency: (wall_union_ns > 0).then(|| total_ns as f64 / wall_union_ns as f64),
        p50_ms: pct(PERCENTILES[0]),
        p80_ms: pct(PERCENTILES[1]),
        p95_ms: pct(PERCENTILES[2]),
        p99_ms: pct(PERCENTILES[3]),
    };

    if !tree {
        suppressed.push(Suppressed {
            field: "sampled.self_ms".into(),
            reason: "level `timing` retains no parent identity, so children cannot \
                     be attributed"
                .into(),
            remedy: Some("define the collector at level `tree`".into()),
        });
        return out;
    }

    let self_time = self_time(&durations, &intervals, &parents, &traces, &by_id);
    out.nested_matches = self_time.nested_matches;
    out.overlapping_child_ms = ns_to_ms(self_time.clipped_away_ns);
    out.overlapping_child_spans = self_time.clipped_spans;

    if self_time.nested_matches == 0 {
        // Not zero, and not the total either. With nothing matched below any
        // matched span, self time is equal to total time by construction and
        // carries no information at all — reporting the number would invite
        // the reader to conclude the work is unnested when the filter simply
        // did not match the children.
        suppressed.push(Suppressed {
            field: "sampled.self_ms".into(),
            reason: "no matched span has a matched parent, so self time would \
                     equal total time by construction"
                .into(),
            remedy: Some("broaden the filter so nested spans are matched too".into()),
        });
    } else {
        out.self_ms = Some(ns_to_ms(self_time.self_ns));
    }
    out
}

/// Sample standard deviation in milliseconds, Bessel-corrected.
///
/// `None` below two records: with one observation the spread is undefined, and
/// reporting 0.0 would say the opposite — that the measurement was perfectly
/// repeatable.
///
/// Two-pass on purpose. The single-pass `E[x²] - E[x]²` form catastrophically
/// cancels exactly where this field is used: a 86 ms mean with a 0.5 ms spread
/// squares to two nearly equal terms around 7.4e15, and the difference that
/// survives is mostly rounding error.
fn sample_stddev_ms(durations: &[i64]) -> Option<f64> {
    if durations.len() < 2 {
        return None;
    }
    let n = durations.len() as f64;
    let mean = durations.iter().map(|d| *d as f64).sum::<f64>() / n;
    let variance = durations
        .iter()
        .map(|d| {
            let dev = *d as f64 - mean;
            dev * dev
        })
        .sum::<f64>()
        / (n - 1.0);
    Some(variance.sqrt() / 1_000_000.0)
}

struct SelfTime {
    self_ns: i128,
    nested_matches: u64,
    /// Child time that fell outside its parent and was clipped away.
    clipped_away_ns: i128,
    clipped_spans: u64,
}

/// Self time by clipped interval union (§5.3).
///
/// ```text
/// self = duration − |union( clip(child, parent) for matched children )|
/// ```
///
/// The union is what makes this correct under concurrency: a 100 ms parent
/// with two concurrent 60 ms children has 40 ms of self time. Summing the
/// children instead gives −20, and clamping that at zero reports 0 plus an
/// uninterpretable residue — on exactly the `tokio::spawn` workload this
/// feature exists to measure.
///
/// **Self time cannot come out negative here, and that is a property, not an
/// oversight.** Every clipped child is a subset of its parent's interval, so
/// their union measures at most the parent's duration. What clipping *hides*
/// is the anomaly: a child that started before its parent or outlived it means
/// clock skew or an instrumentation bug. So the clipped-away mass is what gets
/// reported, in a pair — one child off by a second and a thousand off by a
/// millisecond call for opposite remedies.
fn self_time(
    durations: &[i64],
    intervals: &[(i64, i64)],
    parents: &[u64],
    traces: &[u128],
    by_id: &HashMap<(u128, u64), usize>,
) -> SelfTime {
    let mut children: HashMap<usize, Vec<Interval>> = HashMap::new();
    let mut nested_matches = 0u64;
    let mut clipped_away_ns: i128 = 0;
    let mut clipped_spans = 0u64;

    for (i, (&parent_id, &trace)) in parents.iter().zip(traces.iter()).enumerate() {
        if parent_id == 0 {
            continue;
        }
        let Some(&p) = by_id.get(&(trace, parent_id)) else {
            continue;
        };
        // A span that is its own parent would otherwise subtract its whole
        // duration from itself. Malformed, but it arrives over a network — and
        // so does a MUTUAL pair, where each claims the other as its child and
        // the resulting self time matches neither span's duration nor their
        // union. `walk_path` already defends against cycles on this same data
        // with a visited set; this is the same hazard on the same input, so it
        // gets the same treatment at the depth a child lookup can afford.
        if p == i || by_id.get(&(traces[p], parents[p])) == Some(&i) {
            continue;
        }
        nested_matches += 1;
        let child = Interval {
            start: intervals[i].0,
            end: intervals[i].1,
        };
        let bounds = Interval {
            start: intervals[p].0,
            end: intervals[p].1,
        };
        match child.clip(bounds) {
            Some(c) => {
                let lost = child.len() - c.len();
                if lost > 0 {
                    clipped_away_ns += lost as i128;
                    clipped_spans += 1;
                }
                children.entry(p).or_default().push(c);
            }
            None => {
                // Entirely outside its parent: contributes nothing to the
                // union, and every nanosecond of it was clipped.
                clipped_away_ns += child.len() as i128;
                clipped_spans += 1;
            }
        }
    }

    let mut self_ns: i128 = 0;
    for (i, d) in durations.iter().enumerate() {
        let covered = match children.get(&i) {
            None => 0,
            Some(cs) => {
                let mut v: Vec<(i64, i64)> = cs.iter().map(|c| (c.start, c.end)).collect();
                union_len(&mut v)
            }
        };
        // Saturating at zero is unreachable by the argument above; it is here
        // so that a future edit which breaks the clip cannot silently produce
        // a negative total.
        self_ns += (*d as i128 - covered).max(0);
    }

    SelfTime {
        self_ns,
        nested_matches,
        clipped_away_ns,
        clipped_spans,
    }
}

/// Measure of the union of half-open intervals. Sorts in place.
fn union_len(intervals: &mut [(i64, i64)]) -> i128 {
    if intervals.is_empty() {
        return 0;
    }
    intervals.sort_unstable();
    let mut total: i128 = 0;
    let (mut cs, mut ce) = intervals[0];
    for &(s, e) in &intervals[1..] {
        if s <= ce {
            ce = ce.max(e);
        } else {
            total += (ce - cs) as i128;
            cs = s;
            ce = e;
        }
    }
    total + (ce - cs) as i128
}

/// §5.7 — the lower quantile's 0-based index into a sorted slice of `n`, from
/// rank `⌊1 + q(n−1)⌋`, 1-indexed.
///
/// This is the convention DDSketch's accuracy guarantee is stated against, so
/// it is also the convention the sketch's own percentiles follow. Using a
/// different one anywhere would make `estimated` and `sampled` disagree by one
/// order statistic on the same data, which reads as sketch error and is not.
///
/// One function, three callers — the sketch, the sample tier, and the repaired
/// `traces.slow`. Two implementations of a convention is how they drift.
pub fn quantile_index(n: usize, q: f64) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let rank = 1.0 + q * (n as f64 - 1.0);
    Some((rank.floor() as usize).clamp(1, n) - 1)
}

fn quantile_sorted(sorted: &[i64], q: f64) -> Option<i64> {
    quantile_index(sorted.len(), q).map(|i| sorted[i])
}

// ---------------------------------------------------------------------------
// Breakdowns
// ---------------------------------------------------------------------------

fn group_rows(
    snap: &CollectorSnapshot,
    opts: &ProfileOptions,
    cut_ns: Option<i64>,
    suppressed: &mut Vec<Suppressed>,
) -> (Vec<ProfileGroup>, usize) {
    let level = snap.def.level;
    if opts.group_by.is_sample_derived() && !level.has_tree() {
        suppressed.push(Suppressed {
            field: "groups".into(),
            reason: format!(
                "`group_by: {}` needs span identity, which level `{}` does not retain",
                opts.group_by.as_str(),
                level.as_str()
            ),
            remedy: Some("define the collector at level `tree`".into()),
        });
        return (Vec::new(), 0);
    }

    match opts.group_by {
        GroupBy::None => (Vec::new(), 0),
        GroupBy::Name => exact_rows(
            snap.per_name
                .iter()
                .map(|(id, e)| (snap.names.resolve(*id).to_string(), e)),
            "name",
            opts.top_n,
        ),
        GroupBy::Group => {
            if snap.def.group_keys.is_empty() {
                suppressed.push(Suppressed {
                    field: "groups".into(),
                    reason: "this collector declares no group_keys".into(),
                    remedy: Some(
                        "define a collector with group_keys, e.g. [\"cache.enabled\"]".into(),
                    ),
                });
                return (Vec::new(), 0);
            }
            exact_rows(
                snap.per_group
                    .iter()
                    .map(|(ids, e)| (group_label(&snap.group_values, ids), e)),
                "group",
                opts.top_n,
            )
        }
        GroupBy::Trace => trace_rows(&snap.samples, cut_ns, opts.top_n),
        GroupBy::Path => path_rows(snap, cut_ns, opts.top_n),
    }
}

/// Rows straight off the exact tier: both `exact` and `estimated` are real,
/// `sampled` is absent because these axes are aggregated at ingest.
fn exact_rows<'a>(
    rows: impl Iterator<Item = (String, &'a ExactStats)>,
    axis: &str,
    top_n: usize,
) -> (Vec<ProfileGroup>, usize) {
    let mut v: Vec<ProfileGroup> = rows
        .map(|(key, e)| ProfileGroup {
            key,
            exact: Some(exact_view(e)),
            estimated: Some(estimated_view(e, axis)),
            sampled: None,
            path_incomplete: false,
        })
        .collect();
    let total = rank_and_truncate(&mut v, top_n);
    (v, total)
}

/// The declared group keys' values, joined. A single key renders bare so the
/// common case reads as the value itself rather than a one-element tuple.
fn group_label(interners: &[Interner], ids: &[u32]) -> String {
    if ids.len() == 1 {
        return interners
            .first()
            .map(|i| i.resolve(ids[0]).to_string())
            .unwrap_or_else(|| OVERFLOW_LABEL.to_string());
    }
    ids.iter()
        .enumerate()
        .map(|(i, id)| {
            interners
                .get(i)
                .map(|n| n.resolve(*id))
                .unwrap_or(OVERFLOW_LABEL)
        })
        .collect::<Vec<_>>()
        .join(" / ")
}

fn trace_rows(
    samples: &SampleSnapshot,
    cut_ns: Option<i64>,
    top_n: usize,
) -> (Vec<ProfileGroup>, usize) {
    let mut per_trace: HashMap<u128, Vec<i64>> = HashMap::new();
    let mut intervals: HashMap<u128, Vec<(i64, i64)>> = HashMap::new();
    for r in samples.records() {
        if cut_ns.is_some_and(|c| r.start_ns < c) {
            continue;
        }
        per_trace
            .entry(r.trace_id)
            .or_default()
            .push(r.end_ns - r.start_ns);
        intervals
            .entry(r.trace_id)
            .or_default()
            .push((r.start_ns, r.end_ns));
    }

    let mut v: Vec<ProfileGroup> = per_trace
        .into_iter()
        .map(|(trace, mut durations)| {
            durations.sort_unstable();
            let total: i128 = durations.iter().map(|d| *d as i128).sum();
            let union = intervals
                .get_mut(&trace)
                .map(|iv| union_len(iv))
                .unwrap_or(0);
            let pct = |q: f64| quantile_sorted(&durations, q).map(|ns| ns as f64 / 1_000_000.0);
            ProfileGroup {
                key: format!("{trace:032x}"),
                // No per-trace exact aggregate exists, by design: trace id is
                // an unbounded axis, and keeping a sketch per trace is how a
                // bounded collector becomes a leak.
                exact: None,
                estimated: None,
                sampled: Some(ProfileSampled {
                    complete: samples.complete,
                    sample_count: durations.len() as u64,
                    // One number per row is worth carrying; the duration list
                    // is not, because per-row it multiplies by `top_n`.
                    stddev_ms: sample_stddev_ms(&durations),
                    durations_ms: None,
                    self_ms: None,
                    nested_matches: 0,
                    overlapping_child_ms: 0.0,
                    overlapping_child_spans: 0,
                    wall_union_ms: Some(ns_to_ms(union)),
                    achieved_concurrency: (union > 0).then(|| total as f64 / union as f64),
                    p50_ms: pct(PERCENTILES[0]),
                    p80_ms: pct(PERCENTILES[1]),
                    p95_ms: pct(PERCENTILES[2]),
                    p99_ms: pct(PERCENTILES[3]),
                }),
                path_incomplete: false,
            }
        })
        .collect();
    let total = rank_and_truncate(&mut v, top_n);
    (v, total)
}

/// One call path's aggregate — the shared product of the path walk.
///
/// Rendered two ways: as a `ProfileGroup` row for a live read, and as a
/// `PathRow` stored in a snapshot so a flame graph can be produced from a
/// recorded run (§9.8). One walk with two renderings rather than two walks:
/// the second implementation is where they would drift, and a flame graph that
/// disagreed with the profile it came from would be worse than no flame graph.
pub struct PathAggregate {
    /// Matched ancestors, root first, one frame per element. Kept unjoined
    /// because a span named `parse > eval` is indistinguishable from two frames
    /// once ` > ` is a separator, and a flame-graph renderer that splits the
    /// joined form back apart would emit a stack that never ran.
    pub frames: Vec<String>,
    pub self_ns: i128,
    pub count: u64,
    pub incomplete: bool,
}

impl PathAggregate {
    /// The display form, `[?] > a > b` when the chain is a suffix.
    pub fn path(&self) -> String {
        let joined = self.frames.join(" > ");
        if self.incomplete {
            format!("[?] > {joined}")
        } else {
            joined
        }
    }
}

/// Call paths (§5.4): self time aggregated by the chain of matched ancestors,
/// ranked by self time descending then by path so equal rows are stable.
///
/// Paths resolve only where ancestors are matched, so the idiom is a broad
/// filter plus read-time narrowing. A walk that stops at a parent which is not
/// retained yields a suffix, marked `[?]` and `path_incomplete`, rather than a
/// path silently rooted at the wrong place.
pub fn path_aggregates(snap: &CollectorSnapshot, cut_ns: Option<i64>) -> Vec<PathAggregate> {
    let samples = &snap.samples;
    let mut durations: Vec<i64> = Vec::new();
    let mut intervals: Vec<(i64, i64)> = Vec::new();
    let mut parents: Vec<u64> = Vec::new();
    let mut traces: Vec<u128> = Vec::new();
    let mut names: Vec<u32> = Vec::new();
    let mut by_id: HashMap<(u128, u64), usize> = HashMap::new();

    for r in samples.records() {
        if cut_ns.is_some_and(|c| r.start_ns < c) {
            continue;
        }
        let idx = durations.len();
        durations.push(r.end_ns - r.start_ns);
        intervals.push((r.start_ns, r.end_ns));
        by_id.insert((r.trace_id, r.span_id), idx);
        parents.push(r.parent_span_id);
        traces.push(r.trace_id);
        names.push(r.name_id);
    }

    // Per-span self time, so a path's figure is time spent in that path and
    // nothing matched below it — the same quantity a flame graph's flat column
    // reports, and the one that survives being summed across siblings.
    let mut children: HashMap<usize, Vec<(i64, i64)>> = HashMap::new();
    for (i, (&parent_id, &trace)) in parents.iter().zip(traces.iter()).enumerate() {
        if parent_id == 0 {
            continue;
        }
        let Some(&p) = by_id.get(&(trace, parent_id)) else {
            continue;
        };
        if p == i {
            continue;
        }
        let child = Interval {
            start: intervals[i].0,
            end: intervals[i].1,
        };
        let bounds = Interval {
            start: intervals[p].0,
            end: intervals[p].1,
        };
        if let Some(c) = child.clip(bounds) {
            children.entry(p).or_default().push((c.start, c.end));
        }
    }

    struct Row {
        self_ns: i128,
        count: u64,
    }
    // Keyed by (frames, incomplete): a complete chain `a > b` and a suffix of
    // some deeper chain that also ends `a > b` are different paths, and folding
    // them together would attribute an unknown prefix's time to a known root.
    let mut rows: HashMap<(Vec<String>, bool), Row> = HashMap::new();

    for (i, d) in durations.iter().enumerate() {
        let covered = children.get_mut(&i).map(|v| union_len(v)).unwrap_or(0);
        let self_ns = (*d as i128 - covered).max(0);
        let (frames, incomplete) = walk_path(i, &parents, &traces, &names, &by_id, &snap.names);
        let row = rows.entry((frames, incomplete)).or_insert(Row {
            self_ns: 0,
            count: 0,
        });
        row.self_ns += self_ns;
        row.count += 1;
    }

    let mut v: Vec<PathAggregate> = rows
        .into_iter()
        .map(|((frames, incomplete), r)| PathAggregate {
            frames,
            self_ns: r.self_ns,
            count: r.count,
            incomplete,
        })
        .collect();
    // `incomplete` is part of the tiebreak because it is part of the KEY: a
    // complete chain `a > b` and a suffix of some deeper chain that also ends
    // `a > b` are different rows with identical frames. Without it they compare
    // Equal at equal self time, and since `sort_by` is stable, HashMap iteration
    // order decides — so `top_n` and the stored path list would differ between
    // two runs over identical data. Equal self time is not exotic: every parent
    // fully covered by its children contributes zero.
    v.sort_by(|a, b| {
        b.self_ns
            .cmp(&a.self_ns)
            .then_with(|| a.frames.cmp(&b.frames))
            .then_with(|| a.incomplete.cmp(&b.incomplete))
    });
    v
}

fn path_rows(
    snap: &CollectorSnapshot,
    cut_ns: Option<i64>,
    top_n: usize,
) -> (Vec<ProfileGroup>, usize) {
    let complete = snap.samples.complete;
    // Materialised before `take` for the same reason `rank_and_truncate` hands
    // its count back: the denominator has to be the pre-truncation one, and
    // this path truncates with `take` rather than going through that helper.
    let aggregates = path_aggregates(snap, cut_ns);
    let total = aggregates.len();
    let rows = aggregates
        .into_iter()
        .take(top_n)
        .map(|p| ProfileGroup {
            key: p.path(),
            exact: None,
            estimated: None,
            sampled: Some(ProfileSampled {
                complete,
                sample_count: p.count,
                // A path aggregate carries a count and a self-time sum, not the
                // per-span durations either figure would need.
                stddev_ms: None,
                durations_ms: None,
                self_ms: Some(ns_to_ms(p.self_ns)),
                nested_matches: 0,
                overlapping_child_ms: 0.0,
                overlapping_child_spans: 0,
                wall_union_ms: None,
                achieved_concurrency: None,
                p50_ms: None,
                p80_ms: None,
                p95_ms: None,
                p99_ms: None,
            }),
            path_incomplete: p.incomplete,
        })
        .collect();
    (rows, total)
}

/// Rows retained in a snapshot's stored path list (§9.8's `folded` format).
///
/// Two caps, because either alone leaks: a row count bounds a pathological
/// fan-out, and a byte budget bounds pathologically *long* paths — a `tree`
/// collector can walk 64 ancestors deep, so 200 rows of that shape is a
/// different order of cost from 200 rows of `handler > db`.
pub const SNAPSHOT_PATH_ROWS: usize = 200;
pub const SNAPSHOT_PATH_BYTES: usize = 64 * 1024;

/// The path list a snapshot stores, plus whether it was cut short.
///
/// Computed at snapshot time or never: the samples it walks are not retained,
/// so a flame graph not taken here cannot be taken later. Truncation is
/// returned rather than inferred from the row count — a flame graph rendered
/// from a partial list is missing mass, and that is not something a reader can
/// see by looking at it.
pub fn project_paths(snap: &CollectorSnapshot) -> (Vec<logmon_broker_protocol::PathRow>, bool) {
    if !snap.def.level.has_tree() {
        return (Vec::new(), false);
    }
    let all = path_aggregates(snap, None);
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for p in all.iter().take(SNAPSHOT_PATH_ROWS) {
        bytes += p.frames.iter().map(String::len).sum::<usize>();
        if bytes > SNAPSHOT_PATH_BYTES && !out.is_empty() {
            break;
        }
        out.push(logmon_broker_protocol::PathRow {
            frames: p.frames.clone(),
            self_ms: ns_to_ms(p.self_ns),
            count: p.count,
            incomplete: p.incomplete,
        });
    }
    let truncated = out.len() < all.len();
    (out, truncated)
}

/// Walk matched ancestors, root first. Returns the frames and whether the chain
/// is a suffix rather than complete.
fn walk_path(
    start: usize,
    parents: &[u64],
    traces: &[u128],
    names: &[u32],
    by_id: &HashMap<(u128, u64), usize>,
    interner: &Interner,
) -> (Vec<String>, bool) {
    let mut chain: Vec<String> = vec![interner.resolve(names[start]).to_string()];
    let mut visited: Vec<usize> = vec![start];
    let mut cur = start;
    let mut incomplete = false;

    loop {
        let parent_id = parents[cur];
        if parent_id == 0 {
            break; // a real root: the chain is complete
        }
        let Some(&p) = by_id.get(&(traces[cur], parent_id)) else {
            // A parent that exists upstream but is not in the matched set —
            // either the filter excluded it or truncation cut it. Either way
            // this chain is a suffix and must not read as rooted.
            incomplete = true;
            break;
        };
        // A cycle would otherwise walk until the depth cap and render a path
        // that repeats. Both guards are needed: the visited set catches the
        // cycle, the cap catches a chain that is merely absurdly deep.
        if visited.contains(&p) || visited.len() >= MAX_PATH_DEPTH {
            incomplete = true;
            break;
        }
        chain.push(interner.resolve(names[p]).to_string());
        visited.push(p);
        cur = p;
    }

    chain.reverse();
    (chain, incomplete)
}

/// Rank by total time descending, then by key so equal rows are stable across
/// reads. Truncation is silent in the row list, so `top_n` belongs beside a
/// count the caller can compare against `matched`.
/// Returns the row count **before** truncation — the denominator a reader needs
/// to tell "the top 20 of 20" from "the top 20 of 900". Taken here rather than
/// recomputed by the caller so the count and the cut can never describe
/// different collections.
fn rank_and_truncate(v: &mut Vec<ProfileGroup>, top_n: usize) -> usize {
    v.sort_by(|a, b| {
        let ta = a.exact.as_ref().map(|e| e.total_ms).unwrap_or(0.0);
        let tb = b.exact.as_ref().map(|e| e.total_ms).unwrap_or(0.0);
        tb.total_cmp(&ta).then_with(|| a.key.cmp(&b.key))
    });
    let total = v.len();
    v.truncate(top_n);
    total
}

#[cfg(test)]
mod tests;
