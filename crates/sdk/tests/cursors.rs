use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::LogsRecent;
use logmon_broker_sdk::{Broker, Filter};

#[test]
fn cursor_builder_emits_c_ge() {
    let f = Filter::builder().cursor("mycur").build();
    assert_eq!(f, "c>=mycur");
}

#[test]
fn cursor_combined_with_other_qualifiers() {
    let f = Filter::builder()
        .cursor("run-1")
        .level_at_least(logmon_broker_sdk::Level::Error)
        .build();
    assert!(f.contains("c>=run-1"));
    assert!(f.contains("l>=ERROR"));
}

#[tokio::test]
async fn cursor_end_to_end_advances_with_exact_max_seq() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    daemon.inject_log(Level::Info, "first").await;
    daemon.inject_log(Level::Info, "second").await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r1 = broker
        .logs_recent(LogsRecent {
            filter: Some(Filter::builder().cursor("e2e-cur").build()),
            count: Some(10),
            ..Default::default()
        })
        .await
        .unwrap();

    assert_eq!(r1.logs.len(), 2);
    assert_eq!(r1.logs[0].message, "first"); // oldest first
    let last_seq = r1.logs.last().unwrap().seq;
    assert_eq!(r1.cursor_advanced_to, Some(last_seq));
}
