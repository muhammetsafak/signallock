# Packaging

How to build the distributable package for each platform from source. The version comes from `workspace.package.version` in `Cargo.toml`; `scripts/version.sh` prints it, and every packaging script and the release workflow read it from there.

## macOS

```bash
./scripts/build-macos.sh                # universal (arm64 + x86_64) target/SignalLock.app
./scripts/make-dmg.sh                   # build the .app, then dist/SignalLock-<version>.dmg + .sha256
./scripts/make-dmg.sh --skip-build      # package the .app already in target/
```

Without `APPLE_DEV_ID` the bundle is signed ad-hoc. That is enough for local runs (it is what makes the Bluetooth prompt appear) but is not distributable. With `APPLE_DEV_ID` set, `build-macos.sh` signs with the hardened runtime and a secure timestamp. `make-dmg.sh` additionally re-signs the app and the DMG with it. With `NOTARY_PROFILE` (a `notarytool` keychain profile) it notarizes and staples both. Release builds do this in CI only; see [Releasing](releasing.md).

On a Rust toolchain without `rustup` (for example Homebrew's), set `SIGNALLOCK_BUILD_STD=1` so the script builds the missing macOS target's standard library itself.

## Windows

Build the binary, then run Inno Setup 6 from the repository root:

```powershell
cargo build --release
iscc /DMyAppVersion=<workspace version> packaging\windows\SignalLock.iss
```

Output: `dist\SignalLock-<version>-setup.exe`. The installer is unsigned. It keeps the `AppId` of the 0.1.0 installer, so installing over an existing 0.1.0 is an in-place upgrade. CI pins Inno Setup 6.7.1.

## Linux

```bash
./scripts/build-appimage.sh             # dist/SignalLock-<version>-x86_64.AppImage + .sha256
```

Runs on Linux x86_64 only. It needs `linuxdeploy` and `appimagetool` on `PATH` (or in `LINUXDEPLOY` / `APPIMAGETOOL`); the script does not install them. CI downloads pinned, checksum-verified releases of both.
