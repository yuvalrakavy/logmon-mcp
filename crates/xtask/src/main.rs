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
    /// Regenerate schema and fail if it differs from the committed file
    VerifySchema {
        #[arg(long, default_value = "crates/protocol/protocol-v1.schema.json")]
        expected: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::GenSchema { out } => gen_schema(&out)?,
        Cmd::VerifySchema { expected } => verify_schema(&expected)?,
    }
    Ok(())
}

/// Regenerate the schema into a temp file and fail if it differs from the
/// expected committed copy. Used in CI / pre-commit to guard against drift.
fn verify_schema(expected: &PathBuf) -> anyhow::Result<()> {
    let tmp = tempfile::NamedTempFile::new()?;
    gen_schema(&tmp.path().to_path_buf())?;
    let on_disk = fs::read_to_string(expected)?;
    let regenerated = fs::read_to_string(tmp.path())?;
    if on_disk != regenerated {
        anyhow::bail!(
            "{} is out of date — run 'cargo xtask gen-schema' and commit the result",
            expected.display()
        );
    }
    println!("schema {} is up to date", expected.display());
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
            // logs.fields. The nested row types are listed too: this map is
            // hand-maintained and `verify-schema` re-runs it against itself, so
            // an omission here is a live surface nothing describes.
            "LogsFields":            schema_for!(LogsFields),
            "LogsFieldsResult":      schema_for!(LogsFieldsResult),
            "FieldStats":            schema_for!(FieldStats),
            "TopValue":              schema_for!(TopValue),
            "ValueKind":             schema_for!(ValueKind),
            "FieldSource":           schema_for!(FieldSource),
            "LogsContext":           schema_for!(LogsContext),
            "LogsContextResult":     schema_for!(LogsContextResult),
            "LogsExport":            schema_for!(LogsExport),
            "LogsExportResult":      schema_for!(LogsExportResult),
            "EvidenceVerdict":       schema_for!(EvidenceVerdict),
            "NarrowedRange":         schema_for!(NarrowedRange),
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
            "SpansExport":           schema_for!(SpansExport),
            "SpansExportResult":     schema_for!(SpansExportResult),
            // cases.* — the archive contract
            "CaseAnchor":            schema_for!(CaseAnchor),
            "CasesCreate":           schema_for!(CasesCreate),
            "CasesCreateResult":     schema_for!(CasesCreateResult),
            "CaseFile":              schema_for!(CaseFile),
            "CaseNote":              schema_for!(CaseNote),
            "BookmarksAdd":          schema_for!(BookmarksAdd),
            "BookmarksAddResult":    schema_for!(BookmarksAddResult),
            "BookmarksList":         schema_for!(BookmarksList),
            "BookmarksListResult":   schema_for!(BookmarksListResult),
            "BookmarksRemove":       schema_for!(BookmarksRemove),
            "BookmarksRemoveResult": schema_for!(BookmarksRemoveResult),
            "BookmarksClear":        schema_for!(BookmarksClear),
            "BookmarksClearResult":  schema_for!(BookmarksClearResult),
            "SessionsList":           schema_for!(SessionsList),
            "SessionsListResult":     schema_for!(SessionsListResult),
            "SessionsDrop":           schema_for!(SessionsDrop),
            "SessionsDropResult":     schema_for!(SessionsDropResult),
            "StatusGet":             schema_for!(StatusGet),
            "StatusGetResult":       schema_for!(StatusGetResult),
            "TraceIngestCounts":     schema_for!(TraceIngestCounts),
            "SessionStartParams":    schema_for!(SessionStartParams),
            "SessionStartResult":    schema_for!(SessionStartResult),
            // domains.* (DomainsCreateResult is an alias of DomainInfo; DomainsUseResult is
            // its own struct and is listed below — the old comment said otherwise)
            "DomainInfo":            schema_for!(DomainInfo),
            "DomainsCreate":         schema_for!(DomainsCreate),
            "DomainsDelete":         schema_for!(DomainsDelete),
            "DomainsDeleteResult":   schema_for!(DomainsDeleteResult),
            "DomainsList":           schema_for!(DomainsList),
            "DomainsListResult":     schema_for!(DomainsListResult),
            "DomainsUse":            schema_for!(DomainsUse),
            "DomainsClear":          schema_for!(DomainsClear),
            "DomainsClearResult":    schema_for!(DomainsClearResult),
            // Single-field request types. Present because the list below is
            // hand-maintained, so an omission means a live tool whose parameters
            // nothing describes — and `verify-schema` cannot see it, since it
            // re-runs this same list against itself. A drift test in the shim
            // (`param_drift_tests`) is what catches that from the other side.
            "CollectorsReset":       schema_for!(CollectorsReset),
            "CollectorsRemove":      schema_for!(CollectorsRemove),
            "SessionsRename":         schema_for!(SessionsRename),
            "SessionsRenameResult":   schema_for!(SessionsRenameResult),
            "DomainsUseResult":      schema_for!(DomainsUseResult),
            // tools.manifest — what the daemon tells a shim about its own surface
            "ToolsManifest":          schema_for!(logmon_broker_protocol::mcp_tools::ToolsManifest),
            "ToolsManifestResult":    schema_for!(logmon_broker_protocol::mcp_tools::ToolsManifestResult),
            "ManifestEntry":          schema_for!(logmon_broker_protocol::mcp_tools::ManifestEntry),
            "CliHints":               schema_for!(logmon_broker_protocol::mcp_tools::CliHints),
            // domain_data.* — the provenance registry
            "DomainDataEntry":         schema_for!(DomainDataEntry),
            "DomainDataUpdate":        schema_for!(DomainDataUpdate),
            "DomainDataOutcome":       schema_for!(DomainDataOutcome),
            "DomainDataUpdateResult":  schema_for!(DomainDataUpdateResult),
            "DomainDataRemove":        schema_for!(DomainDataRemove),
            "DomainDataRemoveOutcome": schema_for!(DomainDataRemoveOutcome),
            "DomainDataRemoveResult":  schema_for!(DomainDataRemoveResult),
            "DomainDataGet":           schema_for!(DomainDataGet),
            "DomainDataKey":           schema_for!(DomainDataKey),
            "DomainDataGetResult":     schema_for!(DomainDataGetResult),
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
            // collectors.* / traces.profile
            "CollectorsAdd":         schema_for!(CollectorsAdd),
            "CollectorsAddResult":   schema_for!(CollectorsAddResult),
            "CollectorsList":        schema_for!(CollectorsList),
            "CollectorsListResult":  schema_for!(CollectorsListResult),
            "CollectorInfo":         schema_for!(CollectorInfo),
            "CollectorsGet":         schema_for!(CollectorsGet),
            "CollectorsName":        schema_for!(CollectorsName),
            "CollectorsResetResult": schema_for!(CollectorsResetResult),
            "CollectorsDiscarded":   schema_for!(CollectorsDiscarded),
            "CollectorsRemoveResult": schema_for!(CollectorsRemoveResult),
            "TracesProfile":         schema_for!(TracesProfile),
            "CollectorsEdit":        schema_for!(CollectorsEdit),
            "CollectorsEditResult":  schema_for!(CollectorsEditResult),
            "CollectorsSnapshot":    schema_for!(CollectorsSnapshot),
            "CollectorsHistory":     schema_for!(CollectorsHistory),
            "CollectorsHistoryResult": schema_for!(CollectorsHistoryResult),
            "SnapshotSummary":       schema_for!(SnapshotSummary),
            "RunToRunFloor":         schema_for!(RunToRunFloor),
            "CollectorsDiff":        schema_for!(CollectorsDiff),
            "CollectorsDiffResult":  schema_for!(CollectorsDiffResult),
            "DiffArm":               schema_for!(DiffArm),
            "DiffRow":               schema_for!(DiffRow),
            "DiffGroup":             schema_for!(DiffGroup),
            "DiffMark":              schema_for!(DiffMark),
            "PathRow":               schema_for!(PathRow),
            "ThresholdSpec":         schema_for!(ThresholdSpec),
            "ThresholdInfo":         schema_for!(ThresholdInfo),
            "CollectorsDocument":    schema_for!(CollectorsDocument),
            "CollectorsDocumentResult": schema_for!(CollectorsDocumentResult),
            "ProfileResult":         schema_for!(ProfileResult),
            "ProfileWindow":         schema_for!(ProfileWindow),
            "ProfileIngest":         schema_for!(ProfileIngest),
            "ProfileExact":          schema_for!(ProfileExact),
            "ProfileEstimated":      schema_for!(ProfileEstimated),
            "ProfileSampled":        schema_for!(ProfileSampled),
            "ProfileGroup":          schema_for!(ProfileGroup),
            "Suppressed":            schema_for!(Suppressed),
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
