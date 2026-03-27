use logmon_mcp_server::gelf::tcp::start_tcp_listener;
use logmon_mcp_server::gelf::message::LogEntry;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use serde_json::json;

#[tokio::test]
async fn test_tcp_listener_receives_gelf() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(100);
    let handle = start_tcp_listener("127.0.0.1:0", tx).await.unwrap();

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
    let msg = json!({"version":"1.1","host":"tcp-test","short_message":"msg1","level":6});
    stream.write_all(msg.to_string().as_bytes()).await.unwrap();
    stream.write_all(b"\0").await.unwrap();

    sleep(Duration::from_millis(100)).await;
    let entry = rx.try_recv().unwrap();
    assert_eq!(entry.message, "msg1");
    assert_eq!(entry.seq, 0);
}

#[tokio::test]
async fn test_tcp_multiple_clients() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(100);
    let handle = start_tcp_listener("127.0.0.1:0", tx).await.unwrap();

    for i in 0..3 {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
        let msg = json!({"version":"1.1","host":"client","short_message":format!("from {i}"),"level":6});
        stream.write_all(msg.to_string().as_bytes()).await.unwrap();
        stream.write_all(b"\0").await.unwrap();
        sleep(Duration::from_millis(50)).await;
    }

    sleep(Duration::from_millis(100)).await;
    let mut count = 0;
    while rx.try_recv().is_ok() { count += 1; }
    assert_eq!(count, 3);
}

#[tokio::test]
async fn test_tcp_malformed_not_sent() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LogEntry>(100);
    let handle = start_tcp_listener("127.0.0.1:0", tx).await.unwrap();

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", handle.port())).await.unwrap();
    stream.write_all(b"not valid json\0").await.unwrap();

    sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err());
}
