//! `core.db` access (§2.5, §5.4): one writer thread with the two-priority, per-account
//! round-robin scheduler of `kourier-db`, two read-only connections for UI queries, a third
//! for media-handle misses (§8.6), and message search's own pair (search.db and core.db, both
//! read-only and lazy: G1e). No rusqlite call ever runs on a tokio worker: writes go to the
//! writer thread, reads to the blocking pool. The search index lives in its own file with its
//! own writer (`search::index`); the store links the two ([`SearchLink`]): the wake, the
//! `applied` the task committed, whether the file has been judged, and the prune of the log.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use kourier_db::rusqlite::{Connection, InterruptHandle};
use kourier_db::{DbKey, Migrations, Priority, Writer, WriterHandle, migration, migration_rebuild};
use kourier_model::AccountId;
use tokio::sync::Semaphore;

use crate::error::{CoreError, Result};
use crate::search::file;
use crate::search::log;

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
        migration!(13, "0013_attachment_requests"),
        migration!(14, "0014_merge_indexes"),
        migration!(15, "0015_quiet_notices"),
        migration!(16, "0016_message_key_author"),
        migration!(17, "0017_outbox_cancel"),
        migration!(18, "0018_people"),
        migration!(19, "0019_group_page"),
        migration!(20, "0020_chat_media"),
        migration!(21, "0021_verified_notice"),
        // (S-P, protected chats; the number is final, the next step takes 23.)
        migration!(22, "0022_protected_chats"),
        // (KT-2, cloud ids and read positions; the number is settled at the landing.)
        migration!(23, "0023_cloud_ids"),
        // (G1, message search; the number is settled at the landing: the tests find the step
        // by its name, `_search`.)
        migration!(24, "0024_search"),
        // (The settings batch: the draft time of a chat; the number is settled at the landing.)
        migration!(25, "0025_draft_time"),
        // (RegExp filters, R1: the table of the saved filters; the number is settled at the
        // landing, the tests find the step by its name, `_filters`.)
        migration!(26, "0026_filters"),
        // (3D viewer V2, the model mark of an attachment; the number is settled at the landing,
        // the tests find the step by its name, `_model3d`.)
        migration!(27, "0027_model3d"),
    ],
);

/// Largest number of history messages one writer job applies (§4.10): one `HistoryBatch`.
pub const HISTORY_CHUNK: usize = kourier_backend_api::MAX_HISTORY_BATCH;

/// Prepared statements the writer connection keeps (rusqlite's default is 16).
pub(crate) const WRITER_STATEMENT_CACHE: usize = 128;

/// Prepared statements each read connection keeps (rusqlite's default is 16).
pub(crate) const READER_STATEMENT_CACHE: usize = 128;

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
    /// What the store and the search task share of search.db.
    link: Arc<SearchLink>,
    /// The read connections of message search (G1e), opened at the first search.
    search: SearchReader,
}

/// What the store and the search task (`search::index::keep`) share: the files, the wake, the
/// `applied` search.db last COMMITTED, whether the task has judged the file (the reader opens
/// nothing before), and the coalesced prune of the log.
pub(crate) struct SearchLink {
    paths: file::Paths,
    /// Set by every write job that leaves log rows above `applied`: the search task sleeps on
    /// it.
    wake: Arc<tokio::sync::Notify>,
    applied: AtomicI64,
    /// Whether the task has judged the file (a search waits for it up to its time cap).
    ready: tokio::sync::watch::Sender<bool>,
    /// The newest `applied` a prune was asked for, and whether a prune job is queued.
    prune_to: AtomicI64,
    prune_posted: AtomicBool,
    /// The interrupt of the search task's writer connection (set when the task opens the file):
    /// the core's shutdown ends a running step or flush with it (review C4).
    writer_interrupt: Mutex<Option<InterruptHandle>>,
    /// How many prune jobs ran, and the most rows one deleted (tests: the cap holds).
    #[cfg(test)]
    prune_jobs: AtomicI64,
    #[cfg(test)]
    prune_most: AtomicI64,
}

impl SearchLink {
    /// The files (search.db, core.db, their keys).
    pub(crate) fn paths(&self) -> &file::Paths {
        &self.paths
    }

    /// What the search task sleeps on ([`Store::write`] sets it).
    pub(crate) fn wake(&self) -> Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    /// The `applied` search.db last committed.
    pub(crate) fn applied(&self) -> i64 {
        self.applied.load(Ordering::SeqCst)
    }

    /// The `applied` search.db committed (the wake's measure).
    pub(crate) fn set_applied(&self, applied: i64) {
        self.applied.store(applied, Ordering::SeqCst);
    }

    /// Keeps the interrupt of the search writer's connection (the task's, after every open).
    pub(crate) fn set_writer_interrupt(&self, handle: InterruptHandle) {
        *self
            .writer_interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Interrupts what the search writer runs now (nothing when it is idle).
    pub(crate) fn interrupt_writer(&self) {
        if let Some(handle) = self
            .writer_interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            handle.interrupt();
        }
    }

    /// Whether the query may open search.db: the task has judged (or rebuilt) the file.
    pub(crate) fn set_ready(&self, ready: bool) {
        self.ready.send_replace(ready);
    }

    pub(crate) fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    /// How many searches wait for the judge now (only a search subscribes to the flag): tests
    /// set the flag while one is seen waiting, with no wall time between.
    #[cfg(test)]
    pub(crate) fn searches_waiting(&self) -> usize {
        self.ready.receiver_count()
    }
}

/// What a search on [`SearchReader`] gave.
#[derive(Debug)]
pub(crate) enum Searched<R> {
    /// The page.
    Done(R),
    /// The index file has not been judged yet (the search task opens it at `UiReady`, and
    /// while it builds it again): the query answers "not ready", never "nothing".
    NotReady,
    /// A newer search was asked while this one waited for the connections (MEDIUM-3): it ends
    /// unanswered.
    Superseded,
}

/// Message search's own read connections: search.db (the walk over `messages_fts`) and core.db
/// (the look-up of every hit), so a long search never takes a UI reader (G1e). They are opened
/// LAZILY at the first search after the search task has judged the file (the start does not pay
/// for them; before that a search answers [`Searched::NotReady`]) and closed by
/// [`Store::release_memory`] and before a rebuild of search.db. For that the reader keeps the
/// files' paths and keys (`DbKey`, zeroized on drop, never logged: copies in memory for as long
/// as the store is open, beside the ones the open connections hold inside SQLCipher). One search
/// runs at a time: a newer one interrupts the running one (it answers what it found, `capped`),
/// and so does the time cap; an interrupt is tied to the search's GENERATION under one lock
/// (L6), so it can never land on the next search; a search that waited for the connections while
/// a newer one was asked ends unanswered (MEDIUM-3).
struct SearchReader {
    link: Arc<SearchLink>,
    conns: Arc<tokio::sync::Mutex<Option<(Connection, Connection)>>>,
    /// The generation that runs on the connections now, and their interrupts.
    running: Arc<RunningSearch>,
    generation: Arc<AtomicU64>,
}

/// The generation of the search that runs, with the interrupts of its two connections.
type RunningSearch = Mutex<Option<(u64, [InterruptHandle; 2])>>;

/// Interrupts the search that runs, when `stop` says so of its generation.
fn interrupt_if(running: &RunningSearch, stop: impl Fn(u64) -> bool) {
    let running = running.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((generation, handles)) = running.as_ref()
        && stop(*generation)
    {
        for handle in handles {
            handle.interrupt();
        }
    }
}

/// Clears the running generation when a search ends, a panicking one included.
struct Running<'a>(&'a RunningSearch);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl SearchReader {
    fn new(link: Arc<SearchLink>) -> Self {
        Self {
            link,
            conns: Arc::new(tokio::sync::Mutex::new(None)),
            running: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    async fn run<R, F>(&self, f: F) -> Result<Searched<R>>
    where
        R: Send + 'static,
        F: FnOnce(&Connection, &Connection, Instant) -> Result<R> + Send + 'static,
    {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // A newer search ends the one that runs.
        interrupt_if(&self.running, |g| g < generation);
        // The file is judged at `UiReady` within milliseconds (a new one is empty): a search
        // asked before waits for it, at most its time cap.
        if !self.link.is_ready() {
            let mut ready = self.link.ready.subscribe();
            let _ =
                tokio::time::timeout(crate::search::query::TIME_CAP, ready.wait_for(|r| *r)).await;
        }
        // SQLite drops an interrupt that comes while no statement runs, so one that met the older
        // search between its start and its first statement was lost and the older one ran to its
        // time cap while this one waited (`a_newer_search_ends_one_that_has_not_begun_its_statement`):
        // it is said again until the older one lets go of the connections.
        let lock = self.conns.clone().lock_owned();
        tokio::pin!(lock);
        let guard = loop {
            tokio::select! {
                guard = &mut lock => break guard,
                () = tokio::time::sleep(SEARCH_INTERRUPT_AGAIN) => {
                    interrupt_if(&self.running, |g| g < generation);
                }
            }
        };
        // MEDIUM-3: a newer search was asked while this one waited: it goes first.
        if self.generation.load(Ordering::SeqCst) != generation {
            return Ok(Searched::Superseded);
        }
        // Judged under the guard: a rebuild takes the guard after it cleared the flag, so no
        // search opens the file it is about to delete.
        if !self.link.is_ready() {
            return Ok(Searched::NotReady);
        }
        let (started, started_rx) = tokio::sync::oneshot::channel::<()>();
        let timer = {
            let running = self.running.clone();
            tokio::spawn(async move {
                if started_rx.await.is_ok() {
                    // A margin over the walk's own check: this is for one long step.
                    tokio::time::sleep(crate::search::query::TIME_CAP + SEARCH_INTERRUPT_MARGIN)
                        .await;
                    interrupt_if(&running, |g| g == generation);
                }
            })
        };
        let running = self.running.clone();
        let paths = self.link.paths.clone();
        let out = tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            if guard.is_none() {
                let search = kourier_db::open_read_only(&paths.search, &paths.search_key)?;
                let core = kourier_db::open_read_only(&paths.core, &paths.core_key)?;
                *guard = Some((search, core));
            }
            let (search, core) = guard.as_ref().ok_or(CoreError::Stopped)?;
            *running.lock().unwrap_or_else(PoisonError::into_inner) = Some((
                generation,
                [search.get_interrupt_handle(), core.get_interrupt_handle()],
            ));
            let _running = Running(&running);
            let _ = started.send(());
            f(
                search,
                core,
                Instant::now() + crate::search::query::TIME_CAP,
            )
        })
        .await
        .map_err(|_| CoreError::Stopped);
        timer.abort();
        out?.map(Searched::Done)
    }

    /// Closes the connections (a running search ends first); the next search opens them again.
    async fn release(&self) -> Result<()> {
        let conns = self.conns.lock().await.take();
        if let Some(conns) = conns {
            tokio::task::spawn_blocking(move || drop(conns))
                .await
                .map_err(|_| CoreError::Stopped)?;
        }
        Ok(())
    }
}

/// How long after the time cap the search's own interrupt comes (the walk stops by itself at
/// the cap between two rows; the interrupt is for a single step that runs on).
const SEARCH_INTERRUPT_MARGIN: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a newer search that waits says its interrupt again (one said before the older
/// search's first statement is lost).
const SEARCH_INTERRUPT_AGAIN: std::time::Duration = std::time::Duration::from_millis(10);

/// A small pool of read-only connections used from the blocking pool.
struct ReadPool {
    conns: Arc<Mutex<Vec<Connection>>>,
    permits: Arc<Semaphore>,
    /// How many connections the pool has.
    size: usize,
}

impl ReadPool {
    fn open(path: &Path, key: &DbKey, n: usize) -> Result<Self> {
        let conns = (0..n)
            .map(|_| kourier_db::open_read_only(path, key))
            .collect::<kourier_db::Result<Vec<_>>>()?;
        for c in &conns {
            c.set_prepared_statement_cache_capacity(READER_STATEMENT_CACHE);
        }
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
                kourier_db::release_memory(conn);
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
    /// [`Store::open_with`] with search.db beside core.db ([`file::beside`]) under core.db's
    /// key (tests; the app gives search.db its own key).
    #[cfg(test)]
    pub(crate) fn open(path: &Path, key: &DbKey) -> Result<(Self, bool)> {
        Self::open_with(path, key, &file::beside(path), key)
    }

    /// Opens (creating if missing) and migrates `core.db`, starts the writer thread and opens the
    /// read connections. Returns whether the file was new. search.db is not touched here: the
    /// search task opens and judges it at `UiReady`.
    pub(crate) fn open_with(
        path: &Path,
        key: &DbKey,
        search_path: &Path,
        search_key: &DbKey,
    ) -> Result<(Self, bool)> {
        let fresh = !path.exists();
        let mut conn = kourier_db::open(path, key)?;
        kourier_db::migrate(&mut conn, &MIGRATIONS)?;
        // rusqlite keeps 16 prepared statements by default; one job of the writer (an identity
        // merge, a history batch) runs more distinct ones than that and prepared them again
        // and again (`target/link-feedback/tail.md`: half the samples of a merge were in
        // `sqlite3_prepare_v3`). Hiding the window flushes the cache ([`release`]).
        conn.set_prepared_statement_cache_capacity(WRITER_STATEMENT_CACHE);
        let writer = Writer::spawn("kourier-core-db", conn)?;
        let handle = writer.handle();
        let ui = ReadPool::open(path, key, UI_READERS)?;
        let media = ReadPool::open(path, key, 1)?;
        let link = Arc::new(SearchLink {
            paths: file::Paths {
                core: path.to_owned(),
                core_key: key.clone(),
                search: search_path.to_owned(),
                search_key: search_key.clone(),
            },
            wake: Arc::new(tokio::sync::Notify::new()),
            applied: AtomicI64::new(0),
            ready: tokio::sync::watch::Sender::new(false),
            prune_to: AtomicI64::new(0),
            prune_posted: AtomicBool::new(false),
            writer_interrupt: Mutex::new(None),
            #[cfg(test)]
            prune_jobs: AtomicI64::new(0),
            #[cfg(test)]
            prune_most: AtomicI64::new(0),
        });
        Ok((
            Self {
                writer: Mutex::new(Some(writer)),
                handle,
                ui,
                media,
                link: link.clone(),
                search: SearchReader::new(link),
            },
            fresh,
        ))
    }

    /// Runs `f` on the writer thread (one job of the scheduler) and awaits its result.
    ///
    /// After the job the search log is peeked above the `applied` search.db committed (one
    /// cached statement on the primary key): a job that left rows there, whatever it was, wakes
    /// the search task. That is how every writer of `messages` reaches the index without knowing
    /// of it (the triggers fill the log).
    pub(crate) async fn write<R, F>(&self, priority: Priority, lane: u64, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    {
        let link = self.link.clone();
        self.handle
            .call(priority, lane, move |conn| {
                let out = f(conn);
                if log::pending(conn, link.applied.load(Ordering::SeqCst)).unwrap_or(false) {
                    link.wake.notify_one();
                }
                out
            })
            .await?
    }

    /// What the store shares with the search task.
    pub(crate) fn search_link(&self) -> Arc<SearchLink> {
        self.link.clone()
    }

    /// THE PRUNE of the search log up to `applied`, a value search.db COMMITTED: Low jobs of the
    /// writer, coalesced (one queued at a time, the newest `applied` wins). Never waits. One job
    /// deletes at most [`log::PRUNE_ROWS`] rows and posts itself again while rows at or below
    /// the wanted position remain, so a High job never waits for a whole-range delete.
    pub(crate) fn post_prune(&self, applied: i64) {
        self.link.prune_to.fetch_max(applied, Ordering::SeqCst);
        Self::submit_prune(&self.handle, &self.link);
    }

    /// Queues the prune job unless one is queued already.
    fn submit_prune(handle: &WriterHandle, link: &Arc<SearchLink>) {
        if link.prune_posted.swap(true, Ordering::SeqCst) {
            return;
        }
        let (job, again) = (link.clone(), handle.clone());
        let posted = handle.submit(Priority::Low, GLOBAL_LANE, move |conn| {
            // Cleared before the value is read: a later ask posts a job of its own.
            job.prune_posted.store(false, Ordering::SeqCst);
            let to = job.prune_to.load(Ordering::SeqCst);
            match log::prune(conn, to) {
                Ok((_gone, more)) => {
                    #[cfg(test)]
                    {
                        job.prune_jobs.fetch_add(1, Ordering::SeqCst);
                        let gone = i64::try_from(_gone).unwrap_or(i64::MAX);
                        job.prune_most.fetch_max(gone, Ordering::SeqCst);
                    }
                    if more {
                        Self::submit_prune(&again, &job);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "search: pruning the log failed; the next prune retries");
                }
            }
        });
        if posted.is_err() {
            link.prune_posted.store(false, Ordering::SeqCst);
        }
    }

    /// THE SHUTDOWN's part of search (review C4): the search writer's running step or flush and
    /// the query's running search are interrupted, so the blocking jobs end at once instead of
    /// holding the process up to its shutdown timeout. An interrupted write rolls back (the
    /// kill loops prove a lost step is safe); the task, told it is stopping, ends.
    pub(crate) fn interrupt_search(&self) {
        self.link.interrupt_writer();
        interrupt_if(&self.search.running, |_| true);
    }

    /// `(prune jobs run, most rows one deleted)` (tests).
    #[cfg(test)]
    pub(crate) fn prune_counts(&self) -> (i64, i64) {
        (
            self.link.prune_jobs.load(Ordering::SeqCst),
            self.link.prune_most.load(Ordering::SeqCst),
        )
    }

    /// Closes the search's read connections (before a rebuild of search.db; a running search
    /// ends first).
    pub(crate) async fn release_search(&self) {
        if let Err(e) = self.search.release().await {
            tracing::debug!(error = %e, "search: closing the read connections failed");
        }
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

    /// Runs a message search on its own read connections ([`SearchReader`]): `f` gets
    /// search.db's, core.db's and the time its page must end by. A newer search interrupts this
    /// one.
    pub(crate) async fn search<R, F>(&self, f: F) -> Result<Searched<R>>
    where
        R: Send + 'static,
        F: FnOnce(&Connection, &Connection, Instant) -> Result<R> + Send + 'static,
    {
        self.search.run(f).await
    }

    /// Whether the search's connections are open (tests of the lazy open).
    #[cfg(test)]
    pub(crate) async fn search_is_open(&self) -> bool {
        self.search.conns.lock().await.is_some()
    }

    /// Gives back the memory the connections keep for speed (`UiCommand::ReleaseMemory`):
    /// page caches and prepared statements of the writer and of every read connection, and the
    /// search's connection itself (opened again by the next search).
    pub(crate) async fn release_memory(&self) -> Result<()> {
        self.write(Priority::Low, GLOBAL_LANE, |c| {
            kourier_db::release_memory(c);
            Ok(())
        })
        .await?;
        self.ui.release_memory().await?;
        self.media.release_memory().await?;
        self.search.release().await
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

    use kourier_db::rusqlite::StatementStatus;
    use kourier_db::rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::collections::BTreeMap;

    /// Per SQL text: how often it ran, and how often it ran as a fresh prepare (its run count
    /// was still 1, the statement had never run before).
    static SEEN: StdMutex<BTreeMap<String, (u32, u32)>> = StdMutex::new(BTreeMap::new());

    fn tracer(ev: TraceEvent<'_>) {
        if let TraceEvent::Stmt(s, _) = ev {
            let fresh = s.get_status(StatementStatus::Run) == 0;
            let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
            let e = seen.entry(s.sql().into_owned()).or_default();
            e.0 += 1;
            e.1 += u32::from(fresh);
        }
    }

    /// The UI readers keep every statement of a list + timeline round prepared: after one
    /// warm-up round no statement is prepared again (with rusqlite's default of 16 cached
    /// statements the LRU thrashed and SQLite parsed the same SQL for every row, structs.md #4).
    /// Counted by SQLite's own trace: a statement whose run counter is 0 when it starts has
    /// just been prepared.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ui_readers_do_not_prepare_a_round_again() {
        use crate::facade::{ChatMediaQuery, ConversationListQuery, MediaKind};
        use crate::facade::{TimelineAnchor, TimelineQuery};
        use kourier_fake::scripts::remote;
        use kourier_model::{ConvKey, MsgIdScope, NetworkKind, UserId};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("core.db");
        let key = DbKey::from([1; 32]);
        let me = UserId::parse("aci:00000000-0000-4000-8000-0000000000aa").expect("id");
        {
            let mut h = crate::testing::IngestHarness::open(
                &path,
                &key,
                NetworkKind::Fake,
                MsgIdScope::AccountAuthor,
                &[me],
            )
            .expect("harness");
            for c in 0..3 {
                let by = UserId::parse(&format!("aci:00000000-0000-4000-8000-{:012x}", 0x100 + c))
                    .expect("id");
                let conv = ConvKey::Direct(by.clone());
                let messages: Vec<_> = (0..5)
                    .map(|i| remote(&conv, &by, &format!("{c}-{i}"), 1_700_000_000_000 + i, "hi"))
                    .collect();
                h.apply(vec![kourier_backend_api::BackendEvent::HistoryBatch {
                    conv,
                    messages,
                    has_more: false,
                    progress: None,
                }])
                .expect("history");
            }
        }
        let (store, _) = Store::open(&path, &key).expect("open");
        for c in store
            .ui
            .conns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            c.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(tracer));
        }
        let (mut distinct, mut fresh) = (0, u32::MAX);
        for _round in 0..2 {
            SEEN.lock().unwrap_or_else(PoisonError::into_inner).clear();
            let page = store
                .read(|c| crate::query::conversations(c, &ConversationListQuery::default()))
                .await
                .expect("list");
            for row in &page.rows {
                let id = row.id;
                store
                    .read(move |c| {
                        crate::outbox::network_of(c, id)?;
                        let q = TimelineQuery {
                            conv: id,
                            anchor: TimelineAnchor::Newest,
                            limit: 50,
                        };
                        let p = crate::query::timeline(
                            c,
                            &q,
                            1_700_000_100_000,
                            true,
                            &Default::default(),
                            None,
                        )?;
                        crate::query::page_media(c, &p)?;
                        crate::query::conversation(c, id)?;
                        crate::query::group(c, id)?;
                        crate::query::chat_media(
                            c,
                            &ChatMediaQuery {
                                conv: id,
                                kind: MediaKind::Visual,
                                before: None,
                                limit: 50,
                            },
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("timeline");
            }
            let seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
            distinct = seen.len();
            fresh = seen.values().map(|v| v.1).sum();
        }
        assert!(distinct >= 12, "the round runs many statements: {distinct}");
        assert!(
            distinct < READER_STATEMENT_CACHE,
            "the cache holds the round with room to spare: {distinct}"
        );
        assert_eq!(fresh, 0, "a warm round prepared {fresh} statements again");
    }
}
