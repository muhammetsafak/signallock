//! SignalLock desktop app library.
//!
//! Holds every module of the `signallock` binary so that unit tests
//! (`cargo test -p signallock --lib`) and the manual probes under `examples/`
//! can reach them. `main.rs` is a thin entry point that consumes this library.

pub mod app_state;
pub mod ble;
pub mod lock_service;
pub mod logging;
pub mod platform;
pub mod tray;
pub mod ui;
