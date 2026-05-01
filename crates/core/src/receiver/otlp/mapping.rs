use crate::gelf::message::Level;
use chrono::{DateTime, Utc, TimeZone};

/// Map OTLP severity_number to our Level enum.
/// OTLP severity: 1-4=Trace, 5-8=Debug, 9-12=Info, 13-16=Warn, 17-24=Error/Fatal
pub fn severity_to_level(severity: i32) -> Level {
    match severity {
        1..=4 => Level::Trace,
        5..=8 => Level::Debug,
        9..=12 => Level::Info,
        13..=16 => Level::Warn,
        17..=24 => Level::Error,
        _ => Level::Info, // Unspecified → Info
    }
}

/// Convert 16-byte trace_id to u128. Returns None if empty or all zeros.
pub fn bytes_to_trace_id(bytes: &[u8]) -> Option<u128> {
    if bytes.len() != 16 {
        return None;
    }
    let val = u128::from_be_bytes(bytes.try_into().ok()?);
    if val == 0 { None } else { Some(val) }
}

/// Convert 8-byte span_id to u64. Returns None if empty or all zeros.
pub fn bytes_to_span_id(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != 8 {
        return None;
    }
    let val = u64::from_be_bytes(bytes.try_into().ok()?);
    if val == 0 { None } else { Some(val) }
}

/// Convert nanosecond timestamp to DateTime<Utc>.
pub fn nanos_to_datetime(nanos: u64) -> DateTime<Utc> {
    let secs = (nanos / 1_000_000_000) as i64;
    let nsecs = (nanos % 1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nsecs).single().unwrap_or_else(Utc::now)
}

/// Extract service.name and host.name from key-value pairs.
pub fn extract_resource_attrs(attrs: &[(String, String)]) -> (String, String) {
    let mut service = "unknown".to_string();
    let mut host = "unknown".to_string();
    for (k, v) in attrs {
        match k.as_str() {
            "service.name" => service = v.clone(),
            "host.name" => host = v.clone(),
            _ => {}
        }
    }
    (service, host)
}

/// Convert OTLP KeyValue attributes to a list of (key, value_string) pairs.
/// This extracts the string representation from AnyValue variants.
pub fn key_values_to_pairs(
    kvs: &[opentelemetry_proto::tonic::common::v1::KeyValue],
) -> Vec<(String, String)> {
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    kvs.iter()
        .filter_map(|kv| {
            let val = kv.value.as_ref()?.value.as_ref()?;
            let s = match val {
                Value::StringValue(s) => s.clone(),
                Value::IntValue(i) => i.to_string(),
                Value::DoubleValue(d) => d.to_string(),
                Value::BoolValue(b) => b.to_string(),
                Value::BytesValue(b) => format!("{:?}", b),
                Value::ArrayValue(_) => return None,
                Value::KvlistValue(_) => return None,
            };
            Some((kv.key.clone(), s))
        })
        .collect()
}
