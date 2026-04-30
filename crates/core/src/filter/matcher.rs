use crate::filter::parser::*;
use crate::gelf::message::LogEntry;
use crate::span::types::SpanEntry;

/// Check if a LogEntry matches a ParsedFilter
pub fn matches_entry(filter: &ParsedFilter, entry: &LogEntry) -> bool {
    match filter {
        ParsedFilter::All => true,
        ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qs) => qs.iter().all(|q| matches_qualifier(q, entry)),
    }
}

fn matches_qualifier(qualifier: &Qualifier, entry: &LogEntry) -> bool {
    match qualifier {
        Qualifier::BarePattern(pattern) => matches_any_field(pattern, entry),
        Qualifier::SelectorPattern(selector, pattern) => matches_selector(selector, pattern, entry),
        Qualifier::LevelFilter { op, level } => matches_level(*op, *level, entry.level),
        Qualifier::DurationFilter(..) => false, // duration only applies to spans
        Qualifier::BookmarkFilter { .. } => false,
        Qualifier::TimestampFilter { op, ts } => match op {
            BookmarkOp::Gte => entry.timestamp >= *ts,
            BookmarkOp::Lte => entry.timestamp <= *ts,
        },
    }
}

fn matches_any_field(pattern: &Pattern, entry: &LogEntry) -> bool {
    if matches_pattern(pattern, &entry.message) {
        return true;
    }
    if let Some(ref fm) = entry.full_message {
        if matches_pattern(pattern, fm) {
            return true;
        }
    }
    if matches_pattern(pattern, &entry.host) {
        return true;
    }
    if let Some(ref facility) = entry.facility {
        if matches_pattern(pattern, facility) {
            return true;
        }
    }
    if let Some(ref file) = entry.file {
        if matches_pattern(pattern, file) {
            return true;
        }
    }
    if let Some(line) = entry.line {
        if matches_pattern(pattern, &line.to_string()) {
            return true;
        }
    }
    for value in entry.additional_fields.values() {
        let s = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if matches_pattern(pattern, &s) {
            return true;
        }
    }
    false
}

fn matches_selector(selector: &Selector, pattern: &Pattern, entry: &LogEntry) -> bool {
    match selector {
        Selector::Message => matches_pattern(pattern, &entry.message),
        Selector::FullMessage => entry
            .full_message
            .as_deref()
            .map(|fm| matches_pattern(pattern, fm))
            .unwrap_or(false),
        Selector::MessageOrFull => {
            matches_pattern(pattern, &entry.message)
                || entry
                    .full_message
                    .as_deref()
                    .map(|fm| matches_pattern(pattern, fm))
                    .unwrap_or(false)
        }
        Selector::Host => matches_pattern(pattern, &entry.host),
        Selector::Facility => entry
            .facility
            .as_deref()
            .map(|f| matches_pattern(pattern, f))
            .unwrap_or(false),
        Selector::File => entry
            .file
            .as_deref()
            .map(|f| matches_pattern(pattern, f))
            .unwrap_or(false),
        Selector::Line => entry
            .line
            .map(|ln| matches_pattern(pattern, &ln.to_string()))
            .unwrap_or(false),
        Selector::AdditionalField(name) => entry
            .additional_fields
            .get(name)
            .map(|value| {
                let s = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                matches_pattern(pattern, &s)
            })
            .unwrap_or(false),
        Selector::SpanName | Selector::ServiceName | Selector::SpanStatus | Selector::SpanKind => false,
    }
}

fn matches_pattern(pattern: &Pattern, text: &str) -> bool {
    match pattern {
        // Lowercase both sides: the DSL string parser (parser.rs:246)
        // already lowercases substrings, but the JSON deserializer path
        // (parser.rs:107) does not, so any programmatic `add_filter` call
        // with an uppercase character would silently never match.
        // Lowercasing here makes the invariant hold for all call sites.
        Pattern::Substring(s) => text.to_lowercase().contains(&s.to_lowercase()),
        Pattern::Regex { compiled, .. } => compiled.is_match(text),
    }
}

fn matches_level(op: LevelOp, filter_level: crate::gelf::message::Level, entry_level: crate::gelf::message::Level) -> bool {
    match op {
        LevelOp::Eq => entry_level == filter_level,
        LevelOp::Gte => entry_level >= filter_level,
        LevelOp::Lte => entry_level <= filter_level,
    }
}

/// Check if a SpanEntry matches a ParsedFilter
pub fn matches_span(filter: &ParsedFilter, span: &SpanEntry) -> bool {
    match filter {
        ParsedFilter::All => true,
        ParsedFilter::None => false,
        ParsedFilter::Qualifiers(qualifiers) => {
            qualifiers.iter().all(|q| matches_span_qualifier(q, span))
        }
    }
}

fn matches_span_qualifier(qualifier: &Qualifier, span: &SpanEntry) -> bool {
    match qualifier {
        Qualifier::BarePattern(pattern) => {
            matches_pattern(pattern, &span.name)
        }
        Qualifier::SelectorPattern(selector, pattern) => {
            match selector {
                Selector::SpanName => matches_pattern(pattern, &span.name),
                Selector::ServiceName => matches_pattern(pattern, &span.service_name),
                Selector::SpanStatus => {
                    let pat_lower = match pattern {
                        Pattern::Substring(s) => s.clone(),
                        Pattern::Regex { source, .. } => source.to_lowercase(),
                    };
                    match &span.status {
                        crate::span::types::SpanStatus::Error(msg) => {
                            pat_lower == "error" || matches_pattern(pattern, msg)
                        }
                        crate::span::types::SpanStatus::Ok => pat_lower == "ok",
                        crate::span::types::SpanStatus::Unset => pat_lower == "unset",
                    }
                }
                Selector::SpanKind => {
                    let kind_str = format!("{:?}", span.kind).to_lowercase();
                    matches_pattern(pattern, &kind_str)
                }
                Selector::AdditionalField(key) => {
                    span.attributes.get(key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|v| matches_pattern(pattern, v))
                }
                _ => false, // log selectors don't match spans
            }
        }
        Qualifier::DurationFilter(op, threshold) => {
            match op {
                DurationOp::Gte => span.duration_ms >= *threshold,
                DurationOp::Lte => span.duration_ms <= *threshold,
            }
        }
        Qualifier::LevelFilter { .. } => false, // log-only
        Qualifier::BookmarkFilter { .. } => false,
        Qualifier::TimestampFilter { op, ts } => match op {
            BookmarkOp::Gte => span.start_time >= *ts,
            BookmarkOp::Lte => span.start_time <= *ts,
        },
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;
    use crate::gelf::message::{LogEntry, Level, LogSource};
    use chrono::Utc;
    use std::collections::HashMap;

    fn log_entry_at(ts: chrono::DateTime<chrono::Utc>) -> LogEntry {
        LogEntry {
            seq: 1,
            timestamp: ts,
            level: Level::Info,
            message: "m".into(),
            full_message: None,
            host: "h".into(),
            facility: None,
            file: None,
            line: None,
            additional_fields: HashMap::new(),
            trace_id: None,
            span_id: None,
            matched_filters: Vec::new(),
            source: LogSource::Filter,
        }
    }

    #[test]
    fn timestamp_gte_matches_entry_at_or_after() {
        let cutoff = Utc::now() - chrono::Duration::seconds(10);
        let q = Qualifier::TimestampFilter { op: BookmarkOp::Gte, ts: cutoff };
        assert!(matches_qualifier(&q, &log_entry_at(Utc::now())));
        assert!(!matches_qualifier(&q, &log_entry_at(cutoff - chrono::Duration::seconds(5))));
    }

    #[test]
    fn timestamp_lte_matches_entry_at_or_before() {
        let cutoff = Utc::now();
        let q = Qualifier::TimestampFilter { op: BookmarkOp::Lte, ts: cutoff };
        assert!(matches_qualifier(&q, &log_entry_at(cutoff - chrono::Duration::seconds(5))));
        assert!(!matches_qualifier(&q, &log_entry_at(cutoff + chrono::Duration::seconds(5))));
    }

    #[test]
    fn bookmark_filter_returns_false_in_matcher() {
        // BookmarkFilter should be resolved away before matching; if it
        // somehow reaches the matcher, it must return false (safe default).
        let q = Qualifier::BookmarkFilter {
            op: BookmarkOp::Gte,
            name: "x".into(),
        };
        assert!(!matches_qualifier(&q, &log_entry_at(Utc::now())));
    }
}
