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
    /// (gh #28). A user agent writes next to its `daemon.log` — in the config dir the SERVICE
    /// will use, which ignores a `LOGMON_CONFIG_DIR` set in the installing shell (launchd sets
    /// `HOME` but passes on nothing from that shell); a system daemon runs as root, so its config
    /// dir is root's,
    /// and it writes under `/var/log` instead.
    fn stderr_path(&self) -> Result<PathBuf> {
        match self {
            Scope::User => {
                let home = dirs::home_dir().context("no home dir")?;
                Ok(stderr_path_under(&home))
            }
            Scope::System => Ok(PathBuf::from("/var/log/logmon-broker.stderr.log")),
        }
    }

    fn bootstrap_target(&self) -> String {
        match self {
            Scope::User => format!("gui/{}", current_uid()),
            Scope::System => "system".into(),
        }
    }
}

/// A user agent's stderr file for the user whose home is `home`.
fn stderr_path_under(home: &std::path::Path) -> PathBuf {
    logmon_broker_core::daemon::persistence::service_config_dir(home).join("daemon.stderr.log")
}

/// Current effective uid. We avoid the `users` crate (unmaintained) and
/// just call `getuid(2)` directly — signal-safe and never fails on Unix.
fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions, returns the caller's
    // real uid, and is safe to call at any point.
    unsafe { libc::getuid() }
}

/// What `install` writes, for a scope and binary: the plist's path, the stderr file's path, and
/// the rendered plist. Pure — `install` adds the filesystem and `launchctl` — so the wiring
/// between the three is testable on both scopes.
struct Plan {
    plist_path: PathBuf,
    stderr_path: PathBuf,
    rendered: String,
}

fn plan(scope: Scope, exe: &str) -> Result<Plan> {
    let plist_path = scope.plist_path()?;
    let stderr_path = scope.stderr_path()?;
    let stderr_str = stderr_path
        .to_str()
        .with_context(|| format!("stderr path is not valid UTF-8: {}", stderr_path.display()))?;
    let rendered = render_plist(exe, stderr_str);
    Ok(Plan {
        plist_path,
        stderr_path,
        rendered,
    })
}

pub fn install(scope: Scope) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let exe_str = exe
        .to_str()
        .with_context(|| format!("current_exe path is not valid UTF-8: {}", exe.display()))?;
    let Plan {
        plist_path,
        stderr_path,
        rendered,
    } = plan(scope, exe_str)?;

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
/// and an `&` or `<` in it would make the plist one launchd refuses to load. One pass over the
/// TEMPLATE, never over what was substituted into it — chained `replace`s re-scanned the
/// binary path for the next placeholder.
fn render_plist(binary: &str, stderr: &str) -> String {
    let mut out = String::with_capacity(PLIST_TEMPLATE.len() + binary.len() + stderr.len());
    let mut rest = PLIST_TEMPLATE;
    while let Some(start) = rest.find('{') {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let (value, len) = if from.starts_with("{BINARY_PATH}") {
            (Some(binary), "{BINARY_PATH}".len())
        } else if from.starts_with("{STDERR_PATH}") {
            (Some(stderr), "{STDERR_PATH}".len())
        } else {
            (None, 1)
        };
        match value {
            Some(v) => out.push_str(&xml_escape(v)),
            None => out.push('{'),
        }
        rest = &from[len..];
    }
    out.push_str(rest);
    out
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

    /// A user agent's stderr file is in the config dir the SERVICE uses — `~/.config/logmon`,
    /// whatever `LOGMON_CONFIG_DIR` the installing shell had (launchd does not pass it on).
    #[test]
    fn a_user_agent_writes_stderr_beside_the_services_daemon_log() {
        assert_eq!(
            stderr_path_under(std::path::Path::new("/Users/u")),
            PathBuf::from("/Users/u/.config/logmon/daemon.stderr.log")
        );
    }

    /// A system install writes its plist to `/Library/LaunchDaemons` and sends stderr to
    /// `/var/log` (a root daemon's config dir is root's) — and the plist names that file.
    #[test]
    fn a_system_install_sends_stderr_to_var_log() {
        let p = plan(Scope::System, "/usr/local/bin/logmon-broker").unwrap();
        assert_eq!(
            p.plist_path,
            PathBuf::from("/Library/LaunchDaemons/logmon.broker.plist")
        );
        assert_eq!(
            p.stderr_path,
            PathBuf::from("/var/log/logmon-broker.stderr.log")
        );
        assert!(
            p.rendered
                .contains("<string>/var/log/logmon-broker.stderr.log</string>"),
            "{}",
            p.rendered
        );
    }

    /// A user install's plist names the stderr file its own plan writes, under the user's home.
    #[test]
    fn a_user_install_names_the_stderr_file_it_creates() {
        let p = plan(Scope::User, "/usr/local/bin/logmon-broker").unwrap();
        let stderr = p.stderr_path.to_str().unwrap();
        assert!(
            stderr.ends_with("/.config/logmon/daemon.stderr.log"),
            "{stderr}"
        );
        assert!(
            p.rendered.contains(&format!("<string>{stderr}</string>")),
            "{}",
            p.rendered
        );
    }

    /// A substituted path is never read as a placeholder.
    #[test]
    fn a_path_that_spells_a_placeholder_is_left_as_written() {
        let plist = render_plist("/opt/{STDERR_PATH}/bin", "/var/log/x.log");
        assert!(
            plist.contains("<string>/opt/{STDERR_PATH}/bin</string>"),
            "{plist}"
        );
    }
}
