//! Windows in-app update (ADR-015): run the verified Inno Setup installer
//! silently; the installer closes the app, replaces the files and starts the new
//! version from its `[Run]` entry (`packaging/windows/SignalLock.iss`).
//!
//! This file never spawns. The installer is started by the [`CommandRunner`] it
//! is given (`update_service.rs` owns spawning), and the release page is opened
//! with `ShellExecuteW`, a Win32 call and not a process.
//!
//! **The installer is staged, then locked, then started.**
//!
//! 1. A fresh, empty directory is created inside the updates cache (user-only
//!    profile folder) and the installer is moved into it. Nothing else lives
//!    there, so no DLL or other file can be planted beside the installer for it
//!    to load.
//! 2. The file is opened for reading with a share mode that lets nobody else
//!    write to it, rename it or delete it. The handle is held in [`Relaunch`]
//!    until the installer has been started, so the bytes cannot be swapped
//!    between this check and the start.
//! 3. [`relaunch`] starts it with `/SILENT /SUPPRESSMSGBOXES /NORESTART
//!    /CLOSEAPPLICATIONS /SignalLockUpdate=1`. `/RESTARTAPPLICATIONS` is not used
//!    and neither is a separate app start: relaunching is the `.iss` file's job,
//!    through the `IsUpdateInstall` check that `/SignalLockUpdate=1` switches on.
//!
//! The file is compiled under `cfg(test)` on every platform so the staging
//! logic is tested wherever `cargo test` runs. Only the share-mode open and
//! `ShellExecuteW` are Windows-only.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use semver::Version;
use sha2::{Digest, Sha256};

use crate::logging;
use crate::update::VerifiedPackage;
use crate::update_service::{
    CommandRunner, DryRunReport, InstallError, Installed, Result, describe_command, io_error,
};

/// Arguments of the silent update install. The first four are Inno Setup's;
/// the last is read by `IsUpdateInstall` in the `.iss` file and makes its `[Run]`
/// entry start the app after a silent install.
pub const INSTALLER_ARGS: [&str; 5] = [
    "/SILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/CLOSEAPPLICATIONS",
    "/SignalLockUpdate=1",
];

/// The staged installer. Holding the file open (write, rename and delete
/// denied) is the point of keeping this value around until [`relaunch`].
#[derive(Debug)]
pub struct Relaunch {
    installer: PathBuf,
    _lock: File,
}

impl Relaunch {
    /// The installer has to stay on disk until it has been started.
    pub fn holds_package_files(&self) -> bool {
        true
    }
}

fn installer_args() -> Vec<OsString> {
    INSTALLER_ARGS.iter().map(OsString::from).collect()
}

/// Equality without an early exit on the first differing byte.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A file name that is only a plain `*.exe` name: no separators, no `..`.
fn validate_installer_name(name: &str) -> std::result::Result<(), String> {
    let plain = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'+'));
    if plain && !name.starts_with('.') && name.to_ascii_lowercase().ends_with(".exe") {
        Ok(())
    } else {
        Err(format!("unexpected installer file name: {name:?}"))
    }
}

/// Moves `package` into a new, empty directory under `updates_dir` and returns
/// the new path.
fn stage(package: &Path, asset_name: &str, updates_dir: &Path, pid: u32) -> Result<PathBuf> {
    validate_installer_name(asset_name).map_err(InstallError::Verification)?;
    let n = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    // `create_dir` (not `create_dir_all`) fails if the name is already taken, so
    // the directory is new and empty by construction.
    let dir = updates_dir.join(format!("install-{pid}-{n}"));
    fs::create_dir(&dir).map_err(|e| io_error("create the installer folder", &e))?;
    let staged = dir.join(asset_name);
    if let Err(e) = fs::rename(package, &staged) {
        let _ = fs::remove_dir_all(&dir);
        return Err(io_error("move the installer", &e));
    }
    Ok(staged)
}

/// Opens `path` for reading so that nobody else can write to, rename or delete
/// it while the handle lives (`FILE_SHARE_READ` only).
#[cfg(windows)]
fn open_locked(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// Other platforms have no share modes; the test build only needs a handle.
#[cfg(not(windows))]
fn open_locked(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Stages and locks the installer of `pkg`. Nothing is started yet.
pub fn install(pkg: &VerifiedPackage, _runner: &dyn CommandRunner) -> Result<Installed<Relaunch>> {
    let Some(updates_dir) = pkg.path().parent() else {
        return Err(InstallError::Io(
            "the installer has no parent folder".to_owned(),
        ));
    };
    install_at(
        pkg.path(),
        pkg.asset_name(),
        pkg.sha256(),
        updates_dir,
        std::process::id(),
    )
}

/// SHA-256 of everything `file` yields from its current position.
fn hash_open_file(mut file: &File) -> io::Result<[u8; 32]> {
    use io::Read;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..read]);
    }
}

/// [`install`] for an explicit package, asset name, expected SHA-256, updates
/// folder and pid.
///
/// Once the installer is staged and locked it is hashed again through the locked
/// handle and compared with `expected`, the hash Faz 03 verified. The handle
/// denies writes, so the bytes that were hashed are the bytes that will run; a
/// file swapped between the download check and the lock is refused and removed.
pub(crate) fn install_at(
    package: &Path,
    asset_name: &str,
    expected: &[u8; 32],
    updates_dir: &Path,
    pid: u32,
) -> Result<Installed<Relaunch>> {
    let installer = stage(package, asset_name, updates_dir, pid)?;
    let lock = open_locked(&installer).map_err(|e| io_error("lock the installer", &e))?;
    let unchanged = hash_open_file(&lock).map_err(|e| io_error("check the installer", &e))?;
    if !constant_time_eq(&unchanged, expected) {
        drop(lock);
        if let Some(dir) = installer.parent() {
            let _ = fs::remove_dir_all(dir);
        }
        return Err(InstallError::Verification(
            "the installer changed after it was verified".to_owned(),
        ));
    }
    tracing::info!(target: logging::APP, "installer staged at {}", installer.display());
    Ok(Installed::Ready(Relaunch {
        installer,
        _lock: lock,
    }))
}

/// Starts the installer and then lets go of its file. The installer closes the
/// app, so the caller quits right after this returns.
pub fn relaunch(relaunch: Relaunch, runner: &dyn CommandRunner) -> Result<()> {
    let result = runner
        .spawn_detached(&relaunch.installer, &installer_args(), &[])
        .map_err(|e| {
            InstallError::Command(format!(
                "could not start the installer {}: {e}",
                relaunch.installer.display()
            ))
        });
    drop(relaunch);
    result
}

/// Opens `url` in the default browser with `ShellExecuteW`.
#[cfg(windows)]
pub fn open_url(url: &str, _runner: &dyn CommandRunner) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let url = HSTRING::from(url);
    // SAFETY: all string arguments are valid, NUL-terminated UTF-16 that
    // outlive the call (`w!` literals and a local `HSTRING`); a null window
    // handle, parameters and directory are documented as allowed.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &url,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Documented: a value greater than 32 means success, anything else is an error code.
    if (result.0 as usize) > 32 {
        Ok(())
    } else {
        Err(InstallError::Command(format!(
            "ShellExecuteW could not open the release page (code {})",
            result.0 as usize
        )))
    }
}

/// The steps [`install_at`] and [`relaunch`] would take. `package` is only used
/// for its name; `target` is ignored (there is no running copy to replace).
pub fn dry_run(package: &Path, version: &Version, _target: Option<&Path>) -> DryRunReport {
    let updates_dir = package.parent().unwrap_or(package);
    let name = package
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Err(reason) = validate_installer_name(&name) {
        return DryRunReport {
            steps: Vec::new(),
            manual_reason: Some(reason),
        };
    }
    let dir = updates_dir.join(format!("install-{}-<n>", std::process::id()));
    let staged = dir.join(&name);
    DryRunReport {
        steps: vec![
            format!("create the new, empty folder {}", dir.display()),
            format!("move {} -> {}", package.display(), staged.display()),
            format!(
                "open {} for reading and keep it open: other processes may not write to, rename or delete it (FILE_SHARE_READ)",
                staged.display()
            ),
            "(after the app releases its single-instance mutex)".to_owned(),
            format!(
                "start detached: {}",
                describe_command(&staged.to_string_lossy(), &installer_args())
            ),
            "close the handle; the app quits, the installer closes any left-over window,"
                .to_owned(),
            format!("replaces the files and its [Run] entry starts version {version}"),
            "empty the updates cache folder (what is still in use is removed at the next launch)"
                .to_owned(),
        ],
        manual_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;
    use crate::update_service::test_support::FakeRunner;

    const NAME: &str = "SignalLock-1.1.0-setup.exe";

    /// `install_at` with the hash of what is in the package file right now (zeros
    /// when there is no file), as Faz 03 would have recorded it.
    fn install_current(
        package: &Path,
        name: &str,
        updates_dir: &Path,
        pid: u32,
    ) -> Result<Installed<Relaunch>> {
        let hash = fs::read(package)
            .map(|bytes| Sha256::digest(&bytes).into())
            .unwrap_or([0_u8; 32]);
        install_at(package, name, &hash, updates_dir, pid)
    }

    fn version() -> Version {
        Version::parse("1.1.0").unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_command_line_is_the_silent_update_install() {
        assert_eq!(
            INSTALLER_ARGS,
            [
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CLOSEAPPLICATIONS",
                "/SignalLockUpdate=1"
            ]
        );
        assert!(
            !INSTALLER_ARGS.contains(&"/RESTARTAPPLICATIONS"),
            "relaunching is the .iss file's job, not an installer switch"
        );
    }

    #[test]
    fn the_installer_is_moved_into_a_fresh_folder_of_its_own() {
        let updates = TestDir::new("win-stage");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ installer").unwrap();
        let Installed::Ready(staged) =
            install_current(&package, NAME, updates.path(), 4242).unwrap()
        else {
            panic!("expected Ready");
        };
        assert!(staged.holds_package_files());
        assert!(!package.exists(), "the original path is gone");
        assert_eq!(fs::read(&staged.installer).unwrap(), b"MZ installer");
        let dir = staged.installer.parent().unwrap();
        assert_eq!(dir.parent().unwrap(), updates.path());
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("install-4242-")
        );
        assert_eq!(names(dir), vec![NAME], "nothing else beside the installer");
    }

    #[test]
    fn an_installer_that_changed_after_verification_is_refused_and_removed() {
        let updates = TestDir::new("win-changed");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ swapped after the hash was taken").unwrap();
        let verified: [u8; 32] = Sha256::digest(b"MZ the verified installer").into();
        let result = install_at(&package, NAME, &verified, updates.path(), 3);
        assert!(
            matches!(result, Err(InstallError::Verification(_))),
            "{result:?}"
        );
        assert!(names(updates.path()).is_empty(), "nothing is left to run");

        // The same file with the matching hash is accepted.
        fs::write(&package, b"MZ the verified installer").unwrap();
        let result = install_at(&package, NAME, &verified, updates.path(), 3);
        assert!(matches!(result, Ok(Installed::Ready(_))));
    }

    #[test]
    fn two_installs_never_share_a_folder() {
        let updates = TestDir::new("win-stage-twice");
        let first = updates.path().join(NAME);
        fs::write(&first, b"one").unwrap();
        let Installed::Ready(a) = install_current(&first, NAME, updates.path(), 1).unwrap() else {
            panic!("expected Ready");
        };
        fs::write(&first, b"two").unwrap();
        let Installed::Ready(b) = install_current(&first, NAME, updates.path(), 1).unwrap() else {
            panic!("expected Ready");
        };
        assert_ne!(a.installer.parent(), b.installer.parent());
    }

    #[test]
    fn names_that_are_not_a_plain_exe_name_are_refused() {
        let updates = TestDir::new("win-bad-name");
        let package = updates.path().join("x.exe");
        fs::write(&package, b"MZ").unwrap();
        for name in [
            "",
            "..\\evil.exe",
            "../evil.exe",
            "dir/setup.exe",
            "dir\\setup.exe",
            "setup.exe.zip",
            "setup",
            ".hidden.exe",
            "set up.exe",
            "setup.exe ",
            "C:setup.exe",
        ] {
            let result = install_current(&package, name, updates.path(), 1);
            assert!(
                matches!(result, Err(InstallError::Verification(_))),
                "{name:?} -> {result:?}"
            );
        }
        assert!(package.exists(), "a refused name moves nothing");
        assert_eq!(
            names(updates.path()),
            vec!["x.exe"],
            "and creates no folder"
        );
    }

    #[test]
    fn a_missing_package_is_an_error_and_leaves_no_folder() {
        let updates = TestDir::new("win-missing");
        let result = install_current(&updates.path().join(NAME), NAME, updates.path(), 1);
        assert!(matches!(result, Err(InstallError::Io(_))), "{result:?}");
        assert!(names(updates.path()).is_empty());
    }

    #[test]
    fn relaunch_starts_the_staged_installer_detached_with_the_silent_arguments() {
        let updates = TestDir::new("win-relaunch");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ").unwrap();
        let Installed::Ready(staged) = install_current(&package, NAME, updates.path(), 7).unwrap()
        else {
            panic!("expected Ready");
        };
        let installer = staged.installer.clone();
        let runner = FakeRunner::succeeding();
        relaunch(staged, &runner).unwrap();
        assert_eq!(
            runner.spawns(),
            vec![format!(
                "{} /SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /SignalLockUpdate=1",
                installer.display()
            )]
        );
        assert!(runner.calls().is_empty(), "not waited for");
    }

    #[test]
    fn a_failed_start_is_reported() {
        struct Failing;
        impl CommandRunner for Failing {
            fn run(
                &self,
                _: &Path,
                _: &[OsString],
            ) -> io::Result<crate::update_service::CommandOutput> {
                unreachable!("the installer is not waited for")
            }
            fn spawn_detached(&self, _: &Path, _: &[OsString], _: &[&str]) -> io::Result<()> {
                Err(io::Error::other("access denied"))
            }
        }
        let updates = TestDir::new("win-start-fails");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ").unwrap();
        let Installed::Ready(staged) = install_current(&package, NAME, updates.path(), 7).unwrap()
        else {
            panic!("expected Ready");
        };
        assert!(matches!(
            relaunch(staged, &Failing),
            Err(InstallError::Command(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn the_locked_handle_denies_writing_renaming_and_deleting() {
        let updates = TestDir::new("win-lock");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ").unwrap();
        let Installed::Ready(staged) = install_current(&package, NAME, updates.path(), 7).unwrap()
        else {
            panic!("expected Ready");
        };
        let path = staged.installer.clone();
        assert!(fs::OpenOptions::new().write(true).open(&path).is_err());
        assert!(fs::remove_file(&path).is_err());
        assert!(fs::rename(&path, path.with_extension("old")).is_err());
        assert!(
            File::open(&path).is_ok(),
            "reading (running) is still allowed"
        );
        drop(staged);
        assert!(fs::remove_file(&path).is_ok());
    }

    #[test]
    fn the_dry_run_describes_the_staging_and_the_command() {
        let updates = TestDir::new("win-dry-run");
        let package = updates.path().join(NAME);
        fs::write(&package, b"MZ").unwrap();
        let report = dry_run(&package, &version(), None);
        assert_eq!(report.manual_reason, None);
        let text = report.steps.join("\n");
        assert!(text.contains("FILE_SHARE_READ"));
        assert!(text.contains(
            "/SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /SignalLockUpdate=1"
        ));
        assert_eq!(
            names(updates.path()),
            vec![NAME],
            "a dry run creates nothing"
        );
    }

    #[test]
    fn the_dry_run_refuses_a_package_that_is_not_an_exe() {
        let report = dry_run(
            Path::new("/tmp/SignalLock-1.1.0-macos.app.zip"),
            &version(),
            None,
        );
        assert!(report.steps.is_empty());
        assert!(report.manual_reason.is_some());
    }
}
