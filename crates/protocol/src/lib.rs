pub mod mcp_tools;
pub mod methods;
pub mod notifications;

pub use methods::*;
pub use notifications::*;
// Deliberately NOT re-exported at the crate root: `mcp_tools` describes one
// front-end's surface, not the wire protocol, and a future second front-end
// should have to name it rather than find it among the protocol types.

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
    /// Asks the daemon to add a rendered `_display` string to the result.
    ///
    /// **On the envelope rather than in `params`, because it is not a parameter
    /// of any tool.** In `params` it would behave three different ways: an MCP
    /// client validating the advertised `input_schema` rejects it before the
    /// call (50 definitions carry `additionalProperties: false`), four typed
    /// handlers hard-error on it, and the other 45 read fields one at a time and
    /// would silently ignore it — a caller asking for a rendering, getting none,
    /// and told nothing. Making it legal would mean publishing `_display` as a
    /// parameter of every tool in the manifest.
    ///
    /// **Version skew needs no capability.** `default` covers an old client; an
    /// old broker ignores it, because `RpcRequest` has no `deny_unknown_fields`.
    /// Both directions degrade to JSON, which is the same fallback a method
    /// without a renderer takes.
    ///
    /// `skip_serializing_if` so the common case adds nothing to the wire.
    #[serde(default, skip_serializing_if = "is_false")]
    pub display: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Caller-supplied identity/metadata. The daemon currently records and ignores
    /// this field; size validation lands in Task 11.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Value>,
    /// Optional connect-time domain bind: an atomic equivalent of calling
    /// `domains.use` immediately after connecting. Errors the handshake if the
    /// named domain does not exist. Omitted → the `default` domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
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
    /// Capability strings advertised by the daemon. Lets SDKs feature-gate
    /// without sniffing protocol versions. Always serialized (no
    /// `skip_serializing_if`) so callers can rely on the field being present.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl RpcRequest {
    /// A request that wants the raw result. **The default, deliberately** — an
    /// SDK consumer deserializing into a typed struct would discard `_display`,
    /// so it should not pay for one.
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
            display: false,
        }
    }

    /// The same request, asking for a rendered `_display` alongside the result.
    pub fn rendered(self) -> Self {
        Self {
            display: true,
            ..self
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

#[cfg(test)]
mod display_flag_tests {
    use super::*;

    /// The default is the whole of the compatibility story, so it is pinned
    /// directly rather than inferred from a call path.
    #[test]
    fn a_request_asks_for_no_rendering_unless_told_to() {
        let plain = RpcRequest::new(1, "logs.recent", serde_json::json!({}));
        assert!(!plain.display);
        assert!(plain.clone().rendered().display);
        // `rendered()` changes nothing else.
        let r = plain.clone().rendered();
        assert_eq!((r.id, r.method, r.params), (plain.id, plain.method, plain.params));
    }

    /// An old broker must not see a field it does not know, and a caller reading
    /// its own outbound JSON must not see noise: every request that does not
    /// want rendering is byte-identical to what shipped before this field.
    #[test]
    fn the_flag_is_absent_from_the_wire_unless_it_is_set() {
        let plain = serde_json::to_value(RpcRequest::new(7, "status.get", Value::Null)).unwrap();
        assert!(
            plain.get("display").is_none(),
            "a default request grew a key: {plain}"
        );
        let asked =
            serde_json::to_value(RpcRequest::new(7, "status.get", Value::Null).rendered()).unwrap();
        assert_eq!(asked["display"], serde_json::json!(true));
    }

    /// The other direction of skew: a client too old to know the flag sends no
    /// such key, and a new broker must read that as "no rendering" rather than
    /// failing to parse. `RpcRequest` deliberately does not deny unknown fields,
    /// which is what makes the reverse direction work too.
    #[test]
    fn a_request_from_before_this_field_still_parses() {
        let old = r#"{"jsonrpc":"2.0","id":3,"method":"logs.recent","params":{}}"#;
        let parsed: RpcRequest = serde_json::from_str(old).expect("an old client's request");
        assert!(!parsed.display);

        // And a request from a NEWER client, carrying a field this build does
        // not know, is not a parse error either.
        let newer = r#"{"jsonrpc":"2.0","id":3,"method":"logs.recent","params":{},
                        "display":true,"some_future_flag":9}"#;
        let parsed: RpcRequest = serde_json::from_str(newer).expect("a newer client's request");
        assert!(parsed.display);
    }
}
