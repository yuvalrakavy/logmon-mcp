//! System-service install/uninstall — `install-service` /
//! `uninstall-service` subcommands.
//!
//! Public API is OS-agnostic: a [`Scope`] enum and free `install` /
//! `uninstall` functions. Each supported OS has its own submodule that
//! does the real work; the shim here just routes by `cfg(target_os)`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use anyhow::Result;

/// Service install scope. `User` registers the service under the current
/// user's home (LaunchAgents on macOS, `~/.config/systemd/user` on Linux);
/// `System` registers it system-wide (LaunchDaemons on macOS,
/// `/etc/systemd/system` on Linux). System scope typically needs root.
#[derive(Copy, Clone, Debug)]
pub enum Scope {
    User,
    System,
}

#[cfg(target_os = "macos")]
impl From<Scope> for macos::Scope {
    fn from(s: Scope) -> Self {
        match s {
            Scope::User => macos::Scope::User,
            Scope::System => macos::Scope::System,
        }
    }
}

#[cfg(target_os = "linux")]
impl From<Scope> for linux::Scope {
    fn from(s: Scope) -> Self {
        match s {
            Scope::User => linux::Scope::User,
            Scope::System => linux::Scope::System,
        }
    }
}

/// Install the broker as a managed system service in the given scope.
///
/// On macOS this drops a launchd plist and `launchctl bootstrap`s it.
/// On Linux this drops a systemd unit and `systemctl enable --now`s it.
/// On any other OS this returns a graceful error rather than panicking.
pub fn install(scope: Scope) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return macos::install(scope.into());
    }
    #[cfg(target_os = "linux")]
    {
        return linux::install(scope.into());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = scope;
        anyhow::bail!("install-service is only supported on macOS and Linux")
    }
}

/// Uninstall a previously-installed broker service. No-op (with a friendly
/// message) if no install is found in this scope.
pub fn uninstall(scope: Scope) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return macos::uninstall(scope.into());
    }
    #[cfg(target_os = "linux")]
    {
        return linux::uninstall(scope.into());
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = scope;
        anyhow::bail!("uninstall-service is only supported on macOS and Linux")
    }
}
