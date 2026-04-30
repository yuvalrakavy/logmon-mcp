use chrono::{DateTime, Utc, TimeZone};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use thiserror::Error;

fn serialize_opt_trace_id<S: Serializer>(id: &Option<u128>, s: S) -> Result<S::Ok, S::Error> {
    match id {
        Some(v) => s.serialize_some(&format!("{:032x}", v)),
        None => s.serialize_none(),
    }
}

fn deserialize_opt_trace_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u128>, D::Error> {
    let opt: Option<String> = Option::deserialize(d)?;
    match opt {
        Some(s) => u128::from_str_radix(&s, 16).map(Some).map_err(serde::de::Error::custom),
        None => Ok(None),
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl Level {
    pub fn from_syslog(level: u8) -> Self {
        match level {
            0..=3 => Level::Error,
            4 => Level::Warn,
            5 | 6 => Level::Info,
            _ => Level::Debug,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "ERROR" => Some(Level::Error),
            "WARN" | "WARNING" => Some(Level::Warn),
            "INFO" => Some(Level::Info),
            "DEBUG" => Some(Level::Debug),
            "TRACE" => Some(Level::Trace),
            _ => None,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Level::Error => write!(f, "ERROR"),
            Level::Warn => write!(f, "WARN"),
            Level::Info => write!(f, "INFO"),
            Level::Debug => write!(f, "DEBUG"),
            Level::Trace => write!(f, "TRACE"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogSource {
    Filter,
    PreTrigger,
    PostTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    pub timestamp: DateTime<Utc>,
    pub level: Level,
    pub message: String,
    pub full_message: Option<String>,
    pub host: String,
    pub facility: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub additional_fields: HashMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none",
            serialize_with = "serialize_opt_trace_id", deserialize_with = "deserialize_opt_trace_id")]
    pub trace_id: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none",
            serialize_with = "serialize_opt_span_id", deserialize_with = "deserialize_opt_span_id")]
    pub span_id: Option<u64>,
    #[serde(default)]
    pub matched_filters: Vec<String>,
    #[serde(default = "default_source")]
    pub source: LogSource,
}

fn default_source() -> LogSource {
    LogSource::Filter
}

impl LogEntry {
    /// Construct a synthetic log entry for tests / harness injection.
    ///
    /// `seq` is set to 0 — the pipeline reassigns it via `assign_seq()` when the
    /// entry passes through `process_entry`.
    pub fn synthetic(level: Level, message: &str) -> Self {
        Self {
            seq: 0,
            timestamp: Utc::now(),
            level,
            message: message.to_string(),
            full_message: None,
            host: "test".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }
}

#[derive(Debug, Error)]
pub enum GelfParseError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("missing required field: {0}")]
    MissingField(&'static str),
}

pub fn parse_gelf_message(bytes: &[u8], seq: u64) -> Result<LogEntry, GelfParseError> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let obj = v.as_object().ok_or(GelfParseError::MissingField("root object"))?;

    let host = obj.get("host")
        .and_then(|v| v.as_str())
        .ok_or(GelfParseError::MissingField("host"))?
        .to_string();

    let message = obj.get("short_message")
        .and_then(|v| v.as_str())
        .ok_or(GelfParseError::MissingField("short_message"))?
        .to_string();

    let syslog_level = obj.get("level")
        .and_then(|v| v.as_u64())
        .unwrap_or(6) as u8;

    // Check for _level additional field (tracing-gelf sets this for TRACE)
    let level = obj.get("_level")
        .and_then(|v| v.as_str())
        .and_then(Level::from_name)
        .unwrap_or_else(|| Level::from_syslog(syslog_level));

    let timestamp = obj.get("timestamp")
        .and_then(|v| v.as_f64())
        .map(|ts| {
            let secs = ts as i64;
            let nanos = ((ts - secs as f64) * 1_000_000_000.0) as u32;
            Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(Utc::now)
        })
        .unwrap_or_else(Utc::now);

    let full_message = obj.get("full_message").and_then(|v| v.as_str()).map(String::from);
    let facility = obj.get("facility").and_then(|v| v.as_str()).map(String::from);
    let file = obj.get("file").and_then(|v| v.as_str()).map(String::from);
    let line = obj.get("line").and_then(|v| v.as_u64()).map(|v| v as u32);

    // Collect additional fields (keys starting with _), strip the _ prefix
    let mut additional_fields: HashMap<String, serde_json::Value> = obj.iter()
        .filter(|(k, _)| k.starts_with('_') && *k != "_level")
        .map(|(k, v)| (k[1..].to_string(), v.clone()))
        .collect();

    let trace_id = additional_fields.remove("trace_id")
        .and_then(|v| v.as_str().map(String::from))
        .and_then(|s| u128::from_str_radix(&s, 16).ok());

    let span_id = additional_fields.remove("span_id")
        .and_then(|v| v.as_str().map(String::from))
        .and_then(|s| u64::from_str_radix(&s, 16).ok());

    Ok(LogEntry {
        seq,
        timestamp,
        level,
        message,
        full_message,
        host,
        facility,
        file,
        line,
        additional_fields,
        trace_id,
        span_id,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    })
}
