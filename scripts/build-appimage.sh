#!/usr/bin/env bash
# Build dist/SignalLock-<version>-x86_64.AppImage and its .sha256 (Linux only).
#
# Usage: ./scripts/build-appimage.sh
#
# Tools (not installed by this script; CI downloads pinned, checksum-verified
# releases of both):
#   linuxdeploy   https://github.com/linuxdeploy/linuxdeploy  -> LINUXDEPLOY
#                 (default: `linuxdeploy` on PATH). Gathers the binary, its
#                 shared libraries, the .desktop file and the icon into an AppDir.
#   appimagetool  https://github.com/AppImage/appimagetool    -> APPIMAGETOOL
#                 (default: `appimagetool` on PATH). Squashes the AppDir.
#
# System packages needed to BUILD (Debian/Ubuntu names; Ubuntu 22.04 is the
# reference, an older glibc keeps the AppImage runnable on more distros):
#
#   build-essential pkg-config file
#   libdbus-1-dev          btleplug's BlueZ backend (libdbus-sys)
#   libfontconfig1-dev     Slint text rendering (yeslogic-fontconfig-sys)
#   libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-dev libxcb1-dev
#                          Slint/winit (they are loaded with dlopen at run
#                          time; the headers are needed to build)
#   libegl1-mesa-dev libgl1-mesa-dev   Slint's OpenGL renderer
#
# No GTK / libappindicator: the tray uses tray-icon's `ksni` backend (a pure
# Rust StatusNotifierItem over D-Bus), see crates/app/Cargo.toml.
# At run time the host provides the GL stack, Wayland/X11 and a D-Bus session.
#
#   sudo apt-get install -y build-essential pkg-config file libdbus-1-dev \
#       libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
#       libwayland-dev libx11-dev libxcb1-dev libegl1-mesa-dev libgl1-mesa-dev

set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="SignalLock"
BIN_NAME="signallock"
ARCH="x86_64"
OUT_DIR="dist"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "${ARCH}" ]]; then
    echo "build-appimage.sh builds the ${ARCH} AppImage and must run on Linux ${ARCH}." >&2
    exit 1
fi

LINUXDEPLOY="${LINUXDEPLOY:-linuxdeploy}"
APPIMAGETOOL="${APPIMAGETOOL:-appimagetool}"
for tool in "${LINUXDEPLOY}" "${APPIMAGETOOL}"; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "Required tool not found: ${tool} (see the header of this script)." >&2
        exit 1
    fi
done

# The tools are AppImages themselves; runners and containers have no FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH

VERSION="$(./scripts/version.sh)"
OUT_FILE="${OUT_DIR}/${APP_NAME}-${VERSION}-${ARCH}.AppImage"

echo "==> cargo build --release"
cargo build --release --locked

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/signallock-appimage.XXXXXX")"
trap 'rm -rf "${WORK_DIR}"' EXIT
APPDIR="${WORK_DIR}/${APP_NAME}.AppDir"

echo "==> Assembling ${APPDIR} with linuxdeploy"
"${LINUXDEPLOY}" \
    --appdir "${APPDIR}" \
    --executable "target/release/${BIN_NAME}" \
    --desktop-file packaging/linux/signallock.desktop \
    --icon-file packaging/linux/signallock.png

mkdir -p "${OUT_DIR}"
rm -f "${OUT_FILE}" "${OUT_FILE}.sha256"
echo "==> Creating ${OUT_FILE} with appimagetool"
"${APPIMAGETOOL}" --no-appstream "${APPDIR}" "${OUT_FILE}"
chmod +x "${OUT_FILE}"

(cd "${OUT_DIR}" && sha256sum "$(basename "${OUT_FILE}")" > "$(basename "${OUT_FILE}").sha256")

echo
echo "------------------------------------------------------------"
echo "  Output : ${OUT_FILE}"
echo "  Size   : $(du -h "${OUT_FILE}" | awk '{print $1}')"
echo "  SHA256 : $(awk '{print $1}' "${OUT_FILE}.sha256")"
echo "  Version: ${VERSION}"
echo "------------------------------------------------------------"
