use crate::gelf::message::{LogEntry, LogSource};
use crate::receiver::otlp::mapping::*;
use crate::span::types::*;
use opentelemetry_proto::tonic::collector::logs::v1::{
    logs_service_server::{LogsService, LogsServiceServer},
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    trace_service_server::{TraceService, TraceServiceServer},
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{any_value::Value, KeyValue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use chrono::Utc;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an OTLP AnyValue variant to a string representation.
fn format_any_value(v: &Value) -> String {
    match v {
        Value::StringValue(s) => s.clone(),
        Value::IntValue(i) => i.to_string(),
        Value::DoubleValue(d) => d.to_string(),
        Value::BoolValue(b) => b.to_string(),
        Value::BytesValue(b) => b.iter().map(|byte| format!("{:02x}", byte)).collect(),
        Value::ArrayValue(arr) => {
            let items: Vec<String> = arr
                .values
                .iter()
                .filter_map(|av| av.value.as_ref().map(format_any_value))
                .collect();
            format!("[{}]", items.join(", "))
        }
        Value::KvlistValue(kv) => {
            let items: Vec<String> = kv
                .values
                .iter()
                .filter_map(|kv| {
                    let val = kv.value.as_ref()?.value.as_ref()?;
                    Some(format!("{}={}", kv.key, format_any_value(val)))
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Convert an OTLP AnyValue variant to a serde_json::Value.
fn any_value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::StringValue(s) => serde_json::Value::String(s.clone()),
        Value::IntValue(i) => serde_json::json!(i),
        Value::DoubleValue(d) => serde_json::json!(d),
        Value::BoolValue(b) => serde_json::Value::Bool(*b),
        Value::BytesValue(b) => {
            let hex_str: String = b.iter().map(|byte| format!("{:02x}", byte)).collect();
            serde_json::Value::String(hex_str)
        }
        Value::ArrayValue(arr) => {
            let items: Vec<serde_json::Value> = arr
                .values
                .iter()
                .filter_map(|av| av.value.as_ref().map(any_value_to_json))
                .collect();
            serde_json::Value::Array(items)
        }
        Value::KvlistValue(kv) => {
            let map: serde_json::Map<String, serde_json::Value> = kv
                .values
                .iter()
                .filter_map(|kv| {
                    let val = kv.value.as_ref()?.value.as_ref()?;
                    Some((kv.key.clone(), any_value_to_json(val)))
                })
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

/// Convert a slice of OTLP KeyValue attributes to a JSON HashMap.
fn attrs_to_json_map(attrs: &[KeyValue]) -> HashMap<String, serde_json::Value> {
    attrs
        .iter()
        .filter_map(|kv| {
            let val = kv.value.as_ref()?.value.as_ref()?;
            Some((kv.key.clone(), any_value_to_json(val)))
        })
        .collect()
}

/// Map the protobuf span.kind i32 to our SpanKind enum.
fn map_span_kind(kind: i32) -> SpanKind {
    match kind {
        1 => SpanKind::Internal,
        2 => SpanKind::Server,
        3 => SpanKind::Client,
        4 => SpanKind::Producer,
        5 => SpanKind::Consumer,
        _ => SpanKind::Unspecified,
    }
}

/// Map the protobuf Status to our SpanStatus enum.
fn map_span_status(
    status: &Option<opentelemetry_proto::tonic::trace::v1::Status>,
) -> SpanStatus {
    match status {
        Some(s) => match s.code {
            0 => SpanStatus::Unset,
            1 => SpanStatus::Ok,
            2 => SpanStatus::Error(s.message.clone()),
            _ => SpanStatus::Unset,
        },
        None => SpanStatus::Unset,
    }
}

/// Convert protobuf span events to our SpanEvent type.
fn map_span_events(
    events: &[opentelemetry_proto::tonic::trace::v1::span::Event],
) -> Vec<SpanEvent> {
    events
        .iter()
        .map(|e| SpanEvent {
            name: e.name.clone(),
            timestamp: nanos_to_datetime(e.time_unix_nano),
            attributes: attrs_to_json_map(&e.attributes),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// LogsService
// ---------------------------------------------------------------------------

pub struct OtlpLogsService {
    pub log_sender: mpsc::Sender<LogEntry>,
    pub malformed_count: AtomicU64,
}

#[tonic::async_trait]
impl LogsService for OtlpLogsService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let req = request.into_inner();

        for resource_logs in req.resource_logs {
            let resource_attrs = resource_logs
                .resource
                .as_ref()
                .map(|r| key_values_to_pairs(&r.attributes))
                .unwrap_or_default();
            let (service_name, host_name) = extract_resource_attrs(&resource_attrs);

            for scope_logs in resource_logs.scope_logs {
                for log_record in scope_logs.log_records {
                    let message = log_record
                        .body
                        .as_ref()
                        .and_then(|v| v.value.as_ref())
                        .map(format_any_value)
                        .unwrap_or_default();

                    if message.is_empty() {
                        self.malformed_count.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let additional_fields = attrs_to_json_map(&log_record.attributes);

                    let entry = LogEntry {
                        seq: 0, // assigned downstream by pipeline
                        timestamp: if log_record.time_unix_nano > 0 {
                            nanos_to_datetime(log_record.time_unix_nano)
                        } else {
                            Utc::now()
                        },
                        level: severity_to_level(log_record.severity_number),
                        message: message.clone(),
                        full_message: if message.len() > 200 {
                            Some(message)
                        } else {
                            None
                        },
                        host: host_name.clone(),
                        facility: Some(service_name.clone()),
                        file: None,
                        line: None,
                        additional_fields,
                        trace_id: bytes_to_trace_id(&log_record.trace_id),
                        span_id: bytes_to_span_id(&log_record.span_id),
                        matched_filters: Vec::new(),
                        source: LogSource::Filter,
                    };

                    let _ = self.log_sender.send(entry).await;
                }
            }
        }

        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// TraceService
// ---------------------------------------------------------------------------

pub struct OtlpTraceService {
    pub span_sender: mpsc::Sender<SpanEntry>,
    pub malformed_count: AtomicU64,
}

#[tonic::async_trait]
impl TraceService for OtlpTraceService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let req = request.into_inner();

        for resource_spans in req.resource_spans {
            let resource_attrs = resource_spans
                .resource
                .as_ref()
                .map(|r| key_values_to_pairs(&r.attributes))
                .unwrap_or_default();
            let (service_name, _host) = extract_resource_attrs(&resource_attrs);

            for scope_spans in resource_spans.scope_spans {
                for span in scope_spans.spans {
                    let trace_id = match bytes_to_trace_id(&span.trace_id) {
                        Some(id) => id,
                        None => {
                            self.malformed_count.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    let span_id = match bytes_to_span_id(&span.span_id) {
                        Some(id) => id,
                        None => {
                            self.malformed_count.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    if span.name.is_empty() {
                        self.malformed_count.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }

                    let start = nanos_to_datetime(span.start_time_unix_nano);
                    let end = nanos_to_datetime(span.end_time_unix_nano);
                    let duration_ms = (span.end_time_unix_nano as f64
                        - span.start_time_unix_nano as f64)
                        / 1_000_000.0;

                    let entry = SpanEntry {
                        seq: 0, // assigned downstream by span store
                        trace_id,
                        span_id,
                        parent_span_id: bytes_to_span_id(&span.parent_span_id),
                        start_time: start,
                        end_time: end,
                        duration_ms,
                        name: span.name,
                        kind: map_span_kind(span.kind),
                        service_name: service_name.clone(),
                        status: map_span_status(&span.status),
                        attributes: attrs_to_json_map(&span.attributes),
                        events: map_span_events(&span.events),
                    };

                    let _ = self.span_sender.send(entry).await;
                }
            }
        }

        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// Server startup
// ---------------------------------------------------------------------------

/// Start the OTLP gRPC server serving both LogsService and TraceService.
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    log_sender: mpsc::Sender<LogEntry>,
    span_sender: mpsc::Sender<SpanEntry>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let logs_svc = OtlpLogsService {
        log_sender,
        malformed_count: AtomicU64::new(0),
    };
    let trace_svc = OtlpTraceService {
        span_sender,
        malformed_count: AtomicU64::new(0),
    };

    tracing::info!("OTLP gRPC server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(LogsServiceServer::new(logs_svc))
        .add_service(TraceServiceServer::new(trace_svc))
        .serve_with_shutdown(addr, async move {
            let _ = shutdown_rx.recv().await;
        })
        .await?;

    Ok(())
}
