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
}

struct SpanStoreInner {
    buffer: VecDeque<SpanEntry>,
    trace_index: HashMap<u128, Vec<u64>>,
    capacity: usize,
}

impl SpanStore {
    pub fn new(capacity: usize, seq_counter: Arc<SeqCounter>) -> Self {
        Self {
            inner: RwLock::new(SpanStoreInner {
                buffer: VecDeque::with_capacity(capacity),
                trace_index: HashMap::new(),
                capacity,
            }),
            seq_counter,
        }
    }

    pub fn insert(&self, mut span: SpanEntry) -> u64 {
        span.seq = self.seq_counter.next();
        let seq = span.seq;
        let mut inner = self.inner.write().unwrap();

        if inner.buffer.len() >= inner.capacity {
            if let Some(evicted) = inner.buffer.pop_front() {
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
        spans.sort_by(|a, b| a.start_time.cmp(&b.start_time));
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
        traces.sort_by(|a, b| b.1.cmp(&a.1));
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
