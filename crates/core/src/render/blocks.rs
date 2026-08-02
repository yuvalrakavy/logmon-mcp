//! One line per record, for results that are not tabular.
//!
//! A log message contains `|` and newlines, so a markdown table would need heavy
//! escaping and still read badly. Blocks read the same to a terminal and to an
//! agent.

use serde_json::Value;

/// Records rendered before the list is cut. The deleted CLI used the same
/// figure, and `logs.recent` defaults to exactly this many.
pub const MAX_BLOCK_RECORDS: usize = 50;

/// Bytes rendered before the list is cut, whichever limit is reached first.
pub const MAX_BLOCK_BYTES: usize = 16 * 1024;

/// Join `lines` into a block list, truncating and **saying so**.
///
/// **The cap is not optional.** `logs.export` defaults `count` to `usize::MAX`,
/// so an uncapped renderer would turn every stored record into one string and
/// ship it — inverting the entire reason for rendering. And a cut that says
/// nothing is worse than no cut at all: 50 records of 1,000 read as the whole
/// answer, which is the false-completeness failure this project keeps finding.
///
/// `empty` is rendered for an empty list rather than an empty string: `(no logs)`
/// is information, a blank reply is indistinguishable from a broken renderer.
pub fn blocks(lines: Vec<String>, empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_string();
    }
    let total = lines.len();
    let mut out = String::new();
    let mut shown = 0usize;
    for line in lines.iter().take(MAX_BLOCK_RECORDS) {
        if !out.is_empty() && out.len() + line.len() > MAX_BLOCK_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        shown += 1;
    }
    if shown < total {
        // The daemon is emitting this, so it must not say "use --json" — that
        // is one front-end's vocabulary, and the MCP route has no such flag.
        out.push_str(&format!(
            "\n… {} more record(s) — narrow the filter or lower `count` to see them",
            total - shown
        ));
    }
    out
}

/// A `Value` as a `key=value` fragment: scalars bare, anything else compact JSON.
pub fn compact(v: &Value) -> String {
    scalar(v)
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => crate::render::escape::flatten(s),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// One stored log entry, as the block form renders it.
///
/// Reads fields off the serialized result rather than a typed struct, so an
/// unexpected shape yields a thinner line instead of an error.
pub fn log_line(e: &Value) -> String {
    let seq = e.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
    let ts = e
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        // Seconds precision: the sub-second digits cost an agent tokens and tell
        // a human nothing. `char_indices` rather than a byte slice, because the
        // never-panic rule outranks the fact that this field is always ASCII.
        .char_indices()
        .take_while(|(n, _)| *n < 19)
        .map(|(_, c)| c)
        .collect::<String>();
    // `Level` serializes as the variant NAME — "Warn", "Info" — so the
    // upper-casing is an explicit step, not something that falls out.
    let level = e
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_uppercase();
    let msg = e
        .get("message")
        .and_then(|v| v.as_str())
        .map(crate::render::escape::flatten)
        .unwrap_or_default();
    let mut line = format!("[{seq}] {ts} {level:<5} {msg}");

    // The continuation line: what the sender attached, plus the locators. Sorted
    // — `additional_fields` is a HashMap, so unsorted output differs between
    // renderings of identical data and no test could pin it.
    let mut extra: Vec<String> = Vec::new();
    if let Some(map) = e.get("additional_fields").and_then(|v| v.as_object()) {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for k in keys {
            if let Some(v) = map.get(k) {
                extra.push(format!("{k}={}", scalar(v)));
            }
        }
    }
    for key in ["file", "line", "trace_id", "span_id"] {
        if let Some(v) = e.get(key).filter(|v| !v.is_null()) {
            extra.push(format!("{key}={}", scalar(v)));
        }
    }
    if !extra.is_empty() {
        line.push_str("\n    ");
        line.push_str(&extra.join("  "));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, level: &str, msg: &str) -> Value {
        json!({
            "seq": seq,
            "timestamp": "2026-08-02T03:29:02.961654Z",
            "level": level,
            "message": msg,
        })
    }

    #[test]
    fn an_empty_list_renders_its_marker_not_nothing() {
        assert_eq!(blocks(Vec::new(), "(no logs)"), "(no logs)");
    }

    /// The cut, and the fact that it says so. Silently returning 50 of 1000 is
    /// the false-completeness failure; returning all 1000 inverts the reason
    /// for rendering at all.
    #[test]
    fn a_long_list_is_cut_and_the_remainder_is_named() {
        let lines: Vec<String> = (1..=200).map(|i| format!("line {i}")).collect();
        let out = blocks(lines, "(none)");
        assert_eq!(
            out.lines().filter(|l| l.starts_with("line ")).count(),
            MAX_BLOCK_RECORDS
        );
        assert!(out.contains("150 more record(s)"), "{out}");
        assert!(
            !out.contains("--json"),
            "the daemon must not speak one front-end's vocabulary: {out}"
        );
    }

    #[test]
    fn a_short_list_is_not_cut_and_says_nothing_about_more() {
        let out = blocks(vec!["a".into(), "b".into()], "(none)");
        assert_eq!(out, "a\nb");
    }

    /// A huge single record hits the byte cap before the record cap.
    #[test]
    fn the_byte_cap_cuts_before_the_record_cap_when_records_are_large() {
        let lines: Vec<String> = (0..10).map(|_| "x".repeat(4000)).collect();
        let out = blocks(lines, "(none)");
        assert!(out.contains("more record(s)"), "the byte cap did not fire");
        assert!(out.len() < MAX_BLOCK_BYTES + 4200, "{}", out.len());
    }

    /// `Level` serializes as the variant name, so upper-casing is a step that
    /// has to be taken rather than one that happens.
    #[test]
    fn a_record_line_upper_cases_the_level_and_trims_the_timestamp() {
        let line = log_line(&entry(5628067, "Warn", "worker stalled"));
        assert_eq!(line, "[5628067] 2026-08-02T03:29:02 WARN  worker stalled");
    }

    /// A newline in a message would break the one-line-per-record shape the
    /// whole block form rests on.
    #[test]
    fn a_message_spanning_lines_stays_one_record() {
        let line = log_line(&entry(1, "Error", "panicked at\n  src/main.rs:12"));
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.contains("panicked at   src/main.rs:12"), "{line}");
    }

    /// Where an application's own context lives. Sorted, because the source is a
    /// HashMap and unsorted output differs between renderings of identical data.
    #[test]
    fn additional_fields_reach_the_continuation_line_in_a_stable_order() {
        let mut e = entry(9, "Info", "ok");
        e["additional_fields"] = json!({"target": "store::test", "app": "store", "retries": 3});
        let line = log_line(&e);
        let cont = line.lines().nth(1).expect("a continuation line");
        assert_eq!(cont.trim(), "app=store  retries=3  target=store::test");
    }

    #[test]
    fn a_thin_record_renders_thin_rather_than_failing() {
        assert_eq!(log_line(&json!({})), "[0]  ?     ");
        assert_eq!(log_line(&json!("not an object")), "[0]  ?     ");
    }
}
