use crate::engine::pre_buffer::PreTriggerBuffer;
use crate::engine::seq_counter::SeqCounter;
use crate::filter::parser::ParsedFilter;
use crate::gelf::message::LogEntry;
use crate::store::memory::InMemoryStore;
use crate::store::traits::{LogStore, StoreStats};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Event emitted when a trigger fires (used by daemon's session logic)
#[derive(Clone, serde::Serialize)]
pub struct PipelineEvent {
    pub trigger_id: u32,
    pub trigger_description: Option<String>,
    pub filter_string: String,
    pub matched_entry: LogEntry,
    pub context_before: Vec<LogEntry>,
    pub pre_trigger_flushed: usize,
    pub post_window_size: u32,
    pub trace_id: Option<u128>,
    pub trace_summary: Option<crate::span::types::TraceSummary>,
}

/// Buffer filter info (used by session registry)
pub struct FilterInfo {
    pub id: u32,
    pub filter_string: String,
    pub description: Option<String>,
}

/// Shared infrastructure: store + pre-buffer + seq counter.
/// Per-session logic (triggers, filters, process flow) lives in the daemon.
pub struct LogPipeline {
    store: InMemoryStore,
    pre_buffer: PreTriggerBuffer,
    seq_counter: Arc<SeqCounter>,
    event_sender: broadcast::Sender<PipelineEvent>,
}

impl LogPipeline {
    pub fn new(store_capacity: usize) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        Self {
            store: InMemoryStore::new(store_capacity),
            pre_buffer: PreTriggerBuffer::new(0),
            seq_counter: Arc::new(SeqCounter::new()),
            event_sender,
        }
    }

    pub fn new_with_seq(store_capacity: usize, initial_seq: u64) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        Self {
            store: InMemoryStore::new(store_capacity),
            pre_buffer: PreTriggerBuffer::new(0),
            seq_counter: Arc::new(SeqCounter::new_with_initial(initial_seq)),
            event_sender,
        }
    }

    pub fn new_with_seq_counter(store_capacity: usize, seq_counter: Arc<SeqCounter>) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        Self {
            store: InMemoryStore::new(store_capacity),
            pre_buffer: PreTriggerBuffer::new(0),
            seq_counter,
            event_sender,
        }
    }

    pub fn assign_seq(&self) -> u64 {
        self.seq_counter.next()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<PipelineEvent> {
        self.event_sender.subscribe()
    }

    pub fn send_event(&self, event: PipelineEvent) {
        let _ = self.event_sender.send(event);
    }

    // --- Store operations ---

    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    pub fn store_stats(&self) -> StoreStats {
        self.store.stats()
    }

    pub fn increment_malformed(&self) {
        self.store.increment_malformed();
    }

    pub fn append_to_store(&self, entry: LogEntry) {
        self.store.append(entry);
    }

    pub fn contains_seq(&self, seq: u64) -> bool {
        self.store.contains_seq(seq)
    }

    pub fn clear_logs(&self) -> usize {
        let count = self.store.len();
        self.store.clear();
        count
    }

    pub fn recent_logs(&self, count: usize, filter: Option<&ParsedFilter>) -> Vec<LogEntry> {
        self.store.recent(count, filter)
    }

    pub fn oldest_log_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.store.oldest_timestamp()
    }

    pub fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry> {
        self.store.context_by_seq(seq, before, after)
    }

    pub fn context_by_time(
        &self,
        timestamp: chrono::DateTime<chrono::Utc>,
        window: std::time::Duration,
    ) -> Vec<LogEntry> {
        self.store.context_by_time(timestamp, window)
    }

    /// Return log entries from the store matching the given trace_id.
    pub fn logs_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        self.store.logs_by_trace_id(trace_id)
    }

    /// Return count of log entries in the store matching the given trace_id.
    pub fn count_by_trace_id(&self, trace_id: u128) -> usize {
        self.store.count_by_trace_id(trace_id)
    }

    // --- Pre-buffer operations ---

    pub fn pre_buffer_append(&self, entry: LogEntry) {
        self.pre_buffer.append(entry);
    }

    pub fn pre_buffer_copy(&self, count: usize) -> Vec<LogEntry> {
        self.pre_buffer.flush(count)
    }

    pub fn resize_pre_buffer(&self, size: usize) {
        self.pre_buffer.resize(size);
    }

    /// Return copies of pre-buffer entries matching the given trace_id.
    pub fn pre_buffer_entries_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        self.pre_buffer.entries_by_trace_id(trace_id)
    }
}
