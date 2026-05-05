//! `triggers` subcommand group: add, list, edit, remove.
//!
//! Note: triggers added via CLI persist in the session but the CLI invocation
//! exits before any matching log can fire the trigger. Use CLI for trigger
//! management; subscribe to fires via the long-running MCP shim or a custom
//! SDK consumer.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{TriggersAdd, TriggersEdit, TriggersList, TriggersRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct TriggersCmd {
    #[command(subcommand)]
    verb: TrgVerb,
}

#[derive(Subcommand, Debug)]
enum TrgVerb {
    /// Add a trigger. The CLI invocation exits before fires arrive — use the
    /// MCP shim to subscribe to firings.
    Add {
        #[arg(long)]
        filter: String,
        #[arg(long, default_value_t = 0)]
        pre_window: u32,
        #[arg(long, default_value_t = 0)]
        post_window: u32,
        #[arg(long, default_value_t = 0)]
        notify_context: u32,
        #[arg(long)]
        description: Option<String>,
        /// Auto-remove after the first fire.
        #[arg(long)]
        oneshot: bool,
    },
    List,
    Edit {
        #[arg(long)]
        id: u32,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        pre_window: Option<u32>,
        #[arg(long)]
        post_window: Option<u32>,
        #[arg(long)]
        notify_context: Option<u32>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        oneshot: Option<bool>,
    },
    Remove {
        #[arg(long)]
        id: u32,
    },
}

pub async fn dispatch(broker: &Broker, cmd: TriggersCmd, json: bool) -> i32 {
    match cmd.verb {
        TrgVerb::Add { filter, pre_window, post_window, notify_context, description, oneshot } => {
            let params = TriggersAdd {
                filter,
                pre_window: Some(pre_window),
                post_window: Some(post_window),
                notify_context: Some(notify_context),
                description,
                oneshot,
            };
            let result = match broker.triggers_add(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.add failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trigger added: id={} oneshot={oneshot}", result.id);
            0
        }
        TrgVerb::List => {
            let result = match broker.triggers_list(TriggersList {}).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.list failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.triggers.is_empty() { println!("(no triggers)"); return 0; }
            let rows: Vec<Vec<String>> = result.triggers.iter().map(|t| vec![
                t.id.to_string(),
                t.filter.clone(),
                format!("{}/{}/{}", t.pre_window, t.post_window, t.notify_context),
                t.match_count.to_string(),
                t.oneshot.to_string(),
                t.description.clone().unwrap_or_default(),
            ]).collect();
            format::print_table(&["id", "filter", "pre/post/notify", "fired", "oneshot", "description"], rows);
            0
        }
        TrgVerb::Edit { id, filter, pre_window, post_window, notify_context, description, oneshot } => {
            let params = TriggersEdit { id, filter, pre_window, post_window, notify_context, description, oneshot };
            let result = match broker.triggers_edit(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.edit failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trigger edited: id={}", result.id);
            0
        }
        TrgVerb::Remove { id } => {
            let result = match broker.triggers_remove(TriggersRemove { id }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("triggers.remove failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("removed: id={}", result.removed);
            0
        }
    }
}
