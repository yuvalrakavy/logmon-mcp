//! `logs.fields` over the wire — the contract, not the projection.
//!
//! The projection's arithmetic is unit-tested next to it (`logs::fields`).
//! What only a live daemon can show is the reply SHAPE, the rejections, and —
//! the one that matters most — that the walk covers the whole ring rather than
//! the newest slice, since the handler is where a `count`-limited primitive
//! would have been reached for.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use serde_json::{json, Value};

/// Look a field row up by name, so a reordering does not silently change what
/// an assertion is about.
fn field<'a>(reply: &'a Value, name: &str) -> Option<&'a Value> {
    reply["fields"]
        .as_array()?
        .iter()
        .find(|f| f["field"].as_str() == Some(name))
}

/// Injection is asynchronous, so a read taken immediately can see a partial
/// buffer. Poll until the expected count lands rather than sleeping a guessed
/// interval — a fixed sleep would make every assertion below flaky-or-slow, and
/// a SHORT buffer is exactly what a whole-ring test must not silently accept.
async fn settle(client: &mut TestClient, want: usize) {
    for _ in 0..80 {
        let r: Value = client.call("logs.fields", json!({})).await.unwrap();
        if r["matched"].as_u64().unwrap_or(0) >= want as u64 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("only saw fewer than {want} records after 2s; the buffer never filled")
}

#[tokio::test]
async fn fields_describes_the_whole_buffer_not_the_newest_slice() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Deliberately more than any plausible default `count`. A handler that
    // reached for `recent_logs_with_stats` would describe the tail of this and
    // look entirely correct.
    const N: usize = 400;
    for i in 0..N {
        daemon.inject_log(Level::Info, &format!("m{i}")).await;
    }
    settle(&mut client, N).await;

    let reply: Value = client.call("logs.fields", json!({})).await.unwrap();

    assert_eq!(
        reply["matched"].as_u64(),
        Some(N as u64),
        "every record matched an absent filter"
    );
    assert_eq!(
        reply["scanned"].as_u64(),
        Some(N as u64),
        "and every record was EXAMINED -- a count-limited scan here would \
         describe the newest slice while claiming to describe the buffer"
    );

    let message = field(&reply, "message").expect("message is a reported field");
    assert_eq!(
        message["present"].as_u64(),
        Some(N as u64),
        "presence counted over the whole population"
    );
    assert_eq!(
        message["distinct"].as_u64(),
        Some(N as u64),
        "all {N} messages are distinct; a truncated walk would under-report"
    );
}

#[tokio::test]
async fn an_absent_builtin_is_reported_at_zero_rather_than_omitted() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Warn, "something").await;
    settle(&mut client, 1).await;

    let reply: Value = client.call("logs.fields", json!({})).await.unwrap();

    // Synthetic entries carry no facility. The row must still be there: an
    // agent choosing an axis needs "exists but never populated" to be
    // distinguishable from "no such field", and omission collapses them.
    let fa = field(&reply, "facility").expect(
        "an absent built-in must still be reported -- omitting it reads as \
         'no such field', which sends the caller looking for a different name",
    );
    assert_eq!(fa["present"].as_u64(), Some(0));
    assert_eq!(fa["coverage_pct"].as_f64(), Some(0.0));

    // And the promoted pair are present as rows even with no trace ids, since
    // grouping by `trace_id` is the natural thing for an agent to try.
    let t = field(&reply, "trace_id").expect("promoted rows are seeded");
    assert_eq!(t["source"].as_str(), Some("promoted"));
    assert!(
        t.get("selector").is_none() || t["selector"].is_null(),
        "no log filter selector reaches trace_id; offering one would \
         manufacture a silent-empty filter"
    );
}

/// An empty filter means "no filter", not a parse error.
///
/// Bypassing `parse_and_resolve_filter` to reject the cursor early also
/// bypassed the other half of its contract. Every sibling still routed through
/// it kept accepting `""`, so this method alone would have errored — a
/// divergence nobody would predict from the parameter's documentation.
#[tokio::test]
async fn an_empty_filter_means_no_filter() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    for f in ["", "   "] {
        let reply: Value = client
            .call("logs.fields", json!({ "filter": f }))
            .await
            .unwrap_or_else(|e| panic!("filter {f:?} must mean ALL, got error: {e}"));
        assert_eq!(
            reply["matched"].as_u64(),
            Some(1),
            "and match everything, exactly as logs.recent does with the same input"
        );
    }
}

/// The sibling site had the identical cursor defect; fixing one of two is how a
/// fixed defect stays alive.
#[tokio::test]
async fn traces_profile_also_creates_no_bookmark_on_a_refused_cursor() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    client
        .call::<Value>("traces.profile", json!({ "filter": "c>=never_seen_here" }))
        .await
        .expect_err("refused");

    let after: Value = client.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(
        after["count"].as_u64(),
        Some(0),
        "traces.profile must not write on a refusal either: {after}"
    );
}

/// A built-in and a same-named additional field are DIFFERENT rows.
///
/// GELF strips `_`, so `_file` becomes `additional_fields["file"]` beside the
/// top-level `file`. Merged, coverage exceeded 100% and the built-in's row —
/// the one this whole read exists to surface — was absorbed.
#[tokio::test]
async fn a_builtin_and_a_same_named_extra_do_not_merge() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let mut e = logmon_broker_core::gelf::message::LogEntry::synthetic(Level::Info, "m");
    e.file = Some("builtin.rs".into());
    e.additional_fields
        .insert("file".into(), json!("extra.rs"));
    daemon.inject_entry(e).await;
    settle(&mut client, 1).await;

    let reply: Value = client.call("logs.fields", json!({})).await.unwrap();
    let rows: Vec<&Value> = reply["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .filter(|f| f["field"].as_str() == Some("file"))
        .collect();

    assert_eq!(rows.len(), 2, "two sources, two rows: {rows:?}");
    for r in &rows {
        assert_eq!(r["present"].as_u64(), Some(1), "one record, not two");
        assert!(
            r["coverage_pct"].as_f64().unwrap_or(0.0) <= 100.0,
            "coverage cannot exceed 100% of matched: {r}"
        );
    }
    let selectors: Vec<Option<&str>> = rows.iter().map(|r| r["selector"].as_str()).collect();
    assert!(
        selectors.contains(&Some("fi")) && selectors.contains(&Some("file")),
        "and they carry the different selectors that reach them: {selectors:?}"
    );
}

/// The completeness channel actually FIRES.
///
/// Mutation found the gap: hardcoding `evicted_before_window` to `None` left
/// every test in this file green, because nothing drove a real eviction. The
/// default filter has no lower bound, so `truncated` is structurally false —
/// only a bound filter over an evicted region can exercise it.
#[tokio::test]
async fn truncated_fires_when_the_window_predates_the_buffer() {
    // A ring small enough to wrap in this test's lifetime.
    let mut config = default_test_config();
    config.buffer_size = 20;
    let daemon = TestDaemonHandle::spawn_with_config(config).await;
    let mut client = daemon.connect_anon().await;

    client
        .call::<Value>("bookmarks.add", json!({ "name": "start" }))
        .await
        .expect("mark before the flood");

    for i in 0..200 {
        daemon.inject_log(Level::Info, &format!("m{i}")).await;
    }
    // Wait for the ring to wrap. Polling on `lost_below` rather than sleeping:
    // the property under test is eviction, and a fixed sleep that fired early
    // would make this pass for the wrong reason.
    let mut wrapped = false;
    for _ in 0..80 {
        let r: Value = client.call("logs.fields", json!({})).await.unwrap();
        if r["lost_below"].as_u64().unwrap_or(0) > 0 {
            wrapped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(wrapped, "the ring never wrapped, so this proves nothing");

    let reply: Value = client
        .call("logs.fields", json!({ "filter": "b>=start" }))
        .await
        .unwrap();

    assert_eq!(
        reply["truncated"].as_bool(),
        Some(true),
        "the window starts below what the ring still holds: {reply}"
    );
    assert!(
        reply["evicted_before_window"].as_u64().unwrap_or(0) > 0,
        "and says how much rolled off: {reply}"
    );
}

#[tokio::test]
async fn a_cursor_qualifier_is_rejected() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    client
        .call::<Value>("bookmarks.add", json!({ "name": "mark" }))
        .await
        .expect("bookmark set");

    // `b>=` is the read-only form and must work.
    client
        .call::<Value>("logs.fields", json!({ "filter": "b>=mark" }))
        .await
        .expect("a bookmark bound is fine -- it does not advance");

    // `c>=` advances on read, so a second identical call would describe a
    // different population. A description of the buffer has to be repeatable.
    let err = client
        .call::<Value>("logs.fields", json!({ "filter": "c>=mark" }))
        .await
        .expect_err("a cursor qualifier must be refused");
    let msg = format!("{err}");
    assert!(
        msg.contains("cursor qualifier not permitted"),
        "and the refusal must say why: {msg}"
    );
    assert!(
        msg.contains("b>="),
        "naming the read-only alternative, or the caller has to guess: {msg}"
    );
}

/// A refusal must leave NOTHING behind.
///
/// `resolve_bookmarks` -> `cursor_read_and_advance` auto-creates a bookmark at
/// seq 0 for an unknown cursor name, as a side effect of resolution. Checking
/// the commit handle afterwards is too late — the refusal was honest about not
/// advancing and silently wrong about not writing.
///
/// The test above cannot catch this: it reuses a bookmark created moments
/// earlier, so resolution takes the read-existing branch and never inserts. An
/// UNKNOWN name is the whole fixture.
#[tokio::test]
async fn a_refused_cursor_creates_no_bookmark() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    let before: Value = client.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(before["count"].as_u64(), Some(0), "clean slate");

    client
        .call::<Value>("logs.fields", json!({ "filter": "c>=never_seen_before" }))
        .await
        .expect_err("refused");

    let after: Value = client.call("bookmarks.list", json!({})).await.unwrap();
    assert_eq!(
        after["count"].as_u64(),
        Some(0),
        "a refused call must not mutate the bookmark store; this read is the \
         first thing an agent runs against an unfamiliar buffer, and a quiet \
         workload would accumulate one of these per typo: {after}"
    );
}

#[tokio::test]
async fn the_reply_carries_its_completeness_channel() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    let reply: Value = client.call("logs.fields", json!({})).await.unwrap();

    // A description of a population that will not say whether the population
    // was complete is the defect this whole family of reads exists to avoid.
    assert!(
        reply.get("truncated").is_some(),
        "the completeness channel is part of the contract, not an extra"
    );
    assert_eq!(reply["truncated"].as_bool(), Some(false), "nothing evicted");
    assert!(reply.get("buffer_total").is_some());
}

#[tokio::test]
async fn min_coverage_hides_rows_and_zero_shows_everything() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    daemon.inject_log(Level::Info, "one").await;
    settle(&mut client, 1).await;

    let all: Value = client.call("logs.fields", json!({})).await.unwrap();
    let filtered: Value = client
        .call("logs.fields", json!({ "min_coverage_pct": 50.0 }))
        .await
        .unwrap();

    let n_all = all["fields"].as_array().map(|a| a.len()).unwrap_or(0);
    let n_filtered = filtered["fields"].as_array().map(|a| a.len()).unwrap_or(0);

    assert!(
        n_filtered < n_all,
        "the 0%-coverage built-ins are exactly what the threshold removes \
         ({n_filtered} vs {n_all})"
    );
    assert!(
        field(&filtered, "facility").is_none(),
        "a 0% row is below any positive threshold"
    );
    assert!(
        field(&filtered, "message").is_some(),
        "while a 100% row survives"
    );
}
