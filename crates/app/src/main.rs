//! SignalLock desktop app entry point.
//!
//! Startup order (see `.ssot/02-architecture-app.md`): logging, single-instance
//! guard (retried briefly, for a restart after an update), cleanup of leftover
//! update downloads and of what an interrupted update left beside the app (only
//! by the instance that took the guard), config dir, `Store`, `platform::migrate_legacy`, tokio
//! runtime, `AppState`, tray, Slint event loop.
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

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use signallock::app_state::{
    self, AppState, LockRequester, UiCommand, UpdateDeps, UpdateJob, UpdateJobs,
};
use signallock::ble::{BleScanner, BtleplugScanner};
use signallock::lock_service::LockService;
use signallock::logging;
use signallock::platform::{self, SystemAutostart};
use signallock::tray::{self, MenuAction};
use signallock::ui::{self, Windows};
use signallock::update::GithubUpdateChecker;
use signallock::update_service::{self, SystemInstaller};
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

/// Runs update jobs on the tokio blocking pool and hands their events to
/// `AppState` on the main thread. The HTTP client blocks, so the BLE task and the tick
/// are never involved.
struct BlockingUpdateJobs {
    runtime: tokio::runtime::Handle,
}

impl UpdateJobs for BlockingUpdateJobs {
    fn spawn(&self, job: UpdateJob) {
        self.runtime.spawn_blocking(move || {
            job(&|event| {
                post(move |app| app.on_update_event(event));
            });
        });
    }
}

/// How often and how long a new process waits for the single-instance guard
/// when another instance holds it: the old process of an update restart may
/// still be shutting down (ADR-015). Five retries of 200 ms, one second in all.
const GUARD_RETRIES: u32 = 5;
const GUARD_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Calls `acquire` and, while it reports another running instance, again up to
/// `retries` more times with `delay` between the calls.
fn acquire_with_retry(
    retries: u32,
    delay: Duration,
    mut acquire: impl FnMut() -> instance::Outcome,
) -> instance::Outcome {
    let mut outcome = acquire();
    for _ in 0..retries {
        if !matches!(outcome, instance::Outcome::AlreadyRunning) {
            break;
        }
        std::thread::sleep(delay);
        outcome = acquire();
    }
    outcome
}

/// What an in-place update interrupted halfway leaves **next to the app**, out
/// of reach of the updates cache: on macOS the staging folder
/// `.SignalLock-update-<pid>` beside `SignalLock.app`, on Linux the copy
/// `.SignalLock.update-<pid>` beside the AppImage. Windows leaves nothing there
/// (its installer sits in the cache).
///
/// Only entries that match the name exactly, are not symlinks, are of the right
/// kind and belong to the user that runs the app are removed.
mod leftovers {
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    use signallock::logging;

    // Each platform uses one of the two patterns; the tests cover both.

    /// What the leftover is.
    #[cfg(unix)]
    #[cfg_attr(not(test), allow(dead_code))]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Kind {
        Directory,
        File,
    }

    #[cfg(unix)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub const MACOS_STAGING: (&str, Kind) = (".SignalLock-update-", Kind::Directory);
    #[cfg(unix)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub const LINUX_COPY: (&str, Kind) = (".SignalLock.update-", Kind::File);

    /// Removes the entries of `dir` named `<prefix><digits>` that are real (not
    /// symlinked) `kind` entries owned by `owner_uid`. Returns how many were
    /// removed. A failure to remove one is logged and does not stop the rest.
    #[cfg(unix)]
    pub fn remove_matching(dir: &Path, (prefix, kind): (&str, Kind), owner_uid: u32) -> usize {
        use std::os::unix::fs::MetadataExt;

        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(suffix) = name.strip_prefix(prefix) else {
                continue;
            };
            if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // `symlink_metadata`: a symlink is reported as itself, never as
            // what it points to.
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let right_kind = match kind {
                Kind::Directory => meta.file_type().is_dir(),
                Kind::File => meta.file_type().is_file(),
            };
            if !right_kind || meta.uid() != owner_uid {
                continue;
            }
            let result = match kind {
                Kind::Directory => std::fs::remove_dir_all(entry.path()),
                Kind::File => std::fs::remove_file(entry.path()),
            };
            match result {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    target: logging::APP,
                    "could not remove the update leftover {}: {e}",
                    entry.path().display()
                ),
            }
        }
        removed
    }

    /// The directory that holds the running app: the parent of
    /// `<name>.app` on macOS, of `$APPIMAGE` on Linux. `None` for a bare
    /// binary (`cargo run`), where nothing is touched.
    #[cfg(unix)]
    pub fn app_dir() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            // <dir>/<name>.app/Contents/MacOS/<exe>
            let exe = std::env::current_exe().ok()?;
            let macos = exe.parent()?;
            let contents = macos.parent()?;
            let bundle = contents.parent()?;
            let is_app = bundle
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() > ".app".len() && n.ends_with(".app"));
            (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && is_app)
                .then(|| bundle.parent().map(Path::to_path_buf))?
        }
        #[cfg(target_os = "linux")]
        {
            let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
            appimage
                .is_absolute()
                .then(|| appimage.parent().map(Path::to_path_buf))?
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    /// Startup cleanup beside the app. Runs only in the instance that holds the
    /// single-instance guard. `config_dir` was created by this user and gives
    /// the uid to compare with.
    pub fn clean_beside_the_app(config_dir: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let Some(dir) = app_dir() else { return };
            let Ok(owner) = std::fs::metadata(config_dir).map(|m| m.uid()) else {
                return;
            };
            #[cfg(target_os = "macos")]
            let pattern = MACOS_STAGING;
            #[cfg(not(target_os = "macos"))]
            let pattern = LINUX_COPY;
            let removed = remove_matching(&dir, pattern, owner);
            if removed > 0 {
                tracing::info!(
                    target: logging::APP,
                    "removed {removed} leftover(s) of an interrupted update from {}",
                    dir.display()
                );
            }
        }
        #[cfg(not(unix))]
        let _ = config_dir;
    }
}

/// Takes the single-instance guard again into `slot` (one attempt: no wait,
/// since nobody is shutting down). `true` when the app holds it, `false` when
/// another instance has it or it cannot be taken: the caller must then quit.
fn restore_guard(
    slot: &RefCell<Option<instance::Guard>>,
    acquire: impl FnOnce() -> instance::Outcome,
) -> bool {
    if slot.borrow().is_some() {
        return true;
    }
    match acquire() {
        instance::Outcome::First(guard) => {
            *slot.borrow_mut() = Some(guard);
            true
        }
        instance::Outcome::AlreadyRunning | instance::Outcome::Unavailable(_) => false,
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

    let instance_guard = match acquire_with_retry(GUARD_RETRIES, GUARD_RETRY_DELAY, || {
        instance::acquire(&config_dir)
    }) {
        instance::Outcome::First(guard) => {
            // Leftovers of a previous update (a Windows installer that was
            // still running when the app exited). Local files only, no
            // network. Only the instance that holds the guard may do this:
            // without it another instance could be mid-download, and this
            // would delete its files.
            update_service::cleanup_stale_downloads();
            leftovers::clean_beside_the_app(&config_dir);
            Some(guard)
        }
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
    // Held by the update restart path, which releases it before the new
    // version is started (`UiCommand::ReleaseInstanceGuard`).
    let instance_guard = Rc::new(RefCell::new(instance_guard));
    // Whether this process gave a guard up for the update restart.
    let guard_released = Rc::new(Cell::new(false));

    install_panic_hook();

    let store = Store::new(config_dir.clone());
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
        UpdateDeps {
            // The only place a checker is made, and `AppState` calls it only
            // for the *Check for Updates…* command.
            checker_factory: Box::new(|| Box::new(GithubUpdateChecker::new())),
            // One installer for the whole process (it holds the install lock
            // and the pending relaunch).
            installer: Arc::new(SystemInstaller::new()),
            open_page: Arc::new(update_service::open_release_page),
            jobs: Box::new(BlockingUpdateJobs {
                runtime: runtime.handle().clone(),
            }),
        },
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
        UiCommand::ReleaseInstanceGuard => {
            let released = instance_guard.borrow_mut().take().is_some();
            guard_released.set(released);
            tracing::info!(target: logging::APP, "Single-instance guard released for the update restart (held: {released})");
        }
        UiCommand::RestoreInstanceGuard => {
            // Only a guard this process gave up is taken back. If it cannot be,
            // another instance may be running (the relaunch half-worked): this
            // one quits rather than run beside it.
            if guard_released.get() {
                if restore_guard(&instance_guard, || instance::acquire(&config_dir)) {
                    guard_released.set(false);
                    tracing::info!(target: logging::APP, "Single-instance guard taken again after the failed restart");
                } else {
                    tracing::error!(
                        target: logging::APP,
                        "single-instance guard could not be taken again after the failed restart; quitting so that no second instance runs"
                    );
                    if let Err(e) = slint::quit_event_loop() {
                        tracing::error!(target: logging::APP, "event loop could not be stopped: {e}");
                    }
                }
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
        handle: windows::Win32::Foundation::HANDLE,
    }

    /// Releasing the guard closes the mutex handle, so that the update restart
    /// can let the new process take the mutex while this one shuts down.
    #[cfg(target_os = "windows")]
    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: `handle` came from `CreateMutexW`, is owned by this guard
            // and is closed exactly once, here.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
        }
    }

    #[cfg(target_os = "windows")]
    pub fn acquire(_config_dir: &Path) -> Outcome {
        use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::w;

        // SAFETY: plain Win32 call with a static, NUL-terminated name and no
        // security attributes. The handle is kept in the guard and closed when
        // the guard is dropped (the process end, or the update restart).
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
            Ok(handle) => Outcome::First(Guard { handle }),
            Err(e) => Outcome::Unavailable(format!("mutex: {e}")),
        }
    }
}

#[cfg(all(test, unix))]
mod leftover_tests {
    use super::leftovers::{LINUX_COPY, MACOS_STAGING, remove_matching};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};
    use std::path::Path;

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("signallock-left-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn only_matching_real_user_owned_staging_folders_are_removed() {
        let app_dir = scratch("macos");
        let me = fs::metadata(&app_dir).unwrap().uid();
        let outside = scratch("macos-outside");
        fs::write(outside.join("keep.txt"), b"keep").unwrap();

        // Leftovers of an interrupted update (with content).
        fs::create_dir_all(app_dir.join(".SignalLock-update-4242/SignalLock.app")).unwrap();
        fs::write(
            app_dir.join(".SignalLock-update-4242/SignalLock.app/x"),
            b"x",
        )
        .unwrap();
        fs::create_dir(app_dir.join(".SignalLock-update-7")).unwrap();
        // Things that must stay: the app itself, other names, wrong kind,
        // symlinks (to a directory and dangling) and a non-numeric suffix.
        fs::create_dir(app_dir.join("SignalLock.app")).unwrap();
        fs::create_dir(app_dir.join(".SignalLock-update-")).unwrap();
        fs::create_dir(app_dir.join(".SignalLock-update-abc")).unwrap();
        fs::create_dir(app_dir.join(".SignalLock-update-12x")).unwrap();
        fs::create_dir(app_dir.join("SignalLock-update-5")).unwrap();
        fs::create_dir(app_dir.join("x.SignalLock-update-5")).unwrap();
        fs::write(
            app_dir.join(".SignalLock-update-99"),
            b"a file, not a folder",
        )
        .unwrap();
        symlink(&outside, app_dir.join(".SignalLock-update-31")).unwrap();
        symlink(
            app_dir.join("nowhere"),
            app_dir.join(".SignalLock-update-32"),
        )
        .unwrap();

        assert_eq!(remove_matching(&app_dir, MACOS_STAGING, me), 2);

        assert_eq!(
            names(&app_dir),
            vec![
                ".SignalLock-update-",
                ".SignalLock-update-12x",
                ".SignalLock-update-31",
                ".SignalLock-update-32",
                ".SignalLock-update-99",
                ".SignalLock-update-abc",
                "SignalLock-update-5",
                "SignalLock.app",
                "x.SignalLock-update-5",
            ]
        );
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
        let _ = fs::remove_dir_all(&app_dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn entries_of_another_user_are_left_alone() {
        let dir = scratch("owner");
        let me = fs::metadata(&dir).unwrap().uid();
        fs::create_dir(dir.join(".SignalLock-update-1")).unwrap();
        fs::write(dir.join(".SignalLock.update-1"), b"x").unwrap();

        assert_eq!(remove_matching(&dir, MACOS_STAGING, me.wrapping_add(1)), 0);
        assert_eq!(remove_matching(&dir, LINUX_COPY, me.wrapping_add(1)), 0);
        assert_eq!(names(&dir).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_matching_regular_appimage_copies_are_removed_on_linux_layouts() {
        let dir = scratch("linux");
        let me = fs::metadata(&dir).unwrap().uid();
        let outside = scratch("linux-outside");
        fs::write(outside.join("real.AppImage"), b"user data").unwrap();

        fs::write(dir.join(".SignalLock.update-4242"), b"half a copy").unwrap();
        fs::write(dir.join(".SignalLock.update-8"), b"another").unwrap();
        fs::write(dir.join("SignalLock-1.0.0-x86_64.AppImage"), b"the app").unwrap();
        fs::write(dir.join(".SignalLock.update-"), b"x").unwrap();
        fs::write(dir.join(".SignalLock.update-4z"), b"x").unwrap();
        fs::write(
            dir.join(".SignalLock-update-5"),
            b"macOS name, not ours here",
        )
        .unwrap();
        fs::create_dir(dir.join(".SignalLock.update-77")).unwrap(); // a folder, not a file
        symlink(
            outside.join("real.AppImage"),
            dir.join(".SignalLock.update-9"),
        )
        .unwrap();

        assert_eq!(remove_matching(&dir, LINUX_COPY, me), 2);

        assert_eq!(
            names(&dir),
            vec![
                ".SignalLock-update-5",
                ".SignalLock.update-",
                ".SignalLock.update-4z",
                ".SignalLock.update-77",
                ".SignalLock.update-9",
                "SignalLock-1.0.0-x86_64.AppImage",
            ]
        );
        assert_eq!(
            fs::read(outside.join("real.AppImage")).unwrap(),
            b"user data"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        assert_eq!(
            remove_matching(Path::new("/nonexistent-signallock-dir"), MACOS_STAGING, 0),
            0
        );
    }
}

#[cfg(test)]
mod startup_tests {
    use super::instance::Outcome;
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::time::Instant;

    #[test]
    fn a_new_process_waits_for_the_guard_of_the_old_one() {
        // The old process still holds the guard for two attempts, then lets go.
        let calls = Cell::new(0_u32);
        let outcome = acquire_with_retry(5, Duration::from_millis(1), || {
            calls.set(calls.get() + 1);
            if calls.get() <= 2 {
                Outcome::AlreadyRunning
            } else {
                Outcome::Unavailable("stand-in for the guard".to_owned())
            }
        });
        assert!(matches!(outcome, Outcome::Unavailable(_)));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn the_retry_gives_up_after_the_budget_and_does_not_retry_other_outcomes() {
        let calls = Cell::new(0_u32);
        let outcome = acquire_with_retry(5, Duration::from_millis(1), || {
            calls.set(calls.get() + 1);
            Outcome::AlreadyRunning
        });
        assert!(matches!(outcome, Outcome::AlreadyRunning));
        assert_eq!(calls.get(), 6, "one attempt plus five retries");

        let calls = Cell::new(0_u32);
        let started = Instant::now();
        let outcome = acquire_with_retry(5, Duration::from_secs(10), || {
            calls.set(calls.get() + 1);
            Outcome::Unavailable("no lock file".to_owned())
        });
        assert!(matches!(outcome, Outcome::Unavailable(_)));
        assert_eq!(calls.get(), 1);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_production_retry_budget_is_five_times_200_ms() {
        assert_eq!(GUARD_RETRIES, 5);
        assert_eq!(GUARD_RETRY_DELAY, Duration::from_millis(200));
    }

    #[test]
    fn startup_cleanup_empties_the_updates_directory_and_keeps_it() {
        let dir =
            std::env::temp_dir().join(format!("signallock-startup-cleanup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("install-1-0")).unwrap();
        fs::write(dir.join("SignalLock-1.1.0-setup.exe.part"), b"half").unwrap();
        fs::write(
            dir.join("install-1-0").join("SignalLock-1.1.0-setup.exe"),
            b"x",
        )
        .unwrap();

        assert_eq!(update_service::cleanup_downloads_in(&dir), 0);

        assert!(dir.is_dir(), "the directory itself stays");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_removes_a_symlink_without_touching_its_target() {
        let base =
            std::env::temp_dir().join(format!("signallock-startup-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let updates = base.join("updates");
        let outside = base.join("outside");
        fs::create_dir_all(&updates).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, updates.join("link-to-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("keep.txt"), updates.join("link-to-file")).unwrap();

        assert_eq!(update_service::cleanup_downloads_in(&updates), 0);

        assert_eq!(
            fs::read_dir(&updates).unwrap().count(),
            0,
            "the links are gone"
        );
        assert_eq!(
            fs::read(outside.join("keep.txt")).unwrap(),
            b"keep",
            "their target is not"
        );
        let _ = fs::remove_dir_all(&base);
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::instance::{Outcome, acquire};
    use super::restore_guard;
    use std::cell::RefCell;

    #[test]
    fn a_released_guard_is_taken_again_unless_another_instance_has_it() {
        let dir = std::env::temp_dir().join(format!("signallock-restore-{}", std::process::id()));
        let slot = RefCell::new(None);

        // Free: the app takes its guard back.
        assert!(restore_guard(&slot, || acquire(&dir)));
        assert!(slot.borrow().is_some());
        // Already holding it: nothing to do, no second lock attempt.
        assert!(restore_guard(&slot, || panic!("must not acquire twice")));

        // The guard is given up and a second instance (the half-started new
        // version) takes it: the old process must report that it cannot.
        slot.borrow_mut().take();
        let other = acquire(&dir);
        assert!(matches!(other, Outcome::First(_)));
        assert!(!restore_guard(&slot, || acquire(&dir)));
        assert!(slot.borrow().is_none());

        // When the other one is gone it works again.
        drop(other);
        assert!(restore_guard(&slot, || acquire(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
