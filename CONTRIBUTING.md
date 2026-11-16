# Contributing to SignalLock

Thank you for your interest in contributing. This guide explains how to propose changes.

## Before You Start

- Small fixes (typos, broken links, obvious bugs) can be sent directly as a pull request.
- For a new feature or a change in behavior, open an issue first so the approach can be discussed before you write code.
- Opening a pull request does not guarantee that it will be accepted.
- SignalLock has no backend, no account and no telemetry, and it goes online only when the user clicks *Check for Updates…*. Changes that add a network call, a server or data collection are not accepted.
- Do not report security vulnerabilities in issues or pull requests; follow the [security policy](SECURITY.md).

## Development Setup

Requirements:

- Rust 1.90 or newer. `rust-toolchain.toml` pins the `stable` channel with `rustfmt` and `clippy`.
- The platform build tools listed under [Installation](README.md#installation) in the README.

Clone the repository and build:

```bash
git clone https://github.com/muhammetsafak/signallock.git
cd signallock
cargo build
```

The workspace has two crates:

| Path | What it is |
|---|---|
| `crates/core` | `signallock-core`: proximity engine, settings models, JSON store. Pure Rust, no platform API, no async. |
| `crates/app` | `signallock`: the binary. BLE, platform services (lock, autostart, migration), tray, Slint windows, updater. |

Two manual probes exist for real hardware:

```bash
cargo run -p signallock --example ble_probe
cargo run -p signallock --example lock_probe -- --dry-run
```

`ble_probe` runs a 10 second discovery scan and logs every event. `lock_probe --dry-run` prints the lock strategy and locks nothing; without `--dry-run` it really locks the screen.

On macOS, `cargo run` is fine for compile checks, but the Bluetooth permission prompt needs the signed `.app` bundle; see [docs/packaging.md](docs/packaging.md).

## Running Tests

These are the same commands that CI runs. Run both before opening a pull request.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

```bash
cargo test --workspace --locked
```

CI runs clippy and the tests on macOS, Windows and Linux, because most of `crates/app` is platform-specific. A change that passes on your machine can still fail on another platform.

## Commit Messages

This project follows [Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/):

```text
<type>(<scope>)!: <description>

<body>
```

- **Types:** `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.
- **Scope** is optional, lowercase kebab-case, and names a module or area: `feat(update): ...`.
- **Description** is in English, imperative mood, starts with a lowercase letter, has no trailing period and is at most 72 characters.
- **Body** is optional and explains why the change was made. Wrap lines at 72 characters.
- Mark a breaking change only with `!` after the type or scope, and describe it in the body.
- Do not add footers or trailers (`BREAKING CHANGE:`, `Refs:`, `Closes #12`, `Co-Authored-By:`, `Signed-off-by:` or any tool attribution). Link issues in the pull request description instead.
- Keep one logical change per commit.

Example:

```text
fix(ble): keep the last RSSI sample when the scan restarts
```

## Pull Requests

1. Create a branch from `main` named `<type>/<short-kebab-description>`, for example `feat/away-delay-hint` or `fix/123-tray-tooltip`.
2. Write the pull request title in Conventional Commits format. Pull requests are squash merged, so the title becomes the commit message on `main`.
3. If the change affects users, add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).
4. Fill in the pull request template and link the related issue with `Closes #<number>`.
5. Make sure CI passes. A maintainer reviews the pull request and may ask for changes before merging.

## Code of Conduct

This project is governed by the [Code of Conduct](CODE_OF_CONDUCT.md). By participating, you agree to uphold it.
