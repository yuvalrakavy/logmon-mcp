use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use fs2::FileExt;

use crate::daemon::persistence::config_dir;

/// Holds the connection to the daemon process.
pub struct DaemonConnection {
    #[cfg(unix)]
    pub stream: tokio::net::UnixStream,
    #[cfg(windows)]
    pub stream: tokio::net::TcpStream,
}

/// Connect to the running daemon, starting it if necessary.
///
/// Acquires an exclusive file lock on `daemon.lock` to prevent race conditions
/// when multiple shim instances attempt to start the daemon simultaneously.
pub async fn connect_to_daemon() -> anyhow::Result<DaemonConnection> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir: {}", dir.display()))?;

    let lock_path = dir.join("daemon.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file: {}", lock_path.display()))?;

    lock_file
        .lock_exclusive()
        .context("failed to acquire daemon lock")?;

    let result = try_connect_or_start(&dir).await;

    // Always release the lock, even on error.
    let _ = lock_file.unlock();

    result
}

/// Try to connect to an existing daemon or start a new one.
async fn try_connect_or_start(dir: &Path) -> anyhow::Result<DaemonConnection> {
    let pid_path = dir.join("daemon.pid");

    // If a PID file exists, check whether the process is still alive.
    if pid_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    // Process is alive -- try connecting to its socket.
                    if let Ok(conn) = try_connect(dir).await {
                        return Ok(conn);
                    }
                    // Socket might not be ready yet; fall through to polling.
                } else {
                    // Stale PID file -- clean up.
                    cleanup_stale_files(dir);
                }
            } else {
                // Malformed PID file -- clean up.
                cleanup_stale_files(dir);
            }
        }
    }

    // No running daemon -- start one.
    start_daemon()?;

    // Poll for the socket to appear.
    wait_for_socket(dir).await
}

/// Attempt to connect to the daemon socket.
async fn try_connect(dir: &Path) -> anyhow::Result<DaemonConnection> {
    #[cfg(unix)]
    {
        let socket_path = dir.join("logmon.sock");
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket: {}",
                    socket_path.display()
                )
            })?;
        Ok(DaemonConnection { stream })
    }
    #[cfg(windows)]
    {
        let stream = tokio::net::TcpStream::connect("127.0.0.1:12200")
            .await
            .context("failed to connect to daemon on 127.0.0.1:12200")?;
        Ok(DaemonConnection { stream })
    }
}

/// Remove stale PID file and socket.
fn cleanup_stale_files(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("daemon.pid"));
    #[cfg(unix)]
    let _ = std::fs::remove_file(dir.join("logmon.sock"));
}

/// Start the daemon as a detached child process.
fn start_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("failed to determine current executable path")?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn daemon process")?;
    Ok(())
}

/// Poll until the daemon socket appears and is connectable, or timeout.
async fn wait_for_socket(dir: &Path) -> anyhow::Result<DaemonConnection> {
    let poll_interval = tokio::time::Duration::from_millis(100);
    let timeout = tokio::time::Duration::from_secs(10);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for daemon socket to become available");
        }

        if let Ok(conn) = try_connect(dir).await {
            return Ok(conn);
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Check whether a process with the given PID is alive.
///
/// Uses `kill -0` which sends no signal but checks process existence.
fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        // On Windows, use tasklist to check if the process exists.
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Returns the path to the daemon socket.
#[allow(dead_code)]
pub fn socket_path() -> PathBuf {
    config_dir().join("logmon.sock")
}

/// Returns the path to the daemon PID file.
#[allow(dead_code)]
pub fn pid_path() -> PathBuf {
    config_dir().join("daemon.pid")
}
