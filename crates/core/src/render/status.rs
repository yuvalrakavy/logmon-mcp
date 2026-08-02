//! `status.get` — the single biggest saving in the feature, and the clearest
//! case of the coverage rule.
//!
//! Measured on one live broker: 1,967 bytes of JSON becomes about 300 — roughly
//! 6×, though the ratio is broker-dependent (a quieter instance measured 3.9×).
//! Most of the difference is `broker_tools`, the full list of tool names —
//! **which an MCP client already holds**, because it registered them. Sending it
//! back is noise to the primary client and unreadable to the secondary one.
//!
//! It is still on the result for a caller who wants it. Rendering drops it from
//! the *rendering*, never from the reply.

use serde_json::Value;

fn u(v: &Value, key: &str) -> Option<u64> {
    v.get(key)?.as_u64()
}

/// A duration a human reads without arithmetic. Seconds are exact below a
/// minute and useless above an hour.
fn age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600),
    }
}

/// Only the counters that are non-zero, because a wall of zeros is what the
/// reader has to scan past to find the one that is not.
fn nonzero(v: &Value, label: &str) -> Option<String> {
    let obj = v.as_object()?;
    let hits: Vec<String> = {
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|k| {
                let n = obj.get(k)?.as_u64()?;
                (n > 0).then(|| format!("{k}={n}"))
            })
            .collect()
    };
    (!hits.is_empty()).then(|| format!("{label}: {}", hits.join("  ")))
}

/// `None` unless the result really looks like a `status.get` reply.
pub fn render(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    // `broker_version` is the one field every status reply has had since the
    // method existed; without it this is not a status reply and the renderer
    // must not narrate one.
    let version = obj.get("broker_version")?.as_str()?;

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "broker {version}, up {}",
        u(result, "daemon_uptime_secs").map_or("?".into(), age)
    ));

    if let Some(d) = obj.get("current_domain").and_then(|v| v.as_str()) {
        lines.push(format!("domain: {d}"));
    }
    if let Some(rs) = obj.get("receivers").and_then(|v| v.as_array()) {
        let names: Vec<String> = rs.iter().map(super::blocks::compact).collect();
        // Named even when empty. A broker with nothing bound accepts no
        // telemetry at all, and `receivers:` followed by nothing reads as a
        // rendering bug rather than as the finding it is.
        lines.push(format!(
            "receivers: {}",
            if names.is_empty() {
                "(none bound — this broker is not accepting telemetry)".to_string()
            } else {
                names.join("  ")
            }
        ));
    }
    if let Some(store) = obj.get("store").and_then(|v| v.as_object()) {
        let mut keys: Vec<&String> = store.keys().collect();
        keys.sort();
        let bits: Vec<String> = keys
            .into_iter()
            .filter_map(|k| store.get(k).map(|v| format!("{k}={}", super::blocks::compact(v))))
            .collect();
        lines.push(format!("store: {}", bits.join("  ")));
    }
    if let Some(s) = obj.get("session").and_then(|v| v.as_object()) {
        let name = s
            .get("name")
            .and_then(|v| v.as_str())
            .or_else(|| s.get("id").and_then(|v| v.as_str()))
            .unwrap_or("?");
        lines.push(format!(
            "session {name}: connected={}  triggers={}  filters={}  queued={}",
            s.get("connected").map_or("?".into(), super::blocks::compact),
            s.get("trigger_count").map_or("?".into(), super::blocks::compact),
            s.get("filter_count").map_or("?".into(), super::blocks::compact),
            s.get("queue_size").map_or("?".into(), super::blocks::compact),
        ));
    }
    if let Some(f) = obj.get("active_filters").and_then(|v| v.as_array()) {
        // Stated even when empty: "no filter is narrowing this store" is the
        // fact a reader most needs before trusting a quiet buffer, and an
        // omitted line reads as "not checked".
        let listed: Vec<String> = f.iter().map(super::blocks::compact).collect();
        lines.push(format!(
            "active filters: {}",
            if listed.is_empty() {
                "(none — nothing is narrowing this store)".to_string()
            } else {
                listed.join("  ")
            }
        ));
    }

    // Loss counters, only when something was actually lost.
    for (key, label) in [
        ("receiver_drops", "receiver drops"),
        ("trace_ingest", "trace ingest"),
    ] {
        if let Some(line) = obj.get(key).and_then(|v| nonzero(v, label)) {
            lines.push(line);
        }
    }

    // When each receiver last saw traffic — the answer to "is my app actually
    // sending?", which is the first question behind an empty buffer. Rendered
    // as the raw instants: a relative age would need a clock, and a renderer
    // that reads one is a renderer whose output cannot be pinned by a test.
    if let Some(live) = obj.get("receiver_liveness").and_then(|v| v.as_object()) {
        let mut keys: Vec<&String> = live.keys().collect();
        keys.sort();
        let bits: Vec<String> = keys
            .into_iter()
            .filter_map(|k| live.get(k).map(|v| format!("{k}={}", super::blocks::compact(v))))
            .collect();
        // **Stated when empty, which is the case that matters.** Every field of
        // `ReceiverLiveness` is `Option` and skipped when `None`, so a broker
        // that has never received anything serializes `{}` — and a guard on
        // `bits.is_empty()` made the line appear when it was redundant and
        // vanish when it was the finding. Seven lines advertising four bound
        // listeners, and nothing saying not one had seen a byte.
        lines.push(format!(
            "last traffic: {}",
            if bits.is_empty() {
                "(none — no listener has received anything)".to_string()
            } else {
                bits.join("  ")
            }
        ));
    }

    // `broker_tools` is deliberately reduced to its count: the MCP client that
    // asked already registered every one of them.
    if let Some(n) = obj.get("broker_tools").and_then(|v| v.as_array()).map(Vec::len) {
        lines.push(format!("tools: {n} served"));
    }

    // Anything this renderer does not know about. `status.get` has no record
    // array, so the structural drop rule admits NO omission: a key added to the
    // result later must show up here rather than vanish from a rendering that
    // reads as complete.
    const KNOWN: &[&str] = &[
        "broker_version",
        "daemon_uptime_secs",
        "current_domain",
        "receivers",
        "store",
        "session",
        "active_filters",
        "receiver_drops",
        "trace_ingest",
        "receiver_liveness",
        "broker_tools",
    ];
    let mut unknown: Vec<String> = obj
        .keys()
        .filter(|k| !KNOWN.contains(&k.as_str()) && !k.starts_with('_'))
        .map(|k| format!("{k}={}", super::blocks::compact(&obj[k])))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        lines.push(unknown.join("  "));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply() -> Value {
        json!({
            "broker_version": "0.9.0",
            "daemon_uptime_secs": 53785,
            "current_domain": "default",
            "receivers": ["UDP:12201", "gRPC:4317"],
            "store": {"total_received": 306, "current_size": 306},
            "session": {"id": "cli", "name": "cli", "connected": true,
                        "trigger_count": 2, "filter_count": 0, "queue_size": 0},
            "active_filters": [],
            "receiver_drops": {"gelf_udp": 0, "gelf_tcp": 0},
            "trace_ingest": {"dropped": 0, "shed_batches": 0},
            "broker_tools": ["a", "b", "c"],
        })
    }

    /// The rule this method exists to demonstrate: the tool list is noise to the
    /// client that already registered it.
    #[test]
    fn the_tool_names_are_counted_not_listed() {
        let out = render(&reply()).expect("renders");
        assert!(out.contains("tools: 3 served"), "{out}");
        for name in ["\"a\"", "\"b\"", "\"c\""] {
            assert!(!out.contains(name), "a tool name survived: {out}");
        }
    }

    /// A wall of zeros is what a reader scans past to find the one that is not.
    #[test]
    fn loss_counters_appear_only_when_something_was_lost() {
        let quiet = render(&reply()).expect("renders");
        assert!(!quiet.contains("receiver drops"), "{quiet}");

        let mut lossy = reply();
        lossy["receiver_drops"]["gelf_udp"] = json!(17);
        let out = render(&lossy).expect("renders");
        assert!(out.contains("receiver drops: gelf_udp=17"), "{out}");
        assert!(
            !out.contains("gelf_tcp"),
            "a zero counter rode along with the one that mattered: {out}"
        );
    }

    /// "Nothing is narrowing this store" is the fact a reader needs before
    /// trusting a quiet buffer, so an empty list is STATED rather than omitted.
    #[test]
    fn an_empty_filter_list_is_stated_rather_than_left_out() {
        let out = render(&reply()).expect("renders");
        assert!(out.contains("nothing is narrowing this store"), "{out}");
    }

    #[test]
    fn uptime_is_readable_without_arithmetic() {
        assert_eq!(age(45), "45s");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(53785), "14h56m");
        assert_eq!(age(200_000), "2d07h");
    }

    #[test]
    fn a_shape_that_is_not_a_status_reply_renders_to_none() {
        assert_eq!(render(&json!(null)), None);
        assert_eq!(render(&json!({"uptime": 3})), None);
    }

    /// The structural drop rule, on a result with no record array: **nothing may
    /// be silently omitted.** `status.get` is where this bites hardest, because
    /// its renderer is hand-written per field rather than generic, so a key
    /// added later would quietly vanish from a rendering that reads as complete.
    #[test]
    fn a_key_this_renderer_does_not_know_still_reaches_the_reader() {
        let mut r = reply();
        r["some_future_counter"] = json!(42);
        let out = render(&r).expect("renders");
        assert!(
            out.contains("some_future_counter=42"),
            "an unknown key vanished from a rendering that reads as complete:\n{out}"
        );
    }

    /// The first question behind an empty buffer is whether anything is being
    /// sent at all — **including when the answer is "nothing, ever".**
    ///
    /// Every field of `ReceiverLiveness` is skipped when `None`, so a broker
    /// that has received nothing serializes `{}`. A guard on `bits.is_empty()`
    /// made this line appear when it was redundant and vanish when it was the
    /// finding: seven lines advertising four bound listeners, silent on the fact
    /// that not one had seen a byte.
    #[test]
    fn receiver_liveness_reaches_the_reader_especially_when_it_is_empty() {
        let mut r = reply();
        r["receiver_liveness"] = json!({"gelf_udp": "2026-08-02T06:36:55Z"});
        let out = render(&r).expect("renders");
        assert!(out.contains("last traffic: gelf_udp=2026-08-02T06:36:55Z"), "{out}");

        let mut fresh = reply();
        fresh["receiver_liveness"] = json!({});
        let out = render(&fresh).expect("renders");
        assert!(
            out.contains("no listener has received anything"),
            "a broker that has never received anything said nothing about it:\n{out}"
        );
    }

    /// The lines nothing pinned. Deleting the session, store or domain line
    /// outright left the whole suite green.
    #[test]
    fn every_narrated_line_is_present() {
        let out = render(&reply()).expect("renders");
        for expected in [
            "broker 0.9.0, up ",
            "domain: default",
            "receivers: UDP:12201",
            "store: ",
            "session cli: connected=true",
            "active filters:",
            "tools: ",
        ] {
            assert!(out.contains(expected), "missing {expected:?}:\n{out}");
        }
        // A fixed line count, so a line cannot be dropped without a failure and
        // the join separator cannot silently change.
        assert_eq!(out.lines().count(), 7, "line set changed:\n{out}");
    }

    /// The session is named by `name`, falling back to `id`. The old fixture set
    /// both to `"cli"`, so hard-wiring the label to either one — or to a
    /// constant — was invisible.
    #[test]
    fn the_session_is_named_by_name_and_falls_back_to_id() {
        let mut r = reply();
        r["session"]["id"] = json!("s-1");
        r["session"]["name"] = json!("friendly");
        assert!(render(&r).expect("renders").contains("session friendly:"));

        let mut only_id = reply();
        only_id["session"] = json!({"id": "s-2", "connected": false});
        assert!(render(&only_id).expect("renders").contains("session s-2:"));
    }

    /// Two counters, out of alphabetical order, so the sort is doing work the
    /// fixture can see. The old one set exactly one counter non-zero.
    #[test]
    fn loss_counters_render_in_a_stable_order() {
        let mut r = reply();
        r["receiver_drops"] = json!({"z_udp": 3, "a_tcp": 9, "m_http": 0});
        let out = render(&r).expect("renders");
        assert!(out.contains("receiver drops: a_tcp=9  z_udp=3"), "{out}");
        assert!(!out.contains("m_http"), "a zero counter rode along: {out}");
    }

    /// Same, for the unknown-key backstop.
    #[test]
    fn unknown_keys_render_in_a_stable_order() {
        let mut r = reply();
        r["zebra_metric"] = json!(1);
        r["alpha_metric"] = json!(2);
        let out = render(&r).expect("renders");
        assert!(out.contains("alpha_metric=2  zebra_metric=1"), "{out}");
    }
}
