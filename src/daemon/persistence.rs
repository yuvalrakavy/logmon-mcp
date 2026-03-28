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
    #[serde(default = "default_otlp_grpc_port")]
    pub otlp_grpc_port: u16,
    #[serde(default = "default_otlp_http_port")]
    pub otlp_http_port: u16,
    #[serde(default = "default_span_buffer_size")]
    pub span_buffer_size: usize,
}

fn default_gelf_port() -> u16 {
    12201
}
fn default_buffer_size() -> usize {
    10000
}
fn default_idle_timeout() -> u64 {
    1800
}
fn default_otlp_grpc_port() -> u16 {
    4317
}
fn default_otlp_http_port() -> u16 {
    4318
}
fn default_span_buffer_size() -> usize {
    10000
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            gelf_port: 12201,
            gelf_udp_port: None,
            gelf_tcp_port: None,
            buffer_size: 10000,
            persist_buffer_on_exit: false,
            idle_timeout_secs: 1800,
            otlp_grpc_port: 4317,
            otlp_http_port: 4318,
            span_buffer_size: 10000,
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
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".config").join("logmon")
}
