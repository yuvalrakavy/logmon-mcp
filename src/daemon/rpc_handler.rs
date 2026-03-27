use crate::daemon::log_processor::sync_pre_buffer_size;
use crate::daemon::session::{SessionId, SessionRegistry};
use crate::engine::pipeline::LogPipeline;
use crate::rpc::types::*;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct RpcHandler {
    pipeline: Arc<LogPipeline>,
    sessions: Arc<SessionRegistry>,
    start_time: std::time::Instant,
    receivers_info: Vec<String>,
}

impl RpcHandler {
    pub fn new(
        pipeline: Arc<LogPipeline>,
        sessions: Arc<SessionRegistry>,
        receivers_info: Vec<String>,
    ) -> Self {
        Self {
            pipeline,
            sessions,
            start_time: std::time::Instant::now(),
            receivers_info,
        }
    }

    /// Handle an RPC request for a given session.
    pub fn handle(&self, session_id: &SessionId, request: &RpcRequest) -> RpcResponse {
        let result = match request.method.as_str() {
            "logs.recent" => self.handle_logs_recent(&request.params),
            "logs.context" => self.handle_logs_context(&request.params),
            "logs.export" => self.handle_logs_export(&request.params),
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
        }
    }

    // -----------------------------------------------------------------------
    // logs.*
    // -----------------------------------------------------------------------

    fn handle_logs_recent(&self, params: &Value) -> Result<Value, String> {
        let count = params.get("count").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
        let filter = params.get("filter").and_then(|v| v.as_str());
        let entries = self.pipeline.recent_logs_str(count, filter);
        Ok(json!({ "logs": entries, "count": entries.len() }))
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

    fn handle_logs_export(&self, params: &Value) -> Result<Value, String> {
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX) as usize;
        let filter = params.get("filter").and_then(|v| v.as_str());
        let entries = self.pipeline.recent_logs_str(count, filter);
        Ok(json!({ "logs": entries, "count": entries.len(), "format": "json" }))
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
        let pre = params.get("pre_window").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let post = params.get("post_window").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let ctx = params
            .get("notify_context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let desc = params.get("description").and_then(|v| v.as_str());
        let id = self
            .sessions
            .add_trigger(session_id, filter, pre, post, ctx, desc)
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
        let info = self
            .sessions
            .edit_trigger(session_id, trigger_id, filter, pre, post, ctx, desc)
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
}
