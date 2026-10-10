//! Desktop integration over D-Bus (architecture §8.7, §8.8): notifications, single instance,
//! tray, autostart and the Secret Service.
//!
//! - [`notifications`]: an own client for `org.freedesktop.Notifications` with the KDE extensions
//!   (inline reply, `ActivationToken`, `x-kde-origin-name`) and `image-data`.
//! - [`instance`]: single instance through a well-known bus name and
//!   `org.freedesktop.Application` (`Activate`, `Open`, `ActivateAction`).
//! - [`tray`]: a StatusNotifierItem (ksni) with Show/Quit, activation tokens and an attention
//!   state.
//! - [`autostart`]: the XDG autostart entry.
//! - [`hints`]: resume from suspend (logind) and network changes (NetworkManager) on the system
//!   bus, as hints for the backends to reconnect (§4.7).
//! - [`secret`]: the Secret Service client for the master key (§5.2).
//! - `testing` (cargo feature `testing`): a private bus and mock desktop services for tests.
//!
//! Nothing here knows about conversations or messages: `kourier-app` decides what to show and
//! hands over plain strings and pixels. Log safety (§9.1): this crate never logs notification
//! texts, reply texts, URIs or activation tokens.

pub mod autostart;
pub mod hints;
pub mod instance;
pub mod notifications;
pub mod secret;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tray;

/// The application id (§8.8): desktop file name, Wayland `app_id`, icon name, the default
/// D-Bus name and the notification `desktop-entry` hint.
pub const APP_ID: &str = "app.kourier.Kourier";

/// Why a desktop service call failed. Log-safe: D-Bus errors name methods and services, never
/// the strings Kourier sent.
#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    /// No session bus (or the given address cannot be reached).
    #[error("no D-Bus session bus: {0}")]
    NoBus(String),
    /// A service is missing on the bus.
    #[error("no {0} on the session bus")]
    NoService(&'static str),
    /// A D-Bus call or registration failed.
    #[error("D-Bus error: {0}")]
    Dbus(String),
    /// The tray could not be registered (no StatusNotifierWatcher, or no host shows items).
    #[error("no system tray: {0}")]
    NoTray(String),
    /// A file could not be written or removed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<zbus::Error> for DesktopError {
    fn from(e: zbus::Error) -> Self {
        DesktopError::Dbus(e.to_string())
    }
}

impl From<zbus::fdo::Error> for DesktopError {
    fn from(e: zbus::fdo::Error) -> Self {
        DesktopError::Dbus(e.to_string())
    }
}

/// A D-Bus connection: the session bus, the system bus, or a bus at an explicit address
/// (tests). Cheap to
/// clone; the connection closes when the last clone is dropped.
#[derive(Clone, Debug)]
pub struct Bus(zbus::Connection);

impl Bus {
    /// Connects to the session bus (`DBUS_SESSION_BUS_ADDRESS`). Must run inside the tokio
    /// runtime.
    pub async fn session() -> Result<Self, DesktopError> {
        zbus::Connection::session()
            .await
            .map(Self)
            .map_err(|e| DesktopError::NoBus(e.to_string()))
    }

    /// Connects to the system bus (`DBUS_SYSTEM_BUS_ADDRESS`, else the well-known socket),
    /// where logind and NetworkManager live. Must run inside the tokio runtime.
    pub async fn system() -> Result<Self, DesktopError> {
        zbus::Connection::system()
            .await
            .map(Self)
            .map_err(|e| DesktopError::NoBus(e.to_string()))
    }

    /// Connects to the message bus at `address` (a D-Bus address string).
    pub async fn at(address: &str) -> Result<Self, DesktopError> {
        let builder = zbus::connection::Builder::address(address)
            .map_err(|e| DesktopError::NoBus(e.to_string()))?;
        builder
            .build()
            .await
            .map(Self)
            .map_err(|e| DesktopError::NoBus(e.to_string()))
    }

    /// This connection's unique name (`:1.42`).
    pub fn unique_name(&self) -> Option<String> {
        self.0.unique_name().map(ToString::to_string)
    }

    /// Whether `name` currently has an owner on this bus.
    pub async fn has_owner(&self, name: &str) -> Result<bool, DesktopError> {
        let dbus = zbus::fdo::DBusProxy::new(&self.0).await?;
        let name =
            zbus::names::BusName::try_from(name).map_err(|e| DesktopError::Dbus(e.to_string()))?;
        Ok(dbus.name_has_owner(name).await?)
    }

    pub(crate) fn connection(&self) -> &zbus::Connection {
        &self.0
    }
}
