//! THE SEND CORE (TH6a): the Bundled Messages Send Steps (`csp.struct.yml`) as a persistent queue of sealed rows,
//! resumed by window. It has no user yet: every chat still says `CannotSend::NotYetBuilt`, and nothing in a real build
//! reaches it.
//!
//! THE ORDER, zone by zone: PREPARE (the task, its receivers, every sealed box, the sealed reflect, every nonce) ->
//! the reflect's frame -> its ack -> REFLECTED -> the boxes -> their acks -> ACKS (once per pass; the last one writes
//! the sealed `Sent` update and its nonce) -> the update's frame -> its ack -> DONE. A frame goes only after the zone
//! that holds its bytes has committed, and an answer only after the zone that makes it true. The commit of PREPARE is
//! the point of no return.
//!
//! SEALED ONCE, RESUMED BY WINDOW. A row is sealed once, in PREPARE. A resend writes the stored bytes, equal byte for
//! byte to a frame whose outcome is unknown, and only where that outcome is not durable: a receiver with a durable ack
//! gets nothing, and after `done` no frame goes at all. Never encode again under a stored nonce: libthreema pads
//! randomly, so a second encode would reuse an XSalsa20 keystream over different plaintext. Sealed is not private: CK
//! and DGK rest in the same file, so the user's clear deletes a chat's unsent rows.
//!
//! THE FALLBACK if a real server or phone does not ack a byte-identical repeat (the first run against one will tell;
//! the offline fake cannot): a later step opens the stored bytes (CK or DGRK are in the file) and seals them again
//! under a fresh nonce. That needs no migration, only a wider rule 14.
//!
//! THE PURE PIECES (S2): `props` (what the send steps ask of a message type: one exhaustive match over the pinned
//! enum), `body` (the bodies a send writes: a text and the group member container), `encode` (the padded container,
//! the metadata, the two boxes of a frame, the payload's layout, the `d2d.Envelope` and its seal; the one file that
//! names the three seals, rule 14).
//!
//! THE POINT OF NO RETURN (S4): `receivers` works out each receiver's fate over a read view of the file (spec step
//! 2); `prepare` draws the nonces, seals and hands `queue` the rows of ZONE PREPARE. [`enqueue_job`] is the same inside
//! the caller's zone.
//!
//! THE WIRE (S5a): `machine` is one row on one socket, sans-I/O: what comes after what. The socket's pump
//! (`crate::conn`) feeds it the acks of a pass, runs its zones and writes its frames, each frame after the zone that
//! holds its bytes. `shared` is what the actor and the socket share.
//!
//! WHEN THE SERVER DOES NOT TAKE A ROW (S5b, decision R2; `shared`'s doc): no ack within `ACK_WITHIN` ends the socket,
//! a row with frames unanswered at a missing ack or a server's close has a failed run, the first is free and from the
//! second the row cools, later rows go, and a job gives up at 12. A row is never the reason for `NeedsUserAction`.
//!
//! THE DOOR AND THE CONTRACT (S6): `attempt` is what a `Command::Transmit` becomes behind the parts' test door: the
//! ledger's read, a new row, the attempt attached to its row, and every answer to the core, read from durable rows after
//! the zone that makes it true. In a real build the door is shut: a `Transmit` is refused and nothing arrives here.
//!
//! ONE TASK AT A TIME per account, in row order among the rows that are not cooling: a deliberate deviation from
//! `md.md`'s plain FIFO, so that a row the server will not take holds no later row behind it. The core allows one
//! attempt in flight per conversation, so the skip cannot reorder one chat's `transmit` rows.

use kourier_db::Tx;

use crate::error::Result;

pub(crate) mod attempt;
pub(crate) mod body;
pub(crate) mod encode;
pub(crate) mod machine;
pub(crate) mod prepare;
pub(crate) mod props;
pub(crate) mod queue;
pub(crate) mod receivers;
pub(crate) mod shared;

use prepare::{NewBody, To};

/// A message of the backend's own (a delivery receipt, an FS reject, a creator's answer, a group control): it has no
/// `Transmit`, no reply and no event. A random id and the clock's time ([`enqueue_job`]). `Debug` prints counts.
#[derive(Clone, PartialEq, Eq)]
pub struct NewJob {
    /// A short name of the code's: `[a-z0-9-]`, 1 to 32 characters (the schema's CHECK).
    pub kind: &'static str,
    /// The conversation it belongs to.
    pub to: To,
    /// What it says. A job with `NewBody::Raw` decides itself whether it distributes the profile where the type
    /// decides by its content (L8: a receipt with a reaction's status does, a plain one does not).
    pub body: NewBody,
    /// `None`: everyone the conversation says. `Some`: these alone, whoever the stored group lists (a group creator's
    /// answer to one member; the group need not be stored).
    pub only: Option<Vec<String>>,
}

impl std::fmt::Debug for NewJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewJob")
            .field("kind", &self.kind)
            .field("only", &self.only.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// A JOB, inside the caller's zone: sealed and written with the rows that caused it, so a kill leaves both or neither
/// (a later step calls this in its receive zone and kicks the queue after the commit). Returns the task's id. Fails
/// the caller's zone: [`crate::StoreError::Refused`] when the message cannot go, [`crate::StoreError::QueueFull`] when
/// the job budget is spent (the chat server keeps the message then, nothing is acked).
#[doc(hidden)]
pub fn enqueue_job(tx: &mut Tx<'_>, job: &NewJob, now_ms: i64) -> Result<i64> {
    let rows = prepare::seal_job(tx, job, now_ms)?;
    // A job evicts nothing: no `Transmit` is attached to a row it could take, and none is asked for.
    queue::prepare_send(tx, &rows, &[])
}
