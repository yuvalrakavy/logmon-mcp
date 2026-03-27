use crate::engine::pipeline::PipelineEvent;
use crate::engine::trigger::{TriggerError, TriggerInfo, TriggerManager, TriggerMatch};
use crate::filter::matcher::matches_entry;
use crate::filter::parser::{parse_filter, FilterParseError, ParsedFilter};
use crate::gelf::message::LogEntry;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionId {
    Anonymous(String),
    Named(String),
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionId::Anonymous(id) => write!(f, "{id}"),
            SessionId::Named(name) => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: Option<String>,
    pub connected: bool,
    pub trigger_count: usize,
    pub filter_count: usize,
    pub queue_size: usize,
    pub last_seen_secs_ago: u64,
}

/// Buffer filter entry (per-session).
pub struct BufferFilterEntry {
    pub id: u32,
    pub condition: ParsedFilter,
    pub filter_string: String,
    pub description: Option<String>,
}

pub struct FilterInfo {
    pub id: u32,
    pub filter_string: String,
    pub description: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("invalid session name: {0}")]
    InvalidName(String),
    #[error("session already connected: {0}")]
    AlreadyConnected(String),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("filter error: {0}")]
    FilterError(#[from] FilterParseError),
    #[error("trigger error: {0}")]
    TriggerError(#[from] TriggerError),
}

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

struct SessionState {
    name: Option<String>,
    triggers: TriggerManager,
    filters: RwLock<Vec<BufferFilterEntry>>,
    next_filter_id: AtomicU32,
    notification_queue: Mutex<VecDeque<PipelineEvent>>,
    max_queue_size: usize,
    post_window_remaining: AtomicU32,
    connected: AtomicBool,
    last_seen: Mutex<Instant>,
}

impl SessionState {
    fn new_anonymous() -> Self {
        Self {
            name: None,
            triggers: TriggerManager::new(),
            filters: RwLock::new(Vec::new()),
            next_filter_id: AtomicU32::new(1),
            notification_queue: Mutex::new(VecDeque::new()),
            max_queue_size: 1000,
            post_window_remaining: AtomicU32::new(0),
            connected: AtomicBool::new(true),
            last_seen: Mutex::new(Instant::now()),
        }
    }

    fn new_named(name: &str) -> Self {
        Self {
            name: Some(name.to_string()),
            triggers: TriggerManager::new(),
            filters: RwLock::new(Vec::new()),
            next_filter_id: AtomicU32::new(1),
            notification_queue: Mutex::new(VecDeque::new()),
            max_queue_size: 1000,
            post_window_remaining: AtomicU32::new(0),
            connected: AtomicBool::new(true),
            last_seen: Mutex::new(Instant::now()),
        }
    }

    fn to_info(&self, id: &SessionId) -> SessionInfo {
        let now = Instant::now();
        let last = *self.last_seen.lock().expect("last_seen lock poisoned");
        SessionInfo {
            id: id.clone(),
            name: self.name.clone(),
            connected: self.connected.load(Ordering::Relaxed),
            trigger_count: self.triggers.list().len(),
            filter_count: self
                .filters
                .read()
                .expect("filters lock poisoned")
                .len(),
            queue_size: self
                .notification_queue
                .lock()
                .expect("queue lock poisoned")
                .len(),
            last_seen_secs_ago: now.duration_since(last).as_secs(),
        }
    }

    fn touch(&self) {
        *self.last_seen.lock().expect("last_seen lock poisoned") = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// Session name validation
// ---------------------------------------------------------------------------

fn is_valid_session_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// ---------------------------------------------------------------------------
// SessionRegistry
// ---------------------------------------------------------------------------

pub struct SessionRegistry {
    sessions: RwLock<HashMap<SessionId, SessionState>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    // --- Session lifecycle ---

    pub fn create_anonymous(&self) -> SessionId {
        let id = SessionId::Anonymous(Uuid::new_v4().to_string());
        let state = SessionState::new_anonymous();
        self.sessions
            .write()
            .expect("sessions lock poisoned")
            .insert(id.clone(), state);
        id
    }

    pub fn create_named(&self, name: &str) -> Result<SessionId, SessionError> {
        if !is_valid_session_name(name) {
            return Err(SessionError::InvalidName(name.to_string()));
        }

        let id = SessionId::Named(name.to_string());
        let mut sessions = self.sessions.write().expect("sessions lock poisoned");

        if let Some(existing) = sessions.get(&id) {
            if existing.connected.load(Ordering::Relaxed) {
                return Err(SessionError::AlreadyConnected(name.to_string()));
            }
            // Named session exists but disconnected — reconnect it
            existing.connected.store(true, Ordering::Relaxed);
            existing.touch();
            return Ok(id);
        }

        let state = SessionState::new_named(name);
        sessions.insert(id.clone(), state);
        Ok(id)
    }

    pub fn reconnect(&self, id: &SessionId) -> Result<(), SessionError> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        if state.connected.load(Ordering::Relaxed) {
            return Err(SessionError::AlreadyConnected(id.to_string()));
        }

        match id {
            SessionId::Anonymous(_) => {
                return Err(SessionError::NotFound(id.to_string()));
            }
            SessionId::Named(_) => {
                state.connected.store(true, Ordering::Relaxed);
                state.touch();
            }
        }
        Ok(())
    }

    pub fn disconnect(&self, id: &SessionId) {
        match id {
            SessionId::Anonymous(_) => {
                // Remove anonymous sessions on disconnect
                self.sessions
                    .write()
                    .expect("sessions lock poisoned")
                    .remove(id);
            }
            SessionId::Named(_) => {
                let sessions = self.sessions.read().expect("sessions lock poisoned");
                if let Some(state) = sessions.get(id) {
                    state.connected.store(false, Ordering::Relaxed);
                    state.touch();
                }
            }
        }
    }

    pub fn drop_session(&self, name: &str) -> Result<(), SessionError> {
        let id = SessionId::Named(name.to_string());
        let mut sessions = self.sessions.write().expect("sessions lock poisoned");

        let state = sessions
            .get(&id)
            .ok_or_else(|| SessionError::NotFound(name.to_string()))?;

        if state.connected.load(Ordering::Relaxed) {
            return Err(SessionError::AlreadyConnected(name.to_string()));
        }

        sessions.remove(&id);
        Ok(())
    }

    pub fn is_connected(&self, id: &SessionId) -> bool {
        self.sessions
            .read()
            .expect("sessions lock poisoned")
            .get(id)
            .map(|s| s.connected.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    // --- Query ---

    pub fn get(&self, id: &SessionId) -> Option<SessionInfo> {
        self.sessions
            .read()
            .expect("sessions lock poisoned")
            .get(id)
            .map(|s| s.to_info(id))
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .read()
            .expect("sessions lock poisoned")
            .iter()
            .map(|(id, state)| state.to_info(id))
            .collect()
    }

    /// Returns all session IDs that should be evaluated (connected + disconnected named).
    /// Sorted by largest max pre_window first (optimization for pre-buffer flush).
    pub fn active_session_ids_sorted_by_pre_window(&self) -> Vec<SessionId> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let mut pairs: Vec<(SessionId, u32)> = sessions
            .iter()
            .filter(|(id, state)| {
                state.connected.load(Ordering::Relaxed) || matches!(id, SessionId::Named(_))
            })
            .map(|(id, state)| (id.clone(), state.triggers.max_pre_window()))
            .collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        pairs.into_iter().map(|(id, _)| id).collect()
    }

    // --- Trigger operations (per-session) ---

    pub fn evaluate_triggers(&self, id: &SessionId, entry: &LogEntry) -> Vec<TriggerMatch> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        match sessions.get(id) {
            Some(state) => state.triggers.evaluate(entry),
            None => Vec::new(),
        }
    }

    pub fn list_triggers(&self, id: &SessionId) -> Vec<TriggerInfo> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        match sessions.get(id) {
            Some(state) => state.triggers.list(),
            None => Vec::new(),
        }
    }

    pub fn add_trigger(
        &self,
        id: &SessionId,
        filter: &str,
        pre: u32,
        post: u32,
        ctx: u32,
        desc: Option<&str>,
    ) -> Result<u32, SessionError> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        Ok(state.triggers.add(filter, pre, post, ctx, desc)?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn edit_trigger(
        &self,
        id: &SessionId,
        trigger_id: u32,
        filter: Option<&str>,
        pre: Option<u32>,
        post: Option<u32>,
        ctx: Option<u32>,
        desc: Option<&str>,
    ) -> Result<TriggerInfo, SessionError> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        Ok(state.triggers.edit(trigger_id, filter, pre, post, ctx, desc)?)
    }

    pub fn remove_trigger(&self, id: &SessionId, trigger_id: u32) -> Result<(), SessionError> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        Ok(state.triggers.remove(trigger_id)?)
    }

    /// Returns the maximum pre_window across ALL sessions' triggers.
    pub fn max_pre_window(&self) -> u32 {
        self.sessions
            .read()
            .expect("sessions lock poisoned")
            .values()
            .map(|s| s.triggers.max_pre_window())
            .max()
            .unwrap_or(0)
    }

    // --- Filter operations (per-session) ---

    pub fn list_filters(&self, id: &SessionId) -> Vec<FilterInfo> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        match sessions.get(id) {
            Some(state) => {
                let filters = state.filters.read().expect("filters lock poisoned");
                filters
                    .iter()
                    .map(|f| FilterInfo {
                        id: f.id,
                        filter_string: f.filter_string.clone(),
                        description: f.description.clone(),
                    })
                    .collect()
            }
            None => Vec::new(),
        }
    }

    pub fn add_filter(
        &self,
        id: &SessionId,
        filter: &str,
        desc: Option<&str>,
    ) -> Result<u32, SessionError> {
        let condition = parse_filter(filter)?;
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        let filter_id = state.next_filter_id.fetch_add(1, Ordering::Relaxed);
        let entry = BufferFilterEntry {
            id: filter_id,
            condition,
            filter_string: filter.to_string(),
            description: desc.map(String::from),
        };
        state
            .filters
            .write()
            .expect("filters lock poisoned")
            .push(entry);
        Ok(filter_id)
    }

    pub fn edit_filter(
        &self,
        id: &SessionId,
        filter_id: u32,
        filter: Option<&str>,
        desc: Option<&str>,
    ) -> Result<FilterInfo, SessionError> {
        // Parse new filter first (fail fast)
        let new_condition = match filter {
            Some(f) => Some((f, parse_filter(f)?)),
            None => None,
        };

        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        let mut filters = state.filters.write().expect("filters lock poisoned");
        let entry = filters
            .iter_mut()
            .find(|f| f.id == filter_id)
            .ok_or_else(|| SessionError::NotFound(format!("filter {filter_id}")))?;

        if let Some((filter_str, condition)) = new_condition {
            entry.filter_string = filter_str.to_string();
            entry.condition = condition;
        }
        if let Some(d) = desc {
            entry.description = Some(d.to_string());
        }

        Ok(FilterInfo {
            id: entry.id,
            filter_string: entry.filter_string.clone(),
            description: entry.description.clone(),
        })
    }

    pub fn remove_filter(&self, id: &SessionId, filter_id: u32) -> Result<(), SessionError> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        let state = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        let mut filters = state.filters.write().expect("filters lock poisoned");
        let pos = filters
            .iter()
            .position(|f| f.id == filter_id)
            .ok_or_else(|| SessionError::NotFound(format!("filter {filter_id}")))?;
        filters.remove(pos);
        Ok(())
    }

    /// Evaluate all sessions' filters against an entry (union/OR semantics).
    /// Returns (should_store, matched_descriptions).
    /// If no sessions have any filters, returns (true, vec![]) -- implicit ALL.
    pub fn evaluate_filters(&self, entry: &LogEntry) -> (bool, Vec<String>) {
        let sessions = self.sessions.read().expect("sessions lock poisoned");

        let mut any_filter_exists = false;
        let mut matched = false;
        let mut descriptions = Vec::new();

        for state in sessions.values() {
            let filters = state.filters.read().expect("filters lock poisoned");
            if filters.is_empty() {
                continue;
            }
            any_filter_exists = true;
            for f in filters.iter() {
                if matches_entry(&f.condition, entry) {
                    matched = true;
                    if let Some(ref desc) = f.description {
                        descriptions.push(desc.clone());
                    }
                }
            }
        }

        if !any_filter_exists {
            // No filters defined anywhere => store everything
            return (true, Vec::new());
        }

        (matched, descriptions)
    }

    // --- Post-trigger window (per-session) ---

    /// Decrement post-window counter. Returns true if post-window was active (entry should skip triggers).
    pub fn decrement_post_window(&self, id: &SessionId) -> bool {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        match sessions.get(id) {
            Some(state) => {
                // Use a CAS loop to atomically decrement only if > 0
                loop {
                    let current = state.post_window_remaining.load(Ordering::Relaxed);
                    if current == 0 {
                        return false;
                    }
                    if state
                        .post_window_remaining
                        .compare_exchange(
                            current,
                            current - 1,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
            None => false,
        }
    }

    pub fn set_post_window(&self, id: &SessionId, count: u32) {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        if let Some(state) = sessions.get(id) {
            state.post_window_remaining.store(count, Ordering::Relaxed);
        }
    }

    /// Returns true if ANY session has an active post-window.
    pub fn any_post_window_active(&self) -> bool {
        self.sessions
            .read()
            .expect("sessions lock poisoned")
            .values()
            .any(|s| s.post_window_remaining.load(Ordering::Relaxed) > 0)
    }

    // --- Notification queue ---

    /// If disconnected named session, push to queue. If connected, do nothing
    /// (the daemon forwards via broadcast channel separately).
    pub fn send_or_queue_notification(&self, id: &SessionId, event: PipelineEvent) {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        if let Some(state) = sessions.get(id) {
            if !state.connected.load(Ordering::Relaxed) {
                // Only queue for disconnected named sessions
                if state.name.is_some() {
                    let mut queue = state
                        .notification_queue
                        .lock()
                        .expect("queue lock poisoned");
                    if queue.len() < state.max_queue_size {
                        queue.push_back(event);
                    }
                }
            }
        }
    }

    pub fn drain_notifications(&self, id: &SessionId) -> Vec<PipelineEvent> {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        match sessions.get(id) {
            Some(state) => {
                let mut queue = state
                    .notification_queue
                    .lock()
                    .expect("queue lock poisoned");
                queue.drain(..).collect()
            }
            None => Vec::new(),
        }
    }

    pub fn queue_notification(&self, id: &SessionId, event: PipelineEvent) {
        let sessions = self.sessions.read().expect("sessions lock poisoned");
        if let Some(state) = sessions.get(id) {
            let mut queue = state
                .notification_queue
                .lock()
                .expect("queue lock poisoned");
            if queue.len() < state.max_queue_size {
                queue.push_back(event);
            }
        }
    }

    // --- Persistence helpers ---

    pub fn restore_named(
        &self,
        name: &str,
        persisted: &crate::daemon::persistence::PersistedSession,
    ) {
        let id = SessionId::Named(name.to_string());
        let state = SessionState::new_named(name);
        state.connected.store(false, Ordering::Relaxed);

        // Restore triggers: clear defaults and add persisted ones
        // Remove default triggers first
        let default_triggers = state.triggers.list();
        for t in &default_triggers {
            let _ = state.triggers.remove(t.id);
        }

        for pt in &persisted.triggers {
            let _ = state.triggers.add(
                &pt.filter,
                pt.pre_window,
                pt.post_window,
                pt.notify_context,
                pt.description.as_deref(),
            );
        }

        // Restore filters
        {
            let mut filters = state.filters.write().expect("filters lock poisoned");
            for pf in &persisted.filters {
                if let Ok(condition) = parse_filter(&pf.filter) {
                    let filter_id = state.next_filter_id.fetch_add(1, Ordering::Relaxed);
                    filters.push(BufferFilterEntry {
                        id: filter_id,
                        condition,
                        filter_string: pf.filter.clone(),
                        description: pf.description.clone(),
                    });
                }
            }
        }

        self.sessions
            .write()
            .expect("sessions lock poisoned")
            .insert(id, state);
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
