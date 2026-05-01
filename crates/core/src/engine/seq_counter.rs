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

    /// Returns the next sequence number. Always ≥ 1 — `seq = 0` is reserved
    /// as a sentinel for "before all records," used by cursor auto-create.
    /// See docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md.
    pub fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Returns the most-recently-assigned seq (the value `next()` last returned).
    /// `Relaxed` ordering is correct here: we're reading the counter, not
    /// synchronizing on log-data writes elsewhere.
    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cursor design relies on seq=0 being a never-assigned sentinel.
    /// See docs/superpowers/specs/2026-05-01-cursor-bookmarks-design.md
    /// §Storage `seq = 0 is reserved as a sentinel`.
    #[test]
    fn next_never_returns_zero_from_default() {
        let c = SeqCounter::new();
        assert_eq!(c.next(), 1);
    }

    #[test]
    fn next_never_returns_zero_from_initial_zero() {
        let c = SeqCounter::new_with_initial(0);
        assert_eq!(c.next(), 1);
    }

    #[test]
    fn next_returns_initial_plus_one() {
        let c = SeqCounter::new_with_initial(42);
        assert_eq!(c.next(), 43);
    }
}
