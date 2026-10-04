use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub qualified_name: String,
    pub name: String,
    pub session: String,
    /// The seq position this bookmark anchors. `b>=name` filters records to
    /// `entry.seq > seq`; `b<=name` to `entry.seq < seq`. Strict comparison.
    pub seq: u64,
    /// Wall-clock creation time, retained for human-readable display only.
    /// Never used for filter semantics.
    pub created_at: DateTime<Utc>,
    /// Optional caller-supplied note describing the bookmark.
    pub description: Option<String>,
    /// Used only when this bookmark is read as a cursor (`c>=`): the log store's late counter
    /// as of the cursor's last read (or its creation). A record stored LATE — below a seq the
    /// store already held, by a trigger's flush — and numbered above this mark has not yet
    /// been considered by the cursor, so the next read takes it even though its seq is at or
    /// below `seq` (gh #23; `InMemoryStore::cursor_read`). In memory only: a restarted
    /// daemon's store is empty and its late counter restarts at 0, which is what a restored
    /// bookmark's 0 means.
    pub late_mark: u64,
    /// Used only when this bookmark is read as a cursor: the position it was CREATED at.
    /// Records at or below it are never this cursor's, even if a trigger stores them late
    /// after the creation — a filtered-out record already in the pre-trigger buffer when
    /// `bookmarks.add` ran is flushed later with a fresh late number, and without this floor a
    /// "from now" cursor returned it (gh #23). `bookmarks.add` sets it to the start seq; an
    /// auto-created cursor starts at 0. In memory only: a restored bookmark gets its restored
    /// position, so a `start_seq` set above the counter keeps excluding what lies below it.
    pub floor: u64,
}

#[derive(Debug, Error)]
pub enum BookmarkError {
    #[error("invalid bookmark name: {0}")]
    InvalidName(String),
    #[error("bookmark already exists: {0}")]
    AlreadyExists(String),
    #[error("bookmark not found: {0}")]
    NotFound(String),
}

/// Read-and-advance commit handle. Must be either explicitly committed via
/// [`CursorCommit::commit`] or dropped (which is a no-op — the cursor stays at
/// its current position). The `#[must_use]` reminds callers to handle the
/// result of the query phase.
#[derive(Debug)]
#[must_use = "CursorCommit must be committed or explicitly dropped after the query phase"]
pub struct CursorCommit {
    bookmarks: Arc<RwLock<HashMap<String, Bookmark>>>,
    qualified_name: String,
    session: String,
    name: String,
    lower_bound: u64,
    late_mark: u64,
    floor: u64,
}

impl CursorCommit {
    /// The cursor's seq position when the read began.
    pub fn lower_bound(&self) -> u64 {
        self.lower_bound
    }

    /// The cursor's late mark when the read began — see [`Bookmark::late_mark`].
    pub fn late_mark(&self) -> u64 {
        self.late_mark
    }

    /// The cursor's creation floor — see [`Bookmark::floor`].
    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// Move the cursor to `(seq, late_mark)`, computed from what the read KEPT
    /// (`CursorRead::advance_for`). A no-op when neither moved; otherwise it lands even when
    /// only the mark moved (a read of late records alone, or one that returned nothing past
    /// late records its filter passed by).
    ///
    /// Only over the position this read STARTED from: if the bookmark changed meanwhile — a
    /// `bookmarks.add replace`, or another read of the same cursor that committed first — that
    /// change stands. Overwriting it moved a replaced bookmark back, and a cursor backwards.
    /// If the entry was evicted by `sweep` meanwhile, it is re-inserted only when the SEQ
    /// moved — the advance intent a racing eviction must not lose. A read that moved only the
    /// mark leaves the name to the auto-create path, which warns that the cursor was evicted.
    pub fn commit(self, seq: u64, late_mark: u64) {
        if seq == self.lower_bound && late_mark == self.late_mark {
            return;
        }
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        match map.get_mut(&self.qualified_name) {
            Some(b) => {
                if b.seq == self.lower_bound && b.late_mark == self.late_mark {
                    b.seq = seq;
                    b.late_mark = late_mark;
                }
            }
            None if seq != self.lower_bound => {
                // Evicted during the lock-free query phase — re-insert at the new position.
                map.insert(
                    self.qualified_name.clone(),
                    Bookmark {
                        qualified_name: self.qualified_name.clone(),
                        session: self.session.clone(),
                        name: self.name.clone(),
                        seq,
                        created_at: Utc::now(),
                        description: None,
                        late_mark,
                        floor: self.floor,
                    },
                );
            }
            None => {}
        }
    }
}

/// Maximum number of recently-evicted cursor names tracked for the
/// "auto-recreate after eviction" WARN signal. When the set is full, an
/// arbitrary entry is dropped to make room (HashSet has no insertion order).
const MAX_RECENTLY_EVICTED: usize = 1024;

pub struct BookmarkStore {
    bookmarks: Arc<RwLock<HashMap<String, Bookmark>>>,
    /// Tracks names removed by `sweep` since the last call to
    /// `cursor_read_and_advance` for that name. Lets the primitive distinguish
    /// "fresh auto-create" from "post-eviction auto-recreate" so we can WARN
    /// in the latter case. Bounded to `MAX_RECENTLY_EVICTED` entries; arbitrary
    /// victim dropped when over.
    recently_evicted: Mutex<HashSet<String>>,
}

impl BookmarkStore {
    pub fn new() -> Self {
        Self {
            bookmarks: Arc::new(RwLock::new(HashMap::new())),
            recently_evicted: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for BookmarkStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate a bare bookmark name (the user-supplied form).
/// Allowed: ASCII alphanumerics, '-', '_'. Non-empty. Max 64 chars.
pub fn is_valid_bookmark_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Predicate: should this bookmark be auto-evicted?
///
/// True only when **both** stores have positively lost records past the
/// bookmark's seq. A store that has never dropped anything confirms nothing,
/// however far its oldest record sits above the bookmark — bookmarks cannot
/// outlive their data, but they cannot be killed by absence of data either.
///
/// Compared against each store's `lost_below` — the lowest seq it can still
/// speak for — and **not** against its oldest retained seq. The two differ, and
/// this is the same boundary `evicted_before_window` uses: a bookmark whose
/// window an export would call complete must not be swept out from under that
/// export, and a ring that has never dropped anything has not aged any bookmark
/// out of relevance however far its oldest record sits above the bookmark.
pub fn should_evict(bookmark_seq: u64, log_lost_below: u64, span_lost_below: u64) -> bool {
    // The window itself, not the mark: `b>=name` admits `seq > bookmark_seq`,
    // so the range at stake opens at `bookmark_seq + 1` and a store that has
    // lost everything up to and INCLUDING the bookmark's own seq has lost
    // nothing the window asked for. Written `> bookmark_seq`, the sweep deleted
    // a bookmark in exactly that state — every record its window selects still
    // present — while the comment beside it argued the opposite.
    //
    // Through `evicted_below`, so this and the export path share one boundary
    // rather than agreeing by inspection.
    let window_start = bookmark_seq.saturating_add(1);
    let log_gone = crate::filter::parser::evicted_below(window_start, log_lost_below).is_some();
    let span_gone = crate::filter::parser::evicted_below(window_start, span_lost_below).is_some();
    log_gone && span_gone
}

/// Resolve a name (bare or already-qualified) into a qualified name.
/// Bare names get prefixed with `{current_session}/`.
/// Qualified names (containing `/`) are returned unchanged.
pub fn qualify(name: &str, current_session: &str) -> String {
    if name.contains('/') {
        name.to_string()
    } else {
        format!("{current_session}/{name}")
    }
}

impl BookmarkStore {
    /// Add a bookmark anchored at `seq`. Returns `(bookmark, replaced)` where
    /// `replaced` is true if a bookmark with the same qualified name already
    /// existed and was overwritten (only possible when `replace == true`).
    ///
    /// `seq` may be `0`, which is the "before all records" sentinel used by
    /// cursor auto-create (see `engine::seq_counter`). `description` is an
    /// optional caller-supplied note retained verbatim for display.
    pub fn add(
        &self,
        session: &str,
        name: &str,
        seq: u64,
        description: Option<&str>,
        replace: bool,
    ) -> Result<(Bookmark, bool), BookmarkError> {
        self.add_at(session, name, seq, 0, description, replace)
    }

    /// [`Self::add`] with the cursor's late mark — the log store's late counter now, so a `c>=`
    /// read on this bookmark means "from now" for late records too: those stored late BEFORE
    /// the add are not replayed into it (gh #23). `add` passes 0, which a cursor reads as
    /// "every late record held".
    pub fn add_at(
        &self,
        session: &str,
        name: &str,
        seq: u64,
        late_mark: u64,
        description: Option<&str>,
        replace: bool,
    ) -> Result<(Bookmark, bool), BookmarkError> {
        if !is_valid_bookmark_name(name) {
            return Err(BookmarkError::InvalidName(name.to_string()));
        }
        let qualified_name = format!("{session}/{name}");
        let bookmark = Bookmark {
            qualified_name: qualified_name.clone(),
            name: name.to_string(),
            session: session.to_string(),
            seq,
            created_at: Utc::now(),
            description: description.map(|s| s.to_string()),
            late_mark,
            // Read as a cursor, this bookmark starts at `seq`: nothing at or below it is its.
            floor: seq,
        };
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let existed = map.contains_key(&qualified_name);
        if existed && !replace {
            return Err(BookmarkError::AlreadyExists(qualified_name));
        }
        map.insert(qualified_name, bookmark.clone());
        Ok((bookmark, existed))
    }

    /// Insert a bookmark from a persisted snapshot, preserving the original
    /// `created_at`. Used only by `SessionRegistry::restore_named` during
    /// daemon startup; production code paths use `add()` (which sets
    /// `created_at = Utc::now()`).
    ///
    /// Always overwrites if `qualified_name` already exists (consistent with
    /// `add(.., replace=true)`); the restore path can't usefully error on
    /// a pre-existing in-memory entry because the persisted snapshot is the
    /// source of truth at startup.
    pub fn insert_persisted(&self, bookmark: Bookmark) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.insert(bookmark.qualified_name.clone(), bookmark);
    }

    pub fn list(&self) -> Vec<Bookmark> {
        let map = self.bookmarks.read().expect("bookmarks lock poisoned");
        let mut v: Vec<Bookmark> = map.values().cloned().collect();
        // Newest seq first; tie-break on qualified_name so equal-seq ordering
        // is deterministic (HashMap iteration order is not).
        v.sort_by(|a, b| {
            b.seq
                .cmp(&a.seq)
                .then_with(|| a.qualified_name.cmp(&b.qualified_name))
        });
        v
    }

    pub fn remove(&self, qualified_name: &str) -> Result<(), BookmarkError> {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.remove(qualified_name)
            .map(|_| ())
            .ok_or_else(|| BookmarkError::NotFound(qualified_name.to_string()))
    }

    /// Remove every bookmark whose data has been evicted from both stores.
    ///
    /// Lock-ordering discipline (uniform with `cursor_read_and_advance` to avoid
    /// deadlock AND missed-WARN races):
    /// - Always acquire `bookmarks` (RwLock) BEFORE `recently_evicted` (Mutex).
    /// - Hold bookmarks write lock until AFTER `recently_evicted` is updated, so
    ///   a concurrent `cursor_read_and_advance` waiting on the bookmarks lock
    ///   observes the eviction signal atomically with the entry's removal.
    pub fn sweep(&self, log_lost_below: u64, span_lost_below: u64) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let evicted: Vec<String> = map
            .iter()
            .filter(|(_, b)| should_evict(b.seq, log_lost_below, span_lost_below))
            .map(|(k, _)| k.clone())
            .collect();
        map.retain(|_, b| !should_evict(b.seq, log_lost_below, span_lost_below));

        if !evicted.is_empty() {
            let mut recent = self
                .recently_evicted
                .lock()
                .expect("recently_evicted poisoned");
            for name in evicted {
                if recent.len() >= MAX_RECENTLY_EVICTED {
                    if let Some(victim) = recent.iter().next().cloned() {
                        recent.remove(&victim);
                    }
                }
                recent.insert(name);
            }
        }
        // Both locks released here as `recent` and `map` go out of scope.
    }

    /// Remove every bookmark whose `session` field equals `session`.
    /// Returns the number of bookmarks removed.
    pub fn clear_session(&self, session: &str) -> usize {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let before = map.len();
        map.retain(|_, b| b.session != session);
        before - map.len()
    }

    /// Look up a bookmark by qualified name. Returns the bookmark if it exists.
    pub fn get(&self, qualified_name: &str) -> Option<Bookmark> {
        self.bookmarks
            .read()
            .expect("bookmarks lock poisoned")
            .get(qualified_name)
            .cloned()
    }

    /// Atomic get-or-create + capture the cursor's position. Returns
    /// `(lower_bound, commit_handle)`; the handle also carries the late mark. The caller reads
    /// (`InMemoryStore::cursor_read`) and then commits the advance for what it kept, after the
    /// lock-free query phase.
    ///
    /// On auto-create of a name recently evicted by [`Self::sweep`], logs at
    /// WARN — the next read returns the full buffer instead of a delta.
    pub fn cursor_read_and_advance(&self, session: &str, name: &str) -> (u64, CursorCommit) {
        self.cursor_read_and_advance_at(session, name, 0)
    }

    /// [`Self::cursor_read_and_advance`], auto-creating a missing cursor with `late_counter` —
    /// the log store's late counter now — as its mark. At seq 0 every held record is the
    /// cursor's through its normal read anyway; the mark only says which late records were
    /// numbered before the cursor existed, so their loss is not reported as its own.
    pub fn cursor_read_and_advance_at(
        &self,
        session: &str,
        name: &str,
        late_counter: u64,
    ) -> (u64, CursorCommit) {
        let qualified_name = format!("{session}/{name}");
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let (lower_bound, late_mark, floor) = match map.get(&qualified_name) {
            Some(b) => (b.seq, b.late_mark, b.floor),
            None => {
                // Check whether this is a post-eviction auto-recreate.
                // recently_evicted lock acquired AFTER bookmarks lock — uniform
                // ordering matches `sweep` to prevent deadlocks.
                let was_evicted = {
                    let mut recent = self
                        .recently_evicted
                        .lock()
                        .expect("recently_evicted poisoned");
                    recent.remove(&qualified_name)
                };
                if was_evicted {
                    tracing::warn!(
                        cursor = %qualified_name,
                        "cursor was evicted under buffer churn; auto-recreating at seq=0 (next read returns full buffer)"
                    );
                } else {
                    tracing::debug!(
                        cursor = %qualified_name,
                        "cursor auto-created at seq=0"
                    );
                }
                map.insert(
                    qualified_name.clone(),
                    Bookmark {
                        qualified_name: qualified_name.clone(),
                        session: session.to_string(),
                        name: name.to_string(),
                        seq: 0,
                        created_at: Utc::now(),
                        description: None,
                        late_mark: late_counter,
                        floor: 0,
                    },
                );
                (0, late_counter, 0)
            }
        };
        drop(map);
        (
            lower_bound,
            CursorCommit {
                bookmarks: self.bookmarks.clone(),
                qualified_name,
                session: session.to_string(),
                name: name.to_string(),
                lower_bound,
                late_mark,
                floor,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_then_list_returns_bookmark() {
        let store = BookmarkStore::new();
        let (b, replaced) = store.add("A", "before", 5, None, false).unwrap();
        assert_eq!(b.qualified_name, "A/before");
        assert_eq!(b.session, "A");
        assert_eq!(b.name, "before");
        assert_eq!(b.seq, 5);
        assert!(!replaced);
        let all = store.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].qualified_name, "A/before");
    }

    #[test]
    fn add_duplicate_without_replace_errors() {
        let store = BookmarkStore::new();
        store.add("A", "x", 1, None, false).unwrap();
        let err = store.add("A", "x", 2, None, false).unwrap_err();
        assert!(matches!(err, BookmarkError::AlreadyExists(ref n) if n == "A/x"));
    }

    #[test]
    fn add_duplicate_with_replace_overwrites_seq_and_reports_replaced() {
        let store = BookmarkStore::new();
        let (first, replaced1) = store.add("A", "x", 10, None, false).unwrap();
        assert!(!replaced1);
        let (second, replaced2) = store.add("A", "x", 20, None, true).unwrap();
        assert!(replaced2);
        assert!(second.seq > first.seq);
        assert_eq!(second.seq, 20);
    }

    #[test]
    fn add_with_replace_on_fresh_name_reports_not_replaced() {
        let store = BookmarkStore::new();
        let (_, replaced) = store.add("A", "fresh", 1, None, true).unwrap();
        assert!(
            !replaced,
            "replace=true on a non-existent name is not a replace"
        );
    }

    #[test]
    fn invalid_name_rejected() {
        let store = BookmarkStore::new();
        assert!(matches!(
            store.add("A", "", 0, None, false),
            Err(BookmarkError::InvalidName(_))
        ));
        assert!(matches!(
            store.add("A", "has/slash", 0, None, false),
            Err(BookmarkError::InvalidName(_))
        ));
        assert!(matches!(
            store.add("A", "has space", 0, None, false),
            Err(BookmarkError::InvalidName(_))
        ));
    }

    #[test]
    fn remove_existing_ok() {
        let store = BookmarkStore::new();
        store.add("A", "x", 1, None, false).unwrap();
        store.remove("A/x").unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn remove_missing_errors() {
        let store = BookmarkStore::new();
        assert!(matches!(
            store.remove("A/x"),
            Err(BookmarkError::NotFound(_))
        ));
    }

    #[test]
    fn qualify_helper() {
        assert_eq!(qualify("foo", "A"), "A/foo");
        assert_eq!(qualify("B/foo", "A"), "B/foo");
    }

    #[test]
    fn should_evict_when_both_stores_past_seq() {
        // Bookmark at seq=10; both stores' oldest is past it.
        assert!(should_evict(10, 50, 50));
    }

    #[test]
    fn should_not_evict_when_log_store_still_covers() {
        // Bookmark at seq=60; log store still has older data (oldest=10).
        assert!(!should_evict(60, 10, 120));
    }

    #[test]
    fn should_not_evict_when_span_store_still_covers() {
        // Bookmark at seq=60; span store still has older data (oldest=10).
        assert!(!should_evict(60, 120, 10));
    }

    #[test]
    fn empty_stores_keep_bookmark_alive() {
        // Both stores empty: bookmark survives. The "no data yet" case must
        // not look like "data rolled past."
        assert!(!should_evict(60, 0, 0));
    }

    #[test]
    fn one_empty_store_keeps_bookmark_alive() {
        // Only the side that has data and rolled past is "confirmed gone."
        // If either side has no data, we can't confirm — keep alive.
        assert!(!should_evict(60, 120, 0));
    }

    /// The exact boundary, which is where this disagreed with the export path
    /// by one. `b>=name` admits `seq > 10`, so the window opens at 11: a store
    /// whose floor is exactly 11 has lost everything up to and INCLUDING the
    /// mark and nothing the window asked for. Written `lost_below > seq`, the
    /// sweep deleted the bookmark in that state — while the comment beside it
    /// argued, correctly, that it should not.
    #[test]
    fn the_sweep_boundary_is_the_windows_start_not_the_mark() {
        assert!(
            !should_evict(10, 11, 11),
            "everything below the mark is gone and the window is untouched"
        );
        assert!(
            should_evict(10, 12, 12),
            "seq 11 — the window's first record — has now left too"
        );
        // And it is the same boundary the export path applies to `b>=name`.
        for floor in [11u64, 12] {
            assert_eq!(
                should_evict(10, floor, floor),
                crate::filter::parser::evicted_below(11, floor).is_some(),
                "the sweep and the export must not disagree at floor {floor}"
            );
        }
    }

    #[test]
    fn sweep_removes_evictable_bookmarks() {
        let store = BookmarkStore::new();
        store.add("A", "old", 1, None, false).unwrap();
        store.add("A", "newer", 5, None, false).unwrap();
        // Both stores have advanced past every bookmark — wipe them all.
        store.sweep(100, 100);
        assert!(store.list().is_empty());
    }

    #[test]
    fn sweep_keeps_bookmarks_with_data_behind_them() {
        let store = BookmarkStore::new();
        let (b, _) = store.add("A", "x", 100, None, false).unwrap();
        // Oldest seq in stores is older than the bookmark — data still covers it.
        store.sweep(b.seq - 10, b.seq - 10);
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn clear_session_removes_only_matching_session() {
        let store = BookmarkStore::new();
        store.add("A", "one", 1, None, false).unwrap();
        store.add("A", "two", 2, None, false).unwrap();
        store.add("B", "one", 3, None, false).unwrap();
        let removed = store.clear_session("A");
        assert_eq!(removed, 2);
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].qualified_name, "B/one");
    }

    #[test]
    fn clear_session_empty_session_returns_zero() {
        let store = BookmarkStore::new();
        store.add("A", "x", 1, None, false).unwrap();
        let removed = store.clear_session("nonexistent");
        assert_eq!(removed, 0);
        assert_eq!(store.list().len(), 1);
    }

    // ---- New tests for seq-based positions (Task 2 of cursor design) ----

    #[test]
    fn add_records_seq_and_created_at_and_description() {
        let store = BookmarkStore::new();
        let before = Utc::now();
        let (bm, replaced) = store
            .add("session-a", "checkpoint", 42, Some("note"), false)
            .unwrap();
        let after = Utc::now();
        assert_eq!(bm.seq, 42);
        assert_eq!(bm.description.as_deref(), Some("note"));
        assert_eq!(bm.session, "session-a");
        assert_eq!(bm.name, "checkpoint");
        assert!(bm.created_at >= before && bm.created_at <= after);
        assert!(!replaced);
    }

    #[test]
    fn add_replace_false_errors_on_existing() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "x", 1, None, false).unwrap();
        let err = store.add("s", "x", 2, None, false).unwrap_err();
        assert!(matches!(err, BookmarkError::AlreadyExists(_)));
    }

    #[test]
    fn add_replace_true_overwrites() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "x", 1, None, false).unwrap();
        let (bm, replaced) = store.add("s", "x", 2, None, true).unwrap();
        assert!(replaced);
        assert_eq!(bm.seq, 2);
    }

    #[test]
    fn evict_by_seq_when_both_stores_advanced_past() {
        let store = BookmarkStore::new();
        store.add("s", "old", 10, None, false).unwrap();
        store.add("s", "new", 100, None, false).unwrap();
        store.sweep(50, 50);
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].seq, 100);
    }

    #[test]
    fn evict_skips_when_either_store_empty() {
        let store = BookmarkStore::new();
        store.add("s", "x", 5, None, false).unwrap();
        store.sweep(100, 0);
        assert_eq!(store.list().len(), 1);
        store.sweep(0, 100);
        assert_eq!(store.list().len(), 1);
    }

    // ---- New tests for cursor_read_and_advance + CursorCommit (Task 6) ----

    #[test]
    fn cursor_read_and_advance_auto_creates_at_zero() {
        let store = BookmarkStore::new();
        let (lower, _commit) = store.cursor_read_and_advance("s", "fresh");
        assert_eq!(lower, 0);
        let listed = store.list();
        let entry = listed
            .iter()
            .find(|b| b.qualified_name == "s/fresh")
            .unwrap();
        assert_eq!(entry.seq, 0);
    }

    #[test]
    fn cursor_read_and_advance_returns_existing_seq() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "existing", 50, None, false).unwrap();
        let (lower, _commit) = store.cursor_read_and_advance("s", "existing");
        assert_eq!(lower, 50);
    }

    #[test]
    fn commit_advances_when_max_greater_than_lower() {
        let store = BookmarkStore::new();
        let (lower, commit) = store.cursor_read_and_advance("s", "c");
        assert_eq!(lower, 0);
        commit.commit(100, 0);
        let entry = store
            .list()
            .into_iter()
            .find(|b| b.qualified_name == "s/c")
            .unwrap();
        assert_eq!(entry.seq, 100);
    }

    #[test]
    fn commit_no_op_when_max_le_lower() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "c", 50, None, false).unwrap();
        let (lower, commit) = store.cursor_read_and_advance("s", "c");
        assert_eq!(lower, 50);
        commit.commit(50, 0); // No new records — max equals lower.
        let entry = store
            .list()
            .into_iter()
            .find(|b| b.qualified_name == "s/c")
            .unwrap();
        assert_eq!(entry.seq, 50);
    }

    /// A commit moves the cursor only from where its read STARTED: a bookmark replaced during
    /// the read keeps the replacement (it used to be moved back to the read's position).
    #[test]
    fn a_commit_does_not_overwrite_a_bookmark_replaced_during_the_read() {
        let store = BookmarkStore::new();
        let _ = store.add_at("s", "c", 50, 3, None, false).unwrap();
        let (_, commit) = store.cursor_read_and_advance("s", "c");
        let _ = store.add_at("s", "c", 500, 9, None, true).unwrap();
        commit.commit(80, 4);
        let (_, after) = store.cursor_read_and_advance("s", "c");
        assert_eq!((after.lower_bound(), after.late_mark()), (500, 9));
    }

    /// A swept cursor is re-inserted only when its SEQ moved (the advance a racing eviction must
    /// not lose); a read that moved only the late mark leaves the name to auto-create.
    #[test]
    fn a_mark_only_commit_does_not_re_insert_a_swept_cursor() {
        let store = BookmarkStore::new();
        let (_, commit) = store.cursor_read_and_advance("s", "c");
        store.sweep(u64::MAX, u64::MAX);
        commit.commit(0, 7);
        assert!(store.list().iter().all(|b| b.qualified_name != "s/c"));
    }

    /// A read that returned only late records leaves the seq where it was and moves the mark;
    /// the commit must land (it used to return early whenever the seq did not move, so the same
    /// late records came back on every read — gh #23).
    #[test]
    fn commit_moves_the_late_mark_alone() {
        let store = BookmarkStore::new();
        let _ = store.add_at("s", "c", 50, 3, None, false).unwrap();
        let (_, commit) = store.cursor_read_and_advance("s", "c");
        assert_eq!((commit.lower_bound(), commit.late_mark()), (50, 3));
        commit.commit(50, 9);
        let (_, again) = store.cursor_read_and_advance("s", "c");
        assert_eq!((again.lower_bound(), again.late_mark()), (50, 9));
    }

    #[test]
    fn commit_re_inserts_after_eviction_race() {
        let store = BookmarkStore::new();
        let (_lower, commit) = store.cursor_read_and_advance("s", "c");
        // Simulate eviction sweep removing the entry between read-and-advance and commit.
        store.sweep(u64::MAX, u64::MAX);
        assert!(store.list().iter().all(|b| b.qualified_name != "s/c"));
        // Commit re-inserts at the new position, late mark included.
        commit.commit(200, 7);
        let entry = store
            .list()
            .into_iter()
            .find(|b| b.qualified_name == "s/c")
            .unwrap();
        assert_eq!((entry.seq, entry.late_mark), (200, 7));
    }

    #[tracing_test::traced_test]
    #[test]
    fn auto_create_after_eviction_logs_warn() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "evicted", 5, None, false).unwrap();
        // Sweep evicts the bookmark.
        store.sweep(u64::MAX, u64::MAX);
        // Subsequent c>= reference auto-recreates and should WARN.
        let (lower, _commit) = store.cursor_read_and_advance("s", "evicted");
        assert_eq!(lower, 0); // Recreated at seq=0
        assert!(logs_contain("auto-recreating at seq=0"));
    }
}
