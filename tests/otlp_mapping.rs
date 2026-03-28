use logmon_mcp_server::receiver::otlp::mapping::*;
use logmon_mcp_server::gelf::message::Level;

#[test]
fn test_severity_to_level() {
    assert_eq!(severity_to_level(1), Level::Trace);
    assert_eq!(severity_to_level(5), Level::Debug);
    assert_eq!(severity_to_level(9), Level::Info);
    assert_eq!(severity_to_level(13), Level::Warn);
    assert_eq!(severity_to_level(17), Level::Error);
    assert_eq!(severity_to_level(21), Level::Error);
    assert_eq!(severity_to_level(0), Level::Info);
}

#[test]
fn test_bytes_to_trace_id() {
    let bytes = vec![
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb1, 0x6e, 0x0f,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];
    assert_eq!(
        bytes_to_trace_id(&bytes),
        Some(0x4bf92f3577b16e0f0000000000000001_u128)
    );
}

#[test]
fn test_bytes_to_trace_id_empty() {
    assert_eq!(bytes_to_trace_id(&[]), None);
    assert_eq!(bytes_to_trace_id(&[0; 16]), None);
}

#[test]
fn test_bytes_to_span_id() {
    let bytes = vec![0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7];
    assert_eq!(
        bytes_to_span_id(&bytes),
        Some(0x00f067aa0ba902b7_u64)
    );
}

#[test]
fn test_bytes_to_span_id_empty() {
    assert_eq!(bytes_to_span_id(&[]), None);
    assert_eq!(bytes_to_span_id(&[0; 8]), None);
}

#[test]
fn test_nanos_to_datetime() {
    let nanos = 1_700_000_000_000_000_000_u64; // 2023-11-14T22:13:20Z
    let dt = nanos_to_datetime(nanos);
    assert_eq!(dt.timestamp(), 1700000000);
}

#[test]
fn test_extract_resource_attrs() {
    let attrs = vec![
        ("service.name".to_string(), "my-service".to_string()),
        ("host.name".to_string(), "my-host".to_string()),
        ("other".to_string(), "value".to_string()),
    ];
    let (service, host) = extract_resource_attrs(&attrs);
    assert_eq!(service, "my-service");
    assert_eq!(host, "my-host");
}

#[test]
fn test_extract_resource_attrs_defaults() {
    let attrs: Vec<(String, String)> = vec![];
    let (service, host) = extract_resource_attrs(&attrs);
    assert_eq!(service, "unknown");
    assert_eq!(host, "unknown");
}
