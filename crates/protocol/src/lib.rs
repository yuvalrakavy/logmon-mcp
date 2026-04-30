pub mod methods;
pub mod notifications;

pub use methods::*;
pub use notifications::*;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

/// JSON-RPC 2.0 success response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 notification (no id, no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

/// Envelope: either a response or a notification from daemon.
/// Do NOT use serde untagged — it's fragile. Use parse_daemon_message_from_str instead.
#[derive(Debug, Clone)]
pub enum DaemonMessage {
    Response(RpcResponse),
    Notification(RpcNotification),
}

/// Parse a daemon message by checking for `id` field presence.
pub fn parse_daemon_message_from_str(line: &str) -> anyhow::Result<DaemonMessage> {
    let v: serde_json::Value = serde_json::from_str(line)?;
    if v.get("id").is_some() {
        Ok(DaemonMessage::Response(serde_json::from_value(v)?))
    } else {
        Ok(DaemonMessage::Notification(serde_json::from_value(v)?))
    }
}

/// session.start parameters
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionStartParams {
    pub name: Option<String>,
    pub protocol_version: u32,
}

/// session.start response
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionStartResult {
    pub session_id: String,
    pub is_new: bool,
    pub queued_notifications: usize,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub daemon_uptime_secs: u64,
    pub buffer_size: usize,
    pub receivers: Vec<String>,
}

impl RpcRequest {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

impl RpcResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
        }
    }
}

impl RpcNotification {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            method: method.to_string(),
            params,
        }
    }
}
