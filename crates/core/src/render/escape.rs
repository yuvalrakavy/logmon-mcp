//! Caller text on its way into a rendered document or table.
//!
//! One implementation, shared by the case-document renderer and by `_display`,
//! so a fix to either reaches both. The case document found these hazards first;
//! `_display` renders the same untrusted strings into the same markdown.

/// Collapse every line break YAML and markdown recognise into a space, and trim.
///
/// A `reason` containing a newline and `---` would otherwise open a second
/// front-matter block, and one spelling `\n## Evidence` would produce a second
/// Evidence section — the document's own structure, forged by a value it is
/// reporting. The three Unicode line breaks are here for the reason they are in
/// `yaml_str`: they are not C0, so a check keyed on C0 misses them.
pub fn flatten(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}' => ' ',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Caller text on its way into a markdown table cell.
///
/// **Backslashes are escaped first, and that order is the whole correctness of
/// this function.** Escaping only the pipe turns `id\|name` into `id\\|name`,
/// where markdown consumes `\\` as one literal backslash and the `|` behind it
/// is an unescaped delimiter — the row gains a column and every later cell
/// slides under the wrong header. Not hypothetical: the filter DSL compiles Rust
/// regexes, so matching a literal pipe in a pipe-delimited log format is written
/// `m~/id\|name/`, and a filter string is exactly the kind of caller text that
/// reaches a cell. A value ending in a lone backslash is the mirror case, where
/// it escapes the row's own closing delimiter.
pub fn cell(s: &str) -> String {
    flatten(s).replace('\\', "\\\\").replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count delimiters the way a markdown renderer does: scan forward, and let
    /// a backslash consume the character after it. The naive "is the previous
    /// byte a backslash" test encodes the SAME mistake as a buggy escaper and
    /// reports a broken row as fine.
    fn delimiters(row: &str) -> usize {
        let b = row.as_bytes();
        let (mut n, mut i) = (0, 0);
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'|' => {
                    n += 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        n
    }

    #[test]
    fn no_value_can_add_a_column() {
        for value in [
            "plain",
            "a | b",
            r"id\|name",
            r"trailing\",
            r"\\|both",
            "line\nbreak",
        ] {
            let row = format!("| {} | next |", cell(value));
            assert_eq!(
                delimiters(&row),
                3,
                "value {value:?} changed the row's shape: {row:?}"
            );
        }
    }

    #[test]
    fn flatten_collapses_every_break_yaml_recognises() {
        assert_eq!(flatten("a\u{85}b\u{2028}c\u{2029}d\ne"), "a b c d e");
        assert_eq!(flatten("  padded  "), "padded");
    }
}
