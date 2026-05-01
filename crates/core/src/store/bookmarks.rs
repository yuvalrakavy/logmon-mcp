use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::RwLock;
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

pub struct BookmarkStore {
    bookmarks: RwLock<HashMap<String, Bookmark>>,
}

impl BookmarkStore {
    pub fn new() -> Self {
        Self { bookmarks: RwLock::new(HashMap::new()) }
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
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
        v.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.qualified_name.cmp(&b.qualified_name)));
        v
    }

    pub fn remove(&self, qualified_name: &str) -> Result<(), BookmarkError> {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.remove(qualified_name)
            .map(|_| ())
            .ok_or_else(|| BookmarkError::NotFound(qualified_name.to_string()))
    }

    /// Remove every bookmark whose data has been evicted from both stores.
    pub fn sweep(
        &self,
        oldest_log_seq: Option<u64>,
        oldest_span_seq: Option<u64>,
    ) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.retain(|_, b| !should_evict(b.seq, oldest_log_seq, oldest_span_seq));
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
        assert!(!replaced, "replace=true on a non-existent name is not a replace");
    }

    #[test]
    fn invalid_name_rejected() {
        let store = BookmarkStore::new();
        assert!(matches!(store.add("A", "", 0, None, false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has/slash", 0, None, false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has space", 0, None, false), Err(BookmarkError::InvalidName(_))));
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
        assert!(matches!(store.remove("A/x"), Err(BookmarkError::NotFound(_))));
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
        let (bm, replaced) = store.add("session-a", "checkpoint", 42, Some("note"), false).unwrap();
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
}
