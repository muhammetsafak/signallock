//! In-process Linux lock primitives, all over D-Bus (zbus). The command
//! fallbacks (`xflock4`, `swaylock`, `hyprlock`) spawn processes and live in
//! `lock_service.rs`.
//!
//! The order is chosen by `lock_service.rs` (ADR-014, "Lock primitives": the
//! desktop's own strategy first, then login1 with verification, then the rest).
//!
//! zbus 6 is async-only; each public function drives its own call with
//! `zbus::block_on`, so these are plain blocking functions. Call them from a
//! blocking context, not from inside an async task.

use std::time::{Duration, Instant};
use zbus::OwnedObjectPath;

use zbus::proxy::{Builder, CacheProperties};
use zbus::{Connection, Proxy};

use super::super::{PlatformError, Result};

const LOGIN1_SERVICE: &str = "org.freedesktop.login1";
const LOGIN1_MANAGER_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
pub(super) const LOGIN1_SESSION_IFACE: &str = "org.freedesktop.login1.Session";
/// logind resolves `auto` to the calling process's session.
const LOGIN1_AUTO_SESSION_PATH: &str = "/org/freedesktop/login1/session/auto";

const GNOME_SERVICE: &str = "org.gnome.ScreenSaver";
const GNOME_PATHS: &[&str] = &["/org/gnome/ScreenSaver"];
const FREEDESKTOP_SERVICE: &str = "org.freedesktop.ScreenSaver";
const FREEDESKTOP_PATHS: &[&str] = &["/org/freedesktop/ScreenSaver", "/ScreenSaver"];
const CINNAMON_SERVICE: &str = "org.cinnamon.ScreenSaver";
const CINNAMON_PATHS: &[&str] = &["/org/cinnamon/ScreenSaver"];

const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn describe(what: &str, e: zbus::Error) -> PlatformError {
    PlatformError::new(format!("{what}: {e}"))
}

/// Object path of the caller's login1 session: `$XDG_SESSION_ID` when set,
/// else logind's `auto` alias.
pub(super) async fn login1_session_path(conn: &Connection) -> zbus::Result<OwnedObjectPath> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        let manager = Proxy::new(
            conn,
            LOGIN1_SERVICE,
            LOGIN1_MANAGER_PATH,
            LOGIN1_MANAGER_IFACE,
        )
        .await?;
        if let Ok(path) = manager
            .call::<_, _, OwnedObjectPath>("GetSession", &(id.as_str(),))
            .await
        {
            return Ok(path);
        }
    }
    OwnedObjectPath::try_from(LOGIN1_AUTO_SESSION_PATH)
}

async fn login1_proxy(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    let path = login1_session_path(conn).await?;
    Proxy::new_owned(conn.clone(), LOGIN1_SERVICE, path, LOGIN1_SESSION_IFACE).await
}

/// `org.freedesktop.login1.Session.Lock`, then verification.
///
/// logind's `Lock` only emits a `Lock` signal on the session and returns
/// success at once; nothing locks unless some locker listens. So success is
/// declared only if the session's `LockedHint` turns `true` within `timeout`
/// (GNOME and KDE set it). Otherwise this returns `Err` and the caller moves
/// on to the next strategy.
pub fn lock_login1_verified(timeout: Duration) -> Result<()> {
    let proxy = zbus::block_on(async {
        let conn = Connection::system().await?;
        let path = login1_session_path(&conn).await?;
        // No property cache: every read must hit the bus.
        let proxy = Builder::<Proxy<'static>>::new(&conn)
            .destination(LOGIN1_SERVICE)
            .path(path)
            .interface(LOGIN1_SESSION_IFACE)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        proxy.call_method("Lock", &()).await?;
        Ok::<_, zbus::Error>(proxy)
    })
    .map_err(|e| describe("login1 Session.Lock", e))?;

    let deadline = Instant::now() + timeout;
    loop {
        match zbus::block_on(proxy.get_property::<bool>("LockedHint")) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(describe("login1 LockedHint", e)),
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::new(
                "login1 Session.Lock returned, but LockedHint did not become true in time",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Whether the caller has an active login1 session. Does not lock.
pub fn login1_available() -> bool {
    zbus::block_on(async {
        let conn = Connection::system().await?;
        login1_proxy(&conn)
            .await?
            .get_property::<bool>("Active")
            .await
    })
    .unwrap_or(false)
}

async fn screensaver_lock(
    service: &'static str,
    paths: &'static [&'static str],
) -> zbus::Result<()> {
    let conn = Connection::session().await?;
    let mut last_error = None;
    for path in paths {
        let proxy = Proxy::new(&conn, service, *path, service).await?;
        match proxy.call_method("Lock", &()).await {
            Ok(_) => return Ok(()),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| zbus::Error::Failure("no screensaver object path".into())))
}

async fn name_has_owner(name: &'static str) -> zbus::Result<bool> {
    let conn = Connection::session().await?;
    let name = zbus::names::BusName::try_from(name)?;
    zbus::fdo::DBusProxy::new(&conn)
        .await?
        .name_has_owner(name)
        .await
        .map_err(zbus::Error::from)
}

fn lock_screensaver(service: &'static str, paths: &'static [&'static str]) -> Result<()> {
    zbus::block_on(screensaver_lock(service, paths)).map_err(|e| describe(service, e))
}

fn screensaver_available(service: &'static str) -> bool {
    zbus::block_on(name_has_owner(service)).unwrap_or(false)
}

/// `org.gnome.ScreenSaver.Lock` on the session bus.
pub fn lock_gnome() -> Result<()> {
    lock_screensaver(GNOME_SERVICE, GNOME_PATHS)
}

/// `org.freedesktop.ScreenSaver.Lock` on the session bus (KDE and others).
pub fn lock_freedesktop() -> Result<()> {
    lock_screensaver(FREEDESKTOP_SERVICE, FREEDESKTOP_PATHS)
}

/// `org.cinnamon.ScreenSaver.Lock` on the session bus.
pub fn lock_cinnamon() -> Result<()> {
    lock_screensaver(CINNAMON_SERVICE, CINNAMON_PATHS)
}

pub fn gnome_available() -> bool {
    screensaver_available(GNOME_SERVICE)
}

pub fn freedesktop_available() -> bool {
    screensaver_available(FREEDESKTOP_SERVICE)
}

pub fn cinnamon_available() -> bool {
    screensaver_available(CINNAMON_SERVICE)
}
