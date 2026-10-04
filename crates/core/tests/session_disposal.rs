//! A session the TTL sweep disposes of stops sizing the pre-trigger buffer — in every domain,
//! not only `default` (gh #25) — and the sweep and `sessions.drop` dispose of a session only
//! while it is abandoned. Driven through `RpcHandler::dispose_expired_session`, the sweep's
//! whole per-session body, because the sweep itself runs at most once a minute; a session's
//! age is set with `backdate_last_seen` rather than slept through.
#![cfg(feature = "test-support")]

use logmon_broker_core::daemon::domain::{
    Domain, DomainConfig, DomainId, DomainRegistry, DomainSource,
};
use logmon_broker_core::daemon::rpc_handler::{DomainPolicy, ExpiredDisposal, RpcHandler};
use logmon_broker_core::daemon::session::{SessionId, SessionRegistry};
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::store::bookmarks::BookmarkStore;
use logmon_broker_protocol::RpcRequest;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const TTL: Duration = Duration::from_secs(60);
/// Twice the TTL: a session last seen this long ago is abandoned.
const LONG_AGO: Duration = Duration::from_secs(120);

fn count(handler: &RpcHandler, id: &SessionId, method: &str, key: &str) -> usize {
    let resp = handler.handle(id, &RpcRequest::new(9, method, json!({})));
    resp.result.unwrap()[key].as_array().unwrap().len()
}

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
    sessions.backdate_last_seen(&id, LONG_AGO);
    assert!(
        sessions.expired_disconnected(TTL).contains(&id),
        "the sweep lists it"
    );
    // It reconnects before the disposal.
    sessions.reconnect(&id).unwrap();
    assert_eq!(
        handler.dispose_expired_session(&id, TTL),
        ExpiredDisposal::NotAbandoned
    );
    assert!(sessions.is_connected(&id), "still registered and connected");
    assert_eq!(
        count(&handler, &id, "bookmarks.list", "bookmarks"),
        1,
        "its bookmark survived"
    );
}

/// One that reconnected AND left again before the disposal is no longer abandoned either —
/// last seen a moment ago — and keeps what it made meanwhile. Checking only that it was
/// disconnected disposed of it, collector and all.
#[test]
fn a_session_active_again_since_the_listing_is_not_disposed() {
    let (handler, sessions, collectors) = single_domain_handler();
    let id = sessions.create_named("cli").unwrap();
    sessions.disconnect(&id);
    sessions.backdate_last_seen(&id, LONG_AGO);
    assert!(
        sessions.expired_disconnected(TTL).contains(&id),
        "the sweep lists it"
    );

    // A CLI invocation as this session: connect, arm a collector, leave.
    sessions.reconnect(&id).unwrap();
    let req = RpcRequest::new(1, "collectors.add", json!({ "name": "c", "filter": "ALL" }));
    assert!(handler.handle(&id, &req).error.is_none());
    sessions.disconnect(&id);
    let reserved = collectors.reserved_bytes();
    assert!(reserved > 0);

    assert_eq!(
        handler.dispose_expired_session(&id, TTL),
        ExpiredDisposal::NotAbandoned
    );
    assert_eq!(
        collectors.reserved_bytes(),
        reserved,
        "its collector is intact"
    );
    assert_eq!(count(&handler, &id, "collectors.list", "collectors"), 1);

    // The instrument fires: abandoned for real, it is disposed of.
    sessions.backdate_last_seen(&id, LONG_AGO);
    assert_eq!(
        handler.dispose_expired_session(&id, TTL),
        ExpiredDisposal::Disposed {
            bookmarks: 0,
            collectors: 1
        }
    );
    assert_eq!(collectors.reserved_bytes(), 0);
    // And a session already gone is reported so, not as kept.
    assert_eq!(
        handler.dispose_expired_session(&id, TTL),
        ExpiredDisposal::Gone
    );
}

/// `sessions.drop` given a name no session holds reclaims nothing keyed by the bare name — the
/// name may be a live anonymous session's id (`sessions.list` shows those), and that session's
/// bookmarks and cursors used to be wiped while the drop replied "not found".
#[test]
fn dropping_an_anonymous_session_s_id_leaves_its_bookmarks_alone() {
    let (handler, sessions, _) = single_domain_handler();
    let anon = sessions.create_anonymous();
    let add = RpcRequest::new(1, "bookmarks.add", json!({ "name": "mark" }));
    assert!(handler.handle(&anon, &add).error.is_none());

    let other = sessions.create_named("other").unwrap();
    let resp = handler.handle(
        &other,
        &RpcRequest::new(2, "sessions.drop", json!({ "name": anon.to_string() })),
    );
    assert!(resp.error.is_some(), "no named session holds that name");
    assert_eq!(
        count(&handler, &anon, "bookmarks.list", "bookmarks"),
        1,
        "the anonymous session's bookmark survived"
    );
}

/// `sessions.drop` clears the session's bookmarks. A later session of the same name used to
/// inherit them: its cursors resumed from the old positions and `bookmarks.add` refused names
/// it had never used.
#[test]
fn a_dropped_session_leaves_no_bookmarks_to_the_next_holder_of_its_name() {
    let (handler, sessions, _) = single_domain_handler();
    let gone = sessions.create_named("gone").unwrap();
    let add = RpcRequest::new(1, "bookmarks.add", json!({ "name": "mark" }));
    assert!(handler.handle(&gone, &add).error.is_none());
    sessions.disconnect(&gone);

    let other = sessions.create_named("other").unwrap();
    let resp = handler.handle(
        &other,
        &RpcRequest::new(2, "sessions.drop", json!({ "name": "gone" })),
    );
    assert!(resp.error.is_none(), "{:?}", resp.error);

    let again = sessions.create_named("gone").unwrap();
    assert_eq!(count(&handler, &again, "bookmarks.list", "bookmarks"), 0);
    let resp = handler.handle(&again, &add);
    assert!(resp.error.is_none(), "a fresh `mark`: {:?}", resp.error);
}

/// A rename carries the session's bookmarks to the new name, positions intact. Left under the
/// old name, its cursors auto-created at 0 under the new one and replayed everything they had
/// already returned.
#[test]
fn a_rename_carries_the_session_s_bookmarks() {
    let (handler, sessions, _) = single_domain_handler();
    let before = sessions.create_named("before").unwrap();
    let add = RpcRequest::new(
        1,
        "bookmarks.add",
        json!({ "name": "mark", "start_seq": 42 }),
    );
    assert!(handler.handle(&before, &add).error.is_none());

    let rename = RpcRequest::new(2, "sessions.rename", json!({ "name": "after" }));
    assert!(handler.handle(&before, &rename).error.is_none());

    let after = SessionId::Named("after".into());
    let listed = handler.handle(&after, &RpcRequest::new(3, "bookmarks.list", json!({})));
    let listed = listed.result.unwrap();
    let marks = listed["bookmarks"].as_array().unwrap();
    assert_eq!(marks.len(), 1, "{listed}");
    assert_eq!(marks[0]["qualified_name"], json!("after/mark"), "{listed}");
    assert_eq!(
        marks[0]["seq"],
        json!(42),
        "its position came with it: {listed}"
    );
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
        sessions.backdate_last_seen(id, LONG_AGO);
        assert!(matches!(
            handler.dispose_expired_session(id, TTL),
            ExpiredDisposal::Disposed { .. }
        ));
    }
    assert_eq!(default_pipeline.pre_buffer_capacity(), 500);
    assert_eq!(other_pipeline.pre_buffer_capacity(), 500);
}
