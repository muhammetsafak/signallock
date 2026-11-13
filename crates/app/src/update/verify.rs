//! Download to disk and SHA-256 verification.
//!
//! **What the hash proves, and what it does not (ADR-015, known risk).** The
//! `.sha256` file is a release asset of the same GitHub Release as the package,
//! so it comes from the same source. A match therefore catches corruption and
//! truncated downloads; it does *not* prove where the package came from. Do not
//! read more into [`VerifiedPackage`] than that. On macOS the Apple code
//! signature, checked later by the installer (Faz 04), is the additional
//! barrier.
//!
//! The only way to obtain a [`VerifiedPackage`] is [`verify_stream`]: the
//! constructor is private to this module, and `verify_stream` itself, the
//! checksum parser and the [`StreamSpec`] are visible to `update/` only, so
//! nothing outside it can pair a self-computed hash with a file and get a
//! "verified" package.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

use super::client::map_read_error;
use super::{DownloadProgress, UpdateError};

const CHUNK: usize = 64 * 1024;

/// A package that was streamed to disk and matched its `.sha256` file.
///
/// Fields are private and there is no public constructor: every value of this
/// type went through [`verify_stream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPackage {
    path: PathBuf,
    version: Version,
    asset_name: String,
}

impl VerifiedPackage {
    fn new(path: PathBuf, version: Version, asset_name: String) -> Self {
        Self {
            path,
            version,
            asset_name,
        }
    }

    /// Test-only constructor for the installer's tests. Absent from release
    /// builds, so nothing outside a test can skip verification.
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, version: Version, asset_name: &str) -> Self {
        Self::new(path, version, asset_name.to_owned())
    }

    /// The verified file, inside the updates cache directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The release version the package belongs to.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// The release asset name, e.g. `SignalLock-1.1.0-setup.exe`.
    pub fn asset_name(&self) -> &str {
        &self.asset_name
    }
}

fn io_error(context: &str, error: &io::Error) -> UpdateError {
    UpdateError::Io(format!("{context}: {error}"))
}

/// `<platform cache>/SignalLock/updates/`, created user-only (`0700` on Unix).
pub fn updates_dir() -> Result<PathBuf, UpdateError> {
    let dirs = directories::BaseDirs::new()
        .ok_or_else(|| UpdateError::Io("no home directory for the update cache".to_owned()))?;
    prepare_dir(&dirs.cache_dir().join("SignalLock").join("updates"))
}

/// Creates `dir` (and parents) and restricts it to the current user.
pub(super) fn prepare_dir(dir: &Path) -> Result<PathBuf, UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| io_error("create update directory", &e))?;
        // A directory that already existed keeps its old mode otherwise.
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_error("restrict update directory", &e))?;
    }
    #[cfg(not(unix))]
    {
        // %LOCALAPPDATA% is already private to the user.
        fs::create_dir_all(dir).map_err(|e| io_error("create update directory", &e))?;
    }
    Ok(dir.to_owned())
}

/// Parses a `.sha256` file: one line, `<64 hex>␠␠<asset name>`, optionally
/// ended by a single `\n` or `\r\n` (what `shasum` writes). Hex is accepted in
/// either case. Anything else (a second line, other spacing, a different file
/// name) is a [`UpdateError::VerificationFailed`].
pub(super) fn parse_checksum_file(text: &str, asset_name: &str) -> Result<[u8; 32], UpdateError> {
    let bad = |why: &str| UpdateError::VerificationFailed(format!("checksum file: {why}"));

    let line = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if line.contains(['\n', '\r']) {
        return Err(bad("more than one line"));
    }
    let (hex_part, rest) = line.split_at_checked(64).ok_or_else(|| bad("too short"))?;
    let name = rest
        .strip_prefix("  ")
        .ok_or_else(|| bad("expected two spaces after the hash"))?;
    if !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad("hash is not 64 hex digits"));
    }
    let mut digest = [0_u8; 32];
    hex::decode_to_slice(hex_part, &mut digest).map_err(|_| bad("hash is not valid hex"))?;
    if name != asset_name {
        return Err(bad("file name does not match the package"));
    }
    Ok(digest)
}

/// Equality without an early exit on the first differing byte.
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A file name that is safe to join onto the updates directory.
fn validate_asset_name(name: &str) -> Result<(), UpdateError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err(UpdateError::VerificationFailed(
            "unsafe asset file name".to_owned(),
        ))
    }
}

/// Deletes the `.part` file unless disarmed. Covers errors and panics alike.
struct PartFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_part_file(path: &Path) -> Result<File, UpdateError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error("remove stale .part file", &e)),
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|e| io_error("create .part file", &e))
}

/// What a download is expected to turn out as.
pub(super) struct StreamSpec<'a> {
    /// Directory to write into (see [`updates_dir`]).
    pub(super) dir: &'a Path,
    pub(super) version: &'a Version,
    pub(super) asset_name: &'a str,
    /// SHA-256 from the `.sha256` file.
    pub(super) expected: [u8; 32],
    /// Hard cap on the package size.
    pub(super) max_bytes: u64,
    /// Size for progress reporting, when known.
    pub(super) total: Option<u64>,
}

/// Streams `reader` into `<dir>/<asset>.part` while hashing it, compares the
/// result with `spec.expected` and, only on a match, renames the file to its
/// final name and returns the [`VerifiedPackage`].
///
/// Any failure (read error, too many bytes, mismatch) deletes the `.part` file.
pub(super) fn verify_stream(
    reader: &mut dyn Read,
    spec: &StreamSpec<'_>,
    progress: &mut dyn FnMut(DownloadProgress),
) -> Result<VerifiedPackage, UpdateError> {
    validate_asset_name(spec.asset_name)?;
    let final_path = spec.dir.join(spec.asset_name);
    let mut part = PartFile {
        path: spec.dir.join(format!("{}.part", spec.asset_name)),
        armed: true,
    };

    let mut file = create_part_file(&part.path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; CHUNK];
    let mut downloaded: u64 = 0;
    loop {
        let read = reader.read(&mut buffer).map_err(|e| map_read_error(&e))?;
        if read == 0 {
            break;
        }
        downloaded += read as u64;
        if downloaded > spec.max_bytes {
            return Err(UpdateError::TooLarge {
                limit: spec.max_bytes,
            });
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|e| io_error("write .part file", &e))?;
        progress(DownloadProgress {
            downloaded,
            total: spec.total,
        });
    }
    file.sync_all()
        .map_err(|e| io_error("flush .part file", &e))?;
    drop(file);

    let actual: [u8; 32] = hasher.finalize().into();
    if !constant_time_eq(&actual, &spec.expected) {
        tracing::warn!(
            asset = spec.asset_name,
            expected = %hex::encode(spec.expected),
            actual = %hex::encode(actual),
            "SHA-256 mismatch; package deleted"
        );
        return Err(UpdateError::VerificationFailed(
            "SHA-256 does not match the checksum file".to_owned(),
        ));
    }

    // Windows cannot rename over an existing file.
    match fs::remove_file(&final_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error("replace previous download", &e)),
    }
    fs::rename(&part.path, &final_path).map_err(|e| io_error("finalise download", &e))?;
    part.armed = false;

    tracing::info!(
        asset = spec.asset_name,
        bytes = downloaded,
        "SHA-256 matches the checksum file. The checksum file comes from the same \
         release as the package, so this rules out corruption, not tampering (ADR-015)"
    );
    Ok(VerifiedPackage::new(
        final_path,
        spec.version.clone(),
        spec.asset_name.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const PACKAGE: &[u8] = b"signallock-test-package-bytes";
    const PACKAGE_SHA256: &str = "eb493637d7f3b93c79eb8daf230fc99ef2e7e764befd5c2fb3bc3f8d3d907be0";
    const NAME: &str = "SignalLock-1.1.0-setup.exe";
    const FIXTURE: &str =
        include_str!("../../tests/fixtures/update/SignalLock-1.1.0-setup.exe.sha256");

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "signallock-verify-{label}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }

        fn prepared(label: &str) -> Self {
            let dir = Self::new(label);
            prepare_dir(&dir.0).expect("prepare");
            dir
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn entries(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn digest(hex_str: &str) -> [u8; 32] {
        let mut out = [0_u8; 32];
        hex::decode_to_slice(hex_str, &mut out).expect("hex");
        out
    }

    fn run(
        dir: &Path,
        reader: &mut dyn Read,
        expected: [u8; 32],
        name: &str,
        max_bytes: u64,
    ) -> Result<VerifiedPackage, UpdateError> {
        let version = Version::new(1, 1, 0);
        verify_stream(
            reader,
            &StreamSpec {
                dir,
                version: &version,
                asset_name: name,
                expected,
                max_bytes,
                total: Some(PACKAGE.len() as u64),
            },
            &mut |_| {},
        )
    }

    // --- checksum file parsing --------------------------------------------

    #[test]
    fn parses_the_line_shasum_writes_including_its_trailing_lf() {
        assert_eq!(FIXTURE, format!("{PACKAGE_SHA256}  {NAME}\n"));
        assert_eq!(
            parse_checksum_file(FIXTURE, NAME).expect("fixture"),
            digest(PACKAGE_SHA256)
        );
    }

    #[test]
    fn accepts_no_trailing_newline_one_lf_or_one_crlf() {
        let line = format!("{PACKAGE_SHA256}  {NAME}");
        for text in [line.clone(), format!("{line}\n"), format!("{line}\r\n")] {
            assert_eq!(
                parse_checksum_file(&text, NAME).expect("accepted"),
                digest(PACKAGE_SHA256),
                "{text:?}"
            );
        }
    }

    #[test]
    fn rejects_a_second_line_blank_lines_and_stray_carriage_returns() {
        let line = format!("{PACKAGE_SHA256}  {NAME}");
        for text in [
            format!("{line}\n\n"),
            format!("{line}\n{line}\n"),
            format!("{line}\n{PACKAGE_SHA256}  other.exe\n"),
            format!("{line}\r\n\r\n"),
            format!("{line}\r"),
            format!("{line}\n\r\n"),
            format!("\n{line}\n"),
            format!("# comment\n{line}\n"),
        ] {
            assert!(
                matches!(
                    parse_checksum_file(&text, NAME),
                    Err(UpdateError::VerificationFailed(_))
                ),
                "{text:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_whitespace_differences() {
        for text in [
            format!("{PACKAGE_SHA256} {NAME}\n"),   // one space
            format!("{PACKAGE_SHA256}   {NAME}\n"), // three spaces
            format!("{PACKAGE_SHA256}\t{NAME}\n"),  // tab
            format!("{PACKAGE_SHA256} *{NAME}\n"),  // sha256sum binary marker
            format!(" {PACKAGE_SHA256}  {NAME}\n"), // leading space
            format!("{PACKAGE_SHA256}  {NAME} \n"), // trailing space
            format!("{PACKAGE_SHA256}  {NAME}\n "), // trailing space after LF
        ] {
            assert!(
                matches!(
                    parse_checksum_file(&text, NAME),
                    Err(UpdateError::VerificationFailed(_))
                ),
                "{text:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_malformed_hashes() {
        let good = PACKAGE_SHA256;
        for hash in [
            "",
            "abc",
            &good[..63],
            &format!("{good}0"),
            &format!("{}g", &good[..63]),
            &format!("{}é", &good[..62]), // multi-byte char straddling byte 64
        ] {
            let text = format!("{hash}  {NAME}\n");
            assert!(
                matches!(
                    parse_checksum_file(&text, NAME),
                    Err(UpdateError::VerificationFailed(_))
                ),
                "{text:?} must be rejected"
            );
        }
        assert!(parse_checksum_file("", NAME).is_err());
        assert!(parse_checksum_file("\n", NAME).is_err());
    }

    #[test]
    fn rejects_a_hash_for_a_different_file_name() {
        for other in [
            "SignalLock-1.1.0-macos.app.zip",
            "signallock-1.1.0-setup.exe",
            "",
        ] {
            let text = format!("{PACKAGE_SHA256}  {other}\n");
            assert!(
                matches!(
                    parse_checksum_file(&text, NAME),
                    Err(UpdateError::VerificationFailed(_))
                ),
                "{other:?}"
            );
        }
    }

    #[test]
    fn hex_case_does_not_matter() {
        let upper = format!("{}  {NAME}\n", PACKAGE_SHA256.to_ascii_uppercase());
        let mixed = format!(
            "{}{}  {NAME}\n",
            PACKAGE_SHA256[..32].to_ascii_uppercase(),
            &PACKAGE_SHA256[32..]
        );
        for text in [upper, mixed] {
            assert_eq!(
                parse_checksum_file(&text, NAME).expect("case-insensitive"),
                digest(PACKAGE_SHA256)
            );
        }
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        let a = digest(PACKAGE_SHA256);
        let mut b = a;
        assert!(constant_time_eq(&a, &b));
        b[31] ^= 1;
        assert!(!constant_time_eq(&a, &b));
        b = a;
        b[0] ^= 0x80;
        assert!(!constant_time_eq(&a, &b));
    }

    // --- streaming and verification ---------------------------------------

    #[test]
    fn matching_download_is_renamed_and_returned() {
        let dir = TempDir::prepared("ok");
        let package = run(
            dir.path(),
            &mut Cursor::new(PACKAGE),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect("verified");
        assert_eq!(package.asset_name(), NAME);
        assert_eq!(package.version(), &Version::new(1, 1, 0));
        assert_eq!(package.path(), dir.path().join(NAME));
        assert_eq!(fs::read(package.path()).expect("read"), PACKAGE);
        assert_eq!(dir.entries(), [NAME], "no .part file may remain");
    }

    #[test]
    fn uppercase_hex_in_the_checksum_file_verifies_the_same_download() {
        let dir = TempDir::prepared("upper");
        let text = format!("{}  {NAME}\n", PACKAGE_SHA256.to_ascii_uppercase());
        let expected = parse_checksum_file(&text, NAME).expect("parse");
        run(dir.path(), &mut Cursor::new(PACKAGE), expected, NAME, 1024).expect("verified");
    }

    #[test]
    fn modified_download_is_deleted_and_fails_verification() {
        let dir = TempDir::prepared("tampered");
        let mut tampered = PACKAGE.to_vec();
        tampered[3] ^= 0x01;
        let error = run(
            dir.path(),
            &mut Cursor::new(tampered),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect_err("hash mismatch");
        assert!(
            matches!(error, UpdateError::VerificationFailed(_)),
            "{error:?}"
        );
        assert_eq!(dir.entries(), Vec::<String>::new(), "nothing may remain");
    }

    #[test]
    fn truncated_download_fails_verification_and_leaves_nothing() {
        let dir = TempDir::prepared("truncated");
        let error = run(
            dir.path(),
            &mut Cursor::new(&PACKAGE[..10]),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect_err("truncated");
        assert!(matches!(error, UpdateError::VerificationFailed(_)));
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn hash_of_another_file_does_not_verify() {
        let dir = TempDir::prepared("wrong-file");
        let other_hash = hex::encode(Sha256::digest(b"some other release asset"));
        let error = run(
            dir.path(),
            &mut Cursor::new(PACKAGE),
            digest(&other_hash),
            NAME,
            1024,
        )
        .expect_err("hash of another file");
        assert!(matches!(error, UpdateError::VerificationFailed(_)));
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn over_size_download_is_too_large_and_the_part_file_is_deleted() {
        let dir = TempDir::prepared("too-large");
        let error = run(
            dir.path(),
            &mut Cursor::new(vec![7_u8; 5000]),
            digest(PACKAGE_SHA256),
            NAME,
            4096,
        )
        .expect_err("over the cap");
        assert_eq!(error, UpdateError::TooLarge { limit: 4096 });
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn download_exactly_at_the_cap_is_allowed() {
        let dir = TempDir::prepared("at-cap");
        let data = vec![9_u8; 4096];
        let expected: [u8; 32] = Sha256::digest(&data).into();
        run(dir.path(), &mut Cursor::new(data), expected, NAME, 4096).expect("at the cap");
    }

    struct FailingReader {
        sent: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.sent {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "connection lost",
                ));
            }
            self.sent = true;
            buf[..4].copy_from_slice(b"abcd");
            Ok(4)
        }
    }

    #[test]
    fn read_error_midway_deletes_the_part_file() {
        let dir = TempDir::prepared("read-error");
        let error = run(
            dir.path(),
            &mut FailingReader { sent: false },
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect_err("read error");
        assert!(matches!(error, UpdateError::Network(_)), "{error:?}");
        assert!(dir.entries().is_empty());
    }

    #[test]
    fn a_previous_download_is_replaced_and_a_stale_part_file_is_ignored() {
        let dir = TempDir::prepared("replace");
        fs::write(dir.path().join(NAME), b"old package").expect("seed final");
        fs::write(dir.path().join(format!("{NAME}.part")), b"stale partial").expect("seed part");
        let package = run(
            dir.path(),
            &mut Cursor::new(PACKAGE),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect("verified");
        assert_eq!(fs::read(package.path()).expect("read"), PACKAGE);
        assert_eq!(dir.entries(), [NAME]);
    }

    #[test]
    fn failed_download_keeps_an_earlier_verified_package_untouched() {
        let dir = TempDir::prepared("keep-old");
        fs::write(dir.path().join(NAME), b"old package").expect("seed final");
        let error = run(
            dir.path(),
            &mut Cursor::new(b"corrupt".to_vec()),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect_err("mismatch");
        assert!(matches!(error, UpdateError::VerificationFailed(_)));
        assert_eq!(
            fs::read(dir.path().join(NAME)).expect("read"),
            b"old package"
        );
        assert_eq!(dir.entries(), [NAME]);
    }

    #[test]
    fn progress_is_reported_with_a_growing_byte_count() {
        let dir = TempDir::prepared("progress");
        let data = vec![1_u8; CHUNK * 2 + 100];
        let expected: [u8; 32] = Sha256::digest(&data).into();
        let version = Version::new(1, 1, 0);
        let mut seen = Vec::new();
        verify_stream(
            &mut Cursor::new(data.clone()),
            &StreamSpec {
                dir: dir.path(),
                version: &version,
                asset_name: NAME,
                expected,
                max_bytes: 1 << 20,
                total: Some(data.len() as u64),
            },
            &mut |p| seen.push(p),
        )
        .expect("verified");
        assert!(seen.len() >= 3);
        assert!(seen.windows(2).all(|w| w[0].downloaded < w[1].downloaded));
        assert_eq!(seen.last().expect("last").downloaded, data.len() as u64);
        assert_eq!(seen.last().expect("last").total, Some(data.len() as u64));
    }

    #[test]
    fn unsafe_asset_names_are_refused_before_touching_the_disk() {
        let dir = TempDir::prepared("unsafe-name");
        for name in [
            "",
            "../evil",
            "a/b",
            "a\\b",
            ".hidden",
            "name with space",
            "x\0y",
        ] {
            let error = run(
                dir.path(),
                &mut Cursor::new(PACKAGE),
                digest(PACKAGE_SHA256),
                name,
                1024,
            )
            .expect_err(name);
            assert!(
                matches!(error, UpdateError::VerificationFailed(_)),
                "{name:?}"
            );
        }
        assert!(dir.entries().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn updates_directory_and_files_are_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("perms");
        // A pre-existing, too-open directory is tightened.
        fs::create_dir_all(dir.path()).expect("mkdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
        prepare_dir(dir.path()).expect("prepare");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(dir.path()), 0o700);

        let package = run(
            dir.path(),
            &mut Cursor::new(PACKAGE),
            digest(PACKAGE_SHA256),
            NAME,
            1024,
        )
        .expect("verified");
        assert_eq!(mode(package.path()), 0o600);
    }

    #[test]
    fn test_constructor_builds_a_package_for_installer_tests() {
        let package =
            VerifiedPackage::for_test(PathBuf::from("/tmp/x"), Version::new(1, 1, 0), NAME);
        assert_eq!(package.path(), Path::new("/tmp/x"));
        assert_eq!(package.asset_name(), NAME);
    }

    #[test]
    fn nested_updates_directory_is_created() {
        let root = TempDir::new("nested");
        let nested = root.path().join("SignalLock").join("updates");
        assert_eq!(prepare_dir(&nested).expect("prepare"), nested);
        assert!(nested.is_dir());
    }
}
