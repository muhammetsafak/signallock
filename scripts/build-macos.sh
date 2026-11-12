#!/usr/bin/env bash
# Build target/SignalLock.app: a universal (arm64 + x86_64) release build of
# the `signallock` binary, wrapped in an app bundle.
#
# Usage: ./scripts/build-macos.sh
#
# Signing (same path as the legacy macos/build-app.sh and
# .ssot/08-signing-identity.md):
#   APPLE_DEV_ID   "Developer ID Application: Name (TEAMID)" (or the SHA-1 of
#                  that certificate). When set, the bundle is signed with the
#                  hardened runtime and a secure timestamp. When unset, the
#                  bundle is signed ad-hoc: fine for local runs (it is what
#                  makes the Bluetooth permission prompt appear), NOT
#                  distributable.
#
# Rust targets: both `aarch64-apple-darwin` and `x86_64-apple-darwin` need a
# standard library. With rustup the script installs the missing one. On a
# toolchain without rustup (e.g. Homebrew's rust) set
# SIGNALLOCK_BUILD_STD=1 to build the missing one from rust-src with
# `-Zbuild-std` (uses RUSTC_BOOTSTRAP, so it works on a stable compiler).

set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="SignalLock"
BIN_NAME="signallock"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
APP_DIR="target/${APP_NAME}.app"
UNIVERSAL_DIR="target/universal/release"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "build-macos.sh must run on macOS." >&2
    exit 1
fi

VERSION="$(./scripts/version.sh)"

# Keep the binaries' minimum OS in step with the bundle's LSMinimumSystemVersion.
MIN_MACOS="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' packaging/macos/Info.plist)"
export MACOSX_DEPLOYMENT_TARGET="${MIN_MACOS}"

has_std() {
    local libdir
    libdir="$(rustc --print target-libdir --target "$1")"
    compgen -G "${libdir}/libstd-*" >/dev/null
}

build_target() {
    local target="$1"
    if has_std "${target}"; then
        echo "==> cargo build --release (${target})"
        cargo build --release --locked --target "${target}"
    elif command -v rustup >/dev/null 2>&1; then
        echo "==> rustup target add ${target}"
        rustup target add "${target}"
        echo "==> cargo build --release (${target})"
        cargo build --release --locked --target "${target}"
    elif [[ "${SIGNALLOCK_BUILD_STD:-0}" == "1" ]]; then
        echo "==> cargo build --release -Zbuild-std (${target}; no prebuilt std)"
        RUSTC_BOOTSTRAP=1 cargo build --release --locked \
            -Zbuild-std=std,panic_abort --target "${target}"
    else
        echo "No standard library for ${target}, and no rustup to install it." >&2
        echo "Install it with 'rustup target add ${target}', or set SIGNALLOCK_BUILD_STD=1." >&2
        exit 1
    fi
}

for target in "${TARGETS[@]}"; do
    build_target "${target}"
done

echo "==> lipo (universal binary)"
rm -rf "${UNIVERSAL_DIR}"
mkdir -p "${UNIVERSAL_DIR}"
lipo -create -output "${UNIVERSAL_DIR}/${BIN_NAME}" \
    "target/aarch64-apple-darwin/release/${BIN_NAME}" \
    "target/x86_64-apple-darwin/release/${BIN_NAME}"

echo "==> Assembling ${APP_DIR} (version ${VERSION})"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"
cp "${UNIVERSAL_DIR}/${BIN_NAME}" "${APP_DIR}/Contents/MacOS/${APP_NAME}"
cp packaging/macos/Info.plist "${APP_DIR}/Contents/Info.plist"
cp packaging/macos/SignalLock.icns "${APP_DIR}/Contents/Resources/SignalLock.icns"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString ${VERSION}" "${APP_DIR}/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion ${VERSION}" "${APP_DIR}/Contents/Info.plist"

if [[ -n "${APPLE_DEV_ID:-}" ]]; then
    echo "==> Signing with Developer ID (hardened runtime, secure timestamp)"
    codesign --force --options runtime --timestamp --sign "${APPLE_DEV_ID}" "${APP_DIR}"
else
    echo "==> APPLE_DEV_ID not set: ad-hoc signing (local use only, not distributable)"
    codesign --force --sign - "${APP_DIR}"
fi
codesign --verify --strict --verbose=2 "${APP_DIR}"

echo
echo "Built   : ${APP_DIR}"
echo "Version : ${VERSION}"
echo "Archs   : $(lipo -archs "${APP_DIR}/Contents/MacOS/${APP_NAME}")"
