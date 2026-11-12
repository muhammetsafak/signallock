//! `LockService`: runs the per-platform lock strategy chain (ADR-014,
//! "Lock primitives") and reports which strategy locked the screen.
//!
//! This is the only file in the workspace that spawns a process (ADR-002, as
//! carried into ADR-014): the macOS `CGSession` and `pmset` fallbacks and the
//! Linux `xflock4` / `swaylock` / `hyprlock` fallbacks. In-process primitives
//! (`SACLockScreenImmediate`, `LockWorkStation`, D-Bus calls) live in
//! `platform/<os>/lock.rs` and are called from here.
//!
//! `lock_now` blocks the calling thread (a process wait, a D-Bus round trip);
//! call it from a blocking context, not from an async task.
//!
//! `main.rs` still declares this module privately and nothing consumes it
//! there until the `AppState` phase (Faz 05), hence the allowance below; the
//! library target (`lib.rs`) exports it for tests and `examples/lock_probe.rs`.

#![allow(dead_code)]

use std::fmt;

use crate::logging;
#[cfg(not(target_os = "windows"))]
use crate::platform::PlatformError;
use crate::platform::Result;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use crate::platform::native;

/// The strategy that locked the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMethod {
    /// macOS: `SACLockScreenImmediate()` from the private `login` framework.
    MacLoginFramework,
    /// macOS: `CGSession -suspend`.
    MacCgSession,
    /// macOS: `pmset displaysleepnow` (locks only if "Require password" is on).
    MacPmsetDisplaySleep,
    /// Windows: `LockWorkStation`.
    WindowsLockWorkStation,
    /// Linux: `org.freedesktop.login1` `Session.Lock`, verified through the
    /// session's `LockedHint`.
    LinuxLogind,
    /// Linux: `org.gnome.ScreenSaver.Lock`.
    LinuxGnomeScreenSaver,
    /// Linux: `org.freedesktop.ScreenSaver.Lock`.
    LinuxFreedesktopScreenSaver,
    /// Linux: `org.cinnamon.ScreenSaver.Lock`.
    LinuxCinnamonScreenSaver,
    /// Linux: the `xflock4` command.
    LinuxXflock4,
    /// Linux: the `swaylock` command.
    LinuxSwaylock,
    /// Linux: the `hyprlock` command.
    LinuxHyprlock,
}

impl LockMethod {
    /// Stable name used in logs and by `lock_probe`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MacLoginFramework => "SACLockScreenImmediate",
            Self::MacCgSession => "CGSession -suspend",
            Self::MacPmsetDisplaySleep => "pmset displaysleepnow",
            Self::WindowsLockWorkStation => "LockWorkStation",
            Self::LinuxLogind => "login1 Session.Lock",
            Self::LinuxGnomeScreenSaver => "org.gnome.ScreenSaver.Lock",
            Self::LinuxFreedesktopScreenSaver => "org.freedesktop.ScreenSaver.Lock",
            Self::LinuxCinnamonScreenSaver => "org.cinnamon.ScreenSaver.Lock",
            Self::LinuxXflock4 => "xflock4",
            Self::LinuxSwaylock => "swaylock",
            Self::LinuxHyprlock => "hyprlock",
        }
    }
}

impl fmt::Display for LockMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Locks the screen of the current session.
#[derive(Debug, Default, Clone, Copy)]
pub struct LockService;

impl LockService {
    pub fn new() -> Self {
        Self
    }

    /// Runs the strategy chain in order; the first success wins and is logged.
    pub fn lock_now(&self) -> Result<LockMethod> {
        let result = lock_chain();
        match &result {
            Ok(LockMethod::MacPmsetDisplaySleep) => tracing::info!(
                target: logging::LOCK,
                "Triggered display sleep via pmset (locks only if 'Require password' is on)"
            ),
            Ok(method) => tracing::info!(target: logging::LOCK, "Locked via {method}"),
            Err(e) => tracing::error!(target: logging::LOCK, "All lock strategies failed: {e}"),
        }
        result
    }

    /// The strategy `lock_now` would use first on this machine, without
    /// locking anything. `None` when no strategy looks usable. A strategy that
    /// is available can still fail when run, in which case `lock_now` falls
    /// through to the next one.
    pub fn select_method(&self) -> Option<LockMethod> {
        select_method()
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
const CG_SESSION_PATH: &str =
    "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession";
#[cfg(target_os = "macos")]
const PMSET_PATH: &str = "/usr/bin/pmset";

#[cfg(target_os = "macos")]
fn lock_chain() -> Result<LockMethod> {
    match native::lock_screen_immediate() {
        Ok(()) => return Ok(LockMethod::MacLoginFramework),
        Err(e) => tracing::debug!(target: logging::LOCK, "login framework strategy: {e}"),
    }

    for (path, args, method) in MAC_COMMANDS {
        if !std::path::Path::new(path).exists() {
            tracing::debug!(target: logging::LOCK, "{path} not present");
            continue;
        }
        match run_and_wait(path, args) {
            Ok(()) => return Ok(method),
            Err(e) => tracing::error!(target: logging::LOCK, "{e}"),
        }
    }
    Err(PlatformError::new("no lock strategy succeeded"))
}

#[cfg(target_os = "macos")]
const MAC_COMMANDS: [(&str, &[&str], LockMethod); 2] = [
    (CG_SESSION_PATH, &["-suspend"], LockMethod::MacCgSession),
    (
        PMSET_PATH,
        &["displaysleepnow"],
        LockMethod::MacPmsetDisplaySleep,
    ),
];

#[cfg(target_os = "macos")]
fn select_method() -> Option<LockMethod> {
    if native::is_available() {
        return Some(LockMethod::MacLoginFramework);
    }
    MAC_COMMANDS
        .iter()
        .find(|(path, _, _)| std::path::Path::new(path).exists())
        .map(|(_, _, method)| *method)
}

/// Runs `path args` to completion; `Err` carries the exit status and stderr.
#[cfg(target_os = "macos")]
fn run_and_wait(path: &str, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new(path)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| PlatformError::new(format!("failed to run {path}: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(PlatformError::new(format!(
        "{path} exited {}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn lock_chain() -> Result<LockMethod> {
    native::lock_workstation().map(|()| LockMethod::WindowsLockWorkStation)
}

#[cfg(target_os = "windows")]
fn select_method() -> Option<LockMethod> {
    Some(LockMethod::WindowsLockWorkStation)
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

/// How long login1 `Session.Lock` has to turn the session's `LockedHint` on
/// before the chain moves on (ADR-014).
#[cfg(target_os = "linux")]
const LOGIND_VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Desktop environments with a strategy of their own (ADR-014).
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Gnome,
    Kde,
    Cinnamon,
    Sway,
    Hyprland,
    Xfce,
}

/// Maps `$XDG_CURRENT_DESKTOP` to a [`Desktop`]. The variable is a
/// colon-separated list (`ubuntu:GNOME`, `X-Cinnamon`); the first recognised
/// entry wins, compared case-insensitively. Pure.
#[cfg(any(target_os = "linux", test))]
pub fn detect_desktop(xdg_current_desktop: &str) -> Option<Desktop> {
    xdg_current_desktop.split(':').find_map(|entry| {
        match entry.trim().to_ascii_lowercase().as_str() {
            "gnome" => Some(Desktop::Gnome),
            "kde" => Some(Desktop::Kde),
            "x-cinnamon" | "cinnamon" => Some(Desktop::Cinnamon),
            "sway" => Some(Desktop::Sway),
            "hyprland" => Some(Desktop::Hyprland),
            "xfce" | "x-xfce" => Some(Desktop::Xfce),
            _ => None,
        }
    })
}

/// The strategy chain for `desktop`, in order (ADR-014, "Lock primitives"):
///
/// 1. the desktop's own strategy, which counts as success on its own result;
/// 2. login1 `Session.Lock`, which counts only after the `LockedHint` check;
/// 3. the remaining D-Bus screensavers, then the locker commands found in
///    `$PATH` (`swaylock`, `hyprlock`, `xflock4`).
///
/// A strategy appears once. Pure.
#[cfg(any(target_os = "linux", test))]
pub fn linux_strategy_order(desktop: Option<Desktop>) -> Vec<LockMethod> {
    const GENERIC_TAIL: [LockMethod; 6] = [
        LockMethod::LinuxGnomeScreenSaver,
        LockMethod::LinuxFreedesktopScreenSaver,
        LockMethod::LinuxCinnamonScreenSaver,
        LockMethod::LinuxSwaylock,
        LockMethod::LinuxHyprlock,
        LockMethod::LinuxXflock4,
    ];

    let own = desktop.map(|desktop| match desktop {
        Desktop::Gnome => LockMethod::LinuxGnomeScreenSaver,
        Desktop::Kde => LockMethod::LinuxFreedesktopScreenSaver,
        Desktop::Cinnamon => LockMethod::LinuxCinnamonScreenSaver,
        Desktop::Sway => LockMethod::LinuxSwaylock,
        Desktop::Hyprland => LockMethod::LinuxHyprlock,
        Desktop::Xfce => LockMethod::LinuxXflock4,
    });

    let mut order = Vec::with_capacity(GENERIC_TAIL.len() + 2);
    order.extend(own);
    order.push(LockMethod::LinuxLogind);
    order.extend(GENERIC_TAIL.into_iter().filter(|m| Some(*m) != own));
    order
}

/// The locker command of a command strategy.
#[cfg(any(target_os = "linux", test))]
fn linux_command_name(method: LockMethod) -> Option<&'static str> {
    match method {
        LockMethod::LinuxSwaylock => Some("swaylock"),
        LockMethod::LinuxHyprlock => Some("hyprlock"),
        LockMethod::LinuxXflock4 => Some("xflock4"),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn current_desktop() -> Option<Desktop> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .and_then(|value| detect_desktop(&value))
}

#[cfg(target_os = "linux")]
fn lock_chain() -> Result<LockMethod> {
    let desktop = current_desktop();
    tracing::debug!(target: logging::LOCK, "desktop: {desktop:?}");
    for method in linux_strategy_order(desktop) {
        match run_linux_step(method) {
            Ok(()) => return Ok(method),
            Err(e) => tracing::warn!(target: logging::LOCK, "{method} did not lock: {e}"),
        }
    }
    Err(PlatformError::new("no lock strategy succeeded"))
}

#[cfg(target_os = "linux")]
fn run_linux_step(method: LockMethod) -> Result<()> {
    if let Some(name) = linux_command_name(method) {
        let path = find_in_path(name)
            .ok_or_else(|| PlatformError::new(format!("{name} not found in PATH")))?;
        return run_locker(&path);
    }
    match method {
        LockMethod::LinuxLogind => native::lock_login1_verified(LOGIND_VERIFY_TIMEOUT),
        LockMethod::LinuxGnomeScreenSaver => native::lock_gnome(),
        LockMethod::LinuxFreedesktopScreenSaver => native::lock_freedesktop(),
        LockMethod::LinuxCinnamonScreenSaver => native::lock_cinnamon(),
        other => Err(PlatformError::new(format!(
            "{other} is not a Linux strategy"
        ))),
    }
}

/// The first strategy of the chain that looks usable, without locking.
#[cfg(target_os = "linux")]
fn select_method() -> Option<LockMethod> {
    linux_strategy_order(current_desktop())
        .into_iter()
        .find(|method| match linux_command_name(*method) {
            Some(name) => find_in_path(name).is_some(),
            None => match method {
                LockMethod::LinuxLogind => native::login1_available(),
                LockMethod::LinuxGnomeScreenSaver => native::gnome_available(),
                LockMethod::LinuxFreedesktopScreenSaver => native::freedesktop_available(),
                LockMethod::LinuxCinnamonScreenSaver => native::cinnamon_available(),
                _ => false,
            },
        })
}

/// Starts a screen locker. `swaylock` and `hyprlock` stay in the foreground
/// until the user unlocks, so a process that is still running shortly after
/// the start counts as success; one that already exited must have exited 0.
#[cfg(target_os = "linux")]
fn run_locker(path: &std::path::Path) -> Result<()> {
    use std::time::{Duration, Instant};

    const SETTLE: Duration = Duration::from_millis(500);

    let mut child = std::process::Command::new(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| PlatformError::new(format!("failed to run {}: {e}", path.display())))?;

    let deadline = Instant::now() + SETTLE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(PlatformError::new(format!(
                    "{} exited {status}",
                    path.display()
                )));
            }
            Ok(None) if Instant::now() >= deadline => {
                // Still running: it is the locker. Reap it in the background
                // once the user unlocks.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                return Err(PlatformError::new(format!(
                    "waiting for {} failed: {e}",
                    path.display()
                )));
            }
        }
    }
}

/// First executable named `name` in the absolute `$PATH` directories.
#[cfg(target_os = "linux")]
fn find_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_dirs(name, std::env::split_paths(&path))
}

#[cfg(any(target_os = "linux", test))]
fn find_in_dirs(
    name: &str,
    dirs: impl IntoIterator<Item = std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    // A relative or empty `$PATH` entry (`PATH=/usr/bin:`) would resolve against
    // the working directory and could run a file the user never installed.
    dirs.into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(any(target_os = "linux", test))]
fn is_executable_file(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// Other targets
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn lock_chain() -> Result<LockMethod> {
    Err(PlatformError::new(
        "locking is not supported on this platform",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn select_method() -> Option<LockMethod> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::platform::test_util::TestDir;

    #[test]
    fn method_names_are_distinct_and_non_empty() {
        let all = [
            LockMethod::MacLoginFramework,
            LockMethod::MacCgSession,
            LockMethod::MacPmsetDisplaySleep,
            LockMethod::WindowsLockWorkStation,
            LockMethod::LinuxLogind,
            LockMethod::LinuxGnomeScreenSaver,
            LockMethod::LinuxFreedesktopScreenSaver,
            LockMethod::LinuxCinnamonScreenSaver,
            LockMethod::LinuxXflock4,
            LockMethod::LinuxSwaylock,
            LockMethod::LinuxHyprlock,
        ];
        let mut names: Vec<_> = all.iter().map(|m| m.as_str()).collect();
        assert!(names.iter().all(|n| !n.is_empty()));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
    }

    #[test]
    fn desktop_is_detected_from_xdg_current_desktop() {
        assert_eq!(detect_desktop("GNOME"), Some(Desktop::Gnome));
        assert_eq!(detect_desktop("ubuntu:GNOME"), Some(Desktop::Gnome));
        assert_eq!(detect_desktop("KDE"), Some(Desktop::Kde));
        assert_eq!(detect_desktop("X-Cinnamon"), Some(Desktop::Cinnamon));
        assert_eq!(detect_desktop("sway"), Some(Desktop::Sway));
        assert_eq!(detect_desktop("Hyprland"), Some(Desktop::Hyprland));
        assert_eq!(detect_desktop("XFCE"), Some(Desktop::Xfce));
        assert_eq!(detect_desktop("hyprland"), Some(Desktop::Hyprland));
        assert_eq!(detect_desktop("i3"), None);
        assert_eq!(detect_desktop(""), None);
    }

    #[test]
    fn each_desktop_tries_its_own_strategy_first_then_login1() {
        use LockMethod::*;
        let cases = [
            (Desktop::Gnome, LinuxGnomeScreenSaver),
            (Desktop::Kde, LinuxFreedesktopScreenSaver),
            (Desktop::Cinnamon, LinuxCinnamonScreenSaver),
            (Desktop::Sway, LinuxSwaylock),
            (Desktop::Hyprland, LinuxHyprlock),
            (Desktop::Xfce, LinuxXflock4),
        ];
        for (desktop, own) in cases {
            let order = linux_strategy_order(Some(desktop));
            assert_eq!(order[0], own, "{desktop:?}");
            assert_eq!(order[1], LinuxLogind, "{desktop:?}");
        }
    }

    #[test]
    fn sway_order_matches_adr_014() {
        use LockMethod::*;
        assert_eq!(
            linux_strategy_order(Some(Desktop::Sway)),
            [
                LinuxSwaylock,
                LinuxLogind,
                LinuxGnomeScreenSaver,
                LinuxFreedesktopScreenSaver,
                LinuxCinnamonScreenSaver,
                LinuxHyprlock,
                LinuxXflock4,
            ]
        );
    }

    #[test]
    fn unknown_desktop_starts_with_login1_then_the_generic_tail() {
        use LockMethod::*;
        assert_eq!(
            linux_strategy_order(None),
            [
                LinuxLogind,
                LinuxGnomeScreenSaver,
                LinuxFreedesktopScreenSaver,
                LinuxCinnamonScreenSaver,
                LinuxSwaylock,
                LinuxHyprlock,
                LinuxXflock4,
            ]
        );
    }

    #[test]
    fn every_strategy_appears_exactly_once() {
        let desktops = [
            None,
            Some(Desktop::Gnome),
            Some(Desktop::Kde),
            Some(Desktop::Cinnamon),
            Some(Desktop::Sway),
            Some(Desktop::Hyprland),
            Some(Desktop::Xfce),
        ];
        for desktop in desktops {
            let order = linux_strategy_order(desktop);
            assert_eq!(order.len(), 7, "{desktop:?}");
            let mut unique = order.clone();
            unique.sort_by_key(|m| m.as_str());
            unique.dedup();
            assert_eq!(unique.len(), 7, "{desktop:?}");
        }
    }

    #[test]
    fn command_strategies_map_to_their_binaries() {
        assert_eq!(
            linux_command_name(LockMethod::LinuxSwaylock),
            Some("swaylock")
        );
        assert_eq!(
            linux_command_name(LockMethod::LinuxHyprlock),
            Some("hyprlock")
        );
        assert_eq!(
            linux_command_name(LockMethod::LinuxXflock4),
            Some("xflock4")
        );
        assert_eq!(linux_command_name(LockMethod::LinuxLogind), None);
    }

    #[cfg(unix)]
    #[test]
    fn find_in_dirs_returns_the_first_executable_match() {
        use std::os::unix::fs::PermissionsExt;

        let first = TestDir::new("path-first");
        let second = TestDir::new("path-second");
        // Present but not executable in `first`, executable in `second`.
        let plain = first.path().join("hyprlock");
        std::fs::write(&plain, b"").unwrap();
        let exe = second.path().join("hyprlock");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let found = find_in_dirs(
            "hyprlock",
            [first.path().to_path_buf(), second.path().to_path_buf()],
        );

        assert_eq!(found, Some(exe));
        assert_eq!(find_in_dirs("swaylock", [first.path().to_path_buf()]), None);
    }

    #[cfg(unix)]
    #[test]
    fn relative_and_empty_path_entries_are_ignored() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new("path-relative");
        let exe = dir.path().join("hyprlock");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The same directory, spelled relative to the current directory, so it
        // would resolve if relative entries were honoured.
        let depth = std::env::current_dir().unwrap().components().count() - 1;
        let relative = std::path::PathBuf::from("../".repeat(depth))
            .join(dir.path().strip_prefix("/").unwrap());
        assert_eq!(
            std::fs::canonicalize(&relative).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );

        let found = find_in_dirs(
            "hyprlock",
            [
                std::path::PathBuf::new(),
                std::path::PathBuf::from("."),
                relative,
            ],
        );

        assert_eq!(found, None);
    }
}
