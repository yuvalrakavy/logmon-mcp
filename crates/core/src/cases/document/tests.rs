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
        stem: "checkout-hang-260731-141530".into(),
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
            clamped: false,
            short_before: 0,
            short_after: 0,
            log_lost_below: 0,
            spans_evicted_before_window: None,
        },
        logdata: FilePointer {
            file: "checkout-hang-260731-141530.logdata.jsonl".into(),
            records: 700,
        },
        spandata: FilePointer {
            file: "checkout-hang-260731-141530.spandata.jsonl".into(),
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
        fm.contains(
            "seq_range: {from: 40672, to: 41372, requested_before_missing: 0, \
             requested_after_missing: 0, clamped: false}"
        ),
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
            // OLDEST first in insertion order, so "renders in insertion order"
            // and "renders newest-validated first" are distinguishable. The
            // earlier fixture had them coincide, which made the sort — and
            // therefore the render cap's whole meaning — unfalsifiable.
            f.validated_at = t("2026-07-31T14:03:11Z") - chrono::Duration::seconds(399 - n);
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
    // Newest-validated survives the cut; the stalest is what falls off. Without
    // the sort the cap drops an arbitrary subset instead of the least useful
    // one, which is what gives the cap its meaning.
    assert!(table.contains("/Big/key0399"), "{}", &table[..2000]);
    assert!(
        !table.contains("/Big/key0000"),
        "the stalest key should be cut, not whichever happened to be last"
    );
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
        out.contains("Spans: 168 captured, and the span ring had dropped nothing below seq 40672"),
        "{out}"
    );
    assert!(
        out.contains("Session filters never narrow spans"),
        "a reader cannot otherwise tell whether the filter applied to spans too: {out}"
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

/// A window narrower than requested is stated — and the document must say
/// **why**, because "the ring dropped them" and "the domain has no more history"
/// are opposite conclusions.
#[test]
fn a_short_window_says_whether_the_shortfall_is_a_loss() {
    // Short, and records really did leave.
    let mut lost = base();
    lost.window.short_before = 321;
    // In practice `from` and the floor coincide here: `from` is the first
    // SURVIVING record, so a window short at the bottom starts exactly where
    // the ring's floor is.
    lost.window.log_lost_below = 40672;
    lost.window.verdict = EvidenceVerdict::Evicted;
    let out = render(&lost).body;
    assert!(out.contains("**narrower than requested**"), "{out}");
    assert!(out.contains("321 record(s) short"), "{out}");
    assert!(out.contains("dropped everything under seq 40672"), "{out}");
    assert!(out.contains("**gone** rather than never recorded"), "{out}");
    assert!(
        out.contains("Raise the domain's `log_buffer_size`"),
        "and the remedy is the one that would help: {out}"
    );

    // Short, but nothing ever left — a young domain, not a gap.
    let mut young = base();
    young.window.short_before = 321;
    let out = render(&young).body;
    assert!(out.contains("**narrower than requested**"), "{out}");
    assert!(
        out.contains("empty past rather than a loss"),
        "a domain with no more history has lost nothing: {out}"
    );
    assert!(
        !out.contains("gone"),
        "and must not be described as loss: {out}"
    );
}

/// The shortfall has two ends and they mean opposite things. A window short only
/// at the TOP asked for more future than had happened; nothing was lost, and the
/// document must not explain it with a sentence about how far back the store
/// reaches.
///
/// The defect: the cause line was gated on `short_before > 0 || short_after > 0`
/// while saying only "seq N is as far back as this store has ever held" — a
/// claim about the bottom, printed for a shortfall at the top, and false
/// whenever the window's bottom is simply where `before` ran out.
#[test]
fn a_shortfall_after_the_anchor_is_not_explained_by_the_stores_floor() {
    let mut i = base();
    i.window.short_after = 400;
    let out = render(&i).body;
    assert!(out.contains("400 after"), "{out}");
    assert!(
        out.contains("history that had not happened yet"),
        "a ring evicts from the bottom, so a shortfall above the anchor is never a loss: {out}"
    );
    assert!(
        !out.contains("as far back as this store has ever held"),
        "and nothing about the BOTTOM belongs in the explanation: {out}"
    );
    assert!(
        !out.contains("empty past"),
        "which is the bottom's phrasing, not the top's: {out}"
    );

    // Both ends short: each gets its own sentence, neither borrows the other's.
    let mut both = base();
    both.window.short_before = 12;
    both.window.short_after = 400;
    let out = render(&both).body;
    assert!(out.contains("empty past rather than a loss"), "{out}");
    assert!(out.contains("history that had not happened yet"), "{out}");
}

/// `evicted` names the store's floor and the caller's shortfall as two separate
/// numbers, and **claims no count of what was lost**.
///
/// The defect: `short_before` — bounded by `before`, not by what the ring
/// dropped — was rendered as records-lost. A ring of 30 that had ever received
/// 32 records, asked for 100 of context, reported "at least 71 records are
/// gone" from a domain whose entire history was 32.
#[test]
fn the_evicted_verdict_claims_no_count_of_lost_records() {
    let mut i = base();
    i.window.verdict = EvidenceVerdict::Evicted;
    i.window.from = 3;
    i.window.short_before = 71;
    i.window.log_lost_below = 3;
    let r = render(&i);

    assert!(
        r.body.contains("It starts at seq 3, the ring has dropped everything below that"),
        "the store fact — a seq: {}",
        r.body
    );
    assert!(
        r.body.contains("71 record(s) short before the anchor"),
        "the request fact — a count of what did not come back: {}",
        r.body
    );
    assert!(
        r.body
            .contains("bounded by what was REQUESTED, not by what was lost"),
        "and the verdict says outright why the number below is not a loss count: {}",
        r.body
    );
    assert!(
        r.body.contains("not knowable from here"),
        "and that the split between the two is unrecoverable: {}",
        r.body
    );
    for claim in ["71 records are gone", "up to 71", "at least 71"] {
        assert!(
            !r.body.contains(claim),
            "a shortfall bounded by `before` must never be rendered as records lost ({claim}): {}",
            r.body
        );
    }
    let note = r
        .notes
        .iter()
        .find(|n| n.kind == NOTE_CAPTURE_GAP)
        .expect("evicted raises a capture-gap note");
    assert!(
        !note.detail.contains("up to 71") && !note.detail.contains("71 records are gone"),
        "and the note carries the same two facts, not a fabricated count: {}",
        note.detail
    );
}

/// The clamp is its own fact, and — the defect this replaces — it must not
/// contradict `complete` four lines above it.
#[test]
fn a_clamped_request_never_contradicts_the_verdict() {
    let mut i = base();
    i.window.clamped = true;
    let out = render(&i).body;

    assert_eq!(i.window.verdict, EvidenceVerdict::Complete);
    assert!(out.contains("**`complete`**"), "{out}");
    assert!(out.contains("clamped to the maximum"), "{out}");
    // Structural, not literal. Three negative assertions used to name the exact
    // sentences the old renderer emitted — strings that exist nowhere in the
    // crate now, so no plausible edit could make them fire again. This one can:
    // a clamped window is not a capped one, in ANY wording, and "clamped" does
    // not contain "capped".
    assert!(
        !out.contains("capped"),
        "nothing inside the window was cut, so no sentence may call this read \
         capped while the verdict above says complete: {out}"
    );
    // And the remedy must be one a caller can act on: the clamp is hard, so
    // "ask for more" is not it.
    assert!(out.contains("will not widen this window"), "{out}");
    assert!(
        out.contains("clamped: true"),
        "and front matter carries it under its own name: {out}"
    );
}

/// §4.2's own trap, rendered: one counter numbers both stores, so a range of N
/// seqs holding fewer than N log records is normal. A reader who cannot tell
/// that from a gap will read the arithmetic as loss.
#[test]
fn the_complete_paragraph_explains_why_records_can_be_fewer_than_seqs() {
    let out = render(&base()).body;
    assert!(
        out.contains("one counter numbers both"),
        "701 seqs and 700 records must be reconcilable from the document alone: {out}"
    );
}

/// The provenance table grades each key, and the grade must come from the key —
/// hard-wiring the column to either value renders a table that reads as
/// authoritative and is uniformly wrong.
#[test]
fn the_provenance_table_grades_each_key_from_the_core_set() {
    let mut i = base();
    i.registry.push(fact("/Data/seed", "8814", "2026-07-31T14:03:11Z"));
    let out = render(&i).body;

    let rows: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("| `/") && l.contains(" | "))
        .collect();
    assert!(rows.len() > CORE_KEYS.len(), "{out}");
    for row in &rows {
        let kind = row.split('|').nth(2).expect("a kind column").trim();
        let key = row.split('`').nth(1).expect("a key");
        let expected = if CORE_KEYS.contains(&key) {
            "core"
        } else {
            "contextual"
        };
        assert_eq!(kind, expected, "`{key}` is graded {kind}: {row}");
    }
    // Both values are actually present, so a hard-wired column cannot pass by
    // matching a fixture that happens to be all of one kind.
    assert!(rows.iter().any(|r| r.contains("| core |")), "{out}");
    assert!(rows.iter().any(|r| r.contains("| contextual |")), "{out}");
}

/// §2's caveat is repeated in §4 because §2 is where the caveat is and §4 is
/// where the trap is: a reader who scrolled past it sees a tidy run of
/// consecutive lines and a pointer to "the full window", both of which are true
/// only of what was STORED. Deleting the repeat leaves §4 reading as
/// reassurance.
#[test]
fn the_filter_caveat_is_repeated_where_the_trap_is() {
    let plain = render(&base()).body;
    let section_4 = |body: &str| body.split("## 4. ").nth(1).unwrap_or("").to_string();
    assert!(
        !section_4(&plain).contains("not the entries that occurred"),
        "an unfiltered window has no such caveat to make: {plain}"
    );

    let mut narrowed = base();
    narrowed.window.verdict = EvidenceVerdict::Filtered;
    narrowed.window.narrowed_by = vec![NarrowedRange {
        from_seq: 40672,
        to_seq: 40800,
        filters: vec!["l>=ERROR".into()],
    }];
    let body = render(&narrowed).body;
    assert!(
        section_4(&body).contains("not the entries that occurred"),
        "the caveat belongs where the neighbour run is, not only in §2: {body}"
    );
    assert!(
        body.matches("not the entries that occurred").count() >= 1
            && section_4(&body).contains("as stored"),
        "and it qualifies the logdata pointer that sits beside it: {body}"
    );
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
        7,
        "the row kept its shape: {row}"
    );
}

/// A **backslash before a pipe** still splits the row, because escaping the pipe
/// and not the backslash produces `\\|` — markdown consumes `\\` as one literal
/// backslash and the `|` behind it is an unescaped delimiter.
///
/// Not hypothetical: the filter DSL compiles Rust regexes, so matching a literal
/// pipe in a pipe-delimited log format is written `m~/id\|name/`, and a filter
/// string is exactly the kind of caller text that reaches a cell. A value ending
/// in a lone backslash is the mirror case — it escapes the row's own closing
/// delimiter.
#[test]
fn a_backslash_before_a_pipe_does_not_split_the_row() {
    // Count delimiters the way a markdown renderer does: scan forward, and let
    // a backslash consume the character after it. The naive "is the previous
    // byte a backslash" test encodes the SAME mistake as the buggy escaper and
    // reports the broken row as fine — it did, on the first draft of this test.
    fn delimiters(row: &str) -> usize {
        let b = row.as_bytes();
        let (mut n, mut i) = (0, 0);
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'|' => {
                    n += 1;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        n
    }

    let plain = render(&base()).body;
    let expected = delimiters(
        plain
            .lines()
            .find(|l| l.starts_with("| `/Build/commit`"))
            .expect("a baseline row"),
    );

    for value in [r"id\|name", r"trailing\", r"\\|both"] {
        let mut i = base();
        i.registry = vec![fact("/Note", value, "2026-07-31T14:03:11Z")];
        let out = render(&i).body;
        let row = out
            .lines()
            .find(|l| l.starts_with("| `/Note`"))
            .expect("the key is rendered as a table row");
        assert_eq!(
            delimiters(row),
            expected,
            "value {value:?} changed the row's column count: {row:?}"
        );
    }
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
