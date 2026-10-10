//! `FakeNetwork`: a scripted backend for core, UI and screenshot tests (architecture §10.2).
//!
//! It implements `kourier_backend_api::Network` like a real backend: each account actor owns a
//! real `backend.db` (SQLCipher) with a stage-1 inbox, mirrored state and `core_outbox`, commits
//! the events it implies in the same transaction as its state change, and ships them through
//! `kourier_db::Forwarder`, deleting rows only after the core's ack. What it does is a
//! [`Script`]: login flows (automatic, rotating QR codes, failure, relink identity checks),
//! deliveries of durable events, ephemeral events, connection states and the catch-up boundary,
//! slow forwarding, disconnects, panics, failures and unlinks. The script cursor is durable, so
//! a killed or panicked actor continues where it stopped. An unlink wipes `backend.db` and the
//! cursor with it; the [`FakeControl`] remembers how far each account got, so a relink in the
//! same process plays the rest of the script, not all of it again and not none of it.
//!
//! Durable commands (`SendReceipts`, `SyncReadMarker`, and the deletes for me made on this
//! device, `ClearConversation` and `DeleteAttachment`) go into a job table, unique on the
//! command's id, in the transaction that confirms them with `CommandPersisted`; tests read
//! them with [`FakeControl::durable_commands`] or, from a killed process's files, [`job_log`].
//! The fake names the attachments of own messages
//! (`Network::attachment_net_ids`) like those it delivers ([`scripts::attachment_name`]).
//!
//! - [`scripts`]: one script per network shape for the conformance suite (§7.3).
//! - [`calls`]: every signal of a call a linked device gets, as a script step, and a script
//!   that plays them all.
//! - [`conformance`]: the contract rules every shipped envelope is checked against.
//! - [`fixture`]: deterministic synthetic data for UI fixture mode.
//! - [`attachments`]: synthetic attachment pointers; `DownloadAttachment` writes their plaintext
//!   (deterministic pictures and bytes) into the core's blob target, and outgoing attachments
//!   are read back from the core's blobs before a transmit is accepted.
//!
//! History transfer (architecture §14.2 item 14): a script with a [`TransferScript`] walks
//! one between a fresh link and the login's `Done` step, and [`Step::Transfer`] leaves an
//! account in the middle of one or at its end; the actor reports the stored state at every
//! start and every `Command::Connect` and obeys `SkipHistoryTransfer` and
//! `DismissHistoryTransfer` as the contract says. [`FakeNetwork::with_history`] changes what
//! the descriptor says about history.
//!
//! Own sends: `Command::Transmit` is answered from the script's [`TransmitRule`]s; accepted copies
//! go to a durable server log that doubles as send progress (a resend skips recipients that have
//! the message), readable with [`server_log`]. Rows the core refuses for good are quarantined by
//! the forwarder's poison policy.
//!
//! Kill points (§10.4, `KOURIER_FAILPOINT`): [`FAILPOINT_AFTER_INBOX_COMMIT`],
//! [`FAILPOINT_BEFORE_ACK`], [`FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT`],
//! [`FAILPOINT_AFTER_SERVER_ACCEPT`], and the forwarder's `after_core_commit` from `kourier-db`.

mod actor;
pub mod attachments;
pub mod calls;
pub mod conformance;
pub mod fixture;
mod flavor;
mod network;
mod script;
pub mod scripts;

pub use actor::{
    FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT, FAILPOINT_AFTER_INBOX_COMMIT,
    FAILPOINT_AFTER_SERVER_ACCEPT, FAILPOINT_BEFORE_ACK, Job, MIGRATIONS, ServerDelivery, job_log,
    server_log,
};
pub use flavor::Flavor;
pub use network::{FakeControl, FakeNetwork, TransmitRecord};
pub use script::{
    CHALLENGE_TOKEN, Delivery, LoginScript, Script, ScriptError, Step, TransferScript,
    TransmitRule, challenge_error, challenge_error_after,
};
