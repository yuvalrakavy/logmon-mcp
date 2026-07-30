//! Snapshot history — spec §6.
//!
//! The driving workflow is a sequence, not a reading: arm a collector, run the
//! suite, snapshot, change one thing, run again, snapshot, then compare. A
//! `reset` throws the run away; a `snapshot` keeps it and starts the next
//! window in the same breath.
//!
//! **A snapshot carries its own definition.** Not a reference to the live
//! collector's — a copy of the filter, level, group keys and policy as they
//! were at the moment it was taken. Without that, a later comparison would read
//! the *current* definition and present it as proof of what the recorded run
//! measured, which is wrong precisely when someone has edited the collector
//! between runs — the case a comparison exists to survive.

use crate::collector::exact::{ns_to_ms, ExactStats};
use crate::collector::intern::Interner;
use crate::collector::project::Projected;
use crate::collector::sample::Level;
use crate::collector::sketch::SketchLayout;
use crate::collector::state::{CollectorDef, CollectorSnapshot};
use crate::receiver::TraceIngestLoss;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::{PathRow, ProfileSampled};
use std::collections::HashMap;
use std::sync::Arc;

/// Snapshots retained per collector before the oldest is evicted (§6.2).
pub const MAX_SNAPSHOTS: usize = 50;

/// What a snapshot keeps beyond the always-recorded core (§6.3).
///
/// The core — exact tier, sketches with their layout identity, the ingest
/// block, and the definition as of that moment — is not optional, because
/// every one of those is something a later reader would otherwise have to take
/// from the live collector and call evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotPolicy {
    pub per_name: bool,
    pub per_group: bool,
    /// Compute and store the sample-derived figures — self time, wall union,
    /// percentiles. **Computed at snapshot time on purpose**: the samples
    /// themselves are not retained, so a projection not taken here can never
    /// be taken later. Requires `timing` or above.
    pub projections: bool,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            per_name: true,
            per_group: true,
            projections: true,
        }
    }
}

impl SnapshotPolicy {
    /// A policy asking for more than the level can produce is an error, not a
    /// silent downgrade — the caller believes they are recording something.
    pub fn validate(&self, level: Level) -> Result<(), String> {
        if self.projections && !level.has_samples() {
            return Err(format!(
                "snapshot policy asks for `projections`, but level `{}` retains no \
                 per-span records to project from; use level `timing` or above, or set \
                 projections: false",
                level.as_str()
            ));
        }
        Ok(())
    }
}

/// One recorded run.
#[derive(Clone)]
pub struct StoredSnapshot {
    pub label: String,
    pub description: Option<String>,
    /// Provenance logmon cannot infer — commit, build profile, configuration.
    /// Per-snapshot rather than per-collector because two arms of a comparison
    /// differ in exactly this.
    pub meta: serde_json::Value,
    pub taken_at: DateTime<Utc>,
    /// The definition as of this snapshot (§6.3), never the live one.
    pub def: Arc<CollectorDef>,
    pub policy: SnapshotPolicy,
    pub window_start: DateTime<Utc>,
    pub wall_ms: f64,
    pub total: ExactStats,
    pub per_name: Option<HashMap<u32, ExactStats>>,
    pub per_group: Option<HashMap<Vec<u32>, ExactStats>>,
    pub names: Interner,
    pub group_values: Vec<Interner>,
    /// Span loss across this window. `None` when it could not be attributed.
    pub ingest: Option<TraceIngestLoss>,
    pub projections: Option<ProfileSampled>,
    /// Top call paths by self time, for rendering a flame graph from a recorded
    /// run (§9.8). Empty below level `tree`, or when the policy declined
    /// projections.
    pub paths: Vec<PathRow>,
    /// The path list was cut short by its row or byte cap, so a flame graph
    /// built from it is missing mass.
    pub paths_truncated: bool,
    /// `false` if the sample budget truncated during this window. Travels with
    /// the data so a snapshot cannot launder a truncated run into a clean-
    /// looking record (A10).
    pub sample_complete: bool,
    pub cardinality_capped: bool,
}

/// Shallow on purpose, like `Collector`'s: a snapshot holds interners and
/// per-axis maps, so a derived `Debug` would turn any diagnostic print — or an
/// `unwrap_err` in a test — into pages of output.
impl std::fmt::Debug for StoredSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredSnapshot")
            .field("label", &self.label)
            .field("taken_at", &self.taken_at)
            .field("count", &self.total.count)
            .field("total_ms", &self.total_ms())
            .field("sample_complete", &self.sample_complete)
            .finish()
    }
}

impl StoredSnapshot {
    /// Build from a taken view. `projections` is computed by the caller,
    /// outside the collector lock.
    #[allow(clippy::too_many_arguments)]
    pub fn from_view(
        label: String,
        description: Option<String>,
        meta: serde_json::Value,
        taken_at: DateTime<Utc>,
        policy: SnapshotPolicy,
        view: CollectorSnapshot,
        ingest: Option<TraceIngestLoss>,
        projected: Projected,
    ) -> Self {
        let window_start = view.window_start();
        let wall_ms = (taken_at - window_start).num_microseconds().unwrap_or(0) as f64 / 1000.0;
        Self {
            label,
            description,
            meta,
            taken_at,
            def: view.def.clone(),
            policy,
            window_start,
            wall_ms: wall_ms.max(0.0),
            per_name: policy.per_name.then(|| view.per_name.clone()),
            per_group: policy.per_group.then(|| view.per_group.clone()),
            names: view.names.clone(),
            group_values: view.group_values.clone(),
            sample_complete: view.samples.complete,
            cardinality_capped: view.cardinality_capped(),
            total: view.total,
            ingest,
            projections: projected.sampled,
            paths: projected.paths,
            paths_truncated: projected.paths_truncated,
        }
    }

    /// The layout its sketches were built with. Two snapshots may only be
    /// merged or subtracted across an identical layout (§6.5).
    pub fn layout(&self) -> SketchLayout {
        self.total.sketch().layout()
    }

    pub fn total_ms(&self) -> f64 {
        ns_to_ms(self.total.total_ns)
    }
}

/// Label allocation and eviction for one collector.
///
/// Auto-labels never reuse a number after eviction (§6.2). `snapshot-3`
/// referring to two different runs over a collector's life would make every
/// note, commit message and conversation that mentions one ambiguous.
pub struct History {
    snapshots: Vec<StoredSnapshot>,
    next_auto: u64,
    evicted: u64,
    max: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    DuplicateLabel(String),
    InvalidLabel(String),
    UnknownLabel { label: String, known: Vec<String> },
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::DuplicateLabel(l) => write!(
                f,
                "a snapshot labelled `{l}` already exists for this collector; labels are \
                 unique per collector so a reference to one is never ambiguous"
            ),
            HistoryError::InvalidLabel(l) => write!(
                f,
                "invalid snapshot label `{l}`: use letters, digits, `.`, `_` and `-` only \
                 (`@` would break the `collector@label` syntax)"
            ),
            HistoryError::UnknownLabel { label, known } => {
                if known.is_empty() {
                    write!(
                        f,
                        "no snapshot labelled `{label}`; this collector has none yet"
                    )
                } else {
                    write!(
                        f,
                        "no snapshot labelled `{label}`; this collector has: {}",
                        known.join(", ")
                    )
                }
            }
        }
    }
}

impl std::error::Error for HistoryError {}

/// Labels reach `collector@label` syntax and, in phase 3, filenames.
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

impl History {
    pub fn new() -> Self {
        Self::with_max(MAX_SNAPSHOTS)
    }

    pub fn with_max(max: usize) -> Self {
        Self {
            snapshots: Vec::new(),
            next_auto: 1,
            evicted: 0,
            max: max.max(1),
        }
    }

    /// Reserve a label, auto-generating one when none is given.
    ///
    /// Auto-generation skips any number a caller has already taken by hand, so
    /// an explicit `snapshot-4` never collides with a later automatic one.
    pub fn reserve_label(&mut self, requested: Option<&str>) -> Result<String, HistoryError> {
        match requested {
            Some(l) => {
                if !is_valid_label(l) {
                    return Err(HistoryError::InvalidLabel(l.to_string()));
                }
                if self.snapshots.iter().any(|s| s.label == l) {
                    return Err(HistoryError::DuplicateLabel(l.to_string()));
                }
                Ok(l.to_string())
            }
            None => loop {
                let candidate = format!("snapshot-{}", self.next_auto);
                self.next_auto += 1;
                if !self.snapshots.iter().any(|s| s.label == candidate) {
                    return Ok(candidate);
                }
            },
        }
    }

    /// Store a snapshot, evicting the oldest if the cap is reached.
    pub fn push(&mut self, snap: StoredSnapshot) {
        self.snapshots.push(snap);
        while self.snapshots.len() > self.max {
            self.snapshots.remove(0);
            self.evicted += 1;
        }
    }

    /// Oldest first.
    pub fn all(&self) -> &[StoredSnapshot] {
        &self.snapshots
    }

    pub fn get(&self, label: &str) -> Result<&StoredSnapshot, HistoryError> {
        self.snapshots
            .iter()
            .find(|s| s.label == label)
            .ok_or_else(|| HistoryError::UnknownLabel {
                label: label.to_string(),
                known: self.snapshots.iter().map(|s| s.label.clone()).collect(),
            })
    }

    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// Restore the counters a persisted file recorded.
    ///
    /// The auto-label counter has to come back, or a restarted daemon would
    /// re-issue `snapshot-1` for a run that is not the first — the ambiguity
    /// the never-reuse rule exists to prevent, reintroduced by a restart.
    pub fn restore_counters(&mut self, next_auto: u64, evicted: u64) {
        self.next_auto = next_auto.max(1);
        self.evicted = evicted;
    }

    /// The next auto-label number, for persistence.
    pub fn next_auto(&self) -> u64 {
        self.next_auto
    }

    /// How many were dropped to the cap. Reported rather than inferred: a
    /// caller comparing "the first run" against the latest needs to know the
    /// first run is gone.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Merging and the run-to-run floor (§6.5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum MergeError {
    Empty,
    /// Sketches built with different parameters cannot be combined, and the
    /// mismatch is provable from a recorded fact rather than guessed at.
    LayoutMismatch {
        first: String,
        offending: String,
    },
    /// Runs recorded under different definitions. §7.1 keeps history across a
    /// structural edit, so this is reachable through the sanctioned workflow —
    /// and combining them would report the spread across *configurations* as
    /// scheduling variance.
    DefinitionMismatch {
        a: String,
        b: String,
    },
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Empty => write!(f, "no snapshots to merge"),
            MergeError::LayoutMismatch { first, offending } => write!(
                f,
                "cannot merge snapshots recorded with different sketch layouts \
                 ({first} vs {offending}); merging them would produce percentiles with no \
                 stated accuracy at all"
            ),
            MergeError::DefinitionMismatch { a, b } => write!(
                f,
                "cannot merge runs recorded under different definitions: {a} vs {b}. An arm \
                 is one measurement of one population — summing two of them would report \
                 the spread across configurations as scheduling variance, and would fold \
                 one filter's span names into another's. Name the runs you want \
                 individually (`<collector>@<label>`), or re-record them under one \
                 definition"
            ),
        }
    }
}

impl std::error::Error for MergeError {}

/// Combine runs. Exact scalars and sketches are additive; sample-derived
/// projections are **not**, and are omitted with a reason rather than summed
/// into something meaningless (self time across two runs is not a self time).
pub fn merge(snapshots: &[StoredSnapshot]) -> Result<ExactStats, MergeError> {
    let (first, rest) = snapshots.split_first().ok_or(MergeError::Empty)?;
    let layout = first.layout();
    let mut acc = first.total.clone();
    for s in rest {
        if s.layout() != layout {
            return Err(MergeError::LayoutMismatch {
                first: format!("{layout:?}"),
                offending: format!("{:?}", s.layout()),
            });
        }
        // `ExactStats::merge` refuses a layout mismatch itself; the check above
        // exists to name WHICH snapshot disagreed, which the inner error cannot.
        acc.merge(&s.total)
            .map_err(|_| MergeError::LayoutMismatch {
                first: format!("{layout:?}"),
                offending: format!("{:?}", s.layout()),
            })?;
    }
    Ok(acc)
}

/// Spread across repeats of one configuration (§6.5).
#[derive(Debug, Clone)]
pub struct RunToRunFloor {
    pub metric: &'static str,
    pub runs: usize,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// Coefficient of variation as a percentage. `None` for a single run —
    /// **unknown, never absent**: a floor reported as zero would license
    /// calling any difference significant.
    pub cv_pct: Option<f64>,
}

impl RunToRunFloor {
    /// What the number does *not* cover. Stated with the figure because a
    /// bare spread invites being read as a total error bar.
    pub const CAVEAT: &'static str =
        "covers run-to-run scheduling variance only — not ingest loss, not sample \
         truncation, and not thermal or frequency drift over a long session";

    pub fn over_total_ms(snapshots: &[StoredSnapshot]) -> Option<Self> {
        let values: Vec<f64> = snapshots.iter().map(|s| s.total_ms()).collect();
        Self::over(values, "total_ms")
    }

    /// The spread of the matched **count** across runs.
    ///
    /// Kept apart from the duration spread because they are different
    /// quantities: a suite that runs a fixed number of iterations has near-zero
    /// count variance while its timings vary by percent, and using one as a
    /// threshold for the other over-suppresses in one direction and
    /// under-suppresses in the other.
    pub fn over_count(snapshots: &[StoredSnapshot]) -> Option<Self> {
        let values: Vec<f64> = snapshots.iter().map(|s| s.total.count as f64).collect();
        Self::over(values, "count")
    }

    fn over(values: Vec<f64>, metric: &'static str) -> Option<Self> {
        let n = values.len();
        if n == 0 {
            return None;
        }
        let mean = values.iter().sum::<f64>() / n as f64;
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        // Sample standard deviation (n−1): these are repeats drawn from a
        // process, not the whole population of runs that could ever happen.
        let cv_pct = if n < 2 || mean == 0.0 {
            None
        } else {
            let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
            Some(var.sqrt() / mean * 100.0)
        };
        Some(Self {
            metric,
            runs: n,
            min,
            max,
            mean,
            cv_pct,
        })
    }
}

#[cfg(test)]
mod tests;
