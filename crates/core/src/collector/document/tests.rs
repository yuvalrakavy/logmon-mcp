//! §12: V18 document completeness including per-arm provenance, `aggregation`
//! and `correctness_evidence` · V20 sizing · V21 bytes-vs-path · V22 lossless
//! regeneration · V23 body order and per-table tier.

use super::*;
use crate::collector::history::{SnapshotPolicy, StoredSnapshot};
use crate::collector::sample::Level;
use crate::collector::state::{Collector, CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
use crate::filter::parser::parse_filter;
use crate::receiver::TraceIngestLoss;
use crate::span::types::{SpanEntry, SpanKind, SpanStatus};
use chrono::TimeZone;
use std::collections::HashMap;

const S: i64 = 1_700_000_000_000_000_000;
const MS: i64 = 1_000_000;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_nanos(S + secs * 1_000_000_000)
}

fn def(level: Level) -> CollectorDef {
    CollectorDef {
        name: "cache".into(),
        filter_string: "sv=store_server".into(),
        filter: parse_filter("sv=store_server").unwrap(),
        level,
        group_keys: vec![],
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: Some("the read-through cache".into()),
        threshold: None,
    }
}

fn span(id: u64, parent: Option<u64>, start_ns: i64, dur_ns: i64, name: &str) -> SpanEntry {
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: id,
        parent_span_id: parent,
        start_time: Utc.timestamp_nanos(start_ns),
        end_time: Utc.timestamp_nanos(start_ns + dur_ns),
        duration_ms: 0.0,
        name: name.into(),
        kind: SpanKind::Internal,
        service_name: "store_server".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

/// An arm of `n` spans named `name`, each `each_ms` long.
fn arm(spec: &str, n: usize, each_ms: i64, name: &str) -> Arm {
    arm_at(spec, Level::Tree, |c| {
        for i in 0..n {
            c.ingest(&span(
                i as u64 + 1,
                None,
                S + i as i64 * 50 * MS,
                each_ms * MS,
                name,
            ));
        }
    })
}

fn arm_at(spec: &str, level: Level, feed: impl FnOnce(&Collector)) -> Arm {
    let c = Collector::new(def(level), at(0));
    feed(&c);
    Arm::from_live(
        spec.into(),
        &c.snapshot(),
        Some(TraceIngestLoss::default()),
        at(60),
    )
}

/// A recorded run, so `meta` and stored paths are exercised.
fn stored(label: &str, n: usize, each_ms: i64, meta: serde_json::Value) -> StoredSnapshot {
    let c = Collector::new(def(Level::Tree), at(0));
    for i in 0..n {
        c.ingest(&span(
            i as u64 + 1,
            None,
            S + i as i64 * 50 * MS,
            each_ms * MS,
            "reconcile",
        ));
    }
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    StoredSnapshot::from_view(
        label.into(),
        Some(format!("run {label}")),
        meta,
        at(60),
        SnapshotPolicy::default(),
        view,
        Some(TraceIngestLoss::default()),
        projected,
    )
}

fn md(arms: &[Arm], opts: &DocumentOptions) -> String {
    document(arms, opts, at(120)).expect("rendered").content
}

// ---------------------------------------------------------------------------
// Body order and completeness (V18, V23)
// ---------------------------------------------------------------------------

#[test]
fn the_body_is_in_the_order_the_spec_specifies() {
    // V23. Most-decision-relevant first, reference last — a reader who stops
    // after one screen must have got the part that changes what they do.
    let doc = md(
        &[arm("a", 20, 10, "reconcile"), arm("b", 20, 5, "reconcile")],
        &DocumentOptions::default(),
    );
    let idx = |h: &str| {
        doc.find(h)
            .unwrap_or_else(|| panic!("missing section: {h}"))
    };
    let order = [
        "## 1. What moved",
        "## 2. What to do next",
        "## 3. Per-run detail",
        "## 4. The full vocabulary",
        "## 5. Definitions and how to read the numbers",
    ];
    let positions: Vec<usize> = order.iter().map(|h| idx(h)).collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "sections out of order: {positions:?}"
    );

    // And the collector-config block is demoted BELOW the tier definitions
    // (§9.5), because a reader who needs the config already knows what they are
    // looking for while one who needs the tiers does not know they need them.
    assert!(
        idx("### The three tiers are not three views of one number")
            < idx("### The collectors this describes"),
        "tier definitions must precede the collector config"
    );
}

#[test]
fn every_front_matter_field_is_present_even_when_it_has_no_value() {
    // §9.4: no field is ever merely absent. An absent key is indistinguishable
    // from a key nobody thought to emit, and these are meant to be grepped.
    let doc = md(&[arm("a", 5, 10, "reconcile")], &DocumentOptions::default());
    let fm = doc.split("---").nth(1).expect("front matter");

    for key in [
        "collector:",
        "generated_at:",
        "arm_count:",
        "question:",
        "finding:",
        "filter_intent:",
        "trustworthy:",
        "correctness_evidence:",
        "aggregation:",
        "sidecar:",
        "arms:",
        "limitations_active:",
    ] {
        assert!(fm.contains(key), "front matter missing {key}:\n{fm}");
    }
    // The unset ones carry explicit values rather than being omitted.
    assert!(fm.contains("question: null"), "{fm}");
    assert!(fm.contains("finding: null"), "{fm}");
    assert!(
        fm.contains("correctness_evidence: unknown"),
        "defaults to unknown, never absent:\n{fm}"
    );
}

#[test]
fn per_arm_provenance_is_read_from_meta_and_says_unknown_when_absent() {
    // §9.4: `git_sha` and `build_profile` are per-arm, and `build_profile` is
    // promoted out of `meta` because comparing debug against release is worse
    // than having no data.
    let with = Arm::from_snapshot(
        "cache@a".into(),
        &stored(
            "a",
            5,
            10,
            serde_json::json!({ "git_sha": "abc123", "build_profile": "release" }),
        ),
    );
    let without = Arm::from_snapshot("cache@b".into(), &stored("b", 5, 10, serde_json::json!({})));

    let doc = md(&[with, without], &DocumentOptions::default());
    let fm = doc.split("---").nth(1).unwrap();
    assert!(fm.contains("git_sha: \"abc123\""), "{fm}");
    assert!(fm.contains("build_profile: \"release\""), "{fm}");
    assert_eq!(
        fm.matches("git_sha: unknown").count(),
        1,
        "the arm without provenance says unknown rather than omitting it:\n{fm}"
    );
}

#[test]
fn aggregation_is_always_stated_and_a_single_run_arm_is_not_trustworthy() {
    // §9.4: a reader looking at one figure cannot otherwise tell one run from
    // the mean of five, which makes the floor load-bearing and unverifiable.
    let doc = md(
        &[arm("a", 20, 10, "reconcile"), arm("b", 20, 5, "reconcile")],
        &DocumentOptions::default(),
    );
    assert!(doc.contains("trustworthy: false"));
    assert!(doc.contains("every arm is a single run"), "{doc}");
    assert!(
        doc.contains("**Single-run arm.**"),
        "and the limitation is listed with its remedy"
    );
    assert!(doc.contains("Repeat each arm and compare"));
}

#[test]
fn a_merged_arm_document_is_trustworthy_and_prints_a_real_floor() {
    let runs: Vec<StoredSnapshot> = [10, 11, 12]
        .iter()
        .enumerate()
        .map(|(i, ms)| stored(&format!("r{i}"), 20, *ms, serde_json::json!({})))
        .collect();
    let other: Vec<StoredSnapshot> = [5, 6, 7]
        .iter()
        .enumerate()
        .map(|(i, ms)| stored(&format!("s{i}"), 20, *ms, serde_json::json!({})))
        .collect();
    let a = Arm::merged("cache@*".into(), &runs).unwrap();
    let b = Arm::merged("cache2@*".into(), &other).unwrap();

    let doc = md(&[a, b], &DocumentOptions::default());
    assert!(doc.contains("trustworthy: true"), "{doc}");
    assert!(doc.contains("every arm merges several runs"));
    assert!(
        doc.contains("Run-to-run, `cache@*`:") && doc.contains("CV"),
        "a real floor is printed"
    );
    assert!(
        !doc.contains("**Single-run arm.**"),
        "and the single-run limitation is not listed"
    );
}

// ---------------------------------------------------------------------------
// The two percentages (§9.5)
// ---------------------------------------------------------------------------

#[test]
fn a_ranked_table_gives_both_percentages_and_carries_an_other_row() {
    // §9.5: "share of improvement" and "change to this row" are different
    // numbers readers conflate, and a ranked table without an `other` row lets
    // a reader add up the visible rows and conclude they are the whole change.
    let mk = |spec: &str, hot: i64, cold: i64| {
        arm_at(spec, Level::Tree, |c| {
            for i in 0..5i64 {
                let base = S + i * 100 * MS;
                c.ingest(&span(i as u64 * 2 + 1, None, base, hot * MS, "hot"));
                c.ingest(&span(
                    i as u64 * 2 + 2,
                    None,
                    base + 50 * MS,
                    cold * MS,
                    "cold",
                ));
            }
        })
    };
    let doc = md(
        &[mk("a", 100, 10), mk("b", 20, 10)],
        &DocumentOptions::default(),
    );

    assert!(doc.contains("Δ to this row"), "{doc}");
    assert!(doc.contains("share of total Δ"), "{doc}");

    // Both rows are shown and they reconcile, so the table says so and there is
    // no `other` row. An all-zero `other` row on a complete table is noise that
    // makes the informative case easier to ignore.
    assert!(doc.contains("**By name** — all 2 rows"), "{doc}");
    assert!(!doc.contains("_other ("), "nothing was withheld: {doc}");

    // `hot` accounts for the entire change, `cold` for none of it — and a row
    // that did not move must read `0.0%`, not `-0.0%`, because section 5 gives
    // a negative share the specific meaning "moved against the total".
    assert!(
        doc.contains("| +100.0% |"),
        "hot is the whole change: {doc}"
    );
    assert!(!doc.contains("-0.0%"), "no signed zero: {doc}");
    assert!(doc.contains("| 0.0% |"), "cold moved not at all: {doc}");

    // The definitions section explains the distinction rather than leaving the
    // two column headings to speak for themselves.
    assert!(doc.contains("### The two percentages in a ranked table"));
    assert!(doc.contains("moved **against** the total"));
}

#[test]
fn a_truncated_ranked_table_says_how_many_it_withheld_and_totals_them() {
    // §9.5. Without the pre-truncation count the table could only say "top 3 of
    // 3" while having dropped 47, and a reader would add up the visible rows and
    // conclude they were the whole change.
    let mk = |spec: &str, first: i64| {
        arm_at(spec, Level::Tree, |c| {
            for i in 0..20u64 {
                c.ingest(&span(
                    i + 1,
                    None,
                    S + i as i64 * 10 * MS,
                    if i == 0 { first } else { 10 } * MS,
                    &format!("op{i}"),
                ));
            }
        })
    };
    let doc = md(
        &[mk("a", 500), mk("b", 100)],
        &DocumentOptions {
            top_n: 3,
            ..Default::default()
        },
    );
    assert!(doc.contains("top 3 of 20"), "{doc}");
    assert!(
        doc.contains("_other (17 rows not shown)_"),
        "and names how many: {doc}"
    );
}

#[test]
fn an_avg_ms_delta_is_suppressed_and_the_document_says_the_population_moved() {
    let doc = md(
        &[arm("a", 10, 10, "reconcile"), arm("b", 40, 10, "reconcile")],
        &DocumentOptions::default(),
    );
    assert!(
        doc.contains("**Population moved.**"),
        "the limitation is active: {doc}"
    );
    assert!(doc.contains("the denominator moved with the numerator"));
}

// ---------------------------------------------------------------------------
// Limitations (§9.6)
// ---------------------------------------------------------------------------

#[test]
fn only_the_limitations_that_apply_are_listed() {
    // The fix for rev 2's failure: it listed every limitation, and a reader
    // combined two individually-true rows into a false conclusion. A limitation
    // that does not apply makes the ones that do harder to find.
    let clean: Vec<StoredSnapshot> = [10, 10, 10]
        .iter()
        .enumerate()
        .map(|(i, ms)| stored(&format!("r{i}"), 20, *ms, serde_json::json!({})))
        .collect();
    let a = Arm::merged("cache@*".into(), &clean).unwrap();
    let b = Arm::merged("cache2@*".into(), &clean).unwrap();
    let doc = md(&[a, b], &DocumentOptions::default());

    assert!(!doc.contains("**Ingest loss.**"), "no loss, not listed");
    assert!(!doc.contains("**Arm truncated.**"), "not truncated");
    assert!(!doc.contains("**Nested matches.**"), "nothing nested");
    assert!(!doc.contains("**Single-run arm.**"), "three runs each");
    // These always apply and are always listed.
    assert!(doc.contains("**Estimated percentiles.**"));
    assert!(doc.contains("**Time, not correctness.**"));
    assert!(doc.contains("**Work, not elapsed time.**"));
}

#[test]
fn ingest_loss_is_listed_and_the_vocabulary_says_consistency_is_not_completeness() {
    // §9.5's requirement, and a genuinely counter-intuitive point: the counters
    // reconciling to `matched` proves nothing about whether the population is
    // complete.
    // Symmetric loss, because the diff *refuses* asymmetric loss outright —
    // §5.6 calls that the one place marking is insufficient. So a document
    // that reports loss at all is one where both arms lost spans.
    let loss = TraceIngestLoss {
        dropped: 7,
        shed_batches: 1,
        malformed: 0,
    };
    let mut a = arm("a", 20, 10, "reconcile");
    let mut b = arm("b", 20, 10, "reconcile");
    a.ingest = Some(loss);
    b.ingest = Some(loss);
    let doc = md(&[a, b], &DocumentOptions::default());

    assert!(doc.contains("**Ingest loss.**"), "{doc}");
    assert!(doc.contains("before the collector saw them"));
    assert!(
        doc.contains("consistency and not completeness"),
        "the reconciliation note is present"
    );
    assert!(doc.contains("`matched` is a lower bound"));
    assert!(
        doc.contains("| `a` | 20 | 0 | 0 | 0 | 0 | 7 | 1 | 0 |"),
        "and the vocabulary table shows every counter including the zeros"
    );
}

#[test]
fn a_document_of_asymmetrically_lossy_arms_needs_the_flag_and_then_says_so() {
    // Two-way evidence for the rule above: the document cannot launder a
    // refusal, and when the caller overrides it the override is visible.
    let mut a = arm("a", 20, 10, "reconcile");
    a.ingest = Some(TraceIngestLoss {
        dropped: 7,
        shed_batches: 0,
        malformed: 0,
    });
    let arms = vec![a, arm("b", 20, 10, "reconcile")];
    let err = document(&arms, &DocumentOptions::default(), at(120)).expect_err("refused");
    assert!(err.to_string().contains("allow_lossy"), "{err}");

    let doc = md(
        &arms,
        &DocumentOptions {
            allow: crate::collector::diff::DiffAllowances {
                lossy: true,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert!(doc.contains("**Ingest loss.**"), "{doc}");
    assert!(doc.contains("trustworthy: false"));
}

#[test]
fn unknown_ingest_loss_is_a_distinct_limitation_from_loss() {
    // `None` is unknown, not zero, all the way to the rendered document.
    let mut a = arm("a", 20, 10, "reconcile");
    a.ingest = None;
    let doc = md(
        &[a, arm("b", 20, 10, "reconcile")],
        &DocumentOptions::default(),
    );
    assert!(doc.contains("**Ingest loss unknown.**"), "{doc}");
    assert!(
        doc.contains("ingest_loss: unknown"),
        "in the front matter too"
    );
    assert!(!doc.contains("**Ingest loss.**"), "and not the other one");
}

#[test]
fn the_effect_matrix_says_which_numbers_each_limitation_reaches() {
    let mut a = arm("a", 20, 10, "reconcile");
    a.sample_complete = false;
    let doc = md(
        &[a, arm("b", 20, 10, "reconcile")],
        &DocumentOptions::default(),
    );

    assert!(doc.contains("### Which numbers each limitation reaches"));
    assert!(doc.contains("| number | "), "the matrix header");
    // Truncation reaches the sampled rows and not the exact ones — which is the
    // pair rev 2 got individually right and jointly misleading.
    let matrix = doc
        .split("### Which numbers each limitation reaches")
        .nth(1)
        .unwrap()
        .split("### The two floors")
        .next()
        .unwrap();
    let line = |m: &str| {
        matrix
            .lines()
            .find(|l| l.starts_with(&format!("| `{m}`")))
            .unwrap_or_else(|| panic!("no matrix row for {m} in:\n{matrix}"))
    };
    // Truncation reaches the sampled percentiles, which this document HAS.
    assert!(
        line("sampled p50/p80/p95/p99").contains("yes"),
        "truncation reaches the sampled tier: {}",
        line("sampled p50/p80/p95/p99")
    );
    // And `self_ms` reads `n/a` rather than `-`: these arms have no nesting, so
    // self time was suppressed and is not in the document. Marking it `-` would
    // be a clean bill of health for a number the reader cannot see, which is
    // worse than saying nothing.
    assert!(
        line("self_ms").contains("n/a"),
        "absent, not clean: {}",
        line("self_ms")
    );
    assert!(
        matrix.contains("**not in this document at all**"),
        "and the legend explains n/a: {matrix}"
    );
    // Every active limitation has a column, so a `-` means checked-and-clear
    // rather than never-considered.
    assert!(matrix.contains("Every limitation listed above has a column here"));
    assert!(
        matrix.contains("retries"),
        "including the one that inflates everything: {matrix}"
    );

    let count_row = line("count");
    let truncation_col = matrix
        .lines()
        .find(|l| l.starts_with("| number |"))
        .unwrap()
        .split('|')
        .position(|c| c.trim() == "truncation")
        .expect("truncation column present");
    let cells: Vec<&str> = count_row.split('|').collect();
    assert_eq!(
        cells[truncation_col].trim(),
        "-",
        "truncation does NOT reach count: {count_row}"
    );
}

#[test]
fn both_floors_are_named_separately_and_never_conflated() {
    // §6.5: two distinct floors, and a result must not conflate them.
    let doc = md(
        &[arm("a", 20, 10, "reconcile"), arm("b", 20, 5, "reconcile")],
        &DocumentOptions::default(),
    );
    let floors = doc.split("### The two floors").nth(1).unwrap();
    assert!(floors.contains("Run-to-run"));
    assert!(floors.contains("Measurement resolution"));
    assert!(
        floors.contains("repeating the run will not improve it"),
        "the distinction that matters: one floor shrinks with repetition and one does not"
    );
}

// ---------------------------------------------------------------------------
// Per-run detail (§9.5 point 3)
// ---------------------------------------------------------------------------

#[test]
fn every_per_run_table_declares_its_tier() {
    let doc = md(
        &[arm("a", 20, 10, "reconcile")],
        &DocumentOptions::default(),
    );
    let detail = doc.split("## 3. Per-run detail").nth(1).unwrap();
    assert!(detail.contains("**Exact tier**"));
    assert!(detail.contains("**Estimated tier**"));
    assert!(detail.contains("**Sampled tier**"));
}

#[test]
fn a_truncated_arms_sampled_percentiles_are_visually_demoted() {
    // §9.5: not printed at equal weight beside estimated ones they may differ
    // from by 40%. Struck through in the markdown so the demotion survives
    // being pasted somewhere without the surrounding prose.
    let mut a = arm("a", 20, 10, "reconcile");
    a.sample_complete = false;
    if let Some(p) = a.projections.as_mut() {
        p.complete = false;
    }
    let doc = md(&[a], &DocumentOptions::default());
    assert!(
        doc.contains("**Sampled tier — TRUNCATED, read the estimated tier instead.**"),
        "{doc}"
    );
    assert!(doc.contains("~~"), "and the values are struck through");
}

#[test]
fn a_scalar_arm_says_why_the_sampled_tier_is_absent() {
    let doc = md(
        &[arm_at("a", Level::Scalar, |c| {
            for i in 0..5 {
                c.ingest(&span(i + 1, None, S, 10 * MS, "reconcile"));
            }
        })],
        &DocumentOptions::default(),
    );
    assert!(doc.contains("level `scalar`, which retains no"), "{doc}");
}

#[test]
fn a_merged_arm_says_sample_derived_figures_do_not_merge() {
    let runs: Vec<StoredSnapshot> = [10, 10]
        .iter()
        .enumerate()
        .map(|(i, ms)| stored(&format!("r{i}"), 5, *ms, serde_json::json!({})))
        .collect();
    let doc = md(
        &[Arm::merged("cache@*".into(), &runs).unwrap()],
        &DocumentOptions::default(),
    );
    assert!(doc.contains("do not merge"), "{doc}");
    assert!(doc.contains("is not a self time"));
}

#[test]
fn a_single_arm_document_says_nothing_moved_rather_than_inventing_a_comparison() {
    let doc = md(
        &[arm("a", 20, 10, "reconcile")],
        &DocumentOptions::default(),
    );
    assert!(doc.contains("describes **one** arm"), "{doc}");
    assert!(doc.contains("no comparison to report"));
}

// ---------------------------------------------------------------------------
// Regeneration and sizing (V20, V22)
// ---------------------------------------------------------------------------

#[test]
fn regeneration_with_a_finding_is_lossless_and_adds_only_the_finding() {
    // V22, and the reason `path` is optional: `finding` normally arrives after
    // the first read, so regeneration has to be free.
    let arms = || vec![arm("a", 20, 10, "reconcile"), arm("b", 20, 5, "reconcile")];
    let first = md(&arms(), &DocumentOptions::default());
    let second = md(
        &arms(),
        &DocumentOptions {
            question: Some("did the cache help".into()),
            finding: Some("yes, halved".into()),
            ..Default::default()
        },
    );
    assert!(second.contains("**Question.** did the cache help"));
    assert!(second.contains("**Finding.** yes, halved"));
    assert!(second.contains("question: \"did the cache help\""));

    // Everything the first one said is still said: nothing is traded for the
    // annotations.
    for section in [
        "## 1. What moved",
        "## 2. What to do next",
        "## 3. Per-run detail",
        "## 4. The full vocabulary",
        "## 5. Definitions",
    ] {
        assert!(first.contains(section) && second.contains(section));
    }
}

#[test]
fn a_document_is_tens_of_kilobytes_and_the_bulk_goes_to_a_sidecar() {
    // V20. A cardinality-heavy collector is the case that would otherwise
    // produce a megabyte of tables.
    let mk = |spec: &str| {
        arm_at(spec, Level::Tree, |c| {
            for i in 0..400u64 {
                c.ingest(&span(
                    i + 1,
                    None,
                    S + i as i64 * MS,
                    (i as i64 % 50 + 1) * MS,
                    &format!("op{i}"),
                ));
            }
        })
    };
    let d = document(
        &[mk("a"), mk("b")],
        &DocumentOptions {
            top_n: 100,
            ..Default::default()
        },
        at(120),
    )
    .expect("rendered");

    assert!(
        d.bytes < SIZE_SOFT_CAP * 2,
        "document is {} bytes, which is not 'tens of KB'",
        d.bytes
    );
    let side = d.sidecar.as_ref().expect("a sidecar is always produced");
    assert!(side.name.ends_with(".sidecar.json"));
    // §9.7: a percentile table plus layout identity, which is what decides
    // whether two documents are comparable. Raw buckets are NOT in it.
    assert!(side.content.contains("percentiles_ms"));
    assert!(side.content.contains("max_num_bins"));
    assert!(
        !side.content.contains("bins\":["),
        "raw sketch buckets are deliberately absent"
    );
}

#[test]
fn the_sidecar_is_named_in_the_front_matter_so_the_two_can_be_reunited() {
    let d = document(
        &[arm("a", 5, 10, "reconcile")],
        &DocumentOptions::default(),
        at(120),
    )
    .unwrap();
    let name = &d.sidecar.as_ref().unwrap().name;
    assert!(
        d.content.contains(&format!("sidecar: \"{name}\"")),
        "front matter must name it: {}",
        d.content.split("---").nth(1).unwrap()
    );
}

// ---------------------------------------------------------------------------
// json
// ---------------------------------------------------------------------------

#[test]
fn the_json_form_carries_the_same_claims_as_the_markdown() {
    let d = document(
        &[arm("a", 20, 10, "reconcile"), arm("b", 20, 5, "reconcile")],
        &DocumentOptions {
            format: Format::Json,
            ..Default::default()
        },
        at(120),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&d.content).expect("valid json");

    assert_eq!(v["trustworthy"], false);
    assert_eq!(v["correctness_evidence"], "unknown");
    assert_eq!(v["arms"][0]["git_sha"], "unknown");
    assert_eq!(v["arms"][0]["runs"], 1);
    assert!(v["comparisons"][0]["rows"].as_array().unwrap().len() > 4);
    assert!(
        v["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["remedy"].is_string()),
        "every limitation carries a remedy"
    );
}

// ---------------------------------------------------------------------------
// folded (§9.8)
// ---------------------------------------------------------------------------

fn folded_of(s: &StoredSnapshot) -> Result<Document, DocumentError> {
    document(
        &[Arm::from_snapshot("cache@r".into(), s)],
        &DocumentOptions {
            format: Format::Folded,
            ..Default::default()
        },
        at(120),
    )
}

fn nested_snapshot() -> StoredSnapshot {
    let c = Collector::new(def(Level::Tree), at(0));
    c.ingest(&span(1, None, S, 100 * MS, "handler"));
    c.ingest(&span(2, Some(1), S + 10 * MS, 60 * MS, "db;query"));
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    StoredSnapshot::from_view(
        "r".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        Some(TraceIngestLoss::default()),
        projected,
    )
}

#[test]
fn folded_emits_integer_microseconds_with_a_single_ascii_space() {
    // §9.8: speedscope matches ^(.*) (\d+)$ and DROPS a non-matching line
    // silently, while flamegraph.pl accepts decimals — so a decimal would
    // render in one tool and vanish without warning in the other.
    let d = folded_of(&nested_snapshot()).expect("rendered");
    assert_eq!(d.format, "folded");
    assert!(!d.content.is_empty());

    let re = regex::Regex::new(r"^(.*) (\d+)$").unwrap();
    for line in d.content.lines() {
        assert!(
            re.is_match(line),
            "speedscope would silently drop this line: {line:?}"
        );
        assert!(!line.contains('.'), "no decimals: {line:?}");
        let count = line.rsplit(' ').next().unwrap();
        assert!(count.parse::<u64>().is_ok(), "{line:?}");
    }

    // 40 ms of self time in `handler` is 40000 µs.
    assert!(
        d.content.lines().any(|l| l == "handler 40000"),
        "{}",
        d.content
    );
}

#[test]
fn a_semicolon_in_a_span_name_is_escaped_because_it_is_the_frame_separator() {
    let d = folded_of(&nested_snapshot()).unwrap();
    assert!(
        d.content.contains("handler;db,query"),
        "`;` in a name becomes `,`: {}",
        d.content
    );
    // And exactly one separator, so the stack is two frames deep and not three.
    let line = d
        .content
        .lines()
        .find(|l| l.starts_with("handler;db"))
        .unwrap();
    assert_eq!(line.split(';').count(), 2, "{line}");
}

#[test]
fn a_span_name_containing_the_display_separator_does_not_split_into_two_frames() {
    // The reason paths are stored as frames rather than as a joined string: a
    // span named `parse > eval` is indistinguishable from two frames once ` > `
    // is the separator, and splitting the joined form back apart would emit a
    // stack that never ran.
    let c = Collector::new(def(Level::Tree), at(0));
    c.ingest(&span(1, None, S, 50 * MS, "parse > eval"));
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    let s = StoredSnapshot::from_view(
        "r".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        Some(TraceIngestLoss::default()),
        projected,
    );
    assert_eq!(s.paths[0].frames, vec!["parse > eval"], "one frame");

    let d = folded_of(&s).unwrap();
    let line = d.content.lines().next().unwrap();
    assert_eq!(line.split(';').count(), 1, "still one frame: {line}");
    assert!(line.starts_with("parse > eval "), "{line}");
}

#[test]
fn folded_below_tree_is_refused_with_the_reason_and_a_remedy() {
    // §11 lists this as an error. It is not a silent empty file.
    let c = Collector::new(def(Level::Timing), at(0));
    c.ingest(&span(1, None, S, 10 * MS, "op"));
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    let s = StoredSnapshot::from_view(
        "r".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        None,
        projected,
    );
    let err = folded_of(&s).expect_err("refused");
    let msg = err.to_string();
    assert!(msg.contains("level `timing`"), "{msg}");
    assert!(msg.contains("Re-run at level `tree`"), "{msg}");
    assert!(msg.contains("--format md"), "and the alternative: {msg}");
}

#[test]
fn folded_of_several_arms_is_refused_because_a_flame_graph_of_a_difference_is_not_one() {
    let err = document(
        &[arm("a", 5, 10, "reconcile"), arm("b", 5, 10, "reconcile")],
        &DocumentOptions {
            format: Format::Folded,
            ..Default::default()
        },
        at(120),
    )
    .expect_err("refused");
    assert!(err.to_string().contains("one arm at a time"), "{err}");
}

#[test]
fn folded_of_a_merged_arm_is_refused_because_paths_do_not_merge() {
    let runs: Vec<StoredSnapshot> = [10, 10]
        .iter()
        .enumerate()
        .map(|(i, ms)| stored(&format!("r{i}"), 5, *ms, serde_json::json!({})))
        .collect();
    let err = document(
        &[Arm::merged("cache@*".into(), &runs).unwrap()],
        &DocumentOptions {
            format: Format::Folded,
            ..Default::default()
        },
        at(120),
    )
    .expect_err("refused");
    assert!(err.to_string().contains("two measurements"), "{err}");
}

#[test]
fn the_folded_sidecar_carries_the_invocation_because_the_format_has_no_unit() {
    let d = folded_of(&nested_snapshot()).unwrap();
    let side = d.sidecar.expect("sidecar");
    assert!(side.name.ends_with(".folded.meta.json"));
    assert!(side
        .content
        .contains("flamegraph.pl --countname us --nametype Span"));
    assert!(side.content.contains("microseconds"));
    assert!(side.content.contains("max_num_bins"), "layout identity too");
}

#[test]
fn an_incomplete_path_becomes_a_literal_root_frame() {
    // §9.8: `[?]` renders as a literal root frame, so the unresolved prefix is
    // visible in the graph rather than attributed to whatever comes next.
    let c = Collector::new(def(Level::Tree), at(0));
    // A parent that was never matched: the walk stops and the chain is a suffix.
    c.ingest(&span(2, Some(999), S, 30 * MS, "orphan"));
    let view = c.swap(at(60));
    let projected = crate::collector::project::project_for_snapshot(&view);
    assert!(projected.paths[0].incomplete);

    let s = StoredSnapshot::from_view(
        "r".into(),
        None,
        serde_json::Value::Null,
        at(60),
        SnapshotPolicy::default(),
        view,
        Some(TraceIngestLoss::default()),
        projected,
    );
    let d = folded_of(&s).unwrap();
    assert!(d.content.starts_with("[?];orphan "), "{:?}", d.content);
    assert!(d.sidecar.unwrap().content.contains("root_frame_[?]"));
}

#[test]
fn a_truncated_path_list_warns_because_a_flame_graph_hides_missing_mass() {
    let mut s = nested_snapshot();
    s.paths_truncated = true;
    let d = folded_of(&s).unwrap();
    assert!(
        d.warnings.iter().any(|w| w.contains("missing mass")),
        "{:?}",
        d.warnings
    );
}

#[test]
fn documenting_no_arms_is_an_error() {
    assert!(document(&[], &DocumentOptions::default(), at(120)).is_err());
}

#[test]
fn filter_intent_comes_from_the_collector_description_not_the_runs() {
    // Two arms of one collector share a filter, so `filter_intent` must not read
    // `varies`. The bug was reading the SNAPSHOT description ("what this run
    // was") where the COLLECTOR description ("what the filter captures")
    // belongs — the two are different fields with different jobs.
    let a = Arm::from_snapshot("cache@a".into(), &stored("a", 5, 10, serde_json::json!({})));
    let b = Arm::from_snapshot("cache@b".into(), &stored("b", 5, 10, serde_json::json!({})));
    assert_ne!(
        a.description, b.description,
        "the runs describe themselves differently, which is the trap"
    );
    assert_eq!(
        a.def.description, b.def.description,
        "but they share one filter intent"
    );

    let doc = md(&[a, b], &DocumentOptions::default());
    assert!(
        doc.contains("filter_intent: \"the read-through cache\""),
        "{}",
        doc.split("---").nth(1).unwrap()
    );
    assert!(!doc.contains("filter_intent: \"varies\""));
}

#[test]
fn an_explicit_filter_intent_overrides_the_description() {
    let doc = md(
        &[arm("a", 5, 10, "reconcile")],
        &DocumentOptions {
            filter_intent: Some("only the cold path".into()),
            ..Default::default()
        },
    );
    assert!(
        doc.contains("filter_intent: \"only the cold path\""),
        "{doc}"
    );
}

#[test]
fn the_nesting_remedy_adapts_when_there_is_no_self_ms_to_quote() {
    // The unactionable-remedy defect class again: "quote `self_ms`" is useless
    // in a document whose arms have none, and merged arms never do. A remedy
    // the reader cannot follow teaches them to skip the ones they can.
    let nested = |label: &str, child_ms: i64| {
        let c = Collector::new(def(Level::Tree), at(0));
        for i in 0..5i64 {
            let base = S + i * 200 * MS;
            let root = i as u64 * 2 + 1;
            c.ingest(&span(root, None, base, 100 * MS, "outer"));
            c.ingest(&span(
                root + 1,
                Some(root),
                base + 5 * MS,
                child_ms * MS,
                "inner",
            ));
        }
        let view = c.swap(at(60));
        let projected = crate::collector::project::project_for_snapshot(&view);
        StoredSnapshot::from_view(
            label.into(),
            None,
            serde_json::Value::Null,
            at(60),
            SnapshotPolicy::default(),
            view,
            Some(TraceIngestLoss::default()),
            projected,
        )
    };

    // A single-run arm HAS self time, so the remedy is the direct one.
    let one = Arm::from_snapshot("cache@r".into(), &nested("r", 50));
    assert_eq!(one.nesting, "detected");
    let doc = md(&[one], &DocumentOptions::default());
    assert!(doc.contains("**Quote `self_ms`**"), "{doc}");
    assert!(!doc.contains("no `self_ms` to quote"));

    // Merged arms do NOT, so the remedy has to change.
    let runs: Vec<StoredSnapshot> = (0..3).map(|i| nested(&format!("r{i}"), 50 + i)).collect();
    let merged = Arm::merged("cache@*".into(), &runs).unwrap();
    assert_eq!(merged.nesting, "detected", "the verdict survives the merge");
    assert!(merged.projections.is_none(), "but the figure does not");

    let doc = md(&[merged], &DocumentOptions::default());
    assert!(
        doc.contains("**this document has no `self_ms` to quote instead**"),
        "{doc}"
    );
    assert!(
        doc.contains("Document a single run"),
        "and it says how to get one: {doc}"
    );
    assert!(!doc.contains("**Quote `self_ms`** for anything"));
}

#[test]
fn a_suppressed_family_of_rows_is_reported_rather_than_silently_missing() {
    // `DiffResult.suppressed` says a whole family could not be produced. A
    // reader who notices `self_ms` is absent from the table has no other way to
    // learn whether it was zero, unmeasurable, or never asked for.
    let runs: Vec<StoredSnapshot> = (0..2)
        .map(|i| stored(&format!("r{i}"), 5, 10, serde_json::json!({})))
        .collect();
    let doc = md(
        &[
            Arm::merged("cache@*".into(), &runs).unwrap(),
            arm("b", 5, 10, "reconcile"),
        ],
        &DocumentOptions::default(),
    );
    assert!(
        doc.contains("**`sampled` not available.**"),
        "the suppression is rendered: {doc}"
    );
    // And it names the real reason — merging — rather than the generic one.
    assert!(doc.contains("merges several runs"), "{doc}");
    assert!(
        !doc.contains("projections disabled"),
        "which is not what happened here: {doc}"
    );
}

#[test]
fn a_count_row_is_thresholded_by_the_count_spread_not_the_duration_spread() {
    // Variance is per-metric. A suite with a fixed iteration count has near-zero
    // count variance while its timings vary by percent, so transferring the
    // duration CV onto a count row would strike out a real five-span difference
    // in three hundred and report it as noise.
    let runs = |n: usize, ms: [i64; 3]| -> Vec<StoredSnapshot> {
        ms.iter()
            .enumerate()
            .map(|(i, m)| stored(&format!("r{i}"), n, *m, serde_json::json!({})))
            .collect()
    };
    // Identical counts across runs, varying durations: count CV is 0, total CV
    // is not.
    let a = Arm::merged("a@*".into(), &runs(100, [10, 12, 14])).unwrap();
    let b = Arm::merged("b@*".into(), &runs(105, [10, 12, 14])).unwrap();
    assert_eq!(
        a.count_floor.as_ref().unwrap().cv_pct,
        Some(0.0),
        "a fixed iteration count has no spread"
    );
    assert!(
        a.floor.as_ref().unwrap().cv_pct.unwrap() > 1.0,
        "but timings do"
    );

    let d = crate::collector::diff::diff(a, b, &crate::collector::diff::DiffOptions::default())
        .unwrap();
    let count = d
        .rows
        .iter()
        .find(|r| r.metric == "count" && r.tier == crate::collector::diff::Tier::Exact)
        .unwrap();
    assert_eq!(
        count.threshold_pct,
        Some(0.0),
        "the COUNT floor, not the duration one"
    );
    assert!(
        !count.suppressed,
        "so a 5% population difference is reported, not struck out"
    );
}
