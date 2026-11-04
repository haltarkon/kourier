//! The names the live path stated (W4 fix F3): which events are noted, and that a name is noted again at a newer time.
//! Every id is synthetic.

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_model::{ContactPatch, ConvKey, ConversationPatch, Patch, UserId};

use super::{Note, named, notes_of, prune, record, titled};
use crate::store::{KourierStore, StoreOwner};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([9; 32]),
        "wa-named",
    )
    .expect("open");
    (dir, owner)
}

fn group() -> ConvKey {
    ConvKey::Group("wag:120363000000000001".into())
}

fn user() -> UserId {
    UserId::from_parts("pn", "15550100101").expect("user")
}

fn title_event(conv: ConvKey, title: Patch<String>) -> BackendEvent {
    let mut patch = ConversationPatch::new(conv);
    patch.title = title;
    BackendEvent::ConversationUpsert(patch)
}

fn name_event(user: UserId, name: Patch<String>) -> BackendEvent {
    let mut patch = ContactPatch::new(user);
    patch.profile_name = name;
    BackendEvent::ContactUpsert(patch)
}

async fn noted(store: &KourierStore, notes: Vec<Note>, at: i64) {
    store
        .write("note", move |tx| record(tx, &notes, at))
        .await
        .expect("record");
}

/// Only a title or a push name that is SET is a name the live path stated: a timer change, a cleared title and an
/// untouched field are not.
#[test]
fn only_a_title_or_a_profile_name_that_is_set_is_noted() {
    let events = vec![
        title_event(group(), Patch::Set("Trip".into())),
        title_event(group(), Patch::Unchanged),
        title_event(group(), Patch::Clear),
        name_event(user(), Patch::Set("Alex Synthetic".into())),
        name_event(user(), Patch::Unchanged),
        BackendEvent::ConversationUpsert(ConversationPatch::new(group())),
    ];
    assert_eq!(notes_of(&events).len(), 2);
}

#[tokio::test]
async fn a_noted_title_and_a_noted_push_name_are_found_and_nothing_else_is() {
    let (_dir, owner) = open();
    let store = owner.store();
    let notes = notes_of(&[
        title_event(group(), Patch::Set("Trip".into())),
        name_event(user(), Patch::Set("Alex Synthetic".into())),
    ]);
    noted(&store, notes, 1_000).await;
    let (g, u) = (group().encode(), user().as_str().to_owned());
    let other = ConvKey::Group("wag:120363000000000002".into()).encode();
    let found = store
        .read(move |conn| {
            Ok((
                titled(conn, &g)?,
                titled(conn, &other)?,
                // A user's name is not a title, and a title's key is not a user.
                titled(conn, &u)?,
                named(conn, &[user().as_str(), "pn:15550100102"])?,
            ))
        })
        .await
        .expect("look");
    assert!(found.0, "the noted title");
    assert!(!found.1, "another group's title is not noted");
    assert!(!found.2, "a user is no conversation");
    assert_eq!(
        found.3.into_iter().collect::<Vec<_>>(),
        [user().as_str().to_owned()],
        "only the person whose name was stated"
    );
    owner.close();
}

#[tokio::test]
async fn a_name_noted_again_moves_to_the_newer_time_and_old_notes_are_forgotten() {
    let (_dir, owner) = open();
    let store = owner.store();
    let notes = notes_of(&[title_event(group(), Patch::Set("Trip".into()))]);
    noted(&store, notes.clone(), 1_000).await;
    noted(&store, notes, 5_000).await;
    let gone = store
        .write("prune", |tx| prune(tx, 3_000))
        .await
        .expect("prune");
    assert_eq!(
        gone, 0,
        "the second note is at 5000: nothing is older than 3000"
    );
    let gone = store
        .write("prune", |tx| prune(tx, 6_000))
        .await
        .expect("prune");
    assert_eq!(gone, 1);
    owner.close();
}
