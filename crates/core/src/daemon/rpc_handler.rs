use crate::daemon::domain::{Domain, DomainId, DomainRegistry, DomainSource};
use crate::daemon::domain_lifecycle::{spawn_ephemeral_domain, DomainPortSpec};
use crate::daemon::log_processor::sync_pre_buffer_size_for_domain;
use crate::daemon::session::{SessionId, SessionRegistry};
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
        receivers_info: Vec<String>,
        domain_policy: DomainPolicy,
    ) -> Self {
        Self {
            domains,
            sessions,
            start_time: std::time::Instant::now(),
            receivers_info,
            domain_policy,
            create_lock: tokio::sync::Mutex::new(()),
        }
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
            "filters.list" => self.handle_filters_list(session_id),
            "filters.add" => self.handle_filters_add(session_id, &request.params),
            "filters.edit" => self.handle_filters_edit(session_id, &request.params),
            "filters.remove" => self.handle_filters_remove(session_id, &request.params),
            "triggers.list" => self.handle_triggers_list(session_id),
            "triggers.add" => self.handle_triggers_add(session_id, &request.params),
            "triggers.edit" => self.handle_triggers_edit(session_id, &request.params),
            "triggers.remove" => self.handle_triggers_remove(session_id, &request.params),
            "session.list" => self.handle_session_list(),
            "session.drop" => self.handle_session_drop(&request.params),
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
            "domains.clear" => self.handle_domains_clear(session_id),
            "traces.recent" => self.handle_traces_recent(session_id, &request.params),
            "traces.get" => self.handle_traces_get(session_id, &request.params),
            "traces.summary" => self.handle_traces_summary(session_id, &request.params),
            "traces.slow" => self.handle_traces_slow(session_id, &request.params),
            "traces.logs" => self.handle_traces_logs(session_id, &request.params),
            "spans.context" => self.handle_spans_context(session_id, &request.params),
            "bookmarks.add" => self.handle_bookmarks_add(session_id, &request.params),
            "bookmarks.list" => self.handle_bookmarks_list(session_id, &request.params),
            "bookmarks.remove" => self.handle_bookmarks_remove(session_id, &request.params),
            "bookmarks.clear" => self.handle_bookmarks_clear(session_id, &request.params),
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
            .map(|d| domain_to_info(d, self.domain_policy.stale_after_secs))
            .collect();
        domains.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::to_value(DomainsListResult { domains }).map_err(|e| e.to_string())
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

        serde_json::to_value(domain_to_info(
            &new_domain,
            self.domain_policy.stale_after_secs,
        ))
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
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());

        // trace_id shortcut path — does NOT support cursor; reject c>= if present.
        if let Some(trace_id_hex) = params.get("trace_id").and_then(|v| v.as_str()) {
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
        let seq = params
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: seq".to_string())?;
        let before = params.get("before").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let after = params.get("after").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let entries = d.pipeline.context_by_seq(seq, before, after);
        Ok(json!({ "logs": entries, "count": entries.len() }))
    }

    fn handle_logs_export(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        let oldest_first = cursor_commit.is_some();
        let (entries, stats) =
            d.pipeline
                .recent_logs_with_stats(count, resolved.as_ref(), oldest_first);

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

    fn handle_status(&self, session_id: &SessionId) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let session_info = self.sessions.get(session_id);
        let stats = d.pipeline.store_stats();
        let drops = d.metrics.snapshot();
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
        let filter = params
            .get("filter")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: filter".to_string())?;
        // Reject bookmark filters in registered (long-lived) filters.
        let parsed = crate::filter::parser::parse_filter(filter).map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
        let desc = params.get("description").and_then(|v| v.as_str());
        let id = self
            .sessions
            .add_filter(session_id, filter, desc)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size_for_domain(&d.pipeline, &self.sessions, d.id());
        Ok(json!({ "id": id }))
    }

    fn handle_filters_edit(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let filter_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
        let filter = params.get("filter").and_then(|v| v.as_str());
        let desc = params.get("description").and_then(|v| v.as_str());
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
        let filter_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
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
                })
            })
            .collect();
        Ok(json!({ "triggers": items }))
    }

    fn handle_triggers_add(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let filter = params
            .get("filter")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: filter".to_string())?;
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
        // is honored (the `.map(...).unwrap_or(...)` only defaults when absent).
        let pre = params
            .get("pre_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_PRE_WINDOW);
        let post = params
            .get("post_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_POST_WINDOW);
        let ctx = params
            .get("notify_context")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(crate::engine::trigger::DEFAULT_TRIGGER_NOTIFY_CONTEXT);
        let desc = params.get("description").and_then(|v| v.as_str());
        let oneshot = params
            .get("oneshot")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
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
        let trigger_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
        let filter = params.get("filter").and_then(|v| v.as_str());
        let pre = params
            .get("pre_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let post = params
            .get("post_window")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let ctx = params
            .get("notify_context")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let desc = params.get("description").and_then(|v| v.as_str());
        let oneshot = params.get("oneshot").and_then(|v| v.as_bool());
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
        }))
    }

    fn handle_triggers_remove(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trigger_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
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
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
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
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16)
            .map_err(|_| "invalid trace_id: must be 32-char hex")?;
        let include_logs = params
            .get("include_logs")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Resolve filter (used to filter spans within the trace below)
        let filter_str = params.get("filter").and_then(|v| v.as_str());
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
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
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
        let min_duration = params
            .get("min_duration_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(100.0);
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) =
            self.parse_and_resolve_filter(filter_str, session_id, &d.bookmarks)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.slow".to_string());
        }
        let group_by = params.get("group_by").and_then(|v| v.as_str());

        let slow = d
            .span_store
            .slow_spans(min_duration, count, resolved.as_ref());

        match group_by {
            Some("name") => {
                // Group by span name, compute aggregates
                let mut groups: std::collections::HashMap<String, Vec<f64>> =
                    std::collections::HashMap::new();
                for s in &slow {
                    groups
                        .entry(s.name.clone())
                        .or_default()
                        .push(s.duration_ms);
                }
                let mut result: Vec<Value> = groups
                    .iter()
                    .map(|(name, durations)| {
                        let mut sorted = durations.clone();
                        sorted
                            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
                        let p95_idx = ((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1);
                        json!({
                            "name": name,
                            "avg_ms": (avg * 10.0).round() / 10.0,
                            "p95_ms": sorted[p95_idx],
                            "count": sorted.len(),
                        })
                    })
                    .collect();
                result.sort_by(|a, b| {
                    b["avg_ms"]
                        .as_f64()
                        .unwrap_or(0.0)
                        .partial_cmp(&a["avg_ms"].as_f64().unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                Ok(json!({ "grouped_by": "name", "groups": result }))
            }
            _ => Ok(json!({ "spans": slow, "count": slow.len() })),
        }
    }

    fn handle_traces_logs(&self, session_id: &SessionId, params: &Value) -> Result<Value, String> {
        let d = self.resolve_domain(session_id)?;
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id = u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let filter_str = params.get("filter").and_then(|v| v.as_str());
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
        let seq = params
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or("missing required parameter: seq")?;
        let before = params.get("before").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        let after = params.get("after").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

        let spans = d.span_store.context_by_seq(seq, before, after);
        Ok(json!({ "spans": spans, "count": spans.len() }))
    }

    // -----------------------------------------------------------------------
    // session.*
    // -----------------------------------------------------------------------
    //
    // Session methods intentionally span domains — config is global, only data
    // is partitioned (spec §2). They resolve no domain.

    fn handle_session_list(&self) -> Result<Value, String> {
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

    fn handle_session_drop(&self, params: &Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        self.sessions
            .drop_session(name)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "dropped": name }))
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
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let replace = params
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // `start_seq` defaults to the daemon's current seq counter — i.e.,
        // "the cursor we'd hand out right now". Cursor auto-create (later
        // task) uses 0 explicitly to mean "before all records". Distinguish
        // "absent / null" (use default) from "present but wrong type/range"
        // (error, don't silently coerce).
        let start_seq = match params.get("start_seq") {
            None => d.pipeline.current_seq(),
            Some(v) if v.is_null() => d.pipeline.current_seq(),
            Some(v) => v
                .as_u64()
                .ok_or_else(|| "start_seq must be a non-negative integer".to_string())?,
        };
        let description = params.get("description").and_then(|v| v.as_str());

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
        let session_filter = params.get("session").and_then(|v| v.as_str());

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
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
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
        // Default to the calling session if no explicit session is given.
        let session = params
            .get("session")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
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

    fn sweep_bookmarks(&self, d: &Domain) {
        let oldest_log = d.pipeline.oldest_log_seq();
        let oldest_span = d.span_store.oldest_seq();
        d.bookmarks.sweep(oldest_log, oldest_span);
    }
}
