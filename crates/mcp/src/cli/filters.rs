//! `filters` subcommand group: add, list, edit, remove.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{FiltersAdd, FiltersEdit, FiltersList, FiltersRemove};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct FiltersCmd {
    #[command(subcommand)]
    verb: FltVerb,
}

#[derive(Subcommand, Debug)]
enum FltVerb {
    Add {
        #[arg(long)]
        filter: String,
        #[arg(long)]
        description: Option<String>,
    },
    List,
    Edit {
        #[arg(long)]
        id: u32,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    Remove {
        #[arg(long)]
        id: u32,
    },
}

pub async fn dispatch(broker: &Broker, cmd: FiltersCmd, json: bool) -> i32 {
    match cmd.verb {
        FltVerb::Add {
            filter,
            description,
        } => {
            let result = match broker
                .filters_add(FiltersAdd {
                    filter,
                    description,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("filters.add failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("filter added: id={}", result.id);
            0
        }
        FltVerb::List => {
            let result = match broker.filters_list(FiltersList {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("filters.list failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.filters.is_empty() {
                println!("(no filters)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .filters
                .iter()
                .map(|f| {
                    vec![
                        f.id.to_string(),
                        f.filter.clone(),
                        f.description.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            format::print_table(&["id", "filter", "description"], rows);
            0
        }
        FltVerb::Edit {
            id,
            filter,
            description,
        } => {
            let result = match broker
                .filters_edit(FiltersEdit {
                    id,
                    filter,
                    description,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("filters.edit failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("filter edited: id={}", result.id);
            0
        }
        FltVerb::Remove { id } => {
            let result = match broker.filters_remove(FiltersRemove { id }).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("filters.remove failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("removed: id={}", result.removed);
            0
        }
    }
}
