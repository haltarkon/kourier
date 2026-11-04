//! `core_outbox` for the Telegram backend (TG-2 brief §3.5, C7; critic M9): writing
//! `AccountReady` together with the account's user id, and shipping the rows to the core.
//!
//! The rules of the Signal and WhatsApp outboxes (architecture §5.6), over the actor's own
//! connection (the actor's thread owns it; nothing here is sent to another thread):
//!
//! - `AccountReady` is the ONLY durable event of TG-2. It is appended in the SAME transaction as
//!   `UPDATE account SET user_id` ([`append_ready`]), with kill points before
//!   ([`INSIDE_ACCOUNT_READY`]) and after ([`AFTER_ACCOUNT_READY`]) the commit: a crash leaves
//!   either neither or both.
//! - Rows ship in `seq` order as one durable [`Envelope`] ([`ship_pending`]) and are deleted only
//!   after the core's `Ack(Ok)`. A row left by a crash ships at the next actor start, before
//!   anything else; the core's idempotent ingest absorbs a repeated `AccountReady`.
//! - A row the core refuses for good is quarantined (`core_outbox_poison`), never shipped again; a
//!   row this crate cannot decode is quarantined at once. Retryable refusals keep the rows and ship
//!   them again after a capped back-off.
//! - Cancel safety (critic H1): [`ship_pending`] may be dropped at any await point (the actor's
//!   `select!` lost the race to a command): the rows stay until a later pass completes them.
//!
//! The rows carry the own ids, the number and the display name as the core reads them (plain
//! JSON, like every backend's `core_outbox`); they never reach a log line: errors name the
//! operation or the codec's log-safe class only.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope, decode_outbox_row, encode_outbox_row};
use kourier_db::rusqlite::Connection;
use kourier_db::{Batch, Forwarder, ForwarderConfig, OutboxSource, Rejected, failpoint, outbox};
use kourier_model::PersistError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::error::{StoreError, sql};

/// The kill point after `AccountReady` and the user id were written, before the commit.
pub(crate) const INSIDE_ACCOUNT_READY: &str = "tg-inside-account-ready";
/// The kill point right after the commit of `AccountReady` and the user id.
pub(crate) const AFTER_ACCOUNT_READY: &str = "tg-after-account-ready";
/// The kill point when receive rows are committed and read for shipping, nothing handed to the
/// core yet ([`Shipper::kick`], TG-3 C10a).
pub(crate) const BEFORE_SHIP: &str = "tg-before-ship";
/// The kill point when the core acked a shipped batch, its rows not yet deleted
/// ([`Shipper::settle`], TG-3 C10a).
pub(crate) const AFTER_ACK: &str = "tg-after-ack";

/// Appends `event` (the login's `AccountReady`) to `core_outbox` and sets the account's user id,
/// in ONE transaction; returns the row's `seq`. Refused (nothing written) when the event is not
/// storable, the account row is missing, or the row holds another user id.
pub(crate) fn append_ready(
    conn: &Connection,
    user_id: i64,
    event: &BackendEvent,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let row = encode_outbox_row(std::slice::from_ref(event))
        .map_err(|_| StoreError("encode AccountReady"))?;
    let tx = conn
        .unchecked_transaction()
        .map_err(sql("begin AccountReady"))?;
    db::set_user_id(&tx, user_id)?;
    let seq = outbox::append(&tx, &row, now_ms).map_err(|e| {
        tracing::error!(error = %e, "appending AccountReady failed");
        StoreError("append AccountReady")
    })?;
    failpoint::hit(INSIDE_ACCOUNT_READY);
    tx.commit().map_err(sql("commit AccountReady"))?;
    failpoint::hit(AFTER_ACCOUNT_READY);
    Ok(seq)
}

/// How a [`ship_pending`] pass ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShipEnd {
    /// The outbox is empty.
    Drained,
    /// The core's event channel is closed: the rows stay.
    CoreGone,
    /// The account's token fired: the rows stay.
    Cancelled,
    /// Reading or deleting rows failed (logged where it was mapped): the rows stay.
    Store(StoreError),
}

/// What one [`ship_pending`] pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shipped {
    /// The highest `seq` the core committed in this pass (`None`: nothing was acked).
    pub(crate) acked: Option<i64>,
    /// Rows moved to `core_outbox_poison` in this pass (undecodable, or refused for good).
    pub(crate) quarantined: u32,
    /// Why the pass ended.
    pub(crate) end: ShipEnd,
}

/// Ships every waiting `core_outbox` row to the core, oldest first, until the outbox is empty, the
/// core is gone or `cancel` fires. A row is deleted only after the core's `Ack(Ok)`. Dropping the
/// returned future at any await point leaves every unacked row for the next pass.
pub(crate) async fn ship_pending(
    conn: &Connection,
    events: &mpsc::Sender<Envelope>,
    cancel: &CancellationToken,
) -> Shipped {
    let cfg = ForwarderConfig::default();
    let retry_min = cfg.retry_min;
    let retry_max = cfg.retry_max;
    let mut delay = retry_min;
    let forwarder = Forwarder::new(conn, cfg);
    let mut shipped = Shipped {
        acked: None,
        quarantined: 0,
        end: ShipEnd::Drained,
    };
    loop {
        if cancel.is_cancelled() {
            shipped.end = ShipEnd::Cancelled;
            return shipped;
        }
        let batch = match forwarder.next_batch() {
            Ok(Some(batch)) => batch,
            Ok(None) => return shipped,
            Err(e) => {
                tracing::error!(error = %e, "reading core_outbox failed");
                shipped.end = ShipEnd::Store(StoreError("read core_outbox"));
                return shipped;
            }
        };
        let last = batch.last_seq();
        let decoded = match decode_batch(conn, &batch, &mut shipped.quarantined) {
            Ok(decoded) => decoded,
            Err(e) => {
                shipped.end = ShipEnd::Store(e);
                return shipped;
            }
        };
        if decoded.is_empty() {
            // Every row of the batch was undecodable and is quarantined now.
            continue;
        }
        let (envelope, waiter) = Envelope::durable(decoded);
        let sent = tokio::select! {
            () = cancel.cancelled() => None,
            sent = events.send(envelope) => Some(sent.is_ok()),
        };
        match sent {
            None => {
                shipped.end = ShipEnd::Cancelled;
                return shipped;
            }
            Some(false) => {
                shipped.end = ShipEnd::CoreGone;
                return shipped;
            }
            Some(true) => {}
        }
        let answer = tokio::select! {
            () = cancel.cancelled() => {
                shipped.end = ShipEnd::Cancelled;
                return shipped;
            }
            answer = waiter.wait() => answer,
        };
        match answer {
            Ok(()) => {
                if let Err(e) = forwarder.complete(last) {
                    // The rows stay and ship again at the next pass (absorbed by idempotent ingest).
                    tracing::error!(error = %e, "deleting forwarded core_outbox rows failed");
                    shipped.end = ShipEnd::Store(StoreError("delete core_outbox rows"));
                    return shipped;
                }
                shipped.acked = Some(last);
                delay = retry_min;
            }
            Err(e) if !e.retryable => match forwarder.reject(&batch, &e.detail) {
                Ok(Rejected::Quarantined { .. }) => shipped.quarantined += 1,
                Ok(Rejected::Isolating { .. }) => {}
                Err(e) => {
                    tracing::error!(error = %e, "quarantining a refused core_outbox row failed");
                    shipped.end = ShipEnd::Store(StoreError("quarantine core_outbox row"));
                    return shipped;
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "the core did not ack an outbox batch; retrying");
                tokio::select! {
                    () = cancel.cancelled() => {
                        shipped.end = ShipEnd::Cancelled;
                        return shipped;
                    }
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(retry_max);
            }
        }
    }
}

/// What the shipping arm's future in flight came to ([`Shipper`], TG-3 C7b, lead L-4).
pub(crate) enum Sent {
    /// The core answered the batch's ack.
    Acked(Result<(), PersistError>),
    /// The core's event channel is closed: the rows stay.
    CoreGone,
    /// A retry's back-off is over: the next pass reads the rows again.
    Waited,
}

/// The shipping arm's future: owns everything it needs (a clone of the event sender), polled by
/// the actor's `select!` across wakes. Dropping it leaves every unacked row for a later pass.
pub(crate) type ShipFuture = Pin<Box<dyn Future<Output = Sent>>>;

/// What settling one [`Sent`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Rows were acked and deleted, quarantined, or a retry waits: the arm goes on.
    More,
    /// The core is gone: the actor ends.
    CoreGone,
    /// Reading or deleting rows failed: the rows stay for the next start (or the next commit).
    Store,
}

/// Shipping as a wake arm (TG-3 C7b, lead L-4): the actor hands a batch to the core
/// synchronously ([`Shipper::kick`]) and polls the future of its ack ([`Shipper::future`],
/// `Wake::Acked`) beside the inbound frames, so the receive never waits for the core's commit.
/// One batch in flight, rows in `seq` order, the poison and retry rules of [`ship_pending`].
pub(crate) struct Shipper {
    forwarder: Forwarder<Rc<Connection>>,
    /// The batch whose ack is awaited (`None` while a retry's back-off runs).
    batch: Option<Batch>,
    /// The ack's future, or the back-off's.
    in_flight: Option<ShipFuture>,
    delay: Duration,
    retry_min: Duration,
    retry_max: Duration,
}

impl Shipper {
    /// A shipper over the actor's connection (nothing in flight).
    pub(crate) fn new(conn: Rc<Connection>) -> Self {
        let cfg = ForwarderConfig::default();
        let (retry_min, retry_max) = (cfg.retry_min, cfg.retry_max);
        Self {
            forwarder: Forwarder::new(conn, cfg),
            batch: None,
            in_flight: None,
            delay: retry_min,
            retry_min,
            retry_max,
        }
    }

    /// The future the actor's `select!` polls.
    pub(crate) fn future(&mut self) -> Option<&mut ShipFuture> {
        self.in_flight.as_mut()
    }

    /// Forgets what is in flight (the blocking pass of an `AccountReady` ships those rows again;
    /// the core's idempotent ingest takes the repeat).
    pub(crate) fn abandon(&mut self) {
        self.in_flight = None;
        self.batch = None;
    }

    /// Hands the next batch to the core when nothing is in flight and rows wait. The send itself
    /// is part of the future (a full event channel never blocks the loop).
    pub(crate) fn kick(
        &mut self,
        conn: &Connection,
        events: &mpsc::Sender<Envelope>,
    ) -> Result<(), StoreError> {
        if self.in_flight.is_some() {
            return Ok(());
        }
        loop {
            let batch = match self.forwarder.next_batch() {
                Ok(Some(batch)) => batch,
                Ok(None) => return Ok(()),
                Err(e) => {
                    tracing::error!(error = %e, "reading core_outbox failed");
                    return Err(StoreError("read core_outbox"));
                }
            };
            let mut quarantined = 0;
            let decoded = decode_batch(conn, &batch, &mut quarantined)?;
            if decoded.is_empty() {
                continue;
            }
            failpoint::hit(BEFORE_SHIP);
            let (envelope, waiter) = Envelope::durable(decoded);
            self.batch = Some(batch);
            // Handed over in the loop (brief §4.3); only a full event channel moves the send
            // into the future.
            self.in_flight = Some(match events.try_send(envelope) {
                Ok(()) => Box::pin(async move { Sent::Acked(waiter.wait().await) }),
                Err(mpsc::error::TrySendError::Full(envelope)) => {
                    let events = events.clone();
                    Box::pin(async move {
                        if events.send(envelope).await.is_err() {
                            return Sent::CoreGone;
                        }
                        Sent::Acked(waiter.wait().await)
                    })
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    Box::pin(std::future::ready(Sent::CoreGone))
                }
            });
            return Ok(());
        }
    }

    /// The future in flight came to `sent`: rows deleted after an ack, quarantined after a
    /// refusal for good, or a back-off before the same rows ship again.
    pub(crate) fn settle(&mut self, sent: Sent) -> Settled {
        self.in_flight = None;
        let batch = self.batch.take();
        match sent {
            Sent::CoreGone => Settled::CoreGone,
            Sent::Waited => Settled::More,
            Sent::Acked(answer) => {
                let Some(batch) = batch else {
                    return Settled::More;
                };
                match answer {
                    Ok(()) => {
                        failpoint::hit(AFTER_ACK);
                        if let Err(e) = self.forwarder.complete(batch.last_seq()) {
                            tracing::error!(error = %e, "deleting forwarded core_outbox rows failed");
                            return Settled::Store;
                        }
                        self.delay = self.retry_min;
                        Settled::More
                    }
                    Err(e) if !e.retryable => match self.forwarder.reject(&batch, &e.detail) {
                        Ok(_) => Settled::More,
                        Err(e) => {
                            tracing::error!(error = %e, "quarantining a refused core_outbox row failed");
                            Settled::Store
                        }
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "the core did not ack an outbox batch; retrying");
                        let delay = self.delay;
                        self.delay = (self.delay * 2).min(self.retry_max);
                        self.in_flight = Some(Box::pin(async move {
                            tokio::time::sleep(delay).await;
                            Sent::Waited
                        }));
                        Settled::More
                    }
                }
            }
        }
    }
}

/// The oldest `seq` still in `core_outbox` (`None`: empty).
pub(crate) fn oldest_seq(conn: &Connection) -> Result<Option<i64>, StoreError> {
    conn.query_row("SELECT min(seq) FROM core_outbox", [], |r| r.get(0))
        .map_err(sql("read core_outbox"))
}

/// The newest `seq` in `core_outbox` (`None`: empty).
pub(crate) fn newest_seq(conn: &Connection) -> Result<Option<i64>, StoreError> {
    conn.query_row("SELECT max(seq) FROM core_outbox", [], |r| r.get(0))
        .map_err(sql("read core_outbox"))
}

/// The events of a batch in order. A row that does not decode is moved to `core_outbox_poison`
/// with the codec's log-safe reason (counted in `quarantined`); the other rows still ship.
fn decode_batch(
    conn: &Connection,
    batch: &Batch,
    quarantined: &mut u32,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut all = Vec::new();
    for row in batch.rows() {
        match decode_outbox_row(&row.envelope) {
            Ok(decoded) => all.extend(decoded),
            Err(e) => {
                let reason = format!("undecodable row: {e}");
                <Connection as OutboxSource>::quarantine(conn, row.seq, &reason).map_err(|e| {
                    tracing::error!(error = %e, "quarantining an undecodable core_outbox row failed");
                    StoreError("quarantine core_outbox row")
                })?;
                tracing::error!(seq = row.seq, %reason, "core_outbox row quarantined");
                *quarantined += 1;
            }
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests;
