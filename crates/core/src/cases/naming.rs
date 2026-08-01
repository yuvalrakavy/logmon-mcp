//! Case filenames — spec §5.3.
//!
//! `<prefix>-<yymmdd>-<hhmmss>[-<id>]`, shared by all three files:
//!
//! ```text
//! checkout-hang-260731-021530.md
//! checkout-hang-260731-021530.logdata.jsonl
//! checkout-hang-260731-021530.spandata.jsonl
//! ```
//!
//! **The timestamp is in the name deliberately.** §4.1 argues against encoding a
//! query axis into the layout, but that argument is about axes where picking one
//! privileges it over equally good alternatives. Time is not one of many: it is
//! the axis the filesystem already sorts by, for free. Fixed-width
//! `yymmdd-hhmmss` makes lexicographic order chronological order, so `ls`
//! answers "what happened around then" over an archive nobody has indexed.
//!
//! **UTC, not local.** Local time makes names unsortable across a DST boundary
//! and incomparable between machines, and this archive is meant to be read years
//! later on some other host. Second resolution here; front-matter carries the
//! precise instant and remains the record.
//!
//! **Parse from the right.** `safe_name` preserves `-`, so a natural prefix like
//! `checkout-hang` survives intact and splitting from the left is ambiguous. The
//! trailing fields are fixed-shape — six digits, six digits, optional four hex —
//! so a tool walking the archive recovers them deterministically from the end.

use crate::collector::document::safe_name;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// The prefix is truncated to this, not rejected — a capture must not fail over
/// a long name, and the evidence is what matters.
pub const MAX_PREFIX_BYTES: usize = 48;

/// The floor's floor. Reached only when the domain name itself sanitises to
/// nothing usable — `is_valid_name` admits `___` — which is why it exists at all
/// rather than being what §5.3 rejects: a literal `case` as the *default* spends
/// the first five sort-significant characters of every filename saying nothing.
const LAST_RESORT_PREFIX: &str = "case";

/// The filename prefix, most specific source first: the `prefix` parameter, then
/// `/case-name` from the registry, then the domain name.
///
/// The parameter wins because a caller naming *this* capture knows more than a
/// session-wide default. `/case-name` sits between them so a project sets its
/// archive's vocabulary once instead of on every call — a convention that has to
/// be restated per call is one that gets skipped.
///
/// **The domain is the floor and it earns that place**: it is the only identity
/// logmon already has, and the skill recommends one domain per project or test
/// run, so it is already per-project. A level that sanitises to nothing drops
/// through rather than landing in the archive — a directory of `___-260731-*.md`
/// is worse than one of `t3-260731-*.md`, and leaving the optional parameter
/// with no default at all would produce `-260731-021530.md`, a leading-dash
/// filename most CLI tools read as a flag.
pub fn resolve_prefix(param: Option<&str>, case_name: Option<&str>, domain: &str) -> String {
    [param, case_name, Some(domain)]
        .into_iter()
        .flatten()
        .find_map(usable_prefix)
        .unwrap_or_else(|| LAST_RESORT_PREFIX.to_string())
}

/// Sanitise, bound, and accept only if something readable survives.
///
/// Sanitising **before** truncating is what makes the byte bound safe:
/// `safe_name` collapses everything outside `[A-Za-z0-9_-]` — including every
/// non-ASCII character — to `_`, so the result is pure ASCII and no byte index
/// can fall inside a character.
///
/// The alphanumeric test runs **after** truncation, since a prefix of 48
/// underscores followed by a letter loses the letter to the bound.
fn usable_prefix(raw: &str) -> Option<String> {
    let mut s = safe_name(raw);
    s.truncate(MAX_PREFIX_BYTES);
    s.chars().any(|c| c.is_ascii_alphanumeric()).then_some(s)
}

/// `<prefix>-<yymmdd>-<hhmmss>`, in UTC.
pub fn stem(prefix: &str, at: DateTime<Utc>) -> String {
    format!("{prefix}-{}", at.format("%y%m%d-%H%M%S"))
}

/// A fresh four-hex-character disambiguator.
///
/// Random rather than a counter, because a counter has to enumerate the
/// directory to pick a value and two writers enumerating concurrently pick the
/// same one — which is the collision this exists to break.
pub fn roll_id() -> String {
    Uuid::new_v4().simple().to_string()[..4].to_string()
}

/// The three paths a stem owns, in the order they are reported.
pub fn file_names(stem: &str) -> [String; 3] {
    [
        format!("{stem}.md"),
        format!("{stem}.logdata.jsonl"),
        format!("{stem}.spandata.jsonl"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn names_sort_lexicographically_into_chronological_order() {
        let mut names = vec![
            stem("x", at("2027-01-01T00:00:00Z")),
            stem("x", at("2026-12-31T23:59:59Z")),
            stem("x", at("2026-08-01T00:00:00Z")),
            stem("x", at("2026-07-31T23:59:59Z")),
        ];
        let chronological = vec![
            names[3].clone(),
            names[2].clone(),
            names[1].clone(),
            names[0].clone(),
        ];
        names.sort();
        assert_eq!(
            names, chronological,
            "lexicographic order must be chronological across a month AND a year boundary"
        );
    }

    #[test]
    fn the_timestamp_is_utc_whatever_the_host_is() {
        // 02:15:30Z is 22:15:30 the previous day in New York. The name must be
        // the first of those, or two machines investigating one incident
        // produce names that cannot be compared.
        assert_eq!(
            stem("x", at("2026-07-31T02:15:30+00:00")),
            "x-260731-021530"
        );
        assert_eq!(
            stem("x", at("2026-07-30T22:15:30-04:00")),
            "x-260731-021530",
            "the same instant, written in another zone, is the same name"
        );
    }

    #[test]
    fn a_prefix_containing_a_dash_survives_and_the_fields_parse_from_the_right() {
        let s = stem("checkout-hang", at("2026-07-31T02:15:30Z"));
        assert_eq!(s, "checkout-hang-260731-021530");

        // Splitting from the LEFT would take `checkout` as the whole prefix.
        // From the right the trailing fields are fixed-shape.
        let mut parts: Vec<&str> = s.rsplitn(3, '-').collect();
        parts.reverse();
        assert_eq!(parts, ["checkout-hang", "260731", "021530"]);
    }

    #[test]
    fn a_traversal_or_control_character_is_sanitised_before_it_reaches_a_path() {
        let p = resolve_prefix(Some("../../etc/passwd"), None, "t3");
        assert!(!p.contains('/'), "{p}");
        assert!(!p.contains(".."), "{p}");
        assert_eq!(p, "______etc_passwd");

        let p = resolve_prefix(Some("a\nb\u{0}c"), None, "t3");
        assert_eq!(p, "a_b_c");
    }

    #[test]
    fn an_over_long_prefix_is_truncated_rather_than_rejected() {
        let long = "a".repeat(200);
        let p = resolve_prefix(Some(&long), None, "t3");
        assert_eq!(p.len(), MAX_PREFIX_BYTES);
        assert!(p.chars().all(|c| c == 'a'));

        // Non-ASCII is collapsed to ASCII BEFORE the bound applies, so the byte
        // truncation cannot land inside a character. `ok` keeps the result
        // usable; without it the two rules interact and the whole prefix drops
        // through, which the next assertion pins.
        let unicode = "日本語ok".repeat(40);
        let p = resolve_prefix(Some(&unicode), None, "t3");
        assert_eq!(p.len(), MAX_PREFIX_BYTES);
        assert!(p.is_ascii(), "{p}");
        assert!(p.starts_with("___ok___ok"), "{p}");

        // And a long prefix whose first 48 bytes sanitise to nothing readable
        // drops through rather than filling the archive with underscores —
        // truncation happens first, so it is the TRUNCATED value that has to
        // pass the readability test.
        let all_wide = "日本語".repeat(40);
        assert_eq!(resolve_prefix(Some(&all_wide), None, "t3"), "t3");
    }

    #[test]
    fn the_prefix_falls_through_each_level_and_at_the_transitions() {
        // Parameter wins.
        assert_eq!(
            resolve_prefix(Some("checkout-hang"), Some("ht-server"), "t3"),
            "checkout-hang"
        );
        // No parameter → the registry's /case-name.
        assert_eq!(resolve_prefix(None, Some("ht-server"), "t3"), "ht-server");
        // Neither → the domain.
        assert_eq!(resolve_prefix(None, None, "t3"), "t3");

        // A level that sanitises to nothing usable DROPS THROUGH rather than
        // landing in the archive: `___-260731-*.md` is worse than `t3-*`.
        assert_eq!(
            resolve_prefix(Some("..."), Some("ht-server"), "t3"),
            "ht-server"
        );
        assert_eq!(resolve_prefix(Some("..."), Some("///"), "t3"), "t3");
        assert_eq!(resolve_prefix(Some(""), None, "t3"), "t3");

        // And an omitted prefix never produces a leading dash.
        assert!(!stem(
            &resolve_prefix(None, None, "t3"),
            at("2026-07-31T02:15:30Z")
        )
        .starts_with('-'));
        // Even when every level is unusable, which `is_valid_name` permits for
        // a domain.
        assert_eq!(resolve_prefix(None, None, "___"), "case");
    }

    #[test]
    fn a_rolled_id_is_four_hex_characters_and_does_not_repeat() {
        let a = roll_id();
        assert_eq!(a.len(), 4, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        // Not a proof of randomness, but a counter starting from a fixed value
        // would fail this.
        let rolls: std::collections::HashSet<String> = (0..64).map(|_| roll_id()).collect();
        assert!(rolls.len() > 50, "{} distinct in 64 rolls", rolls.len());
    }

    #[test]
    fn the_three_files_share_one_stem() {
        let s = stem("checkout-hang", at("2026-07-31T02:15:30Z"));
        assert_eq!(
            file_names(&s),
            [
                "checkout-hang-260731-021530.md",
                "checkout-hang-260731-021530.logdata.jsonl",
                "checkout-hang-260731-021530.spandata.jsonl",
            ]
        );
    }
}
