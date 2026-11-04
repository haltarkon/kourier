//! The push-name rule and the zone's "on change only" (W6a2, B8a; D8). Every name and id is synthetic.

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_model::{ContactPatch, Patch, UserId};

use super::{NAME_CAP, clean, keep_changed, push_name_event, state_push_name};
use crate::store::{KourierStore, StoreOwner};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([15; 32]),
        "wa-names",
    )
    .expect("open");
    (dir, owner)
}

fn ann() -> UserId {
    UserId::from_parts("pn", "15550180101").expect("an id")
}

fn merged() -> BackendEvent {
    BackendEvent::IdentityMerged {
        from: UserId::from_parts("pn", "15550180102").expect("an id"),
        into: UserId::from_parts("lid", "99900180102").expect("an id"),
    }
}

fn line() -> BackendEvent {
    BackendEvent::ContactUpsert(ContactPatch::new(
        UserId::from_parts("pn", "15550180103").expect("an id"),
    ))
}

#[test]
fn a_push_name_is_trimmed_and_the_placeholder_and_blanks_say_no_name() {
    assert_eq!(
        clean("  Ann Example \n").as_deref(),
        Some("Ann Example"),
        "trimmed"
    );
    for none in ["", "   ", "\n\t", "-", " - ", "\u{a0}"] {
        assert_eq!(clean(none), None, "{none:?} is no name");
    }
    // Only the whole placeholder is one: a name that holds a dash stays.
    assert_eq!(clean("-Ann").as_deref(), Some("-Ann"));
    assert_eq!(clean("--").as_deref(), Some("--"));
}

#[test]
fn a_push_name_is_cut_by_characters_never_in_the_middle_of_one() {
    let long = "\u{e9}".repeat(NAME_CAP + 40);
    let cut = clean(&long).expect("a name");
    assert_eq!(cut.chars().count(), NAME_CAP);
    assert_eq!(
        clean(&"n".repeat(NAME_CAP)).expect("a name").len(),
        NAME_CAP
    );
}

async fn state(
    store: &KourierStore,
    events: Vec<BackendEvent>,
    name: &'static str,
) -> (bool, Vec<BackendEvent>) {
    store
        .write("test: state a name", move |tx| {
            let mut events = events;
            let added = state_push_name(tx, &mut events, &ann(), name)?;
            Ok((added, events))
        })
        .await
        .expect("state")
}

#[tokio::test]
async fn a_name_is_stated_when_it_changes_and_not_again_and_goes_after_the_merges() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (added, events) = state(&store, vec![merged(), line()], "Ann Example").await;
    assert!(added, "the first name is stated");
    assert_eq!(
        events,
        vec![
            merged(),
            push_name_event(ann(), "Ann Example".to_owned()),
            line()
        ],
        "after the merge, in front of the rest"
    );
    let (added, events) = state(&store, vec![line()], "Ann Example").await;
    assert!(!added, "the same name is told once");
    assert_eq!(events, vec![line()], "nothing was added");
    let (added, _) = state(&store, vec![line()], "Ann Renamed").await;
    assert!(added, "a changed name is stated");
    let (added, _) = state(&store, vec![line()], "Ann Example").await;
    assert!(added, "and a name that came back is a change again");
    let (added, events) = state(&store, Vec::new(), "Ann Other").await;
    assert!(added);
    assert_eq!(events, vec![push_name_event(ann(), "Ann Other".to_owned())]);
    owner.close();
}

#[tokio::test]
async fn keep_changed_drops_a_name_the_core_has_and_touches_nothing_else() {
    let (_dir, owner) = open();
    let store = owner.store();
    let named = |name: &str| push_name_event(ann(), name.to_owned());
    let mut other = ContactPatch::new(ann());
    other.profile_name = Patch::Unchanged;
    let mixed = vec![
        merged(),
        named("Ann Example"),
        BackendEvent::ContactUpsert(other),
    ];
    let kept = store
        .write("test: keep", {
            let mixed = mixed.clone();
            move |tx| keep_changed(tx, mixed)
        })
        .await
        .expect("keep");
    assert_eq!(kept, mixed, "the first time everything stays");
    let kept = store
        .write("test: keep again", move |tx| keep_changed(tx, mixed))
        .await
        .expect("keep");
    assert_eq!(
        kept.len(),
        2,
        "the told name goes, the merge and the contact with no name stay: {kept:?}"
    );
    assert!(!kept.contains(&named("Ann Example")));
    owner.close();
}
