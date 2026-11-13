//! `releases/latest` parsing, version comparison and platform asset selection.

use std::cmp::Ordering;

use semver::Version;
use serde::Deserialize;

use super::{CheckOutcome, Release, ReleaseAsset, UpdateError};

/// Every release page the app may ever offer to open starts with this
/// (ADR-015). `Release::html_url` is guaranteed to.
pub const RELEASE_PAGE_PREFIX: &str = "https://github.com/muhammetsafak/signallock/releases/";

/// Longest release-notes text kept, in bytes. The text is shown as plain text.
const NOTES_MAX_BYTES: usize = 16 * 1024;

/// The platform a package is selected for. Separate from `cfg` so that asset
/// selection can be tested for all platforms on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    MacOs,
    Windows,
    LinuxX86_64,
    /// Linux on another architecture, or an OS without a release package.
    Unsupported,
}

impl Target {
    /// The platform this binary was compiled for.
    pub const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::MacOs
    } else if cfg!(target_os = "windows") {
        Self::Windows
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Self::LinuxX86_64
    } else {
        Self::Unsupported
    };

    /// The exact package name for `version` (ADR-014 / ADR-015).
    pub fn package_name(self, version: &Version) -> Option<String> {
        match self {
            Self::MacOs => Some(format!("SignalLock-{version}-macos.app.zip")),
            Self::Windows => Some(format!("SignalLock-{version}-setup.exe")),
            Self::LinuxX86_64 => Some(format!("SignalLock-{version}-x86_64.AppImage")),
            Self::Unsupported => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Parses `v<semver>`. Strict: the `v` is required.
fn parse_tag(tag: &str) -> Result<Version, UpdateError> {
    tag.strip_prefix('v')
        .and_then(|rest| Version::parse(rest).ok())
        .ok_or_else(|| UpdateError::InvalidResponse(format!("unrecognised release tag {tag:?}")))
}

/// Decides what a `releases/latest` answer means for the running version.
///
/// * draft or prerelease (flag, or a semver pre-release tag) -> `UpToDate`
/// * tag not greater than `current` by SemVer precedence -> `UpToDate` (never a
///   downgrade; build metadata is ignored)
/// * newer, but no package or no `.sha256` for `target` -> `NoAssetForPlatform`
pub fn evaluate(
    json: &[u8],
    current: &Version,
    target: Target,
) -> Result<CheckOutcome, UpdateError> {
    let api: ApiRelease = serde_json::from_slice(json)
        .map_err(|e| UpdateError::InvalidResponse(format!("release JSON: {e}")))?;

    if api.draft || api.prerelease {
        return Ok(CheckOutcome::UpToDate);
    }
    let version = parse_tag(&api.tag_name)?;
    if !version.pre.is_empty() {
        return Ok(CheckOutcome::UpToDate);
    }
    // SemVer 2.0: build metadata does not take part in precedence, so
    // `1.0.0+hotfix` is the same version as `1.0.0`. `Version`'s own `Ord`
    // does compare it, hence `cmp_precedence`.
    if version.cmp_precedence(current) != Ordering::Greater {
        return Ok(CheckOutcome::UpToDate);
    }
    if !version.build.is_empty() {
        // Newer, but the package names (ADR-014) have no place for `+...`.
        return Err(UpdateError::InvalidResponse(format!(
            "release tag {:?} carries build metadata, which release packages do not use",
            api.tag_name
        )));
    }

    let package_name = target
        .package_name(&version)
        .ok_or(UpdateError::NoAssetForPlatform)?;
    let checksum_name = format!("{package_name}.sha256");
    let find = |name: &str| {
        api.assets
            .iter()
            .find(|asset| asset.name == name)
            .map(|asset| ReleaseAsset {
                name: asset.name.clone(),
                url: asset.browser_download_url.clone(),
                size: (asset.size > 0).then_some(asset.size),
            })
    };
    let asset = find(&package_name).ok_or(UpdateError::NoAssetForPlatform)?;
    let checksum_asset = find(&checksum_name).ok_or(UpdateError::NoAssetForPlatform)?;

    Ok(CheckOutcome::Available(Release {
        html_url: release_page(&api.html_url, &version),
        notes: sanitize_notes(api.body.as_deref().unwrap_or("")),
        version,
        asset,
        checksum_asset,
    }))
}

/// The release page URL from the API when it is under the release page prefix,
/// otherwise one built from the tag.
fn release_page(html_url: &str, version: &Version) -> String {
    let plain = !html_url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace());
    if plain && html_url.starts_with(RELEASE_PAGE_PREFIX) {
        html_url.to_owned()
    } else {
        format!("{RELEASE_PAGE_PREFIX}tag/v{version}")
    }
}

/// Release notes as safe plain text: `\r\n` -> `\n`, control characters other
/// than newline and tab dropped, length capped at [`NOTES_MAX_BYTES`].
fn sanitize_notes(body: &str) -> String {
    let mut text: String = body
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    if text.len() > NOTES_MAX_BYTES {
        let mut end = NOTES_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = include_str!("../../tests/fixtures/update/release-1.1.0.json");
    const DRAFT: &str = include_str!("../../tests/fixtures/update/release-draft.json");
    const PRERELEASE: &str = include_str!("../../tests/fixtures/update/release-prerelease.json");
    const BAD_TAG: &str = include_str!("../../tests/fixtures/update/release-bad-tag.json");
    const NO_V_TAG: &str = include_str!("../../tests/fixtures/update/release-no-v-tag.json");
    const SEMVER_PRE_TAG: &str =
        include_str!("../../tests/fixtures/update/release-semver-prerelease-tag.json");
    const MISSING_CHECKSUMS: &str =
        include_str!("../../tests/fixtures/update/release-missing-checksum.json");
    const FOREIGN_HTML_URL: &str =
        include_str!("../../tests/fixtures/update/release-foreign-html-url.json");

    fn v(s: &str) -> Version {
        Version::parse(s).expect("test version")
    }

    fn available(json: &str, current: &str, target: Target) -> Release {
        match evaluate(json.as_bytes(), &v(current), target).expect("evaluates") {
            CheckOutcome::Available(release) => release,
            CheckOutcome::UpToDate => panic!("expected Available"),
        }
    }

    // --- version comparison ----------------------------------------------

    #[test]
    fn newer_tag_is_available() {
        let release = available(FULL, "1.0.0", Target::MacOs);
        assert_eq!(release.version, v("1.1.0"));
    }

    #[test]
    fn equal_version_is_up_to_date() {
        assert_eq!(
            evaluate(FULL.as_bytes(), &v("1.1.0"), Target::MacOs),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn older_tag_is_up_to_date_never_a_downgrade() {
        assert_eq!(
            evaluate(FULL.as_bytes(), &v("1.2.0"), Target::MacOs),
            Ok(CheckOutcome::UpToDate)
        );
        assert_eq!(
            evaluate(FULL.as_bytes(), &v("2.0.0"), Target::Windows),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn comparison_is_semver_not_lexical() {
        // 1.10.0 > 1.9.0 although "1.10.0" < "1.9.0" as strings.
        let json = FULL.replace("v1.1.0", "v1.10.0").replace("1.1.0", "1.10.0");
        assert!(matches!(
            evaluate(json.as_bytes(), &v("1.9.0"), Target::MacOs),
            Ok(CheckOutcome::Available(_))
        ));
    }

    #[test]
    fn build_metadata_does_not_make_a_tag_newer() {
        // `1.1.0+hotfix` has the same precedence as `1.1.0`.
        let json = FULL.replace("v1.1.0", "v1.1.0+hotfix");
        for current in ["1.1.0", "1.1.0+other", "1.2.0"] {
            assert_eq!(
                evaluate(json.as_bytes(), &v(current), Target::MacOs),
                Ok(CheckOutcome::UpToDate),
                "{current}"
            );
        }
        // A running version with build metadata is equally ignored.
        assert_eq!(
            evaluate(FULL.as_bytes(), &v("1.1.0+local"), Target::MacOs),
            Ok(CheckOutcome::UpToDate)
        );
        assert!(matches!(
            evaluate(FULL.as_bytes(), &v("1.0.0+local"), Target::MacOs),
            Ok(CheckOutcome::Available(_))
        ));
    }

    #[test]
    fn newer_tag_with_build_metadata_is_refused_not_offered() {
        let json = FULL.replace("v1.1.0", "v1.2.0+hotfix");
        let error = evaluate(json.as_bytes(), &v("1.1.0"), Target::MacOs).expect_err("refused");
        assert!(
            matches!(error, UpdateError::InvalidResponse(_)),
            "{error:?}"
        );
    }

    #[test]
    fn draft_and_prerelease_flags_are_up_to_date() {
        for json in [DRAFT, PRERELEASE] {
            assert_eq!(
                evaluate(json.as_bytes(), &v("1.0.0"), Target::MacOs),
                Ok(CheckOutcome::UpToDate)
            );
        }
    }

    #[test]
    fn semver_prerelease_tag_is_up_to_date_even_without_the_flag() {
        assert_eq!(
            evaluate(SEMVER_PRE_TAG.as_bytes(), &v("1.0.0"), Target::MacOs),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn broken_tags_are_invalid_responses() {
        for json in [BAD_TAG, NO_V_TAG] {
            let error = evaluate(json.as_bytes(), &v("1.0.0"), Target::MacOs).expect_err("bad tag");
            assert!(
                matches!(error, UpdateError::InvalidResponse(_)),
                "{error:?}"
            );
        }
    }

    #[test]
    fn malformed_json_is_an_invalid_response() {
        for body in ["", "not json", "[]", "{}", r#"{"tag_name": 5}"#] {
            let error = evaluate(body.as_bytes(), &v("1.0.0"), Target::MacOs).expect_err(body);
            assert!(
                matches!(error, UpdateError::InvalidResponse(_)),
                "{error:?}"
            );
        }
    }

    // --- asset selection -------------------------------------------------

    #[test]
    fn picks_the_exact_asset_for_each_platform() {
        let mac = available(FULL, "1.0.0", Target::MacOs);
        assert_eq!(mac.asset.name, "SignalLock-1.1.0-macos.app.zip");
        assert_eq!(
            mac.checksum_asset.name,
            "SignalLock-1.1.0-macos.app.zip.sha256"
        );

        let windows = available(FULL, "1.0.0", Target::Windows);
        assert_eq!(windows.asset.name, "SignalLock-1.1.0-setup.exe");
        assert_eq!(
            windows.checksum_asset.name,
            "SignalLock-1.1.0-setup.exe.sha256"
        );

        let linux = available(FULL, "1.0.0", Target::LinuxX86_64);
        assert_eq!(linux.asset.name, "SignalLock-1.1.0-x86_64.AppImage");
        assert_eq!(
            linux.checksum_asset.name,
            "SignalLock-1.1.0-x86_64.AppImage.sha256"
        );
    }

    #[test]
    fn asset_urls_and_sizes_come_from_the_matching_assets() {
        let mac = available(FULL, "1.0.0", Target::MacOs);
        assert_eq!(
            mac.asset.url,
            "https://github.com/muhammetsafak/signallock/releases/download/v1.1.0/SignalLock-1.1.0-macos.app.zip"
        );
        assert_eq!(mac.asset.size, Some(12_345_678));
        assert_eq!(mac.checksum_asset.size, Some(85));
    }

    #[test]
    fn dmg_and_decoy_assets_are_never_selected() {
        // The fixture also carries a .dmg, a .deb, an aarch64 AppImage and a
        // package of another version; none of them may be picked.
        for target in [Target::MacOs, Target::Windows, Target::LinuxX86_64] {
            let release = available(FULL, "1.0.0", target);
            assert_eq!(Some(release.asset.name), target.package_name(&v("1.1.0")));
        }
    }

    #[test]
    fn linux_without_an_x86_64_asset_is_no_asset_for_platform() {
        assert_eq!(
            evaluate(FULL.as_bytes(), &v("1.0.0"), Target::Unsupported),
            Err(UpdateError::NoAssetForPlatform)
        );
        // aarch64 AppImage present in the fixture must not satisfy x86_64.
        let no_x86 = FULL.replace("x86_64.AppImage", "decoy.AppImage");
        assert_eq!(
            evaluate(no_x86.as_bytes(), &v("1.0.0"), Target::LinuxX86_64),
            Err(UpdateError::NoAssetForPlatform)
        );
    }

    #[test]
    fn missing_checksum_asset_is_an_error_not_a_fallback() {
        // macOS has its .sha256 in this fixture, Windows and Linux do not.
        assert!(matches!(
            evaluate(MISSING_CHECKSUMS.as_bytes(), &v("1.0.0"), Target::MacOs),
            Ok(CheckOutcome::Available(_))
        ));
        for target in [Target::Windows, Target::LinuxX86_64] {
            assert_eq!(
                evaluate(MISSING_CHECKSUMS.as_bytes(), &v("1.0.0"), target),
                Err(UpdateError::NoAssetForPlatform),
                "{target:?}"
            );
        }
    }

    #[test]
    fn missing_package_is_no_asset_for_platform() {
        let json = FULL.replace(
            "SignalLock-1.1.0-setup.exe\"",
            "SignalLock-1.1.0-other.exe\"",
        );
        assert_eq!(
            evaluate(json.as_bytes(), &v("1.0.0"), Target::Windows),
            Err(UpdateError::NoAssetForPlatform)
        );
    }

    #[test]
    fn asset_check_only_applies_to_newer_releases() {
        // Same release without any assets is still just "up to date" when the
        // running version is not older.
        let no_assets = r#"{"tag_name":"v1.0.0","draft":false,"prerelease":false,"assets":[]}"#;
        assert_eq!(
            evaluate(no_assets.as_bytes(), &v("1.0.0"), Target::MacOs),
            Ok(CheckOutcome::UpToDate)
        );
    }

    #[test]
    fn package_names_follow_the_adr_exactly() {
        let version = v("2.3.4");
        assert_eq!(
            Target::MacOs.package_name(&version).as_deref(),
            Some("SignalLock-2.3.4-macos.app.zip")
        );
        assert_eq!(
            Target::Windows.package_name(&version).as_deref(),
            Some("SignalLock-2.3.4-setup.exe")
        );
        assert_eq!(
            Target::LinuxX86_64.package_name(&version).as_deref(),
            Some("SignalLock-2.3.4-x86_64.AppImage")
        );
        assert_eq!(Target::Unsupported.package_name(&version), None);
    }

    // --- release page and notes -------------------------------------------

    #[test]
    fn html_url_is_kept_when_under_the_release_prefix() {
        let release = available(FULL, "1.0.0", Target::MacOs);
        assert_eq!(
            release.html_url,
            "https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0"
        );
    }

    #[test]
    fn foreign_html_url_is_replaced_by_one_built_from_the_tag() {
        let release = available(FOREIGN_HTML_URL, "1.0.0", Target::MacOs);
        assert_eq!(
            release.html_url,
            "https://github.com/muhammetsafak/signallock/releases/tag/v1.1.0"
        );
        assert!(release.html_url.starts_with(RELEASE_PAGE_PREFIX));
    }

    #[test]
    fn notes_are_plain_text_with_control_characters_removed() {
        let release = available(FULL, "1.0.0", Target::MacOs);
        assert_eq!(
            release.notes,
            "Fixes the lock chain.\n\n- one\n- two\t(tab kept)"
        );
    }

    #[test]
    fn null_notes_become_empty() {
        let json = r#"{"tag_name":"v1.1.0","body":null,"assets":[
            {"name":"SignalLock-1.1.0-macos.app.zip","browser_download_url":"https://github.com/a","size":1},
            {"name":"SignalLock-1.1.0-macos.app.zip.sha256","browser_download_url":"https://github.com/b","size":1}]}"#;
        assert_eq!(available(json, "1.0.0", Target::MacOs).notes, "");
    }

    #[test]
    fn long_notes_are_truncated_on_a_char_boundary() {
        let long = "é".repeat(NOTES_MAX_BYTES); // 2 bytes each
        let cut = sanitize_notes(&long);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= NOTES_MAX_BYTES + '…'.len_utf8());
        assert_eq!(sanitize_notes("short"), "short");
    }
}
