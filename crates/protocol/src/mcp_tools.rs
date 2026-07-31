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

/// `(MCP tool name, RPC method)` for every agent-facing tool.
///
/// Source order matches `crates/mcp/src/server.rs`, which makes a diff against
/// that file readable when a tool is added.
pub const TOOLS: &[(&str, &str)] = &[
    ("get_status", "status.get"),
    ("get_recent_logs", "logs.recent"),
    ("get_log_context", "logs.context"),
    ("export_logs", "logs.export"),
    ("clear_logs", "logs.clear"),
    ("get_filters", "filters.list"),
    ("add_filter", "filters.add"),
    ("edit_filter", "filters.edit"),
    ("remove_filter", "filters.remove"),
    ("get_triggers", "triggers.list"),
    ("add_trigger", "triggers.add"),
    ("edit_trigger", "triggers.edit"),
    ("remove_trigger", "triggers.remove"),
    ("get_recent_traces", "traces.recent"),
    ("get_trace", "traces.get"),
    ("get_trace_summary", "traces.summary"),
    ("get_slow_spans", "traces.slow"),
    ("add_collector", "collectors.add"),
    ("list_collectors", "collectors.list"),
    ("get_collector", "collectors.get"),
    ("edit_collector", "collectors.edit"),
    ("snapshot_collector", "collectors.snapshot"),
    ("get_collector_history", "collectors.history"),
    ("diff_collectors", "collectors.diff"),
    ("reset_collector", "collectors.reset"),
    ("document_collectors", "collectors.document"),
    ("remove_collector", "collectors.remove"),
    ("profile_traces", "traces.profile"),
    ("get_span_context", "spans.context"),
    ("get_trace_logs", "traces.logs"),
    ("add_bookmark", "bookmarks.add"),
    ("list_bookmarks", "bookmarks.list"),
    ("remove_bookmark", "bookmarks.remove"),
    ("clear_bookmarks", "bookmarks.clear"),
    ("get_sessions", "session.list"),
    ("drop_session", "session.drop"),
    ("create_domain", "domains.create"),
    ("delete_domain", "domains.delete"),
    ("list_domains", "domains.list"),
    ("use_domain", "domains.use"),
    ("rename_session", "session.rename"),
    ("clear_domain", "domains.clear"),
    ("update_domain_data", "domain_data.update"),
    ("get_domain_data", "domain_data.get"),
    ("remove_domain_data", "domain_data.remove"),
];

/// Tool names, sorted — what the daemon puts in `broker_tools`.
///
/// Derived once. `handle_status` calls this on every status request, and the
/// error path can discard the result, so re-sorting 42 fresh `String`s per call
/// is pure waste.
pub fn tool_names() -> &'static [String] {
    static NAMES: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        let mut v: Vec<String> = TOOLS.iter().map(|(t, _)| (*t).to_string()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_is_well_formed_and_unique() {
        // Pinned so a tool cannot be added here without also being added to the
        // shim — the source-scan test in `mcp/src/server.rs` is the other half.
        assert_eq!(TOOLS.len(), 45);
        for (tool, method) in TOOLS {
            assert!(!tool.is_empty() && !method.is_empty());
            assert!(
                method.contains('.'),
                "`{method}` is not a `group.verb` RPC name"
            );
        }
        let mut tools: Vec<&str> = TOOLS.iter().map(|(t, _)| *t).collect();
        tools.sort_unstable();
        let before = tools.len();
        tools.dedup();
        assert_eq!(before, tools.len(), "duplicate tool name");

        let mut methods: Vec<&str> = TOOLS.iter().map(|(_, m)| *m).collect();
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
            TOOLS.first().map(|(t, _)| *t),
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
