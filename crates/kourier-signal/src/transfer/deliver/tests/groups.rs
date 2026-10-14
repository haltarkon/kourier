//! Step S4b: the delivery of group chats. The `groups` row is written in the chats zone
//! WITHOUT the snapshot, with the flags the Storage Service merge compares, 'left' for a
//! group we left and the hash of the features sent; a refresh is queued for every group (a
//! left one too) and the job runner is woken; a row that has the server's state already
//! gets no title, members or timer; a redo forgets and sends a group's features again. A
//! real `backend.db` with an account behind it.

use kourier_db::OutboxSource as _;
use kourier_model::{Member, MemberRole};

use super::frames::{append_the_pins, deliver_the_chats, events, upsert_of};
use super::review::{fed_from, reading};
use super::*;
use crate::actor::testing;
use crate::groups::{self, GroupKeys};
use crate::receive::tests::fixture;
use crate::transfer::stage::{ChatFrame, GroupMembership, MasterKey, StagedGroup};

fn keys(i: u8) -> GroupKeys {
    GroupKeys::from_master([i; 32])
}

fn staged(i: u8, membership: GroupMembership) -> StagedGroup {
    StagedGroup {
        master_key: MasterKey([i; 32]),
        title: Some(format!("Group {i}")),
        members: Some(vec![Member {
            user: peer(0),
            role: MemberRole::Admin,
        }]),
        membership,
        blocked: false,
        whitelisted: true,
    }
}

/// A stage of one person and the group chats `groups` (each with one loud row, pinned in
/// the order given when `pinned`).
fn stage_of_groups(dir: &std::path::Path, groups: &[(StagedGroup, ChatFrame, bool)]) -> Recorded {
    let mut writer = StageWriter::create(dir, &key(), StageLimits::default()).unwrap();
    let contact = StagedContact {
        user: peer(0),
        aci: Some(super::review::aci(0)),
        pni: None,
        e164: None,
        profile_key: None,
        profile_sharing: true,
        blocked: false,
        hidden: false,
        unregistered_at: None,
        profile_name: None,
        system_name: None,
        nickname: None,
    };
    writer.contact(0, &contact).unwrap();
    let mut counters = Vec::new();
    for (i, (group, frame, pinned)) in groups.iter().enumerate() {
        let i = i as u32;
        let chat = StagedChat {
            kind: crate::transfer::stage::StagedKind::Group(Box::new(group.clone())),
            frame: *frame,
        };
        let conv = GroupKeys::from_master(group.master_key.0).conv_key();
        writer
            .chat(i, &conv, &chat, pinned.then_some(i + 1))
            .unwrap();
        let mut msg = message(i, u64::from(i) + 1, 10);
        msg.conv = conv;
        writer.item(i, u64::from(i) + 1, &msg).unwrap();
        counters.push(ChatCounters {
            idx: i,
            n_rows: 1,
            n_loud: 1,
            last_at: 1_000 * (i64::from(i) + 1),
        });
    }
    let frames = groups.len() as u64 + 7;
    let totals = writer.commit(&counters, frames).unwrap();
    Recorded {
        messages: totals.messages,
        chats: totals.chats,
        frames,
    }
}

fn row(db: &ZoneDb, i: u8) -> groups::GroupRow {
    db.read(|c| groups::load_by_master(c, &[i; 32]))
        .unwrap()
        .expect("the group row")
}

fn refreshes(db: &ZoneDb) -> i64 {
    db.read(|c| -> Result<i64> {
        Ok(c.query_row(
            "SELECT count(*) FROM jobs WHERE kind = 'group_refresh'",
            [],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_group_is_delivered_with_the_archives_title_members_and_timer_and_its_row_without_a_snapshot()
 {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[
            (
                staged(1, GroupMembership::Member),
                ChatFrame {
                    archived: true,
                    marked_unread: true,
                    muted_until: Some(i64::MAX),
                    timer_secs: Some(3_600),
                    timer_version: 1,
                },
                true,
            ),
            (
                staged(2, GroupMembership::Left),
                ChatFrame::default(),
                false,
            ),
            (
                staged(3, GroupMembership::Invited),
                ChatFrame::default(),
                false,
            ),
            (
                staged(4, GroupMembership::Requesting),
                ChatFrame::default(),
                false,
            ),
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    deliver_the_chats(&shared, dir.path(), recorded);
    let all = events(&shared.db);

    let first = upsert_of(&all, &keys(1).conv_key());
    assert_eq!(first["title"]["Set"], "Group 1");
    assert_eq!(first["members"]["upsert"][0]["role"], "Admin");
    assert_eq!(first["timer"]["Set"]["secs"], 3_600);
    assert_eq!(first["archived"], true);
    assert_eq!(first["marked_unread"], true);
    assert_eq!(first["muted_until"]["Set"], i64::MAX);
    assert_eq!(
        first["features"]["cannot_send"], "StillLoading",
        "a member's group before its first fetch is loading, not 'no longer a member'"
    );
    let r1 = row(&shared.db, 1);
    assert!(
        r1.snapshot.is_none() && r1.revision.is_none(),
        "no snapshot stored"
    );
    assert!(r1.features_hash.is_some(), "the core has the group");
    assert!(r1.archived && r1.marked_unread && r1.muted_until == Some(i64::MAX));
    assert!(
        shared
            .db
            .read(|c| groups::whitelisted(c, &keys(1).id))
            .unwrap()
    );
    assert_eq!(r1.membership, groups::Membership::Unknown);

    let second = upsert_of(&all, &keys(2).conv_key());
    assert_eq!(second["features"]["cannot_send"], "NotMember");
    assert_eq!(row(&shared.db, 2).membership, groups::Membership::Left);

    // Invited: the row says so from the start (not 'unknown'), and the chat says what to do
    // (accept the invitation on the phone), not "no longer a member".
    let third = upsert_of(&all, &keys(3).conv_key());
    assert_eq!(third["request_pending"], true, "invited, as on the phone");
    assert_eq!(third["features"]["cannot_send"], "InvitationPending");
    assert_eq!(row(&shared.db, 3).membership, groups::Membership::Pending);

    // Asked to join: waiting for an admin, which is not a pending message request.
    let fourth = upsert_of(&all, &keys(4).conv_key());
    assert!(!fourth.contains_key("request_pending"), "{fourth:?}");
    assert_eq!(fourth["features"]["cannot_send"], "JoinRequestPending");
    assert_eq!(
        row(&shared.db, 4).membership,
        groups::Membership::Requesting
    );

    // One refresh per group, the left one too (D10).
    assert_eq!(refreshes(&shared.db), 4);

    // The pin list names the group, which the core has.
    let pins = append_the_pins(&shared, dir.path(), recorded);
    assert_eq!(pins, vec![keys(1).conv_key()]);
}

/// Never over the server's state: a refresh that ran during the delivery stored the state;
/// a chats row that comes after it (a redo, a rebuild) carries no title, members or timer,
/// and leaves the row's flags as they stand.
#[tokio::test(flavor = "current_thread")]
async fn a_group_whose_state_was_fetched_gets_no_title_members_or_timer_from_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[(
            staged(1, GroupMembership::Member),
            ChatFrame {
                archived: true,
                timer_secs: Some(60),
                ..ChatFrame::default()
            },
            false,
        )],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let account = fixture.account.clone();
    // The server's state, as a fetch stores it; the Storage Service said "not archived".
    let snapshot = groups::Snapshot {
        revision: 9,
        title: "Server title".to_owned(),
        members: vec![groups::SnapMember {
            aci: uuid::Uuid::from(account.aci),
            admin: false,
            joined_at: 1,
        }],
        ..groups::Snapshot::default()
    };
    shared
        .db
        .zone(|tx| -> Result<()> {
            groups::ensure(tx, &keys(1), NOW)?;
            tx.execute(
                "UPDATE groups SET revision = 9, state = ?2, membership = 'member',
                   archived = 0 WHERE master_key = ?1",
                (&[1u8; 32][..], snapshot.to_json()?),
            )?;
            Ok(())
        })
        .unwrap();
    deliver_the_chats(&shared, dir.path(), recorded);
    let upsert = upsert_of(&events(&shared.db), &keys(1).conv_key());
    // The server's title, members and timer, never the archive's ("Group 1", an admin, 60 s):
    // a rebuilt core gets a whole group whatever the refresh does.
    assert_eq!(upsert["title"]["Set"], "Server title", "{upsert:?}");
    let members = upsert["members"]["upsert"].as_array().unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0]["role"], "Member");
    assert_eq!(upsert["timer"], "Clear");
    assert_eq!(
        upsert["archived"], false,
        "the row's flag, not the archive's"
    );
    assert!(
        upsert["features"]["cannot_send"].is_null(),
        "the row's features: a member can send"
    );
    let r = row(&shared.db, 1);
    assert!(!r.archived, "the row's flags are left alone");
    assert_eq!(r.snapshot.unwrap().title, "Server title");
    assert_eq!(refreshes(&shared.db), 1);
}

/// A chats row the core refuses is taken back: the groups it named are no longer "in the
/// core", and the singles that are appended again state the features again.
#[tokio::test(flavor = "current_thread")]
async fn a_redo_of_a_row_of_groups_forgets_and_sends_their_features_again() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[
            (
                staged(1, GroupMembership::Member),
                ChatFrame::default(),
                true,
            ),
            (
                staged(2, GroupMembership::Member),
                ChatFrame::default(),
                true,
            ),
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let chats_seq = deliver_the_chats(&shared, dir.path(), recorded);
    assert!(row(&shared.db, 1).features_hash.is_some());
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    assert!(row(&shared.db, 1).features_hash.is_none());
    assert!(row(&shared.db, 2).features_hash.is_none());
    let before = super::review::outbox(&shared.db).len();
    let mut singles = Vec::new();
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.place, Place::Redo { .. }) {
            singles.push(append(&shared, chunk, NOW).unwrap().seq.unwrap());
        }
    }
    assert_eq!(singles.len(), 2);
    let again: Vec<BackendEvent> = super::review::outbox(&shared.db)
        .into_iter()
        .skip(before)
        .flat_map(|(_, e)| e)
        .collect();
    for i in [1, 2] {
        assert!(upsert_of(&again, &keys(i).conv_key()).contains_key("features"));
        assert!(row(&shared.db, i).features_hash.is_some());
    }
    // One of them is dropped for good: it is left out of the pins.
    shared.db.quarantine(singles[0], "refused again").unwrap();
    settle(&shared.db, MAX_REFUSED_RUN).unwrap();
    let pins = append_the_pins(&shared, dir.path(), recorded);
    assert_eq!(pins.len(), 1, "{pins:?}");
    // Each refresh is queued once, however often the zone ran.
    assert_eq!(refreshes(&shared.db), 2);
}

/// A rebuilt core (`StartMode::Rebuild` during a delivery) has no group and the refresh may
/// never come for a group that is left (the rebuild queues none for it): the chats row says
/// what the STORED state says, a whole group, not a bare one.
#[tokio::test(flavor = "current_thread")]
async fn a_group_with_a_state_sends_the_states_title_members_and_timer_even_when_we_left() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[
            (
                staged(1, GroupMembership::Member),
                ChatFrame {
                    timer_secs: Some(60),
                    ..ChatFrame::default()
                },
                false,
            ),
            (
                staged(2, GroupMembership::Member),
                ChatFrame::default(),
                false,
            ),
        ],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    // We were removed after a fetch: the row is 'left' and holds the state without us.
    let snapshot = |title: &str, timer_secs| groups::Snapshot {
        revision: 9,
        title: title.to_owned(),
        timer_secs,
        members: vec![
            groups::SnapMember {
                aci: super::review::aci(5),
                admin: true,
                joined_at: 1,
            },
            groups::SnapMember {
                aci: super::review::aci(6),
                admin: false,
                joined_at: 2,
            },
        ],
        ..groups::Snapshot::default()
    };
    shared
        .db
        .zone(|tx| -> Result<()> {
            for (i, state) in [(1u8, snapshot("Old title", 3_600)), (2, snapshot("", 0))] {
                groups::ensure(tx, &keys(i), NOW)?;
                tx.execute(
                    "UPDATE groups SET revision = 9, state = ?2, membership = 'left'
                     WHERE master_key = ?1",
                    (&[i; 32][..], state.to_json()?),
                )?;
            }
            Ok(())
        })
        .unwrap();
    deliver_the_chats(&shared, dir.path(), recorded);
    let all = events(&shared.db);
    let upsert = upsert_of(&all, &keys(1).conv_key());
    assert_eq!(upsert["title"]["Set"], "Old title");
    assert_eq!(upsert["timer"]["Set"]["secs"], 3_600);
    let members = upsert["members"]["upsert"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["role"], "Admin");
    assert_eq!(members[1]["role"], "Member");
    assert_eq!(upsert["members"]["full"], true);
    assert_eq!(upsert["features"]["cannot_send"], "NotMember");
    assert_eq!(upsert["request_pending"], false);
    // A stored state without a title or a timer says that there is none.
    let bare = upsert_of(&all, &keys(2).conv_key());
    assert_eq!(bare["title"], "Clear");
    assert_eq!(bare["timer"], "Clear");
}

/// After the first fetch found an invitation over (a 403: Desktop's pre-join fetch fails the
/// same way and the group is left; a 404: it is gone) a redo of the chats row is the stage's
/// word again, and the stage still says "invited": it must not offer the invitation again,
/// and the server's verdict stays in the row.
#[tokio::test(flavor = "current_thread")]
async fn a_redo_after_the_invitation_was_found_over_does_not_offer_it_again() {
    use crate::groups::apply::{Outcome, Update, apply};
    for (outcome, verdict) in [
        (
            Outcome::NotMember { change: None },
            groups::Membership::Left,
        ),
        (Outcome::Gone, groups::Membership::Gone),
    ] {
        let dir = tempfile::tempdir().unwrap();
        // Two groups, so that the row is taken back as singles and not dropped.
        let recorded = stage_of_groups(
            dir.path(),
            &[
                (
                    staged(1, GroupMembership::Invited),
                    ChatFrame::default(),
                    false,
                ),
                (
                    staged(2, GroupMembership::Member),
                    ChatFrame::default(),
                    false,
                ),
            ],
        );
        let fixture = fixture();
        reading(&fixture.db, recorded);
        let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = test.shared;
        let chats_seq = deliver_the_chats(&shared, dir.path(), recorded);
        assert_eq!(row(&shared.db, 1).membership, groups::Membership::Pending);
        // The first fetch: the server does not give an invited group's state.
        let account = fixture.account.clone();
        shared
            .db
            .zone(|tx| {
                apply(
                    tx,
                    &account,
                    dir.path(),
                    Update {
                        keys: keys(1),
                        outcome,
                    },
                    None,
                    NOW,
                )
            })
            .unwrap();
        assert_eq!(row(&shared.db, 1).membership, verdict);
        // The row is refused and comes again, from the stage that still says "invited".
        shared
            .db
            .quarantine(chats_seq, "refused by a test")
            .unwrap();
        assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
        let before = super::review::outbox(&shared.db).len();
        for chunk in fed_from(&shared.db, dir.path(), recorded) {
            if matches!(chunk.place, Place::Redo { .. }) {
                append(&shared, chunk, NOW).unwrap();
            }
        }
        let again: Vec<BackendEvent> = super::review::outbox(&shared.db)
            .into_iter()
            .skip(before)
            .flat_map(|(_, e)| e)
            .collect();
        let upsert = upsert_of(&again, &keys(1).conv_key());
        assert_eq!(upsert["request_pending"], false, "{verdict:?}: {upsert:?}");
        assert_eq!(upsert["features"]["cannot_send"], "NotMember");
        assert_eq!(row(&shared.db, 1).membership, verdict, "the verdict stands");
    }
}

/// What the server said stands over the archive: a 403 that came before the import's row
/// (the group was never told to the core) keeps 'left' for a group the archive calls ours.
#[tokio::test(flavor = "current_thread")]
async fn the_servers_verdict_before_the_import_stands_over_the_archives_word() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[(
            staged(1, GroupMembership::Member),
            ChatFrame::default(),
            false,
        )],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    shared
        .db
        .zone(|tx| -> Result<()> {
            groups::ensure(tx, &keys(1), NOW)?;
            tx.execute(
                "UPDATE groups SET membership = 'left' WHERE master_key = ?1",
                [&[1u8; 32][..]],
            )?;
            Ok(())
        })
        .unwrap();
    deliver_the_chats(&shared, dir.path(), recorded);
    assert_eq!(row(&shared.db, 1).membership, groups::Membership::Left);
    let upsert = upsert_of(&events(&shared.db), &keys(1).conv_key());
    assert_eq!(upsert["features"]["cannot_send"], "NotMember");
}

/// The chats row with a group queues its refresh and wakes the job runner when its zone
/// commits: without the wake the groups stay "still loading" until something else wakes it
/// (at the latest the end of the transfer).
#[tokio::test(flavor = "current_thread")]
async fn a_chats_row_with_a_group_wakes_the_job_runner() {
    use futures::FutureExt as _;
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_groups(
        dir.path(),
        &[(
            staged(1, GroupMembership::Member),
            ChatFrame::default(),
            false,
        )],
    );
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    let mut woken = false;
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        match chunk.payload {
            Payload::Contacts(_) => {
                // The people's profile fetches wake it too: let that go.
                append(&shared, chunk, NOW).unwrap();
                while shared.jobs_wake.notified().now_or_never().is_some() {}
            }
            Payload::Chats(_) => {
                append(&shared, chunk, NOW).unwrap();
                woken = shared.jobs_wake.notified().now_or_never().is_some();
                break;
            }
            _ => break,
        }
    }
    assert!(
        woken,
        "the zone of a row of chats with a group wakes the runner"
    );
    assert_eq!(refreshes(&shared.db), 1);
}
