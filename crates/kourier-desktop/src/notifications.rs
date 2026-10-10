//! A client for `org.freedesktop.Notifications` (Desktop Notifications spec 1.2) with the KDE
//! extensions Plasma implements (architecture §8.7, research qt-kirigami §7.2):
//!
//! - hints `desktop-entry`, `category`, `urgency`, `image-data`, `x-kde-origin-name`,
//!   `x-kde-reply-placeholder-text`, `x-kde-reply-submit-button-text`;
//! - the `inline-reply` action and the `NotificationReplied(id, text)` signal;
//! - `ActivationToken(id, token)`, which the server sends right before `ActionInvoked`.
//!
//! [`Notification::body`] is plain text. When the server announces `body-markup`, the client
//! escapes it, so text from the network can never inject markup or links.

use std::collections::HashMap;

use futures_util::StreamExt as _;
use tokio::sync::mpsc;
use zbus::zvariant::Value;

use crate::{Bus, DesktopError};

/// The bus name of the notification server.
pub const SERVICE: &str = "org.freedesktop.Notifications";
/// Its object path.
pub const PATH: &str = "/org/freedesktop/Notifications";
/// The action key a click on the notification body invokes.
pub const DEFAULT_ACTION: &str = "default";
/// The KDE inline-reply action key.
pub const INLINE_REPLY_ACTION: &str = "inline-reply";

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn activation_token(&self, id: u32, activation_token: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_replied(&self, id: u32, text: String) -> zbus::Result<()>;
}

/// How urgent a notification is (`urgency` hint).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    /// Low.
    Low,
    /// Normal (the default).
    #[default]
    Normal,
    /// Critical: stays until dismissed.
    Critical,
}

impl Urgency {
    fn byte(self) -> u8 {
        match self {
            Urgency::Low => 0,
            Urgency::Normal => 1,
            Urgency::Critical => 2,
        }
    }
}

/// Raw pixels for the `image-data` hint (spec type `(iiibiiay)`).
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Bytes per row.
    pub rowstride: i32,
    /// Whether there is an alpha channel.
    pub has_alpha: bool,
    /// Bits per sample (8).
    pub bits_per_sample: i32,
    /// Channels (4 with alpha, 3 without).
    pub channels: i32,
    /// Row-major samples, `rowstride * height` bytes.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageData")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl ImageData {
    /// Tightly packed 8-bit RGBA pixels. `None` when `data` does not hold `width * height`
    /// pixels or a dimension is zero or too large.
    pub fn rgba(width: u32, height: u32, data: Vec<u8>) -> Option<Self> {
        let w = i32::try_from(width).ok().filter(|w| *w > 0 && *w <= 4096)?;
        let h = i32::try_from(height)
            .ok()
            .filter(|h| *h > 0 && *h <= 4096)?;
        let rowstride = w.checked_mul(4)?;
        let len = usize::try_from(rowstride.checked_mul(h)?).ok()?;
        (data.len() == len).then_some(Self {
            width: w,
            height: h,
            rowstride,
            has_alpha: true,
            bits_per_sample: 8,
            channels: 4,
            data,
        })
    }
}

/// An inline reply field (KDE extension).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InlineReply {
    /// The placeholder of the empty text field.
    pub placeholder: String,
    /// The label of the button that sends the reply (empty: the server's default).
    pub submit_label: String,
}

/// One notification to show or replace. Strings are plain text.
#[derive(Clone, Default, PartialEq)]
pub struct Notification {
    /// The id of a notification this one replaces (0 = a new one).
    pub replaces_id: u32,
    /// The application name the server shows.
    pub app_name: String,
    /// An icon name; empty uses the desktop entry's icon.
    pub icon: String,
    /// The title line.
    pub summary: String,
    /// The text (plain; escaped for servers with `body-markup`).
    pub body: String,
    /// Whether a click on the notification invokes [`DEFAULT_ACTION`].
    pub default_action: bool,
    /// Extra buttons as `(key, label)`.
    pub actions: Vec<(String, String)>,
    /// An inline reply field, when the server supports it.
    pub inline_reply: Option<InlineReply>,
    /// The `desktop-entry` hint (the app id).
    pub desktop_entry: Option<String>,
    /// The `category` hint (`im.received`).
    pub category: Option<String>,
    /// The `x-kde-origin-name` hint: which account this is about when there are several.
    pub origin_name: Option<String>,
    /// The `image-data` hint (an avatar).
    pub image: Option<ImageData>,
    /// The `urgency` hint.
    pub urgency: Urgency,
    /// Milliseconds until it expires; -1 = the server's default, 0 = never.
    pub timeout_ms: i32,
}

// Summaries, bodies, origins and images are personal data (§9.1).
impl std::fmt::Debug for Notification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notification")
            .field("replaces_id", &self.replaces_id)
            .field("default_action", &self.default_action)
            .field("actions", &self.actions.len())
            .field("inline_reply", &self.inline_reply.is_some())
            .field("category", &self.category)
            .field("image", &self.image.is_some())
            .field("urgency", &self.urgency)
            .finish_non_exhaustive()
    }
}

/// Why the server closed a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// It expired.
    Expired,
    /// The user dismissed it.
    Dismissed,
    /// `CloseNotification` closed it.
    Closed,
    /// Any other reason.
    Other,
}

impl CloseReason {
    fn from_code(code: u32) -> Self {
        match code {
            1 => CloseReason::Expired,
            2 => CloseReason::Dismissed,
            3 => CloseReason::Closed,
            _ => CloseReason::Other,
        }
    }
}

/// What the server reports about notifications (every server-side notification, filter by
/// the ids [`NotificationClient::notify`] returned).
#[derive(Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// An action was invoked (`default` = a click on the notification).
    Action {
        /// The notification.
        id: u32,
        /// The action key.
        key: String,
    },
    /// An XDG activation token for the action that follows.
    ActivationToken {
        /// The notification.
        id: u32,
        /// The token (secret-ish: never logged).
        token: String,
    },
    /// The user sent an inline reply.
    Replied {
        /// The notification.
        id: u32,
        /// The reply text (personal data).
        text: String,
    },
    /// The notification is gone.
    Closed {
        /// The notification.
        id: u32,
        /// Why.
        reason: CloseReason,
    },
}

impl std::fmt::Debug for NotificationEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Action { id, key } => f
                .debug_struct("Action")
                .field("id", id)
                .field("key", key)
                .finish(),
            Self::ActivationToken { id, .. } => f
                .debug_struct("ActivationToken")
                .field("id", id)
                .finish_non_exhaustive(),
            Self::Replied { id, .. } => f
                .debug_struct("Replied")
                .field("id", id)
                .finish_non_exhaustive(),
            Self::Closed { id, reason } => f
                .debug_struct("Closed")
                .field("id", id)
                .field("reason", reason)
                .finish(),
        }
    }
}

/// The server's capabilities (`GetCapabilities`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities(Vec<String>);

impl Capabilities {
    /// Whether the server announced `name`.
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|c| c == name)
    }

    /// Action buttons and the default action.
    pub fn actions(&self) -> bool {
        self.has("actions")
    }

    /// The body is interpreted as markup.
    pub fn body_markup(&self) -> bool {
        self.has("body-markup")
    }

    /// KDE inline replies.
    pub fn inline_reply(&self) -> bool {
        self.has("inline-reply")
    }

    /// The raw list.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// Escapes text for a server that interprets the body as markup.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A connection to the notification server.
#[derive(Clone, Debug)]
pub struct NotificationClient {
    proxy: NotificationsProxy<'static>,
    caps: Capabilities,
    owner: Option<String>,
}

/// Whether a call failed because nobody owns (or can be started for) the called name.
fn is_no_service(e: &zbus::Error) -> bool {
    const NAMES: [&str; 2] = [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
    ];
    match e {
        zbus::Error::MethodError(name, _, _) => NAMES.contains(&name.as_str()),
        zbus::Error::FDO(e) => matches!(
            **e,
            zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
        ),
        _ => false,
    }
}

impl NotificationClient {
    /// Connects to the notification server on `bus` and reads its capabilities.
    ///
    /// The first call may let the bus start the server or wait for it (D-Bus activation):
    /// Plasma's service file runs `plasma_waitforname` until plasmashell owns the name, which
    /// matters when Kourier autostarts before the panel. So this can take a while; run it off
    /// any loop that must stay responsive. [`DesktopError::NoService`] when there is no server
    /// and none can be started.
    pub async fn connect(bus: &Bus) -> Result<Self, DesktopError> {
        let proxy = NotificationsProxy::new(bus.connection()).await?;
        let caps = match proxy.get_capabilities().await {
            Ok(caps) => Capabilities(caps),
            Err(e) if is_no_service(&e) => return Err(DesktopError::NoService(SERVICE)),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            proxy,
            caps,
            owner: name_owner(bus).await,
        })
    }

    /// The unique bus name of the server this client talked to when it connected (compare
    /// with [`watch_server`]'s announcements to notice a restart).
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// What the server supports.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// The server's `(name, vendor, version, spec_version)`.
    pub async fn server_information(
        &self,
    ) -> Result<(String, String, String, String), DesktopError> {
        Ok(self.proxy.get_server_information().await?)
    }

    /// Shows (or replaces) a notification; returns its id.
    pub async fn notify(&self, n: &Notification) -> Result<u32, DesktopError> {
        let body = if self.caps.body_markup() {
            escape_markup(&n.body)
        } else {
            n.body.clone()
        };
        let mut actions: Vec<&str> = Vec::new();
        if self.caps.actions() {
            if n.default_action {
                // Plasma does not show a button for `default`; an empty label keeps it hidden
                // on servers that do.
                actions.extend([DEFAULT_ACTION, ""]);
            }
            for (key, label) in &n.actions {
                actions.extend([key.as_str(), label.as_str()]);
            }
        }
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        if let Some(entry) = &n.desktop_entry {
            hints.insert("desktop-entry", Value::from(entry.as_str()));
        }
        if let Some(category) = &n.category {
            hints.insert("category", Value::from(category.as_str()));
        }
        hints.insert("urgency", Value::from(n.urgency.byte()));
        if let Some(origin) = &n.origin_name {
            hints.insert("x-kde-origin-name", Value::from(origin.as_str()));
        }
        if let Some(img) = &n.image {
            hints.insert(
                "image-data",
                Value::from((
                    img.width,
                    img.height,
                    img.rowstride,
                    img.has_alpha,
                    img.bits_per_sample,
                    img.channels,
                    img.data.clone(),
                )),
            );
        }
        if let Some(reply) = &n.inline_reply
            && self.caps.inline_reply()
        {
            actions.extend([INLINE_REPLY_ACTION, ""]);
            if !reply.placeholder.is_empty() {
                hints.insert(
                    "x-kde-reply-placeholder-text",
                    Value::from(reply.placeholder.as_str()),
                );
            }
            if !reply.submit_label.is_empty() {
                hints.insert(
                    "x-kde-reply-submit-button-text",
                    Value::from(reply.submit_label.as_str()),
                );
            }
        }
        let id = self
            .proxy
            .notify(
                &n.app_name,
                n.replaces_id,
                &n.icon,
                &n.summary,
                &body,
                &actions,
                &hints,
                n.timeout_ms,
            )
            .await?;
        Ok(id)
    }

    /// Closes a notification (no error when it is already gone).
    pub async fn close(&self, id: u32) -> Result<(), DesktopError> {
        Ok(self.proxy.close_notification(id).await?)
    }

    /// Subscribes to the server's signals. The forwarding task ends when the receiver is
    /// dropped or the connection closes. Must run inside the tokio runtime.
    pub async fn subscribe(
        &self,
    ) -> Result<mpsc::UnboundedReceiver<NotificationEvent>, DesktopError> {
        type Events = std::pin::Pin<Box<dyn futures_util::Stream<Item = NotificationEvent> + Send>>;
        let closed: Events = Box::pin(self.proxy.receive_notification_closed().await?.filter_map(
            |s| async move {
                let a = s.args().ok()?;
                Some(NotificationEvent::Closed {
                    id: a.id,
                    reason: CloseReason::from_code(a.reason),
                })
            },
        ));
        let action: Events = Box::pin(self.proxy.receive_action_invoked().await?.filter_map(
            |s| async move {
                let a = s.args().ok()?;
                Some(NotificationEvent::Action {
                    id: a.id,
                    key: a.action_key,
                })
            },
        ));
        let token: Events = Box::pin(self.proxy.receive_activation_token().await?.filter_map(
            |s| async move {
                let a = s.args().ok()?;
                Some(NotificationEvent::ActivationToken {
                    id: a.id,
                    token: a.activation_token,
                })
            },
        ));
        let replied: Events = Box::pin(
            self.proxy
                .receive_notification_replied()
                .await?
                .filter_map(|s| async move {
                    let a = s.args().ok()?;
                    Some(NotificationEvent::Replied {
                        id: a.id,
                        text: a.text,
                    })
                }),
        );
        let mut events = futures_util::stream::select_all([closed, action, token, replied]);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    e = events.next() => match e {
                        Some(e) => {
                            if tx.send(e).is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });
        Ok(rx)
    }
}

/// The unique name that owns the notification service now.
async fn name_owner(bus: &Bus) -> Option<String> {
    let dbus = zbus::fdo::DBusProxy::new(bus.connection()).await.ok()?;
    let name = zbus::names::BusName::try_from(SERVICE).ok()?;
    dbus.get_name_owner(name).await.ok().map(|o| o.to_string())
}

/// Watches the notification server's bus name: its new owner's unique name each time it gets
/// one (the server started after Kourier, or restarted). Notification ids of a previous owner
/// mean nothing to the new one. The forwarding task ends when the receiver is dropped or the
/// connection closes. Must run inside the tokio runtime.
pub async fn watch_server(bus: &Bus) -> Result<mpsc::UnboundedReceiver<String>, DesktopError> {
    let dbus = zbus::fdo::DBusProxy::builder(bus.connection())
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let mut changes = dbus
        .receive_name_owner_changed_with_args(&[(0, SERVICE)])
        .await?;
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tx.closed() => break,
                s = changes.next() => match s {
                    Some(s) => {
                        let owner = s
                            .args()
                            .ok()
                            .and_then(|a| a.new_owner().as_ref().map(ToString::to_string));
                        if let Some(owner) = owner
                            && tx.send(owner).is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });
    Ok(rx)
}
