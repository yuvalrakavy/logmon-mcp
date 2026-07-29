//! The collector registry — spec §4.4, §3.4.
//!
//! **Domain-keyed, independent of `SessionState::domain`.** If collectors were
//! reached the way triggers are — via `active_session_ids_for_domain` — then a
//! `use_domain` call mid-run would silently stop a collector while it still
//! reported its pinned domain: exactly the failure pinning exists to prevent.
//!
//! A collector is owned by a session for lifecycle, and pinned to the domain it
//! was created in for ingest. Those are different questions and the registry
//! keeps them apart.

use crate::collector::state::{Collector, CollectorDef, CollectorSnapshot};
use crate::daemon::domain::DomainId;
use crate::daemon::session::SessionId;
use crate::filter::matcher::matches_span;
use crate::receiver::{ReceiverMetrics, TraceIngestLoss};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use std::sync::{Arc, RwLock};

/// Daemon-wide sample reservation (§3.4). Enforced at arm time, so four
/// default-sized collectors is the practical ceiling across every session and
/// domain — and `add` says so when it refuses.
pub const DEFAULT_MAX_TOTAL_SAMPLE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum RegistryError {
    DuplicateName(String),
    NotFound(String),
    /// Arming would exceed the daemon-wide reservation. Carries the numbers so
    /// the caller can say what would fit rather than only that it did not.
    BudgetExceeded {
        requested: usize,
        remaining: usize,
        total: usize,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::DuplicateName(n) => {
                write!(f, "a collector named `{n}` already exists in this session")
            }
            RegistryError::NotFound(n) => write!(f, "no collector named `{n}` in this session"),
            RegistryError::BudgetExceeded {
                requested,
                remaining,
                total,
            } => write!(
                f,
                "arming would reserve {requested} bytes but only {remaining} of the \
                 daemon-wide {total} remain; reduce max_sample_bytes, lower the level, \
                 or remove another collector"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

struct Entry {
    owner: SessionId,
    domain: DomainId,
    collector: Arc<Collector>,
    /// The pinned domain's counters, held from arm time so a later read can
    /// prove the baseline and the current reading share an origin.
    metrics: Arc<ReceiverMetrics>,
    /// Span loss at arm time (or at the last reset). Lives here rather than on
    /// the collector because it is zeroed under the *registry's* write lock,
    /// which is the lock that excludes ingest — so a reset moves the data and
    /// the baseline together, with nothing able to slip between them (A13).
    ingest_baseline: TraceIngestLoss,
}

/// A collector plus everything the read path needs to interpret it.
pub struct ArmedCollector {
    pub collector: Arc<Collector>,
    /// The domain the collector was pinned to at arm time. A `use_domain` by
    /// the owning session does not move it.
    pub domain: DomainId,
    pub metrics: Arc<ReceiverMetrics>,
    pub ingest_baseline: TraceIngestLoss,
}

impl Entry {
    fn armed(&self) -> ArmedCollector {
        ArmedCollector {
            collector: self.collector.clone(),
            domain: self.domain.clone(),
            metrics: self.metrics.clone(),
            ingest_baseline: self.ingest_baseline,
        }
    }
}

pub struct CollectorRegistry {
    /// One lock. The population is bounded by the daemon reservation (four
    /// default-sized collectors), so a linear scan on the ingest path is
    /// cheaper than the hashing and allocation an index would cost.
    entries: RwLock<Vec<Entry>>,
    max_total_sample_bytes: usize,
}

impl CollectorRegistry {
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_MAX_TOTAL_SAMPLE_BYTES)
    }

    pub fn with_budget(max_total_sample_bytes: usize) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            max_total_sample_bytes,
        }
    }

    pub fn add(
        &self,
        owner: &SessionId,
        domain: &DomainId,
        metrics: Arc<ReceiverMetrics>,
        def: CollectorDef,
        now: DateTime<Utc>,
    ) -> Result<Arc<Collector>, RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        if g.iter()
            .any(|e| &e.owner == owner && e.collector.def().name == def.name)
        {
            return Err(RegistryError::DuplicateName(def.name));
        }

        let reserved: usize = g.iter().map(|e| e.collector.def().max_sample_bytes).sum();
        let remaining = self.max_total_sample_bytes.saturating_sub(reserved);
        if def.max_sample_bytes > remaining {
            return Err(RegistryError::BudgetExceeded {
                requested: def.max_sample_bytes,
                remaining,
                total: self.max_total_sample_bytes,
            });
        }

        let collector = Arc::new(Collector::new(def, now));
        g.push(Entry {
            owner: owner.clone(),
            domain: domain.clone(),
            collector: collector.clone(),
            ingest_baseline: metrics.trace_ingest_loss(),
            metrics,
        });
        Ok(collector)
    }

    pub fn get(&self, owner: &SessionId, name: &str) -> Option<ArmedCollector> {
        let g = self.entries.read().expect("registry lock poisoned");
        g.iter()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .map(Entry::armed)
    }

    pub fn list(&self, owner: &SessionId) -> Vec<ArmedCollector> {
        let g = self.entries.read().expect("registry lock poisoned");
        g.iter()
            .filter(|e| &e.owner == owner)
            .map(Entry::armed)
            .collect()
    }

    /// Discard a collector's data and start a fresh window.
    ///
    /// Under the registry's **write** lock, which is the lock `ingest_span`
    /// takes for reading — so the data, the window, and the ingest baseline all
    /// move together and no span can land between the swap and the re-baseline.
    /// Returns what was discarded, so a caller that wanted the run can still
    /// have it.
    pub fn reset(
        &self,
        owner: &SessionId,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<CollectorSnapshot, RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let e = g
            .iter_mut()
            .find(|e| &e.owner == owner && e.collector.def().name == name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let taken = e.collector.swap(now);
        e.ingest_baseline = e.metrics.trace_ingest_loss();
        Ok(taken)
    }

    pub fn remove(&self, owner: &SessionId, name: &str) -> Result<(), RegistryError> {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let before = g.len();
        g.retain(|e| !(&e.owner == owner && e.collector.def().name == name));
        if g.len() == before {
            return Err(RegistryError::NotFound(name.to_string()));
        }
        Ok(())
    }

    /// Drop every collector owned by a session — the lifecycle counterpart of
    /// session disposal.
    pub fn drop_session(&self, owner: &SessionId) -> usize {
        let mut g = self.entries.write().expect("registry lock poisoned");
        let before = g.len();
        g.retain(|e| &e.owner != owner);
        before - g.len()
    }

    /// Bytes reserved across every armed collector.
    pub fn reserved_bytes(&self) -> usize {
        self.entries
            .read()
            .expect("registry lock poisoned")
            .iter()
            .map(|e| e.collector.def().max_sample_bytes)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .expect("registry lock poisoned")
            .is_empty()
    }

    /// The ingest path. Offers one span to every collector pinned to `domain`
    /// whose filter matches it.
    ///
    /// Matching uses the collector's **pre-parsed** filter, so no parsing
    /// happens per span — the defect that cost 28 µs per span per session on
    /// the trigger path.
    pub fn ingest_span(&self, domain: &DomainId, span: &SpanEntry) {
        let g = self.entries.read().expect("registry lock poisoned");
        if g.is_empty() {
            return;
        }
        for e in g.iter() {
            if &e.domain != domain {
                continue;
            }
            if matches_span(&e.collector.def().filter, span) {
                e.collector.ingest(span);
            }
        }
    }
}

impl Default for CollectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::sample::Level;
    use crate::filter::parser::parse_filter;
    use crate::span::types::{SpanKind, SpanStatus};
    use chrono::TimeZone;
    use std::collections::HashMap;

    const S: i64 = 1_700_000_000_000_000_000;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_nanos(S)
    }

    fn metrics() -> Arc<ReceiverMetrics> {
        Arc::new(ReceiverMetrics::new())
    }

    fn def(name: &str, filter: &str, bytes: usize) -> CollectorDef {
        CollectorDef {
            name: name.into(),
            filter_string: filter.into(),
            filter: parse_filter(filter).expect("valid"),
            level: Level::Tree,
            group_keys: vec![],
            max_sample_bytes: bytes,
            description: None,
        }
    }

    fn span(service: &str) -> SpanEntry {
        SpanEntry {
            seq: 0,
            trace_id: 1,
            span_id: 2,
            parent_span_id: None,
            start_time: Utc.timestamp_nanos(S),
            end_time: Utc.timestamp_nanos(S + 1_000),
            duration_ms: 0.001,
            name: "op".into(),
            kind: SpanKind::Internal,
            service_name: service.into(),
            status: SpanStatus::Ok,
            attributes: HashMap::new(),
            events: vec![],
        }
    }

    fn sid(n: &str) -> SessionId {
        SessionId::Named(n.to_string())
    }

    fn dom(n: &str) -> DomainId {
        DomainId::new(n).expect("valid domain name")
    }

    #[test]
    fn only_matching_spans_in_the_pinned_domain_are_collected() {
        let r = CollectorRegistry::new();
        let d_a = dom("a");
        let d_b = dom("b");
        let c = r
            .add(
                &sid("s1"),
                &d_a,
                metrics(),
                def("c", "sv=svc", 1 << 20),
                now(),
            )
            .expect("armed");

        r.ingest_span(&d_a, &span("svc")); // matches, right domain
        r.ingest_span(&d_a, &span("other")); // wrong service
        r.ingest_span(&d_b, &span("svc")); // right service, wrong domain

        assert_eq!(c.snapshot().total.count, 1);
    }

    #[test]
    fn a_collector_keeps_its_pinned_domain_regardless_of_its_owner() {
        // §4.4: the pin is the collector's, not the session's. A session that
        // later binds elsewhere must not silently stop its collector.
        let r = CollectorRegistry::new();
        let pinned = dom("t3");
        let c = r
            .add(
                &sid("s1"),
                &pinned,
                metrics(),
                def("c", "ALL", 1 << 20),
                now(),
            )
            .expect("armed");

        r.ingest_span(&pinned, &span("svc"));
        r.ingest_span(&DomainId::default_domain(), &span("svc"));

        assert_eq!(
            c.snapshot().total.count,
            1,
            "only the pinned domain feeds it"
        );
    }

    #[test]
    fn duplicate_names_are_rejected_per_session_not_globally() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .expect("first");
        assert_eq!(
            r.add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
                .expect_err("a duplicate name in one session is refused"),
            RegistryError::DuplicateName("c".into())
        );
        // A different session may reuse the name.
        assert!(r
            .add(&sid("s2"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .is_ok());
    }

    #[test]
    fn the_daemon_budget_is_a_reservation_checked_at_arm_time() {
        let r = CollectorRegistry::with_budget(100);
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("a", "ALL", 60), now())
            .expect("fits");
        let err = r
            .add(&sid("s1"), &d, metrics(), def("b", "ALL", 60), now())
            .expect_err("does not fit");
        assert_eq!(
            err,
            RegistryError::BudgetExceeded {
                requested: 60,
                remaining: 40,
                total: 100
            },
            "the error must say what would fit, not only that it did not"
        );
        // Removing frees the reservation.
        r.remove(&sid("s1"), "a").expect("removed");
        assert!(r
            .add(&sid("s1"), &d, metrics(), def("b", "ALL", 60), now())
            .is_ok());
    }

    #[test]
    fn removal_and_session_disposal_stop_collection() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        let c = r
            .add(&sid("s1"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .expect("armed");
        r.ingest_span(&d, &span("svc"));
        assert_eq!(c.snapshot().total.count, 1);

        r.remove(&sid("s1"), "c").expect("removed");
        r.ingest_span(&d, &span("svc"));
        assert_eq!(
            c.snapshot().total.count,
            1,
            "a removed collector receives nothing more"
        );
        assert_eq!(r.reserved_bytes(), 0, "and its reservation is released");

        r.add(&sid("s2"), &d, metrics(), def("x", "ALL", 1 << 20), now())
            .expect("armed");
        assert_eq!(r.drop_session(&sid("s2")), 1);
        assert!(r.is_empty());
    }

    #[test]
    fn removing_an_unknown_collector_is_an_error_not_a_silent_no_op() {
        let r = CollectorRegistry::new();
        assert_eq!(
            r.remove(&sid("s1"), "nope"),
            Err(RegistryError::NotFound("nope".into()))
        );
    }

    #[test]
    fn list_and_get_are_scoped_to_the_owning_session() {
        let r = CollectorRegistry::new();
        let d = dom("a");
        r.add(&sid("s1"), &d, metrics(), def("a", "ALL", 1 << 20), now())
            .unwrap();
        r.add(&sid("s1"), &d, metrics(), def("b", "ALL", 1 << 20), now())
            .unwrap();
        r.add(&sid("s2"), &d, metrics(), def("c", "ALL", 1 << 20), now())
            .unwrap();

        assert_eq!(r.list(&sid("s1")).len(), 2);
        assert_eq!(r.list(&sid("s2")).len(), 1);
        assert!(r.get(&sid("s1"), "c").is_none(), "no cross-session access");
        assert!(r.get(&sid("s2"), "c").is_some());
    }
}
