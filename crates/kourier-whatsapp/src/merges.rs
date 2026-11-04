//! The merge sweep: which numbers and which LIDs are ONE person (W6a2, chunk B9; decision D7 and the critic's H4 in
//! `target/wa/w6-decisions.md`; the contract is `kourier-backend-api` `BackendEvent::IdentityMerged`).
//!
//! WHY IT IS CAREFUL: a merge is FINAL. The core re-authors the alias's messages under the canonical id and nothing takes
//! it back (`kourier-core/src/ingest/merge.rs`), so Kourier states a pair only when the SERVER or OUR OWN PHONE vouches for
//! it, and a wrong one (a recycled number, a number change, a stale row) is worse than a missing one: a missing one is
//! repaired by the next message of the person, which carries both ids (W3b's live pairs: `map::vouched_pair`).
//!
//! THE SOURCES (D7 as amended by H4):
//! - the store's `lid_pn_mapping` (upstream learned the rows; the learning source is upstream's word for how), only the rows
//!   whose source is [`SOURCES`]: `usync` (the server answered a query for the number), `recipient_latest_lid` (the server
//!   named the recipient's latest LID), `migration_sync_latest` (our own phone's LID-migration push, which upstream consumes
//!   itself and stores here: this is how "our phone's LID-migration rows" reach Kourier) and `blocklist_active` (the server's
//!   list). NEVER `migration_sync_old` or `blocklist_inactive` (upstream calls them stale), `device_notification` or `other`
//!   (observational), `pairing` (our own device) or a peer's message (`peer_pn_message`, `peer_lid_message`: the live path
//!   merges what a stanza vouches for, in the same message);
//! - the phone's address book (`address_book`, B8b): a `contact` mutation that names both the number and the LID of the
//!   person is our phone's authenticated word. Upstream does NOT learn these pairs, and Kourier does not feed them to
//!   upstream either: its feeder would send a query for the numbers it cannot place (a burst for an address book of hundreds)
//!   and would choose the authority of a row upstream routes sends by.
//!
//! THE RULES, each a test: a number has ONE current LID (the newest row; the table keeps old ones, `get_pn_mapping` takes the
//! newest), and only that one can merge, so a recycled number never joins the older LID; when the store's current row and the
//! address book disagree, or the book names two LIDs for one number, NOTHING is merged for that number; a LID named by two
//! numbers (a number change) merges neither; a pair that would put a second pair around a number or a LID the core was already
//! told about is never stated (`merges_stated`, the contract's "final"); our own ids are skipped; and only a pair of which
//! the core KNOWS one of the two people is stated at all (`persons_known`, D8's scope: a stranger's number from the phone's
//! contacts never reaches core.db). That scope is applied to the pairs `decide` CHOSE, never to what it reads: a stranger's
//! row is still a witness against a pair (W6a2 PRIV-3). Each pair is stated ONCE ([`state`], `merges_stated`); a `Rebuild` forgets the statements
//! and the next sweep states them anew.
//!
//! WHEN: [`Sweeper`] runs the sweep when the connection comes `Online` and again when something that can make a pair due
//! happened: the writer committed an address-book change or a group's member list, the history delivery finished its quiet
//! step. A pair needs no network: the sweep reads the store and writes through the holding event writer, in zones of at most
//! [`PER_ZONE`] pairs. A person who first appears in a message mid-connection is merged at the next of those, or by the
//! message's own pair (W3b) when it has one.
//!
//! Nothing here asks the network and nothing is sent: counts only in the log, never a number, an id or a name.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, Transaction};
use kourier_model::UserId;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use wacore::store::error::StoreError;

use crate::events::EventWriter;
use crate::map::OwnIds;
use crate::stage2::Stage2;
use crate::store::KourierStore;
use crate::store::merges::{self as rows, Current};

/// The learning sources of `lid_pn_mapping` Kourier states a merge from. See the module doc; a source that is not in this
/// list is not a source (a new one upstream adds is silent until someone reads what it means).
pub(crate) const SOURCES: [&str; 4] = [
    "usync",
    "recipient_latest_lid",
    "migration_sync_latest",
    "blocklist_active",
];

/// The most pairs one zone states.
pub(crate) const PER_ZONE: usize = 256;

/// Whether the store's `learning_source` is one a merge can stand on.
pub(crate) fn vouches(source: &str) -> bool {
    SOURCES.contains(&source)
}

/// A number and a LID that are one person, as the contract's ids. `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Pair {
    pub(crate) pn: UserId,
    pub(crate) lid: UserId,
}

impl std::fmt::Debug for Pair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pair(..)")
    }
}

/// What a sweep did and left, in numbers (the log's only content).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Pairs chosen to be stated.
    pub(crate) merged: usize,
    /// Numbers whose only row is of a source that does not vouch.
    pub(crate) unvouched: usize,
    /// Numbers (or LIDs) the store and the address book, or two numbers, disagree about.
    pub(crate) conflicts: usize,
    /// Pairs that name one of our own ids.
    pub(crate) own: usize,
    /// Pairs that would cross a pair the core was already told.
    pub(crate) crossing: usize,
    /// Chosen pairs of nobody the core knows (D8's scope: a stranger's number stays out of core.db). Not a reason to log
    /// by itself: an address book of hundreds leaves the same count at every sweep.
    pub(crate) strangers: usize,
}

/// The pairs `currents` (the store's current row per number) and `book` (the address book's pairs) vouch for, as encoded
/// ids `(number, lid)` in number order, and what was left out and why. PURE: the rules of the module doc, in this order:
/// per number, the store's current row stands alone only with a vouching source; the address book alone stands when it
/// names one LID; both together stand when they name the same LID (our phone agrees with the store, whatever its source);
/// anything else is left; then a LID that two numbers claim is left for both.
pub(crate) fn decide(
    currents: &[Current],
    book: &[(String, String)],
) -> (Vec<(String, String)>, Counts) {
    let mut counts = Counts::default();
    let mut by_number: BTreeMap<&str, (Option<&Current>, BTreeSet<&str>)> = BTreeMap::new();
    for current in currents {
        by_number.entry(&current.pn).or_default().0 = Some(current);
    }
    for (pn, lid) in book {
        by_number.entry(pn).or_default().1.insert(lid);
    }
    let mut chosen: BTreeMap<&str, &str> = BTreeMap::new();
    for (pn, (store, named)) in &by_number {
        let pick = match (store, named.len()) {
            (Some(current), 0) if vouches(&current.source) => Some(current.lid.as_str()),
            (Some(_), 0) => {
                counts.unvouched += 1;
                None
            }
            (Some(current), 1) if named.contains(current.lid.as_str()) => {
                Some(current.lid.as_str())
            }
            (None, 1) => named.iter().next().copied(),
            _ => {
                counts.conflicts += 1;
                None
            }
        };
        if let Some(lid) = pick {
            chosen.insert(pn, lid);
        }
    }
    let mut claims: BTreeMap<&str, usize> = BTreeMap::new();
    for lid in chosen.values() {
        *claims.entry(lid).or_default() += 1;
    }
    let mut pairs = Vec::new();
    for (pn, lid) in chosen {
        if claims.get(lid).copied().unwrap_or(0) > 1 {
            counts.conflicts += 1;
        } else {
            pairs.push((pn.to_owned(), lid.to_owned()));
        }
    }
    counts.merged = pairs.len();
    (pairs, counts)
}

/// The pairs to state now: [`decide`] over EVERYTHING the store and the address book hold, then our own ids, every pair of
/// nobody the core knows (`persons_known`: D8's scope) and every pair that would cross a pair the core was told left out.
/// The scope comes AFTER `decide` on purpose (W6a2 PRIV-3): a number whose store row names a stranger's LID while the book
/// names a known one is a conflict, not a pair of the known one, and a book that names two LIDs for one number names two
/// whichever of them the core knows. `own` must know both our ids (the caller checks).
pub(crate) fn gather(conn: &Connection, own: &OwnIds) -> Result<(Vec<Pair>, Counts), StoreError> {
    let (chosen, mut counts) = decide(&rows::currents(conn)?, &rows::book_pairs(conn)?);
    let mut pairs = Vec::with_capacity(chosen.len());
    for (pn, lid) in chosen {
        let (Ok(pn_id), Ok(lid_id)) = (UserId::parse(&pn), UserId::parse(&lid)) else {
            counts.merged -= 1;
            continue;
        };
        if own.is_own(&pn_id) || own.is_own(&lid_id) {
            counts.merged -= 1;
            counts.own += 1;
        } else if !rows::either_known(conn, &pn, &lid)? {
            counts.merged -= 1;
            counts.strangers += 1;
        } else if rows::crosses_a_stated_pair(conn, &pn, &lid)? {
            counts.merged -= 1;
            counts.crossing += 1;
        } else {
            pairs.push(Pair {
                pn: pn_id,
                lid: lid_id,
            });
        }
    }
    Ok((pairs, counts))
}

/// The zone: states each pair the core was not told, as `IdentityMerged { from: number, into: lid }`, and records it in
/// `merges_stated` in the same transaction. The zone is the authority, whatever the sweep read a moment before: a pair that
/// is stated already, crosses one that is, or names nobody the core knows is left out here. How many pairs went out.
pub(crate) fn state(
    tx: &Transaction<'_>,
    pairs: &[Pair],
) -> Result<(Vec<BackendEvent>, usize), StoreError> {
    let mut events = Vec::new();
    for pair in pairs {
        let (pn, lid) = (pair.pn.as_str(), pair.lid.as_str());
        if !rows::either_known(tx, pn, lid)? || rows::crosses_a_stated_pair(tx, pn, lid)? {
            continue;
        }
        if rows::put_stated(tx, pn, lid)? {
            events.push(BackendEvent::IdentityMerged {
                from: pair.pn.clone(),
                into: pair.lid.clone(),
            });
        }
    }
    let stated = events.len();
    // The person is the LID's from now on: his address-book name and the number the phone shows with it, which the core
    // keeps for the number's id only (a merge carries a name, never the verdict that a number may be shown), are stated for
    // him, after the merges (B8b's machinery; nothing is stated for a person the address book does not name).
    let lids: Vec<UserId> = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::IdentityMerged { into, .. } => Some(into.clone()),
            _ => None,
        })
        .collect();
    events.extend(crate::book::states(tx, &lids)?);
    Ok((events, stated))
}

/// The pairs `events` state (`IdentityMerged { from: number, into: LID }`, the live path's own: a message's `sender_alt`, a
/// notice's members, a receipt's actor, a history chat that names both ids) are recorded as stated, in the zone that writes
/// them. The sweep never states anything but a pair of its own sources, but the contract's "final" holds for every pair the
/// core was told by whatever path: a number that a message merged into one LID must not be merged into another because the
/// store's newest row says so (a recycled number), and the sweep's crossing check reads this record.
pub(crate) fn record(tx: &Transaction<'_>, events: &[BackendEvent]) -> Result<(), StoreError> {
    for event in events {
        if let BackendEvent::IdentityMerged { from, into } = event
            && from.scheme() == "pn"
            && into.scheme() == "lid"
        {
            rows::put_stated(tx, from.as_str(), into.as_str())?;
        }
    }
    Ok(())
}

/// How one sweep ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The pairs due were stated (there may have been none).
    Done,
    /// Nothing was read or written: our ids are not both known, or a read or a zone failed. The next trigger looks again.
    Later,
}

/// One sweep: reads what is due and states it, [`PER_ZONE`] pairs to a zone. The log says what became of it in numbers.
pub(crate) async fn sweep(store: &KourierStore, writer: &EventWriter, stage2: &Stage2) -> Outcome {
    let own = stage2.own().await;
    if own.pn.is_none() || own.lid.is_none() {
        tracing::debug!("the merges are not looked at: our own ids are not both known yet");
        return Outcome::Later;
    }
    let found = store.read(move |conn| gather(conn, &own)).await;
    let Ok((pairs, counts)) = found else {
        tracing::warn!(
            "the merges could not be read; they are looked at again at the next trigger"
        );
        return Outcome::Later;
    };
    for chunk in pairs.chunks(PER_ZONE) {
        let (chunk, writer) = (chunk.to_vec(), writer.clone());
        if !matches!(
            tokio::task::spawn_blocking(move || writer.write_merges_and_wait(&chunk)).await,
            Ok(Ok(()))
        ) {
            tracing::warn!(
                "the merges could not be stated; they are looked at again at the next trigger"
            );
            return Outcome::Later;
        }
    }
    if counts.merged + counts.unvouched + counts.conflicts + counts.own + counts.crossing > 0 {
        tracing::info!(
            merged = counts.merged,
            unvouched = counts.unvouched,
            conflicts = counts.conflicts,
            own = counts.own,
            crossing = counts.crossing,
            strangers = counts.strangers,
            "the merges were looked at"
        );
    }
    Outcome::Done
}

/// The actor's side: ONE task per client that sweeps when it starts and again at every wake.
#[derive(Default)]
pub(crate) struct Sweeper {
    task: Option<JoinHandle<()>>,
}

impl Sweeper {
    /// The connection is `Online`: starts the task unless one runs. `wake` is the writer's (and the history's) notify: a
    /// permit that came while no task ran gives one more sweep at the start, which costs two reads.
    pub(crate) fn start(
        &mut self,
        store: KourierStore,
        writer: EventWriter,
        stage2: Arc<Stage2>,
        wake: Arc<Notify>,
    ) {
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            wake.notify_one();
            return;
        }
        self.task = Some(tokio::spawn(async move {
            loop {
                sweep(&store, &writer, &stage2).await;
                wake.notified().await;
            }
        }));
    }

    /// The client goes: the task ends at once.
    pub(crate) async fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests;
