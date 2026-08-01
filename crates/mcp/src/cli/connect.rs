//! CLI-tuned broker connection helper.
//!
//! - reconnect_max_attempts(0): no retry on EOF (CLI is one-shot).
//! - call_timeout(5s): fail fast.
//! - error message guides the user to start the broker as a service.

use anyhow::Result;
use logmon_broker_sdk::Broker;
use std::time::Duration;

/// Open the CLI's connection, reporting what invoked it.
///
/// `argv` is the command path and its arguments. **Only the leading path
/// segments are sent** as `client_info`: an argument value can carry a filter
/// expression, a description or a bookmark name, and none of those belongs in a
/// diagnostic field that other sessions can read.
///
/// This used to map a clap enum to a group name, which meant a new command
/// needed a new arm here. The path is now derived from the command the user
/// typed, so there is nothing to keep in step.
pub async fn connect_cli(session: &str, argv: &[String]) -> Result<Broker> {
    let path: Vec<&str> = argv
        .iter()
        .take_while(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();

    let client_info = serde_json::json!({
        "name": "logmon-mcp",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "cli",
        "argv": path,
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

#[cfg(test)]
mod tests {
    /// A value must never reach client_info. `--filter` can carry anything the
    /// user was searching for, and client_info is visible to other sessions.
    #[test]
    fn only_the_command_path_is_reported() {
        let argv: Vec<String> = ["logs", "recent", "--filter", "secret-customer-id"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let path: Vec<&str> = argv
            .iter()
            .take_while(|a| !a.starts_with('-'))
            .map(String::as_str)
            .collect();
        assert_eq!(path, ["logs", "recent"]);
    }
}
