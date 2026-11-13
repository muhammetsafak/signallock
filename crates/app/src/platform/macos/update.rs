//! macOS in-app update (ADR-015): unpack the signed `.app.zip` next to the
//! running bundle, check its Apple code signature, swap it in by rename and
//! relaunch it.
//!
//! This file decides and orchestrates but never spawns: `ditto`, `codesign`,
//! `plutil` and `open` are run by the [`CommandRunner`] it is given
//! (`update_service.rs` owns the process spawning).
//!
//! Order, and why: the package is unpacked into a staging directory in the
//! *same directory* as the running bundle (same volume, so the swap is a pair of
//! renames); nothing in the installed bundle changes until the new one has been
//! unpacked and has passed every check; the old bundle is renamed, not deleted,
//! until the new one is in place, and moved back if anything fails.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::logging;
use crate::update::VerifiedPackage;
use crate::update_service::{
    CommandOutput, CommandRunner, DryRunReport, InstallError, Installed, Result, describe_command,
    dir_is_writable, io_error,
};

/// Apple Developer Team ID the release workflow signs with (ADR-015).
pub const TEAM_ID: &str = "U7275J9F9Q";
/// Bundle identifier of the app (unchanged since 0.1.0).
pub const BUNDLE_ID: &str = "com.signallock.app";
/// The one item the `.app.zip` must contain.
pub const BUNDLE_NAME: &str = "SignalLock.app";

const DITTO: &str = "/usr/bin/ditto";
const CODESIGN: &str = "/usr/bin/codesign";
const PLUTIL: &str = "/usr/bin/plutil";
const OPEN: &str = "/usr/bin/open";

/// What to start after a successful swap: the bundle at its usual path, which
/// now holds the new version.
#[derive(Debug)]
pub struct Relaunch {
    bundle: PathBuf,
}

impl Relaunch {
    /// Whether files under the updates directory must be kept until
    /// [`relaunch`] has run. Not on macOS: the zip has been unpacked.
    pub fn holds_package_files(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------

/// `…/SignalLock.app/Contents/MacOS/SignalLock` → `…/SignalLock.app`.
///
/// `None` unless the executable sits exactly at `<name>.app/Contents/MacOS/<file>`,
/// which is how a plain `cargo run` binary is told apart from an installed app.
pub fn bundle_path_from_exe(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let bundle = contents.parent()?;
    let is_app = bundle
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.len() > ".app".len() && name.ends_with(".app"));
    (macos_dir.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
        .then(|| bundle.to_owned())
}

/// Whether Gatekeeper's App Translocation runs the app from a randomized,
/// read-only copy (`…/AppTranslocation/<UUID>/d/SignalLock.app`). Replacing that
/// path would change nothing the user keeps.
///
/// Path check only. `SecTranslateIsTranslatedURL` would need Security.framework
/// and CoreFoundation bindings that the crate does not have, and a translocated
/// bundle also fails the writability check below.
pub fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
}

/// `Identifier=` and `TeamIdentifier=` of `codesign -dv` output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SigningInfo {
    pub identifier: Option<String>,
    pub team_id: Option<String>,
}

/// Reads `Identifier=` and `TeamIdentifier=` from `codesign -dv` output (it
/// writes to stderr). A key that appears twice with different values makes the
/// output ambiguous, so the key is reported as absent.
pub fn parse_codesign_display(output: &str) -> SigningInfo {
    fn take(slot: &mut Option<Option<String>>, value: &str) {
        match slot {
            None => *slot = Some(Some(value.to_owned())),
            Some(Some(existing)) if existing != value => *slot = Some(None),
            Some(_) => {}
        }
    }
    let mut identifier = None;
    let mut team_id = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("Identifier=") {
            take(&mut identifier, value);
        } else if let Some(value) = line.strip_prefix("TeamIdentifier=") {
            take(&mut team_id, value);
        }
    }
    SigningInfo {
        identifier: identifier.flatten(),
        team_id: team_id.flatten(),
    }
}

/// Fails unless the signature names this app and this Team ID.
pub fn check_signing(info: &SigningInfo) -> Result<()> {
    let reject = |what: &str| Err(InstallError::Verification(what.to_owned()));
    match info.identifier.as_deref() {
        Some(BUNDLE_ID) => {}
        Some(_) => return reject("the new bundle has a different identifier"),
        None => return reject("the new bundle has no readable identifier"),
    }
    match info.team_id.as_deref() {
        Some(TEAM_ID) => Ok(()),
        Some(_) => reject("the new bundle is signed by a different team"),
        None => reject("the new bundle has no readable Team ID"),
    }
}

/// The requirement `codesign --verify -R` tests on top of the bundle's own
/// signature: Apple-anchored Developer ID chain, this Team ID, this identifier.
/// Without the anchor, a self-signed certificate that merely carries the same
/// organizational unit would look like our Team ID.
fn requirement() -> String {
    format!(
        "anchor apple generic and certificate leaf[subject.OU] = \"{TEAM_ID}\" and identifier \"{BUNDLE_ID}\""
    )
}

fn ditto_args(zip: &Path, staging: &Path) -> Vec<OsString> {
    vec!["-x".into(), "-k".into(), zip.into(), staging.into()]
}

fn codesign_verify_args(app: &Path) -> Vec<OsString> {
    vec![
        "--verify".into(),
        "--deep".into(),
        "--strict".into(),
        "--verbose=2".into(),
        format!("-R={}", requirement()).into(),
        app.into(),
    ]
}

fn codesign_display_args(app: &Path) -> Vec<OsString> {
    vec!["-dv".into(), app.into()]
}

fn plutil_args(plist: &Path) -> Vec<OsString> {
    vec![
        "-extract".into(),
        "CFBundleShortVersionString".into(),
        "raw".into(),
        "-o".into(),
        "-".into(),
        plist.into(),
    ]
}

fn open_args(bundle: &Path) -> Vec<OsString> {
    vec!["-n".into(), bundle.into()]
}

fn staging_path(bundle_parent: &Path, pid: u32) -> PathBuf {
    bundle_parent.join(format!(".SignalLock-update-{pid}"))
}

fn old_bundle_path(bundle: &Path, pid: u32) -> PathBuf {
    let mut name = bundle.file_name().unwrap_or_default().to_owned();
    name.push(format!(".old-{pid}"));
    bundle.with_file_name(name)
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

/// A staging directory that is removed, with whatever is in it, when dropped.
struct Staging(PathBuf);

impl Staging {
    /// Creates `path` user-only. A leftover from a crashed attempt is removed
    /// first.
    fn create(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => fs::remove_file(path)?,
            Err(_) => {}
        }
        fs::DirBuilder::new().mode(0o700).create(path)?;
        Ok(Self(path.to_owned()))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The one thing `ditto` must have produced: a real directory named
/// `SignalLock.app` and nothing else.
fn single_bundle(staging: &Path) -> Result<PathBuf> {
    let reject = |what: &str| Err(InstallError::Verification(what.to_owned()));
    let mut entries =
        fs::read_dir(staging).map_err(|e| io_error("read the unpacked update", &e))?;
    let first = match entries.next() {
        Some(entry) => entry.map_err(|e| io_error("read the unpacked update", &e))?,
        None => return reject("the update package is empty"),
    };
    if entries.next().is_some() {
        return reject("the update package holds more than the app bundle");
    }
    if first.file_name() != BUNDLE_NAME {
        return reject("the update package does not hold SignalLock.app");
    }
    let is_real_dir = first.file_type().map(|t| t.is_dir()).unwrap_or(false);
    if !is_real_dir {
        return reject("SignalLock.app in the update package is not a directory");
    }
    Ok(first.path())
}

/// Replaces `current` with `new`: `current` is moved to `old`, `new` takes its
/// place, and if that second step fails `old` is moved back.
///
/// `rename` is a parameter so that tests can make a step fail.
fn swap_bundle(
    current: &Path,
    new: &Path,
    old: &Path,
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    rename(current, old)?;
    if let Err(error) = rename(new, current) {
        return match rename(old, current) {
            Ok(()) => Err(error),
            Err(rollback) => Err(io::Error::other(format!(
                "{error}; moving the old version back also failed ({rollback}); it is at {}",
                old.display()
            ))),
        };
    }
    Ok(())
}

/// Removes `<bundle name>.old-<digits>` siblings left behind by an update that
/// was interrupted.
fn remove_stale_old_bundles(bundle: &Path) {
    let (Some(parent), Some(name)) = (bundle.parent(), bundle.file_name()) else {
        return;
    };
    let prefix = format!("{}.old-", name.to_string_lossy());
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(suffix) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let is_real_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let removed = if is_real_dir {
            fs::remove_dir_all(entry.path())
        } else {
            fs::remove_file(entry.path())
        };
        if let Err(e) = removed {
            tracing::warn!(target: logging::APP, "could not remove {}: {e}", entry.path().display());
        }
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Installs `pkg` over the running bundle. See the module documentation.
pub fn install(pkg: &VerifiedPackage, runner: &dyn CommandRunner) -> Result<Installed<Relaunch>> {
    let exe = std::env::current_exe().map_err(|e| io_error("find the running executable", &e))?;
    install_at(&exe, pkg.path(), pkg.version(), std::process::id(), runner)
}

/// [`install`] for an explicit executable path, package, version and process id.
pub(crate) fn install_at(
    exe: &Path,
    package: &Path,
    version: &Version,
    pid: u32,
    runner: &dyn CommandRunner,
) -> Result<Installed<Relaunch>> {
    let manual = |reason: &str| Ok(Installed::Manual(reason.to_owned()));

    let Some(bundle) = bundle_path_from_exe(exe) else {
        return manual("not running from an .app bundle");
    };
    if is_translocated(&bundle) {
        return manual("running under App Translocation; move the app to Applications first");
    }
    let Some(parent) = bundle.parent() else {
        return manual("the bundle has no parent directory");
    };
    if !dir_is_writable(parent) {
        return manual("the folder that holds the app is not writable");
    }

    let staging = match Staging::create(&staging_path(parent, pid)) {
        Ok(staging) => staging,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return manual("the folder that holds the app is not writable");
        }
        Err(e) => return Err(io_error("create the staging folder", &e)),
    };

    run_checked(
        runner,
        DITTO,
        &ditto_args(package, &staging.0),
        "unpack the update",
    )?;
    let new_bundle = single_bundle(&staging.0)?;
    verify_new_bundle(runner, &new_bundle, version)?;

    let old = old_bundle_path(&bundle, pid);
    if fs::symlink_metadata(&old).is_ok() {
        fs::remove_dir_all(&old).map_err(|e| io_error("remove a leftover old version", &e))?;
    }
    swap_bundle(&bundle, &new_bundle, &old, &|from, to| fs::rename(from, to))
        .map_err(|e| io_error("replace the app", &e))?;

    // The new version is in place; what remains is housekeeping.
    if let Err(e) = fs::remove_dir_all(&old) {
        tracing::warn!(target: logging::APP, "could not remove the old version {}: {e}", old.display());
    }
    remove_stale_old_bundles(&bundle);
    drop(staging);
    Ok(Installed::Ready(Relaunch { bundle }))
}

fn run_checked(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[OsString],
    what: &str,
) -> Result<CommandOutput> {
    let output = runner
        .run(Path::new(program), args)
        .map_err(|e| InstallError::Command(format!("could not {what} ({program}): {e}")))?;
    if output.success {
        Ok(output)
    } else {
        Err(InstallError::Command(format!(
            "could not {what}: {program} {}: {}",
            output.status,
            output.stderr.trim()
        )))
    }
}

/// Signature, Team ID, identifier and version of the unpacked bundle. Any
/// failure is a [`InstallError::Verification`]: the caller deletes the staging
/// folder and nothing was replaced.
fn verify_new_bundle(runner: &dyn CommandRunner, app: &Path, expected: &Version) -> Result<()> {
    let verify = runner
        .run(Path::new(CODESIGN), &codesign_verify_args(app))
        .map_err(|e| InstallError::Command(format!("could not run codesign: {e}")))?;
    if !verify.success {
        return Err(InstallError::Verification(format!(
            "the code signature of the new bundle is not valid: {}",
            first_line(&verify.stderr)
        )));
    }

    let display = run_checked(
        runner,
        CODESIGN,
        &codesign_display_args(app),
        "read the signature of the new bundle",
    )?;
    check_signing(&parse_codesign_display(&format!(
        "{}\n{}",
        display.stdout, display.stderr
    )))?;

    let plist = app.join("Contents").join("Info.plist");
    let version = run_checked(
        runner,
        PLUTIL,
        &plutil_args(&plist),
        "read the version of the new bundle",
    )?;
    let found = Version::parse(version.stdout.trim()).map_err(|_| {
        InstallError::Verification("the new bundle has no readable version".to_owned())
    })?;
    if found.cmp_precedence(expected) != std::cmp::Ordering::Equal {
        return Err(InstallError::Verification(format!(
            "the new bundle is version {found}, not {expected}"
        )));
    }
    Ok(())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("no details").trim()
}

/// Starts the updated app with `open -n` (a second instance of the same bundle
/// is allowed; the caller has already released the single-instance guard).
pub fn relaunch(relaunch: Relaunch, runner: &dyn CommandRunner) -> Result<()> {
    run_checked(
        runner,
        OPEN,
        &open_args(&relaunch.bundle),
        "start the updated app",
    )
    .map(|_| ())
}

/// Opens `url` in the default browser with `/usr/bin/open`.
pub fn open_url(url: &str, runner: &dyn CommandRunner) -> Result<()> {
    run_checked(
        runner,
        OPEN,
        &[OsString::from(url)],
        "open the release page",
    )
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

/// The steps [`install_at`] would take. `target` is the `.app` bundle to
/// pretend is running; `None` inspects the running executable.
pub fn dry_run(package: &Path, version: &Version, target: Option<&Path>) -> DryRunReport {
    let exe = match target {
        Some(bundle) => bundle.join("Contents").join("MacOS").join("SignalLock"),
        None => match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                return DryRunReport {
                    steps: Vec::new(),
                    manual_reason: Some(format!("cannot find the running executable: {e}")),
                };
            }
        },
    };
    dry_run_at(&exe, package, version, std::process::id())
}

fn dry_run_at(exe: &Path, package: &Path, version: &Version, pid: u32) -> DryRunReport {
    let manual = |reason: String| DryRunReport {
        steps: Vec::new(),
        manual_reason: Some(reason),
    };
    let Some(bundle) = bundle_path_from_exe(exe) else {
        return manual(format!(
            "not running from an .app bundle ({}); would offer the release page",
            exe.display()
        ));
    };
    if is_translocated(&bundle) {
        return manual(format!(
            "{} runs under App Translocation; would offer the release page",
            bundle.display()
        ));
    }
    let parent = bundle.parent().unwrap_or(&bundle);
    // Permission bits only: a dry run writes nothing, so it cannot probe.
    let looks_read_only = fs::metadata(parent).is_ok_and(|m| m.permissions().readonly());
    if looks_read_only {
        return manual(format!(
            "{} is not writable; would offer the release page",
            parent.display()
        ));
    }

    let staging = staging_path(parent, pid);
    let new_bundle = staging.join(BUNDLE_NAME);
    let old = old_bundle_path(&bundle, pid);
    let line = |program: &str, args: Vec<OsString>| describe_command(program, &args);
    let steps = vec![
        format!("running bundle: {}", bundle.display()),
        format!(
            "check that {} can be written (a probe file is created and removed)",
            parent.display()
        ),
        format!("create {} (mode 0700)", staging.display()),
        line(DITTO, ditto_args(package, &staging)),
        format!("require exactly one item in the staging folder: a directory named {BUNDLE_NAME}"),
        line(CODESIGN, codesign_verify_args(&new_bundle)),
        line(CODESIGN, codesign_display_args(&new_bundle)),
        format!("require Identifier={BUNDLE_ID} and TeamIdentifier={TEAM_ID}"),
        line(
            PLUTIL,
            plutil_args(&new_bundle.join("Contents").join("Info.plist")),
        ),
        format!("require CFBundleShortVersionString == {version}"),
        format!("rename {} -> {}", bundle.display(), old.display()),
        format!("rename {} -> {}", new_bundle.display(), bundle.display()),
        format!(
            "on a failed rename: move {} back to {}",
            old.display(),
            bundle.display()
        ),
        format!("delete {} and {}", old.display(), staging.display()),
        format!("delete leftover {}.old-<pid> folders", bundle.display()),
        "empty the updates cache folder".to_owned(),
        "(after the app releases its single-instance guard)".to_owned(),
        line(OPEN, open_args(&bundle)),
    ];
    DryRunReport {
        steps,
        manual_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;
    use crate::update_service::test_support::{FakeRunner, failed, ok};

    const SAMPLE_DISPLAY: &str = "\
Executable=/Applications/SignalLock.app/Contents/MacOS/signallock
Identifier=com.signallock.app
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=30+7 location=embedded
Signature size=9012
Authority=Developer ID Application: Muhammet Safak (U7275J9F9Q)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=12 Nov 2026 at 10:00:00
Info.plist entries=22
TeamIdentifier=U7275J9F9Q
Runtime Version=14.0.0
Sealed Resources version=2 rules=13 files=4
Internal requirements count=1 size=180
";

    fn version() -> Version {
        Version::parse("1.1.0").unwrap()
    }

    // ----- bundle path -----

    #[test]
    fn the_bundle_is_found_from_the_executable_path() {
        assert_eq!(
            bundle_path_from_exe(Path::new(
                "/Applications/SignalLock.app/Contents/MacOS/signallock"
            )),
            Some(PathBuf::from("/Applications/SignalLock.app"))
        );
        assert_eq!(
            bundle_path_from_exe(Path::new(
                "/Users/me/Apps/My Signal.app/Contents/MacOS/SignalLock"
            )),
            Some(PathBuf::from("/Users/me/Apps/My Signal.app"))
        );
    }

    #[test]
    fn a_plain_binary_is_not_a_bundle() {
        for exe in [
            "/Users/me/app/target/debug/signallock",
            "/usr/local/bin/signallock",
            "/Applications/SignalLock.app/Contents/Resources/signallock",
            "/Applications/SignalLock.app/Contents/MacOS",
            "/Applications/NotAnApp/Contents/MacOS/signallock",
            "/Applications/.app/Contents/MacOS/signallock",
            "signallock",
            "/signallock",
        ] {
            assert_eq!(bundle_path_from_exe(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn app_translocation_paths_are_detected() {
        assert!(is_translocated(Path::new(
            "/private/var/folders/ab/xyz/T/AppTranslocation/6F0C-4E/d/SignalLock.app"
        )));
        assert!(!is_translocated(Path::new("/Applications/SignalLock.app")));
        assert!(!is_translocated(Path::new(
            "/Users/me/AppTranslocationNotes/SignalLock.app"
        )));
    }

    // ----- codesign output -----

    #[test]
    fn identifier_and_team_id_are_read_from_codesign_output() {
        let info = parse_codesign_display(SAMPLE_DISPLAY);
        assert_eq!(info.identifier.as_deref(), Some("com.signallock.app"));
        assert_eq!(info.team_id.as_deref(), Some("U7275J9F9Q"));
        assert_eq!(check_signing(&info), Ok(()));
    }

    #[test]
    fn crlf_output_is_read_the_same() {
        let info = parse_codesign_display(&SAMPLE_DISPLAY.replace('\n', "\r\n"));
        assert_eq!(check_signing(&info), Ok(()));
    }

    #[test]
    fn a_missing_or_foreign_team_id_is_rejected() {
        let ad_hoc = SAMPLE_DISPLAY.replace("TeamIdentifier=U7275J9F9Q", "TeamIdentifier=not set");
        assert!(matches!(
            check_signing(&parse_codesign_display(&ad_hoc)),
            Err(InstallError::Verification(_))
        ));
        let other = SAMPLE_DISPLAY.replace("U7275J9F9Q\nRuntime", "AAAAAAAAAA\nRuntime");
        assert!(matches!(
            check_signing(&parse_codesign_display(&other)),
            Err(InstallError::Verification(_))
        ));
        let none = SAMPLE_DISPLAY.replace("TeamIdentifier=U7275J9F9Q\n", "");
        assert!(matches!(
            check_signing(&parse_codesign_display(&none)),
            Err(InstallError::Verification(_))
        ));
    }

    #[test]
    fn a_foreign_identifier_is_rejected() {
        let other = SAMPLE_DISPLAY.replace(
            "Identifier=com.signallock.app",
            "Identifier=com.signallock.app.evil",
        );
        assert!(matches!(
            check_signing(&parse_codesign_display(&other)),
            Err(InstallError::Verification(_))
        ));
    }

    #[test]
    fn the_team_identifier_line_is_not_taken_for_the_identifier() {
        let info = parse_codesign_display("TeamIdentifier=U7275J9F9Q\n");
        assert_eq!(info.identifier, None);
        assert_eq!(info.team_id.as_deref(), Some("U7275J9F9Q"));
        assert!(check_signing(&info).is_err());
    }

    #[test]
    fn conflicting_duplicate_keys_are_treated_as_absent() {
        let info = parse_codesign_display(
            "Identifier=com.signallock.app\nIdentifier=com.other\nTeamIdentifier=U7275J9F9Q\n",
        );
        assert_eq!(info.identifier, None);
        assert!(check_signing(&info).is_err());
        // The same value twice is not a conflict.
        let same = parse_codesign_display(
            "Identifier=com.signallock.app\nIdentifier=com.signallock.app\nTeamIdentifier=U7275J9F9Q\n",
        );
        assert_eq!(check_signing(&same), Ok(()));
    }

    #[test]
    fn the_verify_command_pins_team_id_identifier_and_the_apple_anchor() {
        let args = codesign_verify_args(Path::new("/tmp/x/SignalLock.app"));
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &text[..4],
            ["--verify", "--deep", "--strict", "--verbose=2"]
        );
        assert!(text[4].starts_with("-R=anchor apple generic and "));
        assert!(text[4].contains("certificate leaf[subject.OU] = \"U7275J9F9Q\""));
        assert!(text[4].contains("identifier \"com.signallock.app\""));
        assert_eq!(text[5], "/tmp/x/SignalLock.app");
    }

    // ----- swap -----

    fn fake_bundle(dir: &Path, name: &str, marker: &str) -> PathBuf {
        let bundle = dir.join(name);
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::write(bundle.join("Contents/MacOS/SignalLock"), marker).unwrap();
        bundle
    }

    fn marker(bundle: &Path) -> String {
        fs::read_to_string(bundle.join("Contents/MacOS/SignalLock")).unwrap()
    }

    #[test]
    fn swap_puts_the_new_bundle_in_place_and_keeps_the_old_one_aside() {
        let dir = TestDir::new("swap-ok");
        let current = fake_bundle(dir.path(), "SignalLock.app", "old");
        let new = fake_bundle(&dir.path().join("stage"), "SignalLock.app", "new");
        let old = dir.path().join("SignalLock.app.old-1");
        swap_bundle(&current, &new, &old, &|a, b| fs::rename(a, b)).unwrap();
        assert_eq!(marker(&current), "new");
        assert_eq!(marker(&old), "old");
        assert!(!new.exists());
    }

    #[test]
    fn a_failed_second_rename_puts_the_old_bundle_back() {
        let dir = TestDir::new("swap-rollback");
        let current = fake_bundle(dir.path(), "SignalLock.app", "old");
        let new = fake_bundle(&dir.path().join("stage"), "SignalLock.app", "new");
        let old = dir.path().join("SignalLock.app.old-1");
        let calls = std::cell::Cell::new(0);
        let failing = |from: &Path, to: &Path| {
            calls.set(calls.get() + 1);
            if from == new {
                return Err(io::Error::other("simulated failure"));
            }
            fs::rename(from, to)
        };
        let err = swap_bundle(&current, &new, &old, &failing).expect_err("must fail");
        assert_eq!(err.to_string(), "simulated failure");
        assert_eq!(calls.get(), 3, "old aside, new fails, old back");
        assert_eq!(marker(&current), "old", "the old version is back in place");
        assert!(!old.exists());
        assert_eq!(
            marker(&new),
            "new",
            "the new bundle is left in the staging folder"
        );
    }

    #[test]
    fn a_failed_rollback_names_where_the_old_bundle_is() {
        let dir = TestDir::new("swap-rollback-fails");
        let current = fake_bundle(dir.path(), "SignalLock.app", "old");
        let new = fake_bundle(&dir.path().join("stage"), "SignalLock.app", "new");
        let old = dir.path().join("SignalLock.app.old-1");
        let failing = |from: &Path, to: &Path| {
            if from == new || from == old {
                return Err(io::Error::other("simulated failure"));
            }
            fs::rename(from, to)
        };
        let err = swap_bundle(&current, &new, &old, &failing).expect_err("must fail");
        assert!(err.to_string().contains(&old.display().to_string()));
        assert_eq!(marker(&old), "old");
    }

    #[test]
    fn stale_old_bundles_are_removed_and_other_siblings_are_not() {
        let dir = TestDir::new("stale-old");
        let bundle = fake_bundle(dir.path(), "SignalLock.app", "current");
        fake_bundle(dir.path(), "SignalLock.app.old-123", "stale");
        fake_bundle(dir.path(), "SignalLock.app.old-9", "stale");
        fake_bundle(dir.path(), "SignalLock.app.old-", "keep");
        fake_bundle(dir.path(), "SignalLock.app.old-12x", "keep");
        fake_bundle(dir.path(), "Other.app.old-5", "keep");
        remove_stale_old_bundles(&bundle);
        assert!(!dir.path().join("SignalLock.app.old-123").exists());
        assert!(!dir.path().join("SignalLock.app.old-9").exists());
        assert!(dir.path().join("SignalLock.app.old-").exists());
        assert!(dir.path().join("SignalLock.app.old-12x").exists());
        assert!(dir.path().join("Other.app.old-5").exists());
        assert_eq!(marker(&bundle), "current");
    }

    #[test]
    fn the_staging_folder_must_hold_exactly_one_signal_lock_app_directory() {
        let dir = TestDir::new("single");
        assert!(single_bundle(dir.path()).is_err(), "empty");

        let one = TestDir::new("single-ok");
        fake_bundle(one.path(), "SignalLock.app", "x");
        assert_eq!(
            single_bundle(one.path()).unwrap(),
            one.path().join("SignalLock.app")
        );

        let two = TestDir::new("single-two");
        fake_bundle(two.path(), "SignalLock.app", "x");
        fs::write(two.path().join("README.txt"), "x").unwrap();
        assert!(single_bundle(two.path()).is_err(), "extra item");

        let wrong = TestDir::new("single-wrong");
        fake_bundle(wrong.path(), "Other.app", "x");
        assert!(single_bundle(wrong.path()).is_err(), "wrong name");

        let file = TestDir::new("single-file");
        fs::write(file.path().join("SignalLock.app"), "not a dir").unwrap();
        assert!(single_bundle(file.path()).is_err(), "a file");

        let link = TestDir::new("single-link");
        let target = TestDir::new("single-link-target");
        std::os::unix::fs::symlink(target.path(), link.path().join("SignalLock.app")).unwrap();
        assert!(single_bundle(link.path()).is_err(), "a symlink");
    }

    // ----- install orchestration with a scripted runner -----

    /// A running install in a temp dir: `<dir>/SignalLock.app` with `old`, a
    /// package file, and a runner whose `ditto` unpacks a bundle marked `new`
    /// and whose `codesign`/`plutil` answer as configured.
    struct Setup {
        dir: TestDir,
        exe: PathBuf,
        package: PathBuf,
    }

    fn setup(label: &str) -> Setup {
        let dir = TestDir::new(label);
        fake_bundle(dir.path(), "SignalLock.app", "old");
        let exe = dir.path().join("SignalLock.app/Contents/MacOS/SignalLock");
        let package = dir.path().join("SignalLock-1.1.0-macos.app.zip");
        fs::write(&package, b"zip").unwrap();
        Setup { dir, exe, package }
    }

    fn scripted(
        verify_ok: bool,
        display: &'static str,
        plist_version: &'static str,
        unpack_extra: bool,
    ) -> FakeRunner {
        FakeRunner::new(move |program, args| {
            let program = program.to_string_lossy();
            match &*program {
                "/usr/bin/ditto" => {
                    let staging = PathBuf::from(&args[3]);
                    let bundle = fake_bundle(&staging, "SignalLock.app", "new");
                    fs::create_dir_all(bundle.join("Contents")).unwrap();
                    fs::write(bundle.join("Contents/Info.plist"), "plist").unwrap();
                    if unpack_extra {
                        fs::write(staging.join("extra.txt"), "x").unwrap();
                    }
                    Ok(ok(String::new(), String::new()))
                }
                "/usr/bin/codesign" if args[0] == "--verify" => Ok(if verify_ok {
                    ok(String::new(), "valid on disk".to_owned())
                } else {
                    failed("a sealed resource is missing or invalid")
                }),
                "/usr/bin/codesign" => Ok(ok(String::new(), display.to_owned())),
                "/usr/bin/plutil" => Ok(ok(format!("{plist_version}\n"), String::new())),
                other => panic!("unexpected program {other}"),
            }
        })
    }

    fn siblings(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_good_package_replaces_the_bundle_and_cleans_up() {
        let s = setup("install-ok");
        let runner = scripted(true, SAMPLE_DISPLAY, "1.1.0", false);
        let outcome = install_at(&s.exe, &s.package, &version(), 4242, &runner).unwrap();
        let Installed::Ready(relaunch_info) = outcome else {
            panic!("expected Ready");
        };
        assert_eq!(relaunch_info.bundle, s.dir.path().join("SignalLock.app"));
        assert!(!relaunch_info.holds_package_files());
        assert_eq!(marker(&relaunch_info.bundle), "new");
        // No staging folder, no .old bundle, no probe file left behind.
        assert_eq!(
            siblings(s.dir.path()),
            vec!["SignalLock-1.1.0-macos.app.zip", "SignalLock.app"]
        );
        // The commands that ran, in order, all by absolute path.
        let calls = runner.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].starts_with("/usr/bin/ditto -x -k "));
        assert!(calls[1].starts_with("/usr/bin/codesign --verify --deep --strict --verbose=2 "));
        assert!(calls[2].starts_with("/usr/bin/codesign -dv "));
        assert!(
            calls[3].starts_with("/usr/bin/plutil -extract CFBundleShortVersionString raw -o - ")
        );
        assert!(
            runner.spawns().is_empty(),
            "nothing is started before relaunch"
        );
    }

    #[test]
    fn a_failed_signature_check_leaves_the_installed_bundle_untouched() {
        let s = setup("install-bad-sig");
        let runner = scripted(false, SAMPLE_DISPLAY, "1.1.0", false);
        let err = install_at(&s.exe, &s.package, &version(), 4242, &runner).expect_err("rejected");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");
        assert_eq!(
            siblings(s.dir.path()),
            vec!["SignalLock-1.1.0-macos.app.zip", "SignalLock.app"],
            "the staging folder is gone"
        );
    }

    #[test]
    fn a_foreign_team_id_is_rejected_before_anything_is_replaced() {
        let s = setup("install-foreign-team");
        let foreign: &'static str = Box::leak(
            SAMPLE_DISPLAY
                .replace("TeamIdentifier=U7275J9F9Q", "TeamIdentifier=AAAAAAAAAA")
                .into_boxed_str(),
        );
        let runner = scripted(true, foreign, "1.1.0", false);
        let err = install_at(&s.exe, &s.package, &version(), 4242, &runner).expect_err("rejected");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");
    }

    #[test]
    fn a_different_bundle_version_is_rejected() {
        let s = setup("install-wrong-version");
        let runner = scripted(true, SAMPLE_DISPLAY, "1.0.5", false);
        let err = install_at(&s.exe, &s.package, &version(), 4242, &runner).expect_err("rejected");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");

        let unreadable = scripted(true, SAMPLE_DISPLAY, "not a version", false);
        let err =
            install_at(&s.exe, &s.package, &version(), 4242, &unreadable).expect_err("rejected");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
    }

    #[test]
    fn a_package_with_an_extra_item_is_rejected_before_codesign_runs() {
        let s = setup("install-extra-item");
        let runner = scripted(true, SAMPLE_DISPLAY, "1.1.0", true);
        let err = install_at(&s.exe, &s.package, &version(), 4242, &runner).expect_err("rejected");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
        assert_eq!(runner.calls().len(), 1, "only ditto ran");
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");
    }

    #[test]
    fn a_failing_ditto_is_a_command_error_and_leaves_no_staging_folder() {
        let s = setup("install-ditto-fails");
        let runner = FakeRunner::new(|_, _| Ok(failed("ditto: cannot read the archive")));
        let err = install_at(&s.exe, &s.package, &version(), 4242, &runner).expect_err("fails");
        assert!(matches!(err, InstallError::Command(_)), "{err:?}");
        assert_eq!(
            siblings(s.dir.path()),
            vec!["SignalLock-1.1.0-macos.app.zip", "SignalLock.app"]
        );
    }

    #[test]
    fn a_leftover_old_bundle_from_an_earlier_crash_is_removed_on_success() {
        let s = setup("install-stale-old");
        fake_bundle(s.dir.path(), "SignalLock.app.old-77", "crashed");
        let runner = scripted(true, SAMPLE_DISPLAY, "1.1.0", false);
        let outcome = install_at(&s.exe, &s.package, &version(), 4242, &runner).unwrap();
        assert!(matches!(outcome, Installed::Ready(_)));
        assert!(!s.dir.path().join("SignalLock.app.old-77").exists());
    }

    #[test]
    fn running_outside_a_bundle_is_manual_and_runs_nothing() {
        let dir = TestDir::new("install-not-bundle");
        let exe = dir.path().join("target/debug/signallock");
        let package = dir.path().join("p.zip");
        fs::write(&package, b"zip").unwrap();
        let runner = FakeRunner::succeeding();
        let outcome = install_at(&exe, &package, &version(), 1, &runner).unwrap();
        assert!(matches!(outcome, Installed::Manual(_)));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn app_translocation_is_manual_and_runs_nothing() {
        let dir = TestDir::new("install-translocated");
        let exe = dir
            .path()
            .join("AppTranslocation/ABC/d/SignalLock.app/Contents/MacOS/signallock");
        let package = dir.path().join("p.zip");
        fs::write(&package, b"zip").unwrap();
        let runner = FakeRunner::succeeding();
        let outcome = install_at(&exe, &package, &version(), 1, &runner).unwrap();
        let Installed::Manual(reason) = outcome else {
            panic!("expected Manual");
        };
        assert!(reason.contains("Translocation"), "{reason}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn a_folder_that_cannot_be_written_is_manual_and_runs_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let s = setup("install-read-only");
        let sealed = TestDir::new("install-read-only-probe");
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o000)).unwrap();
        let ignores_modes = fs::read_dir(sealed.path()).is_ok();
        fs::set_permissions(sealed.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if ignores_modes {
            return; // running as root: mode bits cannot make a folder unwritable
        }
        fs::set_permissions(s.dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let runner = FakeRunner::succeeding();
        let outcome = install_at(&s.exe, &s.package, &version(), 4242, &runner);
        fs::set_permissions(s.dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let Installed::Manual(reason) = outcome.unwrap() else {
            panic!("expected Manual");
        };
        assert!(reason.contains("not writable"), "{reason}");
        assert!(runner.calls().is_empty());
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");
    }

    // ----- relaunch, open, dry run -----

    #[test]
    fn relaunch_opens_the_bundle_in_a_new_instance_by_absolute_path() {
        let runner = FakeRunner::succeeding();
        let bundle = PathBuf::from("/Applications/SignalLock.app");
        relaunch(Relaunch { bundle }, &runner).unwrap();
        assert_eq!(
            runner.calls(),
            vec!["/usr/bin/open -n /Applications/SignalLock.app"]
        );
    }

    #[test]
    fn a_failing_open_is_reported() {
        let runner = FakeRunner::new(|_, _| Ok(failed("The application cannot be opened")));
        let bundle = PathBuf::from("/Applications/SignalLock.app");
        assert!(matches!(
            relaunch(Relaunch { bundle }, &runner),
            Err(InstallError::Command(_))
        ));
    }

    #[test]
    fn the_dry_run_lists_every_step_for_a_bundle() {
        let dir = TestDir::new("dry-run");
        let bundle = fake_bundle(dir.path(), "SignalLock.app", "old");
        let package = dir.path().join("SignalLock-1.1.0-macos.app.zip");
        let report = dry_run(&package, &version(), Some(&bundle));
        assert_eq!(report.manual_reason, None);
        let text = report.steps.join("\n");
        assert!(text.contains("/usr/bin/ditto -x -k "));
        assert!(text.contains("/usr/bin/codesign --verify --deep --strict --verbose=2 "));
        assert!(text.contains("TeamIdentifier=U7275J9F9Q"));
        assert!(text.contains("/usr/bin/plutil -extract"));
        assert!(text.contains("/usr/bin/open -n "));
        assert!(!dir.path().join(".SignalLock-update-1").exists());
        assert_eq!(
            siblings(dir.path()),
            vec!["SignalLock.app"],
            "a dry run creates nothing"
        );
    }

    #[test]
    fn the_dry_run_of_a_plain_binary_reports_the_manual_fallback() {
        let report = dry_run_at(
            Path::new("/work/target/debug/signallock"),
            Path::new("/tmp/p.zip"),
            &version(),
            1,
        );
        assert!(report.steps.is_empty());
        assert!(
            report
                .manual_reason
                .unwrap()
                .contains("not running from an .app bundle")
        );
    }

    // ----- the real tools, in a temp dir -----

    /// Runs the real `ditto`, `codesign` and `plutil` on an unsigned fake bundle
    /// in a temp dir: the unpack and the layout check succeed, and the signature
    /// check must refuse it, leaving the installed bundle as it was.
    #[test]
    fn an_unsigned_bundle_is_refused_by_the_real_tools() {
        use crate::update_service::SystemRunner;
        let s = setup("install-real-tools");
        let src = TestDir::new("install-real-tools-src");
        let unsigned = fake_bundle(src.path(), "SignalLock.app", "unsigned");
        fs::create_dir_all(unsigned.join("Contents")).unwrap();
        fs::write(
            unsigned.join("Contents/Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>1.1.0</string></dict></plist>"#,
        )
        .unwrap();
        let zip_out = SystemRunner
            .run(
                Path::new(DITTO),
                &[
                    "-c".into(),
                    "-k".into(),
                    "--keepParent".into(),
                    unsigned.clone().into_os_string(),
                    s.package.clone().into_os_string(),
                ],
            )
            .expect("ditto runs");
        assert!(zip_out.success, "{}", zip_out.stderr);

        let err = install_at(&s.exe, &s.package, &version(), 4242, &SystemRunner)
            .expect_err("an unsigned bundle must be refused");
        assert!(matches!(err, InstallError::Verification(_)), "{err:?}");
        assert_eq!(marker(&s.dir.path().join("SignalLock.app")), "old");
        assert_eq!(
            siblings(s.dir.path()),
            vec!["SignalLock-1.1.0-macos.app.zip", "SignalLock.app"]
        );
    }
}
