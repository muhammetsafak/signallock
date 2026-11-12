//! SignalLock desktop app entry point.
//!
//! Startup order (see `.ssot/02-architecture-app.md`): logging, single-instance
//! guard, config dir, `Store`, `platform::migrate_legacy`, tokio runtime
//! thread, `AppState`, tray, Slint event loop. Only the first step and the
//! config directory exist so far; the rest arrive in Faz 03 to 05.

mod app_state;
mod ble;
mod lock_service;
mod logging;
mod platform;
mod tray;

fn main() {
    let config_dir = platform::config_dir();
    let _log_guard = logging::init(&config_dir);
    tracing::info!(
        target: logging::APP,
        "SignalLock {} starting (config dir: {})",
        env!("CARGO_PKG_VERSION"),
        config_dir.display()
    );
}
