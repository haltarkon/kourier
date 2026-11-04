//! `core_outbox` for the WhatsApp backend (W2, chunk C4a): writing `AccountReady` in one store
//! zone, and the forwarder that ships the rows to the core.
//!
//! The same rules as `kourier-signal`'s outbox (architecture §5.6), over [`KourierStore`]'s writer
//! thread instead of a connection of the actor, so nothing here blocks the actor's thread:
//!
//! - A row is appended in the SAME transaction as the change that implies it, with kill points
//!   around the commit ([`failpoints::INSIDE_ACCOUNT_READY`], [`failpoints::AFTER_ACCOUNT_READY`]).
//! - Rows ship in `seq` order as one durable [`Envelope`]; they are deleted only after the core's
//!   `Ack(Ok)`. A crash anywhere re-ships them, which the core's idempotent ingest absorbs.
//! - A row the core refuses for good is quarantined (`core_outbox_poison`), never shipped again;
//!   a refused batch is shipped row by row to find which. A row this crate cannot decode is
//!   quarantined at once. Retryable refusals keep the rows and ship them again after a capped
//!   backoff.
//! - The last settled `seq` (acked or quarantined) is published on a `watch`, which is how the
//!   actor learns that the core has `AccountReady` (the login's `Done` waits for it).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    BackendEvent, BackendNotice, Envelope, decode_outbox_row, encode_outbox_row,
};
use kourier_db::outbox;
use kourier_model::PersistError;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use wacore::store::error::StoreError;

use crate::db::Points;
use crate::error;
use crate::link::Ready;
use crate::store::KourierStore;

/// Rows per envelope (architecture §5.6: at most 100).
const BATCH: usize = 100;
/// The first and the longest wait after the core did not ack.
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The `AccountReady` of `ready`, as the core is told.
pub(crate) fn account_ready(ready: &Ready) -> BackendEvent {
    BackendEvent::AccountReady {
        own_ids: ready.own_ids.clone(),
        device: Some(u32::from(ready.device)),
        display_name: None,
        number: Some(ready.number.clone()),
    }
}

/// Appends `AccountReady` to `core_outbox` in one zone and returns its `seq`. The caller wakes the
/// forwarder after the commit.
pub(crate) async fn append_ready(
    store: &KourierStore,
    ready: &Ready,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let row = encode_outbox_row(&[account_ready(ready)])
        .map_err(|_| error::invalid("AccountReady could not be encoded"))?;
    let points = Points {
        inside: crate::failpoints::INSIDE_ACCOUNT_READY,
        after: crate::failpoints::AFTER_ACCOUNT_READY,
    };
    store
        .write_at("append AccountReady", points, move |tx| {
            // The link session asks no snapshot of Kourier's: the books carry the time of the pairing (F6).
            crate::appstate::startup::link_in(tx, now_ms)?;
            // And the blocklist's stamp (W6a2, Q1): a new account's first `Online` asks the list.
            crate::store::people::forget_blocklist_stamp(tx)?;
            outbox::append(tx, &row, now_ms)
                .map_err(|_| error::database("append AccountReady failed"))
        })
        .await
}

/// [`append_ready`] for a `Rebuild` (critic M4): the rebuilt core lost every chat's features, so the statements are
/// forgotten in the zone that states `AccountReady` again; the start sweep (`features::sweep`) states them anew.
/// The call log comes back too (fix F5, DUR-2): the history's kept call records are due again (the next quiet step
/// places them) and the call log's collection is marked dirty, so the start's resync replays what the phone's log
/// still holds, by the usual spacing.
pub(crate) async fn append_ready_again(
    store: &KourierStore,
    ready: &Ready,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let row = encode_outbox_row(&[account_ready(ready)])
        .map_err(|_| error::invalid("AccountReady could not be encoded"))?;
    let points = Points {
        inside: crate::failpoints::INSIDE_ACCOUNT_READY,
        after: crate::failpoints::AFTER_ACCOUNT_READY,
    };
    let activity = store.activity().clone();
    store
        .write_at("append AccountReady again", points, move |tx| {
            crate::send::started::forget_statements(tx)?;
            // And what the phone said about the chats (W6a1, H5): the sweep states each chat with its settings.
            crate::store::chat_state::forget_statements(tx)?;
            // And what the core was told of groups and people (W6a2, B3): the blocklist, the names, the merges and the
            // groups' state; the start's reads state each anew.
            crate::store::people::forget_statements(tx)?;
            // And the call log (F5): the records the history kept, and the live log's collection (upstream's own
            // name for it, `appstate::map::CALL_LOG`: marked dirty so the next start's resync replays the phone's log
            // into the rebuilt core; G5, one name).
            crate::history::deliver::retake_calls(tx)?;
            crate::store::redirty_in(tx, &activity, &[crate::appstate::map::CALL_LOG.to_owned()])?;
            outbox::append(tx, &row, now_ms)
                .map_err(|_| error::database("append AccountReady failed"))
        })
        .await
}

/// The start sweep of the chats' features (W5a, D9): the statements that are missing or stale, appended in ONE
/// zone with the marks that say they were stated. The count of statements; the caller wakes nothing (the
/// forwarder starts after it).
pub(crate) async fn append_statements(
    store: &KourierStore,
    now_ms: i64,
) -> Result<usize, StoreError> {
    store
        .write("state the chats' features", move |tx| {
            let mut stated = 0;
            for events in crate::features::sweep(tx)? {
                stated += events.len();
                let row = encode_outbox_row(&events)
                    .map_err(|_| error::invalid("a features statement could not be encoded"))?;
                outbox::append(tx, &row, now_ms)
                    .map_err(|_| error::database("append a features statement failed"))?;
            }
            Ok(stated)
        })
        .await
}

/// Appends a notice for the person to `core_outbox` in a zone of its own and returns its `seq` (durable: the core has it
/// before the UI shows it). The caller wakes the forwarder after the commit.
pub(crate) async fn append_notice(
    store: &KourierStore,
    notice: BackendNotice,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let row = encode_outbox_row(&[BackendEvent::Notice(notice)])
        .map_err(|_| error::invalid("the notice could not be encoded"))?;
    store
        .write("append a notice", move |tx| {
            outbox::append(tx, &row, now_ms).map_err(|_| error::database("append a notice failed"))
        })
        .await
}

/// Appends the answer to one download request (`AttachmentDone`) to `core_outbox` in a zone of its own and
/// returns its `seq`. The caller wakes the forwarder after the commit. A kill before the commit leaves no
/// row and the core asks again at its next start (it resends an unanswered download); a kill after it
/// leaves the row, which the forwarder ships at the next start (W3b, chunk C4c).
pub(crate) async fn append_download_answer(
    store: &KourierStore,
    answer: BackendEvent,
    now_ms: i64,
) -> Result<i64, StoreError> {
    let row = encode_outbox_row(&[answer])
        .map_err(|_| error::invalid("the download answer could not be encoded"))?;
    let points = Points {
        inside: crate::failpoints::INSIDE_DOWNLOAD_ANSWER,
        after: crate::failpoints::AFTER_DOWNLOAD_ANSWER,
    };
    store
        .write_at("append download answer", points, move |tx| {
            outbox::append(tx, &row, now_ms)
                .map_err(|_| error::database("append download answer failed"))
        })
        .await
}

/// The core asked for `user` to be refreshed (`RefreshContact`): what this backend holds of him is stated again, in one zone
/// ([`crate::book::restate`]), and its `seq` is returned (`None`: nothing to say). Nothing is asked of the network. The
/// caller wakes the forwarder after the commit.
pub(crate) async fn append_restated_contact(
    store: &KourierStore,
    user: kourier_model::UserId,
    now_ms: i64,
) -> Result<Option<i64>, StoreError> {
    store
        .write("restate a contact", move |tx| {
            let events = crate::book::restate(tx, &user)?;
            if events.is_empty() {
                return Ok(None);
            }
            let row = encode_outbox_row(&events)
                .map_err(|_| error::invalid("the contact could not be encoded"))?;
            outbox::append(tx, &row, now_ms)
                .map(Some)
                .map_err(|_| error::database("append a restated contact failed"))
        })
        .await
}

/// The highest `seq` in `core_outbox` (0 when empty).
pub(crate) async fn max_seq(store: &KourierStore) -> Result<i64, StoreError> {
    store
        .read(|conn| {
            conn.query_row("SELECT coalesce(max(seq), 0) FROM core_outbox", [], |r| {
                r.get(0)
            })
            .map_err(error::sql("read core_outbox"))
        })
        .await
}

/// How many rows up to `seq` are still waiting for the core.
async fn waiting(store: &KourierStore, seq: i64) -> Result<i64, StoreError> {
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM core_outbox WHERE seq <= ?1",
                [seq],
                |r| r.get(0),
            )
            .map_err(error::sql("read core_outbox"))
        })
        .await
}

/// Waits until the core acked (or the poison policy settled) every row up to `seq`, or the
/// forwarder is gone.
pub(crate) async fn wait_acked(store: &KourierStore, mut acked: watch::Receiver<i64>, seq: i64) {
    loop {
        match waiting(store, seq).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if acked.changed().await.is_err() {
            return;
        }
    }
}

/// Why a batch did not reach the core.
enum Shipped {
    Acked,
    /// The core is gone.
    Gone,
    Refused(PersistError),
}

async fn ship(events: &mpsc::Sender<Envelope>, all: Vec<BackendEvent>) -> Shipped {
    let (envelope, waiter) = Envelope::durable(all);
    if events.send(envelope).await.is_err() {
        return Shipped::Gone;
    }
    match waiter.wait().await {
        Ok(()) => Shipped::Acked,
        Err(e) => Shipped::Refused(e),
    }
}

/// Sleeps `d` unless `stop` fires first; true when it did.
async fn pause(stop: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        () = stop.cancelled() => true,
        () = tokio::time::sleep(d) => false,
    }
}

/// Moves the row `seq` to `core_outbox_poison` with a log-safe `reason`.
async fn quarantine(store: &KourierStore, seq: i64, reason: &str) {
    let reason = reason.to_owned();
    let moved = store
        .write("quarantine core_outbox row", move |tx| {
            outbox::quarantine(tx, seq, &reason, kourier_db::now_ms())
                .map_err(|_| error::database("quarantine core_outbox row failed"))
        })
        .await;
    match moved {
        Ok(_) => tracing::error!(seq, "core_outbox row quarantined"),
        Err(e) => tracing::error!(seq, error = %e, "quarantining a core_outbox row failed"),
    }
}

/// Deletes every row up to `through` (the core committed them).
async fn delete_through(store: &KourierStore, through: i64) {
    let deleted = store
        .write("delete forwarded core_outbox rows", move |tx| {
            outbox::delete_through(tx, through)
                .map_err(|_| error::database("delete forwarded core_outbox rows failed"))
        })
        .await;
    if let Err(e) = deleted {
        // The rows stay and ship again (absorbed by idempotent ingest).
        tracing::error!(error = %e, "deleting forwarded core_outbox rows failed");
    }
}

/// The forwarder task: ships `core_outbox` in order, publishes the last settled `seq` on `acked`.
/// Returns when `stop` fires or the core is gone.
pub(crate) async fn forward(
    store: KourierStore,
    events: mpsc::Sender<Envelope>,
    wake: Arc<Notify>,
    acked: watch::Sender<i64>,
    stop: CancellationToken,
) {
    let mut delay = RETRY_MIN;
    loop {
        let rows = match store
            .read(|conn| outbox::peek(conn, BATCH).map_err(|_| error::database("read core_outbox")))
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "reading core_outbox failed");
                if pause(&stop, Duration::from_secs(1)).await {
                    return;
                }
                continue;
            }
        };
        if rows.is_empty() {
            tokio::select! {
                () = stop.cancelled() => return,
                () = wake.notified() => continue,
            }
        }
        let last = rows.last().map_or(0, |r| r.seq);
        // The rows that decode; the others are quarantined now.
        let mut good: Vec<(i64, Vec<BackendEvent>)> = Vec::new();
        for row in &rows {
            match decode_outbox_row(&row.envelope) {
                Ok(decoded) => good.push((row.seq, decoded)),
                Err(e) => quarantine(&store, row.seq, &format!("undecodable row: {e}")).await,
            }
        }
        if good.is_empty() {
            acked.send_replace(last);
            continue;
        }
        let batch: Vec<BackendEvent> = good.iter().flat_map(|(_, e)| e.clone()).collect();
        let shipped = tokio::select! {
            () = stop.cancelled() => return,
            shipped = ship(&events, batch) => shipped,
        };
        match shipped {
            Shipped::Acked => {
                delete_through(&store, last).await;
                acked.send_replace(last);
                delay = RETRY_MIN;
            }
            Shipped::Gone => return,
            Shipped::Refused(e) if !e.retryable && good.len() == 1 => {
                // One row: it is the refused one.
                quarantine(&store, good[0].0, &e.detail).await;
                acked.send_replace(good[0].0);
            }
            Shipped::Refused(e) if !e.retryable => {
                tracing::warn!(
                    rows = good.len(),
                    "the core refused an outbox batch; shipping its rows one by one"
                );
                // The refused row is the one the core refuses alone; the rows before it are acked.
                let mut retry_later = false;
                for (seq, decoded) in good {
                    let one = tokio::select! {
                        () = stop.cancelled() => return,
                        one = ship(&events, decoded) => one,
                    };
                    match one {
                        Shipped::Acked => {
                            delete_through(&store, seq).await;
                            acked.send_replace(seq);
                        }
                        Shipped::Gone => return,
                        Shipped::Refused(e) if !e.retryable => {
                            quarantine(&store, seq, &e.detail).await;
                            acked.send_replace(seq);
                        }
                        // Retryable: leave the row; the loop ships it again after the pause.
                        Shipped::Refused(_) => {
                            retry_later = true;
                            break;
                        }
                    }
                }
                if retry_later {
                    if pause(&stop, delay).await {
                        return;
                    }
                    delay = (delay * 2).min(RETRY_MAX);
                }
            }
            Shipped::Refused(e) => {
                tracing::warn!(detail = %e.detail, "the core did not ack an outbox batch; retrying");
                if pause(&stop, delay).await {
                    return;
                }
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
    }
}
