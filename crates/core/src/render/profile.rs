//! The narrative renderers: a collector's numbers, and a comparison of two.
//!
//! **Ported from the deleted CLI, not re-derived.** These encode judgements that
//! are not recoverable from the result shape — `(TRUNCATED)` beside a sample
//! count, suppressed fields printed with their remedy, a struck-through delta
//! shown with the threshold that struck it. Re-describing them from field names
//! is how you get a plausible renderer that is quietly wrong. Originals at
//! `git show 693223d^:crates/mcp/src/cli/collectors.rs`.
//!
//! These read the serialized `Value` like every other renderer here. The design
//! permits deserializing into `ProfileResult`/`CollectorsDiffResult` instead,
//! but reading fields keeps the never-fail rule true by construction rather than
//! by a `.ok()?` that silently refuses a result whose shape drifted by one
//! optional field.

use super::blocks::compact;
use serde_json::Value;
use std::fmt::Write;

fn f(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64()
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}

fn n(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// `{:.1}` where a number is a duration, `—` where it is absent.
///
/// **A null is a different claim from a zero**, and the deleted renderer's own
/// comment says the reader has to be able to tell which they are looking at —
/// so `unwrap_or(0.0)` is exactly what must NOT be ported.
fn ms(v: &Value, key: &str) -> String {
    f(v, key).map_or("—".to_string(), |x| format!("{x:.1}"))
}

fn ms3(v: &Value, key: &str) -> String {
    f(v, key).map_or("—".to_string(), |x| format!("{x:.3}"))
}

/// `collectors.get` and `traces.profile` — one renderer, two methods, because
/// both return `ProfileResult` and the deleted CLI called `print_profile` from
/// both call sites.
pub fn render(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    // `filter` is on every profile reply and on nothing else this renderer is
    // wired to; without it this is not a profile and it must not narrate one.
    let filter = obj.get("filter")?.as_str()?;
    let label = s(result, "collector").unwrap_or("(ad-hoc)");
    let level = s(result, "level").unwrap_or("?");

    let mut o = String::new();
    let _ = writeln!(o, "{label}  {filter}  level={level}");
    if let Some(d) = s(result, "description") {
        let _ = writeln!(o, "  {d}");
    }
    let wall = result
        .get("window")
        .and_then(|w| f(w, "wall_ms"))
        .map_or("—".to_string(), |x| format!("{:.1}", x / 1000.0));
    let _ = writeln!(
        o,
        "  matched {} over {wall}s of wall time, nesting {}",
        n(result, "matched").map_or("—".to_string(), |x| x.to_string()),
        s(result, "nesting").unwrap_or("—")
    );

    if let Some(e) = obj.get("exact") {
        let _ = writeln!(
            o,
            "  exact:     total {}ms  avg {}ms  min {}ms  max {}ms  errors {}",
            ms(e, "total_ms"),
            ms3(e, "avg_ms"),
            ms3(e, "min_ms"),
            ms(e, "max_ms"),
            n(e, "error_count").map_or("—".to_string(), |x| x.to_string())
        );
    }
    if let Some(e) = obj.get("estimated") {
        let _ = writeln!(
            o,
            "  estimated: p50 {}ms  p80 {}ms  p95 {}ms  p99 {}ms  (±{}%)",
            ms3(e, "p50_ms"),
            ms3(e, "p80_ms"),
            ms3(e, "p95_ms"),
            ms3(e, "p99_ms"),
            ms(e, "alpha_pct")
        );
    }
    if let Some(sm) = obj.get("sampled") {
        let _ = writeln!(
            o,
            "  sampled:   {} records{}  p50 {}ms  p95 {}ms",
            n(sm, "sample_count").map_or("—".to_string(), |x| x.to_string()),
            // The one word that says the percentiles above describe part of the
            // data rather than all of it.
            if sm.get("complete").and_then(|c| c.as_bool()) == Some(false) {
                " (TRUNCATED)"
            } else {
                ""
            },
            ms3(sm, "p50_ms"),
            ms3(sm, "p95_ms")
        );
        if let Some(self_ms) = f(sm, "self_ms") {
            let _ = writeln!(
                o,
                "             self {self_ms:.1}ms across {} nested matches",
                n(sm, "nested_matches").unwrap_or(0)
            );
        }
        if let (Some(w), Some(c)) = (f(sm, "wall_union_ms"), f(sm, "achieved_concurrency")) {
            let _ = writeln!(o, "             wall union {w:.1}ms, concurrency {c:.2}x");
        }
        if n(sm, "overlapping_child_spans").unwrap_or(0) > 0 {
            let _ = writeln!(
                o,
                "             {} child span(s) clipped, {}ms outside their parent",
                n(sm, "overlapping_child_spans").unwrap_or(0),
                ms(sm, "overlapping_child_ms")
            );
        }
    }

    if let Some(i) = obj.get("ingest") {
        let lost = n(i, "drops_in_window").unwrap_or(0)
            + n(i, "shed_batches").unwrap_or(0)
            + n(i, "malformed_dropped").unwrap_or(0);
        if lost > 0 {
            let _ = writeln!(
                o,
                "  INGEST LOSS in this window: {} dropped, {} batches shed, {} malformed \
                 (per-{}, unfiltered)",
                n(i, "drops_in_window").unwrap_or(0),
                n(i, "shed_batches").unwrap_or(0),
                n(i, "malformed_dropped").unwrap_or(0),
                s(i, "attribution").unwrap_or("?")
            );
        }
        if n(i, "malformed_timestamps").unwrap_or(0) > 0
            || n(i, "negative_duration_spans").unwrap_or(0) > 0
        {
            let _ = writeln!(
                o,
                "  unusable input: {} malformed timestamps, {} negative durations",
                n(i, "malformed_timestamps").unwrap_or(0),
                n(i, "negative_duration_spans").unwrap_or(0)
            );
        }
    }

    if let Some(g) = s(result, "grouped_by") {
        let _ = writeln!(o, "\n  by {g}:");
        for row in obj.get("groups").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
            // **Flattened, and clipped to the column it is formatted into.**
            // A group key is an interned span-attribute VALUE, so it is
            // emitter-controlled: a newline in one splits the row and drops
            // the count and duration onto a continuation line with no columns.
            // `{key:<40}` is a minimum width, so a long key shifts every cell
            // after it rather than being cut.
            //
            // This is the defect fixed in `render/fields.rs`, which was live
            // here the whole time — the design gate's claim that "every sibling
            // renderer flattens" was false, and `traces.profile` and
            // `collectors.get` both read this path.
            let key = clip_cell(s(row, "key").unwrap_or("?"), 40);
            let count = row
                .get("exact")
                .and_then(|e| n(e, "count"))
                .or_else(|| row.get("sampled").and_then(|s| n(s, "sample_count")))
                .unwrap_or(0);
            match (
                row.get("exact").and_then(|e| f(e, "total_ms")),
                row.get("sampled").and_then(|s| f(s, "self_ms")),
            ) {
                (Some(t), _) => {
                    let _ = writeln!(o, "    {key:<40} {count:>8}  {t:>12.1}ms");
                }
                (None, Some(sf)) => {
                    let _ = writeln!(o, "    {key:<40} {count:>8}  {sf:>12.1}ms self");
                }
                _ => {
                    let _ = writeln!(o, "    {key:<40} {count:>8}");
                }
            }
        }
    }

    // Last, and never omitted: a null is a different claim from a zero, and the
    // reader has to be able to tell which they are looking at.
    let suppressed = obj
        .get("suppressed")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty());
    if let Some(list) = suppressed {
        let _ = writeln!(o, "\n  not reported:");
        for item in list {
            // Flattened too: `reason` and `remedy` quote caller-supplied names
            // — a `group_keys` entry, an attribute — so they carry emitter text
            // into the same table.
            let _ = writeln!(
                o,
                "    {} — {}",
                crate::render::escape::flatten(s(item, "field").unwrap_or("?")),
                crate::render::escape::flatten(s(item, "reason").unwrap_or("?"))
            );
            if let Some(rem) = s(item, "remedy") {
                let _ = writeln!(o, "      try: {}", crate::render::escape::flatten(rem));
            }
        }
    }
    for w in obj
        .get("warnings")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        let _ = writeln!(o, "  warning: {}", compact(w));
    }
    if obj.get("cardinality_capped").and_then(|v| v.as_bool()) == Some(true) {
        let _ = writeln!(o, "  note: a cardinality cap folded values into __overflow__");
    }

    // **The backstop.** This renderer is hand-written per field, like
    // `status::render`, so nothing structural stops a key from vanishing — and
    // one already had: `collectors.get` carries the armed `threshold`, which the
    // deleted CLI never printed either, so porting its structure faithfully
    // ported the omission. A guard on the whole result is what turns "the
    // renderer does not know about it" into a line rather than a silence.
    const KNOWN: &[&str] = &[
        "collector",
        "filter",
        "level",
        "description",
        "matched",
        "nesting",
        "window",
        "exact",
        "estimated",
        "sampled",
        "ingest",
        "grouped_by",
        "groups",
        "suppressed",
        "warnings",
        "cardinality_capped",
    ];
    let mut rest: Vec<String> = obj
        .keys()
        .filter(|k| !KNOWN.contains(&k.as_str()) && !k.starts_with('_'))
        .filter(|k| !obj[k.as_str()].is_null())
        .map(|k| format!("{k}={}", compact(&obj[k.as_str()])))
        .collect();
    rest.sort();
    if !rest.is_empty() {
        let _ = writeln!(o, "  {}", rest.join("  "));
    }
    Some(o.trim_end().to_string())
}

/// `collectors.diff` — a comparison, leading with what makes it untrustworthy.
///
/// A reader who stops after the first screen must not come away with a delta
/// they should not have believed, so the doubt goes above the numbers rather
/// than below them.
pub fn render_diff(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    let (a, b) = (obj.get("a")?, obj.get("b")?);
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{}  ->  {}",
        s(a, "spec").unwrap_or("?"),
        s(b, "spec").unwrap_or("?")
    );
    for (which, arm) in [("a", a), ("b", b)] {
        let _ = writeln!(
            o,
            "  {which}: {} | {} | {}",
            s(arm, "aggregation").unwrap_or("?"),
            s(arm, "level").unwrap_or("?"),
            s(arm, "filter").unwrap_or("?")
        );
    }
    if s(a, "level") != s(b, "level") {
        let _ = writeln!(o, "  compared at: {}", s(result, "level").unwrap_or("?"));
    }

    if obj.get("trustworthy").and_then(|v| v.as_bool()) == Some(false) {
        let _ = writeln!(
            o,
            "\nNOT TRUSTWORTHY — read the notes before acting on any number below."
        );
    }
    for m in obj.get("marks").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        match s(m, "overridden_by") {
            Some(flag) => {
                let _ = writeln!(
                    o,
                    "  [{}] {} (permitted by --{flag})",
                    s(m, "kind").unwrap_or("?"),
                    s(m, "detail").unwrap_or("")
                );
            }
            None => {
                let _ = writeln!(
                    o,
                    "  [{}] {}",
                    s(m, "kind").unwrap_or("?"),
                    s(m, "detail").unwrap_or("")
                );
            }
        }
    }

    o.push('\n');
    o.push_str(&diff_rows(obj.get("rows")));
    for g in obj.get("groups").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
        let presence = s(g, "presence").unwrap_or("both");
        let label = if presence == "both" {
            String::new()
        } else {
            format!("  ({presence})")
        };
        let _ = write!(o, "\n\n{}{label}\n", s(g, "key").unwrap_or("?"));
        o.push_str(&diff_rows(g.get("rows")));
    }
    if n(result, "overflow_rows_suppressed").unwrap_or(0) > 0 {
        let _ = write!(
            o,
            "\n\n{} group row(s) not compared: a cardinality cap folded them into \
             __overflow__, which aggregates a different arbitrary set on each arm",
            n(result, "overflow_rows_suppressed").unwrap_or(0)
        );
    }
    for item in obj
        .get("suppressed")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        let _ = match s(item, "remedy") {
            Some(rem) => write!(
                o,
                "\n\n{}: {} — {rem}",
                s(item, "field").unwrap_or("?"),
                s(item, "reason").unwrap_or("?")
            ),
            None => write!(
                o,
                "\n\n{}: {}",
                s(item, "field").unwrap_or("?"),
                s(item, "reason").unwrap_or("?")
            ),
        };
    }

    // The floors last, because they qualify everything above: a reader who has
    // already seen a delta needs to know what would have had to be true for it
    // to mean anything.
    for (which, arm) in [("a", a), ("b", b)] {
        let _ = match arm.get("floor").filter(|v| !v.is_null()) {
            Some(fl) => match f(fl, "cv_pct") {
                Some(cv) => write!(
                    o,
                    "\n\nfloor {which}: {} runs, {:.1}-{:.1}ms (mean {:.1}), CV {cv:.1}%\n  {}",
                    n(fl, "runs").unwrap_or(0),
                    f(fl, "min").unwrap_or(0.0),
                    f(fl, "max").unwrap_or(0.0),
                    f(fl, "mean").unwrap_or(0.0),
                    s(fl, "caveat").unwrap_or("")
                ),
                None => write!(o, "\n\nfloor {which}: UNKNOWN (one run)"),
            },
            None => write!(
                o,
                "\n\nfloor {which}: UNKNOWN — {} is a single run. Take several and compare \
                 `<collector>@*` to find out whether a difference is real.",
                s(arm, "spec").unwrap_or("?")
            ),
        };
    }
    Some(o.trim_end().to_string())
}

/// Caller text on its way into a fixed-width column.
///
/// Flatten then truncate on CHARACTER boundaries — `&s[..n]` panics
/// mid-codepoint, and both group keys and span names are emitter-controlled.
fn clip_cell(s: &str, max: usize) -> String {
    let s = crate::render::escape::flatten(s);
    if s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn diff_rows(rows: Option<&Value>) -> String {
    let Some(rows) = rows.and_then(|v| v.as_array()) else {
        return "(no rows)".to_string();
    };
    let cell = |v: Option<f64>| v.map_or("—".to_string(), |x| format!("{x:.3}"));
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let delta = match f(r, "delta") {
                None => "—".to_string(),
                Some(d) => {
                    let pct = f(r, "delta_pct").map_or(String::new(), |p| format!(" ({p:+.1}%)"));
                    // The strike-through is the suppression made visible, and
                    // the threshold that caused it prints beside it — a delta
                    // struck through with a bound other than the displayed one
                    // is forbidden.
                    if r.get("suppressed").and_then(|v| v.as_bool()) == Some(true) {
                        format!("[{d:+.3}{pct}]")
                    } else {
                        format!("{d:+.3}{pct}")
                    }
                }
            };
            let threshold = match (f(r, "threshold_abs"), f(r, "threshold_pct")) {
                (Some(abs), Some(pct)) => format!("{abs:.3} ({pct:.1}% of mean)"),
                _ => "unknown".to_string(),
            };
            vec![
                s(r, "metric").unwrap_or("?").to_string(),
                s(r, "tier").unwrap_or("?").to_string(),
                cell(f(r, "a")),
                cell(f(r, "b")),
                delta,
                threshold,
                f(r, "error_bound_pct").map_or("—".to_string(), |e| format!("{e:.0}%")),
                if r.get("trustworthy").and_then(|v| v.as_bool()) == Some(false) {
                    "!"
                } else {
                    ""
                }
                .to_string(),
            ]
        })
        .collect();
    super::table::table(
        &[
            "metric", "tier", "a", "b", "delta", "threshold", "err/delta", "",
        ],
        &table,
        "(no rows)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile() -> Value {
        json!({
            "collector": "lookup", "filter": "sn=Lookup", "level": "tree",
            "matched": 40, "nesting": "flat", "window": {"wall_ms": 12000.0},
            "exact": {"total_ms": 1840.0, "avg_ms": 46.0, "min_ms": 12.0,
                      "max_ms": 210.0, "error_count": 0},
            "sampled": {"sample_count": 40, "complete": true,
                        "p50_ms": 44.0, "p95_ms": 190.0},
            "suppressed": [],
            "warnings": [],
        })
    }

    #[test]
    fn a_profile_leads_with_the_headline_and_the_window() {
        let out = render(&profile()).expect("renders");
        assert!(out.starts_with("lookup  sn=Lookup  level=tree"), "{out}");
        assert!(out.contains("matched 40 over 12.0s of wall time"), "{out}");
        assert!(out.contains("exact:     total 1840.0ms"), "{out}");
    }

    /// The word that says the percentiles above describe part of the data.
    #[test]
    fn a_truncated_sample_says_so_beside_its_count() {
        let mut p = profile();
        p["sampled"]["complete"] = json!(false);
        assert!(render(&p).expect("renders").contains("(TRUNCATED)"));
        assert!(!render(&profile()).expect("renders").contains("TRUNCATED"));
    }

    /// **A null is a different claim from a zero.** The original wrote
    /// `unwrap_or(0.0)`, which the same file warns against elsewhere; porting it
    /// verbatim would have shipped an absent average as a measured `0.000ms`.
    #[test]
    fn an_absent_number_renders_as_absent_not_as_zero() {
        let mut p = profile();
        p["exact"]["avg_ms"] = json!(null);
        let out = render(&p).expect("renders");
        assert!(out.contains("avg —ms"), "{out}");
        assert!(!out.contains("avg 0.000ms"), "a null became a measurement: {out}");
    }

    /// Suppressed fields are never omitted — the whole reason this instrument is
    /// trusted is that it says what it declined to compute, and why.
    #[test]
    fn a_suppressed_field_prints_its_reason_and_its_remedy() {
        let mut p = profile();
        p["suppressed"] = json!([{
            "field": "self_ms",
            "reason": "no matched span has a matched parent",
            "remedy": "broaden the filter so nested spans are matched too"
        }]);
        let out = render(&p).expect("renders");
        assert!(out.contains("not reported:"), "{out}");
        assert!(out.contains("self_ms — no matched span"), "{out}");
        assert!(out.contains("try: broaden the filter"), "{out}");
    }

    #[test]
    fn ingest_loss_is_reported_only_when_something_was_lost() {
        let mut p = profile();
        p["ingest"] = json!({"drops_in_window": 0, "shed_batches": 0,
                             "malformed_dropped": 0, "attribution": "domain"});
        assert!(!render(&p).expect("renders").contains("INGEST LOSS"));
        p["ingest"]["shed_batches"] = json!(4);
        assert!(render(&p).expect("renders").contains("INGEST LOSS"));
    }

    #[test]
    fn a_shape_that_is_not_a_profile_renders_to_none() {
        assert_eq!(render(&json!(null)), None);
        assert_eq!(render(&json!({"collector": "x"})), None);
    }

    /// A hand-written renderer has no structural guard against a key vanishing,
    /// and one already had: `collectors.get` carries the armed `threshold`,
    /// which the deleted CLI never printed either — so porting its structure
    /// faithfully ported the omission.
    #[test]
    fn a_key_this_renderer_does_not_know_still_reaches_the_reader() {
        let mut p = profile();
        p["threshold"] = json!({"metric": "avg_ms", "op": "gte", "value": 250.0});
        p["some_future_field"] = json!(7);
        let out = render(&p).expect("renders");
        assert!(out.contains("avg_ms"), "the armed threshold vanished:\n{out}");
        assert!(out.contains("some_future_field=7"), "{out}");
    }

    fn diff() -> Value {
        json!({
            "a": {"spec": "put@before", "aggregation": "single", "level": "tree",
                  "filter": "sn=Put", "floor": null},
            "b": {"spec": "put@after", "aggregation": "single", "level": "tree",
                  "filter": "sn=Put", "floor": null},
            "level": "tree", "trustworthy": true, "marks": [],
            "rows": [{"metric": "avg_ms", "tier": "exact", "a": 1840.0, "b": 47.0,
                      "delta": -1793.0, "delta_pct": -97.4, "suppressed": false,
                      "trustworthy": true}],
            "groups": [], "suppressed": [], "overflow_rows_suppressed": 0
        })
    }

    /// Doubt goes ABOVE the numbers: a reader who stops after the first screen
    /// must not come away with a delta they should not have believed.
    #[test]
    fn an_untrustworthy_diff_says_so_before_any_number() {
        let mut d = diff();
        d["trustworthy"] = json!(false);
        let out = render_diff(&d).expect("renders");
        let warn = out.find("NOT TRUSTWORTHY").expect("the warning");
        let number = out.find("-1793").expect("the delta");
        assert!(warn < number, "the delta was readable before the caveat:\n{out}");
    }

    /// A suppressed delta is struck through, never hidden — a row that vanished
    /// would read as a row that did not exist.
    #[test]
    fn a_suppressed_delta_is_bracketed_rather_than_removed() {
        let mut d = diff();
        d["rows"][0]["suppressed"] = json!(true);
        let out = render_diff(&d).expect("renders");
        assert!(out.contains("[-1793.000"), "{out}");
    }

    /// A single run has no spread, and UNKNOWN is the honest answer — not zero.
    #[test]
    fn a_single_run_floor_reads_unknown_and_says_what_to_do() {
        let out = render_diff(&diff()).expect("renders");
        assert!(out.contains("floor a: UNKNOWN"), "{out}");
        assert!(out.contains("Take several and compare"), "{out}");
        assert!(!out.contains("CV 0.0%"), "a single run claimed a spread: {out}");
    }

    #[test]
    fn a_shape_that_is_not_a_diff_renders_to_none() {
        assert_eq!(render_diff(&json!({"a": {}})), None);
        assert_eq!(render_diff(&json!(null)), None);
    }

    /// A group key is an interned span-ATTRIBUTE value, so it is
    /// emitter-controlled — and a newline in one used to split the row, landing
    /// the count and duration on a continuation line with no columns.
    ///
    /// This is the defect `render/fields.rs` was fixed for; it was live here
    /// the whole time, on the path `traces.profile` and `collectors.get` share.
    /// The claim that "every sibling renderer flattens" was simply false.
    #[test]
    fn a_group_key_cannot_split_the_row_or_shift_its_columns() {
        // The sentinel after each break is deliberately distinctive. A first
        // version guarded with `starts_with('b')` and fired on the literal
        // section header `by group:` — a check that cannot tell the defect from
        // the page it is printed on is not a check.
        for (bad, tail) in [
            ("HEAD\n  TAILMARKER at foo.rs:12", "TAILMARKER"),
            ("HEAD\u{2028}TAILMARKER", "TAILMARKER"),
            ("HEAD\rTAILMARKER", "TAILMARKER"),
        ] {
            let r = json!({
                "filter": "ALL", "level": "tree", "matched": 4, "nesting": "detected",
                "grouped_by": "group",
                "groups": [{"key": bad, "exact": {"count": 3, "total_ms": 1.0}}],
                "suppressed": [{"field": bad, "reason": bad, "remedy": bad}],
            });
            let out = render(&r).expect("renders");
            for line in out.lines() {
                assert!(
                    !line.trim_start().starts_with(tail),
                    "a break survived into the table for {bad:?}:\n{out}"
                );
            }
            // Positive evidence, not just the absence of a split: the row still
            // carries its own count and duration on the line with its key.
            let row = out
                .lines()
                .find(|l| l.contains("HEAD"))
                .unwrap_or_else(|| panic!("no key row in:\n{out}"));
            assert!(
                row.contains(tail) && row.contains('3') && row.contains("1.0ms"),
                "the whole row must survive on one line: {row}"
            );
        }

        // And a long key is CUT rather than allowed to shift the columns —
        // `{key:<40}` is a minimum width, not a maximum.
        let r = json!({
            "filter": "ALL", "level": "tree", "matched": 1, "nesting": "detected",
            "grouped_by": "group",
            "groups": [{"key": "x".repeat(120), "exact": {"count": 1, "total_ms": 1.0}}],
        });
        let out = render(&r).expect("renders");
        assert!(out.contains('…'), "clipped: {out}");
    }
}
