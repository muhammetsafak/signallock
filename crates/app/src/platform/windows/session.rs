//! Session lock/unlock notifications through `WTSRegisterSessionNotification`.
//!
//! A hidden message-only window receives `WM_WTSSESSION_CHANGE` on a
//! dedicated thread that runs the message loop. This replaces the C# app's
//! `SystemEvents.SessionSwitch` (`windows/AppState.cs`).
//!
//! The window is a built-in `STATIC` control that is subclassed, because
//! registering a window class needs `Win32_Graphics_Gdi`, a `windows` crate
//! feature this crate does not enable.

use std::mem::transmute;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use ::windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_WNDPROC, GetMessageW,
    HWND_MESSAGE, MSG, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_WTSSESSION_CHANGE, WNDPROC, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use ::windows::core::w;
use tokio::sync::mpsc;

use super::super::SessionEvent;
use crate::logging;

static SENDER: OnceLock<mpsc::Sender<SessionEvent>> = OnceLock::new();
/// The `STATIC` control's own window procedure, replaced by [`wnd_proc`].
static ORIGINAL_PROC: AtomicIsize = AtomicIsize::new(0);

pub fn session_events() -> mpsc::Receiver<SessionEvent> {
    let (tx, rx) = mpsc::channel(8);
    if SENDER.set(tx).is_err() {
        tracing::error!(
            target: logging::APP,
            "session_events() was already called; this receiver stays silent"
        );
        return rx;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("session-events".into())
        .spawn(message_loop)
    {
        tracing::error!(
            target: logging::APP,
            "cannot start the session event thread; unlock re-arm is disabled: {e}"
        );
    }
    rx
}

fn message_loop() {
    // SAFETY: plain Win32 calls on the thread that owns the window; every
    // pointer passed is to a local that outlives the call.
    unsafe {
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("SignalLockSessionEvents"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(e) => {
                tracing::error!(
                    target: logging::APP,
                    "cannot create the session event window; unlock re-arm is disabled: {e}"
                );
                return;
            }
        };

        let original =
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wnd_proc as *const () as usize as isize);
        ORIGINAL_PROC.store(original, Ordering::Release);

        if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            tracing::error!(
                target: logging::APP,
                "WTSRegisterSessionNotification failed; unlock re-arm is disabled: {e}"
            );
            return;
        }

        let mut msg = MSG::default();
        // `GetMessageW` is 0 on WM_QUIT and -1 on error; both end the loop.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        let event = match wparam.0 as u32 {
            WTS_SESSION_LOCK => Some(SessionEvent::Locked),
            WTS_SESSION_UNLOCK => Some(SessionEvent::Unlocked),
            _ => None,
        };
        if let (Some(event), Some(tx)) = (event, SENDER.get()) {
            tracing::info!(target: logging::APP, "session event: {event:?}");
            if let Err(e) = tx.try_send(event) {
                tracing::warn!(target: logging::APP, "session event dropped: {e}");
            }
        }
        return LRESULT(0);
    }

    let original = ORIGINAL_PROC.load(Ordering::Acquire);
    // SAFETY: `original` is the pointer `SetWindowLongPtrW` returned for this
    // window, i.e. the control's own `WNDPROC`, or 0 (which becomes `None`).
    let previous = unsafe { transmute::<isize, WNDPROC>(original) };
    // SAFETY: forwards the message unchanged to the window's own procedure.
    unsafe {
        match previous {
            Some(_) => CallWindowProcW(previous, hwnd, msg, wparam, lparam),
            None => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
