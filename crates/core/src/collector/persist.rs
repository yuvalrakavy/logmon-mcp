//! Per-collector persistence — spec §10.
//!
//! **One file per collector, holding the definition *and* its history**, and
//! both written together on every durable change. An earlier revision made
//! snapshots write-through but left the definition to graceful shutdown, so a
//! `kill -9` produced orphan history with nothing to attach it to — the
//! restart test failed by construction rather than by accident.
//!
//! What comes back is deliberately less than what went in. A collector is
//! restored **armed but zeroed**: its definition and every recorded run
//! survive, its live window does not. The live window is a partial measurement
//! interrupted by a daemon restart, and resuming it would produce a `wall_ms`
//! spanning an outage with a span count that skipped it.

use crate::collector::exact::ExactStats;
use crate::collector::history::{History, SnapshotPolicy, StoredSnapshot};
use crate::collector::intern::Interner;
use crate::collector::sample::Level;
use crate::collector::sketch::{DurationSketch, SketchLayout};
use crate::collector::state::CollectorDef;
use crate::daemon::persistence::atomic_write;
use crate::filter::parser::parse_filter;
use crate::receiver::TraceIngestLoss;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::{PathRow, ProfileSampled};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Bumped only when a change cannot be expressed additively. A file whose
/// version this build does not know is quarantined rather than guessed at.
pub const FORMAT_VERSION: u32 = 1;

/// Subdirectory under the config dir. Keeps collector files out of the
/// directory holding `state.json`, `daemon.pid` and the socket, so the boot
/// sweeps and this one cannot reach each other's files by accident.
pub const COLLECTORS_DIR: &str = "collectors";

fn layout_repr(l: &SketchLayout) -> PersistedLayout {
    PersistedLayout {
        algo: l.algo.to_string(),
        alpha: l.alpha,
        unit: l.unit.to_string(),
        min_ns: l.min_ns,
        max_ns: l.max_ns,
        max_num_bins: l.max_num_bins,
    }
}

/// The layout as recorded, not as currently compiled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedLayout {
    pub algo: String,
    pub alpha: f64,
    pub unit: String,
    pub min_ns: f64,
    pub max_ns: f64,
    pub max_num_bins: u32,
}

impl PersistedLayout {
    /// Back to the in-memory form. `SketchLayout`'s string fields are
    /// `&'static str`, so a layout that does not match the current build
    /// cannot be represented — and that is the honest answer: this build has
    /// no algorithm by that name to compute with.
    fn to_layout(&self) -> Option<SketchLayout> {
        let cur = SketchLayout::CURRENT;
        (self.algo == cur.algo && self.unit == cur.unit).then_some(SketchLayout {
            algo: cur.algo,
            alpha: self.alpha,
            unit: cur.unit,
            min_ns: self.min_ns,
            max_ns: self.max_ns,
            max_num_bins: self.max_num_bins,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedExact {
    pub count: u64,
    /// `i128` as a string: JSON numbers are `f64` in most parsers, which would
    /// silently round a nanosecond total past 2^53 — about 104 days of summed
    /// span time, well inside what a long-running collector reaches.
    pub total_ns: String,
    pub min_ns: Option<i64>,
    pub max_ns: Option<i64>,
    pub error_count: u64,
    pub negative_duration_spans: u64,
    pub malformed_timestamps: u64,
    pub out_of_range_spans: u64,
    /// The sketch, base64-free: a byte array, since the file is already JSON
    /// and a `Vec<u8>` round-trips without another encoding to get wrong.
    pub sketch: Vec<u8>,
    pub layout: PersistedLayout,
}

impl PersistedExact {
    pub fn of(e: &ExactStats) -> Self {
        Self {
            count: e.count,
            total_ns: e.total_ns.to_string(),
            min_ns: e.min_ns,
            max_ns: e.max_ns,
            error_count: e.error_count,
            negative_duration_spans: e.negative_duration_spans,
            malformed_timestamps: e.malformed_timestamps,
            out_of_range_spans: e.out_of_range_spans,
            sketch: e.sketch().to_bytes(),
            layout: layout_repr(&e.sketch().layout()),
        }
    }

    pub fn restore(&self) -> Result<ExactStats, String> {
        let layout = self
            .to_layout_checked()
            .ok_or_else(|| format!("unknown sketch layout `{}`", self.layout.algo))?;
        let sketch = DurationSketch::from_bytes(&self.sketch, layout)?;
        // `well_formed_count()` is `count - negative - malformed` on `u64`, so a
        // file whose counts disagree underflows to something astronomical and
        // silently reports a near-zero average. Unreachable through this
        // feature's own logic, which is exactly why it belongs here: this is the
        // boundary where a hand-edited or half-written file arrives, and the
        // rest of the loader already refuses what it cannot trust.
        let excluded = self
            .negative_duration_spans
            .saturating_add(self.malformed_timestamps);
        if excluded > self.count {
            return Err(format!(
                "inconsistent counts: {excluded} spans excluded from the sum but only \
                 {} matched",
                self.count
            ));
        }
        Ok(ExactStats::from_parts(
            self.count,
            self.total_ns
                .parse::<i128>()
                .map_err(|e| format!("total_ns is not an integer: {e}"))?,
            self.min_ns,
            self.max_ns,
            self.error_count,
            self.negative_duration_spans,
            self.malformed_timestamps,
            self.out_of_range_spans,
            sketch,
        ))
    }

    fn to_layout_checked(&self) -> Option<SketchLayout> {
        self.layout.to_layout()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSnapshot {
    pub label: String,
    pub description: Option<String>,
    pub meta: serde_json::Value,
    pub taken_at: DateTime<Utc>,
    pub window_start: DateTime<Utc>,
    pub wall_ms: f64,
    /// The definition **as of this snapshot** (§6.3) — recorded so a later
    /// reader never has to take it from the live collector.
    pub filter: String,
    pub level: String,
    pub group_keys: Vec<String>,
    pub max_sample_bytes: usize,
    pub collector_description: Option<String>,
    #[serde(default)]
    pub threshold: Option<PersistedThreshold>,
    pub policy_per_name: bool,
    pub policy_per_group: bool,
    pub policy_projections: bool,
    pub total: PersistedExact,
    /// Per-axis breakdowns are dropped on the way to disk, deliberately: a
    /// collector at the name cap carries 257 sketches, and fifty snapshots of
    /// those is tens of megabytes per collector. The headline tier, the
    /// projections and the definition are what a comparison actually reads.
    pub projections: Option<ProfileSampled>,
    /// Top call paths by self time, so `collectors.document --format folded`
    /// works on a recorded run (§9.8).
    ///
    /// **Additive and optional, and `FORMAT_VERSION` deliberately does not
    /// move for it.** The only version gate in `load_all` refuses a file
    /// *newer* than this build, so a defaulted field reads correctly in both
    /// directions: a file written before paths existed loads with an empty
    /// list, and a file written with them loads in a build that ignores the
    /// key. Bumping the version would instead make every existing snapshot
    /// unreadable in exchange for nothing.
    #[serde(default)]
    pub paths: Vec<PathRow>,
    #[serde(default)]
    pub paths_truncated: bool,
    pub ingest_dropped: Option<u64>,
    pub ingest_shed_batches: Option<u64>,
    pub ingest_malformed: Option<u64>,
    pub sample_complete: bool,
    pub cardinality_capped: bool,
}

/// A threshold as recorded on disk (§8).
///
/// Flat scalars rather than the in-memory enums, so a file written by a build
/// that knew a metric this one does not can be rejected by name instead of
/// failing to parse — the loader quarantines what it cannot understand, and
/// "unknown metric `p95_ms`" is a better quarantine reason than a serde error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedThreshold {
    pub metric: String,
    #[serde(default)]
    pub group: Option<String>,
    pub op: String,
    pub value: f64,
    pub window_ms: u64,
}

impl PersistedThreshold {
    pub fn of(t: &crate::collector::threshold::Threshold) -> Self {
        Self {
            metric: t.metric.as_str().to_string(),
            group: t.group.clone(),
            op: t.op.as_str().to_string(),
            value: t.value,
            window_ms: t.window_ms,
        }
    }

    pub fn restore(&self) -> Result<crate::collector::threshold::Threshold, String> {
        use crate::collector::threshold::{Metric, Op, Threshold};
        Ok(Threshold {
            metric: Metric::parse(&self.metric)?,
            group: self.group.clone(),
            op: Op::parse(&self.op)?,
            value: self.value,
            window_ms: self.window_ms,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedCollector {
    pub version: u32,
    pub name: String,
    pub owner: String,
    /// The pinned domain. Recorded here because `PersistedSession` has no
    /// domain field and session restore hard-codes `default`, so without this
    /// every restored collector would silently re-pin to `default`.
    pub domain: String,
    pub filter: String,
    pub level: String,
    pub group_keys: Vec<String>,
    pub max_sample_bytes: usize,
    pub description: Option<String>,
    /// Additive and optional, so `FORMAT_VERSION` does not move: a file written
    /// before thresholds existed loads with none, and one written with a
    /// threshold loads in a build that ignores the key.
    #[serde(default)]
    pub threshold: Option<PersistedThreshold>,
    pub armed_at: DateTime<Utc>,
    pub snapshots: Vec<PersistedSnapshot>,
    pub next_auto_label: u64,
    pub snapshots_evicted: u64,
}

fn level_from_str(s: &str) -> Result<Level, String> {
    Ok(match s {
        "scalar" => Level::Scalar,
        "timing" => Level::Timing,
        "tree" => Level::Tree,
        other => return Err(format!("unknown level `{other}`")),
    })
}

impl PersistedSnapshot {
    pub fn of(s: &StoredSnapshot) -> Self {
        Self {
            label: s.label.clone(),
            description: s.description.clone(),
            meta: s.meta.clone(),
            taken_at: s.taken_at,
            window_start: s.window_start,
            wall_ms: s.wall_ms,
            filter: s.def.filter_string.clone(),
            level: s.def.level.as_str().to_string(),
            group_keys: s.def.group_keys.clone(),
            max_sample_bytes: s.def.max_sample_bytes,
            collector_description: s.def.description.clone(),
            threshold: s.def.threshold.as_ref().map(PersistedThreshold::of),
            policy_per_name: s.policy.per_name,
            policy_per_group: s.policy.per_group,
            policy_projections: s.policy.projections,
            total: PersistedExact::of(&s.total),
            projections: s.projections.clone(),
            paths: s.paths.clone(),
            paths_truncated: s.paths_truncated,
            ingest_dropped: s.ingest.map(|i| i.dropped),
            ingest_shed_batches: s.ingest.map(|i| i.shed_batches),
            ingest_malformed: s.ingest.map(|i| i.malformed),
            sample_complete: s.sample_complete,
            cardinality_capped: s.cardinality_capped,
        }
    }

    pub fn restore(&self, name: &str) -> Result<StoredSnapshot, String> {
        let level = level_from_str(&self.level)?;
        let filter = parse_filter(&self.filter)
            .map_err(|e| format!("recorded filter `{}` no longer parses: {e}", self.filter))?;
        let ingest = match (
            self.ingest_dropped,
            self.ingest_shed_batches,
            self.ingest_malformed,
        ) {
            (Some(dropped), Some(shed_batches), Some(malformed)) => Some(TraceIngestLoss {
                dropped,
                shed_batches,
                malformed,
            }),
            _ => None,
        };
        Ok(StoredSnapshot {
            label: self.label.clone(),
            description: self.description.clone(),
            meta: self.meta.clone(),
            taken_at: self.taken_at,
            def: Arc::new(CollectorDef {
                name: name.to_string(),
                filter_string: self.filter.clone(),
                filter,
                level,
                group_keys: self.group_keys.clone(),
                max_sample_bytes: self.max_sample_bytes,
                description: self.collector_description.clone(),
                // Dropped, never propagated. A snapshot's threshold is inert
                // metadata (§6.3) that nothing evaluates, so a metric name this
                // build does not know must not condemn a recorded run — which is
                // what `?` did here, and the opposite of what the live
                // collector's restore does with the same failure.
                threshold: self.threshold.as_ref().and_then(|t| match t.restore() {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::warn!(
                            snapshot = %self.label,
                            error = %e,
                            "dropping a recorded snapshot's threshold metadata this build                              cannot represent; the run itself was restored"
                        );
                        None
                    }
                }),
            }),
            policy: SnapshotPolicy {
                per_name: self.policy_per_name,
                per_group: self.policy_per_group,
                projections: self.policy_projections,
            },
            window_start: self.window_start,
            wall_ms: self.wall_ms,
            total: self.total.restore()?,
            // Dropped on the way to disk; a restored snapshot says so by
            // carrying `None` rather than an empty map that would read as
            // "this run matched nothing under any name".
            per_name: None,
            per_group: None,
            names: Interner::new(0),
            group_values: Vec::new(),
            ingest,
            projections: self.projections.clone(),
            paths: self.paths.clone(),
            paths_truncated: self.paths_truncated,
            sample_complete: self.sample_complete,
            cardinality_capped: self.cardinality_capped,
        })
    }
}

/// Where a collector's file lives: `{owner}.{name}.json`, each part [`encode`]d.
///
/// Owner and name both reach the filename, so two sessions may each hold a collector called
/// `perf` without colliding on disk — and no two collectors share a file, even on a
/// case-insensitive filesystem (the macOS default). It was `{owner}__{name}.json`, and both
/// parts may contain `_`: owner `a__b` with collector `c` and owner `a` with collector `b__c`
/// wrote one file, each overwriting the other's definition and history. `.` never appears
/// inside an encoded part, so it cannot be confused with the separator.
pub fn collector_path(dir: &Path, owner: &str, name: &str) -> PathBuf {
    dir.join(COLLECTORS_DIR)
        .join(format!("{}.{}.json", encode(owner), encode(name)))
}

/// Keep `[a-z0-9_-]`; percent-encode every other byte as `%XX` (uppercase hex) — `%` itself,
/// and uppercase letters. One-to-one, and still one-to-one once a case-insensitive filesystem
/// folds case: names are case-sensitive, so `Perf` and `perf` are two collectors, and with the
/// letters kept they were one file on macOS. Lowercase names stay readable; nothing unexpected
/// (an anonymous id from a wider surface, a `..`) is ever trusted into a path.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Move `path` aside as `<path>.<suffix>` — or `<path>.<suffix>.N` for the first `N` not taken,
/// so an earlier set-aside copy is never overwritten. Returns where it went, or `None` if the
/// move failed (the file is then left where it was).
fn set_aside(path: &Path, suffix: &str) -> Option<PathBuf> {
    let first = PathBuf::from(format!("{}.{suffix}", path.display()));
    let mut candidate = first.clone();
    let mut n = 1;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}.{n}", first.display()));
        n += 1;
    }
    std::fs::rename(path, &candidate).ok().map(|_| candidate)
}

pub fn save(dir: &Path, file: &PersistedCollector) -> anyhow::Result<()> {
    let path = collector_path(dir, &file.owner, &file.name);
    let bytes = serde_json::to_vec_pretty(file)?;
    atomic_write(&path, &bytes)
}

pub fn delete(dir: &Path, owner: &str, name: &str) -> bool {
    std::fs::remove_file(collector_path(dir, owner, name)).is_ok()
}

/// Everything readable in the directory, plus what had to be set aside.
pub struct LoadOutcome {
    pub collectors: Vec<PersistedCollector>,
    /// Files that could not be read. Quarantined, never deleted — a file this
    /// build cannot parse may be readable by the next one, and is in any case
    /// the only record of the runs it held.
    pub quarantined: Vec<(PathBuf, String)>,
    /// Readable files set aside because a newer copy of the same collector won — never
    /// deleted either.
    pub superseded: Vec<(PathBuf, String)>,
}

pub fn load_all(dir: &Path) -> LoadOutcome {
    let mut out = LoadOutcome {
        collectors: Vec::new(),
        quarantined: Vec::new(),
        superseded: Vec::new(),
    };
    let Ok(entries) = std::fs::read_dir(dir.join(COLLECTORS_DIR)) else {
        return out;
    };
    // Listed up front: files are renamed below, and a directory read while it changes may or
    // may not show the new names — which could load one collector twice.
    let paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    let mut by_collector: HashMap<
        (String, String),
        Vec<(PathBuf, SystemTime, PersistedCollector)>,
    > = HashMap::new();
    for path in paths {
        match read_one(&path) {
            Ok(c) => {
                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                by_collector
                    .entry((c.owner.clone(), c.name.clone()))
                    .or_default()
                    .push((path, modified, c));
            }
            Err(reason) => {
                let moved = set_aside(&path, "corrupt").unwrap_or(path);
                out.quarantined.push((moved, reason));
            }
        }
    }
    // Each collector's file belongs at its canonical path. A file elsewhere was written under
    // an earlier naming (or moved by hand). Of several copies of one collector — an earlier
    // naming's file beside the current one, after a downgrade and an upgrade — the most
    // recently written is the collector, and the others are set aside, never deleted. The
    // winner is then moved into place, unless something else is already there: a file whose
    // content names ANOTHER collector, put there by hand, is not this load's to overwrite.
    let mut keys: Vec<_> = by_collector.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let mut copies = by_collector.remove(&key).expect("listed from the map");
        let canonical = collector_path(dir, &key.0, &key.1);
        // Newest first; on a tie, the one already in place.
        copies.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then((a.0 != canonical).cmp(&(b.0 != canonical)))
        });
        let mut copies = copies.into_iter();
        let (path, _, c) = copies.next().expect("a group has at least one copy");
        for (stale, _, _) in copies {
            let moved = set_aside(&stale, "superseded").unwrap_or(stale);
            out.superseded.push((
                moved,
                format!("a newer copy of the collector is {}", path.display()),
            ));
        }
        if path != canonical {
            if canonical.exists() {
                tracing::warn!(
                    ?path,
                    ?canonical,
                    "a collector file's canonical name holds another collector; left where it is"
                );
            } else if let Err(e) = std::fs::rename(&path, &canonical) {
                // Loaded anyway; the next write goes to the canonical path, and the next boot
                // finds this copy older and sets it aside.
                tracing::warn!(?path, ?canonical, error = %e,
                    "could not move a collector file to its canonical name");
            }
        }
        out.collectors.push(c);
    }
    out
}

fn read_one(path: &Path) -> Result<PersistedCollector, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let file: PersistedCollector =
        serde_json::from_slice(&bytes).map_err(|e| format!("unparseable: {e}"))?;
    if file.version > FORMAT_VERSION {
        // Forward compatibility is not claimed. A newer file may hold fields
        // this build would drop on the next write, turning a downgrade into
        // silent data loss.
        return Err(format!(
            "written by a newer logmon (format {} > {FORMAT_VERSION})",
            file.version
        ));
    }
    Ok(file)
}

/// Rebuild the in-memory history a file describes.
///
/// A snapshot that cannot be restored does not condemn the file: the others
/// are still perfectly good runs, and refusing all of them because one had an
/// unreadable sketch would lose far more than it protects.
pub fn restore_history(file: &PersistedCollector) -> (History, Vec<String>) {
    let mut history = History::new();
    let mut errors = Vec::new();
    for s in &file.snapshots {
        match s.restore(&file.name) {
            Ok(snap) => history.push(snap),
            Err(e) => errors.push(format!("snapshot `{}`: {e}", s.label)),
        }
    }
    history.restore_counters(file.next_auto_label, file.snapshots_evicted);
    (history, errors)
}

#[cfg(test)]
mod tests;
