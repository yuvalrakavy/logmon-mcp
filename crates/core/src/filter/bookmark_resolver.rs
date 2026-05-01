use crate::filter::parser::{ParsedFilter, Qualifier};
use crate::store::bookmarks::{qualify, BookmarkStore};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookmarkResolutionError {
    #[error("bookmark not found: {0}")]
    NotFound(String),
}

/// Walk a parsed filter and rewrite every `BookmarkFilter` qualifier to a
/// `SeqFilter` by looking up the bookmark in the store. Bare names are
/// qualified against `current_session`. Returns `NotFound` on the first
/// missing bookmark.
pub fn resolve_bookmarks(
    filter: ParsedFilter,
    store: &BookmarkStore,
    current_session: &str,
) -> Result<ParsedFilter, BookmarkResolutionError> {
    match filter {
        ParsedFilter::All | ParsedFilter::None => Ok(filter),
        ParsedFilter::Qualifiers(qs) => {
            let mut out = Vec::with_capacity(qs.len());
            for q in qs {
                match q {
                    Qualifier::BookmarkFilter { op, name } => {
                        let qualified = qualify(&name, current_session);
                        let bookmark = store
                            .get(&qualified)
                            .ok_or(BookmarkResolutionError::NotFound(qualified))?;
                        out.push(Qualifier::SeqFilter {
                            op,
                            value: bookmark.seq,
                        });
                    }
                    other => out.push(other),
                }
            }
            Ok(ParsedFilter::Qualifiers(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::{parse_filter, BookmarkOp};

    #[test]
    fn bare_name_resolves_against_current_session() {
        let store = BookmarkStore::new();
        store.add("A", "before", 7, None, false).unwrap();
        let filter = parse_filter("b>=before").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert_eq!(qs.len(), 1);
            assert!(matches!(qs[0], Qualifier::SeqFilter { op: BookmarkOp::Gte, value: 7 }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn qualified_name_resolves_directly() {
        let store = BookmarkStore::new();
        store.add("B", "x", 12, None, false).unwrap();
        let filter = parse_filter("b<=B/x").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert!(matches!(qs[0], Qualifier::SeqFilter { op: BookmarkOp::Lte, value: 12 }));
        } else {
            panic!("expected qualifiers");
        }
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
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert!(matches!(qs[0], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn multiple_bookmarks_all_resolved() {
        let store = BookmarkStore::new();
        store.add("A", "start", 1, None, false).unwrap();
        store.add("A", "end", 99, None, false).unwrap();
        let filter = parse_filter("b>=start, b<=end, l>=info").unwrap();
        let resolved = resolve_bookmarks(filter, &store, "A").unwrap();
        if let ParsedFilter::Qualifiers(qs) = resolved {
            assert_eq!(qs.len(), 3);
            assert!(matches!(qs[0], Qualifier::SeqFilter { value: 1, .. }));
            assert!(matches!(qs[1], Qualifier::SeqFilter { value: 99, .. }));
            assert!(matches!(qs[2], Qualifier::LevelFilter { .. }));
        } else {
            panic!("expected qualifiers");
        }
    }

    #[test]
    fn all_and_none_filters_pass_through_unchanged() {
        let store = BookmarkStore::new();
        let resolved = resolve_bookmarks(ParsedFilter::All, &store, "A").unwrap();
        assert!(matches!(resolved, ParsedFilter::All));
        let resolved = resolve_bookmarks(ParsedFilter::None, &store, "A").unwrap();
        assert!(matches!(resolved, ParsedFilter::None));
    }
}
