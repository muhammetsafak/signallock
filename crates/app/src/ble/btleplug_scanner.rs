//! btleplug implementation of [`BleScanner`].
//!
//! One actor task owns the btleplug adapter. The trait methods only send it a
//! command over an unbounded channel, so they never block (the main thread
//! calls them) and the scanner is usable without awaiting anything. The actor
//! turns commands and btleplug events into [`BleEvent`]s.
//!
//! # Behaviour copied from the references
//!
//! `macos/Sources/SignalLock/BluetoothDeviceScanner.swift` and
//! `windows/Services/BluetoothScanner.cs`:
//!
//! - Scan for every peripheral (`ScanFilter::default()`); in monitoring mode
//!   the events of the trusted device are the only ones forwarded.
//! - Duplicate advertisements are not filtered: btleplug's CoreBluetooth
//!   backend scans with `CBCentralManagerScanOptionAllowDuplicatesKey` and its
//!   BlueZ backend with `DuplicateData`, and WinRT delivers every advertisement
//!   (`Active` scanning). The proximity engine needs a reading per
//!   advertisement.
//! - An RSSI of 127 means "not available" and is dropped.
//! - A scan is only started while the adapter is powered on. Starting while a
//!   scan runs stops it first (`startScanIfPossible` / `StartIfPossible`).
//! - When the adapter goes back to powered on while a scan mode is active and
//!   no scan runs, the scan is restarted (`centralManagerDidUpdateState`).
//!   Nothing else restarts a scan: a scan that dies on its own is logged and
//!   stays dead (Windows `OnStopped`), and the monitor's silence path locks.
//!   Small differences from the Swift code: the restart also covers discovery
//!   mode, and an explicit `stop()` ends the mode, so a later power cycle does
//!   not silently resume a scan nobody asked for.
//! - No name means no name: [`BleEvent::Discovered::name`] is `None` and the
//!   UI shows [`super::UNKNOWN_DEVICE_NAME`].
//!
//! # Where the RSSI comes from (btleplug 0.13.5, read from its source)
//!
//! The backends do not report a reading the same way, so the events that count
//! as "one advertisement" differ per platform (see [`sample_source`]):
//!
//! - **Windows:** `DeviceDiscovered` / `DeviceUpdated` fire once per
//!   advertisement with the properties already updated.
//!   `RssiUpdate` only fires on a change and would double-count.
//! - **macOS:** `DeviceUpdated` fires only for adverts that carry a name, and
//!   `properties().rssi` is updated by the manufacturer-data, service-data and
//!   services advertisement events, which fire before the property is read.
//!   An advert that carries none of those three is invisible to btleplug
//!   there, so a device that only ever advertises a bare name is never seen.
//! - **Linux (BlueZ):** `DeviceDiscovered` once, then `RssiUpdate` and the same
//!   three advertisement events as property changes arrive. `DeviceUpdated` is
//!   never emitted.
//!
//! One advertisement can produce several of those events (macOS: one per
//! advertised field; BlueZ: `RssiUpdate` next to a field event), which would
//! count the same reading two or three times in the proximity window. The
//! [`AdvertDeduper`] folds events of the same device that arrive within
//! [`DEDUP_WINDOW`] into one reading. The first event of an advertisement
//! already carries its RSSI in `properties()`.
//!
//! Adapter availability: btleplug reports `PoweredOn`, `PoweredOff` and
//! `Unknown` only. `Unauthorized` is reported when the adapter cannot be opened
//! because the OS denied access (macOS `PermissionDenied`), `Unsupported` when
//! there is no adapter. A Windows radio in the `Disabled` state, which the C#
//! reference maps to `Unauthorized`, shows up here as `Unknown`.

use std::collections::HashMap;
use std::future::pending;
use std::pin::Pin;
use std::time::{Duration, Instant};

use btleplug::api::{
    Central, CentralEvent, CentralState, Manager as _, Peripheral as _, ScanFilter,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use super::{BleEvent, BleScanner, BluetoothAvailability, DeviceId};
use crate::logging::BLUETOOTH;

/// CoreBluetooth and WinRT report 127 when the RSSI is not available.
const RSSI_UNAVAILABLE: i16 = 127;

/// Capacity of the event channel. Advertisements arrive at a few dozen per
/// second at most; the consumer only forwards them.
const EVENT_CHANNEL_CAPACITY: usize = 256;

type EventStream = Pin<Box<dyn Stream<Item = CentralEvent> + Send>>;

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// Commands from the [`BleScanner`] methods to the actor.
#[derive(Debug)]
enum Command {
    StartDiscovery,
    StartMonitoring(DeviceId),
    Stop,
}

/// The btleplug-backed scanner. Create it with the handle of the tokio runtime
/// that should run the BLE task; dropping it stops the scan and the task.
pub struct BtleplugScanner {
    commands: mpsc::UnboundedSender<Command>,
    events: Option<mpsc::Receiver<BleEvent>>,
}

impl BtleplugScanner {
    /// Spawns the actor on `runtime`. On macOS the Bluetooth permission prompt
    /// can appear as soon as the actor opens the adapter.
    pub fn new(runtime: &tokio::runtime::Handle) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        runtime.spawn(run(command_rx, event_tx));
        Self {
            commands: command_tx,
            events: Some(event_rx),
        }
    }

    fn send(&self, command: Command) {
        if let Err(e) = self.commands.send(command) {
            warn!(target: BLUETOOTH, "BLE task is gone, dropping command {:?}", e.0);
        }
    }
}

impl BleScanner for BtleplugScanner {
    fn start_discovery(&self) {
        self.send(Command::StartDiscovery);
    }

    fn start_monitoring(&self, id: DeviceId) {
        self.send(Command::StartMonitoring(id));
    }

    fn stop(&self) {
        self.send(Command::Stop);
    }

    fn take_events(&mut self) -> Option<mpsc::Receiver<BleEvent>> {
        self.events.take()
    }
}

// ---------------------------------------------------------------------------
// Scan state machine (pure, unit-tested)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Idle,
    Discovery,
    Monitoring(DeviceId),
}

/// What the actor must do to the adapter after a state change.
#[derive(Debug, PartialEq, Eq)]
enum ScanAction {
    None,
    /// Start a scan; `stop_first` when one is believed to be running.
    Start {
        stop_first: bool,
    },
    Stop,
}

/// Mode, availability and "is a scan running" bookkeeping. It decides; the
/// actor performs the btleplug calls and reports failures back.
#[derive(Debug, Default)]
struct ScanPlan {
    availability: BluetoothAvailability,
    mode: Mode,
    scanning: bool,
}

impl ScanPlan {
    /// `startDiscoveryScan` / `startMonitoringScan`.
    fn begin(&mut self, mode: Mode) -> ScanAction {
        self.mode = mode;
        self.start_if_possible()
    }

    /// Sets the mode without touching the scan. Used when the adapter is
    /// (re)opened first: the power-on transition then starts the scan for this
    /// mode, so only one start happens.
    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// (Re)starts the scan for the current mode.
    fn restart(&mut self) -> ScanAction {
        self.start_if_possible()
    }

    /// `stopScan`. Ends the mode (see the module docs).
    fn stop(&mut self) -> ScanAction {
        self.mode = Mode::Idle;
        if self.scanning {
            self.scanning = false;
            ScanAction::Stop
        } else {
            ScanAction::None
        }
    }

    /// `centralManagerDidUpdateState`. Returns whether the availability
    /// changed, and the scan restart when the adapter came back.
    fn set_availability(&mut self, new: BluetoothAvailability) -> (bool, ScanAction) {
        let changed = new != self.availability;
        self.availability = new;
        if new != BluetoothAvailability::PoweredOn {
            // The OS stops the scan when the adapter goes away.
            self.scanning = false;
            return (changed, ScanAction::None);
        }
        if self.mode != Mode::Idle && !self.scanning {
            return (changed, self.start_if_possible());
        }
        (changed, ScanAction::None)
    }

    /// The `start_scan` call failed.
    fn start_failed(&mut self) {
        self.scanning = false;
    }

    /// The event stream ended: whatever scan ran is gone.
    fn adapter_lost(&mut self) {
        self.scanning = false;
    }

    fn start_if_possible(&mut self) -> ScanAction {
        if self.availability != BluetoothAvailability::PoweredOn {
            return ScanAction::None;
        }
        let stop_first = self.scanning;
        self.scanning = true;
        ScanAction::Start { stop_first }
    }
}

/// Turns one reading into the event for the current mode.
fn route(mode: &Mode, id: DeviceId, name: Option<String>, rssi: i16) -> Option<BleEvent> {
    if rssi == RSSI_UNAVAILABLE {
        return None;
    }
    match mode {
        Mode::Idle => None,
        Mode::Discovery => Some(BleEvent::Discovered {
            id,
            name: name.filter(|n| !n.is_empty()),
            rssi,
        }),
        Mode::Monitoring(tracked) if *tracked == id => Some(BleEvent::Sample { id, rssi }),
        Mode::Monitoring(_) => None,
    }
}

fn availability_from_state(state: &CentralState) -> BluetoothAvailability {
    match state {
        CentralState::PoweredOn => BluetoothAvailability::PoweredOn,
        CentralState::PoweredOff => BluetoothAvailability::PoweredOff,
        CentralState::Unknown => BluetoothAvailability::Unknown,
    }
}

/// How an adapter-open failure is reported (Windows reference: no adapter is
/// `Unsupported`, any other failure `Unknown`).
fn availability_from_error(error: &btleplug::Error) -> BluetoothAvailability {
    match error {
        btleplug::Error::PermissionDenied => BluetoothAvailability::Unauthorized,
        btleplug::Error::NoAdapterAvailable => BluetoothAvailability::Unsupported,
        _ => BluetoothAvailability::Unknown,
    }
}

// ---------------------------------------------------------------------------
// btleplug event mapping
// ---------------------------------------------------------------------------

/// Where the RSSI of a reading comes from.
#[derive(Debug, PartialEq, Eq)]
enum RssiSource {
    /// The event carries it.
    Event(i16),
    /// Read `peripheral.properties().rssi`.
    Properties,
}

// Which btleplug events stand for one advertisement, per backend (module docs).
const USE_DEVICE_EVENTS: bool = cfg!(not(target_os = "macos"));
const USE_ADVERTISEMENT_EVENTS: bool = cfg!(not(target_os = "windows"));
const USE_RSSI_EVENTS: bool = cfg!(not(target_os = "windows"));

/// Events of one device closer together than this are one advertisement. BLE
/// advertising intervals are at least 20 ms; the events of a single
/// advertisement are microseconds apart. Windows delivers exactly one event
/// per advertisement, so nothing is folded there.
const DEDUP_WINDOW: Duration = if cfg!(target_os = "windows") {
    Duration::ZERO
} else {
    Duration::from_millis(10)
};

/// Folds the several btleplug events of one advertisement into one reading.
/// The window is anchored at the first event, so a rejected event never
/// extends it.
#[derive(Debug, Default)]
struct AdvertDeduper {
    last_accepted: HashMap<DeviceId, Instant>,
}

impl AdvertDeduper {
    /// Above this many remembered devices, entries older than the window are
    /// dropped. An entry that old can no longer fold anything, so pruning never
    /// changes a decision; it only bounds the map (macOS hands out a new
    /// CoreBluetooth UUID whenever a phone rotates its address).
    const PRUNE_ABOVE: usize = 256;

    fn accept(&mut self, id: &DeviceId, now: Instant, window: Duration) -> bool {
        if self.last_accepted.len() > Self::PRUNE_ABOVE {
            self.last_accepted
                .retain(|_, previous| now.saturating_duration_since(*previous) < window);
        }
        if let Some(previous) = self.last_accepted.get(id)
            && now.saturating_duration_since(*previous) < window
        {
            return false;
        }
        self.last_accepted.insert(id.clone(), now);
        true
    }

    /// Forgets every device (scan mode change or stop).
    fn clear(&mut self) {
        self.last_accepted.clear();
    }
}

fn sample_source(event: &CentralEvent) -> Option<(&PeripheralId, RssiSource)> {
    match event {
        CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id)
            if USE_DEVICE_EVENTS =>
        {
            Some((id, RssiSource::Properties))
        }
        CentralEvent::RssiUpdate { id, rssi } if USE_RSSI_EVENTS => {
            Some((id, RssiSource::Event(*rssi)))
        }
        CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::ServiceDataAdvertisement { id, .. }
        | CentralEvent::ServicesAdvertisement { id, .. }
            if USE_ADVERTISEMENT_EVENTS =>
        {
            Some((id, RssiSource::Properties))
        }
        _ => None,
    }
}

/// The platform-neutral id of a btleplug peripheral. CoreBluetooth has no
/// address (btleplug returns zeros), so its UUID identity is used; the other
/// backends use the Bluetooth address.
fn device_id(peripheral: &Peripheral) -> Option<DeviceId> {
    if cfg!(target_vendor = "apple") {
        peripheral.id().to_string().parse().ok()
    } else {
        Some(DeviceId::from_address_bytes(
            peripheral.address().into_inner(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Actor
// ---------------------------------------------------------------------------

struct Actor {
    events: mpsc::Sender<BleEvent>,
    plan: ScanPlan,
    /// Kept alive next to the adapter: some backends tie the session to it.
    manager: Option<Manager>,
    adapter: Option<Adapter>,
    stream: Option<EventStream>,
    dedup: AdvertDeduper,
    /// An `AdapterState` has been sent at least once.
    announced: bool,
    /// The receiver is gone: nobody listens, the task ends.
    closed: bool,
}

async fn run(mut commands: mpsc::UnboundedReceiver<Command>, events: mpsc::Sender<BleEvent>) {
    let mut actor = Actor {
        events,
        plan: ScanPlan::default(),
        manager: None,
        adapter: None,
        stream: None,
        dedup: AdvertDeduper::default(),
        announced: false,
        closed: false,
    };

    // Open the adapter right away, like the macOS reference creates its
    // `CBCentralManager` in `init`: the permission prompt and the first
    // `AdapterState` should not wait for the first scan request.
    actor.ensure_adapter().await;

    while !actor.closed {
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => actor.handle_command(command).await,
                None => break,
            },
            event = next_event(&mut actor.stream) => match event {
                Some(event) => actor.handle_event(event).await,
                None => actor.stream_ended(),
            },
        }
    }

    actor.shutdown().await;
}

/// The next btleplug event; pends forever while there is no stream.
async fn next_event(stream: &mut Option<EventStream>) -> Option<CentralEvent> {
    match stream {
        Some(stream) => stream.next().await,
        None => pending().await,
    }
}

impl Actor {
    async fn handle_command(&mut self, command: Command) {
        debug!(target: BLUETOOTH, "Command: {command:?}");
        match command {
            Command::StartDiscovery => self.start(Mode::Discovery).await,
            Command::StartMonitoring(id) => self.start(Mode::Monitoring(id)).await,
            Command::Stop => {
                self.dedup.clear();
                let action = self.plan.stop();
                self.perform(action).await;
            }
        }
    }

    async fn start(&mut self, mode: Mode) {
        // Like the Windows reference, which refreshes the availability on every
        // start: a failed or lost adapter is reopened here. The mode is set
        // first, so a reopen that finds the adapter powered on starts the scan
        // for the new mode itself and no second start follows.
        self.plan.set_mode(mode);
        self.dedup.clear();
        let reopened = self.ensure_adapter().await;
        if !reopened {
            let action = self.plan.restart();
            self.perform(action).await;
        }
    }

    async fn handle_event(&mut self, event: CentralEvent) {
        if let CentralEvent::StateUpdate(state) = &event {
            let action = self
                .apply_availability(availability_from_state(state))
                .await;
            self.perform(action).await;
            return;
        }
        self.on_advertisement(&event).await;
    }

    async fn on_advertisement(&mut self, event: &CentralEvent) {
        let Some((peripheral_id, source)) = sample_source(event) else {
            return;
        };
        let received_at = Instant::now();
        if self.plan.mode == Mode::Idle {
            return;
        }
        let Some(adapter) = self.adapter.clone() else {
            return;
        };
        let Ok(peripheral) = adapter.peripheral(peripheral_id).await else {
            return;
        };
        let Some(id) = device_id(&peripheral) else {
            return;
        };
        // Monitoring: drop everything but the trusted device before doing any
        // further work.
        if let Mode::Monitoring(tracked) = &self.plan.mode
            && *tracked != id
        {
            return;
        }
        if !self.dedup.accept(&id, received_at, DEDUP_WINDOW) {
            return;
        }

        // Discovery needs the name, so the properties are read in that mode
        // even when the event carries the RSSI.
        let needs_properties =
            source == RssiSource::Properties || matches!(self.plan.mode, Mode::Discovery);
        let properties = if needs_properties {
            peripheral.properties().await.ok().flatten()
        } else {
            None
        };
        let rssi = match source {
            RssiSource::Event(rssi) => Some(rssi),
            RssiSource::Properties => properties.as_ref().and_then(|p| p.rssi),
        };
        let Some(rssi) = rssi else {
            return;
        };
        let name = properties.and_then(|p| p.advertisement_name.or(p.local_name));

        if let Some(event) = route(&self.plan.mode, id, name, rssi) {
            trace!(target: BLUETOOTH, "{event:?}");
            self.emit(event).await;
        }
    }

    /// Opens the first adapter and subscribes to its events, unless that was
    /// already done. Failures are mapped to an `AdapterState` and never panic.
    /// Returns `true` when an adapter was opened by this call.
    async fn ensure_adapter(&mut self) -> bool {
        if self.adapter.is_some() {
            return false;
        }
        match open_adapter().await {
            Ok((manager, adapter, stream, state)) => {
                info!(target: BLUETOOTH, "Adapter opened");
                self.manager = Some(manager);
                self.adapter = Some(adapter);
                self.stream = Some(stream);
                let action = self.apply_availability(state).await;
                self.perform(action).await;
                true
            }
            Err(availability) => {
                let action = self.apply_availability(availability).await;
                self.perform(action).await;
                false
            }
        }
    }

    /// Records a new availability, tells the consumer when it changed (and the
    /// first time), and returns the scan action that follows from it. The send
    /// waits for room in the channel: a dropped state change would leave the UI
    /// showing a stale state, because an unchanged value is never re-sent.
    async fn apply_availability(&mut self, new: BluetoothAvailability) -> ScanAction {
        let (changed, action) = self.plan.set_availability(new);
        if changed || !self.announced {
            self.announced = true;
            info!(target: BLUETOOTH, "Availability changed: {new:?}");
            self.emit(BleEvent::AdapterState(new)).await;
        }
        action
    }

    async fn perform(&mut self, action: ScanAction) {
        if action == ScanAction::None {
            return;
        }
        let Some(adapter) = self.adapter.clone() else {
            return;
        };
        match action {
            ScanAction::None => {}
            ScanAction::Start { stop_first } => {
                if stop_first && let Err(e) = adapter.stop_scan().await {
                    warn!(target: BLUETOOTH, "Scan stop failed: {e}");
                }
                match adapter.start_scan(ScanFilter::default()).await {
                    Ok(()) => info!(target: BLUETOOTH, "Scan started ({:?})", self.plan.mode),
                    Err(e) => {
                        error!(target: BLUETOOTH, "Scan start failed: {e}");
                        self.plan.start_failed();
                    }
                }
            }
            ScanAction::Stop => {
                if let Err(e) = adapter.stop_scan().await {
                    warn!(target: BLUETOOTH, "Scan stop failed: {e}");
                } else {
                    info!(target: BLUETOOTH, "Scan stopped");
                }
            }
        }
    }

    async fn emit(&mut self, event: BleEvent) {
        if self.events.send(event).await.is_err() {
            self.closed = true;
        }
    }

    /// The btleplug event stream ended: the scan is gone and stays gone until
    /// the next start command (no restart-on-error loop).
    fn stream_ended(&mut self) {
        warn!(target: BLUETOOTH, "Adapter event stream ended; scanning stopped");
        self.stream = None;
        self.adapter = None;
        self.manager = None;
        self.plan.adapter_lost();
    }

    async fn shutdown(&mut self) {
        let action = self.plan.stop();
        self.perform(action).await;
    }
}

/// Opens the first adapter, subscribes to its events (before any scan starts)
/// and reads its current state.
async fn open_adapter()
-> Result<(Manager, Adapter, EventStream, BluetoothAvailability), BluetoothAvailability> {
    let manager = Manager::new().await.map_err(|e| {
        warn!(target: BLUETOOTH, "Bluetooth manager unavailable: {e}");
        availability_from_error(&e)
    })?;
    let adapters = manager.adapters().await.map_err(|e| {
        warn!(target: BLUETOOTH, "Adapter query failed: {e}");
        availability_from_error(&e)
    })?;
    let Some(adapter) = adapters.into_iter().next() else {
        warn!(target: BLUETOOTH, "No Bluetooth adapter found");
        return Err(BluetoothAvailability::Unsupported);
    };
    let stream = adapter.events().await.map_err(|e| {
        warn!(target: BLUETOOTH, "Adapter event subscription failed: {e}");
        BluetoothAvailability::Unknown
    })?;
    let state = match adapter.adapter_state().await {
        Ok(state) => availability_from_state(&state),
        Err(e) => {
            warn!(target: BLUETOOTH, "Adapter state query failed: {e}");
            BluetoothAvailability::Unknown
        }
    };
    Ok((manager, adapter, stream, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> DeviceId {
        DeviceId::from_address(n).unwrap()
    }

    fn powered_on_plan() -> ScanPlan {
        let mut plan = ScanPlan::default();
        plan.set_availability(BluetoothAvailability::PoweredOn);
        plan
    }

    // --- ScanPlan -------------------------------------------------------

    #[test]
    fn nothing_starts_while_the_adapter_is_not_powered_on() {
        let mut plan = ScanPlan::default();
        assert_eq!(plan.begin(Mode::Discovery), ScanAction::None);
        plan.set_availability(BluetoothAvailability::PoweredOff);
        assert_eq!(plan.begin(Mode::Monitoring(id(1))), ScanAction::None);
        assert!(!plan.scanning);
    }

    #[test]
    fn a_requested_scan_starts_when_the_adapter_powers_on() {
        let mut plan = ScanPlan::default();
        plan.set_availability(BluetoothAvailability::PoweredOff);
        plan.begin(Mode::Monitoring(id(1)));
        let (changed, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert!(changed);
        assert_eq!(action, ScanAction::Start { stop_first: false });
        assert!(plan.scanning);
    }

    #[test]
    fn power_cycle_restarts_the_active_mode_once() {
        let mut plan = powered_on_plan();
        assert_eq!(
            plan.begin(Mode::Monitoring(id(1))),
            ScanAction::Start { stop_first: false }
        );
        let (_, action) = plan.set_availability(BluetoothAvailability::PoweredOff);
        assert_eq!(action, ScanAction::None);
        assert!(!plan.scanning);
        let (_, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert_eq!(action, ScanAction::Start { stop_first: false });
        // A repeated "powered on" while scanning must not restart anything.
        let (changed, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert!(!changed);
        assert_eq!(action, ScanAction::None);
    }

    #[test]
    fn an_idle_scanner_never_starts_on_power_on() {
        let mut plan = ScanPlan::default();
        let (_, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert_eq!(action, ScanAction::None);
        assert!(!plan.scanning);
    }

    #[test]
    fn stop_ends_the_mode_so_a_power_cycle_does_not_resume_it() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Monitoring(id(1)));
        assert_eq!(plan.stop(), ScanAction::Stop);
        assert_eq!(plan.mode, Mode::Idle);
        plan.set_availability(BluetoothAvailability::PoweredOff);
        let (_, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert_eq!(action, ScanAction::None);
        // Stopping twice is a no-op.
        assert_eq!(plan.stop(), ScanAction::None);
    }

    #[test]
    fn starting_while_scanning_stops_the_running_scan_first() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Discovery);
        assert_eq!(
            plan.begin(Mode::Monitoring(id(2))),
            ScanAction::Start { stop_first: true }
        );
        assert_eq!(plan.mode, Mode::Monitoring(id(2)));
    }

    #[test]
    fn a_failed_start_is_not_counted_as_scanning() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Discovery);
        plan.start_failed();
        assert_eq!(
            plan.begin(Mode::Discovery),
            ScanAction::Start { stop_first: false }
        );
    }

    #[test]
    fn a_lost_adapter_clears_the_running_scan() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Discovery);
        plan.adapter_lost();
        assert!(!plan.scanning);
    }

    // --- route ------------------------------------------------------------

    #[test]
    fn discovery_reports_every_device_with_its_name() {
        let event = route(&Mode::Discovery, id(1), Some("Watch".into()), -60);
        assert_eq!(
            event,
            Some(BleEvent::Discovered {
                id: id(1),
                name: Some("Watch".into()),
                rssi: -60
            })
        );
    }

    #[test]
    fn an_empty_name_is_reported_as_no_name() {
        let event = route(&Mode::Discovery, id(1), Some(String::new()), -60);
        assert_eq!(
            event,
            Some(BleEvent::Discovered {
                id: id(1),
                name: None,
                rssi: -60
            })
        );
    }

    #[test]
    fn monitoring_reports_only_the_trusted_device() {
        let mode = Mode::Monitoring(id(1));
        assert_eq!(
            route(&mode, id(1), Some("Watch".into()), -70),
            Some(BleEvent::Sample {
                id: id(1),
                rssi: -70
            })
        );
        assert_eq!(route(&mode, id(2), None, -50), None);
    }

    #[test]
    fn rssi_127_is_dropped_in_every_mode() {
        assert_eq!(route(&Mode::Discovery, id(1), None, 127), None);
        assert_eq!(route(&Mode::Monitoring(id(1)), id(1), None, 127), None);
        // 126 is a (weird but) valid reading.
        assert!(route(&Mode::Discovery, id(1), None, 126).is_some());
    }

    #[test]
    fn idle_reports_nothing() {
        assert_eq!(route(&Mode::Idle, id(1), None, -50), None);
    }

    // --- availability mapping --------------------------------------------

    #[test]
    fn btleplug_states_map_to_availability() {
        assert_eq!(
            availability_from_state(&CentralState::PoweredOn),
            BluetoothAvailability::PoweredOn
        );
        assert_eq!(
            availability_from_state(&CentralState::PoweredOff),
            BluetoothAvailability::PoweredOff
        );
        assert_eq!(
            availability_from_state(&CentralState::Unknown),
            BluetoothAvailability::Unknown
        );
    }

    #[test]
    fn open_failures_map_to_the_reference_availability() {
        assert_eq!(
            availability_from_error(&btleplug::Error::PermissionDenied),
            BluetoothAvailability::Unauthorized
        );
        assert_eq!(
            availability_from_error(&btleplug::Error::NoAdapterAvailable),
            BluetoothAvailability::Unsupported
        );
        assert_eq!(
            availability_from_error(&btleplug::Error::NotConnected),
            BluetoothAvailability::Unknown
        );
    }
    // --- ScanPlan: reopening after a lost adapter ---------------------------

    #[test]
    fn reopening_after_a_lost_adapter_starts_the_new_mode_exactly_once() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Discovery);
        plan.adapter_lost();
        // `start_monitoring` after the stream ended: mode first, then the
        // reopened adapter reports powered on.
        plan.set_mode(Mode::Monitoring(id(2)));
        let (_, action) = plan.set_availability(BluetoothAvailability::PoweredOn);
        assert_eq!(action, ScanAction::Start { stop_first: false });
        // The actor does not call `restart` after a reopen; had it, this would
        // be the second start.
        assert_eq!(plan.mode, Mode::Monitoring(id(2)));
    }

    #[test]
    fn restart_on_an_open_adapter_stops_the_running_scan_first() {
        let mut plan = powered_on_plan();
        plan.begin(Mode::Discovery);
        plan.set_mode(Mode::Monitoring(id(2)));
        assert_eq!(plan.restart(), ScanAction::Start { stop_first: true });
    }

    // --- AdvertDeduper ----------------------------------------------------

    #[test]
    fn the_events_of_one_advertisement_count_as_one_reading() {
        let window = Duration::from_millis(10);
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        // Manufacturer data, service data and services of one advertisement.
        assert!(dedup.accept(&id(1), t0, window));
        assert!(!dedup.accept(&id(1), t0 + Duration::from_micros(200), window));
        assert!(!dedup.accept(&id(1), t0 + Duration::from_micros(400), window));
        // The next advertisement, one advertising interval later.
        assert!(dedup.accept(&id(1), t0 + Duration::from_millis(100), window));
    }

    #[test]
    fn the_window_is_anchored_at_the_first_event() {
        let window = Duration::from_millis(10);
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        assert!(dedup.accept(&id(1), t0, window));
        assert!(!dedup.accept(&id(1), t0 + Duration::from_millis(6), window));
        // 11 ms after the first event: a rejected event must not have moved
        // the window, so this is a new advertisement.
        assert!(dedup.accept(&id(1), t0 + Duration::from_millis(11), window));
    }

    #[test]
    fn devices_are_deduplicated_independently() {
        let window = Duration::from_millis(10);
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        assert!(dedup.accept(&id(1), t0, window));
        assert!(dedup.accept(&id(2), t0, window));
        assert!(!dedup.accept(&id(2), t0, window));
    }

    #[test]
    fn the_map_is_bounded_and_pruning_never_changes_a_decision() {
        let window = Duration::from_millis(10);
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        // A crowd of rotating addresses, each seen once, long ago.
        for n in 0..(AdvertDeduper::PRUNE_ABOVE as u64 + 50) {
            assert!(dedup.accept(&id(n + 1), t0, window));
        }
        assert!(dedup.last_accepted.len() > AdvertDeduper::PRUNE_ABOVE);
        // A device seen just now folds as before, while the stale crowd is gone.
        let later = t0 + Duration::from_secs(60);
        assert!(dedup.accept(&id(1), later, window));
        assert!(!dedup.accept(&id(1), later + Duration::from_millis(1), window));
        assert_eq!(dedup.last_accepted.len(), 1);
    }

    #[test]
    fn clear_forgets_every_device() {
        let window = Duration::from_millis(10);
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        assert!(dedup.accept(&id(1), t0, window));
        dedup.clear();
        assert!(dedup.accept(&id(1), t0, window));
    }

    #[test]
    fn a_zero_window_folds_nothing() {
        let mut dedup = AdvertDeduper::default();
        let t0 = Instant::now();
        assert!(dedup.accept(&id(1), t0, Duration::ZERO));
        assert!(dedup.accept(&id(1), t0, Duration::ZERO));
    }

    #[test]
    fn windows_folds_nothing_and_the_others_fold() {
        assert_eq!(DEDUP_WINDOW == Duration::ZERO, cfg!(target_os = "windows"));
    }

    // --- sample_source: which btleplug events are a reading ----------------

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn peripheral_id() -> PeripheralId {
        #[cfg(target_os = "macos")]
        return uuid::Uuid::nil().into();
        #[cfg(target_os = "windows")]
        return btleplug::api::BDAddr::from([1, 2, 3, 4, 5, 6]).into();
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn source_of(event: CentralEvent) -> Option<RssiSource> {
        sample_source(&event).map(|(_, source)| source)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_takes_readings_from_the_advertisement_field_events_only() {
        let id = peripheral_id();
        // One reading per advertised field, folded later by the deduper.
        assert_eq!(
            source_of(CentralEvent::ManufacturerDataAdvertisement {
                id: id.clone(),
                manufacturer_data: Default::default()
            }),
            Some(RssiSource::Properties)
        );
        assert_eq!(
            source_of(CentralEvent::ServiceDataAdvertisement {
                id: id.clone(),
                service_data: Default::default()
            }),
            Some(RssiSource::Properties)
        );
        assert_eq!(
            source_of(CentralEvent::ServicesAdvertisement {
                id: id.clone(),
                services: vec![]
            }),
            Some(RssiSource::Properties)
        );
        assert_eq!(
            source_of(CentralEvent::RssiUpdate {
                id: id.clone(),
                rssi: -60
            }),
            Some(RssiSource::Event(-60))
        );
        // Their properties carry no (fresh) RSSI on macOS.
        assert_eq!(source_of(CentralEvent::DeviceDiscovered(id.clone())), None);
        assert_eq!(source_of(CentralEvent::DeviceUpdated(id.clone())), None);
        assert_eq!(source_of(CentralEvent::DeviceConnected(id)), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_takes_readings_from_device_events_only() {
        let id = peripheral_id();
        assert_eq!(
            source_of(CentralEvent::DeviceDiscovered(id.clone())),
            Some(RssiSource::Properties)
        );
        assert_eq!(
            source_of(CentralEvent::DeviceUpdated(id.clone())),
            Some(RssiSource::Properties)
        );
        // `RssiUpdate` only fires on a change and would double-count.
        assert_eq!(
            source_of(CentralEvent::RssiUpdate {
                id: id.clone(),
                rssi: -60
            }),
            None
        );
        assert_eq!(
            source_of(CentralEvent::ServicesAdvertisement {
                id,
                services: vec![]
            }),
            None
        );
    }

    #[test]
    fn state_updates_are_never_a_reading() {
        assert!(sample_source(&CentralEvent::StateUpdate(CentralState::PoweredOn)).is_none());
    }
}
