//! One bounded writer per run. A failed partial write is retried from the
//! previous complete line; readers can use all complete lines in the meantime.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const MAX_RECORDS: usize = 4096;
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// A closed run can still be retrying a failed write when its successor opens.
/// Account for both queues against the same per-domain allowance.
#[derive(Default)]
pub(super) struct QueueBudget {
    used: Mutex<(usize, usize)>, // records and encoded bytes
}

impl QueueBudget {
    fn reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.lock().unwrap();
        if used.0 >= MAX_RECORDS || bytes > MAX_BYTES.saturating_sub(used.1) {
            return false;
        }
        used.0 += 1;
        used.1 += bytes;
        true
    }

    fn release(&self, records: usize, bytes: usize) {
        let mut used = self.used.lock().unwrap();
        used.0 -= records;
        used.1 -= bytes;
    }
}

#[derive(Default)]
struct State {
    budget: Arc<QueueBudget>,
    queue: VecDeque<Arc<[u8]>>,
    bytes: usize,
    lost: u64,
    gap_reported: u64,
    first_error: Option<String>,
    last_error: Option<String>,
    degraded: bool,
    stopping: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        self.budget.release(self.queue.len(), self.bytes);
    }
}

type Shared = Arc<(Mutex<State>, Condvar)>;

pub(super) struct Journal {
    pub path: PathBuf,
    shared: Shared,
}

impl Journal {
    pub fn open(path: &Path, budget: Arc<QueueBudget>) -> io::Result<Self> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "test-log needs an absolute file path",
            ));
        }
        // The nightly owns the run directory. Never overwrite another run.
        let file = OpenOptions::new().create_new(true).write(true).open(path)?;
        Self::from_file(path, file, budget)
    }

    fn from_file(path: &Path, file: File, budget: Arc<QueueBudget>) -> io::Result<Self> {
        let mut state = State::default();
        state.budget = budget;
        let shared = Arc::new((Mutex::new(state), Condvar::new()));
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("test-log-writer".into())
            .spawn(move || write_loop(file, worker))?;
        Ok(Self {
            path: path.into(),
            shared,
        })
    }

    pub fn append(&self, record: Value) {
        let mut bytes = serde_json::to_vec(&record).expect("JSON value serializes");
        bytes.push(b'\n');
        let mut s = self.shared.0.lock().unwrap();
        if !s.budget.reserve(bytes.len()) {
            s.lost += 1;
            self.shared.1.notify_one();
            return;
        }
        s.bytes += bytes.len();
        s.queue.push_back(bytes.into());
        self.shared.1.notify_one();
    }

    pub fn status(&self) -> Value {
        let s = self.shared.0.lock().unwrap();
        let pending = s.queue.len() + usize::from(s.gap_reported < s.lost);
        let persistence = if s.lost != 0 {
            "incomplete"
        } else if pending != 0 {
            "degraded"
        } else {
            "complete"
        };
        json!({"path":self.path, "persistence":persistence, "pending_records":pending,
            "lost_records":s.lost,"first_write_error":s.first_error,"latest_write_error":s.last_error,
            "test_log_health":if s.degraded || s.lost != 0 {"degraded"} else {"healthy"}})
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap().stopping = true;
        self.shared.1.notify_one();
    }
}

fn write_loop(mut file: File, shared: Shared) {
    let mut position = 0;
    let mut partial = false;
    loop {
        let (bytes, gap) = {
            let mut s = shared.0.lock().unwrap();
            while s.queue.is_empty() && s.gap_reported == s.lost && !s.stopping {
                s = shared.1.wait(s).unwrap();
            }
            if let Some(front) = s.queue.front() {
                (front.clone(), None)
            } else if s.gap_reported < s.lost {
                let mut bytes =
                    serde_json::to_vec(&json!({"event":"test-log-gap","lost_records":s.lost}))
                        .unwrap();
                bytes.push(b'\n');
                (bytes.into(), Some(s.lost))
            } else {
                return;
            }
        };
        let result = (|| {
            if partial {
                file.set_len(position)?;
                file.seek(SeekFrom::Start(position))?;
            }
            file.write_all(&bytes)
        })();
        let mut s = shared.0.lock().unwrap();
        match result {
            Ok(()) => {
                partial = false;
                position += bytes.len() as u64;
                if let Some(lost) = gap {
                    s.gap_reported = lost;
                } else {
                    s.queue.pop_front();
                    s.bytes -= bytes.len();
                    s.budget.release(1, bytes.len());
                }
                s.degraded = false;
            }
            Err(err) => {
                partial = true;
                s.degraded = true;
                let msg = err.to_string();
                s.first_error.get_or_insert_with(|| msg.clone());
                s.last_error = Some(msg);
                if s.stopping {
                    return;
                }
                drop(s);
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn successive_unwritable_runs_share_the_domain_byte_budget() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let budget = Arc::new(QueueBudget::default());
        let open = || {
            Journal::from_file(
                temp.path(),
                File::open(temp.path()).unwrap(),
                budget.clone(),
            )
            .unwrap()
        };
        let first = open();
        let second = open();
        let row = json!({"event":"end-test"});
        let bytes = serde_json::to_vec(&row).unwrap().len() + 1;
        assert!(budget.reserve(MAX_BYTES - bytes));
        first.append(row.clone());
        second.append(row.clone());
        assert_eq!(first.status()["lost_records"], 0);
        assert_eq!(second.status()["lost_records"], 1);
        budget.release(1, MAX_BYTES - bytes);
        second.append(row);
        assert_eq!(second.status()["lost_records"], 1);
    }

    #[test]
    fn write_failure_does_not_stall_acks_and_reports_pending_rows() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let file = File::open(temp.path()).unwrap(); // read-only descriptor forces append failure
        let journal = Journal::from_file(temp.path(), file, Arc::default()).unwrap();
        journal.append(json!({"event":"end-test"}));
        let limit = Instant::now() + Duration::from_secs(2);
        loop {
            let status = journal.status();
            if status["test_log_health"] == "degraded" {
                assert_eq!(status["pending_records"], 1);
                assert_eq!(status["lost_records"], 0);
                assert_eq!(status["persistence"], "degraded");
                assert!(status["first_write_error"].is_string());
                break;
            }
            assert!(Instant::now() < limit);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
