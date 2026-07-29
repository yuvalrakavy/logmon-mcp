//! Collector state — spec §3.6.
//!
//! **One lock over everything.** Ingest takes it once per matched span and
//! updates every structure inside it, so a reader can never observe the exact
//! tier and the sample tier disagreeing. Non-destructive reads clone (sealed
//! chunks are `Arc` pointer copies, so only the active chunk is really
//! copied); only `swap` is destructive. That shape is not a guess — a probe
//! showed the swap-and-fold alternative over-reports self time, because self
//! time is not additive across a generation boundary.

use crate::collector::exact::{classify, is_error, Duration, ExactStats};
use crate::collector::intern::{
    Interner, ABSENT_ID, DEFAULT_GROUP_VALUE_CAP, DEFAULT_NAME_CAP, OVERFLOW_ID,
};
use crate::collector::sample::{pack_flags, Level, SampleRecord, SampleSnapshot, SampleTier};
use crate::filter::parser::ParsedFilter;
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Distinct group tuples carrying their own stats and sketch (§3.5).
/// Past this, tuples fold into an all-overflow tuple.
pub const MAX_GROUP_TUPLES: usize = 64;

/// Default per-collector sample budget (§3.4).
pub const DEFAULT_MAX_SAMPLE_BYTES: usize = 64 * 1024 * 1024;

/// A collector's definition. Immutable for the collector's lifetime — every
/// structural edit is a reset plus a new definition (§7.1), which is what lets
/// a snapshot record the definition it was taken under.
#[derive(Debug, Clone)]
pub struct CollectorDef {
    pub name: String,
    pub filter_string: String,
    pub filter: ParsedFilter,
    pub level: Level,
    pub group_keys: Vec<String>,
    pub max_sample_bytes: usize,
    pub description: Option<String>,
}

struct Inner {
    total: ExactStats,
    per_name: HashMap<u32, ExactStats>,
    per_group: HashMap<Vec<u32>, ExactStats>,
    names: Interner,
    group_values: Vec<Interner>,
    samples: SampleTier,
    armed_at: DateTime<Utc>,
    zeroed_at: Option<DateTime<Utc>>,
    group_tuples_capped: bool,
}

impl Inner {
    fn new(def: &CollectorDef, now: DateTime<Utc>) -> Self {
        Self {
            total: ExactStats::new(),
            per_name: HashMap::new(),
            per_group: HashMap::new(),
            names: Interner::new(DEFAULT_NAME_CAP),
            group_values: def
                .group_keys
                .iter()
                .map(|_| Interner::new(DEFAULT_GROUP_VALUE_CAP))
                .collect(),
            samples: SampleTier::new(def.level, def.group_keys.len(), def.max_sample_bytes),
            armed_at: now,
            zeroed_at: None,
            group_tuples_capped: false,
        }
    }
}

/// A consistent view, taken under the lock and projected outside it.
pub struct CollectorSnapshot {
    pub def: Arc<CollectorDef>,
    pub total: ExactStats,
    pub per_name: HashMap<u32, ExactStats>,
    pub per_group: HashMap<Vec<u32>, ExactStats>,
    pub names: Interner,
    pub group_values: Vec<Interner>,
    pub samples: SampleSnapshot,
    pub armed_at: DateTime<Utc>,
    pub zeroed_at: Option<DateTime<Utc>>,
    pub group_tuples_capped: bool,
}

impl CollectorSnapshot {
    /// The window the data actually covers. Measured from the later of arming
    /// and the last zeroing, so a window whose data was discarded by a reset
    /// does not read as one that collected throughout (§5.1).
    pub fn window_start(&self) -> DateTime<Utc> {
        match self.zeroed_at {
            Some(z) if z > self.armed_at => z,
            _ => self.armed_at,
        }
    }

    /// Whether any cardinality cap folded values into `__overflow__`.
    pub fn cardinality_capped(&self) -> bool {
        self.names.is_capped()
            || self.group_tuples_capped
            || self.group_values.iter().any(|i| i.is_capped())
    }
}

pub struct Collector {
    def: Arc<CollectorDef>,
    inner: RwLock<Inner>,
}

impl Collector {
    pub fn new(def: CollectorDef, now: DateTime<Utc>) -> Self {
        let inner = Inner::new(&def, now);
        Self {
            def: Arc::new(def),
            inner: RwLock::new(inner),
        }
    }

    pub fn def(&self) -> &Arc<CollectorDef> {
        &self.def
    }

    /// Fold one matched span in. Takes the lock once and updates every
    /// structure inside it.
    ///
    /// The caller has already established that the span matches — matching is
    /// done against the pre-parsed filter outside this call, so the lock is
    /// held only for the update.
    pub fn ingest(&self, span: &SpanEntry) {
        // Classified once, then applied to every aggregate. Doing it per
        // aggregate would be both wasteful and a chance for the three to
        // disagree about the same span.
        let d = classify(span);
        let err = is_error(span);

        let mut g = self.inner.write().expect("collector lock poisoned");

        g.total.record(d, err);

        let name_id = g.names.intern(&span.name);
        g.per_name.entry(name_id).or_default().record(d, err);

        let mut group_ids: Vec<u32> = Vec::new();
        if !self.def.group_keys.is_empty() {
            group_ids.reserve_exact(self.def.group_keys.len());
            for (i, key) in self.def.group_keys.iter().enumerate() {
                let id = match span.attributes.get(key) {
                    None => ABSENT_ID,
                    Some(v) => match render_attribute(v) {
                        None => ABSENT_ID,
                        Some(s) => g.group_values[i].intern(&s),
                    },
                };
                group_ids.push(id);
            }
            let known = g.per_group.contains_key(&group_ids);
            if !known && g.per_group.len() >= MAX_GROUP_TUPLES {
                g.group_tuples_capped = true;
                group_ids = vec![OVERFLOW_ID; self.def.group_keys.len()];
            }
            g.per_group
                .entry(group_ids.clone())
                .or_default()
                .record(d, err);
        }

        // The sample tier only wants well-formed timings; a malformed or
        // negative duration would poison every interval projection built on
        // it. It is still counted in the exact tier above.
        if let Duration::Ok(_) = d {
            let (Some(start), Some(end)) = (
                span.start_time.timestamp_nanos_opt(),
                span.end_time.timestamp_nanos_opt(),
            ) else {
                return;
            };
            let rec = SampleRecord {
                start_ns: start,
                end_ns: end,
                name_id,
                flags: pack_flags(&span.status, &span.kind),
                span_id: span.span_id,
                parent_span_id: span.parent_span_id.unwrap_or(0),
                trace_id: span.trace_id,
            };
            g.samples.push(&rec, &group_ids);
        }
    }

    /// Clone a consistent view without disturbing collection.
    pub fn snapshot(&self) -> CollectorSnapshot {
        let g = self.inner.read().expect("collector lock poisoned");
        CollectorSnapshot {
            def: self.def.clone(),
            total: g.total.clone(),
            per_name: g.per_name.clone(),
            per_group: g.per_group.clone(),
            names: g.names.clone(),
            group_values: g.group_values.clone(),
            samples: g.samples.snapshot(),
            armed_at: g.armed_at,
            zeroed_at: g.zeroed_at,
            group_tuples_capped: g.group_tuples_capped,
        }
    }

    /// Take everything and start clean, returning what was taken.
    ///
    /// Every structure is replaced together, under the one lock, so the
    /// returned view and the fresh state can never overlap or lose a span —
    /// the property A11 asserts field by field.
    pub fn swap(&self, now: DateTime<Utc>) -> CollectorSnapshot {
        let mut g = self.inner.write().expect("collector lock poisoned");
        let armed_at = g.armed_at;
        let mut fresh = Inner::new(&self.def, armed_at);
        fresh.zeroed_at = Some(now);
        let old = std::mem::replace(&mut *g, fresh);
        CollectorSnapshot {
            def: self.def.clone(),
            total: old.total,
            per_name: old.per_name,
            per_group: old.per_group,
            names: old.names,
            group_values: old.group_values,
            samples: old.samples.snapshot(),
            armed_at: old.armed_at,
            zeroed_at: old.zeroed_at,
            group_tuples_capped: old.group_tuples_capped,
        }
    }
}

/// Render a span attribute for interning (§3.3).
///
/// Deliberately independent of `matches_span`, whose attribute arm is
/// `.and_then(|v| v.as_str())` — so a boolean `cache.enabled`, the driving
/// example's most likely encoding, is invisible to it. Reusing that view would
/// silently collapse a two-arm A/B into one group.
pub fn render_attribute(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        // Arrays and objects are excluded rather than stringified: a
        // group key whose value is a structure is not a dimension.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::parser::parse_filter;
    use crate::span::types::{SpanKind, SpanStatus};
    use chrono::TimeZone;

    const S: i64 = 1_700_000_000_000_000_000;

    fn def(level: Level, group_keys: &[&str]) -> CollectorDef {
        CollectorDef {
            name: "c".into(),
            filter_string: "sv=svc".into(),
            filter: parse_filter("sv=svc").unwrap(),
            level,
            group_keys: group_keys.iter().map(|s| s.to_string()).collect(),
            max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
            description: None,
        }
    }

    fn span(name: &str, start: i64, end: i64) -> SpanEntry {
        SpanEntry {
            seq: 0,
            trace_id: 7,
            span_id: 9,
            parent_span_id: None,
            start_time: Utc.timestamp_nanos(start),
            end_time: Utc.timestamp_nanos(end),
            duration_ms: 0.0,
            name: name.into(),
            kind: SpanKind::Internal,
            service_name: "svc".into(),
            status: SpanStatus::Ok,
            attributes: HashMap::new(),
            events: vec![],
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.timestamp_nanos(S)
    }

    #[test]
    fn ingest_updates_every_tier_consistently() {
        let c = Collector::new(def(Level::Tree, &[]), now());
        c.ingest(&span("a", S, S + 1_000));
        c.ingest(&span("b", S, S + 3_000));

        let s = c.snapshot();
        assert_eq!(s.total.count, 2);
        assert_eq!(s.total.total_ns, 4_000);
        assert_eq!(s.per_name.len(), 2, "one entry per distinct span name");
        assert_eq!(
            s.samples.len(),
            2,
            "the sample tier agrees with the exact tier"
        );
    }

    #[test]
    fn a_malformed_span_counts_but_is_kept_out_of_the_sample_tier() {
        // It must reach `count` (it matched) and stay out of every interval
        // projection (its timestamps are not usable).
        let c = Collector::new(def(Level::Tree, &[]), now());
        c.ingest(&span("ok", S, S + 1_000));
        c.ingest(&span("bad", 0, S)); // epoch-zero start

        let s = c.snapshot();
        assert_eq!(s.total.count, 2, "both matched");
        assert_eq!(s.total.malformed_timestamps, 1);
        assert_eq!(s.total.total_ns, 1_000, "the malformed one is not summed");
        assert_eq!(s.samples.len(), 1, "nor retained as a sample");
    }

    #[test]
    fn group_keys_render_non_string_attributes() {
        // The driving A/B: a kill-switch emitted as an OTLP BoolValue. The
        // matcher's `.as_str()` view cannot see it, which would collapse both
        // arms into one group.
        let c = Collector::new(def(Level::Timing, &["cache.enabled"]), now());
        let mut on = span("x", S, S + 10);
        on.attributes
            .insert("cache.enabled".into(), serde_json::json!(true));
        let mut off = span("x", S, S + 20);
        off.attributes
            .insert("cache.enabled".into(), serde_json::json!(false));
        c.ingest(&on);
        c.ingest(&off);

        let s = c.snapshot();
        assert_eq!(s.per_group.len(), 2, "true and false are distinct groups");
        let labels: Vec<&str> = s
            .per_group
            .keys()
            .map(|k| s.group_values[0].resolve(k[0]))
            .collect();
        assert!(labels.contains(&"true") && labels.contains(&"false"));
    }

    #[test]
    fn a_span_missing_the_group_key_lands_in_absent_not_nowhere() {
        let c = Collector::new(def(Level::Timing, &["cache.enabled"]), now());
        c.ingest(&span("x", S, S + 10));

        let s = c.snapshot();
        assert_eq!(s.per_group.len(), 1);
        let key = s.per_group.keys().next().unwrap();
        assert_eq!(key[0], ABSENT_ID);
        assert_eq!(
            s.per_group.values().map(|e| e.count).sum::<u64>(),
            s.total.count,
            "the group breakdown must account for every matched span"
        );
    }

    #[test]
    fn swap_takes_everything_and_loses_nothing() {
        // A11's invariant, field by field: what the swap returned plus what
        // remains equals everything ingested.
        let c = Collector::new(def(Level::Tree, &[]), now());
        for i in 0..5 {
            c.ingest(&span("a", S, S + 1_000 + i));
        }
        let taken = c.swap(now());
        for i in 0..3 {
            c.ingest(&span("a", S, S + 2_000 + i));
        }
        let live = c.snapshot();

        assert_eq!(taken.total.count + live.total.count, 8);
        assert_eq!(
            taken.samples.len() + live.samples.len(),
            8,
            "no sample is lost or duplicated across the swap"
        );
        assert_eq!(
            taken.total.total_ns + live.total.total_ns,
            (0..5).map(|i| 1_000 + i).sum::<i128>() + (0..3).map(|i| 2_000 + i).sum::<i128>()
        );
    }

    #[test]
    fn swap_resets_the_window_but_keeps_armed_at() {
        let armed = now();
        let c = Collector::new(def(Level::Timing, &[]), armed);
        c.ingest(&span("a", S, S + 10));
        let later = Utc.timestamp_nanos(S + 60_000_000_000);
        c.swap(later);

        let live = c.snapshot();
        assert_eq!(live.armed_at, armed, "arming time is history, not state");
        assert_eq!(live.zeroed_at, Some(later));
        assert_eq!(
            live.window_start(),
            later,
            "the live window starts at the zeroing, so wall time is not overstated"
        );
    }

    #[test]
    fn a_snapshot_is_isolated_from_later_ingestion() {
        let c = Collector::new(def(Level::Tree, &[]), now());
        c.ingest(&span("a", S, S + 10));
        let s = c.snapshot();
        c.ingest(&span("a", S, S + 20));
        assert_eq!(
            s.total.count, 1,
            "the projection sees a fixed point in time"
        );
        assert_eq!(c.snapshot().total.count, 2);
    }

    #[test]
    fn per_name_stats_are_capped_and_the_cap_is_reported() {
        let c = Collector::new(def(Level::Timing, &[]), now());
        for i in 0..(DEFAULT_NAME_CAP + 50) {
            c.ingest(&span(&format!("name{i}"), S, S + 10));
        }
        let s = c.snapshot();
        assert!(s.names.is_capped());
        assert!(s.cardinality_capped(), "the result must say so");
        assert_eq!(
            s.per_name.len(),
            DEFAULT_NAME_CAP + 1,
            "capped names share one overflow bucket"
        );
        assert_eq!(
            s.per_name.values().map(|e| e.count).sum::<u64>(),
            s.total.count,
            "even capped, the breakdown accounts for every span"
        );
    }

    #[test]
    fn render_attribute_covers_the_scalar_json_types() {
        assert_eq!(render_attribute(&serde_json::json!("s")), Some("s".into()));
        assert_eq!(
            render_attribute(&serde_json::json!(true)),
            Some("true".into())
        );
        assert_eq!(render_attribute(&serde_json::json!(42)), Some("42".into()));
        assert_eq!(render_attribute(&serde_json::json!([1, 2])), None);
        assert_eq!(render_attribute(&serde_json::json!({"a": 1})), None);
    }
}
