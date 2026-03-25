use crate::gelf::message::LogEntry;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct PreTriggerBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: AtomicUsize,
}

impl PreTriggerBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            capacity: AtomicUsize::new(capacity),
        }
    }

    pub fn append(&self, entry: LogEntry) {
        let cap = self.capacity.load(Ordering::Relaxed);
        if cap == 0 {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        entries.push_back(entry);
        while entries.len() > cap {
            entries.pop_front();
        }
    }

    /// Drain the last `pre_window` entries from the buffer.
    /// The flushed entries are removed; remaining entries stay.
    /// Returns entries in chronological order (oldest first).
    pub fn flush(&self, pre_window: usize) -> Vec<LogEntry> {
        let mut entries = self.entries.lock().unwrap();
        let len = entries.len();
        if len == 0 || pre_window == 0 {
            return Vec::new();
        }
        let n = pre_window.min(len);
        // split_off at (len - n) gives us the last n entries
        let tail = entries.split_off(len - n);
        tail.into_iter().collect()
    }

    pub fn resize(&self, new_capacity: usize) {
        self.capacity.store(new_capacity, Ordering::Relaxed);
        let mut entries = self.entries.lock().unwrap();
        while entries.len() > new_capacity {
            entries.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
