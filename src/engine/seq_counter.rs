use std::sync::atomic::{AtomicU64, Ordering};

/// Shared monotonic sequence counter for logs and spans.
pub struct SeqCounter {
    counter: AtomicU64,
}

impl Default for SeqCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqCounter {
    pub fn new() -> Self {
        Self { counter: AtomicU64::new(0) }
    }

    pub fn new_with_initial(initial: u64) -> Self {
        Self { counter: AtomicU64::new(initial) }
    }

    /// Returns the next sequence number (1-based). Matches existing LogPipeline::assign_seq() behavior.
    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}
