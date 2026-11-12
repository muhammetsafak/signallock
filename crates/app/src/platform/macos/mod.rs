//! macOS services: lock (private `login` framework), autostart
//! (`SMAppService`), legacy `UserDefaults` migration and the screen
//! lock/unlock notifications.

pub mod autostart;
pub mod lock;
pub mod migrate;
pub mod session;
