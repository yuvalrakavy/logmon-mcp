use super::*;
use crate::gelf::message::Level;
use crate::span::types::{SpanKind, SpanStatus};

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn entry(seq: u64, level: Level, message: &str) -> LogEntry {
    let mut e = LogEntry::synthetic(level, message);
    e.seq = seq;
    e.timestamp = t("2026-07-31T14:02:07Z");
    e
}

fn fact(path: &str, value: &str, validated: &str) -> RegistryFact {
    RegistryFact {
        path: path.into(),
        value: value.into(),
        created_at: t("2026-07-31T08:12:04Z"),
        validated_at: t(validated),
        ttl_secs: None,
        expired: None,
    }
}

/// A complete, unremarkable capture. Every other test here varies one thing
/// against this.
fn base() -> CaseInput {
    CaseInput {
        stem: "checkout-hang-260731-021530".into(),
        captured_at: t("2026-07-31T14:15:30Z"),
        domain: "t3".into(),
        incarnation: Some("2".into()),
        reason: "hang at 20/20".into(),
        anchor: Anchor {
            kind: "seq",
            label: "41022".into(),
            seq: 41022,
            at: t("2026-07-31T14:02:07Z"),
            message: "checkout worker stalled awaiting lock".into(),
            of_many: None,
        },
        window: Window {
            from: 40672,
            to: 41372,
            verdict: EvidenceVerdict::Complete,
            narrowed_by: Vec::new(),
            capped: false,
            evicted_before_window: None,
            spans_evicted_before_window: None,
        },
        logdata: FilePointer {
            file: "checkout-hang-260731-021530.logdata.jsonl".into(),
            records: 700,
        },
        spandata: FilePointer {
            file: "checkout-hang-260731-021530.spandata.jsonl".into(),
            records: 168,
        },
        registry: CORE_KEYS
            .iter()
            .map(|k| fact(k, "v", "2026-07-31T14:03:11Z"))
            .collect(),
        asserted: Vec::new(),
        neighbours: vec![
            entry(41021, Level::Info, "before"),
            entry(41022, Level::Error, "checkout worker stalled awaiting lock"),
            entry(41023, Level::Info, "after"),
        ],
        spans: Vec::new(),
        collectors: vec![CollectorLine {
            name: "checkout".into(),
            owner: "cli".into(),
            matched: 42,
            snapshots: 1,
            latest_snapshot: Some(("before".into(), t("2026-07-31T10:15:30Z"))),
            zeroed_by: None,
        }],
    }
}

/// The whole point of front-matter is that it is small and fixed-schema, so a
/// tool walking the archive can read it without parsing the body.
#[test]
fn front_matter_is_fixed_schema_and_carries_the_index_fields() {
    let out = render(&base()).body;
    let fm = out.split("---").nth(1).expect("front matter is delimited");

    for key in [
        "case:",
        "logmon_format:",
        "captured_at:",
        "domain:",
        "incarnation:",
        "reason:",
        "anchor:",
        "headline:",
        "verdict:",
        "seq_range:",
        "logdata:",
        "spandata:",
        "provenance:",
        "asserted:",
    ] {
        assert!(fm.contains(key), "front matter is missing `{key}`:\n{fm}");
    }
    assert!(fm.contains("logmon_format: 1"), "{fm}");
    assert!(fm.contains("verdict: complete"), "{fm}");
    assert!(
        fm.contains("seq_range: {from: 40672, to: 41372, capped: false}"),
        "{fm}"
    );
    // Small: the index surface must not become the document.
    assert!(fm.len() < 1024, "front matter is {} bytes:\n{fm}", fm.len());
}

/// §5.2's ordering, and it is load-bearing rather than cosmetic: a caveat
/// reached after 400 lines has already failed, and a reader who acts on the
/// suggestions before reading the verdict has been misled by layout.
#[test]
fn evidence_precedes_what_to_do_next_which_precedes_the_bulk() {
    let out = render(&base()).body;
    let at = |h: &str| {
        out.find(h)
            .unwrap_or_else(|| panic!("missing `{h}`:\n{out}"))
    };
    assert!(at("# checkout worker stalled") < at("## 2. Evidence"));
    assert!(at("## 2. Evidence") < at("## 3. What to do next"));
    assert!(at("## 3. What to do next") < at("## 4. Anchor entry"));
    assert!(at("## 4. Anchor entry") < at("## 5. Provenance"));
    assert!(at("## 5. Provenance") < at("## 6. Collector state"));
    assert!(at("## 6. Collector state") < at("## 7. Evidence files"));
}

/// The headline must identify THIS incident — a headline that cannot tell two
/// documents apart is not an index.
#[test]
fn the_headline_is_the_anchor_entrys_own_message() {
    let out = render(&base()).body;
    assert!(
        out.starts_with("---") && out.contains("\n# checkout worker stalled awaiting lock\n"),
        "{out}"
    );
    // And both instants are present, because a case written after the fact is
    // about the incident rather than about now.
    assert!(out.contains("2026-07-31T14:02:07+00:00"), "{out}");
    assert!(out.contains("13m after the anchor"), "{out}");
}

#[test]
fn a_filtered_window_names_the_filter_and_the_seqs_and_says_what_it_costs() {
    let mut i = base();
    i.window.verdict = EvidenceVerdict::Filtered;
    i.window.narrowed_by = vec![NarrowedRange {
        from_seq: 40672,
        to_seq: 41100,
        filters: vec!["service:checkout".into()],
    }];
    let r = render(&i);

    assert!(r.body.contains("**`filtered`**"), "{}", r.body);
    assert!(r.body.contains("`service:checkout`"), "{}", r.body);
    assert!(r.body.contains("40672–41100"), "{}", r.body);
    assert!(
        r.body.contains("429 of this window's 701 seqs"),
        "the cost is counted, not gestured at:\n{}",
        r.body
    );
    assert!(
        r.body
            .contains("is not a supported conclusion over that range"),
        "{}",
        r.body
    );
    // And the remedy names the filter too, in the section after the verdict.
    let next = r.body.split("## 3. What to do next").nth(1).unwrap();
    assert!(next.contains("service:checkout"), "{next}");
    // A caller can act on it without parsing markdown.
    assert!(
        r.notes.iter().any(|n| n.kind == NOTE_CAPTURE_GAP),
        "{:?}",
        r.notes
    );
}

/// Absence must not read as validation — the `matched: 0` lesson applied to
/// provenance.
#[test]
fn an_empty_registry_is_stated_as_loudly_as_a_stale_one() {
    let mut i = base();
    i.registry = Vec::new();
    let r = render(&i);
    assert!(r.body.contains("0 of 3 core keys"), "{}", r.body);
    assert!(r.body.contains("empty"), "{}", r.body);
    assert!(
        r.notes
            .iter()
            .any(|n| n.kind == NOTE_PROVENANCE && n.detail.contains("/Build/commit")),
        "{:?}",
        r.notes
    );
    // Full coverage still prints the line, rather than the section going quiet
    // — silence is indistinguishable from a document that never checked.
    let full = render(&base()).body;
    assert!(full.contains("3 of 3 core keys"), "{full}");
}

/// Coverage names the MISSING keys: "missing `/Build/profile`" is actionable
/// where "2 of 3" is not.
#[test]
fn coverage_names_the_missing_key_rather_than_only_counting() {
    let mut i = base();
    i.registry.retain(|f| f.path != "/Build/profile");
    let out = render(&i).body;
    assert!(out.contains("2 of 3"), "{out}");
    assert!(out.contains("/Build/profile"), "{out}");
    assert!(
        out.contains("provenance: {core: \"2 of 3\", missing: [\"/Build/profile\"]}"),
        "and in front matter, where a tool can read it:\n{out}"
    );
}

/// Staleness is reported as **age**, never as a verdict — `/Action` set three
/// minutes ago is already wrong if the action changed two minutes ago. A key
/// that states its own lifetime is the one exception, and even there the
/// document reports whether the caller's own stated lifetime elapsed.
#[test]
fn every_key_carries_an_age_and_no_freshness_verdict() {
    let mut i = base();
    i.registry = vec![
        fact("/Build/commit", "9f2a1c4", "2026-07-31T08:15:30Z"),
        RegistryFact {
            ttl_secs: Some(1800),
            expired: Some(false),
            ..fact("/Action", "checkout smoke", "2026-07-31T13:58:00Z")
        },
        RegistryFact {
            ttl_secs: Some(60),
            expired: Some(true),
            ..fact("/Env/host", "ci-7", "2026-07-31T09:00:00Z")
        },
    ];
    let out = render(&i).body;
    assert!(out.contains("| `/Build/commit` |"), "{out}");
    assert!(out.contains("| 6h |"), "an age, in the table:\n{out}");
    assert!(out.contains("30m — within"), "{out}");
    assert!(out.contains("1m — **elapsed**"), "{out}");
    for forbidden in ["current", "fresh", "up to date", "trustworthy"] {
        assert!(
            !out.to_lowercase().contains(forbidden),
            "the document must not render a freshness judgement (`{forbidden}`):\n{out}"
        );
    }
}

/// §3.1 permits 1.1 MB of registry and the document renders it, so the bulk
/// MOVES with a pointer rather than being cut in silence.
#[test]
fn an_oversized_registry_is_capped_with_a_count_and_a_pointer() {
    let mut i = base();
    i.registry = (0..400)
        .map(|n| {
            let mut f = fact(
                &format!("/Big/key{n:04}"),
                &"x".repeat(400),
                "2026-07-31T14:03:11Z",
            );
            // Newest-validated first is the render order, so make the order
            // observable.
            f.validated_at = t("2026-07-31T14:03:11Z") - chrono::Duration::seconds(n);
            f
        })
        .collect();
    let r = render(&i);

    let table = r.body.split("## 5. Provenance").nth(1).unwrap();
    assert!(
        table.len() < REGISTRY_RENDER_CAP + 4096,
        "the rendered registry ran to {} bytes",
        table.len()
    );
    assert!(
        table.contains("were not rendered"),
        "a cut must be stated:\n{table}"
    );
    assert!(
        table.contains("get_domain_data"),
        "and it must say where the rest is:\n{table}"
    );
    assert!(
        r.notes.iter().any(|n| n.kind == NOTE_TRUNCATED),
        "{:?}",
        r.notes
    );
    // Newest-validated survives the cut; oldest is what falls off.
    assert!(table.contains("/Big/key0000"), "{}", &table[..2000]);
    assert!(!table.contains("/Big/key0399"), "the oldest should be cut");
}

/// The sigil stays in the key, so a reader grepping for `/Data/seed` and one
/// grepping for `@/Data/seed` are asking two different questions.
#[test]
fn an_asserted_fact_keeps_its_sigil_and_is_dated_against_the_anchor() {
    let mut i = base();
    i.asserted = vec![ScopedFact {
        key: "@/Data/seed".into(),
        value: "8814".into(),
    }];
    let out = render(&i).body;
    assert!(
        out.contains("asserted: {\"@/Data/seed\": \"8814\"}"),
        "{out}"
    );
    assert!(out.contains("- `@/Data/seed`: 8814"), "{out}");
    assert!(
        out.contains("not validated — recorded at capture, 13m after the anchor"),
        "a fact recorded 13m after the incident must not read as evidence about it:\n{out}"
    );
    // And when there are none, the document says so rather than going quiet.
    assert!(
        render(&base())
            .body
            .contains("Nothing was asserted for this case alone"),
        "{}",
        render(&base()).body
    );
}

/// The selection is by DOMAIN across owners, so the section says whose each
/// collector is — and when there are none it says that in those words, because
/// omission reads as "nothing interesting".
#[test]
fn collector_state_names_the_owner_and_says_so_when_there_are_none() {
    let out = render(&base()).body;
    assert!(out.contains("| `checkout` | `cli` |"), "{out}");
    assert!(out.contains("whoever armed it"), "{out}");

    let mut i = base();
    i.collectors = Vec::new();
    let r = render(&i);
    let section = r.body.split("## 6. Collector state").nth(1).unwrap();
    assert!(
        section.contains("No collectors were armed on domain `t3`")
            && section.contains("or any other"),
        "a session-scoped wording would state something false about the domain:\n{section}"
    );
    // And it becomes a suggestion, in the section after the verdict.
    assert!(
        r.body.contains("No collector was armed on `t3`"),
        "{}",
        r.body
    );
}

/// The span line is separate from the verdict, because the two stores share a
/// seq axis and evict independently — one verdict cannot honestly cover both.
#[test]
fn the_span_line_reports_the_span_rings_own_retention() {
    let out = render(&base()).body;
    assert!(
        out.contains("Spans: 168 captured; the span ring had not evicted below seq 40672"),
        "{out}"
    );

    let mut i = base();
    i.window.spans_evicted_before_window = Some(12);
    let r = render(&i);
    assert!(r.body.contains("up to 12"), "{}", r.body);
    assert!(
        r.body.contains("The log verdict above does not cover this"),
        "{}",
        r.body
    );
    assert_eq!(
        r.notes
            .iter()
            .filter(|n| n.kind == NOTE_CAPTURE_GAP)
            .count(),
        1,
        "{:?}",
        r.notes
    );
}

#[test]
fn the_anchor_is_marked_among_its_neighbours() {
    let out = render(&base()).body;
    assert!(
        out.contains("> {:>8}".replace("{:>8}", "   41022").as_str()),
        "{out}"
    );
    assert!(out.contains("    41021"), "{out}");
}

/// A `trace_id` matching many entries anchors on the earliest by seq **and says
/// so** — a wrong anchor is a wrong document, not a wrong parameter.
#[test]
fn a_many_matched_anchor_says_which_one_it_took() {
    let mut i = base();
    i.anchor.kind = "trace_id";
    i.anchor.label = "7f3a".into();
    i.anchor.of_many = Some(14);
    let out = render(&i).body;
    assert!(out.contains("matched 14 entries"), "{out}");
    assert!(out.contains("earliest by seq (41022)"), "{out}");
    assert!(out.contains("anchor: {kind: trace_id"), "{out}");
}

/// A capped read is stated in the document, not left to be inferred from the
/// record count.
#[test]
fn a_capped_read_is_stated_and_becomes_a_suggestion() {
    let mut i = base();
    i.window.capped = true;
    let out = render(&i).body;
    assert!(out.contains("**capped**"), "{out}");
    assert!(
        out.contains("seq_range: {from: 40672, to: 41372, capped: true}"),
        "{out}"
    );
    assert!(out.contains("larger `before`/`after`"), "{out}");
}

/// `partial` was cut in §9.4. A removed verdict that a renderer still emits is
/// the failure mode.
#[test]
fn the_renderer_emits_only_the_four_verdicts() {
    for v in [
        EvidenceVerdict::Complete,
        EvidenceVerdict::Evicted,
        EvidenceVerdict::Filtered,
        EvidenceVerdict::CannotVerify,
    ] {
        let mut i = base();
        i.window.verdict = v;
        let out = render(&i).body;
        let line = out
            .lines()
            .find(|l| l.starts_with("verdict: "))
            .expect("front matter carries a verdict");
        assert!(
            ["complete", "evicted", "filtered", "cannot_verify"]
                .contains(&line.trim_start_matches("verdict: ")),
            "{line}"
        );
        assert!(!out.contains("partial"), "{out}");
    }
}

/// A value with a newline and `---` must not end the front matter, and must not
/// break a table row in the body.
#[test]
fn a_hostile_value_survives_both_rendering_contexts() {
    let mut i = base();
    i.reason = "boom\n---\nnot: front matter".into();
    i.registry = vec![fact(
        "/Note",
        "a | b\n## Evidence\n\nverdict: complete",
        "2026-07-31T14:03:11Z",
    )];
    let out = render(&i).body;

    // Exactly two `---` lines: the front matter's own delimiters.
    assert_eq!(
        out.lines().filter(|l| l.trim() == "---").count(),
        2,
        "a value ended the front matter:\n{out}"
    );
    assert_eq!(
        out.matches("## 2. Evidence").count(),
        1,
        "a value produced a second Evidence section:\n{out}"
    );
    // The pipe is escaped and the newline flattened, so the row stays a row.
    let row = out
        .lines()
        .find(|l| l.starts_with("| `/Note`"))
        .expect("the key is rendered as a table row");
    assert!(row.contains("a \\| b"), "{row}");
    assert_eq!(
        row.matches('|').count() - 1,
        6,
        "the row kept its shape: {row}"
    );
}

/// The three Unicode line breaks YAML recognises beyond `\n` and `\r`. They are
/// not C0, so a hex-escape arm keyed on C0 misses them, and a raw one inside a
/// double-quoted scalar ends the scalar.
#[test]
fn the_unicode_line_breaks_are_escaped_in_front_matter() {
    let mut i = base();
    i.reason = "a\u{85}b\u{2028}c\u{2029}d".into();
    let out = render(&i).body;
    let line = out
        .lines()
        .find(|l| l.starts_with("reason: "))
        .expect("front matter carries a reason");
    assert_eq!(line, r#"reason: "a\Nb\Lc\Pd""#);
    for raw in ['\u{85}', '\u{2028}', '\u{2029}'] {
        assert!(
            !out.contains(raw),
            "a raw {raw:?} survived into the document"
        );
    }
}

#[test]
fn the_slowest_spans_are_summarised_and_the_bulk_is_pointed_at() {
    let mut i = base();
    i.spans = (1..=8)
        .map(|n| SpanEntry {
            seq: n,
            trace_id: 7,
            span_id: n,
            parent_span_id: None,
            start_time: t("2026-07-31T14:02:07Z"),
            end_time: t("2026-07-31T14:02:08Z"),
            duration_ms: n as f64 * 10.0,
            name: format!("op{n}"),
            kind: SpanKind::Internal,
            service_name: "svc".into(),
            status: SpanStatus::Ok,
            attributes: Default::default(),
            events: Vec::new(),
        })
        .collect();
    let out = render(&i).body;
    assert!(out.contains("| `op8` | `svc` | 80.0 |"), "{out}");
    assert!(out.contains("| `op4` | `svc` | 40.0 |"), "{out}");
    assert!(
        !out.contains("| `op3` |"),
        "only the slowest few belong in the document:\n{out}"
    );
    assert!(
        out.contains("full trees, attributes and events are in the"),
        "{out}"
    );
}

#[test]
#[ignore = "sample generator: cargo test -- --ignored --nocapture sample_document"]
fn sample_document() {
    let mut i = base();
    i.reason = "checkout hangs at iteration 20 of 20, reproducibly".into();
    i.window.verdict = EvidenceVerdict::Filtered;
    i.window.narrowed_by = vec![NarrowedRange {
        from_seq: 40672,
        to_seq: 41100,
        filters: vec!["service:checkout".into()],
    }];
    i.registry = vec![
        fact("/Build/commit", "9f2a1c4", "2026-07-31T08:15:30Z"),
        RegistryFact {
            ttl_secs: Some(1800),
            expired: Some(false),
            ..fact(
                "/Action",
                "checkout smoke, 20 iterations",
                "2026-07-31T13:58:00Z",
            )
        },
        RegistryFact {
            ttl_secs: Some(3600),
            expired: Some(true),
            ..fact("/Env/host", "ci-7", "2026-07-31T09:00:00Z")
        },
    ];
    i.asserted = vec![ScopedFact {
        key: "@/Data/seed".into(),
        value: "8814".into(),
    }];
    i.neighbours = vec![
        entry(
            41019,
            Level::Info,
            "reserve: acquiring inventory lock sku=A11",
        ),
        entry(
            41020,
            Level::Debug,
            "reserve: lock held by txn 8f21, waiting",
        ),
        entry(41021, Level::Warn, "reserve: wait exceeded 2000ms"),
        entry(41022, Level::Error, "checkout worker stalled awaiting lock"),
        entry(41023, Level::Info, "health: worker pool 3/4 responsive"),
    ];
    i.spans = vec![
        span_named("checkout.reserve", "checkout", 4210.0),
        span_named("db.select_inventory", "postgres", 18.4),
        span_named("checkout.price", "checkout", 6.1),
    ];
    i.collectors = vec![
        CollectorLine {
            name: "checkout".into(),
            owner: "cli".into(),
            matched: 4021,
            snapshots: 2,
            latest_snapshot: Some(("before".into(), t("2026-07-31T10:15:30Z"))),
            zeroed_by: None,
        },
        CollectorLine {
            name: "db".into(),
            owner: "claude-t3".into(),
            matched: 0,
            snapshots: 0,
            latest_snapshot: None,
            zeroed_by: Some("daemon_restart"),
        },
    ];
    println!("{}", render(&i).body);
}

fn span_named(name: &str, service: &str, ms: f64) -> SpanEntry {
    SpanEntry {
        seq: 1,
        trace_id: 7,
        span_id: 1,
        parent_span_id: None,
        start_time: t("2026-07-31T14:02:07Z"),
        end_time: t("2026-07-31T14:02:11Z"),
        duration_ms: ms,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: service.into(),
        status: SpanStatus::Ok,
        attributes: Default::default(),
        events: Vec::new(),
    }
}

#[test]
fn a_complete_capture_with_nothing_missing_says_there_is_nothing_to_do() {
    let r = render(&base());
    let next = r.body.split("## 3. What to do next").nth(1).unwrap();
    assert!(next.contains("Nothing limits this capture"), "{next}");
    assert!(r.notes.is_empty(), "{:?}", r.notes);
}
