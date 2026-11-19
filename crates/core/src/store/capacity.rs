//! The capacity of a single record ring. The ring's write lock protects this
//! policy together with the eviction decision it governs.
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy)]
pub(crate) struct RingCapacity {
    pub initial: usize,
    pub current: usize,
    pub maximum: Option<usize>,
    pub protected_since: Option<DateTime<Utc>>,
}

impl RingCapacity {
    pub fn fixed(initial: usize) -> Self {
        Self {
            initial,
            current: initial,
            maximum: None,
            protected_since: None,
        }
    }

    pub fn configure(&mut self, initial: Option<usize>, maximum: Option<usize>) {
        if let Some(initial) = initial {
            self.initial = initial;
            self.current = initial;
        }
        // A ceiling-only edit preserves already-admitted evidence up to the new ceiling.
        self.current = self.current.min(maximum.unwrap_or(self.initial));
        self.maximum = maximum;
    }

    /// Called immediately before a record would leave. A full idle ring just rolls.
    pub fn grow_for(&mut self, evicted_at: DateTime<Utc>) -> bool {
        let Some(maximum) = self.maximum else {
            return false;
        };
        if self.current >= maximum || self.protected_since.is_none_or(|s| evicted_at < s) {
            return false;
        }
        self.current = self.current.saturating_mul(2).max(1).min(maximum);
        true
    }
}
