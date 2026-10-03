use crate::filter::matcher::matches_entry;
use crate::filter::parser::ParsedFilter;
use crate::gelf::message::LogEntry;
use crate::store::traits::{LogStore, StoreStats};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;

/// Inner state guarded by a single `RwLock`. Collapsing the three
/// previously-separate `entries`/`seq_set`/`trace_index` locks into
/// one structurally prevents lock-ordering bugs (the previous
/// configuration had `append` and `logs_by_trace_id` taking the
/// inner locks in opposite orders, which deadlocked under concurrent
/// load — see notes/2026-05-05-logmon-broker-rwlock-bug-findings.md).
/// Mirrors the single-lock pattern in `SpanStore`.
struct StoreInner {
    /// The held records in ASCENDING SEQ ORDER, distinct. An invariant this store
    /// establishes — `append`, `insert_sorted` and `from_records` are its only writers and each
    /// keeps it — and every reader relies on: a position is a position in seq, `front()` and
    /// `back()` are the lowest and highest, and a seq is found by binary search.
    ///
    /// It used to be the order records were STORED, which a trigger breaks: it stores the
    /// record that fired it, then the older records of its pre-window
    /// (`daemon/log_processor.rs`). Every reader that took a position for a seq was then
    /// wrong across that stretch — see the seq-ordered ring spec.
    entries: VecDeque<LogEntry>,
    /// Each trace's held seqs, ascending.
    trace_index: HashMap<u128, Vec<u64>>,
}

impl StoreInner {
    /// The index of the record with this seq, if held.
    fn position(&self, seq: u64) -> Option<usize> {
        self.entries.binary_search_by_key(&seq, |e| e.seq).ok()
    }

    /// Whether this seq is held. The common question — the seq just assigned, which is newer
    /// than everything held — is answered by `back()` alone.
    fn holds(&self, seq: u64) -> bool {
        match self.entries.back() {
            None => false,
            Some(newest) if seq > newest.seq => false,
            Some(newest) if seq == newest.seq => true,
            Some(_) => self.position(seq).is_some(),
        }
    }

    /// Forget `seq` in its trace's list.
    fn unindex(&mut self, trace_id: Option<u128>, seq: u64) {
        let Some(tid) = trace_id else { return };
        if let Some(seqs) = self.trace_index.get_mut(&tid) {
            if let Ok(i) = seqs.binary_search(&seq) {
                seqs.remove(i);
            }
            if seqs.is_empty() {
                self.trace_index.remove(&tid);
            }
        }
    }
}

/// `list` and `add`, both ascending and disjoint, merged into `list` in one pass.
fn merge_ascending(list: &mut Vec<u64>, add: &[u64]) {
    if add
        .first()
        .is_none_or(|first| list.last().is_none_or(|last| first > last))
    {
        list.extend_from_slice(add);
        return;
    }
    let old = std::mem::take(list);
    list.reserve(old.len() + add.len());
    let (mut a, mut b) = (old.into_iter().peekable(), add.iter().copied().peekable());
    loop {
        match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if x < y => list.push(a.next().unwrap()),
            (Some(_), Some(_)) => list.push(b.next().unwrap()),
            (Some(_), None) => list.push(a.next().unwrap()),
            (None, Some(_)) => list.push(b.next().unwrap()),
            (None, None) => break,
        }
    }
}

pub struct InMemoryStore {
    inner: RwLock<StoreInner>,
    max_capacity: usize,
    total_stored: AtomicU64,
    total_received: AtomicU64,
    malformed_count: AtomicU64,
    /// The lowest seq this store can still speak for: records below it were
    /// held and are now gone. `0` means nothing has ever left.
    ///
    /// **This is not `oldest_seq()`, and confusing the two is a wrong answer,
    /// not a rounding error.** A ring that has never filled has an oldest seq
    /// equal to the first record it ever received — and since one `SeqCounter`
    /// feeds both this store and the span store, the seqs below that belonged
    /// to spans, or to nothing at all. Reading `oldest_seq` as an eviction
    /// boundary reports loss on any domain that logged before it traced, which
    /// is the ordinary shape rather than an edge case.
    lost_below: AtomicU64,
    /// The lowest seq this store will still ADMIT, beyond `lost_below`: a clear raises it to
    /// one past every seq handed out, so nothing that arrived before the clear is stored after
    /// it. Kept apart from `lost_below` on purpose — that one is a LOSS claim every reader
    /// grades eviction from, and the seqs between the newest held log and the counter belong
    /// to spans or to logs a filter kept out, none of which this store ever held or lost.
    admit_from: AtomicU64,
}

impl InMemoryStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            // Lazy allocation (§6): start empty and reserve the full ring on the
            // first append (see `append`), so an idle store holds ~0 buffer
            // memory — matters when many domains are created but stay idle.
            inner: RwLock::new(StoreInner {
                entries: VecDeque::new(),
                trace_index: HashMap::new(),
            }),
            max_capacity: capacity,
            total_stored: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            malformed_count: AtomicU64::new(0),
            lost_below: AtomicU64::new(0),
            admit_from: AtomicU64::new(0),
        }
    }

    /// Build a store already holding `records` — how a case is loaded.
    ///
    /// **A constructor, not an insert path.** `append` would work: it takes each
    /// entry's own seq and never reassigns it. But it also increments
    /// `total_received`/`total_stored` as if the records had arrived, and leaves
    /// `lost_below` at `0` — which claims this store can speak for the whole seq
    /// axis down to the beginning of time. A loaded case can speak for its window
    /// and nothing below it, and that is the difference between a truthful
    /// postmortem domain and one that answers "nothing happened before this"
    /// when it means "I was not there".
    ///
    /// `lost_below` is the window's own lower bound: everything below it is
    /// unavailable here, whatever the reason. **`total_received`/`total_stored`
    /// stay at 0** — a case is a window cut out of a stream, and asserting it
    /// received exactly what it holds would claim a delivery record it does not
    /// have. Readers of those counters must treat a postmortem domain as
    /// unmeasured rather than as perfect.
    ///
    /// Records must be ascending and distinct — the order this store's every reader relies
    /// on. Asserted rather than trusted: `load_case` validates first (`cases::load`), so a
    /// violation here is a bug, and it must not become a ring that binary-searches wrong.
    pub fn from_records(capacity: usize, records: Vec<LogEntry>, lost_below: u64) -> Self {
        assert!(
            records.windows(2).all(|w| w[0].seq < w[1].seq),
            "InMemoryStore::from_records: records must be in ascending, distinct seq order"
        );
        let mut entries = VecDeque::with_capacity(capacity.max(records.len()));
        let mut trace_index: HashMap<u128, Vec<u64>> = HashMap::new();
        for e in records {
            if let Some(tid) = e.trace_id {
                trace_index.entry(tid).or_default().push(e.seq);
            }
            entries.push_back(e);
        }
        Self {
            max_capacity: capacity.max(entries.len()),
            inner: RwLock::new(StoreInner {
                entries,
                trace_index,
            }),
            total_stored: AtomicU64::new(0),
            total_received: AtomicU64::new(0),
            malformed_count: AtomicU64::new(0),
            lost_below: AtomicU64::new(lost_below),
            admit_from: AtomicU64::new(0),
        }
    }

    /// The lowest seq this store can still speak for — see [`Self::lost_below`].
    /// `0` when nothing has ever been dropped, which is the only value that
    /// licenses a completeness claim down to the start of the axis.
    pub fn lost_below(&self) -> u64 {
        self.lost_below.load(Ordering::Relaxed)
    }

    /// The admission floor — see the field. Not a loss claim.
    pub fn admit_from(&self) -> u64 {
        self.admit_from.load(Ordering::Relaxed)
    }

    pub fn increment_malformed(&self) {
        self.malformed_count.fetch_add(1, Ordering::Relaxed);
    }

    /// The held records with seqs in `[from, to]`, ascending, and the floor — read under ONE
    /// lock, so the two describe the same instant. A caller that read the records and then
    /// the floor separately could see a record it holds evicted in between, and report it
    /// gone while handing it out.
    pub fn range_with_floor(&self, from: u64, to: u64) -> (Vec<LogEntry>, u64) {
        let inner = self.inner.read().unwrap();
        let start = inner.entries.partition_point(|e| e.seq < from);
        let end = inner.entries.partition_point(|e| e.seq <= to);
        let records = inner
            .entries
            .range(start..end.max(start))
            .cloned()
            .collect();
        (records, self.lost_below.load(Ordering::Relaxed))
    }

    /// Empty the ring, and refuse from now on every record that arrived before this call —
    /// held or not. `newest_assigned` is the highest seq handed out so far; a record still in
    /// the pre-trigger buffer, or still on its way to the store, carries a seq at or below it.
    ///
    /// The plain `clear` raises the floor only past the newest record HELD, so a trigger
    /// firing after it could still flush, from the pre-trigger buffer, records that arrived
    /// before the clear but had been kept out by a filter. This raises the ADMISSION floor
    /// past the counter, and the loss floor (`lost_below`) only past what was held — raising
    /// the loss floor to the counter claimed logs lost over seqs that were spans, or nothing.
    pub fn clear_through(&self, newest_assigned: u64) {
        let mut inner = self.inner.write().unwrap();
        self.clear_locked(&mut inner);
        self.admit_from
            .fetch_max(newest_assigned.saturating_add(1), Ordering::Relaxed);
    }

    /// The lowest seq a write may store: below the loss floor (older than something evicted
    /// or cleared) or below the admission floor (arrived before a clear) is refused.
    fn refuse_below(&self) -> u64 {
        self.lost_below
            .load(Ordering::Relaxed)
            .max(self.admit_from.load(Ordering::Relaxed))
    }

    /// `clear`'s body, under the caller's write guard: a clear loses records exactly as an
    /// eviction does, so the floor rises past the newest held.
    fn clear_locked(&self, inner: &mut StoreInner) {
        if let Some(newest) = inner.entries.back().map(|e| e.seq) {
            self.lost_below
                .fetch_max(newest.saturating_add(1), Ordering::Relaxed);
        }
        inner.entries.clear();
        inner.trace_index.clear();
    }

    /// Store a batch of records that may be OLDER than records already held — a trigger's
    /// pre-window and the earlier records of its trace — keeping the ring in seq order.
    /// Returns how many held records had to move to make room (the tail above the batch's
    /// lowest seq), for the cost test and as a metric.
    ///
    /// The store owns its input, because the natural batch is not well-formed: the trigger
    /// drains the pre-window (the newest entries) and then reads its trace's OLDER entries, so
    /// it arrives unsorted, and a merge fed an unsorted batch would corrupt the ring with every
    /// binary search then answering wrongly and silently. So the batch is sorted here, and a
    /// seq repeated within it, already held, or below the floor is dropped.
    pub fn insert_sorted(&self, batch: Vec<LogEntry>) -> usize {
        let mut inner = self.inner.write().unwrap();
        self.merge_locked(&mut inner, batch)
    }

    /// Evict the lowest held record: the floor rises past it and its trace forgets it.
    fn evict_front(&self, inner: &mut StoreInner) {
        if let Some(evicted) = inner.entries.pop_front() {
            // The floor is raised with `fetch_max`: eviction takes the lowest seq, so this
            // never lowers it — and if that invariant ever broke, the floor would still not
            // move down and start claiming records it once called lost.
            self.lost_below
                .fetch_max(evicted.seq.saturating_add(1), Ordering::Relaxed);
            inner.unindex(evicted.trace_id, evicted.seq);
        }
    }

    /// Reserve the whole ring once, on the first write (§6 lazy allocation): an idle store
    /// holds no buffer, and `pop_front` never shrinks capacity.
    fn reserve_ring(&self, inner: &mut StoreInner) {
        if inner.entries.capacity() == 0 {
            inner.entries.reserve_exact(self.max_capacity);
        }
    }

    /// `insert_sorted`'s body, under the caller's write guard. See there.
    ///
    /// Counters: a seq already held, or repeated in the batch, is not a new receipt and is
    /// not counted at all; a new one counts as received, and as stored unless a floor (the
    /// loss floor or the admission floor) refuses it. So `total_received - total_stored` is
    /// exactly the refused offers — a record offered by two flushes is refused twice.
    fn merge_locked(&self, inner: &mut StoreInner, mut batch: Vec<LogEntry>) -> usize {
        batch.sort_by_key(|e| e.seq);
        batch.dedup_by_key(|e| e.seq);
        batch.retain(|e| !inner.holds(e.seq));
        self.total_received
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
        let floor = self.refuse_below();
        batch.retain(|e| e.seq >= floor);
        if batch.is_empty() {
            return 0;
        }
        self.reserve_ring(inner);

        // The held records above the batch's lowest seq come off, to be merged back.
        let split = inner.entries.partition_point(|e| e.seq < batch[0].seq);
        let tail: Vec<LogEntry> = inner.entries.drain(split..).collect();
        let moved = tail.len();

        // Each trace's list takes the batch's seqs for it in ONE merge, not a sorted insert
        // per record — the same O(batch x tail) shape this function exists to avoid.
        let mut by_trace: HashMap<u128, Vec<u64>> = HashMap::new();
        for e in &batch {
            if let Some(tid) = e.trace_id {
                by_trace.entry(tid).or_default().push(e.seq);
            }
        }
        for (tid, seqs) in by_trace {
            merge_ascending(inner.trace_index.entry(tid).or_default(), &seqs);
        }

        let stored = batch.len() as u64;
        let mut merged: Vec<LogEntry> = Vec::with_capacity(tail.len() + batch.len());
        let (mut t, mut b) = (tail.into_iter().peekable(), batch.into_iter().peekable());
        loop {
            let take_tail = match (t.peek(), b.peek()) {
                (Some(x), Some(y)) => x.seq < y.seq,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            merged.push(if take_tail { t.next() } else { b.next() }.unwrap());
        }

        // Capacity BEFORE pushing back: past the one-time `reserve_exact` the deque would
        // reallocate, and never give the memory back. The lowest records leave first — the
        // front of the ring, then the lowest of the merged run — each as an eviction.
        let mut excess = (inner.entries.len() + merged.len()).saturating_sub(self.max_capacity);
        while excess > 0 && !inner.entries.is_empty() {
            self.evict_front(inner);
            excess -= 1;
        }
        let mut merged = merged.into_iter();
        for dropped in merged.by_ref().take(excess) {
            self.lost_below
                .fetch_max(dropped.seq.saturating_add(1), Ordering::Relaxed);
            inner.unindex(dropped.trace_id, dropped.seq);
        }
        inner.entries.extend(merged);
        self.total_stored.fetch_add(stored, Ordering::Relaxed);
        moved
    }

    /// Like the `recent` query, but also returns how many buffered records were
    /// EXAMINED to produce the result (respecting the early-stop at `count`).
    /// Powers B2's `scanned` field: `matched=0, scanned>0` means "filter's
    /// fault, data is flowing", whereas `scanned=0` means an empty buffer.
    pub fn recent_with_scanned(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> (Vec<LogEntry>, usize) {
        let inner = self.inner.read().unwrap();
        let mut result = Vec::new();
        let mut scanned = 0usize;

        if oldest_first {
            // Cursor-driven path: walk forward (oldest → newest) and take the
            // first `count` filter-matching records. Pagination drains
            // monotonically across calls.
            for entry in inner.entries.iter() {
                scanned += 1;
                if let Some(f) = filter {
                    if !matches_entry(f, entry) {
                        continue;
                    }
                }
                result.push(entry.clone());
                if result.len() >= count {
                    break;
                }
            }
        } else {
            // Default path: newest-first, preserves prior behavior.
            for entry in inner.entries.iter().rev() {
                scanned += 1;
                if let Some(f) = filter {
                    if !matches_entry(f, entry) {
                        continue;
                    }
                }
                result.push(entry.clone());
                if result.len() >= count {
                    break;
                }
            }
        }

        (result, scanned)
    }

    /// Visit every stored record matching `filter`, oldest first, **without
    /// cloning any of them**, and report what the walk saw.
    ///
    /// `recent_with_scanned` cannot serve a whole-population read, for two
    /// independent reasons. It early-stops once `count` matches are collected
    /// (see the `break`s above), so a caller describing "the buffer" would
    /// silently be describing its newest slice — and would look correct on any
    /// fixture smaller than the default. And it clones every match, including a
    /// `HashMap` per record, for data a projection folds and discards.
    ///
    /// Mirrors `SpanStore::for_each_matching`, which exists for the same reason
    /// on the span side. Inherent rather than on `LogStore` because the pipeline
    /// holds a concrete store, exactly as `recent_with_scanned` does.
    pub fn for_each_matching<F>(&self, filter: Option<&ParsedFilter>, mut f: F) -> ScanCounts
    where
        F: FnMut(&LogEntry),
    {
        let inner = self.inner.read().unwrap();
        let mut counts = ScanCounts {
            held: inner.entries.len(),
            oldest_seq: inner.entries.front().map(|e| e.seq),
            newest_seq: inner.entries.back().map(|e| e.seq),
            lost_below: self.lost_below.load(Ordering::Relaxed),
            ..ScanCounts::default()
        };
        for entry in inner.entries.iter() {
            counts.scanned += 1;
            if let Some(p) = filter {
                if !matches_entry(p, entry) {
                    continue;
                }
            }
            counts.matched += 1;
            f(entry);
        }
        counts
    }
}

/// What a full-buffer walk examined and what it kept.
///
/// Two named fields rather than a `(usize, usize)`: they are the same type and
/// differ only in meaning, so a tuple is one transposition away from reporting
/// a filter that matched everything as one that matched nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanCounts {
    /// Records examined — the whole ring, never a `count`-limited prefix.
    pub scanned: usize,
    /// Records that passed the filter and reached the visitor.
    pub matched: usize,
    /// The ring as the walk saw it — its size, lowest and highest seq, and the loss floor —
    /// read under the walk's own lock, so a reply built from these never pairs the walk with
    /// a count, bound or floor read after an eviction.
    pub held: usize,
    pub oldest_seq: Option<u64>,
    pub newest_seq: Option<u64>,
    pub lost_below: u64,
}

impl LogStore for InMemoryStore {
    /// Store one record, keeping the ring in seq order (see `StoreInner::entries`). The
    /// normal case — the seq just assigned, newer than everything held — is a `push_back`; a
    /// record older than the newest held goes through the same merge `insert_sorted` uses.
    /// A seq already held is skipped and not counted (it is not a new receipt); a seq below
    /// either floor — older than something already evicted or cleared, or arrived before a
    /// `clear_through` — is counted as received and refused.
    fn append(&self, entry: LogEntry) {
        let mut inner = self.inner.write().unwrap();
        if !inner
            .entries
            .back()
            .is_none_or(|newest| entry.seq > newest.seq)
        {
            // Older than the newest held (or equal to it): the merge handles held seqs, the
            // floor and the counters for this case.
            self.merge_locked(&mut inner, vec![entry]);
            return;
        }
        self.total_received.fetch_add(1, Ordering::Relaxed);
        // The floors before the fast path's push: after a `clear` the ring is empty while the
        // floors are not.
        if entry.seq < self.refuse_below() {
            return;
        }
        if self.max_capacity == 0 {
            // A ring of no records stores and evicts at once, as the merge does with the run
            // it has no room for — so both write paths agree that it holds nothing (the push
            // below would otherwise hold one record past a capacity of zero).
            self.lost_below
                .fetch_max(entry.seq.saturating_add(1), Ordering::Relaxed);
            self.total_stored.fetch_add(1, Ordering::Relaxed);
            return;
        }

        self.reserve_ring(&mut inner);
        if inner.entries.len() >= self.max_capacity {
            // The one place a record leaves in the normal path, under the same write guard
            // that removes it, so the floor and the removal cannot be observed out of step.
            self.evict_front(&mut inner);
        }
        if let Some(tid) = entry.trace_id {
            // The newest seq, so the list stays ascending.
            inner.trace_index.entry(tid).or_default().push(entry.seq);
        }
        inner.entries.push_back(entry);
        self.total_stored.fetch_add(1, Ordering::Relaxed);
    }

    fn recent(
        &self,
        count: usize,
        filter: Option<&ParsedFilter>,
        oldest_first: bool,
    ) -> Vec<LogEntry> {
        self.recent_with_scanned(count, filter, oldest_first).0
    }

    fn context_by_seq(&self, seq: u64, before: usize, after: usize) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();

        // The ring is in seq order, so this is the anchor's place by seq as well as by
        // position, and the slice around it is its seq neighbourhood.
        let Some(idx) = inner.position(seq) else {
            return Vec::new();
        };

        let start = idx.saturating_sub(before);
        let end = (idx + after + 1).min(inner.entries.len());

        inner.entries.range(start..end).cloned().collect()
    }

    fn context_by_time(&self, timestamp: DateTime<Utc>, window: Duration) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();
        let window_ns = window.as_nanos() as i64;

        inner
            .entries
            .iter()
            .filter(|e| {
                let diff = (e.timestamp - timestamp)
                    .num_nanoseconds()
                    .map(|ns| ns.abs())
                    .unwrap_or(i64::MAX);
                diff <= window_ns
            })
            .cloned()
            .collect()
    }

    fn contains_seq(&self, seq: u64) -> bool {
        self.inner.read().unwrap().holds(seq)
    }

    /// A trace's records in seq order.
    ///
    /// Reads only the trace's own records, each found by binary search: O(k log n), not
    /// O(ring). It used to walk the whole ring under the read lock, which on a full
    /// 500,000-record buffer made every call cost the whole ring while ingestion waited for
    /// the write lock — and a client that polls this per trace (a test harness waiting for a
    /// marker record) issues many such calls a second.
    fn logs_by_trace_id(&self, trace_id: u128) -> Vec<LogEntry> {
        let inner = self.inner.read().unwrap();
        let Some(seqs) = inner.trace_index.get(&trace_id) else {
            return Vec::new();
        };
        seqs.iter()
            .filter_map(|&seq| inner.entries.get(inner.position(seq)?))
            .cloned()
            .collect()
    }

    fn count_by_trace_id(&self, trace_id: u128) -> usize {
        let inner = self.inner.read().unwrap();
        inner
            .trace_index
            .get(&trace_id)
            .map_or(0, |seqs| seqs.len())
    }

    /// A clear loses records exactly as an eviction does, and a capture taken afterwards must
    /// not read the emptied range as never-having-existed. The daemon clears through
    /// [`InMemoryStore::clear_through`], which also refuses records that arrived before the
    /// clear and were never held.
    fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        self.clear_locked(&mut inner);
    }

    fn len(&self) -> usize {
        self.inner.read().unwrap().entries.len()
    }

    /// Read under the ring's lock: every writer moves `total_received` and `total_stored` under
    /// the write guard, so the pair is never caught between a writer's two increments (which
    /// could show more stored than received). `malformed_count` is a lone counter, written
    /// without the lock.
    fn stats(&self) -> StoreStats {
        let _guard = self.inner.read().unwrap();
        StoreStats {
            total_received: self.total_received.load(Ordering::Relaxed),
            total_stored: self.total_stored.load(Ordering::Relaxed),
            malformed_count: self.malformed_count.load(Ordering::Relaxed),
        }
    }

    fn oldest_timestamp(&self) -> Option<DateTime<Utc>> {
        self.inner
            .read()
            .unwrap()
            .entries
            .front()
            .map(|e| e.timestamp)
    }

    fn oldest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().entries.front().map(|e| e.seq)
    }

    fn newest_seq(&self) -> Option<u64> {
        self.inner.read().unwrap().entries.back().map(|e| e.seq)
    }
}

#[cfg(test)]
mod full_walk_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    /// F2 — the walk covers the WHOLE buffer, not a `count`-limited prefix.
    ///
    /// The property every population-describing read rests on. The obvious
    /// primitive next door (`recent_with_scanned`) early-stops at `count`, so a
    /// summary built on it silently describes the newest slice while claiming
    /// to describe the buffer — and looks correct on any fixture smaller than
    /// the default. The fixture here is deliberately larger than any plausible
    /// default so that a capped implementation cannot pass.
    ///
    /// Negative control: bound the loop in `for_each_matching` at 50 and this
    /// goes red while the exactness tests stay green.
    #[test]
    fn for_each_matching_visits_every_record_regardless_of_any_count() {
        let store = InMemoryStore::new(10_000);
        const N: usize = 2_000;
        for i in 0..N {
            let mut e = LogEntry::synthetic(Level::Info, &format!("m{i}"));
            e.seq = i as u64 + 1;
            store.append(e);
        }

        let mut seen = 0usize;
        let counts = store.for_each_matching(None, |_| seen += 1);

        assert_eq!(counts.scanned, N, "every buffered record examined");
        assert_eq!(counts.matched, N, "no filter, so every record matched");
        assert_eq!(seen, N, "and every one reached the visitor");

        // The contrast that motivates this method existing at all.
        let (_, capped_scan) = store.recent_with_scanned(50, None, false);
        assert!(
            capped_scan < N,
            "recent_with_scanned early-stops ({capped_scan} of {N}); a summary \
             built on it would describe the newest slice as though it were the \
             buffer"
        );
    }

    /// `matched` and `scanned` are different numbers and must not be swapped —
    /// the reason `ScanCounts` has named fields rather than being a tuple.
    #[test]
    fn a_filter_narrows_matched_while_scanned_still_covers_everything() {
        use crate::filter::parser::parse_filter;
        let store = InMemoryStore::new(1_000);
        for i in 0..100 {
            let lvl = if i % 10 == 0 { Level::Error } else { Level::Info };
            let mut e = LogEntry::synthetic(lvl, "m");
            e.seq = i + 1;
            store.append(e);
        }
        let f = parse_filter("l>=ERROR").expect("filter parses");

        let mut visited = 0usize;
        let counts = store.for_each_matching(Some(&f), |_| visited += 1);

        assert_eq!(counts.scanned, 100, "the filter narrows matches, not the walk");
        assert_eq!(counts.matched, 10, "one in ten");
        assert_eq!(visited, 10, "the visitor sees matches only");
    }
}

#[cfg(test)]
mod lazy_alloc_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry};

    #[test]
    fn buffer_is_unallocated_until_first_append() {
        let store = InMemoryStore::new(10_000);
        // Idle: no ring allocated (§6 — matters with many idle domains).
        assert_eq!(
            store.inner.read().unwrap().entries.capacity(),
            0,
            "a freshly-created store reserves no buffer"
        );

        let mut first = LogEntry::synthetic(Level::Info, "first");
        first.seq = 1;
        store.append(first);
        let cap = store.inner.read().unwrap().entries.capacity();
        assert!(cap >= 10_000, "first append reserves the full ring: {cap}");

        let mut second = LogEntry::synthetic(Level::Info, "second");
        second.seq = 2;
        store.append(second);
        assert_eq!(
            store.inner.read().unwrap().entries.capacity(),
            cap,
            "subsequent appends do not re-allocate"
        );
    }
}

#[cfg(test)]
mod oldest_ts_tests {
    use super::*;
    use crate::gelf::message::{Level, LogEntry, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;

    fn entry(seq: u64) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "m".to_string(),
            full_message: None,
            host: "h".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    #[test]
    fn oldest_timestamp_empty_store_returns_none() {
        let store = InMemoryStore::new(10);
        assert!(store.oldest_timestamp().is_none());
    }

    #[test]
    fn oldest_timestamp_returns_front_entry_timestamp() {
        let store = InMemoryStore::new(10);
        let mut e1 = entry(1);
        e1.timestamp = Utc::now() - chrono::Duration::seconds(60);
        let mut e2 = entry(2);
        e2.timestamp = Utc::now();
        store.append(e1.clone());
        store.append(e2);
        assert_eq!(store.oldest_timestamp(), Some(e1.timestamp));
    }

    #[test]
    fn recent_with_scanned_reports_all_records_examined_when_filter_matches_none() {
        use crate::filter::parser::parse_filter;
        let store = InMemoryStore::new(10);
        for i in 1..=5 {
            store.append(entry(i)); // Level::Info
        }
        // No Info record matches l>=ERROR, so the query walks the whole buffer.
        // This is the exact 0-result case B2 must make self-diagnosing:
        // matched=0 but scanned>0 means "filter's fault", not "empty buffer".
        let filter = parse_filter("l>=ERROR").unwrap();
        let (matched, scanned) = store.recent_with_scanned(50, Some(&filter), false);
        assert_eq!(matched.len(), 0, "no Info record matches l>=ERROR");
        assert_eq!(scanned, 5, "all 5 buffered records were examined");
    }

    #[test]
    fn newest_seq_returns_back_entry_seq_not_front() {
        let store = InMemoryStore::new(10);
        assert!(store.newest_seq().is_none());
        store.append(entry(1));
        store.append(entry(2));
        store.append(entry(3));
        assert_eq!(store.newest_seq(), Some(3), "newest = back of the deque");
        assert_eq!(
            store.oldest_seq(),
            Some(1),
            "oldest = front (guards a mixup)"
        );
    }
}

#[cfg(test)]
mod concurrency_tests {
    //! Regression tests for the single-RwLock invariant.
    //!
    //! Before the 2026-05-05 fix, `InMemoryStore` held three separate
    //! `RwLock`s and `append` / `logs_by_trace_id` acquired them in
    //! opposite orders. Concurrent writer + reader → ABBA deadlock,
    //! observed in production after several hours of test load.
    //! These tests pin the contract: append + logs_by_trace_id +
    //! len from many concurrent OS threads must complete promptly,
    //! never wedging.

    use super::*;
    use crate::gelf::message::{Level, LogEntry, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn entry(seq: u64) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "m".to_string(),
            full_message: None,
            host: "h".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    /// Hammer `append` (writer) and `logs_by_trace_id` + `len` (readers)
    /// concurrently from real OS threads. Pre-fix, this wedges within
    /// a few thousand iterations on multi-core hardware. Post-fix, it
    /// completes in well under a second.
    ///
    /// Uses `spawn_blocking` to guarantee the workers actually run on
    /// separate OS threads (the `multi_thread` runtime alone may
    /// cooperatively schedule them on one worker, which masks the bug).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_and_logs_by_trace_id_do_not_deadlock() {
        // Small workload: enough contention to reliably trigger ABBA pre-fix
        // (which it did within seconds during diagnosis), but cheap enough
        // that the post-fix fair-RwLock contention completes well under
        // the watchdog. The bug is structural — small workload exposes it
        // just as well as a large one. A heavier workload pushes the test
        // past 20 s on contention alone, which would mask real regressions.
        let store = Arc::new(InMemoryStore::new(1_000));

        let writer_store = Arc::clone(&store);
        let writer = tokio::task::spawn_blocking(move || {
            for i in 0..5_000u64 {
                let mut e = entry(i);
                e.trace_id = Some((i % 16) as u128);
                writer_store.append(e);
            }
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&store);
            readers.push(tokio::task::spawn_blocking(move || {
                for _ in 0..1_000 {
                    let _ = s.logs_by_trace_id(0);
                    let _ = s.len();
                }
            }));
        }

        let work = async {
            writer.await.unwrap();
            for r in readers {
                r.await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("deadlock — append/logs_by_trace_id ABBA regression");
    }

    /// Same shape but with `clear` mixed in. `clear` previously took the
    /// three locks in the same order as `append`, but a future refactor
    /// that flipped its order would re-introduce ABBA against
    /// `logs_by_trace_id` — this guards it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clear_does_not_deadlock_with_readers() {
        let store = Arc::new(InMemoryStore::new(10_000));

        for i in 0..1_000u64 {
            let mut e = entry(i);
            e.trace_id = Some((i % 8) as u128);
            store.append(e);
        }

        let clear_store = Arc::clone(&store);
        let clearer = tokio::task::spawn_blocking(move || {
            for _ in 0..200 {
                clear_store.clear();
                std::thread::sleep(Duration::from_micros(10));
            }
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&store);
            readers.push(tokio::task::spawn_blocking(move || {
                for _ in 0..5_000 {
                    let _ = s.logs_by_trace_id(0);
                    let _ = s.len();
                }
            }));
        }

        let work = async {
            clearer.await.unwrap();
            for r in readers {
                r.await.unwrap();
            }
        };
        tokio::time::timeout(Duration::from_secs(20), work)
            .await
            .expect("deadlock — clear/logs_by_trace_id ABBA regression");
    }
}

#[cfg(test)]
mod seq_order_tests {
    //! The ring holds records in seq order (`StoreInner::entries`). The oracle for the
    //! differential below is a MODEL kept beside the store — the records that should be held,
    //! the exact floor, the counters — never a scan of the ring itself, which a merge that
    //! silently dropped records would still pass.

    use super::*;
    use crate::gelf::message::{Level, LogEntry, LogSource};
    use chrono::Utc;
    use std::collections::{BTreeMap, HashMap};

    fn entry(seq: u64, trace_id: Option<u128>) -> LogEntry {
        LogEntry {
            seq,
            timestamp: Utc::now(),
            level: Level::Info,
            message: "m".to_string(),
            full_message: None,
            host: "h".to_string(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    fn ring(store: &InMemoryStore) -> Vec<u64> {
        store
            .inner
            .read()
            .unwrap()
            .entries
            .iter()
            .map(|e| e.seq)
            .collect()
    }

    fn seqs(entries: Vec<LogEntry>) -> Vec<u64> {
        entries.into_iter().map(|e| e.seq).collect()
    }

    /// The trigger's batch: unsorted (the trace read returns entries OLDER than the drained
    /// window), with a repeat and a seq already held. It lands in seq order, and the records
    /// above its lowest seq are the only ones moved.
    #[test]
    fn an_unsorted_batch_with_repeats_lands_in_seq_order() {
        let store = InMemoryStore::new(16);
        for s in [1, 2, 10] {
            store.append(entry(s, None));
        }
        let moved = store.insert_sorted(vec![
            entry(8, Some(7)),
            entry(5, Some(7)),
            entry(5, Some(7)),
            entry(2, None),
        ]);
        assert_eq!(ring(&store), vec![1, 2, 5, 8, 10]);
        assert_eq!(
            moved, 1,
            "only the record above the batch's lowest seq (10) moved"
        );
        assert_eq!(
            seqs(store.logs_by_trace_id(7)),
            vec![5, 8],
            "the trace reads in seq order"
        );
        let stats = store.stats();
        assert_eq!(
            (stats.total_received, stats.total_stored),
            (5, 5),
            "the repeat and the held seq are not new receipts"
        );
    }

    /// A merge past capacity drops the LOWEST records — the front first, then the lowest of
    /// the merged run — and each raises the floor, before anything is pushed back.
    #[test]
    fn a_merge_past_capacity_drops_the_lowest_and_raises_the_floor() {
        let store = InMemoryStore::new(4);
        for s in [3, 4, 10] {
            store.append(entry(s, Some(1)));
        }
        store.insert_sorted(vec![entry(6, Some(1)), entry(5, Some(1))]);
        assert_eq!(ring(&store), vec![4, 5, 6, 10]);
        assert_eq!(store.lost_below(), 4, "3 was evicted");
        assert_eq!(
            seqs(store.logs_by_trace_id(1)),
            vec![4, 5, 6, 10],
            "and unindexed"
        );
        assert!(
            store.inner.read().unwrap().entries.capacity() < 8,
            "the ring did not grow past its one reservation"
        );

        // A batch entirely below what the ring keeps after the merge.
        store.insert_sorted(vec![entry(7, None), entry(8, None), entry(9, None)]);
        assert_eq!(
            ring(&store),
            vec![7, 8, 9, 10],
            "the lowest four of all seven are kept"
        );
        assert_eq!(store.lost_below(), 7);
    }

    /// After a clear the floor is the newest seq held then, plus one, and the ring is empty:
    /// the floor is checked before the fast path, so an older record is refused — counted as
    /// received, not stored — and a newer one is stored.
    #[test]
    fn after_a_clear_an_older_record_is_refused() {
        let store = InMemoryStore::new(16);
        for s in 5..=7 {
            store.append(entry(s, None));
        }
        store.clear();
        assert_eq!(store.lost_below(), 8);
        store.append(entry(3, None));
        assert_eq!(ring(&store), Vec::<u64>::new(), "below the floor: refused");
        store.append(entry(9, None));
        assert_eq!(ring(&store), vec![9]);
        let stats = store.stats();
        assert_eq!((stats.total_received, stats.total_stored), (5, 4));
    }

    /// `clear_through` refuses every record that arrived before the clear — the seq counter's
    /// value, not just the newest HELD — so a record a filter kept out, still in the
    /// pre-trigger buffer, cannot be flushed in after the clear.
    #[test]
    fn a_clear_through_the_counter_refuses_records_never_held() {
        let store = InMemoryStore::new(16);
        store.append(entry(5, None));
        // 6-8 arrived (seq assigned) and were never stored.
        store.clear_through(8);
        assert_eq!(
            store.lost_below(),
            6,
            "the LOSS floor rises past what was held (5) only — 6-8 were never held, so \
             nothing was lost there"
        );
        store.insert_sorted(vec![entry(6, None), entry(7, None), entry(8, None)]);
        store.append(entry(9, None));
        assert_eq!(ring(&store), vec![9], "only what arrived after the clear");
    }

    /// Clearing a ring that never held anything claims no loss: the loss floor stays 0, so a
    /// capture taken afterwards does not read an empty past as an evicted one.
    #[test]
    fn clearing_a_ring_that_held_nothing_claims_no_loss() {
        let store = InMemoryStore::new(16);
        store.clear_through(40);
        assert_eq!(store.lost_below(), 0);
        // The admission floor still refuses what arrived before the clear, on the fast path
        // (an empty ring, a seq above the loss floor).
        store.append(entry(30, None));
        assert_eq!(ring(&store), Vec::<u64>::new(), "arrived before the clear");
        store.append(entry(41, None));
        assert_eq!(ring(&store), vec![41]);
    }

    /// A ring of capacity 0 holds nothing on EITHER write path. The fast path used to evict
    /// nothing from the empty ring and push, holding one record the merge path never would.
    #[test]
    fn a_ring_of_capacity_zero_holds_nothing_on_either_path() {
        let store = InMemoryStore::new(0);
        store.append(entry(10, None));
        assert_eq!(ring(&store), Vec::<u64>::new(), "the fast path");
        assert_eq!(store.lost_below(), 11, "stored and evicted at once");
        store.insert_sorted(vec![entry(12, None), entry(11, None)]);
        assert_eq!(ring(&store), Vec::<u64>::new(), "the merge path");
        assert!(!store.contains_seq(10));
    }

    /// The range read is inclusive at both ends, ascending, empty for a range that holds
    /// nothing, and returns the floor. (That the two come from ONE lock is structural — no
    /// single-threaded test can tell.)
    #[test]
    fn a_range_read_returns_the_held_records_and_the_floor_together() {
        let store = InMemoryStore::new(4);
        for s in [2, 4, 6, 8, 10] {
            store.append(entry(s, None));
        }
        let (records, floor) = store.range_with_floor(4, 8);
        assert_eq!(seqs(records), vec![4, 6, 8]);
        assert_eq!(floor, 3, "2 was evicted");
        let (records, _) = store.range_with_floor(5, 5);
        assert!(records.is_empty());
        let (records, _) = store.range_with_floor(0, 100);
        assert_eq!(seqs(records), vec![4, 6, 8, 10]);
    }

    /// A loaded case's records are reachable by trace: `from_records` builds the trace index
    /// the readers use, not just the ring.
    #[test]
    fn a_loaded_store_reads_by_trace() {
        let store = InMemoryStore::from_records(
            16,
            vec![entry(1, Some(7)), entry(2, None), entry(3, Some(7))],
            1,
        );
        assert_eq!(seqs(store.logs_by_trace_id(7)), vec![1, 3]);
        assert_eq!(store.count_by_trace_id(7), 2);
    }

    /// A trace whose last record is evicted leaves no entry behind in the index: one leaked
    /// key per trace ever evicted would grow without bound.
    #[test]
    fn evicting_a_traces_last_record_removes_the_trace() {
        let store = InMemoryStore::new(2);
        store.append(entry(1, Some(7)));
        store.append(entry(2, Some(8)));
        store.append(entry(3, Some(8)));
        assert_eq!(store.count_by_trace_id(7), 0);
        let inner = store.inner.read().unwrap();
        assert!(
            !inner.trace_index.contains_key(&7),
            "no empty list for trace 7"
        );
        assert_eq!(inner.trace_index.len(), 1);
    }

    /// The full walk is oldest first — by seq, including records merged in out of order.
    #[test]
    fn the_full_walk_is_in_seq_order() {
        let store = InMemoryStore::new(16);
        for s in [1, 5, 3] {
            store.append(entry(s, None));
        }
        let mut seen = Vec::new();
        store.for_each_matching(None, |e| seen.push(e.seq));
        assert_eq!(seen, vec![1, 3, 5]);
    }

    /// A repeated seq is a bug, not a ring, whichever order it arrives in.
    #[test]
    #[should_panic(expected = "ascending, distinct seq order")]
    fn from_records_refuses_a_repeated_seq() {
        let _ = InMemoryStore::from_records(16, vec![entry(1, None), entry(1, None)], 0);
    }

    /// The case that used to read `[10, 5]` (the order stored): seq order now.
    #[test]
    fn a_record_appended_out_of_seq_order_is_held_in_seq_order() {
        let store = InMemoryStore::new(16);
        store.append(entry(10, Some(7)));
        store.append(entry(5, Some(7)));
        store.append(entry(11, Some(8)));
        assert_eq!(ring(&store), vec![5, 10, 11]);
        assert_eq!(seqs(store.logs_by_trace_id(7)), vec![5, 10]);
        assert_eq!(seqs(store.context_by_seq(10, 1, 1)), vec![5, 10, 11]);
    }

    /// The constructor owns the invariant too: an unsorted case is a bug, not a ring.
    #[test]
    #[should_panic(expected = "ascending, distinct seq order")]
    fn from_records_refuses_records_out_of_seq_order() {
        let _ = InMemoryStore::from_records(16, vec![entry(2, None), entry(1, None)], 0);
    }

    /// xorshift64: deterministic, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// The model: what should be held, the exact floor, the counters.
    #[derive(Default)]
    struct Model {
        held: BTreeMap<u64, Option<u128>>,
        floor: u64,
        /// The admission floor a `clear_through` sets; never a loss claim.
        admit: u64,
        received: u64,
        stored: u64,
    }

    impl Model {
        fn offer(&mut self, batch: &[(u64, Option<u128>)], cap: usize) {
            let mut fresh: BTreeMap<u64, Option<u128>> = BTreeMap::new();
            for &(s, t) in batch {
                if !self.held.contains_key(&s) {
                    fresh.entry(s).or_insert(t);
                }
            }
            self.received += fresh.len() as u64;
            for (s, t) in fresh {
                if s >= self.floor && s >= self.admit {
                    self.held.insert(s, t);
                    self.stored += 1;
                }
            }
            while self.held.len() > cap {
                let (s, _) = self.held.pop_first().unwrap();
                self.floor = self.floor.max(s + 1);
            }
        }

        fn clear(&mut self) {
            if let Some((&newest, _)) = self.held.last_key_value() {
                self.floor = self.floor.max(newest + 1);
            }
            self.held.clear();
        }

        fn clear_through(&mut self, newest_assigned: u64) {
            self.clear();
            self.admit = self.admit.max(newest_assigned + 1);
        }
    }

    /// Differential: a seeded mix of in-order appends, single late appends, unsorted batches
    /// with repeats and held seqs (the trigger shape), below-floor arrivals, merges that
    /// overflow a small ring, and clears — checked against the model after every step.
    ///
    /// In-order appends skip seqs, as a real ring does (records a filter kept out), because
    /// those gaps are what a trigger's flush fills; with no gaps every late record is either
    /// held already or below the floor, and almost no merge moves anything.
    #[test]
    fn the_ring_matches_the_model_under_every_writer() {
        const CAP: usize = 16;
        let store = InMemoryStore::new(CAP);
        let mut model = Model::default();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut next_seq = 1_000u64;
        let (mut late, mut batches, mut moved_any, mut refused, mut clears, mut nonempty) =
            (0, 0, 0, 0, 0, 0);
        let mut clears_through = 0;
        for _ in 0..5_000 {
            let r = rng.next();
            let trace = match r % 4 {
                0 => None,
                k => Some(k as u128),
            };
            match (r >> 8) % 10 {
                // A batch reaching back up to 40 seqs: older than the newest held, some held
                // already, some repeated, some (after evictions) below the floor.
                0..=2 => {
                    let n = 1 + (r >> 16) % 6;
                    let batch: Vec<(u64, Option<u128>)> = (0..n)
                        .map(|i| {
                            let back = (rng.next() % 40) + 1;
                            let t = if i % 2 == 0 { trace } else { None };
                            (next_seq.saturating_sub(back), t)
                        })
                        .collect();
                    let floor_before = model.floor.max(model.admit);
                    refused += batch.iter().filter(|(s, _)| *s < floor_before).count();
                    model.offer(&batch, CAP);
                    let moved =
                        store.insert_sorted(batch.iter().map(|&(s, t)| entry(s, t)).collect());
                    if moved > 0 {
                        moved_any += 1;
                    }
                    batches += 1;
                }
                // A single record older than the newest: `append`'s merge path.
                3 => {
                    let s = next_seq.saturating_sub((r >> 16) % 30 + 1);
                    model.offer(&[(s, trace)], CAP);
                    store.append(entry(s, trace));
                    late += 1;
                }
                4 if (r >> 16).is_multiple_of(25) => {
                    if (r >> 24).is_multiple_of(2) {
                        model.clear();
                        store.clear();
                    } else {
                        // The daemon's clear: through the counter, which sits above the
                        // newest held whenever records were kept out — and the counter, like
                        // the real one, moves on past it.
                        let through = next_seq + (r >> 26) % 5;
                        model.clear_through(through);
                        store.clear_through(through);
                        next_seq = through + 1;
                        clears_through += 1;
                    }
                    clears += 1;
                }
                _ => {
                    model.offer(&[(next_seq, trace)], CAP);
                    store.append(entry(next_seq, trace));
                    next_seq += 1 + (r >> 20) % 3;
                }
            }

            let expect: Vec<u64> = model.held.keys().copied().collect();
            assert_eq!(
                ring(&store),
                expect,
                "the ring holds exactly the model's records, ascending"
            );
            assert_eq!(store.lost_below(), model.floor, "the floor is exact");
            let stats = store.stats();
            assert_eq!(
                (stats.total_received, stats.total_stored),
                (model.received, model.stored),
                "the counters"
            );
            for t in 1..=3u128 {
                let by_model: Vec<u64> = model
                    .held
                    .iter()
                    .filter(|(_, &tt)| tt == Some(t))
                    .map(|(&s, _)| s)
                    .collect();
                let got = seqs(store.logs_by_trace_id(t));
                assert_eq!(got, by_model, "trace {t}");
                assert_eq!(
                    store.count_by_trace_id(t),
                    by_model.len(),
                    "trace {t} count"
                );
                if !got.is_empty() {
                    nonempty += 1;
                }
            }
            let probe = next_seq.saturating_sub(rng.next() % 50);
            assert_eq!(
                store.contains_seq(probe),
                model.held.contains_key(&probe),
                "contains {probe}"
            );
            if let Some(&anchor) = expect.get(expect.len() / 2) {
                let i = expect.len() / 2;
                let want = &expect[i.saturating_sub(2)..(i + 3).min(expect.len())];
                assert_eq!(
                    seqs(store.context_by_seq(anchor, 2, 2)),
                    want,
                    "context around {anchor}"
                );
            }
        }
        // Vacuity guards: the run exercised what it claims to.
        assert!(late > 100, "late appends: {late}");
        assert!(batches > 500, "batches: {batches}");
        assert!(
            moved_any > 100,
            "merges that moved held records: {moved_any}"
        );
        assert!(refused > 50, "below-floor offers: {refused}");
        assert!(clears > 2, "clears: {clears}");
        assert!(
            clears_through > 0,
            "clears through the counter: {clears_through}"
        );
        assert!(
            nonempty > 1_000,
            "trace lookups that returned records: {nonempty}"
        );
    }
}
