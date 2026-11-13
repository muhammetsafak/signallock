//! Tray icon and menu (`tray-icon`, `muda`).
//!
//! Items, order, labels and enable rules match
//! `macos/Sources/SignalLock/MenuBarController.swift` (Windows:
//! `windows/UI/TrayController.cs`), plus *Check for Updates…* (ADR-015) after
//! *Test Lock*. The menu is derived from an
//! [`AppSnapshot`] by the pure function [`build_menu_model`]; [`Tray`] applies
//! the model to the native menu, updating items in place when the layout is
//! unchanged and rebuilding it when an item appears or disappears (the
//! "Last Auto-Lock" and Bluetooth warning lines, Start versus Pause).
//!
//! Clicks are turned into [`MenuAction`]s by `main` and run through
//! [`dispatch`], i.e. they become `AppState` commands.
//!
//! On macOS the tray must be created on the main thread after the event loop
//! started; `main` schedules [`install`] with `invoke_from_event_loop`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Instant, SystemTime};

use muda::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::app_state::{AppSnapshot, AppState};
use crate::ble::BluetoothAvailability;
use crate::logging;

/// Tray tooltips are limited to 63 characters on Windows (`NotifyIcon.Text`);
/// the same cap is used everywhere for one look.
const TOOLTIP_MAX_CHARS: usize = 63;

/// What a clickable menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    StartMonitoring,
    PauseMonitoring,
    SelectDevice,
    Settings,
    TestLock,
    CheckForUpdates,
    Quit,
}

impl MenuAction {
    /// The `muda` item id.
    pub const fn id(self) -> &'static str {
        match self {
            Self::StartMonitoring => "start-monitoring",
            Self::PauseMonitoring => "pause-monitoring",
            Self::SelectDevice => "select-device",
            Self::Settings => "settings",
            Self::TestLock => "test-lock",
            Self::CheckForUpdates => "check-for-updates",
            Self::Quit => "quit",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        const ALL: [MenuAction; 7] = [
            MenuAction::StartMonitoring,
            MenuAction::PauseMonitoring,
            MenuAction::SelectDevice,
            MenuAction::Settings,
            MenuAction::TestLock,
            MenuAction::CheckForUpdates,
            MenuAction::Quit,
        ];
        ALL.into_iter().find(|action| action.id() == id)
    }
}

/// Runs a clicked menu item as the matching `AppState` command.
pub fn dispatch(action: MenuAction, app: &AppState) {
    match action {
        MenuAction::StartMonitoring => app.start_monitoring(),
        MenuAction::PauseMonitoring => app.stop_monitoring(),
        MenuAction::SelectDevice => app.open_picker(),
        MenuAction::Settings => app.open_settings(),
        MenuAction::TestLock => app.test_lock(),
        MenuAction::CheckForUpdates => app.check_for_updates(),
        MenuAction::Quit => app.quit(),
    }
}

/// Identity of a menu position. Two models with the same slots in the same
/// order can be applied to the native menu by editing items in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// A disabled, informational line; the name only tells them apart.
    Info(&'static str),
    Action(MenuAction),
    Separator,
}

/// One line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub slot: Slot,
    pub text: String,
    pub enabled: bool,
}

impl MenuEntry {
    fn info(name: &'static str, text: impl Into<String>) -> Self {
        Self {
            slot: Slot::Info(name),
            text: text.into(),
            enabled: false,
        }
    }

    fn action(action: MenuAction, text: &str, enabled: bool) -> Self {
        Self {
            slot: Slot::Action(action),
            text: text.to_owned(),
            enabled,
        }
    }

    fn separator() -> Self {
        Self {
            slot: Slot::Separator,
            text: String::new(),
            enabled: true,
        }
    }
}

const DASH: &str = "\u{2014}";

fn status_line(snapshot: &AppSnapshot) -> &'static str {
    if snapshot.monitoring {
        if snapshot.away {
            "Status: Away (waiting to lock)"
        } else {
            "Status: Monitoring"
        }
    } else {
        "Status: Paused"
    }
}

fn last_seen_text(last_seen: Option<Instant>, now: Instant) -> String {
    let Some(last_seen) = last_seen else {
        return DASH.to_owned();
    };
    let seconds = now.saturating_duration_since(last_seen).as_secs();
    if seconds < 5 {
        "just now".to_owned()
    } else if seconds < 60 {
        format!("{seconds} seconds ago")
    } else {
        format!("{} min ago", seconds / 60)
    }
}

/// Local wall-clock time of day, `HH:MM:SS`.
fn clock_text(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(time)
        .format("%H:%M:%S")
        .to_string()
}

#[cfg(target_os = "macos")]
mod wording {
    pub const UNAUTHORIZED: &str = "\u{26A0}\u{FE0E} Bluetooth permission denied";
    pub const UNSUPPORTED: &str = "\u{26A0}\u{FE0E} BLE not supported on this Mac";
    pub const POWERED_OFF: &str = "\u{26A0}\u{FE0E} Bluetooth is off";
    pub const UNKNOWN: &str = "\u{26A0}\u{FE0E} Bluetooth state unknown";
}

#[cfg(target_os = "windows")]
mod wording {
    pub const UNAUTHORIZED: &str = "\u{26A0} Bluetooth blocked by policy";
    pub const UNSUPPORTED: &str = "\u{26A0} BLE not supported on this PC";
    pub const POWERED_OFF: &str = "\u{26A0} Bluetooth is off";
    pub const UNKNOWN: &str = "\u{26A0} Bluetooth state unknown";
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod wording {
    pub const UNAUTHORIZED: &str = "\u{26A0} Bluetooth access denied";
    pub const UNSUPPORTED: &str = "\u{26A0} BLE not supported on this computer";
    pub const POWERED_OFF: &str = "\u{26A0} Bluetooth is off";
    pub const UNKNOWN: &str = "\u{26A0} Bluetooth state unknown";
}

/// `None` when Bluetooth is ready and there is nothing to warn about.
fn bluetooth_warning(availability: BluetoothAvailability) -> Option<&'static str> {
    match availability {
        BluetoothAvailability::PoweredOn => None,
        BluetoothAvailability::PoweredOff => Some(wording::POWERED_OFF),
        BluetoothAvailability::Unauthorized => Some(wording::UNAUTHORIZED),
        BluetoothAvailability::Unsupported => Some(wording::UNSUPPORTED),
        BluetoothAvailability::Unknown => Some(wording::UNKNOWN),
    }
}

/// The menu for `snapshot`, as of `now`. Pure, so it is unit-tested.
pub fn build_menu_model(snapshot: &AppSnapshot, now: Instant) -> Vec<MenuEntry> {
    let mut entries = Vec::with_capacity(16);

    entries.push(MenuEntry::info("header", "SignalLock"));
    entries.push(MenuEntry::info("status", status_line(snapshot)));

    let device = snapshot
        .trusted_device
        .as_ref()
        .map_or("(none)", |d| d.name.as_str());
    entries.push(MenuEntry::info(
        "trusted",
        format!("Trusted Device: {device}"),
    ));

    let signal = snapshot
        .rssi
        .map_or_else(|| DASH.to_owned(), |rssi| format!("{rssi} dBm"));
    entries.push(MenuEntry::info(
        "signal",
        format!("Current Signal: {signal}"),
    ));
    entries.push(MenuEntry::info(
        "last-seen",
        format!("Last Seen: {}", last_seen_text(snapshot.last_seen, now)),
    ));

    if let Some(at) = snapshot.last_triggered_lock_at {
        entries.push(MenuEntry::info(
            "last-lock",
            format!("Last Auto-Lock: {}", clock_text(at)),
        ));
    }
    if let Some(warning) = bluetooth_warning(snapshot.availability) {
        entries.push(MenuEntry::info("bluetooth", warning));
    }

    entries.push(MenuEntry::separator());

    if snapshot.monitoring {
        entries.push(MenuEntry::action(
            MenuAction::PauseMonitoring,
            "Pause Monitoring",
            true,
        ));
    } else {
        let can_start = snapshot.trusted_device.is_some()
            && snapshot.availability == BluetoothAvailability::PoweredOn;
        entries.push(MenuEntry::action(
            MenuAction::StartMonitoring,
            "Start Monitoring",
            can_start,
        ));
    }
    entries.push(MenuEntry::action(
        MenuAction::SelectDevice,
        "Select Trusted Device\u{2026}",
        true,
    ));
    entries.push(MenuEntry::action(
        MenuAction::Settings,
        "Settings\u{2026}",
        true,
    ));
    entries.push(MenuEntry::action(MenuAction::TestLock, "Test Lock", true));
    // Disabled while a check, download, verification or install runs (ADR-015).
    entries.push(MenuEntry::action(
        MenuAction::CheckForUpdates,
        "Check for Updates\u{2026}",
        !snapshot.update.is_busy(),
    ));
    entries.push(MenuEntry::separator());
    entries.push(MenuEntry::action(MenuAction::Quit, "Quit SignalLock", true));

    entries
}

/// Hover text (Windows and Linux; the macOS reference has none): `SignalLock — Monitoring · Device · -62 dBm`, cut to the
/// tooltip limit.
pub fn tooltip_text(snapshot: &AppSnapshot) -> String {
    let state = if snapshot.monitoring {
        "Monitoring"
    } else {
        "Paused"
    };
    let device = snapshot
        .trusted_device
        .as_ref()
        .map_or("(none)", |d| d.name.as_str());
    let signal = snapshot
        .rssi
        .map_or_else(|| DASH.to_owned(), |rssi| format!("{rssi} dBm"));
    format!("SignalLock \u{2014} {state} \u{00B7} {device} \u{00B7} {signal}")
        .chars()
        .take(TOOLTIP_MAX_CHARS)
        .collect()
}

enum NativeItem {
    Item(MenuItem),
    Separator(PredefinedMenuItem),
}

impl NativeItem {
    fn as_menu_item(&self) -> &dyn IsMenuItem {
        match self {
            Self::Item(item) => item,
            Self::Separator(separator) => separator,
        }
    }
}

/// The macOS key equivalents of the reference menu (Settings: Cmd+, ; Quit:
/// Cmd+Q). Windows and Linux menus show none.
#[cfg(target_os = "macos")]
fn accelerator_for(action: MenuAction) -> Option<muda::accelerator::Accelerator> {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    match action {
        MenuAction::Settings => Some(Accelerator::new(Modifiers::META, Code::Comma)),
        MenuAction::Quit => Some(Accelerator::new(Modifiers::META, Code::KeyQ)),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn accelerator_for(_action: MenuAction) -> Option<muda::accelerator::Accelerator> {
    None
}

/// The tray icon with its menu. Create with [`install`], on the main thread.
pub struct Tray {
    /// Owns the native icon; dropping it removes the icon from the tray.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    icon: TrayIcon,
    menu: Menu,
    entries: Vec<MenuEntry>,
    items: Vec<NativeItem>,
    #[cfg(not(target_os = "macos"))]
    tooltip: String,
}

impl Tray {
    fn new() -> Result<Self, String> {
        let menu = Menu::new();
        let builder = TrayIconBuilder::new().with_menu(Box::new(menu.clone()));
        // The macOS reference has no tooltip; Windows has one (`TrayController.cs`).
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_tooltip("SignalLock");
        #[cfg(target_os = "macos")]
        let builder =
            builder.with_icon_templated(load_icon(include_bytes!("../assets/tray-template.png"))?);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(load_icon(include_bytes!("../assets/tray-color.png"))?);

        let icon = builder.build().map_err(|e| e.to_string())?;
        Ok(Self {
            icon,
            menu,
            entries: Vec::new(),
            items: Vec::new(),
            #[cfg(not(target_os = "macos"))]
            tooltip: String::new(),
        })
    }

    /// Applies the current state to the icon, the tooltip and the menu.
    pub fn update(&mut self, snapshot: &AppSnapshot) {
        let model = build_menu_model(snapshot, Instant::now());

        let same_layout = model.len() == self.entries.len()
            && model
                .iter()
                .zip(&self.entries)
                .all(|(a, b)| a.slot == b.slot);
        if same_layout {
            for ((new, old), item) in model.iter().zip(&self.entries).zip(&self.items) {
                if let NativeItem::Item(item) = item {
                    if new.text != old.text {
                        item.set_text(&new.text);
                    }
                    if new.enabled != old.enabled {
                        item.set_enabled(new.enabled);
                    }
                }
            }
        } else {
            self.rebuild(&model);
        }
        self.entries = model;

        #[cfg(not(target_os = "macos"))]
        {
            let tooltip = tooltip_text(snapshot);
            if tooltip != self.tooltip {
                if let Err(e) = self.icon.set_tooltip(Some(&tooltip)) {
                    tracing::debug!(target: logging::APP, "tray tooltip not set: {e}");
                }
                self.tooltip = tooltip;
            }
        }
    }

    fn rebuild(&mut self, model: &[MenuEntry]) {
        for item in self.items.drain(..) {
            if let Err(e) = self.menu.remove(item.as_menu_item()) {
                tracing::warn!(target: logging::APP, "tray menu item not removed: {e}");
            }
        }
        for entry in model {
            let item = match entry.slot {
                Slot::Separator => NativeItem::Separator(PredefinedMenuItem::separator()),
                Slot::Info(_) => NativeItem::Item(MenuItem::new(&entry.text, entry.enabled, None)),
                Slot::Action(action) => NativeItem::Item(MenuItem::with_id(
                    action.id(),
                    &entry.text,
                    entry.enabled,
                    accelerator_for(action),
                )),
            };
            if let Err(e) = self.menu.append(item.as_menu_item()) {
                tracing::warn!(target: logging::APP, "tray menu item not added: {e}");
            }
            self.items.push(item);
        }
    }
}

fn load_icon(png: &[u8]) -> Result<Icon, String> {
    let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| format!("tray icon decode failed: {e}"))?
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).map_err(|e| format!("tray icon rejected: {e}"))
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Creates the tray icon and keeps it in sync with `app`. Main thread only. A
/// failure is logged and the app keeps running without an icon (on Linux this
/// is the documented behaviour of a desktop without a StatusNotifierItem
/// host).
pub fn install(app: &Rc<AppState>) {
    match Tray::new() {
        Ok(mut tray) => {
            tray.update(&app.snapshot());
            TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
            app.subscribe(|snapshot| {
                TRAY.with(|slot| {
                    if let Some(tray) = slot.borrow_mut().as_mut() {
                        tray.update(snapshot);
                    }
                });
            });
            tracing::info!(target: logging::APP, "Tray icon created");
        }
        Err(e) => tracing::error!(target: logging::APP, "Tray icon could not be created: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::UpdateState;
    use crate::update::DownloadProgress;
    use signallock_core::{AppSettings, TrustedDevice};
    use std::time::Duration;

    fn snapshot() -> AppSnapshot {
        AppSnapshot {
            availability: BluetoothAvailability::PoweredOn,
            monitoring: false,
            away: false,
            last_triggered_lock_at: None,
            last_seen: None,
            rssi: None,
            trusted_device: None,
            settings: AppSettings::default(),
            discovered: Vec::new(),
            update: UpdateState::Idle,
        }
    }

    fn device() -> TrustedDevice {
        TrustedDevice {
            identifier: "11111111-2222-3333-4444-555555555555".into(),
            name: "My Phone".into(),
        }
    }

    fn texts(model: &[MenuEntry]) -> Vec<&str> {
        model.iter().map(|e| e.text.as_str()).collect()
    }

    fn find(model: &[MenuEntry], action: MenuAction) -> &MenuEntry {
        model
            .iter()
            .find(|e| e.slot == Slot::Action(action))
            .expect("action present")
    }

    #[test]
    fn paused_menu_without_a_device_matches_the_reference_plus_the_update_item() {
        let model = build_menu_model(&snapshot(), Instant::now());
        assert_eq!(
            texts(&model),
            vec![
                "SignalLock",
                "Status: Paused",
                "Trusted Device: (none)",
                "Current Signal: \u{2014}",
                "Last Seen: \u{2014}",
                "",
                "Start Monitoring",
                "Select Trusted Device\u{2026}",
                "Settings\u{2026}",
                "Test Lock",
                "Check for Updates\u{2026}",
                "",
                "Quit SignalLock",
            ]
        );
        // Start needs a trusted device; the info lines are never clickable.
        assert!(!find(&model, MenuAction::StartMonitoring).enabled);
        assert!(model[..5].iter().all(|e| !e.enabled));
        assert!(find(&model, MenuAction::Quit).enabled);
    }

    #[test]
    fn start_is_enabled_only_with_a_device_and_ready_bluetooth() {
        let mut s = snapshot();
        s.trusted_device = Some(device());
        assert!(
            find(
                &build_menu_model(&s, Instant::now()),
                MenuAction::StartMonitoring
            )
            .enabled
        );

        s.availability = BluetoothAvailability::PoweredOff;
        assert!(
            !find(
                &build_menu_model(&s, Instant::now()),
                MenuAction::StartMonitoring
            )
            .enabled
        );
    }

    #[test]
    fn monitoring_menu_shows_pause_status_signal_and_last_seen() {
        let now = Instant::now();
        let mut s = snapshot();
        s.trusted_device = Some(device());
        s.monitoring = true;
        s.rssi = Some(-62);
        s.last_seen = Some(now);
        let model = build_menu_model(&s, now);

        let t = texts(&model);
        assert!(t.contains(&"Status: Monitoring"));
        assert!(t.contains(&"Trusted Device: My Phone"));
        assert!(t.contains(&"Current Signal: -62 dBm"));
        assert!(t.contains(&"Last Seen: just now"));
        assert!(t.contains(&"Pause Monitoring"));
        assert!(!t.contains(&"Start Monitoring"));

        s.away = true;
        assert!(texts(&build_menu_model(&s, now)).contains(&"Status: Away (waiting to lock)"));
    }

    #[test]
    fn last_seen_wording_follows_the_reference() {
        let now = Instant::now();
        let ago = |secs: u64| last_seen_text(Some(now - Duration::from_secs(secs)), now);
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(4), "just now");
        assert_eq!(ago(5), "5 seconds ago");
        assert_eq!(ago(59), "59 seconds ago");
        assert_eq!(ago(60), "1 min ago");
        assert_eq!(ago(190), "3 min ago");
        assert_eq!(last_seen_text(None, now), "\u{2014}");
    }

    #[test]
    fn warning_and_last_auto_lock_lines_appear_only_when_relevant() {
        let mut s = snapshot();
        let base = build_menu_model(&s, Instant::now());
        assert!(!base.iter().any(|e| e.slot == Slot::Info("bluetooth")));
        assert!(!base.iter().any(|e| e.slot == Slot::Info("last-lock")));

        s.availability = BluetoothAvailability::PoweredOff;
        s.last_triggered_lock_at = Some(SystemTime::now());
        let model = build_menu_model(&s, Instant::now());
        let warning = model
            .iter()
            .find(|e| e.slot == Slot::Info("bluetooth"))
            .unwrap();
        assert!(warning.text.ends_with("Bluetooth is off"));
        let lock = model
            .iter()
            .find(|e| e.slot == Slot::Info("last-lock"))
            .unwrap();
        assert!(lock.text.starts_with("Last Auto-Lock: "));
        assert_eq!(lock.text.len(), "Last Auto-Lock: 00:00:00".len());
    }

    #[test]
    fn tooltip_is_capped_at_63_characters() {
        let mut s = snapshot();
        s.monitoring = true;
        s.rssi = Some(-62);
        s.trusted_device = Some(TrustedDevice {
            name: "A very long device name \u{00E7}\u{011F}\u{0131}\u{00F6}\u{015F}\u{00FC} that goes on".into(),
            ..device()
        });
        let tip = tooltip_text(&s);
        assert_eq!(tip.chars().count(), 63);
        assert!(tip.starts_with("SignalLock \u{2014} Monitoring \u{00B7} A very long"));

        s.trusted_device = Some(device());
        assert_eq!(
            tooltip_text(&s),
            "SignalLock \u{2014} Monitoring \u{00B7} My Phone \u{00B7} -62 dBm"
        );
    }

    #[test]
    fn menu_action_ids_round_trip() {
        for action in [
            MenuAction::StartMonitoring,
            MenuAction::PauseMonitoring,
            MenuAction::SelectDevice,
            MenuAction::Settings,
            MenuAction::TestLock,
            MenuAction::CheckForUpdates,
            MenuAction::Quit,
        ] {
            assert_eq!(MenuAction::from_id(action.id()), Some(action));
        }
        assert_eq!(MenuAction::from_id("nope"), None);
    }

    #[test]
    fn check_for_updates_sits_between_settings_and_quit() {
        let model = build_menu_model(&snapshot(), Instant::now());
        let position = |action| model.iter().position(|e| e.slot == Slot::Action(action));
        let check = position(MenuAction::CheckForUpdates).expect("item present");
        assert!(position(MenuAction::Settings).unwrap() < check);
        assert!(check < position(MenuAction::Quit).unwrap());
        assert_eq!(model[check].text, "Check for Updates\u{2026}");
    }

    #[test]
    fn check_for_updates_is_disabled_while_a_job_runs() {
        let busy = [
            UpdateState::Checking,
            UpdateState::Downloading {
                progress: DownloadProgress {
                    downloaded: 10,
                    total: Some(100),
                },
            },
            UpdateState::Verifying,
            UpdateState::Installing,
            UpdateState::RestartPending,
        ];
        let idle = [
            UpdateState::Idle,
            UpdateState::UpToDate,
            UpdateState::Available {
                version: "1.1.0".into(),
                notes: String::new(),
            },
            UpdateState::ConfirmInstall {
                version: "1.1.0".into(),
            },
            UpdateState::ManualRequired {
                url: "https://example.invalid/".into(),
            },
            UpdateState::Failed {
                message: "x".into(),
            },
        ];
        let mut s = snapshot();
        for state in busy {
            s.update = state.clone();
            let model = build_menu_model(&s, Instant::now());
            assert!(
                !find(&model, MenuAction::CheckForUpdates).enabled,
                "{state:?} must disable the item"
            );
        }
        for state in idle {
            s.update = state.clone();
            let model = build_menu_model(&s, Instant::now());
            assert!(
                find(&model, MenuAction::CheckForUpdates).enabled,
                "{state:?} must leave the item enabled"
            );
        }
    }

    #[test]
    fn the_embedded_icons_decode() {
        assert!(load_icon(include_bytes!("../assets/tray-template.png")).is_ok());
        assert!(load_icon(include_bytes!("../assets/tray-color.png")).is_ok());
    }
}
