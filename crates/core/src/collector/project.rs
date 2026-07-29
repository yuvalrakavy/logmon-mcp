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

    let (grouped_by, groups) = if opts.group_by == GroupBy::None {
        (None, Vec::new())
    } else {
        (
            Some(opts.group_by.as_str().to_string()),
            group_rows(snap, opts, cut_ns, &mut suppressed),
        )
    };

    ProfileResult {
        collector: Some(snap.def.name.clone()),
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
        cardinality_capped: snap.cardinality_capped(),
        suppressed,
        // Filled by the RPC layer for `traces.profile`, whose filter has no
        // arm-time moment at which to report admission warnings. A collector
        // reports them from `collectors.add` instead, which is when the caller
        // can still cheaply change their mind.
        warnings: Vec::new(),
    }
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
    Some(min_start + (ms * 1_000_000.0) as i64)
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

    let mut out = ProfileSampled {
        complete: samples.complete,
        sample_count,
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
        // duration from itself. Malformed, but it arrives over a network.
        if p == i {
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
) -> Vec<ProfileGroup> {
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
        return Vec::new();
    }

    match opts.group_by {
        GroupBy::None => Vec::new(),
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
                return Vec::new();
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
) -> Vec<ProfileGroup> {
    let mut v: Vec<ProfileGroup> = rows
        .map(|(key, e)| ProfileGroup {
            key,
            exact: Some(exact_view(e)),
            estimated: Some(estimated_view(e, axis)),
            sampled: None,
            path_incomplete: false,
        })
        .collect();
    rank_and_truncate(&mut v, top_n);
    v
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

fn trace_rows(samples: &SampleSnapshot, cut_ns: Option<i64>, top_n: usize) -> Vec<ProfileGroup> {
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
    rank_and_truncate(&mut v, top_n);
    v
}

/// Call paths (§5.4): self time aggregated by the chain of matched ancestors.
///
/// Paths resolve only where ancestors are matched, so the idiom is a broad
/// filter plus read-time narrowing. A walk that stops at a parent which is not
/// retained yields a suffix, marked `[?]` and `path_incomplete`, rather than a
/// path silently rooted at the wrong place.
fn path_rows(snap: &CollectorSnapshot, cut_ns: Option<i64>, top_n: usize) -> Vec<ProfileGroup> {
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
        incomplete: bool,
    }
    let mut rows: HashMap<String, Row> = HashMap::new();

    for (i, d) in durations.iter().enumerate() {
        let covered = children.get_mut(&i).map(|v| union_len(v)).unwrap_or(0);
        let self_ns = (*d as i128 - covered).max(0);
        let (path, incomplete) = walk_path(i, &parents, &traces, &names, &by_id, &snap.names);
        let row = rows.entry(path).or_insert(Row {
            self_ns: 0,
            count: 0,
            incomplete: false,
        });
        row.self_ns += self_ns;
        row.count += 1;
        row.incomplete |= incomplete;
    }

    let mut v: Vec<ProfileGroup> = rows
        .into_iter()
        .map(|(key, r)| ProfileGroup {
            key,
            exact: None,
            estimated: None,
            sampled: Some(ProfileSampled {
                complete: samples.complete,
                sample_count: r.count,
                self_ms: Some(ns_to_ms(r.self_ns)),
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
            path_incomplete: r.incomplete,
        })
        .collect();
    v.sort_by(|a, b| {
        let sa = a.sampled.as_ref().and_then(|s| s.self_ms).unwrap_or(0.0);
        let sb = b.sampled.as_ref().and_then(|s| s.self_ms).unwrap_or(0.0);
        sb.total_cmp(&sa).then_with(|| a.key.cmp(&b.key))
    });
    v.truncate(top_n);
    v
}

/// Walk matched ancestors, root first. Returns the rendered path and whether
/// it is a suffix rather than a complete chain.
fn walk_path(
    start: usize,
    parents: &[u64],
    traces: &[u128],
    names: &[u32],
    by_id: &HashMap<(u128, u64), usize>,
    interner: &Interner,
) -> (String, bool) {
    let mut chain: Vec<&str> = vec![interner.resolve(names[start])];
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
        chain.push(interner.resolve(names[p]));
        visited.push(p);
        cur = p;
    }

    chain.reverse();
    let mut path = chain.join(" > ");
    if incomplete {
        path = format!("[?] > {path}");
    }
    (path, incomplete)
}

/// Rank by total time descending, then by key so equal rows are stable across
/// reads. Truncation is silent in the row list, so `top_n` belongs beside a
/// count the caller can compare against `matched`.
fn rank_and_truncate(v: &mut Vec<ProfileGroup>, top_n: usize) {
    v.sort_by(|a, b| {
        let ta = a.exact.as_ref().map(|e| e.total_ms).unwrap_or(0.0);
        let tb = b.exact.as_ref().map(|e| e.total_ms).unwrap_or(0.0);
        tb.total_cmp(&ta).then_with(|| a.key.cmp(&b.key))
    });
    v.truncate(top_n);
}

#[cfg(test)]
mod tests;
