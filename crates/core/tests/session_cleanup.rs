//! A connection disconnects its session on every exit — including the one where a write to a
//! client that has already gone fails.
//!
//! The writes in `handle_connection` return through `?` when the client has gone, and the
//! disconnect used to sit at the end of the function, so it was skipped: a named session stayed
//! `connected` and its name was refused until the broker restarted. To make that write fail on
//! every run rather than by luck, the client connects while the accept loop is paused, sends
//! `session.start`, and closes — so the daemon's first write, the `session.start` reply, goes
//! to a peer that is gone.
#![cfg(feature = "test-support")]

use logmon_broker_core::test_support::*;
use logmon_broker_protocol::{RpcRequest, PROTOCOL_VERSION};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A `sessions.rename` whose reply cannot be written still leaves the session under its NEW
/// name disconnected. The rename commits before the reply is written; the cleanup used to be
/// re-keyed only after that write, so a failed write disconnected the OLD id (gone) and left
/// the new name connected for good. The client reads the handshake reply, sends the rename and
/// closes at once — the daemon writes the reply after the close, in practice always (the close
/// is the next syscall; the reply needs a wake-up and the rename), and a reply written before
/// the close would only make this run pass for the wrong reason, never fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_rename_reply_still_disconnects_the_session_by_its_new_name() {
    use tokio::io::AsyncBufReadExt;
    let daemon = spawn_test_daemon().await;
    let stream = tokio::net::UnixStream::connect(&daemon.socket_path)
        .await
        .expect("connect");
    let (r, mut w) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    for (id, method, params) in [
        (
            1,
            "session.start",
            serde_json::json!({ "name": "orig", "protocol_version": PROTOCOL_VERSION }),
        ),
        (
            2,
            "sessions.rename",
            serde_json::json!({ "name": "renamed" }),
        ),
    ] {
        let mut line = serde_json::to_vec(&RpcRequest::new(id, method, params)).unwrap();
        line.push(b'\n');
        w.write_all(&line).await.unwrap();
        if id == 1 {
            lines
                .next_line()
                .await
                .unwrap()
                .expect("the handshake reply");
        }
    }
    drop(w);
    drop(lines);

    for _ in 0..200 {
        if let Ok(c) = daemon.try_connect_named("renamed", None).await {
            c.close().await.unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the session `renamed` is still connected after its client went away");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_reply_write_still_disconnects_the_named_session() {
    let daemon = spawn_test_daemon().await;
    daemon.pause_accept().await;

    let mut stream = tokio::net::UnixStream::connect(&daemon.socket_path)
        .await
        .expect("connect while paused");
    let start = RpcRequest::new(
        1,
        "session.start",
        serde_json::json!({ "name": "gone", "protocol_version": PROTOCOL_VERSION }),
    );
    let mut line = serde_json::to_vec(&start).unwrap();
    line.push(b'\n');
    stream.write_all(&line).await.unwrap();
    drop(stream);

    daemon.resume_accept().await;

    // Without the disconnect the name stays held by a session nothing is serving.
    for _ in 0..200 {
        if let Ok(c) = daemon.try_connect_named("gone", None).await {
            c.close().await.unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the session `gone` is still connected after its client went away");
}
