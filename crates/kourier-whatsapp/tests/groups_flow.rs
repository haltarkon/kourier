//! W6a2 B11: THE GROUP HALF AND THE TWO NEW FRAMES, END TO END over the scripted server. The real actor, client and
//! store (the send tests' `Rig`) answer a start's two group reads, a member's promote notice, a peer admin's delete,
//! our own admin delete and the 1:1 timer set, and the test plays the core (every envelope acknowledged, its events
//! kept) and the peer (it decrypts what the client writes, and writes group messages with a sender key of its own,
//! `Peer::group_message_with_id`).
//!
//! What this shows that the unit tests (over `FakeSession`) cannot:
//!  - `start`: at `Online` the client asks the overview and ONE batch (two `w:g2` IQs, nothing else), the core hears the
//!    group ONCE, with its title, the whole member list (a super admin and an admin are `Admin`) and the features of
//!    the same zone (`admin_delete`, `group_info`), one `IdentityMerged` for a member whose number the store vouches
//!    for (a `usync` row), and a promote notice moves a member to `Admin` and nothing is asked of the server;
//!  - `wait`: an admin's delete that arrives while the member list is NOT stated (the batch was refused) is held with
//!    no event and no poison attempt, and the NEXT start's batch releases it into exactly one `Deleted{by}` (the
//!    kill sample's recovery in its plain form: the release is in the zone that states the list);
//!  - `admin_delete`: our admin's delete of another member's message is ONE `<message edit="8">` to the group whose
//!    protocol message has `fromMe: false` and the author as `participant`, which the member's phone reads as a
//!    revoke; the command is answered `Sent`;
//!  - `timer`: the 1:1 timer set is ONE frame carrying `EPHEMERAL_SETTING` to the peer, the command is answered after
//!    the ack, and the core hears the chat's timer and the notice.
//!
//! Every body runs in ONE offline child under `unshare -rn` (KOURIER_REQUIRE_UNSHARE=1); the parent checks that
//! nothing the child printed holds a secret of the fixture, and the child that nothing it logged does (D10: no number,
//! LID, group, subject or name). Every value is SYNTHETIC.

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::decode_outbox_row;
use kourier_backend_api::{
    AllocatedId, BackendEvent, Command, OutgoingKind, OutgoingRequest, TransmitOutcome,
};
use kourier_db::outbox;
use kourier_model::{
    Body, ConvKey, ConversationChange, MemberRole, MsgTarget, Patch, SystemEvent, TargetAuthor,
    UserId,
};
use kourier_whatsapp::failpoints as fp;
use wacore::store::traits::{LidPnMappingEntry, ProtocolStore};
use wacore_binary::Node;
use wacore_binary::marshal::{marshal, unmarshal_packed_ref};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use crate::receive::{account_dir, count, raw};
use crate::send_wire::{Rig, Script, now_secs, server_of};
use crate::support::capture::Capture;
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Answer, BatchEntry, Gp2, GroupSpec, Member};
use crate::support::{self, db_path, key};
use crate::wire_kill::copy_dir;

// ---- the fixture's synthetic values (each a substring of no other) ----------------------------------

/// A LID group.
const G: &str = "120363000091827364";
/// A member the store vouches for with a `usync` row, and the revoked message's author in the `wait` case.
const M_LID: &str = "811223344556677";
const M_PN: &str = "15557402968";
const SUBJECT: &str = "Ocelot flow subject";
const NOTIFY: &str = "Capybara flow notify";
/// The ids of the messages: the peer's delete, our delete, the timer message, the messages they name.
const PEER_REVOKE_ID: &str = "3EB0C0DE6C3D4E5F";
const OUR_REVOKE_ID: &str = "3EB0C0DE6B2C3D4E";
const TIMER_ID: &str = "3EB0C0DE6D4E5F60";
const TARGET_ID: &str = "3EB0C0DE6A1B2C3D";
/// The stanza ids of the failed-zone cases (a notice and a `groups_dirty`).
const FAILED_NOTICE_ID: &str = "flow-gp2-fails";
const FAILED_DIRTY_ID: &str = "flow-dirty-fails";
const SUBJECT_T: u64 = 1_760_100_000;
const NOTICE_T: u64 = 1_760_200_000;

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("an id")
}

fn group_key() -> ConvKey {
    ConvKey::Group(format!("wag:{G}").into())
}

fn lid(user: &str) -> String {
    format!("{user}@lid")
}

fn pn(user: &str) -> String {
    format!("{user}@s.whatsapp.net")
}

/// The group as the server answers: us the super admin, the peer with `peer_kind`, and (`with_member`) a member the
/// store vouches for.
fn spec(peer_kind: Option<&'static str>, with_member: bool) -> GroupSpec {
    let mut participants = vec![
        Member {
            jid: lid(peer::ACCOUNT_LID),
            phone_number: Some(pn(peer::ACCOUNT_PN)),
            kind: Some("superadmin"),
        },
        Member {
            jid: lid(peer::PEER_LID),
            phone_number: None,
            kind: peer_kind,
        },
    ];
    if with_member {
        participants.push(Member {
            jid: lid(M_LID),
            phone_number: Some(pn(M_PN)),
            kind: None,
        });
    }
    GroupSpec {
        user: G.into(),
        subject: Some(SUBJECT.into()),
        subject_time: Some(SUBJECT_T),
        creation: Some(1_759_000_000),
        addressing_mode: "lid",
        size: Some(u32::try_from(participants.len()).expect("size")),
        participants,
        ..GroupSpec::default()
    }
}

/// A server that answers a start's two group reads with `batch`, and the single query of the send path.
fn server_with(batch: Arc<Mutex<Option<GroupSpec>>>, refuse: Arc<AtomicBool>) -> wire::Wire {
    let server = server_of(&Script::acked());
    server.on_iq("w:g2", wire::participating(vec![spec(None, true)]));
    server.on_iq("w:g2", move |request: &Node| {
        if !wire::is_group_batch(request) {
            return None;
        }
        if refuse.load(Ordering::SeqCst) {
            return Some(Answer::Error(503));
        }
        let whole = batch.lock().expect("lock").clone()?;
        Some(wire::batch_answer(&[BatchEntry::Found(Box::new(whole))]))
    });
    server
}

/// The account's directory, the store's `usync` row for [`M_LID`], and the peer's session.
async fn account() -> (tempfile::TempDir, wacore::store::Device) {
    let (dir, device) = account_dir().await;
    let owner =
        kourier_whatsapp::StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open");
    owner
        .store()
        .put_lid_mapping(&LidPnMappingEntry {
            lid: M_LID.into(),
            phone_number: M_PN.into(),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            learning_source: "usync".into(),
        })
        .await
        .expect("the member's mapping");
    owner.close();
    (dir, device)
}

fn w_g2_iqs(server: &wire::Wire) -> usize {
    server
        .frame_nodes()
        .into_iter()
        .filter(|n| {
            n.tag == "iq"
                && n.attrs
                    .get("xmlns")
                    .map(|v| v.as_str().into_owned())
                    .as_deref()
                    == Some("w:g2")
        })
        .count()
}

fn upserts(events: &[BackendEvent]) -> Vec<kourier_model::ConversationPatch> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) if p.key == group_key() => Some(p.clone()),
            _ => None,
        })
        .collect()
}

fn role_of(list: &kourier_model::MemberList, who: &UserId) -> Option<MemberRole> {
    list.upsert.iter().find(|m| &m.user == who).map(|m| m.role)
}

fn deletes(events: &[BackendEvent]) -> Vec<BackendEvent> {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Deleted { .. }))
        .cloned()
        .collect()
}

fn merges(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::IdentityMerged { .. }))
        .count()
}

/// The peer's admin delete of `M_LID`'s message `TARGET_ID`, as a group message.
fn revoke_of_members_message() -> wa::Message {
    let mut key = wa::MessageKey::default();
    key.id = Some(TARGET_ID.into());
    key.from_me = Some(false);
    key.remote_jid = Some(format!("{G}@g.us"));
    key.participant = Some(lid(M_LID));
    let mut protocol = wa::message::ProtocolMessage::default();
    protocol.r#type = Some(wa::message::protocol_message::Type::REVOKE);
    protocol.key = MessageField::some(key);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(protocol);
    message
}

fn group_jid() -> wacore_binary::Jid {
    format!("{G}@g.us").parse().expect("a group jid")
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---- start -----------------------------------------------------------------------------------------

/// A start asks the overview and ONE batch, the core hears the group once with its features, a vouched pair is merged
/// once, and a promote notice is a delta with no new question to the server.
async fn a_start_states_the_group_once_and_a_notice_moves_a_role() {
    let (dir, _device) = account().await;
    let server = server_with(
        Arc::new(Mutex::new(Some(spec(None, true)))),
        Arc::new(AtomicBool::new(false)),
    );
    let rig = Rig::start(dir.path(), server).await;
    rig.heard(1, |e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == group_key() && p.members.is_some())
    })
    .await;
    rig.heard(1, |e| matches!(e, BackendEvent::IdentityMerged { .. }))
        .await;
    let events = rig.events();
    // The chat's first statement is the generic one (its features, bare); the zone's own upsert follows it whole.
    let all = upserts(&events);
    let stated: Vec<_> = all
        .iter()
        .filter(|p| p.members.is_some())
        .cloned()
        .collect();
    assert_eq!(stated.len(), 1, "the whole group is stated once");
    assert!(
        all.iter()
            .filter(|p| p.members.is_none())
            .all(|p| p.features == stated[0].features),
        "a bare statement before it says the same features"
    );
    let first = &stated[0];
    assert_eq!(first.title, Patch::Set(SUBJECT.to_owned()));
    let list = first.members.as_ref().expect("members");
    assert!(list.full, "the member list is a snapshot");
    assert_eq!(list.upsert.len(), 3);
    assert_eq!(
        role_of(list, &user("lid", peer::ACCOUNT_LID)),
        Some(MemberRole::Admin),
        "a super admin is an admin"
    );
    assert_eq!(
        role_of(list, &user("lid", peer::PEER_LID)),
        Some(MemberRole::Member)
    );
    assert_eq!(role_of(list, &user("lid", M_LID)), Some(MemberRole::Member));
    let features = first.features.as_ref().expect("the features ride along");
    assert!(features.admin_delete, "a group states admin_delete");
    assert!(
        features.group_info.is_some(),
        "the group's rules ride along"
    );
    assert!(features.cannot_send.is_none(), "we may send");
    let g2_before = w_g2_iqs(&rig.server);
    assert_eq!(g2_before, 2, "the start asks the overview and one batch");
    let pair = events.iter().find_map(|e| match e {
        BackendEvent::IdentityMerged { from, into } => Some((from.clone(), into.clone())),
        _ => None,
    });
    assert_eq!(pair, Some((user("pn", M_PN), user("lid", M_LID))));
    // A notice: the peer promotes the member, a delta and the features again, no IQ; the actor's name rides along.
    let promote = Gp2 {
        group: G.into(),
        id: "flow-gp2-promote".into(),
        t: NOTICE_T,
        participant: Some(lid(peer::PEER_LID)),
        participant_pn: None,
        notify: Some(NOTIFY.into()),
        lid: true,
        children: vec![wire::gp2_people(
            "promote",
            &[(lid(M_LID), Some(pn(M_PN)), None)],
        )],
    };
    rig.server.send(&promote.node());
    rig.heard(1, |e| {
        matches!(e, BackendEvent::ConversationUpsert(p)
            if p.key == group_key() && p.members.as_ref().is_some_and(|m| !m.full))
    })
    .await;
    let events = rig.events();
    let stated = upserts(&events);
    let delta = stated
        .iter()
        .filter_map(|p| p.members.as_ref())
        .find(|l| !l.full)
        .expect("the promote is a delta");
    assert_eq!(
        role_of(delta, &user("lid", M_LID)),
        Some(MemberRole::Admin),
        "the action is the role"
    );
    assert_eq!(delta.upsert.len(), 1);
    let named = rig.events().iter().filter(|e| {
        matches!(e, BackendEvent::ContactUpsert(c)
            if c.user == user("lid", peer::PEER_LID) && c.profile_name == Patch::Set(NOTIFY.to_owned()))
    }).count();
    assert_eq!(named, 1, "the actor's name is stated once");
    assert!(
        stated.last().and_then(|p| p.features.as_ref()).is_some(),
        "a delta is stated with the features"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(w_g2_iqs(&rig.server), g2_before, "a notice asks nothing");
    assert_eq!(merges(&rig.events()), 1, "the pair is stated once");
    rig.finish().await;
}

// ---- wait ------------------------------------------------------------------------------------------

/// An admin's delete that arrives while the member list is not stated waits (no event), and the next start's batch
/// releases it into exactly one `Deleted` by the admin.
async fn an_admins_delete_waits_for_the_member_list_and_the_next_batch_releases_it() {
    let (dir, device) = account().await;
    let refuse = Arc::new(AtomicBool::new(true));
    let batch = Arc::new(Mutex::new(Some(spec(Some("admin"), true))));
    let rig = Rig::start(dir.path(), server_with(batch.clone(), refuse.clone())).await;
    wait_for("the refused batch was asked", || w_g2_iqs(&rig.server) >= 2).await;
    let mut peer = Peer::new(&device, None).await;
    let stanza = peer
        .group_message_with_id(
            &group_jid(),
            &revoke_of_members_message(),
            PEER_REVOKE_ID,
            "text",
        )
        .await;
    rig.server.send(&stanza);
    let conn = raw(dir.path());
    wait_for("the delete is held for the member list", || {
        count(
            &conn,
            "SELECT count(*) FROM inbox WHERE members_wait IS NOT NULL",
        ) == 1
    })
    .await;
    assert!(
        deletes(&rig.events()).is_empty(),
        "no delete before the member list"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM group_state WHERE members_stated <> 0"
        ),
        0
    );
    rig.finish().await;
    // The next start: the batch answers.
    refuse.store(false, Ordering::SeqCst);
    let rig = Rig::start(dir.path(), server_with(batch, refuse)).await;
    rig.heard(1, |e| matches!(e, BackendEvent::Deleted { .. }))
        .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let events = rig.events();
    let all = deletes(&events);
    let [
        BackendEvent::Deleted {
            target,
            by,
            for_everyone,
            ..
        },
    ] = all.as_slice()
    else {
        panic!("exactly one delete: {}", all.len());
    };
    assert!(for_everyone);
    assert_eq!(by, &Some(user("lid", peer::PEER_LID)), "by the admin");
    assert_eq!(target.conv, Some(group_key()));
    assert_eq!(target.author, TargetAuthor::User(user("lid", M_LID)));
    assert_eq!(&*target.local, TARGET_ID);
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM inbox WHERE members_wait IS NOT NULL"
        ),
        0,
        "nothing waits any more"
    );
    rig.finish().await;
}

// ---- admin_delete ----------------------------------------------------------------------------------

/// Our admin's delete of another member's message: ONE frame to the group, `edit="8"`, the key of another's message.
async fn our_admin_delete_reaches_the_member_as_a_revoke_with_the_authors_key() {
    let (dir, device) = account().await;
    let server = server_of(&Script::acked());
    let own: &'static str = lid(peer::ACCOUNT_LID).leak();
    let member: &'static str = lid(peer::PEER_LID).leak();
    server.on_iq("w:g2", wire::participating(vec![spec(None, false)]));
    server.on_iq(
        "w:g2",
        wire::group_batch(vec![BatchEntry::Found(Box::new(spec(None, false)))]),
    );
    server.on_iq(
        "w:g2",
        wire::group_info(G, "lid", vec![(own, Some("superadmin")), (member, None)]),
    );
    let rig = Rig::start(dir.path(), server).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    // The row says we are an admin only after the batch.
    rig.heard(1, |e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == group_key() && p.members.is_some())
    })
    .await;
    let delete = OutgoingRequest {
        conv: group_key(),
        local: OUR_REVOKE_ID.into(),
        sent_at: i64::try_from(now_secs() * 1000).expect("time"),
        kind: OutgoingKind::Delete {
            target: MsgTarget {
                conv: Some(group_key()),
                author: TargetAuthor::User(user("lid", peer::PEER_LID)),
                local: TARGET_ID.into(),
                alternates: Vec::new(),
            },
            for_everyone: true,
        },
        only: None,
        is_recipient_update: false,
    };
    let outcome = rig.transmit(&delete).await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "the admin delete is not answered Sent: {}",
        match &outcome {
            TransmitOutcome::Failed(e) => format!("{:?}", e.kind),
            _ => "?".to_owned(),
        }
    );
    let frames = rig.message_frames(OUR_REVOKE_ID);
    assert_eq!(frames.len(), 1, "an admin delete is ONE frame");
    let frame = &frames[0];
    let attr = |key: &str| frame.attrs.get(key).map(|v| v.as_str().into_owned());
    assert_eq!(attr("to"), Some(format!("{G}@g.us")));
    assert_eq!(attr("edit").as_deref(), Some("8"), "an admin revoke");
    let read = peer
        .read_group(frame)
        .await
        .expect("the member reads the group frame");
    let protocol = read
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(
        protocol.r#type,
        Some(wa::message::protocol_message::Type::REVOKE)
    );
    let key = protocol.key.as_option().expect("the key");
    assert_eq!(key.from_me, Some(false), "the key is another's message");
    assert_eq!(key.id.as_deref(), Some(TARGET_ID));
    assert_eq!(
        key.participant.as_deref(),
        Some(lid(peer::PEER_LID).as_str())
    );
    assert_eq!(
        key.remote_jid.as_deref(),
        Some(format!("{G}@g.us").as_str())
    );
    rig.finish().await;
}

// ---- timer -----------------------------------------------------------------------------------------

/// The 1:1 timer set: ONE frame carrying the setting, answered after the ack, confirmed to the core.
async fn the_timer_set_reaches_the_peer_and_is_confirmed_after_the_ack() {
    let (dir, device) = account_dir().await;
    let rig = Rig::start(dir.path(), server_of(&Script::acked())).await;
    let mut peer = Peer::new(&device, None).await;
    rig.peer_writes_first(&mut peer).await;
    let chat = ConvKey::Direct(user("lid", peer::PEER_LID));
    let week = Duration::from_secs(604_800);
    let (reply, answer) = tokio::sync::oneshot::channel();
    rig.command(Command::ChangeConversation {
        conv: chat.clone(),
        change: ConversationChange::SetTimer { timer: Some(week) },
        id: Some(AllocatedId {
            local: TIMER_ID.into(),
            sent_at: i64::try_from(now_secs() * 1000).expect("time"),
        }),
        reply,
    })
    .await;
    let answered = tokio::time::timeout(Duration::from_secs(30), answer)
        .await
        .expect("the actor answers")
        .expect("an answer");
    assert!(answered.is_ok(), "the timer set is accepted");
    let frames = rig.message_frames(TIMER_ID);
    assert_eq!(frames.len(), 1, "a timer set is ONE frame");
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let setting = read
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(
        setting.r#type,
        Some(wa::message::protocol_message::Type::EPHEMERAL_SETTING)
    );
    assert_eq!(setting.ephemeral_expiration, Some(604_800));
    rig.heard(
        1,
        |e| matches!(e, BackendEvent::Message(m) if &*m.local == TIMER_ID),
    )
    .await;
    let events = rig.events();
    let notice = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Message(m) if &*m.local == TIMER_ID => Some(m.clone()),
            _ => None,
        })
        .expect("the notice");
    assert_eq!(
        notice.body,
        Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: Some(week)
        })
    );
    let timed = events.iter().any(|e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == chat && p.timer == Patch::Set(week))
    });
    assert!(timed, "the chat's timer is stated");
    rig.finish().await;
}

// ---- a zone that fails ------------------------------------------------------------------------------

/// DUR-7: how many `<ack>` frames the client wrote for stanza `id`.
fn acks(server: &wire::Wire, id: &str) -> usize {
    server
        .frames()
        .iter()
        .filter(|f| f.tag == "ack" && f.attr("id") == Some(id))
        .count()
}

/// The deltas of a member list (not a snapshot) that make [`M_LID`] an admin, in what the core heard.
fn promotions(events: &[BackendEvent]) -> usize {
    upserts(events)
        .iter()
        .filter_map(|p| p.members.as_ref())
        .filter(|list| !list.full && role_of(list, &user("lid", M_LID)) == Some(MemberRole::Admin))
        .count()
}

/// The peer promotes the member (as in the start case), stanza id `id`.
fn promote_notice(id: &str) -> Gp2 {
    Gp2 {
        group: G.into(),
        id: id.into(),
        t: NOTICE_T,
        participant: Some(lid(peer::PEER_LID)),
        participant_pn: None,
        notify: Some(NOTIFY.into()),
        lid: true,
        children: vec![wire::gp2_people(
            "promote",
            &[(lid(M_LID), Some(pn(M_PN)), None)],
        )],
    }
}

/// A start with the group stated, the pair merged and the connection quiet.
async fn quiet_rig(dir: &Path) -> Rig {
    let server = server_with(
        Arc::new(Mutex::new(Some(spec(None, true)))),
        Arc::new(AtomicBool::new(false)),
    );
    let rig = Rig::start(dir, server).await;
    rig.heard(1, |e| {
        matches!(e, BackendEvent::ConversationUpsert(p) if p.key == group_key() && p.members.is_some())
    })
    .await;
    rig.heard(1, |e| matches!(e, BackendEvent::IdentityMerged { .. }))
        .await;
    wait_for("the start's two reads", || w_g2_iqs(&rig.server) >= 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig
}

/// The next zone of the handler FAILS (the seam of D3) when `stanza` arrives: upstream sends no `<ack>` for it, the
/// connection ends, and the new connection is online and has done its own start (so the redelivery that follows is
/// not racing the start's fetch) when this returns.
async fn lose_the_zone_of(rig: &Rig, id: &str, stanza: &Node) {
    let before = rig.state_count();
    kourier_whatsapp::fault::arm(kourier_whatsapp::fault::EVENT_WRITE, 1);
    rig.server.send(stanza);
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(30)).await,
        "no new connection after the failed write (frames {:?})",
        rig.server.frames()
    );
    assert_eq!(
        acks(&rig.server, id),
        0,
        "the stanza was acknowledged before it was durable"
    );
    kourier_whatsapp::fault::disarm();
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
}

/// DUR-7, rule (a) for a group's notice (it has a branch of its own since W6a2): the zone that holds its acts FAILS, upstream
/// sends no `<ack>`, the connection ends, the core heard nothing of it, and the server's redelivery of the same stanza is
/// written ONCE and acknowledged. Mutants: the branch does not end the connection (the stanza is acked with nothing
/// durable); the branch's result is ignored.
async fn a_failed_notice_zone_withholds_the_ack_and_the_redelivery_is_written_once() {
    let (dir, _device) = account().await;
    let rig = quiet_rig(dir.path()).await;
    let promote = promote_notice(FAILED_NOTICE_ID).node();
    lose_the_zone_of(&rig, FAILED_NOTICE_ID, &promote).await;
    assert_eq!(
        promotions(&rig.events()),
        0,
        "the core heard a notice that was not durable"
    );
    rig.server.send(&promote);
    rig.heard(1, |e| {
        matches!(e, BackendEvent::ConversationUpsert(p)
            if p.key == group_key() && p.members.as_ref().is_some_and(|m| !m.full))
    })
    .await;
    wait_for("the redelivery is acknowledged", || {
        acks(&rig.server, FAILED_NOTICE_ID) == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        promotions(&rig.events()),
        1,
        "the redelivery is written once"
    );
    assert_eq!(
        acks(&rig.server, FAILED_NOTICE_ID),
        1,
        "and acknowledged once"
    );
    rig.finish().await;
}

/// DUR-7, the same for the server's `groups_dirty`: the zone that marks the group FAILS, no `<ack>`, the connection
/// ends, no mark is left; the redelivery marks the group (once) and is acknowledged. Mutants: as above for this branch.
async fn a_failed_groups_dirty_zone_withholds_the_ack_and_the_redelivery_marks_once() {
    let (dir, _device) = account().await;
    let rig = quiet_rig(dir.path()).await;
    let conn = raw(dir.path());
    let marks = "SELECT count(*) FROM group_state WHERE dirty_at IS NOT NULL";
    assert_eq!(count(&conn, marks), 0, "no mark before the notification");
    let dirty = wire::groups_dirty(FAILED_DIRTY_ID, NOTICE_T + 1, &[G]);
    lose_the_zone_of(&rig, FAILED_DIRTY_ID, &dirty).await;
    assert_eq!(
        count(&conn, marks),
        0,
        "a mark was left by a zone that failed"
    );
    rig.server.send(&dirty);
    wait_for("the redelivery is acknowledged", || {
        acks(&rig.server, FAILED_DIRTY_ID) == 1
    })
    .await;
    assert_eq!(
        count(&conn, marks),
        1,
        "the redelivery marks the group once"
    );
    assert_eq!(acks(&rig.server, FAILED_DIRTY_ID), 1);
    rig.finish().await;
}

// ---- the kill sample -------------------------------------------------------------------------------

const ROLE: &str = "WA_GROUPS_FLOW_ROLE";
const DIR: &str = "WA_GROUPS_FLOW_DIR";
const STANZA: &str = "WA_GROUPS_FLOW_STANZA";
const DELIVERED: &str = "WA_RIG_DELIVERED_LOG";
/// The most hits of one failpoint the `run` child makes. A loop that goes past it is a bug.
const MAX_HITS: u32 = 40;

fn reopen(dir: &Path) -> kourier_db::rusqlite::Connection {
    kourier_db::open(&db_path(dir), &key()).expect("reopen")
}

/// How many `Deleted` events the children of `dir` gave the core (one line each, before the acknowledgement).
fn delivered(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("delivered.log")).map_or(0, |text| text.lines().count())
}

/// The `Deleted` events `core_outbox` still holds (written, not acknowledged).
fn held_deletes(dir: &Path) -> usize {
    let conn = reopen(dir);
    outbox::peek(&conn, 10_000)
        .expect("peek the outbox")
        .iter()
        .flat_map(|row| decode_outbox_row(&row.envelope).expect("decodes"))
        .filter(|event| matches!(event, BackendEvent::Deleted { .. }))
        .count()
}

/// The child of the kill sample (a no-op as an ordinary test). `prepare`: the first start, whose batch the server
/// refuses, receives the peer's admin delete and holds it for the member list; `run`: a start whose batch answers, until
/// the delete has been released, mapped and given to the core.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_child() {
    if !support::offline::in_child() {
        return;
    }
    let dir = PathBuf::from(std::env::var(DIR).expect("the parent names the directory"));
    let role = std::env::var(ROLE).expect("the parent names the role");
    let conn = raw(&dir);
    if role == "prepare" {
        let bytes = std::fs::read(std::env::var(STANZA).expect("the stanza file")).expect("read");
        let stanza = unmarshal_packed_ref(&bytes)
            .expect("the stanza unpacks")
            .to_owned();
        let refuse = Arc::new(AtomicBool::new(true));
        let batch = Arc::new(Mutex::new(Some(spec(Some("admin"), true))));
        let rig = Rig::start(&dir, server_with(batch, refuse)).await;
        wait_for("the refused batch was asked", || w_g2_iqs(&rig.server) >= 2).await;
        rig.server.send(&stanza);
        wait_for("the delete is held for the member list", || {
            count(
                &conn,
                "SELECT count(*) FROM inbox WHERE members_wait IS NOT NULL",
            ) == 1
        })
        .await;
        rig.finish().await;
        return;
    }
    let batch = Arc::new(Mutex::new(Some(spec(Some("admin"), true))));
    let rig = Rig::start(&dir, server_with(batch, Arc::new(AtomicBool::new(false)))).await;
    wait_for("the delete is given to the core or already was", || {
        count(&conn, "SELECT count(*) FROM inbox WHERE state = 'new'") == 0
            && count(
                &conn,
                "SELECT count(*) FROM group_state WHERE members_stated <> 0",
            ) == 1
    })
    .await;
    // The core hears what stage 2 appended (or left from the killed run) before the child ends.
    wait_for("the core has everything", || {
        count(&conn, "SELECT count(*) FROM core_outbox") == 0
    })
    .await;
    rig.finish().await;
}

/// Runs the child once as `role`, `point` arming a failpoint.
fn run_kill_child(
    dir: &Path,
    role: &str,
    stanza: Option<&Path>,
    point: Option<&str>,
) -> (std::process::ExitStatus, String) {
    let mut command = support::offline::child_command(module_path!(), "kill_child");
    command
        .env(ROLE, role)
        .env(DIR, dir)
        .env(DELIVERED, dir.join("delivered.log"))
        .env_remove(STANZA)
        .env_remove("KOURIER_FAILPOINT");
    if let Some(stanza) = stanza {
        command.env(STANZA, stanza);
    }
    if let Some(point) = point {
        command.env("KOURIER_FAILPOINT", point);
    }
    let out = command.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for secret in secrets() {
        assert!(
            !said.contains(secret),
            "a child of the kill sample printed one of the fixture's secrets"
        );
    }
    (out.status, said)
}

/// The end state of a start whose batch answered: the delete is finished, nothing waits, the list is stated.
fn assert_released(dir: &Path, what: &str) {
    let conn = reopen(dir);
    assert_eq!(
        (
            count(&conn, "SELECT count(*) FROM inbox WHERE state = 'new'"),
            count(
                &conn,
                "SELECT count(*) FROM inbox WHERE members_wait IS NOT NULL"
            ),
            count(
                &conn,
                "SELECT count(*) FROM group_state WHERE members_stated <> 0"
            ),
        ),
        (0, 0, 1),
        "{what}: the delete is finished, nothing waits and the list is stated"
    );
}

/// W6a2 B11, the kill sample: the member list's statement, the release of the delete that waits for it and the delete's own
/// event are one chain of zones; the process is ABORTED inside and after every commit of the event writer and of stage 2 during
/// the start whose batch answers (a power cut: `KOURIER_FAILPOINT=<point>:<n>`, n = 1, 2, ... until the child ends normally),
/// and the next start must end in the same place: the delete released and given to the core ONCE, or once more only when the
/// killed start had given it and the row was still in `core_outbox` (delivered again, ingest is idempotent).
///
/// What the loop itself proves (DUR-9): every one of the four points is hit and kills at least once, a kill lands in the
/// state "list stated, delete released, not yet given" and one in "list not stated, delete still waiting", and at every kill
/// the recovery ends released and given once. The mutants it kills (run in the fix round, `target/wa/w6a2/mutants.log`): an
/// admin's delete mapped before the member list (K1); nothing is released when the list is stated (K2); the release leaves the
/// row parked (K3); a finished row is never finished, so the delete is mapped again at every step (K4); stage 2 has no kill
/// point after its commit (K5: the per-point count fails). It does NOT kill "the release is its own zone after the
/// statement's": the release is re-run by EVERY answer that carries a list (`groups/zone.rs` `release_waiting`), so the next
/// start's batch releases a delete the killed start left held, and nothing is lost; that rule is read, not killed.
#[tokio::test]
async fn an_admins_delete_is_released_once_after_a_kill_around_the_batch_zone() {
    if !support::offline::unshare_or_skip("the kill sample runs real clients") {
        return;
    }
    let (template, device) = account().await;
    let scratch = tempfile::tempdir().expect("temp dir");
    let stanza_file = scratch.path().join("delete.bin");
    let mut peer = Peer::new(&device, None).await;
    let stanza = peer
        .group_message_with_id(
            &group_jid(),
            &revoke_of_members_message(),
            PEER_REVOKE_ID,
            "text",
        )
        .await;
    std::fs::write(&stanza_file, marshal(&stanza).expect("the stanza marshals")).expect("write");
    let (status, said) = run_kill_child(template.path(), "prepare", Some(&stanza_file), None);
    assert!(status.success(), "the preparation failed: {said}");
    // The kills per point, and the states the process was killed in: (list stated, delete still waiting) and whether the
    // delete had been given to the core (DUR-9).
    let mut kills_at = Vec::new();
    let mut states = std::collections::BTreeSet::new();
    for point in [
        fp::INSIDE_EVENT_WRITE,
        fp::AFTER_EVENT_WRITE,
        fp::INSIDE_STAGE2,
        fp::AFTER_STAGE2,
    ] {
        let mut at_this_point = 0;
        for n in 1..=MAX_HITS + 1 {
            assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
            let dir = tempfile::tempdir().expect("temp dir");
            copy_dir(template.path(), dir.path());
            let what = format!("{point}:{n}");
            let (status, said) = run_kill_child(dir.path(), "run", None, Some(&what));
            if status.success() {
                assert_released(dir.path(), &what);
                assert_eq!(delivered(dir.path()), 1, "{what}: one delete, one delivery");
                break;
            }
            assert_eq!(
                status.signal(),
                Some(6),
                "{what} must abort: {status:?} {said}"
            );
            assert!(said.contains("hit: aborting"), "{what}: {said}");
            let seen_at_kill = delivered(dir.path());
            let held = held_deletes(dir.path());
            let stated_at_kill = {
                let conn = reopen(dir.path());
                (
                    count(
                        &conn,
                        "SELECT count(*) FROM group_state WHERE members_stated <> 0",
                    ) == 1,
                    count(
                        &conn,
                        "SELECT count(*) FROM inbox WHERE members_wait IS NOT NULL",
                    ) == 1,
                )
            };
            let (status, said) = run_kill_child(dir.path(), "run", None, None);
            assert!(status.success(), "{what}: the recovery failed: {said}");
            assert_released(dir.path(), &what);
            let expected = if seen_at_kill == 0 { 1 } else { 1 + held };
            assert_eq!(
                delivered(dir.path()),
                expected,
                "{what}: the delete is given to the core once (given at the kill: {seen_at_kill}, still held: {held})"
            );
            eprintln!(
                "kill {what}: given at the kill {seen_at_kill}, held {held}, list stated at the kill: {}, delete waiting at the kill: {}",
                stated_at_kill.0, stated_at_kill.1
            );
            at_this_point += 1;
            states.insert((stated_at_kill, seen_at_kill > 0));
        }
        kills_at.push((point, at_this_point));
    }
    for (point, n) in &kills_at {
        assert!(
            *n >= 1,
            "{point} never killed the child: the sample says nothing about it ({kills_at:?})"
        );
    }
    assert!(
        states.contains(&((true, false), false)),
        "no kill landed with the list stated, the delete released and not yet given ({states:?})"
    );
    assert!(
        states.contains(&((false, true), false)),
        "no kill landed before the list was stated, with the delete still waiting ({states:?})"
    );
}

// ---- the parent and the child ----------------------------------------------------------------------

/// What the child's output and logs must never hold (D10): the fixture's numbers, LIDs, groups and names.
fn secrets() -> Vec<&'static str> {
    vec![
        peer::ACCOUNT_PN,
        peer::ACCOUNT_LID,
        peer::PEER_LID,
        G,
        M_LID,
        M_PN,
        SUBJECT,
        NOTIFY,
        "Synthetic",
    ]
}

const CASE: &str = "WA_GROUPS_FLOW_CASE";

/// One parent per case; nothing the child printed holds a secret of the fixture.
fn parent(case: &str) {
    let output = support::offline::run_child_output(module_path!(), "flow_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn a_start_states_the_group_once_and_a_notice_moves_a_role_over_the_wire() {
    parent("start");
}

#[test]
fn an_admins_delete_waits_for_the_member_list_and_the_next_batch_releases_it_over_the_wire() {
    parent("wait");
}

#[test]
fn our_admin_delete_reaches_the_member_as_a_revoke_with_the_authors_key_over_the_wire() {
    parent("admin_delete");
}

#[test]
fn the_timer_set_reaches_the_peer_and_is_confirmed_after_the_ack_over_the_wire() {
    parent("timer");
}

#[test]
fn a_failed_notice_zone_withholds_the_ack_and_the_redelivery_is_written_once_over_the_wire() {
    parent("notice_fails");
}

#[test]
fn a_failed_groups_dirty_zone_withholds_the_ack_and_the_redelivery_marks_once_over_the_wire() {
    parent("dirty_fails");
}

/// The child (a no-op as an ordinary test), with every log line recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "start" => a_start_states_the_group_once_and_a_notice_moves_a_role().await,
        "wait" => an_admins_delete_waits_for_the_member_list_and_the_next_batch_releases_it().await,
        "admin_delete" => {
            our_admin_delete_reaches_the_member_as_a_revoke_with_the_authors_key().await
        }
        "timer" => the_timer_set_reaches_the_peer_and_is_confirmed_after_the_ack().await,
        "notice_fails" => {
            a_failed_notice_zone_withholds_the_ack_and_the_redelivery_is_written_once().await
        }
        "dirty_fails" => {
            a_failed_groups_dirty_zone_withholds_the_ack_and_the_redelivery_marks_once().await
        }
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert_eq!(
        log.leaks(&secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a log line holds something shaped like a key or a token"
    );
}
