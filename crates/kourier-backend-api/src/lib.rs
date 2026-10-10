//! The core ↔ backend boundary (architecture §4): the [`Network`] factory and its registry, the
//! per-account [`AccountContext`], [`Command`]s to a backend, [`BackendEvent`]s in
//! [`Envelope`]s with an [`Ack`] back, and the [`Clock`].
//!
//! Message passing, not async trait objects: each account is an actor that owns its runtime;
//! the only trait objects are the synchronous [`Network`] and [`Clock`]. Everything a backend
//! emits is serde data (a backend could later run in a child process); durable events travel
//! through the backend's `core_outbox` (§4.1 principle 6, §5.6) and are acked only after the
//! core's commit.
//!
//! Contract rules beyond the types: [`Envelope::validate`] (acks, history batches, ids, call
//! lines); a call is reported signal by signal as messages that merge into one row
//! ([`IncomingMessage::call`]); during
//! a link `AccountReady` is the first durable event ([`Envelope::validate_linking`]); a row the
//! core refuses for good (a non-retryable [`Ack`] error) is quarantined by the backend, never
//! retried forever (see [`Ack`]); own sends reuse the id allocated at enqueue for every attempt
//! ([`OutgoingRequest`]).

pub mod clock;
pub mod command;
pub mod context;
pub mod event;
pub mod network;

/// The data contract, re-exported for backends.
pub use kourier_model as model;

pub use clock::{Clock, FakeClock, Sleep, SystemClock};
pub use command::{
    BlobTarget, CLEAR_ANCHORS, Command, DurableCommand, OutgoingKind, OutgoingRequest, Relink,
    Reply, TransmitOutcome,
};
pub use context::{
    AccountContext, AccountHandle, BackendConfig, BackendEnv, COMMAND_QUEUE, ContextParams,
    CoreEnds, DbKey, EVENT_QUEUE, Exit, InitialSync, LocalPrefsView, LocalTestEnv, ProxyConfig,
    StartMode, UiPresence, spawn_account_thread,
};
pub use event::{
    Ack, AckWaiter, BackendEvent, BackendNotice, CodecError, ContractViolation, Envelope,
    HistoryEdit, HistoryReaction, IncomingMessage, MAX_HISTORY_BATCH, MAX_HISTORY_EDITS,
    OUTBOX_FORMAT, decode_outbox_row, encode_outbox_row,
};
pub use network::{AllocatedId, Network, NetworkRegistry, RegistryError};
