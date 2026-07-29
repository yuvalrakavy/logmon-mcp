//! The sample tier — spec §3.2, §3.4, §3.6.
//!
//! Columnar, chunked, and capped. Holds one record per matched span so that
//! self time, wall-clock union, call paths and sample-exact percentiles stay
//! read-time projections rather than ingest-time decisions.

use crate::span::types::{SpanKind, SpanStatus};
use std::sync::Arc;

/// Records per chunk.
///
/// **Measured, not chosen** (`docs/superpowers/probes-2026-07-29-gen-probe`):
/// a non-destructive read copies the active chunk under the lock, and that
/// copy costs 0.29 ms at 8 192 records against 1.80 ms at 65 536, with ingest
/// throughput under concurrent readers of 19.0 M vs 4.3 M spans/s. Smaller
/// chunks mean more `Arc`s in the sealed list, which is a pointer each.
pub const CHUNK_RECORDS: usize = 8_192;

/// Retention level (§3.2). Ordered: each unlocks what the previous does, plus
/// its own columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Exact tier only — no sample tier at all, so 0 bytes per match.
    Scalar,
    /// Timings, so sample-exact percentiles, wall union and warm-up exclusion.
    Timing,
    /// Adds identity, so self time, nesting detection, per-trace rollups and
    /// call paths. The default.
    Tree,
}

impl Level {
    /// Bytes retained per matched span, before group keys.
    pub const fn bytes_per_record(self) -> usize {
        match self {
            // start_ns 8 + end_ns 8 + name_id 4 + flags 1
            Level::Scalar => 0,
            Level::Timing => 21,
            // + span_id 8 + parent_span_id 8 + trace_id 16
            Level::Tree => 53,
        }
    }

    pub const fn has_samples(self) -> bool {
        !matches!(self, Level::Scalar)
    }

    pub const fn has_tree(self) -> bool {
        matches!(self, Level::Tree)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Level::Scalar => "scalar",
            Level::Timing => "timing",
            Level::Tree => "tree",
        }
    }
}

/// 4 bytes per match per declared group key (§3.2).
pub const BYTES_PER_GROUP_KEY: usize = 4;

/// Declared group keys per collector.
///
/// **A hard cap, because the group-key column is allocated eagerly and is the
/// one dimension the byte budget does not see.** `SampleChunk::with_capacity`
/// reserves `CHUNK_RECORDS * n_group_keys * 4` bytes the moment a collector is
/// constructed, while the daemon-wide reservation checks only the *declared*
/// `max_sample_bytes`. So `max_sample_bytes: 1024` with a hundred thousand
/// group keys passes every budget gate and then asks for gigabytes — and
/// `Vec::with_capacity` answers an allocation failure with `handle_alloc_error`,
/// which aborts the process rather than unwinding. One RPC call could stop the
/// daemon.
///
/// Eight is far above real use: §13 defers multi-key `group_by` entirely, so
/// the working case is one key.
pub const MAX_GROUP_KEYS: usize = 8;

/// Status and kind packed into one byte: status in bits 0–1, kind in bits 2–4.
pub fn pack_flags(status: &SpanStatus, kind: &SpanKind) -> u8 {
    let s: u8 = match status {
        SpanStatus::Unset => 0,
        SpanStatus::Ok => 1,
        SpanStatus::Error(_) => 2,
    };
    let k: u8 = match kind {
        SpanKind::Unspecified => 0,
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    };
    s | (k << 2)
}

pub fn flags_is_error(flags: u8) -> bool {
    flags & 0b11 == 2
}

pub fn flags_kind(flags: u8) -> SpanKind {
    match (flags >> 2) & 0b111 {
        1 => SpanKind::Internal,
        2 => SpanKind::Server,
        3 => SpanKind::Client,
        4 => SpanKind::Producer,
        5 => SpanKind::Consumer,
        _ => SpanKind::Unspecified,
    }
}

/// One span's retained record, as offered to the tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRecord {
    pub start_ns: i64,
    pub end_ns: i64,
    pub name_id: u32,
    pub flags: u8,
    pub span_id: u64,
    /// `0` means root — safe because `bytes_to_span_id` maps an all-zero id to
    /// `None`, so 0 is never a real span id.
    pub parent_span_id: u64,
    pub trace_id: u128,
}

/// A record-aligned group of columns. Chunks are sealed only when full, so
/// allocated capacity exceeds retained payload by at most one partial chunk
/// (§3.4) — which is what makes the byte budget a real bound.
#[derive(Debug, Clone, Default)]
pub struct SampleChunk {
    pub start_ns: Vec<i64>,
    pub end_ns: Vec<i64>,
    pub name_id: Vec<u32>,
    pub flags: Vec<u8>,
    /// Tree level only; empty otherwise.
    pub span_id: Vec<u64>,
    pub parent_span_id: Vec<u64>,
    pub trace_id: Vec<u128>,
    /// Flattened `record_index * n_group_keys + key_index`.
    pub group_ids: Vec<u32>,
}

impl SampleChunk {
    fn with_capacity(level: Level, n_group_keys: usize) -> Self {
        let n = CHUNK_RECORDS;
        Self {
            start_ns: Vec::with_capacity(n),
            end_ns: Vec::with_capacity(n),
            name_id: Vec::with_capacity(n),
            flags: Vec::with_capacity(n),
            span_id: if level.has_tree() {
                Vec::with_capacity(n)
            } else {
                Vec::new()
            },
            parent_span_id: if level.has_tree() {
                Vec::with_capacity(n)
            } else {
                Vec::new()
            },
            trace_id: if level.has_tree() {
                Vec::with_capacity(n)
            } else {
                Vec::new()
            },
            group_ids: Vec::with_capacity(n * n_group_keys),
        }
    }

    pub fn len(&self) -> usize {
        self.name_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The capped, chunked sample tier.
///
/// `Clone` is cheap by construction: sealed chunks are `Arc` pointer copies, so
/// only the active chunk is really copied — the same bound that makes a
/// non-destructive read O(active chunk) rather than O(retained).
#[derive(Clone)]
pub struct SampleTier {
    level: Level,
    n_group_keys: usize,
    max_bytes: usize,
    sealed: Vec<Arc<SampleChunk>>,
    active: SampleChunk,
    retained: usize,
    complete: bool,
}

/// A consistent view of the tier, taken under the collector lock and projected
/// outside it (§3.6).
pub struct SampleSnapshot {
    pub level: Level,
    pub n_group_keys: usize,
    pub sealed: Vec<Arc<SampleChunk>>,
    pub active: SampleChunk,
    pub complete: bool,
}

impl SampleTier {
    pub fn new(level: Level, n_group_keys: usize, max_bytes: usize) -> Self {
        // Clamped rather than asserted: this is the allocation site, so it is
        // the last place that can refuse an absurd width, and aborting the
        // daemon is never the better outcome. Callers validate first and return
        // a proper error; this is the backstop for one that forgets.
        let n_group_keys = n_group_keys.min(MAX_GROUP_KEYS);
        Self {
            level,
            n_group_keys,
            max_bytes,
            sealed: Vec::new(),
            active: SampleChunk::with_capacity(level, n_group_keys),
            retained: 0,
            complete: true,
        }
    }

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn len(&self) -> usize {
        self.retained
    }

    pub fn is_empty(&self) -> bool {
        self.retained == 0
    }

    /// `false` once the budget stopped retention — the sample is a prefix.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    fn bytes_per_record(&self) -> usize {
        self.level.bytes_per_record() + self.n_group_keys * BYTES_PER_GROUP_KEY
    }

    fn chunk_bytes(&self) -> usize {
        CHUNK_RECORDS * self.bytes_per_record()
    }

    /// How many chunks the budget affords. At least one, so a budget smaller
    /// than a single chunk still retains something rather than nothing.
    fn max_chunks(&self) -> usize {
        (self.max_bytes / self.chunk_bytes().max(1)).max(1)
    }

    /// Allocated capacity, not retained payload (§3.4).
    pub fn bytes_allocated(&self) -> usize {
        (self.sealed.len() + 1) * self.chunk_bytes()
    }

    /// Offer one record. Returns whether it was retained.
    ///
    /// At `Scalar` there is no sample tier, so nothing is ever retained and
    /// `complete` stays `true` — an absent tier is not a truncated one, and the
    /// two must not be conflated when reading `complete`.
    pub fn push(&mut self, rec: &SampleRecord, group_ids: &[u32]) -> bool {
        if !self.level.has_samples() {
            return false;
        }
        debug_assert_eq!(group_ids.len(), self.n_group_keys);

        // Truncation is terminal, and deliberately WITHOUT a `!self.complete`
        // early-return. The budget check below re-fires on every subsequent
        // call, because `sealed` only ever grows and `max_chunks` is fixed —
        // so retention can never resume and the retained set stays a
        // contiguous prefix.
        //
        // A guard was tried here and removed: it was unreachable, and
        // mutation-testing proved it (deleting it changed nothing). Dead code
        // that *looks* like the mechanism is worse than no code, because the
        // next reader — this one included — mistakes it for the thing keeping
        // the invariant. `truncation_is_terminal` pins the behaviour instead.
        if self.active.len() >= CHUNK_RECORDS {
            // A new chunk is about to be allocated — check the budget BEFORE
            // paying for it, so allocation never exceeds what was reserved.
            if self.sealed.len() + 1 >= self.max_chunks() {
                self.complete = false;
                return false;
            }
            // The sealed chunk becomes immutable and is thereafter shared by
            // pointer, which is what makes a read O(active chunk).
            let full = std::mem::replace(
                &mut self.active,
                SampleChunk::with_capacity(self.level, self.n_group_keys),
            );
            self.sealed.push(Arc::new(full));
        }

        self.active.start_ns.push(rec.start_ns);
        self.active.end_ns.push(rec.end_ns);
        self.active.name_id.push(rec.name_id);
        self.active.flags.push(rec.flags);
        if self.level.has_tree() {
            self.active.span_id.push(rec.span_id);
            self.active.parent_span_id.push(rec.parent_span_id);
            self.active.trace_id.push(rec.trace_id);
        }
        self.active.group_ids.extend_from_slice(group_ids);
        self.retained += 1;
        true
    }

    /// Clone a consistent view. Sealed chunks are `Arc` pointer copies; only
    /// the active chunk is actually copied, which is what bounds the work done
    /// under the lock to O(active chunk) rather than O(retained).
    pub fn snapshot(&self) -> SampleSnapshot {
        SampleSnapshot {
            level: self.level,
            n_group_keys: self.n_group_keys,
            sealed: self.sealed.clone(),
            active: self.active.clone(),
            complete: self.complete,
        }
    }
}

impl SampleSnapshot {
    pub fn len(&self) -> usize {
        self.sealed.iter().map(|c| c.len()).sum::<usize>() + self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every retained record, sealed chunks first then the active one — i.e.
    /// ingestion order.
    pub fn records(&self) -> impl Iterator<Item = SampleRecord> + '_ {
        let tree = self.level.has_tree();
        self.sealed
            .iter()
            .map(|c| c.as_ref())
            .chain(std::iter::once(&self.active))
            .flat_map(move |c| {
                (0..c.len()).map(move |i| SampleRecord {
                    start_ns: c.start_ns[i],
                    end_ns: c.end_ns[i],
                    name_id: c.name_id[i],
                    flags: c.flags[i],
                    span_id: if tree { c.span_id[i] } else { 0 },
                    parent_span_id: if tree { c.parent_span_id[i] } else { 0 },
                    trace_id: if tree { c.trace_id[i] } else { 0 },
                })
            })
    }

    /// The interned group-key ids for record `i`, in declaration order.
    pub fn group_ids_at<'a>(&self, chunk: &'a SampleChunk, i: usize) -> &'a [u32] {
        if self.n_group_keys == 0 {
            return &[];
        }
        let base = i * self.n_group_keys;
        &chunk.group_ids[base..base + self.n_group_keys]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(i: i64) -> SampleRecord {
        SampleRecord {
            start_ns: i,
            end_ns: i + 100,
            name_id: (i % 7) as u32,
            flags: pack_flags(&SpanStatus::Ok, &SpanKind::Internal),
            span_id: i as u64 + 1,
            parent_span_id: 0,
            trace_id: i as u128,
        }
    }

    #[test]
    fn flags_round_trip_every_status_and_kind() {
        for status in [
            SpanStatus::Unset,
            SpanStatus::Ok,
            SpanStatus::Error("x".into()),
        ] {
            for kind in [
                SpanKind::Unspecified,
                SpanKind::Internal,
                SpanKind::Server,
                SpanKind::Client,
                SpanKind::Producer,
                SpanKind::Consumer,
            ] {
                let f = pack_flags(&status, &kind);
                assert_eq!(
                    flags_kind(f),
                    kind,
                    "kind survives packing beside {status:?}"
                );
                assert_eq!(
                    flags_is_error(f),
                    matches!(status, SpanStatus::Error(_)),
                    "error bit survives packing beside {kind:?}"
                );
            }
        }
    }

    #[test]
    fn scalar_retains_nothing_and_stays_complete() {
        // An ABSENT sample tier is not a TRUNCATED one. Conflating them would
        // make `complete: false` mean two different things.
        let mut t = SampleTier::new(Level::Scalar, 0, 1 << 20);
        for i in 0..100 {
            assert!(!t.push(&rec(i), &[]));
        }
        assert_eq!(t.len(), 0);
        assert!(t.is_complete(), "nothing was truncated; there is no tier");
    }

    #[test]
    fn timing_omits_the_tree_columns() {
        let mut t = SampleTier::new(Level::Timing, 0, 1 << 20);
        t.push(&rec(1), &[]);
        let s = t.snapshot();
        assert_eq!(s.active.span_id.len(), 0, "no identity columns at timing");
        assert_eq!(s.active.start_ns.len(), 1);
        // And they read back as zero rather than as stale data.
        let r: Vec<_> = s.records().collect();
        assert_eq!(r[0].span_id, 0);
        assert_eq!(r[0].start_ns, 1);
    }

    #[test]
    fn records_read_back_in_ingestion_order_across_the_seal_boundary() {
        let mut t = SampleTier::new(Level::Tree, 0, 1 << 30);
        let n = CHUNK_RECORDS + CHUNK_RECORDS / 2;
        for i in 0..n {
            assert!(t.push(&rec(i as i64), &[]));
        }
        let s = t.snapshot();
        assert_eq!(s.len(), n, "every record is visible exactly once");
        let got: Vec<i64> = s.records().map(|r| r.start_ns).collect();
        let want: Vec<i64> = (0..n as i64).collect();
        assert_eq!(got, want, "sealed chunks precede the active one, in order");
    }

    #[test]
    fn truncation_stops_retention_and_marks_incomplete() {
        // Budget for exactly two chunks.
        let per_rec = Level::Tree.bytes_per_record();
        let max = 2 * CHUNK_RECORDS * per_rec;
        let mut t = SampleTier::new(Level::Tree, 0, max);

        let mut retained = 0;
        for i in 0..(CHUNK_RECORDS * 4) {
            if t.push(&rec(i as i64), &[]) {
                retained += 1;
            }
        }
        assert!(!t.is_complete(), "the budget stopped retention");
        assert_eq!(t.len(), retained);
        assert!(
            retained <= 2 * CHUNK_RECORDS,
            "retained {retained} must not exceed the budgeted capacity"
        );
        assert!(
            t.bytes_allocated() <= max + CHUNK_RECORDS * per_rec,
            "allocation exceeds the budget by at most one partial chunk"
        );
        // Prefix truncation: what was retained is the FIRST records, which is
        // what makes the retained set subtree-coherent for early traces.
        let s = t.snapshot();
        let first = s.records().next().expect("non-empty");
        assert_eq!(first.start_ns, 0);
    }

    #[test]
    fn truncation_is_terminal() {
        // Retention must never resume, or the retained set stops being a
        // contiguous prefix and self time / paths silently describe a set with
        // a hole in it.
        let per_rec = Level::Tree.bytes_per_record();
        let mut t = SampleTier::new(Level::Tree, 0, 2 * CHUNK_RECORDS * per_rec);
        for i in 0..(CHUNK_RECORDS * 3) {
            t.push(&rec(i as i64), &[]);
        }
        assert!(!t.is_complete());
        let at_truncation = t.len();

        for i in 0..(CHUNK_RECORDS * 5) {
            assert!(
                !t.push(&rec(1_000_000 + i as i64), &[]),
                "no record is retained after truncation"
            );
        }
        assert_eq!(t.len(), at_truncation, "the retained set never grows again");

        // And it is still a prefix: the last retained record precedes every
        // record offered after truncation.
        let s = t.snapshot();
        let last = s.records().last().expect("non-empty");
        assert!(last.start_ns < 1_000_000);
    }

    #[test]
    fn group_ids_are_stored_per_record_and_read_back_positionally() {
        let mut t = SampleTier::new(Level::Timing, 2, 1 << 20);
        t.push(&rec(1), &[10, 20]);
        t.push(&rec(2), &[11, 21]);
        let s = t.snapshot();
        assert_eq!(s.group_ids_at(&s.active, 0), &[10, 20]);
        assert_eq!(s.group_ids_at(&s.active, 1), &[11, 21]);
    }

    #[test]
    fn snapshot_is_isolated_from_later_ingestion() {
        // The read path projects outside the lock, so a snapshot must not
        // observe records appended after it was taken.
        let mut t = SampleTier::new(Level::Tree, 0, 1 << 30);
        t.push(&rec(1), &[]);
        let s = t.snapshot();
        t.push(&rec(2), &[]);
        assert_eq!(s.len(), 1, "the snapshot is a point-in-time view");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn bytes_per_record_matches_the_declared_column_widths() {
        // V1: re-derived rather than trusted from the spec's table.
        let timing = 8 + 8 + 4 + 1;
        let tree = timing + 8 + 8 + 16;
        assert_eq!(Level::Timing.bytes_per_record(), timing);
        assert_eq!(Level::Tree.bytes_per_record(), tree);
        assert_eq!(Level::Scalar.bytes_per_record(), 0);
        assert_eq!(std::mem::size_of::<i64>() * 2 + 4 + 1, timing);
        assert_eq!(std::mem::size_of::<u128>(), 16);
    }
}
