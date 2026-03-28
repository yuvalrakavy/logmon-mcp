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
        Pattern::Substring(s) => text.to_lowercase().contains(s.as_str()),
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
                        .map_or(false, |v| matches_pattern(pattern, v))
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
    }
}
