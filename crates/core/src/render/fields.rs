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

    out.push_str(&format!(
        "\n  {:<22}{:>8} {:>5}  {:>9}  {:<8} {}\n",
        "field", "present", "cov", "distinct", "kind", "top values"
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
        let promoted = f.get("promoted").and_then(Value::as_bool) == Some(true);
        let tops = f.get("top_values").map(top_values).unwrap_or_default();

        out.push_str(&format!(
            "  {:<22}{:>8} {:>4.0}%  {:>9}  {:<8} {}{}\n",
            clip(name, 22),
            present,
            cov,
            distinct,
            kind,
            tops,
            if promoted { "  [promoted]" } else { "" }
        ));
    }

    // The two facts a reader cannot derive from the rows, and would otherwise
    // have to be told by a human.
    out.push_str(
        "\n  A field at 0% exists but is never populated — group by a field with coverage.\n\
           [promoted] fields are lifted out of additional_fields by the parser.\n",
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
                 "kind": "string",
                 "top_values": [{"value": "svc::a", "count": 60}, {"value": "svc::b", "count": 40}]},
                {"field": "facility", "present": 0, "coverage_pct": 0.0, "distinct": 0,
                 "kind": "other", "top_values": []},
                {"field": "trace_id", "present": 50, "coverage_pct": 50.0,
                 "kind": "string", "top_values": [], "promoted": true},
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
    #[test]
    fn a_multibyte_value_is_clipped_without_panicking() {
        let mut r = reply();
        r["fields"][0]["top_values"] =
            json!([{"value": "日本語".repeat(40), "count": 1}]);
        let out = render(&r).expect("renders without panicking");
        assert!(out.contains('…'), "clipped: {out}");
    }
}
