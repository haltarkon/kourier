//! The tray icon (architecture §8.8): a StatusNotifierItem through ksni with a Show and a Quit
//! entry; a click toggles the window; unread messages switch it to `NeedsAttention`.
//!
//! On Wayland the host hands an XDG activation token over (`ProvideXdgActivationToken`) right
//! before `Activate` or a menu event; the token travels with the [`TrayEvent`] so the window
//! may raise itself.
//!
//! ksni opens its own session-bus connection (`DBUS_SESSION_BUS_ADDRESS`).

use ksni::menu::StandardItem;
use ksni::{MenuItem, OfflineReason, Status, ToolTip, TrayMethods as _};
use tokio::sync::mpsc;

use crate::DesktopError;

/// An ARGB32 image (network byte order) for the tray.
#[derive(Clone, PartialEq, Eq)]
pub struct TrayIcon {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// `width * height * 4` bytes, A R G B per pixel.
    pub argb: Vec<u8>,
}

impl std::fmt::Debug for TrayIcon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrayIcon")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// What the tray shows; the labels are translated by the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayConfig {
    /// The item id (the app id).
    pub id: String,
    /// The title ("Kourier").
    pub title: String,
    /// An icon theme name (used when `icons` is empty).
    pub icon_name: String,
    /// The normal icon in several sizes.
    pub icons: Vec<TrayIcon>,
    /// The icon while unread messages wait.
    pub attention_icons: Vec<TrayIcon>,
    /// "Show Kourier".
    pub show_label: String,
    /// "Quit".
    pub quit_label: String,
}

/// The tray's changing state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayState {
    /// Unread messages (attention when > 0).
    pub unread: u32,
    /// The tooltip line under the title ("3 unread messages").
    pub tooltip: String,
}

/// What the user did with the tray.
#[derive(Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// A click on the icon: toggle the window.
    Activate {
        /// The XDG activation token the host provided.
        token: Option<String>,
    },
    /// The Show entry.
    Show {
        /// The XDG activation token the host provided.
        token: Option<String>,
    },
    /// The Quit entry.
    Quit,
    /// The tray host appeared (`true`) or went away (`false`).
    Available(bool),
}

impl std::fmt::Debug for TrayEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Activate { token } => f
                .debug_struct("Activate")
                .field("token", &token.is_some())
                .finish(),
            Self::Show { token } => f
                .debug_struct("Show")
                .field("token", &token.is_some())
                .finish(),
            Self::Quit => f.write_str("Quit"),
            Self::Available(a) => f.debug_tuple("Available").field(a).finish(),
        }
    }
}

struct Item {
    config: TrayConfig,
    state: TrayState,
    token: Option<String>,
    events: mpsc::UnboundedSender<TrayEvent>,
}

fn icons(icons: &[TrayIcon]) -> Vec<ksni::Icon> {
    icons
        .iter()
        .map(|i| ksni::Icon {
            width: i.width,
            height: i.height,
            data: i.argb.clone(),
        })
        .collect()
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        self.config.id.clone()
    }

    fn title(&self) -> String {
        self.config.title.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let token = self.token.take();
        let _ = self.events.send(TrayEvent::Activate { token });
    }

    fn provide_xdg_activation_token(&mut self, token: String) {
        self.token = Some(token);
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn status(&self) -> Status {
        if self.state.unread > 0 {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    fn icon_name(&self) -> String {
        if self.config.icons.is_empty() {
            self.config.icon_name.clone()
        } else {
            String::new()
        }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icons(&self.config.icons)
    }

    fn attention_icon_name(&self) -> String {
        if self.config.attention_icons.is_empty() {
            self.config.icon_name.clone()
        } else {
            String::new()
        }
    }

    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        icons(&self.config.attention_icons)
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
            title: self.config.title.clone(),
            description: self.state.tooltip.clone(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.config.show_label.clone(),
                icon_name: "window".into(),
                activate: Box::new(|t: &mut Self| {
                    let token = t.token.take();
                    let _ = t.events.send(TrayEvent::Show { token });
                }),
                ..StandardItem::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: self.config.quit_label.clone(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.events.send(TrayEvent::Quit);
                }),
                ..StandardItem::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        let _ = self.events.send(TrayEvent::Available(true));
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        tracing::info!(?reason, "the tray host went away");
        let _ = self.events.send(TrayEvent::Available(false));
        // Keep the item: the watcher usually comes back (plasmashell restart).
        true
    }
}

/// A running tray item. Dropping it keeps the item until [`Tray::shutdown`].
pub struct Tray {
    handle: ksni::Handle<Item>,
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray").finish_non_exhaustive()
    }
}

impl Tray {
    /// Registers the item with the StatusNotifierWatcher of the session bus. Fails when there
    /// is no watcher or no host. Must run inside the tokio runtime.
    pub async fn spawn(
        config: TrayConfig,
        events: mpsc::UnboundedSender<TrayEvent>,
    ) -> Result<Self, DesktopError> {
        let item = Item {
            config,
            state: TrayState::default(),
            token: None,
            events,
        };
        let handle = item
            .spawn()
            .await
            .map_err(|e| DesktopError::NoTray(e.to_string()))?;
        Ok(Self { handle })
    }

    /// Updates the unread count and the tooltip.
    pub async fn set_state(&self, state: TrayState) {
        let _ = self
            .handle
            .update(|item| {
                item.state = state;
            })
            .await;
    }

    /// Removes the item from the tray.
    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}
