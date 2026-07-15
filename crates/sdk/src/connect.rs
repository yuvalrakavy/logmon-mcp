use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::reconnect::{initial_connect, is_transport_error, map_bridge_error, ConnectionManager};
use crate::{BrokerError, Notification};

/// Builder for [`Broker`] connections. Use [`Broker::connect`] to obtain one
/// with default settings, then chain builder methods before calling
/// [`BrokerBuilder::open`].
pub struct BrokerBuilder {
    socket_path: Option<PathBuf>,
    session_name: Option<String>,
    client_info: Option<Value>,
    reconnect_max_attempts: u32,
    reconnect_max_backoff: Duration,
    reconnect_initial_backoff: Duration,
    call_timeout: Option<Duration>, // None = compute dynamically from reconnect knobs
    notification_buffer: usize,
}

impl Default for BrokerBuilder {
    fn default() -> Self {
        Self {
            socket_path: None,
            session_name: None,
            client_info: None,
            reconnect_max_attempts: 10,
            reconnect_max_backoff: Duration::from_secs(30),
            reconnect_initial_backoff: Duration::from_millis(100),
            call_timeout: None, // resolved lazily; default = max_attempts × max_backoff
            notification_buffer: 100,
        }
    }
}

impl BrokerBuilder {
    pub fn session_name(mut self, name: impl Into<String>) -> Self {
        self.session_name = Some(name.into());
        self
    }
    pub fn client_info(mut self, info: Value) -> Self {
        self.client_info = Some(info);
        self
    }
    pub fn socket_path(mut self, p: PathBuf) -> Self {
        self.socket_path = Some(p);
        self
    }
    pub fn reconnect_max_attempts(mut self, n: u32) -> Self {
        self.reconnect_max_attempts = n;
        self
    }
    pub fn reconnect_max_backoff(mut self, d: Duration) -> Self {
        self.reconnect_max_backoff = d;
        self
    }
    pub fn reconnect_initial_backoff(mut self, d: Duration) -> Self {
        self.reconnect_initial_backoff = d;
        self
    }
    pub fn call_timeout(mut self, d: Duration) -> Self {
        self.call_timeout = Some(d);
        self
    }

    /// Returns the explicit `call_timeout` if set, otherwise
    /// `reconnect_max_attempts × reconnect_max_backoff`. On overflow, falls
    /// back to 5 minutes so we never silently produce a zero-duration
    /// timeout.
    pub(crate) fn resolved_call_timeout(&self) -> Duration {
        self.call_timeout.unwrap_or_else(|| {
            self.reconnect_max_backoff
                .checked_mul(self.reconnect_max_attempts)
                .unwrap_or_else(|| Duration::from_secs(300))
        })
    }

    pub(crate) fn resolved_socket_path(&self) -> Option<PathBuf> {
        self.socket_path.clone().or_else(|| {
            std::env::var("LOGMON_BROKER_SOCKET")
                .ok()
                .map(PathBuf::from)
        })
    }

    pub(crate) fn session_name_clone(&self) -> Option<String> {
        self.session_name.clone()
    }

    pub(crate) fn client_info_clone(&self) -> Option<Value> {
        self.client_info.clone()
    }

    pub(crate) fn notification_buffer(&self) -> usize {
        self.notification_buffer
    }

    pub(crate) fn resolved_initial_backoff(&self) -> Duration {
        self.reconnect_initial_backoff
    }

    pub(crate) fn resolved_max_backoff(&self) -> Duration {
        self.reconnect_max_backoff
    }

    pub(crate) fn resolved_max_attempts(&self) -> u32 {
        self.reconnect_max_attempts
    }

    pub async fn open(self) -> Result<Broker, BrokerError> {
        let config = Arc::new(self);
        let manager = initial_connect(config).await?;
        Ok(Broker { inner: manager })
    }
}

/// Cheap-to-clone handle to a connected broker session. All clones share the
/// same underlying connection-manager and session state.
#[derive(Clone)]
pub struct Broker {
    pub(crate) inner: Arc<ConnectionManager>,
}

impl Broker {
    /// Returns a [`BrokerBuilder`] with default settings.
    pub fn connect() -> BrokerBuilder {
        BrokerBuilder::default()
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn is_new_session(&self) -> bool {
        self.inner.is_new_session
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.inner.capabilities.iter().any(|c| c == name)
    }

    pub fn daemon_uptime(&self) -> Duration {
        self.inner.daemon_uptime
    }

    /// Subscribe to broker-originated typed notifications. Each subscriber
    /// gets its own broadcast receiver; lagged subscribers see
    /// [`broadcast::error::RecvError::Lagged`]. Unparseable wire notifications
    /// are dropped before reaching subscribers.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.inner.notification_tx.subscribe()
    }

    /// Untyped JSON-RPC call — convenience pass-through used by the MCP shim
    /// and by [`Broker::call_typed`].
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, BrokerError> {
        let deadline = tokio::time::Instant::now() + self.inner.config.resolved_call_timeout();
        let bridge = self.inner.current_bridge(deadline).await?;
        match bridge.call(method, params).await {
            Ok(v) => Ok(v),
            Err(e) => {
                if is_transport_error(&e) {
                    // The bridge is dead — drive the state machine into
                    // Reconnecting (or PermanentlyFailed for anonymous). The
                    // bridge's own EOF notify will also fire; both paths are
                    // idempotent.
                    self.inner.enter_disconnect().await;
                }
                Err(map_bridge_error(e))
            }
        }
    }

    /// Typed JSON-RPC dispatch helper. Serializes `params` to JSON, sends the
    /// request through the bridge, then deserializes the result into `R`. The
    /// per-method wrappers in [`crate::methods`] all delegate here.
    pub async fn call_typed<P, R>(&self, method: &str, params: P) -> Result<R, BrokerError>
    where
        P: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        let value =
            serde_json::to_value(&params).map_err(|e| BrokerError::Protocol(e.to_string()))?;
        let result_value = self.call(method, value).await?;
        serde_json::from_value(result_value).map_err(|e| BrokerError::Protocol(e.to_string()))
    }
}

pub(crate) fn default_socket_path() -> PathBuf {
    // Must match the broker daemon's `core::daemon::persistence::config_dir()`
    // location, which is `$HOME/.config/logmon/` on every Unix (including macOS,
    // where `dirs::config_dir()` would otherwise return `~/Library/Application
    // Support/`). Hard-code `.config/logmon/` so the SDK and broker agree on
    // every platform without the SDK depending on `core`.
    #[cfg(unix)]
    {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".config").join("logmon").join("logmon.sock")
    }
    #[cfg(windows)]
    {
        // TCP path — socket_path unused on Windows; left for symmetry
        PathBuf::from("127.0.0.1:12200")
    }
}
