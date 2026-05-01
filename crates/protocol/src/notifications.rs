//! Typed notification payloads for `notifications/*` server-pushed messages.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::LogEntry;

/// Payload of `notifications/trigger_fired`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TriggerFiredPayload {
    pub trigger_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub filter_string: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    /// New in v1; mirrors the `oneshot` param on `triggers.add`. Tells the
    /// client this is the only fire it will see for this trigger ID.
    #[serde(default)]
    pub oneshot: bool,
    pub matched_entry: LogEntry,
    /// Context entries that arrived before the matched_entry. Size at most
    /// `notify_context`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_before: Vec<LogEntry>,
}
