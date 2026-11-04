//! The people a history chunk named that the core does not know (W6a2 fix F11, PRIV-5): kept, and stated once where the person
//! first becomes known. Every id and name is synthetic.

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_model::{ContactPatch, Patch, UserId};

use super::{release, scope};
use crate::book::states;
use crate::history::named;
use crate::store::people::{self, NameKind};
use crate::store::{KourierStore, StoreOwner, book, exec};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([19; 32]),
        "wa-strangers",
    )
    .expect("open");
    (dir, owner)
}

const A_PN: &str = "15550491201";
const B_PN: &str = "15550491202";
const B_LID: &str = "99900491202";
const C_PN: &str = "15550491203";
const D_PN: &str = "15550491204";
const E_PN: &str = "15550491205";
const E_LID: &str = "99900491205";
const Q_LID: &str = "99900491299";

fn pn(value: &str) -> UserId {
    UserId::from_parts("pn", value).expect("a number")
}

fn lid(value: &str) -> UserId {
    UserId::from_parts("lid", value).expect("a lid")
}

fn merged(number: &str, id: &str) -> BackendEvent {
    BackendEvent::IdentityMerged {
        from: pn(number),
        into: lid(id),
    }
}

fn push_name(user: UserId, name: &str) -> BackendEvent {
    crate::names::push_name_event(user, name.to_owned())
}

async fn know(store: &KourierStore, who: UserId) {
    store
        .write("test: know", move |tx| {
            book::know(tx, who.as_str()).map(drop)
        })
        .await
        .expect("know");
}

async fn do_scope(store: &KourierStore, events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    store
        .write("test: scope", move |tx| scope(tx, events))
        .await
        .expect("scope")
}

async fn appear(store: &KourierStore, who: Vec<UserId>) -> Vec<BackendEvent> {
    store
        .write("test: appear", move |tx| states(tx, &who))
        .await
        .expect("states")
}

/// `(kept pairs, kept names, stated pairs)`.
async fn counts(store: &KourierStore) -> (i64, i64, i64) {
    store
        .read(|conn| {
            let one = |table: &str| -> Result<i64, wacore::store::error::StoreError> {
                crate::store::first(
                    conn,
                    "count",
                    &format!("SELECT count(*) FROM {table}"),
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(Option::unwrap_or_default)
            };
            Ok((
                one("kept_pairs")?,
                one("kept_names")?,
                one("merges_stated")?,
            ))
        })
        .await
        .expect("counts")
}

fn name_of(event: &BackendEvent) -> (&str, &str) {
    match event {
        BackendEvent::ContactUpsert(ContactPatch {
            user,
            profile_name: Patch::Set(name),
            ..
        }) => (user.as_str(), name.as_str()),
        _ => panic!("a push name"),
    }
}

#[tokio::test]
async fn a_chunk_states_its_people_for_the_people_the_core_knows_and_keeps_the_rest() {
    let (_dir, owner) = open();
    let store = owner.store();
    know(&store, pn(A_PN)).await;
    know(&store, pn(C_PN)).await;
    let a_pair = merged(A_PN, "99900491201");
    let kept_pair = merged(B_PN, B_LID);
    // The review's chunk: two pairs, two names, and a contact upsert of another kind that is none of the scope's business.
    let mut other = ContactPatch::new(pn(D_PN));
    other.system_name = Patch::Set("Dee Book".to_owned());
    let stated = do_scope(
        &store,
        vec![
            a_pair.clone(),
            kept_pair,
            push_name(pn(C_PN), "Cy Known"),
            push_name(pn(D_PN), "Dee Stranger"),
            BackendEvent::ContactUpsert(other.clone()),
        ],
    )
    .await;
    assert_eq!(
        stated,
        [
            a_pair,
            push_name(pn(C_PN), "Cy Known"),
            BackendEvent::ContactUpsert(other)
        ],
        "the pair of a known person, the name of a known person and the other kind: stated, in order"
    );
    assert_eq!(
        counts(&store).await,
        (1, 1, 0),
        "the stranger's pair and his name are kept, none is recorded as told"
    );
    owner.close();
}

#[tokio::test]
async fn a_kept_pair_and_name_are_stated_once_where_the_person_first_becomes_known() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The chunk named Bea (a pair and a name), Cal (a name) and Eli (a pair).
    do_scope(
        &store,
        vec![
            merged(B_PN, B_LID),
            merged(E_PN, E_LID),
            push_name(pn(B_PN), "Bea Synthetic"),
            push_name(pn(C_PN), "Cal Synthetic"),
        ],
    )
    .await;
    assert_eq!(counts(&store).await, (2, 2, 0));

    // Cal and Bea appear in one zone: Bea's merge first, then the names; Eli waits.
    let events = appear(&store, vec![pn(C_PN), pn(B_PN)]).await;
    assert_eq!(events.len(), 3, "one merge and two names: {events:?}");
    assert_eq!(events[0], merged(B_PN, B_LID), "the merge goes first");
    assert_eq!(
        (name_of(&events[1]), name_of(&events[2])),
        (
            (pn(C_PN).as_str(), "Cal Synthetic"),
            (pn(B_PN).as_str(), "Bea Synthetic")
        )
    );
    assert_eq!(
        counts(&store).await,
        (1, 0, 1),
        "Eli's pair stays kept, the others are forgotten from the kept tables, Bea's pair is recorded as told"
    );
    // Said once: the same people again say nothing.
    assert!(appear(&store, vec![pn(C_PN), pn(B_PN)]).await.is_empty());
    // And the name is recorded as told, so a live message with the same name states nothing.
    let told = store
        .read(|conn| people::name_of(conn, pn(B_PN).as_str(), NameKind::Profile))
        .await
        .expect("read");
    assert_eq!(told.as_deref(), Some("Bea Synthetic"));

    // Eli appears by the LID side of his pair: the merge is released too.
    let events = appear(&store, vec![lid(E_LID)]).await;
    assert_eq!(events, [merged(E_PN, E_LID)]);
    assert_eq!(counts(&store).await, (0, 0, 2));
    owner.close();
}

#[tokio::test]
async fn a_kept_name_older_than_one_the_core_was_told_or_the_live_path_stated_is_not_stated() {
    let (_dir, owner) = open();
    let store = owner.store();
    do_scope(
        &store,
        vec![
            push_name(pn(B_PN), "Bea Chunk"),
            push_name(pn(C_PN), "Cal Chunk"),
        ],
    )
    .await;
    // Bea's live name was stated since the chunk (the core was told it); Cal's was noted by the live path for the history.
    store
        .write("test: live names", |tx| {
            people::put_name(tx, pn(B_PN).as_str(), NameKind::Profile, "Bea Live")?;
            named::record(
                tx,
                &named::notes_of(&[push_name(pn(C_PN), "Cal Live")]),
                1_000,
            )
        })
        .await
        .expect("live names");
    assert!(
        appear(&store, vec![pn(B_PN), pn(C_PN)]).await.is_empty(),
        "the live names stand: the chunk's are older"
    );
    assert_eq!(counts(&store).await, (0, 0, 0), "and they are forgotten");
    let told = store
        .read(|conn| people::name_of(conn, pn(B_PN).as_str(), NameKind::Profile))
        .await
        .expect("read");
    assert_eq!(told.as_deref(), Some("Bea Live"));
    owner.close();
}

#[tokio::test]
async fn a_kept_pair_that_would_cross_a_pair_the_core_was_told_is_never_stated() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The chunk paired Bea's number with one LID; a message had merged it into another before the person appeared.
    do_scope(&store, vec![merged(B_PN, B_LID)]).await;
    store
        .write("test: stated", |tx| {
            exec(
                tx,
                "test",
                "INSERT INTO merges_stated(pn, lid) VALUES (?1, ?2)",
                kourier_db::rusqlite::params![pn(B_PN).as_str(), lid(Q_LID).as_str()],
            )
            .map(drop)
        })
        .await
        .expect("stated");
    assert!(
        appear(&store, vec![pn(B_PN)]).await.is_empty(),
        "a merge is final: a second pair around the number is not stated"
    );
    assert_eq!(counts(&store).await, (0, 0, 1), "the kept pair is let go");
    owner.close();
}

#[tokio::test]
async fn a_chunks_pair_that_crosses_a_pair_the_core_was_told_is_dropped_even_for_a_known_person() {
    let (_dir, owner) = open();
    let store = owner.store();
    know(&store, pn(B_PN)).await;
    store
        .write("test: stated", |tx| {
            exec(
                tx,
                "test",
                "INSERT INTO merges_stated(pn, lid) VALUES (?1, ?2)",
                kourier_db::rusqlite::params![pn(B_PN).as_str(), lid(Q_LID).as_str()],
            )
            .map(drop)
        })
        .await
        .expect("stated");
    let stated = do_scope(&store, vec![merged(B_PN, B_LID)]).await;
    assert!(stated.is_empty());
    assert_eq!(counts(&store).await, (0, 0, 1), "and it is not kept either");
    owner.close();
}

#[tokio::test]
async fn release_says_nothing_for_a_person_with_nothing_kept() {
    let (_dir, owner) = open();
    let store = owner.store();
    let events = store
        .write("test: release", |tx| release(tx, &pn(A_PN)))
        .await
        .expect("release");
    assert!(events.is_empty());
    owner.close();
}
