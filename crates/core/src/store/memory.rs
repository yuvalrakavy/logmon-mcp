use crate::gelf::message::LogEntry;
use crate::filter::parser::ParsedFilter;
use crate::filter::matcher::matches_entry;
use crate::store::traits::{LogStore, StoreStats};
use chrono::{DateTime, Utc};
use std::collections::{VecDeque, HashSet, HashMap};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Inner state guarded by a single `RwLock`. Collapsing the three
/// previously-separate `entries`/`seq_set`/`trace_index` locks into
/// one structurally prevents lock-ordering bugs (the previous
/// configuration had `append` and `logs_by_trace_id` taking the
/// inner locks in opposite orders, which deadlocked under concurrent
/// load — see notes/2026-05-05-logmon-broker-rwlock-bug-findings.md).
/// Mirrors the single-lock pattern in `SpanStore`.
struct StoreInner {
    entries: VecDeque<LogEntry>,
    seq_set: HashSet<u64>,
    trace_index: HashMap<u128, Vec<u64>>,
}

pub struct InMemoryStore {
    inner: RwLock<StoreInner>,
    max_capacity: usize,
    total_stored: AtomicU64,
    total_received: AtomicU64,
    malformed_count: AtomicU64,
}

impl InMemoryStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: RwLock::new(StoreInner {
                entries: VecDeque::with_capacity(capacity),
                seq_set: HashSet::new(),
                trace_index: HashMap::new(),
            }),
            max_capacity: capacity,
            total_stored: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            malformed_count: AtomicU64::new(0),
        }
    }

    pub fn increment_malformed(&self) {
        self.malformed_count.fetch_add(1, Ordering::Relaxed);
    }
}

impl LogStore for InMemoryStore {
    fn append(&self, entry: LogEntry) {
        self.total_received.fetch_add(1, Ordering::Relaxed);
        self.total_stored.fetch_add(1, Ordering::Relaxed);

        let seq = entry.seq;
        let trace_id = entry.trace_id;
        let mut inner = self.inner.write().unwrap();

        if inner.entries.len() >= self.max_capacity {
            if let Some(evicted) = inner.entries.pop_front() {
                inner.seq_set.remove(&evicted.seq);
                if let Some(evicted_trace) = evicted.trace_id {
                    if let Some(seqs) = inner.trace_index.get_mut(&evicted_trace) {
                        seqs.retain(|&s| s != evicted.seq);
                        if seqs.is_empty() {
                            inner.trace_index.remove(&evicted_trace);
                        }
                    }
                }
            }
        }

        inner.entries.push_back(entry);
        inner.seq_set.insert(seq);
        if let Some(tid) = trace_id {
            inner.trace_index.entry(tid).or_default().push(seq);
        }
    }

    fn recent(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();
        let mut result = Vec::new();

        if oldest_first {
            // Cursor-driven path: walk forward (oldest → newest) and take the
            // first `count` filter-matching records. Pagination drains
            // monotonically across calls.
            for entry in inner.entries.iter() {
                if let Some(f) = filter {
                    if !matches_entry(f, entry) {
                        continue;
                    }
                }
                result.push(entry.clone());
                if result.len() >= count {
                    break;
                }
            }
        } else {
            // Default path: newest-first, preserves prior behavior.
            for entry in inner.entries.iter().rev() {
                if let Some(f) = filter {
                    if !matches_entry(f, entry) {
                        continue;
                    }
                }
                result.push(entry.clone());
                if result.len() >= count {
                    break;
                }
            }
        }

        result
    }

    fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();

        // Find index of the entry with the given seq
        let idx = match inner.entries.iter().position(|e| e.seq == seq) {
            Some(i) => i,
            None => return Vec::new(),
        };

        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(inner.entries.len());

        inner.entries.range(start..end).cloned().collect()
    }

    fn context_by_time(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();
        let window_ns = window.as_nanos() as i64;

        inner.entries.iter()
            .filter(|e| {
                let diff = (e.timestamp - timestamp).num_nanoseconds()
                    .map(|ns| ns.abs())
                    .unwrap_or(i64::MAX);
                diff <= window_ns
            })
            .cloned()
            .collect()
    }

    fn contains_seq(&self, seq: u64) -> bool {
        self.inner.read().unwrap().seq_set.contains(&seq)
    }

    fn logs_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();
        let Some(seqs) = inner.trace_index.get(&trace_id) else {
            return Vec::new();
        };
        let seq_set: HashSet<u64> = seqs.iter().copied().collect();
        inner.entries.iter()
            .filter(|e| seq_set.contains(&e.seq))
            .cloned()
            .collect()
    }

    fn count_by_trace_id(&self, trace_id: u128) -> usize {
        let inner = self.inner.read().unwrap();
        inner.trace_index.get(&trace_id).map_or(0, |seqs| seqs.len())
    }

    fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.entries.clear();
        inner.seq_set.clear();
        inner.trace_index.clear();
    }

    fn len(&self) -> usize {
        self.inner.read().unwrap().entries.len()
    }

    fn stats(&self) -> StoreStats {
        StoreStats {
            total_received: self.total_received.load(Ordering::Relaxed),
            total_stored: self.total_stored.load(Ordering::Relaxed),
            malformed_count: self.malformed_count.load(Ordering::Relaxed),
        }
    }

    fn oldest_timestamp(&self) -> Option<DateTime<Utc>> {
        self.inner.read().unwrap().entries.front().map(|e| e.timestamp)
    }

    fn oldest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().entries.front().map(|e| e.seq)
    }
}

#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::gelf::message::{LogEntry, Level, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;

    fn entry(seq: u64) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "m".to_string(),
            full_message: None,
            host: "h".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    #[test]
    fn oldest_timestamp_empty_store_returns_none() {
        let store = InMemoryStore::new(10);
        assert!(store.oldest_timestamp().is_none());
    }

    #[test]
    fn oldest_timestamp_returns_front_entry_timestamp() {
        let store = InMemoryStore::new(10);
        let mut e1 = entry(1);
        e1.timestamp = Utc::now() - chrono::Duration::seconds(60);
        let mut e2 = entry(2);
        e2.timestamp = Utc::now();
        store.append(e1.clone());
        store.append(e2);
        assert_eq!(store.oldest_timestamp(), Some(e1.timestamp));
    }
}

#[cfg(test)]
mod concurrency_tests {
    //! Regression tests for the single-RwLock invariant.
    //!
    //! Before the 2026-05-05 fix, `InMemoryStore` held three separate
    //! `RwLock`s and `append` / `logs_by_trace_id` acquired them in
    //! opposite orders. Concurrent writer + reader → ABBA deadlock,
    //! observed in production after several hours of test load.
    //! These tests pin the contract: append + logs_by_trace_id +
    //! len from many concurrent OS threads must complete promptly,
    //! never wedging.

    use super::*;
    use crate::gelf::message::{LogEntry, Level, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn entry(seq: u64) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "m".to_string(),
            full_message: None,
            host: "h".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    /// Hammer `append` (writer) and `logs_by_trace_id` + `len` (readers)
    /// concurrently from real OS threads. Pre-fix, this wedges within
    /// a few thousand iterations on multi-core hardware. Post-fix, it
    /// completes in well under a second.
    ///
    /// Uses `spawn_blocking` to guarantee the workers actually run on
    /// separate OS threads (the `multi_thread` runtime alone may
    /// cooperatively schedule them on one worker, which masks the bug).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_and_logs_by_trace_id_do_not_deadlock() {
        // Small workload: enough contention to reliably trigger ABBA pre-fix
        // (which it did within seconds during diagnosis), but cheap enough
        // that the post-fix fair-RwLock contention completes well under
        // the watchdog. The bug is structural — small workload exposes it
        // just as well as a large one. A heavier workload pushes the test
        // past 20 s on contention alone, which would mask real regressions.
        let store = Arc::new(InMemoryStore::new(1_000));

        let writer_store = Arc::clone(&store);
        let writer = tokio::task::spawn_blocking(move || {
            for i in 0..5_000u64 {
                let mut e = entry(i);
                e.trace_id = Some((i % 16) as u128);
                writer_store.append(e);
            }
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&store);
            readers.push(tokio::task::spawn_blocking(move || {
                for _ in 0..1_000 {
                    let _ = s.logs_by_trace_id(0);
                    let _ = s.len();
                }
            }));
        }

        let work = async {
            writer.await.unwrap();
            for r in readers {
                r.await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("deadlock — append/logs_by_trace_id ABBA regression");
    }

    /// Same shape but with `clear` mixed in. `clear` previously took the
    /// three locks in the same order as `append`, but a future refactor
    /// that flipped its order would re-introduce ABBA against
    /// `logs_by_trace_id` — this guards it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clear_does_not_deadlock_with_readers() {
        let store = Arc::new(InMemoryStore::new(10_000));

        for i in 0..1_000u64 {
            let mut e = entry(i);
            e.trace_id = Some((i % 8) as u128);
            store.append(e);
        }

        let clear_store = Arc::clone(&store);
        let clearer = tokio::task::spawn_blocking(move || {
            for _ in 0..200 {
                clear_store.clear();
                std::thread::sleep(Duration::from_micros(10));
            }
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&store);
            readers.push(tokio::task::spawn_blocking(move || {
                for _ in 0..5_000 {
                    let _ = s.logs_by_trace_id(0);
                    let _ = s.len();
                }
            }));
        }

        let work = async {
            clearer.await.unwrap();
            for r in readers {
                r.await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("deadlock — clear/logs_by_trace_id ABBA regression");
    }
}
