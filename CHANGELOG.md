# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-11-16

First release of the single Rust codebase for macOS, Windows and Linux. It
replaces the separate Swift, .NET and Linux apps and upgrades their
installations in place.

### Added

- Lock the computer when the trusted Bluetooth device moves out of range, with
  the same proximity algorithm and settings on all three platforms.
- Tray / menu-bar app with a device picker and a settings window.
- Launch at login.
- One-time migration of the settings and trusted device saved by the previous
  macOS and Windows apps.
- "Check for Updates" menu item: a user-initiated check against GitHub
  Releases that downloads, verifies (SHA-256) and installs the new version.
  There is no startup, background or scheduled check.
- Packages: a signed and notarized DMG for macOS (universal), an installer for
  Windows and an AppImage for Linux, each published with a `.sha256` file.

[Unreleased]: https://github.com/muhammetsafak/signallock/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/muhammetsafak/signallock/releases/tag/v1.0.0
