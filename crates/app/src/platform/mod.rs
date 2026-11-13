//! Per-platform services, selected with `#[cfg(target_os = ...)]`.
//!
//! Three things live behind this module (ADR-014):
//!
//! - in-process lock primitives (`<os>/lock.rs`), called only from
//!   `lock_service.rs`,
//! - login autostart ([`Autostart`], implemented by [`SystemAutostart`]),
//! - the one-time import of the legacy apps' settings ([`migrate_legacy`]),
//!   and the session lock/unlock event source ([`session_events`]).
//!
//! The other platforms' modules are not compiled. The exception is the unit
//! test build: files that hold only portable logic (the Linux `.desktop`
//! entry, the Windows file migration, the Windows `Run` value) are compiled
//! on every platform under `cfg(test)` so that `cargo test` on a developer
//! machine covers them. Their OS API parts stay behind `cfg(target_os)`.
//!
//! `main.rs` still declares this module privately and nothing consumes it
//! there until the `AppState` phase (Faz 05), hence the allowances below; the
//! library target (`lib.rs`) exports it for tests and `examples/lock_probe.rs`.

#![allow(dead_code)]

use std::fmt;
use std::path::PathBuf;

use signallock_core::Store;

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", test))]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as sys;
#[cfg(target_os = "macos")]
use self::macos as sys;
#[cfg(target_os = "windows")]
use self::windows as sys;

/// In-process lock primitives of this platform, for `lock_service.rs` only.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) use self::sys::lock as native;

/// In-app update install logic of this platform, for `update_service.rs` only.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub(crate) use self::sys::update as native_update;

/// The platform's [`Autostart`] implementation.
#[allow(unused_imports)]
#[cfg(target_os = "linux")]
pub use self::linux::autostart::LinuxAutostart as SystemAutostart;
/// The platform's [`Autostart`] implementation.
#[allow(unused_imports)]
#[cfg(target_os = "macos")]
pub use self::macos::autostart::MacAutostart as SystemAutostart;
/// The platform's [`Autostart`] implementation.
#[allow(unused_imports)]
#[cfg(target_os = "windows")]
pub use self::windows::autostart::WindowsAutostart as SystemAutostart;

/// Failure of a platform service. Carries a human-readable message only; the
/// callers log it and fall through to the next strategy or tell the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    message: String,
}

impl PlatformError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlatformError {}

pub type Result<T> = std::result::Result<T, PlatformError>;

/// Login autostart: whether the app starts when the user logs in.
pub trait Autostart {
    /// `false` when the state cannot be determined.
    fn is_enabled(&self) -> bool;

    /// Idempotent: enabling an enabled entry or disabling a missing one is
    /// `Ok`.
    fn set_enabled(&self, on: bool) -> Result<()>;
}

/// Result of [`migrate_legacy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// Nothing to do on this platform (Linux has no legacy app).
    NotApplicable,
    /// The new store already has its files (macOS) or its files are already
    /// in the new format (Windows); nothing was touched.
    AlreadyMigrated,
    /// No legacy data was found; nothing was written.
    NothingToImport,
    /// Legacy data was written to the new store.
    Imported {
        settings: bool,
        trusted_device: bool,
    },
}

/// Lock-state change of the user's session, used to re-arm the proximity
/// monitor after the user unlocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    Locked,
    Unlocked,
}

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

/// Imports the settings of the legacy native apps into `store`, once.
///
/// Idempotent: a second call finds the work done and changes nothing. The
/// legacy data (macOS `UserDefaults` keys, the Windows PascalCase files) is
/// never deleted.
pub fn migrate_legacy(store: &Store) -> Result<MigrationOutcome> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        sys::migrate::migrate_legacy(store)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = store;
        Ok(MigrationOutcome::NotApplicable)
    }
}

/// Starts listening for the session being locked and unlocked.
///
/// The returned receiver yields [`SessionEvent`]s until the process exits. A
/// source that cannot be set up is logged and leaves the receiver silent
/// rather than failing, because losing the unlock re-arm is not fatal.
///
/// On macOS call this from the main thread: the notification is delivered
/// through the main run loop. `AppState` calls `arm()` on `Unlocked` (Faz 05).
pub fn session_events() -> tokio::sync::mpsc::Receiver<SessionEvent> {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        sys::session::session_events()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        tokio::sync::mpsc::channel(1).1
    }
}

/// Throwaway directories for tests (the app crate has no `tempfile`).
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh directory under the system temp dir, removed on drop.
    pub struct TestDir(PathBuf);

    impl TestDir {
        pub fn new(label: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "signallock-test-{label}-{}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test dir");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
