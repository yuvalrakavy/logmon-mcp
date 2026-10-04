//! A session the TTL sweep disposes of stops sizing the pre-trigger buffer — in every domain,
//! not only `default` (gh #25). Driven through `RpcHandler::dispose_expired_session`, the
//! sweep's whole per-session body, because the sweep itself runs at most once a minute.

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
use logmon_broker_protocol::RpcRequest;
use serde_json::json;
use std::sync::Arc;

fn domain(name: DomainId, seq: Arc<SeqCounter>) -> (Arc<Domain>, Arc<LogPipeline>) {
    let pipeline = Arc::new(LogPipeline::new_with_seq_counter(10_000, seq.clone()));
    let d = Arc::new(Domain::from_parts(
        DomainConfig {
            name,
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 10_000,
            span_buffer_size: 1000,
            source: DomainSource::Config,
        },
        pipeline.clone(),
        Arc::new(SpanStore::new(1000, seq)),
        Arc::new(BookmarkStore::new()),
        Arc::new(ReceiverMetrics::new()),
    ));
    (d, pipeline)
}

fn add_big_trigger(handler: &RpcHandler, id: &SessionId) {
    let req = RpcRequest::new(
        1,
        "triggers.add",
        json!({ "filter": "m=__never_fires__", "pre_window": 3000 }),
    );
    let resp = handler.handle(id, &req);
    assert!(resp.error.is_none(), "{:?}", resp.error);
}

fn single_domain_handler() -> (
    RpcHandler,
    Arc<SessionRegistry>,
    Arc<logmon_broker_core::collector::registry::CollectorRegistry>,
) {
    let seq = Arc::new(SeqCounter::new());
    let domains = Arc::new(DomainRegistry::new());
    domains.insert(domain(DomainId::default_domain(), seq).0);
    let sessions = Arc::new(SessionRegistry::new());
    let collectors = Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new());
    let handler = RpcHandler::new(
        domains,
        sessions.clone(),
        collectors.clone(),
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 10_000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    );
    (handler, sessions, collectors)
}

/// The sweep lists expired sessions and disposes of them a moment later. One that reconnected
/// in between is live: it keeps its bookmarks and stays registered.
#[test]
fn a_session_that_reconnected_after_the_sweep_listed_it_is_not_disposed() {
    let (handler, sessions, _) = single_domain_handler();
    let id = sessions.create_named("comeback").unwrap();
    let req = RpcRequest::new(1, "bookmarks.add", json!({ "name": "mark" }));
    assert!(handler.handle(&id, &req).error.is_none());
    sessions.disconnect(&id);
    // (The sweep would list it now.) It reconnects before the disposal.
    sessions.reconnect(&id).unwrap();
    assert_eq!(handler.dispose_expired_session(&id), (0, 0));
    assert!(sessions.is_connected(&id), "still registered and connected");
    let listed = handler.handle(&id, &RpcRequest::new(2, "bookmarks.list", json!({})));
    let bookmarks = listed.result.unwrap()["bookmarks"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(bookmarks, 1, "its bookmark survived");
}

/// `sessions.drop` on a CONNECTED session is refused before anything of it is released — it
/// used to destroy the session's collectors and reply `dropped` while the session stayed.
#[test]
fn dropping_a_connected_session_leaves_its_collectors_alone() {
    let (handler, sessions, collectors) = single_domain_handler();
    let live = sessions.create_named("live").unwrap();
    let req = RpcRequest::new(1, "collectors.add", json!({ "name": "c", "filter": "ALL" }));
    assert!(handler.handle(&live, &req).error.is_none());
    let reserved = collectors.reserved_bytes();
    assert!(reserved > 0);

    let other = sessions.create_named("other").unwrap();
    let resp = handler.handle(
        &other,
        &RpcRequest::new(2, "sessions.drop", json!({ "name": "live" })),
    );
    assert!(resp.error.is_some(), "a connected session is refused");
    assert_eq!(
        collectors.reserved_bytes(),
        reserved,
        "its collector is intact"
    );
}

#[test]
fn an_expired_session_stops_sizing_the_buffer_in_every_domain() {
    let seq = Arc::new(SeqCounter::new());
    let domains = Arc::new(DomainRegistry::new());
    let (default, default_pipeline) = domain(DomainId::default_domain(), seq.clone());
    let other_id = DomainId::new("other").unwrap();
    let (other, other_pipeline) = domain(other_id.clone(), seq);
    domains.insert(default);
    domains.insert(other);
    let sessions = Arc::new(SessionRegistry::new());
    let handler = RpcHandler::new(
        domains,
        sessions.clone(),
        Arc::new(logmon_broker_core::collector::registry::CollectorRegistry::new()),
        vec!["test".into()],
        DomainPolicy {
            max_domains: 32,
            default_log_buffer_size: 10_000,
            default_span_buffer_size: 1000,
            stale_after_secs: 60,
        },
    );

    // One session per domain with a 3,000-record window; each domain also keeps a session with
    // only the default 500.
    let big_default = sessions.create_named("big-default").unwrap();
    let big_other = sessions.create_named("big-other").unwrap();
    sessions.set_domain(&big_other, other_id.clone());
    let keeper_other = sessions.create_named("keeper-other").unwrap();
    sessions.set_domain(&keeper_other, other_id);
    let _keeper_default = sessions.create_named("keeper-default").unwrap();
    add_big_trigger(&handler, &big_default);
    add_big_trigger(&handler, &big_other);
    assert_eq!(default_pipeline.pre_buffer_capacity(), 3000);
    assert_eq!(other_pipeline.pre_buffer_capacity(), 3000);

    for id in [&big_default, &big_other] {
        sessions.disconnect(id);
        handler.dispose_expired_session(id);
    }
    assert_eq!(default_pipeline.pre_buffer_capacity(), 500);
    assert_eq!(other_pipeline.pre_buffer_capacity(), 500);
}
