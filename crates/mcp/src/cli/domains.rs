//! `domains` subcommand group: create, delete, list, clear.
//!
//! Wraps the daemon's `domains.*` RPCs. Ephemeral-only for now — durable
//! (`persist`) domains arrive with the durability stage. There is deliberately
//! no `use` verb: a CLI session is short-lived, so a sticky bind would not
//! survive the invocation. Use the global `--domain <name>` flag to bind a
//! single invocation (e.g. `logmon --domain t3 logs recent`, or
//! `logmon --domain t3 domains clear`).

use clap::{Args, Subcommand};
use logmon_broker_protocol::{DomainsClear, DomainsCreate, DomainsDelete, DomainsList};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct DomainsCmd {
    #[command(subcommand)]
    verb: DomVerb,
}

#[derive(Subcommand, Debug)]
enum DomVerb {
    /// Create (or idempotently ensure) an ephemeral domain. Omitted ports are
    /// auto-allocated; a port of 0 disables that receiver.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        gelf_port: Option<u16>,
        #[arg(long)]
        otlp_grpc_port: Option<u16>,
        #[arg(long)]
        otlp_http_port: Option<u16>,
        #[arg(long)]
        log_buffer_size: Option<usize>,
        #[arg(long)]
        span_buffer_size: Option<usize>,
    },
    /// Delete an ephemeral domain (refuses config-declared domains incl. default).
    Delete {
        #[arg(long)]
        name: String,
    },
    /// List all live domains.
    List,
    /// Dispose the bound domain's data (logs + spans); keeps the domain alive.
    /// Target a non-default domain with the global `--domain` flag.
    Clear,
}

pub async fn dispatch(broker: &Broker, cmd: DomainsCmd, json: bool) -> i32 {
    match cmd.verb {
        DomVerb::Create {
            name,
            gelf_port,
            otlp_grpc_port,
            otlp_http_port,
            log_buffer_size,
            span_buffer_size,
        } => {
            let result = match broker
                .domains_create(DomainsCreate {
                    name,
                    gelf_port,
                    otlp_grpc_port,
                    otlp_http_port,
                    log_buffer_size,
                    span_buffer_size,
                    persist: false,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("domains.create failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "created: {} (gelf={} otlp_grpc={} otlp_http={} source={})",
                result.name,
                result.gelf_port,
                result.otlp_grpc_port,
                result.otlp_http_port,
                result.source,
            );
            0
        }
        DomVerb::Delete { name } => {
            let result = match broker.domains_delete(DomainsDelete { name }).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("domains.delete failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("deleted: {} ({})", result.name, result.deleted);
            0
        }
        DomVerb::List => {
            let result = match broker.domains_list(DomainsList {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("domains.list failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.domains.is_empty() {
                println!("(no domains)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .domains
                .iter()
                .map(|d| {
                    vec![
                        d.name.clone(),
                        d.source.clone(),
                        d.gelf_port.to_string(),
                        d.otlp_grpc_port.to_string(),
                        d.otlp_http_port.to_string(),
                        d.log_count.to_string(),
                        d.span_count.to_string(),
                    ]
                })
                .collect();
            format::print_table(
                &[
                    "name",
                    "source",
                    "gelf",
                    "otlp_grpc",
                    "otlp_http",
                    "logs",
                    "spans",
                ],
                rows,
            );
            0
        }
        DomVerb::Clear => {
            let result = match broker.domains_clear(DomainsClear {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("domains.clear failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "cleared: {} logs, {} spans",
                result.logs_cleared, result.spans_cleared
            );
            0
        }
    }
}
