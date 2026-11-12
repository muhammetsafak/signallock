//! Session lock/unlock events from logind. The caller's login1 session emits
//! `Lock` and `Unlock` signals, and desktops that lock themselves (GNOME)
//! flip its `LockedHint` property instead, so both are watched. Repeated
//! states are collapsed: only changes are forwarded.

use futures::StreamExt;
use futures::stream;
use tokio::sync::mpsc;
use zbus::{Connection, Proxy};

use super::super::SessionEvent;
use super::lock::{LOGIN1_SESSION_IFACE, login1_session_path};
use crate::logging;

pub fn session_events() -> mpsc::Receiver<SessionEvent> {
    let (tx, rx) = mpsc::channel(8);
    let spawned = std::thread::Builder::new()
        .name("session-events".into())
        .spawn(move || {
            if let Err(e) = zbus::block_on(watch(tx)) {
                tracing::warn!(
                    target: logging::APP,
                    "login1 session watch ended; unlock re-arm is disabled: {e}"
                );
            }
        });
    if let Err(e) = spawned {
        tracing::error!(
            target: logging::APP,
            "cannot start the session event thread; unlock re-arm is disabled: {e}"
        );
    }
    rx
}

enum Input<'a> {
    Event(SessionEvent),
    Hint(zbus::proxy::PropertyChanged<'a, bool>),
}

async fn watch(tx: mpsc::Sender<SessionEvent>) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let path = login1_session_path(&conn).await?;
    let proxy = Proxy::new(&conn, "org.freedesktop.login1", path, LOGIN1_SESSION_IFACE).await?;

    let locks = proxy
        .receive_signal("Lock")
        .await?
        .map(|_| Input::Event(SessionEvent::Locked));
    let unlocks = proxy
        .receive_signal("Unlock")
        .await?
        .map(|_| Input::Event(SessionEvent::Unlocked));
    let hints = proxy
        .receive_property_changed::<bool>("LockedHint")
        .await
        .map(Input::Hint);
    let mut inputs = stream::select(stream::select(locks, unlocks), hints);

    let mut last = None;
    while let Some(input) = inputs.next().await {
        let event = match input {
            Input::Event(event) => event,
            Input::Hint(changed) => match changed.get().await {
                Ok(true) => SessionEvent::Locked,
                Ok(false) => SessionEvent::Unlocked,
                Err(_) => continue,
            },
        };
        if last == Some(event) {
            continue;
        }
        last = Some(event);
        tracing::info!(target: logging::APP, "session event: {event:?}");
        if tx.send(event).await.is_err() {
            break;
        }
    }
    Ok(())
}
