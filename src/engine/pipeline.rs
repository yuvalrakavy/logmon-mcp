use crate::engine::pre_buffer::PreTriggerBuffer;
use crate::engine::trigger::{TriggerManager, TriggerInfo};
use crate::filter::matcher::matches_entry;
use crate::filter::parser::{ParsedFilter, parse_filter, FilterParseError};
use crate::gelf::message::{LogEntry, LogSource};
use crate::store::memory::InMemoryStore;
use crate::store::traits::LogStore;
use std::sync::{RwLock, atomic::{AtomicU32, AtomicU64, Ordering}};
use thiserror::Error;

pub struct PipelineEvent {
    pub trigger_id: u32,
    pub trigger_description: Option<String>,
    pub filter_string: String,
    pub matched_entry: LogEntry,
    pub context_before: Vec<LogEntry>,
    pub pre_trigger_flushed: usize,
    pub post_window_size: u32,
}

pub struct FilterInfo {
    pub id: u32,
    pub filter_string: String,
    pub description: Option<String>,
}

struct BufferFilterEntry {
    id: u32,
    condition: ParsedFilter,
    filter_string: String,
    description: Option<String>,
}

pub struct LogPipeline {
    store: InMemoryStore,
    pre_buffer: PreTriggerBuffer,
    triggers: TriggerManager,
    filters: RwLock<Vec<BufferFilterEntry>>,
    post_window_remaining: AtomicU32,
    next_filter_id: AtomicU32,
    seq_counter: AtomicU64,
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("filter not found: {0}")]
    FilterNotFound(u32),
    #[error("invalid filter: {0}")]
    InvalidFilter(#[from] FilterParseError),
    #[error("trigger error: {0}")]
    TriggerError(#[from] crate::engine::trigger::TriggerError),
}

impl LogPipeline {
    pub fn new(store_capacity: usize) -> Self {
        let triggers = TriggerManager::new();
        let max_pre = triggers.max_pre_window() as usize;
        Self {
            store: InMemoryStore::new(store_capacity),
            pre_buffer: PreTriggerBuffer::new(max_pre),
            triggers,
            filters: RwLock::new(Vec::new()),
            post_window_remaining: AtomicU32::new(0),
            next_filter_id: AtomicU32::new(1),
            seq_counter: AtomicU64::new(0),
        }
    }

    pub fn assign_seq(&self) -> u64 {
        self.seq_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn store_len(&self) -> usize {
        self.store.len()
    }

    pub fn recent_logs(&self, count: usize, filter_str: Option<&str>) -> Vec<LogEntry> {
        let parsed = filter_str.and_then(|s| parse_filter(s).ok());
        self.store.recent(count, parsed.as_ref())
    }

    pub fn process(&self, mut entry: LogEntry) -> Vec<PipelineEvent> {
        let post_remaining = self.post_window_remaining.load(Ordering::Relaxed);

        if post_remaining > 0 {
            // Post-trigger window active: bypass filters and trigger evaluation
            entry.source = LogSource::PostTrigger;
            self.store.append(entry.clone());
            self.pre_buffer.append(entry);
            self.post_window_remaining.fetch_sub(1, Ordering::Relaxed);
            return Vec::new();
        }

        // Normal flow: add to pre-buffer first
        self.pre_buffer.append(entry.clone());

        // Evaluate triggers
        let matches = self.triggers.evaluate(&entry);
        let mut events = Vec::new();

        if !matches.is_empty() {
            // Find largest pre_window, post_window, and notify_context across all matching triggers
            let max_pre = matches.iter().map(|m| m.pre_window).max().unwrap();
            let max_post = matches.iter().map(|m| m.post_window).max().unwrap();
            let max_notify = matches.iter().map(|m| m.notify_context).max().unwrap();

            // Flush pre-buffer entries into store only if there are enough entries
            // to provide meaningful context (at least max_notify_context entries).
            // This prevents rescuing a single unrelated pre-buffer entry when the
            // pre-trigger context is too shallow to be useful.
            if self.pre_buffer.len() > max_notify as usize {
                let flushed = self.pre_buffer.flush(max_pre as usize);
                for mut flushed_entry in flushed {
                    if !self.store.contains_seq(flushed_entry.seq) {
                        flushed_entry.source = LogSource::PreTrigger;
                        self.store.append(flushed_entry);
                    }
                }
            } else {
                // Not enough pre-buffer entries for context; clear without storing
                let _ = self.pre_buffer.flush(max_pre as usize);
            }

            // Store the triggering entry itself (if not already stored by pre-buffer flush)
            if !self.store.contains_seq(entry.seq) {
                let mut trigger_entry = entry.clone();
                trigger_entry.source = LogSource::PreTrigger;
                self.store.append(trigger_entry);
            }

            // Activate post-window
            self.post_window_remaining.store(max_post, Ordering::Relaxed);

            // Create events for each matching trigger
            for m in matches {
                let context = self.store.context_by_seq(entry.seq, m.notify_context as usize, 0);
                let context_before: Vec<LogEntry> = context.into_iter()
                    .filter(|e| e.seq != entry.seq)
                    .collect();

                events.push(PipelineEvent {
                    trigger_id: m.id,
                    trigger_description: m.description,
                    filter_string: m.filter_string,
                    matched_entry: entry.clone(),
                    context_before,
                    pre_trigger_flushed: max_pre as usize,
                    post_window_size: m.post_window,
                });
            }

            return events;
        }

        // Evaluate buffer filters
        let filters = self.filters.read().unwrap();
        if filters.is_empty() {
            // No filters = store everything (implicit ALL)
            self.store.append(entry);
        } else {
            // OR semantics: store if any filter matches
            let mut matched_descriptions = Vec::new();
            let mut any_match = false;
            for f in filters.iter() {
                if matches_entry(&f.condition, &entry) {
                    any_match = true;
                    if let Some(desc) = &f.description {
                        matched_descriptions.push(desc.clone());
                    }
                }
            }
            if any_match {
                entry.matched_filters = matched_descriptions;
                entry.source = LogSource::Filter;
                self.store.append(entry);
            }
        }

        events
    }

    // --- Filter management ---

    pub fn add_filter(&self, filter_str: &str, description: Option<&str>) -> Result<u32, PipelineError> {
        let condition = parse_filter(filter_str)?;
        let id = self.next_filter_id.fetch_add(1, Ordering::Relaxed);
        let entry = BufferFilterEntry {
            id,
            condition,
            filter_string: filter_str.to_string(),
            description: description.map(String::from),
        };
        self.filters.write().unwrap().push(entry);
        Ok(id)
    }

    pub fn edit_filter(&self, id: u32, filter_str: Option<&str>, description: Option<&str>) -> Result<FilterInfo, PipelineError> {
        // Parse filter before acquiring write lock to fail fast
        let new_condition = match filter_str {
            Some(s) => Some((s, parse_filter(s)?)),
            None => None,
        };

        let mut filters = self.filters.write().unwrap();
        let f = filters.iter_mut().find(|f| f.id == id)
            .ok_or(PipelineError::FilterNotFound(id))?;

        if let Some((s, cond)) = new_condition {
            f.filter_string = s.to_string();
            f.condition = cond;
        }
        if let Some(desc) = description {
            f.description = Some(desc.to_string());
        }

        Ok(FilterInfo {
            id: f.id,
            filter_string: f.filter_string.clone(),
            description: f.description.clone(),
        })
    }

    pub fn remove_filter(&self, id: u32) -> Result<(), PipelineError> {
        let mut filters = self.filters.write().unwrap();
        let pos = filters.iter().position(|f| f.id == id)
            .ok_or(PipelineError::FilterNotFound(id))?;
        filters.remove(pos);
        Ok(())
    }

    pub fn list_filters(&self) -> Vec<FilterInfo> {
        self.filters.read().unwrap().iter().map(|f| FilterInfo {
            id: f.id,
            filter_string: f.filter_string.clone(),
            description: f.description.clone(),
        }).collect()
    }

    // --- Trigger management (delegates to TriggerManager, then resizes pre_buffer) ---

    pub fn add_trigger(
        &self,
        filter_str: &str,
        pre_window: u32,
        post_window: u32,
        notify_context: u32,
        description: Option<&str>,
    ) -> Result<u32, PipelineError> {
        let id = self.triggers.add(filter_str, pre_window, post_window, notify_context, description)?;
        self.pre_buffer.resize(self.triggers.max_pre_window() as usize);
        Ok(id)
    }

    pub fn edit_trigger(
        &self,
        id: u32,
        filter: Option<&str>,
        pre_window: Option<u32>,
        post_window: Option<u32>,
        notify_context: Option<u32>,
        description: Option<&str>,
    ) -> Result<TriggerInfo, PipelineError> {
        let info = self.triggers.edit(id, filter, pre_window, post_window, notify_context, description)?;
        self.pre_buffer.resize(self.triggers.max_pre_window() as usize);
        Ok(info)
    }

    pub fn remove_trigger(&self, id: u32) -> Result<(), PipelineError> {
        self.triggers.remove(id)?;
        self.pre_buffer.resize(self.triggers.max_pre_window() as usize);
        Ok(())
    }

    pub fn list_triggers(&self) -> Vec<TriggerInfo> {
        self.triggers.list()
    }
}
