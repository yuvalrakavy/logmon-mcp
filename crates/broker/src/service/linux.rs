//! systemd service install/uninstall for Linux.
//!
//! Renders the bundled unit template against the current binary path,
//! drops it under `~/.config/systemd/user` (User scope) or
//! `/etc/systemd/system` (System scope), then `systemctl daemon-reload`
//! + `enable --now`. Uninstall does the reverse: `disable --now`,
//!   remove unit, `daemon-reload`.
//!
//! The unit type is `Type=notify`, which matches the broker's sd_notify
//! READY=1 emit on startup (see Task 20). systemd will mark the unit
//! active only after that signal arrives.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

const UNIT_TEMPLATE: &str = include_str!("../../templates/systemd.service.template");
const UNIT_NAME: &str = "logmon-broker.service";

/// User-vs-system scope for service install. Local scope to this module —
/// the public API in `service/mod.rs` re-exports a top-level `Scope` and
/// converts to/from this one via `From`.
#[derive(Copy, Clone, Debug)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn unit_path(&self) -> Result<PathBuf> {
        match self {
            Scope::User => {
                let home = dirs::home_dir().context("no home dir")?;
                Ok(home.join(".config/systemd/user").join(UNIT_NAME))
            }
            Scope::System => Ok(PathBuf::from(format!("/etc/systemd/system/{UNIT_NAME}"))),
        }
    }
}

pub fn install(scope: Scope) -> Result<()> {
    let exe = std::env::current_exe().context("resolve current_exe")?;
    let exe_str = exe
        .to_str()
        .with_context(|| format!("current_exe path is not valid UTF-8: {}", exe.display()))?;
    let unit_path = scope.unit_path()?;
    let rendered = UNIT_TEMPLATE.replace("{BINARY_PATH}", exe_str);

    if let Some(parent) = unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create unit parent dir: {}", parent.display()))?;
    }
    std::fs::write(&unit_path, rendered)
        .with_context(|| format!("failed to write unit: {}", unit_path.display()))?;

    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", "--now", UNIT_NAME])?;
    println!("installed and started: {}", unit_path.display());
    Ok(())
}

pub fn uninstall(scope: Scope) -> Result<()> {
    let unit_path = scope.unit_path()?;
    // Best-effort: ignore failure if the unit isn't currently loaded.
    let _ = systemctl(scope, &["disable", "--now", UNIT_NAME]);
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("failed to remove unit: {}", unit_path.display()))?;
        println!("removed {}", unit_path.display());
    } else {
        println!("not installed (no-op)");
    }
    let _ = systemctl(scope, &["daemon-reload"]);
    Ok(())
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut cmd = std::process::Command::new("systemctl");
    if matches!(scope, Scope::User) {
        cmd.arg("--user");
    }
    cmd.args(args);
    let status = cmd.status().context("invoke systemctl")?;
    if !status.success() {
        bail!("systemctl {args:?} failed (exit {:?})", status.code());
    }
    Ok(())
}
