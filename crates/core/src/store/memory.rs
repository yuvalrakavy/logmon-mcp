use crate::filter::matcher::matches_entry;
use crate::filter::parser::ParsedFilter;
use crate::gelf::message::LogEntry;
use crate::store::traits::{LogStore, StoreStats};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
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
    /// The lowest seq this store can still speak for: records below it were
    /// held and are now gone. `0` means nothing has ever left.
    ///
    /// **This is not `oldest_seq()`, and confusing the two is a wrong answer,
    /// not a rounding error.** A ring that has never filled has an oldest seq
    /// equal to the first record it ever received — and since one `SeqCounter`
    /// feeds both this store and the span store, the seqs below that belonged
    /// to spans, or to nothing at all. Reading `oldest_seq` as an eviction
    /// boundary reports loss on any domain that logged before it traced, which
    /// is the ordinary shape rather than an edge case.
    lost_below: AtomicU64,
}

impl InMemoryStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            // Lazy allocation (§6): start empty and reserve the full ring on the
            // first append (see `append`), so an idle store holds ~0 buffer
            // memory — matters when many domains are created but stay idle.
            inner: RwLock::new(StoreInner {
                entries: VecDeque::new(),
                seq_set: HashSet::new(),
                trace_index: HashMap::new(),
            }),
            max_capacity: capacity,
            total_stored: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            malformed_count: AtomicU64::new(0),
            lost_below: AtomicU64::new(0),
        }
    }

    /// The lowest seq this store can still speak for — see [`Self::lost_below`].
    /// `0` when nothing has ever been dropped, which is the only value that
    /// licenses a completeness claim down to the start of the axis.
    pub fn lost_below(&self) -> u64 {
        self.lost_below.load(Ordering::Relaxed)
    }

    pub fn increment_malformed(&self) {
        self.malformed_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Like the `recent` query, but also returns how many buffered records were
    /// EXAMINED to produce the result (respecting the early-stop at `count`).
    /// Powers B2's `scanned` field: `matched=0, scanned>0` means "filter's
    /// fault, data is flowing", whereas `scanned=0` means an empty buffer.
    pub fn recent_with_scanned(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> (Vec<LogEntry>, usize) {
        let inner = self.inner.read().unwrap();
        let mut result = Vec::new();
        let mut scanned = 0usize;

        if oldest_first {
            // Cursor-driven path: walk forward (oldest → newest) and take the
            // first `count` filter-matching records. Pagination drains
            // monotonically across calls.
            for entry in inner.entries.iter() {
                scanned += 1;
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
                scanned += 1;
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

        (result, scanned)
    }

    /// Visit every stored record matching `filter`, oldest first, **without
    /// cloning any of them**, and report what the walk saw.
    ///
    /// `recent_with_scanned` cannot serve a whole-population read, for two
    /// independent reasons. It early-stops once `count` matches are collected
    /// (see the `break`s above), so a caller describing "the buffer" would
    /// silently be describing its newest slice — and would look correct on any
    /// fixture smaller than the default. And it clones every match, including a
    /// `HashMap` per record, for data a projection folds and discards.
    ///
    /// Mirrors `SpanStore::for_each_matching`, which exists for the same reason
    /// on the span side. Inherent rather than on `LogStore` because the pipeline
    /// holds a concrete store, exactly as `recent_with_scanned` does.
    pub fn for_each_matching<F>(&self, filter: Option<&ParsedFilter>, mut f: F) -> ScanCounts
    where
        F: FnMut(&LogEntry),
    {
        let inner = self.inner.read().unwrap();
        let mut counts = ScanCounts::default();
        for entry in inner.entries.iter() {
            counts.scanned += 1;
            if let Some(p) = filter {
                if !matches_entry(p, entry) {
                    continue;
                }
            }
            counts.matched += 1;
            f(entry);
        }
        counts
    }
}

/// What a full-buffer walk examined and what it kept.
///
/// Two named fields rather than a `(usize, usize)`: they are the same type and
/// differ only in meaning, so a tuple is one transposition away from reporting
/// a filter that matched everything as one that matched nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCounts {
    /// Records examined — the whole ring, never a `count`-limited prefix.
    pub scanned: usize,
    /// Records that passed the filter and reached the visitor.
    pub matched: usize,
}

impl LogStore for InMemoryStore {
    fn append(&self, entry: LogEntry) {
        self.total_received.fetch_add(1, Ordering::Relaxed);
        self.total_stored.fetch_add(1, Ordering::Relaxed);

        let seq = entry.seq;
        let trace_id = entry.trace_id;
        let mut inner = self.inner.write().unwrap();

        // Lazy allocation (§6): reserve the full ring capacity ONCE, on the first
        // append. `reserve_exact` on an empty deque allocates exactly the ring;
        // subsequent appends see a non-zero capacity and skip this. `pop_front`
        // never shrinks capacity, so the guard stays false thereafter.
        if inner.entries.capacity() == 0 {
            inner.entries.reserve_exact(self.max_capacity);
        }

        if inner.entries.len() >= self.max_capacity {
            if let Some(evicted) = inner.entries.pop_front() {
                inner.seq_set.remove(&evicted.seq);
                // The one place a record actually leaves. Recorded under the
                // same write guard that removes it, so the boundary and the
                // removal cannot be observed out of step.
                self.lost_below
                    .store(evicted.seq.saturating_add(1), Ordering::Relaxed);
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
        self.recent_with_scanned(count, filter, oldest_first).0
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

        inner
            .entries
            .iter()
            .filter(|e| {
                let diff = (e.timestamp - timestamp)
                    .num_nanoseconds()
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
        inner
            .entries
            .iter()
            .filter(|e| seq_set.contains(&e.seq))
            .cloned()
            .collect()
    }

    fn count_by_trace_id(&self, trace_id: u128) -> usize {
        let inner = self.inner.read().unwrap();
        inner
            .trace_index
            .get(&trace_id)
            .map_or(0, |seqs| seqs.len())
    }

    fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        // A clear loses records exactly as an eviction does, and a capture taken
        // afterwards must not read the emptied range as never-having-existed.
        if let Some(newest) = inner.entries.back().map(|e| e.seq) {
            self.lost_below
                .store(newest.saturating_add(1), Ordering::Relaxed);
        }
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
        self.inner
            .read()
            .unwrap()
            .entries
            .front()
            .map(|e| e.timestamp)
    }

    fn oldest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().entries.front().map(|e| e.seq)
    }

    fn newest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().entries.back().map(|e| e.seq)
    }
}

#[cfg(test)]
mod full_walk_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    /// F2 — the walk covers the WHOLE buffer, not a `count`-limited prefix.
    ///
    /// The property every population-describing read rests on. The obvious
    /// primitive next door (`recent_with_scanned`) early-stops at `count`, so a
    /// summary built on it silently describes the newest slice while claiming
    /// to describe the buffer — and looks correct on any fixture smaller than
    /// the default. The fixture here is deliberately larger than any plausible
    /// default so that a capped implementation cannot pass.
    ///
    /// Negative control: bound the loop in `for_each_matching` at 50 and this
    /// goes red while the exactness tests stay green.
    #[test]
    fn for_each_matching_visits_every_record_regardless_of_any_count() {
        let store = InMemoryStore::new(10_000);
        const N: usize = 2_000;
        for i in 0..N {
            store.append(LogEntry::synthetic(Level::Info, &format!("m{i}")));
        }

        let mut seen = 0usize;
        let counts = store.for_each_matching(None, |_| seen += 1);

        assert_eq!(counts.scanned, N, "every buffered record examined");
        assert_eq!(counts.matched, N, "no filter, so every record matched");
        assert_eq!(seen, N, "and every one reached the visitor");

        // The contrast that motivates this method existing at all.
        let (_, capped_scan) = store.recent_with_scanned(50, None, false);
        assert!(
            capped_scan < N,
            "recent_with_scanned early-stops ({capped_scan} of {N}); a summary \
             built on it would describe the newest slice as though it were the \
             buffer"
        );
    }

    /// `matched` and `scanned` are different numbers and must not be swapped —
    /// the reason `ScanCounts` has named fields rather than being a tuple.
    #[test]
    fn a_filter_narrows_matched_while_scanned_still_covers_everything() {
        use crate::filter::parser::parse_filter;
        let store = InMemoryStore::new(1_000);
        for i in 0..100 {
            let lvl = if i % 10 == 0 { Level::Error } else { Level::Info };
            store.append(LogEntry::synthetic(lvl, "m"));
        }
        let f = parse_filter("l>=ERROR").expect("filter parses");

        let mut visited = 0usize;
        let counts = store.for_each_matching(Some(&f), |_| visited += 1);

        assert_eq!(counts.scanned, 100, "the filter narrows matches, not the walk");
        assert_eq!(counts.matched, 10, "one in ten");
        assert_eq!(visited, 10, "the visitor sees matches only");
    }
}

#[cfg(test)]
mod lazy_alloc_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    #[test]
    fn buffer_is_unallocated_until_first_append() {
        let store = InMemoryStore::new(10_000);
        // Idle: no ring allocated (§6 — matters with many idle domains).
        assert_eq!(
            store.inner.read().unwrap().entries.capacity(),
            0,
            "a freshly-created store reserves no buffer"
        );

        store.append(LogEntry::synthetic(Level::Info, "first"));
        let cap = store.inner.read().unwrap().entries.capacity();
        assert!(cap >= 10_000, "first append reserves the full ring: {cap}");

        store.append(LogEntry::synthetic(Level::Info, "second"));
        assert_eq!(
            store.inner.read().unwrap().entries.capacity(),
            cap,
            "subsequent appends do not re-allocate"
        );
    }
}

#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry, LogSource};
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

    #[test]
    fn recent_with_scanned_reports_all_records_examined_when_filter_matches_none() {
        use crate::filter::parser::parse_filter;
        let store = InMemoryStore::new(10);
        for i in 1..=5 {
            store.append(entry(i)); // Level::Info
        }
        // No Info record matches l>=ERROR, so the query walks the whole buffer.
        // This is the exact 0-result case B2 must make self-diagnosing:
        // matched=0 but scanned>0 means "filter's fault", not "empty buffer".
        let filter = parse_filter("l>=ERROR").unwrap();
        let (matched, scanned) = store.recent_with_scanned(50, Some(&filter), false);
        assert_eq!(matched.len(), 0, "no Info record matches l>=ERROR");
        assert_eq!(scanned, 5, "all 5 buffered records were examined");
    }

    #[test]
    fn newest_seq_returns_back_entry_seq_not_front() {
        let store = InMemoryStore::new(10);
        assert!(store.newest_seq().is_none());
        store.append(entry(1));
        store.append(entry(2));
        store.append(entry(3));
        assert_eq!(store.newest_seq(), Some(3), "newest = back of the deque");
        assert_eq!(
            store.oldest_seq(),
            Some(1),
            "oldest = front (guards a mixup)"
        );
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
    use crate::gelf::message::{Level, LogEntry, LogSource};
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
