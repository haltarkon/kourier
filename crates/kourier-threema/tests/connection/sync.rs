//! TH5 A7a: THE CONTACT AND GROUP SYNC through the real actor against the offline fake. The phone reflects
//! `ContactSync` / `GroupSync` deltas; the core side reads the LITERAL events after the zone (a contact's
//! `ContactUpsert` with its names set and cleared, hidden by the acquaintance level; a group's
//! `ConversationUpsert` with its title and sorted members, the notices of an update, `ConversationRemoved` of
//! a delete); the BLOBs in the file changed field by field (H6: the stored key stays; members replaced); a
//! text after a `GroupSync.Create` in the SAME batch is mapped (the zone's read sees the earlier write); and
//! what a Rebuild states over the file afterwards (`store::stated_events`, the rows `ready_join` writes) is
//! written out literally (L4). Each test is a parent/child `unshare -rn` pair like its neighbours.
//!
//! TH5b B4 (decision R13): the SETTINGS, PROFILE and LIST syncs the same way: a settings delta states
//! `AccountSettings` and the `ContactUpsert` of every stored contact whose blocked state changed (and of no
//! other); a profile delta states `AccountReady` with the nickname, an empty nickname clears the stored one
//! and states no name (F5); a list's Create / Update / Delete is stored and states nothing; a Rebuild after
//! them states what they left, written out.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    AccountSettingsPatch, Body, ContactPatch, ConvKey, ConversationPatch, Member, MemberList,
    MemberRole, Origin, Patch, Redacted, SystemEvent, UserId,
};
use kourier_threema::store;
use libthreema::protobuf::d2d_sync;
use prost::Message as _;
use tracing::Level;

use super::harness::{self, H, ME, scene};
use super::reflect::{PEER, PHONE, connect, phone_of, wait_acks};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::envelope::{
    Author, ContactDelta, GroupDelta, MemberChange, Msg, Reflect as _,
};
use crate::support::warns::Warns;

/// A contact the phone creates (synthetic).
const NEWC: &str = "A7SNEWC1";
/// The groups' creator (a contact), synthetic.
const CREATOR: &str = "A7SCRTR1";
/// A member the phone adds (synthetic).
const PEER2: &str = "A7SPEER2";
/// The seeded group (deleted by the test) and the group the phone creates.
const OLD_GROUP: u64 = 0x0102_0304_0506_0708;
const NEW_GROUP: u64 = 0x1112_1314_1516_1718;

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
}

/// The spec's group key written out here (creator, a dot, the 16 hex digits of the little-endian bytes).
fn group_key_text(creator: &str, group_id: u64) -> ConvKey {
    let digits: String = group_id
        .to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ConvKey::Group(format!("threema:{creator}.{digits}").into())
}

/// Reads envelopes until `n` events came, ACKING each (the core applied it).
async fn events(h: &mut H, n: usize) -> Vec<BackendEvent> {
    let mut got = Vec::new();
    while got.len() < n {
        let envelope = h.envelope().await;
        let ack = envelope.ack.expect("a durable envelope");
        got.extend(envelope.events);
        ack.complete(Ok(()));
    }
    assert!(got.len() == n, "more events than expected");
    got
}

fn contact(id: &str, profile: Patch<String>, system: Patch<String>, hidden: bool) -> BackendEvent {
    let mut patch = ContactPatch::new(user(id));
    patch.profile_name = profile;
    patch.system_name = system;
    patch.hidden = Some(hidden);
    patch.blocked = Some(false);
    BackendEvent::ContactUpsert(patch)
}

fn set(text: &str) -> Patch<String> {
    Patch::Set(text.to_owned())
}

/// A 1:1 chat as A7b states it (E1): Threema's features, then what `state` sets (the phone's chat state).
fn direct(id: &str, state: impl FnOnce(&mut ConversationPatch)) -> BackendEvent {
    let mut patch = ConversationPatch::new(ConvKey::Direct(user(id)));
    patch.features = Some(support::threema_features());
    state(&mut patch);
    BackendEvent::ConversationUpsert(patch)
}

/// A seeded or created contact's chat: no chat state from the phone, the features alone.
fn plain(_: &mut ConversationPatch) {}

fn group(id: u64, title: &str, members: &[&str]) -> BackendEvent {
    group_with(id, title, members, plain)
}

/// A group's `ConversationUpsert` (with Threema's features, A7b) and the chat state `state` sets.
fn group_with(
    id: u64,
    title: &str,
    members: &[&str],
    state: impl FnOnce(&mut ConversationPatch),
) -> BackendEvent {
    let mut patch = ConversationPatch::new(group_key_text(CREATOR, id));
    patch.title = set(title);
    let mut list = vec![Member {
        user: user(CREATOR),
        role: MemberRole::Admin,
    }];
    list.extend(members.iter().map(|m| Member {
        user: user(m),
        role: MemberRole::Member,
    }));
    patch.members = Some(MemberList::snapshot(list));
    patch.features = Some(support::threema_features());
    state(&mut patch);
    BackendEvent::ConversationUpsert(patch)
}

/// The file's stored message of a contact / a group (a second reader).
fn stored_contact(h: &H, id: &str) -> Option<d2d_sync::Contact> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        let bytes: Option<Vec<u8>> = c
            .query_row("SELECT sync FROM contact WHERE identity = ?1", [id], |r| {
                r.get(0)
            })
            .ok();
        Ok::<_, kourier_db::Error>(bytes)
    })
    .expect("read the contact")
    .map(|b| d2d_sync::Contact::decode(b.as_slice()).expect("a stored contact"))
}

fn stored_group(h: &H, creator: &str, id: u64) -> Option<d2d_sync::Group> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        let bytes: Option<Vec<u8>> = c
            .query_row(
                "SELECT sync FROM grp WHERE creator = ?1 AND group_id = ?2",
                kourier_db::rusqlite::params![creator, i64::from_ne_bytes(id.to_ne_bytes())],
                |r| r.get(0),
            )
            .ok();
        Ok::<_, kourier_db::Error>(bytes)
    })
    .expect("read the group")
    .map(|b| d2d_sync::Group::decode(b.as_slice()).expect("a stored group"))
}

/// What a Rebuild states over the file now: row 1 is `AccountReady` alone (the seeded file has no profile
/// or settings row); the rest is returned.
fn rebuild_rows(h: &H) -> Vec<Vec<BackendEvent>> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let mut rows = db.read(store::stated_events).expect("the stated rows");
    assert!(
        rows.first()
            .is_some_and(|r| r.len() == 1 && matches!(r[0], BackendEvent::AccountReady { .. })),
        "row 1 is not AccountReady alone"
    );
    rows.remove(0);
    rows
}

#[test]
fn contact_syncs_reach_the_core_as_literal_events_and_the_blob_changes_field_by_field() {
    support::offline::run_child(
        module_path!(),
        "contact_syncs_reach_the_core_as_literal_events_and_the_blob_changes_field_by_field_child",
    );
}

/// A create, an update with names and a DIFFERENT key (H6), an update that CLEARS the names (M1) and a
/// removal (acquaintance level `GROUP_OR_DELETED`, the proto has no `ContactSync.Delete`).
#[tokio::test]
async fn contact_syncs_reach_the_core_as_literal_events_and_the_blob_changes_field_by_field_child()
{
    if !support::offline::in_child() {
        return;
    }
    let s = scene(701).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.contact_create(
                ContactDelta::create(NEWC, [5; 32], START_MS)
                    .names("Synthetic", "Person")
                    .nickname("Synth"),
            ),
        );
        phone.reflect_envelope(
            &author.contact_update(
                ContactDelta::new(PEER)
                    .nickname("Peer Nick")
                    .public_key([9; 32]),
            ),
        );
        phone.reflect_envelope(&author.contact_update(ContactDelta::new(NEWC).clear_names()));
        phone.reflect_envelope(&author.contact_delete(PEER));
    });
    // A7b (E1): each contact that is DIRECT after its delta states its 1:1 chat right after it; the removal
    // (hidden) states none (the lead's call).
    let got = events(&mut h, 7).await;
    let expected = vec![
        contact(NEWC, set("Synth"), set("Synthetic Person"), false),
        direct(NEWC, plain),
        contact(PEER, set("Peer Nick"), Patch::Unchanged, false),
        direct(PEER, plain),
        contact(NEWC, set("Synth"), Patch::Clear, false),
        direct(NEWC, plain),
        contact(PEER, set("Peer Nick"), Patch::Unchanged, true),
    ];
    assert!(
        got == expected,
        "the contact events differ from the literal ones"
    );
    // The BLOBs, field by field.
    let peer = stored_contact(&h, PEER).expect("the peer is stored");
    assert!(
        peer.public_key == Some(vec![7; 32]),
        "H6: a delta replaced the stored key"
    );
    assert!(peer.nickname.as_deref() == Some("Peer Nick"));
    assert_eq!(
        peer.acquaintance_level,
        Some(d2d_sync::contact::AcquaintanceLevel::GroupOrDeleted as i32)
    );
    assert_eq!(peer.created_at, Some(START_MS), "an absent field changed");
    let new = stored_contact(&h, NEWC).expect("the new contact is stored");
    assert!(new.public_key == Some(vec![5; 32]));
    assert!(new.first_name.as_deref() == Some("") && new.last_name.as_deref() == Some(""));
    assert!(new.nickname.as_deref() == Some("Synth"));
    // A Rebuild states what the deltas left, by ID.
    let rows = rebuild_rows(&h);
    let rebuilt = vec![vec![
        contact(NEWC, set("Synth"), Patch::Clear, false),
        contact(PEER, set("Peer Nick"), Patch::Unchanged, true),
        direct(NEWC, plain),
    ]];
    assert!(rows == rebuilt, "a Rebuild states other contacts");
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

#[test]
fn group_syncs_reach_the_core_with_their_notices_and_a_text_after_a_create_is_mapped() {
    support::offline::run_child(
        module_path!(),
        "group_syncs_reach_the_core_with_their_notices_and_a_text_after_a_create_is_mapped_child",
    );
}

/// A `GroupSync.Create` and a text in that group in ONE batch (the text is mapped: the zone reads the
/// create's row), then an update that replaces the members and the name (the notices, sorted), then a
/// `GroupSync.Delete` of the seeded group (`ConversationRemoved`, the row gone), then a Rebuild's rows.
#[tokio::test]
async fn group_syncs_reach_the_core_with_their_notices_and_a_text_after_a_create_is_mapped_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(702)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, OLD_GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.group_create(GroupDelta::create(
            CREATOR,
            NEW_GROUP,
            "Synthetic team",
            &[PEER],
            START_MS,
        )));
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::text([4; 8], START_MS + 1, "synthetic hello")
                    .in_group(CREATOR, NEW_GROUP)
                    .nonce([4; 24]),
            ),
        );
    });
    let got = events(&mut h, 2).await;
    // The own ID ("KOURIER3") sorts before PEER ("PEERAAAA").
    assert!(
        got[0] == group(NEW_GROUP, "Synthetic team", &[ME, PEER]),
        "the created group's event differs (no notice: a Create shows none)"
    );
    let BackendEvent::Message(text) = &got[1] else {
        panic!("the text after the create was not mapped");
    };
    assert!(
        text.conv == group_key_text(CREATOR, NEW_GROUP)
            && text.body == Body::text("synthetic hello")
    );

    h.net().act(|_| {
        phone.reflect_envelope(
            &author.group_update(
                GroupDelta::new(CREATOR, NEW_GROUP)
                    .members(&[PEER2])
                    .name("Renamed team"),
                &[(PEER, MemberChange::Kicked), (PEER2, MemberChange::Added)],
            ),
        );
    });
    let got = events(&mut h, 4).await;
    assert!(
        got[0] == group(NEW_GROUP, "Renamed team", &[PEER2, ME]),
        "the updated group's event differs"
    );
    let by = Some(user(CREATOR));
    let notices: Vec<(&str, &SystemEvent, &Origin)> = got[1..]
        .iter()
        .map(|e| match e {
            BackendEvent::Message(m) => match &m.body {
                Body::System(s) => (&*m.local, s, &m.origin),
                _ => panic!("a notice is a system row"),
            },
            _ => panic!("a notice is a message"),
        })
        .collect();
    let expected = [
        SystemEvent::MembersAdded {
            by: by.clone(),
            users: vec![user(PEER2)],
        },
        SystemEvent::MembersRemoved {
            by: by.clone(),
            users: vec![user(PEER)],
        },
        SystemEvent::TitleChanged {
            by,
            title: "Renamed team".to_owned(),
        },
    ];
    for (i, (local, notice, origin)) in notices.iter().enumerate() {
        assert!(*notice == &expected[i], "notice {i} differs");
        assert!(
            local.starts_with("gs:")
                && local.ends_with(&format!(":{i}"))
                && local.len() == 3 + 16 + 2,
            "notice {i}'s id is not gs:<16 hex>:<index>"
        );
        assert!(
            **origin
                == Origin::Remote {
                    author: user(CREATOR),
                    device: None
                }
        );
    }
    let stored = stored_group(&h, CREATOR, NEW_GROUP).expect("the group is stored");
    assert_eq!(
        stored.member_identities.map(|m| m.identities),
        Some(vec![PEER2.to_owned()]),
        "the members were not replaced whole"
    );
    assert!(stored.name.as_deref() == Some("Renamed team"));

    h.net().act(|_| {
        phone.reflect_envelope(&author.group_delete(CREATOR, OLD_GROUP));
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![BackendEvent::ConversationRemoved {
            conv: group_key_text(CREATOR, OLD_GROUP)
        }],
        "a delete is not ConversationRemoved"
    );
    assert!(
        stored_group(&h, CREATOR, OLD_GROUP).is_none(),
        "the deleted group's row stays"
    );
    // A Rebuild: the seeded contacts, then the one group left, as the deltas left it.
    let rows = rebuild_rows(&h);
    let rebuilt = vec![vec![
        contact(CREATOR, Patch::Unchanged, Patch::Unchanged, false),
        contact(PEER, Patch::Unchanged, Patch::Unchanged, false),
        direct(CREATOR, plain),
        direct(PEER, plain),
        group(NEW_GROUP, "Renamed team", &[PEER2, ME]),
    ]];
    assert!(rows == rebuilt, "a Rebuild states other contacts or groups");
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

/// The mute end the phone sets in A7b's tests (ms).
const MUTE_AT: u64 = START_MS + 86_400_000;

fn mute_at() -> Patch<i64> {
    Patch::Set(i64::try_from(MUTE_AT).expect("fits"))
}

/// The bring-back's `ConversationUpsert` (H5): `archived: Some(false)` alone.
fn unarchive(key: ConvKey) -> BackendEvent {
    let mut patch = ConversationPatch::new(key);
    patch.archived = Some(false);
    BackendEvent::ConversationUpsert(patch)
}

/// The next envelope's events, read BEFORE its ack (the file's state at that point is the zone's).
async fn next_unacked(h: &mut H) -> (Vec<BackendEvent>, kourier_backend_api::Ack) {
    let envelope = h.envelope().await;
    (envelope.events, envelope.ack.expect("a durable envelope"))
}

const NORMAL: Option<i32> = Some(d2d_sync::ConversationVisibility::Normal as i32);
const ARCHIVED: Option<i32> = Some(d2d_sync::ConversationVisibility::Archived as i32);

#[test]
fn a_contact_syncs_one_to_one_chat_follows_the_phone_and_a_message_brings_it_back() {
    support::offline::run_child(
        module_path!(),
        "a_contact_syncs_one_to_one_chat_follows_the_phone_and_a_message_brings_it_back_child",
    );
}

/// A7b: a `ContactSync.Create` of an archived, muted, protected chat states the contact AND its 1:1 chat
/// (E1, QC, QA); a reflected text into that ARCHIVED chat brings it back (H5): `ConversationUpsert {
/// archived: Some(false) }` FIRST in the text's row, and the stored BLOB's visibility is NORMAL when the
/// core gets that row (the same zone); a second text brings nothing back; a delta that HIDES the contact
/// states no chat (the lead's call), one that makes it direct again states it from the BLOB; a Rebuild
/// states the same, literally.
#[tokio::test]
async fn a_contact_syncs_one_to_one_chat_follows_the_phone_and_a_message_brings_it_back_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(703).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.contact_create(
                ContactDelta::create(NEWC, [5; 32], START_MS)
                    .archived()
                    .muted_until(MUTE_AT)
                    .protected(),
            ),
        );
    });
    let got = events(&mut h, 2).await;
    let archived_chat = |p: &mut ConversationPatch| {
        p.protected = Some(true);
        p.archived = Some(true);
        p.pinned_order = Patch::Clear;
        p.muted_until = mute_at();
    };
    assert!(
        got == vec![
            contact(NEWC, Patch::Unchanged, Patch::Unchanged, false),
            direct(NEWC, archived_chat),
        ],
        "a created contact's chat is not stated with the phone's state"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            NEWC,
            &Msg::text([6; 8], START_MS + 1, "synthetic back").nonce([6; 24]),
        ));
    });
    let (got, ack) = next_unacked(&mut h).await;
    assert!(
        got.len() == 2 && got[0] == unarchive(ConvKey::Direct(user(NEWC))),
        "the text into the archived chat does not bring it back first"
    );
    assert!(
        matches!(&got[1], BackendEvent::Message(m) if m.body == Body::text("synthetic back")),
        "the text is not the row's second event"
    );
    assert_eq!(
        stored_contact(&h, NEWC)
            .expect("stored")
            .conversation_visibility,
        NORMAL,
        "H5: the stored chat is still archived when the core gets the row"
    );
    ack.complete(Ok(()));

    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            NEWC,
            &Msg::text([7; 8], START_MS + 2, "synthetic again").nonce([7; 24]),
        ));
    });
    let got = events(&mut h, 1).await;
    assert!(
        matches!(got.as_slice(), [BackendEvent::Message(_)]),
        "a text into a chat that is not archived brings something back"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.contact_update(ContactDelta::new(NEWC).hidden()));
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![contact(NEWC, Patch::Unchanged, Patch::Unchanged, true)],
        "a hidden contact's chat is stated"
    );
    h.net().act(|_| {
        phone.reflect_envelope(&author.contact_update(ContactDelta::new(NEWC).direct()));
    });
    let got = events(&mut h, 2).await;
    let back_chat = |p: &mut ConversationPatch| {
        p.protected = Some(true);
        p.archived = Some(false);
        p.pinned_order = Patch::Clear;
        p.muted_until = mute_at();
    };
    assert!(
        got == vec![
            contact(NEWC, Patch::Unchanged, Patch::Unchanged, false),
            direct(NEWC, back_chat),
        ],
        "a contact made direct again does not state its chat as the file holds it"
    );
    let rows = rebuild_rows(&h);
    let rebuilt = vec![vec![
        contact(NEWC, Patch::Unchanged, Patch::Unchanged, false),
        contact(PEER, Patch::Unchanged, Patch::Unchanged, false),
        direct(NEWC, back_chat),
        direct(PEER, plain),
    ]];
    assert!(rows == rebuilt, "a Rebuild states other contacts or chats");
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

#[test]
fn a_group_sync_protects_and_unprotects_and_a_message_brings_the_group_back() {
    support::offline::run_child(
        module_path!(),
        "a_group_sync_protects_and_unprotects_and_a_message_brings_the_group_back_child",
    );
}

/// A7b: a `GroupSync.Update` to PROTECTED, PINNED, "only when mentioned" states `protected: Some(true)`, the
/// pin, NOT muted (M8); the next one back to DEFAULT, ARCHIVED, muted until a time states `protected:
/// Some(false)`, archived, the mute; a text into the archived group brings it back (H5, the stored group
/// NORMAL when the core gets the row); a Rebuild states the same, literally.
#[tokio::test]
async fn a_group_sync_protects_and_unprotects_and_a_message_brings_the_group_back_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(704)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, OLD_GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.group_update(
                GroupDelta::new(CREATOR, OLD_GROUP)
                    .protected()
                    .pinned()
                    .mentioned_only(),
                &[],
            ),
        );
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![group_with(OLD_GROUP, "synthetic group", &[ME], |p| {
            p.protected = Some(true);
            p.archived = Some(false);
            p.pinned_order = Patch::Set(0);
            p.muted_until = Patch::Clear;
        })],
        "the protected, pinned group differs (only when mentioned is not muted)"
    );
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.group_update(
                GroupDelta::new(CREATOR, OLD_GROUP)
                    .unprotected()
                    .archived()
                    .muted_until(MUTE_AT),
                &[],
            ),
        );
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![group_with(OLD_GROUP, "synthetic group", &[ME], |p| {
            p.protected = Some(false);
            p.archived = Some(true);
            p.pinned_order = Patch::Clear;
            p.muted_until = mute_at();
        })],
        "the group back to unprotected, archived and muted differs"
    );
    assert_eq!(
        stored_group(&h, CREATOR, OLD_GROUP)
            .expect("stored")
            .conversation_visibility,
        ARCHIVED
    );
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::text([8; 8], START_MS + 3, "synthetic group back")
                    .in_group(CREATOR, OLD_GROUP)
                    .nonce([8; 24]),
            ),
        );
    });
    let (got, ack) = next_unacked(&mut h).await;
    assert!(
        got.len() == 2 && got[0] == unarchive(group_key_text(CREATOR, OLD_GROUP)),
        "the text into the archived group does not bring it back first"
    );
    assert!(matches!(&got[1], BackendEvent::Message(_)));
    assert_eq!(
        stored_group(&h, CREATOR, OLD_GROUP)
            .expect("stored")
            .conversation_visibility,
        NORMAL,
        "H5: the stored group is still archived when the core gets the row"
    );
    ack.complete(Ok(()));
    let rows = rebuild_rows(&h);
    let rebuilt = vec![vec![
        contact(CREATOR, Patch::Unchanged, Patch::Unchanged, false),
        contact(PEER, Patch::Unchanged, Patch::Unchanged, false),
        direct(CREATOR, plain),
        direct(PEER, plain),
        group_with(OLD_GROUP, "synthetic group", &[ME], |p| {
            p.protected = Some(false);
            p.archived = Some(false);
            p.pinned_order = Patch::Clear;
            p.muted_until = mute_at();
        }),
    ]];
    assert!(
        rows == rebuilt,
        "a Rebuild states other contacts, chats or groups"
    );
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

// TH5b B4 (R13): THE SETTINGS, THE PROFILE AND THE LISTS ---------------------------------------------------

/// The stored contacts of B4's settings test (synthetic, sorted A < B < C < D) and a blocked ID the file
/// holds no contact for.
const SET_A: &str = "B4SETA01";
const SET_B: &str = "B4SETB01";
const SET_C: &str = "B4SETC01";
const SET_D: &str = "B4SETD01";
const SET_STRANGER: &str = "B4SETS01";
/// The lists the phone creates (synthetic ids).
const LIST_ONE: u64 = 0x0b04_1111_0000_0001;
const LIST_UNKNOWN: u64 = 0x0b04_2222_0000_0002;
const LIST_EMPTY: u64 = 0x0b04_3333_0000_0003;

fn identities(ids: &[&str]) -> Option<libthreema::protobuf::common::Identities> {
    Some(libthreema::protobuf::common::Identities {
        identities: ids.iter().map(|i| (*i).to_owned()).collect(),
    })
}

/// A stored contact's `ContactUpsert` with its blocked flag (direct, not hidden).
fn blocked_contact(id: &str, profile: Patch<String>, blocked: bool) -> BackendEvent {
    let mut patch = ContactPatch::new(user(id));
    patch.profile_name = profile;
    patch.system_name = Patch::Unchanged;
    patch.hidden = Some(false);
    patch.blocked = Some(blocked);
    BackendEvent::ContactUpsert(patch)
}

/// `AccountSettings` with the two switches the core knows.
fn switches(read_receipts: bool, typing: bool) -> BackendEvent {
    BackendEvent::AccountSettings(AccountSettingsPatch {
        read_receipts: Some(read_receipts),
        typing_indicators: Some(typing),
        ..AccountSettingsPatch::default()
    })
}

/// `AccountReady` of the own ID with the display name (F5: `None` for an empty nickname).
fn ready(display_name: Option<&str>) -> BackendEvent {
    BackendEvent::AccountReady {
        own_ids: vec![user(ME)],
        device: None,
        display_name: display_name.map(|n| Redacted::new(n.to_owned())),
        number: None,
    }
}

/// What a Rebuild states over the file now, every row (row 1 included).
fn rebuild_all(h: &H) -> Vec<Vec<BackendEvent>> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(store::stated_events).expect("the stated rows")
}

/// The file's one row of `table` (`settings`, `own_profile`), decoded.
fn stored_one<M: prost::Message + Default>(h: &H, table: &str) -> Option<M> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        let bytes: Option<Vec<u8>> = c
            .query_row(&format!("SELECT sync FROM {table} WHERE id = 1"), [], |r| {
                r.get(0)
            })
            .ok();
        Ok::<_, kourier_db::Error>(bytes)
    })
    .expect("read the row")
    .map(|b| M::decode(b.as_slice()).expect("a stored message"))
}

fn stored_list(h: &H, id: u64) -> Option<d2d_sync::DistributionList> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        let bytes: Option<Vec<u8>> = c
            .query_row(
                "SELECT sync FROM dist_list WHERE id = ?1",
                [i64::from_ne_bytes(id.to_ne_bytes())],
                |r| r.get(0),
            )
            .ok();
        Ok::<_, kourier_db::Error>(bytes)
    })
    .expect("read the list")
    .map(|b| d2d_sync::DistributionList::decode(b.as_slice()).expect("a stored list"))
}

#[test]
fn a_settings_delta_blocks_and_unblocks_stored_contacts_and_a_rebuild_states_what_it_left() {
    support::offline::run_child(
        module_path!(),
        "a_settings_delta_blocks_and_unblocks_stored_contacts_and_a_rebuild_states_what_it_left_child",
    );
}

/// R13 with LITERAL events: the first delta turns read receipts off and typing on and blocks B, D and a
/// stranger (`AccountSettings`, then B's and D's `ContactUpsert`; the stranger has no contact: nothing); the
/// second, in ONE batch with a contact sync of A after it, turns read receipts on and typing off and blocks
/// [D, A] (A blocked, B unblocked; D stays blocked and C stays unblocked: NO event for either), and A's contact
/// sync reads the batch's new settings (stated blocked); a profile delta states the nickname. The stored
/// settings are the delta's (the blocked list replaced, never appended), and a Rebuild states the new
/// settings, the blocked contacts and the nickname, written out.
#[tokio::test]
async fn a_settings_delta_blocks_and_unblocks_stored_contacts_and_a_rebuild_states_what_it_left_child()
 {
    if !support::offline::in_child() {
        return;
    }
    use d2d_sync::{ReadReceiptPolicy as Rr, TypingIndicatorPolicy as Ti};
    let s = scene(741).with_contacts(&[SET_A, SET_B, SET_C, SET_D]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.settings_update(d2d_sync::Settings {
            read_receipt_policy: Some(Rr::DontSendReadReceipt as i32),
            typing_indicator_policy: Some(Ti::SendTypingIndicator as i32),
            blocked_identities: identities(&[SET_B, SET_D, SET_STRANGER]),
            ..Default::default()
        }));
    });
    let got = events(&mut h, 3).await;
    assert!(
        got == vec![
            switches(false, true),
            blocked_contact(SET_B, Patch::Unchanged, true),
            blocked_contact(SET_D, Patch::Unchanged, true),
        ],
        "the first settings delta's events differ from the literal ones"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.settings_update(d2d_sync::Settings {
            read_receipt_policy: Some(Rr::SendReadReceipt as i32),
            typing_indicator_policy: Some(Ti::DontSendTypingIndicator as i32),
            blocked_identities: identities(&[SET_D, SET_A]),
            ..Default::default()
        }));
        phone
            .reflect_envelope(&author.contact_update(ContactDelta::new(SET_A).nickname("Synth A")));
    });
    let got = events(&mut h, 5).await;
    assert!(
        got == vec![
            switches(true, false),
            blocked_contact(SET_A, Patch::Unchanged, true),
            blocked_contact(SET_B, Patch::Unchanged, false),
            blocked_contact(SET_A, set("Synth A"), true),
            direct(SET_A, plain),
        ],
        "the second settings delta (or the contact sync after it) differs from the literal events"
    );
    let stored: d2d_sync::Settings = stored_one(&h, "settings").expect("the settings are stored");
    assert!(
        stored
            == d2d_sync::Settings {
                read_receipt_policy: Some(Rr::SendReadReceipt as i32),
                typing_indicator_policy: Some(Ti::DontSendTypingIndicator as i32),
                blocked_identities: identities(&[SET_D, SET_A]),
                ..Default::default()
            },
        "the stored settings are not the second delta's (the blocked list replaced whole)"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.profile_update(d2d_sync::UserProfile {
            nickname: Some("Synthetic Own".to_owned()),
            ..Default::default()
        }));
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![ready(Some("Synthetic Own"))],
        "the profile delta is not AccountReady with the nickname"
    );
    // A Rebuild: AccountReady with the nickname and the settings first, then the contacts with their blocked
    // flags, then their 1:1 chats.
    let rows = rebuild_all(&h);
    let rebuilt = vec![
        vec![ready(Some("Synthetic Own")), switches(true, false)],
        vec![
            blocked_contact(SET_A, set("Synth A"), true),
            blocked_contact(SET_B, Patch::Unchanged, false),
            blocked_contact(SET_C, Patch::Unchanged, false),
            blocked_contact(SET_D, Patch::Unchanged, true),
            direct(SET_A, plain),
            direct(SET_B, plain),
            direct(SET_C, plain),
            direct(SET_D, plain),
        ],
    ];
    assert!(
        rows == rebuilt,
        "a Rebuild states other settings, contacts or names"
    );
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

#[test]
fn a_profile_delta_states_the_nickname_and_an_empty_one_clears_the_stored_one() {
    support::offline::run_child(
        module_path!(),
        "a_profile_delta_states_the_nickname_and_an_empty_one_clears_the_stored_one_child",
    );
}

/// R13 + F5 with LITERAL events: a profile delta with a nickname states `AccountReady` with it, and a Rebuild
/// states the same; one with an EMPTY nickname stores the empty string (the spec: as-is; the picture the
/// first delta set stays) and states `AccountReady` with NO name (the core keeps its old display name: the
/// contract's limit), and a Rebuild states no name either.
#[tokio::test]
async fn a_profile_delta_states_the_nickname_and_an_empty_one_clears_the_stored_one_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(742).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let removed = Some(libthreema::protobuf::common::DeltaImage {
        image: Some(libthreema::protobuf::common::delta_image::Image::Removed(
            libthreema::protobuf::common::Unit {},
        )),
    });
    h.net().act(|_| {
        phone.reflect_envelope(&author.profile_update(d2d_sync::UserProfile {
            nickname: Some("Synthetic Own".to_owned()),
            profile_picture: removed.clone(),
            ..Default::default()
        }));
    });
    let got = events(&mut h, 1).await;
    assert!(
        got == vec![ready(Some("Synthetic Own"))],
        "the nickname is not stated"
    );
    let rows = rebuild_all(&h);
    assert!(
        rows.first() == Some(&vec![ready(Some("Synthetic Own"))]),
        "a Rebuild's row 1 is not AccountReady with the nickname"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.profile_update(d2d_sync::UserProfile {
            nickname: Some(String::new()),
            ..Default::default()
        }));
    });
    let got = events(&mut h, 1).await;
    assert!(got == vec![ready(None)], "an empty nickname states a name");
    let stored: d2d_sync::UserProfile =
        stored_one(&h, "own_profile").expect("the profile is stored");
    assert!(
        stored
            == d2d_sync::UserProfile {
                nickname: Some(String::new()),
                profile_picture: removed,
                ..Default::default()
            },
        "the stored profile is not the empty nickname with the first delta's picture"
    );
    let rows = rebuild_all(&h);
    let rebuilt = vec![
        vec![ready(None)],
        vec![
            contact(PEER, Patch::Unchanged, Patch::Unchanged, false),
            direct(PEER, plain),
        ],
    ];
    assert!(
        rows == rebuilt,
        "a Rebuild states a name for an empty nickname"
    );
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

#[test]
fn a_list_create_update_and_delete_are_stored_and_state_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_list_create_update_and_delete_are_stored_and_state_nothing_child",
    );
}

/// R13: a list `Create` and an `Update` in ONE batch (the update reads the create's row) write the row, the
/// update's fields replacing the create's (members replaced whole), and state NOTHING; then a `Delete` removes
/// the row, an `Update` and a `Delete` of a list the file lacks are `list-unknown` and a `Create` with an EMPTY
/// member list is `list-invalid` (one WARN per rule); everything is acked, nothing deferred, no `core_outbox`
/// row; a Rebuild states no list.
#[tokio::test]
async fn a_list_create_update_and_delete_are_stored_and_state_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(743).with_contacts(&[PEER, PEER2]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let list = |id: u64, name: &str, members: &[&str]| d2d_sync::DistributionList {
        distribution_list_id: id,
        name: Some(name.to_owned()),
        created_at: Some(START_MS),
        member_identities: identities(members),
        conversation_category: Some(d2d_sync::ConversationCategory::Default as i32),
        conversation_visibility: Some(d2d_sync::ConversationVisibility::Normal as i32),
    };
    h.net().act(|_| {
        phone.reflect_envelope(&author.list_create(list(
            LIST_ONE,
            "Synthetic list",
            &[PEER, PEER2],
        )));
        phone.reflect_envelope(&author.list_update(d2d_sync::DistributionList {
            distribution_list_id: LIST_ONE,
            name: Some("Renamed list".to_owned()),
            member_identities: identities(&[PEER2]),
            ..Default::default()
        }));
    });
    wait_acks(&h, 2).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert!(
        stored_list(&h, LIST_ONE) == Some(list(LIST_ONE, "Renamed list", &[PEER2])),
        "the stored list is not the create with the update's name and members"
    );
    assert_eq!(
        (
            h.file_rows("dist_list"),
            h.file_rows("deferred"),
            h.file_rows("core_outbox")
        ),
        (1, 0, 0),
        "a list sync left another row"
    );

    h.net().act(|_| {
        phone.reflect_envelope(&author.list_delete(LIST_ONE));
        phone.reflect_envelope(&author.list_update(list(LIST_UNKNOWN, "Nobody's", &[PEER])));
        phone.reflect_envelope(&author.list_delete(LIST_UNKNOWN));
        phone.reflect_envelope(&author.list_create(list(LIST_EMPTY, "Empty", &[])));
    });
    wait_acks(&h, 6).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(
        (
            h.file_rows("dist_list"),
            h.file_rows("deferred"),
            h.file_rows("core_outbox")
        ),
        (0, 0, 0),
        "the delete left the row, or a dropped sync wrote one"
    );
    for line in [
        "reflected envelopes acked and dropped: 2 by list-unknown",
        "reflected envelopes acked and dropped: 1 by list-invalid",
    ] {
        assert_eq!(
            warns.at(Level::WARN).iter().filter(|w| **w == line).count(),
            1,
            "not one WARN: {line}"
        );
    }
    // A Rebuild states the contacts (by ID: "A7SPEER2" before "PEERAAAA") and their chats, and no list.
    let rows = rebuild_rows(&h);
    let rebuilt = vec![vec![
        contact(PEER2, Patch::Unchanged, Patch::Unchanged, false),
        contact(PEER, Patch::Unchanged, Patch::Unchanged, false),
        direct(PEER2, plain),
        direct(PEER, plain),
    ]];
    assert!(
        rows == rebuilt,
        "a Rebuild states other rows than the contacts and their chats"
    );
    h.finish().await;
}

#[test]
fn a_settings_delta_over_1201_stored_contacts_is_three_rows_of_one_zone() {
    support::offline::run_child(
        module_path!(),
        "a_settings_delta_over_1201_stored_contacts_is_three_rows_of_one_zone_child",
    );
}

/// TH5b S-L3 (FIX-S), the BATCH zone's side of the rows of at most 500 events: a reflected settings delta that blocks
/// 1,201 stored contacts states 1,202 events, `AccountSettings` and then every contact's `ContactUpsert` in ID order.
/// When its ack comes the zone has written them as THREE `core_outbox` rows of 500, 500 and 202 events (none acked by
/// the core yet), and the core side gets the 1,202 events in that order. (The kill loop of the same split is store
/// level: `kill::a_delta_of_1202_events_is_three_rows_of_at_most_500_all_or_nothing`.)
#[tokio::test]
async fn a_settings_delta_over_1201_stored_contacts_is_three_rows_of_one_zone_child() {
    if !support::offline::in_child() {
        return;
    }
    let ids: Vec<String> = (0..1_201).map(|i| format!("L3{i:06}")).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    let s = scene(744).with_contacts(&ids);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.settings_update(d2d_sync::Settings {
            blocked_identities: identities(&ids),
            ..Default::default()
        }));
    });
    wait_acks(&h, 1).await;
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let rows: Vec<Vec<u8>> = db
        .read(|c: &Connection| {
            let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
            let got = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()?;
            Ok::<_, kourier_db::Error>(got)
        })
        .expect("the core_outbox rows");
    drop(db);
    let sizes: Vec<usize> = rows
        .iter()
        .map(|r| {
            kourier_backend_api::decode_outbox_row(r)
                .expect("a row decodes")
                .len()
        })
        .collect();
    assert_eq!(
        sizes,
        [500, 500, 202],
        "the delta's events are not three rows of at most 500"
    );
    let mut expected = vec![BackendEvent::AccountSettings(
        AccountSettingsPatch::default(),
    )];
    expected.extend(
        ids.iter()
            .map(|id| blocked_contact(id, Patch::Unchanged, true)),
    );
    let got = events(&mut h, 1_202).await;
    assert!(
        got == expected,
        "the core side did not get AccountSettings and the 1,201 blocked contacts in ID order"
    );
    h.finish().await;
}
