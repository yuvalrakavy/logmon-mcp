//! Presentation, supplied by the daemon.
//!
//! A client that shows a reply to a reader — the CLI without `--json`, the MCP
//! route — asks for it with `RpcRequest::display`, and gets a `_display` string
//! beside the result. **The shim keeps no knowledge of any result shape**; that
//! knowledge lives here, next to the handler that produced the result.
//!
//! # What gets a renderer
//!
//! The primary client is an AI agent; the CLI human is secondary. For an agent a
//! small flat JSON object is *already* the ideal reply — unambiguous,
//! machine-parseable, cheap. What costs an agent is volume and noise. So:
//!
//! > **Render where rendering removes noise. Leave a small flat result as JSON.**
//!
//! Measured on live replies: `status.get` renders 6.7× smaller, and what it
//! drops is 47 tool names an MCP client already holds. Record reads render ~2×
//! smaller with no JSON punctuation to mis-parse. A `filters.add` reply is 50
//! bytes and rendering it would save 20 while risking a hidden field — so
//! mutations return JSON, by rule rather than by omission.
//!
//! # The absent renderer is the mechanism
//!
//! [`for_method`] returning `None` means no `_display` key, and the client falls
//! back to JSON exactly as it does against a broker too old to know the flag.
//! That is what lets renderers land one method at a time, with no flag day and
//! no coordinated release.

pub mod blocks;
pub mod escape;
pub mod status;
pub mod table;

use serde_json::Value;
use table::Col;

/// The columns each list read renders, and the marker for an empty one.
///
/// **Not every field.** A rendering that reproduced the whole record would save
/// nothing and read worse than the JSON; these are the columns that answer the
/// question the read is for. A caller who needs a field that is not here asks
/// for the reply unrendered — the structured result is still right there.
const LISTS: &[(&str, &str, &[Col], &str)] = &[
    (
        "domains.list",
        "domains",
        &[
            ("name", "name"),
            ("src", "source"),
            ("logs", "log_count"),
            ("spans", "span_count"),
            ("oldest", "oldest_seq"),
            ("newest", "newest_seq"),
            ("idle_s", "idle_secs"),
            ("stale", "stale"),
        ],
        "(no domains)",
    ),
    (
        "sessions.list",
        "sessions",
        &[
            ("session", "name"),
            ("connected", "connected"),
            ("triggers", "trigger_count"),
            ("filters", "filter_count"),
            ("queued", "queue_size"),
            ("last_seen_s", "last_seen_secs_ago"),
        ],
        "(no sessions)",
    ),
    // `filters.list` and `triggers.list` are NOT here — see `dsl_list`.
    (
        "bookmarks.list",
        "bookmarks",
        &[
            ("bookmark", "qualified_name"),
            ("seq", "seq"),
            ("created", "created_at"),
            ("description", "description"),
        ],
        "(no bookmarks)",
    ),
    (
        "collectors.list",
        "collectors",
        &[
            ("collector", "name"),
            ("filter", "filter"),
            ("level", "level"),
            ("domain", "domain"),
            ("matched", "matched"),
            ("snapshots", "snapshots"),
            ("armed", "armed_at"),
        ],
        "(no collectors)",
    ),
];

/// Every key on a log/span read that is NOT the record array.
///
/// **The never-drop rule is structural, not a list of field names.** A renderer
/// drops only the records — which the reader is getting rendered — and states
/// every other key. An earlier draft of the design enumerated five names in
/// prose and was wrong in both directions: three of the five are not on
/// `LogsRecentResult` at all, while the two that matter most there were missing.
///
/// The two that matter, and why, from the primary client's side:
///
/// - **`scanned`.** A filter matching nothing over a live buffer returns
///   `{logs: [], count: 0, scanned: 4000}`. Rendering only `(no logs)` tells an
///   agent the system is quiet while 4,000 records flowed past.
/// - **`cursor_advanced_to`.** The call MUTATED session read position. An agent
///   not told will be baffled when its next call returns nothing.
fn diagnostics(result: &Value, record_key: &str) -> String {
    let Some(obj) = result.as_object() else {
        return String::new();
    };
    let mut bits: Vec<String> = Vec::new();
    for (k, v) in obj {
        if k == record_key || v.is_null() || k.starts_with('_') {
            continue;
        }
        bits.push(format!("{k}={}", blocks::compact(v)));
    }
    // Deterministic: a serde_json map preserves insertion order only by feature,
    // and a rendering that reorders between calls cannot be pinned by a test.
    bits.sort();
    if bits.is_empty() {
        String::new()
    } else {
        format!("\n{}", bits.join("  "))
    }
}

/// The derived note the deleted CLI computed rather than read.
///
/// `empty && scanned > 0` is not a field on any result — it is the heuristic that
/// tells "nothing matched" from "nothing is happening", and it has to be ported
/// as logic or it is lost.
///
/// **It names the query, not the filter.** The original ran only over
/// `LogsExport { count, filter }`, where a filter was the only thing that could
/// have excluded anything. `logs.export` has since gained `from_seq`/`to_seq`,
/// and porting the wording verbatim moved a true sentence onto a shape it was
/// never true for: a range read wholly above the buffer scans every record,
/// matches none, and got told to check a filter it never passed — beside a
/// `verdict=complete` saying the daemon vouched for the window.
///
/// The buffer's extent is stated instead, because in that case it IS the answer.
fn empty_but_flowing(result: &Value) -> Option<String> {
    let count = result.get("count")?.as_u64()?;
    let scanned = result.get("scanned")?.as_u64()?;
    if count > 0 || scanned == 0 {
        return None;
    }
    let extent = match (
        result.get("buffer_oldest_seq").and_then(|v| v.as_u64()),
        result.get("buffer_newest_seq").and_then(|v| v.as_u64()),
    ) {
        (Some(lo), Some(hi)) => format!(" The stored range is seqs {lo}–{hi}."),
        _ => String::new(),
    };
    Some(format!(
        "\n0 of {scanned} scanned records matched — records ARE flowing, so what \
         was asked for is what to check: the filter, or a seq range outside what \
         is stored.{extent}"
    ))
}

/// The rendered form of `result`, or `None` when this method has no renderer.
///
/// **Never fails and never panics.** A renderer reads the fields it names off a
/// `Value` and yields `None` on any shape it does not recognise: a presentation
/// bug that turned a working call into an error would be strictly worse than the
/// JSON it replaced. Renderers are held to three rules the `Option` cannot
/// enforce — no byte slicing of a string (`&s[..n]` panics mid-codepoint), no
/// bare `usize` subtraction, no `unwrap`/`expect`.
pub fn for_method(method: &str, result: &Value) -> Option<String> {
    if let Some((_, key, cols, empty)) = LISTS.iter().find(|(m, ..)| *m == method) {
        return table::table_read(result, key, cols, empty);
    }
    match method {
        "logs.recent" | "logs.context" | "logs.export" | "traces.logs" => log_read(result),
        "spans.export" | "spans.context" => span_read(result),
        // `traces.slow` answers two shapes from one method: grouped it is a
        // table of names, ungrouped it is a list of spans. The branch is a
        // property of the RESULT, so the renderer keeps it rather than picking
        // one — and a reply carrying neither array is a third case that must not
        // be collapsed into "no slow spans", which would claim the query found
        // none when in fact it returned nothing to look at.
        "traces.slow" => {
            if result.get("groups").is_some() {
                table::table_read(result, "groups", SLOW_GROUP_COLS, "(no groups)")
            } else if result.get("spans").is_some() {
                span_read(result)
            } else {
                None
            }
        }
        "traces.recent" => table::table_read(result, "traces", TRACE_COLS, "(no traces)"),
        "filters.list" => dsl_list(result, "filters", "(no filters)"),
        "triggers.list" => dsl_list(result, "triggers", "(no triggers)"),
        "status.get" => status::render(result),
        // Renderers are wired in per method as they land. Until one claims a
        // method, its reply is JSON — see the module docs.
        _ => None,
    }
}

const TRACE_COLS: &[Col] = &[
    ("trace_id", "trace_id"),
    ("root", "root_span_name"),
    ("service", "service_name"),
    ("ms", "total_duration_ms"),
    ("spans", "span_count"),
    ("logs", "linked_log_count"),
    ("errors", "has_errors"),
    ("started", "start_time"),
];

/// **`max_ms` is not optional here.** The floor decides which names appear, not
/// which spans are counted, so a name qualifies on its slowest span while its
/// average sits far below the floor — and `TracesSlowGroup`'s own doc calls that
/// gap "the useful signal". Rendering count/avg/p95 without it produced a query
/// for "slower than 5000 ms" answering with a row whose every displayed number
/// was under 533, from twenty spans at 10 ms and one at 11 seconds. The reader
/// concludes the tool is broken, or that the name averages 533 ms.
const SLOW_GROUP_COLS: &[Col] = &[
    ("name", "name"),
    ("count", "count"),
    ("max_ms", "max_ms"),
    ("p95_ms", "p95_ms"),
    ("avg_ms", "avg_ms"),
    ("p50_ms", "p50_ms"),
];

/// Filters and triggers: the DSL string **verbatim**, one entry per block.
///
/// These are tables everywhere else in this module, and they are not tables here
/// for one reason: a markdown cell escapes `|`, and the CLI prints the rendering
/// as raw terminal text rather than through a markdown renderer. So a filter
/// configured `m~/id\|name/` displayed as `m~/id\\\|name/`, and the two default
/// triggers displayed `/panic\|unwrap failed\|stack backtrace/` — a regex that
/// matches one literal string instead of three alternatives. Copying the cell
/// back registers something different from what is running.
///
/// The escaping is correct for markdown and the row must not break; the fix is
/// to keep DSL out of a cell. The block form flattens newlines and escapes
/// nothing, so the string a reader copies is the string that is configured.
fn dsl_list(result: &Value, key: &str, empty: &str) -> Option<String> {
    let records = result.get(key)?.as_array()?;
    let lines: Vec<String> = records
        .iter()
        .map(|e| {
            let id = e.get("id").map(blocks::compact).unwrap_or_default();
            let filter = e
                .get("filter")
                .and_then(|v| v.as_str())
                .map(escape::flatten)
                .unwrap_or_default();
            let mut line = format!("[{id}] {filter}");
            let mut extra: Vec<String> = Vec::new();
            for (label, field) in [
                ("matched", "match_count"),
                ("pre", "pre_window"),
                ("post", "post_window"),
                ("notify", "notify_context"),
                ("oneshot", "oneshot"),
                ("post_remaining", "post_remaining"),
            ] {
                if let Some(v) = e.get(field).filter(|v| !v.is_null()) {
                    extra.push(format!("{label}={}", blocks::compact(v)));
                }
            }
            if let Some(d) = e.get("description").and_then(|v| v.as_str()) {
                extra.push(format!("— {}", escape::flatten(d)));
            }
            if !extra.is_empty() {
                line.push_str("\n    ");
                line.push_str(&extra.join("  "));
            }
            line
        })
        .collect();
    let mut out = blocks::blocks(lines, empty);
    out.push_str(&diagnostics(result, key));
    Some(out)
}

/// A span read: the records, then everything the result says about itself.
fn span_read(result: &Value) -> Option<String> {
    let records = result.get("spans")?.as_array()?;
    let lines: Vec<String> = records.iter().map(blocks::span_line).collect();
    let mut out = blocks::blocks(lines, "(no spans)");
    out.push_str(&diagnostics(result, "spans"));
    Some(out)
}

/// A log read: the records, then everything the result says about itself.
///
/// **`None` unless the result really carries a `logs` array.** An earlier
/// version rendered `(no logs)` for any shape at all, so a `null` result — or
/// any reply this renderer did not understand — came back claiming there were no
/// logs. The empty marker means "this read returned nothing", which is a claim,
/// and a renderer may only make it about a result it recognises.
fn log_read(result: &Value) -> Option<String> {
    let records = result.get("logs")?.as_array()?;
    let lines: Vec<String> = records.iter().map(blocks::log_line).collect();
    let mut out = blocks::blocks(lines, "(no logs)");
    if let Some(note) = empty_but_flowing(result) {
        out.push_str(&note);
    }
    out.push_str(&diagnostics(result, "logs"));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fallback is the feature. A method nobody has written a renderer for
    /// must produce no `_display` rather than an empty one, so a client can tell
    /// "not rendered" from "rendered to nothing".
    #[test]
    fn an_unclaimed_method_renders_to_none() {
        assert_eq!(for_method("logs.clear", &json!({"cleared": 3})), None);
        assert_eq!(for_method("no.such.method", &json!({})), None);
    }

    /// A renderer is handed whatever the handler returned, and an empty marker
    /// is a CLAIM — "this read returned nothing" — so it may only be made about
    /// a result the renderer recognises. Rendering `(no logs)` for a shape that
    /// carries no `logs` array at all would state something untrue about a reply
    /// the renderer did not understand.
    #[test]
    fn an_unrecognised_shape_renders_to_none_not_to_an_empty_marker() {
        for v in [json!(null), json!([1, 2, 3]), json!("text"), json!(7)] {
            assert_eq!(for_method("logs.recent", &v), None, "{v}");
        }
        // Missing the array entirely is not "no logs" either.
        assert_eq!(for_method("logs.recent", &json!({"count": 0})), None);
        // But a genuinely empty read IS "no logs".
        let empty = for_method("logs.recent", &json!({"logs": [], "count": 0}))
            .expect("an empty log read still renders");
        assert!(empty.starts_with("(no logs)"), "{empty}");
    }

    /// The structural drop rule: only the record array may be absent from the
    /// rendering. A prose list of field names cannot be checked; this can.
    #[test]
    fn every_key_but_the_records_reaches_the_rendering() {
        let result = json!({
            "logs": [{"seq": 1, "level": "Info", "message": "hi",
                      "timestamp": "2026-08-02T03:29:02Z"}],
            "count": 1,
            "scanned": 40,
            "truncated": false,
            "evicted_before_window": 12,
            "cursor_advanced_to": 99,
        });
        let out = for_method("logs.recent", &result).expect("a log read renders");
        for key in ["count", "scanned", "truncated", "evicted_before_window", "cursor_advanced_to"]
        {
            assert!(out.contains(key), "`{key}` was dropped from:\n{out}");
        }
    }

    /// `traces.slow` answers two shapes from one method, and a third that is
    /// neither. Collapsing the third into the empty-spans marker would claim the
    /// query found no slow spans when in fact the result carried nothing to look
    /// at — a different fact.
    #[test]
    fn traces_slow_keeps_all_three_of_its_branches_apart() {
        let grouped = for_method(
            "traces.slow",
            &json!({"groups": [{"name": "put", "count": 3, "avg_ms": 12.0, "p95_ms": 30.0}]}),
        )
        .expect("grouped renders");
        assert!(grouped.contains("| name "), "a table: {grouped}");
        assert!(grouped.contains("| put "), "{grouped}");

        let listed = for_method(
            "traces.slow",
            &json!({"spans": [{"seq": 1, "name": "put", "service_name": "s",
                               "duration_ms": 11017.1}]}),
        )
        .expect("ungrouped renders");
        assert!(listed.contains("11017ms"), "blocks: {listed}");
        assert!(!listed.contains("| name "), "not a table: {listed}");

        // Neither array: no claim at all.
        assert_eq!(for_method("traces.slow", &json!({"count": 0})), None);
    }

    /// `traces.logs` returns log records, so it renders as one.
    #[test]
    fn a_traces_logs_reply_renders_as_log_records() {
        let out = for_method(
            "traces.logs",
            &json!({"logs": [{"seq": 4, "level": "Error", "message": "boom",
                              "timestamp": "2026-08-02T03:29:02Z"}], "count": 1}),
        )
        .expect("renders");
        assert!(out.contains("[4] 2026-08-02T03:29:02 ERROR boom"), "{out}");
    }

    /// The derived note, which is not a field on any result: `count == 0` with
    /// `scanned > 0` is the difference between "your filter matched nothing" and
    /// "the system is quiet", and an agent told only `(no logs)` concludes the
    /// second.
    #[test]
    fn an_empty_read_over_a_live_buffer_says_the_query_is_what_to_check() {
        let out = for_method("logs.recent", &json!({"logs": [], "count": 0, "scanned": 4000}))
            .expect("renders");
        assert!(out.contains("0 of 4000 scanned records matched"), "{out}");
        assert!(out.contains("records ARE flowing"), "{out}");

        // A genuinely quiet buffer must NOT get the note — it would be false.
        let quiet = for_method("logs.recent", &json!({"logs": [], "count": 0, "scanned": 0}))
            .expect("renders");
        assert!(!quiet.contains("ARE flowing"), "{quiet}");

        // **And a read that MATCHED must not get it either.** Both fixtures
        // above set `count: 0`, so dropping the `count == 0` term from the
        // predicate left them green — and the mutant printed "0 of 4000
        // matched" directly above the records that had matched. A flat lie to
        // whoever reads it.
        let matched = for_method(
            "logs.recent",
            &json!({"logs": [{"seq": 1, "level": "Info", "message": "hi",
                              "timestamp": "2026-08-02T03:29:02Z"}],
                    "count": 1, "scanned": 4000}),
        )
        .expect("renders");
        assert!(
            !matched.contains("scanned records matched"),
            "the note fired on a read that returned records: {matched}"
        );

        // The buffer's extent is named, because when the cause is a seq range
        // outside it, that IS the answer — and the note must not blame a filter
        // the caller never passed.
        let ranged = for_method(
            "logs.export",
            &json!({"logs": [], "count": 0, "scanned": 210,
                    "buffer_oldest_seq": 1, "buffer_newest_seq": 210}),
        )
        .expect("renders");
        assert!(ranged.contains("stored range is seqs 1–210"), "{ranged}");
        assert!(
            !ranged.contains("the filter is what to check"),
            "an export with no filter was told to check its filter: {ranged}"
        );
    }

    /// The record array is the ONE key a rendering may omit — and the reason is
    /// that the reader is getting it rendered instead. Re-emitting it in the
    /// diagnostics tail doubles the reply and inverts the entire point.
    ///
    /// `every_key_but_the_records_reaches_the_rendering` is named for this rule
    /// and only checks the positive half, so turning the skip off left it green.
    #[test]
    fn the_record_array_is_not_re_emitted_in_the_diagnostics() {
        let out = for_method(
            "logs.recent",
            &json!({"logs": [{"seq": 1, "level": "Info", "message": "unique-marker",
                              "timestamp": "2026-08-02T03:29:02Z"}], "count": 1}),
        )
        .expect("renders");
        assert_eq!(
            out.matches("unique-marker").count(),
            1,
            "the records were rendered AND dumped as JSON:\n{out}"
        );
        assert!(!out.contains("\"message\""), "raw record JSON in the tail:\n{out}");
    }

    /// Spans render, refuse an unrecognised shape, and carry their diagnostics —
    /// none of which had a single unit test. Every CLI span test passes
    /// `--json`, so the rendered path was never reached at any level.
    #[test]
    fn a_span_read_renders_its_records_and_its_diagnostics() {
        let out = for_method(
            "spans.export",
            &json!({"spans": [{"seq": 9, "name": "put", "service_name": "svc",
                               "duration_ms": 11017.13, "trace_id": "7f3a",
                               "status": {"type": "error"},
                               "attributes": {"b": 2, "a": 1}}],
                    "count": 1, "scanned": 40}),
        )
        .expect("renders");
        assert!(out.contains("svc  put"), "{out}");
        assert!(out.contains("11017ms"), "{out}");
        assert!(out.contains("status=error"), "an errored span must say so: {out}");
        assert!(out.contains("trace_id=7f3a"), "correlation is lost: {out}");
        assert!(out.contains("a=1  b=2"), "attributes sorted: {out}");
        assert!(out.contains("scanned=40"), "diagnostics dropped: {out}");

        for shape in [json!(null), json!({"count": 0}), json!("x")] {
            assert_eq!(
                for_method("spans.export", &shape),
                None,
                "an unrecognised shape was claimed as an empty span read: {shape}"
            );
        }
        assert_eq!(for_method("spans.context", &json!({"spans": []})).as_deref(),
                   Some("(no spans)"));
    }

    /// `traces.recent` had no test at all — its whole CLI surface uses `--json`.
    #[test]
    fn a_traces_recent_reply_renders_as_a_table() {
        let out = for_method(
            "traces.recent",
            &json!({"traces": [{"trace_id": "7c57", "root_span_name": "distribute",
                                "service_name": "store", "total_duration_ms": 0.014,
                                "span_count": 1, "linked_log_count": 0,
                                "has_errors": false, "start_time": "2026-08-02T08:05:18Z"}],
                    "count": 1}),
        )
        .expect("renders");
        assert!(out.contains("| trace_id |"), "{out}");
        assert!(out.contains("| 7c57 "), "{out}");
        assert!(out.contains("| distribute "), "{out}");
    }

    /// The grouped `traces.slow` table must carry `max_ms` — the field the
    /// display floor is evaluated against. Without it a query for "slower than
    /// 5000ms" answers with a row whose every displayed number is far below it.
    #[test]
    fn a_grouped_slow_row_shows_the_number_the_floor_tested() {
        let out = for_method(
            "traces.slow",
            &json!({"groups": [{"name": "handler", "count": 21, "avg_ms": 533.3,
                                "p50_ms": 10.0, "p95_ms": 10.0, "max_ms": 11000.0}],
                    "display_floor_ms": 5000.0}),
        )
        .expect("renders");
        assert!(out.contains("| max_ms "), "{out}");
        assert!(out.contains("11000"), "the one number explaining the row: {out}");
        assert!(out.contains("display_floor_ms=5000"), "{out}");
    }
}
