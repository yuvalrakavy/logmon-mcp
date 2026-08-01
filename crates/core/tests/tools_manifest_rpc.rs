//! `tools.manifest` over the RPC boundary — the daemon states its own surface.
//!
//! **This is the method that decouples the shim from the daemon.** A shim that
//! registers what this returns exposes whatever the daemon declares; a shim
//! built against a compiled-in list can only ever expose what it shipped with.
//! So the assertions here are about *sufficiency*: not "does it answer", but
//! "does it answer with everything a shim would need to build a tool it has
//! never heard of".

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::{RpcRequest, RpcResponse};
use serde_json::{json, Value};
use std::sync::Arc;

fn make_domain(name: &str) -> Arc<Domain> {
    let seq = Arc::new(SeqCounter::new());
    Arc::new(Domain::from_parts(
        DomainConfig {
            name: DomainId::new(name).expect("valid domain name"),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 1000,
            span_buffer_size: 1000,
            source: DomainSource::Config,
        },
        Arc::new(LogPipeline::new_with_seq_counter(1000, seq.clone())),
        Arc::new(SpanStore::new(1000, seq)),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ))
}

fn call(method: &str, params: Value) -> RpcResponse {
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(make_domain("default"));
    let handler = RpcHandler::new(
        domains,
        Arc::new(SessionRegistry::new()),
        Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new()),
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 1000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    );
    handler.handle(
        &SessionId::Named("t".into()),
        &RpcRequest::new(1, method, params),
    )
}

fn manifest() -> Value {
    let r = call("tools.manifest", json!({}));
    r.result.unwrap_or_else(|| panic!("failed: {:?}", r.error))
}

#[test]
fn the_daemon_states_every_tool_it_knows() {
    let m = manifest();
    let tools = m["tools"].as_array().expect("a tools array");
    assert_eq!(
        tools.len(),
        logmon_broker_protocol::mcp_tools::TOOLS.len(),
        "the manifest must describe the whole surface, not part of it"
    );
    assert_eq!(
        m["protocol_version"],
        logmon_broker_protocol::PROTOCOL_VERSION
    );
    assert!(
        m["broker_version"].is_string(),
        "a shim needs to know which daemon taught it, to report skew"
    );
}

/// The sufficiency claim, stated as an assertion: name, description and schema
/// are exactly what `ToolRoute::new_dyn` needs, so if all three are present for
/// every tool, a generic shim can register the lot.
#[test]
fn every_entry_carries_what_it_takes_to_register_a_tool() {
    let m = manifest();
    let mut incomplete = Vec::new();
    for t in m["tools"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap_or("<unnamed>");
        if t["method"].as_str().is_none_or(str::is_empty) {
            incomplete.push(format!("{name}: no method"));
        }
        if t["description"].as_str().is_none_or(str::is_empty) {
            incomplete.push(format!("{name}: no description"));
        }
        if !t["input_schema"].is_object() {
            incomplete.push(format!("{name}: no input_schema"));
        }
    }
    assert!(
        incomplete.is_empty(),
        "a shim could not build these tools from the manifest:\n  {}",
        incomplete.join("\n  ")
    );
}

/// A parameter added to the daemon has to appear here, or the whole exercise is
/// pointless: the shim would still need a rebuild to reach it.
#[test]
fn a_tools_parameters_reach_the_manifest() {
    let m = manifest();
    let add = m["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "add_collector")
        .expect("add_collector is in the manifest");

    let props = add["input_schema"]["properties"]
        .as_object()
        .expect("its parameters are described");
    for expected in ["name", "filter", "level", "group_keys", "threshold"] {
        assert!(
            props.contains_key(expected),
            "`{expected}` is a real add_collector parameter but the manifest omits it"
        );
    }

    // And the closed sets Phase A declared travel with it, so a shim can
    // validate or offer completions without knowing anything about logmon.
    let level = &props["level"]["enum"];
    assert!(
        level
            .as_array()
            .is_some_and(|v| v.iter().any(|x| x == "tree")),
        "the accepted values must reach the shim too, not just the field name: {level}"
    );
}

/// The manifest describes the protocol, not the caller, so it must not depend
/// on session state — including a session that was never created.
#[test]
fn the_manifest_does_not_depend_on_the_session() {
    let first = manifest();
    let second = manifest();
    assert_eq!(first, second, "the same question must get the same answer");
}
