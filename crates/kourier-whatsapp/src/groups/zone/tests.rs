//! The zone half of a group's notice (W6a2, B5): what the acts of a notice do against the group's row. Synthetic values only:
//! the tails are 8 or more digits and a substring of no other value here.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_model::{
    Body, CannotSend, ConvKey, ConversationPatch, Member, MemberRole, Origin, Patch, SystemEvent,
    UserId,
};

use wacore_binary::Jid;
use whatsapp_rust::{GroupMetadata, GroupMetadataResult, GroupParticipant};

use super::{Counts, apply, fetched, fetched_by_group};
use crate::groups::{self, Change, Rules, State};
use crate::map::OwnIds;
use crate::notice::{Act, Noticed};
use crate::store::{KourierStore, StoreOwner, people};

const GROUP: &str = "120363000071529611";
const ADA: &str = "99000071522";
const BOB: &str = "99000071533";
const NOW: i64 = 1_800_000_000_000;

fn conv() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn lid(digits: &str) -> UserId {
    UserId::from_parts("lid", digits).expect("a user")
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([21; 32]),
        "wa-group-zone",
    )
    .expect("open");
    (dir, owner)
}

/// The line a notice tells.
fn line() -> BackendEvent {
    BackendEvent::Message(IncomingMessage::new(
        conv(),
        "gp:3178000001:0",
        Origin::Remote {
            author: lid(ADA),
            device: None,
        },
        Body::System(SystemEvent::DescriptionChanged { by: Some(lid(ADA)) }),
        1_700_000_100_000,
        NOW,
    ))
}

fn notice(acts: Vec<Act>) -> Noticed {
    Noticed {
        conv: conv(),
        events: vec![line()],
        acts,
    }
}

async fn seed(store: &KourierStore, state: State) {
    store
        .write("test: seed a group", move |tx| {
            people::put_group(tx, &conv(), &state)
        })
        .await
        .expect("seed");
}

async fn row(store: &KourierStore) -> State {
    store
        .read(|conn| people::group(conn, &conv()))
        .await
        .expect("row")
}

async fn marks(store: &KourierStore) -> Vec<(ConvKey, i64)> {
    store.read(people::dirty).await.expect("marks")
}

async fn run(store: &KourierStore, noticed: Noticed, now: i64) -> Vec<BackendEvent> {
    store
        .write("test: a group's notice", move |tx| apply(tx, &noticed, now))
        .await
        .expect("apply")
}

/// The upsert in front of the line, if the zone made one; the line is always last.
fn upsert(events: &[BackendEvent]) -> Option<&ConversationPatch> {
    let (last, rest) = events.split_last().expect("events");
    assert!(
        matches!(last, BackendEvent::Message(_)),
        "the line stays last: {events:?}"
    );
    match rest {
        [] => None,
        [BackendEvent::ConversationUpsert(patch)] => Some(patch),
        other => panic!("one upsert at most: {other:?}"),
    }
}

fn whole() -> State {
    State {
        members_stated: true,
        rules: Some(Rules::default()),
        ..State::default()
    }
}

fn admin(digits: &str) -> Member {
    Member {
        user: lid(digits),
        role: MemberRole::Admin,
    }
}

/// M5: a rename older than the title the core was told sets nothing and the row keeps the newer time; a newer one (or one
/// of the same second) sets the title and moves the time on.
/// Mutants: the zone sets the title whatever the row says; the row's time moved back by an older rename.
#[tokio::test]
async fn a_rename_older_than_the_title_the_core_was_told_sets_nothing() {
    let (_dir, owner) = open();
    let store = owner.store();
    seed(
        &store,
        State {
            subject_time: Some(2000),
            ..State::default()
        },
    )
    .await;
    let title = |at_s| Act::Title {
        title: "A synthetic name".to_owned(),
        at_s,
    };
    // Older: only the line.
    let events = run(&store, notice(vec![title(1000)]), NOW).await;
    assert_eq!(events, vec![line()], "no title is set");
    assert_eq!(row(&store).await.subject_time, Some(2000));
    // The same second and a newer one set it.
    for at in [2000, 3000] {
        let events = run(&store, notice(vec![title(at)]), NOW).await;
        let patch = upsert(&events).expect("the title");
        assert_eq!(patch.title, Patch::Set("A synthetic name".to_owned()));
        assert!(patch.features.is_none(), "a title restates no features");
        assert_eq!(row(&store).await.subject_time, Some(at));
    }
    // A group with no row yet: any rename sets the title and makes the row.
    let (_dir, owner) = open();
    let events = run(&owner.store(), notice(vec![title(5)]), NOW).await;
    assert!(upsert(&events).is_some());
    assert_eq!(row(&owner.store()).await.subject_time, Some(5));
}

/// D9, M4: a member delta is stated only when the core holds the group's whole list, and then ALWAYS with the group's
/// features in the same patch; with no list the group is marked for a fetch and no delta is stated (it would show a
/// partial list).
/// Mutants: the delta stated with no list; the delta stated without the features.
#[tokio::test]
async fn a_member_delta_needs_the_whole_list_and_always_comes_with_the_features() {
    let (_dir, owner) = open();
    let store = owner.store();
    let promote = || {
        notice(vec![Act::Members {
            upsert: vec![admin(BOB)],
            remove: vec![lid(ADA)],
        }])
    };
    // No list stated: no delta, a mark.
    let events = run(&store, promote(), NOW).await;
    assert_eq!(events, vec![line()]);
    assert_eq!(marks(&store).await, vec![(conv(), NOW)]);
    assert!(
        !row(&store).await.members_stated,
        "a delta never makes a list"
    );
    // The list is stated: the delta, with the features made from the row.
    seed(
        &store,
        State {
            rules: Some(Rules {
                announcements_only: true,
                ..Rules::default()
            }),
            ..whole()
        },
    )
    .await;
    let events = run(&store, promote(), NOW + 1).await;
    let patch = upsert(&events).expect("the delta");
    let list = patch.members.as_ref().expect("members");
    assert!(!list.full, "a delta, not a snapshot");
    assert_eq!(list.upsert, vec![admin(BOB)]);
    assert_eq!(list.remove, vec![lid(ADA)]);
    let features = patch.features.as_ref().expect("the members never go alone");
    assert_eq!(features.cannot_send, Some(CannotSend::AdminsOnly));
    assert_eq!(
        features,
        &groups::features(&conv(), &row(&store).await),
        "the features of the row"
    );
}

/// D10, M4: a rule follows the row and the features are stated again from it (`group_info`, and `cannot_send` while only
/// admins may send and none of our ids is one); a rule that changes nothing states nothing; with no rules known the group
/// is marked for a fetch instead (one bit alone would claim the others off); our own promote lifts `AdminsOnly`.
/// Mutants: no restate after `Announce`; `Announce` ignored by the row.
#[tokio::test]
async fn a_rule_changes_the_row_and_restates_the_features() {
    let (_dir, owner) = open();
    let store = owner.store();
    seed(&store, whole()).await;
    let announce = |on| notice(vec![Act::Rule(Change::Announcements(on))]);
    let events = run(&store, announce(true), NOW).await;
    let patch = upsert(&events).expect("a restate");
    let features = patch.features.as_ref().expect("features");
    assert!(features.group_info.expect("group_info").announcements_only);
    assert_eq!(features.cannot_send, Some(CannotSend::AdminsOnly));
    assert!(patch.members.is_none() && patch.title == Patch::Unchanged);
    assert!(row(&store).await.rules.expect("rules").announcements_only);
    // The same again: nothing to say.
    let events = run(&store, announce(true), NOW).await;
    assert_eq!(events, vec![line()]);
    // We are made an admin: we may send again.
    let events = run(&store, notice(vec![Act::Rule(Change::OwnAdmin(true))]), NOW).await;
    let features = upsert(&events)
        .and_then(|p| p.features.clone())
        .expect("a restate");
    assert_eq!(features.cannot_send, None);
    assert!(features.group_info.expect("group_info").announcements_only);
    // The other rules.
    let events = run(
        &store,
        notice(vec![
            Act::Rule(Change::Locked(true)),
            Act::Rule(Change::MembersAdminOnly(true)),
        ]),
        NOW,
    )
    .await;
    let info = upsert(&events)
        .and_then(|p| p.features.as_ref())
        .and_then(|f| f.group_info)
        .expect("group_info");
    assert!(info.attributes_admin_only && info.members_admin_only && info.announcements_only);
    // A group whose rules were never answered: a mark, no statement.
    let (_dir, other) = open();
    let events = run(&other.store(), announce(true), NOW).await;
    assert_eq!(events, vec![line()]);
    assert_eq!(marks(&other.store()).await, vec![(conv(), NOW)]);
    assert_eq!(row(&other.store()).await.rules, None);
}

/// D9, D10: a removal of us makes the group `NotMember`; being put back claims nothing (the rules and the list are not
/// known any more) and asks for a fetch; a deletion ends the group for everyone and keeps us out of it.
/// Mutants: `Gone` states nothing; `Back` keeps the old rules.
#[tokio::test]
async fn our_place_in_the_group_follows_removal_return_and_deletion() {
    let (_dir, owner) = open();
    let store = owner.store();
    seed(
        &store,
        State {
            rules: Some(Rules {
                announcements_only: true,
                ..Rules::default()
            }),
            ..whole()
        },
    )
    .await;
    let events = run(&store, notice(vec![Act::Gone]), NOW).await;
    let features = upsert(&events)
        .and_then(|p| p.features.clone())
        .expect("a statement");
    assert_eq!(features.cannot_send, Some(CannotSend::NotMember));
    assert!(row(&store).await.not_member);
    // Told twice: said once.
    let events = run(&store, notice(vec![Act::Gone]), NOW).await;
    assert_eq!(events, vec![line()]);
    // Back: nothing is claimed of the group until it is read, and the group is fetched.
    let events = run(&store, notice(vec![Act::Back]), NOW + 5).await;
    let features = upsert(&events)
        .and_then(|p| p.features.clone())
        .expect("a statement");
    assert_eq!(features.cannot_send, None, "AdminsOnly is not carried over");
    assert!(features.group_info.is_none());
    let after = row(&store).await;
    assert!(!after.not_member && after.rules.is_none() && !after.members_stated);
    assert_eq!(marks(&store).await, vec![(conv(), NOW + 5)]);
    // A deletion ends it: terminated, not a member.
    seed(&store, whole()).await;
    let events = run(&store, notice(vec![Act::Deleted]), NOW).await;
    let features = upsert(&events)
        .and_then(|p| p.features.clone())
        .expect("a statement");
    assert_eq!(features.cannot_send, Some(CannotSend::NotMember));
    assert!(features.group_info.expect("terminated").terminated);
    assert!(row(&store).await.not_member);
}

/// A mark keeps the time of the FIRST ask; the zone's upsert goes in front of the line and behind the pairs; a notice with
/// no act changes nothing and writes no row.
/// Mutant: the mark overwritten by the later ask.
#[tokio::test]
async fn a_mark_keeps_its_first_time_and_the_upsert_goes_in_front_of_the_line() {
    let (_dir, owner) = open();
    let store = owner.store();
    let merge = BackendEvent::IdentityMerged {
        from: UserId::from_parts("pn", "31000071544").expect("a user"),
        into: lid(BOB),
    };
    let events = run(&store, notice(vec![Act::Fetch]), NOW).await;
    assert_eq!(events, vec![line()]);
    run(&store, notice(vec![Act::Fetch]), NOW + 60_000).await;
    assert_eq!(marks(&store).await, vec![(conv(), NOW)], "the first ask");
    // The upsert sits between the merge and the line.
    let with_merge = Noticed {
        conv: conv(),
        events: vec![merge.clone(), line()],
        acts: vec![Act::Title {
            title: "x".to_owned(),
            at_s: 9,
        }],
    };
    let events = run(&store, with_merge, NOW).await;
    assert!(matches!(
        events.as_slice(),
        [
            BackendEvent::IdentityMerged { .. },
            BackendEvent::ConversationUpsert(_),
            BackendEvent::Message(_)
        ]
    ));
    // No act: the events as they are, no row.
    let (_dir, other) = open();
    let events = run(&other.store(), notice(vec![]), NOW).await;
    assert_eq!(events, vec![line()]);
    assert_eq!(row(&other.store()).await, State::default());
    assert!(marks(&other.store()).await.is_empty());
}

// ---- B6: the zone half of a fetch's answer ----

const GROUP_TWO: &str = "120363000071529622";
const GROUP_THREE: &str = "120363000071529633";
const OUR_LID: &str = "99900071544";

fn group_of(digits: &str) -> ConvKey {
    ConvKey::Group(format!("wag:{digits}").into())
}

fn jid(digits: &str) -> Jid {
    format!("{digits}@g.us").parse().expect("a jid")
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(lid(OUR_LID)),
        pn: None,
    }
}

/// A group of Ada (admin) and us.
fn found_group(digits: &str) -> GroupMetadataResult {
    let mut metadata = GroupMetadata::new(jid(digits));
    metadata.subject = Some("A synthetic group".into());
    metadata.subject_time = Some(1_700_000_100);
    let mut ada = GroupParticipant::new(format!("{ADA}@lid").parse().expect("a jid"));
    ada.participant_type = whatsapp_rust::ParticipantType::Admin;
    let us = GroupParticipant::new(format!("{OUR_LID}@lid").parse().expect("a jid"));
    metadata.participants = vec![ada, us];
    GroupMetadataResult::Found(Box::new(metadata))
}

async fn answer(
    store: &KourierStore,
    answers: Vec<GroupMetadataResult>,
) -> (Vec<BackendEvent>, Counts) {
    store
        .write("test: a fetch's answer", move |tx| {
            fetched(tx, &answers, &own(), NOW, NOW)
        })
        .await
        .expect("fetched")
}

async fn marks_of(store: &KourierStore) -> Vec<ConvKey> {
    marks(store)
        .await
        .into_iter()
        .map(|(conv, _)| conv)
        .collect()
}

/// F22 (DUR-13): the events of an answer come out one vector PER GROUP, in the order of the answer, so the writer appends one
/// `core_outbox` row for each. A group that states nothing (a `Truncated` entry, a refusal of a group never stated) makes none.
/// Mutant: every group's events pushed into the first vector.
#[tokio::test]
async fn an_answer_makes_the_events_of_each_group_apart() {
    let (_dir, owner) = open();
    let store = owner.store();
    let answers = vec![
        found_group(GROUP),
        GroupMetadataResult::Truncated {
            id: jid(GROUP_FOUR),
            participant_count: 1500,
        },
        found_group(GROUP_TWO),
        GroupMetadataResult::NotFound(jid(GROUP_FIVE)),
        found_group(GROUP_THREE),
    ];
    let (rows, counts) = store
        .write("test: an answer by group", move |tx| {
            fetched_by_group(tx, &answers, &own(), NOW, NOW)
        })
        .await
        .expect("fetched");
    assert_eq!(
        (counts.found, counts.truncated, counts.not_member),
        (3, 1, 1)
    );
    let keys: Vec<Vec<&ConvKey>> = rows
        .iter()
        .map(|events| {
            events
                .iter()
                .map(|event| match event {
                    BackendEvent::ConversationUpsert(patch) => &patch.key,
                    other => panic!("an upsert only: {other:?}"),
                })
                .collect()
        })
        .collect();
    let (one, two, three) = (group_of(GROUP), group_of(GROUP_TWO), group_of(GROUP_THREE));
    assert_eq!(keys, [vec![&one], vec![&two], vec![&three]]);
}

/// D9, scout T3 + T5: an answer states a `Found` group whole and clears ITS mark; a `Truncated` entry records the row and
/// states nothing and keeps its mark (the single query that follows clears it); a group the answer does not name keeps its
/// row and its mark (absence proves nothing).
/// Mutants: a truncated entry clears the mark; the whole answer clears every mark.
#[tokio::test]
async fn an_answer_states_a_found_group_and_clears_only_what_it_answered_whole() {
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("test: marks", |tx| {
            for digits in [GROUP, GROUP_TWO, GROUP_THREE] {
                people::mark_dirty(tx, &group_of(digits), NOW - 5000)?;
            }
            Ok(())
        })
        .await
        .expect("marks");
    let (events, counts) = answer(
        &store,
        vec![
            found_group(GROUP),
            GroupMetadataResult::Truncated {
                id: jid(GROUP_TWO),
                participant_count: 1500,
            },
        ],
    )
    .await;
    assert_eq!(
        counts,
        Counts {
            found: 1,
            truncated: 1,
            not_member: 0,
            skipped: 0,
            ..Counts::default()
        }
    );
    let [BackendEvent::ConversationUpsert(patch)] = events.as_slice() else {
        panic!("exactly the found group's upsert: {} events", events.len());
    };
    assert_eq!(patch.key, conv());
    assert!(patch.members.as_ref().is_some_and(|list| list.full));
    assert!(
        patch.features.is_some(),
        "members never go without features"
    );
    assert_eq!(
        marks_of(&store).await,
        [group_of(GROUP_TWO), group_of(GROUP_THREE)]
    );
    assert!(row(&store).await.members_stated);
    let two = store
        .read(|conn| people::group(conn, &group_of(GROUP_TWO)))
        .await
        .expect("row");
    assert!(two.truncated && !two.members_stated);
    let three = store
        .read(|conn| people::group(conn, &group_of(GROUP_THREE)))
        .await
        .expect("row");
    assert_eq!(
        three,
        State::default(),
        "a group the answer did not name is untouched"
    );
}

/// D9: a group the server says we are not in is recorded and its mark cleared; the core is told `NotMember` only for a
/// group the backend stated before (otherwise it would get a chat with no title for a group nobody here has seen).
/// Mutants: a refusal of a never-stated group is stated; the refusal's mark stays.
#[tokio::test]
async fn a_refusal_says_not_a_member_only_of_a_group_the_core_has() {
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("test: a stated chat and two marks", |tx| {
            crate::send::started::mark_stated(
                tx,
                &group_of(GROUP),
                crate::features::VERSION,
                true,
            )?;
            people::mark_dirty(tx, &group_of(GROUP), NOW)?;
            people::mark_dirty(tx, &group_of(GROUP_TWO), NOW)
        })
        .await
        .expect("seed");
    let (events, counts) = answer(
        &store,
        vec![
            GroupMetadataResult::Forbidden(jid(GROUP)),
            GroupMetadataResult::NotFound(jid(GROUP_TWO)),
        ],
    )
    .await;
    assert_eq!(counts.not_member, 2);
    let [BackendEvent::ConversationUpsert(patch)] = events.as_slice() else {
        panic!("only the stated group is told: {} events", events.len());
    };
    assert_eq!(patch.key, conv());
    assert_eq!(
        patch.features.as_ref().and_then(|f| f.cannot_send.clone()),
        Some(CannotSend::NotMember)
    );
    assert!(marks_of(&store).await.is_empty(), "a refusal is an answer");
    let two = store
        .read(|conn| people::group(conn, &group_of(GROUP_TWO)))
        .await
        .expect("row");
    assert!(two.not_member, "the row records it for both");
}

/// M1: a mark made after the ask is not covered by its answer (the server computed it before the notice) and stays; one made
/// before it is cleared.
/// Mutant: the answer clears a mark newer than its ask.
#[tokio::test]
async fn a_mark_made_after_the_ask_survives_the_answer() {
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("test: marks", |tx| {
            people::mark_dirty(tx, &group_of(GROUP), NOW - 1)?;
            people::mark_dirty(tx, &group_of(GROUP_TWO), NOW + 1)
        })
        .await
        .expect("marks");
    answer(&store, vec![found_group(GROUP), found_group(GROUP_TWO)]).await;
    assert_eq!(marks_of(&store).await, [group_of(GROUP_TWO)]);
}

/// Fix F16 (DUR-2): an OLDER mark stands, the ask is made after it, and ANOTHER notice marks the group after the ask while the
/// answer is on its way: the answer does not cover the newer notice, so the mark stays (and a mark whose newest ask the answer
/// does cover goes). Mutant: the mark cleared by the time of its first ask.
#[tokio::test]
async fn a_newer_mark_on_top_of_an_older_one_survives_the_answer() {
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("test: marks", |tx| {
            // Group one: marked at NOW - 5 s, again at NOW + 1 (after the ask at NOW). Group two: both asks before the ask.
            people::mark_dirty(tx, &group_of(GROUP), NOW - 5000)?;
            people::mark_dirty(tx, &group_of(GROUP), NOW + 1)?;
            people::mark_dirty(tx, &group_of(GROUP_TWO), NOW - 5000)?;
            people::mark_dirty(tx, &group_of(GROUP_TWO), NOW - 1)
        })
        .await
        .expect("marks");
    answer(&store, vec![found_group(GROUP), found_group(GROUP_TWO)]).await;
    assert_eq!(marks_of(&store).await, [group_of(GROUP)]);
    let left = store.read(people::dirty).await.expect("marks");
    assert_eq!(
        left,
        [(group_of(GROUP), NOW - 5000)],
        "the order still goes by the first ask"
    );
}

// ---- B7a: the zone that states a group's members lets its waiting admin deletes go ----

const GROUP_FOUR: &str = "120363000071529644";
const GROUP_FIVE: &str = "120363000071529655";

/// An inbox row of an admin's delete that waits for the member list of `group` (`state = 'new'`, parked, marked).
async fn waiting(store: &KourierStore, id: &'static str, group: &str) {
    let key = group_of(group);
    let chat = format!("{group}@g.us");
    store
        .write("test: a waiting delete", move |tx| {
            tx.execute(
                "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, message, meta, state, parked_until, \
                                   members_wait) \
                 VALUES (?1, 'x@lid', ?2, 'e2e', 1, 0, x'00', x'00', 'new', 4000000000000, ?3)",
                kourier_db::rusqlite::params![chat, id, key.encode()],
            )
            .map_err(crate::error::sql("test: a waiting delete"))
            .map(drop)
        })
        .await
        .expect("insert");
}

async fn inbox_of(store: &KourierStore, id: &'static str) -> (String, i64, Option<String>, bool) {
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT state, parked_until, members_wait, message IS NULL FROM inbox WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(crate::error::sql("test: read an inbox row"))
        })
        .await
        .expect("row")
}

/// D11 + M3: the answer that states a group's whole member list lets THAT group's waiting deletes go (due at once, the park
/// and the mark cleared) in the same zone; a group the answer does not name and a group whose entry is `Truncated` keep
/// their rows waiting; a `NotFound` entry for a group whose list was never stated finishes its waiting deletes with no event
/// and wipes them. An entry that is `Found` with NO participants states no list (B4) and lets nothing go.
/// Mutants: every group's rows are let go; an answer with no list lets its rows go; a refusal lets them go instead of
/// ending them.
#[tokio::test]
async fn an_answer_that_states_the_members_lets_that_groups_deletes_go_and_a_refusal_ends_them() {
    let (_dir, owner) = open();
    let store = owner.store();
    for (id, group) in [
        ("A", GROUP),
        ("B", GROUP_TWO),
        ("C", GROUP_THREE),
        ("D", GROUP_FOUR),
        ("E", GROUP_FIVE),
    ] {
        waiting(&store, id, group).await;
    }
    let (_, counts) = answer(
        &store,
        vec![
            found_group(GROUP),
            GroupMetadataResult::Truncated {
                id: jid(GROUP_THREE),
                participant_count: 1500,
            },
            GroupMetadataResult::NotFound(jid(GROUP_FOUR)),
            GroupMetadataResult::Found(Box::new(GroupMetadata::new(jid(GROUP_FIVE)))),
        ],
    )
    .await;
    assert_eq!((counts.released, counts.ended), (1, 1));
    let group = |digits: &str| Some(group_of(digits).encode());
    assert_eq!(
        inbox_of(&store, "A").await,
        ("new".to_owned(), 0, None, false),
        "due at once, nothing else touched"
    );
    assert_eq!(
        inbox_of(&store, "B").await,
        ("new".to_owned(), 4_000_000_000_000, group(GROUP_TWO), false),
        "a group the answer did not name"
    );
    assert_eq!(
        inbox_of(&store, "C").await,
        (
            "new".to_owned(),
            4_000_000_000_000,
            group(GROUP_THREE),
            false
        ),
        "a truncated entry states no list"
    );
    assert_eq!(
        inbox_of(&store, "D").await,
        ("done".to_owned(), 0, None, true),
        "we are not in the group: finished and wiped"
    );
    assert_eq!(
        inbox_of(&store, "E").await,
        (
            "new".to_owned(),
            4_000_000_000_000,
            group(GROUP_FIVE),
            false
        ),
        "an answer with no participants states no list"
    );
    owner.close();
}

/// Fix F2 (PROTOCOL-2): a `Found` answer flagged `missing_participant_identification` lets no waiting admin delete go
/// (the core would drop an admin's delete for good when the list left the admin out), states no member list on a group
/// whose list the core never got, and removes nobody on one it holds; the group stays owed a single query. The unflagged
/// answer after it releases the delete as before.
/// Mutant: the flag ignored.
#[tokio::test]
async fn an_incomplete_answer_releases_nothing_and_removes_nobody() {
    let flagged = |digits: &str| {
        let GroupMetadataResult::Found(mut metadata) = found_group(digits) else {
            panic!("a found group");
        };
        metadata.has_missing_participant_identification = true;
        GroupMetadataResult::Found(metadata)
    };
    let (_dir, owner) = open();
    let store = owner.store();
    waiting(&store, "A", GROUP).await;
    store
        .write("test: the list of the second group was stated", |tx| {
            people::put_group(tx, &group_of(GROUP_TWO), &whole())
        })
        .await
        .expect("seed");
    let (events, counts) = answer(&store, vec![flagged(GROUP), flagged(GROUP_TWO)]).await;
    assert_eq!(counts.released, 0, "an incomplete list releases nothing");
    assert_eq!(
        inbox_of(&store, "A").await,
        (
            "new".to_owned(),
            4_000_000_000_000,
            Some(group_of(GROUP).encode()),
            false
        ),
        "still waiting"
    );
    let first = upserts(&events)
        .into_iter()
        .find(|patch| patch.key == group_of(GROUP))
        .expect("the first group");
    assert!(first.members.is_none(), "no list stated on no list");
    assert!(!row(&store).await.members_stated);
    assert!(row(&store).await.truncated, "owed a single query");
    let second = upserts(&events)
        .into_iter()
        .find(|patch| patch.key == group_of(GROUP_TWO))
        .expect("the second group");
    let list = second.members.as_ref().expect("a delta");
    assert!(!list.full && list.remove.is_empty());
    // The whole answer afterwards lets the first group's delete go.
    let (_, counts) = answer(&store, vec![found_group(GROUP)]).await;
    assert_eq!(counts.released, 1);
    assert_eq!(
        inbox_of(&store, "A").await,
        ("new".to_owned(), 0, None, false)
    );
    owner.close();
}

/// Fix F5 (PROTOCOL-9 + DUR-10): a notice that says we are out (`Gone`) or that the group is over (`Deleted`) finishes
/// that group's parked admin deletes at once, in the notice's own zone (no event, the content wiped, the park and the mark
/// cleared); another group's stay, and a group whose list the core holds has none to finish.
/// Mutant: the finish dropped.
#[tokio::test]
async fn leaving_a_group_ends_its_waiting_deletes() {
    let (_dir, owner) = open();
    let store = owner.store();
    for act in [Act::Gone, Act::Deleted] {
        let a = if matches!(act, Act::Gone) { "G1" } else { "D1" };
        let b = if matches!(act, Act::Gone) { "G2" } else { "D2" };
        seed(&store, State::default()).await;
        waiting(&store, a, GROUP).await;
        waiting(&store, b, GROUP_TWO).await;
        let events = run(&store, notice(vec![act]), NOW).await;
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, BackendEvent::Message(m) if &*m.local == a)),
            "a finish is no event"
        );
        assert_eq!(
            inbox_of(&store, a).await,
            ("done".to_owned(), 0, None, true),
            "finished and wiped"
        );
        assert_eq!(
            inbox_of(&store, b).await,
            (
                "new".to_owned(),
                4_000_000_000_000,
                Some(group_of(GROUP_TWO).encode()),
                false
            ),
            "another group's delete keeps waiting"
        );
    }
    owner.close();
}

// ---- Q3: a subgroup states its community only when the core has that chat ----

/// A subgroup of `parent`.
fn subgroup_of(digits: &str, parent: &str) -> GroupMetadataResult {
    let GroupMetadataResult::Found(mut metadata) = found_group(digits) else {
        panic!("a found group");
    };
    metadata.parent_group_jid = Some(jid(parent));
    GroupMetadataResult::Found(metadata)
}

fn upserts(events: &[BackendEvent]) -> Vec<&ConversationPatch> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) => Some(patch),
            _ => None,
        })
        .collect()
}

async fn row_of(store: &KourierStore, digits: &'static str) -> State {
    store
        .read(move |conn| people::group(conn, &group_of(digits)))
        .await
        .expect("row")
}

/// The core was told the chat `digits` (what a features statement of a group at a version some build states leaves).
async fn core_knows(store: &KourierStore, digits: &'static str) {
    store
        .write("test: the core knows a group", move |tx| {
            crate::send::started::mark_stated(tx, &group_of(digits), 4, true)
        })
        .await
        .expect("stated");
}

/// Q3: a subgroup answered before its community is stated WITHOUT a parent (the core would make a bare, title-less chat of
/// the community) and the row names the parent and waits; the zone that states the community afterwards also states it for
/// the subgroup, once. Kills "the parent stated unconditionally", "the later statement dropped" and "stated again at every
/// fetch of the community".
#[tokio::test]
async fn a_subgroup_before_its_community_has_no_parent_until_the_community_is_stated() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (events, counts) = answer(&store, vec![subgroup_of(GROUP_TWO, GROUP)]).await;
    let [patch] = upserts(&events).try_into().expect("the subgroup's upsert");
    assert_eq!(patch.key, group_of(GROUP_TWO));
    assert_eq!(patch.parent, Patch::Unchanged, "the community is unknown");
    assert!(patch.title != Patch::Unchanged, "the rest is stated");
    assert_eq!(counts.parents_waiting, 1);
    let waiting = row_of(&store, GROUP_TWO).await;
    assert_eq!(waiting.parent, Some(conv()));
    assert!(!waiting.parent_stated);

    // The community is stated (by a batch or a single query): both upserts are in THIS zone, the community first.
    let (events, counts) = answer(&store, vec![found_group(GROUP)]).await;
    let upserts_now = upserts(&events);
    assert_eq!(upserts_now.len(), 2, "{events:?}");
    assert_eq!(upserts_now[0].key, conv());
    assert_eq!(upserts_now[1].key, group_of(GROUP_TWO));
    assert_eq!(upserts_now[1].parent, Patch::Set(conv()));
    assert_eq!(
        *upserts_now[1],
        {
            let mut only_the_parent = ConversationPatch::new(group_of(GROUP_TWO));
            only_the_parent.parent = Patch::Set(conv());
            only_the_parent
        },
        "the subgroup is told its parent and nothing else"
    );
    assert_eq!(counts.parents_released, 1);
    assert!(row_of(&store, GROUP_TWO).await.parent_stated);

    // The community fetched again: nothing more is said of the subgroup.
    let (events, counts) = answer(&store, vec![found_group(GROUP)]).await;
    assert_eq!(upserts(&events).len(), 1, "{events:?}");
    assert_eq!(counts.parents_released, 0);
}

/// Q3: a community the core already has (Kourier stated it before) is named at once; so is one stated EARLIER in the same
/// answer; one stated LATER in the same answer is told after it, in the same zone.
/// Kills "the core's own knowledge is not asked" and "the answer's order is not looked at".
#[tokio::test]
async fn a_known_or_just_stated_community_is_named_at_once() {
    // The core has the community chat.
    let (_dir, owner) = open();
    let store = owner.store();
    core_knows(&store, GROUP).await;
    let (events, counts) = answer(&store, vec![subgroup_of(GROUP_TWO, GROUP)]).await;
    let [patch] = upserts(&events).try_into().expect("the subgroup's upsert");
    assert_eq!(patch.parent, Patch::Set(conv()));
    assert_eq!(counts.parents_waiting, 0);
    assert!(row_of(&store, GROUP_TWO).await.parent_stated);

    // The community comes first in the answer.
    let (_dir, owner) = open();
    let store = owner.store();
    let (events, counts) = answer(
        &store,
        vec![found_group(GROUP), subgroup_of(GROUP_TWO, GROUP)],
    )
    .await;
    let all = upserts(&events);
    assert_eq!(all.len(), 2, "{events:?}");
    assert_eq!(all[1].key, group_of(GROUP_TWO));
    assert_eq!(all[1].parent, Patch::Set(conv()));
    assert_eq!((counts.parents_waiting, counts.parents_released), (0, 0));

    // The subgroup comes first: stated without, then told after the community in the same zone.
    let (_dir, owner) = open();
    let store = owner.store();
    let (events, counts) = answer(
        &store,
        vec![subgroup_of(GROUP_TWO, GROUP), found_group(GROUP)],
    )
    .await;
    let all = upserts(&events);
    assert_eq!(all.len(), 3, "{events:?}");
    assert_eq!(
        all.iter()
            .map(|patch| patch.key.clone())
            .collect::<Vec<_>>(),
        [group_of(GROUP_TWO), conv(), group_of(GROUP_TWO)]
    );
    assert_eq!(all[0].parent, Patch::Unchanged);
    assert_eq!(all[2].parent, Patch::Set(conv()));
    assert_eq!((counts.parents_waiting, counts.parents_released), (1, 1));
    assert!(row_of(&store, GROUP_TWO).await.parent_stated);
}

/// W6a2 B8a (D8): the actor's name that a notice names is stated by the zone only when the core was not told it, whether or not
/// the notice has acts of its own; the upsert of the group still sits between the name and the line.
/// Mutant: the zone states every name the notice names.
#[tokio::test]
async fn the_actors_name_is_stated_once_and_again_only_when_it_changes() {
    let (_dir, owner) = open();
    let store = owner.store();
    let name = |text: &str| crate::names::push_name_event(lid(ADA), text.to_owned());
    let with_name = |text: &str, acts: Vec<Act>| Noticed {
        conv: conv(),
        events: vec![name(text), line()],
        acts,
    };
    let title = || Act::Title {
        title: "x".to_owned(),
        at_s: 9,
    };
    // With acts: the name, the group's upsert, the line.
    let events = run(&store, with_name("Ada Example", vec![title()]), NOW).await;
    assert!(matches!(
        events.as_slice(),
        [
            BackendEvent::ContactUpsert(_),
            BackendEvent::ConversationUpsert(_),
            BackendEvent::Message(_)
        ]
    ));
    // The same name again, a notice with no act at all: the line alone.
    let events = run(&store, with_name("Ada Example", vec![]), NOW).await;
    assert_eq!(events, vec![line()], "the core has the name");
    // A new name, no act: stated again.
    let events = run(&store, with_name("Ada Renamed", vec![]), NOW).await;
    assert_eq!(events, vec![name("Ada Renamed"), line()]);
    owner.close();
}
