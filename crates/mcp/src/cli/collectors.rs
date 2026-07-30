//! `collectors` subcommand group: add, list, get, reset, remove, profile.
//!
//! The human-readable rendering is deliberately opinionated: it leads with the
//! headline number, then names anything the result could not answer. A profile
//! that prints only the numbers it *has* reads as complete when it is not.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{
    CollectorsAdd, CollectorsDiff, CollectorsDiffResult, CollectorsDocument, CollectorsEdit,
    CollectorsGet, CollectorsHistory, CollectorsList, CollectorsName, CollectorsSnapshot, DiffRow,
    ProfileResult, TracesProfile,
};
use logmon_broker_sdk::Broker;

use super::format;

#[derive(Args, Debug)]
pub struct CollectorsCmd {
    #[command(subcommand)]
    verb: ColVerb,
}

#[derive(Subcommand, Debug)]
enum ColVerb {
    /// Arm a collector over a span filter.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        filter: String,
        /// scalar | timing | tree (default tree)
        #[arg(long)]
        level: Option<String>,
        /// Repeatable: --group-key cache.enabled
        #[arg(long = "group-key")]
        group_keys: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        max_sample_bytes: Option<u64>,
    },
    /// List this session's collectors.
    List,
    /// Read a collector's numbers.
    Get {
        #[arg(long)]
        name: String,
        /// Read a recorded run by label instead of the live window.
        #[arg(long)]
        snapshot: Option<String>,
        /// name | group | trace | path
        #[arg(long)]
        group_by: Option<String>,
        #[arg(long)]
        skip_warmup_ms: Option<f64>,
        #[arg(long)]
        top_n: Option<u64>,
    },
    /// Change an armed collector. Anything but --description discards the live
    /// window; recorded snapshots are never touched.
    Edit {
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        filter: Option<String>,
        /// scalar | timing | tree
        #[arg(long)]
        level: Option<String>,
        #[arg(long = "group-key")]
        group_keys: Vec<String>,
        #[arg(long)]
        max_sample_bytes: Option<u64>,
        /// Re-pin to another domain (only while zeroed).
        #[arg(long)]
        domain: Option<String>,
    },
    /// Record the current window as a named run and start the next one.
    Snapshot {
        #[arg(long)]
        name: String,
        #[arg(long)]
        label: Option<String>,
        /// What this run was. Recorded with the data — give one.
        #[arg(long)]
        description: Option<String>,
        /// Provenance as JSON, e.g. '{"commit":"abc123"}'.
        #[arg(long)]
        meta: Option<String>,
        /// Record without zeroing (default is to zero and start the next run).
        #[arg(long)]
        no_reset: bool,
    },
    /// List a collector's recorded runs, oldest first.
    History {
        #[arg(long)]
        name: String,
        #[arg(long)]
        limit: Option<u64>,
        /// Also combine them and report the run-to-run spread.
        #[arg(long)]
        merge: bool,
    },
    /// Compare two arms and report what moved.
    ///
    /// An arm is `<collector>` (live window), `<collector>@<label>` (one
    /// recorded run), or `<collector>@*` (every recorded run merged). Prefer
    /// `@*` on both sides: single-run arms have no spread, so every threshold
    /// comes back unknown and nothing can be called a result.
    Diff {
        /// Baseline arm.
        a: String,
        /// The arm to compare against it.
        b: String,
        /// Break the deltas down by `name` or `group`.
        #[arg(long)]
        group_by: Option<String>,
        #[arg(long)]
        top_n: Option<u64>,
        /// Compare arms whose filters matched different populations.
        #[arg(long)]
        allow_mismatch: bool,
        /// Compare when one arm lost spans and the other did not.
        #[arg(long)]
        allow_lossy: bool,
        /// Compare when both arms hit the sample budget.
        #[arg(long)]
        allow_truncated: bool,
    },
    /// Write up a measurement: what moved, what to do next, every caveat beside
    /// the number it qualifies.
    ///
    /// The first arm is the baseline. Pass --path to write the document (and its
    /// sidecar) to disk; otherwise it goes to stdout. Regeneration is free and
    /// lossless, so the normal shape is to read it, then regenerate with
    /// --finding filled in.
    Document {
        /// Arms to document; the first is the baseline.
        names: Vec<String>,
        /// md (default) | json | folded
        #[arg(long)]
        format: Option<String>,
        /// Where to write it. Omitted: stdout.
        #[arg(long)]
        path: Option<String>,
        /// What you were trying to find out.
        #[arg(long)]
        question: Option<String>,
        /// What you concluded. Usually supplied on a second run.
        #[arg(long)]
        finding: Option<String>,
        /// What the filter was meant to capture.
        #[arg(long)]
        filter_intent: Option<String>,
        /// Evidence the faster arm is still correct.
        #[arg(long)]
        correctness_evidence: Option<String>,
        /// name (default) | group
        #[arg(long)]
        group_by: Option<String>,
        #[arg(long)]
        top_n: Option<u64>,
        #[arg(long)]
        allow_mismatch: bool,
        #[arg(long)]
        allow_lossy: bool,
        #[arg(long)]
        allow_truncated: bool,
    },
    /// Zero a collector and start a fresh window, keeping it armed.
    Reset {
        #[arg(long)]
        name: String,
    },
    /// Remove a collector and release its sample budget.
    Remove {
        #[arg(long)]
        name: String,
    },
    /// Profile spans already in the buffer, without arming anything.
    Profile {
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        group_by: Option<String>,
        #[arg(long = "group-key")]
        group_keys: Vec<String>,
        #[arg(long)]
        skip_warmup_ms: Option<f64>,
        #[arg(long)]
        top_n: Option<u64>,
    },
}

pub async fn dispatch(broker: &Broker, cmd: CollectorsCmd, json: bool) -> i32 {
    match cmd.verb {
        ColVerb::Add {
            name,
            filter,
            level,
            group_keys,
            description,
            max_sample_bytes,
        } => {
            let result = match broker
                .collectors_add(CollectorsAdd {
                    name,
                    filter,
                    level,
                    group_keys: (!group_keys.is_empty()).then_some(group_keys),
                    description,
                    max_sample_bytes,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.add failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "armed {} at level {} on domain {} — {}",
                result.name, result.level, result.domain, result.filter
            );
            if !result.group_keys.is_empty() {
                println!("  grouped by: {}", result.group_keys.join(", "));
            }
            // Warnings print even in the success path, and prominently: every
            // one of them describes a filter that arms cleanly and then
            // collects nothing, or collects the wrong thing.
            for w in &result.warnings {
                println!("  warning: {w}");
            }
            0
        }

        ColVerb::List => {
            let result = match broker.collectors_list(CollectorsList {}).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.list failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.collectors.is_empty() {
                println!("(no collectors)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .collectors
                .iter()
                .map(|c| {
                    vec![
                        c.name.clone(),
                        c.level.clone(),
                        c.domain.clone(),
                        format!("{}", c.matched),
                        if c.sample_complete { "" } else { "truncated" }.to_string(),
                        c.filter.clone(),
                    ]
                })
                .collect();
            format::print_table(
                &["name", "level", "domain", "matched", "sample", "filter"],
                rows,
            );
            println!(
                "{} collector(s), {} bytes reserved",
                result.count, result.reserved_bytes
            );
            0
        }

        ColVerb::Edit {
            name,
            description,
            filter,
            level,
            group_keys,
            max_sample_bytes,
            domain,
        } => {
            let result = match broker
                .collectors_edit(CollectorsEdit {
                    name,
                    description,
                    filter,
                    level,
                    group_keys: (!group_keys.is_empty()).then_some(group_keys),
                    max_sample_bytes,
                    domain,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.edit failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "{} is now level {} on domain {} — {}",
                result.name, result.level, result.domain, result.filter
            );
            // Printed whether or not anything was discarded: "nothing was
            // discarded" is the reassurance a caller wants after an edit they
            // were not sure was structural.
            println!("  {}", result.note);
            for w in &result.warnings {
                println!("  warning: {w}");
            }
            0
        }

        ColVerb::Snapshot {
            name,
            label,
            description,
            meta,
            no_reset,
        } => {
            let meta = match meta
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
            {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => {
                    format::error(&format!("--meta is not valid JSON: {e}"), json);
                    return 1;
                }
                None => None,
            };
            let result = match broker
                .collectors_snapshot(CollectorsSnapshot {
                    name,
                    label,
                    description,
                    meta,
                    reset: Some(!no_reset),
                    per_name: None,
                    per_group: None,
                    projections: None,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.snapshot failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "recorded `{}`: {} spans, {:.1}ms total over {:.1}s{}",
                result.label,
                result.exact.count,
                result.exact.total_ms,
                result.wall_ms / 1000.0,
                if result.sample_complete {
                    ""
                } else {
                    "  (SAMPLE TRUNCATED)"
                },
            );
            if let Some(d) = &result.description {
                println!("  {d}");
            }
            0
        }

        ColVerb::History { name, limit, merge } => {
            let result = match broker
                .collectors_history(CollectorsHistory {
                    name,
                    limit,
                    merge: Some(merge),
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.history failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            if result.snapshots.is_empty() {
                println!("(no recorded runs)");
                return 0;
            }
            let rows: Vec<Vec<String>> = result
                .snapshots
                .iter()
                .map(|s| {
                    vec![
                        s.label.clone(),
                        format!("{}", s.exact.count),
                        format!("{:.1}", s.exact.total_ms),
                        s.exact.avg_ms.map_or("-".into(), |v| format!("{v:.3}")),
                        s.sampled
                            .as_ref()
                            .and_then(|p| p.self_ms)
                            .map_or("-".into(), |v| format!("{v:.1}")),
                        if s.sample_complete { "" } else { "truncated" }.to_string(),
                        s.description.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            format::print_table(
                &[
                    "label",
                    "count",
                    "total_ms",
                    "avg_ms",
                    "self_ms",
                    "sample",
                    "description",
                ],
                rows,
            );
            if result.evicted > 0 {
                println!(
                    "{} older run(s) dropped to the retention cap",
                    result.evicted
                );
            }
            if let Some(m) = &result.merged {
                println!(
                    "\nmerged: {} spans, {:.1}ms total, avg {:.3}ms",
                    m.count,
                    m.total_ms,
                    m.avg_ms.unwrap_or(0.0)
                );
            }
            if let Some(f) = &result.floor {
                match f.cv_pct {
                    Some(cv) => println!(
                        "spread over {} runs: {:.1}–{:.1}ms (mean {:.1}), CV {cv:.1}%",
                        f.runs, f.min, f.max, f.mean
                    ),
                    // Never printed as zero: that would license calling any
                    // difference significant.
                    None => println!(
                        "spread: UNKNOWN — a single run cannot establish one. Repeat the \
                         run to find out whether a difference is real."
                    ),
                }
                println!("  {}", f.caveat);
            }
            for s in &result.suppressed {
                println!("  not reported: {} — {}", s.field, s.reason);
            }
            0
        }

        ColVerb::Get {
            name,
            snapshot,
            group_by,
            skip_warmup_ms,
            top_n,
        } => {
            let result = match broker
                .collectors_get(CollectorsGet {
                    name,
                    snapshot,
                    group_by,
                    skip_warmup_ms,
                    top_n,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.get failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            print_profile(&result);
            0
        }

        ColVerb::Diff {
            a,
            b,
            group_by,
            top_n,
            allow_mismatch,
            allow_lossy,
            allow_truncated,
        } => {
            let result = match broker
                .collectors_diff(CollectorsDiff {
                    a,
                    b,
                    group_by,
                    top_n,
                    allow_mismatch: Some(allow_mismatch),
                    allow_lossy: Some(allow_lossy),
                    allow_truncated: Some(allow_truncated),
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // The refusals carry their own remedy and the flag that
                    // permits them, so the error text is the whole answer.
                    format::error(&format!("collectors.diff failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            print_diff(&result);
            0
        }

        ColVerb::Document {
            names,
            format,
            path,
            question,
            finding,
            filter_intent,
            correctness_evidence,
            group_by,
            top_n,
            allow_mismatch,
            allow_lossy,
            allow_truncated,
        } => {
            if names.is_empty() {
                format::error(
                    "give at least one arm: <collector>, <collector>@<label>, or \
                     <collector>@* for every recorded run merged",
                    json,
                );
                return 1;
            }
            let result = match broker
                .collectors_document(CollectorsDocument {
                    names,
                    format,
                    question,
                    finding,
                    filter_intent,
                    correctness_evidence,
                    group_by,
                    top_n,
                    allow_mismatch: Some(allow_mismatch),
                    allow_lossy: Some(allow_lossy),
                    allow_truncated: Some(allow_truncated),
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.document failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            // The daemon returns bytes and the client writes them (spec 9.9):
            // the broker runs as a service, so a relative path would resolve
            // against its working directory rather than the caller's.
            match &path {
                None => {
                    println!("{}", result.content);
                    for w in &result.warnings {
                        eprintln!("note: {w}");
                    }
                    if result.sidecar_name.is_some() {
                        eprintln!(
                            "note: a sidecar was produced but not written — pass --path to \
                             write both"
                        );
                    }
                }
                Some(p) => {
                    if let Err(e) = std::fs::write(p, &result.content) {
                        format::error(&format!("could not write {p}: {e}"), json);
                        return 1;
                    }
                    println!("wrote {p} ({} bytes)", result.bytes);
                    // The sidecar goes beside the document, under the name the
                    // document's own front-matter already refers to — so the two
                    // can be reunited after being moved or mailed on.
                    if let (Some(name), Some(body)) =
                        (&result.sidecar_name, &result.sidecar_content)
                    {
                        let dir = std::path::Path::new(p)
                            .parent()
                            .unwrap_or_else(|| std::path::Path::new("."));
                        let side = dir.join(name);
                        match std::fs::write(&side, body) {
                            Ok(()) => println!("wrote {} ({} bytes)", side.display(), body.len()),
                            Err(e) => eprintln!("warning: could not write sidecar {name}: {e}"),
                        }
                    }
                    for w in &result.warnings {
                        eprintln!("note: {w}");
                    }
                }
            }
            0
        }

        ColVerb::Reset { name } => {
            let result = match broker.collectors_reset(CollectorsName { name }).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.reset failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "{}: discarded {} spans totalling {:.1}ms ({} errors)",
                result.name,
                result.discarded.matched,
                result.discarded.total_ms,
                result.discarded.error_count
            );
            0
        }

        ColVerb::Remove { name } => {
            let result = match broker.collectors_remove(CollectorsName { name }).await {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("collectors.remove failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            println!(
                "removed {} ({} bytes still reserved)",
                result.removed, result.reserved_bytes
            );
            0
        }

        ColVerb::Profile {
            filter,
            group_by,
            group_keys,
            skip_warmup_ms,
            top_n,
        } => {
            let result = match broker
                .traces_profile(TracesProfile {
                    filter,
                    group_by,
                    group_keys: (!group_keys.is_empty()).then_some(group_keys),
                    skip_warmup_ms,
                    top_n,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    format::error(&format!("traces.profile failed: {e}"), json);
                    return 1;
                }
            };
            if json {
                format::print_json(&result);
                return 0;
            }
            print_profile(&result);
            0
        }
    }
}

fn print_profile(r: &ProfileResult) {
    let label = r.collector.as_deref().unwrap_or("(ad-hoc)");
    println!("{label}  {}  level={}", r.filter, r.level);
    if let Some(d) = &r.description {
        println!("  {d}");
    }
    println!(
        "  matched {} over {:.1}s of wall time, nesting {}",
        r.matched,
        r.window.wall_ms / 1000.0,
        r.nesting
    );

    if let Some(e) = &r.exact {
        println!(
            "  exact:     total {:.1}ms  avg {:.3}ms  min {:.3}ms  max {:.1}ms  errors {}",
            e.total_ms,
            e.avg_ms.unwrap_or(0.0),
            e.min_ms.unwrap_or(0.0),
            e.max_ms.unwrap_or(0.0),
            e.error_count
        );
    }
    if let Some(e) = &r.estimated {
        println!(
            "  estimated: p50 {:.3}ms  p80 {:.3}ms  p95 {:.3}ms  p99 {:.3}ms  (±{:.1}%)",
            e.p50_ms.unwrap_or(0.0),
            e.p80_ms.unwrap_or(0.0),
            e.p95_ms.unwrap_or(0.0),
            e.p99_ms.unwrap_or(0.0),
            e.alpha_pct
        );
    }
    if let Some(s) = &r.sampled {
        println!(
            "  sampled:   {} records{}  p50 {:.3}ms  p95 {:.3}ms",
            s.sample_count,
            if s.complete { "" } else { " (TRUNCATED)" },
            s.p50_ms.unwrap_or(0.0),
            s.p95_ms.unwrap_or(0.0)
        );
        if let Some(self_ms) = s.self_ms {
            println!(
                "             self {self_ms:.1}ms across {} nested matches",
                s.nested_matches
            );
        }
        if let (Some(w), Some(c)) = (s.wall_union_ms, s.achieved_concurrency) {
            println!("             wall union {w:.1}ms, concurrency {c:.2}x");
        }
        if s.overlapping_child_spans > 0 {
            println!(
                "             {} child span(s) clipped, {:.1}ms outside their parent",
                s.overlapping_child_spans, s.overlapping_child_ms
            );
        }
    }

    if let Some(i) = &r.ingest {
        let lost = i.drops_in_window + i.shed_batches + i.malformed_dropped;
        if lost > 0 {
            println!(
                "  INGEST LOSS in this window: {} dropped, {} batches shed, {} malformed \
                 (per-{}, unfiltered)",
                i.drops_in_window, i.shed_batches, i.malformed_dropped, i.attribution
            );
        }
        if i.malformed_timestamps > 0 || i.negative_duration_spans > 0 {
            println!(
                "  unusable input: {} malformed timestamps, {} negative durations",
                i.malformed_timestamps, i.negative_duration_spans
            );
        }
    }

    if let Some(g) = &r.grouped_by {
        println!("\n  by {g}:");
        for row in &r.groups {
            let total = row.exact.as_ref().map(|e| e.total_ms);
            let self_ms = row.sampled.as_ref().and_then(|s| s.self_ms);
            let n = row
                .exact
                .as_ref()
                .map(|e| e.count)
                .or_else(|| row.sampled.as_ref().map(|s| s.sample_count))
                .unwrap_or(0);
            match (total, self_ms) {
                (Some(t), _) => println!("    {:<40} {n:>8}  {t:>12.1}ms", row.key),
                (None, Some(s)) => println!("    {:<40} {n:>8}  {s:>12.1}ms self", row.key),
                _ => println!("    {:<40} {n:>8}", row.key),
            }
        }
    }

    // Last, and never omitted: a null is a different claim from a zero, and the
    // reader has to be able to tell which they are looking at.
    if !r.suppressed.is_empty() {
        println!("\n  not reported:");
        for s in &r.suppressed {
            println!("    {} — {}", s.field, s.reason);
            if let Some(rem) = &s.remedy {
                println!("      try: {rem}");
            }
        }
    }
    for w in &r.warnings {
        println!("  warning: {w}");
    }
    if r.cardinality_capped {
        println!("  note: a cardinality cap folded values into __overflow__");
    }
}

/// A comparison, rendered for a human.
///
/// Leads with what makes the numbers untrustworthy rather than burying it: a
/// reader who stops after the first screen must not come away with a delta they
/// should not have believed. Suppressed rows are shown struck through with the
/// threshold that struck them, never hidden — a row that vanished would read as
/// a row that did not exist.
fn print_diff(r: &CollectorsDiffResult) {
    println!("{}  ->  {}", r.a.spec, r.b.spec);
    println!("  a: {} | {} | {}", r.a.aggregation, r.a.level, r.a.filter);
    println!("  b: {} | {} | {}", r.b.aggregation, r.b.level, r.b.filter);
    if r.a.level != r.b.level {
        println!("  compared at: {}", r.level);
    }

    if !r.trustworthy {
        println!("\nNOT TRUSTWORTHY — read the notes before acting on any number below.");
    }
    for m in &r.marks {
        match &m.overridden_by {
            Some(flag) => println!("  [{}] {} (permitted by --{})", m.kind, m.detail, flag),
            None => println!("  [{}] {}", m.kind, m.detail),
        }
    }

    println!();
    print_diff_rows(&r.rows);

    for g in &r.groups {
        let label = match g.presence.as_str() {
            "both" => String::new(),
            other => format!("  ({other})"),
        };
        println!("\n{}{}", g.key, label);
        print_diff_rows(&g.rows);
    }
    if r.overflow_rows_suppressed > 0 {
        println!(
            "\n{} group row(s) not compared: a cardinality cap folded them into \
             __overflow__, which aggregates a different arbitrary set on each arm",
            r.overflow_rows_suppressed
        );
    }

    for s in &r.suppressed {
        match &s.remedy {
            Some(rem) => println!("\n{}: {} — {rem}", s.field, s.reason),
            None => println!("\n{}: {}", s.field, s.reason),
        }
    }

    // The floors last, because they qualify everything above and a reader who
    // has already seen a delta needs to know what would have had to be true for
    // it to mean something.
    for (which, arm) in [("a", &r.a), ("b", &r.b)] {
        match &arm.floor {
            Some(f) => match f.cv_pct {
                Some(cv) => println!(
                    "\nfloor {which}: {} runs, {:.1}-{:.1}ms (mean {:.1}), CV {cv:.1}%\n  {}",
                    f.runs, f.min, f.max, f.mean, f.caveat
                ),
                None => println!("\nfloor {which}: UNKNOWN (one run)"),
            },
            None => println!(
                "\nfloor {which}: UNKNOWN — {} is a single run. Take several and compare \
                 `<collector>@*` to find out whether a difference is real.",
                arm.spec
            ),
        }
    }
}

fn print_diff_rows(rows: &[DiffRow]) {
    let cell = |v: Option<f64>| v.map_or("-".to_string(), |x| format!("{x:.3}"));
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let delta = match r.delta {
                None => "-".to_string(),
                Some(d) => {
                    let pct = r
                        .delta_pct
                        .map_or(String::new(), |p| format!(" ({p:+.1}%)"));
                    // The strike-through is the suppression made visible, and
                    // the threshold that caused it prints beside it — 6.5
                    // forbids striking a delta through with a bound other than
                    // the one displayed.
                    if r.suppressed {
                        format!("[{d:+.3}{pct}]")
                    } else {
                        format!("{d:+.3}{pct}")
                    }
                }
            };
            let threshold = match (r.threshold_abs, r.threshold_pct) {
                (Some(abs), Some(pct)) => format!("{abs:.3} ({pct:.1}% of mean)"),
                _ => "unknown".to_string(),
            };
            vec![
                r.metric.clone(),
                r.tier.clone(),
                cell(r.a),
                cell(r.b),
                delta,
                threshold,
                r.error_bound_pct
                    .map_or("-".to_string(), |e| format!("{e:.0}%")),
                if r.trustworthy { "" } else { "!" }.to_string(),
            ]
        })
        .collect();
    format::print_table(
        &[
            "metric",
            "tier",
            "a",
            "b",
            "delta",
            "threshold",
            "err/delta",
            "",
        ],
        table,
    );
    // Suppression and doubt both print their reason: a bracketed delta with no
    // explanation is indistinguishable from a formatting artifact.
    for r in rows {
        if r.suppressed {
            println!(
                "  [{}] below the {} floor — present, but not evidence of a change",
                r.metric, r.threshold_basis
            );
        }
        for n in &r.notes {
            println!("  {}: {n}", r.metric);
        }
    }
}
