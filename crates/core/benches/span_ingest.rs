//! Per-span ingest cost.
//!
//! Exists because the span-time-collector design (§12, A14) needs an honest
//! baseline: collector overhead has to be measured against the *current* ingest
//! path, not against zero. It also gives the before/after for §4.2's matcher
//! de-allocation.
//!
//! Note what the "1 session" case includes: `SessionRegistry` seeds every
//! session with two triggers, and trigger 2 is a regex. `is_span_filter_str`
//! calls `parse_filter` on every trigger for every span, so that regex is
//! compiled once per span per bound session on the path as it stands.

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion};
use logmon_broker_core::daemon::session::SessionRegistry;
use logmon_broker_core::daemon::span_processor::process_span;
use logmon_broker_core::engine::pipeline::LogPipeline;
use logmon_broker_core::engine::seq_counter::SeqCounter;
use logmon_broker_core::filter::matcher::matches_span;
use logmon_broker_core::filter::parser::parse_filter;
use logmon_broker_core::span::store::SpanStore;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;

fn make_span() -> SpanEntry {
    let now = Utc::now();
    SpanEntry {
        seq: 0,
        trace_id: 0xabc_u128,
        span_id: 0xdef_u64,
        parent_span_id: None,
        start_time: now,
        end_time: now,
        duration_ms: 12.5,
        name: "schema.resolve_path".to_string(),
        kind: SpanKind::Internal,
        service_name: "store_server".to_string(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

fn ingest_with_sessions(c: &mut Criterion, n_sessions: usize) {
    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(10_000, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(10_000));
    for _ in 0..n_sessions {
        sessions.create_anonymous();
    }
    let span = make_span();

    c.bench_function(&format!("span_ingest/{n_sessions}_sessions"), |b| {
        b.iter(|| {
            process_span(
                black_box(&span),
                black_box(&store),
                black_box(&sessions),
                black_box(&pipeline),
            )
        })
    });
}

fn bench_ingest(c: &mut Criterion) {
    // 0 = store + session scan only. 1 and 4 add the seeded-trigger evaluation,
    // which is where the per-span parse and regex compile live.
    ingest_with_sessions(c, 0);
    ingest_with_sessions(c, 1);
    ingest_with_sessions(c, 4);
}

/// A14, the observer effect: what arming a collector costs the ingest path.
///
/// Measured at the level that retains the most, over the flagship filter, with
/// one bound session so the trigger path is present too — i.e. the cost as a
/// user would actually pay it, not in isolation.
fn ingest_with_collectors(c: &mut Criterion, n_collectors: usize) {
    use logmon_broker_core::collector::registry::CollectorRegistry;
    use logmon_broker_core::collector::sample::Level;
    use logmon_broker_core::collector::state::{CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
    use logmon_broker_core::daemon::domain::DomainId;
    use logmon_broker_core::daemon::session::SessionId;
    use logmon_broker_core::daemon::span_processor::process_span_for_domain;
    use logmon_broker_core::receiver::ReceiverMetrics;

    let seq = Arc::new(SeqCounter::new());
    let store = Arc::new(SpanStore::new(10_000, seq));
    let sessions = Arc::new(SessionRegistry::new());
    let pipeline = Arc::new(LogPipeline::new(10_000));
    sessions.create_anonymous();

    let domain = DomainId::default_domain();
    let collectors = CollectorRegistry::new();
    let metrics = Arc::new(ReceiverMetrics::new());
    for i in 0..n_collectors {
        // A quarter of the daemon reservation each, so four fit — the ceiling
        // §3.4 actually permits.
        collectors
            .add(
                &SessionId::Named(format!("s{i}")),
                &domain,
                metrics.clone(),
                CollectorDef {
                    name: format!("c{i}"),
                    filter_string: "sv=store_server".into(),
                    filter: parse_filter("sv=store_server").expect("valid"),
                    level: Level::Tree,
                    group_keys: vec![],
                    max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
                    description: None,
                },
                Utc::now(),
            )
            .expect("within budget");
    }
    let span = make_span();

    c.bench_function(&format!("span_ingest/{n_collectors}_collectors"), |b| {
        b.iter(|| {
            process_span_for_domain(
                black_box(&span),
                black_box(&store),
                black_box(&sessions),
                black_box(&pipeline),
                black_box(&collectors),
                black_box(&domain),
            )
        })
    });
}

fn bench_collectors(c: &mut Criterion) {
    ingest_with_collectors(c, 0);
    ingest_with_collectors(c, 1);
    ingest_with_collectors(c, 4);
}

/// The §4.2 claim under test on its own: a substring match is two `to_lowercase`
/// allocations per pattern per span. `sv=store_server` is the design's own
/// flagship filter.
fn bench_matcher(c: &mut Criterion) {
    let span = make_span();

    let f_service = parse_filter("sv=store_server").expect("valid");
    c.bench_function("matches_span/substring_service", |b| {
        b.iter(|| matches_span(black_box(&f_service), black_box(&span)))
    });

    let f_name = parse_filter("sn=schema.resolve_path").expect("valid");
    c.bench_function("matches_span/substring_name", |b| {
        b.iter(|| matches_span(black_box(&f_name), black_box(&span)))
    });

    // Allocation-free control: the duration arm touches no strings, so the gap
    // between this and the substring arms is the allocation cost.
    let f_dur = parse_filter("d>=10").expect("valid");
    c.bench_function("matches_span/duration_control", |b| {
        b.iter(|| matches_span(black_box(&f_dur), black_box(&span)))
    });

    // Two qualifiers AND-ed — the realistic collector filter shape.
    let f_both = parse_filter("sv=store_server, d>=10").expect("valid");
    c.bench_function("matches_span/service_and_duration", |b| {
        b.iter(|| matches_span(black_box(&f_both), black_box(&span)))
    });
}

/// Attribution: `is_span_filter_str` calls `parse_filter` on every trigger for
/// every span. These are the two filter strings `SessionRegistry` seeds into
/// every session, so this is the per-span, per-session cost paid today.
fn bench_parse(c: &mut Criterion) {
    c.bench_function("parse_filter/seeded_level", |b| {
        b.iter(|| parse_filter(black_box("l>=ERROR")).expect("valid"))
    });
    c.bench_function("parse_filter/seeded_regex", |b| {
        b.iter(|| parse_filter(black_box("/panic|unwrap failed|stack backtrace/")).expect("valid"))
    });
    c.bench_function("parse_filter/collector_shape", |b| {
        b.iter(|| parse_filter(black_box("sv=store_server, d>=10")).expect("valid"))
    });
}

criterion_group!(
    benches,
    bench_ingest,
    bench_collectors,
    bench_matcher,
    bench_parse
);
criterion_main!(benches);
