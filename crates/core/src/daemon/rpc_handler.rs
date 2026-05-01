use crate::daemon::log_processor::sync_pre_buffer_size;
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::engine::pipeline::LogPipeline;
use logmon_broker_protocol::*;
use crate::span::store::SpanStore;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    span_store: Arc<SpanStore>,
    sessions: Arc<SessionRegistry>,
    bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}

impl RpcHandler {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        span_store: Arc<SpanStore>,
        sessions: Arc<SessionRegistry>,
        bookmarks: Arc<crate::store::bookmarks::BookmarkStore>,
        receivers_info: Vec<String>,
    ) -> Self {
        Self {
            pipeline,
            span_store,
            sessions,
            bookmarks,
            start_time: std::time::Instant::now(),
            receivers_info,
        }
    }

    /// Handle an RPC request for a given session.
    pub fn handle(&self, session_id: &SessionId, request: &RpcRequest) -> RpcResponse {
        let result = match request.method.as_str() {
            "logs.recent" => self.handle_logs_recent(session_id, &request.params),
            "logs.context" => self.handle_logs_context(&request.params),
            "logs.export" => self.handle_logs_export(session_id, &request.params),
            "logs.clear" => self.handle_logs_clear(),
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
            "traces.recent" => self.handle_traces_recent(session_id, &request.params),
            "traces.get" => self.handle_traces_get(session_id, &request.params),
            "traces.summary" => self.handle_traces_summary(&request.params),
            "traces.slow" => self.handle_traces_slow(session_id, &request.params),
            "traces.logs" => self.handle_traces_logs(session_id, &request.params),
            "spans.context" => self.handle_spans_context(&request.params),
            "bookmarks.add" => self.handle_bookmarks_add(session_id, &request.params),
            "bookmarks.list" => self.handle_bookmarks_list(&request.params),
            "bookmarks.remove" => self.handle_bookmarks_remove(session_id, &request.params),
            "bookmarks.clear" => self.handle_bookmarks_clear(session_id, &request.params),
            _ => Err(format!("unknown method: {}", request.method)),
        };

        match result {
            Ok(value) => RpcResponse::success(request.id, value),
            Err(msg) => RpcResponse::error(request.id, -32601, &msg),
        }
    }

    pub fn build_session_start_result(&self, session_id: &SessionId) -> SessionStartResult {
        let info = self.sessions.get(session_id);
        SessionStartResult {
            session_id: session_id.to_string(),
            is_new: true, // caller can override for reconnects
            queued_notifications: info.as_ref().map_or(0, |i| i.queue_size),
            trigger_count: info.as_ref().map_or(0, |i| i.trigger_count),
            filter_count: info.as_ref().map_or(0, |i| i.filter_count),
            daemon_uptime_secs: self.start_time.elapsed().as_secs(),
            buffer_size: self.pipeline.store_len(),
            receivers: self.receivers_info.clone(),
            capabilities: vec![
                "bookmarks".into(),
                "oneshot_triggers".into(),
                "client_info".into(),
            ],
        }
    }

    // -----------------------------------------------------------------------
    // logs.*
    // -----------------------------------------------------------------------

    /// Parse a filter string and resolve any bookmark qualifiers against the
    /// bookmark store using `session_id` as the current session.
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
    /// non-cursor handlers, the value is always `None`. Task 9 will add an
    /// explicit reject-with-error path for handlers that disallow cursor
    /// qualifiers (e.g. `traces.recent`, `traces.get`, `traces.slow`); for
    /// now non-cursor handlers simply ignore the field.
    fn parse_and_resolve_filter(
        &self,
        filter_str: Option<&str>,
        session_id: &SessionId,
    ) -> Result<
        (
            Option<crate::filter::parser::ParsedFilter>,
            Option<crate::store::bookmarks::CursorCommit>,
        ),
        String,
    > {
        let Some(s) = filter_str else { return Ok((None, None)) };
        if s.trim().is_empty() {
            return Ok((None, None));
        }
        let parsed = crate::filter::parser::parse_filter(s).map_err(|e| e.to_string())?;
        let resolved = crate::filter::bookmark_resolver::resolve_bookmarks(
            parsed,
            &self.bookmarks,
            &session_id.to_string(),
        )
        .map_err(|e| e.to_string())?;
        Ok((Some(resolved.filter), resolved.cursor_commit))
    }

    fn handle_logs_recent(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());

        // Optional trace_id filter (unchanged)
        if let Some(trace_id_hex) = params.get("trace_id").and_then(|v| v.as_str()) {
            let trace_id = u128::from_str_radix(trace_id_hex, 16)
                .map_err(|_| "invalid trace_id")?;
            let logs = self.pipeline.logs_by_trace_id(trace_id);
            return Ok(json!({ "logs": logs, "count": logs.len() }));
        }

        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;
        let oldest_first = cursor_commit.is_some();
        let entries = self.pipeline.recent_logs(count, resolved.as_ref(), oldest_first);

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

        let mut result = json!({ "logs": entries, "count": entries.len() });
        if let Some(s) = advanced_to {
            result["cursor_advanced_to"] = json!(s);
        }
        Ok(result)
    }

    fn handle_logs_context(&self, params: &Value) -> Result<Value, String> {
        let seq = params
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: seq".to_string())?;
        let before = params.get("before").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let after = params.get("after").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
        let entries = self.pipeline.context_by_seq(seq, before, after);
        Ok(json!({ "logs": entries, "count": entries.len() }))
    }

    fn handle_logs_export(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;
        let oldest_first = cursor_commit.is_some();
        let entries = self.pipeline.recent_logs(count, resolved.as_ref(), oldest_first);

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

        let mut result =
            json!({ "logs": entries, "count": entries.len(), "format": "json" });
        if let Some(s) = advanced_to {
            result["cursor_advanced_to"] = json!(s);
        }
        Ok(result)
    }

    fn handle_logs_clear(&self) -> Result<Value, String> {
        let cleared = self.pipeline.clear_logs();
        Ok(json!({ "cleared": cleared }))
    }

    // -----------------------------------------------------------------------
    // status.*
    // -----------------------------------------------------------------------

    fn handle_status(&self, session_id: &SessionId) -> Result<Value, String> {
        let session_info = self.sessions.get(session_id);
        let stats = self.pipeline.store_stats();
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
                "current_size": self.pipeline.store_len(),
            },
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

    fn handle_filters_add(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let filter = params
            .get("filter")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: filter".to_string())?;
        // Reject bookmark filters in registered (long-lived) filters.
        let parsed = crate::filter::parser::parse_filter(filter)
            .map_err(|e| e.to_string())?;
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
        sync_pre_buffer_size(&self.pipeline, &self.sessions);
        Ok(json!({ "id": id }))
    }

    fn handle_filters_edit(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
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
        let filter_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
        self.sessions
            .remove_filter(session_id, filter_id)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size(&self.pipeline, &self.sessions);
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

    fn handle_triggers_add(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let filter = params
            .get("filter")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: filter".to_string())?;
        // Reject bookmark filters in registered (long-lived) triggers.
        let parsed = crate::filter::parser::parse_filter(filter)
            .map_err(|e| e.to_string())?;
        if crate::filter::parser::contains_bookmark_qualifier(&parsed) {
            return Err(
                "bookmarks and cursors (b>=, b<=, c>=) are not allowed in registered filters/triggers — use them only in query tools"
                    .to_string(),
            );
        }
        let pre = params.get("pre_window").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let post = params.get("post_window").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let ctx = params
            .get("notify_context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let desc = params.get("description").and_then(|v| v.as_str());
        let oneshot = params
            .get("oneshot")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let id = self
            .sessions
            .add_trigger(session_id, filter, pre, post, ctx, desc, oneshot)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size(&self.pipeline, &self.sessions);
        Ok(json!({ "id": id }))
    }

    fn handle_triggers_edit(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let trigger_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
        let filter = params.get("filter").and_then(|v| v.as_str());
        let pre = params.get("pre_window").and_then(|v| v.as_u64()).map(|v| v as u32);
        let post = params.get("post_window").and_then(|v| v.as_u64()).map(|v| v as u32);
        let ctx = params
            .get("notify_context")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let desc = params.get("description").and_then(|v| v.as_str());
        let oneshot = params.get("oneshot").and_then(|v| v.as_bool());
        let info = self
            .sessions
            .edit_trigger(session_id, trigger_id, filter, pre, post, ctx, desc, oneshot)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size(&self.pipeline, &self.sessions);
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
        let trigger_id = params
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing required parameter: id".to_string())?
            as u32;
        self.sessions
            .remove_trigger(session_id, trigger_id)
            .map_err(|e| e.to_string())?;
        sync_pre_buffer_size(&self.pipeline, &self.sessions);
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
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.recent".to_string());
        }

        let pipeline = &self.pipeline;
        let summaries = self.span_store.recent_traces(
            count,
            resolved.as_ref(),
            |trace_id| pipeline.count_by_trace_id(trace_id) as u32,
        );
        Ok(json!({ "traces": summaries, "count": summaries.len() }))
    }

    fn handle_traces_get(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
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
        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.get".to_string());
        }

        let mut spans = self.span_store.get_trace(trace_id);
        if let Some(f) = resolved.as_ref() {
            spans.retain(|s| crate::filter::matcher::matches_span(f, s));
        }
        let logs = if include_logs {
            self.pipeline.logs_by_trace_id(trace_id)
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

    fn handle_traces_summary(&self, params: &Value) -> Result<Value, String> {
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id =
            u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let spans = self.span_store.get_trace(trace_id);
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

    fn handle_traces_slow(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let min_duration = params
            .get("min_duration_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(100.0);
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;
        if cursor_commit.is_some() {
            return Err("cursor qualifier not permitted in traces.slow".to_string());
        }
        let group_by = params.get("group_by").and_then(|v| v.as_str());

        let slow = self
            .span_store
            .slow_spans(min_duration, count, resolved.as_ref());

        match group_by {
            Some("name") => {
                // Group by span name, compute aggregates
                let mut groups: std::collections::HashMap<String, Vec<f64>> =
                    std::collections::HashMap::new();
                for s in &slow {
                    groups.entry(s.name.clone()).or_default().push(s.duration_ms);
                }
                let mut result: Vec<Value> = groups
                    .iter()
                    .map(|(name, durations)| {
                        let mut sorted = durations.clone();
                        sorted.sort_by(|a, b| {
                            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
                        let p95_idx =
                            ((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1);
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

    fn handle_traces_logs(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        let trace_id_hex = params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .ok_or("missing required parameter: trace_id")?;
        let trace_id =
            u128::from_str_radix(trace_id_hex, 16).map_err(|_| "invalid trace_id")?;

        let filter_str = params.get("filter").and_then(|v| v.as_str());
        let (resolved, cursor_commit) = self.parse_and_resolve_filter(filter_str, session_id)?;

        // logs_by_trace_id already returns logs in stored (seq-ascending) order
        // — that's the same order cursor pagination wants — so no ordering
        // switch is needed on this code path.
        let mut logs = self.pipeline.logs_by_trace_id(trace_id);
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

    fn handle_spans_context(&self, params: &Value) -> Result<Value, String> {
        let seq = params
            .get("seq")
            .and_then(|v| v.as_u64())
            .ok_or("missing required parameter: seq")?;
        let before = params.get("before").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
        let after = params.get("after").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

        let spans = self.span_store.context_by_seq(seq, before, after);
        Ok(json!({ "spans": spans, "count": spans.len() }))
    }

    // -----------------------------------------------------------------------
    // session.*
    // -----------------------------------------------------------------------

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
            None => self.pipeline.current_seq(),
            Some(v) if v.is_null() => self.pipeline.current_seq(),
            Some(v) => v
                .as_u64()
                .ok_or_else(|| "start_seq must be a non-negative integer".to_string())?,
        };
        let description = params.get("description").and_then(|v| v.as_str());

        // Sweep before adding so the store stays tidy.
        self.sweep_bookmarks();

        let session = session_id.to_string();
        let (bookmark, replaced) = self
            .bookmarks
            .add(&session, name, start_seq, description, replace)
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "qualified_name": bookmark.qualified_name,
            "seq": bookmark.seq,
            "replaced": replaced,
        }))
    }

    fn handle_bookmarks_list(&self, params: &Value) -> Result<Value, String> {
        self.sweep_bookmarks();
        let session_filter = params.get("session").and_then(|v| v.as_str());

        let items: Vec<Value> = self
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
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required parameter: name".to_string())?;
        let qualified = crate::store::bookmarks::qualify(name, &session_id.to_string());
        self.bookmarks
            .remove(&qualified)
            .map_err(|e| e.to_string())?;
        Ok(json!({ "removed": qualified }))
    }

    fn handle_bookmarks_clear(
        &self,
        session_id: &SessionId,
        params: &Value,
    ) -> Result<Value, String> {
        // Default to the calling session if no explicit session is given.
        let session = params
            .get("session")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| session_id.to_string());
        let removed_count = self.bookmarks.clear_session(&session);
        Ok(json!({ "removed_count": removed_count, "session": session }))
    }

    fn sweep_bookmarks(&self) {
        let oldest_log = self.pipeline.oldest_log_seq();
        let oldest_span = self.span_store.oldest_seq();
        self.bookmarks.sweep(oldest_log, oldest_span);
    }
}
