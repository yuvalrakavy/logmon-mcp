//! CLI mode for `logmon-mcp`. Routed when `Cli::command` is `Some(_)`.
//!
//! Each subcommand group lives in its own module and exposes:
//! - A `clap::Args`-style struct (e.g. `LogsCmd`) used by `main.rs`.
//! - An `async fn dispatch(broker, cmd, json) -> i32` that runs the command
//!   and returns the desired process exit code.

pub mod bookmarks;
pub mod connect;
pub mod filters;
pub mod format;
pub mod logs;
pub mod sessions;
pub mod spans;
pub mod status;
pub mod traces;
pub mod triggers;

use crate::Subcommand;

/// Top-level CLI dispatch. Returns the process exit code.
pub async fn dispatch(cmd: Subcommand, session: Option<String>, json: bool) -> i32 {
    let session_name = session.unwrap_or_else(|| "cli".to_string());

    let broker = match connect::connect_cli(&session_name, &cmd).await {
        Ok(b) => b,
        Err(e) => {
            format::error(&e.to_string(), json);
            return 1;
        }
    };

    match cmd {
        Subcommand::Logs(c) => logs::dispatch(&broker, c, json).await,
        Subcommand::Bookmarks(c) => bookmarks::dispatch(&broker, c, json).await,
        Subcommand::Triggers(c) => triggers::dispatch(&broker, c, json).await,
        Subcommand::Filters(c) => filters::dispatch(&broker, c, json).await,
        Subcommand::Traces(c) => traces::dispatch(&broker, c, json).await,
        Subcommand::Spans(c) => spans::dispatch(&broker, c, json).await,
        Subcommand::Sessions(c) => sessions::dispatch(&broker, c, json).await,
        Subcommand::Status => status::dispatch(&broker, json).await,
    }
}
