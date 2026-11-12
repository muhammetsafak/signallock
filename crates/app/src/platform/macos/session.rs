//! Screen lock and unlock notifications. `com.apple.screenIsLocked` and
//! `com.apple.screenIsUnlocked` are posted on the distributed notification
//! center, as `AppState.swift` observes them.

use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationName, NSObjectProtocol, NSString,
};
use tokio::sync::mpsc;

use super::super::SessionEvent;
use crate::logging;

const LOCKED: &str = "com.apple.screenIsLocked";
const UNLOCKED: &str = "com.apple.screenIsUnlocked";

struct Ivars {
    tx: mpsc::Sender<SessionEvent>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class does not
    // implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct SessionObserver;

    impl SessionObserver {
        #[unsafe(method(screenLocked:))]
        fn screen_locked(&self, _notification: &NSNotification) {
            self.send(SessionEvent::Locked);
        }

        #[unsafe(method(screenUnlocked:))]
        fn screen_unlocked(&self, _notification: &NSNotification) {
            self.send(SessionEvent::Unlocked);
        }
    }

    unsafe impl NSObjectProtocol for SessionObserver {}
);

impl SessionObserver {
    fn new(mtm: MainThreadMarker, tx: mpsc::Sender<SessionEvent>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { tx });
        // SAFETY: `init` is NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, event: SessionEvent) {
        tracing::info!(target: logging::APP, "session event: {event:?}");
        // The receiver may be gone or full; a dropped re-arm cue is logged,
        // never fatal.
        if let Err(e) = self.ivars().tx.try_send(event) {
            tracing::warn!(target: logging::APP, "session event dropped: {e}");
        }
    }
}

/// The observer must outlive the process; the notification center holds it
/// unretained. Only the main thread creates it.
struct Keepalive(#[allow(dead_code)] Retained<SessionObserver>);

// SAFETY: the value is only stored, never touched from another thread.
unsafe impl Send for Keepalive {}
unsafe impl Sync for Keepalive {}

static OBSERVER: OnceLock<Keepalive> = OnceLock::new();

pub fn session_events() -> mpsc::Receiver<SessionEvent> {
    let (tx, rx) = mpsc::channel(8);

    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!(
            target: logging::APP,
            "session_events() must be called from the main thread; unlock re-arm is disabled"
        );
        return rx;
    };
    if OBSERVER.get().is_some() {
        tracing::error!(
            target: logging::APP,
            "session_events() was already called; this receiver stays silent"
        );
        return rx;
    }

    let observer = SessionObserver::new(mtm, tx);
    let center = NSDistributedNotificationCenter::defaultCenter();
    let locked: &NSNotificationName = &NSString::from_str(LOCKED);
    let unlocked: &NSNotificationName = &NSString::from_str(UNLOCKED);
    let target: &AnyObject = &observer;
    // SAFETY: the selectors are implemented by `SessionObserver` with the
    // `(NSNotification *)` signature, `observer` is kept alive in `OBSERVER`,
    // and the names are valid NSStrings.
    unsafe {
        center.addObserver_selector_name_object(target, sel!(screenLocked:), Some(locked), None);
        center.addObserver_selector_name_object(
            target,
            sel!(screenUnlocked:),
            Some(unlocked),
            None,
        );
    }
    let _ = OBSERVER.set(Keepalive(observer));
    rx
}
