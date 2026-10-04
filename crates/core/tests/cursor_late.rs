//! A cursor gets the records a trigger stores LATE (gh #23).
//!
//! Every test arranges the same shape: a session filter stores only `marker` records, so a WARN
//! is kept out of the buffer; the cursor reads the marker, which puts its position ABOVE the
//! WARN; then an ERROR fires the default `l>=ERROR` trigger, whose pre-window flushes the WARN
//! into the buffer below the cursor. A cursor that is only a seq never returns it.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::{Level, LogEntry};
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};
use std::time::Duration;

const TRACE: u128 = 0x5ca1ab1e;

fn traced(level: Level, msg: &str) -> LogEntry {
    let mut e = LogEntry::synthetic(level, msg);
    e.trace_id = Some(TRACE);
    e
}

fn messages(r: &Value) -> Vec<String> {
    r["logs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["message"].as_str().unwrap().to_string())
        .collect()
}

/// A session that stores only `marker` records (plus whatever its default triggers store).
async fn marker_only_session(daemon: &TestDaemonHandle) -> TestClient {
    let mut c = daemon.connect_named("reader", None).await;
    let _: Value = c
        .call("filters.add", json!({ "filter": "m=marker" }))
        .await
        .unwrap();
    c
}

/// Wait until a record with `msg` is stored.
async fn wait_stored(c: &mut TestClient, msg: &str) {
    for _ in 0..500 {
        let r: Value = c
            .call("logs.recent", json!({ "filter": format!("m={msg}") }))
            .await
            .unwrap();
        if r["count"].as_u64().unwrap() > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{msg} was never stored");
}

/// V1: `logs.recent` with a cursor returns the WARN stored late, counted in `cursor_late`, on
/// the read after the flush — and never again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_gets_a_record_stored_below_its_position() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    // The cursor is created by a read that finds nothing — the harness's lane shape — so the
    // late mark it is created with is the one every later read builds on.
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new());
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Info, "marker").await;
    wait_stored(&mut c, "marker").await;

    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["marker"]);
    assert!(r.get("cursor_late").is_none(), "no late records yet: {r}");

    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;

    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["early-warn", "boom"]);
    assert_eq!(r["cursor_late"], json!(1), "{r}");
    // Through the typed result too: the field is part of the protocol, not only of the JSON.
    let typed: logmon_broker_protocol::LogsRecentResult = serde_json::from_value(r).unwrap();
    assert_eq!(typed.cursor_late, Some(1));

    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new());
}

/// V2: the same through `traces.logs` — the path the Store test harness drains each lane by.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trace_cursor_gets_its_trace_s_record_stored_below_its_position() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    let trace = format!("{TRACE:032x}");
    daemon.inject_entry(traced(Level::Warn, "early-warn")).await;
    daemon.inject_entry(traced(Level::Info, "marker")).await;
    wait_stored(&mut c, "marker").await;

    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["marker"]);

    daemon.inject_entry(traced(Level::Error, "boom")).await;
    wait_stored(&mut c, "boom").await;

    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["early-warn", "boom"]);
    assert_eq!(r["cursor_late"], json!(1), "{r}");
    assert!(
        r.get("cursor_late_lost").is_none(),
        "a trace read does not count loss: {r}"
    );
    let typed: logmon_broker_protocol::TracesLogsResult = serde_json::from_value(r).unwrap();
    assert_eq!(typed.cursor_late, Some(1));

    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new());
}

/// A trace cursor with another qualifier applies it to BOTH parts: the late INFO and the INFO
/// stored after the ERROR are passed by, the late WARN and the ERROR come back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trace_cursor_filters_its_late_and_its_normal_records_alike() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    let trace = format!("{TRACE:032x}");
    daemon.inject_entry(traced(Level::Info, "early-info")).await;
    daemon.inject_entry(traced(Level::Warn, "early-warn")).await;
    daemon.inject_entry(traced(Level::Info, "marker")).await;
    wait_stored(&mut c, "marker").await;
    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["marker"]);

    daemon.inject_entry(traced(Level::Error, "boom")).await;
    daemon.inject_entry(traced(Level::Info, "after-info")).await;
    wait_stored(&mut c, "after-info").await;

    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane, l>=WARN" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["early-warn", "boom"], "{r}");
    assert_eq!(r["cursor_late"], json!(1), "{r}");
}

/// The loss count reaches the reply: a late record evicted before the cursor read it is
/// reported once, and an ordinary read carries no such key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_record_evicted_before_the_read_is_reported_lost() {
    let mut config = default_test_config();
    config.buffer_size = 4;
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut c = marker_only_session(&daemon).await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Info, "marker").await;
    wait_stored(&mut c, "marker").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["marker"]);
    assert!(r.get("cursor_late_lost").is_none(), "{r}");

    daemon.inject_log(Level::Error, "boom").await;
    // The ERROR's post-window stores what follows; enough to push the late WARN out.
    for i in 0..8 {
        daemon.inject_log(Level::Info, &format!("marker {i}")).await;
    }
    wait_stored(&mut c, "marker 7").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert!(!messages(&r).contains(&"early-warn".to_string()), "{r}");
    assert_eq!(r["cursor_late_lost"], json!(1), "{r}");
}

/// A cursor read whose position has rolled out of the buffer says so — `truncated`, with the
/// count of what rolled off — like a `b>=` read always has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_read_past_an_evicted_stretch_is_truncated() {
    let mut config = default_test_config();
    config.buffer_size = 4;
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut c = daemon.connect_named("reader", None).await;
    daemon.inject_log(Level::Info, "first").await;
    wait_stored(&mut c, "first").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["first"]);
    for i in 0..10 {
        daemon.inject_log(Level::Info, &format!("next {i}")).await;
    }
    wait_stored(&mut c, "next 9").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 2 }))
        .await
        .unwrap();
    assert_eq!(r["truncated"], json!(true), "{r}");
    assert!(r["evicted_before_window"].as_u64().unwrap() > 0, "{r}");
}

/// `capped` on a cursor export is a fact: exactly `count` records left is not capped, one more
/// is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_export_is_capped_only_when_more_remain() {
    let daemon = spawn_test_daemon().await;
    let mut c = daemon.connect_named("reader", None).await;
    for i in 0..3 {
        daemon.inject_log(Level::Info, &format!("rec {i}")).await;
    }
    wait_stored(&mut c, "rec 2").await;
    let r: Value = c
        .call("logs.export", json!({ "filter": "c>=x", "count": 3 }))
        .await
        .unwrap();
    assert_eq!(r["count"], json!(3));
    assert_eq!(r["capped"], json!(false), "{r}");
    for i in 3..6 {
        daemon.inject_log(Level::Info, &format!("rec {i}")).await;
    }
    wait_stored(&mut c, "rec 5").await;
    let r: Value = c
        .call("logs.export", json!({ "filter": "c>=x", "count": 2 }))
        .await
        .unwrap();
    assert_eq!(r["capped"], json!(true), "{r}");
}

/// A bookmark whose explicit `start_seq` is ABOVE the counter keeps excluding what lies below
/// it after a restart: a WARN flushed late below that position is not returned. (A restored
/// floor of 0 returned it, though the same cursor before the restart did not.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_future_cursor_still_excludes_records_below_it() {
    let mut daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    let _: Value = c
        .call(
            "bookmarks.add",
            json!({ "name": "future", "start_seq": 1_000_000 }),
        )
        .await
        .unwrap();
    c.close().await.unwrap();
    daemon.restart().await;

    let mut c = daemon.connect_named("reader", None).await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=future", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new(), "{r}");
}

/// A cursor restored after a daemon restart still gets the records a trigger stores behind it
/// — the restored late mark belongs to the new incarnation's numbering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_cursor_gets_late_records_after_a_restart() {
    let mut daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    daemon.inject_log(Level::Info, "marker before").await;
    wait_stored(&mut c, "marker before").await;
    let _: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    c.close().await.unwrap();
    daemon.restart().await;

    let mut c = daemon.connect_named("reader", None).await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Info, "marker after").await;
    wait_stored(&mut c, "marker after").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["marker after"], "{r}");
    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=cur", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["early-warn", "boom"], "{r}");
}

/// V4: `logs.export` asks for one more record than it returns; a late record that probe took
/// and the reply dropped comes back on the next read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_export_probe_does_not_consume_the_record_it_drops() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    for w in ["w1", "w2", "w3"] {
        daemon.inject_log(Level::Warn, w).await;
    }
    daemon.inject_log(Level::Info, "marker").await;
    wait_stored(&mut c, "marker").await;
    let _: Value = c
        .call("logs.export", json!({ "filter": "c>=x" }))
        .await
        .unwrap();

    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;

    let r: Value = c
        .call("logs.export", json!({ "filter": "c>=x", "count": 2 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["w1", "w2"]);
    assert_eq!(r["capped"], json!(true));
    let typed: logmon_broker_protocol::LogsExportResult = serde_json::from_value(r).unwrap();
    assert_eq!(typed.cursor_late, Some(2));
    let r: Value = c
        .call("logs.export", json!({ "filter": "c>=x", "count": 10 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["w3", "boom"]);
}

/// A bookmark added while a filtered-out WARN is still in the pre-trigger buffer, read as a
/// cursor, still means "from now": a trigger that fires after the add flushes that WARN late
/// with a fresh late number, and the cursor must not take it — it arrived before the add.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_from_now_cursor_never_takes_a_record_that_arrived_before_it() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    daemon.inject_log(Level::Warn, "pre-run-warn").await;
    daemon.inject_log(Level::Info, "marker").await;
    wait_stored(&mut c, "marker").await;

    let _: Value = c
        .call("bookmarks.add", json!({ "name": "run" }))
        .await
        .unwrap();
    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;

    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=run", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["boom"], "{r}");
}

/// A cursor created after late records left the buffer does not report them as its own loss:
/// they were numbered before it existed, and `truncated` already says the buffer rolled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_cursor_does_not_report_late_records_lost_before_it_existed() {
    let mut config = default_test_config();
    config.buffer_size = 4;
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut c = marker_only_session(&daemon).await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Info, "marker").await;
    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;
    // The ERROR's post-window stores what follows; enough to push the late WARN out.
    for i in 0..8 {
        daemon.inject_log(Level::Info, &format!("marker {i}")).await;
    }
    wait_stored(&mut c, "marker 7").await;

    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=fresh", "count": 50 }))
        .await
        .unwrap();
    assert!(
        r.get("cursor_late_lost").is_none(),
        "a new cursor reported loss from before it existed: {r}"
    );
}

/// A method that does not take a cursor refuses `c>=` BEFORE resolving it — resolving
/// auto-creates the cursor, so a refusal after it left a bookmark behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_cursor_leaves_no_bookmark_behind() {
    let daemon = spawn_test_daemon().await;
    let mut c = daemon.connect_named("refused", None).await;
    let trace = format!("{TRACE:032x}");
    for (method, params) in [
        ("traces.recent", json!({ "filter": "c>=probe" })),
        (
            "traces.get",
            json!({ "trace_id": trace, "filter": "c>=probe" }),
        ),
        ("traces.slow", json!({ "filter": "c>=probe" })),
        ("spans.export", json!({ "filter": "c>=probe" })),
    ] {
        let err = c
            .call::<Value>(method, params)
            .await
            .expect_err("a cursor is refused");
        assert!(
            err.to_string().contains("cursor qualifier not permitted"),
            "{method}: {err}"
        );
        let listed: Value = c.call("bookmarks.list", json!({})).await.unwrap();
        assert_eq!(
            listed["bookmarks"].as_array().map(Vec::len),
            Some(0),
            "{method} left a bookmark: {listed}"
        );
    }
}

/// A5: a bookmark added AFTER a late flush, read as a cursor, means "from now" — it does not
/// replay the late records stored before it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bookmark_added_after_a_late_flush_does_not_replay_it() {
    let daemon = spawn_test_daemon().await;
    let mut c = marker_only_session(&daemon).await;
    daemon.inject_log(Level::Warn, "early-warn").await;
    daemon.inject_log(Level::Info, "marker").await;
    daemon.inject_log(Level::Error, "boom").await;
    wait_stored(&mut c, "boom").await;

    let _: Value = c
        .call("bookmarks.add", json!({ "name": "now" }))
        .await
        .unwrap();
    let r: Value = c
        .call("logs.recent", json!({ "filter": "c>=now", "count": 50 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new(), "{r}");
}
