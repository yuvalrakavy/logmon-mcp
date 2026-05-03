use crate::gelf::message::{LogEntry, LogSource};
use crate::receiver::otlp::mapping::*;
use crate::receiver::{ReceiverMetrics, ReceiverSource};
use crate::span::types::*;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
struct AppState {
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    metrics: Arc<ReceiverMetrics>,
}

/// Threshold above which OTLP HTTP returns 429 instead of consuming the
/// remaining channel headroom. Compliant clients (tracing-init's OTLP
/// exporter) retry with exponential backoff, so this is a soft brake — no
/// information is lost as long as clients honour it.
///
/// Counter semantics: when the threshold gate fires and we return 429, we
/// do NOT bump per-source drop counters — the 429 response IS the
/// backpressure signal, observable to clients (and their own metrics).
/// Per-source counters track entries that survived the gate but lost the
/// race to a full channel mid-loop. This keeps `status.get`'s
/// `receiver_drops` honest about "broker actually dropped this entry"
/// versus "broker rejected the request body wholesale, client knows."
const BACKPRESSURE_THRESHOLD_PCT: u64 = 80;

fn channel_used_pct<T>(sender: &mpsc::Sender<T>) -> u64 {
    let cap = sender.max_capacity() as u64;
    if cap == 0 {
        return 0;
    }
    let avail = sender.capacity() as u64;
    (cap - avail) * 100 / cap
}

pub async fn start_http_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    metrics: Arc<ReceiverMetrics>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let state = AppState {
        log_sender,
        span_sender,
        metrics,
    };
    let app = Router::new()
        .route("/v1/logs", post(handle_logs))
        .route("/v1/traces", post(handle_traces))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("OTLP HTTP server listening on {}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.recv().await;
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON extraction helpers
// ---------------------------------------------------------------------------

/// Extract resource attributes from an OTLP JSON resource object.
/// Expects `value.resource.attributes[]` where each attribute has `key` and `value`.
fn extract_json_resource_attrs(resource_value: &serde_json::Value) -> Vec<(String, String)> {
    let attrs = resource_value
        .get("resource")
        .and_then(|r| r.get("attributes"))
        .and_then(|a| a.as_array());

    match attrs {
        Some(arr) => arr
            .iter()
            .filter_map(|attr| {
                let key = attr.get("key")?.as_str()?;
                let val = extract_json_attr_string_value(attr.get("value")?)?;
                Some((key.to_string(), val))
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Extract a string representation from an OTLP JSON attribute value object.
/// Handles `stringValue`, `intValue`, `doubleValue`, `boolValue`.
fn extract_json_attr_string_value(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.get("stringValue").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = value.get("intValue") {
        // OTLP JSON encodes intValue as a string
        return Some(s.as_str().map(|s| s.to_string()).unwrap_or_else(|| {
            s.as_i64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| s.to_string())
        }));
    }
    if let Some(d) = value.get("doubleValue").and_then(|v| v.as_f64()) {
        return Some(d.to_string());
    }
    if let Some(b) = value.get("boolValue").and_then(|v| v.as_bool()) {
        return Some(b.to_string());
    }
    None
}

/// Convert an OTLP JSON attribute value to a serde_json::Value.
fn json_attr_to_json_value(value: &serde_json::Value) -> serde_json::Value {
    if let Some(s) = value.get("stringValue").and_then(|v| v.as_str()) {
        return serde_json::Value::String(s.to_string());
    }
    if let Some(s) = value.get("intValue") {
        if let Some(n) = s.as_str().and_then(|s| s.parse::<i64>().ok()) {
            return serde_json::json!(n);
        }
        if let Some(n) = s.as_i64() {
            return serde_json::json!(n);
        }
        return s.clone();
    }
    if let Some(d) = value.get("doubleValue").and_then(|v| v.as_f64()) {
        return serde_json::json!(d);
    }
    if let Some(b) = value.get("boolValue").and_then(|v| v.as_bool()) {
        return serde_json::Value::Bool(b);
    }
    if let Some(arr) = value.get("arrayValue").and_then(|v| v.get("values")).and_then(|v| v.as_array()) {
        let items: Vec<serde_json::Value> = arr.iter().map(json_attr_to_json_value).collect();
        return serde_json::Value::Array(items);
    }
    if let Some(kvs) = value.get("kvlistValue").and_then(|v| v.get("values")).and_then(|v| v.as_array()) {
        let map: serde_json::Map<String, serde_json::Value> = kvs
            .iter()
            .filter_map(|kv| {
                let key = kv.get("key")?.as_str()?.to_string();
                let val = kv.get("value").map(json_attr_to_json_value)?;
                Some((key, val))
            })
            .collect();
        return serde_json::Value::Object(map);
    }
    serde_json::Value::Null
}

/// Extract attributes array into a HashMap<String, serde_json::Value>.
fn json_attrs_to_map(attrs: Option<&serde_json::Value>) -> HashMap<String, serde_json::Value> {
    match attrs.and_then(|a| a.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|attr| {
                let key = attr.get("key")?.as_str()?.to_string();
                let val = attr.get("value").map(json_attr_to_json_value)?;
                Some((key, val))
            })
            .collect(),
        None => HashMap::new(),
    }
}

/// Parse a hex trace ID string to u128.
fn hex_to_trace_id(hex: &str) -> Option<u128> {
    let val = u128::from_str_radix(hex, 16).ok()?;
    if val == 0 {
        None
    } else {
        Some(val)
    }
}

/// Parse a hex span ID string to u64.
fn hex_to_span_id(hex: &str) -> Option<u64> {
    let val = u64::from_str_radix(hex, 16).ok()?;
    if val == 0 {
        None
    } else {
        Some(val)
    }
}

/// Parse a nanosecond timestamp from a JSON value (string or number).
fn parse_nanos(value: &serde_json::Value) -> u64 {
    if let Some(s) = value.as_str() {
        s.parse::<u64>().unwrap_or(0)
    } else {
        value.as_u64().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Log record parsing
// ---------------------------------------------------------------------------

/// Convert a JSON log record to a LogEntry.
fn parse_json_log_record(
    record: &serde_json::Value,
    service: &str,
    host: &str,
) -> Option<LogEntry> {
    // Extract body — can be a stringValue wrapper or plain string
    let message = record
        .get("body")
        .and_then(|b| {
            // OTLP JSON wraps body in AnyValue: {"stringValue": "..."}
            b.get("stringValue")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| b.as_str().map(|s| s.to_string()))
        })
        .unwrap_or_default();

    if message.is_empty() {
        return None;
    }

    let severity = record
        .get("severityNumber")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    let time_nanos = record
        .get("timeUnixNano")
        .map(parse_nanos)
        .unwrap_or(0);

    let trace_id = record
        .get("traceId")
        .and_then(|v| v.as_str())
        .and_then(hex_to_trace_id);

    let span_id = record
        .get("spanId")
        .and_then(|v| v.as_str())
        .and_then(hex_to_span_id);

    let additional_fields = json_attrs_to_map(record.get("attributes"));

    Some(LogEntry {
        seq: 0,
        timestamp: if time_nanos > 0 {
            nanos_to_datetime(time_nanos)
        } else {
            Utc::now()
        },
        level: severity_to_level(severity),
        message: message.clone(),
        full_message: if message.len() > 200 {
            Some(message)
        } else {
            None
        },
        host: host.to_string(),
        facility: Some(service.to_string()),
        file: None,
        line: None,
        additional_fields,
        trace_id,
        span_id,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    })
}

// ---------------------------------------------------------------------------
// Span parsing
// ---------------------------------------------------------------------------

/// Map JSON span kind integer to SpanKind enum.
fn map_json_span_kind(kind: i64) -> SpanKind {
    match kind {
        1 => SpanKind::Internal,
        2 => SpanKind::Server,
        3 => SpanKind::Client,
        4 => SpanKind::Producer,
        5 => SpanKind::Consumer,
        _ => SpanKind::Unspecified,
    }
}

/// Map JSON status object to SpanStatus enum.
fn map_json_span_status(status: Option<&serde_json::Value>) -> SpanStatus {
    match status {
        Some(s) => {
            let code = s.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            match code {
                0 => SpanStatus::Unset,
                1 => SpanStatus::Ok,
                2 => {
                    let msg = s
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    SpanStatus::Error(msg)
                }
                _ => SpanStatus::Unset,
            }
        }
        None => SpanStatus::Unset,
    }
}

/// Convert JSON span events array to Vec<SpanEvent>.
fn map_json_span_events(events: Option<&serde_json::Value>) -> Vec<SpanEvent> {
    match events.and_then(|e| e.as_array()) {
        Some(arr) => arr
            .iter()
            .map(|e| {
                let name = e
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let time_nanos = e.get("timeUnixNano").map(parse_nanos).unwrap_or(0);
                SpanEvent {
                    name,
                    timestamp: nanos_to_datetime(time_nanos),
                    attributes: json_attrs_to_map(e.get("attributes")),
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Convert a JSON span to a SpanEntry.
fn parse_json_span(span_json: &serde_json::Value, service: &str) -> Option<SpanEntry> {
    let trace_id = span_json
        .get("traceId")
        .and_then(|v| v.as_str())
        .and_then(hex_to_trace_id)?;
    let span_id = span_json
        .get("spanId")
        .and_then(|v| v.as_str())
        .and_then(hex_to_span_id)?;
    let name = span_json
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if name.is_empty() {
        return None;
    }

    let parent_span_id = span_json
        .get("parentSpanId")
        .and_then(|v| v.as_str())
        .and_then(hex_to_span_id);

    let kind = span_json
        .get("kind")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let start_nanos = span_json
        .get("startTimeUnixNano")
        .map(parse_nanos)
        .unwrap_or(0);
    let end_nanos = span_json
        .get("endTimeUnixNano")
        .map(parse_nanos)
        .unwrap_or(0);

    let start = nanos_to_datetime(start_nanos);
    let end = nanos_to_datetime(end_nanos);
    let duration_ms = (end_nanos as f64 - start_nanos as f64) / 1_000_000.0;

    Some(SpanEntry {
        seq: 0,
        trace_id,
        span_id,
        parent_span_id,
        start_time: start,
        end_time: end,
        duration_ms,
        name,
        kind: map_json_span_kind(kind),
        service_name: service.to_string(),
        status: map_json_span_status(span_json.get("status")),
        attributes: json_attrs_to_map(span_json.get("attributes")),
        events: map_json_span_events(span_json.get("events")),
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_logs(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    handle_logs_inner(&state, body).await
}

async fn handle_logs_inner(state: &AppState, body: serde_json::Value) -> StatusCode {
    if channel_used_pct(&state.log_sender) >= BACKPRESSURE_THRESHOLD_PCT {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    if let Some(resource_logs) = body.get("resourceLogs").and_then(|v| v.as_array()) {
        for rl in resource_logs {
            let resource_attrs = extract_json_resource_attrs(rl);
            let (service, host) = extract_resource_attrs(&resource_attrs);

            if let Some(scope_logs) = rl.get("scopeLogs").and_then(|v| v.as_array()) {
                for sl in scope_logs {
                    if let Some(records) = sl.get("logRecords").and_then(|v| v.as_array()) {
                        for record in records {
                            if let Some(entry) = parse_json_log_record(record, &service, &host) {
                                let _ = state.metrics.try_send_log(
                                    &state.log_sender,
                                    entry,
                                    ReceiverSource::OtlpHttpLogs,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    StatusCode::OK
}

async fn handle_traces(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> StatusCode {
    handle_traces_inner(&state, body).await
}

async fn handle_traces_inner(state: &AppState, body: serde_json::Value) -> StatusCode {
    if channel_used_pct(&state.span_sender) >= BACKPRESSURE_THRESHOLD_PCT {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    if let Some(resource_spans) = body.get("resourceSpans").and_then(|v| v.as_array()) {
        for rs in resource_spans {
            let resource_attrs = extract_json_resource_attrs(rs);
            let (service, _host) = extract_resource_attrs(&resource_attrs);

            if let Some(scope_spans) = rs.get("scopeSpans").and_then(|v| v.as_array()) {
                for ss in scope_spans {
                    if let Some(spans) = ss.get("spans").and_then(|v| v.as_array()) {
                        for span_json in spans {
                            if let Some(entry) = parse_json_span(span_json, &service) {
                                let _ = state.metrics.try_send_span(
                                    &state.span_sender,
                                    entry,
                                    ReceiverSource::OtlpHttpTraces,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(log_cap: usize, span_cap: usize) -> AppState {
        let (log_sender, _log_rx) = mpsc::channel(log_cap);
        let (span_sender, _span_rx) = mpsc::channel(span_cap);
        AppState {
            log_sender,
            span_sender,
            metrics: Arc::new(ReceiverMetrics::new()),
        }
    }

    #[test]
    fn channel_used_pct_reads_capacity() {
        let state = make_state(10, 10);
        // Empty channel → 0% used.
        assert_eq!(channel_used_pct(&state.log_sender), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_traces_returns_429_when_span_channel_at_capacity() {
        let state = make_state(10, 1);
        // Fill the span channel.
        let dummy_span = crate::span::types::SpanEntry {
            seq: 0, trace_id: 1, span_id: 1, parent_span_id: None,
            start_time: chrono::Utc::now(), end_time: chrono::Utc::now(),
            duration_ms: 0.0, name: "x".into(),
            kind: crate::span::types::SpanKind::Internal,
            service_name: "s".into(),
            status: crate::span::types::SpanStatus::Unset,
            attributes: std::collections::HashMap::new(),
            events: vec![],
        };
        state.span_sender.try_send(dummy_span).unwrap();

        // POST a single span — channel is full, so the threshold gate
        // returns 429 BEFORE any per-entry try_send_span runs. The
        // counter stays at 0 (the 429 is the backpressure signal).
        let body = serde_json::json!({
            "resourceSpans": [{
                "resource": { "attributes": [] },
                "scopeSpans": [{
                    "spans": [{
                        "traceId": "0102030405060708090a0b0c0d0e0f10",
                        "spanId":  "0102030405060708",
                        "name": "synthetic",
                        "startTimeUnixNano": "0",
                        "endTimeUnixNano":   "0"
                    }]
                }]
            }]
        });

        let status = handle_traces_inner(&state, body).await;
        // 80%+ full → 429 (span channel capacity=1, used=1 → 100%).
        assert_eq!(status.as_u16(), 429);
        // 80%+ full → 429 returned BEFORE any try_send_span runs, so
        // nothing is dropped at the per-entry level. The 429 response IS
        // the backpressure signal; the body is rejected wholesale.
        assert_eq!(state.metrics.snapshot().otlp_http_traces, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_traces_below_threshold_sends_and_returns_200() {
        // Span channel cap=10, so 1 entry = 10% used, well below 80%.
        let state = make_state(10, 10);

        let body = serde_json::json!({
            "resourceSpans": [{
                "resource": { "attributes": [] },
                "scopeSpans": [{
                    "spans": [{
                        "traceId": "0102030405060708090a0b0c0d0e0f10",
                        "spanId":  "0102030405060708",
                        "name": "synthetic",
                        "startTimeUnixNano": "0",
                        "endTimeUnixNano":   "0"
                    }]
                }]
            }]
        });

        let status = handle_traces_inner(&state, body).await;
        assert_eq!(status.as_u16(), 200);
        // Counter unchanged — entry made it through.
        assert_eq!(state.metrics.snapshot().otlp_http_traces, 0);
        // Channel now contains exactly one span.
        assert_eq!(state.span_sender.capacity(), 9, "expected one slot consumed");
    }
}
