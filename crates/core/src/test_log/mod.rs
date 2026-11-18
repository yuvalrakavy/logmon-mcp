//! Run ownership, execution lifecycle and capture admission for a single domain.

mod run;
mod writer;

use crate::cases::document::RegistryFact;
use crate::daemon::domain::Domain;
use crate::gelf::message::LogEntry;
use crate::span::types::SpanEntry;
use chrono::Utc;
use logmon_broker_protocol::*;
use run::Run;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::Instant;

#[derive(Default)]
pub struct TestLog {
    pub(crate) capture_pool: Arc<Mutex<crate::nightly::CapturePool>>,
    state: Mutex<State>,
    observing: AtomicBool,
    changed: Condvar,
    writer_budget: Arc<writer::QueueBudget>,
}

#[derive(Default)]
struct State {
    run: Option<Run>,
    // Old close receipts are small; full evidence and summaries live in the run file.
    closed: HashMap<String, ClosedRun>,
}

struct ClosedRun {
    result: Value,
    pending_writer: Option<Arc<writer::Journal>>,
}

impl ClosedRun {
    fn result(&self) -> Value {
        let mut result = self.result.clone();
        if let Some(writer) = &self.pending_writer {
            if let (Some(output), Value::Object(status)) = (result.as_object_mut(), writer.status())
            {
                output.extend(status);
            }
        }
        result
    }
}

fn parse<T: DeserializeOwned>(v: &Value) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| e.to_string())
}

fn refused(id: &str, code: &str) -> Value {
    json!({"status":"refused","run_id":id,"code":code})
}

impl TestLog {
    pub(crate) fn rpc(
        &self,
        d: &Arc<Domain>,
        method: &str,
        params: &Value,
        provenance: Vec<RegistryFact>,
    ) -> Result<Value, String> {
        let id = params.get("run_id").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() {
            return Ok(refused(id, "missing_run_id"));
        }
        if method == "test_log.begin" {
            return self.begin(d, parse(params)?, provenance);
        }
        let mut state = self.state.lock().unwrap();
        if matches!(method, "test_log.close" | "test_log.seal") {
            if let Some(receipt) = state.closed.get(id) {
                return Ok(receipt.result());
            }
        }
        let Some(run) = state.run.as_mut() else {
            return Ok(refused(id, "no_active_run"));
        };
        if run.config.run_id != id {
            return Ok(refused(id, "wrong_run_id"));
        }
        run.last_activity = Utc::now();
        let mut result = match method {
            "test_log.start" => run.start(d, parse(params)?)?,
            "test_log.end" => run.end(d, parse(params)?, false, &provenance)?,
            "test_log.machine_load" => run.machine_load(d, parse(params)?),
            "test_log.close" | "test_log.seal" => {
                if method.ends_with("seal") {
                    let _: TestLogSeal = parse(params)?;
                } else {
                    let _: TestLogClose = parse(params)?;
                }
                run.close(d, method.ends_with("seal"), &provenance);
                run.close_result()
            }
            "test_log.replay" => {
                let events = params
                    .get("events")
                    .and_then(Value::as_array)
                    .ok_or("missing events array")?;
                if events.len() > 100 {
                    return Ok(json!({"status":"refused","code":"batch_too_large","results":[]}));
                }
                let mut results = Vec::with_capacity(events.len());
                for item in events {
                    let result = match parse::<TestLogEnd>(item) {
                        Ok(end) if end.run_id == id => match run.end(d, end, true, &provenance) {
                            Ok(mut ack) => {
                                if ack["status"] == "accepted" {
                                    ack["status"] = json!("applied");
                                }
                                ack
                            }
                            Err(message) => {
                                json!({"status":"refused","code":"invalid_event","message":message})
                            }
                        },
                        Ok(_) => refused(id, "wrong_run_id"),
                        Err(message) => {
                            json!({"status":"refused","code":"invalid_event","message":message})
                        }
                    };
                    results.push(result);
                }
                json!({"status":"accepted","results":results})
            }
            _ => return Err(format!("unknown method: {method}")),
        };
        result["test_log_health"] = run.journal.status()["test_log_health"].clone();
        self.changed.notify_all();
        Ok(result)
    }

    fn begin(
        &self,
        d: &Arc<Domain>,
        config: TestLogBegin,
        provenance: Vec<RegistryFact>,
    ) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap();
        if let Some(receipt) = state.closed.get(&config.run_id) {
            return Ok(receipt.result());
        }
        if let Some(run) = &state.run {
            if run.config.run_id == config.run_id {
                return Ok(
                    json!({"status":"already_applied","run_id":config.run_id,"closing":run.closing,"test_log_health":run.journal.status()["test_log_health"]}),
                );
            }
            let result = run.close_result();
            if !run.closing || result["finalization"] != "settled" {
                return Ok(
                    json!({"status":"refused","code":"run_active","run_id":config.run_id,"owner_run_id":run.config.run_id,"last_activity":run.last_activity}),
                );
            }
        }
        Run::validate_config(&config)?;
        let journal = match writer::Journal::open(
            Path::new(&config.jsonl_path),
            self.writer_budget.clone(),
        ) {
            Ok(writer) => writer,
            Err(e) => {
                return Ok(
                    json!({"status":"refused","code":"test_log_unwritable","run_id":config.run_id,"path":config.jsonl_path,"message":e.to_string()}),
                );
            }
        };
        // Release completed writers but let an earlier closed run retry outstanding
        // appends on its own file even after a successor claims the lane.
        for old in state.closed.values_mut() {
            let result = old.result();
            if result["pending_records"] == 0 {
                old.result = result;
                old.pending_writer = None;
            }
        }
        let id = config.run_id.clone();
        let mut run = Run::new(config, journal, provenance);
        run.append(
            "begin-test-log",
            json!({"settings":run.config,"sampling_algorithm":"fnv1a64-v1"}),
        );
        run.last_activity = Utc::now();
        let weak = Arc::downgrade(d);
        let worker_id = id.clone();
        std::thread::Builder::new()
            .name("test-log-lifecycle".into())
            .spawn(move || loop {
                let Some(domain) = weak.upgrade() else { break };
                let owner = domain.test_log.clone();
                let mut guard = owner.state.lock().unwrap();
                let Some(run) = guard.run.as_mut().filter(|r| r.config.run_id == worker_id) else {
                    break;
                };
                let now = Instant::now();
                run.tick(&domain, now);
                let delay = run.next_wake(Instant::now());
                drop(domain);
                drop(owner.changed.wait_timeout(guard, delay).unwrap());
            })
            .map_err(|e| e.to_string())?;
        let health = run.journal.status()["test_log_health"].clone();
        if let Some(mut previous) = state.run.replace(run) {
            previous.flush_late_counts(Instant::now());
            let result = previous.close_result();
            let pending_writer = (result["pending_records"] != 0).then_some(previous.journal);
            state.closed.insert(
                previous.config.run_id,
                ClosedRun {
                    result,
                    pending_writer,
                },
            );
        }
        self.observing.store(true, Ordering::Release);
        self.changed.notify_all();
        Ok(json!({"status":"accepted","run_id":id,"test_log_health":health}))
    }

    pub(crate) fn close_result(&self, id: &str) -> Option<Value> {
        let state = self.state.lock().unwrap();
        state.closed.get(id).map(ClosedRun::result).or_else(|| {
            state
                .run
                .as_ref()
                .filter(|r| r.config.run_id == id)
                .map(Run::close_result)
        })
    }
    pub fn observe_log(&self, record: &LogEntry) {
        if self.observing.load(Ordering::Acquire) {
            if let Some(r) = self.state.lock().unwrap().run.as_mut() {
                r.observe_log(record)
            }
        }
    }
    pub fn observe_span(&self, record: &SpanEntry) {
        if self.observing.load(Ordering::Acquire) {
            if let Some(r) = self.state.lock().unwrap().run.as_mut() {
                r.observe_span(record)
            }
        }
    }
}

#[cfg(test)]
mod tests;
