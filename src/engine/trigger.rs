use crate::filter::parser::{ParsedFilter, parse_filter, FilterParseError};
use crate::filter::matcher::matches_entry;
use crate::gelf::message::LogEntry;
use std::sync::{RwLock, atomic::{AtomicU32, AtomicU64, Ordering}};
use thiserror::Error;

/// Info snapshot of a trigger (for listing/returning to callers)
#[derive(Debug, Clone)]
pub struct TriggerInfo {
    pub id: u32,
    pub filter_string: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    pub description: Option<String>,
    pub match_count: u64,
}

/// A trigger match result
pub struct TriggerMatch {
    pub id: u32,
    pub description: Option<String>,
    pub filter_string: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
}

struct Trigger {
    id: u32,
    condition: ParsedFilter,
    filter_string: String,
    pre_window: u32,
    post_window: u32,
    notify_context: u32,
    description: Option<String>,
    match_count: AtomicU64,
}

impl Trigger {
    fn to_info(&self) -> TriggerInfo {
        TriggerInfo {
            id: self.id,
            filter_string: self.filter_string.clone(),
            pre_window: self.pre_window,
            post_window: self.post_window,
            notify_context: self.notify_context,
            description: self.description.clone(),
            match_count: self.match_count.load(Ordering::Relaxed),
        }
    }
}

pub struct TriggerManager {
    triggers: RwLock<Vec<Trigger>>,
    next_id: AtomicU32,
}

#[derive(Debug, Error)]
pub enum TriggerError {
    #[error("trigger not found: {0}")]
    NotFound(u32),
    #[error("invalid filter: {0}")]
    InvalidFilter(#[from] FilterParseError),
}

impl TriggerManager {
    pub fn new() -> Self {
        let trigger1 = Trigger {
            id: 1,
            condition: parse_filter("l>=ERROR").expect("default filter 1 must be valid"),
            filter_string: "l>=ERROR".to_string(),
            pre_window: 500,
            post_window: 200,
            notify_context: 5,
            description: Some("Error-level log detected".to_string()),
            match_count: AtomicU64::new(0),
        };

        let trigger2 = Trigger {
            id: 2,
            condition: parse_filter("/panic|unwrap failed|stack backtrace/")
                .expect("default filter 2 must be valid"),
            filter_string: "/panic|unwrap failed|stack backtrace/".to_string(),
            pre_window: 500,
            post_window: 200,
            notify_context: 5,
            description: Some("Panic or unwrap failure detected".to_string()),
            match_count: AtomicU64::new(0),
        };

        TriggerManager {
            triggers: RwLock::new(vec![trigger1, trigger2]),
            next_id: AtomicU32::new(3),
        }
    }

    pub fn list(&self) -> Vec<TriggerInfo> {
        self.triggers
            .read()
            .expect("triggers lock poisoned")
            .iter()
            .map(|t| t.to_info())
            .collect()
    }

    pub fn get(&self, id: u32) -> Option<TriggerInfo> {
        self.triggers
            .read()
            .expect("triggers lock poisoned")
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.to_info())
    }

    pub fn add(
        &self,
        filter_str: &str,
        pre_window: u32,
        post_window: u32,
        notify_context: u32,
        description: Option<&str>,
    ) -> Result<u32, TriggerError> {
        let condition = parse_filter(filter_str)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let trigger = Trigger {
            id,
            condition,
            filter_string: filter_str.to_string(),
            pre_window,
            post_window,
            notify_context,
            description: description.map(String::from),
            match_count: AtomicU64::new(0),
        };
        self.triggers
            .write()
            .expect("triggers lock poisoned")
            .push(trigger);
        Ok(id)
    }

    pub fn edit(
        &self,
        id: u32,
        filter: Option<&str>,
        pre_window: Option<u32>,
        post_window: Option<u32>,
        notify_context: Option<u32>,
        description: Option<&str>,
    ) -> Result<TriggerInfo, TriggerError> {
        // Parse filter first (before acquiring write lock) to fail fast on bad input
        let new_condition = match filter {
            Some(f) => Some((f, parse_filter(f)?)),
            None => None,
        };

        let mut triggers = self.triggers.write().expect("triggers lock poisoned");
        let trigger = triggers
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(TriggerError::NotFound(id))?;

        if let Some((filter_str, condition)) = new_condition {
            trigger.filter_string = filter_str.to_string();
            trigger.condition = condition;
        }
        if let Some(pw) = pre_window {
            trigger.pre_window = pw;
        }
        if let Some(pw) = post_window {
            trigger.post_window = pw;
        }
        if let Some(nc) = notify_context {
            trigger.notify_context = nc;
        }
        if let Some(desc) = description {
            trigger.description = Some(desc.to_string());
        }

        Ok(trigger.to_info())
    }

    pub fn remove(&self, id: u32) -> Result<(), TriggerError> {
        let mut triggers = self.triggers.write().expect("triggers lock poisoned");
        let pos = triggers
            .iter()
            .position(|t| t.id == id)
            .ok_or(TriggerError::NotFound(id))?;
        triggers.remove(pos);
        Ok(())
    }

    pub fn max_pre_window(&self) -> u32 {
        self.triggers
            .read()
            .expect("triggers lock poisoned")
            .iter()
            .map(|t| t.pre_window)
            .max()
            .unwrap_or(0)
    }

    pub fn evaluate(&self, entry: &LogEntry) -> Vec<TriggerMatch> {
        let triggers = self.triggers.read().expect("triggers lock poisoned");
        triggers
            .iter()
            .filter_map(|t| {
                if matches_entry(&t.condition, entry) {
                    t.match_count.fetch_add(1, Ordering::Relaxed);
                    Some(TriggerMatch {
                        id: t.id,
                        description: t.description.clone(),
                        filter_string: t.filter_string.clone(),
                        pre_window: t.pre_window,
                        post_window: t.post_window,
                        notify_context: t.notify_context,
                    })
                } else {
                    None
                }
            })
            .collect()
    }
}

impl Default for TriggerManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gelf::message::{Level, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_entry(level: Level, msg: &str) -> LogEntry {
        LogEntry {
            seq: 1, timestamp: Utc::now(), level,
            message: msg.to_string(), full_message: None,
            host: "test".into(), facility: None, file: None, line: None,
            additional_fields: HashMap::new(),
            matched_filters: Vec::new(), source: LogSource::Filter,
        }
    }

    #[test]
    fn test_default_triggers() {
        let mgr = TriggerManager::new();
        let triggers = mgr.list();
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].id, 1);
        assert_eq!(triggers[1].id, 2);
    }

    #[test]
    fn test_add_trigger() {
        let mgr = TriggerManager::new();
        let id = mgr.add("fa=mqtt", 500, 200, 5, Some("MQTT watch")).unwrap();
        assert_eq!(id, 3);
        assert_eq!(mgr.list().len(), 3);
    }

    #[test]
    fn test_remove_trigger() {
        let mgr = TriggerManager::new();
        assert!(mgr.remove(1).is_ok());
        assert_eq!(mgr.list().len(), 1);
        assert!(mgr.remove(99).is_err());
    }

    #[test]
    fn test_edit_trigger() {
        let mgr = TriggerManager::new();
        let result = mgr.edit(1, Some("l>=WARN"), Some(100), Some(50), Some(3), Some("edited")).unwrap();
        assert_eq!(result.pre_window, 100);
        assert_eq!(result.post_window, 50);
        assert_eq!(result.notify_context, 3);
        assert_eq!(result.description.as_deref(), Some("edited"));
    }

    #[test]
    fn test_max_pre_window() {
        let mgr = TriggerManager::new();
        assert_eq!(mgr.max_pre_window(), 500); // default triggers have 500
        mgr.add("fa=test", 1000, 200, 5, None).unwrap();
        assert_eq!(mgr.max_pre_window(), 1000);
    }

    #[test]
    fn test_evaluate_matches_error() {
        let mgr = TriggerManager::new();
        let entry = make_entry(Level::Error, "something failed");
        let matches = mgr.evaluate(&entry);
        assert!(!matches.is_empty());
        // Should match trigger 1 (l>=ERROR)
        assert!(matches.iter().any(|m| m.id == 1));
    }

    #[test]
    fn test_evaluate_no_match() {
        let mgr = TriggerManager::new();
        let entry = make_entry(Level::Info, "all good");
        let matches = mgr.evaluate(&entry);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_evaluate_increments_match_count() {
        let mgr = TriggerManager::new();
        let entry = make_entry(Level::Error, "fail");
        mgr.evaluate(&entry);
        mgr.evaluate(&entry);
        let info = mgr.get(1).unwrap();
        assert_eq!(info.match_count, 2);
    }

    #[test]
    fn test_evaluate_panic_trigger() {
        let mgr = TriggerManager::new();
        let entry = make_entry(Level::Debug, "thread panicked: unwrap failed");
        let matches = mgr.evaluate(&entry);
        assert!(matches.iter().any(|m| m.id == 2));
    }

    #[test]
    fn test_get_nonexistent() {
        let mgr = TriggerManager::new();
        assert!(mgr.get(99).is_none());
    }
}
