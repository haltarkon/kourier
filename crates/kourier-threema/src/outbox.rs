//! Writing core events into `core_outbox` (architecture §4.1 principle 6) and the forwarder that
//! ships them to the core (§5.6): Signal's `outbox.rs` over the same `Rc<ZoneDb>`, with this
//! crate's errors.
//!
//! An event is durable only together with the state change that implies it: [`append_events`]
//! takes the zone's [`Tx`]. The forwarder ships rows in `seq` order as one durable `Envelope` and
//! deletes them only after the core's `Ack(Ok)`; a row this crate cannot decode, or that the core
//! refuses for good, is quarantined (`core_outbox_poison`) and never blocks the account. `acked` says a
//! `seq` is SETTLED (applied or quarantined); [`refused`] tells the two apart (the join needs it for its
//! first row, P6).

use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope, decode_outbox_row, encode_outbox_row};
use kourier_db::{Batch, Forwarder, ForwarderConfig, OutboxSource as _, Rejected, Tx, ZoneDb};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{Result, StoreError};
use crate::join::events::ROW_EVENTS;

/// Appends `events` as one `core_outbox` row in this zone and wakes the forwarder once the zone has
/// committed. Returns the row's `seq`.
pub(crate) fn append_events(
    tx: &mut Tx<'_>,
    events: &[BackendEvent],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<i64> {
    let row = encode_outbox_row(events).map_err(|e| {
        tracing::error!(error = %e, "a core_outbox row could not be encoded");
        StoreError::Failed("encode core_outbox row failed")
    })?;
    let seq = tx.outbox_append(&row, now)?;
    let wake = wake.clone();
    tx.on_commit(move || wake.notify_one());
    Ok(seq)
}

/// Appends `events` IN ORDER as `core_outbox` rows of at most [`ROW_EVENTS`] events each, all in this zone
/// (TH5b S-L3, the rule of TH4's zone 2: the core ingests one row in one transaction, and a settings delta
/// that changes the blocked state of many stored contacts states one `ContactUpsert` each), so a kill leaves
/// every row or none. An event is never split (a re-map page's `HistoryBatch` is ONE event); no events, no row.
/// The batch zone (one envelope's events) and the re-map zone (one page's) append through it.
pub(crate) fn append_in_rows(
    tx: &mut Tx<'_>,
    events: &[BackendEvent],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<()> {
    for row in events.chunks(ROW_EVENTS) {
        append_events(tx, row, now, wake)?;
    }
    Ok(())
}

/// The highest `seq` in `core_outbox` (0 when empty).
pub(crate) fn max_seq(db: &ZoneDb) -> Result<i64> {
    db.read(|c| {
        c.query_row("SELECT coalesce(max(seq), 0) FROM core_outbox", [], |r| {
            r.get(0)
        })
        .map_err(crate::error::sql("read core_outbox failed"))
    })
}

/// Whether the row `seq` was QUARANTINED (the core refused it for good, or it did not decode) rather than
/// applied (P6). The forwarder publishes a settled `seq` on `acked` only AFTER the quarantine's commit
/// (and after the delete of an applied row), and `core_outbox.seq` is never reused (`AUTOINCREMENT`), so
/// once `acked` reached `seq` this is exact: the poison table holds the row, or the core applied it.
pub(crate) fn refused(db: &ZoneDb, seq: i64) -> Result<bool> {
    db.read(|c| {
        c.query_row(
            "SELECT EXISTS(SELECT 1 FROM core_outbox_poison WHERE seq = ?1)",
            [seq],
            |r| r.get(0),
        )
        .map_err(crate::error::sql("read core_outbox_poison failed"))
    })
}

/// Why a shipped batch was not committed by the core.
enum Ship {
    /// The core dropped its end of the events channel: the account is being stopped.
    CoreGone,
    /// The core did not commit; `retryable: false` is a refusal for good.
    Rejected { retryable: bool, detail: String },
}

/// The forwarder task: ships `core_outbox` rows in `seq` order as `Envelope{events, Ack}`, deletes
/// them after the core's `Ack(Ok)` and publishes the last settled `seq` (acked or quarantined) on
/// `acked`. Returns when `stop` fires or the core is gone. Retryable failures keep the rows and ship
/// them again after a capped backoff.
pub(crate) async fn forward(
    db: Rc<ZoneDb>,
    events: mpsc::Sender<Envelope>,
    wake: Rc<Notify>,
    acked: watch::Sender<i64>,
    stop: CancellationToken,
) {
    let cfg = ForwarderConfig::default();
    let forwarder = Forwarder::new(db.clone(), cfg.clone());
    let mut delay = cfg.retry_min;
    loop {
        let batch = match forwarder.next_batch() {
            Ok(Some(batch)) => batch,
            Ok(None) => {
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = wake.notified() => continue,
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "reading core_outbox failed");
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
        };
        let last = batch.last_seq();
        let decoded = match decode_batch(&db, &batch) {
            Ok(decoded) => decoded,
            Err(e) => {
                tracing::error!(error = %e, "quarantining an undecodable core_outbox row failed");
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_secs(1)) => continue,
                }
            }
        };
        if decoded.is_empty() {
            // Every row of the batch was undecodable and is quarantined now.
            acked.send_replace(last);
            continue;
        }
        let shipped = tokio::select! {
            () = stop.cancelled() => return,
            shipped = ship(&events, decoded) => shipped,
        };
        match shipped {
            Ok(()) => {
                if let Err(e) = forwarder.complete(last) {
                    // The rows stay and ship again (absorbed by the core's idempotent ingest);
                    // wait first, so a failing disk does not become a busy loop.
                    tracing::error!(error = %e, "deleting forwarded core_outbox rows failed");
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = tokio::time::sleep(Duration::from_secs(1)) => continue,
                    }
                }
                acked.send_replace(last);
                delay = cfg.retry_min;
            }
            Err(Ship::CoreGone) => return,
            Err(Ship::Rejected {
                retryable: false,
                detail,
            }) => {
                match forwarder.reject(&batch, &detail) {
                    Ok(Rejected::Quarantined { seq }) => {
                        acked.send_replace(seq);
                    }
                    Ok(Rejected::Isolating { .. }) => {}
                    Err(e) => {
                        tracing::error!(error = %e, "quarantining a refused core_outbox row failed");
                        tokio::select! {
                            () = stop.cancelled() => return,
                            () = tokio::time::sleep(delay) => {}
                        }
                    }
                }
                delay = cfg.retry_min;
            }
            Err(Ship::Rejected { .. }) => {
                tracing::warn!("the core did not ack an outbox batch; retrying");
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(cfg.retry_max);
            }
        }
    }
}

/// The events of a batch in order. A row that does not decode is moved to `core_outbox_poison` with
/// the codec's log-safe reason; the other rows still ship.
fn decode_batch(
    db: &ZoneDb,
    batch: &Batch,
) -> std::result::Result<Vec<BackendEvent>, kourier_db::Error> {
    let mut all = Vec::new();
    for row in batch.rows() {
        match decode_outbox_row(&row.envelope) {
            Ok(decoded) => all.extend(decoded),
            Err(e) => {
                let reason = format!("undecodable row: {e}");
                db.quarantine(row.seq, &reason)?;
                tracing::error!(seq = row.seq, %reason, "core_outbox row quarantined");
            }
        }
    }
    Ok(all)
}

/// One envelope for the core; `Ok` only after the core committed it.
async fn ship(
    events: &mpsc::Sender<Envelope>,
    all: Vec<BackendEvent>,
) -> std::result::Result<(), Ship> {
    let (envelope, waiter) = Envelope::durable(all);
    events.send(envelope).await.map_err(|_| Ship::CoreGone)?;
    waiter.wait().await.map_err(|e| Ship::Rejected {
        retryable: e.retryable,
        detail: e.detail,
    })
}
