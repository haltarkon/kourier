//! Starting and stopping Kourier (§2.5, §8.2): the tokio runtime, paths, the master key, the
//! network registry, the core; the [`AppHandle`] the UI talks through; orderly shutdown.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendConfig, Clock, NetworkRegistry, SystemClock};
use kourier_core::facade::{CoreHandle, Query, QueryData, UiCommand, UiEvent};
use kourier_core::{Core, CoreConfig, CoreError};
use kourier_desktop::secret::SecretService;
use kourier_model::{ConversationId, LoginFlow, NetworkKind, RiskNotice};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::{self, ConfigError};
use crate::desktop::{self, Connected, DesktopHandle, DesktopOptions};
use crate::keys::{KeyError, Keyring};
use crate::paths::{PathError, Paths, Profile};
use crate::vault::{AnyKeyring, AppVault};

/// How long the core may take to stop its accounts at shutdown before they are cancelled.
pub const CORE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the runtime may take to finish its tasks after the core stopped (§8.2 step 5).
pub const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long closing notifications and the tray may take at shutdown.
pub const DESKTOP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Why Kourier could not start. Every variant is log-safe and fit to show the user.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The tokio runtime could not be built.
    #[error("cannot start the runtime: {0}")]
    Runtime(std::io::Error),
    /// The data directories cannot be located.
    #[error(transparent)]
    Paths(#[from] PathError),
    /// The data directories cannot be created.
    #[error("cannot create the data directories: {0}")]
    Dirs(std::io::Error),
    /// No usable Secret Service. The passphrase dialog (§5.2) is not implemented yet.
    #[error(
        "no usable Secret Service ({0}); Kourier keeps its database key there. Unlock or install a Secret Service provider (KWallet or GNOME Keyring); passphrase mode is not available yet"
    )]
    NoSecretService(KeyError),
    /// The master key or the core.db key could not be loaded.
    #[error(transparent)]
    Keys(#[from] KeyError),
    /// The backend environment from `KOURIER_SIGNAL_ENV` is unusable.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The core did not start (core.db, migrations, reconciliation).
    #[error("cannot open the message store: {0}")]
    Core(#[from] CoreError),
    /// Fixture mode needs the `fake` cargo feature.
    #[error("fixture mode needs a build with the `fake` feature")]
    FixtureUnavailable,
    /// Fixture mode could not seed its data.
    #[error("fixture mode: {0}")]
    Fixture(String),
    /// Another process holds this profile's lock ([`crate::paths::ProfileLock`]) and could not
    /// be asked to show its window (no shared session bus). Nothing was opened or changed.
    #[error(
        "another Kourier process is using this profile; stop it first (two processes on one profile would break its sessions)"
    )]
    ProfileInUse,
    /// Kourier already runs for this profile; it was asked to show its window (or open the
    /// URIs; a `--background` launch asks nothing). The launch should exit successfully.
    #[error("Kourier is already running for this profile")]
    AlreadyRunning,
    /// The running instance could not be reached.
    #[error("Kourier is already running but did not answer: {0}")]
    Instance(kourier_desktop::DesktopError),
}

/// What [`App::start`] runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Launch {
    /// The user's profile with the compiled-in networks.
    #[default]
    Normal,
    /// A throwaway profile in a temporary `KOURIER_HOME` with only the fake network (§10.2).
    Fixture(FixtureOptions),
    /// The reference profile of the quality targets (§1.3) in a directory of its own, with
    /// only the fake network; an empty one is seeded first ([`crate::perf`]). For
    /// `cargo xtask perf`.
    #[cfg(feature = "perf")]
    Perf(crate::perf::PerfOptions),
}

/// Fixture mode settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureOptions {
    /// Start without an account (the welcome page) and link the fake account through its
    /// rotating QR codes; otherwise the account is linked and its data ingested before the UI
    /// starts.
    pub link: bool,
    /// How long each QR code of the linking walk stays valid.
    pub qr_valid_for: Duration,
    /// QR codes shown before the phone "scans" the last one.
    pub qr_codes: u32,
    /// Messages per fixture conversation.
    pub messages_per_conversation: usize,
    /// Adds the showcase chats on top of the generated ones: quotes of a known, a missing and
    /// a deleted message with an edited photo caption, a chat with a disappearing-message
    /// timer, and a chat with stand-ins for content Kourier does not show yet.
    pub showcase: bool,
    /// The state the account ends in once its data is delivered (the account banners).
    pub state: FixtureState,
    /// With the showcase chats: one more received and read message in the timer chat,
    /// [`EXPIRING_TEXT`], that disappears this long after it was delivered, a few seconds
    /// into the run (tests of what happens to a message whose time is up). `None`: no such
    /// message.
    pub expiring: Option<Duration>,
    /// With the showcase chats, a linked start and [`FixtureState::Online`]: the peer of the
    /// timer chat keeps writing after the start (a chat that receives while it is read).
    /// `None`: nothing arrives.
    pub arrivals: Option<Arrivals>,
    /// With the showcase chats: the missed call of the chat with calls is unread
    /// (`--page=calls`; in every other run it is read, so no other picture counts it).
    pub calls_unread: bool,
    /// With the showcase chats: the last call of the chat with calls starts at the fixture's
    /// "now", rings for this long (at most [`FixtureOptions::RING`]) and then turns into a
    /// missed call; its chat is then the first of the list below the pinned ones
    /// (`--page=calls`). `None`: no call
    /// rings and the chat keeps its place below the fold, so no other picture changes.
    pub calls_ring_for: Option<Duration>,
    /// The history transfer of the fixture account (architecture §14.2 item 14); `None`:
    /// none, as before.
    pub transfer: Option<FixtureTransfer>,
    /// The fixture network says that it hands a newly linked device the account's history
    /// (`HistorySource::PushedOnLink`): its account offers "Transfer History from Phone…".
    /// Always so when [`Self::transfer`] is set.
    pub history_on_link: bool,
    /// How long each stage of a [`FixtureTransfer::Walk`] lasts (tests shorten it).
    pub transfer_stage: Duration,
}

/// The history transfer of fixture mode ([`FixtureOptions::transfer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureTransfer {
    /// With [`FixtureOptions::link`]: the link walks a whole transfer (waiting for the
    /// phone, a download whose size becomes known, the read; a few seconds each) up to the
    /// point where this end comes: `Done`, `Skipped` (the phone's user declined) or a
    /// failure of a kind. "Continue Without History" ends it sooner.
    Walk(kourier_model::HistoryTransfer),
    /// With [`FixtureOptions::link`]: the link reaches this state (waiting, downloading,
    /// reading) and stays there until it is skipped.
    Hold(kourier_model::HistoryTransfer),
    /// Without [`FixtureOptions::link`]: the linked fixture account stands at this state,
    /// as after a restart of the application in the middle of a transfer, or after one
    /// that ended and was not dismissed. The backend reports it once the UI is up.
    Left(kourier_model::HistoryTransfer),
}

impl FixtureTransfer {
    /// How long each stage of [`FixtureTransfer::Walk`] lasts unless
    /// [`FixtureOptions::transfer_stage`] says otherwise.
    pub const STAGE: Duration = Duration::from_secs(4);
}

impl FixtureOptions {
    /// The longest a call rings (`kourier_model::MAX_RING_MS`): what `--page=calls` asks for,
    /// long enough for a picture.
    pub const RING: Duration = Duration::from_millis(kourier_model::MAX_RING_MS.unsigned_abs());
}

/// Messages that arrive in the fixture's timer chat after the start
/// ([`FixtureOptions::arrivals`]), on the fixture's clock. They are not part of the data a
/// start waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrivals {
    /// How many messages arrive; their texts are `"Arrival 1"`, `"Arrival 2"`, ….
    pub count: u32,
    /// The time between two of them (and before the first).
    pub every: Duration,
    /// Each one is read on arrival and disappears this long after it; `None`: they stay.
    pub lifetime: Option<Duration>,
}

/// The text of the fixture message that comes due shortly after the start
/// ([`FixtureOptions::expiring`]).
pub const EXPIRING_TEXT: &str = "Gone in a moment.";

impl Default for FixtureOptions {
    fn default() -> Self {
        Self {
            link: false,
            qr_valid_for: Duration::from_secs(5),
            qr_codes: 3,
            messages_per_conversation: 120,
            showcase: true,
            state: FixtureState::Online,
            expiring: None,
            arrivals: None,
            calls_unread: false,
            calls_ring_for: None,
            transfer: None,
            history_on_link: false,
            transfer_stage: FixtureTransfer::STAGE,
        }
    }
}

/// Where the fixture account ends up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureState {
    /// Connected and caught up.
    #[default]
    Online,
    /// Not connected.
    Offline,
    /// Connecting.
    Connecting,
    /// The network asks for a challenge to be solved in the browser (any answer is accepted).
    Challenge,
    /// The system clock is two hours ahead.
    ClockSkew,
    /// The device was unlinked; its history stays and "Link Again" works.
    LoggedOut,
}

/// How to start.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartOptions {
    /// `--profile <name>`.
    pub profile: Profile,
    /// Normal or fixture mode.
    pub launch: Launch,
    /// Desktop integration (single instance, notifications, tray).
    pub desktop: DesktopOptions,
}

/// The environment variable that switches the history transfer of a new link ON for one run
/// (`KOURIER_HISTORY_TRANSFER=1`), in every build. Without it Signal's backend offers no
/// transfer and no "Transfer History from Phone…": the import is not whole yet (architecture
/// §14.2 item 14), and a phone that is asked exports and uploads its whole history. The
/// switch is for the runs that try the import; it becomes the default, and this variable
/// goes away, when the import is whole.
pub const HISTORY_TRANSFER_ENV: &str = "KOURIER_HISTORY_TRANSFER";

/// Whether the value of [`HISTORY_TRANSFER_ENV`] asks for the transfer: exactly `1`.
#[cfg_attr(not(feature = "signal"), allow(dead_code))]
fn history_transfer_asked(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// A network the user can add an account of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkInfo {
    /// The network.
    pub kind: NetworkKind,
    /// Its display name from the descriptor.
    pub name: String,
    /// Its login flows (the first one is the default).
    pub flows: Vec<LoginFlow>,
    /// A notice the user must accept before linking (§7.4).
    pub risk_notice: Option<RiskNotice>,
    /// Where the network's history comes from, as its descriptor says
    /// (`HistorySource::PushedOnLink`: a newly linked device is handed the account's
    /// history, so a UI offers "Transfer History from Phone…" for its accounts).
    pub history: kourier_model::HistorySource,
}

/// What fixture mode knows about its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureInfo {
    /// A fixed "now" for time labels: screenshots do not depend on the day they are taken.
    pub now_ms: i64,
    /// The conversation `--page=chat` opens (the first group).
    pub demo_conversation: Option<ConversationId>,
    /// A conversation with a photo, a file and a voice note (`--page=media`, `viewer`,
    /// `compose`).
    pub media_conversation: Option<ConversationId>,
    /// The showcase chat with quotes, an edited caption and recent own messages
    /// ([`FixtureOptions::showcase`]).
    pub quotes_conversation: Option<ConversationId>,
    /// The showcase chat with a disappearing-message timer.
    pub timer_conversation: Option<ConversationId>,
    /// The showcase chat with stand-ins for content Kourier does not show yet.
    pub kinds_conversation: Option<ConversationId>,
    /// The showcase chat with albums of two, three, four and many pictures and a text that
    /// was edited twice (`--page=album`, `albumviewer`, `revisions`).
    pub albums_conversation: Option<ConversationId>,
    /// The showcase chat with one long text, shown cut with "Read more" (`--page=readmore`).
    pub readmore_conversation: Option<ConversationId>,
    /// The direct chat with a call line of every state of a 1:1 call, the last one ringing
    /// (`--page=calls`).
    pub calls_conversation: Option<ConversationId>,
}

/// Facts about the running app, the same for every [`AppHandle`] clone.
#[derive(Debug)]
struct AppInfo {
    networks: Vec<NetworkInfo>,
    hostname: String,
    fixture: Option<FixtureInfo>,
    fixture_mode: bool,
    /// The fake network's control, in fixture mode (what its accounts were told).
    #[cfg(feature = "fake")]
    fake: Option<Arc<kourier_fake::FakeControl>>,
    /// What seeding the reference profile took, when this start seeded it.
    #[cfg(feature = "perf")]
    seeded: Option<crate::perf::SeedReport>,
}

/// The UI's handle to the running app: commands, events, queries and a few facts. Cloneable
/// and cheap; never blocks.
#[derive(Clone, Debug)]
pub struct AppHandle {
    core: CoreHandle,
    runtime: tokio::runtime::Handle,
    shutdown: CancellationToken,
    info: Arc<AppInfo>,
    desktop: DesktopHandle,
    avatars: Arc<crate::media::AvatarHandles>,
    pasted: Arc<crate::media::PastedFiles>,
    archives: Arc<crate::folders::ArchiveFiles>,
}

impl AppHandle {
    /// The desktop integration: preferences, events (activation, tray), window state.
    pub fn desktop(&self) -> &DesktopHandle {
        &self.desktop
    }

    /// Sends a command to the core without blocking. Fails only when the core has stopped.
    pub fn send(&self, cmd: UiCommand) -> Result<(), CoreError> {
        self.core.send(cmd)
    }

    /// Subscribes to the core's events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.core.subscribe()
    }

    /// Runs a query and awaits the rows (tests and app-side tasks; the UI uses
    /// `UiCommand::Query`).
    pub async fn query(&self, query: Query) -> Result<QueryData, CoreError> {
        self.core.query(query).await
    }

    /// The runtime UI-side tasks (the event pump) run on.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    /// Cancelled when the app shuts down: producers feeding the UI stop on it (§8.2 step 5).
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// The compiled-in networks, in registration order.
    pub fn networks(&self) -> &[NetworkInfo] {
        &self.info.networks
    }

    /// This computer's host name (the default device name when linking).
    pub fn hostname(&self) -> &str {
        &self.info.hostname
    }

    /// Fixture facts, once the fixture data is in `core.db`.
    pub fn fixture(&self) -> Option<FixtureInfo> {
        self.info.fixture
    }

    /// Whether this is fixture mode (with or without seeded data).
    pub fn is_fixture(&self) -> bool {
        self.info.fixture_mode
    }

    /// Fixture mode: the names of the commands the fake network's accounts received so far
    /// (`Connect`, `SkipHistoryTransfer`, `DismissHistoryTransfer`, …), in order. Tests of
    /// the UI count them; empty outside fixture mode.
    #[cfg(feature = "fake")]
    pub fn fixture_commands(&self) -> Vec<&'static str> {
        self.info
            .fake
            .as_ref()
            .map(|c| c.all_commands())
            .unwrap_or_default()
    }

    /// What seeding the reference profile took, when this start seeded it (`Launch::Perf` on
    /// an empty profile).
    #[cfg(feature = "perf")]
    pub fn seeded(&self) -> Option<crate::perf::SeedReport> {
        self.info.seeded
    }

    /// The provider URL (`image://kourier/avatar/<handle>?v=<rev>`) of a contact's or
    /// conversation's avatar file; `None` for a path outside the account's blob directory.
    /// Never blocks (see [`crate::media::AvatarHandles`]).
    pub fn avatar_url(
        &self,
        account: &kourier_model::AccountUuid,
        avatar: &kourier_model::BlobRef,
    ) -> Option<String> {
        self.avatars.url(account, avatar)
    }

    /// Decrypts the avatar `handle` of an `image://kourier/avatar/…` URL into memory.
    /// Blocking: for the image provider's worker thread only.
    pub fn read_avatar(
        &self,
        handle: u64,
    ) -> Result<zeroize::Zeroizing<Vec<u8>>, crate::media::MediaError> {
        self.avatars.read(handle)
    }

    /// The core's media handle table (attachments of the `image://kourier/{att,thumb,blurhash}`
    /// URLs, §8.6).
    pub fn media(&self) -> kourier_core::facade::MediaHandles {
        self.core.media()
    }

    /// Writes a file pasted into the composer (`name` is a plain file name) into the tmpfs
    /// media cache and returns its path for `UiCommand::SendFiles`. Blocking (small files).
    pub fn save_pasted(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<std::path::PathBuf, crate::media::MediaError> {
        self.pasted.save(name, bytes)
    }

    /// Deletes a file Kourier made for the composer (a pasted picture, the archive of a
    /// folder) once the composer no longer needs it; other paths are ignored. Never waits
    /// for a disk.
    pub fn release_pasted(&self, path: &std::path::Path) -> bool {
        if self.pasted.release(path) {
            return true;
        }
        // An archive is on the disk and can be large: it is forgotten now and deleted on a
        // worker thread (the UI thread calls this). Should the runtime be closing already,
        // the wipe at exit removes it.
        match self.archives.take(path) {
            Some(place) => {
                self.runtime
                    .spawn_blocking(move || crate::folders::remove_place(&place));
                true
            }
            None => false,
        }
    }

    /// Whether `path` is a file Kourier made for the composer: one
    /// [`save_pasted`](Self::save_pasted) wrote, or an archive at a path
    /// [`reserve_archive`](Self::reserve_archive) handed out (a refused send hands its files
    /// back: these are deleted when they leave the composer).
    pub fn is_pasted(&self, path: &std::path::Path) -> bool {
        self.pasted.owns(path) || self.archives.owns(path)
    }

    /// Where the archive of a folder called `folder` is to be written
    /// ([`crate::folders::pack`]): a new private place on the disk, deleted like a pasted
    /// file ([`release_pasted`](Self::release_pasted), at exit, at the next start).
    pub fn reserve_archive(
        &self,
        folder: &str,
    ) -> Result<std::path::PathBuf, crate::folders::FolderError> {
        self.archives.reserve(folder)
    }
}

/// The running app. Call [`App::shutdown`] after the UI's event loop returned; dropping it
/// shuts down too.
pub struct App {
    runtime: Option<tokio::runtime::Runtime>,
    core: Option<Core>,
    handle: AppHandle,
    paths: Paths,
    desktop_task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(feature = "fake")]
    _fixture_home: Option<tempfile::TempDir>,
    /// The profile's lock, released when the app is gone (after `stop`, which `Drop` runs).
    _profile_lock: Option<crate::paths::ProfileLock>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// Sets the process umask to 077: every file and directory Kourier creates is private (§9.4).
pub fn set_private_umask() {
    // SAFETY: umask only swaps the process file mode creation mask; it cannot fail.
    unsafe {
        libc::umask(0o077);
    }
}

/// This computer's host name, or `Kourier` when it has none.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; gethostname writes at most that many.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let name = if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).trim().to_owned()
    } else {
        String::new()
    };
    if name.is_empty() {
        "Kourier".to_owned()
    } else {
        name
    }
}

fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(2)
        .clamp(1, 4)
}

fn network_info(registry: &NetworkRegistry) -> Vec<NetworkInfo> {
    registry
        .iter()
        .map(|n| {
            let d = n.descriptor();
            NetworkInfo {
                kind: n.kind(),
                name: d.display_name.clone(),
                flows: d.login_flows.clone(),
                risk_notice: d.risk_notice.clone(),
                history: d.history,
            }
        })
        .collect()
}

/// What `start` builds before the core runs.
struct Setup {
    paths: Paths,
    keyring: AnyKeyring,
    registry: NetworkRegistry,
    backend: BackendConfig,
    #[cfg(feature = "fake")]
    fake: Option<kourier_fake::FakeNetwork>,
    #[cfg(feature = "fake")]
    home: Option<tempfile::TempDir>,
    /// Fixture profile pictures, written once the fixture account exists.
    #[cfg(feature = "fake")]
    avatars: crate::fixture_media::FixtureAvatars,
    /// The core's clock: the system clock, or in fixture mode one near the fixture's dates.
    clock: Arc<dyn Clock>,
    /// Fixture mode's "now" (ms): a little after the newest fixture message.
    #[cfg(feature = "fake")]
    fixture_now: Option<i64>,
    /// The steps of the fixture script that are its data, when more follow
    /// ([`FixtureOptions::arrivals`]).
    #[cfg(feature = "fake")]
    fixture_seed: Option<usize>,
    /// The peer of the fixture's chat with calls.
    #[cfg(feature = "fake")]
    fixture_calls: Option<kourier_model::UserId>,
    /// An empty reference profile to seed: what to remember of its script, and its root.
    #[cfg(feature = "perf")]
    perf_seed: Option<(crate::perf::Reference, std::path::PathBuf)>,
}

impl App {
    /// Starts Kourier: umask 077, the runtime "kourier-rt" (`min(4, cores)` workers), paths,
    /// logging, the master key, the networks, the core (§8.2 step 2). Backends do not connect
    /// until the UI sends `UiCommand::UiReady` after its first frame. Call it on the main
    /// thread before the UI starts; it blocks while the keyring answers and `core.db` opens
    /// (and, in fixture mode, while the fixture data is ingested).
    pub fn start(opts: StartOptions) -> Result<Self, AppError> {
        set_private_umask();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads())
            .thread_name("kourier-rt")
            .enable_all()
            .build()
            .map_err(AppError::Runtime)?;
        // §8.2 step 1: a second launch of a profile hands over to the first and exits.
        let instance = match &opts.launch {
            Launch::Normal => Some(desktop::instance_name(&Paths::resolve(
                opts.profile.clone(),
            )?)),
            Launch::Fixture(_) => None,
            // Like fixture mode: a measuring run claims no name and takes no requests.
            #[cfg(feature = "perf")]
            Launch::Perf(_) => None,
        };
        let connection =
            match runtime.block_on(desktop::connect(&opts.desktop, instance.as_deref())) {
                Ok(Connected::Run(c)) => c,
                Ok(Connected::Forwarded) => return Err(AppError::AlreadyRunning),
                Err(e) => return Err(AppError::Instance(e)),
            };
        // One process per profile, also where no bus name says so (offscreen, no session
        // bus): taken before a log file, a key or a database of the profile is opened, and
        // held until the app is gone. Fixture and measuring runs have directories of their
        // own per run.
        let profile_lock = match &opts.launch {
            Launch::Normal => {
                let paths = Paths::resolve(opts.profile.clone())?;
                Some(
                    crate::paths::ProfileLock::acquire(&paths).map_err(|e| match e {
                        crate::paths::LockError::Held => AppError::ProfileInUse,
                        crate::paths::LockError::Io(e) => AppError::Dirs(e),
                    })?,
                )
            }
            Launch::Fixture(_) => None,
            #[cfg(feature = "perf")]
            Launch::Perf(_) => None,
        };
        #[allow(unused_mut)]
        let mut setup = match &opts.launch {
            Launch::Normal => runtime.block_on(normal_setup(&opts.profile))?,
            Launch::Fixture(f) => fixture_setup(f)?,
            #[cfg(feature = "perf")]
            Launch::Perf(p) => perf_setup(p)?,
        };
        // An empty reference profile is seeded once the core runs.
        #[cfg(feature = "perf")]
        let perf_seed = setup.perf_seed.take();
        let core_key = setup.keyring.core_db_key();
        let keyring = Arc::new(setup.keyring);
        let vault = Arc::new(AppVault::new(keyring, setup.paths.clone()));
        let networks = network_info(&setup.registry);
        let shutdown = CancellationToken::new();
        let mut cfg = CoreConfig::new(
            setup.paths.core_db(),
            kourier_db::DbKey::from_bytes(*core_key),
            setup.registry,
            vault,
            setup.clock.clone(),
            runtime.handle().clone(),
        );
        cfg.backend = setup.backend;
        // Plaintext playback copies live under $XDG_RUNTIME_DIR (tmpfs); the core uses its
        // `attachments/` subdirectory only (§5.8).
        cfg.media_cache = Some(setup.paths.media_dir());
        cfg.cancel = shutdown.child_token();
        let core = runtime.block_on(Core::start(cfg))?;
        let handle = core.handle();
        #[cfg(feature = "fake")]
        let fixture = match (&opts.launch, &setup.fake) {
            (Launch::Fixture(f), Some(fake)) if !f.link => Some(runtime.block_on(seed_fixture(
                &handle,
                fake,
                &setup.paths,
                &setup.avatars,
                setup.fixture_now,
                setup.fixture_seed,
                setup.fixture_calls.clone(),
            ))?),
            _ => None,
        };
        // ("Link Again" of the fixture's unlinked account needs nothing here: the fake does
        // not replay a script the account already ran, so the relinked account comes online
        // with its history in `core.db`.)
        #[cfg(not(feature = "fake"))]
        let fixture = None;
        #[cfg(feature = "perf")]
        let seeded =
            match (perf_seed, &setup.fake) {
                (Some((reference, home)), Some(fake)) => Some(runtime.block_on(
                    crate::perf::seed(&handle, fake, &setup.paths, &reference, &home),
                )?),
                _ => None,
            };
        crate::trace::mark_with_cpu("startup.app_started", &[]);
        tracing::info!(
            networks = networks.len(),
            fixture = matches!(opts.launch, Launch::Fixture(_)),
            "core started"
        );
        let (desktop, desktop_task) = desktop::spawn(
            runtime.handle(),
            handle.clone(),
            connection,
            &opts.desktop,
            &setup.paths,
            shutdown.child_token(),
        );
        // Resume and network hints for the backends (§4.7); the task ends with `shutdown`.
        desktop::spawn_hints(
            runtime.handle(),
            handle.clone(),
            opts.desktop.system_bus.clone(),
            shutdown.child_token(),
        );
        let avatars = Arc::new(crate::media::AvatarHandles::new(
            &setup.paths.accounts_dir(),
        ));
        let pasted = Arc::new(crate::media::PastedFiles::new(&setup.paths.media_dir()));
        // (Emptied now: what a run that crashed left behind goes.)
        let archives = Arc::new(crate::folders::ArchiveFiles::new(
            &setup.paths.archives_dir(),
        ));
        let handle = AppHandle {
            core: handle,
            runtime: runtime.handle().clone(),
            desktop,
            avatars,
            pasted,
            archives,
            shutdown,
            info: Arc::new(AppInfo {
                networks,
                hostname: hostname(),
                fixture,
                fixture_mode: matches!(opts.launch, Launch::Fixture(_)),
                #[cfg(feature = "fake")]
                fake: setup.fake.as_ref().map(kourier_fake::FakeNetwork::control),
                #[cfg(feature = "perf")]
                seeded,
            }),
        };
        Ok(Self {
            runtime: Some(runtime),
            core: Some(core),
            handle,
            paths: setup.paths,
            desktop_task: Some(desktop_task),
            #[cfg(feature = "fake")]
            _fixture_home: setup.home,
            _profile_lock: profile_lock,
        })
    }

    /// The handle the UI uses.
    pub fn handle(&self) -> AppHandle {
        self.handle.clone()
    }

    /// The profile's directories.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Orderly shutdown (§8.2 step 5): cancel the producers that feed the UI, stop the core
    /// (accounts get `Shutdown`, queued writes finish, `core.db` closes) within
    /// [`CORE_SHUTDOWN_TIMEOUT`], then give the runtime [`RUNTIME_SHUTDOWN_TIMEOUT`].
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        self.handle.shutdown.cancel();
        // The desktop task closes our notifications and removes the tray icon first.
        if let Some(task) = self.desktop_task.take() {
            let _ = runtime
                .block_on(async { tokio::time::timeout(DESKTOP_SHUTDOWN_TIMEOUT, task).await });
        }
        if let Some(core) = self.core.take() {
            let done = runtime.block_on(async {
                tokio::time::timeout(CORE_SHUTDOWN_TIMEOUT, core.shutdown()).await
            });
            if done.is_err() {
                tracing::warn!("the core did not stop in time; cancelling its accounts");
            }
        }
        runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        // Pasted composer files are plaintext in tmpfs: they go at exit (§5.8). So do the
        // archives of folders, which are plaintext on the disk.
        self.handle.pasted.clear();
        self.handle.archives.clear();
        tracing::info!("stopped");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The user's profile: paths, logging, the Secret Service master key, compiled-in networks.
async fn normal_setup(profile: &Profile) -> Result<Setup, AppError> {
    let paths = Paths::resolve(profile.clone())?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    let backend = config::backend_config(|k| std::env::var(k).ok(), &paths.state)?;
    #[cfg(debug_assertions)]
    let dev_keyring = if crate::dev::wants_dev_keyring(|k| std::env::var_os(k)) {
        tracing::info!("development keyring (KOURIER_DEV_KEYRING); the Secret Service is not used");
        Some(crate::dev::open_keyring(&paths)?)
    } else {
        None
    };
    #[cfg(not(debug_assertions))]
    let dev_keyring: Option<Keyring<crate::keys::NoSecretStore>> = None;
    let keyring = match dev_keyring {
        Some(k) => AnyKeyring::Passphrase(k),
        None => AnyKeyring::SecretService(secret_service_keyring(&paths).await?),
    };
    #[allow(unused_mut)]
    let mut registry = NetworkRegistry::new();
    #[cfg(feature = "signal")]
    {
        let transfer = history_transfer_asked(std::env::var_os(HISTORY_TRANSFER_ENV).as_deref());
        if transfer {
            tracing::warn!(
                "history transfer at link time is switched on for this run (experimental: the import is not whole)"
            );
        }
        register(
            &mut registry,
            Arc::new(kourier_signal::SignalNetwork::new().with_history_transfer_offer(transfer)),
        );
    }
    #[cfg(feature = "fake")]
    let fake = {
        let demo = demo_network(&FixtureOptions {
            link: true,
            // Debug builds: the state the demo account ends in (tests of the account states
            // on a real profile, e.g. a challenge answered from a second launch).
            #[cfg(debug_assertions)]
            state: crate::dev::demo_state(|k| std::env::var_os(k)),
            ..FixtureOptions::default()
        })?;
        register(&mut registry, Arc::new(demo.network.clone()));
        Some(demo.network)
    };
    Ok(Setup {
        paths,
        keyring,
        registry,
        backend,
        #[cfg(feature = "fake")]
        fake,
        #[cfg(feature = "fake")]
        home: None,
        #[cfg(feature = "fake")]
        avatars: Default::default(),
        clock: Arc::new(SystemClock),
        #[cfg(feature = "fake")]
        fixture_now: None,
        #[cfg(feature = "fake")]
        fixture_seed: None,
        #[cfg(feature = "fake")]
        fixture_calls: None,
        #[cfg(feature = "perf")]
        perf_seed: None,
    })
}

/// This profile's master key from the Secret Service (created for a fresh profile).
async fn secret_service_keyring(paths: &Paths) -> Result<Keyring<SecretService>, AppError> {
    let store = SecretService::connect(false)
        .await
        .map_err(|e| AppError::NoSecretService(KeyError::Secret(e)))?;
    Keyring::with_secret_service(store, &paths.secret_profile(), !paths.has_data())
        .await
        .map_err(|e| match e {
            KeyError::Secret(_) => AppError::NoSecretService(e),
            other => AppError::Keys(other),
        })
}

#[cfg(any(feature = "signal", feature = "fake"))]
fn register(registry: &mut NetworkRegistry, network: Arc<dyn kourier_backend_api::Network>) {
    if let Err(e) = registry.register(network) {
        tracing::warn!(error = %e, "network registered twice; ignored");
    }
}

#[cfg(not(feature = "fake"))]
fn fixture_setup(_: &FixtureOptions) -> Result<Setup, AppError> {
    Err(AppError::FixtureUnavailable)
}

/// A throwaway profile below a temporary directory, a passphrase keyring with a fixed
/// passphrase (cheap KDF: nothing here is secret) and only the fake network.
#[cfg(feature = "fake")]
fn fixture_setup(f: &FixtureOptions) -> Result<Setup, AppError> {
    let home = tempfile::Builder::new()
        .prefix("kourier-fixture-")
        .tempdir()
        .map_err(AppError::Dirs)?;
    let home_os = home.path().as_os_str().to_owned();
    let paths = Paths::from_env(Profile::default(), |k| {
        (k == crate::paths::HOME_ENV).then(|| home_os.clone())
    })?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    let kdf = crate::keys::KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    let (keyring, _) = Keyring::create_passphrase(b"kourier fixture", kdf)?;
    let demo = demo_network(f)?;
    let mut registry = NetworkRegistry::new();
    register(&mut registry, Arc::new(demo.network.clone()));
    // The fixture's messages carry fixed dates: the core's clock starts a little after the
    // newest one, so recent own messages are inside the edit and delete windows and timers
    // have not run out (with the system clock everything would be months old).
    let clock: Arc<dyn Clock> = match demo.now_ms {
        Some(now) => Arc::new(crate::fixture_clock::FixtureClock::starting_at(now)),
        None => Arc::new(SystemClock),
    };
    Ok(Setup {
        paths,
        keyring: AnyKeyring::Passphrase(keyring),
        registry,
        backend: BackendConfig::default(),
        fake: Some(demo.network),
        home: Some(home),
        avatars: demo.avatars,
        clock,
        fixture_now: demo.now_ms,
        fixture_seed: demo.seed_steps,
        fixture_calls: demo.calls_peer,
        #[cfg(feature = "perf")]
        perf_seed: None,
    })
}

/// The reference profile of the quality targets below `p.home`: a passphrase keyring with a
/// fixed passphrase, only the fake network with the reference script, the system clock
/// ([`crate::perf`]).
#[cfg(feature = "perf")]
fn perf_setup(p: &crate::perf::PerfOptions) -> Result<Setup, AppError> {
    let fresh = crate::perf::is_fresh(p);
    let paths = crate::perf::paths(p)?;
    paths.create_dirs().map_err(AppError::Dirs)?;
    crate::logging::init(Some(&paths.log_dir()));
    let keyring = crate::perf::open_keyring(&paths.config)?;
    let net = crate::perf::network(p, fresh)?;
    let mut registry = NetworkRegistry::new();
    register(&mut registry, Arc::new(net.network.clone()));
    Ok(Setup {
        paths,
        keyring: AnyKeyring::Passphrase(keyring),
        registry,
        backend: BackendConfig::default(),
        fake: Some(net.network),
        home: None,
        avatars: Default::default(),
        clock: crate::perf::clock(p),
        fixture_now: None,
        fixture_seed: None,
        fixture_calls: None,
        perf_seed: net.seeding.map(|reference| (reference, p.home.clone())),
    })
}

/// How long after its newest message fixture mode's "now" is.
#[cfg(feature = "fake")]
const FIXTURE_NOW_AFTER_NEWEST_MS: i64 = 7 * 60_000;

/// The fake network of fixture mode and what belongs to its data.
#[cfg(feature = "fake")]
struct Demo {
    network: kourier_fake::FakeNetwork,
    /// Profile pictures to write once the account is linked.
    avatars: crate::fixture_media::FixtureAvatars,
    /// "Now" for this data: a little after its newest message.
    now_ms: Option<i64>,
    /// The steps of the script that are the fixture data, when more follow
    /// ([`FixtureOptions::arrivals`]): a start waits for these only.
    seed_steps: Option<usize>,
    /// The peer of the chat with calls (`--page=calls`).
    calls_peer: Option<kourier_model::UserId>,
}

/// The fake network with the fixture data set: an automatic login for seeded fixture mode
/// (with the fixture's media and the profile pictures to write once linked), otherwise the
/// device-name question and rotating QR codes, the last one "scanned".
#[cfg(feature = "fake")]
fn demo_network(f: &FixtureOptions) -> Result<Demo, AppError> {
    use kourier_fake::fixture::{FixtureSpec, FixtureState as FakeState, fixture};
    use kourier_fake::{FakeNetwork, Flavor, LoginScript};
    let spec = FixtureSpec {
        messages_per_conversation: f.messages_per_conversation,
        showcase: f.showcase,
        state: match f.state {
            FixtureState::Online => FakeState::Online,
            FixtureState::Offline => FakeState::Offline,
            FixtureState::Connecting => FakeState::Connecting,
            FixtureState::Challenge => FakeState::Challenge,
            FixtureState::ClockSkew => FakeState::ClockSkew,
            FixtureState::LoggedOut => FakeState::LoggedOut,
        },
        ..FixtureSpec::default()
    };
    let mut script = fixture(&spec).map_err(|e| AppError::Fixture(e.to_string()))?;
    let now_ms =
        crate::fixture_media::newest_message(&script).map(|t| t + FIXTURE_NOW_AFTER_NEWEST_MS);
    let mut avatars = crate::fixture_media::FixtureAvatars::default();
    if f.showcase {
        script = crate::fixture_media::with_showcase_extras(script);
        if let Some(timer) = f.expiring {
            script = crate::fixture_media::with_expiring(script, timer);
        }
    }
    let mut seed_steps = None;
    let mut calls_peer = None;
    if f.showcase
        && let Some(now) = now_ms
    {
        (script, calls_peer) = crate::fixture_media::with_calls(
            script,
            now,
            crate::fixture_media::CallsOptions {
                unread: f.calls_unread,
                ring_for: f.calls_ring_for,
            },
        );
    }
    if !f.link {
        (script, avatars) = crate::fixture_media::with_fixture_media(script);
        // What arrives later comes after everything a start waits for.
        if let (Some(arrivals), Some(now), true, FixtureState::Online) =
            (f.arrivals, now_ms, f.showcase, f.state)
        {
            script = script.step(kourier_fake::Step::WaitAcked);
            seed_steps = Some(script.steps.len());
            script = crate::fixture_media::with_arrivals(script, arrivals, now);
        }
    }
    if f.link {
        let codes = f.qr_codes.max(1);
        script.login = LoginScript::Qr {
            ask_device_name: true,
            codes,
            valid_for: f.qr_valid_for,
            scanned_at: Some(codes - 1),
            linked_as: None,
        };
    }
    // The history transfer: walked or held by the link, or left where a restart found it.
    {
        use kourier_fake::{Step, TransferScript};
        use kourier_model::{ConnState, HistoryTransfer};
        match (f.transfer, f.link) {
            (Some(FixtureTransfer::Walk(end)), true) => {
                let stage = f.transfer_stage;
                script = script.with_transfer(match end {
                    HistoryTransfer::Failed(kind) => TransferScript::failing(stage, kind),
                    HistoryTransfer::Skipped => TransferScript::ending(stage, end),
                    _ => TransferScript::walk(stage, HistoryTransfer::Done),
                });
            }
            (Some(FixtureTransfer::Hold(state)), true) => {
                script = script.with_transfer(TransferScript::holding(state));
            }
            (Some(FixtureTransfer::Left(state)), false) => {
                if !state.is_over() {
                    // (What a backend reports while its transfer holds receiving back.)
                    script = script.step(Step::Conn(ConnState::Syncing { fraction: None }));
                }
                script = script.step(Step::Transfer(state));
            }
            _ => {}
        }
    }
    let mut network = FakeNetwork::new(Flavor::SignalLike, script);
    if f.history_on_link || f.transfer.is_some() {
        network = network.with_history(kourier_model::HistorySource::PushedOnLink);
    }
    Ok(Demo {
        network,
        avatars,
        now_ms,
        seed_steps,
        calls_peer,
    })
}

/// Links the fixture account and waits until the fake delivered everything and the core acked
/// it (the first `seed_steps` steps of a script that goes on after its data); returns a fixed
/// "now" just after the newest message and the first group.
#[cfg(feature = "fake")]
async fn seed_fixture(
    core: &CoreHandle,
    fake: &kourier_fake::FakeNetwork,
    paths: &Paths,
    avatars: &crate::fixture_media::FixtureAvatars,
    now_ms: Option<i64>,
    seed_steps: Option<usize>,
    calls_peer: Option<kourier_model::UserId>,
) -> Result<FixtureInfo, AppError> {
    use kourier_core::facade::ConversationListQuery;
    use kourier_model::ConvKind;
    const SEED_TIMEOUT: Duration = Duration::from_secs(120);
    let control = fake.control();
    core.send(UiCommand::AddAccount {
        network: NetworkKind::Fake,
        flow: "qr".into(),
    })?;
    let started = std::time::Instant::now();
    let seeded = |c: &kourier_fake::FakeControl| {
        !c.finished().is_empty() || seed_steps.is_some_and(|steps| c.ran_steps(steps))
    };
    tokio::time::timeout(SEED_TIMEOUT, control.wait_for(seeded))
        .await
        .map_err(|_| AppError::Fixture("the fixture data did not arrive in time".into()))?;
    let page = match core
        .query(Query::Conversations(ConversationListQuery {
            limit: 500,
            ..ConversationListQuery::default()
        }))
        .await?
    {
        QueryData::Conversations(p) => p,
        _ => return Err(AppError::Fixture("unexpected query answer".into())),
    };
    // The profile pictures the fake's contacts point at, in the account's blob directory.
    if let QueryData::Accounts(accounts) = core.query(Query::Accounts).await?
        && let Some(account) = accounts.first()
    {
        avatars
            .write(&paths.blobs_dir(&account.uuid))
            .map_err(|e| AppError::Fixture(format!("fixture avatars: {e}")))?;
    }
    let newest = page.rows.iter().map(|r| r.last_activity).max().unwrap_or(0);
    let newest_sent = page
        .rows
        .iter()
        .filter_map(|r| r.preview.as_ref().map(|p| p.sent_at))
        .max()
        .unwrap_or(newest);
    tracing::info!(
        conversations = page.rows.len(),
        ms = started.elapsed().as_millis(),
        "fixture data ingested"
    );
    let mut media_conversation = None;
    for row in page.rows.iter().filter(|r| r.preview.is_some()) {
        let q = Query::Timeline(kourier_core::facade::TimelineQuery {
            conv: row.id,
            anchor: kourier_core::facade::TimelineAnchor::Newest,
            limit: 10,
        });
        if let QueryData::Timeline(t) = core.query(q).await?
            && t.rows.iter().any(|m| {
                matches!(&m.body, kourier_model::Body::Media { attachments, .. }
                    if attachments.iter().any(|a| a.kind == kourier_model::AttachmentKind::File))
            })
        {
            media_conversation = Some(row.id);
            break;
        }
    }
    // The showcase chats, by their peers.
    let direct = |peer: Option<kourier_model::UserId>| {
        let peer = peer?;
        page.rows
            .iter()
            .find(|r| r.kind == ConvKind::Direct && r.peer.as_ref() == Some(&peer))
            .map(|r| r.id)
    };
    let peer_of = |key: Result<kourier_model::ConvKey, kourier_fake::ScriptError>| {
        key.ok().and_then(|k| k.direct_peer().cloned())
    };
    Ok(FixtureInfo {
        // The same instant the core's fixture clock started at.
        now_ms: now_ms.unwrap_or(newest_sent + FIXTURE_NOW_AFTER_NEWEST_MS),
        demo_conversation: page
            .rows
            .iter()
            .find(|r| r.kind != ConvKind::Direct)
            .map(|r| r.id),
        media_conversation,
        quotes_conversation: direct(peer_of(kourier_fake::fixture::showcase_quotes())),
        timer_conversation: direct(peer_of(kourier_fake::fixture::showcase_timer())),
        kinds_conversation: direct(crate::fixture_media::kinds_peer()),
        albums_conversation: direct(crate::fixture_media::albums_peer()),
        readmore_conversation: direct(crate::fixture_media::readmore_peer()),
        calls_conversation: direct(calls_peer),
    })
}

#[cfg(test)]
mod transfer_switch_tests {
    use std::ffi::OsStr;

    use super::{HISTORY_TRANSFER_ENV, history_transfer_asked};

    /// The history transfer of a new link is off unless the run's environment says exactly
    /// `1`: no other value switches it on, and nothing switches it on by default.
    #[test]
    fn the_history_transfer_is_asked_for_only_by_the_value_one() {
        assert_eq!(HISTORY_TRANSFER_ENV, "KOURIER_HISTORY_TRANSFER");
        assert!(!history_transfer_asked(None));
        for other in ["", "0", "true", "yes", "on", "1 ", " 1", "11"] {
            assert!(
                !history_transfer_asked(Some(OsStr::new(other))),
                "{other:?}"
            );
        }
        assert!(history_transfer_asked(Some(OsStr::new("1"))));
    }
}
