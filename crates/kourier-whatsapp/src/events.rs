//! The handler's durable writes (W3a; decisions D1, D4, D5; architecture §7.1 rule (a)).
//!
//! Upstream acknowledges a stanza to the server AFTER the event handler returned (`node_io.rs`
//! `maybe_deferred_ack`): a receipt, a notification, a call. So an event whose loss would matter is
//! written BEFORE the handler returns, and the handler waits for the COMMIT:
//! [`EventWriter::write_and_wait`] puts the event's `core_outbox` row into a shared buffer and blocks
//! the calling thread on a std channel until the writer thread has committed the row. A failed write
//! returns [`WriteFailed`]; the caller then ends the connection (`Client::signal_shutdown_sync`, which
//! suppresses this ack and every later one), and the server sends the stanza again (decision D5).
//!
//! Placement (D1): the handler runs on a worker of the account's OWN client runtime (two workers,
//! apart from Signal's shared one), so a commit parks one of that account's two workers and never
//! another account's I/O. The std channel (not `block_in_place`, which panics on a current-thread
//! runtime; not tokio's `blocking_recv`, which panics inside any runtime) works on every runtime. No
//! deadlock: the writer thread never waits for a runtime, so a parked worker cannot hold up the job it
//! waits for.
//!
//! LINK HOLD (fix F5): while a login has no account yet the write waits, bounded, for `AccountReady` to
//! be recorded ([`crate::hold`]) before it queues anything, so no row can lead `AccountReady` in
//! `core_outbox`; a wait that runs out fails the write ([`WriteFailed::Held`]) and the stanza is not
//! acknowledged.
//!
//! GROUP COMMIT: one writer job drains the shared buffer and commits every row in it in ONE zone, then
//! releases every waiter. A drain of receipts handled by two workers at once costs one fsync for both
//! instead of one each, and anything that queued while the zone ran goes into the next job (FIFO per
//! the writer's one lane, so the order of the buffer is the order of the rows). The group is bounded
//! ([`MAX_GROUP`]); what is left is queued as the next job at once.
//!
//! What rule (a) does NOT hold, so that nobody builds on it (W3b and W6 read this):
//! - newsletter and PDO messages: upstream acknowledges them on its own path BEFORE any handler runs
//!   (`dispatch_node_scoped` takes the deferred ack at `should_ack`, the lane worker decrypts later),
//!   so they are AT MOST ONCE. They are written with [`Inbox::commit_detached`], which queues the zone
//!   in order and does not wait, and nothing claims that an ack waits for them (channels' durability
//!   goes into K8b's brief);
//! - events dispatched after the ack: device notifications and `server_sync` spawn their work and dispatch later. A
//!   group's `w:gp2` notice is NOT one of them: it is dispatched before the ack and written here (W6a2 does not register
//!   upstream's `GroupNotificationDurabilityHook`: the inline path is at least once, and the start's group batch repairs
//!   what an unmapped action lost);
//! - events on the read loop: a durable kind dispatched there must never block (W3a has none; a frame
//!   of `<message>`, `<success>` or `<ib>` is handled inline on the loop).
//!
//! Logs carry counts and error kinds only.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::Connection;
use kourier_model::{ConvKey, UserId};
use tokio::sync::Notify;
use whatsapp_rust::GroupMetadataResult;

use crate::appstate::map::Mapped;
use crate::appstate::zone;
use crate::blocklist;
use crate::calls::log as call_log;
use crate::db::{self, Points};
use crate::error;
use crate::fault;
use crate::features;
use crate::groups;
use crate::history::{live, named, reads};
use crate::hold;
use crate::map::OwnIds;
use crate::merges;
use crate::notice::Noticed;
use crate::send::ledger;
use crate::store::{KourierStore, people};

/// Most rows one group commit takes; the rest is the next job.
pub(crate) const MAX_GROUP: usize = 256;

/// The kill points of the group commit (`failpoints::INSIDE_EVENT_WRITE` / `AFTER_EVENT_WRITE`).
const POINTS: Points = Points {
    inside: crate::failpoints::INSIDE_EVENT_WRITE,
    after: crate::failpoints::AFTER_EVENT_WRITE,
};

/// Why a write did not happen. The caller ends the connection on every one of them (D5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteFailed {
    /// The events could not be encoded (a bug of this crate).
    Invalid,
    /// The store is closed (the account is going away).
    Closed,
    /// The zone failed (a full disk, a damaged file): nothing of its group was written.
    Zone,
    /// The job was lost (the writer thread's job panicked or the writer went away).
    Lost,
    /// A link had no account yet when the wait ran out ([`crate::hold`], fix F5): nothing was written.
    Held,
}

impl fmt::Display for WriteFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "the events could not be encoded",
            Self::Closed => "the store is closed",
            Self::Zone => "the write failed",
            Self::Lost => "the write was lost",
            Self::Held => "the link has no account yet",
        })
    }
}

impl std::error::Error for WriteFailed {}

/// One row waiting for the next group commit, and the thread that waits for it.
struct Entry {
    row: Vec<u8>,
    /// The events of the row when the zone has to look at them (W5a): one names a chat whose features may need a
    /// statement (D9: a call line, a group's notice), or it is a receipt of our own messages in a direct chat that
    /// the ledger may move to the chat it holds (fix F9). Decided in the zone that commits the row, which then writes
    /// them with it.
    zoned: Option<Vec<BackendEvent>>,
    /// The titles and push names the row states (W4 fix F3), noted in the zone that commits it.
    named: Vec<named::Note>,
    /// The chats the row says were read on the phone (W4 fix F7), noted in the same zone for the history's hint.
    reads: Vec<String>,
    /// An app-state event of the phone (W6a1, A4): its events are made IN the zone, from the mirror it writes there
    /// ([`zone::apply`]); `row` is empty and nothing is appended when the mirror says nothing changed.
    settings: Option<Mapped>,
    /// The reconcile of a collection after a snapshot of ours (W6a1, A5b): the collection and the generation the snapshot
    /// opened. Made in the zone ([`zone::reconcile`]) so the mirror's reset and the core's events are one commit.
    reconcile: Option<(String, i64)>,
    /// One record of the phone's call log (W6a1, A6) and our ids at the time: its conversation and its events are made
    /// IN the zone ([`call_log::place`]); `row` is empty and nothing is appended when the record says nothing a line
    /// shows.
    call_log: Option<(call_log::Seen, OwnIds)>,
    /// The people the phone's blocklist names (W6a2, B3): the statement is made IN the zone, from `blocked_stated`
    /// ([`blocklist::state`]); `row` is empty and nothing is appended when the list changed nothing.
    blocklist: Option<Vec<UserId>>,
    /// A group's notice (W6a2, B5): the events that need no row and the acts that do; the zone makes the acts into events
    /// against the group's row ([`groups::zone::apply`]) and then treats the result like any events that name a chat.
    noticed: Option<Noticed>,
    /// The answers of a group fetch (W6a2, B6), our ids at the time and when the ask was made (ms): the groups' rows and
    /// their events are made IN the zone ([`groups::zone::fetched_by_group`], one `core_outbox` row per group); `row` is empty and nothing is appended when the answers say nothing to state.
    fetched: Option<(Vec<GroupMetadataResult>, OwnIds, i64)>,
    /// Groups the server's `groups_dirty` named (W6a2, B6): their "fetch me" marks are written IN the zone.
    dirty: Option<Vec<ConvKey>>,
    /// The pairs of a number and a LID the merge sweep found due (W6a2, B9): stated IN the zone, against `merges_stated`
    /// ([`merges::state`]); `row` is empty and nothing is appended when the zone finds every pair stated already.
    merges: Option<Vec<merges::Pair>>,
    waiter: SyncSender<Result<(), WriteFailed>>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    /// A drain job is queued (or running its take): a new entry needs no job of its own.
    scheduled: bool,
}

struct Shared {
    store: KourierStore,
    clock: Arc<dyn Clock>,
    /// Wakes the `core_outbox` forwarder after a commit.
    wake: Arc<Notify>,
    /// Woken after a commit that marked a group for a fetch (W6a2, B6).
    marks: Arc<Notify>,
    /// Woken after a commit that can make a merge due: an address-book change of the phone or a group's member list (W6a2, B9).
    merges: Arc<Notify>,
    /// Stage 2's wake, set once by the actor: woken after a commit that let a group's waiting admin deletes go (W6a2, B7a).
    stage2_wake: OnceLock<Arc<Notify>>,
    max_group: usize,
    /// How long a write waits for the link hold to open, in ms (tests shorten it).
    patience_ms: AtomicU64,
    state: Mutex<State>,
    /// Committed groups (for the tests and the measurement).
    commits: AtomicU64,
}

/// The handler's way to a durable `core_outbox` row. Cheap to clone.
#[derive(Clone)]
pub(crate) struct EventWriter {
    shared: Arc<Shared>,
}

impl EventWriter {
    pub(crate) fn new(store: KourierStore, clock: Arc<dyn Clock>, wake: Arc<Notify>) -> Self {
        Self::with_limit(store, clock, wake, MAX_GROUP)
    }

    /// The store the rows go to (a handler that must also read it: the call interceptor).
    pub(crate) fn store(&self) -> &KourierStore {
        &self.shared.store
    }

    /// [`new`](Self::new) with another group bound (tests).
    pub(crate) fn with_limit(
        store: KourierStore,
        clock: Arc<dyn Clock>,
        wake: Arc<Notify>,
        max_group: usize,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                clock,
                wake,
                marks: Arc::new(Notify::new()),
                merges: Arc::new(Notify::new()),
                stage2_wake: OnceLock::new(),
                max_group: max_group.max(1),
                patience_ms: AtomicU64::new(
                    u64::try_from(hold::PATIENCE.as_millis()).unwrap_or(u64::MAX),
                ),
                state: Mutex::new(State::default()),
                commits: AtomicU64::new(0),
            }),
        }
    }

    /// Writes `events` as ONE `core_outbox` row and returns after COMMIT. BLOCKS the calling thread:
    /// call it from the handler on the account's client runtime, never from a task that serves the
    /// store's other callers. `Err` means nothing was written and the stanza must not be acknowledged.
    pub(crate) fn write_and_wait(&self, events: &[BackendEvent]) -> Result<(), WriteFailed> {
        self.write_and_wait_reading(events, &[])
    }

    /// [`write_and_wait`](Self::write_and_wait) for a row that also says the chats `read_on_the_phone` were read on
    /// another device of ours: in the zone that writes the row, they are noted as read for the history's unread
    /// hint (W4 fix F7), so a chunk that names the chat afterwards states no hint for it.
    pub(crate) fn write_and_wait_reading(
        &self,
        events: &[BackendEvent],
        read_on_the_phone: &[ConvKey],
    ) -> Result<(), WriteFailed> {
        let row = encode_outbox_row(events).map_err(|_| WriteFailed::Invalid)?;
        self.submit(|waiter| Entry {
            row,
            zoned: (features::names_a_chat(events) || ledger::names_an_own_receipt(events))
                .then(|| events.to_vec()),
            named: named::notes_of(events),
            reads: read_on_the_phone.iter().map(ConvKey::encode).collect(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes what the phone's app state said (W6a1, A4): the mirror and the core events it causes, in ONE zone of the
    /// group commit, and returns after COMMIT like [`write_and_wait`](Self::write_and_wait) (the same hold, the same
    /// failure: the caller ends the connection). Nothing is appended to `core_outbox` when the phone said nothing new.
    pub(crate) fn write_settings_and_wait(&self, mapped: Mapped) -> Result<(), WriteFailed> {
        let (collection, counts) = (mapped.collection, mapped.from_full_sync);
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: Some(mapped),
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })?;
        // Durable now: a record of a snapshot's replay counts for the reconcile of a snapshot of ours that is out for this
        // collection (A5b). A patch does not (DUR-1): an incremental sync of the collection can run inside the window,
        // before upstream reserves the collection for the snapshot, and it is not the snapshot's replay.
        if counts {
            self.shared.store.activity().replayed(collection);
        }
        Ok(())
    }

    /// The reconcile of `collection` after the snapshot of `generation` (W6a1, A5b): the rows it left on an older
    /// generation go back to their defaults and the core is told, in ONE zone of the group commit; returns after COMMIT,
    /// held by the link hold like every handler write. BLOCKS the calling thread (the start-up task calls it through
    /// `spawn_blocking`).
    pub(crate) fn write_reconcile_and_wait(
        &self,
        collection: &str,
        generation: i64,
    ) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: Some((collection.to_owned(), generation)),
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes one record of the phone's call log (W6a1, A6): its line and the chat's statement, if it needs one, in ONE
    /// zone of the group commit, and returns after COMMIT like [`write_and_wait`](Self::write_and_wait) (the same
    /// hold, the same failure: the caller ends the connection). Nothing is appended to `core_outbox` when the record
    /// says nothing a line shows or names no chat.
    pub(crate) fn write_call_log_and_wait(
        &self,
        seen: call_log::Seen,
        own: OwnIds,
    ) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: Some((seen, own)),
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes what the phone's blocklist says (W6a2, B3): `listed` is the people it names now (`blocklist::people`); the
    /// zone compares them with what the core was told and states the difference, and returns after COMMIT like
    /// [`write_and_wait`](Self::write_and_wait) (the same hold, the same failure). Nothing is appended when the list
    /// changed nothing. BLOCKS the calling thread (the start-up task calls it through `spawn_blocking`).
    pub(crate) fn write_blocklist_and_wait(&self, listed: &[UserId]) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: Some(listed.to_vec()),
            noticed: None,
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes a group's notice (W6a2, B5): its events and what the group's row makes of its acts, in ONE zone of the group
    /// commit, and returns after COMMIT like [`write_and_wait`](Self::write_and_wait) (the same hold, the same failure: the
    /// caller ends the connection and the server sends the notification again). The titles it states are noted for the
    /// history like any other (W4 fix F3).
    pub(crate) fn write_notice_and_wait(&self, noticed: Noticed) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: Some(noticed),
            fetched: None,
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes the answers of a group fetch (W6a2, B6; `asked_at_ms` is when the ask was made: a mark newer than that is not
    /// covered by the answer and stays): the groups' rows and the events they come to, in ONE zone of the group commit, and returns after COMMIT like [`write_and_wait`](Self::write_and_wait) (the same hold, the same failure).
    /// Nothing is appended when the answers state nothing. BLOCKS the calling thread (the fetch task calls it through
    /// `spawn_blocking`).
    pub(crate) fn write_groups_fetched_and_wait(
        &self,
        answers: Vec<GroupMetadataResult>,
        own: OwnIds,
        asked_at_ms: i64,
    ) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: Some((answers, own, asked_at_ms)),
            dirty: None,
            merges: None,
            waiter,
        })
    }

    /// Writes the "fetch this group" marks of a `groups_dirty` (W6a2, B6): one zone, the same wait and the same failure as
    /// a notice (the server sends the stanza again). Wakes the fetch policy after the commit.
    pub(crate) fn write_groups_dirty_and_wait(
        &self,
        groups: Vec<ConvKey>,
    ) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: Some(groups),
            merges: None,
            waiter,
        })
    }

    /// Writes the pairs of a number and a LID the merge sweep found due (W6a2, B9): the zone states each one the core was not
    /// told, with its `merges_stated` row, and returns after COMMIT like [`write_and_wait`](Self::write_and_wait) (the same
    /// hold, the same failure). Nothing is appended when every pair was stated already. BLOCKS the calling thread (the sweep
    /// calls it through `spawn_blocking`).
    pub(crate) fn write_merges_and_wait(&self, pairs: &[merges::Pair]) -> Result<(), WriteFailed> {
        self.submit(|waiter| Entry {
            row: Vec::new(),
            zoned: None,
            named: Vec::new(),
            reads: Vec::new(),
            settings: None,
            reconcile: None,
            call_log: None,
            blocklist: None,
            noticed: None,
            fetched: None,
            dirty: None,
            merges: Some(pairs.to_vec()),
            waiter,
        })
    }

    /// Woken after a commit that can make a merge due (an address-book change, a group's member list) and by the history
    /// delivery's quiet step: the merge sweep waits on it (W6a2, B9). A permit is kept when nobody waits.
    pub(crate) fn merges(&self) -> Arc<Notify> {
        self.shared.merges.clone()
    }

    /// Stage 2's wake: the writer wakes it after a commit whose zone let a group's waiting admin deletes go (W6a2, B7a).
    /// Set once by the actor; a second call changes nothing.
    pub(crate) fn wakes_stage2_with(&self, wake: Arc<Notify>) {
        let _ = self.shared.stage2_wake.set(wake);
    }

    /// Woken after a commit that marked a group for a fetch (a notice the event cannot carry, a `groups_dirty`): the fetch
    /// policy waits on it (W6a2, B6). A permit is kept when nobody waits.
    pub(crate) fn marks(&self) -> Arc<Notify> {
        self.shared.marks.clone()
    }

    /// The part every write shares: waits for the link hold, queues the entry and parks until its group committed.
    fn submit(
        &self,
        entry: impl FnOnce(SyncSender<Result<(), WriteFailed>>) -> Entry,
    ) -> Result<(), WriteFailed> {
        // While a link has no account, a row would land below `AccountReady` (fix F5): wait for the
        // hold, bounded. Nothing is queued before it opens.
        let patience = Duration::from_millis(self.shared.patience_ms.load(Ordering::Relaxed));
        if !self.shared.store.hold().wait_open(patience) {
            return Err(WriteFailed::Held);
        }
        let (waiter, answer) = sync_channel(1);
        let schedule = {
            let mut state = self.shared.lock();
            state.entries.push(entry(waiter));
            !std::mem::replace(&mut state.scheduled, true)
        };
        if schedule {
            self.shared.schedule();
        }
        answer.recv().unwrap_or(Err(WriteFailed::Lost))
    }

    /// How long a write waits for the link hold (tests).
    #[cfg(test)]
    pub(crate) fn set_patience(&self, patience: Duration) {
        let ms = u64::try_from(patience.as_millis()).unwrap_or(u64::MAX);
        self.shared.patience_ms.store(ms, Ordering::Relaxed);
    }

    /// Rows waiting for a commit.
    #[cfg(test)]
    pub(crate) fn buffered(&self) -> usize {
        self.shared.lock().entries.len()
    }

    /// Groups committed so far.
    #[cfg(test)]
    pub(crate) fn commits(&self) -> u64 {
        self.shared.commits.load(Ordering::SeqCst)
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues one drain job on the writer. A writer that stopped fails every waiter at once.
    fn schedule(self: &Arc<Self>) {
        let shared = Arc::clone(self);
        if self.store.queue(move |conn| shared.drain(conn)).is_err() {
            self.fail_all(WriteFailed::Closed);
        }
    }

    /// Fails everything that waits and lets the next entry queue a job again.
    fn fail_all(&self, why: WriteFailed) {
        let entries = {
            let mut state = self.lock();
            state.scheduled = false;
            std::mem::take(&mut state.entries)
        };
        for entry in entries {
            let _ = entry.waiter.send(Err(why));
        }
    }

    /// The drain job, on the writer thread: takes up to [`MAX_GROUP`] rows, commits them in one zone,
    /// releases their waiters.
    fn drain(self: &Arc<Self>, conn: &mut Connection) {
        let (group, more) = {
            let mut state = self.lock();
            let take = state.entries.len().min(self.max_group);
            let group: Vec<Entry> = state.entries.drain(..take).collect();
            state.scheduled = !state.entries.is_empty();
            (group, state.scheduled)
        };
        if more {
            self.schedule();
        }
        if group.is_empty() {
            return;
        }
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        // What the reconciles of this group did, in rows (logged once the commit is durable).
        let mut reconciled = zone::Reconciled::default();
        // What the call-log records of this group came to (logged as numbers once the commit is durable).
        let mut records = call_log::Counts::default();
        // What the blocklist statements of this group came to (logged as numbers once the commit is durable).
        let mut listed = blocklist::Counts::default();
        // What the group fetches of this group came to (logged as numbers once the commit is durable).
        let mut answered = groups::zone::Counts::default();
        // How many merges this group stated (logged as a number once the commit is durable).
        let mut paired = 0usize;
        let outcome = db::zone(conn, "write events", Some(POINTS), |tx| {
            // The names the group states are noted while a push can still state them (W4 fixes F3, F4).
            let open = group.iter().any(|entry| {
                !entry.named.is_empty() || entry.noticed.is_some() || entry.fetched.is_some()
            }) && live::wanted(tx)?;
            for entry in &group {
                // What the phone's app state said: the mirror is written here and the events come out of it, so
                // there may be none (W6a1).
                if let Some(mapped) = &entry.settings {
                    let events = zone::apply(tx, mapped)?;
                    if !events.is_empty() {
                        let row = encode_outbox_row(&events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    continue;
                }
                // A record of the phone's call log: its line, quiet, in the chat of its call (W6a1, A6).
                if let Some((seen, own)) = &entry.call_log {
                    let placed = call_log::place(tx, seen, own, now)?;
                    records.add(placed.verdict);
                    if !placed.events.is_empty() {
                        let row = encode_outbox_row(&placed.events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    continue;
                }
                // What the phone's blocklist says: the difference from what the core was told, one row (W6a2, B3).
                if let Some(names) = &entry.blocklist {
                    let (events, counts) = blocklist::state(tx, names, now)?;
                    listed.blocked += counts.blocked;
                    listed.unblocked += counts.unblocked;
                    if !events.is_empty() {
                        let row = encode_outbox_row(&events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    continue;
                }
                // The pairs of a number and a LID the sweep found due: each is stated once, in the zone that records it (W6a2, B9).
                if let Some(pairs) = &entry.merges {
                    let (events, stated) = merges::state(tx, pairs)?;
                    paired += stated;
                    if !events.is_empty() {
                        let row = encode_outbox_row(&events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    continue;
                }
                // A group's notice: the acts are made into events against the group's row, then the chat is stated like
                // for any events that name it, and the titles it sets are noted for the history (W6a2, B5).
                if let Some(noticed) = &entry.noticed {
                    let made = groups::zone::apply(tx, noticed, now)?;
                    let stated = zone::prepare(tx, &made)?;
                    let events = stated.as_deref().unwrap_or(&made);
                    if !events.is_empty() {
                        let row = encode_outbox_row(events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    if open {
                        named::record(tx, &named::notes_of(events), now)?;
                    }
                    continue;
                }
                // The answers of a group fetch: each group's row and its events, the chat stated like for any events that
                // name it, and the titles it sets noted for the history (W6a2, B6).
                if let Some((answers, own, asked_at)) = &entry.fetched {
                    let (rows, counts) =
                        groups::zone::fetched_by_group(tx, answers, own, *asked_at, now)?;
                    answered.found += counts.found;
                    answered.truncated += counts.truncated;
                    answered.not_member += counts.not_member;
                    answered.skipped += counts.skipped;
                    answered.released += counts.released;
                    answered.ended += counts.ended;
                    answered.parents_waiting += counts.parents_waiting;
                    answered.parents_released += counts.parents_released;
                    // One row per group (F22, DUR-13): all in this zone, but a group the core refuses is one row.
                    for made in &rows {
                        let stated = zone::prepare(tx, made)?;
                        let events = stated.as_deref().unwrap_or(made);
                        if !events.is_empty() {
                            let row = encode_outbox_row(events)
                                .map_err(|_| error::invalid("the events could not be encoded"))?;
                            outbox::append(tx, &row, now)
                                .map_err(|_| error::database("append events failed"))?;
                        }
                        if open {
                            named::record(tx, &named::notes_of(events), now)?;
                        }
                    }
                    continue;
                }
                // The server's `groups_dirty`: each group it names is marked for a fetch (W6a2, B6).
                if let Some(named_groups) = &entry.dirty {
                    for conv in named_groups {
                        people::mark_dirty(tx, conv, now)?;
                    }
                    continue;
                }
                // The rows a snapshot of ours left unmentioned go back to their defaults, with the core's words.
                if let Some((collection, generation)) = &entry.reconcile {
                    let done = zone::reconcile(tx, collection, *generation)?;
                    if !done.events.is_empty() {
                        let row = encode_outbox_row(&done.events)
                            .map_err(|_| error::invalid("the events could not be encoded"))?;
                        outbox::append(tx, &row, now)
                            .map_err(|_| error::database("append events failed"))?;
                    }
                    reconciled.reset += done.reset;
                    reconciled.kept += done.kept;
                    continue;
                }
                // A receipt of our own messages goes to the chat the ledger holds for them (fix F9), and a chat the
                // row names is stated in the same row, before its first event (D9), with what the phone said about
                // it and, for a message that brings an archived chat back, the unarchive (W6a1, H6).
                let stated = match &entry.zoned {
                    Some(events) => {
                        // A call line goes to the chat the phone's log already filed its call under (DUR-9), then it is
                        // noted for the call log (A6): its outcome lands in this chat.
                        let lined = call_log::rehome_lines(tx, events)?;
                        let homed = lined.as_deref().unwrap_or(events);
                        call_log::note_lines(tx, homed, now)?;
                        let home = ledger::rehome_receipts(tx, homed)?;
                        let moved = home.or(lined);
                        let events = moved.as_deref().unwrap_or(events);
                        zone::prepare(tx, events)?
                            .or(moved)
                            .map(|zoned| encode_outbox_row(&zoned))
                            .transpose()
                            .map_err(|_| error::invalid("the events could not be encoded"))?
                    }
                    None => None,
                };
                outbox::append(tx, stated.as_deref().unwrap_or(&entry.row), now)
                    .map_err(|_| error::database("append events failed"))?;
                if open {
                    named::record(tx, &entry.named, now)?;
                }
                for chat in &entry.reads {
                    reads::note_in(tx, chat)?;
                }
            }
            // The seam of D3: everything written, nothing committed.
            fault::hit(fault::EVENT_WRITE)
        });
        let answer = match outcome {
            Ok(()) => {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.wake.notify_one();
                // A mark asked for a fetch: the policy looks at the marks (a permit stays when it is busy).
                if group.iter().any(|entry| {
                    entry.dirty.is_some()
                        || entry.noticed.as_ref().is_some_and(|n| !n.acts.is_empty())
                }) {
                    self.marks.notify_one();
                }
                // An address-book change or a group's member list can make a merge due: the sweep looks again (W6a2, B9).
                if answered.found > 0
                    || group
                        .iter()
                        .any(|entry| entry.settings.as_ref().is_some_and(Mapped::is_address_book))
                {
                    self.merges.notify_one();
                }
                // Rows a group's member list made due: stage 2 maps them now (W6a2, D11).
                if answered.released > 0
                    && let Some(wake) = self.stage2_wake.get()
                {
                    wake.notify_one();
                }
                if paired > 0 {
                    tracing::debug!(merged = paired, "merges were stated");
                }
                if answered.any() {
                    tracing::debug!(
                        found = answered.found,
                        truncated = answered.truncated,
                        not_member = answered.not_member,
                        skipped = answered.skipped,
                        released = answered.released,
                        ended = answered.ended,
                        parents_waiting = answered.parents_waiting,
                        parents_released = answered.parents_released,
                        "the answers of a group fetch were stated"
                    );
                }
                if reconciled.reset + reconciled.kept > 0 {
                    tracing::info!(
                        reset = reconciled.reset,
                        kept = reconciled.kept,
                        "a snapshot's reconcile put chat settings back to their defaults"
                    );
                }
                if listed.blocked + listed.unblocked > 0 {
                    tracing::debug!(
                        blocked = listed.blocked,
                        unblocked = listed.unblocked,
                        "the blocklist's difference was stated"
                    );
                }
                if records.any() {
                    tracing::debug!(
                        lines = records.lines,
                        silent = records.silent,
                        unplaced = records.unplaced,
                        refused = records.refused,
                        "call-log records were put into chats"
                    );
                }
                // A count only (D10). Every waiter is a parked worker of the client runtime, so the
                // size of a group is the number of workers one commit freed; `tests/measure.rs` reads it.
                tracing::debug!(rows = group.len(), "the handler's group commit is durable");
                Ok(())
            }
            Err(e) => {
                // The error names the operation, never an input.
                tracing::error!(error = %e, rows = group.len(), "the handler's events could not be written");
                Err(WriteFailed::Zone)
            }
        };
        for entry in group {
            let _ = entry.waiter.send(answer);
        }
    }
}

#[cfg(test)]
mod tests;
