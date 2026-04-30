//! Tests for the SDK's typed filter builder (Task 15).

use logmon_broker_sdk::{Filter, FilterSpanKind, FilterSpanStatus, Level};

#[test]
fn level_at_least() {
    let f = Filter::builder().level_at_least(Level::Error).build();
    assert_eq!(f, "l>=ERROR");
}

#[test]
fn multiple_qualifiers_comma_joined() {
    let f = Filter::builder()
        .level_at_least(Level::Error)
        .message("started")
        .build();
    assert!(f.contains("l>=ERROR"));
    assert!(f.contains("m=started"));
    assert!(f.contains(", "));
}

#[test]
fn regex_with_case_insensitive() {
    let f = Filter::builder()
        .pattern_regex("/panic|unwrap/", true)
        .build();
    assert_eq!(f, "/panic|unwrap/i");
}

#[test]
fn regex_without_slashes_input() {
    // Caller may also pass a raw regex body — `regex_lit` strips slashes
    // either way.
    let f = Filter::builder()
        .pattern_regex("panic|unwrap", false)
        .build();
    assert_eq!(f, "/panic|unwrap/");
}

#[test]
fn additional_field_substring() {
    let f = Filter::builder()
        .additional_field("_test_id", "run_abc")
        .build();
    assert_eq!(f, "_test_id=run_abc");
}

#[test]
fn span_status_typed_enum() {
    let f = Filter::builder()
        .span_status(FilterSpanStatus::Error)
        .build();
    assert_eq!(f, "st=error");
}

#[test]
fn span_kind_typed_enum() {
    let f = Filter::builder().span_kind(FilterSpanKind::Server).build();
    assert_eq!(f, "sk=server");
}

#[test]
fn duration_at_least() {
    let f = Filter::builder().duration_at_least_ms(100).build();
    assert_eq!(f, "d>=100");
}

#[test]
fn bookmark_after() {
    let f = Filter::builder().bookmark_after("test_start").build();
    assert_eq!(f, "b>=test_start");
}

#[test]
fn match_all() {
    let f = Filter::builder().match_all().build();
    assert_eq!(f, "ALL");
}

#[test]
fn match_none() {
    let f = Filter::builder().match_none().build();
    assert_eq!(f, "NONE");
}

#[test]
fn quoted_value_with_comma() {
    let f = Filter::builder().message("hello, world").build();
    assert_eq!(f, "m=\"hello, world\"");
}

#[test]
fn quoted_value_with_double_quote_is_escaped() {
    let f = Filter::builder().message(r#"say "hi","#).build();
    // Comma forces quoting; embedded double-quotes are backslash-escaped.
    assert_eq!(f, r#"m="say \"hi\",""#);
}

#[test]
fn empty_builder_produces_empty_string() {
    let f = Filter::builder().build();
    assert_eq!(f, "");
}

// ---- Integration test: builder output round-trips through the broker. ----

#[tokio::test]
async fn builder_strings_parse_in_broker() {
    use logmon_broker_core::gelf::message::Level as GelfLevel;
    use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
    use logmon_broker_protocol::{LogsRecent, TriggersList, TriggersRemove};
    use logmon_broker_sdk::Broker;

    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    // Drop default triggers so their post-window doesn't trap the second
    // `info` log into the stored buffer.
    let triggers = broker.triggers_list(TriggersList {}).await.unwrap();
    for t in &triggers.triggers {
        broker
            .triggers_remove(TriggersRemove { id: t.id })
            .await
            .unwrap();
    }

    daemon.inject_log(GelfLevel::Error, "first").await;
    daemon.inject_log(GelfLevel::Info, "second").await;

    // Give the pipeline a moment to ingest both entries.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let f = Filter::builder().level_at_least(Level::Error).build();
    let r = broker
        .logs_recent(LogsRecent {
            filter: Some(f),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(r.count, 1, "expected exactly one ERROR log, got {r:?}");
    assert_eq!(r.logs[0].message, "first");
}
