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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};

use signallock_core::{AppSettings, Decision, ProximityMonitor, Store, TrustedDevice};

use crate::ble::{BleEvent, BleScanner, BluetoothAvailability, DeviceId, display_name};
use crate::lock_service::LockMethod;
use crate::logging;
use crate::platform::{Autostart, PlatformError, SessionEvent};

/// Discovered devices unseen for this long drop out of the picker list
/// (reference behaviour).
pub const DISCOVERY_STALE_AFTER: Duration = Duration::from_secs(15);

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

    pub fn quit(&self) {
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
    use std::cell::Cell;
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

    struct Harness {
        _dir: TestDir,
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
        );
        Harness {
            _dir: dir,
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
}
