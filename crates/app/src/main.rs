//! SignalLock desktop app entry point.
//!
//! Startup order (see `.ssot/02-architecture-app.md`): logging, single-instance
//! guard, config dir, `Store`, `platform::migrate_legacy`, tokio runtime,
//! `AppState`, tray, Slint event loop.
//!
//! Thread model: the Slint event loop runs on the main thread together with
//! `AppState`, the tray and the windows. The tokio runtime has its own worker
//! threads (BLE task, 1 Hz tick, blocking lock calls, forwarding of the
//! platform's session events). Everything that crosses over to the main thread
//! goes through [`post`], i.e. `slint::invoke_from_event_loop`.

// No console window behind the release build on Windows.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use signallock::app_state::{self, AppState, LockRequester, UiCommand};
use signallock::ble::{BleScanner, BtleplugScanner};
use signallock::lock_service::LockService;
use signallock::logging;
use signallock::platform::{self, SystemAutostart};
use signallock::tray::{self, MenuAction};
use signallock::ui::{self, Windows};
use signallock_core::Store;

/// Name of the worker threads of the tokio runtime; the panic hook keys on it.
const RUNTIME_THREAD_NAME: &str = "signallock-tokio";

/// Runs `f` on the main thread with the installed `AppState`. Returns `false`
/// once the event loop is gone (the caller should stop producing events).
fn post(f: impl FnOnce(&Rc<AppState>) + Send + 'static) -> bool {
    slint::invoke_from_event_loop(move || {
        if app_state::with_current(f).is_none() {
            tracing::warn!(target: logging::APP, "event dropped: AppState is not installed");
        }
    })
    .is_ok()
}

/// Runs `LockService::lock_now` on a blocking tokio thread and reports the
/// outcome back to `AppState` on the main thread.
struct BackgroundLocker {
    runtime: tokio::runtime::Handle,
}

impl LockRequester for BackgroundLocker {
    fn request_lock(&self) {
        self.runtime.spawn_blocking(|| {
            let result = LockService::new().lock_now();
            post(move |app| app.on_lock_result(result));
        });
    }
}

fn main() -> ExitCode {
    let config_dir = platform::config_dir();
    let _log_guard = logging::init(&config_dir);
    tracing::info!(
        target: logging::APP,
        "SignalLock {} starting (config dir: {})",
        env!("CARGO_PKG_VERSION"),
        config_dir.display()
    );

    let _instance = match instance::acquire(&config_dir) {
        instance::Outcome::First(guard) => Some(guard),
        instance::Outcome::AlreadyRunning => {
            tracing::info!(target: logging::APP, "another instance is already running, exiting");
            return ExitCode::SUCCESS;
        }
        instance::Outcome::Unavailable(reason) => {
            tracing::warn!(
                target: logging::APP,
                "single-instance guard unavailable ({reason}); continuing without it"
            );
            None
        }
    };

    install_panic_hook();

    let store = Store::new(config_dir);
    match platform::migrate_legacy(&store) {
        Ok(outcome) => {
            tracing::info!(target: logging::APP, "Legacy settings migration: {outcome:?}")
        }
        Err(e) => tracing::warn!(target: logging::APP, "Legacy settings migration failed: {e}"),
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(RUNTIME_THREAD_NAME)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            tracing::error!(target: logging::APP, "tokio runtime could not be started: {e}");
            return ExitCode::FAILURE;
        }
    };

    // `invoke_from_event_loop` (the bridge from every other thread) needs the
    // platform backend, which Slint otherwise picks when the first window is
    // created. Select it now, on the main thread.
    if let Err(e) = slint::BackendSelector::new().select() {
        tracing::error!(target: logging::APP, "UI backend could not be selected: {e}");
        return ExitCode::FAILURE;
    }

    let mut scanner = BtleplugScanner::new(runtime.handle());
    let ble_events = scanner.take_events();
    let app = AppState::new(
        store,
        Box::new(scanner),
        Box::new(BackgroundLocker {
            runtime: runtime.handle().clone(),
        }),
        Box::new(SystemAutostart::new()),
    );
    app_state::install(app.clone());

    let windows = Windows::install(&app);
    app.set_ui_handler(move |command| match command {
        UiCommand::Quit => {
            tracing::info!(target: logging::APP, "Quit requested");
            if let Err(e) = slint::quit_event_loop() {
                tracing::error!(target: logging::APP, "event loop could not be stopped: {e}");
            }
        }
        window_command => windows.handle(window_command),
    });

    // Menu clicks arrive on whatever thread the tray backend uses.
    muda::MenuEvent::set_event_handler(Some(|event: muda::MenuEvent| {
        match MenuAction::from_id(event.id().as_ref()) {
            Some(action) => {
                post(move |app| tray::dispatch(action, app));
            }
            None => tracing::debug!(target: logging::APP, "unknown menu id {:?}", event.id()),
        }
    }));

    // Nothing here uses tray clicks or hover, but `tray-icon` queues every
    // such event on an unbounded channel while no handler is set; a handler
    // keeps that queue empty for the life of the process.
    tray_icon::TrayIconEvent::set_event_handler(Some(|_: tray_icon::TrayIconEvent| {}));

    // On macOS the unlock notification is delivered through the main run loop,
    // so the source is created here, on the main thread.
    let mut session_events = platform::session_events();

    if let Some(mut events) = ble_events {
        runtime.spawn(async move {
            while let Some(event) = events.recv().await {
                if !post(move |app| app.on_ble_event(event)) {
                    break;
                }
            }
        });
    }
    runtime.spawn(async move {
        while let Some(event) = session_events.recv().await {
            if !post(move |app| app.on_session_event(event)) {
                break;
            }
        }
    });
    runtime.spawn(async {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !post(|app| app.on_tick()) {
                break;
            }
        }
    });

    // The tray is created once the event loop runs (required on macOS), then
    // monitoring starts if there is a trusted device (ADR-008).
    if let Err(e) = slint::invoke_from_event_loop(|| {
        ui::prepare_event_loop();
        app_state::with_current(|app| {
            tray::install(app);
            app.start();
        });
    }) {
        tracing::error!(target: logging::APP, "startup could not be scheduled: {e}");
        return ExitCode::FAILURE;
    }

    let result = slint::run_event_loop_until_quit();
    tracing::info!(target: logging::APP, "SignalLock exiting");
    runtime.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(target: logging::APP, "event loop failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A panic on a tokio thread means no BLE and no tick: the app would sit in
/// the tray protecting nothing. Log it and end the process instead
/// (`.ssot/02-architecture-app.md`, failure table). Other threads keep the
/// default behaviour.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            target: logging::APP,
            "panic on thread '{}': {info}",
            thread.name().unwrap_or("?")
        );
        default_hook(info);
        if thread.name() == Some(RUNTIME_THREAD_NAME) {
            // Give the non-blocking log writer a moment to flush the line.
            std::thread::sleep(Duration::from_millis(200));
            std::process::exit(1);
        }
    }));
}

/// Single-instance guard. Windows: the named mutex of `windows/Program.cs`.
/// macOS and Linux: an exclusive `File::try_lock` on a lock file in the config
/// directory. The guard is held for the life of the process.
mod instance {
    use super::Path;

    pub enum Outcome {
        First(Guard),
        AlreadyRunning,
        Unavailable(String),
    }

    #[cfg(not(target_os = "windows"))]
    pub struct Guard {
        _file: std::fs::File,
    }

    #[cfg(not(target_os = "windows"))]
    pub fn acquire(config_dir: &Path) -> Outcome {
        use std::fs::{File, TryLockError};

        if let Err(e) = std::fs::create_dir_all(config_dir) {
            return Outcome::Unavailable(format!("config dir: {e}"));
        }
        let file = match File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(config_dir.join("signallock.lock"))
        {
            Ok(file) => file,
            Err(e) => return Outcome::Unavailable(format!("lock file: {e}")),
        };
        match file.try_lock() {
            Ok(()) => Outcome::First(Guard { _file: file }),
            Err(TryLockError::WouldBlock) => Outcome::AlreadyRunning,
            Err(TryLockError::Error(e)) => Outcome::Unavailable(format!("lock: {e}")),
        }
    }

    #[cfg(target_os = "windows")]
    pub struct Guard {
        _handle: windows::Win32::Foundation::HANDLE,
    }

    #[cfg(target_os = "windows")]
    pub fn acquire(_config_dir: &Path) -> Outcome {
        use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::w;

        // SAFETY: plain Win32 call with a static, NUL-terminated name and no
        // security attributes. The handle is kept in the guard and never
        // closed, so the mutex lives as long as the process.
        let created = unsafe { CreateMutexW(None, true, w!("Global\\SignalLock.SingleInstance")) };
        match created {
            // SAFETY: reads the calling thread's last-error value right after
            // the call above, before anything else can overwrite it.
            Ok(handle) if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS => {
                // SAFETY: `handle` was just returned by `CreateMutexW` and is
                // not used afterwards. Another instance owns the mutex.
                let _ = unsafe { CloseHandle(handle) };
                Outcome::AlreadyRunning
            }
            Ok(handle) => Outcome::First(Guard { _handle: handle }),
            Err(e) => Outcome::Unavailable(format!("mutex: {e}")),
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::instance::{Outcome, acquire};

    #[test]
    fn a_second_instance_is_detected_while_the_first_holds_the_lock() {
        let dir = std::env::temp_dir().join(format!("signallock-instance-{}", std::process::id()));
        let first = acquire(&dir);
        assert!(matches!(first, Outcome::First(_)));
        assert!(matches!(acquire(&dir), Outcome::AlreadyRunning));
        drop(first);
        assert!(matches!(acquire(&dir), Outcome::First(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
