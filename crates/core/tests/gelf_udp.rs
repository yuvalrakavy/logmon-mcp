use logmon_broker_core::gelf::udp::start_udp_listener;
use logmon_broker_core::gelf::message::LogEntry;
use std::net::UdpSocket;
use tokio::time::{sleep, Duration};
use serde_json::json;

#[tokio::test]
async fn test_udp_listener_receives_gelf() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(100);
    let handle = start_udp_listener("127.0.0.1:0", tx).await.unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let msg = json!({
        "version": "1.1", "host": "test",
        "short_message": "hello from udp", "level": 6
    });
    socket.send_to(msg.to_string().as_bytes(), format!("127.0.0.1:{}", handle.port())).unwrap();

    sleep(Duration::from_millis(100)).await;
    let entry = rx.try_recv().unwrap();
    assert_eq!(entry.message, "hello from udp");
    assert_eq!(entry.seq, 0); // listener doesn't assign seq
}

#[tokio::test]
async fn test_udp_malformed_not_sent() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(100);
    let handle = start_udp_listener("127.0.0.1:0", tx).await.unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.send_to(b"not json", format!("127.0.0.1:{}", handle.port())).unwrap();

    sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err()); // nothing sent for malformed
}
