use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{bail, Context};
use fs2::FileExt;

use logmon_broker_core::daemon::persistence::config_dir;

/// Locate the `logmon-broker` binary using a three-tier lookup:
/// 1. `LOGMON_BROKER_BIN` env var
/// 2. `which logmon-broker` (any directory on `$PATH`)
/// 3. Sibling of the current executable (e.g., when both binaries live in
///    the same `target/debug/` directory during development)
fn locate_broker_binary() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("LOGMON_BROKER_BIN") {
        return Ok(p.into());
    }
    if let Ok(p) = which::which("logmon-broker") {
        return Ok(p);
    }
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join("logmon-broker");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "logmon-broker not found. Tried: \
         (1) LOGMON_BROKER_BIN env var, \
         (2) `which logmon-broker` (any directory on $PATH), \
         (3) sibling of current executable. \
         Install via 'cargo install --path crates/broker', \
         set LOGMON_BROKER_BIN to an absolute path, \
         or run 'logmon-broker install-service' to register it as a system service."
    )
}

/// Ensure the broker is running, starting it if necessary.
///
/// Acquires an exclusive file lock on `daemon.lock` to prevent races between
/// concurrent shim instances. On return, the broker has been verified to be
/// listening on its socket — but this function does NOT return a connection;
/// the caller (typically the SDK's `Broker::connect`) opens its own.
pub async fn ensure_broker_running() -> anyhow::Result<()> {
    // **An explicitly named socket means "use that broker".** The SDK reads
    // `LOGMON_BROKER_SOCKET` (`sdk/src/connect.rs:96`) and connects there, but
    // this function works off `config_dir()` — so without this check, pointing
    // the shim at one broker makes it start a SECOND one somewhere else, and
    // then talk to neither the one it started nor the one it was told about.
    if std::env::var_os("LOGMON_BROKER_SOCKET").is_some() {
        tracing::debug!("LOGMON_BROKER_SOCKET is set; not starting a broker");
        return Ok(());
    }

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

    let result = try_start_or_verify(&dir).await;

    // Always release the lock, even on error.
    let _ = lock_file.unlock();

    result
}

/// Verify a running broker (probing its socket) or spawn a new one and
/// poll until its socket becomes connectable.
async fn try_start_or_verify(dir: &Path) -> anyhow::Result<()> {
    let pid_path = dir.join("daemon.pid");

    // If a PID file exists, check whether the process is still alive.
    if pid_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if is_process_alive(pid) {
                    // Process is alive -- probe the socket.
                    if probe_socket(dir).await.is_ok() {
                        return Ok(());
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
    start_broker(dir)?;

    // Poll for the socket to appear.
    wait_for_socket(dir).await
}

/// Probe the broker socket to verify it accepts connections. Returns Ok on
/// success and discards the connection — the SDK opens its own.
async fn probe_socket(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let socket_path = dir.join("logmon.sock");
        tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket: {}",
                    socket_path.display()
                )
            })?;
    }
    #[cfg(windows)]
    {
        let _ = dir;
        tokio::net::TcpStream::connect("127.0.0.1:12200")
            .await
            .context("failed to connect to daemon on 127.0.0.1:12200")?;
    }
    Ok(())
}

/// Remove stale PID file and socket.
fn cleanup_stale_files(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("daemon.pid"));
    #[cfg(unix)]
    let _ = std::fs::remove_file(dir.join("logmon.sock"));
}

/// Start the broker as a detached child process.
///
/// The broker binary is invoked with no subcommand — `logmon-broker` runs
/// the daemon by default. (Status / install-service modes use explicit
/// subcommands.)
///
/// Refuses when `LOGMON_CONFIG_DIR` relocated the state directory and that
/// directory carries no `config.json`. The variable moves **state, not ports**,
/// and this is the one path that starts a broker on the user's behalf — with no
/// way to pass port flags. Auto-spawning there binds the *default* ports, so it
/// collides with whatever already holds them (typically the user's live
/// service), exits 1, and leaves the caller polling a socket that will never
/// appear. A directory with its own `config.json` has taken responsibility for
/// its ports and is allowed through.
fn start_broker(dir: &Path) -> anyhow::Result<()> {
    if std::env::var_os("LOGMON_CONFIG_DIR").is_some_and(|v| !v.is_empty())
        && !dir.join("config.json").exists()
    {
        anyhow::bail!(
            "refusing to auto-start a broker in {}: LOGMON_CONFIG_DIR relocated the state \
             directory, but that variable moves state and NOT ports — a broker started here \
             would bind the default ports and collide with whatever already holds them. \
             Either start it yourself with explicit ports:\n\
             \n    LOGMON_CONFIG_DIR={} logmon-broker --gelf-port 0 --otlp-grpc-port 0 \
             --otlp-http-port 0\n\n\
             or put a config.json with non-conflicting ports in that directory, which also \
             lets this auto-start work.",
            dir.display(),
            dir.display()
        );
    }
    let bin = locate_broker_binary()?;
    // stderr to a file, never to /dev/null: this child can fail for reasons
    // only it can see (a port clash, a too-long socket path), and discarding
    // them turns every such failure into an unexplained ten-second timeout.
    let log = std::fs::File::create(dir.join("autostart.log")).ok();
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    match log {
        Some(f) => {
            cmd.stderr(f);
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    cmd.spawn().context("failed to spawn broker process")?;
    Ok(())
}

/// Poll until the broker socket appears and is connectable, or timeout.
async fn wait_for_socket(dir: &Path) -> anyhow::Result<()> {
    let poll_interval = tokio::time::Duration::from_millis(100);
    let timeout = tokio::time::Duration::from_secs(10);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out waiting for daemon socket to become available in {}; the broker's \
             own stderr is in {}",
                dir.display(),
                dir.join("autostart.log").display()
            );
        }

        if probe_socket(dir).await.is_ok() {
            return Ok(());
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
