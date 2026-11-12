//! Login item through `SMAppService.mainApp` (macOS 13+). Mirrors
//! `LoginItemService.swift`. Registration only works from the signed `.app`
//! bundle; from a bare `cargo run` binary `register` fails and is reported as
//! an error.

use objc2_service_management::{SMAppService, SMAppServiceStatus};

use super::super::{Autostart, PlatformError, Result};
use crate::logging;

#[derive(Debug, Default, Clone, Copy)]
pub struct MacAutostart;

impl MacAutostart {
    pub fn new() -> Self {
        Self
    }

    fn status() -> SMAppServiceStatus {
        // SAFETY: `mainAppService` and `status` have no preconditions.
        unsafe { SMAppService::mainAppService().status() }
    }
}

impl Autostart for MacAutostart {
    fn is_enabled(&self) -> bool {
        Self::status() == SMAppServiceStatus::Enabled
    }

    fn set_enabled(&self, on: bool) -> Result<()> {
        let enabled = self.is_enabled();
        if on == enabled {
            return Ok(());
        }
        // SAFETY: `mainAppService` has no preconditions.
        let service = unsafe { SMAppService::mainAppService() };
        // SAFETY: `registerAndReturnError` / `unregisterAndReturnError` have no
        // preconditions; the error is returned, not thrown.
        let result = unsafe {
            if on {
                service.registerAndReturnError()
            } else {
                service.unregisterAndReturnError()
            }
        };
        result.map_err(|e| {
            let message = format!(
                "login item {} failed: {}",
                if on { "register" } else { "unregister" },
                e.localizedDescription()
            );
            tracing::warn!(target: logging::APP, "{message}");
            PlatformError::new(message)
        })
    }
}
