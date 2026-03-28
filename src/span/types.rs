use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer, Deserializer};
use std::collections::HashMap;

fn serialize_trace_id<S: Serializer>(id: &u128, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:032x}", id))
}

fn deserialize_trace_id<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
    let s = String::deserialize(d)?;
    u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
}

fn serialize_span_id<S: Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("{:016x}", id))
}

fn deserialize_span_id<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    u64::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
}

fn serialize_opt_span_id<S: Serializer>(id: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    match id {
        Some(v) => s.serialize_some(&format!("{:016x}", v)),
        None => s.serialize_none(),
    }
}

fn deserialize_opt_span_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let opt: Option<String> = Option::deserialize(d)?;
    match opt {
        Some(s) => u64::from_str_radix(&s, 16).map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEntry {
    pub seq: u64,
    #[serde(serialize_with = "serialize_trace_id", deserialize_with = "deserialize_trace_id")]
    pub trace_id: u128,
    #[serde(serialize_with = "serialize_span_id", deserialize_with = "deserialize_span_id")]
    pub span_id: u64,
    #[serde(serialize_with = "serialize_opt_span_id", deserialize_with = "deserialize_opt_span_id")]
    pub parent_span_id: Option<u64>,

    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub duration_ms: f64,

    pub name: String,
    pub kind: SpanKind,
    pub service_name: String,

    pub status: SpanStatus,

    pub attributes: HashMap<String, serde_json::Value>,
    pub events: Vec<SpanEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "message")]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanEvent {
    pub name: String,
    pub timestamp: DateTime<Utc>,
    pub attributes: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    #[serde(serialize_with = "serialize_trace_id", deserialize_with = "deserialize_trace_id")]
    pub trace_id: u128,
    pub root_span_name: String,
    pub service_name: String,
    pub start_time: DateTime<Utc>,
    pub total_duration_ms: f64,
    pub span_count: u32,
    pub has_errors: bool,
    pub linked_log_count: u32,
}
