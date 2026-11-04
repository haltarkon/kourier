//! Show a saved file in the user's file manager: `org.freedesktop.FileManager1.ShowItems`
//! (Dolphin, Nautilus and the others implement it: the folder opens with the file selected).
//!
//! One call on the session bus, made by the caller on a thread of its own; this crate never logs
//! the URI (a path is personal data, §9.1). When the service is not there ([`DesktopError`]) the
//! caller falls back to opening the folder (`kourier_app::opener`).

use crate::{Bus, DesktopError};

/// The file manager's bus name.
pub const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
/// Its object.
pub const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";

/// Asks the file manager to show the file at `uri` (a `file://` URI) selected in its folder.
/// A name that has no owner and that the bus cannot start either is
/// [`DesktopError::NoService`] (the caller opens the folder another way); a file manager that is
/// not running but can be started (Dolphin ships a D-Bus service file for it) is started by the
/// call.
pub async fn show_items(bus: &Bus, uri: &str) -> Result<(), DesktopError> {
    if !bus.has_owner(FILE_MANAGER).await? && !is_activatable(bus).await? {
        return Err(DesktopError::NoService("file manager"));
    }
    bus.connection()
        .call_method(
            Some(FILE_MANAGER),
            FILE_MANAGER_PATH,
            Some("org.freedesktop.FileManager1"),
            "ShowItems",
            &(vec![uri], ""),
        )
        .await?;
    Ok(())
}

/// Whether the bus can start the file manager's service by itself.
async fn is_activatable(bus: &Bus) -> Result<bool, DesktopError> {
    let dbus = zbus::fdo::DBusProxy::new(bus.connection()).await?;
    Ok(dbus
        .list_activatable_names()
        .await?
        .iter()
        .any(|n| n.as_str() == FILE_MANAGER))
}
