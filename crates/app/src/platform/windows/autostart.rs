//! Login autostart through `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! Per-user, no admin rights. Mirrors `windows/Services/LoginItemService.cs`.
//!
//! The value name must stay `SignalLock`: the installer's uninstall step
//! deletes exactly that value (`windows/installer/SignalLock.iss`,
//! `[UninstallRun]`).

use std::path::Path;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))] // only the registry code reads it
pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "SignalLock";

/// The registry value for `exe`: the path in double quotes, so that a path
/// with spaces (`C:\Program Files\...`) launches correctly.
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(target_os = "windows")]
pub use imp::WindowsAutostart;

#[cfg(target_os = "windows")]
mod imp {
    use std::io;

    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};

    use super::{RUN_KEY_PATH, VALUE_NAME, run_value};
    use crate::logging;
    use crate::platform::{Autostart, PlatformError, Result};

    #[derive(Debug, Default, Clone, Copy)]
    pub struct WindowsAutostart;

    impl WindowsAutostart {
        pub fn new() -> Self {
            Self
        }
    }

    impl Autostart for WindowsAutostart {
        fn is_enabled(&self) -> bool {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let key = match hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_READ) {
                Ok(key) => key,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
                Err(e) => {
                    tracing::warn!(target: logging::APP, "Login-item check failed: {e}");
                    return false;
                }
            };
            match key.get_value::<String, _>(VALUE_NAME) {
                Ok(value) => !value.trim().is_empty(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => {
                    tracing::warn!(target: logging::APP, "Login-item check failed: {e}");
                    false
                }
            }
        }

        fn set_enabled(&self, on: bool) -> Result<()> {
            let hkcu = RegKey::predef(HKEY_CURRENT_USER);
            let result = if on {
                let exe = std::env::current_exe().map_err(|e| {
                    PlatformError::new(format!("cannot locate the executable: {e}"))
                })?;
                hkcu.create_subkey_with_flags(RUN_KEY_PATH, KEY_SET_VALUE)
                    .and_then(|(key, _)| key.set_value(VALUE_NAME, &run_value(&exe)))
            } else {
                match hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_SET_VALUE) {
                    Ok(key) => match key.delete_value(VALUE_NAME) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                        other => other,
                    },
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                }
            };
            result.map_err(|e| {
                let message = format!("login item toggle failed: {e}");
                tracing::warn!(target: logging::APP, "{message}");
                PlatformError::new(message)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_quotes_the_path() {
        let value = run_value(Path::new(r"C:\Program Files\SignalLock\signallock.exe"));
        assert_eq!(value, "\"C:\\Program Files\\SignalLock\\signallock.exe\"");
    }

    /// Pins the literal only. The installer that deletes this value lives in
    /// the `macos`/`windows` repository (`windows/installer/SignalLock.iss`,
    /// `[UninstallRun]` runs `reg delete ... /v SignalLock`), out of reach of a
    /// test in this repository: changing the name here means changing the
    /// installer too, and this test failing is the reminder.
    #[test]
    fn value_name_is_pinned_to_the_one_the_installer_removes() {
        assert_eq!(VALUE_NAME, "SignalLock");
    }
}
