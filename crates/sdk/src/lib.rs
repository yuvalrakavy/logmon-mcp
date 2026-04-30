//! `logmon-broker-sdk` — async client for the logmon broker daemon.
//!
//! The SDK owns the wire-protocol transport (length-delimited JSON-RPC
//! framing), the `DaemonBridge` request/response router, and the
//! [`Broker`]/[`BrokerBuilder`] connect surface used by both the MCP shim
//! and downstream embedders.

pub mod bridge;
pub mod connect;
pub mod filter;
mod methods;
pub mod transport;

pub use filter::{Filter, FilterBuilder, FilterSpanKind, FilterSpanStatus, Level};

pub use connect::{Broker, BrokerBuilder};
pub use logmon_broker_protocol::{
    RpcNotification, RpcRequest, RpcResponse, TriggerFiredPayload, PROTOCOL_VERSION,
};

/// Server-pushed notifications surfaced to SDK callers via
/// [`Broker::subscribe_notifications`].
///
/// The bridge converts wire-level [`RpcNotification`] frames into this typed
/// enum before broadcasting; unparseable frames are logged and dropped, so
/// subscribers only see well-formed events.
///
/// `#[non_exhaustive]` lets future variants ship without a major-version
/// bump — match arms must include a wildcard.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Notification {
    /// A registered trigger matched a pipeline event. Payload mirrors the
    /// `notifications/trigger_fired` JSON-RPC notification.
    TriggerFired(TriggerFiredPayload),

    /// The bridge re-established its session after a transient disconnect.
    /// Defined here so subscribers can pattern-match without conditional
    /// compilation; emission lands with the reconnect state machine in
    /// Task 16. Until then this variant is unreachable.
    Reconnected,
}

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
