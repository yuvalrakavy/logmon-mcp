//! Domains — Model A: a domain is a *full instance* of the per-broker
//! machinery.
//!
//! Each [`Domain`] owns its own seq-referencing state (log pipeline + span
//! store + bookmark namespace + receiver metrics), all fed by a single
//! per-domain [`SeqCounter`]. Domains share nothing with each other, so a
//! record ingested on one domain's port is invisible to consumers bound to
//! another — cross-domain queries are impossible by construction.
//!
//! Sessions (the config scope: triggers, filters, notification queue) are
//! *global* and merely **bind** to a domain; only *data* is partitioned. See
//! [`crate::daemon::session::SessionState::domain`] for the binding.
//!
//! The `default` domain is always present and is the `N=1` back-compat anchor:
//! with no `domains.create`, everything runs on `default` and behaves exactly
//! as the pre-domains broker did.

use crate::engine::pipeline::LogPipeline;
use crate::engine::seq_counter::SeqCounter;
use crate::receiver::gelf::GelfReceiver;
use crate::receiver::otlp::OtlpReceiver;
use crate::receiver::ReceiverMetrics;
use crate::span::store::SpanStore;
use crate::store::bookmarks::BookmarkStore;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::task::JoinHandle;

/// Validated domain identifier — the registry key. Same charset as session
/// names (`[A-Za-z0-9_-]`, non-empty; see [`super::session::is_valid_name`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DomainId(String);

impl DomainId {
    /// The name of the always-present default domain.
    pub const DEFAULT: &'static str = "default";

    /// Construct a validated domain id, rejecting empty / out-of-charset names.
    pub fn new(name: &str) -> Result<Self, DomainError> {
        if !super::session::is_valid_name(name) {
            return Err(DomainError::InvalidName(name.to_string()));
        }
        Ok(Self(name.to_string()))
    }

    /// The always-present `default` domain id (the `N=1` anchor).
    pub fn default_domain() -> Self {
        Self(Self::DEFAULT.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for the `default` domain — the one `domains.delete` refuses and the
    /// one every session binds to until it calls `use_domain`.
    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }
}

impl std::fmt::Display for DomainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("invalid domain name: {0}")]
    InvalidName(String),
}

/// Where a domain came from — surfaced as `domains.list`'s `source` field and
/// used by `domains.delete` (which refuses `Config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainSource {
    /// Declared in `config.json`'s `domains` array (incl. the implicit
    /// `default`). User-authored; `domains.delete` refuses these.
    Config,
    /// Created via `domains.create(..., persist=true)` — recorded in the
    /// machine-owned domains store so it is re-created on the next boot.
    Persistent,
    /// Created via `domains.create` (default `persist=false`). Gone on restart.
    Ephemeral,
}

impl DomainSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            DomainSource::Config => "config",
            DomainSource::Persistent => "persistent",
            DomainSource::Ephemeral => "ephemeral",
        }
    }
}

/// Static description of a domain: identity, ingest ports, buffer sizes, and
/// provenance. The port fields hold the ports the domain's receivers actually
/// bound (resolved at create time); `0` means that receiver is disabled. For
/// `default` these mirror the daemon-level config values.
#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub name: DomainId,
    /// GELF UDP+TCP port (`0` = GELF disabled for this domain).
    pub gelf_port: u16,
    /// OTLP gRPC port (`0` = disabled).
    pub otlp_grpc_port: u16,
    /// OTLP HTTP port (`0` = disabled).
    pub otlp_http_port: u16,
    pub log_buffer_size: usize,
    pub span_buffer_size: usize,
    pub source: DomainSource,
}

/// A fully isolated instance of the per-broker machinery (Model A).
///
/// All fields are `Arc` so the inner store-level code is shared with the
/// per-domain processors and query handlers exactly as the pre-domains broker
/// shared its single global set — the store internals are byte-for-byte
/// unchanged; only *which* instance a query resolves to differs.
pub struct Domain {
    pub config: DomainConfig,
    pub pipeline: Arc<LogPipeline>,
    pub span_store: Arc<SpanStore>,
    pub bookmarks: Arc<BookmarkStore>,
    pub metrics: Arc<ReceiverMetrics>,
    /// Live receivers + processor tasks for a `domains.create`d domain. `None`
    /// for `default` (fed by the daemon-level receivers in `run_with_overrides`)
    /// and for test-constructed domains. Held here so that when the last
    /// `Arc<Domain>` drops — the domain was deleted AND no in-flight query still
    /// holds it — the listeners stop and the processors are aborted. This is the
    /// delete-while-bound Arc-graceful teardown.
    pub receivers: Option<DomainReceivers>,
}

impl Domain {
    /// Assemble a domain from already-constructed parts. Used for `default`,
    /// whose seq counter / pipeline / bookmarks are seeded from `state.json`
    /// (and used to restore named sessions) before the `Domain` wrapper exists.
    /// See [`crate::daemon::server::run_with_overrides`]. `receivers` is `None`
    /// (default's receivers are daemon-level); the create path sets it after.
    pub fn from_parts(
        config: DomainConfig,
        pipeline: Arc<LogPipeline>,
        span_store: Arc<SpanStore>,
        bookmarks: Arc<BookmarkStore>,
        metrics: Arc<ReceiverMetrics>,
    ) -> Self {
        Self {
            config,
            pipeline,
            span_store,
            bookmarks,
            metrics,
            receivers: None,
        }
    }

    /// Build a brand-new domain with freshly-allocated, independent machinery:
    /// its own seq space, log + span buffers, bookmark namespace, and receiver
    /// metrics. Used by tests. `initial_seq` seeds the domain's seq counter
    /// (`0` for a fresh ephemeral domain).
    pub fn new(config: DomainConfig, initial_seq: u64) -> Self {
        Self::new_with_metrics(config, initial_seq, Arc::new(ReceiverMetrics::new()))
    }

    /// Like [`Self::new`] but reuses a caller-provided `ReceiverMetrics`. The
    /// create path uses this so a live domain's receivers and its query surface
    /// (`status.get` drop counts) share ONE metrics instance.
    pub fn new_with_metrics(
        config: DomainConfig,
        initial_seq: u64,
        metrics: Arc<ReceiverMetrics>,
    ) -> Self {
        let seq_counter = Arc::new(SeqCounter::new_with_initial(initial_seq));
        let pipeline = Arc::new(LogPipeline::new_with_seq_counter(
            config.log_buffer_size,
            seq_counter.clone(),
        ));
        let span_store = Arc::new(SpanStore::new(config.span_buffer_size, seq_counter));
        let bookmarks = Arc::new(BookmarkStore::new());
        Self::from_parts(config, pipeline, span_store, bookmarks, metrics)
    }

    /// This domain's id (its config name).
    pub fn id(&self) -> &DomainId {
        &self.config.name
    }
}

/// Live receivers + processor tasks feeding a `domains.create`d domain (see
/// [`Domain::receivers`]). Dropping this stops the GELF/OTLP listeners (via
/// their own `Drop`) and aborts the domain's log + span processor tasks, so a
/// deleted domain leaves nothing running.
pub struct DomainReceivers {
    /// Held for its `Drop` (stops the UDP/TCP listeners). `None` if GELF was
    /// disabled (`gelf_port = 0`) for this domain.
    #[allow(dead_code)]
    gelf: Option<GelfReceiver>,
    /// Held for its `Drop` (signals gRPC/HTTP shutdown + aborts their tasks).
    /// `None` if OTLP was disabled for this domain.
    #[allow(dead_code)]
    otlp: Option<OtlpReceiver>,
    log_processor: JoinHandle<()>,
    span_processor: JoinHandle<()>,
}

impl DomainReceivers {
    pub fn new(
        gelf: Option<GelfReceiver>,
        otlp: Option<OtlpReceiver>,
        log_processor: JoinHandle<()>,
        span_processor: JoinHandle<()>,
    ) -> Self {
        Self {
            gelf,
            otlp,
            log_processor,
            span_processor,
        }
    }
}

impl Drop for DomainReceivers {
    fn drop(&mut self) {
        // The GELF/OTLP receivers stop via their own `Drop`; abort the
        // processor tasks so a deleted domain leaves nothing running.
        self.log_processor.abort();
        self.span_processor.abort();
    }
}

/// The set of live domains. `get` clones the `Arc<Domain>` OUT of the read
/// guard so a query never holds the registry lock while it runs — which is
/// what makes delete-while-bound graceful: an in-flight query keeps its clone
/// alive after the domain is removed from the map.
pub struct DomainRegistry {
    domains: RwLock<HashMap<DomainId, Arc<Domain>>>,
}

impl DomainRegistry {
    pub fn new() -> Self {
        Self {
            domains: RwLock::new(HashMap::new()),
        }
    }

    /// Resolve a domain by id, cloning its `Arc` out of the guard.
    pub fn get(&self, id: &DomainId) -> Option<Arc<Domain>> {
        self.domains
            .read()
            .expect("domains lock poisoned")
            .get(id)
            .cloned()
    }

    /// Insert (or replace) a domain, keyed by `domain.config.name`.
    pub fn insert(&self, domain: Arc<Domain>) {
        let id = domain.config.name.clone();
        self.domains
            .write()
            .expect("domains lock poisoned")
            .insert(id, domain);
    }

    /// Remove a domain, returning its `Arc` if it was present. Any in-flight
    /// query that already resolved this domain keeps running against its clone.
    pub fn remove(&self, id: &DomainId) -> Option<Arc<Domain>> {
        self.domains
            .write()
            .expect("domains lock poisoned")
            .remove(id)
    }

    pub fn contains(&self, id: &DomainId) -> bool {
        self.domains
            .read()
            .expect("domains lock poisoned")
            .contains_key(id)
    }

    /// A snapshot of all live domains (Arc clones). Order is unspecified.
    pub fn list(&self) -> Vec<Arc<Domain>> {
        self.domains
            .read()
            .expect("domains lock poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.domains.read().expect("domains lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for DomainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> DomainConfig {
        DomainConfig {
            name: DomainId::new(name).unwrap(),
            gelf_port: 0,
            otlp_grpc_port: 0,
            otlp_http_port: 0,
            log_buffer_size: 100,
            span_buffer_size: 100,
            source: DomainSource::Ephemeral,
        }
    }

    #[test]
    fn domain_id_validates_charset() {
        assert!(DomainId::new("t3").is_ok());
        assert!(DomainId::new("production").is_ok());
        assert!(DomainId::new("a_b-1").is_ok());
        assert!(DomainId::new("").is_err());
        assert!(DomainId::new("bad name").is_err());
        assert!(DomainId::new("bad/slash").is_err());
    }

    #[test]
    fn default_domain_is_default() {
        let d = DomainId::default_domain();
        assert_eq!(d.as_str(), "default");
        assert!(d.is_default());
        assert!(!DomainId::new("t3").unwrap().is_default());
    }

    #[test]
    fn registry_insert_get_remove() {
        let reg = DomainRegistry::new();
        assert!(reg.is_empty());

        let id = DomainId::new("t3").unwrap();
        reg.insert(Arc::new(Domain::new(cfg("t3"), 0)));
        assert_eq!(reg.len(), 1);
        assert!(reg.contains(&id));

        let got = reg.get(&id).expect("t3 present");
        assert_eq!(got.id().as_str(), "t3");

        assert!(reg.get(&DomainId::new("nope").unwrap()).is_none());

        let removed = reg.remove(&id).expect("removed t3");
        assert_eq!(removed.id().as_str(), "t3");
        assert!(reg.get(&id).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn get_clones_out_of_guard_survives_remove() {
        // The delete-while-bound contract: a resolved Arc stays usable after
        // the domain is removed from the registry.
        let reg = DomainRegistry::new();
        let id = DomainId::new("t3").unwrap();
        reg.insert(Arc::new(Domain::new(cfg("t3"), 0)));

        let held = reg.get(&id).expect("t3 present");
        reg.remove(&id);
        assert!(reg.get(&id).is_none());
        // The previously-resolved handle is still valid — its pipeline answers.
        assert_eq!(held.id().as_str(), "t3");
        assert_eq!(held.pipeline.store_len(), 0);
    }

    #[test]
    fn each_domain_has_independent_seq_and_store() {
        // Two fresh domains do not share seq space or buffers.
        let a = Domain::new(cfg("a"), 0);
        let b = Domain::new(cfg("b"), 0);
        let s1 = a.pipeline.assign_seq();
        let s2 = a.pipeline.assign_seq();
        assert_eq!((s1, s2), (1, 2));
        // b's counter is independent — its first seq is also 1, not 3.
        assert_eq!(b.pipeline.assign_seq(), 1);
    }
}
