# Multi-Session Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor logmon-mcp into a daemon + shim architecture so multiple Claude Code sessions share one log collector.

**Architecture:** Single binary, two modes. Daemon owns GELF listeners, shared log buffer, session registry (per-session triggers/filters). Shim is a thin MCP-to-RPC bridge that auto-starts the daemon. Communication via JSON-RPC over Unix socket (macOS/Linux) or TCP (Windows).

**Tech Stack:** Rust, Tokio, rmcp, serde/serde_json, clap (subcommands), uuid

**Spec:** `docs/superpowers/specs/2026-03-27-multi-session-design.md`

---

## File Map

Changes marked: `[new]`, `[modify]`, `[unchanged]`. Unmarked = unchanged.

```
src/
├── main.rs                    [modify] CLI dispatch: shim vs daemon
├── lib.rs                     [modify] add new modules
├── config.rs                  [modify] clap subcommands
├── rpc/                       [new]
│   ├── mod.rs                 protocol types shared by shim and daemon
│   ├── types.rs               RpcRequest, RpcResponse, RpcNotification
│   └── transport.rs           read/write JSON-RPC over AsyncRead+AsyncWrite
├── shim/                      [new]
│   ├── mod.rs
│   ├── auto_start.rs          daemon detection, PID check, spawn
│   └── bridge.rs              MCP stdio ↔ daemon RPC translation
├── daemon/                    [new]
│   ├── mod.rs
│   ├── server.rs              socket listener, connection management
│   ├── session.rs             SessionRegistry, SessionState
│   ├── log_processor.rs       main loop: channel → trigger eval → storage
│   ├── rpc_handler.rs         dispatch RPC requests to session/store
│   └── persistence.rs         state.json + config.json read/write
├── receiver/                  [new]
│   ├── mod.rs                 LogReceiver trait
│   └── gelf.rs                GelfReceiver (wraps existing UDP/TCP)
├── engine/
│   ├── mod.rs
│   ├── pipeline.rs            [modify] reduced to shared infra (store + pre-buffer + seq)
│   ├── pre_buffer.rs          [unchanged]
│   └── trigger.rs             [unchanged]
├── filter/                    [unchanged]
├── store/                     [unchanged]
├── gelf/
│   ├── mod.rs
│   ├── message.rs             [unchanged]
│   ├── udp.rs                 [modify] take mpsc::Sender instead of Arc<LogPipeline>
│   └── tcp.rs                 [modify] take mpsc::Sender instead of Arc<LogPipeline>
└── mcp/
    ├── mod.rs
    ├── server.rs              [modify] tools call RPC instead of pipeline directly
    └── notifications.rs       [modify] forward daemon RPC notifications to MCP
```

### New Dependencies

```toml
uuid = { version = "1", features = ["v4"] }
fs2 = "0.4"                    # file locking (cross-platform)
async-trait = "0.1"
tracing-appender = "0.2"       # daemon log file rotation

[dev-dependencies]
tempfile = "3"                 # temp dirs for persistence tests
```

---

## Phase 1: Foundation (Shared Protocol + Infrastructure)

### Task 1: RPC Protocol Types

**Files:**
- Create: `src/rpc/mod.rs`
- Create: `src/rpc/types.rs`
- Create: `src/rpc/transport.rs`
- Modify: `src/lib.rs` (add `pub mod rpc;`)

- [ ] **Step 1: Create RPC type definitions**

`src/rpc/types.rs`:
```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

/// JSON-RPC 2.0 success response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    pub result: Option<Value>,
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 notification (no id, no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

/// Envelope: either a response or a notification from daemon
#[derive(Debug, Clone)]
pub enum DaemonMessage {
    Response(RpcResponse),
    Notification(RpcNotification),
}

/// Parse a daemon message by checking for `id` field presence.
/// Do NOT use serde untagged — it's fragile and can silently misparse.
pub fn parse_daemon_message_from_str(line: &str) -> anyhow::Result<DaemonMessage> {
    let v: serde_json::Value = serde_json::from_str(line)?;
    if v.get("id").is_some() {
        Ok(DaemonMessage::Response(serde_json::from_value(v)?))
    } else {
        Ok(DaemonMessage::Notification(serde_json::from_value(v)?))
    }
}

/// session.start parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartParams {
    pub name: Option<String>,
    pub protocol_version: u32,
}

/// session.start response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartResult {
    pub session_id: String,
    pub is_new: bool,
    pub queued_notifications: usize,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub daemon_uptime_secs: u64,
    pub buffer_size: usize,
    pub receivers: Vec<String>,
}

impl RpcRequest {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        }
    }
}

impl RpcResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), id, result: Some(result), error: None }
    }
    pub fn error(id: u64, code: i32, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(), id, result: None,
            error: Some(RpcError { code, message: message.to_string(), data: None }),
        }
    }
}

impl RpcNotification {
    pub fn new(method: &str, params: Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), method: method.to_string(), params }
    }
}
```

- [ ] **Step 2: Create RPC transport (newline-delimited JSON over async streams)**

`src/rpc/transport.rs`:
```rust
use super::types::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Write a JSON-RPC message followed by newline
pub async fn write_message<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &impl serde::Serialize) -> anyhow::Result<()> {
    let json = serde_json::to_string(msg)?;
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Read one newline-delimited JSON message
pub async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Option<String>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 { return Ok(None); } // EOF
    Ok(Some(line))
}

/// Read and parse a DaemonMessage (response or notification)
pub async fn read_daemon_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Option<DaemonMessage>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(super::types::parse_daemon_message_from_str(&line)?)),
        None => Ok(None),
    }
}

/// Read and parse an RpcRequest
pub async fn read_request<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> anyhow::Result<Option<RpcRequest>> {
    match read_line(reader).await? {
        Some(line) => Ok(Some(serde_json::from_str(&line)?)),
        None => Ok(None),
    }
}
```

- [ ] **Step 3: Create `src/rpc/mod.rs`**

```rust
pub mod types;
pub mod transport;
```

- [ ] **Step 4: Add `pub mod rpc;` to `src/lib.rs`, add uuid and fs2 to Cargo.toml**

- [ ] **Step 5: Verify compilation**

Run: `cargo build`

- [ ] **Step 6: Commit**

```bash
git add src/rpc/ src/lib.rs Cargo.toml
git commit -m "feat: JSON-RPC protocol types and transport"
```

---

### Task 2: Persistence Module

**Files:**
- Create: `src/daemon/mod.rs` (start with just persistence)
- Create: `src/daemon/persistence.rs`
- Create: `tests/persistence.rs`
- Modify: `src/lib.rs` (add `pub mod daemon;`)

- [ ] **Step 1: Write persistence tests**

`tests/persistence.rs`:
```rust
use logmon_mcp_server::daemon::persistence::{DaemonState, DaemonConfig, load_state, save_state, load_config};
use std::path::PathBuf;

#[test]
fn test_state_default() {
    let state = DaemonState::default();
    assert_eq!(state.seq_block, 0);
    assert!(state.named_sessions.is_empty());
}

#[test]
fn test_state_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");

    let mut state = DaemonState::default();
    state.seq_block = 49000;
    save_state(&path, &state).unwrap();

    let loaded = load_state(&path).unwrap();
    assert_eq!(loaded.seq_block, 49000);
}

#[test]
fn test_state_missing_file_returns_default() {
    let path = PathBuf::from("/nonexistent/state.json");
    let state = load_state(&path).unwrap();
    assert_eq!(state.seq_block, 0);
}

#[test]
fn test_config_default() {
    let config = DaemonConfig::default();
    assert_eq!(config.gelf_port, 12201);
    assert_eq!(config.buffer_size, 10000);
    assert_eq!(config.idle_timeout_secs, 1800);
    assert!(!config.persist_buffer_on_exit);
}

#[test]
fn test_config_missing_file_returns_default() {
    let path = PathBuf::from("/nonexistent/config.json");
    let config = load_config(&path).unwrap();
    assert_eq!(config.gelf_port, 12201);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test persistence`

- [ ] **Step 3: Implement persistence**

`src/daemon/persistence.rs`:
```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const SEQ_BLOCK_SIZE: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub triggers: Vec<PersistedTrigger>,
    pub filters: Vec<PersistedFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTrigger {
    pub filter: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedFilter {
    pub filter: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonState {
    #[serde(default)]
    pub seq_block: u64,
    #[serde(default)]
    pub named_sessions: HashMap<String, PersistedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_gelf_port")]
    pub gelf_port: u16,
    #[serde(default)]
    pub gelf_udp_port: Option<u16>,
    #[serde(default)]
    pub gelf_tcp_port: Option<u16>,
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    #[serde(default)]
    pub persist_buffer_on_exit: bool,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
}

fn default_gelf_port() -> u16 { 12201 }
fn default_buffer_size() -> usize { 10000 }
fn default_idle_timeout() -> u64 { 1800 }

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            gelf_port: 12201,
            gelf_udp_port: None,
            gelf_tcp_port: None,
            buffer_size: 10000,
            persist_buffer_on_exit: false,
            idle_timeout_secs: 1800,
        }
    }
}

pub fn load_state(path: &Path) -> anyhow::Result<DaemonState> {
    if !path.exists() {
        return Ok(DaemonState::default());
    }
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

pub fn save_state(path: &Path, state: &DaemonState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)?;
    std::fs::write(path, content)?;
    Ok(())
}

pub fn load_config(path: &Path) -> anyhow::Result<DaemonConfig> {
    if !path.exists() {
        return Ok(DaemonConfig::default());
    }
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

/// Returns the logmon config directory (~/.config/logmon/)
pub fn config_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config")
        .join("logmon")
}
```

Note: We don't actually need the `dirs` crate — just `home_dir()` from the standard library or a simple `$HOME` env read. Check if `dirs::home_dir()` is available or use `std::env::var("HOME")`.

`src/daemon/mod.rs`:
```rust
pub mod persistence;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --test persistence`
Expected: All 5 tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/daemon/ tests/persistence.rs src/lib.rs Cargo.toml
git commit -m "feat: daemon state and config persistence"
```

---

### Task 3: Refactor GELF Listeners to Use Channel

**Files:**
- Modify: `src/gelf/udp.rs`
- Modify: `src/gelf/tcp.rs`
- Modify: `tests/gelf_udp.rs`
- Modify: `tests/gelf_tcp.rs`

- [ ] **Step 1: Refactor UDP listener to accept `mpsc::Sender<LogEntry>` instead of `Arc<LogPipeline>`**

Change `start_udp_listener` signature:
```rust
pub async fn start_udp_listener(
    addr: &str,
    sender: mpsc::Sender<LogEntry>,
) -> anyhow::Result<UdpListenerHandle>
```

Instead of calling `pipeline.assign_seq()` and `pipeline.process()`, the listener now:
- Parses GELF message with seq=0 (daemon will assign seq later)
- Sends the entry via `sender.send(entry).await`
- For malformed messages: sends a special error entry or tracks separately

Actually, the listener can't assign seq anymore — the daemon does that. Two options:
1. Send raw bytes and let daemon parse + assign seq
2. Send parsed LogEntry with seq=0, daemon assigns seq after receiving

Option 2 is cleaner. The `LogEntry.seq` field is set to 0 by the listener, and the daemon assigns the real seq. Alternatively, pass an `AtomicU64` for the seq counter so the listener can assign seqs directly. But per the spec, seq assignment is in the daemon's main loop.

Go with option 2: parse in listener, send with seq=0, daemon assigns seq.

For malformed messages, the listener can log to stderr and not send anything. The malformed counter moves to the daemon (it tracks errors from any receiver).

- [ ] **Step 2: Refactor TCP listener similarly**

Same change: take `mpsc::Sender<LogEntry>`, parse and send, seq=0.

- [ ] **Step 3: Update UDP test**

Tests need to create an `mpsc::channel` and check received entries instead of checking pipeline:

```rust
#[tokio::test]
async fn test_udp_listener_receives_gelf() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    let handle = start_udp_listener("127.0.0.1:0", tx).await.unwrap();

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let msg = serde_json::json!({
        "version": "1.1", "host": "test", "short_message": "hello", "level": 6
    });
    socket.send_to(msg.to_string().as_bytes(), format!("127.0.0.1:{}", handle.port())).unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;
    let entry = rx.try_recv().unwrap();
    assert_eq!(entry.message, "hello");
}
```

- [ ] **Step 4: Update TCP test similarly**

- [ ] **Step 5: Temporarily disable pipeline integration tests**

The existing `tests/pipeline.rs` tests call `LogPipeline::process()` and other methods that will be removed in Task 6. Comment out or delete the tests that depend on `process()`, `add_filter()`, `add_trigger()`, etc. Keep only the `test_seq_counter` test and the `make_entry` helper. The full integration tests will be rewritten in Task 7 (log processor) and Task 14 (multi-session).

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: All tests pass (some pipeline tests removed, GELF tests updated)

- [ ] **Step 6: Commit**

```bash
git add src/gelf/ tests/gelf_udp.rs tests/gelf_tcp.rs
git commit -m "refactor: GELF listeners use channel instead of pipeline"
```

---

### Task 4: LogReceiver Trait + GelfReceiver

**Files:**
- Create: `src/receiver/mod.rs`
- Create: `src/receiver/gelf.rs`
- Modify: `src/lib.rs` (add `pub mod receiver;`)

- [ ] **Step 1: Define LogReceiver trait**

`src/receiver/mod.rs`:
```rust
pub mod gelf;

use crate::gelf::message::LogEntry;
use async_trait::async_trait;
use tokio::sync::mpsc;

#[async_trait]
pub trait LogReceiver: Send + Sync {
    async fn start(sender: mpsc::Sender<LogEntry>) -> anyhow::Result<Box<dyn LogReceiver>>
    where Self: Sized;

    fn name(&self) -> &str;
    fn listening_on(&self) -> Vec<String>;
    async fn shutdown(self: Box<Self>);
}
```

Note: We need `async_trait` crate. Add `async-trait = "0.1"` to Cargo.toml.

- [ ] **Step 2: Implement GelfReceiver**

`src/receiver/gelf.rs`:
```rust
use super::LogReceiver;
use crate::gelf::message::LogEntry;
use crate::gelf::{udp, tcp};
use async_trait::async_trait;
use tokio::sync::mpsc;

pub struct GelfReceiverConfig {
    pub udp_addr: String,
    pub tcp_addr: String,
}

pub struct GelfReceiver {
    udp_handle: udp::UdpListenerHandle,
    tcp_handle: tcp::TcpListenerHandle,
}

impl GelfReceiver {
    pub async fn start_with_config(
        config: GelfReceiverConfig,
        sender: mpsc::Sender<LogEntry>,
    ) -> anyhow::Result<Self> {
        let udp_handle = udp::start_udp_listener(&config.udp_addr, sender.clone()).await?;
        let tcp_handle = tcp::start_tcp_listener(&config.tcp_addr, sender).await?;
        Ok(Self { udp_handle, tcp_handle })
    }

    pub fn udp_port(&self) -> u16 { self.udp_handle.port() }
    pub fn tcp_port(&self) -> u16 { self.tcp_handle.port() }
}

#[async_trait]
impl LogReceiver for GelfReceiver {
    async fn start(sender: mpsc::Sender<LogEntry>) -> anyhow::Result<Box<dyn LogReceiver>> {
        // Default ports — actual config comes from GelfReceiverConfig
        unimplemented!("Use start_with_config instead")
    }

    fn name(&self) -> &str { "gelf" }

    fn listening_on(&self) -> Vec<String> {
        vec![
            format!("UDP:{}", self.udp_handle.port()),
            format!("TCP:{}", self.tcp_handle.port()),
        ]
    }

    async fn shutdown(self: Box<Self>) {
        // Drop handles (triggers oneshot shutdown)
    }
}
```

- [ ] **Step 3: Add `async-trait` to Cargo.toml, add `pub mod receiver;` to lib.rs**

- [ ] **Step 4: Verify compilation**

Run: `cargo build`

- [ ] **Step 5: Commit**

```bash
git add src/receiver/ src/lib.rs Cargo.toml
git commit -m "feat: LogReceiver trait and GelfReceiver"
```

---

## Phase 2: Daemon

### Task 5: Session Registry

**Files:**
- Create: `src/daemon/session.rs`
- Create: `tests/session_registry.rs`
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Write session registry tests**

`tests/session_registry.rs`:
```rust
use logmon_mcp_server::daemon::session::{SessionRegistry, SessionId};

#[test]
fn test_create_anonymous_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    assert!(registry.get(&id).is_some());
}

#[test]
fn test_create_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("store-debug").unwrap();
    assert_eq!(id, SessionId::Named("store-debug".to_string()));
    assert!(registry.get(&id).is_some());
}

#[test]
fn test_invalid_session_name() {
    let registry = SessionRegistry::new();
    assert!(registry.create_named("../bad").is_err());
    assert!(registry.create_named("").is_err());
    assert!(registry.create_named("has spaces").is_err());
}

#[test]
fn test_reconnect_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    assert!(!registry.is_connected(&id));
    registry.reconnect(&id).unwrap();
    assert!(registry.is_connected(&id));
}

#[test]
fn test_cannot_connect_to_active_session() {
    let registry = SessionRegistry::new();
    registry.create_named("test").unwrap();
    assert!(registry.create_named("test").is_err()); // already connected
}

#[test]
fn test_anonymous_removed_on_disconnect() {
    let registry = SessionRegistry::new();
    let id = registry.create_anonymous();
    registry.disconnect(&id);
    assert!(registry.get(&id).is_none());
}

#[test]
fn test_named_persists_on_disconnect() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    assert!(registry.get(&id).is_some());
    assert!(!registry.is_connected(&id));
}

#[test]
fn test_drop_named_session() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    registry.drop_session("test").unwrap();
    assert!(registry.get(&id).is_none());
}

#[test]
fn test_list_sessions() {
    let registry = SessionRegistry::new();
    registry.create_anonymous();
    registry.create_named("alpha").unwrap();
    let list = registry.list();
    assert_eq!(list.len(), 2);
}

#[test]
fn test_notification_queue() {
    let registry = SessionRegistry::new();
    let id = registry.create_named("test").unwrap();
    registry.disconnect(&id);
    // Queue some notifications
    let event = logmon_mcp_server::engine::pipeline::PipelineEvent {
        trigger_id: 1,
        trigger_description: None,
        filter_string: "l>=ERROR".to_string(),
        matched_entry: logmon_mcp_server::gelf::message::LogEntry {
            seq: 1, timestamp: chrono::Utc::now(),
            level: logmon_mcp_server::gelf::message::Level::Error,
            message: "test".into(), full_message: None,
            host: "test".into(), facility: None, file: None, line: None,
            additional_fields: std::collections::HashMap::new(),
            matched_filters: vec![], source: logmon_mcp_server::gelf::message::LogSource::Filter,
        },
        context_before: vec![],
        pre_trigger_flushed: 0,
        post_window_size: 0,
    };
    registry.queue_notification(&id, event);
    let queued = registry.drain_notifications(&id);
    assert_eq!(queued.len(), 1);
}
```

- [ ] **Step 2: Implement SessionRegistry**

`src/daemon/session.rs`:

Key types:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionId {
    Anonymous(String),  // UUID
    Named(String),
}

pub struct SessionInfo {
    pub id: SessionId,
    pub connected: bool,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub queue_size: usize,
    pub last_seen: std::time::Instant,
}
```

`SessionRegistry` wraps `RwLock<HashMap<SessionId, SessionState>>`. Each `SessionState` uses interior mutability for fields that change per-log (so `process_entry` can hold a read lock on the registry):

```rust
struct SessionState {
    name: Option<String>,
    triggers: TriggerManager,              // match_count uses AtomicU64 internally
    filters: RwLock<Vec<BufferFilterEntry>>, // changed by RPC, read by process_entry
    notification_queue: Mutex<VecDeque<PipelineEvent>>, // push from process_entry
    max_queue_size: usize,                 // 1000, immutable after creation
    post_window_remaining: AtomicU32,      // decremented per-log by process_entry
    connected: AtomicBool,
    last_seen: Mutex<Instant>,
}
```

This design means `process_entry` holds a READ lock on the registry HashMap while writing to individual sessions via their interior mutability. No contention between sessions. RPC handlers that add/remove sessions take a WRITE lock on the HashMap (rare, fast).

**Session-removal safety**: All `SessionRegistry` methods that operate on a session return `Option` or `Result`. `process_entry` must handle the case where a session is removed between getting the ID list and operating on it — skip silently, don't panic.

Session name validation: `regex::Regex::new(r"^[a-zA-Z0-9_-]+$")`

- [ ] **Step 3: Run tests**

Run: `cargo test --test session_registry`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/session.rs tests/session_registry.rs src/daemon/mod.rs
git commit -m "feat: session registry with anonymous and named sessions"
```

---

### Task 6: Refactor LogPipeline to Shared Infrastructure

**Files:**
- Modify: `src/engine/pipeline.rs`
- Modify: `tests/pipeline.rs`

- [ ] **Step 1: Refactor LogPipeline**

Remove per-session state from `LogPipeline`. It becomes:

```rust
pub struct LogPipeline {
    store: InMemoryStore,
    pre_buffer: PreTriggerBuffer,
    seq_counter: AtomicU64,
    event_sender: broadcast::Sender<PipelineEvent>,
}
```

Remove: `TriggerManager`, `filters`, `post_window_remaining`, `next_filter_id`.

Keep methods: `assign_seq()`, `subscribe_events()`, `store_len()`, `store_stats()`, `increment_malformed()`, `clear_logs()`, and store access methods (`recent_logs`, `context_by_seq`, `context_by_time`).

Remove: `process()`, `add_filter/edit_filter/remove_filter/list_filters`, `add_trigger/edit_trigger/remove_trigger/list_triggers`. These move to the daemon's session-aware log processor.

Note: `BufferFilterEntry` and `FilterInfo` types currently defined in `pipeline.rs` are needed by `session.rs`. Extract them to a shared location (e.g., `engine/pipeline.rs` keeps them as public types, or move to a `types.rs`).

Add: `append_to_store(entry)`, `pre_buffer_append(entry)`, `pre_buffer_copy(n) -> Vec<LogEntry>`, `contains_seq(seq)`, `resize_pre_buffer(size)`.

- [ ] **Step 2: Update pipeline tests**

The existing pipeline tests that test trigger/filter behavior need to move to integration tests in a later task (Task 10). For now, update tests to only test the shared infrastructure:

```rust
#[test]
fn test_seq_counter() {
    let pipeline = LogPipeline::new(1000);
    assert_eq!(pipeline.assign_seq(), 1);
    assert_eq!(pipeline.assign_seq(), 2);
}

#[test]
fn test_store_and_query() {
    let pipeline = LogPipeline::new(1000);
    let entry = make_entry(1, Level::Info, "hello");
    pipeline.append_to_store(entry);
    assert_eq!(pipeline.store_len(), 1);
}

#[test]
fn test_pre_buffer() {
    let pipeline = LogPipeline::new(1000);
    pipeline.resize_pre_buffer(5);
    for i in 1..=3 {
        pipeline.pre_buffer_append(make_entry(i, Level::Info, &format!("msg {i}")));
    }
    let copied = pipeline.pre_buffer_copy(2);
    assert_eq!(copied.len(), 2);
}

#[test]
fn test_clear_logs() {
    let pipeline = LogPipeline::new(1000);
    pipeline.append_to_store(make_entry(1, Level::Info, "hello"));
    let cleared = pipeline.clear_logs();
    assert_eq!(cleared, 1);
    assert_eq!(pipeline.store_len(), 0);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test --test pipeline`

- [ ] **Step 4: Commit**

```bash
git add src/engine/pipeline.rs tests/pipeline.rs
git commit -m "refactor: LogPipeline to shared infrastructure (store + pre-buffer + seq)"
```

---

### Task 7: Daemon Log Processor

**Files:**
- Create: `src/daemon/log_processor.rs`
- Create: `tests/log_processor.rs`
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Write log processor tests**

Test the core processing flow using `process_entry` directly (not through the channel — that's tested in integration). Import `process_entry` as a public function for testability.

```rust
use logmon_mcp_server::daemon::log_processor::process_entry;
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str) -> LogEntry {
    LogEntry {
        seq: 0, // daemon assigns real seq
        timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: Some("test::module".into()),
        file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_log_stored_when_no_filters() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid = sessions.create_anonymous();

    let mut entry = make_entry(Level::Info, "hello");
    process_entry(&mut entry, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);
    assert!(entry.seq > 0); // seq was assigned
}

#[test]
fn test_trigger_fires_and_flushes_pre_buffer() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Session has default error trigger (pre_window=500)

    // Add a filter so INFO logs are NOT stored normally
    sessions.add_filter(&sid, "l>=ERROR", Some("errors")).unwrap();

    // Send 10 INFO entries — go to pre-buffer but not store
    for _ in 0..10 {
        let mut entry = make_entry(Level::Info, "background noise");
        process_entry(&mut entry, &pipeline, &sessions);
    }
    assert_eq!(pipeline.store_len(), 0);

    // Send ERROR — trigger fires, pre-buffer flushes
    let mut error_entry = make_entry(Level::Error, "crash!");
    process_entry(&mut error_entry, &pipeline, &sessions);
    // Pre-buffer entries + triggering entry should be in store
    assert!(pipeline.store_len() > 1);
}

#[test]
fn test_post_window_skips_triggers() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Edit trigger to have post_window=2
    let triggers = sessions.list_triggers(&sid);
    sessions.edit_trigger(&sid, triggers[0].id, None, None, Some(2), None, None).unwrap();

    // Add filter to block INFO
    sessions.add_filter(&sid, "l>=ERROR", None).unwrap();

    // Fire trigger
    let mut entry = make_entry(Level::Error, "crash");
    process_entry(&mut entry, &pipeline, &sessions);
    let after_trigger = pipeline.store_len();

    // 2 post-window entries — stored despite filter
    let mut p1 = make_entry(Level::Info, "post 1");
    process_entry(&mut p1, &pipeline, &sessions);
    let mut p2 = make_entry(Level::Info, "post 2");
    process_entry(&mut p2, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), after_trigger + 2);

    // 3rd entry — post-window expired, filtered out
    let mut p3 = make_entry(Level::Info, "filtered");
    process_entry(&mut p3, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), after_trigger + 2);
}

#[test]
fn test_per_session_filters() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid_a = sessions.create_anonymous();
    let sid_b = sessions.create_anonymous();

    sessions.add_filter(&sid_a, "fa=mqtt", None).unwrap();
    sessions.add_filter(&sid_b, "l>=ERROR", None).unwrap();

    // MQTT INFO → stored (matches A's filter)
    let mut e1 = make_entry(Level::Info, "mqtt msg");
    e1.facility = Some("app::mqtt".into());
    process_entry(&mut e1, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);

    // Non-MQTT DEBUG → not stored
    let mut e2 = make_entry(Level::Debug, "debug msg");
    e2.facility = Some("app::http".into());
    process_entry(&mut e2, &pipeline, &sessions);
    assert_eq!(pipeline.store_len(), 1);

    // ERROR → stored (matches B's filter)
    let mut e3 = make_entry(Level::Error, "error msg");
    e3.facility = Some("app::http".into());
    process_entry(&mut e3, &pipeline, &sessions);
    assert!(pipeline.store_len() > 1); // error + possible pre-buffer flush
}

#[test]
fn test_notification_queued_for_disconnected_session() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_named("test-queue").unwrap();
    sessions.disconnect(&sid);

    let mut entry = make_entry(Level::Error, "crash while disconnected");
    process_entry(&mut entry, &pipeline, &sessions);

    let queued = sessions.drain_notifications(&sid);
    assert_eq!(queued.len(), 1);
}

#[test]
fn test_zero_sessions_stores_everything() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    // No sessions at all

    let mut entry = make_entry(Level::Debug, "nobody listening");
    process_entry(&mut entry, &pipeline, &sessions);
    // No filters from any session = implicit ALL
    assert_eq!(pipeline.store_len(), 1);
}

#[test]
fn test_trigger_with_pre_window_zero() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    // Edit default trigger to pre_window=0
    let triggers = sessions.list_triggers(&sid);
    sessions.edit_trigger(&sid, triggers[0].id, None, Some(0), None, None, None).unwrap();

    // Send some context then an error
    for _ in 0..5 {
        let mut e = make_entry(Level::Info, "context");
        process_entry(&mut e, &pipeline, &sessions);
    }
    let mut error = make_entry(Level::Error, "crash");
    process_entry(&mut error, &pipeline, &sessions);

    // Triggering entry must be stored even with pre_window=0
    let logs = pipeline.recent_logs(100, None);
    assert!(logs.iter().any(|l| l.message == "crash"));
}

#[test]
fn test_session_removed_during_processing_no_panic() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();

    // Remove the session from another "thread" (simulated)
    sessions.disconnect(&sid);

    // This should not panic — process_entry skips missing sessions
    let mut entry = make_entry(Level::Error, "nobody home");
    process_entry(&mut entry, &pipeline, &sessions);
    // Entry stored via "no sessions = no filters = store everything" path
    assert_eq!(pipeline.store_len(), 1);
}
```

- [ ] **Step 2: Implement log processor**

`src/daemon/log_processor.rs`:

```rust
use crate::daemon::session::SessionRegistry;
use crate::engine::pipeline::LogPipeline;
use crate::gelf::message::{LogEntry, LogSource};
use crate::filter::matcher::matches_entry;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Spawns the main log processing loop.
/// Reads LogEntries from the channel, assigns seq, evaluates triggers/filters, stores.
/// Also handles seq block persistence — writes state.json when crossing a block boundary.
pub fn spawn_log_processor(
    mut receiver: mpsc::Receiver<LogEntry>,
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
    state_path: std::path::PathBuf,
    state: Arc<Mutex<DaemonState>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut entry) = receiver.recv().await {
            process_entry(&mut entry, &pipeline, &sessions);

            // Persist state when crossing a seq block boundary
            if entry.seq > 0 && entry.seq % SEQ_BLOCK_SIZE == 0 {
                let mut s = state.lock().unwrap();
                s.seq_block = entry.seq + SEQ_BLOCK_SIZE;
                if let Err(e) = save_state(&state_path, &s) {
                    tracing::error!("Failed to persist state: {e}");
                }
            }
        }
    })
}

pub fn process_entry(entry: &mut LogEntry, pipeline: &LogPipeline, sessions: &SessionRegistry) {
    // 1. Assign seq
    entry.seq = pipeline.assign_seq();

    // 2. Append to pre-trigger buffer
    pipeline.pre_buffer_append(entry.clone());

    // 3. Evaluate triggers per session
    let mut any_post_window_active = false;
    let mut trigger_matched_any = false;

    // Get session list sorted by largest pre_window first
    let session_ids = sessions.active_session_ids_sorted_by_pre_window();

    for sid in &session_ids {
        // 3a. Check post-window
        if sessions.decrement_post_window(sid) {
            any_post_window_active = true;
            continue;
        }

        // 3b. Evaluate triggers
        let matches = sessions.evaluate_triggers(sid, entry);
        if !matches.is_empty() {
            trigger_matched_any = true;
            let max_pre = matches.iter().map(|m| m.pre_window).max().unwrap_or(0);
            let max_post = matches.iter().map(|m| m.post_window).max().unwrap_or(0);

            // Always store the triggering entry
            if !pipeline.contains_seq(entry.seq) {
                let mut trigger_entry = entry.clone();
                trigger_entry.source = LogSource::PreTrigger;
                pipeline.append_to_store(trigger_entry);
            }

            // Copy pre_window entries from pre-buffer into store
            let pre_entries = pipeline.pre_buffer_copy(max_pre as usize);
            for mut pre_entry in pre_entries {
                if !pipeline.contains_seq(pre_entry.seq) {
                    pre_entry.source = LogSource::PreTrigger;
                    pipeline.append_to_store(pre_entry);
                }
            }

            // Activate post-window for this session
            sessions.set_post_window(sid, max_post);
            any_post_window_active = true;

            // Build notification event and send/queue
            for m in &matches {
                let context_before = pipeline.store_context_before(entry.seq, m.notify_context as usize);
                let event = PipelineEvent {
                    trigger_id: m.id,
                    trigger_description: m.description.clone(),
                    filter_string: m.filter_string.clone(),
                    matched_entry: entry.clone(),
                    context_before,
                    pre_trigger_flushed: max_pre as usize,
                    post_window_size: m.post_window,
                };
                sessions.send_or_queue_notification(sid, event);
            }
        }
    }

    // 4. Buffer storage (if entry not already stored by trigger logic)
    if !pipeline.contains_seq(entry.seq) {
        if any_post_window_active {
            // Post-window active — store unconditionally
            let mut store_entry = entry.clone();
            store_entry.source = LogSource::PostTrigger;
            pipeline.append_to_store(store_entry);
        } else {
            // Evaluate union of all sessions' filters
            let (should_store, matched_descriptions) = sessions.evaluate_filters(entry);
            if should_store {
                let mut store_entry = entry.clone();
                store_entry.matched_filters = matched_descriptions;
                store_entry.source = LogSource::Filter;
                pipeline.append_to_store(store_entry);
            }
        }
    }
}
```

The implementation follows the spec's Log Processing Flow exactly. Key detail: session iteration needs read access to the registry, trigger evaluation needs write access to match_count and post_window_remaining. Use fine-grained locking within SessionState.

- [ ] **Step 3: Run tests**

Run: `cargo test --test log_processor`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/log_processor.rs tests/log_processor.rs src/daemon/mod.rs
git commit -m "feat: daemon log processor with per-session triggers and filters"
```

---

### Task 8: Daemon RPC Handler

**Files:**
- Create: `src/daemon/rpc_handler.rs`
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Implement RPC request dispatcher**

`src/daemon/rpc_handler.rs`:

```rust
use crate::daemon::session::{SessionRegistry, SessionId};
use crate::engine::pipeline::LogPipeline;
use crate::rpc::types::*;
use serde_json::Value;
use std::sync::Arc;

pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}

impl RpcHandler {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        sessions: Arc<SessionRegistry>,
        receivers_info: Vec<String>,
    ) -> Self {
        Self { pipeline, sessions, start_time: std::time::Instant::now(), receivers_info }
    }

    /// Handle an RPC request for a given session
    pub fn handle(&self, session_id: &SessionId, request: &RpcRequest) -> RpcResponse {
        let result = match request.method.as_str() {
            "logs.recent" => self.handle_logs_recent(session_id, &request.params),
            "logs.context" => self.handle_logs_context(&request.params),
            "logs.export" => self.handle_logs_export(&request.params),
            "logs.clear" => self.handle_logs_clear(),
            "status.get" => self.handle_status(session_id),
            "filters.list" => self.handle_filters_list(session_id),
            "filters.add" => self.handle_filters_add(session_id, &request.params),
            "filters.edit" => self.handle_filters_edit(session_id, &request.params),
            "filters.remove" => self.handle_filters_remove(session_id, &request.params),
            "triggers.list" => self.handle_triggers_list(session_id),
            "triggers.add" => self.handle_triggers_add(session_id, &request.params),
            "triggers.edit" => self.handle_triggers_edit(session_id, &request.params),
            "triggers.remove" => self.handle_triggers_remove(session_id, &request.params),
            "session.list" => self.handle_session_list(),
            "session.drop" => self.handle_session_drop(session_id, &request.params),
            _ => Err(format!("unknown method: {}", request.method)),
        };

        match result {
            Ok(value) => RpcResponse::success(request.id, value),
            Err(msg) => RpcResponse::error(request.id, -32601, &msg),
        }
    }

    // Each handler extracts params, calls pipeline/session methods, returns Value
    // ...
}
```

Each `handle_*` method follows the same pattern: parse params from `Value`, call appropriate methods on pipeline/sessions, return `Result<Value, String>`.

Key details:
- `logs.recent`: applies session's filters as default lens if no explicit filter
- `status.get`: includes session identity, receiver info, daemon uptime
- `session.drop`: validates not dropping own session, only named sessions
- All trigger/filter operations resize pre-buffer afterward

- [ ] **Step 2: Verify compilation**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/daemon/rpc_handler.rs src/daemon/mod.rs
git commit -m "feat: daemon RPC request handler"
```

---

### Task 9: Daemon Server (Socket Listener + Lifecycle)

**Files:**
- Create: `src/daemon/server.rs`
- Modify: `src/daemon/mod.rs`

- [ ] **Step 1: Implement daemon server**

`src/daemon/server.rs`:

```rust
use crate::daemon::log_processor::spawn_log_processor;
use crate::daemon::persistence::*;
use crate::daemon::rpc_handler::RpcHandler;
use crate::daemon::session::{SessionRegistry, SessionId};
use crate::engine::pipeline::LogPipeline;
use crate::receiver::gelf::{GelfReceiver, GelfReceiverConfig};
use crate::rpc::transport;
use crate::rpc::types::*;
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    let config_dir = config_dir();
    std::fs::create_dir_all(&config_dir)?;

    // Redirect tracing to daemon.log (daily rotation)
    let file_appender = tracing_appender::rolling::daily(&config_dir, "daemon.log");
    tracing_subscriber::fmt()
        .with_writer(file_appender)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("logmon_mcp_server=info".parse().unwrap())
        )
        .init();

    // Load state
    let state = load_state(&config_dir.join("state.json"))?;
    let initial_seq = state.seq_block;

    // Create pipeline with seq starting from the reserved block
    let pipeline = Arc::new(LogPipeline::new_with_seq(config.buffer_size, initial_seq));

    // Reserve next seq block
    let mut new_state = state.clone();
    new_state.seq_block = initial_seq + SEQ_BLOCK_SIZE;
    save_state(&config_dir.join("state.json"), &new_state)?;

    // Create session registry, restore named sessions from state
    let sessions = Arc::new(SessionRegistry::new());
    for (name, persisted) in &state.named_sessions {
        sessions.restore_named(name, persisted);
    }

    // Start log receiver channel
    let (log_tx, log_rx) = mpsc::channel(10000);

    // Start GELF receiver
    let gelf_config = GelfReceiverConfig {
        udp_addr: format!("0.0.0.0:{}", config.gelf_udp_port.unwrap_or(config.gelf_port)),
        tcp_addr: format!("0.0.0.0:{}", config.gelf_tcp_port.unwrap_or(config.gelf_port)),
    };
    let gelf_receiver = GelfReceiver::start_with_config(gelf_config, log_tx).await?;

    // Write PID file (only after receivers succeed)
    let pid_path = config_dir.join("daemon.pid");
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // Start log processor
    let _processor = spawn_log_processor(log_rx, pipeline.clone(), sessions.clone());

    // Create RPC handler
    let rpc_handler = Arc::new(RpcHandler::new(
        pipeline.clone(),
        sessions.clone(),
        gelf_receiver.listening_on(),
    ));

    // Listen on socket
    #[cfg(unix)]
    {
        let sock_path = config_dir.join("logmon.sock");
        // Remove stale socket if exists
        let _ = std::fs::remove_file(&sock_path);
        let listener = tokio::net::UnixListener::bind(&sock_path)?;
        eprintln!("Daemon listening on {:?}", sock_path);

        // Accept connections loop
        loop {
            let (stream, _addr) = listener.accept().await?;
            let handler = rpc_handler.clone();
            let sessions = sessions.clone();
            let pipeline = pipeline.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, handler, sessions, pipeline).await {
                    tracing::warn!("Connection error: {e}");
                }
            });
        }
    }

    #[cfg(windows)]
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:12200").await?;
        loop {
            let (stream, _addr) = listener.accept().await?;
            let handler = rpc_handler.clone();
            let sessions = sessions.clone();
            let pipeline = pipeline.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, handler, sessions, pipeline).await {
                    tracing::warn!("Connection error: {e}");
                }
            });
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&pid_path);
    #[cfg(unix)]
    let _ = std::fs::remove_file(&config_dir.join("logmon.sock"));

    Ok(())
}
```

The `handle_connection` function (generic over the stream type using `AsyncRead + AsyncWrite`):

```rust
async fn handle_connection<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: S,
    handler: Arc<RpcHandler>,
    sessions: Arc<SessionRegistry>,
    pipeline: Arc<LogPipeline>,
) -> anyhow::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);

    // 1. Read session.start request
    let start_req = transport::read_request(&mut reader).await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before session.start"))?;

    if start_req.method != "session.start" {
        let resp = RpcResponse::error(start_req.id, -32600, "first message must be session.start");
        transport::write_message(&mut writer, &resp).await?;
        return Ok(());
    }

    // 2. Validate protocol version
    let params: SessionStartParams = serde_json::from_value(start_req.params)?;
    if params.protocol_version != PROTOCOL_VERSION {
        let resp = RpcResponse::error(start_req.id, -32600,
            &format!("protocol version mismatch: expected {}, got {}", PROTOCOL_VERSION, params.protocol_version));
        transport::write_message(&mut writer, &resp).await?;
        return Ok(());
    }

    // 3. Create/reconnect session
    let session_id = match &params.name {
        Some(name) => {
            if sessions.is_disconnected_named(name) {
                sessions.reconnect_named(name)?
            } else {
                sessions.create_named(name)?
            }
        }
        None => sessions.create_anonymous(),
    };

    // Send session.start response
    let start_result = handler.build_session_start_result(&session_id);
    let resp = RpcResponse::success(start_req.id, serde_json::to_value(&start_result)?);
    transport::write_message(&mut writer, &resp).await?;

    // 4. Drain queued notifications (for reconnected named sessions)
    let queued = sessions.drain_notifications(&session_id);
    for event in queued {
        let notif = RpcNotification::new("trigger.fired", serde_json::to_value(&event)?);
        transport::write_message(&mut writer, &notif).await?;
    }

    // 5. Main RPC loop
    let event_rx = pipeline.subscribe_events();
    // ... select between reading RPC requests and forwarding notifications
    // On EOF or error: disconnect session

    sessions.disconnect(&session_id);
    Ok(())
}
```

**Idle timeout** is implemented in the accept loop: the daemon tracks connected session count. When it reaches zero, a timer starts. If no new connections arrive within `idle_timeout_secs` and no named sessions have active triggers, the daemon breaks out of the accept loop and shuts down.

**State persistence**: the log processor calls `pipeline.check_seq_block()` which returns true when a new block boundary is crossed. The daemon then writes state.json. Also written on clean shutdown.

- [ ] **Step 2: Add `new_with_seq(buffer_size, initial_seq)` to LogPipeline**

The pipeline needs to start its seq counter from the persisted block value.

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/daemon/server.rs src/engine/pipeline.rs src/daemon/mod.rs
git commit -m "feat: daemon server with socket listener and lifecycle"
```

---

## Phase 3: Shim

### Task 10: Shim Auto-Start

**Files:**
- Create: `src/shim/mod.rs`
- Create: `src/shim/auto_start.rs`
- Modify: `src/lib.rs` (add `pub mod shim;`)

- [ ] **Step 1: Implement daemon detection and auto-start**

`src/shim/auto_start.rs`:

```rust
use crate::daemon::persistence::config_dir;
use fs2::FileExt;
use std::path::PathBuf;
use std::time::Duration;

pub struct DaemonConnection {
    #[cfg(unix)]
    pub stream: tokio::net::UnixStream,
    #[cfg(windows)]
    pub stream: tokio::net::TcpStream,
}

pub async fn connect_to_daemon() -> anyhow::Result<DaemonConnection> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;

    // Acquire file lock
    let lock_path = dir.join("daemon.lock");
    let lock_file = std::fs::File::create(&lock_path)?;
    lock_file.lock_exclusive()?;

    let result = try_connect_or_start(&dir).await;

    // Release lock
    lock_file.unlock()?;

    result
}

async fn try_connect_or_start(dir: &PathBuf) -> anyhow::Result<DaemonConnection> {
    // Check PID file
    let pid_path = dir.join("daemon.pid");
    if pid_path.exists() {
        let pid_str = std::fs::read_to_string(&pid_path)?;
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_alive(pid) {
                // Daemon running, try to connect
                return connect(dir).await;
            }
        }
        // Stale PID — clean up
        let _ = std::fs::remove_file(&pid_path);
        #[cfg(unix)]
        let _ = std::fs::remove_file(dir.join("logmon.sock"));
    }

    // Start daemon
    start_daemon()?;

    // Wait for socket/port to appear (timeout 10s)
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(10) {
            anyhow::bail!("Daemon failed to start within 10 seconds");
        }
        match connect(dir).await {
            Ok(conn) => return Ok(conn),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

fn start_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped()) // or log file
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
async fn connect(dir: &PathBuf) -> anyhow::Result<DaemonConnection> {
    let stream = tokio::net::UnixStream::connect(dir.join("logmon.sock")).await?;
    Ok(DaemonConnection { stream })
}

#[cfg(windows)]
async fn connect(dir: &PathBuf) -> anyhow::Result<DaemonConnection> {
    let stream = tokio::net::TcpStream::connect("127.0.0.1:12200").await?;
    Ok(DaemonConnection { stream })
}

fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
```

No `unsafe` or `libc` dependency — uses the `kill -0` command which works on macOS and Linux. On Windows, use `tasklist /FI "PID eq {pid}"` instead.

- [ ] **Step 2: Verify compilation**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/shim/ src/lib.rs Cargo.toml
git commit -m "feat: shim auto-start with daemon detection"
```

---

### Task 11: Shim Bridge (MCP ↔ RPC)

**Files:**
- Create: `src/shim/bridge.rs`
- Modify: `src/mcp/server.rs` (tools now send RPC instead of calling pipeline)
- Modify: `src/mcp/notifications.rs` (forward RPC notifications to MCP)

- [ ] **Step 1: Implement the shim bridge**

The bridge connects MCP tool handlers to the daemon via RPC. The MCP tool handlers need access to the daemon connection to send requests and receive responses.

`src/shim/bridge.rs`:

```rust
use crate::rpc::types::*;
use crate::rpc::transport;
use std::sync::Arc;
use tokio::io::{BufReader, WriteHalf, ReadHalf};
use tokio::sync::{Mutex, oneshot, broadcast};
use std::collections::HashMap;

/// Bridge between shim and daemon
/// Handles multiplexing: sends requests, matches responses by id, forwards notifications
/// The writer is type-erased behind a trait object for platform independence.
pub struct DaemonBridge {
    writer: Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>,
    next_id: AtomicU64,
    notification_tx: broadcast::Sender<RpcNotification>,
}

impl DaemonBridge {
    /// Send an RPC request and wait for the response
    pub async fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest::new(id, method, params);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        // Write request
        let mut writer = self.writer.lock().await;
        transport::write_message(&mut *writer, &request).await?;

        // Wait for response
        let response = rx.await?;
        match response.error {
            Some(err) => anyhow::bail!("{}", err.message),
            None => Ok(response.result.unwrap_or(serde_json::Value::Null)),
        }
    }

    /// Subscribe to daemon notifications
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<RpcNotification> {
        self.notification_tx.subscribe()
    }
}

/// Spawns reader loop that routes responses to pending requests and notifications to broadcast
pub fn spawn_reader_loop(bridge: Arc<DaemonBridge>, reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        loop {
            match transport::read_daemon_message(&mut reader).await {
                Ok(Some(DaemonMessage::Response(resp))) => {
                    if let Some(tx) = bridge.pending.lock().await.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
                Ok(Some(DaemonMessage::Notification(notif))) => {
                    let _ = bridge.notification_tx.send(notif);
                }
                Ok(None) => break, // EOF — daemon disconnected
                Err(e) => {
                    eprintln!("RPC read error: {e}");
                    break;
                }
            }
        }
        // Daemon disconnected — trigger reconnection:
        // 1. Send MCP notification to Claude: "logmon daemon disconnected"
        // 2. Call connect_to_daemon() (auto-start procedure)
        // 3. Re-send session.start with same session name
        // 4. Send MCP notification: "logmon daemon reconnected" with state summary
        // For anonymous sessions, triggers/filters are lost — inform Claude
    });
}
```

- [ ] **Step 2: Modify MCP server to use DaemonBridge**

`GelfMcpServer` now holds `Arc<DaemonBridge>` instead of `Arc<LogPipeline>`. Each tool handler calls `self.bridge.call("method", params).await`.

Example for `get_recent_logs`:
```rust
#[rmcp::tool(description = "Fetch recent log entries...")]
async fn get_recent_logs(&self, Parameters(params): Parameters<GetRecentLogsParams>) -> Result<CallToolResult, rmcp::ErrorData> {
    let result = self.bridge.call("logs.recent", serde_json::to_value(&params).unwrap()).await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap()
    )]))
}
```

All tools follow the same pattern: serialize params → call bridge → return result.

**Path resolution** for `export_logs`: the shim resolves relative paths before sending:
```rust
// In the export_logs tool handler, before calling bridge:
if let Some(ref path) = params.path {
    if !std::path::Path::new(path).is_absolute() {
        params.path = Some(std::env::current_dir()?.join(path).to_string_lossy().to_string());
    }
}
```

**`run_shim` function** (called from main.rs):
```rust
pub async fn run_shim(conn: DaemonConnection, session_name: Option<String>) -> anyhow::Result<()> {
    // Split the connection stream
    let (reader, writer) = tokio::io::split(conn.stream);

    // Create bridge
    let (notification_tx, _) = broadcast::channel(100);
    let bridge = Arc::new(DaemonBridge {
        writer: Mutex::new(Box::new(writer)),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        notification_tx: notification_tx.clone(),
    });

    // Spawn reader loop
    spawn_reader_loop(bridge.clone(), Box::new(reader));

    // Send session.start
    let start_result = bridge.call("session.start", serde_json::to_value(&SessionStartParams {
        name: session_name,
        protocol_version: PROTOCOL_VERSION,
    })?).await?;

    let session_info: SessionStartResult = serde_json::from_value(start_result)?;
    eprintln!("Connected to daemon: session={}, new={}", session_info.session_id, session_info.is_new);

    // Create MCP server with bridge
    let server = GelfMcpServer::new_with_bridge(bridge.clone(), session_info);
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server.serve(transport).await?;

    // Forward daemon notifications to MCP
    let notification_rx = bridge.subscribe_notifications();
    spawn_notification_forwarder(notification_rx, running.peer().clone());

    running.waiting().await?;

    // Send session.stop
    let _ = bridge.call("session.stop", serde_json::json!({})).await;
    Ok(())
}
```

- [ ] **Step 3: Modify notifications to forward from daemon RPC**

`notifications.rs` now subscribes to `bridge.subscribe_notifications()` instead of pipeline events.

- [ ] **Step 4: Verify compilation**

Run: `cargo build`

- [ ] **Step 5: Commit**

```bash
git add src/shim/bridge.rs src/mcp/server.rs src/mcp/notifications.rs
git commit -m "feat: shim bridge (MCP tools → daemon RPC)"
```

---

## Phase 4: Integration

### Task 12: Config Refactor + Main Rewrite

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Refactor config with clap subcommands**

```rust
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "logmon-mcp-server")]
pub struct Cli {
    /// Session name (for named persistent sessions)
    #[arg(long)]
    pub session: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run as daemon (long-running log collector)
    Daemon {
        #[arg(long, default_value = "12201")]
        gelf_port: u16,
        #[arg(long)]
        gelf_udp_port: Option<u16>,
        #[arg(long)]
        gelf_tcp_port: Option<u16>,
        #[arg(long, default_value = "10000")]
        buffer_size: usize,
    },
}
```

No subcommand = shim mode. `daemon` subcommand = daemon mode.

- [ ] **Step 2: Rewrite main.rs**

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(/* ... */)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { gelf_port, gelf_udp_port, gelf_tcp_port, buffer_size }) => {
            // Load config file, CLI overrides
            let mut config = load_config(&config_dir().join("config.json"))?;
            config.gelf_port = gelf_port;
            if let Some(p) = gelf_udp_port { config.gelf_udp_port = Some(p); }
            if let Some(p) = gelf_tcp_port { config.gelf_tcp_port = Some(p); }
            config.buffer_size = buffer_size;
            daemon::server::run_daemon(config).await
        }
        None => {
            // Shim mode
            let conn = shim::auto_start::connect_to_daemon().await?;
            shim::bridge::run_shim(conn, cli.session).await
        }
    }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build`

- [ ] **Step 4: Commit**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: CLI dispatch (shim mode vs daemon mode)"
```

---

### Task 13: New MCP Tools (get_sessions, drop_session)

**Files:**
- Modify: `src/mcp/server.rs`

- [ ] **Step 1: Add get_sessions and drop_session tools**

```rust
#[rmcp::tool(description = "List all sessions connected to the log collector")]
async fn get_sessions(&self) -> Result<CallToolResult, rmcp::ErrorData> {
    let result = self.bridge.call("session.list", serde_json::json!({})).await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap()
    )]))
}

#[derive(Deserialize, JsonSchema)]
struct DropSessionParams {
    name: String,
}

#[rmcp::tool(description = "Remove a named session and all its state")]
async fn drop_session(&self, Parameters(params): Parameters<DropSessionParams>) -> Result<CallToolResult, rmcp::ErrorData> {
    let result = self.bridge.call("session.drop", serde_json::to_value(&params).unwrap()).await
        .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap()
    )]))
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo build`

- [ ] **Step 3: Commit**

```bash
git add src/mcp/server.rs
git commit -m "feat: get_sessions and drop_session MCP tools"
```

---

### Task 14: Integration Tests

**Files:**
- Create: `tests/multi_session.rs`

- [ ] **Step 1: Write multi-session integration tests**

These tests use `process_entry` directly with the `SessionRegistry` and `LogPipeline` (avoiding socket/daemon startup complexity). The socket-level integration can be tested manually or in a dedicated E2E test later.

```rust
use logmon_mcp_server::daemon::log_processor::process_entry;
use logmon_mcp_server::daemon::session::SessionRegistry;
use logmon_mcp_server::engine::pipeline::LogPipeline;
use logmon_mcp_server::gelf::message::{LogEntry, Level, LogSource};
use logmon_mcp_server::filter::parser::parse_filter;
use logmon_mcp_server::filter::matcher::matches_entry;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

fn make_entry(level: Level, msg: &str, facility: &str) -> LogEntry {
    LogEntry {
        seq: 0, timestamp: Utc::now(), level,
        message: msg.to_string(), full_message: None,
        host: "test".into(), facility: Some(facility.into()),
        file: None, line: None,
        additional_fields: HashMap::new(),
        matched_filters: Vec::new(), source: LogSource::Filter,
    }
}

#[test]
fn test_two_sessions_share_logs() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let _sid_a = sessions.create_anonymous();
    let _sid_b = sessions.create_anonymous();

    let mut entry = make_entry(Level::Info, "shared log", "app::main");
    process_entry(&mut entry, &pipeline, &sessions);

    // Both sessions can see the log
    let filter = parse_filter("ALL").unwrap();
    let logs = pipeline.recent_logs(10, Some(&filter));
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "shared log");
}

#[test]
fn test_per_session_filter_lens() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid_a = sessions.create_anonymous();
    let sid_b = sessions.create_anonymous();

    sessions.add_filter(&sid_a, "fa=mqtt", None).unwrap();
    sessions.add_filter(&sid_b, "fa=http", None).unwrap();

    // Send mqtt log and http log
    let mut mqtt_entry = make_entry(Level::Info, "mqtt msg", "app::mqtt");
    process_entry(&mut mqtt_entry, &pipeline, &sessions);
    let mut http_entry = make_entry(Level::Info, "http msg", "app::http");
    process_entry(&mut http_entry, &pipeline, &sessions);

    // Both are stored (union of filters)
    assert_eq!(pipeline.store_len(), 2);

    // Session A's default lens sees only mqtt
    let filter_a = parse_filter("fa=mqtt").unwrap();
    let logs_a = pipeline.recent_logs(10, Some(&filter_a));
    assert_eq!(logs_a.len(), 1);
    assert_eq!(logs_a[0].message, "mqtt msg");

    // Session B's default lens sees only http
    let filter_b = parse_filter("fa=http").unwrap();
    let logs_b = pipeline.recent_logs(10, Some(&filter_b));
    assert_eq!(logs_b.len(), 1);
    assert_eq!(logs_b[0].message, "http msg");
}

#[test]
fn test_named_session_survives_disconnect() {
    let pipeline = Arc::new(LogPipeline::new(1000));
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_named("persistent").unwrap();

    // Add custom trigger
    sessions.add_trigger(&sid, "fa=special", 100, 50, 3, Some("custom")).unwrap();
    let triggers_before = sessions.list_triggers(&sid);
    assert_eq!(triggers_before.len(), 3); // 2 defaults + 1 custom

    // Disconnect
    sessions.disconnect(&sid);
    assert!(!sessions.is_connected(&sid));

    // Triggers still exist
    let triggers_after = sessions.list_triggers(&sid);
    assert_eq!(triggers_after.len(), 3);

    // Reconnect
    sessions.reconnect(&sid).unwrap();
    assert!(sessions.is_connected(&sid));
}

#[test]
fn test_anonymous_session_cleanup_on_disconnect() {
    let sessions = Arc::new(SessionRegistry::new());
    let sid = sessions.create_anonymous();
    assert_eq!(sessions.list().len(), 1);
    sessions.disconnect(&sid);
    assert_eq!(sessions.list().len(), 0);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test --test multi_session`

- [ ] **Step 3: Commit**

```bash
git add tests/multi_session.rs
git commit -m "test: multi-session integration tests"
```

---

### Task 15: Update Skill File

**Files:**
- Modify: `skill/logmon.md`

- [ ] **Step 1: Update skill to document sessions**

Add to the skill file:
- `--session <name>` flag for persistent sessions
- `get_sessions` and `drop_session` tools
- Workflow: "Monitoring a specific component across sessions"
- Note about shared buffer (clear_logs affects all sessions)

- [ ] **Step 2: Commit**

```bash
git add skill/logmon.md
git commit -m "docs: update skill for multi-session support"
```

---

### Task 16: Final Polish

- [ ] **Step 1: Run full test suite**

Run: `cargo test`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`

- [ ] **Step 3: Fix any issues**

- [ ] **Step 4: Release build**

Run: `cargo build --release`

- [ ] **Step 5: Commit and verify**

```bash
git add -A
git commit -m "chore: clippy fixes and final polish"
```
