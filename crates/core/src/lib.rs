//! Platform-independent core of SignalLock.
//!
//! This crate has no platform API, no async runtime and no clock reads. It
//! owns three things:
//!
//! - [`proximity`]: the proximity decision engine (ADR-005, ADR-010),
//! - [`settings`]: the persisted models and their value ranges,
//! - [`store`]: the two-file JSON store (`settings.json`, `trusted-device.json`).

pub mod proximity;
pub mod settings;
pub mod store;

pub use proximity::{Decision, ProximityMonitor, ProximityState};
pub use settings::{AppSettings, TrustedDevice};
pub use store::Store;
