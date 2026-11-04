//! The helper supervisor (TG-1b B-4; TG-1 plan §2.4 as the critic corrected it: B1, M1, M3, M4, L8).
//!
//! One helper per account, started when a login needs TDLib (never at the network's `spawn` or
//! the actor's start). The start sequence:
//!
//! 1. the account's SOCKS5 gate starts FIRST (loopback, an OS-chosen port, fresh credentials): the
//!    helper is locked down to its port (`--gate-port`) before it makes any client;
//! 2. the helper is spawned (`TgParts`' maker), its stderr DRAINED by a thread that counts lines
//!    and never keeps or logs one;
//! 3. the hello within [`Timings::hello`], checked against the pinned TDLib (protocol, version,
//!    commit); its lock-down word is logged once per actor by the caller;
//! 4. ONE client; then the `G` frame and `setTdlibParameters` written back to back, NO wait in
//!    between (B1: TDLib parks `addProxy` until the parameters, `Td.cpp:189-247, 350-356,
//!    1043-1069`, BSL-1.0, read for behaviour; the helper writes `addProxy` itself right before it
//!    forwards the parameters, M1);
//! 5. BOTH answers awaited: the parameters' `ok`, and `Inbound::GateProxy` naming THIS client, ok
//!    and the gate's port. A refused proxy (`ok: false`, another port, or the helper's
//!    `F gate-proxy-failed`, which leaves the helper running: re-check N2) means this client has no
//!    proxy: the start fails typed and the helper is stopped (the helper also sets TDLib's
//!    `store_all_files_in_files_directory` between the proxy and the parameters, K2; its
//!    `F files-option-failed` ends the start typed `Helper(FilesOptionFailed)`);
//! 6. the first authorization state after the parameters: ANY login state, `Ready` or
//!    `LoggingOut` (TDLib resumes a login from its binlog, `AuthManager.cpp:98-160, 304-335`,
//!    BSL-1.0, read for behaviour) is handed over in [`FirstState`] with the updates the start saw
//!    (TG-2 M3); `Closing`/`Closed` end the start, an unknown state is unexpected.
//!
//! The stop (M3, one closer: the helper): `Q` at t0, then the helper's bye and exit are awaited until
//! t0 + [`Timings::kill_after`] (17 s; the helper's own deadline is 15 s, its whole shutdown
//! 16.5 s), then SIGKILL and reap. A cancelled token kills and reaps at once. Every start that
//! fails after the spawn stops the helper the same way, so no child is left unreaped.
//!
//! Nothing here logs a frame's content, a credential, a port or a path: fixed texts, counts and
//! fixed classes only.

use std::fmt;
use std::fs::DirBuilder;
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kourier_model::AccountUuid;
use kourier_tdlib::protocol::{FailureCode, Hello};
use kourier_tdlib::spawn::dir_arg_allowed;
use serde_json::{Value, json};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::api::ApiCredentials;
use crate::error::LinkError;
use crate::gate::{self, GateHandle, GateOptions};
use crate::link::{HelperLink, Inbound, LinkOptions, check_pin};
use crate::parts::{HelperArgs, HelperChild, HelperProcess, LockdownNeed, TgEnv, TgParts};
#[cfg(test)]
use crate::pin::PIN;
use crate::pin::Pin;

/// The supervisor's times. [`Timings::default`] is the shipped set; tests scale them down.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timings {
    /// The link's own timeouts (a request's answer).
    pub(crate) link: LinkOptions,
    /// How long the helper may take to send its hello.
    pub(crate) hello: Duration,
    /// How long the rest of the start (client, gate, parameters, first state) may take.
    pub(crate) start: Duration,
    /// From `Q` to SIGKILL (M3: the helper's deadline is 15 s, its whole shutdown 16.5 s).
    pub(crate) kill_after: Duration,
    /// How often the stop looks whether the child ended.
    pub(crate) poll: Duration,
    /// The gate's limits.
    pub(crate) gate: GateOptions,
    /// The helper's heartbeat (its `ServeOptions::heartbeat`, 25 s): no frame for two of them is
    /// a wedged helper (M4).
    pub(crate) beat: Duration,
    /// How often the watchdog asks TDLib `testCallEmpty` while the helper lives (M4: 5 min).
    pub(crate) probe_every: Duration,
    /// How long a clean stop waits for TDLib's `Closed` while it reads what TDLib still hands
    /// out (Q-D3: [`crate::actor::CLOSE_DRAIN`], 3 s). A test whose success must not depend on a
    /// loaded machine's speed gives it a longer bound (TF8).
    pub(crate) close_drain: Duration,
    /// How many objects the chat machine learns before the link (TG-4 K6b): `None` is the
    /// product's bound (`receive::chats::LEARN_BOUND`, 4,096). A kill test lowers it so a chat
    /// told past the bound (asked with `getChat` at the link) is reachable with a handful of
    /// objects; no shipped path sets it (U9: the field exists only in a test build, the kill child
    /// IS the test binary).
    #[cfg(test)]
    pub(crate) learn_bound: Option<usize>,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            link: LinkOptions::default(),
            hello: Duration::from_secs(5),
            start: Duration::from_secs(30),
            kill_after: Duration::from_secs(17),
            poll: Duration::from_millis(50),
            gate: GateOptions::default(),
            beat: Duration::from_secs(25),
            probe_every: Duration::from_secs(300),
            close_drain: crate::actor::CLOSE_DRAIN,
            #[cfg(test)]
            learn_bound: None,
        }
    }
}

/// What a failed start means for the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// The helper died or wedged (EOF, a fatal frame, a signal, the start's bound): restart after
    /// the back-off; it counts towards the five aborts.
    Abort,
    /// A passing condition that is no abort (L8: TDLib's database still locked by an orphan
    /// helper): restart after the back-off, not counted.
    Transient,
    /// A property of this build or a refusal (no library, no lock-down, another TDLib, a refused
    /// proxy or parameters, a gate that cannot bind): restarting would loop. Answered at once.
    Never,
}

/// The restart back-off (TG-1 plan §2.4; WhatsApp's numbers, `kourier-whatsapp/src/actor.rs`
/// `REBUILD_MIN`/`REBUILD_MAX`/`STABLE_RUN`): 5 s doubling to 5 min, back to 5 s after a helper
/// that lived 120 s; five aborts within 10 minutes tell the person once per burst.
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
    /// When the recent aborts happened (the clock's ms), oldest first, within [`Backoff::WINDOW`].
    aborts: std::collections::VecDeque<u64>,
    /// The person was told of this burst.
    told: bool,
    /// Failures of the transient kind in a row (G12): a stuck lock is no passing condition.
    transients: usize,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: Self::MIN,
            aborts: std::collections::VecDeque::new(),
            told: false,
            transients: 0,
        }
    }
}

impl Backoff {
    pub(crate) const MIN: Duration = Duration::from_secs(5);
    pub(crate) const MAX: Duration = Duration::from_secs(300);
    pub(crate) const STABLE: Duration = Duration::from_secs(120);
    pub(crate) const WINDOW: Duration = Duration::from_secs(600);
    pub(crate) const BURST: usize = 5;
    /// Transient failures in a row that are free: the fifth, and each one after it, counts as an
    /// abort (G12), and the person is told once when the cap is reached (a lock that does not go
    /// away is a library that keeps failing; the back-off alone would stretch five aborts over
    /// more than the window).
    pub(crate) const TRANSIENT_CAP: usize = 5;

    /// The failures of the transient kind in a row so far.
    pub(crate) fn transients(&self) -> usize {
        self.transients
    }

    /// A start reached TDLib's phone-number step: the back-off starts over (G13). The recent aborts
    /// stay, for the burst notice.
    pub(crate) fn reached(&mut self) {
        self.next = Self::MIN;
        self.transients = 0;
    }

    /// A start that failed with `retry` (`Abort` or `Transient`) after the helper lived `life`, at
    /// `now_ms`: the wait before the next start, and whether this failure makes the person to be
    /// told: the burst of [`Backoff::BURST`] aborts (once, until the window holds fewer again), or
    /// the cap of transient failures in a row (once per run).
    pub(crate) fn failed(&mut self, retry: Retry, life: Duration, now_ms: u64) -> (Duration, bool) {
        if life >= Self::STABLE {
            self.next = Self::MIN;
        }
        let wait = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        let window = u64::try_from(Self::WINDOW.as_millis()).unwrap_or(u64::MAX);
        while self
            .aborts
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= window)
        {
            self.aborts.pop_front();
        }
        let counts = match retry {
            Retry::Abort => {
                self.transients = 0;
                true
            }
            Retry::Transient => {
                self.transients += 1;
                self.transients >= Self::TRANSIENT_CAP
            }
            Retry::Never => false,
        };
        if counts {
            self.aborts.push_back(now_ms);
        }
        let capped = retry == Retry::Transient && self.transients == Self::TRANSIENT_CAP;
        let burst = self.aborts.len() >= Self::BURST;
        let tell = (burst && !self.told) || capped;
        self.told = burst;
        (wait, tell)
    }
}

/// What the hello said of the helper's lock-down (fixed words only reach the log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lockdown {
    On,
    Unavailable,
    /// A helper that ran none (a test's serve loop) or said something else.
    Other,
}

impl Lockdown {
    fn from_hello(word: &str) -> Self {
        match word {
            "on" => Self::On,
            "unavailable" => Self::Unavailable,
            _ => Self::Other,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Unavailable => "unavailable",
            Self::Other => "other",
        }
    }
}

/// Why a start failed: fixed classes, never a frame's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum StartError {
    #[error("the account's gate did not start")]
    Gate,
    #[error("the TDLib helper did not start")]
    Spawn(io::ErrorKind),
    #[error("the TDLib helper's link failed")]
    Link(LinkError),
    #[error("the TDLib helper reported a failure")]
    Helper(FailureCode),
    #[error("TDLib did not take the account's gate as its proxy")]
    GateProxy,
    #[error("TDLib refused the parameters")]
    Parameters(LinkError),
    #[error("the TDLib helper reported no lock-down")]
    NoLockdown,
    #[error("TDLib went to an unexpected state during the start")]
    UnexpectedState,
    #[error("the TDLib helper ended during the start")]
    Ended,
    #[error("the TDLib start took too long")]
    Timeout,
    #[error("the start was cancelled")]
    Cancelled,
    /// Parts that dial were asked to start for Telegram's PRODUCTION servers (TR R0a fix G-3, review
    /// F3). The application never reaches it (its confirmation refuses production while D-4 Q1
    /// stands); it is the crate's own belt. It goes when D-4 Q1 is answered.
    #[error("this build does not dial Telegram's production servers")]
    DialsProduction,
    /// Parts that dial were asked to start with the test-only API pair (fix G-3, review F7): the
    /// application never has it of itself (tests only), but a kept run profile whose API file holds
    /// the dummy pair, with a confirmed order, reaches this refusal (that is why the belt exists); a
    /// dial with it would contact Telegram's test servers with a key no one registered. Its words
    /// reach the UI as the transient detail, like the other `Never` errors (`actor.rs`).
    #[error("this build does not dial Telegram with the test API key")]
    DialsDummyKey,
}

impl StartError {
    /// Whether (and how) a start that failed this way is tried again.
    pub(crate) fn retry(self) -> Retry {
        match self {
            Self::Ended | Self::Timeout | Self::Helper(FailureCode::TdlibFatal) => Retry::Abort,
            Self::Link(LinkError::Closed | LinkError::Frame | LinkError::Timeout) => Retry::Abort,
            Self::Link(LinkError::HelloTimeout) => Retry::Abort,
            Self::Parameters(LinkError::Closed | LinkError::Frame | LinkError::Timeout) => {
                Retry::Abort
            }
            Self::Parameters(LinkError::DatabaseLocked) => Retry::Transient,
            // A passing shortage of processes or memory (G15): tried again like an abort.
            Self::Spawn(io::ErrorKind::WouldBlock | io::ErrorKind::OutOfMemory) => Retry::Abort,
            Self::Gate
            | Self::Spawn(_)
            | Self::Link(_)
            | Self::Helper(_)
            | Self::GateProxy
            | Self::Parameters(_)
            | Self::NoLockdown
            | Self::UnexpectedState
            | Self::Cancelled
            | Self::DialsProduction
            | Self::DialsDummyKey => Retry::Never,
        }
    }

    /// The helper is not this build's: another TDLib or protocol than the pinned one, or no
    /// lock-down where the application requires it (a stale or foreign binary): a property of the
    /// installation, answered as unsupported (G15, G5).
    pub(crate) fn is_wrong_helper(self) -> bool {
        matches!(
            self,
            Self::Link(LinkError::VersionMismatch) | Self::NoLockdown
        )
    }

    /// The helper could not use its library: a property of this build, not a passing failure.
    pub(crate) fn is_missing_library(self) -> bool {
        matches!(
            self,
            Self::Helper(FailureCode::LibOpenFailed | FailureCode::LibSymbolMissing)
        )
    }
}

/// What one account's start needs besides the parts.
pub(crate) struct AccountStart<'a> {
    pub(crate) uuid: AccountUuid,
    /// TDLib's database key, standard base64 (`keys::tdlib_key_base64`).
    pub(crate) key: &'a Zeroizing<String>,
    /// `<account>/tdlib`.
    pub(crate) database_dir: PathBuf,
    /// `<files root>/<files_dir_name>`.
    pub(crate) files_dir: PathBuf,
    pub(crate) env: TgEnv,
    pub(crate) api: ApiCredentials,
    /// What Telegram's list of sessions shows for this one (`device_model`; his answer U-4:
    /// `Kourier (<host name>)`, made by the caller).
    pub(crate) device_model: &'a str,
}

/// The name of an account's files directory under the files root: the whole uuid (32 hex digits),
/// not [`AccountUuid::short`] (32 bits: two accounts of one profile could meet, and one start would
/// empty the other's live files directory; G15).
pub(crate) fn files_dir_name(uuid: AccountUuid) -> String {
    uuid.0.simple().to_string()
}

/// The two directories of an account's TDLib: the database beside `backend.db` and the files under
/// the application's files root, both 0700; the files directory is emptied first (TDLib writes its
/// downloads there in plaintext).
pub(crate) fn account_dirs(
    backend_db: &Path,
    files_root: &Path,
    uuid: AccountUuid,
) -> io::Result<(PathBuf, PathBuf)> {
    let database_dir = backend_db
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?
        .join("tdlib");
    let files_dir = files_root.join(files_dir_name(uuid));
    let mut dirs = DirBuilder::new();
    dirs.recursive(true).mode(0o700);
    dirs.create(&database_dir)?;
    std::fs::set_permissions(&database_dir, std::fs::Permissions::from_mode(0o700))?;
    empty_files_dir(&files_dir)?;
    dirs.create(&files_dir)?;
    std::fs::set_permissions(&files_dir, std::fs::Permissions::from_mode(0o700))?;
    Ok((database_dir, files_dir))
}

/// Removes an account's files directory (at the start, and after the helper stopped).
pub(crate) fn empty_files_dir(files_dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(files_dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `setTdlibParameters` (TG-1 plan §2.7) as borrowed fields: serialized straight into a zeroizing
/// text by [`crate::link::secret_json`], so the database key and the API hash never sit in a
/// `serde_json::Value` (TG-1b Q-C3-1, TG-2 M10).
#[derive(serde::Serialize)]
struct TdlibParameters<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    use_test_dc: bool,
    database_directory: std::borrow::Cow<'a, str>,
    files_directory: std::borrow::Cow<'a, str>,
    database_encryption_key: &'a str,
    use_file_database: bool,
    use_chat_info_database: bool,
    use_message_database: bool,
    use_secret_chats: bool,
    api_id: i32,
    api_hash: &'a str,
    system_language_code: &'static str,
    device_model: &'a str,
    system_version: &'static str,
    application_version: &'static str,
}

/// `setTdlibParameters` (TG-1 plan §2.7): message DB off, chat-info and file DB on, secret chats
/// off, the stored environment, the HKDF key, the dummy-or-compiled API pair, honest strings; as a
/// zeroizing JSON text for [`HelperLink::request_secret`].
pub(crate) fn parameters_secret(account: &AccountStart<'_>) -> Zeroizing<String> {
    crate::link::secret_json(&TdlibParameters {
        ty: "setTdlibParameters",
        use_test_dc: account.env == TgEnv::Test,
        database_directory: account.database_dir.to_string_lossy(),
        files_directory: account.files_dir.to_string_lossy(),
        database_encryption_key: account.key.as_str(),
        use_file_database: true,
        use_chat_info_database: true,
        use_message_database: false,
        use_secret_chats: false,
        api_id: account.api.id(),
        api_hash: account.api.hash(),
        system_language_code: "en",
        device_model: account.device_model,
        system_version: "Linux",
        application_version: env!("CARGO_PKG_VERSION"),
    })
}

/// The helper's stderr, drained by a thread that counts its lines and keeps none.
struct StderrCount {
    lines: Arc<AtomicU64>,
    done: std::sync::mpsc::Receiver<()>,
}

impl StderrCount {
    fn start(mut from: Box<dyn Read + Send>) -> Option<Self> {
        let lines = Arc::new(AtomicU64::new(0));
        let counted = lines.clone();
        let (tx, done) = std::sync::mpsc::channel();
        // The thread logs (if it ever did) to the subscriber of the one that started it, like the
        // link's threads: a test's capture sees it too.
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        std::thread::Builder::new()
            .name("tg-helper-err".to_owned())
            .spawn(move || {
                let _logs = tracing::dispatcher::set_default(&dispatch);
                let mut buf = [0u8; 4096];
                let mut partial = false;
                loop {
                    let n = match from.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    let chunk = &buf[..n];
                    let ends = chunk.iter().filter(|b| **b == b'\n').count() as u64;
                    counted.fetch_add(ends, Ordering::SeqCst);
                    partial = chunk.last() != Some(&b'\n');
                }
                if partial {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                buf.fill(0);
                let _ = tx.send(());
            })
            .ok()?;
        Some(Self { lines, done })
    }

    /// The count once the pipe ended (or after `within`, whatever it is then).
    fn finish(self, within: Duration) -> u64 {
        let _ = self.done.recv_timeout(within);
        self.lines.load(Ordering::SeqCst)
    }
}

/// How a helper ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StopReport {
    /// Its exit status, once reaped.
    pub(crate) status: Option<ExitStatus>,
    /// Its bye's `forced`, if a bye came.
    pub(crate) forced: Option<bool>,
    /// The supervisor had to SIGKILL it.
    pub(crate) killed: bool,
    /// Its stderr's line count (the lines themselves are never kept).
    pub(crate) stderr_lines: u64,
    /// The gate's final counts (taken after the helper was reaped and the gate stopped).
    pub(crate) gate: Option<crate::gate::GateCounts>,
}

/// The watchdog's `testCallEmpty` in flight.
type Probe<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, LinkError>> + Send + 'a>>;

/// Why the watchdog gave a live helper up (M4); the caller stops it and restarts after the back-off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wedged {
    /// No frame for two heartbeats: the helper's receiver (or TDLib under it) is stuck.
    Silent,
    /// TDLib did not answer `testCallEmpty` within the link's request timeout.
    Unanswered,
    /// The helper ended (EOF, its bye, a fatal frame).
    Ended,
    /// The token was cancelled.
    Cancelled,
}

/// How many updates of the start [`FirstState`] keeps (TG-2 M3); the rest are counted, not kept.
pub(crate) const FIRST_FRAMES: usize = 512;

/// What the start saw of TDLib's state, handed to the session (TG-2 M3): the first authorization
/// state after the parameters, and EVERY update the start read from the client's creation on (the
/// first state's own update among them, in order; `WaitTdlibParameters` states left out), so a
/// `my_id` option, a stored freeze or an early connection state is not lost. The session replays
/// `frames` before it reads the inbound queue. Each text is zeroized when dropped.
pub(crate) struct FirstState {
    /// The first authorization state after the parameters (a later one may follow in `frames`).
    pub(crate) state: AuthKind,
    /// The updates' JSON, in the order they came, at most [`FIRST_FRAMES`].
    pub(crate) frames: Vec<Zeroizing<String>>,
    /// How many updates beyond [`FIRST_FRAMES`] were dropped (the session then asks TDLib for its
    /// state again instead of trusting the replay).
    pub(crate) dropped: usize,
}

impl fmt::Debug for FirstState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The kind and counts only: the frames are TDLib's JSON.
        f.debug_struct("FirstState")
            .field("state", &self.state)
            .field("frames", &self.frames.len())
            .field("dropped", &self.dropped)
            .finish()
    }
}

/// A running helper with one client that reached its first authorization state.
pub(crate) struct Helper {
    link: HelperLink,
    inbound: mpsc::Receiver<Inbound>,
    child: Box<dyn HelperChild>,
    gate: Option<GateHandle>,
    stderr: Option<StderrCount>,
    /// What the start saw (taken once by the session).
    first: Option<FirstState>,
    /// The client the start made.
    pub(crate) client: i32,
    /// The hello's lock-down word.
    pub(crate) lockdown: Lockdown,
}

/// Starts the gate, the helper and its one client, up to its first authorization state. A failure
/// after the spawn stops the helper (killed and reaped) before it returns; its report is dropped
/// ([`start_reporting`] keeps it).
pub(crate) async fn start(
    parts: &TgParts,
    account: &AccountStart<'_>,
    net_rt: &Handle,
    t: &Timings,
    cancel: &CancellationToken,
) -> Result<Helper, StartError> {
    start_reporting(parts, account, net_rt, t, cancel).await.0
}

/// [`start`], and the stopped helper's report when the start failed after the spawn (LOW D2: the
/// real-library tests sum every gate's accepted connections, the failed start's too).
pub(crate) async fn start_reporting(
    parts: &TgParts,
    account: &AccountStart<'_>,
    net_rt: &Handle,
    t: &Timings,
    cancel: &CancellationToken,
) -> (Result<Helper, StartError>, Option<StopReport>) {
    // Parts that DIAL Telegram start for the test servers with a real API pair only (TR R0a fix
    // G-3, review F3 and F7): refused here, typed (class Never; the actor answers it through its
    // Never arm), BEFORE the gate starts and before the helper maker is called, whatever the
    // application decided. A test's parts never dial, so no test is touched.
    if parts.dials() {
        if account.env == TgEnv::Production {
            return (Err(StartError::DialsProduction), None);
        }
        if account.api.is_dummy() {
            return (Err(StartError::DialsDummyKey), None);
        }
    }
    // The helper takes its directories as UTF-8 arguments (since PROTOCOL 4); any other path is refused
    // here, typed, before anything starts (critic L-a), never as the helper's exit 2.
    if !(dir_arg_allowed(&account.database_dir) && dir_arg_allowed(&account.files_dir)) {
        return (Err(StartError::Spawn(io::ErrorKind::InvalidInput)), None);
    }
    let gate = match gate::start(net_rt, parts.dialer().clone(), t.gate, cancel.child_token()) {
        Ok(gate) => gate,
        Err(_) => return (Err(StartError::Gate), None),
    };
    let HelperProcess {
        sock,
        stderr,
        mut child,
    } = match parts.spawn_helper(&HelperArgs {
        account: account.uuid,
        gate_port: gate.port(),
        database_dir: account.database_dir.clone(),
        files_dir: account.files_dir.clone(),
    }) {
        Ok(process) => process,
        Err(e) => {
            // No helper: the gate stops with the error (its counts are the report's).
            let counts = gate.stop().await;
            return (
                Err(StartError::Spawn(e.kind())),
                Some(StopReport {
                    status: None,
                    forced: None,
                    killed: false,
                    stderr_lines: 0,
                    gate: Some(counts),
                }),
            );
        }
    };
    let stderr = stderr.and_then(StderrCount::start);
    let (link, inbound) = match HelperLink::start(sock, t.link) {
        Ok(started) => started,
        Err(_) => {
            let _ = child.kill();
            let status = child.wait().ok();
            let counts = gate.stop().await;
            return (
                Err(StartError::Link(LinkError::Closed)),
                Some(StopReport {
                    status,
                    forced: None,
                    killed: true,
                    stderr_lines: stderr.map_or(0, |s| s.finish(Duration::from_secs(1))),
                    gate: Some(counts),
                }),
            );
        }
    };
    let mut helper = Helper {
        link,
        inbound,
        child,
        gate: Some(gate),
        stderr,
        first: None,
        client: 0,
        lockdown: Lockdown::Other,
    };
    match helper
        .start_sequence(account, t, cancel, parts.lockdown_need(), parts.pin())
        .await
    {
        Ok(first) => {
            helper.first = Some(first);
            (Ok(helper), None)
        }
        Err(e) => {
            let report = helper.stop(t, cancel).await;
            tracing::debug!(
                killed = report.killed,
                stderr_lines = report.stderr_lines,
                "the TDLib helper was stopped after a failed start"
            );
            (Err(e), Some(report))
        }
    }
}

/// What the start saw on the inbound queue besides the answers.
#[derive(Default)]
struct Progress {
    /// The gate's `addedProxy` for the client: `(ok, port)`.
    gate: Option<(bool, Option<u16>)>,
    /// The parameters were answered `ok`.
    parameters: bool,
    /// The first authorization state after the parameters.
    state: Option<AuthKind>,
    /// Every update seen (TG-2 M3), at most [`FIRST_FRAMES`].
    frames: Vec<Zeroizing<String>>,
    /// Updates beyond [`FIRST_FRAMES`].
    dropped: usize,
    failure: Option<StartError>,
}

/// TDLib's authorization states by name (`td_api.tl:198-251`; fixed names, nothing else of an
/// update is kept): what the start tells apart and hands over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthKind {
    WaitParameters,
    WaitPhoneNumber,
    WaitCode,
    WaitPassword,
    WaitEmailAddress,
    WaitEmailCode,
    WaitOtherDevice,
    WaitRegistration,
    WaitPremiumPurchase,
    Ready,
    LoggingOut,
    Closing,
    Closed,
    /// A state this build does not know.
    Other,
}

/// The state of an `updateAuthorizationState`, `None` for any other line (a serde probe, never a
/// substring).
fn auth_state(json: &str) -> Option<AuthKind> {
    #[derive(serde::Deserialize)]
    struct Inner {
        #[serde(rename = "@type")]
        ty: String,
    }
    #[derive(serde::Deserialize)]
    struct Probe {
        #[serde(rename = "@type")]
        ty: String,
        authorization_state: Option<Inner>,
    }
    let probe: Probe = serde_json::from_str(json).ok()?;
    if probe.ty != "updateAuthorizationState" {
        return None;
    }
    Some(match probe.authorization_state?.ty.as_str() {
        "authorizationStateWaitTdlibParameters" => AuthKind::WaitParameters,
        "authorizationStateWaitPhoneNumber" => AuthKind::WaitPhoneNumber,
        "authorizationStateWaitCode" => AuthKind::WaitCode,
        "authorizationStateWaitPassword" => AuthKind::WaitPassword,
        "authorizationStateWaitEmailAddress" => AuthKind::WaitEmailAddress,
        "authorizationStateWaitEmailCode" => AuthKind::WaitEmailCode,
        "authorizationStateWaitOtherDeviceConfirmation" => AuthKind::WaitOtherDevice,
        "authorizationStateWaitRegistration" => AuthKind::WaitRegistration,
        "authorizationStateWaitPremiumPurchase" => AuthKind::WaitPremiumPurchase,
        "authorizationStateReady" => AuthKind::Ready,
        "authorizationStateLoggingOut" => AuthKind::LoggingOut,
        "authorizationStateClosing" => AuthKind::Closing,
        "authorizationStateClosed" => AuthKind::Closed,
        _ => AuthKind::Other,
    })
}

impl Progress {
    fn see(&mut self, client: i32, frame: Inbound) {
        if self.failure.is_some() {
            return;
        }
        match frame {
            Inbound::Update(json) => {
                // Moved into the zeroizing text, not copied.
                let json = Zeroizing::new(json);
                match auth_state(&json) {
                    Some(AuthKind::WaitParameters) => {}
                    Some(AuthKind::Closed) => self.failure = Some(StartError::Ended),
                    Some(state) => {
                        if self.state.is_none() {
                            self.state = Some(state);
                        }
                        self.keep(json);
                    }
                    None => self.keep(json),
                }
            }
            Inbound::GateProxy {
                client: of,
                ok,
                port,
            } => {
                if of == client && self.gate.is_none() {
                    self.gate = Some((ok, port));
                } else {
                    self.failure = Some(StartError::GateProxy);
                }
            }
            // The helper refused its own `addProxy` answer and closed the client (N2: the helper
            // goes on; the supervisor stops it).
            Inbound::Refused(FailureCode::GateProxyFailed) => {
                self.failure = Some(StartError::GateProxy);
            }
            // K2: TDLib did not take the helper's `store_all_files_in_files_directory`; the helper
            // closed the client (downloads would land in `tdlib/`, outside the rules' REFER). A
            // bug of this build or library: typed as the helper's code, class Never.
            Inbound::Refused(FailureCode::FilesOptionFailed) => {
                self.failure = Some(StartError::Helper(FailureCode::FilesOptionFailed));
            }
            Inbound::Refused(_) | Inbound::Heartbeat | Inbound::Hello(_) => {}
            Inbound::Fatal(code) => self.failure = Some(StartError::Helper(code)),
            Inbound::Bye(_) | Inbound::Closed => self.failure = Some(StartError::Ended),
            Inbound::Garbled => self.failure = Some(StartError::Link(LinkError::Frame)),
        }
    }

    fn keep(&mut self, json: Zeroizing<String>) {
        if self.frames.len() < FIRST_FRAMES {
            self.frames.push(json);
        } else {
            self.dropped += 1;
        }
    }

    /// Done when the gate's proxy is in, the parameters are answered and a first state came that a
    /// session can take (any login state, `Ready`, or `LoggingOut`: a logout TDLib goes on with);
    /// an error as soon as anything says the start cannot get there: `Closing` (TDLib is ending:
    /// the next start sees what is left) or a state this build does not know.
    fn outcome(&self, gate_port: u16) -> Option<Result<(), StartError>> {
        if let Some(e) = self.failure {
            return Some(Err(e));
        }
        if let Some((ok, port)) = self.gate
            && !(ok && port == Some(gate_port))
        {
            return Some(Err(StartError::GateProxy));
        }
        match self.state {
            Some(AuthKind::Other) => return Some(Err(StartError::UnexpectedState)),
            Some(AuthKind::Closing) => return Some(Err(StartError::Ended)),
            // Never stored (left out, or a failure), so never a first state.
            Some(AuthKind::WaitParameters | AuthKind::Closed) | None => {}
            Some(
                AuthKind::WaitPhoneNumber
                | AuthKind::WaitCode
                | AuthKind::WaitPassword
                | AuthKind::WaitEmailAddress
                | AuthKind::WaitEmailCode
                | AuthKind::WaitOtherDevice
                | AuthKind::WaitRegistration
                | AuthKind::WaitPremiumPurchase
                | AuthKind::Ready
                | AuthKind::LoggingOut,
            ) => {
                if self.gate.is_some() && self.parameters {
                    return Some(Ok(()));
                }
            }
        }
        None
    }

    /// The first state and the kept updates, once [`Self::outcome`] said `Ok`.
    fn into_first(self) -> Result<FirstState, StartError> {
        match self.state {
            Some(state) => Ok(FirstState {
                state,
                frames: self.frames,
                dropped: self.dropped,
            }),
            None => Err(StartError::UnexpectedState),
        }
    }
}

impl Helper {
    /// The gate's counts while the helper runs (the real-library tests read them).
    #[cfg(test)]
    pub(crate) fn gate_counts(&self) -> Option<crate::gate::GateCounts> {
        self.gate.as_ref().map(GateHandle::counts)
    }

    /// What the start saw, once (the session takes it before it reads [`Self::inbound`]).
    pub(crate) fn take_first(&mut self) -> Option<FirstState> {
        self.first.take()
    }

    /// The helper's link (the session clones it for a request in flight, TG-2 H1).
    pub(crate) fn link(&self) -> &HelperLink {
        &self.link
    }

    /// The helper's inbound frames (the session's own `select!` arm).
    pub(crate) fn inbound(&mut self) -> &mut mpsc::Receiver<Inbound> {
        &mut self.inbound
    }
}

impl Helper {
    async fn start_sequence(
        &mut self,
        account: &AccountStart<'_>,
        t: &Timings,
        cancel: &CancellationToken,
        need: LockdownNeed,
        pin: &Pin,
    ) -> Result<FirstState, StartError> {
        let hello = tokio::select! {
            () = cancel.cancelled() => return Err(StartError::Cancelled),
            hello = tokio::time::timeout(t.hello, wait_hello(&mut self.inbound)) => {
                hello.unwrap_or(Err(StartError::Link(LinkError::HelloTimeout)))?
            }
        };
        check_pin(&hello, pin).map_err(StartError::Link)?;
        self.lockdown = Lockdown::from_hello(&hello.landlock);
        drop(hello);
        // The real helper locks itself down BEFORE its hello, so `on` or `unavailable` is the
        // only word it can say; anything else is a helper that is not this build's (G5). Parts that
        // dial need `on` itself: a hello that says `unavailable` is refused too (TR R0a D-11).
        let refused = match need {
            LockdownNeed::Off => false,
            LockdownNeed::OnOrUnavailable => self.lockdown == Lockdown::Other,
            LockdownNeed::On => self.lockdown != Lockdown::On,
        };
        if refused {
            return Err(StartError::NoLockdown);
        }
        let Some(gate) = self.gate.as_ref() else {
            return Err(StartError::Gate);
        };
        let (port, user, pass) = (gate.port(), gate.user().to_owned(), gate.pass().to_owned());
        let sequence = run_sequence(
            &self.link,
            &mut self.inbound,
            account,
            (port, user, pass),
            &mut self.client,
        );
        tokio::select! {
            () = cancel.cancelled() => Err(StartError::Cancelled),
            done = tokio::time::timeout(t.start, sequence) => {
                done.unwrap_or(Err(StartError::Timeout))
            }
        }
    }

    /// Stops the helper (M3): `Q` and the end of the link at t0; its bye and exit awaited until
    /// t0 + `kill_after` (at once when `cancel` is cancelled), then SIGKILL; always reaped.
    pub(crate) async fn stop(self, t: &Timings, cancel: &CancellationToken) -> StopReport {
        let Self {
            link,
            mut inbound,
            mut child,
            gate,
            stderr,
            ..
        } = self;
        let deadline = Instant::now() + t.kill_after;
        link.quit();
        // The writer ends once its last sender is gone: the helper reads the end of its input too.
        drop(link);
        let mut forced = None;
        let mut status = None;
        let mut reading = true;
        loop {
            if let Ok(Some(s)) = child.try_wait() {
                status = Some(s);
                break;
            }
            if cancel.is_cancelled() || Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                frame = inbound.recv(), if reading => match frame {
                    Some(Inbound::Bye(bye)) => forced = Some(bye.forced),
                    Some(Inbound::Closed) | None => reading = false,
                    Some(_) => {}
                },
                () = tokio::time::sleep(t.poll) => {}
            }
        }
        let killed = status.is_none();
        if killed {
            let _ = child.kill();
            status = child.wait().ok();
        }
        // A bye that came with the exit: the link's reader thread can lag the exit (seen under a
        // load of 115), so read on until it reaches the end of fd 3 (`Closed`), at most a second
        // and never past the cancel; then whatever is already queued.
        let until = Instant::now() + Duration::from_secs(1);
        while reading {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(until) => break,
                frame = inbound.recv() => match frame {
                    Some(Inbound::Bye(bye)) => forced = Some(bye.forced),
                    Some(Inbound::Closed) | None => reading = false,
                    Some(_) => {}
                },
            }
        }
        while let Ok(frame) = inbound.try_recv() {
            if let Inbound::Bye(bye) = frame {
                forced = Some(bye.forced);
            }
        }
        let gate = match gate {
            Some(gate) => Some(gate.stop().await),
            None => None,
        };
        let stderr_lines = stderr.map_or(0, |s| s.finish(Duration::from_secs(1)));
        tracing::debug!(killed, forced, stderr_lines, "the TDLib helper stopped");
        StopReport {
            status,
            forced,
            killed,
            stderr_lines,
            gate,
        }
    }
}

impl Helper {
    /// The watchdog (M4) of a helper that lives: every frame (a heartbeat, an update) counts as
    /// life; no frame for 2 x [`Timings::beat`] is [`Wedged::Silent`]; every
    /// [`Timings::probe_every`] it asks TDLib `testCallEmpty` on the start's client, and an answer
    /// that does not come within the link's timeout is [`Wedged::Unanswered`] (an error answer is
    /// an answer). It returns only when the helper is given up; TG-2's connection loop calls it
    /// (TG-1b's helper lives only during a start, which has its own bound). Updates are dropped
    /// here unread.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn watch(&mut self, t: &Timings, cancel: &CancellationToken) -> Wedged {
        let Self {
            link,
            inbound,
            client,
            ..
        } = self;
        let client = *client;
        let mut last = Instant::now();
        let mut probe_at = last + t.probe_every;
        let mut probe: Option<Probe<'_>> = None;
        loop {
            let probing = probe.is_some();
            let answer = async {
                match probe.as_mut() {
                    Some(p) => p.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Wedged::Cancelled,
                frame = inbound.recv() => match frame {
                    None
                    | Some(Inbound::Closed | Inbound::Bye(_) | Inbound::Fatal(_)) => {
                        return Wedged::Ended;
                    }
                    Some(_) => last = Instant::now(),
                },
                answer = answer => {
                    match answer {
                        Err(LinkError::Timeout) => return Wedged::Unanswered,
                        Err(LinkError::Closed) => return Wedged::Ended,
                        _ => {}
                    }
                    probe = None;
                    probe_at = Instant::now() + t.probe_every;
                }
                () = tokio::time::sleep_until(last + t.beat * 2) => return Wedged::Silent,
                () = tokio::time::sleep_until(probe_at), if !probing => {
                    probe = Some(Box::pin(
                        link.request(client, json!({"@type": "testCallEmpty"})),
                    ));
                }
            }
        }
    }
}

async fn wait_hello(inbound: &mut mpsc::Receiver<Inbound>) -> Result<Hello, StartError> {
    loop {
        match inbound.recv().await {
            Some(Inbound::Heartbeat) => {}
            Some(Inbound::Hello(hello)) => return Ok(hello),
            Some(Inbound::Fatal(code)) => return Err(StartError::Helper(code)),
            Some(Inbound::Closed | Inbound::Bye(_)) | None => return Err(StartError::Ended),
            Some(_) => return Err(StartError::Link(LinkError::Frame)),
        }
    }
}

/// Client, gate, parameters (written back to back), then both answers and the first state.
async fn run_sequence(
    link: &HelperLink,
    inbound: &mut mpsc::Receiver<Inbound>,
    account: &AccountStart<'_>,
    (port, user, pass): (u16, String, String),
    client_out: &mut i32,
) -> Result<FirstState, StartError> {
    let mut progress = Progress::default();
    // No client exists yet, so nothing on the queue names one.
    let created = crate::link::drive(inbound, link.create_client(), |f| progress.see(0, f)).await;
    let client = match created {
        Ok(client) => client,
        Err(e) => return Err(typed_after_link_failure(e, inbound, StartError::Link)),
    };
    if let Some(e) = progress.failure {
        return Err(e);
    }
    *client_out = client;
    link.gate(client, port, user, pass)
        .map_err(StartError::Link)?;
    let answer = crate::link::drive(
        inbound,
        link.request_secret(client, parameters_secret(account)),
        |f| progress.see(client, f),
    )
    .await;
    if let Some(e) = progress.failure {
        return Err(e);
    }
    answer.map_err(|e| match e {
        // TG-2b: the helper refused parameters naming other directories than its lock-down's (a
        // bug of this build): typed as the helper's code, class Never.
        LinkError::Refused(FailureCode::RefusedParametersPaths) => {
            StartError::Helper(FailureCode::RefusedParametersPaths)
        }
        e => typed_after_link_failure(e, inbound, StartError::Parameters),
    })?;
    progress.parameters = true;
    loop {
        if let Some(done) = progress.outcome(port) {
            return match done {
                Ok(()) => progress.into_first(),
                Err(e) => Err(e),
            };
        }
        match inbound.recv().await {
            Some(frame) => progress.see(client, frame),
            None => return Err(StartError::Ended),
        }
    }
}

/// Types a start call that failed with `error` (K10, the security review's L4). `drive` prefers
/// the failed answer over the frames behind it, so when the helper died of TDLib's fatal path the
/// end of its stream ("closed", or a broken line before it) overtook the `F tdlib-fatal` that the
/// reader had queued first: the start was typed `Link(Closed)` or `Parameters(Closed)`. When the
/// call failed `Closed` or `Frame` and a `Fatal(TdlibFatal)` is queued, the start is
/// `Helper(TdlibFatal)`. Both classes are `Retry::Abort`: only the diagnosis changes.
fn typed_after_link_failure(
    error: LinkError,
    inbound: &mut mpsc::Receiver<Inbound>,
    otherwise: fn(LinkError) -> StartError,
) -> StartError {
    if matches!(error, LinkError::Closed | LinkError::Frame) {
        // Everything the reader queued before the end is already there: no waiting.
        let mut fatal = false;
        while let Ok(frame) = inbound.try_recv() {
            fatal |= matches!(frame, Inbound::Fatal(FailureCode::TdlibFatal));
        }
        if fatal {
            return StartError::Helper(FailureCode::TdlibFatal);
        }
    }
    otherwise(error)
}

#[cfg(test)]
mod tests;
