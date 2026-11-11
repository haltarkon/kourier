//! TH5a A9b: THREEMA DESKTOP'S OWN RECEIVE TESTS AS SEQUENCE ORACLES. Desktop's mocha specs of the reflected D2D
//! tasks (`TD` = `threema-desktop/apps/desktop/src/test/mocha/common/network/protocol/task`, at `93a6615c`)
//! say what a reflected envelope must do; here the SAME input goes through the offline fake's phone
//! (`tests/support/mock/{body,envelope}.rs`, written from the spec, not from Kourier's parser) to the real
//! actor, and the OUTCOME is asserted in Kourier's terms: the events the core side gets (read and acked),
//! or nothing; the file's rows (`core_outbox`, `deferred`, the nonces, the stored BLOBs); the WARN lines.
//! Behaviour is ported, not code: no Desktop code and no literal datum is copied (the texts, names and IDs
//! are synthetic); `REUSE.toml` names Threema AG beside Kourier all the same, as for every file ported from
//! Desktop. `task/d2m/reflected.spec.ts` is NOT an oracle (it
//! stubs `_processMessage`) and is not read here.
//!
//! WHERE KOURIER DIFFERS ON PURPOSE the test asserts Kourier's rule and says which decision it is:
//! - GROUP CONTROL (premise 6 of `briefs/th5.md`): Desktop's group specs run without a protocol version
//!   (`D2dProtocolVersion.UNSPECIFIED`, its V0.1 fallback) and APPLY a reflected GROUP_SETUP / GROUP_NAME /
//!   GROUP_LEAVE; Kourier drops all three (V0.1 / unversioned: the spec's "will desync" WARN, rule
//!   `group-control-desync`; V0.2+: quietly) and takes the group's state, members and notices ONLY from
//!   `GroupSync` (`md-d2d.proto` `GroupSync.Update`). The tests send Desktop's input, see the drop, and
//!   then send the `GroupSync` that carries the same change and assert its events.
//! - H5 (his QC): a text into an ARCHIVED chat brings it back in the same row; Desktop's reflected message
//!   does not (there the CSP receiver unarchives and reflects a sync).
//! - E1 (his QB): a contact that becomes DIRECT states its 1:1 chat at once.
//! - E5 (the lead's call after A7b): a message of a HIDDEN contact states its chat's patch first.
//! - A `ContactSync.Create` of a stored contact is an UPDATE (Desktop discards it); an `Update` of an unknown
//!   contact WITH a key creates it (`briefs/th5.md` 2.6; Desktop discards it).
//! - An EPHEMERAL reflected envelope is never acked (the spec, `md-d2m.struct.yml`; premise 3 of the brief);
//!   Desktop acks every reflected one.
//! - Two envelopes with one message id: Desktop's `hasMessage` discards the second; Kourier's backend has no
//!   such memory (its nonce store is the replay check) and ships both: the core's idempotent ingest
//!   (`docs/architecture.md` 5.6) is the dedup. Asserted as it is; see the test's doc.
//! - WHO MAY EDIT OR DELETE (TH5b B6): Desktop's content-update task refuses an edit or a delete whose sender is
//!   not the author of the target. Kourier's backend keeps no message table and the wire names no author in
//!   an edit, so it states the modifier with `by` = the sender and the CORE drops the one that is not the
//!   author's (`kourier-core/src/ingest/modifier.rs` `edit` / `delete`). The tests assert the event.
//! - POLLS (TH5b R15, R18): Desktop applies a poll vote to its poll model (and refuses several choices on a
//!   single-choice poll). Kourier maps no poll in TH5b: a poll is an unsupported message, its votes wait in
//!   `deferred` as `poll-vote`, unread, acked. The three cases assert that, not Desktop's tally.
//!
//! EVERY DESKTOP CASE READ, with its status (P = ported here, N = not applicable): 35 of the 36 ported, 1 N.
//!
//! `TD/d2d/reflected-message.spec.ts`
//! - :93  incoming text from a contact ........................ P `incoming_text_from_a_contact`
//! - :135 incoming text from a group .......................... P `incoming_text_from_a_group`
//! - :187 incoming EDIT from a contact ........................ P `an_incoming_edit_from_a_contact_reaches_the_core_at_its_own_time`
//! - :261 incoming DELETE from a contact ...................... P `an_incoming_delete_from_a_contact_reaches_the_core_at_its_own_time`
//! - :381 outgoing text to a contact .......................... P `outgoing_text_to_a_contact`
//! - :416 outgoing text to a group ............................ P `outgoing_text_to_a_group`
//! - :463 reject text bytes directed at a group ............... P `text_bytes_directed_at_a_group_are_dropped`
//! - :499 reject group text bytes directed at a contact ....... P `group_text_without_a_container_directed_at_a_contact_is_dropped`
//! - :532 reject text directed at a group (same input as :463)  P `text_bytes_directed_at_a_group_are_dropped` (twice)
//! - :572 reject group text directed at a contact ............. P `group_text_with_a_container_directed_at_a_contact_is_dropped`
//! - :609 reject group message, conversation mismatch ......... P `a_container_that_names_another_group_is_dropped`
//! - :660 outgoing EDIT ....................................... P `an_outgoing_edit_reaches_the_core_with_no_author_at_its_own_time`
//! - :737 outgoing DELETE ..................................... P `an_outgoing_delete_reaches_the_core_with_no_author_at_its_own_time`
//!
//! `TD/d2d/reflected-group-name.spec.ts`
//! - :41  incoming: group name is updated ..................... P (Kourier's rule) `reflected_group_names_are_dropped_and_the_group_sync_renames`
//! - :87  outgoing: group name is updated ..................... P (same test, second half)
//!
//! `TD/common/group-setup.ts` (run as `d2d/reflected-incoming-group-setup.spec.ts:29`, mode `d2d`)
//! - :70  setup for a new group ............................... P (Kourier's rule) `a_reflected_group_setup_is_dropped_and_the_group_sync_creates`
//! - :181 duplicate IDs, creator as member ................... P (same test; the revoked contact is N: Desktop asks its directory, the phone's sync is already the truth)
//! - :261 setup for an existing group, a member added ......... P `group_setup_states_come_through_the_group_sync_with_notices`
//! - :347 handle being removed from a group ................... P (same test)
//! - :420 handle being re-added to a group .................... P (same test)
//!
//! `TD/common/group-leave.ts` (run as `d2d/reflected-incoming-group-leave.spec.ts:24`, mode `d2d`)
//! - :68  the sender is the creator: discard .................. P `a_reflected_group_leave_is_dropped_and_sends_nothing`
//! - :85  group unknown and we are the creator: discard ....... P (same test)
//! - :102 group unknown, not the creator: group sync request .. P (same test: a follower sends nothing; in `d2d` mode Desktop expects nothing either)
//! - :166 sender is not a contact (mode `csp` only) ........... N (the CSP receive path, not a reflection)
//! - :212 remove a member from the local group ................ P (Kourier's rule) `a_leave_reaches_the_core_through_the_group_sync`
//!
//! `TD/d2d/reflected-outgoing-group-leave.spec.ts`
//! - :56  ignore an outgoing leave for an unknown group ....... P `a_reflected_group_leave_is_dropped_and_sends_nothing`
//! - :69  ignore it for a group we are not part of (KICKED) ... P (same test: the stored state stays KICKED)
//! - :96  process it for a group we are part of ............... P (Kourier's rule) `a_leave_reaches_the_core_through_the_group_sync`
//! - :123 we are the creator: disband, members kept ........... P (same test)
//!
//! `TD/d2d/reflected-incoming-delivery-receipt.spec.ts`
//! - :73  incoming receipt for an outbound message ............ P `the_contacts_receipts_for_our_message_reach_the_core`
//! - :137 outgoing receipt for an inbound message ............. P `our_phones_receipts_for_the_contacts_message_reach_the_core`
//!
//! `TD/d2d/reflected-message-content-update.spec.ts`
//! - :129 reject an EDIT from a sender who is not the author .. P (Kourier's rule: the core rejects) `an_edit_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse`
//! - :175 reject a DELETE from a sender who is not the author . P (Kourier's rule: the core rejects) `a_delete_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse`
//!
//! `TD/d2d/reflected-poll-update.spec.ts`
//! - :63  discard a multi-choice vote on a single-choice poll . P (Kourier's rule: deferred) `a_vote_with_several_choices_on_a_single_choice_poll_waits_unread`
//! - :71  accept a single choice on a single-choice poll ...... P (Kourier's rule: deferred) `a_vote_with_one_choice_on_a_single_choice_poll_waits_unread`
//! - :79  accept several choices on a multiple-choice poll .... P (Kourier's rule: deferred) `a_vote_with_several_choices_on_a_multiple_choice_poll_waits_unread`
//!
//! The task code the specs run (no spec of its own; `TC` = `threema-desktop/apps/desktop/src/common/network/
//! protocol/task/d2d`), the cases where Desktop DISCARDS:
//! - `TC/reflected-incoming-message.ts:203` the sender is not a contact (log "missing sender contact") ..... P `a_message_from_an_unknown_contact_or_into_an_unknown_group_is_dropped`
//! - `TC/reflected-incoming-message.ts:244` the conversation is not found (a group text of an unknown group) P (same test)
//! - `TC/reflected-incoming-message.ts:253` / `reflected-outgoing-message.ts:281` the message id was already received P `a_second_envelope_with_a_known_message_id_is_shipped_again` (Kourier differs)
//! - `TC/reflected-outgoing-message.ts:270` the conversation is not found ................................... P `an_outgoing_message_to_an_unknown_contact_or_group_is_dropped`
//! - `TC/reflected-incoming-message.ts:172` the CSP nonce is persisted before the sender is looked up .... P (the dropped tests count the nonces)
//! - `TC/reflected-outgoing-message.ts:241`, `:256` the conversation type / the group identity does not match . P (the same tests as spec `:463` .. `:609`)
//! - `TC/reflected-contact-sync.ts:79` a `create` of an existing contact is discarded ....................... P `contact_syncs_create_update_and_discard_as_kourier_does`
//! - `TC/reflected-contact-sync.ts:92` an `update` of an unknown contact is discarded ....................... P (same test)
//! - `TC/reflected-group-sync.ts:67`, `:87` a `delete` / an `update` of an unknown group is discarded ....... P `a_group_sync_update_or_delete_of_an_unknown_group_is_dropped`
//! - `TC/reflected-group-sync.ts:115`, `:126` a `create` asserts that its creator and members are contacts ........... N (an assertion that ends Desktop's task, not a rule; Kourier states what the phone's sync says)
//! - `TD/../d2m/reflected.ts:110-132` Desktop acks every reflected envelope, ephemeral or not (not a spec; the brief's premise 3) . P `an_ephemeral_typing_is_not_acked_and_one_without_the_flag_is`
//!
//! Each test is a parent/child `unshare -rn` pair like its neighbours; both clocks stay where they are.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::Connection;
use kourier_model::{
    Body, ConvKey, ConversationPatch, Member, MemberList, MemberRole, MsgTarget, Origin,
    OutgoingState, Patch, ReceiptKind, SystemEvent, TargetAuthor, UserId,
};
use kourier_threema::store;
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;
use tracing::Level;

use super::harness::{self, H, ME, Scene, scene, traced};
use super::reflect::{PEER, PHONE, connect, counts, deferred, phone_of, wait_acks};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::envelope::{
    Author, ContactDelta, Conv, GroupDelta, MemberChange, Msg, Reflect as _,
};
use crate::support::mock::phone::Phone;
use crate::support::warns::Warns;

/// A second contact, a member of groups (synthetic).
const PEER2: &str = "A9BPEER2";
/// A sender the file has no contact for (synthetic).
const STRANGER: &str = "A9BSTRG1";
/// The groups of these tests (creator `PEER` unless a test says `ME`).
const GROUP: u64 = 0x0a09_0807_0605_0403;
const GROUP2: u64 = 0x2121_2222_2323_2424;
const GROUP3: u64 = 0x3131_3232_3333_3434;

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

/// The phone, the actor on the fake and the number of reflected-acks the test expects so far.
struct W {
    h: H,
    phone: Phone,
    author: Author,
    acks: usize,
}

impl W {
    /// Starts the actor over the scene's file and waits until it is online.
    async fn new(s: Scene) -> Self {
        let phone = phone_of(&s);
        let mut h = harness::start(s);
        connect(&mut h).await;
        Self {
            h,
            phone,
            author: Author::new(PHONE),
            acks: 0,
        }
    }

    /// Reflects these envelopes in one go (each will be acked).
    fn reflect(&mut self, envelopes: &[d2d::Envelope]) {
        self.acks += envelopes.len();
        let phone = &mut self.phone;
        self.h.net().act(|_| {
            for envelope in envelopes {
                phone.reflect_envelope(envelope);
            }
        });
    }

    /// Reflects these envelopes and waits until the fake holds their acks (the zone committed first).
    async fn reflect_acked(&mut self, envelopes: &[d2d::Envelope]) {
        self.reflect(envelopes);
        wait_acks(&self.h, self.acks).await;
    }

    /// The core side: reads envelopes until `n` events came, ACKING each.
    async fn events(&mut self, n: usize) -> Vec<BackendEvent> {
        let mut got = Vec::new();
        while got.len() < n {
            let envelope = self.h.envelope().await;
            let ack = envelope.ack.expect("a durable envelope");
            got.extend(envelope.events);
            ack.complete(Ok(()));
        }
        assert!(got.len() == n, "more events than expected");
        got
    }

    /// The actor has taken everything and shipped nothing more.
    async fn quiet(&mut self) {
        self.h.sync().await;
        self.h.nothing_for_the_core();
    }

    /// Everything reflected so far was dropped: nothing for the core, no `core_outbox` row, no `deferred`
    /// row, `d2d` D2D nonces and `csp` CSP nonces (Desktop persists the CSP nonce before it looks at the
    /// sender or the conversation).
    async fn only_nonces(&mut self, d2d: i64, csp: i64) {
        self.quiet().await;
        assert_eq!(
            self.h.file_rows("core_outbox"),
            0,
            "a dropped message left a row"
        );
        assert_eq!(
            counts(&self.h),
            (0, d2d, csp),
            "not the nonces of the dropped ones"
        );
    }
}

/// How many envelopes `warns` counted as dropped by `rule` (the line is `reflected envelopes acked and
/// dropped: <n> by <rule>`, one per rule and batch; a quiet rule has no WARN).
fn dropped_by(warns: &Warns, rule: &str) -> usize {
    warns
        .at(Level::WARN)
        .iter()
        .filter_map(|line| {
            let rest = line.strip_prefix("reflected envelopes acked and dropped: ")?;
            let (n, by) = rest.split_once(' ')?;
            (by.strip_prefix("by ")? == rule)
                .then(|| n.parse::<usize>().ok())
                .flatten()
        })
        .sum()
}

fn message(event: &BackendEvent) -> &IncomingMessage {
    match event {
        BackendEvent::Message(m) => m,
        _ => panic!("not a message event"),
    }
}

fn notice(event: &BackendEvent) -> &SystemEvent {
    match &message(event).body {
        Body::System(s) => s,
        _ => panic!("not a system notice"),
    }
}

/// A 1:1 chat as the join states it: Threema's features, then what `state` sets.
fn direct(id: &str, state: impl FnOnce(&mut ConversationPatch)) -> BackendEvent {
    let mut patch = ConversationPatch::new(ConvKey::Direct(user(id)));
    patch.features = Some(support::threema_features());
    state(&mut patch);
    BackendEvent::ConversationUpsert(patch)
}

fn plain(_: &mut ConversationPatch) {}

/// The `ConversationUpsert` of a group as the sync states it: the creator first as `Admin`, then `ids` (the
/// other members and the user, as the final list holds them) sorted, Threema's features.
fn group_ev(creator: &str, id: u64, title: &str, ids: &[&str]) -> BackendEvent {
    let mut patch = ConversationPatch::new(group_key_text(creator, id));
    patch.title = Patch::Set(title.to_owned());
    let mut sorted: Vec<&str> = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut list = vec![Member {
        user: user(creator),
        role: MemberRole::Admin,
    }];
    list.extend(sorted.iter().map(|m| Member {
        user: user(m),
        role: MemberRole::Member,
    }));
    patch.members = Some(MemberList::snapshot(list));
    patch.features = Some(support::threema_features());
    BackendEvent::ConversationUpsert(patch)
}

/// The bring-back's `ConversationUpsert` (H5): `archived: Some(false)` alone.
fn unarchive(key: ConvKey) -> BackendEvent {
    let mut patch = ConversationPatch::new(key);
    patch.archived = Some(false);
    BackendEvent::ConversationUpsert(patch)
}

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

fn members_of(group: &d2d_sync::Group) -> Vec<String> {
    group
        .member_identities
        .as_ref()
        .map(|m| m.identities.clone())
        .unwrap_or_default()
}

fn user_state(group: &d2d_sync::Group) -> Option<i32> {
    group.user_state
}

const STATE_MEMBER: Option<i32> = Some(d2d_sync::group::UserState::Member as i32);
const STATE_KICKED: Option<i32> = Some(d2d_sync::group::UserState::Kicked as i32);
const STATE_LEFT: Option<i32> = Some(d2d_sync::group::UserState::Left as i32);

// --- reflected-message.spec.ts :93, :135, :381, :416 -------------------------------------------------

#[test]
fn incoming_text_from_a_contact() {
    support::offline::run_child(module_path!(), "incoming_text_from_a_contact_child");
}

/// Desktop `:93`: the message is in the contact's conversation, inbound, with `createdAt` as sent and
/// `receivedAt` the reflection's time. Kourier: ONE `Message` for the 1:1 chat, `Remote` author, `sent_at` =
/// `created_at`, `server_at` = the mediator's `Reflected.timestamp`, the body whole; nothing for the group.
#[tokio::test]
async fn incoming_text_from_a_contact_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(901).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let reflected_at = w.h.now();
    let envelope = w.author.incoming(
        PEER,
        &Msg::text([1; 8], created, "synthetic hello").nonce([1; 24]),
    );
    w.reflect(&[envelope]);
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, ConvKey::Direct(user(PEER)));
    assert_eq!(&*m.local, "0101010101010101");
    assert_eq!(
        m.origin,
        Origin::Remote {
            author: user(PEER),
            device: None
        }
    );
    assert_eq!(m.body, Body::text("synthetic hello"));
    assert_eq!(m.sent_at, i64::try_from(created).unwrap_or(0), "createdAt");
    assert_eq!(
        m.server_at,
        Some(i64::try_from(reflected_at).unwrap_or(0)),
        "the reflection's time"
    );
    assert!(m.initial_out_state.is_none() && !m.catch_up);
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 1, 1));
    w.h.finish().await;
}

#[test]
fn incoming_text_from_a_group() {
    support::offline::run_child(module_path!(), "incoming_text_from_a_group_child");
}

/// Desktop `:135`: the message is in the GROUP's conversation, not in the contact's. Kourier: one `Message`
/// under the group's key (the creator's ID and the group id), authored by the sender; none for the 1:1 chat.
#[tokio::test]
async fn incoming_text_from_a_group_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(902).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let reflected_at = w.h.now();
    let envelope = w.author.incoming(
        PEER,
        &Msg::text([2; 8], created, "synthetic group hello")
            .in_group(PEER, GROUP)
            .nonce([2; 24]),
    );
    w.reflect(&[envelope]);
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, group_key_text(PEER, GROUP));
    assert_eq!(m.body, Body::text("synthetic group hello"));
    assert_eq!(
        m.origin,
        Origin::Remote {
            author: user(PEER),
            device: None
        }
    );
    assert_eq!(m.sent_at, i64::try_from(created).unwrap_or(0));
    assert_eq!(m.server_at, Some(i64::try_from(reflected_at).unwrap_or(0)));
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 1, 1));
    w.h.finish().await;
}

#[test]
fn outgoing_text_to_a_contact() {
    support::offline::run_child(module_path!(), "outgoing_text_to_a_contact_child");
}

/// Desktop `:381`: an OUTBOUND message in the contact's conversation, `createdAt` as sent, `sentAt` NOT yet
/// set. Kourier: one `Message` of our other device, `sent_at` = `created_at`, `initial_out_state:
/// Sending` (C9: a separate `OutgoingMessageUpdate` states it sent, TH5b R10; without one it stays "not yet sent" as in
/// Desktop).
#[tokio::test]
async fn outgoing_text_to_a_contact_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(903).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let envelope = w.author.outgoing(
        &Conv::contact(PEER),
        &Msg::text([3; 8], START_MS, "synthetic out").nonce([3; 24]),
    );
    w.reflect(&[envelope]);
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, ConvKey::Direct(user(PEER)));
    assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(m.body, Body::text("synthetic out"));
    assert_eq!(m.sent_at, i64::try_from(START_MS).unwrap_or(0));
    assert_eq!(
        m.initial_out_state,
        Some(OutgoingState::Sending { progress: None })
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 1, 1));
    w.h.finish().await;
}

#[test]
fn outgoing_text_to_a_group() {
    support::offline::run_child(module_path!(), "outgoing_text_to_a_group_child");
}

/// Desktop `:416`: the message is in the group's conversation, outbound, not in the contact's. Kourier: one
/// `Message` under the group's key, our other device, `Sending`.
#[tokio::test]
async fn outgoing_text_to_a_group_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(904).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let envelope = w.author.outgoing(
        &Conv::group(PEER, GROUP),
        &Msg::text([4; 8], START_MS, "synthetic group out")
            .in_group(PEER, GROUP)
            .nonce([4; 24]),
    );
    w.reflect(&[envelope]);
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, group_key_text(PEER, GROUP));
    assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(m.body, Body::text("synthetic group out"));
    assert_eq!(
        m.initial_out_state,
        Some(OutgoingState::Sending { progress: None })
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 1, 1));
    w.h.finish().await;
}

// --- reflected-message.spec.ts :463, :499, :532, :572, :609: the five rejects ------------------------

#[test]
fn text_bytes_directed_at_a_group_are_dropped() {
    support::offline::run_child(
        module_path!(),
        "text_bytes_directed_at_a_group_are_dropped_child",
    );
}

/// Desktop `:463` and `:532` (the same input twice): a plain TEXT whose conversation is a GROUP is not
/// processed; neither the contact's nor the group's conversation gets a message. Kourier: acked, dropped
/// by `conversation-mismatch`, nothing for the core, the CSP nonces still written.
#[tokio::test]
async fn text_bytes_directed_at_a_group_are_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(905).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    for (n, id) in [[5u8; 8], [6; 8]].into_iter().enumerate() {
        let envelope = w.author.outgoing(
            &Conv::group(PEER, GROUP),
            &Msg::text(id, START_MS, "synthetic bytes").nonce([id[0]; 24]),
        );
        w.reflect_acked(&[envelope]).await;
        assert_eq!(dropped_by(&warns, "conversation-mismatch"), n + 1);
    }
    w.only_nonces(2, 2).await;
    w.h.finish().await;
}

#[test]
fn group_text_without_a_container_directed_at_a_contact_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "group_text_without_a_container_directed_at_a_contact_is_dropped_child",
    );
}

/// Desktop `:499`: a GROUP_TEXT whose body is plain text (no container) and whose conversation is a CONTACT
/// is not processed. Kourier: dropped by `conversation-mismatch`.
#[tokio::test]
async fn group_text_without_a_container_directed_at_a_contact_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(906).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let envelope = w.author.outgoing(
        &Conv::contact(PEER),
        &Msg::raw(
            [7; 8],
            START_MS,
            ty::GROUP_TEXT,
            body::text("synthetic bytes"),
        )
        .nonce([7; 24]),
    );
    w.reflect_acked(&[envelope]).await;
    assert_eq!(dropped_by(&warns, "conversation-mismatch"), 1);
    w.only_nonces(1, 1).await;
    w.h.finish().await;
}

#[test]
fn group_text_with_a_container_directed_at_a_contact_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "group_text_with_a_container_directed_at_a_contact_is_dropped_child",
    );
}

/// Desktop `:572`: a GROUP_TEXT with its container, conversation a CONTACT: not processed. Kourier: dropped
/// by `conversation-mismatch`.
#[tokio::test]
async fn group_text_with_a_container_directed_at_a_contact_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(907).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let envelope = w.author.outgoing(
        &Conv::contact(PEER),
        &Msg::text([8; 8], START_MS, "synthetic bytes")
            .in_group(PEER, GROUP)
            .nonce([8; 24]),
    );
    w.reflect_acked(&[envelope]).await;
    assert_eq!(dropped_by(&warns, "conversation-mismatch"), 1);
    w.only_nonces(1, 1).await;
    w.h.finish().await;
}

#[test]
fn a_container_that_names_another_group_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "a_container_that_names_another_group_is_dropped_child",
    );
}

/// Desktop `:609`: two groups exist; the envelope says group 1, the container says group 2: processed
/// nowhere (neither group, nor the contact). Kourier: dropped by `group-mismatch`.
#[tokio::test]
async fn a_container_that_names_another_group_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(908)
        .with_contacts(&[PEER])
        .with_group(PEER, GROUP)
        .with_group(PEER, GROUP2);
    let mut w = W::new(s).await;
    let envelope = w.author.outgoing(
        &Conv::group(PEER, GROUP),
        &Msg::text([9; 8], START_MS, "synthetic bytes")
            .in_group(PEER, GROUP2)
            .nonce([9; 24]),
    );
    w.reflect_acked(&[envelope]).await;
    assert_eq!(dropped_by(&warns, "group-mismatch"), 1);
    w.only_nonces(1, 1).await;
    w.h.finish().await;
}

// --- the task code: what Desktop discards (M2) -------------------------------------------------------

#[test]
fn a_message_from_an_unknown_contact_or_into_an_unknown_group_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "a_message_from_an_unknown_contact_or_into_an_unknown_group_is_dropped_child",
    );
}

/// `TC/reflected-incoming-message.ts`: a sender that is no contact is discarded after the CSP nonce was
/// persisted; a group text for a conversation that does not exist is discarded too. Decision M2: acked and
/// dropped (rules `sender-unknown`, `group-unknown`), one counted WARN each, no row, no event, the nonces
/// written; a text of a known sender in a known group right after is still mapped.
#[tokio::test]
async fn a_message_from_an_unknown_contact_or_into_an_unknown_group_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(909).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let stranger = w.author.incoming(
        STRANGER,
        &Msg::text([1; 8], START_MS, "synthetic stranger").nonce([1; 24]),
    );
    let stranger_in_group = w.author.incoming(
        STRANGER,
        &Msg::text([2; 8], START_MS, "synthetic stranger in group")
            .in_group(PEER, GROUP)
            .nonce([2; 24]),
    );
    let lost_group = w.author.incoming(
        PEER,
        &Msg::text([3; 8], START_MS, "synthetic lost group")
            .in_group(PEER, GROUP2)
            .nonce([3; 24]),
    );
    w.reflect_acked(&[stranger]).await;
    w.reflect_acked(&[stranger_in_group]).await;
    w.reflect_acked(&[lost_group]).await;
    assert_eq!(dropped_by(&warns, "sender-unknown"), 2);
    assert_eq!(dropped_by(&warns, "group-unknown"), 1);
    w.only_nonces(3, 3).await;
    let known = w.author.incoming(
        PEER,
        &Msg::text([4; 8], START_MS, "synthetic known").nonce([4; 24]),
    );
    w.reflect(&[known]);
    let got = w.events(1).await;
    assert_eq!(message(&got[0]).body, Body::text("synthetic known"));
    w.h.finish().await;
}

#[test]
fn an_outgoing_message_to_an_unknown_contact_or_group_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "an_outgoing_message_to_an_unknown_contact_or_group_is_dropped_child",
    );
}

/// `TC/reflected-outgoing-message.ts`: the CSP nonces are persisted first, then a conversation that is not
/// found is discarded. Kourier: `contact-unknown` / `group-unknown`, one WARN each, nothing else.
#[tokio::test]
async fn an_outgoing_message_to_an_unknown_contact_or_group_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(910).with_contacts(&[PEER]).with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let to_stranger = w.author.outgoing(
        &Conv::contact(STRANGER),
        &Msg::text([1; 8], START_MS, "synthetic nobody").nonce([1; 24]),
    );
    let to_lost_group = w.author.outgoing(
        &Conv::group(PEER, GROUP2),
        &Msg::text([2; 8], START_MS, "synthetic nowhere")
            .in_group(PEER, GROUP2)
            .nonce([2; 24]),
    );
    w.reflect_acked(&[to_stranger]).await;
    w.reflect_acked(&[to_lost_group]).await;
    assert_eq!(dropped_by(&warns, "contact-unknown"), 1);
    assert_eq!(dropped_by(&warns, "group-unknown"), 1);
    w.only_nonces(2, 2).await;
    w.h.finish().await;
}

#[test]
fn a_second_envelope_with_a_known_message_id_is_shipped_again() {
    support::offline::run_child(
        module_path!(),
        "a_second_envelope_with_a_known_message_id_is_shipped_again_child",
    );
}

/// `TC/reflected-incoming-message.ts`: a message whose id the conversation already has is discarded
/// ("already received"). KOURIER DIFFERS: its backend keeps no list of message ids; the replay check is the
/// NONCE store (the same bytes again are acked with nothing new, `reflect::...`), so a second envelope with
/// the same message id but a fresh nonce is a second `Message` event with the same `local`, and the core's
/// idempotent ingest (`docs/architecture.md` 5.6: "core ingest is idempotent"; the key lookup of
/// `kourier-core/src/ingest/message.rs` drops a duplicate) makes it one row. This test pins that boundary
/// behaviour so a change to it is a decision.
#[tokio::test]
async fn a_second_envelope_with_a_known_message_id_is_shipped_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(911).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let first = w.author.incoming(
        PEER,
        &Msg::text([1; 8], START_MS, "synthetic once").nonce([1; 24]),
    );
    let again = w.author.incoming(
        PEER,
        &Msg::text([1; 8], START_MS, "synthetic once").nonce([2; 24]),
    );
    w.reflect_acked(&[first]).await;
    w.reflect_acked(&[again]).await;
    let got = w.events(2).await;
    assert_eq!(message(&got[0]).local, message(&got[1]).local);
    assert_eq!(message(&got[0]).body, message(&got[1]).body);
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 2, 2));
    w.h.finish().await;
}

// --- where Kourier differs on purpose -----------------------------------------------------------------

#[test]
fn a_text_into_an_archived_chat_brings_it_back_in_the_same_row() {
    support::offline::run_child(
        module_path!(),
        "a_text_into_an_archived_chat_brings_it_back_in_the_same_row_child",
    );
}

/// H5 (his QC, `th5-decisions.md`): Desktop's `:93` stores the message and nothing else (the CSP receiver
/// unarchives and reflects a sync); Kourier's first event of the row is `ConversationUpsert { archived:
/// Some(false) }`, then the `Message`, and the stored contact is NORMAL when the core gets the row.
#[tokio::test]
async fn a_text_into_an_archived_chat_brings_it_back_in_the_same_row_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(912).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let archive = w.author.contact_update(ContactDelta::new(PEER).archived());
    w.reflect(&[archive]);
    let got = w.events(2).await;
    assert!(
        matches!(&got[1], BackendEvent::ConversationUpsert(p) if p.archived == Some(true)),
        "the chat was not stated archived"
    );
    let text = w.author.incoming(
        PEER,
        &Msg::text([1; 8], START_MS, "synthetic back").nonce([1; 24]),
    );
    w.reflect(&[text]);
    let got = w.events(2).await;
    assert!(
        got[0] == unarchive(ConvKey::Direct(user(PEER))),
        "the bring-back is not first"
    );
    assert_eq!(message(&got[1]).body, Body::text("synthetic back"));
    assert_eq!(
        stored_contact(&w.h, PEER)
            .expect("stored")
            .conversation_visibility,
        Some(d2d_sync::ConversationVisibility::Normal as i32)
    );
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn a_text_from_a_hidden_contact_states_its_chat_first() {
    support::offline::run_child(
        module_path!(),
        "a_text_from_a_hidden_contact_states_its_chat_first_child",
    );
}

/// E5 (the lead's call after A7b): Desktop's `:93` has the conversation from the contact's creation; here a
/// contact the phone HID has no stated chat, so the message's row states the whole 1:1 patch (Threema's
/// features) FIRST and then the `Message`.
#[tokio::test]
async fn a_text_from_a_hidden_contact_states_its_chat_first_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(913).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let hide = w.author.contact_update(ContactDelta::new(PEER).hidden());
    w.reflect(&[hide]);
    let got = w.events(1).await;
    assert!(matches!(&got[0], BackendEvent::ContactUpsert(_)));
    let text = w.author.incoming(
        PEER,
        &Msg::text([1; 8], START_MS, "synthetic hidden").nonce([1; 24]),
    );
    w.reflect(&[text]);
    let got = w.events(2).await;
    assert!(
        got[0] == direct(PEER, plain),
        "a hidden contact's message does not state its chat first"
    );
    assert_eq!(message(&got[1]).body, Body::text("synthetic hidden"));
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn an_ephemeral_typing_is_not_acked_and_one_without_the_flag_is() {
    support::offline::run_child(
        module_path!(),
        "an_ephemeral_typing_is_not_acked_and_one_without_the_flag_is_child",
    );
}

/// Desktop's `d2m/reflected.ts:110-132` acks EVERY reflected
/// envelope (premise 3 of the brief). The spec says an ephemeral one must not be acked; Kourier follows the
/// spec (C5, M5): the EPHEMERAL flag decides the ack, the kind decides the event. The flagged typing is
/// neither acked nor written and reaches the core as ONE ephemeral `Typing` (TH5b B3, R12); the same typing
/// without the flag is acked, writes its D2D nonce and states nothing.
#[tokio::test]
async fn an_ephemeral_typing_is_not_acked_and_one_without_the_flag_is_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(914).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let ephemeral = w.author.incoming(PEER, &Msg::typing([1; 8], START_MS, 1));
    let flagless = w.author.incoming(PEER, &Msg::typing([2; 8], START_MS, 1));
    let phone = &mut w.phone;
    w.h.net().act(|_| {
        phone.reflect_envelope(&ephemeral);
        phone.reflect_with(&flagless, 0);
    });
    let acks = wait_acks(&w.h, 1).await;
    assert!(acks[0].known);
    let typing = w.h.envelope().await;
    assert!(
        typing.ack.is_none()
            && matches!(
                typing.events.as_slice(),
                [BackendEvent::Typing { on: true, .. }]
            ),
        "the flagged typing is not one ephemeral Typing"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 1, 0), "the ephemeral one wrote a nonce");
    w.h.finish().await;
}

// --- group control: Desktop applies it, Kourier takes the group from the GroupSync -------------------

/// The group control message `ty` from `from` for `(creator, group)` as Desktop's specs build it.
fn control(ty: u8, creator: &str, group: u64, inner: &[u8]) -> Msg {
    let body = if matches!(ty, ty::GROUP_LEAVE) {
        body::group_member_container(creator, group, inner)
    } else {
        body::group_creator_container(group, inner)
    };
    Msg::raw([0; 8], START_MS, ty, body)
}

#[test]
fn reflected_group_names_are_dropped_and_the_group_sync_renames() {
    support::offline::run_child(
        module_path!(),
        "reflected_group_names_are_dropped_and_the_group_sync_renames_child",
    );
}

/// Desktop `reflected-group-name.spec.ts:41` (incoming, we are not the creator) and `:87` (outgoing, we ARE
/// the creator): the reflected GROUP_NAME renames the group. Kourier (premise 6): the unversioned reflection
/// of Desktop's specs is dropped with the `group-control-desync` WARN, a V0.3 one quietly, the group keeps
/// its name and nothing reaches the core; the `GroupSync.Update` that carries the name gives the
/// `ConversationUpsert` with the new title and the `TitleChanged` notice (`by` = the creator, `None` when it
/// is us).
#[tokio::test]
async fn reflected_group_names_are_dropped_and_the_group_sync_renames_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(915)
        .with_contacts(&[PEER, PEER2])
        .with_group(PEER, GROUP)
        .with_group(ME, GROUP2);
    let mut w = W::new(s).await;
    // :41 incoming from the creator.
    let mut name = control(ty::GROUP_NAME, PEER, GROUP, b"synthetic renamed");
    name.id = [1; 8];
    name.nonces = vec![[1; 24]];
    let old_phone = w.author.unversioned();
    let versioned = w.author;
    w.reflect_acked(&[old_phone.incoming(PEER, &name)]).await;
    assert_eq!(dropped_by(&warns, "group-control-desync"), 1);
    name.id = [2; 8];
    name.nonces = vec![[2; 24]];
    w.reflect_acked(&[versioned.incoming(PEER, &name)]).await;
    assert_eq!(
        dropped_by(&warns, "group-control-desync"),
        1,
        "a V0.3 phone's group control is not the desync case"
    );
    w.only_nonces(2, 2).await;
    assert_eq!(
        stored_group(&w.h, PEER, GROUP)
            .expect("stored")
            .name
            .as_deref(),
        Some("synthetic group"),
        "a reflected GROUP_NAME renamed the group"
    );
    let update =
        versioned.group_update(GroupDelta::new(PEER, GROUP).name("synthetic renamed"), &[]);
    w.reflect(&[update]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic renamed", &[ME]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::TitleChanged {
            by: Some(user(PEER)),
            title: "synthetic renamed".to_owned()
        }
    );
    // :87 outgoing, we are the creator.
    let mut own = control(ty::GROUP_NAME, ME, GROUP2, b"synthetic ours");
    own.id = [3; 8];
    own.nonces = vec![[3; 24]];
    w.reflect_acked(&[old_phone.outgoing(&Conv::group(ME, GROUP2), &own)])
        .await;
    assert_eq!(dropped_by(&warns, "group-control-desync"), 2);
    assert_eq!(
        stored_group(&w.h, ME, GROUP2)
            .expect("stored")
            .name
            .as_deref(),
        Some("synthetic group")
    );
    let update = versioned.group_update(GroupDelta::new(ME, GROUP2).name("synthetic ours"), &[]);
    w.reflect(&[update]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(ME, GROUP2, "synthetic ours", &[]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::TitleChanged {
            by: None,
            title: "synthetic ours".to_owned()
        }
    );
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn a_reflected_group_setup_is_dropped_and_the_group_sync_creates() {
    support::offline::run_child(
        module_path!(),
        "a_reflected_group_setup_is_dropped_and_the_group_sync_creates_child",
    );
}

/// Desktop `group-setup.ts:70` (a new group from a setup that lists a member and us) and `:181` (duplicate
/// IDs, the creator and us listed as members): the setup creates the group with the members once each, the
/// creator not among them. Kourier: the reflected GROUP_SETUP is dropped (`group-control-desync`, nothing
/// for the core, no group row); the `GroupSync.Create` creates it: ONE `ConversationUpsert` (no notice: a
/// Create shows none) with the creator `Admin`, then the member and us, each once, sorted, even when the
/// phone's list names the creator, us and a member twice. Desktop's third rule there (a revoked contact is
/// left out, from its directory) has no counterpart: the phone's sync is the truth.
#[tokio::test]
async fn a_reflected_group_setup_is_dropped_and_the_group_sync_creates_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(916).with_contacts(&[PEER, PEER2]);
    let mut w = W::new(s).await;
    let old_phone = w.author.unversioned();
    let phone = w.author;
    let list = [PEER2.as_bytes(), ME.as_bytes()].concat();
    let mut setup = control(ty::GROUP_SETUP, PEER, GROUP, &list);
    setup.id = [1; 8];
    setup.nonces = vec![[1; 24]];
    w.reflect_acked(&[old_phone.incoming(PEER, &setup)]).await;
    assert_eq!(dropped_by(&warns, "group-control-desync"), 1);
    w.only_nonces(1, 1).await;
    assert!(
        stored_group(&w.h, PEER, GROUP).is_none(),
        "a setup created a group"
    );
    // :70
    let create = phone.group_create(GroupDelta::create(
        PEER,
        GROUP,
        "synthetic new",
        &[PEER2],
        START_MS,
    ));
    w.reflect(&[create]);
    let got = w.events(1).await;
    assert!(
        got[0] == group_ev(PEER, GROUP, "synthetic new", &[PEER2, ME]),
        "the created group differs"
    );
    // :181 duplicates, the creator and us in the list.
    let dup = phone.group_create(GroupDelta::create(
        PEER,
        GROUP2,
        "synthetic dup",
        &[PEER, PEER2, ME, PEER2, ME],
        START_MS,
    ));
    w.reflect(&[dup]);
    let got = w.events(1).await;
    assert!(
        got[0] == group_ev(PEER, GROUP2, "synthetic dup", &[PEER2, ME]),
        "duplicates or the creator were stated as members"
    );
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn group_setup_states_come_through_the_group_sync_with_notices() {
    support::offline::run_child(
        module_path!(),
        "group_setup_states_come_through_the_group_sync_with_notices_child",
    );
}

/// Desktop `group-setup.ts:261` (a setup that adds a member to a group we are in: the member list grows,
/// the creation date stays), `:347` (a setup that lists nobody: we are KICKED, the member list stays) and
/// `:420` (a setup that lists us again after we LEFT: MEMBER again). Kourier: each setup is dropped
/// (`group-control-desync`), and the `GroupSync.Update` carries the change: members replaced / the user's
/// state, the notices (`MembersAdded`; `MembersRemoved` with us; `MembersAdded` with us), the stored BLOB's
/// member list left as the phone sent it and `created_at` untouched.
#[tokio::test]
async fn group_setup_states_come_through_the_group_sync_with_notices_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(917)
        .with_contacts(&[PEER, PEER2])
        .with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let old_phone = w.author.unversioned();
    let phone = w.author;
    let by = Some(user(PEER));
    let setup = |id: u8, list: &[u8]| {
        let mut m = control(ty::GROUP_SETUP, PEER, GROUP, list);
        m.id = [id; 8];
        m.nonces = vec![[id; 24]];
        old_phone.incoming(PEER, &m)
    };
    // :261 a member is added.
    w.reflect_acked(&[setup(1, &[PEER2.as_bytes(), ME.as_bytes()].concat())])
        .await;
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).members(&[PEER2]),
        &[(PEER2, MemberChange::Added)],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic group", &[PEER2, ME]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::MembersAdded {
            by: by.clone(),
            users: vec![user(PEER2)]
        }
    );
    let stored = stored_group(&w.h, PEER, GROUP).expect("stored");
    assert_eq!(members_of(&stored), vec![PEER2.to_owned()]);
    assert_eq!(
        stored.created_at,
        Some(START_MS),
        "the creation date changed"
    );
    assert_eq!(user_state(&stored), STATE_MEMBER);
    // :347 we are removed: the member list stays.
    w.reflect_acked(&[setup(2, &[])]).await;
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).user_state(d2d_sync::group::UserState::Kicked),
        &[],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic group", &[PEER2]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::MembersRemoved {
            by: by.clone(),
            users: vec![user(ME)]
        }
    );
    let stored = stored_group(&w.h, PEER, GROUP).expect("stored");
    assert_eq!(user_state(&stored), STATE_KICKED);
    assert_eq!(
        members_of(&stored),
        vec![PEER2.to_owned()],
        "the member list changed"
    );
    assert_eq!(stored.created_at, Some(START_MS));
    // :420 we are added again (Desktop starts from LEFT; the notice is the same).
    w.reflect_acked(&[setup(3, &[PEER2.as_bytes(), ME.as_bytes()].concat())])
        .await;
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).user_state(d2d_sync::group::UserState::Member),
        &[],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic group", &[PEER2, ME]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::MembersAdded {
            by,
            users: vec![user(ME)]
        }
    );
    let stored = stored_group(&w.h, PEER, GROUP).expect("stored");
    assert_eq!(user_state(&stored), STATE_MEMBER);
    assert_eq!(stored.created_at, Some(START_MS));
    assert_eq!(dropped_by(&warns, "group-control-desync"), 3);
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn a_reflected_group_leave_is_dropped_and_sends_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_reflected_group_leave_is_dropped_and_sends_nothing_child",
    );
}

/// Desktop `group-leave.ts:68` (the creator "leaves" its own group), `:85` (a leave for an unknown group of
/// ours), `:102` (an unknown group of someone else: a LEADER sends a group sync request, in `d2d` mode
/// nothing is expected) and `reflected-outgoing-group-leave.spec.ts:56` (our leave from an unknown group),
/// `:69` (our leave from a group we were KICKED from: the state stays). Kourier: every reflected
/// GROUP_LEAVE is dropped by its type (`group-control-desync`, unversioned), nothing for the core; a follower
/// reflects and sends nothing on the wire (no `reflect`, no CSP outgoing message); the KICKED group's stored
/// state stays KICKED.
#[tokio::test]
async fn a_reflected_group_leave_is_dropped_and_sends_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(918)
        .with_contacts(&[PEER, PEER2])
        .with_group(PEER, GROUP)
        .with_group(ME, GROUP2);
    let mut w = W::new(s).await;
    let phone = w.author;
    let old_phone = phone.unversioned();
    let conn = w.h.conn();
    let leave = |id: u8, creator: &str, group: u64| {
        let mut m = control(ty::GROUP_LEAVE, creator, group, &[]);
        m.id = [id; 8];
        m.nonces = vec![[id; 24]];
        m
    };
    // :68 the creator leaves its own group.
    w.reflect_acked(&[old_phone.incoming(PEER, &leave(1, PEER, GROUP))])
        .await;
    // :85 a group we created that the file lacks.
    w.reflect_acked(&[old_phone.incoming(PEER2, &leave(2, ME, GROUP2 + 1))])
        .await;
    // :102 a group of someone else that the file lacks.
    w.reflect_acked(&[old_phone.incoming(PEER2, &leave(3, PEER, GROUP2 + 2))])
        .await;
    // outgoing :56 from an unknown group.
    w.reflect_acked(&[
        old_phone.outgoing(&Conv::group(PEER, GROUP2 + 2), &leave(4, PEER, GROUP2 + 2))
    ])
    .await;
    assert_eq!(dropped_by(&warns, "group-control-desync"), 4);
    w.only_nonces(4, 4).await;
    // outgoing :69, KICKED first (the phone's sync; the group has a member, so it stays shown), then our leave.
    w.reflect(&[phone.group_update(GroupDelta::new(PEER, GROUP).members(&[PEER2]), &[])]);
    w.events(1).await;
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).user_state(d2d_sync::group::UserState::Kicked),
        &[],
    )]);
    w.events(2).await;
    w.reflect_acked(&[old_phone.outgoing(&Conv::group(PEER, GROUP), &leave(5, PEER, GROUP))])
        .await;
    assert_eq!(
        user_state(&stored_group(&w.h, PEER, GROUP).expect("stored")),
        STATE_KICKED,
        "a dropped leave changed the state"
    );
    w.quiet().await;
    assert_eq!(
        traced(&w.h.s.world, conn, "in", "reflect"),
        0,
        "a follower reflected"
    );
    assert_eq!(
        traced(&w.h.s.world, conn, "in", "csp-outgoing-message"),
        0,
        "a follower sent a CSP message (a group sync request)"
    );
    w.h.finish().await;
}

#[test]
fn a_leave_reaches_the_core_through_the_group_sync() {
    support::offline::run_child(
        module_path!(),
        "a_leave_reaches_the_core_through_the_group_sync_child",
    );
}

/// Desktop `group-leave.ts:212` (a member's leave removes it from the local group; the same member again
/// changes nothing), `reflected-outgoing-group-leave.spec.ts:96` (our leave from a group we are in: LEFT) and
/// `:123` (we created the group: LEFT, members kept). Kourier takes each from the `GroupSync.Update`: a
/// `LEFT` change of a stored member gives `MemberLeft` and the list without it; the same member again gives
/// the `ConversationUpsert` alone; our `user_state` `LEFT` gives `MemberLeft` for us and the stored state
/// LEFT with the member list kept, for a group of someone else and for our own.
#[tokio::test]
async fn a_leave_reaches_the_core_through_the_group_sync_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(919)
        .with_contacts(&[PEER, PEER2])
        .with_group(PEER, GROUP)
        .with_group(ME, GROUP2)
        .with_group(PEER, GROUP3);
    let mut w = W::new(s).await;
    let phone = w.author;
    // The members of the three groups.
    w.reflect(&[
        phone.group_update(GroupDelta::new(PEER, GROUP).members(&[PEER2]), &[]),
        phone.group_update(GroupDelta::new(ME, GROUP2).members(&[PEER, PEER2]), &[]),
        phone.group_update(GroupDelta::new(PEER, GROUP3).members(&[PEER2]), &[]),
    ]);
    w.events(3).await;
    // :212 a member leaves; then the same member again.
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).members(&[]),
        &[(PEER2, MemberChange::Left)],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic group", &[ME]));
    assert_eq!(
        *notice(&got[1]),
        SystemEvent::MemberLeft { user: user(PEER2) }
    );
    assert!(members_of(&stored_group(&w.h, PEER, GROUP).expect("stored")).is_empty());
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).members(&[]),
        &[(PEER2, MemberChange::Left)],
    )]);
    let got = w.events(1).await;
    assert!(
        got[0] == group_ev(PEER, GROUP, "synthetic group", &[ME]),
        "a leave of a non-member changed the members"
    );
    // outgoing :96 we leave a group of someone else that has a member: LEFT, the member list kept, and `MemberLeft` for us.
    w.reflect(&[phone.group_update(
        GroupDelta::new(PEER, GROUP3).user_state(d2d_sync::group::UserState::Left),
        &[],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(PEER, GROUP3, "synthetic group", &[PEER2]));
    assert_eq!(*notice(&got[1]), SystemEvent::MemberLeft { user: user(ME) });
    let stored = stored_group(&w.h, PEER, GROUP3).expect("stored");
    assert_eq!(user_state(&stored), STATE_LEFT);
    assert_eq!(members_of(&stored), vec![PEER2.to_owned()]);
    // M1 (Kourier's rule), round A M-1: the same leave from the group that has nobody left is not shown
    // any more, and NOTHING is stated for it (never `ConversationRemoved`, which would delete the chat's
    // history on the core's side while the phone keeps it): the zone commits and the next envelope's
    // events are the first the core hears (a stray `ConversationRemoved` would be `got[0]` below).
    w.reflect_acked(&[phone.group_update(
        GroupDelta::new(PEER, GROUP).user_state(d2d_sync::group::UserState::Left),
        &[],
    )])
    .await;
    assert_eq!(
        user_state(&stored_group(&w.h, PEER, GROUP).expect("stored")),
        STATE_LEFT
    );
    // outgoing :123 we created the group and disband it: LEFT, the members stay.
    w.reflect(&[phone.group_update(
        GroupDelta::new(ME, GROUP2).user_state(d2d_sync::group::UserState::Left),
        &[],
    )]);
    let got = w.events(2).await;
    assert!(got[0] == group_ev(ME, GROUP2, "synthetic group", &[PEER, PEER2]));
    assert_eq!(*notice(&got[1]), SystemEvent::MemberLeft { user: user(ME) });
    let stored = stored_group(&w.h, ME, GROUP2).expect("stored");
    assert_eq!(user_state(&stored), STATE_LEFT);
    let mut members = members_of(&stored);
    members.sort();
    assert_eq!(
        members,
        vec![PEER2.to_owned(), PEER.to_owned()],
        "sorted by ID"
    );
    w.quiet().await;
    w.h.finish().await;
}

// --- contact and group syncs: what Desktop discards ---------------------------------------------------

#[test]
fn contact_syncs_create_update_and_discard_as_kourier_does() {
    support::offline::run_child(
        module_path!(),
        "contact_syncs_create_update_and_discard_as_kourier_does_child",
    );
}

/// `TC/reflected-contact-sync.ts`: a `create` of an existing contact is discarded ("contact already
/// exists"); an `update` of an unknown contact is discarded. Kourier: the Create of a STORED contact is an
/// UPDATE (its nickname is applied, the stored key stays: H6), an `Update` of an unknown contact WITHOUT a
/// key is dropped (`contact-unknown`, one WARN, a nonce, no event) and WITH a 32-byte key creates it
/// (`briefs/th5.md` 2.6); a contact created this way states the contact AND its 1:1 chat at once (E1).
#[tokio::test]
async fn contact_syncs_create_update_and_discard_as_kourier_does_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(920).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let phone = w.author;
    // A create of the stored contact.
    w.reflect(&[phone
        .contact_create(ContactDelta::create(PEER, [9; 32], START_MS).nickname("Synthetic Nick"))]);
    let got = w.events(2).await;
    assert!(
        matches!(&got[0], BackendEvent::ContactUpsert(p) if p.profile_name == Patch::Set("Synthetic Nick".to_owned()))
    );
    assert!(got[1] == direct(PEER, plain));
    let stored = stored_contact(&w.h, PEER).expect("stored");
    assert_eq!(stored.nickname.as_deref(), Some("Synthetic Nick"));
    assert_eq!(
        stored.public_key,
        Some(vec![7; 32]),
        "H6: the stored key stays"
    );
    // An update of an unknown contact without a key.
    w.reflect_acked(&[phone.contact_update(ContactDelta::new(STRANGER).nickname("Nobody"))])
        .await;
    assert_eq!(dropped_by(&warns, "contact-unknown"), 1);
    assert!(stored_contact(&w.h, STRANGER).is_none());
    // An update of an unknown contact with a key creates it, contact and chat together (E1).
    w.reflect(&[phone.contact_update(
        ContactDelta::new(PEER2)
            .public_key([5; 32])
            .nickname("Synthetic Two"),
    )]);
    let got = w.events(2).await;
    assert!(
        matches!(&got[0], BackendEvent::ContactUpsert(p) if p.profile_name == Patch::Set("Synthetic Two".to_owned()))
    );
    assert!(
        got[1] == direct(PEER2, plain),
        "the new contact's chat is not stated at once"
    );
    assert!(stored_contact(&w.h, PEER2).is_some());
    w.quiet().await;
    w.h.finish().await;
}

#[test]
fn a_group_sync_update_or_delete_of_an_unknown_group_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "a_group_sync_update_or_delete_of_an_unknown_group_is_dropped_child",
    );
}

/// `TC/reflected-group-sync.ts`: an `update` for an unknown group is discarded, a `delete` of a group that
/// does not exist is discarded. Kourier: both dropped by `group-unknown`, acked, one WARN each, nothing for
/// the core, nothing in `grp`; the same sync as a Create works (the group appears).
#[tokio::test]
async fn a_group_sync_update_or_delete_of_an_unknown_group_is_dropped_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(921).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let phone = w.author;
    w.reflect_acked(&[
        phone.group_update(GroupDelta::new(PEER, GROUP).name("synthetic lost"), &[]),
        phone.group_delete(PEER, GROUP),
    ])
    .await;
    assert_eq!(dropped_by(&warns, "group-unknown"), 2);
    w.only_nonces(2, 0).await;
    assert!(stored_group(&w.h, PEER, GROUP).is_none());
    w.reflect(&[phone.group_create(GroupDelta::create(
        PEER,
        GROUP,
        "synthetic found",
        &[],
        START_MS,
    ))]);
    let got = w.events(1).await;
    assert!(got[0] == group_ev(PEER, GROUP, "synthetic found", &[ME]));
    w.quiet().await;
    w.h.finish().await;
}

// --- TH5b B6: the eleven cases that waited for the edits, the deletes, the receipts and the polls -------

/// The time `t` (ms of the clock) as the events carry it.
fn ms(t: u64) -> i64 {
    i64::try_from(t).unwrap_or(0)
}

/// A message reference in `conv`: the id as its 16 hex digits (the wire bytes in order), the author as given.
fn target(conv: ConvKey, local: &str, author: TargetAuthor) -> MsgTarget {
    MsgTarget {
        conv: Some(conv),
        author,
        local: local.into(),
        alternates: Vec::new(),
    }
}

#[test]
fn an_incoming_edit_from_a_contact_reaches_the_core_at_its_own_time() {
    support::offline::run_child(
        module_path!(),
        "an_incoming_edit_from_a_contact_reaches_the_core_at_its_own_time_child",
    );
}

/// Ported from Threema Desktop `reflected-message.spec.ts:187`: a text from the contact, then its EDIT. In
/// Desktop the message keeps `createdAt` and `receivedAt` (the reflection's time), takes the new text and a
/// `lastEditedAt` that is the EDIT's `created_at`, not the time it was reflected. Kourier: the text is the first
/// event; the edit is ONE `Edited` in the 1:1 chat that names the text by its id (author unknown: the wire names
/// none), `by` the contact, the new body, `edited_at` = the edit's own `created_at` (R2), which is earlier than the
/// reflection's time. Desktop's spec gives the edit the id of the text; here the edit has an id of its own (the
/// target is the protobuf's `message_id`, as on a real phone).
#[tokio::test]
async fn an_incoming_edit_from_a_contact_reaches_the_core_at_its_own_time_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(922).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let edited = START_MS - 7_000;
    let reflected_at = w.h.now();
    let text = w.author.incoming(
        PEER,
        &Msg::text([1; 8], created, "synthetic hello").nonce([1; 24]),
    );
    let edit = w.author.incoming(
        PEER,
        &Msg::edit([2; 8], edited, [1; 8], "synthetic edited").nonce([2; 24]),
    );
    w.reflect(&[text, edit]);
    let got = w.events(2).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, ConvKey::Direct(user(PEER)));
    assert_eq!(&*m.local, "0101010101010101");
    assert_eq!(m.body, Body::text("synthetic hello"));
    assert_eq!(m.sent_at, ms(created), "createdAt");
    assert_eq!(m.server_at, Some(ms(reflected_at)), "receivedAt");
    assert!(
        got[1]
            == BackendEvent::Edited {
                target: target(
                    ConvKey::Direct(user(PEER)),
                    "0101010101010101",
                    TargetAuthor::Unknown
                ),
                by: Some(user(PEER)),
                new_local: None,
                body: Body::text("synthetic edited"),
                edited_at: ms(edited),
                keeps_quote: None,
            },
        "the second event is not the contact's edit at its own time"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 2, 2));
    w.h.finish().await;
}

#[test]
fn an_incoming_delete_from_a_contact_reaches_the_core_at_its_own_time() {
    support::offline::run_child(
        module_path!(),
        "an_incoming_delete_from_a_contact_reaches_the_core_at_its_own_time_child",
    );
}

/// Ported from Threema Desktop `reflected-message.spec.ts:261`: a text from the contact, then its DELETE. In
/// Desktop the message becomes a `deleted` one with `deletedAt` = the DELETE's `created_at`, `receivedAt` the text's
/// and no edit time. Kourier: the text, then ONE `Deleted` for everyone, `by` the contact, `at` = the delete's own
/// `created_at` (R2: not the reflection's time); the core makes the stand-in. The delete has an id of its own, as
/// in the edit case above.
#[tokio::test]
async fn an_incoming_delete_from_a_contact_reaches_the_core_at_its_own_time_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(923).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let deleted = START_MS - 7_000;
    let text = w.author.incoming(
        PEER,
        &Msg::text([1; 8], created, "synthetic hello").nonce([1; 24]),
    );
    let delete = w
        .author
        .incoming(PEER, &Msg::delete([2; 8], deleted, [1; 8]).nonce([2; 24]));
    w.reflect(&[text, delete]);
    let got = w.events(2).await;
    assert_eq!(message(&got[0]).body, Body::text("synthetic hello"));
    assert_eq!(message(&got[0]).sent_at, ms(created), "createdAt");
    assert!(
        got[1]
            == BackendEvent::Deleted {
                target: target(
                    ConvKey::Direct(user(PEER)),
                    "0101010101010101",
                    TargetAuthor::Unknown
                ),
                by: Some(user(PEER)),
                for_everyone: true,
                at: ms(deleted),
            },
        "the second event is not the contact's delete at its own time"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 2, 2));
    w.h.finish().await;
}

#[test]
fn an_outgoing_edit_reaches_the_core_with_no_author_at_its_own_time() {
    support::offline::run_child(
        module_path!(),
        "an_outgoing_edit_reaches_the_core_with_no_author_at_its_own_time_child",
    );
}

/// Ported from Threema Desktop `reflected-message.spec.ts:660`: our phone sends a text to the contact and then
/// edits it. In Desktop the message is OUTBOUND, keeps its `createdAt` and has `lastEditedAt` = the edit's
/// `created_at`. Kourier: the text is the message of our other device (`Sending`), the edit is ONE `Edited` with
/// `by: None` (it is us) at the edit's own `created_at`. Desktop's spec dates the edit BEFORE the text (it takes
/// `new Date()` for the text and "7 seconds ago" for the edit); here the edit is later, which is what a phone does.
#[tokio::test]
async fn an_outgoing_edit_reaches_the_core_with_no_author_at_its_own_time_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(924).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let edited = START_MS - 7_000;
    let to_peer = Conv::contact(PEER);
    let text = w.author.outgoing(
        &to_peer,
        &Msg::text([3; 8], created, "synthetic out").nonce([3; 24]),
    );
    let edit = w.author.outgoing(
        &to_peer,
        &Msg::edit([4; 8], edited, [3; 8], "synthetic out edited").nonce([4; 24]),
    );
    w.reflect(&[text, edit]);
    let got = w.events(2).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, ConvKey::Direct(user(PEER)));
    assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(m.body, Body::text("synthetic out"));
    assert_eq!(m.sent_at, ms(created), "createdAt");
    assert_eq!(
        m.initial_out_state,
        Some(OutgoingState::Sending { progress: None })
    );
    assert!(
        got[1]
            == BackendEvent::Edited {
                target: target(
                    ConvKey::Direct(user(PEER)),
                    "0303030303030303",
                    TargetAuthor::Unknown
                ),
                by: None,
                new_local: None,
                body: Body::text("synthetic out edited"),
                edited_at: ms(edited),
                keeps_quote: None,
            },
        "the second event is not our edit at its own time"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 2, 2));
    w.h.finish().await;
}

#[test]
fn an_outgoing_delete_reaches_the_core_with_no_author_at_its_own_time() {
    support::offline::run_child(
        module_path!(),
        "an_outgoing_delete_reaches_the_core_with_no_author_at_its_own_time_child",
    );
}

/// Ported from Threema Desktop `reflected-message.spec.ts:737`: our phone sends a text to the contact and then
/// deletes it. In Desktop the message is OUTBOUND and `deleted` with `deletedAt` = the delete's `created_at` and no
/// edit time. Kourier: the text, then ONE `Deleted` for everyone with `by: None` at the delete's own `created_at`.
/// The delete is later than the text here (Desktop's spec dates it before; see the edit case above).
#[tokio::test]
async fn an_outgoing_delete_reaches_the_core_with_no_author_at_its_own_time_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(925).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let created = START_MS - 20_000;
    let deleted = START_MS - 7_000;
    let to_peer = Conv::contact(PEER);
    let text = w.author.outgoing(
        &to_peer,
        &Msg::text([3; 8], created, "synthetic out").nonce([3; 24]),
    );
    let delete = w.author.outgoing(
        &to_peer,
        &Msg::delete([4; 8], deleted, [3; 8]).nonce([4; 24]),
    );
    w.reflect(&[text, delete]);
    let got = w.events(2).await;
    let m = message(&got[0]);
    assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(m.body, Body::text("synthetic out"));
    assert_eq!(m.sent_at, ms(created), "createdAt");
    assert!(
        got[1]
            == BackendEvent::Deleted {
                target: target(
                    ConvKey::Direct(user(PEER)),
                    "0303030303030303",
                    TargetAuthor::Unknown
                ),
                by: None,
                for_everyone: true,
                at: ms(deleted),
            },
        "the second event is not our delete at its own time"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 2, 2));
    w.h.finish().await;
}

// --- reflected-incoming-delivery-receipt.spec.ts :73, :137 --------------------------------------------

#[test]
fn the_contacts_receipts_for_our_message_reach_the_core() {
    support::offline::run_child(
        module_path!(),
        "the_contacts_receipts_for_our_message_reach_the_core_child",
    );
}

/// Ported from Threema Desktop `reflected-incoming-delivery-receipt.spec.ts:73`: three receipts of the contact for
/// a message of OURS: received (status 1), read (2) and declined (4). Desktop marks the message delivered at the
/// receipt's time, read at the receipt's time and adds a thumbs-down reaction at the receipt's time. Kourier: the
/// message, then `Receipts { Delivered }` and `Receipts { Read }` by the contact on OUR message
/// (`TargetAuthor::Own`) at each receipt's own `created_at` (R2), then the legacy pair of the declined one (C13):
/// "remove every reaction at `created_at - 1`", then the thumbs-down at `created_at`, by the contact on OUR message
/// (`TargetAuthor::Own`, P-L2: the spec's receipt steps 1.3 bind it). DIFFERENCE IN THE INPUT: Desktop's spec runs its task directly and passes OUR identity as the
/// reacting sender (so its reaction is ours); the incoming path passes the contact's, and so does this port.
#[tokio::test]
async fn the_contacts_receipts_for_our_message_reach_the_core_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(926).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let author = w.author;
    let created = |n: u64| START_MS - 3_600_000 + n;
    let ours = author.outgoing(
        &Conv::contact(PEER),
        &Msg::text([6; 8], created(0), "synthetic ours").nonce([6; 24]),
    );
    let receipt = |n: u8, status: u8| {
        author.incoming(
            PEER,
            &Msg::receipt([n; 8], created(u64::from(n)), status, &[[6; 8]]).nonce([n; 24]),
        )
    };
    w.reflect(&[ours, receipt(1, 1), receipt(2, 2), receipt(3, 4)]);
    let got = w.events(5).await;
    let chat = ConvKey::Direct(user(PEER));
    assert_eq!(
        message(&got[0]).origin,
        Origin::OwnOtherDevice { device: None }
    );
    assert_eq!(message(&got[0]).body, Body::text("synthetic ours"));
    let receipts = |kind: ReceiptKind, n: u64| BackendEvent::Receipts {
        targets: vec![target(chat.clone(), "0606060606060606", TargetAuthor::Own)],
        by: user(PEER),
        kind,
        at: ms(created(n)),
    };
    assert!(
        got[1] == receipts(ReceiptKind::Delivered, 1),
        "the received receipt is not Delivered at its own time"
    );
    assert!(
        got[2] == receipts(ReceiptKind::Read, 2),
        "the read receipt is not Read at its own time"
    );
    // P-L2: the peer reacts to OUR message (the spec's receipt steps 1.3 bind it to us).
    let thumb = target(chat.clone(), "0606060606060606", TargetAuthor::Own);
    assert!(
        got[3]
            == BackendEvent::Reaction {
                target: thumb.clone(),
                by: Some(user(PEER)),
                emoji: String::new(),
                remove: true,
                at: ms(created(3)) - 1,
            },
        "the declined receipt does not first take the contact's reactions away"
    );
    assert!(
        got[4]
            == BackendEvent::Reaction {
                target: thumb,
                by: Some(user(PEER)),
                emoji: "\u{1f44e}".to_owned(),
                remove: false,
                at: ms(created(3)),
            },
        "the declined receipt is not the thumbs-down at its own time"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 4, 4), "not four nonces of each, no row");
    assert!(
        !warns.at(Level::WARN).iter().any(|w| w.contains("receipt")),
        "a receipt was warned"
    );
    w.h.finish().await;
}

#[test]
fn our_phones_receipts_for_the_contacts_message_reach_the_core() {
    support::offline::run_child(
        module_path!(),
        "our_phones_receipts_for_the_contacts_message_reach_the_core_child",
    );
}

/// Ported from Threema Desktop `reflected-incoming-delivery-receipt.spec.ts:137`: OUR phone's receipts for a
/// message of the contact: received (1), read (2), acknowledged (3) and declined (4). Desktop ignores "received" for
/// an incoming message, marks it read at the receipt's time, adds a thumbs-up and then REPLACES it by a thumbs-down
/// (one reaction in the end, ours). Kourier: the message, nothing at all for the received receipt (acked, its
/// nonces written, no WARN), `Receipts { Read }` by us on a message of unknown author, then for each of 3 and 4 the
/// legacy pair (C13) with `by: None`: "remove every reaction at `created_at - 1`", then the emoji at `created_at`,
/// on the contact's message (`TargetAuthor::User(contact)`, P-L2: the outgoing receipt steps 1.3 bind it to the peer).
/// The one reaction in the end is the core's result of the two pairs (a removal with no emoji takes away every
/// reaction of `by` not newer than it); this test asserts the events the backend gives.
#[tokio::test]
async fn our_phones_receipts_for_the_contacts_message_reach_the_core_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(927).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let author = w.author;
    let created = |n: u64| START_MS - 3_600_000 + n;
    let theirs = author.incoming(
        PEER,
        &Msg::text([7; 8], created(0), "synthetic theirs").nonce([7; 24]),
    );
    let to_peer = Conv::contact(PEER);
    let receipt = |n: u8, status: u8| {
        author.outgoing(
            &to_peer,
            &Msg::receipt([n; 8], created(u64::from(n)), status, &[[7; 8]]).nonce([n; 24]),
        )
    };
    w.reflect(&[
        theirs,
        receipt(1, 1),
        receipt(2, 2),
        receipt(3, 3),
        receipt(4, 4),
    ]);
    let got = w.events(6).await;
    let chat = ConvKey::Direct(user(PEER));
    assert_eq!(message(&got[0]).body, Body::text("synthetic theirs"));
    assert!(
        got[1]
            == BackendEvent::Receipts {
                targets: vec![target(
                    chat.clone(),
                    "0707070707070707",
                    TargetAuthor::Unknown
                )],
                by: user(ME),
                kind: ReceiptKind::Read,
                at: ms(created(2)),
            },
        "our read receipt is not Read by us at its own time"
    );
    // P-L2: our phone reacts to the PEER's message (the outgoing receipt steps 1.3 bind it to the peer).
    let on = target(chat, "0707070707070707", TargetAuthor::User(user(PEER)));
    for (i, (n, emoji)) in [(3, "\u{1f44d}"), (4, "\u{1f44e}")].into_iter().enumerate() {
        assert!(
            got[2 + 2 * i]
                == BackendEvent::Reaction {
                    target: on.clone(),
                    by: None,
                    emoji: String::new(),
                    remove: true,
                    at: ms(created(n)) - 1,
                },
            "receipt {n} does not first take our reactions away"
        );
        assert!(
            got[3 + 2 * i]
                == BackendEvent::Reaction {
                    target: on.clone(),
                    by: None,
                    emoji: emoji.to_owned(),
                    remove: false,
                    at: ms(created(n)),
                },
            "receipt {n} is not our reaction at its own time"
        );
    }
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 5, 5), "not five nonces of each, no row");
    assert!(
        !warns.at(Level::WARN).iter().any(|w| w.contains("receipt")),
        "our own received receipt was warned (it is quiet)"
    );
    w.h.finish().await;
}

// --- reflected-message-content-update.spec.ts :129, :175 ------------------------------------------------

/// The group of `:129` and `:175` (the author is the creator, a second contact is a member) with a text of the
/// author in it: the file has the group, the phone's sync gives it the other member, and the text of the author
/// (id `[1; 8]`) is read from the core side before the test goes on.
async fn a_group_text_of_its_author(seed: u64) -> W {
    let s = scene(seed)
        .with_contacts(&[PEER, PEER2])
        .with_group(PEER, GROUP);
    let mut w = W::new(s).await;
    let phone = w.author;
    w.reflect(&[phone.group_update(GroupDelta::new(PEER, GROUP).members(&[PEER2]), &[])]);
    let got = w.events(1).await;
    assert!(
        got[0] == group_ev(PEER, GROUP, "synthetic group", &[PEER2, ME]),
        "the group does not hold the second member"
    );
    let created = START_MS - 20_000;
    let original = phone.incoming(
        PEER,
        &Msg::text([1; 8], created, "synthetic original")
            .in_group(PEER, GROUP)
            .nonce([1; 24]),
    );
    w.reflect(&[original]);
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, group_key_text(PEER, GROUP));
    assert_eq!(
        m.origin,
        Origin::Remote {
            author: user(PEER),
            device: None
        }
    );
    assert_eq!(m.body, Body::text("synthetic original"));
    w
}

#[test]
fn an_edit_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse() {
    support::offline::run_child(
        module_path!(),
        "an_edit_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse_child",
    );
}

/// Ported from Threema Desktop `reflected-message-content-update.spec.ts:129`: the author sends a group text, another
/// member of the group sends an EDIT that names the author's message; the edit must be ignored (the text and
/// `lastEditedAt` unchanged). KOURIER DIFFERS ON PURPOSE (the core, not the backend, checks a modifier's target): the
/// backend keeps no message table and the wire names no author in an edit, so it states the edit as the other
/// member's (`by: Some(<that member>)`, the target's author unknown), and the CORE refuses it because `by` is not
/// the author of the row it finds (`kourier-core/src/ingest/modifier.rs` `edit`: the author column of `by` against
/// the row's). The test asserts the event, whose `by` is the other member while the original's author (asserted
/// when the text is read) is the creator; the user sees the same as in Desktop (the text unchanged).
#[tokio::test]
async fn an_edit_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut w = a_group_text_of_its_author(928).await;
    let edited = START_MS - 5_000;
    let edit = w.author.incoming(
        PEER2,
        &Msg::edit([2; 8], edited, [1; 8], "synthetic edited")
            .in_group(PEER, GROUP)
            .nonce([2; 24]),
    );
    w.reflect(&[edit]);
    let got = w.events(1).await;
    let want = BackendEvent::Edited {
        target: target(
            group_key_text(PEER, GROUP),
            "0101010101010101",
            TargetAuthor::Unknown,
        ),
        by: Some(user(PEER2)),
        new_local: None,
        body: Body::text("synthetic edited"),
        edited_at: ms(edited),
        keeps_quote: None,
    };
    assert!(
        got[0] == want,
        "the other member's edit is not stated as the other member's"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 3, 2));
    w.h.finish().await;
}

#[test]
fn a_delete_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse() {
    support::offline::run_child(
        module_path!(),
        "a_delete_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse_child",
    );
}

/// Ported from Threema Desktop `reflected-message-content-update.spec.ts:175`: the same with a DELETE from the other
/// member; the message must stay. KOURIER DIFFERS ON PURPOSE as in the edit case: the backend states `Deleted` with
/// `by: Some(<that member>)` and the CORE refuses it (`ingest/modifier.rs` `delete`: only the author deletes for
/// everyone, or an admin where the chat's features say `admin_delete`; Threema states none).
#[tokio::test]
async fn a_delete_from_a_member_who_is_not_the_author_is_stated_for_the_core_to_refuse_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut w = a_group_text_of_its_author(929).await;
    let deleted = START_MS - 5_000;
    let delete = w.author.incoming(
        PEER2,
        &Msg::delete([2; 8], deleted, [1; 8])
            .in_group(PEER, GROUP)
            .nonce([2; 24]),
    );
    w.reflect(&[delete]);
    let got = w.events(1).await;
    assert!(
        got[0]
            == BackendEvent::Deleted {
                target: target(
                    group_key_text(PEER, GROUP),
                    "0101010101010101",
                    TargetAuthor::Unknown
                ),
                by: Some(user(PEER2)),
                for_everyone: true,
                at: ms(deleted),
            },
        "the other member's delete is not stated as the other member's"
    );
    w.quiet().await;
    assert_eq!(counts(&w.h), (0, 3, 2));
    w.h.finish().await;
}

// --- reflected-poll-update.spec.ts :63, :71, :79 --------------------------------------------------------

/// A poll from the contact and a vote of the contact on it, both reflected: the poll is the one event (an
/// unsupported message with its raw body), the vote waits UNREAD in `deferred` as `poll-vote` at this build's
/// version, byte for byte as sealed, acked, with both nonces written. The fake's poll bodies are opaque filler
/// (`[U: real phone]`: the pinned protocol holds no poll body), so each case sends bodies of its own and the
/// outcome is the same; what is pinned is that the vote is kept WHOLE for the build that maps polls.
async fn a_poll_and_its_vote(seed: u64, poll: Vec<u8>, vote: Vec<u8>) -> W {
    let s = scene(seed).with_contacts(&[PEER]);
    let mut w = W::new(s).await;
    let setup = w.author.incoming(
        PEER,
        &Msg::raw([1; 8], START_MS - 20_000, ty::POLL_SETUP, poll).nonce([1; 24]),
    );
    let vote = w.author.incoming(
        PEER,
        &Msg::raw([2; 8], START_MS - 10_000, ty::POLL_VOTE, vote).nonce([2; 24]),
    );
    let sealed = vote.encode_to_vec();
    w.reflect_acked(&[setup, vote]).await;
    let got = w.events(1).await;
    let m = message(&got[0]);
    assert_eq!(m.conv, ConvKey::Direct(user(PEER)));
    assert_eq!(
        m.body,
        Body::Unsupported {
            kind_hint: "poll".to_owned(),
            summary: None
        }
    );
    assert!(m.raw.is_some(), "the poll lost its raw body");
    w.quiet().await;
    let kept = deferred(&w.h);
    assert_eq!(
        kept.iter()
            .map(|(kind, version, source, _)| (kind.as_str(), *version, source.as_str()))
            .collect::<Vec<_>>(),
        vec![("poll-vote", 2, "reflected")],
        "not the one deferred vote"
    );
    assert!(
        kept[0].3 == sealed,
        "the deferred vote is not the plaintext as sealed"
    );
    assert_eq!(counts(&w.h), (1, 2, 2), "not the nonces of both, one row");
    w
}

#[test]
fn a_vote_with_several_choices_on_a_single_choice_poll_waits_unread() {
    support::offline::run_child(
        module_path!(),
        "a_vote_with_several_choices_on_a_single_choice_poll_waits_unread_child",
    );
}

/// Ported from Threema Desktop `reflected-poll-update.spec.ts:63`: a vote that selects two choices of a SINGLE-choice
/// poll is discarded (nobody's vote is counted). KOURIER DIFFERS ON PURPOSE (R15, R18: poll votes stay deferred in
/// TH5b): the backend reads no vote, so there is nothing to discard and nothing to count; the vote waits in
/// `deferred` unread, acked. The single-choice rule is the job of the build that maps polls, which reads the vote
/// from the kept plaintext; this case becomes its oracle then.
#[tokio::test]
async fn a_vote_with_several_choices_on_a_single_choice_poll_waits_unread_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = a_poll_and_its_vote(930, body::filler(40, 3), body::filler(24, 5)).await;
    w.h.finish().await;
}

#[test]
fn a_vote_with_one_choice_on_a_single_choice_poll_waits_unread() {
    support::offline::run_child(
        module_path!(),
        "a_vote_with_one_choice_on_a_single_choice_poll_waits_unread_child",
    );
}

/// Ported from Threema Desktop `reflected-poll-update.spec.ts:71`: a vote that selects one choice of a single-choice
/// poll is accepted (two votes counted, one selected). KOURIER DIFFERS ON PURPOSE as in `:63` (R15, R18): the vote is
/// not read in TH5b, so nothing is counted; it waits in `deferred`, acked, for the build that maps polls.
#[tokio::test]
async fn a_vote_with_one_choice_on_a_single_choice_poll_waits_unread_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = a_poll_and_its_vote(931, body::filler(40, 4), body::filler(20, 6)).await;
    w.h.finish().await;
}

#[test]
fn a_vote_with_several_choices_on_a_multiple_choice_poll_waits_unread() {
    support::offline::run_child(
        module_path!(),
        "a_vote_with_several_choices_on_a_multiple_choice_poll_waits_unread_child",
    );
}

/// Ported from Threema Desktop `reflected-poll-update.spec.ts:79`: a vote that selects two choices of a
/// MULTIPLE-choice poll is accepted (two votes counted, both selected). KOURIER DIFFERS ON PURPOSE as in `:63`
/// (R15, R18): the vote is not read in TH5b, so nothing is counted; it waits in `deferred`, acked, for the build
/// that maps polls.
#[tokio::test]
async fn a_vote_with_several_choices_on_a_multiple_choice_poll_waits_unread_child() {
    if !support::offline::in_child() {
        return;
    }
    let w = a_poll_and_its_vote(932, body::filler(48, 5), body::filler(28, 7)).await;
    w.h.finish().await;
}
