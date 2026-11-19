//! §7 `status.get` situational awareness (Wave 2 stage 2.4): `current_domain`
//! plus an echo of `active_filters`, so one call answers "where am I / what is
//! narrowing me". Additive, back-compat (an older daemon → serde defaults).
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{DomainInfo, StatusGetResult};
use serde_json::{json, Value};

#[tokio::test]
async fn status_reports_current_domain_and_active_filters() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // Default binding + one registered filter.
    let _: Value = client
        .call("filters.add", json!({ "filter": "fa=web" }))
        .await
        .unwrap();
    let status: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    assert_eq!(
        status.current_domain, "default",
        "a fresh session is bound to default"
    );
    assert!(
        status.active_filters.iter().any(|f| f == "fa=web"),
        "active_filters must echo the session's registered filters: {:?}",
        status.active_filters
    );

    // current_domain follows use_domain.
    let _: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    let _: DomainInfo = client
        .call("domains.use", json!({ "name": "t3" }))
        .await
        .unwrap();
    let status: StatusGetResult = client.call("status.get", json!({})).await.unwrap();
    assert_eq!(
        status.current_domain, "t3",
        "current_domain follows the session's binding"
    );
}

#[tokio::test]
async fn test_log_queries_respect_domain_binding_and_publish_consistent_progress() {
    use logmon_broker_protocol::{TestLogListResult, TestLogStatusResult, TEST_LOG_SCHEMA_VERSION};
    let daemon = spawn_test_daemon().await;
    let mut observer = daemon.connect_anon().await;
    let mut runner = daemon.connect_anon().await;
    let initial: Value = observer.call("domains.list", json!({})).await.unwrap();
    for method in ["test_log.status", "test_log.list"] {
        let refused: Value = observer
            .call(method, json!({"domain":"absent"}))
            .await
            .unwrap();
        assert_eq!(refused["error"]["code"], "unknown_domain");
        assert_eq!(refused["schema_version"], TEST_LOG_SCHEMA_VERSION);
    }
    let after: Value = observer.call("domains.list", json!({})).await.unwrap();
    assert_eq!(
        after["domains"].as_array().unwrap().len(),
        initial["domains"].as_array().unwrap().len()
    );
    let idle: Value = observer.call("status.get", json!({})).await.unwrap();
    assert_eq!(idle["test_log_schema_version"], TEST_LOG_SCHEMA_VERSION);
    assert_eq!(idle.get("test_log"), Some(&Value::Null));

    let _: Value = runner
        .call("domains.create", json!({"name":"t3"}))
        .await
        .unwrap();
    let _: Value = runner
        .call("domains.use", json!({"name":"t3"}))
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let begin: Value = runner.call("test_log.begin", json!({
        "run_id":"night", "jsonl_path":tmp.path().join("run.jsonl"),"planned_executions":2, "grace_period_ms":0
    })).await.unwrap();
    assert!(begin.get("code").is_none(), "{begin}");
    let _: Value = runner.call("test_log.start", json!({
        "run_id":"night","execution_id":"one","name":"chain","started_at":chrono::Utc::now(),
        "selector":{"field":"trace_id","value":"0123456789abcdef0123456789abcdef"}
    })).await.unwrap();

    let active: Value = observer
        .call("test_log.status", json!({"domain":"t3"}))
        .await
        .unwrap();
    assert!(matches!(
        serde_json::from_value::<TestLogStatusResult>(active.clone()).unwrap(),
        TestLogStatusResult::Success { .. }
    ));
    assert_eq!(active["test_log"]["executions"]["planned"], 2);
    assert_eq!(active["test_log"]["executions"]["in_flight"], 1);
    assert_eq!(
        active["test_log"]["executions"]["by_verdict"],
        json!({"OK":0,"FAIL":0,"TIMEOUT":0,"ABORTED":0,"NO_VERDICT":0})
    );
    let list: Value = observer
        .call("test_log.list", json!({"domain":"t3"}))
        .await
        .unwrap();
    assert!(matches!(
        serde_json::from_value::<TestLogListResult>(list.clone()).unwrap(),
        TestLogListResult::Success { .. }
    ));
    assert_eq!(
        list["runs"][0]["last_event_at"],
        active["test_log"]["last_event_at"]
    );
    let lanes: Value = observer.call("domains.list", json!({})).await.unwrap();
    let lane = lanes["domains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "t3")
        .unwrap();
    let bound: Value = runner.call("status.get", json!({})).await.unwrap();
    assert_eq!(lane["test_log_schema_version"], TEST_LOG_SCHEMA_VERSION);
    assert_eq!(
        lane["test_log"],
        json!({"run_id":"night","state":"open","in_flight":1})
    );
    assert_eq!(bound["test_log"], lane["test_log"]);
    let unaffected: Value = observer.call("status.get", json!({})).await.unwrap();
    assert_eq!(unaffected["current_domain"], "default");
    let missing: Value = observer
        .call("test_log.status", json!({"domain":"t3","run_id":"missing"}))
        .await
        .unwrap();
    assert_eq!(missing["error"]["code"], "run_not_found");
    let _: Value = runner
        .call(
            "test_log.seal",
            json!({"run_id":"night","wait_timeout_ms":5000}),
        )
        .await
        .unwrap();
    let settled: Value = observer
        .call("test_log.status", json!({"domain":"t3","run_id":"night"}))
        .await
        .unwrap();
    assert_eq!(settled["test_log"]["state"], "sealed");
    let idle_again: Value = runner.call("test_log.status", json!({})).await.unwrap();
    assert_eq!(idle_again.get("test_log"), Some(&Value::Null));
}
