//! TCP keepalive for the ingest inputs' accepted connections.

use crate::throttle::{note, Throttle};
use std::time::Duration;

/// How long a connection may sit silent before the OS starts checking that its peer is still
/// there, and how often it checks. A sender that lost power or its network never closes its
/// connection, and without keepalive the broker held it — a file descriptor — for the life of
/// the process; enough of them, and the broker could accept nothing, its clients included.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

static KEEPALIVE_ERRORS: Throttle = Throttle::new();

/// Turn on keepalive for an accepted connection — GELF TCP, OTLP gRPC and OTLP HTTP alike.
/// Best effort: a connection it cannot be set on is still served.
pub(crate) fn keep_alive(stream: &tokio::net::TcpStream) {
    let params = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    if let Err(e) = socket2::SockRef::from(stream).set_tcp_keepalive(&params) {
        if let Some(n) = KEEPALIVE_ERRORS.hit() {
            note(format_args!(
                "could not turn on keepalive for an ingest connection ({n} so far): {e}"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An accepted connection gets the broker's keepalive, so a sender that vanished without
    /// closing it does not hold its file descriptor for the life of the process. (The getters
    /// that read the settings back exist on the platforms logmon supports.)
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn keep_alive_turns_on_keepalive_with_the_broker_s_timings() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        let sock = socket2::SockRef::from(&accepted);
        assert!(!sock.keepalive().unwrap(), "off before");
        keep_alive(&accepted);
        assert!(sock.keepalive().unwrap());
        assert_eq!(sock.keepalive_time().unwrap(), KEEPALIVE_IDLE);
        assert_eq!(sock.keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
    }
}
