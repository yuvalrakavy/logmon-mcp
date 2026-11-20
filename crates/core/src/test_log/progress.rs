//! Derived from accepted lifecycle transitions, independently of journal delivery.
use super::writer::{Health, Journal};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use logmon_broker_protocol::{TestLogBegin, TestLogLane, TestLogRunState, TEST_LOG_SCHEMA_VERSION};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RETAINED: usize = 10;

#[derive(Default)]
pub(super) struct History(ArcSwap<Catalog>);
#[derive(Clone, Default)]
struct Catalog {
    current: Option<(Arc<Slot>, Arc<View>)>,
    settled: VecDeque<(Arc<Slot>, Arc<View>)>,
}

struct Slot {
    id: String,
    began: DateTime<Utc>,
    fields: Value,
    path: String,
    journal: Arc<Health>,
}

#[derive(Clone)]
struct Running {
    name: String,
    trace: Option<String>,
    start: DateTime<Utc>,
    deadline: Option<Instant>,
}

#[derive(Clone, Default)]
struct View {
    state: &'static str,
    planned: Option<u64>,
    last: Option<DateTime<Utc>>,
    completion: Option<DateTime<Utc>>,
    settled: Option<DateTime<Utc>>,
    started: usize,
    ended: usize,
    verdicts: [usize; 5],
    oldest: Vec<(String, Running)>,
    failures: VecDeque<Arc<Value>>,
    failure_total: usize,
    cases: [usize; 5], // pending, ready, failed, skipped, minimal by failure limit
    files: Arc<BTreeMap<String, Vec<String>>>,
    max_failures: u64,
    finalized: usize,
    replayed: usize,
}

const VERDICTS: [&str; 5] = ["OK", "FAIL", "TIMEOUT", "ABORTED", "NO_VERDICT"];

impl View {
    fn aggregates(&self) -> Value {
        let [pending, ready, failed, skipped, minimal] = self.cases;
        json!({"executions":{"planned":self.planned,"started":self.started,"ended":self.ended,
            "in_flight":self.started-self.ended,"by_verdict":Object::from_iter(VERDICTS.into_iter().zip(self.verdicts).map(|(k,n)|(k.into(),json!(n))))},
            "cases":{"requested":pending+ready+failed+skipped,"ready_executions":ready,
            "distinct_cases_written":self.files.len(),"pending":pending,"failed":failed,"skipped":skipped,
            "refused_by_max_failures":0,"minimal_by_failure_limit":minimal,"max_failures":self.max_failures},
            "summaries":{"final":self.finalized,"pending":self.ended-self.finalized},"replayed_ends":self.replayed})
    }
}
type Object = serde_json::Map<String, Value>;

impl Slot {
    fn render(&self, v: &View, detail: bool) -> Value {
        let now = v.settled.unwrap_or_else(Utc::now);
        let mut out = v.aggregates();
        let o = out.as_object_mut().unwrap();
        o.extend(
            json!({"run_id":self.id,"began_at":self.began,"state":v.state,
            "last_event_at":v.last,"last_completion_at":v.completion,"fields":self.fields,
            "elapsed_ms":(now-self.began).num_milliseconds().max(0)})
            .as_object()
            .unwrap()
            .clone(),
        );
        if detail {
            out["jsonl_path"] = json!(self.path);
            out["longest_in_flight"] = json!(v.oldest.iter().map(|(id,r)| {
                let elapsed=(now-r.start).num_milliseconds().max(0) as u64;
                json!({"name":r.name,"execution_id":id,"trace_id":r.trace,"started_at":r.start,
                    "elapsed_ms":elapsed,"deadline_overdue_ms":r.deadline.map(|t|Instant::now().saturating_duration_since(t).as_millis() as u64)})
            }).collect::<Vec<_>>());
            out["recent_failures"] = json!(v.failures.iter().map(Arc::as_ref).collect::<Vec<_>>());
            out["recent_failures_truncated"] = json!(v.failure_total > v.failures.len());
            out["cases"]["files"] = json!(v
                .files
                .iter()
                .map(|(p, ids)| json!({"path":p,"execution_ids":ids}))
                .collect::<Vec<_>>());
            out["journal"] = self.journal.json();
        }
        out
    }
}

impl History {
    // Lifecycle transitions are serialized by the domain. Readers load a single
    // immutable catalog and never acquire the domain, journal, or publisher mutex.
    fn publish(&self, slot: Arc<Slot>, view: Arc<View>, settled: bool) {
        let mut c = (**self.0.load()).clone();
        if settled {
            c.current = None;
            c.settled.push_front((slot, view));
            c.settled.truncate(RETAINED);
        } else {
            c.current = Some((slot, view));
        }
        self.0.store(Arc::new(c));
    }
    pub fn retains(&self, id: &str) -> bool {
        self.0.load().settled.iter().any(|(s, _)| s.id == id)
    }
    pub fn status(&self, domain: &str, id: Option<&str>) -> Value {
        let slot = {
            let c = self.0.load();
            if let Some(id) = id {
                c.current
                    .iter()
                    .chain(c.settled.iter())
                    .find(|(s, _)| s.id == id)
                    .cloned()
            } else {
                c.current.clone()
            }
        };
        if let (Some(id), None) = (id, &slot) {
            return json!({"schema_version":TEST_LOG_SCHEMA_VERSION,"error":{"code":"run_not_found",
               "message":format!("Run {id} is not retained in domain {domain}."),"domain":domain,"run_id":id}});
        }
        json!({"schema_version":TEST_LOG_SCHEMA_VERSION,"domain":domain,"test_log":slot.map(|(s,v)|s.render(&v,true))})
    }
    pub fn list(&self, domain: &str) -> Value {
        let mut slots = {
            let c = self.0.load();
            c.current
                .iter()
                .chain(c.settled.iter())
                .cloned()
                .collect::<Vec<_>>()
        };
        slots.sort_by_key(|(s, _)| std::cmp::Reverse(s.began));
        json!({"schema_version":TEST_LOG_SCHEMA_VERSION,"domain":domain,"runs":slots.iter().map(|(s,v)|s.render(v,false)).collect::<Vec<_>>()})
    }
    pub fn projection(&self) -> Option<TestLogLane> {
        let (slot, v) = self.0.load().current.clone()?;
        let state = match v.state {
            "open" => TestLogRunState::Open,
            "closing" => TestLogRunState::Closing,
            _ => return None,
        };
        Some(TestLogLane {
            run_id: slot.id.clone(),
            state,
            in_flight: (v.started - v.ended) as u64,
        })
    }
}

pub(super) struct Progress {
    history: Arc<History>,
    slot: Arc<Slot>,
    model: Mutex<Model>,
}
#[derive(Default)]
struct Model {
    view: View,
    running: BTreeMap<(DateTime<Utc>, String), Running>,
    outcomes: HashMap<String, usize>,
    finals: HashSet<String>,
    decisions: HashMap<String, Decision>,
    sealed: bool,
    default_timeout: u64,
}
struct Decision {
    bucket: usize,
    status: String,
    path: Value,
    reason: Value,
    size: Value,
    detail: Value,
}
impl Decision {
    fn apply(&self, row: &mut Value) {
        row["case_status"] = json!(self.status);
        row["case_path"] = self.path.clone();
        if !self.reason.is_null() {
            row["case_reason"] = self.reason.clone();
        }
        for (key, value) in [("case_size", &self.size), ("case_detail", &self.detail)] {
            if !value.is_null() {
                row[key] = value.clone();
            }
        }
    }
}
fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()?.parse().ok()
}

impl Progress {
    pub fn new(
        config: &TestLogBegin,
        began: DateTime<Utc>,
        journal: Arc<Journal>,
        history: Arc<History>,
    ) -> Self {
        let view = View {
            state: "open",
            planned: config.planned_executions,
            last: Some(began),
            max_failures: super::full_failure_limit(config),
            ..View::default()
        };
        let slot = Arc::new(Slot {
            id: config.run_id.clone(),
            began,
            fields: json!(config.fields),
            path: config.jsonl_path.clone(),
            journal: journal.progress_health(),
        });
        Self {
            history,
            slot,
            model: Mutex::new(Model {
                view,
                default_timeout: config.test_timeout_ms.unwrap_or(600000),
                ..Model::default()
            }),
        }
    }
    pub fn publish_begin(&self) {
        self.history.publish(
            self.slot.clone(),
            Arc::new(self.model.lock().unwrap().view.clone()),
            false,
        )
    }
    pub fn apply(&self, event: &str, d: &mut Value) {
        if !matches!(
            event,
            "start-test"
                | "test-log-plan"
                | "end-test"
                | "incomplete"
                | "summary-final"
                | "case-ready"
                | "case-error"
                | "case-skipped"
                | "machine-load"
                | "end-test-log"
                | "seal-test-log"
                | "run-settled"
        ) {
            return;
        }
        let mut m = self.model.lock().unwrap();
        let now = Utc::now();
        m.view.last = Some(now);
        let id = d["execution_id"].as_str().unwrap_or_default();
        match event {
            "test-log-plan" => {
                m.view.planned = d["planned_executions"].as_u64();
                m.view.max_failures = d["max_failures"].as_u64().expect("derived failure limit");
            }
            "start-test" => {
                if let Some(start) = timestamp(&d["started_at"]) {
                    let r = Running {
                        name: d["name"].as_str().unwrap_or_default().into(),
                        trace: (d["selector"]["field"] == "trace_id")
                            .then(|| d["selector"]["value"].as_str().unwrap_or_default().into()),
                        start,
                        deadline: Instant::now().checked_add(Duration::from_millis(
                            d["test_timeout_ms"].as_u64().unwrap_or(m.default_timeout),
                        )),
                    };
                    m.running.insert((start, id.into()), r);
                    m.view.started += 1;
                }
            }
            "end-test" | "incomplete" => m.end(id, d, now),
            "summary-final" => {
                if m.finals.insert(id.into()) {
                    m.view.finalized += 1;
                }
            }
            "case-ready" | "case-error" | "case-skipped" => m.decision(id, d, true),
            "end-test-log" | "seal-test-log" => {
                m.view.state = "closing";
                m.sealed = event == "seal-test-log";
            }
            "run-settled" => {
                m.view.state = if m.sealed { "sealed" } else { "closed" };
                m.view.settled = Some(now);
                // Preserve the original numeric executions field for existing readers.
                d["aggregates"] = m.view.aggregates();
            }
            _ => {}
        }
        m.view.oldest = m
            .running
            .iter()
            .take(3)
            .map(|((_, id), r)| (id.clone(), r.clone()))
            .collect();
        self.history.publish(
            self.slot.clone(),
            Arc::new(m.view.clone()),
            event == "run-settled",
        );
    }
}

impl Model {
    fn end(&mut self, id: &str, d: &Value, now: DateTime<Utc>) {
        let verdict = d["verdict"].as_str().unwrap_or("NO_VERDICT");
        let Some(bucket) = VERDICTS.iter().position(|x| *x == verdict) else {
            return;
        };
        let prior = self.outcomes.insert(id.into(), bucket);
        if prior == Some(bucket) {
            return;
        }
        if let Some(old) = prior {
            self.view.verdicts[old] -= 1;
        } else {
            if timestamp(&d["started_at"])
                .and_then(|t| self.running.remove(&(t, id.to_owned())))
                .is_none()
            {
                self.view.started += 1;
            }
            self.view.ended += 1;
        }
        self.view.verdicts[bucket] += 1;
        let at = timestamp(&d["ended_at"])
            .or_else(|| timestamp(&d["closed_at"]))
            .or_else(|| timestamp(&d["captured_at"]))
            .unwrap_or(now);
        self.view.completion = Some(self.view.completion.map_or(at, |last| last.max(at)));
        if d["snapshot_delayed"] == true {
            self.view.replayed += 1;
        }
        if (1..=3).contains(&bucket) {
            self.view.failure_total += 1;
            let mut row = json!({"execution_id":id,"name":d["name"],"failed_test":d["failed_test"].as_str(),"verdict":verdict,
                "message":d["message"].as_str(),"ended_at":at,"case_status":d["case_status"],"case_path":null,
                "rerun_command":d["fields"]["rerun_command"].as_str()});
            if let Some(existing) = self.decisions.get(id) {
                existing.apply(&mut row);
            }
            let index = self
                .view
                .failures
                .iter()
                .position(|r| timestamp(&r["ended_at"]).is_some_and(|t| t <= at))
                .unwrap_or(self.view.failures.len());
            self.view.failures.insert(index, Arc::new(row));
            self.view.failures.truncate(20);
        }
        self.decision(id, d, false);
    }
    fn decision(&mut self, id: &str, d: &Value, terminal: bool) {
        let Some(status) = d["case_status"].as_str() else {
            return;
        };
        let bucket = match status {
            "pending" => 0,
            "ready" => 1,
            "error" => 2,
            _ => 3,
        };
        if let Some(old) = self.decisions.get(id) {
            if old.bucket != 0 || !terminal {
                return;
            }
            self.view.cases[old.bucket] -= 1;
        } else if d["case_size"] == "minimal" {
            self.view.cases[4] += 1;
        }
        let reason = match bucket {
            2 => d.get("message").cloned().unwrap_or_else(|| json!(status)),
            3 => json!(status),
            _ => Value::Null,
        };
        let decision = Decision {
            bucket,
            status: if bucket == 3 {
                "skipped".into()
            } else {
                status.into()
            },
            path: d["case_path"].clone(),
            reason,
            size: d["case_size"].clone(),
            detail: d["case_detail"].clone(),
        };
        self.view.cases[bucket] += 1;
        if let Some(path) = d["case_path"].as_str() {
            Arc::make_mut(&mut self.view.files)
                .entry(path.into())
                .or_default()
                .push(id.into());
        }
        if let Some(row) = self
            .view
            .failures
            .iter_mut()
            .find(|r| r["execution_id"] == id)
        {
            decision.apply(Arc::make_mut(row));
        }
        self.decisions.insert(id.into(), decision);
    }
}
