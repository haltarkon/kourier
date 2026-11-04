//! The blocklist read (W6a2, B3, D22): who counts as a person, what the zone states, and when the actor's side asks.
//! Synthetic values only: the tails are 8 or more digits and a substring of no other value here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_model::{Patch, UserId};
use wacore_binary::Jid;

use super::{Counts, Outcome, REREAD_MS, Reader, due, people, state};
use crate::map::OwnIds;
use crate::store::{KourierStore, StoreOwner};

const PN_A: &str = "15550170411";
const PN_B: &str = "15550170422";
const LID_C: &str = "99900170433";
const OWN_PN: &str = "15550170455";
const OWN_LID: &str = "99900170466";
/// The time of the zone in these tests (ms).
const NOW_MS: i64 = 1_800_000_000_000;

fn jid(text: &str) -> Jid {
    text.parse().expect("a jid")
}

fn pn(digits: &str) -> UserId {
    UserId::from_parts("pn", digits).expect("a user")
}

fn lid(digits: &str) -> UserId {
    UserId::from_parts("lid", digits).expect("a user")
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(lid(OWN_LID)),
        pn: Some(pn(OWN_PN)),
    }
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([15; 32]),
        "wa-blocklist",
    )
    .expect("open");
    (dir, owner)
}

/// The statement of `listed`, with the books moved: what a zone of the event writer does.
async fn stated(store: &KourierStore, listed: Vec<UserId>) -> (Vec<(UserId, bool)>, Counts) {
    let (events, counts) = store
        .write("test: state the blocklist", move |tx| {
            state(tx, &listed, NOW_MS)
        })
        .await
        .expect("state");
    let said = events
        .into_iter()
        .map(|event| match event {
            BackendEvent::ContactUpsert(patch) => {
                // Only `blocked` is said: no name, no avatar, nothing else.
                assert!(
                    patch.profile_name == Patch::Unchanged && patch.system_name == Patch::Unchanged,
                    "the blocklist states nothing but the flag"
                );
                (patch.user, patch.blocked.expect("the flag"))
            }
            other => panic!("the blocklist stated {other:?}"),
        })
        .collect();
    (said, counts)
}

/// D22: the people a list names are numbers and LIDs, each once, in id order; our own ids and everything that is no
/// person (a group, the status feed, a broadcast list) are left out. Kills "own ids kept" and "a duplicate kept".
#[test]
fn a_list_names_each_person_once_and_leaves_out_our_ids_and_what_is_no_person() {
    let listed = [
        jid(&format!("{PN_B}@s.whatsapp.net")),
        jid(&format!("{LID_C}@lid")),
        jid(&format!("{PN_A}@s.whatsapp.net")),
        // The same person again, as a device of his.
        jid(&format!("{PN_B}:3@s.whatsapp.net")),
        jid(&format!("{OWN_PN}@s.whatsapp.net")),
        jid(&format!("{OWN_LID}@lid")),
        jid("120363000071529611@g.us"),
        jid("status@broadcast"),
    ];
    assert_eq!(
        people(&listed, &own()),
        [lid(LID_C), pn(PN_A), pn(PN_B)],
        "ordered by id, once each"
    );
    assert_eq!(people(&[], &own()), Vec::<UserId>::new());
}

/// D22: the first list states each person blocked; the same list again states nothing; a person who is gone from the list
/// is stated unblocked, and the unblock comes BEFORE any new block. Kills "no unblock", "blocks first" and "a repeat
/// stated again".
#[tokio::test]
async fn a_person_is_stated_blocked_then_unblocked_and_a_repeat_states_nothing() {
    let (_dir, owner) = open();
    let store = owner.store();

    let (first, counts) = stated(&store, vec![pn(PN_A), pn(PN_B)]).await;
    assert_eq!(first, [(pn(PN_A), true), (pn(PN_B), true)]);
    assert_eq!(
        counts,
        Counts {
            blocked: 2,
            unblocked: 0
        }
    );

    let (again, counts) = stated(&store, vec![pn(PN_A), pn(PN_B)]).await;
    assert!(again.is_empty(), "nothing changed: {again:?}");
    assert_eq!(counts, Counts::default());

    // PN_A was unblocked on the phone and LID_C blocked: the unblock is said first.
    let (changed, counts) = stated(&store, vec![lid(LID_C), pn(PN_B)]).await;
    assert_eq!(changed, [(pn(PN_A), false), (lid(LID_C), true)]);
    assert_eq!(
        counts,
        Counts {
            blocked: 1,
            unblocked: 1
        }
    );

    // The list empties: everybody left is unblocked.
    let (cleared, _) = stated(&store, Vec::new()).await;
    assert_eq!(cleared, [(lid(LID_C), false), (pn(PN_B), false)]);
    let (nothing, _) = stated(&store, Vec::new()).await;
    assert!(nothing.is_empty());
}

/// The actor's side: a read that came back `Later` is asked again at the next `Online`, one that was stated or refused
/// is not, and a task still running is never doubled. Kills "`Later` counts as done", "`Refused` counts as not done" and
/// "a second task while one runs".
#[tokio::test]
async fn a_read_that_came_back_later_is_asked_again_and_a_stated_or_refused_one_is_not() {
    for (first, asked_again) in [
        (Outcome::Later, true),
        // The stamp said less than a day: nothing was asked, and a reconnect looks at the stamp again (Q1).
        (Outcome::Fresh, true),
        (Outcome::Stated, false),
        (Outcome::Refused, false),
    ] {
        let asked = Arc::new(AtomicUsize::new(0));
        let mut reader = Reader::default();
        for _ in 0..3 {
            let asked = asked.clone();
            reader
                .start(move || async move {
                    let n = asked.fetch_add(1, Ordering::SeqCst);
                    // Only the first run has the outcome under test; a later one that happens is stated.
                    if n == 0 { first } else { Outcome::Stated }
                })
                .await;
            // Let the task end before the next `Online`.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
        }
        let want = if asked_again { 2 } else { 1 };
        assert_eq!(asked.load(Ordering::SeqCst), want, "after {first:?}");
    }

    // A task that has not ended is not doubled by the next `Online`.
    let asked = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut reader = Reader::default();
    for _ in 0..3 {
        let (asked, gate) = (asked.clone(), gate.clone());
        reader
            .start(move || async move {
                asked.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
                Outcome::Stated
            })
            .await;
        tokio::task::yield_now().await;
    }
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    reader.abort().await;
}

/// Fix F7a (PROTOCOL-4): a read that FINISHED keeps its outcome when the client is released: a refused list is not asked
/// again by the next client of the same start (a rebuild after a failed handler write), a stated one neither, while one
/// that came back `Later` is. Kills "abort drops a finished task's outcome" (the old form: `Refused` asked again).
#[tokio::test]
async fn a_finished_read_keeps_its_outcome_when_the_client_is_released() {
    for (first, asked_again) in [
        (Outcome::Refused, false),
        (Outcome::Stated, false),
        (Outcome::Later, true),
    ] {
        let asked = Arc::new(AtomicUsize::new(0));
        let mut reader = Reader::default();
        for round in 0..2 {
            let asked = asked.clone();
            reader
                .start(move || async move {
                    let n = asked.fetch_add(1, Ordering::SeqCst);
                    if n == 0 { first } else { Outcome::Stated }
                })
                .await;
            // The task ends, then the client is released (the actor's `stop_client`), then the next `Online`.
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            if round == 0 {
                reader.abort().await;
            }
        }
        let want = if asked_again { 2 } else { 1 };
        assert_eq!(asked.load(Ordering::SeqCst), want, "after {first:?}");
    }
}

/// Q1: a read is due when nothing was ever stated, when the stamp is a day old, or when it lies in the future (a clock set
/// back); a stamp one millisecond younger than a day is not. Kills "the day is 23 hours", "an exact day is not due", "a
/// future stamp silences the list" and "no stamp is not due".
#[test]
fn a_read_is_due_after_a_day_or_when_the_stamp_is_not_credible() {
    assert!(due(None, NOW_MS), "never stated");
    assert!(
        !due(Some(NOW_MS), NOW_MS),
        "stated this very moment: not due"
    );
    assert!(
        !due(Some(NOW_MS - REREAD_MS + 1), NOW_MS),
        "a ms short of a day"
    );
    assert!(due(Some(NOW_MS - REREAD_MS), NOW_MS), "a day exactly");
    assert!(due(Some(NOW_MS + 1), NOW_MS), "a stamp from the future");
    assert_eq!(REREAD_MS, 86_400_000);
}

/// Q1: the zone that states an answer stamps it, also when the list changed nothing; the next answer moves the stamp.
/// Kills "the stamp only when something changed" and "no stamp".
#[tokio::test]
async fn the_zone_that_states_an_answer_stamps_it_even_when_nothing_changed() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert_eq!(
        store
            .read(crate::store::people::blocklist_read_at)
            .await
            .expect("read"),
        None
    );
    let (said, _) = stated(&store, Vec::new()).await;
    assert!(said.is_empty(), "an empty list says nothing");
    assert_eq!(
        store
            .read(crate::store::people::blocklist_read_at)
            .await
            .expect("read"),
        Some(NOW_MS),
        "stamped although nothing changed"
    );
}

/// Q1: a link (the zone that states `AccountReady` for it) deletes the stamp, so a new account's first `Online` asks the
/// list whatever an earlier account of this file stamped. Kills "the link keeps the stamp".
#[tokio::test]
async fn a_link_forgets_the_stamp_so_its_first_online_asks() {
    let (_dir, owner) = open();
    let store = owner.store();
    stated(&store, vec![pn(PN_A)]).await;
    assert_eq!(
        store
            .read(crate::store::people::blocklist_read_at)
            .await
            .expect("read"),
        Some(NOW_MS)
    );
    let ready = crate::link::Ready::of(pn(OWN_PN), Some(lid(OWN_LID)), 3);
    crate::outbox::append_ready(&store, &ready, NOW_MS + 5)
        .await
        .expect("append");
    assert_eq!(
        store
            .read(crate::store::people::blocklist_read_at)
            .await
            .expect("read"),
        None,
        "the link left the stamp"
    );
    // What the core was told stays: the link is not a Rebuild (the rows of a new account are not this step's business).
}
