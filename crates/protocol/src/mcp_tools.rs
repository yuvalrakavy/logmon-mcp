//! The agent-facing surface: which MCP tool maps to which RPC method.
//!
//! One list, in the crate both sides already depend on. The daemon reads it to
//! state what a shim of this version exposes (`status.get`'s `broker_tools`);
//! the shim reads it to diff that against what it actually has.
//!
//! It does **not** build the shim's tools — those are `#[rmcp::tool]` attributes
//! on `GelfMcpServer`. This list is a mirror of them, held honest by a test that
//! compares it against the router those attributes generate. The pairing is the
//! unit: a tool cannot appear here without the method it calls, so the two names
//! cannot drift apart from each other.
//!
//! Why the daemon needs *tool* names rather than its own method names: an agent
//! holds tool names, and the two vocabularies do not correspond
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
/// The description lives here rather than only on the shim's `#[rmcp::tool]`
/// attribute because the DAEMON has to be able to serve it: a shim that
/// registers its tools from the daemon needs the text from somewhere the daemon
/// links, and the daemon does not link the shim. Held honest against the
/// attributes by a test in `crates/mcp/src/server.rs`.
pub struct Tool {
    pub name: &'static str,
    pub method: &'static str,
    pub description: &'static str,
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
    },
    Tool {
        name: "get_recent_logs",
        method: "logs.recent",
        description: "Get recent log entries from the buffer, newest first. Optionally filtered by a DSL expression. Response carries matched/scanned/buffer_total diagnostics (scanned=0 = empty buffer; matched=0 with scanned>0 = filter matched nothing while data flows) plus truncated/evicted_before_window when a bookmark/cursor window rolled off. Unknown-selector comparison typos like level>=WARN are rejected with a suggestion.",
    },
    Tool {
        name: "get_log_context",
        method: "logs.context",
        description: "Get log entries surrounding a specific entry identified by seq number. Returns context before and after.",
    },
    Tool {
        name: "export_logs",
        method: "logs.export",
        description: "Export log entries to a file. Supports json or text format.",
    },
    Tool {
        name: "clear_logs",
        method: "logs.clear",
        description: "Clear all log entries from the in-memory buffer.",
    },
    Tool {
        name: "get_filters",
        method: "filters.list",
        description: "List all buffer filters. Logs are stored only if they match at least one filter (OR semantics). If no filters are configured, all logs are stored.",
    },
    Tool {
        name: "add_filter",
        method: "filters.add",
        description: "Add a new buffer filter. Logs matching this filter will be stored. Uses OR semantics with existing filters.",
    },
    Tool {
        name: "edit_filter",
        method: "filters.edit",
        description: "Edit an existing buffer filter by ID.",
    },
    Tool {
        name: "remove_filter",
        method: "filters.remove",
        description: "Remove a buffer filter by ID.",
    },
    Tool {
        name: "get_triggers",
        method: "triggers.list",
        description: "List all triggers. Triggers capture a window of logs around matching entries and emit notifications.",
    },
    Tool {
        name: "add_trigger",
        method: "triggers.add",
        description: "Add a new trigger. When a log matches the filter, the pre/post windows are captured and a notification is emitted.",
    },
    Tool {
        name: "edit_trigger",
        method: "triggers.edit",
        description: "Edit an existing trigger by ID. Only the provided fields are updated.",
    },
    Tool {
        name: "remove_trigger",
        method: "triggers.remove",
        description: "Remove a trigger by ID.",
    },
    Tool {
        name: "get_recent_traces",
        method: "traces.recent",
        description: "List recent traces with timing and error info",
    },
    Tool {
        name: "get_trace",
        method: "traces.get",
        description: "Get full trace detail — span tree + linked logs",
    },
    Tool {
        name: "get_trace_summary",
        method: "traces.summary",
        description: "Compact timing breakdown highlighting bottlenecks",
    },
    Tool {
        name: "get_slow_spans",
        method: "traces.slow",
        description: "Find slow spans, optionally grouped by operation name",
    },
    Tool {
        name: "add_collector",
        method: "collectors.add",
        description: "Arm a span time collector. Accumulates exact totals, percentiles and \
                       (at tree level) self time for every span matching the filter, from now \
                       until reset or removed. Use for before/after measurement: arm, run the \
                       workload, read, reset, change one thing, run again.",
    },
    Tool {
        name: "list_collectors",
        method: "collectors.list",
        description: "List this session's collectors and what each has matched so far",
    },
    Tool {
        name: "get_collector",
        method: "collectors.get",
        description: "Read a collector's numbers. Returns exact totals, sketch percentiles, \
                       and sample-derived figures separately — they cover different \
                       populations and any field that cannot be computed says why.",
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
    },
    Tool {
        name: "snapshot_collector",
        method: "collectors.snapshot",
        description: "Record the current window as a named run and start the next one. This \
                       is the between-runs move for a before/after comparison: arm, run A, \
                       snapshot, change one thing, run B, snapshot, then compare. Unlike \
                       reset_collector it KEEPS the run. Always pass a description.",
    },
    Tool {
        name: "get_collector_history",
        method: "collectors.history",
        description: "List a collector's recorded runs, oldest first, each with the \
                       definition it was taken under. With merge=true also combines them and \
                       reports the run-to-run spread — which is what tells you whether a \
                       difference between two runs is real or just noise.",
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
    },
    Tool {
        name: "reset_collector",
        method: "collectors.reset",
        description: "Zero a collector and start a fresh window, keeping it armed. DISCARDS \
                       the run — use snapshot_collector if you want to keep it. Returns a \
                       summary of what was thrown away.",
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
    },
    Tool {
        name: "remove_collector",
        method: "collectors.remove",
        description: "Remove a collector and release its sample budget",
    },
    Tool {
        name: "profile_traces",
        method: "traces.profile",
        description: "Profile spans already in the buffer, without arming anything. Same \
                       numbers as get_collector but over what is stored now — use it to look \
                       back at a run that already happened, and a collector to measure one \
                       that has not started.",
    },
    Tool {
        name: "get_span_context",
        method: "spans.context",
        description: "Get spans surrounding a specific span in time",
    },
    Tool {
        name: "get_trace_logs",
        method: "traces.logs",
        description: "Get all logs linked to a trace",
    },
    Tool {
        name: "add_bookmark",
        method: "bookmarks.add",
        description: "Set a named bookmark at the current moment. Bookmarks are timestamps usable in filter DSL via b>=name / b<=name. Use them to scope queries to a range without destructively clearing logs.",
    },
    Tool {
        name: "list_bookmarks",
        method: "bookmarks.list",
        description: "List all live bookmarks across all sessions, newest first. Optionally filter by session name.",
    },
    Tool {
        name: "remove_bookmark",
        method: "bookmarks.remove",
        description: "Remove a bookmark by name. Bare name resolves to the current session; use 'session/name' to remove a bookmark from another session.",
    },
    Tool {
        name: "clear_bookmarks",
        method: "bookmarks.clear",
        description: "Clear all bookmarks for a session at once. Defaults to the calling session. Useful for iterative debugging workflows: wipe all bookmarks, re-add fresh ones, repeat. Pass an explicit session name to clear another session's bookmarks.",
    },
    Tool {
        name: "get_sessions",
        method: "session.list",
        description: "List all active sessions connected to the daemon.",
    },
    Tool {
        name: "drop_session",
        method: "session.drop",
        description: "Drop (disconnect) a session by name.",
    },
    Tool {
        name: "create_domain",
        method: "domains.create",
        description: "Create (or idempotently ensure) an isolated domain — a full broker instance with its own log/span buffers, receivers, and triggers. Omitted ports are auto-allocated; a port of 0 disables that receiver. Ephemeral (gone on daemon restart).",
    },
    Tool {
        name: "delete_domain",
        method: "domains.delete",
        description: "Delete a domain and tear down its receivers. Refuses config-declared domains including 'default'.",
    },
    Tool {
        name: "list_domains",
        method: "domains.list",
        description: "List all live domains with their ports, source (config/persistent/ephemeral), and log/span counts.",
    },
    Tool {
        name: "use_domain",
        method: "domains.use",
        description: "Bind this session to a domain. Subsequent log/trace queries and trigger notifications target that domain until you switch again. Errors if the domain does not exist.",
    },
    Tool {
        name: "rename_session",
        method: "session.rename",
        description: "Rename this logmon session to a meaningful name (convention: <Project>-Main-<short8> for a home/main conversation, <Project>-tN-<branch> after claiming a dev-track lane; sanitize '/' to '-'). Preserves all session state. ERRORS with 'already connected' when the target name is held by a LIVE session — that means another conversation is already working that dev-track: STOP rather than fight over it. A stale (disconnected) holder is displaced automatically.",
    },
    Tool {
        name: "clear_domain",
        method: "domains.clear",
        description: "Dispose the bound domain's data — logs and spans — keeping the domain and its receivers alive. Sequence numbers stay monotonic. (logs.clear is the logs-only cousin.)",
    },
    Tool {
        name: "update_domain_data",
        method: "domain_data.update",
        description: "Record provenance for the bound domain — what was true of the project while these logs were produced. Logs without it are a dump; logs with it are evidence. Core keys: /Build/commit, /Build/profile, /Action. Also /Versions/<component>, /Env/host, /Data/seed (the one that turns \"fails 1 in 20\" into a reproduction). Send a value to set it, or a key alone to confirm what is already there. Returns one outcome per entry: created, updated, validated, unknown, or rejected.",
    },
    Tool {
        name: "get_domain_data",
        method: "domain_data.get",
        description: "Read the bound domain's provenance registry. Each key carries its value, when that value came into force, when it was last confirmed, and its age. A key with a stated lifetime also carries an expiry verdict; a key without one carries an age and NO verdict, deliberately — absent means unknown, not fresh. Also reports which recommended core keys are missing.",
    },
    Tool {
        name: "remove_domain_data",
        method: "domain_data.remove",
        description: "Remove provenance keys by prefix, matched on segment boundaries (/Versions removes /Versions/*; /Ver removes nothing). Reports a count per pattern. THERE IS NO UNDO — and note that removing a key and setting it again resets when its value came into force, which turns a months-old confirmed fact into a fresh-looking one. To drop only a lifetime, send ttl: false to update_domain_data instead.",
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

/// Tool names a broker advertises that this build has no tool for.
///
/// A free function over both lists rather than logic inside the relay, because
/// in any single build the broker's list *is* `tool_names()` — so an end-to-end
/// test can only ever exercise the equal case. Every other case needs synthetic
/// input, which needs a seam.
pub fn missing_tools(broker_tools: &[String], local: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = broker_tools
        .iter()
        .filter(|t| !local.contains(&t.as_str()))
        .cloned()
        .collect();
    v.sort();
    // A malformed broker could repeat a name; saying it twice would also
    // inflate the count in the sentence built from this.
    v.dedup();
    v
}

/// The one sentence both front-ends show, or `None` when there is nothing to say.
///
/// Shared so the MCP tool result and the CLI **cannot** disagree. They did in an
/// earlier revision: the CLI compared version strings while the shim compared
/// tool sets, and this repo's own history has two commits stamped `0.5.1` with
/// different tool sets — so one surface printed an all-clear while the other
/// correctly reported two missing tools. Versions do not track capability here;
/// only the tool sets do.
///
/// `None` in three cases, each deliberate: the broker advertised nothing (an
/// older build — unknown, not "everything is missing"), the sets match (a notice
/// in the everyday case is what teaches a reader to ignore it), and the shim is
/// *ahead* of the broker (not a gap in the shim).
pub fn skew_note(broker_version: &str, broker_tools: &[String], local: &[&str]) -> Option<String> {
    if broker_tools.is_empty() {
        return None;
    }
    let missing = missing_tools(broker_tools, local);
    if missing.is_empty() {
        return None;
    }
    let version = if broker_version.is_empty() {
        "of unknown version"
    } else {
        broker_version
    };
    // Numerator is the INTERSECTION, not the local total: a shim can hold tools
    // the broker does not advertise (a rename, a removal, a different branch),
    // and "42 of the 42 tools ... not reachable: x" contradicts itself.
    let reachable = broker_tools.len() - missing.len();
    Some(format!(
        "This logmon MCP shim reaches {reachable} of the {} tools broker {version} supports. \
         Not reachable from this shim: {}. Reinstall the `logmon-mcp` binary \
         (`cargo install --path crates/mcp` from a logmon-mcp checkout) and restart \
         this MCP server to use them.",
        broker_tools.len(),
        missing.join(", "),
    ))
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
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
        .map(|t| ManifestEntry {
            name: t.name.to_string(),
            method: t.method.to_string(),
            description: t.description.to_string(),
            input_schema: definitions
                .and_then(|d| d.get(t.definition_name()))
                .cloned(),
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

    #[test]
    fn missing_tools_names_only_what_is_absent() {
        let local = ["a", "b"];
        let broker = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(missing_tools(&broker, &local), vec!["c".to_string()]);
    }

    #[test]
    fn an_equal_set_is_missing_nothing() {
        let local = ["a", "b"];
        let broker = vec!["b".to_string(), "a".to_string()];
        assert!(
            missing_tools(&broker, &local).is_empty(),
            "the everyday case: a false positive here is what makes a reader stop trusting the notice"
        );
    }

    #[test]
    fn a_shim_ahead_of_its_broker_makes_no_reversed_claim() {
        // The shim holds tools the broker never mentions. That is not a gap in
        // the shim, and reporting one would be a claim in the wrong direction.
        let local = ["a", "b", "c"];
        let broker = vec!["a".to_string()];
        assert!(missing_tools(&broker, &local).is_empty());
    }

    /// Sortedness has to be checked against an independently-sorted copy, not
    /// against another call to `tool_names()`. Every other consumer derives its
    /// expectation by calling this function, so dropping the `.sort()` left the
    /// entire suite green — `TOOLS` is in source order, so the output silently
    /// became source-ordered and nothing compared it to anything else.
    #[test]
    fn tool_names_come_back_sorted() {
        let names = tool_names();
        let mut independently_sorted = names.to_vec();
        independently_sorted.sort();
        assert_eq!(
            names,
            independently_sorted.as_slice(),
            "tool_names() must be sorted: a stable wire order is what lets a \
             reader diff two brokers' lists by eye"
        );
        assert_ne!(
            names.first().map(String::as_str),
            TOOLS.first().map(|t| t.name),
            "if TOOLS ever happens to be in sorted order this test stops \
             discriminating — reorder the table or pin a literal instead"
        );
    }

    #[test]
    fn the_note_counts_the_intersection_not_the_local_total() {
        // A shim holding a tool the broker does not advertise (a rename, a
        // removal, a different branch) must not be counted as reaching it.
        // "42 of the 42 tools ... not reachable: x" contradicts itself.
        let local = ["a", "b", "gone_from_broker"];
        let broker = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let note = skew_note("1.0.0", &broker, &local).expect("c is missing");
        assert!(
            note.contains("reaches 2 of the 3"),
            "numerator must be the intersection: {note}"
        );
        assert!(note.contains("Not reachable from this shim: c"));
    }

    #[test]
    fn the_note_names_a_binary_and_a_command_that_can_be_run() {
        let note = skew_note("1.0.0", &["x".to_string()], &[]).expect("x is missing");
        assert!(
            note.contains("logmon-mcp"),
            "name the binary, since the broker is a different one: {note}"
        );
        assert!(note.contains("cargo install"), "and the command: {note}");
        assert!(
            note.contains("restart"),
            "reinstalling without restarting the MCP server changes nothing: {note}"
        );
    }

    #[test]
    fn an_unknown_broker_version_still_produces_a_usable_note() {
        let note = skew_note("", &["x".to_string()], &[]).expect("x is missing");
        assert!(note.contains("unknown version"), "{note}");
        assert!(note.contains("Not reachable from this shim: x"));
    }

    #[test]
    fn a_repeated_broker_entry_is_named_once() {
        let broker = vec!["x".to_string(), "x".to_string()];
        let note = skew_note("1.0.0", &broker, &[]).expect("x is missing");
        assert_eq!(
            note.matches("x").count(),
            1,
            "a malformed broker must not make the note stutter: {note}"
        );
    }

    #[test]
    fn an_empty_broker_list_yields_no_claim_rather_than_a_full_one() {
        // An older broker sends nothing. Absent must not read as "you are
        // missing everything" — nor as "you are missing nothing", which is why
        // the caller checks for the field's presence, not this result.
        let local = ["a", "b"];
        assert!(missing_tools(&[], &local).is_empty());
    }
}
