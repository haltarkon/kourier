//! "Other Application…" (Open in Slicer): the XDG desktop portal's
//! `org.freedesktop.portal.OpenURI.OpenFile`. The portal (KDE's own implementation under Plasma)
//! shows its application chooser for the file (`ask: true`) and starts the one the person picks.
//!
//! The caller hands an open, read-only file; the descriptor goes over the bus, so the portal
//! needs no path and a sandboxed program gets the file through the document portal. One call on
//! the session bus, made by the caller on a thread of its own; this crate never logs a path
//! (§9.1). Without a portal ([`DesktopError::NoService`]) the caller falls back to the desktop's
//! default application (`kourier_app::opener`).

use std::collections::HashMap;
use std::fs::File;

use zbus::zvariant::{Fd, Value};

use crate::{Bus, DesktopError};

/// The portal's bus name.
pub const PORTAL: &str = "org.freedesktop.portal.Desktop";
/// Its object.
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// The interface that opens files.
pub const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";

/// Asks the portal to open `file` (read-only, never writable) and to let the person choose the
/// application (`ask: true`). `Ok` means the portal took the request (the chooser may still be
/// cancelled); a portal that is not on the bus and cannot be started is
/// [`DesktopError::NoService`].
pub async fn open_file_asking(bus: &Bus, file: &File) -> Result<(), DesktopError> {
    if !bus.has_owner(PORTAL).await? && !is_activatable(bus).await? {
        return Err(DesktopError::NoService("desktop portal"));
    }
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("ask", Value::from(true));
    options.insert("writable", Value::from(false));
    // The reply is the request's object path; nothing waits for the person's choice.
    bus.connection()
        .call_method(
            Some(PORTAL),
            PORTAL_PATH,
            Some(OPEN_URI),
            "OpenFile",
            &("", Fd::from(file), options),
        )
        .await?;
    Ok(())
}

/// Whether the bus can start the portal by itself.
async fn is_activatable(bus: &Bus) -> Result<bool, DesktopError> {
    let dbus = zbus::fdo::DBusProxy::new(bus.connection()).await?;
    Ok(dbus
        .list_activatable_names()
        .await?
        .iter()
        .any(|n| n.as_str() == PORTAL))
}
