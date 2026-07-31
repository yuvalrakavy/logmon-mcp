use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const SEQ_BLOCK_SIZE: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    #[serde(default)]
    pub triggers: Vec<PersistedTrigger>,
    #[serde(default)]
    pub filters: Vec<PersistedFilter>,
    /// Caller-supplied identity blob from the most recent `session.start`.
    /// `#[serde(default)]` so existing `state.json` files (written before
    /// client_info landed) load with `None`.
    #[serde(default)]
    pub client_info: Option<serde_json::Value>,
    /// Tolerant: deserialize errors → empty vec + WARN log. Old-shape entries
    /// (with `timestamp` instead of `seq`) are silently dropped on load so a
    /// pre-cursor state.json doesn't crash the daemon.
    #[serde(default, deserialize_with = "deserialize_bookmarks_lenient")]
    pub bookmarks: Vec<PersistedBookmark>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBookmark {
    /// Bare bookmark name (NOT the qualified `session/name` form). The owning
    /// session is implicit in [`PersistedSession`]'s map key.
    pub name: String,
    /// Anchor seq the bookmark was created at.
    pub seq: u64,
    /// Wall-clock creation time, retained for human display only.
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Lenient deserializer: on any error, log a WARN and return an empty vec.
/// This is the migration path for pre-cursor `state.json` files whose
/// bookmark entries used `timestamp` instead of `seq`.
fn deserialize_bookmarks_lenient<'de, D>(
    deserializer: D,
) -> Result<Vec<PersistedBookmark>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    match serde_json::from_value::<Vec<PersistedBookmark>>(raw) {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "discarding bookmarks from previous-version state.json"
            );
            Ok(Vec::new())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTrigger {
    pub filter: String,
    pub pre_window: u32,
    pub post_window: u32,
    pub notify_context: u32,
    pub description: Option<String>,
    /// `#[serde(default)]` so existing `state.json` files (written before
    /// oneshot landed) load with `false`.
    #[serde(default)]
    pub oneshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedFilter {
    pub filter: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonState {
    #[serde(default)]
    pub seq_block: u64,
    #[serde(default)]
    pub named_sessions: HashMap<String, PersistedSession>,
}

/// A durable, user-declared domain from `config.json`. Re-created at boot with
/// its declared ports; buffers start empty and seq at 0 (like `default` — domain
/// data is never persisted). Ports mirror `domains.create`: omitted →
/// auto-allocate; `Some(0)` → disable that receiver; `Some(n)` → bind exactly
/// `n`. NOTE: omitted ports re-allocate on every restart, so declare explicit
/// ports for a domain an external producer targets by a fixed port.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDomain {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gelf_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_grpc_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_http_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_buffer_size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_buffer_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_gelf_port")]
    pub gelf_port: u16,
    #[serde(default)]
    pub gelf_udp_port: Option<u16>,
    #[serde(default)]
    pub gelf_tcp_port: Option<u16>,
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
    #[serde(default)]
    pub persist_buffer_on_exit: bool,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_otlp_grpc_port")]
    pub otlp_grpc_port: u16,
    #[serde(default = "default_otlp_http_port")]
    pub otlp_http_port: u16,
    #[serde(default = "default_span_buffer_size")]
    pub span_buffer_size: usize,
    /// Cap on the number of API-created domains (ephemeral + persistent).
    /// `config`-declared domains and the implicit `default` are always honored
    /// and do NOT count against this. `domains.create` refuses once the cap is
    /// reached.
    #[serde(default = "default_max_domains")]
    pub max_domains: usize,
    /// Durable, user-declared domains re-created at boot (declarations-only:
    /// empty buffers, fresh seq). Empty by default; an old `config.json` without
    /// the key loads as `[]`.
    #[serde(default)]
    pub domains: Vec<ConfigDomain>,
    /// A domain is reported `stale` in `domains.list` once it has been idle (no
    /// log/span received) longer than this many seconds. Tune to your workload's
    /// cadence; `idle_secs` is always reported raw regardless of this. (#2.)
    #[serde(default = "default_stale_after_secs")]
    pub stale_after_secs: u64,
    /// A session that stays DISCONNECTED longer than this many seconds is
    /// disposed by the periodic sweep. Connected sessions never expire — TTL
    /// measures abandonment, not lifetime. Keeps unique-per-conversation
    /// session names (meaningful-session-names spec, 2026-07-17) from
    /// accumulating forever.
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,
}

fn default_stale_after_secs() -> u64 {
    60
}
fn default_session_ttl_secs() -> u64 {
    86_400 // 24 h
}
fn default_gelf_port() -> u16 {
    12201
}
fn default_buffer_size() -> usize {
    10000
}
fn default_idle_timeout() -> u64 {
    1800
}
fn default_otlp_grpc_port() -> u16 {
    4317
}
fn default_otlp_http_port() -> u16 {
    4318
}
fn default_span_buffer_size() -> usize {
    10000
}
fn default_max_domains() -> usize {
    32
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            gelf_port: 12201,
            gelf_udp_port: None,
            gelf_tcp_port: None,
            buffer_size: 10000,
            persist_buffer_on_exit: false,
            idle_timeout_secs: 1800,
            otlp_grpc_port: 4317,
            otlp_http_port: 4318,
            span_buffer_size: 10000,
            max_domains: 32,
            domains: Vec::new(),
            stale_after_secs: 60,
            session_ttl_secs: 86_400,
        }
    }
}

/// Suffix for the temp file an atomic write goes through. A crash between
/// write and rename leaves one behind; [`sweep_temp_files`] clears them at
/// boot.
pub const TEMP_SUFFIX: &str = ".tmp-write";

/// Write a file so a reader never sees it half-written.
///
/// Temp file **in the same directory** (a rename across filesystems is a copy,
/// which is not atomic), `fsync` the file so its contents are durable before
/// anything points at them, `rename` — atomic on POSIX — then `fsync` the
/// *directory* so the rename itself survives a power loss. Skipping the last
/// step is the common version of this bug: the data is durable but the name
/// still points at the old inode.
pub fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("path {} has no file name", path.display()))?
        .to_string_lossy()
        .to_string();
    let tmp = parent.join(format!("{file_name}{TEMP_SUFFIX}"));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Best-effort: some filesystems refuse to open a directory for sync, and
    // failing the whole write over that would be worse than the lost
    // guarantee.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Remove temp files left by a crash mid-write.
///
/// The existing boot sweep clears `daemon.pid` and the socket; without this,
/// an interrupted write leaks a file per crash forever.
pub fn sweep_temp_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(TEMP_SUFFIX)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Move an unreadable file aside so the next boot is not blocked by it.
///
/// Returns where it went. Renamed rather than deleted: it is the only evidence
/// of whatever went wrong, and a daemon that silently destroys the file it
/// could not parse leaves nothing to diagnose.
pub(crate) fn quarantine(path: &Path) -> Option<std::path::PathBuf> {
    let base = format!("{}.corrupt", path.display());
    for n in 0..10 {
        let candidate = if n == 0 {
            std::path::PathBuf::from(&base)
        } else {
            std::path::PathBuf::from(format!("{base}.{n}"))
        };
        if !candidate.exists() {
            return std::fs::rename(path, &candidate).ok().map(|_| candidate);
        }
    }
    // Ten corruptions deep, stop accumulating and reuse the first slot.
    let last = std::path::PathBuf::from(&base);
    std::fs::rename(path, &last).ok().map(|_| last)
}

/// Load persisted state, surviving a file that cannot be parsed.
///
/// **Never returns an error for a corrupt file.** `state.json` is written on a
/// path that can be interrupted by a crash or a full disk, and propagating a
/// parse error meant the daemon refused to start until a human found and
/// deleted the file by hand — turning a recoverable loss of session bookkeeping
/// into an outage of the whole broker. The file is moved aside, the loss is
/// logged loudly, and the daemon comes up empty.
pub fn load_state(path: &Path) -> anyhow::Result<DaemonState> {
    if !path.exists() {
        return Ok(DaemonState::default());
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                path = %path.display(), error = %e,
                "could not read persisted state; starting with none"
            );
            return Ok(DaemonState::default());
        }
    };
    match serde_json::from_str(&content) {
        Ok(state) => Ok(state),
        Err(e) => {
            let moved = quarantine(path);
            tracing::error!(
                path = %path.display(),
                error = %e,
                quarantined_to = ?moved,
                "persisted state is unreadable and has been moved aside; named sessions, \
                 their filters, triggers and bookmarks are lost, but the daemon is starting"
            );
            Ok(DaemonState::default())
        }
    }
}

pub fn save_state(path: &Path, state: &DaemonState) -> anyhow::Result<()> {
    let content = serde_json::to_vec_pretty(state)?;
    atomic_write(path, &content)
}

pub fn load_config(path: &Path) -> anyhow::Result<DaemonConfig> {
    if !path.exists() {
        return Ok(DaemonConfig::default());
    }
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

/// Returns the logmon config directory: `$LOGMON_CONFIG_DIR`, else
/// `~/.config/logmon/`.
///
/// The env var exists for **cross-process** redirection — a real `kill -9`
/// harness spawning the actual binary, or a second daemon run beside a
/// developer's live one. Precedence is `DaemonOverrides.config_dir` > env >
/// default; note it is that ONE field that isolates, not `DaemonOverrides` as a
/// whole — `socket_path` alone moves the socket while state still lands here.
/// The live launchd/systemd service is unaffected by a stray shell var, because
/// neither inherits the shell's environment.
///
/// **It must be absolute.** A relative value is ignored, for the reason the
/// variable exists: processes in a tree share an environment but not a working
/// directory, so `LOGMON_CONFIG_DIR=probe` would put the daemon's state in one
/// place and its clients' socket lookup in another. Ignoring is the safe
/// asymmetry — daemon and client fall back to the *same* default and so cannot
/// disagree — and `warn_if_ignored` exists so it is not silent.
///
/// Three other readers follow this resolution and must stay in step:
/// the SDK's `default_socket_path()` (duplicated rather than imported, keeping
/// the SDK independent of this crate), `logmon-mcp`'s `auto_start`, and the
/// broker's own `status` subcommand.
pub fn config_dir() -> std::path::PathBuf {
    config_dir_from(
        std::env::var_os("LOGMON_CONFIG_DIR"),
        std::env::var_os("HOME"),
    )
}

/// The override, if it is usable at all.
///
/// Blank is ignored — `LOGMON_CONFIG_DIR= cmd` is the shell idiom for "unset
/// for this invocation" — and so is anything relative, per `config_dir`'s
/// contract. Whitespace-only counts as blank, matching how the shim already
/// treats `LOGMON_DOMAIN`.
fn usable_override(v: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    let v = v?;
    if v.as_encoded_bytes().trim_ascii().is_empty() {
        return None;
    }
    let p = std::path::PathBuf::from(v);
    p.is_absolute().then_some(p)
}

/// Whether `LOGMON_CONFIG_DIR` was set to something this build will not use, so
/// a caller with a log can say so instead of silently using the default. `Some`
/// carries the offending value.
pub fn ignored_config_dir_override() -> Option<std::ffi::OsString> {
    let raw = std::env::var_os("LOGMON_CONFIG_DIR")?;
    if raw.as_encoded_bytes().trim_ascii().is_empty() {
        return None; // deliberately unset for this invocation; not a mistake
    }
    usable_override(Some(raw.clone())).is_none().then_some(raw)
}

/// The pure resolution, split from the wrapper because the wrapper cannot be
/// unit-tested: tests share one process, so `std::env::set_var` in a test
/// races every concurrently-running test that reads the environment.
fn config_dir_from(
    override_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> std::path::PathBuf {
    if let Some(dir) = usable_override(override_dir) {
        return dir;
    }
    let home = home
        .filter(|h| !h.as_encoded_bytes().trim_ascii().is_empty())
        .unwrap_or_else(|| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("logmon")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here writes into its own temp dir. **Nothing in this module
    /// may touch `config_dir()`** — a developer's live daemon reads that path,
    /// and a test that wrote there would corrupt a running broker's state.
    fn tmp() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn a_corrupt_state_file_is_quarantined_rather_than_blocking_the_boot() {
        // The defect: load_state propagated the parse error, so a truncated
        // file meant the daemon refused to start until a human deleted it by
        // hand - a recoverable loss of bookkeeping turned into a broker outage.
        let d = tmp();
        let path = d.path().join("state.json");
        std::fs::write(&path, "{ this is not json").expect("write");

        let state = load_state(&path).expect("must not fail the boot");
        assert!(state.named_sessions.is_empty(), "started with no state");
        assert!(
            !path.exists(),
            "the unreadable file was moved out of the way"
        );
        assert!(
            d.path().join("state.json.corrupt").exists(),
            "and kept, because it is the only evidence of what went wrong"
        );
    }

    #[test]
    fn repeated_corruption_does_not_overwrite_the_first_evidence() {
        let d = tmp();
        let path = d.path().join("state.json");
        for _ in 0..3 {
            std::fs::write(&path, "nonsense").expect("write");
            load_state(&path).expect("survives");
        }
        assert!(d.path().join("state.json.corrupt").exists());
        assert!(d.path().join("state.json.corrupt.1").exists());
        assert!(d.path().join("state.json.corrupt.2").exists());
    }

    #[test]
    fn a_missing_state_file_is_not_an_error() {
        let d = tmp();
        let state = load_state(&d.path().join("nothing-here.json")).expect("fine");
        assert!(state.named_sessions.is_empty());
    }

    #[test]
    fn state_round_trips_through_an_atomic_write() {
        let d = tmp();
        let path = d.path().join("state.json");
        let state = DaemonState {
            seq_block: 4242,
            ..Default::default()
        };
        save_state(&path, &state).expect("saved");
        assert_eq!(load_state(&path).expect("loaded").seq_block, 4242);
    }

    #[test]
    fn an_atomic_write_leaves_no_temp_file_behind() {
        let d = tmp();
        let path = d.path().join("f.json");
        atomic_write(&path, b"payload").expect("written");
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_SUFFIX))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a completed write cleans up after itself"
        );
    }

    #[test]
    fn an_atomic_write_replaces_the_previous_contents_wholesale() {
        // The property the whole exercise is for: a reader sees either the old
        // file or the new one, never a prefix of the new one over the old.
        let d = tmp();
        let path = d.path().join("f.json");
        atomic_write(&path, b"a much longer original payload").expect("first");
        atomic_write(&path, b"short").expect("second");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"short",
            "no tail of the longer original survives"
        );
    }

    #[test]
    fn a_concurrent_reader_never_observes_a_partial_write() {
        // THE property, and the only one that distinguishes rename from
        // copy-then-delete: both leave identical final contents, so a
        // single-threaded test cannot tell them apart. Only a reader racing
        // the writer can. With a copy, this reader sees a prefix of the long
        // payload, or a long payload with the short one's head on it.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let d = tmp();
        let path = d.path().join("state.json");
        let short = vec![b'a'; 64];
        let long = vec![b'b'; 256 * 1024];
        atomic_write(&path, &long).expect("seed");

        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let path = path.clone();
            let (short, long) = (short.clone(), long.clone());
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut reads = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    // A missing file is fine — rename is atomic but the reader
                    // can still lose the race to open it at all.
                    if let Ok(seen) = std::fs::read(&path) {
                        assert!(
                            seen == short || seen == long,
                            "observed a partial write: {} bytes",
                            seen.len()
                        );
                        reads += 1;
                    }
                }
                reads
            })
        };

        for i in 0..200 {
            atomic_write(&path, if i % 2 == 0 { &short } else { &long }).expect("write");
        }
        stop.store(true, Ordering::Relaxed);
        let reads = reader.join().expect("reader finished");
        assert!(
            reads > 0,
            "the reader must have actually observed something"
        );
    }

    #[test]
    fn the_boot_sweep_removes_temp_files_a_crash_left_behind() {
        let d = tmp();
        std::fs::write(d.path().join(format!("state.json{TEMP_SUFFIX}")), "half").unwrap();
        std::fs::write(d.path().join(format!("other.json{TEMP_SUFFIX}")), "half").unwrap();
        std::fs::write(d.path().join("state.json"), "{}").unwrap();

        assert_eq!(sweep_temp_files(d.path()), 2);
        assert!(
            d.path().join("state.json").exists(),
            "and touches nothing else"
        );
        assert_eq!(sweep_temp_files(d.path()), 0, "idempotent");
    }

    // Pure-function tests, not `std::env::set_var`: tests share one process,
    // and a global env mutation races every concurrent test that reads it.
    #[test]
    fn the_env_var_overrides_the_home_derived_default() {
        let got = config_dir_from(Some("/tmp/elsewhere".into()), Some("/home/u".into()));
        assert_eq!(got, std::path::PathBuf::from("/tmp/elsewhere"));
    }

    #[test]
    fn without_the_var_the_default_is_dot_config_logmon_under_home() {
        let got = config_dir_from(None, Some("/home/u".into()));
        assert_eq!(got, std::path::PathBuf::from("/home/u/.config/logmon"));
    }

    #[test]
    fn an_empty_var_is_ignored_rather_than_resolving_against_the_cwd() {
        // `LOGMON_CONFIG_DIR= cmd` is the shell idiom for "unset for this
        // invocation"; honoring the empty string would make the config dir
        // whatever directory the daemon happened to start in.
        let got = config_dir_from(Some("".into()), Some("/home/u".into()));
        assert_eq!(got, std::path::PathBuf::from("/home/u/.config/logmon"));
    }

    #[test]
    fn a_relative_override_is_ignored_because_processes_share_no_cwd() {
        // The variable's whole purpose is cross-process redirection, and a
        // relative path would resolve differently in the daemon and in a client
        // started from another directory — the one failure it must not cause.
        for rel in ["probe", "./probe", "../probe"] {
            let got = config_dir_from(Some(rel.into()), Some("/home/u".into()));
            assert_eq!(
                got,
                std::path::PathBuf::from("/home/u/.config/logmon"),
                "{rel} must fall back to the default, not resolve against a cwd"
            );
        }
        // And absolute still works, or the check above would be vacuous.
        let got = config_dir_from(Some("/tmp/probe".into()), Some("/home/u".into()));
        assert_eq!(got, std::path::PathBuf::from("/tmp/probe"));
    }

    #[test]
    fn a_whitespace_only_override_counts_as_blank() {
        let got = config_dir_from(Some("   ".into()), Some("/home/u".into()));
        assert_eq!(got, std::path::PathBuf::from("/home/u/.config/logmon"));
    }

    #[test]
    fn no_home_falls_back_to_the_current_directory_as_before() {
        let got = config_dir_from(None, None);
        assert_eq!(got, std::path::PathBuf::from("./.config/logmon"));
        let got = config_dir_from(None, Some("".into()));
        assert_eq!(got, std::path::PathBuf::from("./.config/logmon"));
    }
}
