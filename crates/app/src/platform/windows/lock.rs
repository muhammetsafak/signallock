//! `LockWorkStation` (user32), the only Windows strategy; the C# app has no
//! fallback either (`windows/Services/LockService.cs`).

use ::windows::Win32::System::Shutdown::LockWorkStation;

use super::super::{PlatformError, Result};

pub fn lock_workstation() -> Result<()> {
    // SAFETY: `LockWorkStation` takes no arguments and has no preconditions.
    unsafe { LockWorkStation() }
        .map_err(|e| PlatformError::new(format!("LockWorkStation failed: {e}")))
}
