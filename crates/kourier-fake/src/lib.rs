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
//! Roaming settings (A10a): the fake plays a network that writes the archive, pin, mute and
//! marked-unread settings it keeps for the account's other devices. [`Script::roaming_writes`]
//! is what it states of them in its `AccountCaps` (default `Some(true)`; `None` = it states
//! nothing, like a network that cannot), `PIN_LIMIT` the number of pins it allows. It keeps
//! the account's pin list (a new pin goes to the end, archiving unpins, pinning unarchives),
//! refuses a roaming change while the flag it last stated is not `Some(true)`, obeys
//! `Command::SetRoamingWrites` by stating its caps again, and applies and acknowledges
//! `Command::ChangeConversationDurable`.
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
//! The fake that plays Telegram (KT-1b, `FakeNetwork::playing(NetworkKind::Telegram, ..)`; no
//! Telegram code is in this crate): the `phone` login flow is a table of accounts keyed on the
//! DIGITS the person types ([`PhoneLoginScript`], [`PhoneAccount`]): a refusal at `phone`, the code
//! with its delivery and the wait before a new one, the two-step password, a login e-mail; a number
//! in no row fails at `registration`. A QR scan is followed by the password of the account the
//! phone scans in, and only then by the check of its number (TDLib's order). [`telegram`] holds
//! what the run tool's rehearsal plays (`FakeNetwork::rehearsal_telegram()`: a first QR code nobody
//! scans, the second linking with the password, a direct chat and a basic group served on link)
//! and its synthetic constants, and `telegram_features`, the features the Telegram backend states.
//!
//! Own sends: `Command::Transmit` is answered from the script's [`TransmitRule`]s; accepted copies
//! go to a durable server log that doubles as send progress (a resend skips recipients that have
//! the message), readable with [`server_log`]. The log is keyed by the message's id, the recipient
//! and the CONVERSATION (K-B1: a network with random ids may meet one id in two chats, and those
//! are two messages; a row an older build logged has no conversation and counts for any chat).
//! Rows the core refuses for good are quarantined by the forwarder's poison policy.
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
pub mod rehearsal;
mod safety;
mod script;
pub mod scripts;
pub mod telegram;
mod upload;

pub use actor::{
    FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT, FAILPOINT_AFTER_INBOX_COMMIT,
    FAILPOINT_AFTER_SERVER_ACCEPT, FAILPOINT_BEFORE_ACK, Job, MIGRATIONS, ServerDelivery, job_log,
    server_log,
};
pub use flavor::{Flavor, WHATSAPP_LIKE_BUDGET};
pub use network::{AbortRecord, FakeControl, FakeNetwork, TransmitRecord};
pub use rehearsal::{Arrival, ArriveError, PhoneAct};
pub use script::{
    BackfillScript, CHALLENGE_TOKEN, Delivery, EMOJI_FOR, FAKE_EMOJI, LoginScript, PIN_LIMIT,
    PairCodeScript, PasswordScript, PhoneAccount, PhoneLoginScript, SafetyScript, Script,
    ScriptError, Step, TransferScript, TransmitRule, UploadScript, challenge_error,
    challenge_error_after,
};
