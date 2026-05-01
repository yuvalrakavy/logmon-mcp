use crate::gelf::message::LogEntry;
use crate::filter::parser::ParsedFilter;
use chrono::{DateTime, Utc};
use std::time::Duration;

pub struct StoreStats {
    pub total_received: u64,
    pub total_stored: u64,
    pub malformed_count: u64,
}

pub trait LogStore: Send + Sync {
    fn append(&self, entry: LogEntry);
    fn recent(&self, count: usize, filter: Option<&ParsedFilter>) -> Vec<LogEntry>;
    fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry>;
    fn context_by_time(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry>;
    fn contains_seq(&self, seq: u64) -> bool;
    fn clear(&self);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn logs_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry>;
    fn count_by_trace_id(&self, trace_id: u128) -> usize;
    fn stats(&self) -> StoreStats;
    fn oldest_timestamp(&self) -> Option<DateTime<Utc>>;
    /// Seq of the oldest entry currently in the buffer, or `None` if empty.
    /// Used by the bookmark sweep — see `crate::store::bookmarks::should_evict`.
    fn oldest_seq(&self) -> Option<u64>;
}
