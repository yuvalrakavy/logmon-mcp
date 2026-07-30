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
];

/// Tool names only, sorted — what the daemon puts in `broker_tools`.
pub fn tool_names() -> Vec<String> {
    let mut v: Vec<String> = TOOLS.iter().map(|(t, _)| (*t).to_string()).collect();
    v.sort();
    v
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
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pair_is_well_formed_and_unique() {
        assert_eq!(TOOLS.len(), 42);
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

    #[test]
    fn an_empty_broker_list_yields_no_claim_rather_than_a_full_one() {
        // An older broker sends nothing. Absent must not read as "you are
        // missing everything" — nor as "you are missing nothing", which is why
        // the caller checks for the field's presence, not this result.
        let local = ["a", "b"];
        assert!(missing_tools(&[], &local).is_empty());
    }
}
