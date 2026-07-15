use chrono::Utc;
use logmon_broker_core::filter::matcher::matches_span;
use logmon_broker_core::filter::parser::*;
use logmon_broker_core::span::types::*;
use std::collections::HashMap;

// Parser tests
#[test]
fn test_parse_span_name_selector() {
    let f = parse_filter("sn=query_database").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::SpanName, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_service_name_selector() {
    let f = parse_filter("sv=store_server").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::ServiceName, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_status_selector() {
    let f = parse_filter("st=error").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::SpanStatus, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_kind_selector() {
    let f = parse_filter("sk=server").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(
            &qs[0],
            Qualifier::SelectorPattern(Selector::SpanKind, _)
        ));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_duration_gte() {
    let f = parse_filter("d>=100").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::DurationFilter(DurationOp::Gte, d) if *d == 100.0));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_duration_lte() {
    let f = parse_filter("d<=50.5").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::DurationFilter(DurationOp::Lte, d) if *d == 50.5));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_combined_span_filter() {
    let f = parse_filter("sv=store_server,d>=100").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 2);
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_is_span_filter() {
    assert!(is_span_filter(&parse_filter("sn=query").unwrap()));
    assert!(is_span_filter(&parse_filter("d>=100").unwrap()));
    assert!(!is_span_filter(&parse_filter("l>=ERROR").unwrap()));
    assert!(!is_span_filter(&parse_filter("fa=mqtt").unwrap()));
}

#[test]
fn test_mixed_log_span_selectors_error() {
    let result = parse_filter("l>=ERROR,sn=query");
    assert!(result.is_err());
}

#[test]
fn test_attribute_match_unknown_selector() {
    let f = parse_filter("http.method=POST").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(
            matches!(&qs[0], Qualifier::SelectorPattern(Selector::AdditionalField(name), _) if name == "http.method")
        );
    } else {
        panic!("expected Qualifiers");
    }
}

// Matcher tests
fn make_span(name: &str, service: &str, duration_ms: f64, status: SpanStatus) -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 0xabc_u128,
        span_id: 0xdef_u64,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms,
        name: name.to_string(),
        kind: SpanKind::Server,
        service_name: service.to_string(),
        status,
        attributes: HashMap::new(),
        events: vec![],
    }
}

#[test]
fn test_match_span_name() {
    let f = parse_filter("sn=query_database").unwrap();
    assert!(matches_span(
        &f,
        &make_span("query_database", "svc", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_span_name_no_match() {
    let f = parse_filter("sn=query_database").unwrap();
    assert!(!matches_span(
        &f,
        &make_span("authenticate", "svc", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_service_name() {
    let f = parse_filter("sv=store_server").unwrap();
    assert!(matches_span(
        &f,
        &make_span("op", "store_server", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_duration_gte() {
    let f = parse_filter("d>=100").unwrap();
    assert!(matches_span(
        &f,
        &make_span("op", "svc", 150.0, SpanStatus::Ok)
    ));
    assert!(!matches_span(
        &f,
        &make_span("op", "svc", 50.0, SpanStatus::Ok)
    ));
    assert!(matches_span(
        &f,
        &make_span("op", "svc", 100.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_status_error() {
    let f = parse_filter("st=error").unwrap();
    assert!(matches_span(
        &f,
        &make_span("op", "svc", 10.0, SpanStatus::Error("fail".into()))
    ));
    assert!(!matches_span(
        &f,
        &make_span("op", "svc", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_status_error_substring() {
    let f = parse_filter("st=timeout").unwrap();
    assert!(matches_span(
        &f,
        &make_span(
            "op",
            "svc",
            10.0,
            SpanStatus::Error("connection timeout".into())
        )
    ));
    assert!(!matches_span(
        &f,
        &make_span("op", "svc", 10.0, SpanStatus::Error("null pointer".into()))
    ));
}

#[test]
fn test_match_kind() {
    let f = parse_filter("sk=server").unwrap();
    assert!(matches_span(
        &f,
        &make_span("op", "svc", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_bare_text_against_span_name() {
    let f = parse_filter("query").unwrap();
    assert!(matches_span(
        &f,
        &make_span("query_database", "svc", 10.0, SpanStatus::Ok)
    ));
    assert!(!matches_span(
        &f,
        &make_span("authenticate", "svc", 10.0, SpanStatus::Ok)
    ));
}

#[test]
fn test_match_attribute() {
    let f = parse_filter("http.method=POST").unwrap();
    let mut span = make_span("op", "svc", 10.0, SpanStatus::Ok);
    span.attributes
        .insert("http.method".to_string(), serde_json::json!("POST"));
    assert!(matches_span(&f, &span));
}

#[test]
fn test_match_combined_span_filter() {
    let f = parse_filter("sv=store_server,d>=100").unwrap();
    assert!(matches_span(
        &f,
        &make_span("op", "store_server", 150.0, SpanStatus::Ok)
    ));
    assert!(!matches_span(
        &f,
        &make_span("op", "store_server", 50.0, SpanStatus::Ok)
    ));
    assert!(!matches_span(
        &f,
        &make_span("op", "other", 150.0, SpanStatus::Ok)
    ));
}
