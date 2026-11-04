//! `KourierStore`: the whole store contract of whatsapp-rust (`wacore::store::traits::Backend`) over
//! one SQLCipher file per account.
//!
//! One connection lives on a `kourier_db::Writer` thread. Every trait method copies its borrowed
//! arguments into owned values and submits ONE job; the job is the whole operation, so a batch is
//! one transaction and a read-modify-write is one job (never a read job followed by a write job).
//! One thread means one serial history: the merges and compare-and-swaps are atomic without a
//! retry loop and no `SQLITE_BUSY` can happen. Blocking rusqlite never runs on a tokio worker.
//!
//! Who owns what: the ACTOR owns the [`Writer`] through a [`StoreOwner`]; a [`KourierStore`] holds a
//! [`WriterHandle`] only, so a client task that drops the last clone never joins a thread. The actor
//! closes the owner after the client released the store (`close`): jobs already queued finish,
//! later calls fail with "store closed" and never wait.
//!
//! A caller that drops its future does not cancel the job: it runs to its end on the writer thread,
//! all or nothing. That is what "cancellation, including detached backend I/O, must leave the row
//! unchanged" asks of a read, and "either the previous state or the complete patch" of a write.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use kourier_db::rusqlite::{Connection, OptionalExtension, Params, Row, Transaction};
use kourier_db::{DbKey, Priority, Writer, WriterHandle};
use wacore::store::error::StoreError;

use crate::appstate::Activity;
use crate::db::{self, Points};
use crate::error::{self, OpenError, sql};
use crate::hold::LinkHold;

mod appsync;
pub(crate) mod book;
pub(crate) use appsync::redirty_in;
pub(crate) mod call_lines;
pub(crate) mod chat_state;
mod device;
pub(crate) use device::own_forms;
pub(crate) mod merges;
mod pending;
pub(crate) mod people;
mod protocol;
pub(crate) use protocol::keep_history_token;
pub(crate) mod resyncs;
pub(crate) mod secret;
mod signal;
pub(crate) mod strangers;

/// The one lane of the writer (one account per file).
const LANE: u64 = 0;

/// The longest a thread that is no task waits for a read ([`KourierStore::read_blocking`]).
const BLOCKING_READ: std::time::Duration = std::time::Duration::from_secs(10);

/// How many prepared statements the connection keeps.
const STATEMENTS: usize = 128;

/// The store of one account. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct KourierStore {
    inner: Arc<Inner>,
}

struct Inner {
    handle: WriterHandle,
    /// What moved in the app-state path (rule (b), `crate::appstate`), shared by every clone.
    activity: Arc<Activity>,
    /// Whether a link without an account keeps the receive path from `core_outbox` (`crate::hold`),
    /// shared by every clone.
    hold: Arc<LinkHold>,
}

/// Owns the writer thread of one account's store; the actor holds it. Dropping it finishes the queued
/// jobs and joins the thread, like [`StoreOwner::close`].
pub struct StoreOwner {
    writer: Writer,
    store: KourierStore,
}

impl StoreOwner {
    /// Opens (creating if needed) the account's `backend.db` and starts its writer thread
    /// `thread_name`.
    pub fn open(path: &Path, key: &DbKey, thread_name: &str) -> Result<Self, OpenError> {
        let conn = db::open(path, key)?;
        // Every statement of this crate is a fixed text: keep them all prepared.
        conn.set_prepared_statement_cache_capacity(STATEMENTS);
        let writer = Writer::spawn(thread_name, conn)?;
        let store = KourierStore {
            inner: Arc::new(Inner {
                handle: writer.handle(),
                activity: Arc::new(Activity::default()),
                hold: Arc::new(LinkHold::default()),
            }),
        };
        Ok(Self { writer, store })
    }

    /// A handle for the client. Any number of clones.
    pub fn store(&self) -> KourierStore {
        self.store.clone()
    }

    /// Finishes the jobs already queued and stops the thread. Every later call of any clone fails.
    pub fn close(self) {
        // The connection comes back; dropping it closes the file.
        drop(self.writer.shutdown());
    }
}

impl fmt::Debug for StoreOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreOwner { .. }")
    }
}

impl fmt::Debug for KourierStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KourierStore { .. }")
    }
}

impl KourierStore {
    /// The counters of the app-state path: the clearing points of rule (b) look at them.
    pub(crate) fn activity(&self) -> &Arc<Activity> {
        &self.inner.activity
    }

    /// The receive hold of a link: closed while a login has no account yet (`crate::hold`).
    pub(crate) fn hold(&self) -> &Arc<LinkHold> {
        &self.inner.hold
    }

    /// A job that only reads (live priority): `f` runs on the writer thread without a transaction.
    pub(crate) async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(Priority::High, move |conn| f(conn)).await
    }

    /// [`read`](Self::read) for a thread that is no task: the call interceptor and the other handlers
    /// that run on a worker of the client's runtime and park on a std channel (the way
    /// `events::EventWriter::write_and_wait` waits; not `block_in_place`, which panics on a
    /// current-thread runtime, and not tokio's `blocking_recv`, which panics inside any runtime). The
    /// writer thread never waits for a runtime, so the parked worker cannot hold up the job it waits
    /// for. Waits at most [`BLOCKING_READ`]; a store that is closed or does not answer is an `Err`.
    pub(crate) fn read_blocking<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (answer, wait) = std::sync::mpsc::sync_channel(1);
        self.queue(move |conn| {
            // A caller that gave up has dropped the receiver; the answer is lost on purpose.
            let _ = answer.send(f(conn));
        })?;
        wait.recv_timeout(BLOCKING_READ)
            .map_err(|_| error::job_failed())?
    }

    /// A job that writes (live priority): `f` runs in one `BEGIN IMMEDIATE` transaction.
    pub(crate) async fn write<T, F>(&self, op: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(Priority::High, move |conn| db::zone(conn, op, None, f))
            .await
    }

    /// [`write`](Self::write) with kill points around the commit.
    pub(crate) async fn write_at<T, F>(
        &self,
        op: &'static str,
        points: Points,
        f: F,
    ) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(Priority::High, move |conn| {
            db::zone(conn, op, Some(points), f)
        })
        .await
    }

    /// [`write_at`](Self::write_at) that returns as soon as the job is QUEUED, not when it ran, and
    /// drops the answer: for the at-most-once kinds whose acknowledgement never waits for their
    /// write (newsletters, PDO). Jobs run in the order they were queued, so the order of the calls
    /// is the order of the writes, also against the jobs that do wait. `Err` only when the writer
    /// stopped; a zone that fails is logged (its operation, never an input) and nobody is told.
    pub(crate) fn write_at_detached<F>(
        &self,
        op: &'static str,
        points: Points,
        f: F,
    ) -> Result<(), StoreError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<(), StoreError> + Send + 'static,
    {
        self.queue(move |conn| {
            if let Err(e) = db::zone(conn, op, Some(points), f) {
                tracing::error!(error = %e, "a detached write failed");
            }
        })
    }

    /// [`write_at_detached`](Self::write_at_detached) without kill points: a small write whose place
    /// in the writer's order is what matters (the catch-up boundary), not its answer.
    pub(crate) fn write_detached<F>(&self, op: &'static str, f: F) -> Result<(), StoreError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<(), StoreError> + Send + 'static,
    {
        self.queue(move |conn| {
            if let Err(e) = db::zone(conn, op, None, f) {
                tracing::error!(error = %e, "a detached write failed");
            }
        })
    }

    /// Queues `f` on the writer thread (live priority, the store's lane) and returns once it is
    /// queued; nobody waits for the answer. `Err` when the writer stopped.
    pub(crate) fn queue<F>(&self, f: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut Connection) + Send + 'static,
    {
        self.inner
            .handle
            .submit(Priority::High, LANE, f)
            .map(drop)
            .map_err(|_| error::closed())
    }

    /// A job that writes at low priority (expiry sweeps, upkeep): live work is served first.
    pub(crate) async fn sweep<T, F>(&self, op: &'static str, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(Priority::Low, move |conn| db::zone(conn, op, None, f))
            .await
    }

    /// [`sweep`](Self::sweep) with kill points around the commit: the low-priority zone of the history's
    /// delivery rows (W4), which live work is served ahead of.
    pub(crate) async fn sweep_at<T, F>(
        &self,
        op: &'static str,
        points: Points,
        f: F,
    ) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(Priority::Low, move |conn| {
            db::zone(conn, op, Some(points), f)
        })
        .await
    }

    /// The raw connection, for the maintenance that must not sit in a transaction.
    pub(crate) async fn raw<T, F>(&self, priority: Priority, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        self.run(priority, f).await
    }

    async fn run<T, F>(&self, priority: Priority, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        // `submit` fails when the writer stopped; the answer is lost when the job PANICKED (the
        // thread lives on and the next call works): two different failures, two different texts.
        match self.inner.handle.submit(priority, LANE, f) {
            Err(_) => Err(error::closed()),
            Ok(answer) => match answer.await {
                Ok(result) => result,
                Err(_) => Err(error::job_failed()),
            },
        }
    }
}

/// Runs one statement that changes rows; the number of rows it changed.
pub(crate) fn exec(
    conn: &Connection,
    op: &'static str,
    statement: &str,
    params: impl Params,
) -> Result<usize, StoreError> {
    conn.prepare_cached(statement)
        .and_then(|mut s| s.execute(params))
        .map_err(sql(op))
}

/// The first row of a query, mapped by `map`; `None` when there is none.
pub(crate) fn first<T>(
    conn: &Connection,
    op: &'static str,
    statement: &str,
    params: impl Params,
    map: impl FnOnce(&Row<'_>) -> kourier_db::rusqlite::Result<T>,
) -> Result<Option<T>, StoreError> {
    conn.prepare_cached(statement)
        .and_then(|mut s| s.query_row(params, map).optional())
        .map_err(sql(op))
}

/// Every row of a query, mapped by `map`, in the query's order.
pub(crate) fn rows<T>(
    conn: &Connection,
    op: &'static str,
    statement: &str,
    params: impl Params,
    map: impl FnMut(&Row<'_>) -> kourier_db::rusqlite::Result<T>,
) -> Result<Vec<T>, StoreError> {
    conn.prepare_cached(statement)
        .and_then(|mut s| s.query_map(params, map).and_then(|rows| rows.collect()))
        .map_err(sql(op))
}

/// A blob read from a column.
pub(crate) fn blob(row: &Row<'_>, index: usize) -> kourier_db::rusqlite::Result<Vec<u8>> {
    row.get(index)
}

#[cfg(test)]
mod tests;
