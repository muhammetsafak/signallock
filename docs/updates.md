# Updates

SignalLock can update itself, but only when you ask it to. Click **Check for Updates…** in the tray / menu-bar menu.

## When the App Goes Online

Only on that click. There is no check at startup, none in the background and none on a schedule. The menu item is disabled while a check, download, verification or install is running.

## What Is Sent

An HTTPS request to `api.github.com` (the latest release of this repository). If a newer version exists and you choose *Download & Install*, the app then downloads the package for your platform and its `.sha256` file from GitHub. The request carries no cookie, token, account, device identifier or setting; the only identifying header is `User-Agent: SignalLock/<version>`. GitHub sees your IP address, the version you run and the time of the request, as for any download from GitHub. Nothing goes to a SignalLock server; there is none. Only GitHub hosts are contacted directly (`api.github.com`, `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`), and a redirect to any other host is refused. The standard proxy variables (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`) are honoured; if one is set, the connection goes to the proxy, which is asked only to tunnel to one of those hosts, and TLS runs end to end to the GitHub host, so the proxy sees the destination host and the timing, not the content. A proxy that intercepts TLS (re-signs certificates) is not trusted: behind one, the update check fails and the error window offers **Try Again**, **Open Download Page** and **Close**; installing from the download page is the way forward.

## The Flow

*Check for Updates…* opens an update window: up to date, or the new version with its release notes. *Download & Install* shows the download, then asks you to confirm: **SignalLock will quit and restart to finish the update, and monitoring is paused until the new version has started.** Cancelling deletes the downloaded file. Only a version newer than the running one is offered. Downloads are kept in `SignalLock/updates/` under the platform cache directory (`~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` on Linux) and are deleted afterwards (on Windows, the installer that is still running is removed at the next start).

## How Each Platform Installs

| Platform | Asset | What happens |
|---|---|---|
| macOS | `SignalLock-<version>-macos.app.zip` | Unpacked next to the running app and swapped in by rename; the old bundle is moved back if anything fails. The new bundle must pass `codesign` with Apple's anchor, Team ID `U7275J9F9Q` and identifier `com.signallock.app`, and its version must match the release. The app then relaunches. |
| Windows | `SignalLock-<version>-setup.exe` | The installer runs silently over the existing install and starts the new version. An all-users install still shows the UAC prompt; if you decline it, the installer does not run and you have to start SignalLock again yourself. |
| Linux | `SignalLock-<version>-x86_64.AppImage` | Only an AppImage started through `$APPIMAGE` is replaced in place (atomic rename), then started. `.deb`, `.rpm` and Flatpak are not updated. There is no aarch64 asset. |

## When You Have to Install by Hand

If the app cannot replace itself (macOS: not running from a `.app` bundle, running under App Translocation or in a folder it cannot write to; Linux: not started from an AppImage, or the AppImage or its folder is not writable), the window offers **Open Download Page**. The browser opens only when you click it. The same button appears after a failed update, along with *Try Again*; a release without a package for your platform or without its `.sha256` file also ends there.

## What Is Verified, and What Is Not

Verification is **SHA-256 only**. Every release asset is published with a `<asset>.sha256` file; the app checks that the file names the same asset and that the downloaded bytes hash to the value in it. Any mismatch, truncated or oversized download deletes the file and installs nothing (fail-closed). The app also refuses an asset whose name or version does not match the release tag. On macOS the code-signature check above comes on top.

## Known Risk

This risk is known and accepted. The `.sha256` file comes from the same GitHub Release as the package, so it catches corruption and incomplete downloads; it does not prove where the package came from. What stops a malicious update depends on what the attacker controls:

- *An attacker who can replace or upload release assets but cannot run the release workflow:* Windows and Linux install the replaced package. macOS rejects it, because the attacker cannot produce a bundle signed by Team ID `U7275J9F9Q`.
- *An attacker who can push a `v*` tag or change the workflow* (a compromised GitHub account, a leaked token with `contents: write`, a compromised release workflow): all three platforms install the attacker's package. On macOS it is built and signed with the real Developer ID, so the signature check passes.

What remains against both: TLS to GitHub-owned hosts only, the host allowlist, no downgrade, your confirmation before every install, and 2FA on the GitHub account. A required reviewer on the `release` environment is not set up; it is a separate decision.

## Other Limits

GitHub's unauthenticated API allows 60 requests per hour per IP, so a shared network can hit it (the app reports it and does not retry). If GitHub moves its asset downloads to a new host, updates fail closed until a release, installed by hand, widens the allowlist.
