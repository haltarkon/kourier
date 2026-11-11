//! Ported from Threema Desktop `apps/desktop/src/test/mocha/common/network/protocol/task/csp/outgoing-csp-messages.spec.ts` at `93a6615c` (`TDS` below).
//!
//! TH6a S7. THE DESKTOP SPEC OF THE OUTGOING MESSAGE STEPS AS A SEQUENCE ORACLE. Desktop's `OutgoingCspMessagesTask` runs
//! against a scripted network and asserts, in order, what the task writes (a reflect, the boxes with their acks, the
//! reflect of the update) and what a receiver and the device group would open. Here the SAME situations go through the
//! real actor over the offline fake: the order of the frames is read from the fake's trace, the boxes are opened by the
//! fake's peer (`world.peer(id)`, the spec's receive rules on the fake's own crypto) and the reflections by the fake's
//! phone (`take_reflected` + `open_envelope`), and the fake's builders write every expected body (rule 9). What is
//! asserted is the wire, not Desktop's model calls (`services.model.*`, `TestHandle`). Behaviour is ported, not code: no
//! Desktop code and no literal datum is copied (names, IDs and texts are synthetic). A case that fails against Kourier is
//! reported, never bent.
//!
//! HOW A CASE REACHES THE ACTOR. A text goes through `Command::Transmit` (the core's door; the parts open the send core's
//! test door with `Scene::send_open`); a receipt and a group control are jobs, and their BODIES are written by the fake's
//! builders (`body::delivery_receipt`, `body::group_creator_container`, `body::group_member_container`) and handed to the
//! door as bytes (critic L9: the send core has no writer for them in TH6a). A job's reply is the row's end (the row is
//! gone), a `Transmit`'s is `Sent`.
//!
//! WHERE KOURIER DIFFERS ON PURPOSE (each case's doc says which applies):
//! - NO PROFILE-PICTURE MESSAGES until TH8. Desktop's every send reflects, and sends to every receiver, a
//!   `CONTACT_SET_PROFILE_PICTURE` beside the message (step 2.1.5 of the Bundled Messages Send Steps; its expectations
//!   count N + 1 reflected messages and a second box per receiver). Kourier has no picture to distribute yet: ONE
//!   `OutgoingMessage` reflect, ONE box per receiver.
//! - THE ENVELOPE VERSION is V0_3 where Desktop writes V0_2 (`TD/.../protocol/manager.ts:345`): every reflection is
//!   opened and its protocol version asserted.
//! - THE FLAGS come from `props` (the spec's `Flags` list of the type: a text 0x01, a control 0x00), where Desktop's
//!   caller hands the task `CspMessageFlags`.
//! - THE ORDER OF THE BOXES is Kourier's: the receivers sorted by ID (S4; a Gateway ID's `*` sorts before a letter or a
//!   digit). Desktop sends in its database's order with the creator last ("this expects that the messages were sent in a
//!   synchronous manner in the order of the users in the database"); the spec fixes none. The expected receivers are
//!   written out literally in Kourier's order.
//! - A TEXT ALWAYS DISTRIBUTES THE NICKNAME (`Props`: a text is `Yes`), so Desktop's "no nickname when distribution is off"
//!   (`TDS:537`, a text with `allowUserProfileDistribution: false`) is the same wire effect from a type that does not
//!   distribute: a delivery receipt with `distributes: false`. A receipt has no `Sent` update, where Desktop's text ends
//!   with one.
//! - THE UPDATE: Desktop's group control and leave cases expect none, and so does Kourier (`props`: `reflect_sent_update`
//!   is true for the conversation messages alone).
//!
//! EVERY DESKTOP CASE OF THE SPEC, with its status (P = ported here, S = left for a later step): 14 of the 19 ported.
//!
//! `TDS`, `describe('nickname is')`
//! - :531 sent encrypted, receiver no Gateway ID ............ P `a_nickname_goes_encrypted_to_a_receiver_that_is_no_gateway_id`
//! - :534 sent encrypted and legacy to a Gateway ID ......... P `a_nickname_goes_encrypted_and_legacy_to_a_gateway_id`
//! - :537 not sent when distribution is off ................. P (a receipt job) `no_nickname_goes_when_the_type_does_not_distribute`
//!
//! `TDS`, `describe('1:1 contact messaging')`
//! - :544 send and reflect a text ........................... P `a_text_to_a_contact_is_reflected_boxed_and_updated`
//! - :619 send and reflect an edit .......................... S TH7 (the kind is TH7's; the core's `edit` is off)
//! - :759 send and reflect a delete ......................... S TH7 (the same)
//!
//! `TDS`, `describe('group messaging')`
//! - :992 a message to all group members .................... P `a_group_text_goes_to_every_member`
//! - :1037 no members: still reflected and updated .......... P `a_group_without_members_still_reflects_and_updates`
//! - :1054 also to the creator, when it is not us ........... P `a_group_text_goes_to_the_creator_too`
//!
//! `TDS`, `describe('gateway group messaging')`
//! - :1108 creator no Gateway ID, name under the cloud ...... P `a_creator_that_is_no_gateway_id_gets_the_text_whatever_the_name`
//! - :1155 Gateway creator, name starts with the cloud ...... P `a_gateway_creator_gets_the_text_when_the_name_starts_with_the_cloud`
//! - :1199 Gateway creator, name without the cloud .......... P `a_gateway_creator_gets_no_text_without_the_cloud`
//! - :1237 a group leave reaches a Gateway creator .......... P (a job of `GROUP_LEAVE`) `a_group_leave_reaches_a_gateway_creator`
//!
//! `TDS`, `describe('contact blocking')`
//! - :1277 no 1:1 message to a blocked contact .............. P `a_text_to_a_blocked_contact_is_reflected_and_updated_with_no_box`
//! - :1343 no group message to a blocked member ............. P `a_group_text_skips_a_blocked_member`
//! - :1438 a group control goes to a blocked member ......... P (a job of `GROUP_NAME`) `a_group_control_goes_to_a_blocked_member`
//!
//! `TDS`, `describe('bundled sending')`
//! - :1531 profile distribution, one user ................... S TH8 (bundling exists for profile pictures; Kourier has one message per task)
//! - :1649 profile distribution, several users .............. S TH8 (the same)
//! - :1771 a default and a legacy encoder per receiver ...... S TH7 (specifics: the legacy reaction; `Outgoing.specific` has a unit test)
//!
//! Each test is a parent/child `unshare -rn` pair like its neighbours; both clocks stay where they are (no case waits for
//! a deadline).

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::rusqlite::params;
use kourier_model::{Body, ConvKey, UserId};
use kourier_threema::store::{self, NewBody, NewJob, To};
use libthreema::protobuf::common::{GroupIdentity, Identities};
use libthreema::protobuf::d2d::envelope::Content;
use libthreema::protobuf::d2d::outgoing_message_update::{Sent as SentMark, Update, update};
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;

use super::harness::{self, DEVICE, H, ME, Scene, scene};
use super::reflect::{connect, phone_of};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, MsgId, ty};
use crate::support::mock::peer::Opened;
use crate::support::mock::phone::{Phone, open_envelope};

/// The users of these cases and the Gateway ID (synthetic). `*` sorts before a digit and a letter.
const ONE: &str = "DSKUSER1";
const TWO: &str = "DSKUSER2";
const GATE: &str = "*DSKGATE";
/// The group of these cases.
const GROUP: u64 = 0x1122_3344_5566_7788;
/// The nickname of the user, and the text of the messages (synthetic).
const NICK: &str = "Quill";
const TEXT: &str = "synthetic desktop text";
/// The id of a `Transmit`'s message (8 wire bytes; the contract's `local` is their text, the message's id the
/// little-endian `u64`), and its time in ms.
const ID: MsgId = [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18];
const CREATED: u64 = 1_761_600_000_456;
/// The flags of a text: send a push.
const PUSH: u8 = 0x01;
/// A message a receipt names (synthetic).
const SUBJECT: MsgId = [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38];
/// The upper bound of a wait (never a measure).
const WALL: Duration = Duration::from_secs(10);

/// The two frames of a reflect and its ack, and of a box and its ack, as the fake's trace names them.
const REFLECT: [&str; 2] = ["reflect", "reflect-ack"];
const BOX: [&str; 2] = ["csp-outgoing-message", "csp-outgoing-message-ack"];

/// The send path's frames of a message with `boxes` receivers: the reflect, a box and its ack each, and, when the type owes
/// one, the update's reflect.
fn order(boxes: usize, update: bool) -> Vec<&'static str> {
    let mut frames = REFLECT.to_vec();
    for _ in 0..boxes {
        frames.extend(BOX);
    }
    if update {
        frames.extend(REFLECT);
    }
    frames
}

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
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

/// The phone's name of a conversation.
fn conversation(to: &To) -> d2d::ConversationId {
    let id = match to {
        To::Contact(id) => d2d::conversation_id::Id::Contact(id.clone()),
        To::Group { creator, group_id } => d2d::conversation_id::Id::Group(GroupIdentity {
            group_id: *group_id,
            creator_identity: creator.clone(),
        }),
    };
    d2d::ConversationId { id: Some(id) }
}

/// A `Transmit` of the plain text.
fn request(to: &To) -> OutgoingRequest {
    OutgoingRequest {
        conv: conv_key(to),
        local: Arc::from(body::id_hex(ID).as_str()),
        sent_at: i64::try_from(CREATED).expect("ms fit i64"),
        kind: OutgoingKind::Message {
            body: Body::text(TEXT),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
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

/// The settings of a user who blocked these contacts.
fn blocking(ids: &[&str]) -> d2d_sync::Settings {
    d2d_sync::Settings {
        blocked_identities: Some(Identities {
            identities: ids.iter().map(|i| (*i).to_owned()).collect(),
        }),
        ..Default::default()
    }
}

/// What one run put on the wire.
struct Seen {
    /// The send path's frames in the fake's order, by the names of its trace.
    order: Vec<String>,
    /// The receivers of the boxes the chat server kept, in the order the boxes came.
    receivers: Vec<String>,
    /// The flags of every reflect the mediator saw (0: none is ephemeral, so each is acked).
    reflect_flags: Vec<u16>,
    /// What the phone opened of the reflections, in order.
    reflected: Vec<d2d::Envelope>,
}

/// What a box is expected to hold when its peer opens it.
struct Message<'a> {
    ty: u8,
    flags: u8,
    id: u64,
    created_ms: u64,
    body: &'a [u8],
}

/// What the phone is expected to open: the message and, when the type owes one, the `Sent` update after it.
struct Expect<'a> {
    conv: d2d::ConversationId,
    ty: u8,
    /// The message's id when the case knows it (a `Transmit`'s); a job's is drawn by the door.
    id: Option<u64>,
    created_ms: u64,
    body: &'a [u8],
    nonces: Vec<[u8; 24]>,
    update: bool,
}

/// The phone, the actor on the fake and the frames the case has seen.
struct W {
    h: H,
    phone: Phone,
}

impl W {
    /// Starts the actor over the scene's file beside a phone that leads, and waits until it is online.
    async fn new(s: Scene) -> Self {
        let phone = phone_of(&s);
        let mut h = harness::start(s);
        connect(&mut h).await;
        Self { h, phone }
    }

    /// A `Transmit` of the plain text to `to`, answered `Sent` (its row is `done`: every frame went and was acked).
    async fn transmit(&mut self, to: &To) -> TransmitOutcome {
        let answer = self.h.transmit(request(to)).await;
        let outcome = match tokio::time::timeout(WALL, answer).await {
            Err(_) => panic!("the Transmit was not answered in time"),
            Ok(Err(_)) => panic!("the actor dropped the Transmit's reply"),
            Ok(Ok(outcome)) => outcome,
        };
        assert!(
            matches!(outcome, TransmitOutcome::Sent { .. }),
            "the Transmit was not answered Sent"
        );
        outcome
    }

    /// Waits (a wall BOUND) until the queue holds no row: a job's row goes with its last frame.
    async fn queue_empty(&self) {
        let db = self.h.db();
        let emptied = tokio::time::timeout(WALL, async {
            while !db.read(store::send_tasks).expect("the tasks").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            emptied.is_ok(),
            "the queue never emptied; the fake's rules: {:?}",
            self.h.s.world.violations().rules()
        );
    }

    /// The group's stored name, as the phone's sync last said it.
    fn rename(&self, creator: &str, group_id: u64, name: &str) {
        let id = i64::from_ne_bytes(group_id.to_ne_bytes());
        self.h
            .db()
            .zone(|tx| {
                let sync: Vec<u8> = tx.query_row(
                    "SELECT sync FROM grp WHERE creator = ?1 AND group_id = ?2",
                    params![creator, id],
                    |r| r.get(0),
                )?;
                let mut group = d2d_sync::Group::decode(sync.as_slice()).expect("a group");
                group.name = Some(name.to_owned());
                tx.execute(
                    "UPDATE grp SET sync = ?3 WHERE creator = ?1 AND group_id = ?2",
                    params![creator, id, group.encode_to_vec()],
                )?;
                Ok::<(), kourier_db::Error>(())
            })
            .expect("rename a group");
    }

    /// Everything the wire has seen so far: the frames of the send path, the boxes' receivers and what the phone opened.
    fn seen(&mut self) -> Seen {
        let conn = self.h.conn().0.to_string();
        let order = self
            .h
            .s
            .world
            .trace()
            .lines()
            .iter()
            .filter_map(|line| {
                let mut parts = line.split(' ').skip(1);
                let (on, _, name) = (parts.next()?, parts.next()?, parts.next()?);
                (on == conn
                    && matches!(
                        name,
                        "reflect"
                            | "reflect-ack"
                            | "csp-outgoing-message"
                            | "csp-outgoing-message-ack"
                    ))
                .then(|| name.to_owned())
            })
            .collect();
        let mediator = self.h.s.world.mediator();
        let receivers = mediator
            .csp_sent()
            .iter()
            .map(|one| String::from_utf8_lossy(&one.receiver).into_owned())
            .collect();
        let reflect_flags = mediator.reflects().iter().map(|r| r.flags).collect();
        let reflected = self
            .phone
            .take_reflected()
            .iter()
            .map(|(_, sealed)| {
                open_envelope(&self.h.s.keys, sealed).expect("the phone opens the envelope")
            })
            .collect();
        Seen {
            order,
            receivers,
            reflect_flags,
            reflected,
        }
    }

    /// The peer `id` opened exactly one message: from this device, with the expected fields and under no discard.
    #[track_caller]
    fn opens(&mut self, id: &str, expected: &Message<'_>) -> Opened {
        let mut opened = self.h.s.world.peer(id).drain();
        assert_eq!(opened.len(), 1, "one message at the peer");
        let one = opened.remove(0);
        assert!(one.sender == ME, "the sender is not this device's ID");
        assert!(
            one.message_id == expected.id,
            "the peer opened another message id"
        );
        assert_eq!(one.created_at_ms, expected.created_ms, "created-at");
        assert_eq!(
            u64::from(one.created_at_s),
            expected.created_ms / 1000,
            "the head's created-at"
        );
        assert_eq!(one.flags, expected.flags, "the flags");
        assert_eq!(one.ty, expected.ty, "the container's type");
        assert_eq!(one.discard, None, "a discard");
        assert!(one.body == expected.body, "the peer opened another body");
        one
    }

    /// The peer `id` was sent nothing.
    #[track_caller]
    fn none_at(&mut self, id: &str) {
        assert!(
            self.h.s.world.peer(id).drain().is_empty(),
            "a message went to a receiver that must get none"
        );
    }
}

/// The id of the message the phone's first reflection carries.
#[track_caller]
fn message_id_of(reflected: &[d2d::Envelope]) -> u64 {
    match reflected.first().and_then(|e| e.content.as_ref()) {
        Some(Content::OutgoingMessage(message)) => message.message_id,
        _ => panic!("the first reflection is no OutgoingMessage"),
    }
}

/// Desktop's `getExpectedD2dOutgoingReflectedMessage` and `getExpectedD2dOutgoingReflectedMessageUpdate`, on what the phone
/// opened: the first reflection is the message (its id, time, type, conversation, body and the boxes' nonces), the second,
/// when the type owes one, the `Sent` update of the same message and conversation. Every envelope is this device's, in
/// protocol version V0_3. The message's id.
#[track_caller]
fn the_phone(reflected: &[d2d::Envelope], expected: &Expect<'_>) -> u64 {
    let count = if expected.update { 2 } else { 1 };
    assert_eq!(reflected.len(), count, "the reflections the phone opened");
    for envelope in reflected {
        assert_eq!(envelope.device_id, DEVICE, "the reflection's device");
        assert_eq!(
            envelope.protocol_version,
            d2d::ProtocolVersion::V03 as u32,
            "the version the reflections carry"
        );
    }
    let Some(Content::OutgoingMessage(message)) = &reflected[0].content else {
        panic!("the first reflection is no OutgoingMessage");
    };
    assert_eq!(message.created_at, expected.created_ms, "createdAt");
    assert_eq!(message.r#type, i32::from(expected.ty), "the type");
    if let Some(id) = expected.id {
        assert!(message.message_id == id, "the reflected message id");
    }
    assert!(
        message.conversation.as_ref() == Some(&expected.conv),
        "the reflected conversation"
    );
    assert!(message.body == expected.body, "the reflected body");
    let mut listed = message.nonces.clone();
    listed.sort();
    let mut boxes: Vec<Vec<u8>> = expected.nonces.iter().map(|n| n.to_vec()).collect();
    boxes.sort();
    assert!(
        listed == boxes,
        "the reflect does not list the boxes' nonces"
    );
    if expected.update {
        let Some(Content::OutgoingMessageUpdate(update)) = &reflected[1].content else {
            panic!("the second reflection is no OutgoingMessageUpdate");
        };
        assert!(
            update.updates
                == [Update {
                    conversation: Some(expected.conv.clone()),
                    message_id: message.message_id,
                    update: Some(update::Update::Sent(SentMark {})),
                }],
            "the update is not the Sent of this message and conversation"
        );
    }
    message.message_id
}

// ---- describe('nickname is') ---------------------------------------------------------------------------------------------

/// Desktop's `runNicknameTest` for a text: the user's nickname is NICK and `receiver` is a stored contact. The frames are
/// the reflect, the box and the update; the peer opens the one text. Returns the run and what the peer opened.
async fn nickname_text(seed: u64, receiver: &str, nickname: Option<&str>) -> (W, Opened) {
    let mut s = scene(seed).with_peer(receiver);
    if let Some(nickname) = nickname {
        s = s.with_nickname(nickname);
    }
    let mut w = W::new(s).await;
    let to = contact(receiver);
    w.transmit(&to).await;
    let seen = w.seen();
    assert_eq!(seen.order, order(1, true), "the order on the wire");
    assert_eq!(seen.reflect_flags, [0, 0], "a reflect is never ephemeral");
    assert!(
        seen.receivers == [receiver],
        "the box went to another receiver"
    );
    let text = body::text(TEXT);
    let one = w.opens(
        receiver,
        &Message {
            ty: ty::TEXT,
            flags: PUSH,
            id: body::id_u64(ID),
            created_ms: CREATED,
            body: &text,
        },
    );
    the_phone(
        &seen.reflected,
        &Expect {
            conv: conversation(&to),
            ty: ty::TEXT,
            id: Some(body::id_u64(ID)),
            created_ms: CREATED,
            body: &text,
            nonces: vec![one.nonce],
            update: true,
        },
    );
    (w, one)
}

#[test]
fn a_nickname_goes_encrypted_to_a_receiver_that_is_no_gateway_id() {
    support::offline::run_child(
        module_path!(),
        "a_nickname_goes_encrypted_to_a_receiver_that_is_no_gateway_id_child",
    );
}

/// `TDS:531`: the nickname is in the encrypted metadata, and the head's legacy nickname field stays empty (the metadata
/// is the nickname's home since V0.2; the legacy field is for a Gateway ID's sake alone). Same wire as Desktop but one
/// message, no profile picture (TH8).
#[tokio::test]
async fn a_nickname_goes_encrypted_to_a_receiver_that_is_no_gateway_id_child() {
    if !support::offline::in_child() {
        return;
    }
    let (w, one) = nickname_text(9_101, ONE, Some(NICK)).await;
    assert_eq!(
        one.nickname.as_deref(),
        Some(NICK),
        "the metadata's nickname"
    );
    assert!(
        one.bytes[32..64] == [0_u8; 32],
        "the legacy nickname field is not empty"
    );
    w.h.finish().await;
}

#[test]
fn a_nickname_goes_encrypted_and_legacy_to_a_gateway_id() {
    support::offline::run_child(
        module_path!(),
        "a_nickname_goes_encrypted_and_legacy_to_a_gateway_id_child",
    );
}

/// `TDS:534`: to a Gateway ID the nickname goes in the metadata AND in the head's legacy field (a Gateway ID reads the
/// legacy one), as the stored text zero-padded to 32 bytes.
#[tokio::test]
async fn a_nickname_goes_encrypted_and_legacy_to_a_gateway_id_child() {
    if !support::offline::in_child() {
        return;
    }
    let (w, one) = nickname_text(9_102, GATE, Some(NICK)).await;
    assert_eq!(
        one.nickname.as_deref(),
        Some(NICK),
        "the metadata's nickname"
    );
    let mut legacy = [0_u8; 32];
    legacy[..NICK.len()].copy_from_slice(NICK.as_bytes());
    assert!(
        one.bytes[32..64] == legacy,
        "the legacy nickname field is not the nickname"
    );
    w.h.finish().await;
}

#[test]
fn no_nickname_goes_when_the_type_does_not_distribute() {
    support::offline::run_child(
        module_path!(),
        "no_nickname_goes_when_the_type_does_not_distribute_child",
    );
}

/// `TDS:537`: with the user profile distribution off no nickname is in the metadata and the legacy field is empty, for an
/// ordinary receiver and for a Gateway ID. DIFFERS: a Kourier text always distributes (`Props`), so the effect comes from
/// a type that does not: a read receipt (`distributes: false`) to each of the two, as two jobs of the fake's own bytes,
/// each reflect, box and ack in turn. A receipt owes no update, where Desktop's text ends with one.
#[tokio::test]
async fn no_nickname_goes_when_the_type_does_not_distribute_child() {
    if !support::offline::in_child() {
        return;
    }
    let receipt = body::delivery_receipt(2, &[SUBJECT]);
    let s = scene(9_103)
        .with_peer(ONE)
        .with_peer(GATE)
        .with_nickname(NICK)
        .job(raw_job(
            contact(ONE),
            ty::DELIVERY_RECEIPT,
            receipt.clone(),
            false,
        ))
        .job(raw_job(
            contact(GATE),
            ty::DELIVERY_RECEIPT,
            receipt.clone(),
            false,
        ));
    let mut w = W::new(s).await;
    w.queue_empty().await;
    let seen = w.seen();
    assert_eq!(
        seen.order,
        [order(1, false), order(1, false)].concat(),
        "the order on the wire"
    );
    assert!(
        seen.receivers == [ONE, GATE],
        "the boxes went to other receivers or in another order"
    );
    assert_eq!(seen.reflected.len(), 2, "one reflection per receipt");
    for (at, receiver) in [ONE, GATE].into_iter().enumerate() {
        let one_reflection = &seen.reflected[at..=at];
        let id = message_id_of(one_reflection);
        let one = w.opens(
            receiver,
            &Message {
                ty: ty::DELIVERY_RECEIPT,
                flags: 0,
                id,
                created_ms: START_MS,
                body: &receipt,
            },
        );
        assert_eq!(one.nickname, None, "a nickname went with a receipt");
        assert!(
            one.bytes[32..64] == [0_u8; 32],
            "the legacy nickname field is not empty"
        );
        the_phone(
            one_reflection,
            &Expect {
                conv: conversation(&contact(receiver)),
                ty: ty::DELIVERY_RECEIPT,
                id: None,
                created_ms: START_MS,
                body: &receipt,
                nonces: vec![one.nonce],
                update: false,
            },
        );
    }
    w.h.finish().await;
}

// ---- describe('1:1 contact messaging') -----------------------------------------------------------------------------------

#[test]
fn a_text_to_a_contact_is_reflected_boxed_and_updated() {
    support::offline::run_child(
        module_path!(),
        "a_text_to_a_contact_is_reflected_boxed_and_updated_child",
    );
}

/// `TDS:544`: a 1:1 text is reflected first, then boxed to the contact and acked, and the `Sent` update is reflected
/// last; the reflect names the contact, the message's id, time and type, and the update the same conversation and id. No
/// nickname is stored here: the case asserts the order and the openings, not the nickname (`TDS:531` does). DIFFERS: no
/// `CONTACT_SET_PROFILE_PICTURE` (TH8), so one reflected message and one box.
#[tokio::test]
async fn a_text_to_a_contact_is_reflected_boxed_and_updated_child() {
    if !support::offline::in_child() {
        return;
    }
    let (w, _) = nickname_text(9_104, ONE, None).await;
    w.h.finish().await;
}

// ---- describe('group messaging') -----------------------------------------------------------------------------------------

/// A group text for these cases: who is in the group, whom the user blocked and who is expected to get a box.
struct Run<'a> {
    seed: u64,
    creator: &'a str,
    members: &'a [&'a str],
    blocked: &'a [&'a str],
    /// The stored name of the group when the case needs one; the harness's own does not start with the cloud.
    name: Option<&'a str>,
    /// Who gets a box, in the order the boxes go (Kourier's: sorted by ID).
    receivers: &'a [&'a str],
}

/// A `Transmit` of a text to the group of `run`, and everything the wire shows of it: the reflect first, the boxes to
/// exactly `run.receivers` each acked, the update last; what the peers open is the member container the fake builds; no
/// other member of the group is sent anything; the phone opens the message with the boxes' nonces and the update.
async fn group_text(run: &Run<'_>) -> W {
    let mut s = scene(run.seed).with_members(run.creator, GROUP, run.members);
    if !run.blocked.is_empty() {
        s = s.with_settings(blocking(run.blocked));
    }
    let mut w = W::new(s).await;
    if let Some(name) = run.name {
        w.rename(run.creator, GROUP, name);
    }
    let to = group(run.creator, GROUP);
    w.transmit(&to).await;
    let seen = w.seen();
    assert_eq!(
        seen.order,
        order(run.receivers.len(), true),
        "the order on the wire"
    );
    assert_eq!(seen.reflect_flags, [0, 0], "a reflect is never ephemeral");
    assert!(
        seen.receivers == run.receivers,
        "the boxes went to other receivers or in another order"
    );
    let container = body::group_member_container(run.creator, GROUP, &body::text(TEXT));
    let message = Message {
        ty: ty::GROUP_TEXT,
        flags: PUSH,
        id: body::id_u64(ID),
        created_ms: CREATED,
        body: &container,
    };
    let mut opened = Vec::new();
    for receiver in run.receivers {
        opened.push(w.opens(receiver, &message));
    }
    let everybody = run.members.iter().copied().chain([run.creator]);
    for id in everybody.filter(|id| *id != ME && !run.receivers.contains(id)) {
        w.none_at(id);
    }
    the_phone(
        &seen.reflected,
        &Expect {
            conv: conversation(&to),
            ty: ty::GROUP_TEXT,
            id: Some(body::id_u64(ID)),
            created_ms: CREATED,
            body: &container,
            nonces: opened.iter().map(|one| one.nonce).collect(),
            update: true,
        },
    );
    w
}

#[test]
fn a_group_text_goes_to_every_member() {
    support::offline::run_child(module_path!(), "a_group_text_goes_to_every_member_child");
}

/// `TDS:992`: a text in the user's own group goes to every member, each box acked, between the message's reflect and the
/// update that names the group and its creator (the user). DIFFERS: no profile picture to each member (TH8), so one
/// reflected message and one box each.
#[tokio::test]
async fn a_group_text_goes_to_every_member_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_201,
        creator: ME,
        members: &[ONE, TWO],
        blocked: &[],
        name: None,
        receivers: &[ONE, TWO],
    })
    .await;
    w.h.finish().await;
}

#[test]
fn a_group_without_members_still_reflects_and_updates() {
    support::offline::run_child(
        module_path!(),
        "a_group_without_members_still_reflects_and_updates_child",
    );
}

/// `TDS:1037`: a group nobody else is in: the device group still learns of the message (the reflect, then the update),
/// and no box goes to anybody; the reflect lists no nonce.
#[tokio::test]
async fn a_group_without_members_still_reflects_and_updates_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_202,
        creator: ME,
        members: &[],
        blocked: &[],
        name: None,
        receivers: &[],
    })
    .await;
    w.h.finish().await;
}

#[test]
fn a_group_text_goes_to_the_creator_too() {
    support::offline::run_child(module_path!(), "a_group_text_goes_to_the_creator_too_child");
}

/// `TDS:1054`: the creator of the group is another user and is not in the member list; it gets the text as well, and the
/// update names it as the group's creator. DIFFERS: the order of the boxes is Kourier's, the receivers sorted by ID (the
/// creator first here), where Desktop sends to the creator last.
#[tokio::test]
async fn a_group_text_goes_to_the_creator_too_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_203,
        creator: ONE,
        members: &[TWO],
        blocked: &[],
        name: None,
        receivers: &[ONE, TWO],
    })
    .await;
    w.h.finish().await;
}

// ---- describe('gateway group messaging') ---------------------------------------------------------------------------------

/// A name that starts with the cloud emoji, as a Gateway group that captures every message carries it (synthetic).
const CLOUD_NAME: &str = "\u{2601}\u{fe0f} synthetic captured group";
/// A name that does not.
const PLAIN_NAME: &str = "synthetic plain group";

#[test]
fn a_creator_that_is_no_gateway_id_gets_the_text_whatever_the_name() {
    support::offline::run_child(
        module_path!(),
        "a_creator_that_is_no_gateway_id_gets_the_text_whatever_the_name_child",
    );
}

/// `TDS:1108`: the creator is an ordinary user and the group's name starts with the cloud: it gets the text, as it would
/// under any name (the name matters for a Gateway creator alone). DIFFERS: the order of the boxes (sorted by ID).
#[tokio::test]
async fn a_creator_that_is_no_gateway_id_gets_the_text_whatever_the_name_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_204,
        creator: ONE,
        members: &[TWO],
        blocked: &[],
        name: Some(CLOUD_NAME),
        receivers: &[ONE, TWO],
    })
    .await;
    w.h.finish().await;
}

#[test]
fn a_gateway_creator_gets_the_text_when_the_name_starts_with_the_cloud() {
    support::offline::run_child(
        module_path!(),
        "a_gateway_creator_gets_the_text_when_the_name_starts_with_the_cloud_child",
    );
}

/// `TDS:1155`: the creator is a Gateway ID and the group's name starts with the cloud (the Gateway captures the group's
/// messages): it gets the text, with the member. The update names the Gateway ID as the creator. DIFFERS: the order of the
/// boxes (the Gateway ID's `*` sorts first).
#[tokio::test]
async fn a_gateway_creator_gets_the_text_when_the_name_starts_with_the_cloud_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_205,
        creator: GATE,
        members: &[TWO],
        blocked: &[],
        name: Some(CLOUD_NAME),
        receivers: &[GATE, TWO],
    })
    .await;
    w.h.finish().await;
}

#[test]
fn a_gateway_creator_gets_no_text_without_the_cloud() {
    support::offline::run_child(
        module_path!(),
        "a_gateway_creator_gets_no_text_without_the_cloud_child",
    );
}

/// `TDS:1199`: the creator is a Gateway ID and the name does not start with the cloud: a text does NOT go to it; the
/// member gets it, and the update still names the Gateway ID as the creator.
#[tokio::test]
async fn a_gateway_creator_gets_no_text_without_the_cloud_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_206,
        creator: GATE,
        members: &[TWO],
        blocked: &[],
        name: Some(PLAIN_NAME),
        receivers: &[TWO],
    })
    .await;
    w.h.finish().await;
}

/// A group control of the user's, as a job of the fake's own bytes (`container` is its body, the fake's), and everything
/// the wire shows of it: the reflect first, the boxes to exactly `run.receivers` each acked, NO update (a control owes
/// none); the peers open the container the fake built under the flags 0x00; the phone opens the one `OutgoingMessage` with
/// the boxes' nonces. The job is sealed when the file is seeded, so `run.name` is not used: the harness's group name has no
/// cloud.
async fn group_control(run: &Run<'_>, control_ty: u8, container: Vec<u8>) -> W {
    let mut s = scene(run.seed)
        .with_members(run.creator, GROUP, run.members)
        .job(raw_job(
            group(run.creator, GROUP),
            control_ty,
            container.clone(),
            false,
        ));
    if !run.blocked.is_empty() {
        s = s.with_settings(blocking(run.blocked));
    }
    let mut w = W::new(s).await;
    w.queue_empty().await;
    let seen = w.seen();
    assert_eq!(
        seen.order,
        order(run.receivers.len(), false),
        "the order on the wire"
    );
    assert_eq!(seen.reflect_flags, [0], "a reflect is never ephemeral");
    assert!(
        seen.receivers == run.receivers,
        "the boxes went to other receivers or in another order"
    );
    let message = Message {
        ty: control_ty,
        flags: 0,
        id: message_id_of(&seen.reflected),
        created_ms: START_MS,
        body: &container,
    };
    let mut opened = Vec::new();
    for receiver in run.receivers {
        opened.push(w.opens(receiver, &message));
    }
    let everybody = run.members.iter().copied().chain([run.creator]);
    for id in everybody.filter(|id| *id != ME && !run.receivers.contains(id)) {
        w.none_at(id);
    }
    the_phone(
        &seen.reflected,
        &Expect {
            conv: conversation(&group(run.creator, GROUP)),
            ty: control_ty,
            id: None,
            created_ms: START_MS,
            body: &container,
            nonces: opened.iter().map(|one| one.nonce).collect(),
            update: false,
        },
    );
    w
}

#[test]
fn a_group_leave_reaches_a_gateway_creator() {
    support::offline::run_child(
        module_path!(),
        "a_group_leave_reaches_a_gateway_creator_child",
    );
}

/// `TDS:1237`: a group leave is sent to the Gateway creator although the name has no cloud (the harness's group name has
/// none; the control is one of the types the creator always gets), beside the member; reflected first, no update. The job is the fake's bytes: a
/// `GROUP_LEAVE` in the member container that names the creator. DIFFERS: the order of the boxes (the Gateway ID first).
#[tokio::test]
async fn a_group_leave_reaches_a_gateway_creator_child() {
    if !support::offline::in_child() {
        return;
    }
    let leave = body::group_member_container(GATE, GROUP, &[]);
    let w = group_control(
        &Run {
            seed: 9_207,
            creator: GATE,
            members: &[TWO],
            blocked: &[],
            name: None,
            receivers: &[GATE, TWO],
        },
        ty::GROUP_LEAVE,
        leave,
    )
    .await;
    w.h.finish().await;
}

// ---- describe('contact blocking') ----------------------------------------------------------------------------------------

#[test]
fn a_text_to_a_blocked_contact_is_reflected_and_updated_with_no_box() {
    support::offline::run_child(
        module_path!(),
        "a_text_to_a_blocked_contact_is_reflected_and_updated_with_no_box_child",
    );
}

/// `TDS:1277`: the user blocked the contact: no box goes, and the device group still learns of the message, the reflect and
/// then the update ("there is no exception for blocked contacts here specified by the protocol"). The reflect lists no
/// nonce.
#[tokio::test]
async fn a_text_to_a_blocked_contact_is_reflected_and_updated_with_no_box_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_301).with_peer(ONE).with_settings(blocking(&[ONE]));
    let mut w = W::new(s).await;
    let to = contact(ONE);
    w.transmit(&to).await;
    let seen = w.seen();
    assert_eq!(seen.order, order(0, true), "the order on the wire");
    assert_eq!(seen.reflect_flags, [0, 0], "a reflect is never ephemeral");
    assert!(seen.receivers.is_empty(), "a box went to a blocked contact");
    w.none_at(ONE);
    let text = body::text(TEXT);
    the_phone(
        &seen.reflected,
        &Expect {
            conv: conversation(&to),
            ty: ty::TEXT,
            id: Some(body::id_u64(ID)),
            created_ms: CREATED,
            body: &text,
            nonces: Vec::new(),
            update: true,
        },
    );
    w.h.finish().await;
}

#[test]
fn a_group_text_skips_a_blocked_member() {
    support::offline::run_child(module_path!(), "a_group_text_skips_a_blocked_member_child");
}

/// `TDS:1343`: a member the user blocked gets no text; the other member does, between the reflect and the update, and
/// the blocked one's peer is sent nothing. DIFFERS: no profile picture (TH8).
#[tokio::test]
async fn a_group_text_skips_a_blocked_member_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = group_text(&Run {
        seed: 9_302,
        creator: ME,
        members: &[ONE, TWO],
        blocked: &[TWO],
        name: None,
        receivers: &[ONE],
    })
    .await;
    w.h.finish().await;
}

#[test]
fn a_group_control_goes_to_a_blocked_member() {
    support::offline::run_child(
        module_path!(),
        "a_group_control_goes_to_a_blocked_member_child",
    );
}

/// `TDS:1438`: a group name change is a control the blocking does not hold back: both members get it, the blocked one
/// too, between the reflect and the end; no update. The job is the fake's bytes: a `GROUP_NAME` in the creator container.
#[tokio::test]
async fn a_group_control_goes_to_a_blocked_member_child() {
    if !support::offline::in_child() {
        return;
    }
    let name = body::group_creator_container(GROUP, "synthetic new name".as_bytes());
    let w = group_control(
        &Run {
            seed: 9_303,
            creator: ME,
            members: &[ONE, TWO],
            blocked: &[TWO],
            name: None,
            receivers: &[ONE, TWO],
        },
        ty::GROUP_NAME,
        name,
    )
    .await;
    w.h.finish().await;
}
