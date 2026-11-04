//! TH6a, the send core against the offline fake.
//!
//! S4, `prepare_*`: ZONE PREPARE with no actor and no socket. A seeded file, the door, and the rows read back: the
//! fake's peer and phone (a second implementation, rule 9) open what was sealed; each receiver's fate is a literal row;
//! a refusal leaves nothing; no column holds a text. Nothing here reaches a wire: the frames are S5a's. The rows are
//! SEALED, which is no privacy boundary (CK and DGK rest in the same file): what a test proves is that the text does not
//! lie in a column to be read with a grep.
//!
//! S5a, `order_*`: the frames. libthreema's `Reflect` and `ReflectAck` against the fake's independent parsers, then the
//! real actor over the fake's parts (parent/child pairs, a job from the door): each frame after the zone that allows it,
//! the socket's other work beside a wait, what the socket does when it is going. ORDER IS READ FROM THE FAKE'S TRACE, not
//! a server rule: a real server would not refuse a box before a reflect. The windows of a kill are S8's.
//!
//! S5b, `deadline_*`, `cooling_*`, `stalled_*`, `sweep_*`: what happens when an expected ack does not come or the server
//! will not take a row. The two clocks moved together to literal times (`at(s)`), no wall clock.
//!
//! S6, `contract_*`, `clear_*`: `Command::Transmit` behind the parts' test door, every row of the contract with a literal
//! outcome (an error's log-safe detail is not compared), and what the user's clear takes of the queue. Each answer is read
//! back against the frames the fake saw: what the core is told and what went out must agree.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Command, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::ZoneDb;
use kourier_db::rusqlite::types::ValueRef;
use kourier_db::rusqlite::{Connection, params};
use kourier_model::{
    BackendError, Body, CannotSend, CmdId, ConnState, ConvKey, ErrorKind, MsgTarget,
    RecipientState, TargetAuthor, UserId,
};
use kourier_threema::StoreError;
use kourier_threema::failpoints as fp;
use kourier_threema::store::{
    self, NewBody, NewJob, NewMessage, NonceScope, ReceiverState, SealedUpdate, SendEnd,
    SendOrigin, SendReceiver, SendRefused, SendState, SendTask, Specific, To,
};
use libthreema::common::{Nonce, ThreemaId};
use libthreema::d2m::payload::{IncomingPayload, OutgoingPayload, Reflect, ReflectFlags};
use libthreema::protobuf::common::{GroupIdentity, Identities};
use libthreema::protobuf::d2d::envelope::Content;
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;

use super::harness::{self, DEVICE, H, ME, Scene, scene, traced};
use super::reflect::{PEER, PHONE, connect, online, phone_of, wait_acks};
use crate::mock::d2m::{group as mock_group, new_device, online as online_client, pump};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::csp::Sent;
use crate::support::mock::d2m::{
    ConnId, Mediator, ReflectSeen, datagram, kind, parse_reflect, reflect_ack_payload, split,
};
use crate::support::mock::envelope::{Author, ContactDelta, Msg, Reflect as _};
use crate::support::mock::peer::{Discard, Opened};
use crate::support::mock::phone::{Phone, open_envelope};
use crate::support::piped::{self, Mode};
use crate::support::warns::Warns;
use crate::support::{db_path, key};

const ANN: &str = "PEER0001";
const BOB: &str = "PEER0002";
const CRE: &str = "PEER0003";
const DEE: &str = "PEER0004";
const EVE: &str = "PEER0005";
const GATE: &str = "*GATEWAY";

/// A message id's text, and the `MessageId.0` of its wire bytes 01..08.
const LOCAL: &str = "0102030405060708";
const ID: u64 = 0x0807_0605_0403_0201;
/// The message's time, and the same in seconds.
const T0: i64 = 1_761_500_000_123;
const T0_S: u32 = 1_761_500_000;
/// When the zone runs.
const NOW: i64 = T0 + 7;
const GROUP: u64 = 0x42;
/// The flags of a text: send a push.
const PUSH: u8 = 0x01;

/// A seeded file of a scene, open.
struct File {
    _dir: tempfile::TempDir,
    db: ZoneDb,
}

impl File {
    fn of(s: &Scene) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        s.seed_file(dir.path());
        let db = store::open(&db_path(dir.path()), &key()).expect("open the file");
        Self { _dir: dir, db }
    }

    /// The one task of the file.
    #[track_caller]
    fn task(&self) -> SendTask {
        let mut tasks = self.db.read(store::send_tasks).expect("the tasks");
        assert_eq!(tasks.len(), 1, "one task");
        tasks.remove(0)
    }

    fn receivers(&self, task: i64) -> Vec<SendReceiver> {
        self.db
            .read(|c: &Connection| store::send_receivers(c, task))
            .expect("the receivers")
    }

    fn count(&self, table: &str) -> i64 {
        self.db
            .read(|c: &Connection| {
                c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .map_err(kourier_db::Error::from)
            })
            .expect("a count")
    }

    /// Neither table of the queue nor a nonce store holds anything.
    #[track_caller]
    fn holds_nothing(&self, what: &str) {
        for table in ["send_task", "send_receiver", "csp_nonce", "d2d_nonce"] {
            assert_eq!(self.count(table), 0, "{what}: {table}");
        }
    }

    fn has_nonce(&self, scope: NonceScope, hash: &[u8; 32]) -> bool {
        self.db
            .read(|c: &Connection| store::contains_nonce(c, scope, hash))
            .expect("a nonce's look")
    }

    /// Rewrites a stored contact.
    fn bend_contact(&self, identity: &str, bend: impl FnOnce(&mut d2d_sync::Contact)) {
        bend_contact(&self.db, identity, bend);
    }

    /// Rewrites a stored group.
    fn bend_group(&self, creator: &str, group_id: u64, bend: impl FnOnce(&mut d2d_sync::Group)) {
        bend_group(&self.db, creator, group_id, bend);
    }
}

/// Rewrites a stored contact.
fn bend_contact(db: &ZoneDb, identity: &str, bend: impl FnOnce(&mut d2d_sync::Contact)) {
    db.zone(|tx| {
        let sync: Vec<u8> = tx.query_row(
            "SELECT sync FROM contact WHERE identity = ?1",
            params![identity],
            |r| r.get(0),
        )?;
        let mut contact = d2d_sync::Contact::decode(sync.as_slice()).expect("a contact");
        bend(&mut contact);
        tx.execute(
            "UPDATE contact SET sync = ?2 WHERE identity = ?1",
            params![identity, contact.encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("bend a contact");
}

/// Rewrites a stored group.
fn bend_group(db: &ZoneDb, creator: &str, group_id: u64, bend: impl FnOnce(&mut d2d_sync::Group)) {
    let id = i64::from_ne_bytes(group_id.to_ne_bytes());
    db.zone(|tx| {
        let sync: Vec<u8> = tx.query_row(
            "SELECT sync FROM grp WHERE creator = ?1 AND group_id = ?2",
            params![creator, id],
            |r| r.get(0),
        )?;
        let mut group = d2d_sync::Group::decode(sync.as_slice()).expect("a group");
        bend(&mut group);
        tx.execute(
            "UPDATE grp SET sync = ?3 WHERE creator = ?1 AND group_id = ?2",
            params![creator, id, group.encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("bend a group");
}

fn contact(identity: &str) -> To {
    To::Contact(identity.to_owned())
}

fn group(creator: &str, group_id: u64) -> To {
    To::Group {
        creator: creator.to_owned(),
        group_id,
    }
}

/// A `Transmit` of a text.
fn text_to(to: To, text: &str) -> NewMessage {
    NewMessage {
        local: LOCAL.to_owned(),
        kind: "text",
        to,
        created_at: T0,
        body: NewBody::Text(text.to_owned()),
        specific: Vec::new(),
        only: None,
    }
}

/// A job of a body the fake wrote (L9).
fn raw_job(to: To, ty: u8, body: Vec<u8>, distributes: bool) -> NewJob {
    NewJob {
        kind: "test-job",
        to,
        body: NewBody::Raw {
            ty,
            body,
            distributes,
        },
        only: None,
    }
}

fn enqueue(file: &File, job: &NewJob) -> Result<i64, StoreError> {
    file.db.zone(|tx| store::enqueue_job(tx, job, NOW))
}

/// What the chat server would keep of a stored box.
fn sent_of(payload: &[u8]) -> Sent {
    Sent {
        sender: payload[..8].try_into().expect("8 bytes"),
        receiver: payload[8..16].try_into().expect("8 bytes"),
        message_id: u64::from_le_bytes(payload[16..24].try_into().expect("8 bytes")),
        created_at: u32::from_le_bytes(payload[24..28].try_into().expect("4 bytes")),
        flags: payload[28],
        bytes: payload.to_vec(),
        conn: 0,
        replaced: false,
    }
}

/// The stored receiver opened at the fake's peer of its ID.
#[track_caller]
fn opened(s: &Scene, receiver: &SendReceiver) -> Opened {
    let payload = receiver.payload.as_deref().expect("a pending receiver");
    s.world
        .peer(&receiver.identity)
        .open_sent(&sent_of(payload), 0)
        .expect("the peer opens the box")
}

/// The sealed reflect opened as the phone does.
#[track_caller]
fn reflected(s: &Scene, sealed: &[u8]) -> Content {
    let envelope = open_envelope(&s.keys, sealed).expect("the phone opens the envelope");
    assert_eq!(envelope.device_id, DEVICE);
    assert_eq!(
        envelope.protocol_version,
        d2d::ProtocolVersion::V03 as u32,
        "the version the reflections carry"
    );
    assert!(
        envelope.encoded_len() >= 64,
        "an envelope is padded to 64 bytes at least"
    );
    envelope.content.expect("a content")
}

#[track_caller]
fn outgoing(s: &Scene, task: &SendTask) -> d2d::OutgoingMessage {
    let sealed = task.reflect_env.as_deref().expect("a sealed reflect");
    match reflected(s, sealed) {
        Content::OutgoingMessage(message) => message,
        _ => panic!("the reflect is an OutgoingMessage"),
    }
}

fn conversation_contact(identity: &str) -> d2d::ConversationId {
    d2d::ConversationId {
        id: Some(d2d::conversation_id::Id::Contact(identity.to_owned())),
    }
}

fn conversation_group(creator: &str, group_id: u64) -> d2d::ConversationId {
    d2d::ConversationId {
        id: Some(d2d::conversation_id::Id::Group(GroupIdentity {
            group_id,
            creator_identity: creator.to_owned(),
        })),
    }
}

/// The nonce store's form of a nonce, by libthreema's own line (S2 pinned the code's call with literals).
fn stored_form(nonce: [u8; 24]) -> [u8; 32] {
    Nonce(nonce)
        .hash_with(ThreemaId::try_from(ME).expect("the own ID"))
        .0
}

/// The rows are exactly these receivers in these states. A failure says the states and never an ID.
#[track_caller]
fn same_fates(receivers: &[SendReceiver], expected: &[(&str, ReceiverState)], what: &str) {
    let found: Vec<(&str, ReceiverState)> = receivers
        .iter()
        .map(|r| (r.identity.as_str(), r.state))
        .collect();
    let states: Vec<ReceiverState> = receivers.iter().map(|r| r.state).collect();
    assert!(found == expected, "{what}: the rows' states are {states:?}");
}

fn block(ids: &[&str]) -> d2d_sync::Settings {
    d2d_sync::Settings {
        blocked_identities: Some(Identities {
            identities: ids.iter().map(|i| (*i).to_owned()).collect(),
        }),
        ..Default::default()
    }
}

// ---- a message, sealed and opened ----------------------------------------------------------------------------------

/// A text to a contact: the row with literal fields, the box as its peer opens it, the reflect as the phone opens it,
/// both nonces durable in the rows that use them.
#[test]
fn prepare_a_text_to_a_contact_opens_at_the_peer_and_at_the_phone() {
    let s = scene(7_101).with_peer(ANN).with_nickname("Kourier Test");
    let file = File::of(&s);
    let task_id = store::prepare_outgoing(&file.db, &text_to(contact(ANN), "hello"), NOW, &[])
        .expect("prepare");

    let task = file.task();
    assert_eq!(task.id, task_id);
    assert_eq!(task.origin, SendOrigin::Transmit);
    assert_eq!(task.kind, "text");
    assert_eq!(task.local.as_deref(), Some(LOCAL));
    assert_eq!(task.message_id, ID);
    assert!(
        d2d::ConversationId::decode(task.conversation.as_slice()).expect("a conversation")
            == conversation_contact(ANN),
        "the conversation differs"
    );
    assert_eq!(task.created_at, T0);
    assert_eq!(task.state, SendState::Prepared);
    assert!(task.has_update, "a text owes the Sent update");
    assert_eq!(task.sent_at, None);
    assert_eq!(task.attempts, 0);
    assert_eq!(task.queued_at, NOW);
    assert_eq!(task.finished_at, None);
    assert!(task.update_env.is_none(), "no update is sealed yet");

    let receivers = file.receivers(task.id);
    same_fates(
        &receivers,
        &[(ANN, ReceiverState::Pending)],
        "the receivers' fates",
    );
    let ann = opened(&s, &receivers[0]);
    assert!(ann.sender == ME, "the sender differs");
    assert_eq!(ann.message_id, ID);
    assert_eq!(ann.created_at_s, T0_S);
    assert_eq!(ann.created_at_ms, 1_761_500_000_123);
    assert_eq!(ann.flags, PUSH);
    assert_eq!(ann.ty, 0x01);
    assert!(ann.body == b"hello", "the body differs");
    assert_eq!(ann.nickname.as_deref(), Some("Kourier Test"));
    assert_eq!(ann.discard, None);

    let message = outgoing(&s, &task);
    assert!(
        message.conversation == Some(conversation_contact(ANN)),
        "the conversation differs"
    );
    assert_eq!(message.message_id, ID);
    assert_eq!(message.created_at, 1_761_500_000_123);
    assert_eq!(message.r#type, 0x01);
    assert!(message.body == b"hello", "the body differs");
    assert_eq!(message.thread_message_id, None);
    assert!(message.nonces == [ann.nonce.to_vec()], "the nonces differ");

    let reflect = task.reflect_env.as_deref().expect("a sealed reflect");
    assert_eq!(
        task.bytes,
        i64::try_from(reflect.len() + ann.bytes.len()).expect("a length"),
        "the budget counts the reflect and the box"
    );
    assert_eq!(file.count("csp_nonce"), 1);
    assert!(file.has_nonce(NonceScope::Csp, &stored_form(ann.nonce)));
    assert_eq!(file.count("d2d_nonce"), 1);
    let d2d_nonce: [u8; 24] = reflect[..24].try_into().expect("the nonce ahead");
    assert!(file.has_nonce(NonceScope::D2d, &stored_form(d2d_nonce)));
    s.world.violations().assert_empty();
}

/// A group text: every member and the creator get the same id and time, each under its own nonce, in the member
/// container; `OutgoingMessage.nonces` lists every box's nonce, each once, and the body is the container.
#[test]
fn prepare_a_group_text_goes_to_every_member_and_the_creator() {
    let s = scene(7_102).with_members(CRE, GROUP, &[ANN, BOB]);
    let file = File::of(&s);
    store::prepare_outgoing(&file.db, &text_to(group(CRE, GROUP), "hi all"), NOW, &[])
        .expect("prepare");

    let task = file.task();
    let receivers = file.receivers(task.id);
    same_fates(
        &receivers,
        &[
            (ANN, ReceiverState::Pending),
            (BOB, ReceiverState::Pending),
            (CRE, ReceiverState::Pending),
        ],
        "the members and the creator the spec's list leaves out",
    );
    let container = body::group_member_container(CRE, GROUP, b"hi all");
    let boxes: Vec<Opened> = receivers.iter().map(|r| opened(&s, r)).collect();
    for one in &boxes {
        assert_eq!(one.message_id, ID);
        assert_eq!(one.created_at_ms, 1_761_500_000_123);
        assert_eq!(one.flags, PUSH);
        assert_eq!(one.ty, 0x41);
        assert!(one.body == container, "the body differs");
    }
    let mut nonces: Vec<[u8; 24]> = boxes.iter().map(|o| o.nonce).collect();
    nonces.dedup();
    assert_eq!(nonces.len(), 3, "a nonce per receiver");

    let message = outgoing(&s, &task);
    assert!(
        message.conversation == Some(conversation_group(CRE, GROUP)),
        "the conversation differs"
    );
    assert_eq!(message.r#type, 0x41);
    assert!(message.body == container, "the body differs");
    assert!(
        message.nonces
            == [
                boxes[0].nonce.to_vec(),
                boxes[1].nonce.to_vec(),
                boxes[2].nonce.to_vec(),
            ],
        "OutgoingMessage.nonces lists exactly the boxes' nonces"
    );
    assert_eq!(file.count("csp_nonce"), 3);
    for one in &boxes {
        assert!(file.has_nonce(NonceScope::Csp, &stored_form(one.nonce)));
    }
    s.world.violations().assert_empty();
}

/// A group of mine does not get a box for me, and it holds no creator to add.
#[test]
fn prepare_my_own_group_has_no_box_for_me() {
    let s = scene(7_103).with_members(ME, GROUP, &[ANN]);
    let file = File::of(&s);
    file.bend_group(ME, GROUP, |g| {
        let members = g.member_identities.as_mut().expect("members");
        members.identities.push(ME.to_owned());
    });
    store::prepare_outgoing(&file.db, &text_to(group(ME, GROUP), "mine"), NOW, &[])
        .expect("prepare");
    let task = file.task();
    let receivers = file.receivers(task.id);
    same_fates(
        &receivers,
        &[(ANN, ReceiverState::Pending)],
        "the receivers' fates",
    );
    assert!(
        opened(&s, &receivers[0]).body == body::group_member_container(ME, GROUP, b"mine"),
        "the body differs"
    );
    s.world.violations().assert_empty();
}

/// A notes group: nobody gets a box, the reflect goes with an empty list, the update stays owed, no CSP nonce.
#[test]
fn prepare_a_notes_group_reflects_and_writes_no_box() {
    let s = scene(7_104).with_members(ME, GROUP, &[]);
    let file = File::of(&s);
    store::prepare_outgoing(&file.db, &text_to(group(ME, GROUP), "a note"), NOW, &[])
        .expect("prepare");
    let task = file.task();
    assert_eq!(task.state, SendState::Prepared);
    assert!(task.has_update);
    assert!(file.receivers(task.id).is_empty());
    let message = outgoing(&s, &task);
    assert!(
        message.body == body::group_member_container(ME, GROUP, b"a note"),
        "the body differs"
    );
    assert!(message.nonces.is_empty(), "the spec's legitimate case");
    assert_eq!(file.count("csp_nonce"), 0);
    assert_eq!(file.count("d2d_nonce"), 1);
    s.world.violations().assert_empty();
}

// ---- each receiver's fate, as rows ---------------------------------------------------------------------------------

/// The fates side by side: the user gets no row, a blocked, an invalid and an unstored receiver get their state and no
/// box, and the reflect's list, the budget and the nonce store hold the two that get one, and nobody else.
#[test]
fn prepare_each_fate_is_its_row_and_the_nonces_list_those_who_get_a_box() {
    let mut s = scene(7_105)
        .with_members(CRE, GROUP, &[ANN, BOB, DEE])
        .with_settings(block(&[BOB]));
    s.members
        .get_mut(&(CRE.to_owned(), GROUP))
        .expect("the group")
        .extend([ME.to_owned(), EVE.to_owned()]);
    let file = File::of(&s);
    file.bend_contact(DEE, |c| {
        c.activity_state = Some(d2d_sync::contact::ActivityState::Invalid as i32);
    });
    store::prepare_outgoing(&file.db, &text_to(group(CRE, GROUP), "some go"), NOW, &[])
        .expect("prepare");

    let task = file.task();
    let receivers = file.receivers(task.id);
    same_fates(
        &receivers,
        &[
            (ANN, ReceiverState::Pending),
            (BOB, ReceiverState::Blocked),
            (CRE, ReceiverState::Pending),
            (DEE, ReceiverState::Invalid),
            (EVE, ReceiverState::NoKey),
        ],
        "the user has no row",
    );
    for receiver in &receivers {
        assert_eq!(
            receiver.payload.is_some(),
            receiver.state == ReceiverState::Pending,
            "a box only for a pending receiver"
        );
    }
    let (ann, cre) = (opened(&s, &receivers[0]), opened(&s, &receivers[2]));
    assert!(
        outgoing(&s, &task).nonces == [ann.nonce.to_vec(), cre.nonce.to_vec()],
        "OutgoingMessage.nonces lists exactly the Send receivers"
    );
    assert_eq!(
        file.count("csp_nonce"),
        2,
        "no nonce for a receiver with no box"
    );
    let reflect = task.reflect_env.as_deref().expect("a reflect").len();
    assert_eq!(
        task.bytes,
        i64::try_from(reflect + ann.bytes.len() + cre.bytes.len()).expect("a length")
    );
    s.world.violations().assert_empty();
}

/// The one WARN line per rule says a count and no ID; a block is the user's choice and quiet.
#[test]
fn prepare_the_left_out_receivers_are_counted_in_one_line_per_rule() {
    use crate::support::warns::Warns;
    if crate::support::in_own_process!() {
        return;
    }
    let warns = Warns::install();
    let mut s = scene(7_106)
        .with_members(CRE, GROUP, &[ANN, BOB, DEE])
        .with_settings(block(&[BOB]));
    s.members
        .get_mut(&(CRE.to_owned(), GROUP))
        .expect("the group")
        .extend([ME.to_owned(), EVE.to_owned(), "PEER0006".to_owned()]);
    let file = File::of(&s);
    file.bend_contact(DEE, |c| {
        c.activity_state = Some(d2d_sync::contact::ActivityState::Invalid as i32);
    });
    store::prepare_outgoing(&file.db, &text_to(group(CRE, GROUP), "some go"), NOW, &[])
        .expect("prepare");
    assert_eq!(
        warns.at(tracing::Level::WARN),
        [
            "receivers of a send left out: 1 by send-receiver-own",
            "receivers of a send left out: 1 by send-receiver-invalid",
            "receivers of a send left out: 2 by send-receiver-no-key",
        ]
    );
    s.world.violations().assert_empty();
}

/// Everybody blocked, or a 1:1 chat with a blocked contact: no box, and the reflect and the update still owed (Desktop's
/// case: the device group still learns of the message).
#[test]
fn prepare_a_blocked_contact_gets_no_box_and_the_device_group_is_still_told() {
    let s = scene(7_107).with_peer(ANN).with_settings(block(&[ANN]));
    let file = File::of(&s);
    store::prepare_outgoing(&file.db, &text_to(contact(ANN), "unseen"), NOW, &[]).expect("prepare");
    let task = file.task();
    same_fates(
        &file.receivers(task.id),
        &[(ANN, ReceiverState::Blocked)],
        "the receivers' fates",
    );
    assert!(task.has_update);
    assert!(outgoing(&s, &task).nonces.is_empty());
    assert_eq!(file.count("csp_nonce"), 0);
    s.world.violations().assert_empty();
}

/// Under `BLOCK_UNKNOWN` a contact that is not direct goes while it shares a group with the user and is blocked once it
/// shares none: the file's own groups are what the steps read.
#[test]
fn prepare_the_unknown_contact_policy_reads_the_stored_groups() {
    use d2d_sync::contact::AcquaintanceLevel;
    use d2d_sync::group::UserState;
    use d2d_sync::settings::UnknownContactPolicy;
    let strict = d2d_sync::Settings {
        unknown_contact_policy: Some(UnknownContactPolicy::BlockUnknown as i32),
        ..Default::default()
    };
    let s = scene(7_108)
        .with_members(CRE, GROUP, &[ANN])
        .with_settings(strict);
    let file = File::of(&s);
    file.bend_contact(ANN, |c| {
        c.acquaintance_level = Some(AcquaintanceLevel::GroupOrDeleted as i32);
    });
    store::prepare_outgoing(&file.db, &text_to(contact(ANN), "known"), NOW, &[]).expect("prepare");
    let first = file.task();
    same_fates(
        &file.receivers(first.id),
        &[(ANN, ReceiverState::Pending)],
        "in a group the user is in: not blocked",
    );

    file.bend_group(CRE, GROUP, |g| {
        g.user_state = Some(UserState::Left as i32);
    });
    let mut later = text_to(contact(ANN), "unknown");
    later.local = "1102030405060708".to_owned();
    let task = store::prepare_outgoing(&file.db, &later, NOW, &[]).expect("prepare");
    same_fates(
        &file.receivers(task),
        &[(ANN, ReceiverState::Blocked)],
        "the group is left: blocked",
    );
    s.world.violations().assert_empty();
}

// ---- no plaintext --------------------------------------------------------------------------------------------------

/// Every value of `table`, read back through SQL: the count of its columns, of its rows and the bytes of each value.
fn every_value(file: &File, table: &str) -> (usize, usize, Vec<Vec<u8>>) {
    file.db
        .read(|c: &Connection| {
            let mut statement = c.prepare(&format!("SELECT * FROM {table}"))?;
            let columns = statement.column_count();
            let mut rows = statement.query([])?;
            let (mut count, mut values) = (0, Vec::new());
            while let Some(row) = rows.next()? {
                count += 1;
                for column in 0..columns {
                    match row.get_ref(column)? {
                        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                            values.push(bytes.to_vec())
                        }
                        ValueRef::Integer(n) => values.push(n.to_string().into_bytes()),
                        ValueRef::Null | ValueRef::Real(_) => {}
                    }
                }
            }
            Ok::<_, kourier_db::Error>((columns, count, values))
        })
        .expect("read a table")
}

fn holds(values: &[Vec<u8>], marker: &[u8]) -> bool {
    values
        .iter()
        .any(|value| value.windows(marker.len()).any(|w| w == marker))
}

/// Where `marker` stands in `values`: the index of the value and the offset in it, for every place it is found.
fn finds(values: &[Vec<u8>], marker: &[u8]) -> Vec<(usize, usize)> {
    let mut places = Vec::new();
    for (at, value) in values.iter().enumerate() {
        for (offset, window) in value.windows(marker.len()).enumerate() {
            if window == marker {
                places.push((at, offset));
            }
        }
    }
    places
}

/// The text and the nickname rest sealed: no column of either table holds the text, in any state of the row, and none
/// holds the nickname EXCEPT the legacy nickname field of a `*` receiver's head (S-L3): by protocol (the head is in the
/// clear on the wire and the field carries the user's own nickname, also in `own_profile` in the same file), cut at 32
/// bytes, found at offset 32 of that one stored box and nowhere else while the box waits for its ack. (Sealed, not
/// private: the key is in the same file.) The peer and the phone find the markers, so they were there to be found, and a
/// column that lies in the clear would show them.
#[test]
fn prepare_no_column_of_the_rows_holds_the_text_or_the_nickname_but_a_gateway_heads_legacy_nickname()
 {
    const TEXT: &str = "MARKER-7f3a-THE-BODY";
    // 38 bytes: over the legacy field's 32, so what the head of a Gateway's box holds is the cut.
    const NICK: &str = "MARKER-2b9e-THE-NICK-AND-ITS-LONG-TAIL";
    let cut = &NICK[..32];
    assert_eq!(NICK.len(), 38, "the nickname is longer than the field");
    let s = scene(7_109)
        .with_members(CRE, GROUP, &[ANN, BOB, GATE])
        .with_nickname(NICK);
    let file = File::of(&s);
    store::prepare_outgoing(&file.db, &text_to(group(CRE, GROUP), TEXT), NOW, &[])
        .expect("prepare");
    let task = file.task();
    let receivers = file.receivers(task.id);
    same_fates(
        &receivers,
        &[
            (GATE, ReceiverState::Pending),
            (ANN, ReceiverState::Pending),
            (BOB, ReceiverState::Pending),
            (CRE, ReceiverState::Pending),
        ],
        "the receivers' fates",
    );
    let gateway = receivers
        .iter()
        .find(|receiver| receiver.identity == GATE)
        .expect("the Gateway's row");
    let gateway_box = gateway.payload.clone().expect("the Gateway's box");
    assert!(
        gateway_box[32..64] == *cut.as_bytes(),
        "the Gateway's legacy field is not the cut nickname"
    );
    for receiver in receivers.iter().filter(|r| r.identity != GATE) {
        let payload = receiver.payload.as_deref().expect("a pending receiver");
        assert!(
            payload[32..64] == [0_u8; 32],
            "the legacy field of a receiver that is no Gateway is not zero"
        );
    }

    // `waiting`: the Gateway's box still waits for its ack, so the cut stands in it and in nothing else.
    let look = |state: &str, waiting: bool| {
        let (task_columns, task_rows, tasks) = every_value(&file, "send_task");
        let (receiver_columns, receiver_rows, boxes) = every_value(&file, "send_receiver");
        assert_eq!(
            (task_columns, task_rows, receiver_columns, receiver_rows),
            (16, 1, 4, 4),
            "{state}: every column of every row was read"
        );
        for marker in [TEXT, NICK] {
            assert!(
                !holds(&tasks, marker.as_bytes()),
                "{state}: send_task holds a marker in the clear"
            );
            assert!(
                !holds(&boxes, marker.as_bytes()),
                "{state}: send_receiver holds a marker in the clear"
            );
        }
        assert!(
            finds(&tasks, cut.as_bytes()).is_empty(),
            "{state}: send_task holds the cut nickname"
        );
        let places = finds(&boxes, cut.as_bytes());
        if waiting {
            assert_eq!(
                places.len(),
                1,
                "{state}: the places of the cut nickname in send_receiver"
            );
            let (at, offset) = places[0];
            assert_eq!(offset, 32, "{state}: the offset of the cut nickname");
            assert!(
                boxes[at] == gateway_box,
                "{state}: the cut nickname stands in another value than the Gateway's box"
            );
        } else {
            assert!(
                places.is_empty(),
                "{state}: the cut nickname stays after the box was emptied"
            );
        }
    };
    look("prepared", true);

    let opened_box = opened(&s, gateway);
    assert!(
        holds(&[opened_box.body], TEXT.as_bytes()),
        "the peer finds the text"
    );
    assert_eq!(
        opened_box.nickname.as_deref(),
        Some(NICK),
        "the peer finds the whole nickname in the metadata"
    );
    assert!(
        holds(&[outgoing(&s, &task).body], TEXT.as_bytes()),
        "the phone finds the text"
    );

    store::mark_send_reflected(&file.db, task.id).expect("reflected");
    look("reflected", true);
    let (envelope, nonce) = file
        .db
        .read(|c: &Connection| store::seal_sent_update(c, &task))
        .expect("the update");
    let acked = [GATE, ANN, BOB, CRE];
    let update = SealedUpdate {
        envelope: &envelope,
        nonce,
    };
    assert_eq!(
        store::mark_send_acks(&file.db, task.id, &acked, Some(update), NOW),
        Ok(SendEnd::Acked)
    );
    look("acked", false);
    s.world.violations().assert_empty();
}

// ---- refused: nothing written --------------------------------------------------------------------------------------

/// Each refusal by its reason, through the zone: nothing of it stays in either table or nonce store.
#[test]
fn prepare_a_refused_message_writes_nothing() {
    use d2d_sync::group::UserState;
    let s = scene(7_110)
        .with_peer(ANN)
        .with_members(CRE, GROUP, &[ANN])
        .with_members(CRE, GROUP + 1, &[ANN])
        .with_members(CRE, GROUP + 2, &[ANN]);
    let file = File::of(&s);
    file.bend_group(CRE, GROUP + 1, |g| {
        g.user_state = Some(UserState::Left as i32);
    });
    file.bend_group(CRE, GROUP + 2, |g| {
        g.user_state = Some(UserState::Kicked as i32);
    });
    let refused = |message: &NewMessage, reason: SendRefused, what: &str| {
        assert_eq!(
            store::prepare_outgoing(&file.db, message, NOW, &[]),
            Err(StoreError::Refused(reason)),
            "{what}"
        );
        file.holds_nothing(what);
    };

    for local in [
        "",
        "0102",
        "01020304050607",
        "0102030405060708a",
        "010203040506070G",
        "AABBCCDDEEFF0011",
    ] {
        let mut bad = text_to(contact(ANN), "x");
        bad.local = local.to_owned();
        refused(&bad, SendRefused::BadLocal, "a local that does not parse");
    }
    refused(
        &text_to(contact(DEE), "x"),
        SendRefused::UnknownContact,
        "a contact the file does not hold",
    );
    refused(
        &text_to(group(CRE, 0x99), "x"),
        SendRefused::UnknownGroup,
        "a group the file does not hold",
    );
    refused(
        &text_to(group(CRE, GROUP + 1), "x"),
        SendRefused::NotAMember,
        "a group the user left",
    );
    refused(
        &text_to(group(CRE, GROUP + 2), "x"),
        SendRefused::NotAMember,
        "a group the user was kicked from",
    );
    refused(
        &text_to(group("not an id", GROUP), "x"),
        SendRefused::BadId,
        "a creator that is no ID",
    );
    for number in [ty::INVALID, ty::UNKNOWN] {
        let mut bad = text_to(contact(ANN), "x");
        bad.body = NewBody::Raw {
            ty: number,
            body: vec![1],
            distributes: false,
        };
        refused(&bad, SendRefused::UnknownType, "a type no message has");
    }
    let mut bad = text_to(contact(ANN), "x");
    bad.specific = vec![Specific {
        identity: ANN.to_owned(),
        ty: ty::UNKNOWN,
        body: vec![1],
    }];
    refused(&bad, SendRefused::UnknownType, "a specific of no type");
    let mut bad = text_to(contact(ANN), "x");
    bad.only = Some(vec!["not an id".to_owned()]);
    refused(&bad, SendRefused::BadId, "a receiver that is no ID");

    // After all of them the file still takes the message.
    store::prepare_outgoing(&file.db, &text_to(contact(ANN), "x"), NOW, &[]).expect("it goes");
    s.world.violations().assert_empty();
}

/// P-L1: a type the chat server never acks (flag 0x04: the typing indicator, 0x90) has no place in a persistent queue, a
/// row of it would wait for an ack that never comes (`ACK_WITHIN`, a close, the same box on every socket). PREPARE
/// refuses it by its flags, whoever asks: a job, a `Transmit`, a receiver's specific message; nothing is written, no
/// nonce either, and the refusal comes before the conversation is looked at. The types around it still go.
#[test]
fn prepare_a_type_without_a_server_ack_is_refused() {
    let s = scene(7_119).with_peer(ANN).with_members(CRE, GROUP, &[ANN]);
    let file = File::of(&s);
    let not_queued = Err(StoreError::Refused(SendRefused::NotQueued));
    let typing = |to: To| raw_job(to, ty::TYPING_INDICATOR, vec![1], false);

    assert_eq!(
        enqueue(&file, &typing(contact(ANN))),
        not_queued,
        "a typing indicator as a job"
    );
    file.holds_nothing("a typing indicator as a job");

    let mut message = text_to(contact(ANN), "x");
    message.body = NewBody::Raw {
        ty: ty::TYPING_INDICATOR,
        body: vec![1],
        distributes: false,
    };
    assert_eq!(
        store::prepare_outgoing(&file.db, &message, NOW, &[]),
        not_queued,
        "a typing indicator as a Transmit"
    );
    file.holds_nothing("a typing indicator as a Transmit");

    let mut message = text_to(group(CRE, GROUP), "x");
    message.specific = vec![Specific {
        identity: ANN.to_owned(),
        ty: ty::TYPING_INDICATOR,
        body: vec![1],
    }];
    assert_eq!(
        store::prepare_outgoing(&file.db, &message, NOW, &[]),
        not_queued,
        "a typing indicator as a receiver's specific message"
    );
    file.holds_nothing("a typing indicator as a specific message");

    assert_eq!(
        enqueue(&file, &typing(contact(DEE))),
        not_queued,
        "a typing indicator to a contact the file does not hold"
    );
    file.holds_nothing("a typing indicator to a stranger");

    // The rest of the file still takes a message, and a type that carries other flags (a receipt: none) is no refusal.
    store::prepare_outgoing(&file.db, &text_to(contact(ANN), "x"), NOW, &[]).expect("a text goes");
    enqueue(
        &file,
        &raw_job(contact(ANN), ty::DELIVERY_RECEIPT, vec![1; 9], false),
    )
    .expect("a receipt goes");
    assert_eq!(file.count("send_task"), 2);
    s.world.violations().assert_empty();
}

/// A body no box can hold is refused with nothing written, wherever the seal found it; the same body goes to a notes
/// group, which has no box, up to the envelope's limit. (N and N+1 are `send::prepare`'s unit tests.)
#[test]
fn prepare_a_body_over_the_limits_is_refused_with_nothing_written() {
    let s = scene(7_111).with_peer(ANN).with_members(ME, GROUP, &[]);
    let file = File::of(&s);
    let too_large = Err(StoreError::Refused(SendRefused::TooLarge));
    let big_text = "x".repeat(8_200);
    assert_eq!(
        store::prepare_outgoing(&file.db, &text_to(contact(ANN), &big_text), NOW, &[]),
        too_large
    );
    file.holds_nothing("a box over its limit");
    let huge_text = "x".repeat(66_000);
    assert_eq!(
        store::prepare_outgoing(&file.db, &text_to(group(ME, GROUP), &huge_text), NOW, &[]),
        too_large
    );
    file.holds_nothing("an envelope over its limit");

    store::prepare_outgoing(&file.db, &text_to(group(ME, GROUP), &big_text), NOW, &[])
        .expect("no box in a notes group");
    assert_eq!(file.count("send_task"), 1);
    s.world.violations().assert_empty();
}

/// The seal and the rows are ONE zone: a zone that fails leaves neither, and the same message goes afterwards.
#[test]
fn prepare_a_zone_that_fails_leaves_nothing_of_the_seal() {
    if crate::support::in_own_process!() {
        return;
    }
    let s = scene(7_112).with_peer(ANN);
    let file = File::of(&s);
    let message = text_to(contact(ANN), "x");
    store::fail_send_zone(fp::INSIDE_SEND_PREPARE, 13);
    assert_eq!(
        store::prepare_outgoing(&file.db, &message, NOW, &[]),
        Err(StoreError::Sql {
            op: "a send zone failed",
            code: Some(13),
        })
    );
    file.holds_nothing("a failed zone");
    store::fail_send_zone("", 0);
    store::prepare_outgoing(&file.db, &message, NOW, &[]).expect("it goes");
    s.world.violations().assert_empty();
}

// ---- only ----------------------------------------------------------------------------------------------------------

/// R4: no ledger row and `only` is `Some`: `Send` fates go to `only` alone, the others have no row, `nonces` lists those,
/// and the row can reach `done`.
#[test]
fn prepare_under_only_gives_a_row_to_those_alone_and_can_reach_done() {
    let s = scene(7_113).with_members(CRE, GROUP, &[ANN, BOB]);
    let file = File::of(&s);
    let mut message = text_to(group(CRE, GROUP), "again");
    message.only = Some(vec![BOB.to_owned(), BOB.to_owned()]);
    let task_id = store::prepare_outgoing(&file.db, &message, NOW, &[]).expect("prepare");
    let task = file.task();
    let receivers = file.receivers(task_id);
    same_fates(
        &receivers,
        &[(BOB, ReceiverState::Pending)],
        "the receivers' fates",
    );
    let bob = opened(&s, &receivers[0]);
    assert!(
        bob.body == body::group_member_container(CRE, GROUP, b"again"),
        "the body differs"
    );
    assert!(
        outgoing(&s, &task).nonces == [bob.nonce.to_vec()],
        "the nonces differ"
    );
    assert_eq!(file.count("csp_nonce"), 1);

    store::mark_send_reflected(&file.db, task_id).expect("reflected");
    let (envelope, nonce) = file
        .db
        .read(|c: &Connection| store::seal_sent_update(c, &task))
        .expect("the update");
    let update = SealedUpdate {
        envelope: &envelope,
        nonce,
    };
    assert_eq!(
        store::mark_send_acks(&file.db, task_id, &[BOB], Some(update), NOW),
        Ok(SendEnd::Acked)
    );
    assert_eq!(
        store::mark_send_done(&file.db, task_id, T0 + 9, NOW + 1),
        Ok(SendEnd::Done)
    );
    let done = file.task();
    assert_eq!(done.state, SendState::Done);
    assert_eq!(done.sent_at, Some(T0 + 9));
    s.world.violations().assert_empty();
}

// ---- the Threema Gateway -------------------------------------------------------------------------------------------

/// A Gateway creator gets a group text only when the group's name carries the cloud; the rest of the group gets it either
/// way.
#[test]
fn prepare_a_gateway_creator_gets_a_text_only_under_the_cloud() {
    let s = scene(7_114).with_members(GATE, GROUP, &[ANN]);
    let file = File::of(&s);
    store::prepare_outgoing(&file.db, &text_to(group(GATE, GROUP), "plain"), NOW, &[])
        .expect("prepare");
    same_fates(
        &file.receivers(file.task().id),
        &[(ANN, ReceiverState::Pending)],
        "a creator's name without the cloud: no box to it",
    );

    file.bend_group(GATE, GROUP, |g| {
        g.name = Some("\u{2601} captured".to_owned())
    });
    let mut later = text_to(group(GATE, GROUP), "captured");
    later.local = "1102030405060708".to_owned();
    let task = store::prepare_outgoing(&file.db, &later, NOW, &[]).expect("prepare");
    let receivers = file.receivers(task);
    same_fates(
        &receivers,
        &[
            (GATE, ReceiverState::Pending),
            (ANN, ReceiverState::Pending),
        ],
        "the receivers' fates",
    );
    assert!(
        opened(&s, &receivers[0]).body == body::group_member_container(GATE, GROUP, b"captured"),
        "the body differs"
    );
    s.world.violations().assert_empty();
}

/// The nickname is in the metadata for everybody, and in the head's legacy field for a Gateway ID alone: what the peer's
/// rules hold byte by byte (`peer-legacy-nickname-*`).
#[test]
fn prepare_the_legacy_nickname_goes_to_a_gateway_id_alone() {
    let s = scene(7_115)
        .with_peer(ANN)
        .with_peer(GATE)
        .with_nickname("Kourier Test");
    let file = File::of(&s);
    let task = store::prepare_outgoing(&file.db, &text_to(contact(GATE), "to a gateway"), NOW, &[])
        .expect("prepare");
    let receivers = file.receivers(task);
    let gateway = opened(&s, &receivers[0]);
    assert_eq!(gateway.nickname.as_deref(), Some("Kourier Test"));
    let mut legacy = [0_u8; 32];
    legacy[..12].copy_from_slice(b"Kourier Test");
    assert!(
        gateway.bytes[32..64] == legacy,
        "the legacy nickname field differs"
    );

    let mut later = text_to(contact(ANN), "to a person");
    later.local = "1102030405060708".to_owned();
    let task = store::prepare_outgoing(&file.db, &later, NOW, &[]).expect("prepare");
    let person = opened(&s, &file.receivers(task)[0]);
    assert_eq!(person.nickname.as_deref(), Some("Kourier Test"));
    assert!(
        person.bytes[32..64] == [0_u8; 32],
        "the legacy nickname field differs"
    );
    s.world.violations().assert_empty();
}

/// What a text carries as its nickname, under a stored one.
fn nickname_sent(seed: u64, stored: Option<&str>) -> (Option<String>, Scene) {
    let mut s = scene(seed).with_peer(ANN);
    if let Some(nickname) = stored {
        s = s.with_nickname(nickname);
    }
    let file = File::of(&s);
    let task =
        store::prepare_outgoing(&file.db, &text_to(contact(ANN), "x"), NOW, &[]).expect("prepare");
    let ann = opened(&s, &file.receivers(task)[0]);
    (ann.nickname, s)
}

/// A profile with no nickname: an empty string in the metadata (the spec's "cleared"), never the user's ID.
#[test]
fn prepare_a_cleared_nickname_is_sent_empty_and_the_own_id_never() {
    let (none, s) = nickname_sent(7_116, None);
    assert_eq!(none.as_deref(), Some(""));
    s.world.violations().assert_empty();
    let (blank, s) = nickname_sent(7_117, Some("  "));
    assert_eq!(blank.as_deref(), Some(""));
    s.world.violations().assert_empty();
    let (own, s) = nickname_sent(7_118, Some(ME));
    assert_eq!(own.as_deref(), Some(""));
    s.world.violations().assert_empty();
}

// ---- a specific message ---------------------------------------------------------------------------------------------

/// The spec's _specific_ message: its receiver gets its type, body and flags, is judged for blocking by its type, and the
/// reflect holds the canonical one with every box's nonce.
#[test]
fn prepare_a_specific_message_goes_to_its_receiver_and_the_phone_gets_the_canonical_one() {
    let s = scene(7_120)
        .with_members(CRE, GROUP, &[ANN, BOB, DEE, EVE])
        .with_settings(block(&[DEE, EVE]))
        .with_nickname("Kourier Test");
    let file = File::of(&s);
    let canonical = body::group_member_container(CRE, GROUP, b"canonical reaction");
    let legacy = body::group_member_container(CRE, GROUP, b"legacy receipt");
    let control = body::group_creator_container(GROUP, b"a control");
    let mut message = text_to(group(CRE, GROUP), "unused");
    message.kind = "reaction";
    message.body = NewBody::Raw {
        ty: ty::GROUP_REACTION,
        body: canonical.clone(),
        distributes: false,
    };
    message.specific = vec![
        Specific {
            identity: BOB.to_owned(),
            ty: ty::GROUP_DELIVERY_RECEIPT,
            body: legacy.clone(),
        },
        // A blocked member the canonical type would leave out, under a type exempt from blocking.
        Specific {
            identity: DEE.to_owned(),
            ty: ty::GROUP_NAME,
            body: control.clone(),
        },
    ];
    let task = store::prepare_outgoing(&file.db, &message, NOW, &[]).expect("prepare");
    let receivers = file.receivers(task);
    same_fates(
        &receivers,
        &[
            (ANN, ReceiverState::Pending),
            (BOB, ReceiverState::Pending),
            (CRE, ReceiverState::Pending),
            (DEE, ReceiverState::Pending),
            (EVE, ReceiverState::Blocked),
        ],
        "the receivers' fates",
    );
    let of = |at: usize| opened(&s, &receivers[at]);
    let (ann, bob, cre, dee) = (of(0), of(1), of(2), of(3));
    assert_eq!(
        (ann.ty, ann.flags, ann.nickname.as_deref()),
        (0x83, PUSH, Some("Kourier Test")),
        "the canonical message: flags and profile of its type"
    );
    assert!(ann.body == canonical, "the body differs");
    assert_eq!(
        (bob.ty, bob.flags, bob.nickname.as_deref()),
        (0x81, 0x00, None),
        "a receipt: no push, and the caller said it does not distribute"
    );
    assert!(bob.body == legacy, "the body differs");
    assert!(cre.body == canonical, "the body differs");
    assert_eq!((dee.ty, dee.flags), (0x4b, 0x00));
    assert!(dee.body == control, "the body differs");
    assert_eq!(ann.message_id, bob.message_id, "one id for every receiver");

    let task = file.task();
    let message = outgoing(&s, &task);
    assert_eq!(message.r#type, 0x83);
    assert!(message.body == canonical, "the body differs");
    assert!(
        message.nonces
            == [
                ann.nonce.to_vec(),
                bob.nonce.to_vec(),
                cre.nonce.to_vec(),
                dee.nonce.to_vec(),
            ],
        "the nonces differ"
    );
    s.world.violations().assert_empty();
}

// ---- the jobs ------------------------------------------------------------------------------------------------------

/// A job: no `local`, a random id, the clock's time, its zone is the caller's. A receipt does not distribute by its type:
/// the job says (L8). It has no update.
#[test]
fn prepare_a_job_is_sealed_in_the_callers_zone_and_says_whether_it_distributes() {
    let s = scene(7_121).with_peer(ANN).with_nickname("Kourier Test");
    let file = File::of(&s);
    let receipt = body::delivery_receipt(1, &[[1, 2, 3, 4, 5, 6, 7, 8]]);
    let job = raw_job(contact(ANN), ty::DELIVERY_RECEIPT, receipt.clone(), false);
    let task_id = enqueue(&file, &job).expect("the job");
    let task = file.task();
    assert_eq!(task.id, task_id);
    assert_eq!(task.origin, SendOrigin::Job);
    assert_eq!(task.kind, "test-job");
    assert_eq!(task.local, None);
    assert_eq!(task.created_at, NOW);
    assert_eq!(task.queued_at, NOW);
    assert_eq!(task.state, SendState::Prepared);
    assert!(!task.has_update, "a receipt has no Sent update");

    let receivers = file.receivers(task.id);
    let ann = opened(&s, &receivers[0]);
    assert_eq!(ann.message_id, task.message_id);
    assert_eq!(ann.created_at_ms, 1_761_500_000_130);
    assert_eq!((ann.ty, ann.flags), (0x80, 0x00));
    assert!(ann.body == receipt, "the body differs");
    assert_eq!(ann.nickname, None, "a plain receipt does not distribute");
    let message = outgoing(&s, &task);
    assert_eq!(message.r#type, 0x80);
    assert_eq!(message.message_id, task.message_id);

    // A reaction's status: the job says it does; another id.
    let reaction = body::delivery_receipt(3, &[[1, 2, 3, 4, 5, 6, 7, 8]]);
    let job = raw_job(contact(ANN), ty::DELIVERY_RECEIPT, reaction, true);
    let second = enqueue(&file, &job).expect("the second job");
    assert_eq!(
        opened(&s, &file.receivers(second)[0]).nickname.as_deref(),
        Some("Kourier Test")
    );
    let ids = file.db.read(store::send_tasks).expect("the tasks");
    assert_ne!(ids[0].message_id, ids[1].message_id, "a random id per job");
    s.world.violations().assert_empty();
}

/// The caller's zone decides: rolled back, nothing of the job stays, its nonces neither.
#[test]
fn prepare_a_job_rolls_back_with_the_callers_zone() {
    let s = scene(7_122).with_peer(ANN);
    let file = File::of(&s);
    let job = raw_job(contact(ANN), ty::DELIVERY_RECEIPT, vec![1; 9], false);
    let out = file.db.zone(|tx| {
        store::enqueue_job(tx, &job, NOW)?;
        Err::<(), _>(StoreError::Failed("the caller's zone failed"))
    });
    assert_eq!(out, Err(StoreError::Failed("the caller's zone failed")));
    file.holds_nothing("a rolled back zone");
    s.world.violations().assert_empty();
}

/// A group control goes to a blocked member (exempt), a text does not; a group leave reaches a Gateway creator whatever
/// the name (`always`), and a job needs no membership: the user has left.
#[test]
fn prepare_a_job_of_a_group_control_goes_where_a_text_does_not() {
    use d2d_sync::group::UserState;
    let s = scene(7_123)
        .with_members(ME, GROUP, &[ANN])
        .with_members(GATE, GROUP + 1, &[BOB])
        .with_settings(block(&[ANN]));
    let file = File::of(&s);
    file.bend_group(GATE, GROUP + 1, |g| {
        g.user_state = Some(UserState::Left as i32);
    });

    let name = body::group_creator_container(GROUP, b"new name");
    let job = raw_job(group(ME, GROUP), ty::GROUP_NAME, name.clone(), false);
    let task = enqueue(&file, &job).expect("a control");
    let receivers = file.receivers(task);
    same_fates(
        &receivers,
        &[(ANN, ReceiverState::Pending)],
        "the receivers' fates",
    );
    let ann = opened(&s, &receivers[0]);
    assert_eq!((ann.ty, ann.flags, ann.nickname), (0x4b, 0x00, None));
    assert!(ann.body == name, "the body differs");

    let text = store::prepare_outgoing(&file.db, &text_to(group(ME, GROUP), "x"), NOW, &[])
        .expect("a text");
    same_fates(
        &file.receivers(text),
        &[(ANN, ReceiverState::Blocked)],
        "the receivers' fates",
    );

    let leave = body::group_member_container(GATE, GROUP + 1, &[]);
    let job = raw_job(group(GATE, GROUP + 1), ty::GROUP_LEAVE, leave, false);
    let task = enqueue(&file, &job).expect("a leave");
    same_fates(
        &file.receivers(task),
        &[
            (GATE, ReceiverState::Pending),
            (BOB, ReceiverState::Pending),
        ],
        "the receivers' fates",
    );
    s.world.violations().assert_empty();
}

/// `only` of a job replaces the list (a creator's answer to one member); the group need not be stored then, and without
/// `only` it must be.
#[test]
fn prepare_only_of_a_job_replaces_the_list_and_a_list_needs_the_group() {
    let s = scene(7_124).with_peer(ANN);
    let file = File::of(&s);
    let setup = body::group_creator_container(0x99, &[]);
    let mut job = raw_job(group(ME, 0x99), ty::GROUP_SETUP, setup, true);
    assert_eq!(
        enqueue(&file, &job),
        Err(StoreError::Refused(SendRefused::UnknownGroup))
    );
    file.holds_nothing("a job with no list");
    job.only = Some(vec![ANN.to_owned()]);
    let task = enqueue(&file, &job).expect("the answer");
    same_fates(
        &file.receivers(task),
        &[(ANN, ReceiverState::Pending)],
        "the receivers' fates",
    );
    assert!(
        outgoing(&s, &file.task()).conversation == Some(conversation_group(ME, 0x99)),
        "the conversation differs"
    );
    s.world.violations().assert_empty();
}

/// A type that does not reflect starts `reflected`: no sealed reflect, no D2D nonce, and the box is owed at once. A job's
/// row with nothing owed after its acks is gone.
#[test]
fn prepare_a_type_that_does_not_reflect_starts_reflected() {
    let s = scene(7_125).with_peer(ANN);
    let file = File::of(&s);
    let job = raw_job(
        contact(ANN),
        ty::CONTACT_REQUEST_PROFILE_PICTURE,
        Vec::new(),
        false,
    );
    let task_id = enqueue(&file, &job).expect("the job");
    let task = file.task();
    assert_eq!(task.state, SendState::Reflected);
    assert_eq!(task.reflect_env, None);
    assert!(!task.has_update);
    assert_eq!(file.count("d2d_nonce"), 0);
    assert_eq!(file.count("csp_nonce"), 1);
    let ann = opened(&s, &file.receivers(task_id)[0]);
    assert_eq!((ann.ty, ann.flags), (0x1a, 0x00));
    assert_eq!(
        store::mark_send_acks(&file.db, task_id, &[ANN], None, NOW),
        Ok(SendEnd::Deleted)
    );
    assert_eq!(file.count("send_task"), 0);
    s.world.violations().assert_empty();
}

// ---- the update -----------------------------------------------------------------------------------------------------

/// The `Sent` update of a row as the phone opens it: the conversation and the id alone; the last ACKS zone writes it with
/// its nonce, under a D2D nonce of its own.
#[test]
fn prepare_the_sent_update_opens_at_the_phone_and_its_nonce_is_durable_with_it() {
    use d2d::outgoing_message_update::{Sent as SentMark, Update, update};
    let s = scene(7_126).with_members(CRE, GROUP, &[ANN]);
    let file = File::of(&s);
    let task_id = store::prepare_outgoing(&file.db, &text_to(group(CRE, GROUP), "x"), NOW, &[])
        .expect("prepare");
    let task = file.task();
    let reflect_nonce: [u8; 24] = task.reflect_env.as_deref().expect("a reflect")[..24]
        .try_into()
        .expect("the nonce ahead");
    store::mark_send_reflected(&file.db, task_id).expect("reflected");

    let (envelope, nonce) = file
        .db
        .read(|c: &Connection| store::seal_sent_update(c, &task))
        .expect("the update");
    match reflected(&s, &envelope) {
        Content::OutgoingMessageUpdate(message) => assert!(
            message.updates
                == [Update {
                    conversation: Some(conversation_group(CRE, GROUP)),
                    message_id: ID,
                    update: Some(update::Update::Sent(SentMark {})),
                }],
            "the update differs"
        ),
        _ => panic!("the update is an OutgoingMessageUpdate"),
    }
    let update_nonce: [u8; 24] = envelope[..24].try_into().expect("the nonce ahead");
    assert!(
        nonce == stored_form(update_nonce),
        "the hash of its own nonce"
    );
    assert!(update_nonce != reflect_nonce, "a nonce of its own");
    assert!(!file.has_nonce(NonceScope::D2d, &nonce), "not durable yet");

    let update = SealedUpdate {
        envelope: &envelope,
        nonce,
    };
    assert_eq!(
        store::mark_send_acks(&file.db, task_id, &[ANN, CRE], Some(update), NOW),
        Ok(SendEnd::Acked)
    );
    assert!(
        file.has_nonce(NonceScope::D2d, &nonce),
        "durable with the bytes"
    );
    assert!(
        file.task().update_env.as_deref() == Some(envelope.as_slice()),
        "the stored update differs"
    );
    s.world.violations().assert_empty();
}

// ---- S5a: libthreema against the fake's parsers ------------------------------------------------------------------------

/// PREMISE 10, THE FIRST TEST: libthreema's `Reflect` encode and `ReflectAck` decode have never run in Kourier. The real
/// encoder's datagram, byte for byte, through the fake's own parser, and the fake's ack through the real decoder; then the
/// two together on the wire: the id and the time the fake stored it with come back. Literals on both sides.
#[test]
fn order_libthreemas_reflect_and_reflect_ack_agree_with_the_fakes_parsers() {
    let w = crate::support::mock::world(7_200);
    let g = mock_group(&w);
    let mut c = online_client(&w, &g, new_device(0xF000_0000_0000_0002));

    let d = c
        .proto
        .create_payload(OutgoingPayload::Reflect(Reflect {
            flags: ReflectFlags(0x0100),
            reflect_id: 0x0403_0201,
            envelope: vec![0xaa, 0xbb, 0xcc],
        }))
        .expect("the client encodes a Reflect")
        .outgoing_datagram
        .expect("a datagram");
    assert_eq!(
        d.0,
        [
            0x80, 0, 0, 0, // the container: the type and three reserved bytes
            8, 0, // header-length, reserved
            0x00, 0x01, // flags, little-endian
            0x01, 0x02, 0x03, 0x04, // reflect-id, little-endian
            0xaa, 0xbb, 0xcc, // the envelope
        ],
        "the bytes of a Reflect"
    );
    let (payload_type, payload) = split(&d.0).expect("a container");
    assert_eq!(payload_type, kind::REFLECT);
    assert_eq!(
        parse_reflect(payload),
        Some((0x0100, 0x0403_0201, &[0xaa, 0xbb, 0xcc][..])),
        "the fake's parser reads the real encoder"
    );

    let conn = c.conn();
    w.mediator().send_raw(
        conn,
        datagram(
            kind::REFLECT_ACK,
            &reflect_ack_payload(0x0807_0605, 0x1817_1615_1413_1211),
        ),
    );
    pump(w.mediator(), &mut c);
    w.clock().advance(Duration::from_millis(1234));
    w.mediator().receive(conn, d.0);
    pump(w.mediator(), &mut c);
    let acks: Vec<(u32, u64)> = c
        .payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::ReflectAck(ack) => Some((ack.reflect_id, ack.timestamp)),
            _ => None,
        })
        .collect();
    assert_eq!(
        acks,
        [
            (0x0807_0605, 0x1817_1615_1413_1211),
            (0x0403_0201, START_MS + 1234)
        ],
        "the real decoder reads the fake's ack, and the fake acks the real encoder's reflect"
    );
    w.violations().assert_empty();
}

// ---- S5a: the frames on a socket -----------------------------------------------------------------------------------------

/// The upper bound of a wait (never a measure).
const WALL: Duration = Duration::from_secs(10);
/// The whole of a 1:1 job with its update, as the fake's trace reads.
const ONE_TO_ONE: [&str; 6] = [
    "reflect",
    "reflect-ack",
    "csp-outgoing-message",
    "csp-outgoing-message-ack",
    "reflect",
    "reflect-ack",
];
const FULL: i32 = 13;
const CONSTRAINT_CHECK: i32 = 275;

/// A job of a text. A later step's jobs are receipts and answers; a text is what proves the most steps.
fn text_job(to: To, text: &str) -> NewJob {
    NewJob {
        kind: "test-job",
        to,
        body: NewBody::Text(text.to_owned()),
        only: None,
    }
}

/// What the client wrote and the servers answered on `conn`, by the names of the send path, in the fake's order.
fn order_on(h: &H, conn: ConnId) -> Vec<String> {
    h.s.world
        .trace()
        .lines()
        .iter()
        .filter_map(|line| {
            let mut parts = line.split(' ').skip(1);
            let (on, _, name) = (parts.next()?, parts.next()?, parts.next()?);
            (on == conn.0.to_string()
                && matches!(
                    name,
                    "reflect" | "reflect-ack" | "csp-outgoing-message" | "csp-outgoing-message-ack"
                ))
            .then(|| name.to_owned())
        })
        .collect()
}

fn tasks(h: &H) -> Vec<SendTask> {
    h.db().read(store::send_tasks).expect("the tasks")
}

fn receivers_of(h: &H, task: i64) -> Vec<SendReceiver> {
    h.db()
        .read(|c: &Connection| store::send_receivers(c, task))
        .expect("the receivers")
}

/// Waits (a wall BOUND, no timer of the test's) until `done` holds of the file.
async fn wait_file(h: &H, what: &str, done: impl Fn(&ZoneDb) -> bool) {
    let db = h.db();
    let reached = tokio::time::timeout(WALL, async {
        while !done(&db) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "the file never showed {what}; the fake's rules: {:?}",
        h.s.world.violations().rules()
    );
}

/// Waits until the queue holds `n` rows.
async fn wait_rows(h: &H, n: usize) {
    wait_file(h, &format!("{n} rows of the queue"), |db| {
        db.read(store::send_tasks).expect("the tasks").len() == n
    })
    .await;
}

/// Waits until the fake holds the ack of `receiver`'s box.
async fn wait_held(h: &H, receiver: &str) {
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.csp_held_acks(receiver) == 1, WALL).await,
        "the chat server never held the ack of a box"
    );
}

fn syncing(s: &ConnState) -> bool {
    matches!(s, ConnState::Syncing { .. })
}

fn online_as<'a>(detail: &'a str) -> impl Fn(&ConnState) -> bool + 'a {
    move |s| matches!(s, ConnState::Online { detail: Some(d) } if d == detail)
}

fn offline_with_a_retry(s: &ConnState) -> bool {
    matches!(
        s,
        ConnState::Offline {
            retry_at_ms: Some(_)
        }
    )
}

/// The fake's parsers read what the real actor wrote: a group's job goes reflect, its ack, every box, their acks, the
/// update, its ack, and the row is gone. Each frame as the row stored it; each peer opens one message, and what it opens is
/// what the fake's own builder writes (§2.9 check 5); the phone opens the `OutgoingMessage` that names the boxes' nonces and
/// then the `Sent` update.
#[test]
fn order_a_group_job_goes_reflect_ack_boxes_acks_update_ack_and_the_row_is_gone() {
    support::offline::run_child(
        module_path!(),
        "order_a_group_job_goes_reflect_ack_boxes_acks_update_ack_and_the_row_is_gone_child",
    );
}

#[tokio::test]
async fn order_a_group_job_goes_reflect_ack_boxes_acks_update_ack_and_the_row_is_gone_child() {
    if !support::offline::in_child() {
        return;
    }
    const TEXT: &str = "synthetic group text";
    let s = scene(7_201)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .job(text_job(group(CRE, GROUP), TEXT));
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    let task = tasks(&h).remove(0);
    let stored = receivers_of(&h, task.id);
    assert_eq!(stored.len(), 3, "two members and the creator");
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 0).await;

    assert_eq!(
        order_on(&h, conn),
        [
            "reflect",
            "reflect-ack",
            "csp-outgoing-message",
            "csp-outgoing-message-ack",
            "csp-outgoing-message",
            "csp-outgoing-message-ack",
            "csp-outgoing-message",
            "csp-outgoing-message-ack",
            "reflect",
            "reflect-ack",
        ],
        "the order on the wire"
    );
    let reflects = h.s.world.mediator().reflects();
    assert_eq!(reflects.len(), 2, "the message and its update");
    assert_eq!(
        reflects
            .iter()
            .map(|r| (r.reflect_id, r.flags))
            .collect::<Vec<_>>(),
        [(1, 0), (2, 0)],
        "a new id each, never ephemeral"
    );
    assert!(
        Some(&reflects[0].bytes) == task.reflect_env.as_ref(),
        "the reflect is not the row's bytes"
    );
    let sent = h.s.world.mediator().csp_sent();
    assert_eq!(sent.len(), 3, "one box each");
    for receiver in &stored {
        let on_the_wire = sent
            .iter()
            .filter(|one| one.receiver.as_slice() == receiver.identity.as_bytes())
            .map(|one| &one.bytes)
            .collect::<Vec<_>>();
        assert!(
            on_the_wire == [receiver.payload.as_ref().expect("a stored box")],
            "a box is not the row's bytes"
        );
    }

    let expected = body::group_member_container(CRE, GROUP, &body::text(TEXT));
    let mut nonces = Vec::new();
    for id in [ANN, BOB, CRE] {
        let mut opened = h.s.world.peer(id).drain();
        assert_eq!(opened.len(), 1, "one message at each peer");
        let one = opened.remove(0);
        assert_eq!((one.ty, one.discard), (ty::GROUP_TEXT, None));
        assert_eq!(one.message_id, task.message_id);
        assert!(one.body == expected, "the body is not the fake's builder's");
        nonces.push(one.nonce.to_vec());
    }
    let at_the_phone = phone.take_reflected();
    assert_eq!(at_the_phone.len(), 2, "the phone got both reflects");
    let Content::OutgoingMessage(message) = reflected(&h.s, &at_the_phone[0].1) else {
        panic!("the first reflect is no OutgoingMessage");
    };
    assert_eq!(message.message_id, task.message_id);
    assert_eq!(message.r#type, u32::from(ty::GROUP_TEXT) as i32);
    assert!(message.body == expected, "the phone's body differs");
    let mut listed = message.nonces.clone();
    listed.sort();
    nonces.sort();
    assert!(
        listed == nonces,
        "the reflect does not list the boxes' nonces"
    );
    let Content::OutgoingMessageUpdate(update) = reflected(&h.s, &at_the_phone[1].1) else {
        panic!("the second reflect is no OutgoingMessageUpdate");
    };
    assert_eq!(update.updates.len(), 1, "one message's update");

    assert_eq!(
        h.file_rows("send_receiver"),
        0,
        "the receivers went with it"
    );
    assert_eq!(h.file_rows("d2d_nonce"), 2, "both envelopes' nonces stay");
    assert_eq!(h.file_rows("csp_nonce"), 3, "every box's nonce stays");
    h.finish().await;
}

/// A 1:1 text: the peer opens what the fake's builder writes, a plain TEXT; the order is the short one.
#[test]
fn order_a_text_job_opens_at_its_peer_as_the_fakes_builder_writes_it() {
    support::offline::run_child(
        module_path!(),
        "order_a_text_job_opens_at_its_peer_as_the_fakes_builder_writes_it_child",
    );
}

#[tokio::test]
async fn order_a_text_job_opens_at_its_peer_as_the_fakes_builder_writes_it_child() {
    if !support::offline::in_child() {
        return;
    }
    const TEXT: &str = "synthetic text";
    let s = scene(7_202)
        .with_peer(ANN)
        .job(text_job(contact(ANN), TEXT));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 0).await;
    assert_eq!(order_on(&h, conn), ONE_TO_ONE);
    let mut opened = h.s.world.peer(ANN).drain();
    assert_eq!(opened.len(), 1);
    let one = opened.remove(0);
    assert_eq!((one.ty, one.discard), (ty::TEXT, None));
    assert!(
        one.body == body::text(TEXT),
        "the body is not the fake's builder's"
    );
    h.finish().await;
}

/// The mutant "boxes before the reflect-ack" dies here. With the reflect-ack withheld the reflect goes and NOT ONE box; the
/// row stays `prepared`. The ack released: the boxes go, and the row walks on.
#[test]
fn order_a_withheld_reflect_ack_holds_every_box() {
    support::offline::run_child(
        module_path!(),
        "order_a_withheld_reflect_ack_holds_every_box_child",
    );
}

#[tokio::test]
async fn order_a_withheld_reflect_ack_holds_every_box_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_203)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .job(text_job(group(CRE, GROUP), "synthetic"));
    s.world.mediator().knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.reflect_acks_held(conn) == 1, WALL).await,
        "the reflect never came"
    );
    h.sync().await;
    assert_eq!(
        m.csp_sent().len(),
        0,
        "a box went while the reflect-ack is withheld"
    );
    assert_eq!(tasks(&h)[0].state, SendState::Prepared);
    assert!(
        tasks(&h)[0].reflect_env.is_some(),
        "the sealed reflect went before its ack"
    );

    m.knobs(|k| k.withhold_reflect_acks = false);
    h.net().act(|m| m.release_reflect_acks(conn));
    wait_rows(&h, 0).await;
    let order = order_on(&h, conn);
    assert_eq!(
        order[..2],
        ["reflect", "reflect-ack"],
        "the order on the wire"
    );
    assert_eq!(m.csp_sent().len(), 3, "one box each after the ack");
    h.finish().await;
}

/// The mutant "update before the last ack" dies here. The ack of ONE receiver is held: the others' are durable (their rows
/// `acked`, their boxes gone), the row stays `reflected`, no update is sealed and none goes. The last ack: the update, its
/// ack, the row gone.
#[test]
fn order_a_withheld_message_ack_of_one_receiver_holds_the_update() {
    support::offline::run_child(
        module_path!(),
        "order_a_withheld_message_ack_of_one_receiver_holds_the_update_child",
    );
}

#[tokio::test]
async fn order_a_withheld_message_ack_of_one_receiver_holds_the_update_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_204)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .job(text_job(group(CRE, GROUP), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(BOB));
    let mut h = harness::start(s);
    let task = tasks(&h)[0].id;
    connect(&mut h).await;
    wait_held(&h, BOB).await;
    wait_file(&h, "the two other receivers acked", |db| {
        let rows = db
            .read(|c: &Connection| store::send_receivers(c, task))
            .expect("the receivers");
        rows.iter()
            .filter(|r| r.state == ReceiverState::Acked)
            .count()
            == 2
    })
    .await;
    h.sync().await;
    assert_eq!(m.reflects().len(), 1, "an update went before the last ack");
    let row = tasks(&h).remove(0);
    assert_eq!(row.state, SendState::Reflected);
    assert_eq!(
        row.update_env, None,
        "an update was sealed before the last ack"
    );
    assert_eq!(h.file_rows("d2d_nonce"), 1, "the update's nonce came early");
    same_fates(
        &receivers_of(&h, task),
        &[
            (ANN, ReceiverState::Acked),
            (BOB, ReceiverState::Pending),
            (CRE, ReceiverState::Acked),
        ],
        "after the two acks",
    );
    assert!(
        receivers_of(&h, task)[0].payload.is_none(),
        "an acked receiver keeps its box"
    );

    assert_eq!(h.net().act(|m| m.release_message_acks(BOB)), 1);
    wait_rows(&h, 0).await;
    assert_eq!(m.reflects().len(), 2, "the update after the last ack");
    h.finish().await;
}

/// A job runs from `Online` and not before; the catch-up holds it. A follower: no promotion, no unblock.
#[test]
fn order_a_job_waits_for_online_on_a_follower() {
    support::offline::run_child(
        module_path!(),
        "order_a_job_waits_for_online_on_a_follower_child",
    );
}

#[tokio::test]
async fn order_a_job_waits_for_online_on_a_follower_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_205)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    // The phone came first and leads.
    let _phone = phone_of(&s);
    let h = waited_for_online(s, "follower").await;
    h.finish().await;
}

/// The same on a device the mediator promotes with Dry: the send path does not care, and nothing asks the chat server for
/// messages (the fake's `csp-unblock-*` rules stay armed for the last check).
#[test]
fn order_a_job_waits_for_online_on_a_promoted_device() {
    support::offline::run_child(
        module_path!(),
        "order_a_job_waits_for_online_on_a_promoted_device_child",
    );
}

#[tokio::test]
async fn order_a_job_waits_for_online_on_a_promoted_device_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_206)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let h = waited_for_online(s, "leader").await;
    h.finish().await;
}

/// With Dry held the account is `Syncing`: the job stays, whatever the role will be. Dry: `Online { detail }`, and the job
/// runs by itself with nobody asking.
async fn waited_for_online(s: Scene, detail: &str) -> H {
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    h.sync().await;
    let m = h.s.world.mediator();
    assert_eq!(m.reflects().len(), 0, "a job went before Online");
    assert_eq!(tasks(&h)[0].state, SendState::Prepared);
    let conn = h.conn();
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state(detail, online_as(detail)).await;
    wait_rows(&h, 0).await;
    assert_eq!(order_on(&h, conn), ONE_TO_ONE);
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
        0,
        "the send path asked for messages"
    );
    h
}

/// A wait for acks holds nothing else: with a box's ack withheld the keep-alive's echo goes out and is answered, and a
/// reflected text goes through its zone and is acked; the row stands, and the ack lets it finish.
#[test]
fn order_an_echo_and_a_reflection_still_flow_while_acks_wait() {
    support::offline::run_child(
        module_path!(),
        "order_an_echo_and_a_reflection_still_flow_while_acks_wait_child",
    );
}

#[tokio::test]
async fn order_an_echo_and_a_reflection_still_flow_while_acks_wait_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_207)
        .with_peer(ANN)
        .with_contacts(&[PEER])
        .job(text_job(contact(ANN), "synthetic"));
    let mut phone = phone_of(&s);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_held(&h, ANN).await;

    let before = m.csp_echo_requests();
    h.advance_to(START_MS + 10_000).await;
    assert!(
        h.net()
            .wait(|| m.csp_echo_requests() == before + 1, WALL)
            .await,
        "no echo while an ack is awaited"
    );
    // The next one comes only if that one was answered: else this deadline ends the socket.
    h.advance_to(START_MS + 20_000).await;
    assert!(
        h.net()
            .wait(|| m.csp_echo_requests() == before + 2, WALL)
            .await,
        "the first echo stayed unanswered while an ack is awaited"
    );
    assert_eq!(h.net().opened().len(), 1, "the socket went");

    let text = Msg::text([1; 8], START_MS + 1, "synthetic backlog").nonce([1; 24]);
    h.net()
        .act(|_| phone.reflect_envelope(&Author::new(PHONE).incoming(PEER, &text)));
    let got = h.messages(1).await;
    assert!(!got[0].catch_up, "a live text");
    wait_acks(&h, 1).await;

    assert_eq!(tasks(&h)[0].state, SendState::Reflected, "the row stands");
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_rows(&h, 0).await;
    h.finish().await;
}

/// `Command::Connect` on an open socket kicks the queue: a job that appeared behind the socket's back waits for a look, and
/// `Connect` is one.
#[test]
fn order_a_connect_kicks_the_queue() {
    support::offline::run_child(module_path!(), "order_a_connect_kicks_the_queue_child");
}

#[tokio::test]
async fn order_a_connect_kicks_the_queue_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_208).with_peer(ANN);
    // The phone leads, so no promotion comes after `Online` to wake the socket.
    let _phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.sync().await;
    let conn = h.conn();
    h.job(&text_job(contact(ANN), "synthetic"));
    h.sync().await;
    let m = h.s.world.mediator();
    assert_eq!(
        m.reflects().len(),
        0,
        "the socket looked with nothing to wake it"
    );
    h.send(Command::Connect).await;
    wait_rows(&h, 0).await;
    assert_eq!(order_on(&h, conn), ONE_TO_ONE);
    assert_eq!(
        h.net().opened().len(),
        1,
        "a Connect on an open socket opens no other"
    );
    h.finish().await;
}

/// L4. The last message-ack and the server's close reach the client in ONE drain (the ack hook of a reflection holds the
/// client's thread until the test has queued them): the pass's ZONE ACKS commits, what the server said is durable with the
/// sealed update, and NO frame goes after the server's close. The next socket writes the stored update by itself, under an id
/// of ITS connection (an `acked` row needs nobody, R1), and the row is gone.
#[test]
fn order_a_close_found_with_the_ack_commits_the_zone_and_writes_no_frame() {
    support::offline::run_child(
        module_path!(),
        "order_a_close_found_with_the_ack_commits_the_zone_and_writes_no_frame_child",
    );
}

#[tokio::test]
async fn order_a_close_found_with_the_ack_commits_the_zone_and_writes_no_frame_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_209)
        .with_peer(ANN)
        .with_contacts(&[PEER])
        .job(text_job(contact(ANN), "synthetic"));
    let mut phone = phone_of(&s);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_held(&h, ANN).await;
    let conn = h.conn();

    in_one_drain(&h, &mut phone, |m| {
        assert_eq!(m.release_message_acks(ANN), 1, "the held ack");
        m.close(conn, 1000);
    })
    .await;
    h.wait_state("Offline", offline_with_a_retry).await;

    let row = tasks(&h).remove(0);
    assert_eq!(
        row.state,
        SendState::Acked,
        "the pass's zone did not commit"
    );
    assert_eq!(row.attempts, 0, "nothing stood unanswered: no failed run");
    let update = row.update_env.expect("the zone holds the sealed update");
    // The phone's reflection is the fake's list too.
    let of_this_device = || -> Vec<ReflectSeen> {
        let phone = phone.conn();
        m.reflects()
            .into_iter()
            .filter(|r| Some(r.conn) != phone)
            .collect()
    };
    assert_eq!(
        of_this_device().len(),
        1,
        "the update went with the server's close"
    );
    assert_eq!(
        h.net().late_sends(conn),
        0,
        "a frame after the server's close"
    );

    h.send(Command::Connect).await;
    wait_rows(&h, 0).await;
    let reflects = of_this_device();
    assert_eq!(reflects.len(), 2, "the next socket writes the update");
    assert!(reflects[1].bytes == update, "not the stored update");
    assert!(
        reflects[1].conn != conn,
        "the update came on another socket"
    );
    assert_eq!(reflects[1].reflect_id, 1, "an id of its own connection");
    h.finish().await;
}

/// What `queue` queues reaches the client in ONE drain: the ack hook of a reflection holds the client's thread, inside its send of
/// the ack, until the test has queued it. The drain is what the client does next.
async fn in_one_drain(h: &H, phone: &mut Phone, queue: impl FnOnce(&Mediator)) {
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (reached_tx, go_rx) = (
        std::sync::Mutex::new(reached_tx),
        std::sync::Mutex::new(go_rx),
    );
    h.s.world.mediator().on_reflected_ack(move |_| {
        let _ = reached_tx.lock().expect("the hook's channel").send(());
        let _ = go_rx.lock().expect("the hook's channel").recv_timeout(WALL);
    });
    let text = Msg::text([1; 8], START_MS + 1, "synthetic").nonce([1; 24]);
    h.net()
        .act(|_| phone.reflect_envelope(&Author::new(PHONE).incoming(PEER, &text)));
    reached_rx
        .recv_timeout(WALL)
        .expect("the reflected ack never reached the mediator");
    h.net().act(queue);
    go_tx.send(()).expect("the hook is waiting");
}

/// A zone the STORE fails ends the socket (close 1000) and what it would allow does not go; the row stands, and the next
/// socket writes THE SAME reflect under a new id.
#[test]
fn order_a_zone_the_store_fails_closes_1000_and_writes_no_box() {
    support::offline::run_child(
        module_path!(),
        "order_a_zone_the_store_fails_closes_1000_and_writes_no_box_child",
    );
}

#[tokio::test]
async fn order_a_zone_the_store_fails_closes_1000_and_writes_no_box_child() {
    if !support::offline::in_child() {
        return;
    }
    store::fail_send_zone(fp::INSIDE_SEND_REFLECTED, FULL);
    let s = scene(7_210)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    let h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    assert!(
        h.net().wait(|| m.closed(first) == Some(1000), WALL).await,
        "the socket was not closed 1000"
    );
    assert_eq!(m.reflects().len(), 1, "the reflect went");
    assert_eq!(m.csp_sent().len(), 0, "a box went after the failed zone");
    assert_eq!(tasks(&h)[0].state, SendState::Prepared, "the row stands");

    store::fail_send_zone("", 0);
    h.send(Command::Connect).await;
    wait_rows(&h, 0).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 3, "the reflect again, then the update");
    assert!(reflects[1].bytes == reflects[0].bytes, "another reflect");
    assert_eq!(reflects[1].reflect_id, 1, "an id of its own connection");
    h.finish().await;
}

/// A zone that fails by its CONTENT does not close (decision R2): one WARN by its rule, the row cools at once, even at its
/// first failed run (it would run again in the same pass), a later row goes, and at its time the row runs again on the same
/// socket and finishes: THE SAME reflect under a new id.
#[test]
fn order_a_zone_that_fails_by_content_leaves_the_row_and_a_later_row_goes() {
    support::offline::run_child(
        module_path!(),
        "order_a_zone_that_fails_by_content_leaves_the_row_and_a_later_row_goes_child",
    );
}

#[tokio::test]
async fn order_a_zone_that_fails_by_content_leaves_the_row_and_a_later_row_goes_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    store::fail_send_zone(fp::INSIDE_SEND_REFLECTED, CONSTRAINT_CHECK);
    let s = scene(7_211)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"))
        // No reflect, so no zone REFLECTED: it can pass.
        .job(raw_job(
            contact(ANN),
            ty::CONTACT_REQUEST_PROFILE_PICTURE,
            Vec::new(),
            false,
        ));
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 1).await;
    assert_eq!(
        tasks(&h)[0].state,
        SendState::Prepared,
        "the first row stands"
    );
    assert_eq!(m.reflects().len(), 1, "the first row's reflect alone");
    let mut opened = h.s.world.peer(ANN).drain();
    assert_eq!(opened.len(), 1, "the later row's box alone");
    assert_eq!(opened.remove(0).ty, ty::CONTACT_REQUEST_PROFILE_PICTURE);
    assert_eq!(
        m.closed(conn),
        None,
        "a failure by content closed the socket"
    );
    assert_eq!(
        warns
            .at(tracing::Level::WARN)
            .iter()
            .filter(|w| *w == "send rows cooling: 1 by send-zone-failed")
            .count(),
        1,
        "not one WARN send-zone-failed"
    );
    assert_eq!(tasks(&h)[0].attempts, 1, "a failed run of the row");

    store::fail_send_zone("", 0);
    through_echoes(&h, at(0), at(110)).await;
    assert_eq!(m.reflects().len(), 1, "the row ran before its time");
    h.advance_to(at(120)).await;
    wait_rows(&h, 0).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 3, "the reflect again, then the update");
    assert!(reflects[1].bytes == reflects[0].bytes, "another reflect");
    assert_eq!(h.net().opened().len(), 1, "the socket stayed");
    h.finish().await;
}

/// The mutant "a fixed reflect-id" dies here (the fake's `reflect-id-repeated`). ONE TASK AT A TIME: two rows on one socket go
/// one after the other in queue order, the second only after the first is gone, and every reflect has an id of its own.
#[test]
fn order_two_rows_go_one_after_the_other_under_new_reflect_ids() {
    support::offline::run_child(
        module_path!(),
        "order_two_rows_go_one_after_the_other_under_new_reflect_ids_child",
    );
}

#[tokio::test]
async fn order_two_rows_go_one_after_the_other_under_new_reflect_ids_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_212)
        .with_peer(ANN)
        .with_peer(BOB)
        .job(text_job(contact(ANN), "synthetic one"))
        .job(text_job(contact(BOB), "synthetic two"));
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 0).await;
    assert_eq!(
        m.reflects()
            .iter()
            .map(|r| r.reflect_id)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4],
        "a new id per frame, unique on the connection"
    );
    let twice: Vec<&str> = ONE_TO_ONE.iter().chain(&ONE_TO_ONE).copied().collect();
    assert_eq!(order_on(&h, conn), twice, "the order on the wire");
    let sent = m.csp_sent();
    assert!(
        sent[0].receiver.as_slice() == ANN.as_bytes()
            && sent[1].receiver.as_slice() == BOB.as_bytes(),
        "not in queue order"
    );
    h.finish().await;
}

/// A type that does not reflect starts at its box; with no update it ends with the last ack.
#[test]
fn order_a_type_that_does_not_reflect_goes_straight_to_its_box() {
    support::offline::run_child(
        module_path!(),
        "order_a_type_that_does_not_reflect_goes_straight_to_its_box_child",
    );
}

#[tokio::test]
async fn order_a_type_that_does_not_reflect_goes_straight_to_its_box_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_213).with_peer(ANN).job(raw_job(
        contact(ANN),
        ty::CONTACT_REQUEST_PROFILE_PICTURE,
        Vec::new(),
        false,
    ));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 0).await;
    assert_eq!(
        order_on(&h, conn),
        ["csp-outgoing-message", "csp-outgoing-message-ack"]
    );
    h.finish().await;
}

/// A notes group has nobody to write to: reflect, its ack, the zone of no acks, the update, its ack, no box.
#[test]
fn order_a_notes_group_reflects_and_updates_with_no_box() {
    support::offline::run_child(
        module_path!(),
        "order_a_notes_group_reflects_and_updates_with_no_box_child",
    );
}

#[tokio::test]
async fn order_a_notes_group_reflects_and_updates_with_no_box_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_214)
        .with_members(ME, GROUP, &[])
        .job(text_job(group(ME, GROUP), "synthetic note"));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_rows(&h, 0).await;
    assert_eq!(
        order_on(&h, conn),
        ["reflect", "reflect-ack", "reflect", "reflect-ack"]
    );
    h.finish().await;
}

/// R1, and who may run. From the login-ack, with Dry held (so `Syncing`, and no job would go): an `acked` row writes its stored
/// update by itself and is `done` at the time of the real ack, as the real decoder read it; an open `transmit` row with no
/// `Transmit` attached writes nothing: the core decides.
#[test]
fn order_an_acked_row_goes_by_itself_at_the_login_ack_and_an_open_transmit_row_does_not() {
    support::offline::run_child(
        module_path!(),
        "order_an_acked_row_goes_by_itself_at_the_login_ack_and_an_open_transmit_row_does_not_child",
    );
}

#[tokio::test]
async fn order_an_acked_row_goes_by_itself_at_the_login_ack_and_an_open_transmit_row_does_not_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_215).with_peer(ANN);
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    let (waiting, walked, update) = {
        let db = h.db();
        let waiting = store::prepare_outgoing(&db, &text_to(contact(ANN), "synthetic"), NOW, &[])
            .expect("the open row");
        let mut second = text_to(contact(ANN), "synthetic too");
        second.local = "1112131415161718".to_owned();
        let walked = store::prepare_outgoing(&db, &second, NOW, &[]).expect("the row to walk");
        store::mark_send_reflected(&db, walked).expect("its reflect's zone");
        let row = db
            .read(|c: &Connection| store::send_task(c, walked))
            .expect("the row")
            .expect("the row");
        let (envelope, nonce) = db
            .read(|c: &Connection| store::seal_sent_update(c, &row))
            .expect("the update");
        let end = store::mark_send_acks(
            &db,
            walked,
            &[ANN],
            Some(SealedUpdate {
                envelope: &envelope,
                nonce,
            }),
            NOW,
        );
        assert_eq!(end, Ok(SendEnd::Acked));
        (waiting, walked, envelope)
    };
    // Both clocks on, so the time of the ack is a literal no other clock says.
    h.advance_idle(1_234).await;
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    wait_file(&h, "the row done", |db| {
        db.read(|c: &Connection| store::send_task(c, walked))
            .expect("the row")
            .expect("the row")
            .state
            == SendState::Done
    })
    .await;
    h.sync().await;
    let rows = tasks(&h);
    let done = rows.iter().find(|t| t.id == walked).expect("the row");
    assert_eq!(
        done.sent_at,
        Some(i64::try_from(START_MS + 1_234).expect("ms"))
    );
    assert_eq!(done.update_env, None, "a done row keeps no sealed bytes");
    let open = rows.iter().find(|t| t.id == waiting).expect("the row");
    assert_eq!(open.state, SendState::Prepared, "an unattached row moved");
    let m = h.s.world.mediator();
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 1, "the update alone");
    assert!(reflects[0].bytes == update, "not the stored update");
    assert_eq!(m.csp_sent().len(), 0, "no box");
    assert!(syncing(&h.state()), "the catch-up is not over");
    h.finish().await;
}

/// The file of a piped world with a contact and a job to it, for a child to send.
fn piped_with_a_job(seed: u64) -> piped::Parent {
    let p = piped::parent(seed);
    let peer = p.world.add_identity(ANN);
    let db = p.open();
    let sync = ContactDelta::create(ANN, peer.ck.public, START_MS).build();
    db.zone(|tx| {
        tx.execute(
            "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
            params![ANN, sync.encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("seed the contact");
    let now = i64::try_from(START_MS).expect("ms fit i64");
    db.zone(|tx| store::enqueue_job(tx, &text_job(contact(ANN), "synthetic"), now))
        .expect("seed the job");
    drop(db);
    p
}

/// The child that sends the job of [`piped_with_a_job`] until its failpoint ends it.
#[tokio::test]
async fn order_a_frame_point_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.online().await;
    c.finish_killed().await;
}

/// A child killed at `point`, on a fresh world: it aborted there, and the parent reads what the fake got and the file says.
fn killed_at(seed: u64, point: &str) -> Option<(piped::Parent, SendTask)> {
    let p = piped_with_a_job(seed);
    let ended = p
        .spawn(
            module_path!(),
            "order_a_frame_point_child",
            Mode::Resume,
            Some(point),
        )?
        .wait();
    assert!(
        ended.aborted_at_failpoint(),
        "the child did not die at its point"
    );
    let mut rows = p.open().read(store::send_tasks).expect("the tasks");
    assert_eq!(rows.len(), 1, "the row stands");
    let row = rows.remove(0);
    Some((p, row))
}

/// `AFTER_SEND_REFLECT` fires with the reflect's frame on the wire, before any zone: the row is `prepared`.
#[test]
fn order_the_point_after_the_reflect_is_hit_with_its_frame_written() {
    let Some((p, row)) = killed_at(7_216, fp::AFTER_SEND_REFLECT) else {
        return;
    };
    let reflects = p.world.mediator().reflects();
    assert_eq!(reflects.len(), 1);
    assert!(
        Some(&reflects[0].bytes) == row.reflect_env.as_ref(),
        "not the row's reflect"
    );
    assert_eq!(p.world.mediator().csp_sent().len(), 0);
    assert_eq!(row.state, SendState::Prepared);
    p.world.violations().assert_empty();
}

/// `AFTER_SEND_BOX` fires with the box's frame on the wire, after ZONE REFLECTED: the row is `reflected`, the reflect gone.
#[test]
fn order_the_point_after_a_box_is_hit_with_its_frame_written_after_the_reflects_zone() {
    let armed = format!("{}:1", fp::AFTER_SEND_BOX);
    let Some((p, row)) = killed_at(7_217, &armed) else {
        return;
    };
    assert_eq!(p.world.mediator().reflects().len(), 1);
    assert_eq!(p.world.mediator().csp_sent().len(), 1);
    assert_eq!(row.state, SendState::Reflected);
    assert_eq!(
        row.reflect_env, None,
        "the reflect's zone came after the box"
    );
    p.world.violations().assert_empty();
}

/// `AFTER_SEND_UPDATE` fires with the update's frame on the wire, after the last ZONE ACKS: the row is `acked`, and the frame
/// is the bytes that zone stored.
#[test]
fn order_the_point_after_the_update_is_hit_with_its_frame_written_after_the_acks_zone() {
    let Some((p, row)) = killed_at(7_218, fp::AFTER_SEND_UPDATE) else {
        return;
    };
    let reflects = p.world.mediator().reflects();
    assert_eq!(reflects.len(), 2);
    assert!(
        Some(&reflects[1].bytes) == row.update_env.as_ref(),
        "not the row's update"
    );
    assert_eq!(p.world.mediator().csp_sent().len(), 1);
    assert_eq!(row.state, SendState::Acked);
    p.world.violations().assert_empty();
}

// ---- S5b: the deadline ---------------------------------------------------------------------------------------------------

/// A close code of the mediator's class that reconnects.
const REFUSED: u16 = 4000;

/// `s` seconds after both clocks' start.
fn at(s: u64) -> u64 {
    START_MS + s * 1_000
}

fn retry_at(when: u64) -> impl Fn(&ConnState) -> bool {
    move |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(when),
        }
    }
}

fn held(s: &ConnState) -> bool {
    matches!(s, ConnState::NeedsUserAction(_))
}

/// Moves both clocks to `target`, a time the actor waits for, stopping at each echo of the socket logged in at `login` on the
/// way (every 10 s, each answered). The actor waits for one thing at a time, so each stop is what it says.
async fn through_echoes(h: &H, login: u64, target: u64) {
    while h.now() < target {
        let echo = login + (h.now() - login) / 10_000 * 10_000 + 10_000;
        h.advance_to(echo.min(target)).await;
    }
}

/// Waits until the fake kept `n` boxes in all, and not more.
async fn wait_boxes(h: &H, n: usize) {
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.csp_sent().len() >= n, WALL).await,
        "the chat server never kept {n} boxes (it kept {})",
        m.csp_sent().len()
    );
    assert_eq!(m.csp_sent().len(), n, "more boxes than expected");
}

/// Waits until the client closed `conn` with 1000.
async fn wait_closed_1000(h: &H, conn: ConnId, what: &str) {
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.closed(conn) == Some(1000), WALL).await,
        "{what}: the socket was not closed 1000"
    );
}

/// Waits for the retry at `when` the backoff announced, moves both clocks there and waits for `Online`.
async fn reconnect_at(h: &mut H, when: u64) {
    h.wait_state("Offline with the retry", retry_at(when)).await;
    h.advance_to(when).await;
    h.wait_state("Online again", online).await;
}

/// The reflect-acks the mediator holds for `conn` reach `n`.
async fn wait_reflect_held(h: &H, conn: ConnId, n: usize) {
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.reflect_acks_held(conn) == n, WALL).await,
        "the mediator never held {n} reflect-acks"
    );
}

/// The mutant "`ACK_WITHIN` arm removed" dies here. Echoes are answered, the box's ack stays away: exactly 60 s after the box
/// the client closes 1000, the row stands with one failed run, the backoff's first step (the socket was up a minute) and the
/// next socket write THE SAME BOX: the first failed run is free. The peer sees one message and a nonce's discard. An ack lost
/// with its socket is lost; the next one finishes the row.
#[test]
fn deadline_no_ack_within_60_s_closes_1000_and_the_next_socket_writes_the_same_box() {
    support::offline::run_child(
        module_path!(),
        "deadline_no_ack_within_60_s_closes_1000_and_the_next_socket_writes_the_same_box_child",
    );
}

#[tokio::test]
async fn deadline_no_ack_within_60_s_closes_1000_and_the_next_socket_writes_the_same_box_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_230)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_held(&h, ANN).await;
    let first = h.conn();

    through_echoes(&h, at(0), at(50)).await;
    assert_eq!(m.closed(first), None, "closed before ACK_WITHIN");
    h.advance_to(at(60)).await;
    wait_closed_1000(&h, first, "no ack within 60 s").await;
    assert_eq!(tasks(&h)[0].attempts, 1, "one failed run");

    reconnect_at(&mut h, at(65)).await;
    wait_boxes(&h, 2).await;
    let sent = m.csp_sent();
    assert!(sent[1].bytes == sent[0].bytes, "not the same box");
    assert!(sent[1].conn != first.0, "the box came on the old socket");
    assert_eq!(m.reflects().len(), 1, "the reflect's ack is durable");
    let opened = h.s.world.peer(ANN).drain();
    assert_eq!(
        opened.iter().map(|o| o.discard).collect::<Vec<_>>(),
        [None, Some(Discard::Nonce)],
        "one message at the peer"
    );

    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_rows(&h, 0).await;
    assert_eq!(m.reflects().len(), 2, "the update after the last ack");
    h.finish().await;
}

/// The reflect's ack stays away: the same, at the first frame of the row. The next socket writes THE SAME REFLECT under an id of
/// its own connection, and no box went before its ack.
#[test]
fn deadline_a_reflect_ack_that_stays_away_ends_the_socket_and_the_next_writes_the_same_reflect() {
    support::offline::run_child(
        module_path!(),
        "deadline_a_reflect_ack_that_stays_away_ends_the_socket_and_the_next_writes_the_same_reflect_child",
    );
}

#[tokio::test]
async fn deadline_a_reflect_ack_that_stays_away_ends_the_socket_and_the_next_writes_the_same_reflect_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_231)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let first = h.conn();
    wait_reflect_held(&h, first, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    wait_closed_1000(&h, first, "no reflect-ack within 60 s").await;
    assert_eq!(
        m.csp_sent().len(),
        0,
        "a box went without its reflect's ack"
    );
    assert_eq!(tasks(&h)[0].state, SendState::Prepared);

    m.knobs(|k| k.withhold_reflect_acks = false);
    reconnect_at(&mut h, at(65)).await;
    wait_rows(&h, 0).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 3, "the reflect again, then the update");
    assert!(
        reflects[1].bytes == reflects[0].bytes,
        "not the same reflect"
    );
    assert_eq!(reflects[1].reflect_id, 1, "an id of its own connection");
    h.finish().await;
}

/// The deadline runs from the LAST frame written or ack heard. A group with two acks held: the first at +30 s pushes it to +90 s;
/// the last at +80 s writes the update, and with its reflect-ack held the deadline is +140 s. There the row is `acked`, with
/// everything durable but the update, and the next socket writes the stored update by itself.
#[test]
fn deadline_every_ack_heard_and_every_frame_written_starts_it_again() {
    support::offline::run_child(
        module_path!(),
        "deadline_every_ack_heard_and_every_frame_written_starts_it_again_child",
    );
}

#[tokio::test]
async fn deadline_every_ack_heard_and_every_frame_written_starts_it_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_232)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .job(text_job(group(CRE, GROUP), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        k.csp.withhold_message_ack_of(ANN);
        k.csp.withhold_message_ack_of(BOB);
    });
    let mut h = harness::start(s);
    let task = tasks(&h)[0].id;
    connect(&mut h).await;
    wait_held(&h, BOB).await;
    let first = h.conn();
    let acked = |n: usize| {
        move |db: &ZoneDb| {
            db.read(|c: &Connection| store::send_receivers(c, task))
                .expect("the receivers")
                .iter()
                .filter(|r| r.state == ReceiverState::Acked)
                .count()
                == n
        }
    };
    wait_file(&h, "the creator's ack", acked(1)).await;

    through_echoes(&h, at(0), at(30)).await;
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_file(&h, "the first held ack", acked(2)).await;
    through_echoes(&h, at(0), at(80)).await;
    assert_eq!(
        m.closed(first),
        None,
        "the ack heard at +30 s pushed nothing"
    );

    m.knobs(|k| k.withhold_reflect_acks = true);
    assert_eq!(h.net().act(|m| m.release_message_acks(BOB)), 1);
    wait_reflect_held(&h, first, 1).await;
    through_echoes(&h, at(0), at(130)).await;
    assert_eq!(m.closed(first), None, "the update's frame pushed nothing");
    h.advance_to(at(140)).await;
    wait_closed_1000(&h, first, "no reflect-ack of the update").await;
    let row = tasks(&h).remove(0);
    assert_eq!(row.state, SendState::Acked);
    assert_eq!(row.attempts, 1, "one failed run");

    m.knobs(|k| k.withhold_reflect_acks = false);
    reconnect_at(&mut h, at(145)).await;
    wait_rows(&h, 0).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 3, "the message and the update twice");
    assert!(
        reflects[2].bytes == reflects[1].bytes,
        "not the stored update"
    );
    h.finish().await;
}

/// A row in state `acked` with the update's frame out. Written through the doors, as the socket's zones would; its task's id.
fn acked_row(h: &H) -> i64 {
    let db = h.db();
    let walked = store::prepare_outgoing(&db, &text_to(contact(ANN), "synthetic"), NOW, &[])
        .expect("the row to walk");
    store::mark_send_reflected(&db, walked).expect("its reflect's zone");
    let row = db
        .read(|c: &Connection| store::send_task(c, walked))
        .expect("the row")
        .expect("the row");
    let (envelope, nonce) = db
        .read(|c: &Connection| store::seal_sent_update(c, &row))
        .expect("the update");
    let update = SealedUpdate {
        envelope: &envelope,
        nonce,
    };
    let end = store::mark_send_acks(&db, walked, &[ANN], Some(update), NOW);
    assert_eq!(end, Ok(SendEnd::Acked));
    walked
}

/// L1. Before Dry the mediator delivers its backlog first and a reflect-ack comes behind it: a reflection then pushes the deadline
/// (an `acked` row runs from the login-ack, with Dry held). Each one starts the 60 s again; the echoes do not.
#[test]
fn deadline_a_reflection_before_dry_pushes_it() {
    support::offline::run_child(
        module_path!(),
        "deadline_a_reflection_before_dry_pushes_it_child",
    );
}

#[tokio::test]
async fn deadline_a_reflection_before_dry_pushes_it_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_233).with_peer(ANN).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        k.hold_dry = true;
        k.withhold_reflect_acks = true;
    });
    let mut h = harness::start(s);
    acked_row(&h);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    let conn = h.conn();
    wait_reflect_held(&h, conn, 1).await;

    through_echoes(&h, at(0), at(30)).await;
    let text = Msg::text([1; 8], START_MS + 1, "synthetic backlog").nonce([1; 24]);
    h.net()
        .act(|_| phone.reflect_envelope(&Author::new(PHONE).incoming(PEER, &text)));
    wait_acks(&h, 1).await;
    through_echoes(&h, at(0), at(80)).await;
    assert_eq!(m.closed(conn), None, "the backlog pushed nothing");
    h.advance_to(at(90)).await;
    wait_closed_1000(&h, conn, "60 s after the backlog's reflection").await;
    h.finish().await;
}

/// After Dry a reflection is no reason for a late ack: it pushes nothing, the socket ends 60 s after the box.
#[test]
fn deadline_a_reflection_after_dry_pushes_nothing() {
    support::offline::run_child(
        module_path!(),
        "deadline_a_reflection_after_dry_pushes_nothing_child",
    );
}

#[tokio::test]
async fn deadline_a_reflection_after_dry_pushes_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_234)
        .with_peer(ANN)
        .with_contacts(&[PEER])
        .job(text_job(contact(ANN), "synthetic"));
    let mut phone = phone_of(&s);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_held(&h, ANN).await;
    let conn = h.conn();
    through_echoes(&h, at(0), at(30)).await;
    let text = Msg::text([1; 8], START_MS + 1, "synthetic").nonce([1; 24]);
    h.net()
        .act(|_| phone.reflect_envelope(&Author::new(PHONE).incoming(PEER, &text)));
    let got = h.messages(1).await;
    assert!(!got[0].catch_up, "a live text");
    wait_acks(&h, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    wait_closed_1000(&h, conn, "60 s after the box").await;
    h.finish().await;
}

// ---- S5b: the cooling ----------------------------------------------------------------------------------------------------

/// The mutant "first-run-free removed" dies here. A server restart or a one-off close is common: the first close with a box
/// unanswered counts one failed run and the row runs again at the first thing the next socket does; nobody waits for minutes.
#[test]
fn cooling_a_one_off_close_resends_at_once() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_one_off_close_resends_at_once_child",
    );
}

#[tokio::test]
async fn cooling_a_one_off_close_resends_at_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_235)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.close_on_box_to(ANN, REFUSED));
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    assert!(
        h.net().wait(|| m.csp_refused().len() == 1, WALL).await,
        "the chat server never refused the box"
    );
    m.knobs(|k| k.csp.stop_closing_on_box_to(ANN));
    h.wait_state("Offline with the retry", retry_at(at(5)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 1, "one failed run");
    h.advance_to(at(5)).await;
    // Both clocks stand: nothing but the free first run can finish the row.
    wait_rows(&h, 0).await;
    assert_eq!(m.closed(first), Some(REFUSED));
    assert_eq!(m.reflects().len(), 2, "the message once and the update");
    assert_eq!(m.csp_sent().len(), 1, "the box the server took");
    h.finish().await;
}

/// Moves both clocks, deadline by deadline, while the account is up or waits for its retry, until `done`. It does not say when:
/// the row's cooling decides, and a hold of the account ends the test whatever else holds.
async fn walk_until(h: &H, done: impl Fn() -> bool) {
    loop {
        assert!(
            !held(&h.state()),
            "a row the server refuses reached NeedsUserAction"
        );
        if done() {
            return;
        }
        assert!(h.now() < at(8_000), "past a job's whole way to its give-up");
        let next = tokio::time::timeout(WALL, async {
            loop {
                let state = h.state();
                let quiet = match &state {
                    ConnState::Online { .. } => true,
                    ConnState::Offline {
                        retry_at_ms: Some(at),
                    } => *at > h.now(),
                    _ => false,
                };
                if held(&state) || done() {
                    return None;
                }
                if quiet && let Some(next) = h.clock.next_deadline() {
                    return Some(next);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the actor neither waits on its clock nor holds");
        if let Some(next) = next {
            h.step_to(next).await;
        }
    }
}

/// The mutant "cooling removed" dies here, on the `NeedsUserAction` assertion. A server that closes on every box to one receiver,
/// for ever: no row holds the account. After the row's second close a later row completes on the next socket. The row runs on,
/// paced by its cooling, each socket up past `WORKED_AFTER`, and at its 12th failed run the job is given up: gone with its
/// receivers, one WARN by its rule, the nonces permanent, and the account up.
#[test]
fn cooling_repeated_closes_never_reach_the_hold_a_later_row_completes_and_the_job_gives_up() {
    support::offline::run_child(
        module_path!(),
        "cooling_repeated_closes_never_reach_the_hold_a_later_row_completes_and_the_job_gives_up_child",
    );
}

#[tokio::test]
async fn cooling_repeated_closes_never_reach_the_hold_a_later_row_completes_and_the_job_gives_up_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(7_236)
        .with_peer(ANN)
        .with_peer(BOB)
        .job(text_job(contact(ANN), "synthetic refused"))
        .job(text_job(contact(BOB), "synthetic later"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.close_on_box_to(ANN, REFUSED));
    let h = harness::start(s);
    h.send(Command::Connect).await;

    walk_until(&h, || m.csp_sent().len() == 1).await;
    wait_rows(&h, 1).await;
    assert_eq!(m.csp_refused().len(), 2, "the later row waited longer");
    assert_eq!(h.s.world.peer(BOB).drain().len(), 1, "the later row's box");
    assert_eq!(tasks(&h)[0].attempts, 2);

    walk_until(&h, || m.csp_refused().len() == 12).await;
    wait_rows(&h, 0).await;
    assert_eq!(h.file_rows("send_receiver"), 0, "its receivers stayed");
    assert_eq!(h.file_rows("csp_nonce"), 2, "the nonces are permanent");
    assert_eq!(
        warns
            .at(tracing::Level::WARN)
            .iter()
            .filter(|w| *w == "send rows given up: 1 by send-job-given-up")
            .count(),
        1,
        "not one WARN send-job-given-up"
    );
    // About two hours, as R2 says. The next socket comes up and nothing more goes.
    assert_eq!(h.now(), at(7_145), "the row did not run by its cooling");
    let sockets = h.net().opened().len();
    walk_until(&h, || h.net().opened().len() > sockets).await;
    walk_until(&h, || online(&h.state())).await;
    assert_eq!(m.csp_refused().len(), 12, "a box after the give-up");
    h.finish().await;
}

/// A server's close is a failed run of the row only with frames of it unanswered: the reflect-ack and the close in ONE drain leave
/// the reflect answered and its zone committed, and the boxes that zone allows never go (L4): nothing stood unanswered.
#[test]
fn cooling_a_close_found_with_the_reflect_ack_is_no_failed_run() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_close_found_with_the_reflect_ack_is_no_failed_run_child",
    );
}

#[tokio::test]
async fn cooling_a_close_found_with_the_reflect_ack_is_no_failed_run_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_249)
        .with_peer(ANN)
        .with_contacts(&[PEER])
        .job(text_job(contact(ANN), "synthetic"));
    let mut phone = phone_of(&s);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    wait_reflect_held(&h, conn, 1).await;
    in_one_drain(&h, &mut phone, |m| {
        assert_eq!(m.release_reflect_acks(conn), 1, "the held ack");
        m.close(conn, REFUSED);
    })
    .await;
    h.wait_state("Offline", offline_with_a_retry).await;
    let row = tasks(&h).remove(0);
    assert_eq!(row.state, SendState::Reflected, "the pass's zone");
    assert_eq!(row.attempts, 0, "a box that never went is a failed run");
    assert_eq!(m.csp_sent().len(), 0, "a box after the server's close");
    h.finish().await;
}

/// The hold is out of reach however many rows the server refuses one after the other, each with its two closes in under a minute:
/// the mutant that takes the failed run's reset of `Backoff` away dies here, at the sixth row, on the `NeedsUserAction` assertion.
/// Once every row cools the socket stays.
#[test]
fn cooling_a_queue_of_refused_rows_never_reaches_the_hold() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_queue_of_refused_rows_never_reaches_the_hold_child",
    );
}

#[tokio::test]
async fn cooling_a_queue_of_refused_rows_never_reaches_the_hold_child() {
    if !support::offline::in_child() {
        return;
    }
    const ALL: [&str; 7] = [ANN, BOB, CRE, DEE, EVE, "PEER0006", "PEER0007"];
    let mut s = scene(7_248);
    for id in ALL {
        s = s.with_peer(id).job(text_job(contact(id), "synthetic"));
    }
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        for id in ALL {
            k.csp.close_on_box_to(id, REFUSED);
        }
    });
    let h = harness::start(s);
    h.send(Command::Connect).await;
    walk_until(&h, || h.net().opened().len() == 15).await;
    walk_until(&h, || online(&h.state())).await;
    assert_eq!(m.csp_refused().len(), 14, "two closes each");
    let rows = tasks(&h);
    assert_eq!(rows.len(), 7, "every row stands");
    assert!(
        rows.iter().all(|row| row.attempts == 2),
        "two failed runs each"
    );
    assert_eq!(m.closed(h.conn()), None, "the socket after them stays");
    h.finish().await;
}

/// A server that stays silent on a box: `NoAck` always has a minute up, so `Backoff` paces nothing and the cooling does, at
/// literal times. Run 1 at +0 s is free; run 2 at once on the next socket, at +65 s; then 2 minutes after its end, on a socket
/// that stays up and waits for the row by itself; then 4 minutes. Every run writes the same box.
#[test]
fn cooling_a_silent_server_paces_by_the_cooling() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_silent_server_paces_by_the_cooling_child",
    );
}

#[tokio::test]
async fn cooling_a_silent_server_paces_by_the_cooling_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_237)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_boxes(&h, 1).await;

    // Run 1 ends at +60 s, free.
    through_echoes(&h, at(0), at(60)).await;
    reconnect_at(&mut h, at(65)).await;
    wait_boxes(&h, 2).await;
    assert_eq!(tasks(&h)[0].attempts, 1);

    // Run 2 ends at +125 s: the row cools until +245 s, and the next socket has nothing to do with it.
    through_echoes(&h, at(65), at(125)).await;
    reconnect_at(&mut h, at(130)).await;
    assert_eq!(tasks(&h)[0].attempts, 2);
    through_echoes(&h, at(130), at(240)).await;
    assert_eq!(m.csp_sent().len(), 2, "the row ran while it cools");
    let third = h.conn();

    // Run 3, on the socket that stands. It ends at +305 s: 4 minutes, until +545 s.
    h.advance_to(at(245)).await;
    wait_boxes(&h, 3).await;
    assert_eq!(
        m.csp_sent()[2].conn,
        third.0,
        "the row ran without a new socket"
    );
    through_echoes(&h, at(130), at(300)).await;
    h.advance_to(at(305)).await;
    wait_closed_1000(&h, third, "no ack 60 s after the third box").await;
    reconnect_at(&mut h, at(310)).await;
    assert_eq!(tasks(&h)[0].attempts, 3);
    through_echoes(&h, at(310), at(540)).await;
    assert_eq!(m.csp_sent().len(), 3, "the row ran while it cools");

    // Run 4.
    h.advance_to(at(545)).await;
    wait_boxes(&h, 4).await;
    let sent = m.csp_sent();
    assert!(
        sent.iter().all(|one| one.bytes == sent[0].bytes),
        "not the same box every time"
    );
    assert_eq!(m.reflects().len(), 1, "the reflect is durable");
    assert!(online(&h.state()), "the account is up");
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_rows(&h, 0).await;
    h.finish().await;
}

/// R2: "a restart loses the cooling, not the count". A row that cools runs at once under a new actor, and its next failed run
/// cools by the durable count: its fourth, 8 minutes.
#[test]
fn cooling_a_restart_loses_the_cooling_and_not_the_count() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_restart_loses_the_cooling_and_not_the_count_child",
    );
}

#[tokio::test]
async fn cooling_a_restart_loses_the_cooling_and_not_the_count_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_238)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    let task = tasks(&h)[0].id;
    let db = h.db();
    assert_eq!(store::count_failed_run(&db, task), Ok(1));
    assert_eq!(store::count_failed_run(&db, task), Ok(2));
    connect(&mut h).await;
    wait_boxes(&h, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    h.wait_state("Offline with the retry", retry_at(at(65)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 3);

    let mut h = h.restart().await;
    connect(&mut h).await;
    wait_boxes(&h, 2).await;
    through_echoes(&h, at(60), at(120)).await;
    reconnect_at(&mut h, at(125)).await;
    assert_eq!(tasks(&h)[0].attempts, 4, "the count went on");
    // 8 minutes from +120 s: +600 s.
    through_echoes(&h, at(125), at(595)).await;
    assert_eq!(m.csp_sent().len(), 2, "the row ran while it cools");
    h.advance_to(at(600)).await;
    wait_boxes(&h, 3).await;
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_rows(&h, 0).await;
    h.finish().await;
}

/// A job at its 12th failed run ends there, from the durable count: the row goes, one WARN by its rule, and the next socket has
/// nothing to write.
#[test]
fn cooling_a_job_ends_at_its_twelfth_failed_run() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_job_ends_at_its_twelfth_failed_run_child",
    );
}

#[tokio::test]
async fn cooling_a_job_ends_at_its_twelfth_failed_run_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(7_239)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    let task = tasks(&h)[0].id;
    let db = h.db();
    for runs in 1..=10 {
        assert_eq!(store::count_failed_run(&db, task), Ok(runs));
    }
    connect(&mut h).await;
    wait_boxes(&h, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    h.wait_state("Offline with the retry", retry_at(at(65)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 11, "not yet: the 11th of 12");
    let given_up = |warns: &Warns| {
        warns
            .at(tracing::Level::WARN)
            .iter()
            .filter(|w| *w == "send rows given up: 1 by send-job-given-up")
            .count()
    };
    assert_eq!(given_up(&warns), 0);
    h.advance_to(at(65)).await;
    h.wait_state("Online again", online).await;
    // 15 minutes from +60 s: +960 s.
    through_echoes(&h, at(65), at(960)).await;
    wait_boxes(&h, 2).await;
    through_echoes(&h, at(65), at(1_020)).await;
    wait_rows(&h, 0).await;
    assert_eq!(given_up(&warns), 1, "not one WARN send-job-given-up");
    reconnect_at(&mut h, at(1_025)).await;
    h.sync().await;
    assert_eq!(m.csp_sent().len(), 2, "a box after the give-up");
    h.finish().await;
}

/// A give-up the store refuses leaves the row, cooling, with one WARN; the failed run after it ends the job.
#[test]
fn cooling_a_give_up_that_fails_leaves_the_row_cooling() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_give_up_that_fails_leaves_the_row_cooling_child",
    );
}

#[tokio::test]
async fn cooling_a_give_up_that_fails_leaves_the_row_cooling_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    store::fail_send_zone(fp::INSIDE_SEND_GIVE_UP, FULL);
    let s = scene(7_240)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    let task = tasks(&h)[0].id;
    let db = h.db();
    for runs in 1..=11 {
        assert_eq!(store::count_failed_run(&db, task), Ok(runs));
    }
    connect(&mut h).await;
    wait_boxes(&h, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    reconnect_at(&mut h, at(65)).await;
    assert_eq!(tasks(&h)[0].attempts, 12, "the row stands");
    assert_eq!(
        warns
            .at(tracing::Level::WARN)
            .iter()
            .filter(|w| *w == "a send job could not be given up; it cools")
            .count(),
        1,
        "not one WARN of the failed give-up"
    );
    store::fail_send_zone("", 0);
    through_echoes(&h, at(65), at(955)).await;
    assert_eq!(m.csp_sent().len(), 1, "the row ran while it cools");
    h.advance_to(at(960)).await;
    wait_boxes(&h, 2).await;
    through_echoes(&h, at(65), at(1_020)).await;
    wait_rows(&h, 0).await;
    h.finish().await;
}

/// A `transmit` row has no give-up of its own: past 12 failed runs it stands and cools (the core's budget, then the sweep). An
/// `acked` row runs by itself (R1), so this needs no `Transmit`; a run that is answered finishes it.
#[test]
fn cooling_a_transmit_row_has_no_give_up() {
    support::offline::run_child(
        module_path!(),
        "cooling_a_transmit_row_has_no_give_up_child",
    );
}

#[tokio::test]
async fn cooling_a_transmit_row_has_no_give_up_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_241).with_peer(ANN);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    let task = acked_row(&h);
    let db = h.db();
    for runs in 1..=11 {
        assert_eq!(store::count_failed_run(&db, task), Ok(runs));
    }
    connect(&mut h).await;
    let first = h.conn();
    wait_reflect_held(&h, first, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    reconnect_at(&mut h, at(65)).await;
    let row = tasks(&h).remove(0);
    assert_eq!((row.attempts, row.state), (12, SendState::Acked), "the row");
    // 15 minutes from +60 s: +960 s.
    m.knobs(|k| k.withhold_reflect_acks = false);
    through_echoes(&h, at(65), at(955)).await;
    assert_eq!(m.reflects().len(), 1, "the row ran while it cools");
    h.advance_to(at(960)).await;
    wait_file(&h, "the row done", |db| {
        db.read(store::send_tasks).expect("the tasks")[0].state == SendState::Done
    })
    .await;
    h.finish().await;
}

/// R9. A server that drops a byte-identical repeat without an ack: the first box was kept and its ack was lost with the socket, so
/// every later run stays unanswered, and that is all a real server's refusal of a repeat could do to Kourier. The account stays
/// up, the later row goes as soon as the row cools, and the row goes on at its pace.
#[test]
fn cooling_drop_repeats_keeps_the_account_up_and_later_rows_moving() {
    support::offline::run_child(
        module_path!(),
        "cooling_drop_repeats_keeps_the_account_up_and_later_rows_moving_child",
    );
}

#[tokio::test]
async fn cooling_drop_repeats_keeps_the_account_up_and_later_rows_moving_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_242)
        .with_peer(ANN)
        .with_peer(BOB)
        .job(text_job(contact(ANN), "synthetic"))
        .job(text_job(contact(BOB), "synthetic later"));
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        k.csp.withhold_message_ack_of(ANN);
        k.csp.drop_repeats = true;
    });
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_held(&h, ANN).await;

    through_echoes(&h, at(0), at(60)).await;
    reconnect_at(&mut h, at(65)).await;
    assert!(
        h.net()
            .wait(|| m.csp_repeats_dropped().len() == 1, WALL)
            .await,
        "the server never dropped the repeat"
    );
    assert_eq!(tasks(&h).len(), 2, "the later row runs behind the first");

    through_echoes(&h, at(65), at(125)).await;
    reconnect_at(&mut h, at(130)).await;
    wait_rows(&h, 1).await;
    let later = h.s.world.peer(BOB).drain();
    assert_eq!(later.len(), 1, "the later row's box on the next socket");
    assert_eq!(tasks(&h)[0].attempts, 2);

    through_echoes(&h, at(130), at(240)).await;
    h.advance_to(at(245)).await;
    assert!(
        h.net()
            .wait(|| m.csp_repeats_dropped().len() == 2, WALL)
            .await,
        "the row did not run again at its time"
    );
    assert!(online(&h.state()), "the account is up");
    h.finish().await;
}

/// The mutant "`Dead` counted as a failed run" dies here. A lost network says nothing about the row: unanswered echoes end two
/// sockets, a socket is lost without a close frame (1006), and no failed run is counted: every next socket writes the box at once.
#[test]
fn cooling_a_lost_network_does_not_cool() {
    support::offline::run_child(module_path!(), "cooling_a_lost_network_does_not_cool_child");
}

#[tokio::test]
async fn cooling_a_lost_network_does_not_cool_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_243)
        .with_peer(ANN)
        .job(text_job(contact(ANN), "synthetic"));
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        k.csp.withhold_message_ack_of(ANN);
        k.csp.withhold_echo = true;
    });
    let mut h = harness::start(s);
    connect(&mut h).await;
    wait_boxes(&h, 1).await;

    // The echo at +10 s stays unanswered: `Dead` at +20 s.
    h.advance_to(at(10)).await;
    h.advance_to(at(20)).await;
    h.wait_state("Offline with the retry", retry_at(at(25)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 0, "`Dead` counted a failed run");

    h.advance_to(at(25)).await;
    wait_boxes(&h, 2).await;
    h.advance_to(at(35)).await;
    h.advance_to(at(45)).await;
    h.wait_state("Offline with the retry", retry_at(at(55)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 0, "`Dead` counted a failed run");

    h.advance_to(at(55)).await;
    wait_boxes(&h, 3).await;
    h.net().lose(h.conn());
    h.wait_state("Offline with the retry", retry_at(at(70)))
        .await;
    assert_eq!(tasks(&h)[0].attempts, 0, "1006 counted a failed run");

    m.knobs(|k| k.csp.withhold_echo = false);
    h.advance_to(at(70)).await;
    wait_boxes(&h, 4).await;
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_rows(&h, 0).await;
    h.finish().await;
}

// ---- S5b: the stalled write ----------------------------------------------------------------------------------------------

/// A write that stalls (C14, L5) ends the socket 10 s of the clock after it began, whatever else is due: closed 1000, `Dead`, so
/// nothing cools and the row goes at once on the next socket. The frame that stalled never arrived; nothing is torn on this side.
#[test]
fn stalled_a_write_ends_the_socket_in_10_s_of_the_clock() {
    support::offline::run_child(
        module_path!(),
        "stalled_a_write_ends_the_socket_in_10_s_of_the_clock_child",
    );
}

#[tokio::test]
async fn stalled_a_write_ends_the_socket_in_10_s_of_the_clock_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_244).with_peer(ANN);
    // The phone leads, so no promotion comes after `Online` to wake the socket.
    let _phone = phone_of(&s);
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.sync().await;
    let first = h.conn();
    h.wait_deadline(at(10)).await;
    h.advance_through(at(3)).await;

    h.net().stall_mediator_sends(true);
    h.job(&text_job(contact(ANN), "synthetic"));
    h.send(Command::Connect).await;
    assert!(
        h.net()
            .wait(|| h.net().stalled_mediator_sends() == 1, WALL)
            .await,
        "the write never stalled"
    );
    h.advance_to(at(13)).await;
    wait_closed_1000(&h, first, "a stalled write").await;
    h.wait_state("Offline with the retry", retry_at(at(18)))
        .await;
    assert_eq!(m.reflects().len(), 0, "the frame was not delivered");
    assert_eq!(
        tasks(&h)[0].attempts,
        0,
        "a stalled write counted a failed run"
    );

    h.net().stall_mediator_sends(false);
    h.advance_to(at(18)).await;
    wait_rows(&h, 0).await;
    assert_eq!(m.reflects().len(), 2, "the message and its update");
    assert_eq!(m.csp_sent().len(), 1);
    h.finish().await;
}

/// A stalled write does not hold the actor's stop: with no clock moving, `Shutdown` ends it at once and the socket is closed 1000
/// (the harness's `finish` checks both).
#[test]
fn stalled_a_write_does_not_hold_a_shutdown() {
    support::offline::run_child(
        module_path!(),
        "stalled_a_write_does_not_hold_a_shutdown_child",
    );
}

#[tokio::test]
async fn stalled_a_write_does_not_hold_a_shutdown_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_245).with_peer(ANN);
    let _phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.sync().await;
    h.net().stall_mediator_sends(true);
    h.job(&text_job(contact(ANN), "synthetic"));
    h.send(Command::Connect).await;
    assert!(
        h.net()
            .wait(|| h.net().stalled_mediator_sends() == 1, WALL)
            .await,
        "the write never stalled"
    );
    h.finish().await;
}

// ---- S5b: the sweep ------------------------------------------------------------------------------------------------------

/// A job 1 ms past its 14 days at `now`, as a row left behind would be.
fn an_old_job(h: &H, now: u64) -> i64 {
    const FOURTEEN_DAYS: u64 = 14 * 86_400_000;
    let queued = i64::try_from(now - FOURTEEN_DAYS - 1).expect("ms fit i64");
    h.db()
        .zone(|tx| store::enqueue_job(tx, &text_job(contact(ANN), "synthetic"), queued))
        .expect("the row")
}

/// C15. The sweep runs at EVERY socket's first clock-proved `ServerInfo`, not once per actor: a row past its age written after
/// the first socket went is gone when the next socket logs in, and no frame of it went.
#[test]
fn sweep_runs_at_every_socket() {
    support::offline::run_child(module_path!(), "sweep_runs_at_every_socket_child");
}

#[tokio::test]
async fn sweep_runs_at_every_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(7_246).with_peer(ANN);
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    assert!(h.net().close_device(DEVICE, REFUSED), "no connection");
    h.wait_state("Offline with the retry", retry_at(at(5)))
        .await;
    an_old_job(&h, at(5));
    assert_eq!(tasks(&h).len(), 1, "the row is there");
    reconnect_at(&mut h, at(5)).await;
    assert_eq!(tasks(&h).len(), 0, "the second socket did not sweep");
    assert_eq!(m.reflects().len(), 0, "the row ran");
    assert_eq!(
        warns
            .at(tracing::Level::WARN)
            .iter()
            .filter(|w| *w == "send rows dropped by the sweep: 1 by send-job-expired")
            .count(),
        1,
        "not one WARN send-job-expired"
    );
    h.finish().await;
}

/// A clock nothing proved sweeps nothing (C15, the age bound's rule): 21 minutes off the socket holds for the clock; connected
/// anyway it sweeps nothing either (Dry held, so the job does not run and the row can be read).
#[test]
fn sweep_never_runs_under_a_clock_nothing_proved() {
    support::offline::run_child(
        module_path!(),
        "sweep_never_runs_under_a_clock_nothing_proved_child",
    );
}

#[tokio::test]
async fn sweep_never_runs_under_a_clock_nothing_proved_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_247).with_peer(ANN);
    s.world.mediator().knobs(|k| {
        k.clock_skew_ms = 21 * 60_000;
        k.hold_dry = true;
    });
    let mut h = harness::start(s);
    an_old_job(&h, at(0));
    h.send(Command::Connect).await;
    h.wait_state("FixClock", held).await;
    assert_eq!(tasks(&h).len(), 1, "a skewed clock swept the row");
    kourier_threema::connect_anyway(h.uuid());
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    assert_eq!(
        tasks(&h).len(),
        1,
        "connected anyway, the sweep took the row"
    );
    h.finish().await;
}

// ---- S6: the door and the contract -----------------------------------------------------------------------------------------

/// The local of a second, third .. message.
const LOCAL_2: &str = "1112131415161718";
const LOCAL_3: &str = "2122232425262728";
const LOCAL_4: &str = "3132333435363738";
const LOCAL_5: &str = "4142434445464748";
const LOCAL_6: &str = "5152535455565758";
const TEXT: &str = "synthetic contract text";
const TRANSIENT: ErrorKind = ErrorKind::Transient { retry_after: None };

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
}

/// The core's key of a conversation, written out here: the spec's text of a group is not read from the code.
fn conv_key(to: &To) -> ConvKey {
    match to {
        To::Contact(id) => ConvKey::Direct(user(id)),
        To::Group { creator, group_id } => {
            let digits: String = group_id
                .to_le_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            ConvKey::Group(format!("threema:{creator}.{digits}").into())
        }
    }
}

/// A `Transmit` of a plain text.
fn request(to: &To, local: &str, only: Option<&[&str]>) -> OutgoingRequest {
    OutgoingRequest {
        conv: conv_key(to),
        local: Arc::from(local),
        sent_at: T0,
        kind: OutgoingKind::Message {
            body: Body::text(TEXT),
            reply_to: None,
            expiry: None,
        },
        only: only.map(|ids| ids.iter().map(|id| user(id)).collect()),
        is_recipient_update: false,
    }
}

/// A row prepared by the door under `local`.
fn message(to: To, local: &str, created_at: i64) -> NewMessage {
    NewMessage {
        local: local.to_owned(),
        created_at,
        ..text_to(to, TEXT)
    }
}

/// The answer, within the wall bound.
async fn answered(answer: tokio::sync::oneshot::Receiver<TransmitOutcome>) -> TransmitOutcome {
    match tokio::time::timeout(WALL, answer).await {
        Err(_) => panic!("the Transmit was not answered in time"),
        Ok(Err(_)) => panic!("the actor dropped the Transmit's reply"),
        Ok(Ok(outcome)) => outcome,
    }
}

fn sent_at(server_at: Option<u64>, ids: &[&str]) -> TransmitOutcome {
    TransmitOutcome::Sent {
        server_at: server_at.map(|at| i64::try_from(at).expect("ms")),
        per_recipient: acked_list(ids),
    }
}

fn acked_list(ids: &[&str]) -> Vec<(UserId, RecipientState)> {
    ids.iter()
        .map(|id| (user(id), RecipientState::Sent { sealed: false }))
        .collect()
}

fn partial(delivered: &[&str], failed: &[(&str, ErrorKind)]) -> TransmitOutcome {
    TransmitOutcome::Partial {
        delivered: acked_list(delivered),
        failed: failed
            .iter()
            .map(|(id, kind)| (user(id), BackendError::new(kind.clone(), "")))
            .collect(),
    }
}

fn failed(kind: ErrorKind) -> TransmitOutcome {
    TransmitOutcome::Failed(BackendError::new(kind, ""))
}

/// An outcome without the errors' log-safe details: what the core acts on.
fn bare(outcome: &TransmitOutcome) -> TransmitOutcome {
    let plain = |e: &BackendError| BackendError::new(e.kind.clone(), "");
    match outcome {
        TransmitOutcome::Failed(e) => TransmitOutcome::Failed(plain(e)),
        TransmitOutcome::Partial { delivered, failed } => TransmitOutcome::Partial {
            delivered: delivered.clone(),
            failed: failed.iter().map(|(u, e)| (u.clone(), plain(e))).collect(),
        },
        TransmitOutcome::Sent { .. } | TransmitOutcome::SentAs { .. } => outcome.clone(),
    }
}

/// An outcome with no ID in it, for a failed test's sentence.
fn shape(outcome: &TransmitOutcome) -> String {
    match outcome {
        TransmitOutcome::Sent {
            server_at,
            per_recipient,
        } => format!(
            "Sent at {:?} to {}",
            server_at.map(|at| at - i64::try_from(START_MS).expect("ms")),
            per_recipient.len()
        ),
        TransmitOutcome::Partial { delivered, failed } => format!(
            "Partial to {}, failed {:?}",
            delivered.len(),
            failed.iter().map(|(_, e)| &e.kind).collect::<Vec<_>>()
        ),
        TransmitOutcome::Failed(e) => format!("Failed {:?}", e.kind),
        // Threema gives a message no id of its own (KT-2): never answered so.
        TransmitOutcome::SentAs { .. } => "SentAs".to_owned(),
    }
}

/// The literal comparison: `outcome` is `expected`, by what the core acts on.
#[track_caller]
fn says(outcome: &TransmitOutcome, expected: &TransmitOutcome, what: &str) {
    assert!(
        bare(outcome) == *expected,
        "{what}: the answer is {}, not {}",
        shape(outcome),
        shape(expected)
    );
}

/// What the wire has seen so far: reflects, boxes the server kept, boxes it refused.
fn frames(h: &H) -> (usize, usize, usize) {
    let m = h.s.world.mediator();
    (
        m.reflects().len(),
        m.csp_sent().len(),
        m.csp_refused().len(),
    )
}

/// The states of the row's receivers, by ID.
fn states_of(h: &H, task: i64) -> Vec<ReceiverState> {
    receivers_of(h, task).iter().map(|r| r.state).collect()
}

/// The row of `local`.
#[track_caller]
fn row_of(h: &H, local: &str) -> SendTask {
    h.db()
        .read(|c: &Connection| store::send_ledger(c, local))
        .expect("the ledger")
        .expect("the row of a local")
}

/// Waits until the row of `local` stands at `state`.
async fn wait_state_of(h: &H, local: &str, state: SendState) {
    wait_file(h, "the row at its state", |db| {
        db.read(|c: &Connection| store::send_ledger(c, local))
            .expect("the ledger")
            .is_some_and(|row| row.state == state)
    })
    .await;
}

/// The first row of the contract, and the wake-up the `kicked` arm must give: a `Transmit` on an idle socket goes at once, with
/// no clock moved and nothing else to wake the loop (the phone leads, so no promotion comes). The answer comes after ZONE DONE:
/// `Sent` at the time of the update's reflect-ack, a literal only that ack can say; the row is the ledger's; the peer opens the
/// one text and the wire is the literal order. Removing the arm kills this test by its wall bound.
#[test]
fn contract_a_text_goes_at_once_and_is_answered_sent_when_its_row_is_done() {
    support::offline::run_child(
        module_path!(),
        "contract_a_text_goes_at_once_and_is_answered_sent_when_its_row_is_done_child",
    );
}

#[tokio::test]
async fn contract_a_text_goes_at_once_and_is_answered_sent_when_its_row_is_done_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_301).with_peer(ANN).send_open();
    let _phone = phone_of(&s);
    let mut h = harness::start(s);
    h.advance_idle(1_234).await;
    connect(&mut h).await;
    h.sync().await;
    let conn = h.conn();

    let answer = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    let outcome = answered(answer).await;
    says(
        &outcome,
        &sent_at(Some(START_MS + 1_234), &[ANN]),
        "the first send",
    );
    assert_eq!(order_on(&h, conn), ONE_TO_ONE, "the order on the wire");
    let row = row_of(&h, LOCAL);
    assert_eq!(
        row.state,
        SendState::Done,
        "the answer came before the zone"
    );
    assert_eq!(row.origin, SendOrigin::Transmit);
    assert_eq!(row.message_id, ID, "the local is the message's id");
    assert_eq!(row.created_at, T0, "the request's time is the message's");
    assert_eq!(states_of(&h, row.id), [ReceiverState::Acked]);
    let opened = h.s.world.peer(ANN).drain();
    assert_eq!(opened.len(), 1, "one message at the peer");
    assert_eq!(opened[0].ty, ty::TEXT);
    assert!(opened[0].body == body::text(TEXT), "another body");
    h.finish().await;
}

/// THE LEDGER (L7: a ledger mutant dies by the reply, a plain `INSERT` meets `send_by_local`). A second `Transmit` of a `local` on
/// the same socket and a third after a restart, with no socket at all: no frame, no new row, and the answer the first got.
#[test]
fn contract_a_second_transmit_writes_no_frame_and_gets_the_same_answer() {
    support::offline::run_child(
        module_path!(),
        "contract_a_second_transmit_writes_no_frame_and_gets_the_same_answer_child",
    );
}

#[tokio::test]
async fn contract_a_second_transmit_writes_no_frame_and_gets_the_same_answer_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_302).with_peer(ANN).send_open();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let expected = sent_at(Some(START_MS), &[ANN]);
    let first = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&first, &expected, "the first");
    let wire = frames(&h);
    assert_eq!(wire, (2, 1, 0), "a reflect, a box and the update");

    let second = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&second, &expected, "the second, from the ledger");
    h.sync().await;
    assert_eq!(frames(&h), wire, "the second wrote a frame");
    assert_eq!(tasks(&h).len(), 1, "a second row");

    let h = h.restart().await;
    let third = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &third,
        &expected,
        "the third, after a restart and with no socket",
    );
    assert_eq!(frames(&h), wire, "the third wrote a frame");
    h.finish().await;
}

/// A receiver lost for good, and the wire stayed the spec's: `Partial`, `delivered` the two with a box, the invalid one
/// `PeerUnregistered`, the one with no key `Unsupported`, neither retryable (C8, C13); the blocked one is not named (C9), the user
/// has no row. The ledger answers the same.
#[test]
fn contract_a_done_row_with_an_invalid_and_a_no_key_receiver_is_partial() {
    support::offline::run_child(
        module_path!(),
        "contract_a_done_row_with_an_invalid_and_a_no_key_receiver_is_partial_child",
    );
}

#[tokio::test]
async fn contract_a_done_row_with_an_invalid_and_a_no_key_receiver_is_partial_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut s = scene(7_303)
        .with_members(CRE, GROUP, &[ANN, BOB, DEE])
        .with_settings(block(&[BOB]))
        .send_open();
    s.members
        .get_mut(&(CRE.to_owned(), GROUP))
        .expect("the group")
        .extend([ME.to_owned(), EVE.to_owned()]);
    let mut h = harness::start(s);
    bend_contact(&h.db(), DEE, |c| {
        c.activity_state = Some(d2d_sync::contact::ActivityState::Invalid as i32);
    });
    connect(&mut h).await;

    let to = group(CRE, GROUP);
    let outcome = answered(h.transmit(request(&to, LOCAL, None)).await).await;
    let expected = partial(
        &[ANN, CRE],
        &[
            (DEE, ErrorKind::PeerUnregistered),
            (EVE, ErrorKind::Unsupported),
        ],
    );
    says(&outcome, &expected, "invalid and no key");
    assert_eq!(
        outcome.retryable_members(),
        Vec::<UserId>::new(),
        "a resend is offered"
    );
    assert_eq!(frames(&h), (2, 2, 0), "the reflect, two boxes, the update");
    assert_eq!(row_of(&h, LOCAL).state, SendState::Done);
    let again = answered(h.transmit(request(&to, LOCAL, None)).await).await;
    says(&again, &expected, "from the ledger");
    assert_eq!(frames(&h), (2, 2, 0), "the ledger wrote a frame");
    h.finish().await;
}

/// Nobody to send to is no failure: a notes group, and a chat whose one receiver the user blocked, are `Sent` to an empty list,
/// at the update's time, with no box (Desktop's view too).
#[test]
fn contract_a_notes_group_and_a_blocked_contact_are_sent_to_an_empty_list() {
    support::offline::run_child(
        module_path!(),
        "contract_a_notes_group_and_a_blocked_contact_are_sent_to_an_empty_list_child",
    );
}

#[tokio::test]
async fn contract_a_notes_group_and_a_blocked_contact_are_sent_to_an_empty_list_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_304)
        .with_members(ME, GROUP, &[])
        .with_peer(ANN)
        .with_settings(block(&[ANN]))
        .send_open();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let notes = answered(h.transmit(request(&group(ME, GROUP), LOCAL, None)).await).await;
    says(&notes, &sent_at(Some(START_MS), &[]), "a notes group");
    let blocked = answered(h.transmit(request(&contact(ANN), LOCAL_2, None)).await).await;
    says(&blocked, &sent_at(Some(START_MS), &[]), "a blocked contact");
    assert_eq!(frames(&h), (4, 0, 0), "two reflects each and no box");
    assert_eq!(
        states_of(&h, row_of(&h, LOCAL_2).id),
        [ReceiverState::Blocked]
    );
    h.finish().await;
}

/// The socket is lost with some acked AND some pending: `Partial`, the pending one retryable. A lost network is no failed run. The
/// core's next attempt names that one: the same box goes to it alone, and the row finishes with `Sent` for it at the real time.
#[test]
fn contract_a_lost_socket_with_some_acked_and_some_pending_is_partial() {
    support::offline::run_child(
        module_path!(),
        "contract_a_lost_socket_with_some_acked_and_some_pending_is_partial_child",
    );
}

#[tokio::test]
async fn contract_a_lost_socket_with_some_acked_and_some_pending_is_partial_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_305)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(BOB));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let to = group(CRE, GROUP);
    let answer = h.transmit(request(&to, LOCAL, None)).await;
    wait_held(&h, BOB).await;
    wait_file(&h, "the two acks durable", |db| {
        let row = db
            .read(|c: &Connection| store::send_ledger(c, LOCAL))
            .expect("the ledger")
            .expect("the row");
        db.read(|c: &Connection| store::send_receivers(c, row.id))
            .expect("the receivers")
            .iter()
            .filter(|r| r.state == ReceiverState::Acked)
            .count()
            == 2
    })
    .await;
    let boxes = m.csp_sent();
    assert_eq!(boxes.len(), 3, "one box each");

    h.net().lose(h.conn());
    let outcome = answered(answer).await;
    says(
        &outcome,
        &partial(&[ANN, CRE], &[(BOB, TRANSIENT)]),
        "some acked and some pending",
    );
    assert_eq!(outcome.retryable_members(), [user(BOB)]);
    assert_eq!(
        row_of(&h, LOCAL).attempts,
        0,
        "a lost network is no failed run"
    );

    h.net().act(|m| m.release_message_acks(BOB));
    reconnect_at(&mut h, at(5)).await;
    let second = answered(h.transmit(request(&to, LOCAL, Some(&[BOB]))).await).await;
    says(
        &second,
        &sent_at(Some(at(5)), &[BOB]),
        "the retry of the one that was pending",
    );
    let again = m.csp_sent();
    assert_eq!(again.len(), 4, "the box of the one it names, and no other");
    let first_bob = boxes
        .iter()
        .find(|b| b.receiver.as_slice() == BOB.as_bytes())
        .expect("its first box");
    assert!(again[3].bytes == first_bob.bytes, "not the same box");
    assert_eq!(m.reflects().len(), 2, "the message once and its update");
    assert_eq!(
        h.s.world
            .peer(BOB)
            .drain()
            .iter()
            .map(|o| o.discard)
            .collect::<Vec<_>>(),
        [None, Some(Discard::Nonce)],
        "one message at the peer"
    );
    h.finish().await;
}

/// No ack within 60 s and none acked: `Failed(Transient)` at the deadline and not before. The attempt is over: the next socket
/// leaves the row alone until the core comes back, and then writes THE SAME box.
#[test]
fn contract_no_ack_with_none_acked_is_failed_transient_at_the_deadline() {
    support::offline::run_child(
        module_path!(),
        "contract_no_ack_with_none_acked_is_failed_transient_at_the_deadline_child",
    );
}

#[tokio::test]
async fn contract_no_ack_with_none_acked_is_failed_transient_at_the_deadline_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_306).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let mut answer = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_held(&h, ANN).await;
    through_echoes(&h, at(0), at(50)).await;
    assert!(answer.try_recv().is_err(), "answered before the deadline");
    h.advance_to(at(60)).await;
    says(&answered(answer).await, &failed(TRANSIENT), "no ack");

    reconnect_at(&mut h, at(65)).await;
    h.sync().await;
    assert_eq!(m.csp_sent().len(), 1, "a row with no attempt moved");
    assert_eq!(row_of(&h, LOCAL).state, SendState::Reflected);

    let retry = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_boxes(&h, 2).await;
    let sent = m.csp_sent();
    assert!(sent[1].bytes == sent[0].bytes, "not the same box");
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    says(
        &answered(retry).await,
        &sent_at(Some(at(65)), &[ANN]),
        "the retry",
    );
    h.finish().await;
}

/// R1, THE CRITIC'S H1: the socket goes between the last box's ack and the update's reflect-ack. The answer is `Failed(Transient)`,
/// never a `Partial` with nobody failed (the core would stop and never hear the time). The next socket writes THE SAME update with
/// no `Transmit` attached, and the retry finds `done` and the real `server_at`.
#[test]
fn contract_an_acked_row_is_answered_transient_and_finishes_by_itself() {
    support::offline::run_child(
        module_path!(),
        "contract_an_acked_row_is_answered_transient_and_finishes_by_itself_child",
    );
}

#[tokio::test]
async fn contract_an_acked_row_is_answered_transient_and_finishes_by_itself_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_307).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let answer = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_held(&h, ANN).await;
    let first = h.conn();
    m.knobs(|k| k.withhold_reflect_acks = true);
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    wait_reflect_held(&h, first, 1).await;
    wait_state_of(&h, LOCAL, SendState::Acked).await;

    h.net().lose(first);
    says(&answered(answer).await, &failed(TRANSIENT), "state acked");
    let update = m.reflects()[1].bytes.clone();
    m.knobs(|k| k.withhold_reflect_acks = false);

    reconnect_at(&mut h, at(5)).await;
    wait_state_of(&h, LOCAL, SendState::Done).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 3, "the message and the update twice");
    assert!(reflects[2].bytes == update, "not the same update");
    assert_eq!(m.csp_sent().len(), 1, "a box after its ack was durable");

    let retry = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &retry,
        &sent_at(Some(at(5)), &[ANN]),
        "the retry finds done",
    );
    assert_eq!(m.reflects().len(), 3, "the ledger wrote a frame");
    h.finish().await;
}

/// The same two rows when the ack stays away instead (`NoAck`, 60 s): some acked and some pending is `Partial`, and the row, once it
/// is `acked`, `Failed(Transient)`.
#[test]
fn contract_no_ack_with_some_acked_is_partial_and_on_an_acked_row_transient() {
    support::offline::run_child(
        module_path!(),
        "contract_no_ack_with_some_acked_is_partial_and_on_an_acked_row_transient_child",
    );
}

#[tokio::test]
async fn contract_no_ack_with_some_acked_is_partial_and_on_an_acked_row_transient_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_322)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(BOB));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let to = group(CRE, GROUP);
    let answer = h.transmit(request(&to, LOCAL, None)).await;
    wait_held(&h, BOB).await;
    through_echoes(&h, at(0), at(60)).await;
    says(
        &answered(answer).await,
        &partial(&[ANN, CRE], &[(BOB, TRANSIENT)]),
        "no ack from one of three",
    );

    // The next socket: the one that was pending goes, its ack comes, and the update's ack stays away.
    reconnect_at(&mut h, at(65)).await;
    let conn = h.conn();
    let retry = h.transmit(request(&to, LOCAL, Some(&[BOB]))).await;
    wait_boxes(&h, 4).await;
    m.knobs(|k| k.withhold_reflect_acks = true);
    assert_eq!(h.net().act(|m| m.release_message_acks(BOB)), 1);
    wait_reflect_held(&h, conn, 1).await;
    wait_state_of(&h, LOCAL, SendState::Acked).await;
    through_echoes(&h, at(65), at(125)).await;
    says(
        &answered(retry).await,
        &failed(TRANSIENT),
        "no ack of the update",
    );
    h.finish().await;
}

/// The socket is lost with none acked: `Failed(Transient)`, and an attached row that had not begun gets the same: nothing stays
/// attached past a socket.
#[test]
fn contract_a_lost_socket_with_none_acked_is_failed_transient_for_every_attempt() {
    support::offline::run_child(
        module_path!(),
        "contract_a_lost_socket_with_none_acked_is_failed_transient_for_every_attempt_child",
    );
}

#[tokio::test]
async fn contract_a_lost_socket_with_none_acked_is_failed_transient_for_every_attempt_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_323).with_peer(ANN).with_peer(BOB).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let running = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_held(&h, ANN).await;
    let behind = h.transmit(request(&contact(BOB), LOCAL_2, None)).await;
    h.sync().await;
    assert_eq!(m.reflects().len(), 1, "one row at a time");

    h.net().lose(h.conn());
    says(
        &answered(running).await,
        &failed(TRANSIENT),
        "the row that ran",
    );
    says(
        &answered(behind).await,
        &failed(TRANSIENT),
        "the row behind it",
    );
    assert_eq!(row_of(&h, LOCAL_2).state, SendState::Prepared, "it stands");
    h.finish().await;
}

/// A zone the store fails on an open row, with none acked: `Failed(Transient)`, no box; the row stands for the next attempt.
#[test]
fn contract_a_reflected_zone_the_store_fails_is_failed_transient() {
    support::offline::run_child(
        module_path!(),
        "contract_a_reflected_zone_the_store_fails_is_failed_transient_child",
    );
}

#[tokio::test]
async fn contract_a_reflected_zone_the_store_fails_is_failed_transient_child() {
    if !support::offline::in_child() {
        return;
    }
    store::fail_send_zone(fp::INSIDE_SEND_REFLECTED, FULL);
    let s = scene(7_324).with_peer(ANN).send_open();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &outcome,
        &failed(TRANSIENT),
        "ZONE REFLECTED failed by the store",
    );
    wait_closed_1000(&h, conn, "the store failed").await;
    assert_eq!(
        row_of(&h, LOCAL).state,
        SendState::Prepared,
        "the row stands"
    );
    assert_eq!(h.s.world.mediator().csp_sent().len(), 0, "a box");
    store::fail_send_zone("", 0);
    h.finish().await;
}

/// C5: a `transmit` row runs from the login-ack (the core's `can_send`), not from `Online`: the whole send goes while Dry is held.
#[test]
fn contract_a_transmit_goes_at_syncing_before_dry() {
    support::offline::run_child(
        module_path!(),
        "contract_a_transmit_goes_at_syncing_before_dry_child",
    );
}

#[tokio::test]
async fn contract_a_transmit_goes_at_syncing_before_dry_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_325).with_peer(ANN).send_open();
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&outcome, &sent_at(Some(START_MS), &[ANN]), "at Syncing");
    assert!(syncing(&h.state()), "the catch-up is not over");
    h.finish().await;
}

/// No socket past the login-ack: `Failed(Transient)` and nothing written, so nothing passes its point of no return with no way to
/// go on. A `Disconnect` answers what was waiting the same way, and a row stays what it was.
#[test]
fn contract_with_no_socket_a_transmit_is_transient_and_writes_nothing() {
    support::offline::run_child(
        module_path!(),
        "contract_with_no_socket_a_transmit_is_transient_and_writes_nothing_child",
    );
}

#[tokio::test]
async fn contract_with_no_socket_a_transmit_is_transient_and_writes_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_308).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    let early = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&early, &failed(TRANSIENT), "before any socket");
    assert_eq!(tasks(&h).len(), 0, "a row without a socket");
    assert_eq!(h.file_rows("csp_nonce"), 0, "a nonce without a socket");
    assert_eq!(h.file_rows("d2d_nonce"), 0, "a nonce without a socket");

    connect(&mut h).await;
    let waiting = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_held(&h, ANN).await;
    h.send(Command::Disconnect).await;
    says(&answered(waiting).await, &failed(TRANSIENT), "a Disconnect");
    h.wait_state("Offline", |s| {
        *s == ConnState::Offline { retry_at_ms: None }
    })
    .await;
    assert_eq!(
        row_of(&h, LOCAL).state,
        SendState::Reflected,
        "the row stands"
    );
    let wire = frames(&h);
    let late = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &late,
        &failed(TRANSIENT),
        "a row that stands, with no socket",
    );
    assert_eq!(frames(&h), wire, "a frame with no socket");
    assert_eq!(tasks(&h).len(), 1, "a second row");
    h.finish().await;
}

/// Everything refused before PREPARE, on a live socket, each with its literal kind: nothing is written and no frame goes.
#[test]
fn contract_what_is_refused_writes_nothing_and_says_why() {
    support::offline::run_child(
        module_path!(),
        "contract_what_is_refused_writes_nothing_and_says_why_child",
    );
}

#[tokio::test]
async fn contract_what_is_refused_writes_nothing_and_says_why_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_309)
        .with_peer(ANN)
        .with_members(CRE, GROUP, &[ANN])
        .send_open();
    let mut h = harness::start(s);
    bend_group(&h.db(), CRE, GROUP, |g| {
        g.user_state = Some(d2d_sync::group::UserState::Left as i32);
    });
    connect(&mut h).await;
    let plain = request(&contact(ANN), LOCAL, None);
    let with_kind = |kind: OutgoingKind| OutgoingRequest {
        kind,
        ..plain.clone()
    };
    let target = MsgTarget {
        conv: Some(plain.conv.clone()),
        author: TargetAuthor::Own,
        local: LOCAL_2.into(),
        alternates: Vec::new(),
    };
    let other_network = UserId::parse("aci:00000000-0000-0000-0000-000000000000").expect("a user");
    let cases: Vec<(&str, OutgoingRequest, ErrorKind)> = vec![
        (
            "a local that is no id",
            request(&contact(ANN), "not-an-id", None),
            ErrorKind::InvalidInput,
        ),
        (
            "a local in upper case",
            request(&contact(ANN), "00FF112233445566", None),
            ErrorKind::InvalidInput,
        ),
        (
            "only of another network",
            OutgoingRequest {
                only: Some(vec![other_network]),
                ..plain.clone()
            },
            ErrorKind::InvalidInput,
        ),
        (
            "only with no ID",
            request(&contact(ANN), LOCAL, Some(&["short"])),
            ErrorKind::InvalidInput,
        ),
        (
            "an unknown contact",
            request(&contact(EVE), LOCAL, None),
            ErrorKind::InvalidInput,
        ),
        (
            "an unknown group",
            request(&group(CRE, GROUP + 1), LOCAL, None),
            ErrorKind::InvalidInput,
        ),
        (
            "a group the user left",
            request(&group(CRE, GROUP), LOCAL, None),
            ErrorKind::InvalidInput,
        ),
        (
            "a list",
            OutgoingRequest {
                conv: ConvKey::Broadcast("threema:a-list".into()),
                ..plain.clone()
            },
            ErrorKind::Unsupported,
        ),
        (
            "a reaction",
            with_kind(OutgoingKind::React {
                target: target.clone(),
                emoji: "x".to_owned(),
                remove: false,
            }),
            ErrorKind::Unsupported,
        ),
        (
            "a delete",
            with_kind(OutgoingKind::Delete {
                target,
                for_everyone: true,
            }),
            ErrorKind::Unsupported,
        ),
        (
            "a body that is no text",
            with_kind(OutgoingKind::Message {
                body: Body::Unsupported {
                    kind_hint: "x".to_owned(),
                    summary: None,
                },
                reply_to: None,
                expiry: None,
            }),
            ErrorKind::Unsupported,
        ),
        (
            "a text with a timer",
            with_kind(OutgoingKind::Message {
                body: Body::text(TEXT),
                reply_to: None,
                expiry: Some(Duration::from_secs(60)),
            }),
            ErrorKind::Unsupported,
        ),
        (
            "a text over the box's limit",
            with_kind(OutgoingKind::Message {
                body: Body::text("x".repeat(9_000)),
                reply_to: None,
                expiry: None,
            }),
            ErrorKind::TooLarge,
        ),
    ];
    for (what, req, kind) in cases {
        let outcome = answered(h.transmit(req).await).await;
        says(&outcome, &failed(kind), what);
        assert_eq!(tasks(&h).len(), 0, "{what}: a row");
        assert_eq!(h.file_rows("csp_nonce"), 0, "{what}: a nonce");
        assert_eq!(h.file_rows("d2d_nonce"), 0, "{what}: a nonce");
    }
    h.sync().await;
    assert_eq!(frames(&h), (0, 0, 0), "a refusal wrote a frame");
    h.finish().await;
}

/// The same `local` in another chat cannot be this message (2^-64 per pair, or a bug): `Internal`, no second row, no frame.
#[test]
fn contract_the_same_local_in_another_chat_is_internal() {
    support::offline::run_child(
        module_path!(),
        "contract_the_same_local_in_another_chat_is_internal_child",
    );
}

#[tokio::test]
async fn contract_the_same_local_in_another_chat_is_internal_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_310).with_peer(ANN).with_peer(BOB).send_open();
    let h = harness::start(s);
    store::prepare_outgoing(&h.db(), &text_to(contact(ANN), TEXT), NOW, &[]).expect("the row");
    let outcome = answered(h.transmit(request(&contact(BOB), LOCAL, None)).await).await;
    says(&outcome, &failed(ErrorKind::Internal), "another chat");
    assert_eq!(tasks(&h).len(), 1, "a second row");
    h.finish().await;
}

/// R3's budget through the actor: a full queue is `Transient { 60 s }` and nothing is written.
#[test]
fn contract_a_full_queue_waits_a_minute() {
    support::offline::run_child(module_path!(), "contract_a_full_queue_waits_a_minute_child");
}

#[tokio::test]
async fn contract_a_full_queue_waits_a_minute_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_311).with_peer(ANN).send_open();
    let mut h = harness::start(s);
    h.db()
        .zone(|tx| {
            for n in 0..store::TRANSMIT_BUDGET.tasks {
                tx.execute(
                    "INSERT INTO send_task (origin, kind, local, message_id, conversation, created_at, \
                       state, has_update, bytes, queued_at, reflect_env) \
                     VALUES ('transmit', 'text', ?1, ?2, x'0a', 1, 'prepared', 1, 40, ?3, zeroblob(40))",
                    params![format!("{n:016x}"), n, NOW],
                )?;
            }
            Ok::<(), kourier_db::Error>(())
        })
        .expect("a full budget");
    connect(&mut h).await;
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &outcome,
        &failed(ErrorKind::Transient {
            retry_after: Some(Duration::from_secs(60)),
        }),
        "a full queue",
    );
    assert_eq!(h.file_rows("send_task"), 2_000, "a row over the budget");
    assert_eq!(h.file_rows("csp_nonce"), 0, "a nonce over the budget");
    h.finish().await;
}

/// S-M1 (b) through the actor: a full queue of rows nobody owns and more than 24 hours old makes room for a NEW `Transmit`
/// (its PREPARE takes the oldest of them in the same zone and the message gets its row), and the row a `Transmit` is
/// attached to is never taken, though it is the oldest of all: the attached ids reach PREPARE (the mutant `&[]` in `decide`
/// dies here). The first row's reflect-ack is withheld, so it stays attached and running while the second message comes.
#[test]
fn contract_a_full_queue_makes_room_from_old_rows_and_never_takes_the_attached_one() {
    support::offline::run_child(
        module_path!(),
        "contract_a_full_queue_makes_room_from_old_rows_and_never_takes_the_attached_one_child",
    );
}

#[tokio::test]
async fn contract_a_full_queue_makes_room_from_old_rows_and_never_takes_the_attached_one_child() {
    if !support::offline::in_child() {
        return;
    }
    const HOUR: i64 = 3_600_000;
    let s = scene(7_313).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    let now = i64::try_from(h.now()).expect("ms");
    // Id 1 is the oldest row of all (100 hours) and the first `Transmit` will attach to it; ids 2 to 2000 are 25 hours old.
    store::prepare_outgoing(
        &h.db(),
        &message(contact(ANN), LOCAL, T0),
        now - 100 * HOUR,
        &[],
    )
    .expect("the oldest row");
    h.db()
        .zone(|tx| {
            for n in 1..store::TRANSMIT_BUDGET.tasks {
                tx.execute(
                    "INSERT INTO send_task (origin, kind, local, message_id, conversation, created_at, \
                       state, has_update, bytes, queued_at, reflect_env) \
                     VALUES ('transmit', 'text', ?1, ?2, x'0a', 1, 'prepared', 1, 40, ?3, zeroblob(40))",
                    params![format!("{n:016x}"), n, now - 25 * HOUR],
                )?;
            }
            Ok::<(), kourier_db::Error>(())
        })
        .expect("a full budget");
    assert_eq!(h.file_rows("send_task"), 2_000, "the budget is spent");
    connect(&mut h).await;
    let conn = h.conn();
    let _first = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_reflect_held(&h, conn, 1).await;

    let _second = h.transmit(request(&contact(ANN), LOCAL_2, None)).await;
    h.sync().await;
    let rows = tasks(&h);
    assert_eq!(rows.len(), 2_000, "one out, one in");
    assert!(
        rows.iter()
            .any(|r| r.id == 1 && r.local.as_deref() == Some(LOCAL)),
        "the row a Transmit is attached to was taken"
    );
    assert!(
        rows.iter().all(|r| r.id != 2),
        "the oldest row nobody owns stayed"
    );
    assert!(
        rows.iter().any(|r| r.local.as_deref() == Some(LOCAL_2)),
        "the new message has no row"
    );
    m.knobs(|k| k.withhold_reflect_acks = false);
    h.finish().await;
}

/// R6, THE STORE'S FAILURE: with ZONE DONE failing by code 13 the reply is `Failed(Transient)`, NEVER `Sent`: an answer comes after
/// the zone that makes it true (the mutant "answer before `mark_send_done`" dies here, with no race). The socket closes 1000, the
/// row stays `acked`, the next socket finishes it by itself, and the retry gets the real time.
#[test]
fn contract_a_done_zone_the_store_fails_is_never_answered_sent() {
    support::offline::run_child(
        module_path!(),
        "contract_a_done_zone_the_store_fails_is_never_answered_sent_child",
    );
}

#[tokio::test]
async fn contract_a_done_zone_the_store_fails_is_never_answered_sent_child() {
    if !support::offline::in_child() {
        return;
    }
    store::fail_send_zone(fp::INSIDE_SEND_DONE, FULL);
    let s = scene(7_312).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &outcome,
        &failed(TRANSIENT),
        "ZONE DONE failed by the store",
    );
    wait_closed_1000(&h, conn, "the store failed").await;
    assert_eq!(row_of(&h, LOCAL).state, SendState::Acked, "the row stands");

    store::fail_send_zone("", 0);
    h.send(Command::Connect).await;
    wait_state_of(&h, LOCAL, SendState::Done).await;
    assert_eq!(m.reflects().len(), 3, "the update again, by itself");
    let retry = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&retry, &sent_at(Some(START_MS), &[ANN]), "the retry");
    h.finish().await;
}

/// R2, ZONE DONE failing by its CONTENT: no close, the row cools and the attempt is answered at once: it would wait for nothing. The
/// row goes on at its thaw by itself, and the retry gets its time.
#[test]
fn contract_a_done_zone_that_fails_by_content_cools_and_is_answered() {
    support::offline::run_child(
        module_path!(),
        "contract_a_done_zone_that_fails_by_content_cools_and_is_answered_child",
    );
}

#[tokio::test]
async fn contract_a_done_zone_that_fails_by_content_cools_and_is_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    store::fail_send_zone(fp::INSIDE_SEND_DONE, CONSTRAINT_CHECK);
    let s = scene(7_313).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &outcome,
        &failed(TRANSIENT),
        "ZONE DONE failed by its content",
    );
    assert_eq!(
        m.closed(conn),
        None,
        "a failure by content closed the socket"
    );
    let row = row_of(&h, LOCAL);
    assert_eq!((row.state, row.attempts), (SendState::Acked, 1), "the row");

    store::fail_send_zone("", 0);
    through_echoes(&h, at(0), at(120)).await;
    wait_state_of(&h, LOCAL, SendState::Done).await;
    let retry = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&retry, &sent_at(Some(at(120)), &[ANN]), "the retry");
    h.finish().await;
}

/// R2: ZONE PREPARE failing by its content is `Internal`, by the store `Transient`; nothing is written either way, and no frame goes.
#[test]
fn contract_a_prepare_that_fails_says_whose_failure_it_was() {
    support::offline::run_child(
        module_path!(),
        "contract_a_prepare_that_fails_says_whose_failure_it_was_child",
    );
}

#[tokio::test]
async fn contract_a_prepare_that_fails_says_whose_failure_it_was_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_314).with_peer(ANN).send_open();
    let mut h = harness::start(s);
    connect(&mut h).await;
    for (code, kind) in [(CONSTRAINT_CHECK, ErrorKind::Internal), (FULL, TRANSIENT)] {
        store::fail_send_zone(fp::INSIDE_SEND_PREPARE, code);
        let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
        says(&outcome, &failed(kind), "ZONE PREPARE failed");
        assert_eq!(tasks(&h).len(), 0, "a row of a failed zone");
        assert_eq!(h.file_rows("csp_nonce"), 0, "a nonce of a failed zone");
    }
    store::fail_send_zone("", 0);
    h.sync().await;
    assert_eq!(frames(&h), (0, 0, 0), "a frame of a failed zone");
    h.finish().await;
}

/// R2 + S5b's hand-over: A `Transmit` of a row that cools writes NO frame and is told the time left, asked BEFORE an attempt could
/// attach. At its time the row goes. The first failed run was counted before the actor came, so this one cools.
#[test]
fn contract_a_transmit_of_a_cooling_row_writes_nothing_and_is_told_the_time_left() {
    support::offline::run_child(
        module_path!(),
        "contract_a_transmit_of_a_cooling_row_writes_nothing_and_is_told_the_time_left_child",
    );
}

#[tokio::test]
async fn contract_a_transmit_of_a_cooling_row_writes_nothing_and_is_told_the_time_left_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_315).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.close_on_box_to(ANN, REFUSED));
    let mut h = harness::start(s);
    {
        let db = h.db();
        let task =
            store::prepare_outgoing(&db, &text_to(contact(ANN), TEXT), NOW, &[]).expect("the row");
        assert_eq!(store::count_failed_run(&db, task), Ok(1));
    }
    connect(&mut h).await;
    let answer = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    says(
        &answered(answer).await,
        &failed(TRANSIENT),
        "the server's close",
    );
    assert_eq!(m.csp_refused().len(), 1, "the box the server refused");
    m.knobs(|k| k.csp.stop_closing_on_box_to(ANN));
    assert_eq!(row_of(&h, LOCAL).attempts, 2, "the second failed run");

    reconnect_at(&mut h, at(5)).await;
    let wire = frames(&h);
    let cooling = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(
        &cooling,
        &failed(ErrorKind::Transient {
            retry_after: Some(Duration::from_secs(115)),
        }),
        "a row that cools until +120 s, at +5 s",
    );
    h.sync().await;
    assert_eq!(frames(&h), wire, "a cooling row wrote a frame");

    through_echoes(&h, at(5), at(120)).await;
    let later = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&later, &sent_at(Some(at(120)), &[ANN]), "after the thaw");
    h.finish().await;
}

/// R4, THE FIRST ROW: `only` names one of three: that box alone goes, the answer is `Sent` for it with no time, the row stays
/// `reflected`, no update. The rest come through the next `only`, whose last ack writes the update and answers the real time for
/// those it names. The ledger answers for whom each question names; a name the row has no receiver for is told, and writes nothing.
#[test]
fn contract_only_goes_to_those_it_names_and_answers_for_them() {
    support::offline::run_child(
        module_path!(),
        "contract_only_goes_to_those_it_names_and_answers_for_them_child",
    );
}

#[tokio::test]
async fn contract_only_goes_to_those_it_names_and_answers_for_them_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_316)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .send_open();
    let m = s.world.mediator().clone();
    let mut h = harness::start(s);
    let to = group(CRE, GROUP);
    {
        let db = h.db();
        let task =
            store::prepare_outgoing(&db, &text_to(to.clone(), TEXT), NOW, &[]).expect("the row");
        store::mark_send_reflected(&db, task).expect("its reflect's zone");
    }
    connect(&mut h).await;
    h.sync().await;
    assert_eq!(frames(&h), (0, 0, 0), "a row with no attempt moved");

    let first = answered(h.transmit(request(&to, LOCAL, Some(&[BOB]))).await).await;
    says(&first, &sent_at(None, &[BOB]), "one of three");
    let task = row_of(&h, LOCAL);
    assert_eq!(task.state, SendState::Reflected, "the others are pending");
    assert_eq!(task.update_env, None, "an update with others pending");
    assert_eq!(
        states_of(&h, task.id),
        [
            ReceiverState::Pending,
            ReceiverState::Acked,
            ReceiverState::Pending
        ]
    );
    assert_eq!(frames(&h), (0, 1, 0), "the one box, no update");
    assert!(
        m.csp_sent()[0].receiver.as_slice() == BOB.as_bytes(),
        "the box of another"
    );

    let through = answered(h.transmit(request(&to, LOCAL, Some(&[BOB]))).await).await;
    says(
        &through,
        &sent_at(None, &[BOB]),
        "asked again, from the rows",
    );
    assert_eq!(frames(&h), (0, 1, 0), "the same again wrote a frame");

    let rest = answered(h.transmit(request(&to, LOCAL, Some(&[ANN, CRE]))).await).await;
    says(
        &rest,
        &sent_at(Some(START_MS), &[ANN, CRE]),
        "the last two, with the row's end",
    );
    assert_eq!(row_of(&h, LOCAL).state, SendState::Done);
    assert_eq!(frames(&h), (1, 3, 0), "their boxes and the update");

    let all = answered(h.transmit(request(&to, LOCAL, None)).await).await;
    says(
        &all,
        &sent_at(Some(START_MS), &[ANN, BOB, CRE]),
        "the ledger for everyone",
    );
    let stranger = answered(h.transmit(request(&to, LOCAL, Some(&[DEE]))).await).await;
    says(
        &stranger,
        &partial(&[], &[(DEE, ErrorKind::Unsupported)]),
        "a name the row has no receiver for",
    );
    assert_eq!(frames(&h), (1, 3, 0), "the ledger wrote a frame");
    h.finish().await;
}

/// R4, THE SECOND ROW: no ledger row and `only` is `Some`: PREPARE gives a box to those alone, the others no row, the reflect's
/// `nonces` lists those, and the row can reach `done`.
#[test]
fn contract_only_with_no_ledger_row_prepares_for_those_alone_and_reaches_done() {
    support::offline::run_child(
        module_path!(),
        "contract_only_with_no_ledger_row_prepares_for_those_alone_and_reaches_done_child",
    );
}

#[tokio::test]
async fn contract_only_with_no_ledger_row_prepares_for_those_alone_and_reaches_done_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_317)
        .with_members(CRE, GROUP, &[ANN, BOB])
        .send_open();
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let to = group(CRE, GROUP);
    let outcome = answered(h.transmit(request(&to, LOCAL, Some(&[BOB]))).await).await;
    says(
        &outcome,
        &sent_at(Some(START_MS), &[BOB]),
        "a new row under only",
    );
    let task = row_of(&h, LOCAL);
    assert_eq!(task.state, SendState::Done);
    assert_eq!(
        states_of(&h, task.id),
        [ReceiverState::Acked],
        "one receiver"
    );
    assert_eq!(frames(&h), (2, 1, 0), "one box");
    let opened = h.s.world.peer(BOB).drain();
    assert_eq!(opened.len(), 1, "one message at the one it names");
    let at_the_phone = phone.take_reflected();
    let Content::OutgoingMessage(message) = reflected(&h.s, &at_the_phone[0].1) else {
        panic!("the first reflect is no OutgoingMessage");
    };
    assert!(
        message.nonces == [opened[0].nonce.to_vec()],
        "the reflect does not list exactly the one box"
    );
    h.finish().await;
}

/// A second `Transmit` of a `local` with a reply attached answers the older one `Transient`; the newer gets the end. The row runs
/// once on this socket: no second box.
#[test]
fn contract_a_second_transmit_while_one_waits_answers_the_older_transient() {
    support::offline::run_child(
        module_path!(),
        "contract_a_second_transmit_while_one_waits_answers_the_older_transient_child",
    );
}

#[tokio::test]
async fn contract_a_second_transmit_while_one_waits_answers_the_older_transient_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_318).with_peer(ANN).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.csp.withhold_message_ack_of(ANN));
    let mut h = harness::start(s);
    connect(&mut h).await;
    let older = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    wait_held(&h, ANN).await;
    let newer = h.transmit(request(&contact(ANN), LOCAL, None)).await;
    says(&answered(older).await, &failed(TRANSIENT), "the older");
    h.sync().await;
    assert_eq!(m.csp_sent().len(), 1, "a second box on one socket");
    assert_eq!(h.net().act(|m| m.release_message_acks(ANN)), 1);
    says(
        &answered(newer).await,
        &sent_at(Some(START_MS), &[ANN]),
        "the newer",
    );
    h.finish().await;
}

/// A read of the queue that fails by its content stops the queue on this socket, and no row can be named: what waits is answered,
/// and so is what comes later. Nothing hangs on a socket that will not run it.
#[test]
fn contract_a_queue_that_stopped_answers_what_waits() {
    support::offline::run_child(
        module_path!(),
        "contract_a_queue_that_stopped_answers_what_waits_child",
    );
}

#[tokio::test]
async fn contract_a_queue_that_stopped_answers_what_waits_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_319).with_peer(ANN).send_open();
    let mut h = harness::start(s);
    {
        let db = h.db();
        let task = store::prepare_outgoing(&db, &message(contact(ANN), LOCAL, T0), NOW, &[])
            .expect("the row");
        db.zone(|tx| {
            tx.execute_batch("PRAGMA ignore_check_constraints = ON")?;
            tx.execute(
                "UPDATE send_task SET origin = 'not-a-origin' WHERE id = ?1",
                params![task],
            )?;
            Ok::<(), kourier_db::Error>(())
        })
        .expect("a row no build can read");
    }
    connect(&mut h).await;
    let first = answered(h.transmit(request(&contact(ANN), LOCAL_2, None)).await).await;
    says(&first, &failed(TRANSIENT), "the queue stopped");
    let second = answered(h.transmit(request(&contact(ANN), LOCAL_3, None)).await).await;
    says(
        &second,
        &failed(TRANSIENT),
        "it stays stopped on this socket",
    );
    assert_eq!(frames(&h), (0, 0, 0), "a frame of a stopped queue");
    h.finish().await;
}

/// THE PLAIN PARTS on a live socket: a `Transmit` is `Failed(unsupported)`, nothing is written and no frame goes, however good the
/// message (`network::the_actor_runs_offline` holds the same with no socket).
#[test]
fn contract_plain_parts_refuse_a_transmit_on_a_live_socket() {
    support::offline::run_child(
        module_path!(),
        "contract_plain_parts_refuse_a_transmit_on_a_live_socket_child",
    );
}

#[tokio::test]
async fn contract_plain_parts_refuse_a_transmit_on_a_live_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_320).with_peer(ANN);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let outcome = answered(h.transmit(request(&contact(ANN), LOCAL, None)).await).await;
    says(&outcome, &failed(ErrorKind::Unsupported), "plain parts");
    h.sync().await;
    assert_eq!(tasks(&h).len(), 0, "a row under plain parts");
    assert_eq!(frames(&h), (0, 0, 0), "a frame under plain parts");
    h.finish().await;
}

/// The door opens `Transmit` and nothing else: a chat stated behind it still cannot be sent in, so the core refuses a new send there
/// as in a real build.
#[test]
fn contract_every_chat_says_not_yet_built_behind_the_door() {
    support::offline::run_child(
        module_path!(),
        "contract_every_chat_says_not_yet_built_behind_the_door_child",
    );
}

#[tokio::test]
async fn contract_every_chat_says_not_yet_built_behind_the_door_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_321).send_open();
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.net().act(|_| {
        phone.reflect_envelope(
            &Author::new(PHONE).contact_create(ContactDelta::create(ANN, [7; 32], START_MS)),
        );
    });
    let envelope = h.envelope().await;
    let features: Vec<_> = envelope
        .events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ConversationUpsert(patch) => patch.features.as_ref(),
            _ => None,
        })
        .collect();
    assert_eq!(features.len(), 1, "the contact's one chat");
    assert_eq!(features[0].cannot_send, Some(CannotSend::NotYetBuilt));
    envelope.ack.expect("a durable envelope").complete(Ok(()));
    h.finish().await;
}

// ---- S6: the clear ---------------------------------------------------------------------------------------------------------------

/// The core's clear of `conv`.
fn clear_command(cmd: i64, conv: ConvKey, remove: bool, before_ms: Option<i64>) -> Command {
    Command::ClearConversation {
        cmd: CmdId(cmd),
        conv,
        newest: Vec::new(),
        newest_non_expiring: Vec::new(),
        remove,
        before_ms,
        system_before_ms: None,
    }
}

/// The core reads its confirmation of `cmd`, the one event of its row.
async fn confirmed(h: &mut H, cmd: i64) {
    let envelope = h.envelope().await;
    assert!(
        matches!(
            envelope.events.as_slice(),
            [BackendEvent::CommandPersisted { cmd: got }] if *got == CmdId(cmd)
        ),
        "the clear's row is not exactly its confirmation"
    );
    envelope.ack.expect("a durable envelope").complete(Ok(()));
}

/// The row of `local` with `to`'s text, at its zones: `reflected` if the reflect's zone ran.
fn seeded(db: &ZoneDb, to: To, local: &str, created_at: i64, reflected: bool) -> i64 {
    let task =
        store::prepare_outgoing(db, &message(to, local, created_at), NOW, &[]).expect("a row");
    if reflected {
        store::mark_send_reflected(db, task).expect("its reflect's zone");
    }
    task
}

/// R5. The chat's unsent rows at or below `before_ms` go with their receivers and the rest stays, each by its own reason: an ATTACHED
/// row (in flight, past its point of no return), an `acked` row (no body, finishes by itself), a `done` row (the ledger's), a row newer
/// than the bound, another chat's row, a job. A later `Transmit` of a row that went prepares anew under the same id.
#[test]
fn clear_takes_the_chats_unsent_rows_and_keeps_the_rest() {
    support::offline::run_child(
        module_path!(),
        "clear_takes_the_chats_unsent_rows_and_keeps_the_rest_child",
    );
}

#[tokio::test]
async fn clear_takes_the_chats_unsent_rows_and_keeps_the_rest_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_330).with_peer(ANN).with_peer(BOB).send_open();
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut h = harness::start(s);
    let done_at = T0 - 100;
    let (prepared, taken_reflected) = {
        let db = h.db();
        // The acked row runs first and its update's ack stays away: nothing after it moves.
        let acked = acked_row(&h);
        assert_eq!(row_of(&h, LOCAL).id, acked);
        let prepared = seeded(&db, contact(ANN), LOCAL_2, T0, false);
        let taken_reflected = seeded(&db, contact(ANN), LOCAL_3, T0 - 1, true);
        seeded(&db, contact(ANN), LOCAL_4, T0 + 1, false);
        seeded(&db, contact(BOB), LOCAL_5, T0, false);
        // The attempt's row.
        seeded(&db, contact(ANN), LOCAL_6, T0, false);
        let job = text_job(contact(ANN), TEXT);
        db.zone(|tx| store::enqueue_job(tx, &job, NOW))
            .expect("a job");
        // A row at its end, the ledger's.
        let ended = store::prepare_outgoing(
            &db,
            &message(contact(ANN), "6162636465666768", done_at),
            NOW,
            &[],
        )
        .expect("a row");
        store::mark_send_reflected(&db, ended).expect("its reflect's zone");
        let row = db
            .read(|c: &Connection| store::send_task(c, ended))
            .expect("the row")
            .expect("the row");
        let (envelope, nonce) = db
            .read(|c: &Connection| store::seal_sent_update(c, &row))
            .expect("the update");
        let update = SealedUpdate {
            envelope: &envelope,
            nonce,
        };
        assert_eq!(
            store::mark_send_acks(&db, ended, &[ANN], Some(update), NOW),
            Ok(SendEnd::Acked)
        );
        assert_eq!(store::mark_send_done(&db, ended, 1, NOW), Ok(SendEnd::Done));
        (prepared, taken_reflected)
    };
    connect(&mut h).await;
    let conn = h.conn();
    wait_reflect_held(&h, conn, 1).await;
    let waiting = h.transmit(request(&contact(ANN), LOCAL_6, None)).await;
    h.sync().await;
    let before = h.file_rows("send_receiver");

    h.send(clear_command(51, conv_key(&contact(ANN)), false, Some(T0)))
        .await;
    confirmed(&mut h, 51).await;

    let left: Vec<(Option<String>, SendState)> =
        tasks(&h).into_iter().map(|t| (t.local, t.state)).collect();
    assert_eq!(
        left,
        [
            (Some(LOCAL.to_owned()), SendState::Acked),
            (Some(LOCAL_4.to_owned()), SendState::Prepared),
            (Some(LOCAL_5.to_owned()), SendState::Prepared),
            (Some(LOCAL_6.to_owned()), SendState::Prepared),
            (None, SendState::Prepared),
            (Some("6162636465666768".to_owned()), SendState::Done),
        ],
        "what a clear leaves"
    );
    assert_eq!(
        h.file_rows("send_receiver"),
        before - 2,
        "the receivers went with their rows"
    );
    for task in [prepared, taken_reflected] {
        assert_eq!(
            receivers_of(&h, task).len(),
            0,
            "the sealed box of a row that went"
        );
    }

    // Taken and asked for again: prepared anew under the same id, a row with a higher number.
    let again = h.transmit(request(&contact(ANN), LOCAL_2, None)).await;
    h.sync().await;
    let anew = row_of(&h, LOCAL_2);
    assert!(anew.id > prepared, "the row came back under its old number");
    assert_eq!(anew.message_id, 0x1817_1615_1413_1211, "the same id");
    drop((waiting, again));
    h.finish().await;
}

/// `before_ms = None` takes nothing, whatever `remove` says (the clear removed no message a person sent), and is still confirmed; a
/// group's rows go by its key as a 1:1 chat's do, and `remove` changes nothing.
#[test]
fn clear_with_no_time_takes_nothing_and_a_groups_rows_go_by_its_key() {
    support::offline::run_child(
        module_path!(),
        "clear_with_no_time_takes_nothing_and_a_groups_rows_go_by_its_key_child",
    );
}

#[tokio::test]
async fn clear_with_no_time_takes_nothing_and_a_groups_rows_go_by_its_key_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(7_331)
        .with_members(CRE, GROUP, &[ANN])
        .with_members(CRE, GROUP + 1, &[ANN])
        .send_open();
    let mut h = harness::start(s);
    {
        let db = h.db();
        seeded(&db, group(CRE, GROUP), LOCAL, T0, false);
        seeded(&db, group(CRE, GROUP), LOCAL_2, T0, true);
        seeded(&db, group(CRE, GROUP + 1), LOCAL_3, T0, false);
    }
    let conv = conv_key(&group(CRE, GROUP));
    for (cmd, remove) in [(61, false), (62, true)] {
        h.send(clear_command(cmd, conv.clone(), remove, None)).await;
        confirmed(&mut h, cmd).await;
        assert_eq!(tasks(&h).len(), 3, "a clear with no time took a row");
    }
    h.send(clear_command(63, conv, true, Some(T0))).await;
    confirmed(&mut h, 63).await;
    let left: Vec<Option<String>> = tasks(&h).into_iter().map(|t| t.local).collect();
    assert_eq!(left, [Some(LOCAL_3.to_owned())], "the other group's row");
    h.finish().await;
}
