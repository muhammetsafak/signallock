#!/usr/bin/env bash
# Package target/SignalLock.app into dist/SignalLock-<version>.dmg and write
# dist/SignalLock-<version>.dmg.sha256.
#
# Usage:
#   ./scripts/make-dmg.sh               Build the .app (build-macos.sh), then the DMG.
#   ./scripts/make-dmg.sh --skip-build  Use the .app already in target/.
#
# Environment (release builds; see .ssot/08-signing-identity.md):
#   APPLE_DEV_ID     "Developer ID Application: Name (TEAMID)" (or the SHA-1 of
#                    that certificate). The .app is (re-)signed with it (hardened
#                    runtime + secure timestamp) and so is the DMG. The .app is
#                    always built ad-hoc first (build-macos.sh is run without
#                    this variable), and with --skip-build an already built
#                    bundle is simply re-signed: the signing step never needs
#                    cargo, so CI can run it in a job without any build tooling.
#   NOTARY_PROFILE   notarytool keychain profile (xcrun notarytool
#                    store-credentials ...). When set, the .app and then the
#                    .dmg are submitted to Apple's notary service and the
#                    tickets stapled. Requires APPLE_DEV_ID.
#   NOTARY_KEYCHAIN  optional path of the keychain that holds the profile.
#
# Without APPLE_DEV_ID / NOTARY_PROFILE the DMG is ad-hoc signed and
# un-notarized: fine for a local test, never publish it (ADR-014).

set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="SignalLock"
APP_PATH="target/${APP_NAME}.app"
VERSION="$(./scripts/version.sh)"
OUT_DIR="dist"
OUT_DMG="${OUT_DIR}/${APP_NAME}-${VERSION}.dmg"

SKIP_BUILD=false
case "${1:-}" in
    --skip-build) SKIP_BUILD=true ;;
    "") ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
esac

if [[ "${SKIP_BUILD}" == "false" ]]; then
    env -u APPLE_DEV_ID ./scripts/build-macos.sh
fi

if [[ ! -d "${APP_PATH}" ]]; then
    echo "Error: ${APP_PATH} not found. Run without --skip-build first." >&2
    exit 1
fi

BUNDLE_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "${APP_PATH}/Contents/Info.plist")"
if [[ "${BUNDLE_VERSION}" != "${VERSION}" ]]; then
    echo "Error: bundle version ${BUNDLE_VERSION} differs from workspace version ${VERSION}. Rebuild the .app." >&2
    exit 1
fi

if [[ -n "${NOTARY_PROFILE:-}" && -z "${APPLE_DEV_ID:-}" ]]; then
    echo "Error: NOTARY_PROFILE is set but APPLE_DEV_ID is not; Apple only notarizes Developer ID signed code." >&2
    exit 1
fi

NOTARY_ARGS=(--keychain-profile "${NOTARY_PROFILE:-}")
if [[ -n "${NOTARY_KEYCHAIN:-}" ]]; then
    NOTARY_ARGS+=(--keychain "${NOTARY_KEYCHAIN}")
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/signallock-dmg.XXXXXX")"
trap 'rm -rf "${WORK_DIR}"' EXIT

# notarize <file>: submit, wait, and fail unless Apple says "Accepted".
notarize() {
    local file="$1" result status id
    echo "==> Submitting $(basename "${file}") to Apple notarization (this can take a few minutes)"
    result="$(xcrun notarytool submit "${file}" "${NOTARY_ARGS[@]}" --wait --output-format json)"
    status="$(printf '%s' "${result}" | plutil -extract status raw -o - -)"
    id="$(printf '%s' "${result}" | plutil -extract id raw -o - -)"
    if [[ "${status}" != "Accepted" ]]; then
        echo "Error: notarization of $(basename "${file}") ended with status '${status}' (submission ${id})." >&2
        xcrun notarytool log "${id}" "${NOTARY_ARGS[@]}" >&2 || true
        exit 1
    fi
    echo "    accepted (submission ${id})"
}

# ---------------------------------------------------------------------------
# 1a. Developer ID signature for the .app (replaces the ad-hoc one).
# ---------------------------------------------------------------------------
if [[ -n "${APPLE_DEV_ID:-}" ]]; then
    echo "==> Signing ${APP_PATH} with Developer ID (hardened runtime, secure timestamp)"
    codesign --force --options runtime --timestamp --sign "${APPLE_DEV_ID}" "${APP_PATH}"
    codesign --verify --strict --verbose=2 "${APP_PATH}"
fi

# ---------------------------------------------------------------------------
# 1b. Notarize + staple the .app itself, so it still passes Gatekeeper offline
#    after the user drags it out of the DMG.
# ---------------------------------------------------------------------------
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
    if ! codesign -dv --verbose=4 "${APP_PATH}" 2>&1 | grep -q 'Authority=Developer ID Application'; then
        echo "Error: ${APP_PATH} is not signed with a Developer ID Application certificate. Rebuild with APPLE_DEV_ID set." >&2
        exit 1
    fi
    ditto -c -k --keepParent "${APP_PATH}" "${WORK_DIR}/${APP_NAME}.zip"
    notarize "${WORK_DIR}/${APP_NAME}.zip"
    xcrun stapler staple "${APP_PATH}"
    xcrun stapler validate "${APP_PATH}"
else
    echo "==> NOTARY_PROFILE not set; skipping notarization (Gatekeeper will block this build)."
fi

# ---------------------------------------------------------------------------
# 2. Stage the DMG layout: the app + a "drag here" Applications symlink.
# ---------------------------------------------------------------------------
STAGING_DIR="${WORK_DIR}/dmg-stage"
mkdir -p "${STAGING_DIR}"
cp -R "${APP_PATH}" "${STAGING_DIR}/"
ln -s /Applications "${STAGING_DIR}/Applications"

# ---------------------------------------------------------------------------
# 3. Create the compressed read-only DMG.
# ---------------------------------------------------------------------------
mkdir -p "${OUT_DIR}"
rm -f "${OUT_DMG}" "${OUT_DMG}.sha256"
echo "==> Creating ${OUT_DMG}"
hdiutil create \
    -volname "${APP_NAME}" \
    -srcfolder "${STAGING_DIR}" \
    -ov \
    -format UDZO \
    -fs HFS+ \
    -imagekey zlib-level=9 \
    "${OUT_DMG}" >/dev/null

# ---------------------------------------------------------------------------
# 4. Sign, notarize and staple the DMG.
# ---------------------------------------------------------------------------
if [[ -n "${APPLE_DEV_ID:-}" ]]; then
    echo "==> Signing ${OUT_DMG}"
    codesign --force --timestamp --sign "${APPLE_DEV_ID}" "${OUT_DMG}"
    codesign --verify --verbose=2 "${OUT_DMG}"
fi

if [[ -n "${NOTARY_PROFILE:-}" ]]; then
    notarize "${OUT_DMG}"
    echo "==> Stapling notarization ticket"
    xcrun stapler staple "${OUT_DMG}"
    xcrun stapler validate "${OUT_DMG}"
    echo "==> Gatekeeper assessment"
    spctl --assess --type open --context context:primary-signature --verbose=2 "${OUT_DMG}"
    spctl --assess --type execute --verbose=2 "${APP_PATH}"
fi

# ---------------------------------------------------------------------------
# 5. Checksum + report
# ---------------------------------------------------------------------------
(cd "${OUT_DIR}" && shasum -a 256 "$(basename "${OUT_DMG}")" > "$(basename "${OUT_DMG}").sha256")

echo
echo "------------------------------------------------------------"
echo "  Output : ${OUT_DMG}"
echo "  Size   : $(du -h "${OUT_DMG}" | awk '{print $1}')"
echo "  SHA256 : $(awk '{print $1}' "${OUT_DMG}.sha256")"
echo "  Version: ${VERSION}"
echo "------------------------------------------------------------"
