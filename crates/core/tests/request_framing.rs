//! A request that arrives in pieces survives a notification sent between them.
//!
//! The connection loop races reading the next request against forwarding trigger
//! notifications. Each read used to go into a fresh buffer, so when a notification won that
//! race mid-request the bytes already read were dropped, the rest of the line failed to parse,
//! and the daemon closed the connection. Clients write a request and its newline as separate
//! writes, so a trigger firing between them was enough.
#![cfg(feature = "test-support")]

use logmon_broker_core::gelf::message::Level;
use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{RpcRequest, PROTOCOL_VERSION};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const WAIT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_split_around_a_notification_is_still_answered() {
    let daemon = spawn_test_daemon().await;
    let stream = tokio::net::UnixStream::connect(&daemon.socket_path)
        .await
        .expect("connect");
    let (r, mut w) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();

    let mut start = serde_json::to_vec(&RpcRequest::new(
        1,
        "session.start",
        json!({ "name": "split", "protocol_version": PROTOCOL_VERSION }),
    ))
    .unwrap();
    start.push(b'\n');
    w.write_all(&start).await.unwrap();
    tokio::time::timeout(WAIT, lines.next_line())
        .await
        .expect("the handshake reply in time")
        .unwrap()
        .expect("the handshake reply");

    // The first half of a request, then a pause long enough for the daemon to read it. (Were
    // the daemon slower, the half would still be unread when the notification goes out, and
    // the test would pass without exercising the race — never fail for a wrong reason.)
    let request = serde_json::to_vec(&RpcRequest::new(2, "status.get", json!({}))).unwrap();
    let half = request.len() / 2;
    w.write_all(&request[..half]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // An ERROR fires the session's default trigger; its notification reaches the client while
    // the request is still incomplete.
    daemon.inject_log(Level::Error, "boom").await;
    let notification: Value = serde_json::from_str(
        &tokio::time::timeout(WAIT, lines.next_line())
            .await
            .expect("the notification in time")
            .unwrap()
            .expect("the notification, not a closed connection"),
    )
    .unwrap();
    assert_eq!(
        notification["method"],
        json!("trigger_fired"),
        "{notification}"
    );

    w.write_all(&request[half..]).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    let reply = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .expect("the reply in time")
        .unwrap()
        .expect("a reply — the daemon closed the connection instead");
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["id"], json!(2), "{reply}");
    assert!(reply.get("error").is_none_or(Value::is_null), "{reply}");
}
