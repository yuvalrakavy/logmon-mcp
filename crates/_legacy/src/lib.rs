// All engine/filter/store/etc. now live in logmon-broker-core; re-export at
// the same paths so internal `use crate::engine::pipeline::...` calls inside
// _legacy/src/ keep resolving.
pub use logmon_broker_core::engine;
pub use logmon_broker_core::filter;
pub use logmon_broker_core::gelf;
pub use logmon_broker_core::receiver;
pub use logmon_broker_core::span;
pub use logmon_broker_core::store;

pub mod rpc {
    pub use logmon_broker_protocol::*;
    pub mod types {
        pub use logmon_broker_protocol::*;
    }
    pub mod transport;  // still in _legacy/src/rpc/transport.rs until Task 5
}

pub mod config;         // still in _legacy/src/config.rs until Task 4
pub mod shim;           // still in _legacy/src/shim/ until Task 5
pub mod mcp;            // still in _legacy/src/mcp/ until Task 5

pub mod daemon {
    pub use logmon_broker_core::daemon::log_processor;
    pub use logmon_broker_core::daemon::span_processor;
    pub use logmon_broker_core::daemon::persistence;
    pub use logmon_broker_core::daemon::session;
    pub use logmon_broker_core::daemon::rpc_handler;
    pub mod server;     // still in _legacy/src/daemon/server.rs until Task 4
}
