use crate::filter::parser::{BookmarkOp, ParsedFilter, Qualifier, SeqOp};
use crate::store::bookmarks::{qualify, BookmarkStore, CursorCommit};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookmarkResolutionError {
    #[error("bookmark not found: {0}")]
    NotFound(String),
    #[error("cross-session cursor advance is not permitted: {0}")]
    CrossSessionCursorAdvance(String),
}

/// Result of resolving bookmark/cursor qualifiers in a filter.
///
/// `filter` is the rewritten filter (every `BookmarkFilter` and `CursorFilter`
/// has been replaced with the corresponding internal `SeqFilter`). When the
/// input contained at least one `CursorFilter`, `cursor_commit` carries the
/// commit handle returned by `BookmarkStore::cursor_read_and_advance` — the
/// caller is expected to invoke `commit_handle.commit(max_seq)` after the
/// lock-free query phase to advance the cursor. A filter never contains more
/// than one `CursorFilter` (parser-enforced — see `parser.rs:495`), so a
/// single `Option<CursorCommit>` is sufficient.
#[derive(Debug)]
pub struct ResolvedFilter {
    pub filter: ParsedFilter,
    pub cursor_commit: Option<CursorCommit>,
}

/// Walk a parsed filter and rewrite every `BookmarkFilter` / `CursorFilter`
/// qualifier to a `SeqFilter` by looking up the bookmark in the store.
///
/// - `BookmarkFilter` qualifiers: bare names are qualified against
///   `current_session`; `BookmarkOp::Gte` maps to `SeqOp::Gt`,
///   `BookmarkOp::Lte` maps to `SeqOp::Lt`. Returns `NotFound` on the first
///   missing bookmark.
/// - `CursorFilter` qualifiers: cross-session names (qualified to a session
///   other than `current_session`) are rejected with
///   `CrossSessionCursorAdvance` — advancing another session's cursor would
///   silently corrupt that session's read state. For same-session cursors the
///   resolver calls `BookmarkStore::cursor_read_and_advance` to obtain the
///   current lower bound (auto-creating at seq=0 if absent) and a commit
///   handle, emits a `Qualifier::SeqFilter { op: Gt, value: lower }`, and
///   captures the commit handle on the returned `ResolvedFilter`.
pub fn resolve_bookmarks(
    filter: ParsedFilter,
    store: &BookmarkStore,
    current_session: &str,
) -> Result<ResolvedFilter, BookmarkResolutionError> {
    let qs = match filter {
        ParsedFilter::All | ParsedFilter::None => {
            return Ok(ResolvedFilter {
                filter,
                cursor_commit: None,
            });
        }
        ParsedFilter::Qualifiers(qs) => qs,
    };

    let mut out = Vec::with_capacity(qs.len());
    let mut cursor_commit: Option<CursorCommit> = None;

    for q in qs {
        match q {
            Qualifier::BookmarkFilter { op, name } => {
                let qualified = qualify(&name, current_session);
                let bookmark = store
                    .get(&qualified)
                    .ok_or(BookmarkResolutionError::NotFound(qualified))?;
                out.push(Qualifier::SeqFilter {
                    op: match op {
                        BookmarkOp::Gte => SeqOp::Gt,
                        BookmarkOp::Lte => SeqOp::Lt,
                    },
                    value: bookmark.seq,
                });
            }
            Qualifier::CursorFilter { name } => {
                let qualified = qualify(&name, current_session);
                let (target_session, target_name) = split_qualified(&qualified);
                if target_session != current_session {
                    return Err(BookmarkResolutionError::CrossSessionCursorAdvance(
                        qualified,
                    ));
                }
                let (lower, commit) = store.cursor_read_and_advance(target_session, target_name);
                out.push(Qualifier::SeqFilter {
                    op: SeqOp::Gt,
                    value: lower,
                });
                cursor_commit = Some(commit);
            }
            other => out.push(other),
        }
    }

    Ok(ResolvedFilter {
        filter: ParsedFilter::Qualifiers(out),
        cursor_commit,
    })
}

/// Split `qualify`'s output into `(session, name)`. `qualify` always produces
/// a string containing a `/` — bare names get a `{current_session}/` prefix,
/// already-qualified names are passed through untouched (and the parser
/// requires qualified names to contain `/`).
fn split_qualified(qualified: &str) -> (&str, &str) {
    qualified
        .split_once('/')
        .expect("qualify() produced a name without '/' — bug")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::parse_filter;

    #[test]
    fn bare_name_resolves_against_current_session() {
        let store = BookmarkStore::new();
        store.add("A", "before", 7, None, false).unwrap();
        let filter = parse_filter("b>=before").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert_eq!(qs.len(), 1);
            assert!(matches!(
                qs[0],
                Qualifier::SeqFilter {
                    op: SeqOp::Gt,
                    value: 7
                }
            ));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_none());
    }

    #[test]
    fn qualified_name_resolves_directly() {
        let store = BookmarkStore::new();
        store.add("B", "x", 12, None, false).unwrap();
        let filter = parse_filter("b<=B/x").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert!(matches!(
                qs[0],
                Qualifier::SeqFilter {
                    op: SeqOp::Lt,
                    value: 12
                }
            ));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_none());
    }

    #[test]
    fn missing_bookmark_returns_not_found() {
        let store = BookmarkStore::new();
        let filter = parse_filter("b>=ghost").unwrap();
        let err = resolve_bookmarks(filter, &store, "A").unwrap_err();
        assert!(matches!(err, BookmarkResolutionError::NotFound(ref n) if n == "A/ghost"));
    }

    #[test]
    fn non_bookmark_qualifiers_pass_through() {
        let store = BookmarkStore::new();
        let filter = parse_filter("l>=warn").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert!(matches!(qs[0], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_none());
    }

    #[test]
    fn multiple_bookmarks_all_resolved() {
        let store = BookmarkStore::new();
        store.add("A", "start", 1, None, false).unwrap();
        store.add("A", "end", 99, None, false).unwrap();
        let filter = parse_filter("b>=start, b<=end, l>=info").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert_eq!(qs.len(), 3);
            assert!(matches!(qs[0], Qualifier::SeqFilter { value: 1, .. }));
            assert!(matches!(qs[1], Qualifier::SeqFilter { value: 99, .. }));
            assert!(matches!(qs[2], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_none());
    }

    #[test]
    fn all_and_none_filters_pass_through_unchanged() {
        let store = BookmarkStore::new();
        let resolved = resolve_bookmarks(ParsedFilter::All, &store, "A").unwrap();
        assert!(matches!(resolved.filter, ParsedFilter::All));
        assert!(resolved.cursor_commit.is_none());
        let resolved = resolve_bookmarks(ParsedFilter::None, &store, "A").unwrap();
        assert!(matches!(resolved.filter, ParsedFilter::None));
        assert!(resolved.cursor_commit.is_none());
    }

    #[test]
    fn resolves_cursor_qualifier_to_seq_filter_with_auto_create() {
        let store = BookmarkStore::new();
        let filter = parse_filter("c>=mycur").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert!(matches!(
                qs[0],
                Qualifier::SeqFilter {
                    op: SeqOp::Gt,
                    value: 0
                }
            ));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_some());
    }

    #[test]
    fn rejects_cross_session_cursor_advance() {
        let store = BookmarkStore::new();
        let filter = parse_filter("c>=other-session/cur").unwrap();
        let err = resolve_bookmarks(filter, &store, "A").unwrap_err();
        assert!(matches!(
            err,
            BookmarkResolutionError::CrossSessionCursorAdvance(_)
        ));
    }

    #[test]
    fn cursor_qualifier_uses_existing_seq_for_existing_entry() {
        let store = BookmarkStore::new();
        let _ = store.add("A", "existing", 100, None, false).unwrap();
        let filter = parse_filter("c>=existing").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved.filter {
            assert!(matches!(
                qs[0],
                Qualifier::SeqFilter {
                    op: SeqOp::Gt,
                    value: 100
                }
            ));
        } else {
            panic!("expected qualifiers");
        }
        assert!(resolved.cursor_commit.is_some());
    }
}
