//! The history's address-book names (W6a2, B8b; D8): kind 3 of `history_deferred`, taken up once into the mirror, the
//! people the core knows named, the rest waiting. Every id and name is synthetic.

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::{Patch, UserId};
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use super::{PER_PASS, take_up};
use crate::store::book;
use crate::store::{KourierStore, StoreOwner, exec};

const NOW: i64 = 1_760_000_000_000;
const ANN: &str = "15550481401";
const BOB: &str = "15550481402";

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([18; 32]),
        "wa-history-contacts",
    )
    .expect("open");
    (dir, owner)
}

fn inline(pn: &str, full: Option<&str>, first: Option<&str>) -> Vec<u8> {
    let mut c = wa::InlineContact::default();
    c.pn_jid = Some(format!("{pn}@s.whatsapp.net"));
    c.full_name = full.map(str::to_owned);
    c.first_name = first.map(str::to_owned);
    c.encode_to_vec()
}

async fn keep(store: &KourierStore, n: i64, kind: u8, item: Vec<u8>) {
    store
        .write("test: keep", move |tx| {
            exec(
                tx,
                "test: keep",
                "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) VALUES ('c1', ?1, '', ?2, 4, ?3)",
                kourier_db::rusqlite::params![n, item, kind],
            )
            .map(|_| ())
        })
        .await
        .expect("keep");
}

async fn kept(store: &KourierStore, kind: u8) -> i64 {
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM history_deferred WHERE kind = ?1",
                [kind],
                |r| r.get(0),
            )
            .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count")
}

async fn told(store: &KourierStore) -> Vec<(UserId, Patch<String>)> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                for event in decode_outbox_row(&row.envelope).expect("decodes") {
                    if let BackendEvent::ContactUpsert(patch) = event {
                        out.push((patch.user, patch.system_name));
                    }
                }
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

/// Kind 3 is taken up once: the mirror takes each entry, only a person the core knows is named, an entry the app state
/// already holds stays as it is (the live word wins over an old push), a nameless entry is dropped, and the other kinds
/// stay. Mutants: the entry is kept after the take-up (a second pass repeats); the mirror is overwritten by the history;
/// a stranger is named; kind 1 is taken.
#[tokio::test]
async fn the_kept_address_book_is_taken_into_the_mirror_once_and_names_only_the_known() {
    let (_dir, owner) = open();
    let store = owner.store();
    // Ann is a person the core knows; Bob is not. The app state already named Ann ("Ann Live") before the push was taken up.
    store
        .write("test: know", |tx| {
            book::know(tx, &format!("pn:{ANN}")).map(|_| ())
        })
        .await
        .expect("know");
    keep(&store, 1, 3, inline(ANN, Some("Ann Old"), None)).await;
    keep(&store, 2, 3, inline(BOB, None, Some("Bob"))).await;
    keep(&store, 3, 3, inline("15550481403", None, None)).await;
    keep(&store, 4, 1, vec![1, 2, 3]).await;
    store
        .write("test: the live word", |tx| {
            let mut action = wa::sync_action_value::ContactAction::default();
            action.full_name = Some("Ann Live".to_owned());
            let change = crate::book::change_of_action(
                &format!("{ANN}@s.whatsapp.net").parse().expect("jid"),
                &action,
            )
            .expect("a change");
            crate::book::apply(tx, &change, 0).map(|_| ())
        })
        .await
        .expect("apply");
    let before = told(&store).await;
    assert!(
        before.is_empty(),
        "nothing is told to the outbox by a direct zone"
    );

    let done = take_up(&store, NOW).await.expect("a pass").expect("due");
    assert_eq!((done.taken, done.unread), (2, 1));
    assert!(
        done.seq.is_none(),
        "Ann keeps her live name: nothing new to state; Bob is a stranger"
    );
    assert_eq!(
        kept(&store, 3).await,
        0,
        "taken up once: the entries are gone"
    );
    assert_eq!(
        kept(&store, 1).await,
        1,
        "a call record is not an address-book entry"
    );
    let (ann, bob) = store
        .read(|conn| {
            Ok((
                book::of(conn, &format!("pn:{ANN}"))?,
                book::of(conn, &format!("pn:{BOB}"))?,
            ))
        })
        .await
        .expect("read");
    assert_eq!(
        ann.expect("Ann").name,
        "Ann Live",
        "the app state's word stays"
    );
    assert_eq!(
        bob.expect("Bob").name,
        "Bob",
        "a stranger waits in the mirror"
    );
    assert!(
        take_up(&store, NOW).await.expect("a pass").is_none(),
        "nothing is due again"
    );

    // Carla is known and the history names her: stated in the same pass's row.
    store
        .write("test: know", |tx| {
            book::know(tx, "pn:15550481404").map(|_| ())
        })
        .await
        .expect("know");
    keep(
        &store,
        5,
        3,
        inline("15550481404", Some("Carla Example"), None),
    )
    .await;
    let done = take_up(&store, NOW).await.expect("a pass").expect("due");
    assert!(done.seq.is_some());
    let said = told(&store).await;
    assert_eq!(said.len(), 1);
    assert_eq!(
        said[0].0,
        UserId::from_parts("pn", "15550481404").expect("a user")
    );
    assert_eq!(said[0].1, Patch::Set("Carla Example".to_owned()));
    owner.close();
}

#[tokio::test]
async fn a_pass_takes_a_bounded_number_of_entries() {
    let (_dir, owner) = open();
    let store = owner.store();
    let total = PER_PASS + 5;
    for n in 0..total {
        keep(
            &store,
            n,
            3,
            inline(
                &format!("1555048{:04}", 2000 + n),
                Some("Someone Example"),
                None,
            ),
        )
        .await;
    }
    let first = take_up(&store, NOW).await.expect("a pass").expect("due");
    assert_eq!(i64::from(first.taken), PER_PASS);
    let second = take_up(&store, NOW).await.expect("a pass").expect("due");
    assert_eq!(i64::from(second.taken), 5);
    assert!(take_up(&store, NOW).await.expect("a pass").is_none());
    owner.close();
}
