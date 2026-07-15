use chrono::Utc;
use logmon_broker_core::filter::matcher::matches_entry;
use logmon_broker_core::filter::parser::*;
use logmon_broker_core::gelf::message::{Level, LogEntry, LogSource};
use std::collections::HashMap;

fn test_entry(level: Level, message: &str, host: &str) -> LogEntry {
    LogEntry {
        seq: 1,
        timestamp: Utc::now(),
        level,
        message: message.to_string(),
        full_message: None,
        host: host.to_string(),
        facility: Some("test::module".to_string()),
        file: Some("test.rs".to_string()),
        line: Some(42),
        additional_fields: HashMap::new(),
        trace_id: None,
        span_id: None,
        matched_filters: Vec::new(),
        source: LogSource::Filter,
    }
}

#[test]
fn test_parse_all() {
    let f = parse_filter("ALL").unwrap();
    assert!(matches!(f, ParsedFilter::All));
}

#[test]
fn test_parse_none() {
    let f = parse_filter("NONE").unwrap();
    assert!(matches!(f, ParsedFilter::None));
}

#[test]
fn test_parse_bare_substring() {
    let f = parse_filter("bug").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "bug"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_bare_regex() {
    let f = parse_filter("/^ERROR.*timeout/").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(
            &qs[0],
            Qualifier::BarePattern(Pattern::Regex {
                case_insensitive: false,
                ..
            })
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_regex_case_insensitive() {
    let f = parse_filter("/pattern/i").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::BarePattern(Pattern::Regex {
                case_insensitive: true,
                ..
            })
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_selector_pattern() {
    let f = parse_filter("h=myapp").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(
            matches!(&qs[0], Qualifier::SelectorPattern(Selector::Host, Pattern::Substring(s)) if s == "myapp")
        );
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_level_selector() {
    let f = parse_filter("l>=WARN").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::LevelFilter { .. }));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_multiple_qualifiers() {
    let f = parse_filter("bug,h=myapp,l>=ERROR").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 3);
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_quoted_pattern_with_comma() {
    let f = parse_filter("\"connection refused, retrying\"").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(
            matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "connection refused, retrying")
        );
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_quoted_pattern_with_equals() {
    let f = parse_filter("\"key=value\"").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(
            matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "key=value")
        );
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_mfm_selector() {
    let f = parse_filter("mfm=timeout").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::MessageOrFull, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_custom_additional_field() {
    let f = parse_filter("request_id=abc").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(
            matches!(&qs[0], Qualifier::SelectorPattern(Selector::AdditionalField(name), _) if name == "request_id")
        );
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_empty_string_error() {
    assert!(parse_filter("").is_err());
}

#[test]
fn test_parse_level_eq() {
    let f = parse_filter("l=DEBUG").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::LevelFilter {
                op: LevelOp::Eq,
                ..
            }
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_level_lte() {
    let f = parse_filter("l<=INFO").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::LevelFilter {
                op: LevelOp::Lte,
                ..
            }
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_selector_with_regex() {
    let f = parse_filter("fa=/mqtt.*/i").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(
                Selector::Facility,
                Pattern::Regex {
                    case_insensitive: true,
                    ..
                }
            )
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_file_and_line_selectors() {
    let f = parse_filter("fi=test.rs,ln=42").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 2);
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::File, _)
        ));
        assert!(matches!(
            &qs[1],
            Qualifier::SelectorPattern(Selector::Line, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

// --- Matcher tests ---

#[test]
fn test_match_all() {
    let f = parse_filter("ALL").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Debug, "hello", "app")));
}

#[test]
fn test_match_none() {
    let f = parse_filter("NONE").unwrap();
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Debug, "hello", "app")
    ));
}

#[test]
fn test_match_substring_case_insensitive() {
    let f = parse_filter("BUG").unwrap();
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "found a bug here", "app")
    ));
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "BUG report", "app")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Info, "all good", "app")
    ));
}

#[test]
fn test_match_host_selector() {
    let f = parse_filter("h=myapp").unwrap();
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "hello", "myapp")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Info, "hello", "other")
    ));
}

#[test]
fn test_match_level_gte() {
    let f = parse_filter("l>=WARN").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Error, "err", "app")));
    assert!(matches_entry(&f, &test_entry(Level::Warn, "warn", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Info, "info", "app")));
}

#[test]
fn test_match_combined_qualifiers() {
    let f = parse_filter("timeout,h=myapp,l>=WARN").unwrap();
    assert!(matches_entry(
        &f,
        &test_entry(Level::Error, "connection timeout", "myapp")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Error, "connection timeout", "other")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Info, "connection timeout", "myapp")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Error, "success", "myapp")
    ));
}

#[test]
fn test_match_facility_selector() {
    let f = parse_filter("fa=test").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
}

#[test]
fn test_match_file_selector() {
    let f = parse_filter("fi=test.rs").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
}

#[test]
fn test_match_regex() {
    let f = parse_filter("/^found.*bug/i").unwrap();
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "Found a Bug here", "app")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Info, "no match", "app")
    ));
}

#[test]
fn test_bare_substring_matches_all_fields() {
    let f = parse_filter("myapp").unwrap();
    // Should match host field too
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "something", "myapp")
    ));
}

#[test]
fn test_match_line_selector() {
    let f = parse_filter("ln=42").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
    let f2 = parse_filter("ln=99").unwrap();
    assert!(!matches_entry(&f2, &test_entry(Level::Info, "msg", "app")));
}

#[test]
fn test_match_level_eq() {
    let f = parse_filter("l=INFO").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Warn, "msg", "app")));
}

#[test]
fn test_match_level_lte() {
    let f = parse_filter("l<=INFO").unwrap();
    assert!(matches_entry(&f, &test_entry(Level::Info, "msg", "app")));
    assert!(matches_entry(&f, &test_entry(Level::Debug, "msg", "app")));
    assert!(!matches_entry(&f, &test_entry(Level::Warn, "msg", "app")));
}

#[test]
fn test_match_mfm_selector() {
    let f = parse_filter("mfm=timeout").unwrap();
    assert!(matches_entry(
        &f,
        &test_entry(Level::Info, "connection timeout", "app")
    ));
    assert!(!matches_entry(
        &f,
        &test_entry(Level::Info, "success", "app")
    ));
}
