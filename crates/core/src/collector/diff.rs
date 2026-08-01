//! Comparison between two arms — spec §5.6.
//!
//! The driving workflow ends here. Arm a collector, run the suite, snapshot,
//! change one thing, run again, snapshot — and then ask what moved. Everything
//! before this point measures; this is the only place logmon subtracts.
//!
//! **Subtraction is where a measurement design earns or loses its credibility.**
//! Two numbers can each be individually sound and their difference still be
//! meaningless: the arms measured different populations, one lost spans the
//! other did not, one is a truncated prefix, the percentiles came from sketches
//! with different bucket layouts. So this module's real content is not the
//! arithmetic — it is the table of conditions under which it refuses to do the
//! arithmetic at all, and the marks it attaches when it proceeds anyway.
//!
//! Three severities, and the distinction is the point (§5.6):
//!
//! - **Mark** — proceed, and say what differs. A level difference, a regex
//!   spelled differently but compiling identically, one truncated arm.
//! - **Block** — refuse, but name a flag that permits it. A genuine filter
//!   difference, asymmetric ingest loss, both arms truncated. These are cases
//!   where the caller may know something the daemon cannot.
//! - **Refuse** — no override. A sketch layout mismatch is arithmetically
//!   wrong and provable from a recorded fact, so there is nothing to permit.

use crate::collector::exact::{ns_to_ms, ExactStats};
use crate::collector::history::RunToRunFloor;
use crate::collector::history::StoredSnapshot;
use crate::collector::intern::{Interner, OVERFLOW_LABEL};
use crate::collector::project::PERCENTILES;
use crate::collector::sample::Level;
use crate::collector::sketch::{SketchLayout, ALPHA};
use crate::collector::state::{CollectorDef, CollectorSnapshot};
use crate::filter::parser::{DurationOp, ParsedFilter, Pattern, Qualifier, Selector};
use crate::receiver::TraceIngestLoss;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::ProfileSampled;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// How much of an exact `count` difference makes an `avg_ms` delta invalid as
/// an effect size (§9.5).
///
/// `avg = total / count`, so when the denominator moves the delta stops being
/// "each one got cheaper" and becomes a blend of that and "there were fewer of
/// them". Counts are exact, so there is no measurement noise to absorb — but a
/// single span's difference across a million is not what the rule is aimed at,
/// and suppressing on it would train the reader to ignore the suppression.
/// One percent is the line: above it, the population changed.
pub const MATERIAL_COUNT_CHANGE_PCT: f64 = 1.0;

/// What a diff breaks down by. A subset of [`crate::collector::project::GroupBy`]:
/// only the exact-tier axes can be compared.
///
/// `trace` and `path` are absent by construction rather than by omission. Both
/// are projected from the retained per-span records, and a snapshot does not
/// keep those — so for a recorded run those rows never existed and can never be
/// reconstructed. Offering an axis that silently works on live windows and
/// silently fails on snapshots would be worse than not offering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffGroupBy {
    None,
    Name,
    Group,
}

impl DiffGroupBy {
    /// Every axis a diff can group by. See [`GroupBy::ALL`] — same contract:
    /// one enumeration, read by the rejection message and by the schema
    /// agreement test alike.
    ///
    /// [`GroupBy::ALL`]: crate::collector::project::GroupBy::ALL
    pub const ALL: [DiffGroupBy; 3] = [DiffGroupBy::None, DiffGroupBy::Name, DiffGroupBy::Group];

    pub fn as_str(self) -> &'static str {
        match self {
            DiffGroupBy::None => "none",
            DiffGroupBy::Name => "name",
            DiffGroupBy::Group => "group",
        }
    }

    /// What a rejection should say it expected.
    pub fn accepted() -> String {
        crate::rejection::one_of(&Self::ALL.map(Self::as_str))
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" | "" => DiffGroupBy::None,
            "name" => DiffGroupBy::Name,
            "group" => DiffGroupBy::Group,
            _ => return None,
        })
    }
}

/// Which runs an arm covers, and how they were combined (§9.4).
///
/// Always stated, never inferred. A reader looking at a single figure cannot
/// otherwise tell whether it is one run or the mean of five — and the run-to-run
/// floor, which decides whether any delta is real, is computable only in the
/// second case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aggregation {
    /// The collector's current, still-open window.
    Live,
    /// Exactly one recorded run.
    SingleRun(String),
    /// Several recorded runs, exact tiers and sketches merged (§6.5).
    Merged(Vec<String>),
}

impl Aggregation {
    pub fn as_str(&self) -> String {
        match self {
            Aggregation::Live => "live window (single run, still open)".into(),
            Aggregation::SingleRun(l) => format!("single run: {l}"),
            Aggregation::Merged(labels) => {
                format!("merged across {} runs: {}", labels.len(), labels.join(", "))
            }
        }
    }

    /// A single run has no spread, so nothing here can tell a real change from
    /// scheduling noise (§6.5). Reported, because it is the single most common
    /// reason a confident-looking delta is not one.
    pub fn is_single_run(&self) -> bool {
        !matches!(self, Aggregation::Merged(l) if l.len() > 1)
    }
}

/// One side of a comparison, assembled by the caller from a live window or from
/// recorded runs.
///
/// Everything a diff needs to *decide* is in here, not just what it needs to
/// subtract: the definition as of the measurement, whether spans were lost,
/// whether the sample truncated, the sketch layout. §6.3 exists so that these
/// come from the recorded run rather than from the live collector — reading the
/// current definition and presenting it as proof of what a past run measured is
/// wrong in exactly the case a comparison exists to survive.
pub struct Arm {
    /// How the caller named it — `hot`, `hot@baseline`, `hot@*`.
    pub spec: String,
    pub collector: String,
    pub aggregation: Aggregation,
    pub def: Arc<CollectorDef>,
    pub total: ExactStats,
    /// Breakdowns keyed by their **resolved label**, not by interner id.
    ///
    /// Resolved at construction because a merged arm spans several snapshots and
    /// each carries its own interner — the same id means different names in two
    /// of them. Keeping ids here would make the merge silently graft one run's
    /// `db.query` onto another's, which is the kind of defect that produces a
    /// plausible number and no error.
    ///
    /// `None` means the breakdown was not recorded, which is distinct from an
    /// empty map: a restored snapshot drops per-axis maps on the way to disk, and
    /// an empty map would read as "this run matched nothing under any name".
    pub per_name: Option<HashMap<String, ExactStats>>,
    pub per_group: Option<HashMap<String, ExactStats>>,
    pub projections: Option<ProfileSampled>,
    /// `detected`, `undetected`, or `unknown` — whether any matched span sat
    /// under another matched span, which decides whether `total_ms` counts the
    /// inner time twice.
    ///
    /// **Carried explicitly rather than derived from `projections`, because a
    /// merged arm has none.** Deriving it meant every multi-run arm read
    /// `unknown` and every merged diff advised "re-run at `tree`" — to someone
    /// whose runs were already at `tree`. A remedy that cannot be acted on
    /// teaches the reader to ignore the remedies that can.
    pub nesting: &'static str,
    /// Span loss across the window. `None` is **unknown**, not zero.
    pub ingest: Option<TraceIngestLoss>,
    pub sample_complete: bool,
    pub cardinality_capped: bool,
    pub wall_ms: f64,
    pub window_start: Option<DateTime<Utc>>,
    pub taken_at: Option<DateTime<Utc>>,
    pub meta: serde_json::Value,
    pub description: Option<String>,
    /// Top call paths by self time, for a flame graph (§9.8). Empty for a live
    /// arm — a live read projects them fresh — and for a merged one.
    pub paths: Vec<logmon_broker_protocol::PathRow>,
    pub paths_truncated: bool,
    /// Spread of `total_ms` across the runs this arm merges. `None` for a
    /// single run.
    pub floor: Option<RunToRunFloor>,
    /// Spread of `count`, kept separately **because variance is per-metric**.
    ///
    /// Transferring the duration spread onto a count row over-suppresses badly:
    /// a suite that runs a fixed number of iterations has near-zero count
    /// variance, so a 1.6% duration CV would strike out a real five-span
    /// difference in three hundred and report it as noise.
    pub count_floor: Option<RunToRunFloor>,
}

/// Merge one axis across snapshots, keyed by resolved label.
///
/// Sketches refuse a layout mismatch on merge, and a key whose merge fails is
/// dropped rather than silently reported with one run's numbers — the caller
/// already knows the merge succeeded overall, so a per-key failure here can only
/// be a layout mismatch that `merge` would have caught for the total.
fn merge_by_label<'a, F>(snapshots: &'a [StoredSnapshot], extract: F) -> HashMap<String, ExactStats>
where
    F: Fn(&'a StoredSnapshot) -> Option<Vec<(String, &'a ExactStats)>>,
{
    let mut out: HashMap<String, ExactStats> = HashMap::new();
    for s in snapshots {
        for (key, stats) in extract(s).into_iter().flatten() {
            match out.get_mut(&key) {
                None => {
                    out.insert(key, stats.clone());
                }
                Some(acc) => {
                    if acc.merge(stats).is_err() {
                        out.remove(&key);
                    }
                }
            }
        }
    }
    out
}

/// Shallow, like `StoredSnapshot`'s and for the same reason: an arm holds
/// interners and per-axis maps, so a derived `Debug` would turn one failed
/// `expect_err` in a test into pages of resolved strings.
impl std::fmt::Debug for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arm")
            .field("spec", &self.spec)
            .field("aggregation", &self.aggregation)
            .field("level", &self.def.level)
            .field("count", &self.total.count)
            .field("sample_complete", &self.sample_complete)
            .finish()
    }
}

impl std::fmt::Debug for DiffResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffResult")
            .field("a", &self.a)
            .field("b", &self.b)
            .field("level", &self.level)
            .field("trustworthy", &self.trustworthy)
            .field("rows", &self.rows.len())
            .field("groups", &self.groups.len())
            .field("marks", &self.marks)
            .finish()
    }
}

/// `detected` / `undetected` / `unknown`, on the same terms a profile uses.
///
/// `unknown` and `undetected` are not synonyms, and the difference decides
/// whether `total_ms` can be trusted: `undetected` means we looked and found no
/// nesting, `unknown` means the retention level could not look.
fn nesting_of(p: Option<&ProfileSampled>, level: Level) -> &'static str {
    match (p, level.has_tree()) {
        (Some(p), true) if p.nested_matches > 0 => "detected",
        (Some(_), true) => "undetected",
        _ => "unknown",
    }
}

impl Arm {
    /// An arm over a collector's still-open window.
    pub fn from_live(
        spec: String,
        view: &CollectorSnapshot,
        ingest: Option<TraceIngestLoss>,
        read_at: DateTime<Utc>,
    ) -> Self {
        let window_start = view.window_start();
        let wall_ms = (read_at - window_start).num_microseconds().unwrap_or(0) as f64 / 1000.0;
        // Projected once: it walks every retained record, which at the sample
        // cap is millions of them.
        let projections = crate::collector::project::project_samples(view);
        let nesting = nesting_of(projections.as_ref(), view.def.level);
        // Projected here, not left empty. A live arm was previously given no
        // paths on the reasoning that "a live read projects them fresh" — true
        // of `traces.profile`, false of `document --format folded`, which reads
        // this field and never projects. So `folded` on a live collector failed
        // with a message blaming the run for retaining nothing.
        let (paths, paths_truncated) = crate::collector::project::project_paths(view);
        Self {
            spec,
            collector: view.def.name.clone(),
            aggregation: Aggregation::Live,
            def: view.def.clone(),
            total: view.total.clone(),
            per_name: Some(
                view.per_name
                    .iter()
                    .map(|(id, e)| (view.names.resolve(*id).to_string(), e.clone()))
                    .collect(),
            ),
            per_group: Some(
                view.per_group
                    .iter()
                    .map(|(ids, e)| (group_label(&view.group_values, ids), e.clone()))
                    .collect(),
            ),
            projections,
            nesting,
            ingest,
            sample_complete: view.samples.complete,
            cardinality_capped: view.cardinality_capped(),
            wall_ms: wall_ms.max(0.0),
            window_start: Some(window_start),
            taken_at: Some(read_at),
            meta: serde_json::Value::Null,
            description: view.def.description.clone(),
            paths,
            paths_truncated,
            floor: None,
            count_floor: None,
        }
    }

    /// An arm over one recorded run.
    pub fn from_snapshot(spec: String, s: &StoredSnapshot) -> Self {
        Self {
            spec,
            collector: s.def.name.clone(),
            aggregation: Aggregation::SingleRun(s.label.clone()),
            def: s.def.clone(),
            total: s.total.clone(),
            per_name: s.per_name.as_ref().map(|m| {
                m.iter()
                    .map(|(id, e)| (s.names.resolve(*id).to_string(), e.clone()))
                    .collect()
            }),
            per_group: s.per_group.as_ref().map(|m| {
                m.iter()
                    .map(|(ids, e)| (group_label(&s.group_values, ids), e.clone()))
                    .collect()
            }),
            projections: s.projections.clone(),
            nesting: nesting_of(s.projections.as_ref(), s.def.level),
            ingest: s.ingest,
            sample_complete: s.sample_complete,
            cardinality_capped: s.cardinality_capped,
            wall_ms: s.wall_ms,
            window_start: Some(s.window_start),
            taken_at: Some(s.taken_at),
            meta: s.meta.clone(),
            description: s.description.clone(),
            paths: s.paths.clone(),
            paths_truncated: s.paths_truncated,
            floor: None,
            count_floor: None,
        }
    }

    /// An arm over several recorded runs, combined (§6.5).
    ///
    /// This is the only arm shape that yields a run-to-run floor, and therefore
    /// the only one whose deltas can be told apart from scheduling noise. Which
    /// is why it exists: with single-run arms on both sides, every threshold in
    /// the result reads "unknown".
    ///
    /// Three things are *not* combined, each for its own reason:
    ///
    /// - **Sample-derived projections.** Self time across two runs is not a self
    ///   time and a wall union across them is not a union. Omitted, not summed.
    /// - **Per-axis breakdowns, unless every run recorded them.** Merging only
    ///   the runs that have them would produce per-name rows that do not sum to
    ///   the merged total — breaking the one reconciliation rule the design has
    ///   (A11) in a way no reader could detect.
    /// - **Ingest loss**, which is added rather than merged: two windows that
    ///   each lost spans lost them both, so the sum is the honest figure.
    ///
    /// **Refuses runs recorded under different definitions.** §7.1 keeps history
    /// across a structural edit, so a collector's history legitimately spans
    /// several configurations — and combining them would sum two populations,
    /// fold one filter's `db.query` into another's, union two group dimensions
    /// under one label, and report the spread across configurations as
    /// "scheduling variance". An arm is one measurement of one population; that
    /// is what makes `RunToRunFloor` mean anything.
    pub fn merged(
        spec: String,
        snapshots: &[StoredSnapshot],
    ) -> Result<Self, crate::collector::history::MergeError> {
        use crate::collector::history::{merge, MergeError};
        let last = snapshots.last().ok_or(MergeError::Empty)?;

        // Checked here because nothing else can: `merge` compares sketch
        // LAYOUTS, which are compile-time constants and therefore identical for
        // every snapshot in a process whatever its filter; and `diff`'s filter,
        // level and group_keys checks are cross-ARM, never intra-arm.
        for s in snapshots {
            let differs = canonical_filter(&s.def.filter) != canonical_filter(&last.def.filter)
                || s.def.level != last.def.level
                || s.def.group_keys != last.def.group_keys;
            if differs {
                return Err(MergeError::DefinitionMismatch {
                    a: format!(
                        "`{}`: filter `{}`, level `{}`, group_keys {:?}",
                        s.label,
                        s.def.filter_string,
                        s.def.level.as_str(),
                        s.def.group_keys
                    ),
                    b: format!(
                        "`{}`: filter `{}`, level `{}`, group_keys {:?}",
                        last.label,
                        last.def.filter_string,
                        last.def.level.as_str(),
                        last.def.group_keys
                    ),
                });
            }
        }

        let total = merge(snapshots)?;
        let labels: Vec<String> = snapshots.iter().map(|s| s.label.clone()).collect();

        // Every run or none: a partial breakdown cannot reconcile to the total.
        let all_named = snapshots.iter().all(|s| s.per_name.is_some());
        let all_grouped = snapshots.iter().all(|s| s.per_group.is_some());
        let per_name = all_named.then(|| {
            merge_by_label(snapshots, |s| {
                s.per_name.as_ref().map(|m| {
                    m.iter()
                        .map(|(id, e)| (s.names.resolve(*id).to_string(), e))
                        .collect::<Vec<_>>()
                })
            })
        });
        let per_group = all_grouped.then(|| {
            merge_by_label(snapshots, |s| {
                s.per_group.as_ref().map(|m| {
                    m.iter()
                        .map(|(ids, e)| (group_label(&s.group_values, ids), e))
                        .collect::<Vec<_>>()
                })
            })
        });

        // Loss is unknown for the merged arm if it was unknown for any run: one
        // window whose attribution failed is enough to make the total a floor
        // rather than a figure, and a floor presented as a figure is worse than
        // no number.
        let ingest = snapshots
            .iter()
            .try_fold(TraceIngestLoss::default(), |acc, s| {
                s.ingest.map(|i| TraceIngestLoss {
                    dropped: acc.dropped.saturating_add(i.dropped),
                    shed_batches: acc.shed_batches.saturating_add(i.shed_batches),
                    malformed: acc.malformed.saturating_add(i.malformed),
                })
            });

        Ok(Self {
            spec,
            collector: last.def.name.clone(),
            aggregation: Aggregation::Merged(labels),
            // Every run's definition, because the loop above proved they
            // agree on everything that decides what was measured.
            def: last.def.clone(),
            total,
            per_name,
            per_group,
            // Not merged: see above.
            projections: None,
            // The verdict survives even though the projections do not, and the
            // fold is by severity: `detected` anywhere means the merged
            // `total_ms` double-counts, and one run that could not tell makes
            // the answer unknown rather than clean.
            nesting: snapshots
                .iter()
                .map(|s| nesting_of(s.projections.as_ref(), s.def.level))
                .fold("undetected", |acc, n| match (acc, n) {
                    ("detected", _) | (_, "detected") => "detected",
                    ("unknown", _) | (_, "unknown") => "unknown",
                    _ => "undetected",
                }),
            ingest,
            // Truncation anywhere truncates the merged arm: the combined sample
            // is then a set of prefixes, which is not a sample of the whole.
            sample_complete: snapshots.iter().all(|s| s.sample_complete),
            cardinality_capped: snapshots.iter().any(|s| s.cardinality_capped),
            wall_ms: snapshots.iter().map(|s| s.wall_ms).sum(),
            window_start: snapshots.first().map(|s| s.window_start),
            taken_at: Some(last.taken_at),
            meta: last.meta.clone(),
            description: last.description.clone(),
            // Path lists do not merge: a path's self time in run 1 and run 2 are
            // two measurements, not one longer one, and summing them would make
            // a flame graph whose total matches no run that happened.
            paths: Vec::new(),
            paths_truncated: false,
            floor: RunToRunFloor::over_total_ms(snapshots),
            count_floor: RunToRunFloor::over_count(snapshots),
        })
    }

    pub fn layout(&self) -> SketchLayout {
        self.total.sketch().layout()
    }

    /// Whether this arm is a truncated prefix of its run.
    ///
    /// A `scalar` collector retains no samples at all and reports `complete:
    /// true` — an absent tier is not a truncated one, which is what makes §5.6's
    /// "scalar collectors are explicitly permitted" fall out rather than need a
    /// special case here.
    pub fn truncated(&self) -> bool {
        !self.sample_complete
    }

    /// Whether spans were lost before the collector saw them. `None` when the
    /// question cannot be answered — the pinned domain is gone, or the arm is
    /// a live window on recreated counters.
    pub fn lost_spans(&self) -> Option<bool> {
        self.ingest
            .map(|i| i.dropped > 0 || i.shed_batches > 0 || i.malformed > 0)
    }
}

/// A condition that differs between the arms. Attached to the result, never
/// silently absorbed.
#[derive(Debug, Clone)]
pub struct Mark {
    /// `level`, `filter`, `group_keys`, `ingest_loss`, `truncated`, `nesting`,
    /// `cardinality`, `collector`.
    pub kind: String,
    pub detail: String,
    /// The parameter that permitted this, when it was a blocking condition the
    /// caller overrode. Absent for conditions that only ever mark.
    pub overridden_by: Option<String>,
}

impl Mark {
    /// Whether this mark changes how a reader reads the *labels* rather than
    /// whether the *numbers* are sound.
    ///
    /// Two collectors being compared is worth saying and does not make the
    /// arithmetic wrong. Neither does a filter difference that is only a
    /// difference in spelling — the populations are provably the same, which is
    /// how it came to be a mark rather than a block. Everything else means some
    /// number below is not safe to act on.
    fn is_informational(&self) -> bool {
        match self.kind.as_str() {
            "collector" => true,
            // An *overridden* filter mismatch is a real population difference
            // the caller waved through; a spelling-only one is not.
            "filter" => self.overridden_by.is_none(),
            _ => false,
        }
    }
}

/// A refusal. Each carries the flag that would permit it, or states that none
/// exists.
#[derive(Debug, Clone)]
pub enum DiffError {
    /// Percentiles from sketches built with different parameters. No override:
    /// the subtraction is wrong, and the mismatch is proven by a recorded fact
    /// rather than guessed at.
    LayoutMismatch {
        a: String,
        b: String,
    },
    FilterMismatch {
        a: String,
        b: String,
    },
    /// One arm lost spans and the other did not.
    AsymmetricLoss {
        a: String,
        b: String,
    },
    BothTruncated,
}

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffError::LayoutMismatch { a, b } => write!(
                f,
                "cannot diff runs recorded with different sketch layouts ({a} vs {b}): the \
                 percentile buckets do not line up, so a difference between them is \
                 arithmetic on two different scales. There is no flag for this — the \
                 numbers would not mean anything. Re-record both arms on one build"
            ),
            DiffError::FilterMismatch { a, b } => write!(
                f,
                "the arms matched different span populations, so a difference between them \
                 is not an effect of the change under test (`{a}` vs `{b}`). Pass \
                 allow_mismatch: true if the filters are deliberately different and you \
                 want the delta anyway"
            ),
            DiffError::AsymmetricLoss { a, b } => write!(
                f,
                "one arm lost spans before the collector saw them and the other did not \
                 ({a} vs {b}), so `matched` under-counts on one side only and every tier \
                 is affected — including the exact ones. A delta here would measure the \
                 loss as much as the change. Reduce load or raise the channel and re-run; \
                 pass allow_lossy: true to compare anyway"
            ),
            DiffError::BothTruncated => write!(
                f,
                "both arms hit the sample budget, so each retained a contiguous prefix of \
                 its own run — and a faster arm reaches the cap later in wall-clock terms, \
                 so the two prefixes cover different slices of work. Raise \
                 max_sample_bytes or lower the level and re-run; pass allow_truncated: \
                 true to compare anyway, which affects only the sample-derived rows \
                 (`self_ms` and the `sampled` percentiles) — `count`, `total_ms` and the \
                 `estimated` percentiles are unaffected either way"
            ),
        }
    }
}

impl std::error::Error for DiffError {}

/// What the caller may permit (§5.6's block column).
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffAllowances {
    pub mismatch: bool,
    pub lossy: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct DiffOptions {
    pub group_by: DiffGroupBy,
    pub top_n: usize,
    pub allow: DiffAllowances,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            group_by: DiffGroupBy::None,
            top_n: 20,
            allow: DiffAllowances::default(),
        }
    }
}

/// Which tier a row's numbers came from. Never mixed within a row: `exact` and
/// `sampled` can differ by 40 % under truncation, and a row that took one value
/// from each would be uninterpretable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Exact,
    Estimated,
    Sampled,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Estimated => "estimated",
            Tier::Sampled => "sampled",
        }
    }
}

/// Which rule set the row's suppression threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdBasis {
    /// Spread across repeats of one configuration (§6.5).
    RunToRun,
    /// §5.6's `α(a+b)/|a−b|`, expressed as a minimum resolvable change.
    MeasurementResolution,
    /// Both applied; this is the larger.
    Binding,
    /// No threshold could be computed — single-run arms on an exact metric.
    Unknown,
}

impl ThresholdBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            ThresholdBasis::RunToRun => "run-to-run",
            ThresholdBasis::MeasurementResolution => "measurement-resolution",
            ThresholdBasis::Binding => "run-to-run and measurement-resolution",
            ThresholdBasis::Unknown => "unknown",
        }
    }
}

/// One metric's delta, with everything needed to decide whether to believe it.
#[derive(Debug, Clone)]
pub struct Row {
    pub metric: &'static str,
    pub tier: Tier,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub delta: Option<f64>,
    /// Change from `a` to `b`, as a percentage of `a`. `None` when `a` is zero.
    pub delta_pct: Option<f64>,
    /// **The threshold, in the metric's own units.** A delta at or below this
    /// is suppressed, and this is the number printed beside it — §6.5 forbids
    /// striking a delta through using a bound other than the displayed one.
    pub threshold_abs: Option<f64>,
    /// The same threshold as a percentage of the two values' mean.
    pub threshold_pct: Option<f64>,
    pub threshold_basis: ThresholdBasis,
    /// §5.6's `α(a+b)/|a−b|` — the worst-case error on this delta as a
    /// percentage **of the delta**. `estimated` rows only.
    ///
    /// Not a second threshold. `error_bound_pct >= 100` is algebraically the
    /// same condition as `|delta| <= threshold_abs` when the measurement floor
    /// binds, so this is a second *presentation* of one rule.
    pub error_bound_pct: Option<f64>,
    /// Below the printed threshold: present, but not evidence of a change.
    pub suppressed: bool,
    pub trustworthy: bool,
    pub notes: Vec<String>,
}

impl Row {
    fn absent(metric: &'static str, tier: Tier, reason: String) -> Self {
        Self {
            metric,
            tier,
            a: None,
            b: None,
            delta: None,
            delta_pct: None,
            threshold_abs: None,
            threshold_pct: None,
            threshold_basis: ThresholdBasis::Unknown,
            error_bound_pct: None,
            suppressed: false,
            trustworthy: false,
            notes: vec![reason],
        }
    }
}

/// One breakdown key's rows.
#[derive(Debug, Clone)]
pub struct GroupDiff {
    pub key: String,
    /// `both`, `a_only`, or `b_only`.
    pub presence: &'static str,
    pub rows: Vec<Row>,
}

/// The whole comparison.
pub struct DiffResult {
    pub a: Arm,
    pub b: Arm,
    /// `min(level)` — the level both arms can answer at (§5.6).
    pub level: Level,
    /// False when any mark makes a number unsafe to act on. A reader who reads
    /// nothing else should still not be misled.
    pub trustworthy: bool,
    pub rows: Vec<Row>,
    pub grouped_by: DiffGroupBy,
    pub groups: Vec<GroupDiff>,
    /// Comparable keys **before** `top_n` truncation. Without it a renderer can
    /// only say "top 3 of 3" when it truncated 3 out of 87, and a reader adds up
    /// the visible rows and concludes they are the whole change.
    pub groups_total: usize,
    pub marks: Vec<Mark>,
    /// Group rows dropped because one side folded into `__overflow__`.
    pub overflow_rows_suppressed: u64,
    /// Rows that could not be produced at all, with the reason.
    pub suppressed: Vec<logmon_broker_protocol::Suppressed>,
}

/// Compare two arms.
///
/// The order of operations is deliberate: every refusal is decided **before**
/// any arithmetic. A function that computed rows and then discovered the arms
/// were incomparable would have to remember to throw the rows away, and the
/// version of it that forgets is indistinguishable from the version that works
/// until the day the arms differ.
pub fn diff(a: Arm, b: Arm, opts: &DiffOptions) -> Result<DiffResult, DiffError> {
    let mut marks: Vec<Mark> = Vec::new();
    let mut suppressed: Vec<logmon_broker_protocol::Suppressed> = Vec::new();

    // --- Refuse: sketch layout. No override (§5.6). ------------------------
    if a.layout() != b.layout() {
        return Err(DiffError::LayoutMismatch {
            a: describe_layout(&a.layout()),
            b: describe_layout(&b.layout()),
        });
    }

    // --- Block: a genuine filter difference. -------------------------------
    let (fa, fb) = (
        canonical_filter(&a.def.filter),
        canonical_filter(&b.def.filter),
    );
    if fa != fb {
        if !opts.allow.mismatch {
            return Err(DiffError::FilterMismatch {
                a: a.def.filter_string.clone(),
                b: b.def.filter_string.clone(),
            });
        }
        marks.push(Mark {
            kind: "filter".into(),
            detail: format!(
                "the arms matched different span populations: `{}` vs `{}`. Any delta below \
                 includes the effect of that difference",
                a.def.filter_string, b.def.filter_string
            ),
            overridden_by: Some("allow_mismatch".into()),
        });
    } else if a.def.filter_string != b.def.filter_string {
        // Same meaning, different spelling. Marked rather than hidden: two
        // regexes that compile to the same matcher canonicalize equal here, and
        // a reader comparing the two filter strings by eye would otherwise
        // think the tool had missed a difference.
        marks.push(Mark {
            kind: "filter".into(),
            detail: format!(
                "the filter strings differ in spelling but parse to the same matcher \
                 (`{}` vs `{}`), so the populations are the same",
                a.def.filter_string, b.def.filter_string
            ),
            overridden_by: None,
        });
    }

    // --- Block: ingest loss, and only when the asymmetry is PROVEN. --------
    //
    // `None` is unknown, not zero. An arm whose pinned domain was deleted and
    // recreated cannot say whether it lost spans — and refusing on unknown
    // would make the restart-then-re-pin workflow §7.1 exists to support
    // undiffable. So unknown marks (loudly, `trustworthy: false`) and proven
    // asymmetry blocks.
    match (a.lost_spans(), b.lost_spans()) {
        (Some(true), Some(false)) | (Some(false), Some(true)) => {
            if !opts.allow.lossy {
                return Err(DiffError::AsymmetricLoss {
                    a: describe_loss(a.ingest),
                    b: describe_loss(b.ingest),
                });
            }
            marks.push(Mark {
                kind: "ingest_loss".into(),
                detail: format!(
                    "spans were lost on one arm only ({} vs {}); `matched` under-counts on \
                     that side and every tier is affected",
                    describe_loss(a.ingest),
                    describe_loss(b.ingest)
                ),
                overridden_by: Some("allow_lossy".into()),
            });
        }
        (Some(true), Some(true)) => marks.push(Mark {
            kind: "ingest_loss".into(),
            detail: format!(
                "both arms lost spans ({} vs {}); `matched` under-counts on both sides, so \
                 the delta is between two under-counts",
                describe_loss(a.ingest),
                describe_loss(b.ingest)
            ),
            overridden_by: None,
        }),
        (Some(false), Some(false)) => {}
        _ => marks.push(Mark {
            kind: "ingest_loss".into(),
            detail: format!(
                "span loss is unknown for at least one arm ({} vs {}), so an asymmetry \
                 cannot be ruled out",
                describe_loss_opt(&a),
                describe_loss_opt(&b)
            ),
            overridden_by: None,
        }),
    }

    // --- Block: both arms truncated. ---------------------------------------
    if a.truncated() && b.truncated() {
        if !opts.allow.truncated {
            return Err(DiffError::BothTruncated);
        }
        marks.push(Mark {
            kind: "truncated".into(),
            detail: "both arms hit the sample budget, so each covers a prefix of its own \
                     run and the two prefixes are different slices of work. Affects \
                     `self_ms` and the `sampled` percentiles only"
                .into(),
            overridden_by: Some("allow_truncated".into()),
        });
    } else if a.truncated() || b.truncated() {
        marks.push(Mark {
            kind: "truncated".into(),
            detail: format!(
                "arm `{}` hit the sample budget, so its sample-derived rows cover a prefix \
                 of the run rather than all of it. `count`, `total_ms` and the `estimated` \
                 percentiles are unaffected",
                if a.truncated() { &a.spec } else { &b.spec }
            ),
            overridden_by: None,
        });
    }

    // --- Mark: everything else that differs. -------------------------------
    let level = a.def.level.min(b.def.level);
    if a.def.level != b.def.level {
        marks.push(Mark {
            kind: "level".into(),
            detail: format!(
                "the arms were recorded at different levels (`{}` vs `{}`); comparing at \
                 `{}`, which is what both can answer",
                a.def.level.as_str(),
                b.def.level.as_str(),
                level.as_str()
            ),
            overridden_by: None,
        });
    }
    if a.def.group_keys != b.def.group_keys {
        marks.push(Mark {
            kind: "group_keys".into(),
            detail: format!(
                "the arms declare different group_keys ({:?} vs {:?}), so their group \
                 tuples are not the same thing",
                a.def.group_keys, b.def.group_keys
            ),
            overridden_by: None,
        });
    }
    if a.collector != b.collector {
        marks.push(Mark {
            kind: "collector".into(),
            detail: format!(
                "these are two different collectors (`{}` and `{}`), not two runs of one",
                a.collector, b.collector
            ),
            overridden_by: None,
        });
    }
    if a.cardinality_capped || b.cardinality_capped {
        marks.push(Mark {
            kind: "cardinality".into(),
            detail: "a cardinality cap folded values into `__overflow__` on at least one \
                     arm. Those rows are never compared, and a key absent from one arm may \
                     have been folded rather than unseen"
                .into(),
            overridden_by: None,
        });
    }

    // Nesting evidence survives the level reduction (§5.6). Dropping it with
    // the level is how an earlier revision reintroduced the round-1 headline
    // defect: `total_ms` double-counts when a matched span sits under another
    // matched span, and comparing at `timing` does not make that stop being
    // true of the recorded numbers.
    let nested_either = a.nesting == "detected" || b.nesting == "detected";
    let nesting_unknown = a.nesting == "unknown" || b.nesting == "unknown";
    if nested_either {
        marks.push(Mark {
            kind: "nesting".into(),
            detail: "at least one arm has matched spans nested under other matched spans, \
                     so its `total_ms` counts the inner time twice. Quote `self_ms`"
                .into(),
            overridden_by: None,
        });
    } else if nesting_unknown {
        marks.push(Mark {
            kind: "nesting".into(),
            detail: "at least one arm was recorded below level `tree`, so whether its \
                     `total_ms` double-counts nested work is not knowable from what was \
                     retained. Re-run at `tree` to find out"
                .into(),
            overridden_by: None,
        });
    }

    let total_ms_untrustworthy = nested_either || nesting_unknown;

    // --- The arithmetic. ---------------------------------------------------
    let ctx = Ctx {
        floor_pct: binding_floor_pct(a.floor.as_ref(), b.floor.as_ref()),
        count_floor_pct: binding_floor_pct(a.count_floor.as_ref(), b.count_floor.as_ref()),
        count_moved: count_moved_materially(&a.total, &b.total),
        truncated_either: a.truncated() || b.truncated(),
        total_ms_untrustworthy,
        level,
    };

    let rows = overall_rows(&a, &b, &ctx, &mut suppressed);
    let (groups, groups_total, overflow_rows_suppressed) =
        group_rows(&a, &b, &ctx, opts, &mut suppressed);

    // Untrustworthy if anything substantive differs — or if either arm is a
    // single run, which is on its own enough to make a confident-looking delta
    // indistinguishable from scheduling noise (§9.4).
    let trustworthy = !marks.iter().any(|m| !m.is_informational())
        && !a.aggregation.is_single_run()
        && !b.aggregation.is_single_run();

    Ok(DiffResult {
        a,
        b,
        level,
        trustworthy,
        rows,
        grouped_by: opts.group_by,
        groups,
        groups_total,
        marks,
        overflow_rows_suppressed,
        suppressed,
    })
}

// ---------------------------------------------------------------------------
// Wire form
// ---------------------------------------------------------------------------

impl Row {
    fn to_wire(&self) -> logmon_broker_protocol::DiffRow {
        logmon_broker_protocol::DiffRow {
            metric: self.metric.to_string(),
            tier: self.tier.as_str().to_string(),
            a: self.a,
            b: self.b,
            delta: self.delta,
            delta_pct: self.delta_pct,
            threshold_abs: self.threshold_abs,
            threshold_pct: self.threshold_pct,
            threshold_basis: self.threshold_basis.as_str().to_string(),
            error_bound_pct: self.error_bound_pct,
            suppressed: self.suppressed,
            trustworthy: self.trustworthy,
            notes: self.notes.clone(),
        }
    }
}

impl Arm {
    fn to_wire(&self) -> logmon_broker_protocol::DiffArm {
        logmon_broker_protocol::DiffArm {
            spec: self.spec.clone(),
            collector: self.collector.clone(),
            aggregation: self.aggregation.as_str(),
            filter: self.def.filter_string.clone(),
            level: self.def.level.as_str().to_string(),
            group_keys: self.def.group_keys.clone(),
            description: self.description.clone(),
            meta: (!self.meta.is_null()).then(|| self.meta.clone()),
            matched: self.total.count,
            wall_ms: self.wall_ms,
            window_start: self.window_start,
            taken_at: self.taken_at,
            nesting: self.nesting.to_string(),
            sample_complete: self.sample_complete,
            cardinality_capped: self.cardinality_capped,
            ingest: self.ingest.map(|i| logmon_broker_protocol::ProfileIngest {
                drops_in_window: i.dropped,
                shed_batches: i.shed_batches,
                malformed_dropped: i.malformed,
                malformed_timestamps: self.total.malformed_timestamps,
                negative_duration_spans: self.total.negative_duration_spans,
                attribution: "domain".into(),
            }),
            floor: self
                .floor
                .as_ref()
                .map(|f| logmon_broker_protocol::RunToRunFloor {
                    metric: f.metric.to_string(),
                    runs: f.runs,
                    min: f.min,
                    max: f.max,
                    mean: f.mean,
                    cv_pct: f.cv_pct,
                    caveat: RunToRunFloor::CAVEAT.to_string(),
                }),
        }
    }
}

impl DiffResult {
    pub fn to_wire(&self) -> logmon_broker_protocol::CollectorsDiffResult {
        logmon_broker_protocol::CollectorsDiffResult {
            a: self.a.to_wire(),
            b: self.b.to_wire(),
            level: self.level.as_str().to_string(),
            trustworthy: self.trustworthy,
            rows: self.rows.iter().map(Row::to_wire).collect(),
            grouped_by: (self.grouped_by != DiffGroupBy::None)
                .then(|| self.grouped_by.as_str().to_string()),
            groups: self
                .groups
                .iter()
                .map(|g| logmon_broker_protocol::DiffGroup {
                    key: g.key.clone(),
                    presence: g.presence.to_string(),
                    rows: g.rows.iter().map(Row::to_wire).collect(),
                })
                .collect(),
            marks: self
                .marks
                .iter()
                .map(|m| logmon_broker_protocol::DiffMark {
                    kind: m.kind.clone(),
                    detail: m.detail.clone(),
                    overridden_by: m.overridden_by.clone(),
                })
                .collect(),
            groups_total: self.groups_total,
            overflow_rows_suppressed: self.overflow_rows_suppressed,
            suppressed: self.suppressed.clone(),
        }
    }
}

/// Everything the row builders need that is not one of the two values.
struct Ctx {
    /// Run-to-run floor binding both arms, as a percentage. `None` when either
    /// arm is single-run — **unknown, never zero** (§6.5).
    floor_pct: Option<f64>,
    /// The same, for `count`. Separate because variance is per-metric.
    count_floor_pct: Option<f64>,
    count_moved: bool,
    truncated_either: bool,
    total_ms_untrustworthy: bool,
    level: Level,
}

fn overall_rows(
    a: &Arm,
    b: &Arm,
    ctx: &Ctx,
    suppressed: &mut Vec<logmon_broker_protocol::Suppressed>,
) -> Vec<Row> {
    let mut rows = Vec::new();

    rows.push(exact_row(
        "count",
        a.total.count as f64,
        b.total.count as f64,
        ctx,
    ));

    let mut total = exact_row(
        "total_ms",
        ns_to_ms(a.total.total_ns),
        ns_to_ms(b.total.total_ns),
        ctx,
    );
    if ctx.total_ms_untrustworthy {
        total.trustworthy = false;
        total.notes.push(
            "nesting makes this number double-count, or the level cannot say whether it \
             does — see the `nesting` mark"
                .into(),
        );
    }
    rows.push(total);

    // `avg_ms` is exact and still invalid as an effect size once the count has
    // moved, because the denominator moved with it (§9.5).
    match (a.total.avg_ns(), b.total.avg_ns()) {
        (Some(x), Some(y)) if !ctx.count_moved => {
            rows.push(exact_row("avg_ms", x / 1e6, y / 1e6, ctx))
        }
        (Some(_), Some(_)) => {
            let mut r = Row::absent(
                "avg_ms",
                Tier::Exact,
                format!(
                    "`count` moved by more than {MATERIAL_COUNT_CHANGE_PCT}% between the \
                     arms, so an average-per-span delta blends a change in cost with a \
                     change in how many there were"
                ),
            );
            r.a = a.total.avg_ns().map(|v| v / 1e6);
            r.b = b.total.avg_ns().map(|v| v / 1e6);
            r.notes.push(
                "compare `total_ms` for the aggregate and the percentiles for the \
                       per-span shape"
                    .into(),
            );
            rows.push(r);
        }
        _ => {}
    }

    // The one piece of correctness evidence logmon holds. §9.4 says a faster
    // arm may be faster because it is wrong and logmon cannot know — which is
    // true in general, and precisely why the part it *can* see belongs in the
    // comparison rather than in a caveat.
    rows.push(exact_row(
        "error_count",
        a.total.error_count as f64,
        b.total.error_count as f64,
        ctx,
    ));

    // Estimated percentiles: the sketch covers the whole population, so these
    // survive truncation. They carry §5.6's bound instead.
    for (i, name) in PERCENTILE_NAMES.iter().enumerate() {
        let q = PERCENTILES[i];
        match (a.total.quantile_ns(q), b.total.quantile_ns(q)) {
            (Some(x), Some(y)) => rows.push(estimated_row(name, x / 1e6, y / 1e6, ctx)),
            _ => rows.push(Row::absent(
                name,
                Tier::Estimated,
                "at least one arm's sketch is empty, so it has no percentile to compare".into(),
            )),
        }
    }

    // Sample-derived rows.
    let (pa, pb) = (a.projections.as_ref(), b.projections.as_ref());
    match (pa, pb) {
        (Some(pa), Some(pb)) => {
            if ctx.level.has_tree() {
                match (pa.self_ms, pb.self_ms) {
                    (Some(x), Some(y)) => {
                        let mut r = sampled_row("self_ms", x, y, ctx);
                        if ctx.truncated_either {
                            r.trustworthy = false;
                        }
                        rows.push(r);
                    }
                    _ => rows.push(Row::absent(
                        "self_ms",
                        Tier::Sampled,
                        "at least one arm has no matched span nested under another matched \
                         span, so its self time equals its total time by construction and \
                         carries no information"
                            .into(),
                    )),
                }
            } else {
                rows.push(Row::absent(
                    "self_ms",
                    Tier::Sampled,
                    format!(
                        "self time needs parent identity, which level `{}` does not retain",
                        ctx.level.as_str()
                    ),
                ));
            }
            for (i, name) in PERCENTILE_NAMES.iter().enumerate() {
                // Absent on either side means that arm recorded no percentile
                // there at all — an empty window. No row rather than a row of
                // nulls: `sampled` already failed to exist as a block, and the
                // suppression pushed above says so once instead of five times.
                if let (Some(x), Some(y)) = (sampled_pct(pa, i), sampled_pct(pb, i)) {
                    let mut r = sampled_row(name, x, y, ctx);
                    if ctx.truncated_either {
                        r.trustworthy = false;
                    }
                    rows.push(r);
                }
            }
        }
        // Name the actual reason. "Recorded with projections disabled" is a
        // false statement about a merged arm, and a suppression channel is only
        // worth having if a reader can trust what it says.
        _ => {
            let arms: [&Arm; 2] = [a, b];
            // `Merged(_)`, not `len() > 1`: `Arm::merged` drops projections for
            // every merge, including a one-run one, so keying on the length gave
            // a one-run merge the generic reason and told the reader their
            // `tree`-level, projections-enabled run had projections disabled.
            let merged = arms
                .into_iter()
                .find(|x| matches!(&x.aggregation, Aggregation::Merged(_)));
            let (reason, remedy) = match merged {
                Some(m) => (
                    format!(
                        "arm `{}` merges several runs, and sample-derived figures do not \
                         merge: a self time across two runs is not a self time, and a wall \
                         union across them is not a union",
                        m.spec
                    ),
                    "document or diff a single run (`<collector>@<label>`) to get self time \
                     and the sampled percentiles"
                        .to_string(),
                ),
                None => (
                    "at least one arm has no sample-derived figures — recorded with \
                     projections disabled, or defined at level `scalar`"
                        .to_string(),
                    "take snapshots with projections: true, at level `timing` or above".to_string(),
                ),
            };
            suppressed.push(logmon_broker_protocol::Suppressed {
                field: "sampled".into(),
                reason,
                remedy: Some(remedy),
            })
        }
    }

    rows
}

const PERCENTILE_NAMES: [&str; 4] = ["p50_ms", "p80_ms", "p95_ms", "p99_ms"];

fn sampled_pct(p: &ProfileSampled, i: usize) -> Option<f64> {
    match i {
        0 => p.p50_ms,
        1 => p.p80_ms,
        2 => p.p95_ms,
        3 => p.p99_ms,
        _ => None,
    }
}

fn group_rows(
    a: &Arm,
    b: &Arm,
    ctx: &Ctx,
    opts: &DiffOptions,
    suppressed: &mut Vec<logmon_broker_protocol::Suppressed>,
) -> (Vec<GroupDiff>, usize, u64) {
    if opts.group_by == DiffGroupBy::None {
        return (Vec::new(), 0, 0);
    }
    if opts.group_by == DiffGroupBy::Group && a.def.group_keys != b.def.group_keys {
        suppressed.push(logmon_broker_protocol::Suppressed {
            field: "groups".into(),
            reason: format!(
                "the arms declare different group_keys ({:?} vs {:?}), so a row keyed by \
                 one arm's tuple names a different thing in the other",
                a.def.group_keys, b.def.group_keys
            ),
            remedy: Some(
                "compare by `name`, or re-record both arms with the same group_keys".into(),
            ),
        });
        return (Vec::new(), 0, 0);
    }

    if opts.group_by == DiffGroupBy::Group
        && a.def.group_keys.is_empty()
        && b.def.group_keys.is_empty()
    {
        // Otherwise this returns an empty breakdown with no reason, which reads
        // as "nothing differed by group" rather than "there are no groups".
        suppressed.push(logmon_broker_protocol::Suppressed {
            field: "groups".into(),
            reason: "neither arm declares any group_keys, so there is no group axis to                      break the numbers down by"
                .into(),
            remedy: Some(
                "arm the collectors with group_keys, e.g. [\"cache.enabled\"], or compare                  by `name`"
                    .into(),
            ),
        });
        return (Vec::new(), 0, 0);
    }

    let (ra, rb) = match opts.group_by {
        DiffGroupBy::Name => (a.per_name.as_ref(), b.per_name.as_ref()),
        DiffGroupBy::Group => (a.per_group.as_ref(), b.per_group.as_ref()),
        DiffGroupBy::None => unreachable!("returned above"),
    };
    let (ra, rb) = match (ra, rb) {
        (Some(ra), Some(rb)) => (ra, rb),
        _ => {
            suppressed.push(logmon_broker_protocol::Suppressed {
                field: "groups".into(),
                reason: format!(
                    "at least one arm did not record per-{} figures",
                    opts.group_by.as_str()
                ),
                remedy: Some(format!(
                    "take snapshots with per_{}: true",
                    opts.group_by.as_str()
                )),
            });
            return (Vec::new(), 0, 0);
        }
    };

    let mut overflow = 0u64;
    let mut keys: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for k in ra.keys().chain(rb.keys()) {
        // §3.3: an overflow row aggregates an arbitrary, arrival-ordered member
        // set, so the two arms' overflow rows are different populations that
        // happen to share a label. Never compared.
        if is_overflow(k) {
            overflow += 1;
            continue;
        }
        if seen.insert(k.as_str()) {
            keys.push(k.clone());
        }
    }
    // Overflow keys are counted once per arm they appear in above; that is the
    // number of rows dropped, which is what the field means.

    let mut out: Vec<GroupDiff> = keys
        .into_iter()
        .map(|key| {
            let (ea, eb) = (ra.get(&key), rb.get(&key));
            let presence = match (ea, eb) {
                (Some(_), Some(_)) => "both",
                (Some(_), None) => "a_only",
                (None, Some(_)) => "b_only",
                (None, None) => unreachable!("key came from one of the two maps"),
            };
            let rows = match (ea, eb) {
                (Some(ea), Some(eb)) => group_metric_rows(ea, eb, ctx),
                // Present on one side only. The absent side is **not** zero:
                // a key can be missing because nothing matched it, or because a
                // cardinality cap folded it into `__overflow__`. Reporting a
                // delta against an invented zero would be indistinguishable
                // from the case where it really was zero.
                _ => {
                    let e = ea.or(eb).expect("one side present");
                    let mut r = Row::absent(
                        "total_ms",
                        Tier::Exact,
                        format!(
                            "this key appears on one arm only ({presence}), so there is no \
                             pair to subtract"
                        ),
                    );
                    let v = ns_to_ms(e.total_ns);
                    if ea.is_some() {
                        r.a = Some(v)
                    } else {
                        r.b = Some(v)
                    }
                    let mut c = Row::absent(
                        "count",
                        Tier::Exact,
                        format!("this key appears on one arm only ({presence})"),
                    );
                    let n = e.count as f64;
                    if ea.is_some() {
                        c.a = Some(n)
                    } else {
                        c.b = Some(n)
                    }
                    vec![c, r]
                }
            };
            GroupDiff {
                key,
                presence,
                rows,
            }
        })
        .collect();

    // Ranked by the magnitude of the `total_ms` movement: the question a
    // breakdown answers is "where did the time go", and a row that did not
    // move is not an answer to it. Absent deltas sort last rather than as zero.
    out.sort_by(|x, y| {
        let m = |g: &GroupDiff| {
            g.rows
                .iter()
                .find(|r| r.metric == "total_ms")
                .and_then(|r| r.delta)
                .map(f64::abs)
        };
        match (m(x), m(y)) {
            (Some(dx), Some(dy)) => dy.total_cmp(&dx).then_with(|| x.key.cmp(&y.key)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => x.key.cmp(&y.key),
        }
    });
    let total = out.len();
    out.truncate(opts.top_n);
    (out, total, overflow)
}

fn group_metric_rows(ea: &ExactStats, eb: &ExactStats, ctx: &Ctx) -> Vec<Row> {
    let mut rows = vec![
        exact_row("count", ea.count as f64, eb.count as f64, ctx),
        exact_row(
            "total_ms",
            ns_to_ms(ea.total_ns),
            ns_to_ms(eb.total_ns),
            ctx,
        ),
    ];
    // Per-row `count_moved`: the overall counts can be flat while one group's
    // population moves entirely, which is exactly the case where its `avg_ms`
    // delta would mislead.
    let moved = count_moved_materially(ea, eb);
    if let (Some(x), Some(y)) = (ea.avg_ns(), eb.avg_ns()) {
        if moved {
            let mut r = Row::absent(
                "avg_ms",
                Tier::Exact,
                format!(
                    "this key's `count` moved by more than {MATERIAL_COUNT_CHANGE_PCT}%, so \
                     its average-per-span delta blends cost with population"
                ),
            );
            r.a = Some(x / 1e6);
            r.b = Some(y / 1e6);
            rows.push(r);
        } else {
            rows.push(exact_row("avg_ms", x / 1e6, y / 1e6, ctx));
        }
    }
    for (i, name) in PERCENTILE_NAMES.iter().enumerate() {
        let q = PERCENTILES[i];
        if let (Some(x), Some(y)) = (ea.quantile_ns(q), eb.quantile_ns(q)) {
            rows.push(estimated_row(name, x / 1e6, y / 1e6, ctx));
        }
    }
    rows
}

/// The same rendering `traces.profile` uses, so a key quoted from a profile can
/// be found in a diff.
pub fn group_label(interners: &[Interner], ids: &[u32]) -> String {
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

/// A key is an overflow key if any component of it overflowed — a tuple whose
/// second element is `__overflow__` aggregates just as arbitrary a member set
/// as one whose first did.
fn is_overflow(key: &str) -> bool {
    key == OVERFLOW_LABEL || key.split(" / ").any(|p| p == OVERFLOW_LABEL)
}

// ---------------------------------------------------------------------------
// Row construction — one threshold, printed and applied
// ---------------------------------------------------------------------------

fn exact_row(metric: &'static str, a: f64, b: f64, ctx: &Ctx) -> Row {
    // An exact metric has no measurement error: `count` is a count and
    // `total_ms` is a sum of exact durations. The only floor that applies is
    // run-to-run variance, and for single-run arms there is none to apply —
    // which is reported as unknown rather than as zero, because a floor of zero
    // would license calling a one-nanosecond difference a result.
    //
    // **The floor is chosen per metric.** A count's spread across runs and a
    // duration's spread are different quantities, and a suite with a fixed
    // iteration count has essentially none of the first.
    let floor = match metric {
        "count" | "error_count" => ctx.count_floor_pct,
        _ => ctx.floor_pct,
    };
    finish(
        metric,
        Tier::Exact,
        a,
        b,
        floor,
        ThresholdBasis::RunToRun,
        None,
    )
}

fn estimated_row(metric: &'static str, a: f64, b: f64, ctx: &Ctx) -> Row {
    // §5.6, and this is the load-bearing arithmetic of the whole module.
    //
    //     |Δ̃ − Δ| ≤ α(a + b)
    //
    // α is DDSketch's *deterministic worst-case* bound on a quantised value,
    // not the standard deviation of a random error — so the two arms' errors do
    // not combine in quadrature and √2·α (an earlier revision's ±1.4 %) is
    // wrong in both magnitude and kind.
    //
    // Rearranged as a minimum resolvable change: the delta clears its own error
    // bar exactly when |a−b| > α(a+b), i.e. when |Δ| exceeds α·(a+b) = 2α·mean.
    // So the threshold is 2α as a fraction of the mean — 2 % at α = 1 %, which
    // is where §5.6's "~2 %" comes from. It is derived here rather than written
    // as a constant so that changing ALPHA cannot leave a stale number behind.
    let resolution_pct = 2.0 * ALPHA * 100.0;
    let (pct, basis) = match ctx.floor_pct {
        Some(f) if f > resolution_pct => (Some(f), ThresholdBasis::Binding),
        Some(_) => (Some(resolution_pct), ThresholdBasis::Binding),
        None => (Some(resolution_pct), ThresholdBasis::MeasurementResolution),
    };
    let mut row = finish(metric, Tier::Estimated, a, b, pct, basis, None);
    // The error on the delta as a percentage OF THE DELTA — the quantity §5.6
    // names and §9.6 requires per row. `>= 100 %` means the error bar is at
    // least as wide as the delta, which is the same condition as
    // `|delta| <= threshold_abs` whenever the resolution floor is the binding
    // one. Printed as well as applied, so a reader can check the suppression.
    let denom = (a - b).abs();
    row.error_bound_pct = if denom > 0.0 {
        Some(ALPHA * (a + b).abs() / denom * 100.0)
    } else {
        None
    };
    row
}

fn sampled_row(metric: &'static str, a: f64, b: f64, ctx: &Ctx) -> Row {
    // Exact over the records retained, so no measurement floor of its own —
    // the honesty here is `complete`, which travels as a mark rather than as a
    // widened error bar. Widening the bar would suggest the number is fuzzy
    // when the real problem is that it describes a different population.
    finish(
        metric,
        Tier::Sampled,
        a,
        b,
        ctx.floor_pct,
        ThresholdBasis::RunToRun,
        None,
    )
}

fn finish(
    metric: &'static str,
    tier: Tier,
    a: f64,
    b: f64,
    threshold_pct: Option<f64>,
    basis: ThresholdBasis,
    extra_note: Option<String>,
) -> Row {
    let delta = b - a;
    let mean = (a + b).abs() / 2.0;
    let threshold_abs = threshold_pct.map(|p| p / 100.0 * mean);
    let suppressed = threshold_abs.is_some_and(|t| delta.abs() <= t);
    let mut notes: Vec<String> = extra_note.into_iter().collect();
    if threshold_pct.is_none() {
        notes.push(
            "no floor: both arms would have to cover several runs of one configuration for \
             a run-to-run spread to exist, so nothing here separates a real change from \
             scheduling noise"
                .into(),
        );
    }
    Row {
        metric,
        tier,
        a: Some(a),
        b: Some(b),
        delta: Some(delta),
        delta_pct: (a != 0.0).then(|| delta / a.abs() * 100.0),
        threshold_abs,
        threshold_pct,
        threshold_basis: if threshold_pct.is_none() {
            ThresholdBasis::Unknown
        } else {
            basis
        },
        error_bound_pct: None,
        suppressed,
        trustworthy: true,
        notes,
    }
}

/// The run-to-run floor binding *both* arms, as a percentage.
///
/// The larger of the two coefficients of variation: a comparison is only as
/// resolvable as its noisier side. `None` — unknown — the moment either arm is
/// a single run, which is the common case and the reason most diffs report no
/// floor at all.
fn binding_floor_pct(a: Option<&RunToRunFloor>, b: Option<&RunToRunFloor>) -> Option<f64> {
    let ca = a.and_then(|f| f.cv_pct)?;
    let cb = b.and_then(|f| f.cv_pct)?;
    Some(ca.max(cb))
}

fn count_moved_materially(a: &ExactStats, b: &ExactStats) -> bool {
    let (x, y) = (a.count as f64, b.count as f64);
    let base = x.max(y);
    if base == 0.0 {
        return false;
    }
    (x - y).abs() / base * 100.0 > MATERIAL_COUNT_CHANGE_PCT
}

fn describe_layout(l: &SketchLayout) -> String {
    format!(
        "{} α={} {} range {}..{} bins {}",
        l.algo, l.alpha, l.unit, l.min_ns, l.max_ns, l.max_num_bins
    )
}

fn describe_loss(l: Option<TraceIngestLoss>) -> String {
    match l {
        None => "unknown".into(),
        Some(l) => format!(
            "{} dropped, {} shed batches, {} malformed",
            l.dropped, l.shed_batches, l.malformed
        ),
    }
}

fn describe_loss_opt(arm: &Arm) -> String {
    match arm.ingest {
        None => format!("`{}`: unknown", arm.spec),
        Some(_) => format!("`{}`: {}", arm.spec, describe_loss(arm.ingest)),
    }
}

// ---------------------------------------------------------------------------
// Filter canonicalization (§5.6)
// ---------------------------------------------------------------------------

/// A canonical form of a parsed filter, for deciding whether two arms matched
/// the same population.
///
/// Qualifiers are AND-ed, so their order carries no meaning and the canonical
/// form is sorted. Substrings are already lowercased by the parser. Thresholds
/// go in by their bit pattern, which is exactly `total_cmp` equality — that
/// function is a bit-twiddling comparison, so two `f64`s compare equal under it
/// iff their bits match, and using the bits avoids formatting round-trips.
///
/// Regex sources are compared as written. Two spellings that compile to the
/// same matcher will therefore canonicalize *differently* and mark rather than
/// pass silently — deliberately: proving two regexes equivalent is undecidable
/// in general, and a mark that says "these differ in spelling" is honest while
/// a normaliser that got it wrong would not be.
pub fn canonical_filter(f: &ParsedFilter) -> Vec<String> {
    match f {
        ParsedFilter::All => vec!["ALL".into()],
        // Admission blocks `None` at arm time, so this is unreachable through
        // the RPC surface — represented rather than panicked on, because a
        // persisted definition from a build whose admission differed would
        // otherwise take the daemon down on a read.
        ParsedFilter::None => vec!["NONE".into()],
        ParsedFilter::Qualifiers(qs) => {
            let mut v: Vec<String> = qs.iter().map(canonical_qualifier).collect();
            v.sort();
            v
        }
    }
}

fn canonical_qualifier(q: &Qualifier) -> String {
    match q {
        Qualifier::BarePattern(p) => format!("bare:{}", canonical_pattern(p)),
        Qualifier::SelectorPattern(s, p) => {
            format!("sel:{}:{}", canonical_selector(s), canonical_pattern(p))
        }
        Qualifier::LevelFilter { op, level } => format!("lvl:{op:?}:{level:?}"),
        Qualifier::DurationFilter(op, v) => {
            let o = match op {
                DurationOp::Gte => "gte",
                DurationOp::Lte => "lte",
            };
            format!("dur:{o}:{:016x}", v.to_bits())
        }
        Qualifier::BookmarkFilter { op, name } => format!("bm:{op:?}:{name}"),
        Qualifier::CursorFilter { name } => format!("cur:{name}"),
        Qualifier::SeqFilter { op, value } => format!("seq:{op:?}:{value}"),
    }
}

fn canonical_pattern(p: &Pattern) -> String {
    match p {
        Pattern::Substring(s) => format!("sub:{}", s.to_lowercase()),
        Pattern::Regex {
            source,
            case_insensitive,
            ..
        } => format!("re:{case_insensitive}:{source}"),
    }
}

fn canonical_selector(s: &Selector) -> String {
    match s {
        Selector::AdditionalField(f) => format!("af:{}", f.to_lowercase()),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests;
