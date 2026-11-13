# SignalLock

SignalLock locks your computer when a trusted Bluetooth device (phone, watch, earbuds, BLE tag) moves out of range. It is a tray / menu-bar app for **macOS, Windows and Linux**, written once in Rust. It has no backend, no account and no telemetry, and keeps its settings in two small JSON files on your machine. The only network request it ever makes is the one you start yourself with *Check for Updates…* (see [Updates](#updates)).

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

## Updates

SignalLock can update itself, but only when you ask it to (ADR-015 in the parent project's `.ssot/04-decisions.md`). Click **Check for Updates…** in the tray / menu-bar menu.

**When the app goes online.** Only on that click. There is no check at startup, none in the background and none on a schedule. The menu item is disabled while a check, download, verification or install is running.

**What is sent.** An HTTPS request to `api.github.com` (the latest release of this repository). If a newer version exists and you choose *Download & Install*, the app then downloads the package for your platform and its `.sha256` file from GitHub. The request carries no cookie, token, account, device identifier or setting; the only identifying header is `User-Agent: SignalLock/<version>`. GitHub sees your IP address, the version you run and the time of the request, as for any download from GitHub. Nothing goes to a SignalLock server; there is none. Only GitHub hosts are contacted directly (`api.github.com`, `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`), and a redirect to any other host is refused. The standard proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) are honoured; if one is set, the connection goes to the proxy, which is asked only to tunnel to one of those hosts, and TLS runs end to end to the GitHub host, so the proxy sees the destination host and the timing, not the content. A proxy that intercepts TLS (re-signs certificates) is not trusted: behind one, the update check fails and the error window offers **Try Again**, **Open Download Page** and **Close**; installing from the download page is the way forward.

**The flow.** *Check for Updates…* opens an update window: up to date, or the new version with its release notes. *Download & Install* shows the download, then asks you to confirm: **SignalLock will quit and restart to finish the update, and monitoring is paused until the new version has started.** Cancelling deletes the downloaded file. Only a version newer than the running one is offered. Downloads are kept in `SignalLock/updates/` under the platform cache directory (`~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` on Linux) and are deleted afterwards (on Windows, the installer that is still running is removed at the next start).

**How each platform installs.**

| Platform | Asset | What happens |
|---|---|---|
| macOS | `SignalLock-<version>-macos.app.zip` | Unpacked next to the running app and swapped in by rename; the old bundle is moved back if anything fails. The new bundle must pass `codesign` with Apple's anchor, Team ID `U7275J9F9Q` and identifier `com.signallock.app`, and its version must match the release. The app then relaunches. |
| Windows | `SignalLock-<version>-setup.exe` | The installer runs silently over the existing install and starts the new version. An all-users install still shows the UAC prompt; if you decline it, the installer does not run and you have to start SignalLock again yourself. |
| Linux | `SignalLock-<version>-x86_64.AppImage` | Only an AppImage started through `$APPIMAGE` is replaced in place (atomic rename), then started. `.deb`, `.rpm` and Flatpak are not updated. There is no aarch64 asset. |

**When you have to install by hand.** If the app cannot replace itself (macOS: not running from a `.app` bundle, running under App Translocation or in a folder it cannot write to; Linux: not started from an AppImage, or the AppImage or its folder is not writable), the window offers **Open Download Page**. The browser opens only when you click it. The same button appears after a failed update, along with *Try Again*; a release without a package for your platform or without its `.sha256` file also ends there.

**What is verified, and what is not.** Verification is **SHA-256 only**. Every release asset is published with a `<asset>.sha256` file; the app checks that the file names the same asset and that the downloaded bytes hash to the value in it. Any mismatch, truncated or oversized download deletes the file and installs nothing (fail-closed). The app also refuses an asset whose name or version does not match the release tag. On macOS the code-signature check above comes on top.

**Known risk (accepted, ADR-015).** The `.sha256` file comes from the same GitHub Release as the package, so it catches corruption and incomplete downloads; it does not prove where the package came from. What stops a malicious update depends on what the attacker controls:

- *An attacker who can replace or upload release assets but cannot run the release workflow:* Windows and Linux install the replaced package. macOS rejects it, because the attacker cannot produce a bundle signed by Team ID `U7275J9F9Q`.
- *An attacker who can push a `v*` tag or change the workflow* (a compromised GitHub account, a leaked token with `contents: write`, a compromised release workflow): all three platforms install the attacker's package. On macOS it is built and signed with the real Developer ID, so the signature check passes.

What remains against both: TLS to GitHub-owned hosts only, the host allowlist, no downgrade, your confirmation before every install, and 2FA on the GitHub account. A required reviewer on the `release` environment is not set up; it is a separate decision.

Other limits: GitHub's unauthenticated API allows 60 requests per hour per IP, so a shared network can hit it (the app reports it and does not retry). If GitHub moves its asset downloads to a new host, updates fail closed until a release, installed by hand, widens the allowlist.

## Permissions and platform notes

- **macOS:** Bluetooth access (System Settings, Privacy & Security, Bluetooth). The prompt appears on first launch of the signed bundle. The app runs without a Dock icon.
- **Linux:** BlueZ 5.55 or newer. This minimum was set for the earlier `bluer`-based plan and has not been re-verified with `btleplug`.
- **Linux, GNOME:** the stock shell shows no tray icons. Install the AppIndicator / KStatusNotifierItem extension, or the icon will not appear. KDE, Cinnamon, XFCE and similar desktops with a StatusNotifier host work without extras.
- **Windows:** no special permission.

## Settings and data

Two JSON files in the platform config directory, plus a log (update downloads use a separate cache directory, see [Updates](#updates)):

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

A release is a `v*` tag. The workflow calls `ci.yml` first, then builds the packages for the three platforms and publishes them as a GitHub Release (not a draft): `SignalLock-<version>.dmg`, `SignalLock-<version>-macos.app.zip`, `SignalLock-<version>-setup.exe`, `SignalLock-<version>-x86_64.AppImage`, each with a `.sha256` (eight assets in all). The `-macos.app.zip` is the same signed, notarized and stapled bundle that is inside the DMG, zipped with `ditto`; only the in-app updater uses it, and the web site keeps linking the DMG. Assets carry only the versioned name. The macOS package is always signed and notarized: if a secret is missing the job stops, and no ad-hoc package is ever published.

The compile step (`build-macos`) has no access to secrets. Only `sign-macos` does, through the `release` environment. It runs no cargo or build scripts, and it verifies the bundle against the SHA-256 that the build job published. `publish` checks every package against its own job's SHA-256 output before it creates the release, and then checks the shape of each `.sha256` file, because the in-app updater reads them: exactly one line, `<64 lowercase hex>  <asset name>`, LF-terminated, naming its own asset.

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
