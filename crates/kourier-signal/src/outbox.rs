//! Writing core events into `core_outbox` (§4.1 principle 6) and the forwarder that ships them
//! to the core (§5.6).

use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope, decode_outbox_row, encode_outbox_row};
use kourier_db::{Batch, Forwarder, ForwarderConfig, OutboxSource as _, Rejected, Tx, ZoneDb};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

/// Appends the durable `events` implied by this zone's state change as one `core_outbox` row,
/// and wakes the forwarder once the zone has committed. Returns the row's `seq`.
pub(crate) fn append_events(
    tx: &mut Tx<'_>,
    events: &[BackendEvent],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<i64> {
    let row = encode_outbox_row(events)?;
    let seq = tx.outbox_append(&row, now)?;
    let wake = wake.clone();
    tx.on_commit(move || wake.notify_one());
    Ok(seq)
}

/// Appends several `core_outbox` rows in this zone, one per entry of `rows` (empty entries are
/// skipped), and wakes the forwarder once after the commit. Stage 2 writes one row per inbox
/// row, so a row the core refuses for good quarantines one message, not a whole lane step.
pub(crate) fn append_event_rows(
    tx: &mut Tx<'_>,
    rows: &[Vec<BackendEvent>],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<usize> {
    let mut written = 0;
    for events in rows.iter().filter(|events| !events.is_empty()) {
        tx.outbox_append(&encode_outbox_row(events)?, now)?;
        written += 1;
    }
    if written > 0 {
        let wake = wake.clone();
        tx.on_commit(move || wake.notify_one());
    }
    Ok(written)
}

/// The highest `seq` in `core_outbox` (0 when empty).
pub(crate) fn max_seq(db: &ZoneDb) -> Result<i64> {
    db.read(|c| -> Result<i64> {
        Ok(
            c.query_row("SELECT coalesce(max(seq), 0) FROM core_outbox", [], |r| {
                r.get(0)
            })?,
        )
    })
}

/// The forwarder task: ships `core_outbox` rows in `seq` order as `Envelope{events, Ack}` and
/// deletes them after the core's `Ack(Ok)`; publishes the last settled `seq` (acked, or
/// quarantined) on `acked`. Returns when `stop` fires or the core is gone.
///
/// Poison policy (§13 phase C, contract rule on [`kourier_backend_api::Ack`]): a row this crate
/// cannot decode is quarantined at once (`core_outbox_poison`, never silently dropped); a batch
/// the core refuses for good goes to [`Forwarder::reject`], which ships its rows one by one and
/// quarantines the refused one, so a poison row never blocks the account. Retryable failures
/// keep the rows and ship them again after a capped backoff.
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
                    // The rows stay and ship again (absorbed by idempotent ingest); wait first,
                    // so a failing disk does not turn into a busy loop of re-sent envelopes.
                    tracing::error!(error = %e, "deleting forwarded core_outbox rows failed");
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = tokio::time::sleep(Duration::from_secs(1)) => continue,
                    }
                }
                acked.send_replace(last);
                delay = cfg.retry_min;
            }
            Err(Error::CoreGone) => return,
            Err(Error::CoreRejected {
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
            Err(e) => {
                tracing::warn!(error = %e, "core did not ack an outbox batch; retrying");
                tokio::select! {
                    () = stop.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(cfg.retry_max);
            }
        }
    }
}

/// The events of a batch in order. A row that does not decode (corruption, or a format this
/// version does not know) is moved to `core_outbox_poison` with the codec's log-safe reason;
/// the other rows still ship.
fn decode_batch(db: &ZoneDb, batch: &Batch) -> Result<Vec<BackendEvent>> {
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
async fn ship(events: &mpsc::Sender<Envelope>, all: Vec<BackendEvent>) -> Result<()> {
    let (envelope, waiter) = Envelope::durable(all);
    events.send(envelope).await.map_err(|_| Error::CoreGone)?;
    waiter.wait().await.map_err(|e| Error::CoreRejected {
        retryable: e.retryable,
        detail: e.detail,
    })
}

#[cfg(test)]
mod tests {
    use kourier_model::PersistError;
    use tokio::task::{LocalSet, spawn_local};

    use super::*;

    fn rows(db: &ZoneDb) -> i64 {
        db.read(|c| -> Result<i64> {
            Ok(c.query_row("SELECT count(*) FROM core_outbox", [], |r| r.get(0))?)
        })
        .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rows_leave_only_after_the_core_acked_and_a_rejection_ships_them_again() {
        LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let db = Rc::new(
                    crate::db::open(
                        &dir.path().join("backend.db"),
                        &kourier_backend_api::DbKey::new([9; 32]),
                    )
                    .unwrap(),
                );
                let wake = Rc::new(Notify::new());
                let event = BackendEvent::AccountReady {
                    own_ids: Vec::new(),
                    device: Some(2),
                    display_name: None,
                    number: None,
                };
                let seq = db
                    .zone(|tx| append_events(tx, std::slice::from_ref(&event), 1, &wake))
                    .unwrap();
                assert_eq!(max_seq(&db).unwrap(), seq);

                let (events, mut core) = mpsc::channel(4);
                let (acked_tx, mut acked) = watch::channel(0);
                let stop = CancellationToken::new();
                let task = spawn_local(forward(
                    db.clone(),
                    events,
                    wake.clone(),
                    acked_tx,
                    stop.clone(),
                ));

                // The core fails to commit: the row stays and nothing counts as acked.
                let first = core.recv().await.unwrap();
                assert_eq!(first.events, std::slice::from_ref(&event));
                first.ack.unwrap().complete(Err(PersistError {
                    retryable: true,
                    detail: "busy".to_owned(),
                }));
                let again = core.recv().await.unwrap();
                assert_eq!(rows(&db), 1, "kept until the core commits");
                assert_eq!(*acked.borrow_and_update(), 0);

                // The same row again; this time the core commits it.
                assert_eq!(again.events, [event]);
                again.ack.unwrap().complete(Ok(()));
                acked.wait_for(|s| *s == seq).await.unwrap();
                assert_eq!(rows(&db), 0, "deleted after the ack");

                stop.cancel();
                task.await.unwrap();
            })
            .await;
    }

    /// A minimal outbox setup: db, wake, the forwarder task and the fake core's receiver.
    struct Setup {
        _dir: tempfile::TempDir,
        db: Rc<ZoneDb>,
        wake: Rc<Notify>,
        core: mpsc::Receiver<Envelope>,
        acked: watch::Receiver<i64>,
        stop: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let db = Rc::new(
            crate::db::open(
                &dir.path().join("backend.db"),
                &kourier_backend_api::DbKey::new([8; 32]),
            )
            .unwrap(),
        );
        let wake = Rc::new(Notify::new());
        let (events, core) = mpsc::channel(4);
        let (acked_tx, acked) = watch::channel(0);
        let stop = CancellationToken::new();
        let task = spawn_local(forward(
            db.clone(),
            events,
            wake.clone(),
            acked_tx,
            stop.clone(),
        ));
        Setup {
            _dir: dir,
            db,
            wake,
            core,
            acked,
            stop,
            task,
        }
    }

    fn caps(n: u64) -> BackendEvent {
        BackendEvent::AccountCaps(kourier_model::AccountCaps {
            max_attachment_bytes: Some(n),
            ..Default::default()
        })
    }

    fn poison(db: &ZoneDb) -> Vec<kourier_db::PoisonRow> {
        db.read(|c| kourier_db::outbox::poison(c, 10)).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_row_the_core_refuses_for_good_is_quarantined_and_the_rest_still_ships() {
        LocalSet::new()
            .run_until(async {
                let mut s = setup();
                s.db.zone(|tx| {
                    append_event_rows(
                        tx,
                        &[vec![caps(1)], vec![caps(2)], vec![caps(3)]],
                        1,
                        &s.wake,
                    )
                })
                .unwrap();

                // The batch of three is refused for good: nothing is dropped yet.
                let first = s.core.recv().await.unwrap();
                assert_eq!(first.events, [caps(1), caps(2), caps(3)]);
                first.ack.unwrap().complete(Err(PersistError {
                    retryable: false,
                    detail: "contract violation".to_owned(),
                }));
                // Rows ship one by one now; the second is the poison row.
                let one = s.core.recv().await.unwrap();
                assert_eq!(one.events, [caps(1)]);
                one.ack.unwrap().complete(Ok(()));
                let two = s.core.recv().await.unwrap();
                assert_eq!(two.events, [caps(2)]);
                two.ack.unwrap().complete(Err(PersistError {
                    retryable: false,
                    detail: "contract violation".to_owned(),
                }));
                let three = s.core.recv().await.unwrap();
                assert_eq!(three.events, [caps(3)], "the account is not blocked");
                three.ack.unwrap().complete(Ok(()));
                s.acked.wait_for(|seq| *seq == 3).await.unwrap();
                assert_eq!(rows(&s.db), 0);
                let quarantined = poison(&s.db);
                assert_eq!(quarantined.len(), 1, "{quarantined:?}");
                assert_eq!(quarantined[0].seq, 2);
                assert!(quarantined[0].reason.contains("contract violation"));
                s.stop.cancel();
                s.task.await.unwrap();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_undecodable_row_is_quarantined_not_dropped() {
        LocalSet::new()
            .run_until(async {
                let mut s = setup();
                s.db.zone(|tx| -> Result<()> {
                    append_event_rows(tx, &[vec![caps(1)]], 1, &s.wake)?;
                    tx.outbox_append(b"\x09\x00not json", 1)?;
                    append_event_rows(tx, &[vec![caps(3)]], 1, &s.wake)?;
                    Ok(())
                })
                .unwrap();
                let envelope = s.core.recv().await.unwrap();
                assert_eq!(
                    envelope.events,
                    [caps(1), caps(3)],
                    "the decodable rows ship"
                );
                envelope.ack.unwrap().complete(Ok(()));
                s.acked.wait_for(|seq| *seq == 3).await.unwrap();
                assert_eq!(rows(&s.db), 0);
                let quarantined = poison(&s.db);
                assert_eq!(quarantined.len(), 1);
                assert_eq!(quarantined[0].seq, 2);
                assert_eq!(
                    quarantined[0].envelope, b"\x09\x00not json",
                    "kept, not lost"
                );
                assert!(quarantined[0].reason.starts_with("undecodable row"));

                // A batch of only undecodable rows settles too (waiters see progress). The table
                // is empty, so the seq starts at 1 again.
                let seq =
                    s.db.zone(|tx| tx.outbox_append(b"", 1).map_err(crate::error::Error::from))
                        .unwrap();
                s.wake.notify_one();
                s.acked.wait_for(|acked| *acked == seq).await.unwrap();
                assert_eq!(poison(&s.db).len(), 2);
                s.stop.cancel();
                s.task.await.unwrap();
            })
            .await;
    }
}
