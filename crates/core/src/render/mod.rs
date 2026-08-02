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
    (
        "filters.list",
        "filters",
        &[
            ("id", "id"),
            ("filter", "filter"),
            ("description", "description"),
        ],
        "(no filters)",
    ),
    (
        "triggers.list",
        "triggers",
        &[
            ("id", "id"),
            ("filter", "filter"),
            ("pre", "pre_window"),
            ("post", "post_window"),
            ("notify", "notify_context"),
            ("matched", "match_count"),
            ("oneshot", "oneshot"),
            ("description", "description"),
        ],
        "(no triggers)",
    ),
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
/// `empty && scanned > 0` is not a field on any result — it is the B2 heuristic
/// that tells "nothing matched" from "nothing is happening", and it has to be
/// ported as logic or it is lost.
fn empty_but_flowing(result: &Value) -> Option<String> {
    let count = result.get("count")?.as_u64()?;
    let scanned = result.get("scanned")?.as_u64()?;
    (count == 0 && scanned > 0).then(|| {
        format!(
            "\nthe filter matched 0 of {scanned} scanned records — data is flowing, \
             so the filter is what to check"
        )
    })
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
        "logs.recent" | "logs.context" | "logs.export" => log_read(result),
        "status.get" => status::render(result),
        // Renderers are wired in per method as they land. Until one claims a
        // method, its reply is JSON — see the module docs.
        _ => None,
    }
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

    /// The derived note, which is not a field on any result: `count == 0` with
    /// `scanned > 0` is the difference between "your filter matched nothing" and
    /// "the system is quiet", and an agent told only `(no logs)` concludes the
    /// second.
    #[test]
    fn an_empty_read_over_a_live_buffer_says_the_filter_is_what_to_check() {
        let out = for_method("logs.recent", &json!({"logs": [], "count": 0, "scanned": 4000}))
            .expect("renders");
        assert!(out.contains("matched 0 of 4000"), "{out}");
        assert!(out.contains("data is flowing"), "{out}");

        // A genuinely quiet buffer must NOT get the note — it would be false.
        let quiet = for_method("logs.recent", &json!({"logs": [], "count": 0, "scanned": 0}))
            .expect("renders");
        assert!(!quiet.contains("data is flowing"), "{quiet}");
    }
}
