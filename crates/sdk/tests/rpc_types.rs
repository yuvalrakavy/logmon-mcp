use logmon_broker_protocol::*;

#[test]
fn test_rpc_request_roundtrip() {
    let req = RpcRequest::new(1, "logs.recent", serde_json::json!({"count": 10}));
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RpcRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, 1);
    assert_eq!(parsed.method, "logs.recent");
}

#[test]
fn test_rpc_response_success() {
    let resp = RpcResponse::success(42, serde_json::json!({"ok": true}));
    assert_eq!(resp.id, 42);
    assert!(resp.error.is_none());
    assert!(resp.result.is_some());
}

#[test]
fn test_rpc_response_error() {
    let resp = RpcResponse::error(42, -32601, "method not found");
    assert_eq!(resp.id, 42);
    assert!(resp.result.is_none());
    assert_eq!(resp.error.as_ref().unwrap().code, -32601);
}

#[test]
fn test_parse_daemon_message_response() {
    let json = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
    let msg = parse_daemon_message_from_str(&json.to_string()).unwrap();
    assert!(matches!(msg, DaemonMessage::Response(_)));
}

#[test]
fn test_parse_daemon_message_notification() {
    let json = serde_json::json!({"jsonrpc": "2.0", "method": "trigger.fired", "params": {}});
    let msg = parse_daemon_message_from_str(&json.to_string()).unwrap();
    assert!(matches!(msg, DaemonMessage::Notification(_)));
}

#[test]
fn test_parse_daemon_message_invalid_json() {
    assert!(parse_daemon_message_from_str("not json").is_err());
}

#[test]
fn test_session_start_params_roundtrip() {
    let params = SessionStartParams {
        name: Some("store-debug".to_string()),
        protocol_version: PROTOCOL_VERSION,
    };
    let json = serde_json::to_string(&params).unwrap();
    let parsed: SessionStartParams = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.name.as_deref(), Some("store-debug"));
    assert_eq!(parsed.protocol_version, PROTOCOL_VERSION);
}

#[test]
fn test_session_start_result_roundtrip() {
    let result = SessionStartResult {
        session_id: "test-session".to_string(),
        is_new: true,
        queued_notifications: 0,
        trigger_count: 2,
        filter_count: 0,
        daemon_uptime_secs: 100,
        buffer_size: 500,
        receivers: vec!["gelf (UDP:12201, TCP:12201)".to_string()],
    };
    let json = serde_json::to_string(&result).unwrap();
    let parsed: SessionStartResult = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.session_id, "test-session");
    assert!(parsed.is_new);
}
