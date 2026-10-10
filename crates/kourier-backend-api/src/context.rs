//! What the core gives an account actor ([`AccountContext`]), what it gets back
//! ([`AccountHandle`], [`CoreEnds`]) and how the actor ends ([`Exit`]) (architecture §4.2, §4.3).

use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use kourier_model::{
    AccountId, AccountLocalPrefs, AccountUuid, BackendError, ConnState, ConvKey, Redacted,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::clock::Clock;
use crate::command::Command;
use crate::event::Envelope;

/// Capacity of the command channel (core → backend).
pub const COMMAND_QUEUE: usize = 64;
/// Capacity of the envelope channel (backend → core): the forwarder plus ephemeral events.
pub const EVENT_QUEUE: usize = 8;

/// How an account actor starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StartMode {
    /// New or relinked account: an empty `backend.db`, login comes first.
    Fresh,
    /// Normal start with stored credentials.
    Resume,
    /// `core.db` was rebuilt: re-emit mirrored state (contacts, groups, settings) into
    /// `core_outbox` (§4.3 step 8).
    Rebuild,
}

/// The `backend.db` key, unwrapped from `accounts/<uuid>/key.wrap` (§5.2). Zeroized on drop;
/// `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct DbKey(Zeroizing<[u8; 32]>);

impl DbKey {
    /// Wraps raw key bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The raw key (SQLCipher raw-key form). Never log it.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for DbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DbKey([redacted])")
    }
}

/// Backend runtime configuration (§9.2). The environment can be changed only by flags or env
/// variables in debug builds (`KOURIER_SIGNAL_ENV`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendConfig {
    /// Which servers to talk to.
    pub env: BackendEnv,
    /// Outgoing proxy.
    pub proxy: Option<ProxyConfig>,
    /// `tracing` filter for the backend's targets (`KOURIER_LOG` syntax).
    pub log_filter: Option<String>,
    /// How much to sync before login reports `Done` (§6.5 step 8).
    pub initial_sync: InitialSync,
    /// What a first link must find (§4.4): the last digits of the phone number of the account
    /// the user means to link. A backend that learns the account's number while linking
    /// compares it before it registers anything and fails the login on a mismatch, so a QR
    /// code scanned by the wrong phone links nothing. `None`: any account. Only digits count;
    /// a relink is checked against the account's own ids instead (§4.3 step 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_number_suffix: Option<Redacted<String>>,
}

/// Server environment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendEnv {
    /// The real service.
    #[default]
    Production,
    /// The network's staging service.
    Staging,
    /// A local test server (Signal: `@signalapp/mock-server`).
    LocalTest(LocalTestEnv),
}

/// Connection details of a local test server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTestEnv {
    /// Host name or address.
    pub host: String,
    /// Port.
    pub port: u16,
    /// PEM file of the server's CA certificate.
    #[serde(default)]
    pub ca_cert: Option<PathBuf>,
    /// The server's trust root public key (base64; Signal sealed-sender certificates).
    #[serde(default)]
    pub trust_root: Option<String>,
    /// The server's zkgroup public params (base64).
    #[serde(default)]
    pub zk_params: Option<String>,
}

/// An outgoing proxy. The URL may carry credentials, so it is redacted in `Debug`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// `socks5://…` or `http://…`.
    pub url: Redacted<String>,
}

/// What a fresh login syncs before it reports `Done`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InitialSync {
    /// `Done` right after the sync requests are sent (tests, until contact sync exists).
    LinkOnly,
    /// Wait for contact sync and Storage Service sync (production).
    #[default]
    ContactsAndStorage,
}

/// What the UI shows right now (a `watch` value: state, not events). WhatsApp presence and
/// chat-state subscriptions depend on it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPresence {
    /// A Kourier window has focus.
    pub focused: bool,
    /// The open conversation of this account, if any.
    pub visible: Option<ConvKey>,
    /// The session is idle.
    pub idle: bool,
}

/// The Kourier-local preferences a backend may read (never synced; synced settings arrive as
/// `BackendEvent::AccountSettings`).
pub type LocalPrefsView = AccountLocalPrefs;

/// Everything an account actor gets from the core. Build it with [`AccountContext::new`].
pub struct AccountContext {
    /// Core rowid of the account.
    pub account: AccountId,
    /// Stable account uuid.
    pub uuid: AccountUuid,
    /// How to start.
    pub start: StartMode,
    /// `accounts/<uuid>/backend.db`, exclusive to this backend.
    pub backend_db: PathBuf,
    /// Its key.
    pub db_key: DbKey,
    /// `accounts/<uuid>/blobs` (written only through `kourier-blob`).
    pub blob_dir: PathBuf,
    /// The shared multi-thread runtime for network I/O.
    pub net_rt: tokio::runtime::Handle,
    /// Commands from the core (bounded, [`COMMAND_QUEUE`]).
    pub commands: mpsc::Receiver<Command>,
    /// Envelopes to the core (bounded, [`EVENT_QUEUE`]).
    pub events: mpsc::Sender<Envelope>,
    /// Connection state, published by the backend.
    pub conn: watch::Sender<ConnState>,
    /// UI presence.
    pub ui: watch::Receiver<UiPresence>,
    /// Kourier-local preferences.
    pub local_prefs: watch::Receiver<LocalPrefsView>,
    /// Child of the app token; cancelled after a shutdown grace period (§4.7).
    pub cancel: CancellationToken,
    /// Time source for every timer.
    pub clock: Arc<dyn Clock>,
    /// Runtime configuration.
    pub config: BackendConfig,
}

/// The non-channel inputs of [`AccountContext::new`].
pub struct ContextParams {
    /// Core rowid of the account.
    pub account: AccountId,
    /// Stable account uuid.
    pub uuid: AccountUuid,
    /// How to start.
    pub start: StartMode,
    /// `accounts/<uuid>/backend.db`.
    pub backend_db: PathBuf,
    /// Its key.
    pub db_key: DbKey,
    /// `accounts/<uuid>/blobs`.
    pub blob_dir: PathBuf,
    /// The shared network runtime.
    pub net_rt: tokio::runtime::Handle,
    /// Child of the app token.
    pub cancel: CancellationToken,
    /// Time source.
    pub clock: Arc<dyn Clock>,
    /// Runtime configuration.
    pub config: BackendConfig,
    /// Initial UI presence.
    pub ui: UiPresence,
    /// Initial local preferences.
    pub local_prefs: LocalPrefsView,
}

/// The core's ends of an account's channels.
#[derive(Debug)]
pub struct CoreEnds {
    /// Commands to the backend.
    pub commands: mpsc::Sender<Command>,
    /// Envelopes from the backend.
    pub events: mpsc::Receiver<Envelope>,
    /// Connection state.
    pub conn: watch::Receiver<ConnState>,
    /// UI presence updates.
    pub ui: watch::Sender<UiPresence>,
    /// Local preference updates.
    pub local_prefs: watch::Sender<LocalPrefsView>,
}

impl AccountContext {
    /// Creates the account's channels: the backend ends go into the context (passed to
    /// `Network::spawn`), the core keeps the other ends. The connection starts `Offline`.
    pub fn new(p: ContextParams) -> (Self, CoreEnds) {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
        let (ev_tx, ev_rx) = mpsc::channel(EVENT_QUEUE);
        let (conn_tx, conn_rx) = watch::channel(ConnState::default());
        let (ui_tx, ui_rx) = watch::channel(p.ui);
        let (prefs_tx, prefs_rx) = watch::channel(p.local_prefs);
        let ctx = Self {
            account: p.account,
            uuid: p.uuid,
            start: p.start,
            backend_db: p.backend_db,
            db_key: p.db_key,
            blob_dir: p.blob_dir,
            net_rt: p.net_rt,
            commands: cmd_rx,
            events: ev_tx,
            conn: conn_tx,
            ui: ui_rx,
            local_prefs: prefs_rx,
            cancel: p.cancel,
            clock: p.clock,
            config: p.config,
        };
        let ends = CoreEnds {
            commands: cmd_tx,
            events: ev_rx,
            conn: conn_rx,
            ui: ui_tx,
            local_prefs: prefs_tx,
        };
        (ctx, ends)
    }
}

impl fmt::Debug for AccountContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountContext")
            .field("account", &self.account)
            .field("uuid", &self.uuid.short())
            .field("start", &self.start)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// What `Network::spawn` returns; the supervisor awaits `exited`.
#[derive(Debug)]
pub struct AccountHandle {
    /// Resolves when the actor ends. A closed channel means it died without reporting; read it
    /// with [`Exit::from_channel`].
    pub exited: oneshot::Receiver<Exit>,
    /// The actor's OS thread, joined after shutdown (§4.7); `None` if it runs on a shared
    /// runtime.
    pub thread: Option<JoinHandle<()>>,
}

/// How an account actor ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Exit {
    /// After `Command::Shutdown`.
    Shutdown,
    /// The device was unlinked (or `Command::Logout` finished); the core wipes the account's
    /// secrets (§4.3 step 5).
    LoggedOut {
        /// Log-safe reason.
        reason: String,
    },
    /// A fatal error.
    Failed(BackendError),
    /// The actor panicked.
    Panicked(String),
}

impl Exit {
    /// Reads an `exited` result: a dropped sender counts as a panic.
    pub fn from_channel(r: Result<Exit, oneshot::error::RecvError>) -> Self {
        r.unwrap_or_else(|_| Self::Panicked("actor ended without reporting an exit".into()))
    }

    /// Whether the supervisor restarts the actor (with backoff, §4.3 step 4).
    pub fn should_restart(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Panicked(_))
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Runs an account actor on its own OS thread named `name` (Signal: `signal-<uuid8>`). The
/// body's return value, or `Exit::Panicked` if it panics, is sent on the handle's `exited`.
pub fn spawn_account_thread<F>(name: String, body: F) -> Result<AccountHandle, BackendError>
where
    F: FnOnce() -> Exit + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    // std panics on a thread name with a NUL byte; drop them instead.
    let name = name.replace('\0', "");
    let thread = std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let exit = match catch_unwind(AssertUnwindSafe(body)) {
                Ok(exit) => exit,
                Err(payload) => Exit::Panicked(panic_message(payload.as_ref())),
            };
            // The supervisor may be gone already (app shutdown); nothing left to tell.
            let _ = tx.send(exit);
        })
        .map_err(|e| BackendError::internal(format!("spawning the account thread: {e}")))?;
    Ok(AccountHandle {
        exited: rx,
        thread: Some(thread),
    })
}
