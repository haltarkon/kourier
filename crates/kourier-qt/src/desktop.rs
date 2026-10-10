//! The UI side of the desktop integration (§8.7, §8.8): the [`QtPresenter`] that gives
//! `kourier_app::desktop` translated strings, titles, message lines and pixels, and the pump
//! that hands [`DesktopEvent`]s (notification clicks, tray, other launches) to `App` on the
//! Qt thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cxx_qt::CxxQtThread;
use cxx_qt_lib::QString;
use kourier_app::AppHandle;
use kourier_app::desktop::{DesktopEvent, DesktopHandle, ImageData, Presenter, Texts, TrayIcon};
use kourier_app::facade::{AccountRow, ConversationDetail, MessageRow};
use kourier_viewmodel::notify;

use crate::bridge::qobject;

/// Notification avatar size in pixels (Plasma shows them at up to 64 logical pixels).
pub const AVATAR_SIZE: u32 = 96;
/// Tray icon sizes rendered for the StatusNotifierItem.
pub const TRAY_SIZES: [i32; 4] = [16, 22, 32, 64];
/// The bundled app icon (qrc path, `icons/kourier.svg`).
pub const APP_ICON: &str = ":/qt/qml/app/kourier/icons/kourier.svg";

/// The theme's corner radius as thousandths of the side of a list avatar, until the window
/// has said what it is (Breeze's: about 5 px on 47). Square notification avatars have the
/// same proportion of corner to side as the avatars in the list.
const DEFAULT_THEME_CORNER_PERMILLE: u32 = 107;
static THEME_CORNER_PERMILLE: AtomicU32 = AtomicU32::new(DEFAULT_THEME_CORNER_PERMILLE);

/// Tells the notification avatars the theme's corner as a share of a list avatar's side (QML
/// knows the theme; this side only draws). Out of range values are clamped to a circle.
pub(crate) fn set_theme_corner(ratio: f64) {
    let permille = (ratio.clamp(0.0, 0.5) * 1000.0).round() as u32;
    THEME_CORNER_PERMILLE.store(permille, Ordering::Relaxed);
}

/// Corner radius in pixels of a notification avatar of `size` pixels: half the side for a
/// circle, else the theme's share of it.
pub fn avatar_corner(round: bool, size: u32) -> i32 {
    let half = size.div_ceil(2);
    let corner = if round {
        half
    } else {
        (size * THEME_CORNER_PERMILLE.load(Ordering::Relaxed) + 500) / 1000
    };
    i32::try_from(corner.min(half)).unwrap_or(i32::MAX)
}

/// Strings from the viewmodel (translated there) and pixels from Qt.
pub struct QtPresenter {
    texts: Texts,
    icons: Vec<TrayIcon>,
    attention: Vec<TrayIcon>,
    /// Where "Round avatars" is read at each notification; without it, avatars are round.
    desktop: Option<DesktopHandle>,
    /// A fixed answer for tests, ahead of the preferences.
    round: Option<bool>,
}

impl std::fmt::Debug for QtPresenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QtPresenter")
            .field("icons", &self.icons.len())
            .finish_non_exhaustive()
    }
}

fn tray_icons(dot: bool) -> Vec<TrayIcon> {
    let path = QString::from(APP_ICON);
    TRAY_SIZES
        .iter()
        .filter_map(|&size| {
            let argb = qobject::render_icon_argb(&path, size, dot);
            (!argb.is_empty()).then_some(TrayIcon {
                width: size,
                height: size,
                argb,
            })
        })
        .collect()
}

impl QtPresenter {
    /// Builds the presenter; renders the tray icons, so call it on the Qt thread.
    pub fn new() -> Self {
        let t = notify::fixed_texts();
        Self {
            texts: Texts {
                app_name: t.app_name,
                new_message: t.new_message,
                reply_placeholder: t.reply_placeholder,
                tray_show: t.tray_show,
                tray_quit: t.tray_quit,
            },
            icons: tray_icons(false),
            attention: tray_icons(true),
            desktop: None,
            round: None,
        }
    }

    /// Draws avatars as the preferences say ("Round avatars"), also after they change.
    #[must_use]
    pub fn following(mut self, desktop: DesktopHandle) -> Self {
        self.desktop = Some(desktop);
        self
    }

    /// Draws avatars round or square whatever the preferences say (tests).
    #[must_use]
    pub fn with_round(mut self, round: bool) -> Self {
        self.round = Some(round);
        self
    }

    fn round_avatars(&self) -> bool {
        self.round
            .or_else(|| self.desktop.as_ref().map(|d| d.prefs().round_avatars))
            .unwrap_or(true)
    }
}

impl Default for QtPresenter {
    /// [`QtPresenter::new`] (Qt thread only).
    fn default() -> Self {
        Self::new()
    }
}

impl Presenter for QtPresenter {
    fn texts(&self) -> Texts {
        self.texts.clone()
    }

    fn conversation_title(&self, detail: &ConversationDetail) -> String {
        notify::conversation_title(&detail.row)
    }

    fn message_line(&self, detail: &ConversationDetail, message: &MessageRow) -> String {
        notify::message_line(detail, message)
    }

    fn new_messages(&self, count: usize) -> String {
        notify::new_messages(count)
    }

    fn catch_up(&self, messages: u32, conversations: u32) -> String {
        notify::catch_up(messages, conversations)
    }

    fn account_label(&self, account: &AccountRow) -> String {
        notify::account_label(account)
    }

    fn unread_tooltip(&self, unread: u32) -> String {
        notify::unread_tooltip(unread)
    }

    fn avatar(&self, title: &str, detail: &ConversationDetail) -> Option<ImageData> {
        if detail.row.is_self {
            return None;
        }
        let size = i32::try_from(AVATAR_SIZE).ok()?;
        let rgba = qobject::render_avatar_rgba(
            &QString::from(title),
            notify::is_anonymous_title(title),
            size,
            avatar_corner(self.round_avatars(), AVATAR_SIZE),
        );
        ImageData::rgba(AVATAR_SIZE, AVATAR_SIZE, rgba)
    }

    fn tray_icons(&self) -> (Vec<TrayIcon>, Vec<TrayIcon>) {
        (self.icons.clone(), self.attention.clone())
    }
}

/// Attaches the presenter and forwards desktop events to `App` until the app shuts down.
pub(crate) fn start(app: &AppHandle, thread: CxxQtThread<qobject::App>) {
    let desktop = app.desktop().clone();
    desktop.attach(Arc::new(QtPresenter::new().following(desktop.clone())));
    let Some(mut events) = desktop.take_events() else {
        return;
    };
    let stop = app.shutdown_token();
    app.runtime().spawn(async move {
        loop {
            let event = tokio::select! {
                _ = stop.cancelled() => break,
                e = events.recv() => match e {
                    Some(e) => e,
                    None => break,
                },
            };
            if thread
                .queue(move |app| app.on_desktop_event(event))
                .is_err()
            {
                break;
            }
        }
    });
}

impl qobject::App {
    /// Hands a desktop event to QML (Qt thread).
    pub fn on_desktop_event(mut self: std::pin::Pin<&mut Self>, event: DesktopEvent) {
        match event {
            DesktopEvent::Activate {
                token,
                conversation,
            } => {
                tracing::debug!(conversation = conversation.map(|c| c.0), "activation");
                self.activation_requested(
                    QString::from(token.unwrap_or_default().as_str()),
                    conversation.map_or(0, |c| c.0),
                );
            }
            DesktopEvent::Toggle { token } => {
                self.toggle_requested(QString::from(token.unwrap_or_default().as_str()));
            }
            DesktopEvent::Open { uris, token } => {
                // The browser hands a solved challenge back as a `signalcaptcha://` link: it
                // goes to the account that waits for one (the link is a secret: not logged).
                let answer = uris.iter().find(|u| {
                    u.get(..14)
                        .is_some_and(|s| s.eq_ignore_ascii_case("signalcaptcha:"))
                });
                if let (Some(answer), Some(account)) = (answer, self.challenged_account()) {
                    tracing::info!("a solved challenge came back from the browser");
                    self.as_mut()
                        .submit_challenge(account.0, &QString::from(answer.as_str()));
                }
                // §9.4: in M1 a sgnl:// link only brings the window up.
                tracing::info!(links = uris.len(), "sgnl:// link: showing the window");
                self.activation_requested(QString::from(token.unwrap_or_default().as_str()), 0);
            }
            DesktopEvent::Quit => self.quit_requested(),
            DesktopEvent::TrayAvailable(available) => {
                self.set_tray_status(i32::from(available));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_avatar_has_the_lists_proportion_of_corner_to_side() {
        // A circle is half the side (rounded up), whatever the theme says.
        assert_eq!(avatar_corner(true, 96), 48);
        assert_eq!(avatar_corner(true, 95), 48);
        // Before the window has said, Breeze's corner (about 5 px on a 47 px list avatar).
        set_theme_corner(f64::from(DEFAULT_THEME_CORNER_PERMILLE) / 1000.0);
        assert_eq!(avatar_corner(false, 96), 10);
        // The window's value: 6 px on a list avatar of 47 px, sharp corners, nonsense.
        set_theme_corner(6.0 / 47.0);
        assert_eq!(avatar_corner(false, 96), 12);
        set_theme_corner(0.0);
        assert_eq!(avatar_corner(false, 96), 0);
        set_theme_corner(7.0);
        assert_eq!(avatar_corner(false, 96), 48, "never more than a circle");
        set_theme_corner(f64::NAN);
        assert_eq!(avatar_corner(false, 96), 0);
        set_theme_corner(f64::from(DEFAULT_THEME_CORNER_PERMILLE) / 1000.0);
    }
}
