//! Renderer for `logs.fields`.
//!
//! A dedicated renderer rather than a `LISTS` entry: `top_values` is an array
//! of objects, which the generic table cannot flatten, and coverage wants to
//! read as a percentage beside its raw count rather than as a bare float.
//!
//! Held to the module's three rules — no byte slicing of a string, no bare
//! `usize` subtraction, no `unwrap`/`expect`.

use serde_json::Value;

/// Longest value shown inline before it is elided.
const VALUE_WIDTH: usize = 34;

/// Truncate on CHARACTER boundaries. `&s[..n]` panics mid-codepoint, and a
/// field value is arbitrary emitter text.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn top_values(v: &Value) -> String {
    let Some(arr) = v.as_array() else {
        return String::new();
    };
    arr.iter()
        .filter_map(|t| {
            let value = t.get("value")?.as_str()?;
            let count = t.get("count")?.as_u64()?;
            Some(format!("{}({count})", clip(value, VALUE_WIDTH)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn render(result: &Value) -> Option<String> {
    let fields = result.get("fields")?.as_array()?;

    let matched = result.get("matched").and_then(Value::as_u64).unwrap_or(0);
    let scanned = result.get("scanned").and_then(Value::as_u64).unwrap_or(0);
    let total = result
        .get("buffer_total")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str(&format!(
        "log fields — {matched} matched of {scanned} scanned ({total} in buffer)\n"
    ));

    // Eviction is a fact about the WINDOW, not about the fields, so it leads
    // rather than hiding under the table: every figure below describes a
    // population that is missing records.
    if result.get("truncated").and_then(Value::as_bool) == Some(true) {
        let n = result
            .get("evicted_before_window")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "an unknown number of".to_string());
        out.push_str(&format!(
            "  WINDOW TRUNCATED — {n} record(s) below the requested start had already \
             rolled off; these figures describe what remains\n"
        ));
    }

    if fields.is_empty() {
        out.push_str("  (no fields — the buffer is empty or the filter matched nothing)\n");
        return Some(out);
    }

    if result.get("names_capped").and_then(Value::as_bool) == Some(true) {
        out.push_str(
            "  FIELD NAMES CAPPED — more distinct field names than the cap; some rows \
             are missing\n",
        );
    }

    // `filter=` is what a caller pastes, and it is NOT always the field name:
    // the built-in `file` is reached by `fi`, while an additional field named
    // `file` is reached by `file`. Leading with the selector rather than the
    // name is the difference between a map you can act on and one you have to
    // translate.
    out.push_str(&format!(
        "\n  {:<10}{:<20}{:>8} {:>5}  {:>9}  {:<8} {}\n",
        "filter=", "field", "present", "cov", "distinct", "kind", "top values"
    ));

    for f in fields {
        let name = f.get("field").and_then(Value::as_str).unwrap_or("?");
        let present = f.get("present").and_then(Value::as_u64).unwrap_or(0);
        let cov = f.get("coverage_pct").and_then(Value::as_f64).unwrap_or(0.0);
        // `null` distinct is the cap, and it must not render as 0 — that would
        // read as "no distinct values" for the highest-cardinality fields.
        let distinct = match f.get("distinct").and_then(Value::as_u64) {
            Some(d) => d.to_string(),
            None => "capped".to_string(),
        };
        let kind = f.get("kind").and_then(Value::as_str).unwrap_or("?");
        let source = f.get("source").and_then(Value::as_str).unwrap_or("");
        // A row with no selector cannot be filtered on at all. Rendering the
        // field name there would invite exactly the silent-empty filter this
        // column exists to prevent.
        let selector = match f.get("selector").and_then(Value::as_str) {
            Some(s) => clip(s, 9).to_string(),
            None => "(none)".to_string(),
        };
        let tops = f.get("top_values").map(top_values).unwrap_or_default();

        out.push_str(&format!(
            "  {:<10}{:<20}{:>8} {:>4.0}%  {:>9}  {:<8} {}{}\n",
            selector,
            clip(name, 20),
            present,
            cov,
            distinct,
            kind,
            tops,
            if source == "promoted" { "  [promoted]" } else { "" }
        ));
    }

    // The three facts a reader cannot derive from the rows.
    out.push_str(
        "\n  Filter with the `filter=` spelling, not the field name — the built-in `file` \
           is `fi`,\n  while an additional field named `file` is `file`. They are different \
           fields and both may appear.\n  A field at 0% exists but is never populated.  \
           `(none)`: no log filter reaches it —\n  for trace_id/span_id use \
           get_recent_logs(trace_id=...).\n",
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply() -> Value {
        json!({
            "matched": 100, "scanned": 250, "buffer_total": 250, "truncated": false,
            "fields": [
                {"field": "target", "present": 100, "coverage_pct": 100.0, "distinct": 4,
                 "kind": "string", "source": "additional", "selector": "target",
                 "top_values": [{"value": "svc::a", "count": 60}, {"value": "svc::b", "count": 40}]},
                {"field": "facility", "present": 0, "coverage_pct": 0.0, "distinct": 0,
                 "kind": "string", "source": "builtin", "selector": "fa", "top_values": []},
                {"field": "trace_id", "present": 50, "coverage_pct": 50.0,
                 "kind": "string", "source": "promoted", "top_values": []},
            ]
        })
    }

    #[test]
    fn renders_counts_coverage_and_top_values() {
        let out = render(&reply()).expect("renders");
        assert!(out.contains("100 matched of 250 scanned"), "{out}");
        assert!(out.contains("svc::a(60)"), "top values inline: {out}");
        assert!(out.contains("[promoted]"), "{out}");
    }

    /// A `null` distinct is the CAP, and rendering it as `0` would read as "no
    /// distinct values" for exactly the highest-cardinality fields.
    #[test]
    fn a_capped_distinct_renders_as_capped_not_zero() {
        let mut r = reply();
        r["fields"][0]["distinct"] = Value::Null;
        let out = render(&r).expect("renders");
        assert!(out.contains("capped"), "{out}");
    }

    /// Eviction is a fact about the window, not a footnote: every figure below
    /// it describes a population that is missing records.
    #[test]
    fn truncation_is_announced_before_the_table() {
        let mut r = reply();
        r["truncated"] = json!(true);
        r["evicted_before_window"] = json!(17);
        let out = render(&r).expect("renders");
        let warn = out.find("TRUNCATED").expect("announced");
        let table = out.find("top values").expect("table header");
        assert!(warn < table, "the warning must precede the figures it qualifies");
        assert!(out.contains("17"), "and say how many: {out}");
    }

    /// Values are arbitrary emitter text; `&s[..n]` would panic mid-codepoint.
    ///
    /// **The character widths here are the test.** An earlier version used only
    /// `日本語` — 3-byte characters — and `VALUE_WIDTH - 1 == 33` is divisible
    /// by 3, so a byte-slicing implementation landed on a character boundary
    /// *by arithmetic coincidence* and the test stayed green under the very
    /// mutation it existed to catch. Sweeping several widths makes the property
    /// hold independently of what `VALUE_WIDTH` happens to be.
    #[test]
    fn multibyte_values_are_clipped_without_panicking_at_every_width() {
        for s in [
            "é".repeat(60),      // 2-byte
            "日".repeat(60),     // 3-byte
            "🎉".repeat(60),     // 4-byte — 33 is not divisible by 4
            "aé日🎉".repeat(20), // mixed, so no single stride divides evenly
        ] {
            let mut r = reply();
            r["fields"][0]["top_values"] = json!([{"value": s, "count": 1}]);
            let out = render(&r).expect("renders without panicking");
            assert!(out.contains('…'), "clipped: {out}");
        }
    }

    /// The re-model's payoff in the rendering: the column a caller pastes is the
    /// SELECTOR, and a row with none says so rather than offering its name.
    #[test]
    fn the_selector_column_is_rendered_and_absent_ones_are_marked() {
        let out = render(&reply()).expect("renders");
        assert!(out.contains("filter="), "the column is labelled: {out}");
        assert!(out.contains("(none)"), "a row with no selector is marked: {out}");
        assert!(
            out.contains("get_recent_logs(trace_id="),
            "and the legend names the route that does work: {out}"
        );
    }
}
