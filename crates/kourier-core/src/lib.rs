//! The protocol-agnostic core (architecture §2.2, §4, §5): `core.db`, the account registry and
//! supervisor, idempotent ingest, and the UI [`facade`].
//!
//! The composition root builds a [`CoreConfig`] (core.db key, compiled-in networks, an
//! [`AccountVault`] for per-account keys, a [`Clock`](kourier_backend_api::Clock)) and calls
//! [`Core::start`]. The UI talks to the returned [`facade::CoreHandle`] only. The core never names
//! a backend, the app, Qt or D-Bus.
//!
//! - Writes run on one writer thread (`kourier_db::Writer`): live envelopes and UI commands
//!   before history chunks, round-robin across accounts; reads on two read-only connections
//!   (plus one for media lookups), never on a tokio worker.
//! - Each account has a supervisor task: it spawns the backend actor, applies every `Envelope` in
//!   one `core.db` transaction and only then completes its `Ack`, restarts crashed actors with
//!   capped exponential backoff, and runs login, unlink, relink and removal (§4.3).
//! - Ingest is idempotent and converges for any arrival order (§4.9): see [`testing`] for the
//!   harness the property tests use.
//! - The outbox (§4.8): `SendText` stores the message and its outbox row with the network id
//!   allocated once; each account's supervisor transmits the heads of the per-conversation FIFOs
//!   while the account is `Online`, with Signal's backoff inside a one-day budget, `Partial`
//!   resends to the failed members only, identity and challenge pauses (a challenge ends with
//!   the user's answer, or by itself at the time the network gave), and resumes interrupted
//!   sends with unchanged ids after a restart.
//! - Edits, deletes and reactions (§3.5.1-3.5.3): validated against the conversation's
//!   features (edit window and count, delete window, per-user reaction limit), applied locally at
//!   once (revision, tombstone with the files deleted, reaction row), sent through the same
//!   per-conversation FIFOs with their own network ids, and rolled back when the network refuses
//!   them for good.
//! - What the user may do with a message ([`facade::MessageActions`]) is decided by one set of
//!   rules, used for the flags on a timeline row and by the commands themselves.
//! - Disappearing messages: timers start per `Expiry` kind (at the send, or at reading), and a
//!   sweeper on the injected clock deletes expired rows and their files on time, also after a
//!   restart. The keys of expired messages are remembered for [`EXPIRED_MESSAGE_MEMORY`], so a
//!   late copy does not bring one back. What happens to a message that is due by this
//!   computer's clock while no backend has confirmed that clock (a start without a network,
//!   a clock a backend found wrong) is the user's choice, [`ExpiryPolicy`]: hidden until the
//!   time is confirmed (the default), kept until then, or erased by the local clock.
//! - Calls are shown, not taken: a backend reports every signal of a call as a message whose
//!   body is a call line, and ingest joins them into one row in any order
//!   (`kourier_model::call`). The id of a call is chosen by the other side: a line is found
//!   by its conversation and its id on every network, and the other side may only say that
//!   an incoming call was started, rings or was missed (answered, declined and everything
//!   outgoing come from our own devices). A ringing line has a durable deadline on this
//!   computer's clock, two minutes after the signal's receipt at most; a task on the injected
//!   clock turns one that is past it into a missed call, also after a restart or a resume. A
//!   missed incoming call (and a group call started without a ring) is unread and notifies
//!   like a message until the line moves on or is read; one that the clock made missed late,
//!   or before the account caught up, is announced with the catch-up summary, if it is still
//!   missed then. A live line takes the chat's disappearing timer like a message read at
//!   once. Nothing of a line is ever sent.
//! - Reconnect hints (`NetworkChanged`, `Resumed`) are collected for [`HINT_SETTLE`] and reach
//!   each linked backend as one command; nothing of the outbox is sent before the account is
//!   linked, so a relink to another account sends nothing.
//! - A delete for me can carry more than single messages: a conversation's history cleared up
//!   to a point, the chat itself deleted (it is hidden from the list until it is used again;
//!   its row, title and features stay), and one attachment of a message deleted. Rows and
//!   files go, the files after the commit. Made on another own device it arrives as an event
//!   and waits for messages that are not here yet; made here (`ClearConversation`,
//!   `DeleteConversation`, `DeleteAttachment`) it is applied at once, in one transaction with
//!   the durable command that tells the other devices. What a clear removed is remembered
//!   like expired messages, so a late copy does not bring it back.
//! - A receipt counts only from someone its message went to; from anyone else it waits
//!   without effect until its sender turns out to be a recipient (an identity merge, a
//!   membership update, the answer of the send), or two days.
//! - Refusals of user actions carry a [`Refusal`] the UI maps to its own words
//!   ([`CoreError::refusal`]).
//! - Attachments (§5.8): the auto-download policy issues `DownloadAttachment` with fresh blob
//!   targets, `AttachmentDone` records the file, thumbnails and blurhashes are derived after the
//!   download, `SendFiles` imports any number of files into the blob store (reporting its
//!   progress under the request's id) and queues them as the messages the network's packing
//!   rule needs, by each file's kind and format (one transaction, announced with
//!   `FilesStored`); a picture or video can be sent as the file it is; plaintext playback
//!   files live in the tmpfs media cache, and the media handle table
//!   ([`facade::MediaHandles`]) serves the UI's image provider without a database.

pub mod error;
pub mod facade;
pub mod testing;
pub mod vault;

mod actions;
mod attachments;
mod calls;
mod dispatch;
mod expiry;
mod ingest;
mod local;
mod media;
mod modify;
mod outbox;
mod query;
mod runtime;
mod sql;
mod store;
mod supervisor;

pub use attachments::derive::FAILPOINT_BEFORE_DERIVE_COMMIT;
pub use error::{CoreError, Refusal, Result};
pub use expiry::{ExpiryPolicy, REMEMBER as EXPIRED_MESSAGE_MEMORY};
pub use ingest::FAILPOINT_BEFORE_COMMIT;
pub use outbox::{
    BUDGET as OUTBOX_BUDGET, ENQUEUE_CHUNK, FAILPOINT_AFTER_ATTEMPT,
    FAILPOINT_AFTER_CHALLENGE_ANSWER, FAILPOINT_AFTER_ENQUEUE, FAILPOINT_AFTER_OUTCOME,
    FAILPOINT_AFTER_STAGED, FAILPOINT_BEFORE_OUTCOME_COMMIT, FAILPOINT_IN_ENQUEUE,
    MAX_BACKOFF as OUTBOX_MAX_BACKOFF, backoff as outbox_backoff,
};
pub use runtime::{Core, CoreConfig};
pub use store::{HISTORY_CHUNK, MIGRATIONS, UI_READERS};
pub use supervisor::{
    FAILPOINT_AFTER_PENDING_COMMAND, FAILPOINT_BEFORE_ACK, FAILPOINT_BEFORE_DOWNLOAD_SEND,
    HINT_SETTLE, PERF_TARGET, RestartPolicy, Timeouts,
};
pub use vault::{AccountVault, VaultEntry, VaultError, VaultFuture};
