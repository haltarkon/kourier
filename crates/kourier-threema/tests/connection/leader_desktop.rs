//! Ported from Threema Desktop `apps/desktop/src/test/mocha/common/network/protocol/task/csp/incoming-message.spec.ts`, `incoming-delivery-receipt.spec.ts` and `incoming-group-sync-request.spec.ts` at `93a6615c` (`TDS` below).
//!
//! TH6 L7. THE DESKTOP SPECS OF THE INCOMING MESSAGE STEPS AS SEQUENCE ORACLES. Desktop's `IncomingMessageTask` (and the
//! delivery receipt and group sync request tasks) run against a scripted network and a model and assert, in order, what
//! the task writes: a contact reflection, the reflect of the message, the ack to the chat server, a delivery receipt.
//! Here the SAME situations go through the real actor as the LEADER over the offline fake: a peer of the fake
//! (`world.peer(id)`, the spec's send rules on the fake's own crypto) posts the message at the chat server, the actor
//! (phone offline, Kourier promoted) looks the sender up, reflects, acks and sends its jobs; the order of the frames is
//! read from the fake's trace and judged by THE GUARD ORACLE of the harness's `finish` (no ack before its reflect-ack, a
//! discard acked unreflected only when the test declared it), what the phone would see is its follower view
//! (`Phone::view`), what the peers receive is opened by the fake's peers, and what the core's model would hold is
//! the events the core is sent. Every case also reads the count line of its socket (`out_reflect_inbound`,
//! `out_message_ack`, `out_reflect_job`, `out_box_job`, `out_tx`: counts only, equal to the fake's trace) and, for a
//! discard, the RULE the steps named (a DEBUG line). Behaviour is ported, not code: no Desktop code and no literal
//! datum is copied (texts, names and IDs are synthetic). A case that fails against Kourier is reported, never bent.
//!
//! HOW A CASE RUNS. The peers' letters wait at the chat server while the phone is offline (its slot exists, the
//! mediator keeps Kourier's reflections for it); Kourier connects, is promoted, unblocks, takes them in order, and the
//! core side acks what it is sent until the account is `Online` as leader. The receipts and group answers are JOBS of the
//! send queue and go at `Online`: their boxes are opened at the peers. The phone, back as a follower, views the
//! reflections. The case ends with `finish` (the oracle, then the fake's violations).
//!
//! WHERE KOURIER DIFFERS ON PURPOSE (each case's doc says which applies):
//! - THE MODEL IS THE CORE'S. Desktop asserts its own model (`getAllMessages()`, `deliveredAt`, `reactions`,
//!   `model.contacts.getByIdentity`); the backend keeps no message table and the contact lives in `contact` rows, so the
//!   same fact is the event the core gets (a message, an `Edited`, a `Deleted`, a `Reaction`, `Receipts`, a contact
//!   upsert) or the file's rows. A message that names a target Kourier never saw is handed to the core all the same
//!   (the core decides); `:382`'s "only the first timestamp is used" is the core's, the backend states every receipt
//!   with its own time.
//! - GROUP CHANGES WAIT FOR THE PHONE (his TH6-Q1, "They wait for your phone"). Desktop applies a group name from a
//!   blocked creator in a GROUP_SYNC transaction (`:1366`); Kourier HOLDS it: nothing acked, nothing reflected, the
//!   chat server keeps it for the device that leads next. The group-setup cases `:1513`, `:1559`, `:1590` are the
//!   same decision and are not ported (the brief's table).
//! - RECEIPTS ARE COALESCED: Desktop sends one `RECEIVED` receipt per message; Kourier writes ONE receipt job per sender
//!   per ZONE REFLECTED naming up to 30 ids, so the receipts of several messages may be one box or more. The cases
//!   assert the union of the ids, each once and in order, status 1.
//! - THE JOBS ARE THE SEND CORE'S, so each is reflected as an `OutgoingMessage` before its box goes, also an EMPTY
//!   `group-setup` that Desktop sends without a reflection (`{reflect: false}`); the cases count the job reflects with
//!   the job boxes.
//! - THE CREATOR'S ANSWER NEEDS A PICTURE IT CAN SEND. A group whose picture the file does not know to be removed
//!   holds the sync request (`creator-answer-needs-picture`, T-M3; the picture goes with TH8): Desktop's "with a
//!   profile picture" variant of `:167` is HELD here, the "without" variant answers.
//! - A KEPT MESSAGE ALWAYS WRITES ITS CSP NONCE (the pending row holds it; the stage does not read the replay column for
//!   a kept message), so a received or read receipt leaves one too where the spec marks only status 3 and 4: harmless, a
//!   replayed receipt is a repeat of an idempotent state. The cases count the nonce rows.
//! - A DISCARD IS ACKED, COUNTED BY ITS RULE, and its nonce is marked from step 10 on as the type's replay column says
//!   (L3's decision 6); Desktop's tasks write the ack and nothing else. A throttled sync request (`:220`) is a discard
//!   too: acked, where Desktop's task simply writes nothing.
//! - AN UNKNOWN SENDER OF A SYNC REQUEST IS NOT MADE A CONTACT. The pinned spec says "Implicit direct contact creation:
//!   No" for `group-sync-request` (`csp.struct.yml`) and the steps discard the message at 22.4.1 (`csp-sender-unknown`);
//!   Desktop's task, run directly with a `ContactInit`, creates the contact and answers. The `:122` case sends Desktop's
//!   input and asserts the discard.
//!
//! EVERY DESKTOP CASE NAMED BY THE BRIEF (§2.15), with its status: P = ported, 28 cases in 25 tests (the brief's
//! "24" counts `:80` and `:87`, the two states of `:94`, the two of `:122`, the two of `:167`, `:251` with `:259` and
//! `:220` with `:278` each as one).
//!
//! `TDS/incoming-message.spec.ts`
//! - :357  discard a message that seems to come from the user ........ P `a_message_that_seems_to_come_from_the_user_is_discarded_and_acked`
//! - :392  discard a message whose metadata id differs ................ P `a_metadata_id_that_is_not_the_heads_is_discarded_and_acked`
//! - :446  update the nickname of a contact (CONTACT_SYNC) ............ P `a_nickname_is_updated_cleared_and_taken_from_the_metadata_in_a_contact_sync`
//! - :523  create a contact for an unknown contact's text .............. P `an_unknown_senders_contact_is_created_in_a_transaction_before_its_text_is_reflected`
//! - :574  lift GROUP_OR_DELETED to DIRECT on a text ................... P `a_text_lifts_a_groups_contact_to_direct_in_one_contact_sync`
//! - :620  no upgrade for a type that creates no direct contact ........ P `a_receipt_of_a_groups_contact_changes_no_contact`
//! - :653  a text from a known contact ................................. P `a_text_from_a_known_contact_is_reflected_acked_and_answered_with_a_receipt`
//! - :705  a text from an unknown contact .............................. P `a_text_from_an_unknown_contact_reaches_the_core_after_its_contact`
//! - :1144 group texts only from members ............................... P `a_group_text_of_a_non_member_is_discarded_and_asks_the_creator_for_the_group`
//! - :1241 block unknown: no contact, discarded ......................... P `a_text_of_an_unknown_sender_is_discarded_when_unknown_senders_are_blocked`
//! - :1275 a blocked member's group text is discarded ................... P `a_group_text_of_a_blocked_member_is_discarded_and_acked`
//! - :1366 group control from a blocked creator ......................... P (DIFFERS, TH6-Q1: HELD) `a_group_name_of_a_blocked_creator_is_held_for_the_phone`
//! - :1433 a 1:1 text of a blocked contact is discarded ................. P `a_text_of_a_blocked_contact_is_discarded_and_acked`
//! - :1486 a setup from a blocked creator of an unknown group ........... P `a_group_setup_of_a_blocked_creator_for_an_unknown_group_is_discarded_and_acked`
//! - :1682 an edit ....................................................... P `an_edit_follows_its_text_to_the_core_and_is_not_answered`
//! - :1842 a delete ...................................................... P `a_delete_follows_its_text_to_the_core_and_is_not_answered`
//! - :1992 an emoji reaction ............................................. P `a_reaction_follows_its_text_to_the_core_and_is_not_answered`
//! - (:913-1135 files, :1709-2260 the other edit, delete and reaction cases: the shared mapper's, TH5b's ports; :1513, :1559,
//!   :1590 group-setup processing: HELD in TH6, TH6-Q1)
//!
//! `TDS/incoming-delivery-receipt.spec.ts`
//! - :251  received for an outbound message ............................ P `received_and_read_receipts_reach_the_core_at_their_own_times` (with :259)
//! - :259  read for an outbound message ................................ P (the same test)
//! - :267  acknowledge and decline ..................................... P `acknowledge_and_decline_receipts_become_the_contacts_thumbs`
//! - :382  repeated received and read receipts ......................... P `repeated_receipts_are_each_reflected_acked_and_stated`
//! - (:331, :439, :605 the reactions' mapping: TH5b's)
//!
//! `TDS/incoming-group-sync-request.spec.ts`
//! - :80   we are not the creator: ignored ............................. P `a_sync_request_for_a_group_of_another_creator_or_an_unknown_group_is_discarded` (with :87)
//! - :87   the group cannot be found: ignored .......................... P (the same test)
//! - :94   the group is left or kicked: an empty setup ................. P `a_sync_request_for_a_left_or_kicked_group_gets_an_empty_setup`
//! - :122  a non-member, known or unknown: an empty setup ............. P (DIFFERS for the unknown) `a_non_member_gets_an_empty_setup_and_an_unknown_one_is_not_made_a_contact`
//! - :167  a member: setup, name, delete picture (and the picture) ..... P (DIFFERS with a picture: HELD) `a_member_gets_the_groups_setup_name_and_no_picture_and_a_group_with_a_picture_is_held`
//! - :220  a second request within the hour is discarded ............... P `a_second_sync_request_within_the_hour_is_discarded_and_one_after_it_is_answered` (with :278)
//! - :278  one more than an hour later is answered ..................... P (the same test)
//!
//! Each test is a parent/child `unshare -rn` pair like its neighbours.

use kourier_backend_api::{BackendEvent, Command, IncomingMessage};
use kourier_db::rusqlite::params;
use kourier_model::{Body, ConvKey, MsgTarget, ReceiptKind, TargetAuthor, UserId};
use libthreema::protobuf::common;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::transaction_scope::Scope;
use libthreema::protobuf::d2d_sync::contact::AcquaintanceLevel;
use libthreema::protobuf::d2d_sync::settings::UnknownContactPolicy;
use libthreema::protobuf::d2d_sync::{self, group::UserState};
use prost::Message as _;
use tracing::Level;

use super::harness::{self, DEVICE, H, ME, SERVER_GROUP, Scene, scene, traced};
use super::lines::{WALL, at, count_line, counts_of, equals_the_trace, online_as, wait_lines};
use super::reflect::PHONE;
use super::send::through_echoes;
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::GroupDelta;
use crate::support::mock::peer::{Letter, Opened, Peer};
use crate::support::mock::phone::{Phone, View};
use crate::support::mock::txn::{TxnEnd, TxnSeen};
use crate::support::warns::{Line, Warns};

/// The users of these cases (synthetic): stored contacts with real keys, and a sender the file does not hold that the
/// directory knows.
const ONE: &str = "LDSKUSR1";
const TWO: &str = "LDSKUSR2";
const NEW: &str = "LDSKNEW1";
/// The groups of these cases.
const GID: u64 = 0x5a5b_5c5d_5e5f_6061;
const GID2: u64 = 0x6a6b_6c6d_6e6f_7071;
/// D2's words (a message held for the phone) and the DEBUG line of a discard.
const HELD: &str = "a chat server message is held for the phone";
const DISCARDED: &str = "a received message was discarded by the receive steps";

/// The ID of a message as the core names it (its eight wire bytes in hex); the letters' `u64` ids are little-endian.
fn local(id: u64) -> String {
    id.to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The id a body names (eight wire bytes).
fn wire(id: u64) -> [u8; 8] {
    id.to_le_bytes()
}

fn ms(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(0)
}

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
}

/// The spec's group key written out here (creator, a dot, the 16 hex digits of the little-endian bytes).
fn group_key(creator: &str, group_id: u64) -> ConvKey {
    ConvKey::Group(format!("threema:{creator}.{}", local(group_id)).into())
}

/// A message reference in `conv`.
fn target(conv: &ConvKey, id: u64, author: TargetAuthor) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author,
        local: local(id).as_str().into(),
        alternates: Vec::new(),
    }
}

/// The messages among `events`.
fn messages(events: &[BackendEvent]) -> Vec<&IncomingMessage> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// How many contact upserts `events` hold.
fn contact_upserts(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::ContactUpsert(_)))
        .count()
}

/// Where in `events` the first one that `is` stands.
fn position(events: &[BackendEvent], is: impl Fn(&BackendEvent) -> bool) -> Option<usize> {
    events.iter().position(is)
}

/// The settings of a user who blocked these contacts.
fn blocking(ids: &[&str]) -> d2d_sync::Settings {
    d2d_sync::Settings {
        blocked_identities: Some(common::Identities {
            identities: ids.iter().map(|i| (*i).to_owned()).collect(),
        }),
        ..Default::default()
    }
}

/// The settings of a user who blocks every sender that is no contact.
fn block_unknown() -> d2d_sync::Settings {
    d2d_sync::Settings {
        unknown_contact_policy: Some(UnknownContactPolicy::BlockUnknown as i32),
        ..Default::default()
    }
}

/// A group text of `creator`'s group.
fn group_text(id: u64, creator: &str, group_id: u64) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_TEXT,
        &body::group_member_container(creator, group_id, &body::text("synthetic group text")),
    )
}

/// A `group-sync-request` for the user's group `group_id`.
fn sync_request(id: u64, creator: &str, group_id: u64) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_SYNC_REQUEST,
        &body::group_member_container(creator, group_id, &[]),
    )
}

/// What a peer opened, as (type, body), sorted.
fn sorted(opened: &[Opened]) -> Vec<(u8, Vec<u8>)> {
    let mut all: Vec<(u8, Vec<u8>)> = opened.iter().map(|o| (o.ty, o.body.clone())).collect();
    all.sort();
    all
}

/// The group `group_id` of the user, stored with this state of the user and these members.
fn own_group(group_id: u64, members: &[&str], state: UserState) -> d2d_sync::Group {
    GroupDelta::create(ME, group_id, "synthetic group", members, START_MS)
        .user_state(state)
        .build()
}

/// A group of the user whose picture is stated: explicitly removed, or set to a blob.
fn with_picture(mut group: d2d_sync::Group, set: bool) -> d2d_sync::Group {
    group.profile_picture = Some(common::DeltaImage {
        image: Some(if set {
            common::delta_image::Image::Updated(common::Image {
                r#type: 0,
                blob: Some(common::Blob {
                    id: vec![0xb1; 16],
                    ..Default::default()
                }),
            })
        } else {
            common::delta_image::Image::Removed(common::Unit {})
        }),
    });
    group
}

/// The stored group (`creator`, `group_id`) replaced by `group`, as a sync of the phone would have left it.
fn restate_group(h: &H, creator: &str, group_id: u64, group: &d2d_sync::Group) {
    h.db()
        .zone(|tx| {
            let n = tx.execute(
                "UPDATE grp SET sync = ?1 WHERE creator = ?2 AND group_id = ?3",
                params![
                    group.encode_to_vec(),
                    creator,
                    i64::from_ne_bytes(group_id.to_ne_bytes())
                ],
            )?;
            assert_eq!(n, 1, "the group was not stored");
            Ok::<(), kourier_db::Error>(())
        })
        .expect("restate the group");
}

/// Waits (a wall BOUND) until `conn` sent `n` CSP acks of incoming messages, and not more.
async fn wait_acks(h: &H, conn: ConnId, n: usize) {
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-incoming-message-ack") >= n,
                WALL
            )
            .await,
        "{} of {n} acks came",
        traced(world, conn, "in", "csp-incoming-message-ack")
    );
    assert_eq!(
        traced(world, conn, "in", "csp-incoming-message-ack"),
        n,
        "more acks than expected"
    );
}

/// Waits (a wall BOUND) until `peer` was sent `n` boxes (the jobs), and not more; what it opened, in order.
async fn boxes_for(h: &H, peer: &mut Peer, n: usize) -> Vec<Opened> {
    let mut got: Vec<Opened> = Vec::new();
    let came = h
        .net()
        .wait(
            || {
                got.extend(peer.drain());
                got.len() >= n
            },
            WALL,
        )
        .await;
    assert!(came, "{} of {n} boxes came", got.len());
    h.sync().await;
    got.extend(peer.drain());
    assert_eq!(got.len(), n, "more boxes than expected");
    got
}

/// Waits (a wall BOUND) until the receipts that reached `peer` name `want` ids in all; those ids in order. Every box is
/// a delivery receipt of `status` (Kourier's receipt jobs say 1, `received`).
async fn receipt_ids(h: &H, peer: &mut Peer, status: u8, want: usize) -> Vec<u64> {
    let mut got: Vec<Opened> = Vec::new();
    let named = |got: &[Opened]| -> usize {
        got.iter()
            .map(|o| o.body.len().saturating_sub(1) / 8)
            .sum::<usize>()
    };
    let came = h
        .net()
        .wait(
            || {
                got.extend(peer.drain());
                named(&got) >= want
            },
            WALL,
        )
        .await;
    assert!(came, "{} of {want} ids were receipted", named(&got));
    h.sync().await;
    got.extend(peer.drain());
    let mut ids = Vec::new();
    for receipt in &got {
        assert_eq!(receipt.ty, ty::DELIVERY_RECEIPT, "a box that is no receipt");
        assert_eq!(receipt.body[0], status, "the receipt's status");
        let (chunks, rest) = receipt.body[1..].as_chunks::<8>();
        assert!(rest.is_empty(), "a receipt body of whole ids");
        ids.extend(chunks.iter().map(|id| u64::from_le_bytes(*id)));
    }
    ids
}

/// The `ContactSync`s the phone viewed, as (create?, identity, nickname, acquaintance level).
fn synced(view: &View) -> Vec<(bool, String, Option<String>, Option<i32>)> {
    view.contacts()
        .iter()
        .map(|c| match &c.action {
            Some(d2d::contact_sync::Action::Create(c)) => {
                let c = c.contact.as_ref().expect("a contact");
                (
                    true,
                    c.identity.clone(),
                    c.nickname.clone(),
                    c.acquaintance_level,
                )
            }
            Some(d2d::contact_sync::Action::Update(u)) => {
                let c = u.contact.as_ref().expect("a contact");
                (
                    false,
                    c.identity.clone(),
                    c.nickname.clone(),
                    c.acquaintance_level,
                )
            }
            None => panic!("a contact sync without an action"),
        })
        .collect()
}

/// What the phone viewed of the INBOUND side (the reflections of the jobs are left out): the content names in order.
fn inbound_kinds(view: &View) -> Vec<&'static str> {
    view.kinds()
        .into_iter()
        .filter(|k| !k.starts_with("outgoing"))
        .collect()
}

/// How many `OutgoingMessage`s the phone viewed (Kourier's jobs, reflected before their boxes).
fn outgoing_reflects(view: &View) -> usize {
    view.kinds()
        .into_iter()
        .filter(|k| *k == "outgoing_message")
        .count()
}

/// Kourier's own transactions on `conn`: (begun, rejected, committed with one reflect released).
fn kourier_transactions(h: &H, conn: ConnId) -> (usize, usize, usize) {
    let seen = h.s.world.mediator().transactions();
    let begun = seen
        .iter()
        .filter(|t| matches!(t, TxnSeen::Begun { conn: c, .. } if *c == conn))
        .count();
    let rejected = seen
        .iter()
        .filter(|t| matches!(t, TxnSeen::Rejected { conn: c, .. } if *c == conn))
        .count();
    let committed = seen
        .iter()
        .filter(|t| {
            matches!(t, TxnSeen::Ended { conn: c, how: TxnEnd::Committed, released: 1, .. } if *c == conn)
        })
        .count();
    (begun, rejected, committed)
}

/// Every scope a client sealed opens with DGTSK as `CONTACT_SYNC`.
fn scopes_are_contact_sync(h: &H) {
    let scopes = h.s.world.transaction_scopes(&h.s.keys);
    assert!(!scopes.is_empty(), "no transaction");
    assert!(
        scopes.iter().all(|s| *s == Some(Scope::ContactSync)),
        "a scope that is not CONTACT_SYNC"
    );
}

/// The rules the receive steps named for the discards of the socket, in order (the DEBUG line's `rule` field).
fn discarded_by(warns: &Warns) -> Vec<String> {
    warns
        .lines()
        .iter()
        .filter(|l| l.message == DISCARDED)
        .filter_map(|l| l.field("rule").map(|r| r.trim_matches('"').to_owned()))
        .collect()
}

fn is_held(line: &Line) -> bool {
    line.message == HELD
}

/// Counts and names only: no INFO-or-above line of the crate names the own ID or one of these.
fn no_id(warns: &Warns, ids: &[&str]) {
    let ours: Vec<Line> = warns
        .lines()
        .into_iter()
        .filter(|l| l.target.starts_with("kourier_threema") && l.level <= Level::INFO)
        .collect();
    assert!(!ours.is_empty(), "nothing of the crate was captured");
    for line in ours {
        let text = line.text();
        for id in ids.iter().chain([&ME]) {
            assert!(!text.contains(id), "an ID in the line {:?}", line.message);
        }
    }
}

/// The leader's frames of a socket from its end line: (messages in, inbound reflects, CSP acks, job reflects, job
/// boxes, transaction frames).
fn frames(end: &Line) -> (u64, u64, u64, u64, u64, u64) {
    let c = counts_of(end);
    (
        c["in_csp_message"],
        c["out_reflect_inbound"],
        c["out_message_ack"],
        c["out_reflect_job"],
        c["out_box_job"],
        c["out_tx"],
    )
}

/// The phone, the captured lines, the actor on the fake, and what the core was sent.
struct Rig {
    h: H,
    phone: Phone,
    warns: Warns,
    conn: ConnId,
    events: Vec<BackendEvent>,
}

impl Rig {
    /// The phone has its slot and is offline; the actor starts, connects, and its first socket stands.
    async fn up(s: Scene) -> Self {
        Self::up_with(s, &[]).await
    }

    /// The same, after the stored groups `restated` were replaced (creator, group id, the group as a sync leaves it).
    async fn up_with(s: Scene, restated: &[(&str, u64, d2d_sync::Group)]) -> Self {
        let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
        phone.connect();
        phone.disconnect();
        let warns = Warns::install_info_and_debug_of("kourier_threema");
        let h = harness::start(s);
        if !restated.is_empty() {
            h.sync().await;
            for (creator, group_id, group) in restated {
                restate_group(&h, creator, *group_id, group);
            }
        }
        h.send(Command::Connect).await;
        let conn = h.wait_connected().await;
        Self {
            h,
            phone,
            warns,
            conn,
            events: Vec::new(),
        }
    }

    /// Waits until `acks` CSP acks went and `held` messages are held, then until the core has what was shipped and the
    /// account is `Online` as leader; nothing else was acked or held.
    async fn served(&mut self, acks: usize, held: usize) {
        if acks > 0 {
            wait_acks(&self.h, self.conn, acks).await;
        }
        if held > 0 {
            wait_lines(&self.warns, held, is_held).await;
        }
        let events = self
            .h
            .ack_until("Online as leader", online_as("leader"))
            .await;
        self.events.extend(events);
        self.h.sync().await;
        assert_eq!(
            traced(&self.h.s.world, self.conn, "in", "csp-incoming-message-ack"),
            acks,
            "the CSP acks"
        );
        let held_now = self.warns.lines().iter().filter(|l| is_held(l)).count();
        assert_eq!(held_now, held, "the held messages");
    }

    /// The socket ends (the server's 4000): its count line, equal to the fake's trace.
    async fn end(&mut self) -> Line {
        assert!(
            self.h.net().close_device(DEVICE, 4000),
            "no socket to close"
        );
        let end = wait_lines(&self.warns, 1, count_line("end")).await;
        equals_the_trace(&self.h, self.conn, &end[0]);
        end[0].clone()
    }

    /// The phone comes back as a follower and views what was reflected.
    fn viewed(&mut self) -> View {
        self.phone.connect();
        self.phone.view().clone()
    }

    fn csp_queue(&self) -> usize {
        self.h.s.world.mediator().csp_queue_len(ME)
    }

    fn reflects(&self) -> usize {
        self.h.s.world.mediator().reflects().len()
    }
}

// --- incoming-message.spec.ts :357 -----------------------------------------------------------------------------

#[test]
fn a_message_that_seems_to_come_from_the_user_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_message_that_seems_to_come_from_the_user_is_discarded_and_acked_child",
    );
}

/// Ported from `TDS:357`: a message whose sender is the user's own ID (Desktop builds a group-leave type around a text
/// body, a message no honest client sends) is dropped because it is invalid, with the ack to the chat server and no
/// contact for the user's own ID. Kourier: step 5 discards it (`csp-sender-own`) BEFORE any key is looked up, acks it,
/// marks no nonce (the steps before 8 mark none), reflects nothing, writes no contact and no row.
#[tokio::test]
async fn a_message_that_seems_to_come_from_the_user_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_801);
    let mut myself = s.world.peer(ME);
    myself.send(
        &Letter::new(
            ME,
            0x11,
            ty::GROUP_LEAVE,
            &body::text("synthetic from myself"),
        )
        .discarded(),
    );
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-sender-own"], "the rule");
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    assert_eq!(rig.csp_queue(), 0, "the message stays at the chat server");
    assert_eq!(rig.h.file_rows("contact"), 0, "a contact for the user");
    assert_eq!(rig.h.file_rows("csp_nonce"), 0, "a nonce before step 8");
    assert_eq!(rig.h.file_rows("csp_inbound"), 0, "a row for a discard");
    assert!(messages(&rig.events).is_empty(), "the core got a message");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 1, 0, 0, 0), "the leader's frames");
    no_id(&rig.warns, &[]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :392 -----------------------------------------------------------------------------

#[test]
fn a_metadata_id_that_is_not_the_heads_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_metadata_id_that_is_not_the_heads_is_discarded_and_acked_child",
    );
}

/// Ported from `TDS:392`: a text of a known contact whose metadata names another message id than the head is dropped
/// and acked, and no text is created. Kourier: step 13 (`csp-id-mismatch`), acked, nothing reflected, nothing for the
/// core; the nonce IS marked (a text's replay column, from step 10 on).
#[tokio::test]
async fn a_metadata_id_that_is_not_the_heads_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_802).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    one.send(&Letter::text(ME, 0x12, "synthetic hi").meta_id(0x13));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-id-mismatch"], "the rule");
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    assert!(messages(&rig.events).is_empty(), "the core got a message");
    assert_eq!(rig.h.file_rows("csp_nonce"), 1, "the nonce of the text");
    assert_eq!(rig.h.file_rows("csp_inbound"), 0, "a row for a discard");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 1, 0, 0, 0), "the leader's frames");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :446 -----------------------------------------------------------------------------

#[test]
fn a_nickname_is_updated_cleared_and_taken_from_the_metadata_in_a_contact_sync() {
    support::offline::run_child(
        module_path!(),
        "a_nickname_is_updated_cleared_and_taken_from_the_metadata_in_a_contact_sync_child",
    );
}

/// Ported from `TDS:446`: a contact stored under an old nickname sends five texts. The same nickname: no contact
/// reflection; a new one: a contact update; a cleared one (the empty nickname): an update; the empty one again: none,
/// the nickname is already empty; the legacy nickname the old one and the metadata's a new one: the update to the
/// metadata's (the encrypted nickname wins). Kourier: the same five outcomes, each update ONE `ContactSync.Update` in
/// its own `CONTACT_SYNC` transaction (begin, the update reflected inside, commit), so three transactions, and the
/// phone views, in order, the text, an update and its text, an update and its text, a text, an update and its text.
/// Every text is reflected and acked and receipted (the ids' union).
#[tokio::test]
async fn a_nickname_is_updated_cleared_and_taken_from_the_metadata_in_a_contact_sync_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_803).with_contact_as(ONE, Some("oldnick"), false);
    let mut one = s.world.peer(ONE);
    let mut legacy = [0_u8; 32];
    legacy[..7].copy_from_slice(b"oldnick");
    one.send(&Letter::text(ME, 0x21, "synthetic one").nickname("oldnick"));
    one.send(&Letter::text(ME, 0x22, "synthetic two").nickname("newnick"));
    one.send(&Letter::text(ME, 0x23, "synthetic three").nickname(""));
    one.send(&Letter::text(ME, 0x24, "synthetic four").nickname(""));
    one.send(
        &Letter::text(ME, 0x25, "synthetic five")
            .nickname("newnick")
            .legacy(legacy),
    );
    let mut rig = Rig::up(s).await;
    rig.served(5, 0).await;
    assert_eq!(messages(&rig.events).len(), 5, "the texts the core got");
    assert!(
        contact_upserts(&rig.events) >= 3,
        "the core got {} contact upserts",
        contact_upserts(&rig.events)
    );
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (3, 0, 3),
        "three transactions, each with its one reflect"
    );
    scopes_are_contact_sync(&rig.h);
    assert_eq!(rig.h.net().directory().asked().len(), 0, "a lookup");
    let ids = receipt_ids(&rig.h, &mut one, 1, 5).await;
    assert_eq!(ids, [0x21, 0x22, 0x23, 0x24, 0x25], "the receipted ids");
    let end = rig.end().await;
    let (messages_in, reflects, acks, job_reflects, job_boxes, tx) = frames(&end);
    assert_eq!(
        (messages_in, reflects, acks, tx),
        (5, 8, 5, 6),
        "five texts and three updates reflected"
    );
    assert!(
        (1..=5).contains(&job_boxes) && job_boxes == job_reflects,
        "{job_boxes} receipt jobs and {job_reflects} job reflects"
    );
    let view = rig.viewed();
    assert_eq!(
        synced(&view),
        [
            (false, ONE.to_owned(), Some("newnick".to_owned()), None),
            (false, ONE.to_owned(), Some(String::new()), None),
            (false, ONE.to_owned(), Some("newnick".to_owned()), None),
        ],
        "the updates the phone viewed"
    );
    assert_eq!(
        inbound_kinds(&view),
        [
            "incoming_message",
            "contact_sync",
            "incoming_message",
            "contact_sync",
            "incoming_message",
            "incoming_message",
            "contact_sync",
            "incoming_message",
        ],
        "the order of the reflections"
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :523 -----------------------------------------------------------------------------

#[test]
fn an_unknown_senders_contact_is_created_in_a_transaction_before_its_text_is_reflected() {
    support::offline::run_child(
        module_path!(),
        "an_unknown_senders_contact_is_created_in_a_transaction_before_its_text_is_reflected_child",
    );
}

/// Ported from `TDS:523`: a text of a sender the file does not know, whom the directory knows. Desktop: a
/// `CONTACT_SYNC` transaction, the contact's creation reflected, then the text reflected, acked, and a delivery
/// receipt. Kourier: ONE lookup in the directory, ONE transaction (begin, the `ContactSync.Create` reflected inside
/// it, commit), the create reflected BEFORE the text (the phone views the contact first: its identity, the nickname of
/// the message, DIRECT), then the text reflected, acked, and one receipt to the new contact; the contact is in the
/// file. The scope of every transaction opens as `CONTACT_SYNC`.
#[tokio::test]
async fn an_unknown_senders_contact_is_created_in_a_transaction_before_its_text_is_reflected_child()
{
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_804);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(&Letter::text(ME, 0x31, "synthetic text").nickname("Newbie"));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    let asked = rig.h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert_eq!(asked[0].ids, [NEW], "the lookup's IDs");
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (1, 0, 1),
        "one transaction with its one reflect"
    );
    scopes_are_contact_sync(&rig.h);
    assert_eq!(rig.h.file_rows("contact"), 1, "the contact is not stored");
    assert_eq!(receipt_ids(&rig.h, &mut new, 1, 1).await, [0x31]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 2, 1, 1, 1, 2), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(
        synced(&view),
        [(
            true,
            NEW.to_owned(),
            Some("Newbie".to_owned()),
            Some(AcquaintanceLevel::Direct as i32)
        )],
        "the create the phone viewed"
    );
    assert_eq!(
        inbound_kinds(&view),
        ["contact_sync", "incoming_message"],
        "the create comes before the text"
    );
    assert_eq!(view.incoming_ids(), [(NEW.to_owned(), 0x31)]);
    no_id(&rig.warns, &[NEW]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :574 -----------------------------------------------------------------------------

#[test]
fn a_text_lifts_a_groups_contact_to_direct_in_one_contact_sync() {
    support::offline::run_child(
        module_path!(),
        "a_text_lifts_a_groups_contact_to_direct_in_one_contact_sync_child",
    );
}

/// Ported from `TDS:574`: a contact stored only through a group (`GROUP_OR_DELETED`) sends a text with an unchanged
/// nickname: the acquaintance level is lifted to DIRECT and reflected, and no nickname update goes beside it; then the
/// text is reflected, acked and receipted. Kourier: ONE transaction with ONE `ContactSync.Update` that carries the
/// level alone (the nickname stays absent), reflected before the text; no lookup.
#[tokio::test]
async fn a_text_lifts_a_groups_contact_to_direct_in_one_contact_sync_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_805).with_contact_as(ONE, None, true);
    let mut one = s.world.peer(ONE);
    one.send(&Letter::text(ME, 0x41, "synthetic text"));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(rig.h.net().directory().asked().len(), 0, "a lookup");
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (1, 0, 1),
        "one transaction with its one reflect"
    );
    scopes_are_contact_sync(&rig.h);
    assert!(contact_upserts(&rig.events) >= 1, "no contact upsert");
    assert_eq!(receipt_ids(&rig.h, &mut one, 1, 1).await, [0x41]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 2, 1, 1, 1, 2), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(
        synced(&view),
        [(
            false,
            ONE.to_owned(),
            None,
            Some(AcquaintanceLevel::Direct as i32)
        )],
        "the level alone, no nickname"
    );
    assert_eq!(
        inbound_kinds(&view),
        ["contact_sync", "incoming_message"],
        "the update comes before the text"
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :620 -----------------------------------------------------------------------------

#[test]
fn a_receipt_of_a_groups_contact_changes_no_contact() {
    support::offline::run_child(
        module_path!(),
        "a_receipt_of_a_groups_contact_changes_no_contact_child",
    );
}

/// Ported from `TDS:620`: a contact stored only through a group sends a delivery receipt, a type that creates no
/// implicit direct contact (`missingContactHandling: 'discard'`): its acquaintance level stays, the message is
/// reflected and acked. Kourier: the receipt (`implicit direct: No`) runs no transaction and changes no contact; it is
/// reflected as an incoming message and acked; no receipt is sent for a receipt.
#[tokio::test]
async fn a_receipt_of_a_groups_contact_changes_no_contact_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_806).with_contact_as(ONE, None, true);
    let mut one = s.world.peer(ONE);
    one.send(&Letter::new(
        ME,
        0x51,
        ty::DELIVERY_RECEIPT,
        &body::delivery_receipt(1, &[wire(0x77)]),
    ));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (0, 0, 0),
        "a transaction for a receipt"
    );
    assert_eq!(rig.reflects(), 1, "the receipt is reflected, nothing else");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 1, 1, 0, 0, 0), "the leader's frames");
    let view = rig.viewed();
    assert!(synced(&view).is_empty(), "a contact update");
    assert_eq!(view.incoming_ids(), [(ONE.to_owned(), 0x51)]);
    assert!(
        boxes_for(&rig.h, &mut one, 0).await.is_empty(),
        "a box for a receipt"
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :653 -----------------------------------------------------------------------------

#[test]
fn a_text_from_a_known_contact_is_reflected_acked_and_answered_with_a_receipt() {
    support::offline::run_child(
        module_path!(),
        "a_text_from_a_known_contact_is_reflected_acked_and_answered_with_a_receipt_child",
    );
}

/// Ported from `TDS:653`: a text of a known contact is reflected (the sender, the type), acked, and answered with a
/// `RECEIVED` receipt, and the text is part of the 1:1 conversation. Kourier: the phone views ONE incoming message
/// (sender, id, type TEXT, the body byte for byte, the metadata's time, the CSP nonce), the core gets the text in the
/// chat with the contact, no contact changes (no transaction, no lookup), the receipt goes to the sender.
#[tokio::test]
async fn a_text_from_a_known_contact_is_reflected_acked_and_answered_with_a_receipt_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_807).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let created = START_MS - 7_000;
    one.send(&Letter::text(ME, 0x61, "synthetic secret").created_ms(created));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    let got = messages(&rig.events);
    assert_eq!(got.len(), 1, "the texts the core got");
    assert!(
        got[0].conv == ConvKey::Direct(user(ONE)),
        "not in the chat with the contact"
    );
    assert_eq!(&*got[0].local, local(0x61), "the id");
    assert!(
        got[0].body == Body::text("synthetic secret"),
        "not the text sent"
    );
    assert_eq!(got[0].sent_at, ms(created), "the time of the message");
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (0, 0, 0),
        "a transaction for a known contact"
    );
    assert_eq!(rig.h.net().directory().asked().len(), 0, "a lookup");
    assert_eq!(receipt_ids(&rig.h, &mut one, 1, 1).await, [0x61]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 1, 1, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    let incoming = view.incoming();
    assert_eq!(incoming.len(), 1, "not one reflected message");
    let m = incoming[0];
    assert_eq!(
        (
            m.sender_identity.as_str(),
            m.message_id,
            m.r#type,
            m.created_at
        ),
        (ONE, 0x61, 0x01, created),
        "the reflected message's head"
    );
    assert_eq!(m.body, b"synthetic secret", "the reflected body");
    assert_eq!(m.nonce.len(), 24, "the CSP nonce rides in the reflection");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :705 -----------------------------------------------------------------------------

#[test]
fn a_text_from_an_unknown_contact_reaches_the_core_after_its_contact() {
    support::offline::run_child(
        module_path!(),
        "a_text_from_an_unknown_contact_reaches_the_core_after_its_contact_child",
    );
}

/// Ported from `TDS:705`: the text of an unknown sender is stored after the contact is created (the transaction, the
/// creation reflected, the text reflected, acked, receipted). The core's side of `:523`: the core is sent the contact
/// BEFORE the message, and the message lands in the chat with the new contact, with the text sent.
#[tokio::test]
async fn a_text_from_an_unknown_contact_reaches_the_core_after_its_contact_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_808);
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(&Letter::text(ME, 0x71, "synthetic unknown text"));
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    let contact_at = position(&rig.events, |e| matches!(e, BackendEvent::ContactUpsert(_)));
    let message_at = position(&rig.events, |e| matches!(e, BackendEvent::Message(_)));
    assert!(
        contact_at.is_some() && contact_at < message_at,
        "the contact does not come before the message"
    );
    let got = messages(&rig.events);
    assert_eq!(got.len(), 1, "the texts the core got");
    assert!(
        got[0].conv == ConvKey::Direct(user(NEW)),
        "not in the chat with the new contact"
    );
    assert!(
        got[0].body == Body::text("synthetic unknown text"),
        "not the text sent"
    );
    assert_eq!(receipt_ids(&rig.h, &mut new, 1, 1).await, [0x71]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 2, 1, 1, 1, 2), "the leader's frames");
    no_id(&rig.warns, &[NEW]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1144 ----------------------------------------------------------------------------

#[test]
fn a_group_text_of_a_non_member_is_discarded_and_asks_the_creator_for_the_group() {
    support::offline::run_child(
        module_path!(),
        "a_group_text_of_a_non_member_is_discarded_and_asks_the_creator_for_the_group_child",
    );
}

/// Ported from `TDS:1144`: a group created by one contact whose only member is that contact; a second contact, no
/// member, also writes into it. The member's text is reflected and acked (and no receipt: a group message has none);
/// the non-member's is not stored, a `group-sync-request` goes to the creator, and the message is acked. Kourier: the
/// same: the creator's text is reflected and acked, the other's is discarded by `group-not-member` (acked, not
/// reflected), the creator opens exactly one `group-sync-request` for its group, reflected as an `OutgoingMessage`.
#[tokio::test]
async fn a_group_text_of_a_non_member_is_discarded_and_asks_the_creator_for_the_group_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_809).with_members(ONE, GID, &[ONE]).with_peer(TWO);
    let mut one = s.world.peer(ONE);
    let mut two = s.world.peer(TWO);
    one.send(&group_text(0x91, ONE, GID));
    two.send(&group_text(0x92, ONE, GID).discarded());
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let got = messages(&rig.events);
    assert_eq!(got.len(), 1, "the texts the core got");
    assert!(
        got[0].conv == group_key(ONE, GID),
        "not in the group's chat"
    );
    assert_eq!(&*got[0].local, local(0x91), "not the member's text");
    assert_eq!(discarded_by(&rig.warns), ["group-not-member"], "the rule");
    let opened = boxes_for(&rig.h, &mut one, 1).await;
    assert_eq!(
        sorted(&opened),
        [(
            ty::GROUP_SYNC_REQUEST,
            body::group_member_container(ONE, GID, &[])
        )],
        "what the creator opened"
    );
    assert!(boxes_for(&rig.h, &mut two, 0).await.is_empty());
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 1, 2, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(view.incoming_ids(), [(ONE.to_owned(), 0x91)]);
    assert_eq!(
        outgoing_reflects(&view),
        1,
        "the sync request was not reflected"
    );
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1241 ----------------------------------------------------------------------------

#[test]
fn a_text_of_an_unknown_sender_is_discarded_when_unknown_senders_are_blocked() {
    support::offline::run_child(
        module_path!(),
        "a_text_of_an_unknown_sender_is_discarded_when_unknown_senders_are_blocked_child",
    );
}

/// Ported from `TDS:1241`: with the privacy setting "block unknown", the text of a sender the directory knows but the
/// file does not creates no contact and is acked and dropped. Kourier: the directory is asked once (the steps need the
/// sender's key to open the box), then `csp-blocked` discards the text at step 18, acked; no transaction, no contact,
/// no reflect, nothing for the core.
#[tokio::test]
async fn a_text_of_an_unknown_sender_is_discarded_when_unknown_senders_are_blocked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_810).with_settings(block_unknown());
    s.world.add_identity(NEW);
    let mut new = s.world.peer(NEW);
    new.send(&Letter::text(ME, 0xa1, "synthetic text").discarded());
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-blocked"], "the rule");
    let asked = rig.h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (0, 0, 0),
        "a transaction for a blocked sender"
    );
    assert_eq!(rig.h.file_rows("contact"), 0, "a contact was created");
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    assert!(messages(&rig.events).is_empty(), "the core got a message");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 1, 0, 0, 0), "the leader's frames");
    no_id(&rig.warns, &[NEW]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1275 ----------------------------------------------------------------------------

#[test]
fn a_group_text_of_a_blocked_member_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_group_text_of_a_blocked_member_is_discarded_and_acked_child",
    );
}

/// Ported from `TDS:1275`: a group created by one contact with a second, blocked contact as a member. The creator's
/// text is reflected and acked; the blocked member's is acked and dropped, no message created. Kourier: the same;
/// the blocked member's text is discarded at step 18 (`csp-blocked`), BEFORE the group's own steps, so no
/// `group-sync-request` goes either.
#[tokio::test]
async fn a_group_text_of_a_blocked_member_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_811)
        .with_settings(blocking(&[TWO]))
        .with_members(ONE, GID, &[ONE, TWO]);
    let mut one = s.world.peer(ONE);
    let mut two = s.world.peer(TWO);
    one.send(&group_text(0xb1, ONE, GID));
    two.send(&group_text(0xb2, ONE, GID).discarded());
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-blocked"], "the rule");
    let got = messages(&rig.events);
    assert_eq!(got.len(), 1, "the texts the core got");
    assert_eq!(&*got[0].local, local(0xb1), "not the creator's text");
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 1, 2, 0, 0, 0), "the leader's frames");
    assert!(boxes_for(&rig.h, &mut one, 0).await.is_empty());
    assert!(boxes_for(&rig.h, &mut two, 0).await.is_empty());
    let view = rig.viewed();
    assert_eq!(view.incoming_ids(), [(ONE.to_owned(), 0xb1)]);
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1366 ----------------------------------------------------------------------------

#[test]
fn a_group_name_of_a_blocked_creator_is_held_for_the_phone() {
    support::offline::run_child(
        module_path!(),
        "a_group_name_of_a_blocked_creator_is_held_for_the_phone_child",
    );
}

/// Ported from `TDS:1366`, DIFFERS ON PURPOSE (his TH6-Q1, "They wait for your phone"): a group name message of a
/// BLOCKED creator is still processed (group control is exempt from blocking): Desktop begins a `GROUP_SYNC`
/// transaction, reflects the group's update and the name changes. Kourier does not change a group: the name is a REAL
/// change (the stored name differs), so the message is HELD (`group-sync-needed`): no ack, nothing reflected, no zone,
/// no nonce; the chat server keeps it for the phone. The exemption itself is asserted: it is held, not discarded
/// `csp-blocked`. The count line says one held message by that rule.
#[tokio::test]
async fn a_group_name_of_a_blocked_creator_is_held_for_the_phone_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_812)
        .with_settings(blocking(&[ONE]))
        .with_members(ONE, GID, &[ONE]);
    let mut one = s.world.peer(ONE);
    one.send(&Letter::new(
        ME,
        0xc1,
        ty::GROUP_NAME,
        &body::group_creator_container(GID, &body::group_name("synthetic renamed")),
    ));
    let mut rig = Rig::up(s).await;
    rig.served(0, 1).await;
    let held = rig
        .warns
        .lines()
        .into_iter()
        .filter(is_held)
        .filter_map(|l| l.field("rule").map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(held, ["group-sync-needed"], "the hold's rule");
    assert!(discarded_by(&rig.warns).is_empty(), "a discard");
    assert_eq!(rig.csp_queue(), 1, "the chat server lost the message");
    assert_eq!(rig.reflects(), 0, "a held message was reflected");
    assert!(messages(&rig.events).is_empty(), "the core got a message");
    assert_eq!(
        rig.h.file_rows("csp_nonce"),
        0,
        "a held message left a nonce"
    );
    assert_eq!(
        rig.h.file_rows("csp_inbound"),
        0,
        "a held message left a row"
    );
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 0, 0, 0, 0), "the leader's frames");
    assert_eq!(
        (end.field("held"), end.field("held_by")),
        (Some("1"), Some("group-sync-needed:1")),
        "the count line's holds"
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1433 ----------------------------------------------------------------------------

#[test]
fn a_text_of_a_blocked_contact_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_text_of_a_blocked_contact_is_discarded_and_acked_child",
    );
}

/// Ported from `TDS:1433`: a text of a blocked contact is acked and dropped, no message in the 1:1 conversation.
/// Kourier: `csp-blocked` at step 18, acked, nothing reflected, nothing for the core, no receipt.
#[tokio::test]
async fn a_text_of_a_blocked_contact_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_813).with_settings(blocking(&[ONE])).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    one.send(&Letter::text(ME, 0xd1, "synthetic text").discarded());
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-blocked"], "the rule");
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    assert!(messages(&rig.events).is_empty(), "the core got a message");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 1, 0, 0, 0), "the leader's frames");
    assert!(boxes_for(&rig.h, &mut one, 0).await.is_empty());
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1486 ----------------------------------------------------------------------------

#[test]
fn a_group_setup_of_a_blocked_creator_for_an_unknown_group_is_discarded_and_acked() {
    support::offline::run_child(
        module_path!(),
        "a_group_setup_of_a_blocked_creator_for_an_unknown_group_is_discarded_and_acked_child",
    );
}

/// Ported from `TDS:1486`: a `group-setup` of a blocked creator for a group the file does not hold is acked and
/// dropped, and the group is not created (the dedicated blocking steps of a setup). Kourier: the setup's exemption is
/// by content (`DedicatedSteps`): for an unknown group the creator's block counts, `csp-blocked`, acked, nothing
/// reflected, no group row, no hold.
#[tokio::test]
async fn a_group_setup_of_a_blocked_creator_for_an_unknown_group_is_discarded_and_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_814).with_settings(blocking(&[ONE])).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    one.send(
        &Letter::new(
            ME,
            0xe1,
            ty::GROUP_SETUP,
            &body::group_creator_container(GID, &body::group_setup(&[ME])),
        )
        .discarded(),
    );
    let mut rig = Rig::up(s).await;
    rig.served(1, 0).await;
    assert_eq!(discarded_by(&rig.warns), ["csp-blocked"], "the rule");
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    assert_eq!(rig.h.file_rows("grp"), 0, "the group was created");
    assert_eq!(rig.csp_queue(), 0, "the message stays at the chat server");
    let end = rig.end().await;
    assert_eq!(frames(&end), (1, 0, 1, 0, 0, 0), "the leader's frames");
    assert_eq!(
        (end.field("held"), end.field("held_by")),
        (Some("0"), Some("none")),
        "a hold"
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-message.spec.ts :1682, :1842, :1992 --------------------------------------------------------------

#[test]
fn an_edit_follows_its_text_to_the_core_and_is_not_answered() {
    support::offline::run_child(
        module_path!(),
        "an_edit_follows_its_text_to_the_core_and_is_not_answered_child",
    );
}

/// Ported from `TDS:1682`: a text of a known contact, then the contact's edit of it: both are reflected and acked; the
/// text is receipted (the edit is not), and the message ends up edited. Kourier: both reflected (the phone views
/// two incoming messages, types TEXT and EDIT_MESSAGE) and acked; the core gets the text, then an `Edited` for it
/// (the target by its id, the author unknown on the wire, `by` the contact, the new text, at the edit's own time); ONE
/// receipt, for the text alone.
#[tokio::test]
async fn an_edit_follows_its_text_to_the_core_and_is_not_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_815).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let (sent, edited) = (START_MS - 10_000, START_MS - 5_000);
    one.send(&Letter::text(ME, 0x81, "synthetic original").created_ms(sent));
    one.send(
        &Letter::new(
            ME,
            0x82,
            ty::EDIT_MESSAGE,
            &body::edit_message(wire(0x81), "synthetic edited"),
        )
        .created_ms(edited),
    );
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let chat = ConvKey::Direct(user(ONE));
    let want = BackendEvent::Edited {
        target: target(&chat, 0x81, TargetAuthor::Unknown),
        by: Some(user(ONE)),
        new_local: None,
        body: Body::text("synthetic edited"),
        edited_at: ms(edited),
        keeps_quote: None,
    };
    let edit_at = position(&rig.events, |e| *e == want);
    let text_at = position(&rig.events, |e| matches!(e, BackendEvent::Message(_)));
    assert!(
        edit_at.is_some() && text_at < edit_at,
        "the core did not get the text and then its edit"
    );
    assert_eq!(messages(&rig.events).len(), 1, "the edit became a message");
    assert_eq!(receipt_ids(&rig.h, &mut one, 1, 1).await, [0x81]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(
        view.incoming_ids(),
        [(ONE.to_owned(), 0x81), (ONE.to_owned(), 0x82)]
    );
    let types: Vec<i32> = view.incoming().iter().map(|m| m.r#type).collect();
    assert_eq!(types, [0x01, 0x91], "the types the phone viewed");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

#[test]
fn a_delete_follows_its_text_to_the_core_and_is_not_answered() {
    support::offline::run_child(
        module_path!(),
        "a_delete_follows_its_text_to_the_core_and_is_not_answered_child",
    );
}

/// Ported from `TDS:1842`: a text of a known contact, then the contact's delete of it: both reflected and acked, the
/// text receipted, the message ends up deleted. Kourier: both reflected (types TEXT and DELETE_MESSAGE) and acked; the
/// core gets the text, then a `Deleted` for it (`for_everyone`, `by` the contact, at the delete's own time); one
/// receipt, for the text alone.
#[tokio::test]
async fn a_delete_follows_its_text_to_the_core_and_is_not_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_816).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let (sent, deleted) = (START_MS - 10_000, START_MS - 4_000);
    one.send(&Letter::text(ME, 0x91, "synthetic original").created_ms(sent));
    one.send(
        &Letter::new(
            ME,
            0x92,
            ty::DELETE_MESSAGE,
            &body::delete_message(wire(0x91)),
        )
        .created_ms(deleted),
    );
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let chat = ConvKey::Direct(user(ONE));
    let want = BackendEvent::Deleted {
        target: target(&chat, 0x91, TargetAuthor::Unknown),
        by: Some(user(ONE)),
        for_everyone: true,
        at: ms(deleted),
    };
    let delete_at = position(&rig.events, |e| *e == want);
    let text_at = position(&rig.events, |e| matches!(e, BackendEvent::Message(_)));
    assert!(
        delete_at.is_some() && text_at < delete_at,
        "the core did not get the text and then its delete"
    );
    assert_eq!(receipt_ids(&rig.h, &mut one, 1, 1).await, [0x91]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    let types: Vec<i32> = view.incoming().iter().map(|m| m.r#type).collect();
    assert_eq!(types, [0x01, 0x92], "the types the phone viewed");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

#[test]
fn a_reaction_follows_its_text_to_the_core_and_is_not_answered() {
    support::offline::run_child(
        module_path!(),
        "a_reaction_follows_its_text_to_the_core_and_is_not_answered_child",
    );
}

/// Ported from `TDS:1992`: a text of a known contact, then the contact's emoji reaction to it (apply): both reflected
/// and acked, the text receipted, the message ends up with one reaction of that contact. Kourier: both reflected
/// (types TEXT and REACTION) and acked; the core gets the text, then a `Reaction` (apply, `by` the contact, the emoji,
/// at the reaction's own time); one receipt, for the text alone.
#[tokio::test]
async fn a_reaction_follows_its_text_to_the_core_and_is_not_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_817).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let (sent, reacted) = (START_MS - 10_000, START_MS - 3_000);
    one.send(&Letter::text(ME, 0xa1, "synthetic original").created_ms(sent));
    one.send(
        &Letter::new(
            ME,
            0xa2,
            ty::REACTION,
            &body::reaction_apply(wire(0xa1), "\u{1f377}".as_bytes()),
        )
        .created_ms(reacted),
    );
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let chat = ConvKey::Direct(user(ONE));
    let want = BackendEvent::Reaction {
        target: target(&chat, 0xa1, TargetAuthor::Unknown),
        by: Some(user(ONE)),
        emoji: "\u{1f377}".to_owned(),
        remove: false,
        at: ms(reacted),
    };
    let reaction_at = position(&rig.events, |e| *e == want);
    let text_at = position(&rig.events, |e| matches!(e, BackendEvent::Message(_)));
    assert!(
        reaction_at.is_some() && text_at < reaction_at,
        "the core did not get the text and then its reaction"
    );
    assert_eq!(receipt_ids(&rig.h, &mut one, 1, 1).await, [0xa1]);
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    let types: Vec<i32> = view.incoming().iter().map(|m| m.r#type).collect();
    assert_eq!(types, [0x01, 0x82], "the types the phone viewed");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-delivery-receipt.spec.ts :251, :259 ---------------------------------------------------------------

/// The receipt event of the contact `ONE` for the messages `ids` of the user, in the contact's chat.
fn receipts(kind: ReceiptKind, ids: &[u64], at: u64) -> BackendEvent {
    let chat = ConvKey::Direct(user(ONE));
    BackendEvent::Receipts {
        targets: ids
            .iter()
            .map(|id| target(&chat, *id, TargetAuthor::Own))
            .collect(),
        by: user(ONE),
        kind,
        at: ms(at),
    }
}

/// The reaction event of a legacy receipt (status 3 or 4) of `ONE` on the user's message `id`.
fn thumb(id: u64, emoji: &str, remove: bool, at: i64) -> BackendEvent {
    let chat = ConvKey::Direct(user(ONE));
    BackendEvent::Reaction {
        target: target(&chat, id, TargetAuthor::Own),
        by: Some(user(ONE)),
        emoji: emoji.to_owned(),
        remove,
        at,
    }
}

#[test]
fn received_and_read_receipts_reach_the_core_at_their_own_times() {
    support::offline::run_child(
        module_path!(),
        "received_and_read_receipts_reach_the_core_at_their_own_times_child",
    );
}

/// Ported from `TDS:251` and `TDS:259`: three outbound messages; the contact's `received` receipt names the first two
/// and the third stays untouched (Desktop marks the first two delivered at the receipt's time); the same with `read`.
/// Kourier: the backend keeps no outbound message table; each receipt is reflected as an incoming message, acked,
/// and stated to the core as ONE `Receipts` event whose targets are exactly the two named messages (the user's own,
/// in the contact's chat), by the contact, `Delivered` and then `Read`, at each receipt's OWN time; the third id
/// appears nowhere. A KEPT message always writes its CSP nonce (the pending row holds it: the stage ignores the replay
/// column for a kept message, `Keep.nonce` is read by tests only), so each receipt leaves one nonce, also status 1 and
/// 2 that the spec does not protect against replay; no receipt job goes.
#[tokio::test]
async fn received_and_read_receipts_reach_the_core_at_their_own_times_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_818).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let (first, second) = (START_MS - 6_000, START_MS - 4_000);
    one.send(
        &Letter::new(
            ME,
            0x101,
            ty::DELIVERY_RECEIPT,
            &body::delivery_receipt(1, &[wire(0x201), wire(0x202)]),
        )
        .created_ms(first),
    );
    one.send(
        &Letter::new(
            ME,
            0x102,
            ty::DELIVERY_RECEIPT,
            &body::delivery_receipt(2, &[wire(0x201), wire(0x202)]),
        )
        .created_ms(second),
    );
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let delivered = receipts(ReceiptKind::Delivered, &[0x201, 0x202], first);
    let read = receipts(ReceiptKind::Read, &[0x201, 0x202], second);
    assert!(
        rig.events.contains(&delivered),
        "no `Delivered` for the first two at the receipt's time"
    );
    assert!(
        rig.events.contains(&read),
        "no `Read` for the first two at the receipt's time"
    );
    assert!(
        position(&rig.events, |e| *e == delivered) < position(&rig.events, |e| *e == read),
        "the receipts came in another order"
    );
    assert_eq!(
        rig.h.file_rows("csp_nonce"),
        2,
        "a kept receipt leaves its nonce"
    );
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 0, 0, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(
        view.incoming_ids(),
        [(ONE.to_owned(), 0x101), (ONE.to_owned(), 0x102)]
    );
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-delivery-receipt.spec.ts :267 ---------------------------------------------------------------------

#[test]
fn acknowledge_and_decline_receipts_become_the_contacts_thumbs() {
    support::offline::run_child(
        module_path!(),
        "acknowledge_and_decline_receipts_become_the_contacts_thumbs_child",
    );
}

/// Ported from `TDS:267`: an outbound message gets the contact's `acknowledge` receipt (a thumbs-up at the receipt's
/// time) and then its `decline` (the reaction changes to a thumbs-down at that time): one reaction in the end.
/// Kourier: each receipt is reflected and acked and stated as the spec's legacy pair: every reaction of the contact
/// withdrawn at `created_at - 1`, then the thumb at `created_at`, by the contact on the user's message; the one
/// reaction in the end is the core's result of the two pairs. Both nonces are written (a kept message always writes
/// its nonce; status 3 and 4 are the ones the spec protects against replay), and no receipt job goes.
#[tokio::test]
async fn acknowledge_and_decline_receipts_become_the_contacts_thumbs_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_819).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let (first, second) = (START_MS - 6_000, START_MS - 2_000);
    one.send(
        &Letter::new(
            ME,
            0x111,
            ty::DELIVERY_RECEIPT,
            &body::delivery_receipt(3, &[wire(0x211)]),
        )
        .created_ms(first),
    );
    one.send(
        &Letter::new(
            ME,
            0x112,
            ty::DELIVERY_RECEIPT,
            &body::delivery_receipt(4, &[wire(0x211)]),
        )
        .created_ms(second),
    );
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let wanted = [
        thumb(0x211, "", true, ms(first) - 1),
        thumb(0x211, "\u{1f44d}", false, ms(first)),
        thumb(0x211, "", true, ms(second) - 1),
        thumb(0x211, "\u{1f44e}", false, ms(second)),
    ];
    let reactions: Vec<&BackendEvent> = rig
        .events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Reaction { .. }))
        .collect();
    assert!(
        reactions.len() == 4
            && reactions
                .iter()
                .zip(&wanted)
                .all(|(got, want)| *got == want),
        "the core did not get the two thumb pairs in order"
    );
    assert_eq!(
        rig.h.file_rows("csp_nonce"),
        2,
        "the nonces of the two receipts"
    );
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 0, 0, 0), "the leader's frames");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-delivery-receipt.spec.ts :382 ---------------------------------------------------------------------

#[test]
fn repeated_receipts_are_each_reflected_acked_and_stated() {
    support::offline::run_child(
        module_path!(),
        "repeated_receipts_are_each_reflected_acked_and_stated_child",
    );
}

/// Ported from `TDS:382`: the contact's `received` receipt twice and its `read` twice for one outbound message: only
/// the FIRST time of each is used (a repeat changes nothing). Kourier: the backend has no outbound message table, so it
/// reflects, acks and states EVERY receipt with its own time (four `Receipts` events, two `Delivered` and two `Read`);
/// that the first time wins is the core's (its receipt state keeps the earliest). Each receipt is a message of its own
/// (its own id, so none is a replay).
#[tokio::test]
async fn repeated_receipts_are_each_reflected_acked_and_stated_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_820).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    let times = [
        START_MS - 9_000,
        START_MS - 8_000,
        START_MS - 7_000,
        START_MS - 6_000,
    ];
    for (n, (status, at_ms)) in [1_u8, 1, 2, 2].into_iter().zip(times).enumerate() {
        one.send(
            &Letter::new(
                ME,
                0x121 + u64::try_from(n).expect("fits"),
                ty::DELIVERY_RECEIPT,
                &body::delivery_receipt(status, &[wire(0x221)]),
            )
            .created_ms(at_ms),
        );
    }
    let mut rig = Rig::up(s).await;
    rig.served(4, 0).await;
    let wanted = [
        receipts(ReceiptKind::Delivered, &[0x221], times[0]),
        receipts(ReceiptKind::Delivered, &[0x221], times[1]),
        receipts(ReceiptKind::Read, &[0x221], times[2]),
        receipts(ReceiptKind::Read, &[0x221], times[3]),
    ];
    let stated: Vec<&BackendEvent> = rig
        .events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Receipts { .. }))
        .collect();
    assert!(
        stated.len() == 4 && stated.iter().zip(&wanted).all(|(got, want)| *got == want),
        "the core did not get the four receipts in order"
    );
    assert_eq!(
        rig.h.file_rows("csp_nonce"),
        4,
        "a kept receipt leaves its nonce"
    );
    let end = rig.end().await;
    assert_eq!(frames(&end), (4, 4, 4, 0, 0, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(view.incoming_ids().len(), 4, "the receipts reflected");
    no_id(&rig.warns, &[ONE]);
    rig.h.finish().await;
}

// --- incoming-group-sync-request.spec.ts :80, :87 ----------------------------------------------------------------

#[test]
fn a_sync_request_for_a_group_of_another_creator_or_an_unknown_group_is_discarded() {
    support::offline::run_child(
        module_path!(),
        "a_sync_request_for_a_group_of_another_creator_or_an_unknown_group_is_discarded_child",
    );
}

/// Ported from `TDS:80` and `TDS:87`: a request for a group whose creator is another user is ignored (Desktop's case
/// runs without any group, so it is one with "the group cannot be found" as well); a request for a group that cannot be
/// found is ignored. Kourier asserts both at the wire: ONE asks for a group TWO created and stored (`group-sync-not-creator`),
/// ONE asks for a group of the user that the file lacks (`group-unknown`). Each is acked and discarded: nothing
/// reflected, no job, no answer.
#[tokio::test]
async fn a_sync_request_for_a_group_of_another_creator_or_an_unknown_group_is_discarded_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_821).with_members(TWO, GID, &[TWO, ONE]);
    let mut one = s.world.peer(ONE);
    one.send(&sync_request(0x131, TWO, GID).discarded());
    one.send(&sync_request(0x132, ME, GID2).discarded());
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    assert_eq!(
        discarded_by(&rig.warns),
        ["group-sync-not-creator", "group-unknown"],
        "the rules"
    );
    assert_eq!(rig.reflects(), 0, "a discard was reflected");
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 0, 2, 0, 0, 0), "the leader's frames");
    assert!(boxes_for(&rig.h, &mut one, 0).await.is_empty());
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}

// --- incoming-group-sync-request.spec.ts :94 ---------------------------------------------------------------------

#[test]
fn a_sync_request_for_a_left_or_kicked_group_gets_an_empty_setup() {
    support::offline::run_child(
        module_path!(),
        "a_sync_request_for_a_left_or_kicked_group_gets_an_empty_setup_child",
    );
}

/// Ported from `TDS:94` (both states Desktop runs): the user created two groups, left one and was removed from the
/// other, though a contact is a member of each; the contact's request for either is answered with an EMPTY
/// `group-setup`. Kourier: both requests are kept (reflected, acked) and the contact opens exactly two empty setups,
/// one per group, in the creator container; each is reflected as an `OutgoingMessage` (Desktop sends the empty setup
/// without a reflection, the send core reflects what the spec's properties reflect).
#[tokio::test]
async fn a_sync_request_for_a_left_or_kicked_group_gets_an_empty_setup_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_822)
        .with_members(ME, GID, &[ONE, TWO])
        .with_members(ME, GID2, &[ONE, TWO]);
    let mut one = s.world.peer(ONE);
    one.send(&sync_request(0x141, ME, GID));
    one.send(&sync_request(0x142, ME, GID2));
    let restated = [
        (ME, GID, own_group(GID, &[ONE, TWO], UserState::Left)),
        (ME, GID2, own_group(GID2, &[ONE, TWO], UserState::Kicked)),
    ];
    let mut rig = Rig::up_with(s, &restated).await;
    rig.served(2, 0).await;
    let opened = boxes_for(&rig.h, &mut one, 2).await;
    let mut want = vec![
        (
            ty::GROUP_SETUP,
            body::group_creator_container(GID, &body::group_setup(&[])),
        ),
        (
            ty::GROUP_SETUP,
            body::group_creator_container(GID2, &body::group_setup(&[])),
        ),
    ];
    want.sort();
    assert_eq!(sorted(&opened), want, "what the contact opened");
    assert!(discarded_by(&rig.warns).is_empty(), "a discard");
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 2, 2, 2, 2, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(view.incoming_ids().len(), 2, "the requests reflected");
    assert_eq!(outgoing_reflects(&view), 2, "the setups reflected");
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}

// --- incoming-group-sync-request.spec.ts :122 --------------------------------------------------------------------

#[test]
fn a_non_member_gets_an_empty_setup_and_an_unknown_one_is_not_made_a_contact() {
    support::offline::run_child(
        module_path!(),
        "a_non_member_gets_an_empty_setup_and_an_unknown_one_is_not_made_a_contact_child",
    );
}

/// Ported from `TDS:122` (both variants Desktop runs): a user who is no member of the group asks for it, known to the
/// file or not; Desktop answers with an empty `group-setup`, and for the unknown one creates the contact first.
/// Kourier: the known contact's request is kept (reflected, acked) and answered with an empty setup, reflected as an
/// `OutgoingMessage`. DIFFERS ON PURPOSE for the unknown user (the directory knows it): the spec marks
/// `group-sync-request` "Implicit direct contact creation: No", so the steps look the sender up (the key opens the
/// box) and discard the request at 22.4.1 (`csp-sender-unknown`): acked, no transaction, no contact, no setup. [U: a
/// real phone] whether it answers such a stranger.
#[tokio::test]
async fn a_non_member_gets_an_empty_setup_and_an_unknown_one_is_not_made_a_contact_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_823).with_members(ME, GID, &[TWO]).with_peer(ONE);
    s.world.add_identity(NEW);
    let mut one = s.world.peer(ONE);
    let mut new = s.world.peer(NEW);
    one.send(&sync_request(0x151, ME, GID));
    new.send(&sync_request(0x152, ME, GID).discarded());
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let opened = boxes_for(&rig.h, &mut one, 1).await;
    assert_eq!(
        sorted(&opened),
        [(
            ty::GROUP_SETUP,
            body::group_creator_container(GID, &body::group_setup(&[]))
        )],
        "what the known contact opened"
    );
    assert_eq!(discarded_by(&rig.warns), ["csp-sender-unknown"], "the rule");
    let asked = rig.h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert_eq!(asked[0].ids, [NEW], "the lookup's IDs");
    assert_eq!(
        kourier_transactions(&rig.h, rig.conn),
        (0, 0, 0),
        "a transaction for a sender that is no contact"
    );
    assert_eq!(rig.h.file_rows("contact"), 2, "a contact was made");
    assert!(boxes_for(&rig.h, &mut new, 0).await.is_empty());
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 1, 2, 1, 1, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(view.incoming_ids(), [(ONE.to_owned(), 0x151)]);
    assert_eq!(outgoing_reflects(&view), 1, "the setup reflected");
    no_id(&rig.warns, &[ONE, TWO, NEW]);
    rig.h.finish().await;
}

// --- incoming-group-sync-request.spec.ts :167 --------------------------------------------------------------------

#[test]
fn a_member_gets_the_groups_setup_name_and_no_picture_and_a_group_with_a_picture_is_held() {
    support::offline::run_child(
        module_path!(),
        "a_member_gets_the_groups_setup_name_and_no_picture_and_a_group_with_a_picture_is_held_child",
    );
}

/// Ported from `TDS:167` (both variants Desktop runs): a member asks for a group the user created; Desktop answers with
/// the setup (the members), the name, and the picture message: a delete-picture for a group without a picture, the
/// picture for one with. Kourier: the group without a picture (explicitly removed) is answered with the three jobs to
/// the member, each in the creator container, each reflected as an `OutgoingMessage`: the setup of the stored members
/// (the user left out), the name, the delete-picture. DIFFERS ON PURPOSE with a picture (T-M3, TH8): the request is
/// HELD (`creator-answer-needs-picture`): no ack, no reflect, nothing sent, the chat server keeps it for the phone,
/// which can send the picture.
#[tokio::test]
async fn a_member_gets_the_groups_setup_name_and_no_picture_and_a_group_with_a_picture_is_held_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_824)
        .with_members(ME, GID, &[ONE, TWO])
        .with_members(ME, GID2, &[ONE, TWO]);
    let mut one = s.world.peer(ONE);
    one.send(&sync_request(0x161, ME, GID));
    one.send(&sync_request(0x162, ME, GID2));
    let restated = [
        (
            ME,
            GID,
            with_picture(own_group(GID, &[ONE, TWO], UserState::Member), false),
        ),
        (
            ME,
            GID2,
            with_picture(own_group(GID2, &[ONE, TWO], UserState::Member), true),
        ),
    ];
    let mut rig = Rig::up_with(s, &restated).await;
    rig.served(1, 1).await;
    let opened = boxes_for(&rig.h, &mut one, 3).await;
    let inside = |inner: &[u8]| body::group_creator_container(GID, inner);
    let mut want = vec![
        (ty::GROUP_SETUP, inside(&body::group_setup(&[ONE, TWO]))),
        (ty::GROUP_NAME, inside(&body::group_name("synthetic group"))),
        (ty::GROUP_DELETE_PROFILE_PICTURE, inside(&[])),
    ];
    want.sort();
    assert_eq!(sorted(&opened), want, "what the member opened");
    let held = rig
        .warns
        .lines()
        .into_iter()
        .filter(is_held)
        .filter_map(|l| l.field("rule").map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(held, ["creator-answer-needs-picture"], "the hold's rule");
    assert_eq!(rig.csp_queue(), 1, "the held request left the chat server");
    assert_eq!(rig.reflects(), 4, "the request and the three answers");
    let end = rig.end().await;
    assert_eq!(frames(&end), (2, 1, 1, 3, 3, 0), "the leader's frames");
    assert_eq!(
        (end.field("held"), end.field("held_by")),
        (Some("1"), Some("creator-answer-needs-picture:1")),
        "the count line's holds"
    );
    let view = rig.viewed();
    assert_eq!(view.incoming_ids(), [(ONE.to_owned(), 0x161)]);
    assert_eq!(outgoing_reflects(&view), 3, "the three answers reflected");
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}

// --- incoming-group-sync-request.spec.ts :220, :278 --------------------------------------------------------------

#[test]
fn a_second_sync_request_within_the_hour_is_discarded_and_one_after_it_is_answered() {
    support::offline::run_child(
        module_path!(),
        "a_second_sync_request_within_the_hour_is_discarded_and_one_after_it_is_answered_child",
    );
}

/// Ported from `TDS:220` and `TDS:278`: a contact who is no member asks twice for a group of the user: the first is
/// answered with an empty setup, the second is discarded and does not move the memory of the first; a request more than
/// an hour after the memory is answered again. Kourier: the first request is kept (reflected, acked) with one empty
/// setup; the second is discarded `group-sync-throttled` and ACKED (Desktop's task writes nothing), no job; the clocks
/// walk to 65 minutes (through the echoes, never a single jump) and a third request is kept and answered again, so
/// the contact opens two empty setups in all.
#[tokio::test]
async fn a_second_sync_request_within_the_hour_is_discarded_and_one_after_it_is_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_825).with_members(ME, GID, &[TWO]).with_peer(ONE);
    let mut one = s.world.peer(ONE);
    one.send(&sync_request(0x171, ME, GID));
    one.send(&sync_request(0x172, ME, GID).discarded());
    let mut rig = Rig::up(s).await;
    rig.served(2, 0).await;
    let first = boxes_for(&rig.h, &mut one, 1).await;
    let empty = (
        ty::GROUP_SETUP,
        body::group_creator_container(GID, &body::group_setup(&[])),
    );
    assert_eq!(
        sorted(&first),
        std::slice::from_ref(&empty),
        "the first answer"
    );
    assert_eq!(
        discarded_by(&rig.warns),
        ["group-sync-throttled"],
        "the rule"
    );
    assert_eq!(rig.reflects(), 2, "the first request and its setup");
    through_echoes(&rig.h, at(0), at(65 * 60)).await;
    one.send(&sync_request(0x173, ME, GID));
    rig.h.net().flush();
    wait_acks(&rig.h, rig.conn, 3).await;
    let again = boxes_for(&rig.h, &mut one, 1).await;
    assert_eq!(sorted(&again), [empty], "the answer after the hour");
    assert_eq!(
        discarded_by(&rig.warns),
        ["group-sync-throttled"],
        "one discard in all"
    );
    let end = rig.end().await;
    assert_eq!(frames(&end), (3, 2, 3, 2, 2, 0), "the leader's frames");
    let view = rig.viewed();
    assert_eq!(
        view.incoming_ids(),
        [(ONE.to_owned(), 0x171), (ONE.to_owned(), 0x173)],
        "the requests reflected"
    );
    no_id(&rig.warns, &[ONE, TWO]);
    rig.h.finish().await;
}
