//! Linux in-app update (ADR-015): only an AppImage started through `$APPIMAGE`
//! is updated in place. The new file is written next to the running one and
//! renamed over it, which is atomic: `$APPIMAGE` is always either the old or
//! the new file. `.deb`, `.rpm` and Flatpak installs are not touched.
//!
//! This file never spawns. Starting the new AppImage and `xdg-open` go through
//! the [`CommandRunner`] it is given (`update_service.rs` owns spawning).
//!
//! The file is compiled under `cfg(test)` on every platform so that the
//! replacement logic is tested wherever `cargo test` runs; the unix-only calls
//! (file modes) are gated with `cfg(unix)`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::logging;
use crate::update::VerifiedPackage;
use crate::update_service::{
    CommandRunner, DryRunReport, InstallError, Installed, LIGHT_COMMAND_TIMEOUT, Result,
    describe_command, dir_is_writable, io_error,
};

/// Variables the AppImage runtime sets for the process it starts. The new
/// AppImage sets its own; the old ones point into the old, soon unmounted, image.
const APPIMAGE_RUNTIME_VARS: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

/// Where `xdg-open` lives on the distributions that ship it. `PATH` is not
/// searched.
const XDG_OPEN_CANDIDATES: [&str; 3] = [
    "/usr/bin/xdg-open",
    "/bin/xdg-open",
    "/usr/local/bin/xdg-open",
];

/// What to start after a successful replacement: the AppImage at `$APPIMAGE`.
#[derive(Debug)]
pub struct Relaunch {
    appimage: PathBuf,
}

impl Relaunch {
    /// Whether files under the updates directory must be kept until
    /// [`relaunch`] has run. Not on Linux: the AppImage has been copied.
    pub fn holds_package_files(&self) -> bool {
        false
    }
}

/// What the AppImage runtime told this process about itself, plus where the
/// process really runs from.
///
/// `$APPIMAGE` alone proves nothing: the runtime exports it to every child, so a
/// `signallock` started from a terminal inside *another* AppImage (an editor,
/// say) inherits that AppImage's path. The update only trusts it when the chain
/// `current_exe` -> `$APPDIR` -> `$APPIMAGE` holds (see [`resolve_target`]).
#[derive(Debug, Clone, Default)]
pub struct AppImageEnv {
    /// `$APPIMAGE`.
    pub appimage: Option<OsString>,
    /// `$APPDIR`, the runtime's mount point of the image.
    pub appdir: Option<OsString>,
    /// `std::env::current_exe()` (`/proc/self/exe` on Linux).
    pub exe: Option<PathBuf>,
}

impl AppImageEnv {
    /// Reads the variables and the executable path of this process.
    pub fn from_process() -> Self {
        Self {
            appimage: std::env::var_os("APPIMAGE"),
            appdir: std::env::var_os("APPDIR"),
            exe: std::env::current_exe().ok(),
        }
    }
}

/// An absolute, non-empty path from an environment value.
fn absolute_env_path<'a>(
    value: Option<&'a OsString>,
    name: &str,
) -> std::result::Result<&'a Path, String> {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return Err(format!("{name} is not set; not running as an AppImage"));
    };
    let path = Path::new(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{name} is not an absolute path"))
    }
}

/// The process must run from the AppImage mount that `$APPDIR` names:
/// `canonicalize(current_exe)` lies under `canonicalize($APPDIR)`, and that
/// directory looks like an AppDir (it holds `AppRun`, which every AppImage
/// has). This is what separates a SignalLock that is the AppImage from a
/// SignalLock (`.deb`, `.rpm`, a build) that merely inherited the variables.
fn check_running_from_appdir(env: &AppImageEnv) -> std::result::Result<(), String> {
    let appdir = absolute_env_path(env.appdir.as_ref(), "APPDIR")?;
    let appdir = fs::canonicalize(appdir).map_err(|e| format!("APPDIR cannot be resolved: {e}"))?;
    if appdir.parent().is_none() || !appdir.join("AppRun").exists() {
        return Err("APPDIR is not an AppImage mount".to_owned());
    }
    let Some(exe) = env.exe.as_ref() else {
        return Err("the running executable is unknown".to_owned());
    };
    let exe = fs::canonicalize(exe).map_err(|e| format!("the running executable: {e}"))?;
    if exe.starts_with(&appdir) {
        Ok(())
    } else {
        Err(
            "this process does not run from the AppImage that APPIMAGE names \
             (the variables were inherited)"
                .to_owned(),
        )
    }
}

/// `SignalLock*.AppImage`, case-insensitive: the file about to be overwritten
/// must at least be named like ours.
fn is_signal_lock_file_name(real: &Path) -> bool {
    real.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|n| n.starts_with("signallock") && n.ends_with(".appimage"))
}

/// Resolves `$APPIMAGE` to the real file, or says why it must not be trusted.
/// `verify_process` is off only for the dry run with an explicit `--target`.
fn resolve_identity(
    env: &AppImageEnv,
    verify_process: bool,
) -> std::result::Result<PathBuf, String> {
    let path = absolute_env_path(env.appimage.as_ref(), "APPIMAGE")?;
    if verify_process {
        check_running_from_appdir(env)?;
    }
    let real = fs::canonicalize(path).map_err(|e| format!("APPIMAGE cannot be resolved: {e}"))?;
    if !is_signal_lock_file_name(&real) {
        return Err(format!(
            "{} is not a SignalLock AppImage (its name does not start with SignalLock and end with .AppImage)",
            real.display()
        ));
    }
    let meta = fs::metadata(&real).map_err(|e| format!("APPIMAGE cannot be read: {e}"))?;
    if !meta.is_file() {
        return Err("APPIMAGE is not a regular file".to_owned());
    }
    Ok(real)
}

/// The running AppImage file, or why an in-place update is not possible.
///
/// Trusted only when [`check_running_from_appdir`] and the file name check pass
/// (see [`AppImageEnv`]); then it must be a regular file (a symlink is followed
/// to the real file), not read-only, in a directory that accepts a new file.
pub fn resolve_target(env: &AppImageEnv) -> std::result::Result<PathBuf, String> {
    let real = resolve_identity(env, true)?;
    let meta = fs::metadata(&real).map_err(|e| format!("APPIMAGE cannot be read: {e}"))?;
    if meta.permissions().readonly() {
        return Err("the AppImage file is read-only".to_owned());
    }
    let Some(dir) = real.parent() else {
        return Err("the AppImage has no parent directory".to_owned());
    };
    if !dir_is_writable(dir) {
        return Err("the folder that holds the AppImage is not writable".to_owned());
    }
    Ok(real)
}

fn temp_path(target: &Path, pid: u32) -> PathBuf {
    target.with_file_name(format!(".SignalLock.update-{pid}"))
}

/// Removes the temp file on drop unless it was renamed into place.
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_executable(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    options.open(path)
}

/// Copies `package` next to `target` as `.SignalLock.update-<pid>`, makes it
/// `0755` and renames it over `target`. On any failure the temp file is removed
/// and `target` is unchanged.
pub fn replace_file(target: &Path, package: &Path, pid: u32) -> io::Result<()> {
    let tmp_path = temp_path(target, pid);
    // A leftover of a crashed attempt with the same pid.
    let _ = fs::remove_file(&tmp_path);
    let mut tmp = TempFile {
        path: tmp_path,
        armed: true,
    };

    let mut source = File::open(package)?;
    let expected = source.metadata()?.len();
    let mut file = create_executable(&tmp.path)?;
    let copied = io::copy(&mut source, &mut file)?;
    if copied != expected {
        return Err(io::Error::other(format!(
            "copied {copied} of {expected} bytes"
        )));
    }
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` above is filtered by the umask; this is not.
        fs::set_permissions(&tmp.path, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&tmp.path, target)?;
    tmp.armed = false;
    Ok(())
}

/// Installs `pkg` over the running AppImage.
pub fn install(pkg: &VerifiedPackage, _runner: &dyn CommandRunner) -> Result<Installed<Relaunch>> {
    install_at(&AppImageEnv::from_process(), pkg.path(), std::process::id())
}

/// [`install`] with explicit environment, package and process id.
pub(crate) fn install_at(
    env: &AppImageEnv,
    package: &Path,
    pid: u32,
) -> Result<Installed<Relaunch>> {
    let target = match resolve_target(env) {
        Ok(target) => target,
        Err(reason) => return Ok(Installed::Manual(reason)),
    };
    replace_file(&target, package, pid).map_err(|e| io_error("replace the AppImage", &e))?;
    tracing::info!(target: logging::APP, "replaced {}", target.display());
    Ok(Installed::Ready(Relaunch { appimage: target }))
}

/// Starts the new AppImage. It does not inherit the runtime variables of the
/// old one.
pub fn relaunch(relaunch: Relaunch, runner: &dyn CommandRunner) -> Result<()> {
    runner
        .spawn_detached(&relaunch.appimage, &[], &APPIMAGE_RUNTIME_VARS)
        .map_err(|e| {
            InstallError::Command(format!(
                "could not start {}: {e}",
                relaunch.appimage.display()
            ))
        })
}

fn find_xdg_open(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(Path::to_owned)
}

/// Opens `url` with `xdg-open`, waiting for it for at most 15 s. Inside an AppImage
/// the bundled library path would break the system browser, so it is dropped.
pub fn open_url(url: &str, runner: &dyn CommandRunner) -> Result<()> {
    open_url_with(
        url,
        runner,
        &XDG_OPEN_CANDIDATES,
        std::env::var_os("APPDIR").is_some(),
    )
}

fn open_url_with(
    url: &str,
    runner: &dyn CommandRunner,
    candidates: &[&str],
    inside_appimage: bool,
) -> Result<()> {
    let Some(xdg_open) = find_xdg_open(candidates) else {
        return Err(InstallError::Command(
            "xdg-open was not found in /usr/bin, /bin or /usr/local/bin".to_owned(),
        ));
    };
    let remove: &[&str] = if inside_appimage {
        &["LD_LIBRARY_PATH"]
    } else {
        &[]
    };
    // Waited for, with a bound: `xdg-open` hands the URL to the browser and
    // returns; if it hangs it is killed after 15 s instead of living on.
    let output = runner
        .run(
            &xdg_open,
            &[OsString::from(url)],
            LIGHT_COMMAND_TIMEOUT,
            remove,
        )
        .map_err(|e| InstallError::Command(format!("could not run xdg-open: {e}")))?;
    if output.success {
        Ok(())
    } else {
        Err(InstallError::Command(format!(
            "xdg-open {}: {}",
            output.status,
            output.stderr.trim()
        )))
    }
}

/// The steps [`install_at`] would take. `target` stands in for `$APPIMAGE`
/// (then the check that this process runs from that AppImage is skipped and the
/// report says so); `None` reads the environment and checks it fully. Writes
/// nothing (the writability of the folder is judged from its permission bits,
/// not probed).
pub fn dry_run(package: &Path, version: &Version, target: Option<&Path>) -> DryRunReport {
    match target {
        Some(path) => {
            let env = AppImageEnv {
                appimage: Some(path.as_os_str().to_owned()),
                ..AppImageEnv::default()
            };
            dry_run_at(&env, false, package, version, std::process::id())
        }
        None => dry_run_at(
            &AppImageEnv::from_process(),
            true,
            package,
            version,
            std::process::id(),
        ),
    }
}

fn dry_run_at(
    env: &AppImageEnv,
    verify_process: bool,
    package: &Path,
    version: &Version,
    pid: u32,
) -> DryRunReport {
    let manual = |reason: String| DryRunReport {
        steps: Vec::new(),
        manual_reason: Some(format!("{reason}; would offer the release page")),
    };
    let real = match resolve_identity(env, verify_process) {
        Ok(real) => real,
        Err(reason) => return manual(reason),
    };
    if fs::metadata(&real).is_ok_and(|m| m.permissions().readonly()) {
        return manual("the AppImage file is read-only".to_owned());
    }
    let dir = real.parent().unwrap_or(&real);
    // Permission bits only: a dry run writes nothing, so it cannot probe.
    if fs::metadata(dir).is_ok_and(|m| m.permissions().readonly()) {
        return manual(format!("{} is not writable", dir.display()));
    }
    let chain = if verify_process {
        "require: current_exe is under $APPDIR, $APPDIR holds AppRun, the file is named SignalLock*.AppImage"
    } else {
        "(--target given: the check that this process runs from that AppImage is skipped; \
         the SignalLock*.AppImage name check still applies)"
    };

    let tmp = temp_path(&real, pid);
    DryRunReport {
        steps: vec![
            format!("running AppImage: {}", real.display()),
            chain.to_owned(),
            format!(
                "check that {} can be written (a probe file is created and removed)",
                dir.display()
            ),
            format!(
                "copy {} -> {} (mode 0755)",
                package.display(),
                tmp.display()
            ),
            format!(
                "require the copy to be {} bytes long",
                package.metadata().map_or(0, |m| m.len())
            ),
            format!("rename {} -> {} (atomic)", tmp.display(), real.display()),
            format!("expected version after the update: {version}"),
            "(after the app releases its single-instance guard)".to_owned(),
            format!(
                "start detached: {} (without {})",
                describe_command(&real.to_string_lossy(), &[]),
                APPIMAGE_RUNTIME_VARS.join(", ")
            ),
        ],
        manual_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;
    use crate::update_service::test_support::{FakeRunner, RunRequest, failed};

    fn version() -> Version {
        Version::parse("1.1.0").unwrap()
    }

    fn file_with(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// An AppDir mount (`AppRun`, `usr/bin/signallock`) in its own temp dir and
    /// the environment of a SignalLock that runs from it as `appimage`. The
    /// `TestDir` must outlive the use of the environment.
    fn running_from(appimage: &Path) -> (TestDir, AppImageEnv) {
        let mount = TestDir::new("linux-appdir");
        fs::create_dir_all(mount.path().join("usr/bin")).unwrap();
        file_with(mount.path(), "AppRun", b"#!/bin/sh");
        let exe = file_with(&mount.path().join("usr/bin"), "signallock", b"elf");
        let env = AppImageEnv {
            appimage: Some(appimage.as_os_str().to_owned()),
            appdir: Some(mount.path().as_os_str().to_owned()),
            exe: Some(exe),
        };
        (mount, env)
    }

    // ----- which file is the target -----

    #[test]
    fn without_appimage_the_update_is_manual() {
        assert!(resolve_target(&AppImageEnv::default()).is_err());
        let empty = AppImageEnv {
            appimage: Some(OsString::new()),
            ..AppImageEnv::default()
        };
        assert!(resolve_target(&empty).is_err());
    }

    #[test]
    fn a_relative_or_missing_appimage_path_is_manual() {
        let dir = TestDir::new("linux-missing");
        let gone = dir.path().join("SignalLock-gone.AppImage");
        let (_mount, env) = running_from(&gone);
        assert!(resolve_target(&env).is_err());
        let (_mount, mut env) = running_from(&gone);
        env.appimage = Some(OsString::from("SignalLock.AppImage"));
        assert!(resolve_target(&env).is_err());
    }

    #[test]
    fn a_directory_is_not_an_appimage() {
        let dir = TestDir::new("linux-dir-target");
        let named = dir.path().join("SignalLock.AppImage");
        fs::create_dir(&named).unwrap();
        let (_mount, env) = running_from(&named);
        assert!(resolve_target(&env).is_err());
    }

    #[test]
    fn a_writable_appimage_that_this_process_runs_from_resolves_to_its_real_path() {
        let dir = TestDir::new("linux-target-ok");
        let appimage = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let (_mount, env) = running_from(&appimage);
        let resolved = resolve_target(&env).unwrap();
        assert_eq!(resolved, fs::canonicalize(&appimage).unwrap());
        assert_eq!(
            names(dir.path()),
            vec!["SignalLock.AppImage"],
            "no probe file left"
        );
    }

    #[test]
    fn file_names_like_signal_lock_app_images_are_accepted_case_insensitively() {
        for name in [
            "SignalLock.AppImage",
            "SignalLock-1.0.0-x86_64.AppImage",
            "signallock.appimage",
            "SIGNALLOCK.APPIMAGE",
        ] {
            let dir = TestDir::new("linux-name-ok");
            let appimage = file_with(dir.path(), name, b"old");
            let (_mount, env) = running_from(&appimage);
            assert!(resolve_target(&env).is_ok(), "{name}");
        }
    }

    /// The reviewer's scenario: `signallock` (a `.deb` install) is started from a
    /// terminal inside another AppImage and inherits its `APPIMAGE` and `APPDIR`.
    /// The other AppImage is a perfectly consistent mount, and the file is
    /// writable, but this process does not run from it: Manual, nothing touched.
    #[test]
    fn variables_inherited_from_another_appimage_are_not_trusted() {
        let apps = TestDir::new("linux-inherited");
        let other = file_with(apps.path(), "Obsidian.AppImage", b"obsidian");
        let (_other_mount, mut env) = running_from(&other);
        // The process is a package install, not inside the other AppImage's mount.
        let installed = TestDir::new("linux-inherited-usr");
        env.exe = Some(file_with(installed.path(), "signallock", b"elf"));
        let reason = resolve_target(&env).expect_err("inherited variables");
        assert!(reason.contains("inherited"), "{reason}");

        // Same, with a file that is named like ours: still refused, because the
        // process does not run from it.
        let ours = file_with(apps.path(), "SignalLock.AppImage", b"signallock");
        env.appimage = Some(ours.as_os_str().to_owned());
        let reason = resolve_target(&env).expect_err("not running from it");
        assert!(reason.contains("inherited"), "{reason}");

        let package_dir = TestDir::new("linux-inherited-pkg");
        let package = file_with(package_dir.path(), "p", b"new");
        let Installed::Manual(_) = install_at(&env, &package, 3).unwrap() else {
            panic!("expected Manual");
        };
        assert_eq!(fs::read(&other).unwrap(), b"obsidian");
        assert_eq!(fs::read(&ours).unwrap(), b"signallock");
        assert_eq!(
            names(apps.path()),
            vec!["Obsidian.AppImage", "SignalLock.AppImage"]
        );
    }

    #[test]
    fn without_appdir_or_without_the_executable_the_update_is_manual() {
        let dir = TestDir::new("linux-no-appdir");
        let appimage = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let (_mount, env) = running_from(&appimage);

        let mut no_appdir = env.clone();
        no_appdir.appdir = None;
        assert!(resolve_target(&no_appdir).unwrap_err().contains("APPDIR"));

        let mut relative = env.clone();
        relative.appdir = Some(OsString::from("tmp/.mount_x"));
        assert!(resolve_target(&relative).is_err());

        let mut no_exe = env.clone();
        no_exe.exe = None;
        assert!(resolve_target(&no_exe).is_err());

        assert!(
            resolve_target(&env).is_ok(),
            "the intact environment passes"
        );
    }

    #[test]
    fn an_appdir_that_is_not_an_appimage_mount_is_manual() {
        let dir = TestDir::new("linux-not-mount");
        let appimage = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let (mount, env) = running_from(&appimage);
        fs::remove_file(mount.path().join("AppRun")).unwrap();
        assert!(resolve_target(&env).unwrap_err().contains("AppImage mount"));

        // APPDIR=/ would make every executable "inside" it.
        let mut root = env.clone();
        root.appdir = Some(OsString::from("/"));
        root.exe = std::env::current_exe().ok();
        assert!(resolve_target(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_that_links_out_of_the_mount_is_not_inside_it() {
        let dir = TestDir::new("linux-exe-link");
        let appimage = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let (mount, mut env) = running_from(&appimage);
        let outside = TestDir::new("linux-exe-link-outside");
        let real_exe = file_with(outside.path(), "signallock", b"elf");
        let link = mount.path().join("usr/bin/linked");
        std::os::unix::fs::symlink(&real_exe, &link).unwrap();
        env.exe = Some(link);
        assert!(resolve_target(&env).unwrap_err().contains("inherited"));
    }

    #[test]
    fn a_file_that_is_not_named_like_ours_is_never_overwritten() {
        for name in [
            "Obsidian.AppImage",
            "SignalLock",
            "SignalLock.AppImage.bak",
            "NotSignalLock.AppImage",
            "signal.AppImage",
        ] {
            let dir = TestDir::new("linux-name-bad");
            let appimage = file_with(dir.path(), name, b"other");
            // Even when the process really runs from it.
            let (_mount, env) = running_from(&appimage);
            let reason = resolve_target(&env).expect_err(name);
            assert!(
                reason.contains("not a SignalLock AppImage"),
                "{name}: {reason}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_appimage_resolves_to_the_file_behind_it() {
        let dir = TestDir::new("linux-symlink");
        let real = file_with(dir.path(), "SignalLock-1.0.0.AppImage", b"old");
        let link = dir.path().join("SignalLock.AppImage");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (_mount, env) = running_from(&link);
        let resolved = resolve_target(&env).unwrap();
        assert_eq!(resolved, fs::canonicalize(&real).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_with_our_name_to_a_foreign_file_is_refused() {
        let dir = TestDir::new("linux-symlink-foreign");
        let foreign = file_with(dir.path(), "Obsidian.AppImage", b"obsidian");
        let link = dir.path().join("SignalLock.AppImage");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();
        let (_mount, env) = running_from(&link);
        assert!(resolve_target(&env).is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"obsidian");
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_or_folder_is_manual() {
        use std::os::unix::fs::PermissionsExt;
        let sealed = TestDir::new("linux-sealed-probe");
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o000)).unwrap();
        let ignores_modes = fs::read_dir(sealed.path()).is_ok();
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if ignores_modes {
            return; // running as root: mode bits cannot make anything unwritable
        }

        let file_dir = TestDir::new("linux-ro-file");
        let appimage = file_with(file_dir.path(), "SignalLock.AppImage", b"old");
        fs::set_permissions(&appimage, fs::Permissions::from_mode(0o555)).unwrap();
        let (_mount, env) = running_from(&appimage);
        let reason = resolve_target(&env).expect_err("read-only file");
        assert!(reason.contains("read-only"), "{reason}");

        let ro_dir = TestDir::new("linux-ro-dir");
        let appimage = file_with(ro_dir.path(), "SignalLock.AppImage", b"old");
        let (_mount, env) = running_from(&appimage);
        fs::set_permissions(ro_dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let result = resolve_target(&env);
        fs::set_permissions(ro_dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let reason = result.expect_err("read-only folder");
        assert!(reason.contains("not writable"), "{reason}");
    }

    // ----- the replacement -----

    #[test]
    fn replace_swaps_the_file_atomically_and_leaves_nothing_behind() {
        let dir = TestDir::new("linux-replace");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old version");
        let package_dir = TestDir::new("linux-replace-pkg");
        let package = file_with(
            package_dir.path(),
            "SignalLock-1.1.0-x86_64.AppImage",
            b"new version, longer",
        );
        replace_file(&target, &package, 4242).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new version, longer");
        assert_eq!(names(dir.path()), vec!["SignalLock.AppImage"]);
        assert!(package.exists(), "the package is the caller's to delete");
    }

    #[cfg(unix)]
    #[test]
    fn the_new_file_is_executable_whatever_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new("linux-mode");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let package_dir = TestDir::new("linux-mode-pkg");
        let package = file_with(package_dir.path(), "p", b"new");
        replace_file(&target, &package, 1).unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn a_missing_package_leaves_the_target_and_the_folder_unchanged() {
        let dir = TestDir::new("linux-no-package");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let err = replace_file(&target, &dir.path().join("missing"), 7);
        assert!(err.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(names(dir.path()), vec!["SignalLock.AppImage"]);
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file_and_keeps_the_old_one() {
        // The target is a non-empty directory, so renaming a file over it fails
        // after the copy has been written.
        let dir = TestDir::new("linux-rename-fails");
        let target = dir.path().join("SignalLock.AppImage");
        fs::create_dir_all(target.join("inner")).unwrap();
        let package_dir = TestDir::new("linux-rename-fails-pkg");
        let package = file_with(package_dir.path(), "p", b"new");
        assert!(replace_file(&target, &package, 9).is_err());
        assert!(target.join("inner").is_dir());
        assert_eq!(
            names(dir.path()),
            vec!["SignalLock.AppImage"],
            "temp file removed"
        );
    }

    #[test]
    fn a_leftover_temp_file_with_the_same_pid_does_not_block_the_update() {
        let dir = TestDir::new("linux-leftover");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old");
        file_with(dir.path(), ".SignalLock.update-5", b"half written");
        let package_dir = TestDir::new("linux-leftover-pkg");
        let package = file_with(package_dir.path(), "p", b"new");
        replace_file(&target, &package, 5).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(names(dir.path()), vec!["SignalLock.AppImage"]);
    }

    #[test]
    fn install_at_replaces_the_file_and_reports_what_to_relaunch() {
        let dir = TestDir::new("linux-install");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let package_dir = TestDir::new("linux-install-pkg");
        let package = file_with(package_dir.path(), "p", b"new");
        let (_mount, env) = running_from(&target);
        let Installed::Ready(relaunch_info) = install_at(&env, &package, 3).unwrap() else {
            panic!("expected Ready");
        };
        assert_eq!(relaunch_info.appimage, fs::canonicalize(&target).unwrap());
        assert!(!relaunch_info.holds_package_files());
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn install_at_without_appimage_is_manual_and_changes_nothing() {
        let package_dir = TestDir::new("linux-install-manual");
        let package = file_with(package_dir.path(), "p", b"new");
        let Installed::Manual(reason) = install_at(&AppImageEnv::default(), &package, 3).unwrap()
        else {
            panic!("expected Manual");
        };
        assert!(reason.contains("APPIMAGE"), "{reason}");
    }

    // ----- relaunch and xdg-open -----

    #[test]
    fn relaunch_starts_the_appimage_without_the_old_runtime_variables() {
        let runner = FakeRunner::succeeding();
        relaunch(
            Relaunch {
                appimage: PathBuf::from("/home/me/Apps/SignalLock.AppImage"),
            },
            &runner,
        )
        .unwrap();
        assert_eq!(
            runner.spawns(),
            vec!["/home/me/Apps/SignalLock.AppImage [env removed: APPIMAGE,APPDIR,ARGV0,OWD]"]
        );
        assert!(
            runner.calls().is_empty(),
            "started detached, not waited for"
        );
    }

    #[test]
    fn xdg_open_is_found_by_absolute_path_only() {
        let dir = TestDir::new("linux-xdg");
        let present = file_with(dir.path(), "xdg-open", b"#!/bin/sh");
        let present_str = present.to_str().unwrap();
        assert_eq!(
            find_xdg_open(&["/nonexistent/xdg-open", present_str]),
            Some(present.clone())
        );
        assert_eq!(find_xdg_open(&["/nonexistent/xdg-open", "xdg-open"]), None);

        let runner = FakeRunner::succeeding();
        let url = "https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0";
        open_url_with(url, &runner, &[present_str], false).unwrap();
        open_url_with(url, &runner, &[present_str], true).unwrap();
        assert_eq!(
            runner.requests(),
            vec![
                RunRequest {
                    line: format!("{present_str} {url}"),
                    timeout: LIGHT_COMMAND_TIMEOUT,
                    remove_env: vec![],
                },
                RunRequest {
                    line: format!("{present_str} {url}"),
                    timeout: LIGHT_COMMAND_TIMEOUT,
                    remove_env: vec!["LD_LIBRARY_PATH".to_owned()],
                },
            ]
        );
        assert!(runner.spawns().is_empty(), "waited for, not detached");
        assert!(matches!(
            open_url_with(url, &runner, &["/nonexistent/xdg-open"], false),
            Err(InstallError::Command(_))
        ));
    }

    #[test]
    fn an_xdg_open_that_hangs_or_fails_is_reported() {
        let dir = TestDir::new("linux-xdg-fail");
        let present = file_with(dir.path(), "xdg-open", b"#!/bin/sh");
        let present_str = present.to_str().unwrap();
        let url = "https://github.com/muhammetsafak/signallock/releases/";

        let hung = FakeRunner::new(|_, _| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out after 15s and was killed",
            ))
        });
        let err = open_url_with(url, &hung, &[present_str], false).expect_err("timeout");
        assert!(
            matches!(&err, InstallError::Command(m) if m.contains("timed out")),
            "{err:?}"
        );

        let failing = FakeRunner::new(|_, _| Ok(failed("no method available")));
        let err = open_url_with(url, &failing, &[present_str], false).expect_err("exit 1");
        assert!(
            matches!(&err, InstallError::Command(m) if m.contains("no method available")),
            "{err:?}"
        );
    }

    // ----- dry run -----

    #[test]
    fn the_dry_run_with_a_target_skips_the_process_check_and_says_so() {
        let dir = TestDir::new("linux-dry-run");
        let target = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let package_dir = TestDir::new("linux-dry-run-pkg");
        let package = file_with(
            package_dir.path(),
            "SignalLock-1.1.0-x86_64.AppImage",
            b"new",
        );
        let report = dry_run(&package, &version(), Some(&target));
        assert_eq!(report.manual_reason, None);
        let text = report.steps.join("\n");
        assert!(text.contains("(mode 0755)"));
        assert!(text.contains("(atomic)"));
        assert!(text.contains("--target given"));
        assert_eq!(names(dir.path()), vec!["SignalLock.AppImage"]);
        assert_eq!(fs::read(&target).unwrap(), b"old");

        let foreign = file_with(dir.path(), "Obsidian.AppImage", b"obsidian");
        let report = dry_run(&package, &version(), Some(&foreign));
        assert!(report.steps.is_empty());
        assert!(
            report
                .manual_reason
                .unwrap()
                .contains("not a SignalLock AppImage")
        );
    }

    #[test]
    fn the_dry_run_mirrors_the_process_check() {
        let dir = TestDir::new("linux-dry-run-env");
        let appimage = file_with(dir.path(), "SignalLock.AppImage", b"old");
        let (_mount, env) = running_from(&appimage);
        let package = Path::new("/tmp/p");

        let report = dry_run_at(&env, true, package, &version(), 1);
        assert_eq!(report.manual_reason, None);
        assert!(
            report
                .steps
                .join("\n")
                .contains("current_exe is under $APPDIR")
        );

        // Inherited variables: the process lives outside the mount.
        let mut inherited = env.clone();
        inherited.exe = std::env::current_exe().ok();
        let report = dry_run_at(&inherited, true, package, &version(), 1);
        assert!(report.steps.is_empty());
        assert!(report.manual_reason.unwrap().contains("inherited"));
    }

    #[test]
    fn the_dry_run_without_appimage_reports_the_manual_fallback() {
        let report = dry_run_at(
            &AppImageEnv::default(),
            true,
            Path::new("/tmp/p"),
            &version(),
            1,
        );
        assert!(report.steps.is_empty());
        assert!(
            report
                .manual_reason
                .unwrap()
                .contains("APPIMAGE is not set")
        );
    }
}
