//! The inbox: stage 1 of the receive path (W3a; architecture §5.6, §7.1; the Signal backend's
//! `receive::stage1` is the model).
//!
//! [`Inbox::commit`] is what upstream's durability hook calls (through [`crate::hook::Hook`]). One call
//! is ONE writer job and ONE zone: every message of the batch is inserted, in slice order, together
//! with the `core_outbox` facts the batch implies (none in W3a; W3b adds e.g. identity changes), and
//! the call returns only after COMMIT. That is the whole contract of the hook: upstream acknowledges the
//! batch to the server when the hook returns `Ok`, so an `Ok` before the commit would lose a message
//! for good. The zone is `Priority::High` on the store's one lane, the same as every other job of
//! [`KourierStore`], so upstream's own pending-row cleanup (a compare-and-swap job queued AFTER the hook
//! returned) can never run before it (`tests/hook.rs` pins the lane and the priority).
//!
//! Idempotence is the hook contract's second half: upstream is at-least-once. A message that is
//! already there (a replay after a crash that fell between the commit and the ack, or after a hook
//! `Err`) is skipped by the unique key `(chat, sender, id)`, whatever state its row is in: a `done` row
//! stays as the ledger, so a replayed message is never turned into events twice.
//!
//! A row is `catch_up` when it is written while the catch-up boundary is unknown (every new connection
//! starts there, §4.5) or when its stanza came from the offline queue (`meta.is_offline`): such rows
//! are summarized by the core, not notified one by one.
//!
//! Retention (decision D7): the content of a row goes at stage 2 (in the same zone; `secure_delete`
//! clears the file's pages and the old pages stay in `backend.db-wal`, encrypted, until a
//! checkpoint). The ledger row (ids and JIDs, no content) is what makes upstream's redelivery a
//! no-op, and upstream redelivers whatever the server did not see acknowledged, at the next
//! connection's offline drain. So a ledger row goes ONLY when a connection that began after the row
//! was committed has drained durably (`crate::catchup::CatchUp::take_sweep`: upstream said the drain
//! is complete AND Kourier's own boundary is through stage 2 and the core), and then only when the row
//! is also older than [`LEDGER_AGE_MS`] ([`sweep_ledger`]); never by age alone, because an account
//! that was paused for weeks still has unacknowledged messages waiting for it. A row stage 2 has not
//! finished is never swept. The upstream `pending_inbound_messages` row of a message lives only until
//! upstream's cleanup after the hook returned (its own 7-day keepalive sweep is the only other
//! sweep); `sent_messages` keep a payload for `session::SENT_MESSAGE_TTL_SECS`. The message secrets
//! (`store::secret`) are NOT swept by Kourier: W3a deletes none. Upstream's default policy
//! (`Managed`) gives each a deadline by the parent's event time (30 days for a text, 90 for a poll or
//! an event) and its keepalive prunes them; the edits, poll votes and bot replies that need a
//! parent's secret in W4, W5 and K8a are bounded in time by the protocol. Keeping a secret for as
//! long as its message stays in Kourier (`MsgSecretPolicy::Full`, ~270 bytes a message) is a switch
//! those steps may throw; W3a leaves upstream's default.
//!
//! Nothing here logs an id, a JID, a name or a message: failures carry the operation's name only.

use std::fmt;
use std::sync::Arc;

use kourier_backend_api::{BackendEvent, Clock, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::{OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use wacore::store::error::StoreError;

use crate::db::Points;
use crate::error::{self, sql};
use crate::fault;
use crate::store::KourierStore;

/// How old a ledger row must be before the sweep may take it (decision D7): twice the 7 days upstream
/// keeps a message it has not seen acknowledged, the other half being the margin for a clock that
/// stepped. Age is the floor, never the reason: see the module doc.
pub(crate) const LEDGER_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// Deletes the ledger rows (`done`) that were committed before the connection whose drain just
/// completed durably began (`arrival <= covers`, its arrival counter at its start) and are older than
/// [`LEDGER_AGE_MS`]; the number of rows. A low-priority job: live work is served first.
pub(crate) async fn sweep_ledger(
    store: &KourierStore,
    covers: i64,
    now_ms: i64,
) -> Result<usize, StoreError> {
    let oldest = now_ms.saturating_sub(LEDGER_AGE_MS);
    store
        .sweep("sweep the inbox ledger", move |tx| {
            crate::store::exec(
                tx,
                "sweep the inbox ledger",
                "DELETE FROM inbox WHERE state = 'done' AND arrival <= ?1 AND received_at < ?2",
                params![covers, oldest],
            )
        })
        .await
}

/// How a row got into the inbox (`inbox.origin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// An end-to-end encrypted message: it went through the durability hook.
    E2e,
    /// A newsletter message: upstream acks it on its own path, before any handler (at most once).
    Newsletter,
    /// A message recovered by PDO: at most once as well.
    Pdo,
}

impl Origin {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::E2e => "e2e",
            Self::Newsletter => "newsletter",
            Self::Pdo => "pdo",
        }
    }

    /// The inverse of [`as_str`](Self::as_str).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "e2e" => Some(Self::E2e),
            "newsletter" => Some(Self::Newsletter),
            "pdo" => Some(Self::Pdo),
            _ => None,
        }
    }
}

/// What stage 2 needs of a message's `MessageInfo`: OUR serde struct, built by
/// `translate::arrival`, never upstream's `Serialize` output (it has no `Deserialize` and is not a stable
/// format). Every field after `v` has a serde default, so a later step adds fields without a migration.
/// `Debug` prints no name and no id.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InboxMeta {
    /// The layout version of this struct.
    #[serde(default)]
    pub(crate) v: u8,
    /// The stanza's timestamp (seconds since the epoch).
    #[serde(default)]
    pub(crate) timestamp: i64,
    /// The message is our own (a self-fanout or a sync of another device).
    #[serde(default)]
    pub(crate) from_me: bool,
    /// The chat is a group.
    #[serde(default)]
    pub(crate) is_group: bool,
    /// The sender's push name (a name: never logged).
    #[serde(default)]
    pub(crate) push_name: String,
    /// The chat addresses its members by LID.
    #[serde(default)]
    pub(crate) lid_addressing: bool,
    /// The sender's other identity (the PN of a LID sender and the other way round), when the
    /// stanza said.
    #[serde(default)]
    pub(crate) sender_alt: Option<String>,
    /// The other identity of the RECIPIENT of our own direct message (the number of the LID chat's
    /// peer), when the stanza said. Upstream sets it only for our own self-synced LID direct
    /// messages (`peer_recipient_pn`), so only a `from_me` row can vouch a pair with it. Rows
    /// committed before W3b lack it and vouch nothing.
    #[serde(default)]
    pub(crate) recipient_alt: Option<String>,
    /// The disappearing timer in seconds that upstream handed over with the decrypted message. A
    /// replay re-parses the stanza and can lose it (upstream's doc): stage 2 then reads the timer
    /// the stored message itself carries, and a message that carries none has NO expiry (fix F1:
    /// there is no chat-level fallback).
    #[serde(default)]
    pub(crate) ephemeral_expiration: Option<u32>,
    /// The stanza came from the offline queue.
    #[serde(default)]
    pub(crate) is_offline: bool,
}

impl InboxMeta {
    /// The layout this build writes.
    pub(crate) const VERSION: u8 = 1;
}

impl fmt::Debug for InboxMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboxMeta")
            .field("v", &self.v)
            .field("from_me", &self.from_me)
            .field("is_group", &self.is_group)
            .field("is_offline", &self.is_offline)
            .finish_non_exhaustive()
    }
}

/// One message on its way into the inbox. Built by `translate::arrival`; `Debug` prints no content.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Arrival {
    pub(crate) chat: String,
    pub(crate) sender: String,
    pub(crate) id: String,
    pub(crate) origin: Origin,
    /// The encoded `wa::Message` (`waproto::codec::message_to_vec`).
    pub(crate) message: Vec<u8>,
    pub(crate) meta: InboxMeta,
}

impl fmt::Debug for Arrival {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Arrival")
            .field("origin", &self.origin)
            .field("bytes", &self.message.len())
            .finish_non_exhaustive()
    }
}

/// What one commit did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Committed {
    /// Messages the batch had.
    pub(crate) total: usize,
    /// Rows written now; `total - inserted` were already there (a replay).
    pub(crate) inserted: usize,
}

/// The kill points of the inbox zone (`failpoints::INSIDE_INBOX_COMMIT` / `AFTER_INBOX_COMMIT`).
const POINTS: Points = Points {
    inside: crate::failpoints::INSIDE_INBOX_COMMIT,
    after: crate::failpoints::AFTER_INBOX_COMMIT,
};

/// The inbox of one account. Cheap to clone.
#[derive(Clone)]
pub(crate) struct Inbox {
    store: KourierStore,
    clock: Arc<dyn Clock>,
}

impl Inbox {
    pub(crate) fn new(store: KourierStore, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Commits `batch` and `facts` in ONE zone and returns after COMMIT; see the module doc. An
    /// `Err` leaves nothing behind (the zone rolls back) and means the caller must not let the
    /// server be told anything about these messages.
    pub(crate) async fn commit(
        &self,
        batch: Vec<Arrival>,
        facts: Vec<BackendEvent>,
    ) -> Result<Committed, StoreError> {
        if batch.is_empty() && facts.is_empty() {
            return Ok(Committed::default());
        }
        let job = self.job(batch, facts)?;
        self.store.write_at("commit inbox", POINTS, job).await
    }

    /// [`commit`](Self::commit) for the at-most-once kinds (newsletters, PDO): the zone is QUEUED
    /// before this returns, in the order of the calls, and nobody waits for it. Upstream
    /// acknowledges those messages before any handler has run (`node_io.rs` `maybe_deferred_ack`),
    /// so there is no ack to hold and no claim that one waits (decision D4).
    pub(crate) fn commit_detached(&self, batch: Vec<Arrival>) -> Result<(), StoreError> {
        if batch.is_empty() {
            return Ok(());
        }
        let job = self.job(batch, Vec::new())?;
        self.store
            .write_at_detached("commit inbox", POINTS, move |tx| job(tx).map(drop))
    }

    /// Takes the catch-up boundary at THIS point of the writer's order (queued, nobody waits): every
    /// commit queued before it is `<=` the boundary, every one after is `>` it. The handler calls it
    /// when upstream says the drain is complete, before the signal reaches the actor
    /// ([`crate::catchup`]).
    pub(crate) fn take_boundary_detached(&self) -> Result<(), StoreError> {
        self.store.write_detached("take catch-up boundary", |tx| {
            crate::catchup::take(tx).map(drop)
        })
    }

    /// Forgets the boundary at this point of the writer's order (queued, nobody waits): the drain
    /// that begins now is not known to be over, so the rows after this are summarized as catch-up
    /// until its boundary is taken.
    pub(crate) fn forget_boundary_detached(&self) -> Result<(), StoreError> {
        self.store
            .write_detached("forget catch-up boundary", crate::catchup::forget)
    }

    /// The zone body of one commit, with everything that can fail before the writer (encoding)
    /// already done.
    fn job(
        &self,
        batch: Vec<Arrival>,
        facts: Vec<BackendEvent>,
    ) -> Result<
        impl FnOnce(&Transaction<'_>) -> Result<Committed, StoreError> + Send + use<>,
        StoreError,
    > {
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let facts = if facts.is_empty() {
            None
        } else {
            Some(
                encode_outbox_row(&facts)
                    .map_err(|_| error::invalid("inbox facts could not be encoded"))?,
            )
        };
        let metas = batch
            .iter()
            .map(|a| serde_json::to_vec(&a.meta))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| error::invalid("inbox meta could not be encoded"))?;
        Ok(move |tx: &Transaction<'_>| {
            let boundary: Option<i64> = tx
                .query_row("SELECT boundary FROM catch_up WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(sql("read catch-up boundary"))?
                .flatten();
            let mut inserted = 0usize;
            {
                let mut insert = tx
                    .prepare_cached(
                        "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, \
                                           message, meta, state) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'new') \
                         ON CONFLICT (chat, sender, id) DO NOTHING",
                    )
                    .map_err(sql("insert inbox"))?;
                for (arrival, meta) in batch.iter().zip(&metas) {
                    let catch_up = boundary.is_none() || arrival.meta.is_offline;
                    inserted += insert
                        .execute(params![
                            arrival.chat,
                            arrival.sender,
                            arrival.id,
                            arrival.origin.as_str(),
                            now,
                            i64::from(catch_up),
                            arrival.message,
                            meta,
                        ])
                        .map_err(sql("insert inbox"))?;
                }
            }
            if let Some(row) = facts {
                outbox::append(tx, &row, now)
                    .map_err(|_| error::database("append inbox facts failed"))?;
            }
            // The seam of D3: with everything written and nothing committed, a test can make
            // the zone fail.
            fault::hit(fault::INBOX_COMMIT)?;
            Ok(Committed {
                total: batch.len(),
                inserted,
            })
        })
    }
}

#[cfg(test)]
mod tests;
