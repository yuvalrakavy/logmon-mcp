//! Integration test for the v1 capabilities advertised by `session.start`.
//!
//! The daemon must advertise a stable set of capability strings so SDKs can
//! feature-gate without sniffing protocol versions.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::SessionStartResult;

#[tokio::test]
async fn v1_capabilities_advertised() {
    let daemon = spawn_test_daemon().await;
    let result: SessionStartResult = daemon.session_start(None, None).await.unwrap();
    assert!(
        result.capabilities.contains(&"bookmarks".to_string()),
        "missing 'bookmarks' in capabilities: {:?}",
        result.capabilities
    );
    assert!(
        result.capabilities.contains(&"oneshot_triggers".to_string()),
        "missing 'oneshot_triggers' in capabilities: {:?}",
        result.capabilities
    );
    assert!(
        result.capabilities.contains(&"client_info".to_string()),
        "missing 'client_info' in capabilities: {:?}",
        result.capabilities
    );
}
