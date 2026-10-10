//! `core.db` access (§2.5, §5.4): one writer thread with the two-priority, per-account
//! round-robin scheduler of `kourier-db`, two read-only connections for UI queries and a third
//! for media-handle misses (§8.6). No rusqlite call ever runs on a tokio worker: writes go to the
//! writer thread, reads to the blocking pool.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use kourier_db::rusqlite::Connection;
use kourier_db::{DbKey, Migrations, Priority, Writer, WriterHandle, migration, migration_rebuild};
use kourier_model::AccountId;
use tokio::sync::Semaphore;

use crate::error::{CoreError, Result};

/// The `core` schema component (§5.7).
pub const MIGRATIONS: Migrations = Migrations::new(
    "core",
    &[
        migration!(1, "0001_core"),
        migration!(2, "0002_ids_and_downloads"),
        migration_rebuild!(3, "0003_message_ids"),
        migration!(4, "0004_outbox_ids"),
        migration!(5, "0005_timer_version_long_text"),
        migration!(6, "0006_expired_messages"),
        migration!(7, "0007_hidden_number"),
        migration_rebuild!(8, "0008_account_ids_outbox_heads"),
        migration!(9, "0009_expiry_hidden"),
        migration!(10, "0010_call_lines"),
        migration!(11, "0011_call_line_keys"),
        migration!(12, "0012_unread_history"),
    ],
);

/// Largest number of history messages one writer job applies (§4.10): one `HistoryBatch`.
pub const HISTORY_CHUNK: usize = kourier_backend_api::MAX_HISTORY_BATCH;

/// Number of read-only connections for UI queries.
pub const UI_READERS: usize = 2;

/// The writer lane of an account: jobs of one account run in FIFO order, accounts round-robin.
pub(crate) fn lane(account: AccountId) -> u64 {
    u64::try_from(account.0).unwrap_or(0)
}

/// The lane for jobs that belong to no account (startup, rebuild bookkeeping).
pub(crate) const GLOBAL_LANE: u64 = 0;

/// Opened `core.db`: the writer thread and the read pools.
pub(crate) struct Store {
    writer: Mutex<Option<Writer>>,
    handle: WriterHandle,
    ui: ReadPool,
    media: ReadPool,
}

/// A small pool of read-only connections used from the blocking pool.
struct ReadPool {
    conns: Arc<Mutex<Vec<Connection>>>,
    permits: Arc<Semaphore>,
    /// How many connections the pool has.
    size: usize,
}

/// Frees what `conn` keeps only for speed: its page cache (`sqlite3_db_release_memory`) and
/// its cached prepared statements.
fn release(conn: &Connection) {
    conn.flush_prepared_statement_cache();
    if let Err(e) = conn.release_memory() {
        tracing::debug!(error = %e, "releasing a connection's memory failed");
    }
}

impl ReadPool {
    fn open(path: &Path, key: &DbKey, n: usize) -> Result<Self> {
        let conns = (0..n)
            .map(|_| kourier_db::open_read_only(path, key))
            .collect::<kourier_db::Result<Vec<_>>>()?;
        Ok(Self {
            conns: Arc::new(Mutex::new(conns)),
            permits: Arc::new(Semaphore::new(n)),
            size: n,
        })
    }

    /// Gives back the page cache and the prepared statements of every connection of the
    /// pool (they wait for running queries to finish first).
    async fn release_memory(&self) -> Result<()> {
        let all = u32::try_from(self.size).unwrap_or(u32::MAX);
        let permits = self
            .permits
            .clone()
            .acquire_many_owned(all)
            .await
            .map_err(|_| CoreError::Stopped)?;
        let conns = self.conns.clone();
        tokio::task::spawn_blocking(move || {
            let _permits = permits;
            for conn in conns.lock().unwrap_or_else(PoisonError::into_inner).iter() {
                release(conn);
            }
        })
        .await
        .map_err(|_| CoreError::Stopped)
    }

    async fn run<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| CoreError::Stopped)?;
        let conns = self.conns.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let conn = conns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
                .ok_or(CoreError::Stopped)?;
            // Gives the connection back on every exit path, a panicking query included.
            let guard = Return {
                conns: &conns,
                conn: Some(conn),
            };
            let out = match &guard.conn {
                Some(c) => f(c),
                None => Err(CoreError::Stopped),
            };
            drop(guard);
            out
        })
        .await
        .map_err(|_| CoreError::Stopped)?
    }
}

/// Puts a read connection back into its pool on drop.
struct Return<'a> {
    conns: &'a Mutex<Vec<Connection>>,
    conn: Option<Connection>,
}

impl Drop for Return<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.conns
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(c);
        }
    }
}

impl Store {
    /// Opens (creating if missing) and migrates `core.db`, starts the writer thread and opens the
    /// read connections. Returns whether the file was new.
    pub(crate) fn open(path: &Path, key: &DbKey) -> Result<(Self, bool)> {
        let fresh = !path.exists();
        let mut conn = kourier_db::open(path, key)?;
        kourier_db::migrate(&mut conn, &MIGRATIONS)?;
        let writer = Writer::spawn("kourier-core-db", conn)?;
        let handle = writer.handle();
        let ui = ReadPool::open(path, key, UI_READERS)?;
        let media = ReadPool::open(path, key, 1)?;
        Ok((
            Self {
                writer: Mutex::new(Some(writer)),
                handle,
                ui,
                media,
            },
            fresh,
        ))
    }

    /// Runs `f` on the writer thread (one job of the scheduler) and awaits its result.
    pub(crate) async fn write<R, F>(&self, priority: Priority, lane: u64, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        self.handle.call(priority, lane, f).await?
    }

    /// Runs a UI query on one of the two UI read connections.
    pub(crate) async fn read<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
    {
        self.ui.run(f).await
    }

    /// Runs a media-handle lookup on the third read connection (§8.6).
    pub(crate) async fn read_media<R, F>(&self, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
    {
        self.media.run(f).await
    }

    /// Gives back the memory the connections keep for speed (`UiCommand::ReleaseMemory`):
    /// page caches and prepared statements of the writer and of every read connection.
    pub(crate) async fn release_memory(&self) -> Result<()> {
        self.write(Priority::Low, GLOBAL_LANE, |c| {
            release(c);
            Ok(())
        })
        .await?;
        self.ui.release_memory().await?;
        self.media.release_memory().await
    }

    /// Finishes queued writes and stops the writer thread (blocking: call from the blocking
    /// pool).
    pub(crate) fn close(&self) {
        let writer = self
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(w) = writer
            && let Err(e) = w.shutdown()
        {
            tracing::warn!(error = %e, "core.db writer did not stop cleanly");
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Live jobs of any account run before queued history chunks, and accounts take turns
    /// (§4.9: a WhatsApp history import never delays Signal live ingest).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_jobs_overtake_history_and_accounts_take_turns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (store, fresh) =
            Store::open(&dir.path().join("core.db"), &DbKey::from([1; 32])).expect("open");
        assert!(fresh);
        let store = Arc::new(store);
        let order = Arc::new(StdMutex::new(Vec::new()));
        // Hold the writer so the queue fills up.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .write(Priority::High, GLOBAL_LANE, move |_| {
                        let _ = release_rx.recv();
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut jobs = Vec::new();
        for (prio, account, tag) in [
            (Priority::Low, 1, "wa-history-1"),
            (Priority::Low, 1, "wa-history-2"),
            (Priority::High, 2, "signal-live-1"),
            (Priority::High, 1, "wa-live-1"),
            (Priority::High, 2, "signal-live-2"),
        ] {
            let (store, order) = (store.clone(), order.clone());
            jobs.push(tokio::spawn(async move {
                store
                    .write(prio, lane(AccountId(account)), move |_| {
                        order
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(tag);
                        Ok(())
                    })
                    .await
            }));
            // Submission order is the order above.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        release_tx.send(()).expect("release");
        blocker.await.expect("join").expect("blocker");
        for j in jobs {
            j.await.expect("join").expect("job");
        }
        assert_eq!(
            *order.lock().unwrap_or_else(PoisonError::into_inner),
            [
                "signal-live-1",
                "wa-live-1",
                "signal-live-2",
                "wa-history-1",
                "wa-history-2"
            ]
        );
        let s = store.clone();
        tokio::task::spawn_blocking(move || s.close())
            .await
            .expect("close");
    }
}
