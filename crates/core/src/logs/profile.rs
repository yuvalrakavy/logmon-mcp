//! `logs.profile` — how records distribute along one named axis.
//!
//! The sibling of `fields`: that one says which dimensions exist, this one says
//! how the population spreads along the one you pick.
//!
//! **The reserved buckets are the point, not the edge case.** Measured on a
//! live 9347-record buffer, 20 of 31 candidate axes are absent from more than
//! 90% of records — `kind` from 86.4%. A breakdown that silently dropped the
//! records lacking its axis would disagree with `matched` on almost every call,
//! so `__absent__` is a first-class row. `__overflow__` is its sibling for the
//! cardinality cap, which on that same buffer never fires: the highest real
//! axis holds 204 distinct values against a cap of 1024. It is a leak guard,
//! not a working mechanism, and the two must not be conflated — "lacked the
//! field" and "folded past the cap" are different facts.

use crate::collector::intern::{ABSENT_LABEL, DEFAULT_GROUP_VALUE_CAP, OVERFLOW_LABEL};
use crate::gelf::message::{Level, LogEntry};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

pub use logmon_broker_protocol::methods::{LevelCounts, LogGroup, Suppressed};

/// Distinct accumulator KEYS before the fold.
///
/// The cap counts whole keys, not values per member. Interning each member
/// independently — what `collector/state.rs:67-71` does for spans — bounds the
/// table at `cap^K`, which is not a bound. Each row here holds a `LevelCounts`,
/// four bounds and two exemplar `String`s and is accumulated under the store
/// read lock, so an unbounded table is exactly what `intern.rs` warns of: "an
/// unbounded table is how a key like `request.id` turns a bounded collector
/// into a leak".
pub const KEY_CAP: usize = DEFAULT_GROUP_VALUE_CAP;

/// The most tuple members accepted, mirroring `collector::sample::MAX_GROUP_KEYS`.
pub const MAX_KEYS: usize = 8;

/// Which dimension to group along.
///
/// The arms are exactly the rows `logs.fields` reports — the eight `BUILTINS`
/// plus `field` for the open set — so that read's output is this one's input
/// with no translation. `ALL`/`as_str`/`parse` are the single enumeration:
/// the rejection message and the schema-agreement test both read them, so
/// neither can drift, and adding a variant is a compile error at `as_str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    None,
    Level,
    Message,
    Host,
    Facility,
    File,
    Line,
    TraceId,
    SpanId,
    /// Emitter fields, named by `group_keys`.
    Field,
}

impl Axis {
    pub const ALL: [Axis; 10] = [
        Axis::None,
        Axis::Level,
        Axis::Message,
        Axis::Host,
        Axis::Facility,
        Axis::File,
        Axis::Line,
        Axis::TraceId,
        Axis::SpanId,
        Axis::Field,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Axis::None => "none",
            Axis::Level => "level",
            Axis::Message => "message",
            Axis::Host => "host",
            Axis::Facility => "facility",
            Axis::File => "file",
            Axis::Line => "line",
            Axis::TraceId => "trace_id",
            Axis::SpanId => "span_id",
            Axis::Field => "field",
        }
    }

    /// What a rejection should say it expected. Derived, never written out —
    /// the span side wrote its list by hand once and omitted `none`, leaving
    /// the only spelling for "the overall figures" undiscoverable from the
    /// error that was refusing you.
    pub fn accepted() -> String {
        crate::rejection::one_of(&Self::ALL.map(Self::as_str))
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" | "" => Axis::None,
            "level" => Axis::Level,
            "message" => Axis::Message,
            "host" => Axis::Host,
            "facility" => Axis::Facility,
            "file" => Axis::File,
            "line" => Axis::Line,
            "trace_id" => Axis::TraceId,
            "span_id" => Axis::SpanId,
            "field" => Axis::Field,
            _ => return None,
        })
    }

    /// The name this axis is spelled with in a `Suppressed` entry.
    fn member_label(self, keys: &[String], i: usize) -> String {
        match self {
            Axis::Field => keys.get(i).cloned().unwrap_or_default(),
            other => other.as_str().to_string(),
        }
    }
}

/// One member of a group key.
///
/// **Typed rather than a `String`, and that is load-bearing.** `fields::render`
/// returns a string value unchanged, so an emitter field valued literally
/// `"__absent__"` would share a `String` key with the reserved bucket, merge
/// with the genuinely-absent records, and make the denominator lie — the defect
/// the reserved buckets exist to prevent, made invisible by the key type. The
/// span side avoids it by construction: a real value interns to an id at or
/// above `FIRST_REAL_ID`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum MemberKey {
    Value(String),
    Absent,
    Overflow,
}

impl MemberKey {
    fn label(&self) -> &str {
        match self {
            MemberKey::Value(s) => s,
            MemberKey::Absent => ABSENT_LABEL,
            MemberKey::Overflow => OVERFLOW_LABEL,
        }
    }
}

/// Which reserved bucket a whole key is, if any.
///
/// A *partially* absent tuple — `store.reconcile / __absent__` — is a real row
/// about real records that carry one key and lack the other, so it competes for
/// `top_n` like anything else. Only an all-reserved key is a reserved bucket.
/// `Overflow` is always all-or-nothing because the fold replaces the whole key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserved {
    Absent,
    Overflow,
}

fn reserved_kind(key: &[MemberKey]) -> Option<Reserved> {
    if key.iter().all(|m| *m == MemberKey::Absent) {
        Some(Reserved::Absent)
    } else if key.iter().all(|m| *m == MemberKey::Overflow) {
        Some(Reserved::Overflow)
    } else {
        None
    }
}

fn label(key: &[MemberKey]) -> String {
    // A single key renders bare so the common case reads as the value itself
    // rather than a one-element tuple — `collector/project.rs:913-932`.
    if key.len() == 1 {
        return key[0].label().to_string();
    }
    key.iter()
        .map(MemberKey::label)
        .collect::<Vec<_>>()
        .join(" / ")
}

fn bump(levels: &mut LevelCounts, level: Level) {
    match level {
        Level::Trace => levels.trace += 1,
        Level::Debug => levels.debug += 1,
        Level::Info => levels.info += 1,
        Level::Warn => levels.warn += 1,
        Level::Error => levels.error += 1,
    }
}

/// One group's accumulator. Both ends of the row are kept whole.
struct Acc {
    count: usize,
    levels: LevelCounts,
    first_seq: u64,
    first_time: DateTime<Utc>,
    first_exemplar: String,
    last_seq: u64,
    last_time: DateTime<Utc>,
    last_exemplar: String,
}

impl Acc {
    fn new(e: &LogEntry) -> Self {
        let mut levels = LevelCounts::default();
        bump(&mut levels, e.level);
        Self {
            count: 1,
            levels,
            first_seq: e.seq,
            first_time: e.timestamp,
            first_exemplar: e.message.clone(),
            last_seq: e.seq,
            last_time: e.timestamp,
            last_exemplar: e.message.clone(),
        }
    }

    /// Ends are chosen by `seq`, never by visit order.
    ///
    /// The walk is oldest-first while every read an agent has seen is
    /// newest-first, and both orders live in one file (`store/memory.rs`). The
    /// timestamp and exemplar move WITH the seq, in the same branch, so a row
    /// can never describe two records at once: `timestamp` is emitter-supplied
    /// and `seq` is assigned at receipt, so `max(timestamp)` and
    /// `timestamp@max(seq)` are different records under reordering or skew.
    fn observe(&mut self, e: &LogEntry) {
        self.count += 1;
        bump(&mut self.levels, e.level);
        if e.seq < self.first_seq {
            self.first_seq = e.seq;
            self.first_time = e.timestamp;
            self.first_exemplar = e.message.clone();
        }
        if e.seq > self.last_seq {
            self.last_seq = e.seq;
            self.last_time = e.timestamp;
            self.last_exemplar = e.message.clone();
        }
    }
}

/// Folds matched records into group rows without materialising them.
///
/// Driven by `InMemoryStore::for_each_matching`, which hands out borrowed
/// entries under the store lock — so this cannot take a slice.
pub struct GroupMap {
    axis: Axis,
    keys: Vec<String>,
    accs: HashMap<Vec<MemberKey>, Acc>,
    capped: bool,
    /// Per MEMBER, counted on the raw walk before any interning or folding.
    ///
    /// It cannot be derived from the `__absent__` row: an overflow fold
    /// replaces the whole key, so `x / __absent__` becomes
    /// `__overflow__ / __overflow__` and the evidence is destroyed at exactly
    /// the cardinality where a reader most needs it. And a per-AXIS count would
    /// miss the shape tuples were added for — with `["kind", "targett"]` where
    /// only the second is a typo, the tuple is never wholly absent, so nothing
    /// would fire and the reply would look like a valid two-dimensional answer.
    present: Vec<usize>,
    /// Per member: records carrying the field with a value that cannot be a
    /// dimension (null, array, object). They land in `__absent__` and are
    /// announced, because folding them silently would make that bucket mean
    /// two things with nothing saying so.
    structured: Vec<usize>,
    /// Records carrying an ADDITIONAL field of the same name as a built-in
    /// axis — the remedy's candidate, and one `usize` rather than a map of
    /// every name in the buffer, which is the unbounded table `NAME_CAP` was
    /// added to stop.
    ///
    /// Only meaningful off the `field` axis: there the member name already IS
    /// the additional field, so this would restate `present`. It stays 0 for
    /// `trace_id`/`span_id` without a special case, because the parser removes
    /// those from `additional_fields` — which is exactly why they are the
    /// permanent no-candidate case.
    alt: Vec<usize>,
    levels: LevelCounts,
    matched: usize,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    first_time: Option<DateTime<Utc>>,
    last_time: Option<DateTime<Utc>>,
}

/// The ends of the matched population.
///
/// Named fields rather than a 4-tuple, for the reason `ScanCounts` gives one
/// module over: two `Option<u64>` side by side differ only in meaning, so a
/// tuple is one transposition away from reporting a window's ends backwards —
/// and `first`/`last` is exactly the pair a reader would not double-check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub first_time: Option<DateTime<Utc>>,
    pub last_time: Option<DateTime<Utc>>,
}

/// What a value extraction produced for one member of one record.
enum Extracted {
    Value(String),
    /// The field is not there at all.
    Missing,
    /// Present, but null/array/object — `ValueKind::Other`'s own doc calls that
    /// "neither a dimension nor a number", and the span side excludes it too.
    ///
    /// There is a correctness reason beyond consistency: `serde_json` carries
    /// `preserve_order` in this workspace, so `Value::Object.to_string()`
    /// preserves emitter key order and a Rust emitter logging a `HashMap` field
    /// would split one logical value across groups nondeterministically.
    Structured,
}

impl GroupMap {
    pub fn new(axis: Axis, keys: Vec<String>) -> Self {
        let members = if axis == Axis::Field { keys.len().max(1) } else { 1 };
        Self {
            axis,
            keys,
            accs: HashMap::new(),
            capped: false,
            present: vec![0; members],
            structured: vec![0; members],
            alt: vec![0; members],
            levels: LevelCounts::default(),
            matched: 0,
            first_seq: None,
            last_seq: None,
            first_time: None,
            last_time: None,
        }
    }

    /// Extract one member's value, spelled **exactly** as `logs::fields` spells
    /// it (`fields.rs:228-262`).
    ///
    /// Stated per axis rather than routed through one function, because
    /// `logs.fields` does not have one: `render` handles additional fields
    /// only, and every built-in is spelled by hand. The divergences are the
    /// invisible ones — `Display` rather than `Debug` for a level, ids
    /// zero-padded to 32 and 16, and an empty `host` counted ABSENT although
    /// the field is a `String` and nothing else signals absence. A cross-tool
    /// test asserts the two agree, in both directions.
    fn extract(&self, e: &LogEntry, i: usize) -> Extracted {
        let opt = match self.axis {
            // Never `{:?}`: `Level` has a canonical `Display`
            // (`gelf/message.rs`) and `from_name` parses it back. Debug happens
            // to agree today, which is exactly how a spelling drifts unnoticed.
            Axis::Level => Some(e.level.to_string()),
            Axis::Message => Some(e.message.clone()),
            // `LogEntry.host` is `String`, not `Option`, so emptiness is the
            // only absence signal there is — and `logs.fields` treats it that
            // way, reporting such a row at 0% coverage.
            Axis::Host => (!e.host.is_empty()).then(|| e.host.clone()),
            Axis::Facility => e.facility.clone(),
            Axis::File => e.file.clone(),
            Axis::Line => e.line.map(|v| v.to_string()),
            // Zero-padded: `{t:x}` yields a different key for the same trace.
            Axis::TraceId => e.trace_id.map(|t| format!("{t:032x}")),
            Axis::SpanId => e.span_id.map(|s| format!("{s:016x}")),
            Axis::Field => {
                let Some(name) = self.keys.get(i) else {
                    return Extracted::Missing;
                };
                return match e.additional_fields.get(name) {
                    None => Extracted::Missing,
                    Some(v) if is_dimension(v) => {
                        Extracted::Value(crate::logs::fields::render(v))
                    }
                    Some(_) => Extracted::Structured,
                };
            }
            Axis::None => None,
        };
        match opt {
            Some(s) => Extracted::Value(s),
            None => Extracted::Missing,
        }
    }

    pub fn observe(&mut self, e: &LogEntry) {
        self.matched += 1;
        bump(&mut self.levels, e.level);
        if self.first_seq.is_none_or(|s| e.seq < s) {
            self.first_seq = Some(e.seq);
            self.first_time = Some(e.timestamp);
        }
        if self.last_seq.is_none_or(|s| e.seq > s) {
            self.last_seq = Some(e.seq);
            self.last_time = Some(e.timestamp);
        }
        if self.axis == Axis::None {
            return;
        }
        // The remedy's candidate: does an ADDITIONAL field of this built-in's
        // name exist? On the measured buffer the built-in `line` matches 0 of
        // 9347 records while an additional `line` matches 9344, and saying so
        // turns a dead end into the call that works.
        if self.axis != Axis::Field && e.additional_fields.contains_key(self.axis.as_str()) {
            self.alt[0] += 1;
        }

        let mut key: Vec<MemberKey> = Vec::with_capacity(self.present.len());
        for i in 0..self.present.len() {
            key.push(match self.extract(e, i) {
                Extracted::Value(v) => {
                    self.present[i] += 1;
                    MemberKey::Value(v)
                }
                Extracted::Structured => {
                    self.structured[i] += 1;
                    MemberKey::Absent
                }
                Extracted::Missing => MemberKey::Absent,
            });
        }

        // Past the cap a NEW key folds whole; keys already known keep counting,
        // so what is reported stays truthful and the omission is announced.
        //
        // **The reserved buckets are exempt, and that is not a detail.** They
        // are structural, not values competing for space: if `__absent__` were
        // capped, a buffer whose distinct values filled the table BEFORE the
        // first record lacking the field would fold that record into
        // `__overflow__` — conflating "lacked the field" with "folded past the
        // cap", which is the one distinction these two buckets exist to keep.
        // Caught by this module's own cap test, whose fixture appends the
        // key-less record last. A partially-absent tuple (`x / __absent__`) is
        // NOT reserved and is capped like any other row: it describes real
        // records that carry one member.
        let exempt = reserved_kind(&key).is_some();
        if !exempt && !self.accs.contains_key(&key) && self.accs.len() >= KEY_CAP {
            self.capped = true;
            key = vec![MemberKey::Overflow; key.len()];
        }
        self.accs
            .entry(key)
            .and_modify(|a| a.observe(e))
            .or_insert_with(|| Acc::new(e));
    }

    pub fn matched(&self) -> usize {
        self.matched
    }

    pub fn levels(&self) -> LevelCounts {
        self.levels
    }

    pub fn bounds(&self) -> Bounds {
        Bounds {
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            first_time: self.first_time,
            last_time: self.last_time,
        }
    }

    pub fn cardinality_capped(&self) -> bool {
        self.capped
    }

    /// Every member absent from the whole matched population, plus every member
    /// whose values were structured. `matched == 0` yields nothing: every axis
    /// is absent from an empty population, and warning about the axis would
    /// misdirect a reader whose actual problem is the filter.
    ///
    /// `remedy` is `None` when nothing would help — its own contract
    /// (`Suppressed::remedy`: "Absent when nothing would help"). `trace_id` and
    /// `span_id` are the permanent no-candidate case: they are closed-enum
    /// arms, so there is no typo to have made, and the parser removes them from
    /// `additional_fields`, so there is never a same-named field to fall back
    /// on. `reason` stays factual in every case — never a "did you mean" when
    /// nothing was found.
    pub fn suppressed(&self) -> Vec<Suppressed> {
        let mut out = Vec::new();
        if self.matched == 0 || self.axis == Axis::None {
            return out;
        }
        for (i, present) in self.present.iter().enumerate() {
            let name = self.axis.member_label(&self.keys, i);
            if *present == 0 {
                let remedy = (self.alt[i] > 0).then(|| {
                    format!(
                        "an additional field named `{name}` covers {:.2}% — use \
                         group_by=\"field\", group_keys=[\"{name}\"]",
                        100.0 * self.alt[i] as f64 / self.matched as f64
                    )
                });
                out.push(Suppressed {
                    field: format!("group_keys[{i}]"),
                    reason: format!(
                        "`{name}` did not appear in any of the {} matched records, so \
                         every row is `{ABSENT_LABEL}`",
                        self.matched
                    ),
                    remedy,
                });
            } else if self.structured[i] > 0 {
                out.push(Suppressed {
                    field: format!("group_keys[{i}]"),
                    reason: format!(
                        "{} of {} records carry `{name}` with a null, array or object \
                         value, which is not a dimension; they are counted in \
                         `{ABSENT_LABEL}`",
                        self.structured[i], self.matched
                    ),
                    remedy: None,
                });
            }
        }
        out
    }

    /// Close the map. Returns the rows and `groups_total` **before** truncation.
    ///
    /// Reserved buckets sort last and are **outside the `top_n` budget**, so
    /// asking for 20 rows buys 20 real values. On the measured buffer
    /// `__absent__` holds 90%+ on 20 of 31 axes; by count it would take row 1
    /// and a slot, on most axes, for a bucket the caller did not ask about.
    /// `logs.fields` makes presence a column on every row rather than a row of
    /// its own, and this is the nearest equivalent that keeps the sibling
    /// method's row shape.
    pub fn finish(self, top_n: usize) -> (Vec<LogGroup>, usize) {
        if self.axis == Axis::None {
            return (Vec::new(), 0);
        }
        let groups_total = self.accs.len();

        let mut real: Vec<LogGroup> = Vec::new();
        let mut reserved: Vec<(Reserved, LogGroup)> = Vec::new();
        for (key, a) in self.accs {
            let row = LogGroup {
                key: label(&key),
                count: a.count,
                levels: a.levels,
                first_seq: a.first_seq,
                first_time: a.first_time,
                first_exemplar: a.first_exemplar,
                last_seq: a.last_seq,
                last_time: a.last_time,
                last_exemplar: a.last_exemplar,
            };
            match reserved_kind(&key) {
                Some(r) => reserved.push((r, row)),
                None => real.push(row),
            }
        }

        // Ties broken on the key so two identical calls agree; leaving that to
        // `HashMap` iteration is the reproducibility bug this project has ruled
        // against twice.
        real.sort_by(|x, y| y.count.cmp(&x.count).then_with(|| x.key.cmp(&y.key)));
        real.truncate(top_n);

        // `__absent__` before `__overflow__`, always, so two calls agree.
        reserved.sort_by_key(|(r, _)| match r {
            Reserved::Absent => 0,
            Reserved::Overflow => 1,
        });
        real.extend(reserved.into_iter().map(|(_, row)| row));
        (real, groups_total)
    }
}

/// Whether a JSON value can be a group key.
///
/// Null, arrays and objects cannot: `ValueKind::Other`'s own doc says such a
/// value is "present, but neither a dimension nor a number", and
/// `collector/state.rs` excludes them on the span side for the same reason.
fn is_dimension(v: &serde_json::Value) -> bool {
    matches!(
        v,
        serde_json::Value::String(_) | serde_json::Value::Number(_) | serde_json::Value::Bool(_)
    )
}

/// Convenience for callers that already hold the records (tests, mostly).
pub fn summarize<'a, I>(
    entries: I,
    axis: Axis,
    keys: Vec<String>,
    top_n: usize,
) -> (Vec<LogGroup>, usize)
where
    I: IntoIterator<Item = &'a LogEntry>,
{
    let mut map = GroupMap::new(axis, keys);
    for e in entries {
        map.observe(e);
    }
    map.finish(top_n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `top_n` above any fixture's group count.
    ///
    /// Whole-population invariants are asserted over the UNTRUNCATED set: the
    /// returned rows sum to `matched` only when the real-key count is at most
    /// `top_n`, so asserting it against a truncated reply would be a test that
    /// passes only on small fixtures — the trap `scanned` exists to avoid one
    /// level up.
    const ALL_ROWS: usize = 10_000;

    fn at(seq: u64, msg: &str, extra: &[(&str, serde_json::Value)]) -> LogEntry {
        let mut e = LogEntry::synthetic(Level::Info, msg);
        e.seq = seq;
        for (k, v) in extra {
            e.additional_fields.insert((*k).to_string(), v.clone());
        }
        e
    }

    fn row<'a>(rows: &'a [LogGroup], key: &str) -> &'a LogGroup {
        match rows.iter().find(|r| r.key == key) {
            Some(r) => r,
            None => {
                let have: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
                panic!("no row keyed `{key}`; got {have:?}")
            }
        }
    }

    fn field_axis(names: &[&str]) -> (Axis, Vec<String>) {
        (Axis::Field, names.iter().map(|s| (*s).to_string()).collect())
    }

    /// P1 + P2 — every matched record lands in exactly one row, including the
    /// ones lacking the axis.
    ///
    /// These two are one test because they are one property: dropping the
    /// records that lack the axis is what breaks reconciliation, and on the
    /// measured buffer that is 86% of them.
    #[test]
    fn every_matched_record_lands_in_exactly_one_row() {
        let logs = vec![
            at(1, "a", &[("kind", json!("x"))]),
            at(2, "b", &[("kind", json!("y"))]),
            at(3, "c", &[]),
            at(4, "d", &[("kind", json!("x"))]),
        ];
        let (axis, keys) = field_axis(&["kind"]);
        let (rows, total) = summarize(&logs, axis, keys, ALL_ROWS);

        let summed: usize = rows.iter().map(|r| r.count).sum();
        assert_eq!(summed, logs.len(), "rows must account for every record: {rows:?}");
        assert_eq!(total, 3, "x, y and the absent bucket");
        assert_eq!(row(&rows, "x").count, 2);
        assert_eq!(
            row(&rows, ABSENT_LABEL).count,
            1,
            "the record lacking `kind` is a row, not a silent drop"
        );
    }

    /// P3 — a field valued literally `__absent__` must not merge with the
    /// reserved bucket.
    ///
    /// `render` returns a string value unchanged, so a `String` key would put
    /// these records in the same bucket as the ones genuinely lacking the
    /// field, and the denominator would lie while every count still looked
    /// plausible. The typed key is what prevents it.
    #[test]
    fn a_literal_absent_value_does_not_merge_with_the_reserved_bucket() {
        let logs = vec![
            at(1, "a", &[("kind", json!(ABSENT_LABEL))]),
            at(2, "b", &[]),
        ];
        let (axis, keys) = field_axis(&["kind"]);
        let (rows, total) = summarize(&logs, axis, keys, ALL_ROWS);

        assert_eq!(total, 2, "a real `__absent__` value and the bucket are two keys");
        assert_eq!(
            rows.iter().filter(|r| r.key == ABSENT_LABEL).count(),
            2,
            "they render identically -- which is why the KEY, not the label, has \
             to keep them apart: {rows:?}"
        );
        for r in &rows {
            assert_eq!(r.count, 1, "neither absorbed the other");
        }
    }

    /// P3 — past the cap, keys fold to `__overflow__`, distinctly from
    /// `__absent__`, and the fold is announced.
    ///
    /// **The key-less record is appended LAST, after the cap is full**, and
    /// that ordering is the test. Put first, `__absent__` is already in the
    /// table and survives whatever the cap does; put last it is a *new* key
    /// arriving at a full table, and a cap that does not exempt the reserved
    /// buckets folds it into `__overflow__` — silently answering "lacked the
    /// field" with "folded past the cap". That is how this was found.
    #[test]
    fn past_the_cap_keys_fold_to_overflow_and_the_fold_is_announced() {
        let mut logs: Vec<LogEntry> = (0..KEY_CAP + 50)
            .map(|i| at(i as u64, "m", &[("u", json!(format!("v{i}")))]))
            .collect();
        logs.push(at(999_999, "no key here", &[]));

        let (axis, keys) = field_axis(&["u"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        assert!(map.cardinality_capped(), "the fold must be announced");
        let matched = map.matched();
        let (rows, _) = map.finish(ALL_ROWS);

        assert!(
            rows.iter().any(|r| r.key == OVERFLOW_LABEL),
            "the folded keys get a bucket"
        );
        assert_eq!(
            row(&rows, ABSENT_LABEL).count,
            1,
            "and it is NOT the same bucket as `lacked the field entirely`"
        );
        let summed: usize = rows.iter().map(|r| r.count).sum();
        assert_eq!(summed, matched, "the fold must not lose records");
    }

    /// P4 — `groups_total` counts before truncation, reserved buckets included.
    #[test]
    fn groups_total_counts_before_truncation() {
        let mut logs: Vec<LogEntry> = (0..50)
            .map(|i| at(i, "m", &[("k", json!(format!("v{i}")))]))
            .collect();
        logs.push(at(500, "m", &[]));

        let (axis, keys) = field_axis(&["k"]);
        let (rows, total) = summarize(&logs, axis, keys, 5);
        assert_eq!(total, 51, "50 values plus the absent bucket");
        assert_eq!(
            rows.len(),
            6,
            "5 real rows, and the reserved bucket does NOT consume one of them: {rows:?}"
        );
        assert!(
            total > rows.len(),
            "which is exactly the case `groups_total` exists to make visible"
        );
    }

    /// P5, P6 and P7 share one fixture, and the fixture is the test.
    ///
    /// Append order is deliberately not seq order, and timestamp order
    /// contradicts both. On a seq-ordered fixture "the record at max(seq)" and
    /// "the last record visited" coincide, so the obvious wrong implementation
    /// passes; and taking `max(timestamp)` picks a different record than
    /// `timestamp@max(seq)` only when the two orders disagree.
    fn out_of_order() -> Vec<LogEntry> {
        let base = LogEntry::synthetic(Level::Info, "").timestamp;
        let mut v = Vec::new();
        for (seq, msg, secs) in [
            (30u64, "middle", 100i64),
            (10, "oldest by seq", 300), // newest timestamp, oldest seq
            (50, "newest by seq", 1),   // oldest timestamp, newest seq
            (20, "second", 200),
        ] {
            let mut e = at(seq, msg, &[("k", json!("one"))]);
            e.timestamp = base + chrono::Duration::seconds(secs);
            v.push(e);
        }
        v
    }

    #[test]
    fn the_ends_of_a_row_are_min_and_max_seq_not_visit_order() {
        let logs = out_of_order();
        let (axis, keys) = field_axis(&["k"]);
        let (rows, _) = summarize(&logs, axis, keys, ALL_ROWS);
        let r = row(&rows, "one");
        assert_eq!(r.first_seq, 10, "min seq, not the first visited");
        assert_eq!(r.last_seq, 50, "max seq, not the last visited");
    }

    #[test]
    fn each_exemplar_comes_from_the_record_at_its_own_end() {
        let logs = out_of_order();
        let (axis, keys) = field_axis(&["k"]);
        let (rows, _) = summarize(&logs, axis, keys, ALL_ROWS);
        let r = row(&rows, "one");
        assert_eq!(r.first_exemplar, "oldest by seq");
        assert_eq!(r.last_exemplar, "newest by seq");
    }

    #[test]
    fn each_timestamp_comes_from_the_same_record_as_its_seq() {
        let logs = out_of_order();
        let by_seq = |s: u64| logs.iter().find(|e| e.seq == s).unwrap();
        let (axis, keys) = field_axis(&["k"]);
        let (rows, _) = summarize(&logs, axis, keys, ALL_ROWS);
        let r = row(&rows, "one");
        assert_eq!(
            r.first_time,
            by_seq(10).timestamp,
            "the timestamp must come from the record at first_seq, NOT be \
             min(timestamp) -- here the two disagree, which is the point"
        );
        assert_eq!(r.last_time, by_seq(50).timestamp);
        assert!(
            r.first_time > r.last_time,
            "the fixture is only meaningful if timestamp order contradicts seq \
             order; if this fails the other two assertions prove nothing"
        );
    }

    /// The WHOLE-POPULATION bounds, which are a different quantity from any
    /// row's.
    ///
    /// A mutation campaign replaced `bounds()` with `Bounds::default()` and all
    /// 30 tests stayed green: every reply would have carried `first_seq: null`
    /// regardless of what matched, and nothing asserted it. The integration
    /// test whose name suggested it should — "carries its evidence and both
    /// ends of each row" — reads its `first_seq` off a ROW.
    ///
    /// Driven on the out-of-order fixture so min/max is distinguishable from
    /// visit order here too, and asserted across two groups so it cannot be
    /// satisfied by one row's bounds.
    #[test]
    fn the_population_bounds_span_every_matched_record() {
        let mut logs = out_of_order();
        // A second group, and the extremes of the population belong to it.
        let mut low = at(3, "lowest", &[("k", json!("other"))]);
        low.timestamp = logs[0].timestamp - chrono::Duration::seconds(500);
        let mut high = at(90, "highest", &[("k", json!("other"))]);
        high.timestamp = logs[0].timestamp + chrono::Duration::seconds(500);
        logs.push(low);
        logs.push(high);

        let (axis, keys) = field_axis(&["k"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        let b = map.bounds();
        assert_eq!(b.first_seq, Some(3), "min over the whole matched set");
        assert_eq!(b.last_seq, Some(90), "max over the whole matched set");

        let by_seq = |s: u64| logs.iter().find(|e| e.seq == s).unwrap().timestamp;
        assert_eq!(
            b.first_time,
            Some(by_seq(3)),
            "and the times come from those same records, not from min/max timestamp"
        );
        assert_eq!(b.last_time, Some(by_seq(90)));

        // Without this the assertions above would hold for a bounds() that
        // simply tracked one group.
        let (rows, _) = map.finish(ALL_ROWS);
        assert!(rows.len() > 1, "the extremes must span groups: {rows:?}");
    }

    /// An empty population has no bounds, rather than bounds of zero.
    #[test]
    fn an_empty_population_has_no_bounds() {
        let b = GroupMap::new(Axis::Level, Vec::new()).bounds();
        assert_eq!(b, Bounds::default());
        assert_eq!(b.first_seq, None, "not Some(0), which is a real seq");
    }

    /// P8 — a wholly-absent axis MEMBER is announced, named, per member.
    #[test]
    fn a_wholly_absent_field_axis_is_announced() {
        let logs = vec![at(1, "a", &[("kind", json!("x"))])];
        let (axis, keys) = field_axis(&["targett"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        let s = map.suppressed();
        assert_eq!(s.len(), 1, "one entry, for the one absent member: {s:?}");
        assert!(s[0].reason.contains("targett"), "it must NAME it: {s:?}");
        assert_eq!(
            s[0].remedy, None,
            "nothing would help, and `remedy`'s own contract is `absent when \
             nothing would help` -- a `did you mean` with no target misdirects"
        );
    }

    /// P8 — the hole the design gate found: with one good key and one typo, the
    /// TUPLE is never wholly absent, so a per-axis check fires nothing and the
    /// reply looks like a valid two-dimensional answer.
    #[test]
    fn a_wholly_absent_tuple_member_is_announced_by_name() {
        let logs = vec![
            at(1, "a", &[("kind", json!("x"))]),
            at(2, "b", &[("kind", json!("y"))]),
        ];
        let (axis, keys) = field_axis(&["kind", "targett"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        let s = map.suppressed();
        assert_eq!(
            s.len(),
            1,
            "the present member must not be reported, or the warning means \
             nothing: {s:?}"
        );
        assert!(
            s[0].reason.contains("targett") && !s[0].reason.contains("kind"),
            "it must name the ABSENT member specifically: {s:?}"
        );

        let (rows, _) = map.finish(ALL_ROWS);
        assert!(
            rows.iter().all(|r| r.key.ends_with(ABSENT_LABEL)),
            "every row is partially absent -- which is exactly why nothing \
             fired before: {rows:?}"
        );
    }

    /// P8 — a built-in axis absent from the buffer, which is the
    /// best-documented instance: `fi`/`ln`/`fa` match nothing on any emitter
    /// that sends everything as underscore-prefixed extras.
    #[test]
    fn a_wholly_absent_builtin_axis_is_announced_with_the_sharp_remedy() {
        let logs = vec![at(1, "a", &[("line", json!(42))])];
        let mut map = GroupMap::new(Axis::Line, Vec::new());
        for e in &logs {
            map.observe(e);
        }
        // No candidate map is passed in: the projection counted it on the same
        // walk, from the fixture's own additional `line` field. A caller-supplied
        // map is a second source for one fact, and this is the fact the remedy
        // is entirely made of.
        let s = map.suppressed();
        assert_eq!(s.len(), 1, "{s:?}");
        assert!(s[0].reason.contains("line"), "{s:?}");
        let remedy = s[0].remedy.as_deref().expect("a candidate exists, so a remedy does");
        assert!(
            remedy.contains("group_keys=[\"line\"]"),
            "the remedy must be the call that works: {remedy}"
        );
    }

    /// P9 — `trace_id` is the permanent no-candidate case: a closed-enum arm
    /// cannot be a typo, and the parser removes it from `additional_fields`, so
    /// no same-named field can ever exist to suggest.
    #[test]
    fn an_absent_trace_id_axis_gets_a_factual_reason_and_no_remedy() {
        let logs = vec![at(1, "background job", &[])];
        let mut map = GroupMap::new(Axis::TraceId, Vec::new());
        for e in &logs {
            map.observe(e);
        }
        let s = map.suppressed();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].remedy, None);
        assert!(
            !s[0].reason.contains("did you mean"),
            "a factual reason, never a suggestion with no target: {s:?}"
        );
    }

    /// P10 — an empty population warns about nothing. Every axis is absent from
    /// it, and blaming the axis misdirects a reader whose problem is the filter.
    #[test]
    fn an_empty_population_produces_no_absent_axis_warning() {
        let (axis, keys) = field_axis(&["anything"]);
        let map = GroupMap::new(axis, keys);
        assert_eq!(map.matched(), 0);
        assert!(map.suppressed().is_empty());
    }

    /// P12 — a tuple keeps `__absent__` PER MEMBER rather than folding whole.
    #[test]
    fn a_partially_absent_tuple_keeps_its_present_member() {
        let logs = vec![at(1, "a", &[("kind", json!("x"))])];
        let (axis, keys) = field_axis(&["kind", "region"]);
        let (rows, _) = summarize(&logs, axis, keys, ALL_ROWS);
        assert_eq!(
            rows[0].key,
            format!("x / {ABSENT_LABEL}"),
            "folding the whole tuple would throw away the half that IS known"
        );
    }

    /// P13 — two identical calls agree, including where counts tie.
    #[test]
    fn ordering_is_stable_across_identical_calls() {
        let logs = vec![
            at(1, "m", &[("k", json!("a"))]),
            at(2, "m", &[("k", json!("b"))]),
            at(3, "m", &[("k", json!("c"))]),
        ];
        let (axis, keys) = field_axis(&["k"]);
        let once: Vec<String> = summarize(&logs, axis, keys.clone(), ALL_ROWS)
            .0
            .into_iter()
            .map(|r| r.key)
            .collect();
        let twice: Vec<String> = summarize(&logs, axis, keys, ALL_ROWS)
            .0
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(
            once, twice,
            "all three tie on count; leaving that to HashMap iteration makes \
             two identical calls disagree"
        );
    }

    /// P16 — per-group levels sum to the top-level levels, per level.
    #[test]
    fn per_group_levels_reconcile_with_the_whole_population() {
        let mut logs = vec![
            at(1, "a", &[("k", json!("x"))]),
            at(2, "b", &[("k", json!("y"))]),
            at(3, "c", &[]),
        ];
        logs[0].level = Level::Error;
        logs[1].level = Level::Warn;

        let (axis, keys) = field_axis(&["k"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        let whole = map.levels();
        let (rows, _) = map.finish(ALL_ROWS);

        let mut summed = LevelCounts::default();
        for r in &rows {
            summed.trace += r.levels.trace;
            summed.debug += r.levels.debug;
            summed.info += r.levels.info;
            summed.warn += r.levels.warn;
            summed.error += r.levels.error;
        }
        assert_eq!(summed, whole, "two counts of one population must agree");
        assert_eq!(whole.total(), logs.len());
        assert_eq!(whole.error, 1, "and the fixture must actually span levels");
        assert_eq!(whole.warn, 1);
    }

    /// P20 — a structured value is not a dimension: it folds to `__absent__`
    /// and is announced, because folding it silently would make that bucket
    /// mean two things with nothing saying so.
    #[test]
    fn a_structured_value_folds_to_absent_and_is_announced() {
        let logs = vec![
            at(1, "a", &[("k", json!({"nested": 1}))]),
            at(2, "b", &[("k", json!(["a", "b"]))]),
            at(3, "c", &[("k", json!("plain"))]),
        ];
        let (axis, keys) = field_axis(&["k"]);
        let mut map = GroupMap::new(axis, keys);
        for e in &logs {
            map.observe(e);
        }
        let s = map.suppressed();
        assert_eq!(s.len(), 1, "{s:?}");
        assert!(s[0].reason.contains('2'), "it must say HOW MANY: {s:?}");

        let (rows, _) = map.finish(ALL_ROWS);
        assert_eq!(row(&rows, ABSENT_LABEL).count, 2);
        assert_eq!(row(&rows, "plain").count, 1);
    }

    /// P21 — an ungrouped call returns no rows at all, rather than one.
    #[test]
    fn an_ungrouped_call_returns_no_rows_but_still_counts() {
        let logs = vec![at(1, "a", &[]), at(2, "b", &[])];
        let mut map = GroupMap::new(Axis::None, Vec::new());
        for e in &logs {
            map.observe(e);
        }
        assert_eq!(map.matched(), 2, "the population is still described");
        assert_eq!(map.levels().info, 2);
        let (rows, total) = map.finish(ALL_ROWS);
        assert!(rows.is_empty());
        assert_eq!(total, 0, "the handler renders this as `null`, not `0`");
    }

    /// **R — the cross-tool identity.** For every built-in axis, the group keys
    /// this projection produces must equal the values `logs.fields` reports for
    /// the same row.
    ///
    /// One assertion pins level spelling (`Display`, not `Debug`), trace/span
    /// id zero-padding, `line` stringification and the empty-`host` rule at
    /// once — and it fails on drift in EITHER direction. Nothing inside either
    /// tool can see this: both are internally consistent while disagreeing with
    /// each other, which is the failure a caller meets one call apart.
    #[test]
    fn group_keys_agree_with_what_logs_fields_reports() {
        use crate::logs::fields::{self, FieldSource};

        let mut e = LogEntry::synthetic(Level::Warn, "a message");
        e.seq = 7;
        e.host = "box-1".into();
        e.facility = Some("app".into());
        e.file = Some("src/main.rs".into());
        e.line = Some(42);
        e.trace_id = Some(0x1234);
        e.span_id = Some(0xabcd);
        let logs = [e];

        let stats = fields::summarize(&logs, 1, 50);
        let expected = |source: FieldSource, name: &str| -> Vec<String> {
            stats
                .iter()
                .find(|r| r.source == source && r.field == name)
                .unwrap_or_else(|| panic!("logs.fields has no {source:?} row for `{name}`"))
                .top_values
                .iter()
                .map(|t| t.value.clone())
                .collect()
        };

        for (axis, source, name) in [
            (Axis::Level, FieldSource::Builtin, "level"),
            (Axis::Message, FieldSource::Builtin, "message"),
            (Axis::Host, FieldSource::Builtin, "host"),
            (Axis::Facility, FieldSource::Builtin, "facility"),
            (Axis::File, FieldSource::Builtin, "file"),
            (Axis::Line, FieldSource::Builtin, "line"),
            (Axis::TraceId, FieldSource::Promoted, "trace_id"),
            (Axis::SpanId, FieldSource::Promoted, "span_id"),
        ] {
            let (rows, _) = summarize(&logs, axis, Vec::new(), ALL_ROWS);
            let mut got: Vec<String> = rows
                .iter()
                .filter(|r| r.key != ABSENT_LABEL && r.key != OVERFLOW_LABEL)
                .map(|r| r.key.clone())
                .collect();
            let mut want = expected(source, name);
            got.sort();
            want.sort();
            assert_eq!(
                got, want,
                "`{}` disagrees with what logs.fields reports for {source:?}/{name} \
                 -- an agent reading the map and then measuring would get two \
                 different vocabularies one call apart",
                axis.as_str()
            );
        }
    }

    /// The half of R that a populated fixture cannot show: `logs.fields` reports
    /// an empty `host` at 0% coverage, so the profile must treat it as absent
    /// rather than as a group keyed on the empty string.
    #[test]
    fn an_empty_host_is_absent_in_both_tools() {
        use crate::logs::fields::{self, FieldSource};

        let mut e = LogEntry::synthetic(Level::Info, "m");
        e.host = String::new();
        let logs = [e];

        let stats = fields::summarize(&logs, 1, 3);
        let host = stats
            .iter()
            .find(|r| r.source == FieldSource::Builtin && r.field == "host")
            .expect("host row");
        assert_eq!(host.present, 0, "logs.fields calls an empty host absent");

        let (rows, _) = summarize(&logs, Axis::Host, Vec::new(), ALL_ROWS);
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            vec![ABSENT_LABEL],
            "so the profile must too -- an `\"\"` group would be a value the map \
             says does not exist: {rows:?}"
        );
    }

    /// The axis vocabulary is a single enumeration, and `parse` must round-trip
    /// every arm it advertises. A new variant is a compile error at `as_str`;
    /// this catches a stale `ALL` or a `parse` arm that never matches.
    #[test]
    fn every_advertised_axis_parses_back() {
        for a in Axis::ALL {
            assert_eq!(Axis::parse(a.as_str()), Some(a), "{}", a.as_str());
        }
        assert_eq!(Axis::parse(""), Some(Axis::None), "the empty spelling");
        assert_eq!(Axis::parse("LEVEL"), None, "and it is case-sensitive");
        assert!(Axis::accepted().contains("none"), "{}", Axis::accepted());
    }
}
