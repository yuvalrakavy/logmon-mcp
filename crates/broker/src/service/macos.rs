//! launchd service install/uninstall for macOS.
//!
//! Renders the bundled plist template against the current binary path,
//! drops it under `~/Library/LaunchAgents` (User scope) or
//! `/Library/LaunchDaemons` (System scope), then `launchctl bootstrap`s
//! it. Uninstall does the reverse: `bootout` then remove the plist.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

const PLIST_TEMPLATE: &str = include_str!("../../templates/launchd.plist.template");
const LABEL: &str = "logmon.broker";

/// User-vs-system scope for service install. Local scope to this module —
/// the public API in `service/mod.rs` re-exports a top-level `Scope` and
/// converts to/from this one via `From`.
#[derive(Copy, Clone, Debug)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn plist_path(&self) -> Result<PathBuf> {
        match self {
            Scope::User => {
                let home = dirs::home_dir().context("no home dir")?;
                Ok(home
                    .join("Library/LaunchAgents")
                    .join(format!("{LABEL}.plist")))
            }
            Scope::System => Ok(PathBuf::from(format!(
                "/Library/LaunchDaemons/{LABEL}.plist"
            ))),
        }
    }

    /// Where launchd writes the broker's stderr. Without one it is discarded, and `KeepAlive`
    /// restarts a broker that cannot start every 10 s with no trace of why: a failure before
    /// the broker's own log exists (the config dir, `load_config`), a panic, `main`'s error
    /// (gh #28). A user agent writes next to its `daemon.log`; a system daemon runs as root, so
    /// its config dir is root's, and it writes under `/var/log` instead.
    fn stderr_path(&self) -> PathBuf {
        match self {
            Scope::User => {
                logmon_broker_core::daemon::persistence::config_dir().join("daemon.stderr.log")
            }
            Scope::System => PathBuf::from("/var/log/logmon-broker.stderr.log"),
        }
    }

    fn bootstrap_target(&self) -> String {
        match self {
            Scope::User => format!("gui/{}", current_uid()),
            Scope::System => "system".into(),
        }
    }
}

/// Current effective uid. We avoid the `users` crate (unmaintained) and
/// just call `getuid(2)` directly — signal-safe and never fails on Unix.
fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions, returns the caller's
    // real uid, and is safe to call at any point.
    unsafe { libc::getuid() }
}

pub fn install(scope: Scope) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let exe_str = exe
        .to_str()
        .with_context(|| format!("current_exe path is not valid UTF-8: {}", exe.display()))?;
    let plist_path = scope.plist_path()?;
    let stderr_path = scope.stderr_path();
    let stderr_str = stderr_path
        .to_str()
        .with_context(|| format!("stderr path is not valid UTF-8: {}", stderr_path.display()))?;
    let rendered = render_plist(exe_str, stderr_str);

    // launchd does not create the stderr file's directory; without it the redirect fails and
    // stderr is lost again.
    if let Some(parent) = stderr_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create stderr log dir: {}", parent.display()))?;
    }
    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create plist parent dir: {}", parent.display()))?;
    }
    std::fs::write(&plist_path, rendered)
        .with_context(|| format!("failed to write plist: {}", plist_path.display()))?;

    // bootout existing first (idempotent — safe even if not previously loaded)
    let _ = bootout(scope);

    let target = scope.bootstrap_target();
    let plist_str = plist_path
        .to_str()
        .with_context(|| format!("plist path is not valid UTF-8: {}", plist_path.display()))?;
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &target, plist_str])
        .status()
        .context("failed to invoke launchctl")?;
    if !status.success() {
        bail!("launchctl bootstrap failed (exit {:?})", status.code());
    }
    println!("installed and started: {}", plist_path.display());
    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<()> {
    let plist_path = scope.plist_path()?;
    let _ = bootout(scope);
    if plist_path.exists() {
        std::fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove plist: {}", plist_path.display()))?;
        println!("removed {}", plist_path.display());
    } else {
        println!("not installed (no-op)");
    }
    Ok(())
}

fn bootout(scope: Scope) -> Result<()> {
    let target = scope.bootstrap_target();
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{target}/{LABEL}")])
        .status();
    Ok(())
}

/// The plist for this binary and stderr file. Both paths are XML-escaped: a path is free text,
/// and an `&` or `<` in it would make the plist one launchd refuses to load.
fn render_plist(binary: &str, stderr: &str) -> String {
    PLIST_TEMPLATE
        .replace("{BINARY_PATH}", &xml_escape(binary))
        .replace("{STDERR_PATH}", &xml_escape(stderr))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_sends_stderr_to_the_file_it_was_given() {
        let plist = render_plist(
            "/opt/bin/logmon-broker",
            "/Users/u/.config/logmon/daemon.stderr.log",
        );
        assert!(
            plist.contains(
                "<key>StandardErrorPath</key> <string>/Users/u/.config/logmon/daemon.stderr.log</string>"
            ),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/opt/bin/logmon-broker</string>"),
            "{plist}"
        );
        assert!(!plist.contains('{'), "every placeholder is filled: {plist}");
    }

    #[test]
    fn paths_are_xml_escaped() {
        let plist = render_plist("/a&b/<bin>", "/x'y\"z.log");
        assert!(
            plist.contains("<string>/a&amp;b/&lt;bin&gt;</string>"),
            "{plist}"
        );
        assert!(
            plist.contains("<string>/x&apos;y&quot;z.log</string>"),
            "{plist}"
        );
    }

    #[test]
    fn a_user_agent_writes_stderr_beside_its_daemon_log() {
        let path = Scope::User.stderr_path();
        assert_eq!(
            path.parent(),
            Some(logmon_broker_core::daemon::persistence::config_dir().as_path())
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("daemon.stderr.log")
        );
    }
}
