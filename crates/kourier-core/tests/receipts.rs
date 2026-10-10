//! Receipts count only from someone their message went to (§4.9, §3.5.1; Signal Desktop
//! `MessageReceipts.getTargetMessage`). A receipt from anyone else changes nothing: without
//! the rule, whoever knows or guesses the id of a message could mark it delivered, read or
//! viewed. Such a receipt is not thrown away at once, because its sender may be a recipient
//! under an id the core has not been told of yet (a contact's ACI, while the chat is still
//! keyed by their PNI): it waits two days and is applied when an identity merge shows that.
//! A backend that knows the conversation a receipt was sent for states it, and the receipt
//! then only lands there.

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{TimelineAnchor, TimelineQuery};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::scripts::{T0, own, remote, signal_features, target};
use kourier_model::{
    ConvKey, ConversationId, ConversationPatch, Member, MemberList, MemberRole, MsgIdScope,
    NetworkKind, OutgoingState, ReceiptKind, RecipientState, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
const CAROL_PNI: &str = "pni:00000000-0000-4000-8000-0000000ca201";
const CAROL_ACI: &str = "aci:00000000-0000-4000-8000-0000000ca202";
const DAVE: &str = "aci:00000000-0000-4000-8000-00000000da7e";
const MALLORY: &str = "aci:00000000-0000-4000-8000-00000000bad0";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn direct(s: &str) -> ConvKey {
    ConvKey::Direct(uid(s))
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:cmVjZWlwdC1ydWxlcw".into())
}

fn harness(dir: &tempfile::TempDir, scope: MsgIdScope) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        scope,
        &[uid(ME)],
    )
    .expect("harness")
}

fn at(n: i64) -> String {
    (T0 + n * 1000).to_string()
}

fn chat(key: &ConvKey, members: Option<&[&str]>) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(members.is_some()));
    p.members = members.map(|m| {
        MemberList::snapshot(
            m.iter()
                .map(|u| Member {
                    user: uid(u),
                    role: MemberRole::Member,
                })
                .collect(),
        )
    });
    BackendEvent::ConversationUpsert(p)
}

/// An own message sent from another device (no send states of its own).
fn mine(key: &ConvKey, n: i64) -> BackendEvent {
    BackendEvent::Message(own(key, &at(n), T0 + n * 1000, "from me"))
}

/// A receipt as Signal sends it: the timestamp of an own message and nothing else.
fn receipt(by: &str, n: i64, kind: ReceiptKind) -> BackendEvent {
    BackendEvent::Receipts {
        targets: vec![target(None, None, &at(n))],
        by: uid(by),
        kind,
        at: T0 + n * 1000 + 500,
    }
}

fn conv_of(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("conversation")
}

fn states(h: &IngestHarness, key: &ConvKey) -> Vec<Option<OutgoingState>> {
    h.timeline(&TimelineQuery {
        conv: conv_of(h, key),
        anchor: TimelineAnchor::Newest,
        limit: 100,
    })
    .expect("timeline")
    .rows
    .into_iter()
    .map(|r| r.out_state)
    .collect()
}

fn count(h: &IngestHarness, table: &str) -> i64 {
    h.connection()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

fn sent() -> Option<OutgoingState> {
    Some(OutgoingState::Sent { server_at: None })
}

#[test]
fn a_receipt_from_someone_the_message_did_not_go_to_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    h.apply(vec![
        chat(&direct(ALICE), None),
        chat(&group(), Some(&[ME, ALICE, BOB])),
        mine(&direct(ALICE), 1),
        mine(&group(), 2),
    ])
    .expect("apply");
    assert_eq!(states(&h, &direct(ALICE)), [sent()]);

    // Mallory knows (or guesses) both timestamps and says she read the messages.
    h.apply(vec![
        receipt(MALLORY, 1, ReceiptKind::Read),
        receipt(MALLORY, 2, ReceiptKind::Viewed),
        // Bob is in the group, not in the chat with Alice.
        receipt(BOB, 1, ReceiptKind::Read),
    ])
    .expect("apply");
    assert_eq!(
        states(&h, &direct(ALICE)),
        [sent()],
        "not read by its recipient"
    );
    assert_eq!(states(&h, &group()), [sent()]);
    assert_eq!(
        count(&h, "receipts"),
        0,
        "nothing of it is stored as a receipt"
    );
    assert_eq!(
        count(&h, "pending_modifiers"),
        3,
        "they wait, without effect"
    );
    // A replay waits once.
    h.apply(vec![receipt(MALLORY, 1, ReceiptKind::Read)])
        .expect("apply");
    assert_eq!(count(&h, "pending_modifiers"), 3);

    // The recipients' receipts count: the peer of the direct chat, the members of the group.
    h.apply(vec![
        receipt(ALICE, 1, ReceiptKind::Read),
        receipt(ALICE, 2, ReceiptKind::Delivered),
        receipt(BOB, 2, ReceiptKind::Delivered),
    ])
    .expect("apply");
    assert_eq!(states(&h, &direct(ALICE)), [Some(OutgoingState::Read)]);
    assert_eq!(states(&h, &group()), [Some(OutgoingState::Delivered)]);
    assert_eq!(count(&h, "receipts"), 3);

    // Two days later the receipts that never counted are forgotten.
    h.set_now(1_700_000_000_000 + 2 * 24 * 3600 * 1000 + 1);
    h.apply(vec![receipt(ALICE, 2, ReceiptKind::Read)])
        .expect("apply");
    assert_eq!(count(&h, "pending_modifiers"), 0);
    assert_eq!(count(&h, "receipts"), 4);
}

/// Someone who left the group since still got the message: the send state says so.
#[test]
fn a_receipt_from_a_former_member_with_a_send_state_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    h.apply(vec![
        chat(&group(), Some(&[ME, ALICE])),
        mine(&group(), 1),
        // The transcript of the send names who it went to: Alice, and Dave, who is no member
        // any more.
        BackendEvent::OwnSendState {
            local: None,
            target: target(Some(&group()), None, &at(1)),
            per_recipient: vec![
                (uid(ALICE), RecipientState::Sent { sealed: true }),
                (uid(DAVE), RecipientState::Sent { sealed: true }),
            ],
        },
        receipt(DAVE, 1, ReceiptKind::Delivered),
        receipt(ALICE, 1, ReceiptKind::Delivered),
        receipt(MALLORY, 1, ReceiptKind::Read),
    ])
    .expect("apply");
    assert_eq!(states(&h, &group()), [Some(OutgoingState::Delivered)]);
    assert_eq!(count(&h, "receipts"), 2);
}

/// A receipt that arrived before what makes its sender a recipient waits (§4.9), and counts
/// as soon as that arrives, not only at an identity merge: a membership update that names
/// the sender, or the send state of the message (another device's transcript here; the
/// answer of our own transmit is `outbox`'s test). Updates that do not make the sender a
/// recipient of that message change nothing.
#[test]
fn a_waiting_receipt_counts_once_its_sender_becomes_a_recipient() {
    let other_group = ConvKey::Group("gv2:b3RoZXItZ3JvdXA".into());
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    h.apply(vec![
        chat(&group(), Some(&[ME, ALICE])),
        chat(&direct(ALICE), None),
        mine(&group(), 1),
        mine(&direct(ALICE), 2),
        // Dave read the group message before the group's update that adds him arrived; a
        // receipt for the direct message comes from someone only the send knows of.
        receipt(DAVE, 1, ReceiptKind::Read),
        receipt(CAROL_ACI, 2, ReceiptKind::Delivered),
    ])
    .expect("apply");
    assert_eq!(
        (count(&h, "receipts"), count(&h, "pending_modifiers")),
        (0, 2)
    );

    // Dave joins another group; the group's list is sent again without him; Alice's chat is
    // patched: none of it makes Dave a recipient of the message.
    h.apply(vec![
        chat(&other_group, Some(&[ME, DAVE])),
        chat(&group(), Some(&[ME, ALICE])),
        chat(&direct(ALICE), None),
    ])
    .expect("apply");
    assert_eq!(
        (count(&h, "receipts"), count(&h, "pending_modifiers")),
        (0, 2)
    );

    // The group's update names Dave: his receipt counts now.
    h.apply(vec![chat(&group(), Some(&[ME, ALICE, DAVE]))])
        .expect("apply");
    assert_eq!(
        (count(&h, "receipts"), count(&h, "pending_modifiers")),
        (1, 1)
    );
    let by: String = h
        .connection()
        .query_row("SELECT user_id FROM receipts", [], |r| r.get(0))
        .expect("the receipt");
    assert_eq!(by, DAVE);

    // The transcript of the direct message names who it went to.
    h.apply(vec![BackendEvent::OwnSendState {
        local: None,
        target: target(Some(&direct(ALICE)), None, &at(2)),
        per_recipient: vec![(uid(CAROL_ACI), RecipientState::Sent { sealed: true })],
    }])
    .expect("apply");
    assert_eq!(
        (count(&h, "receipts"), count(&h, "pending_modifiers")),
        (2, 0)
    );
    assert_eq!(states(&h, &direct(ALICE)), [Some(OutgoingState::Delivered)]);

    // Any order ends the same: the update and the transcript first, the receipts last.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut late = harness(&dir, MsgIdScope::AccountAuthor);
    late.apply(vec![
        chat(&group(), Some(&[ME, ALICE])),
        chat(&direct(ALICE), None),
        mine(&group(), 1),
        mine(&direct(ALICE), 2),
        chat(&other_group, Some(&[ME, DAVE])),
        chat(&group(), Some(&[ME, ALICE, DAVE])),
        BackendEvent::OwnSendState {
            local: None,
            target: target(Some(&direct(ALICE)), None, &at(2)),
            per_recipient: vec![(uid(CAROL_ACI), RecipientState::Sent { sealed: true })],
        },
        receipt(DAVE, 1, ReceiptKind::Read),
        receipt(CAROL_ACI, 2, ReceiptKind::Delivered),
    ])
    .expect("apply");
    assert_eq!(
        late.dump(DumpMode::OrderInsensitive).expect("dump"),
        h.dump(DumpMode::OrderInsensitive).expect("dump")
    );
}

/// A message of somebody else takes no receipt from a third party; from our own device it is
/// our read of it.
#[test]
fn receipts_for_incoming_messages_come_only_from_our_own_devices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::AccountAuthor);
    let theirs = |by: &str, kind| BackendEvent::Receipts {
        targets: vec![target(None, Some(&uid(ALICE)), &at(1))],
        by: uid(by),
        kind,
        at: T0 + 2_000,
    };
    h.apply(vec![
        chat(&direct(ALICE), None),
        BackendEvent::Message(remote(
            &direct(ALICE),
            &uid(ALICE),
            &at(1),
            T0 + 1_000,
            "hi",
        )),
        theirs(MALLORY, ReceiptKind::Read),
        theirs(ALICE, ReceiptKind::Read),
    ])
    .expect("apply");
    assert_eq!(count(&h, "receipts"), 0);
    assert_eq!(count(&h, "pending_modifiers"), 0, "nothing to wait for");
    let unread = |h: &IngestHarness| -> i64 {
        h.connection()
            .query_row("SELECT unread FROM conversations", [], |r| r.get(0))
            .expect("unread")
    };
    assert_eq!(unread(&h), 1);
    h.apply(vec![theirs(ME, ReceiptKind::Read)]).expect("apply");
    assert_eq!(unread(&h), 0, "read on another own device");
}

/// The case the rule must not break: the message went to a contact's PNI chat and the
/// receipt comes from their ACI before the backend has told the core that both are one
/// person. The receipt waits and counts with the merge, whichever comes first.
#[test]
fn a_receipt_under_another_identity_counts_once_the_identities_merge() {
    let merged = BackendEvent::IdentityMerged {
        from: uid(CAROL_PNI),
        into: uid(CAROL_ACI),
    };
    let late = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&late, MsgIdScope::AccountAuthor);
    h.apply(vec![
        chat(&direct(CAROL_PNI), None),
        mine(&direct(CAROL_PNI), 1),
        receipt(CAROL_ACI, 1, ReceiptKind::Read),
    ])
    .expect("apply");
    assert_eq!(states(&h, &direct(CAROL_PNI)), [sent()], "not yet");
    assert_eq!(count(&h, "pending_modifiers"), 1);
    // A restart in between loses nothing: the receipt waits in core.db.
    drop(h);
    let mut h = harness(&late, MsgIdScope::AccountAuthor);
    h.apply(vec![merged.clone()]).expect("apply");
    assert_eq!(states(&h, &direct(CAROL_ACI)), [Some(OutgoingState::Read)]);
    assert_eq!(count(&h, "pending_modifiers"), 0);

    // The merge first, then the receipt: the same end.
    let early = tempfile::tempdir().expect("tempdir");
    let mut e = harness(&early, MsgIdScope::AccountAuthor);
    e.apply(vec![
        chat(&direct(CAROL_PNI), None),
        mine(&direct(CAROL_PNI), 1),
        merged,
        receipt(CAROL_ACI, 1, ReceiptKind::Read),
    ])
    .expect("apply");
    assert_eq!(
        e.dump(DumpMode::OrderInsensitive).expect("dump"),
        h.dump(DumpMode::OrderInsensitive).expect("dump"),
        "both orders end in the same state"
    );
    // A merge that has nothing to do with it leaves a stranger's receipt waiting.
    h.apply(vec![
        receipt(MALLORY, 1, ReceiptKind::Viewed),
        BackendEvent::IdentityMerged {
            from: uid("pni:00000000-0000-4000-8000-00000000da7f"),
            into: uid(DAVE),
        },
    ])
    .expect("apply");
    assert_eq!(states(&h, &direct(CAROL_ACI)), [Some(OutgoingState::Read)]);
    assert_eq!(count(&h, "pending_modifiers"), 1);
}

/// A network whose receipts name their conversation (per-conversation ids, as WhatsApp and
/// Threema): the receipt lands on the message of that conversation only, also when another
/// conversation has a message with the same id.
#[test]
fn a_receipt_that_states_its_conversation_lands_only_there() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir, MsgIdScope::Conversation);
    let stated = |conv: &ConvKey, by: &str| BackendEvent::Receipts {
        targets: vec![target(Some(conv), None, &at(1))],
        by: uid(by),
        kind: ReceiptKind::Read,
        at: T0 + 2_000,
    };
    h.apply(vec![
        chat(&direct(ALICE), None),
        chat(&direct(BOB), None),
        // The same id in two conversations.
        mine(&direct(ALICE), 1),
        mine(&direct(BOB), 1),
        stated(&direct(ALICE), ALICE),
    ])
    .expect("apply");
    assert_eq!(states(&h, &direct(ALICE)), [Some(OutgoingState::Read)]);
    assert_eq!(
        states(&h, &direct(BOB)),
        [sent()],
        "the other chat is untouched"
    );
    // Alice's receipt stated for Bob's chat: she is no recipient there.
    h.apply(vec![stated(&direct(BOB), ALICE)]).expect("apply");
    assert_eq!(states(&h, &direct(BOB)), [sent()]);
    // A receipt for a conversation that has no such message waits for it there, and is not
    // applied to the message with that id elsewhere.
    let empty = direct(DAVE);
    h.apply(vec![chat(&empty, None), stated(&empty, DAVE)])
        .expect("apply");
    assert_eq!(states(&h, &direct(BOB)), [sent()]);
    h.apply(vec![mine(&empty, 1)]).expect("apply");
    assert_eq!(states(&h, &empty), [Some(OutgoingState::Read)]);
}

/// The rule takes nothing away from the scripted streams: every receipt of the three network
/// scripts and of the UI fixture comes from a recipient of its message, so none is left
/// waiting once the stream is in.
#[test]
fn no_receipt_of_the_scripted_streams_is_left_waiting() {
    use kourier_fake::fixture::{FixtureSpec, fixture};
    use kourier_fake::{Flavor, scripts};
    let mut streams = scripts::all().expect("scripts");
    let spec = FixtureSpec {
        modifiers: true,
        showcase: true,
        images: false,
        ..FixtureSpec::default()
    };
    streams.push((Flavor::SignalLike, fixture(&spec).expect("fixture")));
    for (flavor, script) in streams {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([6; 32]),
            NetworkKind::Fake,
            flavor.descriptor().msg_id_scope,
            &script.own_ids,
        )
        .expect("harness");
        let mut receipts = 0;
        for delivery in script.deliveries() {
            receipts += delivery
                .iter()
                .filter(|e| matches!(e, BackendEvent::Receipts { .. }))
                .count();
            h.apply(delivery).expect("apply");
        }
        let waiting: i64 = h
            .connection()
            .query_row(
                "SELECT count(*) FROM pending_modifiers WHERE kind = 'Receipts'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(waiting, 0, "{}: of {receipts} receipts", script.name);
    }
}
