use crate::gelf::message::LogEntry;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    capacity: AtomicUsize,
}

struct Inner {
    /// `(arrival index, entry)`, oldest arrival first.
    entries: VecDeque<(u64, LogEntry)>,
    /// Entries ever appended (the next arrival index).
    arrivals: u64,
}

impl Inner {
    /// Drop every entry outside the last `cap` arrivals.
    fn expire(&mut self, cap: usize) {
        let oldest_kept = self.arrivals.saturating_sub(cap as u64);
        while self.entries.front().is_some_and(|(a, _)| *a < oldest_kept) {
            self.entries.pop_front();
        }
    }
}

impl PreTriggerBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
                arrivals: 0,
            }),
            capacity: AtomicUsize::new(capacity),
        }
    }

    pub fn append(&self, entry: LogEntry) {
        let cap = self.capacity.load(Ordering::Relaxed);
        if cap == 0 {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let arrival = inner.arrivals;
        inner.arrivals += 1;
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
        tail.into_iter().map(|(_, e)| e).collect()
    }

    pub fn resize(&self, new_capacity: usize) {
        self.capacity.store(new_capacity, Ordering::Relaxed);
        self.inner.lock().unwrap().expire(new_capacity);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Return copies of entries matching the given trace_id.
    pub fn entries_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|(_, e)| e.trace_id == Some(trace_id))
            .map(|(_, e)| e.clone())
            .collect()
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
