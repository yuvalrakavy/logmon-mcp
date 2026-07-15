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
