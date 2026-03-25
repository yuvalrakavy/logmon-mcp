use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use serde_json::json;
use gelf_mcp_server::engine::pipeline::LogPipeline;

#[tokio::test]
async fn test_tcp_listener_receives_gelf() {
    let pipeline = Arc::new(LogPipeline::new(1000));

    let handle = gelf_mcp_server::gelf::tcp::start_tcp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    // Connect and send null-delimited GELF messages
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
    let msg1 = json!({"version":"1.1","host":"tcp-test","short_message":"msg1","level":6});
    let msg2 = json!({"version":"1.1","host":"tcp-test","short_message":"msg2","level":4});

    stream.write_all(msg1.to_string().as_bytes()).await.unwrap();
    stream.write_all(b"\0").await.unwrap();
    stream.write_all(msg2.to_string().as_bytes()).await.unwrap();
    stream.write_all(b"\0").await.unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store_len(), 2);
}

#[tokio::test]
async fn test_tcp_multiple_clients() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let handle = gelf_mcp_server::gelf::tcp::start_tcp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    for i in 0..3 {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
        let msg = json!({"version":"1.1","host":"client","short_message":format!("from {i}"),"level":6});
        stream.write_all(msg.to_string().as_bytes()).await.unwrap();
        stream.write_all(b"\0").await.unwrap();
        // Small delay to ensure connection is processed
        sleep(Duration::from_millis(50)).await;
    }

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store_len(), 3);
}

#[tokio::test]
async fn test_tcp_malformed_message() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let handle = gelf_mcp_server::gelf::tcp::start_tcp_listener(
        "127.0.0.1:0", pipeline.clone()
    ).await.unwrap();

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
    stream.write_all(b"not valid json\0").await.unwrap();

    sleep(Duration::from_millis(100)).await;
    assert_eq!(pipeline.store_len(), 0);
    let stats = pipeline.store_stats();
    assert!(stats.malformed_count > 0);
}
