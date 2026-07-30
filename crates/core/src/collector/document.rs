//! Documenting a measurement — spec §9.
//!
//! Two moments, one artifact. **Now**, this is the synthesis that says what
//! moved and what to do next. **Months later**, it is triage: is this about my
//! thing, can I trust it, what did it conclude. The same bytes have to serve a
//! reader who has all the context and a reader who has none, and the second one
//! is the hard case — which is why §9.3's five questions are the structure and
//! not a checklist appended to it.
//!
//! Is this about my thing? · Can I trust it and is it comparable? · What did it
//! conclude? · Real or noise? · **What should I do next?**
//!
//! The last one is what the present-tense moment adds, and why the limitations
//! are written as instructions rather than as disclaimers.
//!
//! **The name is `document`, not `insights`.** logmon computes comparisons and
//! rankings; the insight is the reader's. `collectors.get` answers a question
//! you already have — this tells you which question to ask.
//!
//! Regeneration is free and lossless, so nothing here is committed anywhere:
//! `path` is optional, and the common shape is to read the document, learn
//! something, and regenerate it with `finding` filled in.

use crate::collector::diff::{
    diff, Aggregation, Arm, DiffAllowances, DiffError, DiffGroupBy, DiffOptions, DiffResult, Row,
    Tier,
};
use crate::collector::history::RunToRunFloor;
use crate::collector::sketch::ALPHA;
use chrono::{DateTime, Utc};
use std::fmt::Write as _;

/// Soft ceiling on the rendered document (§9.7). Past this, the per-run detail
/// tables are replaced by a pointer to the sidecar rather than truncated
/// silently — tens of KB is the target, and a document nobody can read because
/// it is a megabyte long has failed at its only job.
pub const SIZE_SOFT_CAP: usize = 48 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Md,
    Json,
    /// Collapsed stacks for a flame graph.
    Folded,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Json => "json",
            Format::Folded => "folded",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "md" | "markdown" | "" => Format::Md,
            "json" => Format::Json,
            "folded" | "collapsed" => Format::Folded,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DocumentOptions {
    pub format: Format,
    /// What the reader was trying to find out. Carries most of the triage
    /// weight months later, which is why it is asked for rather than inferred.
    pub question: Option<String>,
    /// What was concluded. Normally arrives on the *second* generation, after
    /// the first read — which is why regeneration has to be free.
    pub finding: Option<String>,
    /// What the filter was meant to capture. Defaults to the collector's
    /// description.
    pub filter_intent: Option<String>,
    /// Evidence that the faster arm is still correct. logmon cannot know this,
    /// and "we asked and nobody supplied it" is a different statement from
    /// silence (§9.4).
    pub correctness_evidence: Option<String>,
    pub group_by: DiffGroupBy,
    pub top_n: usize,
    pub allow: DiffAllowances,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            format: Format::Md,
            question: None,
            finding: None,
            filter_intent: None,
            correctness_evidence: None,
            group_by: DiffGroupBy::Name,
            top_n: 15,
            allow: DiffAllowances::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sidecar {
    /// Suggested filename, named in the document's front-matter so the two can
    /// be found together after being moved.
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub content: String,
    pub format: &'static str,
    pub bytes: usize,
    pub sidecar: Option<Sidecar>,
    /// Things the caller should know about the document itself, as distinct
    /// from the measurement it describes.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum DocumentError {
    NoArms,
    /// `folded` on an arm that never retained parent identity.
    FoldedNeedsTree {
        spec: String,
        level: String,
    },
    /// `folded` on an arm whose paths were never recorded.
    FoldedNoPaths {
        spec: String,
        why: &'static str,
    },
    /// `folded` describes one stack set; several arms have no single one.
    FoldedOneArm {
        given: usize,
    },
    /// A comparison between the baseline and one of the arms was refused.
    Comparison(DiffError),
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentError::NoArms => write!(f, "no arms to document"),
            DocumentError::FoldedNeedsTree { spec, level } => write!(
                f,
                "`folded` needs call paths, and arm `{spec}` was recorded at level `{level}`, \
                 which retains no parent identity. Re-run at level `tree`, or use \
                 `--format md`"
            ),
            DocumentError::FoldedNoPaths { spec, why } => write!(
                f,
                "`folded` needs call paths and arm `{spec}` has none: {why}. Use \
                 `--format md`, which does not need them"
            ),
            DocumentError::FoldedOneArm { given } => write!(
                f,
                "`folded` output is one set of collapsed stacks, and {given} arms have no \
                 single one — a flame graph of a difference is not a flame graph. Render one \
                 arm at a time, or use `--format md` to compare"
            ),
            DocumentError::Comparison(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DocumentError {}

/// Render the document. The first arm is the baseline; every other arm is
/// compared against it.
///
/// Baseline-relative rather than pairwise-across-all: with three arms, "what
/// moved" against a fixed reference is a table a reader can scan, while three
/// pairwise comparisons is a matrix they have to reconstruct the story from.
pub fn document(
    arms: &[Arm],
    opts: &DocumentOptions,
    now: DateTime<Utc>,
) -> Result<Document, DocumentError> {
    if arms.is_empty() {
        return Err(DocumentError::NoArms);
    }
    if opts.format == Format::Folded {
        return folded(arms, opts);
    }

    // Compare every later arm against the first.
    let mut comparisons: Vec<DiffResult> = Vec::new();
    for other in &arms[1..] {
        let d = diff(
            clone_arm(&arms[0]),
            clone_arm(other),
            &DiffOptions {
                group_by: opts.group_by,
                top_n: opts.top_n,
                allow: opts.allow,
            },
        )
        .map_err(DocumentError::Comparison)?;
        comparisons.push(d);
    }

    let facts = Facts::gather(arms, &comparisons);
    match opts.format {
        Format::Json => Ok(render_json(arms, &comparisons, &facts, opts, now)),
        _ => Ok(render_md(arms, &comparisons, &facts, opts, now)),
    }
}

/// `Arm` is not `Clone` — it holds an `ExactStats` with a sketch — so the
/// baseline is rebuilt for each comparison rather than borrowed.
///
/// A shallow copy would be wrong here even if it compiled: `diff` consumes its
/// arms so that a `DiffResult` can carry them, which is what lets a caller
/// report the definition each side was measured under without holding the
/// registry open.
fn clone_arm(a: &Arm) -> Arm {
    Arm {
        spec: a.spec.clone(),
        collector: a.collector.clone(),
        aggregation: a.aggregation.clone(),
        def: a.def.clone(),
        total: a.total.clone(),
        per_name: a.per_name.clone(),
        per_group: a.per_group.clone(),
        projections: a.projections.clone(),
        nesting: a.nesting,
        ingest: a.ingest,
        sample_complete: a.sample_complete,
        cardinality_capped: a.cardinality_capped,
        wall_ms: a.wall_ms,
        window_start: a.window_start,
        taken_at: a.taken_at,
        meta: a.meta.clone(),
        description: a.description.clone(),
        paths: a.paths.clone(),
        paths_truncated: a.paths_truncated,
        floor: a.floor.clone(),
        count_floor: a.count_floor.clone(),
    }
}

// ---------------------------------------------------------------------------
// What is true of THIS document (§9.6)
// ---------------------------------------------------------------------------

/// Which limitations actually apply here.
///
/// The point of computing this rather than printing the full catalogue: rev 2
/// listed every limitation, and a reader took the truncation row ("does not
/// affect `total_ms`, `count`, `estimated`") at face value and concluded the
/// exact tier was clean — while the ingest row said drops affect every tier.
/// Both true, jointly misleading. A limitation that does not apply is noise
/// that makes the ones that do apply harder to find.
/// One cell of the reach table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    /// This limitation touches this number.
    Affected,
    /// It does not.
    Clear,
    /// The number is not in this document, so there is nothing to certify.
    Absent,
}

impl Cell {
    fn as_str(self) -> &'static str {
        match self {
            Cell::Affected => "yes",
            Cell::Clear => "-",
            Cell::Absent => "n/a",
        }
    }
}

#[derive(Debug, Default)]
struct Facts {
    /// Whether any arm carries a sampled tier at all.
    sampled_available: bool,
    /// Whether a per-key breakdown was actually produced, so the matrix does not
    /// certify rows the document does not contain.
    breakdown_shown: bool,
    /// Whether any arm actually carries a `self_ms` figure.
    ///
    /// Tracked because the nesting remedy is "quote `self_ms`", and a merged arm
    /// has none — so in exactly the case the remedy matters most, following it
    /// is impossible. Same defect class as advising "re-run at `tree`" to
    /// someone already at `tree`: an unactionable remedy teaches the reader to
    /// skip the actionable ones.
    self_ms_available: bool,
    ingest_loss: bool,
    ingest_unknown: bool,
    truncated: bool,
    nesting_detected: bool,
    nesting_unknown: bool,
    single_run: bool,
    cardinality_capped: bool,
    overflow_suppressed: u64,
    paths_truncated: bool,
    several_services: bool,
    count_moved: bool,
    multi_arm: bool,
}

impl Facts {
    fn gather(arms: &[Arm], comparisons: &[DiffResult]) -> Self {
        let mut f = Facts {
            multi_arm: arms.len() > 1,
            ..Default::default()
        };
        for a in arms {
            match a.lost_spans() {
                Some(true) => f.ingest_loss = true,
                None => f.ingest_unknown = true,
                Some(false) => {}
            }
            f.truncated |= a.truncated();
            f.nesting_detected |= a.nesting == "detected";
            f.nesting_unknown |= a.nesting == "unknown";
            f.single_run |= a.aggregation.is_single_run();
            f.cardinality_capped |= a.cardinality_capped;
            f.paths_truncated |= a.paths_truncated;
            f.self_ms_available |= a.projections.as_ref().is_some_and(|p| p.self_ms.is_some());
            f.sampled_available |= a.projections.is_some();
        }
        for c in comparisons {
            f.breakdown_shown |= !c.groups.is_empty();
            f.overflow_suppressed += c.overflow_rows_suppressed;
            // A suppressed `avg_ms` row is how the diff reports that the
            // denominator moved, so read it from there rather than
            // recomputing the rule in a second place.
            f.count_moved |= c
                .rows
                .iter()
                .any(|r| r.metric == "avg_ms" && r.delta.is_none());
        }
        // A filter spanning services makes clock skew reach the wall union and
        // the path timings. Detected from the filter text rather than from the
        // data, because the data cannot say which service a span came from once
        // it is aggregated.
        f.several_services = arms
            .iter()
            .any(|a| !a.def.filter_string.to_lowercase().contains("sv="));
        f
    }

    /// §9.4: `trustworthy: false` whenever any arm is single-run, truncated, has
    /// ingest loss, or reports nesting it cannot see.
    fn trustworthy(&self, comparisons: &[DiffResult]) -> bool {
        !self.single_run
            && !self.truncated
            && !self.ingest_loss
            && !self.ingest_unknown
            && !self.nesting_unknown
            && comparisons.iter().all(|c| c.trustworthy)
    }

    /// The active limitations, each with the remedy that would clear it (§9.6).
    fn limitations(&self) -> Vec<(&'static str, String)> {
        let mut v: Vec<(&'static str, String)> = Vec::new();
        if self.ingest_loss {
            v.push((
                "Ingest loss",
                "Spans were dropped **before the collector saw them**, so `matched` \
                 under-counts and every tier is affected — including the exact ones. \
                 Reduce load, raise the channel capacity, and re-run."
                    .into(),
            ));
        }
        if self.ingest_unknown {
            v.push((
                "Ingest loss unknown",
                "At least one arm's pinned domain was deleted or recreated, so span loss \
                 cannot be attributed to its window at all. Treat `matched` as a lower \
                 bound; re-arm on a domain that will outlive the run."
                    .into(),
            ));
        }
        if self.truncated {
            v.push((
                "Arm truncated",
                "The sample budget stopped retention mid-run, so self time and the call \
                 paths describe a **prefix** of the run rather than a sample of it. Raise \
                 `max_sample_bytes`, or drop to level `timing` for ~2.5x the records, and \
                 re-run."
                    .into(),
            ));
        }
        if self.nesting_detected {
            // The remedy has to be reachable from where the reader is standing.
            // "Quote `self_ms`" is useless in a document whose arms have none —
            // and merged arms never do, which is exactly the case where nesting
            // matters most. Same defect class as advising "re-run at `tree`" to
            // someone already at `tree`: an unactionable remedy teaches the
            // reader to skip the actionable ones.
            v.push((
                "Nested matches",
                if self.self_ms_available {
                    "Matched spans sit under other matched spans, so `total_ms` counts the \
                     inner time twice. **Quote `self_ms`** for anything you are going to \
                     act on."
                        .to_string()
                } else {
                    "Matched spans sit under other matched spans, so `total_ms` counts the \
                     inner time twice — and **this document has no `self_ms` to quote \
                     instead**, because sample-derived figures do not survive merging \
                     runs. Document a single run (`<collector>@<label>`) to get self time, \
                     or read `total_ms` as an upper bound on the work."
                        .to_string()
                },
            ));
        }
        if self.nesting_unknown {
            v.push((
                "Nesting undetectable",
                "At least one arm was recorded below level `tree`, so whether its \
                 `total_ms` double-counts nested work is not knowable from what was \
                 retained — and nothing in the numbers will tell you. Re-run at `tree`."
                    .into(),
            ));
        }
        if self.single_run {
            v.push((
                "Single-run arm",
                "With one run per arm there is no spread, so **the floor is unknown** and \
                 nothing here separates a real change from scheduling variance. Repeat \
                 each arm and compare `<collector>@*`."
                    .into(),
            ));
        }
        if self.cardinality_capped {
            v.push((
                "Cardinality cap",
                "A cap folded values into `__overflow__`, which aggregates an arbitrary, \
                 arrival-ordered member set. Those rows are never compared, and a key \
                 absent from one arm may have been folded rather than unseen. Narrow the \
                 filter so fewer distinct values reach the collector."
                    .into(),
            ));
        }
        if self.paths_truncated {
            v.push((
                "Path list capped",
                "The stored call paths hit their row or byte cap, so a flame graph built \
                 from them is **missing mass** — nothing about looking at one reveals \
                 that. Treat the widest columns as reliable and the tail as incomplete."
                    .into(),
            ));
        }
        if self.count_moved {
            v.push((
                "Population moved",
                "`count` differs materially between the arms, so `avg_ms` is suppressed as \
                 an effect size: the denominator moved with the numerator. Compare \
                 `total_ms` for the aggregate and the percentiles for the per-span shape."
                    .into(),
            ));
        }
        if self.several_services {
            v.push((
                "Possibly several services",
                "The filter does not pin a service, so spans from several may be mixed — \
                 and clock skew between them reaches the wall union and the path timings. \
                 Add `sv=<service>` if that was not intended."
                    .into(),
            ));
        }
        // Always present: it qualifies every percentile in the document, and a
        // reader who does not know the resolution floor will act on a 1 % move.
        v.push((
            "Estimated percentiles",
            format!(
                "Each is within ±{:.0}% of the true value, and a **delta** between two of \
                 them carries ±α(a+b)/|a−b| — which reaches ±199% for a 1% change. Below \
                 roughly a {:.0}% relative change an estimated delta is not resolvable at \
                 all; those rows are suppressed with the threshold shown beside them.",
                ALPHA * 100.0,
                2.0 * ALPHA * 100.0
            ),
        ));
        v.push((
            "Work, not elapsed time",
            "`total_ms` and `self_ms` are both **work**: under N-way parallelism they \
             exceed the wall-clock time the run took. See `achieved_concurrency` before \
             reading either as duration."
                .into(),
        ));
        v.push((
            "Time, not correctness",
            "A faster arm may be faster because it is wrong, and logmon cannot tell. See \
             `correctness_evidence` in the front-matter for whether anyone said."
                .into(),
        ));
        v.push((
            "Retried batches",
            "OTLP retries are not de-duplicated by `span_id`, so a client that retried a \
             batch contributes those spans more than once."
                .into(),
        ));
        if self.multi_arm {
            v.push((
                "Confounded with time",
                "Arms run back-to-back are confounded with everything else that changed \
                 over that period — thermal state, other load, background work. Prefer \
                 **interleaved A/B/A/B** and compare the merged arms."
                    .into(),
            ));
        }
        v
    }

    /// For each reported number, which of the active limitations touch it.
    ///
    /// This matrix is the fix for rev 2's failure mode: two individually true
    /// limitation rows that were jointly misleading, because neither said which
    /// numbers it reached.
    fn effect_matrix(&self) -> (Vec<&'static str>, Vec<(&'static str, Vec<Cell>)>) {
        // EVERY active limitation gets a column. With only some of them
        // represented, a `-` in the table reads as "this number is clean" when
        // the truth is "the thing that would have dirtied it has no column" —
        // which is the same jointly-misleading shape the matrix exists to fix.
        let mut cols: Vec<&'static str> = Vec::new();
        if self.ingest_loss || self.ingest_unknown {
            cols.push("ingest");
        }
        if self.truncated {
            cols.push("truncation");
        }
        if self.nesting_detected || self.nesting_unknown {
            cols.push("nesting");
        }
        cols.push("estimate error");
        // Retries duplicate whole spans before anything here sees them, so they
        // inflate every count and every duration. Always a column, because it
        // is never possible to rule out from the data.
        cols.push("retries");
        if self.count_moved {
            cols.push("population moved");
        }
        if self.single_run {
            cols.push("no floor");
        }
        if self.cardinality_capped {
            cols.push("cardinality");
        }
        if self.several_services {
            cols.push("clock skew");
        }

        let affected = |col: &str, metric: &str| -> bool {
            match col {
                // Loss happens before the collector, so it reaches everything.
                "ingest" => true,
                // Retries duplicate spans, so they reach everything too.
                "retries" => true,
                "truncation" => matches!(
                    metric,
                    "sampled p50/p80/p95/p99" | "self_ms" | "wall_union_ms / achieved_concurrency"
                ),
                "nesting" => matches!(metric, "total_ms" | "avg_ms"),
                "estimate error" => metric == "estimated p50/p80/p95/p99",
                "population moved" => metric == "avg_ms",
                // A missing floor makes any delta unresolvable, whatever tier.
                "no floor" => true,
                // Only the per-key breakdown folds; the overall figures do not.
                "cardinality" => metric == "per-key breakdown rows",
                // Skew moves interval arithmetic, not sums or counts.
                "clock skew" => {
                    matches!(metric, "self_ms" | "wall_union_ms / achieved_concurrency")
                }
                _ => false,
            }
        };
        // Whether the number is in this document at all. A `-` against a number
        // that was never reported is a clean bill of health for something the
        // reader cannot see, which is worse than saying nothing.
        let present = |metric: &str| -> bool {
            match metric {
                "self_ms" => self.self_ms_available,
                "sampled p50/p80/p95/p99" | "wall_union_ms / achieved_concurrency" => {
                    self.sampled_available
                }
                "per-key breakdown rows" => self.breakdown_shown,
                _ => true,
            }
        };
        let cell = |col: &str, metric: &str| -> Cell {
            if !present(metric) {
                Cell::Absent
            } else if affected(col, metric) {
                Cell::Affected
            } else {
                Cell::Clear
            }
        };
        let metrics = [
            "count",
            "total_ms",
            "avg_ms",
            "error_count",
            "estimated p50/p80/p95/p99",
            "sampled p50/p80/p95/p99",
            "self_ms",
            "wall_union_ms / achieved_concurrency",
            // The row the cardinality cap actually reaches. Without it that
            // column is all dashes under a sentence promising a dash means
            // "checked and unaffected" — the jointly-misleading shape this
            // matrix exists to prevent.
            "per-key breakdown rows",
        ];
        let rows = metrics
            .iter()
            .map(|m| (*m, cols.iter().map(|c| cell(c, m)).collect()))
            .collect();
        (cols, rows)
    }
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn render_md(
    arms: &[Arm],
    comparisons: &[DiffResult],
    facts: &Facts,
    opts: &DocumentOptions,
    now: DateTime<Utc>,
) -> Document {
    let sidecar = build_sidecar(arms, comparisons, opts);
    let mut s = String::new();
    let trustworthy = facts.trustworthy(comparisons);

    front_matter(&mut s, arms, facts, opts, now, trustworthy, &sidecar);

    let names: Vec<&str> = arms.iter().map(|a| a.spec.as_str()).collect();
    let _ = writeln!(s, "\n# {}\n", names.join(" vs "));
    // Said once, up front, because every figure below inherits it. A merged arm
    // reports SUMS across its runs, so `total_ms` is not "how long the run took"
    // and `wall_ms` is not a wall clock — and a reader who assumes either is out
    // by a factor of N with no way to notice.
    let merged: Vec<&Arm> = arms
        .iter()
        .filter(|a| matches!(&a.aggregation, Aggregation::Merged(l) if l.len() > 1))
        .collect();
    if !merged.is_empty() {
        let _ = writeln!(
            s,
            "**Scale.** {} merge several runs, so every figure for {} is a **total across \
             those runs**, not a per-run figure — including `wall_ms`, which is therefore a \
             sum of window lengths rather than an elapsed time. Divide by the run count in \
             the front-matter for a per-run figure. Deltas and percentages are unaffected: \
             both sides are summed the same way.\n",
            if merged.len() == 1 {
                format!("Arm `{}` merges", merged[0].spec)
            } else {
                format!(
                    "Arms {} merge",
                    merged
                        .iter()
                        .map(|a| format!("`{}`", a.spec))
                        .collect::<Vec<_>>()
                        .join(" and ")
                )
            },
            merged
                .iter()
                .map(|a| format!("`{}`", a.spec))
                .collect::<Vec<_>>()
                .join(" and ")
        );
    }
    if let Some(q) = &opts.question {
        let _ = writeln!(s, "**Question.** {q}\n");
    }
    if let Some(fi) = &opts.finding {
        let _ = writeln!(s, "**Finding.** {fi}\n");
    }
    if !trustworthy {
        let _ = writeln!(
            s,
            "> **Not trustworthy.** At least one thing below makes a number unsafe to act \
             on. Section 2 says which, and what would clear it.\n"
        );
    }

    section_what_moved(&mut s, arms, comparisons);
    section_what_to_do(&mut s, arms, facts);

    // Section 3 is the bulk. If the document has already grown past its budget,
    // the tables move to the sidecar rather than being cut without saying so.
    let mut warnings = Vec::new();
    let mut detail = String::new();
    section_per_run(&mut detail, arms);
    if s.len() + detail.len() > SIZE_SOFT_CAP {
        let _ = writeln!(
            s,
            "## 3. Per-run detail\n\nMoved to the sidecar `{}` — the tables exceeded this \
             document's size budget. Nothing was dropped.\n",
            sidecar.name
        );
        warnings.push(format!(
            "per-run detail moved to the sidecar `{}`: the document exceeded {} KiB",
            sidecar.name,
            SIZE_SOFT_CAP / 1024
        ));
    } else {
        s.push_str(&detail);
    }

    section_vocabulary(&mut s, arms, facts);
    section_definitions(&mut s, arms, opts);

    let bytes = s.len();
    Document {
        content: s,
        format: "md",
        bytes,
        sidecar: Some(sidecar),
        warnings,
    }
}

fn front_matter(
    s: &mut String,
    arms: &[Arm],
    facts: &Facts,
    opts: &DocumentOptions,
    now: DateTime<Utc>,
    trustworthy: bool,
    sidecar: &Sidecar,
) {
    // Fixed schema, and **no field is ever merely absent** (§9.4): `null`,
    // `unknown` and `varies` are values. An absent key is indistinguishable
    // from a key nobody thought to emit, and `grep -c` over a directory of
    // these is the whole reason the schema is fixed.
    let _ = writeln!(s, "---");
    let _ = writeln!(s, "collector: {}", yaml_str(&collector_label(arms)));
    let _ = writeln!(s, "generated_at: {}", now.to_rfc3339());
    let _ = writeln!(s, "arm_count: {}", arms.len());
    let _ = writeln!(s, "question: {}", yaml_opt(&opts.question));
    let _ = writeln!(s, "finding: {}", yaml_opt(&opts.finding));
    let _ = writeln!(
        s,
        "filter_intent: {}",
        yaml_opt(&opts.filter_intent.clone().or_else(|| {
            // The COLLECTOR's description, not the snapshot's: the first says
            // what the filter was meant to capture, the second says what one run
            // was. Reading the second made two arms of the same collector report
            // `varies` about a filter they demonstrably share.
            let d: Vec<&str> = arms
                .iter()
                .filter_map(|a| a.def.description.as_deref())
                .collect();
            match d.len() {
                0 => None,
                1 => Some(d[0].to_string()),
                _ if d.iter().all(|x| *x == d[0]) => Some(d[0].to_string()),
                _ => Some("varies".into()),
            }
        }))
    );
    let _ = writeln!(s, "trustworthy: {trustworthy}");
    let _ = writeln!(
        s,
        "correctness_evidence: {}",
        yaml_unknown(&opts.correctness_evidence)
    );
    let _ = writeln!(s, "aggregation: {}", yaml_str(&aggregation_summary(arms)));
    let _ = writeln!(s, "sidecar: {}", yaml_str(&sidecar.name));
    let _ = writeln!(s, "arms:");
    for a in arms {
        let _ = writeln!(s, "  - spec: {}", yaml_str(&a.spec));
        let _ = writeln!(s, "    collector: {}", yaml_str(&a.collector));
        let _ = writeln!(s, "    aggregation: {}", yaml_str(&a.aggregation.as_str()));
        let _ = writeln!(s, "    runs: {}", run_count(&a.aggregation));
        // Promoted out of `meta` because comparing a debug build against a
        // release build is worse than having no data at all (§9.4).
        let _ = writeln!(s, "    build_profile: {}", meta_field(a, "build_profile"));
        let _ = writeln!(s, "    git_sha: {}", meta_field(a, "git_sha"));
        let _ = writeln!(
            s,
            "    correctness_evidence: {}",
            meta_field(a, "correctness_evidence")
        );
        let _ = writeln!(s, "    filter: {}", yaml_str(&a.def.filter_string));
        let _ = writeln!(s, "    level: {}", a.def.level.as_str());
        let _ = writeln!(
            s,
            "    group_keys: [{}]",
            a.def
                .group_keys
                .iter()
                .map(|k| yaml_str(k))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let _ = writeln!(s, "    matched: {}", a.total.count);
        let _ = writeln!(s, "    wall_ms: {:.1}", a.wall_ms);
        let _ = writeln!(s, "    nesting: {}", a.nesting);
        let _ = writeln!(s, "    sample_complete: {}", a.sample_complete);
        let _ = writeln!(s, "    cardinality_capped: {}", a.cardinality_capped);
        match a.ingest {
            Some(i) => {
                let _ = writeln!(
                    s,
                    "    ingest_loss: {{ dropped: {}, shed_batches: {}, malformed: {} }}",
                    i.dropped, i.shed_batches, i.malformed
                );
            }
            // Unknown, not zero. The distinction is the whole reason this field
            // is not a plain integer.
            None => {
                let _ = writeln!(s, "    ingest_loss: unknown");
            }
        }
        let _ = writeln!(s, "    floor_cv_pct: {}", floor_cv(&a.floor));
        let _ = writeln!(
            s,
            "    window_start: {}",
            a.window_start
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "null".into())
        );
        let _ = writeln!(
            s,
            "    taken_at: {}",
            a.taken_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "null".into())
        );
    }
    let _ = writeln!(s, "limitations_active: {}", facts.limitations().len());
    let _ = writeln!(s, "---");
}

fn section_what_moved(s: &mut String, arms: &[Arm], comparisons: &[DiffResult]) {
    let _ = writeln!(s, "## 1. What moved\n");
    if comparisons.is_empty() {
        let _ = writeln!(
            s,
            "Nothing: this document describes **one** arm, so there is no comparison to \
             report. The figures are in section 3.\n"
        );
        let a = &arms[0];
        let _ = writeln!(
            s,
            "| metric | value |\n|---|---|\n| matched | {} |\n| total_ms | {:.1} |\n\
             | avg_ms | {} |\n| self_ms | {} |\n",
            a.total.count,
            crate::collector::exact::ns_to_ms(a.total.total_ns),
            a.total
                .avg_ns()
                .map(|v| format!("{:.3}", v / 1e6))
                .unwrap_or_else(|| "-".into()),
            a.projections
                .as_ref()
                .and_then(|p| p.self_ms)
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "not available".into()),
        );
        return;
    }

    for c in comparisons {
        let _ = writeln!(s, "### {} → {}\n", c.a.spec, c.b.spec);

        // `total_ms` and `self_ms` side by side, because whether the second is
        // present decides which of the two the reader should quote.
        let _ = writeln!(
            s,
            "| metric | tier | {} | {} | Δ | Δ% | threshold | err/Δ |",
            c.a.spec, c.b.spec
        );
        let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
        for r in &c.rows {
            let _ = writeln!(s, "{}", md_row(r));
        }
        let _ = writeln!(s);
        for r in &c.rows {
            if r.suppressed {
                // Three different claims, and only the first is "no change".
                //
                // For an EXACT metric, identical values are identical: a count
                // is a count. For an ESTIMATED one they are not — two equal
                // sketch outputs mean only that both fell in the same bucket,
                // and the true values may still differ by up to 2α. Saying "did
                // not change at all" there contradicts this document's own
                // resolution rule, and a reader would come away believing the
                // tail was verified safe.
                match (r.delta, r.tier) {
                    (Some(0.0), Tier::Exact) => {
                        let _ = writeln!(
                            s,
                            "- `{}` did not change: the two values are identical, and this \
                             tier is exact.",
                            r.metric
                        );
                    }
                    (Some(0.0), _) => {
                        let _ = writeln!(
                            s,
                            "- `{}` came out identical, which for this tier means only that \
                             any real difference is **below the {} floor** — not that there \
                             is none.",
                            r.metric,
                            r.threshold_basis.as_str()
                        );
                    }
                    _ => {
                        let _ = writeln!(
                            s,
                            "- `{}` Δ is bracketed: it is below the {} floor, so it is \
                             present but not evidence of a change.",
                            r.metric,
                            r.threshold_basis.as_str()
                        );
                    }
                }
            }
            for n in &r.notes {
                let _ = writeln!(s, "- `{}`: {n}", r.metric);
            }
        }

        // Whole families of rows that could not be produced at all, with the
        // reason. Rendered here rather than dropped: a reader who notices
        // `self_ms` is missing from the table above has no other way to learn
        // whether it was zero, unmeasurable, or simply not asked for.
        for sup in &c.suppressed {
            match &sup.remedy {
                Some(r) => {
                    let _ = writeln!(
                        s,
                        "- **`{}` not available.** {} — {r}",
                        sup.field, sup.reason
                    );
                }
                None => {
                    let _ = writeln!(s, "- **`{}` not available.** {}", sup.field, sup.reason);
                }
            }
        }

        if !c.groups.is_empty() {
            group_table(s, c);
        }
        let _ = writeln!(s);
    }
}

/// The ranked breakdown, with **both** percentages §9.5 requires.
///
/// "share of total Δ" and "Δ to this row" are different numbers that readers
/// conflate — 4.4 % and −8.4 % can be the same line. So both are printed, with
/// distinct headings, and section 5 says what each one means.
fn group_table(s: &mut String, c: &DiffResult) {
    let overall = c
        .rows
        .iter()
        .find(|r| r.metric == "total_ms" && r.tier == Tier::Exact)
        .and_then(|r| r.delta);

    let truncated = c.groups_total > c.groups.len();
    let _ = writeln!(
        s,
        "\n**By {}** — {} rows, ranked by how much they moved.\n",
        c.grouped_by.as_str(),
        if truncated {
            format!("top {} of {}", c.groups.len(), c.groups_total)
        } else {
            format!("all {}", c.groups.len())
        }
    );
    // `count` is here because without it a reader cannot tell whether a row got
    // cheaper or simply happened less often — and for most changes under test
    // that distinction IS the question.
    let _ = writeln!(
        s,
        "| {} | a count | b count | a total_ms | b total_ms | Δ ms | Δ to this row | \
         share of total Δ |",
        c.grouped_by.as_str()
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");

    let mut shown_delta = 0.0f64;
    for g in &c.groups {
        let row = g.rows.iter().find(|r| r.metric == "total_ms");
        let (a, b, d, dpct) = match row {
            Some(r) => (r.a, r.b, r.delta, r.delta_pct),
            None => (None, None, None, None),
        };
        if let Some(d) = d {
            shown_delta += d;
        }
        let share = match (d, overall) {
            (Some(d), Some(o)) => share_pct(d, o),
            _ => "-".into(),
        };
        let suffix = match g.presence {
            "both" => String::new(),
            other => format!(" _({other})_"),
        };
        let counts = g.rows.iter().find(|r| r.metric == "count");
        let _ = writeln!(
            s,
            "| `{}`{} | {} | {} | {} | {} | {} | {} | {} |",
            g.key,
            suffix,
            opt_f(counts.and_then(|r| r.a), 0),
            opt_f(counts.and_then(|r| r.b), 0),
            opt_f(a, 1),
            opt_f(b, 1),
            d.map(|v| format!("{v:+.1}")).unwrap_or_else(|| "-".into()),
            dpct.map(|v| format!("{v:+.1}%"))
                .unwrap_or_else(|| "-".into()),
            share
        );
    }
    // The `other` row whenever rows were withheld OR the shown ones do not add
    // up: a ranked table without it lets a reader total the visible rows and
    // conclude they account for the whole change. Omitted when every row is
    // shown and they do reconcile, because an all-zero `other` row on a complete
    // table is noise that makes the informative case easier to ignore.
    if let Some(o) = overall {
        let rest = o - shown_delta;
        // A RELATIVE tolerance. `overall` and every group delta are independent
        // integer-nanosecond-to-f64-millisecond conversions, so each carries a
        // rounding; past about 10^6 ms aggregate — which a tree-level collector
        // over a test suite reaches easily — the accumulated error exceeds any
        // fixed epsilon and a fully-reconciling table grows a spurious residual
        // row, which is the noise this condition exists to suppress.
        let tolerance = (o.abs() * 1e-9).max(1e-6);
        // A row present on one arm only has no delta to subtract, so it lands in
        // the residual — but its mass IS attributed, to a row visible in this
        // same table. Naming that "unattributed" sends the reader looking for
        // something that is not missing.
        let one_sided = c.groups.iter().any(|g| g.presence != "both");
        if truncated || rest.abs() > tolerance {
            let _ = writeln!(
                s,
                "| _other ({})_ | - | - | - | - | {rest:+.1} | - | {} |",
                if truncated {
                    format!("{} rows not shown", c.groups_total - c.groups.len())
                } else if one_sided {
                    "rows present on one arm only, which have no delta to subtract".to_string()
                } else {
                    "unattributed residual".to_string()
                },
                share_pct(rest, o)
            );
        }
    }
    if c.overflow_rows_suppressed > 0 {
        let _ = writeln!(
            s,
            "\n{} row(s) are **not** in the table and not in `other`: a cardinality cap \
             folded them into `__overflow__`, which aggregates a different arbitrary set on \
             each arm. The `other` figure is therefore a residual, not a total of the \
             unshown rows.",
            c.overflow_rows_suppressed
        );
    }
}

fn section_what_to_do(s: &mut String, arms: &[Arm], facts: &Facts) {
    let _ = writeln!(s, "## 2. What to do next\n");

    let lims = facts.limitations();
    let _ = writeln!(s, "### Limitations that apply here, and their remedies\n");
    for (name, remedy) in &lims {
        let _ = writeln!(s, "- **{name}.** {remedy}");
    }

    // The per-metric matrix (§9.6): which numbers each active limitation
    // actually reaches.
    let (cols, rows) = facts.effect_matrix();
    let _ = writeln!(
        s,
        "\n### Which numbers each limitation reaches\n\n\
         A limitation that does not touch a number is not a reason to distrust it.\n"
    );
    let _ = writeln!(s, "| number | {} |", cols.join(" | "));
    let _ = writeln!(s, "|---|{}", "---|".repeat(cols.len()));
    let mut any_absent = false;
    for (metric, cells) in rows {
        any_absent |= cells.contains(&Cell::Absent);
        let marks: Vec<&str> = cells.iter().map(|c| c.as_str()).collect();
        let _ = writeln!(s, "| `{metric}` | {} |", marks.join(" | "));
    }
    if any_absent {
        let _ = writeln!(
            s,
            "\n`n/a` means the number is **not in this document at all** — not that it is \
             unaffected. Section 3 says why each one is absent."
        );
    }
    let _ = writeln!(
        s,
        "\nEvery limitation listed above has a column here. A `-` therefore means \
         *checked and unaffected*, not *not considered*."
    );

    let _ = writeln!(s, "\n### The two floors\n");
    let _ = writeln!(
        s,
        "They are different things and a conclusion must not conflate them.\n"
    );
    for a in arms {
        match &a.floor {
            Some(f) => match f.cv_pct {
                Some(cv) => {
                    let _ = writeln!(
                        s,
                        "- **Run-to-run, `{}`:** {} runs, {:.1}–{:.1} ms (mean {:.1}), CV \
                         {cv:.1}%. {}",
                        a.spec,
                        f.runs,
                        f.min,
                        f.max,
                        f.mean,
                        RunToRunFloor::CAVEAT
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "- **Run-to-run, `{}`: UNKNOWN** — one run establishes no spread.",
                        a.spec
                    );
                }
            },
            None => {
                let _ = writeln!(
                    s,
                    "- **Run-to-run, `{}`: UNKNOWN** — a single run. Repeat it and compare \
                     `{}@*`.",
                    a.spec, a.collector
                );
            }
        }
    }
    let _ = writeln!(
        s,
        "- **Measurement resolution:** ±α on each estimated percentile and \
         ±α(a+b)/|a−b| on a delta between two, at α = {:.0}%. An estimated delta below \
         roughly {:.0}% relative change is not resolvable. This is a property of the \
         sketch and repeating the run will not improve it.\n",
        ALPHA * 100.0,
        2.0 * ALPHA * 100.0
    );
}

fn section_per_run(s: &mut String, arms: &[Arm]) {
    let _ = writeln!(s, "## 3. Per-run detail\n");
    for a in arms {
        let _ = writeln!(s, "### `{}`\n", a.spec);
        let _ = writeln!(s, "{}\n", a.aggregation.as_str());

        let _ = writeln!(s, "**Exact tier** — every matched span, no sampling.\n");
        let e = &a.total;
        let _ = writeln!(s, "| metric | value |\n|---|---|");
        let _ = writeln!(s, "| count | {} |", e.count);
        let _ = writeln!(
            s,
            "| total_ms | {:.1} |",
            crate::collector::exact::ns_to_ms(e.total_ns)
        );
        let _ = writeln!(
            s,
            "| avg_ms | {} |",
            e.avg_ns()
                .map(|v| format!("{:.3}", v / 1e6))
                .unwrap_or_else(|| "-".into())
        );
        let _ = writeln!(
            s,
            "| min_ms / max_ms | {} / {} |",
            opt_f(e.min_ns.map(|v| v as f64 / 1e6), 3),
            opt_f(e.max_ns.map(|v| v as f64 / 1e6), 3)
        );
        let _ = writeln!(s, "| error_count | {} |", e.error_count);
        let _ = writeln!(s, "| out_of_range_spans | {} |", e.out_of_range_spans);

        let _ = writeln!(
            s,
            "\n**Estimated tier** — the whole population, ±{:.0}% per value.\n",
            ALPHA * 100.0
        );
        let _ = writeln!(s, "| p50 | p80 | p95 | p99 |\n|---|---|---|---|");
        let q = |p: f64| opt_f(e.quantile_ns(p).map(|v| v / 1e6), 3);
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} |",
            q(0.50),
            q(0.80),
            q(0.95),
            q(0.99)
        );

        match &a.projections {
            None => {
                let _ = writeln!(
                    s,
                    "\n**Sampled tier** — not available. {}\n",
                    if matches!(a.aggregation, Aggregation::Merged(_)) {
                        "Sample-derived figures do not merge: a self time across two runs \
                         is not a self time, and a wall union across them is not a union."
                    } else if !a.def.level.has_samples() {
                        "This arm was recorded at level `scalar`, which retains no \
                         per-span records."
                    } else {
                        "This run was recorded with projections disabled, and the records \
                         they would have come from are not retained."
                    }
                );
            }
            Some(p) => {
                // Visually demoted rather than printed at equal weight (§9.5):
                // a truncated arm's sampled percentiles can differ from its
                // estimated ones by 40 %, and side by side at the same weight
                // the reader has no reason to prefer either.
                if p.complete {
                    let _ = writeln!(
                        s,
                        "\n**Sampled tier** — exact over every retained record.\n"
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "\n**Sampled tier — TRUNCATED, read the estimated tier instead.** \
                         These cover a contiguous prefix of the run, not a sample of it, \
                         and can differ from the estimated percentiles above by tens of \
                         percent.\n"
                    );
                }
                let dm = |v: Option<f64>| match (v, p.complete) {
                    (None, _) => "-".to_string(),
                    (Some(x), true) => format!("{x:.3}"),
                    // Struck through in the markdown, so the demotion survives
                    // being pasted somewhere without this document's prose.
                    (Some(x), false) => format!("~~{x:.3}~~"),
                };
                let _ = writeln!(
                    s,
                    "| sample_count | self_ms | p50 | p80 | p95 | p99 |\n\
                     |---|---|---|---|---|---|"
                );
                let _ = writeln!(
                    s,
                    "| {} | {} | {} | {} | {} | {} |",
                    p.sample_count,
                    dm(p.self_ms),
                    dm(p.p50_ms),
                    dm(p.p80_ms),
                    dm(p.p95_ms),
                    dm(p.p99_ms)
                );
                let _ = writeln!(
                    s,
                    "\n- nested_matches: {} · overlapping_child_ms: {:.1} over {} span(s)",
                    p.nested_matches, p.overlapping_child_ms, p.overlapping_child_spans
                );
                let _ = writeln!(
                    s,
                    "- wall_union_ms: {} · achieved_concurrency: {}",
                    opt_f(p.wall_union_ms, 1),
                    opt_f(p.achieved_concurrency, 2)
                );
            }
        }

        if !a.paths.is_empty() {
            let _ = writeln!(s, "\n**Top call paths** (self time)\n");
            let _ = writeln!(s, "| path | self_ms | spans |\n|---|---|---|");
            for p in a.paths.iter().take(10) {
                let _ = writeln!(s, "| `{}` | {:.1} | {} |", p.path(), p.self_ms, p.count);
            }
            if a.paths_truncated {
                let _ = writeln!(
                    s,
                    "\nThe stored path list was capped, so a flame graph from this arm is \
                     missing mass."
                );
            }
        }
        let _ = writeln!(s);
    }
}

fn section_vocabulary(s: &mut String, arms: &[Arm], facts: &Facts) {
    let _ = writeln!(s, "## 4. The full vocabulary\n");
    let _ = writeln!(
        s,
        "Every counter each arm reports, including the ones that are zero — a counter \
         omitted because it was zero is indistinguishable from one that was never \
         collected.\n"
    );
    let _ = writeln!(
        s,
        "| arm | matched | errors | negative durations | malformed timestamps | \
         out of range | dropped | shed batches | malformed dropped |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|");
    for a in arms {
        let (d, sb, m) = match a.ingest {
            Some(i) => (
                i.dropped.to_string(),
                i.shed_batches.to_string(),
                i.malformed.to_string(),
            ),
            None => ("unknown".into(), "unknown".into(), "unknown".into()),
        };
        let _ = writeln!(
            s,
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} |",
            a.spec,
            a.total.count,
            a.total.error_count,
            a.total.negative_duration_spans,
            a.total.malformed_timestamps,
            a.total.out_of_range_spans,
            d,
            sb,
            m
        );
    }
    if facts.ingest_loss || facts.ingest_unknown {
        // §9.5's requirement, and it is a genuinely counter-intuitive point:
        // the internal consistency of these counters says nothing about
        // whether the population is complete.
        let _ = writeln!(
            s,
            "\n**The counters reconcile to `matched` by construction, which proves \
             consistency and not completeness.** `negative durations`, `malformed \
             timestamps` and `out of range` are subsets of the spans this collector saw. \
             `dropped`, `shed batches` and `malformed dropped` count spans it **never** \
             saw — they are per-domain and unfiltered, so most of what they count may be \
             spans this filter would not have matched anyway. Non-zero there means \
             `matched` is a lower bound, and no arithmetic over the numbers above can \
             recover by how much."
        );
    }
    let _ = writeln!(s);
}

fn section_definitions(s: &mut String, arms: &[Arm], opts: &DocumentOptions) {
    let _ = writeln!(s, "## 5. Definitions and how to read the numbers\n");

    // Round 2's cold reader confirmed this section changes conclusions — without
    // it they would have recommended acting on a number the document itself
    // disowned. The tier definitions come first and the collector config last.
    let _ = writeln!(s, "### The three tiers are not three views of one number\n");
    let _ = writeln!(
        s,
        "- **`exact`** — every matched span, for as long as the collector lived. Never \
         capped. `count` and `total_ms` are exact sums, so a delta between two of them is \
         also exact.\n\
         - **`estimated`** — the same population, from a DDSketch: each value is within \
         ±{:.0}% of the truth. Survives sample truncation, because the sketch is not \
         sampled.\n\
         - **`sampled`** — exact **over the records that were retained**, which is the \
         whole population only while `complete` is true. Once truncated it is a contiguous \
         prefix of the run, not a sample of it.\n\n\
         When two tiers disagree, that is information rather than error. Under truncation \
         `estimated` covers the run and `sampled` covers its beginning.\n",
        ALPHA * 100.0
    );

    let _ = writeln!(s, "### The two percentages in a ranked table\n");
    let _ = writeln!(
        s,
        "They are different numbers and conflating them is the most common misreading of \
         a table like this.\n\n\
         - **Δ to this row** — how much *this row* changed, against its own baseline. A \
         row that halved reads −50%.\n\
         - **share of total Δ** — how much of the *whole* change this row accounts for. A \
         row can be −50% on itself and 4% of the total, if it was small to begin with.\n\n\
         A negative share means the row moved **against** the total: it got slower while \
         the overall figure improved.\n"
    );

    let _ = writeln!(s, "### Suppressed deltas\n");
    let _ = writeln!(
        s,
        "A bracketed `[Δ]` is below the floor printed beside it. The number is real; what \
         is missing is any basis for calling it a change. The threshold shown is the one \
         applied — there is not a second, stricter bound doing the suppressing.\n\n\
         `err/Δ` on an estimated row is the worst-case error **as a percentage of the \
         delta**. At 100% the error bar is exactly as wide as the delta; above that, the \
         sign of the change is not established.\n"
    );

    let _ = writeln!(
        s,
        "### Self time, and why it is often the number to quote\n"
    );
    let _ = writeln!(
        s,
        "`self_ms` is a span's duration minus the union of its matched children clipped to \
         it — time in that span and nothing matched below it. The **union** is what makes \
         it correct under concurrency: a 100 ms parent with two concurrent 60 ms children \
         has 40 ms of self time, where summing the children would give −20.\n\n\
         When `nesting: detected`, `total_ms` counts inner time twice and `self_ms` is the \
         honest aggregate. When `nesting: unknown`, the arm was below level `tree` and \
         neither number can be checked.\n"
    );

    // Only when a path is actually shown. A definition for something absent from
    // the document competes for attention with the ones a reader needs.
    if arms.iter().any(|a| !a.paths.is_empty()) {
        let _ = writeln!(s, "### `[?]` in a call path\n");
        let _ = writeln!(
            s,
            "The walk stopped at a parent that was not retained — excluded by the filter, \
             or cut by truncation. The path is a **suffix**, not a chain rooted where it \
             appears to be. In `folded` output it is a literal root frame.\n"
        );
    }

    let _ = writeln!(s, "### Field glossary\n");
    let _ = writeln!(
        s,
        "Every term the front-matter and the tables use, in one place.\n\n\
         - **arm** — one side of a comparison: a collector's live window, one recorded run, \
         or several runs merged.\n\
         - **`matched`** — spans this collector's filter accepted. The same quantity the \
         tables call `count`.\n\
         - **`level`** — how much is retained per span. `scalar` (counts and totals only), \
         `timing` (adds per-span durations, so sampled percentiles and a wall union), \
         `tree` (adds span identity, so self time, nesting detection and call paths).\n\
         - **`group_keys`** — span attributes the numbers are split by. Empty means no \
         split.\n\
         - **`window_start` / `taken_at`** — when the measured window opened and closed. \
         `generated_at` is when this document was rendered, which may be much later.\n\
         - **`wall_ms`** — the length of the measured window. For a merged arm it is the \
         sum of its runs' windows, not an elapsed time.\n\
         - **`floor_cv_pct`** — the coefficient of variation of `total_ms` across an arm's \
         runs, which section 2 calls the run-to-run floor. `unknown` for a single run.\n\
         - **`sample_complete`** — whether the per-span sample budget held for the whole \
         run. `true` on a merged arm means it held for every run; it does **not** mean this \
         document has a sampled tier, because merging discards one.\n\
         - **`cardinality_capped`** — a distinct-value cap folded some keys into \
         `__overflow__`, which aggregates an arbitrary set and is never compared.\n\
         - **`out_of_range_spans`** — well-formed spans longer than the sketch's declared \
         range: counted in full toward `total_ms`, clamped in the percentiles.\n\
         - **negative durations / malformed timestamps** — matched spans this collector \
         could not use, excluded from `total_ms` and counted.\n\
         - **`dropped` / `shed batches` / `malformed dropped`** — spans lost *before* the \
         collector: a full channel, a whole request body refused, and unusable identity at \
         parse time. Per-domain and unfiltered.\n\
         - **`max_sample_bytes`** — the per-collector budget for retained per-span records.\n\
         - **`sidecar`** — a companion JSON file with the full percentile table and the \
         sketch's layout identity, which is what decides whether two documents are \
         comparable.\n\
         - **`@*`** — an arm spec meaning \"every recorded run of this collector, merged\".\n"
    );

    let _ = writeln!(s, "### The two denominators\n");
    let _ = writeln!(
        s,
        "In the section 1 table these are **not** on the same base, and comparing them \
         directly would be a mistake:\n\n\
         - **Δ%** is relative to the first arm — the ordinary \"how much did it change\" \
         figure.\n\
         - the **threshold percentage** is relative to the *mean* of the two arms, because a \
         floor is a property of the pair rather than of either side.\n\n\
         The absolute threshold beside it is in the metric's own units, so \
         `|Δ| ≤ threshold` is the comparison to make.\n"
    );

    let _ = writeln!(s, "### The collectors this describes\n");
    for a in arms {
        let _ = writeln!(
            s,
            "- `{}` — filter `{}`, level `{}`, group_keys {:?}, max_sample_bytes {}",
            a.spec,
            a.def.filter_string,
            a.def.level.as_str(),
            a.def.group_keys,
            a.def.max_sample_bytes
        );
    }
    let _ = writeln!(
        s,
        "\nRegenerate this document with `collectors.document`; it is a rendering, not a \
         record, and nothing is lost by throwing it away. Breakdown shown by `{}`, top {}.",
        opts.group_by.as_str(),
        opts.top_n
    );
}

// ---------------------------------------------------------------------------
// json and folded
// ---------------------------------------------------------------------------

fn render_json(
    arms: &[Arm],
    comparisons: &[DiffResult],
    facts: &Facts,
    opts: &DocumentOptions,
    now: DateTime<Utc>,
) -> Document {
    let sidecar = build_sidecar(arms, comparisons, opts);
    let v = serde_json::json!({
        "collector": collector_label(arms),
        "generated_at": now.to_rfc3339(),
        "question": opts.question,
        "finding": opts.finding,
        "filter_intent": opts.filter_intent,
        "trustworthy": facts.trustworthy(comparisons),
        "correctness_evidence": opts.correctness_evidence
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        "aggregation": aggregation_summary(arms),
        "arms": arms.iter().map(json_arm).collect::<Vec<_>>(),
        "comparisons": comparisons.iter().map(|c| c.to_wire()).collect::<Vec<_>>(),
        "limitations": facts.limitations().iter()
            .map(|(n, r)| serde_json::json!({ "limitation": n, "remedy": r }))
            .collect::<Vec<_>>(),
        "sidecar": sidecar.name,
    });
    let content =
        serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    let bytes = content.len();
    Document {
        content,
        format: "json",
        bytes,
        sidecar: Some(sidecar),
        warnings: Vec::new(),
    }
}

fn json_arm(a: &Arm) -> serde_json::Value {
    serde_json::json!({
        "spec": a.spec,
        "collector": a.collector,
        "aggregation": a.aggregation.as_str(),
        "runs": run_count(&a.aggregation),
        "build_profile": meta_field(a, "build_profile"),
        "git_sha": meta_field(a, "git_sha"),
        "correctness_evidence": meta_field(a, "correctness_evidence"),
        "filter": a.def.filter_string,
        "level": a.def.level.as_str(),
        "group_keys": a.def.group_keys,
        "matched": a.total.count,
        "wall_ms": a.wall_ms,
        "nesting": a.nesting,
        "sample_complete": a.sample_complete,
        "cardinality_capped": a.cardinality_capped,
        "ingest_loss": match a.ingest {
            Some(i) => serde_json::json!({
                "dropped": i.dropped,
                "shed_batches": i.shed_batches,
                "malformed": i.malformed,
            }),
            None => serde_json::Value::String("unknown".into()),
        },
        "floor_cv_pct": a.floor.as_ref().and_then(|f| f.cv_pct),
        "paths_truncated": a.paths_truncated,
    })
}

/// Collapsed stacks (§9.8).
///
/// The format rules are not stylistic. speedscope's importer matches
/// `^(.*) (\d+)$` and **drops a non-matching line silently**, while
/// flamegraph.pl accepts decimals — so decimals would render in one tool and
/// vanish without warning in the other. Integer microseconds, one ASCII space,
/// `;` inside a span name escaped to `,` because `;` is the frame separator.
fn folded(arms: &[Arm], _opts: &DocumentOptions) -> Result<Document, DocumentError> {
    if arms.len() != 1 {
        return Err(DocumentError::FoldedOneArm { given: arms.len() });
    }
    let a = &arms[0];
    if !a.def.level.has_tree() {
        return Err(DocumentError::FoldedNeedsTree {
            spec: a.spec.clone(),
            level: a.def.level.as_str().to_string(),
        });
    }
    if a.paths.is_empty() {
        return Err(DocumentError::FoldedNoPaths {
            spec: a.spec.clone(),
            why: match a.aggregation {
                Aggregation::Merged(_) => {
                    "a merged arm has no single stack set — a path's self time in two runs \
                     is two measurements, not one longer one"
                }
                _ => {
                    "this run retained no matched spans, or was recorded with projections \
                     disabled"
                }
            },
        });
    }

    let mut s = String::new();
    for p in &a.paths {
        let mut frames: Vec<String> = Vec::new();
        if p.incomplete {
            // A literal root frame, so the unresolved prefix is visible in the
            // graph rather than silently attributed to whatever comes next.
            frames.push("[?]".into());
        }
        // `;` is the frame separator, and any control character would split
        // one stack across two lines — after which speedscope drops the
        // orphaned half silently, which is the failure mode this whole format
        // is written defensively against. Span names come off the wire.
        frames.extend(p.frames.iter().map(|f| {
            f.replace(';', ",")
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
        }));
        // Integer microseconds: see the doc comment. Rounded rather than
        // truncated so a sub-microsecond path does not become a zero-count line
        // that both tools then treat as absent.
        let us = (p.self_ms * 1000.0).round().max(0.0) as u64;
        let _ = writeln!(s, "{} {}", frames.join(";"), us);
    }

    let sidecar = Sidecar {
        name: format!("{}.folded.meta.json", safe_name(&a.spec)),
        // Collapsed-stack format has no unit concept, so the invocation that
        // makes the numbers mean microseconds travels beside the file.
        content: serde_json::to_string_pretty(&serde_json::json!({
            "arm": a.spec,
            "unit": "microseconds",
            "flamegraph_invocation": "flamegraph.pl --countname us --nametype Span",
            "speedscope_note":
                "speedscope matches ^(.*) (\\d+)$ and silently drops lines that do not; \
                 counts here are integers for that reason",
            "root_frame_[?]":
                "the walk stopped at a parent that was not retained, so that stack is a \
                 suffix rather than rooted where it appears",
            "paths_truncated": a.paths_truncated,
            "sample_complete": a.sample_complete,
            "level": a.def.level.as_str(),
            "filter": a.def.filter_string,
            "layout": {
                "algo": a.layout().algo,
                "alpha": a.layout().alpha,
                "unit": a.layout().unit,
                "min_ns": a.layout().min_ns,
                "max_ns": a.layout().max_ns,
                "max_num_bins": a.layout().max_num_bins,
            },
        }))
        .unwrap_or_default(),
    };

    let mut warnings = Vec::new();
    if a.paths_truncated {
        warnings.push(
            "the stored path list was capped, so this flame graph is missing mass — \
             nothing about looking at one reveals that"
                .to_string(),
        );
    }
    if !a.sample_complete {
        warnings.push(
            "this arm hit the sample budget, so these stacks cover a contiguous prefix of \
             the run rather than a sample of it"
                .to_string(),
        );
    }

    let bytes = s.len();
    Ok(Document {
        content: s,
        format: "folded",
        bytes,
        sidecar: Some(sidecar),
        warnings,
    })
}

/// The bulk companion (§9.7): a percentile table plus layout identity.
///
/// Raw sketch buckets are deliberately absent — `Store.bins` is `pub(crate)`
/// with no public iterator, so exposing them needs an upstream PR. The
/// percentile table plus the layout is what §9.3's questions actually need: it
/// is enough to tell whether two documents are comparable.
fn build_sidecar(arms: &[Arm], comparisons: &[DiffResult], opts: &DocumentOptions) -> Sidecar {
    let quantiles: Vec<f64> = (1..=99).map(|i| i as f64 / 100.0).collect();
    Sidecar {
        name: format!("{}.sidecar.json", safe_name(&collector_label(arms))),
        content: serde_json::to_string_pretty(&serde_json::json!({
            "arms": arms.iter().map(|a| serde_json::json!({
                "spec": a.spec,
                "aggregation": a.aggregation.as_str(),
                "layout": {
                    "algo": a.layout().algo,
                    "alpha": a.layout().alpha,
                    "unit": a.layout().unit,
                    "min_ns": a.layout().min_ns,
                    "max_ns": a.layout().max_ns,
                    "max_num_bins": a.layout().max_num_bins,
                },
                "percentiles_ms": quantiles.iter().map(|q| serde_json::json!({
                    "q": q,
                    "estimated_ms": a.total.quantile_ns(*q).map(|v| v / 1e6),
                })).collect::<Vec<_>>(),
                "paths": a.paths.iter().map(|p| serde_json::json!({
                    "frames": p.frames,
                    "self_ms": p.self_ms,
                    "count": p.count,
                    "incomplete": p.incomplete,
                })).collect::<Vec<_>>(),
                "paths_truncated": a.paths_truncated,
            })).collect::<Vec<_>>(),
            "comparisons": comparisons.iter().map(|c| c.to_wire()).collect::<Vec<_>>(),
            "group_by": opts.group_by.as_str(),
        }))
        .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn md_row(r: &Row) -> String {
    let d = match r.delta {
        None => "-".to_string(),
        // Not bracketed: section 5 defines a bracket as "below the floor
        // printed beside it", and zero is not below zero. Bracketing an exact
        // metric whose two values are identical would assert a doubt that does
        // not exist about the strongest rows in the table.
        Some(d) if d == 0.0 && r.tier == Tier::Exact => "0.000".to_string(),
        Some(d) if r.suppressed => format!("[{d:+.3}]"),
        Some(d) => format!("{d:+.3}"),
    };
    // The basis travels with the number. Section 2 insists the two floors must
    // not be conflated, and one unlabelled `threshold` column that silently
    // switches between them by tier is exactly that conflation.
    let threshold = match (r.threshold_abs, r.threshold_pct) {
        (Some(abs), Some(pct)) => format!("{abs:.3} ({pct:.1}% {})", r.threshold_basis.as_str()),
        _ => "unknown".to_string(),
    };
    format!(
        "| `{}` | {} | {} | {} | {} | {} | {} | {} |",
        r.metric,
        r.tier.as_str(),
        opt_f(r.a, 3),
        opt_f(r.b, 3),
        d,
        r.delta_pct
            .map(|v| format!("{v:+.1}%"))
            .unwrap_or_else(|| "-".into()),
        threshold,
        r.error_bound_pct
            .map(|v| format!("{v:.0}%"))
            .unwrap_or_else(|| "-".into()),
    )
}

/// A row's share of the total change.
///
/// Zero is rendered without a sign, because `-0.0%` reads as "this row moved
/// **against** the total" — which section 5 gives a specific meaning to — when
/// in fact the row did not move at all.
fn share_pct(delta: f64, overall: f64) -> String {
    if overall == 0.0 {
        return "-".to_string();
    }
    let pct = delta / overall * 100.0;
    if pct.abs() < 0.05 {
        "0.0%".to_string()
    } else {
        format!("{pct:+.1}%")
    }
}

fn opt_f(v: Option<f64>, places: usize) -> String {
    v.map(|x| format!("{x:.*}", places))
        .unwrap_or_else(|| "-".into())
}

fn collector_label(arms: &[Arm]) -> String {
    let mut names: Vec<&str> = arms.iter().map(|a| a.collector.as_str()).collect();
    names.dedup();
    names.join(", ")
}

fn aggregation_summary(arms: &[Arm]) -> String {
    if arms.iter().all(|a| a.aggregation.is_single_run()) {
        return "every arm is a single run, so no run-to-run spread exists".into();
    }
    if arms.iter().any(|a| a.aggregation.is_single_run()) {
        return "mixed: at least one arm is a single run and at least one merges several".into();
    }
    "every arm merges several runs".into()
}

fn run_count(a: &Aggregation) -> usize {
    match a {
        Aggregation::Live | Aggregation::SingleRun(_) => 1,
        Aggregation::Merged(l) => l.len(),
    }
}

/// A per-arm provenance field, from `meta`.
///
/// `unknown` when absent, never omitted: "we asked and nobody supplied it" is a
/// different statement from silence, and only one of them can be grepped for.
fn meta_field(a: &Arm, key: &str) -> String {
    match a.meta.get(key) {
        Some(serde_json::Value::String(s)) => yaml_str(s),
        Some(v) => yaml_str(&v.to_string()),
        None => "unknown".into(),
    }
}

fn floor_cv(f: &Option<RunToRunFloor>) -> String {
    match f.as_ref().and_then(|f| f.cv_pct) {
        Some(cv) => format!("{cv:.2}"),
        None => "unknown".into(),
    }
}

/// A double-quoted YAML scalar.
///
/// Escapes the backslash, the quote, **and every control character** — the last
/// because `question`, `finding`, `filter_intent` and `meta` are free text from
/// the caller, and an ordinary multi-line finding puts a raw newline inside the
/// scalar. A conforming parser then either folds the break to a space, silently
/// changing the recorded value, or fails on the front-matter altogether, which
/// is the part that exists to be grep-able. No adversarial input needed.
///
/// Not a general YAML emitter: the schema here is fixed and every value is a
/// name, a filter string, or caller text.
fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything else C0, plus DEL, as a YAML hex escape.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn yaml_opt(v: &Option<String>) -> String {
    match v {
        Some(s) => yaml_str(s),
        None => "null".into(),
    }
}

fn yaml_unknown(v: &Option<String>) -> String {
    match v {
        Some(s) => yaml_str(s),
        None => "unknown".into(),
    }
}

/// A filename fragment from an arm or collector label. Collector names are
/// already `[A-Za-z0-9_-]`, but a spec carries `@` and possibly `*`, and the
/// caller may write the result to disk.
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
