use logmon_mcp_server::gelf::message::{Level, parse_gelf_message};
use serde_json::json;

#[test]
fn test_level_from_syslog() {
    assert_eq!(Level::from_syslog(0), Level::Error);
    assert_eq!(Level::from_syslog(3), Level::Error);
    assert_eq!(Level::from_syslog(4), Level::Warn);
    assert_eq!(Level::from_syslog(5), Level::Info);
    assert_eq!(Level::from_syslog(6), Level::Info);
    assert_eq!(Level::from_syslog(7), Level::Debug);
}

#[test]
fn test_level_severity_ordering() {
    assert!(Level::Error > Level::Warn);
    assert!(Level::Warn > Level::Info);
    assert!(Level::Info > Level::Debug);
    assert!(Level::Debug > Level::Trace);
}

#[test]
fn test_parse_minimal_gelf() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "something happened",
        "level": 4
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 1).unwrap();
    assert_eq!(entry.host, "myapp");
    assert_eq!(entry.message, "something happened");
    assert_eq!(entry.level, Level::Warn);
    assert_eq!(entry.seq, 1);
}

#[test]
fn test_parse_full_gelf() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "timeout",
        "full_message": "stack trace here",
        "level": 3,
        "facility": "myapp::network",
        "file": "network.rs",
        "line": 42,
        "timestamp": 1700000000.123,
        "_request_id": "abc-123",
        "_user": "admin"
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 5).unwrap();
    assert_eq!(entry.level, Level::Error);
    assert_eq!(entry.full_message.as_deref(), Some("stack trace here"));
    assert_eq!(entry.facility.as_deref(), Some("myapp::network"));
    assert_eq!(entry.file.as_deref(), Some("network.rs"));
    assert_eq!(entry.line, Some(42));
    assert_eq!(entry.additional_fields.get("request_id").unwrap(), "abc-123");
    assert_eq!(entry.additional_fields.get("user").unwrap(), "admin");
}

#[test]
fn test_parse_gelf_missing_required_fields() {
    let raw = json!({"version": "1.1"});
    assert!(parse_gelf_message(raw.to_string().as_bytes(), 1).is_err());
}

#[test]
fn test_parse_gelf_invalid_json() {
    assert!(parse_gelf_message(b"not json", 1).is_err());
}

#[test]
fn test_trace_level_from_additional_field() {
    let raw = json!({
        "version": "1.1",
        "host": "myapp",
        "short_message": "trace msg",
        "level": 7,
        "_level": "TRACE"
    });
    let entry = parse_gelf_message(raw.to_string().as_bytes(), 1).unwrap();
    assert_eq!(entry.level, Level::Trace);
}
