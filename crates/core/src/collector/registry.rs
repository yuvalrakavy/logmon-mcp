//! The collector registry — spec §4.4, §3.4.
//!
//! **Domain-keyed, independent of `SessionState::domain`.** If collectors were
//! reached the way triggers are — via `active_session_ids_for_domain` — then a
//! `use_domain` call mid-run would silently stop a collector while it still
//! reported its pinned domain: exactly the failure pinning exists to prevent.
//!
//! A collector is owned by a session for lifecycle, and pinned to the domain it
//! was created in for ingest. Those are different questions and the registry
//! keeps them apart.

use crate::collector::history::{History, HistoryError, SnapshotPolicy, StoredSnapshot};
use crate::collector::state::{Collector, CollectorDef, CollectorSnapshot};
use crate::daemon::domain::DomainId;
use crate::daemon::session::SessionId;
use crate::filter::matcher::matches_span;
use crate::receiver::{ReceiverMetrics, TraceIngestLoss};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::ProfileSampled;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Daemon-wide sample reservation (§3.4). Enforced at arm time, so four
/// default-sized collectors is the practical ceiling across every session and
/// domain — and `add` says so when it refuses.
pub const DEFAULT_MAX_TOTAL_SAMPLE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum RegistryError {
    DuplicateName(String),
    NotFound(String),
    /// Arming would exceed the daemon-wide reservation. Carries the numbers so
    /// the caller can say what would fit rather than only that it did not.
    BudgetExceeded {
        requested: usize,
        remaining: usize,
        total: usize,
    },
    Label(HistoryError),
    /// A snapshot policy asking for more than the level can produce.
    PolicyRejected(String),
    /// The change could not be made durable, so it was **not applied**. Only
    /// `edit` fails this way: it is the one operation where a partial success
    /// leaves the on-disk definition and the armed one disagreeing.
    PersistFailed(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::DuplicateName(n) => {
                write!(f, "a collector named `{n}` already exists in this session")
            }
            RegistryError::NotFound(n) => write!(f, "no collector named `{n}` in this session"),
            RegistryError::BudgetExceeded {
                requested,
                remaining,
                total,
            } => write!(
                f,
                "arming would reserve {requested} bytes but only {remaining} of the \
                 daemon-wide {total} remain; reduce max_sample_bytes, lower the level, \
                 or remove another collector"
            ),
            RegistryError::Label(e) => write!(f, "{e}"),
            RegistryError::PolicyRejected(m) => write!(f, "{m}"),
            RegistryError::PersistFailed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RegistryError {}

struct Entry {
    owner: SessionId,
    domain: DomainId,
    collector: Arc<Collector>,
    /// The pinned domain's counters, held from arm time so a later read can
    /// prove the baseline and the current reading share an origin.
    metrics: Arc<ReceiverMetrics>,
    /// Span loss at arm time (or at the last reset). Lives here rather than on
    /// the collector because it is zeroed under the *registry's* write lock,
    /// which is the lock that excludes ingest — so a reset moves the data and
    /// the baseline together, with nothing able to slip between them (A13).
    ingest_baseline: TraceIngestLoss,
    /// Recorded runs. Survives every reset and every structural edit — §7.1
    /// zeroes the collector, never its history, and each snapshot carries the
    /// definition it was taken under.
    history: History,
    /// Why the live window is empty, when it is empty for a reason worth
    /// reporting. `Some("daemon_restart")` on a restored collector, so a
    /// caller reading zero matches can tell "nothing has happened yet" from
    /// "the run you were measuring did not survive the restart".
    zeroed_by: Option<&'static str>,
}

/// Everything `snapshot` varies. A struct rather than nine positional
/// arguments, three of which are `Option<String>` and would silently swap.
pub struct SnapshotRequest {
    pub name: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub meta: serde_json::Value,
    pub policy: SnapshotPolicy,
    pub reset: bool,
    pub now: DateTime<Utc>,
}

/// A collector plus everything the read path needs to interpret it.
pub struct ArmedCollector {
    /// Why the live window is empty, when there is a reason worth reporting.
    /// Lets a caller reading zero matches tell "nothing has happened yet" from
    /// "the run you were measuring did not survive the restart".
    pub zeroed_by: Option<&'static str>,
    pub snapshot_count: usize,
    pub collector: Arc<Collector>,
    /// The domain the collector was pinned to at arm time. A `use_domain` by
    /// the owning session does not move it.
    pub domain: DomainId,
    pub metrics: Arc<ReceiverMetrics>,
    pub ingest_baseline: TraceIngestLoss,
}

impl Entry {
    fn armed(&self) -> ArmedCollector {
        ArmedCollector {
            collector: self.collector.clone(),
            domain: self.domain.clone(),
            metrics: self.metrics.clone(),
            ingest_baseline: self.ingest_baseline,
            zeroed_by: self.zeroed_by,
            snapshot_count: self.history.len(),
        }
    }

    fn to_persisted(&self) -> crate::collector::persist::PersistedCollector {
        self.persisted_with(self.collector.def(), &self.domain)
    }

    /// The file this entry *would* write under a different definition or pin.
    ///
    /// Exists so `edit` can write before it mutates: the on-disk state and the
    /// in-memory state must never disagree about which filter is armed, and the
    /// only way to guarantee that is to make the write the commit point.
    fn persisted_with(
        &self,
        def: &CollectorDef,
        domain: &DomainId,
    ) -> crate::collector::persist::PersistedCollector {
        crate::collector::persist::PersistedCollector {
            version: crate::collector::persist::FORMAT_VERSION,
            name: def.name.clone(),
            owner: self.owner.to_string(),
            domain: domain.to_string(),
            filter: def.filter_string.clone(),
            level: def.level.as_str().to_string(),
            group_keys: def.group_keys.clone(),
            max_sample_bytes: def.max_sample_bytes,
            description: def.description.clone(),
            armed_at: self.collector.armed_at(),
            snapshots: self
                .history
                .all()
                .iter()
                .map(crate::collector::persist::PersistedSnapshot::of)
                .collect(),
            next_auto_label: self.history.next_auto(),
            snapshots_evicted: self.history.evicted(),
        }
    }
}

/// A requested change. `None` means "leave alone" — distinct from a value that
/// happens to equal the current one, which still counts as a structural edit
/// and still zeroes, because the caller asked for it.
#[derive(Debug, Default, Clone)]
pub struct CollectorEdit {
    pub description: Option<String>,
    pub filter: Option<String>,
    pub level: Option<crate::collector::sample::Level>,
    pub group_keys: Option<Vec<String>>,
    pub max_sample_bytes: Option<usize>,
    pub domain: Option<DomainId>,
}

impl CollectorEdit {
    /// Whether this edit changes what is collected, as opposed to what it is
    /// called. Only the latter is free.
    fn is_structural(&self, current: &CollectorDef) -> bool {
        self.filter
            .as_ref()
            .is_some_and(|f| *f != current.filter_string)
            || self.level.is_some_and(|l| l != current.level)
            || self
                .group_keys
                .as_ref()
                .is_some_and(|k| *k != current.group_keys)
            || self
                .max_sample_bytes
                .is_some_and(|b| b != current.max_sample_bytes)
    }

    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.filter.is_none()
            && self.level.is_none()
            && self.group_keys.is_none()
            && self.max_sample_bytes.is_none()
            && self.domain.is_none()
    }
}

pub struct SnapshotOutcome {
    pub snapshot: StoredSnapshot,
    /// `Some` when the run was recorded in memory but could not be written.
    /// Reported rather than logged, because a caller taking snapshots to
    /// compare later needs to know this one may not be there.
    pub persist_error: Option<String>,
}

pub struct EditOutcome {
    /// Whether the live window was discarded. History never is.
    pub zeroed: bool,
    pub collector: Arc<Collector>,
    pub domain: DomainId,
}

/// What `restore` found.
#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    /// Files that could not be parsed at all, moved aside.
    pub quarantined: Vec<(PathBuf, String)>,
    /// Files that parsed but described something this build cannot arm.
    pub rejected: Vec<(String, String)>,
}

pub struct CollectorRegistry {
    /// One lock. The population is bounded by the daemon reservation (four
    /// default-sized collectors), so a linear scan on the ingest path is
    /// cheaper than the hashing and allocation an index would cost.
    entries: RwLock<Vec<Entry>>,
    max_total_sample_bytes: usize,
    /// Where collector files live. `None` disables persistence entirely, which
    /// is what every unit test uses — a test that persisted would write into
    /// whatever directory it was handed, and the live daemon's is one wrong
    /// argument away.
    dir: Option<PathBuf>,
}

impl CollectorRegistry {
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_MAX_TOTAL_SAMPLE_BYTES)
    }

    pub fn with_budget(max_total_sample_bytes: usize) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            max_total_sample_bytes,
            dir: None,
        }
    }

    /// Enable write-through persistence into `dir`.
    pub fn with_persistence(mut self, dir: PathBuf) -> Self {
        self.dir = Some(dir);
        self
    }

    /// Restore collectors from disk (§10).
    ///
    /// **Armed but zeroed.** Definition and history come back; the live window
    /// does not. A live window is a partial measurement interrupted by a
    /// restart, and resuming it would produce a `wall_ms` spanning an outage
    /// with a span count that skipped it.
    ///
    /// The pinned domain is **not** validated here. `restore_named` runs long
    /// before the domain registry is built, so a check at this point would
    /// mark every collector orphaned — including the ones pinned to `default`.
    /// The check is lazy, on first read.
    pub fn restore(
        &self,
        metrics_for: impl Fn(&DomainId) -> Arc<ReceiverMetrics>,
    ) -> RestoreReport {
        let Some(dir) = &self.dir else {
            return RestoreReport::default();
        };
        let outcome = crate::collector::persist::load_all(dir);
        let mut report = RestoreReport {
            quarantined: outcome.quarantined,
            ..Default::default()
        };
        let mut g = self.entries.write().expect("registry lock poisoned");
        for file in outcome.collectors {
            match Self::entry_from_file(&file, &metrics_for) {
                Ok(entry) => {
                    report.restored.push(entry.collector.def().name.clone());
                    g.push(entry);
                }
                Err(e) => report.rejected.push((file.name.clone(), e)),
            }
        }
        report
    }

    fn entry_from_file(
        file: &crate::collector::persist::PersistedCollector,
        metrics_for: &impl Fn(&DomainId) -> Arc<ReceiverMetrics>,
    ) -> Result<Entry, String> {
        let level = match file.level.as_str() {
            "scalar" => crate::collector::sample::Level::Scalar,
            "timing" => crate::collector::sample::Level::Timing,
            "tree" => crate::collector::sample::Level::Tree,
            other => return Err(format!("unknown level `{other}`")),
        };
        let filter = crate::filter::parser::parse_filter(&file.filter)
            .map_err(|e| format!("recorded filter `{}` no longer parses: {e}", file.filter))?;
        let domain = DomainId::new(&file.domain)
            .map_err(|e| format!("recorded domain `{}` is invalid: {e}", file.domain))?;
        let def = CollectorDef {
            name: file.name.clone(),
            filter_string: file.filter.clone(),
            filter,
            level,
            group_keys: file.group_keys.clone(),
            max_sample_bytes: file.max_sample_bytes,
            description: file.description.clone(),
        };
        let (history, snapshot_errors) = crate::collector::persist::restore_history(file);
        if !snapshot_errors.is_empty() {
            tracing::warn!(
                collector = %file.name,
                errors = ?snapshot_errors,
                "some recorded runs could not be restored; the rest were kept"
            );
        }
        let metrics = metrics_for(&domain);
        Ok(Entry {
            owner: SessionId::Named(file.owner.clone()),
            domain,
            // Armed at the ORIGINAL time, so `armed_at` still says when this
            // measurement began rather than when the daemon last booted.
            collector: Arc::new(Collector::new(def, file.armed_at)),
            ingest_baseline: metrics.trace_ingest_loss(),
            metrics,
            history,
            zeroed_by: Some("daemon_restart"),
        })
    }

    /// Write one entry's file. Returns the failure rather than swallowing it,
    /// so each caller can decide: `add` arms anyway, `snapshot` reports the
    /// loss of durability, `edit` refuses outright.
    fn persist(&self, entry: &Entry) -> Result<(), String> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let file = entry.to_persisted();
        crate::collector::persist::save(dir, &file).map_err(|e| {
            tracing::error!(
                collector = %file.name, error = %e,
                "could not persist collector; it will not survive a restart"
            );
            e.to_string()
        })
    }

    pub fn add(
        &self,
        owner: &SessionId,
        domain: &DomainId,
        metrics: Arc<ReceiverMetrics>,
        def: CollectorDef,
        now: DateTime<Utc>,
    ) -> Result<Arc<Collector>, RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        if g.iter()
            .any(|e| &e.owner == owner && e.collector.def().name == def.name)
        {
            return Err(RegistryError::DuplicateName(def.name));
        }

        let reserved: usize = g.iter().map(|e| e.collector.def().max_sample_bytes).sum();
        let remaining = self.max_total_sample_bytes.saturating_sub(reserved);
        if def.max_sample_bytes > remaining {
            return Err(RegistryError::BudgetExceeded {
                requested: def.max_sample_bytes,
                remaining,
                total: self.max_total_sample_bytes,
            });
        }

        let collector = Arc::new(Collector::new(def, now));
        g.push(Entry {
            owner: owner.clone(),
            domain: domain.clone(),
            collector: collector.clone(),
            ingest_baseline: metrics.trace_ingest_loss(),
            metrics,
            history: History::new(),
            zeroed_by: None,
        });
        // Write-through at arm time, not at shutdown. A definition that only
        // reaches disk on a graceful exit is a definition a `kill -9` loses,
        // leaving snapshots with nothing to attach to.
        //
        // A failure here does not refuse the arm. The caller wants to measure
        // something now; losing durability is worse than nothing but far
        // better than refusing to collect at all, and the error is logged.
        let _ = self.persist(g.last().expect("just pushed"));
        Ok(collector)
    }

    /// Apply an edit (§7.1).
    ///
    /// **`description` is free; every other change is a reset plus a config
    /// change.** The window is swapped wholesale — exact tier, sketches,
    /// samples, interners, ingest baseline, `zeroed_at` — and every gate
    /// `add` runs is re-run. History is untouched: snapshots are immutable and
    /// carry the definition they were taken under.
    ///
    /// Ordering matters and three rules collide: the swap wants the lock, I/O
    /// must not happen under it, and a destructive change must not commit
    /// until the write succeeds. So: validate and build under the lock,
    /// persist, then swap — an edit whose persist fails has changed nothing.
    pub fn edit(
        &self,
        owner: &SessionId,
        name: &str,
        change: CollectorEdit,
        now: DateTime<Utc>,
    ) -> Result<EditOutcome, RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let idx = g
            .iter()
            .position(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;

        let old = g[idx].collector.def().clone();
        let structural = change.is_structural(&old);
        let mut def = (*old).clone();
        if let Some(d) = change.description {
            def.description = Some(d).filter(|s| !s.is_empty());
        }
        if let Some(f) = change.filter {
            def.filter = crate::filter::parser::parse_filter(&f)
                .map_err(|e| RegistryError::PolicyRejected(format!("invalid filter: {e}")))?;
            def.filter_string = f;
        }
        if let Some(l) = change.level {
            def.level = l;
        }
        if let Some(k) = change.group_keys {
            def.group_keys = k;
        }
        if let Some(b) = change.max_sample_bytes {
            def.max_sample_bytes = b;
        }

        if structural {
            // Re-check the reservation on EVERY structural edit. Three paths
            // used to walk past it: a free `max_sample_bytes` raise, a
            // scalar→tree raise creating a sample tier with no reservation at
            // all, and a policy change altering retained bytes.
            let reserved: usize = g
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, e)| e.collector.def().max_sample_bytes)
                .sum();
            let remaining = self.max_total_sample_bytes.saturating_sub(reserved);
            if def.max_sample_bytes > remaining {
                return Err(RegistryError::BudgetExceeded {
                    requested: def.max_sample_bytes,
                    remaining,
                    total: self.max_total_sample_bytes,
                });
            }
        }

        let target_domain = match change.domain {
            None => g[idx].domain.clone(),
            Some(d) => {
                // Re-pinnable only while zeroed. §10 makes a collector pinned
                // to an API-created domain always orphaned after a restart, so
                // without a re-pin the sanctioned response to the design's own
                // default failure mode would be deleting the collector and its
                // history. A restored collector is zeroed by definition.
                if !g[idx].collector.is_zeroed() && !structural {
                    return Err(RegistryError::PolicyRejected(
                        "a collector can only be re-pinned to another domain while it is \
                         zeroed; snapshot or reset it first, so the recorded window and \
                         the domain it was measured on cannot disagree"
                            .into(),
                    ));
                }
                d
            }
        };

        // **Persist before mutating** (§7.1). Three rules collide here: the
        // swap wants the lock, I/O must not decide correctness under it, and a
        // destructive change must not commit until the write succeeds. Writing
        // afterwards would mean an edit whose persist failed had zeroed the
        // live collector while the on-disk definition still held the old
        // filter — so a restart would resurrect a definition the caller
        // believes they replaced.
        if let Some(dir) = &self.dir {
            let file = g[idx].persisted_with(&def, &target_domain);
            crate::collector::persist::save(dir, &file).map_err(|e| {
                RegistryError::PersistFailed(format!(
                    "the edit was not applied because it could not be made durable: {e}"
                ))
            })?;
        }

        let entry = &mut g[idx];
        entry.domain = target_domain;
        if structural {
            entry.collector = Arc::new(Collector::new(def, now));
            entry.ingest_baseline = entry.metrics.trace_ingest_loss();
            entry.zeroed_by = Some("edit");
        } else {
            // A description change must not zero anything, so the definition
            // is replaced over the same live data.
            entry.collector = Arc::new(entry.collector.with_def(def));
        }
        Ok(EditOutcome {
            zeroed: structural,
            collector: entry.collector.clone(),
            domain: entry.domain.clone(),
        })
    }

    pub fn get(&self, owner: &SessionId, name: &str) -> Option<ArmedCollector> {
        let g = self.entries.read().expect("registry lock poisoned");
        g.iter()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .map(Entry::armed)
    }

    pub fn list(&self, owner: &SessionId) -> Vec<ArmedCollector> {
        let g = self.entries.read().expect("registry lock poisoned");
        g.iter()
            .filter(|e| &e.owner == owner)
            .map(Entry::armed)
            .collect()
    }

    /// Discard a collector's data and start a fresh window.
    ///
    /// Under the registry's **write** lock, which is the lock `ingest_span`
    /// takes for reading — so the data, the window, and the ingest baseline all
    /// move together and no span can land between the swap and the re-baseline.
    /// Returns what was discarded, so a caller that wanted the run can still
    /// have it.
    pub fn reset(
        &self,
        owner: &SessionId,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<CollectorSnapshot, RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let e = g
            .iter_mut()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let taken = e.collector.swap(now);
        e.ingest_baseline = e.metrics.trace_ingest_loss();
        e.zeroed_by = Some("reset");
        Ok(taken)
    }

    /// Record the current window as a snapshot, and by default start a fresh
    /// one (§6.1).
    ///
    /// Split into two locked steps with the projection computed **between**
    /// them: projecting means sorting every retained duration and walking the
    /// parent map, which at a million samples is long enough that doing it
    /// under the registry's write lock would stall ingest for the duration.
    /// `take` is atomic, which is the part that matters — the recorded window
    /// and the fresh one cannot overlap or lose a span.
    pub fn snapshot<F>(
        &self,
        owner: &SessionId,
        req: SnapshotRequest,
        project: F,
    ) -> Result<SnapshotOutcome, RegistryError>
    where
        F: FnOnce(&CollectorSnapshot) -> Option<ProfileSampled>,
    {
        let SnapshotRequest {
            name,
            label,
            description,
            meta,
            policy,
            reset,
            now,
        } = req;
        let name = name.as_str();
        // Step 1, under the write lock: reserve the label and take the data.
        let (view, ingest, label) = {
            let mut g = self.entries.write().expect("registry lock poisoned");
            let e = g
                .iter_mut()
                .find(|e| &e.owner == owner && e.collector.def().name == name)
                .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
            policy
                .validate(e.collector.def().level)
                .map_err(RegistryError::PolicyRejected)?;
            let label = e
                .history
                .reserve_label(label.as_deref())
                .map_err(RegistryError::Label)?;

            let current = e.metrics.trace_ingest_loss();
            let ingest = Some(current.since(e.ingest_baseline));
            let view = if reset {
                let taken = e.collector.swap(now);
                e.ingest_baseline = current;
                // Named for the operation the caller asked for, not for the
                // swap underneath it. "snapshot" tells a reader the run was
                // KEPT; "reset" would say the opposite of what happened.
                e.zeroed_by = Some("snapshot");
                taken
            } else {
                e.collector.snapshot()
            };
            (view, ingest, label)
        };

        // Step 2, outside every lock: the expensive part.
        let projections = policy.projections.then(|| project(&view)).flatten();

        // Step 3: file it.
        let snap = StoredSnapshot::from_view(
            label,
            description,
            meta,
            now,
            policy,
            view,
            ingest,
            projections,
        );
        let mut g = self.entries.write().expect("registry lock poisoned");
        let e = g
            .iter_mut()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        e.history.push(snap.clone());
        // §6.1 says "does not zero unless the write succeeded". This deviates,
        // deliberately, because the swap above is what makes the recorded
        // window and the fresh one unable to overlap or lose a span — deferring
        // it until after the write would let spans arriving in between fall
        // into the gap. The clause exists so a failed write cannot LOSE the
        // run, and here it cannot: the run is returned to the caller and is in
        // the in-memory history, which the next successful write persists. What
        // is genuinely at risk is durability, so that is what gets reported.
        let persist_error = self.persist(e).err();
        Ok(SnapshotOutcome {
            snapshot: snap,
            persist_error,
        })
    }

    /// Recorded runs, oldest first, plus how many the cap has dropped.
    pub fn history(
        &self,
        owner: &SessionId,
        name: &str,
    ) -> Result<(Vec<StoredSnapshot>, u64), RegistryError> {
        let g = self.entries.read().expect("registry lock poisoned");
        let e = g
            .iter()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        Ok((e.history.all().to_vec(), e.history.evicted()))
    }

    pub fn get_snapshot(
        &self,
        owner: &SessionId,
        name: &str,
        label: &str,
    ) -> Result<StoredSnapshot, RegistryError> {
        let g = self.entries.read().expect("registry lock poisoned");
        let e = g
            .iter()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        e.history.get(label).cloned().map_err(RegistryError::Label)
    }

    pub fn remove(&self, owner: &SessionId, name: &str) -> Result<(), RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let before = g.len();
        g.retain(|e| !(&e.owner == owner && e.collector.def().name == name));
        if g.len() == before {
            return Err(RegistryError::NotFound(name.to_string()));
        }
        // Removal takes the history with it. §10 could not have it both ways:
        // if disposal took history AND snapshots outlived the session, a
        // daemon whose sessions are swept every 24 hours would leak files
        // forever.
        self.delete_file(owner, name);
        Ok(())
    }

    fn delete_file(&self, owner: &SessionId, name: &str) {
        if let Some(dir) = &self.dir {
            crate::collector::persist::delete(dir, &owner.to_string(), name);
        }
    }

    /// Drop every collector owned by a session — the lifecycle counterpart of
    /// session disposal.
    pub fn drop_session(&self, owner: &SessionId) -> usize {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let doomed: Vec<String> = g
            .iter()
            .filter(|e| &e.owner == owner)
            .map(|e| e.collector.def().name.clone())
            .collect();
        g.retain(|e| &e.owner != owner);
        for name in &doomed {
            self.delete_file(owner, name);
        }
        doomed.len()
    }

    /// Bytes reserved across every armed collector.
    pub fn reserved_bytes(&self) -> usize {
        self.entries
            .read()
            .expect("registry lock poisoned")
            .iter()
            .map(|e| e.collector.def().max_sample_bytes)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .expect("registry lock poisoned")
            .is_empty()
    }

    /// The ingest path. Offers one span to every collector pinned to `domain`
    /// whose filter matches it.
    ///
    /// Matching uses the collector's **pre-parsed** filter, so no parsing
    /// happens per span — the defect that cost 28 µs per span per session on
    /// the trigger path.
    pub fn ingest_span(&self, domain: &DomainId, span: &SpanEntry) {
        let g = self.entries.read().expect("registry lock poisoned");
        if g.is_empty() {
            return;
        }
        for e in g.iter() {
            if &e.domain != domain {
                continue;
            }
            if matches_span(&e.collector.def().filter, span) {
                e.collector.ingest(span);
            }
        }
    }
}

impl Default for CollectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::sample::Level;
    use crate::filter::parser::parse_filter;
    use crate::span::types::{SpanKind, SpanStatus};
    use chrono::TimeZone;
    use std::collections::HashMap;

    const S: i64 = 1_700_000_000_000_000_000;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_nanos(S)
    }

    fn metrics() -> Arc<ReceiverMetrics> {
        Arc::new(ReceiverMetrics::new())
    }

    fn def(name: &str, filter: &str, bytes: usize) -> CollectorDef {
        CollectorDef {
            name: name.into(),
            filter_string: filter.into(),
            filter: parse_filter(filter).expect("valid"),
            level: Level::Tree,
            group_keys: vec![],
            max_sample_bytes: bytes,
            description: None,
        }
    }

    fn span(service: &str) -> SpanEntry {
        SpanEntry {
            seq: 0,
            trace_id: 1,
            span_id: 2,
            parent_span_id: None,
            start_time: Utc.timestamp_nanos(S),
            end_time: Utc.timestamp_nanos(S + 1_000),
            duration_ms: 0.001,
            name: "op".into(),
            kind: SpanKind::Internal,
            service_name: service.into(),
            status: SpanStatus::Ok,
            attributes: HashMap::new(),
            events: vec![],
        }
    }

    fn sid(n: &str) -> SessionId {
        SessionId::Named(n.to_string())
    }

    fn dom(n: &str) -> DomainId {
        DomainId::new(n).expect("valid domain name")
    }

    #[test]
    fn only_matching_spans_in_the_pinned_domain_are_collected() {
        let r = CollectorRegistry::new();
        let d_a = dom("a");
        let d_b = dom("b");
        let c = r
            .add(
                &sid("s1"),
                &d_a,
                metrics(),
                def("c", "sv=svc", 1 << 20),
                now(),
            )
            .expect("armed");

        r.ingest_span(&d_a, &span("svc")); // matches, right domain
        r.ingest_span(&d_a, &span("other")); // wrong service
        r.ingest_span(&d_b, &span("svc")); // right service, wrong domain

        assert_eq!(c.snapshot().total.count, 1);
    }

    #[test]
    fn a_collector_keeps_its_pinned_domain_regardless_of_its_owner() {
        // §4.4: the pin is the collector's, not the session's. A session that
        // later binds elsewhere must not silently stop its collector.
        let r = CollectorRegistry::new();
        let pinned = dom("t3");
        let c = r
            .add(
                &sid("s1"),
                &pinned,
                metrics(),
                def("c", "ALL", 1 << 20),
                now(),
            )
            .expect("armed");

        r.ingest_span(&pinned, &span("svc"));
        r.ingest_span(&DomainId::default_domain(), &span("svc"));

        assert_eq!(
            c.snapshot().total.count,
            1,
            "only the pinned domain feeds it"
        );
    }

    #[test]
    fn duplicate_names_are_rejected_per_session_not_globally() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .expect("first");
        assert_eq!(
            r.add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
                .expect_err("a duplicate name in one session is refused"),
            RegistryError::DuplicateName("c".into())
        );
        // A different session may reuse the name.
        assert!(r
            .add(&sid("s2"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .is_ok());
    }

    #[test]
    fn the_daemon_budget_is_a_reservation_checked_at_arm_time() {
        let r = CollectorRegistry::with_budget(100);
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("a", "ALL", 60), now())
            .expect("fits");
        let err = r
            .add(&sid("s1"), &d, metrics(), def("b", "ALL", 60), now())
            .expect_err("does not fit");
        assert_eq!(
            err,
            RegistryError::BudgetExceeded {
                requested: 60,
                remaining: 40,
                total: 100
            },
            "the error must say what would fit, not only that it did not"
        );
        // Removing frees the reservation.
        r.remove(&sid("s1"), "a").expect("removed");
        assert!(r
            .add(&sid("s1"), &d, metrics(), def("b", "ALL", 60), now())
            .is_ok());
    }

    #[test]
    fn removal_and_session_disposal_stop_collection() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        let c = r
            .add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .expect("armed");
        r.ingest_span(&d, &span("svc"));
        assert_eq!(c.snapshot().total.count, 1);

        r.remove(&sid("s1"), "c").expect("removed");
        r.ingest_span(&d, &span("svc"));
        assert_eq!(
            c.snapshot().total.count,
            1,
            "a removed collector receives nothing more"
        );
        assert_eq!(r.reserved_bytes(), 0, "and its reservation is released");

        r.add(&sid("s2"), &d, metrics(), def("x", "ALL", 1 << 20), now())
            .expect("armed");
        assert_eq!(r.drop_session(&sid("s2")), 1);
        assert!(r.is_empty());
    }

    #[test]
    fn removing_an_unknown_collector_is_an_error_not_a_silent_no_op() {
        let r = CollectorRegistry::new();
        assert_eq!(
            r.remove(&sid("s1"), "nope"),
            Err(RegistryError::NotFound("nope".into()))
        );
    }

    #[test]
    fn list_and_get_are_scoped_to_the_owning_session() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("a", "ALL", 1 << 20), now())
            .unwrap();
        r.add(&sid("s1"), &d, metrics(), def("b", "ALL", 1 << 20), now())
            .unwrap();
        r.add(&sid("s2"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .unwrap();

        assert_eq!(r.list(&sid("s1")).len(), 2);
        assert_eq!(r.list(&sid("s2")).len(), 1);
        assert!(r.get(&sid("s1"), "c").is_none(), "no cross-session access");
        assert!(r.get(&sid("s2"), "c").is_some());
    }
}
