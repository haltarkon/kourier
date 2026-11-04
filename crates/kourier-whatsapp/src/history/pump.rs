//! One step of the delivery (W4, C14): the loop the actor's driver runs, cut into steps a test can take one at
//! a time.
//!
//! A [`Pump::step`] does, in this order:
//! 1. SETTLE ([`super::settle::settle`]): the rows the core answered for leave the books, a refused one is
//!    taken back. The running time without an answer is counted here ([`Idle`]); at the bound the oldest
//!    waiting row is taken back ([`settle::strike`]).
//! 2. WINDOW: with [`Limits::window`] rows still unsettled nothing is appended ([`Step::Wait`]).
//! 3. A taken-back row waits: its next unit is read again from the stage and appended as a row of its own
//!    ([`deliver::redo`]), before anything new, so a refused row is cut into singles one unit at a time.
//! 4. Else the next row after the stored cursor ([`deliver::append`]).
//!
//! The rows themselves come from a [`Rows`]: the feeder, which chooses the chunk and reads the stage on a
//! blocking thread. A step does nothing while it has nothing to do: [`Step::Quiet`] says nothing waits and
//! nothing can be fed, [`Step::Wait`] that rows wait for the core (for at most `left`, then look again:
//! the idle time is counted by looks).
//!
//! [`Pump::start`] counts a start of the application ([`settle::note_start`]) and takes back a row found
//! unanswered at too many of them.
//!
//! Nothing here logs an id, a name or a body.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::Clock;
use wacore::store::error::StoreError;

use super::deliver::{self, KIND_CHATS, KIND_MESSAGES, KIND_PEOPLE, Outcome, Own, Source};
use super::rows::{Body, Cursor, Phase, Row};
use super::settle::{self, Entry, Idle, Limits};
use crate::error::sql;
use crate::store::KourierStore;

/// Where the rows come from: the feeder. Reading is a function of the chunk and the cursor, so a row asked
/// for twice is the same row.
pub(crate) trait Rows {
    /// The row that follows the STORED cursor, with the chunk it belongs to: the next row of the chunk the
    /// cursor stands in, or, once that chunk is finished (or none was started: chunk 0), the first row of the
    /// next one. `None`: nothing to deliver now.
    fn next(
        &mut self,
        cursor: Cursor,
    ) -> impl Future<Output = Result<Option<(Source, Row)>, StoreError>> + Send;

    /// The row of ONE unit that begins at `at` (a taken-back row is read again so), or `None` when the chunk
    /// holds nothing there any more.
    fn single(
        &mut self,
        at: Cursor,
    ) -> impl Future<Output = Result<Option<(Source, Row)>, StoreError>> + Send;
}

/// What a step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Something changed (a row appended, answers settled, a row taken back): step again at once.
    /// `seq` is the new row's place in `core_outbox`: wake the forwarder.
    Progress { seq: Option<i64> },
    /// Rows wait for the core, or the window is full: wait for an answer, at most `left`, then step.
    Wait { left: Duration },
    /// Nothing waits and nothing can be fed: wait for a new chunk.
    Quiet,
}

/// The delivery of one account.
pub(crate) struct Pump<R> {
    store: KourierStore,
    rows: R,
    own: Own,
    clock: Arc<dyn Clock>,
    limits: Limits,
    idle: Idle,
    /// Rows in a row that did not begin where they were meant to.
    out_of_step: u32,
}

impl<R: Rows> Pump<R> {
    /// A start of the delivery: counts the start, takes back a row found unanswered at too many starts.
    pub(crate) async fn start(
        store: KourierStore,
        rows: R,
        own: Own,
        clock: Arc<dyn Clock>,
        limits: Limits,
    ) -> Result<Self, StoreError> {
        settle::note_start(&store).await?;
        let ran = settle::load_idle(&store).await?;
        let now = clock.now_ms();
        let mut pump = Self {
            store,
            rows,
            own,
            clock,
            limits,
            idle: Idle::new(ran, now),
            out_of_step: 0,
        };
        let struck = settle::strike(&pump.store, Some(limits.max_starts)).await?;
        if struck.taken > 0 {
            tracing::warn!(
                split = struck.split,
                "a history row stayed unacknowledged over too many starts; taken back"
            );
            pump.idle.zeroed(now);
        }
        Ok(pump)
    }

    /// Does the next thing there is to do (module docs).
    pub(crate) async fn step(&mut self) -> Result<Step, StoreError> {
        let found = settle::settle(&self.store).await?;
        let now = self.clock.now_ms();
        if found.answered > 0 {
            // The settling zone wrote the reset itself.
            self.idle.zeroed(now);
        } else if found.waiting == 0 || !found.front {
            // Nothing waits, or a row of the live path is ahead of the history: what the core does not
            // answer is not the history's fault.
            self.idle.reset(now);
        } else {
            self.idle.tick(now);
        }
        self.idle
            .store(&self.store, now, self.limits.idle_stored_every)
            .await?;
        if found.waiting > 0 && found.front && self.idle.so_far() >= self.limits.idle {
            let struck = settle::strike(&self.store, None).await?;
            self.idle.zeroed(now);
            if struck.taken > 0 {
                tracing::warn!(
                    split = struck.split,
                    "the core answered nothing of the history for too long; its oldest waiting row was taken back"
                );
                return Ok(Step::Progress { seq: None });
            }
        }
        if found.waiting >= self.limits.window.max(1) {
            return Ok(Step::Wait { left: self.left() });
        }
        if found.pending {
            return self.redo().await;
        }
        self.feed(found.waiting).await
    }

    /// How long to wait for an answer before the next look: the rest of the idle bound, but never longer than
    /// the time between two stores of it, so the running time is counted by looks.
    fn left(&self) -> Duration {
        self.limits
            .idle
            .saturating_sub(self.idle.so_far())
            .min(self.limits.idle_stored_every)
            .max(Duration::from_millis(1))
    }

    fn now_ms(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    async fn feed(&mut self, waiting: usize) -> Result<Step, StoreError> {
        let cursor = self.cursor().await?;
        let Some((source, row)) = self.rows.next(cursor).await? else {
            return Ok(if waiting > 0 {
                Step::Wait { left: self.left() }
            } else {
                Step::Quiet
            });
        };
        let outcome =
            deliver::append(&self.store, source, self.own.clone(), row, self.now_ms()).await?;
        self.after(outcome)
    }

    /// The next unit of the oldest taken-back row.
    async fn redo(&mut self) -> Result<Step, StoreError> {
        let Some(entry) = settle::first_pending(&self.store).await? else {
            return Ok(Step::Progress { seq: None });
        };
        let read = self.rows.single(entry.begin()).await?;
        let Some((source, row)) = read.filter(|(_, row)| continues(&entry, row)) else {
            // The chunk holds no unit of it at this place any more: it is redone.
            settle::finish(&self.store, entry.id).await?;
            return Ok(Step::Progress { seq: None });
        };
        let outcome = deliver::redo(
            &self.store,
            source,
            self.own.clone(),
            entry.id,
            row,
            self.now_ms(),
        )
        .await?;
        self.after(outcome)
    }

    fn after(&mut self, outcome: Outcome) -> Result<Step, StoreError> {
        match outcome {
            Outcome::Appended(done) => {
                self.out_of_step = 0;
                Ok(Step::Progress { seq: done.seq })
            }
            Outcome::OutOfStep => {
                // A row of a feeder the stored state has moved away from. A feeder that reads from the
                // stored state cannot be out of step with it: when it is all the same, time after time, the
                // delivery ends instead of turning in a circle.
                self.out_of_step += 1;
                if self.out_of_step > self.limits.max_out_of_step {
                    return Err(settle::out_of_place());
                }
                tracing::warn!("a history row was out of step; reading it anew");
                Ok(Step::Progress { seq: None })
            }
        }
    }

    async fn cursor(&self) -> Result<Cursor, StoreError> {
        self.store
            .read(|conn| {
                conn.query_row(
                    "SELECT chunk, phase, conv, msg FROM history_cursor WHERE id = 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, u32>(2)?,
                            r.get::<_, u32>(3)?,
                        ))
                    },
                )
                .map_err(sql("read the history cursor"))
            })
            .await
            .map(|(chunk, phase, conv, msg)| {
                Cursor::at(
                    chunk,
                    Phase::of_number(phase).unwrap_or(Phase::End),
                    conv,
                    msg,
                )
            })
    }
}

/// Whether `row` is a unit of the taken-back `entry` (the same phase and, for messages, the same chat), not the
/// row that follows it.
fn continues(entry: &Entry, row: &Row) -> bool {
    match &row.body {
        Body::People { .. } => entry.kind == KIND_PEOPLE,
        Body::Chats(_) => entry.kind == KIND_CHATS,
        Body::Messages { conv, .. } => entry.kind == KIND_MESSAGES && *conv == entry.place,
        // An end row is a single: a refused one is dropped, never taken back to be read again.
        Body::End { .. } => false,
    }
}

#[cfg(test)]
mod tests;
