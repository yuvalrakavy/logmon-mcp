use super::*;
use crate::cases::document::{
    render, Anchor, CaseInput, CollectorLine, FilePointer, RegistryFact, SourceCounts, Window,
};
use crate::domain_data::ScopedFact;
use crate::collector::document::yaml_str;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn fixture(stem: &str) -> String {
    let p = format!(
        "{}/tests/fixtures/cases/{stem}.md",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {p}: {e}"))
}

// ---------------------------------------------------------------------------
// The escaping property — the risky half of this module
// ---------------------------------------------------------------------------

/// Every character `yaml_str` branches on, plus enough ordinary ones that a
/// rule which fires too eagerly is caught too.
///
/// **Exhaustive rather than random.** The input space that decides this
/// property is exactly the set of characters the escaper switches on, and it is
/// small enough to enumerate — so enumeration is a stronger statement than any
/// number of random samples, and it cannot flake.
fn interesting() -> Vec<char> {
    let mut v: Vec<char> = (0u32..0x80).filter_map(char::from_u32).collect();
    v.extend([
        '\u{7f}',   // DEL
        '\u{85}',   // NEL      -> \N
        '\u{a0}',   // NBSP     (not escaped — guards over-eagerness)
        '\u{2028}', // LS       -> \L
        '\u{2029}', // PS       -> \P
        '\u{e9}',   // é        multi-byte, unescaped
        '\u{65e5}', // 日
        '\u{1f600}', // 😀      outside the BMP
    ]);
    v
}

#[test]
fn every_single_character_survives_the_round_trip() {
    let mut checked = 0;
    for c in interesting() {
        let s = c.to_string();
        let quoted = yaml_str(&s);
        let back = Cur::new(&quoted, 0)
            .quoted()
            .unwrap_or_else(|e| panic!("{:?} (U+{:04X}) failed to parse: {e}", s, c as u32));
        assert_eq!(
            back, s,
            "U+{:04X} did not survive: emitted {quoted:?}",
            c as u32
        );
        checked += 1;
    }
    assert!(checked > 130, "the alphabet shrank: only {checked} chars");
}

#[test]
fn every_pair_of_interesting_characters_survives() {
    // Pairs catch a scanner that consumes one character too many or too few —
    // the failure a single-character test cannot see, because there is nothing
    // after the mistake to notice it.
    let alphabet = interesting();
    let mut checked = 0usize;
    for &a in &alphabet {
        for &b in &alphabet {
            let s: String = [a, b].iter().collect();
            let quoted = yaml_str(&s);
            let back = Cur::new(&quoted, 0).quoted().unwrap_or_else(|e| {
                panic!("U+{:04X}U+{:04X} failed to parse: {e}", a as u32, b as u32)
            });
            assert_eq!(
                back, s,
                "pair U+{:04X}U+{:04X} did not survive: emitted {quoted:?}",
                a as u32, b as u32
            );
            checked += 1;
        }
    }
    assert!(checked > 18_000, "the pair space shrank: {checked}");
}

#[test]
fn adversarial_strings_survive() {
    // Each of these is a way a naive un-escaper goes wrong.
    let cases = [
        r"\",                    // a lone backslash
        r"\\",                   // an escaped backslash
        r"\n",                   // the TEXT backslash-n, not a newline
        r"\x41",                 // text that looks like an escape
        "\\\u{2028}",            // backslash then a real LS
        "a\"b",                  // an embedded quote
        "}",                     // would close a flow map if unquoted
        ",",                     // would separate a flow item if unquoted
        "{a: b, c: [d]}",        // a whole flow map as literal text
        "\u{0}\u{1f}\u{7f}",     // C0 boundaries and DEL
        "---",                   // would look like a fence
        "key: value",            // would look like a line
        "\u{1f600}\u{1f600}",    // astral pair
    ];
    for s in cases {
        let quoted = yaml_str(s);
        let back = Cur::new(&quoted, 0)
            .quoted()
            .unwrap_or_else(|e| panic!("{s:?} emitted as {quoted:?} failed: {e}"));
        assert_eq!(back, s, "emitted {quoted:?}");
    }
}

#[test]
fn an_escape_the_emitter_never_produces_is_refused() {
    // **This test carries the weight the round-trip property cannot.** That
    // property quantifies over the emitter's IMAGE — every string `yaml_str`
    // can produce — so it says nothing about inputs the emitter never produces.
    // An over-permissive un-escaper passes it completely, and over-permissive
    // is exactly the failure hand-rolling was chosen to prevent: it is how a
    // corrupt document becomes a plausible record instead of an error.
    let refused = [
        r#""\q""#,     // an escape letter that does not exist
        r#""\A""#,     // adjacent to the real \N \L \P
        r#""\ ""#,     // backslash-space
        r#""\0""#,     // C0 is written \x00, never \0
        r#""\b""#,     // JSON has \b and \f; yaml_str writes \x08 / \x0c
        r#""\f""#,
        r#""\x""#,     // \x with no digits
        r#""\xZ1""#,   // \x with non-hex
        r#""\""#,      // the escape eats the closing quote
        r#""abc"#,     // never closed
    ];
    for bad in refused {
        let got = Cur::new(bad, 0).quoted();
        assert!(
            got.is_err(),
            "{bad} should have been refused, got {got:?} — an un-escaper that \
             accepts what the emitter cannot produce turns corruption into data"
        );
    }

    // ...and the mirror: everything the emitter DOES produce must be accepted,
    // so the strictness above cannot be achieved by refusing everything.
    for good in [r#""\\""#, r#""\"""#, r#""\n""#, r#""\r""#, r#""\t""#, r#""\x07""#,
                 r#""\N""#, r#""\L""#, r#""\P""#, r#""""#] {
        assert!(
            Cur::new(good, 0).quoted().is_ok(),
            "{good} is emitter output and must parse"
        );
    }
}

// ---------------------------------------------------------------------------
// Round trip through the REAL emitter — what pins parser to writer
// ---------------------------------------------------------------------------

fn hostile_input() -> CaseInput {
    CaseInput {
        stem: "checkout-hang-260731-141530".into(),
        captured_at: t("2026-07-31T14:15:30Z"),
        domain: "t3".into(),
        incarnation: Some("2".into()),
        // Everything a user can type, in the fields a user controls.
        reason: "he said \"boom\": a,b}c\nline2\ttab\u{2028}ls\u{7}bell".into(),
        anchor: Anchor {
            kind: "seq",
            label: "41022".into(),
            seq: 41022,
            at: t("2026-07-31T14:02:07Z"),
            message: "stalled: {awaiting, lock}".into(),
            of_many: None,
        },
        window: Window {
            from: 40672,
            to: 41372,
            verdict: EvidenceVerdict::Evicted,
            narrowed_by: Vec::new(),
            clamped: true,
            short_before: 3,
            short_after: 7,
            log_lost_below: 0,
            spans_evicted_before_window: None,
        },
        logdata: FilePointer {
            file: "checkout-hang-260731-141530.logdata.jsonl".into(),
            records: 700,
            by_source: Some(SourceCounts { filter: 700, pre_trigger: 0, post_trigger: 0 }),
        },
        spandata: FilePointer {
            file: "checkout-hang-260731-141530.spandata.jsonl".into(),
            records: 168,
            by_source: None,
        },
        registry: Vec::<RegistryFact>::new(),
        asserted: vec![ScopedFact {
            key: "@/capture/note".into(),
            value: "quoted \"thing\", with a comma".into(),
        }],
        neighbours: Vec::new(),
        spans: Vec::new(),
        collectors: Vec::<CollectorLine>::new(),
    }
}

#[test]
fn the_real_emitter_round_trips_through_this_parser() {
    let input = hostile_input();
    let doc = render(&input).body;
    let fm = parse_front_matter(&doc).expect("the emitter's own output must parse");

    assert_eq!(fm.case, input.stem);
    assert_eq!(fm.logmon_format, FORMAT_VERSION);
    assert_eq!(fm.captured_at, input.captured_at);
    assert_eq!(fm.domain, input.domain);
    assert_eq!(fm.incarnation, input.incarnation);
    assert_eq!(fm.reason, input.reason, "the hostile reason must survive");
    assert_eq!(fm.headline, input.anchor.message);
    assert_eq!(fm.anchor.kind, input.anchor.kind);
    assert_eq!(fm.anchor.value, input.anchor.label);
    assert_eq!(fm.anchor.seq, input.anchor.seq);
    assert_eq!(fm.anchor.at, input.anchor.at);
    assert_eq!(fm.verdict, input.window.verdict);
    assert_eq!(fm.seq_range.from, input.window.from);
    assert_eq!(fm.seq_range.to, input.window.to);
    assert_eq!(fm.seq_range.requested_before_missing, input.window.short_before);
    assert_eq!(fm.seq_range.requested_after_missing, input.window.short_after);
    assert!(fm.seq_range.clamped);
    let logdata = fm.logdata.as_ref().expect("declared, so present");
    assert_eq!(logdata.file, input.logdata.file);
    assert_eq!(logdata.records, input.logdata.records);
    let spandata = fm.spandata.as_ref().expect("declared, so present");
    assert_eq!(spandata.file, input.spandata.file);
    assert_eq!(spandata.records, input.spandata.records);
    assert_eq!(
        fm.asserted.get("@/capture/note").map(String::as_str),
        Some("quoted \"thing\", with a comma"),
        "an asserted value containing a quote AND a comma must not split the mapping"
    );
}

#[test]
fn every_verdict_round_trips() {
    // A verdict that fails to parse would be read as a corrupt case; a verdict
    // silently mapped to the wrong variant would mis-state what the evidence
    // can show, which is worse.
    for v in [
        EvidenceVerdict::Complete,
        EvidenceVerdict::Evicted,
        EvidenceVerdict::Filtered,
        EvidenceVerdict::CannotVerify,
    ] {
        let mut input = hostile_input();
        input.window.verdict = v;
        let doc = render(&input).body;
        let fm = parse_front_matter(&doc).unwrap();
        assert_eq!(fm.verdict, v, "verdict {v:?} did not survive");
    }
}

#[test]
fn a_null_incarnation_round_trips_as_none() {
    let mut input = hostile_input();
    input.incarnation = None;
    let doc = render(&input).body;
    assert_eq!(parse_front_matter(&doc).unwrap().incarnation, None);
}

// ---------------------------------------------------------------------------
// The real fixtures — bytes a real broker really emitted
// ---------------------------------------------------------------------------

#[test]
fn the_happy_path_fixture_parses() {
    let fm = parse_front_matter(&fixture("with-spans-260803-214501")).unwrap();
    assert_eq!(fm.case, "with-spans-260803-214501");
    assert_eq!(fm.logmon_format, 1);
    assert_eq!(fm.domain, "default");
    assert_eq!(fm.incarnation.as_deref(), Some("1"));
    assert_eq!(fm.verdict, EvidenceVerdict::Complete);
    let logdata = fm.logdata.as_ref().expect("declared, so present");
    let spandata = fm.spandata.as_ref().expect("declared, so present");
    assert_eq!(logdata.records, 9);
    assert_eq!(spandata.records, 3);
    assert_eq!(logdata.file, "with-spans-260803-214501.logdata.jsonl");
    assert_eq!(spandata.file, "with-spans-260803-214501.spandata.jsonl");
    assert!(
        fm.seq_range.from <= fm.anchor.seq && fm.anchor.seq <= fm.seq_range.to,
        "the anchor must lie inside its own window: {:?} vs anchor {}",
        fm.seq_range,
        fm.anchor.seq
    );
}

#[test]
fn the_zero_span_fixture_parses_and_still_says_complete() {
    // This is the writer defect preserved: a `complete` verdict on a capture
    // that missed every span of the trace it is about. The loader must read it
    // faithfully — it is not the reader's job to second-guess the document.
    let fm = parse_front_matter(&fixture("checkout-hang-260803-214121")).unwrap();
    assert_eq!(
        fm.spandata.as_ref().expect("declared").records,
        0,
        "declared with zero records is `we captured none` — NOT omitted, which \
         is the absent key"
    );
    assert_eq!(fm.verdict, EvidenceVerdict::Complete);
    assert_eq!(fm.logdata.as_ref().expect("declared").records, 6);
}

#[test]
fn the_hostile_reason_fixture_parses() {
    let fm = parse_front_matter(&fixture("nasty-260803-214626")).unwrap();
    assert!(
        fm.reason.contains('"') && fm.reason.contains('\n') && fm.reason.contains('\t'),
        "the escaped characters did not come back: {:?}",
        fm.reason
    );
    assert!(
        fm.reason.contains('\u{7}'),
        "the BEL did not come back: {:?}",
        fm.reason
    );
    assert!(
        fm.reason.contains('\u{2028}'),
        "the line separator did not come back: {:?}",
        fm.reason
    );
}

// ---------------------------------------------------------------------------
// Version policy
// ---------------------------------------------------------------------------

#[test]
fn a_newer_format_is_refused_by_name() {
    let doc = fixture("with-spans-260803-214501").replace("logmon_format: 1", "logmon_format: 2");
    match parse_front_matter(&doc) {
        Err(ParseError::FormatTooNew { found, known }) => {
            assert_eq!((found, known), (2, FORMAT_VERSION));
        }
        other => panic!("expected FormatTooNew, got {other:?}"),
    }
}

#[test]
fn an_older_format_is_accepted() {
    // Refusing older would orphan every archived case on the first bump, which
    // breaks the "readable years later" promise FORMAT_VERSION exists to make.
    // FORMAT_VERSION is 1, so 0 is the only older value expressible.
    let doc = fixture("with-spans-260803-214501").replace("logmon_format: 1", "logmon_format: 0");
    let fm = parse_front_matter(&doc).expect("an older format must still load");
    assert_eq!(fm.logmon_format, 0);
}

#[test]
fn the_version_is_checked_before_anything_is_interpreted() {
    // Order matters: a newer document may use a field in a way this parser
    // would misread, so the refusal must not depend on parsing succeeding.
    let doc = fixture("with-spans-260803-214501")
        .replace("logmon_format: 1", "logmon_format: 9")
        .replace("verdict: complete", "verdict: something_from_the_future");
    assert!(
        matches!(
            parse_front_matter(&doc),
            Err(ParseError::FormatTooNew { .. })
        ),
        "the version check must fire before the verdict is interpreted"
    );
}

// ---------------------------------------------------------------------------
// Negative controls — each names a specific way to be wrong
// ---------------------------------------------------------------------------

#[test]
fn a_missing_required_key_is_named() {
    let doc = fixture("with-spans-260803-214501")
        .lines()
        .filter(|l| !l.starts_with("captured_at:"))
        .collect::<Vec<_>>()
        .join("\n");
    match parse_front_matter(&doc) {
        Err(ParseError::MissingKey(k)) => assert_eq!(k, "captured_at"),
        other => panic!("expected MissingKey(captured_at), got {other:?}"),
    }
}

#[test]
fn an_unknown_key_is_ignored() {
    // Additive forward compatibility: a field added by a later logmon must not
    // stop this one reading the file.
    let doc = fixture("with-spans-260803-214501")
        .replacen("case: ", "future_field: {a: 1}\ncase: ", 1);
    assert!(
        parse_front_matter(&doc).is_ok(),
        "an unknown key must not break the parse"
    );
}

#[test]
fn a_document_with_no_front_matter_says_so() {
    assert_eq!(
        parse_front_matter("# just a heading\n"),
        Err(ParseError::NoFrontMatter)
    );
    assert_eq!(
        parse_front_matter("---\ncase: x\n"),
        Err(ParseError::NoFrontMatter),
        "an unclosed fence is not front matter"
    );
}

#[test]
fn a_bad_verdict_is_refused_rather_than_defaulted() {
    let doc =
        fixture("with-spans-260803-214501").replace("verdict: complete", "verdict: probably_fine");
    match parse_front_matter(&doc) {
        Err(ParseError::BadVerdict(v)) => assert_eq!(v, "probably_fine"),
        other => panic!("expected BadVerdict, got {other:?}"),
    }
}

#[test]
fn a_non_numeric_seq_is_refused() {
    let doc = fixture("with-spans-260803-214501").replace("from: 1001", "from: soon");
    assert!(
        matches!(
            parse_front_matter(&doc),
            Err(ParseError::WrongType { key: "seq_range", .. })
        ),
        "a non-numeric seq must not become 0"
    );
}

#[test]
fn a_brace_inside_a_quoted_value_does_not_close_the_mapping() {
    // This is the entire reason this is a scanner and not `split(',')`. If it
    // regresses, `logdata.records` silently reads as the wrong number.
    let doc = format!(
        "---\n\
         case: x\n\
         logmon_format: 1\n\
         captured_at: \"2026-08-03T21:45:01Z\"\n\
         domain: \"d\"\n\
         incarnation: null\n\
         reason: \"r\"\n\
         anchor: {{kind: seq, value: \"1\", seq: 1, at: \"2026-08-03T21:45:01Z\"}}\n\
         headline: \"h\"\n\
         verdict: complete\n\
         seq_range: {{from: 1, to: 2, requested_before_missing: 0, \
         requested_after_missing: 0, clamped: false}}\n\
         logdata: {{file: {}, records: 9}}\n\
         spandata: {{file: \"s.jsonl\", records: 3}}\n\
         provenance: {{core: \"3 of 3\", missing: []}}\n\
         asserted: {{}}\n\
         ---\n",
        yaml_str("weird}, records: 999, x.jsonl")
    );
    let fm = parse_front_matter(&doc).unwrap();
    let logdata = fm.logdata.as_ref().expect("declared");
    assert_eq!(logdata.file, "weird}, records: 999, x.jsonl");
    assert_eq!(
        logdata.records, 9,
        "the brace inside the string must not have ended the mapping"
    );
}

#[test]
fn the_source_split_round_trips() {
    // The verdict cannot see the unfiltered flight-recorder window, so these
    // counts are the only place that fact survives into a loaded case.
    let mut input = hostile_input();
    input.logdata.by_source = Some(SourceCounts {
        filter: 680,
        pre_trigger: 15,
        post_trigger: 5,
    });
    let doc = render(&input).body;
    let fm = parse_front_matter(&doc).unwrap();

    let c = fm
        .logdata
        .as_ref()
        .expect("declared")
        .by_source
        .expect("emitted for logdata");
    assert_eq!((c.filter, c.pre_trigger, c.post_trigger), (680, 15, 5));

    assert!(
        fm.spandata.as_ref().expect("declared").by_source.is_none(),
        "spans have no filter/flight-recorder split — session filters never \
         narrow them"
    );
}

#[test]
fn an_absent_source_split_is_unknown_not_zero() {
    // A case written before the key existed knows nothing about the split.
    // Defaulting it to zeros would assert that every record matched a filter.
    let fm = parse_front_matter(&fixture("with-spans-260803-214501")).unwrap();
    assert!(
        fm.logdata.as_ref().expect("declared").by_source.is_none(),
        "the fixture predates the key, so the split must read as unknown"
    );
}

#[test]
fn the_narrowing_filters_round_trip_with_their_expressions() {
    // Without these a `filtered` case can only load as `cannot_verify` — and a
    // production capture with a session filter running is exactly that case.
    let mut input = hostile_input();
    input.window.narrowed_by = vec![
        logmon_broker_protocol::NarrowedRange {
            from_seq: 40672,
            to_seq: 40999,
            // A filter DSL string carrying the characters that would end a flow
            // mapping if the scanner were a split on commas.
            filters: vec![r#"msg:"a,b}c""#.into(), "l>=ERROR".into()],
        },
        logmon_broker_protocol::NarrowedRange {
            from_seq: 41000,
            to_seq: 41372,
            filters: vec!["service:auth".into()],
        },
    ];
    let doc = render(&input).body;
    let fm = parse_front_matter(&doc).unwrap();

    let ranges = fm.narrowed_by.as_ref().expect("emitted");
    assert_eq!(ranges.len(), 2, "both stretches: {ranges:?}");
    assert_eq!((ranges[0].from, ranges[0].to), (40672, 40999));
    assert_eq!(
        ranges[0].filters,
        vec![r#"msg:"a,b}c""#, "l>=ERROR"],
        "the expressions must survive verbatim, braces and commas included"
    );
    assert_eq!(ranges[1].filters, vec!["service:auth"]);
}

#[test]
fn no_narrowing_and_unknown_narrowing_are_different_answers() {
    // Empty is a positive fact: nothing narrowed. Absent means the case predates
    // the key, and a loader must fall back to verdict-based seeding rather than
    // concluding the window was unfiltered — which would vouch for evidence it
    // cannot speak for.
    let input = hostile_input(); // narrowed_by is empty
    let doc = render(&input).body;
    assert_eq!(
        parse_front_matter(&doc).unwrap().narrowed_by,
        Some(Vec::new()),
        "the emitter states `nothing narrowed` rather than staying silent"
    );

    let old = fixture("with-spans-260803-214501");
    assert!(
        !old.contains("narrowed_by:"),
        "the fixture predates the key (guards the next assertion)"
    );
    assert_eq!(
        parse_front_matter(&old).unwrap().narrowed_by,
        None,
        "absent must stay unknown, never collapse to `nothing narrowed`"
    );
}

#[test]
fn an_absent_evidence_key_is_omitted_not_empty() {
    // The distinction the whole omit_* option rests on. `records: 0` says the
    // capture looked and found none — a finding about the system. An absent key
    // says nobody looked — a fact about the capture. A reader that collapsed
    // them would answer "what was the slowest span?" with a silence that reads
    // like evidence.
    let doc = fixture("with-spans-260803-214501");

    let present = parse_front_matter(&doc).unwrap();
    assert!(present.spandata.is_some(), "declared in the fixture");

    let omitted_doc = doc
        .lines()
        .filter(|l| !l.starts_with("spandata:"))
        .collect::<Vec<_>>()
        .join("\n");
    let omitted = parse_front_matter(&omitted_doc)
        .expect("an omitted-evidence case is a VALID case, not a malformed one");
    assert!(
        omitted.spandata.is_none(),
        "an absent key must read as omitted rather than erroring"
    );

    // ...and the two must not be confusable in either direction.
    let zero_doc = doc.replace(
        r#"spandata: {file: "with-spans-260803-214501.spandata.jsonl", records: 3}"#,
        r#"spandata: {file: "with-spans-260803-214501.spandata.jsonl", records: 0}"#,
    );
    let zero = parse_front_matter(&zero_doc).unwrap();
    assert_eq!(
        zero.spandata.map(|s| s.records),
        Some(0),
        "captured-none stays Some(0) and never becomes None"
    );
}

#[test]
fn an_absent_key_is_still_told_apart_from_a_malformed_one() {
    // "Optional" must not slide into "ignore anything I cannot read". A key that
    // is present but the wrong shape is corruption, and corruption is not
    // omission.
    let doc = fixture("with-spans-260803-214501").replace(
        r#"spandata: {file: "with-spans-260803-214501.spandata.jsonl", records: 3}"#,
        "spandata: not-a-mapping",
    );
    assert!(
        matches!(
            parse_front_matter(&doc),
            Err(ParseError::WrongType { key: "spandata", .. })
        ),
        "a malformed key must error, not read as omitted"
    );
}

// ---------------------------------------------------------------------------
// The machine-readable registry block
// ---------------------------------------------------------------------------

fn fact(path: &str, value: &str, created: &str, validated: &str, ttl: Option<u64>) -> RegistryFact {
    RegistryFact {
        path: path.into(),
        value: value.into(),
        created_at: t(created),
        validated_at: t(validated),
        ttl_secs: ttl,
        expired: ttl.map(|_| false),
    }
}

#[test]
fn the_registry_block_round_trips_exactly() {
    let mut input = hostile_input();
    input.registry = vec![
        // A value the rendered table cannot give back: `|` would be escaped for
        // the table, and a newline is flattened to a space and trimmed.
        fact(
            "/Build/commit",
            "9f3a11c | dirty\nsecond line  ",
            "2026-07-31T08:12:04Z",
            "2026-07-31T14:03:11Z",
            None,
        ),
        // Timestamps the table floors: 13 days reads as `1w`, and restoring
        // captured_at - 1w would flip this 7-day TTL from within to elapsed.
        fact(
            "/Env/host",
            "prod-web-01",
            "2026-07-18T09:00:00Z",
            "2026-07-18T09:00:00Z",
            Some(7 * 24 * 3600),
        ),
    ];
    let doc = render(&input).body;

    let block = parse_registry(&doc)
        .expect("the block must parse")
        .expect("the emitter must have written one");

    assert_eq!(block.dropped, 0);
    assert_eq!(block.facts.len(), 2);

    let commit = block
        .facts
        .iter()
        .find(|f| f.path == "/Build/commit")
        .expect("the fact is present");
    assert_eq!(
        commit.value, "9f3a11c | dirty\nsecond line  ",
        "the pipe, the newline and the trailing spaces must all survive — the \
         rendered table gives back none of them"
    );

    let host = block
        .facts
        .iter()
        .find(|f| f.path == "/Env/host")
        .expect("the fact is present");
    assert_eq!(
        host.validated_at,
        t("2026-07-18T09:00:00Z"),
        "the timestamp must be exact, not floored to the largest whole unit"
    );
    assert_eq!(host.ttl_secs, Some(7 * 24 * 3600));
    assert_eq!(host.expired, Some(false));
}

#[test]
fn the_rendered_table_really_is_lossy_where_the_block_is_not() {
    // The negative control for the decision to add the block at all. If this
    // ever goes green the block is redundant and should be removed.
    let mut input = hostile_input();
    input.registry = vec![fact(
        "/Build/commit",
        "9f3a11c | dirty\nsecond line  ",
        "2026-07-31T08:12:04Z",
        "2026-07-31T14:03:11Z",
        None,
    )];
    let doc = render(&input).body;
    let table_row = doc
        .lines()
        .find(|l| l.starts_with("| `/Build/commit`"))
        .expect("the human table still renders the fact");

    assert!(
        !table_row.contains("second line  "),
        "the table is expected to have flattened and trimmed this: {table_row}"
    );
    assert!(
        table_row.contains(r"\|"),
        "and to have escaped the pipe for the table: {table_row}"
    );
}

#[test]
fn a_case_written_before_the_block_existed_loads_with_no_registry() {
    // The three fixtures predate this format addition, which makes them the
    // exact input the absent-block path exists for. `Ok(None)` is the answer:
    // not an error, and NOT a reconstruction from the rendered table.
    for stem in [
        "with-spans-260803-214501",
        "checkout-hang-260803-214121",
        "nasty-260803-214626",
    ] {
        let doc = fixture(stem);
        assert!(
            parse_front_matter(&doc).is_ok(),
            "{stem} must still load as a case"
        );
        assert!(
            parse_registry(&doc).unwrap().is_none(),
            "{stem} has no machine block, and that must read as absent rather \
             than as an empty registry"
        );
    }
}

#[test]
fn an_empty_registry_writes_no_block() {
    // §5 returns early when there is nothing to say, so there is no block to
    // find — which is the same `Ok(None)` as a pre-format case. That collapse is
    // deliberate: both mean "no provenance to restore".
    let mut input = hostile_input();
    input.registry = Vec::new();
    let doc = render(&input).body;
    assert!(parse_registry(&doc).unwrap().is_none());
}

#[test]
fn a_newer_block_format_is_refused() {
    let mut input = hostile_input();
    input.registry = vec![fact(
        "/Action",
        "run",
        "2026-07-31T08:12:04Z",
        "2026-07-31T14:03:11Z",
        None,
    )];
    let doc = render(&input).body.replace(
        r#"{"logmon_format":1,"facts""#,
        r#"{"logmon_format":2,"facts""#,
    );
    assert!(
        matches!(
            parse_registry(&doc),
            Err(ParseError::FormatTooNew { found: 2, .. })
        ),
        "the block carries its own version and must check it"
    );
}

#[test]
fn a_corrupt_block_errors_rather_than_reading_as_absent() {
    // Absent and malformed are different facts. Collapsing them would let a
    // truncated document restore an empty registry and call it complete.
    let mut input = hostile_input();
    input.registry = vec![fact(
        "/Action",
        "run",
        "2026-07-31T08:12:04Z",
        "2026-07-31T14:03:11Z",
        None,
    )];
    let doc = render(&input)
        .body
        .replace(r#""facts":["#, r#""facts":{"#);
    assert!(
        parse_registry(&doc).is_err(),
        "a malformed block must not read as `no registry`"
    );
}

#[test]
fn provenance_missing_keys_are_read_as_a_list() {
    let doc = fixture("with-spans-260803-214501").replace(
        "provenance: {core: \"3 of 3\", missing: []}",
        "provenance: {core: \"1 of 3\", missing: [\"/Build/commit\", \"/Action\"]}",
    );
    let fm = parse_front_matter(&doc).unwrap();
    assert_eq!(fm.provenance.core, "1 of 3");
    assert_eq!(fm.provenance.missing, vec!["/Build/commit", "/Action"]);
}
