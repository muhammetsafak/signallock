//! Manual BLE probe: runs a discovery scan for 10 seconds and logs every event.
//!
//! `cargo run -p signallock --example ble_probe`
//!
//! On macOS the first run asks for the Bluetooth permission (for the terminal
//! app that launched it). Without it, the probe logs
//! `AdapterState(Unauthorized)` and finds nothing.

use std::collections::HashSet;
use std::time::Duration;

use signallock::ble::{BleEvent, BleScanner, BtleplugScanner, display_name};
use signallock::logging;

const PROBE_DURATION: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() {
    let log_dir = std::env::temp_dir().join("signallock-ble-probe");
    let _log_guard = logging::init(&log_dir);

    let mut scanner = BtleplugScanner::new(&tokio::runtime::Handle::current());
    let Some(mut events) = scanner.take_events() else {
        tracing::error!("scanner has no event channel");
        return;
    };

    tracing::info!("Discovery scan for {} s", PROBE_DURATION.as_secs());
    scanner.start_discovery();

    let mut discovered = 0usize;
    let mut devices = HashSet::new();
    let mut last_state = None;
    let deadline = tokio::time::sleep(PROBE_DURATION);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    BleEvent::Discovered { id, name, rssi } => {
                        discovered += 1;
                        devices.insert(id.clone());
                        tracing::info!(
                            "Discovered id={id} name={:?} rssi={rssi}",
                            display_name(name.as_deref())
                        );
                    }
                    BleEvent::Sample { id, rssi } => {
                        tracing::info!("Sample id={id} rssi={rssi}");
                    }
                    BleEvent::AdapterState(state) => {
                        last_state = Some(state);
                        tracing::info!("AdapterState({state:?})");
                    }
                }
            }
        }
    }

    scanner.stop();
    // Let the BLE task process the stop before the runtime goes away.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tracing::info!(
        "Probe finished: {discovered} Discovered events from {} devices, last adapter state {}",
        devices.len(),
        match last_state {
            Some(state) => format!("{state:?}"),
            None => "none reported".to_owned(),
        }
    );
}
