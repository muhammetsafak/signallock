//! Linux services: lock (login1 and the desktop screensavers over D-Bus),
//! autostart (`.desktop` file in `$XDG_CONFIG_HOME/autostart`), the session
//! lock/unlock signals, and a no-op legacy migration (there was no Linux app
//! before this one).
//!
//! `autostart` and `migrate` hold portable logic that is also compiled under
//! `cfg(test)` on other platforms; the D-Bus parts are gated on
//! `target_os = "linux"`.

pub mod autostart;
#[cfg(target_os = "linux")]
pub mod lock;
pub mod migrate;
#[cfg(target_os = "linux")]
pub mod session;
