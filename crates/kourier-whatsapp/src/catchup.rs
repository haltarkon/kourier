//! The catch-up machine of one connection (W3a, chunk C7; architecture §4.5; the Signal backend's
//! `CatchUp` in `kourier-signal/src/connection.rs` is the model).
//!
//! The account shows `Syncing` from the moment its connection is up until everything the server held
//! for it is durable here AND the core has it, and only then `Online`. The machine OWNS that
//! transition (decision D2): `states::state_of(Connected)` says `Syncing`, never `Online`.
//!
//! Two facts per connection, kept apart because upstream does not order them: whether `Connected` was
//! seen, and how far the drain is. At the pin `Connected` is dispatched AFTER the offline wait
//! (`node_io.rs` post-login task), so normally `OfflineSyncCompleted` comes first; when the drain's
//! tail is stuck, `Connected` comes first and the completion later. `Online` needs both.
//!
//! The drain's phases:
//!
//! - [`Phase::Backlog`]: the drain is not known to be over.
//! - [`Phase::Stage2`]: Kourier's own BOUNDARY is taken (`catch_up.boundary`, the inbox's arrival
//!   counter at that moment, written by the handler in the writer's order so every hook commit
//!   before it is `<=` it and every one after is `>` it); stage 2 still has rows up to it.
//! - [`Phase::Core`]: stage 2 is through; the core has not yet acknowledged the `core_outbox` rows
//!   that existed at that moment (`seq <= target`).
//! - [`Phase::Through`]: everything up to the boundary is committed in the core.
//!
//! `OfflineSyncCompleted` is NOT proof that the drain is durable (decision D5): upstream also
//! dispatches it after a failed or held-back tail commit and when its own 60 s wait ran out. So the
//! boundary is Kourier's: rows that arrive after it with `is_offline` (a held tail, the rest of a
//! drain that timed out) keep `catch_up = 1` (`Inbox::commit`) and `Online` does not wait for them.
//! A hook or handler write that fails ends the connection (D5), so a boundary is never taken over a
//! failed commit of this connection.
//!
//! A NEW CONNECTION WITHOUT A `Disconnected` (fix F4): upstream's keepalive watchdog and its critical
//! app-state sync timeout reconnect at once and dispatch no `Disconnected`, so the machine cannot rely
//! on that signal to start over. Upstream reports ONE end of a drain per connection (a completion or an
//! interruption), so a preview or a completion that arrives after one, or a `Connected` after a
//! `Connected`, belongs to a NEW connection ([`CatchUp::drain_ended`], [`CatchUp::has_connected`]): the
//! actor resets the machine (no ledger sweep for that connection) and takes the new boundary.
//!
//! A drain that TIMED OUT (fix F7): upstream dispatches `OfflineSyncCompleted` after its own 60 s wait
//! with the number of stanzas it had processed, less than the preview's total. Such a completion is
//! not the end of the drain ([`CatchUp::drain_over`]): the account does not count it as completed, so
//! the ledger sweep waits for a connection whose drain really ended.
//!
//! If upstream's completion never comes (a bug or a stuck finisher), the account must not stay
//! `Syncing` for ever with every message hidden as catch-up: [`Timing::backlog_wait`] after the
//! connection is up, the machine takes the boundary itself and goes on (logged).
//!
//! The machine holds no I/O of its own beyond [`CatchUp::advance`], which reads the store; the actor
//! drives it from the signals, stage 2's progress and the core's acknowledgements.
//!
//! Logs carry counts only.

use std::time::Duration;

use kourier_db::rusqlite::{Connection, Transaction};
use wacore::store::error::StoreError;

use crate::error::sql;
use crate::store::{self, KourierStore};

/// The waits of one machine; the defaults are the production ones.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// How long after the connection is up the machine waits for upstream's completion before it
    /// takes the boundary itself. Upstream's own wait is 60 s and then waits one more window for its
    /// finisher: this is longer than both of them started at `Connected`.
    pub(crate) backlog_wait: Duration,
    /// How long after a failed read of the store the machine looks again.
    pub(crate) recheck: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backlog_wait: Duration::from_secs(90),
            recheck: Duration::from_secs(5),
        }
    }
}

/// How far the drain of the current connection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Backlog,
    Stage2 { boundary: i64 },
    Core { target: i64 },
    Through,
}

/// What the machine wants the account to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// The connection is not known to be up: the machine says nothing.
    Nothing,
    Syncing,
    Online,
}

/// The machine of one connection. Reset whenever the connection or the client ends.
#[derive(Debug)]
pub(crate) struct CatchUp {
    phase: Phase,
    /// `Connected` was seen on this connection.
    connected: bool,
    /// A drain was announced or its boundary taken: the socket is authenticated.
    draining: bool,
    /// Upstream said this connection's offline drain is complete (`OfflineSyncCompleted`) and it was
    /// not the completion of its own 60 s wait (fix F7): the ledger sweep needs this.
    completed: bool,
    /// The drain of this connection has had its one end (a completion, whichever, or an
    /// interruption): anything of the drain that comes next is a new connection's (fix F4).
    ended: bool,
    /// The `total` of this connection's preview, for the check of a timed-out completion.
    preview_total: Option<i32>,
    /// The arrival counter when this connection began: every row up to it was committed by an
    /// earlier connection (the ledger sweep's bound).
    covers: Option<i64>,
}

impl Default for CatchUp {
    fn default() -> Self {
        Self::new()
    }
}

impl CatchUp {
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Backlog,
            connected: false,
            draining: false,
            completed: false,
            ended: false,
            preview_total: None,
            covers: None,
        }
    }

    /// The connection (or the client) ended: the next one starts over.
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    /// The arrival counter at the start of this connection (read right after [`reset`](Self::reset)).
    pub(crate) fn cover(&mut self, arrival: i64) {
        self.covers = Some(arrival);
    }

    /// Upstream dispatched `OfflineSyncCompleted` with `count` stanzas: the drain has had its end.
    /// It is COMPLETE unless the count is below the preview's total, which is what upstream's own
    /// 60 s wait reports (fix F7; without a preview there is nothing to compare, and it counts).
    pub(crate) fn drain_over(&mut self, count: i32) {
        self.draining = true;
        self.ended = true;
        if self.preview_total.is_none_or(|total| count >= total) {
            self.completed = true;
        }
    }

    /// The drain of this connection has had its end (a completion or an interruption).
    pub(crate) fn drain_ended(&self) -> bool {
        self.ended
    }

    /// `Connected` was seen on this connection.
    pub(crate) fn has_connected(&self) -> bool {
        self.connected
    }

    /// The arrival number up to which the inbox ledger may be swept, ONCE per connection, and only
    /// when upstream said the drain is complete and Kourier's own side is through it (stage 2 and the
    /// core): a later connection's drain completed durably (decision D7). `None` before that, after
    /// it was taken, and when the counter at the connection's start is unknown.
    pub(crate) fn take_sweep(&mut self) -> Option<i64> {
        if self.completed && self.phase == Phase::Through {
            self.covers.take()
        } else {
            None
        }
    }

    /// The socket is authenticated, as far as the machine knows.
    pub(crate) fn is_up(&self) -> bool {
        self.connected || self.draining
    }

    /// `Connected` came.
    pub(crate) fn connected(&mut self) {
        self.connected = true;
    }

    /// A drain of `total` stanzas was announced: it starts over.
    pub(crate) fn previewed(&mut self, total: i32) {
        self.draining = true;
        self.phase = Phase::Backlog;
        self.preview_total = Some(total);
    }

    /// The drain ended without its marker: not caught up (the connection went away; what comes next
    /// is a new connection's).
    pub(crate) fn interrupted(&mut self) {
        self.phase = Phase::Backlog;
        self.ended = true;
    }

    /// The boundary of the drain is taken. Only the first one of a drain counts.
    pub(crate) fn boundary_taken(&mut self, boundary: i64) {
        if self.phase == Phase::Backlog {
            self.draining = true;
            self.phase = Phase::Stage2 { boundary };
        }
    }

    /// Whether stage 2's progress can move the machine.
    pub(crate) fn waits_for_stage2(&self) -> bool {
        matches!(self.phase, Phase::Stage2 { .. })
    }

    /// What the account shows: `Online` needs the connection AND the drain.
    pub(crate) fn shown(&self) -> Shown {
        match (self.connected, self.phase) {
            (true, Phase::Through) => Shown::Online,
            _ if self.is_up() => Shown::Syncing,
            _ => Shown::Nothing,
        }
    }

    /// Moves the machine as far as the store lets it: stage 2 through the boundary, then the core
    /// through the rows that were written until then. `Err` leaves the phase as it was.
    pub(crate) async fn advance(
        &mut self,
        store: &KourierStore,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        if let Phase::Stage2 { boundary } = self.phase {
            match store
                .read(move |conn| through(conn, boundary, now_ms))
                .await?
            {
                Some(target) => self.phase = Phase::Core { target },
                None => return Ok(()),
            }
        }
        if let Phase::Core { target } = self.phase
            && store.read(move |conn| core_waiting(conn, target)).await? == 0
        {
            self.phase = Phase::Through;
        }
        Ok(())
    }
}

/// The highest arrival number ever given (`AUTOINCREMENT` numbers are never reused), 0 for none.
const COUNTER: &str = "coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'inbox'), 0)";

/// The arrival counter now: a connection that begins here has every earlier row below it.
pub(crate) async fn counter(store: &KourierStore) -> Result<i64, StoreError> {
    store
        .read(|conn| {
            conn.query_row(&format!("SELECT {COUNTER}"), [], |r| r.get(0))
                .map_err(sql("read the arrival counter"))
        })
        .await
}

/// The boundary of the current connection's drain, `None` while it is not known to be over.
pub(crate) async fn boundary(store: &KourierStore) -> Result<Option<i64>, StoreError> {
    store
        .read(|conn| {
            Ok(store::first(
                conn,
                "read catch-up boundary",
                "SELECT boundary FROM catch_up WHERE id = 1",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .flatten())
        })
        .await
}

/// Takes the boundary now: the arrival counter at this point of the writer's order. Returns it.
pub(crate) async fn take_boundary(store: &KourierStore) -> Result<i64, StoreError> {
    store.write("take catch-up boundary", take).await
}

/// Forgets the boundary: the next drain is not known to be over (every new connection starts
/// there, §4.5).
pub(crate) async fn forget_boundary(store: &KourierStore) -> Result<(), StoreError> {
    store.write("forget catch-up boundary", forget).await
}

/// The zone body of [`take_boundary`], also queued by the handler (`Inbox::take_boundary_detached`).
pub(crate) fn take(tx: &Transaction<'_>) -> Result<i64, StoreError> {
    let counter: i64 = tx
        .query_row(&format!("SELECT {COUNTER}"), [], |r| r.get(0))
        .map_err(sql("read the arrival counter"))?;
    store::exec(
        tx,
        "take catch-up boundary",
        "UPDATE catch_up SET boundary = ?1 WHERE id = 1",
        [counter],
    )?;
    Ok(counter)
}

/// The zone body of [`forget_boundary`], also queued by the handler.
pub(crate) fn forget(tx: &Transaction<'_>) -> Result<(), StoreError> {
    store::exec(
        tx,
        "forget catch-up boundary",
        "UPDATE catch_up SET boundary = NULL WHERE id = 1",
        [],
    )
    .map(drop)
}

/// `None` while stage 2 still has a due row up to `boundary` (a parked row is retried later and
/// does not hold `Online`); else the highest `core_outbox` seq written until now, read in the same
/// snapshot. `core_outbox.seq` is `AUTOINCREMENT` (kourier-db migration 3), so a seq is never
/// reused and the bound alone says which rows are meant.
fn through(conn: &Connection, boundary: i64, now_ms: i64) -> Result<Option<i64>, StoreError> {
    let due: i64 = conn
        .query_row(
            "SELECT count(*) FROM inbox WHERE state = 'new' AND arrival <= ?1 AND parked_until <= ?2",
            [boundary, now_ms],
            |r| r.get(0),
        )
        .map_err(sql("count inbox rows up to the boundary"))?;
    if due > 0 {
        return Ok(None);
    }
    conn.query_row("SELECT coalesce(max(seq), 0) FROM core_outbox", [], |r| {
        r.get(0)
    })
    .map(Some)
    .map_err(sql("read core_outbox"))
}

/// How many `core_outbox` rows up to `target` the core has not acknowledged yet (rows are deleted
/// after the ack, or moved to the poison table when the core refused them for good).
fn core_waiting(conn: &Connection, target: i64) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT count(*) FROM core_outbox WHERE seq <= ?1",
        [target],
        |r| r.get(0),
    )
    .map_err(sql("read core_outbox"))
}

#[cfg(test)]
mod tests;
