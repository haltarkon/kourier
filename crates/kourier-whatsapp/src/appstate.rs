//! Rule (b) of architecture §7.1: when the "dirty" flag of an app-state collection may be cleared, and the start-up
//! resync that repairs what a kill left (W3a, chunk C12, decision D6; W6a1, decisions D2, D4, B1, M7).
//!
//! A collection is dirty from the moment its version moves (`store::appsync`) until the events of
//! that version are known to be durable. Upstream has no public "every event of collection C at
//! version V was dispatched", so the flag cannot be cleared in the zone that writes the events.
//! It is cleared at exactly two places, and never in the middle of a session:
//!
//! 1. After a start-up resync ([`startup::run`]): a collection that a kill or an unquiet quit left dirty is rebuilt
//!    from the server's snapshot, and the one the report calls synced is cleared. The resync returns after every
//!    mutation was dispatched to the event bus, and the one handler waits for each write (rule (a)), so the events
//!    are durable when it returns, unless ANOTHER sync of the same collection moved the version meanwhile:
//!    upstream's reservation keeps two syncs of one collection apart while the resync runs, but not after it
//!    returned. So the clear waits until the connection has been quiet ([`Quiet`]: no app-state version write and
//!    no app-state event for the window) and the job that clears checks, in the writer's own order, that no
//!    version moved since the quiet look ([`KourierStore::clear_quiet`]). A write that slipped in keeps its flag.
//!    The version the flag is compared with is read in that same job, so a concurrent sync cannot move it between
//!    the look and the clear.
//! 2. At a quiet, clean stop ([`stop_quietly`]): the client is paused first (`Client::pause`: it
//!    closes the socket, so no new sync can start and none waits for the network), then the same quiet
//!    look, then the stop and the release of the store, then the counters are compared once more
//!    (a task of the client that outlived the pause would show in them), and only then every flag at or
//!    below its collection's stored version goes. Not quiet = the flags stay = the next start
//!    rebuilds them (the safe side).
//!
//! WHICH COLLECTIONS (fix F2, D4): both points clear only the collections of [`EVENTS_WRITTEN`], the ones whose
//! mutation events the handler really writes into `core_outbox`: since W6a1 all five of upstream's. A collection
//! that holds only verbs Kourier does not map (star, labels, lock, other settings) is still on the list: its
//! mutations are handled by saying nothing, so its events are complete the moment its zone ran. When a later build
//! maps a verb the stored map version ([`startup::MAP_VERSION`]) did not, the collections that grew are marked
//! dirty once and read again.
//!
//! WHAT IS TRAFFIC (D4): only app-state mutation events and version writes move the counters ([`Activity`]);
//! presence, typing, receipts and messages do not, so a busy group does not keep the quiet look from ever coming.
//!
//! THE REASON FOR A RESYNC (critic B1): the dirty set READ WHEN THE STORE OPENS, before the client connects. That
//! set is exactly what a kill or an unquiet quit of an earlier run left. A flag set in THIS session is no reason:
//! the quiet quit clears it, or it stays for the next start. The link session gets no resync of Kourier's: its
//! books are stamped at the link ([`startup::link_in`], in the zone of the link's `AccountReady`). A collection owed a resync keeps its flag at every clearing
//! point until its resync came back synced ([`Activity::owe`]); one whose zone failed in this process
//! ([`Activity::lose`]) keeps it for the rest of the process. The start-up task runs ONCE per start after `Online`,
//! on the client's runtime, one collection after another, each at most once per start and once per 6 hours
//! ([`startup::SPACING_MS`]); a fatal answer is recorded and the collection is asked again only after a new map
//! version or a new link (M7).
//!
//! A CRASH (kill, power cut) therefore leaves every collection that moved since the start dirty, and
//! the next start asks for those once (at most every 6 hours). That costs one snapshot request per moved
//! collection, nothing else.
//!
//! The counters are plain numbers, kept in memory for the life of the store handle ([`Activity`]);
//! nothing here logs a name, a version or an event, only counts and an outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use kourier_model::BackendError;

use crate::session::{Released, Session};
use crate::store::KourierStore;

/// The collections whose mutation events the handler writes into `core_outbox`, the only ones a
/// clearing point may clear (fix F2): all five of upstream's since W6a1 (D4). The clearing points take the list as an
/// argument: the tests of the clearing itself pass their own.
pub(crate) const EVENTS_WRITTEN: &[&str] = &[
    "critical_block",
    "critical_unblock_low",
    "regular_high",
    "regular_low",
    "regular",
];

/// What moved in the app-state path of one account since the store was opened: version writes (by
/// the store, inside the writer's job) and events (by the one handler). Shared by every clone of the
/// store handle, so a new client of the same account continues the same count.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    writes: AtomicU64,
    events: AtomicU64,
    /// The collections whose events were lost in this process: the handler's zone for one of their mutations failed
    /// AFTER upstream's cursor committed (`UP/wacore/src/appstate_sync.rs:1313-1331`), so the event is gone and only a
    /// snapshot brings it back (critic B1 point 3). A clearing point never clears them; the flag stays and the next
    /// start's resync repairs it. In memory on purpose: the flag in `appstate_dirty` is the persisted mark, and a
    /// process that ends takes this set with it, which leaves the flag standing.
    lost: Mutex<BTreeSet<String>>,
    /// The collections this run owes a resync: they were dirty when the store opened, so events may be missing from
    /// the mirror, and their flag must stand at every clearing point until the resync came back synced
    /// ([`startup::run`]). In memory on purpose, like `lost`: the flag itself is the persisted mark.
    owed: Mutex<BTreeSet<String>>,
    /// The snapshots Kourier has asked for that have not come back: per collection, how many of its mapped mutations the
    /// handler has written since the ask (the reconcile's proof that the replay was not empty, critic H1). A collection
    /// without an entry has no snapshot of ours running, so what the handler writes for it (a live sync, the link's
    /// full sync, a recovery upstream started) counts for nothing. In memory: the replay and the reconcile are one
    /// process ([`startup::run`]).
    replays: Mutex<BTreeMap<String, u64>>,
}

/// Both counters at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    /// Version writes (a collection's version or dirty row written or removed).
    pub(crate) writes: u64,
    /// Events the handler saw.
    pub(crate) events: u64,
}

impl Activity {
    /// A version or dirty row is being written (called inside the writer's job, before the commit:
    /// a clear job queued later in the one lane sees it; a rolled-back write only makes the next
    /// look "not quiet", which is the safe side).
    pub(crate) fn wrote(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }

    /// The handler saw an event.
    pub(crate) fn event(&self) {
        self.events.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn sample(&self) -> Sample {
        Sample {
            writes: self.writes.load(Ordering::SeqCst),
            events: self.events.load(Ordering::SeqCst),
        }
    }

    /// The version-write counter alone (what the clearing job compares in the writer's order).
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }

    fn lost_set(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.lost.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn owed_set(&self) -> MutexGuard<'_, BTreeSet<String>> {
        self.owed.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn replay_map(&self) -> MutexGuard<'_, BTreeMap<String, u64>> {
        self.replays.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `collections` were dirty when the store opened: their flags stand until [`repaired`](Self::repaired).
    pub(crate) fn owe(&self, collections: &[String]) {
        self.owed_set().extend(collections.iter().cloned());
    }

    /// The resync of `collection` came back synced and its events are durable: its flag may go at a clearing point.
    pub(crate) fn repaired(&self, collection: &str) {
        self.owed_set().remove(collection);
    }

    /// The collections still owed a resync.
    pub(crate) fn owed(&self) -> Vec<String> {
        self.owed_set().iter().cloned().collect()
    }

    /// An event of `collection` could not be written: its flag must stand until a resync rebuilds the collection.
    pub(crate) fn lose(&self, collection: &str) {
        self.lost_set().insert(collection.to_owned());
    }

    /// The collections whose events were lost in this process.
    pub(crate) fn lost(&self) -> Vec<String> {
        self.lost_set().iter().cloned().collect()
    }

    /// A snapshot of `collection` is about to be asked: from now its mapped mutations are counted
    /// ([`replayed`](Self::replayed)) until [`replay_close`](Self::replay_close).
    pub(crate) fn replay_open(&self, collection: &str) {
        self.replay_map().insert(collection.to_owned(), 0);
    }

    /// The handler wrote a mapped mutation of `collection` that came from a snapshot's replay (its zone is durable;
    /// the caller counts only `from_full_sync` records: DUR-1). Counts only while a snapshot of ours is out for that
    /// collection.
    pub(crate) fn replayed(&self, collection: &str) {
        if let Some(count) = self.replay_map().get_mut(collection) {
            *count += 1;
        }
    }

    /// The snapshot of `collection` came back (or never left): how many mutations of it were written meanwhile.
    pub(crate) fn replay_close(&self, collection: &str) -> u64 {
        self.replay_map().remove(collection).unwrap_or(0)
    }
}

/// How long the connection must have been silent, and how long to wait for that.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Quiet {
    /// No version write and no event for this long = quiet. A sync's version write is followed at
    /// once (no network wait, scout U9) by the dispatch of its mutations, each of which the handler
    /// waits to be durable: this is longer than one commit, not longer than a page of them.
    pub(crate) window: Duration,
    /// After this long without a quiet look the clear is given up (the flags stay).
    pub(crate) bound: Duration,
}

impl Quiet {
    /// The look a quit can still afford with `left` of its grace before the store's release (fix G6, DUR-7 residual):
    /// the look sleeps one window before it can judge anything, so with less than a window left there is no look
    /// (`None`: the flags stay, the next start asks again); otherwise it is the standard look held to what is left.
    pub(crate) fn within(left: Duration) -> Option<Self> {
        let window = Self::STANDARD.window;
        (left >= window).then(|| Self {
            bound: Self::STANDARD.bound.min(left - window),
            ..Self::STANDARD
        })
    }

    /// The production values.
    #[cfg(not(test))]
    pub(crate) const STANDARD: Self = Self {
        window: Duration::from_millis(250),
        bound: Duration::from_secs(3),
    };
    /// Tests wait less.
    #[cfg(test)]
    pub(crate) const STANDARD: Self = Self {
        window: Duration::from_millis(20),
        bound: Duration::from_millis(300),
    };
}

/// Why flags were left as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The connection did not go quiet within the bound, or something moved after the quiet look.
    Busy,
    /// The store failed (the next start asks again).
    Failed,
    /// The handler writes the events of none of the dirty collections (fix F2): they stay dirty.
    NotWritten,
    /// Every collection that could have been cleared lost an event in this process ([`Activity::lose`]) or is owed
    /// a resync that has not come back synced yet ([`Activity::owe`]).
    Lost,
    /// The quit had no time left for even one window of looking (fix G6): the client was stopped, nothing cleared.
    NoTime,
}

/// What a clearing point did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// This many flags were cleared.
    Cleared(usize),
    /// The flags stay; the next start rebuilds those collections.
    Kept(Why),
}

/// What a resync request came back with, in Kourier's own names (the stored names of the collections).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ResyncReport {
    /// Fetched, applied and persisted.
    pub(crate) synced: Vec<String>,
    /// The server refused the collection outright (400/404): asking again gets the same answer (M7).
    pub(crate) fatal: Vec<String>,
}

/// The part of the client a start-up resync needs, in Kourier's own types.
#[async_trait]
pub(crate) trait Resync: Send + Sync {
    /// Rebuilds `collections` from the server's snapshot (upstream's `resync_app_state` in snapshot
    /// mode) and says which the report calls synced and which fatal. `Err` only when nothing was asked (no connection,
    /// no client); a request that went out and failed is `Ok` with the collection in neither list (unsynced). It runs as a task on the client's runtime and is awaited by the caller;
    /// the call itself is not given a deadline (dropping it mid-apply would leave a collection
    /// needing another sync, upstream's doc says), only the wait for the connection could be.
    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError>;
}

/// A session as the start-up resync asks it.
pub(crate) struct Asking(pub(crate) Arc<dyn Session>);

#[async_trait]
impl Resync for Asking {
    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError> {
        self.0.resync(collections).await
    }
}

/// Waits until [`Activity`] stands still for `quiet.window`; the sample it stood still at, or `None`
/// when that did not happen within `quiet.bound`.
pub(crate) async fn quiet_for(store: &KourierStore, quiet: Quiet) -> Option<Sample> {
    let started = tokio::time::Instant::now();
    loop {
        let before = store.activity().sample();
        tokio::time::sleep(quiet.window).await;
        let after = store.activity().sample();
        if before == after {
            return Some(after);
        }
        if started.elapsed() >= quiet.bound {
            return None;
        }
    }
}

/// Clears the flags (of `only`, or of every collection) that are on the `written` list and whose
/// collection has a stored version at or above the flag's, in one job that first checks no version
/// moved since `seen`.
async fn clear(
    store: &KourierStore,
    seen: Sample,
    only: Option<Vec<String>>,
    written: &[&str],
) -> Outcome {
    let mut allowed: Vec<String> = written
        .iter()
        .filter(|name| {
            only.as_ref()
                .is_none_or(|only| only.iter().any(|o| o == **name))
        })
        .map(|name| (*name).to_owned())
        .collect();
    if allowed.is_empty() {
        return Outcome::Kept(Why::NotWritten);
    }
    // A collection that lost an event in this process keeps its flag (critic B1 point 3), and so does one whose resync
    // is still owed: its flag is the only record that the mirror may miss events.
    let lost = store.activity().lost();
    let owed = store.activity().owed();
    allowed.retain(|name| !lost.contains(name) && !owed.contains(name));
    if allowed.is_empty() {
        return Outcome::Kept(Why::Lost);
    }
    match store.clear_quiet(seen.writes, Some(allowed)).await {
        Ok(Some(cleared)) => Outcome::Cleared(cleared),
        Ok(None) => Outcome::Kept(Why::Busy),
        Err(_) => Outcome::Kept(Why::Failed),
    }
}

/// Clearing point 2: pause the client, look for quiet, stop it (the store is released), look again,
/// clear. `already_paused`: the account was paused by `Disconnect` already. With nothing on the
/// `written` list nothing can be cleared, so the quiet look is left out (a quit does not wait for it).
pub(crate) async fn stop_quietly(
    store: &KourierStore,
    session: &dyn Session,
    quiet: Quiet,
    already_paused: bool,
    written: &[&str],
) -> (Released, Outcome) {
    if !already_paused {
        session.pause().await;
    }
    if written.is_empty() {
        let released = session.stop().await;
        return (released, Outcome::Kept(Why::NotWritten));
    }
    let seen = quiet_for(store, quiet).await;
    let released = session.stop().await;
    let outcome = match seen {
        Some(seen) if released == Released::Yes && store.activity().sample() == seen => {
            clear(store, seen, None, written).await
        }
        _ => Outcome::Kept(Why::Busy),
    };
    (released, outcome)
}

/// [`stop_quietly`] for a quit with no time left to look (fix G6, [`Quiet::within`]): the client is paused and stopped,
/// no flag is cleared (the next start asks again). The outcome says why.
pub(crate) async fn stop_without_look(
    session: &dyn Session,
    already_paused: bool,
) -> (Released, Outcome) {
    if !already_paused {
        session.pause().await;
    }
    (session.stop().await, Outcome::Kept(Why::NoTime))
}

pub(crate) mod map;
pub(crate) mod startup;
pub(crate) mod zone;

#[cfg(test)]
mod tests;
