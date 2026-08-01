//! The agent-facing surface: every tool, what it calls, and how to offer it.
//!
//! **This list IS the surface — it stopped being a mirror of one on
//! 2026-08-01.** It used to describe 45 `#[rmcp::tool]` attributes on
//! `GelfMcpServer`, held honest by a test that compared the two. Those
//! attributes are gone: the shim now builds both its MCP router and its CLI
//! from [`manifest()`], which is built from here. There is one place a tool is
//! declared, so there is nothing left for it to drift from.
//!
//! That is what lets a broker gain a tool without its clients being rebuilt,
//! and it is why this module carries more than names: a description an agent
//! reads to choose, a parameter schema with its `$ref`s resolved, and the few
//! command-line facts ([`Cli`]) that a JSON Schema cannot express.
//!
//! Why clients need *tool* names rather than the daemon's method names: an
//! agent holds tool names, and the two vocabularies do not correspond
//! (`traces.slow` is `get_slow_spans`, `collectors.reset` is `reset_collector`
//! but `collectors.document` is `document_collectors`). Asking an agent to map
//! between them yields both false negatives and false positives.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The committed protocol schema, as the daemon serves it.
///
/// Embedded here so that the daemon and the shim resolve a tool's parameters
/// from the *same bytes*, rather than each reading a copy and hoping they
/// agree.
///
/// **This file is generated from this crate, and then compiled back into it.**
/// That is a tooling cycle, not a compile-time one — the file is committed — but
/// it means editing a request type without running `cargo xtask gen-schema`
/// leaves the embedded copy describing the previous shape. `verify-schema` is
/// what closes that, and it is the reason that check is not optional.
pub const SCHEMA_JSON: &str = include_str!("../protocol-v1.schema.json");

/// One agent-facing tool: its MCP name, the RPC method it calls, and the
/// description an agent reads to decide whether to call it.
///
/// The description lives here because the DAEMON has to serve it: a client that
/// registers its tools from the daemon needs the text from a crate the daemon
/// links, and the daemon does not link its clients.
pub struct Tool {
    pub name: &'static str,
    pub method: &'static str,
    pub description: &'static str,
    /// What a front-end needs to know beyond the schema. Almost always
    /// [`Cli::PLAIN`] — the exceptions are listed on the few tools that have
    /// them, rather than in a second table that could disagree with this one.
    pub cli: Cli,
}

/// The parts of a command-line shape that a JSON Schema cannot express.
///
/// **This exists so the shim does not have to know.** Without it, "these two
/// arguments may be given positionally" and "this parameter names a local file"
/// are facts that live in hand-written command definitions — which is exactly
/// the hardcoded knowledge a daemon-taught shim is supposed to be free of.
#[derive(Debug, Clone, Copy)]
pub struct Cli {
    /// Parameters that may be given positionally, in this order.
    pub positional: &'static [&'static str],
    /// Whether the final positional absorbs every remaining argument.
    pub variadic: bool,
    /// How this tool's reply becomes a local file, if it does.
    pub file_output: Option<FileOutput>,
}

/// A reply the client writes to disk rather than printing.
///
/// **The daemon returns bytes and the client writes them**, because a daemon
/// resolves a relative path against its own working directory rather than the
/// caller's. What a generic front-end needs to know is *which* parameter names
/// the file and *which* field of the reply is its body — both stated here, so
/// no front-end has to know that `logs.export` or `collectors.document` behave
/// this way.
#[derive(Debug, Clone, Copy)]
pub struct FileOutput {
    /// The parameter naming the local path. Consumed by the client, never sent.
    pub path_param: &'static str,
    /// Which field of the reply is the file body.
    ///
    /// A string field is written verbatim — `collectors.document` returns
    /// markdown, and JSON-encoding it would put `\n` through the whole file.
    /// Any other field is written as pretty JSON. `None` writes the whole reply.
    pub content_field: Option<&'static str>,
    /// A companion file written beside the first.
    pub sidecar: Option<Sidecar>,
}

/// A second file written next to the first, under a name the reply chooses.
///
/// `collectors.document` produces one: the document's own front-matter refers
/// to it by name, so the two must land in the same directory to be reunited
/// after being moved or mailed on.
#[derive(Debug, Clone, Copy)]
pub struct Sidecar {
    /// Reply field holding the companion's file name.
    pub name_field: &'static str,
    /// Reply field holding its content.
    pub content_field: &'static str,
}

impl Cli {
    /// Every parameter is a named `--flag`, and the reply goes to stdout.
    pub const PLAIN: Cli = Cli {
        positional: &[],
        variadic: false,
        file_output: None,
    };
}

/// [`Cli`] as it crosses the wire. `Cli` itself is `&'static` so it can live in
/// a `const`; this is the owned shape a client deserializes into.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CliHints {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positional: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub variadic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_output: Option<FileOutputHints>,
}

/// [`FileOutput`] as it crosses the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FileOutputHints {
    pub path_param: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<SidecarHints>,
}

/// [`Sidecar`] as it crosses the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SidecarHints {
    pub name_field: String,
    pub content_field: String,
}

impl From<Cli> for CliHints {
    fn from(c: Cli) -> Self {
        CliHints {
            positional: c.positional.iter().map(|s| (*s).to_string()).collect(),
            variadic: c.variadic,
            file_output: c.file_output.map(|f| FileOutputHints {
                path_param: f.path_param.to_string(),
                content_field: f.content_field.map(str::to_string),
                sidecar: f.sidecar.map(|s| SidecarHints {
                    name_field: s.name_field.to_string(),
                    content_field: s.content_field.to_string(),
                }),
            }),
        }
    }
}

impl Tool {
    /// `collectors.edit` -> `CollectorsEdit`: the schema definition describing
    /// this tool's parameters.
    ///
    /// Mechanical rather than a table, because the generator names definitions
    /// after the Rust type and the types are named after the methods. A table
    /// here would be a third thing to keep in step with the other two.
    pub fn definition_name(&self) -> String {
        self.method
            .split('.')
            .flat_map(|seg| seg.split('_'))
            .map(|word| {
                let mut c = word.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    }
}

/// Every agent-facing tool this build defines.
///
/// Source order matches `crates/mcp/src/server.rs`, which makes a diff against
/// that file readable when a tool is added.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "get_status",
        method: "status.get",
        description: "Get current server status including buffer sizes, trigger counts, connection info, and message statistics",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_recent_logs",
        method: "logs.recent",
        description: "Get recent log entries from the buffer, newest first. Optionally filtered by a DSL expression. Response carries matched/scanned/buffer_total diagnostics (scanned=0 = empty buffer; matched=0 with scanned>0 = filter matched nothing while data flows) plus truncated/evicted_before_window when a bookmark/cursor window rolled off. Unknown-selector comparison typos like level>=WARN are rejected with a suggestion.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_log_context",
        method: "logs.context",
        description: "Get log entries surrounding a specific entry identified by seq number. Returns context before and after.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "export_logs",
        method: "logs.export",
        description: "Export log entries to a file. Supports json or text format.",
        cli: Cli {
            positional: &[],
            variadic: false,
            file_output: Some(FileOutput {
                path_param: "path",
                // The entries, not the envelope. `scanned` and `buffer_total`
                // say whether the export is complete; they belong in the reply
                // the caller reads, not in the file they asked for logs in.
                content_field: Some("logs"),
                sidecar: None,
            }),
        },
    },
    Tool {
        name: "clear_logs",
        method: "logs.clear",
        description: "Clear all log entries from the in-memory buffer.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_filters",
        method: "filters.list",
        description: "List all buffer filters. Logs are stored only if they match at least one filter (OR semantics). If no filters are configured, all logs are stored.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "add_filter",
        method: "filters.add",
        description: "Add a new buffer filter. Logs matching this filter will be stored. Uses OR semantics with existing filters.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "edit_filter",
        method: "filters.edit",
        description: "Edit an existing buffer filter by ID.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "remove_filter",
        method: "filters.remove",
        description: "Remove a buffer filter by ID.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_triggers",
        method: "triggers.list",
        description: "List all triggers. Triggers capture a window of logs around matching entries and emit notifications.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "add_trigger",
        method: "triggers.add",
        description: "Add a new trigger. When a log matches the filter, the pre/post windows are captured and a notification is emitted.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "edit_trigger",
        method: "triggers.edit",
        description: "Edit an existing trigger by ID. Only the provided fields are updated.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "remove_trigger",
        method: "triggers.remove",
        description: "Remove a trigger by ID.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_recent_traces",
        method: "traces.recent",
        description: "List recent traces with timing and error info",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_trace",
        method: "traces.get",
        description: "Get full trace detail — span tree + linked logs",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_trace_summary",
        method: "traces.summary",
        description: "Compact timing breakdown highlighting bottlenecks",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_slow_spans",
        method: "traces.slow",
        description: "Find slow spans, optionally grouped by operation name",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "add_collector",
        method: "collectors.add",
        description: "Arm a span time collector. Accumulates exact totals, percentiles and \
                       (at tree level) self time for every span matching the filter, from now \
                       until reset or removed. Use for before/after measurement: arm, run the \
                       workload, read, reset, change one thing, run again.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "list_collectors",
        method: "collectors.list",
        description: "List this session's collectors and what each has matched so far",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_collector",
        method: "collectors.get",
        description: "Read a collector's numbers. Returns exact totals, sketch percentiles, \
                       and sample-derived figures separately — they cover different \
                       populations and any field that cannot be computed says why.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "edit_collector",
        method: "collectors.edit",
        description: "Change an armed collector. Editing only the description changes \
                       nothing else; editing the filter, level, group_keys, max_sample_bytes \
                       or domain DISCARDS the live window, because a window and the \
                       definition describing it must not disagree. Recorded snapshots are \
                       never touched. Use it to re-pin a collector orphaned by a restart, or \
                       to drop a level when the sample budget runs out.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "snapshot_collector",
        method: "collectors.snapshot",
        description: "Record the current window as a named run and start the next one. This \
                       is the between-runs move for a before/after comparison: arm, run A, \
                       snapshot, change one thing, run B, snapshot, then compare. Unlike \
                       reset_collector it KEEPS the run. Always pass a description.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_collector_history",
        method: "collectors.history",
        description: "List a collector's recorded runs, oldest first, each with the \
                       definition it was taken under. With merge=true also combines them and \
                       reports the run-to-run spread — which is what tells you whether a \
                       difference between two runs is real or just noise.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "diff_collectors",
        method: "collectors.diff",
        description: "Compare two runs and get what moved. Arms are \"<collector>\" (live \
                       window), \"<collector>@<label>\" (one recorded run), or \
                       \"<collector>@*\" (every recorded run merged). Prefer @* on both \
                       sides: with single runs there is no run-to-run spread, so nothing \
                       can separate a real change from scheduling noise, and every \
                       threshold comes back \"unknown\". Each row carries the threshold used \
                       to suppress it, and estimated percentile rows carry the error on the \
                       delta — at or above 100% the error bar is wider than the delta. \
                       REFUSES rather than guessing when the arms are not comparable, and \
                       names the flag that would permit it.",
        cli: Cli {
            positional: &["a", "b"],
            variadic: false,
            file_output: None,
        },
    },
    Tool {
        name: "reset_collector",
        method: "collectors.reset",
        description: "Zero a collector and start a fresh window, keeping it armed. DISCARDS \
                       the run — use snapshot_collector if you want to keep it. Returns a \
                       summary of what was thrown away.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "document_collectors",
        method: "collectors.document",
        description: "Write up a measurement: what moved, what to do next, and every caveat \
                       attached to the number it qualifies. Returns the document as text \
                       (markdown by default) plus a sidecar with the full percentile table -- \
                       write them yourself if you want them on disk. Pass `question` when you \
                       generate it and `finding` on a second call once you have read it; \
                       regeneration is free and lossless. `format: folded` gives collapsed \
                       stacks for a flame graph, one arm at a time, level tree only.",
        cli: Cli {
            positional: &["names"],
            variadic: true,
            file_output: Some(FileOutput {
                path_param: "path",
                // Markdown, written verbatim. JSON-encoding it would put a
                // literal `\n` through every line of the document.
                content_field: Some("content"),
                sidecar: Some(Sidecar {
                    name_field: "sidecar_name",
                    content_field: "sidecar_content",
                }),
            }),
        },
    },
    Tool {
        name: "remove_collector",
        method: "collectors.remove",
        description: "Remove a collector and release its sample budget",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "profile_traces",
        method: "traces.profile",
        description: "Profile spans already in the buffer, without arming anything. Same \
                       numbers as get_collector but over what is stored now — use it to look \
                       back at a run that already happened, and a collector to measure one \
                       that has not started.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_span_context",
        method: "spans.context",
        description: "Get spans surrounding a specific span in time",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_trace_logs",
        method: "traces.logs",
        description: "Get all logs linked to a trace",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "add_bookmark",
        method: "bookmarks.add",
        description: "Set a named bookmark at the current moment. Bookmarks are timestamps usable in filter DSL via b>=name / b<=name. Use them to scope queries to a range without destructively clearing logs.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "list_bookmarks",
        method: "bookmarks.list",
        description: "List all live bookmarks across all sessions, newest first. Optionally filter by session name.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "remove_bookmark",
        method: "bookmarks.remove",
        description: "Remove a bookmark by name. Bare name resolves to the current session; use 'session/name' to remove a bookmark from another session.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "clear_bookmarks",
        method: "bookmarks.clear",
        description: "Clear all bookmarks for a session at once. Defaults to the calling session. Useful for iterative debugging workflows: wipe all bookmarks, re-add fresh ones, repeat. Pass an explicit session name to clear another session's bookmarks.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_sessions",
        method: "session.list",
        description: "List all active sessions connected to the daemon.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "drop_session",
        method: "session.drop",
        description: "Drop (disconnect) a session by name.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "create_domain",
        method: "domains.create",
        description: "Create (or idempotently ensure) an isolated domain — a full broker instance with its own log/span buffers, receivers, and triggers. Omitted ports are auto-allocated; a port of 0 disables that receiver. Ephemeral (gone on daemon restart).",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "delete_domain",
        method: "domains.delete",
        description: "Delete a domain and tear down its receivers. Refuses config-declared domains including 'default'.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "list_domains",
        method: "domains.list",
        description: "List all live domains with their ports, source (config/persistent/ephemeral), and log/span counts.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "use_domain",
        method: "domains.use",
        description: "Bind this session to a domain. Subsequent log/trace queries and trigger notifications target that domain until you switch again. Errors if the domain does not exist.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "rename_session",
        method: "session.rename",
        description: "Rename this logmon session to a meaningful name (convention: <Project>-Main-<short8> for a home/main conversation, <Project>-tN-<branch> after claiming a dev-track lane; sanitize '/' to '-'). Preserves all session state. ERRORS with 'already connected' when the target name is held by a LIVE session — that means another conversation is already working that dev-track: STOP rather than fight over it. A stale (disconnected) holder is displaced automatically.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "clear_domain",
        method: "domains.clear",
        description: "Dispose the bound domain's data — logs and spans — keeping the domain and its receivers alive. Sequence numbers stay monotonic. (logs.clear is the logs-only cousin.)",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "update_domain_data",
        method: "domain_data.update",
        description: "Record provenance for the bound domain — what was true of the project while these logs were produced. Logs without it are a dump; logs with it are evidence. Core keys: /Build/commit, /Build/profile, /Action. Also /Versions/<component>, /Env/host, /Data/seed (the one that turns \"fails 1 in 20\" into a reproduction). Send a value to set it, or a key alone to confirm what is already there. Returns one outcome per entry: created, updated, validated, unknown, or rejected.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "get_domain_data",
        method: "domain_data.get",
        description: "Read the bound domain's provenance registry. Each key carries its value, when that value came into force, when it was last confirmed, and its age. A key with a stated lifetime also carries an expiry verdict; a key without one carries an age and NO verdict, deliberately — absent means unknown, not fresh. Also reports which recommended core keys are missing.",
        cli: Cli::PLAIN,
    },
    Tool {
        name: "remove_domain_data",
        method: "domain_data.remove",
        description: "Remove provenance keys by prefix, matched on segment boundaries (/Versions removes /Versions/*; /Ver removes nothing). Reports a count per pattern. THERE IS NO UNDO — and note that removing a key and setting it again resets when its value came into force, which turns a months-old confirmed fact into a fresh-looking one. To drop only a lifetime, send ttl: false to update_domain_data instead.",
        cli: Cli::PLAIN,
    },
];

/// Tool names, sorted — what the daemon puts in `broker_tools`.
///
/// Derived once. `handle_status` calls this on every status request, and the
/// error path can discard the result, so re-sorting 42 fresh `String`s per call
/// is pure waste.
pub fn tool_names() -> &'static [String] {
    static NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        let mut v: Vec<String> = TOOLS.iter().map(|t| t.name.to_string()).collect();
        v.sort();
        v
    });
    &NAMES
}

// `missing_tools` and `skew_note` were removed here on 2026-08-01.
//
// They compared the tools a broker advertises against the ones a shim was
// COMPILED with, and told the user to reinstall when the second was short.
// A shim now registers whatever the broker declares, so the shortfall they
// reported cannot occur: there is no compiled-in list to fall behind. The
// concept dissolved rather than moving somewhere else, which is the clearest
// evidence the manifest is doing the job they were standing in for.
//
// `tool_names` stays: `status.get` still reports what this broker knows, and
// that is a fact about the broker rather than a comparison with a client.

// ---------------------------------------------------------------------------
// The manifest — everything a shim needs to expose a tool it was not built with
// ---------------------------------------------------------------------------

/// One tool, as the daemon serves it: the name an agent calls, the text it reads
/// to decide whether to, and the schema its arguments must satisfy.
///
/// This is the whole point of the exercise. A shim holding these three things
/// can register a tool it has never heard of, which is what lets the daemon gain
/// a tool without the shim being reinstalled.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ManifestEntry {
    pub name: String,
    pub method: String,
    pub description: String,
    /// The JSON Schema for this tool's parameters.
    ///
    /// `None` means the schema has no definition under this tool's derived name
    /// — which is a **generator gap, not a tool without parameters**. The two
    /// are indistinguishable from the outside, so they must not be conflated:
    /// serving `{}` for a missing definition would advertise "takes nothing" for
    /// a tool that takes several, and every call would then be rejected for
    /// unknown fields.
    ///
    /// **`$ref`s are resolved before it is served.** A client would otherwise
    /// have to chase `#/$defs/ThresholdSpec` to discover that `threshold` is an
    /// object with five fields — and a client that cannot see inside a nested
    /// parameter cannot offer it at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,

    /// What a front-end needs beyond the schema to build a command line.
    #[serde(default)]
    pub cli: CliHints,
}

/// Replace every `{"$ref": "#/$defs/X"}` with the definition it names.
///
/// Bounded by `depth` rather than by cycle detection: schemars emits `$defs`
/// per definition, and a request type deep enough to exhaust this is a design
/// problem worth noticing rather than a resolution problem worth solving. A
/// `$ref` left unresolved at the bottom stays as it was, so the worst case is
/// the behaviour before this existed.
fn resolve_refs(node: &mut serde_json::Value, defs: &serde_json::Value, depth: u8) {
    if depth == 0 {
        return;
    }
    match node {
        serde_json::Value::Object(map) => {
            if let Some(name) = map
                .get("$ref")
                .and_then(|r| r.as_str())
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                if let Some(target) = defs.get(name) {
                    let mut replacement = target.clone();
                    resolve_refs(&mut replacement, defs, depth - 1);
                    // The `$ref` site may carry its own `description`; schemars
                    // puts the field's doc comment there and the type's on the
                    // definition. Keeping the field's is the useful half.
                    let described = map.get("description").cloned();
                    *node = replacement;
                    if let (Some(d), Some(obj)) = (described, node.as_object_mut()) {
                        obj.insert("description".into(), d);
                    }
                    return;
                }
            }
            for v in map.values_mut() {
                resolve_refs(v, defs, depth - 1);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                resolve_refs(v, defs, depth - 1);
            }
        }
        _ => {}
    }
}

/// `tools.manifest` takes no parameters: the manifest describes the protocol,
/// not the connection, so nothing about the caller could change the answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolsManifest {}

/// What `tools.manifest` answers with.
///
/// Lives here rather than in `methods` because it describes the MCP front-end's
/// surface, which is the boundary this module exists to keep separate from the
/// wire protocol's own types.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolsManifestResult {
    pub protocol_version: u32,
    /// Which daemon build answered. A shim reports skew against this rather
    /// than guessing from the tool list.
    pub broker_version: String,
    pub tools: Vec<ManifestEntry>,
}

/// One file a client is to write, resolved from a reply.
pub struct PendingFile {
    pub path: std::path::PathBuf,
    pub body: String,
}

/// Resolve a reply into the files a client should write.
///
/// Shared by every front-end deliberately: the CLI and the MCP surface writing
/// *different* files for the same tool is exactly the divergence that put a
/// sidecar on disk from one and not the other. Returns an empty list when the
/// tool produces no file or the caller named no path.
///
/// A string field is written verbatim; anything else is pretty JSON. The
/// sidecar lands in the same directory as the main file, because the document's
/// own front-matter refers to it by bare name.
pub fn files_to_write(
    hints: &CliHints,
    reply: &serde_json::Value,
    path: Option<&str>,
) -> Vec<PendingFile> {
    let (Some(out), Some(path)) = (&hints.file_output, path) else {
        return Vec::new();
    };
    // `-` is stdout by long convention, and the deleted CLI honoured it
    // (`Some("-") | None => print!(…)`). Taken literally it creates a file
    // named `-` in the working directory, which is the kind of thing a user
    // finds weeks later. Returning nothing routes the body to the caller's
    // normal printing path, which is what `-` asks for.
    if path == "-" {
        return Vec::new();
    }

    let render = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };

    let body = match &out.content_field {
        Some(field) => match reply.get(field) {
            Some(v) => render(v),
            // The named field is absent: write nothing rather than an empty
            // file, which would read as "the tool produced nothing".
            None => return Vec::new(),
        },
        None => render(reply),
    };

    let main = std::path::PathBuf::from(path);
    let mut files = vec![PendingFile {
        path: main.clone(),
        body,
    }];

    if let Some(side) = &out.sidecar {
        if let (Some(name), Some(content)) = (
            reply.get(&side.name_field).and_then(|v| v.as_str()),
            reply.get(&side.content_field),
        ) {
            let dir = main
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .to_path_buf();
            files.push(PendingFile {
                path: dir.join(name),
                body: render(content),
            });
        }
    }
    files
}

/// Every tool this protocol version defines, with its parameter schema resolved.
///
/// Built at call time rather than as a `const`, because resolving a definition
/// out of the embedded schema needs a JSON parse.
pub fn manifest() -> Vec<ManifestEntry> {
    let schema: serde_json::Value =
        serde_json::from_str(SCHEMA_JSON).expect("the embedded protocol schema must parse");
    let definitions = schema.get("definitions");

    TOOLS
        .iter()
        .map(|t| {
            let input_schema = definitions
                .and_then(|d| d.get(t.definition_name()))
                .cloned()
                .map(|mut s| {
                    // `$defs` is emitted per definition, so a ref resolves
                    // against the entry's own copy and never a global one.
                    let defs = s.get("$defs").cloned().unwrap_or(serde_json::Value::Null);
                    resolve_refs(&mut s, &defs, 16);
                    if let Some(obj) = s.as_object_mut() {
                        obj.remove("$defs");
                    }
                    s
                });
            ManifestEntry {
                name: t.name.to_string(),
                method: t.method.to_string(),
                description: t.description.to_string(),
                input_schema,
                cli: t.cli.into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    /// A shim registering from the manifest can only expose what the manifest
    /// describes, so a tool whose parameters resolve to nothing is a tool that
    /// shim cannot offer correctly.
    #[test]
    fn every_tool_in_the_manifest_carries_its_parameter_schema() {
        let m = manifest();
        assert_eq!(m.len(), TOOLS.len(), "one entry per tool");

        let unresolved: Vec<&str> = m
            .iter()
            .filter(|e| e.input_schema.is_none())
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            unresolved.is_empty(),
            "no definition in the embedded schema for: {unresolved:?} — the \
             generator's type list is hand-maintained, so this is a gap in it, \
             not a set of tools that take no parameters"
        );
    }

    /// The name derivation is the only link between a tool and its schema, so a
    /// wrong rule silently unresolves every tool it touches.
    #[test]
    fn definition_names_are_derived_from_the_method() {
        let by_name = |n: &str| TOOLS.iter().find(|t| t.name == n).expect("present");
        assert_eq!(
            by_name("edit_collector").definition_name(),
            "CollectorsEdit"
        );
        assert_eq!(by_name("get_status").definition_name(), "StatusGet");
        // Two underscores in one segment, which is where a naive split breaks.
        assert_eq!(
            by_name("get_span_context").definition_name(),
            "SpansContext"
        );
    }

    /// The gap this closes: `threshold` is `{"$ref": "#/$defs/ThresholdSpec"}`,
    /// so a client reading the raw definition sees a parameter it cannot
    /// describe, cannot validate and therefore cannot offer. `add_collector`
    /// could not arm a threshold through any generic front-end.
    #[test]
    fn a_nested_object_parameter_is_resolved_not_left_as_a_ref() {
        let m = manifest();
        let add = m
            .iter()
            .find(|e| e.name == "add_collector")
            .expect("present");
        let threshold = &add.input_schema.as_ref().expect("schema")["properties"]["threshold"];

        let blob = serde_json::to_string(threshold).unwrap();
        assert!(
            !blob.contains("$ref"),
            "a client cannot chase a ref it was not given the target of: {blob}"
        );

        // The five fields must be visible, or a front-end cannot build the object.
        let inner = threshold["anyOf"]
            .as_array()
            .expect("Option<ThresholdSpec> renders as anyOf")
            .iter()
            .find_map(|a| a.get("properties"))
            .expect("the object arm carries its properties");
        for f in ["metric", "group", "op", "value", "window_ms"] {
            assert!(inner.get(f).is_some(), "`{f}` is not reachable: {inner}");
        }
        // And the enums Phase A declared survive resolution.
        assert!(
            inner["op"]["enum"]
                .as_array()
                .is_some_and(|v| v.iter().any(|x| x == "gte")),
            "accepted values must survive: {}",
            inner["op"]
        );
    }

    /// `$defs` is scaffolding for resolution, not something a client should
    /// have to understand once resolution has happened.
    #[test]
    fn resolution_leaves_no_defs_behind() {
        for e in manifest() {
            if let Some(s) = &e.input_schema {
                assert!(s.get("$defs").is_none(), "{} still ships its $defs", e.name);
                assert!(
                    !serde_json::to_string(s).unwrap().contains("$ref"),
                    "{} still contains an unresolved ref",
                    e.name
                );
            }
        }
    }

    /// The CLI facts a schema cannot carry. Without these the shim would need
    /// to know that `collectors diff` takes two positional arms and that
    /// `export_logs` writes a file — which is the hardcoded knowledge the
    /// manifest exists to remove.
    #[test]
    fn cli_hints_reach_the_client() {
        let m = manifest();
        let by = |n: &str| m.iter().find(|e| e.name == n).expect("present").clone();

        assert_eq!(by("diff_collectors").cli.positional, ["a", "b"]);
        assert!(!by("diff_collectors").cli.variadic);

        assert_eq!(by("document_collectors").cli.positional, ["names"]);
        assert!(
            by("document_collectors").cli.variadic,
            "`names` is a list, so the last positional must absorb the rest"
        );

        let export = by("export_logs").cli.file_output.expect("writes a file");
        assert_eq!(
            export.path_param, "path",
            "a daemon resolves a relative path against its own cwd; the write is client-side"
        );
        assert_eq!(
            export.content_field.as_deref(),
            Some("logs"),
            "the entries, not the envelope"
        );

        // `collectors.document` produces TWO files, and the second is the one a
        // generic front-end would silently drop: the document's front-matter
        // names it, so losing it leaves a reference to a file nobody wrote.
        let doc = by("document_collectors")
            .cli
            .file_output
            .expect("writes files");
        assert_eq!(doc.content_field.as_deref(), Some("content"));
        let side = doc.sidecar.expect("and a companion beside it");
        assert_eq!(side.name_field, "sidecar_name");
        assert_eq!(side.content_field, "sidecar_content");

        // Everything else is plain, and says so rather than staying silent.
        let plain = m
            .iter()
            .filter(|e| {
                e.cli.positional.is_empty() && !e.cli.variadic && e.cli.file_output.is_none()
            })
            .count();
        assert_eq!(
            plain,
            TOOLS.len() - 3,
            "only three tools have a special shape"
        );
    }

    /// The regression this replaced: one "write the reply to this path" hint
    /// wrote the whole envelope and no companion file, so `collectors document`
    /// produced neither the markdown nor its sidecar.
    #[test]
    fn a_document_reply_resolves_to_both_files() {
        let doc = manifest()
            .into_iter()
            .find(|e| e.name == "document_collectors")
            .expect("present");
        let reply = serde_json::json!({
            "content": "# Title\n\nbody\n",
            "bytes": 16,
            "sidecar_name": "run-logdata.json",
            "sidecar_content": { "runs": [1, 2] },
            "warnings": [],
        });

        let files = files_to_write(&doc.cli, &reply, Some("/tmp/out/report.md"));
        assert_eq!(files.len(), 2, "the companion file must not be dropped");

        assert_eq!(
            files[0].path,
            std::path::PathBuf::from("/tmp/out/report.md")
        );
        assert_eq!(
            files[0].body, "# Title\n\nbody\n",
            "markdown is written verbatim; JSON-encoding it would put a literal \\n \
             through every line"
        );

        assert_eq!(
            files[1].path,
            std::path::PathBuf::from("/tmp/out/run-logdata.json"),
            "beside the document, because its front-matter names it without a directory"
        );
        assert!(files[1].body.contains("runs"));
    }

    /// No path means no files — and specifically not an empty one, which would
    /// read as "the tool produced nothing".
    #[test]
    fn nothing_is_written_without_a_path_or_a_body() {
        let doc = manifest()
            .into_iter()
            .find(|e| e.name == "document_collectors")
            .expect("present");
        let reply = serde_json::json!({ "content": "x" });
        assert!(files_to_write(&doc.cli, &reply, None).is_empty());

        // Named content field absent from the reply.
        assert!(files_to_write(&doc.cli, &serde_json::json!({}), Some("/tmp/x")).is_empty());

        // A tool that writes no files never does, path or not.
        let plain = manifest()
            .into_iter()
            .find(|e| e.name == "list_collectors")
            .expect("present");
        assert!(files_to_write(&plain.cli, &reply, Some("/tmp/x")).is_empty());
    }

    /// `logs.export` writes the entries. The envelope's `scanned` and
    /// `buffer_total` say whether the export is complete; they belong in the
    /// reply the caller reads, not in the file they asked for logs in.
    #[test]
    fn an_export_writes_entries_not_the_envelope() {
        let ex = manifest()
            .into_iter()
            .find(|e| e.name == "export_logs")
            .expect("present");
        let reply = serde_json::json!({
            "logs": [{ "message": "boom" }],
            "count": 1,
            "scanned": 40,
            "buffer_total": 100,
        });
        let files = files_to_write(&ex.cli, &reply, Some("/tmp/x.json"));
        assert_eq!(files.len(), 1);
        assert!(files[0].body.contains("boom"));
        assert!(
            !files[0].body.contains("buffer_total"),
            "the envelope must not land in the file: {}",
            files[0].body
        );
    }

    /// **Every parameter an agent can set must say what it is.**
    ///
    /// This regressed silently once and was worth 84 of 121 parameters: the
    /// per-field doc comments lived on 37 hand-written shim param structs, and
    /// deleting those structs took the text with them. Agents were choosing
    /// `pre_window`, `oneshot` and `top_n` with nothing to go on — including
    /// the default values the old text spelled out.
    ///
    /// A schema is only a single source of truth if it carries what the thing
    /// it replaced carried.
    #[test]
    fn every_parameter_an_agent_can_set_is_documented() {
        let undocumented: Vec<String> = manifest()
            .iter()
            .filter_map(|e| e.input_schema.as_ref().map(|s| (e, s)))
            .flat_map(|(e, s)| {
                s.get("properties")
                    .and_then(|p| p.as_object())
                    .map(|p| {
                        p.iter()
                            .filter(|(_, v)| {
                                v.get("description")
                                    .and_then(|d| d.as_str())
                                    .is_none_or(str::is_empty)
                            })
                            .map(|(k, _)| format!("{}.{k}", e.name))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            undocumented.is_empty(),
            "{} parameters an agent must guess at: {undocumented:?}",
            undocumented.len()
        );
    }

    /// Descriptions are what an agent reads to choose a tool. An empty one is
    /// not a formatting problem, it is a tool nobody can tell is right.
    #[test]
    fn every_tool_has_a_description() {
        let empty: Vec<&str> = TOOLS
            .iter()
            .filter(|t| t.description.trim().is_empty())
            .map(|t| t.name)
            .collect();
        assert!(empty.is_empty(), "no description for: {empty:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_is_well_formed_and_unique() {
        // Pinned so a tool cannot be added here without also being added to the
        // shim — the source-scan test in `mcp/src/server.rs` is the other half.
        assert_eq!(TOOLS.len(), 45);
        for Tool {
            name: tool, method, ..
        } in TOOLS
        {
            assert!(!tool.is_empty() && !method.is_empty());
            assert!(
                method.contains('.'),
                "`{method}` is not a `group.verb` RPC name"
            );
        }
        let mut tools: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        tools.sort_unstable();
        let before = tools.len();
        tools.dedup();
        assert_eq!(before, tools.len(), "duplicate tool name");

        let mut methods: Vec<&str> = TOOLS.iter().map(|t| t.method).collect();
        methods.sort_unstable();
        let before = methods.len();
        methods.dedup();
        assert_eq!(before, methods.len(), "duplicate method");
    }
}
