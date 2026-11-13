//! Windows services: lock (`LockWorkStation`), autostart (registry `Run` key),
//! legacy file migration and the session lock/unlock notifications.
//!
//! `autostart`, `migrate` and `update` (installer staging, ADR-015) hold
//! portable logic that is also compiled under `cfg(test)` on other platforms;
//! their Win32 parts are gated on `target_os = "windows"`.

pub mod autostart;
#[cfg(target_os = "windows")]
pub mod lock;
pub mod migrate;
#[cfg(target_os = "windows")]
pub mod session;
pub mod update;
