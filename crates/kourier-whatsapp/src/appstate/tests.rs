//! Rule (b)'s two clearing points over a real store (`backend.db` in a temp dir) and a double of the
//! client. Collection names are upstream's wire names; nothing here is a person's.

use std::sync::Arc;
use std::time::Duration;

use kourier_db::DbKey;
use wacore::appstate::hash::HashState;
use wacore::store::traits::AppSyncStore;

use super::{Outcome, Quiet, Why, quiet_for, stop_quietly, stop_without_look};
use crate::session::fake::{Call, FakeSession};
use crate::store::{KourierStore, StoreOwner};

/// The five collections of upstream, spelled out: the real list ([`super::EVENTS_WRITTEN`]) is pinned against it.
const ALL: &[&str] = &[
    "critical_block",
    "critical_unblock_low",
    "regular_high",
    "regular_low",
    "regular",
];

const QUIET: Quiet = Quiet {
    window: Duration::from_millis(30),
    bound: Duration::from_millis(150),
};

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

fn state(version: u64) -> HashState {
    HashState {
        version,
        ..HashState::default()
    }
}

async fn dirty(store: &KourierStore) -> Vec<(String, u64)> {
    store.dirty_collections().await.expect("dirty")
}

fn names(dirty: &[(String, u64)]) -> Vec<&str> {
    dirty.iter().map(|(name, _)| name.as_str()).collect()
}

/// The clear is one job in the writer's order: a version written after the quiet look keeps its
/// flag, and the job reads the stored version itself. Kills "the clear without the check".
#[tokio::test]
async fn a_version_written_between_the_look_and_the_clear_keeps_its_flag() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    let seen = store.activity().writes();
    // A sync lands after the look.
    store
        .commit_patch("regular", state(5), &[], &[])
        .await
        .unwrap();
    assert_eq!(store.clear_quiet(seen, None).await.unwrap(), None);
    assert_eq!(dirty(&store).await, [("regular".to_owned(), 5)]);

    // A look taken after it clears the flag of the version that stands.
    let seen = store.activity().writes();
    assert_eq!(store.clear_quiet(seen, None).await.unwrap(), Some(1));
    assert!(dirty(&store).await.is_empty());
    owner.close();
}

/// A collection that waits for its snapshot (`delete_version`: dirty, no stored version) keeps its
/// flag whatever else is cleared, and `only` limits the clear to the collections named.
#[tokio::test]
async fn a_collection_waiting_for_its_snapshot_keeps_its_flag() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    store.set_version("regular_low", state(2)).await.unwrap();
    store.set_version("critical_block", state(7)).await.unwrap();
    store.delete_version("critical_block").await.unwrap();

    let seen = store.activity().writes();
    let only = Some(vec!["regular".to_owned(), "critical_block".to_owned()]);
    assert_eq!(store.clear_quiet(seen, only).await.unwrap(), Some(1));
    assert_eq!(
        names(&dirty(&store).await),
        ["critical_block", "regular_low"],
        "the unsynced one and the one not named stay"
    );
    owner.close();
}

/// Quiet is a window without a version write or an event; a connection that never stops moving
/// does not get one within the bound.
///
/// On the PAUSED clock (W6a1 A8, strike 1 of a load flake): the noise task and the quiet look measure the SAME virtual
/// time, so every window of the look holds six of the noise's events by construction. On the wall clock the noise's
/// 5 ms sleeps could stall for a whole 30 ms window on a loaded machine (one such run saw the events stand still and
/// got a quiet look), and no margin of the window can be made safe against a scheduler: the test now waits for what
/// it checks (the events the windows must see), not for a stretch of real time.
#[tokio::test(start_paused = true)]
async fn a_connection_that_never_goes_quiet_gets_no_quiet_look() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let seen = quiet_for(&store, QUIET)
        .await
        .expect("an idle store is quiet");
    assert_eq!(seen, store.activity().sample());

    let busy = store.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let noise = tokio::spawn(async move {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            busy.activity().event();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    assert_eq!(quiet_for(&store, QUIET).await, None);
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    noise.await.unwrap();
    owner.close();
}

/// The stop point: pause, look, stop, look again, clear. A quiet stop leaves nothing dirty; a pause
/// already made by `Disconnect` is not made twice.
#[tokio::test]
async fn a_quiet_stop_leaves_nothing_dirty() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    store.set_version("critical_block", state(7)).await.unwrap();

    let session = FakeSession::new();
    let (released, outcome) = stop_quietly(&store, &session, QUIET, false, ALL).await;
    assert_eq!(released, crate::session::Released::Yes);
    assert_eq!(outcome, Outcome::Cleared(2));
    assert_eq!(session.calls(), [Call::Pause, Call::Stop]);
    assert!(dirty(&store).await.is_empty());

    store.set_version("regular", state(5)).await.unwrap();
    let paused = FakeSession::new();
    let (_, outcome) = stop_quietly(&store, &paused, QUIET, true, ALL).await;
    assert_eq!(outcome, Outcome::Cleared(1));
    assert_eq!(paused.calls(), [Call::Stop], "already paused");
    owner.close();
}

/// Traffic that outlives the pause (a task of the client that is still writing when the stop runs)
/// keeps the flags: the second look after the release sees it. Kills "the flags go at an unquiet
/// stop" through the second look.
#[tokio::test]
async fn traffic_during_the_stop_keeps_the_flags() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();

    let session = FakeSession::new();
    let activity = store.activity().clone();
    session.on_stop(move || activity.event());
    let (_, outcome) = stop_quietly(&store, &session, QUIET, false, ALL).await;
    assert_eq!(outcome, Outcome::Kept(Why::Busy));
    assert_eq!(names(&dirty(&store).await), ["regular"]);
    owner.close();
}

/// A connection that stays busy for the whole bound is stopped all the same, and keeps its flags:
/// the next start rebuilds those collections. Kills "the flags go without a quiet look".
#[tokio::test]
async fn a_busy_connection_is_stopped_and_keeps_the_flags() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();

    let busy = store.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let noise = tokio::spawn(async move {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            busy.activity().event();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    let session = FakeSession::new();
    let (_, outcome) = stop_quietly(&store, &session, QUIET, false, ALL).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    noise.await.unwrap();
    assert_eq!(outcome, Outcome::Kept(Why::Busy));
    assert_eq!(session.calls(), [Call::Pause, Call::Stop], "it is stopped");
    assert_eq!(names(&dirty(&store).await), ["regular"]);
    owner.close();
}

/// W6a1 (D4): the real list names all five collections of upstream, so a clean quit after an app-state sync clears
/// what the session's sync set. Kills "the list stays empty" (the flags stand) and "the list names fewer".
#[tokio::test]
async fn the_real_list_names_all_five_collections_and_a_quiet_quit_clears_what_the_session_set() {
    assert_eq!(super::EVENTS_WRITTEN, ALL, "all five of upstream's");
    let (_dir, owner) = owner();
    let store = owner.store();
    for (name, version) in [
        ("regular", 4),
        ("critical_block", 7),
        ("critical_unblock_low", 1),
        ("regular_high", 2),
        ("regular_low", 3),
    ] {
        store.set_version(name, state(version)).await.unwrap();
    }

    let session = FakeSession::new();
    let (released, outcome) =
        stop_quietly(&store, &session, QUIET, false, super::EVENTS_WRITTEN).await;
    assert_eq!(released, crate::session::Released::Yes);
    assert_eq!(outcome, Outcome::Cleared(5));
    assert_eq!(session.calls(), [Call::Pause, Call::Stop]);
    assert!(dirty(&store).await.is_empty());
    owner.close();
}

/// Critic B1: a collection that was dirty when the store opened is OWED a resync, and its flag stands at a quiet
/// stop until the resync came back synced; the others are cleared. Kills "a clearing point ignores the owed set"
/// (the quit would vouch for events the mirror never got).
#[tokio::test]
async fn a_collection_owed_a_resync_keeps_its_flag_at_a_quiet_stop_until_it_is_repaired() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    store.set_version("critical_block", state(7)).await.unwrap();
    store.activity().owe(&["regular".to_owned()]);

    let session = FakeSession::new();
    let (_, outcome) = stop_quietly(&store, &session, QUIET, false, ALL).await;
    assert_eq!(outcome, Outcome::Cleared(1), "only the one nothing is owed");
    assert_eq!(names(&dirty(&store).await), ["regular"]);

    // Everything the list names is owed: nothing to clear.
    let (_, outcome) = stop_quietly(&store, &FakeSession::new(), QUIET, false, &["regular"]).await;
    assert_eq!(outcome, Outcome::Kept(Why::Lost));
    assert_eq!(names(&dirty(&store).await), ["regular"]);

    // Its resync came back synced: the flag may go.
    store.activity().repaired("regular");
    let (_, outcome) = stop_quietly(&store, &FakeSession::new(), QUIET, false, ALL).await;
    assert_eq!(outcome, Outcome::Cleared(1));
    assert!(dirty(&store).await.is_empty());
    owner.close();
}

/// A list with some collections clears those and only those at the stop.
#[tokio::test]
async fn only_the_listed_collections_are_cleared() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    store.set_version("critical_block", state(7)).await.unwrap();

    let session = FakeSession::new();
    let (_, outcome) = stop_quietly(&store, &session, QUIET, false, &["regular"]).await;
    assert_eq!(outcome, Outcome::Cleared(1));
    assert_eq!(names(&dirty(&store).await), ["critical_block"]);

    // An empty list clears nothing and does not wait for a quiet look.
    let (_, outcome) = stop_quietly(&store, &FakeSession::new(), QUIET, false, &[]).await;
    assert_eq!(outcome, Outcome::Kept(Why::NotWritten));
    assert_eq!(names(&dirty(&store).await), ["critical_block"]);
    owner.close();
}

/// H1: only a snapshot of OURS makes the handler's writes a replay. A mutation written while no snapshot is out (a live
/// sync, the link's full sync, a recovery upstream started) counts for nothing, a window starts at zero, counts only the
/// collection it was opened for, and closes. Kills "counts without a window" and "a window keeps an earlier count".
#[tokio::test]
async fn only_what_is_written_while_a_snapshot_of_ours_is_out_is_its_replay() {
    let (_dir, owner) = owner();
    let activity = owner.store().activity().clone();
    activity.replayed("regular_low");
    assert_eq!(activity.replay_close("regular_low"), 0, "no snapshot out");

    activity.replay_open("regular_low");
    activity.replayed("regular_low");
    activity.replayed("regular_low");
    activity.replayed("regular_high");
    assert_eq!(activity.replay_close("regular_low"), 2);
    assert_eq!(activity.replay_close("regular_low"), 0, "closed");

    activity.replay_open("regular_low");
    activity.replayed("regular_low");
    activity.replay_open("regular_low");
    assert_eq!(
        activity.replay_close("regular_low"),
        0,
        "a new ask starts a new window"
    );
    owner.close();
}

/// G6 (DUR-7's residual): a look sleeps one window before it can judge anything, so a quit with less than one window
/// of its grace left (before the store's release) has no look at all; with more it gets the standard look held to what
/// is left. Kills "a look whatever is left" and a bound that forgets the first window.
#[test]
fn a_quit_with_less_than_one_window_left_has_no_look() {
    let window = Quiet::STANDARD.window;
    assert_eq!(Quiet::within(Duration::ZERO).map(|q| q.bound), None);
    assert_eq!(
        Quiet::within(window - Duration::from_millis(1)).map(|q| q.bound),
        None,
        "one millisecond short of a window"
    );
    assert_eq!(
        Quiet::within(window).map(|q| (q.window, q.bound)),
        Some((window, Duration::ZERO)),
        "exactly one window: the first sleep fits, no second"
    );
    let bound = Quiet::STANDARD.bound;
    assert_eq!(
        Quiet::within(window + Duration::from_millis(10)).map(|q| q.bound),
        Some(Duration::from_millis(10)),
        "the bound is what is left after the first window"
    );
    assert_eq!(
        Quiet::within(window + bound + Duration::from_secs(60)).map(|q| q.bound),
        Some(bound),
        "plenty left: the standard bound"
    );
}

/// G6: with no time to look the client is paused and stopped and nothing is cleared, whatever the store holds.
#[tokio::test]
async fn a_stop_without_a_look_clears_nothing() {
    let (_dir, owner) = owner();
    let store = owner.store();
    store.set_version("regular", state(4)).await.unwrap();
    let session = FakeSession::new();
    let (_, outcome) = stop_without_look(&session, false).await;
    assert_eq!(outcome, Outcome::Kept(Why::NoTime));
    assert_eq!(session.calls(), [Call::Pause, Call::Stop]);
    assert_eq!(names(&dirty(&store).await), ["regular"]);
    owner.close();
}
