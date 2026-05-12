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
}

impl CursorCommit {
    /// Advance the cursor to `max_returned_seq`. No-op if
    /// `max_returned_seq <= lower_bound` (no records returned). If the entry
    /// was evicted by `sweep` between read-and-advance and commit, re-inserts
    /// at `max_returned_seq` — preserves advance intent across racing eviction.
    pub fn commit(self, max_returned_seq: u64) {
        if max_returned_seq <= self.lower_bound {
            return;
        }
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        match map.get_mut(&self.qualified_name) {
            Some(b) => {
                b.seq = max_returned_seq;
            }
            None => {
                // Evicted during the lock-free query phase — re-insert at high-water mark.
                map.insert(
                    self.qualified_name.clone(),
                    Bookmark {
                        qualified_name: self.qualified_name.clone(),
                        session: self.session.clone(),
                        name: self.name.clone(),
                        seq: max_returned_seq,
                        created_at: Utc::now(),
                        description: None,
                    },
                );
            }
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
/// True only when **both** stores have *positively confirmed* eviction past
/// the bookmark's seq. A store "confirms eviction" when it has at least
/// one entry AND its oldest entry's seq is greater than the bookmark's seq.
///
/// An empty store does NOT confirm eviction — it could simply have not
/// received any data yet. This means a bookmark created when both stores are
/// empty stays alive until enough data has flowed through both stores to roll
/// past it. This matches the spec intent: bookmarks cannot outlive their data,
/// but cannot be killed by absence of data either.
pub fn should_evict(
    bookmark_seq: u64,
    oldest_log_seq: Option<u64>,
    oldest_span_seq: Option<u64>,
) -> bool {
    let log_evicted = oldest_log_seq.is_some_and(|s| s > bookmark_seq);
    let span_evicted = oldest_span_seq.is_some_and(|s| s > bookmark_seq);
    log_evicted && span_evicted
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
    pub fn sweep(&self, oldest_log_seq: Option<u64>, oldest_span_seq: Option<u64>) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let evicted: Vec<String> = map
            .iter()
            .filter(|(_, b)| should_evict(b.seq, oldest_log_seq, oldest_span_seq))
            .map(|(k, _)| k.clone())
            .collect();
        map.retain(|_, b| !should_evict(b.seq, oldest_log_seq, oldest_span_seq));

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

    /// Atomic get-or-create + capture lower bound. Returns
    /// `(lower_bound, commit_handle)`. The caller filters records with
    /// `entry.seq > lower_bound` and then calls `commit_handle.commit(max_seq)`
    /// after the lock-free query phase.
    ///
    /// On auto-create of a name recently evicted by [`Self::sweep`], logs at
    /// WARN — the next read returns the full buffer instead of a delta.
    pub fn cursor_read_and_advance(&self, session: &str, name: &str) -> (u64, CursorCommit) {
        let qualified_name = format!("{session}/{name}");
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let lower_bound = match map.get(&qualified_name) {
            Some(b) => b.seq,
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
                    },
                );
                0
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
        assert!(should_evict(10, Some(50), Some(50)));
    }

    #[test]
    fn should_not_evict_when_log_store_still_covers() {
        // Bookmark at seq=60; log store still has older data (oldest=10).
        assert!(!should_evict(60, Some(10), Some(120)));
    }

    #[test]
    fn should_not_evict_when_span_store_still_covers() {
        // Bookmark at seq=60; span store still has older data (oldest=10).
        assert!(!should_evict(60, Some(120), Some(10)));
    }

    #[test]
    fn empty_stores_keep_bookmark_alive() {
        // Both stores empty: bookmark survives. The "no data yet" case must
        // not look like "data rolled past."
        assert!(!should_evict(60, None, None));
    }

    #[test]
    fn one_empty_store_keeps_bookmark_alive() {
        // Only the side that has data and rolled past is "confirmed gone."
        // If either side has no data, we can't confirm — keep alive.
        assert!(!should_evict(60, Some(120), None));
    }

    #[test]
    fn sweep_removes_evictable_bookmarks() {
        let store = BookmarkStore::new();
        store.add("A", "old", 1, None, false).unwrap();
        store.add("A", "newer", 5, None, false).unwrap();
        // Both stores have advanced past every bookmark — wipe them all.
        store.sweep(Some(100), Some(100));
        assert!(store.list().is_empty());
    }

    #[test]
    fn sweep_keeps_bookmarks_with_data_behind_them() {
        let store = BookmarkStore::new();
        let (b, _) = store.add("A", "x", 100, None, false).unwrap();
        // Oldest seq in stores is older than the bookmark — data still covers it.
        store.sweep(Some(b.seq - 10), Some(b.seq - 10));
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
        store.sweep(Some(50), Some(50));
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].seq, 100);
    }

    #[test]
    fn evict_skips_when_either_store_empty() {
        let store = BookmarkStore::new();
        store.add("s", "x", 5, None, false).unwrap();
        store.sweep(Some(100), None);
        assert_eq!(store.list().len(), 1);
        store.sweep(None, Some(100));
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
        commit.commit(100);
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
        commit.commit(50); // No new records — max equals lower.
        let entry = store
            .list()
            .into_iter()
            .find(|b| b.qualified_name == "s/c")
            .unwrap();
        assert_eq!(entry.seq, 50);
    }

    #[test]
    fn commit_re_inserts_after_eviction_race() {
        let store = BookmarkStore::new();
        let (_lower, commit) = store.cursor_read_and_advance("s", "c");
        // Simulate eviction sweep removing the entry between read-and-advance and commit.
        store.sweep(Some(u64::MAX), Some(u64::MAX));
        assert!(store.list().iter().all(|b| b.qualified_name != "s/c"));
        // Commit re-inserts at the high-water mark.
        commit.commit(200);
        let entry = store
            .list()
            .into_iter()
            .find(|b| b.qualified_name == "s/c")
            .unwrap();
        assert_eq!(entry.seq, 200);
    }

    #[tracing_test::traced_test]
    #[test]
    fn auto_create_after_eviction_logs_warn() {
        let store = BookmarkStore::new();
        let _ = store.add("s", "evicted", 5, None, false).unwrap();
        // Sweep evicts the bookmark.
        store.sweep(Some(u64::MAX), Some(u64::MAX));
        // Subsequent c>= reference auto-recreates and should WARN.
        let (lower, _commit) = store.cursor_read_and_advance("s", "evicted");
        assert_eq!(lower, 0); // Recreated at seq=0
        assert!(logs_contain("auto-recreating at seq=0"));
    }
}
