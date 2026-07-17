//! Domain binding (Wave 2 stage 2.3): `domains.use` + the `session.start`
//! `domain` param + event re-subscribe. A session's queries and live trigger
//! notifications follow its binding; `default` is the connect-time default.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{DomainInfo, LogsRecentResult, TriggersAddResult};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;

/// Send one GELF/UDP datagram at the given syslog level (6=info, 3=error).
async fn send_gelf_level(port: u16, msg: &str, level: u8) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let payload =
        format!(r#"{{"version":"1.1","host":"test","short_message":"{msg}","level":{level}}}"#);
    sock.send_to(payload.as_bytes(), format!("127.0.0.1:{port}"))
        .await
        .unwrap();
}

async fn send_gelf(port: u16, msg: &str) {
    send_gelf_level(port, msg, 6).await;
}

/// Poll `logs.recent` until a message containing `needle` appears, returning all
/// messages seen on the last poll.
async fn wait_for_log(client: &mut TestClient, needle: &str) -> Vec<String> {
    let mut msgs = Vec::new();
    for _ in 0..40 {
        let r: LogsRecentResult = client
            .call("logs.recent", json!({ "count": 50 }))
            .await
            .unwrap();
        msgs = r.logs.iter().map(|l| l.message.clone()).collect();
        if msgs.iter().any(|m| m.contains(needle)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    msgs
}

#[tokio::test]
async fn use_domain_scopes_queries_to_the_bound_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    // A log in default (via the injected channel).
    daemon.inject_log(Level::Info, "in default").await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();

    // Bind this session to t3.
    client
        .call::<Value>("domains.use", json!({ "name": "t3" }))
        .await
        .expect("domains.use t3");

    // A log ingested on t3's port; the t3-bound session sees it...
    send_gelf(t3.gelf_port, "in t3").await;
    let msgs = wait_for_log(&mut client, "in t3").await;
    assert!(
        msgs.iter().any(|m| m.contains("in t3")),
        "t3-bound query sees t3's log: {msgs:?}"
    );
    // ...and does NOT see default's log.
    assert!(
        !msgs.iter().any(|m| m.contains("in default")),
        "t3-bound query must not see default's log: {msgs:?}"
    );
}

#[tokio::test]
async fn use_domain_rejects_unknown_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    let result = client
        .call::<Value>("domains.use", json!({ "name": "nope" }))
        .await;
    assert!(result.is_err(), "binding a non-existent domain must error");
}

#[tokio::test]
async fn trigger_notifications_follow_use_domain() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    client
        .call::<Value>("domains.use", json!({ "name": "t3" }))
        .await
        .expect("bind t3");

    // A trigger on this (now t3-bound) session, firing on ERROR.
    let add: TriggersAddResult = client
        .call(
            "triggers.add",
            json!({ "filter": "l>=ERROR", "pre_window": 0, "post_window": 0, "notify_context": 0 }),
        )
        .await
        .expect("add trigger");

    // A matching ERROR log ingested on t3's port must reach this session's
    // notification stream — which requires the connection's event subscription
    // to have followed the rebind from default's channel to t3's (§9.4).
    send_gelf_level(t3.gelf_port, "boom on t3", 3).await;

    let notif = client.expect_trigger_fired(add.id).await;
    assert_eq!(
        notif.params.get("trigger_id").and_then(|v| v.as_u64()),
        Some(add.id as u64),
        "trigger_fired for the t3 log reached the t3-bound session"
    );
}

#[tokio::test]
async fn session_start_domain_param_binds_at_connect() {
    let daemon = spawn_test_daemon().await;

    // Create t3 and grab its GELF port via a throwaway client (t3 persists after
    // that client disconnects — domains are daemon-global).
    let t3_port = {
        let mut setup = daemon.connect_anon().await;
        let t3: DomainInfo = setup
            .call("domains.create", json!({ "name": "t3" }))
            .await
            .unwrap();
        t3.gelf_port
    };

    daemon.inject_log(Level::Info, "in default").await;
    send_gelf(t3_port, "in t3").await;

    // A NEW client bound to t3 AT CONNECT (no domains.use round-trip).
    let mut client = daemon.connect_with_domain(None, "t3").await;
    let msgs = wait_for_log(&mut client, "in t3").await;
    assert!(
        msgs.iter().any(|m| m.contains("in t3")),
        "connect-time-bound query sees t3's log: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("in default")),
        "connect-time-bound query must not see default's log: {msgs:?}"
    );
}

#[tokio::test]
async fn session_start_unknown_domain_errors_the_handshake() {
    let daemon = spawn_test_daemon().await;
    let result =
        TestClient::try_connect(&daemon.socket_path, None, None, Some("ghost".into())).await;
    assert!(
        result.is_err(),
        "connecting with an unknown domain must fail the handshake"
    );
}

#[tokio::test]
async fn delete_while_bound_keeps_session_alive_for_rebind() {
    let daemon = spawn_test_daemon().await;
    let mut a = daemon.connect_anon().await;

    a.call::<DomainInfo>("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    a.call::<Value>("domains.use", json!({ "name": "t3" }))
        .await
        .unwrap();
    // Delete the domain A is currently bound to.
    a.call::<Value>("domains.delete", json!({ "name": "t3" }))
        .await
        .unwrap();

    // Let A's event channel observe the teardown + re-subscribe.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The connection SURVIVED: a query surfaces the vanished-domain error (no
    // silent fallback)...
    let q = a
        .call::<LogsRecentResult>("logs.recent", json!({ "count": 5 }))
        .await;
    assert!(
        q.is_err(),
        "query on the deleted bound domain errors — no silent fallback"
    );
    // ...and A can rebind to default and query, proving the connection stayed
    // alive (a disconnect would have failed these calls).
    a.call::<DomainInfo>("domains.use", json!({ "name": "default" }))
        .await
        .expect("rebind to default on a still-alive connection");
    let _: LogsRecentResult = a
        .call("logs.recent", json!({ "count": 5 }))
        .await
        .expect("query works after rebind");
}

/// A non-default → different-domain rebind carries `rebind_warning` — the
/// signature of two clients sharing one session (two agent conversations
/// behind one MCP server process, each rebinding the other's reads). The
/// normal lifecycle stays silent: default → domain, then idempotent re-binds.
/// The bind itself still succeeds (a deliberate switch is legal); the warning
/// rides the RESPONSE so both consumers of a shared stream see it.
#[tokio::test]
async fn a_cross_domain_rebind_warns_but_still_binds() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    for name in ["t1", "t2"] {
        let _: DomainInfo = client
            .call("domains.create", json!({ "name": name }))
            .await
            .unwrap();
    }

    // default → t1: the normal lifecycle, no warning.
    let r: Value = client
        .call("domains.use", json!({ "name": "t1" }))
        .await
        .expect("bind t1");
    assert!(
        r.get("rebind_warning").is_none(),
        "default → t1 is the normal lifecycle, must not warn: {r}"
    );

    // t1 → t1: idempotent re-bind (the recommended bind-before-every-burst
    // workaround), no warning.
    let r: Value = client
        .call("domains.use", json!({ "name": "t1" }))
        .await
        .expect("re-bind t1");
    assert!(
        r.get("rebind_warning").is_none(),
        "an idempotent re-bind must not warn: {r}"
    );

    // t1 → t2: the shared-session signature — warned, AND still bound.
    let r: Value = client
        .call("domains.use", json!({ "name": "t2" }))
        .await
        .expect("bind t2");
    let warning = r
        .get("rebind_warning")
        .and_then(|w| w.as_str())
        .expect("a non-default → non-default rebind must carry rebind_warning");
    assert!(
        warning.contains("t1") && warning.contains("t2"),
        "the warning must name both domains: {warning}"
    );
    assert!(
        warning.contains("--session"),
        "the warning must carry the remedy: {warning}"
    );
    assert_eq!(
        r.get("name").and_then(|v| v.as_str()),
        Some("t2"),
        "the bind must still succeed — warn, not refuse: {r}"
    );

    // t2 → default: the session was bound to a non-default domain and the
    // binding is being replaced — still the collision signature, still warned.
    let r: Value = client
        .call("domains.use", json!({ "name": "default" }))
        .await
        .expect("bind default");
    assert!(
        r.get("rebind_warning").is_some(),
        "replacing a non-default binding warns even when unwinding to default: {r}"
    );
}

// ---------------------------------------------------------------------------
// Meaningful session names (spec docs/2026-07-17-meaningful-session-names-spec.md)
// ---------------------------------------------------------------------------

/// session.rename preserves ALL session state: the domain binding and a live
/// trigger survive the re-key, and the connection keeps working under the new
/// identity (the loop re-keys itself off the echoed name).
#[tokio::test]
async fn rename_preserves_binding_and_triggers() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;

    let t3: DomainInfo = client
        .call("domains.create", json!({ "name": "t3" }))
        .await
        .unwrap();
    client
        .call::<Value>("domains.use", json!({ "name": "t3" }))
        .await
        .unwrap();
    let _: TriggersAddResult = client
        .call("triggers.add", json!({ "filter": "l>=ERROR" }))
        .await
        .unwrap();

    let r: Value = client
        .call("session.rename", json!({ "name": "Store-t3-feat-thing" }))
        .await
        .expect("rename");
    assert_eq!(r.get("name").and_then(|v| v.as_str()), Some("Store-t3-feat-thing"));
    assert!(r.get("displaced_stale_holder").is_none(), "nothing displaced: {r}");

    // The binding survived: a t3-port log is visible without re-binding.
    send_gelf(t3.gelf_port, "after rename").await;
    let msgs = wait_for_log(&mut client, "after rename").await;
    assert!(
        msgs.iter().any(|m| m.contains("after rename")),
        "the t3 binding must survive the rename: {msgs:?}"
    );

    // The registry lists the new name, and domains.list attributes the domain
    // to it (derived bound_sessions).
    let sessions: Value = client.call("session.list", json!({})).await.unwrap();
    let names: Vec<String> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(String::from))
        .collect();
    assert!(
        names.contains(&"Store-t3-feat-thing".to_string()),
        "renamed session must appear by name: {names:?}"
    );
    let domains: Value = client.call("domains.list", json!({})).await.unwrap();
    let t3_entry = domains["domains"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "t3")
        .expect("t3 listed");
    let bound: Vec<&str> = t3_entry["bound_sessions"]
        .as_array()
        .expect("bound_sessions populated")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        bound.iter().any(|b| b.contains("Store-t3-feat-thing")),
        "domains.list must attribute t3 to the renamed session: {bound:?}"
    );
}

/// The one-conversation-per-lane invariant: renaming to a name held by a
/// CONNECTED session errors, and the caller's identity/binding are unchanged.
#[tokio::test]
async fn rename_to_a_live_holder_errors_and_changes_nothing() {
    let daemon = spawn_test_daemon().await;
    let mut holder = daemon.connect_anon().await;
    let mut challenger = daemon.connect_anon().await;

    let _: Value = holder
        .call("session.rename", json!({ "name": "Store-t1-feat-x" }))
        .await
        .expect("holder takes the name");

    let result = challenger
        .call::<Value>("session.rename", json!({ "name": "Store-t1-feat-x" }))
        .await;
    let err = format!("{:?}", result.expect_err("a live holder must refuse the rename"));
    assert!(
        err.contains("already connected"),
        "the error must say the name is actively held: {err}"
    );

    // The challenger still works under its old identity (a follow-up call
    // succeeds and the registry still shows exactly one session by that name).
    let sessions: Value = challenger.call("session.list", json!({})).await.unwrap();
    let named: Vec<&str> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| *n == "Store-t1-feat-x")
        .collect();
    assert_eq!(named.len(), 1, "exactly one holder of the name: {sessions}");
}

/// A DISCONNECTED holder is displaced: a dead conversation must not lock a
/// lane name forever. The displacement is reported, and the stale session is
/// gone from the registry.
#[tokio::test]
async fn rename_displaces_a_disconnected_holder() {
    let daemon = spawn_test_daemon().await;

    // A named session that then disconnects (named sessions persist). The
    // close must be EXPLICIT: dropping a TestClient leaves the socket open
    // (see TestClient::close).
    {
        let mut doomed = daemon.connect_anon().await;
        let _: Value = doomed
            .call("session.rename", json!({ "name": "Store-t2-feat-old" }))
            .await
            .unwrap();
        doomed.close().await.expect("close");
    }
    // Disconnect is EOF-driven and async — wait for the registry to see it.
    let mut claimer = daemon.connect_anon().await;
    let mut disconnected = false;
    for _ in 0..40 {
        let sessions: Value = claimer.call("session.list", json!({})).await.unwrap();
        disconnected = sessions["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "Store-t2-feat-old" && s["connected"] == false);
        if disconnected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(disconnected, "the dropped holder must read disconnected");

    let r: Value = claimer
        .call("session.rename", json!({ "name": "Store-t2-feat-old" }))
        .await
        .expect("a stale holder must be displaced, not block");
    assert_eq!(
        r.get("displaced_stale_holder").and_then(|v| v.as_bool()),
        Some(true),
        "the displacement must be reported: {r}"
    );

    // Exactly one session by that name, and it is connected (the claimer).
    let sessions: Value = claimer.call("session.list", json!({})).await.unwrap();
    let holders: Vec<&Value> = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["name"] == "Store-t2-feat-old")
        .collect();
    assert_eq!(holders.len(), 1, "{sessions}");
    assert_eq!(holders[0]["connected"], true);
}

/// Invalid target names are rejected — including what an unsanitized branch
/// name would produce ('/').
#[tokio::test]
async fn rename_rejects_invalid_names() {
    let daemon = spawn_test_daemon().await;
    let mut client = daemon.connect_anon().await;
    for bad in ["Store-t1-feat/thing", "", "has space"] {
        let result = client
            .call::<Value>("session.rename", json!({ "name": bad }))
            .await;
        assert!(result.is_err(), "{bad:?} must be rejected");
    }
}
