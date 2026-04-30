use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub qualified_name: String,
    pub name: String,
    pub session: String,
    pub timestamp: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
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
/// the bookmark's timestamp. A store "confirms eviction" when it has at least
/// one entry AND its oldest entry is newer than the bookmark.
///
/// An empty store does NOT confirm eviction — it could simply have not
/// received any data yet. This means a bookmark created when both stores are
/// empty stays alive until enough data has flowed through both stores to roll
/// past it. This matches the spec intent: bookmarks cannot outlive their data,
/// but cannot be killed by absence of data either.
pub fn should_evict(
    bookmark_ts: DateTime<Utc>,
    oldest_log_ts: Option<DateTime<Utc>>,
    oldest_span_ts: Option<DateTime<Utc>>,
) -> bool {
    let log_evicted = oldest_log_ts.is_some_and(|t| t > bookmark_ts);
    let span_evicted = oldest_span_ts.is_some_and(|t| t > bookmark_ts);
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
    /// Add a bookmark. Returns `(bookmark, replaced)` where `replaced` is true
    /// if a bookmark with the same qualified name already existed and was
    /// overwritten (only possible when `replace == true`).
    pub fn add(
        &self,
        session: &str,
        name: &str,
        replace: bool,
    ) -> Result<(Bookmark, bool), BookmarkError> {
        if !is_valid_bookmark_name(name) {
            return Err(BookmarkError::InvalidName(name.to_string()));
        }
        let qualified_name = format!("{session}/{name}");
        let now = Utc::now();
        let bookmark = Bookmark {
            qualified_name: qualified_name.clone(),
            name: name.to_string(),
            session: session.to_string(),
            timestamp: now,
            created_at: now,
        };
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        let existed = map.contains_key(&qualified_name);
        if existed && !replace {
            return Err(BookmarkError::AlreadyExists(qualified_name));
        }
        map.insert(qualified_name, bookmark.clone());
        Ok((bookmark, existed))
    }

    pub fn list(&self) -> Vec<Bookmark> {
        let map = self.bookmarks.read().expect("bookmarks lock poisoned");
        let mut v: Vec<Bookmark> = map.values().cloned().collect();
        v.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
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
        oldest_log_ts: Option<DateTime<Utc>>,
        oldest_span_ts: Option<DateTime<Utc>>,
    ) {
        let mut map = self.bookmarks.write().expect("bookmarks lock poisoned");
        map.retain(|_, b| !should_evict(b.timestamp, oldest_log_ts, oldest_span_ts));
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
        let (b, replaced) = store.add("A", "before", false).unwrap(); // returns (Bookmark, bool); ignored here
        assert_eq!(b.qualified_name, "A/before");
        assert_eq!(b.session, "A");
        assert_eq!(b.name, "before");
        assert!(!replaced);
        let all = store.list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].qualified_name, "A/before");
    }

    #[test]
    fn add_duplicate_without_replace_errors() {
        let store = BookmarkStore::new();
        store.add("A", "x", false).unwrap();
        let err = store.add("A", "x", false).unwrap_err();
        assert!(matches!(err, BookmarkError::AlreadyExists(ref n) if n == "A/x"));
    }

    #[test]
    fn add_duplicate_with_replace_overwrites_timestamp_and_reports_replaced() {
        let store = BookmarkStore::new();
        let (first, replaced1) = store.add("A", "x", false).unwrap();
        assert!(!replaced1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (second, replaced2) = store.add("A", "x", true).unwrap();
        assert!(replaced2);
        assert!(second.timestamp > first.timestamp);
    }

    #[test]
    fn add_with_replace_on_fresh_name_reports_not_replaced() {
        let store = BookmarkStore::new();
        let (_, replaced) = store.add("A", "fresh", true).unwrap();
        assert!(!replaced, "replace=true on a non-existent name is not a replace");
    }

    #[test]
    fn invalid_name_rejected() {
        let store = BookmarkStore::new();
        assert!(matches!(store.add("A", "", false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has/slash", false), Err(BookmarkError::InvalidName(_))));
        assert!(matches!(store.add("A", "has space", false), Err(BookmarkError::InvalidName(_))));
    }

    #[test]
    fn remove_existing_ok() {
        let store = BookmarkStore::new();
        store.add("A", "x", false).unwrap();
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
    fn should_evict_when_both_stores_past_timestamp() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(10));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(5));
        assert!(should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn should_not_evict_when_log_store_still_covers() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(120));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(5));
        assert!(!should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn should_not_evict_when_span_store_still_covers() {
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now() - chrono::Duration::seconds(10));
        let oldest_span = Some(Utc::now() - chrono::Duration::seconds(120));
        assert!(!should_evict(bookmark_ts, oldest_log, oldest_span));
    }

    #[test]
    fn empty_stores_keep_bookmark_alive() {
        // Both stores empty: bookmark survives. The "no data yet" case must
        // not look like "data rolled past."
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        assert!(!should_evict(bookmark_ts, None, None));
    }

    #[test]
    fn one_empty_store_keeps_bookmark_alive() {
        // Only the side that has data and rolled past is "confirmed gone."
        // If either side has no data, we can't confirm — keep alive.
        let bookmark_ts = Utc::now() - chrono::Duration::seconds(60);
        let oldest_log = Some(Utc::now()); // log rolled past
        assert!(!should_evict(bookmark_ts, oldest_log, None));
    }

    #[test]
    fn sweep_removes_evictable_bookmarks() {
        let store = BookmarkStore::new();
        store.add("A", "old", false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.add("A", "new", false).unwrap();
        // Pretend both stores have data, all of which is newer than the
        // newest bookmark — both bookmarks should be removed.
        let cutoff = Utc::now() + chrono::Duration::seconds(1);
        store.sweep(Some(cutoff), Some(cutoff));
        assert!(store.list().is_empty());
    }

    #[test]
    fn sweep_keeps_bookmarks_with_data_behind_them() {
        let store = BookmarkStore::new();
        let (b, _) = store.add("A", "x", false).unwrap();
        // Oldest log is older than the bookmark — data still covers it.
        let oldest = b.timestamp - chrono::Duration::seconds(10);
        store.sweep(Some(oldest), Some(oldest));
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn clear_session_removes_only_matching_session() {
        let store = BookmarkStore::new();
        store.add("A", "one", false).unwrap();
        store.add("A", "two", false).unwrap();
        store.add("B", "one", false).unwrap();
        let removed = store.clear_session("A");
        assert_eq!(removed, 2);
        let remaining = store.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].qualified_name, "B/one");
    }

    #[test]
    fn clear_session_empty_session_returns_zero() {
        let store = BookmarkStore::new();
        store.add("A", "x", false).unwrap();
        let removed = store.clear_session("nonexistent");
        assert_eq!(removed, 0);
        assert_eq!(store.list().len(), 1);
    }
}
