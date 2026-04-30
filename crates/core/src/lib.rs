pub mod engine;
pub mod filter;
pub mod gelf;
pub mod receiver;
pub mod span;
pub mod store;
pub mod daemon {
    pub mod log_processor;
    pub mod persistence;
    pub mod rpc_handler;
    pub mod server;
    pub mod session;
    pub mod span_processor;
    pub mod transport;
}

#[cfg(feature = "test-support")]
pub mod test_support;

// Programmatic entry point used by integration tests in the SDK crate.
// Spins up an in-process daemon listening on a caller-provided socket.
pub use daemon::persistence::DaemonConfig;

#[cfg(feature = "test-support")]
pub use test_support::{spawn_test_daemon, TestClient, TestDaemonHandle};
