//! Renderer for `logs.profile`.
//!
//! **The MCP path returns this INSTEAD of the JSON** (`mcp/src/server.rs`), so
//! anything omitted here does not exist for the primary consumer. That is not a
//! stylistic point: this family has already paid for it once, when the ring
//! bounds were dropped from `logs.fields`'s rendering and the skill's own
//! instruction to read them became unfollowable.
//!
//! So the anti-bias figures are required output, not decoration. A reply
//! carrying `groups_total: 900` that rendered 20 rows and nothing else would
//! reproduce, inside the tool built to remove it, the exact defect the whole
//! aggregation family exists for: a sample with nothing bounding what was
//! missed.
//!
//! Held to the module's three rules — no byte slicing of a string, no bare
//! `usize` subtraction, no `unwrap`/`expect` — plus two of its own, in `clip`
//! and `disambiguate` below.

use super::escape::flatten;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::Write;

/// Longest group key shown before it is elided.
const KEY_WIDTH: usize = 40;
/// The key column, sized to what it actually holds.
///
/// **Measured, not assumed.** `{:<width$}` is a MINIMUM width, not a maximum,
/// so a constant narrower than the widest key does not truncate it — it just
/// stops padding, and every later column shifts right for that row alone. That
/// is what a fixed `KEY_WIDTH + 2` did once `disambiguate` began appending
/// ` #<first_seq>` on collisions: the collision test's own fixture produced a
/// 44-character key against a 42-character budget and drifted the seq column by
/// two, for exactly the rows the disambiguator exists to make comparable.
///
/// A fixed reservation wide enough for any suffix is the other wrong answer —
/// it pads ~21 blank characters onto every row for a suffix almost none of them
/// carry. So the width is the widest rendered key, which is bounded by
/// `KEY_WIDTH` plus the suffix and needs no constant at all.
fn key_col(keys: &[String]) -> usize {
    keys.iter()
        .map(|k| k.chars().count())
        .chain(std::iter::once("key".len()))
        .max()
        .unwrap_or(KEY_WIDTH)
        + 2
}
/// Longest exemplar shown.
const EXEMPLAR_WIDTH: usize = 52;
/// `<first>-<last>`. Wide enough for two 8-digit seqs, which the live buffer
/// already reaches — at 13 the pair overflowed its pad and shifted the exemplar
/// column right on data rows but not on the continuation line beneath them.
const SEQ_WIDTH: usize = 18;
/// Columns before the exemplar: count, share, then one per level shown.
const COUNT_WIDTH: usize = 8;
const SHARE_WIDTH: usize = 7;
const LEVEL_WIDTH: usize = 7;

/// Severity order, and the spelling `LevelCounts` uses.
const LEVELS: &[(&str, &str)] = &[
    ("error", "Error"),
    ("warn", "Warn"),
    ("info", "Info"),
    ("debug", "Debug"),
    ("trace", "Trace"),
];

/// Flatten, then truncate on CHARACTER boundaries.
///
/// Flatten is not optional and applies whether or not the string is clipped:
/// group keys, exemplars and the `suppressed` text are all emitter- or
/// caller-derived, and one line break reaching a fixed-width column turns a row
/// into two, the second with no columns. `&s[..n]` panics mid-codepoint.
fn clip(s: &str, max: usize) -> String {
    let s = flatten(s);
    if s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Make clipped keys unique again.
///
/// Measured on a live buffer: two `Error` messages share a 70-character prefix
/// and diverge at character 71, so at any clip width up to 70 they render as
/// the same string. Two rows reading identically is worse than a wide table —
/// the reader either thinks the tool is broken or, worse, treats them as one
/// group.
///
/// One pass: clip, then append ` #<first_seq>` to every row whose clipped key
/// collides. `first_seq` is unique per row — each record lands in exactly one
/// group, so two groups' minima cannot coincide — and it feeds
/// `get_log_context` directly, so the disambiguator doubles as the pointer to
/// go look. A width search that widens until distinct was considered and
/// dropped: a second mechanism for one property, and one unlucky pair widens
/// the column for every row.
fn disambiguate(rows: &[&Value]) -> Vec<String> {
    let clipped: Vec<String> = rows
        .iter()
        .map(|g| clip(g.get("key").and_then(Value::as_str).unwrap_or("?"), KEY_WIDTH))
        .collect();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for c in &clipped {
        *seen.entry(c.as_str()).or_insert(0) += 1;
    }
    clipped
        .iter()
        .zip(rows)
        .map(|(c, g)| {
            if seen.get(c.as_str()).copied().unwrap_or(0) > 1 {
                let seq = g.get("first_seq").and_then(Value::as_u64).unwrap_or(0);
                format!("{c} #{seq}")
            } else {
                c.clone()
            }
        })
        .collect()
}

/// The seq span of the matched population, ` (seq 1-101)`, or empty when
/// nothing matched.
///
/// **Rendered because it is emitted.** The reply carries whole-population
/// `first_seq`/`last_seq`, and the MCP path returns this rendering INSTEAD of
/// the JSON — so an unrendered field does not exist for the primary consumer.
/// A mutation campaign found it: replacing the whole-population bounds with
/// defaults left every test green, because nothing asserted them and nothing
/// printed them. That is the same defect this family already paid for once,
/// when `logs.fields` emitted its ring bounds and rendered none of them.
///
/// It is also the span the per-row `seqs` column is read against: a row running
/// 91-97 means one thing in a population spanning 1-101 and another in one
/// spanning 90-101.
fn span(result: &Value) -> String {
    match (
        result.get("first_seq").and_then(Value::as_u64),
        result.get("last_seq").and_then(Value::as_u64),
    ) {
        (Some(f), Some(l)) => format!(" (seq {f}-{l})"),
        _ => String::new(),
    }
}

fn levels_cell(levels: &Value, present: &[&str]) -> String {
    present
        .iter()
        .map(|k| match levels.get(k).and_then(Value::as_u64).unwrap_or(0) {
            0 => format!("{:>LEVEL_WIDTH$}", ""),
            n => format!("{n:>7}"),
        })
        .collect()
}

pub fn render(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    let empty = Vec::new();
    let groups = result
        .get("groups")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let u = |k: &str| result.get(k).and_then(Value::as_u64);
    let matched = u("matched").unwrap_or(0);
    let scanned = u("scanned").unwrap_or(0);
    let axis = result.get("grouped_by").and_then(Value::as_str);

    let mut o = String::new();

    // The header carries every figure a reader cannot derive from the rows.
    match axis {
        Some(a) => {
            let keys = result
                .get("group_keys")
                .and_then(Value::as_array)
                .map(|ks| {
                    ks.iter()
                        .filter_map(Value::as_str)
                        .map(|k| clip(k, KEY_WIDTH))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let named = if keys.is_empty() {
                format!("`{a}`")
            } else {
                format!("`{a}` [{keys}]")
            };
            let _ = write!(o, "log profile by {named} — {matched} matched{} of {scanned} scanned", span(result));
        }
        None => {
            let _ = write!(
                o,
                "log profile — ungrouped — {matched} matched{} of {scanned} scanned",
                span(result)
            );
        }
    }

    // The absent share, where it cannot be missed. On a typical buffer most
    // axes are absent from over 90% of records, and a reader who does not see
    // that reads the remaining rows as the whole population.
    // **Selected on `reserved`, never on the rendered key.** GELF validates
    // nothing after the `_`, so an emitter field valued literally `__absent__`
    // produces a REAL row whose key is character-for-character the reserved
    // bucket's — and real rows sort first, so a label match returns the wrong
    // one and this share comes out inverted. The projection carries a typed key
    // precisely to keep the two apart; matching on the string threw that away.
    let absent = groups
        .iter()
        .find(|g| g.get("reserved").and_then(Value::as_str) == Some("absent"))
        .and_then(|g| g.get("count").and_then(Value::as_u64));
    if let Some(n) = absent {
        if matched > 0 {
            let pct = 100.0 * n as f64 / matched as f64;
            let carry = matched.saturating_sub(n);
            let _ = write!(o, ", {carry} carry it, {n} __absent__ ({pct:.1}%)");
        }
    }

    // "top N of M" whenever the rows are a sample. `groups_total` counts the
    // reserved buckets, so the comparison is against every row shown.
    if let Some(total) = u("groups_total") {
        let noun = if total == 1 { "group" } else { "groups" };
        if total > groups.len() as u64 {
            let _ = write!(o, ", top {} of {total} {noun}", groups.len());
        } else {
            let _ = write!(o, ", {total} {noun}");
        }
    }
    if let Some(total) = u("buffer_total") {
        let _ = write!(o, " ({total} in buffer)");
    }
    o.push('\n');

    // The whole-population level breakdown and time window.
    //
    // **Emitted means rendered.** The MCP path returns this instead of the
    // JSON, and the per-row columns cannot reconstruct these: a truncated row
    // set does not sum to the population, and the row `seqs` columns say
    // nothing about wall-clock. The tool's own description promises "counts, a
    // per-level breakdown, seq/time bounds", and the ungrouped call — which is
    // what `logmon-mcp logs profile` with no arguments does — has no rows at
    // all, so without this line it printed one sentence and nothing else.
    if let Some(levels) = result.get("levels") {
        let counts: Vec<String> = LEVELS
            .iter()
            .filter_map(|(k, label)| {
                match levels.get(k).and_then(Value::as_u64).unwrap_or(0) {
                    0 => None,
                    n => Some(format!("{label} {n}")),
                }
            })
            .collect();
        if !counts.is_empty() {
            let _ = write!(o, "  levels: {}", counts.join("  "));
            if let (Some(f), Some(l)) = (
                result.get("first_time").and_then(Value::as_str),
                result.get("last_time").and_then(Value::as_str),
            ) {
                let _ = write!(o, "   window: {} .. {}", flatten(f), flatten(l));
            }
            o.push('\n');
        }
    }

    // Eviction first: every figure below it describes a population missing
    // records, so it qualifies the table rather than footnoting it.
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        let n = result
            .get("evicted_before_window")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "an unknown number of".into());
        let _ = writeln!(o, "  TRUNCATED: {n} record(s) rolled off before this window");
    }
    // `truncated` needs a resolved lower bound to mean anything, so on a filter
    // without one it is false however much rolled off. These are the only way a
    // reader sees a wrapped ring.
    let lost = u("lost_below").unwrap_or(0);
    if let (Some(oldest), Some(newest)) = (u("buffer_oldest_seq"), u("buffer_newest_seq")) {
        if lost > 0 {
            let _ = writeln!(
                o,
                "  WRAPPED: the buffer holds seq {oldest}..{newest}; everything below \
                 {lost} has been evicted"
            );
        }
    }
    if result.get("cardinality_capped").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            o,
            "  CAPPED: a cardinality cap folded keys into __overflow__, so that row \
             aggregates an arbitrary, arrival-ordered set"
        );
    }

    if !groups.is_empty() {
        // Only the levels actually present get a column, so a buffer of one
        // level does not print four empty ones.
        let present: Vec<&str> = LEVELS
            .iter()
            .filter(|(k, _)| {
                groups
                    .iter()
                    .any(|g| g.get("levels").and_then(|l| l.get(k)).and_then(Value::as_u64) > Some(0))
            })
            .map(|(k, _)| *k)
            .collect();
        let heads: String = LEVELS
            .iter()
            .filter(|(k, _)| present.contains(k))
            .map(|(_, label)| format!("{label:>LEVEL_WIDTH$}"))
            .collect();

        let refs: Vec<&Value> = groups.iter().collect();
        let keys = disambiguate(&refs);

        // Computed once and shared by the header, the rows and the continuation
        // line, so the three cannot drift apart — which is exactly what
        // hand-repeated widths did on the first pass.
        // Sized AFTER disambiguation, so a suffixed key is what the column is
        // measured against rather than what overflows it.
        let kcol = key_col(&keys);

        let lead = 2 // the row's own indent
            + kcol
            + COUNT_WIDTH
            + SHARE_WIDTH
            + present.len() * LEVEL_WIDTH
            + 2
            + SEQ_WIDTH
            + 1;

        let _ = writeln!(
            o,
            "\n  {:<kcol$}{:>COUNT_WIDTH$}{:>SHARE_WIDTH$}{heads}  {:<SEQ_WIDTH$} exemplar",
            "key",
            "count",
            "share",
            "seqs",
        );
        for (g, key) in groups.iter().zip(keys) {
            let count = g.get("count").and_then(Value::as_u64).unwrap_or(0);
            let share = if matched == 0 {
                0.0
            } else {
                100.0 * count as f64 / matched as f64
            };
            let levels = g.get("levels").cloned().unwrap_or(Value::Null);
            let first = g.get("first_seq").and_then(Value::as_u64).unwrap_or(0);
            let last = g.get("last_seq").and_then(Value::as_u64).unwrap_or(0);
            let fx = g.get("first_exemplar").and_then(Value::as_str).unwrap_or("");
            let lx = g.get("last_exemplar").and_then(Value::as_str).unwrap_or("");

            let _ = writeln!(
                o,
                // `SHARE_WIDTH - 1` because the `%` sign is the last character
                // of that cell — the header pads to SHARE_WIDTH, so hardcoding
                // the number here is how the two came to differ by one.
                "  {:<kcol$}{count:>COUNT_WIDTH$}{:>width$.1}%{}  {:<SEQ_WIDTH$} {}",
                key,
                share,
                levels_cell(&levels, &present),
                format!("{first}-{last}"),
                clip(fx, EXEMPLAR_WIDTH),
                width = SHARE_WIDTH - 1
            );
            // The second line IS the signal: printed only when the group's
            // shape changed across the window, which is what the pair of
            // exemplars exists to show. Indented to `lead` so it lines up under
            // the exemplar it differs from, rather than under a column that
            // happened to be the same width.
            if lx != fx {
                let _ = writeln!(o, "{}{}", " ".repeat(lead), clip(lx, EXEMPLAR_WIDTH));
            }
        }
    }

    // The honesty channel. A wholly-absent axis lands here, and it is usually
    // the most actionable line on the page.
    if let Some(list) = result.get("suppressed").and_then(Value::as_array) {
        if !list.is_empty() {
            let _ = writeln!(o, "\n  not reported:");
            for item in list {
                let s = |k: &str| item.get(k).and_then(Value::as_str);
                let _ = writeln!(
                    o,
                    "    {} — {}",
                    flatten(s("field").unwrap_or("?")),
                    flatten(s("reason").unwrap_or("?"))
                );
                if let Some(r) = s("remedy") {
                    let _ = writeln!(o, "      try: {}", flatten(r));
                }
            }
        }
    }

    // **The backstop.** This renderer is hand-written per field, so nothing
    // structural stops a key from vanishing — and one already had, in the
    // sibling this is modelled on. A guard on the whole result turns "the
    // renderer does not know about it" into a line rather than a silence.
    const KNOWN: &[&str] = &[
        "groups",
        "grouped_by",
        "group_keys",
        "groups_total",
        "cardinality_capped",
        "levels",
        "matched",
        "scanned",
        "buffer_total",
        "first_seq",
        "last_seq",
        "first_time",
        "last_time",
        "buffer_oldest_seq",
        "buffer_newest_seq",
        "lost_below",
        "truncated",
        "evicted_before_window",
        "suppressed",
    ];
    let mut rest: Vec<String> = obj
        .keys()
        .filter(|k| !KNOWN.contains(&k.as_str()) && !k.starts_with('_'))
        .filter(|k| !obj[k.as_str()].is_null())
        .map(|k| format!("{k}={}", flatten(&obj[k.as_str()].to_string())))
        .collect();
    rest.sort();
    if !rest.is_empty() {
        let _ = writeln!(o, "  {}", rest.join("  "));
    }

    Some(o.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reserved(key: &str, kind: &str, count: u64, first: u64, last: u64) -> Value {
        let mut g = group(key, count, first, last);
        g["reserved"] = json!(kind);
        g
    }

    fn group(key: &str, count: u64, first: u64, last: u64) -> Value {
        json!({
            "key": key, "count": count,
            "levels": {"trace": 0, "debug": 0, "info": count, "warn": 0, "error": 0},
            "first_seq": first, "first_time": "2026-08-03T10:00:00Z", "first_exemplar": "one",
            "last_seq": last, "last_time": "2026-08-03T10:01:00Z", "last_exemplar": "one",
        })
    }

    fn reply() -> Value {
        json!({
            "groups": [group("svc::a", 60, 1, 90), group("svc::b", 40, 2, 88)],
            "grouped_by": "field", "group_keys": ["target"],
            "groups_total": 2, "cardinality_capped": false,
            "levels": {"trace": 0, "debug": 0, "info": 100, "warn": 0, "error": 0},
            "matched": 100, "scanned": 250, "buffer_total": 250,
            "lost_below": 0, "truncated": false,
        })
    }

    /// A renderer nobody dispatches to renders nothing.
    ///
    /// `for_method` matches on the method STRING, so a typo or a missing arm
    /// is invisible to every test in this module — they all call `render`
    /// directly. The MCP path returns the rendering instead of the JSON, so an
    /// unwired renderer does not degrade to JSON here: it degrades to JSON for
    /// the agent, silently, which is the whole surface.
    #[test]
    fn the_method_is_wired_into_for_method() {
        assert!(
            crate::render::for_method("logs.profile", &reply()).is_some(),
            "logs.profile has no arm in render::for_method"
        );
    }

    #[test]
    fn renders_the_axis_counts_and_rows() {
        let out = render(&reply()).expect("renders");
        assert!(out.contains("log profile by `field` [target]"), "{out}");
        assert!(out.contains("100 matched of 250 scanned"), "{out}");
        assert!(out.contains("svc::a"), "{out}");
        assert!(out.contains("Info"), "the level column appears: {out}");
    }

    /// The anti-bias figure. A reply whose rows are a SAMPLE must say so, or it
    /// reproduces — inside the tool built to remove it — the defect this whole
    /// family exists for: a sample with nothing bounding what was missed.
    #[test]
    fn a_truncated_row_set_says_top_n_of_m() {
        let mut r = reply();
        r["groups_total"] = json!(900);
        let out = render(&r).expect("renders");
        assert!(
            out.contains("top 2 of 900 groups"),
            "the reader must be able to tell 2-of-2 from 2-of-900: {out}"
        );
    }

    /// `__absent__` is usually the largest bucket, and the header is where a
    /// reader cannot miss it.
    #[test]
    fn the_absent_share_is_in_the_header() {
        let mut r = reply();
        r["groups"] = json!([
            group("svc::a", 20, 1, 90),
            reserved("__absent__", "absent", 80, 3, 99)
        ]);
        let out = render(&r).expect("renders");
        assert!(out.contains("80 __absent__ (80.0%)"), "{out}");
        assert!(out.contains("20 carry it"), "{out}");
    }

    /// The absent share is read off `reserved`, never off the rendered key.
    ///
    /// GELF validates nothing after the `_`, so a field valued literally
    /// `__absent__` is a REAL row whose key matches the reserved bucket's
    /// character for character — and real rows sort first, so a label match
    /// returns the wrong one. Here the truth is 3 carry the axis and 1 lacks
    /// it; a label match reports the exact inverse, and the MCP path returns
    /// this rendering INSTEAD of the JSON, so the inverted number is all the
    /// agent sees.
    #[test]
    fn the_absent_share_ignores_a_real_row_that_merely_looks_reserved() {
        let mut r = reply();
        r["matched"] = json!(4);
        r["groups"] = json!([
            group("__absent__", 3, 1, 3),                 // a real value, sorts first
            reserved("__absent__", "absent", 1, 4, 4),    // the actual bucket
        ]);
        let out = render(&r).expect("renders");
        assert!(
            out.contains("1 __absent__ (25.0%)"),
            "the bucket holds 1 of 4; reading the first row by label reports 3 \
             and inverts the whole header: {out}"
        );
        assert!(out.contains("3 carry it"), "{out}");
    }

    /// The whole-population figures reach the rendering.
    ///
    /// The per-row columns cannot reconstruct them: a truncated row set does
    /// not sum to the population, and the ungrouped call — `logmon-mcp logs
    /// profile` with no arguments — has no rows at all, so without this it
    /// printed one sentence and stopped.
    #[test]
    fn the_population_levels_and_buffer_total_are_rendered() {
        let mut r = reply();
        r["levels"] = json!({"trace": 0, "debug": 0, "info": 90, "warn": 4, "error": 7});
        r["first_time"] = json!("2026-08-03T09:00:00Z");
        r["last_time"] = json!("2026-08-03T10:00:00Z");
        let out = render(&r).expect("renders");
        assert!(out.contains("Error 7"), "the population breakdown: {out}");
        assert!(out.contains("Warn 4") && out.contains("Info 90"), "{out}");
        assert!(out.contains("(250 in buffer)"), "{out}");
        assert!(out.contains("window: "), "the wall-clock span: {out}");
        assert!(
            !out.contains("Debug 0"),
            "a level nobody logged is noise, not information: {out}"
        );
    }

    /// An ungrouped call is the DEFAULT call, and it must still say something.
    #[test]
    fn the_ungrouped_call_still_reports_the_population() {
        let mut r = reply();
        r["groups"] = json!([]);
        r["grouped_by"] = Value::Null;
        r["group_keys"] = Value::Null;
        r["groups_total"] = Value::Null;
        r["levels"] = json!({"trace": 0, "debug": 0, "info": 1, "warn": 0, "error": 1});
        let out = render(&r).expect("renders");
        assert!(out.contains("Error 1") && out.contains("Info 1"), "{out}");
        assert!(
            out.lines().filter(|l| !l.trim().is_empty()).count() >= 2,
            "one line with no level breakdown is what this call used to print: {out}"
        );
    }

    /// Two keys differing only past the clip width must not render identically.
    ///
    /// The live pair: 70 shared characters, diverging at 71. A reader seeing
    /// two identical rows either distrusts the tool or, worse, treats them as
    /// one group.
    #[test]
    fn colliding_clipped_keys_are_disambiguated_by_seq() {
        let long = "AudioMediaSystem /Services/AirZone: malformed State on AirZone/State/5";
        let mut r = reply();
        r["groups"] = json!([
            group(&format!("{long}36875849: executing"), 1, 41, 41),
            group(&format!("{long}20098633: executing"), 1, 77, 77),
        ]);
        let out = render(&r).expect("renders");
        assert!(out.contains("#41") && out.contains("#77"), "{out}");

        let rows: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("AudioMediaSystem"))
            .collect();
        assert_eq!(rows.len(), 2, "{out}");
        assert_ne!(rows[0], rows[1], "two distinct groups must not render alike");

        // **And the suffix must fit its column.** `{:<width$}` is a MINIMUM
        // width: a key clipped to KEY_WIDTH and then suffixed is longer than a
        // KEY_WIDTH-sized pad, so nothing truncates it and every later column
        // shifts right — for exactly the rows the disambiguator exists to make
        // comparable. Measured on this fixture before the fix: a 44-character
        // key against a 42-character budget, drifting the seq column by two.
        let header = out
            .lines()
            .find(|l| l.contains("exemplar"))
            .expect("header");
        let want = header.find("seqs").expect("seqs column");
        for row in &rows {
            let got = row.find('#').and_then(|h| row[h..].find(' ').map(|s| h + s + 1));
            assert_eq!(
                got.map(|g| g.max(want)),
                Some(want),
                "a disambiguated row must not push its columns past the header's:\n\
                 {header}\n{row}"
            );
        }
    }

    /// The population's seq span reaches the rendering.
    ///
    /// The reply carries whole-population bounds and this renderer printed none
    /// of them — found by a mutation campaign, and it is the same defect this
    /// family paid for once already on `logs.fields`. The MCP path returns the
    /// rendering instead of the JSON, so an unrendered field does not exist.
    #[test]
    fn the_population_seq_span_is_rendered() {
        let mut r = reply();
        r["first_seq"] = json!(1);
        r["last_seq"] = json!(101);
        let out = render(&r).expect("renders");
        assert!(
            out.contains("(seq 1-101)"),
            "a row running 91-97 means one thing in a population spanning 1-101 \
             and another in one spanning 90-101: {out}"
        );
    }

    /// And an empty population says nothing rather than `(seq 0-0)`.
    #[test]
    fn a_population_with_no_bounds_renders_no_span() {
        let out = render(&reply()).expect("renders");
        assert!(!out.contains("(seq "), "{out}");
    }

    /// `1 groups` is the kind of thing a reader notices and a test does not.
    /// Caught by running the real binary against a real buffer.
    #[test]
    fn a_single_group_is_not_pluralised() {
        let mut r = reply();
        r["groups"] = json!([group("only", 100, 1, 9)]);
        r["groups_total"] = json!(1);
        let out = render(&r).expect("renders");
        // The property is the noun, not what follows it — an earlier version of
        // this assertion pinned a trailing comma and broke when another field
        // was added after the count.
        assert!(out.contains("1 group"), "{out}");
        assert!(!out.contains("1 groups"), "{out}");
    }

    /// And a non-colliding key keeps its plain form, or the check above would
    /// be indistinguishable from appending a seq to every row.
    #[test]
    fn a_unique_key_is_not_disambiguated() {
        let out = render(&reply()).expect("renders");
        assert!(!out.contains('#'), "no disambiguator where none is needed: {out}");
    }

    /// Every caller-derived string is flattened, clipped or not — the
    /// `suppressed` text is the one that never passes through `clip`.
    #[test]
    fn no_caller_string_carries_a_line_break_into_the_output() {
        for bad in ["a\nb", "a\u{2028}b"] {
            let mut r = reply();
            r["groups"] = json!([group(bad, 1, 1, 1)]);
            r["groups"][0]["first_exemplar"] = json!(bad);
            r["suppressed"] = json!([{"field": bad, "reason": bad, "remedy": bad}]);
            let out = render(&r).expect("renders");
            for line in out.lines() {
                assert!(
                    !line.trim_start().starts_with('b'),
                    "a break survived for {bad:?}:\n{out}"
                );
            }
        }
    }

    /// The pair of exemplars prints as one line when the group did not change,
    /// and two when it did — so the second line IS the signal.
    #[test]
    fn a_second_exemplar_prints_only_when_the_group_changed_shape() {
        let out = render(&reply()).expect("renders");
        assert_eq!(out.matches("one").count(), 2, "one line per row: {out}");

        let mut r = reply();
        r["groups"][0]["last_exemplar"] = json!("run script failed (retry 4)");
        let out = render(&r).expect("renders");
        assert!(out.contains("retry 4"), "the change is shown: {out}");
    }

    /// A wholly-absent axis is announced with its remedy, which is the most
    /// actionable line the reply can carry.
    #[test]
    fn the_suppressed_channel_and_its_remedy_are_rendered() {
        let mut r = reply();
        r["suppressed"] = json!([{
            "field": "group_keys[0]",
            "reason": "`line` did not appear in any of the 9347 matched records",
            "remedy": "an additional field named `line` covers 99.97% — use group_by=\"field\"",
        }]);
        let out = render(&r).expect("renders");
        assert!(out.contains("not reported:"), "{out}");
        assert!(out.contains("did not appear"), "{out}");
        assert!(out.contains("try: "), "the remedy is the point: {out}");
    }

    /// The backstop: a field added to the reply later cannot vanish silently.
    #[test]
    fn an_unknown_reply_key_still_reaches_the_output() {
        let mut r = reply();
        r["some_future_field"] = json!(42);
        let out = render(&r).expect("renders");
        assert!(
            out.contains("some_future_field=42"),
            "the renderer is hand-written per field, so an unknown key must \
             surface rather than disappear: {out}"
        );
    }

    /// An ungrouped reply still describes its population.
    #[test]
    fn an_ungrouped_reply_renders_without_a_table() {
        let mut r = reply();
        r["groups"] = json!([]);
        r["grouped_by"] = Value::Null;
        r["group_keys"] = Value::Null;
        r["groups_total"] = Value::Null;
        let out = render(&r).expect("renders");
        assert!(out.contains("ungrouped"), "{out}");
        assert!(out.contains("100 matched"), "{out}");
    }

    /// A wrapped ring is announced even though `truncated` is false, because
    /// `truncated` needs a resolved lower bound and a default call has none.
    #[test]
    fn a_wrapped_ring_is_announced_from_the_buffer_bounds() {
        let mut r = reply();
        r["lost_below"] = json!(500);
        r["buffer_oldest_seq"] = json!(500);
        r["buffer_newest_seq"] = json!(1500);
        let out = render(&r).expect("renders");
        assert!(out.contains("WRAPPED"), "{out}");
        assert!(out.contains("500"), "{out}");
    }

    /// Multibyte keys clip on character boundaries. The widths sweep several
    /// strides because `KEY_WIDTH - 1` divisible by a character's byte length
    /// makes a byte-slicing implementation land on a boundary by arithmetic
    /// coincidence — which is how this exact test stayed green under its own
    /// mutation once already, in the sibling renderer.
    #[test]
    fn multibyte_keys_clip_without_panicking_at_every_width() {
        for s in ["é".repeat(80), "日".repeat(80), "🎉".repeat(80), "aé日🎉".repeat(30)] {
            let mut r = reply();
            r["groups"] = json!([group(&s, 1, 1, 1)]);
            let out = render(&r).expect("renders without panicking");
            assert!(out.contains('…'), "clipped: {out}");
        }
    }
}
