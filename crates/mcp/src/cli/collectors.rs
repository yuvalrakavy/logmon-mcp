//! `collectors` subcommand group: add, list, get, reset, remove, profile.
//!
//! The human-readable rendering is deliberately opinionated: it leads with the
//! headline number, then names anything the result could not answer. A profile
//! that prints only the numbers it *has* reads as complete when it is not.

use clap::{Args, Subcommand};
use logmon_broker_protocol::{
    CollectorsAdd, CollectorsGet, CollectorsList, CollectorsName, ProfileResult, TracesProfile,
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
        /// name | group | trace | path
        #[arg(long)]
        group_by: Option<String>,
        #[arg(long)]
        skip_warmup_ms: Option<f64>,
        #[arg(long)]
        top_n: Option<u64>,
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

        ColVerb::Get {
            name,
            group_by,
            skip_warmup_ms,
            top_n,
        } => {
            let result = match broker
                .collectors_get(CollectorsGet {
                    name,
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
