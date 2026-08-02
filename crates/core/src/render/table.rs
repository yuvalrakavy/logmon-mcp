//! Padded markdown tables.
//!
//! Aligned in a terminal like the plain columns the deleted CLI printed, and
//! native structure for an agent. **Nothing here is a port** — the deleted
//! `format::print_table` was `comfy_table` with box drawing and automatic
//! wrapping to the terminal width, so the algorithm is specified rather than
//! copied.

use super::escape::cell;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

/// One column: the header a reader sees, and the key it is read from.
pub type Col = (&'static str, &'static str);

/// Render `rows` under `headers`, padded to aligned columns.
///
/// - **Zero rows renders `empty`**, never an empty string: `(no domains)` is
///   information, a blank reply is indistinguishable from a broken renderer.
/// - **Width is `max(header, cells)` measured on the ESCAPED cell** — what is
///   printed is what is measured.
/// - **Every column is left-aligned.** Markdown's `|---:|` marker is
///   deliberately unused: right-aligning numbers means knowing which fields are
///   numeric, which is per-tool knowledge in a generic helper.
/// - **No wrapping and no truncation.** A 200-character filter DSL string makes
///   one long row. `comfy_table` wrapped it; markdown cannot without producing
///   invalid table syntax, and that is the accepted cost of one format serving
///   both surfaces.
///
/// Display width rather than `char` count, so a CJK service name does not
/// misalign every row beneath it.
pub fn table(headers: &[&str], rows: &[Vec<String>], empty: &str) -> String {
    if rows.is_empty() {
        return empty.to_string();
    }
    let escaped: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| cell(c)).collect())
        .collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            escaped
                .iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.width())
                .chain(std::iter::once(h.width()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let pad = |s: &str, w: usize| {
        // `saturating_sub`: a cell wider than its column would otherwise
        // underflow, and the never-panic rule outranks the fact that it cannot
        // happen with widths computed above.
        let mut out = s.to_string();
        out.push_str(&" ".repeat(w.saturating_sub(s.width())));
        out
    };

    let mut s = String::new();
    s.push_str("| ");
    s.push_str(
        &headers
            .iter()
            .enumerate()
            .map(|(i, h)| pad(h, widths[i]))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    s.push_str(" |\n|");
    for w in &widths {
        s.push_str(&"-".repeat(w + 2));
        s.push('|');
    }
    for r in &escaped {
        s.push_str("\n| ");
        s.push_str(
            &(0..headers.len())
                .map(|i| pad(r.get(i).map(String::as_str).unwrap_or(""), widths[i]))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        s.push_str(" |");
    }
    s
}

/// Pull `cols` out of each record in `result[records_key]`.
///
/// `None` unless the key really holds an array — the empty marker is a claim
/// ("this read returned nothing"), and a renderer may only make it about a
/// result it recognises.
pub fn table_read(
    result: &Value,
    records_key: &str,
    cols: &[Col],
    empty: &str,
) -> Option<String> {
    let records = result.get(records_key)?.as_array()?;
    let headers: Vec<&str> = cols.iter().map(|(h, _)| *h).collect();
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|rec| {
            cols.iter()
                .map(|(_, key)| {
                    rec.get(*key)
                        .filter(|v| !v.is_null())
                        .map(super::blocks::compact)
                        .unwrap_or_else(|| "—".to_string())
                })
                .collect()
        })
        .collect();
    let mut out = table(&headers, &rows, empty);
    out.push_str(&super::diagnostics(result, records_key));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_table_renders_its_marker() {
        assert_eq!(table(&["a", "b"], &[], "(no domains)"), "(no domains)");
    }

    #[test]
    fn columns_are_padded_to_the_widest_of_header_and_cells() {
        let rows = vec![
            vec!["default".into(), "config".into()],
            vec!["t1".into(), "ephemeral".into()],
        ];
        let out = table(&["name", "src"], &rows, "(none)");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "| name    | src       |");
        assert_eq!(lines[1], "|---------|-----------|");
        assert_eq!(lines[2], "| default | config    |");
        assert_eq!(lines[3], "| t1      | ephemeral |");
        // Every line is the same rendered width — the point of padding.
        let w = lines[0].width();
        for l in &lines {
            assert_eq!(l.width(), w, "ragged row: {l:?}");
        }
    }

    /// A header longer than every value still sets the column width.
    #[test]
    fn a_long_header_widens_its_column() {
        let rows = vec![vec!["1".into()]];
        let out = table(&["a_very_long_header"], &rows, "(none)");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0].width(), lines[2].width());
    }

    /// Display width, not `char` count: a CJK name is two columns per char, and
    /// counting chars misaligns every row beneath it.
    ///
    /// **The fixture is the test here.** A wide value that is not the widest
    /// cell proves nothing: the padding already measures display width, so a
    /// column sized by `chars().count()` still comes out aligned as long as some
    /// ASCII header or value dominates. The wide value has to BE the widest, so
    /// that under-counting the column leaves it unpadded and every ASCII row
    /// beneath it short. Caught by a negative control, not by reading.
    #[test]
    fn a_wide_character_does_not_misalign_the_rows_beneath_it() {
        let rows = vec![vec!["服务服务".into()], vec!["x".into()]];
        let out = table(&["n"], &rows, "(none)");
        let widths: Vec<usize> = out.lines().map(|l| l.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "rows disagree on width: {widths:?}\n{out}"
        );
    }

    /// A value cannot add a column, however it is spelled.
    ///
    /// **A raw `\|` proves nothing**, because it already reads as an escaped
    /// pipe — an unescaped renderer passes that input unchanged and the row
    /// still parses. The distinguishing inputs are a BARE pipe, which an
    /// unescaped renderer turns into a delimiter, and a backslash-then-pipe,
    /// which a naive escaper turns into one. Both are here; the first was
    /// missing until a negative control showed this test surviving with the
    /// escaping removed.
    #[test]
    fn a_hostile_value_stays_in_its_cell() {
        for hostile in ["a|b", r"id\|name", r"trailing\", "line\nbreak"] {
            let rows = vec![vec![hostile.into(), "x".into()]];
            let out = table(&["a", "b"], &rows, "(none)");
            for line in out.lines() {
                let b = line.as_bytes();
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
                assert_eq!(n, 3, "value {hostile:?} gained a column: {line:?}");
            }
            // And it stays ONE row: a newline would otherwise split the table.
            assert_eq!(out.lines().count(), 3, "value {hostile:?}: {out}");
        }
    }

    /// An absent field is `—`, not an empty cell that reads as an empty string —
    /// **and an explicitly null field is too.**
    ///
    /// The fixture used to carry only a MISSING key, so dropping the null filter
    /// left it green while a `null` rendered as the literal text `null`.
    #[test]
    fn an_absent_or_null_field_is_marked_rather_than_blank() {
        let result = json!({"items": [{"a": 1, "c": null}]});
        let out = table_read(
            &result,
            "items",
            &[("a", "a"), ("b", "b"), ("c", "c")],
            "(none)",
        )
        .expect("renders");
        assert!(out.contains("| 1 | — | — |"), "{out}");
        assert!(!out.contains("null"), "an explicit null rendered as text: {out}");
    }

    #[test]
    fn an_unrecognised_shape_renders_to_none() {
        assert_eq!(table_read(&json!(null), "items", &[("a", "a")], "(n)"), None);
        assert_eq!(table_read(&json!({}), "items", &[("a", "a")], "(n)"), None);
    }
}
