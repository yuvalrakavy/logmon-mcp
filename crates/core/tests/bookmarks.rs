//! Integration test for the bookmarks feature.
//! Constructs the daemon's in-process pieces directly (no socket transport)
//! and exercises the full RPC path: parse → resolve bookmarks → match → return.

use chrono::Utc;
use logmon_broker_core::daemon::log_processor::process_entry;
use logmon_broker_core::daemon::rpc_handler::RpcHandler;
use logmon_broker_core::daemon::session::SessionRegistry;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq: 0,
        timestamp: Utc::now(),
        level,
        message: msg.to_string(),
        full_message: None,
        host: "test".into(),
        facility: Some("app".into()),
        file: None,
        line: None,
        additional_fields: HashMap::new(),
        trace_id: None,
        span_id: None,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

fn build_handler() -> (Arc<RpcHandler>, Arc<LogPipeline>, Arc<SessionRegistry>) {
    let seq = Arc::new(SeqCounter::new());
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone()));
    let span_store = Arc::new(SpanStore::new(1000, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let bookmarks = Arc::new(BookmarkStore::new());
    let metrics = Arc::new(ReceiverMetrics::new());
    let handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        span_store,
        sessions.clone(),
        bookmarks,
        metrics,
        vec!["test".into()],
    ));
    (handler, pipeline, sessions)
}

fn call(
    handler: &RpcHandler,
    session: &logmon_broker_core::daemon::session::SessionId,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let req = RpcRequest::new(1, method, params);
    let resp = handler.handle(session, &req);
    if let Some(err) = resp.error {
        Err(err.message)
    } else {
        Ok(resp.result.unwrap_or(Value::Null))
    }
}

#[test]
fn bookmarks_end_to_end() {
    let (handler, pipeline, sessions) = build_handler();
    let sid_a = sessions.create_named("A").expect("create session A");

    // 1. First batch of logs (before the bookmark)
    let mut e1 = make_entry(Level::Info, "first batch line 1");
    process_entry(&mut e1, &pipeline, &sessions);
    let mut e2 = make_entry(Level::Info, "first batch line 2");
    process_entry(&mut e2, &pipeline, &sessions);

    // 2. Set bookmark "before". Default `start_seq` snaps it to the current
    //    seq counter (= e2's seq), so `b>=before` matches every record with
    //    `entry.seq > e2.seq` — exactly the second-batch entries that follow.
    let r = call(
        &handler,
        &sid_a,
        "bookmarks.add",
        json!({ "name": "before" }),
    )
    .unwrap();
    assert_eq!(r["qualified_name"], "A/before");
    assert_eq!(r["replaced"], false);

    // 3. Second batch of logs (after the bookmark)
    let mut e3 = make_entry(Level::Warn, "second batch line 1");
    process_entry(&mut e3, &pipeline, &sessions);
    let mut e4 = make_entry(Level::Error, "second batch line 2");
    process_entry(&mut e4, &pipeline, &sessions);

    // 4. Set bookmark "after" with an explicit seq beyond e4's so `b<=after`
    //    captures the whole second batch (strict `entry.seq < bookmark.seq`).
    //    `start_seq: u64::MAX` means "everything that ever existed lies before
    //    this bookmark," which is the correct intent for an "after the second
    //    batch" anchor in this test.
    let r = call(
        &handler,
        &sid_a,
        "bookmarks.add",
        json!({ "name": "after", "start_seq": u64::MAX }),
    )
    .unwrap();
    assert_eq!(r["qualified_name"], "A/after");

    // 5. Query: between bookmarks, expect exactly the second batch.
    let r = call(
        &handler,
        &sid_a,
        "logs.recent",
        json!({ "filter": "b>=before, b<=after", "count": 100 }),
    )
    .unwrap();
    let logs = r["logs"].as_array().unwrap();
    assert_eq!(
        logs.len(),
        2,
        "expected only the second-batch entries; got {logs:?}"
    );
    let messages: Vec<&str> = logs
        .iter()
        .map(|l| l["message"].as_str().unwrap())
        .collect();
    assert!(messages.iter().any(|m| m.contains("second batch line 1")));
    assert!(messages.iter().any(|m| m.contains("second batch line 2")));

    // 6. Cross-session access from a second session
    let sid_b = sessions.create_named("B").expect("create session B");
    let r = call(
        &handler,
        &sid_b,
        "logs.recent",
        json!({ "filter": "b>=A/before, b<=A/after", "count": 100 }),
    )
    .unwrap();
    assert_eq!(r["logs"].as_array().unwrap().len(), 2);

    // 7. Replace flag actually overwrites
    let r = call(
        &handler,
        &sid_a,
        "bookmarks.add",
        json!({ "name": "before", "replace": true }),
    )
    .unwrap();
    assert_eq!(r["replaced"], true);

    // 8. Resolution failure returns a clear error
    let err = call(
        &handler,
        &sid_a,
        "logs.recent",
        json!({ "filter": "b>=ghost", "count": 10 }),
    )
    .unwrap_err();
    assert!(err.contains("bookmark not found"), "got: {err}");

    // 9. Registration guard rejects bookmark filters
    let err = call(
        &handler,
        &sid_a,
        "filters.add",
        json!({ "filter": "b>=before" }),
    )
    .unwrap_err();
    assert!(
        err.contains("not allowed in registered filters"),
        "got: {err}"
    );

    // 10. list_bookmarks shows both bookmarks alive (data still covers them).
    let r = call(&handler, &sid_a, "bookmarks.list", json!({})).unwrap();
    assert_eq!(r["count"], 2);

    // 11. remove_bookmark by bare name works
    call(
        &handler,
        &sid_a,
        "bookmarks.remove",
        json!({ "name": "before" }),
    )
    .unwrap();
    let r = call(&handler, &sid_a, "bookmarks.list", json!({})).unwrap();
    assert_eq!(r["count"], 1);
}
