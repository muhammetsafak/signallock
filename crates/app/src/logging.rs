//! `tracing` subscriber: stdout plus `signallock.log` in the config dir.
//!
//! Log categories are `tracing` targets and match the references
//! (`Logging.swift`, `Logging.cs`): `app`, `lock`, `proximity`, `bluetooth`.
//! The file is opened in append mode and never rotated, like the Windows
//! reference; the user rolls it by deleting it. Logging must never crash the
//! app, so a failing file sink degrades to stdout only.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// General application events.
pub const APP: &str = "app";
/// Screen-lock strategies and results.
#[allow(dead_code)] // first used by lock_service (Faz 04)
pub const LOCK: &str = "lock";
/// Proximity decisions. `signallock-core` uses the same literal.
#[allow(dead_code)] // the core crate logs under this target
pub const PROXIMITY: &str = "proximity";
/// BLE scanning.
#[allow(dead_code)] // first used by the ble module (Faz 03)
pub const BLUETOOTH: &str = "bluetooth";

/// File name of the log inside the config dir.
pub const LOG_FILE: &str = "signallock.log";

/// Initialises logging. The returned guard flushes the file sink on drop and
/// must live as long as the process (`main` keeps it in a local).
///
/// The level comes from `RUST_LOG` when set, `info` otherwise.
pub fn init(config_dir: &Path) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout_layer = tracing_subscriber::fmt::layer().with_target(true);

    let (file_layer, guard, file_error) = match open_log_file(config_dir) {
        Ok((writer, guard)) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(writer);
            (Some(layer), Some(guard), None)
        }
        Err(e) => (None, None, Some(e)),
    };

    // A second `init` (tests, repeated calls) must not panic.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(file_layer)
        .try_init();

    if let Some(e) = file_error {
        tracing::warn!(
            target: APP,
            "Log file unavailable in {}: {e}; logging to stdout only",
            config_dir.display()
        );
    }
    guard
}

fn open_log_file(
    config_dir: &Path,
) -> std::io::Result<(
    tracing_appender::non_blocking::NonBlocking,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    std::fs::create_dir_all(config_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_dir.join(LOG_FILE))?;
    Ok(tracing_appender::non_blocking(file))
}
