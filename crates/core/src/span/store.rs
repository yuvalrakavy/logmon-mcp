use crate::engine::seq_counter::SeqCounter;
use crate::filter::matcher::matches_span;
use crate::filter::parser::ParsedFilter;
use crate::span::types::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

pub struct SpanStoreStats {
    pub total_stored: usize,
    pub total_traces: usize,
    pub avg_duration_ms: f64,
}

pub struct SpanStore {
    inner: RwLock<SpanStoreInner>,
    seq_counter: Arc<SeqCounter>,
    /// The lowest seq this ring can still speak for: spans below it were held
    /// and are gone. `0` means nothing has ever left.
    ///
    /// **Not `oldest_seq()`.** One `SeqCounter` feeds this ring and the log
    /// store, so an unfilled span ring's oldest seq is simply the first span it
    /// ever received — every seq below it belonged to a log, or to nothing.
    /// Reading that as an eviction boundary claims spans were lost on every
    /// domain that logged before it traced.
    lost_below: std::sync::atomic::AtomicU64,
}

struct SpanStoreInner {
    /// The held spans in ASCENDING SEQ ORDER, distinct — by construction: `insert` takes its
    /// seq from the shared counter UNDER this ring's write lock and pushes it in the same
    /// critical section, and `from_records` asserts its input. So a seq is found by binary
    /// search and `front()`/`back()` are the lowest and highest (the log store's invariant,
    /// for the same reasons).
    buffer: VecDeque<SpanEntry>,
    /// Each trace's held seqs, ascending.
    trace_index: HashMap<u128, Vec<u64>>,
    capacity: usize,
}

impl SpanStoreInner {
    /// The index of the span with this seq, if held.
    fn position(&self, seq: u64) -> Option<usize> {
        self.buffer.binary_search_by_key(&seq, |s| s.seq).ok()
    }
}

impl SpanStore {
    pub fn new(capacity: usize, seq_counter: Arc<SeqCounter>) -> Self {
        Self {
            // Lazy allocation (§6): start empty and reserve the full ring on the
            // first insert (see `insert`), so an idle span store holds ~0 buffer.
            inner: RwLock::new(SpanStoreInner {
                buffer: VecDeque::new(),
                trace_index: HashMap::new(),
                capacity,
            }),
            seq_counter,
            lost_below: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The lowest seq this ring can still speak for — see [`Self::lost_below`].
    pub fn lost_below(&self) -> u64 {
        self.lost_below.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Build a ring already holding `spans` — how a case is loaded.
    ///
    /// **A constructor rather than `insert`, because `insert` overwrites the
    /// seq** (`span.seq = self.seq_counter.next()`), and a loaded case's spans
    /// must keep the numbers the case document's cross-references point at.
    /// `lost_below` is the window's lower bound for the same reason as the log
    /// store's: a case speaks for its window and nothing beneath it.
    ///
    /// `spans` must be in ascending, distinct seq order — asserted, not trusted: `load_case`
    /// validates first (`cases::load`), so a violation is a bug, and it must not become a
    /// ring that binary-searches wrong.
    pub fn from_records(
        capacity: usize,
        seq_counter: Arc<SeqCounter>,
        spans: Vec<SpanEntry>,
        lost_below: u64,
    ) -> Self {
        assert!(
            spans.windows(2).all(|w| w[0].seq < w[1].seq),
            "SpanStore::from_records: spans must be in ascending, distinct seq order"
        );
        let mut buffer = VecDeque::with_capacity(capacity.max(spans.len()));
        let mut trace_index: HashMap<u128, Vec<u64>> = HashMap::new();
        for s in spans {
            trace_index.entry(s.trace_id).or_default().push(s.seq);
            buffer.push_back(s);
        }
        let cap = capacity.max(buffer.len());
        Self {
            inner: RwLock::new(SpanStoreInner {
                buffer,
                trace_index,
                capacity: cap,
            }),
            seq_counter,
            lost_below: std::sync::atomic::AtomicU64::new(lost_below),
        }
    }

    pub fn insert(&self, mut span: SpanEntry) -> u64 {
        let mut inner = self.inner.write().unwrap();
        // The seq is taken UNDER the write lock, in the same critical section as the push, so
        // the ring is seq-ordered by construction: two inserts can no longer take seqs in one
        // order and push in the other.
        span.seq = self.seq_counter.next();
        let seq = span.seq;

        // Lazy allocation (§6): reserve the full ring ONCE, on the first insert.
        if inner.buffer.capacity() == 0 {
            let cap = inner.capacity;
            inner.buffer.reserve_exact(cap);
        }

        if inner.buffer.len() >= inner.capacity {
            if let Some(evicted) = inner.buffer.pop_front() {
                // The one place a span actually leaves, recorded under the same
                // write guard that removes it. `fetch_max`: eviction takes the lowest seq, so
                // this never lowers the floor, and could not even if that broke.
                self.lost_below.fetch_max(
                    evicted.seq.saturating_add(1),
                    std::sync::atomic::Ordering::Relaxed,
                );
                if let Some(seqs) = inner.trace_index.get_mut(&evicted.trace_id) {
                    seqs.retain(|&s| s != evicted.seq);
                    if seqs.is_empty() {
                        inner.trace_index.remove(&evicted.trace_id);
                    }
                }
            }
        }

        inner
            .trace_index
            .entry(span.trace_id)
            .or_default()
            .push(span.seq);
        inner.buffer.push_back(span);
        seq
    }

    /// The trace's spans, by start time. See [`trace_spans`].
    pub fn get_trace(&self, trace_id: u128) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        trace_spans(&inner, trace_id).into_iter().cloned().collect()
    }

    pub fn slow_spans(
        &self,
        min_duration_ms: f64,
        count: usize,
        filter: Option<&ParsedFilter>,
    ) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        let mut matching: Vec<SpanEntry> = inner
            .buffer
            .iter()
            .filter(|s| s.duration_ms >= min_duration_ms)
            .filter(|s| filter.is_none_or(|f| matches_span(f, s)))
            .cloned()
            .collect();
        matching.sort_by(|a, b| {
            b.duration_ms
                .partial_cmp(&a.duration_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matching.truncate(count);
        matching
    }

    /// Visit every stored span matching `filter`, in insertion order, without
    /// cloning any of them. Returns how many were visited.
    ///
    /// The ad-hoc profile path needs to fold thousands of spans into a
    /// throwaway collector; materialising them into a `Vec<SpanEntry>` first
    /// would deep-clone an attribute map and an event vector per span, under
    /// the read lock, for data thrown away immediately after.
    pub fn for_each_matching<F>(&self, filter: Option<&ParsedFilter>, mut f: F) -> usize
    where
        F: FnMut(&SpanEntry),
    {
        let inner = self.inner.read().unwrap();
        let mut n = 0;
        for s in inner
            .buffer
            .iter()
            .filter(|s| filter.is_none_or(|p| matches_span(p, s)))
        {
            f(s);
            n += 1;
        }
        n
    }

    /// Per-name duration aggregates over the **full** matching population.
    ///
    /// `slow_spans` exists to answer "show me the slowest N"; aggregating its
    /// output answers a different question badly. Filtering by a duration floor
    /// and then truncating to N produces a set that is biased twice over, and
    /// an average taken across it is an average of the tail — reported without
    /// any hint that it is. This aggregates everything that matches the filter
    /// and leaves the floor to the caller as a display decision.
    ///
    /// **Folded inside the read guard on purpose.** Dropping the truncation
    /// would otherwise grow `slow_spans`'s clone set to the whole buffer, and
    /// each `SpanEntry` carries an attribute map and an event vector — paid
    /// while `insert` is waiting for the write lock. Only the durations leave
    /// the guard.
    pub fn duration_by_name(&self, filter: Option<&ParsedFilter>) -> HashMap<String, Vec<f64>> {
        let inner = self.inner.read().unwrap();
        let mut out: HashMap<String, Vec<f64>> = HashMap::new();
        for s in inner
            .buffer
            .iter()
            .filter(|s| filter.is_none_or(|f| matches_span(f, s)))
        {
            match out.get_mut(&s.name) {
                Some(v) => v.push(s.duration_ms),
                None => {
                    out.insert(s.name.clone(), vec![s.duration_ms]);
                }
            }
        }
        out
    }

    pub fn recent_traces<F>(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        linked_log_count: F,
    ) -> Vec<TraceSummary>
    where
        F: Fn(u128) -> u32,
    {
        let inner = self.inner.read().unwrap();
        let mut trace_max_seq: HashMap<u128, u64> = HashMap::new();
        for span in inner.buffer.iter() {
            if filter.is_none_or(|f| matches_span(f, span)) {
                let entry = trace_max_seq.entry(span.trace_id).or_insert(0);
                if span.seq > *entry {
                    *entry = span.seq;
                }
            }
        }

        let mut traces: Vec<(u128, u64)> = trace_max_seq.into_iter().collect();
        // Descending by max seq (most recent trace first).
        traces.sort_by_key(|&(_, max_seq)| std::cmp::Reverse(max_seq));
        traces.truncate(count);

        traces
            .iter()
            .map(|&(trace_id, _)| {
                // Non-empty: the trace is here because at least one of its spans is held.
                // In start-time order, as `get_trace` returns them, so a trace with no root
                // starts at its EARLIEST span and agrees with `build_trace_summary`.
                let spans = trace_spans(&inner, trace_id);
                let root = spans.iter().find(|s| s.parent_span_id.is_none());
                TraceSummary {
                    trace_id,
                    root_span_name: root.map_or("[no root]".to_string(), |r| r.name.clone()),
                    service_name: root.map_or("unknown".to_string(), |r| r.service_name.clone()),
                    start_time: root.map_or(spans[0].start_time, |r| r.start_time),
                    total_duration_ms: root.map_or(0.0, |r| r.duration_ms),
                    span_count: spans.len() as u32,
                    has_errors: spans
                        .iter()
                        .any(|s| matches!(s.status, SpanStatus::Error(_))),
                    linked_log_count: linked_log_count(trace_id),
                }
            })
            .collect()
    }

    pub fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        // Seq-ordered ring: the slice around the anchor is its seq neighbourhood.
        let pos = inner.position(seq);
        match pos {
            Some(idx) => {
                let start = idx.saturating_sub(before);
                let end = (idx + after + 1).min(inner.buffer.len());
                inner.buffer.range(start..end).cloned().collect()
            }
            None => vec![],
        }
    }

    pub fn stats(&self) -> SpanStoreStats {
        let inner = self.inner.read().unwrap();
        let total = inner.buffer.len();
        let traces = inner.trace_index.len();
        let avg = if total > 0 {
            inner.buffer.iter().map(|s| s.duration_ms).sum::<f64>() / total as f64
        } else {
            0.0
        };
        SpanStoreStats {
            total_stored: total,
            total_traces: traces,
            avg_duration_ms: avg,
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().buffer.len()
    }

    /// Dispose all buffered spans, returning how many were removed. The shared
    /// seq counter is NOT reset — seq stays monotonic — so bookmarks/cursors
    /// keep resolving. Mirrors `LogPipeline::clear_logs`; used by `domains.clear`.
    pub fn clear(&self) -> usize {
        let mut inner = self.inner.write().unwrap();
        let n = inner.buffer.len();
        // A clear loses spans exactly as an eviction does.
        if let Some(newest) = inner.buffer.back().map(|s| s.seq) {
            self.lost_below.fetch_max(
                newest.saturating_add(1),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        inner.buffer.clear();
        inner.trace_index.clear();
        n
    }

    /// Seq of the newest span currently buffered, or `None` if empty.
    /// Powers B2's `buffer_newest_seq` field on `traces.recent`.
    pub fn newest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().buffer.back().map(|s| s.seq)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn oldest_timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.inner
            .read()
            .unwrap()
            .buffer
            .front()
            .map(|s| s.start_time)
    }

    /// Seq of the oldest span currently in the buffer, or `None` if empty.
    /// Used by the bookmark sweep — see `crate::store::bookmarks::should_evict`.
    pub fn oldest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().buffer.front().map(|s| s.seq)
    }
}

/// One trace's held spans, by start time (ties in seq order — the sort is stable). Reads
/// only the trace's own spans, each found by binary search, so the cost is the trace's size
/// times log of the ring's, not the ring's. Shared by `get_trace` and `recent_traces`, which
/// used to walk the whole ring once per trace.
fn trace_spans(inner: &SpanStoreInner, trace_id: u128) -> Vec<&SpanEntry> {
    let Some(seqs) = inner.trace_index.get(&trace_id) else {
        return Vec::new();
    };
    let mut spans: Vec<&SpanEntry> = seqs
        .iter()
        .filter_map(|&seq| inner.buffer.get(inner.position(seq)?))
        .collect();
    spans.sort_by_key(|s| s.start_time);
    spans
}

#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::engine::seq_counter::SeqCounter;
    use std::sync::Arc;

    #[test]
    fn empty_span_store_returns_none() {
        let store = SpanStore::new(10, Arc::new(SeqCounter::new()));
        assert!(store.oldest_timestamp().is_none());
    }
}

#[cfg(test)]
mod lazy_alloc_tests {
    use super::*;
    use crate::engine::seq_counter::SeqCounter;
    use crate::span::types::{SpanKind, SpanStatus};
    use std::sync::Arc;

    fn span() -> SpanEntry {
        SpanEntry {
            seq: 0,
            trace_id: 1,
            span_id: 1,
            parent_span_id: None,
            start_time: chrono::Utc::now(),
            end_time: chrono::Utc::now(),
            duration_ms: 0.0,
            name: "s".into(),
            kind: SpanKind::Internal,
            service_name: "svc".into(),
            status: SpanStatus::Unset,
            attributes: std::collections::HashMap::new(),
            events: vec![],
        }
    }

    #[test]
    fn buffer_is_unallocated_until_first_insert() {
        let store = SpanStore::new(10_000, Arc::new(SeqCounter::new()));
        assert_eq!(
            store.inner.read().unwrap().buffer.capacity(),
            0,
            "a freshly-created span store reserves no buffer"
        );

        store.insert(span());
        let cap = store.inner.read().unwrap().buffer.capacity();
        assert!(cap >= 10_000, "first insert reserves the full ring: {cap}");

        store.insert(span());
        assert_eq!(
            store.inner.read().unwrap().buffer.capacity(),
            cap,
            "subsequent inserts do not re-allocate"
        );
    }
}

#[cfg(test)]
mod trace_lookup_tests {
    //! The span ring is in seq order by construction, and `get_trace` reads a trace's spans
    //! by binary search instead of walking the ring. The oracle for the lookups is the
    //! DEFINITION, computed without the index: the ring's spans whose `trace_id` is the
    //! trace's, in seq order, sorted by start time.

    use super::*;
    use crate::engine::seq_counter::SeqCounter;
    use crate::span::types::{SpanKind, SpanStatus};
    use std::sync::Arc;

    fn span(seq: u64, trace_id: u128, start_offset_ms: i64) -> SpanEntry {
        let base = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid time");
        let start_time = base + chrono::Duration::milliseconds(start_offset_ms);
        SpanEntry {
            seq,
            trace_id,
            span_id: 1,
            parent_span_id: None,
            start_time,
            end_time: start_time,
            duration_ms: 0.0,
            name: "s".into(),
            kind: SpanKind::Internal,
            service_name: "svc".into(),
            status: SpanStatus::Unset,
            attributes: std::collections::HashMap::new(),
            events: vec![],
        }
    }

    fn seqs(spans: Vec<SpanEntry>) -> Vec<u64> {
        spans.into_iter().map(|s| s.seq).collect()
    }

    fn by_definition(store: &SpanStore, trace_id: u128) -> Vec<u64> {
        let inner = store.inner.read().unwrap();
        let mut spans: Vec<&SpanEntry> = inner
            .buffer
            .iter()
            .filter(|s| s.trace_id == trace_id)
            .collect();
        spans.sort_by_key(|s| s.start_time);
        spans.into_iter().map(|s| s.seq).collect()
    }

    /// The ring's invariant: strictly ascending seqs, and each trace's list ascending and
    /// naming exactly that trace's held spans.
    fn assert_seq_ordered(store: &SpanStore) {
        let inner = store.inner.read().unwrap();
        let ring: Vec<u64> = inner.buffer.iter().map(|s| s.seq).collect();
        assert!(
            ring.windows(2).all(|w| w[0] < w[1]),
            "ring ascending: {ring:?}"
        );
        for (tid, list) in &inner.trace_index {
            let held: Vec<u64> = inner
                .buffer
                .iter()
                .filter(|s| s.trace_id == *tid)
                .map(|s| s.seq)
                .collect();
            assert_eq!(
                list, &held,
                "trace {tid}'s list is its held spans, ascending"
            );
        }
    }

    /// A trace reads by start time, ties in seq order (103 and 105 share a start), before
    /// and after evictions from a store loaded from a case.
    #[test]
    fn a_trace_reads_by_start_time_before_and_after_evictions() {
        let store = SpanStore::from_records(
            6,
            Arc::new(SeqCounter::new()),
            vec![
                span(103, 1, 0),
                span(104, 1, 3),
                span(105, 1, 0),
                span(109, 2, 2),
            ],
            0,
        );
        assert_eq!(seqs(store.get_trace(1)), vec![103, 105, 104]);
        assert_seq_ordered(&store);

        // A fresh counter starts below the loaded seqs; start it above them, as a loaded
        // case's sealed domain does.
        let store = SpanStore::from_records(
            6,
            Arc::new(SeqCounter::new_with_initial(200)),
            vec![
                span(103, 1, 0),
                span(104, 1, 3),
                span(105, 1, 0),
                span(109, 2, 2),
            ],
            0,
        );
        for k in 0..4 {
            store.insert(span(0, 1, 10 + k));
        }
        assert!(
            store.lost_below() > 0,
            "the inserts evicted from a 6-span ring"
        );
        assert_eq!(seqs(store.get_trace(1)), by_definition(&store, 1));
        assert_eq!(seqs(store.get_trace(2)), by_definition(&store, 2));
        assert_seq_ordered(&store);
    }

    /// The constructor owns the invariant: spans out of seq order are a bug, not a ring.
    #[test]
    #[should_panic(expected = "ascending, distinct seq order")]
    fn from_records_refuses_spans_out_of_seq_order() {
        let _ = SpanStore::from_records(
            6,
            Arc::new(SeqCounter::new()),
            vec![span(105, 1, 0), span(103, 1, 0)],
            0,
        );
    }

    /// Concurrent inserts still leave the ring in seq order: `insert` takes its seq under the
    /// write lock, in the critical section that pushes it.
    #[test]
    fn concurrent_inserts_leave_the_ring_in_seq_order() {
        let store = Arc::new(SpanStore::new(100_000, Arc::new(SeqCounter::new())));
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for k in 0..5_000 {
                        store.insert(span(0, t, k));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(store.len(), 20_000);
        assert_seq_ordered(&store);
    }

    /// `recent_traces` reads each trace by position too, in start-time order — so a trace
    /// with no root starts at its EARLIEST span, as `build_trace_summary` (which reads
    /// `get_trace`) has it, not at whichever span the ring happens to hold first.
    #[test]
    fn a_rootless_trace_starts_at_its_earliest_span_in_recent_traces() {
        let store = SpanStore::new(16, Arc::new(SeqCounter::new()));
        for offset in [5, 2, 9] {
            let mut child = span(0, 7, offset);
            child.parent_span_id = Some(1);
            store.insert(child);
        }
        store.insert(span(0, 8, 1)); // another trace, with a root, inserted last

        let summaries = store.recent_traces(10, None, |_| 0);
        let rootless = summaries
            .iter()
            .find(|t| t.trace_id == 7)
            .expect("trace 7 is listed");
        assert_eq!(rootless.span_count, 3);
        assert_eq!(
            rootless.start_time,
            span(0, 7, 2).start_time,
            "a rootless trace starts at its earliest span"
        );
        assert_eq!(
            summaries.iter().map(|t| t.trace_id).collect::<Vec<_>>(),
            vec![8, 7],
            "most recent trace first"
        );
    }

    /// xorshift64: deterministic, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Differential: a seeded mix of inserts across three traces with start times out of
    /// insertion order, evictions from a small ring and the odd clear — checked against the
    /// definition and the position invariant after every step.
    #[test]
    fn the_lookup_matches_the_definition_under_evictions_and_clears() {
        let store = SpanStore::new(16, Arc::new(SeqCounter::new()));
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (mut clears, mut nonempty, mut evictions) = (0, 0, 0);
        for _ in 0..5_000 {
            let r = rng.next();
            if (r >> 8).is_multiple_of(400) {
                store.clear();
                clears += 1;
            } else {
                let trace = (r % 3) as u128 + 1;
                if store.len() == 16 {
                    evictions += 1;
                }
                store.insert(span(0, trace, ((r >> 16) % 5) as i64));
            }
            for t in 1..=3u128 {
                let got = seqs(store.get_trace(t));
                assert_eq!(got, by_definition(&store, t), "trace {t}");
                if !got.is_empty() {
                    nonempty += 1;
                }
            }
            assert_seq_ordered(&store);
            let ring: Vec<u64> = store
                .inner
                .read()
                .unwrap()
                .buffer
                .iter()
                .map(|s| s.seq)
                .collect();
            if ring.len() >= 5 {
                let i = ring.len() / 2;
                let around: Vec<u64> = store
                    .context_by_seq(ring[i], 2, 2)
                    .iter()
                    .map(|s| s.seq)
                    .collect();
                assert_eq!(around, ring[i - 2..i + 3], "context around {}", ring[i]);
            }
        }
        // Vacuity guards: the run must have exercised what it claims to.
        assert!(clears > 2, "clears exercised: {clears}");
        assert!(evictions > 1_000, "evictions exercised: {evictions}");
        assert!(nonempty > 1_000, "lookups that returned spans: {nonempty}");
    }
}
