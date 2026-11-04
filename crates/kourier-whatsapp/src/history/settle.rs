//! Settling the delivery rows (W4, C14): which rows the core has answered for, and what is done about a row it
//! does not take.
//!
//! A delivery row ([`super::deliver`]) is in `core_outbox` until the forwarder has the core's answer; its
//! entry in `history_delivery` stays until we know which answer it was. SETTLED means GONE FROM
//! `core_outbox`, and then the row's `seq` is looked up in `core_outbox_poison`: not there, the core took
//! it; there, the core refused it for good. (Never [`crate::outbox::wait_acked`]: it returns when the
//! forwarder is gone and cannot tell an acknowledged row from a quarantined one.) The `seq` of a row is never
//! reused (`core_outbox` is AUTOINCREMENT), so an entry cannot be confused with a later row.
//!
//! BACK-PRESSURE: fewer than [`WINDOW`] rows may be unsettled before the next one is appended, so a live
//! message behind the history waits behind two rows at most (the forwarder ships in `seq` order).
//!
//! TAKING A ROW BACK. A row the core refused for good, a row found unacknowledged at more than
//! [`MAX_STARTS`] starts of the application, and the OLDEST waiting row after [`IDLE`] of running time without
//! any answer of the core are taken back: the row leaves `core_outbox` (and its poison copy goes: a refused
//! history must not pile up in `backend.db` with its content), its messages leave the counts, and
//!
//! - a row of several units becomes a PENDING entry (`seq` NULL): [`super::pump`] reads it again from the
//!   stage one unit at a time, as new rows, under the window ([`super::deliver::redo`]);
//! - a single unit is DROPPED and counted (`history_counts.dropped` for a message; a chat, a person or an
//!   end row is logged by kind only).
//!
//! Only the oldest row can be at fault: the forwarder ships in order and the core stops at the first job
//! that fails, so of two waiting rows only the first can be what the core is stuck on. A row of SEVERAL units
//! behind the oldest is split too (it loses nothing; what the core already has is ignored), or it would be
//! shipped with the first singles of the oldest row's content and cost an [`IDLE`] of its own. The bounds
//! count only while the row is the FRONT of `core_outbox`: a live row ahead that the core does not take is
//! not the history's fault, and the history must not be dropped for it.
//!
//! What is not here, on purpose (decision D4): Signal's strike counter, its 60-minute stuck bound and its
//! `Incomplete` end. Those end a transfer that holds live receiving; this push holds nothing.
//!
//! Nothing here logs an id, a name or a body.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use kourier_model::Patch;
use wacore::store::error::StoreError;

use super::deliver::{KIND_CHATS, KIND_MESSAGES, KIND_PEOPLE};
use super::rows::{Cursor, Phase};
use crate::db::Points;
use crate::error::{self, sql};
use crate::failpoints::{AFTER_HISTORY_SETTLE, INSIDE_HISTORY_SETTLE};
use crate::store::people::{self, NameKind};
use crate::store::{KourierStore, exec};

/// Delivery rows that may be unsettled at a time.
pub(crate) const WINDOW: usize = 2;

/// A row found unacknowledged at the front of `core_outbox` at more starts of the application than this is
/// taken back (a row that crashes the process at the core's ingest would otherwise be shipped first at every
/// start, for ever).
pub(crate) const MAX_STARTS: u32 = 3;

/// The running time without any answer of the core after which the oldest waiting row is taken back.
pub(crate) const IDLE: Duration = Duration::from_secs(10 * 60);

/// How often the running time without an answer is stored (a quit loses at most this much of it).
pub(crate) const IDLE_STORED_EVERY: Duration = Duration::from_secs(30);

/// Rows in a row that did not begin where they were meant to (the stored cursor, or a taken-back row's entry,
/// had moved), after which the delivery gives up: a fault of this program must not turn in a circle.
pub(crate) const MAX_OUT_OF_STEP: u32 = 3;

/// The longest stretch one look at the clock adds to the running time. A machine that slept, or a driver that
/// was parked for the link hold, did not RUN for that time.
const MAX_TICK: Duration = Duration::from_secs(60);

/// The bounds of the delivery; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// [`WINDOW`].
    pub(crate) window: usize,
    /// [`MAX_STARTS`].
    pub(crate) max_starts: u32,
    /// [`IDLE`].
    pub(crate) idle: Duration,
    /// [`IDLE_STORED_EVERY`].
    pub(crate) idle_stored_every: Duration,
    /// [`MAX_OUT_OF_STEP`].
    pub(crate) max_out_of_step: u32,
}

impl Limits {
    /// What the app uses.
    pub(crate) const DEFAULT: Self = Self {
        window: WINDOW,
        max_starts: MAX_STARTS,
        idle: IDLE,
        idle_stored_every: IDLE_STORED_EVERY,
        max_out_of_step: MAX_OUT_OF_STEP,
    };
}

const POINTS: Points = Points {
    inside: INSIDE_HISTORY_SETTLE,
    after: AFTER_HISTORY_SETTLE,
};

/// One unsettled delivery row, or a taken-back one waiting to be appended again.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// `history_delivery.id`: the order the rows were appended in.
    pub(crate) id: i64,
    /// Its row in `core_outbox`; `None`: taken back, to be appended again as singles.
    pub(crate) seq: Option<i64>,
    pub(crate) chunk: i64,
    /// The phase number of the first unit the row held (`deliver::KIND_*`; 3: the chunk's end rows).
    pub(crate) kind: i64,
    /// The chat's place in the chunk's plan (messages rows).
    pub(crate) place: u32,
    /// Where in its phase the row began.
    pub(crate) first: u32,
    /// Where the row left the cursor.
    pub(crate) end: Cursor,
    /// One unit: it cannot be cut further.
    pub(crate) single: bool,
    /// The messages the row handed to the core.
    pub(crate) messages: i64,
    pub(crate) starts: u32,
    /// Still in `core_outbox`.
    pub(crate) waiting: bool,
    /// In `core_outbox_poison`: the core refused it for good.
    pub(crate) poisoned: bool,
    /// Nothing is ahead of it in `core_outbox`: if the core is stuck, this row is what it is stuck on.
    pub(crate) front: bool,
}

impl Entry {
    /// The cursor the row began at (and a pending entry is read again from).
    pub(crate) fn begin(&self) -> Cursor {
        Cursor::at(
            self.chunk,
            Phase::of_number(self.kind).unwrap_or(Phase::End),
            if self.kind == KIND_MESSAGES {
                self.place
            } else {
                0
            },
            self.first,
        )
    }
}

/// `a` is at or past `b` in the order of a chunk's rows.
fn reached(a: Cursor, b: Cursor) -> bool {
    (a.phase, a.conv, a.msg) >= (b.phase, b.conv, b.msg)
}

const SELECT: &str = "SELECT d.id, d.seq, d.chunk, d.kind, d.place, d.first, d.end_phase, d.end_place, \
        d.end_first, d.single, d.messages, d.starts, \
        d.seq IS NOT NULL AND EXISTS (SELECT 1 FROM core_outbox o WHERE o.seq = d.seq), \
        d.seq IS NOT NULL AND EXISTS (SELECT 1 FROM core_outbox_poison p WHERE p.seq = d.seq), \
        d.seq IS NOT NULL AND NOT EXISTS (SELECT 1 FROM core_outbox o WHERE o.seq < d.seq) \
     FROM history_delivery d";

fn entry_of(r: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<Entry> {
    let chunk: i64 = r.get(2)?;
    let end_phase: i64 = r.get(6)?;
    Ok(Entry {
        id: r.get(0)?,
        seq: r.get(1)?,
        chunk,
        kind: r.get(3)?,
        place: r.get(4)?,
        first: r.get(5)?,
        end: Cursor::at(
            chunk,
            Phase::of_number(end_phase).unwrap_or(Phase::End),
            r.get(7)?,
            r.get(8)?,
        ),
        single: r.get::<_, i64>(9)? != 0,
        messages: r.get(10)?,
        starts: r.get(11)?,
        waiting: r.get(12)?,
        poisoned: r.get(13)?,
        front: r.get(14)?,
    })
}

/// Every entry, in the order the rows were appended.
pub(crate) fn entries(conn: &Connection) -> Result<Vec<Entry>, StoreError> {
    let op = "read the history delivery entries";
    let mut stmt = conn
        .prepare_cached(&format!("{SELECT} ORDER BY d.id"))
        .map_err(sql(op))?;
    stmt.query_map([], entry_of)
        .map_err(sql(op))?
        .collect::<Result<_, _>>()
        .map_err(sql(op))
}

/// The pending entry `id` (taken back and not yet appended again).
pub(crate) fn pending(conn: &Connection, id: i64) -> Result<Option<Entry>, StoreError> {
    conn.prepare_cached(&format!("{SELECT} WHERE d.id = ?1 AND d.seq IS NULL"))
        .and_then(|mut s| s.query_row([id], entry_of).optional())
        .map_err(sql("read a pending history delivery entry"))
}

/// The oldest pending entry, if any.
pub(crate) async fn first_pending(store: &KourierStore) -> Result<Option<Entry>, StoreError> {
    store
        .read(|conn| Ok(entries(conn)?.into_iter().find(|entry| entry.seq.is_none())))
        .await
}

/// A unit of the pending `parent` was appended again and the cursor of the row read stands at `after`: the
/// entry is finished once `after` reached its end, else it begins at `after`.
pub(crate) fn advance(
    tx: &Transaction<'_>,
    parent: &Entry,
    after: Cursor,
) -> Result<(), StoreError> {
    if reached(after, parent.end) {
        exec(
            tx,
            "finish a taken-back history row",
            "DELETE FROM history_delivery WHERE id = ?1",
            [parent.id],
        )?;
    } else {
        exec(
            tx,
            "advance a taken-back history row",
            "UPDATE history_delivery SET kind = ?2, place = ?3, first = ?4 WHERE id = ?1",
            params![parent.id, after.phase.number(), after.conv, after.msg],
        )?;
    }
    Ok(())
}

/// The pending entry has nothing more to append (the stage holds no unit at its place any more): it ends.
pub(crate) async fn finish(store: &KourierStore, id: i64) -> Result<(), StoreError> {
    store
        .sweep("finish a taken-back history row", move |tx| {
            exec(
                tx,
                "finish a taken-back history row",
                "DELETE FROM history_delivery WHERE id = ?1 AND seq IS NULL",
                [id],
            )
            .map(drop)
        })
        .await
}

/// What a look at the unsettled rows found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Settled {
    /// Rows the core answered for (taken or refused) and this look settled.
    pub(crate) answered: usize,
    /// Rows still waiting in `core_outbox`.
    pub(crate) waiting: usize,
    /// A taken-back row waits to be appended again.
    pub(crate) pending: bool,
    /// The oldest waiting row is the front of `core_outbox`.
    pub(crate) front: bool,
}

fn summary(all: &[Entry]) -> Settled {
    let mut found = Settled::default();
    for entry in all {
        if entry.seq.is_none() {
            found.pending = true;
        } else if entry.waiting {
            if found.waiting == 0 {
                found.front = entry.front;
            }
            found.waiting += 1;
        } else {
            found.answered += 1;
        }
    }
    found
}

/// Looks at the unsettled rows: one that left `core_outbox` is settled (taken by the core: its entry goes;
/// refused: it is taken back), and the running time without an answer starts over. A look that finds nothing
/// answered writes nothing.
pub(crate) async fn settle(store: &KourierStore) -> Result<Settled, StoreError> {
    let looked = store
        .read(|conn| entries(conn).map(|all| summary(&all)))
        .await?;
    if looked.answered == 0 {
        return Ok(looked);
    }
    store
        .sweep_at("settle history rows", POINTS, |tx| {
            let all = entries(tx)?;
            for entry in all.iter().filter(|e| e.seq.is_some() && !e.waiting) {
                if entry.poisoned {
                    take_back(tx, entry)?;
                } else {
                    exec(
                        tx,
                        "settle a history row",
                        "DELETE FROM history_delivery WHERE id = ?1",
                        [entry.id],
                    )?;
                }
            }
            reset_idle(tx)?;
            // The state after the look: what is left of the entries.
            let mut found = summary(&entries(tx)?);
            found.answered = all.iter().filter(|e| e.seq.is_some() && !e.waiting).count();
            Ok(found)
        })
        .await
}

/// Takes `entry` back (module docs). `entry.seq` is `Some`.
fn take_back(tx: &Transaction<'_>, entry: &Entry) -> Result<(), StoreError> {
    let Some(seq) = entry.seq else {
        return Ok(());
    };
    // A chats row stated hints and features, a messages row may carry the unarchive of `appstate::zone::after_history`
    // (G2): what the core never read is due again.
    // A people row (the chunk's pairs and push names) recorded its pairs as told (fix F12): they go back too (fix F18).
    if entry.kind == KIND_CHATS || entry.kind == KIND_MESSAGES || entry.kind == KIND_PEOPLE {
        let events = events_of_row(tx, seq)?;
        if entry.kind != KIND_PEOPLE {
            forget_hints_of_row(tx, &events)?;
        }
        forget_people_of_row(tx, &events, entry.kind, entry.single, seq)?;
    }
    exec(
        tx,
        "take a history row back",
        "DELETE FROM core_outbox WHERE seq = ?1",
        [seq],
    )?;
    // The refused row's copy is not kept: its content is appended again, or dropped.
    exec(
        tx,
        "forget a refused history row",
        "DELETE FROM core_outbox_poison WHERE seq = ?1",
        [seq],
    )?;
    // The row's messages are not in the core: a redo counts them again.
    exec(
        tx,
        "give back the messages of a taken-back row",
        "UPDATE history_counts SET delivered = max(delivered - ?1, 0) WHERE id = 1",
        [entry.messages],
    )?;
    if entry.single {
        exec(
            tx,
            "drop a history unit",
            "DELETE FROM history_delivery WHERE id = ?1",
            [entry.id],
        )?;
        exec(
            tx,
            "count a dropped history message",
            "UPDATE history_counts SET dropped = dropped + ?1 WHERE id = 1",
            [entry.messages],
        )?;
        tracing::warn!(
            kind = entry.kind,
            "a history unit the core would not take was dropped"
        );
    } else {
        exec(
            tx,
            "split a history row",
            "UPDATE history_delivery SET seq = NULL, starts = 0 WHERE id = ?1",
            [entry.id],
        )?;
    }
    Ok(())
}

/// The chats a taken-back chats row STATED a hint for did not tell the core: their hint is stated again by the
/// redo (or by a later chunk, when the unit was dropped). A chat read here is left alone (nothing is stated
/// after a read). The chats are read from the row itself, in `core_outbox` or, when the core refused it, in
/// `core_outbox_poison`; never every chat's. The features the row stated are forgotten too (W5a, D9). A messages row's
/// `archived: false` (the phone's setting moved the chat out, noticed by the delivery's zone) did not tell the core
/// either: the mirror's archive is put back, so the redo notices it again (G2).
fn forget_hints_of_row(tx: &Transaction<'_>, events: &[BackendEvent]) -> Result<(), StoreError> {
    for event in events.iter().cloned() {
        // The unarchive a history row carried (`appstate::zone::after_history`) did not reach the core: the core still
        // holds the chat archived, so the mirror's archive goes back and the redo says it again (G2, NEW-2). Only that
        // zone puts `archived: false` into a history row.
        if let BackendEvent::ConversationUpsert(patch) = &event
            && patch.archived == Some(false)
        {
            crate::store::chat_state::restore_archive(tx, &patch.key)?;
        }
        // A statement the core never read is due again (W5a, D9): the next start's sweep or the chat's next event.
        if crate::features::is_statement(&event)
            && let BackendEvent::ConversationUpsert(patch) = &event
        {
            exec(
                tx,
                "state a chat's features again",
                "UPDATE features_stated SET version = 0 WHERE conv = ?1",
                [patch.key.encode()],
            )?;
            // What the phone said about the chat rode in the same row (W6a1, H5): the core never heard it either.
            exec(
                tx,
                "state a chat's settings again",
                "UPDATE chat_state SET stated = 0 WHERE chat = ?1",
                [patch.key.encode()],
            )?;
            continue;
        }
        if let BackendEvent::ConversationUpsert(patch) = event
            && patch.unread_hint.is_some()
        {
            exec(
                tx,
                "state a history hint again",
                "UPDATE history_chats SET hint_stated = 0 WHERE conv = ?1 AND read_here = 0",
                [patch.key.encode()],
            )?;
        }
    }
    Ok(())
}

/// The events of the row `seq`, in `core_outbox` or, when the core refused it, in `core_outbox_poison`. Empty for a row this
/// version cannot read (it names nothing it could forget).
fn events_of_row(tx: &Transaction<'_>, seq: i64) -> Result<Vec<BackendEvent>, StoreError> {
    let row: Option<Vec<u8>> = tx
        .query_row(
            "SELECT envelope FROM core_outbox WHERE seq = ?1 \
             UNION ALL SELECT envelope FROM core_outbox_poison WHERE seq = ?1 LIMIT 1",
            [seq],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql("read a history row to take back"))?;
    Ok(row
        .and_then(|bytes| decode_outbox_row(&bytes).ok())
        .unwrap_or_default())
}

/// What a taken-back history row's statements recorded of PEOPLE (W6a2 fix F18, DUR-4): the core never read the row, so the
/// records that say it was told are false and the redo (or a later zone) states it all again.
/// - every `IdentityMerged` of the row: its `merges_stated` row goes when THIS row recorded it (a people row records its pairs,
///   F12; a pair an earlier row, the live path or the sweep told stays told, G2); in a chats or messages row, where the pair
///   came out of the kept table ([`crate::strangers::release`]), it is kept again;
/// - every address-book name of a `ContactUpsert` (chats and messages rows): the `names_stated` system and number rows go;
/// - every push name the row stated that is still the one recorded (a newer live name stays): the record goes and the name is
///   kept again for the next release;
/// - a DROPPED chats row (a single the core would not take): the people it named are no longer known to the core
///   (`persons_known`), so no later address-book change is stated for someone the core never got. A person the core also
///   knows from an earlier row is known again by the next zone that names him; the address-book name waits in the mirror
///   until then (the safe side).
fn forget_people_of_row(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
    kind: i64,
    dropped: bool,
    seq: i64,
) -> Result<(), StoreError> {
    let stated_here = kind != KIND_PEOPLE;
    for event in events {
        match event {
            BackendEvent::IdentityMerged { from, into } => {
                crate::store::merges::forget_told_by(tx, from.as_str(), into.as_str(), seq)?;
                if stated_here {
                    crate::store::strangers::keep_pair(tx, from.as_str(), into.as_str())?;
                }
            }
            BackendEvent::ContactUpsert(patch) if stated_here => {
                let user = patch.user.as_str();
                if let Patch::Set(name) = &patch.profile_name
                    && people::name_of(tx, user, NameKind::Profile)?.as_deref() == Some(name)
                {
                    people::drop_name(tx, user, NameKind::Profile)?;
                    crate::store::strangers::keep_name(tx, user, name)?;
                }
                if matches!(patch.system_name, Patch::Set(_)) {
                    people::drop_name(tx, user, NameKind::System)?;
                    people::drop_name(tx, user, NameKind::Number)?;
                }
            }
            _ => {}
        }
    }
    if dropped && kind == KIND_CHATS {
        for user in crate::book::persons(events) {
            crate::store::book::forget_known(tx, user.as_str())?;
        }
    }
    Ok(())
}

fn reset_idle(tx: &Transaction<'_>) -> Result<(), StoreError> {
    exec(
        tx,
        "reset the history idle time",
        "UPDATE history_cursor SET idle_ms = 0 WHERE id = 1 AND idle_ms <> 0",
        [],
    )
    .map(drop)
}

/// A start of the delivery: the row at the front of `core_outbox` that an earlier run left unanswered is
/// counted ([`strike`] takes it back when it was found so too often).
pub(crate) async fn note_start(store: &KourierStore) -> Result<(), StoreError> {
    store
        .sweep("count a history delivery start", |tx| {
            exec(
                tx,
                "count a history delivery start",
                "UPDATE history_delivery SET starts = starts + 1 \
                 WHERE seq IS NOT NULL AND seq IN (SELECT seq FROM core_outbox) \
                   AND NOT EXISTS (SELECT 1 FROM core_outbox o WHERE o.seq < history_delivery.seq)",
                [],
            )
            .map(drop)
        })
        .await
}

/// What a taking back came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Struck {
    /// 1 when the oldest waiting row was taken back (dropped if it is a single, split if it has several
    /// units), else 0. Rows of several units behind it that were split too are in `split`.
    pub(crate) taken: usize,
    pub(crate) split: usize,
    /// A row to append again was made.
    pub(crate) pending: bool,
}

/// Takes back the OLDEST waiting row if it is the front of `core_outbox` (and, with `over_starts`, if it was
/// found unanswered at more starts than that), and splits the rows of several units behind it. The running
/// time without an answer starts over.
pub(crate) async fn strike(
    store: &KourierStore,
    over_starts: Option<u32>,
) -> Result<Struck, StoreError> {
    store
        .sweep("take back the oldest history row", move |tx| {
            let over = |entry: &Entry| over_starts.is_none_or(|max| entry.starts > max);
            let mut waiting = entries(tx)?.into_iter().filter(|entry| entry.waiting);
            let Some(oldest) = waiting.next().filter(|entry| entry.front && over(entry)) else {
                return Ok(Struck::default());
            };
            take_back(tx, &oldest)?;
            let mut struck = Struck {
                taken: 1,
                split: 0,
                pending: !oldest.single,
            };
            for behind in waiting.filter(|entry| !entry.single && over(entry)) {
                take_back(tx, &behind)?;
                struck.split += 1;
                struck.pending = true;
            }
            reset_idle(tx)?;
            Ok(struck)
        })
        .await
}

/// The running time without an answer of the core, stored in `history_cursor.idle_ms`.
pub(crate) async fn load_idle(store: &KourierStore) -> Result<Duration, StoreError> {
    store
        .read(|conn| {
            conn.query_row("SELECT idle_ms FROM history_cursor WHERE id = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|ms| Duration::from_millis(u64::try_from(ms).unwrap_or(0)))
            .map_err(sql("read the history idle time"))
        })
        .await
}

async fn store_idle(store: &KourierStore, idle: Duration) -> Result<(), StoreError> {
    let ms = i64::try_from(idle.as_millis()).unwrap_or(i64::MAX);
    store
        .sweep("store the history idle time", move |tx| {
            exec(
                tx,
                "store the history idle time",
                "UPDATE history_cursor SET idle_ms = ?1 WHERE id = 1",
                [ms],
            )
            .map(drop)
        })
        .await
}

/// The running time without an answer: what earlier runs stored, plus this run's looks at the clock (each
/// adds at most [`MAX_TICK`]).
#[derive(Debug)]
pub(crate) struct Idle {
    ran: Duration,
    last_ms: u64,
    /// What the store holds, and when it was last written.
    stored: Duration,
    stored_ms: u64,
}

impl Idle {
    pub(crate) fn new(ran: Duration, now_ms: u64) -> Self {
        Self {
            ran,
            last_ms: now_ms,
            stored: ran,
            stored_ms: now_ms,
        }
    }

    pub(crate) fn so_far(&self) -> Duration {
        self.ran
    }

    /// The core answered, or nothing waits: the time starts over.
    pub(crate) fn reset(&mut self, now_ms: u64) {
        self.ran = Duration::ZERO;
        self.last_ms = now_ms;
    }

    /// A row waits and the core has not answered: the running time since the last look is added.
    pub(crate) fn tick(&mut self, now_ms: u64) {
        let step = Duration::from_millis(now_ms.saturating_sub(self.last_ms)).min(MAX_TICK);
        self.ran = self.ran.saturating_add(step);
        self.last_ms = now_ms;
    }

    /// Writes the running time when it is due (and a reset that the store does not know of yet is written by
    /// the zones that reset it).
    pub(crate) async fn store(
        &mut self,
        store: &KourierStore,
        now_ms: u64,
        every: Duration,
    ) -> Result<(), StoreError> {
        let due = Duration::from_millis(now_ms.saturating_sub(self.stored_ms)) >= every;
        if self.ran == Duration::ZERO {
            // Zones that reset the time wrote it; a reset by a look with nothing waiting writes it here.
            if self.stored != Duration::ZERO {
                store_idle(store, Duration::ZERO).await?;
                self.stored = Duration::ZERO;
                self.stored_ms = now_ms;
            }
        } else if due && self.ran != self.stored {
            store_idle(store, self.ran).await?;
            self.stored = self.ran;
            self.stored_ms = now_ms;
        }
        Ok(())
    }

    /// A zone reset the stored time.
    pub(crate) fn zeroed(&mut self, now_ms: u64) {
        self.reset(now_ms);
        self.stored = Duration::ZERO;
        self.stored_ms = now_ms;
    }
}

/// The error of a delivery that cannot find its place.
pub(crate) fn out_of_place() -> StoreError {
    error::invalid("the history delivery cannot find its place")
}
