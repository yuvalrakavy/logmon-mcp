use crate::collector::history::{RunToRunFloor, SnapshotPolicy, StoredSnapshot};
use crate::collector::project::{GroupBy, ProfileOptions};
use crate::collector::registry::{
    ArmedCollector, CollectorEdit, CollectorRegistry, SnapshotRequest,
};
use crate::collector::sample::{Level, MAX_GROUP_KEYS};
use crate::collector::state::{Collector, CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
use crate::daemon::domain::{Domain, DomainId, DomainRegistry, DomainSource};
use crate::daemon::domain_lifecycle::{spawn_ephemeral_domain, DomainPortSpec};
use crate::daemon::log_processor::sync_pre_buffer_size_for_domain;
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::domain_data::ScopedData;
use crate::gelf::message::LogEntry;
use crate::span::types::SlowGroupBy;
use chrono::Utc;
use logmon_broker_protocol::*;
use serde_json::{json, Value};
use std::sync::Arc;

/// Domain-creation policy sourced from `DaemonConfig`: the `max_domains` cap and
/// the default buffer sizes applied when `domains.create` omits them.
#[derive(Debug, Clone, Copy)]
pub struct DomainPolicy {
    pub max_domains: usize,
    pub default_log_buffer_size: usize,
    pub default_span_buffer_size: usize,
    /// Idle-seconds threshold above which a domain is reported `stale` (#2).
    pub stale_after_secs: u64,
}

/// Project a live [`Domain`] into its wire [`DomainInfo`] (used by
/// `domains.create` and `domains.list`), including liveness (#2): the last
/// log/span received across the domain's listeners, idle-seconds, and staleness.
fn domain_to_info(d: &Domain, stale_after_secs: u64) -> DomainInfo {
    let liv = d.metrics.liveness();
    let to_dt = chrono::DateTime::<chrono::Utc>::from_timestamp_nanos;
    // Logs arrive on GELF (udp/tcp) + OTLP logs arms; spans on the OTLP trace arms.
    let last_log = [
        liv.gelf_udp,
        liv.gelf_tcp,
        liv.otlp_http_logs,
        liv.otlp_grpc_logs,
    ]
    .into_iter()
    .flatten()
    .max();
    let last_span = [liv.otlp_http_traces, liv.otlp_grpc_traces]
        .into_iter()
        .flatten()
        .max();
    let idle_secs = last_log.max(last_span).map(|n| {
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        (now.saturating_sub(n).max(0) / 1_000_000_000) as u64
    });
    DomainInfo {
        name: d.config.name.to_string(),
        gelf_port: d.config.gelf_port,
        otlp_grpc_port: d.config.otlp_grpc_port,
        otlp_http_port: d.config.otlp_http_port,
        source: d.config.source.as_str().to_string(),
        log_buffer_size: d.config.log_buffer_size,
        span_buffer_size: d.config.span_buffer_size,
        log_count: d.pipeline.store_len(),
        span_count: d.span_store.len(),
        oldest_seq: d.pipeline.oldest_log_seq(),
        newest_seq: d.pipeline.newest_log_seq(),
        last_log_received_at: last_log.map(to_dt),
        last_span_received_at: last_span.map(to_dt),
        idle_secs,
        stale: idle_secs.is_some_and(|i| i >= stale_after_secs),
        // Filled by `domains.list` from the session registry; empty elsewhere.
        bound_sessions: Vec::new(),
    }
}

/// A requested port matches an existing domain's bound port when it is
/// unspecified (`None`) or equal.
fn port_matches(existing: u16, requested: Option<u16>) -> bool {
    requested.is_none_or(|p| p == existing)
}

/// Idempotency for `domains.create`: an existing domain whose ports all match
/// (or were left unspecified) is a no-op returning its info; conflicting ports
/// are an error.
fn ensure_idempotent(
    existing: &Domain,
    requested: &DomainPortSpec,
    stale_after_secs: u64,
) -> Result<Value, String> {
    let matches = port_matches(existing.config.gelf_port, requested.gelf)
        && port_matches(existing.config.otlp_grpc_port, requested.otlp_grpc)
        && port_matches(existing.config.otlp_http_port, requested.otlp_http);
    if matches {
        serde_json::to_value(domain_to_info(existing, stale_after_secs)).map_err(|e| e.to_string())
    } else {
        Err(format!(
            "domain \"{}\" already exists with different ports",
            existing.id()
        ))
    }
}

pub struct RpcHandler {
    /// The set of live domains. Every query resolves its bound domain out of
    /// here at the RPC boundary (see [`Self::resolve_domain`]); the store-level
    /// code then runs against that domain's pipeline / span store / bookmarks
    /// unchanged.
    domains: Arc<DomainRegistry>,
    sessions: Arc<SessionRegistry>,
    /// Daemon-wide, domain-keyed collector registry (spec section 4.4).
    collectors: Arc<CollectorRegistry>,
    /// The per-domain provenance registry (case-documents spec §3). Optional so
    /// a test harness can build a handler without a config dir; when absent the
    /// `domain_data.*` methods say so rather than pretending an empty registry.
    domain_data: Option<Arc<crate::domain_data::DomainDataStore>>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
    domain_policy: DomainPolicy,
    /// Serializes `domains.create` end-to-end. The existence-check → port-bind →
    /// registry-insert sequence straddles an `.await` on this shared handler, so
    /// without this lock two concurrent creates for the same name both pass the
    /// `get()==None` check, both bind their own ports, and the second `insert`
    /// silently overwrites (and tears down) the first — handing that caller a
    /// dead port. Held across the whole create so check-and-commit is atomic. It
    /// also closes the `max_domains` overshoot (N racers all counting the same
    /// pre-insert total). Creation is a rare control-plane op, so serializing it
    /// costs nothing.
    create_lock: tokio::sync::Mutex<()>,
}

impl RpcHandler {
    pub fn new(
        domains: Arc<DomainRegistry>,
        sessions: Arc<SessionRegistry>,
        collectors: Arc<CollectorRegistry>,
        receivers_info: Vec<String>,
        domain_policy: DomainPolicy,
    ) -> Self {
        Self {
            domains,
            sessions,
            collectors,
            domain_data: None,
            start_time: std::time::Instant::now(),
            receivers_info,
            domain_policy,
            create_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Attach the provenance registry.
    ///
    /// Builder-style rather than a fifth constructor argument, so the four
    /// existing `RpcHandler::new` call sites — three of them tests — do not all
    /// have to learn about a config directory to keep compiling.
    pub fn with_domain_data(mut self, store: Arc<crate::domain_data::DomainDataStore>) -> Self {
        self.domain_data = Some(store);
        self
    }

    /// Resolve the [`Domain`] the session is currently bound to, cloning its
    /// `Arc` OUT of the registry guard (F6 contract) so query execution never
    /// holds the registry lock — that is what makes delete-while-bound
    /// graceful. Returns the reader-side vanished-domain error (no silent
    /// fallback to `default`, §5) if the binding points at a deleted domain.
    fn resolve_domain(&self, session_id: &SessionId) -> Result<Arc<Domain>, String> {
        let id = self.sessions.domain_of(session_id);
        self.domains
            .get(&id)
            .ok_or_else(|| format!("domain \"{id}\" no longer exists — use_domain to rebind"))
    }

    /// Handle an RPC request for a given session.
    pub fn handle(&self, session_id: &SessionId, request: &RpcRequest) -> RpcResponse {
        let result = match request.method.as_str() {
            "logs.recent" => self.handle_logs_recent(session_id, &request.params),
            "logs.context" => self.handle_logs_context(session_id, &request.params),
            "logs.export" => self.handle_logs_export(session_id, &request.params),
            "logs.clear" => self.handle_logs_clear(session_id),
            "status.get" => self.handle_status(session_id),
            "tools.manifest" => Self::handle_tools_manifest(),
            "filters.list" => self.handle_filters_list(session_id),
            "filters.add" => self.handle_filters_add(session_id, &request.params),
            "filters.edit" => self.handle_filters_edit(session_id, &request.params),
            "filters.remove" => self.handle_filters_remove(session_id, &request.params),
            "triggers.list" => self.handle_triggers_list(session_id),
            "triggers.add" => self.handle_triggers_add(session_id, &request.params),
            "triggers.edit" => self.handle_triggers_edit(session_id, &request.params),
            "triggers.remove" => self.handle_triggers_remove(session_id, &request.params),
            "sessions.list" => self.handle_sessions_list(),
            "sessions.drop" => self.handle_sessions_drop(&request.params),
            // `domains.create` is async (it binds ports) and is dispatched by
            // `handle_async`, never here. An explicit arm gives a clear error if a
            // future caller ever routes a create through the sync path by mistake,
            // rather than the generic "unknown method".
            "domains.create" => Err(
                "domains.create must be dispatched via handle_async (it binds ports asynchronously)"
                    .to_string(),
            ),
            "domains.delete" => self.handle_domains_delete(&request.params),
            "domains.list" => self.handle_domains_list(),
            "domains.use" => self.handle_domains_use(session_id, &request.params),
            "sessions.rename" => self.handle_sessions_rename(session_id, &request.params),
            "domains.clear" => self.handle_domains_clear(session_id),
            "domain_data.update" => self.handle_domain_data_update(session_id, &request.params),
            "domain_data.remove" => self.handle_domain_data_remove(session_id, &request.params),
            "domain_data.get" => self.handle_domain_data_get(session_id, &request.params),
            "traces.recent" => self.handle_traces_recent(session_id, &request.params),
            "traces.get" => self.handle_traces_get(session_id, &request.params),
            "traces.summary" => self.handle_traces_summary(session_id, &request.params),
            "traces.slow" => self.handle_traces_slow(session_id, &request.params),
            "traces.logs" => self.handle_traces_logs(session_id, &request.params),
            "spans.context" => self.handle_spans_context(session_id, &request.params),
            "spans.export" => self.handle_spans_export(session_id, &request.params),
            "cases.create" => self.handle_cases_create(session_id, &request.params),
            "bookmarks.add" => self.handle_bookmarks_add(session_id, &request.params),
            "bookmarks.list" => self.handle_bookmarks_list(session_id, &request.params),
            "bookmarks.remove" => self.handle_bookmarks_remove(session_id, &request.params),
            "bookmarks.clear" => self.handle_bookmarks_clear(session_id, &request.params),
            "collectors.add" => self.handle_collectors_add(session_id, &request.params),
            "collectors.list" => self.handle_collectors_list(session_id),
            "collectors.get" => self.handle_collectors_get(session_id, &request.params),
            "collectors.edit" => self.handle_collectors_edit(session_id, &request.params),
            "collectors.snapshot" => self.handle_collectors_snapshot(session_id, &request.params),
            "collectors.history" => self.handle_collectors_history(session_id, &request.params),
            "collectors.diff" => self.handle_collectors_diff(session_id, &request.params),
            "collectors.document" => {
                self.handle_collectors_document(session_id, &request.params)
            }
            "collectors.reset" => self.handle_collectors_reset(session_id, &request.params),
            "collectors.remove" => self.handle_collectors_remove(session_id, &request.params),
            "traces.profile" => self.handle_traces_profile(session_id, &request.params),
            _ => Err(format!("unknown method: {}", request.method)),
        };

        match result {
            Ok(value) => RpcResponse::success(request.id, value),
            Err(msg) => RpcResponse::error(request.id, -32601, &msg),
        }
    }

    /// Async dispatch entry used by the connection loop. `domains.create` must
    /// bind real ports before it can respond (async, §9.3), so it is handled
    /// here; every other method — including the synchronous `domains.delete` /
    /// `domains.list` — delegates to the synchronous [`Self::handle`].
    pub async fn handle_async(&self, session_id: &SessionId, request: &RpcRequest) -> RpcResponse {
        if request.method == "domains.create" {
            return match self.handle_domains_create(&request.params).await {
                Ok(value) => RpcResponse::success(request.id, value),
                Err(msg) => RpcResponse::error(request.id, -32601, &msg),
            };
        }
        self.handle(session_id, request)
    }

    pub fn build_session_start_result(&self, session_id: &SessionId) -> SessionStartResult {
        let info = self.sessions.get(session_id);
        // `buffer_size` reflects the connect-time domain's current store size.
        let buffer_size = self
            .domains
            .get(&self.sessions.domain_of(session_id))
            .map_or(0, |d| d.pipeline.store_len());
        SessionStartResult {
            session_id: session_id.to_string(),
            is_new: true, // caller can override for reconnects
            queued_notifications: info.as_ref().map_or(0, |i| i.queue_size),
            trigger_count: info.as_ref().map_or(0, |i| i.trigger_count),
            filter_count: info.as_ref().map_or(0, |i| i.filter_count),
            daemon_uptime_secs: self.start_time.elapsed().as_secs(),
            buffer_size,
            receivers: self.receivers_info.clone(),
            capabilities: vec![
                "bookmarks".into(),
                "oneshot_triggers".into(),
                "client_info".into(),
                "domains".into(),
            ],
        }
    }

    // -----------------------------------------------------------------------
    // domains.* (ephemeral lifecycle — Wave 2 stage 2.2b)
    // -----------------------------------------------------------------------

    /// Upper bound on a `domains.create` buffer size (log or span). The size
    /// flows unclamped into `VecDeque::reserve_exact` on the domain's first
    /// ingest, and `reserve_exact` is INFALLIBLE — an allocation failure aborts
    /// the whole process (every domain, every client), so a client-supplied size
    /// is bounded here. 10M entries is ~100× any realistic per-domain ring and
    /// still comfortably allocatable. (`default`'s config-supplied sizes never
    /// come through this path.)
    const MAX_DOMAIN_BUFFER_SIZE: usize = 10_000_000;

    /// Create (or idempotently ensure) an ephemeral domain. Binds its receivers
    /// synchronously so a port clash is a clean error; refuses once
    /// `max_domains` API-created domains exist.
    async fn handle_domains_create(&self, params: &Value) -> Result<Value, String> {
        let req: DomainsCreate = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid domains.create params: {e}"))?;

        if req.persist {
            return Err(
                "persistent domains are not yet supported — create with persist=false (ephemeral)"
                    .into(),
            );
        }

        let id = DomainId::new(&req.name).map_err(|e| e.to_string())?;
        let requested = DomainPortSpec {
            gelf: req.gelf_port,
            otlp_grpc: req.otlp_grpc_port,
            otlp_http: req.otlp_http_port,
        };

        // No two of a domain's ports (GELF, OTLP gRPC, OTLP HTTP) may coincide —
        // the synchronous bind would otherwise fail with a cryptic "address in
        // use". A `0` (disabled) or omitted port is exempt. (Consumer #4; note the
        // OTLP defaults 4317/4318 are ADJACENT, so a per-track base+N stride must
        // step OTLP by ≥2 across domains — see the config docs.)
        let named_ports = [
            ("gelf_port", req.gelf_port),
            ("otlp_grpc_port", req.otlp_grpc_port),
            ("otlp_http_port", req.otlp_http_port),
        ];
        for i in 0..named_ports.len() {
            for j in (i + 1)..named_ports.len() {
                if let (Some(a), Some(b)) = (named_ports[i].1, named_ports[j].1) {
                    if a != 0 && a == b {
                        return Err(format!(
                            "{} and {} must differ (both = {a})",
                            named_ports[i].0, named_ports[j].0
                        ));
                    }
                }
            }
        }

        // Serialize the whole check-and-commit (see `create_lock`). Held until
        // this function returns, so get()→count→bind→insert is atomic with
        // respect to other concurrent creates on this shared handler.
        let _create_guard = self.create_lock.lock().await;

        // Idempotent ensure: an existing domain with matching (or unspecified)
        // ports is a no-op; conflicting ports are an error.
        if let Some(existing) = self.domains.get(&id) {
            return ensure_idempotent(&existing, &requested, self.domain_policy.stale_after_secs);
        }

        // `max_domains` caps API-created (non-`config`) domains; `config` +
        // `default` are always honored and don't count.
        let api_created = self
            .domains
            .list()
            .iter()
            .filter(|d| d.config.source != DomainSource::Config)
            .count();
        if api_created >= self.domain_policy.max_domains {
            return Err(format!(
                "max_domains ({}) reached — delete a domain before creating another",
                self.domain_policy.max_domains
            ));
        }

        let log_buffer_size = req
            .log_buffer_size
            .unwrap_or(self.domain_policy.default_log_buffer_size);
        let span_buffer_size = req
            .span_buffer_size
            .unwrap_or(self.domain_policy.default_span_buffer_size);

        // Bound the buffer sizes before they reach `reserve_exact` (see
        // `MAX_DOMAIN_BUFFER_SIZE`): reject an absurd request at the boundary
        // rather than aborting the process on first ingest.
        if log_buffer_size > Self::MAX_DOMAIN_BUFFER_SIZE {
            return Err(format!(
                "log_buffer_size {log_buffer_size} exceeds the maximum {}",
                Self::MAX_DOMAIN_BUFFER_SIZE
            ));
        }
        if span_buffer_size > Self::MAX_DOMAIN_BUFFER_SIZE {
            return Err(format!(
                "span_buffer_size {span_buffer_size} exceeds the maximum {}",
                Self::MAX_DOMAIN_BUFFER_SIZE
            ));
        }

        let domain = spawn_ephemeral_domain(
            id,
            requested,
            log_buffer_size,
            span_buffer_size,
            self.sessions.clone(),
            self.collectors.clone(),
            DomainSource::Ephemeral,
        )
        .await
        .map_err(|e| format!("could not create domain: {e}"))?;

        let info = domain_to_info(&domain, self.domain_policy.stale_after_secs);
        self.domains.insert(domain);
        serde_json::to_value(info).map_err(|e| e.to_string())
    }

    /// Delete an ephemeral (or persistent) domain. Refuses `config`-declared
    /// domains (incl. `default`). Teardown is Arc-graceful: an in-flight query
    /// that already resolved the domain keeps running against its clone, and the
    /// receivers stop when the last `Arc` drops.
    fn handle_domains_delete(&self, params: &Value) -> Result<Value, String> {
        let req: DomainsDelete = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid domains.delete params: {e}"))?;
        let id = DomainId::new(&req.name).map_err(|e| e.to_string())?;

        let existing = self
            .domains
            .get(&id)
            .ok_or_else(|| format!("domain \"{id}\" does not exist"))?;
        if existing.config.source == DomainSource::Config {
            return Err(format!(
                "cannot delete config-declared domain \"{id}\" — edit config.json"
            ));
        }
        drop(existing);
        self.domains.remove(&id);

        serde_json::to_value(DomainsDeleteResult {
            name: req.name,
            deleted: true,
        })
        .map_err(|e| e.to_string())
    }

    /// List all live domains (registry view; sorted by name for determinism).
    fn handle_domains_list(&self) -> Result<Value, String> {
        let mut domains: Vec<DomainInfo> = self
            .domains
            .list()
            .iter()
            .map(|d| {
                let mut info = domain_to_info(d, self.domain_policy.stale_after_secs);
                // Who is using this domain — derived from the session
                // registry, the authoritative record of every binding. With
                // meaningful session names the listing reads as a dashboard.
                info.bound_sessions = self.sessions.sessions_bound_to(d.id());
                info
            })
            .collect();
        domains.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::to_value(DomainsListResult { domains }).map_err(|e| e.to_string())
    }

    /// Re-key the calling session to a meaningful name (spec 2026-07-17). A
    /// CONNECTED holder of the target name errors — the one-conversation-per-
    /// lane invariant ("another conversation is working this lane"); a stale
    /// disconnected holder is displaced (its cross-domain bookmarks cleared).
    /// The connection loop re-keys its local session id off the echoed name.
    fn handle_sessions_rename(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let req: SessionsRename = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid sessions.rename params: {e}"))?;
        let (new_id, displaced) = self
            .sessions
            .rename(session_id, &req.name)
            .map_err(|e| e.to_string())?;
        let displaced_stale_holder = match displaced {
            Some(touched) => {
                // The stale holder's bookmarks are keyed by the NAME the
                // renamed session now owns — sweep them so the live session
                // doesn't inherit a dead conversation's bookmarks. (It has
                // made none under this name yet, so the sweep is safe.)
                let key = new_id.to_string();
                let mut cleared = 0;
                for domain_id in &touched {
                    if let Some(d) = self.domains.get(domain_id) {
                        cleared += d.bookmarks.clear_session(&key);
                    }
                }
                // Collectors follow the bookmarks, for the same reason and now
                // actually. Left behind, the displaced session's collectors —
                // their live window AND their recorded history — would become
                // readable and removable by whoever took the name, which is a
                // different conversation's measurements.
                let inherited = self.collectors.drop_session(&new_id);
                tracing::info!(name = %key, bookmarks_cleared = cleared,
                    collectors_cleared = inherited,
                    "displaced a disconnected session holding the target name");
                true
            }
            None => false,
        };
        // The renaming session keeps its OWN collectors: their owner moves with
        // it. Without this they are orphaned under a name nothing resolves —
        // invisible to an owner-scoped list, unreachable by sessions.drop, and
        // never swept, while still holding their share of the reservation.
        let moved = self.collectors.rename_owner(session_id, &new_id);
        tracing::info!(old = %session_id, new = %new_id, collectors_moved = moved,
            "session renamed");
        serde_json::to_value(SessionsRenameResult {
            name: req.name,
            displaced_stale_holder,
        })
        .map_err(|e| e.to_string())
    }

    /// Bind the calling session to an existing domain (`domains.use`). Errors if
    /// the domain is absent — no silent fallback to `default` (§5). `set_domain`
    /// clears the session's notification queue (F1) and resets its post-window
    /// (F3) on a real change. The pre-trigger buffer is re-synced for BOTH the
    /// old and new domain (§9.2/F4): the old domain's max pre-window may shrink
    /// now that this session left it, and the new domain's may grow.
    fn handle_domains_use(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let req: DomainsUse = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid domains.use params: {e}"))?;
        let new_id = DomainId::new(&req.name).map_err(|e| e.to_string())?;
        let new_domain = self
            .domains
            .get(&new_id)
            .ok_or_else(|| format!("domain \"{new_id}\" does not exist — create it first"))?;

        let old_id = self.sessions.domain_of(session_id);
        // set_domain runs BEFORE the syncs so max_pre_window reflects the new
        // binding (F4).
        self.sessions.set_domain(session_id, new_id.clone());

        if old_id != new_id {
            if let Some(old_domain) = self.domains.get(&old_id) {
                sync_pre_buffer_size_for_domain(&old_domain.pipeline, &self.sessions, &old_id);
            }
        }
        sync_pre_buffer_size_for_domain(&new_domain.pipeline, &self.sessions, &new_id);

        // A session's normal lifecycle is default → its domain, then idempotent
        // re-binds. A NON-default → different NON-default switch is the
        // signature of two clients sharing one session (e.g. two agent
        // conversations behind one shared MCP server process, each rebinding
        // the other's reads out from under it). Warn — in the response, so
        // both consumers of the shared stream see it — but still bind: the
        // switch may be deliberate (one client hopping domains on purpose).
        let rebind_warning = if old_id != new_id && old_id != DomainId::default_domain() {
            let msg = format!(
                "session {session_id} was bound to \"{old_id}\" and is now rebound to \
                 \"{new_id}\". If you did not just switch domains on purpose, another client \
                 is sharing this session (two agent conversations behind one MCP server \
                 process?) and your reads may silently target its domain. Fix the setup: \
                 give each client its own `--session <name>` — do NOT paper over it by \
                 re-binding before each query, which only hides the collision."
            );
            tracing::warn!(session = %session_id, old = %old_id, new = %new_id,
                "non-default domain rebind — possible shared session");
            Some(msg)
        } else {
            None
        };

        serde_json::to_value(logmon_broker_protocol::DomainsUseResult {
            domain: domain_to_info(&new_domain, self.domain_policy.stale_after_secs),
            rebind_warning,
        })
        .map_err(|e| e.to_string())
    }

    // -----------------------------------------------------------------------
    // domain_data.* — the provenance registry (case-documents spec §3)
    // -----------------------------------------------------------------------

    /// Resolve the calling session's registry.
    ///
    /// Keyed on the session's **bound domain**, so provenance follows the
    /// domain rather than the connection — which is what makes it survive a
    /// disconnect and be readable by whoever captures the case later.
    fn resolve_domain_data(
        &self,
        session_id: &SessionId,
    ) -> Result<(String, Arc<crate::domain_data::persist::DomainRegistry>), String> {
        let store = self
            .domain_data
            .as_ref()
            .ok_or("domain_data is not available on this broker")?;
        // Resolve through the domain registry first, so a session bound to a
        // deleted domain gets the same vanished-domain error every other query
        // gives rather than silently opening a registry for a domain that is
        // gone.
        let d = self.resolve_domain(session_id)?;
        let name = d.config.name.to_string();
        let reg = store.for_domain(&name);
        Ok((name, reg))
    }

    /// Write an entry list into the calling session's domain registry.
    ///
    /// **The one implementation behind `domain_data.update` and the `data`
    /// shorthands** (§3.8). `collectors.add(data)` and `cases.create(data)` are
    /// *defined* as this call on that call's domain — the same validation, the
    /// same per-entry outcomes, the same `/logmon/` guard, the same caps. Not a
    /// namespace, not a parallel store, and not three semantics behind one
    /// spelling.
    ///
    /// Sigilled keys are refused in here rather than at each caller: the
    /// registry is the wrong scope for a key that names one document, and
    /// `DomainDataRejection::SigilNotAllowedHere` already says so.
    fn apply_data_entries(
        &self,
        session_id: &SessionId,
        raw: Option<&Value>,
    ) -> Result<(Vec<DomainDataOutcome>, Option<String>), String> {
        let (_, reg) = self.resolve_domain_data(session_id)?;
        let (indexed, rejected) = parse_data_entries(raw)?;
        let (indices, entries): (Vec<usize>, Vec<_>) = indexed.into_iter().unzip();
        let now = chrono::Utc::now();
        let (applied, saved) = reg.mutate(|r| r.update(&entries, now));

        // Malformed `value`/`ttl` were rejected before the registry saw them.
        // Splice them back **by index** so the caller gets one outcome per entry
        // sent, in the order sent — a caller matching results to inputs
        // positionally must not have to reconcile two lists.
        let outcomes = merge_outcomes(applied, indices, rejected);
        Ok((outcomes.iter().map(wire_outcome).collect(), saved.err()))
    }

    /// [`Self::apply_data_entries`], plus the one thing `cases.create` can do
    /// that the other two callers cannot: keep a sigilled key (§3.9).
    ///
    /// The split happens **before** the registry sees anything. The registry is
    /// long-lived and shared; a capture's assertions are not, and letting them
    /// in would mean the next case on this domain silently inherits the last
    /// one's seed — laundering one incident's fact into another's document.
    ///
    /// `data` may be absent here, unlike `domain_data.update`'s `entries`: a
    /// capture that records nothing new is ordinary.
    fn apply_case_data_entries(
        &self,
        session_id: &SessionId,
        raw: Option<&Value>,
    ) -> Result<(Vec<DomainDataOutcome>, Option<String>, ScopedData), String> {
        if raw.is_none() {
            return Ok((Vec::new(), None, ScopedData::default()));
        }
        let (_, reg) = self.resolve_domain_data(session_id)?;
        let (indexed, mut rejected) = parse_data_entries(raw)?;

        // Partitioned here rather than through `partition_scoped`, because every
        // outcome has to keep the caller's original index — a caller matching
        // results to inputs positionally must not have to reconcile two lists.
        // The per-entry rules are still `scope_one`'s, not a second copy.
        let mut scoped = ScopedData::default();
        let mut for_registry = Vec::new();
        for (i, entry) in indexed {
            if !crate::domain_data::is_scoped(&entry) {
                for_registry.push((i, entry));
                continue;
            }
            match crate::domain_data::scope_one(&entry, &mut scoped) {
                Ok(()) => rejected.push((
                    i,
                    crate::domain_data::DataOutcome {
                        path: entry.path.clone(),
                        outcome: crate::domain_data::Outcome::Scoped,
                    },
                )),
                Err(rejection) => rejected.push((i, rejection)),
            }
        }

        let (indices, entries): (Vec<usize>, Vec<_>) = for_registry.into_iter().unzip();
        let now = chrono::Utc::now();
        let (applied, saved) = if entries.is_empty() {
            (Vec::new(), Ok(()))
        } else {
            reg.mutate(|r| r.update(&entries, now))
        };
        let outcomes = merge_outcomes(applied, indices, rejected);
        Ok((
            outcomes.iter().map(wire_outcome).collect(),
            saved.err(),
            scoped,
        ))
    }

    fn handle_domain_data_update(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let (outcomes, persist_error) =
            self.apply_data_entries(session_id, params.get("entries"))?;
        serde_json::to_value(DomainDataUpdateResult {
            outcomes,
            persist_error,
        })
        .map_err(|e| e.to_string())
    }

    fn handle_domain_data_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let (_, reg) = self.resolve_domain_data(session_id)?;
        let patterns = opt_str_array(params, "patterns")?.unwrap_or_default();
        if patterns.is_empty() {
            return Err("patterns is required and must be a non-empty array".into());
        }
        let (outcomes, saved) = reg.mutate(|r| r.remove(&patterns));
        serde_json::to_value(DomainDataRemoveResult {
            outcomes: outcomes
                .into_iter()
                .map(|o| DomainDataRemoveOutcome {
                    pattern: o.pattern,
                    removed: o.removed,
                    reason: o.rejected.map(|r| r.as_str().to_string()),
                })
                .collect(),
            persist_error: saved.err(),
        })
        .map_err(|e| e.to_string())
    }

    fn handle_domain_data_get(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let store = self
            .domain_data
            .as_ref()
            .ok_or("domain_data is not available on this broker")?;
        let d = self.resolve_domain(session_id)?;
        let name = d.config.name.to_string();
        let prefix = opt_str(params, "prefix")?;
        let now = chrono::Utc::now();
        // Checked, not cast. `chrono::Duration::seconds` PANICS out of range and
        // `s as i64` turns a large `u64` negative, so the old form could either
        // move the cutoff into the future or take the connection task down —
        // `validated_before_secs: 9223372036854775807` did the latter.
        let cutoff = match opt_u64(params, "validated_before_secs")? {
            None => None,
            Some(s) => Some(
                i64::try_from(s)
                    .ok()
                    .and_then(chrono::Duration::try_seconds)
                    .and_then(|age| now.checked_sub_signed(age))
                    .ok_or_else(|| {
                        format!(
                            "`validated_before_secs` is {s}, which is not a usable age in seconds"
                        )
                    })?,
            ),
        };

        let reg = store.for_domain(&name);
        let (keys, total, missing_core) = reg.read(|r| {
            let keys: Vec<DomainDataKey> = r
                .query(prefix, cutoff)
                .into_iter()
                .map(|(path, e)| DomainDataKey {
                    path: path.clone(),
                    value: e.value.clone(),
                    created_at: e.created_at.to_rfc3339(),
                    validated_at: e.validated_at.to_rfc3339(),
                    age_secs: e.age(now).num_seconds(),
                    ttl: e.ttl_secs.map(crate::domain_data::render_duration),
                    expired: e.is_expired(now),
                })
                .collect();
            let missing: Vec<String> = crate::domain_data::CORE_KEYS
                .iter()
                .filter(|k| r.get(k).is_none())
                .map(|k| k.to_string())
                .collect();
            (keys, r.entries().len(), missing)
        });

        serde_json::to_value(DomainDataGetResult {
            domain: name,
            keys,
            total,
            missing_core,
        })
        .map_err(|e| e.to_string())
    }

    /// Dispose the BOUND domain's data — logs AND spans — keeping the domain and
    /// its receivers alive; seq stays monotonic. (`logs.clear` is the logs-only,
    /// back-compat cousin.)
    fn handle_domains_clear(&self, session_id: &SessionId) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let logs_cleared = d.pipeline.clear_logs();
        let spans_cleared = d.span_store.clear();
        serde_json::to_value(DomainsClearResult {
            logs_cleared,
            spans_cleared,
        })
        .map_err(|e| e.to_string())
    }

    // -----------------------------------------------------------------------
    // logs.*
    // -----------------------------------------------------------------------

    /// Parse a filter string and resolve any bookmark qualifiers against the
    /// given domain's bookmark store using `session_id` as the current session.
    /// Returns `Ok((None, None))` if the input is `None` or an
    /// empty/whitespace-only string — this matches the previous
    /// `recent_logs_str` behavior, which silently treated empty/parse-failed
    /// filters as "no filter". Real parse errors and resolution errors are
    /// surfaced (this is a behavior change from the old `.ok()`-swallowing
    /// path, but a desirable one — bookmark errors must be visible).
    ///
    /// The second tuple element is the cursor commit handle when the filter
    /// contained a `c>=` qualifier. Callers that support cursor semantics
    /// (`logs.recent`, `logs.export`, `traces.logs`) capture it and call
    /// `commit_handle.commit(max_seq)` after the lock-free query phase. For
    /// non-cursor handlers, the value is always `None`.
    fn parse_and_resolve_filter(
        &self,
        filter_str: Option<&str>,
        session_id: &SessionId,
        bookmarks: &crate::store::bookmarks::BookmarkStore,
    ) -> Result<
        (
            Option<crate::filter::parser::ParsedFilter>,
            Option<crate::store::bookmarks::CursorCommit>,
        ),
        String,
    > {
        let Some(s) = filter_str else {
            return Ok((None, None));
        };
        if s.trim().is_empty() {
            return Ok((None, None));
        }
        let parsed = crate::filter::parser::parse_filter(s).map_err(|e| e.to_string())?;
        let resolved = crate::filter::bookmark_resolver::resolve_bookmarks(
            parsed,
            bookmarks,
            &session_id.to_string(),
        )
        .map_err(|e| e.to_string())?;
        Ok((Some(resolved.filter), resolved.cursor_commit))
    }

    fn handle_logs_recent(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let count = opt_usize(params, "count")?.unwrap_or(50);
        let filter_str = opt_str(params, "filter")?;

        // trace_id shortcut path — does NOT support cursor; reject c>= if present.
        if let Some(trace_id_hex) = opt_str(params, "trace_id")? {
            if let Some(s) = filter_str {
                if !s.trim().is_empty() {
                    let parsed =
                        crate::filter::parser::parse_filter(s).map_err(|e| e.to_string())?;
                    if crate::filter::parser::contains_cursor_qualifier(&parsed) {
                        return Err(
                            "cursor qualifier not permitted in logs.recent with trace_id"
                                .to_string(),
                        );
                    }
                }
            }
            let trace_id =
                u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;
            let logs = d.pipeline.logs_by_trace_id(trace_id);
            let count = logs.len();
            // trace_id is an index lookup over the whole buffer, not a filter
            // scan — report buffer stats so B2's `scanned==0 ⇒ empty buffer`
            // heuristic still holds (never a misleading 0 on a non-empty store).
            let buffer_total = d.pipeline.store_len();
            return Ok(json!({
                "logs": logs,
                "count": count,
                "scanned": buffer_total,
                "buffer_total": buffer_total,
                "buffer_oldest_seq": d.pipeline.oldest_log_seq(),
                "buffer_newest_seq": d.pipeline.newest_log_seq(),
            }));
        }

        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        let oldest_first = cursor_commit.is_some();
        let (entries, stats) =
            d.pipeline
                .recent_logs_with_stats(count, resolved.as_ref(), oldest_first);

        // Drive the cursor commit + populate cursor_advanced_to.
        let advanced_to = if let Some(commit) = cursor_commit {
            let max_seq = entries.iter().map(|e| e.seq).max();
            if let Some(s) = max_seq {
                commit.commit(s);
                Some(s)
            } else {
                // No records returned: leave the cursor at its current position.
                // Dropping the unused commit is a no-op.
                drop(commit);
                None
            }
        } else {
            None
        };

        let evicted_before_window = resolved
            .as_ref()
            .and_then(|f| crate::filter::parser::evicted_before_window(f, stats.buffer_oldest_seq));

        let mut result = json!({
            "logs": entries,
            "count": entries.len(),
            "scanned": stats.scanned,
            "buffer_total": stats.buffer_total,
            "buffer_oldest_seq": stats.buffer_oldest_seq,
            "buffer_newest_seq": stats.buffer_newest_seq,
            "truncated": evicted_before_window.is_some(),
            "evicted_before_window": evicted_before_window,
        });
        if let Some(s) = advanced_to {
            result["cursor_advanced_to"] = json!(s);
        }
        Ok(result)
    }

    fn handle_logs_context(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let seq = req_u64(params, "seq")?;
        let before = opt_context_window(params, "before")?.unwrap_or(10);
        let after = opt_context_window(params, "after")?.unwrap_or(10);
        let entries = d.pipeline.context_by_seq(seq, before, after);
        Ok(json!({ "logs": entries, "count": entries.len() }))
    }

    fn handle_logs_export(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let count = opt_usize(params, "count")?.unwrap_or(usize::MAX);
        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        let resolved = lower_seq_range(
            resolved,
            opt_u64(params, "from_seq")?,
            opt_u64(params, "to_seq")?,
        );
        let oldest_first = cursor_commit.is_some();
        // Asked for one more than requested, so `capped` is a fact rather than
        // an inference: "asked N, got N" is true both when the range was cut
        // short and when exactly N existed, and §4.2 forbids calling a capped
        // range complete. `recent_with_scanned` takes the FIRST `count`
        // matching in whichever direction it walks, so taking one extra and
        // dropping it returns the same entries either way.
        let probe = count.saturating_add(1);
        let (mut entries, stats) =
            d.pipeline
                .recent_logs_with_stats(probe, resolved.as_ref(), oldest_first);
        let capped = entries.len() > count;
        entries.truncate(count);

        let advanced_to = if let Some(commit) = cursor_commit {
            let max_seq = entries.iter().map(|e| e.seq).max();
            if let Some(s) = max_seq {
                commit.commit(s);
                Some(s)
            } else {
                drop(commit);
                None
            }
        } else {
            None
        };

        let evicted_before_window = resolved
            .as_ref()
            .and_then(|f| crate::filter::parser::evicted_before_window(f, stats.buffer_oldest_seq));

        // How much of this window the daemon can vouch for (§4.2).
        //
        // The window is the caller's range where it gave one. Where it did not,
        // the upper end is the newest seq ASSIGNED, not the newest stored: a
        // filter that dropped everything since seq N leaves `buffer_newest_seq`
        // below N, and taking that as the window would put the filtered tail
        // outside it and report the remainder `complete` — the reader's "I have
        // everything" is exactly what would be wrong.
        //
        // An empty store gets no window at all, and so no claim. It satisfies
        // every clause of `complete` vacuously — nothing evicted, nothing
        // capped, nothing narrowed — which is why §4.2 makes emptiness a
        // verdict of its own rather than a remark under the table.
        let (lo, hi) = crate::filter::parser::resolved_seq_range(resolved.as_ref());
        let window = stats.buffer_oldest_seq.and_then(|oldest| {
            let from = lo.unwrap_or(oldest);
            // Read AFTER the query, so the window is guaranteed to contain every
            // seq the query could have returned. Reading it first would leave an
            // entry that arrived mid-query outside the window the verdict is
            // about.
            let to = hi.unwrap_or_else(|| d.pipeline.current_seq());
            (from <= to).then_some((from, to))
        });
        let coverage = window.map(|(from, to)| d.pipeline.epochs().coverage(from, to));
        let verdict = crate::engine::epoch::evidence_verdict(
            coverage.as_ref(),
            evicted_before_window.is_some(),
            capped,
        );
        let narrowed_by: Vec<Value> = coverage
            .as_ref()
            .map(|c| {
                c.narrowed
                    .iter()
                    .map(|n| {
                        json!({
                            "from_seq": n.from_seq,
                            "to_seq": n.to_seq,
                            "filters": n.filters,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut result = json!({
            "logs": entries,
            "count": entries.len(),
            "format": "json",
            "scanned": stats.scanned,
            "buffer_total": stats.buffer_total,
            "buffer_oldest_seq": stats.buffer_oldest_seq,
            "buffer_newest_seq": stats.buffer_newest_seq,
            "truncated": evicted_before_window.is_some(),
            "evicted_before_window": evicted_before_window,
            "capped": capped,
            "verdict": verdict,
            "narrowed_by": narrowed_by,
        });
        if let Some(s) = advanced_to {
            result["cursor_advanced_to"] = json!(s);
        }
        Ok(result)
    }

    fn handle_logs_clear(&self, session_id: &SessionId) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let cleared = d.pipeline.clear_logs();
        Ok(json!({ "cleared": cleared }))
    }

    // -----------------------------------------------------------------------
    // status.*
    // -----------------------------------------------------------------------

    /// Every tool this daemon knows, with the schema its arguments must satisfy.
    ///
    /// **This is what lets the daemon gain a tool without the shim being
    /// reinstalled.** A shim that registers from this manifest exposes whatever
    /// the daemon declares; one built against a fixed list can only ever expose
    /// what it was compiled with.
    ///
    /// Takes no session and no parameters: the manifest describes the protocol,
    /// not this connection, so there is nothing about the caller that could
    /// change the answer.
    fn handle_tools_manifest() -> Result<Value, String> {
        Ok(json!({
            "protocol_version": PROTOCOL_VERSION,
            "broker_version": env!("CARGO_PKG_VERSION"),
            "tools": logmon_broker_protocol::mcp_tools::manifest(),
        }))
    }

    fn handle_status(&self, session_id: &SessionId) -> Result<Value, String> {
        // Neither of the two broker facts below depends on a domain, so they are
        // bound here rather than inside the payload — the placement records that
        // they *could* be served when `resolve_domain` fails.
        //
        // They are not, today. This whole function returns `Err` once the
        // caller's bound domain has been deleted, so the skew facts go with it.
        // Accepted rather than fixed: the state needs `use_domain(x)` then
        // `delete_domain(x)` to reach, the error names its own remedy
        // ("use_domain to rebind"), and serving a partial status would mean
        // giving `StatusGetResult::store` a default — letting an absent buffer
        // read as an empty one, which is the conflation this codebase refuses
        // everywhere else. Pinned by `capability_skew.rs`.
        let broker_version = env!("CARGO_PKG_VERSION");
        let broker_tools = logmon_broker_protocol::mcp_tools::tool_names();

        let d = self.resolve_domain(session_id)?;
        let session_info = self.sessions.get(session_id);
        let stats = d.pipeline.store_stats();
        let drops = d.metrics.snapshot();
        let trace_ingest = d.metrics.trace_ingest_loss();
        // §7 situational awareness: where the session is bound + what is
        // narrowing it, so one call answers "where am I / what's filtering me".
        let current_domain = self.sessions.domain_of(session_id).to_string();
        let active_filters: Vec<String> = self
            .sessions
            .list_filters(session_id)
            .iter()
            .map(|f| f.filter_string.clone())
            .collect();
        // Per-listener last-received drill-down for the bound domain (#2).
        let liv = d.metrics.liveness();
        let to_dt = |n: Option<i64>| n.map(chrono::DateTime::<chrono::Utc>::from_timestamp_nanos);
        let receiver_liveness = ReceiverLiveness {
            gelf_udp: to_dt(liv.gelf_udp),
            gelf_tcp: to_dt(liv.gelf_tcp),
            otlp_http_logs: to_dt(liv.otlp_http_logs),
            otlp_http_traces: to_dt(liv.otlp_http_traces),
            otlp_grpc_logs: to_dt(liv.otlp_grpc_logs),
            otlp_grpc_traces: to_dt(liv.otlp_grpc_traces),
        };
        Ok(json!({
            "session": session_info.map(|s| json!({
                "id": s.id.to_string(),
                "name": s.name,
                "connected": s.connected,
                "trigger_count": s.trigger_count,
                "filter_count": s.filter_count,
                "queue_size": s.queue_size,
                "last_seen_secs_ago": s.last_seen_secs_ago,
                "client_info": s.client_info,
            })),
            "daemon_uptime_secs": self.start_time.elapsed().as_secs(),
            // What a shim built at this broker's version exposes. A client
            // holding fewer tools than this is out of date; one holding these
            // or more is current. Tool names rather than RPC method names,
            // because that is the vocabulary an agent can actually compare
            // against what it is holding.
            "broker_version": broker_version,
            "broker_tools": broker_tools,
            "receivers": self.receivers_info,
            "store": {
                "total_received": stats.total_received,
                "total_stored": stats.total_stored,
                "malformed_count": stats.malformed_count,
                "current_size": d.pipeline.store_len(),
            },
            "receiver_drops": {
                "gelf_udp": drops.gelf_udp,
                "gelf_tcp": drops.gelf_tcp,
                "otlp_http_logs": drops.otlp_http_logs,
                "otlp_http_traces": drops.otlp_http_traces,
                "otlp_grpc_logs": drops.otlp_grpc_logs,
                "otlp_grpc_traces": drops.otlp_grpc_traces,
            },
            "trace_ingest": {
                "dropped": trace_ingest.dropped,
                "shed_batches": trace_ingest.shed_batches,
                "malformed_dropped": trace_ingest.malformed,
            },
            "current_domain": current_domain,
            "active_filters": active_filters,
            "receiver_liveness": receiver_liveness,
        }))
    }

    // -----------------------------------------------------------------------
    // filters.*
    // -----------------------------------------------------------------------

    fn handle_filters_list(&self, session_id: &SessionId) -> Result<Value, String> {
        let filters = self.sessions.list_filters(session_id);
        let items: Vec<Value> = filters
            .iter()
            .map(|f| {
                json!({
                    "id": f.id,
                    "filter": f.filter_string,
                    "description": f.description,
                })
            })
            .collect();
        Ok(json!({ "filters": items }))
    }

    fn handle_filters_add(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let filter = req_str(params, "filter")?;
        // Reject bookmark filters in registered (long-lived) filters.
        let parsed = crate::filter::parser::parse_filter(filter).map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
        let desc = opt_str(params, "description")?;
        let id = self
            .sessions
            .add_filter(session_id, filter, desc)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({ "id": id }))
    }

    fn handle_filters_edit(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let filter_id = req_u32(params, "id")?;
        let filter = opt_str(params, "filter")?;
        let desc = opt_str(params, "description")?;
        let info = self
            .sessions
            .edit_filter(session_id, filter_id, filter, desc)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "id": info.id,
            "filter": info.filter_string,
            "description": info.description,
        }))
    }

    fn handle_filters_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let filter_id = req_u32(params, "id")?;
        self.sessions
            .remove_filter(session_id, filter_id)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({ "removed": filter_id }))
    }

    // -----------------------------------------------------------------------
    // triggers.*
    // -----------------------------------------------------------------------

    fn handle_triggers_list(&self, session_id: &SessionId) -> Result<Value, String> {
        let triggers = self.sessions.list_triggers(session_id);
        let items: Vec<Value> = triggers
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "filter": t.filter_string,
                    "pre_window": t.pre_window,
                    "post_window": t.post_window,
                    "notify_context": t.notify_context,
                    "description": t.description,
                    "match_count": t.match_count,
                    "oneshot": t.oneshot,
                    "post_remaining": t.post_remaining,
                })
            })
            .collect();
        Ok(json!({ "triggers": items }))
    }

    fn handle_triggers_add(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let filter = req_str(params, "filter")?;
        // Reject bookmark filters in registered (long-lived) triggers.
        let parsed = crate::filter::parser::parse_filter(filter).map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
        // Omitted windows default to 500/200/5 (§6, decision #4) so an ad-hoc
        // trigger captures context by default. An EXPLICIT value — including 0 —
        // is honored (only an absent or null value defaults).
        let pre = opt_u32(params, "pre_window")?
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_PRE_WINDOW);
        let post = opt_u32(params, "post_window")?
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_POST_WINDOW);
        let ctx = opt_u32(params, "notify_context")?
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_NOTIFY_CONTEXT);
        let desc = opt_str(params, "description")?;
        let oneshot = opt_bool(params, "oneshot")?.unwrap_or(false);
        let id = self
            .sessions
            .add_trigger(session_id, filter, pre, post, ctx, desc, oneshot)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({ "id": id }))
    }

    fn handle_triggers_edit(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trigger_id = req_u32(params, "id")?;
        let filter = opt_str(params, "filter")?;
        let pre = opt_u32(params, "pre_window")?;
        let post = opt_u32(params, "post_window")?;
        let ctx = opt_u32(params, "notify_context")?;
        let desc = opt_str(params, "description")?;
        let oneshot = opt_bool(params, "oneshot")?;
        let info = self
            .sessions
            .edit_trigger(
                session_id, trigger_id, filter, pre, post, ctx, desc, oneshot,
            )
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({
            "id": info.id,
            "filter": info.filter_string,
            "pre_window": info.pre_window,
            "post_window": info.post_window,
            "notify_context": info.notify_context,
            "description": info.description,
            "match_count": info.match_count,
            "oneshot": info.oneshot,
            "post_remaining": info.post_remaining,
        }))
    }

    fn handle_triggers_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trigger_id = req_u32(params, "id")?;
        self.sessions
            .remove_trigger(session_id, trigger_id)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({ "removed": trigger_id }))
    }

    // -----------------------------------------------------------------------
    // traces.* / spans.*
    // -----------------------------------------------------------------------

    fn handle_traces_recent(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let count = opt_usize(params, "count")?.unwrap_or(20);
        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.recent".to_string());
        }

        let pipeline = &d.pipeline;
        let summaries = d
            .span_store
            .recent_traces(count, resolved.as_ref(), |trace_id| {
                pipeline.count_by_trace_id(trace_id) as u32
            });
        // recent_traces walks the whole span buffer, so scanned == buffer_total
        // for this query. (Buffer stats are read separately from the scan — a
        // concurrent write can make them momentarily differ; a diagnostic count,
        // not a strict snapshot.)
        let buffer_total = d.span_store.len();
        Ok(json!({
            "traces": summaries,
            "count": summaries.len(),
            "scanned": buffer_total,
            "buffer_total": buffer_total,
            "buffer_oldest_seq": d.span_store.oldest_seq(),
            "buffer_newest_seq": d.span_store.newest_seq(),
        }))
    }

    fn handle_traces_get(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trace_id_hex = req_str(params, "trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16)
            .map_err(|_| "invalid trace_id: must be 32-char hex")?;
        let include_logs = opt_bool(params, "include_logs")?.unwrap_or(true);

        // Resolve filter (used to filter spans within the trace below)
        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.get".to_string());
        }

        let mut spans = d.span_store.get_trace(trace_id);
        if let Some(f) = resolved.as_ref() {
            spans.retain(|s| crate::filter::matcher::matches_span(f, s));
        }
        let logs = if include_logs {
            d.pipeline.logs_by_trace_id(trace_id)
        } else {
            vec![]
        };

        Ok(json!({
            "trace_id": trace_id_hex,
            "spans": spans,
            "logs": logs,
            "span_count": spans.len(),
            "log_count": logs.len(),
        }))
    }

    fn handle_traces_summary(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trace_id_hex = req_str(params, "trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let spans = d.span_store.get_trace(trace_id);
        if spans.is_empty() {
            return Err(format!("no spans found for trace {trace_id_hex}"));
        }

        // Find root span and compute self-time breakdown for direct children
        let root = spans.iter().find(|s| s.parent_span_id.is_none());
        let root_duration = root.map_or(0.0, |r| r.duration_ms);
        let root_name = root.map_or("[no root]", |r| r.name.as_str());
        let root_span_id = root.map(|r| r.span_id);

        // Direct children of root
        let children: Vec<_> = spans
            .iter()
            .filter(|s| s.parent_span_id == root_span_id)
            .collect();

        let mut breakdown: Vec<Value> = children
            .iter()
            .map(|child| {
                // Self-time = child duration - sum of its direct children's durations
                let grandchildren_time: f64 = spans
                    .iter()
                    .filter(|s| s.parent_span_id == Some(child.span_id))
                    .map(|s| s.duration_ms)
                    .sum();
                let self_time = (child.duration_ms - grandchildren_time).max(0.0);
                let pct = if root_duration > 0.0 {
                    (self_time / root_duration * 100.0).round()
                } else {
                    0.0
                };
                json!({
                    "name": child.name,
                    "self_time_ms": self_time,
                    "total_time_ms": child.duration_ms,
                    "percentage": pct,
                    "is_error": matches!(child.status, crate::span::types::SpanStatus::Error(_)),
                })
            })
            .collect();

        // Sort by self_time descending
        breakdown.sort_by(|a, b| {
            b["self_time_ms"]
                .as_f64()
                .unwrap_or(0.0)
                .partial_cmp(&a["self_time_ms"].as_f64().unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // "other" time
        let accounted: f64 = breakdown
            .iter()
            .map(|b| b["self_time_ms"].as_f64().unwrap_or(0.0))
            .sum();
        let other = (root_duration - accounted).max(0.0);

        Ok(json!({
            "root_span": root_name,
            "total_duration_ms": root_duration,
            "breakdown": breakdown,
            "other_ms": other,
            "span_count": spans.len(),
        }))
    }

    fn handle_traces_slow(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let min_duration = opt_f64(params, "min_duration_ms")?.unwrap_or(100.0);
        let count = opt_usize(params, "count")?.unwrap_or(20);
        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.slow".to_string());
        }
        // An unrecognised string used to select the ungrouped arm silently.
        // That was the documented contract, and it is gone: a typo returning a
        // different answer than the one asked for is the failure the strict
        // readers on this surface exist to close, and read-only does not make a
        // wrong answer safe (#7).
        let group_by = match opt_str(params, "group_by")? {
            None => SlowGroupBy::None,
            Some(s) => SlowGroupBy::parse(s).ok_or_else(|| {
                format!(
                    "unknown group_by `{s}`: expected {}",
                    SlowGroupBy::accepted()
                )
            })?,
        };

        match group_by {
            // §1.1: the grouped arm aggregates the FULL matching population.
            // It used to group `slow_spans`'s output, which is filtered by the
            // duration floor and then truncated to `count` — so `avg_ms` was
            // the average of the slowest few above a floor, presented as the
            // average for that span name. `min_duration_ms` is now a display
            // floor applied after aggregation, and it selects which names to
            // show rather than which spans to count.
            SlowGroupBy::Name => {
                let groups = d.span_store.duration_by_name(resolved.as_ref());
                let population: usize = groups.values().map(|v| v.len()).sum();
                let mut result: Vec<Value> = groups
                    .into_iter()
                    .filter_map(|(name, mut durations)| {
                        durations.sort_by(|a, b| a.total_cmp(b));
                        let max = *durations.last()?;
                        // A name qualifies when it actually produced a span at
                        // least this slow. Its statistics then describe every
                        // span of that name, which is the point.
                        if max < min_duration {
                            return None;
                        }
                        let avg = durations.iter().sum::<f64>() / durations.len() as f64;
                        Some(json!({
                            "name": name,
                            "avg_ms": (avg * 10.0).round() / 10.0,
                            "p50_ms": quantile_ms(&durations, 0.50),
                            "p95_ms": quantile_ms(&durations, 0.95),
                            "max_ms": max,
                            "count": durations.len(),
                        }))
                    })
                    .collect();
                result.sort_by(|a, b| {
                    b["avg_ms"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .total_cmp(&a["avg_ms"].as_f64().unwrap_or(0.0))
                });
                result.truncate(count);
                Ok(json!({
                    "grouped_by": "name",
                    "groups": result,
                    // Both stated, because the numbers above mean something
                    // different from what this method used to return.
                    "population": population,
                    "display_floor_ms": min_duration,
                }))
            }
            SlowGroupBy::None => {
                let slow = d
                    .span_store
                    .slow_spans(min_duration, count, resolved.as_ref());
                Ok(json!({ "spans": slow, "count": slow.len() }))
            }
        }
    }

    fn handle_traces_logs(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trace_id_hex = req_str(params, "trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;

        // logs_by_trace_id already returns logs in stored (seq-ascending) order
        // — that's the same order cursor pagination wants — so no ordering
        // switch is needed on this code path.
        let mut logs = d.pipeline.logs_by_trace_id(trace_id);
        if let Some(f) = resolved.as_ref() {
            logs.retain(|e| crate::filter::matcher::matches_entry(f, e));
        }

        let advanced_to = if let Some(commit) = cursor_commit {
            let max_seq = logs.iter().map(|e| e.seq).max();
            if let Some(s) = max_seq {
                commit.commit(s);
                Some(s)
            } else {
                drop(commit);
                None
            }
        } else {
            None
        };

        let mut result = json!({ "logs": logs, "count": logs.len() });
        if let Some(s) = advanced_to {
            result["cursor_advanced_to"] = json!(s);
        }
        Ok(result)
    }

    fn handle_spans_context(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let seq = req_u64(params, "seq")?;
        let before = opt_context_window(params, "before")?.unwrap_or(5);
        let after = opt_context_window(params, "after")?.unwrap_or(5);

        let spans = d.span_store.context_by_seq(seq, before, after);
        Ok(json!({ "spans": spans, "count": spans.len() }))
    }

    // -----------------------------------------------------------------------
    // cases.*
    // -----------------------------------------------------------------------

    /// Capture a window as three files on disk — spec §5.
    ///
    /// The daemon writes, rather than returning the archive over RPC: a case
    /// carries several hundred kilobytes of JSONL, and handing that back through
    /// a tool result puts the whole thing in the model's context, which is the
    /// outcome §5.2's document/logdata split exists to prevent.
    fn handle_cases_create(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        use crate::cases::document as doc;

        let d = self.resolve_domain(session_id)?;
        let captured_at = chrono::Utc::now();

        let reason = req_str(params, "reason")?.trim().to_string();
        if reason.is_empty() {
            return Err(
                "`reason` must not be empty: a manual case that cannot say why it was \
                        taken has no provenance, and unlike a watch there is no filter standing \
                        in for one"
                    .to_string(),
            );
        }

        // §5.1: rejected, never resolved. The broker runs as a service, so a
        // relative path would resolve against ITS working directory rather than
        // the caller's — and silently writing the archive somewhere nobody
        // looks is the failure mode.
        let dir_raw = req_str(params, "dir")?;
        let dir = std::path::Path::new(dir_raw);
        if !dir.is_absolute() {
            return Err(format!(
                "`dir` must be an absolute path, and `{dir_raw}` is not. The broker runs as a \
                 service, so a relative path would resolve against the broker's working \
                 directory rather than yours."
            ));
        }

        let before = window_count(params, "before")?;
        let after = window_count(params, "after")?;
        let clamped = before.clamped || after.clamped;

        // Registry writes happen BEFORE the copy is rendered, so a key the
        // capturer supplies appears in *this* document rather than landing one
        // document late.
        let (data_outcomes, data_persist_error, scoped) =
            self.apply_case_data_entries(session_id, params.get("data"))?;

        let anchor = self.resolve_case_anchor(&d, params.get("anchor"))?;

        // §5.1: counts of stored RECORDS, not seq distances — one counter feeds
        // both stores, so a 200-seq range holds an unpredictable number of logs.
        let logs = d
            .pipeline
            .context_by_seq(anchor.seq, before.value, after.value);
        let (from, to) = match (logs.first(), logs.last()) {
            (Some(f), Some(l)) => (f.seq, l.seq),
            // `resolve_case_anchor` already proved the anchor is stored, so an
            // empty neighbourhood means it was evicted between the two reads.
            _ => {
                return Err(format!(
                    "the anchor entry at seq {} was evicted while the capture was being taken",
                    anchor.seq
                ))
            }
        };

        // The verdict over the window actually captured. `capped` is false here
        // by construction: `context_by_seq` returns a contiguous run, so nothing
        // inside [from, to] was cut. What `before`/`after` limited is the
        // window's WIDTH, which the front-matter reports separately.
        let coverage = d.pipeline.epochs().coverage(from, to);
        let log_evicted = crate::filter::parser::evicted_below(from, d.pipeline.oldest_log_seq());
        let verdict =
            crate::engine::epoch::evidence_verdict(Some(&coverage), log_evicted.is_some(), false);

        // Spans over the SAME resolved range, so the two files describe one
        // interval by construction rather than by two window parameters that
        // could disagree.
        let span_filter = lower_seq_range(None, Some(from), Some(to));
        let mut spans = Vec::new();
        d.span_store.for_each_matching(span_filter.as_ref(), |s| {
            spans.push(s.clone());
        });
        let span_evicted = crate::filter::parser::evicted_below(from, d.span_store.oldest_seq());

        let domain_name = d.config.name.to_string();
        let (registry, incarnation, case_name) = self.case_registry_copy(session_id, captured_at);
        let collectors = self.case_collector_lines(&d.config.name);

        let prefix = crate::cases::resolve_prefix(
            opt_str(params, "prefix")?,
            case_name.as_deref(),
            &domain_name,
        );
        let files = crate::cases::claim(dir, &prefix, captured_at).map_err(|e| e.to_string())?;
        let names = crate::cases::file_names(&files.stem);

        let anchor_seq = anchor.seq;
        let input = doc::CaseInput {
            stem: files.stem.clone(),
            captured_at,
            domain: domain_name.clone(),
            incarnation,
            reason,
            anchor,
            window: doc::Window {
                from,
                to,
                verdict,
                narrowed_by: coverage
                    .narrowed
                    .iter()
                    .map(|n| NarrowedRange {
                        from_seq: n.from_seq,
                        to_seq: n.to_seq,
                        filters: n.filters.clone(),
                    })
                    .collect(),
                capped: clamped,
                evicted_before_window: log_evicted,
                spans_evicted_before_window: span_evicted,
            },
            logdata: doc::FilePointer {
                file: names[1].clone(),
                records: logs.len() as u64,
            },
            spandata: doc::FilePointer {
                file: names[2].clone(),
                records: spans.len() as u64,
            },
            registry,
            asserted: scoped.facts,
            neighbours: neighbourhood(&logs, anchor_seq),
            spans: spans.clone(),
            collectors,
        };

        // Bulk first: if a logdata write fails the document must not point at a
        // file that is not there.
        let logdata =
            crate::cases::write_jsonl(files.logdata, &files.paths[1], "logdata", logs.iter())
                .map_err(|e| e.to_string())?;
        let spandata =
            crate::cases::write_jsonl(files.spandata, &files.paths[2], "spandata", spans.iter())
                .map_err(|e| e.to_string())?;

        let rendered = doc::render(&input);
        let document_bytes =
            crate::cases::write_document(files.document, &files.paths[0], &rendered.body)
                .map_err(|e| e.to_string())?;

        let mut notes: Vec<CaseNote> = rendered
            .notes
            .iter()
            .map(|n| CaseNote {
                kind: n.kind.to_string(),
                detail: n.detail.clone(),
            })
            .collect();
        if clamped {
            notes.push(CaseNote {
                kind: doc::NOTE_TRUNCATED.to_string(),
                detail: format!(
                    "before/after were clamped to {}; the window is narrower than requested",
                    MAX_CASE_WINDOW
                ),
            });
        }
        if let Some(err) = &data_persist_error {
            // The capture happened and the files are on disk; the registry just
            // could not be made durable. Reported, never fatal.
            notes.push(CaseNote {
                kind: "write".to_string(),
                detail: format!("the domain registry could not be persisted: {err}"),
            });
        }

        serde_json::to_value(CasesCreateResult {
            stem: files.stem,
            paths: files
                .paths
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            verdict,
            document_bytes,
            logdata: CaseFile {
                records: logdata.records,
                bytes: logdata.bytes,
            },
            spandata: CaseFile {
                records: spandata.records,
                bytes: spandata.bytes,
            },
            notes,
            data_outcomes,
            data_persist_error,
        })
        .map_err(|e| e.to_string())
    }

    /// Resolve the tagged anchor to a **stored** entry.
    ///
    /// §5.1: an unresolvable anchor is an error, not a degraded document. The
    /// anchor entry's message is the headline, and a document whose headline
    /// cannot identify the incident fails the one test §5.2 sets for a headline.
    fn resolve_case_anchor(
        &self,
        d: &std::sync::Arc<crate::daemon::domain::Domain>,
        raw: Option<&Value>,
    ) -> Result<crate::cases::document::Anchor, String> {
        use crate::cases::document::Anchor;
        let raw = raw.ok_or(
            "`anchor` is required: it is `{seq}`, `{bookmark}` or `{trace_id}`, tagged rather \
             than one string the daemon guesses at",
        )?;
        let seq = opt_u64(raw, "seq")?;
        let bookmark = opt_str(raw, "bookmark")?;
        let trace_id = opt_str(raw, "trace_id")?;
        let given = [seq.is_some(), bookmark.is_some(), trace_id.is_some()]
            .iter()
            .filter(|b| **b)
            .count();
        if given != 1 {
            return Err(format!(
                "`anchor` takes exactly one of `seq`, `bookmark` or `trace_id`, and {given} were \
                 given. It is tagged rather than sniffed because a bookmark named `12345` and a \
                 seq are indistinguishable as strings, and the failure would be a document \
                 anchored somewhere else."
            ));
        }

        let entry_at = |seq: u64| d.pipeline.context_by_seq(seq, 0, 0).into_iter().next();

        let (kind, label, entry, of_many) = if let Some(s) = seq {
            let e = entry_at(s).ok_or_else(|| {
                format!("no stored log entry has seq {s}, so there is nothing to anchor on")
            })?;
            ("seq", s.to_string(), e, None)
        } else if let Some(name) = bookmark {
            let qualified = crate::store::bookmarks::qualify(name, "");
            let b = d
                .bookmarks
                .get(&qualified)
                .or_else(|| d.bookmarks.list().into_iter().find(|b| b.name == name))
                .ok_or_else(|| format!("no bookmark named `{name}` on this domain"))?;
            // A bookmark marks a BOUNDARY, not a record: `b>=name` selects
            // `seq > bookmark.seq`, strictly. So the anchor is the first stored
            // entry after the mark — the same entry `b>=name` would hand back
            // first, rather than an off-by-one only this call would have.
            let after = crate::filter::parser::ParsedFilter::Qualifiers(vec![
                crate::filter::parser::Qualifier::SeqFilter {
                    op: crate::filter::parser::SeqOp::Gt,
                    value: b.seq,
                },
            ]);
            let (found, _) = d.pipeline.recent_logs_with_stats(1, Some(&after), true);
            let e = found.into_iter().next().ok_or_else(|| {
                format!(
                    "bookmark `{name}` is at seq {}, and no stored entry follows it",
                    b.seq
                )
            })?;
            ("bookmark", name.to_string(), e, None)
        } else {
            let hex = trace_id.expect("checked above");
            let tid = u128::from_str_radix(hex, 16)
                .map_err(|_| format!("`{hex}` is not a hexadecimal trace id"))?;
            let entries = d.pipeline.logs_by_trace_id(tid);
            let n = entries.len();
            // Earliest by seq — `logs_by_trace_id` returns stored order, which
            // is seq-ascending — and the document says how many there were.
            let e = entries
                .into_iter()
                .next()
                .ok_or_else(|| format!("no stored log entry carries trace id `{hex}`"))?;
            ("trace_id", hex.to_string(), e, (n > 1).then_some(n))
        };

        Ok(Anchor {
            kind,
            label,
            seq: entry.seq,
            at: entry.timestamp,
            message: entry.message.clone(),
            of_many,
        })
    }

    /// The registry as of `now`, plus `/logmon/incarnation` and `/case-name`.
    ///
    /// The `/logmon/` namespace is left out of the copy: it is the daemon's own
    /// bookkeeping, `incarnation` is already in front-matter, and a document
    /// listing `/logmon/first_seen` beside `/Build/commit` would present the two
    /// as the same kind of claim. A broker with no config dir returns an empty
    /// copy, which the document then states rather than omits.
    fn case_registry_copy(
        &self,
        session_id: &SessionId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> (
        Vec<crate::cases::document::RegistryFact>,
        Option<String>,
        Option<String>,
    ) {
        let Ok((_, reg)) = self.resolve_domain_data(session_id) else {
            return (Vec::new(), None, None);
        };
        reg.read(|r| {
            let incarnation = r.get("/logmon/incarnation").map(|e| e.value.clone());
            let case_name = r
                .get(crate::domain_data::CASE_NAME_KEY)
                .map(|e| e.value.clone());
            let facts = r
                .entries()
                .iter()
                .filter(|(path, _)| !crate::domain_data::path::is_reserved(path))
                .map(|(path, e)| crate::cases::document::RegistryFact {
                    path: path.clone(),
                    value: e.value.clone(),
                    created_at: e.created_at,
                    validated_at: e.validated_at,
                    ttl_secs: e.ttl_secs,
                    expired: e.is_expired(now),
                })
                .collect();
            (facts, incarnation, case_name)
        })
    }

    /// Collectors measuring this domain, whoever armed them (§5.4).
    ///
    /// Projection is deliberately not computed: `snapshot()` reads counters,
    /// and the sorting of every retained duration that a full projection does
    /// belongs nowhere near a registry lock the ingest path takes.
    fn case_collector_lines(
        &self,
        domain: &crate::daemon::domain::DomainId,
    ) -> Vec<crate::cases::document::CollectorLine> {
        let mut lines: Vec<_> = self
            .collectors
            .list_for_domain(domain)
            .into_iter()
            .map(|a| {
                let snap = a.collector.snapshot();
                crate::cases::document::CollectorLine {
                    name: a.collector.def().name.clone(),
                    owner: a.owner.to_string(),
                    matched: snap.total.count,
                    snapshots: a.snapshot_count,
                    latest_snapshot: a.latest_snapshot,
                    zeroed_by: a.zeroed_by,
                }
            })
            .collect();
        lines.sort_by(|a, b| a.name.cmp(&b.name));
        lines
    }

    /// `spans.export` — every span whose seq lies in an inclusive range (§6.2).
    ///
    /// **The range is lowered into the filter by the same `lower_seq_range` that
    /// `logs.export` uses**, and `matches_span` already understands
    /// `Qualifier::SeqFilter`. So the inclusive semantics, the saturating ±1 and
    /// the eviction detection are shared with the log path by construction —
    /// not written twice and kept in step.
    ///
    /// What is deliberately *not* shared is retention. The span ring evicts
    /// independently of the log pipeline, so the verdict here is computed
    /// against the span store's own `oldest_seq`. A log window reported complete
    /// says nothing about whether the spans over that same range survived.
    fn handle_spans_export(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let count = opt_usize(params, "count")?.unwrap_or(usize::MAX);
        let filter_str = opt_str(params, "filter")?;
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            // A cursor is read-and-advance, so exporting through one would move
            // the caller's read position as a side effect of gathering
            // evidence. Refused for the reason `traces.slow` refuses it.
            return Err("cursor qualifier not permitted in spans.export".to_string());
        }
        let resolved = lower_seq_range(
            resolved,
            opt_u64(params, "from_seq")?,
            opt_u64(params, "to_seq")?,
        );

        // `for_each_matching` returns the TOTAL matched, so `capped` is exact
        // here rather than probed as it must be on the log path — the store
        // tells us how many there were, not merely how many it handed back.
        let mut spans = Vec::new();
        let matched = d.span_store.for_each_matching(resolved.as_ref(), |s| {
            if spans.len() < count {
                spans.push(s.clone());
            }
        });

        let oldest = d.span_store.oldest_seq();
        let evicted_before_window = resolved
            .as_ref()
            .and_then(|f| crate::filter::parser::evicted_before_window(f, oldest));

        Ok(json!({
            "spans": spans,
            "count": spans.len(),
            "matched": matched,
            "capped": matched > count,
            "buffer_oldest_seq": oldest,
            "buffer_newest_seq": d.span_store.newest_seq(),
            "truncated": evicted_before_window.is_some(),
            "evicted_before_window": evicted_before_window,
        }))
    }

    // -----------------------------------------------------------------------
    // session.*
    // -----------------------------------------------------------------------
    //
    // Session methods intentionally span domains — config is global, only data
    // is partitioned (spec §2). They resolve no domain.

    fn handle_sessions_list(&self) -> Result<Value, String> {
        let sessions = self.sessions.list();
        let items: Vec<Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id.to_string(),
                    "name": s.name,
                    "connected": s.connected,
                    "trigger_count": s.trigger_count,
                    "filter_count": s.filter_count,
                    "queue_size": s.queue_size,
                    "last_seen_secs_ago": s.last_seen_secs_ago,
                    "client_info": s.client_info,
                })
            })
            .collect();
        Ok(json!({ "sessions": items }))
    }

    fn handle_sessions_drop(&self, params: &Value) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        // Collectors first, and unconditionally. They are owned by the session
        // and hold a slice of a daemon-wide reservation that only four
        // default-sized collectors fit inside, so a dropped session that kept
        // them would take that budget to the grave.
        //
        // Ordered BEFORE the session drop on purpose: releasing after it meant
        // the `?` aborted on a session that no longer existed, which is exactly
        // the state a restored-after-crash collector is in — the one case where
        // this call is the only way to reclaim the budget at all.
        let collectors = self
            .collectors
            .drop_session(&SessionId::Named(name.to_string()));
        let session_result = self.sessions.drop_session(name);
        if collectors == 0 {
            // Nothing was reclaimed, so a missing session really is an error
            // worth reporting rather than a no-op dressed as success.
            session_result.map_err(|e| e.to_string())?;
        }
        Ok(json!({ "dropped": name, "collectors_released": collectors }))
    }

    // -----------------------------------------------------------------------
    // bookmarks.*
    // -----------------------------------------------------------------------

    fn handle_bookmarks_add(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let name = req_str(params, "name")?;
        let replace = opt_bool(params, "replace")?.unwrap_or(false);
        // `start_seq` defaults to the daemon's current seq counter — i.e.,
        // "the cursor we'd hand out right now". Cursor auto-create (later
        // task) uses 0 explicitly to mean "before all records". This was the
        // one parameter in the file that already refused a wrong-typed value;
        // it is now the rule rather than the exception.
        let start_seq = opt_u64(params, "start_seq")?.unwrap_or_else(|| d.pipeline.current_seq());
        let description = opt_str(params, "description")?;

        // Sweep before adding so the store stays tidy.
        self.sweep_bookmarks(&d);

        let session = session_id.to_string();
        let (bookmark, replaced) = d
            .bookmarks
            .add(&session, name, start_seq, description, replace)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "qualified_name": bookmark.qualified_name,
            "seq": bookmark.seq,
            "replaced": replaced,
        }))
    }

    fn handle_bookmarks_list(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        self.sweep_bookmarks(&d);
        let session_filter = opt_str(params, "session")?;

        let items: Vec<Value> = d
            .bookmarks
            .list()
            .into_iter()
            .filter(|b| session_filter.is_none_or(|s| b.session == s))
            .map(|b| {
                json!({
                    // The qualified ("session/bookmark") name carries both the
                    // session and the bare name; clients can split if needed.
                    "qualified_name": b.qualified_name,
                    "seq": b.seq,
                    "created_at": b.created_at.to_rfc3339(),
                    "description": b.description,
                })
            })
            .collect();

        Ok(json!({ "bookmarks": items, "count": items.len() }))
    }

    fn handle_bookmarks_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let name = req_str(params, "name")?;
        let qualified = crate::store::bookmarks::qualify(name, &session_id.to_string());
        d.bookmarks.remove(&qualified).map_err(|e| e.to_string())?;
        Ok(json!({ "removed": qualified }))
    }

    fn handle_bookmarks_clear(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        // Default to the calling session if no explicit session is given — which
        // is why a wrong-typed one cannot be read as absent: it would clear the
        // CALLER's bookmarks in place of the ones they named.
        let session = opt_str(params, "session")?
            .map(str::to_string)
            .unwrap_or_else(|| session_id.to_string());
        let removed_count = d.bookmarks.clear_session(&session);
        Ok(json!({ "removed_count": removed_count, "session": session }))
    }

    /// Clear all bookmarks for a given session across EVERY domain it has been
    /// bound to over its lifetime (§5), not just its current binding — an
    /// anonymous session that switched domains may hold bookmarks in more than
    /// one store. Used when anonymous sessions disconnect to drop their ephemeral
    /// bookmarks. (Named sessions preserve bookmarks via snapshot.) A domain that
    /// has since been deleted is skipped (its bookmarks went with it).
    pub fn clear_session_bookmarks(&self, session_id: &SessionId) -> usize {
        let session_key = session_id.to_string();
        let mut removed = 0;
        for domain_id in self.sessions.touched_domains(session_id) {
            if let Some(d) = self.domains.get(&domain_id) {
                removed += d.bookmarks.clear_session(&session_key);
            }
        }
        removed
    }

    /// Release a session's collectors and the sample budget they reserved.
    ///
    /// Deliberately **not** called when a named session merely disconnects.
    /// Arming a collector, running a workload, and reading the result later is
    /// the whole point of the feature, and a CLI invocation disconnects between
    /// every one of those steps. Named sessions therefore keep their collectors
    /// exactly as they keep their bookmarks; the TTL sweep and an explicit
    /// `sessions.drop` are what bound the reservation.
    pub fn clear_session_collectors(&self, session_id: &SessionId) -> usize {
        self.collectors.drop_session(session_id)
    }

    fn sweep_bookmarks(&self, d: &Domain) {
        let oldest_log = d.pipeline.oldest_log_seq();
        let oldest_span = d.span_store.oldest_seq();
        d.bookmarks.sweep(oldest_log, oldest_span);
    }

    // -----------------------------------------------------------------------
    // collectors.* (spec §7)
    // -----------------------------------------------------------------------

    fn handle_collectors_add(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        if !is_valid_collector_name(name) {
            return Err(format!(
                "invalid collector name `{name}`: use letters, digits, `_` and `-` only. \
                 Collector names appear in filenames and in `collector@label`, so `/`, `..` \
                 and `@` cannot be permitted"
            ));
        }
        // §10: an anonymous session cannot own a collector. Its name is a UUID
        // that will never be presented again, so the collector would be
        // unreachable the moment the session disconnected — holding a slice of
        // the daemon-wide reservation that nothing could ever release.
        if matches!(session_id, SessionId::Anonymous(_)) {
            return Err(
                "collectors require a named session: they outlive a disconnect, and \
                        an anonymous session's identity does not. Call session.start with a \
                        name, or use traces.profile for a one-off measurement"
                    .to_string(),
            );
        }
        let filter_string = req_str(params, "filter")?;
        let d = self.resolve_domain(session_id)?;

        let parsed = crate::filter::parser::parse_filter(filter_string)
            .map_err(|e| format!("invalid filter: {e}"))?;
        let warnings: Vec<String> = crate::filter::admission::admit_span_filter(&parsed)
            .map_err(|e| e.to_string())?
            .warnings
            .iter()
            .map(|w| w.to_string())
            .collect();

        let level = match opt_str(params, "level")? {
            None => Level::Tree,
            Some("scalar") => Level::Scalar,
            Some("timing") => Level::Timing,
            Some("tree") => Level::Tree,
            Some(other) => {
                return Err(format!(
                    "unknown level `{other}`: expected `scalar`, `timing` or `tree`"
                ))
            }
        };
        let group_keys = group_keys_of(params)?.unwrap_or_default();
        let max_sample_bytes =
            opt_usize(params, "max_sample_bytes")?.unwrap_or(DEFAULT_MAX_SAMPLE_BYTES);

        // Validated against the group keys resolved above, so a threshold naming
        // a group the collector does not split by is refused at arm time rather
        // than silently never matching.
        let (threshold, threshold_warnings) = threshold_of(params, &group_keys)?;
        let mut warnings = warnings;
        warnings.extend(threshold_warnings);

        let def = CollectorDef {
            name: name.to_string(),
            filter_string: filter_string.to_string(),
            filter: parsed,
            level,
            group_keys,
            max_sample_bytes,
            description: opt_str(params, "description")?.map(str::to_string),
            threshold: threshold.flatten(),
        };

        let domain_id = self.sessions.domain_of(session_id);
        let collector = self
            .collectors
            .add(session_id, &domain_id, d.metrics.clone(), def, Utc::now())
            .map_err(|e| e.to_string())?;

        // §3.8's shorthand, applied only once the collector is actually armed:
        // a call that returns an error must not leave provenance behind that
        // its caller never saw an outcome for. Absent — or an explicit null —
        // means no entries, which is this crate's rule for every optional
        // parameter; `domain_data.update` keeps requiring its own `entries`.
        let (data_outcomes, data_persist_error) = match params.get("data") {
            Some(v) if !v.is_null() => self.apply_data_entries(session_id, Some(v))?,
            _ => (Vec::new(), None),
        };

        let mut result = json!({
            "name": collector.def().name,
            "filter": collector.def().filter_string,
            "level": collector.def().level.as_str(),
            "group_keys": collector.def().group_keys,
            "domain": domain_id.to_string(),
            "max_sample_bytes": collector.def().max_sample_bytes,
            // Never a refusal — every one of these is a filter that will
            // collect something, just perhaps not what the caller meant.
            // Returned on `add` because that is the moment the caller can
            // still cheaply change their mind.
            "warnings": warnings,
        });
        // Added only when there is something to say, because the schema
        // declares both as skippable and a reply that always carries an empty
        // list and a null teaches a reader that `data` did something when it
        // did nothing.
        if !data_outcomes.is_empty() {
            result["data_outcomes"] = serde_json::to_value(&data_outcomes).unwrap_or_default();
        }
        if let Some(e) = data_persist_error {
            result["data_persist_error"] = json!(e);
        }
        Ok(result)
    }

    fn handle_collectors_list(&self, session_id: &SessionId) -> Result<Value, String> {
        let items: Vec<Value> = self
            .collectors
            .list(session_id)
            .iter()
            .map(|a| {
                let snap = a.collector.snapshot();
                // §10: the orphan check is LAZY, evaluated here rather than at
                // restore. Session restore runs long before the domain
                // registry is built, so a check at that point would mark every
                // collector orphaned — including those pinned to `default`.
                let orphaned = self.domains.get(&a.domain).is_none();
                json!({
                    "name": a.collector.def().name,
                    "filter": a.collector.def().filter_string,
                    "level": a.collector.def().level.as_str(),
                    "group_keys": a.collector.def().group_keys,
                    "description": a.collector.def().description,
                    "domain": a.domain.to_string(),
                    "matched": snap.total.count,
                    "armed_at": snap.armed_at,
                    "zeroed_at": snap.zeroed_at,
                    "zeroed_by": a.zeroed_by,
                    "snapshots": a.snapshot_count,
                    "sample_complete": snap.samples.complete,
                    "orphaned": orphaned,
                    "orphan_note": orphaned.then_some(
                        "the pinned domain no longer exists, so this collector can never \
                         match another span. Ephemeral domains are not re-created at boot, \
                         so this is the normal outcome of a restart for a collector armed \
                         on one. Re-pin it with collectors.edit, or remove it."
                    ),
                    "threshold": a.collector.threshold().as_ref().map(threshold_json),
                })
            })
            .collect();
        Ok(json!({
            "collectors": items,
            "count": items.len(),
            "reserved_bytes": self.collectors.reserved_bytes(),
        }))
    }

    fn handle_collectors_get(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        // A label reads a recorded run. Served from what the snapshot stored
        // rather than re-projected, because the samples it was projected from
        // are gone — and re-deriving from the live collector would answer a
        // different question with the right-looking shape.
        // Parsed BEFORE the snapshot branch on purpose. Reading the raw params
        // there instead would let a typo like `group_by: "NAME"` hard-error on a
        // live read and pass silently on a recorded one — the same parameter,
        // the same mistake, two different outcomes, and the quiet one is the
        // failure this whole surface exists to close.
        let opts = self.profile_options(params)?;
        if let Some(label) = opt_str(params, "snapshot")? {
            let snap = self
                .collectors
                .get_snapshot(session_id, name, label)
                .map_err(|e| e.to_string())?;
            return Ok(snapshot_as_profile(&snap, &opts));
        }
        let armed = self
            .collectors
            .get(session_id, name)
            .ok_or_else(|| format!("no collector named `{name}` in this session"))?;
        let result = crate::collector::project::profile(
            &armed.collector.snapshot(),
            self.ingest_basis(&armed),
            &opts,
            Utc::now(),
        );
        let mut out = serde_json::to_value(result).map_err(|e| e.to_string())?;
        // Only on a live read. A rolling window is live state, so a recorded run
        // has none — and reporting the LIVE verdict beside a recorded run's
        // numbers would attach a fact about now to a window that closed.
        if let Some(t) = armed.collector.threshold() {
            out["threshold"] = threshold_json(&t);
        }
        Ok(out)
    }

    fn handle_collectors_edit(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        let level = match opt_str(params, "level")? {
            None => None,
            Some("scalar") => Some(Level::Scalar),
            Some("timing") => Some(Level::Timing),
            Some("tree") => Some(Level::Tree),
            Some(other) => {
                return Err(format!(
                    "unknown level `{other}`: expected `scalar`, `timing` or `tree`"
                ))
            }
        };
        // A filter is admitted here on exactly the terms `add` uses. §7.1's
        // rule is "re-run every gate add runs", and admission is one of them —
        // an edit that skipped it could arm a filter `add` would have refused.
        let mut warnings: Vec<String> = Vec::new();
        let new_filter = opt_str(params, "filter")?;
        if let Some(f) = new_filter {
            let parsed = crate::filter::parser::parse_filter(f)
                .map_err(|e| format!("invalid filter: {e}"))?;
            warnings = crate::filter::admission::admit_span_filter(&parsed)
                .map_err(|e| e.to_string())?
                .warnings
                .iter()
                .map(|w| w.to_string())
                .collect();
        }
        // Resolve the domain to its live instance, and carry its METRICS across
        // with the pin. The identity check that decides whether ingest loss can
        // be attributed is pointer equality on this handle, so a re-pin that
        // moved the name but not the counters would report "the pinned domain
        // is gone" about a domain it had just successfully re-pinned to.
        let (domain, new_metrics) = match opt_str(params, "domain")? {
            None => (None, None),
            Some(d) => {
                let id = DomainId::new(d).map_err(|e| e.to_string())?;
                let Some(dom) = self.domains.get(&id) else {
                    return Err(format!("domain \"{d}\" does not exist"));
                };
                (Some(id), Some(dom.metrics.clone()))
            }
        };

        // Present-or-absent matters here (absent means "leave alone"), and when
        // present it goes through the same width check as `add` — §7.1 requires
        // an edit to re-run every gate arming runs, and this is one. Read ONCE
        // and reused below: a second read would have to agree with this one
        // about what "present" means, and there is no reason to give it the
        // chance to disagree.
        let group_keys = group_keys_of(params)?;
        // Against the group keys that will be IN FORCE after this edit, not the
        // ones currently armed: a caller adding a group key and a threshold that
        // uses it in one call must not be refused for a state neither before nor
        // after the edit.
        let effective_group_keys = match &group_keys {
            Some(k) => k.clone(),
            None => self
                .collectors
                .get(session_id, name)
                .map(|a| a.collector.def().group_keys.clone())
                .unwrap_or_default(),
        };
        let (threshold, threshold_warnings) = threshold_of(params, &effective_group_keys)?;
        warnings.extend(threshold_warnings);

        let change = CollectorEdit {
            description: opt_str(params, "description")?.map(str::to_string),
            filter: new_filter.map(str::to_string),
            level,
            group_keys,
            max_sample_bytes: opt_usize(params, "max_sample_bytes")?,
            domain,
            threshold,
        };
        if change.is_empty() {
            return Err(
                "nothing to edit: pass at least one of description, filter, level, \
                        group_keys, max_sample_bytes, domain or threshold"
                    .to_string(),
            );
        }

        let out = self
            .collectors
            .edit(session_id, name, change, new_metrics, Utc::now())
            .map_err(|e| e.to_string())?;
        let def = out.collector.def();
        Ok(json!({
            "name": def.name,
            "filter": def.filter_string,
            "level": def.level.as_str(),
            "group_keys": def.group_keys,
            "description": def.description,
            "domain": out.domain.to_string(),
            "max_sample_bytes": def.max_sample_bytes,
            // Stated plainly, because it is the whole consequence of the edit:
            // a structural change discards the live window. History never.
            "zeroed": out.zeroed,
            "note": if out.zeroed {
                "the live window was discarded; recorded snapshots are untouched and each \
                 keeps the definition it was taken under"
            } else {
                "nothing was discarded"
            },
            "warnings": warnings,
        }))
    }

    fn handle_collectors_snapshot(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        let policy = SnapshotPolicy {
            per_name: opt_bool(params, "per_name")?.unwrap_or(true),
            per_group: opt_bool(params, "per_group")?.unwrap_or(true),
            projections: opt_bool(params, "projections")?.unwrap_or(true),
        };
        let snap = self
            .collectors
            .snapshot(
                session_id,
                SnapshotRequest {
                    name: name.to_string(),
                    label: opt_str(params, "label")?.map(str::to_string),
                    description: opt_str(params, "description")?.map(str::to_string),
                    // `meta` is deliberately untyped — any JSON the caller wants
                    // recorded alongside the run — so there is no wrong type to
                    // reject, and absence really is the only default.
                    meta: params.get("meta").cloned().unwrap_or(Value::Null),
                    policy,
                    // Reset by default: the point of a snapshot is usually to
                    // end one run and begin the next in a single call, which is
                    // what makes the between-runs step one action rather than
                    // two that traffic can be interleaved between. It is also
                    // why a wrong-typed `reset` cannot read as absent — the
                    // default here DESTROYS the live window.
                    reset: opt_bool(params, "reset")?.unwrap_or(true),
                    now: Utc::now(),
                },
                crate::collector::project::project_for_snapshot,
            )
            .map_err(|e| e.to_string())?;
        let mut out = snapshot_summary(&snap.snapshot);
        if let Some(e) = snap.persist_error {
            // Surfaced on the successful response, not raised as an error: the
            // run WAS recorded and is in the response. What failed is that it
            // will not survive a restart, and a caller collecting runs to
            // compare later is exactly who needs to know that.
            out["durable"] = json!(false);
            out["durability_warning"] = json!(format!(
                "this run is in memory and in the response, but could not be written to \
                 disk, so it will not survive a daemon restart: {e}"
            ));
        }
        Ok(out)
    }

    fn handle_collectors_history(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        let (mut snapshots, evicted) = self
            .collectors
            .history(session_id, name)
            .map_err(|e| e.to_string())?;
        // `limit` takes the most RECENT n, then they are presented oldest
        // first — the order a sequence of runs is read in.
        if let Some(limit) = opt_usize(params, "limit")? {
            if snapshots.len() > limit {
                snapshots.drain(..snapshots.len() - limit);
            }
        }

        let mut result = json!({
            "collector": name,
            "snapshots": snapshots.iter().map(snapshot_summary).collect::<Vec<_>>(),
            "count": snapshots.len(),
            "evicted": evicted,
        });

        if opt_bool(params, "merge")?.unwrap_or(false) {
            let mut suppressed: Vec<Value> = Vec::new();
            match crate::collector::history::merge(&snapshots) {
                Ok(m) => {
                    result["merged"] = exact_json(&m);
                    result["merged_estimated"] = estimated_json(&m, "collector");
                    // Stated every time, not only when it bites: someone
                    // merging runs is looking for a total, and self time or a
                    // wall union summed across runs would be a number with no
                    // meaning rather than an approximate one.
                    suppressed.push(json!({
                        "field": "merged.sampled",
                        "reason": "sample-derived figures do not merge — self time and a \
                                   wall-clock union across separate runs are not a self \
                                   time and not a union",
                        "remedy": "read the per-snapshot `sampled` blocks, or compare two \
                                   runs directly rather than merging them",
                    }));
                }
                Err(e) => suppressed.push(json!({
                    "field": "merged",
                    "reason": e.to_string(),
                })),
            }
            if let Some(f) = RunToRunFloor::over_total_ms(&snapshots) {
                result["floor"] = json!({
                    "metric": f.metric,
                    "runs": f.runs,
                    "min": f.min,
                    "max": f.max,
                    "mean": f.mean,
                    "cv_pct": f.cv_pct,
                    "caveat": RunToRunFloor::CAVEAT,
                });
            }
            result["suppressed"] = json!(suppressed);
        }
        Ok(result)
    }

    /// `collectors.diff` — compare two arms (§5.6).
    fn handle_collectors_diff(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        use crate::collector::diff::{diff, DiffAllowances, DiffGroupBy, DiffOptions};

        let a = self.resolve_arm(session_id, req_str(params, "a")?)?;
        let b = self.resolve_arm(session_id, req_str(params, "b")?)?;

        let group_by = match opt_str(params, "group_by")? {
            None => DiffGroupBy::None,
            Some(s) => DiffGroupBy::parse(s).ok_or_else(|| {
                format!(
                    "unknown group_by `{s}` for a diff: expected {}. `trace` and `path` are \
                     projected from per-span records, which a recorded run does not retain, \
                     so there is nothing to compare",
                    DiffGroupBy::accepted()
                )
            })?,
        };
        let flag = |k: &str| opt_bool(params, k).map(|b| b.unwrap_or(false));
        let opts = DiffOptions {
            group_by,
            top_n: opt_usize(params, "top_n")?.unwrap_or(20),
            allow: DiffAllowances {
                mismatch: flag("allow_mismatch")?,
                lossy: flag("allow_lossy")?,
                truncated: flag("allow_truncated")?,
            },
        };

        let result = diff(a, b, &opts).map_err(|e| e.to_string())?;
        serde_json::to_value(result.to_wire()).map_err(|e| e.to_string())
    }

    /// `collectors.document` — render a measurement for a reader (§9).
    fn handle_collectors_document(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        use crate::collector::diff::{DiffAllowances, DiffGroupBy};
        use crate::collector::document::{document, DocumentOptions, Format};

        let names = opt_str_array(params, "names")?.unwrap_or_default();
        if names.is_empty() {
            return Err(
                "missing required parameter: names — the arms to document, first one the \
                 baseline. An arm is `<collector>`, `<collector>@<label>`, or \
                 `<collector>@*` for every recorded run merged"
                    .to_string(),
            );
        }
        // Bounded because each arm is resolved and held in memory at once, and a
        // merged arm carries the union of its runs' per-axis maps. Refused with
        // the number rather than silently truncated: a caller who asked for
        // twenty arms wants to know they did not get them.
        const MAX_ARMS: usize = 8;
        if names.len() > MAX_ARMS {
            return Err(format!(
                "too many arms ({} > {MAX_ARMS}): every arm is resolved and held at once, \
                 and a document comparing more than a handful against one baseline is not \
                 a table anyone reads. Document them in groups",
                names.len()
            ));
        }

        let format = match opt_str(params, "format")? {
            None => Format::Md,
            Some(s) => Format::parse(s).ok_or_else(|| {
                format!("unknown format `{s}`: expected `md`, `json` or `folded`")
            })?,
        };
        let group_by = match opt_str(params, "group_by")? {
            None => DiffGroupBy::Name,
            Some(s) => DiffGroupBy::parse(s).ok_or_else(|| {
                format!(
                    "unknown group_by `{s}`: expected {}",
                    DiffGroupBy::accepted()
                )
            })?,
        };
        // The prose fields are the point of a document. Dropping a wrong-typed
        // `finding` silently would publish a measurement with the conclusion
        // missing and nothing in the output to say one was sent.
        let text = |k: &str| opt_str(params, k).map(|s| s.map(str::to_string));
        let flag = |k: &str| opt_bool(params, k).map(|b| b.unwrap_or(false));

        let opts = DocumentOptions {
            format,
            question: text("question")?,
            finding: text("finding")?,
            filter_intent: text("filter_intent")?,
            correctness_evidence: text("correctness_evidence")?,
            group_by,
            top_n: opt_usize(params, "top_n")?.unwrap_or(15),
            allow: DiffAllowances {
                mismatch: flag("allow_mismatch")?,
                lossy: flag("allow_lossy")?,
                truncated: flag("allow_truncated")?,
            },
        };

        let mut arms = Vec::with_capacity(names.len());
        for spec in &names {
            arms.push(self.resolve_arm(session_id, spec)?);
        }

        let doc = document(&arms, &opts, Utc::now()).map_err(|e| e.to_string())?;
        Ok(json!({
            "content": doc.content,
            "format": doc.format,
            "bytes": doc.bytes,
            "sidecar_name": doc.sidecar.as_ref().map(|s| s.name.clone()),
            "sidecar_content": doc.sidecar.as_ref().map(|s| s.content.clone()),
            "warnings": doc.warnings,
        }))
    }

    /// Resolve `<collector>`, `<collector>@<label>`, or `<collector>@*` into an
    /// arm.
    ///
    /// `*` cannot be a valid snapshot label (`is_valid_label` admits only
    /// `[A-Za-z0-9._-]`), so the wildcard can never collide with a real one —
    /// which is why it is spelled this way rather than as a separate parameter.
    fn resolve_arm(
        &self,
        session_id: &SessionId,
        spec: &str,
    ) -> Result<crate::collector::diff::Arm, String> {
        use crate::collector::diff::Arm;

        let (name, selector) = match spec.split_once('@') {
            None => (spec, None),
            Some((n, label)) => (n, Some(label)),
        };
        if name.is_empty() {
            return Err(format!(
                "`{spec}` names no collector: use `<collector>`, `<collector>@<label>`, or \
                 `<collector>@*` for every recorded run merged"
            ));
        }

        match selector {
            // The live window. Its ingest basis is resolved exactly as
            // `collectors.get` resolves it — pointer equality on the domain's
            // metrics handle, so a domain deleted and recreated under the
            // collector reads as unknown rather than as zero loss.
            None => {
                let armed = self
                    .collectors
                    .get(session_id, name)
                    .ok_or_else(|| format!("no collector named `{name}` in this session"))?;
                let ingest = match self.ingest_basis(&armed) {
                    crate::collector::project::IngestBasis::Same { baseline, current } => {
                        Some(current.since(baseline))
                    }
                    _ => None,
                };
                Ok(Arm::from_live(
                    spec.to_string(),
                    &armed.collector.snapshot(),
                    ingest,
                    Utc::now(),
                ))
            }
            Some("*") => {
                let (snapshots, _evicted) = self
                    .collectors
                    .history(session_id, name)
                    .map_err(|e| e.to_string())?;
                if snapshots.is_empty() {
                    return Err(format!(
                        "collector `{name}` has no recorded runs to merge. Take one with \
                         collectors.snapshot, or name the live window as `{name}`"
                    ));
                }
                Arm::merged(spec.to_string(), &snapshots).map_err(|e| e.to_string())
            }
            Some(label) => {
                let s = self
                    .collectors
                    .get_snapshot(session_id, name, label)
                    .map_err(|e| e.to_string())?;
                Ok(Arm::from_snapshot(spec.to_string(), &s))
            }
        }
    }

    fn handle_collectors_reset(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        let taken = self
            .collectors
            .reset(session_id, name, Utc::now())
            .map_err(|e| e.to_string())?;
        // The discarded run is summarised rather than dropped silently: a
        // reset issued a moment too early is otherwise unrecoverable, and the
        // headline numbers are cheap to hand back.
        Ok(json!({
            "name": name,
            "discarded": {
                "matched": taken.total.count,
                "total_ms": crate::collector::exact::ns_to_ms(taken.total.total_ns),
                "error_count": taken.total.error_count,
                "window_start": taken.window_start(),
            },
        }))
    }

    fn handle_collectors_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let name = req_str(params, "name")?;
        self.collectors
            .remove(session_id, name)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "removed": name,
            "reserved_bytes": self.collectors.reserved_bytes(),
        }))
    }

    /// `traces.profile` — the same projections over an ad-hoc query rather than
    /// an armed collector.
    ///
    /// It answers a question the collector cannot: what the spans *already in
    /// the buffer* look like. A collector must be armed before the run it
    /// measures; this reads what is there now, bounded by the span store's ring
    /// rather than by a sample budget.
    fn handle_traces_profile(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let filter_string = opt_str(params, "filter")?.unwrap_or("ALL");
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(Some(filter_string), session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            // A cursor is read-and-advance, so a second identical `profile`
            // call would return less than the first. A profile must be
            // repeatable.
            return Err("cursor qualifier not permitted in traces.profile".to_string());
        }
        let parsed = resolved.unwrap_or(crate::filter::parser::ParsedFilter::All);
        let warnings: Vec<String> = crate::filter::admission::admit_span_filter(&parsed)
            .map_err(|e| e.to_string())?
            .warnings
            .iter()
            .map(|w| w.to_string())
            .collect();

        // Checked here too, and this is the widest surface of the three: the
        // throwaway collector below is built directly, so it never passes
        // through the registry's reservation gate at all.
        let group_keys = group_keys_of(params)?.unwrap_or_default();

        // A throwaway collector fed the matching spans, so the ad-hoc path and
        // the armed path share one projection implementation rather than two
        // that can disagree.
        let def = CollectorDef {
            name: "ad-hoc".into(),
            filter_string: filter_string.to_string(),
            filter: parsed.clone(),
            level: Level::Tree,
            group_keys,
            max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
            description: None,
            // No threshold on an ad-hoc profile: a rolling guard needs a window
            // it was present for, and this reads spans that already arrived.
            threshold: None,
        };
        let scratch = Collector::new(def, Utc::now());
        let matched = d
            .span_store
            .for_each_matching(Some(&parsed), |s| scratch.ingest(s));

        let opts = self.profile_options(params)?;
        let mut result = crate::collector::project::profile(
            &scratch.snapshot(),
            crate::collector::project::IngestBasis::NoWindow,
            &opts,
            Utc::now(),
        );
        result.collector = None;
        result.description = Some(format!(
            "ad-hoc profile over {matched} spans currently in the `{}` span store",
            d.config.name
        ));
        result.warnings = warnings;
        serde_json::to_value(result).map_err(|e| e.to_string())
    }

    fn profile_options(&self, params: &Value) -> Result<ProfileOptions, String> {
        let group_by = match opt_str(params, "group_by")? {
            None => GroupBy::None,
            Some(s) => GroupBy::parse(s).ok_or_else(|| {
                format!("unknown group_by `{s}`: expected {}", GroupBy::accepted())
            })?,
        };
        Ok(ProfileOptions {
            group_by,
            skip_warmup_ms: opt_f64(params, "skip_warmup_ms")?,
            top_n: opt_usize(params, "top_n")?.unwrap_or(20),
        })
    }

    /// Whether the collector's ingest baseline and the domain's current
    /// counters share an origin.
    ///
    /// Pointer equality on the `Arc`, not a name comparison: a domain deleted
    /// and recreated under the same name gets fresh counters starting at zero,
    /// and a delta against the old baseline would be arithmetic on two
    /// unrelated sequences.
    fn ingest_basis(&self, armed: &ArmedCollector) -> crate::collector::project::IngestBasis {
        use crate::collector::project::IngestBasis;
        match self.domains.get(&armed.domain) {
            Some(d) if Arc::ptr_eq(&d.metrics, &armed.metrics) => IngestBasis::Same {
                baseline: armed.ingest_baseline,
                current: d.metrics.trace_ingest_loss(),
            },
            _ => IngestBasis::Unavailable,
        }
    }
}

fn exact_json(e: &crate::collector::exact::ExactStats) -> Value {
    json!({
        "count": e.count,
        "total_ms": crate::collector::exact::ns_to_ms(e.total_ns),
        "avg_ms": e.avg_ns().map(|ns| ns / 1_000_000.0),
        "min_ms": e.min_ns.map(|ns| ns as f64 / 1_000_000.0),
        "max_ms": e.max_ns.map(|ns| ns as f64 / 1_000_000.0),
        "error_count": e.error_count,
        "out_of_range_spans": e.out_of_range_spans,
    })
}

fn estimated_json(e: &crate::collector::exact::ExactStats, axis: &str) -> Value {
    let q = |p: f64| e.quantile_ns(p).map(|ns| ns / 1_000_000.0);
    json!({
        "axis": axis,
        "alpha_pct": crate::collector::sketch::ALPHA * 100.0,
        "p50_ms": q(0.50),
        "p80_ms": q(0.80),
        "p95_ms": q(0.95),
        "p99_ms": q(0.99),
    })
}

/// A recorded run on the wire.
///
/// Reports the definition **from the snapshot**, never from the live
/// collector — that is the whole reason §6.3 makes a snapshot carry its own.
fn snapshot_summary(s: &StoredSnapshot) -> Value {
    // The 50-duration cap was budgeted for ONE `sampled` block. This is a list
    // view -- `collectors.history` returns every retained run, up to 50 of them,
    // so carrying the durations here would multiply one bounded list into 2500
    // floats nobody asked for, straight into a reader's context. The single-run
    // read (`collectors.get` with a `snapshot` label) still serves them.
    let sampled = s.projections.as_ref().map(|p| ProfileSampled {
        durations_ms: None,
        ..p.clone()
    });
    json!({
        "label": s.label,
        "description": s.description,
        "meta": s.meta,
        "taken_at": s.taken_at,
        "window_start": s.window_start,
        "wall_ms": s.wall_ms,
        "filter": s.def.filter_string,
        "level": s.def.level.as_str(),
        "group_keys": s.def.group_keys,
        "exact": exact_json(&s.total),
        "estimated": estimated_json(&s.total, "collector"),
        "sampled": sampled,
        "ingest": s.ingest.map(|i| json!({
            "drops_in_window": i.dropped,
            "shed_batches": i.shed_batches,
            "malformed_dropped": i.malformed,
            "malformed_timestamps": s.total.malformed_timestamps,
            "negative_duration_spans": s.total.negative_duration_spans,
            "attribution": "domain",
        })),
        "sample_complete": s.sample_complete,
        "cardinality_capped": s.cardinality_capped,
    })
}

/// A recorded run in the same shape as a live read.
///
/// `collectors.get` returns one type whether it is reading the live window or
/// a snapshot — a recorded run *is* a profile, of a window that has closed.
/// Two shapes would mean every client branches on a parameter it passed, and
/// the typed SDK could not name a return type at all.
/// `opts` is read only to report what a recorded run could not honour. A
/// snapshot is served from what it stored, so every read-shaping option arrives
/// after the shaping moment has passed — and handing back numbers that ignored
/// one, with nothing in the shape to say so, is the failure the suppression
/// channel exists to prevent. Taking the parsed options rather than the raw
/// params is what keeps an unparseable value an error here as it is on a live
/// read, instead of a silent no-op.
fn snapshot_as_profile(s: &StoredSnapshot, opts: &ProfileOptions) -> Value {
    let nesting = match (&s.projections, s.def.level.has_tree()) {
        (Some(p), true) if p.nested_matches > 0 => "detected",
        (Some(_), true) => "undetected",
        // No stored projection, or a level that never had parent identity:
        // the question was not asked, and saying "undetected" would let a
        // reader infer a flat call structure from a recording policy.
        _ => "unknown",
    };
    let mut suppressed: Vec<Value> = Vec::new();
    if s.projections.is_none() && s.def.level.has_samples() {
        suppressed.push(json!({
            "field": "sampled",
            "reason": "this snapshot was recorded with projections disabled, and the \
                       samples it would have been projected from are not retained",
            "remedy": "take the next snapshot with projections: true",
        }));
    }
    if opts.skip_warmup_ms.is_some_and(|ms| ms > 0.0) {
        suppressed.push(json!({
            "field": "excluded_by_warmup",
            "reason": "skip_warmup_ms cannot apply to a recorded run: the projection \
                       was computed when the snapshot was taken, and the per-span \
                       records it was computed from are not retained. These numbers \
                       cover the whole recorded window, warm-up included",
            "remedy": "pass skip_warmup_ms on a live read, or reset the collector \
                       after warm-up so the next snapshot records a clean window",
        }));
    }
    // `GroupBy::None` rather than mere absence: `group_by: ""` is a valid way to
    // ask for no grouping, and reporting a suppression for a request nobody made
    // is the same class of lie as staying silent about one that was ignored.
    if opts.group_by != GroupBy::None {
        suppressed.push(json!({
            "field": "groups",
            "reason": "a recorded run is served from what it stored, and this read \
                       path projects no grouping from it",
            // Every shorter remedy here has been wrong. "Use collectors.diff"
            // holds only for `name`/`group`, only between two runs, and only
            // until a restart drops the per-axis breakdowns from disk. "Group on
            // a live read" is the general answer but collides with the warm-up
            // rule if the caller also passes skip_warmup_ms. And a snapshot DOES
            // retain top call paths, which `collectors.document` renders -- so
            // claiming nothing is stored was false for `path`. Each axis gets
            // the answer that is true for it.
            "remedy": match opts.group_by {
                GroupBy::Path => "this run's top call paths ARE stored: read them with \
                                  collectors.document, or group a live window by `path`",
                // Not "use collectors.diff", even qualified: it reads the stored
                // per-name rows only until a restart drops them from disk, and
                // advice whose validity turns on invisible daemon state is worse
                // than advice that is merely incomplete.
                GroupBy::Name | GroupBy::Group => "group a live window before \
                                  snapshotting -- a recorded run keeps its headline \
                                  tiers and projections, not a grouping this path \
                                  can project",
                _ => "group a live window before snapshotting -- per-trace rows are \
                      projected from per-span records, which a snapshot does not keep",
            },
        }));
    }
    json!({
        "collector": s.def.name,
        "description": s.description,
        "filter": s.def.filter_string,
        "level": s.def.level.as_str(),
        "matched": s.total.count,
        "nesting": nesting,
        // A closed window: it opened when the previous one was zeroed and
        // ended when the snapshot was taken.
        "window": {
            "armed_at": s.window_start,
            "read_at": s.taken_at,
            "wall_ms": s.wall_ms,
        },
        "ingest": s.ingest.map(|i| json!({
            "drops_in_window": i.dropped,
            "shed_batches": i.shed_batches,
            "malformed_dropped": i.malformed,
            "malformed_timestamps": s.total.malformed_timestamps,
            "negative_duration_spans": s.total.negative_duration_spans,
            "attribution": "domain",
        })),
        "exact": exact_json(&s.total),
        "estimated": estimated_json(&s.total, "collector"),
        "sampled": s.projections,
        "cardinality_capped": s.cardinality_capped,
        "suppressed": suppressed,
        // Not a live field, but the label is how this run is referred to
        // everywhere else, so a result that omitted it would be unquotable.
        "snapshot": s.label,
    })
}

/// Caller-supplied `group_keys`, refused if wider than the sample tier will
/// allocate for.
///
/// The width is the one dimension the byte budget cannot see: the group-key
/// column is reserved eagerly at `CHUNK_RECORDS * width * 4` bytes, so a tiny
/// declared `max_sample_bytes` with a huge key list passes every reservation
/// check and then asks for gigabytes — which `Vec::with_capacity` answers by
/// aborting the process. Refused here, with the number, rather than clamped
/// silently: a caller who asked to group by fifty things wants to know they did
/// not get it.
///
/// `None` for absent/`null`, `Some(vec![])` for an explicit empty array. The
/// distinction is load-bearing on `collectors.edit`, where an empty list is a
/// structural change that discards the live window and absence means "leave it
/// alone" — so a wrong-typed value read as an empty array zeroed a measurement
/// nobody asked to end.
fn group_keys_of(params: &Value) -> Result<Option<Vec<String>>, String> {
    let Some(keys) = opt_str_array(params, "group_keys")? else {
        return Ok(None);
    };
    if keys.len() > MAX_GROUP_KEYS {
        return Err(format!(
            "too many group_keys ({} > {MAX_GROUP_KEYS}): each one costs a column in every \
             retained sample, and the width is not covered by max_sample_bytes. Group by one \
             attribute — that is the case this is built for",
            keys.len()
        ));
    }
    Ok(Some(keys))
}

/// Parse a `threshold` parameter (§8), validating it against the collector's
/// declared group keys.
///
/// Returns `Ok(None)` when the key is absent — "leave it alone" — and
/// `Ok(Some(None))` for an explicit `null`, which removes an armed threshold.
/// A flat `Option` cannot express both, and conflating them would make it
/// impossible to clear a guard once set.
#[allow(clippy::type_complexity)]
fn threshold_of(
    params: &Value,
    group_keys: &[String],
) -> Result<
    (
        Option<Option<crate::collector::threshold::Threshold>>,
        Vec<String>,
    ),
    String,
> {
    use crate::collector::threshold::{Metric, Op, Threshold};
    let Some(v) = params.get("threshold") else {
        return Ok((None, Vec::new()));
    };
    if v.is_null() {
        return Ok((Some(None), Vec::new()));
    }
    let obj = v.as_object().ok_or_else(|| {
        "threshold must be an object: { metric, op, value, window_ms, group? }".to_string()
    })?;
    let need = |k: &str| -> Result<&Value, String> {
        obj.get(k)
            .ok_or_else(|| format!("threshold is missing `{k}`"))
    };
    let metric = Metric::parse(
        need("metric")?
            .as_str()
            .ok_or("threshold.metric must be a string")?,
    )?;
    let op = Op::parse(
        need("op")?
            .as_str()
            .ok_or("threshold.op must be a string")?,
    )?;
    let value = need("value")?
        .as_f64()
        .ok_or("threshold.value must be a number")?;
    let window_ms = need("window_ms")?
        .as_u64()
        .ok_or("threshold.window_ms must be a positive integer")?;
    let t = Threshold {
        metric,
        // A wrong-typed `group` read as absent moved the guard from the named
        // group onto the collector total — a different question, answered
        // without saying so.
        group: match obj.get("group") {
            None | Some(Value::Null) => None,
            Some(g) => Some(
                g.as_str()
                    .ok_or_else(|| {
                        format!("threshold.group must be a string, not {}", json_type(g))
                    })?
                    .to_string(),
            ),
        },
        op,
        value,
        window_ms,
    };
    let warnings = t.validate(group_keys)?;
    Ok((Some(Some(t)), warnings))
}

/// A threshold's verdict on the wire (§8).
///
/// Built through the protocol struct rather than as a `json!` literal, so the
/// `skip_serializing_if` the schema declares is the one that actually applies.
/// A hand-written literal emits `"last_value": null` where the schema promises
/// the key is absent — and `absent` versus `null` is exactly the distinction
/// this field exists to carry.
fn threshold_json(r: &crate::collector::threshold::ThresholdReport) -> Value {
    serde_json::to_value(logmon_broker_protocol::ThresholdInfo {
        metric: r.metric.to_string(),
        group: r.group.clone(),
        op: r.op.to_string(),
        value: r.value,
        window_ms: r.window_ms,
        effective_window_ms: r.effective_window_ms,
        breached: r.breached,
        fires: r.fires,
        last_value: r.last_value,
        evaluated: r.evaluated,
        note: r.note.to_string(),
    })
    .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Reading parameters — absent, `null`, and present-but-wrong-typed are three
// different things, and only two of them mean the same thing
// ---------------------------------------------------------------------------
//
// `Value::as_str` and its siblings answer `None` to two different questions —
// "was the key absent?" and "was it present with the wrong type?" — and every
// reader in this file used to treat both answers as *absent*. That is how
// `{"filter": {"expr": "level>=ERROR"}}` came to mean **no filter** and return
// the whole buffer, and how `{"reset": "false"}` came to mean **reset** and
// zero a collector's live window, each reporting success.
//
// The rule these helpers impose: **absent — or an explicit `null` — takes the
// default; a present value of the wrong type is refused, naming the parameter
// and what was expected.** Absent ≡ `null` is the rule `DataEntry` already
// states, for the same reason (`domain_data/store.rs:12`): a client that
// serialises an omitted field as `null` must not thereby mean something
// different from a client that omits it.
//
// That half is not a nicety, it is load-bearing. logmon's own MCP shim builds
// its params with `json!({"count": p.count, …})` over `Option` fields
// (`crates/mcp/src/server.rs:475`), and `json!` renders `None` as `null` rather
// than omitting the key — so **every** call from every agent arrives with a
// `null` for each parameter it did not set. Treating `null` as a wrong-typed
// value would refuse the shim's entire surface on first use.

/// How a JSON value reads in an error message. The *type*, never the value:
/// a rejected parameter may be an arbitrarily large object, and an error
/// message is not a place to echo one back.
fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The shared shape: absent/`null` → `Ok(None)`, wrong type → `Err`.
fn opt_of<'a, T>(
    params: &'a Value,
    key: &str,
    expected: &str,
    read: impl FnOnce(&'a Value) -> Option<T>,
) -> Result<Option<T>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => read(v)
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be {expected}, not {got}", got = json_type(v))),
    }
}

fn missing(key: &str) -> String {
    format!("missing required parameter: {key}")
}

fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    opt_of(params, key, "a string", Value::as_str)
}

fn req_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    opt_str(params, key)?.ok_or_else(|| missing(key))
}

fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>, String> {
    // A stringified boolean is the common client mistake, and it used to be the
    // destructive one: `{"reset": "false"}` read as absent, took the default
    // `true`, and discarded the collector's live window. Named explicitly, so
    // the remedy is in the message rather than in the reader's head.
    if let Some(Value::String(s)) = params.get(key) {
        if s == "true" || s == "false" {
            return Err(format!(
                "`{key}` must be a boolean, not the string \"{s}\" — send {s} unquoted"
            ));
        }
    }
    opt_of(params, key, "a boolean", Value::as_bool)
}

fn opt_u64(params: &Value, key: &str) -> Result<Option<u64>, String> {
    opt_of(params, key, "a non-negative integer", Value::as_u64)
}

fn req_u64(params: &Value, key: &str) -> Result<u64, String> {
    opt_u64(params, key)?.ok_or_else(|| missing(key))
}

fn opt_f64(params: &Value, key: &str) -> Result<Option<f64>, String> {
    opt_of(params, key, "a number", Value::as_f64)
}

/// A `u32` parameter, refused rather than truncated when it does not fit.
///
/// `as u32` on a `u64` is a silent wrap: `pre_window: 4294967296` became **0**
/// — the largest window a caller could ask for, capturing nothing, reported as
/// success — and `id: 4294967297` addressed filter **1**. Same defect class as
/// `skip_warmup_ms` in 0.9.0 (CHANGELOG "A large `skip_warmup_ms` no longer
/// silently does nothing"), which is why it is a checked conversion here and
/// not a cast at each call site.
fn opt_u32(params: &Value, key: &str) -> Result<Option<u32>, String> {
    match opt_u64(params, key)? {
        None => Ok(None),
        Some(n) => u32::try_from(n)
            .map(Some)
            .map_err(|_| format!("`{key}` is {n}, which exceeds the maximum {}", u32::MAX)),
    }
}

fn req_u32(params: &Value, key: &str) -> Result<u32, String> {
    opt_u32(params, key)?.ok_or_else(|| missing(key))
}

fn opt_usize(params: &Value, key: &str) -> Result<Option<usize>, String> {
    match opt_u64(params, key)? {
        None => Ok(None),
        Some(n) => usize::try_from(n)
            .map(Some)
            .map_err(|_| format!("`{key}` is {n}, which exceeds the maximum {}", usize::MAX)),
    }
}

/// Upper bound on a `before` / `after` context window.
///
/// `context_by_seq` computes `idx + after + 1` (`store/memory.rs:161`,
/// `span/store.rs:221`). The workspace declares no `[profile.release]`
/// overflow-checks setting, so that addition wraps in release and panics in
/// debug — inside the per-connection task. Bounded by the largest buffer a
/// domain can be given, because a window wider than any possible buffer is
/// asking for records that cannot exist.
const MAX_CONTEXT_WINDOW: u64 = RpcHandler::MAX_DOMAIN_BUFFER_SIZE as u64;

fn opt_context_window(params: &Value, key: &str) -> Result<Option<usize>, String> {
    match opt_u64(params, key)? {
        None => Ok(None),
        Some(n) if n > MAX_CONTEXT_WINDOW => Err(format!(
            "`{key}` is {n}, which exceeds the maximum context window {MAX_CONTEXT_WINDOW}"
        )),
        Some(n) => Ok(Some(n as usize)),
    }
}

/// An array-of-strings parameter.
///
/// `Some(vec![])` for a present-but-empty array, `None` for absent/`null`: the
/// two are not interchangeable. `collectors.edit` treats an empty `group_keys`
/// as a structural change and discards the live window, so reading a wrong-typed
/// value as an empty array — which is what `filter_map` used to do — zeroed a
/// measurement the caller never asked to end.
///
/// Elements are checked too. `filter_map(as_str)` silently DROPPED non-string
/// elements, so `["service", 7]` armed a collector grouped by one key when two
/// were asked for: the same silent substitution, one level down.
fn opt_str_array(params: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(arr) = opt_of(params, key, "an array of strings", Value::as_array)? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("`{key}[{i}]` must be a string, not {}", json_type(v)))?;
        out.push(s.to_string());
    }
    Ok(Some(out))
}

/// The rule session and domain names already share (§7). Collector names reach
/// the filesystem and the `collector@label` syntax, so `/`, `..` and `@` are
/// path traversal and a parse ambiguity respectively.
fn is_valid_collector_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// §5.7's convention, over `f64` milliseconds.
fn quantile_ms(sorted: &[f64], q: f64) -> f64 {
    crate::collector::project::quantile_index(sorted.len(), q)
        .map(|i| sorted[i])
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// domain_data.* helpers
// ---------------------------------------------------------------------------

/// Fold an inclusive `from_seq`/`to_seq` range into the parsed filter.
///
/// **Lowered into the filter rather than carried beside it, and that is the
/// whole point.** `evicted_before_window` reads its lower bound out of the
/// *parsed filter*, via `resolved_lower_bound`, which matches only
/// `Qualifier::SeqFilter { op: Gt }`. A range carried as a loose parameter
/// would never reach it — so a window whose start had already rolled out of
/// the ring would come back `truncated: false`, and a reader deciding whether
/// that window is complete would be told it is. Silence about missing evidence
/// is the one failure this surface exists to prevent.
///
/// Lowering also makes the range compose for free: `resolved_lower_bound` takes
/// the **max** of every lower bound present, so a range alongside a bookmark
/// bound resolves to the tighter of the two with no special case.
///
/// **Both ends are inclusive**, lowered to `Gt(from - 1)` and `Lt(to + 1)`,
/// saturating so the adjustment cannot wrap at `0` or `u64::MAX`. `SeqOp` has
/// only strict variants because bookmark semantics are strict; leaving the
/// *parameter* strict as well would mean a window of "the anchor, plus none
/// before and none after" excludes the anchor itself.
fn lower_seq_range(
    resolved: Option<crate::filter::parser::ParsedFilter>,
    from_seq: Option<u64>,
    to_seq: Option<u64>,
) -> Option<crate::filter::parser::ParsedFilter> {
    use crate::filter::parser::{ParsedFilter, Qualifier, SeqOp};

    if from_seq.is_none() && to_seq.is_none() {
        return resolved;
    }
    let mut bounds = Vec::new();
    if let Some(from) = from_seq {
        bounds.push(Qualifier::SeqFilter {
            op: SeqOp::Gt,
            value: from.saturating_sub(1),
        });
    }
    if let Some(to) = to_seq {
        bounds.push(Qualifier::SeqFilter {
            op: SeqOp::Lt,
            value: to.saturating_add(1),
        });
    }
    match resolved {
        // `None` matches nothing by construction. Narrowing it further would be
        // a no-op, and turning it into a range would widen a filter the caller
        // wrote to exclude everything.
        Some(ParsedFilter::None) => Some(ParsedFilter::None),
        Some(ParsedFilter::Qualifiers(mut qs)) => {
            qs.extend(bounds);
            Some(ParsedFilter::Qualifiers(qs))
        }
        None | Some(ParsedFilter::All) => Some(ParsedFilter::Qualifiers(bounds)),
    }
}

/// Parse the `entries` array into registry entries plus any outcomes that could
/// be decided before the registry saw them.
///
/// `ttl` is the only field that can fail here, and it fails *per entry* rather
/// than failing the batch — matching §3.3's rule that one malformed entry never
/// rejects the rest. Returning the rejections separately keeps that rule in one
/// place instead of teaching the registry about wire types.
type IndexedEntry = (usize, crate::domain_data::DataEntry);
type IndexedOutcome = (usize, crate::domain_data::DataOutcome);

/// Stored records captured either side of the anchor when the caller says
/// nothing. Wide enough to hold the shape of a failure, narrow enough that the
/// transport is not carrying a buffer dump.
const DEFAULT_CASE_WINDOW: usize = 350;

/// Ceiling on `before` and `after`, separately.
///
/// The transport buffers the whole capture, and `logs.export`'s own `count`
/// already defaults to unbounded — so this is the only thing between a typo and
/// a multi-megabyte RPC.
const MAX_CASE_WINDOW: usize = 5_000;

struct WindowCount {
    value: usize,
    /// The request exceeded [`MAX_CASE_WINDOW`] and was cut down to it. Reported
    /// rather than applied silently: the window is narrower than asked for, and
    /// a reader who assumes otherwise draws conclusions over seqs nobody read.
    clamped: bool,
}

fn window_count(params: &Value, key: &str) -> Result<WindowCount, String> {
    let requested = opt_usize(params, key)?.unwrap_or(DEFAULT_CASE_WINDOW);
    Ok(WindowCount {
        value: requested.min(MAX_CASE_WINDOW),
        clamped: requested > MAX_CASE_WINDOW,
    })
}

/// The anchor and its ten neighbours either side, out of the full window.
///
/// §5.2: the document shows this much and the logdata holds the rest. Ten is
/// load-bearing — with only the anchor every reader must open the logdata to
/// learn anything and the document stops being a triage surface; with hundreds,
/// the caveats above get scrolled past.
fn neighbourhood(logs: &[LogEntry], anchor_seq: u64) -> Vec<LogEntry> {
    let w = crate::cases::document::NEIGHBOUR_WINDOW;
    let Some(idx) = logs.iter().position(|e| e.seq == anchor_seq) else {
        return Vec::new();
    };
    let start = idx.saturating_sub(w);
    let end = (idx + w + 1).min(logs.len());
    logs[start..end].to_vec()
}

fn parse_data_entries(
    raw: Option<&Value>,
) -> Result<(Vec<IndexedEntry>, Vec<IndexedOutcome>), String> {
    let arr = raw
        .and_then(|v| v.as_array())
        .ok_or("entries is required and must be an array")?;
    if arr.is_empty() {
        return Err("entries must not be empty".into());
    }
    let mut entries = Vec::with_capacity(arr.len());
    let mut rejected = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        // Named by INDEX, and absent distinguished from wrong-typed. This is a
        // batch: "each entry needs a string path" told a caller with thirty
        // entries only that one of them was wrong, and said "needs a string"
        // whether the key was missing or a number — the same conflation this
        // change removes everywhere else, in miniature.
        let path = match item.get("path") {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => return Err(format!("`entries[{i}]` is missing `path`")),
            Some(other) => {
                return Err(format!(
                    "`entries[{i}].path` must be a string, not {}",
                    json_type(other)
                ))
            }
        };
        let reject = |reason| {
            (
                i,
                crate::domain_data::DataOutcome {
                    path: path.clone(),
                    outcome: crate::domain_data::Outcome::Rejected { reason },
                },
            )
        };
        // Absent and `null` both mean *validate*. Anything else that is not a
        // string is REJECTED rather than read as absent: `{"value": 42}` would
        // otherwise silently confirm the old value instead of setting the new
        // one, and report `validated` while having done nothing the caller
        // asked for. A wrong answer that looks like a right one is the failure
        // this registry exists to prevent.
        let value = match item.get("value") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                rejected.push(reject(crate::domain_data::RejectReason::MalformedValue));
                continue;
            }
        };
        let ttl = match item.get("ttl") {
            None => None,
            Some(v) => match crate::domain_data::store::parse_ttl(v) {
                Ok(t) => t,
                Err(_) => {
                    rejected.push(reject(crate::domain_data::RejectReason::MalformedTtl));
                    continue;
                }
            },
        };
        entries.push((i, crate::domain_data::DataEntry { path, value, ttl }));
    }
    Ok((entries, rejected))
}

/// Merge applied outcomes and pre-registry rejections back into the caller's
/// order.
///
/// By **index**, not by path. Matching on the path looked equivalent and is not:
/// a batch may legitimately repeat a key, and every outcome for that key would
/// then take the position of its first occurrence — reordering the whole reply
/// against a list the caller matches up positionally.
fn merge_outcomes(
    applied: Vec<crate::domain_data::DataOutcome>,
    indices: Vec<usize>,
    rejected: Vec<IndexedOutcome>,
) -> Vec<crate::domain_data::DataOutcome> {
    let mut all: Vec<IndexedOutcome> = indices.into_iter().zip(applied).collect();
    all.extend(rejected);
    all.sort_by_key(|(i, _)| *i);
    all.into_iter().map(|(_, o)| o).collect()
}

fn wire_outcome(o: &crate::domain_data::DataOutcome) -> DomainDataOutcome {
    use crate::domain_data::Outcome;
    DomainDataOutcome {
        path: o.path.clone(),
        outcome: o.outcome.as_str().to_string(),
        cause: match &o.outcome {
            Outcome::Unknown { cause } => Some(cause.as_str().to_string()),
            _ => None,
        },
        reason: match &o.outcome {
            Outcome::Rejected { reason } => Some(reason.as_str().to_string()),
            _ => None,
        },
    }
}
