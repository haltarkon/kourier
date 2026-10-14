//! Groups without a server: keys and ids, the snapshot format, the core's member list and the
//! group conversation features.

use kourier_model::{CannotSend, ConvKey, FormatKind, MemberRole};
use libsignal_core::Aci;
use uuid::Uuid;

use super::apply::member_list;
use super::{
    GroupKeys, Membership, SnapMember, SnapPending, Snapshot, conv_key_of_id, job_key,
    parse_job_key, system_local,
};
use crate::features::group_features;

fn keys() -> GroupKeys {
    GroupKeys::from_master([7; 32])
}

#[test]
fn keys_ids_and_lanes_are_stable_and_never_show_the_master_key() {
    let a = keys();
    let b = GroupKeys::from_slice(&[7; 32]).expect("32 bytes");
    assert_eq!(a.id, b.id);
    assert!(GroupKeys::from_slice(&[7; 31]).is_none());
    // The core's key is built from the group id, never the master key.
    let ConvKey::Group(key) = a.conv_key() else {
        panic!("a group key");
    };
    assert!(key.starts_with("gv2:"));
    assert_eq!(a.conv_key(), conv_key_of_id(&a.id));
    assert!(a.conv_key().validate().is_ok());
    assert_eq!(a.lane(), crate::receive::group_lane(&[7; 32]));
    assert!(!format!("{a:?}").contains("0707"));
    // Refresh job keys round-trip; malformed ones do not parse.
    assert_eq!(parse_job_key(&job_key(&a.id)).as_deref(), Some(&a.id[..]));
    assert_eq!(parse_job_key("abc"), None);
    assert_eq!(parse_job_key("zz"), None);
    // System message ids name group, revision and index.
    let local = system_local(&a.id, 3, 1);
    assert!(local.starts_with("gv2:") && local.ends_with(":3:1"));
    assert_ne!(local, system_local(&a.id, 3, 0));
    assert_ne!(
        local,
        system_local(&GroupKeys::from_master([8; 32]).id, 3, 1)
    );
}

fn member(n: u128, admin: bool) -> SnapMember {
    SnapMember {
        aci: Uuid::from_u128(n),
        admin,
        joined_at: 0,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        revision: 4,
        title: "Climbing".to_owned(),
        members: vec![member(1, true), member(2, false)],
        pending: vec![SnapPending {
            service_id: Uuid::from_u128(3).to_string(),
            admin: false,
        }],
        requesting: vec![Uuid::from_u128(4)],
        ..Snapshot::default()
    }
}

#[test]
fn snapshots_round_trip_and_answer_membership() {
    let snapshot = snapshot();
    let parsed = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
    assert_eq!(parsed, snapshot);
    assert!(parsed.is_member(Aci::from(Uuid::from_u128(1))));
    assert!(!parsed.is_member(Aci::from(Uuid::from_u128(3))));
    assert!(parsed.is_pending(&Uuid::from_u128(3).to_string()));
    assert!(Snapshot::from_json(b"not json").is_err());
}

#[test]
fn the_member_list_is_a_full_snapshot_with_roles() {
    let list = member_list(&snapshot());
    assert!(list.full);
    let roles: Vec<MemberRole> = list.upsert.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            MemberRole::Admin,
            MemberRole::Member,
            MemberRole::Pending,
            MemberRole::Requesting
        ]
    );
}

#[test]
fn group_features_say_why_we_cannot_send() {
    let fixture = crate::receive::tests::fixture();
    let account = &fixture.account;
    let mut snapshot = snapshot();
    snapshot.members.push(SnapMember {
        aci: account.aci.into(),
        admin: false,
        joined_at: 0,
    });
    let limits = crate::features::Limits::with_max_bytes(1000);
    let member = group_features(Some(&snapshot), Membership::Member, account, &limits);
    assert_eq!(member.format_support(FormatKind::Mention).level(), 2);
    assert_eq!(member.cannot_send, None, "members write to the group (S11)");
    assert_eq!(
        group_features(Some(&snapshot), Membership::Left, account, &limits).cannot_send,
        Some(CannotSend::NotMember)
    );
    snapshot.announcements_only = true;
    assert_eq!(
        group_features(Some(&snapshot), Membership::Member, account, &limits).cannot_send,
        Some(CannotSend::AdminsOnly)
    );
    assert_eq!(
        group_features(None, Membership::Gone, account, &limits).cannot_send,
        Some(CannotSend::NotMember)
    );
}

/// Step S4b: a group the core has from a history import before its first fetch (no
/// snapshot, membership unknown) is "still loading", a translatable reason; without a
/// snapshot a group that is left or gone stays "not a member". An invitation and a request to
/// join have reasons of their own: neither is "no longer a member".
#[test]
fn a_group_without_a_snapshot_is_still_loading_unless_we_are_out_of_it() {
    let fixture = crate::receive::tests::fixture();
    let account = &fixture.account;
    let limits = crate::features::Limits::with_max_bytes(1000);
    assert_eq!(
        group_features(None, Membership::Unknown, account, &limits).cannot_send,
        Some(CannotSend::StillLoading)
    );
    for out in [Membership::Left, Membership::Gone] {
        assert_eq!(
            group_features(None, out, account, &limits).cannot_send,
            Some(CannotSend::NotMember),
            "{out:?}"
        );
    }
    assert_eq!(
        group_features(None, Membership::Pending, account, &limits).cannot_send,
        Some(CannotSend::InvitationPending)
    );
    assert_eq!(
        group_features(None, Membership::Requesting, account, &limits).cannot_send,
        Some(CannotSend::JoinRequestPending)
    );
}

/// An invitation says so also when the server's state is known (the live path): "accept it
/// on your phone", not "you are no longer a member"; a group that was ended is not worth
/// accepting.
#[test]
fn an_invitation_and_a_request_to_join_have_their_own_reasons_with_a_snapshot_too() {
    let fixture = crate::receive::tests::fixture();
    let account = &fixture.account;
    let limits = crate::features::Limits::with_max_bytes(1000);
    let mut snapshot = snapshot();
    for (membership, want) in [
        (Membership::Pending, CannotSend::InvitationPending),
        (Membership::Requesting, CannotSend::JoinRequestPending),
        (Membership::Left, CannotSend::NotMember),
        (Membership::Gone, CannotSend::NotMember),
    ] {
        assert_eq!(
            group_features(Some(&snapshot), membership, account, &limits).cannot_send,
            Some(want),
            "{membership:?}"
        );
    }
    snapshot.terminated = true;
    for membership in [Membership::Pending, Membership::Requesting] {
        assert_eq!(
            group_features(Some(&snapshot), membership, account, &limits).cannot_send,
            Some(CannotSend::NotMember),
            "an ended group, {membership:?}"
        );
    }
}

/// `membership_of` knows the fourth case of the archive: we asked to join (only among the
/// requesting), and an invitation by our PNI.
#[test]
fn our_membership_by_aci_pni_and_a_request_to_join() {
    use super::apply::membership_of;
    let aci = Aci::from(Uuid::from_u128(10));
    let pni = libsignal_core::Pni::from(Uuid::from_u128(11));
    let mut s = Snapshot::default();
    assert_eq!(membership_of(&s, aci, pni), Membership::Left);
    s.requesting.push(Uuid::from_u128(10));
    assert_eq!(membership_of(&s, aci, pni), Membership::Requesting);
    s.pending.push(SnapPending {
        service_id: libsignal_core::ServiceId::from(pni).service_id_string(),
        admin: false,
    });
    assert_eq!(membership_of(&s, aci, pni), Membership::Pending);
    s.members.push(member(10, false));
    assert_eq!(membership_of(&s, aci, pni), Membership::Member);
    // The new value is stored and read back (migration 0020).
    let db = crate::receive::tests::fixture().db;
    db.zone(|tx| -> crate::error::Result<()> {
        super::ensure(tx, &keys(), 1)?;
        tx.execute(
            "UPDATE groups SET membership = ?2 WHERE master_key = ?1",
            (&[7u8; 32][..], Membership::Requesting.as_str()),
        )?;
        Ok(())
    })
    .unwrap();
    let row = db
        .read(|c| super::load_by_master(c, &[7; 32]))
        .unwrap()
        .unwrap();
    assert_eq!(row.membership, Membership::Requesting);
}

/// Step S4b: at a start, every imported group that was never fetched and is not left or gone
/// gets its refresh again (its job may have been dropped), once.
#[test]
fn a_start_queues_the_refresh_of_an_imported_group_that_was_never_fetched() {
    let db = crate::receive::tests::fixture().db;
    let group = |i: u8, membership: &str, hash: Option<i64>, state: Option<&[u8]>| {
        let keys = GroupKeys::from_master([i; 32]);
        db.zone(|tx| -> crate::error::Result<()> {
            super::ensure(tx, &keys, 1)?;
            tx.execute(
                "UPDATE groups SET membership = ?2, features_hash = ?3, state = ?4
                 WHERE master_key = ?1",
                (&[i; 32][..], membership, hash, state),
            )?;
            Ok(())
        })
        .unwrap();
        job_key(&keys.id)
    };
    let stranded = group(1, "unknown", Some(5), None);
    let invited = group(2, "pending", Some(5), None);
    let left = group(3, "left", Some(5), None);
    let fetched = group(4, "member", Some(5), Some(b"{}"));
    let not_in_core = group(5, "unknown", None, None);
    let jobs = || -> Vec<String> {
        db.read(|c| -> crate::error::Result<Vec<String>> {
            Ok(
                c.prepare("SELECT key FROM jobs WHERE kind = 'group_refresh' ORDER BY key")?
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?,
            )
        })
        .unwrap()
    };
    for _ in 0..2 {
        db.zone(|tx| crate::maintenance::enqueue_stranded_group_refreshes(tx, 10))
            .unwrap();
    }
    let mut want = vec![stranded, invited];
    want.sort();
    assert_eq!(jobs(), want);
    for other in [left, fetched, not_in_core] {
        assert!(!jobs().contains(&other));
    }
}

/// Step S4b: the import says "invited" from the phone's snapshot and the core has the group
/// (`features_hash` set, no state). The first fetch states `request_pending` from the
/// server's state, also when that is no longer an invitation, and no other roaming flag.
#[test]
fn the_first_fetch_of_an_imported_group_corrects_its_request() {
    use super::apply::{AvatarUpdate, Outcome, Update, apply};
    use super::decode::DecodedState;
    use kourier_backend_api::BackendEvent;
    let fixture = crate::receive::tests::fixture();
    let account = fixture.account.clone();
    let db = fixture.db;
    let dir = tempfile::tempdir().unwrap();
    db.zone(|tx| -> crate::error::Result<()> {
        super::ensure(tx, &keys(), 1)?;
        tx.execute(
            "UPDATE groups SET features_hash = 5, archived = 1 WHERE master_key = ?1",
            [&[7u8; 32][..]],
        )?;
        Ok(())
    })
    .unwrap();
    let mut state = snapshot();
    state
        .members
        .push(member(Uuid::from(account.aci).as_u128(), false));
    let update = Update {
        keys: keys(),
        outcome: Outcome::Current {
            state: Box::new(DecodedState {
                snapshot: state,
                profile_keys: Vec::new(),
            }),
            changes: Vec::new(),
            endorsements: None,
            avatar: AvatarUpdate::Unchanged,
        },
    };
    let applied = db
        .zone(|tx| apply(tx, &account, dir.path(), update, None, 10))
        .unwrap();
    let patch = applied
        .events
        .iter()
        .find_map(|e| match e {
            BackendEvent::ConversationUpsert(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        patch.request_pending,
        Some(false),
        "a member now, no request"
    );
    assert_eq!(
        patch.archived, None,
        "the core has the group: no roaming flag again"
    );
    assert!(
        !applied
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::Message(_))),
        "a first fetch makes no notices"
    );
}

/// What the server's answer `outcome` does to the group row `membership` (stored as the
/// import leaves a group before its first fetch: no state, the core has it), as the events
/// of the zone and the membership the row ends with.
fn answered(
    fixture: &crate::receive::tests::Fixture,
    membership: &str,
    outcome: super::apply::Outcome,
) -> (Vec<kourier_backend_api::BackendEvent>, Membership) {
    use super::apply::{Update, apply};
    let db = &fixture.db;
    db.zone(|tx| -> crate::error::Result<()> {
        tx.execute("DELETE FROM groups", [])?;
        super::ensure(tx, &keys(), 1)?;
        tx.execute(
            "UPDATE groups SET features_hash = 5, membership = ?2 WHERE master_key = ?1",
            (&[7u8; 32][..], membership),
        )?;
        Ok(())
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let applied = db
        .zone(|tx| {
            apply(
                tx,
                &fixture.account,
                dir.path(),
                Update {
                    keys: keys(),
                    outcome,
                },
                None,
                10,
            )
        })
        .unwrap();
    let row = db
        .read(|c| super::load_by_master(c, &[7; 32]))
        .unwrap()
        .unwrap();
    (applied.events, row.membership)
}

fn the_upsert(events: &[kourier_backend_api::BackendEvent]) -> &kourier_model::ConversationPatch {
    events
        .iter()
        .find_map(|e| match e {
            kourier_backend_api::BackendEvent::ConversationUpsert(p) => Some(p),
            _ => None,
        })
        .expect("an upsert")
}

/// The first answer about an imported group when it is NOT a state (403 or 404): the real
/// server answers a non-member so, while the mock answers 200 to anyone. An invitation that
/// ends here is not offered for acceptance any more (`request_pending` stated false, as
/// Desktop's `generateLeftGroupChanges` takes our pending entry out), a request to join
/// survives a 403 (the server gives a requester no state) and not a 404, and a group the
/// import already wrote 'left' stays silent on a 403.
#[test]
fn a_403_or_404_as_the_first_answer_about_an_imported_group() {
    use super::apply::Outcome;
    let fixture = crate::receive::tests::fixture();
    let not_member = || Outcome::NotMember { change: None };
    // (stored, 403: [membership after, request_pending stated], 404: [..]); `None` = no event.
    // What a row ends with: its membership and what the upsert states of the request.
    type After = (Membership, Option<bool>);
    let cases: [(&str, Option<After>, After); 4] = [
        // Never fetched, from a member's or an empty snapshot.
        (
            "unknown",
            Some((Membership::Left, Some(false))),
            (Membership::Gone, Some(false)),
        ),
        // Invited.
        (
            "pending",
            Some((Membership::Left, Some(false))),
            (Membership::Gone, Some(false)),
        ),
        // The archive said we left: the row says so already, a 403 changes nothing.
        ("left", None, (Membership::Gone, Some(false))),
        // Asked to join: a 403 leaves the request standing.
        ("requesting", None, (Membership::Gone, Some(false))),
    ];
    for (stored, forbidden, gone) in cases {
        let (events, after) = answered(&fixture, stored, not_member());
        match forbidden {
            Some((membership, pending)) => {
                assert_eq!(after, membership, "403 on {stored}");
                let patch = the_upsert(&events);
                assert_eq!(patch.request_pending, pending, "403 on {stored}");
                assert_eq!(
                    patch.features.as_ref().unwrap().cannot_send,
                    Some(CannotSend::NotMember),
                    "403 on {stored}"
                );
            }
            None => {
                assert!(events.is_empty(), "403 on {stored}: {events:?}");
                assert_eq!(after.as_str(), stored, "403 on {stored}");
            }
        }
        let (events, after) = answered(&fixture, stored, Outcome::Gone);
        assert_eq!(after, gone.0, "404 on {stored}");
        let patch = the_upsert(&events);
        assert_eq!(patch.request_pending, gone.1, "404 on {stored}");
        assert_eq!(
            patch.features.as_ref().unwrap().cannot_send,
            Some(CannotSend::NotMember),
            "404 on {stored}"
        );
    }
}

/// On the live path too: an invitation whose state was fetched ends with the same statement,
/// and a member who is removed states nothing about a request (there is none).
#[test]
fn an_invitation_that_ends_states_that_the_request_is_over_also_with_a_state() {
    use super::apply::{Outcome, Update, apply};
    let fixture = crate::receive::tests::fixture();
    let account = &fixture.account;
    let row = |membership: &str| {
        fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute("DELETE FROM groups", [])?;
                super::ensure(tx, &keys(), 1)?;
                let mut state = snapshot();
                if membership == "member" {
                    state
                        .members
                        .push(member(Uuid::from(account.aci).as_u128(), false));
                }
                tx.execute(
                    "UPDATE groups SET features_hash = 5, membership = ?2, state = ?3,
                       revision = 4 WHERE master_key = ?1",
                    (&[7u8; 32][..], membership, state.to_json()?),
                )?;
                Ok(())
            })
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        fixture
            .db
            .zone(|tx| {
                apply(
                    tx,
                    account,
                    dir.path(),
                    Update {
                        keys: keys(),
                        outcome: Outcome::NotMember { change: None },
                    },
                    None,
                    10,
                )
            })
            .unwrap()
    };
    let invited = row("pending");
    assert_eq!(the_upsert(&invited.events).request_pending, Some(false));
    let member = row("member");
    assert_eq!(
        the_upsert(&member.events).request_pending,
        None,
        "a member who was removed has no request to state"
    );
}

/// The live decoder and the history import (`transfer::map`, Android's converter) make the
/// same notice for the same change: our own accepted invitation is a join of ours
/// (`Joined`), like our own join by a link; a person who accepted is "X joined"; a member who
/// held an invitation and was promoted by somebody else was added by THAT editor.
#[test]
fn accepted_and_promoted_invitations_make_the_notices_the_import_makes() {
    use super::decode::actions;
    use crate::proto::group_change::Actions;
    use crate::proto::group_change::actions::{
        AddMemberAction, PromoteMemberPendingProfileKeyAction,
    };
    use kourier_model::SystemEvent;
    use libsignal_core::ServiceId;
    let keys = keys();
    let own = Aci::from(Uuid::from_u128(10));
    let (ada, bo) = (
        Aci::from(Uuid::from_u128(11)),
        Aci::from(Uuid::from_u128(12)),
    );
    let user = |aci: Aci| crate::service_id::aci_user_id(aci).unwrap();
    let enc = |aci: Aci| zkgroup::serialize(&keys.secret.encrypt_service_id(ServiceId::from(aci)));
    let promoted = |source: Aci, users: &[Aci]| Actions {
        version: 5,
        source_user_id: enc(source),
        promote_members_pending_profile_key: users
            .iter()
            .map(|u| PromoteMemberPendingProfileKeyAction {
                user_id: enc(*u),
                ..Default::default()
            })
            .collect(),
        ..Actions::default()
    };
    let events = |change: Actions| actions(&keys, &change, own).events;
    let added = |by: Option<Aci>, who: Aci| SystemEvent::MembersAdded {
        by: by.map(user),
        users: vec![user(who)],
    };

    // We accepted our invitation: we joined.
    assert_eq!(events(promoted(own, &[own])), [SystemEvent::Joined]);
    // Somebody else accepted theirs: "Ada joined", `by` is Ada.
    assert_eq!(events(promoted(ada, &[ada])), [added(Some(ada), ada)]);
    // An admin promoted a member who held an invitation: added by the editor, whoever the
    // editor is; we are "You".
    assert_eq!(events(promoted(bo, &[ada])), [added(Some(bo), ada)]);
    assert_eq!(events(promoted(own, &[ada])), [added(None, ada)]);
    assert_eq!(events(promoted(bo, &[own])), [added(Some(bo), own)]);
    // Several promoted by one editor: one notice each (the import folds them).
    assert_eq!(
        events(promoted(bo, &[ada, own])),
        [added(Some(bo), ada), added(Some(bo), own)]
    );

    // Unchanged: our own join by a link is a join of ours; anybody else's join by a link is
    // "X joined"; members added by an editor are added by the editor.
    let add = |source: Aci, who: Aci, link: bool| Actions {
        version: 6,
        source_user_id: enc(source),
        add_members: vec![AddMemberAction {
            added: Some(crate::proto::Member {
                user_id: enc(who),
                ..Default::default()
            }),
            join_from_invite_link: link,
        }],
        ..Actions::default()
    };
    assert_eq!(events(add(own, own, true)), [SystemEvent::Joined]);
    assert_eq!(events(add(ada, ada, true)), [added(Some(ada), ada)]);
    assert_eq!(events(add(bo, ada, false)), [added(Some(bo), ada)]);
    assert_eq!(events(add(own, ada, false)), [added(None, ada)]);
}

/// A title change reads "X removed the group name" only for a removal: an action without a
/// blob (Desktop reads it as `modifyTitle = {}`) or a blob that decrypts to an empty title.
/// A blob that is there and does not decrypt, belongs to another group, or holds something
/// other than a title says nothing about the name, so it makes no notice (a title notice
/// with an empty title would be a false "removed the group name"). The other actions of the
/// same change are not lost.
#[test]
fn a_title_that_does_not_decrypt_is_no_notice_and_a_removal_is() {
    use super::decode::actions;
    use crate::proto::group_attribute_blob::Content;
    use crate::proto::group_change::Actions;
    use crate::proto::group_change::actions::{
        ModifyDisappearingMessageTimerAction, ModifyTitleAction,
    };
    use kourier_model::SystemEvent;
    use libsignal_core::ServiceId;
    use prost::Message as _;
    let keys = keys();
    let other = GroupKeys::from_master([8; 32]);
    let own = Aci::from(Uuid::from_u128(10));
    let ada = Aci::from(Uuid::from_u128(11));
    let enc = |aci: Aci| zkgroup::serialize(&keys.secret.encrypt_service_id(ServiceId::from(aci)));
    let blob = |keys: &GroupKeys, content: Content| {
        let plain = crate::proto::GroupAttributeBlob {
            content: Some(content),
        }
        .encode_to_vec();
        keys.secret.encrypt_blob_with_padding([3; 32], &plain, 4)
    };
    let events = |source: Aci, title: Vec<u8>| {
        let change = Actions {
            version: 7,
            source_user_id: enc(source),
            modify_title: Some(ModifyTitleAction { title }),
            modify_disappearing_message_timer: Some(ModifyDisappearingMessageTimerAction {
                timer: blob(&keys, Content::DisappearingMessagesDuration(60)),
            }),
            ..Actions::default()
        };
        actions(&keys, &change, own).events
    };
    let renamed = |by: Option<Aci>, title: &str| SystemEvent::TitleChanged {
        by: by.map(|aci| crate::service_id::aci_user_id(aci).unwrap()),
        title: title.to_owned(),
    };
    let timer = |by: Option<Aci>| SystemEvent::TimerChanged {
        by: by.map(|aci| crate::service_id::aci_user_id(aci).unwrap()),
        timer: Some(std::time::Duration::from_secs(60)),
    };

    // A new name, and the two forms of a removal.
    assert_eq!(
        events(ada, blob(&keys, Content::Title("Book club".into()))),
        [renamed(Some(ada), "Book club"), timer(Some(ada))]
    );
    assert_eq!(
        events(ada, blob(&keys, Content::Title(String::new()))),
        [renamed(Some(ada), ""), timer(Some(ada))],
        "an empty title that decrypted is a removal"
    );
    assert_eq!(
        events(own, Vec::new()),
        [renamed(None, ""), timer(None)],
        "no blob at all is a removal"
    );

    // A blob that is there and says nothing about the name: no title notice, the rest stays.
    let garbage = vec![9_u8; 48];
    let mut tampered = blob(&keys, Content::Title("Book club".into()));
    tampered[0] ^= 1;
    for (why, title) in [
        ("not a ciphertext", garbage),
        ("a flipped bit", tampered),
        (
            "another group's key",
            blob(&other, Content::Title("Book club".into())),
        ),
        (
            "a blob of another kind",
            blob(&keys, Content::DescriptionText("Book club".into())),
        ),
    ] {
        assert_eq!(events(ada, title), [timer(Some(ada))], "{why}");
    }
}
