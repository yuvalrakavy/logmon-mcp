use std::net::UdpSocket;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use serde_json::json;
use logmon_mcp_server::engine::pipeline::LogPipeline;

#[tokio::test]
async fn test_udp_listener_receives_gelf() {
    let pipeline = Arc::new(LogPipeline::new(1000));

    let handle = logmon_mcp_server::gelf::udp::start_udp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    // Send a GELF message via UDP
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let msg = json!({
        "version": "1.1",
        "host": "test",
        "short_message": "hello from udp",
        "level": 6
    });
    socket.send_to(
        msg.to_string().as_bytes(),
        format!("127.0.0.1:{}", handle.port())
    ).unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store_len(), 1);
    let logs = pipeline.recent_logs(1, None);
    assert_eq!(logs[0].message, "hello from udp");
}

#[tokio::test]
async fn test_udp_malformed_message_counted() {
    let pipeline = Arc::new(LogPipeline::new(1000));

    let handle = logmon_mcp_server::gelf::udp::start_udp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.send_to(b"not valid json", format!("127.0.0.1:{}", handle.port())).unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store_len(), 0);
    // Malformed count should be incremented
    let stats = pipeline.store_stats();
    assert!(stats.malformed_count > 0);
}
