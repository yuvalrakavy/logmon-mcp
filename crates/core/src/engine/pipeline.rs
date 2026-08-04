use crate::engine::epoch::EpochLog;
use crate::engine::pre_buffer::PreTriggerBuffer;
use crate::engine::epoch::FilterPolicy;
use crate::engine::seq_counter::SeqCounter;
use crate::filter::parser::ParsedFilter;
use crate::gelf::message::LogEntry;
use crate::store::memory::InMemoryStore;
use crate::store::traits::{LogStore, StoreStats};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Event emitted when a trigger fires (used by daemon's session logic).
///
/// Fields here include both the engine-internal observability data
/// (`pre_trigger_flushed`, `trace_id`, `trace_summary`) and the data needed
/// to construct the wire-level [`logmon_broker_protocol::TriggerFiredPayload`]
/// (`pre_window`, `notify_context`, `oneshot`). The wire conversion happens at
/// the serialization site in `daemon::server`; this struct is not serialized
/// directly to clients.
///
/// The `session_id` field tags every event with the session it belongs to.
/// This is the single broadcast for all connected clients, so each
/// connection filters by `session_id` before forwarding to its socket.
#[derive(Clone, serde::Serialize)]
pub struct PipelineEvent {
    /// The session this event belongs to, as the daemon-internal string form
    /// of [`crate::daemon::session::SessionId`]. Used for fan-out filtering.
    pub session_id: String,
    pub trigger_id: u32,
    pub trigger_description: Option<String>,
    pub filter_string: String,
    pub matched_entry: LogEntry,
    pub context_before: Vec<LogEntry>,
    /// The actual count flushed from the pre-buffer for this match
    /// (`min(pre_window, available)`). Engine-internal; not on the wire.
    pub pre_trigger_flushed: usize,
    /// The trigger's configured `pre_window`, propagated to the wire payload.
    pub pre_window: u32,
    pub post_window_size: u32,
    /// The trigger's configured `notify_context`, propagated to the wire payload.
    pub notify_context: u32,
    /// The trigger's configured `oneshot` flag. The log/span processor uses
    /// this to decide whether to remove the trigger after dispatch; the wire
    /// payload also carries it so clients can know this is the last fire.
    pub oneshot: bool,
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
    /// What storage policy governed each stretch of this pipeline's seq axis.
    /// Lives here rather than on `Domain` because it is indexed by *this*
    /// counter and answers questions about *this* store — and because it must
    /// reach the log processor, which is handed the pipeline and nothing else.
    epochs: EpochLog,
}

/// Buffer/scan statistics returned alongside a `recent_logs_with_stats` query,
/// powering the B2 `scanned` / `buffer_*` response fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecentStats {
    pub scanned: usize,
    pub buffer_total: usize,
    pub buffer_oldest_seq: Option<u64>,
    pub buffer_newest_seq: Option<u64>,
}

impl LogPipeline {
    pub fn new(store_capacity: usize) -> Self {
        Self::new_with_seq_counter(store_capacity, Arc::new(SeqCounter::new()))
    }

    pub fn new_with_seq(store_capacity: usize, initial_seq: u64) -> Self {
        Self::new_with_seq_counter(
            store_capacity,
            Arc::new(SeqCounter::new_with_initial(initial_seq)),
        )
    }

    /// Build a pipeline already holding a loaded case's records.
    ///
    /// **`epoch_origin` is a parameter, not `seq_counter.current()`, and that is
    /// the whole point of this constructor.** Three consumers constrain a loaded
    /// domain's seq axis and they pull against each other:
    ///
    /// - `logs.export` with no range takes its upper end from `current_seq()`,
    ///   so the counter must sit at the window's TOP or the default window
    ///   inverts and every export reports `cannot_verify`;
    /// - `EpochLog::coverage` needs `origin <= from` to vouch for the window;
    /// - `spans.export` floors at `max(lost_below, origin + 1)`, so an origin of
    ///   `from` makes every span export report phantom truncation.
    ///
    /// One value cannot satisfy all three, and [`Self::new_with_seq_counter`]
    /// hard-wires `EpochLog::new(seq_counter.current())`. Splitting them lets the
    /// counter sit at `to` and the origin at `from - 1`, which satisfies all
    /// three at once.
    ///
    /// `narrowing` gives the policy TRANSITIONS the case recorded: a stretch
    /// that was filtered opens with its filters and closes by returning to
    /// unfiltered. Empty means unfiltered throughout.
    ///
    /// **A baseline unfiltered epoch is always written first, and it has to be.**
    /// `EpochLog::coverage` returns `EpochCoverage::default()` — `covered: false`
    /// — for an EMPTY log, documented as "silence is not evidence of an
    /// unfiltered store". So seeding only the narrowed stretches leaves an
    /// unfiltered case with an empty log, and every export over it reports
    /// `cannot_verify` about a window the case calls complete. Caught by the
    /// unbounded-export test, which is the one shape that shows it.
    pub fn from_records(
        store_capacity: usize,
        seq_counter: Arc<SeqCounter>,
        epoch_origin: u64,
        narrowing: &[(u64, FilterPolicy)],
        records: Vec<LogEntry>,
        lost_below: u64,
    ) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        let log = EpochLog::new(epoch_origin);
        // The first observation opens its epoch at the ORIGIN rather than at the
        // seq observed (`epoch.rs`, `None => self.origin_seq`), so this one
        // covers everything from the window's floor upward.
        log.observe(epoch_origin, &FilterPolicy::new(Vec::new()));
        for (seq, policy) in narrowing {
            log.observe(*seq, policy);
        }
        Self {
            store: InMemoryStore::from_records(store_capacity, records, lost_below),
            pre_buffer: PreTriggerBuffer::new(0),
            seq_counter,
            event_sender,
            epochs: log,
        }
    }

    pub fn new_with_seq_counter(store_capacity: usize, seq_counter: Arc<SeqCounter>) -> Self {
        let (event_sender, _) = broadcast::channel(100);
        // The epoch log opens at wherever this counter starts. For a restored
        // domain that is the seq the previous incarnation reached, so a window
        // carried over from that run falls below the log and reports
        // `cannot_verify` rather than inheriting this run's policy.
        let epochs = EpochLog::new(seq_counter.current());
        Self {
            store: InMemoryStore::new(store_capacity),
            pre_buffer: PreTriggerBuffer::new(0),
            seq_counter,
            event_sender,
            epochs,
        }
    }

    /// This pipeline's record of the storage policy over its seq axis — see
    /// [`EpochLog`]. Written by the log processor as it decides storage; read
    /// by `logs.export` to say how much of a window it can vouch for.
    pub fn epochs(&self) -> &EpochLog {
        &self.epochs
    }

    /// The lowest seq the log store can still speak for; `0` when nothing has
    /// ever left it. Not [`Self::oldest_log_seq`] — see
    /// [`InMemoryStore::lost_below`].
    ///
    /// [`InMemoryStore::lost_below`]: crate::store::memory::InMemoryStore::lost_below
    pub fn lost_below(&self) -> u64 {
        self.store.lost_below()
    }

    pub fn assign_seq(&self) -> u64 {
        self.seq_counter.next()
    }

    /// Current value of the shared seq counter (the highest seq assigned so
    /// far, or `0` if none). Used by `bookmarks.add` to default `start_seq`
    /// to "the cursor we'd hand out right now".
    pub fn current_seq(&self) -> u64 {
        self.seq_counter.current()
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

    pub fn recent_logs(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> Vec<LogEntry> {
        self.store.recent(count, filter, oldest_first)
    }

    /// Like `recent_logs`, but also returns buffer/scan stats for B2's
    /// `scanned` / `buffer_*` response fields (one query; snapshot-consistent
    /// enough for a diagnostic count).
    pub fn recent_logs_with_stats(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> (Vec<LogEntry>, RecentStats) {
        let (entries, scanned) = self.store.recent_with_scanned(count, filter, oldest_first);
        let stats = RecentStats {
            scanned,
            buffer_total: self.store.len(),
            buffer_oldest_seq: self.store.oldest_seq(),
            buffer_newest_seq: self.store.newest_seq(),
        };
        (entries, stats)
    }

    /// Fold the **whole** matching population without materialising it.
    ///
    /// The seam every population-describing read needs.
    /// `recent_logs_with_stats` cannot serve them: it early-stops at `count`,
    /// so a summary built on it would describe the newest slice of the buffer
    /// while claiming to describe the buffer.
    ///
    /// Returns the walk's counts alongside the same buffer facts
    /// `recent_logs_with_stats` reports, so a caller can state its population
    /// and its completeness from one query.
    pub fn for_each_log<F>(
        &self,
        filter: Option<&ParsedFilter>,
        f: F,
    ) -> (crate::store::memory::ScanCounts, RecentStats)
    where
        F: FnMut(&LogEntry),
    {
        let counts = self.store.for_each_matching(filter, f);
        let stats = RecentStats {
            scanned: counts.scanned,
            buffer_total: self.store.len(),
            buffer_oldest_seq: self.store.oldest_seq(),
            buffer_newest_seq: self.store.newest_seq(),
        };
        (counts, stats)
    }

    pub fn oldest_log_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.store.oldest_timestamp()
    }

    /// Seq of the oldest log entry currently in the store, or `None` if empty.
    /// Drives bookmark eviction — see `bookmarks::should_evict`.
    pub fn oldest_log_seq(&self) -> Option<u64> {
        self.store.oldest_seq()
    }

    /// Seq of the newest log entry currently in the store, or `None` if empty.
    pub fn newest_log_seq(&self) -> Option<u64> {
        self.store.newest_seq()
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
