//! Value interning with a cardinality cap — spec §3.3, §3.5.
//!
//! Span names and group-key values become `u32` ids so the sample tier can
//! hold 4 bytes instead of a `String`. Every interner is capped: an unbounded
//! table is how a key like `request.id` turns a bounded collector into a leak.

use std::collections::HashMap;

/// Reserved: everything past the cardinality cap folds here. Reported so a
/// capped axis is never mistaken for a complete one.
pub const OVERFLOW_ID: u32 = 0;
pub const OVERFLOW_LABEL: &str = "__overflow__";

/// Reserved: the span did not carry this group key at all. Under a broad
/// filter that is most spans, and silently dropping them would make the group
/// breakdown disagree with the collector total.
pub const ABSENT_ID: u32 = 1;
pub const ABSENT_LABEL: &str = "__absent__";

const FIRST_REAL_ID: u32 = 2;

/// Distinct values per group key before overflow (§3.3).
pub const DEFAULT_GROUP_VALUE_CAP: usize = 1024;
/// Distinct span names carrying their own stats and sketch (§3.5).
pub const DEFAULT_NAME_CAP: usize = 256;

#[derive(Debug, Clone)]
pub struct Interner {
    to_id: HashMap<String, u32>,
    to_value: Vec<String>,
    cap: usize,
    capped: bool,
}

impl Interner {
    pub fn new(cap: usize) -> Self {
        Self {
            to_id: HashMap::new(),
            to_value: Vec::new(),
            cap,
            capped: false,
        }
    }

    /// Intern a value, or fold it into `__overflow__` once the cap is reached.
    ///
    /// Note which values land in the overflow bucket is **arrival order**, so
    /// two collectors' `__overflow__` rows aggregate different member sets —
    /// which is why §5.6 suppresses that row in a diff rather than comparing
    /// it.
    pub fn intern(&mut self, value: &str) -> u32 {
        if let Some(id) = self.to_id.get(value) {
            return *id;
        }
        if self.to_value.len() >= self.cap {
            self.capped = true;
            return OVERFLOW_ID;
        }
        let id = FIRST_REAL_ID + self.to_value.len() as u32;
        self.to_value.push(value.to_string());
        self.to_id.insert(value.to_string(), id);
        id
    }

    pub fn resolve(&self, id: u32) -> &str {
        match id {
            OVERFLOW_ID => OVERFLOW_LABEL,
            ABSENT_ID => ABSENT_LABEL,
            _ => self
                .to_value
                .get((id - FIRST_REAL_ID) as usize)
                .map(String::as_str)
                .unwrap_or(OVERFLOW_LABEL),
        }
    }

    /// Whether any value was folded into `__overflow__`.
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    pub fn len(&self) -> usize {
        self.to_value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_value.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable_and_round_trips() {
        let mut i = Interner::new(10);
        let a = i.intern("store.reconcile");
        let b = i.intern("schema.resolve_path");
        assert_eq!(
            i.intern("store.reconcile"),
            a,
            "the same value keeps its id"
        );
        assert_ne!(a, b);
        assert_eq!(i.resolve(a), "store.reconcile");
        assert_eq!(i.resolve(b), "schema.resolve_path");
        assert!(!i.is_capped());
    }

    #[test]
    fn reserved_ids_are_never_handed_out() {
        let mut i = Interner::new(10);
        for n in ["a", "b", "c"] {
            let id = i.intern(n);
            assert_ne!(
                id, OVERFLOW_ID,
                "a real value must not collide with overflow"
            );
            assert_ne!(id, ABSENT_ID, "nor with absent");
        }
        assert_eq!(i.resolve(OVERFLOW_ID), OVERFLOW_LABEL);
        assert_eq!(i.resolve(ABSENT_ID), ABSENT_LABEL);
    }

    #[test]
    fn values_past_the_cap_fold_into_overflow_and_the_cap_is_reported() {
        let mut i = Interner::new(3);
        let ids: Vec<u32> = (0..3).map(|n| i.intern(&format!("v{n}"))).collect();
        assert!(!i.is_capped(), "exactly at the cap is not over it");
        assert!(ids.iter().all(|id| *id >= FIRST_REAL_ID));

        let over = i.intern("v3");
        assert_eq!(over, OVERFLOW_ID);
        assert!(i.is_capped(), "the cap must be reported, never silent");
        assert_eq!(i.resolve(over), OVERFLOW_LABEL);

        // Already-interned values keep working after the cap is hit.
        assert_eq!(i.intern("v0"), ids[0]);
        // And memory stops growing.
        assert_eq!(i.len(), 3);
        for n in 4..100 {
            assert_eq!(i.intern(&format!("v{n}")), OVERFLOW_ID);
        }
        assert_eq!(i.len(), 3, "a capped interner is bounded");
    }

    #[test]
    fn an_unknown_id_resolves_to_overflow_rather_than_panicking() {
        // Ids can arrive from a persisted snapshot written under a different
        // interner. Resolving must degrade, not abort.
        let i = Interner::new(4);
        assert_eq!(i.resolve(9_999), OVERFLOW_LABEL);
    }
}
