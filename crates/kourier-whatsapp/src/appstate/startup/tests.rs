//! The start-up resync over a real store (`backend.db` in a temp dir), a double of the client and a fake clock
//! (W6a1, chunk A5a). Collection names are upstream's wire names; nothing here is a person's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kourier_backend_api::{BackendEvent, FakeClock};
use kourier_db::DbKey;
use kourier_model::{BackendError, ContactPatch, ConvKey, ConversationPatch, Patch, UserId};
use tokio::sync::Notify;
use wacore::appstate::hash::HashState;
use wacore::store::traits::AppSyncStore;

use super::{MAP_VERSION, SPACING_MS, link_in, open, run};
use crate::appstate::map::{Mapped, Mirror, Setting};
use crate::appstate::{EVENTS_WRITTEN, Quiet, Resync, ResyncReport};
use crate::events::EventWriter;
use crate::features;
use crate::send::started;
use crate::store::chat_state::{self, Field};
use crate::store::{KourierStore, StoreOwner, resyncs};

/// A time on the wall clock (the books are stamped by the clock the task is given).
const T0: u64 = 1_790_000_000_000;

const QUIET: Quiet = Quiet {
    window: Duration::from_millis(20),
    bound: Duration::from_millis(200),
};

const SIX_HOURS: Duration = Duration::from_millis(SPACING_MS as u64);

fn owner() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([5; 32]),
        "wa-test",
    )
    .expect("open");
    (dir, owner)
}

/// The link's stamp (`outbox::append_ready` runs the same function inside the zone of `AccountReady`).
async fn link(store: &KourierStore, now: i64) -> wacore::store::error::Result<()> {
    store
        .write("stamp the resync books at the link", move |tx| {
            link_in(tx, now)
        })
        .await
}

fn state(version: u64) -> HashState {
    HashState {
        version,
        ..HashState::default()
    }
}

async fn dirty(store: &KourierStore) -> Vec<String> {
    store
        .dirty_collections()
        .await
        .expect("dirty")
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Sets a collection's version (which flags it dirty).
async fn moved(store: &KourierStore, name: &str, version: u64) {
    store.set_version(name, state(version)).await.unwrap();
}

/// The books of a collection.
async fn book(store: &KourierStore, name: &str) -> Option<resyncs::Book> {
    let name = name.to_owned();
    store
        .read(move |conn| resyncs::book(conn, &name))
        .await
        .unwrap()
}

/// The double of upstream's resync: records what it was asked for and what the store showed while it ran.
struct Server {
    store: KourierStore,
    asked: Mutex<Vec<Vec<String>>>,
    /// The dirty collections the store showed during each call.
    showed: Mutex<Vec<Vec<String>>>,
    /// The collections the report calls synced (the others come back retryable).
    syncs: Mutex<Vec<&'static str>>,
    /// The collections the report calls fatal.
    fatal: Mutex<Vec<&'static str>>,
    /// The request never reaches the server.
    unreachable: AtomicBool,
    /// What the snapshot of a collection replays: the mapped mutations the handler writes while it runs.
    replays: Mutex<Vec<(&'static str, Vec<Mapped>)>>,
    /// Mutations of ANOTHER collection that arrive while any snapshot of ours runs: upstream's own full sync of a
    /// collection it found without a baseline (the link, a recovery), which the handler writes like any other.
    alongside: Mutex<Vec<Mapped>>,
    /// PATCHES (`from_full_sync` false) that arrive inside the window, before the snapshot is served: an incremental
    /// sync of the collection that held upstream's reservation when the task asked (DUR-1).
    patches: Mutex<Vec<Mapped>>,
    /// The collections that lose an event during the replay (a zone of the handler failed).
    loses: Mutex<Vec<&'static str>>,
    /// The hold closes while the replay runs (the reconcile that follows cannot get through).
    closes_the_hold: AtomicBool,
}

impl Server {
    fn new(store: &KourierStore, syncs: &[&'static str]) -> Self {
        Self {
            store: store.clone(),
            asked: Mutex::new(Vec::new()),
            showed: Mutex::new(Vec::new()),
            syncs: Mutex::new(syncs.to_vec()),
            fatal: Mutex::new(Vec::new()),
            unreachable: AtomicBool::new(false),
            replays: Mutex::new(Vec::new()),
            alongside: Mutex::new(Vec::new()),
            patches: Mutex::new(Vec::new()),
            loses: Mutex::new(Vec::new()),
            closes_the_hold: AtomicBool::new(false),
        }
    }

    /// The snapshot of `collection` replays `mapped`.
    fn replaying(self, collection: &'static str, mapped: Vec<Mapped>) -> Self {
        self.replays.lock().unwrap().push((collection, mapped));
        self
    }

    fn asked(&self) -> Vec<Vec<String>> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Resync for Server {
    async fn resync(&self, collections: Vec<String>) -> Result<ResyncReport, BackendError> {
        let showing = dirty(&self.store).await;
        self.showed.lock().unwrap().push(showing);
        self.asked.lock().unwrap().push(collections.clone());
        if self.unreachable.load(Ordering::SeqCst) {
            return Err(BackendError::transient("the request did not leave"));
        }
        // The replay: what the handler does for each mutation (its zone, then the count).
        let writer = writer(&self.store);
        for mapped in self.patches.lock().unwrap().clone() {
            writer
                .write_settings_and_wait(mapped)
                .expect("a patch inside the window");
        }
        for mut mapped in self.alongside.lock().unwrap().clone() {
            mapped.from_full_sync = true;
            writer
                .write_settings_and_wait(mapped)
                .expect("an upstream-started replay");
        }
        for name in &collections {
            let replay: Vec<Mapped> = self
                .replays
                .lock()
                .unwrap()
                .iter()
                .filter(|(collection, _)| collection == name)
                .flat_map(|(_, mapped)| mapped.clone())
                .collect();
            for mut mapped in replay {
                mapped.from_full_sync = true;
                writer
                    .write_settings_and_wait(mapped)
                    .expect("a replayed mutation");
            }
            if let Some(lost) = self.loses.lock().unwrap().iter().find(|l| **l == name) {
                self.store.activity().lose(lost);
            }
        }
        if self.closes_the_hold.load(Ordering::SeqCst) {
            self.store.hold().set_closed(true);
        }
        let pick = |list: &Mutex<Vec<&'static str>>| {
            let list = list.lock().unwrap();
            collections
                .iter()
                .filter(|name| list.contains(&name.as_str()))
                .cloned()
                .collect()
        };
        Ok(ResyncReport {
            synced: pick(&self.syncs),
            fatal: pick(&self.fatal),
        })
    }
}

/// The handler's writer over `store` (the task's reconcile and the double's replay go through one like it).
fn writer(store: &KourierStore) -> EventWriter {
    EventWriter::new(
        store.clone(),
        Arc::new(FakeClock::new(T0)),
        Arc::new(Notify::new()),
    )
}

fn clock(offset: Duration) -> FakeClock {
    FakeClock::new(T0 + u64::try_from(offset.as_millis()).unwrap())
}

/// The start-up task: the collections are asked ONE AFTER ANOTHER, a flag goes only for a collection the report
/// called synced and only AFTER its call returned, and a collection that came back unsynced keeps its flag AND its
/// owed mark. Kills "cleared before the resync returned" (the store shows nothing dirty during the call), "all
/// collections in one request" and "the owed mark dropped when asked".
#[tokio::test]
async fn the_reasons_are_asked_one_after_another_and_only_a_synced_one_is_cleared_after_its_call() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular", 4).await;
    moved(&store, "critical_block", 7).await;
    let reasons = open(&store).await.unwrap();
    assert_eq!(reasons, ["critical_block", "regular"]);
    assert_eq!(store.activity().owed(), ["critical_block", "regular"]);

    let server = Server::new(&store, &["regular"]);
    let left = run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert!(left.is_empty());
    assert_eq!(server.asked(), [["critical_block"], ["regular"]]);
    assert_eq!(
        *server.showed.lock().unwrap(),
        [["critical_block", "regular"], ["critical_block", "regular"]],
        "the flags stood while each resync ran"
    );
    assert_eq!(dirty(&store).await, ["critical_block"]);
    assert_eq!(
        store.activity().owed(),
        ["critical_block"],
        "an unsynced collection is still owed"
    );
    assert_eq!(
        book(&store, "regular").await.map(|b| b.generation),
        Some(1),
        "a resync of ours opened the first generation"
    );
    owner.close();
}

/// Once per 6 hours (D2): a second start inside the window asks nothing, and the collection that is still dirty is
/// asked again as soon as the window is over, not a millisecond before. Kills "spacing 0" and "off by one".
#[tokio::test]
async fn a_second_start_within_six_hours_asks_nothing_and_the_first_after_them_asks_again() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "critical_block", 7).await;
    let server = Server::new(&store, &[]);
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert_eq!(server.asked().len(), 1);

    // The next start: still dirty, still owed, but asked a moment ago.
    let reasons = open(&store).await.unwrap();
    assert_eq!(reasons, ["critical_block"]);
    for offset in [
        Duration::from_secs(60),
        SIX_HOURS - Duration::from_millis(1),
    ] {
        run(
            &store,
            &writer(&store),
            &server,
            &clock(offset),
            QUIET,
            EVENTS_WRITTEN,
            reasons.clone(),
        )
        .await;
        assert_eq!(server.asked().len(), 1, "inside the window at {offset:?}");
    }
    assert_eq!(dirty(&store).await, ["critical_block"]);
    assert_eq!(store.activity().owed(), ["critical_block"]);

    // The window is over: asked again; this time the server syncs it.
    server.syncs.lock().unwrap().push("critical_block");
    run(
        &store,
        &writer(&store),
        &server,
        &clock(SIX_HOURS),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert_eq!(server.asked().len(), 2);
    assert!(dirty(&store).await.is_empty());
    assert!(store.activity().owed().is_empty());
    owner.close();
}

/// Five starts in a crash loop: the request is stamped BEFORE it goes, so a process that dies in the middle (or a
/// request that never leaves) still counts as asked, and the loop asks once in 6 hours. A run that meets a request
/// that never reached the server hands the collections it did not get to back. Kills "stamped after the request" and
/// "the run goes on after an unreachable server".
#[tokio::test]
async fn five_starts_in_a_crash_loop_ask_once_and_an_unreachable_server_ends_the_run() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular", 4).await;
    moved(&store, "critical_block", 7).await;
    let server = Server::new(&store, &[]);
    server.unreachable.store(true, Ordering::SeqCst);

    let reasons = open(&store).await.unwrap();
    let left = run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert_eq!(server.asked().len(), 1, "the run stopped at the first");
    assert_eq!(left, ["regular"], "the one it did not get to");

    for start in 1..5u64 {
        let reasons = open(&store).await.unwrap();
        let left = run(
            &store,
            &writer(&store),
            &server,
            &clock(Duration::from_secs(600 * start)),
            QUIET,
            EVENTS_WRITTEN,
            reasons,
        )
        .await;
        assert!(
            left.is_empty(),
            "critical_block is spaced, regular is asked"
        );
    }
    // critical_block once at the first start, regular once at the second: two collections, one request each.
    assert_eq!(
        server.asked(),
        [["critical_block"], ["regular"]],
        "a crash loop asks each collection once"
    );
    owner.close();
}

/// M7: a fatal answer is recorded; the collection is not a reason and not asked again after the window either, until
/// a new map version or a new link. Kills "`fatal_at` ignored".
#[tokio::test]
async fn a_fatal_answer_is_asked_again_only_after_a_new_map_version_or_a_new_link() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular", 4).await;
    let server = Server::new(&store, &[]);
    server.fatal.lock().unwrap().push("regular");

    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons.clone(),
    )
    .await;
    assert_eq!(server.asked().len(), 1);
    assert!(book(&store, "regular").await.unwrap().fatal_at.is_some());
    assert_eq!(dirty(&store).await, ["regular"], "the flag stands");

    // The next start: the flag is in the dirty set but no reason; even handed in, it is not asked.
    assert!(open(&store).await.unwrap().is_empty());
    run(
        &store,
        &writer(&store),
        &server,
        &clock(SIX_HOURS * 2),
        QUIET,
        EVENTS_WRITTEN,
        reasons.clone(),
    )
    .await;
    assert_eq!(server.asked().len(), 1, "a fatal collection is not asked");

    // A new map version: a reason again, asked after the window.
    store
        .write("test", |tx| resyncs::set_map_version(tx, 0))
        .await
        .unwrap();
    let again = open(&store).await.unwrap();
    assert_eq!(again, ["regular"]);
    assert_eq!(
        book(&store, "regular").await.unwrap().fatal_at,
        None,
        "the new map version cleared the mark"
    );
    run(
        &store,
        &writer(&store),
        &server,
        &clock(SIX_HOURS * 2),
        QUIET,
        EVENTS_WRITTEN,
        again,
    )
    .await;
    assert_eq!(server.asked().len(), 2);
    assert!(book(&store, "regular").await.unwrap().fatal_at.is_some());

    // A new link clears the mark too (and stamps the books: nothing is asked for 6 hours).
    link(&store, i64::try_from(T0).unwrap()).await.unwrap();
    assert_eq!(book(&store, "regular").await.unwrap().fatal_at, None);
    owner.close();
}

/// F13: the link keeps its own time, apart from the books: `asked_at` moves with every snapshot of ours, the pairing's
/// time does not; a store that saw no link has none; a new link of the same file rewrites it. Kills "the link writes
/// no time" and "the time kept in the books".
#[tokio::test]
async fn the_link_keeps_the_pairings_time_apart_from_the_books() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let linked = |store: &KourierStore| {
        let store = store.clone();
        async move { store.read(resyncs::linked_at).await.unwrap() }
    };
    assert_eq!(linked(&store).await, None, "no link, no time");
    link(&store, 5_000).await.unwrap();
    assert_eq!(linked(&store).await, Some(5_000));
    assert_eq!(book(&store, "regular").await.unwrap().asked_at, 5_000);
    // A snapshot of ours later moves the book and leaves the pairing's time.
    store
        .write("ask", |tx| resyncs::asked(tx, "regular", 90_000))
        .await
        .unwrap();
    assert_eq!(book(&store, "regular").await.unwrap().asked_at, 90_000);
    assert_eq!(linked(&store).await, Some(5_000));
    // A new link of the file is a new pairing.
    link(&store, 200_000).await.unwrap();
    assert_eq!(linked(&store).await, Some(200_000));
    owner.close();
}

/// A report that is neither synced nor fatal (retryable, skipped) clears nothing, records nothing, and the
/// collection stays owed.
#[tokio::test]
async fn an_unsynced_report_keeps_the_flag_the_owed_mark_and_records_no_fatal() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular", 4).await;
    let server = Server::new(&store, &[]);
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert_eq!(dirty(&store).await, ["regular"]);
    assert_eq!(store.activity().owed(), ["regular"]);
    assert_eq!(book(&store, "regular").await.unwrap().fatal_at, None);
    owner.close();
}

/// The list of the caller limits the clear: both came back synced, the list names one.
#[tokio::test]
async fn only_the_listed_collections_are_cleared_after_a_resync() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular", 4).await;
    moved(&store, "critical_block", 7).await;
    let server = Server::new(&store, &["regular", "critical_block"]);
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        &["critical_block"],
        reasons,
    )
    .await;
    assert_eq!(dirty(&store).await, ["regular"]);
    owner.close();
}

/// B1 point 1 and D4: the reasons are the dirty set of the five collections at the open; a flag of a name Kourier
/// does not list is none. (The flag of this session is no reason because nothing reads the set again: the actor's
/// test pins that.)
#[tokio::test]
async fn the_reasons_are_the_dirty_collections_of_the_list_at_open() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular_low", 3).await;
    moved(&store, "some_future_collection", 1).await;
    assert_eq!(open(&store).await.unwrap(), ["regular_low"]);
    owner.close();
}

/// D4: a build that maps more verbs marks the collections that grew dirty ONCE; a collection that never moved is
/// left alone, and the same build at the next start marks nothing. Kills "dirty at every open" and "dirty with no
/// stored version".
#[tokio::test]
async fn a_new_map_version_marks_the_grown_collections_dirty_once() {
    let (_dir, owner) = owner();
    let store = owner.store();
    moved(&store, "regular_low", 3).await;
    moved(&store, "regular", 2).await;
    // Bootstrapped and empty: nothing was ever applied, so nothing can have been lost.
    moved(&store, "regular_high", 0).await;
    // They were cleared by an earlier build's quiet quit.
    let seen = store.activity().writes();
    store.clear_quiet(seen, None).await.unwrap();
    assert!(dirty(&store).await.is_empty());

    assert_eq!(
        store.read(resyncs::map_version).await.unwrap(),
        0,
        "no row reads 0"
    );
    let reasons = open(&store).await.unwrap();
    assert_eq!(
        reasons,
        ["regular", "regular_low"],
        "the two that ever moved; critical_block has no version and regular_high is at 0"
    );
    assert_eq!(store.read(resyncs::map_version).await.unwrap(), MAP_VERSION);

    // The same build at the next start: cleared again, nothing marked.
    let seen = store.activity().writes();
    store.clear_quiet(seen, None).await.unwrap();
    assert!(open(&store).await.unwrap().is_empty());
    owner.close();
}

/// B8b: a store last read at map version 1 (W6a1, which mapped nothing of the `contact` verb and whose quiet quit may have
/// cleared that collection's flag) reads `critical_unblock_low` once at this build, and no other collection. Kills "GREW
/// without the address book's collection" (nothing marked) and "every collection again" (more reasons).
#[tokio::test]
async fn a_store_at_map_version_one_asks_the_address_books_collection_once() {
    let (_dir, owner) = owner();
    let store = owner.store();
    for name in ["regular_low", "critical_unblock_low", "regular"] {
        moved(&store, name, 3).await;
    }
    let seen = store.activity().writes();
    store.clear_quiet(seen, None).await.unwrap();
    assert!(dirty(&store).await.is_empty());
    store
        .write("test: map version 1", |tx| resyncs::set_map_version(tx, 1))
        .await
        .unwrap();

    let reasons = open(&store).await.unwrap();
    assert_eq!(
        reasons,
        ["critical_unblock_low"],
        "only the collection of the new verb"
    );
    assert_eq!(store.read(resyncs::map_version).await.unwrap(), MAP_VERSION);

    // Once: cleared by the quiet quit, the same build marks nothing at the next start.
    let seen = store.activity().writes();
    store.clear_quiet(seen, None).await.unwrap();
    assert!(open(&store).await.unwrap().is_empty());
    owner.close();
}

/// The link (critic B1 point 4): every collection of the list counts as asked at the link and the map version is the
/// build's, so a crash in the link session asks nothing for 6 hours and a fresh store marks nothing at its first
/// open. Kills "the link stamps nothing".
#[tokio::test]
async fn the_link_stamps_every_collection_so_a_crash_in_the_link_session_asks_nothing() {
    let (_dir, owner) = owner();
    let store = owner.store();
    link(&store, i64::try_from(T0).unwrap()).await.unwrap();
    for name in EVENTS_WRITTEN {
        assert_eq!(
            book(&store, name).await.map(|b| b.asked_at),
            Some(i64::try_from(T0).unwrap()),
            "{name}"
        );
    }
    assert_eq!(store.read(resyncs::map_version).await.unwrap(), MAP_VERSION);

    // The link session's full sync moved everything; a kill left the flags. The next start: five reasons, none asked.
    for name in EVENTS_WRITTEN {
        moved(&store, name, 2).await;
    }
    let server = Server::new(&store, EVENTS_WRITTEN);
    let reasons = open(&store).await.unwrap();
    assert_eq!(reasons.len(), 5);
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::from_secs(3600)),
        QUIET,
        EVENTS_WRITTEN,
        reasons.clone(),
    )
    .await;
    assert!(server.asked().is_empty(), "inside the window of the link");
    run(
        &store,
        &writer(&store),
        &server,
        &clock(SIX_HOURS),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;
    assert_eq!(server.asked().len(), 5, "one request per collection after");
    owner.close();
}

// The reconcile (A5b, critic H1). Two chats of the fictitious 555-01xx range, tails that are substrings of no other
// value.
const FIRST: &str = "15550148296037";
const SECOND: &str = "15550172940518";

fn direct(value: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", value).expect("a user"))
}

/// The core knows `chat`: its features were stated.
async fn known(store: &KourierStore, chat: &ConvKey) {
    let chat = chat.clone();
    store
        .write("known", move |tx| {
            started::mark_stated(tx, &chat, features::VERSION, true)
        })
        .await
        .expect("mark stated");
}

/// The phone's mutation as the handler hands it to the writer.
fn mutation(chat: &ConvKey, field: Field, value: i64, at: i64) -> Mapped {
    Mapped {
        collection: match field {
            Field::Muted => "regular_high",
            _ => "regular_low",
        },
        mirror: Some(Mirror::Chat(Setting {
            chat: chat.clone(),
            field,
            value,
            action_ms: at,
        })),
        events: Vec::new(),
        from_full_sync: false,
    }
}

/// The events of every `core_outbox` row, in order.
async fn outbox(store: &KourierStore) -> Vec<BackendEvent> {
    store
        .read(|conn| {
            let rows =
                kourier_db::outbox::peek(conn, 1000).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .iter()
                .flat_map(|row| {
                    kourier_backend_api::decode_outbox_row(&row.envelope).expect("decodes")
                })
                .collect())
        })
        .await
        .expect("read the outbox")
}

/// What the mirror holds for a chat's field: (value, generation).
async fn held(store: &KourierStore, chat: &ConvKey, field: Field) -> Option<(i64, i64)> {
    let chat = chat.clone();
    store
        .read(move |conn| chat_state::get(conn, &chat, field))
        .await
        .expect("get")
        .map(|row| (row.value, row.generation))
}

/// Two chats the phone muted and one archived, written live (generation 0) as the handler would; `regular_high` was
/// left dirty by a kill. Returns the store's events so far.
async fn muted_world(store: &KourierStore) -> (ConvKey, ConvKey) {
    let (first, second) = (direct(FIRST), direct(SECOND));
    known(store, &first).await;
    known(store, &second).await;
    let live = writer(store);
    for mapped in [
        mutation(&first, Field::Muted, i64::MAX, 10),
        mutation(&second, Field::Muted, i64::MAX, 11),
        mutation(&second, Field::Archived, 1, 12),
    ] {
        live.write_settings_and_wait(mapped)
            .expect("a live mutation");
    }
    moved(store, "regular_high", 5).await;
    assert_eq!(outbox(store).await.len(), 3, "three live events");
    (first, second)
}

/// H1: a snapshot of ours that came back synced and replayed a mutation of the collection resets the rows of THAT
/// collection it did not mention, and tells the core; the row it did mention, and the other collection's rows, stay.
/// Kills "the reconcile never runs", "a row of the generation reset too" and "the collection ignored".
#[tokio::test]
async fn an_unseen_mute_is_reset_after_a_replay_that_touched_another_row() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]).replaying(
        "regular_high",
        vec![mutation(&second, Field::Muted, i64::MAX, 20)],
    );
    let reasons = open(&store).await.unwrap();
    assert_eq!(reasons, ["regular_high"]);
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    // The replay said nothing of the first chat: its mute is gone, and the core is told in one more row.
    assert_eq!(held(&store, &first, Field::Muted).await, Some((0, 1)));
    let mut patch = ConversationPatch::new(first.clone());
    patch.muted_until = Patch::Clear;
    let events = outbox(&store).await;
    assert_eq!(
        events.last(),
        Some(&BackendEvent::ConversationUpsert(patch)),
        "the reconcile's row follows the live ones"
    );
    assert_eq!(
        events.len(),
        4,
        "the replay of what the core has says nothing, the reset one thing"
    );
    // The second chat's mute was mentioned (the new generation), its archive lives in the other collection.
    assert_eq!(
        held(&store, &second, Field::Muted).await,
        Some((i64::MAX, 1))
    );
    assert_eq!(held(&store, &second, Field::Archived).await, Some((1, 0)));
    assert!(
        dirty(&store).await.is_empty(),
        "the collection is repaired and cleared"
    );
    owner.close();
}

// The address book's reconcile (F19, DUR-6) over the whole path. Contacts of the fictitious 555-06xx range.
const BOSS_PN: &str = "15550611201";
const ANN_PN: &str = "15550611202";

/// The phone's `contact` mutation naming the number `pn` with `name`, as the handler hands it to the writer.
fn contact_mutation(pn: &str, name: &str) -> Mapped {
    let mut action = waproto::whatsapp::sync_action_value::ContactAction::default();
    action.full_name = Some(name.to_owned());
    let change = crate::book::change_of_action(
        &format!("{pn}@s.whatsapp.net").parse().expect("jid"),
        &action,
    )
    .expect("a change");
    Mapped {
        collection: crate::appstate::map::CONTACT,
        mirror: Some(Mirror::Book(change)),
        events: Vec::new(),
        from_full_sync: false,
    }
}

/// F19: a contact deleted on the phone while Kourier was down is not in the snapshot the start-up resync asks for, and the
/// snapshot carries no REMOVE for it: the replay mentions Ann only, so Old Boss (generation 0) goes from the mirror and his
/// name is taken back from the core; Ann is stamped with the new generation (live rows take the collection's current one).
/// Kills "the address book is not reconciled" and "a live contact is not stamped with the collection's generation".
#[tokio::test]
async fn a_contact_the_replay_left_unmentioned_is_taken_back_from_the_core() {
    let (_dir, owner) = owner();
    let store = owner.store();
    for pn in [BOSS_PN, ANN_PN] {
        let id = UserId::from_parts("pn", pn).expect("a user");
        store
            .write("known", move |tx| {
                crate::store::book::know(tx, id.as_str()).map(drop)
            })
            .await
            .expect("know");
    }
    let live = writer(&store);
    for (pn, name) in [(BOSS_PN, "Old Boss"), (ANN_PN, "Ann")] {
        live.write_settings_and_wait(contact_mutation(pn, name))
            .expect("a live contact");
    }
    moved(&store, "critical_unblock_low", 5).await;
    assert_eq!(outbox(&store).await.len(), 2, "two names told live");

    let server = Server::new(&store, &["critical_unblock_low"]).replaying(
        "critical_unblock_low",
        vec![contact_mutation(ANN_PN, "Ann")],
    );
    let reasons = open(&store).await.unwrap();
    assert_eq!(reasons, ["critical_unblock_low"]);
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    let mut patch = ContactPatch::new(UserId::from_parts("pn", BOSS_PN).expect("a user"));
    patch.system_name = Patch::Clear;
    patch.identifiers = Some(Vec::new());
    patch.number_shown = Some(false);
    let events = outbox(&store).await;
    assert_eq!(
        events.last(),
        Some(&BackendEvent::ContactUpsert(patch)),
        "the reconcile's row follows the live ones"
    );
    assert_eq!(
        events.len(),
        3,
        "the replay of what the core has says nothing"
    );
    let book = store
        .read(|conn| {
            let mut ask = conn
                .prepare("SELECT contact, gen FROM address_book ORDER BY contact")
                .map_err(|_| crate::error::database("prepare"))?;
            let rows = ask
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|_| crate::error::database("query"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| crate::error::database("rows"))?;
            Ok(rows)
        })
        .await
        .expect("book");
    assert_eq!(book, [(format!("pn:{ANN_PN}"), 1)]);
    assert!(dirty(&store).await.is_empty());
    owner.close();
}

/// H1: a snapshot that came back synced but replayed NOTHING changes nothing in the mirror or the core (the report
/// cannot tell an empty collection from a request the server chose not to serve), though the mirror holds rows the
/// phone may have dropped. The flag still goes: the collection is read. Kills "reset on every synced report".
#[tokio::test]
async fn an_empty_replay_keeps_the_mirror() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]);
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    assert_eq!(server.asked().len(), 1);
    assert_eq!(
        held(&store, &first, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(
        held(&store, &second, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(outbox(&store).await.len(), 3, "the core is told nothing");
    assert!(dirty(&store).await.is_empty());
    owner.close();
}

/// A collection that lost an event during the replay (a zone failed: the connection ended) is not reconciled, however
/// much else the replay mentioned: the replay was not whole. Its flag stands. Kills "the lost set ignored".
#[tokio::test]
async fn a_replay_that_lost_an_event_resets_nothing_and_keeps_the_flag() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]).replaying(
        "regular_high",
        vec![mutation(&second, Field::Muted, i64::MAX, 20)],
    );
    server.loses.lock().unwrap().push("regular_high");
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    assert_eq!(
        held(&store, &first, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(outbox(&store).await.len(), 3);
    assert_eq!(dirty(&store).await, ["regular_high"]);
    owner.close();
}

/// A reconcile that could not commit (the link hold is closed, the wait ran out) leaves the mirror as it was AND the
/// flag and the owed mark standing: the next allowed start reads the collection again. Kills "a failed reconcile goes on
/// to clear".
#[tokio::test]
async fn a_reconcile_that_does_not_commit_keeps_the_flag_and_the_owed_mark() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]).replaying(
        "regular_high",
        vec![mutation(&second, Field::Muted, i64::MAX, 20)],
    );
    server.closes_the_hold.store(true, Ordering::SeqCst);
    let reasons = open(&store).await.unwrap();
    let held_back = writer(&store);
    held_back.set_patience(Duration::from_millis(50));
    run(
        &store,
        &held_back,
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    assert_eq!(
        held(&store, &first, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(outbox(&store).await.len(), 3);
    assert_eq!(dirty(&store).await, ["regular_high"]);
    assert_eq!(store.activity().owed(), ["regular_high"]);
    store.hold().set_closed(false);
    owner.close();
}

/// A snapshot UPSTREAM started itself (the link's full sync, a recovery of another collection) that is handled while ours
/// is out is not our replay: the count is per collection. Our snapshot of `regular_high` is answered EMPTY while
/// upstream replays `regular_low`; the mute that the empty answer left alone stays, and upstream's own replay is applied
/// as it comes (no reset for it either). Kills "one count for all collections".
#[tokio::test]
async fn a_replay_upstream_started_for_another_collection_is_not_our_replay() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]);
    server
        .alongside
        .lock()
        .unwrap()
        .push(mutation(&first, Field::Archived, 1, 30));
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    assert_eq!(
        held(&store, &first, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(
        held(&store, &second, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    // Upstream's replay was applied as any event is: the first chat is archived now, and nothing else was told.
    let mut patch = ConversationPatch::new(first.clone());
    patch.archived = Some(true);
    let events = outbox(&store).await;
    assert_eq!(events.len(), 4);
    assert_eq!(
        events.last(),
        Some(&BackendEvent::ConversationUpsert(patch))
    );
    assert_eq!(held(&store, &second, Field::Archived).await, Some((1, 0)));
    owner.close();
}

/// DUR-1: a PATCH of the collection written inside the window (an incremental sync that held upstream's reservation
/// when the task asked) is not the snapshot's replay: the snapshot comes back empty, so nothing is reset, though the
/// handler wrote a mapped mutation of that collection meanwhile. Kills "every mapped mutation counts" (the
/// `from_full_sync` flag ignored).
#[tokio::test]
async fn a_patch_inside_the_window_is_not_the_replay_and_an_empty_answer_resets_nothing() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let (first, second) = muted_world(&store).await;
    let server = Server::new(&store, &["regular_high"]);
    server
        .patches
        .lock()
        .unwrap()
        .push(mutation(&second, Field::Muted, i64::MAX, 20));
    let reasons = open(&store).await.unwrap();
    run(
        &store,
        &writer(&store),
        &server,
        &clock(Duration::ZERO),
        QUIET,
        EVENTS_WRITTEN,
        reasons,
    )
    .await;

    assert_eq!(server.asked().len(), 1);
    // The first chat's mute is NOT reset.
    assert_eq!(
        held(&store, &first, Field::Muted).await,
        Some((i64::MAX, 0))
    );
    assert_eq!(
        outbox(&store).await.len(),
        3,
        "the patch said nothing the core lacked, and no reset followed"
    );
    assert!(dirty(&store).await.is_empty(), "the collection is read");
    owner.close();
}
