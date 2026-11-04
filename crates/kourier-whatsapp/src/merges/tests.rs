//! The merge sweep's rules (W6a2, B9; D7, H4). Synthetic values only: no test prints an id.

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_db::rusqlite::params;
use kourier_model::UserId;

use super::{Counts, Pair, SOURCES, decide, gather, state, vouches};
use crate::map::OwnIds;
use crate::store::merges::Current;
use crate::store::{KourierStore, StoreOwner, book, exec};

fn current(pn: &str, lid: &str, source: &str) -> Current {
    Current {
        pn: format!("pn:{pn}"),
        lid: format!("lid:{lid}"),
        source: source.to_owned(),
    }
}

fn book_pair(pn: &str, lid: &str) -> (String, String) {
    (format!("pn:{pn}"), format!("lid:{lid}"))
}

fn pair(pn: &str, lid: &str) -> (String, String) {
    book_pair(pn, lid)
}

/// Only the four sources the decision names stand alone; every other source upstream has (stale, observational, our own
/// device, a peer's message) and one it may add later merges nothing, and is counted as unvouched. Mutants: `other` or
/// `migration_sync_old` allowed.
#[test]
fn only_a_vouching_source_stands_alone() {
    // The four, by name: a test that walked `SOURCES` would pass for any list.
    assert_eq!(SOURCES.len(), 4);
    for source in [
        "usync",
        "recipient_latest_lid",
        "migration_sync_latest",
        "blocklist_active",
    ] {
        let (pairs, counts) = decide(&[current("15550482001", "99900482001", source)], &[]);
        assert_eq!(pairs, [pair("15550482001", "99900482001")], "{source}");
        assert_eq!(counts.merged, 1);
    }
    for source in [
        "migration_sync_old",
        "blocklist_inactive",
        "device_notification",
        "other",
        "pairing",
        "peer_pn_message",
        "peer_lid_message",
        "a_source_upstream_adds_later",
        "",
    ] {
        assert!(!vouches(source), "{source}");
        let (pairs, counts) = decide(&[current("15550482002", "99900482002", source)], &[]);
        assert!(pairs.is_empty(), "{source}");
        assert_eq!((counts.merged, counts.unvouched), (0, 1), "{source}");
    }
}

/// The address book (our phone's own word) stands alone when it names ONE LID for the number; with the store it must name
/// the store's current LID (then the source of the row does not matter: our phone agrees with it); a book that names two
/// LIDs, or another one than the store's current, merges nothing for the number.
#[test]
fn the_address_book_stands_when_it_is_unambiguous_and_agrees_with_the_store() {
    let one = [book_pair("15550482101", "99900482101")];
    let (pairs, _) = decide(&[], &one);
    assert_eq!(
        pairs,
        [pair("15550482101", "99900482101")],
        "the book alone"
    );

    let (pairs, counts) = decide(
        &[],
        &[
            book_pair("15550482102", "99900482102"),
            book_pair("15550482102", "99900482103"),
        ],
    );
    assert!(pairs.is_empty(), "two LIDs for one number");
    assert_eq!(counts.conflicts, 1);

    let (pairs, _) = decide(
        &[current("15550482104", "99900482104", "other")],
        &[book_pair("15550482104", "99900482104")],
    );
    assert_eq!(
        pairs,
        [pair("15550482104", "99900482104")],
        "our phone agrees with an observational row"
    );

    let (pairs, counts) = decide(
        &[current("15550482105", "99900482105", "usync")],
        &[book_pair("15550482105", "99900482199")],
    );
    assert!(
        pairs.is_empty(),
        "the book names another LID than the store's current"
    );
    assert_eq!(counts.conflicts, 1);

    let (pairs, _) = decide(
        &[current("15550482106", "99900482106", "usync")],
        &[book_pair("15550482106", "99900482106")],
    );
    assert_eq!(
        pairs,
        [pair("15550482106", "99900482106")],
        "agreeing sources: one pair"
    );
}

/// A LID that two numbers claim (a number change) is merged with neither.
#[test]
fn a_lid_two_numbers_claim_goes_to_neither() {
    let (pairs, counts) = decide(
        &[current("15550482201", "99900482201", "usync")],
        &[book_pair("15550482202", "99900482201")],
    );
    assert!(pairs.is_empty());
    assert_eq!(counts.conflicts, 2);
    // An unrelated pair beside them still stands.
    let (pairs, _) = decide(
        &[
            current("15550482201", "99900482201", "usync"),
            current("15550482203", "99900482203", "usync"),
        ],
        &[book_pair("15550482202", "99900482201")],
    );
    assert_eq!(pairs, [pair("15550482203", "99900482203")]);
}

// ---- with a store ----

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([19; 32]),
        "wa-merges",
    )
    .expect("open");
    (dir, owner)
}

fn user(scheme: &str, digits: &str) -> UserId {
    UserId::from_parts(scheme, digits).expect("a user id")
}

fn own() -> OwnIds {
    OwnIds {
        pn: Some(user("pn", "15550489001")),
        lid: Some(user("lid", "99900489001")),
    }
}

async fn plant(store: &KourierStore, rows: &[(&str, &str, i64, &str)], known: &[&str]) {
    let rows: Vec<(String, String, i64, String)> = rows
        .iter()
        .map(|(lid, phone, at, source)| {
            (
                (*lid).to_owned(),
                (*phone).to_owned(),
                *at,
                (*source).to_owned(),
            )
        })
        .collect();
    let known: Vec<String> = known.iter().map(|k| (*k).to_owned()).collect();
    store
        .write("test: plant", move |tx| {
            for (lid, phone, at, source) in &rows {
                exec(
                    tx,
                    "test",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source)
                     VALUES (?1, ?2, 1, ?3, ?4)",
                    params![lid, phone, at, source],
                )?;
            }
            for person in &known {
                book::know(tx, person)?;
            }
            Ok(())
        })
        .await
        .expect("plant");
}

async fn gathered(store: &KourierStore) -> (Vec<Pair>, Counts) {
    let own = own();
    store
        .read(move |conn| gather(conn, &own))
        .await
        .expect("gather")
}

fn merged(events: &[BackendEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::IdentityMerged { from, into } => {
                Some((from.as_str().to_owned(), into.as_str().to_owned()))
            }
            _ => None,
        })
        .collect()
}

/// A number with two LIDs merges its CURRENT one only, and when the newest row is not a vouching one (a recycled number's
/// old owner is the vouched row, the new owner's only a peer's message) NOTHING merges: the older row may belong to
/// someone else now. Mutant: every row of the number is a candidate.
#[tokio::test]
async fn only_the_numbers_current_lid_can_merge() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(
        &store,
        &[
            ("99900483001", "15550483001", 10, "usync"),
            ("99900483002", "15550483001", 20, "usync"),
            ("99900483003", "15550483003", 10, "usync"),
            ("99900483004", "15550483003", 20, "peer_pn_message"),
        ],
        &["pn:15550483001", "pn:15550483003"],
    )
    .await;
    let (pairs, counts) = gathered(&store).await;
    assert_eq!(
        pairs,
        [Pair {
            pn: user("pn", "15550483001"),
            lid: user("lid", "99900483002")
        }]
    );
    assert_eq!((counts.merged, counts.unvouched), (1, 1));
}

/// A stranger's row is still a witness (W6a2 PRIV-3): the scope (`persons_known`) is applied to the pairs `decide` chose, not to
/// what it reads. A recycled number whose store row names the stranger's LID while a stale address-book entry names the
/// known person's LID is a conflict, not a pair of the known one; a book that names two LIDs for one number names two
/// whichever of them the core knows. A clean pair of a known person and a clean pair of a stranger show what the scope still
/// does: the first stands, the second is counted as a stranger's. Mutants: the store's rows read only for known people; the
/// book's pairs read only for known people.
#[tokio::test]
async fn a_strangers_row_still_vetoes_a_pair_of_a_known_person() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(
        &store,
        &[
            // A (15550485001) was recycled: the store's current row names X, a stranger.
            ("99900485003", "15550485001", 20, "usync"),
            // A clean pair of a known person, and one of a stranger.
            ("99900485011", "15550485011", 10, "usync"),
            ("99900485021", "15550485021", 10, "usync"),
        ],
        // Y (the LID the stale book entry names for A), the LID the book names for B, and the clean known pair.
        &["lid:99900485002", "lid:99900485012", "pn:15550485011"],
    )
    .await;
    store
        .write("test: the book", |tx| {
            for (contact, pn, lid) in [
                // The stale entry: A <-> Y, Y is known, A and X are not.
                ("pn:15550485001", "pn:15550485001", "lid:99900485002"),
                // B <-> Y2 (known) and B <-> Z2 (nobody): two LIDs for one number.
                ("pn:15550485004", "pn:15550485004", "lid:99900485012"),
                ("lid:99900485013", "pn:15550485004", "lid:99900485013"),
            ] {
                book::put(
                    tx,
                    &book::Row {
                        contact: contact.to_owned(),
                        name: "Synthetic".to_owned(),
                        pn: Some(pn.to_owned()),
                        lid: Some(lid.to_owned()),
                    },
                )?;
            }
            Ok(())
        })
        .await
        .expect("the book");
    let (pairs, counts) = gathered(&store).await;
    assert_eq!(
        pairs,
        [Pair {
            pn: user("pn", "15550485011"),
            lid: user("lid", "99900485011")
        }],
        "only the clean pair of the known person is stated"
    );
    assert_eq!(
        (counts.merged, counts.conflicts, counts.strangers),
        (1, 2, 1)
    );
}

/// Our own number and our own LID are never merged; a pair that would put a second pair around one the core was told
/// is left out (a LID changed for a merged number, a number changed for a merged LID). Mutants: own ids not skipped, the
/// crossing check dropped.
#[tokio::test]
async fn our_own_ids_and_a_pair_around_a_stated_one_are_left_out() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(
        &store,
        &[
            // Our own number with a LID that is not ours, and a number with our LID.
            ("99900483101", "15550489001", 10, "usync"),
            ("99900489001", "15550483102", 10, "usync"),
            // The core was told (15550483103, 99900483103); the number got a new LID.
            ("99900483104", "15550483103", 20, "usync"),
            // A clean pair.
            ("99900483105", "15550483105", 10, "usync"),
        ],
        &[
            "pn:15550489001",
            "pn:15550483102",
            "pn:15550483103",
            "pn:15550483105",
        ],
    )
    .await;
    store
        .write("test: told", |tx| {
            crate::store::merges::put_stated(tx, "pn:15550483103", "lid:99900483103").map(|_| ())
        })
        .await
        .expect("told");
    let (pairs, counts) = gathered(&store).await;
    assert_eq!(
        pairs,
        [Pair {
            pn: user("pn", "15550483105"),
            lid: user("lid", "99900483105")
        }]
    );
    assert_eq!((counts.own, counts.crossing), (2, 1));
}

/// The zone states each pair once and records it in the same transaction, so a second sweep states nothing; a pair of
/// nobody the core knows and one that crosses a stated pair are left out here too, whatever the sweep read before.
/// Mutants: a pair stated twice (no record), the zone trusting the sweep's scope.
#[tokio::test]
async fn the_zone_states_a_pair_once_and_checks_again() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(&store, &[], &["pn:15550484001", "lid:99900484002"]).await;
    let due = vec![
        Pair {
            pn: user("pn", "15550484001"),
            lid: user("lid", "99900484001"),
        },
        // Nobody the core knows.
        Pair {
            pn: user("pn", "15550484009"),
            lid: user("lid", "99900484009"),
        },
        // Known by the LID only.
        Pair {
            pn: user("pn", "15550484002"),
            lid: user("lid", "99900484002"),
        },
    ];
    let first = due.clone();
    let (events, count) = store
        .write("test: state", move |tx| state(tx, &first))
        .await
        .expect("state");
    assert_eq!(
        merged(&events),
        [
            ("pn:15550484001".to_owned(), "lid:99900484001".to_owned()),
            ("pn:15550484002".to_owned(), "lid:99900484002".to_owned()),
        ]
    );
    assert_eq!(count, 2);

    let again = due.clone();
    let (events, count) = store
        .write("test: again", move |tx| state(tx, &again))
        .await
        .expect("state again");
    assert!(events.is_empty(), "a pair is stated once");
    assert_eq!(count, 0);

    // A second LID for a stated number crosses it.
    let crossing = vec![Pair {
        pn: user("pn", "15550484001"),
        lid: user("lid", "99900484099"),
    }];
    let (events, _) = store
        .write("test: crossing", move |tx| state(tx, &crossing))
        .await
        .expect("state crossing");
    assert!(events.is_empty());
}

/// A pair a ROW states (a message's own pair, a notice's, a history chat's) is recorded in the zone that writes it, so the
/// sweep never puts a second pair around the same number: a number the phone merged into one LID is not merged into the
/// store's newest LID (a recycled number). A repeat of the same pair is no crossing. Mutants: the record dropped; a pair
/// of other schemes recorded.
#[tokio::test]
async fn a_pair_a_row_stated_blocks_a_second_pair_around_the_same_number() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(
        &store,
        &[("99900486002", "15550486001", 20, "usync")],
        &["pn:15550486001"],
    )
    .await;
    let (pairs, _) = gathered(&store).await;
    assert_eq!(
        pairs.len(),
        1,
        "without the row's pair the sweep would state it"
    );

    store
        .write("test: a row's pair", |tx| {
            crate::features::with_statements(
                tx,
                &[
                    BackendEvent::IdentityMerged {
                        from: user("pn", "15550486001"),
                        into: user("lid", "99900486001"),
                    },
                    // Not a pair of a number and a LID: never recorded.
                    BackendEvent::IdentityMerged {
                        from: user("lid", "99900486009"),
                        into: user("pn", "15550486009"),
                    },
                ],
            )
            .map(drop)
        })
        .await
        .expect("a row's zone");
    let (pairs, counts) = gathered(&store).await;
    assert!(
        pairs.is_empty(),
        "the sweep put a second pair around a merged number"
    );
    assert_eq!(counts.crossing, 1);

    // The same pair again is no second pair.
    store
        .write("test: the same pair", |tx| {
            crate::features::with_statements(
                tx,
                &[BackendEvent::IdentityMerged {
                    from: user("pn", "15550486001"),
                    into: user("lid", "99900486001"),
                }],
            )
            .map(drop)
        })
        .await
        .expect("a row's zone");
    let recorded: i64 = store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM merges_stated", [], |r| r.get(0))
                .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(
        recorded, 1,
        "one pair, recorded once; the reversed one is no pair"
    );
}

/// The core keeps a merged-in contact's name but never its verdict that a number may be shown, so the zone that merges a
/// number into a LID states, after the merge, the name and the number the phone's address book holds for the person
/// (B8b's statements, for the LID now); a pair the address book does not name gets the merge alone. Mutants: no restatement;
/// the restatement before the merge.
#[tokio::test]
async fn a_merged_person_gets_the_address_books_name_and_number_for_his_lid() {
    use kourier_model::{IdentifierKind, Patch};

    let (_dir, owner) = open();
    let store = owner.store();
    plant(&store, &[], &["pn:15550487001", "pn:15550487002"]).await;
    store
        .write("test: the book", |tx| {
            book::put(
                tx,
                &book::Row {
                    contact: "pn:15550487001".to_owned(),
                    name: "Ann Saved".to_owned(),
                    pn: Some("pn:15550487001".to_owned()),
                    lid: Some("lid:99900487001".to_owned()),
                },
            )
            .map(drop)
        })
        .await
        .expect("book");
    let due = vec![
        Pair {
            pn: user("pn", "15550487001"),
            lid: user("lid", "99900487001"),
        },
        Pair {
            pn: user("pn", "15550487002"),
            lid: user("lid", "99900487002"),
        },
    ];
    let (events, stated) = store
        .write("test: state", move |tx| state(tx, &due))
        .await
        .expect("state");
    assert_eq!(stated, 2);
    assert_eq!(events.len(), 3, "two merges and one restatement");
    assert!(
        matches!(&events[0], BackendEvent::IdentityMerged { .. })
            && matches!(&events[1], BackendEvent::IdentityMerged { .. }),
        "the merges come first"
    );
    let BackendEvent::ContactUpsert(patch) = &events[2] else {
        panic!("the third event is the person's statement");
    };
    assert_eq!(patch.user, user("lid", "99900487001"), "stated for the LID");
    assert_eq!(patch.system_name, Patch::Set("Ann Saved".to_owned()));
    assert_eq!(patch.number_shown, Some(true));
    let numbers: Vec<_> = patch
        .identifiers
        .iter()
        .flatten()
        .map(|i| (i.kind, i.value.as_str()))
        .collect();
    assert_eq!(numbers, [(IdentifierKind::Phone, "+15550487001")]);
}
