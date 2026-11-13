//! `AppState`: the single owner of the store, the proximity monitor, the BLE
//! scanner, the lock requester and autostart.
//!
//! The flow mirrors `macos/Sources/SignalLock/AppState.swift` (and
//! `windows/AppState.cs`). It lives on the main thread, which is the Slint
//! event loop thread, and is deliberately not `Send`: background work (the
//! BLE task, the 1 Hz tick, the blocking lock call, the session-unlock source)
//! posts events to it with `slint::invoke_from_event_loop`, and the closure
//! reaches the instance through [`with_current`]. Nothing else mutates state.
//!
//! The collaborators are trait objects (`BleScanner`, [`LockRequester`],
//! `Autostart`) so that the tests can drive the whole state machine with fakes
//! and an injected clock; the `*_at` methods take the time explicitly, the
//! plain ones read `Instant::now()` (ADR-014: only `AppState` reads the clock,
//! the core never does).
//!
//! In-app update (ADR-015): `AppState` also owns the [`UpdateState`]. The four
//! commands (`check_for_updates`, `install_update`, `open_release_page`,
//! `dismiss_update`) run network and install work on a blocking thread through
//! [`UpdateJobs`]; the result comes back as an [`UpdateEvent`] on the main
//! thread. No update object exists until the user clicks *Check for Updates…*.
//!
//! Deviations from the references, all deliberate:
//!
//! - While the picker is open (discovery scan) a monitoring start does not
//!   swap the scanner away from discovery; `stop_discovery` switches to the
//!   monitoring scan when the picker closes. The monitor is fed from
//!   discovery events meanwhile, as in the references.
//! - A failed "start at login" change keeps the real autostart state instead
//!   of writing `false`.
//! - Picker wording and the Bluetooth warnings are platform-neutral or
//!   per-platform (`tray.rs`, `device_picker.slint`) instead of the Mac-only
//!   or PC-only sentences of the references; the tray tooltip exists on
//!   Windows and Linux only (as in the references); "Last Auto-Lock" is
//!   always 24-hour `HH:MM:SS` where the Swift reference follows the system
//!   locale.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use signallock_core::{AppSettings, Decision, ProximityMonitor, Store, TrustedDevice};

use crate::ble::{BleEvent, BleScanner, BluetoothAvailability, DeviceId, display_name};
use crate::lock_service::LockMethod;
use crate::logging;
use crate::platform::{Autostart, PlatformError, SessionEvent};
use crate::update::release::RELEASE_PAGE_PREFIX;
use crate::update::{
    CheckOutcome, DownloadProgress, Release, UpdateChecker, UpdateError, VerifiedPackage,
};
use crate::update_service::{
    HEAVY_COMMAND_TIMEOUT, InstallError, InstallOutcome, LIGHT_COMMAND_TIMEOUT, UpdateInstaller,
};

/// Discovered devices unseen for this long drop out of the picker list
/// (reference behaviour).
pub const DISCOVERY_STALE_AFTER: Duration = Duration::from_secs(15);

/// Worst case of the install step plus the relaunch, from the helper-command
/// timeouts of `update_service.rs`. The longest path is macOS: `ditto`,
/// `codesign --verify` and `codesign -dv` (heavy), `plutil` (light) while
/// installing, then `open -n` (light) to relaunch. Windows and Linux run no
/// helper that long (the installer and the new AppImage are only spawned;
/// Linux opens a page with one light `xdg-open`).
pub const INSTALL_WORST_CASE: Duration =
    Duration::from_secs(3 * HEAVY_COMMAND_TIMEOUT.as_secs() + 2 * LIGHT_COMMAND_TIMEOUT.as_secs());

/// How far above [`INSTALL_WORST_CASE`] a Quit still waits.
const QUIT_DEFER_MARGIN: Duration = Duration::from_secs(30);

/// The longest a Quit waits for a running install step or relaunch (see
/// [`AppState::quit`]): the worst case of the helper commands plus a margin.
/// Each command is bounded by its own timeout; this outer bound only has to
/// catch a path that has none, so it must stay above the sum, or the app
/// would exit while a slow but healthy install still runs and leave a
/// half-written staging folder beside the app.
pub const QUIT_DEFER_LIMIT: Duration =
    Duration::from_secs(INSTALL_WORST_CASE.as_secs() + QUIT_DEFER_MARGIN.as_secs());

/// Asks for the screen to be locked. The production implementation runs
/// `LockService::lock_now` on a blocking thread and reports back through
/// [`AppState::on_lock_result`]; tests count the requests.
pub trait LockRequester {
    fn request_lock(&self);
}

/// A window or process-level action the state asks the UI layer to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiCommand {
    OpenSettings,
    OpenPicker,
    /// Show the update window (it follows [`UpdateState`]).
    OpenUpdate,
    /// The update is installed and the new version is about to be started:
    /// drop the single-instance guard first so the new process can take it.
    ReleaseInstanceGuard,
    /// The restart failed after the guard was released: take it again.
    RestoreInstanceGuard,
    Quit,
}

/// A device seen during a discovery scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub id: DeviceId,
    /// Already resolved: the "Unknown Device" placeholder for unnamed ones.
    pub name: String,
    pub rssi: i16,
    pub last_seen: Instant,
}

/// Everything the tray and the windows render, copied out of [`AppState`].
#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub availability: BluetoothAvailability,
    pub monitoring: bool,
    /// The away delay elapsed (`isAway` in the references).
    pub away: bool,
    pub last_triggered_lock_at: Option<SystemTime>,
    pub last_seen: Option<Instant>,
    /// Smoothed RSSI as last reported by the monitor, in dBm.
    pub rssi: Option<i32>,
    pub trusted_device: Option<TrustedDevice>,
    pub settings: AppSettings,
    /// Sorted strongest first; refreshed once per tick while discovering.
    pub discovered: Vec<DiscoveredDevice>,
    pub update: UpdateState,
}

// ---------------------------------------------------------------------------
// In-app update types
// ---------------------------------------------------------------------------

/// Where the in-app update stands (ADR-015). Owned by [`AppState`]; the tray
/// and the update window render it from [`AppSnapshot::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    /// A newer release exists. `notes` is plain text, never markup.
    Available {
        version: String,
        notes: String,
    },
    Downloading {
        progress: DownloadProgress,
    },
    /// The last byte arrived; the checksum is being compared.
    Verifying,
    /// The package is verified and on disk. Waiting for the user to confirm
    /// that SignalLock may quit and restart (declining deletes the file).
    ConfirmInstall {
        version: String,
    },
    Installing,
    /// Installed; the new version is being started and this process quits.
    RestartPending,
    /// Updating in place is not possible here; `url` is the release page.
    ManualRequired {
        url: String,
    },
    Failed {
        message: String,
    },
}

impl UpdateState {
    /// A job is running: the menu item is disabled and a second command is
    /// ignored.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::Downloading { .. }
                | Self::Verifying
                | Self::Installing
                | Self::RestartPending
        )
    }

    /// States that wait for the user: the update window is brought up when
    /// one is entered, even if the user closed it while a job was running.
    fn needs_attention(&self) -> bool {
        matches!(
            self,
            Self::UpToDate
                | Self::Available { .. }
                | Self::ConfirmInstall { .. }
                | Self::ManualRequired { .. }
                | Self::Failed { .. }
        )
    }
}

/// What a blocking update job reports back to the main thread.
#[derive(Debug)]
pub enum UpdateEvent {
    Checked(Result<CheckOutcome, UpdateError>),
    Progress(DownloadProgress),
    Downloaded(Result<VerifiedPackage, UpdateError>),
    Installed(Result<InstallOutcome, InstallError>),
    Relaunched(Result<(), InstallError>),
    /// `generation` is that of the update state when the page was requested;
    /// a result for an earlier state is dropped.
    PageOpened {
        generation: u64,
        result: Result<(), InstallError>,
    },
}

/// A blocking piece of update work. It may call the `emit` function any number
/// of times; each event reaches [`AppState::on_update_event`] on the main
/// thread, in order.
pub type UpdateJob = Box<dyn FnOnce(&dyn Fn(UpdateEvent)) + Send>;

/// Runs [`UpdateJob`]s off the main thread. `main` implements it on the tokio
/// blocking pool; tests run the jobs by hand.
pub trait UpdateJobs {
    fn spawn(&self, job: UpdateJob);
}

/// Opens a release page in the browser.
pub type PageOpener = dyn Fn(&str) -> Result<(), InstallError> + Send + Sync;

/// Everything `AppState` needs to run an update, injected so the tests use
/// fakes and no real network or installer is ever reached from them.
pub struct UpdateDeps {
    /// Creates the checker. Called **only** by [`AppState::check_for_updates`]:
    /// no network object exists before the user's click (ADR-015).
    pub checker_factory: Box<dyn Fn() -> Box<dyn UpdateChecker>>,
    /// The one installer of the process. A fresh one per click would defeat the
    /// concurrent-install lock and the pending `relaunch`.
    pub installer: Arc<dyn UpdateInstaller + Send + Sync>,
    /// Opens a release page in the browser (`update_service::open_release_page`).
    pub open_page: Arc<PageOpener>,
    pub jobs: Box<dyn UpdateJobs>,
}

/// Mutable update bookkeeping, besides the state itself.
struct UpdateInner {
    state: UpdateState,
    /// Created by the last check; also downloads, so one click is one checker.
    checker: Option<Arc<Mutex<Box<dyn UpdateChecker>>>>,
    release: Option<Release>,
    package: Option<VerifiedPackage>,
    /// Monitoring was on when the install began and is switched back on if the
    /// install does not end in a restart.
    resume_monitoring: bool,
    /// Counts every change of `state`. A result that left for a state earlier
    /// than the current one (a slow `xdg-open`) must not rewrite it.
    generation: u64,
}

/// Limits how often download progress reaches the main thread: once per
/// percent when the size is known, once per 256 KiB when it is not.
#[derive(Debug, Default)]
struct ProgressThrottle {
    last_percent: Option<u64>,
    last_bytes: u64,
}

impl ProgressThrottle {
    const UNKNOWN_SIZE_STEP: u64 = 256 * 1024;

    fn admit(&mut self, progress: DownloadProgress) -> bool {
        match progress.total.filter(|total| *total > 0) {
            Some(total) => {
                let percent = progress.downloaded.min(total) * 100 / total;
                if self.last_percent == Some(percent) {
                    return false;
                }
                self.last_percent = Some(percent);
                true
            }
            None => {
                if progress.downloaded.saturating_sub(self.last_bytes) < Self::UNKNOWN_SIZE_STEP {
                    return false;
                }
                self.last_bytes = progress.downloaded;
                true
            }
        }
    }
}

type Observer = Rc<dyn Fn(&AppSnapshot)>;
type UiHandler = Rc<dyn Fn(UiCommand)>;

struct Inner {
    store: Store,
    monitor: ProximityMonitor,
    scanner: Box<dyn BleScanner>,
    locker: Box<dyn LockRequester>,
    autostart: Box<dyn Autostart>,

    settings: AppSettings,
    trusted: Option<TrustedDevice>,
    /// `trusted.identifier` parsed; `None` when there is no trusted device or
    /// its identifier is not a valid `DeviceId`.
    trusted_id: Option<DeviceId>,
    availability: BluetoothAvailability,
    monitoring: bool,
    away: bool,
    last_triggered_lock_at: Option<SystemTime>,
    last_seen: Option<Instant>,
    rssi: Option<i32>,

    discovering: bool,
    devices: HashMap<DeviceId, DiscoveredDevice>,
    discovered: Vec<DiscoveredDevice>,
}

impl Inner {
    fn is_trusted(&self, id: &DeviceId) -> bool {
        self.trusted_id.as_ref() == Some(id)
    }

    /// Copies what the monitor knows into the published fields. Called after
    /// a sample and after a re-arm, like `didUpdateSmoothedRSSI`.
    fn sync_reading(&mut self) {
        self.rssi = self.monitor.smoothed_rssi();
        self.last_seen = self.monitor.last_seen();
    }

    fn persist_settings(&mut self, next: AppSettings) {
        if let Err(e) = self.store.save_settings(&next) {
            // The store already logged the failure; the in-memory value stays
            // so the running app keeps behaving as the user set it.
            tracing::warn!(target: logging::APP, "settings were not saved: {e}");
        }
        self.monitor.update_settings(next);
        self.settings = next;
    }

    fn refresh_discovery(&mut self, now: Instant) {
        self.devices
            .retain(|_, d| now.saturating_duration_since(d.last_seen) <= DISCOVERY_STALE_AFTER);
        let mut list: Vec<DiscoveredDevice> = self.devices.values().cloned().collect();
        // Strongest first; name and id break ties so the list does not jitter.
        list.sort_by(|a, b| {
            b.rssi
                .cmp(&a.rssi)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        self.discovered = list;
    }
}

/// See the module documentation.
pub struct AppState {
    inner: RefCell<Inner>,
    update: RefCell<UpdateInner>,
    update_deps: UpdateDeps,
    /// Set while a Quit waits for an install or its relaunch: the moment the
    /// wait ends at the latest (see [`AppState::quit`]).
    quit_deadline: Cell<Option<Instant>>,
    observers: RefCell<Vec<Observer>>,
    ui_handler: RefCell<Option<UiHandler>>,
}

impl AppState {
    /// Loads the persisted settings (clamped into their allowed ranges) and
    /// the trusted device from `store`. Nothing is started; call
    /// [`AppState::start`] once the UI is wired.
    pub fn new(
        store: Store,
        scanner: Box<dyn BleScanner>,
        locker: Box<dyn LockRequester>,
        autostart: Box<dyn Autostart>,
        update_deps: UpdateDeps,
    ) -> Rc<Self> {
        let settings = store.load_settings().clamp();
        let trusted = store.load_trusted_device();
        let trusted_id = trusted.as_ref().and_then(|t| match DeviceId::from_str(&t.identifier) {
            Ok(id) => Some(id),
            Err(_) => {
                tracing::error!(
                    target: logging::APP,
                    "Trusted device identifier '{}' is not a valid device id; monitoring cannot start",
                    t.identifier
                );
                None
            }
        });
        let monitor = ProximityMonitor::new(settings);
        Rc::new(Self {
            inner: RefCell::new(Inner {
                store,
                monitor,
                scanner,
                locker,
                autostart,
                settings,
                trusted,
                trusted_id,
                availability: BluetoothAvailability::Unknown,
                monitoring: false,
                away: false,
                last_triggered_lock_at: None,
                last_seen: None,
                rssi: None,
                discovering: false,
                devices: HashMap::new(),
                discovered: Vec::new(),
            }),
            update: RefCell::new(UpdateInner {
                state: UpdateState::Idle,
                checker: None,
                release: None,
                package: None,
                resume_monitoring: false,
                generation: 0,
            }),
            update_deps,
            quit_deadline: Cell::new(None),
            observers: RefCell::new(Vec::new()),
            ui_handler: RefCell::new(None),
        })
    }

    /// Startup behaviour of ADR-008: with a trusted device, monitoring starts
    /// on its own. The persisted `monitoring_enabled` flag is not consulted.
    pub fn start(&self) {
        let has_trusted = self.inner.borrow().trusted.is_some();
        if has_trusted {
            self.start_monitoring();
        } else {
            self.notify();
        }
    }

    // ----- Observation -----

    pub fn snapshot(&self) -> AppSnapshot {
        let inner = self.inner.borrow();
        AppSnapshot {
            availability: inner.availability,
            monitoring: inner.monitoring,
            away: inner.away,
            last_triggered_lock_at: inner.last_triggered_lock_at,
            last_seen: inner.last_seen,
            rssi: inner.rssi,
            trusted_device: inner.trusted.clone(),
            settings: inner.settings,
            discovered: inner.discovered.clone(),
            update: self.update.borrow().state.clone(),
        }
    }

    /// Registers a view. It is called after every state change, on the main
    /// thread, with no internal borrow held, so it may call back into the
    /// state.
    pub fn subscribe(&self, observer: impl Fn(&AppSnapshot) + 'static) {
        self.observers.borrow_mut().push(Rc::new(observer));
    }

    /// Where `open_settings`, `open_picker` and `quit` go. Set by `main`.
    pub fn set_ui_handler(&self, handler: impl Fn(UiCommand) + 'static) {
        *self.ui_handler.borrow_mut() = Some(Rc::new(handler));
    }

    fn notify(&self) {
        let snapshot = self.snapshot();
        let observers: Vec<Observer> = self.observers.borrow().clone();
        for observer in observers {
            observer(&snapshot);
        }
    }

    fn send_ui(&self, command: UiCommand) {
        let handler = self.ui_handler.borrow().clone();
        match handler {
            Some(handler) => handler(command),
            None => tracing::warn!(target: logging::APP, "no UI handler for {command:?}"),
        }
    }

    // ----- Commands -----

    pub fn open_settings(&self) {
        self.send_ui(UiCommand::OpenSettings);
    }

    pub fn open_picker(&self) {
        self.send_ui(UiCommand::OpenPicker);
    }

    /// Ends the app. While an update is being installed (or started) the exit
    /// waits for that step to finish: leaving mid-install would drop the
    /// blocking thread and could leave a half-copied file next to the app
    /// (macOS staging, Linux `.SignalLock.update-<pid>`), outside the updates
    /// cache that the startup cleanup empties. An install that ends in a
    /// restart quits anyway; one that fails or is manual quits right after.
    ///
    /// The wait is bounded by [`QUIT_DEFER_LIMIT`] (checked once per tick, see
    /// [`AppState::on_tick_at`]): when it runs out the update is marked
    /// `Failed`, monitoring is put back as it was and the app quits.
    pub fn quit(&self) {
        self.quit_at(Instant::now());
    }

    pub fn quit_at(&self, now: Instant) {
        let installing = matches!(
            self.update.borrow().state,
            UpdateState::Installing | UpdateState::RestartPending
        );
        if installing {
            if self.quit_deadline.get().is_none() {
                tracing::info!(
                    target: logging::APP,
                    "Quit deferred until the update install step ends (at most {} s)",
                    QUIT_DEFER_LIMIT.as_secs()
                );
                // A second click does not push the deadline out.
                self.quit_deadline.set(Some(now + QUIT_DEFER_LIMIT));
            }
            return;
        }
        self.send_ui(UiCommand::Quit);
    }

    /// Applies `change` to the settings, clamps, saves and hands the result to
    /// the monitor. An unchanged result writes nothing and notifies nobody, so
    /// a view that echoes a value back cannot loop; a view whose control may
    /// now disagree with the state re-reads [`AppState::snapshot`].
    pub fn update_settings(&self, change: impl FnOnce(&mut AppSettings)) {
        let changed = {
            let mut inner = self.inner.borrow_mut();
            let mut next = inner.settings;
            change(&mut next);
            let next = next.clamp();
            let changed = next != inner.settings;
            if changed {
                inner.persist_settings(next);
            }
            changed
        };
        if changed {
            self.notify();
        }
    }

    pub fn set_start_at_login(&self, enabled: bool) {
        let changed = {
            let mut inner = self.inner.borrow_mut();
            let actual = match inner.autostart.set_enabled(enabled) {
                Ok(()) => enabled,
                Err(e) => {
                    tracing::warn!(target: logging::APP, "start at login was not changed: {e}");
                    inner.autostart.is_enabled()
                }
            };
            let changed = inner.settings.start_at_login != actual;
            if changed {
                let mut next = inner.settings;
                next.start_at_login = actual;
                inner.persist_settings(next);
            }
            changed
        };
        if changed {
            self.notify();
        }
    }

    /// Makes `id` the trusted device (the name is taken from the discovery
    /// list). A running monitor restarts on the new device; a paused one stays
    /// paused (as in `AppState.swift` and `AppState.cs`).
    pub fn select_device(&self, id: &DeviceId) {
        {
            let mut inner = self.inner.borrow_mut();
            let name = inner
                .devices
                .get(id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| display_name(None).to_owned());
            let trusted = TrustedDevice {
                identifier: id.to_string(),
                name,
            };
            if let Err(e) = inner.store.save_trusted_device(Some(&trusted)) {
                tracing::warn!(target: logging::APP, "trusted device was not saved: {e}");
            }
            tracing::info!(
                target: logging::APP,
                "Trusted device selected: {} (id={})",
                trusted.name,
                trusted.identifier
            );
            inner.trusted = Some(trusted);
            inner.trusted_id = Some(id.clone());
        }
        if self.inner.borrow().monitoring {
            self.start_monitoring();
        } else {
            self.notify();
        }
    }

    pub fn clear_trusted_device(&self) {
        let was_monitoring = {
            let mut inner = self.inner.borrow_mut();
            if let Err(e) = inner.store.save_trusted_device(None) {
                tracing::warn!(target: logging::APP, "trusted device was not cleared: {e}");
            }
            inner.trusted = None;
            inner.trusted_id = None;
            inner.monitoring
        };
        if was_monitoring {
            self.stop_monitoring();
        } else {
            self.notify();
        }
    }

    pub fn start_monitoring(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            let Some(trusted) = inner.trusted.clone() else {
                drop(inner);
                self.notify();
                return;
            };
            let Some(id) = inner.trusted_id.clone() else {
                tracing::error!(
                    target: logging::APP,
                    "Trusted device identifier '{}' is not a valid device id",
                    trusted.identifier
                );
                drop(inner);
                self.notify();
                return;
            };
            inner.monitor.arm();
            if !inner.discovering {
                inner.scanner.start_monitoring(id);
            }
            inner.monitoring = true;
            inner.away = false;
            inner.sync_reading();
            if !inner.settings.monitoring_enabled {
                let mut next = inner.settings;
                next.monitoring_enabled = true;
                inner.persist_settings(next);
            }
            tracing::info!(
                target: logging::APP,
                "Monitoring started (device={}, threshold={} dBm, awayDelay={}s)",
                trusted.name,
                inner.settings.rssi_threshold,
                inner.settings.away_delay_seconds
            );
        }
        self.notify();
    }

    pub fn stop_monitoring(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            if !inner.discovering {
                inner.scanner.stop();
            }
            inner.monitoring = false;
            inner.away = false;
            inner.rssi = None;
            if inner.settings.monitoring_enabled {
                let mut next = inner.settings;
                next.monitoring_enabled = false;
                inner.persist_settings(next);
            }
            tracing::info!(target: logging::APP, "Monitoring stopped");
        }
        self.notify();
    }

    /// The "Test Lock" menu item.
    pub fn test_lock(&self) {
        self.inner.borrow().locker.request_lock();
    }

    // ----- In-app update (ADR-015) -----

    /// *Check for Updates…* (tray) and *Try Again* (update window): the only
    /// way into the update flow, and the only place a checker is created.
    ///
    /// While a job runs the command is ignored. While a release or a
    /// confirmation is on screen it only brings the window up again.
    pub fn check_for_updates(&self) {
        let state = self.update.borrow().state.clone();
        if state.is_busy() {
            tracing::debug!(target: logging::APP, "update check ignored: a job is running");
            return;
        }
        if matches!(
            state,
            UpdateState::Available { .. } | UpdateState::ConfirmInstall { .. }
        ) {
            self.send_ui(UiCommand::OpenUpdate);
            return;
        }

        tracing::info!(target: logging::APP, "Update check requested by the user");
        let checker = Arc::new(Mutex::new((self.update_deps.checker_factory)()));
        let stale_package = {
            let mut update = self.update.borrow_mut();
            update.checker = Some(checker.clone());
            update.release = None;
            update.resume_monitoring = false;
            update.package.take()
        };
        // Normally none (Failed and the other states hold no package); a
        // verified file must never be dropped without deleting it.
        if let Some(package) = stale_package {
            discard_package(&package);
        }
        self.set_update_state(UpdateState::Checking);
        self.send_ui(UiCommand::OpenUpdate);
        self.update_deps.jobs.spawn(Box::new(move |emit| {
            let result = lock_checker(&checker).check();
            emit(UpdateEvent::Checked(result));
        }));
    }

    /// *Download & Install* while a release is shown, then the confirmation of
    /// the restart once the package is verified. Anything else is ignored.
    pub fn install_update(&self) {
        let state = self.update.borrow().state.clone();
        match state {
            UpdateState::Available { .. } => self.start_download(),
            UpdateState::ConfirmInstall { .. } => self.start_install(),
            other => {
                tracing::debug!(target: logging::APP, "install ignored in state {other:?}");
            }
        }
    }

    /// *Later*, *Cancel*, *Close* and the window's close button. Leaves the
    /// result on screen behind; a verified package that was not confirmed is
    /// deleted. Ignored while a job runs.
    pub fn dismiss_update(&self) {
        let package = {
            let mut update = self.update.borrow_mut();
            if update.state.is_busy() || update.state == UpdateState::Idle {
                return;
            }
            update.state = UpdateState::Idle;
            update.generation += 1;
            update.release = None;
            update.checker = None;
            update.package.take()
        };
        if let Some(package) = package {
            discard_package(&package);
        }
        self.notify();
    }

    /// *Open Download Page*: the release page of the release on screen (the
    /// page `ManualRequired` carries, else the one of the release found, else
    /// the releases page). The browser opens only because the user asked.
    pub fn open_release_page(&self) {
        let url = {
            let update = self.update.borrow();
            match &update.state {
                UpdateState::ManualRequired { url } => url.clone(),
                _ => update.release.as_ref().map_or_else(
                    || format!("{RELEASE_PAGE_PREFIX}latest"),
                    |release| release.html_url.clone(),
                ),
            }
        };
        let generation = self.update.borrow().generation;
        let open = self.update_deps.open_page.clone();
        self.update_deps.jobs.spawn(Box::new(move |emit| {
            emit(UpdateEvent::PageOpened {
                generation,
                result: open(&url),
            });
        }));
    }

    fn start_download(&self) {
        let (checker, release) = {
            let update = self.update.borrow();
            (update.checker.clone(), update.release.clone())
        };
        let (Some(checker), Some(release)) = (checker, release) else {
            self.set_update_state(UpdateState::Failed {
                message: "The update is no longer available. Check again.".to_owned(),
            });
            return;
        };
        tracing::info!(target: logging::APP, "Downloading update {}", release.version);
        self.set_update_state(UpdateState::Downloading {
            progress: DownloadProgress {
                downloaded: 0,
                total: release.asset.size,
            },
        });
        self.update_deps.jobs.spawn(Box::new(move |emit| {
            let mut throttle = ProgressThrottle::default();
            let result = lock_checker(&checker).download(&release, &mut |progress| {
                if throttle.admit(progress) {
                    emit(UpdateEvent::Progress(progress));
                }
            });
            emit(UpdateEvent::Downloaded(result));
        }));
    }

    /// The user confirmed the restart. Monitoring stops first (it is switched
    /// back on if the install does not end in a restart), then the installer
    /// runs on a blocking thread.
    fn start_install(&self) {
        let package = self.update.borrow_mut().package.take();
        let Some(package) = package else {
            self.set_update_state(UpdateState::Failed {
                message: "The downloaded update is gone. Check again.".to_owned(),
            });
            return;
        };
        let was_monitoring = self.inner.borrow().monitoring;
        self.update.borrow_mut().resume_monitoring = was_monitoring;
        tracing::info!(target: logging::APP, "Installing update {}", package.version());
        self.set_update_state(UpdateState::Installing);
        if was_monitoring {
            self.stop_monitoring();
        }
        let installer = self.update_deps.installer.clone();
        self.update_deps.jobs.spawn(Box::new(move |emit| {
            emit(UpdateEvent::Installed(installer.install(package)));
        }));
    }

    /// A job reported back. Events that do not fit the current state (a late
    /// one) are dropped.
    pub fn on_update_event(&self, event: UpdateEvent) {
        self.handle_update_event(event);
        // A Quit that waited for the install step goes out as soon as the step
        // has ended without a restart (which quits by itself).
        if self.quit_deadline.get().is_some() && !self.update.borrow().state.is_busy() {
            self.quit_deadline.set(None);
            self.send_ui(UiCommand::Quit);
        }
    }

    fn handle_update_event(&self, event: UpdateEvent) {
        let state = self.update.borrow().state.clone();
        match event {
            UpdateEvent::Checked(result) if state == UpdateState::Checking => match result {
                Ok(CheckOutcome::UpToDate) => {
                    tracing::info!(target: logging::APP, "SignalLock is up to date");
                    self.set_update_state(UpdateState::UpToDate);
                }
                Ok(CheckOutcome::Available(release)) => {
                    tracing::info!(target: logging::APP, "Update {} is available", release.version);
                    let next = UpdateState::Available {
                        version: release.version.to_string(),
                        notes: release.notes.clone(),
                    };
                    self.update.borrow_mut().release = Some(release);
                    self.set_update_state(next);
                }
                Err(e) => self.fail_update(&e),
            },
            UpdateEvent::Progress(progress)
                if matches!(
                    state,
                    UpdateState::Downloading { .. } | UpdateState::Verifying
                ) =>
            {
                let complete = progress
                    .total
                    .is_some_and(|total| total > 0 && progress.downloaded >= total);
                let next = if complete {
                    UpdateState::Verifying
                } else {
                    UpdateState::Downloading { progress }
                };
                if next != state {
                    self.set_update_state(next);
                }
            }
            UpdateEvent::Downloaded(result)
                if matches!(
                    state,
                    UpdateState::Downloading { .. } | UpdateState::Verifying
                ) =>
            {
                match result {
                    Ok(package) => {
                        tracing::info!(target: logging::APP, "Update {} downloaded and verified", package.version());
                        let next = UpdateState::ConfirmInstall {
                            version: package.version().to_string(),
                        };
                        self.update.borrow_mut().package = Some(package);
                        self.set_update_state(next);
                    }
                    Err(e) => self.fail_update(&e),
                }
            }
            UpdateEvent::Installed(result) if state == UpdateState::Installing => match result {
                Ok(InstallOutcome::RestartPending) => {
                    self.set_update_state(UpdateState::RestartPending);
                    // Order (ADR-015): installed, guard released, new version
                    // started, then quit.
                    self.send_ui(UiCommand::ReleaseInstanceGuard);
                    let installer = self.update_deps.installer.clone();
                    self.update_deps.jobs.spawn(Box::new(move |emit| {
                        emit(UpdateEvent::Relaunched(installer.relaunch()));
                    }));
                }
                Ok(InstallOutcome::ManualRequired(url)) => {
                    tracing::info!(target: logging::APP, "Update cannot be installed in place");
                    self.resume_monitoring_after_update();
                    self.set_update_state(UpdateState::ManualRequired { url });
                }
                Err(e) => {
                    self.resume_monitoring_after_update();
                    self.fail_update(&e);
                }
            },
            UpdateEvent::Relaunched(result) if state == UpdateState::RestartPending => match result
            {
                Ok(()) => {
                    self.quit_deadline.set(None);
                    self.send_ui(UiCommand::Quit);
                }
                Err(e) => {
                    self.send_ui(UiCommand::RestoreInstanceGuard);
                    self.resume_monitoring_after_update();
                    self.fail_update(&e);
                }
            },
            UpdateEvent::PageOpened { generation, result } => {
                if let Err(e) = result {
                    tracing::warn!(target: logging::APP, "release page not opened: {e}");
                    // Shown only if the state is still the one the request
                    // left from, and never over a state that owns a package
                    // or a running job (a late result of an earlier session).
                    let current = self.update.borrow().generation == generation;
                    let holds_work = state.is_busy()
                        || matches!(
                            state,
                            UpdateState::Available { .. } | UpdateState::ConfirmInstall { .. }
                        );
                    if current && !holds_work {
                        self.set_update_state(UpdateState::Failed {
                            message: e.to_string(),
                        });
                    } else {
                        tracing::debug!(target: logging::APP, "page-open failure ignored (state moved on)");
                    }
                }
            }
            late => {
                tracing::debug!(target: logging::APP, "update event dropped in state {state:?}: {late:?}");
            }
        }
    }

    /// The bound of a deferred Quit. The step it waited for is still running on
    /// its blocking thread and cannot be stopped; its late result is dropped
    /// (the state is no longer `Installing`/`RestartPending`).
    fn expire_deferred_quit(&self, now: Instant) {
        let Some(deadline) = self.quit_deadline.get() else {
            return;
        };
        if now < deadline {
            return;
        }
        self.quit_deadline.set(None);
        let state = self.update.borrow().state.clone();
        if state.is_busy() {
            tracing::warn!(
                target: logging::APP,
                "Update step still running after {} s ({state:?}); marking the update failed and quitting",
                QUIT_DEFER_LIMIT.as_secs()
            );
            // If the app were to stay up, protection is back as it was.
            self.resume_monitoring_after_update();
            self.set_update_state(UpdateState::Failed {
                message:
                    "The update did not finish in time and was stopped. SignalLock is quitting."
                        .to_owned(),
            });
        }
        self.send_ui(UiCommand::Quit);
    }

    fn fail_update(&self, error: &dyn std::fmt::Display) {
        tracing::warn!(target: logging::APP, "Update failed: {error}");
        self.set_update_state(UpdateState::Failed {
            message: error.to_string(),
        });
    }

    fn resume_monitoring_after_update(&self) {
        let resume = std::mem::take(&mut self.update.borrow_mut().resume_monitoring);
        if resume {
            self.start_monitoring();
        }
    }

    /// Stores `state`, tells the views and, for a state that waits for the
    /// user, asks for the update window.
    fn set_update_state(&self, state: UpdateState) {
        let attention = state.needs_attention();
        {
            let mut update = self.update.borrow_mut();
            update.state = state;
            update.generation += 1;
        }
        self.notify();
        if attention {
            self.send_ui(UiCommand::OpenUpdate);
        }
    }

    // ----- Discovery (the picker) -----

    pub fn start_discovery(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.discovering = true;
            inner.devices.clear();
            inner.discovered.clear();
            inner.scanner.start_discovery();
        }
        self.notify();
    }

    /// Ends discovery and resumes monitoring where it was. Idempotent: the
    /// picker calls it from several close paths.
    pub fn stop_discovery(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            if !inner.discovering {
                return;
            }
            inner.discovering = false;
            inner.devices.clear();
            inner.discovered.clear();
            if !inner.monitoring {
                inner.scanner.stop();
            } else if let Some(id) = inner.trusted_id.clone() {
                inner.scanner.start_monitoring(id);
            }
        }
        self.notify();
    }

    // ----- Events from the background -----

    pub fn on_ble_event(&self, event: BleEvent) {
        self.on_ble_event_at(event, Instant::now());
    }

    pub fn on_ble_event_at(&self, event: BleEvent, now: Instant) {
        {
            let mut inner = self.inner.borrow_mut();
            match event {
                BleEvent::AdapterState(availability) => {
                    inner.availability = availability;
                    if availability != BluetoothAvailability::PoweredOn && inner.monitoring {
                        inner.rssi = None;
                    }
                }
                BleEvent::Discovered { id, name, rssi } => {
                    if inner.monitoring && inner.is_trusted(&id) {
                        inner.monitor.on_sample(i32::from(rssi), now);
                        inner.sync_reading();
                    }
                    if inner.discovering {
                        let device = DiscoveredDevice {
                            id: id.clone(),
                            name: display_name(name.as_deref()).to_owned(),
                            rssi,
                            last_seen: now,
                        };
                        inner.devices.insert(id, device);
                    }
                }
                BleEvent::Sample { id, rssi } => {
                    if inner.monitoring && inner.is_trusted(&id) {
                        inner.monitor.on_sample(i32::from(rssi), now);
                        inner.sync_reading();
                    }
                }
            }
        }
        self.notify();
    }

    /// One evaluation step of the proximity monitor (1 Hz) and, while the
    /// picker is open, the discovery list refresh.
    pub fn on_tick(&self) {
        self.on_tick_at(Instant::now());
    }

    pub fn on_tick_at(&self, now: Instant) {
        self.expire_deferred_quit(now);
        let lock = {
            let mut inner = self.inner.borrow_mut();
            let mut lock = false;
            if inner.monitoring {
                let decision = inner.monitor.tick(now);
                inner.away = inner.monitor.is_away();
                if decision == Some(Decision::Lock) {
                    inner.last_triggered_lock_at = Some(SystemTime::now());
                    lock = true;
                }
            }
            if inner.discovering {
                inner.refresh_discovery(now);
            }
            lock
        };
        if lock {
            tracing::info!(target: logging::APP, "Proximity decided to lock the screen");
            self.inner.borrow().locker.request_lock();
        }
        self.notify();
    }

    /// The outcome of a lock request. `LockService` has already logged it;
    /// this is the hook for the post-lock follow-up, which the references do
    /// not have (the monitor's one-lock-per-episode guarantee is enough).
    pub fn on_lock_result(&self, result: Result<LockMethod, PlatformError>) {
        if let Err(e) = result {
            tracing::error!(target: logging::APP, "Lock request failed: {e}");
        }
    }

    /// The user's session was locked or unlocked. On unlock, a running
    /// monitor starts a fresh away-detection cycle (otherwise the ADR-010
    /// presence gate never resets after a lock).
    pub fn on_session_event(&self, event: SessionEvent) {
        if event != SessionEvent::Unlocked {
            return;
        }
        {
            let mut inner = self.inner.borrow_mut();
            if !inner.monitoring {
                return;
            }
            tracing::info!(target: logging::APP, "Session unlocked, rearming proximity monitor");
            inner.monitor.arm();
            inner.away = false;
            inner.sync_reading();
        }
        self.notify();
    }
}

fn lock_checker(
    checker: &Mutex<Box<dyn UpdateChecker>>,
) -> std::sync::MutexGuard<'_, Box<dyn UpdateChecker>> {
    checker.lock().unwrap_or_else(|e| e.into_inner())
}

/// Deletes a verified package the user did not confirm.
fn discard_package(package: &VerifiedPackage) {
    match std::fs::remove_file(package.path()) {
        Ok(()) => tracing::info!(target: logging::APP, "Downloaded update deleted (not confirmed)"),
        Err(e) => tracing::warn!(target: logging::APP, "Downloaded update not deleted: {e}"),
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<AppState>>> = const { RefCell::new(None) };
}

/// Makes `app` reachable from `slint::invoke_from_event_loop` closures. Call
/// on the main thread.
pub fn install(app: Rc<AppState>) {
    CURRENT.with(|current| *current.borrow_mut() = Some(app));
}

/// Runs `f` with the installed `AppState`. `None` off the main thread or
/// before [`install`].
pub fn with_current<R>(f: impl FnOnce(&Rc<AppState>) -> R) -> Option<R> {
    let app = CURRENT.with(|current| current.borrow().clone())?;
    Some(f(&app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_util::TestDir;
    use crate::update::ReleaseAsset;
    use semver::Version;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    const TRUSTED_ID: &str = "11111111-2222-3333-4444-555555555555";
    const OTHER_ID: &str = "AAAAAAAA-2222-3333-4444-555555555555";

    type CallLog = Rc<RefCell<Vec<String>>>;

    struct FakeScanner {
        calls: CallLog,
    }

    impl BleScanner for FakeScanner {
        fn start_discovery(&self) {
            self.calls.borrow_mut().push("discovery".into());
        }
        fn start_monitoring(&self, id: DeviceId) {
            self.calls.borrow_mut().push(format!("monitoring:{id}"));
        }
        fn stop(&self) {
            self.calls.borrow_mut().push("stop".into());
        }
        fn take_events(&mut self) -> Option<mpsc::Receiver<BleEvent>> {
            None
        }
    }

    struct FakeLocker {
        requests: Rc<Cell<u32>>,
    }

    impl LockRequester for FakeLocker {
        fn request_lock(&self) {
            self.requests.set(self.requests.get() + 1);
        }
    }

    struct FakeAutostart {
        enabled: Rc<Cell<bool>>,
        fail: bool,
    }

    impl Autostart for FakeAutostart {
        fn is_enabled(&self) -> bool {
            self.enabled.get()
        }
        fn set_enabled(&self, on: bool) -> crate::platform::Result<()> {
            if self.fail {
                return Err(PlatformError::new("denied"));
            }
            self.enabled.set(on);
            Ok(())
        }
    }

    // ----- Update fakes: no real network, installer or browser -----

    /// What the fake checker answers. Cloned on every call.
    struct Script {
        check: Result<CheckOutcome, UpdateError>,
        progress: Vec<DownloadProgress>,
        download: Result<VerifiedPackage, UpdateError>,
    }

    struct FakeChecker {
        script: Arc<Mutex<Script>>,
    }

    impl UpdateChecker for FakeChecker {
        fn check(&self) -> Result<CheckOutcome, UpdateError> {
            self.script.lock().unwrap().check.clone()
        }

        fn download(
            &self,
            _release: &Release,
            progress: &mut dyn FnMut(DownloadProgress),
        ) -> Result<VerifiedPackage, UpdateError> {
            let script = self.script.lock().unwrap();
            for step in &script.progress {
                progress(*step);
            }
            script.download.clone()
        }
    }

    struct FakeInstaller {
        calls: Mutex<Vec<String>>,
        install: Mutex<Result<InstallOutcome, InstallError>>,
        relaunch: Mutex<Result<(), InstallError>>,
    }

    impl UpdateInstaller for FakeInstaller {
        fn install(&self, pkg: VerifiedPackage) -> Result<InstallOutcome, InstallError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("install:{}", pkg.version()));
            self.install.lock().unwrap().clone()
        }

        fn relaunch(&self) -> Result<(), InstallError> {
            self.calls.lock().unwrap().push("relaunch".to_owned());
            self.relaunch.lock().unwrap().clone()
        }
    }

    type JobQueue = Rc<RefCell<VecDeque<UpdateJob>>>;

    struct QueuedJobs(JobQueue);

    impl UpdateJobs for QueuedJobs {
        fn spawn(&self, job: UpdateJob) {
            self.0.borrow_mut().push_back(job);
        }
    }

    const NEW_VERSION: &str = "1.1.0";
    const ASSET: &str = "SignalLock-1.1.0-x86_64.AppImage";

    fn release() -> Release {
        Release {
            version: Version::parse(NEW_VERSION).unwrap(),
            // Markup on purpose: it must reach the view as the raw text.
            notes: "Fixes.\n<b>not html</b> **not markdown**".to_owned(),
            html_url: format!("{RELEASE_PAGE_PREFIX}tag/v{NEW_VERSION}"),
            asset: ReleaseAsset {
                name: ASSET.to_owned(),
                url: format!(
                    "https://github.com/muhammetsafak/signallock/releases/download/v{NEW_VERSION}/{ASSET}"
                ),
                size: Some(1000),
            },
            checksum_asset: ReleaseAsset {
                name: format!("{ASSET}.sha256"),
                url: format!(
                    "https://github.com/muhammetsafak/signallock/releases/download/v{NEW_VERSION}/{ASSET}.sha256"
                ),
                size: Some(100),
            },
        }
    }

    /// The fakes behind an [`UpdateDeps`], kept so a test can script them and
    /// look at what happened.
    struct Rig {
        script: Arc<Mutex<Script>>,
        installer: Arc<FakeInstaller>,
        opened: Arc<Mutex<Vec<String>>>,
        /// What opening a page reports (a failure is a slow or missing browser).
        open_result: Arc<Mutex<Result<(), InstallError>>>,
        checkers_created: Rc<Cell<u32>>,
        jobs: JobQueue,
    }

    fn rig() -> (UpdateDeps, Rig) {
        let script = Arc::new(Mutex::new(Script {
            check: Ok(CheckOutcome::UpToDate),
            progress: Vec::new(),
            download: Err(UpdateError::Network("no download scripted".to_owned())),
        }));
        let installer = Arc::new(FakeInstaller {
            calls: Mutex::new(Vec::new()),
            install: Mutex::new(Ok(InstallOutcome::RestartPending)),
            relaunch: Mutex::new(Ok(())),
        });
        let opened = Arc::new(Mutex::new(Vec::new()));
        let open_result = Arc::new(Mutex::new(Ok(())));
        let checkers_created = Rc::new(Cell::new(0));
        let jobs: JobQueue = Rc::default();
        let deps = UpdateDeps {
            checker_factory: Box::new({
                let script = script.clone();
                let created = checkers_created.clone();
                move || {
                    created.set(created.get() + 1);
                    Box::new(FakeChecker {
                        script: script.clone(),
                    })
                }
            }),
            installer: installer.clone(),
            open_page: Arc::new({
                let opened = opened.clone();
                let open_result = open_result.clone();
                move |url| {
                    opened.lock().unwrap().push(url.to_owned());
                    open_result.lock().unwrap().clone()
                }
            }),
            jobs: Box::new(QueuedJobs(jobs.clone())),
        };
        (
            deps,
            Rig {
                script,
                installer,
                opened,
                open_result,
                checkers_created,
                jobs,
            },
        )
    }

    struct Harness {
        _dir: TestDir,
        downloads: TestDir,
        rig: Rig,
        ui: Rc<RefCell<Vec<UiCommand>>>,
        store: Store,
        app: Rc<AppState>,
        scanner_calls: CallLog,
        lock_requests: Rc<Cell<u32>>,
        autostart: Rc<Cell<bool>>,
    }

    fn trusted() -> TrustedDevice {
        TrustedDevice {
            identifier: TRUSTED_ID.to_owned(),
            name: "My Phone".to_owned(),
        }
    }

    fn id(s: &str) -> DeviceId {
        DeviceId::from_str(s).expect("valid id")
    }

    fn harness(with_trusted: bool, autostart_fails: bool) -> Harness {
        let dir = TestDir::new("app-state");
        let store = Store::new(dir.path().to_path_buf());
        if with_trusted {
            store.save_trusted_device(Some(&trusted())).unwrap();
        }
        let scanner_calls: CallLog = Rc::default();
        let lock_requests = Rc::new(Cell::new(0));
        let autostart = Rc::new(Cell::new(false));
        let (deps, rig) = rig();
        let app = AppState::new(
            store.clone(),
            Box::new(FakeScanner {
                calls: scanner_calls.clone(),
            }),
            Box::new(FakeLocker {
                requests: lock_requests.clone(),
            }),
            Box::new(FakeAutostart {
                enabled: autostart.clone(),
                fail: autostart_fails,
            }),
            deps,
        );
        let ui: Rc<RefCell<Vec<UiCommand>>> = Rc::default();
        app.set_ui_handler({
            let ui = ui.clone();
            move |command| ui.borrow_mut().push(command)
        });
        Harness {
            _dir: dir,
            downloads: TestDir::new("app-state-downloads"),
            rig,
            ui,
            store,
            app,
            scanner_calls,
            lock_requests,
            autostart,
        }
    }

    fn sample(rssi: i16) -> BleEvent {
        BleEvent::Sample {
            id: id(TRUSTED_ID),
            rssi,
        }
    }

    #[test]
    fn monitoring_starts_at_launch_when_a_trusted_device_exists() {
        let h = harness(true, false);
        h.app.start();

        let snap = h.app.snapshot();
        assert!(snap.monitoring);
        assert_eq!(snap.trusted_device, Some(trusted()));
        assert_eq!(
            *h.scanner_calls.borrow(),
            vec![format!("monitoring:{}", id(TRUSTED_ID))]
        );
        assert!(h.store.load_settings().monitoring_enabled);
    }

    #[test]
    fn monitoring_does_not_start_without_a_trusted_device() {
        let h = harness(false, false);
        h.app.start();
        h.app.start_monitoring();

        assert!(!h.app.snapshot().monitoring);
        assert!(h.scanner_calls.borrow().is_empty());
    }

    #[test]
    fn an_invalid_trusted_identifier_does_not_start_monitoring() {
        let dir = TestDir::new("app-state-bad-id");
        let store = Store::new(dir.path().to_path_buf());
        store
            .save_trusted_device(Some(&TrustedDevice {
                identifier: "not-an-id".into(),
                name: "Broken".into(),
            }))
            .unwrap();
        let calls: CallLog = Rc::default();
        let app = AppState::new(
            store,
            Box::new(FakeScanner {
                calls: calls.clone(),
            }),
            Box::new(FakeLocker {
                requests: Rc::default(),
            }),
            Box::new(FakeAutostart {
                enabled: Rc::default(),
                fail: false,
            }),
            rig().0,
        );
        app.start();

        assert!(!app.snapshot().monitoring);
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_lock_decision_calls_the_lock_requester_exactly_once() {
        let h = harness(true, false);
        h.app.update_settings(|s| s.away_delay_seconds = 5);
        h.app.start();

        let t0 = Instant::now();
        // Near sample confirms presence (ADR-010 gate)...
        h.app.on_ble_event_at(sample(-50), t0);
        h.app.on_tick_at(t0 + Duration::from_secs(1));
        assert_eq!(h.lock_requests.get(), 0);

        // ...then silence past the away delay decides to lock, once.
        for secs in 2..=20 {
            h.app.on_tick_at(t0 + Duration::from_secs(secs));
        }
        assert_eq!(h.lock_requests.get(), 1);
        let snap = h.app.snapshot();
        assert!(snap.away);
        assert!(snap.last_triggered_lock_at.is_some());
    }

    #[test]
    fn test_lock_requests_a_lock_without_touching_the_auto_lock_time() {
        let h = harness(true, false);
        h.app.test_lock();

        assert_eq!(h.lock_requests.get(), 1);
        assert!(h.app.snapshot().last_triggered_lock_at.is_none());
    }

    #[test]
    fn unlock_rearms_the_monitor_and_closes_the_presence_gate() {
        let h = harness(true, false);
        h.app.update_settings(|s| s.away_delay_seconds = 5);
        h.app.start();

        let t0 = Instant::now();
        h.app.on_ble_event_at(sample(-50), t0);
        for secs in 1..=10 {
            h.app.on_tick_at(t0 + Duration::from_secs(secs));
        }
        assert_eq!(h.lock_requests.get(), 1);

        h.app.on_session_event(SessionEvent::Unlocked);
        let snap = h.app.snapshot();
        assert!(!snap.away);
        assert_eq!(snap.rssi, None);
        assert_eq!(snap.last_seen, None);

        // The device never comes back near: no second lock (no re-lock loop).
        for secs in 11..=60 {
            h.app.on_tick_at(t0 + Duration::from_secs(secs));
        }
        assert_eq!(h.lock_requests.get(), 1);
    }

    #[test]
    fn unlock_is_ignored_while_monitoring_is_off() {
        let h = harness(false, false);
        h.app.on_session_event(SessionEvent::Unlocked);
        assert!(!h.app.snapshot().monitoring);
    }

    #[test]
    fn a_settings_update_is_clamped_and_written_to_the_store() {
        let h = harness(false, false);
        h.app.update_settings(|s| {
            s.rssi_threshold = -70;
            s.away_delay_seconds = 500;
        });

        let saved = h.store.load_settings();
        assert_eq!(saved.rssi_threshold, -70);
        assert_eq!(saved.away_delay_seconds, 120);
        assert_eq!(h.app.snapshot().settings, saved);
    }

    #[test]
    fn persisted_settings_are_clamped_on_load() {
        let dir = TestDir::new("app-state-clamp");
        let store = Store::new(dir.path().to_path_buf());
        store
            .save_settings(&AppSettings {
                away_delay_seconds: 0,
                ..AppSettings::default()
            })
            .unwrap();
        let app = AppState::new(
            store,
            Box::new(FakeScanner {
                calls: Rc::default(),
            }),
            Box::new(FakeLocker {
                requests: Rc::default(),
            }),
            Box::new(FakeAutostart {
                enabled: Rc::default(),
                fail: false,
            }),
            rig().0,
        );
        assert_eq!(app.snapshot().settings.away_delay_seconds, 5);
    }

    #[test]
    fn start_at_login_follows_autostart_and_keeps_the_real_state_on_failure() {
        let h = harness(false, false);
        h.app.set_start_at_login(true);
        assert!(h.autostart.get());
        assert!(h.store.load_settings().start_at_login);

        let failing = harness(false, true);
        failing.app.set_start_at_login(true);
        assert!(!failing.app.snapshot().settings.start_at_login);
        assert!(!failing.store.load_settings().start_at_login);
    }

    fn discover_trusted(h: &Harness) {
        h.app.start_discovery();
        h.app.on_ble_event_at(
            BleEvent::Discovered {
                id: id(TRUSTED_ID),
                name: Some("My Phone".into()),
                rssi: -60,
            },
            Instant::now(),
        );
    }

    #[test]
    fn selecting_a_device_saves_it_and_leaves_paused_monitoring_paused() {
        let h = harness(false, false);
        discover_trusted(&h);
        h.app.select_device(&id(TRUSTED_ID));

        let snap = h.app.snapshot();
        assert!(!snap.monitoring);
        assert_eq!(snap.trusted_device, Some(trusted()));
        assert_eq!(h.store.load_trusted_device(), Some(trusted()));
        assert!(!h.store.load_settings().monitoring_enabled);

        // Closing the picker stops the scanner: nothing is being monitored.
        h.app.stop_discovery();
        assert_eq!(
            *h.scanner_calls.borrow(),
            vec!["discovery".to_owned(), "stop".to_owned()]
        );
    }

    #[test]
    fn selecting_a_device_restarts_a_running_monitor_on_the_new_device() {
        let h = harness(true, false);
        h.app.start();
        h.app.on_ble_event_at(sample(-50), Instant::now());
        assert_eq!(h.app.snapshot().rssi, Some(-50));

        h.app.start_discovery();
        h.app.on_ble_event_at(
            BleEvent::Discovered {
                id: id(OTHER_ID),
                name: Some("Watch".into()),
                rssi: -60,
            },
            Instant::now(),
        );
        h.app.select_device(&id(OTHER_ID));

        let snap = h.app.snapshot();
        assert!(snap.monitoring);
        assert_eq!(
            snap.trusted_device.map(|d| d.name),
            Some("Watch".to_owned())
        );
        // The monitor was re-armed: the old device's reading is gone.
        assert_eq!(snap.rssi, None);
        // The picker still owns the scanner until it closes...
        assert_eq!(
            h.scanner_calls.borrow().last().map(String::as_str),
            Some("discovery")
        );
        // ...then the monitoring scan targets the new device.
        h.app.stop_discovery();
        assert_eq!(
            h.scanner_calls.borrow().last().cloned(),
            Some(format!("monitoring:{}", id(OTHER_ID)))
        );
    }

    #[test]
    fn forgetting_the_device_stops_monitoring_and_clears_the_store() {
        let h = harness(true, false);
        h.app.start();
        h.app.clear_trusted_device();

        let snap = h.app.snapshot();
        assert!(!snap.monitoring);
        assert!(snap.trusted_device.is_none());
        assert!(h.store.load_trusted_device().is_none());
        assert!(!h.store.load_settings().monitoring_enabled);
        assert_eq!(
            h.scanner_calls.borrow().last().map(String::as_str),
            Some("stop")
        );
    }

    #[test]
    fn discovery_lists_devices_strongest_first_and_drops_stale_ones() {
        let h = harness(false, false);
        h.app.start_discovery();
        let t0 = Instant::now();
        let seen = |id_str: &str, name: Option<&str>, rssi: i16| BleEvent::Discovered {
            id: id(id_str),
            name: name.map(str::to_owned),
            rssi,
        };
        h.app
            .on_ble_event_at(seen(TRUSTED_ID, Some("Far"), -90), t0);
        h.app
            .on_ble_event_at(seen(OTHER_ID, None, -40), t0 + Duration::from_secs(10));
        h.app.on_tick_at(t0 + Duration::from_secs(11));

        let names: Vec<_> = h
            .app
            .snapshot()
            .discovered
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["Unknown Device", "Far"]);

        // "Far" was last seen 16 s ago, the other one 6 s ago.
        h.app.on_tick_at(t0 + Duration::from_secs(16));
        let names: Vec<_> = h
            .app
            .snapshot()
            .discovered
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["Unknown Device"]);
    }

    #[test]
    fn stop_discovery_stops_the_scanner_when_not_monitoring_and_is_idempotent() {
        let h = harness(false, false);
        h.app.start_discovery();
        h.app.stop_discovery();
        h.app.stop_discovery();

        assert_eq!(
            *h.scanner_calls.borrow(),
            vec!["discovery".to_owned(), "stop".to_owned()]
        );
    }

    #[test]
    fn the_trusted_device_keeps_feeding_the_monitor_during_discovery() {
        let h = harness(true, false);
        h.app.start();
        h.app.start_discovery();
        h.app.on_ble_event_at(
            BleEvent::Discovered {
                id: id(TRUSTED_ID),
                name: None,
                rssi: -55,
            },
            Instant::now(),
        );
        assert_eq!(h.app.snapshot().rssi, Some(-55));
    }

    #[test]
    fn samples_of_other_devices_are_ignored() {
        let h = harness(true, false);
        h.app.start();
        h.app.on_ble_event_at(
            BleEvent::Sample {
                id: id(OTHER_ID),
                rssi: -30,
            },
            Instant::now(),
        );
        assert_eq!(h.app.snapshot().rssi, None);
    }

    #[test]
    fn bluetooth_going_away_clears_the_reading() {
        let h = harness(true, false);
        h.app.start();
        h.app.on_ble_event_at(sample(-50), Instant::now());
        assert_eq!(h.app.snapshot().rssi, Some(-50));

        h.app.on_ble_event_at(
            BleEvent::AdapterState(BluetoothAvailability::PoweredOff),
            Instant::now(),
        );
        let snap = h.app.snapshot();
        assert_eq!(snap.availability, BluetoothAvailability::PoweredOff);
        assert_eq!(snap.rssi, None);
    }

    #[test]
    fn observers_are_called_after_a_change_and_may_call_back() {
        let h = harness(false, false);
        let seen = Rc::new(Cell::new(0));
        let app = h.app.clone();
        let seen_in = seen.clone();
        h.app.subscribe(move |snap| {
            seen_in.set(seen_in.get() + 1);
            // Re-entrant, idempotent call (what a UI echo of a property does).
            app.update_settings(|s| s.rssi_threshold = snap.settings.rssi_threshold);
        });
        h.app.update_settings(|s| s.rssi_threshold = -60);
        // The change notifies; the idempotent echo changes nothing and does not.
        assert_eq!(seen.get(), 1);
    }

    #[test]
    fn commands_reach_the_ui_handler() {
        let h = harness(false, false);
        let got: Rc<RefCell<Vec<UiCommand>>> = Rc::default();
        let got_in = got.clone();
        h.app.set_ui_handler(move |c| got_in.borrow_mut().push(c));
        h.app.open_settings();
        h.app.open_picker();
        h.app.quit();
        assert_eq!(
            *got.borrow(),
            vec![
                UiCommand::OpenSettings,
                UiCommand::OpenPicker,
                UiCommand::Quit
            ]
        );
    }

    // ----- In-app update -----

    impl Harness {
        /// Runs the oldest queued update job and delivers its events, as the
        /// main loop would. `false` when no job was queued.
        fn step(&self) -> bool {
            let job = self.rig.jobs.borrow_mut().pop_front();
            let Some(job) = job else { return false };
            self.run_job(job);
            true
        }

        fn run_job(&self, job: UpdateJob) {
            let events = RefCell::new(Vec::new());
            job(&|event| events.borrow_mut().push(event));
            for event in events.into_inner() {
                self.app.on_update_event(event);
            }
        }

        /// Takes the oldest queued job without running it (a slow one).
        fn hold_job(&self) -> UpdateJob {
            self.rig
                .jobs
                .borrow_mut()
                .pop_front()
                .expect("a queued job")
        }

        fn quits(&self) -> usize {
            self.ui
                .borrow()
                .iter()
                .filter(|c| **c == UiCommand::Quit)
                .count()
        }

        fn queued_jobs(&self) -> usize {
            self.rig.jobs.borrow().len()
        }

        fn update(&self) -> UpdateState {
            self.app.snapshot().update
        }

        fn install_calls(&self) -> Vec<String> {
            self.rig.installer.calls.lock().unwrap().clone()
        }

        /// A verified package whose file really exists in a temp directory.
        fn script_download(&self, progress: Vec<DownloadProgress>) -> std::path::PathBuf {
            let path = self.downloads.path().join(ASSET);
            std::fs::write(&path, b"package bytes").unwrap();
            let package = VerifiedPackage::for_test(
                path.clone(),
                Version::parse(NEW_VERSION).unwrap(),
                ASSET,
            );
            let mut script = self.rig.script.lock().unwrap();
            script.progress = progress;
            script.download = Ok(package);
            path
        }

        fn script_release(&self) {
            self.rig.script.lock().unwrap().check = Ok(CheckOutcome::Available(release()));
        }

        /// Walks check -> download until the confirmation is on screen.
        fn reach_confirmation(&self) -> std::path::PathBuf {
            self.script_release();
            let path = self.script_download(vec![
                DownloadProgress {
                    downloaded: 250,
                    total: Some(1000),
                },
                DownloadProgress {
                    downloaded: 1000,
                    total: Some(1000),
                },
            ]);
            self.app.check_for_updates();
            self.step();
            self.app.install_update();
            self.step();
            assert_eq!(
                self.update(),
                UpdateState::ConfirmInstall {
                    version: NEW_VERSION.to_owned()
                }
            );
            path
        }
    }

    fn progress(downloaded: u64, total: Option<u64>) -> DownloadProgress {
        DownloadProgress { downloaded, total }
    }

    #[test]
    fn no_update_object_exists_until_the_user_clicks() {
        let h = harness(true, false);
        h.app.start();
        let t0 = Instant::now();
        h.app.on_ble_event_at(sample(-50), t0);
        for secs in 1..=30 {
            h.app.on_tick_at(t0 + Duration::from_secs(secs));
        }
        h.app.stop_monitoring();
        h.app.start_monitoring();
        h.app.open_settings();
        h.app.start_discovery();
        h.app.stop_discovery();
        // The commands that need a release do nothing without one.
        h.app.install_update();
        h.app.dismiss_update();

        assert!(h.app.snapshot().monitoring);
        assert_eq!(
            h.rig.checkers_created.get(),
            0,
            "startup and monitoring create no checker"
        );
        assert_eq!(h.queued_jobs(), 0);
        assert_eq!(h.update(), UpdateState::Idle);

        h.app.check_for_updates();
        assert_eq!(
            h.rig.checkers_created.get(),
            1,
            "the click creates exactly one"
        );
        assert_eq!(h.queued_jobs(), 1);
    }

    #[test]
    fn check_download_and_install_end_in_a_restart() {
        let h = harness(true, false);
        h.app.start();
        let seen: Rc<RefCell<Vec<UpdateState>>> = Rc::default();
        h.app.subscribe({
            let seen = seen.clone();
            move |snapshot| {
                let mut seen = seen.borrow_mut();
                if seen.last() != Some(&snapshot.update) {
                    seen.push(snapshot.update.clone());
                }
            }
        });
        h.script_release();
        h.script_download(vec![
            progress(250, Some(1000)),
            progress(251, Some(1000)),
            progress(1000, Some(1000)),
        ]);

        h.app.check_for_updates();
        assert_eq!(h.update(), UpdateState::Checking);
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::OpenUpdate));
        h.step();
        assert_eq!(
            h.update(),
            UpdateState::Available {
                version: NEW_VERSION.to_owned(),
                notes: "Fixes.\n<b>not html</b> **not markdown**".to_owned(),
            }
        );

        h.app.install_update();
        assert!(matches!(h.update(), UpdateState::Downloading { .. }));
        h.step();
        assert_eq!(
            h.update(),
            UpdateState::ConfirmInstall {
                version: NEW_VERSION.to_owned()
            }
        );
        // Monitoring keeps running through the check and the download, and the
        // installer has not been touched before the user confirms.
        assert!(h.app.snapshot().monitoring);
        assert!(h.install_calls().is_empty());

        h.app.install_update();
        assert_eq!(h.update(), UpdateState::Installing);
        assert!(
            !h.app.snapshot().monitoring,
            "monitoring stops before the install runs"
        );
        assert_eq!(
            h.scanner_calls.borrow().last().map(String::as_str),
            Some("stop")
        );
        assert!(
            !h.store.load_settings().monitoring_enabled,
            "and the setting is saved"
        );
        assert!(
            h.install_calls().is_empty(),
            "the install job has not run yet"
        );

        h.step();
        assert_eq!(h.update(), UpdateState::RestartPending);
        assert_eq!(h.install_calls(), vec![format!("install:{NEW_VERSION}")]);
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::ReleaseInstanceGuard));

        h.step();
        assert_eq!(
            h.install_calls(),
            vec![format!("install:{NEW_VERSION}"), "relaunch".to_owned()]
        );
        assert_eq!(
            *h.ui.borrow(),
            vec![
                UiCommand::OpenUpdate, // checking
                UiCommand::OpenUpdate, // release found
                UiCommand::OpenUpdate, // confirmation
                UiCommand::ReleaseInstanceGuard,
                UiCommand::Quit,
            ],
            "installed, guard released, new version started, then quit"
        );
        assert_eq!(
            h.rig.checkers_created.get(),
            1,
            "one click, one checker, also for the download"
        );
        assert_eq!(h.queued_jobs(), 0);

        let states = seen.borrow().clone();
        assert_eq!(
            states,
            vec![
                UpdateState::Checking,
                UpdateState::Available {
                    version: NEW_VERSION.to_owned(),
                    notes: "Fixes.\n<b>not html</b> **not markdown**".to_owned(),
                },
                UpdateState::Downloading {
                    progress: progress(0, Some(1000))
                },
                UpdateState::Downloading {
                    progress: progress(250, Some(1000))
                },
                UpdateState::Verifying,
                UpdateState::ConfirmInstall {
                    version: NEW_VERSION.to_owned()
                },
                UpdateState::Installing,
                UpdateState::RestartPending,
            ]
        );
    }

    #[test]
    fn declining_the_confirmation_deletes_the_download_and_installs_nothing() {
        let h = harness(true, false);
        h.app.start();
        let file = h.reach_confirmation();
        assert!(file.exists());

        h.app.dismiss_update();

        assert!(!file.exists(), "the downloaded file is deleted");
        assert_eq!(h.update(), UpdateState::Idle);
        assert!(h.install_calls().is_empty());
        assert_eq!(h.queued_jobs(), 0);
        assert!(h.app.snapshot().monitoring, "monitoring was never stopped");
        // A later click starts over, and cannot install the deleted file.
        h.app.install_update();
        assert_eq!(h.queued_jobs(), 0);
    }

    #[test]
    fn a_failed_install_keeps_the_app_running_and_resumes_monitoring() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        *h.rig.installer.install.lock().unwrap() =
            Err(InstallError::Verification("wrong Team ID".to_owned()));

        h.app.install_update();
        assert!(!h.app.snapshot().monitoring);
        h.step();

        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: "The update was rejected and not installed: wrong Team ID".to_owned()
            }
        );
        assert!(h.app.snapshot().monitoring, "monitoring is back on");
        assert_eq!(
            *h.scanner_calls.borrow(),
            vec![
                format!("monitoring:{}", id(TRUSTED_ID)),
                "stop".to_owned(),
                format!("monitoring:{}", id(TRUSTED_ID)),
            ]
        );
        assert_eq!(
            h.install_calls(),
            vec![format!("install:{NEW_VERSION}")],
            "no relaunch"
        );
        assert!(!h.ui.borrow().contains(&UiCommand::Quit));
        assert!(!h.ui.borrow().contains(&UiCommand::ReleaseInstanceGuard));
    }

    #[test]
    fn a_manual_outcome_offers_the_page_and_opens_it_only_on_request() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        let page = format!("{RELEASE_PAGE_PREFIX}tag/v{NEW_VERSION}");
        *h.rig.installer.install.lock().unwrap() = Ok(InstallOutcome::ManualRequired(page.clone()));

        h.app.install_update();
        h.step();

        assert_eq!(
            h.update(),
            UpdateState::ManualRequired { url: page.clone() }
        );
        assert!(h.app.snapshot().monitoring, "monitoring resumes");
        assert!(
            h.rig.opened.lock().unwrap().is_empty(),
            "nothing opens by itself"
        );
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::OpenUpdate));

        h.app.open_release_page();
        h.step();
        assert_eq!(*h.rig.opened.lock().unwrap(), vec![page]);
        assert!(matches!(h.update(), UpdateState::ManualRequired { .. }));
    }

    #[test]
    fn a_failed_check_shows_the_reason_and_try_again_runs_a_new_check() {
        let h = harness(false, false);
        h.rig.script.lock().unwrap().check = Err(UpdateError::Network("timed out".to_owned()));

        h.app.check_for_updates();
        h.step();
        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: "Could not reach GitHub: timed out".to_owned()
            }
        );
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::OpenUpdate));

        // Try Again is the same command.
        h.rig.script.lock().unwrap().check = Ok(CheckOutcome::UpToDate);
        h.app.check_for_updates();
        assert_eq!(h.update(), UpdateState::Checking);
        assert_eq!(h.rig.checkers_created.get(), 2);
        h.step();
        assert_eq!(h.update(), UpdateState::UpToDate);
    }

    #[test]
    fn a_failed_download_never_reaches_the_installer() {
        let h = harness(true, false);
        h.script_release();
        h.rig.script.lock().unwrap().download = Err(UpdateError::VerificationFailed(
            "checksum mismatch".to_owned(),
        ));

        h.app.check_for_updates();
        h.step();
        h.app.install_update();
        h.step();

        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: "The download failed verification: checksum mismatch".to_owned()
            }
        );
        // "Open Download Page" from here goes to the release that was found.
        h.app.open_release_page();
        h.step();
        assert_eq!(
            *h.rig.opened.lock().unwrap(),
            vec![format!("{RELEASE_PAGE_PREFIX}tag/v{NEW_VERSION}")]
        );
        h.app.install_update();
        assert!(h.install_calls().is_empty());
        assert_eq!(h.queued_jobs(), 0);
    }

    #[test]
    fn a_failed_relaunch_restores_the_guard_and_resumes_monitoring() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        *h.rig.installer.relaunch.lock().unwrap() =
            Err(InstallError::Command("open failed".to_owned()));

        h.app.install_update();
        h.step();
        h.step();

        assert!(matches!(h.update(), UpdateState::Failed { .. }));
        assert!(h.app.snapshot().monitoring);
        assert_eq!(
            h.ui.borrow()[3..],
            [
                UiCommand::ReleaseInstanceGuard,
                UiCommand::RestoreInstanceGuard,
                UiCommand::OpenUpdate
            ]
        );
        assert!(!h.ui.borrow().contains(&UiCommand::Quit));
    }

    #[test]
    fn a_running_job_ignores_further_commands() {
        let h = harness(false, false);
        h.app.check_for_updates();
        assert!(h.update().is_busy());

        h.app.check_for_updates();
        h.app.install_update();
        h.app.dismiss_update();

        assert_eq!(h.update(), UpdateState::Checking);
        assert_eq!(h.rig.checkers_created.get(), 1);
        assert_eq!(h.queued_jobs(), 1);
    }

    #[test]
    fn up_to_date_and_a_release_on_screen_behave_as_dialogs() {
        let h = harness(false, false);
        h.app.check_for_updates();
        h.step();
        assert_eq!(h.update(), UpdateState::UpToDate);
        h.app.install_update();
        assert_eq!(h.queued_jobs(), 0, "nothing to install");
        h.app.dismiss_update();
        assert_eq!(h.update(), UpdateState::Idle);

        // With a release on screen the menu item only raises the window.
        h.script_release();
        h.app.check_for_updates();
        h.step();
        let before = h.ui.borrow().len();
        h.app.check_for_updates();
        assert_eq!(h.rig.checkers_created.get(), 2);
        assert_eq!(h.queued_jobs(), 0);
        assert_eq!(h.ui.borrow().len(), before + 1);
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::OpenUpdate));
        // "Later" closes it.
        h.app.dismiss_update();
        assert_eq!(h.update(), UpdateState::Idle);
    }

    #[test]
    fn events_that_do_not_fit_the_state_are_dropped() {
        let h = harness(false, false);
        h.app
            .on_update_event(UpdateEvent::Progress(progress(5, Some(10))));
        h.app
            .on_update_event(UpdateEvent::Checked(Ok(CheckOutcome::UpToDate)));
        h.app
            .on_update_event(UpdateEvent::Installed(Ok(InstallOutcome::RestartPending)));
        h.app.on_update_event(UpdateEvent::Relaunched(Ok(())));
        assert_eq!(h.update(), UpdateState::Idle);
        assert!(h.ui.borrow().is_empty());
    }

    #[test]
    fn the_download_page_defaults_to_the_releases_page_and_a_failed_open_is_shown() {
        let h = harness(false, false);
        h.app.open_release_page();
        h.step();
        let opened = h.rig.opened.lock().unwrap().clone();
        assert_eq!(opened, vec![format!("{RELEASE_PAGE_PREFIX}latest")]);
        assert!(crate::update_service::is_allowed_release_url(&opened[0]));

        let generation = h.app.update.borrow().generation;
        h.app.on_update_event(UpdateEvent::PageOpened {
            generation,
            result: Err(InstallError::UrlNotAllowed),
        });
        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: "Only SignalLock release pages can be opened.".to_owned()
            }
        );
    }

    #[test]
    fn busy_states_are_exactly_the_ones_with_a_job() {
        let busy = [
            UpdateState::Checking,
            UpdateState::Downloading {
                progress: progress(0, None),
            },
            UpdateState::Verifying,
            UpdateState::Installing,
            UpdateState::RestartPending,
        ];
        assert!(busy.iter().all(UpdateState::is_busy));
        let idle = [
            UpdateState::Idle,
            UpdateState::UpToDate,
            UpdateState::Available {
                version: String::new(),
                notes: String::new(),
            },
            UpdateState::ConfirmInstall {
                version: String::new(),
            },
            UpdateState::ManualRequired { url: String::new() },
            UpdateState::Failed {
                message: String::new(),
            },
        ];
        assert!(idle.iter().all(|state| !state.is_busy()));
    }

    #[test]
    fn progress_is_throttled_to_one_event_per_percent() {
        let mut throttle = ProgressThrottle::default();
        let admitted: Vec<bool> = [0, 5, 9, 10, 10, 19, 20, 999, 1000]
            .into_iter()
            .map(|downloaded| throttle.admit(progress(downloaded, Some(1000))))
            .collect();
        // 0 %, 0 %, 0 %, 1 %, 1 %, 1 %, 2 %, 99 %, 100 %.
        assert_eq!(
            admitted,
            vec![true, false, false, true, false, false, true, true, true]
        );

        let mut unknown = ProgressThrottle::default();
        assert!(!unknown.admit(progress(1024, None)));
        assert!(unknown.admit(progress(256 * 1024, None)));
        assert!(!unknown.admit(progress(256 * 1024 + 10, None)));
        assert!(unknown.admit(progress(512 * 1024, None)));
    }

    #[test]
    fn a_late_page_open_failure_does_not_overwrite_the_confirmation() {
        let h = harness(true, false);
        h.app.start();
        // A failed check leaves "Open Download Page" on screen.
        h.rig.script.lock().unwrap().check = Err(UpdateError::Network("timed out".to_owned()));
        h.app.check_for_updates();
        h.step();
        *h.rig.open_result.lock().unwrap() = Err(InstallError::Command("xdg-open hung".to_owned()));
        h.app.open_release_page();
        let slow_open = h.hold_job();

        // Meanwhile the user tries again and reaches the confirmation.
        let file = h.reach_confirmation();
        assert!(file.exists());

        // The old result arrives now and is dropped.
        h.run_job(slow_open);
        assert_eq!(
            h.update(),
            UpdateState::ConfirmInstall {
                version: NEW_VERSION.to_owned()
            }
        );
        assert!(file.exists(), "the verified package is still there");
        // And the confirmation still works as before: declining deletes it.
        h.app.dismiss_update();
        assert!(!file.exists());
        assert_eq!(h.update(), UpdateState::Idle);
    }

    #[test]
    fn a_page_open_failure_of_an_earlier_session_is_ignored_even_in_a_failed_state() {
        let h = harness(false, false);
        h.rig.script.lock().unwrap().check = Err(UpdateError::Network("first".to_owned()));
        h.app.check_for_updates();
        h.step();
        *h.rig.open_result.lock().unwrap() = Err(InstallError::Command("old failure".to_owned()));
        h.app.open_release_page();
        let slow_open = h.hold_job();

        h.rig.script.lock().unwrap().check = Err(UpdateError::Network("second".to_owned()));
        h.app.check_for_updates();
        h.step();
        let message = "Could not reach GitHub: second".to_owned();
        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: message.clone()
            }
        );

        h.run_job(slow_open);
        assert_eq!(h.update(), UpdateState::Failed { message });

        // A failure of the request made in the state that is on screen shows.
        h.app.open_release_page();
        h.step();
        assert_eq!(
            h.update(),
            UpdateState::Failed {
                message: "Could not install the update: old failure".to_owned()
            }
        );
    }

    #[test]
    fn quit_waits_for_an_install_that_ends_in_a_restart() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        h.app.install_update();
        assert_eq!(h.update(), UpdateState::Installing);

        h.app.quit();
        assert_eq!(h.quits(), 0, "no exit while the install job is pending");
        assert!(h.install_calls().is_empty());

        h.step(); // the install runs to its end
        assert_eq!(h.install_calls(), vec![format!("install:{NEW_VERSION}")]);
        assert_eq!(h.quits(), 0, "still waiting for the relaunch");

        h.step(); // relaunch, then the one exit
        assert_eq!(h.quits(), 1);
        assert_eq!(
            h.install_calls().last().map(String::as_str),
            Some("relaunch")
        );
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::Quit));
    }

    #[test]
    fn quit_after_a_failed_install_exits_once_the_install_step_has_returned() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        *h.rig.installer.install.lock().unwrap() = Err(InstallError::Io("disk full".to_owned()));
        h.app.install_update();

        h.app.quit();
        assert_eq!(h.quits(), 0);
        h.step();

        assert_eq!(h.install_calls(), vec![format!("install:{NEW_VERSION}")]);
        assert_eq!(h.quits(), 1);
        assert!(matches!(h.update(), UpdateState::Failed { .. }));
        assert_eq!(h.queued_jobs(), 0, "no relaunch after a failed install");
    }

    #[test]
    fn quit_is_immediate_when_no_install_is_running() {
        let h = harness(false, false);
        h.app.quit();
        assert_eq!(h.quits(), 1);

        // A running check or download is not an install: nothing to protect.
        h.app.check_for_updates();
        assert_eq!(h.update(), UpdateState::Checking);
        h.app.quit();
        assert_eq!(h.quits(), 2);
    }

    #[test]
    fn a_deferred_quit_gives_up_after_the_limit_when_the_install_hangs() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        h.app.install_update();
        assert!(!h.app.snapshot().monitoring);

        let t0 = Instant::now();
        h.app.quit_at(t0);
        h.app.quit_at(t0 + Duration::from_secs(100)); // does not extend the wait
        h.app
            .on_tick_at(t0 + QUIT_DEFER_LIMIT - Duration::from_secs(1));
        assert_eq!(h.quits(), 0, "still inside the limit");
        assert_eq!(h.update(), UpdateState::Installing);

        h.app.on_tick_at(t0 + QUIT_DEFER_LIMIT);
        assert_eq!(h.quits(), 1);
        assert!(matches!(h.update(), UpdateState::Failed { .. }));
        assert!(h.app.snapshot().monitoring, "monitoring is back as it was");
        assert_eq!(h.ui.borrow().last(), Some(&UiCommand::Quit));

        // The hung install returns after all: its result is dropped, there is
        // no second exit and the state stays as it is.
        h.app
            .on_tick_at(t0 + QUIT_DEFER_LIMIT + Duration::from_secs(50));
        h.step();
        assert_eq!(h.quits(), 1);
        assert!(matches!(h.update(), UpdateState::Failed { .. }));
        assert_eq!(
            h.queued_jobs(),
            0,
            "a late install result starts no relaunch"
        );
    }

    #[test]
    fn the_quit_limit_is_above_the_worst_case_install_path() {
        // Helper commands of each platform's install + relaunch: (heavy, light)
        // counts. The macOS row is checked against the commands that
        // `install_at` and `relaunch` really run, in `platform/macos/update.rs`
        // (`each_helper_gets_its_bound`), so it cannot drift from the code;
        // Windows and Linux spawn only (Linux: one `xdg-open`).
        let paths = [
            ("macos", 3, 2), // ditto, codesign, codesign | plutil, open -n
            ("windows", 0, 0),
            ("linux", 0, 1), // xdg-open only
        ];
        let heavy = HEAVY_COMMAND_TIMEOUT.as_secs();
        let light = LIGHT_COMMAND_TIMEOUT.as_secs();
        let worst = paths
            .iter()
            .map(|(_, h, l)| h * heavy + l * light)
            .max()
            .unwrap();
        assert_eq!(worst, 3 * 120 + 2 * 15, "values of update_service.rs");
        assert_eq!(INSTALL_WORST_CASE.as_secs(), worst);
        assert_eq!(QUIT_DEFER_LIMIT.as_secs(), worst + 30);
        for (platform, h, l) in paths {
            assert!(
                QUIT_DEFER_LIMIT.as_secs() >= h * heavy + l * light + 30,
                "{platform}: the limit must clear its worst case by 30 s"
            );
        }
        assert!(QUIT_DEFER_LIMIT > INSTALL_WORST_CASE);
    }

    #[test]
    fn a_hung_relaunch_also_ends_in_failed_and_quit_after_the_limit() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        h.app.install_update();
        h.step(); // installed, guard released, relaunch queued
        assert_eq!(h.update(), UpdateState::RestartPending);
        let hung_relaunch = h.hold_job();

        let t0 = Instant::now();
        h.app.quit_at(t0);
        h.app.on_tick_at(t0 + Duration::from_secs(160));
        assert_eq!(h.quits(), 0, "the old 150 s bound would have fired here");
        h.app
            .on_tick_at(t0 + QUIT_DEFER_LIMIT + Duration::from_secs(1));
        assert_eq!(h.quits(), 1);
        assert!(matches!(h.update(), UpdateState::Failed { .. }));

        h.run_job(hung_relaunch);
        assert_eq!(h.quits(), 1, "the late relaunch result does not quit again");
    }

    #[test]
    fn a_deferred_quit_that_was_served_in_time_is_not_repeated_by_the_timer() {
        let h = harness(true, false);
        h.app.start();
        h.reach_confirmation();
        h.app.install_update();
        let t0 = Instant::now();
        h.app.quit_at(t0);
        h.step();
        h.step();
        assert_eq!(h.quits(), 1);

        h.app.on_tick_at(t0 + Duration::from_secs(400));
        assert_eq!(h.quits(), 1);
    }
}
