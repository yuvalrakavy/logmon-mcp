//! `sessions` subcommand group: list, drop.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{SessionDrop, SessionList};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct SessionsCmd {
    #[command(subcommand)]
    verb: SesVerb,
}

#[derive(Subcommand, Debug)]
enum SesVerb {
    List,
    Drop {
        #[arg(long)]
        name: String,
    },
}

pub async fn dispatch(broker: &Broker, cmd: SessionsCmd, json: bool) -> i32 {
    match cmd.verb {
        SesVerb::List => {
            let result = match broker.session_list(SessionList {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("session.list failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.sessions.is_empty() {
                println!("(no sessions)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .sessions
                .iter()
                .map(|s| {
                    vec![
                        s.id.clone(),
                        s.name.clone().unwrap_or_else(|| "(anonymous)".into()),
                        s.connected.to_string(),
                        s.trigger_count.to_string(),
                        s.filter_count.to_string(),
                        s.queue_size.to_string(),
                        format!("{}s", s.last_seen_secs_ago),
                    ]
                })
                .collect();
            format::print_table(
                &[
                    "id",
                    "name",
                    "connected",
                    "triggers",
                    "filters",
                    "queue",
                    "last_seen",
                ],
                rows,
            );
            0
        }
        SesVerb::Drop { name } => {
            let result = match broker.session_drop(SessionDrop { name }).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("session.drop failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("dropped: {}", result.dropped);
            0
        }
    }
}
