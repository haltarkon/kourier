//! [`Session`]: the actor's view of ONE WhatsApp client (W2, chunk C3).
//!
//! The login machine ([`crate::link`]) and the actor (C4) never touch whatsapp-rust's `Client`
//! directly; they call this trait. Two implementations exist: `RealSession` (`session::client`, over a bot
//! that runs on the account's own runtime) and the test double [`fake::FakeSession`]. Every type in the
//! signatures is Kourier's own, so the machine and its tests name nothing of upstream and a pin
//! bump cannot move them (`translate.rs` and `RealSession` are the two files that do).
//!
//! What a call means (the contract `RealSession` keeps and the double records):
//!
//! - Every method is an `async fn` whose work runs as a task on the account's client runtime and is
//!   awaited by the caller (critic H1: upstream's calls spawn internally, and a spawn from the
//!   actor's current-thread runtime would starve whenever the actor is not polling).
//! - None of them logs a number, a code or a JID.
//! - `logout` never fails as an error: how far it got is the answer ([`DeviceLogout`]). It is also
//!   upstream's TERMINAL stop of the client: after it a new client is needed for anything else.
//! - `stop` is the shutdown order (`store_release`, `shutdown`, drop, wait for the release,
//!   bounded): it returns whether the store was released in time; a timeout is logged by the
//!   caller and never turns a finished logout into a failure (critic M9).
//!
//! THE SEND HALF (W5a, chunk 7). [`Session::send`] is the ONE way a message, an edit, a revoke or a
//! reaction leaves: `RealSession` makes it the crate's one `Client::send` call, with the core's local
//! as the stanza id (decision D2; `tests/builder.rs` holds the single call site). [`Outgoing`] carries
//! the chat and the message exactly as `send::content` built them (the proto and the `Jid` are the two
//! upstream types the send half passes through untouched; nothing outside `send::content` and
//! `RealSession` reads them). `Ok` from `send` means WRITTEN to the socket, never "the server took
//! it": the server's ack comes as a signal (chunk 8). A [`SendFailure`] is upstream's typed pre-wire
//! error, read by position; what it means for the core is `send::error_of`'s (decision D6).
//! `mark_read` and `typing` are upstream's `mark_as_read`/`mark_as_played` and `chatstate().send`;
//! `lid_migrated` and `group_mode` are the inputs of `send::address::Addressing` (decision D20).
//!
//! THE GROUP AND BLOCKLIST HALF (W6a2, chunk B3; decisions D9, D22, M1). [`Session::participating`],
//! [`Session::fetch_groups`], [`Session::fetch_group`] (the single query of a group, B6) and [`Session::blocklist`] are the
//! ONLY reads of the groups and the blocklist: one IQ each,
//! never retried here, never a fallback to per-group queries (a failed batch is ONE failed IQ for every group in it:
//! scout T4). Each is the crate's one call of upstream's `list_participating`, `fetch_metadata_batch`, `fetch_metadata` and
//! `get_blocklist` (`tests/builder.rs` holds the call sites, and that `add_lid_pn_mapping` is called nowhere). The
//! caller owns WHEN: after `Online` only, under the fetch bucket (B6), once per start (the blocklist). A failure is an
//! [`AskFailure`]: upstream's TYPED error by position, never its text (a `GroupError` or `BlockingError` carries the
//! server's words and the whole rejection stanza, L5).
//!
//! THE UPLOAD HALF (W5b, brief §2.2, decisions G14, G17). [`Session::upload`] hands a prepared file (WhatsApp's
//! ciphertext in the sealed temporary file, read through [`SealedSource`]) to upstream's `upload_stream`: the `w:m`
//! IQ on the chat socket, the hosts in order, the one forced refetch after a 401/403, the resume check from 5 MiB.
//! `RealSession` runs it ONLY as a task on the client's runtime that is ABORTED when the caller drops the future
//! (a stall, a stop, a quit: the attempt's future goes and the task with it; a blocking POST still running ends at
//! its next read of the source, which fails once the attempt is gone). The answer is Kourier's: [`Uploaded`] (the
//! CDN's pointer, never printed) or an [`UploadFailure`] read from upstream's error chain BY TYPE (the HTTP status
//! through `ErrorChainExt::http_status`), never by formatting it: the chain carries the CDN's answer body and can
//! carry the URL with its `auth`. A session replaced mid-upload answers `NoClient` (G17); a POST that finished on
//! the CDN before is an orphan there.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kourier_backend_api::Clock;
use kourier_model::{BackendError, DeviceLogout};
use wacore::msg_secret::MsgSecretPolicy;
use wacore::net::{HttpClient, TransportFactory};
use wacore::types::events::EventHandler;
use wacore_binary::OwnedNodeRef;
use whatsapp_rust::bot::{Bot, BotBuilder, Provided};
use whatsapp_rust::{
    CacheConfig, GroupMetadataResult, Interception, InterceptorHandle, PresencePolicy,
    StanzaInterceptor, TokioRuntime,
};

use crate::appstate::ResyncReport;
use crate::calls::CallInterceptor;
use crate::events::EventWriter;
use crate::history::capture::{Capture, CaptureHook};
use crate::hook::Hook;
use crate::inbox::Inbox;
use crate::prepair::PrePair;
use crate::props;
use crate::send::route::Shape;
use crate::send::upload::SealedSource;
use crate::stage2::Stage2;
use crate::store::KourierStore;
use crate::tap::Tap;
use crate::translate::HoldState;

mod client;
pub(crate) use client::RELEASE_WAIT;

pub(crate) use client::RealFactory;

/// The builder of the client once all four slots are filled (what a fixture's step receives).
pub type Builder = BotBuilder<Provided, Provided, Provided, Provided>;

/// Why a pair-code request did not produce a code. The typed values of upstream's `PairError`
/// (and of the `PairingCodeError` event, which only repeats them), without an upstream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairCodeFailure {
    /// Local validation refused the number (empty, shorter than 7 digits, starts with `0`). Kourier
    /// checks all three before the call; this is upstream's own check disagreeing.
    InvalidNumber,
    /// A code is already on screen and still valid. Upstream says this is no failure.
    AlreadyOutstanding,
    /// The request was withdrawn (`cancel_pair_code`) while it ran. Not a failure either.
    Cancelled,
    /// The server refused. `code` is the wire code (400, 403, 429, 452, 500, ...), `backoff` the
    /// delay the server named, when it did.
    Refused {
        code: i32,
        backoff: Option<Duration>,
    },
    /// No answer: not connected, a timeout, a transport failure, or a failure with no typed status.
    NoAnswer,
}

/// What `stop` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Released {
    /// The store was released by the client in time.
    Yes,
    /// The release did not come within the bound (a task still holds the store).
    TimedOut,
}

/// Whether a send asks the recipient's device list again first (upstream's `device_freshness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Devices {
    /// The cached list (upstream's default).
    #[default]
    Cached,
    /// The list asked again: the one retry after [`SendFailure::NoRecipientDevice`] (brief §2.3).
    Refresh,
}

/// Whether a send asks for the group's metadata again first (upstream's `group_metadata_freshness`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Groups {
    /// The cached metadata (upstream's default).
    #[default]
    Cached,
    /// The metadata asked again: the one frame after a 421 (the group's addressing was stale; fix F1).
    Refresh,
}

/// One frame to write: what `send::content` built for one request, under the core's local. No `Debug`:
/// it holds the chat and the text, which no log line may carry.
#[derive(Clone, PartialEq)]
pub(crate) struct Outgoing {
    /// The chat (`send::address::jid_of` of the request's conversation, nothing else: D6 item (4)).
    pub(crate) to: wacore_binary::Jid,
    /// The core's local: the stanza id (decision D2).
    pub(crate) id: String,
    /// The message, as built.
    pub(crate) message: waproto::whatsapp::Message,
    /// The chat's timer in seconds (`SendOptions::with_ephemeral_expiration`).
    pub(crate) expiration: Option<u32>,
    pub(crate) devices: Devices,
    /// Fresh group metadata for the frame (fix F1: the first frame after a 421's floor).
    pub(crate) groups: Groups,
}

/// Why a frame (or a receipt, a typing state) was NOT written: upstream's typed errors by position,
/// never their text. Log-safe (no id, no code but the IQ's status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendFailure {
    /// No client runs (not started, stopped, or its runtime is gone).
    NoClient,
    /// The socket is not connected.
    NotConnected,
    /// The client has no identity yet (not paired, mid LID migration).
    NotLoggedIn,
    /// The socket failed while the frame was written: it may have left (the connection ends with it).
    Socket,
    /// An IQ of the send (usync, prekeys, group info) got no answer in time.
    IqTimeout,
    /// An IQ of the send was answered with an error status.
    IqRefused { code: u16 },
    /// No device of the recipient could be encrypted for: nothing was sent.
    NoRecipientDevice,
    /// The server refused the recipient's primary device while the sessions were set up: nothing sent.
    PrimaryDeviceRejected,
    /// The request is invalid for upstream (an id it does not take, a recipient it refuses).
    InvalidRequest,
    /// Anything else upstream reports (encryption, protobuf, an internal channel).
    Internal,
}

/// What the CDN answered for an upload (W5b): where the file is. No `Clone`, and a `Debug` that prints only the time:
/// the url and the direct path are secrets of the message (they reach no log line).
pub(crate) struct Uploaded {
    /// The file's URL (with its own query; never logged).
    pub(crate) url: String,
    /// The file's path on the CDN (the message's `directPath`; never logged).
    pub(crate) direct_path: String,
    /// When the media key was made, in seconds (the message's `mediaKeyTimestamp`).
    pub(crate) media_key_timestamp: i64,
}

impl std::fmt::Debug for Uploaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uploaded")
            .field("media_key_timestamp", &self.media_key_timestamp)
            .finish_non_exhaustive()
    }
}

/// Why an upload did not finish, read from upstream's error BY TYPE (never its text). Log-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadFailure {
    /// No client runs (not started, stopped, or replaced while the upload ran: G17).
    NoClient,
    /// The media server answered this HTTP status (the last host's, after upstream's one refetch of the
    /// `media_conn` on a 401/403).
    Status(u16),
    /// No HTTP exchange finished: the `w:m` IQ failed or timed out, the connection or the client failed, a read
    /// of the source failed (the stop), an answer did not read.
    Incomplete,
}

/// What a lookup of a profile picture gave (W8). Its `Debug` holds no URL and no bytes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PictureAnswer {
    /// The chat has this picture: the server's id of it and the bytes of its small preview.
    Found { id: String, bytes: Vec<u8> },
    /// Still the picture of the id that was named.
    Unchanged,
    /// No picture, or its owner's privacy hides it from this account.
    None,
}

impl std::fmt::Debug for PictureAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found { bytes, .. } => write!(f, "Found({} bytes)", bytes.len()),
            Self::Unchanged => f.write_str("Unchanged"),
            Self::None => f.write_str("None"),
        }
    }
}

/// Why a file did not download (W7), by upstream's typed error (never its text: a cause may hold a signed URL). Log-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadFailure {
    /// No client runs (not started, stopped, or replaced while the download ran).
    NoClient,
    /// The media servers no longer have the file under this reference, after a fresh `media_conn`.
    Gone,
    /// No host gave a file that verified, or no route could be had: worth trying again.
    Unreachable,
    /// The destination could not be written (a full disk, a file larger than it said).
    Sink,
    /// The pointer cannot be made a request.
    Invalid,
}

/// Why a read of the groups or of the blocklist got no answer, read from upstream's typed error BY POSITION (L5: a
/// `GroupError` or `BlockingError` carries the server's text and the whole rejection stanza, so it is never formatted).
/// Log-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskFailure {
    /// No client runs (not started, stopped, or its runtime is gone).
    NoClient,
    /// The socket is not connected (or went away while the IQ waited): the ask may be made again on the next
    /// connection.
    NotConnected,
    /// The IQ got no answer in time: the same.
    Timeout,
    /// The server answered with an error status: `code` is the wire code, `backoff` the delay the server named (seconds
    /// on the wire).
    Refused {
        code: u16,
        backoff: Option<Duration>,
    },
    /// The answer did not read (one malformed entry fails a whole batch, scout T4): asking again gets the same.
    Unreadable,
    /// Anything else upstream reports.
    Internal,
}

impl AskFailure {
    /// The failure's kind as a log-safe word (never a text of the server's or upstream's).
    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::NoClient => "no-client",
            Self::NotConnected => "not-connected",
            Self::Timeout => "timeout",
            Self::Refused { .. } => "refused",
            Self::Unreadable => "unreadable",
            Self::Internal => "internal",
        }
    }

    /// Whether the ask never reached the server or got no answer in time: the next connection may ask again. Every other
    /// failure is an answer (or an unreadable one) and is final for the start.
    pub(crate) fn is_later(self) -> bool {
        matches!(self, Self::NoClient | Self::NotConnected | Self::Timeout)
    }
}

/// Which receipt [`Session::mark_read`] sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadKind {
    /// `read` (upstream turns a DM's into `read-self` when the phone's setting says so).
    Read,
    /// `played` (a voice note or a view-once item was opened).
    Played,
}

/// The client of ONE account, as the actor drives it.
#[async_trait]
pub(crate) trait Session: Send + Sync {
    /// Builds the client and starts it on the account's runtime (it connects by itself). Not called
    /// before the first `Connect` or `StartLogin` (decision L2/M4). The error is log-safe and says
    /// why nothing was started (a proxy is set, the environment is not Production, ...).
    async fn start(&self) -> Result<(), BackendError>;

    /// Stops reconnecting and drops the connection, keeping the client (upstream's `pause`):
    /// `Command::Disconnect`.
    async fn pause(&self);

    /// Undoes [`pause`](Self::pause) (upstream's `resume`): `Command::Connect` after a pause.
    async fn resume(&self);

    /// Asks the server for a pair code for `phone_number` (digits with the country code, no `+`, no
    /// leading `0`). The code itself arrives as a signal, not as the return value. Kourier's own
    /// validation has run; upstream's must agree.
    async fn pair_with_code(&self, phone_number: &str) -> Result<(), PairCodeFailure>;

    /// Withdraws the outstanding pair-code request, if any.
    async fn cancel_pair_code(&self);

    /// Removes this companion from the account (upstream's remove-companion request if connected,
    /// then a terminal stop). `Removed` when the server confirmed, `NotReached` otherwise.
    async fn logout(&self) -> DeviceLogout;

    /// Reconnects at once if the client is not online (`NetworkChanged`, `Resumed`).
    async fn reconnect_now(&self);

    /// Ends the live connection because a written frame heard no ack within `ACK_WAIT` (W5a, D5; whatsmeow
    /// ends it at its `ErrMessageTimedOut`): upstream's `reconnect`, which closes the socket and comes back on its
    /// own backoff (5 s at least). Returns when the socket is closed; the actor and the drain's boundary hear it as
    /// an unclean end of the connection. Nothing when no client runs.
    async fn drop_connection(&self);

    /// Asks the server for the reach-out lock now (the pull after a connect). `Err` when the answer
    /// did not come; the caller retries and states nothing until it does (critic M1).
    async fn fetch_hold(&self) -> Result<HoldState, BackendError>;

    /// The start-up resync (W6a1, `crate::appstate::startup`): asks the server for the snapshot of `collections` (the
    /// stored names) and says which the report calls synced and which fatal. `Err` only when the request never reached
    /// the server. `RealSession` runs it as a task on the client's runtime and awaits it there: the replay's events
    /// are handed to the handler on the thread that polls the call, and the handler parks that thread until each
    /// zone committed, so it must never be the actor's. The call is not given a deadline (dropping it mid-apply would
    /// leave a collection needing another sync); the caller bounds how long it WAITS.
    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError>;

    /// The shutdown order: release the store, shut the client down, drop it, wait for the release
    /// (bounded to 5 s).
    async fn stop(&self) -> Released;

    /// Waits up to `limit` for the release a [`stop`](Self::stop) that returned `TimedOut` did not see
    /// (`Duration::ZERO` only looks). `Yes` when nothing is pending, or the release came: the store has
    /// no user left from this client and a new one may be made (fix F9: never two clients on one store).
    async fn release_wait(&self, limit: Duration) -> Released;

    /// Writes ONE frame: `Client::send` with `out.id` as the stanza id. `Ok` = written to the socket
    /// (not acked). Never retried here: the caller decides (D5, D6).
    async fn send(&self, out: Outgoing) -> Result<(), SendFailure>;

    /// Uploads a prepared file (W5b): upstream's `upload_stream` over `source` with `info`, under the upload type
    /// of `shape` (the same value the file was encrypted under: `Shape::media_type`). Dropping the future aborts
    /// the upload's task (see the module doc). Never retried here: the caller decides.
    async fn upload(
        &self,
        source: SealedSource,
        info: wacore::upload::EncryptedMediaInfo,
        shape: Shape,
    ) -> Result<Uploaded, UploadFailure>;

    /// Fetches, verifies and decrypts the file `params` names into `sink` (W7): upstream's `download_to_writer`, with
    /// its host failover and its one fresh `media_conn` after a rejection. Dropping the future aborts the download's task.
    async fn download(
        &self,
        params: whatsapp_rust::download::DownloadParams,
        sink: crate::download::BlobSink,
    ) -> Result<crate::download::BlobSink, DownloadFailure>;

    /// Looks up the small preview of the picture of `chat` (a person, or a group when `group`), naming the id of the one
    /// already held (W8): one IQ, and a GET of the picture when it is a new one.
    async fn profile_picture(
        &self,
        chat: wacore_binary::Jid,
        group: bool,
        known: Option<String>,
    ) -> Result<PictureAnswer, AskFailure>;

    /// Sends a `read` or `played` receipt for `ids` in `chat` (`sender`: the author in a group).
    async fn mark_read(
        &self,
        chat: wacore_binary::Jid,
        sender: Option<wacore_binary::Jid>,
        ids: Vec<String>,
        kind: ReadKind,
    ) -> Result<(), SendFailure>;

    /// Sends the typing state of `chat` (`on`: composing, else paused).
    async fn typing(&self, chat: wacore_binary::Jid, on: bool) -> Result<(), SendFailure>;

    /// Makes sure the client holds THIS connection's read-receipts setting before a 1:1 `read`/`played` receipt
    /// is written (fix F5): upstream decides `read` or `read-self` by the setting its background init fetches after
    /// the login, in parallel with other work, and a receipt that goes out before that answer says `read` on a
    /// phone that turned read receipts off. `Ok` when the setting is the client's own now (asked from the server
    /// and applied exactly as upstream's background init applies it); `Err` when it could not be asked (nothing
    /// is written then: the caller keeps the receipt for the next connection). Bounded by upstream's IQ timeout.
    async fn privacy_known(&self) -> Result<(), SendFailure>;

    /// Whether the account is 1:1 LID-migrated (upstream's `is_lid_migrated`); `false` without a client.
    async fn lid_migrated(&self) -> bool;

    /// The addressing mode of a group (upstream's cached routing view, the one its own send reads).
    async fn group_mode(
        &self,
        group: wacore_binary::Jid,
    ) -> Result<crate::send::address::Namespace, SendFailure>;

    /// The id of every group the account is in: ONE IQ (`participating`, the overview projection: ids, titles, hierarchy,
    /// counts, no members; only the ids are kept, the batch that follows gives the rest, and the upstream overview type has
    /// no constructor a test could use). W6a2 D9: the first half of the start's two IQs, after `Online`.
    async fn participating(&self) -> Result<Vec<wacore_binary::Jid>, AskFailure>;

    /// The whole metadata of `groups`: ONE batch IQ (`fetch_metadata_batch`, never more than the caller's bucket
    /// allows), answered per group in the server's order: `Found` (with the members' numbers filled from the store's
    /// pairs, no IQ), `Truncated` (no member list: never an empty one), `Forbidden`, `NotFound`. A group asked and not
    /// answered has NO entry (scout T3). `Err` is the whole IQ failing: no partial result, no retry, no fallback to
    /// single queries (scout T4). An empty list asks nothing.
    async fn fetch_groups(
        &self,
        groups: Vec<wacore_binary::Jid>,
    ) -> Result<Vec<GroupMetadataResult>, AskFailure>;

    /// The whole metadata of ONE group: one single-query IQ (`fetch_metadata`, W6a2 B6), for a group the batch answered
    /// `Truncated` and for a group a notice marked for a fetch. The server's 403 and 404 are ANSWERS here, as in the
    /// batch: `Forbidden` and `NotFound` (we are not in the group), so `groups::map` reads one result type. `Err` is the
    /// IQ failing (no retry here; the fetch policy's bucket decides about the next).
    async fn fetch_group(
        &self,
        group: wacore_binary::Jid,
    ) -> Result<GroupMetadataResult, AskFailure>;

    /// The invite link of a group, whole (`https://chat.whatsapp.com/<code>`); `reset` replaces
    /// it by a new one first. The server answers an administrator only. The link is never a log field.
    async fn group_invite_link(
        &self,
        group: wacore_binary::Jid,
        reset: bool,
    ) -> Result<String, AskFailure>;

    /// The phone's blocklist (W6a2 D22): ONE IQ, the people it names in the namespace it wrote them (a number or a LID).
    async fn blocklist(&self) -> Result<Vec<wacore_binary::Jid>, AskFailure>;
}

/// The receive path a client is wired to (W3a). The actor makes these once per account and hands the
/// same ones to every client it makes, so a new client after a failed write meets the same inbox,
/// the same group-commit buffer and the same stage 2.
#[derive(Clone)]
pub(crate) struct Receive {
    /// Stage 1: what the durability hook commits, and what the handler queues for the at-most-once
    /// kinds.
    pub(crate) inbox: Inbox,
    /// The handler's way to a durable `core_outbox` row (rule (a)).
    pub(crate) writer: EventWriter,
    /// Stage 2: woken by the hook's commits, holds the hook while its backlog is full.
    pub(crate) stage2: Arc<Stage2>,
    /// The history capture (W4): the account's stage file and the driver's wake. One per account, so a
    /// client made again meets the same stage and a chunk captured twice is one row.
    pub(crate) capture: Capture,
    /// The server's acks of our message stanzas (W5a, `send::acks`): one registry per account, so a send
    /// waits in the same place whichever client wrote its frame, and the handler of every client answers it.
    pub(crate) acks: Arc<crate::send::acks::Acks>,
}

/// What the actor gives the client it makes.
pub(crate) struct Wiring {
    /// Where the client's signals go (the event handler of the real client calls it).
    pub(crate) tap: Tap,
    /// The account's store (a handle: the actor keeps the owner).
    pub(crate) store: KourierStore,
    /// The actor's clock: the supervisor's waits and the times it states are on it.
    pub(crate) clock: Arc<dyn Clock>,
    /// The receive path.
    pub(crate) receive: Receive,
}

/// How long upstream keeps the payload of a message we sent, for retry receipts: its own default,
/// stated explicitly so that a pin bump that changes the default is seen (decision D7; the test
/// that pins it is a later chunk's).
pub(crate) const SENT_MESSAGE_TTL_SECS: u64 = 7200;

/// How long upstream keeps a message's secret (the key a poll vote, an event reply or a bot reply
/// is decrypted with): as long as the message itself (decision D7, fix F6), so it never prunes
/// (`Full`; upstream's default `Managed` drops them after 30 days, polls and events after 90). The
/// store keeps growing by one row per message that carries a secret; the cost is the price of
/// decrypting a vote on a poll of any age (W4/W5/K8a).
pub(crate) const MSG_SECRET_POLICY: MsgSecretPolicy = MsgSecretPolicy::Full;

/// The cache configuration of the client: upstream's, with the two values Kourier states.
#[allow(
    clippy::field_reassign_with_default,
    reason = "CacheConfig is non_exhaustive: a struct literal cannot name its fields"
)]
pub(crate) fn cache_config() -> CacheConfig {
    let mut config = CacheConfig::default();
    config.sent_message_ttl_secs = SENT_MESSAGE_TTL_SECS;
    config.msg_secret_policy = MSG_SECRET_POLICY;
    config
}

/// The two hooks upstream waits for before it answers anybody, registered by [`build_bot`] and nowhere else.
pub(crate) struct Hooks {
    /// Commits what a message batch holds before the server is told it arrived (W3a).
    pub(crate) durability: Hook,
    /// Keeps a history chunk in the stage before the phone is told it arrived (W4).
    pub(crate) capture: CaptureHook,
}

/// Makes the [`Session`] of ONE client. The actor calls it on its own thread at the first
/// `Connect` or `StartLogin`, and once more for each client it makes again (a terminal verdict
/// needs a new client, and only after the old one released the store: never two clients on one
/// store).
pub(crate) trait SessionFactory: Send + 'static {
    fn make(&mut self, wiring: Wiring) -> Arc<dyn Session>;

    /// What the phone lists this device as (`Kourier (<host>)`), from the host name the application
    /// gave: the same text the client is built with and the logout report names to the person.
    fn device_name(&self) -> String;
}

/// THE builder of the real client (the only text of the crate that builds one: the source check
/// in `tests/builder.rs` lists `src/session.rs` alone). Both slots come from the factory's parts,
/// so a test hands in guards and a build without them does not compile (whatsapp-rust is built
/// without a default transport or HTTP client).
///
/// The durability hook is registered here, once, and nowhere else: upstream's `build()` probes the
/// store with it (a gap in the store contract shows at the first build). So is the history capture
/// hook (W4): without it upstream would answer the phone for a chunk before anything is kept. The event handler is the
/// ONE raw handler (`with_event_handler`); no `on_event` or `on_message` closure is registered
/// anywhere (their delivery policies drop events, and the source check in `tests/builder.rs` refuses
/// them).
///
/// `host_name` is the application's (`NetParts::with_host_name`), the one source of the name.
///
/// What the phone is told is [`props::device_props`], set on EVERY build: upstream reads it at
/// pairing only, so a renamed host keeps the old name on the phone, and a start must not leave
/// upstream's default (`os: "rust"`) in the store. Presence is Kourier's to announce
/// (`Manual`): nothing is announced as available in W2, so the phone keeps notifying. The event
/// handler is a raw one (see `session::client`).
pub(crate) async fn build_bot(
    store: KourierStore,
    transport: Arc<dyn TransportFactory>,
    http: Arc<dyn HttpClient>,
    handler: impl EventHandler + 'static,
    hooks: Hooks,
    hook: Option<crate::parts::Hook>,
    host_name: &str,
) -> Result<Bot, whatsapp_rust::bot::BotBuilderError> {
    let builder = Bot::builder()
        .with_backend(store)
        .with_transport_factory_arc(transport)
        .with_http_client_arc(http)
        .with_runtime(TokioRuntime)
        .with_device_props(props::device_props(host_name))
        .with_presence_policy(PresencePolicy::Manual)
        .with_inbound_durability_hook(hooks.durability)
        .with_history_sync_capture_hook(hooks.capture)
        .with_cache_config(cache_config())
        .with_event_handler(handler);
    // Only the scripted fixture of the tests sets a step (see `NetParts::with_builder_hook`).
    let builder = match hook {
        Some(hook) => hook(builder),
        None => builder,
    };
    builder.build().await
}

/// The one stanza interceptor of a client: the pairing's (W2c, `prepair`: a pairing that fails the rules is
/// refused before its device exists) and the call's (W3b, `calls`: calls are shown, never taken), routed by the
/// stanza's tag. One registration, so there is one handle to keep and one place to get the order right.
pub(crate) struct Stanzas {
    pub(crate) pairing: PrePair,
    pub(crate) calls: CallInterceptor,
}

impl StanzaInterceptor for Stanzas {
    fn intercept(&self, node: &OwnedNodeRef) -> Interception {
        match &*node.get().tag {
            "iq" => self.pairing.intercept(node),
            "call" => self.calls.intercept(node),
            _ => Interception::Pass,
        }
    }
}

/// Registers the stanza interceptor on the client `bot` was built with, BEFORE the client starts, so the
/// calls of the offline queue are claimed too and no pairing can reach upstream's own handler unjudged (W2c,
/// W3b). The ONE place that registers an interceptor (the source check in `tests/builder.rs` refuses the name
/// anywhere else). The returned handle must live as long as the client: dropping it un-registers the
/// interceptor, and upstream's own handlers would answer the next offer and complete the next pairing.
pub(crate) fn register_stanzas(bot: &Bot, stanzas: Stanzas) -> InterceptorHandle {
    bot.client().add_stanza_interceptor(Arc::new(stanzas))
}

#[cfg(test)]
pub(crate) mod fake;
