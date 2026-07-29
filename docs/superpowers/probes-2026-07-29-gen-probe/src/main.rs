// Design probe for logmon span-time-collector rev 3 §3.6.
//
// Question 1: does rev 3's "every read is a swap, fold into a retired accumulator"
//             preserve sample-derived projections (self time) across reads?
// Question 2: does the alternative — ONE lock over {sealed chunks, active chunk},
//             non-destructive reads clone — stay consistent under concurrent
//             ingest, and what does the active-chunk copy actually cost?

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const CHUNK: usize = 65536;

#[derive(Clone, Copy, Debug)]
struct Rec {
    seq: u64,
    span_id: u64,
    parent: u64,
    start: i64,
    end: i64,
}

#[derive(Clone, Default)]
struct Chunk {
    recs: Vec<Rec>,
}

#[derive(Clone, Copy, Default, Debug)]
struct Exact {
    count: u64,
    total_ns: i128,
}

// ---------------------------------------------------------------- synthetic data
//
// Each group is one parent with two OVERLAPPING children:
//   parent [t, t+100]   child1 [t+10, t+70]   child2 [t+40, t+100]
// union(children clipped to parent) = [t+10, t+100] = 90  ->  parent self = 10
// naive sum of children = 120  ->  100 - 120 = -20 (the rule rev 2 used)
// children have no children of their own -> self = their own duration.
// Per group: total duration 220, true self time 10 + 60 + 60 = 130.

fn gen_records(n_groups: u64) -> Vec<Rec> {
    gen_scaled(n_groups, 1)
}

fn gen_scaled(n_groups: u64, k: i64) -> Vec<Rec> {
    let mut v = Vec::with_capacity((n_groups * 3) as usize);
    let mut seq = 0u64;
    for i in 0..n_groups {
        let base = (i as i64) * 1000 * k;
        let pid = i * 10 + 1;
        v.push(Rec { seq, span_id: pid, parent: 0, start: base, end: base + 100 * k });
        seq += 1;
        v.push(Rec { seq, span_id: i * 10 + 2, parent: pid, start: base + 10 * k, end: base + 70 * k });
        seq += 1;
        v.push(Rec { seq, span_id: i * 10 + 3, parent: pid, start: base + 40 * k, end: base + 100 * k });
        seq += 1;
    }
    v
}

// ------------------------------------------------- self time, §5.3 clipped union

fn self_time(recs: &[Rec]) -> i128 {
    let mut children: HashMap<u64, Vec<(i64, i64)>> = HashMap::new();
    for r in recs {
        if r.parent != 0 {
            children.entry(r.parent).or_default().push((r.start, r.end));
        }
    }
    let mut total: i128 = 0;
    for r in recs {
        let dur = (r.end - r.start) as i128;
        let covered = match children.get(&r.span_id) {
            None => 0i128,
            Some(kids) => {
                // clip each child to the parent interval, then union
                let mut iv: Vec<(i64, i64)> = kids
                    .iter()
                    .map(|&(s, e)| (s.max(r.start), e.min(r.end)))
                    .filter(|&(s, e)| e > s)
                    .collect();
                iv.sort_by_key(|&(s, _)| s);
                let mut acc = 0i128;
                let mut cur: Option<(i64, i64)> = None;
                for (s, e) in iv {
                    match cur {
                        None => cur = Some((s, e)),
                        Some((cs, ce)) => {
                            if s <= ce {
                                cur = Some((cs, ce.max(e)));
                            } else {
                                acc += (ce - cs) as i128;
                                cur = Some((s, e));
                            }
                        }
                    }
                }
                if let Some((cs, ce)) = cur {
                    acc += (ce - cs) as i128;
                }
                acc
            }
        };
        total += (dur - covered).max(0);
    }
    total
}

// ============================================================ Q1: does the fold hold?

fn probe_fold(recs: &[Rec], reads_at: &[usize]) {
    let reference = self_time(recs);

    // Design A (rev 3 §3.6): each read swaps the generation out, projects it,
    // and folds the SCALAR into a retired accumulator. Chunks are discarded —
    // keeping them would defeat the memory bound the swap exists to provide.
    let mut folded: i128 = 0;
    let mut start = 0usize;
    let mut generations = 0;
    let mut split_groups = 0;
    for &cut in reads_at {
        let cut = cut.min(recs.len());
        if cut > start {
            folded += self_time(&recs[start..cut]);
            generations += 1;
            // a group is split if the cut lands strictly inside one
            if cut % 3 != 0 {
                split_groups += 1;
            }
            start = cut;
        }
    }
    if start < recs.len() {
        folded += self_time(&recs[start..]);
        generations += 1;
    }

    // Design B: one lock, non-destructive read sees everything.
    let whole = self_time(recs);

    println!("--- Q1: self time across reads ({} records, {} generations) ---", recs.len(), generations);
    println!("  reference (true)          : {reference}");
    println!("  Design B (single lock)    : {whole}   {}", if whole == reference { "OK" } else { "WRONG" });
    println!("  Design A (swap-and-fold)  : {folded}   {}", if folded == reference { "OK" } else { "WRONG" });
    if folded != reference {
        let err = folded - reference;
        println!(
            "  -> over-reports self time by {err} ns across {split_groups} split parent/child groups \
             ({:.1}% of reference)",
            (err as f64) * 100.0 / (reference as f64)
        );
    }
    println!();
}

// ==================================== Q2: single-lock store under concurrent reads

struct Store {
    inner: Mutex<Inner>,
}
#[derive(Default)]
struct Inner {
    sealed: Vec<Arc<Chunk>>,
    active: Chunk,
    exact: Exact,
}

struct Snapshot {
    sealed: Vec<Arc<Chunk>>,
    active: Chunk,
    exact: Exact,
}

impl Store {
    fn new() -> Self {
        Store { inner: Mutex::new(Inner::default()) }
    }
    fn ingest(&self, r: Rec) {
        let mut g = self.inner.lock().unwrap();
        if g.active.recs.len() >= CHUNK {
            let full = std::mem::take(&mut g.active);
            g.sealed.push(Arc::new(full));
        }
        g.active.recs.push(r);
        g.exact.count += 1;
        g.exact.total_ns += (r.end - r.start) as i128;
    }
    /// Non-destructive read: clone the Arc list (cheap) + copy the active chunk.
    /// Returns (snapshot, nanos the lock was held).
    fn read(&self) -> (Snapshot, u128) {
        let t0 = Instant::now();
        let g = self.inner.lock().unwrap();
        let snap = Snapshot { sealed: g.sealed.clone(), active: g.active.clone(), exact: g.exact };
        let held = t0.elapsed().as_nanos();
        drop(g);
        (snap, held)
    }
}

fn flatten(s: &Snapshot) -> Vec<Rec> {
    let mut v = Vec::with_capacity(s.exact.count as usize);
    for c in &s.sealed {
        v.extend_from_slice(&c.recs);
    }
    v.extend_from_slice(&s.active.recs);
    v
}

fn probe_concurrent(total: u64, n_readers: usize) {
    let recs = gen_records(total / 3);
    let store = Arc::new(Store::new());
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let worst_hold = Arc::new(AtomicU64::new(0));
    let violations = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..n_readers {
        let store = store.clone();
        let done = done.clone();
        let reads = reads.clone();
        let worst_hold = worst_hold.clone();
        let violations = violations.clone();
        handles.push(thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let (snap, held) = store.read();
                worst_hold.fetch_max(held as u64, Ordering::Relaxed);
                reads.fetch_add(1, Ordering::Relaxed);
                let flat = flatten(&snap);
                // INVARIANT 1: the exact tier agrees with the retained records.
                if flat.len() as u64 != snap.exact.count {
                    violations.fetch_add(1, Ordering::Relaxed);
                }
                // INVARIANT 2: records form a contiguous prefix 0..n with no gaps
                // and no duplicates — a seal-boundary race shows up here.
                for (i, r) in flat.iter().enumerate() {
                    if r.seq != i as u64 {
                        violations.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }));
    }

    let t0 = Instant::now();
    for r in &recs {
        store.ingest(*r);
    }
    let ingest_ns = t0.elapsed().as_nanos();
    done.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    let (snap, _) = store.read();
    let flat = flatten(&snap);
    println!("--- Q2: single-lock store, 1 writer + {n_readers} readers ---");
    println!("  ingested                  : {}", recs.len());
    println!("  final retained            : {}", flat.len());
    println!("  exact.count               : {}", snap.exact.count);
    println!("  concurrent reads done     : {}", reads.load(Ordering::Relaxed));
    println!("  invariant violations      : {}", violations.load(Ordering::Relaxed));
    println!("  worst read lock hold      : {:.3} ms", worst_hold.load(Ordering::Relaxed) as f64 / 1e6);
    println!("  ingest throughput         : {:.0} spans/sec",
             recs.len() as f64 / (ingest_ns as f64 / 1e9));
    println!("  self time (concurrent)    : {}", self_time(&flat));
    println!("  self time (reference)     : {}", self_time(&recs));
    println!();
}

fn main() {
    let recs = gen_records(20_000); // 60k records
    println!("== logmon rev 3 §3.6 design probe ==\n");

    // Reads land at arbitrary points — the realistic case, since a read is
    // driven by the operator, not aligned to trace boundaries.
    let cuts: Vec<usize> = (1..=12).map(|k| k * 4_999).collect();
    probe_fold(&recs, &cuts);

    // Control: reads aligned exactly to group boundaries never split a parent.
    let aligned: Vec<usize> = (1..=12).map(|k| k * 4_998).collect(); // 4998 % 3 == 0
    probe_fold(&recs, &aligned);

    // Realistic scale: parent 200 ms with ~180 ms of child coverage, and the
    // operator reads the collector 100 times over the run.
    println!("== realistic scale: parent 200ms, 2M spans, 100 reads ==");
    let big = gen_scaled(666_666, 2_000_000);
    let many: Vec<usize> = (1..=100).map(|k| k * 19_997).collect();
    probe_fold(&big, &many);

    probe_concurrent(60_000, 3);
}
