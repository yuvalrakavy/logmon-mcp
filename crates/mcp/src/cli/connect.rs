//! CLI-tuned broker connection helper.
//!
//! - reconnect_max_attempts(0): no retry on EOF (CLI is one-shot).
//! - call_timeout(5s): fail fast.
//! - error message guides the user to start the broker as a service.

use anyhow::Result;
use logmon_broker_sdk::Broker;
use std::time::Duration;

use crate::Subcommand;

pub async fn connect_cli(session: &str, cmd: &Subcommand) -> Result<Broker> {
    let argv = subcommand_argv(cmd);
    let client_info = serde_json::json!({
        "name": "logmon-mcp",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "cli",
        "argv": argv,
    });

    Broker::connect()
        .session_name(session)
        .client_info(client_info)
        .reconnect_max_attempts(0)
        .call_timeout(Duration::from_secs(5))
        .open()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "broker not running\n\nStart the broker with one of:\n  logmon-broker install-service --scope user   (recommended; daemonized via launchd/systemd)\n  logmon-broker                                 (foreground, for debugging)\n\nUnderlying error: {e}"
            )
        })
}

/// Extract the subcommand group for client_info. Group only — verb is
/// intentionally omitted per the spec (extracting the verb requires
/// pattern-matching the inner Cmd enum at the connect site, adds coupling
/// for diagnostic value that isn't load-bearing for any feature).
fn subcommand_argv(cmd: &Subcommand) -> Vec<&'static str> {
    match cmd {
        Subcommand::Logs(_) => vec!["logs"],
        Subcommand::Bookmarks(_) => vec!["bookmarks"],
        Subcommand::Triggers(_) => vec!["triggers"],
        Subcommand::Filters(_) => vec!["filters"],
        Subcommand::Traces(_) => vec!["traces"],
        Subcommand::Spans(_) => vec!["spans"],
        Subcommand::Sessions(_) => vec!["sessions"],
        Subcommand::Status => vec!["status"],
    }
}
