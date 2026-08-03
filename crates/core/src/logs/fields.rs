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
pub use logmon_broker_protocol::methods::{FieldStats, TopValue, ValueKind};

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
    if a == b {
        a
    } else {
        ValueKind::Mixed
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

/// Accumulator for one field across the walk.
#[derive(Default)]
struct Acc {
    present: usize,
    counts: HashMap<String, usize>,
    capped: bool,
    kind: Option<ValueKind>,
    promoted: bool,
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
    accs: HashMap<String, Acc>,
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

/// Built-in fields, always reported — `(name, promoted)`.
///
/// **Seeded so an absent one renders at 0% rather than vanishing.** This is the
/// whole point of the tool: `fi`/`ln`/`fa` match nothing on a buffer whose
/// emitter sends everything as underscore-prefixed additional fields, and an
/// omitted row would read as "no such field" when the truth is "this field
/// exists and is never populated — group by the additional-field spelling".
/// Omission and emptiness are different facts and must not render the same.
const BUILTINS: &[(&str, bool)] = &[
    ("level", false),
    ("message", false),
    ("host", false),
    ("facility", false),
    ("file", false),
    ("line", false),
    // Lifted out of `additional_fields` by the parser, so an agent that grep-ed
    // a record for field names would not find them there.
    ("trace_id", true),
    ("span_id", true),
];

impl FieldMap {
    pub fn new() -> Self {
        let mut accs: HashMap<String, Acc> = HashMap::new();
        for (name, promoted) in BUILTINS {
            accs.entry((*name).to_string()).or_insert(Acc {
                promoted: *promoted,
                ..Acc::default()
            });
        }
        Self { accs }
    }

    fn bump(&mut self, name: &str, rendered: String, kind: ValueKind, promoted: bool) {
        let a = self.accs.entry(name.to_string()).or_default();
        a.promoted = promoted;
        a.observe(rendered, kind);
    }

    /// Observe one record. Built-ins are recorded as ordinary fields, including
    /// when they are never populated — see the module doc.
    pub fn observe(&mut self, e: &LogEntry) {
        self.bump("level", format!("{:?}", e.level), ValueKind::String, false);
        self.bump("message", e.message.clone(), ValueKind::String, false);
        if !e.host.is_empty() {
            self.bump("host", e.host.clone(), ValueKind::String, false);
        }
        if let Some(v) = &e.facility {
            self.bump("facility", v.clone(), ValueKind::String, false);
        }
        if let Some(v) = &e.file {
            self.bump("file", v.clone(), ValueKind::String, false);
        }
        if let Some(v) = e.line {
            self.bump("line", v.to_string(), ValueKind::Integer, false);
        }
        // Promoted: the parser `.remove()`s these from `additional_fields`
        // (`gelf/message.rs:216-224`), so without a row here an agent grouping
        // by `trace_id` would get a silent 100%-absent bucket.
        if let Some(t) = e.trace_id {
            self.bump("trace_id", format!("{t:032x}"), ValueKind::String, true);
        }
        if let Some(s) = e.span_id {
            self.bump("span_id", format!("{s:016x}"), ValueKind::String, true);
        }
        for (k, v) in &e.additional_fields {
            self.bump(k, render(v), kind_of(v), false);
        }
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
            .map(|(field, a)| {
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
                    kind: a.kind.unwrap_or(ValueKind::Other),
                    promoted: a.promoted,
                    field,
                }
            })
            .collect();

        // Coverage descending, then name — stable across reads, per
        // `collector/project.rs:1254-1266`.
        rows.sort_by(|x, y| y.present.cmp(&x.present).then_with(|| x.field.cmp(&y.field)));
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

    fn row<'a>(rows: &'a [FieldStats], name: &str) -> &'a FieldStats {
        match rows.iter().find(|r| r.field == name) {
            Some(r) => r,
            None => {
                let have: Vec<&String> = rows.iter().map(|r| &r.field).collect();
                panic!("no row for `{name}`; got {have:?}")
            }
        }
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

        let k = row(&rows, "kind");
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
        let f = row(&rows, "id");
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
        let builtin_absent = row(&rows, "facility");
        assert_eq!(builtin_absent.present, 0);
        assert_eq!(builtin_absent.coverage_pct, 0.0);
        assert_eq!(
            row(&rows, "file").present,
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
        let t = row(&rows, "trace_id");
        assert_eq!(t.present, 1);
        assert!(t.promoted, "marked, or an agent looks for it in the wrong place");
        assert!(row(&rows, "span_id").promoted);
    }

    /// F5 — past the cap, `distinct` is `null` rather than a partial count.
    #[test]
    fn past_the_cap_distinct_is_unknown_not_wrong() {
        let logs: Vec<LogEntry> = (0..DISTINCT_CAP + 50)
            .map(|i| entry("m", &[("u", json!(format!("v{i}")))]))
            .collect();
        let rows = summarize(&logs, logs.len(), 3);
        assert_eq!(
            row(&rows, "u").distinct,
            None,
            "a capped count reported as a total is a wrong number wearing a \
             plausible one"
        );
        assert_eq!(row(&rows, "u").present, logs.len(), "presence is still exact");
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
        assert_eq!(row(&rows, "n").kind, ValueKind::Mixed);

        let ints = vec![entry("a", &[("n", json!(1))]), entry("b", &[("n", json!(2))])];
        assert_eq!(
            row(&summarize(&ints, 2, 3), "n").kind,
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
