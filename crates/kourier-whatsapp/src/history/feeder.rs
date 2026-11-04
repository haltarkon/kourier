//! The real feeder (W4 C15): the [`Rows`] the delivery pump is given. It chooses the chunk, has its rows read
//! on a thread of its own, and makes the rows that end a chunk and the push.
//!
//! WHERE THE WORK RUNS. A chunk is 1-20 MB of zlib; reading a row inflates it (once per chunk, and again when
//! a row asks for an earlier chat). That happens on the WORKER THREAD, which keeps the chunk it last read
//! and its [`Reader`], never on the actor's runtime. The questions of what comes next are small reads of
//! the stage and `backend.db` and are asked here.
//!
//! WHAT [`Rows::next`] DOES, from the cursor the store holds:
//! 1. The cursor stands INSIDE a chunk (not at its end): the chunk's next row. When the chunk's content is
//!    done, a row with no events ([`Body::End`] with `finished`) moves the cursor to the chunk's end and
//!    records the chunk as delivered in the same zone. A chunk the stage no longer holds (or that does not
//!    read any more) is given up the same way with no record, and a chunk that does not read is set aside
//!    (`bad`) so it is not chosen again.
//! 2. The cursor stands at the END of a chunk (or at none): first the record of the chunk as delivered, if the
//!    last content row left it missing; then the end rows owed ([`ending`]: the
//!    conversations the phone said it has no more of in the chunk just delivered, and those of a sync type
//!    whose push is over, at most [`END_PER_ROW`] a row), then the next chunk by
//!    [`Facts::next_chunk`] from its first position.
//!
//! So everything is a function of the stored cursor, the stage and the books: a row asked for twice is the
//! same row, and a restart goes on from wherever the last zone left things. Nothing here logs an id, a name
//! or a body.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use kourier_backend_api::Clock;
use kourier_db::Priority;
use kourier_db::rusqlite::Connection;
use kourier_model::ConvKey;
use tokio::sync::oneshot;
use wacore::store::error::StoreError;

use super::deliver::Source;
use super::feed::{self, Bad};
use super::progress::{self, Delivered, Facts, Staged, State};
use super::pump::Rows;
use super::rows::{self, Body, Bounds, ChunkEnd, Cursor, Next, Phase, Reader, Row};
use super::stage::StageHandle;
use crate::error::{self, sql};
use crate::map::Ctx;
use crate::store::{KourierStore, exec};

/// Most conversations one end row ends.
pub(crate) const END_PER_ROW: usize = 100;

/// What the worker found at a place of a chunk.
#[expect(
    clippy::large_enum_variant,
    reason = "made once, sent over a channel and taken apart at once: a box would add an allocation per row"
)]
enum Read {
    /// The stage holds no planned chunk with bytes under that number.
    Gone,
    /// The chunk does not read any more.
    Unreadable(Bad),
    Row {
        head: Head,
        found: Next,
    },
}

/// What a row needs to know of its chunk.
#[derive(Clone, Debug)]
struct Head {
    msg_id: String,
    sync_type: Option<i32>,
    order: Option<u32>,
    progress: Option<u32>,
    skipped: u32,
}

impl Head {
    fn source(&self) -> Source {
        Source {
            msg_id: self.msg_id.clone(),
            sync_type: self.sync_type,
            progress: progress::fraction(self.progress),
        }
    }

    fn end(&self) -> ChunkEnd {
        ChunkEnd {
            sync_type: self.sync_type,
            order: self.order,
            progress: self.progress,
            skipped: self.skipped,
        }
    }
}

struct Job {
    seq: i64,
    at: Cursor,
    bounds: Bounds,
    reply: oneshot::Sender<Result<Read, StoreError>>,
}

/// The thread that reads the rows. It stops when the feeder is dropped.
struct Worker {
    jobs: Option<Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

/// How long the worker keeps a chunk it has loaded (up to 64 MiB inflated, with its plan) while no job asks for it
/// (W4 fix MEDIUM-2).
const IDLE: Duration = Duration::from_secs(30);

impl Worker {
    fn start(stage: StageHandle, ctx: Ctx, clock: Arc<dyn Clock>) -> Self {
        Self::start_with(stage, ctx, clock, IDLE, Arc::new(AtomicUsize::new(0)))
    }

    /// [`start`](Self::start) with the idle time and a counter of the chunks the worker let go of (the test seam).
    fn start_with(
        stage: StageHandle,
        ctx: Ctx,
        clock: Arc<dyn Clock>,
        idle: Duration,
        released: Arc<AtomicUsize>,
    ) -> Self {
        let (jobs, inbox) = channel();
        let thread = std::thread::Builder::new()
            .name("wa-history-feed".to_owned())
            .spawn(move || work(&stage, &ctx, clock.as_ref(), &inbox, idle, &released))
            .ok();
        Self {
            jobs: thread.is_some().then_some(jobs),
            thread,
        }
    }

    async fn read(&self, seq: i64, at: Cursor, bounds: Bounds) -> Result<Read, StoreError> {
        let (reply, answer) = oneshot::channel();
        let sent = self.jobs.as_ref().map(|jobs| {
            jobs.send(Job {
                seq,
                at,
                bounds,
                reply,
            })
        });
        match sent {
            Some(Ok(())) => answer
                .await
                .unwrap_or_else(|_| Err(error::database("the history feeder stopped"))),
            _ => Err(error::database("the history feeder is not running")),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // The thread ends when its inbox is closed; wait for the read it may be in.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The worker's loop: one chunk at a time, loaded when a job asks for another number than the one it holds. The
/// chunk is let go of (W4 fix MEDIUM-2) when its last row was read (`Next::End`), when it stopped reading, when
/// another chunk is asked for, and after `idle` without a job: an account whose delivery is quiet holds none.
/// `released` counts the chunks let go of.
fn work(
    stage: &StageHandle,
    ctx: &Ctx,
    clock: &dyn Clock,
    inbox: &Receiver<Job>,
    idle: Duration,
    released: &AtomicUsize,
) {
    let mut pending: Option<Job> = None;
    loop {
        let Some(job) = pending.take().or_else(|| inbox.recv().ok()) else {
            return;
        };
        let seq = job.seq;
        let loaded = match rows::load(stage, seq) {
            Ok(Some(loaded)) => loaded,
            other => {
                let _ = job.reply.send(other.map(|_| Read::Gone));
                continue;
            }
        };
        let head = Head {
            msg_id: loaded.msg_id.clone(),
            sync_type: loaded.sync_type,
            order: loaded.order,
            progress: loaded.progress,
            skipped: loaded.skipped,
        };
        let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
        let mut reader = Reader::new(&loaded.bytes, &loaded.entries, ctx, now);
        let mut job = job;
        loop {
            let read = match reader.next(job.at, job.bounds) {
                Ok(found) => Read::Row {
                    head: head.clone(),
                    found,
                },
                Err(bad) => Read::Unreadable(bad),
            };
            // The last row was read, or the chunk does not read: nothing more is asked of it here.
            let finished = matches!(
                read,
                Read::Unreadable(_)
                    | Read::Row {
                        found: Next::End,
                        ..
                    }
            );
            let _ = job.reply.send(Ok(read));
            if finished {
                break;
            }
            match inbox.recv_timeout(idle) {
                Ok(next) if next.seq == seq => job = next,
                Ok(next) => {
                    pending = Some(next);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        // `loaded` and the reader go here, at the end of the turn.
        drop(reader);
        drop(loaded);
        released.fetch_add(1, Ordering::SeqCst);
    }
}

/// The real [`Rows`] of one account.
pub(crate) struct Feeder {
    stage: StageHandle,
    store: KourierStore,
    worker: Worker,
    clock: Arc<dyn Clock>,
}

impl Feeder {
    /// A feeder over the account's stage and books; `ctx` names our own ids for the map, `clock` stamps the
    /// rows' arrival.
    pub(crate) fn new(
        stage: StageHandle,
        store: KourierStore,
        ctx: Ctx,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let worker = Worker::start(stage.clone(), ctx, Arc::clone(&clock));
        Self {
            stage,
            store,
            worker,
            clock,
        }
    }

    /// The row to append while the cursor stands inside a chunk.
    async fn inside(&self, at: Cursor) -> Result<(Source, Row), StoreError> {
        let read = self.worker.read(at.chunk, at, Bounds::DEFAULT).await?;
        self.row_or_end(at, read).await
    }

    /// The row of a `Read` at `at`, or the row that finishes the chunk, or the row that gives it up.
    async fn row_or_end(&self, at: Cursor, read: Read) -> Result<(Source, Row), StoreError> {
        match read {
            Read::Row {
                head,
                found: Next::Row(row),
            } => Ok((head.source(), row)),
            Read::Row {
                head,
                found: Next::End,
            } => Ok((head.source(), finish(at, Some(head.end())))),
            Read::Gone => Ok((nothing(), finish(at, None))),
            Read::Unreadable(bad) => {
                self.set_aside(at.chunk, bad).await?;
                Ok((nothing(), finish(at, None)))
            }
        }
    }

    /// The first row of the next chunk, if there is one.
    async fn begin(&self) -> Result<Option<(Source, Row)>, StoreError> {
        let stage = self.stage.clone();
        let report = tokio::task::spawn_blocking(move || feed::plan_pending(&stage))
            .await
            .map_err(|_| error::database("the history planner stopped"))??;
        if report.bad > 0 {
            self.count_bad(report.bad).await?;
        }
        let facts = self.facts().await?;
        let Some(seq) = facts.next_chunk() else {
            return Ok(None);
        };
        let at = Cursor::start(seq);
        let read = self.worker.read(seq, at, Bounds::DEFAULT).await?;
        self.row_or_end(at, read).await.map(Some)
    }

    /// The end row owed at the end of the chunk the cursor stands at, if one is.
    async fn ending_row(
        &self,
        stored: Cursor,
        facts: &Facts,
    ) -> Result<Option<(Source, Row)>, StoreError> {
        let over = facts.over_types();
        // Nothing can still arrive and the push is not running (F6): every chat that owes an end gets it.
        let everything = facts.account_over();
        let chunk = stored.chunk;
        let convs = self
            .store
            .read(move |conn| ending(conn, chunk, &over, everything))
            .await?;
        if convs.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            nothing(),
            Row {
                before: stored,
                after: Cursor::at(chunk, Phase::End, 0, 0),
                body: Body::End {
                    finished: None,
                    convs,
                },
            },
        )))
    }

    /// The stage's chunks, the chunks delivered and the notices still open.
    async fn facts(&self) -> Result<Facts, StoreError> {
        let stage = self.stage.clone();
        let staged = tokio::task::spawn_blocking(move || read_staged(&stage))
            .await
            .map_err(|_| error::database("the history stage read stopped"))??;
        let (delivered, announced, lost) = self
            .store
            .read(|conn| {
                Ok((
                    read_delivered(conn)?,
                    read_announced(conn)?,
                    read_lost(conn)?,
                ))
            })
            .await?;
        Ok(Facts {
            staged,
            delivered,
            announced,
            lost,
            now_ms: i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX),
        })
    }

    /// A chunk that does not read: kept aside in the stage, counted.
    async fn set_aside(&self, seq: i64, bad: Bad) -> Result<(), StoreError> {
        tracing::warn!(
            kind = bad.word(),
            "a history chunk stopped reading and is kept aside"
        );
        self.stage
            .write_at(
                Priority::Low,
                "set a history chunk aside",
                None,
                move |tx| {
                    exec(
                        tx,
                        "set a history chunk aside",
                        "UPDATE chunks SET state = 'bad', bad_at = ?2 WHERE seq = ?1 AND state = 'planned'",
                        (seq, kourier_db::now_ms()),
                    )
                    .map(drop)
                },
            )
            .await?;
        self.count_bad(1).await
    }

    async fn count_bad(&self, n: u32) -> Result<(), StoreError> {
        self.store
            .sweep("count history chunks that did not read", move |tx| {
                exec(
                    tx,
                    "count history chunks that did not read",
                    "UPDATE history_counts SET bad = bad + ?1 WHERE id = 1",
                    [n],
                )
                .map(drop)
            })
            .await
    }
}

impl Rows for Feeder {
    async fn next(&mut self, stored: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        if stored.chunk != 0 && stored.phase != Phase::End {
            return self.inside(stored).await.map(Some);
        }
        if stored.chunk != 0 {
            let facts = self.facts().await?;
            if facts.owes_record(stored.chunk) {
                // The last content row moved the cursor to the end of the chunk; the chunk itself is
                // recorded as delivered by the row that stands at its end.
                return self.inside(stored).await.map(Some);
            }
            if let Some(row) = self.ending_row(stored, &facts).await? {
                return Ok(Some(row));
            }
        }
        self.begin().await
    }

    async fn single(&mut self, at: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        match self.worker.read(at.chunk, at, Bounds::SINGLE).await? {
            Read::Row {
                head,
                found: Next::Row(row),
            } => Ok(Some((head.source(), row))),
            _ => Ok(None),
        }
    }
}

/// The source of a row that belongs to no chunk's content (an end row, a chunk given up).
fn nothing() -> Source {
    Source {
        msg_id: String::new(),
        sync_type: None,
        progress: None,
    }
}

/// The row that moves the cursor from `at` to the end of its chunk. With `finished` it also records the
/// chunk as delivered; without, the chunk is given up.
fn finish(at: Cursor, finished: Option<ChunkEnd>) -> Row {
    Row {
        before: at,
        after: Cursor::at(at.chunk, Phase::End, 0, 0),
        body: Body::End {
            finished,
            convs: Vec::new(),
        },
    }
}

/// The conversations whose push is over now, at most [`END_PER_ROW`], in the order of their ids: those that
/// the chunk `chunk` just delivered said it has no more of, and those whose last chunk was of a sync type in
/// `over`; with `everything` (the account's push is over, F6) every conversation that owes an end.
fn ending(
    conn: &Connection,
    chunk: i64,
    over: &[Option<i32>],
    everything: bool,
) -> Result<Vec<ConvKey>, StoreError> {
    let mut statement = conn
        .prepare_cached(
            "SELECT conv, ended, last_chunk, sync_type FROM history_chats WHERE more = 1 ORDER BY conv",
        )
        .map_err(sql("read the conversations a push has not ended"))?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i32>>(3)?,
            ))
        })
        .map_err(sql("read the conversations a push has not ended"))?;
    let mut convs = Vec::new();
    for row in rows {
        let (conv, ended, last_chunk, sync_type) =
            row.map_err(sql("read the conversations a push has not ended"))?;
        if !(everything || (ended && last_chunk == chunk) || over.contains(&sync_type)) {
            continue;
        }
        // A text that is no conversation cannot be ended; it stays out of the way.
        if let Ok(conv) = ConvKey::parse(&conv) {
            convs.push(conv);
            if convs.len() >= END_PER_ROW {
                break;
            }
        }
    }
    Ok(convs)
}

fn read_staged(stage: &StageHandle) -> Result<Vec<Staged>, StoreError> {
    let conn = stage
        .reader()
        .map_err(|_| error::database("the history stage could not be read"))?;
    let mut statement = conn
        .prepare_cached(
            "SELECT seq, msg_id, sync_type, chunk_order, state, captured_at FROM chunks ORDER BY seq",
        )
        .map_err(sql("read the history chunks"))?;
    let rows = statement
        .query_map([], |r| {
            Ok(Staged {
                seq: r.get(0)?,
                msg_id: r.get(1)?,
                sync_type: r.get(2)?,
                order: r.get(3)?,
                state: State::of_text(&r.get::<_, String>(4)?),
                captured_at: r.get(5)?,
            })
        })
        .map_err(sql("read the history chunks"))?;
    rows.collect::<Result<_, _>>()
        .map_err(sql("read the history chunks"))
}

fn read_delivered(conn: &Connection) -> Result<Vec<Delivered>, StoreError> {
    let mut statement = conn
        .prepare_cached("SELECT chunk, sync_type, chunk_order, progress FROM history_chunks")
        .map_err(sql("read the delivered history chunks"))?;
    let rows = statement
        .query_map([], |r| {
            Ok(Delivered {
                chunk: r.get(0)?,
                sync_type: r.get(1)?,
                order: r.get(2)?,
                progress: r.get(3)?,
            })
        })
        .map_err(sql("read the delivered history chunks"))?;
    rows.collect::<Result<_, _>>()
        .map_err(sql("read the delivered history chunks"))
}

fn read_lost(conn: &Connection) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT lost > 0 FROM history_counts WHERE id = 1",
        [],
        |r| r.get::<_, bool>(0),
    )
    .map_err(sql("read the lost history count"))
}

fn read_announced(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut statement = conn
        .prepare_cached("SELECT msg_id FROM history_announced WHERE lost = 0")
        .map_err(sql("read the announced history chunks"))?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql("read the announced history chunks"))?;
    rows.collect::<Result<_, _>>()
        .map_err(sql("read the announced history chunks"))
}

#[cfg(test)]
mod tests;
