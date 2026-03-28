use logmon_mcp_server::span::types::*;
use chrono::Utc;
use std::collections::HashMap;

#[test]
fn test_span_entry_creation() {
    let span = SpanEntry {
        seq: 0,
        trace_id: 0x4bf92f3577b16e0f0000000000000001_u128,
        span_id: 0x00f067aa0ba902b7_u64,
        parent_span_id: None,
        start_time: Utc::now(),
        end_time: Utc::now(),
        duration_ms: 42.5,
        name: "query_database".to_string(),
        kind: SpanKind::Internal,
        service_name: "store_server".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    };
    assert_eq!(span.name, "query_database");
    assert_eq!(span.duration_ms, 42.5);
    assert!(span.parent_span_id.is_none());
}

#[test]
fn test_span_kind_default() {
    assert!(matches!(SpanKind::Unspecified, SpanKind::Unspecified));
}

#[test]
fn test_span_status_error() {
    let status = SpanStatus::Error("connection timeout".to_string());
    if let SpanStatus::Error(msg) = &status {
        assert_eq!(msg, "connection timeout");
    } else {
        panic!("expected Error");
    }
}

#[test]
fn test_span_event() {
    let event = SpanEvent {
        name: "exception".to_string(),
        timestamp: Utc::now(),
        attributes: {
            let mut m = HashMap::new();
            m.insert("exception.message".to_string(), serde_json::json!("null pointer"));
            m
        },
    };
    assert_eq!(event.name, "exception");
}

#[test]
fn test_trace_id_hex_formatting() {
    let trace_id: u128 = 0x4bf92f3577b16e0f0000000000000001;
    let hex = format!("{:032x}", trace_id);
    assert_eq!(hex, "4bf92f3577b16e0f0000000000000001");
    assert_eq!(hex.len(), 32);
}

#[test]
fn test_trace_id_hex_parsing() {
    let hex = "4bf92f3577b16e0f0000000000000001";
    let parsed = u128::from_str_radix(hex, 16).unwrap();
    assert_eq!(parsed, 0x4bf92f3577b16e0f0000000000000001);
}

#[test]
fn test_span_id_hex_formatting() {
    let span_id: u64 = 0x00f067aa0ba902b7;
    let hex = format!("{:016x}", span_id);
    assert_eq!(hex, "00f067aa0ba902b7");
    assert_eq!(hex.len(), 16);
}

#[test]
fn test_span_entry_serialization() {
    let span = SpanEntry {
        seq: 1,
        trace_id: 0xabc123_u128,
        span_id: 0xdef456_u64,
        parent_span_id: Some(0xdef000_u64),
        start_time: Utc::now(),
        end_time: Utc::now(),
        duration_ms: 100.0,
        name: "test".to_string(),
        kind: SpanKind::Server,
        service_name: "svc".to_string(),
        status: SpanStatus::Unset,
        attributes: HashMap::new(),
        events: vec![],
    };
    let json = serde_json::to_value(&span).unwrap();
    // trace_id and span_id should serialize as hex strings
    assert!(json["trace_id"].is_string());
    assert!(json["span_id"].is_string());
}

#[test]
fn test_trace_summary_creation() {
    let summary = TraceSummary {
        trace_id: 0xabc_u128,
        root_span_name: "POST /api/test".to_string(),
        service_name: "myapp".to_string(),
        start_time: Utc::now(),
        total_duration_ms: 250.0,
        span_count: 5,
        has_errors: false,
        linked_log_count: 3,
    };
    assert_eq!(summary.span_count, 5);
    assert!(!summary.has_errors);
}
