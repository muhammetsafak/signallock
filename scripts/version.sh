#!/usr/bin/env bash
# Prints the workspace version (workspace.package.version in the root
# Cargo.toml). This is the single source of the release version: the macOS
# bundle, the Windows installer, the AppImage and the CI tag check all read it
# from here, and it is the value behind CARGO_PKG_VERSION.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

version="$(awk '
    /^\[/ { in_section = ($0 == "[workspace.package]") }
    in_section && /^version[[:space:]]*=/ {
        gsub(/^[^"]*"|".*$/, "")
        print
        exit
    }
' "${root}/Cargo.toml")"

if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]]; then
    echo "version.sh: could not read a SemVer workspace.package.version from ${root}/Cargo.toml (got '${version}')" >&2
    exit 1
fi

printf '%s\n' "${version}"
