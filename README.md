# SignalLock

SignalLock locks your computer when a trusted Bluetooth device (phone, watch, earbuds, BLE tag) moves out of range. It is a tray / menu-bar app for **macOS, Windows and Linux**, written once in Rust. It makes no network calls and keeps its settings in two small JSON files on your machine.

This directory is a single Cargo workspace. It replaces the per-platform codebases (`macos/` Swift, `windows/` C#); see ADR-014 in the parent project's `.ssot/04-decisions.md`.

## Layout

| Path | What it is |
|---|---|
| `crates/core` | `signallock-core`: proximity engine, settings models, JSON store. Pure Rust, no platform API, no async. |
| `crates/app` | `signallock`: the binary. BLE, platform services (lock, autostart, migration), tray, Slint windows. |
| `packaging/` | `Info.plist` and icon (macOS), Inno Setup script and icon (Windows), `.desktop` file and icon (Linux). |
| `scripts/` | Packaging scripts and `version.sh`. |
| `.github/workflows/` | `ci.yml` and `release.yml`. |

Module-by-module architecture, thread model and failure modes: `.ssot/02-architecture-app.md` in the parent project (private working notes, not part of this repository).

The version lives once, in `workspace.package.version` in `Cargo.toml`. `scripts/version.sh` prints it; every packaging script and the release workflow read it from there.

## Requirements

- Rust **1.90** or newer. `rust-toolchain.toml` pins the `stable` channel with `rustfmt` and `clippy`.
- **macOS:** macOS 13 or newer, Xcode command-line tools (`xcode-select --install`).
- **Windows:** Windows 10 or 11, the MSVC build tools.
- **Linux:** a desktop session with a D-Bus session bus, BlueZ, and these packages to build (Debian/Ubuntu names; Ubuntu 22.04 is the reference):

```bash
sudo apt-get install -y build-essential pkg-config file libdbus-1-dev \
    libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libegl1-mesa-dev libgl1-mesa-dev
```

There is no GTK or libappindicator dependency: on Linux the tray uses the `ksni` backend of `tray-icon` (StatusNotifierItem over D-Bus).

## Run and test

```bash
cargo run -p signallock                 # run the app (debug build)
cargo test --workspace                  # unit and integration tests
cargo fmt --all --check                 # formatting
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release                   # release binary: target/release/signallock
```

Two manual probes exist for real hardware:

```bash
cargo run -p signallock --example ble_probe          # 10 s discovery scan, logs every event
cargo run -p signallock --example lock_probe -- --dry-run   # prints the lock strategy, locks nothing
```

`lock_probe` without `--dry-run` really locks the screen.

On macOS, `cargo run` is fine for compile checks, but the Bluetooth permission prompt needs the signed `.app` bundle (below).

## Package

### macOS

```bash
./scripts/build-macos.sh                # universal (arm64 + x86_64) target/SignalLock.app
./scripts/make-dmg.sh                   # build the .app, then dist/SignalLock-<version>.dmg + .sha256
./scripts/make-dmg.sh --skip-build      # package the .app already in target/
```

Without `APPLE_DEV_ID` the bundle is signed ad-hoc. That is enough for local runs (it is what makes the Bluetooth prompt appear) but is not distributable. With `APPLE_DEV_ID` set, `build-macos.sh` signs with the hardened runtime and a secure timestamp. `make-dmg.sh` additionally re-signs the app and the DMG with it. With `NOTARY_PROFILE` (a `notarytool` keychain profile) it notarizes and staples both. Release builds do this in CI only; see [Releasing](#releasing).

On a Rust toolchain without `rustup` (for example Homebrew's), set `SIGNALLOCK_BUILD_STD=1` so the script builds the missing macOS target's standard library itself.

### Windows

Build the binary, then run Inno Setup 6 from the `app/` directory:

```powershell
cargo build --release
iscc /DMyAppVersion=<workspace version> packaging\windows\SignalLock.iss
```

Output: `dist\SignalLock-<version>-setup.exe`. The installer is unsigned. It keeps the `AppId` of the 0.1.0 installer, so installing over an existing 0.1.0 is an in-place upgrade. CI pins Inno Setup 6.7.1.

### Linux

```bash
./scripts/build-appimage.sh             # dist/SignalLock-<version>-x86_64.AppImage + .sha256
```

Runs on Linux x86_64 only. It needs `linuxdeploy` and `appimagetool` on `PATH` (or in `LINUXDEPLOY` / `APPIMAGETOOL`); the script does not install them. CI downloads pinned, checksum-verified releases of both.

## Permissions and platform notes

- **macOS:** Bluetooth access (System Settings, Privacy & Security, Bluetooth). The prompt appears on first launch of the signed bundle. The app runs without a Dock icon.
- **Linux:** BlueZ 5.55 or newer. This minimum was set for the earlier `bluer`-based plan and has not been re-verified with `btleplug`.
- **Linux, GNOME:** the stock shell shows no tray icons. Install the AppIndicator / KStatusNotifierItem extension, or the icon will not appear. KDE, Cinnamon, XFCE and similar desktops with a StatusNotifier host work without extras.
- **Windows:** no special permission.

## Settings and data

Two JSON files in the platform config directory, plus a log:

| Platform | Directory |
|---|---|
| macOS | `~/Library/Application Support/SignalLock/` |
| Windows | `%APPDATA%\SignalLock\` |
| Linux | `$XDG_CONFIG_HOME/SignalLock/` (fallback `~/.config/SignalLock/`) |

| File | Content |
|---|---|
| `settings.json` | RSSI threshold, away delay, smoothing window, monitoring flag, start at login |
| `trusted-device.json` | the one trusted device (identifier and name) |
| `signallock.log` | `tracing` log: targets `app`, `lock`, `proximity`, `bluetooth` |

Writes are atomic. A missing file means defaults; a corrupt file falls back to defaults with a warning in the log.

## Upgrading from 0.1.0

- **macOS:** on first launch the old `UserDefaults` values (domain `com.signallock.app`, keys `SignalLock.Settings.v1` and `SignalLock.TrustedDevice.v1`) are copied once into the JSON files. The old keys are left in place and the JSON files are authoritative from then on.
- **Windows:** the 0.1.0 files are already in `%APPDATA%\SignalLock\`. They use PascalCase field names; they are read as they are and rewritten once in camelCase. Downgrading to 0.1.0 afterwards loses the settings.
- **Linux:** nothing to migrate.

The bundle ID `com.signallock.app` and the Windows autostart value name `SignalLock` are unchanged.

## CI

- `ci.yml` runs on pushes to `main`, on pull requests and on manual dispatch: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and a release build on `macos-latest`, `windows-latest` and `ubuntu-22.04`. It reads no secrets.
- `release.yml` runs on `v*` tags. See below.

## Releasing

A release is a `v*` tag. The workflow calls `ci.yml` first, then builds the three packages and publishes them as a GitHub Release (not a draft): `SignalLock-<version>.dmg`, `SignalLock-<version>-setup.exe`, `SignalLock-<version>-x86_64.AppImage`, each with a `.sha256`. Assets carry only the versioned name. The macOS package is always signed and notarized: if a secret is missing the job stops, and no ad-hoc package is ever published.

The compile step (`build-macos`) has no access to secrets. Only `sign-macos` does, through the `release` environment. It runs no cargo or build scripts, and it verifies the bundle against the SHA-256 that the build job published. `publish` checks every package against its own job's SHA-256 output before it creates the release.

### One-time setup

Do this after the repository exists on GitHub. The order matters: environment and rule first, secrets second.

1. **Developer ID Application certificate.** `security find-identity -v -p codesigning | grep "Developer ID Application"` must print a line. Apple Development and Developer ID Installer certificates do not work.
2. **Export it as `.p12`** (Keychain Access, My Certificates, export the certificate row with its private key, with a strong password). Keep the file out of iCloud and out of the repository.
3. **Create the `release` environment** (Settings, Environments, New environment, name exactly `release`). Under *Deployment branches and tags*, choose *Selected branches and tags* and add one rule: ref type **Tag**, pattern `v*`. Add no other rule. Optionally require yourself as reviewer, and keep administrator bypass off.
4. **Add the secrets to the environment, not to the repository.** Every command needs `--env release`:

   | Secret | Value |
   |---|---|
   | `MACOS_CERT_P12_BASE64` | base64 of the `.p12` |
   | `MACOS_CERT_PASSWORD` | the `.p12` password |
   | `APPLE_ID` | Apple ID used for notarization |
   | `APPLE_APP_PASSWORD` | app-specific password for that Apple ID (account.apple.com, Sign-In and Security) |
   | `APPLE_TEAM_ID` | Apple Developer Team ID |

   ```bash
   base64 -i DeveloperID.p12 | gh secret set MACOS_CERT_P12_BASE64 --env release --repo muhammetsafak/signallock
   gh secret set MACOS_CERT_PASSWORD --env release --repo muhammetsafak/signallock
   gh secret set APPLE_ID            --env release --repo muhammetsafak/signallock
   gh secret set APPLE_APP_PASSWORD  --env release --repo muhammetsafak/signallock
   gh secret set APPLE_TEAM_ID       --env release --repo muhammetsafak/signallock
   ```

   Commands without a piped value prompt for it, so nothing lands in shell history.
5. **Clean up.** Delete the `.p12` (`rm -P DeveloperID.p12`) and keep neither it nor its password in the repository.
6. **Check.** `gh secret list --env release --repo muhammetsafak/signallock` lists the five names; `gh secret list --repo muhammetsafak/signallock` must not.

If a secret leaks: revoke the certificate at developer.apple.com, delete the app-specific password at account.apple.com, create new ones and update the environment secrets.

### Cut a release

1. Set `workspace.package.version` in `Cargo.toml` (and refresh `Cargo.lock`), commit.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag must equal the workspace version, or the `verify-version` job fails.

There is no dry run on a branch, because the environment admits only `v*` tags: the first tag push is the real run. `publish` does not run unless all three packages were built, so a failed notarization or a missing secret creates no release; delete the tag, fix, and tag again. To re-run signing without publishing, start *Release* manually (Actions, Run workflow) with an existing `v*` tag selected in *Use workflow from*; the packages are kept as workflow artifacts. A version with a hyphen, such as `1.0.0-rc.1`, is published as a pre-release.

## License

GPL-3.0-or-later (`license` in `Cargo.toml`); the full text is in [`LICENSE`](./LICENSE).
