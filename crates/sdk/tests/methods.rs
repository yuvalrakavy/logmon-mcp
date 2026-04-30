//! Integration tests for the SDK's typed method dispatch (Task 13).

use logmon_broker_core::test_support::spawn_test_daemon_for_sdk;
use logmon_broker_protocol::{
    LogsRecent, StatusGet, StatusGetResult, TriggersAdd, TriggersList, TriggersListResult,
};
use logmon_broker_sdk::Broker;

#[tokio::test]
async fn typed_methods_round_trip() {
    let daemon = spawn_test_daemon_for_sdk().await;
    let broker = Broker::connect()
        .socket_path(daemon.socket_path.clone())
        .open()
        .await
        .unwrap();

    // logs.recent — typed param, typed result
    let result = broker
        .logs_recent(LogsRecent {
            count: Some(10),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(result.logs.is_empty());
    assert_eq!(result.count, 0);

    // triggers.add with oneshot
    let added = broker
        .triggers_add(TriggersAdd {
            filter: "l>=ERROR".into(),
            oneshot: true,
            description: Some("test".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(added.id > 0);

    // triggers.list — confirms the just-added trigger round-trips back
    let triggers: TriggersListResult = broker.triggers_list(TriggersList {}).await.unwrap();
    assert!(
        triggers.triggers.iter().any(|t| t.id == added.id && t.oneshot),
        "expected triggers.list to contain trigger {} with oneshot=true; got {:?}",
        added.id,
        triggers.triggers
    );

    // status.get — daemon health probe
    let status: StatusGetResult = broker.status_get(StatusGet {}).await.unwrap();
    // daemon_uptime_secs is u64, so >= 0 is trivially true; assert the
    // session info is populated instead.
    assert!(
        status.session.is_some(),
        "status.get should return session info for the connected client"
    );
}
