//! The per-domain **epoch log** — a time-extended record of the storage
//! *policy*, so a range read can say what it could not have seen.
//!
//! # Why a record of the policy, and not of the entries
//!
//! Storage is conditional: when any session bound to a domain holds a filter,
//! only matching entries are kept. So "nothing appeared before the error" is
//! ambiguous between *absence of cause* and *absence of recording*, and only
//! the second is a bug in the reader's reasoning.
//!
//! Stored-entry provenance cannot resolve it. [`LogSource`] explains why a
//! **kept** entry was kept; **no stored entry can testify about an absent
//! one**. Neither can a point-in-time read of the live filter set: an anonymous
//! session is removed on disconnect and takes its filters with it, so a capture
//! taken minutes after the run sees no filters and would assert that none were
//! narrowing a window that recorded a tenth of it.
//!
//! What resolves it is a record of the policy *over time*: each entry here is
//! the first seq decided under a policy, plus the policy itself. A range is
//! claimable as complete only when it lies wholly inside one unfiltered epoch.
//!
//! # Where the boundary is stamped, and why it is exact
//!
//! The seq is assigned at the top of `process_entry_for_domain` and the storage
//! decision is taken at the bottom, so a flip recorded on the RPC thread
//! (`current_seq() + 1`, beside the mutation) would be approximate by the depth
//! of whatever is in flight — and `complete` would be claimed over entries the
//! other policy actually judged.
//!
//! So nothing stamps from the outside. [`EpochLog::observe`] is called by the
//! log processor with the very [`FilterPolicy`] that just decided the entry in
//! hand, read under the same lock guards that made the decision. The boundary
//! is therefore not "close to" the flip; it *is* the first entry decided
//! differently.
//!
//! # Why observing only the entries the policy actually decided is sound
//!
//! `observe` is called from one place: the branch where filters decide storage.
//! Entries stored by a trigger or inside a post-trigger window skip it, so a
//! boundary can land *later* than the flip — never earlier.
//!
//! That direction is safe, and provably so in both cases. A late boundary
//! attributes some entries to the previous epoch; every one of those entries
//! took the unconditional path, so every one of them **is in the store**. If
//! the previous epoch was filtered, the range over-reports `filtered` — missing
//! information, not wrong information. If it was unfiltered, `complete` is
//! claimed over entries that are all present, which is what `complete` asserts.
//!
//! [`LogSource`]: crate::gelf::message::LogSource

use logmon_broker_protocol::EvidenceVerdict;
use std::collections::VecDeque;
use std::sync::RwLock;

/// Epochs retained per domain. Flips are rare — a filter added, edited,
/// removed, a session bound elsewhere or disposed — so this is generous for any
/// real session and still bounds a client that edits a filter in a loop.
///
/// Overflow drops the OLDEST, and [`EpochCoverage::covered`] then reports the
/// loss rather than letting the surviving oldest epoch speak for a range that
/// predates it: dropping a record must not manufacture a `complete`.
const EPOCH_CAPACITY: usize = 1024;

/// The filter policy that decided whether one entry was stored: the union of
/// every filter string held by a session bound to the domain.
///
/// §4.2 describes an epoch as `(seq_at_flip, filtered: bool, filter_strings)`.
/// The bool is not stored, because it is exactly `!filters.is_empty()` and two
/// fields that must agree are two fields that can disagree.
///
/// The strings, not just the boolean, are what the policy is compared on.
/// `filters.edit` replaces a condition in place and never moves the boolean: a
/// domain filtered by `l>=ERROR` over 0–100 and edited to `service:auth` for
/// 100–200 would otherwise be reported as *"filtered by [service:auth] over
/// 0–200"* — wrong information rather than missing information.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterPolicy {
    filters: Vec<String>,
}

impl FilterPolicy {
    /// Sorted and deduped, so the value is a function of the *policy* rather
    /// than of `HashMap` iteration order — without which inserting an unrelated
    /// session could rehash the session map and fabricate an epoch boundary out
    /// of a reordering.
    ///
    /// Deduping is not merely tidiness: two sessions holding the same filter
    /// string narrow the store exactly as one does, so removing the duplicate
    /// is not a flip and must not read as one.
    pub fn new(mut filters: Vec<String>) -> Self {
        filters.sort_unstable();
        filters.dedup();
        Self { filters }
    }

    /// True when the domain was narrowing storage — §4.2's `filtered`.
    pub fn is_filtered(&self) -> bool {
        !self.filters.is_empty()
    }

    pub fn filters(&self) -> &[String] {
        &self.filters
    }
}

/// One epoch: the policy, and the first seq decided under it.
#[derive(Debug, Clone)]
pub struct Epoch {
    pub seq_at_flip: u64,
    pub policy: FilterPolicy,
}

/// One stretch of a queried range over which storage was narrowed, and by what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarrowedSpan {
    /// Clamped to the queried range, so the caller is told which of *its* seqs
    /// were affected rather than where the epoch happened to begin.
    pub from_seq: u64,
    pub to_seq: u64,
    pub filters: Vec<String>,
}

/// What the epoch log can say about one seq range.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpochCoverage {
    /// False when part of the range predates the oldest epoch retained — a
    /// range from before this daemon incarnation, or one whose epochs were
    /// dropped to keep the log bounded. The log is silent about that part, and
    /// silence is not evidence of an unfiltered store.
    pub covered: bool,
    /// Every stretch of the range over which storage was narrowed. Empty when
    /// the whole range stored everything.
    pub narrowed: Vec<NarrowedSpan>,
}

/// The per-domain epoch log. Held by [`LogPipeline`], because it is indexed by
/// that pipeline's seq axis and records the decisions taken over it.
///
/// [`LogPipeline`]: crate::engine::pipeline::LogPipeline
#[derive(Debug)]
pub struct EpochLog {
    epochs: RwLock<VecDeque<Epoch>>,
    /// The seq this incarnation started from. The first epoch is stamped here
    /// rather than at `0`, so a range carried over from a previous daemon run —
    /// whose seq counter this one resumed — falls below the log and reports
    /// `cannot_verify` instead of inheriting this incarnation's policy.
    ///
    /// That is §4.2's "a restart is an epoch boundary on every domain",
    /// obtained from the fact that the log is per-process rather than from any
    /// restart detection.
    origin_seq: u64,
}

impl EpochLog {
    pub fn new(origin_seq: u64) -> Self {
        Self {
            epochs: RwLock::new(VecDeque::new()),
            origin_seq,
        }
    }

    /// Record that `seq` was decided under `policy`, opening a new epoch if the
    /// policy differs from the one in force.
    ///
    /// Called from the log processor's storage-decision branch with the policy
    /// that decision was made from — see the module docs for why that, and not
    /// a stamp beside the mutation, is what makes the boundary exact.
    pub fn observe(&self, seq: u64, policy: &FilterPolicy) {
        {
            // Unchanged is the overwhelmingly common case and it is per-entry,
            // so it must not take a write lock.
            let log = self.epochs.read().expect("epoch log lock poisoned");
            if log.back().is_some_and(|e| e.policy == *policy) {
                return;
            }
        }
        let mut log = self.epochs.write().expect("epoch log lock poisoned");
        let seq_at_flip = match log.back() {
            Some(last) if last.policy == *policy => return,
            // Never below the previous boundary: `coverage` reads the log as
            // ordered, and a caller processing entries concurrently could
            // otherwise interleave two seqs out of order.
            Some(last) => seq.max(last.seq_at_flip),
            // The first observation opens the epoch at the incarnation's
            // origin, not at the seq observed. Any entry between the two either
            // did not exist or took the unconditional path — see the module
            // docs — so no entry in that stretch can be missing, whichever
            // policy it is attributed to.
            None => self.origin_seq,
        };
        log.push_back(Epoch {
            seq_at_flip,
            policy: policy.clone(),
        });
        while log.len() > EPOCH_CAPACITY {
            log.pop_front();
        }
    }

    /// What the log can say about the inclusive range `from..=to`.
    pub fn coverage(&self, from: u64, to: u64) -> EpochCoverage {
        let log = self.epochs.read().expect("epoch log lock poisoned");
        let Some(first) = log.front() else {
            // Nothing has been decided in this domain yet, so nothing is known
            // about any range in it.
            return EpochCoverage::default();
        };
        let mut coverage = EpochCoverage {
            covered: from >= first.seq_at_flip,
            narrowed: Vec::new(),
        };
        for (i, epoch) in log.iter().enumerate() {
            // An epoch runs until the next one begins, and the last runs
            // forever — the policy in force now is the policy for every seq
            // yet to be assigned.
            let epoch_end = log
                .get(i + 1)
                .map_or(u64::MAX, |next| next.seq_at_flip.saturating_sub(1));
            if epoch.seq_at_flip > to || epoch_end < from {
                continue;
            }
            if epoch.policy.is_filtered() {
                coverage.narrowed.push(NarrowedSpan {
                    from_seq: epoch.seq_at_flip.max(from),
                    to_seq: epoch_end.min(to),
                    filters: epoch.policy.filters().to_vec(),
                });
            }
        }
        coverage
    }

    /// The seq this incarnation started from — the lowest seq this log can say
    /// anything about at all.
    pub fn origin_seq(&self) -> u64 {
        self.origin_seq
    }

    /// The epochs currently retained, oldest first. Test/diagnostic surface.
    pub fn epochs(&self) -> Vec<Epoch> {
        self.epochs
            .read()
            .expect("epoch log lock poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// §4.2's four verdicts for one range read.
///
/// `coverage` is `None` when the range itself could not be established — an
/// empty store has no window for a verdict to be about, and every clause of
/// `complete` is vacuously satisfied by one.
///
/// **Order of precedence is the strongest reason the range is not complete.**
/// `cannot_verify` outranks the rest because it says the daemon cannot vouch
/// for the range at all. `filtered` outranks `evicted` because filtered entries
/// were never recorded and nothing measures them, whereas eviction is both
/// detected and quantified — `evicted_before_window` names the size of the gap.
/// The facts behind both survive on the wire regardless of which name the
/// verdict takes.
pub fn evidence_verdict(
    coverage: Option<&EpochCoverage>,
    evicted: bool,
    capped: bool,
) -> EvidenceVerdict {
    let Some(coverage) = coverage else {
        return EvidenceVerdict::CannotVerify;
    };
    // A capped read stopped short of what matched, so what it did not return is
    // unknown rather than absent — and "asked N, got N" cannot tell the two
    // apart, which is why `capped` is carried explicitly.
    if !coverage.covered || capped {
        return EvidenceVerdict::CannotVerify;
    }
    if !coverage.narrowed.is_empty() {
        return EvidenceVerdict::Filtered;
    }
    if evicted {
        return EvidenceVerdict::Evicted;
    }
    EvidenceVerdict::Complete
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(filters: &[&str]) -> FilterPolicy {
        FilterPolicy::new(filters.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn policy_is_order_and_duplicate_insensitive() {
        // Two sessions in either iteration order are one policy — otherwise a
        // HashMap rehash would fabricate a flip.
        assert_eq!(policy(&["b", "a"]), policy(&["a", "b"]));
        // And two sessions holding the same filter narrow the store exactly as
        // one does, so losing the duplicate is not a flip.
        assert_eq!(policy(&["a", "a"]), policy(&["a"]));
        assert!(!policy(&[]).is_filtered());
        assert!(policy(&["a"]).is_filtered());
    }

    #[test]
    fn first_observation_opens_at_the_origin_not_at_the_seq() {
        let log = EpochLog::new(0);
        log.observe(7, &policy(&[]));
        assert_eq!(log.epochs()[0].seq_at_flip, 0);

        // A restored domain resumes its predecessor's counter, so its first
        // epoch starts where this incarnation did — not at 0, which would let
        // it speak for the previous run's seqs.
        let restored = EpochLog::new(500);
        restored.observe(507, &policy(&[]));
        assert_eq!(restored.epochs()[0].seq_at_flip, 500);
        assert!(!restored.coverage(499, 600).covered);
        assert!(restored.coverage(500, 600).covered);
    }

    #[test]
    fn an_unchanged_policy_does_not_open_an_epoch() {
        let log = EpochLog::new(0);
        for seq in 1..=50 {
            log.observe(seq, &policy(&["l>=ERROR"]));
        }
        assert_eq!(log.epochs().len(), 1, "50 entries, one policy, one epoch");
    }

    #[test]
    fn editing_a_filter_string_opens_an_epoch_though_the_boolean_never_moves() {
        // The `filters.edit` case: filtered before and filtered after, so a
        // marker keyed on the boolean would report the NEW filter string over
        // the OLD range — wrong information rather than missing information.
        let log = EpochLog::new(0);
        log.observe(1, &policy(&["l>=ERROR"]));
        log.observe(101, &policy(&["service:auth"]));

        let c = log.coverage(1, 200);
        assert_eq!(c.narrowed.len(), 2, "{c:?}");
        assert_eq!(c.narrowed[0].filters, vec!["l>=ERROR".to_string()]);
        assert_eq!((c.narrowed[0].from_seq, c.narrowed[0].to_seq), (1, 100));
        assert_eq!(c.narrowed[1].filters, vec!["service:auth".to_string()]);
        assert_eq!((c.narrowed[1].from_seq, c.narrowed[1].to_seq), (101, 200));
    }

    #[test]
    fn narrowed_spans_are_clamped_to_the_queried_range() {
        let log = EpochLog::new(0);
        log.observe(1, &policy(&[]));
        log.observe(100, &policy(&["l>=ERROR"]));
        log.observe(200, &policy(&[]));

        // The caller is told which of ITS seqs were narrowed, not where the
        // epoch began.
        let c = log.coverage(150, 175);
        assert_eq!(c.narrowed.len(), 1);
        assert_eq!((c.narrowed[0].from_seq, c.narrowed[0].to_seq), (150, 175));

        // A range wholly inside the unfiltered tail is clean.
        assert!(log.coverage(200, 300).narrowed.is_empty());
        // Including seqs not yet assigned: the last epoch runs forever.
        assert!(log.coverage(10_000, u64::MAX).narrowed.is_empty());
    }

    #[test]
    fn an_empty_log_covers_nothing() {
        let log = EpochLog::new(0);
        let c = log.coverage(0, u64::MAX);
        assert!(!c.covered, "no decision has been taken in this domain yet");
        assert_eq!(
            evidence_verdict(Some(&c), false, false),
            EvidenceVerdict::CannotVerify
        );
    }

    #[test]
    fn dropping_the_oldest_epoch_is_reported_rather_than_papered_over() {
        let log = EpochLog::new(0);
        // Alternate so every observation is a genuine flip.
        for seq in 1..=(EPOCH_CAPACITY as u64 + 10) {
            let p = if seq % 2 == 0 {
                policy(&["a"])
            } else {
                policy(&["b"])
            };
            log.observe(seq, &p);
        }
        let epochs = log.epochs();
        assert_eq!(epochs.len(), EPOCH_CAPACITY);
        let oldest = epochs[0].seq_at_flip;
        assert!(oldest > 0, "the origin epoch was dropped: {oldest}");
        assert!(
            !log.coverage(0, oldest + 1).covered,
            "a range below the retained log must not inherit the oldest survivor's policy"
        );
        assert!(log.coverage(oldest, oldest + 1).covered);
    }

    #[test]
    fn the_verdict_set_is_closed_and_defaults_to_the_safe_end() {
        for (v, s) in [
            (EvidenceVerdict::Complete, "complete"),
            (EvidenceVerdict::Evicted, "evicted"),
            (EvidenceVerdict::Filtered, "filtered"),
            (EvidenceVerdict::CannotVerify, "cannot_verify"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), serde_json::json!(s));
        }
        // §9.4 cut `partial`. A closed set has to refuse it from the wire, not
        // merely stop producing it.
        assert!(serde_json::from_str::<EvidenceVerdict>("\"partial\"").is_err());
        // A reply from a broker too old to send the field must not read as a
        // clean bill of health.
        assert_eq!(EvidenceVerdict::default(), EvidenceVerdict::CannotVerify);
    }

    #[test]
    fn the_verdict_orders_by_the_strongest_reason_it_is_not_complete() {
        let clean = EpochCoverage {
            covered: true,
            narrowed: Vec::new(),
        };
        let narrowed = EpochCoverage {
            covered: true,
            narrowed: vec![NarrowedSpan {
                from_seq: 1,
                to_seq: 2,
                filters: vec!["l>=ERROR".into()],
            }],
        };
        let uncovered = EpochCoverage {
            covered: false,
            narrowed: Vec::new(),
        };

        assert_eq!(
            evidence_verdict(Some(&clean), false, false),
            EvidenceVerdict::Complete
        );
        assert_eq!(
            evidence_verdict(Some(&clean), true, false),
            EvidenceVerdict::Evicted
        );
        assert_eq!(
            evidence_verdict(Some(&clean), false, true),
            EvidenceVerdict::CannotVerify
        );
        assert_eq!(
            evidence_verdict(Some(&narrowed), true, false),
            EvidenceVerdict::Filtered
        );
        assert_eq!(
            evidence_verdict(Some(&uncovered), false, false),
            EvidenceVerdict::CannotVerify
        );
        // An empty store: no window, so no verdict about one. Every clause of
        // `complete` is otherwise vacuously satisfied by it.
        assert_eq!(
            evidence_verdict(None, false, false),
            EvidenceVerdict::CannotVerify
        );
    }
}
