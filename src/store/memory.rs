use crate::gelf::message::LogEntry;
use crate::filter::parser::ParsedFilter;
use crate::filter::matcher::matches_entry;
use crate::store::traits::{LogStore, StoreStats};
use chrono::{DateTime, Utc};
use std::collections::{VecDeque, HashSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct InMemoryStore {
    entries: RwLock<VecDeque<LogEntry>>,
    seq_set: RwLock<HashSet<u64>>,
    max_capacity: usize,
    total_stored: AtomicU64,
    total_received: AtomicU64,
    malformed_count: AtomicU64,
}

impl InMemoryStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(VecDeque::with_capacity(capacity)),
            seq_set: RwLock::new(HashSet::new()),
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
        let mut entries = self.entries.write().unwrap();
        let mut seq_set = self.seq_set.write().unwrap();

        if entries.len() >= self.max_capacity {
            if let Some(evicted) = entries.pop_front() {
                seq_set.remove(&evicted.seq);
            }
        }

        entries.push_back(entry);
        seq_set.insert(seq);
    }

    fn recent(&self, count: usize, filter: Option<&ParsedFilter>) -> Vec<LogEntry> {
        let entries = self.entries.read().unwrap();
        let mut result = Vec::new();

        for entry in entries.iter().rev() {
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

        result
    }

    fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry> {
        let entries = self.entries.read().unwrap();

        // Find index of the entry with the given seq
        let idx = match entries.iter().position(|e| e.seq == seq) {
            Some(i) => i,
            None => return Vec::new(),
        };

        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(entries.len());

        entries.range(start..end).cloned().collect()
    }

    fn context_by_time(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry> {
        let entries = self.entries.read().unwrap();
        let window_ns = window.as_nanos() as i64;

        entries.iter()
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
        self.seq_set.read().unwrap().contains(&seq)
    }

    fn clear(&self) {
        self.entries.write().unwrap().clear();
        self.seq_set.write().unwrap().clear();
    }

    fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    fn stats(&self) -> StoreStats {
        StoreStats {
            total_received: self.total_received.load(Ordering::Relaxed),
            total_stored: self.total_stored.load(Ordering::Relaxed),
            malformed_count: self.malformed_count.load(Ordering::Relaxed),
        }
    }
}
