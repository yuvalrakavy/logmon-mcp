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
    let rendered = PLIST_TEMPLATE.replace("{BINARY_PATH}", exe_str);

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create plist parent dir: {}", parent.display())
        })?;
    }
    std::fs::write(&plist_path, rendered)
        .with_context(|| format!("failed to write plist: {}", plist_path.display()))?;

    // bootout existing first (idempotent — safe even if not previously loaded)
    let _ = bootout(scope);

    let target = scope.bootstrap_target();
    let plist_str = plist_path.to_str().with_context(|| {
        format!("plist path is not valid UTF-8: {}", plist_path.display())
    })?;
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
