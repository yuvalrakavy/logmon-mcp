//! V11, finally out-of-process: a collector's definition and history survive a
//! real `SIGKILL`, not a graceful shutdown.
//!
//! §12's objection to testing this with `restart()` was that a graceful path
//! writes on the way down, so it passes regardless of whether the files were
//! already complete. Phase 3 answered with a stricter in-process test (the file
//! is asserted on disk *while the daemon runs*), but nothing ever exercised the
//! actual sequence a user hits: process killed cold, new process, same
//! directory. This does, using the two pieces that were missing:
//! `LOGMON_CONFIG_DIR` for cross-process redirection, and
//! `CARGO_BIN_EXE_logmon-broker` for the real binary.
//!
//! **Safety**: everything runs against a tempdir. The spawned daemons get
//! `--gelf-port 0` (kernel-assigned) and both OTLP ports 0 (disabled), so they
//! can never collide with a developer's live daemon — and the live daemon's
//! own config dir is never read or written, because the child's environment
//! carries the override and this test's environment does not.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// A spawned broker that cannot outlive the test. `Drop` kills it, so a failed
/// assertion cannot leak a process that would go on holding the socket.
struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    fn spawn(dir: &Path) -> Self {
        // Output goes to a file in the tempdir, never to /dev/null: a harness
        // that discards the child's diagnostic is a harness that cannot be
        // debugged, and this one's whole job is to survive a hostile shutdown.
        let log = std::fs::File::create(dir.join("spawn.log")).expect("create spawn.log");
        let child = Command::new(env!("CARGO_BIN_EXE_logmon-broker"))
            // The override rides on the CHILD's environment. Setting it via
            // `std::env::set_var` in the test process instead would race every
            // concurrently-running test that reads the environment.
            .env("LOGMON_CONFIG_DIR", dir)
            .args([
                "--gelf-port",
                "0",
                "--otlp-grpc-port",
                "0",
                "--otlp-http-port",
                "0",
            ])
            .stdout(log.try_clone().expect("dup"))
            .stderr(log)
            .spawn()
            .expect("spawn logmon-broker");
        let mut d = Daemon {
            child,
            dir: dir.to_path_buf(),
        };
        d.await_ready();
        d
    }

    /// A daemon spawned WITHOUT waiting for readiness, logging to `log_name`.
    ///
    /// For the case where refusing to start is the expected outcome — waiting
    /// for readiness there would just burn the deadline. Still a `Daemon`, so
    /// `Drop` cannot leak it if an assertion fires first.
    fn spawn_unchecked(dir: &Path, log_name: &str) -> Self {
        let log = std::fs::File::create(dir.join(log_name)).expect("create log");
        let child = Command::new(env!("CARGO_BIN_EXE_logmon-broker"))
            .env("LOGMON_CONFIG_DIR", dir)
            .args([
                "--gelf-port",
                "0",
                "--otlp-grpc-port",
                "0",
                "--otlp-http-port",
                "0",
            ])
            .stdout(log.try_clone().expect("dup"))
            .stderr(log)
            .spawn()
            .expect("spawn logmon-broker");
        Daemon {
            child,
            dir: dir.to_path_buf(),
        }
    }

    fn spawn_log(&self) -> String {
        self.diagnostics("spawn.log")
    }

    fn log_named(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    /// Everything the child could have told us, labelled.
    ///
    /// The captured stream is EMPTY on a healthy start: the daemon installs a
    /// rolling file appender and logs to `daemon.log.<date>`, so stderr carries
    /// only what escapes before that — a panic, or a `main` that returns Err.
    /// Reporting the stream alone therefore says nothing on the timeout path,
    /// which is exactly where the diagnostic is needed.
    fn diagnostics(&self, stream_name: &str) -> String {
        let stream = self.log_named(stream_name);
        let daemon_logs: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("daemon.log"))
            .collect();
        let daemon = daemon_logs
            .iter()
            .map(|n| format!("--- {n} ---\n{}", self.log_named(n)))
            .collect::<Vec<_>>()
            .join("\n");
        let listing: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        format!(
            "--- {stream_name} (child stderr; empty is NORMAL, the daemon logs to a file) ---\n\
             {stream}\n{daemon}\n--- dir contents: {listing:?} ---"
        )
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("logmon.sock")
    }

    /// Readiness is **a connect that succeeds**, not a socket file that exists.
    ///
    /// The file appears when the daemon binds and *survives the process*: a
    /// Unix socket is not unlinked on exit, so a daemon that bound and then
    /// died leaves a file that every later `connect` answers with
    /// `ECONNREFUSED`. Waiting on the file therefore reports ready for the one
    /// state this test most needs to distinguish — which is exactly how it
    /// failed first time out.
    ///
    /// Also polls the child: a daemon that exited is reported as that, with its
    /// log, instead of timing out with no explanation.
    ///
    /// **The deadline is generous because the FIRST exec of a freshly linked
    /// binary is slow, and that is measured, not guessed.** Instrumented on this
    /// machine: first spawn 6.2 s / 117 polls, second spawn of the same image
    /// 52 ms / 2 polls, and 753 ms / 15 polls on a later run with the binary
    /// warm. The daemon's own log shows it reaching "listening" 13 ms after
    /// start, so none of that is startup work — it is macOS validating and
    /// caching a newly written ~100 MB debug binary, and it lands on whichever
    /// test happens to exec it first. Under `cargo test --workspace
    /// --all-features` the binary is rebuilt AND the machine is running dozens
    /// of test binaries, which is how a 20 s deadline failed both tests here
    /// while each passed alone.
    ///
    /// A slow pass costs seconds; a false failure costs a debugging session.
    /// The check is a poll, so a warm start still returns in ~50 ms.
    fn await_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "daemon exited before becoming ready ({status}); log:\n{}",
                    self.spawn_log()
                );
            }
            if std::os::unix::net::UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon never accepted a connection on {} within 20s; log:\n{}",
                self.socket().display(),
                self.spawn_log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL — the point of the whole file. No shutdown path runs.
    ///
    /// Takes `&mut self` and lets `Drop` run normally. An earlier version
    /// `mem::forget`'d to avoid "double-killing a reaped child", which std
    /// forecloses: `Child::kill` returns `Ok(())` once a status is cached
    /// (`if self.status.is_some() { return Ok(()) }`), precisely so a pid that
    /// may have been recycled is never signalled. `Child` has no `Drop` of its
    /// own, so nothing went unreaped — the `forget` leaked the `PathBuf`, and
    /// would have leaked anything owning that `Daemon` later gained.
    fn kill_dash_nine(&mut self) {
        self.child.kill().expect("SIGKILL");
        self.child.wait().expect("reap");
        // The socket file survives a kill -9 (nothing unlinked it). The next
        // daemon must cope with that, which is part of what restarting into the
        // same directory exercises.
    }
}

impl Drop for Daemon {
    /// **The SIGKILL here is load-bearing, not merely expedient.** A graceful
    /// shutdown reaches `send_otel_beacon("OTEL:OFFLINE")`, which multicasts to
    /// 239.255.77.1:4399 unconditionally — no check that this daemon ever
    /// started an OTLP receiver. Switching this to SIGTERM would make
    /// `cargo test` broadcast "collector offline" to every app on the
    /// developer's machine that uses the tracing-init circuit breaker.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn client(sock: PathBuf, session: &str) -> logmon_broker_sdk::Broker {
    match logmon_broker_sdk::Broker::connect()
        .socket_path(sock.clone())
        .session_name(session)
        .call_timeout(Duration::from_secs(10))
        .open()
        .await
    {
        Ok(b) => b,
        Err(e) => panic!(
            "connect to spawned daemon at {} failed: {e}   (socket exists: {}, path len: {})",
            sock.display(),
            sock.exists(),
            sock.as_os_str().len()
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_collector_and_its_history_survive_a_real_kill_dash_nine() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path().to_path_buf();

    // ---- first incarnation: arm, record two runs, die cold ----------------
    let mut first = Daemon::spawn(&dir);
    {
        let b = client(first.socket(), "kill9").await;
        b.call(
            "collectors.add",
            serde_json::json!({
                "name": "surviving",
                "filter": "sv=kill9-svc",
                "level": "tree",
                "threshold": {
                    "metric": "count", "op": "gt", "value": 100.0, "window_ms": 5000
                },
            }),
        )
        .await
        .expect("armed");
        // Two labelled runs, so restart must restore BOTH the history and the
        // auto-label counter (a reused `snapshot-N` after restart is the
        // ambiguity §6.2's never-reuse rule exists to prevent).
        for label in ["before", "after"] {
            b.call(
                "collectors.snapshot",
                serde_json::json!({ "name": "surviving", "label": label }),
            )
            .await
            .expect("snapshot");
        }
        b.call(
            "collectors.snapshot",
            serde_json::json!({ "name": "surviving" }),
        )
        .await
        .expect("auto-labelled snapshot");
    }
    first.kill_dash_nine();
    drop(first);

    // ---- second incarnation: same directory, cold socket file -------------
    let second_d = Daemon::spawn(&dir);
    let b = client(second_d.socket(), "kill9").await;

    let listed = b
        .call("collectors.list", serde_json::json!({}))
        .await
        .expect("list");
    let c = &listed["collectors"][0];
    assert_eq!(c["name"], "surviving", "{listed}");
    assert_eq!(c["filter"], "sv=kill9-svc");
    assert_eq!(
        c["zeroed_by"], "daemon_restart",
        "an empty window must say WHY it is empty: {c}"
    );
    assert_eq!(c["snapshots"], 3, "both labelled runs plus the auto one");
    assert_eq!(
        c["threshold"]["metric"], "count",
        "the guard survives with the definition: {c}"
    );
    assert_eq!(
        c["threshold"]["evaluated"], false,
        "but its rolling window does not — a restored collector is zeroed"
    );

    let history = b
        .call(
            "collectors.history",
            serde_json::json!({ "name": "surviving" }),
        )
        .await
        .expect("history");
    let labels: Vec<&str> = history["snapshots"]
        .as_array()
        .expect("snapshots")
        .iter()
        .map(|s| s["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["before", "after", "snapshot-1"], "{history}");

    // The auto-label counter came back too: the next automatic label continues
    // the sequence rather than re-issuing snapshot-1 for a different run.
    let next = b
        .call(
            "collectors.snapshot",
            serde_json::json!({ "name": "surviving" }),
        )
        .await
        .expect("post-restart snapshot");
    assert_eq!(next["label"], "snapshot-2", "{next}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_directory_is_refused_while_the_first_lives() {
    // Cross-process coverage the in-process suite cannot have. Named for the
    // mechanism that actually excludes: the daemon's stale-pid sweep reads
    // `daemon.pid` and asks whether that process is alive. `daemon.lock` is
    // flocked only by the MCP shim's auto-start, never by the daemon — an
    // earlier name pointed a maintainer at the wrong file.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let first = Daemon::spawn(tmp.path());

    // Wrapped in `Daemon` so the timeout path below cannot leak a live broker
    // holding ephemeral ports and rooted at a directory `TempDir` then deletes.
    let mut second = Daemon::spawn_unchecked(tmp.path(), "second.log");

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = second.child.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "a second daemon neither exited nor was refused within 15s; its log:\n{}",
            second.diagnostics("second.log")
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(
        !status.success(),
        "a second daemon on a live directory must refuse to start; its log:\n{}",
        second.diagnostics("second.log")
    );
    // The REASON, not just the failure. `!status.success()` alone would read
    // green if the second daemon died for any unrelated cause — a future
    // startup step failing in a tempdir, a sandbox blocking the ephemeral bind,
    // `kill` missing from PATH — and that green would be recorded as "the
    // exclusion invariant holds" when it holds for no reason at all.
    let log = second.diagnostics("second.log");
    assert!(
        log.contains("another broker is already running"),
        "expected the refusal to name the running broker; got:\n{log}"
    );

    drop(second);
    drop(first);
}
