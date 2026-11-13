//! In-app update, platform-independent half (ADR-015): ask GitHub for the
//! latest release, compare versions, pick the package for this platform,
//! download it and verify it against its `.sha256` file.
//!
//! Installing is not done here (`update_service.rs`). Nothing in this module
//! runs unless the user clicked *Check for Updates…*: callers create a
//! [`GithubUpdateChecker`] inside that command handler and nowhere else. All
//! network code in the app lives in this directory.
//!
//! **Integrity limit.** The hash is checked against a file from the same
//! release, so it catches corruption and truncated downloads only; it does not
//! prove who published the package (ADR-015, known risk).

pub mod client;
pub mod release;
pub mod verify;

use std::fmt;
use std::path::PathBuf;

use semver::Version;

pub use release::Target;
pub use verify::VerifiedPackage;

use client::{Resource, UpdateClient};

/// One downloadable file of a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    /// Exact asset name, e.g. `SignalLock-1.1.0-setup.exe`.
    pub name: String,
    /// `browser_download_url`. Checked against the host allowlist on use.
    pub url: String,
    /// Size from the API, when it reported one.
    pub size: Option<u64>,
}

/// A release newer than the running version, with the package for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Release notes as plain text. Never render as HTML or Markdown.
    pub notes: String,
    /// Release page; always starts with `release::RELEASE_PAGE_PREFIX`.
    pub html_url: String,
    /// The package for this platform.
    pub asset: ReleaseAsset,
    /// `<asset>.sha256`.
    pub checksum_asset: ReleaseAsset,
}

// One value per user click; boxing would only complicate the public surface.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate,
    Available(Release),
}

/// Bytes received so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// Connection, DNS, TLS or timeout failure, or a broken redirect.
    Network(String),
    /// GitHub's request limit. `reset_at_unix` is when it resets, if known.
    RateLimited { reset_at_unix: Option<u64> },
    /// GitHub answered with a status the updater does not handle.
    UnexpectedStatus(u16),
    /// A body that is not what GitHub's API documents (bad JSON, bad tag).
    InvalidResponse(String),
    /// No package, or no `.sha256`, for this platform in the newer release.
    NoAssetForPlatform,
    /// Checksum missing, malformed, or not matching the download.
    VerificationFailed(String),
    /// A body larger than its cap.
    TooLarge { limit: u64 },
    /// A URL or redirect target outside the allowlist.
    HostNotAllowed(String),
    /// A non-HTTPS URL. Holds `scheme://host` only.
    InsecureUrl(String),
    /// Local file system failure (update cache directory, `.part` file).
    Io(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network(detail) => write!(f, "Could not reach GitHub: {detail}"),
            Self::RateLimited {
                reset_at_unix: Some(reset),
            } => match chrono::DateTime::from_timestamp(i64::try_from(*reset).unwrap_or(0), 0) {
                Some(at) => write!(
                    f,
                    "GitHub's request limit was reached. Try again after {}.",
                    at.with_timezone(&chrono::Local).format("%H:%M")
                ),
                None => write!(f, "GitHub's request limit was reached. Try again later."),
            },
            Self::RateLimited {
                reset_at_unix: None,
            } => {
                write!(f, "GitHub's request limit was reached. Try again later.")
            }
            Self::UnexpectedStatus(status) => write!(f, "GitHub answered with HTTP {status}."),
            Self::InvalidResponse(detail) => write!(f, "Unexpected answer from GitHub: {detail}"),
            Self::NoAssetForPlatform => {
                write!(f, "This release has no package for this computer.")
            }
            Self::VerificationFailed(detail) => {
                write!(f, "The download failed verification: {detail}")
            }
            Self::TooLarge { limit } => {
                write!(f, "The download is larger than the allowed {limit} bytes.")
            }
            Self::HostNotAllowed(host) => {
                write!(f, "Download refused: host {host:?} is not allowed.")
            }
            Self::InsecureUrl(origin) => write!(f, "Download refused: {origin} is not HTTPS."),
            Self::Io(detail) => write!(f, "Local file error: {detail}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// What the app needs from the updater. Blocking: run it under `spawn_blocking`.
pub trait UpdateChecker: Send {
    /// Asks GitHub for the latest release. One request.
    fn check(&self) -> Result<CheckOutcome, UpdateError>;

    /// Downloads `release`'s package and checks it against its `.sha256`.
    /// Returns only a verified package; on any failure the downloaded file is
    /// deleted. `progress` is called after every chunk written.
    fn download(
        &self,
        release: &Release,
        progress: &mut dyn FnMut(DownloadProgress),
    ) -> Result<VerifiedPackage, UpdateError>;
}

/// [`UpdateChecker`] against GitHub Releases.
pub struct GithubUpdateChecker {
    client: UpdateClient,
    current: Version,
    target: Target,
    /// Download directory; `None` means the platform cache directory.
    updates_dir: Option<PathBuf>,
}

impl GithubUpdateChecker {
    /// The production checker: GitHub, the running version, this platform.
    pub fn new() -> Self {
        Self {
            client: UpdateClient::new(),
            // Cargo refuses to build a package whose version is not valid semver.
            current: Version::parse(env!("CARGO_PKG_VERSION"))
                .expect("CARGO_PKG_VERSION is valid semver"),
            target: Target::CURRENT,
            updates_dir: None,
        }
    }

    /// Checker against a loopback server, with a fixed platform and version
    /// and a private download directory. Test builds only.
    #[cfg(test)]
    pub(crate) fn for_test(
        client: UpdateClient,
        current: Version,
        target: Target,
        updates_dir: PathBuf,
    ) -> Self {
        Self {
            client,
            current,
            target,
            updates_dir: Some(updates_dir),
        }
    }
}

impl Default for GithubUpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateChecker for GithubUpdateChecker {
    fn check(&self) -> Result<CheckOutcome, UpdateError> {
        let json = self
            .client
            .fetch_bytes(&self.client.latest_release_url(), Resource::ApiJson)?;
        release::evaluate(&json, &self.current, self.target)
    }

    fn download(
        &self,
        release: &Release,
        progress: &mut dyn FnMut(DownloadProgress),
    ) -> Result<VerifiedPackage, UpdateError> {
        if release
            .asset
            .size
            .is_some_and(|size| size > Resource::Package.max_bytes())
        {
            return Err(UpdateError::TooLarge {
                limit: Resource::Package.max_bytes(),
            });
        }
        let dir = match &self.updates_dir {
            Some(dir) => verify::prepare_dir(dir)?,
            None => verify::updates_dir()?,
        };

        // The small file first: a bad or missing checksum stops the download
        // before a single package byte is fetched.
        let checksum_bytes = self
            .client
            .fetch_bytes(&release.checksum_asset.url, Resource::Checksum)?;
        let checksum_text = String::from_utf8(checksum_bytes).map_err(|_| {
            UpdateError::VerificationFailed("checksum file is not UTF-8 text".to_owned())
        })?;
        let expected = verify::parse_checksum_file(&checksum_text, &release.asset.name)?;

        let mut body = self.client.open(&release.asset.url, Resource::Package)?;
        let total = body.content_length().or(release.asset.size);
        verify::verify_stream(
            &mut body,
            &verify::StreamSpec {
                dir: &dir,
                version: &release.version,
                asset_name: &release.asset.name,
                expected,
                max_bytes: Resource::Package.max_bytes(),
                total,
            },
            progress,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::client::test_server::{Reply, TestServer};
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;

    const PACKAGE: &[u8] = b"signallock-test-package-bytes";
    const NAME: &str = "SignalLock-1.1.0-x86_64.AppImage";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("signallock-update-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn release_json(server: &TestServer, package_url: Option<&str>) -> String {
        let package_url = package_url.map_or_else(|| server.url("/dl/package"), str::to_owned);
        format!(
            r#"{{"tag_name":"v1.1.0","draft":false,"prerelease":false,
            "html_url":"https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0",
            "body":"notes","assets":[
              {{"name":"{NAME}","browser_download_url":"{package_url}","size":{}}},
              {{"name":"{NAME}.sha256","browser_download_url":"{}","size":97}}]}}"#,
            PACKAGE.len(),
            server.url("/dl/checksum")
        )
    }

    fn checksum_line(name: &str, data: &[u8]) -> String {
        format!("{}  {name}\n", hex::encode(Sha256::digest(data)))
    }

    fn checker(server: &TestServer, current: &str, dir: &Scratch) -> GithubUpdateChecker {
        GithubUpdateChecker::for_test(
            UpdateClient::for_test(&server.base, &[&server.host]),
            Version::parse(current).expect("version"),
            Target::LinuxX86_64,
            dir.0.clone(),
        )
    }

    const LATEST: &str = "/repos/muhammetsafak/signallock/releases/latest";

    fn serve_release(server: &TestServer, package: &[u8], checksum: &str) {
        server.route(LATEST, Reply::ok(release_json(server, None)));
        server.route("/dl/package", Reply::ok(package.to_vec()));
        server.route("/dl/checksum", Reply::ok(checksum.as_bytes().to_vec()));
    }

    fn only_part_free(dir: &Scratch) -> Vec<String> {
        match fs::read_dir(&dir.0) {
            Ok(entries) => entries
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn check_reports_a_newer_release_and_up_to_date() {
        let server = TestServer::start();
        let dir = Scratch::new("check");
        serve_release(&server, PACKAGE, &checksum_line(NAME, PACKAGE));

        let CheckOutcome::Available(release) =
            checker(&server, "1.0.0", &dir).check().expect("check")
        else {
            panic!("expected Available");
        };
        assert_eq!(release.version, Version::new(1, 1, 0));
        assert_eq!(release.asset.name, NAME);
        assert_eq!(release.notes, "notes");

        assert_eq!(
            checker(&server, "1.1.0", &dir).check().expect("check"),
            CheckOutcome::UpToDate
        );
        // One request per check, to the API path only.
        let paths: Vec<_> = server.seen().into_iter().map(|s| s.path).collect();
        assert_eq!(paths, [LATEST, LATEST]);
    }

    #[test]
    fn check_surfaces_rate_limiting() {
        let server = TestServer::start();
        let dir = Scratch::new("ratelimit");
        server.route(
            LATEST,
            Reply::status(403)
                .header("x-ratelimit-remaining", "0")
                .header("x-ratelimit-reset", "1893456000"),
        );
        let error = checker(&server, "1.0.0", &dir)
            .check()
            .expect_err("limited");
        assert_eq!(
            error,
            UpdateError::RateLimited {
                reset_at_unix: Some(1_893_456_000)
            }
        );
        assert!(error.to_string().contains("request limit"));
    }

    #[test]
    fn download_returns_a_verified_package() {
        let server = TestServer::start();
        let dir = Scratch::new("download-ok");
        serve_release(&server, PACKAGE, &checksum_line(NAME, PACKAGE));
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };

        let mut last = None;
        let package = checker
            .download(&release, &mut |p| last = Some(p))
            .expect("verified download");
        assert_eq!(package.asset_name(), NAME);
        assert_eq!(package.version(), &Version::new(1, 1, 0));
        assert_eq!(fs::read(package.path()).expect("read"), PACKAGE);
        assert_eq!(only_part_free(&dir), [NAME]);
        assert_eq!(
            last,
            Some(DownloadProgress {
                downloaded: PACKAGE.len() as u64,
                total: Some(PACKAGE.len() as u64)
            })
        );
    }

    #[test]
    fn download_of_a_modified_package_fails_and_leaves_no_file() {
        let server = TestServer::start();
        let dir = Scratch::new("download-tampered");
        serve_release(
            &server,
            b"signallock-test-package-BYTES",
            &checksum_line(NAME, PACKAGE),
        );
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };
        let error = checker
            .download(&release, &mut |_| {})
            .expect_err("tampered");
        assert!(
            matches!(error, UpdateError::VerificationFailed(_)),
            "{error:?}"
        );
        assert!(only_part_free(&dir).is_empty());
    }

    #[test]
    fn checksum_for_another_file_name_stops_before_the_package_is_fetched() {
        let server = TestServer::start();
        let dir = Scratch::new("download-wrong-name");
        serve_release(
            &server,
            PACKAGE,
            &checksum_line("SignalLock-1.1.0-setup.exe", PACKAGE),
        );
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };
        let error = checker
            .download(&release, &mut |_| {})
            .expect_err("wrong name");
        assert!(matches!(error, UpdateError::VerificationFailed(_)));
        assert!(
            server.seen().iter().all(|s| s.path != "/dl/package"),
            "the package must not be requested"
        );
    }

    #[test]
    fn malformed_checksum_files_fail_closed() {
        for body in [
            "not a checksum\n",
            "",
            &format!("{}  {NAME}\n{}  {NAME}\n", "a".repeat(64), "b".repeat(64)),
        ] {
            let server = TestServer::start();
            let dir = Scratch::new("download-bad-checksum");
            serve_release(&server, PACKAGE, body);
            let checker = checker(&server, "1.0.0", &dir);
            let CheckOutcome::Available(release) = checker.check().expect("check") else {
                panic!("expected Available");
            };
            let error = checker
                .download(&release, &mut |_| {})
                .expect_err("bad checksum");
            assert!(
                matches!(error, UpdateError::VerificationFailed(_)),
                "{body:?}"
            );
        }
    }

    #[test]
    fn oversize_checksum_file_is_too_large() {
        let server = TestServer::start();
        let dir = Scratch::new("download-big-checksum");
        serve_release(&server, PACKAGE, &"a".repeat(4000));
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };
        assert_eq!(
            checker
                .download(&release, &mut |_| {})
                .expect_err("big checksum"),
            UpdateError::TooLarge {
                limit: client::CHECKSUM_MAX_BYTES
            }
        );
    }

    #[test]
    fn package_url_outside_the_allowlist_is_refused() {
        let server = TestServer::start();
        let dir = Scratch::new("download-foreign-host");
        server.route(
            LATEST,
            Reply::ok(release_json(&server, Some("http://evil.example/package"))),
        );
        server.route(
            "/dl/checksum",
            Reply::ok(checksum_line(NAME, PACKAGE).into_bytes()),
        );
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };
        assert_eq!(
            checker
                .download(&release, &mut |_| {})
                .expect_err("foreign host"),
            UpdateError::HostNotAllowed("evil.example".to_owned())
        );
        assert!(only_part_free(&dir).is_empty());
    }

    #[test]
    fn package_that_redirects_off_the_allowlist_is_refused() {
        let server = TestServer::start();
        let dir = Scratch::new("download-redirect-away");
        serve_release(&server, PACKAGE, &checksum_line(NAME, PACKAGE));
        server.route(
            "/dl/package",
            Reply::redirect("http://evil.example/package"),
        );
        let checker = checker(&server, "1.0.0", &dir);
        let CheckOutcome::Available(release) = checker.check().expect("check") else {
            panic!("expected Available");
        };
        assert_eq!(
            checker
                .download(&release, &mut |_| {})
                .expect_err("redirect away"),
            UpdateError::HostNotAllowed("evil.example".to_owned())
        );
    }

    #[test]
    fn api_size_over_the_package_cap_is_refused_without_any_request() {
        let server = TestServer::start();
        let dir = Scratch::new("download-huge");
        let checker = checker(&server, "1.0.0", &dir);
        let release = Release {
            version: Version::new(1, 1, 0),
            notes: String::new(),
            html_url: release::RELEASE_PAGE_PREFIX.to_owned(),
            asset: ReleaseAsset {
                name: NAME.to_owned(),
                url: server.url("/dl/package"),
                size: Some(Resource::Package.max_bytes() + 1),
            },
            checksum_asset: ReleaseAsset {
                name: format!("{NAME}.sha256"),
                url: server.url("/dl/checksum"),
                size: None,
            },
        };
        assert!(matches!(
            checker.download(&release, &mut |_| {}),
            Err(UpdateError::TooLarge { .. })
        ));
        assert!(server.seen().is_empty());
    }

    #[test]
    fn error_messages_are_user_readable() {
        for error in [
            UpdateError::Network("timed out".to_owned()),
            UpdateError::RateLimited {
                reset_at_unix: None,
            },
            UpdateError::UnexpectedStatus(500),
            UpdateError::NoAssetForPlatform,
            UpdateError::VerificationFailed("x".to_owned()),
            UpdateError::TooLarge { limit: 1 },
            UpdateError::HostNotAllowed("h".to_owned()),
            UpdateError::InsecureUrl("http://h".to_owned()),
            UpdateError::Io("disk".to_owned()),
            UpdateError::InvalidResponse("bad".to_owned()),
        ] {
            assert!(!error.to_string().is_empty());
        }
    }
}
