# Releasing

## CI

- `ci.yml` runs on pushes to `main`, on pull requests and on manual dispatch. The `lint` job runs `cargo fmt --check` and `cargo clippy -D warnings`, and the `test` job runs `cargo test` and a release build, both on `ubuntu-22.04`. The `platforms` job repeats clippy, the tests and the release build on `macos-latest` and `windows-latest`. It reads no secrets.
- `release.yml` runs on `vX.Y.Z` tags. See below.

## Release Workflow

A release is a `vX.Y.Z` tag. The workflow calls `ci.yml` first, then builds the packages for the three platforms and publishes them as a GitHub Release (not a draft): `SignalLock-<version>.dmg`, `SignalLock-<version>-macos.app.zip`, `SignalLock-<version>-setup.exe`, `SignalLock-<version>-x86_64.AppImage`, each with a `.sha256` (eight assets in all). The `-macos.app.zip` is the same signed, notarized and stapled bundle that is inside the DMG, zipped with `ditto`; only the in-app updater uses it, and the web site keeps linking the DMG. Assets carry only the versioned name. The macOS package is always signed and notarized: if a secret is missing the job stops, and no ad-hoc package is ever published.

The compile step (`build-macos`) has no access to secrets. Only `sign-macos` does, through the `release` environment. It runs no cargo or build scripts, and it verifies the bundle against the SHA-256 that the build job published. `publish` checks every package against its own job's SHA-256 output before it creates the release, and then checks the shape of each `.sha256` file, because the in-app updater reads them: exactly one line, `<64 lowercase hex>  <asset name>`, LF-terminated, naming its own asset.

## One-Time Setup

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

## Cut a Release

1. Set `workspace.package.version` in `Cargo.toml` (and refresh `Cargo.lock`), commit.
2. Update `CHANGELOG.md`: move the `[Unreleased]` entries under the new version heading.
3. `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`. The tag is annotated and must equal the workspace version, or the `verify-version` job fails.

There is no dry run on a branch, because the environment admits only `v*` tags: the first tag push is the real run. `publish` does not run unless all three packages were built, so a failed notarization or a missing secret creates no release; delete the tag, fix, and tag again. To re-run signing without publishing, start *Release* manually (Actions, Run workflow) with an existing `v*` tag selected in *Use workflow from*; the packages are kept as workflow artifacts. A version with a hyphen, such as `1.0.0-rc.1`, is published as a pre-release.
