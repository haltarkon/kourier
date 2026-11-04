//! The start-up resync (W6a1, chunk A5a; decisions D2, D4, B1 and M7 of `target/wa/w6-decisions.md`).
//!
//! WHY: upstream commits a patch's version BEFORE it hands the mutations on (`UP/wacore/src/appstate_sync.rs:1300-1330`),
//! so a kill between the two loses the events of that version, and an incremental sync never resends them. Only a
//! snapshot replays. Kourier's mirror ([`crate::store::chat_state`]) is a copy of what the phone said, so the repair is
//! one snapshot of each collection that may have lost something.
//!
//! WHAT IS A REASON (critic B1): only the dirty set READ WHEN THE STORE OPENS, before the client connects ([`open`]).
//! It is exactly what a kill or an unquiet quit left. A flag set in this session is none: the link's full sync or an
//! incremental sync in the catch-up would otherwise ask five snapshots at the riskiest moment. A failed zone in this
//! session ([`crate::appstate::Activity::lose`]) keeps its flag standing, and that flag IS the mark "resync at the
//! next start". The link session asks nothing of its own: [`link_in`] stamps the books so that a crash in it does not
//! either.
//!
//! HOW OFTEN: the reasons are asked ONE task after `Online` ([`run`], spawned by the actor on the client's runtime
//! through [`crate::session::Session::resync`]), one collection after another, each at most once per start (the
//! actor hands the list over once) and at most once per [`SPACING_MS`] (`appstate_resyncs.asked_at`, stamped BEFORE
//! the request so that a crash in the middle counts as asked: five starts in a crash loop ask once per 6 hours). A
//! collection the server answered fatally (400/404; asking again gets the same answer) is recorded
//! (`appstate_resyncs.fatal_at`) and asked again only after a new map version ([`MAP_VERSION`]) or a new link
//! (M7). Upstream's own retries, its key requests and its snapshot recovery stay upstream's (D3).
//!
//! THE MAP VERSION (D4): [`MAP_VERSION`] counts the verbs Kourier maps. A build whose number is higher than the one
//! the store was last read with marks the collections whose verb set grew dirty once ([`GREW`]), so the rows the older
//! build ignored are read again; a collection with no stored version never moved and is left alone.
//!
//! THE RECONCILE (A5b, critic H1 and M8): a snapshot "replays, it does not reconcile", so what the phone no longer has
//! stays in Kourier's mirror. After a snapshot Kourier asked for came back SYNCED, the mirror's rows of that collection
//! that the replay did not touch (they stayed on an older generation) go back to their defaults and the core is told,
//! in one zone ([`crate::appstate::zone::reconcile`]) ... but ONLY when the replay mentioned at least one mutation of the
//! collection: a report that calls a collection synced cannot tell an empty collection from a request the server chose
//! not to serve, and one empty answer must not unpin, unarchive and unmute everything. The proof is a count the handler
//! keeps while the snapshot is out ([`crate::appstate::Activity::replay_open`]); a collection that lost an event in this
//! process, or whose zone failed, is not reconciled either, and its flag stands. Snapshots upstream starts itself (the
//! link, a recovery) never pass here and reset nothing. Rows with an open intent of W6b are never reset.
//!
//! Counts only in the log: no collection's contents, no name, no number.

use std::collections::BTreeSet;

use kourier_backend_api::Clock;
use kourier_db::rusqlite::Transaction;
use tokio::task::JoinHandle;
use wacore::store::error::Result;

use super::{EVENTS_WRITTEN, Quiet, Resync, clear, quiet_for};
use crate::events::EventWriter;
use crate::store::{KourierStore, resyncs};

/// How many verbs Kourier maps, as a number: raise it WHENEVER a build maps an app-state verb the earlier one
/// ignored, and name the collections of that verb in [`GREW`]. 1 = W6a1 (archive, pin, mute, marked unread, clear and
/// delete of chats, delete for me, the call log); 2 = W6a2 (the address book's `contact` verb, B8b).
pub(crate) const MAP_VERSION: i64 = 2;

/// The collections whose verb set grew at each map version: (version, collections). A store last read with a lower
/// number marks the union of the rows above its number dirty. Version 1 is the first map: all five collections. Version 2
/// reads the `contact` verb, which lives in `critical_unblock_low` (B8b; W6a1 mapped nothing of it and the quiet quit may
/// have cleared that collection's flag, so the address book is read once by the collection's snapshot). No verb of
/// `critical_block` is mapped.
const GREW: &[(i64, &[&str])] = &[(1, EVENTS_WRITTEN), (2, &["critical_unblock_low"])];

/// One collection is asked for a snapshot at most once in this long (D2): five starts in a crash loop ask once.
pub(crate) const SPACING_MS: i64 = 6 * 60 * 60 * 1000;

/// How long a quiet quit waits for a resync that is running before it stops the client (M7); past it the task is ended,
/// which leaves the collection's flag standing (safe: the next start asks again). It is NOT a wait of its own: the
/// actor runs it BESIDE the quit's waits for announced history chunks and for the acks of sends in the air, under the
/// same deadline (`lost::QUIT_WAIT`, 15 s; fix F7, DUR-7), so the waits never add up and a clean quit stays inside the
/// network's `shutdown_grace` (20 s, see `Actor::finish`).
#[cfg(not(test))]
pub(crate) const STOP_WAIT: std::time::Duration = crate::history::lost::QUIT_WAIT;
#[cfg(test)]
pub(crate) const STOP_WAIT: std::time::Duration = std::time::Duration::from_millis(1000);

/// The collections whose verbs a store last read at map version `stored` did not have.
fn grown_since(stored: i64) -> Vec<String> {
    let mut names = BTreeSet::new();
    for (version, collections) in GREW {
        if *version > stored {
            names.extend(collections.iter().map(|name| (*name).to_owned()));
        }
    }
    names.into_iter().collect()
}

/// The link: the session of a fresh link needs no snapshot of Kourier's (upstream's initial full sync fills every
/// collection through the handler), so every collection counts as asked at `now`, a fatal mark from an earlier
/// account of this file goes, and the map version is the build's. It runs INSIDE the zone that appends `AccountReady`
/// for the link ([`crate::outbox::append_ready`]), so the books carry the time of the pairing, not the time the
/// link page was opened (fix F6, DUR-6): a kill a minute into the link session asks nothing for 6 hours from the
/// SCAN.
pub(crate) fn link_in(tx: &Transaction<'_>, now: i64) -> Result<()> {
    resyncs::stamp_link(tx, EVENTS_WRITTEN, now)?;
    // The time of the pairing, kept apart from the books (a later snapshot re-stamps those): a call of the phone's log
    // from before it adds no chat (F13, `calls::log::place`).
    resyncs::set_linked(tx, now)?;
    resyncs::set_map_version(tx, MAP_VERSION)
}

/// The store was opened for a run that is no link: the reasons for a resync, read BEFORE any client exists (B1).
/// A new map version first marks the grown collections dirty and clears the fatal marks; the reasons are the dirty
/// collections of [`EVENTS_WRITTEN`] without a fatal mark. They are also recorded as owed
/// ([`crate::appstate::Activity::owe`]), so no clearing point lets go of their flags before their resync is through.
pub(crate) async fn open(store: &KourierStore) -> Result<Vec<String>> {
    let stored = store.read(resyncs::map_version).await?;
    if stored < MAP_VERSION {
        let grew = grown_since(stored);
        let marked = store.redirty(grew).await?;
        store
            .write("record the app-state map version", |tx| {
                resyncs::clear_fatal(tx)?;
                resyncs::set_map_version(tx, MAP_VERSION)
            })
            .await?;
        tracing::info!(
            marked,
            "the app-state map grew; the collections it reads again are dirty"
        );
    }
    let fatal = store.read(resyncs::fatal_collections).await?;
    let reasons: Vec<String> = store
        .dirty_collections()
        .await?
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| EVENTS_WRITTEN.contains(&name.as_str()) && !fatal.contains(name))
        .collect();
    store.activity().owe(&reasons);
    Ok(reasons)
}

/// Whether one reason is asked now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Ask,
    /// The server answered fatally and nothing new happened since.
    Fatal,
    /// Asked less than [`SPACING_MS`] ago (or the clock went back).
    Spaced,
}

fn gate(book: Option<resyncs::Book>, now: i64) -> Gate {
    match book {
        Some(book) if book.fatal_at.is_some() => Gate::Fatal,
        Some(book) if book.asked_at.saturating_add(SPACING_MS) > now => Gate::Spaced,
        _ => Gate::Ask,
    }
}

/// What a run of the task did, in numbers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    asked: usize,
    synced: usize,
    fatal: usize,
    unsynced: usize,
    spaced: usize,
    held_back: usize,
    cleared: usize,
    busy: usize,
    /// Synced collections whose unmentioned rows went back to their defaults.
    reconciled: usize,
    /// Synced collections whose replay mentioned nothing: the mirror stayed (H1).
    empty: usize,
    /// Synced collections not reconciled because an event of theirs was lost in this process.
    lost: usize,
    /// Reconciles whose zone failed (the flag stands).
    reconcile_failed: usize,
}

/// The task: asks `reasons` (the open set) one after another and clears the flag of each that came back synced once
/// the connection has been quiet. Returns the reasons it did not get to because a request never reached the server
/// (the connection went away): they wait for the next `Online` of this start, which is the only way a collection is
/// asked twice in one start. A store that fails to answer ends the run the same way. `writer` is the handler's
/// (the reconcile is one of its zones: held by the link hold, durable before it returns).
pub(crate) async fn run(
    store: &KourierStore,
    writer: &EventWriter,
    resync: &dyn Resync,
    clock: &dyn Clock,
    quiet: Quiet,
    written: &[&str],
    reasons: Vec<String>,
) -> Vec<String> {
    let mut counts = Counts::default();
    let mut rest = reasons.into_iter();
    let mut waiting = Vec::new();
    while let Some(name) = rest.next() {
        let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
        let Ok(book) = store
            .read({
                let name = name.clone();
                move |conn| resyncs::book(conn, &name)
            })
            .await
        else {
            waiting.push(name);
            waiting.extend(rest);
            break;
        };
        match gate(book, now) {
            Gate::Fatal => {
                counts.held_back += 1;
                continue;
            }
            Gate::Spaced => {
                counts.spaced += 1;
                continue;
            }
            Gate::Ask => {}
        }
        // Stamped BEFORE the request: a kill in the middle of a replay counts as asked.
        let opened = store
            .write("record a resync", {
                let name = name.clone();
                move |tx| resyncs::asked(tx, &name, now)
            })
            .await;
        let Ok(generation) = opened else {
            waiting.push(name);
            waiting.extend(rest);
            break;
        };
        counts.asked += 1;
        tracing::debug!(generation, "a snapshot of an app-state collection is asked");
        // From here the handler counts the mutations of this collection it writes: the replay's (H1).
        store.activity().replay_open(&name);
        let answered = resync.resync(vec![name.clone()]).await;
        let mentioned = store.activity().replay_close(&name);
        let Ok(report) = answered else {
            // The request never reached the server: nothing was replayed. The rest waits for the next `Online`.
            tracing::warn!("the app-state resync did not reach the server");
            waiting.extend(rest);
            break;
        };
        if report.fatal.contains(&name) {
            counts.fatal += 1;
            let marked = store
                .write("record a fatal resync", {
                    let name = name.clone();
                    move |tx| resyncs::fatal(tx, &name, now)
                })
                .await;
            if marked.is_err() {
                tracing::warn!("a fatal resync answer could not be recorded");
            }
            continue;
        }
        if !report.synced.contains(&name) {
            counts.unsynced += 1;
            continue;
        }
        counts.synced += 1;
        match reconcile(store, writer, &name, generation, mentioned).await {
            Reconcile::Done => counts.reconciled += 1,
            Reconcile::Empty => counts.empty += 1,
            Reconcile::Lost => counts.lost += 1,
            Reconcile::Failed => {
                // The mirror may still hold what the phone dropped: the flag and the owed mark stand.
                counts.reconcile_failed += 1;
                continue;
            }
        }
        // The replay returned after every mutation was dispatched and the one handler waited for each write
        // (rule (a)): the events are durable, the collection owes nothing; its flag goes after the quiet look.
        store.activity().repaired(&name);
        match quiet_for(store, quiet).await {
            Some(seen) => {
                if let super::Outcome::Cleared(n) =
                    clear(store, seen, Some(vec![name.clone()]), written).await
                {
                    counts.cleared += n;
                }
            }
            None => counts.busy += 1,
        }
    }
    tracing::info!(
        asked = counts.asked,
        synced = counts.synced,
        fatal = counts.fatal,
        unsynced = counts.unsynced,
        spaced = counts.spaced,
        held_back = counts.held_back,
        cleared = counts.cleared,
        busy = counts.busy,
        reconciled = counts.reconciled,
        empty = counts.empty,
        lost = counts.lost,
        reconcile_failed = counts.reconcile_failed,
        waiting = waiting.len(),
        "the start-up resync ran"
    );
    waiting
}

/// What became of the reconcile of one synced collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reconcile {
    /// The zone ran (it may have found nothing to reset).
    Done,
    /// The replay mentioned nothing: the mirror stays (H1).
    Empty,
    /// An event of the collection was lost in this process: the replay was not whole, nothing is reset.
    Lost,
    /// The zone did not commit.
    Failed,
}

/// The reconcile of `collection` after its snapshot of `generation`, which mentioned `mentioned` mutations of it.
async fn reconcile(
    store: &KourierStore,
    writer: &EventWriter,
    collection: &str,
    generation: i64,
    mentioned: u64,
) -> Reconcile {
    if store
        .activity()
        .lost()
        .iter()
        .any(|lost| lost == collection)
    {
        return Reconcile::Lost;
    }
    if mentioned == 0 {
        return Reconcile::Empty;
    }
    let writer = writer.clone();
    let collection = collection.to_owned();
    let written = tokio::task::spawn_blocking(move || {
        writer.write_reconcile_and_wait(&collection, generation)
    })
    .await;
    match written {
        Ok(Ok(())) => Reconcile::Done,
        _ => Reconcile::Failed,
    }
}

/// The actor's side of the task: the reasons not yet asked in this start and the task that asks them.
#[derive(Default)]
pub(crate) struct Resyncer {
    /// The reasons nobody asked yet.
    pending: Vec<String>,
    /// The running task; its answer is the reasons it did not get to.
    task: Option<JoinHandle<Vec<String>>>,
}

impl Resyncer {
    /// The run's reasons ([`open`]).
    pub(crate) fn new(reasons: Vec<String>) -> Self {
        Self {
            pending: reasons,
            task: None,
        }
    }

    /// The connection is `Online`: starts the task over what is left, unless one is running. `spawn` runs the future
    /// the caller built over [`run`] (the actor knows the session, the clock and the store).
    pub(crate) async fn start<F>(&mut self, spawn: impl FnOnce(Vec<String>) -> F)
    where
        F: std::future::Future<Output = Vec<String>> + Send + 'static,
    {
        if let Some(task) = &self.task {
            if !task.is_finished() {
                return;
            }
            if let Some(task) = self.task.take() {
                // A task that died leaves its reasons unasked; they are not asked again in this start.
                self.pending.extend(task.await.unwrap_or_default());
            }
        }
        if self.pending.is_empty() {
            return;
        }
        let names = std::mem::take(&mut self.pending);
        self.task = Some(tokio::spawn(spawn(names)));
    }

    /// The client stops quietly: a running task gets up to `limit` to end, then it is ended (M7). Its flag stays
    /// standing either way unless the task cleared it.
    pub(crate) async fn finish(&mut self, limit: std::time::Duration) {
        let Some(mut task) = self.task.take() else {
            return;
        };
        match tokio::time::timeout(limit, &mut task).await {
            Ok(rest) => self.pending.extend(rest.unwrap_or_default()),
            Err(_) => {
                tracing::warn!("the quit did not wait longer for the app-state resync");
                task.abort();
                let _ = task.await;
            }
        }
    }

    /// The client goes without a wait: the task ends at once.
    pub(crate) async fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests;
