//! Rendering the case document — spec §5.2.
//!
//! A pure function over already-gathered facts: no I/O, no querying, no clock.
//! Everything it needs arrives in [`CaseInput`], so the ordering rules below can
//! be asserted without standing a daemon up.
//!
//! # The order, and why it departs from the house order
//!
//! 1. **Headline** — the anchor entry's own message. It must identify *this*
//!    incident; a headline that cannot tell two documents apart is not an index.
//! 2. **Evidence** — the §4.2 verdict, the span line, provenance with ages.
//!    Before anything it qualifies: a caveat reached after 400 lines has already
//!    failed.
//! 3. **What to do next.** `collector::document`'s house order has this at
//!    position 2, and this departs deliberately, because §4.2's verdict can
//!    invalidate the guess and a reader who acts before reading it has been
//!    misled by layout.
//! 4. Anchor entry and neighbours.
//! 5. Provenance — the registry copy, each key with its age. The **only** place
//!    the registry is rendered in full.
//! 6. Collector state.
//! 7. Pointers to the logdata files.
//!
//! # Ages, never verdicts
//!
//! *"last validated 34 days ago"* is a fact; *"provenance is current"* is a
//! judgement logmon cannot make — `/Action` set three minutes ago is already
//! wrong if the action changed two minutes ago. The one exception is a key that
//! carries its own `ttl`, where the caller stated the lifetime and the document
//! only reports whether it has elapsed.

use super::FORMAT_VERSION;
use crate::collector::document::yaml_str;
use crate::domain_data::{render_duration, ScopedFact, CORE_KEYS};
use crate::gelf::message::{LogEntry, LogSource};
use crate::span::types::SpanEntry;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::{EvidenceVerdict, NarrowedRange};
use std::fmt::Write;

/// The rendered registry's budget. Past it, keys are rendered newest-validated
/// first and the remainder becomes a count and a pointer — §5.2's own rule that
/// bulk *moves* rather than being cut in silence.
///
/// It exists because §3.1 permits 256 keys × 4 KiB and the document renders
/// them: nothing else stops a triage surface being three times the evidence it
/// points at, in a document whose whole job is to be read first.
pub const REGISTRY_RENDER_CAP: usize = 64 * 1024;

/// Neighbours shown either side of the anchor.
///
/// Load-bearing at this size: with only the anchor every reader must open the
/// logdata to learn anything and the document stops being a triage surface;
/// with hundreds, the caveats at the top get scrolled past — which the section
/// ordering exists to prevent.
pub const NEIGHBOUR_WINDOW: usize = 10;

/// Slowest spans listed in the timing summary.
pub const SLOW_SPAN_LINES: usize = 5;

/// How the anchor was named, and what it resolved to.
#[derive(Debug, Clone)]
pub struct Anchor {
    /// `seq`, `bookmark` or `trace_id` — tagged rather than sniffed, because
    /// sniffing misfires on a bookmark named `12345` and the failure is silent.
    pub kind: &'static str,
    /// The value as the caller gave it.
    pub label: String,
    pub seq: u64,
    pub at: DateTime<Utc>,
    /// The anchor entry's own message — the headline.
    pub message: String,
    /// Set when the anchor matched several entries and the earliest by seq was
    /// taken. The headline says so rather than picking silently.
    pub of_many: Option<usize>,
}

/// The window the capture covers, and everything known about its completeness.
#[derive(Debug, Clone)]
pub struct Window {
    pub from: u64,
    pub to: u64,
    pub verdict: EvidenceVerdict,
    pub narrowed_by: Vec<NarrowedRange>,
    /// The caller's `before`/`after` exceeded the maximum and were cut down to
    /// it.
    ///
    /// **Not `capped`.** Nothing inside `[from, to]` was omitted — the window
    /// is narrower than requested, not incomplete — and calling it capped put
    /// "nothing was capped" and "the read was **capped**" four lines apart in
    /// one document, which is the wrong-information failure this whole feature
    /// exists to prevent.
    pub clamped: bool,
    /// Requested-but-absent records below and above the window: how much of
    /// what the CALLER asked for did not come back. A request fact, bounded by
    /// `before`/`after` and by nothing else.
    pub short_before: usize,
    pub short_after: usize,
    /// The log store's floor — the lowest seq it can still speak for, `0` when
    /// nothing has ever left it. A STORE fact.
    ///
    /// **A seq, not a count, and the two must never be multiplied into each
    /// other.** `from` is by construction the first SURVIVING record, so
    /// `log_lost_below - from` is zero here and `short_before` is the only
    /// number in reach — but it is bounded by `before`, not by what the ring
    /// dropped. Rendering it as records-lost made a domain that had ever held
    /// 32 records report 71 of them gone.
    ///
    /// The span side below CAN carry a count, and the asymmetry is real rather
    /// than an oversight: the window's ends come from the LOG store, so a span
    /// floor above `from` measures a genuine stretch of the window the span
    /// ring no longer covers.
    pub log_lost_below: u64,
    /// The SPAN ring's own eviction, as an upper bound on spans gone from
    /// inside `[from, to]`. Separate because the two stores share a seq axis
    /// but evict independently, and one verdict cannot honestly speak for both.
    pub spans_evicted_before_window: Option<u64>,
}

/// A logdata file, as the document points at it.
#[derive(Debug, Clone)]
pub struct FilePointer {
    pub file: String,
    pub records: u64,
    /// How the records got stored. `None` for spandata, which has no such split.
    pub by_source: Option<SourceCounts>,
}

/// The split between records stored because they matched a **filter** and those
/// the flight recorder flushed as an **unfiltered window** around a trigger.
///
/// **Reported because the verdict cannot see it.** The pre-trigger flush calls
/// `append_to_store` directly (`daemon/log_processor.rs:90`) and never reaches
/// the `epochs().observe` on the storage-decision branch, so those seqs keep the
/// surrounding epoch's policy — a window holding a complete unfiltered burst
/// still grades `filtered`. That under-claims in the safe direction, but it
/// under-claims about the anchor's own neighbourhood, which is the part a reader
/// most needs to trust. These counts are the finer truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceCounts {
    pub filter: u64,
    pub pre_trigger: u64,
    pub post_trigger: u64,
}

impl SourceCounts {
    pub fn of(entries: &[LogEntry]) -> Self {
        let mut c = Self::default();
        for e in entries {
            match e.source {
                LogSource::Filter => c.filter += 1,
                LogSource::PreTrigger => c.pre_trigger += 1,
                LogSource::PostTrigger => c.post_trigger += 1,
            }
        }
        c
    }
}

/// One registry key as of `captured_at`.
///
/// **This is the wire format as well as the render input**, and deliberately one
/// type rather than two. The rendered table beneath §5 is lossy by design — its
/// values pass through a presentation escaper with no inverse and its timestamps
/// floor to the largest whole unit — so a reader that reconstructed facts from
/// it would silently round a 13-day-old fact to `1w` and flip a 7-day TTL from
/// elapsed to within. The machine block emitted beside it carries this struct
/// verbatim, and `Serialize`/`Deserialize` here is what keeps the two halves
/// from being two definitions that drift.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistryFact {
    pub path: String,
    pub value: String,
    pub created_at: DateTime<Utc>,
    pub validated_at: DateTime<Utc>,
    pub ttl_secs: Option<u64>,
    /// `None` when the key states no lifetime — age with no verdict.
    pub expired: Option<bool>,
}

/// One collector measuring this domain at capture.
#[derive(Debug, Clone)]
pub struct CollectorLine {
    pub name: String,
    /// Named because the selection is across owners: the CLI and the shim are
    /// different sessions on the same domain.
    pub owner: String,
    pub matched: u64,
    pub snapshots: usize,
    /// Label and instant of the most recent recorded run, if any.
    pub latest_snapshot: Option<(String, DateTime<Utc>)>,
    pub zeroed_by: Option<&'static str>,
}

/// Everything the renderer needs, gathered by the caller.
#[derive(Debug, Clone)]
pub struct CaseInput {
    pub stem: String,
    pub captured_at: DateTime<Utc>,
    pub domain: String,
    /// From `/logmon/incarnation`. §3.5: a domain name is not an identity, so a
    /// document records the era its seq range belongs to.
    pub incarnation: Option<String>,
    pub reason: String,
    pub anchor: Anchor,
    pub window: Window,
    pub logdata: FilePointer,
    pub spandata: FilePointer,
    pub registry: Vec<RegistryFact>,
    pub asserted: Vec<ScopedFact>,
    /// The anchor entry and its neighbours, oldest first, anchor included.
    pub neighbours: Vec<LogEntry>,
    pub spans: Vec<SpanEntry>,
    pub collectors: Vec<CollectorLine>,
}

/// A thing the caller should know that is not a failure — a capture gap, a
/// provenance observation, a write problem.
///
/// `kind` is what makes these actionable without parsing markdown; one prose
/// list multiplexing all three would not be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: &'static str,
    pub detail: String,
}

pub const NOTE_CAPTURE_GAP: &str = "capture_gap";
pub const NOTE_PROVENANCE: &str = "provenance";
pub const NOTE_TRUNCATED: &str = "truncated";

/// The document body and the notes gathered while rendering it.
pub struct Rendered {
    pub body: String,
    pub notes: Vec<Note>,
}

/// Core-key coverage: how many of §3.6.1's three are present, and **which are
/// missing**.
///
/// Named rather than counted, because "missing `/Build/commit`" is actionable
/// where "2 of 3" is not. Full coverage still prints the line rather than
/// omitting it: silence is indistinguishable from a document that never
/// checked.
fn core_coverage(registry: &[RegistryFact]) -> (usize, Vec<&'static str>) {
    let missing: Vec<&'static str> = CORE_KEYS
        .iter()
        .filter(|k| !registry.iter().any(|f| f.path == **k))
        .copied()
        .collect();
    (CORE_KEYS.len() - missing.len(), missing)
}

/// An age, in the largest whole unit — `13m`, `6h`, `34d`.
///
/// Deliberately NOT [`render_duration`], which renders the shortest *exact*
/// literal so a caller's stated `ttl` round-trips: an age of 13m23s comes back
/// from it as `803s`, which is the right answer to a different question. A
/// lifetime the caller wrote keeps the exact form; an age the document measured
/// gets this one.
fn approx_age(secs: u64) -> String {
    for (unit, mult) in [("w", 604_800u64), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs >= mult {
            return format!("{}{unit}", secs / mult);
        }
    }
    format!("{secs}s")
}

/// Negative ages — a key validated after the capture instant, which a clock
/// adjustment can produce — read as `0s` rather than as a negative duration.
fn age_of(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    approx_age((now - then).num_seconds().max(0) as u64)
}

/// Caller text on its way into the body, where nothing quotes it.
///
// Front-matter has `yaml_str`; the body has these. They moved to
// `crate::render::escape` when `_display` began rendering the same untrusted
// strings into the same markdown — one implementation, so a fix to either
// surface reaches both.
use crate::render::escape::{cell, flatten};

/// A code fence long enough to contain `content`.
///
/// A log message containing three backticks would otherwise close the block
/// early and spill the rest of the window into the prose.
fn fence_for(content: &str) -> String {
    let longest = content.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

pub fn render(input: &CaseInput) -> Rendered {
    let mut notes = Vec::new();
    let mut s = String::with_capacity(8 * 1024);
    front_matter(&mut s, input);
    headline(&mut s, input);
    evidence(&mut s, input, &mut notes);
    what_to_do(&mut s, input);
    neighbours(&mut s, input);
    provenance(&mut s, input, &mut notes);
    collectors(&mut s, input);
    pointers(&mut s, input);
    Rendered { body: s, notes }
}

// ---------------------------------------------------------------------------
// Front matter — the index surface: fixed-schema, small, grep-able
// ---------------------------------------------------------------------------

fn front_matter(s: &mut String, i: &CaseInput) {
    let (core, missing) = core_coverage(&i.registry);
    let _ = writeln!(s, "---");
    let _ = writeln!(s, "case: {}", i.stem);
    let _ = writeln!(s, "logmon_format: {FORMAT_VERSION}");
    let _ = writeln!(s, "captured_at: {}", yaml_str(&i.captured_at.to_rfc3339()));
    let _ = writeln!(s, "domain: {}", yaml_str(&i.domain));
    let _ = writeln!(
        s,
        "incarnation: {}",
        match &i.incarnation {
            Some(v) => yaml_str(v),
            None => "null".into(),
        }
    );
    let _ = writeln!(s, "reason: {}", yaml_str(&i.reason));
    let _ = writeln!(
        s,
        "anchor: {{kind: {}, value: {}, seq: {}, at: {}}}",
        i.anchor.kind,
        yaml_str(&i.anchor.label),
        i.anchor.seq,
        yaml_str(&i.anchor.at.to_rfc3339())
    );
    let _ = writeln!(s, "headline: {}", yaml_str(&i.anchor.message));
    let _ = writeln!(s, "verdict: {}", verdict_str(i.window.verdict));
    let _ = writeln!(
        s,
        "seq_range: {{from: {}, to: {}, requested_before_missing: {}, \
         requested_after_missing: {}, clamped: {}}}",
        i.window.from, i.window.to, i.window.short_before, i.window.short_after, i.window.clamped
    );
    // Evidence keys are OMITTED when the capture omitted that evidence, which is
    // a different fact from `records: 0` ("we captured none"). The key's presence
    // mirrors the file's, so the two cannot contradict each other.
    file_pointer(s, "logdata", &i.logdata);
    file_pointer(s, "spandata", &i.spandata);
    // Empty means nothing narrowed — a real answer. ABSENT means a case written
    // before this key existed, which a reader must not read as "unfiltered".
    let ranges: Vec<String> = i
        .window
        .narrowed_by
        .iter()
        .map(|n| {
            let fs: Vec<String> = n.filters.iter().map(|f| yaml_str(f)).collect();
            format!(
                "{{from: {}, to: {}, filters: [{}]}}",
                n.from_seq,
                n.to_seq,
                fs.join(", ")
            )
        })
        .collect();
    let _ = writeln!(s, "narrowed_by: [{}]", ranges.join(", "));
    let missing_yaml: Vec<String> = missing.iter().map(|k| yaml_str(k)).collect();
    let _ = writeln!(
        s,
        "provenance: {{core: \"{core} of {}\", missing: [{}]}}",
        CORE_KEYS.len(),
        missing_yaml.join(", ")
    );
    // The sigil is kept in the key, so a reader grepping the archive for
    // `/Data/seed` and one grepping for `@/Data/seed` are asking two different
    // questions and get two different answers.
    let asserted: Vec<String> = i
        .asserted
        .iter()
        .map(|f| format!("{}: {}", yaml_str(&f.key), yaml_str(&f.value)))
        .collect();
    let _ = writeln!(s, "asserted: {{{}}}", asserted.join(", "));
    let _ = writeln!(s, "---\n");
}

/// Emit one evidence pointer.
///
/// Separate from the front-matter body because the `omit_logdata` /
/// `omit_spandata` options will make this key absent rather than empty, and the
/// call site is where that decision belongs — not buried in a format string.
fn file_pointer(s: &mut String, key: &str, p: &FilePointer) {
    match p.by_source {
        Some(c) => {
            let _ = writeln!(
                s,
                "{key}: {{file: {}, records: {}, by_source: {{filter: {}, \
                 pre_trigger: {}, post_trigger: {}}}}}",
                yaml_str(&p.file),
                p.records,
                c.filter,
                c.pre_trigger,
                c.post_trigger
            );
        }
        None => {
            let _ = writeln!(
                s,
                "{key}: {{file: {}, records: {}}}",
                yaml_str(&p.file),
                p.records
            );
        }
    }
}

fn verdict_str(v: EvidenceVerdict) -> &'static str {
    match v {
        EvidenceVerdict::Complete => "complete",
        EvidenceVerdict::Evicted => "evicted",
        EvidenceVerdict::Filtered => "filtered",
        EvidenceVerdict::CannotVerify => "cannot_verify",
    }
}

// ---------------------------------------------------------------------------
// 1. Headline
// ---------------------------------------------------------------------------

fn headline(s: &mut String, i: &CaseInput) {
    let _ = writeln!(s, "# {}\n", flatten(&i.anchor.message));
    // Numbered, because the body's other headings are — a document that opens
    // at "## 2." reads as one whose first section was lost.
    let _ = writeln!(s, "## 1. What this is\n");
    let _ = writeln!(
        s,
        "A **case**: one window of a running system, frozen to disk by `logmon`, a log and \
         trace broker. It was captured by a person or an agent who thought it worth keeping; \
         the reason they gave is below. Nothing here is a diagnosis.\n"
    );
    let _ = writeln!(
        s,
        "`{}` on **domain** `{}` — a logmon domain is one isolated stream of logs and traces, \
         normally one per project or test run — at {}, because: {}\n",
        i.stem,
        flatten(&i.domain),
        i.anchor.at.to_rfc3339(),
        flatten(&i.reason)
    );
    let _ = writeln!(
        s,
        "A **seq** is a per-domain counter stamped on every record as it arrives, and it is \
         the only ordering this archive has — timestamps can tie. One counter numbers both \
         logs and spans, so consecutive seqs are not consecutive log records. The \
         **incarnation** is which run of the daemon assigned them: seqs are comparable only \
         within one, which is why it is in the front matter.\n"
    );
    if let Some(n) = i.anchor.of_many {
        let _ = writeln!(
            s,
            "The anchor `{}` matched {n} entries; the earliest by seq ({}) was taken.\n",
            flatten(&i.anchor.label),
            i.anchor.seq
        );
    }
    // Both instants, because a case written after the fact is about the
    // incident and not about now — and every age below is measured from the
    // capture, not from the moment being investigated.
    let lag = (i.captured_at - i.anchor.at).num_seconds();
    if lag > 0 {
        let _ = writeln!(
            s,
            "Captured at {}, {} after the anchor.\n",
            i.captured_at.to_rfc3339(),
            approx_age(lag as u64)
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Evidence — before anything it qualifies
// ---------------------------------------------------------------------------

fn evidence(s: &mut String, i: &CaseInput, notes: &mut Vec<Note>) {
    let _ = writeln!(
        s,
        "## 2. Evidence — what this capture can and cannot show\n"
    );
    // The verdict grades the CAPTURE, not the incident. In front matter it sits
    // under `headline:` in a field called `verdict`, which everywhere else means
    // the finding — so the axis is established here, before the value.
    let _ = writeln!(
        s,
        "The verdict below grades **how much of this window logmon can vouch for**. It is not \
         a finding about the incident. Storage is conditional — a filter held by any session \
         on this domain narrows what gets kept — so it exists to tell *absence of cause* from \
         *absence of recording*.\n"
    );
    let w = &i.window;
    let span = w.to.saturating_sub(w.from).saturating_add(1);

    match w.verdict {
        EvidenceVerdict::Complete => {
            let _ = writeln!(
                s,
                "**`complete`** — seqs {}–{} lay wholly inside one unfiltered epoch, nothing \
                 was evicted from it. Everything the daemon stored over these seqs is in the \
                 logdata.\n\n\
                 Fewer records than seqs is normal and not a gap: one counter numbers both \
                 logs and spans, so a span consumes a seq no log will ever occupy.\n",
                w.from, w.to
            );
        }
        EvidenceVerdict::Filtered => {
            for n in &w.narrowed_by {
                let affected = n.to_seq.saturating_sub(n.from_seq).saturating_add(1);
                let _ = writeln!(
                    s,
                    "**`filtered`** — a session filter held {} over seqs {}–{}, so {affected} \
                     of this window's {span} seqs were stored matches-only. \"Nothing appeared \
                     before this\" is not a supported conclusion over that range.\n",
                    n.filters
                        .iter()
                        .map(|f| format!("`{f}`"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                    n.from_seq,
                    n.to_seq
                );
                notes.push(Note {
                    kind: NOTE_CAPTURE_GAP,
                    detail: format!(
                        "seqs {}–{} were stored matches-only, under {}",
                        n.from_seq,
                        n.to_seq,
                        n.filters.join(", ")
                    ),
                });
            }
        }
        EvidenceVerdict::Evicted => {
            let _ = writeln!(
                s,
                "**`evicted`** — this window reaches the bottom of what the log ring still \
                 holds. It starts at seq {}, the ring has dropped everything below that, and \
                 records the capture asked for are **gone**. How many is not knowable from \
                 here: a ring keeps no tally of what it evicted, and the shortfall stated \
                 below is bounded by what was REQUESTED, not by what was lost.\n",
                w.from
            );
            notes.push(Note {
                kind: NOTE_CAPTURE_GAP,
                detail: format!(
                    "the log ring has dropped everything below seq {}; {} requested record(s) \
                     before the anchor did not come back",
                    w.log_lost_below, w.short_before
                ),
            });
        }
        EvidenceVerdict::CannotVerify => {
            let _ = writeln!(
                s,
                "**`cannot_verify`** — no claim can be made about seqs {}–{}. The store is \
                 empty, or the window predates what this daemon records about its own storage \
                 policy (including a window carried over from a previous run), or the read was \
                 capped short of what matched.\n",
                w.from, w.to
            );
            notes.push(Note {
                kind: NOTE_CAPTURE_GAP,
                detail: format!(
                    "no completeness claim is possible for seqs {}–{}",
                    w.from, w.to
                ),
            });
        }
    }

    // The window can be narrower than the caller asked for, and the reasons are
    // not one fact but three: records that LEFT are a gap, a domain that never
    // had that much history is not, and neither of those is what a shortfall
    // ABOVE the anchor means. Each end gets its own sentence, because a single
    // cause line gated on `before || after` explained the bottom while the
    // shortfall was at the top.
    if w.short_before > 0 || w.short_after > 0 {
        let _ = writeln!(
            s,
            "The window is **narrower than requested**: {} record(s) short before the anchor \
             and {} after.\n",
            w.short_before, w.short_after
        );
    }
    if w.short_before > 0 {
        let _ = if w.log_lost_below > 0 {
            writeln!(
                s,
                "Below the window: the log ring has dropped everything under seq {}, so some \
                 share of those {} missing record(s) are **gone** rather than never recorded. \
                 The split is not recoverable — the ring counts what it holds, not what it \
                 dropped.\n",
                w.log_lost_below, w.short_before
            )
        } else {
            writeln!(
                s,
                "Below the window: seq {} is the oldest record this store has ever held and it \
                 has dropped nothing, so the shortfall before the anchor is an empty past \
                 rather than a loss.\n",
                w.from
            )
        };
    }
    if w.short_after > 0 {
        let _ = writeln!(
            s,
            "After the window: seq {} was the newest record stored when the capture was taken, \
             so the shortfall after the anchor is history that had not happened yet. A ring \
             evicts from the bottom, never the top — nothing was lost here.\n",
            w.to
        );
    }
    if w.clamped {
        let _ = writeln!(
            s,
            "`before`/`after` were **clamped to the maximum**, so this window is as wide as a \
             capture can be. What lies beyond it was not read, and is neither present nor \
             ruled out.\n"
        );
    }

    // The verdict above grades the seq axis, and the axis cannot see this: the
    // flight recorder's flush stores entries without the epoch log observing
    // them, so a window holding a complete unfiltered burst still grades
    // `filtered`. Saying so here is what stops the verdict being read as the
    // whole truth about the anchor's own neighbourhood.
    if let Some(c) = i.logdata.by_source {
        let unfiltered = c.pre_trigger + c.post_trigger;
        if unfiltered > 0 {
            let _ = writeln!(
                s,
                "Of these {} record(s), **{} were captured unfiltered** — the flight \
                 recorder's window around the trigger ({} before, {} after) — and {} \
                 were stored because they matched a filter. Any verdict of \
                 `filtered` above grades the seq range as a whole and does not \
                 know about the unfiltered window inside it.\n",
                i.logdata.records, unfiltered, c.pre_trigger, c.post_trigger, c.filter
            );
        }
    }

    // The span line is its own, because the span ring has its own retention and
    // one verdict cannot honestly cover two stores that evict independently.
    match w.spans_evicted_before_window {
        None => {
            let _ = writeln!(
                s,
                "Spans: {} captured, and the span ring had dropped nothing below seq {}. \
                 **Session filters never narrow spans** — they are stored unconditionally — so \
                 whatever the verdict above says about the logs, the spans over this range are \
                 all of them.\n",
                i.spandata.records, w.from
            );
        }
        Some(gap) => {
            let _ = writeln!(
                s,
                "Spans: {} captured; the span ring **had** evicted below seq {} — up to {gap} \
                 spans over this window are gone. The log verdict above does not cover this: \
                 the two stores share a seq axis but evict independently.\n",
                i.spandata.records, w.from
            );
            notes.push(Note {
                kind: NOTE_CAPTURE_GAP,
                detail: format!(
                    "the span ring evicted below seq {}; up to {gap} spans are gone",
                    w.from
                ),
            });
        }
    }

    // Provenance summary — the full copy is in §5, this is the qualifier.
    let (core, missing) = core_coverage(&i.registry);
    let mut line = if missing.is_empty() {
        format!(
            "Provenance: **{core} of {} core keys**, all present.",
            CORE_KEYS.len()
        )
    } else {
        notes.push(Note {
            kind: NOTE_PROVENANCE,
            detail: format!("missing core keys: {}", missing.join(", ")),
        });
        format!(
            "Provenance: **{core} of {} core keys** — missing {}.",
            CORE_KEYS.len(),
            missing
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    // Core and contextual are listed separately, and contextual keys are NAMED
    // rather than counted: `/Versions/<component>` is an open family, and a
    // bare count is not comparable between two documents.
    let describe = |f: &RegistryFact| {
        let age = age_of(i.captured_at, f.validated_at);
        match (f.ttl_secs, f.expired) {
            (Some(secs), Some(true)) => format!(
                "`{}` ({age}, past its stated {} lifetime)",
                f.path,
                render_duration(secs)
            ),
            (Some(secs), _) => format!(
                "`{}` ({age}, within its stated {} lifetime)",
                f.path,
                render_duration(secs)
            ),
            // No `ttl`, no verdict — an age and nothing more.
            (None, _) => format!("`{}` ({age})", f.path),
        }
    };
    let (present, contextual): (Vec<&RegistryFact>, Vec<&RegistryFact>) = i
        .registry
        .iter()
        .partition(|f| CORE_KEYS.contains(&f.path.as_str()));
    if !present.is_empty() {
        line.push_str(&format!(
            " Present: {}.",
            present
                .iter()
                .map(|f| describe(f))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !contextual.is_empty() {
        line.push_str(&format!(
            " Contextual: {}.",
            contextual
                .iter()
                .map(|f| describe(f))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if i.registry.is_empty() {
        // Absence must not read as validation.
        line.push_str(
            " The domain registry is **empty**: nothing at all was recorded about what produced \
             these logs. That is said rather than omitted, because an omitted section reads as \
             \"nothing interesting\".",
        );
    }
    let _ = writeln!(s, "{line}\n");

    if i.asserted.is_empty() {
        let _ = writeln!(s, "Nothing was asserted for this case alone.\n");
    } else {
        let lag = (i.captured_at - i.anchor.at).num_seconds().max(0) as u64;
        let _ = writeln!(
            s,
            "Asserted for this case, not validated — recorded at capture, {} after the anchor:\n",
            approx_age(lag)
        );
        for f in &i.asserted {
            let _ = writeln!(s, "- `{}`: {}", flatten(&f.key), flatten(&f.value));
        }
        let _ = writeln!(s);
    }
}

// ---------------------------------------------------------------------------
// 3. What to do next — derived, and deliberately after the verdict
// ---------------------------------------------------------------------------

/// What limits this capture, and what would clear it.
///
/// Derived rather than caller prose: §5.1 mints no parameter for a capturer's
/// own suggestion, and `collector::document`'s house version of this section is
/// limitations-and-remedies too. A capturer with something to say has the `@`
/// sigil, which lands in Evidence above where it is qualified alongside
/// everything else.
fn what_to_do(s: &mut String, i: &CaseInput) {
    let _ = writeln!(s, "## 3. What to do next\n");
    let mut any = false;
    let mut item = |s: &mut String, text: String| {
        any = true;
        let _ = writeln!(s, "- {text}");
    };

    match i.window.verdict {
        EvidenceVerdict::Filtered => {
            let filters: Vec<String> = i
                .window
                .narrowed_by
                .iter()
                .flat_map(|n| n.filters.iter())
                .map(|f| format!("`{f}`"))
                .collect();
            item(
                s,
                format!(
                    "**The window was narrowed by {}.** Re-run with those filters removed \
                     (`remove_filter`) if you need to know what did *not* match — no later \
                     read can recover what was never stored.",
                    filters.join(", ")
                ),
            );
        }
        EvidenceVerdict::Evicted => item(
            s,
            "**The ring had already dropped part of this window.** Raise the domain's \
             `log_buffer_size`, or capture closer to the event."
                .into(),
        ),
        EvidenceVerdict::CannotVerify => item(
            s,
            "**No completeness claim was possible.** Check whether the store was cleared, \
             whether the daemon restarted inside this window, and whether the read was capped."
                .into(),
        ),
        EvidenceVerdict::Complete => {}
    }
    if i.window.short_before > 0 && i.window.log_lost_below > 0 {
        item(
            s,
            "**Records below this window had already been dropped.** Raise the domain's \
             `log_buffer_size`, or capture sooner after the event — nothing can recover them \
             now."
                .into(),
        );
    } else if i.window.clamped {
        item(
            s,
            "**`before`/`after` hit the maximum.** A larger value will not widen this window; \
             capture a second case anchored further out if you need more."
                .into(),
        );
    }
    let (_, missing) = core_coverage(&i.registry);
    if !missing.is_empty() {
        item(
            s,
            format!(
                "**Record {}** with `update_domain_data`, so the next case on this domain can \
                 say what produced it.",
                missing
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        );
    }
    if i.collectors.is_empty() {
        item(
            s,
            format!(
                "**No collector was armed on `{}`.** Arming one before the next run turns \
                 \"it felt slow\" into a distribution.",
                i.domain
            ),
        );
    }
    if !any {
        let _ = writeln!(
            s,
            "Nothing limits this capture: the window is complete, the core provenance keys are \
             present, and a collector was measuring this domain. Read section 4, then the \
             logdata."
        );
    }
    let _ = writeln!(s);
}

// ---------------------------------------------------------------------------
// 4. Anchor entry and neighbours
// ---------------------------------------------------------------------------

fn neighbours(s: &mut String, i: &CaseInput) {
    let _ = writeln!(s, "## 4. Anchor entry and neighbours\n");
    // §2's caveat is repeated HERE because this is where the trap is. A reader
    // who scrolled past it now sees a tidy run of consecutive lines and a
    // pointer to "the full window", and both are true only of what was STORED.
    if !i.window.narrowed_by.is_empty() {
        let _ = writeln!(
            s,
            "**These are the entries that were stored, not the entries that occurred.** A \
             filter was narrowing this domain over part of this range (§2), so the run below \
             can look continuous while entries between these seqs were never recorded. The \
             logdata file holds the full window *as stored*, which is the same qualification.\n"
        );
    }
    let _ = writeln!(
        s,
        "The anchor is marked `>`. Columns are seq, timestamp, level, message. The full \
         stored window is in the logdata file (§7).\n"
    );
    if i.neighbours.is_empty() {
        let _ = writeln!(
            s,
            "No entries were retained around seq {} — the anchor itself is gone from the \
             store.\n",
            i.anchor.seq
        );
        return;
    }
    let mut block = String::new();
    for e in &i.neighbours {
        let mark = if e.seq == i.anchor.seq { '>' } else { ' ' };
        let _ = writeln!(
            block,
            "{mark} {:>8}  {}  {:<5}  {}",
            e.seq,
            e.timestamp.to_rfc3339(),
            format!("{:?}", e.level).to_uppercase(),
            flatten(&e.message)
        );
    }
    // Sized to the content: a log message containing three backticks would
    // otherwise close the block early and spill the window into the prose.
    let fence = fence_for(&block);
    let _ = writeln!(s, "{fence}");
    s.push_str(&block);
    let _ = writeln!(s, "{fence}\n");

    if !i.spans.is_empty() {
        let mut slowest: Vec<&SpanEntry> = i.spans.iter().collect();
        slowest.sort_by(|a, b| {
            b.duration_ms
                .partial_cmp(&a.duration_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let _ = writeln!(
            s,
            "Slowest spans over this window — full trees, attributes and events are in the \
             spandata file:\n"
        );
        let _ = writeln!(s, "| span | service | ms |");
        let _ = writeln!(s, "|---|---|---|");
        for sp in slowest.iter().take(SLOW_SPAN_LINES) {
            let _ = writeln!(
                s,
                "| `{}` | `{}` | {:.1} |",
                cell(&sp.name),
                cell(&sp.service_name),
                sp.duration_ms
            );
        }
        let _ = writeln!(s);
    }
}

// ---------------------------------------------------------------------------
// 5. Provenance — the registry copy, the only full rendering of it
// ---------------------------------------------------------------------------

fn provenance(s: &mut String, i: &CaseInput, notes: &mut Vec<Note>) {
    let _ = writeln!(s, "## 5. Provenance\n");
    if i.registry.is_empty() {
        let _ = writeln!(
            s,
            "The registry for domain `{}` was **empty at capture**. Nothing here was recorded \
             about the build, the action under test, or the environment.\n",
            i.domain
        );
        return;
    }
    let _ = writeln!(
        s,
        "As of {}. Ages are since last validation; a key with no stated lifetime carries an \
         age and no verdict.\n",
        i.captured_at.to_rfc3339()
    );

    // Newest-validated first, so the budget below cuts the least useful end.
    let mut facts: Vec<&RegistryFact> = i.registry.iter().collect();
    facts.sort_by_key(|f| std::cmp::Reverse(f.validated_at));

    let _ = writeln!(
        s,
        "**core** marks the three keys a case cannot be acted on without — `{}` — graded in \
         the front matter's `provenance` line. Everything else is contextual and is named \
         rather than counted, since it is an open set.\n",
        CORE_KEYS.join("`, `")
    );
    let header =
        "| key | kind | value | age | in force since | lifetime |\n|---|---|---|---|---|---|\n";
    s.push_str(header);
    // The budget governs the TABLE, which is the document's own size risk: §3.1
    // permits 1.1 MB of registry and this renders it. The machine-readable
    // registry is a separate artifact and carries every fact regardless, so
    // truncating here costs a reader nothing but scrolling — the note below says
    // where the rest is, and it travels with the case.
    let mut used = 0usize;
    let mut rendered = 0usize;
    for f in &facts {
        let lifetime = match (f.ttl_secs, f.expired) {
            (Some(secs), Some(true)) => format!("{} — **elapsed**", render_duration(secs)),
            (Some(secs), _) => format!("{} — within", render_duration(secs)),
            (None, _) => "—".to_string(),
        };
        let row = format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            cell(&f.path),
            if CORE_KEYS.contains(&f.path.as_str()) {
                "core"
            } else {
                "contextual"
            },
            cell(&f.value),
            age_of(i.captured_at, f.validated_at),
            age_of(i.captured_at, f.created_at),
            lifetime
        );
        if used + row.len() > REGISTRY_RENDER_CAP {
            break;
        }
        used += row.len();
        s.push_str(&row);
        rendered += 1;
    }
    let _ = writeln!(s);
    if rendered < facts.len() {
        let dropped = facts.len() - rendered;
        let _ = writeln!(
            s,
            "{dropped} further key(s) were not rendered here: this document's registry \
             budget of {} KiB was reached. **Nothing was lost** — every fact, unrounded, \
             is in `{}`, which travels with this case. That file is the record; this \
             table is the reading.\n",
            REGISTRY_RENDER_CAP / 1024,
            super::bundle::registry_entry_name(&i.stem)
        );
        notes.push(Note {
            kind: NOTE_TRUNCATED,
            detail: format!(
                "{dropped} registry keys exceeded the document's render budget; \
                 all of them are in the registry file"
            ),
        });
    }
}

/// The registry as its own artifact — `<stem>.registry.json`, what `load_case`
/// restores from.
///
/// **The §5 table cannot serve this.** `cell()` runs values through `flatten()`,
/// which collapses newlines and trims, with no inverse; `age_of` floors
/// timestamps to the largest whole unit, so `created_at` and `validated_at`
/// survive only to the nearest day or week; and `ttl`/`expired` arrive as a
/// rendered verdict rather than a number and a bool. Restoring a registry from
/// that would produce provenance that looks exact and is not — the failure mode
/// a postmortem domain exists to avoid.
///
/// **It carries EVERY fact, and `dropped` is therefore always 0.** An earlier
/// draft put this inside the document and had to share the document's 64 KiB
/// budget, which capped what a loader could restore. Its own entry costs the
/// document nothing, so the cap goes back to governing only the human table —
/// and the table's truncation note points here rather than at a live domain
/// that the postmortem scenario guarantees is gone.
pub fn registry_json(registry: &[RegistryFact]) -> Vec<u8> {
    let payload = serde_json::json!({
        "logmon_format": FORMAT_VERSION,
        "facts": registry,
        "dropped": 0,
    });
    serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec())
}

// ---------------------------------------------------------------------------
// 6. Collector state at capture
// ---------------------------------------------------------------------------

fn collectors(s: &mut String, i: &CaseInput) {
    let _ = writeln!(s, "## 6. Collector state at capture\n");
    if i.collectors.is_empty() {
        // In these words: omission reads as "nothing interesting", and a
        // session-scoped wording would state something false about the domain.
        let _ = writeln!(
            s,
            "**No collectors were armed on domain `{}` at capture** — by this session or any \
             other. There are no measurements to go with these logs.\n",
            i.domain
        );
        return;
    }
    let _ = writeln!(
        s,
        "A **collector** is a standing measurement: a filter armed against this domain that \
         accumulates span timings until someone reads or resets it. Every one pinned to `{}` \
         is listed, whoever armed it — `armed by` is the session that did.\n\n\
         **`matched` counts the collector's whole live window, not this capture's seqs**, and \
         `latest run` is a recorded snapshot which may predate the incident entirely. Read \
         both against their own ages, not against the window above.\n",
        i.domain
    );
    let _ = writeln!(s, "| collector | armed by | matched | runs | latest run |");
    let _ = writeln!(s, "|---|---|---|---|---|");
    for c in &i.collectors {
        let latest = match &c.latest_snapshot {
            Some((label, at)) => format!("`{}`, {} ago", cell(label), age_of(i.captured_at, *at)),
            None => "—".into(),
        };
        let matched = match c.zeroed_by {
            Some(why) => format!("{} (window zeroed by {why})", c.matched),
            None => c.matched.to_string(),
        };
        let _ = writeln!(
            s,
            "| `{}` | `{}` | {} | {} | {} |",
            cell(&c.name),
            cell(&c.owner),
            matched,
            c.snapshots,
            latest
        );
    }
    let _ = writeln!(s);
}

// ---------------------------------------------------------------------------
// 7. The logdata files
// ---------------------------------------------------------------------------

fn pointers(s: &mut String, i: &CaseInput) {
    let _ = writeln!(s, "## 7. Evidence files\n");
    let _ = writeln!(s, "| file | records | first line |");
    let _ = writeln!(s, "|---|---|---|");
    let _ = writeln!(
        s,
        "| `{}` | {} | `{{\"logmon_format\":{FORMAT_VERSION},\"kind\":\"logdata\"}}` |",
        i.logdata.file, i.logdata.records
    );
    let _ = writeln!(
        s,
        "| `{}` | {} | `{{\"logmon_format\":{FORMAT_VERSION},\"kind\":\"spandata\"}}` |",
        i.spandata.file, i.spandata.records
    );
    // Printed in full, not elided. The sentence naming the header as the
    // contract is the last place to hide it behind an ellipsis: nothing indexes
    // this archive, so this line is what a reader years from now has.
    let _ = writeln!(
        s,
        "\nEach file is JSONL. **The first line is a header, not a record**, and it is the \
         format contract — `logmon_format` is the integer to branch on if the record shape \
         ever changes. Every line after it is one record, and `records` above counts those \
         alone.\n\n\
         Logdata records are log entries: `seq`, `timestamp`, `level`, `message`, `host`, and \
         optional `full_message`, `facility`, `file`, `line`, `trace_id`, `span_id`, plus a \
         `source` saying which rule stored the entry and `matched_filters` naming any filter \
         it matched. `additional_fields` is an object holding every custom field the sender \
         attached — a GELF `_request_id` arrives here as `request_id`, and it is where \
         anything application-specific will be. Spandata records are spans: `seq`, `trace_id`, `span_id`, \
         `parent_span_id`, `start_time`, `end_time`, `duration_ms`, `name`, `kind`, \
         `service_name`, `status`, `attributes`, `events`. Both are seq-ascending, and both \
         number their seqs from the same counter — which is why a span and a log never share \
         one.\n\n\
         A file with no records still exists, with its header: absent cannot be told from \
         \"we captured none\"."
    );
}

#[cfg(test)]
mod tests;
