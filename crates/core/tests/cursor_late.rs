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

    let r: Value = c
        .call(
            "traces.logs",
            json!({ "trace_id": trace, "filter": "c>=lane" }),
        )
        .await
        .unwrap();
    assert_eq!(messages(&r), Vec::<String>::new());
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
    let r: Value = c
        .call("logs.export", json!({ "filter": "c>=x", "count": 10 }))
        .await
        .unwrap();
    assert_eq!(messages(&r), vec!["w3", "boom"]);
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
