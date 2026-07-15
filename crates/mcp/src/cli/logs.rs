//! `logs` subcommand group: recent, context, export, clear.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{LogEntry, LogsClear, LogsContext, LogsExport, LogsRecent};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct LogsCmd {
    #[command(subcommand)]
    verb: LogsVerb,
}

#[derive(Subcommand, Debug)]
enum LogsVerb {
    /// Fetch recent log entries (newest first by default; oldest first when filter contains c>=).
    Recent {
        #[arg(long, default_value_t = 50)]
        count: u64,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long, value_name = "TRACE_ID_HEX")]
        trace_id: Option<String>,
    },
    /// Fetch logs surrounding a specific seq.
    Context {
        #[arg(long)]
        seq: u64,
        #[arg(long, default_value_t = 10)]
        before: u64,
        #[arg(long, default_value_t = 10)]
        after: u64,
    },
    /// Export matching logs (returns the same shape as recent + a format field).
    Export {
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        filter: Option<String>,
        /// Write output to FILE instead of stdout. Use `-` for stdout. Existing files are overwritten.
        #[arg(long, value_name = "FILE")]
        out: Option<String>,
    },
    /// Clear the entire log buffer (affects all sessions).
    Clear,
}

pub async fn dispatch(broker: &Broker, cmd: LogsCmd, json: bool) -> i32 {
    match cmd.verb {
        LogsVerb::Recent {
            count,
            filter,
            trace_id,
        } => {
            let params = LogsRecent {
                count: Some(count),
                filter,
                trace_id,
            };
            let result = match broker.logs_recent(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("logs.recent failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
            format::print_blocks(blocks, "(no logs)");
            print_query_diagnostics(
                result.truncated,
                result.evicted_before_window,
                result.logs.is_empty(),
                result.scanned,
            );
            if let Some(seq) = result.cursor_advanced_to {
                println!("\ncursor advanced to seq={seq}");
            }
            0
        }
        LogsVerb::Context { seq, before, after } => {
            let params = LogsContext {
                seq,
                before: Some(before),
                after: Some(after),
            };
            let result = match broker.logs_context(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("logs.context failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
            format::print_blocks(blocks, "(no logs)");
            0
        }
        LogsVerb::Export { count, filter, out } => {
            let params = LogsExport { count, filter };
            let result = match broker.logs_export(params).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("logs.export failed: {e}"), json);
                    return 1;
                }
            };
            if !json {
                print_query_diagnostics(
                    result.truncated,
                    result.evicted_before_window,
                    result.logs.is_empty(),
                    result.scanned,
                );
            }

            // Render the output payload first, then redirect to file or stdout.
            let payload = if json {
                format::json_string(&result)
            } else {
                let blocks: Vec<String> = result.logs.iter().map(format_entry).collect();
                let rendered = format::truncate_blocks(
                    blocks,
                    format::DEFAULT_MAX_BLOCK_RECORDS,
                    format::DEFAULT_MAX_BLOCK_BYTES,
                );
                format!("{rendered}\n")
            };

            match out.as_deref() {
                Some("-") | None => print!("{payload}"),
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &payload) {
                        format::error(&format!("failed to write {path}: {e}"), json);
                        return 1;
                    }
                    if !json {
                        eprintln!("wrote {} log entries to {path}", result.count);
                    }
                }
            }
            0
        }
        LogsVerb::Clear => {
            let result = match broker.logs_clear(LogsClear {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("logs.clear failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!("cleared {} log entries", result.cleared);
            0
        }
    }
}

/// Render a single log entry as a two-line block.
fn format_entry(e: &LogEntry) -> String {
    let header = format!(
        "[seq={}] {} {} {}: {}",
        e.seq,
        e.timestamp.to_rfc3339(),
        format::format_level(&e.level),
        e.facility.as_deref().unwrap_or("-"),
        e.message,
    );
    let mut secondary = Vec::new();
    if let Some(file) = &e.file {
        if let Some(line) = e.line {
            secondary.push(format!("file={file}:{line}"));
        } else {
            secondary.push(format!("file={file}"));
        }
    }
    if !e.host.is_empty() {
        secondary.push(format!("host={}", e.host));
    }
    if let Some(tid) = &e.trace_id {
        secondary.push(format!("trace={tid}"));
    }
    if let Some(sid) = &e.span_id {
        secondary.push(format!("span={sid}"));
    }
    if secondary.is_empty() {
        header
    } else {
        format!("{header}\n  {}", secondary.join(" "))
    }
}

/// Print B2/B5 query diagnostics to stderr (so piped stdout stays clean): a
/// truncation warning when a bookmark/cursor window rolled off the buffer, and a
/// "matched nothing but data is flowing" note that distinguishes a bad filter
/// from an empty buffer.
fn print_query_diagnostics(truncated: bool, evicted: Option<u64>, empty: bool, scanned: usize) {
    if truncated {
        let n = evicted
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        eprintln!(
            "warning: window truncated — ~{n} record(s) rolled off before the oldest retained log; results are incomplete"
        );
    }
    if empty && scanned > 0 {
        eprintln!(
            "note: filter matched 0 of {scanned} scanned records — data is flowing, so check the filter"
        );
    }
}
