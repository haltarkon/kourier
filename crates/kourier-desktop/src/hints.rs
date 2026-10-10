//! Reconnect hints from the system bus (architecture §4.7): the machine woke from suspend
//! (logind's `PrepareForSleep(false)`), or its connectivity changed (NetworkManager's
//! `StateChanged`, or another connection became the primary one). A backend that hears of it
//! reconnects at once instead of waiting for its keepalive to fail, which after a resume can
//! take minutes (the failure Flare had: stuck offline after suspend, Flare issue 297).
//!
//! The watcher only listens: three match rules on the bus, no polling, no calls to the
//! services. A service that is not there (no systemd, no NetworkManager) costs nothing and logs
//! nothing: the bus accepts a match for a name nobody owns, and the signals arrive once
//! somebody does. Signals count only when the owner of the well-known name sends them; the bus
//! checks that, so another client cannot fake a resume.
//!
//! Every signal becomes one [`Hint`]: the core collects the hints of a burst itself
//! (`kourier_core::HINT_SETTLE`).
//!
//! The system bus can go away while Kourier runs (the bus daemon is restarted by an update, or
//! by hand). [`SystemHints::watch_reconnecting`] then connects again: it tries after
//! [`RECONNECT_FIRST`], doubling the wait up to [`RECONNECT_MAX`], for as long as the bus stays
//! away, and gives one [`Hint::NetworkChanged`] when it is back (a resume or a network change
//! may have gone unheard meanwhile; a backend that is connected only checks its connection).
//! Nothing is tried while the bus is there.
//!
//! A machine that has no system bus when Kourier starts is waited for in the same way (the
//! bus daemon may be restarting in that very moment, or come up after the session), with a
//! longer last wait, [`ABSENT_MAX`]: where a bus never comes (a container) that is one try to
//! connect every ten minutes, a `connect` that fails at once and nothing else.

use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt as _;
use zbus::message::Type;
use zbus::{MatchRule, MessageStream};

use crate::{Bus, DesktopError};

/// logind's bus name.
pub const LOGIND: &str = "org.freedesktop.login1";
/// logind's manager object.
pub const LOGIND_PATH: &str = "/org/freedesktop/login1";
/// logind's manager interface (`PrepareForSleep`).
pub const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";
/// NetworkManager's bus name and main interface (`StateChanged`, `PrimaryConnection`).
pub const NETWORK_MANAGER: &str = "org.freedesktop.NetworkManager";
/// NetworkManager's main object.
pub const NETWORK_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";

/// How many signals of one kind may wait while the sink is busy. A full queue loses nothing:
/// the connection stops reading until the task has taken one (zbus holds its reader back),
/// and this connection carries nothing else. The sink only passes the hint on, so the queue
/// does not fill in practice.
const QUEUE: usize = 16;

/// The first wait before the watcher connects to a system bus that went away.
pub const RECONNECT_FIRST: Duration = Duration::from_millis(500);
/// The longest wait between two tries (each wait is twice the one before).
pub const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// The longest wait between two tries while no system bus has been there since the start:
/// longer than [`RECONNECT_MAX`], because where there was none there may never be one.
pub const ABSENT_MAX: Duration = Duration::from_secs(600);

/// What the system told us.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hint {
    /// The machine resumed from suspend or hibernation.
    Resumed,
    /// The network changed: NetworkManager's state, or which connection is the primary one.
    NetworkChanged,
}

/// Listens for [`Hint`]s until dropped.
#[derive(Debug)]
pub struct SystemHints {
    task: tokio::task::JoinHandle<()>,
    /// The bus was there when the watcher started.
    connected: bool,
}

impl Drop for SystemHints {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn rule(
    sender: &'static str,
    path: &'static str,
    interface: &'static str,
    member: &'static str,
) -> Result<zbus::match_rule::Builder<'static>, DesktopError> {
    let bad = |e: zbus::Error| DesktopError::Dbus(e.to_string());
    MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(sender)
        .map_err(bad)?
        .path(path)
        .map_err(bad)?
        .interface(interface)
        .map_err(bad)?
        .member(member)
        .map_err(bad)
}

/// The three signal streams of one connection.
struct Streams {
    sleep: MessageStream,
    state: MessageStream,
    properties: MessageStream,
}

impl Streams {
    /// Puts the match rules on `bus`.
    async fn open(bus: &Bus) -> Result<Self, DesktopError> {
        let conn = bus.connection();
        let sleep = rule(LOGIND, LOGIND_PATH, LOGIND_MANAGER, "PrepareForSleep")?.build();
        let state = rule(
            NETWORK_MANAGER,
            NETWORK_MANAGER_PATH,
            NETWORK_MANAGER,
            "StateChanged",
        )?
        .build();
        let properties = rule(
            NETWORK_MANAGER,
            NETWORK_MANAGER_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )?
        .arg(0, NETWORK_MANAGER)
        .map_err(|e| DesktopError::Dbus(e.to_string()))?
        .build();
        Ok(Self {
            sleep: MessageStream::for_match_rule(sleep, conn, Some(QUEUE)).await?,
            state: MessageStream::for_match_rule(state, conn, Some(QUEUE)).await?,
            properties: MessageStream::for_match_rule(properties, conn, Some(QUEUE)).await?,
        })
    }

    /// Hands every hint to `sink` until the connection closes. (`&mut`: a shared reference
    /// to the sink could not cross an await in a spawned task unless the sink were `Sync`.)
    async fn pump(&mut self, sink: &mut (impl Fn(Hint) + Send)) {
        loop {
            let hint = tokio::select! {
                m = self.sleep.next() => match m {
                    // `true` announces the suspend, `false` the resume.
                    Some(Ok(m)) => (m.body().deserialize::<bool>() == Ok(false))
                        .then_some(Hint::Resumed),
                    Some(Err(_)) => None,
                    None => break,
                },
                m = self.state.next() => match m {
                    Some(Ok(_)) => Some(Hint::NetworkChanged),
                    Some(Err(_)) => None,
                    None => break,
                },
                m = self.properties.next() => match m {
                    Some(Ok(m)) => primary_changed(&m).then_some(Hint::NetworkChanged),
                    Some(Err(_)) => None,
                    None => break,
                },
            };
            if let Some(hint) = hint {
                sink(hint);
            }
        }
    }
}

impl SystemHints {
    /// Starts listening on `bus` (the system bus; a private bus in tests) and calls `sink`
    /// for every hint, on the runtime this is called from. The match rules are in place when
    /// this returns. Works whether or not logind and NetworkManager are on the bus. Ends for
    /// good when the bus closes ([`Self::watch_reconnecting`] connects again).
    pub async fn watch(
        bus: &Bus,
        sink: impl Fn(Hint) + Send + 'static,
    ) -> Result<Self, DesktopError> {
        let mut streams = Streams::open(bus).await?;
        let mut sink = sink;
        let task = tokio::spawn(async move {
            streams.pump(&mut sink).await;
            // The bus went away: said once.
            tracing::info!("the system bus closed: no more resume or network hints");
        });
        Ok(Self {
            task,
            connected: true,
        })
    }

    /// Whether the bus was there when the watcher started. `false` only for
    /// [`Self::watch_reconnecting`] on a machine without a system bus: it listens once one
    /// appears.
    pub fn connected_at_start(&self) -> bool {
        self.connected
    }

    /// Like [`Self::watch`], on the bus `connect` connects to, and for as long as the watcher
    /// lives: when that bus closes, `connect` is called again (after [`RECONNECT_FIRST`],
    /// then at doubling waits up to [`RECONNECT_MAX`]) until it is back, and listening goes
    /// on. Each return of the bus gives `sink` one [`Hint::NetworkChanged`], because a hint
    /// may have been missed while nobody listened.
    ///
    /// The first connection is tried before this returns: when it succeeds the match rules
    /// are in place then. When it fails there is no system bus right now
    /// ([`Self::connected_at_start`] says so, for the caller to note once), and the watcher
    /// waits for one in the same way, with waits that double up to [`ABSENT_MAX`]. A bus
    /// that appears gives `sink` the same one hint.
    pub async fn watch_reconnecting<C, F>(connect: C, sink: impl Fn(Hint) + Send + 'static) -> Self
    where
        C: Fn() -> F + Send + 'static,
        F: Future<Output = Result<Bus, DesktopError>> + Send,
    {
        let first = match connect().await {
            Ok(bus) => Streams::open(&bus).await.ok().map(|streams| (bus, streams)),
            Err(_) => None,
        };
        let connected = first.is_some();
        let mut sink = sink;
        let task = tokio::spawn(async move {
            // (The connection is held so that it lives as long as its streams are read.)
            let mut current = first;
            // How long the waits may grow: longer while no bus was ever there.
            let mut longest = if connected { RECONNECT_MAX } else { ABSENT_MAX };
            loop {
                if let Some((bus, mut streams)) = current.take() {
                    streams.pump(&mut sink).await;
                    drop(bus);
                    tracing::info!("the system bus closed: waiting for it to come back");
                    longest = RECONNECT_MAX;
                }
                let mut wait = RECONNECT_FIRST;
                current = loop {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(longest);
                    let Ok(again) = connect().await else {
                        continue;
                    };
                    // (A bus that closes again while the rules are set: the next round.)
                    if let Ok(streams) = Streams::open(&again).await {
                        break Some((again, streams));
                    }
                };
                tracing::info!("the system bus is there: listening for hints");
                sink(Hint::NetworkChanged);
            }
        });
        Self { task, connected }
    }
}

/// Whether a `PropertiesChanged` of NetworkManager is about its primary connection (the
/// default route moved to another network, e.g. from Wi-Fi to a cable).
fn primary_changed(message: &zbus::Message) -> bool {
    type Changed<'a> = (
        &'a str,
        std::collections::HashMap<&'a str, zbus::zvariant::Value<'a>>,
        Vec<&'a str>,
    );
    let body = message.body();
    let Ok((_, changed, invalidated)) = body.deserialize::<Changed<'_>>() else {
        return false;
    };
    changed.contains_key("PrimaryConnection") || invalidated.contains(&"PrimaryConnection")
}
