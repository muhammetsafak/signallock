//! BLE layer: `BleEvent`, `DeviceId`, `BleScanner` and the btleplug
//! implementation. Nothing outside this module imports btleplug.
//!
//! Two scan modes, never merged (`.ssot/02-architecture-app.md` rule 3):
//!
//! - **discovery** (device picker): every peripheral, reported as
//!   `BleEvent::Discovered { id, name, rssi }`;
//! - **monitoring** (proximity): only the trusted device, reported as
//!   `BleEvent::Sample { id, rssi }`.
//!
//! `main.rs` still declares this module privately and nothing consumes it
//! there until the `AppState` phase (Faz 05), hence the two allowances below;
//! the library target (`lib.rs`) exports it for tests and `examples/ble_probe.rs`.

#![allow(dead_code)]

mod btleplug_scanner;
mod id;

#[allow(unused_imports)]
pub use btleplug_scanner::BtleplugScanner;
#[allow(unused_imports)]
pub use id::{DeviceId, ParseDeviceIdError};

use tokio::sync::mpsc;

/// What the picker shows for a peripheral that advertises no name
/// (`"Unknown Device"` in both references).
pub const UNKNOWN_DEVICE_NAME: &str = "Unknown Device";

/// State of the Bluetooth adapter, as the menu and the settings UI need it.
///
/// Mirrors `BluetoothAvailability` in both references (`ready` is called
/// `PoweredOn` here). What a platform can report differs: CoreBluetooth and
/// BlueZ never say `Unauthorized` through btleplug once an adapter exists, and
/// Windows reports a disabled radio as `Unknown`; see `btleplug_scanner.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BluetoothAvailability {
    PoweredOn,
    PoweredOff,
    Unauthorized,
    Unsupported,
    #[default]
    Unknown,
}

/// An event from the scanner task. Delivered over the receiver returned by
/// [`BleScanner::take_events`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BleEvent {
    /// Discovery mode: an advertisement from `id`. `name` is `None` when the
    /// device advertises none; the UI then shows [`UNKNOWN_DEVICE_NAME`]
    /// (see [`display_name`]).
    Discovered {
        id: DeviceId,
        name: Option<String>,
        rssi: i16,
    },
    /// Monitoring mode: an RSSI reading for the trusted device.
    Sample { id: DeviceId, rssi: i16 },
    /// The adapter changed state (also sent once when the scanner starts).
    AdapterState(BluetoothAvailability),
}

/// The name to show for a discovered device.
pub fn display_name(name: Option<&str>) -> &str {
    match name {
        Some(name) if !name.is_empty() => name,
        _ => UNKNOWN_DEVICE_NAME,
    }
}

/// The scanner as `AppState` (Faz 05) sees it. Commands never block and never
/// fail: problems are logged and surface as [`BleEvent::AdapterState`]. Tests
/// inject a fake implementation.
pub trait BleScanner {
    /// Start (or restart) a discovery scan: every peripheral is reported.
    fn start_discovery(&self);

    /// Start (or restart) a monitoring scan: only `id` is reported.
    fn start_monitoring(&self, id: DeviceId);

    /// Stop scanning. Adapter state changes are still reported.
    fn stop(&self);

    /// The event channel. Returns the receiver the first time and `None`
    /// afterwards.
    fn take_events(&mut self) -> Option<mpsc::Receiver<BleEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unnamed_devices_show_the_reference_placeholder() {
        assert_eq!(display_name(None), "Unknown Device");
        assert_eq!(display_name(Some("")), "Unknown Device");
        assert_eq!(display_name(Some("AirPods Pro")), "AirPods Pro");
    }
}
