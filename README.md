# SignalLock

Tray app that locks your computer when a trusted Bluetooth device moves out of range.

[![CI](https://github.com/muhammetsafak/signallock/actions/workflows/ci.yml/badge.svg)](https://github.com/muhammetsafak/signallock/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/muhammetsafak/signallock)](LICENSE)
[![Designed & Maintained with Tan](https://muhammetsafak.com/badges/designed-maintained-with-tan.svg)](https://muhammetsafak.com/tan/)

SignalLock is a tray / menu-bar app for macOS, Windows and Linux, written once in Rust. You pick one Bluetooth device you carry (phone, watch, earbuds, BLE tag), and the computer locks when that device's signal stays weak for long enough. It is for people who walk away from an unlocked machine. It has no backend, no account and no telemetry, and the only network request it ever makes is the update check you start yourself.

## Features

- Locks the computer when the trusted device's signal stays below a threshold for a set delay.
- Lets you pick the trusted device from a live Bluetooth scan, and change or forget it later.
- Exposes three sensitivity settings: RSSI threshold, away delay and smoothing window.
- Shows status, current signal, last-seen time and the last automatic lock in the tray menu.
- Starts at login when you enable it.
- Offers a *Test Lock* menu item to check that locking works on your system.
- Updates itself only when you click *Check for Updates…*, and verifies the download with SHA-256.
- Keeps its settings in two JSON files on your machine.

## Quick Start

You need Rust 1.90 or newer and the platform build tools listed under [Installation](#installation).

```bash
git clone https://github.com/muhammetsafak/signallock.git
cd signallock
cargo run -p signallock --release
```

A SignalLock icon appears in the tray / menu bar. On first run its menu reads:

```text
SignalLock
Status: Paused
Trusted Device: (none)
Current Signal: —
Last Seen: —
Start Monitoring
Select Trusted Device…
Settings…
Test Lock
Check for Updates…
Quit SignalLock
```

On macOS, `cargo run` does not trigger the Bluetooth permission prompt. Build the signed bundle instead:

```bash
./scripts/build-macos.sh
open target/SignalLock.app
```

## Installation

No release has been published from this repository yet, so the app is built from source. Once releases exist, the packages will be on the [releases page](https://github.com/muhammetsafak/signallock/releases).

Build requirements:

- **All platforms:** Rust 1.90 or newer. `rust-toolchain.toml` pins the `stable` channel with `rustfmt` and `clippy`.
- **macOS:** macOS 13 or newer, Xcode command-line tools (`xcode-select --install`).
- **Windows:** Windows 10 or 11, the MSVC build tools.
- **Linux:** a desktop session with a D-Bus session bus, BlueZ, and these packages (Debian/Ubuntu names; Ubuntu 22.04 is the reference):

```bash
sudo apt-get install -y build-essential pkg-config file libdbus-1-dev \
    libfontconfig1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libegl1-mesa-dev libgl1-mesa-dev
```

There is no GTK or libappindicator dependency: on Linux the tray uses StatusNotifierItem over D-Bus.

To build the distributable package for a platform (`.dmg`, Inno Setup installer, AppImage), see [docs/packaging.md](docs/packaging.md).

## Usage

1. Open the tray menu and choose **Select Trusted Device…**. Click **Select** next to your device in the scan list, then **Done**.
2. Choose **Start Monitoring**. The status line changes to `Status: Monitoring`.
3. Walk away with the device. The status changes to `Status: Away (waiting to lock)`, and the computer locks when the away delay runs out.
4. Use **Pause Monitoring** to stop locking without forgetting the device.

**Settings…** opens the sensitivity controls:

| Setting | Range | Default | Effect |
|---|---|---|---|
| RSSI threshold | -100 to -40 dBm | -80 dBm | Signals weaker than this are treated as far. |
| Away delay | 5 to 120 s | 20 s | How long the device must stay far before the lock. Higher values reduce false locks from BLE noise. |
| Smoothing | 1 to 20 samples | 5 | How many signal samples are averaged. |

**Test Lock** locks the screen immediately. **Check for Updates…** is the only action that goes online; what it sends, how it verifies the download and its known limits are in [docs/updates.md](docs/updates.md).

Platform notes:

- **macOS:** the app needs Bluetooth access (System Settings, Privacy & Security, Bluetooth). The prompt appears on first launch of the signed bundle. The app runs without a Dock icon.
- **Linux:** BlueZ 5.55 or newer. This minimum was set for an earlier `bluer`-based plan and has not been re-verified with `btleplug`.
- **Linux, GNOME:** the stock shell shows no tray icons. Install the AppIndicator / KStatusNotifierItem extension, or the icon will not appear. KDE, Cinnamon, XFCE and similar desktops with a StatusNotifier host work without extras.
- **Windows:** no special permission.

## How It Works

SignalLock scans Bluetooth Low Energy advertisements. In discovery mode it lists every nearby device for the picker; in monitoring mode it follows only the trusted device. Each signal strength reading (RSSI) goes into a moving average over the smoothing window. When the average is weaker than the threshold, the device counts as away; when it has been away for the full away delay, the app locks the session with the platform's own lock mechanism.

The code is a single Cargo workspace:

| Path | What it is |
|---|---|
| `crates/core` | `signallock-core`: proximity engine, settings models, JSON store. Pure Rust, no platform API, no async. |
| `crates/app` | `signallock`: the binary. BLE, platform services (lock, autostart, migration), tray, Slint windows, updater. |
| `packaging/` | `Info.plist` and icon (macOS), Inno Setup script and icon (Windows), `.desktop` file and icon (Linux). |
| `scripts/` | Packaging scripts and `version.sh`. |

## Configuration

All settings are changed in the app; there are no environment variables or command-line options. They are stored in two JSON files in the platform config directory, next to a log:

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

Writes are atomic. A missing file means defaults; a corrupt file falls back to defaults with a warning in the log. Update downloads use a separate cache directory; see [docs/updates.md](docs/updates.md).

## Upgrading

Version 0.1.0 was a separate native app per platform. Its settings carry over:

- **macOS:** on first launch the old `UserDefaults` values (domain `com.signallock.app`, keys `SignalLock.Settings.v1` and `SignalLock.TrustedDevice.v1`) are copied once into the JSON files. The old keys are left in place and the JSON files are authoritative from then on.
- **Windows:** the 0.1.0 files are already in `%APPDATA%\SignalLock\`. They use PascalCase field names; they are read as they are and rewritten once in camelCase. Downgrading to 0.1.0 afterwards loses the settings.
- **Linux:** nothing to migrate.

The bundle ID `com.signallock.app` and the Windows autostart value name `SignalLock` are unchanged.

## Documentation

- [Updates](docs/updates.md): when the app goes online, what is sent, how each platform installs, what is verified and the known risk.
- [Packaging](docs/packaging.md): building the `.dmg`, the Windows installer and the AppImage.
- [Releasing](docs/releasing.md): the CI and release workflows, one-time signing setup and how to cut a release.

## Project Status

**Beta.** No release has been tagged from this repository yet; behavior and packaging can still change before the first one.

## Contributing

Contributions are welcome. Read the [contributing guide](CONTRIBUTING.md) before opening an issue or pull request.

## Security

Do not report security vulnerabilities through public issues; follow the [security policy](SECURITY.md) instead.

## License

Licensed under the [GNU GPL v3.0 or later](LICENSE) (`GPL-3.0-or-later`).
