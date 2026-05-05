//! `traces` subcommand group: recent, get, summary, slow, logs.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{
    LogEntry, SpanEntry, TraceSummary, TracesGet, TracesLogs, TracesRecent,
    TracesSlow, TracesSummary,
};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct TracesCmd {
    #[command(subcommand)]
    verb: TrcVerb,
}

#[derive(Subcommand, Debug)]
enum TrcVerb {
    Recent {
        #[arg(long, default_value_t = 20)]
        count: u64,
        #[arg(long)]
        filter: Option<String>,
    },
    Get {
        #[arg(long)]
        trace_id: String,
        #[arg(long)]
        include_logs: bool,
        #[arg(long)]
        filter: Option<String>,
    },
    Summary {
        #[arg(long)]
        trace_id: String,
    },
    Slow {
        #[arg(long)]
        min_duration_ms: Option<f64>,
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        group_by: Option<String>,
    },
    Logs {
        #[arg(long)]
        trace_id: String,
        #[arg(long)]
        filter: Option<String>,
    },
}

pub async fn dispatch(broker: &Broker, cmd: TracesCmd, json: bool) -> i32 {
    match cmd.verb {
        TrcVerb::Recent { count, filter } => {
            let result = match broker.traces_recent(TracesRecent { count: Some(count), filter }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.recent failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if result.traces.is_empty() { println!("(no traces)"); return 0; }
            let rows: Vec<Vec<String>> = result.traces.iter().map(format_trace_row).collect();
            format::print_table(&["trace_id", "service", "root", "duration_ms", "spans", "errors"], rows);
            0
        }
        TrcVerb::Get { trace_id, include_logs, filter } => {
            let result = match broker.traces_get(TracesGet {
                trace_id, include_logs: Some(include_logs), filter,
            }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.get failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("trace: {} ({} spans, {} logs)", result.trace_id, result.span_count, result.log_count);
            let blocks: Vec<String> = result.spans.iter().map(format_span).collect();
            format::print_blocks(blocks, "(no spans)");
            0
        }
        TrcVerb::Summary { trace_id } => {
            let result = match broker.traces_summary(TracesSummary { trace_id }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.summary failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            println!("root: {}", result.root_span);
            println!("total: {:.1}ms ({} spans)", result.total_duration_ms, result.span_count);
            for entry in &result.breakdown {
                println!(
                    "  {:>5.1}%  {:.1}ms  self={:.1}ms  {}{}",
                    entry.percentage, entry.total_time_ms, entry.self_time_ms,
                    entry.name,
                    if entry.is_error { " (error)" } else { "" },
                );
            }
            if result.other_ms > 0.1 {
                println!("  ?       {:.1}ms  (other)", result.other_ms);
            }
            0
        }
        TrcVerb::Slow { min_duration_ms, count, filter, group_by } => {
            let result = match broker.traces_slow(TracesSlow { min_duration_ms, count, filter, group_by }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.slow failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            if let Some(groups) = &result.groups {
                if groups.is_empty() { println!("(no slow spans)"); return 0; }
                let rows: Vec<Vec<String>> = groups.iter().map(|g| vec![
                    g.name.clone(),
                    format!("{}", g.count),
                    format!("{:.1}", g.avg_ms),
                    format!("{:.1}", g.p95_ms),
                ]).collect();
                format::print_table(&["name", "count", "avg_ms", "p95_ms"], rows);
            } else if let Some(spans) = &result.spans {
                if spans.is_empty() { println!("(no slow spans)"); return 0; }
                let blocks: Vec<String> = spans.iter().map(format_span).collect();
                format::print_blocks(blocks, "(no slow spans)");
            } else {
                println!("(empty result)");
            }
            0
        }
        TrcVerb::Logs { trace_id, filter } => {
            let result = match broker.traces_logs(TracesLogs { trace_id, filter }).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("traces.logs failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.logs.iter().map(format_log_oneline).collect();
            format::print_blocks(blocks, "(no logs)");
            if let Some(seq) = result.cursor_advanced_to {
                println!("\ncursor advanced to seq={seq}");
            }
            0
        }
    }
}

fn format_trace_row(t: &TraceSummary) -> Vec<String> {
    vec![
        t.trace_id.clone(),
        t.service_name.clone(),
        t.root_span_name.clone(),
        format!("{:.1}", t.total_duration_ms),
        t.span_count.to_string(),
        if t.has_errors { "yes" } else { "no" }.to_string(),
    ]
}

fn format_span(s: &SpanEntry) -> String {
    let parent = s.parent_span_id.as_deref().unwrap_or("(root)");
    let status = match &s.status {
        logmon_broker_protocol::SpanStatus::Ok => "ok".to_string(),
        logmon_broker_protocol::SpanStatus::Unset => "unset".to_string(),
        logmon_broker_protocol::SpanStatus::Error(m) => format!("error: {m}"),
    };
    format!(
        "[seq={}] {} {} ({:.1}ms) status={status}\n  trace={} span={} parent={parent} service={}",
        s.seq, s.start_time.to_rfc3339(), s.name, s.duration_ms,
        s.trace_id, s.span_id, s.service_name,
    )
}

fn format_log_oneline(e: &LogEntry) -> String {
    format!(
        "[seq={}] {} {} {}",
        e.seq, e.timestamp.to_rfc3339(), format::format_level(&e.level), e.message
    )
}
