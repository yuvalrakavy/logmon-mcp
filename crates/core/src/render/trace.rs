//! `traces.get` and `traces.summary` — one trace, spans or breakdown.
//!
//! Ported from `git show 693223d^:crates/mcp/src/cli/traces.rs`.

use super::blocks;
use serde_json::Value;
use std::fmt::Write;

/// `traces.get` — the header, then the span tree as blocks.
///
/// The header is the part that was easy to lose: `(N spans, M logs)` is how a
/// reader tells "this trace is small" from "the read was cut", and the block
/// form's own cap says nothing about the logs.
pub fn render_trace(result: &Value) -> Option<String> {
    let spans = result.get("spans")?.as_array()?;
    let mut o = format!(
        "trace: {} ({} spans, {} logs)\n",
        result
            .get("trace_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        result.get("span_count").map_or("?".into(), blocks::compact),
        result.get("log_count").map_or("?".into(), blocks::compact),
    );
    o.push_str(&blocks::blocks(
        spans.iter().map(blocks::span_line).collect(),
        "(no spans)",
    ));
    o.push_str(&super::diagnostics(result, "spans"));
    Some(o)
}

/// `traces.summary` — where the wall clock went, largest share first.
pub fn render_summary(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    let breakdown = obj.get("breakdown")?.as_array()?;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "root: {}",
        obj.get("root_span").and_then(|v| v.as_str()).unwrap_or("?")
    );
    let _ = writeln!(
        o,
        "total: {}ms ({} spans)",
        obj.get("total_duration_ms")
            .and_then(|v| v.as_f64())
            .map_or("—".to_string(), |x| format!("{x:.1}")),
        obj.get("span_count").map_or("?".into(), blocks::compact)
    );
    for e in breakdown {
        let g = |k: &str| e.get(k).and_then(|v| v.as_f64());
        let _ = writeln!(
            o,
            "  {:>5}%  {}ms  self={}ms  {}{}",
            g("percentage").map_or("—".to_string(), |x| format!("{x:.1}")),
            g("total_time_ms").map_or("—".to_string(), |x| format!("{x:.1}")),
            g("self_time_ms").map_or("—".to_string(), |x| format!("{x:.1}")),
            e.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            if e.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                " (error)"
            } else {
                ""
            }
        );
    }
    // **The unattributed remainder, stated.** Without it the percentages do not
    // add up and a reader either distrusts the whole breakdown or, worse, treats
    // the largest listed span as the whole cost.
    if let Some(other) = obj.get("other_ms").and_then(|v| v.as_f64()) {
        if other > 0.1 {
            let _ = writeln!(o, "  ?       {other:.1}ms  (other)");
        }
    }
    Some(o.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_trace_leads_with_its_span_and_log_counts() {
        let out = render_trace(&json!({
            "trace_id": "7f3a", "span_count": 3, "log_count": 12,
            "spans": [{"seq": 1, "name": "root", "service_name": "svc",
                       "duration_ms": 210.0}],
        }))
        .expect("renders");
        assert!(out.starts_with("trace: 7f3a (3 spans, 12 logs)"), "{out}");
        assert!(out.contains("svc  root"), "{out}");
    }

    #[test]
    fn a_summary_orders_the_breakdown_and_names_the_remainder() {
        let out = render_summary(&json!({
            "root_span": "POST /export", "total_duration_ms": 2100.0, "span_count": 9,
            "breakdown": [
                {"name": "s3.PutObject", "percentage": 92.0, "total_time_ms": 1932.0,
                 "self_time_ms": 1930.0, "is_error": false},
                {"name": "db.query", "percentage": 5.0, "total_time_ms": 105.0,
                 "self_time_ms": 105.0, "is_error": true}
            ],
            "other_ms": 63.0,
        }))
        .expect("renders");
        assert!(out.contains("root: POST /export"), "{out}");
        assert!(out.contains("total: 2100.0ms (9 spans)"), "{out}");
        assert!(out.contains("s3.PutObject"), "{out}");
        assert!(out.contains("db.query (error)"), "an errored span must say so: {out}");
        assert!(out.contains("63.0ms  (other)"), "the remainder is stated: {out}");
    }

    /// Below the noise floor the remainder is not worth a line, and printing
    /// `0.0ms (other)` invites a reader to look for something that is not there.
    #[test]
    fn a_negligible_remainder_is_not_printed() {
        let out = render_summary(&json!({
            "root_span": "r", "total_duration_ms": 10.0, "span_count": 1,
            "breakdown": [], "other_ms": 0.05,
        }))
        .expect("renders");
        assert!(!out.contains("(other)"), "{out}");
    }

    #[test]
    fn an_unrecognised_shape_renders_to_none() {
        assert_eq!(render_trace(&json!({"trace_id": "x"})), None);
        assert_eq!(render_summary(&json!({"root_span": "x"})), None);
        assert_eq!(render_trace(&json!(null)), None);
        assert_eq!(render_summary(&json!(null)), None);
    }
}
