use logmon_broker_core::gelf::message::{parse_gelf_message, Level};
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
    assert_eq!(
        entry.additional_fields.get("request_id").unwrap(),
        "abc-123"
    );
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

#[test]
fn test_parse_gelf_with_trace_context() {
    let json = r#"{"version":"1.1","host":"app","short_message":"traced log","_trace_id":"4bf92f3577b16e0f0000000000000001","_span_id":"00f067aa0ba902b7"}"#;
    let entry = parse_gelf_message(json.as_bytes(), 1).unwrap();
    assert_eq!(
        entry.trace_id,
        Some(0x4bf92f3577b16e0f0000000000000001_u128)
    );
    assert_eq!(entry.span_id, Some(0x00f067aa0ba902b7_u64));
    assert!(!entry.additional_fields.contains_key("trace_id"));
    assert!(!entry.additional_fields.contains_key("span_id"));
}

/// A `_trace_id` that is not lowercase hex must SURVIVE as an ordinary field.
///
/// The promotion used to `.remove()` the value and then parse what it had
/// removed, so anything that failed to parse was gone: not promoted, not
/// additional, no `GelfParseError` variant, and nothing in any reply to say so.
/// A UUID, a decimal id, an OTLP id with dashes — all ordinary things for an
/// emitter to send under that name, and all silently destroyed on ingest.
///
/// Keeping it is strictly better than erroring: the value stays queryable,
/// `logs.fields` reports it with a working selector, and the emitter's mistake
/// is visible rather than invisible.
#[test]
fn a_non_hex_trace_id_is_kept_as_an_additional_field_not_dropped() {
    // Note what is NOT in this list: a digits-only id. Every decimal digit is
    // also a hex digit, so `"12345678901234567890"` parses — as a completely
    // different number than the emitter meant. That is a real and separate
    // hazard (a decimal id silently reinterpreted as hex) and it is not what
    // this test is about; putting it here would have made the fixture assert
    // something false.
    for bad in [
        "4bf92f35-77b1-6e0f-0000-000000000001", // a UUID, dashes and all
        "0xdeadbeef",                           // the `0x` prefix is not hex
        "not-an-id",
        "", // empty
    ] {
        let json = format!(
            r#"{{"version":"1.1","host":"app","short_message":"m","_trace_id":"{bad}","_span_id":"{bad}"}}"#
        );
        let entry = parse_gelf_message(json.as_bytes(), 1).unwrap();

        assert_eq!(entry.trace_id, None, "`{bad}` is not a hex trace id");
        assert_eq!(
            entry.additional_fields.get("trace_id").and_then(|v| v.as_str()),
            Some(bad),
            "but the value the emitter sent must still be THERE -- removing it \
             and then failing to parse it is how `{bad}` disappeared entirely"
        );
        assert_eq!(
            entry.additional_fields.get("span_id").and_then(|v| v.as_str()),
            Some(bad)
        );
    }
}

/// And a NON-string value is kept too — the old code took the same path.
#[test]
fn a_numeric_trace_id_is_kept_as_an_additional_field() {
    let json = r#"{"version":"1.1","host":"app","short_message":"m","_trace_id":12345}"#;
    let entry = parse_gelf_message(json.as_bytes(), 1).unwrap();
    assert_eq!(entry.trace_id, None);
    assert_eq!(
        entry.additional_fields.get("trace_id").and_then(|v| v.as_u64()),
        Some(12345),
        "a number is not a hex string, and it must not vanish either"
    );
}

#[test]
fn test_parse_gelf_without_trace_context() {
    let json = r#"{"version":"1.1","host":"app","short_message":"plain log"}"#;
    let entry = parse_gelf_message(json.as_bytes(), 1).unwrap();
    assert_eq!(entry.trace_id, None);
    assert_eq!(entry.span_id, None);
}

#[test]
fn test_parse_gelf_invalid_trace_id() {
    let json =
        r#"{"version":"1.1","host":"app","short_message":"bad trace","_trace_id":"not-valid-hex"}"#;
    let entry = parse_gelf_message(json.as_bytes(), 1).unwrap();
    assert_eq!(entry.trace_id, None);
}
