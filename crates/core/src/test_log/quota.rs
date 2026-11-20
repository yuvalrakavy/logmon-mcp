use crate::gelf::message::{Level, LogEntry};
use crate::span::types::{SpanEntry, SpanStatus};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

// The owner's choices on 2026-11-20. These are unmeasured.
const TAIL_RECORDS: u64 = 200;
const ERROR_ALLOWANCE: u64 = 1000;
const TAIL_BYTES: usize = 16 * 1024 * 1024;

pub(super) const FAULT: &str = "excessive_telemetry";

#[derive(Clone, Copy)]
pub(super) struct Crossing {
    kind: usize,
    limit: u64,
    ring_size: usize,
}

impl Crossing {
    pub fn details(self, shared_with: Vec<Value>) -> Value {
        let kind = ["logs", "spans"][self.kind];
        json!({"kind":kind,"limit":self.limit,"ring_size":self.ring_size,
            "shared_with":shared_with})
    }
}

#[derive(Clone, Copy)]
pub(super) enum Record<'a> {
    Log(&'a LogEntry),
    Span(&'a SpanEntry),
}

impl Record<'_> {
    fn kind(self) -> usize {
        usize::from(matches!(self, Self::Span(_)))
    }
    fn is_error(self) -> bool {
        match self {
            Self::Log(row) => row.level == Level::Error,
            Self::Span(row) => matches!(row.status, SpanStatus::Error(_)),
        }
    }
}

impl<'a> Record<'a> {
    fn serializable(self) -> TailRecord<'a> {
        match self {
            Self::Log(row) => TailRecord::Log(row),
            Self::Span(row) => TailRecord::Span(row),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
enum TailRecord<'a> {
    Log(&'a LogEntry),
    Span(&'a SpanEntry),
}

// Allow for the tail entry and its indexes, plus the chunk descriptor storage.
const TAIL_ENTRY_OVERHEAD: usize = 192;
const TAIL_CHUNK_OVERHEAD: usize = 2 * std::mem::size_of::<TailChunk>();

struct Charge {
    used: Arc<Mutex<usize>>,
    bytes: usize,
}

impl Charge {
    fn reserve(used: &Arc<Mutex<usize>>, bytes: usize) -> Option<Self> {
        let mut total = used.lock().unwrap();
        if bytes > TAIL_BYTES - *total {
            return None;
        }
        *total += bytes;
        Some(Self {
            used: used.clone(),
            bytes,
        })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        *self.used.lock().unwrap() -= self.bytes;
    }
}

struct TailChunk {
    bytes: Box<[u8]>,
    written: usize,
    _charge: Charge,
}

struct TailEntry(Vec<TailChunk>);

struct TailEncoder<'a> {
    quotas: &'a mut Quotas,
    chunks: Vec<TailChunk>,
    charged: usize,
}

impl Write for TailEncoder<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self
            .chunks
            .last()
            .is_none_or(|c| c.written == c.bytes.len())
        {
            // Charge each buffer before allocating it. Separate chunks avoid
            // holding both an old and a larger copy during a buffer resize.
            let first = self.chunks.is_empty();
            let overhead = TAIL_CHUNK_OVERHEAD + usize::from(first) * TAIL_ENTRY_OVERHEAD;
            let next = self
                .chunks
                .last()
                .map_or(512, |chunk| (chunk.bytes.len() * 2).min(4096));
            let target = next.max(bytes.len().min(65536));
            let capacity = target.min((TAIL_BYTES - self.charged).saturating_sub(overhead));
            if capacity == 0 {
                return Err(std::io::Error::other("record exceeds tail byte budget"));
            }
            let used = capacity + overhead;
            let charge = self
                .quotas
                .reserve(used)
                .ok_or_else(|| std::io::Error::other("tail byte budget exhausted"))?;
            self.charged += used;
            self.chunks.push(TailChunk {
                bytes: vec![0; capacity].into_boxed_slice(),
                written: 0,
                _charge: charge,
            });
        }
        let chunk = self.chunks.last_mut().unwrap();
        let len = bytes.len().min(chunk.bytes.len() - chunk.written);
        chunk.bytes[chunk.written..chunk.written + len].copy_from_slice(&bytes[..len]);
        chunk.written += len;
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Stream {
    seen: u64,
    errors: u64,
    refused: u64,
    tail_seen: u64,
    tail: BTreeSet<(u64, u64)>, // per-stream refusal ordinal, domain arrival order
}

#[derive(Default)]
pub(super) struct Group {
    streams: [Stream; 2],
    pub live: usize,
    pub executions: Vec<String>,
    pub crossing: Option<Crossing>,
    pub case_owner: Option<String>,
    pub case_path: Option<PathBuf>,
    pub case_error: Option<String>,
}

pub(super) struct Quotas {
    pub groups: Vec<Group>,
    capacities: [usize; 2],
    limits: [u64; 2],
    used: Arc<Mutex<usize>>,
    arrival: u64,
    tail: BTreeMap<u64, (usize, usize, u64, Arc<TailEntry>)>,
}

impl Quotas {
    pub fn new(capacities: [usize; 2], percent: f64) -> Self {
        Self {
            groups: Vec::new(),
            capacities,
            limits: capacities.map(|size| ((size as f64 * percent / 100.) as u64).max(1)),
            used: Arc::default(),
            arrival: 0,
            tail: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, group: usize, execution: String) {
        if group == self.groups.len() {
            self.groups.push(Group::default());
        }
        let group = &mut self.groups[group];
        group.live += 1;
        group.executions.push(execution);
    }

    pub fn admit(&mut self, record: Record<'_>, groups: &[usize]) -> (bool, Vec<usize>) {
        if groups.is_empty() {
            return (true, Vec::new());
        }
        let kind = record.kind();
        let limit = self.limits[kind];
        let mut under_limit = false;
        let mut crossed = Vec::new();
        for &id in groups {
            let group = &mut self.groups[id];
            if group.crossing.is_some() {
                continue;
            }
            let stream = &mut group.streams[kind];
            if stream.seen < limit {
                stream.seen += 1;
                under_limit = true;
            } else {
                group.crossing = Some(Crossing {
                    kind,
                    limit,
                    ring_size: self.capacities[kind],
                });
                crossed.push(id);
            }
        }
        let exempt_error = !under_limit
            && record.is_error()
            && groups
                .iter()
                .all(|&id| self.groups[id].streams[kind].errors < limit.min(ERROR_ALLOWANCE));
        if exempt_error {
            for &id in groups {
                self.groups[id].streams[kind].errors += 1;
            }
        }
        let accepted = under_limit || exempt_error;
        for &id in groups {
            if self.groups[id].crossing.is_some() {
                self.groups[id].streams[kind].refused += u64::from(!accepted);
                self.record_tail(id, kind, record);
            }
        }
        (accepted, crossed)
    }

    fn remove_tail(&mut self, arrival: u64) {
        let (group, kind, ordinal, _) = self.tail.remove(&arrival).expect("indexed tail entry");
        self.groups[group].streams[kind]
            .tail
            .remove(&(ordinal, arrival));
    }

    fn reserve(&mut self, bytes: usize) -> Option<Charge> {
        if bytes > TAIL_BYTES {
            return None;
        }
        loop {
            if let Some(charge) = Charge::reserve(&self.used, bytes) {
                return Some(charge);
            }
            let oldest = *self.tail.first_key_value()?.0;
            self.remove_tail(oldest);
        }
    }

    fn record_tail(&mut self, id: usize, kind: usize, record: Record<'_>) {
        let stream = &mut self.groups[id].streams[kind];
        stream.tail_seen += 1;
        let ordinal = stream.tail_seen;
        if let Some(&(old, arrival)) = stream.tail.first() {
            if old <= ordinal.saturating_sub(TAIL_RECORDS) {
                self.remove_tail(arrival);
            }
        }
        let mut encoder = TailEncoder {
            quotas: self,
            chunks: Vec::new(),
            charged: 0,
        };
        if serde_json::to_writer(&mut encoder, &record.serializable()).is_err() {
            return;
        }
        let entry = Arc::new(TailEntry(encoder.chunks));
        self.arrival += 1;
        self.groups[id].streams[kind]
            .tail
            .insert((ordinal, self.arrival));
        self.tail.insert(self.arrival, (id, kind, ordinal, entry));
    }

    pub fn finish(&mut self, id: usize) {
        self.groups[id].live -= 1;
    }

    pub fn clear_tails(&mut self) {
        self.tail.clear();
        for group in &mut self.groups {
            for stream in &mut group.streams {
                stream.tail.clear();
            }
        }
    }

    fn counts(&self, id: usize) -> Value {
        let [logs, spans] = &self.groups[id].streams;
        json!({"refused_telemetry":{"logs":logs.refused,"spans":spans.refused},
            "telemetry_tail_shortened":{
                "logs":logs.tail_seen.min(TAIL_RECORDS) - logs.tail.len() as u64,
                "spans":spans.tail_seen.min(TAIL_RECORDS) - spans.tail.len() as u64,
            },
            "telemetry_tail_pool_bytes":*self.used.lock().unwrap()})
    }

    pub fn save(&mut self, id: usize, execution: &str, dir: &Path) -> (Value, Option<TailJob>) {
        let relative = format!("cases/{}.telemetry-tail.jsonl", uuid::Uuid::new_v4());
        let path = dir.join(&relative);
        // Account for the job and its indexes as well as the payloads pinned
        // by its Arc references. Reservation may remove the oldest live rows.
        let bytes =
            256 + execution.len() + path.as_os_str().len() + 32 * TAIL_RECORDS as usize + 8192;
        let charge = self.reserve(bytes);
        let mut info = self.counts(id);
        let job = charge.map(|charge| {
            let mut rows: Vec<_> = self.groups[id]
                .streams
                .iter()
                .flat_map(|stream| stream.tail.iter().map(|&(_, arrival)| arrival))
                .collect();
            rows.sort_unstable();
            let rows = rows
                .into_iter()
                .map(|arrival| self.tail[&arrival].3.clone())
                .collect();
            info["telemetry_tail_path"] = json!(relative);
            TailJob {
                execution: execution.into(),
                path,
                rows,
                _charge: charge,
            }
        });
        if job.is_none() {
            info["telemetry_tail_error"] = json!("tail writer memory budget exhausted");
            let [logs, spans] = &self.groups[id].streams;
            info["telemetry_tail_shortened"] = json!({
                "logs":logs.tail_seen.min(TAIL_RECORDS),"spans":spans.tail_seen.min(TAIL_RECORDS),
            });
        }
        (info, job)
    }
}

pub(super) struct TailJob {
    execution: String,
    path: PathBuf,
    rows: Vec<Arc<TailEntry>>,
    _charge: Charge,
}

impl TailJob {
    fn write(&self) -> Result<(), String> {
        let parent = self.path.parent().expect("tail path has parent");
        let staging = self.path.with_extension("pending");
        let result = (|| {
            std::fs::create_dir_all(parent)?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)?;
            {
                let mut writer = BufWriter::new(file);
                for row in &self.rows {
                    for chunk in &row.0 {
                        writer.write_all(&chunk.bytes[..chunk.written])?;
                    }
                    writer.write_all(b"\n")?;
                }
                writer.flush()?;
            }
            std::fs::rename(&staging, &self.path)?;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(staging);
        }
        result.map_err(|err| err.to_string())
    }
}

type TailResult = (String, Result<(), String>);
type TailReceipt = (TailResult, Charge);

#[derive(Default)]
pub(super) struct TailWriter {
    sender: Option<mpsc::Sender<TailJob>>,
    finished: Option<mpsc::Receiver<TailReceipt>>,
    pub pending: usize,
}

impl TailWriter {
    pub fn send(&mut self, job: TailJob) -> Result<(), String> {
        if self.sender.is_none() {
            let (jobs, queue) = mpsc::channel::<TailJob>();
            let (finished, results) = mpsc::channel();
            std::thread::Builder::new()
                .name("test-telemetry-tail".into())
                .spawn(move || {
                    for job in queue {
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.write()))
                                .unwrap_or_else(|_| Err("telemetry tail writer panicked".into()));
                        let TailJob {
                            execution,
                            path,
                            rows,
                            _charge,
                        } = job;
                        drop(path);
                        drop(rows);
                        if finished.send(((execution, result), _charge)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|err| err.to_string())?;
            self.finished = Some(results);
            self.sender = Some(jobs);
        }
        self.sender
            .as_ref()
            .unwrap()
            .send(job)
            .map_err(|err| err.to_string())?;
        self.pending += 1;
        Ok(())
    }

    pub fn collect(&mut self) -> Vec<TailResult> {
        let results: Vec<_> = self
            .finished
            .iter()
            .flat_map(mpsc::Receiver::try_iter)
            .map(|(result, _charge)| result)
            .collect();
        self.pending -= results.len();
        results
    }
}
