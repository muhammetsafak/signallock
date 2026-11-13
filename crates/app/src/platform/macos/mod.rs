//! macOS services: lock (private `login` framework), autostart
//! (`SMAppService`), legacy `UserDefaults` migration and the screen
//! lock/unlock notifications, and the in-app update install (ADR-015).

pub mod autostart;
pub mod lock;
pub mod migrate;
pub mod session;
pub mod update;
