//! The Threema backend: a linked device on libthreema (docs/architecture.md §7.4).
//!
//! Built through TH6a: the account's store (schema 1: this device's identity and the two permanent
//! nonce stores; schema 2: what a device join imports, and its `join_ready` marker; schema 3: the
//! `deferred` envelopes, with their bounds and kept sum; schema 4: the send queue, sealed), the parts, the network and its account
//! actor, the connection to the mediator and the chat server, the real connectors with the servers'
//! pinned certificates, the device join (the QR link, the three emoji, the import of contacts, groups
//! and settings in two zones), and the FOLLOWER'S RECEIVE: every envelope the phone reflects is
//! opened, replay-checked, mapped and made durable in ONE zone per batch before its ack. TH5a maps
//! text with quotes and mentions, the contact and group syncs (with the group notices) and the chat
//! state of every conversation (a 1:1 chat for every direct contact, protected, archived, pinned,
//! muted as the phone has it); TH5b adds files (one attachment, or a sticker, with its caption; the
//! download is TH8's) and locations, `*bold*` / `_italic_` / `~strike~` markup, edits, deletes and
//! reactions, delivery receipts and the legacy thumbs-up / thumbs-down, the phone's `sent` and `read`
//! updates, the typing indicator (straight to the core, never acked), the settings, profile and
//! distribution list syncs, and the clear zone (the user's Clear Chat deletes that chat's `deferred` rows, since TH6a
//! its unsent send rows too, and confirms the command in one zone). What this build still cannot map (poll votes and calls) is kept whole in
//! `deferred` and mapped again by a later build (`MAP_VERSION` 2: the first start of this build maps
//! every version-1 row); the account is `Syncing` until the core has taken the backlog.
//!
//! THE SEND CORE (TH6a, `send`'s module doc): the Bundled Messages Send Steps as a persistent queue of sealed rows,
//! resumed by window, with the contract's answers, the cooling of a row the server will not take and the user's clear
//! of what is unsent. Since TH8 every chat can be sent in: a text, or one file per message (`send::files`). libthreema types stay inside this crate: no public item names
//! one.
//!
//! WHAT CAN CONTACT THREEMA. Building the network, or calling a maker, dials nothing: every WebSocket
//! and every HTTPS request goes through [`ThreemaParts`], a lazy maker the actor calls on its own
//! thread, only for an account with a stored identity that connects, or for a link that passed the
//! gate of `StartLogin` (B1: an EMPTY [`AllowList`] fails the login at step `qr` before the maker is
//! called). `dial/` holds the REAL connectors and the application's one entry to them; only
//! `kourier-app` makes them. A normal start of the application has Threema on and takes any ID
//! ([`AllowList::any`]); a start by the run tool links only in a prepared run profile (a kept profile
//! under `<data>/kourier-threema-run/` with a `threema-allowed-ids` file): that list of test IDs is the
//! one gate before the first socket of a link, and the ID check at `EssentialData` is the second line.
//! Tests hand in parts that refuse and count, or the offline fake.
//!
//! Modules:
//! - `network`: [`ThreemaNetwork`], the descriptor, the device label and the id of an own message;
//!   `msg_id`: the ONE text form of a message id (libthreema's).
//! - `parts`: [`ThreemaParts`], the lazy maker of a client's WebSocket connector and HTTPS client
//!   (this crate's own traits), the servers' data and the allow-list, a required argument of
//!   [`ThreemaNetwork::new`]; `dial`: the ONE module that names the real connectors (TH3: the pins,
//!   TLS, the WebSocket client, the HTTPS client, the production servers' data). A source check in
//!   `tests/builder.rs` keeps every other file, tests included, from naming a socket, a TLS or HTTP
//!   crate or a Threema host, and from asking for the real parts (only `kourier-app` does).
//! - `reflect` (TH5a, TH5b): the follower's receive. The batch: open (P5), the nonce rule, replay check, ONE zone,
//!   the acks it allows; the typing of an ephemeral envelope (`open_ephemeral`: no zone, no ack); `classify`:
//!   the one table of what each envelope kind and CSP type is (mapped, deferred, unsupported, dropped,
//!   ephemeral) and `MAP_VERSION`; `body`, `map`: the CSP bodies (text, markup, file, location, receipts,
//!   modifiers) and the events of a message, an update or a modifier, and `conversation_of`, the chat a
//!   stored row belongs to; `sync`, `notice`: the contact, group, settings, profile and list deltas field
//!   by field and the group notices; `remap`: the start's second look at `deferred` under a newer
//!   `MAP_VERSION`. Only this directory names P5's open and seal functions (`tests/builder.rs` rule 14).
//! - `actor`: the account actor (Signal's thread shape): it opens the store, owns the connection and
//!   the join task, and answers every request (a `Transmit` is refused in a real build and reaches the send core only
//!   behind the parts' test door; the user's Clear Chat is carried out, TH5b R1, and takes the chat's unsent send
//!   rows too, TH6a R5); `outbox`: `core_outbox` rows and the forwarder that ships them to the core.
//! - `conn`: the account's connection (TH3): the mediator and the chat server inside it, the
//!   keep-alive, what a close means for the state the core sees; it hands an ephemeral typing to the
//!   core on the actor's events channel (TH5b); it runs the send queue's turn at the end of every pass, bounds
//!   the wait for an ack and a stalled write, and sweeps the queue at every socket (TH6a).
//! - `close`: what a closed mediator connection means (the spec's close codes, a `close-error`, the
//!   backoff); pure, used by the connection and by the fake's tests.
//! - `db`: opening `backend.db` and its migrations (component `threema`); `store`: the identity row,
//!   the nonce stores, the two zones of a join, the `deferred` envelopes (schema 3: the bounds,
//!   the kept sum, the re-map's pages, the clear's zone, which also takes the chat's unsent send rows), the
//!   doc-hidden doors of the send queue (schema 4) and the settings, profile and list writes of
//!   the receive path; `error`: what a failed operation says (its name, never an
//!   input).
//! - `send` (TH6a): the send core. `queue`: schema 4's rows, sealed, the zones that move them, the two budgets and
//!   the sweep; `props`, `body`, `encode`: what a message type asks of a send and the bytes of a send, the one file
//!   `encode` naming the three seals (rule 14); `receivers`, `prepare`: each receiver's fate and ZONE PREPARE;
//!   `machine`: one row on one socket, sans-I/O; `shared`: what the actor and the socket share, with the cooling;
//!   `attempt`: a `Transmit` behind the test door and every answer to the core. Nothing in a real build reaches it.
//! - `leader` (TH6, being built): the leader's receive. `zone` (L1): schema 5's `csp_inbound`, ZONE RECEIVE (a batch
//!   of messages taken from the chat server, kept ones mapped through the follower's own mapper with their sealed
//!   reflect, in one zone), ZONE REFLECTED (with the delivery receipts as jobs), the sweep and the step-0 read. Nothing
//!   in a real build calls it yet.
//! - `logs`: which log records are libthreema's (by module path) and the level rule for them (WARN and
//!   up, whatever the user's filter says); the app's log calls it (TH4 chunk L, C11).
//! - `join` (TH4): the device join; the offer and the text of its QR code, the emoji of the rendezvous path
//!   hash, the join task (the rendezvous up to `EssentialData`), the checks of the phone's data, the rules
//!   for the ID it brings and the core events of a joined account; `proto`: `url-payloads.proto`, the one
//!   protocol file libthreema does not build; `allow`: [`AllowList`], the Threema IDs a link may bring (M6, B1).

mod actor;
mod allow;
mod blob;
#[doc(hidden)]
pub mod close;
mod conn;
mod db;
pub mod dial;
mod error;
mod join;
mod leader;
pub mod logs;
mod msg_id;
mod network;
mod outbox;
mod parts;
mod pictures;
mod proto;
mod reflect;
mod send;
#[doc(hidden)]
pub mod store;

pub use allow::AllowList;
pub use error::{OpenError, StoreError};

/// Whether this build of the backend can link a device (decision Q2): `true` since TH4 (the device
/// join). The application lists Threema as "compiled in but off" only when it can link (an action
/// that does nothing yet is not shown) AND the profile is a prepared run profile whose test-ID list
/// is not empty (decision E1: `kourier-app/src/threema.rs`); a plain debug start keeps Threema
/// absent, and a hand-edited `networks-on` still registers it (the backend then refuses `StartLogin`
/// at once, B1's gate).
pub const LINKING_BUILT: bool = true;

/// Failpoint names of this backend (`KOURIER_FAILPOINT=<name>[:<n>]`, architecture §10.4), for
/// the kill loop (`tests/kill.rs`). Each operation that writes more than one row hits its `INSIDE`
/// point with everything written and nothing committed, and its `AFTER` point right after the
/// commit.
pub mod failpoints {
    /// The link's commit (`store::record_link`: the identity row and `AccountReady` in
    /// `core_outbox`): inside the transaction, before `COMMIT`.
    pub const INSIDE_SAVE_IDENTITY: &str = "threema_inside_save_identity";
    /// The link's commit: right after `COMMIT`.
    pub const AFTER_SAVE_IDENTITY: &str = "threema_after_save_identity";
    /// A batch of nonces of both stores (`store::add_nonce_batches`): inside the transaction.
    pub const INSIDE_ADD_NONCES: &str = "threema_inside_add_nonces";
    /// A batch of nonces of both stores: right after `COMMIT`.
    pub const AFTER_ADD_NONCES: &str = "threema_after_add_nonces";
    /// Zone 1 of a join (`store::import_join`: the identity, profile, settings, contacts, groups,
    /// lists, held blobs and hashed nonces; no `core_outbox` row): inside the transaction.
    pub const INSIDE_IMPORT: &str = "threema_inside_import";
    /// Zone 1 of a join: right after `COMMIT`.
    pub const AFTER_IMPORT: &str = "threema_after_import";
    /// Zone 2 of a join and a `Rebuild`'s start (`store::ready_join`: the `core_outbox` rows,
    /// `AccountReady` first): inside the transaction.
    pub const INSIDE_READY: &str = "threema_inside_ready";
    /// Zone 2 of a join: right after `COMMIT`.
    pub const AFTER_READY: &str = "threema_after_ready";
    /// The zone of one batch of reflected envelopes (TH5: their nonces, the `deferred` rows with the
    /// bounds' deletes and the kept sum, the `core_outbox` rows; `store::REFLECTED_POINTS`): inside the
    /// transaction, before `COMMIT` and before any ack.
    pub const INSIDE_REFLECTED: &str = "threema_inside_reflected";
    /// The batch's zone: right after `COMMIT`, before any ack.
    pub const AFTER_REFLECTED: &str = "threema_after_reflected";
    /// TH5a A9a (kill window K3): right after ONE `reflected-ack` of a committed batch went out (its
    /// frame written), before the next: a kill after some acks of a zone and before the rest.
    pub const AFTER_REFLECTED_ACK: &str = "threema_after_reflected_ack";
    /// One zone of the start's re-map of `deferred` (at most 30 rows: each deleted or given its new
    /// version, with the `core_outbox` rows of its events; `store::REMAP_POINTS`): inside the
    /// transaction.
    pub const INSIDE_REMAP: &str = "threema_inside_remap";
    /// One zone of the re-map: right after `COMMIT`.
    pub const AFTER_REMAP: &str = "threema_after_remap";
    /// The start's age bound of `deferred` (`store::expire_deferred`: every row past 45 days and the
    /// kept sum): inside the transaction.
    pub const INSIDE_EXPIRE: &str = "threema_inside_expire";
    /// The start's age bound: right after `COMMIT`.
    pub const AFTER_EXPIRE: &str = "threema_after_expire";
    /// TH5b R1, the zone of the user's Clear Chat / Delete Chat (`Command::ClearConversation`,
    /// `store::clear_deferred`: every `deferred` row of that chat deleted with the kept sum; TH6a R5: the
    /// chat's unsent `transmit` rows of the send queue at or below the clear's `before_ms`, but those with a
    /// `Transmit` attached, with their receivers; and the command's `CommandPersisted` in `core_outbox`):
    /// inside the transaction.
    pub const INSIDE_CLEAR: &str = "threema_inside_clear";
    /// The clear's zone: right after `COMMIT`.
    pub const AFTER_CLEAR: &str = "threema_after_clear";
    /// TH6a, ZONE PREPARE of a send (`store::prepare_send_zone`: the task, its receivers with their sealed boxes and
    /// the nonces of both stores): inside the transaction. Its commit is the point of no return.
    pub const INSIDE_SEND_PREPARE: &str = "threema_inside_send_prepare";
    /// ZONE PREPARE: right after `COMMIT`, before the reflect's frame.
    pub const AFTER_SEND_PREPARE: &str = "threema_after_send_prepare";
    /// The frame of a send's reflect is written, its ack not heard.
    pub const AFTER_SEND_REFLECT: &str = "threema_after_send_reflect";
    /// ZONE REFLECTED (`store::mark_send_reflected`: the reflect-ack made durable, the sealed reflect emptied): inside
    /// the transaction, before any box.
    pub const INSIDE_SEND_REFLECTED: &str = "threema_inside_send_reflected";
    /// ZONE REFLECTED: right after `COMMIT`, before any box.
    pub const AFTER_SEND_REFLECTED: &str = "threema_after_send_reflected";
    /// The k-th box's frame of a pass is written, its ack not heard (`threema_after_send_box:k`).
    pub const AFTER_SEND_BOX: &str = "threema_after_send_box";
    /// ZONE ACKS (`store::mark_send_acks`: one pass's acks, and in the last one the sealed update and its nonce):
    /// inside the transaction.
    pub const INSIDE_SEND_ACKS: &str = "threema_inside_send_acks";
    /// ZONE ACKS: right after `COMMIT`.
    pub const AFTER_SEND_ACKS: &str = "threema_after_send_acks";
    /// The frame of a send's update is written, its ack not heard.
    pub const AFTER_SEND_UPDATE: &str = "threema_after_send_update";
    /// ZONE DONE (`store::mark_send_done`: the update's reflect-ack made durable; a transmit row `done`, a job's row
    /// deleted with its receivers): inside the transaction.
    pub const INSIDE_SEND_DONE: &str = "threema_inside_send_done";
    /// ZONE DONE: right after `COMMIT`, before the reply.
    pub const AFTER_SEND_DONE: &str = "threema_after_send_done";
    /// The send queue's sweep (`store::sweep_sends`: every row past its age, with its receivers): inside the
    /// transaction.
    pub const INSIDE_SEND_SWEEP: &str = "threema_inside_send_sweep";
    /// The sweep: right after `COMMIT`.
    pub const AFTER_SEND_SWEEP: &str = "threema_after_send_sweep";
    /// A job's give-up after its failed runs (`store::give_up_send_job`: its row with its receivers): inside the
    /// transaction.
    pub const INSIDE_SEND_GIVE_UP: &str = "threema_inside_send_give_up";
    /// A job's give-up: right after `COMMIT`.
    pub const AFTER_SEND_GIVE_UP: &str = "threema_after_send_give_up";
    /// TH6, ZONE RECEIVE of the leader (`store::receive_inbound`: a batch of messages taken from the chat server, each
    /// kept one with its events, jobs, both nonces and its `csp_inbound` row `pending` with the sealed reflect; each
    /// discarded one with its nonce and jobs): inside the transaction, before any frame.
    pub const INSIDE_INBOUND_RECEIVE: &str = "threema_inside_inbound_receive";
    /// ZONE RECEIVE: right after `COMMIT`, before the reflects and the acks of the discards.
    pub const AFTER_INBOUND_RECEIVE: &str = "threema_after_inbound_receive";
    /// TH6, ZONE REFLECTED of the leader (`store::mark_inbound_reflected`: the rows whose reflect-ack came, `reflected`,
    /// with the delivery-receipt jobs): inside the transaction, before any CSP ack.
    pub const INSIDE_INBOUND_REFLECTED: &str = "threema_inside_inbound_reflected";
    /// ZONE REFLECTED: right after `COMMIT`, before the CSP acks.
    pub const AFTER_INBOUND_REFLECTED: &str = "threema_after_inbound_reflected";
    /// TH6, the leader's sweep (`store::sweep_inbound`: `reflected` rows past 14 days, `pending` rows past 30 days with
    /// their CSP nonces): inside the transaction.
    pub const INSIDE_INBOUND_SWEEP: &str = "threema_inside_inbound_sweep";
    /// The leader's sweep: right after `COMMIT`.
    pub const AFTER_INBOUND_SWEEP: &str = "threema_after_inbound_sweep";
    /// TH6 L4b: the k-th inbound `Reflect` of a socket is written, its ack not heard (`threema_after_inbound_reflect:k`).
    pub const AFTER_INBOUND_REFLECT: &str = "threema_after_inbound_reflect";
    /// TH6 L4b: the k-th CSP `message-ack` of an incoming message is written (`threema_after_inbound_ack:k`).
    pub const AFTER_INBOUND_ACK: &str = "threema_after_inbound_ack";
    /// TH6 L5: the leader's `BeginTransaction` (scope `CONTACT_SYNC`) is written, its answer not heard.
    pub const AFTER_TX_BEGIN: &str = "threema_after_tx_begin";
    /// TH6 L5: the `ContactSync` reflect inside the transaction is written, its ack not heard.
    pub const AFTER_TX_REFLECT: &str = "threema_after_tx_reflect";
    /// TH6 L5: `CommitTransaction` is written, its ack not heard.
    pub const AFTER_TX_COMMIT: &str = "threema_after_tx_commit";
}
/// "Connect Anyway, until restart" of the clock warning (his Q4, decision C2): the account's
/// connections ignore a clock that is off by more than 20 minutes until the Kourier process ends.
/// A plain `Connect` never sets it. NOTHING calls this in TH3 but the crate's tests; KH1f wires the
/// warning's button to it (no new `Command` variant).
#[doc(hidden)]
pub fn connect_anyway(account: kourier_model::AccountUuid) {
    conn::skew::set_anyway(account);
}

/// The three emoji of a rendezvous path hash (TH4, `join::emoji`: Desktop's list, `EMOJI[byte % 128]`
/// of the first three bytes), for the join tests to compare the `emoji` step with the fake phone's OWN
/// path hash (equal emoji: equal hashes). Nothing in the application calls it.
#[doc(hidden)]
#[must_use]
pub fn emoji_of_path_hash(rph: &[u8; 32]) -> [&'static str; 3] {
    join::emoji::emoji_of(rph)
}

pub use network::ThreemaNetwork;
pub use parts::{
    HttpsClient, HttpsMethod, HttpsRequest, HttpsResponse, NetError, ServerParams, ThreemaParts,
    Transports, UntrustedKind, WsConnector, WsEvent, WsKind, WsRequest, WsSender,
};
