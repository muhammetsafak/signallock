//! Per-platform services, selected with `#[cfg(target_os = ...)]`. The other
//! platforms' modules are not compiled.
//!
//! Later phases add `migrate_legacy(&Store)`, the `Autostart` trait and the
//! session-unlock event source here.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Directory that holds `settings.json`, `trusted-device.json` and the log.
///
/// `<config dir>/SignalLock` on every platform: `~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows, `$XDG_CONFIG_HOME` (fallback `~/.config`)
/// on Linux. `ProjectDirs` is deliberately not used: on macOS it would yield a
/// bundle-ID-shaped directory and on Windows it nests an extra `config\` level.
///
/// Falls back to a `SignalLock` directory next to the current directory only
/// when the OS reports no home directory at all, so startup never panics.
pub fn config_dir() -> PathBuf {
    match directories::BaseDirs::new() {
        Some(dirs) => dirs.config_dir().join("SignalLock"),
        None => PathBuf::from("SignalLock"),
    }
}
