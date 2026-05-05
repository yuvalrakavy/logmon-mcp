//! `spans` subcommand group: context.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{SpanEntry, SpansContext};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct SpansCmd {
    #[command(subcommand)]
    verb: SpnVerb,
}

#[derive(Subcommand, Debug)]
enum SpnVerb {
    Context {
        #[arg(long)]
        seq: u64,
        #[arg(long, default_value_t = 5)]
        before: u64,
        #[arg(long, default_value_t = 5)]
        after: u64,
    },
}

pub async fn dispatch(broker: &Broker, cmd: SpansCmd, json: bool) -> i32 {
    match cmd.verb {
        SpnVerb::Context { seq, before, after } => {
            let params = SpansContext { seq, before: Some(before), after: Some(after) };
            let result = match broker.spans_context(params).await {
                Ok(r) => r,
                Err(e) => { format::error(&format!("spans.context failed: {e}"), json); return 1; }
            };
            if json { format::print_json(&result); return 0; }
            let blocks: Vec<String> = result.spans.iter().map(format_span).collect();
            format::print_blocks(blocks);
            0
        }
    }
}

fn format_span(s: &SpanEntry) -> String {
    let parent = s.parent_span_id.as_deref().unwrap_or("(root)");
    format!(
        "[seq={}] {} {} ({:.1}ms)\n  trace={} span={} parent={parent}",
        s.seq, s.start_time.to_rfc3339(), s.name, s.duration_ms,
        s.trace_id, s.span_id,
    )
}
