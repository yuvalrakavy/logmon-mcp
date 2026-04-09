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
}
