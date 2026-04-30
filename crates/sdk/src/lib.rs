//! `logmon-broker-sdk` — async client for the logmon broker daemon.
//!
//! The SDK owns the wire-protocol transport (length-delimited JSON-RPC
//! framing), the `DaemonBridge` request/response router, and the
//! [`Broker`]/[`BrokerBuilder`] connect surface used by both the MCP shim
//! and downstream embedders.

pub mod bridge;
pub mod connect;
pub mod transport;

pub use connect::{Broker, BrokerBuilder};
pub use logmon_broker_protocol::{
    RpcNotification, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};

/// Errors surfaced by the SDK to its callers.
///
/// [`bridge::BridgeError`] is the lower-level type produced by the JSON-RPC
/// router; the SDK converts those into [`BrokerError`] before returning them.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("transport: {0}")]
    Transport(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("rpc error {code}: {message}")]
    Method { code: i32, message: String },
    #[error("disconnected")]
    Disconnected,
    #[error("session lost")]
    SessionLost,
}
