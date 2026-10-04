use crate::gelf::message::LogEntry;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// The last `capacity` ARRIVALS, minus whatever a trigger has already flushed.
///
/// Each entry carries its arrival index, and an entry leaves once it is older than the last
/// `capacity` arrivals — whether or not a flush has meanwhile drained newer ones. Counting by
/// length instead let a flush's hole keep older entries alive: one trigger with a small
/// pre-window draining the newest few every few records left a large pre-window's buffer
/// reaching far further back than its capacity, and a trigger flushing from there merged
/// records into the log ring that far back, moving every record stored since under the
/// ring's write lock (see the seq-ordered ring spec, §8). Bounded by arrivals, a flush only
/// ever reaches back `capacity` records.
pub struct PreTriggerBuffer {
    inner: Mutex<Inner>,
}

struct Inner {
    /// `(arrival index, entry)`, oldest arrival first.
    entries: VecDeque<(u64, LogEntry)>,
    /// Entries ever appended (the next arrival index).
    arrivals: u64,
    /// Under the same lock as the entries, not beside them: an `append` that read the
    /// capacity, lost a race to `resize(0)`, and then pushed would strand its entry — no
    /// later append runs `expire` at capacity 0, so it would outlive any number of arrivals
    /// and come back with the next trigger.
    capacity: usize,
    /// Each trace's buffered entries, by arrival index, ascending — so a trace's lookup costs
    /// its own size. It used to scan every entry under this lock on each traced firing, and a
    /// `pre_window` may now be as large as the log ring itself (gh #29). Kept in step with
    /// `entries` by every removal: `expire` takes the oldest arrivals (each its trace's
    /// oldest), `flush` the newest (each its trace's newest).
    by_trace: HashMap<u128, VecDeque<u64>>,
}

impl Inner {
    /// Drop every entry outside the last `cap` arrivals.
    fn expire(&mut self, cap: usize) {
        let oldest_kept = self.arrivals.saturating_sub(cap as u64);
        while self.entries.front().is_some_and(|(a, _)| *a < oldest_kept) {
            if let Some((_, e)) = self.entries.pop_front() {
                self.forget(e.trace_id, true);
            }
        }
    }

    /// Drop one entry of `trace_id` from the index: its oldest (`front`) or its newest.
    fn forget(&mut self, trace_id: Option<u128>, front: bool) {
        let Some(tid) = trace_id else { return };
        if let Some(arrivals) = self.by_trace.get_mut(&tid) {
            if front {
                arrivals.pop_front();
            } else {
                arrivals.pop_back();
            }
            if arrivals.is_empty() {
                self.by_trace.remove(&tid);
            }
        }
    }
}

impl PreTriggerBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                arrivals: 0,
                capacity,
                by_trace: HashMap::new(),
            }),
        }
    }

    pub fn append(&self, entry: LogEntry) {
        let mut inner = self.inner.lock().unwrap();
        let cap = inner.capacity;
        if cap == 0 {
            return;
        }
        let arrival = inner.arrivals;
        inner.arrivals += 1;
        if let Some(tid) = entry.trace_id {
            inner.by_trace.entry(tid).or_default().push_back(arrival);
        }
        inner.entries.push_back((arrival, entry));
        inner.expire(cap);
    }

    /// Drain the last `pre_window` entries from the buffer.
    /// The flushed entries are removed; remaining entries stay.
    /// Returns entries in chronological order (oldest first).
    pub fn flush(&self, pre_window: usize) -> Vec<LogEntry> {
        let mut inner = self.inner.lock().unwrap();
        let len = inner.entries.len();
        if len == 0 || pre_window == 0 {
            return Vec::new();
        }
        let n = pre_window.min(len);
        // split_off at (len - n) gives us the last n entries
        let tail = inner.entries.split_off(len - n);
        // The newest arrivals overall, so each is the newest of its own trace.
        for (_, e) in &tail {
            inner.forget(e.trace_id, false);
        }
        tail.into_iter().map(|(_, e)| e).collect()
    }

    pub fn resize(&self, new_capacity: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.capacity = new_capacity;
        inner.expire(new_capacity);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return copies of entries matching the given trace_id, oldest arrival first.
    ///
    /// Through the trace's own index, each entry found by binary search on its arrival index
    /// (`entries` is ascending by arrival): O(k log n) for a trace of k entries, not a scan of
    /// the whole buffer under the lock that every ingested record also takes.
    pub fn entries_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        let inner = self.inner.lock().unwrap();
        let Some(arrivals) = inner.by_trace.get(&trace_id) else {
            return Vec::new();
        };
        arrivals
            .iter()
            .filter_map(|a| {
                let i = inner.entries.binary_search_by_key(a, |(x, _)| *x).ok()?;
                inner.entries.get(i).map(|(_, e)| e.clone())
            })
            .collect()
    }

    /// Traces in the index and the arrivals it holds — for the tests that pin the index
    /// against `entries` (a stale index would hold arrivals `entries` no longer has).
    #[cfg(test)]
    fn index_sizes(&self) -> (usize, usize) {
        let inner = self.inner.lock().unwrap();
        (
            inner.by_trace.len(),
            inner.by_trace.values().map(VecDeque::len).sum(),
        )
    }
}

#[cfg(test)]
mod arrival_window_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    fn entry(seq: u64) -> LogEntry {
        let mut e = LogEntry::synthetic(Level::Info, "m");
        e.seq = seq;
        e
    }

    fn seqs(v: Vec<LogEntry>) -> Vec<u64> {
        v.into_iter().map(|e| e.seq).collect()
    }

    /// A flush's hole does not let older entries outlive the last `capacity` arrivals: with
    /// capacity 5, arrivals 1-5, a flush of the newest 2, then arrivals 6-8, the buffer holds
    /// only what arrived among the last 5 (4-8) minus the drained 4-5 — not 1-3 as well.
    #[test]
    fn a_flush_does_not_extend_the_window_past_capacity_arrivals() {
        let buf = PreTriggerBuffer::new(5);
        for s in 1..=5 {
            buf.append(entry(s));
        }
        assert_eq!(seqs(buf.flush(2)), vec![4, 5]);
        for s in 6..=8 {
            buf.append(entry(s));
        }
        assert_eq!(
            seqs(buf.flush(100)),
            vec![6, 7, 8],
            "1-3 arrived before the last 5 arrivals, so they are gone even though the \
             buffer holds fewer than 5"
        );
    }

    /// Shrinking the capacity expires by arrival too.
    #[test]
    fn a_resize_expires_by_arrival() {
        let buf = PreTriggerBuffer::new(10);
        for s in 1..=6 {
            buf.append(entry(s));
        }
        assert_eq!(seqs(buf.flush(1)), vec![6]);
        buf.resize(3);
        assert_eq!(
            seqs(buf.flush(100)),
            vec![4, 5],
            "the last 3 arrivals are 4-6, 6 drained"
        );
    }
}

/// The trace index answers exactly what a scan of the buffer would (gh #29), and holds nothing
/// the buffer does not — a stale arrival would not change an answer (each one is checked against
/// `entries`), only leak, so the index's size is pinned separately.
#[cfg(test)]
mod trace_index_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    fn entry(seq: u64, trace: Option<u128>) -> LogEntry {
        let mut e = LogEntry::synthetic(Level::Info, "m");
        e.seq = seq;
        e.trace_id = trace;
        e
    }

    /// The pre-index answer: every buffered entry of the trace, by a full scan.
    fn scan(buf: &PreTriggerBuffer, trace: u128) -> Vec<u64> {
        let inner = buf.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|(_, e)| e.trace_id == Some(trace))
            .map(|(_, e)| e.seq)
            .collect()
    }

    /// `(traces, traced entries)` as the buffer itself holds them.
    fn held(buf: &PreTriggerBuffer) -> (usize, usize) {
        let inner = buf.inner.lock().unwrap();
        let traced: Vec<u128> = inner
            .entries
            .iter()
            .filter_map(|(_, e)| e.trace_id)
            .collect();
        let distinct: std::collections::HashSet<u128> = traced.iter().copied().collect();
        (distinct.len(), traced.len())
    }

    #[test]
    fn the_index_matches_a_scan_through_appends_flushes_and_resizes() {
        let buf = PreTriggerBuffer::new(8);
        // xorshift: deterministic, so a failure reproduces.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut seq = 0u64;
        for step in 0..5_000 {
            match next() % 10 {
                // Mostly appends, a third of them untraced, over four traces.
                0..=6 => {
                    seq += 1;
                    let t = next() % 6;
                    buf.append(entry(seq, (t < 4).then_some(t as u128)));
                }
                7 | 8 => {
                    buf.flush((next() % 5) as usize);
                }
                _ => buf.resize((next() % 12) as usize),
            }
            for t in 0..4u128 {
                let got: Vec<u64> = buf.entries_by_trace_id(t).iter().map(|e| e.seq).collect();
                assert_eq!(got, scan(&buf, t), "trace {t} at step {step}");
            }
            assert_eq!(buf.index_sizes(), held(&buf), "index size at step {step}");
        }
    }

    /// A trace whose entries all left the buffer leaves no key behind.
    #[test]
    fn a_trace_whose_entries_left_has_no_index_entry() {
        let buf = PreTriggerBuffer::new(2);
        buf.append(entry(1, Some(7)));
        buf.append(entry(2, None));
        buf.append(entry(3, None));
        assert!(buf.entries_by_trace_id(7).is_empty());
        assert_eq!(buf.index_sizes(), (0, 0));
    }
}
