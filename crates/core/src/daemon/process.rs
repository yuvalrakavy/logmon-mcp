//! Cross-platform process-liveness probe.
//!
//! Shared between the broker binary's `status` subcommand and the daemon's
//! startup-time stale-pid sweep. Kept dependency-free (just `std::process`)
//! so it works on every supported target without pulling in a libc shim.

/// Returns `true` if the kernel reports a process with the given pid is
/// currently alive (and we have permission to signal it).
///
/// Implementation:
/// - Unix: `kill -0 <pid>` — exit 0 iff the process exists and we can
///   signal it. Doesn't actually deliver a signal.
/// - Windows: `tasklist /FI "PID eq <pid>" /NH` — exit 0 iff the process
///   exists.
///
/// This is a best-effort probe used to decide whether a stale `daemon.pid`
/// file should be removed. False positives (a recycled pid that now belongs
/// to an unrelated process) are rare in practice but possible — the design
/// accepts that, since the alternative (refusing to start when a stale
/// pid file's number happens to be alive) is strictly worse for ops.
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
