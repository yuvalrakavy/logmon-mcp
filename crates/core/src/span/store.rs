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
    buffer: VecDeque<SpanEntry>,
    trace_index: HashMap<u128, Vec<u64>>,
    capacity: usize,
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

    pub fn insert(&self, mut span: SpanEntry) -> u64 {
        span.seq = self.seq_counter.next();
        let seq = span.seq;
        let mut inner = self.inner.write().unwrap();

        // Lazy allocation (§6): reserve the full ring ONCE, on the first insert.
        if inner.buffer.capacity() == 0 {
            let cap = inner.capacity;
            inner.buffer.reserve_exact(cap);
        }

        if inner.buffer.len() >= inner.capacity {
            if let Some(evicted) = inner.buffer.pop_front() {
                // The one place a span actually leaves, recorded under the same
                // write guard that removes it.
                self.lost_below.store(
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

    pub fn get_trace(&self, trace_id: u128) -> Vec<SpanEntry> {
        let inner = self.inner.read().unwrap();
        let seqs = match inner.trace_index.get(&trace_id) {
            Some(s) => s,
            None => return vec![],
        };
        let mut spans: Vec<SpanEntry> = inner
            .buffer
            .iter()
            .filter(|s| seqs.contains(&s.seq))
            .cloned()
            .collect();
        spans.sort_by_key(|s| s.start_time);
        spans
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
                let spans: Vec<&SpanEntry> = inner
                    .buffer
                    .iter()
                    .filter(|s| s.trace_id == trace_id)
                    .collect();
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
        let pos = inner.buffer.iter().position(|s| s.seq == seq);
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
            self.lost_below.store(
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
