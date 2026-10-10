//! A dedicated writer thread for one connection (the `core.db` writer of §2.5 and §5.4).
//!
//! No blocking rusqlite call ever runs on a tokio worker: async code submits a closure and awaits
//! its result. Jobs have two priorities (live envelopes and UI commands first, then history
//! chunks) and, inside a priority, round-robin across lanes (one lane per account), so one
//! account's history import cannot starve another account's live traffic. Batching several
//! writes into one transaction is the job's business.

use crate::{Error, Result};
use rusqlite::Connection;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

/// Job priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Live envelopes and UI commands.
    High,
    /// History chunks and maintenance.
    Low,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// Round-robin queue of lanes; each lane is FIFO.
#[derive(Default)]
struct LaneQueue {
    lanes: VecDeque<(u64, VecDeque<Job>)>,
}

impl LaneQueue {
    fn push(&mut self, lane: u64, job: Job) {
        match self.lanes.iter_mut().find(|(l, _)| *l == lane) {
            Some((_, jobs)) => jobs.push_back(job),
            None => self.lanes.push_back((lane, VecDeque::from([job]))),
        }
    }

    fn pop(&mut self) -> Option<Job> {
        let (lane, mut jobs) = self.lanes.pop_front()?;
        let job = jobs.pop_front();
        if !jobs.is_empty() {
            self.lanes.push_back((lane, jobs));
        }
        job
    }
}

#[derive(Default)]
struct Queues {
    high: LaneQueue,
    low: LaneQueue,
    closed: bool,
}

struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queues> {
        // A job never runs under this lock, so poisoning cannot leave the queues half-updated.
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Owns the writer thread; dropping it finishes queued jobs and joins the thread.
pub struct Writer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Connection>>,
}

/// Cheap, cloneable handle for submitting jobs.
#[derive(Clone)]
pub struct WriterHandle {
    shared: Arc<Shared>,
}

impl Writer {
    /// Moves `conn` to a new thread named `name`.
    pub fn spawn(name: &str, conn: Connection) -> Result<Self> {
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            ready: Condvar::new(),
        });
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || run(&thread_shared, conn))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// A handle for submitting jobs.
    pub fn handle(&self) -> WriterHandle {
        WriterHandle {
            shared: self.shared.clone(),
        }
    }

    /// Runs the jobs already queued, stops the thread and returns the connection. Later
    /// submissions fail with [`Error::WriterGone`].
    pub fn shutdown(mut self) -> Result<Connection> {
        self.close();
        self.thread
            .take()
            .ok_or(Error::WriterGone)?
            .join()
            .map_err(|_| Error::WriterGone)
    }

    fn close(&self) {
        self.shared.lock().closed = true;
        self.shared.ready.notify_all();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl WriterHandle {
    /// Queues `f` and returns a receiver for its result. The receiver errors when the job
    /// panicked or the writer stopped first.
    pub fn submit<R, F>(&self, priority: Priority, lane: u64, f: F) -> Result<oneshot::Receiver<R>>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = tx.send(f(conn));
        });
        {
            let mut q = self.shared.lock();
            if q.closed {
                return Err(Error::WriterGone);
            }
            match priority {
                Priority::High => q.high.push(lane, job),
                Priority::Low => q.low.push(lane, job),
            }
        }
        self.shared.ready.notify_one();
        Ok(rx)
    }

    /// Runs `f` on the writer thread and awaits its result.
    pub async fn call<R, F>(&self, priority: Priority, lane: u64, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        self.submit(priority, lane, f)?
            .await
            .map_err(|_| Error::WriterGone)
    }

    /// Runs `f` on the writer thread and blocks until it is done. For plain threads only; inside
    /// a tokio runtime use [`call`](Self::call).
    pub fn call_blocking<R, F>(&self, priority: Priority, lane: u64, f: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        self.submit(priority, lane, f)?
            .blocking_recv()
            .map_err(|_| Error::WriterGone)
    }
}

fn run(shared: &Shared, mut conn: Connection) -> Connection {
    loop {
        let job = {
            let mut q = shared.lock();
            loop {
                if let Some(job) = q.high.pop().or_else(|| q.low.pop()) {
                    break Some(job);
                }
                if q.closed {
                    break None;
                }
                q = shared.ready.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(job) = job else { return conn };
        if catch_unwind(AssertUnwindSafe(|| job(&mut conn))).is_err() {
            // The caller sees WriterGone through its dropped sender; the thread lives on. A
            // `Transaction` rolls back on unwind, but a raw BEGIN would still be open.
            tracing::error!("database writer job panicked");
            if !conn.is_autocommit() {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn writer() -> (tempfile::TempDir, Writer) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::open(&dir.path().join("w.db"), &crate::DbKey::from([2; 32])).unwrap();
        (dir, Writer::spawn("test-writer", conn).unwrap())
    }

    /// Blocks the writer until the returned sender is used, so jobs queue up behind it.
    fn block(h: &WriterHandle) -> mpsc::Sender<()> {
        let (tx, rx) = mpsc::channel::<()>();
        h.submit(Priority::High, 0, move |_| {
            let _ = rx.recv();
        })
        .unwrap();
        tx
    }

    #[test]
    fn high_first_then_round_robin_across_lanes() {
        let (_dir, w) = writer();
        let h = w.handle();
        let order = Arc::new(Mutex::new(Vec::new()));
        let release = block(&h);
        let mut rxs = Vec::new();
        for (prio, lane, tag) in [
            (Priority::Low, 1, "history-a1"),
            (Priority::Low, 1, "history-a2"),
            (Priority::Low, 2, "history-b1"),
            (Priority::High, 1, "live-a1"),
            (Priority::High, 1, "live-a2"),
            (Priority::High, 2, "live-b1"),
        ] {
            let order = order.clone();
            rxs.push(
                h.submit(prio, lane, move |_| order.lock().unwrap().push(tag))
                    .unwrap(),
            );
        }
        release.send(()).unwrap();
        for rx in rxs {
            rx.blocking_recv().unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            [
                "live-a1",
                "live-b1",
                "live-a2",
                "history-a1",
                "history-b1",
                "history-a2"
            ]
        );
    }

    #[tokio::test]
    async fn call_runs_on_the_writer_thread() {
        let (_dir, w) = writer();
        let h = w.handle();
        let (name, n) = h
            .call(Priority::High, 7, |conn| {
                let n: i64 = conn.query_row("SELECT 6 * 7", [], |r| r.get(0)).unwrap();
                (std::thread::current().name().map(str::to_owned), n)
            })
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("test-writer"));
        assert_eq!(n, 42);
    }

    #[test]
    fn a_panicking_job_does_not_kill_the_writer() {
        let (_dir, w) = writer();
        let h = w.handle();
        let res = h.call_blocking(Priority::High, 0, |conn| {
            conn.execute_batch("BEGIN; CREATE TABLE t(x);").unwrap();
            panic!("job bug");
        });
        assert!(matches!(res, Err::<(), _>(Error::WriterGone)));
        let tables: i64 = h
            .call_blocking(Priority::High, 0, |conn| {
                conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
                    .unwrap()
            })
            .unwrap();
        assert_eq!(tables, 0, "the open transaction was rolled back");
    }

    #[test]
    fn shutdown_finishes_queued_jobs_then_refuses() {
        let (_dir, w) = writer();
        let h = w.handle();
        let release = block(&h);
        let rx = h.submit(Priority::Low, 0, |_| 5).unwrap();
        release.send(()).unwrap();
        let conn = w.shutdown().unwrap();
        assert_eq!(rx.blocking_recv().unwrap(), 5);
        assert!(conn.is_autocommit());
        assert!(matches!(
            h.submit(Priority::High, 0, |_| ()),
            Err(Error::WriterGone)
        ));
    }
}
