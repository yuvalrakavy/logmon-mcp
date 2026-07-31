//! Key syntax for the domain registry — spec §3.1, §3.7, §3.9.
//!
//! A key is an optional `@` sigil followed by a path. The sigil scopes the fact
//! to one case document instead of to the domain (§3.9); strip it and what
//! remains is an ordinary path, so `@/Data/seed` and `/Data/seed` are the same
//! fact at two scopes rather than two spellings of one.

/// Longest legal path, sigil excluded. The sigil is excluded deliberately: a
/// fact and its document-scoped twin must be legal in exactly the same cases.
pub const MAX_PATH_BYTES: usize = 256;

/// Longest legal value. The registry is copied into every never-deleted
/// document, so it is not a payload store (§3.1).
pub const MAX_VALUE_BYTES: usize = 4096;

/// Keys per domain, `/logmon/*` excluded (§3.7 — otherwise logmon could lock an
/// agent out of its own registry).
pub const MAX_KEYS: usize = 256;

/// Document-scoped keys per case (§3.9). Separate from [`MAX_KEYS`] because
/// these never reach the registry.
pub const MAX_SIGIL_KEYS: usize = 64;

/// The one reserved first segment. Everything logmon itself produces lives
/// under it and no agent may write there (§3.7).
pub const RESERVED_SEGMENT: &str = "logmon";

/// The document-scope marker (§3.9). Chosen over `!`, `*`, `?`, `~`, `$`, `&`
/// and `#` because it is not special to any common shell — these keys get typed
/// at a CLI — and because it cannot occur inside a path, so `@/x` needs no
/// escaping to be unambiguous.
pub const SIGIL: char = '@';

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Empty, missing the leading `/`, an empty segment, or an out-of-charset
    /// byte.
    Malformed,
    TooLong,
    /// A `@` key arrived somewhere with no document to put it in (§3.9).
    SigilNotAllowed,
}

/// A parsed key: a path, and whether it was sigilled.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    path: String,
    sigil: bool,
}

impl Key {
    /// Parse a key. `allow_sigil` is false on every surface that writes the
    /// registry and true only where a document exists to receive it.
    ///
    /// **Syntax only — the reservation is policy and is checked by the caller**
    /// via [`Key::is_reserved`], because `set_reserved` is a legitimate writer
    /// of `/logmon/*` and shares this parser. So `@/logmon/x` on a registry
    /// surface reports `SigilNotAllowed`: it never gets far enough to be tested
    /// for the reservation, and either answer tells the caller no.
    pub fn parse(raw: &str, allow_sigil: bool) -> Result<Self, KeyError> {
        let (sigil, path) = match raw.strip_prefix(SIGIL) {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        validate_path(path)?;
        if sigil && !allow_sigil {
            return Err(KeyError::SigilNotAllowed);
        }
        Ok(Self {
            path: path.to_string(),
            sigil,
        })
    }

    /// The path with no sigil — what prefix matching and the reservation see.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether this key belongs to one document rather than to the domain.
    pub fn is_scoped(&self) -> bool {
        self.sigil
    }

    /// True for `/logmon/...` and for the bare `/logmon`.
    pub fn is_reserved(&self) -> bool {
        is_reserved(&self.path)
    }

    /// How the key renders — with its sigil, so a reader sees the scope in the
    /// key itself and `grep '/Data/seed'` and `grep '@/Data/seed'` stay two
    /// different questions (§3.9).
    pub fn rendered(&self) -> String {
        if self.sigil {
            format!("{SIGIL}{}", self.path)
        } else {
            self.path.clone()
        }
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.rendered())
    }
}

/// Whether a path's **first segment** is the reserved one.
///
/// Stated over segments rather than as a byte prefix, and that is the whole
/// point. A `starts_with("/logmon/")` guard accepts the bare `/logmon` — a
/// legal path — which an agent could then seat a value at, rendered unprefixed
/// beside the broker's own keys, and which `remove` would refuse to delete.
/// Read the other way, `remove("/logmon")` under segment matching would wipe
/// every reserved key at once, and `remove` has no undo.
///
/// One rule covers both directions: any pattern that matches a reserved key is
/// itself reserved, because prefix matching only ever strips whole trailing
/// segments and so can never change the first one.
pub fn is_reserved(path: &str) -> bool {
    first_segment(path) == Some(RESERVED_SEGMENT)
}

fn first_segment(path: &str) -> Option<&str> {
    path.strip_prefix('/')?.split('/').next()
}

/// Whether `pattern` matches `path` on segment boundaries (§3.1).
///
/// `/Versions` matches `/Versions/ht_server` and `/Versions`; it does **not**
/// match `/VersionsOld/x`. Byte-prefix matching would let `/Ver` delete
/// `/Versions/*`.
pub fn matches_prefix(pattern: &str, path: &str) -> bool {
    if path == pattern {
        return true;
    }
    match path.strip_prefix(pattern) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

fn validate_path(path: &str) -> Result<(), KeyError> {
    if path.len() > MAX_PATH_BYTES {
        return Err(KeyError::TooLong);
    }
    let Some(body) = path.strip_prefix('/') else {
        return Err(KeyError::Malformed);
    };
    if body.is_empty() {
        return Err(KeyError::Malformed);
    }
    for segment in body.split('/') {
        if segment.is_empty() {
            // Catches `//a`, `/a//b` and the trailing `/a/` in one check, so
            // each key has exactly one spelling.
            return Err(KeyError::Malformed);
        }
        if !segment.bytes().all(is_segment_byte) {
            return Err(KeyError::Malformed);
        }
    }
    Ok(())
}

/// ASCII letters, digits, `_`, `-`, `.`.
///
/// Close to `session::is_valid_name` but not equal to it: that allows only
/// `[A-Za-z0-9_-]`, and `.` is wanted here for versions and hostnames. Stated
/// because an earlier draft cited that function as *matching* this rule.
fn is_segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> Key {
        Key::parse(raw, true).unwrap_or_else(|e| panic!("{raw} should parse, got {e:?}"))
    }

    fn err(raw: &str) -> KeyError {
        Key::parse(raw, true).unwrap_err()
    }

    #[test]
    fn accepts_the_shapes_the_key_set_uses() {
        for raw in [
            "/Build/commit",
            "/Action",
            "/Versions/ht_server",
            "/Env/os",
            "/a.b-c_d/e",
            "/1",
        ] {
            assert_eq!(ok(raw).path(), raw);
        }
    }

    #[test]
    fn rejects_every_way_a_path_can_be_malformed() {
        for raw in [
            "", "Action", "/", "//a", "/a//b", "/a/", "/a b", "/a\tb", "/héllo", "/a/*", "/a?b",
        ] {
            assert_eq!(err(raw), KeyError::Malformed, "{raw} should be malformed");
        }
    }

    #[test]
    fn a_path_over_the_cap_is_too_long_not_malformed() {
        let long = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert_eq!(err(&long), KeyError::TooLong);
        let at_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        assert_eq!(ok(&at_cap).path().len(), MAX_PATH_BYTES);
    }

    #[test]
    fn the_sigil_does_not_change_what_lengths_are_legal() {
        // `@/x` and `/x` are the same fact at two scopes, so one must not be
        // legal where the other is not.
        let at_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES - 1));
        assert!(Key::parse(&format!("@{at_cap}"), true).is_ok());
        let over = format!("/{}", "a".repeat(MAX_PATH_BYTES));
        assert_eq!(
            Key::parse(&format!("@{over}"), true),
            Err(KeyError::TooLong)
        );
    }

    #[test]
    fn the_bare_reserved_segment_is_reserved_too() {
        // The guard an implementer reaches for first — `starts_with("/logmon/")`
        // — accepts this, and it is then unremovable.
        assert!(ok("/logmon").is_reserved());
        assert!(ok("/logmon/version").is_reserved());
    }

    #[test]
    fn reservation_is_by_segment_not_by_byte_prefix() {
        assert!(!ok("/logmonster").is_reserved());
        assert!(!ok("/logmon2/x").is_reserved());
        assert!(!ok("/x/logmon").is_reserved());
    }

    #[test]
    fn every_pattern_that_matches_a_reserved_key_is_itself_reserved() {
        // This is what makes one check cover both directions. If it ever fails,
        // `remove` has a hole that silently deletes H3's and H6's defence.
        let reserved = ["/logmon", "/logmon/version", "/logmon/incarnation"];
        for pattern in ["/logmon", "/logmon/version", "/logmonster", "/x"] {
            let matches_any = reserved.iter().any(|k| matches_prefix(pattern, k));
            if matches_any {
                assert!(
                    is_reserved(pattern),
                    "{pattern} matches a reserved key but is not itself reserved"
                );
            }
        }
    }

    #[test]
    fn prefix_matching_stops_at_segment_boundaries() {
        assert!(matches_prefix("/Versions", "/Versions/ht_server"));
        assert!(matches_prefix("/Versions", "/Versions"));
        assert!(!matches_prefix("/Ver", "/Versions/ht_server"));
        assert!(!matches_prefix("/Versions", "/VersionsOld/x"));
    }

    #[test]
    fn a_sigil_key_is_a_path_plus_scope() {
        let k = ok("@/Data/seed");
        assert!(k.is_scoped());
        assert_eq!(k.path(), "/Data/seed");
        assert_eq!(k.rendered(), "@/Data/seed");
    }

    #[test]
    fn the_sigil_survives_rendering_so_two_greps_stay_two_questions() {
        assert_ne!(ok("@/Data/seed").rendered(), ok("/Data/seed").rendered());
    }

    #[test]
    fn scoped_and_unscoped_are_different_keys() {
        assert_ne!(ok("@/Data/seed"), ok("/Data/seed"));
    }

    #[test]
    fn a_sigil_is_refused_where_there_is_no_document_to_put_it_in() {
        assert_eq!(
            Key::parse("@/Data/seed", false),
            Err(KeyError::SigilNotAllowed)
        );
        assert!(Key::parse("/Data/seed", false).is_ok());
    }

    #[test]
    fn a_sigil_does_not_launder_the_reservation() {
        // Reported as Reserved rather than SigilNotAllowed: the laundering
        // attempt is the more useful thing to name.
        assert!(ok("@/logmon/version").is_reserved());
        assert_eq!(
            Key::parse("@/logmon/version", false),
            Err(KeyError::SigilNotAllowed)
        );
    }

    #[test]
    fn a_lone_sigil_is_malformed_not_a_key() {
        assert_eq!(err("@"), KeyError::Malformed);
        assert_eq!(err("@/"), KeyError::Malformed);
    }

    #[test]
    fn a_sigil_anywhere_but_the_front_is_not_a_sigil() {
        assert_eq!(err("/Data/@seed"), KeyError::Malformed);
        assert_eq!(err("/@"), KeyError::Malformed);
    }

    #[test]
    fn keys_are_compared_byte_wise() {
        assert_ne!(ok("/Build/Commit"), ok("/Build/commit"));
    }
}
