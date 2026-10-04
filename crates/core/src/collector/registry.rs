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
use crate::collector::project::Projected;
use crate::collector::state::{Collector, CollectorDef, CollectorSnapshot};
use crate::daemon::domain::DomainId;
use crate::daemon::session::SessionId;
use crate::filter::matcher::matches_span;
use crate::receiver::{ReceiverMetrics, TraceIngestLoss};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

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
    /// This collector's identity for the life of the daemon — unique among every collector it
    /// has armed or restored. A name is not one: a collector removed and re-armed under the
    /// same name is a different collector, and a write prepared for the first, landing after
    /// the second was armed, put the removed collector's file back under the new one's name.
    id: u64,
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
    /// The session that armed it. Carried because a case document selects
    /// across owners (§5.4) and a reader needs to know whose collector a
    /// number came from — the CLI and the shim are different sessions on the
    /// same domain.
    pub owner: SessionId,
    /// Label and instant of the most recent recorded run, if any. §5.4 wants
    /// each collector's current numbers **and any snapshot it holds**, and
    /// `snapshot_count` alone cannot say when the last one was taken — which is
    /// the part that tells a reader whether it predates the build they are
    /// looking at.
    pub latest_snapshot: Option<(String, chrono::DateTime<chrono::Utc>)>,
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
            owner: self.owner.clone(),
            collector: self.collector.clone(),
            domain: self.domain.clone(),
            metrics: self.metrics.clone(),
            ingest_baseline: self.ingest_baseline,
            zeroed_by: self.zeroed_by,
            snapshot_count: self.history.len(),
            latest_snapshot: self
                .history
                .all()
                .last()
                .map(|s| (s.label.clone(), s.taken_at)),
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
            threshold: def
                .threshold
                .as_ref()
                .map(crate::collector::persist::PersistedThreshold::of),
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
    /// `Some(None)` clears an armed threshold; `Some(Some(t))` sets one. Nested
    /// because "leave it alone" and "remove it" are different requests, and a
    /// flat `Option` cannot express both.
    pub threshold: Option<Option<crate::collector::threshold::Threshold>>,
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
            // The rolling ring IS measurement state, so changing the limit it is
            // evaluated against zeroes the window on the same terms as changing
            // the filter (§7.1). Evaluating a new limit over a window
            // accumulated under the old one would breach for reasons unrelated
            // to load.
            || self
                .threshold
                .as_ref()
                .is_some_and(|t| *t != current.threshold)
    }

    pub fn is_empty(&self) -> bool {
        self.description.is_none()
            && self.filter.is_none()
            && self.level.is_none()
            && self.group_keys.is_none()
            && self.max_sample_bytes.is_none()
            && self.domain.is_none()
            && self.threshold.is_none()
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
    /// Readable files set aside because another copy of the same collector won.
    pub superseded: Vec<(PathBuf, String)>,
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
    /// Who wrote each collector file last, and the lock its writes and deletes take.
    files: FileLedger,
    /// The next [`Entry::id`].
    next_id: std::sync::atomic::AtomicU64,
}

/// The collector files' bookkeeping. Each path has a lock that its writes and deletes take,
/// guarding the sequence number of the path's last successful write (0: none in this run). A
/// write's number is taken when it finds its collector live, not when it finishes — so it says
/// whether the write is ordered before or after a removal, however long its save takes.
///
/// A deferred delete is decided by BYTES, not by who holds the path: it carries the sequence
/// number current when the delete was decided, and removes the file only if no write ordered
/// after that has succeeded on the path. Bytes written since belong to whoever wrote them — a new holder of
/// the name, a collector that moved onto the path — not to the collector being deleted; bytes
/// not rewritten are still that collector's, whoever holds the name now (kept, a restart would
/// restore a dead collector under a live name). Deciding by liveness got both wrong: a new
/// holder whose own write had FAILED made the dead bytes read as held.
///
/// Per path, so one collector's slow fsync never waits on another's, and never by ingest. Never
/// taken while `entries` is held: writes and deletes run outside that lock by design (a write
/// takes `entries` for reading INSIDE its path lock, to check the collector still exists).
#[derive(Default)]
struct FileLedger {
    paths: Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<u64>>>>,
    seq: std::sync::atomic::AtomicU64,
}

impl FileLedger {
    fn path_lock(&self, path: &std::path::Path) -> Arc<Mutex<u64>> {
        self.paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(path.to_path_buf())
            .or_default()
            .clone()
    }

    /// Drop `path`'s entry once its file is gone, so per-launch session names do not grow the
    /// map for the daemon's life. Only when no one else holds the entry: `lock` is the caller's
    /// clone and the map's is the other, and new clones are made only under the map lock held
    /// here. A later write starts a fresh entry, its stamp from the same global sequence.
    ///
    /// **Called with the path's lock still held.** Released first, a write already waiting on
    /// it could land — its stamp in this entry — and finish before the count was read, and the
    /// entry went with its stamp: a stale delete arriving later found a fresh entry at 0 and
    /// removed a live collector's only file. Holding it here cannot deadlock: nothing waits on
    /// a path's lock while holding the map's.
    fn forget_if_unused(&self, path: &std::path::Path, lock: &Arc<Mutex<u64>>) {
        let mut paths = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(lock) == 2 {
            paths.remove(path);
        }
    }

    /// The sequence number of the latest write so far — what a deferred delete carries.
    fn now(&self) -> u64 {
        self.seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn next(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }
}

/// File work a lifecycle change left for after the caller's locks — see
/// [`CollectorRegistry::finish`]. Dropped unfinished, its files stay on disk (to be restored
/// at the next boot), so it is `must_use`.
#[derive(Default)]
#[must_use = "pass it to CollectorRegistry::finish once the caller's locks are released"]
pub struct PendingFiles {
    /// `(old owner, new owner, name, ledger seq at the move)` of each collector that moved.
    moved: Vec<(SessionId, SessionId, String, u64)>,
    /// `(owner, name, ledger seq at the detach)` of each detached collector's file.
    unlink: Vec<(SessionId, String, u64)>,
    /// The detached collectors themselves, so their memory is freed off every lock.
    detached: Vec<Entry>,
}

impl PendingFiles {
    /// Fold `other`'s work into this one.
    pub fn absorb(&mut self, other: PendingFiles) {
        self.moved.extend(other.moved);
        self.unlink.extend(other.unlink);
        self.detached.extend(other.detached);
    }
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
            files: FileLedger::default(),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn new_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
        now: DateTime<Utc>,
        metrics_for: impl Fn(&DomainId) -> Arc<ReceiverMetrics>,
    ) -> RestoreReport {
        let Some(dir) = &self.dir else {
            return RestoreReport::default();
        };
        let outcome = crate::collector::persist::load_all(dir);
        let mut report = RestoreReport {
            quarantined: outcome.quarantined,
            superseded: outcome.superseded,
            ..Default::default()
        };
        let mut g = self.entries.write().expect("registry lock poisoned");
        for file in outcome.collectors {
            match Self::entry_from_file(&file, now, &metrics_for, self.new_id()) {
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
        now: DateTime<Utc>,
        metrics_for: &impl Fn(&DomainId) -> Arc<ReceiverMetrics>,
        id: u64,
    ) -> Result<Entry, String> {
        let level = match file.level.as_str() {
            "scalar" => crate::collector::sample::Level::Scalar,
            "timing" => crate::collector::sample::Level::Timing,
            "tree" => crate::collector::sample::Level::Tree,
            other => return Err(format!("unknown level `{other}`")),
        };
        let filter = crate::filter::parser::parse_filter(&file.filter)
            .map_err(|e| format!("recorded filter `{}` no longer parses: {e}", file.filter))?;
        // `collectors.add`/`edit` refuse a bookmark or cursor qualifier — it never matches in a
        // registered filter, so the collector would measure nothing — but an earlier version
        // accepted one, and its file is still on disk.
        if crate::filter::parser::contains_bookmark_qualifier(&filter) {
            return Err(format!(
                "recorded filter `{}` carries a bookmark or cursor qualifier, which never \
                 matches in a collector",
                file.filter
            ));
        }
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
            threshold: match &file.threshold {
                None => None,
                Some(t) => match t.restore() {
                    Ok(t) => Some(t),
                    // A threshold this build cannot represent must not take the
                    // collector down with it: the definition and its whole
                    // history are still restorable, and a guard that stopped
                    // existing is a smaller loss than a run that did.
                    Err(e) => {
                        tracing::warn!(
                            collector = %file.name,
                            error = %e,
                            "dropping a recorded threshold this build cannot represent; the \
                             collector and its history were restored without it"
                        );
                        None
                    }
                },
            },
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
            id,
            owner: SessionId::Named(file.owner.clone()),
            domain,
            // Armed at the ORIGINAL time, so `armed_at` still says when this
            // measurement began rather than when the daemon last booted — but
            // the WINDOW starts now. Those are two different instants, and
            // passing only the first left `zeroed_at` unset, so `window_start`
            // fell through to `armed_at` and `wall_ms` spanned the outage with a
            // span count that skipped it.
            collector: Arc::new(Collector::new_restored(def, file.armed_at, now)),
            ingest_baseline: metrics.trace_ingest_loss(),
            metrics,
            history,
            zeroed_by: Some("daemon_restart"),
        })
    }

    /// Write a prepared file. Returns the failure rather than swallowing it, so
    /// each caller can decide: `add` arms anyway, `snapshot` reports the loss of
    /// durability, `edit` refuses outright.
    ///
    /// **Takes the prepared file, not the entry, so it can be called with no
    /// lock held.** `atomic_write` does two `fsync`s, and `ingest_span` takes
    /// this registry's lock for reading on every domain's span processor — so a
    /// write under the lock lets one slow fsync stall span ingest daemon-wide
    /// until the 65 536-slot channel overflows and starts dropping. Bookkeeping
    /// must not be able to cost telemetry.
    ///
    /// Returns `Ok(true)` when written, `Ok(false)` when the collector no longer exists where
    /// the file was prepared for (nothing to persist: it was removed, or moved and its mover
    /// writes it), `Err` when the write failed. `id` is the [`Entry::id`] of the collector
    /// `file` was prepared from.
    ///
    /// A collector that no longer exists is not written: a write prepared before a removal and
    /// landing after its delete put the file back, and the removed collector returned at the
    /// next boot. "Exists" is by identity and owner, never by name: a collector removed and
    /// re-armed under its name is another collector, and the file prepared for the first would
    /// otherwise overwrite the second's. The write's place in the [`FileLedger`] order is
    /// taken at the moment it finds the collector live, in the same `entries` guard — a removal
    /// decides its delete under that lock's write side, so a write that saw the collector
    /// before the removal is ordered before it (and its file is the removal's to delete), and
    /// one ordered after it cannot have seen the collector. Stamped after the save instead, a
    /// write that was under way when a removal came in looked newer than the removal, and its
    /// file survived it.
    fn write(
        &self,
        id: u64,
        file: &crate::collector::persist::PersistedCollector,
    ) -> Result<bool, String> {
        self.write_pausing(id, file, || {})
    }

    /// [`Self::write`], running `between` after the write has taken its place in the order and
    /// before it saves — the window a slow fsync holds open — so a test can drive a removal
    /// into it.
    fn write_pausing(
        &self,
        id: u64,
        file: &crate::collector::persist::PersistedCollector,
        between: impl FnOnce(),
    ) -> Result<bool, String> {
        let Some(dir) = &self.dir else {
            return Ok(true);
        };
        let path = crate::collector::persist::collector_path(dir, &file.owner, &file.name);
        let lock = self.files.path_lock(&path);
        let mut last = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Every collector owner is a named session (an anonymous one cannot own one).
        let owner = SessionId::Named(file.owner.clone());
        let stamp = {
            let g = self.entries.read().expect("registry lock poisoned");
            g.iter()
                .any(|e| e.id == id && e.owner == owner)
                .then(|| self.files.next())
        };
        let Some(stamp) = stamp else {
            // Nothing to write. `path_lock` may have just made this path's entry; it goes again
            // unless it records a write — a stamp a later delete needs — or someone else holds
            // it. Kept, every such write grew the map for the daemon's life.
            if *last == 0 {
                self.files.forget_if_unused(&path, &lock);
            }
            return Ok(false);
        };
        between();
        match crate::collector::persist::save(dir, file) {
            Ok(()) => {
                *last = stamp;
                Ok(true)
            }
            Err(e) => {
                tracing::error!(
                    collector = %file.name, owner = %file.owner, error = %e,
                    "could not persist collector; a restart restores its last saved state, if any"
                );
                Err(e.to_string())
            }
        }
    }

    /// Delete `owner`'s file for `name` unless a write to that path ordered after `since` (a
    /// [`FileLedger::now`] taken when the delete was decided) has succeeded — see
    /// [`FileLedger`]. Only the canonical path: a copy at another name — one the boot migration
    /// could not move into place, or a moved collector's old file — is not this delete's to
    /// find, and comes back at the next boot.
    fn delete_unless_written_since(&self, owner: &SessionId, name: &str, since: u64) {
        self.delete_pausing(owner, name, since, || {});
    }

    /// [`Self::delete_unless_written_since`], running `before_forget` once the file is gone
    /// and before the path's entry may be forgotten — so a test can start a write on the path
    /// in that window.
    fn delete_pausing(
        &self,
        owner: &SessionId,
        name: &str,
        since: u64,
        before_forget: impl FnOnce(),
    ) {
        let Some(dir) = &self.dir else { return };
        let path = crate::collector::persist::collector_path(dir, &owner.to_string(), name);
        let lock = self.files.path_lock(&path);
        let last = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *last > since {
            return;
        }
        if !crate::collector::persist::delete(dir, &owner.to_string(), name) && path.exists() {
            tracing::error!(
                ?path,
                "could not delete a removed collector's file; it will be restored at the next boot"
            );
            return;
        }
        // The path's entry goes with its file — unless another caller holds it, in which case
        // it stays for them (and the map lock keeps anyone new from taking it meanwhile).
        before_forget();
        self.files.forget_if_unused(&path, &lock);
        drop(last);
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
        let id = self.new_id();
        g.push(Entry {
            id,
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
        //
        // Serialized under the lock, written outside it: the write does two
        // fsyncs and this lock gates every domain's ingest.
        let file = g.last().expect("just pushed").to_persisted();
        drop(g);
        let _ = self.write(id, &file);
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
    /// `new_metrics` must be the re-pinned domain's counters whenever
    /// `change.domain` is set — the caller has already resolved that domain to
    /// check it exists, so it is the only place that can supply them.
    pub fn edit(
        &self,
        owner: &SessionId,
        name: &str,
        change: CollectorEdit,
        new_metrics: Option<Arc<ReceiverMetrics>>,
        now: DateTime<Utc>,
    ) -> Result<EditOutcome, RegistryError> {
        // Read-only in this first phase: everything is validated and the file is
        // built before anything is mutated, so the write can be the commit point.
        let g = self.entries.write().expect("registry lock poisoned");
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
        // The outer `Some` is "the caller asked about this field"; the inner one
        // is the value. `Some(None)` therefore removes an armed guard, which a
        // flat `Option` could not express — and forgetting this arm is how the
        // edit reported `zeroed: true` while keeping the old limit.
        if let Some(t) = change.threshold {
            def.threshold = t;
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
        let file = g[idx].persisted_with(&def, &target_domain);
        let id = g[idx].id;
        drop(g);
        let written = self.write(id, &file).map_err(|e| {
            RegistryError::PersistFailed(format!(
                "the edit was not applied because it could not be made durable: {e}"
            ))
        })?;
        if !written {
            return Err(RegistryError::NotFound(name.to_string()));
        }

        // Re-acquire and re-find: the write happened with no lock held, so the
        // collector could have been removed under us. Nothing has been mutated
        // yet, so reporting it gone is accurate rather than a partial edit. By
        // identity: a collector re-armed under the name since is not the one whose
        // file was just written, and editing it would leave its memory and its
        // file disagreeing.
        let mut g = self.entries.write().expect("registry lock poisoned");
        let idx = g
            .iter()
            .position(|e| e.id == id && &e.owner == owner)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let entry = &mut g[idx];
        entry.domain = target_domain;
        // The metrics handle must move with the pin. Left behind, the identity
        // check in `ingest_basis` fails forever and every later read reports
        // "the pinned domain is gone or was recreated" about a domain that is
        // present and was just re-pinned to — on the design's own sanctioned
        // repair path for an orphaned collector.
        if let Some(m) = new_metrics {
            entry.metrics = m;
        }
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

    /// Every collector pinned to `domain`, **whoever armed it** — the selection
    /// a case document needs (§5.4).
    ///
    /// "The calling session's collectors" is wrong twice here. `ArmedCollector.
    /// domain` is a pin that a later `use_domain` does not move, so an
    /// owner-scoped list can carry collectors measuring a *different* domain;
    /// and the CLI connects as session `cli` while the shim uses its own, so an
    /// MCP-created case would see none of the CLI's collectors on the very
    /// domain it is about.
    ///
    /// Returns the `ArmedCollector`s and nothing derived, so projection — which
    /// sorts every retained duration — happens after this guard drops rather
    /// than under a lock the ingest path takes.
    pub fn list_for_domain(&self, domain: &DomainId) -> Vec<ArmedCollector> {
        let g = self.entries.read().expect("registry lock poisoned");
        g.iter()
            .filter(|e| &e.domain == domain)
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
        F: FnOnce(&CollectorSnapshot) -> Projected,
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
        let (view, ingest, label, id) = {
            let mut g = self.entries.write().expect("registry lock poisoned");
            let e = g
                .iter_mut()
                .find(|e| &e.owner == owner && e.collector.def().name == name)
                .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
            let id = e.id;
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
            (view, ingest, label, id)
        };

        // Step 2, outside every lock: the expensive part.
        let projected = if policy.projections {
            project(&view)
        } else {
            Projected::default()
        };

        // Step 3: file it.
        let snap = StoredSnapshot::from_view(
            label,
            description,
            meta,
            now,
            policy,
            view,
            ingest,
            projected,
        );
        // The window was already taken in step 1, so a collector removed under
        // us must NOT turn into an error: that would discard a run the caller
        // asked to keep and which exists nowhere else. Return it instead, and
        // say it could not be filed. Found by identity, not by owner: a collector whose
        // session was renamed since step 1 is the same collector, and its history is where
        // the run belongs — looked up by the old owner, it read as removed and the run
        // was dropped from a history that still existed.
        let mut g = self.entries.write().expect("registry lock poisoned");
        let file = match g.iter_mut().find(|e| e.id == id) {
            Some(e) => {
                e.history.push(snap.clone());
                Some(e.to_persisted())
            }
            None => None,
        };
        drop(g);
        let Some(file) = file else {
            return Ok(SnapshotOutcome {
                snapshot: snap,
                persist_error: Some(
                    "the collector was removed while this snapshot was being taken, so the \
                     run is in this response only — it is in no history and was not written"
                        .into(),
                ),
            });
        };
        // §6.1 says "does not zero unless the write succeeded". This deviates,
        // deliberately, because the swap above is what makes the recorded
        // window and the fresh one unable to overlap or lose a span — deferring
        // it until after the write would let spans arriving in between fall
        // into the gap. The clause exists so a failed write cannot LOSE the
        // run, and here it cannot: the run is returned to the caller and is in
        // the in-memory history, which the next successful write persists. What
        // is genuinely at risk is durability, so that is what gets reported.
        let written = self.write(id, &file);
        let persist_error = self.snapshot_persist_error(id, written);
        Ok(SnapshotOutcome {
            snapshot: snap,
            persist_error,
        })
    }

    /// What [`Self::snapshot`] reports about filing a run into collector `id`, given what its
    /// write said.
    ///
    /// A write that found the collector gone from where it was prepared for is two different
    /// outcomes. Moved (its session renamed since): the run is in its history, and its mover
    /// writes the collector's current state, run included — nothing to report here (a failure
    /// of that write is the mover's to log). Removed: the run is in no history and was not
    /// written, exactly as when the removal came before the run was filed — so it is reported
    /// the same way, never as filed.
    fn snapshot_persist_error(&self, id: u64, written: Result<bool, String>) -> Option<String> {
        match written {
            Ok(true) => None,
            Err(e) => Some(e),
            Ok(false) => {
                let kept = self
                    .entries
                    .read()
                    .expect("registry lock poisoned")
                    .iter()
                    .any(|e| e.id == id);
                (!kept).then(|| {
                    "the collector was removed while this snapshot was being written, so the \
                     run is in this response only — it is in no history and was not written"
                        .to_string()
                })
            }
        }
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
        // The file is deleted after the lock — file I/O never runs under it — unless a write
        // ordered after the removal has succeeded on the path (see `FileLedger`).
        let since = self.files.now();
        drop(g);
        // Removal takes the history with it. §10 could not have it both ways:
        // if disposal took history AND snapshots outlived the session, a
        // daemon whose sessions are swept every 24 hours would leak files
        // forever.
        self.delete_unless_written_since(owner, name, since);
        Ok(())
    }

    /// Move every collector owned by `old` to `new`, rewriting their files.
    ///
    /// A rename that left `Entry::owner` behind would orphan the session's own
    /// collectors: invisible to its owner-scoped `list`, unreachable by
    /// `sessions.drop` (the old name no longer resolves), and untouched by the
    /// TTL sweep (it iterates the session map) — while still holding their
    /// share of a daemon-wide reservation that only four collectors fit inside.
    pub fn rename_owner(&self, old: &SessionId, new: &SessionId) -> usize {
        let (moved, files) = self.move_owner(old, new);
        self.finish(files);
        moved
    }

    /// [`Self::rename_owner`]'s in-memory half: the entries change owner now, and the files
    /// move when the returned work is [`finish`](Self::finish)ed. Split so a caller can make the
    /// ownership change under its own lock and the file I/O after it. A rename to the
    /// session's own name moves nothing — taken through the move, each collector's file was
    /// written and then deleted at the same path, so every collector was gone at the next
    /// restart.
    pub fn move_owner(&self, old: &SessionId, new: &SessionId) -> (usize, PendingFiles) {
        let mut pending = PendingFiles::default();
        if old == new {
            return (0, pending);
        }
        let mut g = self.entries.write().expect("registry lock poisoned");
        let since = self.files.now();
        for e in g.iter_mut().filter(|e| &e.owner == old) {
            e.owner = new.clone();
            pending.moved.push((
                old.clone(),
                new.clone(),
                e.collector.def().name.clone(),
                since,
            ));
        }
        (pending.moved.len(), pending)
    }

    /// Drop every collector owned by a session — the lifecycle counterpart of
    /// session disposal.
    pub fn drop_session(&self, owner: &SessionId) -> usize {
        let (dropped, files) = self.detach_session(owner);
        self.finish(files);
        dropped
    }

    /// [`Self::drop_session`]'s in-memory half: the session's collectors leave the registry
    /// (and their reservation is released) now; their files are unlinked, and their memory
    /// freed, when the returned work is [`finish`](Self::finish)ed.
    pub fn detach_session(&self, owner: &SessionId) -> (usize, PendingFiles) {
        let mut pending = PendingFiles::default();
        let mut g = self.entries.write().expect("registry lock poisoned");
        let since = self.files.now();
        let (doomed, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut *g)
            .into_iter()
            .partition(|e| &e.owner == owner);
        *g = kept;
        for e in &doomed {
            pending
                .unlink
                .push((owner.clone(), e.collector.def().name.clone(), since));
        }
        pending.detached = doomed;
        (pending.unlink.len(), pending)
    }

    /// Do the file work [`Self::detach_session`] and [`Self::move_owner`] left, with no lock
    /// held: file I/O on the path that gates every domain's ingest — here or under the caller's
    /// lock — lets one slow disk stall telemetry daemon-wide.
    ///
    /// Every delete here is decided by bytes ([`FileLedger`]): a path goes only if no write
    /// ordered after the detach or move has succeeded on it — whoever holds the name now. A moved collector is
    /// written to its new path from its CURRENT state, and its old file goes only once that
    /// write succeeded: until then it is the collector's only copy on disk. A detached
    /// collector's path that a moved collector took over (a rename displacing a stale holder of
    /// the same collector name) needs no special case: the mover's write, if it lands, is a
    /// write since the detach.
    pub fn finish(&self, pending: PendingFiles) {
        for (old, new, name, since) in &pending.moved {
            let file = self
                .entries
                .read()
                .expect("registry lock poisoned")
                .iter()
                .find(|e| &e.owner == new && &e.collector.def().name == name)
                .map(|e| (e.id, e.to_persisted()));
            let landed = match file {
                Some((id, file)) => self.write(id, &file).is_ok(),
                // Removed since it moved: its old file has nothing left to protect.
                None => true,
            };
            if landed {
                self.delete_unless_written_since(old, name, *since);
            }
        }
        for (owner, name, since) in &pending.unlink {
            self.delete_unless_written_since(owner, name, *since);
        }
        // `detached` is dropped here, freeing the detached collectors' samples off every lock.
    }

    /// Every distinct owner with at least one live collector.
    ///
    /// Exists so the daemon can guarantee an owner is a session it can still
    /// reach. A collector file is written through on `add`, but a named session
    /// only reaches `state.json` on graceful shutdown — so after a `kill -9` the
    /// collectors come back and their owner does not, leaving them invisible to
    /// an owner-scoped `list`, unreachable by `sessions.drop`, and untouched by
    /// the TTL sweep, while still holding their share of the reservation.
    pub fn owners(&self) -> Vec<SessionId> {
        let g = self.entries.read().expect("registry lock poisoned");
        let mut seen: Vec<SessionId> = Vec::new();
        for e in g.iter() {
            if !seen.contains(&e.owner) {
                seen.push(e.owner.clone());
            }
        }
        seen
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
            threshold: None,
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

    /// A detached collector's file is unlinked only if nothing holds its path when the work is
    /// finished: by then the name may have a new holder that armed a collector of the same
    /// name, and that file is the new collector's only copy on disk.
    #[test]
    fn finishing_a_detach_spares_a_file_a_new_holder_wrote() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        assert!(path.exists());

        let (detached, pending) = r.detach_session(&sid("s"));
        assert_eq!(detached, 1);
        // The name's next holder arms its own `c` before the detach's files are dealt with.
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed again");
        r.finish(pending);
        assert!(path.exists(), "the new holder's file survives");

        // The instrument fires: with no new holder, the file goes.
        let (_, pending) = r.detach_session(&sid("s"));
        r.finish(pending);
        assert!(!path.exists());
    }

    /// A moved collector's old file goes only once its new file is written, and the new file
    /// holds the collector as it is when the move is finished.
    #[test]
    fn finishing_a_move_writes_the_new_file_then_removes_the_old() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        r.add(
            &sid("old"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let (moved, pending) = r.move_owner(&sid("old"), &sid("new"));
        assert_eq!(moved, 1);
        r.finish(pending);
        assert!(crate::collector::persist::collector_path(d.path(), "new", "c").exists());
        assert!(!crate::collector::persist::collector_path(d.path(), "old", "c").exists());
    }

    /// Make the next write of `owner`'s `name` file fail: a directory where its temp file goes.
    fn block_write(dir: &std::path::Path, owner: &str, name: &str) {
        let path = crate::collector::persist::collector_path(dir, owner, name);
        let tmp = format!(
            "{}{}",
            path.display(),
            crate::daemon::persistence::TEMP_SUFFIX
        );
        std::fs::create_dir_all(tmp).unwrap();
    }

    /// A dropped session's collector file goes even though a NEW holder of the name has armed a
    /// collector of the same name — when that holder's own write failed, the bytes at the path
    /// are still the dropped collector's, and kept, a restart restored it under the live name.
    /// The path reads as held, which is why the delete is decided by writes, not by liveness.
    #[test]
    fn a_detach_removes_the_dead_bytes_under_a_new_holder_whose_write_failed() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let (_, pending) = r.detach_session(&sid("s"));
        block_write(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed again, its write failing");
        r.finish(pending);
        assert!(!path.exists(), "the dropped collector's bytes are gone");
    }

    /// A file prepared before its collector was removed, and written after the removal's
    /// delete, does not bring the collector back at the next boot.
    #[test]
    fn a_write_landing_after_a_removal_does_not_restore_the_file() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let (id, prepared) = prepared(&r, "c");
        r.remove(&sid("s"), "c").expect("removed");
        assert!(!path.exists());
        assert_eq!(
            r.write(id, &prepared),
            Ok(false),
            "not written: the collector is gone"
        );
        assert!(!path.exists(), "and the file stays gone");
        assert!(
            r.files.paths.lock().unwrap().is_empty(),
            "the path's entry the write made is not kept"
        );
    }

    /// `(id, prepared file)` of collector `name` as it stands.
    fn prepared(
        r: &CollectorRegistry,
        name: &str,
    ) -> (u64, crate::collector::persist::PersistedCollector) {
        r.entries
            .read()
            .unwrap()
            .iter()
            .find(|e| e.collector.def().name == name)
            .map(|e| (e.id, e.to_persisted()))
            .expect("the entry")
    }

    /// A file prepared for a collector that was then removed does not land on a collector
    /// re-armed under the same name: that is another collector, with its own file.
    #[test]
    fn a_write_prepared_for_a_removed_collector_spares_one_re_armed_under_its_name() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=old", 1 << 20),
            now(),
        )
        .expect("armed");
        let (id, stale) = prepared(&r, "c");
        r.remove(&sid("s"), "c").expect("removed");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=new", 1 << 20),
            now(),
        )
        .expect("re-armed");

        assert_eq!(r.write(id, &stale), Ok(false), "not the same collector");
        let on_disk = std::fs::read_to_string(&path).expect("the new collector's file");
        assert!(on_disk.contains("sv=new"), "{on_disk}");
        assert!(!on_disk.contains("sv=old"), "{on_disk}");
    }

    /// A rename between a snapshot's two steps leaves the run in the renamed collector's
    /// history: it is the same collector. Looked up by its old owner, it read as removed and
    /// the run went into no history.
    #[test]
    fn a_rename_during_a_snapshot_keeps_the_run_in_the_collector_s_history() {
        let r = CollectorRegistry::new();
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let outcome = r
            .snapshot(
                &sid("s"),
                SnapshotRequest {
                    name: "c".into(),
                    label: Some("run".into()),
                    description: None,
                    meta: serde_json::Value::Null,
                    policy: SnapshotPolicy::default(),
                    reset: false,
                    now: now(),
                },
                // Between the two locked steps.
                |_| {
                    let (moved, files) = r.move_owner(&sid("s"), &sid("t"));
                    assert_eq!(moved, 1);
                    r.finish(files);
                    Projected::default()
                },
            )
            .expect("taken");
        assert_eq!(outcome.persist_error, None);
        let (history, _) = r.history(&sid("t"), "c").expect("renamed");
        assert!(
            history.iter().any(|s| s.label == "run"),
            "the run was filed"
        );
    }

    /// A write that reaches a path while a delete of it is finishing keeps its stamp in the
    /// path's entry: forgotten after the delete let go of the path, the write could land
    /// first, and a stale delete arriving later found no stamp and removed the live file.
    #[test]
    fn a_write_waiting_on_a_deleted_path_keeps_its_stamp() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=old", 1 << 20),
            now(),
        )
        .expect("armed");
        // A removal, as `remove` makes it: out of the registry, its delete decided now.
        let since = {
            let mut g = r.entries.write().unwrap();
            g.retain(|e| e.collector.def().name != "c");
            r.files.now()
        };
        let held = || {
            r.files
                .paths
                .lock()
                .unwrap()
                .get(&path)
                .map(Arc::strong_count)
        };

        std::thread::scope(|s| {
            let mut rearm = None;
            r.delete_pausing(&sid("s"), "c", since, || {
                // The name is re-armed: its write wants the path the delete is finishing on.
                let writer = s.spawn(|| {
                    r.add(
                        &sid("s"),
                        &dom("a"),
                        metrics(),
                        def("c", "sv=new", 1 << 20),
                        now(),
                    )
                    .expect("re-armed")
                });
                // Until the write has either finished or sat waiting on the path a while.
                let start = std::time::Instant::now();
                let mut waiting_since = None;
                loop {
                    if writer.is_finished() {
                        break;
                    }
                    if held() == Some(3) {
                        let w = *waiting_since.get_or_insert_with(std::time::Instant::now);
                        if w.elapsed() > std::time::Duration::from_millis(300) {
                            break;
                        }
                    }
                    assert!(start.elapsed() < std::time::Duration::from_secs(10));
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                rearm = Some(writer);
            });
            rearm.expect("spawned").join().unwrap();
        });
        assert!(path.exists(), "the re-armed collector's file");

        // A delete decided before the re-armed collector's write — a stale one.
        r.delete_unless_written_since(&sid("s"), "c", since);
        assert!(
            path.exists(),
            "the stale delete left the live collector's file"
        );
    }

    /// A deleted file's path lock is forgotten with it, so session names that come and go do
    /// not grow the ledger for the daemon's life.
    #[test]
    fn a_deleted_file_s_path_lock_is_forgotten() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        assert_eq!(r.files.paths.lock().unwrap().len(), 1, "the write's lock");
        r.remove(&sid("s"), "c").expect("removed");
        assert_eq!(
            r.files.paths.lock().unwrap().len(),
            0,
            "forgotten with its file"
        );
    }

    /// A write that found its collector live BEFORE a removal decided its delete is ordered
    /// before that removal, however long the write itself takes: the removal deletes its file.
    /// The real `write`, with a removal driven into the window between its liveness check and
    /// its save — where a slow fsync would leave it.
    #[test]
    fn a_write_under_way_when_a_removal_comes_in_is_deleted_by_it() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let (id, file) = prepared(&r, "c");
        let gone = || {
            !r.entries
                .read()
                .unwrap()
                .iter()
                .any(|e| e.collector.def().name == "c")
        };

        std::thread::scope(|s| {
            let mut remover = None;
            let written = r.write_pausing(id, &file, || {
                // The removal decides now, then waits for the path the write holds.
                remover = Some(s.spawn(|| r.remove(&sid("s"), "c").expect("removed")));
                for _ in 0..5000 {
                    if gone() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                assert!(gone(), "the removal decided while the write was under way");
            });
            assert_eq!(written, Ok(true), "the write found the collector live");
            remover.expect("spawned").join().unwrap();
        });
        assert!(!path.exists(), "the removal deleted the write's file");
    }

    /// A path's entry another caller holds — a write waiting on it — is kept when a delete
    /// removes the file: it carries the stamp that write will record.
    #[test]
    fn a_path_lock_another_caller_holds_is_kept() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        let path = crate::collector::persist::collector_path(d.path(), "s", "c");
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let held = r.files.path_lock(&path);
        r.remove(&sid("s"), "c").expect("removed");
        assert!(!path.exists(), "the file went");
        assert_eq!(
            r.files.paths.lock().unwrap().len(),
            1,
            "kept for the caller holding it"
        );
        drop(held);
    }

    /// A snapshot whose collector is gone from where its write was prepared for is reported as
    /// filed only if the collector still exists: moved, its mover writes it; removed, the run
    /// is in no history and was not written, and the caller is told.
    #[test]
    fn a_snapshot_is_reported_unfiled_only_when_its_collector_was_removed() {
        let r = CollectorRegistry::new();
        r.add(
            &sid("s"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        let (id, _) = prepared(&r, "c");

        let (moved, files) = r.move_owner(&sid("s"), &sid("t"));
        assert_eq!(moved, 1);
        r.finish(files);
        assert_eq!(
            r.snapshot_persist_error(id, Ok(false)),
            None,
            "moved: the run is in its history and the mover writes it"
        );

        r.remove(&sid("t"), "c").expect("removed");
        let reported = r.snapshot_persist_error(id, Ok(false));
        assert!(
            reported.as_deref().is_some_and(|m| m.contains("removed")),
            "removed: {reported:?}"
        );
        assert_eq!(r.snapshot_persist_error(id, Ok(true)), None);
    }

    /// A moved collector's old file stays when its new one cannot be written (a full disk): it
    /// is the collector's only copy on disk.
    #[test]
    fn a_move_whose_write_fails_keeps_the_old_file() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        r.add(
            &sid("old"),
            &dom("a"),
            metrics(),
            def("c", "sv=svc", 1 << 20),
            now(),
        )
        .expect("armed");
        block_write(d.path(), "new", "c");
        let (_, pending) = r.move_owner(&sid("old"), &sid("new"));
        r.finish(pending);
        assert!(
            crate::collector::persist::collector_path(d.path(), "old", "c").exists(),
            "the only copy survives"
        );
    }

    /// A collector that moves into a path a detached collector of the same name left (a rename
    /// displacing a stale holder): if the mover's write fails, the bytes still at that path are
    /// the DETACHED collector's, and they go — kept, a restart would restore a dead session's
    /// definition and history under the mover's name. The path reads as held (by the mover),
    /// which is why liveness cannot decide it.
    #[test]
    fn a_failed_move_onto_a_detached_collector_s_path_removes_its_bytes() {
        let d = tempfile::TempDir::new().unwrap();
        let r = CollectorRegistry::new().with_persistence(d.path().to_path_buf());
        for owner in ["stale", "mover"] {
            r.add(
                &sid(owner),
                &dom("a"),
                metrics(),
                def("c", "sv=svc", 1 << 20),
                now(),
            )
            .expect("armed");
        }
        let stale_path = crate::collector::persist::collector_path(d.path(), "stale", "c");
        assert!(stale_path.exists());
        // The rename's transfer, as `handle_sessions_rename` does it.
        let (_, mut pending) = r.detach_session(&sid("stale"));
        let (_, moved) = r.move_owner(&sid("mover"), &sid("stale"));
        pending.absorb(moved);
        block_write(d.path(), "stale", "c");
        r.finish(pending);
        assert!(
            !stale_path.exists(),
            "the detached collector's bytes are gone"
        );
        assert!(
            crate::collector::persist::collector_path(d.path(), "mover", "c").exists(),
            "the mover's own copy is kept"
        );
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
