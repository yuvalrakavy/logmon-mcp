#![recursion_limit = "512"]

use clap::{Parser, Subcommand};
use logmon_broker_protocol::*;
use schemars::schema_for;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate protocol-v1.schema.json from schemars-derived types
    GenSchema {
        #[arg(long, default_value = "crates/protocol/protocol-v1.schema.json")]
        out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::GenSchema { out } => gen_schema(&out)?,
    }
    Ok(())
}

fn gen_schema(out: &PathBuf) -> anyhow::Result<()> {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "logmon-broker-protocol-v1",
        "description": format!("logmon broker JSON-RPC protocol, PROTOCOL_VERSION = {}", PROTOCOL_VERSION),
        "version": PROTOCOL_VERSION,
        "definitions": {
            // Method param/result structs
            "LogsRecent":            schema_for!(LogsRecent),
            "LogsRecentResult":      schema_for!(LogsRecentResult),
            "LogsContext":           schema_for!(LogsContext),
            "LogsContextResult":     schema_for!(LogsContextResult),
            "LogsExport":            schema_for!(LogsExport),
            "LogsExportResult":      schema_for!(LogsExportResult),
            "LogsClear":             schema_for!(LogsClear),
            "LogsClearResult":       schema_for!(LogsClearResult),
            "FiltersList":           schema_for!(FiltersList),
            "FiltersListResult":     schema_for!(FiltersListResult),
            "FiltersAdd":            schema_for!(FiltersAdd),
            "FiltersAddResult":      schema_for!(FiltersAddResult),
            "FiltersEdit":           schema_for!(FiltersEdit),
            "FiltersEditResult":     schema_for!(FiltersEditResult),
            "FiltersRemove":         schema_for!(FiltersRemove),
            "FiltersRemoveResult":   schema_for!(FiltersRemoveResult),
            "TriggersList":          schema_for!(TriggersList),
            "TriggersListResult":    schema_for!(TriggersListResult),
            "TriggersAdd":           schema_for!(TriggersAdd),
            "TriggersAddResult":     schema_for!(TriggersAddResult),
            "TriggersEdit":          schema_for!(TriggersEdit),
            "TriggersEditResult":    schema_for!(TriggersEditResult),
            "TriggersRemove":        schema_for!(TriggersRemove),
            "TriggersRemoveResult":  schema_for!(TriggersRemoveResult),
            "TracesRecent":          schema_for!(TracesRecent),
            "TracesRecentResult":    schema_for!(TracesRecentResult),
            "TracesGet":             schema_for!(TracesGet),
            "TracesGetResult":       schema_for!(TracesGetResult),
            "TracesSummary":         schema_for!(TracesSummary),
            "TracesSummaryResult":   schema_for!(TracesSummaryResult),
            "TracesSlow":            schema_for!(TracesSlow),
            "TracesSlowResult":      schema_for!(TracesSlowResult),
            "TracesLogs":            schema_for!(TracesLogs),
            "TracesLogsResult":      schema_for!(TracesLogsResult),
            "SpansContext":          schema_for!(SpansContext),
            "SpansContextResult":    schema_for!(SpansContextResult),
            "BookmarksAdd":          schema_for!(BookmarksAdd),
            "BookmarksAddResult":    schema_for!(BookmarksAddResult),
            "BookmarksList":         schema_for!(BookmarksList),
            "BookmarksListResult":   schema_for!(BookmarksListResult),
            "BookmarksRemove":       schema_for!(BookmarksRemove),
            "BookmarksRemoveResult": schema_for!(BookmarksRemoveResult),
            "BookmarksClear":        schema_for!(BookmarksClear),
            "BookmarksClearResult":  schema_for!(BookmarksClearResult),
            "SessionList":           schema_for!(SessionList),
            "SessionListResult":     schema_for!(SessionListResult),
            "SessionDrop":           schema_for!(SessionDrop),
            "SessionDropResult":     schema_for!(SessionDropResult),
            "StatusGet":             schema_for!(StatusGet),
            "StatusGetResult":       schema_for!(StatusGetResult),
            "SessionStartParams":    schema_for!(SessionStartParams),
            "SessionStartResult":    schema_for!(SessionStartResult),
            // Notification payloads
            "TriggerFiredPayload":   schema_for!(TriggerFiredPayload),
            // Shared types
            "LogEntry":              schema_for!(LogEntry),
            "Level":                 schema_for!(Level),
            "LogSource":             schema_for!(LogSource),
            "SpanEntry":             schema_for!(SpanEntry),
            "SpanKind":              schema_for!(SpanKind),
            "SpanStatus":            schema_for!(SpanStatus),
            "SpanEvent":             schema_for!(SpanEvent),
            "TraceSummary":          schema_for!(TraceSummary),
            "TraceSummaryBreakdownEntry": schema_for!(TraceSummaryBreakdownEntry),
            "TracesSlowGroup":       schema_for!(TracesSlowGroup),
            "FilterInfo":            schema_for!(FilterInfo),
            "TriggerInfo":           schema_for!(TriggerInfo),
            "SessionInfo":           schema_for!(SessionInfo),
            "StoreStats":            schema_for!(StoreStats),
            "BookmarkInfo":          schema_for!(BookmarkInfo),
        }
    });
    let pretty = serde_json::to_string_pretty(&schema)?;
    fs::write(out, format!("{pretty}\n"))?;
    println!("wrote {}", out.display());
    Ok(())
}
