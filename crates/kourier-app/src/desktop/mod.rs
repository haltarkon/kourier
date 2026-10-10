//! Desktop integration wiring (architecture §8.7, §8.8): the core's notification events become
//! desktop notifications, clicks and inline replies come back as [`DesktopEvent`]s and core
//! commands; the tray, single instance (`org.freedesktop.Application`), autostart entry and the
//! preferences that drive them ([`Prefs`]).
//!
//! The D-Bus clients live in `kourier-desktop`; the pure bookkeeping in [`policy`]. The UI
//! supplies a [`Presenter`] (translated strings, conversation titles, message lines, avatar
//! and tray pixels) once Qt runs, and consumes [`DesktopEvent`]s on its thread.
//!
//! Desktop services are on unless [`DesktopMode`] turns them off: `KOURIER_DESKTOP=0`, or an
//! offscreen Qt platform (headless runs never touch the session's notifications or tray;
//! `KOURIER_DESKTOP=1` forces them on, for tests on a private bus).
//!
//! Besides the session bus there is the system bus ([`SystemBus`]): logind says when the
//! machine wakes from suspend and NetworkManager when the network changes, and both reach the
//! core as reconnect hints (`UiCommand::Resumed`, `UiCommand::NetworkChanged`, §4.7). Without
//! them (no system bus, no systemd, no NetworkManager) Kourier still notices through its
//! keepalive, only later.

#[cfg(all(test, feature = "fake"))]
mod e2e;
pub mod policy;
mod service;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use kourier_core::facade::{AccountRow, ConversationDetail, CoreHandle, MessageRow, UiCommand};
use kourier_desktop::Bus;
use kourier_desktop::autostart::{self, AutostartEntry};
use kourier_desktop::hints::{Hint, SystemHints};
use kourier_desktop::instance::{self, Claim, Instance, InstanceRequest};
use kourier_model::{Body, ConversationId};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use kourier_desktop::APP_ID;
pub use kourier_desktop::notifications::ImageData;
pub use kourier_desktop::tray::TrayIcon;
pub use policy::Texts;

use crate::paths::Paths;
use crate::prefs::{Prefs, PrefsError};

/// `KOURIER_DESKTOP=0|1` turns desktop services off or forces them on.
pub const DESKTOP_ENV: &str = "KOURIER_DESKTOP";

/// How long a second launch waits for the running instance to take its request.
pub const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether to use the session's desktop services (notifications, tray, single instance).
/// The default is `Off`, so library users and tests never touch the session bus by accident;
/// the binary uses [`DesktopMode::from_env`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DesktopMode {
    /// On (the binary's choice unless the Qt platform is headless).
    Auto,
    /// On, forced (`KOURIER_DESKTOP=1`).
    On,
    /// Off: no D-Bus at all (preferences and autostart still work).
    #[default]
    Off,
}

impl DesktopMode {
    /// Reads [`DESKTOP_ENV`] and `QT_QPA_PLATFORM`.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Self {
        match get(DESKTOP_ENV).as_deref().map(str::trim) {
            Some("1" | "on" | "yes" | "true") => return DesktopMode::On,
            Some("0" | "off" | "no" | "false") => return DesktopMode::Off,
            _ => {}
        }
        match get("QT_QPA_PLATFORM").as_deref() {
            Some(p) if p.starts_with("offscreen") || p.starts_with("minimal") => DesktopMode::Off,
            _ => DesktopMode::Auto,
        }
    }

    /// Whether desktop services run.
    pub fn enabled(self) -> bool {
        self != DesktopMode::Off
    }
}

/// `DBUS_SYSTEM_BUS_ADDRESS`: where the system bus is, when not at its usual socket.
pub const SYSTEM_BUS_ENV: &str = "DBUS_SYSTEM_BUS_ADDRESS";

/// Where the resume and network hints come from (§4.7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SystemBus {
    /// The machine's system bus: logind and NetworkManager.
    System,
    /// The bus at this D-Bus address (tests: a private bus with mock services).
    At(String),
    /// None: no hints (the default, so library users and tests never listen to the machine's
    /// logind or NetworkManager by accident).
    #[default]
    Off,
}

impl SystemBus {
    /// The bus for a desktop mode: none when desktop services are off; the system bus in the
    /// automatic mode; and when the services are forced on (`KOURIER_DESKTOP=1`, the set-up
    /// of tests on a private bus) only a bus named with [`SYSTEM_BUS_ENV`], so such a run
    /// never listens to the machine it happens to run on.
    pub fn from_env(mode: DesktopMode, get: impl Fn(&str) -> Option<String>) -> Self {
        let named = get(SYSTEM_BUS_ENV).filter(|a| !a.trim().is_empty());
        match (mode, named) {
            (DesktopMode::Off, _) | (DesktopMode::On, None) => Self::Off,
            (_, Some(address)) => Self::At(address),
            (DesktopMode::Auto, None) => Self::System,
        }
    }
}

/// How the desktop integration starts (from the command line and the environment).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DesktopOptions {
    /// On, off or automatic.
    pub mode: DesktopMode,
    /// Where resume and network hints come from.
    pub system_bus: SystemBus,
    /// `--background`: start hidden in the tray (when the tray is on).
    pub background: bool,
    /// `sgnl://…` URIs from the command line.
    pub uris: Vec<String>,
    /// `$XDG_ACTIVATION_TOKEN` of this launch (forwarded to a running instance).
    pub activation_token: Option<String>,
}

// URIs and tokens are never logged.
impl std::fmt::Debug for DesktopOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopOptions")
            .field("mode", &self.mode)
            .field("system_bus", &matches!(self.system_bus, SystemBus::Off))
            .field("background", &self.background)
            .field("uris", &self.uris.len())
            .field("activation_token", &self.activation_token.is_some())
            .finish()
    }
}

/// What the desktop asks the UI to do. Handled on the Qt thread.
#[derive(Clone, PartialEq, Eq)]
pub enum DesktopEvent {
    /// Show and raise the window (with the XDG activation token), and open a conversation when
    /// a notification of it was clicked.
    Activate {
        /// The XDG activation token.
        token: Option<String>,
        /// The conversation to open.
        conversation: Option<ConversationId>,
    },
    /// A tray click: hide the window when it is shown and active, else show and raise it.
    Toggle {
        /// The XDG activation token.
        token: Option<String>,
    },
    /// `sgnl://` URIs from another launch (M1: only focus the window, §9.4).
    Open {
        /// The URIs.
        uris: Vec<String>,
        /// The XDG activation token.
        token: Option<String>,
    },
    /// Quit (tray menu).
    Quit,
    /// Whether a tray icon is shown (close-to-tray needs one).
    TrayAvailable(bool),
}

impl std::fmt::Debug for DesktopEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Activate {
                token,
                conversation,
            } => f
                .debug_struct("Activate")
                .field("token", &token.is_some())
                .field("conversation", conversation)
                .finish(),
            Self::Toggle { token } => f
                .debug_struct("Toggle")
                .field("token", &token.is_some())
                .finish(),
            Self::Open { uris, token } => f
                .debug_struct("Open")
                .field("uris", &uris.len())
                .field("token", &token.is_some())
                .finish(),
            Self::Quit => f.write_str("Quit"),
            Self::TrayAvailable(a) => f.debug_tuple("TrayAvailable").field(a).finish(),
        }
    }
}

/// What the UI contributes to notifications and the tray: translated strings, names, message
/// lines and pixels. Called on runtime threads. The defaults are plain English without pixels.
pub trait Presenter: Send + Sync + 'static {
    /// Fixed strings.
    fn texts(&self) -> Texts {
        Texts::default()
    }

    /// The conversation's title (sender of a 1:1 chat, or the group).
    fn conversation_title(&self, detail: &ConversationDetail) -> String {
        detail
            .row
            .title
            .clone()
            .unwrap_or_else(|| "Unknown".to_owned())
    }

    /// One line for a message ("Ari: see you" in groups).
    fn message_line(&self, _detail: &ConversationDetail, message: &MessageRow) -> String {
        match &message.body {
            Body::Text(t) => t.body.clone(),
            _ => self.texts().new_message,
        }
    }

    /// "3 new messages".
    fn new_messages(&self, count: usize) -> String {
        format!("{count} new messages")
    }

    /// The catch-up summary ("12 new messages in 4 chats").
    fn catch_up(&self, messages: u32, conversations: u32) -> String {
        format!("{messages} new messages in {conversations} chats")
    }

    /// The account's label in notifications when several accounts exist.
    fn account_label(&self, account: &AccountRow) -> String {
        account
            .display_name
            .clone()
            .unwrap_or_else(|| account.network.as_str().to_owned())
    }

    /// The tray tooltip line.
    fn unread_tooltip(&self, unread: u32) -> String {
        match unread {
            0 => String::new(),
            n => format!("{n} unread"),
        }
    }

    /// The conversation's avatar for `image-data`.
    fn avatar(&self, _title: &str, _detail: &ConversationDetail) -> Option<ImageData> {
        None
    }

    /// The tray icons: normal and attention, several sizes each (empty: the icon theme's).
    fn tray_icons(&self) -> (Vec<TrayIcon>, Vec<TrayIcon>) {
        (Vec::new(), Vec::new())
    }
}

/// [`Presenter`] with the defaults (before the UI attaches, and in tests).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlainPresenter;

impl Presenter for PlainPresenter {}

/// Commands to the desktop task.
enum Command {
    Attach(Arc<dyn Presenter>),
    Prefs(Prefs),
    Focused(bool),
    Unread(u32),
}

struct Shared {
    commands: mpsc::UnboundedSender<Command>,
    events: Mutex<Option<mpsc::UnboundedReceiver<DesktopEvent>>>,
    prefs: Mutex<Prefs>,
    enabled: bool,
    background: bool,
}

/// The UI's handle to the desktop integration. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct DesktopHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for DesktopHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopHandle")
            .field("enabled", &self.shared.enabled)
            .finish_non_exhaustive()
    }
}

impl DesktopHandle {
    /// The events for the UI (once; `None` afterwards).
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<DesktopEvent>> {
        self.shared
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Whether desktop services (D-Bus) run.
    pub fn is_enabled(&self) -> bool {
        self.shared.enabled
    }

    /// Started with `--background`.
    pub fn background(&self) -> bool {
        self.shared.background
    }

    /// The current preferences.
    pub fn prefs(&self) -> Prefs {
        self.shared
            .prefs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Changes the preferences: saved to `kourier.toml` and applied (tray, autostart,
    /// notification content) in the background.
    pub fn set_prefs(&self, prefs: Prefs) {
        let mut current = self
            .shared
            .prefs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *current == prefs {
            return;
        }
        *current = prefs.clone();
        drop(current);
        let _ = self.shared.commands.send(Command::Prefs(prefs));
    }

    /// The UI is up: use its strings and pixels; the tray starts now (when enabled).
    pub fn attach(&self, presenter: Arc<dyn Presenter>) {
        let _ = self.shared.commands.send(Command::Attach(presenter));
    }

    /// Whether a Kourier window has focus (a focused window closes catch-up summaries).
    pub fn set_focused(&self, focused: bool) {
        let _ = self.shared.commands.send(Command::Focused(focused));
    }

    /// Unread messages (tray attention and tooltip).
    pub fn set_unread(&self, unread: u32) {
        let _ = self.shared.commands.send(Command::Unread(unread));
    }
}

/// The single-instance bus name of a profile: the app id for the default profile, a hashed
/// suffix for named profiles and `KOURIER_HOME` (test) profiles, so they never activate the
/// user's running Kourier.
pub fn instance_name(paths: &Paths) -> String {
    let profile = paths.secret_profile();
    if profile == crate::paths::Profile::default().name() {
        return APP_ID.to_owned();
    }
    let digest = Sha256::digest(profile.as_bytes());
    let hex: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("{APP_ID}.p{hex}")
}

/// The session bus and, for the first instance, the owned name.
#[derive(Debug, Default)]
pub(crate) struct Connection {
    bus: Option<Bus>,
    instance: Option<Instance>,
}

/// The outcome of [`connect`].
pub(crate) enum Connected {
    /// This process runs (with or without a bus).
    Run(Connection),
    /// Another instance got the request; this process should exit 0.
    Forwarded,
}

/// Connects to the session bus and, with `instance`, claims the single-instance name; a later
/// launch forwards its activation or URIs to the owner (a `--background` launch without URIs,
/// such as autostart, leaves a running Kourier as it is). No bus = no desktop services.
pub(crate) async fn connect(
    opts: &DesktopOptions,
    instance: Option<&str>,
) -> Result<Connected, kourier_desktop::DesktopError> {
    if !opts.mode.enabled() {
        return Ok(Connected::Run(Connection::default()));
    }
    let bus = match Bus::session().await {
        Ok(bus) => bus,
        Err(e) => {
            tracing::warn!(error = %e, "no session bus: no notifications, tray or single instance");
            return Ok(Connected::Run(Connection::default()));
        }
    };
    let Some(name) = instance else {
        return Ok(Connected::Run(Connection {
            bus: Some(bus),
            instance: None,
        }));
    };
    match instance::claim(bus, name).await? {
        Claim::Primary(inst) => Ok(Connected::Run(Connection {
            bus: Some(inst.bus().clone()),
            instance: Some(inst),
        })),
        Claim::Secondary(_) if opts.background && opts.uris.is_empty() => {
            tracing::info!("already running; a background launch changes nothing");
            Ok(Connected::Forwarded)
        }
        Claim::Secondary(bus) => {
            let request = if opts.uris.is_empty() {
                InstanceRequest::Activate {
                    token: opts.activation_token.clone(),
                }
            } else {
                InstanceRequest::Open {
                    uris: opts.uris.clone(),
                    token: opts.activation_token.clone(),
                }
            };
            tokio::time::timeout(FORWARD_TIMEOUT, instance::forward(&bus, name, &request))
                .await
                .map_err(|_| {
                    kourier_desktop::DesktopError::Dbus(
                        "the running instance did not answer in time".to_owned(),
                    )
                })??;
            Ok(Connected::Forwarded)
        }
    }
}

/// Where the autostart entry of a profile goes and what it runs. Profiles under
/// `KOURIER_HOME` keep it inside their own config directory (inert), so tests never write into
/// the user's autostart folder.
fn autostart_target(paths: &Paths) -> (PathBuf, AutostartEntry) {
    let profile = paths.profile();
    let file = match profile.name() {
        "default" => APP_ID.to_owned(),
        name => format!("{APP_ID}-{name}"),
    };
    let path = autostart::entry_path(&paths.autostart_config_home(), &file);
    let program = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kourier"));
    let mut args = Vec::new();
    if profile.name() != "default" {
        args.push(format!("--profile={}", profile.name()));
    }
    let entry = AutostartEntry {
        program,
        args,
        background: false,
        name: "Kourier".to_owned(),
        icon: APP_ID.to_owned(),
    };
    (path, entry)
}

/// Starts the desktop task on `runtime`: loads the preferences, syncs the autostart entry,
/// subscribes to the core and serves notifications, the tray and the instance requests.
pub(crate) fn spawn(
    runtime: &tokio::runtime::Handle,
    core: CoreHandle,
    connection: Connection,
    opts: &DesktopOptions,
    paths: &Paths,
    stop: CancellationToken,
) -> (DesktopHandle, tokio::task::JoinHandle<()>) {
    let prefs_path = paths.config_file();
    let mut prefs = Prefs::load_or_default(&prefs_path);
    let (autostart_path, entry) = autostart_target(paths);
    // The entry's existence is the truth (the user may remove it in System Settings).
    prefs.autostart = autostart::is_enabled(&autostart_path);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let enabled = connection.bus.is_some();
    let shared = Arc::new(Shared {
        commands,
        events: Mutex::new(Some(events_rx)),
        prefs: Mutex::new(prefs.clone()),
        enabled,
        background: opts.background,
    });
    if !opts.uris.is_empty() {
        let _ = events_tx.send(DesktopEvent::Open {
            uris: opts.uris.clone(),
            token: None,
        });
    }
    let service = service::Service::new(
        core,
        events_tx,
        prefs,
        service::Files {
            prefs: prefs_path,
            autostart: autostart_path,
            entry,
        },
    );
    let task = runtime.spawn(service.run(connection, command_rx, stop));
    (DesktopHandle { shared }, task)
}

/// Forwards resume and network hints from the system bus to the core (§4.7): logind's
/// `PrepareForSleep(false)` becomes `UiCommand::Resumed`, a NetworkManager state or
/// primary-connection change `UiCommand::NetworkChanged`. The core collects a burst and tells
/// every linked backend once. A missing logind or NetworkManager needs no handling at all (the
/// watcher only listens). A system bus that goes away later (its daemon is restarted) is
/// connected to again when it is back, and one that is not there at the start is waited for
/// (`SystemHints::watch_reconnecting`); either way the core is told once that something may
/// have changed meanwhile.
pub(crate) fn spawn_hints(
    runtime: &tokio::runtime::Handle,
    core: CoreHandle,
    bus: SystemBus,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if bus == SystemBus::Off {
        return None;
    }
    Some(runtime.spawn(async move {
        let sink = move |hint: Hint| {
            let command = match hint {
                Hint::Resumed => UiCommand::Resumed,
                Hint::NetworkChanged => UiCommand::NetworkChanged,
            };
            // A stopped core needs no hints.
            let _ = core.send(command);
        };
        // The same bus again whenever it went away and came back (a restarted bus daemon).
        let connect = move || {
            let bus = bus.clone();
            async move {
                match bus {
                    SystemBus::System => Bus::system().await,
                    SystemBus::At(address) => Bus::at(&address).await,
                    SystemBus::Off => Err(kourier_desktop::DesktopError::NoBus(
                        "no system bus is watched".into(),
                    )),
                }
            }
        };
        let watcher = SystemHints::watch_reconnecting(connect, sink).await;
        if !watcher.connected_at_start() {
            // Said once. The watcher goes on trying, at long intervals.
            tracing::info!(
                "no system bus yet: until one appears, a resume or a network change is noticed by the keepalive only"
            );
        }
        stop.cancelled().await;
        drop(watcher);
    }))
}

/// Keeps only the URIs Kourier handles, for the command line: `sgnl:` links, and
/// `signalcaptcha:` links (what the browser hands back after a challenge was solved there).
pub fn handled_uris(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let scheme = |a: &str, scheme: &str| {
        a.get(..scheme.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
    };
    args.into_iter()
        .filter(|a| {
            a.len() <= kourier_desktop::instance::MAX_URI_LEN
                && (scheme(a, "sgnl:") || scheme(a, "signalcaptcha:"))
        })
        .take(kourier_desktop::instance::MAX_URIS)
        .collect()
}

/// Saves the preferences (on a blocking thread).
async fn save_prefs(path: PathBuf, prefs: Prefs) -> Result<(), PrefsError> {
    tokio::task::spawn_blocking(move || prefs.save(&path))
        .await
        .map_err(|e| PrefsError::Io(std::io::Error::other(e.to_string())))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn mode_from_env() {
        assert_eq!(DesktopMode::from_env(env(&[])), DesktopMode::Auto);
        assert_eq!(
            DesktopMode::from_env(env(&[("QT_QPA_PLATFORM", "offscreen")])),
            DesktopMode::Off
        );
        assert_eq!(
            DesktopMode::from_env(env(&[("QT_QPA_PLATFORM", "wayland")])),
            DesktopMode::Auto
        );
        assert_eq!(
            DesktopMode::from_env(env(&[("QT_QPA_PLATFORM", "offscreen"), (DESKTOP_ENV, "1")])),
            DesktopMode::On
        );
        assert_eq!(
            DesktopMode::from_env(env(&[(DESKTOP_ENV, "0")])),
            DesktopMode::Off
        );
        assert!(DesktopMode::Auto.enabled() && DesktopMode::On.enabled());
        assert!(!DesktopMode::Off.enabled());
    }

    #[test]
    fn the_system_bus_is_used_only_where_it_is_meant() {
        let named = env(&[(SYSTEM_BUS_ENV, "unix:path=/tmp/private")]);
        // The app on a desktop: the machine's logind and NetworkManager.
        assert_eq!(
            SystemBus::from_env(DesktopMode::Auto, env(&[])),
            SystemBus::System
        );
        // Headless runs and library users: nothing.
        assert_eq!(
            SystemBus::from_env(DesktopMode::Off, env(&[])),
            SystemBus::Off
        );
        assert_eq!(
            SystemBus::from_env(DesktopMode::Off, &named),
            SystemBus::Off
        );
        assert_eq!(SystemBus::default(), SystemBus::Off);
        // Tests force desktop services on a private bus: the system bus only when named.
        assert_eq!(
            SystemBus::from_env(DesktopMode::On, env(&[])),
            SystemBus::Off
        );
        assert_eq!(
            SystemBus::from_env(DesktopMode::On, &named),
            SystemBus::At("unix:path=/tmp/private".into())
        );
        assert_eq!(
            SystemBus::from_env(DesktopMode::Auto, &named),
            SystemBus::At("unix:path=/tmp/private".into())
        );
        assert_eq!(
            SystemBus::from_env(DesktopMode::Auto, env(&[(SYSTEM_BUS_ENV, " ")])),
            SystemBus::System
        );
    }

    #[test]
    fn instance_names() {
        let resolve = |profile: crate::paths::Profile, home: Option<&str>| {
            let home = home.map(std::ffi::OsString::from);
            Paths::from_env(profile, |k| match k {
                crate::paths::HOME_ENV => home.clone(),
                "HOME" => Some("/home/u".into()),
                "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
                _ => None,
            })
            .expect("paths")
        };
        let default = resolve(crate::paths::Profile::default(), None);
        assert_eq!(instance_name(&default), APP_ID);
        let work = resolve(crate::paths::Profile::named("work").expect("name"), None);
        let test = resolve(crate::paths::Profile::default(), Some("/tmp/k"));
        let (a, b) = (instance_name(&work), instance_name(&test));
        assert!(a.starts_with("app.kourier.Kourier.p") && a.len() == APP_ID.len() + 14);
        assert_ne!(a, b);
        assert_ne!(b, APP_ID, "test profiles never activate the user's Kourier");
        assert!(zbus_name_ok(&a) && zbus_name_ok(&b));
    }

    fn zbus_name_ok(name: &str) -> bool {
        name.split('.').all(|e| {
            !e.is_empty()
                && !e.starts_with(|c: char| c.is_ascii_digit())
                && e.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    }

    #[test]
    fn autostart_paths() {
        let paths = Paths::from_env(
            crate::paths::Profile::named("work").expect("name"),
            |k| match k {
                "HOME" => Some("/home/u".into()),
                "XDG_RUNTIME_DIR" => Some("/run/user/1000".into()),
                _ => None,
            },
        )
        .expect("paths");
        let (path, entry) = autostart_target(&paths);
        assert_eq!(
            path,
            PathBuf::from("/home/u/.config/autostart/app.kourier.Kourier-work.desktop")
        );
        assert_eq!(entry.args, vec!["--profile=work".to_owned()]);
        let test = Paths::from_env(crate::paths::Profile::default(), |k| match k {
            crate::paths::HOME_ENV => Some("/tmp/k".into()),
            _ => None,
        })
        .expect("paths");
        let (path, entry) = autostart_target(&test);
        assert_eq!(
            path,
            PathBuf::from("/tmp/k/kourier/config/autostart/app.kourier.Kourier.desktop")
        );
        assert!(entry.args.is_empty());
    }

    #[test]
    fn only_sgnl_uris() {
        let uris = handled_uris(
            [
                "--fixture",
                "sgnl://linkdevice?uuid=x",
                "SGNL://signal.me/#p/x",
                "https://signal.me/#p/x",
                "sgn",
                "signalcaptcha://signal-hcaptcha.token",
                "signalcaptcha",
            ]
            .map(String::from),
        );
        assert_eq!(
            uris,
            vec![
                "sgnl://linkdevice?uuid=x",
                "SGNL://signal.me/#p/x",
                // What the browser hands back after a solved challenge.
                "signalcaptcha://signal-hcaptcha.token"
            ]
        );
    }

    #[test]
    fn a_long_challenge_answer_is_not_cut_off() {
        // A captcha token is about 2,600 bytes today and has no documented maximum; Signal
        // Desktop sets no limit on the link. One three times as long must still arrive
        // whole; the bound on input from other processes stays.
        let answer = format!(
            "signalcaptcha://signal-hcaptcha.5fad97ac-7d06-4e44-b18a-b950b20148ff.challenge.P1_{}",
            "eyJ0eXAiOiJKV1Qi".repeat(500)
        );
        let bytes = answer.len();
        assert!(bytes > 8000);
        assert!(
            handled_uris([answer.clone()]) == vec![answer],
            "a challenge answer of {bytes} bytes did not arrive whole"
        );
        let endless = format!(
            "signalcaptcha://{}",
            "x".repeat(kourier_desktop::instance::MAX_URI_LEN)
        );
        assert!(handled_uris([endless]).is_empty());
        let many = (0..100).map(|i| format!("sgnl://signal.me/#p/{i}"));
        assert_eq!(
            handled_uris(many).len(),
            kourier_desktop::instance::MAX_URIS
        );
    }
}
