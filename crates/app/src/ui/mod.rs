//! The Settings, Device Picker and Update windows (Slint) and their wiring to
//! `AppState`.
//!
//! The `.slint` files hold the layout and every piece of visible copy (all
//! English); this module only moves values: it pushes snapshots into the
//! windows' properties and turns their callbacks into `AppState` commands.
//! Views never call services directly (`02-architecture-app.md`, data flow).
//!
//! The windows are created on first use and then hidden and shown again, so
//! closing one never ends the app (the loop runs with
//! `slint::run_event_loop_until_quit`; only the Quit menu item ends it).
//! Everything here runs on the main thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;

use signallock_core::settings::{
    AWAY_DELAY_SECONDS_RANGE, RSSI_SMOOTHING_WINDOW_RANGE, RSSI_THRESHOLD_RANGE,
};
use slint::{CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app_state::{AppSnapshot, AppState, DiscoveredDevice, UiCommand, UpdateState};
use crate::ble::{BluetoothAvailability, DeviceId};
use crate::logging;

mod settings_ui {
    include!(concat!(env!("OUT_DIR"), "/settings.rs"));
}
mod picker_ui {
    include!(concat!(env!("OUT_DIR"), "/device_picker.rs"));
}

mod update_ui {
    include!(concat!(env!("OUT_DIR"), "/update.rs"));
}

use picker_ui::{BluetoothStatus, DevicePickerWindow, DeviceRow};
use settings_ui::SettingsWindow;
use update_ui::{UpdateStage, UpdateWindow};

/// What the picker list shows, comparable so an unchanged list is not
/// pushed into the model again (the list refreshes once per second).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowKey {
    name: String,
    identifier: String,
    rssi: i32,
}

fn row_keys(devices: &[DiscoveredDevice]) -> Vec<RowKey> {
    devices
        .iter()
        .map(|d| RowKey {
            name: d.name.clone(),
            identifier: d.id.to_string(),
            rssi: i32::from(d.rssi),
        })
        .collect()
}

fn bluetooth_status(availability: BluetoothAvailability) -> BluetoothStatus {
    match availability {
        BluetoothAvailability::PoweredOn => BluetoothStatus::Ready,
        BluetoothAvailability::PoweredOff => BluetoothStatus::PoweredOff,
        BluetoothAvailability::Unauthorized => BluetoothStatus::Unauthorized,
        BluetoothAvailability::Unsupported => BluetoothStatus::Unsupported,
        BluetoothAvailability::Unknown => BluetoothStatus::Unknown,
    }
}

/// What the update window shows for an [`UpdateState`]: its stage and the
/// values that stage reads. Pure, so the mapping is unit-tested without a window.
#[derive(Debug, Clone, PartialEq)]
struct UpdateView {
    stage: UpdateStage,
    version: String,
    notes: String,
    /// 0..=1 while the size is known, negative otherwise.
    progress: f32,
    message: String,
}

fn update_view(state: &UpdateState) -> UpdateView {
    let view = |stage| UpdateView {
        stage,
        version: String::new(),
        notes: String::new(),
        progress: -1.0,
        message: String::new(),
    };
    match state {
        UpdateState::Idle => view(UpdateStage::Idle),
        UpdateState::Checking => view(UpdateStage::Checking),
        UpdateState::UpToDate => view(UpdateStage::UpToDate),
        UpdateState::Available { version, notes } => UpdateView {
            version: version.clone(),
            notes: notes.clone(),
            ..view(UpdateStage::Available)
        },
        UpdateState::Downloading { progress } => UpdateView {
            progress: progress
                .total
                .filter(|total| *total > 0)
                .map_or(-1.0, |total| {
                    (progress.downloaded.min(total) as f64 / total as f64) as f32
                }),
            ..view(UpdateStage::Downloading)
        },
        UpdateState::Verifying => view(UpdateStage::Verifying),
        UpdateState::ConfirmInstall { version } => UpdateView {
            version: version.clone(),
            ..view(UpdateStage::Confirm)
        },
        UpdateState::Installing => view(UpdateStage::Installing),
        UpdateState::RestartPending => view(UpdateStage::Restarting),
        UpdateState::ManualRequired { .. } => view(UpdateStage::Manual),
        UpdateState::Failed { message } => UpdateView {
            message: message.clone(),
            ..view(UpdateStage::Failed)
        },
    }
}

fn apply_update(window: &UpdateWindow, state: &UpdateState) {
    let view = update_view(state);
    window.set_stage(view.stage);
    window.set_version(view.version.as_str().into());
    window.set_notes(view.notes.as_str().into());
    window.set_progress(view.progress);
    window.set_message(view.message.as_str().into());
}

/// Owner of the three windows.
pub struct Windows {
    app: Rc<AppState>,
    settings: RefCell<Option<Rc<SettingsWindow>>>,
    picker: RefCell<Option<Rc<DevicePickerWindow>>>,
    update: RefCell<Option<Rc<UpdateWindow>>>,
    /// The state last pushed into the update window; an unchanged state (the
    /// once-per-second notifications) is not pushed again.
    shown_update: RefCell<Option<UpdateState>>,
    rows: Rc<VecModel<DeviceRow>>,
    shown_rows: RefCell<Vec<RowKey>>,
}

impl Windows {
    /// Creates the controller and subscribes it to `app`. Main thread only.
    pub fn install(app: &Rc<AppState>) -> Rc<Self> {
        let windows = Rc::new(Self {
            app: app.clone(),
            settings: RefCell::new(None),
            picker: RefCell::new(None),
            update: RefCell::new(None),
            shown_update: RefCell::new(None),
            rows: Rc::new(VecModel::default()),
            shown_rows: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&windows);
        app.subscribe(move |snapshot| {
            if let Some(windows) = weak.upgrade() {
                windows.refresh(snapshot);
            }
        });
        windows
    }

    /// Performs a window command; `Quit` and the single-instance guard
    /// commands are not window concerns and are ignored here.
    pub fn handle(self: &Rc<Self>, command: UiCommand) {
        match command {
            UiCommand::OpenSettings => self.open_settings(),
            UiCommand::OpenPicker => self.open_picker(),
            UiCommand::OpenUpdate => self.open_update(),
            UiCommand::Quit | UiCommand::ReleaseInstanceGuard | UiCommand::RestoreInstanceGuard => {
            }
        }
    }

    /// Re-reads the state into every visible window.
    fn refresh(&self, snapshot: &AppSnapshot) {
        let settings = self.settings.borrow().clone();
        if let Some(window) = settings.filter(|w| w.window().is_visible()) {
            apply_settings(&window, snapshot);
        }
        let picker = self.picker.borrow().clone();
        if let Some(window) = picker.filter(|w| w.window().is_visible()) {
            self.apply_picker(&window, snapshot);
        }
        let update = self.update.borrow().clone();
        // `Idle` is never drawn: it only exists while the window is being
        // dismissed, and a blank frame would flash before it hides.
        if let Some(window) = update
            .filter(|w| w.window().is_visible())
            .filter(|_| snapshot.update != UpdateState::Idle)
        {
            let mut shown = self.shown_update.borrow_mut();
            if shown.as_ref() != Some(&snapshot.update) {
                apply_update(&window, &snapshot.update);
                *shown = Some(snapshot.update.clone());
            }
        }
    }

    // ----- Update -----

    fn open_update(self: &Rc<Self>) {
        let existing = self.update.borrow().clone();
        let window = match existing {
            Some(window) => window,
            None => match self.build_update().map(Rc::new) {
                Ok(window) => {
                    *self.update.borrow_mut() = Some(window.clone());
                    window
                }
                Err(e) => {
                    tracing::error!(target: logging::APP, "Update window could not be created: {e}");
                    return;
                }
            },
        };
        let state = self.app.snapshot().update;
        apply_update(&window, &state);
        *self.shown_update.borrow_mut() = Some(state);
        show(&*window, "Update");
    }

    fn build_update(self: &Rc<Self>) -> Result<UpdateWindow, slint::PlatformError> {
        let window = UpdateWindow::new()?;
        window.set_current_version(env!("CARGO_PKG_VERSION").into());

        window.on_check_again({
            let app = self.app.clone();
            move || app.check_for_updates()
        });
        // Both buttons run `install_update`: which step it is depends on the
        // state (download, then the confirmed restart).
        window.on_download_install({
            let app = self.app.clone();
            move || app.install_update()
        });
        window.on_confirm_install({
            let app = self.app.clone();
            move || app.install_update()
        });
        window.on_open_page({
            let app = self.app.clone();
            move || app.open_release_page()
        });
        // Hide first, then dismiss: the state change must not repaint a window
        // that is on its way out.
        window.on_dismiss({
            let app = self.app.clone();
            let weak = window.as_weak();
            move || {
                if let Some(window) = weak.upgrade() {
                    hide(&window, "Update");
                }
                app.dismiss_update();
            }
        });
        // The title-bar close button dismisses too (declining a confirmation
        // deletes the downloaded file). While a job runs `dismiss_update` does
        // nothing, the window just hides, and it comes back when the job ends
        // in a state that waits for the user.
        window.window().on_close_requested({
            let app = self.app.clone();
            move || {
                app.dismiss_update();
                CloseRequestResponse::HideWindow
            }
        });
        Ok(window)
    }

    // ----- Settings -----

    fn open_settings(self: &Rc<Self>) {
        let existing = self.settings.borrow().clone();
        let window = match existing {
            Some(window) => window,
            None => match self.build_settings().map(Rc::new) {
                Ok(window) => {
                    *self.settings.borrow_mut() = Some(window.clone());
                    window
                }
                Err(e) => {
                    tracing::error!(target: logging::APP, "Settings window could not be created: {e}");
                    return;
                }
            },
        };
        apply_settings(&window, &self.app.snapshot());
        show(&*window, "Settings");
    }

    fn build_settings(self: &Rc<Self>) -> Result<SettingsWindow, slint::PlatformError> {
        let window = SettingsWindow::new()?;
        window.set_threshold_min(*RSSI_THRESHOLD_RANGE.start());
        window.set_threshold_max(*RSSI_THRESHOLD_RANGE.end());
        window.set_away_delay_min(*AWAY_DELAY_SECONDS_RANGE.start() as i32);
        window.set_away_delay_max(*AWAY_DELAY_SECONDS_RANGE.end() as i32);
        window.set_smoothing_min(*RSSI_SMOOTHING_WINDOW_RANGE.start() as i32);
        window.set_smoothing_max(*RSSI_SMOOTHING_WINDOW_RANGE.end() as i32);

        // After each command the window is re-synced from the state: a
        // control may hold a value the state did not accept (clamped, or a
        // failed autostart change) and `AppState` does not notify for those.
        let resync = {
            let windows = Rc::downgrade(self);
            move || {
                if let Some(windows) = windows.upgrade() {
                    windows.refresh(&windows.app.snapshot());
                }
            }
        };

        let app = self.app.clone();
        window.on_monitoring_toggled({
            let app = app.clone();
            let resync = resync.clone();
            move |on| {
                if on {
                    app.start_monitoring();
                } else {
                    app.stop_monitoring();
                }
                resync();
            }
        });
        window.on_rssi_threshold_changed({
            let app = app.clone();
            let resync = resync.clone();
            move |value| {
                app.update_settings(|s| s.rssi_threshold = value);
                resync();
            }
        });
        window.on_away_delay_changed({
            let app = app.clone();
            let resync = resync.clone();
            move |value| {
                app.update_settings(|s| s.away_delay_seconds = u32::try_from(value).unwrap_or(0));
                resync();
            }
        });
        window.on_smoothing_window_changed({
            let app = app.clone();
            let resync = resync.clone();
            move |value| {
                app.update_settings(|s| {
                    s.rssi_smoothing_window = u32::try_from(value).unwrap_or(0);
                });
                resync();
            }
        });
        window.on_start_at_login_toggled({
            let app = app.clone();
            move |on| {
                app.set_start_at_login(on);
                resync();
            }
        });
        window.on_change_device({
            let app = app.clone();
            move || app.open_picker()
        });
        window.on_forget_device({
            let app = app.clone();
            move || app.clear_trusted_device()
        });
        window.on_close_window({
            let weak = window.as_weak();
            move || {
                if let Some(window) = weak.upgrade() {
                    hide(&window, "Settings");
                }
            }
        });
        Ok(window)
    }

    // ----- Device picker -----

    fn open_picker(self: &Rc<Self>) {
        let existing = self.picker.borrow().clone();
        let window = match existing {
            Some(window) => window,
            None => match self.build_picker().map(Rc::new) {
                Ok(window) => {
                    *self.picker.borrow_mut() = Some(window.clone());
                    window
                }
                Err(e) => {
                    tracing::error!(target: logging::APP, "Device picker could not be created: {e}");
                    return;
                }
            },
        };
        let was_open = window.window().is_visible();
        if !was_open {
            // A fresh list for a fresh session; the discovery scan ends when
            // the window closes (`close_picker`).
            self.shown_rows.borrow_mut().clear();
            self.rows.set_vec(Vec::new());
            self.app.start_discovery();
        }
        self.apply_picker(&window, &self.app.snapshot());
        show(&*window, "Device Picker");
    }

    fn build_picker(self: &Rc<Self>) -> Result<DevicePickerWindow, slint::PlatformError> {
        let window = DevicePickerWindow::new()?;
        window.set_devices(ModelRc::from(self.rows.clone()));

        window.on_select_device({
            let app = self.app.clone();
            let weak = window.as_weak();
            move |identifier: SharedString| match DeviceId::from_str(identifier.as_str()) {
                Ok(id) => {
                    app.select_device(&id);
                    if let Some(window) = weak.upgrade() {
                        close_picker(&app, &window);
                    }
                }
                Err(_) => tracing::error!(
                    target: logging::APP,
                    "Picker row has an invalid device id: {identifier}"
                ),
            }
        });
        window.on_done({
            let app = self.app.clone();
            let weak = window.as_weak();
            move || {
                if let Some(window) = weak.upgrade() {
                    close_picker(&app, &window);
                }
            }
        });
        // The title-bar close button: end discovery, then hide.
        window.window().on_close_requested({
            let app = self.app.clone();
            move || {
                app.stop_discovery();
                CloseRequestResponse::HideWindow
            }
        });
        Ok(window)
    }

    fn apply_picker(&self, window: &DevicePickerWindow, snapshot: &AppSnapshot) {
        window.set_bluetooth_status(bluetooth_status(snapshot.availability));

        let keys = row_keys(&snapshot.discovered);
        let mut shown = self.shown_rows.borrow_mut();
        sync_rows(&self.rows, &shown, &keys);
        *shown = keys;
    }
}

/// What [`sync_rows`] did to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSync {
    Unchanged,
    /// Rows were updated in place: no row changed its device.
    InPlace,
    /// The model was reset (the rows' delegates are rebuilt).
    Reset,
}

/// Brings `model` from the rows described by `shown` to those in `keys`.
///
/// A row of the list is a reused Slint delegate. If one delegate could be
/// handed a different device while the user holds its `Select` button down,
/// the release would select that other device (the button reads the device
/// when it is clicked). So a row keeps its device for as long as the delegate
/// lives: when the devices are the same and in the same order, only their
/// RSSI changes and the rows are updated in place (no reset, so hover, focus
/// and a click in progress survive); when a device appears, disappears or the
/// order changes, the model is reset. A click that straddles a reset is lost,
/// never redirected to another device.
fn sync_rows(model: &VecModel<DeviceRow>, shown: &[RowKey], keys: &[RowKey]) -> RowSync {
    if shown == keys {
        return RowSync::Unchanged;
    }
    let to_row = |key: &RowKey| DeviceRow {
        name: key.name.as_str().into(),
        identifier: key.identifier.as_str().into(),
        rssi: key.rssi,
    };
    let same_devices = shown.len() == keys.len()
        && shown
            .iter()
            .zip(keys)
            .all(|(old, new)| old.identifier == new.identifier);
    if same_devices {
        for (index, (new, old)) in keys.iter().zip(shown).enumerate() {
            if new != old {
                model.set_row_data(index, to_row(new));
            }
        }
        RowSync::InPlace
    } else {
        model.set_vec(keys.iter().map(to_row).collect::<Vec<_>>());
        RowSync::Reset
    }
}

fn apply_settings(window: &SettingsWindow, snapshot: &AppSnapshot) {
    let settings = &snapshot.settings;
    window.set_monitoring_on(snapshot.monitoring);
    window.set_rssi_threshold(settings.rssi_threshold as f32);
    window.set_away_delay(i32::try_from(settings.away_delay_seconds).unwrap_or(i32::MAX));
    window.set_smoothing_window(i32::try_from(settings.rssi_smoothing_window).unwrap_or(i32::MAX));
    window.set_start_at_login(settings.start_at_login);
    match &snapshot.trusted_device {
        Some(device) => {
            window.set_has_trusted_device(true);
            window.set_trusted_name(device.name.as_str().into());
            window.set_trusted_identifier(device.identifier.as_str().into());
        }
        None => {
            window.set_has_trusted_device(false);
            window.set_trusted_name(SharedString::new());
            window.set_trusted_identifier(SharedString::new());
        }
    }
}

/// Ends discovery and hides the picker.
fn close_picker(app: &AppState, window: &DevicePickerWindow) {
    app.stop_discovery();
    hide(window, "Device Picker");
}

fn show<C: ComponentHandle>(window: &C, name: &str) {
    match window.show() {
        Ok(()) => {
            #[cfg(target_os = "macos")]
            macos::activate_app();
            tracing::info!(target: logging::APP, "{name} window shown");
        }
        Err(e) => tracing::error!(target: logging::APP, "{name} window could not be shown: {e}"),
    }
}

fn hide<C: ComponentHandle>(window: &C, name: &str) {
    match window.hide() {
        Ok(()) => tracing::info!(target: logging::APP, "{name} window closed"),
        Err(e) => tracing::error!(target: logging::APP, "{name} window could not be closed: {e}"),
    }
}

/// Process-level setup that has to run on the main thread once the event loop
/// is running: on macOS the Dock icon is hidden by switching the activation
/// policy to `Accessory` (the bundle does the same with `LSUIElement`, Faz 06;
/// this covers `cargo run`). A no-op elsewhere.
pub fn prepare_event_loop() {
    #[cfg(target_os = "macos")]
    macos::set_accessory_policy();
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    use crate::logging;

    pub fn set_accessory_policy() {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::error!(target: logging::APP, "activation policy must be set on the main thread");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
            tracing::info!(target: logging::APP, "Activation policy set to Accessory (no Dock icon)");
        } else {
            tracing::warn!(target: logging::APP, "Activation policy could not be set");
        }
    }

    /// An accessory app is not frontmost by itself; bring its window up.
    pub fn activate_app() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn device(id: &str, name: &str, rssi: i16) -> DiscoveredDevice {
        DiscoveredDevice {
            id: DeviceId::from_str(id).unwrap(),
            name: name.to_owned(),
            rssi,
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn rows_keep_the_order_and_content_of_the_list() {
        let keys = row_keys(&[
            device("11111111-2222-3333-4444-555555555555", "Phone", -50),
            device("AA:BB:CC:DD:EE:FF", "Watch", -70),
        ]);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].name, "Phone");
        assert_eq!(keys[0].identifier, "11111111-2222-3333-4444-555555555555");
        assert_eq!(keys[0].rssi, -50);
        assert_eq!(keys[1].identifier, "AABBCCDDEEFF");
    }

    fn key(id: &str, rssi: i32) -> RowKey {
        RowKey {
            name: id.to_owned(),
            identifier: id.to_owned(),
            rssi,
        }
    }

    fn contents(model: &VecModel<DeviceRow>) -> Vec<(String, i32)> {
        model
            .iter()
            .map(|row| (row.identifier.to_string(), row.rssi))
            .collect()
    }

    fn identifiers(model: &VecModel<DeviceRow>) -> Vec<String> {
        contents(model).into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn an_rssi_change_updates_rows_in_place() {
        let model = VecModel::<DeviceRow>::default();
        let first = vec![key("a", -50), key("b", -60)];
        assert_eq!(sync_rows(&model, &[], &first), RowSync::Reset);
        assert_eq!(contents(&model), vec![("a".into(), -50), ("b".into(), -60)]);

        let second = vec![key("a", -52), key("b", -60)];
        assert_eq!(sync_rows(&model, &first, &second), RowSync::InPlace);
        assert_eq!(contents(&model), vec![("a".into(), -52), ("b".into(), -60)]);
        assert_eq!(sync_rows(&model, &second, &second), RowSync::Unchanged);
    }

    #[test]
    fn a_reorder_resets_so_a_held_select_button_never_changes_device() {
        // Device A is on row 0 and the user holds its `Select` down. If the
        // update swapped A and B in place, the same delegate would now show B
        // and releasing would select B. A reorder must rebuild the rows.
        let model = VecModel::<DeviceRow>::default();
        let before = vec![key("a", -50), key("b", -52)];
        sync_rows(&model, &[], &before);
        let device_on_row_0_at_press = identifiers(&model)[0].clone();
        assert_eq!(device_on_row_0_at_press, "a");

        let after = vec![key("b", -49), key("a", -51)];
        let outcome = sync_rows(&model, &before, &after);

        assert_eq!(outcome, RowSync::Reset);
        assert_eq!(identifiers(&model), vec!["b", "a"]);
    }

    #[test]
    fn rows_only_change_device_through_a_reset() {
        // The invariant behind the two tests above, over assorted updates:
        // after an in-place sync every row still shows the same device.
        let steps = [
            vec![key("a", -50), key("b", -60), key("c", -70)],
            vec![key("a", -51), key("b", -61), key("c", -71)],
            vec![key("b", -50), key("a", -51), key("c", -71)],
            vec![key("b", -50), key("a", -51)],
            vec![key("a", -50), key("d", -51)],
            vec![],
        ];
        let model = VecModel::<DeviceRow>::default();
        let mut shown: Vec<RowKey> = Vec::new();
        for keys in steps {
            let before = identifiers(&model);
            let outcome = sync_rows(&model, &shown, &keys);
            if outcome != RowSync::Reset {
                assert_eq!(identifiers(&model), before, "in-place sync moved a device");
            }
            assert_eq!(
                identifiers(&model),
                keys.iter()
                    .map(|k| k.identifier.clone())
                    .collect::<Vec<_>>()
            );
            shown = keys;
        }
    }

    #[test]
    fn every_availability_maps_to_a_status() {
        use BluetoothAvailability as A;
        assert!(matches!(
            bluetooth_status(A::PoweredOn),
            BluetoothStatus::Ready
        ));
        assert!(matches!(
            bluetooth_status(A::PoweredOff),
            BluetoothStatus::PoweredOff
        ));
        assert!(matches!(
            bluetooth_status(A::Unauthorized),
            BluetoothStatus::Unauthorized
        ));
        assert!(matches!(
            bluetooth_status(A::Unsupported),
            BluetoothStatus::Unsupported
        ));
        assert!(matches!(
            bluetooth_status(A::Unknown),
            BluetoothStatus::Unknown
        ));
    }

    #[test]
    fn every_update_state_maps_to_its_stage_and_values() {
        use crate::update::DownloadProgress;

        let view = |state: UpdateState| update_view(&state);
        assert_eq!(view(UpdateState::Idle).stage, UpdateStage::Idle);
        assert_eq!(view(UpdateState::Checking).stage, UpdateStage::Checking);
        assert_eq!(view(UpdateState::UpToDate).stage, UpdateStage::UpToDate);
        assert_eq!(view(UpdateState::Verifying).stage, UpdateStage::Verifying);
        assert_eq!(view(UpdateState::Installing).stage, UpdateStage::Installing);
        assert_eq!(
            view(UpdateState::RestartPending).stage,
            UpdateStage::Restarting
        );

        // Release notes pass through untouched: the window shows them as text.
        let available = view(UpdateState::Available {
            version: "1.1.0".into(),
            notes: "<b>bold</b> **x**\nline two".into(),
        });
        assert_eq!(available.stage, UpdateStage::Available);
        assert_eq!(available.version, "1.1.0");
        assert_eq!(available.notes, "<b>bold</b> **x**\nline two");

        let confirm = view(UpdateState::ConfirmInstall {
            version: "1.1.0".into(),
        });
        assert_eq!(confirm.stage, UpdateStage::Confirm);
        assert_eq!(confirm.version, "1.1.0");

        let manual = view(UpdateState::ManualRequired {
            url: "https://github.com/muhammetsafak/signallock/releases/".into(),
        });
        assert_eq!(manual.stage, UpdateStage::Manual);
        assert!(manual.message.is_empty(), "the URL is never shown as text");

        let failed = view(UpdateState::Failed {
            message: "Could not reach GitHub: timed out".into(),
        });
        assert_eq!(failed.stage, UpdateStage::Failed);
        assert_eq!(failed.message, "Could not reach GitHub: timed out");

        let downloading = |downloaded, total| {
            view(UpdateState::Downloading {
                progress: DownloadProgress { downloaded, total },
            })
        };
        assert_eq!(downloading(0, Some(200)).progress, 0.0);
        assert_eq!(downloading(50, Some(200)).progress, 0.25);
        assert_eq!(downloading(200, Some(200)).progress, 1.0);
        assert_eq!(
            downloading(250, Some(200)).progress,
            1.0,
            "never above 100 %"
        );
        assert!(
            downloading(10, None).progress < 0.0,
            "unknown size is indeterminate"
        );
        assert!(downloading(10, Some(0)).progress < 0.0);
        assert_eq!(downloading(1, Some(2)).stage, UpdateStage::Downloading);
    }
}
