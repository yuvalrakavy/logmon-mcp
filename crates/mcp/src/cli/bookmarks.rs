//! `bookmarks` subcommand group: add, list, remove, clear.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{BookmarksAdd, BookmarksClear, BookmarksList, BookmarksRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct BookmarksCmd {
    #[command(subcommand)]
    verb: BkmVerb,
}

#[derive(Subcommand, Debug)]
enum BkmVerb {
    /// Add a bookmark. Default start_seq is the current seq counter; pass --start-seq to override.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        start_seq: Option<u64>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    /// List all bookmarks (optionally filter by session).
    List {
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove a bookmark by qualified name (`session/name`).
    Remove {
        #[arg(long)]
        name: String,
    },
    /// Clear bookmarks for a session (default = current session).
    Clear {
        #[arg(long)]
        session: Option<String>,
    },
}

pub async fn dispatch(broker: &Broker, cmd: BookmarksCmd, json: bool) -> i32 {
    match cmd.verb {
        BkmVerb::Add {
            name,
            start_seq,
            description,
            replace,
        } => {
            let params = BookmarksAdd {
                name,
                description,
                start_seq,
                replace,
            };
            let result = match broker.bookmarks_add(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("bookmarks.add failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            let action = if result.replaced { "replaced" } else { "added" };
            println!(
                "bookmark {action}: {} (seq={})",
                result.qualified_name, result.seq
            );
            0
        }
        BkmVerb::List { session } => {
            let params = BookmarksList { session };
            let result = match broker.bookmarks_list(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("bookmarks.list failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.bookmarks.is_empty() {
                println!("(no bookmarks)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .bookmarks
                .iter()
                .map(|b| {
                    vec![
                        b.qualified_name.clone(),
                        b.seq.to_string(),
                        b.created_at.to_rfc3339(),
                        b.description.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            format::print_table(&["name", "seq", "created_at", "description"], rows);
            0
        }
        BkmVerb::Remove { name } => {
            let params = BookmarksRemove { name };
            let result = match broker.bookmarks_remove(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("bookmarks.remove failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("removed: {}", result.removed);
            0
        }
        BkmVerb::Clear { session } => {
            let params = BookmarksClear { session };
            let result = match broker.bookmarks_clear(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("bookmarks.clear failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "cleared {} bookmark(s) for session {}",
                result.removed_count, result.session
            );
            0
        }
    }
}
