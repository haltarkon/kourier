//! The generic `core_outbox` table and its forwarder (§4.1 principle 6, §5.6).
//!
//! Every backend's `backend.db` has `core_outbox(seq, envelope, created_at)`. A transaction that
//! changes protocol or mirrored state appends the events it implies here, *in the same
//! transaction* ([`append`], [`Tx::outbox_append`](crate::Tx::outbox_append)). One
//! [`Forwarder`] per account ships rows to the core in `seq` order and deletes them only after
//! the core acknowledged its commit. A crash anywhere in between re-forwards the rows, which the
//! core's idempotent ingest absorbs. Rows are opaque bytes here (a versioned serde
//! `Vec<BackendEvent>` written by the backend); this crate never decodes them.
//!
//! Poison policy: a row the core refuses for good ([`Refusal::Reject`]: a contract violation, an
//! undecodable row) must not block the account. The forwarder ships the rows of a rejected batch
//! one by one until it finds the refused row, moves that row to `core_outbox_poison` with a
//! log-safe reason ([`quarantine`]), logs an error and continues with the next row. Retryable
//! refusals ([`Refusal::Retry`]) keep the rows and ship them again after a capped backoff.
//!
//! Ids are never reused: `seq` is `AUTOINCREMENT` (migration 3), so a row appended after the
//! table emptied still sorts after, and never collides with, every row ever acked; poison rows
//! have their own `id`, so quarantining the same seq twice keeps both rows.

use crate::{Error, Migrations, Result, ZoneDb, failpoint, migration};
use rusqlite::{Connection, OptionalExtension};
use std::cell::Cell;
use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;
use tokio::sync::Notify;

/// The `core_outbox` schema, migrated by every backend next to its own migrations.
pub const MIGRATIONS: Migrations = Migrations::new(
    "core_outbox",
    &[
        migration!(1, "0001_core_outbox"),
        migration!(2, "0002_core_outbox_poison"),
        migration!(3, "0003_core_outbox_autoincrement"),
    ],
);

/// The kill point between the core's `Ack(Ok)` and the forwarder's `DELETE` (§10.4).
pub const FAILPOINT_AFTER_CORE_COMMIT: &str = "after_core_commit";

/// One `core_outbox` row.
#[derive(Clone, PartialEq, Eq)]
pub struct OutboxRow {
    /// Position in the outbox; rows ship in ascending order.
    pub seq: i64,
    /// Opaque, versioned envelope bytes.
    pub envelope: Vec<u8>,
    /// Milliseconds since the epoch when the row was written.
    pub created_at: i64,
}

impl std::fmt::Debug for OutboxRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print envelope bytes: they carry message content (§9.1).
        f.debug_struct("OutboxRow")
            .field("seq", &self.seq)
            .field("len", &self.envelope.len())
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Appends one envelope and returns its `seq`. Fails with [`Error::OutsideTransaction`] when
/// `conn` is not inside a transaction: an event is durable only together with the state change
/// that implies it.
pub fn append(conn: &Connection, envelope: &[u8], created_at: i64) -> Result<i64> {
    if conn.is_autocommit() {
        return Err(Error::OutsideTransaction);
    }
    conn.prepare_cached("INSERT INTO core_outbox(envelope, created_at) VALUES (?1, ?2)")?
        .execute((envelope, created_at))?;
    Ok(conn.last_insert_rowid())
}

/// Up to `limit` rows in `seq` order.
pub fn peek(conn: &Connection, limit: usize) -> Result<Vec<OutboxRow>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT seq, envelope, created_at FROM core_outbox ORDER BY seq LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(OutboxRow {
                seq: r.get(0)?,
                envelope: r.get(1)?,
                created_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Deletes every row with `seq <= through` and returns how many went.
pub fn delete_through(conn: &Connection, through: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached("DELETE FROM core_outbox WHERE seq <= ?1")?
        .execute([through])?)
}

/// Number of rows waiting.
pub fn len(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT count(*) FROM core_outbox", [], |r| r.get(0))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// One row the core refused for good, kept in `core_outbox_poison`.
#[derive(Clone, PartialEq, Eq)]
pub struct PoisonRow {
    /// Its own key (never reused).
    pub id: i64,
    /// Its former `core_outbox` seq.
    pub seq: i64,
    /// The opaque envelope bytes.
    pub envelope: Vec<u8>,
    /// When the row was written (ms).
    pub created_at: i64,
    /// When it was quarantined (ms).
    pub quarantined_at: i64,
    /// Why the core refused it (log-safe).
    pub reason: String,
}

impl std::fmt::Debug for PoisonRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print envelope bytes: they carry message content (§9.1).
        f.debug_struct("PoisonRow")
            .field("id", &self.id)
            .field("seq", &self.seq)
            .field("len", &self.envelope.len())
            .field("created_at", &self.created_at)
            .field("quarantined_at", &self.quarantined_at)
            .field("reason", &self.reason)
            .finish()
    }
}

/// Moves the row `seq` from `core_outbox` to `core_outbox_poison` with `reason` (log-safe) and
/// returns whether a row was moved. `conn` must be inside a transaction, so the copy and the
/// delete commit together. Every quarantine adds a poison row; none is ever overwritten.
pub fn quarantine(conn: &Connection, seq: i64, reason: &str, now: i64) -> Result<bool> {
    if conn.is_autocommit() {
        return Err(Error::OutsideTransaction);
    }
    let moved = conn
        .prepare_cached(
            "INSERT INTO core_outbox_poison(seq, envelope, created_at, quarantined_at, reason)
             SELECT seq, envelope, created_at, ?2, ?3 FROM core_outbox WHERE seq = ?1",
        )?
        .execute((seq, now, reason))?;
    conn.prepare_cached("DELETE FROM core_outbox WHERE seq = ?1")?
        .execute([seq])?;
    Ok(moved > 0)
}

/// Up to `limit` quarantined rows, in quarantine order.
pub fn poison(conn: &Connection, limit: usize) -> Result<Vec<PoisonRow>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT id, seq, envelope, created_at, quarantined_at, reason FROM core_outbox_poison
         ORDER BY id LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(PoisonRow {
                id: r.get(0)?,
                seq: r.get(1)?,
                envelope: r.get(2)?,
                created_at: r.get(3)?,
                quarantined_at: r.get(4)?,
                reason: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Number of quarantined rows.
pub fn poison_len(conn: &Connection) -> Result<u64> {
    let n: Option<i64> = conn
        .query_row("SELECT count(*) FROM core_outbox_poison", [], |r| r.get(0))
        .optional()?;
    Ok(n.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
}

/// Where a [`Forwarder`] reads and deletes rows: a [`ZoneDb`] (a backend actor's connection)
/// or a plain [`Connection`].
pub trait OutboxSource {
    /// Up to `limit` rows in `seq` order.
    fn peek(&self, limit: usize) -> Result<Vec<OutboxRow>>;
    /// Deletes every row with `seq <= through`.
    fn delete_through(&self, through: i64) -> Result<()>;
    /// Moves the row `seq` to `core_outbox_poison` in one transaction (see [`quarantine`]).
    fn quarantine(&self, seq: i64, reason: &str) -> Result<()>;
}

impl OutboxSource for Connection {
    fn peek(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        peek(self, limit)
    }

    fn delete_through(&self, through: i64) -> Result<()> {
        delete_through(self, through).map(drop)
    }

    fn quarantine(&self, seq: i64, reason: &str) -> Result<()> {
        let tx = self.unchecked_transaction()?;
        quarantine(&tx, seq, reason, crate::now_ms())?;
        tx.commit()?;
        Ok(())
    }
}

impl OutboxSource for ZoneDb {
    fn peek(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        self.read(|c| peek(c, limit))
    }

    fn delete_through(&self, through: i64) -> Result<()> {
        self.zone(|tx| delete_through(tx, through).map(drop))
    }

    fn quarantine(&self, seq: i64, reason: &str) -> Result<()> {
        self.zone(|tx| quarantine(tx, seq, reason, crate::now_ms()).map(drop))
    }
}

impl<T: OutboxSource + ?Sized> OutboxSource for &T {
    fn peek(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        (**self).peek(limit)
    }

    fn delete_through(&self, through: i64) -> Result<()> {
        (**self).delete_through(through)
    }

    fn quarantine(&self, seq: i64, reason: &str) -> Result<()> {
        (**self).quarantine(seq, reason)
    }
}

impl<T: OutboxSource + ?Sized> OutboxSource for std::rc::Rc<T> {
    fn peek(&self, limit: usize) -> Result<Vec<OutboxRow>> {
        (**self).peek(limit)
    }

    fn delete_through(&self, through: i64) -> Result<()> {
        (**self).delete_through(through)
    }

    fn quarantine(&self, seq: i64, reason: &str) -> Result<()> {
        (**self).quarantine(seq, reason)
    }
}

/// Consecutive rows shipped to the core as one `Envelope` with one `Ack`.
#[derive(Debug)]
pub struct Batch {
    rows: Vec<OutboxRow>,
}

impl Batch {
    /// The rows, in `seq` order (never empty).
    pub fn rows(&self) -> &[OutboxRow] {
        &self.rows
    }

    /// Takes the rows.
    pub fn into_rows(self) -> Vec<OutboxRow> {
        self.rows
    }

    /// The highest `seq` in the batch: the core's ack deletes through it.
    pub fn last_seq(&self) -> i64 {
        self.rows.last().map_or(0, |r| r.seq)
    }
}

/// Forwarder limits.
#[derive(Clone, Debug)]
pub struct ForwarderConfig {
    /// Rows per `Envelope` (§5.6: ≤ 100).
    pub max_rows: usize,
    /// First retry delay after the core did not ack.
    pub retry_min: Duration,
    /// Retry delay cap.
    pub retry_max: Duration,
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            max_rows: 100,
            retry_min: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
        }
    }
}

/// Why the core did not commit a shipped [`Batch`]: the sink's error in
/// [`Forwarder::run_with`] and [`Forwarder::drain_with`]. The text must be log-safe (never quote
/// the payload); it is logged and stored with a quarantined row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not committed for a reason that may pass (the core is busy or gone, a disk error, the ack
    /// was dropped): the same rows ship again after a capped backoff.
    Retry(String),
    /// Refused for good (a contract violation, an undecodable row): shipping the same rows again
    /// cannot help. A single row is quarantined; a batch of several rows is split so that only
    /// the refused row is quarantined and the others still reach the core.
    Reject(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(r) => write!(f, "not committed: {r}"),
            Self::Reject(r) => write!(f, "refused for good: {r}"),
        }
    }
}

/// What [`Forwarder::reject`] did with a refused batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The batch was one row: it moved to `core_outbox_poison`.
    Quarantined {
        /// Its former seq.
        seq: i64,
    },
    /// The batch had several rows: rows through `through` now ship one per batch, so the next
    /// rejection names the poison row exactly.
    Isolating {
        /// The last seq of the refused batch.
        through: i64,
    },
}

/// Ships `core_outbox` rows to the core in `seq` order and deletes them after the ack.
///
/// The sink passed to [`run_with`](Self::run_with) / [`drain_with`](Self::drain_with) turns a
/// [`Batch`] into the boundary's `Envelope`, hands it to the core and resolves `Ok` only after the
/// core's `Ack(Ok)`, i.e. after its `core.db` commit. [`Refusal::Retry`] means "not committed":
/// the same rows ship again after a backoff. [`Refusal::Reject`] applies the poison policy (see
/// the module docs): the forwarder never blocks on a row the core refuses for good. Batching is
/// natural: whatever accumulated while the previous envelope was in flight ships next, up to
/// [`ForwarderConfig::max_rows`], so an idle outbox adds no latency. Dropping a running
/// forwarder is safe at any await point: unacked rows stay and ship again.
///
/// A custom forwarding loop uses [`next_batch`](Self::next_batch), then
/// [`complete`](Self::complete) after the ack or [`reject`](Self::reject) after a permanent
/// refusal.
pub struct Forwarder<S> {
    source: S,
    cfg: ForwarderConfig,
    /// After a rejected batch of several rows: ship rows through this seq one at a time.
    isolate_through: Cell<Option<i64>>,
}

impl<S: OutboxSource> Forwarder<S> {
    /// A forwarder over `source`.
    pub fn new(source: S, cfg: ForwarderConfig) -> Self {
        Self {
            source,
            cfg,
            isolate_through: Cell::new(None),
        }
    }

    /// The next rows to ship, oldest first, or `None` when the outbox is empty. While a rejected
    /// batch is being split, each batch is a single row.
    pub fn next_batch(&self) -> Result<Option<Batch>> {
        let mut rows = self.source.peek(self.cfg.max_rows.max(1))?;
        let Some(first) = rows.first().map(|r| r.seq) else {
            return Ok(None);
        };
        match self.isolate_through.get() {
            Some(through) if first <= through => rows.truncate(1),
            Some(_) => self.isolate_through.set(None),
            None => {}
        }
        Ok(Some(Batch { rows }))
    }

    /// The core committed everything through `last_seq`: delete those rows.
    pub fn complete(&self, last_seq: i64) -> Result<()> {
        failpoint::hit(FAILPOINT_AFTER_CORE_COMMIT);
        self.source.delete_through(last_seq)
    }

    /// The core refused `batch` for good (`reason` is log-safe). One row: it is quarantined and
    /// an error is logged. Several rows: nothing is dropped yet; the following batches through
    /// the batch's last seq are single rows, so the next rejection names the poison row.
    pub fn reject(&self, batch: &Batch, reason: &str) -> Result<Rejected> {
        match batch.rows() {
            [row] => {
                self.source.quarantine(row.seq, reason)?;
                tracing::error!(
                    seq = row.seq,
                    reason,
                    "the core refused a core_outbox row for good; quarantined"
                );
                Ok(Rejected::Quarantined { seq: row.seq })
            }
            rows => {
                let through = batch.last_seq();
                tracing::warn!(
                    rows = rows.len(),
                    reason,
                    "the core refused an outbox batch; shipping its rows one by one"
                );
                self.isolate_through.set(Some(through));
                Ok(Rejected::Isolating { through })
            }
        }
    }

    /// Forwards forever: ships whatever is queued, then waits on `wake` (signal it with
    /// `notify_one` after every commit that appended rows, e.g. from
    /// [`Tx::on_commit`](crate::Tx::on_commit)). Returns only on a database error.
    pub async fn run_with<F, Fut>(&self, wake: &Notify, mut ship: F) -> Result<Infallible>
    where
        F: FnMut(Batch) -> Fut,
        Fut: Future<Output = std::result::Result<(), Refusal>>,
    {
        let mut delay = self.cfg.retry_min;
        loop {
            if !self.step(&mut ship, &mut delay).await? {
                wake.notified().await;
            }
        }
    }

    /// Forwards until the outbox is empty (shutdown and unlink drain, §4.3 step 5), retrying
    /// unacked batches and quarantining refused rows. Bound it with a timeout.
    pub async fn drain_with<F, Fut>(&self, mut ship: F) -> Result<()>
    where
        F: FnMut(Batch) -> Fut,
        Fut: Future<Output = std::result::Result<(), Refusal>>,
    {
        let mut delay = self.cfg.retry_min;
        while self.step(&mut ship, &mut delay).await? {}
        Ok(())
    }

    /// [`run_with`](Self::run_with) for a sink whose every error is retryable (it never
    /// quarantines).
    pub async fn run<F, Fut, E>(&self, wake: &Notify, mut ship: F) -> Result<Infallible>
    where
        F: FnMut(Batch) -> Fut,
        Fut: Future<Output = std::result::Result<(), E>>,
        E: std::fmt::Display,
    {
        self.run_with(wake, |b| retry_only(ship(b))).await
    }

    /// [`drain_with`](Self::drain_with) for a sink whose every error is retryable.
    pub async fn drain<F, Fut, E>(&self, mut ship: F) -> Result<()>
    where
        F: FnMut(Batch) -> Fut,
        Fut: Future<Output = std::result::Result<(), E>>,
        E: std::fmt::Display,
    {
        self.drain_with(|b| retry_only(ship(b))).await
    }

    /// Ships one batch: deletes it after the ack, waits a backoff after a retryable refusal,
    /// applies the poison policy after a permanent one. `false` when the outbox was empty.
    async fn step<F, Fut>(&self, ship: &mut F, delay: &mut Duration) -> Result<bool>
    where
        F: FnMut(Batch) -> Fut,
        Fut: Future<Output = std::result::Result<(), Refusal>>,
    {
        let Some(batch) = self.next_batch()? else {
            return Ok(false);
        };
        let last = batch.last_seq();
        let rows = batch.rows.len();
        // The sink consumes the batch; keep what a rejection needs.
        let keep = Batch {
            rows: batch
                .rows
                .iter()
                .map(|r| OutboxRow {
                    seq: r.seq,
                    envelope: Vec::new(),
                    created_at: r.created_at,
                })
                .collect(),
        };
        match ship(batch).await {
            Ok(()) => {
                self.complete(last)?;
                *delay = self.cfg.retry_min;
            }
            Err(Refusal::Retry(e)) => {
                tracing::warn!(rows, error = %e, "core did not ack an outbox batch; retrying");
                tokio::time::sleep(*delay).await;
                *delay = (*delay * 2).min(self.cfg.retry_max);
            }
            Err(Refusal::Reject(reason)) => {
                self.reject(&keep, &reason)?;
                *delay = self.cfg.retry_min;
            }
        }
        Ok(true)
    }
}

/// Maps every error of a legacy sink to [`Refusal::Retry`].
async fn retry_only<Fut, E>(fut: Fut) -> std::result::Result<(), Refusal>
where
    Fut: Future<Output = std::result::Result<(), E>>,
    E: std::fmt::Display,
{
    fut.await.map_err(|e| Refusal::Retry(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbKey, migrate};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn db() -> (tempfile::TempDir, Rc<ZoneDb>) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::open(&dir.path().join("b.db"), &DbKey::from([4; 32])).unwrap();
        migrate(&mut conn, &MIGRATIONS).unwrap();
        (dir, Rc::new(ZoneDb::new(conn)))
    }

    fn append_n(db: &ZoneDb, from: u32, n: u32) {
        db.zone(|tx| -> Result<()> {
            for i in from..from + n {
                tx.outbox_append(&i.to_le_bytes(), 0)?;
            }
            Ok(())
        })
        .unwrap();
    }

    fn decode(batch: &Batch) -> Vec<u32> {
        batch
            .rows()
            .iter()
            .map(|r| u32::from_le_bytes(r.envelope[..4].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn append_outside_a_transaction_is_refused() {
        let (_dir, db) = db();
        let conn = Rc::try_unwrap(db).ok().unwrap().into_inner();
        assert!(matches!(
            append(&conn, b"x", 0),
            Err(Error::OutsideTransaction)
        ));
    }

    #[tokio::test]
    async fn drain_ships_in_seq_order_in_bounded_batches_and_deletes_after_ack() {
        let (_dir, db) = db();
        append_n(&db, 0, 250);
        let cfg = ForwarderConfig {
            max_rows: 100,
            ..ForwarderConfig::default()
        };
        let fwd = Forwarder::new(db.clone(), cfg);
        let seen = RefCell::new(Vec::new());
        let sizes = RefCell::new(Vec::new());
        fwd.drain(|batch| {
            sizes.borrow_mut().push(batch.rows().len());
            seen.borrow_mut().extend(decode(&batch));
            async { Ok::<_, Infallible>(()) }
        })
        .await
        .unwrap();
        assert_eq!(*sizes.borrow(), [100, 100, 50]);
        assert_eq!(*seen.borrow(), (0..250).collect::<Vec<_>>());
        assert_eq!(db.read(len).unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn nack_keeps_rows_and_retries_the_same_batch() {
        let (_dir, db) = db();
        append_n(&db, 0, 3);
        let fwd = Forwarder::new(db.clone(), ForwarderConfig::default());
        let attempts = RefCell::new(Vec::new());
        fwd.drain(|batch| {
            let n = attempts.borrow().len();
            attempts.borrow_mut().push(decode(&batch));
            // The core fails twice (say, disk full), then commits.
            let db = db.clone();
            async move {
                assert_eq!(db.read(len).unwrap(), 3, "nothing deleted before the ack");
                if n < 2 { Err("persist failed") } else { Ok(()) }
            }
        })
        .await
        .unwrap();
        assert_eq!(*attempts.borrow(), vec![vec![0, 1, 2]; 3]);
        assert_eq!(db.read(len).unwrap(), 0);
    }

    /// Poison policy: the core refuses the batch holding row 2 for good. The rows of that batch
    /// ship one by one, only row 2 is quarantined (with its reason), every other row reaches the
    /// core in order, and batching resumes after the split batch.
    #[tokio::test]
    async fn a_refused_row_is_quarantined_and_the_rows_after_it_still_ship() {
        let (_dir, db) = db();
        append_n(&db, 0, 8);
        let cfg = ForwarderConfig {
            max_rows: 5,
            ..ForwarderConfig::default()
        };
        let fwd = Forwarder::new(db.clone(), cfg);
        let batches = RefCell::new(Vec::new());
        let committed = RefCell::new(Vec::new());
        fwd.drain_with(|batch| {
            let values = decode(&batch);
            batches.borrow_mut().push(values.clone());
            let refused = values.contains(&2);
            if !refused {
                committed.borrow_mut().extend(values);
            }
            async move {
                if refused {
                    Err(Refusal::Reject("invalid id in Message: empty".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            *batches.borrow(),
            [
                vec![0, 1, 2, 3, 4],
                vec![0],
                vec![1],
                vec![2],
                vec![3],
                vec![4],
                vec![5, 6, 7]
            ]
        );
        assert_eq!(*committed.borrow(), [0, 1, 3, 4, 5, 6, 7]);
        assert_eq!(db.read(len).unwrap(), 0);
        let poison = db.read(|c| poison(c, 10)).unwrap();
        assert_eq!(poison.len(), 1);
        assert_eq!(poison[0].envelope, 2u32.to_le_bytes());
        assert_eq!(poison[0].reason, "invalid id in Message: empty");
        assert!(!format!("{:?}", poison[0]).contains("envelope:"));
        assert_eq!(db.read(poison_len).unwrap(), 1);
    }

    /// A retryable refusal never quarantines: the same rows ship again after a doubling backoff.
    #[tokio::test(start_paused = true)]
    async fn a_retryable_refusal_keeps_the_rows_and_backs_off() {
        let (_dir, db) = db();
        append_n(&db, 0, 2);
        let fwd = Forwarder::new(db.clone(), ForwarderConfig::default());
        let times = RefCell::new(Vec::new());
        let start = tokio::time::Instant::now();
        fwd.drain_with(|batch| {
            times.borrow_mut().push((start.elapsed(), decode(&batch)));
            let n = times.borrow().len();
            async move {
                if n <= 3 {
                    Err(Refusal::Retry("database is locked".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        let times = times.into_inner();
        let at: Vec<u128> = times.iter().map(|(t, _)| t.as_millis()).collect();
        assert_eq!(at, [0, 100, 300, 700]);
        assert!(times.iter().all(|(_, v)| *v == [0, 1]));
        assert_eq!(db.read(poison_len).unwrap(), 0);
        assert_eq!(db.read(len).unwrap(), 0);
    }

    #[test]
    fn quarantine_needs_a_transaction_and_a_plain_connection_source_works() {
        let (_dir, db) = db();
        append_n(&db, 0, 2);
        let conn = Rc::try_unwrap(db).ok().unwrap().into_inner();
        assert!(matches!(
            quarantine(&conn, 1, "x", 0),
            Err(Error::OutsideTransaction)
        ));
        OutboxSource::quarantine(&conn, 1, "undecodable row").unwrap();
        assert_eq!(len(&conn).unwrap(), 1);
        assert_eq!(poison(&conn, 10).unwrap()[0].seq, 1);
        // Quarantining a seq that is gone moves nothing.
        let tx = conn.unchecked_transaction().unwrap();
        assert!(!quarantine(&tx, 1, "again", 0).unwrap());
        tx.commit().unwrap();
        assert_eq!(poison_len(&conn).unwrap(), 1);
    }

    fn seqs(conn: &Connection) -> Vec<i64> {
        peek(conn, 100).unwrap().iter().map(|r| r.seq).collect()
    }

    /// AUTOINCREMENT: once the table emptied (every row acked and deleted), new rows never get
    /// a seq that was used before, also across a reopen.
    #[test]
    fn seqs_never_repeat_after_the_table_empties() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.db");
        let key = DbKey::from([4; 32]);
        let mut conn = crate::open(&path, &key).unwrap();
        migrate(&mut conn, &MIGRATIONS).unwrap();
        let append_one = |conn: &Connection| {
            let tx = conn.unchecked_transaction().unwrap();
            let seq = append(&tx, b"x", 0).unwrap();
            tx.commit().unwrap();
            seq
        };
        let first: Vec<i64> = (0..3).map(|_| append_one(&conn)).collect();
        assert_eq!(first, [1, 2, 3]);
        assert_eq!(delete_through(&conn, 3).unwrap(), 3);
        assert_eq!(len(&conn).unwrap(), 0);
        assert_eq!(append_one(&conn), 4);
        delete_through(&conn, 4).unwrap();
        drop(conn);
        let conn = crate::open(&path, &key).unwrap();
        assert_eq!(append_one(&conn), 5);
        assert_eq!(seqs(&conn), [5]);
    }

    /// Quarantining the same seq twice (a row of an older build, or a replayed quarantine) keeps
    /// both poison rows: a later quarantine never overwrites an earlier one.
    #[test]
    fn a_later_quarantine_never_overwrites_an_earlier_one() {
        let (_dir, db) = db();
        let conn = Rc::try_unwrap(db).ok().unwrap().into_inner();
        for reason in ["first", "second"] {
            let tx = conn.unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO core_outbox(seq, envelope, created_at) VALUES (7, ?1, 0)",
                [reason.as_bytes()],
            )
            .unwrap();
            assert!(quarantine(&tx, 7, reason, 1).unwrap());
            tx.commit().unwrap();
        }
        let rows = poison(&conn, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].seq, rows[0].reason.as_str()), (7, "first"));
        assert_eq!((rows[1].seq, rows[1].reason.as_str()), (7, "second"));
        assert!(rows[0].id < rows[1].id);
        assert_eq!(rows[1].envelope, b"second");
    }

    /// A `backend.db` of an older build (schema 2) is rebuilt when opened: every row keeps its
    /// seq, quarantined rows get ids, and new seqs start above every seq on record, the
    /// quarantined ones included.
    #[test]
    fn migration_3_rebuilds_both_tables_and_keeps_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let key = DbKey::from([4; 32]);
        let mut conn = crate::open(&path, &key).unwrap();
        let v2 = Migrations::new(MIGRATIONS.component, &MIGRATIONS.steps[..2]);
        migrate(&mut conn, &v2).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for seq in [3, 4, 9] {
            tx.execute(
                "INSERT INTO core_outbox(seq, envelope, created_at) VALUES (?1, ?2, 5)",
                (seq, format!("row {seq}").into_bytes()),
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO core_outbox_poison(seq, envelope, created_at, quarantined_at, reason)
             VALUES (12, x'00', 1, 2, 'bad row')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(conn);

        let mut conn = crate::open(&path, &key).unwrap();
        let out = migrate(&mut conn, &MIGRATIONS).unwrap();
        assert_eq!((out.from, out.to), (2, 3));
        let rows = peek(&conn, 10).unwrap();
        assert_eq!(seqs(&conn), [3, 4, 9]);
        assert_eq!(rows[2].envelope, b"row 9");
        assert_eq!(rows[2].created_at, 5);
        let p = poison(&conn, 10).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].id, p[0].seq, p[0].reason.as_str()),
            (1, 12, "bad row")
        );
        delete_through(&conn, 9).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(append(&tx, b"new", 0).unwrap(), 13);
        tx.commit().unwrap();
        // Opening again migrates nothing.
        assert_eq!(migrate(&mut conn, &MIGRATIONS).unwrap().from, 3);
    }

    #[tokio::test]
    async fn run_wakes_on_notify_and_batches_what_accumulated() {
        let (_dir, db) = db();
        let wake = Rc::new(Notify::new());
        let got = Rc::new(RefCell::new(Vec::new()));
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let fwd_db = db.clone();
                let fwd_wake = wake.clone();
                let fwd_got = got.clone();
                let task = tokio::task::spawn_local(async move {
                    let fwd = Forwarder::new(fwd_db, ForwarderConfig::default());
                    fwd.run(&fwd_wake, |batch| {
                        fwd_got.borrow_mut().push(decode(&batch));
                        async { Ok::<_, Infallible>(()) }
                    })
                    .await
                });
                for round in 0u32..3 {
                    // A zone commits rows and wakes the forwarder from its on_commit hook.
                    let w = wake.clone();
                    db.zone(|tx| -> Result<()> {
                        tx.outbox_append(&(round * 2).to_le_bytes(), 0)?;
                        tx.outbox_append(&(round * 2 + 1).to_le_bytes(), 0)?;
                        tx.on_commit(move || w.notify_one());
                        Ok(())
                    })
                    .unwrap();
                    while db.read(len).unwrap() > 0 {
                        tokio::task::yield_now().await;
                    }
                }
                task.abort();
            })
            .await;
        assert_eq!(*got.borrow(), [vec![0, 1], vec![2, 3], vec![4, 5]]);
    }
}
