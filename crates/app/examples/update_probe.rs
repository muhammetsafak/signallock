//! Manual probe for the update install (ADR-015).
//!
//! ```text
//! cargo run --example update_probe -- --dry-run <package> [--target <path>]
//! ```
//!
//! Prints the steps `update_service` would take to install the local package
//! `<package>` on this machine, and changes nothing: it creates no file, runs
//! no program and deletes nothing. The package's file name must be the release
//! asset name (`SignalLock-<version>-macos.app.zip`, `…-setup.exe`,
//! `…-x86_64.AppImage`); the version is read from it.
//!
//! `--target` stands in for the running installation: the `.app` bundle on
//! macOS, the AppImage file on Linux (there the check that the process runs from
//! that AppImage is skipped, since the probe is not inside one; the file name
//! check still applies). Without it the probe inspects itself, which is not a
//! bundle or an AppImage, so the result is the manual fallback.
//!
//! There is deliberately no way to run a real install from here: that is
//! tested with a real release on a real installation.

use std::path::PathBuf;
use std::process::ExitCode;

use semver::Version;
use signallock::update_service;

fn usage() -> ExitCode {
    eprintln!("usage: update_probe --dry-run <package> [--target <path>]");
    eprintln!("(this probe only prints the steps; it never installs anything)");
    ExitCode::from(2)
}

/// `SignalLock-1.1.0-setup.exe` → `1.1.0`. Only the three release asset
/// names are understood.
fn version_from_file_name(name: &str) -> Option<Version> {
    const SUFFIXES: [&str; 3] = ["-macos.app.zip", "-setup.exe", "-x86_64.AppImage"];
    let rest = name.strip_prefix("SignalLock-")?;
    let version = SUFFIXES
        .iter()
        .find_map(|suffix| rest.strip_suffix(suffix))?;
    Version::parse(version).ok()
}

fn main() -> ExitCode {
    let mut dry_run = false;
    let mut package: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--target" => match args.next() {
                Some(path) => target = Some(PathBuf::from(path)),
                None => return usage(),
            },
            other if other.starts_with("--") => return usage(),
            other if package.is_none() => package = Some(PathBuf::from(other)),
            _ => return usage(),
        }
    }
    let (true, Some(package)) = (dry_run, package) else {
        return usage();
    };

    if !package.is_file() {
        eprintln!("{} is not a file", package.display());
        return ExitCode::from(2);
    }
    let name = package
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(version) = version_from_file_name(&name) else {
        eprintln!(
            "cannot read a version from the file name {name:?} (expected SignalLock-<version>-…)"
        );
        return ExitCode::from(2);
    };
    let package = std::fs::canonicalize(&package).unwrap_or(package);

    let report = update_service::dry_run(&package, &version, target.as_deref());
    println!("dry run: installing {name} (version {version}); nothing is changed");
    match &report.manual_reason {
        Some(reason) => {
            println!("result: ManualRequired: {reason}");
            println!(
                "release page: {}",
                update_service::release_page_for(&version)
            );
        }
        None => {
            for (index, step) in report.steps.iter().enumerate() {
                println!("{:>2}. {step}", index + 1);
            }
            println!("result: RestartPending");
        }
    }
    ExitCode::SUCCESS
}
