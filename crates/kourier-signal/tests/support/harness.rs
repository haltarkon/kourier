//! Rust driver for the Signal mock-server harness (`tests/mock-harness/harness.mjs`,
//! docs/architecture.md §10.3).
//!
//! [`Harness::start_or_skip`] finds Node ≥ 24, spawns `node harness.mjs`, reads the handshake
//! line ([`Handshake`]: everything a libsignal-net client needs to reach the mock) and then
//! speaks the JSON-lines protocol documented in `tests/mock-harness/README.md`. Requests carry
//! an id and responses may arrive in any order, so several requests can be in flight at once
//! (for example a `completeProvision` while the backend under test links). Events the harness
//! reports on its own are queued separately ([`Harness::next_event`]).
//!
//! A new harness command is one entry in `harness.mjs`, a line in the README and, when tests use
//! it often, a typed wrapper here; anything else goes through [`Harness::request`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

/// Version of the JSON-lines protocol this driver speaks (`PROTOCOL_VERSION` in harness.mjs).
pub const PROTOCOL_VERSION: u32 = 1;

/// Overrides the Node binary (must be ≥ 24).
pub const NODE_ENV: &str = "KOURIER_NODE";

/// When set (to anything), a missing Node or `node_modules` fails the test instead of skipping.
pub const REQUIRE_ENV: &str = "KOURIER_REQUIRE_MOCK_HARNESS";

/// When set, the harness's stderr is forwarded to ours as it arrives.
pub const VERBOSE_ENV: &str = "KOURIER_HARNESS_VERBOSE";

/// Node as installed by nvm, relative to `$HOME`; the nvm default alias stays at 22.
const NVM_NODE: &str = ".nvm/versions/node/v24.21.0/bin/node";

/// The oldest Node major version the harness runs on (`engines` in package.json).
const MIN_NODE_MAJOR: u32 = 24;

/// How many stderr lines of the harness are kept for error messages.
const STDERR_TAIL: usize = 40;

/// Slack on top of a command's own `timeout` argument before the driver gives up on it.
const REQUEST_SLACK: Duration = Duration::from_secs(10);

/// Errors of the harness driver.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Node ≥ 24 or the npm dependencies are missing and [`REQUIRE_ENV`] is set.
    #[error("mock harness unavailable: {0}")]
    Unavailable(String),
    /// `$KOURIER_NODE` names something that is not a usable Node ≥ 24.
    #[error("{NODE_ENV}: {0}")]
    BadNode(String),
    /// Spawning or talking to the harness process failed.
    #[error("mock harness I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The harness did not start; the message carries the tail of its stderr.
    #[error("mock harness did not start: {0}")]
    Startup(String),
    /// A line from the harness, or a result, did not have the expected shape.
    #[error("mock harness protocol: {0}")]
    Protocol(String),
    /// The harness answered a request with an error.
    #[error("mock harness {cmd}: {code}: {message}")]
    Remote {
        /// The command that failed.
        cmd: String,
        /// Stable error code: `badRequest`, `notFound`, `timeout`, `unknownCommand`, `failed`.
        code: String,
        /// Human-readable detail.
        message: String,
    },
    /// No response arrived in time (the driver's own guard, not the harness's `timeout` code).
    #[error("mock harness {cmd}: no response after {after:?}")]
    Timeout {
        /// The command that timed out.
        cmd: String,
        /// How long the driver waited.
        after: Duration,
    },
    /// The harness process is gone; the message says why and carries the tail of its stderr.
    #[error("mock harness closed: {0}")]
    Closed(String),
    /// A base64 field of the handshake or a result did not decode.
    #[error("mock harness field {field}: {source}")]
    Base64 {
        /// Name of the field.
        field: &'static str,
        /// Decoder error.
        source: base64::DecodeError,
    },
}

impl HarnessError {
    /// The harness's error code if this is a [`HarnessError::Remote`].
    pub fn remote_code(&self) -> Option<&str> {
        match self {
            Self::Remote { code, .. } => Some(code),
            _ => None,
        }
    }
}

// -------------------------------------------------------------------------------------------
// Locating Node and the harness
// -------------------------------------------------------------------------------------------

/// Where the harness lives: `tests/mock-harness/` at the workspace root (two levels above
/// this crate).
pub fn harness_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(2)
        .unwrap_or(manifest_dir)
        .join("tests/mock-harness")
}

/// Parses the major version out of `node --version` output (`v24.21.0`).
pub fn parse_node_major(version: &str) -> Option<u32> {
    version
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// A Node binary and the harness directory, ready to spawn.
#[derive(Debug, Clone)]
pub struct Setup {
    /// The Node binary (≥ 24).
    pub node: PathBuf,
    /// `tests/mock-harness/`.
    pub dir: PathBuf,
}

/// Why the harness cannot run here (Node or `node_modules` missing).
#[derive(Debug, Clone)]
pub struct Unavailable(pub String);

/// The major version of `node`, or why it could not be determined.
async fn node_major(node: &Path) -> Result<u32, String> {
    let output = Command::new(node)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("{}: {error}", node.display()))?;
    let version = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(format!("{} --version: {}", node.display(), output.status));
    }
    parse_node_major(&version)
        .ok_or_else(|| format!("{} --version printed {:?}", node.display(), version.trim()))
}

/// Finds Node ≥ 24 (`$KOURIER_NODE`, then the nvm install, then `node` on `PATH`) and checks
/// that `npm ci` has been run in the harness directory.
pub async fn locate() -> Result<Result<Setup, Unavailable>, HarnessError> {
    let dir = harness_dir();
    if !dir.join("harness.mjs").is_file() {
        return Err(HarnessError::Startup(format!(
            "{} has no harness.mjs",
            dir.display()
        )));
    }

    let node = if let Some(explicit) = std::env::var_os(NODE_ENV) {
        let node = PathBuf::from(explicit);
        match node_major(&node).await {
            Ok(major) if major >= MIN_NODE_MAJOR => node,
            Ok(major) => {
                return Err(HarnessError::BadNode(format!(
                    "{} is Node {major}, the harness needs ≥ {MIN_NODE_MAJOR}",
                    node.display()
                )));
            }
            Err(reason) => return Err(HarnessError::BadNode(reason)),
        }
    } else {
        let mut tried = Vec::new();
        let mut candidates = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(Path::new(&home).join(NVM_NODE));
        }
        candidates.push(PathBuf::from("node"));
        let mut found = None;
        for candidate in candidates {
            match node_major(&candidate).await {
                Ok(major) if major >= MIN_NODE_MAJOR => {
                    found = Some(candidate);
                    break;
                }
                Ok(major) => tried.push(format!("{} is Node {major}", candidate.display())),
                Err(reason) => tried.push(reason),
            }
        }
        match found {
            Some(node) => node,
            None => {
                return Ok(Err(Unavailable(format!(
                    "no Node ≥ {MIN_NODE_MAJOR} found (set {NODE_ENV}); tried: {}",
                    tried.join("; ")
                ))));
            }
        }
    };

    let mock_server = dir.join("node_modules/@signalapp/mock-server/package.json");
    if !mock_server.is_file() {
        return Ok(Err(Unavailable(format!(
            "{} is missing; run `npm ci` in {}",
            mock_server.display(),
            dir.display()
        ))));
    }
    Ok(Ok(Setup { node, dir }))
}

/// [`locate`], or prints why the harness cannot run here and returns `None` (an error
/// instead when [`REQUIRE_ENV`] is set).
pub async fn locate_or_skip() -> Result<Option<Setup>, HarnessError> {
    match locate().await? {
        Ok(setup) => Ok(Some(setup)),
        Err(Unavailable(reason)) if std::env::var_os(REQUIRE_ENV).is_some() => {
            Err(HarnessError::Unavailable(reason))
        }
        Err(Unavailable(reason)) => {
            // Straight to stderr: libtest captures `eprintln!`, and a skip must be visible.
            let _ = writeln!(
                std::io::stderr(),
                "skipping: mock harness unavailable: {reason}"
            );
            Ok(None)
        }
    }
}

// -------------------------------------------------------------------------------------------
// Handshake
// -------------------------------------------------------------------------------------------

/// The first line the harness prints: how to reach the mock server.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    /// JSON-lines protocol version ([`PROTOCOL_VERSION`]).
    pub protocol: u32,
    /// Process id of the harness.
    pub pid: u32,
    /// TLS server name to connect with (`localhost`, a SAN of the mock certificate).
    pub host: String,
    /// The address the server listens on (`127.0.0.1`).
    pub ip: String,
    /// TCP port of every service (chat, storage, CDNs, provisioning).
    pub port: u16,
    /// HTTP version the client must use; websockets are HTTP/2 extended CONNECT only.
    pub http_version: u8,
    /// The CA that signed the server certificate, PEM.
    pub ca_pem: String,
    /// The same CA, DER in standard base64 ([`Handshake::ca_der`]).
    pub ca_der: String,
    /// Sender-certificate trust root, a 33-byte public key in base64.
    pub trust_root: String,
    /// zkgroup `ServerPublicParams`, base64.
    pub zk_public_params: String,
    /// zkgroup generic server public params, base64.
    pub generic_server_public_params: String,
    /// Backup server public params, base64.
    pub backup_server_public_params: String,
    /// Base URLs of the services.
    pub urls: Urls,
    /// Websocket paths.
    pub paths: Paths,
    /// The remote config `/v2/config` serves at start (Desktop's mock config).
    pub remote_config: BTreeMap<String, RemoteConfigValue>,
    /// State directory of the server (CDN3 files). The default one is removed when the
    /// [`Harness`] is dropped or shut down.
    pub state_dir: PathBuf,
    /// Versions of the pieces involved.
    pub versions: Versions,
}

impl Handshake {
    /// The CA certificate as DER, for libsignal-net's `RootCertificates::FromDer`.
    pub fn ca_der(&self) -> Result<Vec<u8>, HarnessError> {
        decode_base64("caDer", &self.ca_der)
    }

    /// The sender-certificate trust root (33 bytes, DJB type byte first).
    pub fn trust_root(&self) -> Result<Vec<u8>, HarnessError> {
        decode_base64("trustRoot", &self.trust_root)
    }

    /// zkgroup `ServerPublicParams`.
    pub fn zk_public_params(&self) -> Result<Vec<u8>, HarnessError> {
        decode_base64("zkPublicParams", &self.zk_public_params)
    }

    /// zkgroup generic server public params.
    pub fn generic_server_public_params(&self) -> Result<Vec<u8>, HarnessError> {
        decode_base64(
            "genericServerPublicParams",
            &self.generic_server_public_params,
        )
    }

    /// Backup server public params.
    pub fn backup_server_public_params(&self) -> Result<Vec<u8>, HarnessError> {
        decode_base64(
            "backupServerPublicParams",
            &self.backup_server_public_params,
        )
    }
}

/// Base URLs of the mock's services (all on [`Handshake::port`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Urls {
    /// Chat service (REST and websockets).
    pub chat: String,
    /// Storage Service (also serves groups).
    pub storage: String,
    /// CDNs by number (`"0"`, `"2"`, `"3"`).
    pub cdn: BTreeMap<String, String>,
    /// Update manifests.
    pub resources: String,
    /// Calling SFU.
    pub sfu: String,
}

/// Websocket paths of the chat service.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paths {
    /// Authenticated and unauthenticated chat socket.
    pub chat_websocket: String,
    /// Provisioning socket (device linking).
    pub provisioning_websocket: String,
}

/// One remote-config entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RemoteConfigValue {
    /// Whether the flag is on.
    pub enabled: bool,
    /// Optional value (`"ws"` keeps a libsignal-net request on the websocket).
    #[serde(default)]
    pub value: Option<String>,
}

/// Versions reported by the harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Versions {
    /// `@signalapp/mock-server`.
    pub mock_server: Option<String>,
    /// The `@signalapp/libsignal-client` the mock server uses.
    pub libsignal_client: Option<String>,
    /// Node (`process.version`).
    pub node: String,
}

fn decode_base64(field: &'static str, value: &str) -> Result<Vec<u8>, HarnessError> {
    BASE64
        .decode(value)
        .map_err(|source| HarnessError::Base64 { field, source })
}

// -------------------------------------------------------------------------------------------
// Wire format
// -------------------------------------------------------------------------------------------

/// An error object on the wire.
#[derive(Debug, Clone, Deserialize)]
struct WireError {
    code: String,
    message: String,
}

/// One line from the harness's stdout.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Line {
    Ready(Box<Handshake>),
    Response {
        id: Option<u64>,
        ok: bool,
        #[serde(default)]
        result: Value,
        error: Option<WireError>,
    },
    Event {
        event: String,
        #[serde(default)]
        data: Value,
    },
    Fatal {
        error: WireError,
    },
}

/// Something the harness reported on its own (`provisionOpened`, `deliveryFailed`,
/// `unhandledRejection`, ...).
#[derive(Debug, Clone)]
pub struct HarnessEvent {
    /// Event name.
    pub event: String,
    /// Event payload.
    pub data: Value,
}

type Reply = Result<Value, WireError>;

/// Requests waiting for a response; `closed` is set once no response can arrive any more.
#[derive(Default)]
struct Pending {
    waiting: HashMap<u64, oneshot::Sender<Reply>>,
    closed: Option<String>,
}

/// State shared with the reader tasks.
#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
    stderr_tail: Mutex<VecDeque<String>>,
}

/// Locks a mutex; a panic in another holder does not make the state unusable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn stderr_tail(&self) -> String {
        let tail = lock(&self.stderr_tail);
        if tail.is_empty() {
            String::from("(no stderr)")
        } else {
            tail.iter().cloned().collect::<Vec<_>>().join("\n")
        }
    }

    /// Fails every waiting request and every later one with `reason`.
    fn close(&self, reason: String) {
        let mut pending = lock(&self.pending);
        if pending.closed.is_none() {
            pending.closed = Some(reason);
        }
        // Dropping the senders wakes the waiters, which then read `closed`.
        pending.waiting.clear();
    }

    fn closed_reason(&self) -> String {
        let reason = lock(&self.pending)
            .closed
            .clone()
            .unwrap_or_else(|| String::from("harness went away"));
        format!("{reason}\n{}", self.stderr_tail())
    }
}

// -------------------------------------------------------------------------------------------
// The driver
// -------------------------------------------------------------------------------------------

/// Options for [`Harness::start_with`].
#[derive(Debug, Clone)]
pub struct HarnessOptions {
    /// How long to wait for the handshake line.
    pub startup_timeout: Duration,
    /// Guard for requests without a `timeout` argument.
    pub request_timeout: Duration,
    /// The mock server's own API timeout (`--api-timeout`, its default is 60 s).
    pub api_timeout: Option<Duration>,
    /// Keep the server's state (CDN3 files) here; by default it goes to a temporary directory
    /// removed when the [`Harness`] is dropped, even if the process had to be killed.
    pub state_dir: Option<PathBuf>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(60),
            // Longer than the harness's longest default wait (completeProvision, 60 s).
            request_timeout: Duration::from_secs(90),
            api_timeout: None,
            state_dir: None,
        }
    }
}

/// A running `harness.mjs`. Dropping it kills the process; [`Harness::shutdown`] stops it
/// cleanly.
pub struct Harness {
    handshake: Handshake,
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<HarnessEvent>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    request_timeout: Duration,
    /// The default state directory. Last field: removed after the child is killed.
    _state_dir: Option<tempfile::TempDir>,
}

impl Harness {
    /// Starts the harness with default options, or prints why it cannot run here and returns
    /// `None` (an error instead when [`REQUIRE_ENV`] is set).
    pub async fn start_or_skip() -> Result<Option<Self>, HarnessError> {
        Self::start_or_skip_with(HarnessOptions::default()).await
    }

    /// [`Harness::start_or_skip`] with options.
    pub async fn start_or_skip_with(options: HarnessOptions) -> Result<Option<Self>, HarnessError> {
        match locate_or_skip().await? {
            Some(setup) => Self::start_with(&setup, options).await.map(Some),
            None => Ok(None),
        }
    }

    /// Spawns `node harness.mjs` from `setup` and waits for its handshake.
    pub async fn start_with(setup: &Setup, options: HarnessOptions) -> Result<Self, HarnessError> {
        let mut command = Command::new(&setup.node);
        command
            .arg(setup.dir.join("harness.mjs"))
            .current_dir(&setup.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(api_timeout) = options.api_timeout {
            command.arg(format!("--api-timeout={}", millis(api_timeout)));
        }
        let temp_state_dir = match &options.state_dir {
            Some(state_dir) => {
                command.arg("--state-dir").arg(state_dir);
                None
            }
            None => {
                let state_dir = tempfile::Builder::new().prefix("kourier-mock-").tempdir()?;
                command.arg("--state-dir").arg(state_dir.path());
                Some(state_dir)
            }
        };
        let mut child = command.spawn()?;

        let missing = |what: &str| HarnessError::Startup(format!("no {what} pipe"));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;

        let shared = Arc::new(Shared::default());
        tokio::spawn(read_stderr(stderr, Arc::clone(&shared)));

        let mut lines = BufReader::new(stdout).lines();
        let handshake =
            match tokio::time::timeout(options.startup_timeout, read_handshake(&mut lines)).await {
                Ok(Ok(handshake)) => handshake,
                Ok(Err(reason)) => {
                    // Let the stderr reader catch up with a dying process before reporting.
                    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                    return Err(HarnessError::Startup(format!(
                        "{reason}\n{}",
                        shared.stderr_tail()
                    )));
                }
                Err(_) => {
                    return Err(HarnessError::Startup(format!(
                        "no handshake within {:?}\n{}",
                        options.startup_timeout,
                        shared.stderr_tail()
                    )));
                }
            };
        if handshake.protocol != PROTOCOL_VERSION {
            return Err(HarnessError::Startup(format!(
                "harness speaks protocol {}, this driver {PROTOCOL_VERSION}",
                handshake.protocol
            )));
        }

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_stdout(lines, Arc::clone(&shared), events_tx));

        Ok(Self {
            handshake,
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            events: tokio::sync::Mutex::new(events_rx),
            shared,
            next_id: AtomicU64::new(1),
            request_timeout: options.request_timeout,
            _state_dir: temp_state_dir,
        })
    }

    /// How to reach the mock server.
    pub fn handshake(&self) -> &Handshake {
        &self.handshake
    }

    /// The last lines the harness wrote to stderr (for assertion messages).
    pub fn stderr_tail(&self) -> String {
        self.shared.stderr_tail()
    }

    /// Sends `cmd` with `args` and returns the raw `result`. Safe to call concurrently.
    pub async fn request_value(&self, cmd: &str, args: Value) -> Result<Value, HarnessError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let guard = match args.get("timeout").and_then(Value::as_u64) {
            Some(ms) => Duration::from_millis(ms) + REQUEST_SLACK,
            None => self.request_timeout,
        };
        let line = serde_json::to_string(&json!({ "id": id, "cmd": cmd, "args": args }))
            .map_err(|error| HarnessError::Protocol(format!("{cmd}: {error}")))?;

        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut pending = lock(&self.shared.pending);
            if pending.closed.is_some() {
                drop(pending);
                return Err(HarnessError::Closed(self.shared.closed_reason()));
            }
            pending.waiting.insert(id, reply_tx);
        }

        if let Err(error) = self.write_line(&line).await {
            lock(&self.shared.pending).waiting.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(guard, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(HarnessError::Remote {
                cmd: cmd.to_owned(),
                code: error.code,
                message: error.message,
            }),
            Ok(Err(_)) => Err(HarnessError::Closed(self.shared.closed_reason())),
            Err(_) => {
                lock(&self.shared.pending).waiting.remove(&id);
                Err(HarnessError::Timeout {
                    cmd: cmd.to_owned(),
                    after: guard,
                })
            }
        }
    }

    /// Sends `cmd` with `args` and deserializes the `result`.
    pub async fn request<T: DeserializeOwned>(
        &self,
        cmd: &str,
        args: Value,
    ) -> Result<T, HarnessError> {
        let result = self.request_value(cmd, args).await?;
        serde_json::from_value(result)
            .map_err(|error| HarnessError::Protocol(format!("{cmd} result: {error}")))
    }

    async fn write_line(&self, line: &str) -> Result<(), HarnessError> {
        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(HarnessError::Closed(String::from("stdin already closed")));
        };
        let written = async {
            pipe.write_all(line.as_bytes()).await?;
            pipe.write_all(b"\n").await?;
            pipe.flush().await
        };
        written.await.map_err(|error| {
            HarnessError::Closed(format!(
                "writing to the harness: {error}\n{}",
                self.stderr_tail()
            ))
        })
    }

    /// The next event the harness reported, waiting up to `timeout`.
    pub async fn next_event(&self, timeout: Duration) -> Result<HarnessEvent, HarnessError> {
        let mut events = self.events.lock().await;
        match tokio::time::timeout(timeout, events.recv()).await {
            Ok(Some(event)) => Ok(event),
            Ok(None) => Err(HarnessError::Closed(self.shared.closed_reason())),
            Err(_) => Err(HarnessError::Timeout {
                cmd: String::from("next_event"),
                after: timeout,
            }),
        }
    }

    /// Every event reported so far that nobody has taken yet.
    pub async fn drain_events(&self) -> Vec<HarnessEvent> {
        let mut events = self.events.lock().await;
        let mut drained = Vec::new();
        while let Ok(event) = events.try_recv() {
            drained.push(event);
        }
        drained
    }

    /// Asks the harness to stop and waits for it to exit (killing it after 10 s).
    pub async fn shutdown(self) -> Result<ExitStatus, HarnessError> {
        match self.request_value("shutdown", json!({})).await {
            Ok(_) | Err(HarnessError::Closed(_)) => {}
            Err(error) => return Err(error),
        }
        drop(self.stdin.lock().await.take());
        let mut child = self.child.lock().await;
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(status) => Ok(status?),
            Err(_) => {
                child.kill().await?;
                Ok(child.wait().await?)
            }
        }
    }

    // -- typed commands ----------------------------------------------------------------------

    /// Liveness check; returns the harness's protocol version.
    pub async fn ping(&self) -> Result<u32, HarnessError> {
        #[derive(Deserialize)]
        struct Pong {
            protocol: u32,
        }
        Ok(self.request::<Pong>("ping", json!({})).await?.protocol)
    }

    /// Registers a scripted account with a primary device ("phone").
    pub async fn create_primary(&self, spec: &CreatePrimary) -> Result<Primary, HarnessError> {
        self.request("createPrimary", to_args(spec)?).await
    }

    /// Waits until a client opens a provisioning socket; returns its address (the `uuid` of
    /// the client's `sgnl://linkdevice` URL).
    pub async fn wait_for_provision(&self, timeout: Duration) -> Result<String, HarnessError> {
        #[derive(Deserialize)]
        struct Opened {
            address: String,
        }
        let args = json!({ "timeout": millis(timeout) });
        Ok(self
            .request::<Opened>("waitForProvision", args)
            .await?
            .address)
    }

    /// Links the client that shows `url` to `primary`'s account and waits until it has
    /// uploaded its one-time prekeys. Returns the new device.
    pub async fn complete_provision(
        &self,
        url: &str,
        primary: &str,
        timeout: Duration,
    ) -> Result<DeviceInfo, HarnessError> {
        let args = json!({ "url": url, "primary": primary, "timeout": millis(timeout) });
        self.request("completeProvision", args).await
    }

    /// Sends a text message from a primary to one device.
    pub async fn send_text(&self, send: &SendText) -> Result<Sent, HarnessError> {
        self.request("sendText", to_args(send)?).await
    }

    /// Sends a message with attachments (encrypted like Desktop, stored on the mock's CDN) from
    /// a primary to one device.
    pub async fn send_attachments(
        &self,
        send: &SendAttachments,
    ) -> Result<SentAttachments, HarnessError> {
        self.request("sendAttachments", to_args(send)?).await
    }

    /// Downloads and decrypts (and verifies) an attachment a client uploaded, as a phone would.
    pub async fn fetch_attachment(
        &self,
        pointer: &Value,
    ) -> Result<FetchedAttachment, HarnessError> {
        self.request("fetchAttachment", json!({ "pointer": pointer }))
            .await
    }

    /// Queues many texts from a primary to one device (catch-up and throughput loads).
    pub async fn send_texts(&self, send: &SendTexts) -> Result<SentTexts, HarnessError> {
        self.request("sendTexts", to_args(send)?).await
    }

    /// Waits for the next text a primary received, optionally only one `from` a device ref.
    pub async fn wait_for_message(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedMessage, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForMessage", args).await
    }

    /// Waits until `primary` answered the CONTACTS, BLOCKED and CONFIGURATION sync requests of
    /// `device`, then resets that sync state.
    pub async fn wait_for_sync(
        &self,
        primary: &str,
        device: &str,
        timeout: Duration,
    ) -> Result<(), HarnessError> {
        let args = json!({ "primary": primary, "device": device, "timeout": millis(timeout) });
        self.request_value("waitForSync", args).await.map(drop)
    }

    /// Waits until `device` has acknowledged every envelope addressed to it (nothing in flight,
    /// nothing queued for its next connection).
    pub async fn wait_for_deliveries(
        &self,
        device: &str,
        timeout: Duration,
    ) -> Result<(), HarnessError> {
        let args = json!({ "device": device, "timeout": millis(timeout) });
        self.request_value("waitForDeliveries", args)
            .await
            .map(drop)
    }

    /// Every device the server registered for `primary`'s account, the primary first.
    pub async fn list_devices(&self, primary: &str) -> Result<Vec<DeviceInfo>, HarnessError> {
        #[derive(Deserialize)]
        struct Devices {
            devices: Vec<DeviceInfo>,
        }
        let args = json!({ "primary": primary });
        Ok(self.request::<Devices>("listDevices", args).await?.devices)
    }

    /// Unlinks `device` from `primary`'s account, as the phone does: its credentials stop
    /// working (403 on the next websocket upgrade) and its open socket is closed with 4401.
    pub async fn unlink(&self, primary: &str, device: &str) -> Result<(), HarnessError> {
        let args = json!({ "primary": primary, "device": device });
        self.request_value("unlink", args).await.map(drop)
    }

    /// Opens a second websocket with `device`'s own credentials and closes it again: the server
    /// closes the device's first socket with 4409. Returns whether a socket was taken over.
    pub async fn second_socket(&self, device: &str) -> Result<bool, HarnessError> {
        let value = self
            .request_value("secondSocket", json!({ "device": device }))
            .await?;
        Ok(value["tookOver"].as_bool().unwrap_or(false))
    }

    /// Closes `device`'s websocket from the server's side with close code `code`; whether it
    /// had one.
    pub async fn close_socket(&self, device: &str, code: u16) -> Result<bool, HarnessError> {
        let value = self
            .request_value("closeSocket", json!({ "device": device, "code": code }))
            .await?;
        Ok(value["closed"].as_bool().unwrap_or(false))
    }

    /// Refuses every websocket upgrade with `device`'s credentials with HTTP `status`, though
    /// the device is still linked (until [`Self::stop_fail_auth`]).
    pub async fn fail_auth(&self, device: &str, status: u16) -> Result<(), HarnessError> {
        self.request_value("failAuth", json!({ "device": device, "status": status }))
            .await
            .map(drop)
    }

    /// Ends a [`Self::fail_auth`].
    pub async fn stop_fail_auth(&self, device: &str) -> Result<(), HarnessError> {
        self.request_value("failAuth", json!({ "device": device, "stop": true }))
            .await
            .map(drop)
    }

    /// Refuses the next `count` websocket upgrades under the name `username`
    /// (`"<aci>.<deviceId>"`) with HTTP `status`: for a device that is not linked yet, so that
    /// the first connection after its link is refused already.
    pub async fn fail_auth_named(
        &self,
        username: &str,
        status: u16,
        count: u32,
    ) -> Result<(), HarnessError> {
        let args = json!({ "username": username, "status": status, "count": count });
        self.request_value("failAuth", args).await.map(drop)
    }

    /// The phone's user chooses "Transfer message history": `primary` puts this ephemeral
    /// backup key into the provisioning message of a link whose URL offers `backup5`.
    pub async fn set_ephemeral_backup_key(&self, primary: &str) -> Result<[u8; 32], HarnessError> {
        let value = self
            .request_value("setEphemeralBackupKey", json!({ "primary": primary }))
            .await?;
        let key = value
            .get("key")
            .and_then(Value::as_str)
            .and_then(|key| BASE64.decode(key).ok())
            .and_then(|key| <[u8; 32]>::try_from(key.as_slice()).ok());
        key.ok_or_else(|| HarnessError::Protocol("setEphemeralBackupKey: no key".to_owned()))
    }

    /// Puts the file at `path` on the mock's CDN 3 as the phone's uploaded archive.
    pub async fn store_transfer_archive(&self, path: &Path) -> Result<ArchivePlace, HarnessError> {
        self.request("storeTransferArchive", json!({ "path": path }))
            .await
    }

    /// The phone answers the new device's long-poll with the archive's place.
    pub async fn provide_transfer_archive(
        &self,
        device: &str,
        place: &ArchivePlace,
    ) -> Result<(), HarnessError> {
        let args = json!({ "device": device, "cdn": place.cdn, "key": place.key });
        self.request_value("provideTransferArchive", args)
            .await
            .map(drop)
    }

    /// The phone answers the new device's long-poll with an error value
    /// (`RELINK_REQUESTED`, `CONTINUE_WITHOUT_UPLOAD`).
    pub async fn provide_transfer_error(
        &self,
        device: &str,
        error: &str,
    ) -> Result<(), HarnessError> {
        self.request_value(
            "provideTransferArchive",
            json!({ "device": device, "error": error }),
        )
        .await
        .map(drop)
    }

    /// Faults of the transfer long-poll: never answered (`silent`), or the next `fail`
    /// answered with 500.
    pub async fn hold_transfer_archive(&self, silent: bool, fail: u32) -> Result<(), HarnessError> {
        self.request_value(
            "holdTransferArchive",
            json!({ "silent": silent, "fail": fail }),
        )
        .await
        .map(drop)
    }

    /// The next `count` transfer long-polls are answered with `status` (a refusal such as
    /// 403, or 429).
    pub async fn refuse_transfer_archive(
        &self,
        status: u16,
        count: u32,
    ) -> Result<(), HarnessError> {
        self.request_value(
            "holdTransferArchive",
            json!({ "fail": count, "status": status }),
        )
        .await
        .map(drop)
    }

    /// Every transfer long-poll is answered 204 at once instead of being held for its
    /// timeout (`immediate`), or held again.
    pub async fn answer_transfer_archive_at_once(
        &self,
        immediate: bool,
    ) -> Result<(), HarnessError> {
        self.request_value("holdTransferArchive", json!({ "immediate": immediate }))
            .await
            .map(drop)
    }

    /// [`Self::set_ephemeral_backup_key`] for a phone that sends its key whether or not the
    /// linking URL offers `backup5` (no real phone does; a client must ignore such a key).
    pub async fn force_ephemeral_backup_key(&self, primary: &str) -> Result<(), HarnessError> {
        self.request_value(
            "setEphemeralBackupKey",
            json!({ "primary": primary, "force": true }),
        )
        .await
        .map(drop)
    }

    /// The transfer long-polls clients sent so far.
    pub async fn transfer_polls(&self) -> Result<Vec<TransferPoll>, HarnessError> {
        #[derive(Deserialize)]
        struct Stats {
            polls: Vec<TransferPoll>,
        }
        let stats: Stats = self.request("transferStats", json!({})).await?;
        Ok(stats.polls)
    }

    /// The downloads asked of CDN 3 so far, in order.
    pub async fn downloads(&self) -> Result<Vec<DownloadStat>, HarnessError> {
        #[derive(Deserialize)]
        struct Stats {
            downloads: Vec<DownloadStat>,
        }
        let stats: Stats = self.request("downloadStats", json!({})).await?;
        Ok(stats.downloads)
    }

    /// Holds the answer to every `PUT /v1/devices/link` back for `hold` after the server
    /// linked the device (zero: answers at once again).
    pub async fn delay_link(&self, hold: Duration) -> Result<(), HarnessError> {
        self.request_value("delayLink", json!({ "ms": millis(hold) }))
            .await
            .map(drop)
    }

    /// Answers every request a client makes from now on `rtt` later (zero: at once again):
    /// a round trip as a real network has.
    pub async fn set_latency(&self, rtt: Duration) -> Result<(), HarnessError> {
        self.request_value("setLatency", json!({ "ms": millis(rtt) }))
            .await
            .map(drop)
    }

    /// Moves the clock the server states on every websocket upgrade by `skew_ms`.
    pub async fn set_server_time(&self, skew_ms: i64) -> Result<(), HarnessError> {
        self.request_value("setServerTime", json!({ "skewMs": skew_ms }))
            .await
            .map(drop)
    }

    /// Answers keepalives with 500 (`on`) or serves them again.
    pub async fn fail_keepalive(&self, on: bool) -> Result<(), HarnessError> {
        self.request_value("failKeepalive", json!({ "on": on }))
            .await
            .map(drop)
    }

    /// How `device` has been connecting (`connectionStats`).
    pub async fn connection_stats(&self, device: &str) -> Result<ConnectionStats, HarnessError> {
        self.request("connectionStats", json!({ "device": device }))
            .await
    }

    /// The one-time prekeys the server holds for `device` and its key uploads (`keyCounts`).
    pub async fn key_counts(&self, device: &str) -> Result<KeyCounts, HarnessError> {
        self.request("keyCounts", json!({ "device": device })).await
    }

    /// Uses up `device`'s one-time prekeys of `kind` (`"ACI"` or `"PNI"`) on the server until
    /// `leave` of each sort are left.
    pub async fn consume_keys(
        &self,
        device: &str,
        kind: &str,
        leave: u32,
    ) -> Result<(), HarnessError> {
        let args = json!({ "device": device, "kind": kind, "leave": leave });
        self.request_value("consumeKeys", args).await.map(drop)
    }

    /// `primary` changes its phone number; its linked devices get the `PniChangeNumber` sync.
    pub async fn change_number(&self, primary: &str) -> Result<ChangedNumber, HarnessError> {
        self.request("changeNumber", json!({ "primary": primary }))
            .await
    }

    /// Client 1:1 sends to `target` are refused with a challenge (428 and `token`) until a
    /// client answers it; `accept`: the only captcha solution that is accepted (`None`: any).
    pub async fn challenge(
        &self,
        target: &str,
        token: &str,
        accept: Option<&str>,
    ) -> Result<(), HarnessError> {
        let mut args = json!({ "target": target, "token": token });
        if let Some(accept) = accept {
            args["accept"] = json!(accept);
        }
        self.request_value("challenge", args).await.map(drop)
    }

    /// Like [`Self::challenge`], with the `Retry-After` (seconds) the 428 states.
    pub async fn challenge_for(
        &self,
        target: &str,
        token: &str,
        retry_after: u32,
    ) -> Result<(), HarnessError> {
        let args = json!({ "target": target, "token": token, "retryAfter": retry_after });
        self.request_value("challenge", args).await.map(drop)
    }

    /// Lifts the challenge on sends to `target` without an answer (the server's own doing).
    pub async fn stop_challenge(&self, target: &str) -> Result<(), HarnessError> {
        self.request_value("challenge", json!({ "target": target, "stop": true }))
            .await
            .map(drop)
    }

    /// Multi-recipient sends over the websocket route are refused with a challenge (428,
    /// `token`, `Retry-After: retry_after`) until [`Self::stop_challenge_multi_recipient`].
    /// Returns how many were refused so far.
    pub async fn challenge_multi_recipient(
        &self,
        token: &str,
        retry_after: u32,
    ) -> Result<u64, HarnessError> {
        let args = json!({ "token": token, "retryAfter": retry_after });
        let value = self.request_value("challengeMultiRecipient", args).await?;
        Ok(value["refused"].as_u64().unwrap_or(0))
    }

    /// Serves multi-recipient sends over the websocket route as before; returns how many were
    /// refused with the challenge.
    pub async fn stop_challenge_multi_recipient(&self) -> Result<u64, HarnessError> {
        let value = self
            .request_value("challengeMultiRecipient", json!({ "stop": true }))
            .await?;
        Ok(value["refused"].as_u64().unwrap_or(0))
    }

    /// What clients answered to challenges so far, and the challenges still open.
    pub async fn challenge_answers(&self) -> Result<ChallengeAnswers, HarnessError> {
        self.request("challengeAnswers", json!({})).await
    }

    /// Registers one more scripted (undriven) device on `primary`'s account.
    pub async fn add_device(&self, primary: &str) -> Result<DeviceInfo, HarnessError> {
        self.request("addDevice", json!({ "primary": primary }))
            .await
    }

    /// Waits for the next client envelope addressed to the scripted device `device`.
    pub async fn wait_for_envelope(
        &self,
        device: &str,
        timeout: Duration,
    ) -> Result<ScriptedEnvelope, HarnessError> {
        let args = json!({ "device": device, "timeout": millis(timeout) });
        self.request("waitForEnvelope", args).await
    }

    /// Sends a receipt (`delivery`, `read`, `viewed`) from a primary to one device.
    pub async fn send_receipt(
        &self,
        from: &str,
        to: &str,
        kind: &str,
        timestamps: &[u64],
        sealed: bool,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "type": kind, "timestamps": timestamps, "sealed": sealed,
            "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendReceipt", args).await
    }

    /// Sends a 1:1 typing indicator (`started`, `stopped`) from a primary to one device.
    pub async fn send_typing(
        &self,
        from: &str,
        to: &str,
        action: &str,
        sealed: bool,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "action": action, "sealed": sealed,
            "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendTyping", args).await
    }

    /// The phone's sync transcript of a text it "sent" to `destination` (an ACI), to its linked
    /// device `to`, with the phone's profile key in it (as real phones send in accepted chats).
    pub async fn send_sync_sent(
        &self,
        from: &str,
        to: &str,
        destination: &str,
        text: &str,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "destination": destination, "text": text,
            "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendSyncSent", args).await
    }

    /// The phone accepted (or blocked, …) the chat with `thread` (an ACI): a
    /// `MessageRequestResponse` sync to its linked device `to`.
    pub async fn send_message_request_response(
        &self,
        from: &str,
        to: &str,
        thread: &str,
        kind: &str,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "thread": thread, "type": kind,
            "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendMessageRequestResponse", args).await
    }

    /// A configuration sync (read receipts, typing indicators) from the phone to its linked
    /// device `to`.
    pub async fn send_configuration(
        &self,
        from: &str,
        to: &str,
        read_receipts: bool,
        typing_indicators: bool,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "readReceipts": read_receipts,
            "typingIndicators": typing_indicators, "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendConfiguration", args).await
    }

    /// Waits for the next receipt a primary decrypted, optionally only one `from` a device.
    pub async fn wait_for_receipt(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedReceipt, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForReceipt", args).await
    }

    /// Waits for the next typing indicator a primary decrypted, optionally `from` a device.
    pub async fn wait_for_typing(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedTyping, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForTyping", args).await
    }

    /// Waits for the next Content a primary decrypted that was a sender-key distribution
    /// message and nothing else, optionally `from` a device.
    pub async fn wait_for_distribution(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedDistribution, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForDistribution", args).await
    }

    /// Waits for the next sync message of `kind` (`sent`, `read`, …) a primary got from one of
    /// its linked devices, optionally `from` one device.
    pub async fn wait_for_sync_message(
        &self,
        primary: &str,
        from: Option<&str>,
        kind: &str,
        timeout: Duration,
    ) -> Result<ReceivedSync, HarnessError> {
        let mut args = json!({ "primary": primary, "kind": kind, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForSyncMessage", args).await
    }

    /// A group typing indicator (`started`, `stopped`) from a member to one device.
    pub async fn send_group_typing(
        &self,
        from: &str,
        to: &str,
        group: &str,
        action: &str,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "action": action, "group": group,
            "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendTyping", args).await
    }

    /// Creates a group on the mock's Storage Service (title, description, timer, avatar,
    /// members) as the phone of `spec.from` would.
    pub async fn create_group(&self, spec: &CreateGroup) -> Result<GroupInfo, HarnessError> {
        self.request("createGroup", to_args(spec)?).await
    }

    /// One signed change to a group, and the update message to `spec.to`.
    pub async fn modify_group(&self, spec: &ModifyGroup) -> Result<ModifiedGroup, HarnessError> {
        self.request("modifyGroup", to_args(spec)?).await
    }

    /// A group text from a member to some devices, pairwise or by sender key.
    pub async fn send_group_text(&self, spec: &SendGroupText) -> Result<GroupSent, HarnessError> {
        self.request("sendGroupText", to_args(spec)?).await
    }

    /// Sends `to` a 1:1 message the device cannot decrypt (a sealed sender-key message whose
    /// distribution message is never sent), with content hint `hint` (`default`, `resendable`,
    /// `implicit`). Returns the message timestamp.
    pub async fn send_undecryptable(
        &self,
        from: &str,
        to: &str,
        text: &str,
        hint: &str,
        timestamp: u64,
    ) -> Result<u64, HarnessError> {
        let sent: Sent = self
            .request(
                "sendUndecryptable",
                json!({ "from": from, "to": to, "text": text, "hint": hint, "timestamp": timestamp }),
            )
            .await?;
        Ok(sent.timestamp)
    }

    /// Sends `to` a retry request for the message that device `sender_device` sent at
    /// `timestamp` (so the recipient resends it from its Message Send Log).
    pub async fn send_retry_request(
        &self,
        from: &str,
        to: &str,
        timestamp: u64,
        sender_device: u32,
    ) -> Result<(), HarnessError> {
        let _: Sent = self
            .request(
                "sendRetryRequest",
                json!({
                    "from": from, "to": to, "timestamp": timestamp, "senderDevice": sender_device,
                }),
            )
            .await?;
        Ok(())
    }

    /// Waits for the next retry request a primary decrypted, optionally only one `from` a device.
    pub async fn wait_for_decryption_error(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedDecryptionError, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForDecryptionError", args).await
    }

    /// Makes every Storage Service request for `group` fail with 500 (`fail`), or work again.
    pub async fn fail_group_fetch(&self, group: &str, fail: bool) -> Result<(), HarnessError> {
        let _: Value = self
            .request_value("failGroupFetch", json!({ "group": group, "fail": fail }))
            .await?;
        Ok(())
    }

    /// Makes every linked device's Storage Service manifest request fail with 500 (`fail`),
    /// or work again. Returns how many were refused so far.
    pub async fn fail_storage_manifest(&self, fail: bool) -> Result<u64, HarnessError> {
        let v = self
            .request_value("failStorageManifest", json!({ "fail": fail }))
            .await?;
        Ok(v["refused"].as_u64().unwrap_or(0))
    }

    /// Every profile request answers 429 with `Retry-After: retry_after_secs` (`None`: served
    /// again). Returns how many were refused so far.
    pub async fn limit_profile_fetches(
        &self,
        retry_after_secs: Option<u32>,
    ) -> Result<u64, HarnessError> {
        let args = match retry_after_secs {
            Some(secs) => json!({ "retryAfter": secs }),
            None => json!({ "stop": true }),
        };
        let v = self.request_value("limitProfileFetches", args).await?;
        Ok(v["refused"].as_u64().unwrap_or(0))
    }

    /// Replaces the phone's Storage Service state (a new manifest version).
    pub async fn set_storage_state(
        &self,
        spec: &StorageSpec,
    ) -> Result<StorageWritten, HarnessError> {
        self.request("setStorageState", to_args(spec)?).await
    }

    /// `SyncMessage.FetchLatest{type}` from the phone to its linked device `to` (`type`:
    /// `storageManifest`, `localProfile`, `subscriptionStatus`); waits for the delivery.
    pub async fn send_fetch_latest(
        &self,
        from: &str,
        to: &str,
        kind: &str,
    ) -> Result<Sent, HarnessError> {
        let args = json!({
            "from": from, "to": to, "type": kind, "awaitDelivery": true, "timeout": 60_000,
        });
        self.request("sendFetchLatest", args).await
    }

    /// Sets one remote-config entry served by `/v2/config`.
    pub async fn set_remote_config(
        &self,
        key: &str,
        enabled: bool,
        value: Option<&str>,
    ) -> Result<(), HarnessError> {
        let mut args = json!({ "key": key, "enabled": enabled });
        if let Some(value) = value {
            args["value"] = json!(value);
        }
        self.request_value("setRemoteConfig", args).await.map(drop)
    }

    /// Sends a `DataMessage` built from parts, or with `edit` an `EditMessage` around it
    /// (`sendData`; `args` are the command's arguments as JSON: see the harness README): to one
    /// device, to several devices of a group (`to` an array), or with `sync` as the phone's
    /// transcript.
    pub async fn send_data(&self, args: Value) -> Result<SentData, HarnessError> {
        self.request("sendData", args).await
    }

    /// Sends a sync message other than a transcript from a phone to its linked device
    /// (`sendSync`): `deleteForMe`, `viewOnceOpen`, `viewed`, `read` or `callEvent`.
    pub async fn send_sync(&self, args: Value) -> Result<SentData, HarnessError> {
        self.request("sendSync", args).await
    }

    /// The phone's answer to a request for lost files (`sendSync {attachmentBackfillResponse}`)
    /// from the primary `from` to its linked device `to`; `pointers` of the result are what
    /// it put in, in entry order (long text last).
    pub async fn send_backfill_answer(
        &self,
        from: &str,
        to: &str,
        answer: &BackfillAnswer,
        await_delivery: bool,
    ) -> Result<SentData, HarnessError> {
        let args = json!({
            "from": from, "to": to, "attachmentBackfillResponse": answer,
            "awaitDelivery": await_delivery, "timeout": 60_000,
        });
        self.request("sendSync", args).await
    }

    /// The next request for lost files (`SyncMessage.attachmentBackfillRequest`) the primary
    /// got, optionally only one `from` a device.
    pub async fn wait_for_backfill_request(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<BackfillRequest, HarnessError> {
        let sync = self
            .wait_for_sync_message(primary, from, "attachmentBackfillRequest", timeout)
            .await?;
        let request = sync
            .sync_message
            .pointer("/content/attachmentBackfillRequest")
            .or_else(|| sync.sync_message.get("attachmentBackfillRequest"))
            .cloned()
            .unwrap_or(Value::Null);
        let message = &request["targetMessage"];
        Ok(BackfillRequest {
            source: sync.source,
            author: message
                .pointer("/author/authorServiceIdBinary")
                .or_else(|| message.get("authorServiceIdBinary"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            sent_timestamp: message["sentTimestamp"].as_u64().or_else(|| {
                message["sentTimestamp"]
                    .as_str()
                    .and_then(|t| t.parse().ok())
            }),
            conversation: request["targetConversation"].clone(),
            raw: request,
        })
    }

    /// Sends call signalling from a primary to a device (`sendCall`; `args` are the command's
    /// arguments as JSON: an `offer`, a `hangup` of a type, a group `ring`, raw `opaque`
    /// bytes, `ageMs` for the time the envelope waited on the server).
    pub async fn send_call(&self, args: Value) -> Result<SentData, HarnessError> {
        self.request("sendCall", args).await
    }

    /// Waits for the next `DataMessage` of `kind` (`message`, `reaction`, `delete`,
    /// `adminDelete`, `timer`) a primary got, optionally only one `from` a device or in `group`;
    /// messages of other kinds stay buffered.
    pub async fn wait_for_data(
        &self,
        primary: &str,
        kind: &str,
        from: Option<&str>,
        group: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedMessage, HarnessError> {
        let mut args = json!({ "primary": primary, "kind": kind, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        if let Some(group) = group {
            args["group"] = json!(group);
        }
        self.request("waitForMessage", args).await
    }

    /// Waits for the next reaction a primary got ([`Self::wait_for_data`] of kind `reaction`).
    pub async fn wait_for_reaction(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedMessage, HarnessError> {
        self.wait_for_data(primary, "reaction", from, None, timeout)
            .await
    }

    /// Waits for the next delete for everyone a primary got.
    pub async fn wait_for_delete(
        &self,
        primary: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedMessage, HarnessError> {
        self.wait_for_data(primary, "delete", from, None, timeout)
            .await
    }

    /// Waits for the next edit a primary got, optionally only one `from` a device or in
    /// `group`.
    pub async fn wait_for_edit(
        &self,
        primary: &str,
        from: Option<&str>,
        group: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedEdit, HarnessError> {
        let mut args = json!({ "primary": primary, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        if let Some(group) = group {
            args["group"] = json!(group);
        }
        self.request("waitForEdit", args).await
    }

    /// Waits for the next message `primary` got in `group` (a group id), optionally only one
    /// `from` a device.
    pub async fn wait_for_group_message(
        &self,
        primary: &str,
        group: &str,
        from: Option<&str>,
        timeout: Duration,
    ) -> Result<ReceivedMessage, HarnessError> {
        let mut args = json!({ "primary": primary, "group": group, "timeout": millis(timeout) });
        if let Some(from) = from {
            args["from"] = json!(from);
        }
        self.request("waitForMessage", args).await
    }

    /// Client 1:1 sends to `target` answer 429 with `Retry-After: retry_after_secs` for the next
    /// `count` sends (`None`: until [`Self::stop_rate_limit`]).
    pub async fn rate_limit(
        &self,
        target: &str,
        count: Option<u32>,
        retry_after_secs: u32,
    ) -> Result<(), HarnessError> {
        let mut args = json!({ "target": target, "retryAfter": retry_after_secs });
        if let Some(count) = count {
            args["count"] = json!(count);
        }
        self.request_value("rateLimit", args).await.map(drop)
    }

    /// Ends a [`Self::rate_limit`].
    pub async fn stop_rate_limit(&self, target: &str) -> Result<(), HarnessError> {
        self.request_value("rateLimit", json!({ "target": target, "stop": true }))
            .await
            .map(drop)
    }

    /// Refuses (`reject`) or accepts again every multi-recipient send's group send token.
    pub async fn reject_group_send_tokens(&self, reject: bool) -> Result<(), HarnessError> {
        self.request_value("rejectGroupSendTokens", json!({ "reject": reject }))
            .await
            .map(drop)
    }

    /// Answers (`on`) or serves again every multi-recipient send with a response libsignal's
    /// typed API does not know.
    pub async fn answer_multi_recipient_unexpectedly(&self, on: bool) -> Result<(), HarnessError> {
        self.request_value("answerMultiRecipientUnexpectedly", json!({ "on": on }))
            .await
            .map(drop)
    }

    /// What clients sent so far (`sendStats`).
    pub async fn send_stats(&self) -> Result<SendStats, HarnessError> {
        self.request("sendStats", json!({})).await
    }

    /// `primary` forgets the sender keys it holds from `from`; returns how many.
    pub async fn break_sender_keys(&self, primary: &str, from: &str) -> Result<u64, HarnessError> {
        let value = self
            .request_value(
                "breakSenderKeys",
                json!({ "primary": primary, "from": from }),
            )
            .await?;
        Ok(value["forgotten"].as_u64().unwrap_or(0))
    }

    /// Waits until `primary` could not decrypt a client envelope (it then sends a retry request).
    pub async fn wait_for_decrypt_failure(
        &self,
        primary: &str,
        timeout: Duration,
    ) -> Result<DecryptFailure, HarnessError> {
        self.request(
            "waitForDecryptFailure",
            json!({ "primary": primary, "timeout": millis(timeout) }),
        )
        .await
    }
}

/// How a device has been connecting (`connectionStats`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStats {
    /// Authenticated websocket upgrades the device asked for (refused ones included).
    pub connects: u64,
    /// The device has a socket now.
    pub open: bool,
    /// Keepalive requests the server got (from any socket).
    pub keepalives: u64,
}

/// The server's prekeys of one device (`keyCounts`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyCounts {
    /// Keys of the ACI identity.
    pub aci: Option<IdentityKeys>,
    /// Keys of the PNI identity.
    pub pni: Option<IdentityKeys>,
}

/// The server's prekeys of one identity of a device.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityKeys {
    /// One-time EC prekeys left.
    pub count: u64,
    /// One-time Kyber prekeys left.
    pub pq_count: u64,
    /// Id of the signed prekey.
    pub signed_pre_key_id: Option<u32>,
    /// Id of the Kyber last-resort key.
    pub last_resort_key_id: Option<u32>,
    /// What each client upload carried, in order.
    pub uploads: Vec<KeyUpload>,
}

/// One client key upload (`PUT /v2/keys`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyUpload {
    /// One-time EC prekeys in it.
    pub pre_keys: u32,
    /// One-time Kyber prekeys in it.
    pub pq_pre_keys: u32,
    /// Id of its signed prekey, if it brought one.
    pub signed_pre_key: Option<u32>,
    /// Id of its Kyber last-resort key, if it brought one.
    pub pq_last_resort_pre_key: Option<u32>,
}

/// A primary after `changeNumber`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedNumber {
    /// The new PNI (`PNI:<uuid>`).
    pub pni: String,
    /// The PNI before the change.
    pub old_pni: String,
    /// The new number.
    pub e164: String,
    /// The new PNI identity key (base64 of the serialized public key).
    pub pni_identity_key: String,
    /// The new PNI registration id.
    pub pni_registration_id: Option<u32>,
}

/// Challenge answers the server got and the challenges still open (`challengeAnswers`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeAnswers {
    /// Every `PUT /v1/challenge`, in order.
    pub answers: Vec<ChallengeAnswer>,
    /// Open challenges by the recipient's service id.
    pub open: std::collections::BTreeMap<String, OpenChallenge>,
}

/// One `PUT /v1/challenge`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeAnswer {
    /// Its `type` (`captcha`).
    pub r#type: String,
    /// The challenge token it names.
    pub token: String,
    /// The solution.
    pub captcha: String,
    /// The server accepted it and lifted the challenge.
    pub accepted: bool,
    /// The HTTP status it got.
    pub status: u16,
}

/// A challenge nobody answered yet.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenChallenge {
    /// Its token.
    pub token: String,
    /// Sends refused with it so far.
    pub refused: u64,
}

/// What clients sent so far (`sendStats`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendStats {
    /// Client 1:1 sends per destination service id (rate-limited ones included).
    pub single: std::collections::BTreeMap<String, u64>,
    /// The `urgent` flag of every 1:1 send per destination service id, in order.
    #[serde(default)]
    pub urgency: std::collections::BTreeMap<String, Vec<SendFlag>>,
    /// Multi-recipient sends served.
    pub multi: u64,
    /// Recipients (service ids) of every multi-recipient send, in order.
    pub multi_recipients: Vec<String>,
    /// Multi-recipient sends refused for their group send token.
    pub refused_tokens: u64,
    /// Multi-recipient sends answered by `answerMultiRecipientUnexpectedly`.
    #[serde(default)]
    pub unexpected_answers: u64,
    /// Storage Service group requests per group id.
    pub group_fetches: std::collections::BTreeMap<String, u64>,
    /// Attachment uploads clients started on CDN 3 (TUS creations).
    #[serde(default)]
    pub uploads: u64,
    /// Every request a client made to send a message to anybody, answered with success or
    /// not: the 1:1 route (messages, receipts, typing, call signalling, sync messages to its
    /// own account) plus every multi-recipient send on either route.
    #[serde(default)]
    pub send_requests: u64,
    /// `PUT /v1/devices/link` requests served: one for every device a client linked (or
    /// tried to link). A link refused before this request leaves none.
    #[serde(default)]
    pub link_requests: u64,
    /// The path of every `GET /v1/certificate/delivery`, in order: the sender certificates
    /// clients fetched (the kind without the number asks `?includeE164=false`).
    #[serde(default)]
    pub certificate_requests: Vec<String>,
}

/// One 1:1 send as the server saw its request (`SendStats::urgency`).
#[derive(Debug, Clone, Deserialize)]
pub struct SendFlag {
    /// The request's timestamp.
    pub timestamp: u64,
    /// The request said it was urgent.
    pub urgent: bool,
}

impl SendStats {
    /// 1:1 sends to the primary with ACI `aci`.
    pub fn single_to(&self, aci: &str) -> u64 {
        self.single.get(aci).copied().unwrap_or(0)
    }
}

/// An envelope a primary could not decrypt (`waitForDecryptFailure`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecryptFailure {
    /// Ref of the sending device.
    pub source: String,
    /// The envelope timestamp.
    pub timestamp: u64,
    /// The inner envelope type (`SenderKey`, `CipherText`, `PreKey`, `Plaintext`).
    pub envelope_type: String,
}

/// Milliseconds for a `timeout` argument (at least 1, the harness rejects 0).
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn to_args<T: Serialize>(value: &T) -> Result<Value, HarnessError> {
    serde_json::to_value(value).map_err(|error| HarnessError::Protocol(error.to_string()))
}

async fn read_handshake(lines: &mut Lines<BufReader<ChildStdout>>) -> Result<Handshake, String> {
    loop {
        let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? else {
            return Err(String::from("harness exited before its handshake"));
        };
        if line.trim().is_empty() {
            continue;
        }
        return match serde_json::from_str::<Line>(&line) {
            Ok(Line::Ready(handshake)) => Ok(*handshake),
            Ok(Line::Fatal { error }) => Err(format!("{}: {}", error.code, error.message)),
            Ok(_) => Err(format!("expected the handshake, got {line}")),
            Err(error) => Err(format!("bad handshake line ({error}): {line}")),
        };
    }
}

async fn read_stdout(
    mut lines: Lines<BufReader<ChildStdout>>,
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<HarnessEvent>,
) {
    let reason = loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break String::from("harness exited"),
            Err(error) => break format!("reading harness stdout: {error}"),
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(&line) {
            Ok(Line::Response {
                id,
                ok,
                result,
                error,
            }) => {
                let reply = if ok {
                    Ok(result)
                } else {
                    Err(error.unwrap_or_else(|| WireError {
                        code: String::from("failed"),
                        message: String::from("error response without an error"),
                    }))
                };
                // A response with no id (unparseable request) has no waiter.
                let waiter = id.and_then(|id| lock(&shared.pending).waiting.remove(&id));
                if let Some(waiter) = waiter {
                    // The requester may have given up already.
                    let _ = waiter.send(reply);
                }
            }
            Ok(Line::Event { event, data }) => {
                let _ = events.send(HarnessEvent { event, data });
            }
            Ok(Line::Fatal { error }) => {
                break format!("harness failed: {}: {}", error.code, error.message);
            }
            Ok(Line::Ready(_)) => {
                break String::from("second handshake line");
            }
            Err(error) => break format!("bad line from the harness ({error}): {line}"),
        }
    };
    shared.close(reason);
}

async fn read_stderr(stderr: tokio::process::ChildStderr, shared: Arc<Shared>) {
    let verbose = std::env::var_os(VERBOSE_ENV).is_some();
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if verbose {
            eprintln!("{line}");
        }
        let mut tail = lock(&shared.stderr_tail);
        if tail.len() == STDERR_TAIL {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

// -------------------------------------------------------------------------------------------
// Typed arguments and results
// -------------------------------------------------------------------------------------------

/// Arguments of `createPrimary`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePrimary {
    /// Profile name of the account.
    pub profile_name: String,
    /// Refs of earlier primaries served to linked devices as contact sync.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<String>,
    /// Groups this primary creates (with itself as a member).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupSpec>,
    /// `false` registers an account without a phone number (default `true`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_e164: Option<bool>,
    /// Extra mock secondary devices of the account (default 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_devices: Option<u32>,
    /// Profile avatar (image bytes, base64), served encrypted from CDN0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Profile "about" text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Profile "about" emoji.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about_emoji: Option<String>,
}

impl CreatePrimary {
    /// An account with just a profile name.
    pub fn named(profile_name: impl Into<String>) -> Self {
        Self {
            profile_name: profile_name.into(),
            ..Self::default()
        }
    }
}

/// Arguments of `setStorageState`: the phone's whole Storage Service state.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// The phone (a primary ref).
    pub primary: String,
    /// Its contacts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<StorageContact>,
    /// Its groups (`GroupV2Record`s of harness groups).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<StorageGroup>,
    /// Synthetic contacts ("Contact <n>", random ACIs, not accepted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_contacts: Option<u32>,
    /// The account record's settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<StorageAccount>,
    /// `Some(false)`: legacy record keys (no `recordIkm` in the manifest).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ikm: Option<bool>,
}

/// One `ContactRecord` of [`StorageSpec`] (absent fields take the harness defaults).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageContact {
    /// The contact (a primary ref).
    #[serde(rename = "ref")]
    pub contact_ref: String,
    /// Include their profile key (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_key: Option<bool>,
    /// Profile given name (default: their profile name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// Address-book given name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_given_name: Option<String>,
    /// Accepted chat (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whitelisted: Option<bool>,
    /// Blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
    /// Archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Marked unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked_unread: Option<bool>,
    /// Muted until (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted_until: Option<u64>,
    /// Pinned (in this order).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// The record names the contact's PNI and number only (a contact the phone knows by
    /// number): no ACI, profile key, identity key or profile name.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub by_pni: bool,
    /// The record carries the PNI beside the ACI (the phone joined the two).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub with_pni: bool,
}

impl StorageContact {
    /// A whitelisted contact with profile key and identity key.
    pub fn of(contact_ref: impl Into<String>) -> Self {
        Self {
            contact_ref: contact_ref.into(),
            ..Self::default()
        }
    }
}

/// One `GroupV2Record` of [`StorageSpec`] (absent fields take the harness defaults).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageGroup {
    /// The group id (base64).
    pub id: String,
    /// Blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
    /// Accepted (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whitelisted: Option<bool>,
    /// Archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Marked unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marked_unread: Option<bool>,
    /// Muted until (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted_until: Option<u64>,
    /// Pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
}

/// Arguments of `createGroup`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateGroup {
    /// The creating primary (an administrator).
    pub from: String,
    /// Title.
    pub title: String,
    /// Refs of the other members.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Refs of administrators among them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub admins: Vec<String>,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Disappearing-message timer in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer: Option<u32>,
    /// Avatar image bytes, base64.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Only administrators may send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announcements_only: Option<bool>,
}

/// Arguments of `modifyGroup`: one signed change.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModifyGroup {
    /// The member making the change.
    pub from: String,
    /// The group id.
    pub group: String,
    /// New title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// New timer (seconds, 0 = off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer: Option<u32>,
    /// New avatar (base64; empty removes it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Refs of primaries to add.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_members: Vec<String>,
    /// Refs of members to remove.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove_members: Vec<String>,
    /// Only administrators may send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announcements_only: Option<bool>,
    /// Devices that get the update message.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    /// The update's timestamp (default now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// The result of `modifyGroup`.
#[derive(Debug, Clone, Deserialize)]
pub struct ModifiedGroup {
    /// The new revision.
    pub revision: u64,
    /// The update message's timestamp.
    pub timestamp: u64,
    /// The group after the change.
    pub group: GroupInfo,
}

/// Arguments of `sendGroupText`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendGroupText {
    /// The sending member.
    pub from: String,
    /// The group id.
    pub group: String,
    /// Target devices.
    pub to: Vec<String>,
    /// Text.
    pub text: String,
    /// Timestamp (default now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    /// Sender key (sealed) instead of pairwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sender_key: bool,
}

/// The result of `sendGroupText`.
#[derive(Debug, Clone, Deserialize)]
pub struct GroupSent {
    /// The text's timestamp.
    pub timestamp: u64,
    /// The target devices.
    pub to: Vec<String>,
}

/// The `AccountRecord` settings of [`StorageSpec`].
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageAccount {
    /// Read receipts (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_receipts: Option<bool>,
    /// Typing indicators (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing_indicators: Option<bool>,
    /// Link previews (default false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_previews: Option<bool>,
    /// `everybody` (default) or `nobody`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone_number_sharing: Option<String>,
    /// Universal disappearing timer in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub universal_expire_timer: Option<u32>,
    /// Note to Self archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_to_self_archived: Option<bool>,
    /// Note to Self marked unread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_to_self_marked_unread: Option<bool>,
}

/// The result of `setStorageState`.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageWritten {
    /// The new manifest version.
    pub version: u64,
    /// Records in the manifest.
    pub records: u64,
}

/// A group to create in `createPrimary`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupSpec {
    /// Group title.
    pub title: String,
    /// Refs of other primaries to add.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// A primary created by the harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Primary {
    /// Device ref `<aci>.<deviceId>`.
    #[serde(rename = "ref")]
    pub device_ref: String,
    /// ACI (lowercase UUID).
    pub aci: String,
    /// PNI (`PNI:<uuid>`), absent for accounts without a phone number.
    pub pni: Option<String>,
    /// Device id (1 for a primary).
    pub device_id: u32,
    /// Phone number, absent for accounts without one.
    pub e164: Option<String>,
    /// Profile name.
    pub profile_name: String,
    /// Profile key, base64.
    pub profile_key: String,
    /// ACI identity public key, base64.
    pub identity_key: String,
    /// Refs of the account's other devices.
    pub secondary_devices: Vec<String>,
    /// Refs of the contacts served as contact sync.
    pub contacts: Vec<String>,
    /// Groups this primary created.
    pub groups: Vec<GroupInfo>,
}

/// A group created by the harness.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInfo {
    /// Group identifier, base64 (what `sendText{group}` takes).
    pub id: String,
    /// Title.
    pub title: String,
    /// Group master key, base64.
    pub master_key: String,
    /// Current revision.
    pub revision: u64,
    /// ACIs of the full members.
    #[serde(default)]
    pub members: Vec<String>,
    /// CDN0 key of the encrypted avatar.
    #[serde(default)]
    pub avatar_key: Option<String>,
}

/// A retry request (`DecryptionErrorMessage`) a primary decrypted.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedDecryptionError {
    /// The device the request came from.
    pub source: String,
    /// The original message's timestamp.
    pub timestamp: u64,
    /// The device the original message was sent from (the recipient's own device id).
    pub sender_device: u32,
    /// Whether the request named a ratchet key.
    pub has_ratchet_key: bool,
}

/// A device known to the server.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    /// Device ref `<aci>.<deviceId>`.
    #[serde(rename = "ref")]
    pub device_ref: String,
    /// ACI of the account.
    pub aci: String,
    /// PNI of the account.
    pub pni: Option<String>,
    /// Device id.
    pub device_id: u32,
    /// Phone number of the account.
    pub e164: Option<String>,
    /// `completeProvision` only: the capabilities the linking URL offered.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `completeProvision` only: whether the phone put an ephemeral backup key into its
    /// provisioning message (it does when the URL offers `backup5` and
    /// `setEphemeralBackupKey` chose a transfer).
    #[serde(default)]
    pub sent_transfer_key: bool,
    /// `completeProvision` only: ms from the provisioning message leaving the phone to the
    /// link request reaching the server.
    #[serde(default)]
    pub link_request_after_ms: Option<u64>,
    /// `completeProvision` only: ms from the provisioning message leaving the phone to the
    /// server's answer to the link request (the new device exists for the phone).
    #[serde(default)]
    pub linked_after_ms: Option<u64>,
}

/// Where a transfer archive is on the mock's CDN (`storeTransferArchive`).
#[derive(Debug, Clone, Deserialize)]
pub struct ArchivePlace {
    /// The CDN number (3).
    pub cdn: u32,
    /// The object key.
    pub key: String,
    /// The file's size in bytes.
    pub size: u64,
}

/// One long-poll for the transfer archive (`transferStats`).
#[derive(Debug, Clone, Deserialize)]
pub struct TransferPoll {
    /// When it arrived (ms since the epoch).
    pub at: u64,
    /// The seconds the client asked the server to hold it.
    pub timeout: Option<f64>,
    /// 204, 200, 500 or `"silent"`; null while it is held.
    pub status: Value,
}

/// One download asked of CDN 3 (`downloadStats`).
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadStat {
    /// The object key.
    pub key: String,
    /// The request's `Range` header.
    pub range: Option<String>,
    /// The answer's status.
    pub status: u16,
    /// The first byte served.
    pub start: u64,
    /// Body bytes written.
    pub sent: u64,
    /// The transfer was reset part way (`breakDownloads`).
    pub broken: bool,
}

/// Arguments of `sendText`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendText {
    /// Sending primary (ref or ACI).
    pub from: String,
    /// Target device (ref, or ACI for device 1).
    pub to: String,
    /// Message body.
    pub text: String,
    /// Send as sealed sender.
    pub sealed: bool,
    /// Message timestamp in ms (default: now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
    /// Include the sender's profile key.
    pub with_profile_key: bool,
    /// Group id from [`GroupInfo::id`]: sets the group context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// `"PNI"`: address the target's PNI (a PreKey message from its PNI prekeys); default ACI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id_kind: Option<String>,
    /// Wait until the target acknowledged the envelope.
    pub await_delivery: bool,
    /// Bound for `await_delivery`, in ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

impl SendText {
    /// A plain, unsealed text from `from` to `to`.
    pub fn new(from: impl Into<String>, to: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            text: text.into(),
            ..Self::default()
        }
    }
}

/// Result of `waitForDistribution`: a sender-key distribution message sent on its own.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedDistribution {
    /// Ref of the sending device.
    pub source: String,
    /// It arrived as sealed sender.
    pub sealed: bool,
    /// Group id (base64) the sealed content named, if sealed with one.
    pub sealed_group_id: Option<String>,
    /// The sealed content's hint (`default`, `resendable`, `implicit`), if sealed.
    pub content_hint: Option<String>,
}

/// One attachment of `sendAttachments`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingFile {
    /// The plaintext (base64).
    pub data: String,
    /// Content type (default `application/octet-stream`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// File name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Width in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Pointer flags (1 voice note, 8 GIF).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<u32>,
    /// Blurhash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur_hash: Option<String>,
    /// Audio duration in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f32>,
    /// What a hostile sender puts into the pointer in place of the truth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge: Option<ForgedPointer>,
}

/// Pointer fields of an [`OutgoingFile`] that lie about the stored blob.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgedPointer {
    /// A digest that does not match the blob.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub digest: bool,
    /// The pointer's `size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
    /// The pointer's CDN number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_number: Option<u32>,
    /// The pointer's CDN key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdn_key: Option<String>,
}

impl OutgoingFile {
    /// `bytes` as `content_type`.
    pub fn new(bytes: &[u8], content_type: &str) -> Self {
        Self {
            data: BASE64.encode(bytes),
            content_type: Some(content_type.to_owned()),
            ..Self::default()
        }
    }
}

/// Arguments of `sendAttachments`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendAttachments {
    /// Sending primary (ref or ACI).
    pub from: String,
    /// Target device (ref, or ACI for device 1).
    pub to: String,
    /// The files.
    pub attachments: Vec<OutgoingFile>,
    /// Body; longer than 2048 bytes it goes out trimmed with a long-text attachment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The first file is the message's sticker.
    pub sticker: bool,
    /// Send as sealed sender.
    pub sealed: bool,
    /// Wait until the target acknowledged the envelope.
    pub await_delivery: bool,
    /// Bound for `await_delivery`, in ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Send it as an `EditMessage` for the message sent at this timestamp (the edit of a
    /// message with attachments).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_target: Option<u64>,
}

/// Result of `sendAttachments`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentAttachments {
    /// Timestamp of the sent message.
    pub timestamp: u64,
    /// Ref of the target device.
    pub to: String,
    /// The pointers as sent.
    pub pointers: Vec<Value>,
}

/// Result of `fetchAttachment`: the plaintext and what verified.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchedAttachment {
    /// The plaintext (base64), padding stripped to `size`.
    pub data: String,
    /// The pointer's size.
    pub size: u64,
    /// Length of the blob on the CDN.
    pub blob_length: u64,
    /// The HMAC matched.
    pub mac_valid: bool,
    /// The pointer's digest matched.
    pub digest_valid: bool,
    /// The incremental MAC matched (`None` without one).
    pub incremental_mac_valid: Option<bool>,
    /// The incremental MAC's chunk size.
    pub chunk_size: Option<u64>,
}

impl FetchedAttachment {
    /// The plaintext bytes.
    pub fn bytes(&self) -> Vec<u8> {
        BASE64.decode(&self.data).unwrap_or_default()
    }
}

/// Arguments of `sendTexts`: `count` texts `"<prefix> <i>"` with timestamps
/// `start_timestamp + i`, queued without waiting for delivery.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTexts {
    /// Sending primary (ref or ACI).
    pub from: String,
    /// Target device (ref, or ACI for device 1).
    pub to: String,
    /// How many (1..=10000).
    pub count: u32,
    /// Text prefix (default "message").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Send as sealed sender.
    pub sealed: bool,
    /// First timestamp (default: now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_timestamp: Option<u64>,
}

/// Result of `sendTexts`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentTexts {
    /// The timestamps, in send order.
    pub timestamps: Vec<u64>,
    /// Ref of the target device.
    pub to: String,
}

/// Result of `sendText`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    /// Timestamp of the sent message.
    pub timestamp: u64,
    /// Ref of the target device.
    pub to: String,
}

/// A client envelope to a scripted secondary device (`waitForEnvelope`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptedEnvelope {
    /// Ref of the scripted device.
    pub to: String,
    /// Ref of the sender, `None` when sealed.
    pub source: Option<String>,
    /// Envelope type (`SealedSender`, `PreKey`, `CipherText`, …).
    pub envelope_type: String,
    /// Envelope timestamp.
    pub timestamp: u64,
    /// Sent as sealed sender.
    pub sealed: bool,
}

/// A receipt a primary received (`waitForReceipt`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedReceipt {
    /// Ref of the sending device.
    pub source: String,
    /// `delivery`, `read` or `viewed`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Timestamps of the acknowledged messages.
    pub timestamps: Vec<u64>,
    /// Sent as sealed sender.
    pub sealed: bool,
    /// The group id (base64) the sealed content names; `None` when unsealed or none.
    #[serde(default)]
    pub sealed_group_id: Option<String>,
    /// The sealed content's hint (`default`, `resendable`, `implicit`); `None` when unsealed.
    #[serde(default)]
    pub content_hint: Option<String>,
    /// The Content carried a `PniSignatureMessage` beside the receipt.
    #[serde(default)]
    pub pni_signature: bool,
}

/// A typing indicator a primary received (`waitForTyping`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedTyping {
    /// Ref of the sending device.
    pub source: String,
    /// `started` or `stopped`.
    pub action: String,
    /// The indicator's timestamp.
    pub timestamp: u64,
    /// Group id, for group typing.
    pub group_id: Option<String>,
    /// Sent as sealed sender.
    pub sealed: bool,
}

/// A sync message a primary got from a linked device (`waitForSyncMessage`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedSync {
    /// Ref of the sending device.
    pub source: String,
    /// `sent`, `read`, `viewed` or the content field name.
    pub kind: String,
    /// The SyncMessage as JSON (bytes as base64, binary service ids included).
    pub sync_message: Value,
}

/// Result of `sendData`, `sendCall` and `sendSync`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentData {
    /// Timestamp of what was sent (the sender's own).
    pub timestamp: u64,
    /// What the envelope was stamped with (`sendData`, `sendCall`): the server's time, which
    /// is read from the clock after `timestamp` and can be a millisecond later.
    #[serde(default)]
    pub server_timestamp: Option<u64>,
    /// The pointers a backfill answer put in (`sendSync {attachmentBackfillResponse}`), as
    /// JSON, in entry order (long text last).
    #[serde(default)]
    pub pointers: Vec<Value>,
}

/// A request for lost files the phone got (`wait_for_backfill_request`).
#[derive(Debug, Clone)]
pub struct BackfillRequest {
    /// Ref of the sending device.
    pub source: String,
    /// The message's author as the binary service id (base64), when named so.
    pub author: Option<String>,
    /// The message's sent time.
    pub sent_timestamp: Option<u64>,
    /// `targetConversation` as JSON (`identifier.threadServiceIdBinary`, `threadGroupId`, ...).
    pub conversation: Value,
    /// The whole request as JSON.
    pub raw: Value,
}

/// The phone's answer to a request for lost files (`send_backfill_answer`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillAnswer {
    /// The chat: `{"aci": ..}`, `{"group": ..}` or `{"e164": ..}`.
    pub conversation: Value,
    /// The message's author (an ACI).
    pub author: String,
    /// The message's sent time.
    pub timestamp: u64,
    /// `"messageNotFound"` or a number: the whole answer is an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    /// One entry per file, in the phone's order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<BackfillEntry>>,
    /// The long text's slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_text: Option<BackfillEntry>,
    /// Neither a list nor an error.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub empty: bool,
}

/// One slot of a [`BackfillAnswer`], as the harness takes it.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct BackfillEntry(pub Value);

impl BackfillEntry {
    /// `PENDING`: still uploading.
    pub fn pending() -> Self {
        Self(json!({ "status": "pending" }))
    }

    /// `TERMINAL_ERROR`: the phone does not have this file.
    pub fn gone() -> Self {
        Self(json!({ "status": "terminalError" }))
    }

    /// An `AttachmentData` with neither field.
    pub fn empty() -> Self {
        Self(json!({ "empty": true }))
    }

    /// A pointer sent as it is (JSON, as `SentData::pointers` has it).
    pub fn pointer(pointer: &Value) -> Self {
        Self(json!({ "pointer": pointer }))
    }

    /// `data` uploaded now, its pointer named by `client_uuid` (as Android names a
    /// backfilled pointer).
    pub fn upload(data: &[u8], content_type: &str, client_uuid: Option<[u8; 16]>) -> Self {
        let mut entry = json!({ "data": BASE64.encode(data), "contentType": content_type });
        if let Some(uuid) = client_uuid {
            entry["clientUuid"] = json!(BASE64.encode(uuid));
        }
        Self(entry)
    }

    /// [`Self::upload`] with `forge` (`{"digest": "none"}`, `{"size": n}`, ...) merged in.
    pub fn forged(mut self, forge: Value) -> Self {
        self.0["forge"] = forge;
        self
    }
}

/// An edit a primary received and decrypted (`waitForEdit`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedEdit {
    /// Ref of the sending device (unsealed).
    pub source: String,
    /// The sent timestamp of the revision the edit replaces.
    pub target_timestamp: u64,
    /// The new revision's own timestamp.
    pub timestamp: u64,
    /// The new body.
    pub text: String,
    /// Whether it came as sealed sender.
    pub sealed: bool,
    /// Type of the (unsealed) ciphertext.
    pub message_type: String,
    /// Group id when the edit carries a known group context.
    pub group_id: Option<String>,
    /// Group id (base64) the sealed content named, if any.
    #[serde(default)]
    pub sealed_group_id: Option<String>,
    /// The new revision's whole DataMessage as JSON (bytes as base64).
    pub data_message: Value,
    /// Edits still buffered for this primary.
    pub remaining: u64,
}

/// A message a primary received and decrypted (`waitForMessage`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceivedMessage {
    /// Ref of the sending device (unsealed).
    pub source: String,
    /// ACI of the sender.
    pub source_aci: String,
    /// Device id of the sender.
    pub source_device: u32,
    /// Body text.
    pub text: String,
    /// DataMessage timestamp.
    pub timestamp: u64,
    /// Wire envelope type (`SealedSender` when sealed).
    pub envelope_type: String,
    /// Type of the (unsealed) ciphertext: `PreKey`, `CipherText`, `SenderKey`, `Plaintext`.
    pub message_type: String,
    /// What it is: `message`, `reaction`, `delete`, `adminDelete` or `timer` (an update).
    #[serde(default)]
    pub kind: String,
    /// Whether it came as sealed sender.
    pub sealed: bool,
    /// Which identity of the receiver it was addressed to (`ACI` or `PNI`).
    pub service_id_kind: String,
    /// Group id when the message carries a known group context.
    pub group_id: Option<String>,
    /// Group id (base64) the sealed content named (`UnidentifiedSenderMessageContent`), if
    /// sealed with one.
    #[serde(default)]
    pub sealed_group_id: Option<String>,
    /// The Content also carried a sender-key distribution message.
    #[serde(default)]
    pub sender_key_distribution: bool,
    /// The whole DataMessage as JSON (bytes as base64).
    pub data_message: Value,
    /// Messages still buffered for this primary.
    pub remaining: u64,
}
