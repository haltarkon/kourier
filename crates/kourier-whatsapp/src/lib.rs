//! The WhatsApp backend: a companion device on whatsapp-rust (docs/architecture.md §7).
//!
//! W1 is the crate and its store: [`KourierStore`] implements the whole store contract
//! of whatsapp-rust (`wacore::store::traits::Backend`) over one SQLCipher file per account,
//! `accounts/<uuid>/backend.db`, on a dedicated writer thread. Nothing in the crate talks to WhatsApp
//! by itself: a client is built only when an account's actor first connects or starts a login, and
//! `kourier-app` registers the network in a debug build only, while the user has it on (W2b).
//!
//! whatsapp-rust and wacore types stay inside this crate (CLAUDE.md rule: no protocol types in a
//! public item outside a backend crate). The one public type that implements their traits is
//! [`KourierStore`], for the tests of this crate and of `kourier-e2e`.
//!
//! Modules:
//! - `network`: [`WhatsAppNetwork`], the descriptor and the id of an own message; `actor`: the account
//!   actor (opens the store, serves the commands, drives the login and the connection).
//! - `db`: opening `backend.db`, its migrations (component `whatsapp`) and the transaction helper.
//! - `error`: what a failed operation says (its name, never an input).
//! - W2 (link and connection), in pieces: `translate` (upstream events -> Kourier's own `WaSignal`, the one
//!   file that names upstream's event types), `props` (what the phone is told: `Kourier (host)`, DESKTOP,
//!   "Chrome (Linux)" for the pair code), `states` (what a signal means for the connection, a ban, a logout
//!   and the reach-out lock), `session` (the actor's view of one client: a trait of Kourier's own types, and a
//!   test double), `link` (the login machine: QR and phone-number linking, the identity checks of a pairing),
//!   `tap` (where a client's signals enter the actor, with the pairing's checks made at that moment),
//!   `outbox` (`AccountReady` into `core_outbox` in one zone, and the forwarder that ships the rows),
//!   `facts` (the typed facts that today's contract has no event for), and `actor` (the account actor: commands,
//!   the login, the connection, the shutdown order, over a `Session` made by a factory). The real client
//!   (`RealSession` in `session::client`: `Bot::spawn` on a runtime of the account's own, built at the first
//!   `Connect` or `StartLogin`; a failed connect is read where Kourier owns the transport and the HTTP client).
//! - `logs`: what upstream's own log lines may say (the module list, the level rule, the redaction of their
//!   messages); the app's log calls it (W2b).
//! - `parts` + `real`: [`NetParts`], the lazy maker of the transport factory and the HTTP client a client
//!   needs (a required argument of [`WhatsAppNetwork::new`]), and the ONE module that names the real ones.
//!   A source check in `tests/builder.rs` keeps every other file, tests included, from naming them.
//! - W3a (receive: nothing is acked before it is durable), in pieces: `inbox` (stage 1: the hook's one-zone
//!   commit, the ledger that makes a redelivery a no-op and its sweep), `hook` (upstream's
//!   `InboundDurabilityHook` over the inbox), `events` (the handler's durable write for the kinds whose loss
//!   would matter: it waits for the commit, with a group commit), `stage2` (inbox rows -> `core_outbox` events,
//!   one zone), `map` (one row -> the event it stands for), `catchup` (the machine that owns `Online`: the
//!   offline drain is durable and the core has it), `appstate` (rule (b): the two quiet points where an
//!   app-state collection's dirty flag may be cleared), `hold` (the receive hold of a link: nothing reaches `core_outbox`
//!   before `AccountReady` does), and `fault` (a seam that makes a zone fail, for tests).
//! - W2c (Kourier never signs a pairing the login's rules refuse): `prepair` (the stanza interceptor's pairing half: the
//!   `<pair-success>` is judged by the login's rules BEFORE upstream reads it, a refused one is answered with
//!   upstream's own pair error and ends the login; `link::Gate::claim` holds the rules' first-line verdict).
//! - W4 (the history the phone pushes after a link): `history` (a chunk is captured into a stage file of its
//!   own before the phone is told it arrived, then delivered to the core in rows, once, whatever kills the
//!   process in between).
//! - W5a (send): `send` (`send::address`: the chat a conversation key stands for, the namespace ids are written in,
//!   the key of a message a modifier or a quote names).
//! - `features`: what a WhatsApp chat can do (`ConversationFeatures`, each limit with its enforcer) and where it is
//!   stated: once per version and started fact, in the zone that writes the chat's first events, plus a sweep at start.
//! - `store`: [`KourierStore`] and [`StoreOwner`], one module per store trait.

mod actor;
mod appstate;
mod blocklist;
mod book;
mod calls;
mod catchup;
mod db;
mod error;
mod events;
mod facts;
#[doc(hidden)]
pub mod fault;
mod features;
mod groups;
mod history;
mod hold;
mod hook;
mod inbox;
#[cfg(test)]
mod kill_log;
mod link;
pub mod logs;
mod map;
mod markup;
mod media;
mod merges;
mod names;
mod network;
mod notice;
mod outbox;
mod parts;
mod prepair;
mod props;
pub mod real;
#[cfg(test)]
mod receive_kill;
mod send;
mod session;
mod stage2;
mod states;
pub mod store;
mod strangers;
mod tap;
#[cfg(test)]
mod test_log;
mod translate;
mod wam;

pub use error::OpenError;
pub use network::WhatsAppNetwork;
pub use parts::NetParts;
#[doc(hidden)]
pub use session::Builder;
pub use store::{KourierStore, StoreOwner};
pub use wam::{LOG_MARK as WAM_LOG_MARK, wam_looked, wam_sent};

/// Failpoint names of this backend (`KOURIER_FAILPOINT=<name>[:<n>]`, §10.4), for tests. Each
/// operation that writes more than one row hits its `INSIDE` point with everything written and
/// nothing committed, and its `AFTER` point right after the commit.
pub mod failpoints {
    /// `commit_patch`: inside the transaction, before `COMMIT`.
    pub const INSIDE_COMMIT_PATCH: &str = "wa_inside_commit_patch";
    /// `commit_patch`: right after `COMMIT`.
    pub const AFTER_COMMIT_PATCH: &str = "wa_after_commit_patch";
    /// `set_version` (and `delete_version`): inside the transaction.
    pub const INSIDE_SET_VERSION: &str = "wa_inside_set_version";
    /// `set_version` (and `delete_version`): right after `COMMIT`.
    pub const AFTER_SET_VERSION: &str = "wa_after_set_version";
    /// A batch of Signal state (`put_sessions_batch` and its siblings): inside the transaction.
    pub const INSIDE_SESSIONS_BATCH: &str = "wa_inside_sessions_batch";
    /// A batch of Signal state: right after `COMMIT`.
    pub const AFTER_SESSIONS_BATCH: &str = "wa_after_sessions_batch";
    /// `DeviceStore::save`: inside the transaction.
    pub const INSIDE_DEVICE_SAVE: &str = "wa_inside_device_save";
    /// `DeviceStore::save`: right after `COMMIT`.
    pub const AFTER_DEVICE_SAVE: &str = "wa_after_device_save";
    /// `replace_pending_inbound_if` and `delete_pending_inbound_if`: inside the transaction.
    pub const INSIDE_PENDING_CAS: &str = "wa_inside_pending_cas";
    /// `replace_pending_inbound_if` and `delete_pending_inbound_if`: right after `COMMIT`.
    pub const AFTER_PENDING_CAS: &str = "wa_after_pending_cas";
    /// The first write of a stanza (`store_pending_inbound`, `store_pending_inbound_batch`): inside
    /// the transaction, before `COMMIT` (W3a: a kill here leaves no row and the server has been
    /// told nothing).
    pub const INSIDE_PENDING_STORE: &str = "wa_inside_pending_store";
    /// The first write of a stanza: right after `COMMIT` (the hook has not seen it yet).
    pub const AFTER_PENDING_STORE: &str = "wa_after_pending_store";
    /// `put_msg_secrets`: inside the transaction.
    pub const INSIDE_MSG_SECRETS: &str = "wa_inside_msg_secrets";
    /// `put_msg_secrets`: right after `COMMIT`.
    pub const AFTER_MSG_SECRETS: &str = "wa_after_msg_secrets";
    /// `AccountReady` into `core_outbox`: inside the transaction, before `COMMIT` (a kill here
    /// leaves no row).
    pub const INSIDE_ACCOUNT_READY: &str = "wa_inside_account_ready";
    /// `AccountReady` into `core_outbox`: right after `COMMIT` (a kill here leaves the row, which
    /// the next start ships).
    pub const AFTER_ACCOUNT_READY: &str = "wa_after_account_ready";
    /// The answer to a download request into `core_outbox` (W3b): inside the transaction, before `COMMIT`
    /// (a kill here leaves no row; the core asks again at its next start).
    pub const INSIDE_DOWNLOAD_ANSWER: &str = "wa_inside_download_answer";
    /// The answer to a download request: right after `COMMIT` (the forwarder ships the row at the next start).
    pub const AFTER_DOWNLOAD_ANSWER: &str = "wa_after_download_answer";
    /// The hook's inbox zone (W3a): inside the transaction, before `COMMIT` (a kill here leaves no
    /// row and the server has been told nothing).
    pub const INSIDE_INBOX_COMMIT: &str = "wa_inside_inbox_commit";
    /// The hook's inbox zone: right after `COMMIT` (a kill here leaves the rows; the ack never went
    /// out, so the message comes again and the unique key absorbs it).
    pub const AFTER_INBOX_COMMIT: &str = "wa_after_inbox_commit";
    /// The handler's group commit (W3a): inside the transaction, before `COMMIT` (a kill here
    /// leaves none of the group's rows, and no handler of the group has returned).
    pub const INSIDE_EVENT_WRITE: &str = "wa_inside_event_write";
    /// The handler's group commit: right after `COMMIT` (the rows are there, no handler of the
    /// group has returned yet, so none was acknowledged; the redelivery repeats them).
    pub const AFTER_EVENT_WRITE: &str = "wa_after_event_write";
    /// The capture of a history chunk into the stage (W4): inside the transaction, before `COMMIT` (a kill
    /// here leaves no chunk and the phone was told nothing: upstream answers only after the hook's Ok).
    pub const INSIDE_HISTORY_CAPTURE: &str = "wa_inside_history_capture";
    /// The capture of a history chunk: right after `COMMIT` (a kill here leaves the chunk durable and the
    /// receipt unsent; the notice comes again and the unique message id absorbs it).
    pub const AFTER_HISTORY_CAPTURE: &str = "wa_after_history_capture";
    /// One delivery row of the history (W4): inside the transaction, before `COMMIT` (a kill here
    /// leaves no `core_outbox` row, no cursor move and no secret: the row is read and appended again).
    pub const INSIDE_HISTORY_ROW: &str = "wa_inside_history_row";
    /// One delivery row of the history: right after `COMMIT` (the row, its secrets and the cursor are
    /// there together; the forwarder ships the row at the next start).
    pub const AFTER_HISTORY_ROW: &str = "wa_after_history_row";
    /// The zone that settles the delivery rows the core answered for (W4): inside the transaction, before `COMMIT`
    /// (a kill here leaves the entries as they were; the next look settles them again).
    pub const INSIDE_HISTORY_SETTLE: &str = "wa_inside_history_settle";
    /// The settling zone: right after `COMMIT` (the answered entries are gone or taken back; the next row may follow).
    pub const AFTER_HISTORY_SETTLE: &str = "wa_after_history_settle";
    /// The stage job that retires a delivered chunk (W4): inside the transaction, before `COMMIT` (a kill here
    /// leaves the chunk as it was, bytes and all; the next look retires it again).
    pub const INSIDE_HISTORY_DONE: &str = "wa_inside_history_done";
    /// The retiring job: right after `COMMIT` (the chunk is `done` in the stage and `backend.db` does not know it
    /// yet; the next look does the same job again, which changes nothing).
    pub const AFTER_HISTORY_DONE: &str = "wa_after_history_done";
    /// Stage 2's zone (W3a): inside the transaction, before `COMMIT` (a kill here leaves every row
    /// `new` and no event in `core_outbox`).
    pub const INSIDE_STAGE2: &str = "wa_inside_stage2";
    /// Stage 2's zone: right after `COMMIT` (a kill here leaves the events in `core_outbox` and the
    /// rows `done`; the forwarder ships them at the next start).
    pub const AFTER_STAGE2: &str = "wa_after_stage2";
    /// The outcome of a send (W5a, decision D4): inside the transaction, before `COMMIT` (a kill here
    /// leaves no ledger row; the frame was written, so the core's resend goes out again on a later
    /// connection with the same id).
    pub const INSIDE_SEND_OUTCOME: &str = "wa_inside_send_outcome";
    /// The outcome of a send: right after `COMMIT` (the ledger holds it and the core was not answered;
    /// the core's resend is answered from the ledger with no frame).
    pub const AFTER_SEND_OUTCOME: &str = "wa_after_send_outcome";
    /// The upload record of a file (W5b, decision G4): inside the transaction, before `COMMIT` (a kill here leaves
    /// no record and no frame; the core's resend prepares and uploads the file again under a new key).
    pub const INSIDE_MEDIA_UPLOAD: &str = "wa_inside_media_upload";
    /// The upload record: right after `COMMIT` (the record is there and no frame was written; the core's resend
    /// sends the same file from it with no second upload).
    pub const AFTER_MEDIA_UPLOAD: &str = "wa_after_media_upload";
}
