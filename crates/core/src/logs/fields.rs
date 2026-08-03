//! `logs.fields` — what dimensions exist in this buffer.
//!
//! The map an agent needs before it can measure. `logs.profile` requires the
//! caller to name an axis; an agent arriving cold does not know which axes
//! exist, and guessing produces a silent empty result rather than an error.
//!
//! **This is also the tool that makes dead axes visible.** GELF strips the `_`
//! prefix and routes those keys into `additional_fields`
//! (`gelf/message.rs:210-214`), while `LogEntry.file`/`line`/`facility` come
//! from *top-level* GELF keys many emitters never send. So the filter selectors
//! `fi`, `ln` and `fa` can match nothing at all while `file` and `line` match
//! nearly every record as additional fields. Reporting a built-in at 0% is the
//! whole point: absence is a fact, and omitting the row would hide it.

use crate::collector::intern::DEFAULT_GROUP_VALUE_CAP;
use crate::gelf::message::LogEntry;
use std::collections::HashMap;

/// Wire types live in the protocol crate; this module produces them.
pub use logmon_broker_protocol::methods::{FieldSource, FieldStats, TopValue, ValueKind};

/// Field-NAME cardinality cap.
///
/// Values were capped from the start; names were not, and `intern.rs` states
/// the doctrine this violated: "an unbounded table is how a key like
/// `request.id` turns a bounded collector into a leak". A payload minting a
/// fresh field name per record produced ~10k rows and ~1.2 MB of reply on a
/// default ring — every sibling read is bounded, and this one was not.
///
/// Sized against `DEFAULT_GROUP_VALUE_CAP` rather than `DEFAULT_NAME_CAP`,
/// whose own doc says it bounds *distinct span names carrying their own stats
/// and sketch* — a different quantity that happens to share the word "name".
/// 1024 rows is ~120 KB at this reply's row size, and the low bound bit
/// harder than it looks: the walk is oldest-first and there is no eviction, so
/// a saturated cap keeps the field vocabulary of the OLDEST retained records
/// and none of the newest — the reverse of what an investigation wants.
pub const NAME_CAP: usize = DEFAULT_GROUP_VALUE_CAP;

/// Distinct values tracked per field before the counter gives up.
///
/// Shares the collector's group-value cap rather than inventing a second
/// number: both bound "how many distinct values will we hold for one axis", and
/// two independently-chosen limits would drift.
pub const DISTINCT_CAP: usize = DEFAULT_GROUP_VALUE_CAP;

fn kind_of(v: &serde_json::Value) -> ValueKind {
    match v {
        serde_json::Value::String(_) => ValueKind::String,
        serde_json::Value::Bool(_) => ValueKind::Bool,
        serde_json::Value::Number(n) => {
            if n.is_f64() {
                ValueKind::Float
            } else {
                ValueKind::Integer
            }
        }
        _ => ValueKind::Other,
    }
}

fn merge_kind(a: ValueKind, b: ValueKind) -> ValueKind {
    use ValueKind::*;
    match (a, b) {
        (x, y) if x == y => x,
        // An emitter writing `100` for one record and `100.5` for the next is
        // describing one quantity, and it sums fine. Reporting `mixed` there
        // would fire the "do not assume this is summable" warning at the one
        // case where the warning is false.
        (Integer, Float) | (Float, Integer) | (Number, Integer) | (Integer, Number)
        | (Number, Float) | (Float, Number) => Number,
        _ => Mixed,
    }
}

/// Render a value the way the FILTER MATCHER renders it
/// (`filter/matcher.rs:99-105`), so a `top_values` entry is a string the caller
/// can paste into a filter and have it match.
///
/// Diverging here would produce a summary whose own suggestions do not work.
fn render(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Identity of a row. **`(source, name)`, never the name alone.**
///
/// GELF strips the `_` prefix, so a payload carrying both `file` and `_file`
/// yields a top-level `LogEntry.file` and an `additional_fields["file"]`. Keyed
/// on the name alone they merged: coverage went to 200%, a `u32` built-in
/// beside a string extra reported `mixed`, and — worst — the built-in's 0% row
/// was absorbed, which is precisely the fact this read exists to show.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RowKey {
    source: FieldSource,
    name: String,
}

/// Accumulator for one field across the walk.
#[derive(Default)]
struct Acc {
    present: usize,
    counts: HashMap<String, usize>,
    capped: bool,
    kind: Option<ValueKind>,
}

impl Acc {
    fn observe(&mut self, rendered: String, kind: ValueKind) {
        self.present += 1;
        self.kind = Some(match self.kind {
            Some(k) => merge_kind(k, kind),
            None => kind,
        });
        // Past the cap we stop LEARNING new values but keep counting the ones
        // already known, so `top_values` stays truthful about what it saw. The
        // distinct COUNT is what becomes unknowable, and it is reported as
        // `null` rather than as the capped figure.
        if self.counts.len() >= DISTINCT_CAP && !self.counts.contains_key(&rendered) {
            self.capped = true;
            return;
        }
        *self.counts.entry(rendered).or_insert(0) += 1;
    }
}

/// Folds records into a field map without materialising them.
///
/// Driven by `InMemoryStore::for_each_matching`, which hands out borrowed
/// entries under the store lock — so this cannot take a slice, and taking one
/// would mean cloning every matched record (a `HashMap` each) for data thrown
/// away immediately after.
pub struct FieldMap {
    accs: HashMap<RowKey, Acc>,
    names_capped: bool,
}

/// Deliberately delegates to `new`. A derived `Default` would hand back an
/// empty map, so `FieldMap::default()` would silently skip the built-in seeding
/// below and drop every 0%-coverage row — the exact fact this type exists to
/// surface.
impl Default for FieldMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Always-reported fields: `(name, source, selector, statically-known kind)`.
///
/// **Seeded so an absent one renders at 0% rather than vanishing** — the whole
/// point of the tool. `fi`/`ln`/`fa` match nothing on a buffer whose emitter
/// sends everything as underscore-prefixed extras, and an omitted row reads as
/// "no such field" when the truth is "this field exists, is never populated,
/// and you want the other spelling".
///
/// The **kind is stated, not inferred**. These slots are typed in Rust
/// (`line: Option<u32>`), so reporting a never-populated `line` as "neither a
/// dimension nor a number" would be a false claim about the schema on the very
/// row the reader is here to see.
///
/// The **selector is what you actually type**, and it is not the field name:
/// `parse_selector` (`filter/parser.rs:286`) maps `file` to
/// `AdditionalField("file")`, so filtering the BUILT-IN needs `fi`. `trace_id`
/// and `span_id` have no selector at all — the parser removes them from
/// `additional_fields`, so `trace_id=…` matches nothing and reports no error.
const BUILTINS: &[(&str, FieldSource, Option<&str>, ValueKind)] = &[
    ("level", FieldSource::Builtin, Some("l"), ValueKind::String),
    ("message", FieldSource::Builtin, Some("m"), ValueKind::String),
    ("host", FieldSource::Builtin, Some("h"), ValueKind::String),
    ("facility", FieldSource::Builtin, Some("fa"), ValueKind::String),
    ("file", FieldSource::Builtin, Some("fi"), ValueKind::String),
    ("line", FieldSource::Builtin, Some("ln"), ValueKind::Integer),
    ("trace_id", FieldSource::Promoted, None, ValueKind::String),
    ("span_id", FieldSource::Promoted, None, ValueKind::String),
];

/// Keyed on `(source, name)`, not the name alone — the same anti-pattern this
/// module was re-modelled to remove. Unreachable today (built-ins are seeded
/// with a kind, and an additional row cannot exist without a value), but a
/// name-only lookup here would hand a built-in's kind to a same-named
/// additional row the moment that invariant moved.
fn builtin_kind(source: FieldSource, name: &str) -> Option<ValueKind> {
    BUILTINS
        .iter()
        .find(|(n, s, ..)| *n == name && *s == source)
        .map(|(_, _, _, k)| *k)
}

impl FieldMap {
    pub fn new() -> Self {
        let mut accs: HashMap<RowKey, Acc> = HashMap::new();
        for (name, source, _, kind) in BUILTINS {
            accs.entry(RowKey {
                source: *source,
                name: (*name).to_string(),
            })
            .or_insert(Acc {
                // Stated from the schema, so a 0%-coverage row still tells the
                // truth about what the field holds.
                kind: Some(*kind),
                ..Acc::default()
            });
        }
        Self {
            accs,
            names_capped: false,
        }
    }

    fn bump(&mut self, name: &str, source: FieldSource, rendered: String, kind: ValueKind) {
        let key = RowKey {
            source,
            name: name.to_string(),
        };
        // Names are capped like values are. Past the cap we keep counting rows
        // we already know and stop learning new ones, so what is reported stays
        // truthful and the omission is announced rather than silent.
        if !self.accs.contains_key(&key) && self.accs.len() >= NAME_CAP {
            self.names_capped = true;
            return;
        }
        self.accs.entry(key).or_default().observe(rendered, kind);
    }

    /// Observe one record. Built-ins are recorded as ordinary fields, including
    /// when they are never populated — see the module doc.
    pub fn observe(&mut self, e: &LogEntry) {
        use FieldSource::*;
        // `Display`, not `{:?}` — `Level` has a canonical rendering
        // (`gelf/message.rs:71`) and `from_name` parses it back. Debug happened
        // to agree, which is exactly how a spelling drifts unnoticed.
        self.bump("level", Builtin, e.level.to_string(), ValueKind::String);
        self.bump("message", Builtin, e.message.clone(), ValueKind::String);
        if !e.host.is_empty() {
            self.bump("host", Builtin, e.host.clone(), ValueKind::String);
        }
        if let Some(v) = &e.facility {
            self.bump("facility", Builtin, v.clone(), ValueKind::String);
        }
        if let Some(v) = &e.file {
            self.bump("file", Builtin, v.clone(), ValueKind::String);
        }
        if let Some(v) = e.line {
            self.bump("line", Builtin, v.to_string(), ValueKind::Integer);
        }
        // The parser `.remove()`s these from `additional_fields`
        // (`gelf/message.rs:216-224`), so without a row here an agent grouping
        // by `trace_id` would get a silent 100%-absent bucket.
        if let Some(t) = e.trace_id {
            self.bump("trace_id", Promoted, format!("{t:032x}"), ValueKind::String);
        }
        if let Some(s) = e.span_id {
            self.bump("span_id", Promoted, format!("{s:016x}"), ValueKind::String);
        }
        // Keyed as `Additional`, so `_file` gets its OWN row rather than
        // merging with the built-in `file` — they are different values reached
        // by different selectors, and merging them double-counted coverage and
        // hid the built-in's true presence.
        for (k, v) in &e.additional_fields {
            self.bump(k, Additional, render(v), kind_of(v));
        }
    }

    /// Whether the field-NAME cap was hit, so the caller can say so rather than
    /// presenting a truncated map as the whole one.
    pub fn names_capped(&self) -> bool {
        self.names_capped
    }

    /// Close the map.
    ///
    /// `matched` is passed in rather than counted here: the caller already has
    /// it from the walk, and a second count taken over a different iteration is
    /// how two numbers describing one population come to disagree.
    pub fn finish(self, matched: usize, top_values: usize) -> Vec<FieldStats> {
        let mut rows: Vec<FieldStats> = self
            .accs
            .into_iter()
            .map(|(key, a)| {
                let RowKey { source, name: field } = key;
                // A built-in's selector is fixed; an additional field is
                // reached by its own name, because `parse_selector` falls
                // through to `AdditionalField(name)`.
                let selector = match source {
                    FieldSource::Builtin | FieldSource::Promoted => BUILTINS
                        .iter()
                        .find(|(n, s, ..)| *n == field && *s == source)
                        .and_then(|(_, _, sel, _)| sel.map(String::from)),
                    // NOT `Some(field.clone())`. GELF validates nothing after
                    // the `_`, so `_h` yields an additional field named `h` —
                    // and `h=value` resolves to `Selector::Host`, matching a
                    // different field silently. Verified by round-trip, so a
                    // claimed selector always reaches the row it sits on.
                    FieldSource::Additional => {
                        crate::filter::parser::additional_field_selector(&field)
                    }
                };
                // BEFORE truncation. Reading it off `top` afterwards would
                // report `min(distinct, top_values)` — a number that looks
                // plausible, never exceeds the requested row count, and is
                // wrong for every field with more values than rows requested.
                let distinct = if a.capped { None } else { Some(a.counts.len()) };

                let mut top: Vec<TopValue> = a
                    .counts
                    .into_iter()
                    .map(|(value, count)| TopValue { value, count })
                    .collect();
                // Ties by value so two identical calls agree — the same rule
                // `collector::project` applies to its group rows.
                top.sort_by(|x, y| y.count.cmp(&x.count).then_with(|| x.value.cmp(&y.value)));
                top.truncate(top_values);

                FieldStats {
                    coverage_pct: if matched == 0 {
                        0.0
                    } else {
                        100.0 * a.present as f64 / matched as f64
                    },
                    present: a.present,
                    distinct,
                    top_values: top,
                    // A never-observed built-in keeps its stated schema kind
                    // (seeded in `new`); only an unseeded row can be unknown,
                    // and an additional field cannot exist without a value.
                    kind: a
                        .kind
                        .or_else(|| builtin_kind(source, &field))
                        .unwrap_or(ValueKind::Other),
                    source,
                    selector,
                    field,
                }
            })
            .collect();

        // Coverage descending, then name, then source — stable across reads
        // (`collector/project.rs:1254-1266`). Source is in the key because two
        // rows can now share a name, and leaving THAT tie to hash order is the
        // same reproducibility bug one level down.
        rows.sort_by(|x, y| {
            y.present
                .cmp(&x.present)
                .then_with(|| x.field.cmp(&y.field))
                .then_with(|| format!("{:?}", x.source).cmp(&format!("{:?}", y.source)))
        });
        rows
    }
}

/// Convenience for callers that already hold the records (tests, mostly).
pub fn summarize<'a, I>(entries: I, matched: usize, top_values: usize) -> Vec<FieldStats>
where
    I: IntoIterator<Item = &'a LogEntry>,
{
    let mut map = FieldMap::new();
    for e in entries {
        map.observe(e);
    }
    map.finish(matched, top_values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gelf::message::Level;
    use serde_json::json;

    fn entry(msg: &str, extra: &[(&str, serde_json::Value)]) -> LogEntry {
        let mut e = LogEntry::synthetic(Level::Info, msg);
        for (k, v) in extra {
            e.additional_fields.insert((*k).to_string(), v.clone());
        }
        e
    }

    /// Look up by `(source, name)` — the row's real identity. A helper keyed on
    /// the name alone would have hidden the collision this model exists to fix.
    fn row<'a>(rows: &'a [FieldStats], source: FieldSource, name: &str) -> &'a FieldStats {
        match rows
            .iter()
            .find(|r| r.field == name && r.source == source)
        {
            Some(r) => r,
            None => {
                let have: Vec<String> = rows
                    .iter()
                    .map(|r| format!("{:?}/{}", r.source, r.field))
                    .collect();
                panic!("no {source:?} row for `{name}`; got {have:?}")
            }
        }
    }

    fn add<'a>(rows: &'a [FieldStats], name: &str) -> &'a FieldStats {
        row(rows, FieldSource::Additional, name)
    }

    /// F1 — coverage and distinct are exact over a known fixture.
    #[test]
    fn coverage_and_distinct_are_exact() {
        let logs = vec![
            entry("a", &[("kind", json!("x"))]),
            entry("b", &[("kind", json!("y"))]),
            entry("c", &[]),
            entry("d", &[("kind", json!("x"))]),
        ];
        let rows = summarize(&logs, logs.len(), 3);

        let k = add(&rows, "kind");
        assert_eq!(k.present, 3, "three of four carry it");
        assert_eq!(k.distinct, Some(2), "x and y");
        assert!(
            (k.coverage_pct - 75.0).abs() < 1e-9,
            "3/4 is 75%, got {}",
            k.coverage_pct
        );
        assert_eq!(k.kind, ValueKind::String);
        assert_eq!(k.top_values.first().map(|t| t.value.as_str()), Some("x"));
        assert_eq!(k.top_values.first().map(|t| t.count), Some(2));
    }

    /// The bug this caught during implementation: `distinct` read off the
    /// TRUNCATED top-values list reports `min(distinct, top_values)` — always
    /// plausible, never above the row count, and wrong for every field with
    /// more values than rows requested.
    #[test]
    fn distinct_counts_all_values_not_just_the_reported_ones() {
        let logs: Vec<LogEntry> = (0..10)
            .map(|i| entry("m", &[("id", json!(format!("v{i}")))]))
            .collect();
        let rows = summarize(&logs, logs.len(), 2);
        let f = add(&rows, "id");
        assert_eq!(f.top_values.len(), 2, "only two rows were asked for");
        assert_eq!(
            f.distinct,
            Some(10),
            "but ten distinct values were seen -- distinct must be counted \
             before truncation, not derived from the reported rows"
        );
    }

    /// F3 — a built-in absent from the data reports 0%, never omission.
    ///
    /// The property the whole tool exists for: `omitted` and `present but never
    /// populated` are different facts and must not render the same.
    #[test]
    fn an_absent_builtin_is_reported_at_zero_rather_than_dropped() {
        let logs = vec![entry("a", &[("file", json!("src/x.rs"))])];
        let rows = summarize(&logs, logs.len(), 3);

        // `file` here is an ADDITIONAL field; the built-in of the same name is
        // never populated. Both facts have to survive.
        let builtin_absent = row(&rows, FieldSource::Builtin, "facility");
        assert_eq!(builtin_absent.present, 0);
        assert_eq!(builtin_absent.coverage_pct, 0.0);
        assert_eq!(
            add(&rows, "file").present,
            1,
            "the additional field is populated, which is exactly the confusion \
             this row set exists to expose"
        );
    }

    /// F4 — promoted fields appear despite being removed from
    /// `additional_fields` at parse time.
    #[test]
    fn promoted_fields_are_reported_and_marked() {
        let mut e = LogEntry::synthetic(Level::Info, "m");
        e.trace_id = Some(0x1234);
        let rows = summarize(std::slice::from_ref(&e), 1, 3);
        let t = row(&rows, FieldSource::Promoted, "trace_id");
        assert_eq!(t.present, 1);
        assert_eq!(
            t.selector, None,
            "NO log filter selector reaches trace_id -- the parser removes it \
             from additional_fields, so `trace_id=x` would match nothing and \
             report no error. A row claiming a selector here would manufacture \
             the silent-empty bucket this row exists to prevent"
        );
        assert_eq!(
            row(&rows, FieldSource::Promoted, "span_id").selector,
            None
        );
    }

    /// The re-model's core property: a name is not an identity.
    ///
    /// GELF strips `_`, so a payload with both `file` and `_file` yields a
    /// built-in AND an additional field of the same name. Keyed on the name
    /// alone they merged — coverage reached 200% and the built-in's row was
    /// absorbed, hiding exactly the fact this read exists to show.
    #[test]
    fn a_builtin_and_a_same_named_additional_field_stay_separate() {
        let mut e = LogEntry::synthetic(Level::Info, "m");
        e.file = Some("builtin.rs".into());
        e.additional_fields
            .insert("file".into(), json!("extra.rs"));

        let rows = summarize(std::slice::from_ref(&e), 1, 3);

        let builtin = row(&rows, FieldSource::Builtin, "file");
        let extra = add(&rows, "file");

        assert_eq!(builtin.present, 1, "one record, not two");
        assert_eq!(extra.present, 1, "one record, not two");
        for r in [builtin, extra] {
            assert!(
                r.coverage_pct <= 100.0,
                "coverage cannot exceed 100% of matched; got {}",
                r.coverage_pct
            );
        }
        assert_eq!(builtin.selector.as_deref(), Some("fi"));
        assert_eq!(
            extra.selector.as_deref(),
            Some("file"),
            "different selectors reach these two, which is why they cannot \
             share a row"
        );
        assert_eq!(builtin.top_values.first().map(|t| t.value.as_str()), Some("builtin.rs"));
        assert_eq!(extra.top_values.first().map(|t| t.value.as_str()), Some("extra.rs"));
    }

    /// A never-populated built-in states its SCHEMA type rather than claiming
    /// to be "neither a dimension nor a number".
    #[test]
    fn an_unpopulated_builtin_keeps_its_known_kind() {
        let rows = summarize(&[LogEntry::synthetic(Level::Info, "m")], 1, 3);
        let line = row(&rows, FieldSource::Builtin, "line");
        assert_eq!(line.present, 0);
        assert_eq!(
            line.kind,
            ValueKind::Integer,
            "`line` is a u32; reporting `other` because this buffer holds none \
             of it is a false claim about the schema"
        );
    }

    /// Integers and floats under one name are still summable.
    #[test]
    fn integer_and_float_merge_to_number_not_mixed() {
        let logs = vec![
            entry("a", &[("d", json!(100))]),
            entry("b", &[("d", json!(100.5))]),
        ];
        assert_eq!(
            add(&summarize(&logs, 2, 3), "d").kind,
            ValueKind::Number,
            "one quantity emitted with and without a decimal point sums fine; \
             `mixed` would warn at the one case where the warning is false"
        );
    }

    /// Field NAMES are capped like values are, and the cap is announced.
    #[test]
    fn field_names_are_capped_and_the_cap_is_reported() {
        let logs: Vec<LogEntry> = (0..NAME_CAP + 50)
            .map(|i| entry("m", &[(Box::leak(format!("f{i}").into_boxed_str()), json!(1))]))
            .collect();
        let mut map = FieldMap::new();
        for e in &logs {
            map.observe(e);
        }
        assert!(
            map.names_capped(),
            "an unbounded name table is how a per-record key turns a bounded \
             read into a leak"
        );
        let rows = map.finish(logs.len(), 3);
        assert!(rows.len() <= NAME_CAP, "got {} rows", rows.len());
    }

    /// An additional field whose NAME collides with a DSL selector gets no
    /// selector, because none reaches it.
    ///
    /// GELF validates nothing after the `_`, so `_h` produces an additional
    /// field named `h` — and `h=value` resolves to `Selector::Host`, matching a
    /// different field silently and with no error. Claiming `h` here would hand
    /// the caller a filter that returns nothing: the fixed defect displaced out
    /// of name-space and into selector-space.
    #[test]
    fn an_additional_field_named_like_a_selector_gets_no_selector() {
        for reserved in ["h", "m", "fi", "ln", "fa", "fm", "mfm", "sn", "sv", "st", "sk", "l"] {
            let e = entry("x", &[(reserved, json!("SENTINEL"))]);
            let rows = summarize(std::slice::from_ref(&e), 1, 3);
            let r = add(&rows, reserved);
            assert_eq!(
                r.selector, None,
                "`{reserved}=SENTINEL` does not reach this field -- it resolves \
                 to a built-in selector (or fails to parse), so offering it \
                 would be a filter that silently matches nothing"
            );
            assert_eq!(r.present, 1, "the field is still REPORTED, just not filterable");
        }
    }

    /// And an ordinary name keeps its selector, or the check above would be
    /// indistinguishable from returning `None` for everything.
    #[test]
    fn an_ordinary_additional_field_keeps_its_name_as_selector() {
        let e = entry("x", &[("request_id", json!("abc"))]);
        let rows = summarize(std::slice::from_ref(&e), 1, 3);
        assert_eq!(
            add(&rows, "request_id").selector.as_deref(),
            Some("request_id")
        );
    }

    /// F5 — past the cap, `distinct` is `null` rather than a partial count.
    #[test]
    fn past_the_cap_distinct_is_unknown_not_wrong() {
        let logs: Vec<LogEntry> = (0..DISTINCT_CAP + 50)
            .map(|i| entry("m", &[("u", json!(format!("v{i}")))]))
            .collect();
        let rows = summarize(&logs, logs.len(), 3);
        assert_eq!(
            add(&rows, "u").distinct,
            None,
            "a capped count reported as a total is a wrong number wearing a \
             plausible one"
        );
        assert_eq!(add(&rows, "u").present, logs.len(), "presence is still exact");
    }

    /// F6 — disagreeing value types report `mixed`, so nothing downstream
    /// assumes the field can be summed.
    #[test]
    fn mixed_types_are_reported_as_mixed() {
        let logs = vec![
            entry("a", &[("n", json!(1))]),
            entry("b", &[("n", json!("two"))]),
        ];
        let rows = summarize(&logs, logs.len(), 3);
        assert_eq!(add(&rows, "n").kind, ValueKind::Mixed);

        let ints = vec![entry("a", &[("n", json!(1))]), entry("b", &[("n", json!(2))])];
        assert_eq!(
            add(&summarize(&ints, 2, 3), "n").kind,
            ValueKind::Integer,
            "and a consistent field keeps its type, or `mixed` means nothing"
        );
    }

    /// F8 — two identical calls agree, including where counts tie.
    #[test]
    fn ordering_is_stable_across_identical_calls() {
        let logs = vec![
            entry("m", &[("a", json!(1)), ("b", json!(1))]),
            entry("m", &[("a", json!(2)), ("b", json!(2))]),
        ];
        let first: Vec<String> = summarize(&logs, 2, 3).into_iter().map(|r| r.field).collect();
        let second: Vec<String> = summarize(&logs, 2, 3).into_iter().map(|r| r.field).collect();
        assert_eq!(
            first, second,
            "`a` and `b` tie on coverage; leaving the order to HashMap iteration \
             makes two identical calls disagree"
        );
    }

    /// `Default` must not bypass the built-in seeding.
    #[test]
    fn default_seeds_the_builtins_like_new() {
        let rows = FieldMap::default().finish(0, 3);
        assert_eq!(rows.len(), BUILTINS.len());
    }
}
