//! Installing a verified update and relaunching the app (ADR-015).
//!
//! **Second process-spawning file.** Together with `lock_service.rs` this is
//! the only place that starts a process: `/usr/bin/ditto`, `/usr/bin/codesign`,
//! `/usr/bin/plutil` and `/usr/bin/open` on macOS, the verified installer on
//! Windows, the new AppImage and `xdg-open` on Linux. Every program is an
//! absolute path (`PATH` is never searched), arguments go through
//! `Command::arg`, and there is no shell. The platform files
//! (`platform/<os>/update.rs`) hold the install logic but never spawn: they ask
//! the [`CommandRunner`] they are handed, and [`SystemRunner`] below is the only
//! implementation that touches `std::process`.
//!
//! **Fail-closed.** [`UpdateInstaller::install`] takes a
//! [`VerifiedPackage`], a type only `update/verify.rs` can construct after the
//! SHA-256, name and version checks. On macOS the platform code adds the Apple
//! code-signature check before anything is replaced.
//!
//! **Two steps.** Installing and relaunching are separate calls so that the app
//! can stop monitoring, save its settings and release the single-instance guard
//! in between (ADR-015, "Relaunch order"): [`UpdateInstaller::install`] swaps the
//! files and returns [`InstallOutcome::RestartPending`]; the caller then calls
//! [`UpdateInstaller::relaunch`] and quits. On Windows `relaunch` is what starts
//! the installer, and the installer's `[Run]` entry starts the new version.
//!
//! **Downloads on disk.** `<cache>/SignalLock/updates/` is emptied after every
//! install attempt, successful or not. The one exception is Windows, where the
//! installer has to survive until it has been started; what is still locked
//! then is removed at the next launch by [`cleanup_stale_downloads`].

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use semver::Version;

use crate::logging;
use crate::update::release::RELEASE_PAGE_PREFIX;
use crate::update::{VerifiedPackage, verify};

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use crate::platform::native_update as native;

/// What `install` achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The new version is in place (or its installer is staged). Call
    /// [`UpdateInstaller::relaunch`], then quit.
    RestartPending,
    /// Updating in place is not possible here (not an app bundle, no
    /// `$APPIMAGE`, a location that cannot be written, macOS App
    /// Translocation). Nothing was changed. The URL is the release page, to
    /// open only when the user asks for it ([`open_release_page`]).
    ManualRequired(String),
}

/// Why an install, a relaunch or opening the release page failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The unpacked package is not what the release promises (macOS: wrong
    /// layout, signature, Team ID, identifier or version). Nothing was replaced.
    Verification(String),
    /// A local file operation failed.
    Io(String),
    /// A helper program could not be started or reported failure.
    Command(String),
    /// [`UpdateInstaller::relaunch`] without a successful `install` before it.
    NothingToRelaunch,
    /// A URL that [`open_release_page`] refuses to open.
    UrlNotAllowed,
    /// Another `install` of the same [`SystemInstaller`] is still running.
    Busy,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verification(detail) => {
                write!(f, "The update was rejected and not installed: {detail}")
            }
            Self::Io(detail) => write!(f, "Could not install the update: {detail}"),
            Self::Command(detail) => write!(f, "Could not install the update: {detail}"),
            Self::NothingToRelaunch => write!(f, "There is no installed update to start."),
            Self::UrlNotAllowed => write!(f, "Only SignalLock release pages can be opened."),
            Self::Busy => write!(f, "An update is already being installed."),
        }
    }
}

impl std::error::Error for InstallError {}

pub type Result<T> = std::result::Result<T, InstallError>;

pub(crate) fn io_error(context: &str, error: &io::Error) -> InstallError {
    InstallError::Io(format!("{context}: {error}"))
}

/// What a platform's `install` reports: either something to relaunch, or the
/// reason an in-place update is not possible here.
#[derive(Debug)]
pub enum Installed<R> {
    Ready(R),
    Manual(String),
}

/// What `update_probe --dry-run` prints: the steps an install would take, and
/// the reason it would turn into [`InstallOutcome::ManualRequired`], if any.
/// Computed without writing, spawning or deleting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    pub steps: Vec<String>,
    pub manual_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

/// What a finished helper program left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    /// Human-readable exit status for error messages.
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}

/// How the platform install logic starts programs. [`SystemRunner`] is the
/// real one; tests pass a fake that records what was asked.
pub trait CommandRunner {
    /// Runs `program args` to completion and captures its output. `program`
    /// must be an absolute path.
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput>;

    /// Starts `program args` and does not wait: the child outlives this
    /// process, with no inherited standard streams. `remove_env` names
    /// environment variables the child must not inherit.
    fn spawn_detached(
        &self,
        program: &Path,
        args: &[OsString],
        remove_env: &[&str],
    ) -> io::Result<()>;
}

/// The only code in this file that touches `std::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

fn require_absolute(program: &Path) -> io::Result<()> {
    if program.is_absolute() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing to run a relative program path: {}",
                program.display()
            ),
        ))
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput> {
        require_absolute(program)?;
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn spawn_detached(
        &self,
        program: &Path,
        args: &[OsString],
        remove_env: &[&str],
    ) -> io::Result<()> {
        require_absolute(program)?;
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for name in remove_env {
            command.env_remove(name);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own process group: a signal sent to ours does not reach it.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
        }
        command.spawn().map(|_child| ())
    }
}

/// `program arg arg …` for logs and the dry run. An argument with whitespace or
/// a double quote is shown in double quotes (backslash and quote escaped).
/// Lossy for non-UTF-8 paths. For reading only: nothing is ever run from it.
pub(crate) fn describe_command(program: &str, args: &[OsString]) -> String {
    let mut text = program.to_owned();
    for arg in args {
        text.push(' ');
        let arg = arg.to_string_lossy();
        if arg.is_empty() || arg.contains(char::is_whitespace) || arg.contains('"') {
            text.push('"');
            text.push_str(&arg.replace('\\', "\\\\").replace('"', "\\\""));
            text.push('"');
        } else {
            text.push_str(&arg);
        }
    }
    text
}

// ---------------------------------------------------------------------------
// File-system helpers shared by the platform install logic
// ---------------------------------------------------------------------------

// Used by the macOS and Linux install logic; the Linux module is also compiled
// (and its tests run) under `cfg(test)` everywhere.
#[cfg(any(unix, test))]
mod probe {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Whether a new file can be created in `dir`. Tries it, because permission
    /// bits do not tell about read-only volumes, ACLs or immutable flags. The
    /// probe file is removed again.
    pub fn dir_is_writable(dir: &Path) -> bool {
        let n = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let probe = dir.join(format!(".signallock-write-test-{}-{n}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(any(unix, test))]
pub(crate) use probe::dir_is_writable;

/// Removes everything inside `dir` and keeps `dir`. Never follows symlinks.
/// Returns the number of entries that could not be removed (a running Windows
/// installer, for one); those are left for the next launch.
pub(crate) fn empty_dir(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut failed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let result = if is_real_dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if result.is_err() {
            failed += 1;
        }
    }
    failed
}

/// Empties `<cache>/SignalLock/updates/`. Local file work only, no network.
///
/// Called by the installer after every attempt, and by the app once at startup
/// to remove what a previous run could not (the Windows installer that was
/// still running).
pub fn cleanup_stale_downloads() {
    match verify::updates_dir() {
        Ok(dir) => {
            let left = empty_dir(&dir);
            if left > 0 {
                tracing::debug!(
                    target: logging::APP,
                    "{left} downloaded update file(s) are still in use; they are removed at the next launch"
                );
            }
        }
        Err(e) => tracing::debug!(target: logging::APP, "update cache cleanup skipped: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Release page
// ---------------------------------------------------------------------------

/// The release page of `version`, e.g.
/// `https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0`.
pub fn release_page_for(version: &Version) -> String {
    format!("{RELEASE_PAGE_PREFIX}tag/v{version}")
}

/// Whether `url` may be handed to the browser: it must start with
/// `https://github.com/muhammetsafak/signallock/releases/`, hold only plain URL
/// characters, and not contain a dot segment (`/../`, also percent-encoded) that
/// would walk out of that path.
pub fn is_allowed_release_url(url: &str) -> bool {
    const MAX_LEN: usize = 2048;
    let Some(rest) = url.strip_prefix(RELEASE_PAGE_PREFIX) else {
        return false;
    };
    if url.len() > MAX_LEN {
        return false;
    }
    let plain = rest.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.' | b'_' | b'~' | b'/' | b'%' | b'?' | b'=' | b'&' | b'#' | b'+' | b':'
            )
    });
    if !plain {
        return false;
    }
    let lower = rest.to_ascii_lowercase();
    !(lower.contains("..")
        || lower.contains("%2e")
        || lower.contains("%2f")
        || lower.contains("%5c"))
}

/// Opens a SignalLock release page in the default browser. Call it only when
/// the user clicked *Open Download Page*.
pub fn open_release_page(url: &str) -> Result<()> {
    open_release_page_with(&SystemRunner, url)
}

/// [`open_release_page`] with an explicit runner. Refuses before anything runs.
pub fn open_release_page_with(runner: &dyn CommandRunner, url: &str) -> Result<()> {
    if !is_allowed_release_url(url) {
        tracing::warn!(target: logging::APP, "refusing to open a URL outside the release pages");
        return Err(InstallError::UrlNotAllowed);
    }
    open_url(runner, url)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn open_url(runner: &dyn CommandRunner, url: &str) -> Result<()> {
    native::open_url(url, runner)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn open_url(_runner: &dyn CommandRunner, _url: &str) -> Result<()> {
    Err(InstallError::Command(
        "opening a page is not supported on this platform".to_owned(),
    ))
}

// ---------------------------------------------------------------------------
// The installer
// ---------------------------------------------------------------------------

/// What the app needs from the installer.
pub trait UpdateInstaller {
    /// Puts the verified package in place. Blocking: run it under
    /// `spawn_blocking`. On `Ok(RestartPending)` nothing has been started yet;
    /// call [`relaunch`](Self::relaunch). On `Ok(ManualRequired)` and on `Err`
    /// the running installation is untouched.
    fn install(&self, pkg: VerifiedPackage) -> Result<InstallOutcome>;

    /// Starts the new version (macOS and Linux) or the installer (Windows), after
    /// a successful `install`. The caller quits right afterwards.
    fn relaunch(&self) -> Result<()>;
}

/// The installer of this platform.
#[derive(Default)]
pub struct SystemInstaller {
    runner: SystemRunner,
    /// Set while `install` runs. The staging and `.old-<pid>` names are fixed per
    /// process, so two installs at once would delete each other's files (macOS:
    /// the old bundle a rollback needs). A second call is refused, not queued.
    installing: AtomicBool,
    /// Tests point the cleanup at a temp dir instead of the real cache.
    #[cfg(test)]
    updates_dir: Option<std::path::PathBuf>,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    pending: Mutex<Option<native::Relaunch>>,
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    pending: Mutex<Option<()>>,
}

impl SystemInstaller {
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties the updates cache directory.
    fn clean_downloads(&self) {
        #[cfg(test)]
        if let Some(dir) = &self.updates_dir {
            empty_dir(dir);
            return;
        }
        cleanup_stale_downloads();
    }
}

impl UpdateInstaller for SystemInstaller {
    fn install(&self, pkg: VerifiedPackage) -> Result<InstallOutcome> {
        // Before anything else: a refused call must not touch the files (or empty
        // the downloads) of the install that is running.
        if self.installing.swap(true, Ordering::Acquire) {
            tracing::warn!(target: logging::APP, "an update install is already running; request refused");
            return Err(InstallError::Busy);
        }
        let _busy = InstallingGuard(&self.installing);
        let page = release_page_for(pkg.version());
        let installed = install_native(&pkg, &self.runner);
        let mut keep_files = false;
        let outcome = match installed {
            Ok(NativeInstalled::Ready(relaunch)) => {
                keep_files = relaunch_holds_files(&relaunch);
                *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(relaunch);
                tracing::info!(target: logging::APP, "update {} installed, restart pending", pkg.version());
                Ok(InstallOutcome::RestartPending)
            }
            Ok(NativeInstalled::Manual(reason)) => {
                tracing::info!(target: logging::APP, "update cannot be installed in place: {reason}");
                Ok(InstallOutcome::ManualRequired(page))
            }
            Err(e) => {
                tracing::error!(target: logging::APP, "installing update {} failed: {e}", pkg.version());
                Err(e)
            }
        };
        if !keep_files {
            self.clean_downloads();
        }
        outcome
    }

    fn relaunch(&self) -> Result<()> {
        let Some(relaunch) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return Err(InstallError::NothingToRelaunch);
        };
        let held_files = relaunch_holds_files(&relaunch);
        let result = relaunch_native(relaunch, &self.runner);
        if held_files {
            // The handle is closed by now. The installer that was just started
            // keeps its own file locked; that one is removed at the next launch.
            self.clean_downloads();
        }
        match &result {
            Ok(()) => tracing::info!(target: logging::APP, "starting the updated version"),
            Err(e) => tracing::error!(target: logging::APP, "relaunch failed: {e}"),
        }
        result
    }
}

/// Clears [`SystemInstaller::installing`] when `install` ends, on every path.
struct InstallingGuard<'a>(&'a AtomicBool);

impl Drop for InstallingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Steps `install` would take for `package` (a local file named like the
/// release asset) on this platform, and why it would fall back to the release
/// page, if it would. `target` stands in for the running installation (the
/// `.app` bundle on macOS, the AppImage file on Linux); `None` inspects the
/// running process. Writes, spawns and deletes nothing.
pub fn dry_run(package: &Path, version: &Version, target: Option<&Path>) -> DryRunReport {
    dry_run_native(package, version, target)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use native::Relaunch as NativeRelaunch;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
type NativeRelaunch = ();
type NativeInstalled = Installed<NativeRelaunch>;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn install_native(pkg: &VerifiedPackage, runner: &dyn CommandRunner) -> Result<NativeInstalled> {
    native::install(pkg, runner)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn relaunch_native(relaunch: NativeRelaunch, runner: &dyn CommandRunner) -> Result<()> {
    native::relaunch(relaunch, runner)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn relaunch_holds_files(relaunch: &NativeRelaunch) -> bool {
    relaunch.holds_package_files()
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn dry_run_native(package: &Path, version: &Version, target: Option<&Path>) -> DryRunReport {
    native::dry_run(package, version, target)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn install_native(_pkg: &VerifiedPackage, _runner: &dyn CommandRunner) -> Result<NativeInstalled> {
    Ok(Installed::Manual(
        "in-place updates are not supported on this platform".to_owned(),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn relaunch_native(_relaunch: NativeRelaunch, _runner: &dyn CommandRunner) -> Result<()> {
    Err(InstallError::NothingToRelaunch)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn relaunch_holds_files(_relaunch: &NativeRelaunch) -> bool {
    false
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn dry_run_native(_package: &Path, _version: &Version, _target: Option<&Path>) -> DryRunReport {
    DryRunReport {
        steps: Vec::new(),
        manual_reason: Some("in-place updates are not supported on this platform".to_owned()),
    }
}

/// A scripted [`CommandRunner`] for the install logic's tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;
    use std::sync::Mutex;

    use super::{CommandOutput, CommandRunner};

    type Handler = dyn Fn(&Path, &[OsString]) -> io::Result<CommandOutput> + Send + Sync;

    /// Records every request and answers `run` through a closure. `spawn_detached`
    /// is recorded only.
    pub struct FakeRunner {
        handler: Box<Handler>,
        pub calls: Mutex<Vec<String>>,
        pub spawns: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        pub fn new(
            handler: impl Fn(&Path, &[OsString]) -> io::Result<CommandOutput> + Send + Sync + 'static,
        ) -> Self {
            Self {
                handler: Box::new(handler),
                calls: Mutex::new(Vec::new()),
                spawns: Mutex::new(Vec::new()),
            }
        }

        /// A runner whose commands all succeed with empty output.
        pub fn succeeding() -> Self {
            Self::new(|_, _| Ok(ok(String::new(), String::new())))
        }

        pub fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        pub fn spawns(&self) -> Vec<String> {
            self.spawns.lock().unwrap().clone()
        }
    }

    pub fn ok(stdout: String, stderr: String) -> CommandOutput {
        CommandOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout,
            stderr,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn failed(stderr: &str) -> CommandOutput {
        CommandOutput {
            success: false,
            status: "exit status: 1".to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(super::describe_command(&program.to_string_lossy(), args));
            (self.handler)(program, args)
        }

        fn spawn_detached(
            &self,
            program: &Path,
            args: &[OsString],
            remove_env: &[&str],
        ) -> io::Result<()> {
            let mut line = super::describe_command(&program.to_string_lossy(), args);
            if !remove_env.is_empty() {
                line.push_str(&format!(" [env removed: {}]", remove_env.join(",")));
            }
            self.spawns.lock().unwrap().push(line);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeRunner;
    use super::*;
    use crate::platform::test_util::TestDir;

    const BASE: &str = "https://github.com/muhammetsafak/signallock/releases/";

    #[test]
    fn release_urls_under_the_pinned_prefix_are_allowed() {
        for url in [
            "https://github.com/muhammetsafak/signallock/releases/",
            "https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0",
            "https://github.com/muhammetsafak/signallock/releases/tag/v1.2.0-rc.1+build.5",
            "https://github.com/muhammetsafak/signallock/releases/latest",
            "https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0?expanded=true#notes",
        ] {
            assert!(is_allowed_release_url(url), "{url}");
        }
    }

    #[test]
    fn urls_outside_the_prefix_are_refused() {
        for url in [
            "",
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "file:///etc/passwd",
            "file:///Applications/Calculator.app",
            "data:text/html,<script>alert(1)</script>",
            "http://github.com/muhammetsafak/signallock/releases/",
            "https://github.com/muhammetsafak/signallock/releases",
            "https://github.com/muhammetsafak/signallock/",
            "https://github.com/muhammetsafak/other/releases/",
            "https://github.com/someone-else/signallock/releases/",
            "https://github.com.evil.example/muhammetsafak/signallock/releases/",
            "https://evil.example/https://github.com/muhammetsafak/signallock/releases/",
            "HTTPS://github.com/muhammetsafak/signallock/releases/",
            " https://github.com/muhammetsafak/signallock/releases/",
            "ms-msdt:https://github.com/muhammetsafak/signallock/releases/",
        ] {
            assert!(!is_allowed_release_url(url), "{url}");
        }
    }

    #[test]
    fn urls_that_walk_out_of_the_prefix_or_carry_odd_characters_are_refused() {
        for tail in [
            "../../../evil/repo",
            "tag/../../../evil",
            "%2e%2e/%2E%2e/evil",
            "tag%2F..%2Fevil",
            "tag%5Cevil",
            "tag/v1 .0",
            "tag/v1\n.0",
            "tag/v1\".0",
            "tag/v1<script>",
            "tag/v1\\evil",
            "tag/é",
        ] {
            let url = format!("{BASE}{tail}");
            assert!(!is_allowed_release_url(&url), "{url:?}");
        }
        let long = format!("{BASE}{}", "a".repeat(3000));
        assert!(!is_allowed_release_url(&long));
    }

    #[test]
    fn the_release_page_of_a_version_is_an_allowed_url() {
        let version = Version::parse("1.1.0").unwrap();
        let url = release_page_for(&version);
        assert_eq!(url, format!("{BASE}tag/v1.1.0"));
        assert!(is_allowed_release_url(&url));
        let pre = Version::parse("1.2.0-rc.1+b5").unwrap();
        assert!(is_allowed_release_url(&release_page_for(&pre)));
    }

    #[test]
    fn open_release_page_refuses_before_anything_runs() {
        let runner = FakeRunner::succeeding();
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://example.com/",
        ] {
            assert_eq!(
                open_release_page_with(&runner, url),
                Err(InstallError::UrlNotAllowed)
            );
        }
        assert!(runner.calls().is_empty());
        assert!(runner.spawns().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_an_allowed_page_is_opened_with_the_absolute_open_binary() {
        let runner = FakeRunner::succeeding();
        let url = format!("{BASE}tag/v1.1.0");
        open_release_page_with(&runner, &url).unwrap();
        assert_eq!(runner.calls(), vec![format!("/usr/bin/open {url}")]);
    }

    #[test]
    fn a_relative_program_path_is_refused_without_running_it() {
        let runner = SystemRunner;
        let err = runner
            .run(Path::new("ditto"), &[])
            .expect_err("relative path");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = runner
            .spawn_detached(Path::new("./installer"), &[], &[])
            .expect_err("relative path");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn quotes_inside_an_argument_are_escaped_in_the_description() {
        let args = [OsString::from("-R=identifier \"com.x\"")];
        assert_eq!(
            describe_command("/usr/bin/codesign", &args),
            "/usr/bin/codesign \"-R=identifier \\\"com.x\\\"\""
        );
    }

    #[test]
    fn commands_are_described_with_quoting_only_where_needed() {
        let args = [
            OsString::from("-x"),
            OsString::from("/tmp/with space/file.zip"),
        ];
        assert_eq!(
            describe_command("/usr/bin/ditto", &args),
            "/usr/bin/ditto -x \"/tmp/with space/file.zip\""
        );
    }

    #[test]
    fn empty_dir_removes_files_and_directories_but_keeps_the_directory() {
        let dir = TestDir::new("empty-dir");
        fs::write(dir.path().join("a.part"), b"x").unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/deeper/setup.exe"), b"x").unwrap();
        assert_eq!(empty_dir(dir.path()), 0);
        assert!(dir.path().is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn empty_dir_removes_a_symlink_without_following_it() {
        let dir = TestDir::new("empty-dir-link");
        let outside = TestDir::new("empty-dir-outside");
        fs::write(outside.path().join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert_eq!(empty_dir(dir.path()), 0);
        assert!(!dir.path().join("link").exists());
        assert!(outside.path().join("keep.txt").exists());
    }

    #[test]
    fn dir_is_writable_leaves_no_probe_file_behind() {
        let dir = TestDir::new("writable");
        assert!(dir_is_writable(dir.path()));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(!dir_is_writable(&dir.path().join("does-not-exist")));
    }

    #[cfg(unix)]
    #[test]
    fn dir_is_writable_is_false_for_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("read-only");
        let sealed = TestDir::new("sealed");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o000)).unwrap();
        let writable = dir_is_writable(dir.path());
        // root ignores mode bits: a directory without any permission is still readable.
        let ignores_modes = fs::read_dir(sealed.path()).is_ok();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!writable || ignores_modes);
    }

    /// On macOS a test binary is not an `.app` bundle and on Linux there is no
    /// `$APPIMAGE`, so `install` answers `ManualRequired`; either way the
    /// downloaded files are gone afterwards.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_manual_outcome_still_empties_the_updates_directory() {
        if std::env::var_os("APPIMAGE").is_some() {
            return; // the test binary itself runs from an AppImage
        }
        let updates = TestDir::new("installer-cleanup");
        let name = "SignalLock-1.1.0-x86_64.AppImage";
        let package = updates.path().join(name);
        fs::write(&package, b"package").unwrap();
        fs::write(updates.path().join("other.part"), b"leftover").unwrap();
        let installer = SystemInstaller {
            updates_dir: Some(updates.path().to_owned()),
            ..SystemInstaller::default()
        };
        let version = Version::parse("1.1.0").unwrap();
        let pkg = VerifiedPackage::for_test(package, version.clone(), name);
        assert_eq!(
            installer.install(pkg),
            Ok(InstallOutcome::ManualRequired(release_page_for(&version)))
        );
        assert_eq!(fs::read_dir(updates.path()).unwrap().count(), 0);
        assert_eq!(installer.relaunch(), Err(InstallError::NothingToRelaunch));
    }

    /// A second `install` while one is running is refused and must neither touch
    /// the downloaded files nor clear the gate of the running one.
    #[test]
    fn a_second_install_while_one_is_running_is_refused_without_touching_files() {
        let updates = TestDir::new("installer-busy");
        let name = "SignalLock-1.1.0-x86_64.AppImage";
        let package = updates.path().join(name);
        fs::write(&package, b"package").unwrap();
        let installer = SystemInstaller {
            updates_dir: Some(updates.path().to_owned()),
            ..SystemInstaller::default()
        };
        let version = Version::parse("1.1.0").unwrap();
        let pkg = VerifiedPackage::for_test(package.clone(), version.clone(), name);

        installer.installing.store(true, Ordering::SeqCst); // the first install is running
        assert_eq!(installer.install(pkg.clone()), Err(InstallError::Busy));
        assert_eq!(fs::read(&package).unwrap(), b"package", "files untouched");
        assert!(
            installer.installing.load(Ordering::SeqCst),
            "the running install keeps its gate"
        );

        // Once it has finished the gate is open again, on every outcome.
        installer.installing.store(false, Ordering::SeqCst);
        let first = installer.install(pkg.clone());
        assert!(!installer.installing.load(Ordering::SeqCst));
        assert_ne!(first, Err(InstallError::Busy));
        fs::write(&package, b"package").unwrap();
        assert_ne!(installer.install(pkg), Err(InstallError::Busy));
    }

    #[test]
    fn relaunch_without_a_successful_install_is_an_error() {
        let installer = SystemInstaller::new();
        assert_eq!(installer.relaunch(), Err(InstallError::NothingToRelaunch));
    }
}
