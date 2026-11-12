//! In-process macOS lock primitive: `SACLockScreenImmediate` from the private
//! `login` framework (ADR-014, "Lock primitives").
//!
//! The CGSession and pmset fallbacks spawn processes and therefore live in
//! `lock_service.rs`.

use std::ffi::{CStr, CString};

use super::super::{PlatformError, Result};
use crate::logging;

/// Candidate paths for the private `login` binary, in `LockService.swift`
/// order. macOS ships these inside the dyld shared cache rather than as
/// on-disk Mach-O files, so `dlopen` resolves them and a file-existence check
/// would be misleading: the code just tries each one.
pub const LOGIN_FRAMEWORK_PATHS: [&str; 2] = [
    "/System/Library/PrivateFrameworks/login.framework/login",
    "/System/Library/PrivateFrameworks/login.framework/Versions/A/login",
];

const SYMBOL: &CStr = c"SACLockScreenImmediate";

type LockFn = unsafe extern "C" fn() -> i32;

/// Loaded `login` image; closed on drop.
struct LoginImage(*mut libc::c_void);

impl Drop for LoginImage {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful `dlopen` and is closed once.
        unsafe { libc::dlclose(self.0) };
    }
}

fn last_dl_error() -> String {
    // SAFETY: `dlerror` returns null or a NUL-terminated string valid until the
    // next dl* call on this thread; it is copied immediately.
    unsafe {
        let err = libc::dlerror();
        if err.is_null() {
            "unknown".to_owned()
        } else {
            CStr::from_ptr(err).to_string_lossy().into_owned()
        }
    }
}

/// Loads one candidate image and resolves the symbol in it.
fn open_symbol(path: &str) -> Option<(LoginImage, LockFn)> {
    let c_path = CString::new(path).expect("static path has no NUL");
    // SAFETY: `c_path` is a valid NUL-terminated string.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_LAZY) };
    if handle.is_null() {
        tracing::debug!(
            target: logging::LOCK,
            "dlopen failed at {path}: {}",
            last_dl_error()
        );
        return None;
    }
    let image = LoginImage(handle);

    // SAFETY: `image.0` is a live handle and `SYMBOL` is NUL-terminated.
    let sym = unsafe { libc::dlsym(image.0, SYMBOL.as_ptr()) };
    if sym.is_null() {
        tracing::debug!(
            target: logging::LOCK,
            "SACLockScreenImmediate not found in {path}"
        );
        return None;
    }
    // SAFETY: `SACLockScreenImmediate` is `int SACLockScreenImmediate(void)`.
    let func = unsafe { std::mem::transmute::<*mut libc::c_void, LockFn>(sym) };
    Some((image, func))
}

/// Whether the symbol can be resolved, without calling it. Used by the
/// dry-run probe and by `LockService::select_method`.
pub fn is_available() -> bool {
    LOGIN_FRAMEWORK_PATHS
        .iter()
        .any(|path| open_symbol(path).is_some())
}

/// Locks the screen through `SACLockScreenImmediate`. A non-zero return value
/// from one image moves on to the next one, as the Swift app does.
pub fn lock_screen_immediate() -> Result<()> {
    let mut last_rc = None;
    for path in LOGIN_FRAMEWORK_PATHS {
        let Some((_image, func)) = open_symbol(path) else {
            continue;
        };
        // SAFETY: the symbol was resolved from a loaded image that stays open
        // (`_image`) for the duration of the call.
        let rc = unsafe { func() };
        if rc == 0 {
            return Ok(());
        }
        tracing::error!(
            target: logging::LOCK,
            "SACLockScreenImmediate returned {rc} (path {path})"
        );
        last_rc = Some(rc);
    }
    Err(match last_rc {
        Some(rc) => PlatformError::new(format!("SACLockScreenImmediate returned {rc}")),
        None => {
            PlatformError::new("SACLockScreenImmediate is not available in the login framework")
        }
    })
}
