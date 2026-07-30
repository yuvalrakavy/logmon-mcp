//! §12 A4 and A14: reading and resetting while ingest is running.
//!
//! The §3.6 lock discipline exists so a reader can never see the exact tier and
//! the sample tier disagreeing, and so a reset takes everything or nothing.
//! Both are properties that only fail under concurrency, so they are asserted
//! under concurrency.

use chrono::{TimeZone, Utc};
use logmon_broker_core::collector::registry::CollectorRegistry;
use logmon_broker_core::collector::sample::Level;
use logmon_broker_core::collector::state::{CollectorDef, DEFAULT_MAX_SAMPLE_BYTES};
use logmon_broker_core::daemon::domain::DomainId;
use logmon_broker_core::daemon::session::SessionId;
use logmon_broker_core::filter::parser::parse_filter;
use logmon_broker_core::receiver::ReceiverMetrics;
use logmon_broker_core::span::types::{SpanEntry, SpanKind, SpanStatus};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Writer threads. Enough to interleave, few enough to stay fast in CI.
const WRITERS: usize = 4;
/// Spans per writer. 2 000 × 4 = 8 000 — below the 8 192-record chunk size, so
/// the sample tier stays complete and `sample_count` must equal `count`.
const PER_WRITER: usize = 2_000;

const S: i64 = 1_700_000_000_000_000_000;

fn def(name: &str) -> CollectorDef {
    CollectorDef {
        name: name.into(),
        filter_string: "sv=svc".into(),
        filter: parse_filter("sv=svc").expect("valid"),
        level: Level::Tree,
        group_keys: vec![],
        max_sample_bytes: DEFAULT_MAX_SAMPLE_BYTES,
        description: None,
        threshold: None,
    }
}

fn span(i: usize) -> SpanEntry {
    let start = S + (i as i64) * 1_000;
    SpanEntry {
        seq: 0,
        trace_id: 7,
        span_id: i as u64 + 1,
        parent_span_id: None,
        start_time: Utc.timestamp_nanos(start),
        end_time: Utc.timestamp_nanos(start + 1_000_000),
        duration_ms: 1.0,
        name: "op".into(),
        kind: SpanKind::Internal,
        service_name: "svc".into(),
        status: SpanStatus::Ok,
        attributes: HashMap::new(),
        events: vec![],
    }
}

fn armed(name: &str) -> (Arc<CollectorRegistry>, SessionId, DomainId) {
    let registry = Arc::new(CollectorRegistry::new());
    let owner = SessionId::Named("bench".into());
    let domain = DomainId::default_domain();
    registry
        .add(
            &owner,
            &domain,
            Arc::new(ReceiverMetrics::new()),
            def(name),
            Utc::now(),
        )
        .expect("armed");
    (registry, owner, domain)
}

#[test]
fn a14_a_reader_never_sees_the_two_tiers_disagree() {
    let (registry, owner, domain) = armed("c");
    let stop = Arc::new(AtomicBool::new(false));

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let registry = registry.clone();
            let domain = domain.clone();
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    registry.ingest_span(&domain, &span(w * PER_WRITER + i));
                }
            })
        })
        .collect();

    let reader = {
        let registry = registry.clone();
        let owner = owner.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let armed = registry.get(&owner, "c").expect("still armed");
                let snap = armed.collector.snapshot();
                // The property: every retained sample corresponds to a counted
                // span, and nothing well-formed is counted without being
                // retained. A snapshot taken between the two updates would
                // break this — which is why they happen under one lock.
                assert_eq!(
                    snap.samples.len() as u64,
                    snap.total.count,
                    "a reader observed the exact tier and the sample tier out of step"
                );
                reads += 1;
            }
            reads
        })
    };

    for w in writers {
        w.join().expect("writer finished");
    }
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().expect("reader finished");
    assert!(
        reads > 0,
        "the reader must have actually observed something"
    );

    let snap = registry.get(&owner, "c").unwrap().collector.snapshot();
    assert_eq!(snap.total.count, (WRITERS * PER_WRITER) as u64);
    assert_eq!(
        snap.total.total_ns,
        (WRITERS * PER_WRITER) as i128 * 1_000_000
    );
}

#[test]
fn a4_a_reset_during_ingest_takes_everything_or_nothing() {
    // A11's invariant under concurrency: what the resets took plus what remains
    // equals everything ingested, exactly. A reset that zeroed some structures
    // and not others would lose spans here, and only here.
    let (registry, owner, domain) = armed("c");
    let stop = Arc::new(AtomicBool::new(false));

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let registry = registry.clone();
            let domain = domain.clone();
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    registry.ingest_span(&domain, &span(w * PER_WRITER + i));
                }
            })
        })
        .collect();

    let resetter = {
        let registry = registry.clone();
        let owner = owner.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut taken_count = 0u64;
            let mut taken_ns = 0i128;
            let mut resets = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let s = registry.reset(&owner, "c", Utc::now()).expect("armed");
                taken_count += s.total.count;
                taken_ns += s.total.total_ns;
                resets += 1;
                std::thread::yield_now();
            }
            (taken_count, taken_ns, resets)
        })
    };

    for w in writers {
        w.join().expect("writer finished");
    }
    stop.store(true, Ordering::Relaxed);
    let (taken_count, taken_ns, resets) = resetter.join().expect("resetter finished");
    assert!(resets > 1, "the resets must have overlapped the writers");

    let remaining = registry.get(&owner, "c").unwrap().collector.snapshot();
    let total = WRITERS * PER_WRITER;
    assert_eq!(
        taken_count + remaining.total.count,
        total as u64,
        "a span was lost or double-counted across a reset"
    );
    assert_eq!(
        taken_ns + remaining.total.total_ns,
        total as i128 * 1_000_000,
        "the summed duration must survive the resets exactly"
    );
}

#[test]
fn a14_arming_a_collector_does_not_make_the_ingest_path_drop_spans() {
    // The observer effect must not become data loss: with collectors armed,
    // every offered span is still accounted for. Measured cost is in the
    // criterion bench (`span_ingest/N_collectors`); this asserts correctness
    // at the same shape.
    let registry = Arc::new(CollectorRegistry::new());
    let owner = SessionId::Named("s".into());
    let domain = DomainId::default_domain();
    let metrics = Arc::new(ReceiverMetrics::new());
    // Four is the ceiling the daemon reservation permits at the default size.
    for i in 0..4 {
        let mut d = def(&format!("c{i}"));
        d.max_sample_bytes = DEFAULT_MAX_SAMPLE_BYTES;
        registry
            .add(&owner, &domain, metrics.clone(), d, Utc::now())
            .expect("within budget");
    }

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let registry = registry.clone();
            let domain = domain.clone();
            std::thread::spawn(move || {
                for i in 0..PER_WRITER {
                    registry.ingest_span(&domain, &span(w * PER_WRITER + i));
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer finished");
    }

    for i in 0..4 {
        let snap = registry
            .get(&owner, &format!("c{i}"))
            .expect("armed")
            .collector
            .snapshot();
        assert_eq!(
            snap.total.count,
            (WRITERS * PER_WRITER) as u64,
            "collector c{i} missed spans"
        );
        assert!(snap.samples.complete, "and retained all of them");
    }
}
