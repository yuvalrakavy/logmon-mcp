use gelf_mcp_server::filter::parser::*;

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
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Regex { case_insensitive: false, .. })));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_regex_case_insensitive() {
    let f = parse_filter("/pattern/i").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Regex { case_insensitive: true, .. })));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_selector_pattern() {
    let f = parse_filter("h=myapp").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::Host, Pattern::Substring(s)) if s == "myapp"));
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
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "connection refused, retrying"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_quoted_pattern_with_equals() {
    let f = parse_filter("\"key=value\"").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 1);
        assert!(matches!(&qs[0], Qualifier::BarePattern(Pattern::Substring(s)) if s == "key=value"));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_mfm_selector() {
    let f = parse_filter("mfm=timeout").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::MessageOrFull, _)));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_custom_additional_field() {
    let f = parse_filter("request_id=abc").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::AdditionalField(name), _) if name == "request_id"));
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
        assert!(matches!(&qs[0], Qualifier::LevelFilter { op: LevelOp::Eq, .. }));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_level_lte() {
    let f = parse_filter("l<=INFO").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::LevelFilter { op: LevelOp::Lte, .. }));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_selector_with_regex() {
    let f = parse_filter("fa=/mqtt.*/i").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::Facility, Pattern::Regex { case_insensitive: true, .. })));
    } else {
        panic!("expected Qualifiers");
    }
}

#[test]
fn test_parse_file_and_line_selectors() {
    let f = parse_filter("fi=test.rs,ln=42").unwrap();
    if let ParsedFilter::Qualifiers(qs) = f {
        assert_eq!(qs.len(), 2);
        assert!(matches!(&qs[0], Qualifier::SelectorPattern(Selector::File, _)));
        assert!(matches!(&qs[1], Qualifier::SelectorPattern(Selector::Line, _)));
    } else {
        panic!("expected Qualifiers");
    }
}
