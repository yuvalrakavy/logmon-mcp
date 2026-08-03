//! `logs.profile` over the wire — the contract, not the projection.
//!
//! The projection's arithmetic is unit-tested next to it (`logs::profile`).
//! What only a live daemon can show is the reply SHAPE, the parameter
//! rejections, and that the walk covers the whole ring rather than the newest
//! slice — the handler being where a `count`-limited primitive would have been
//! reached for.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::{Level, LogEntry};
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};

/// Injection is asynchronous, so a read taken immediately can see a partial
/// buffer. Poll until the expected count lands rather than sleeping a guessed
/// interval — a SHORT buffer is exactly what a whole-ring test must not
/// silently accept.
async fn settle(client: &mut TestClient, want: usize) {
    for _ in 0..80 {
        let r: Value = client.call("logs.profile", json!({})).await.unwrap();
        if r["matched"].as_u64().unwrap_or(0) >= want as u64 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("only saw fewer than {want} records after 2s; the buffer never filled")
}

fn with_field(msg: &str, name: &str, value: Value) -> LogEntry {
    let mut e = LogEntry::synthetic(Level::Info, msg);
    e.additional_fields.insert(name.to_string(), value);
    e
}

/// Assert an error message SAYS the thing. A rejection a caller cannot act on
/// is barely better than a wrong answer, and every rejection this method adds
/// exists to redirect rather than to refuse.
fn says(e: &anyhow::Error, needle: &str) {
    let text = e.to_string();
    assert!(
        text.contains(needle),
        "the rejection must mention `{needle}`, or the caller cannot act on it: {text}"
    );
}

fn row<'a>(reply: &'a Value, key: &str) -> Option<&'a Value> {
    reply["groups"]
        .as_array()?
        .iter()
        .find(|g| g["key"].as_str() == Some(key))
}

/// The whole ring, never a `count`-limited prefix.
///
/// Deliberately more records than any plausible default. A handler that reached
/// for `recent_logs_with_stats` would profile the tail of this and look
/// entirely correct.
#[tokio::test]
async fn profile_covers_the_whole_buffer_not_the_newest_slice() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    const N: usize = 400;
    for i in 0..N {
        daemon
            .inject_entry(with_field(&format!("m{i}"), "kind", json!(i % 4)))
            .await;
    }
    settle(&mut client, N).await;

    let reply: Value = client
        .call("logs.profile", json!({ "group_by": "field", "group_keys": ["kind"] }))
        .await
        .unwrap();

    assert_eq!(reply["matched"].as_u64(), Some(N as u64));
    assert_eq!(
        reply["scanned"].as_u64(),
        Some(N as u64),
        "scanned must cover the whole ring: {reply}"
    );
    let summed: u64 = reply["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["count"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(summed, N as u64, "rows must account for every record: {reply}");
    assert_eq!(reply["groups_total"].as_u64(), Some(4));
}

/// The reply carries what a reader needs to know the population was complete,
/// plus both ends of every row as whole records.
#[tokio::test]
async fn the_reply_carries_its_evidence_and_both_ends_of_each_row() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_entry(with_field("first one", "k", json!("a"))).await;
    daemon.inject_entry(with_field("second one", "k", json!("a"))).await;
    settle(&mut client, 2).await;

    let reply: Value = client
        .call("logs.profile", json!({ "group_by": "field", "group_keys": ["k"] }))
        .await
        .unwrap();

    // The whole-population bounds are in this list because a mutation campaign
    // showed they were in NO list: zeroing them left every test green, since
    // the row assertions below read a row's ends, not the population's.
    for key in [
        "matched", "scanned", "buffer_total", "lost_below", "truncated", "levels",
        "groups_total", "cardinality_capped", "grouped_by",
        "first_seq", "last_seq", "first_time", "last_time",
    ] {
        assert!(reply.get(key).is_some(), "`{key}` missing from the reply: {reply}");
        assert!(!reply[key].is_null(), "`{key}` is null on a non-empty population: {reply}");
    }
    assert_eq!(reply["grouped_by"].as_str(), Some("field"));
    assert_eq!(reply["group_keys"], json!(["k"]));
    assert_eq!(reply["levels"]["info"].as_u64(), Some(2));

    // `LogsProfileResult` is schema-only: the handler hand-builds the envelope
    // and nothing constructs the struct, so `verify-schema` — which re-runs the
    // definitions map against itself — cannot see the two drift apart. This
    // repo has already named that hazard for `ToolsManifestResult`. One
    // round-trip closes it: a field renamed on either side stops deserializing.
    let typed: logmon_broker_protocol::methods::LogsProfileResult =
        serde_json::from_value(reply.clone())
            .expect("the wire reply must deserialize into the struct that documents it");
    assert_eq!(typed.matched, 2);
    assert_eq!(typed.levels.info, 2);
    assert_eq!(typed.groups.len(), reply["groups"].as_array().map_or(0, Vec::len));
    // **The `#[serde(default)]` fields are asserted, not just deserialized.**
    // Most of this struct defaults, so a rename on either side yields `None`
    // and a round-trip alone stays green — the check would claim more than it
    // tests. These are the ones that would silently become nothing.
    assert!(typed.grouped_by.is_some(), "{reply}");
    assert!(typed.group_keys.is_some(), "{reply}");
    assert!(typed.groups_total.is_some(), "{reply}");
    assert!(typed.first_seq.is_some() && typed.last_seq.is_some(), "{reply}");
    assert!(typed.first_time.is_some() && typed.last_time.is_some(), "{reply}");
    assert!(
        typed.groups.iter().all(|g| g.reserved.is_none()),
        "no reserved bucket in this fixture, and `reserved` must not appear \
         from nowhere: {reply}"
    );

    let a = row(&reply, "a").expect("the group");
    assert_eq!(a["first_exemplar"].as_str(), Some("first one"));
    assert_eq!(a["last_exemplar"].as_str(), Some("second one"));
    assert!(
        a["first_seq"].as_u64() < a["last_seq"].as_u64(),
        "each end is a whole record: {a}"
    );
    assert!(a["first_time"].is_string() && a["last_time"].is_string(), "{a}");
}

/// An ungrouped call describes the population and returns no rows — and
/// `groups_total` is `null`, never `0`.
///
/// `0` would read as "this query touched nothing" rather than "we did not
/// look", which is the distinction the span side's own field doc draws.
#[tokio::test]
async fn an_ungrouped_call_reports_null_rather_than_zero_groups() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Warn, "one").await;
    settle(&mut client, 1).await;

    let reply: Value = client.call("logs.profile", json!({})).await.unwrap();
    assert_eq!(reply["matched"].as_u64(), Some(1));
    assert_eq!(reply["levels"]["warn"].as_u64(), Some(1));
    assert!(
        reply["groups"].as_array().is_none_or(|g| g.is_empty()),
        "no rows without an axis: {reply}"
    );
    assert!(reply["groups_total"].is_null(), "null, not 0: {reply}");
    assert!(reply["grouped_by"].is_null(), "{reply}");
}

/// The three `group_keys` states the parameter has to keep apart, and the one
/// the design gate caught being conflated.
#[tokio::test]
async fn group_keys_is_validated_against_the_axis_without_refusing_an_empty_array() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_entry(with_field("m", "kind", json!("x"))).await;
    settle(&mut client, 1).await;

    // `field` with nothing to group along cannot be answered.
    let e = client
        .call::<Value>("logs.profile", json!({ "group_by": "field" }))
        .await
        .expect_err("field needs keys");
    says(&e, "group_keys");

    // Non-empty keys on a built-in axis: the caller believes they are grouping
    // by fields, so answering a different question silently is worse than one
    // refused round trip. This diverges from `traces.profile` deliberately.
    let e = client
        .call::<Value>(
            "logs.profile",
            json!({ "group_by": "level", "group_keys": ["kind"] }),
        )
        .await
        .expect_err("keys on a built-in axis are refused");
    says(&e, "group_keys");

    // More than MAX_KEYS members. A mutation campaign found this cap had NO
    // probe at all — and the reason it looked covered is worth recording: a
    // project-wide grep for "too many group_keys" hits `collectors_rpc.rs`,
    // which exercises the collector method's own, differently-wired cap. A
    // check tested somewhere else is not a check tested here.
    let e = client
        .call::<Value>(
            "logs.profile",
            json!({
                "group_by": "field",
                "group_keys": ["a", "b", "c", "d", "e", "f", "g", "h", "i"],
            }),
        )
        .await
        .expect_err("nine keys exceed the cap");
    says(&e, "too many group_keys");

    // And eight is accepted, or the check above would be satisfied by a cap of
    // any size — including one that refuses the two-key tuples the feature
    // exists for.
    let reply: Value = client
        .call(
            "logs.profile",
            json!({
                "group_by": "field",
                "group_keys": ["a", "b", "c", "d", "e", "f", "g", "h"],
            }),
        )
        .await
        .expect("eight keys are within the cap");
    assert_eq!(reply["matched"].as_u64(), Some(1), "{reply}");

    // **An explicit empty array is NOT "present".** A generated client that
    // always serialises `group_keys: []` would otherwise have its most ordinary
    // call refused. Elsewhere the two differ — on `collectors.edit` an empty
    // list is a structural clear — but nothing here persists for it to clear.
    for axis in ["none", "level"] {
        let reply: Value = client
            .call(
                "logs.profile",
                json!({ "group_by": axis, "group_keys": [] }),
            )
            .await
            .unwrap_or_else(|e| panic!("`group_keys: []` must be accepted on `{axis}`: {e}"));
        assert!(
            reply["group_keys"].is_null(),
            "and it echoes as null, matching `nothing was asked for`: {reply}"
        );
    }
}

/// An axis no matched record carries is announced, not answered with a lone
/// `__absent__` row that reads like a real result.
#[tokio::test]
async fn a_wholly_absent_axis_is_announced_over_the_wire() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_entry(with_field("m", "kind", json!("x"))).await;
    settle(&mut client, 1).await;

    let reply: Value = client
        .call(
            "logs.profile",
            json!({ "group_by": "field", "group_keys": ["targett"] }),
        )
        .await
        .unwrap();

    // Found by its MARKER, and the marker's wire spelling is asserted here —
    // the renderer derives it from the enum, but nothing else pins what that
    // enum serialises to. Dropping `rename_all` from `ReservedBucket` left the
    // whole suite green while the absent share vanished from every reply.
    let bucket = reply["groups"]
        .as_array()
        .and_then(|gs| gs.iter().find(|g| g["reserved"] == "absent"))
        .unwrap_or_else(|| panic!("no row marked `absent`: {reply}"));
    assert_eq!(bucket["key"].as_str(), Some("__absent__"), "{reply}");
    let s = reply["suppressed"].as_array().expect("the honesty channel");
    assert_eq!(s.len(), 1, "one entry for the one absent member: {reply}");
    assert!(
        s[0]["reason"].as_str().unwrap_or("").contains("targett"),
        "and it names the axis: {s:?}"
    );
}

/// The built-in/additional collision, end to end — the case `logs.fields` was
/// built to expose, met through the other tool.
#[tokio::test]
async fn an_absent_builtin_axis_points_at_the_additional_field_that_works() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    // `_line` arrives as an ADDITIONAL field named `line`; the built-in stays
    // empty, exactly as a real emitter produces it.
    daemon.inject_entry(with_field("m", "line", json!(42))).await;
    settle(&mut client, 1).await;

    let reply: Value = client
        .call("logs.profile", json!({ "group_by": "line" }))
        .await
        .unwrap();

    let s = reply["suppressed"].as_array().expect("announced");
    assert_eq!(s.len(), 1, "{reply}");
    let remedy = s[0]["remedy"].as_str().unwrap_or_default();
    assert!(
        remedy.contains("group_keys=[\"line\"]"),
        "the remedy must be the call that works, not a shrug: {s:?}"
    );
}

/// A cursor advances on read, so a second identical profile would describe a
/// different population.
#[tokio::test]
async fn a_cursor_qualifier_is_refused() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    let e = client
        .call::<Value>("logs.profile", json!({ "filter": "c>=somewhere" }))
        .await
        .expect_err("refused");
    says(&e, "cursor");
}

/// And the refusal leaves nothing behind.
///
/// `resolve_bookmarks` -> `cursor_read_and_advance` AUTO-CREATES a bookmark at
/// seq 0 for an UNKNOWN cursor name, as a side effect of resolution. A refusal
/// issued after resolving is honest about not advancing and silently wrong
/// about not writing. An unknown name is the whole fixture — a known one takes
/// the read-existing branch and never inserts.
#[tokio::test]
async fn a_refused_cursor_creates_no_bookmark() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    let before: Value = client.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(before["count"].as_u64(), Some(0), "clean slate");

    client
        .call::<Value>("logs.profile", json!({ "filter": "c>=never_seen_before" }))
        .await
        .expect_err("refused");

    let after: Value = client.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(
        after["count"].as_u64(),
        Some(0),
        "a refused call must not mutate the bookmark store: {after}"
    );
}

/// An empty filter means "no filter", not a parse error.
///
/// The regression a previous gate caught on `logs.fields`: bypassing
/// `parse_and_resolve_filter` to reject the cursor early also bypasses that
/// half of its contract, and every sibling still routed through it kept the old
/// behaviour — so one method diverged silently.
#[tokio::test]
async fn an_empty_filter_means_everything() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    for filter in ["", "   "] {
        let reply: Value = client
            .call("logs.profile", json!({ "filter": filter }))
            .await
            .unwrap_or_else(|e| panic!("`{filter:?}` must mean no filter, not an error: {e}"));
        assert_eq!(reply["matched"].as_u64(), Some(1), "{reply}");
    }
}

/// An unknown axis is refused with the list, rather than silently answering the
/// ungrouped figures.
#[tokio::test]
async fn an_unknown_axis_is_refused_and_the_error_lists_the_real_ones() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    // `group` is the span side's spelling for the open-set arm. An agent that
    // knows `traces.profile` will try it, and a lenient parser would hand back
    // the ungrouped figures as though the question had been answered.
    let e = client
        .call::<Value>("logs.profile", json!({ "group_by": "group" }))
        .await
        .expect_err("refused");
    says(&e, "field");
}
