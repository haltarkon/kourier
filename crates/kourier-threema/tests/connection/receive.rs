//! TH5 A6: WHAT A REFLECTED MESSAGE BECOMES, through the real actor against the offline fake. The phone
//! reflects an incoming text, an outgoing text and a group text; the core side (the harness reads the actor's
//! envelopes and acks them) sees the three `Message` events in order, and the fake's ack hook finds their
//! `core_outbox` rows in the file at every ack, so the events are durable before the ack. A poll vote and a
//! call offer are DEFERRED (a row, acked, no event); a text of a sender or a group the file does not know is
//! dropped (acked, a counted WARN, no row, no event); a poll is an unsupported message with its raw body; a
//! replay writes no second row; a backlog that came before Dry is catch-up. TH5b: files, locations (B1), edits,
//! deletes, reactions (B2), and B3's delivery receipts (in, out, the group's legacy reaction), the sent and read
//! updates (one row per envelope, the skips counted) and the ephemeral typing (straight to the core, never
//! acked; without the flag acked and stating nothing). Each test is a parent/child `unshare -rn` pair like its
//! neighbours.

use std::sync::{Arc, Mutex};

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    Attachment, AttachmentKind, AttachmentState, BackendBlob, Body, ConvKey, ConversationPatch,
    Location, MsgTarget, Origin, OutgoingState, RangeKind, ReceiptKind, RecipientState,
    TargetAuthor, Text, TextRange, UserId,
};
use kourier_threema::store;
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;
use tracing::Level;

use super::harness::{self, scene};
use super::reflect::{PEER, PHONE, connect, counts, deferred, phone_of, rows, wait_acks};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, FileMeta, FileSpec, Place};
use crate::support::mock::d2m::EPHEMERAL;
use crate::support::mock::envelope::{
    Author, ContactDelta, Conv, GroupDelta, Msg, Reflect as _, Reflection,
};
use crate::support::warns::Warns;

/// The group's creator (a contact), synthetic.
const CREATOR: &str = "A6RCRTR1";
/// A sender the file has no contact for, synthetic.
const STRANGER: &str = "A6RSTRG1";
const GROUP: u64 = 0x0102_0304_0506_0708;
const OTHER_GROUP: u64 = 0x1111_2222_3333_4444;

fn user(id: &str) -> UserId {
    UserId::from_parts("threema", id).expect("a user")
}

/// The key of `GROUP` written out here, from the spec's rule (creator, a dot, the 16 hex digits of the
/// little-endian bytes), not through the crate's `group_key`.
fn group_key_text(creator: &str, group_id: u64) -> ConvKey {
    let digits: String = group_id
        .to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    ConvKey::Group(format!("threema:{creator}.{digits}").into())
}

/// (`core_outbox` rows, `deferred` rows, `core_outbox_poison` rows) of the file now.
fn core_rows(file: &std::path::Path) -> (i64, i64, i64) {
    let db = store::open(file, &key()).expect("open the file");
    db.read(|c: &Connection| {
        Ok::<_, kourier_db::Error>((
            rows(c, "core_outbox"),
            rows(c, "deferred"),
            rows(c, "core_outbox_poison"),
        ))
    })
    .expect("read the rows")
}

#[test]
fn incoming_outgoing_and_group_texts_reach_the_core_in_order_after_the_zone() {
    support::offline::run_child(
        module_path!(),
        "incoming_outgoing_and_group_texts_reach_the_core_in_order_after_the_zone_child",
    );
}

/// Three texts reflected in one go: the fake's ack hook reads the file at each ack and finds the
/// `core_outbox` rows there (the zone committed first) and no `deferred` row; the core side then reads the
/// three `Message` events in the order they were reflected, with the contract's fields.
#[tokio::test]
async fn incoming_outgoing_and_group_texts_reach_the_core_in_order_after_the_zone_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(601)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    let seen: Arc<Mutex<Vec<(i64, i64, i64)>>> = Arc::default();
    let (file, at_ack) = (h.db_file(), seen.clone());
    h.s.world.mediator().on_reflected_ack(move |_ack| {
        at_ack
            .lock()
            .expect("the hook's list")
            .push(core_rows(&file));
    });
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            PEER,
            &Msg::text([1; 8], START_MS, "synthetic in").nonce([1; 24]),
        ));
        phone.reflect_envelope(&author.outgoing(
            &Conv::contact(PEER),
            &Msg::text([2; 8], START_MS + 1, "synthetic out").nonce([2; 24]),
        ));
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::text([3; 8], START_MS + 2, "synthetic group")
                    .in_group(CREATOR, GROUP)
                    .nonce([3; 24]),
            ),
        );
    });
    wait_acks(&h, 3).await;
    let at_acks = seen.lock().expect("the hook's list").clone();
    assert!(
        at_acks.len() == 3
            && at_acks
                .iter()
                .enumerate()
                .all(
                    |(i, (outbox, deferred, _))| *outbox > i64::try_from(i).unwrap_or(0)
                        && *deferred == 0
                ),
        "an ack came before the message's core_outbox row: {at_acks:?}"
    );
    let got = h.messages(3).await;
    // 1: the contact's text.
    assert_eq!(got[0].conv, ConvKey::Direct(user(PEER)));
    assert_eq!(&*got[0].local, "0101010101010101");
    assert_eq!(
        got[0].origin,
        Origin::Remote {
            author: user(PEER),
            device: None
        }
    );
    assert_eq!(got[0].body, Body::text("synthetic in"));
    assert!(got[0].initial_out_state.is_none() && !got[0].catch_up && !got[0].read);
    // 2: our other device's text: ours, Sending.
    assert_eq!(got[1].conv, ConvKey::Direct(user(PEER)));
    assert_eq!(got[1].origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(got[1].body, Body::text("synthetic out"));
    assert_eq!(
        got[1].initial_out_state,
        Some(OutgoingState::Sending { progress: None })
    );
    // 3: the group's text, under the group's key, authored by the sender.
    assert_eq!(got[2].conv, group_key_text(CREATOR, GROUP));
    assert_eq!(got[2].body, Body::text("synthetic group"));
    assert_eq!(
        got[2].origin,
        Origin::Remote {
            author: user(PEER),
            device: None
        }
    );
    for (i, m) in got.iter().enumerate() {
        assert_eq!(
            m.sent_at,
            i64::try_from(START_MS).unwrap_or(0) + i64::try_from(i).unwrap_or(0)
        );
        assert!(m.server_at.is_some(), "no server time");
    }
    h.sync().await;
    h.nothing_for_the_core();
    // Three D2D nonces and three CSP nonces, no deferred row.
    assert_eq!(counts(&h), (0, 3, 3));
    h.finish().await;
}

#[test]
fn a_quote_and_a_mention_reach_the_core_in_the_contracts_form() {
    support::offline::run_child(
        module_path!(),
        "a_quote_and_a_mention_reach_the_core_in_the_contracts_form_child",
    );
}

/// A reply with a quote and a mention of us, in a group: the prefix is gone, the quote is a reply to an
/// unknown-author target in the group, the mention is one placeholder with its range, `mentions_me` is set.
#[tokio::test]
async fn a_quote_and_a_mention_reach_the_core_in_the_contracts_form_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(602)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let text = format!(
        "{}{}",
        body::quote([0x0a; 8], "ok "),
        body::mention(harness::ME)
    );
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::text([4; 8], START_MS, &text)
                    .in_group(CREATOR, GROUP)
                    .nonce([4; 24]),
            ),
        );
    });
    let got = h.messages(1).await;
    let Body::Text(shown) = &got[0].body else {
        panic!("not a text")
    };
    assert_eq!(
        shown.body, "ok \u{fffc}",
        "the quote prefix stayed or the mention did not turn"
    );
    assert_eq!(shown.ranges.len(), 1);
    assert_eq!((shown.ranges[0].start, shown.ranges[0].len), (3, 3));
    assert!(got[0].mentions_me, "the own ID was named");
    let reply = got[0].reply_to.as_ref().expect("a reply");
    assert_eq!(&*reply.target.local, "0a0a0a0a0a0a0a0a");
    assert_eq!(reply.target.conv, Some(group_key_text(CREATOR, GROUP)));
    h.finish().await;
}

#[test]
fn a_poll_vote_and_a_call_offer_are_deferred_and_what_the_file_does_not_know_is_dropped() {
    support::offline::run_child(
        module_path!(),
        "a_poll_vote_and_a_call_offer_are_deferred_and_what_the_file_does_not_know_is_dropped_child",
    );
}

/// C3, M2: a poll vote and a call offer wait in `deferred` (kind, this build's version 2, the plaintext whole), acked, no
/// event (TH5b B2 mapped the edit and the reaction this test pinned as deferred in TH5a: they are in the next tests);
/// a text from a stranger and a group text of a group the file lacks are acked and dropped with one WARN each
/// (no row, no event); a poll is an unsupported message with its raw body, the only event.
#[tokio::test]
async fn a_poll_vote_and_a_call_offer_are_deferred_and_what_the_file_does_not_know_is_dropped_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(603)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let vote = author.incoming(
        PEER,
        &Msg::raw([5; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, 5)).nonce([5; 24]),
    );
    let offer = author.incoming(
        PEER,
        &Msg::raw([6; 8], START_MS, body::ty::CALL_OFFER, body::filler(24, 6)).nonce([6; 24]),
    );
    let poll = author.incoming(
        PEER,
        &Msg::raw([9; 8], START_MS, body::ty::POLL_SETUP, body::filler(40, 3)).nonce([9; 24]),
    );
    h.net().act(|_| {
        phone.reflect_envelope(&vote);
        phone.reflect_envelope(&offer);
        phone.reflect_envelope(&author.incoming(
            STRANGER,
            &Msg::text([7; 8], START_MS, "synthetic stranger").nonce([7; 24]),
        ));
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::text([8; 8], START_MS, "synthetic lost group")
                    .in_group(CREATOR, OTHER_GROUP)
                    .nonce([8; 24]),
            ),
        );
        phone.reflect_envelope(&poll);
    });
    wait_acks(&h, 5).await;
    let got = h.messages(1).await;
    assert_eq!(
        got[0].body,
        Body::Unsupported {
            kind_hint: "poll".to_owned(),
            summary: None
        }
    );
    assert!(got[0].raw.is_some(), "the poll lost its raw body");
    // The deferred rows: the vote and the offer, in order, the plaintext as sealed.
    let kept = deferred(&h);
    let kinds: Vec<(&str, i64, &str)> = kept
        .iter()
        .map(|(kind, version, source, _)| (kind.as_str(), *version, source.as_str()))
        .collect();
    assert_eq!(
        kinds,
        // Version 2: the build's own MAP_VERSION (TH5b B1 raised it from 1).
        vec![("poll-vote", 2, "reflected"), ("call", 2, "reflected")],
        "not the deferred rows of the vote and the offer"
    );
    assert!(
        kept[0].3 == prost::Message::encode_to_vec(&vote)
            && kept[1].3 == prost::Message::encode_to_vec(&offer),
        "a deferred row is not the plaintext as sealed"
    );
    h.sync().await;
    h.nothing_for_the_core();
    // All five D2D nonces; the dropped ones still wrote theirs and their CSP nonces.
    assert_eq!(counts(&h), (2, 5, 5));
    for rule in ["sender-unknown", "group-unknown"] {
        let line = format!("reflected envelopes acked and dropped: 1 by {rule}");
        assert_eq!(
            warns.at(Level::WARN).iter().filter(|w| **w == line).count(),
            1,
            "not one WARN for {rule}"
        );
    }
    h.finish().await;
}

#[test]
fn an_edit_a_delete_and_two_reactions_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out() {
    support::offline::run_child(
        module_path!(),
        "an_edit_a_delete_and_two_reactions_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out_child",
    );
}

/// TH5b B2 (R2, R6, R7, R8): an edit, a delete, a reaction applied and one withdrawn, each in a 1:1 chat from the
/// contact and from our own other device and in a group from the contact and from our own other device (sixteen
/// envelopes in one go). The core side reads the sixteen events in order with the contract's fields written out
/// here: the target is the message id in the conversation of the placement with an unknown author, `by` is the
/// sender or `None`, the times are the envelopes' OWN `created_at` (an hour before the fake's server time, so the
/// reflection's time would show), the edit's body is the text with its markup read. Sixteen D2D and sixteen CSP
/// nonces, nothing in `deferred`: none of them waits any more.
#[tokio::test]
async fn an_edit_a_delete_and_two_reactions_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(610)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    // The messages being changed: ids [0x31; 8] (1:1) and [0x32; 8] (the group), as 16 hex digits.
    const ONE_TO_ONE: [u8; 8] = [0x31; 8];
    const IN_GROUP: [u8; 8] = [0x32; 8];
    let thumbs = "\u{1f44d}";
    let heart = "\u{2764}\u{fe0f}";
    // `created_at` an hour BEFORE the fake's clock: n is the envelope's number.
    let created = |n: u64| START_MS - 3_600_000 + n;
    let make = |n: u8, to: [u8; 8], kind: usize| {
        let at = created(u64::from(n));
        match kind {
            0 => Msg::edit([n; 8], at, to, "see *this*"),
            1 => Msg::delete([n; 8], at, to),
            2 => Msg::reaction([n; 8], at, to, thumbs),
            _ => Msg::withdraw([n; 8], at, to, heart),
        }
        .nonce([n; 24])
    };
    let mut expected: Vec<BackendEvent> = Vec::new();
    let direct = ConvKey::Direct(user(PEER));
    let group = group_key_text(CREATOR, GROUP);
    let mut n = 0u8;
    h.net().act(|_| {
        // Order: 1:1 from the contact, 1:1 from our other device, group from the contact, group from our other device.
        for place in 0..4usize {
            for kind in 0..4usize {
                n += 1;
                let (conv, to, to_text) = if place < 2 {
                    (&direct, ONE_TO_ONE, "3131313131313131")
                } else {
                    (&group, IN_GROUP, "3232323232323232")
                };
                let msg = make(n, to, kind);
                let envelope = match place {
                    0 => author.incoming(PEER, &msg),
                    1 => author.outgoing(&Conv::contact(PEER), &msg),
                    2 => author.incoming(PEER, &msg.in_group(CREATOR, GROUP)),
                    _ => {
                        author.outgoing(&Conv::group(CREATOR, GROUP), &msg.in_group(CREATOR, GROUP))
                    }
                };
                phone.reflect_envelope(&envelope);
                let by = (place % 2 == 0).then(|| user(PEER));
                let target = MsgTarget {
                    conv: Some(conv.clone()),
                    author: TargetAuthor::Unknown,
                    local: to_text.into(),
                    alternates: Vec::new(),
                };
                let at = i64::try_from(created(u64::from(n))).expect("ms fit i64");
                expected.push(match kind {
                    0 => BackendEvent::Edited {
                        target,
                        by,
                        new_local: None,
                        body: Body::Text(Text {
                            body: "see this".to_owned(),
                            ranges: vec![TextRange {
                                start: 4,
                                len: 4,
                                kind: RangeKind::Bold,
                            }],
                        }),
                        edited_at: at,
                        keeps_quote: None,
                    },
                    1 => BackendEvent::Deleted {
                        target,
                        by,
                        for_everyone: true,
                        at,
                    },
                    2 => BackendEvent::Reaction {
                        target,
                        by,
                        emoji: thumbs.to_owned(),
                        remove: false,
                        at,
                    },
                    _ => BackendEvent::Reaction {
                        target,
                        by,
                        emoji: heart.to_owned(),
                        remove: true,
                        at,
                    },
                });
            }
        }
    });
    wait_acks(&h, 16).await;
    let got = events(&mut h, 16).await;
    for (i, (event, want)) in got.iter().zip(&expected).enumerate() {
        assert!(
            event == want,
            "event {i} is not the expected modifier (place {}, kind {})",
            i / 4,
            i % 4
        );
    }
    h.sync().await;
    h.nothing_for_the_core();
    // Sixteen D2D nonces and sixteen CSP nonces, no deferred row.
    assert_eq!(counts(&h), (0, 16, 16));
    h.finish().await;
}

#[test]
fn a_modifier_that_cannot_be_read_or_has_no_place_is_dropped_by_its_rule_with_one_warn_each() {
    support::offline::run_child(
        module_path!(),
        "a_modifier_that_cannot_be_read_or_has_no_place_is_dropped_by_its_rule_with_one_warn_each_child",
    );
}

/// R6-R8, R15: an emoji of 65 bytes and an empty one are dropped `reaction-bytes` (ONE warn for the batch, counting
/// two), one of exactly 64 bytes is a reaction; a reaction with no action is `reaction-invalid`, undecodable
/// protobufs are `edit-invalid` and `delete-invalid`; a modifier of a stranger is `sender-unknown`, one of a group
/// the file lacks `group-unknown`. All acked (every nonce written), one WARN per rule, the one good reaction the
/// only event, no `deferred` row.
#[tokio::test]
async fn a_modifier_that_cannot_be_read_or_has_no_place_is_dropped_by_its_rule_with_one_warn_each_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(611)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let target = [0x41; 8];
    let raw = |n: u8, ty: u8, bytes: Vec<u8>| Msg::raw([n; 8], START_MS, ty, bytes).nonce([n; 24]);
    let sixty_four = "\u{1f44d}".repeat(16);
    let envelopes = [
        // 1: 65 bytes. 2: no bytes. 3: 64 bytes (kept). 4: no action. 5: an edit and 6: a delete that do not decode.
        author.incoming(
            PEER,
            &raw(
                1,
                body::ty::REACTION,
                body::reaction_apply(target, &[b'x'; 65]),
            ),
        ),
        author.incoming(
            PEER,
            &raw(2, body::ty::REACTION, body::reaction_withdraw(target, b"")),
        ),
        author.incoming(
            PEER,
            &raw(
                3,
                body::ty::REACTION,
                body::reaction_apply(target, sixty_four.as_bytes()),
            ),
        ),
        author.incoming(
            PEER,
            &raw(4, body::ty::REACTION, body::reaction_without_action(target)),
        ),
        author.incoming(PEER, &raw(5, body::ty::EDIT_MESSAGE, vec![0x09, 1, 2, 3])),
        author.incoming(PEER, &raw(6, body::ty::DELETE_MESSAGE, vec![0x09, 1])),
        // 7: an edit of a stranger. 8: a delete in a group the file lacks.
        author.incoming(
            STRANGER,
            &Msg::edit([7; 8], START_MS, target, "synthetic").nonce([7; 24]),
        ),
        author.incoming(
            PEER,
            &Msg::delete([8; 8], START_MS, target)
                .in_group(CREATOR, OTHER_GROUP)
                .nonce([8; 24]),
        ),
    ];
    h.net().act(|_| {
        for envelope in &envelopes {
            phone.reflect_envelope(envelope);
        }
    });
    wait_acks(&h, 8).await;
    let got = events(&mut h, 1).await;
    let [BackendEvent::Reaction { emoji, remove, .. }] = got.as_slice() else {
        panic!("the one event is not a reaction")
    };
    assert!(
        *emoji == sixty_four && !*remove,
        "the 64-byte reaction is not the event"
    );
    h.sync().await;
    h.nothing_for_the_core();
    // All eight D2D and CSP nonces; nothing deferred.
    assert_eq!(counts(&h), (0, 8, 8));
    // The eight come in ONE `act`, but the client's pass may drain them as one batch or as several under load (B5/B7:
    // B1's twin test flapped so), and the zone writes ONE WARN per rule AND batch: what is fixed is the COUNT each
    // rule's WARN lines add up to, and no rule has more lines than drops.
    for (rule, expected) in [
        ("reaction-bytes", 2u32),
        ("reaction-invalid", 1),
        ("edit-invalid", 1),
        ("delete-invalid", 1),
        ("sender-unknown", 1),
        ("group-unknown", 1),
    ] {
        let suffix = format!(" by {rule}");
        let lines: Vec<u32> = warns
            .at(Level::WARN)
            .iter()
            .filter_map(|w| {
                w.strip_prefix("reflected envelopes acked and dropped: ")?
                    .strip_suffix(suffix.as_str())?
                    .parse()
                    .ok()
            })
            .collect();
        assert_eq!(
            lines.iter().sum::<u32>(),
            expected,
            "the WARN lines of {rule} do not count {expected} drops"
        );
        assert!(
            !lines.is_empty() && lines.len() <= expected as usize,
            "{rule}: not at most one WARN per batch"
        );
    }
    h.finish().await;
}

#[test]
fn call_signalling_is_dropped_and_an_offer_and_a_hangup_are_deferred() {
    support::offline::run_child(
        module_path!(),
        "call_signalling_is_dropped_and_an_offer_and_a_hangup_are_deferred_child",
    );
}

/// Round B, M-2 = S2 (b): a call's ICE candidate and its ringing are signalling no later build can use: acked
/// and DROPPED (rule `call-signal`, counted without a WARN), no `deferred` row; the offer and the hangup wait
/// in `deferred` (kind `call`) for the call lines. No event for any of them.
#[tokio::test]
async fn call_signalling_is_dropped_and_an_offer_and_a_hangup_are_deferred_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(604).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let call = |n: u8, ty: u8| {
        author.incoming(
            PEER,
            &Msg::raw([n; 8], START_MS, ty, body::filler(24, n)).nonce([n; 24]),
        )
    };
    let offer = call(1, body::ty::CALL_OFFER);
    let hangup = call(4, body::ty::CALL_HANGUP);
    h.net().act(|_| {
        phone.reflect_envelope(&offer);
        phone.reflect_envelope(&call(2, body::ty::CALL_ICE_CANDIDATE));
        phone.reflect_envelope(&call(3, body::ty::CALL_RINGING));
        phone.reflect_envelope(&hangup);
    });
    wait_acks(&h, 4).await;
    let kept = deferred(&h);
    let kinds: Vec<&str> = kept.iter().map(|(kind, ..)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["call", "call"],
        "not the offer and the hangup alone in deferred"
    );
    assert!(
        kept[0].3 == prost::Message::encode_to_vec(&offer)
            && kept[1].3 == prost::Message::encode_to_vec(&hangup),
        "the deferred rows are not the offer and the hangup"
    );
    h.sync().await;
    h.nothing_for_the_core();
    // Every D2D and CSP nonce is written, the dropped ones too.
    assert_eq!(counts(&h), (2, 4, 4));
    assert!(
        !warns
            .at(Level::WARN)
            .iter()
            .any(|w| w.contains("call-signal")),
        "call signalling was warned"
    );
    h.finish().await;
}

#[test]
fn a_replayed_text_is_acked_and_reaches_the_core_once() {
    support::offline::run_child(
        module_path!(),
        "a_replayed_text_is_acked_and_reaches_the_core_once_child",
    );
}

/// The same sealed bytes again (a redelivery after a kill between the commit and the ack): acked, and the
/// file gets no second `core_outbox` row and the core no second event.
#[tokio::test]
async fn a_replayed_text_is_acked_and_reaches_the_core_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(604).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let mut first: Option<Reflection> = None;
    h.net().act(|_| {
        first = Some(phone.reflect_envelope(&Author::new(PHONE).incoming(
            PEER,
            &Msg::text([1; 8], START_MS, "synthetic once").nonce([1; 24]),
        )));
    });
    wait_acks(&h, 1).await;
    assert_eq!(h.messages(1).await.len(), 1);
    let sealed = first.expect("the first reflection").sealed;
    h.net().act(|_| {
        phone.reflect_bytes(&sealed, 0);
    });
    wait_acks(&h, 2).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(counts(&h), (0, 1, 1), "the replay wrote a nonce or a row");
    h.finish().await;
}

#[test]
fn a_backlog_before_dry_is_catch_up_and_a_live_text_is_not() {
    support::offline::run_child(
        module_path!(),
        "a_backlog_before_dry_is_catch_up_and_a_live_text_is_not_child",
    );
}

/// A text queued while the device was away arrives before `ReflectionQueueDry`: `catch_up` (the core acks it:
/// A5b's boundary holds `Online` until then); one reflected to the online device afterwards is live.
#[tokio::test]
async fn a_backlog_before_dry_is_catch_up_and_a_live_text_is_not_child() {
    use kourier_backend_api::Command;
    if !support::offline::in_child() {
        return;
    }
    let s = scene(605).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", |s| {
        matches!(s, kourier_model::ConnState::Offline { .. })
    })
    .await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            PEER,
            &Msg::text([1; 8], START_MS, "synthetic old").nonce([1; 24]),
        ));
    });
    h.send(Command::Connect).await;
    let old = h.messages(1).await;
    assert_eq!(old[0].body, Body::text("synthetic old"));
    assert!(old[0].catch_up, "a message of the backlog is not catch-up");
    h.wait_state("Online", super::reflect::online).await;
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            PEER,
            &Msg::text([2; 8], START_MS, "synthetic new").nonce([2; 24]),
        ));
    });
    let new = h.messages(1).await;
    assert_eq!(new[0].body, Body::text("synthetic new"));
    assert!(!new[0].catch_up, "a live message is catch-up");
    h.finish().await;
}

/// The attachment a file spec becomes, written out here from the spec's fields (the phone's builder made the
/// JSON, the crate parses it; the pointer is that JSON byte for byte).
fn attachment(
    spec: &FileSpec,
    kind: AttachmentKind,
    dims: Option<(u32, u32)>,
    duration_ms: Option<u32>,
) -> Attachment {
    Attachment {
        kind,
        mime: spec.media_type.clone(),
        size: spec.size,
        file_name: spec.name.clone(),
        dims,
        duration_ms,
        state: AttachmentState::Remote,
        remote: Some(BackendBlob::new(1, body::file(spec))),
        ..Attachment::default()
    }
}

/// A file of rendering type 0 or 2 or a video, built from the picture spec's key and blob id.
fn spec_of(
    rendering: u8,
    media_type: &str,
    name: Option<&str>,
    size: u64,
    caption: Option<&str>,
    meta: Option<FileMeta>,
) -> FileSpec {
    FileSpec {
        rendering,
        media_type: media_type.to_owned(),
        name: name.map(str::to_owned),
        size,
        thumbnail_hex: None,
        thumbnail_type: None,
        caption: caption.map(str::to_owned),
        meta,
        ..FileSpec::picture("unused", None)
    }
}

fn dimensions(width: u32, height: u32) -> FileMeta {
    FileMeta {
        width: Some(width),
        height: Some(height),
        animated: None,
        duration: None,
    }
}

#[test]
fn a_file_a_sticker_and_a_location_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out() {
    support::offline::run_child(
        module_path!(),
        "a_file_a_sticker_and_a_location_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out_child",
    );
}

/// TH5b B1 (R3, R4): a picture with a caption, a PDF, a video with its duration, a sticker and three locations,
/// in a 1:1 chat and a group, from a contact and from our own other device, reflected in one go. The core side
/// reads the nine `Message` events in order with the contract's fields written out here (the kind, the type, the
/// size, the name, the dimensions, the duration, the pointer = the JSON as sent, the caption with its markup
/// read, the three forms of a place with an escaped line break). Nine D2D and nine CSP nonces, nothing in
/// `deferred`: none of them waits any more.
#[tokio::test]
async fn a_file_a_sticker_and_a_location_reach_the_core_in_a_1_1_chat_and_a_group_in_and_out_child()
{
    if !support::offline::in_child() {
        return;
    }
    let s = scene(606)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let picture = FileSpec::picture("synthetic.jpg", Some("look *here*"));
    let sticker = spec_of(
        2,
        "image/webp",
        None,
        2_048,
        None,
        Some(dimensions(512, 512)),
    );
    let pdf = spec_of(
        0,
        "application/pdf",
        Some("synthetic.pdf"),
        9_000,
        None,
        None,
    );
    let video = spec_of(
        1,
        "video/mp4",
        Some("clip.mp4"),
        5_000_000,
        Some("_clip_"),
        Some(FileMeta {
            duration: Some("12.5".to_owned()),
            ..dimensions(1280, 720)
        }),
    );
    let author = Author::new(PHONE);
    let t = |n: u64| START_MS + n;
    let place = Place::Named {
        name: "Harbour".to_owned(),
        address: "1 Quay Street".to_owned(),
    };
    h.net().act(|_| {
        for envelope in [
            author.incoming(PEER, &Msg::file([1; 8], t(1), &picture).nonce([1; 24])),
            author.incoming(
                PEER,
                &Msg::file([2; 8], t(2), &sticker)
                    .in_group(CREATOR, GROUP)
                    .nonce([2; 24]),
            ),
            author.incoming(
                PEER,
                &Msg::location([3; 8], t(3), "47.3769", "8.5417", Some("12.5"), &place)
                    .nonce([3; 24]),
            ),
            author.outgoing(
                &Conv::contact(PEER),
                &Msg::file([4; 8], t(4), &pdf).nonce([4; 24]),
            ),
            author.outgoing(
                &Conv::group(CREATOR, GROUP),
                &Msg::location(
                    [5; 8],
                    t(5),
                    "-33.8688",
                    "151.2093",
                    None,
                    &Place::Address("Main 1\nSecond floor".to_owned()),
                )
                .in_group(CREATOR, GROUP)
                .nonce([5; 24]),
            ),
            author.outgoing(
                &Conv::contact(PEER),
                &Msg::file([6; 8], t(6), &sticker).nonce([6; 24]),
            ),
            author.incoming(
                PEER,
                &Msg::file([7; 8], t(7), &video)
                    .in_group(CREATOR, GROUP)
                    .nonce([7; 24]),
            ),
            author.incoming(
                PEER,
                &Msg::location([8; 8], t(8), "0", "0", None, &Place::Nothing)
                    .in_group(CREATOR, GROUP)
                    .nonce([8; 24]),
            ),
            author.incoming(
                PEER,
                &Msg::file([9; 8], t(9), &picture)
                    .in_group(CREATOR, GROUP)
                    .nonce([9; 24]),
            ),
        ] {
            phone.reflect_envelope(&envelope);
        }
    });
    wait_acks(&h, 9).await;
    let got = h.messages(9).await;
    let direct = ConvKey::Direct(user(PEER));
    let group = group_key_text(CREATOR, GROUP);
    let from_peer = Origin::Remote {
        author: user(PEER),
        device: None,
    };
    let ours = Origin::OwnOtherDevice { device: None };
    let sending = Some(OutgoingState::Sending { progress: None });
    let caption = Text {
        body: "look here".to_owned(),
        ranges: vec![TextRange {
            start: 5,
            len: 4,
            kind: RangeKind::Bold,
        }],
    };
    let picture_media = Body::Media {
        attachments: vec![attachment(
            &picture,
            AttachmentKind::Image,
            Some((640, 480)),
            None,
        )],
        caption: Some(caption),
    };
    let sticker_body = Body::Sticker(Box::new(attachment(
        &sticker,
        AttachmentKind::Image,
        Some((512, 512)),
        None,
    )));
    let place_body = |lat_e7, lon_e7, name: Option<&str>, address: Option<&str>| {
        Body::Location(Location {
            lat_e7,
            lon_e7,
            name: name.map(str::to_owned),
            address: address.map(str::to_owned),
        })
    };
    let expected: [(&ConvKey, &Origin, Option<OutgoingState>, Body); 9] = [
        (&direct, &from_peer, None, picture_media.clone()),
        (&group, &from_peer, None, sticker_body.clone()),
        (
            &direct,
            &from_peer,
            None,
            place_body(
                473_769_000,
                85_417_000,
                Some("Harbour"),
                Some("1 Quay Street"),
            ),
        ),
        (
            &direct,
            &ours,
            sending.clone(),
            Body::Media {
                attachments: vec![attachment(&pdf, AttachmentKind::File, None, None)],
                caption: None,
            },
        ),
        (
            &group,
            &ours,
            sending.clone(),
            place_body(
                -338_688_000,
                1_512_093_000,
                None,
                Some("Main 1\nSecond floor"),
            ),
        ),
        (&direct, &ours, sending, sticker_body),
        (
            &group,
            &from_peer,
            None,
            Body::Media {
                attachments: vec![attachment(
                    &video,
                    AttachmentKind::Video,
                    Some((1280, 720)),
                    Some(12_500),
                )],
                caption: Some(Text {
                    body: "clip".to_owned(),
                    ranges: vec![TextRange {
                        start: 0,
                        len: 4,
                        kind: RangeKind::Italic,
                    }],
                }),
            },
        ),
        (&group, &from_peer, None, place_body(0, 0, None, None)),
        (&group, &from_peer, None, picture_media),
    ];
    assert_eq!(got.len(), expected.len());
    for (i, (m, (conv, origin, out_state, wanted))) in got.iter().zip(expected).enumerate() {
        let n = u8::try_from(i + 1).expect("a small index");
        assert_eq!(&m.conv, conv, "message {n}: the chat");
        assert_eq!(&m.origin, origin, "message {n}: the origin");
        assert_eq!(m.initial_out_state, out_state, "message {n}: the out state");
        assert_eq!(m.body, wanted, "message {n}: the body");
        assert_eq!(
            &*m.local,
            format!("{n:02x}").repeat(8),
            "message {n}: the id"
        );
        assert_eq!(
            m.sent_at,
            i64::try_from(START_MS).unwrap_or(0) + i64::from(n),
            "message {n}: the time"
        );
        assert!(m.server_at.is_some(), "message {n}: no server time");
        assert!(
            m.raw.is_none() && m.reply_to.is_none() && !m.mentions_me && !m.catch_up && !m.read,
            "message {n}: a field that should be unset is set"
        );
    }
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(counts(&h), (0, 9, 9), "a file or a location waited");
    h.finish().await;
}

#[test]
fn a_file_and_a_location_that_cannot_be_read_reach_the_core_as_unsupported_with_their_raw_body() {
    support::offline::run_child(
        module_path!(),
        "a_file_and_a_location_that_cannot_be_read_reach_the_core_as_unsupported_with_their_raw_body_child",
    );
}

/// R3, R4: JSON that is no file and a place off the globe are Unsupported messages (`file`, `location`) that keep
/// the body whole in `raw` (the container too, in a group): shown with the viewmodel's default words, never
/// dropped silently and never deferred; acked, one row each.
#[tokio::test]
async fn a_file_and_a_location_that_cannot_be_read_reach_the_core_as_unsupported_with_their_raw_body_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(607)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let broken = Msg::raw(
        [1; 8],
        START_MS,
        body::ty::FILE,
        b"{\"j\":1,\"m\":\"image/jpeg\"}".to_vec(),
    )
    .nonce([1; 24]);
    let off_the_globe = Msg::location([2; 8], START_MS, "91", "0", None, &Place::Nothing)
        .in_group(CREATOR, GROUP)
        .nonce([2; 24]);
    let wrapped = off_the_globe.body.clone();
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(PEER, &broken));
        phone.reflect_envelope(&author.incoming(PEER, &off_the_globe));
    });
    let got = h.messages(2).await;
    assert_eq!(
        got[0].body,
        Body::Unsupported {
            kind_hint: "file".to_owned(),
            summary: None
        }
    );
    assert_eq!(got[0].conv, ConvKey::Direct(user(PEER)));
    assert_eq!(got[0].raw, Some(BackendBlob::new(1, broken.body.clone())));
    assert_eq!(
        got[1].body,
        Body::Unsupported {
            kind_hint: "location".to_owned(),
            summary: None
        }
    );
    assert_eq!(got[1].conv, group_key_text(CREATOR, GROUP));
    assert_eq!(got[1].raw, Some(BackendBlob::new(1, wrapped)));
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(counts(&h), (0, 2, 2));
    h.finish().await;
}

#[test]
fn a_file_or_a_location_of_a_stranger_or_an_unknown_group_is_dropped_like_a_text() {
    support::offline::run_child(
        module_path!(),
        "a_file_or_a_location_of_a_stranger_or_an_unknown_group_is_dropped_like_a_text_child",
    );
}

/// R15: a mapped kind goes through the placement rules (M2, L-3): a file from a stranger, a location from a
/// stranger and a group file of a group the file lacks are acked and dropped with ONE WARN per rule and batch
/// and no row; they were kept whatever the sender while they were deferred.
#[tokio::test]
async fn a_file_or_a_location_of_a_stranger_or_an_unknown_group_is_dropped_like_a_text_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(608)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let picture = FileSpec::picture("synthetic.jpg", None);
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            STRANGER,
            &Msg::file([1; 8], START_MS, &picture).nonce([1; 24]),
        ));
        phone.reflect_envelope(&author.incoming(
            STRANGER,
            &Msg::location([2; 8], START_MS, "1", "2", None, &Place::Nothing).nonce([2; 24]),
        ));
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::file([3; 8], START_MS, &picture)
                    .in_group(CREATOR, OTHER_GROUP)
                    .nonce([3; 24]),
            ),
        );
    });
    wait_acks(&h, 3).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(counts(&h), (0, 3, 3), "a dropped file left a row");
    // The three come in ONE `act`, but under load the client's pass may drain them as one batch or as two (B5: the
    // filter runs at load split them twice, alone 3 of 3 were one batch), and the zone writes ONE WARN per rule AND
    // batch: what is fixed is the COUNT each rule's WARN lines add up to.
    for (rule, expected) in [("sender-unknown", 2u32), ("group-unknown", 1)] {
        let suffix = format!(" by {rule}");
        let lines: Vec<u32> = warns
            .at(Level::WARN)
            .iter()
            .filter_map(|w| {
                w.strip_prefix("reflected envelopes acked and dropped: ")?
                    .strip_suffix(suffix.as_str())?
                    .parse()
                    .ok()
            })
            .collect();
        assert_eq!(
            lines.iter().sum::<u32>(),
            expected,
            "the WARN lines of {rule} do not count {expected} drops"
        );
        assert!(
            !lines.is_empty() && lines.len() <= 2,
            "{rule}: not one WARN per batch"
        );
    }
    h.finish().await;
}

/// Reads envelopes until `n` events came, ACKING each (the core applied it).
async fn events(h: &mut super::harness::H, n: usize) -> Vec<BackendEvent> {
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

/// The next envelope's events, read BEFORE its ack (the file's state at that point is the zone's).
async fn next_unacked(h: &mut super::harness::H) -> (Vec<BackendEvent>, kourier_backend_api::Ack) {
    let envelope = h.envelope().await;
    (envelope.events, envelope.ack.expect("a durable envelope"))
}

/// The stored message of a contact (a second reader of the file).
fn stored_contact(h: &super::harness::H, id: &str) -> d2d_sync::Contact {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let bytes: Vec<u8> = db
        .read(|c: &Connection| {
            c.query_row("SELECT sync FROM contact WHERE identity = ?1", [id], |r| {
                r.get(0)
            })
            .map_err(kourier_db::Error::from)
        })
        .expect("read the contact");
    d2d_sync::Contact::decode(bytes.as_slice()).expect("a stored contact")
}

#[test]
fn a_file_and_a_location_bring_an_archived_chat_back() {
    support::offline::run_child(
        module_path!(),
        "a_file_and_a_location_bring_an_archived_chat_back_child",
    );
}

/// H5 (R15, "Unarchive: Yes" for file and location): two contacts the phone creates ARCHIVED; a file from the first
/// and our own other device's location to the second each bring their chat back: `ConversationUpsert {
/// archived: Some(false) }` FIRST in the message's row, and the stored record says NORMAL when the core gets that
/// row (the zone wrote it); a second file into the same chat brings nothing back.
#[tokio::test]
async fn a_file_and_a_location_bring_an_archived_chat_back_child() {
    if !support::offline::in_child() {
        return;
    }
    const FIRST: &str = "A6RARC01";
    const SECOND: &str = "A6RARC02";
    let s = scene(609).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        for (identity, key_byte) in [(FIRST, 5), (SECOND, 6)] {
            phone.reflect_envelope(&author.contact_create(
                ContactDelta::create(identity, [key_byte; 32], START_MS).archived(),
            ));
        }
    });
    // Each create states the contact and its (archived) chat; the core applies them.
    let created = events(&mut h, 4).await;
    assert!(
        created
            .iter()
            .map(|e| matches!(e, BackendEvent::ContactUpsert(_)))
            .eq([true, false, true, false]),
        "a created contact is not [contact, chat]"
    );
    let archived = d2d_sync::ConversationVisibility::Archived as i32;
    let normal = d2d_sync::ConversationVisibility::Normal as i32;
    for identity in [FIRST, SECOND] {
        assert_eq!(
            stored_contact(&h, identity).conversation_visibility,
            Some(archived),
            "the contact was not created archived"
        );
    }
    let unarchive = |identity: &str| {
        let mut patch = ConversationPatch::new(ConvKey::Direct(user(identity)));
        patch.archived = Some(false);
        BackendEvent::ConversationUpsert(patch)
    };
    let picture = FileSpec::picture("synthetic.jpg", None);
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            FIRST,
            &Msg::file([1; 8], START_MS + 1, &picture).nonce([1; 24]),
        ));
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        events.len() == 2 && events[0] == unarchive(FIRST),
        "the file into the archived chat does not bring it back first"
    );
    assert!(
        matches!(&events[1], BackendEvent::Message(m) if matches!(m.body, Body::Media { .. })),
        "the file is not the row's second event"
    );
    assert_eq!(
        stored_contact(&h, FIRST).conversation_visibility,
        Some(normal),
        "H5: the stored chat is still archived when the core gets the row"
    );
    ack.complete(Ok(()));

    // Our own other device's location into the second archived chat: the same.
    h.net().act(|_| {
        phone.reflect_envelope(&author.outgoing(
            &Conv::contact(SECOND),
            &Msg::location([2; 8], START_MS + 2, "1", "2", None, &Place::Nothing).nonce([2; 24]),
        ));
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        events.len() == 2 && events[0] == unarchive(SECOND),
        "our own location into the archived chat does not bring it back first"
    );
    assert!(
        matches!(&events[1], BackendEvent::Message(m) if matches!(m.body, Body::Location(_))),
        "the location is not the row's second event"
    );
    assert_eq!(
        stored_contact(&h, SECOND).conversation_visibility,
        Some(normal)
    );
    ack.complete(Ok(()));

    // A second file into the chat that is back: the message alone.
    h.net().act(|_| {
        phone.reflect_envelope(&author.incoming(
            FIRST,
            &Msg::file([3; 8], START_MS + 3, &picture).nonce([3; 24]),
        ));
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        matches!(events.as_slice(), [BackendEvent::Message(_)]),
        "a file into a chat that is not archived brings something back"
    );
    ack.complete(Ok(()));
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

// ---- TH5b B3: receipts, the sent and read updates, typing (R2, R9-R12) ---------------------------------

/// A target of `conv` named by its id's text.
fn target_of(conv: &ConvKey, local: &str, author: TargetAuthor) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author,
        local: local.into(),
        alternates: Vec::new(),
    }
}

#[test]
fn delivery_receipts_and_the_legacy_reactions_reach_the_core_in_and_out_and_in_a_group() {
    support::offline::run_child(
        module_path!(),
        "delivery_receipts_and_the_legacy_reactions_reach_the_core_in_and_out_and_in_a_group_child",
    );
}

/// TH5b B3 (R2, R9, C13): the peer's receipts of OUR message with status 1-4, our phone's receipts of the
/// peer's message with status 1-4, and the deprecated reaction in the group from the peer (3) and from our own
/// other device (4); ten envelopes in one go. The core side reads the fifteen literal events in order: the
/// peer's 1 / 2 are `Receipts` by the peer on our message (`TargetAuthor::Own`), our 2 is our own read (`by`
/// our ID, the peer's message), our 1 states nothing (QUIET: no WARN), every 3 / 4 is the pair "remove all at
/// `created_at - 1`, then the emoji at `created_at`" (`by` the sender or `None`; P-L2: the target's author is
/// `Own` for the peer's pair and `User(peer)` for ours in the 1:1 chat, unknown in the group). The times
/// are the receipts' own `created_at` (an hour before the fake's clock). Ten D2D and ten CSP nonces, nothing
/// deferred.
#[tokio::test]
async fn delivery_receipts_and_the_legacy_reactions_reach_the_core_in_and_out_and_in_a_group_child()
{
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(620)
        .with_contacts(&[PEER, CREATOR])
        .with_group(CREATOR, GROUP);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    // Our message the peer received and read, and the peer's message we read.
    const OURS: [u8; 8] = [0x51; 8];
    const THEIRS: [u8; 8] = [0x52; 8];
    const OURS_TEXT: &str = "5151515151515151";
    const THEIRS_TEXT: &str = "5252525252525252";
    let created = |n: u8| START_MS - 3_600_000 + u64::from(n);
    let at = |n: u8| i64::try_from(created(n)).expect("ms fit i64");
    let receipt = |n: u8, status: u8, to: [u8; 8]| {
        Msg::receipt([n; 8], created(n), status, &[to]).nonce([n; 24])
    };
    let to_peer = Conv::contact(PEER);
    let envelopes = [
        author.incoming(PEER, &receipt(1, 1, OURS)),
        author.incoming(PEER, &receipt(2, 2, OURS)),
        author.incoming(PEER, &receipt(3, 3, OURS)),
        author.incoming(PEER, &receipt(4, 4, OURS)),
        author.outgoing(&to_peer, &receipt(5, 1, THEIRS)),
        author.outgoing(&to_peer, &receipt(6, 2, THEIRS)),
        author.outgoing(&to_peer, &receipt(7, 3, THEIRS)),
        author.outgoing(&to_peer, &receipt(8, 4, THEIRS)),
        author.incoming(PEER, &receipt(9, 3, THEIRS).in_group(CREATOR, GROUP)),
        author.outgoing(
            &Conv::group(CREATOR, GROUP),
            &receipt(10, 4, THEIRS).in_group(CREATOR, GROUP),
        ),
    ];
    h.net().act(|_| {
        for envelope in &envelopes {
            phone.reflect_envelope(envelope);
        }
    });
    let direct = ConvKey::Direct(user(PEER));
    let group = group_key_text(CREATOR, GROUP);
    let pair = |conv: &ConvKey,
                local: &str,
                author: TargetAuthor,
                by: Option<UserId>,
                n: u8,
                emoji: &str| {
        let target = target_of(conv, local, author);
        [
            BackendEvent::Reaction {
                target: target.clone(),
                by: by.clone(),
                emoji: String::new(),
                remove: true,
                at: at(n) - 1,
            },
            BackendEvent::Reaction {
                target,
                by,
                emoji: emoji.to_owned(),
                remove: false,
                at: at(n),
            },
        ]
    };
    let (up, down) = ("\u{1f44d}", "\u{1f44e}");
    let receipts = |by: UserId, local: &str, author: TargetAuthor, kind: ReceiptKind, n: u8| {
        BackendEvent::Receipts {
            targets: vec![target_of(&direct, local, author)],
            by,
            kind,
            at: at(n),
        }
    };
    let expected: Vec<BackendEvent> = [
        vec![
            receipts(
                user(PEER),
                OURS_TEXT,
                TargetAuthor::Own,
                ReceiptKind::Delivered,
                1,
            ),
            receipts(
                user(PEER),
                OURS_TEXT,
                TargetAuthor::Own,
                ReceiptKind::Read,
                2,
            ),
        ],
        pair(
            &direct,
            OURS_TEXT,
            TargetAuthor::Own,
            Some(user(PEER)),
            3,
            up,
        )
        .to_vec(),
        pair(
            &direct,
            OURS_TEXT,
            TargetAuthor::Own,
            Some(user(PEER)),
            4,
            down,
        )
        .to_vec(),
        // 5: our own "received" states nothing.
        vec![receipts(
            user(harness::ME),
            THEIRS_TEXT,
            TargetAuthor::Unknown,
            ReceiptKind::Read,
            6,
        )],
        pair(
            &direct,
            THEIRS_TEXT,
            TargetAuthor::User(user(PEER)),
            None,
            7,
            up,
        )
        .to_vec(),
        pair(
            &direct,
            THEIRS_TEXT,
            TargetAuthor::User(user(PEER)),
            None,
            8,
            down,
        )
        .to_vec(),
        pair(
            &group,
            THEIRS_TEXT,
            TargetAuthor::Unknown,
            Some(user(PEER)),
            9,
            up,
        )
        .to_vec(),
        pair(&group, THEIRS_TEXT, TargetAuthor::Unknown, None, 10, down).to_vec(),
    ]
    .concat();
    wait_acks(&h, 10).await;
    let got = events(&mut h, expected.len()).await;
    for (i, (event, want)) in got.iter().zip(&expected).enumerate() {
        assert!(event == want, "event {i} is not the expected receipt event");
    }
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(counts(&h), (0, 10, 10), "not ten nonces of each and no row");
    assert!(
        !warns.at(Level::WARN).iter().any(|w| w.contains("receipt")),
        "a receipt was warned (our own received one is quiet)"
    );
    h.finish().await;
}

#[test]
fn a_sent_and_a_read_update_reach_the_core_as_one_row_each_and_their_skips_are_counted() {
    support::offline::run_child(
        module_path!(),
        "a_sent_and_a_read_update_reach_the_core_as_one_row_each_and_their_skips_are_counted_child",
    );
}

/// TH5b B3 (R10, R11, C9): the phone creates two groups (CREATOR's with PEER and MEMBER, and one of OURS with
/// PEER), then reflects ONE `sent` update over five messages: a 1:1 one (the peer `Sent`), one in CREATOR's group
/// (every stored member but us, the creator added: CREATOR, MEMBER, PEER), one in our group (PEER: the creator
/// is us), a list and a group the file lacks; then ONE `read` update over two chats (two `Receipts` by our own ID
/// dated `Read.at`, the direct chat's two messages in one). Each update is ONE row with its events in order. FIX-P
/// (P-L1): the group the file lacks is the one skip, ONE WARN `1 by update-unknown`; the list is not counted but
/// the envelope is ALSO deferred as `list` (a row of kind `list`, version 2, the plaintext whole); an update of
/// a list alone is acked with no `core_outbox` row, deferred as `list` too, and no WARN at all.
#[tokio::test]
async fn a_sent_and_a_read_update_reach_the_core_as_one_row_each_and_their_skips_are_counted_child()
{
    if !support::offline::in_child() {
        return;
    }
    const MEMBER: &str = "A6RMEMB1";
    const OUR_GROUP: u64 = 0x0a0b_0c0d_0e0f_1011;
    let warns = Warns::install();
    let s = scene(621).with_contacts(&[PEER, CREATOR]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.group_create(GroupDelta::create(
            CREATOR,
            GROUP,
            "synthetic sent",
            &[PEER, MEMBER],
            START_MS,
        )));
        phone.reflect_envelope(&author.group_create(GroupDelta::create(
            harness::ME,
            OUR_GROUP,
            "synthetic ours",
            &[PEER],
            START_MS,
        )));
    });
    let created = events(&mut h, 2).await;
    assert!(
        created
            .iter()
            .all(|e| matches!(e, BackendEvent::ConversationUpsert(_))),
        "a group create is not its chat"
    );
    const READ_AT: u64 = START_MS - 60_000;
    h.net().act(|_| {
        phone.reflect_envelope(&author.sent(&[
            (Conv::contact(PEER), [0x61; 8]),
            (Conv::List(5), [0x62; 8]),
            (Conv::group(CREATOR, GROUP), [0x63; 8]),
            (Conv::group(CREATOR, OTHER_GROUP), [0x64; 8]),
            (Conv::group(harness::ME, OUR_GROUP), [0x65; 8]),
        ]));
    });
    let direct = ConvKey::Direct(user(PEER));
    let theirs = group_key_text(CREATOR, GROUP);
    let ours = group_key_text(harness::ME, OUR_GROUP);
    let sent = |conv: &ConvKey, local: &str, to: &[&str]| BackendEvent::OwnSendState {
        local: None,
        target: target_of(conv, local, TargetAuthor::Own),
        per_recipient: to
            .iter()
            .map(|id| (user(id), RecipientState::Sent { sealed: false }))
            .collect(),
    };
    let (row, ack) = next_unacked(&mut h).await;
    assert!(
        row == vec![
            sent(&direct, "6161616161616161", &[PEER]),
            sent(&theirs, "6363636363636363", &[CREATOR, MEMBER, PEER]),
            sent(&ours, "6565656565656565", &[PEER]),
        ],
        "the sent update is not its three send states in one row"
    );
    ack.complete(Ok(()));
    wait_acks(&h, 3).await;
    // The forwarder may ship several rows in one envelope: the read update comes after the sent one's row.
    h.net().act(|_| {
        phone.reflect_envelope(&author.read(&[
            (Conv::contact(PEER), [0x71; 8], READ_AT),
            (Conv::group(CREATOR, GROUP), [0x72; 8], READ_AT),
            (Conv::contact(PEER), [0x73; 8], READ_AT),
        ]));
    });
    let at = i64::try_from(READ_AT).expect("ms fit i64");
    let (row, ack) = next_unacked(&mut h).await;
    assert!(
        row == vec![
            BackendEvent::Receipts {
                targets: vec![
                    target_of(&direct, "7171717171717171", TargetAuthor::Unknown),
                    target_of(&direct, "7373737373737373", TargetAuthor::Unknown),
                ],
                by: user(harness::ME),
                kind: ReceiptKind::Read,
                at,
            },
            BackendEvent::Receipts {
                targets: vec![target_of(
                    &theirs,
                    "7272727272727272",
                    TargetAuthor::Unknown,
                )],
                by: user(harness::ME),
                kind: ReceiptKind::Read,
                at,
            },
        ],
        "the read update is not two receipts in one row"
    );
    ack.complete(Ok(()));
    wait_acks(&h, 4).await;
    // An update of skips alone: acked, no row, its own WARN.
    h.net().act(|_| {
        phone.reflect_envelope(&author.sent(&[(Conv::List(6), [0x66; 8])]));
    });
    wait_acks(&h, 5).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(
        counts(&h),
        (2, 5, 0),
        "not five D2D nonces and the two list rows"
    );
    let rows = deferred(&h);
    assert!(
        rows.iter().all(|(kind, version, source, _)| kind == "list"
            && *version == 2
            && source == "reflected")
            && rows.len() == 2,
        "the deferred rows are not two reflected `list` rows of version 2"
    );
    for (row, (names_a_list, others)) in rows.iter().zip([(5u64, 4usize), (6, 0)]) {
        let envelope = d2d::Envelope::decode(row.3.as_slice()).expect("the stored plaintext");
        let Some(d2d::envelope::Content::OutgoingMessageUpdate(update)) = envelope.content else {
            panic!("the deferred row is not the sent update");
        };
        let lists = update
            .updates
            .iter()
            .filter(|u| {
                matches!(
                    u.conversation.as_ref().and_then(|c| c.id.as_ref()),
                    Some(d2d::conversation_id::Id::DistributionList(id)) if *id == names_a_list
                )
            })
            .count();
        assert_eq!(lists, 1, "the row lost its list update");
        assert_eq!(update.updates.len(), 1 + others, "the row is not whole");
    }
    // The skips: only the group the file lacks (the list part is no skip), and the list-only update warns nothing.
    let lines = warns.at(Level::WARN);
    let skips: Vec<&String> = lines
        .iter()
        .filter(|w| w.starts_with("reflected message updates skipped:"))
        .collect();
    assert!(
        skips.len() == 1 && *skips[0] == "reflected message updates skipped: 1 by update-unknown",
        "not one WARN `1 by update-unknown`: {skips:?}"
    );
    h.finish().await;
}

#[test]
fn a_message_into_a_notes_group_is_sent_at_once_and_its_sent_update_is_quiet() {
    support::offline::run_child(
        module_path!(),
        "a_message_into_a_notes_group_is_sent_at_once_and_its_sent_update_is_quiet_child",
    );
}

/// FIX-P (P-M1): the phone creates a NOTES group (ours, nobody else) and a group of ours with PEER, writes a text
/// into each (our other device's messages), then reflects ONE `sent` update naming both messages and, apart, one
/// naming the notes message alone. The notes message reaches the core `Sent` at once (the phone's Sent update for
/// it names no receiver, `csp.struct.yml` footnote 2 of the Bundled Messages Send Steps), the shared group's
/// stays `Sending`; the first update is ONE row with the shared group's `OwnSendState` alone (the notes part is
/// skipped), the second states nothing (acked, no row), and no WARN names an update: the notes skip is quiet.
#[tokio::test]
async fn a_message_into_a_notes_group_is_sent_at_once_and_its_sent_update_is_quiet_child() {
    if !support::offline::in_child() {
        return;
    }
    const NOTES: u64 = 0x0b0c_0d0e_0f10_1112;
    const SHARED: u64 = 0x1314_1516_1718_191a;
    let warns = Warns::install();
    let s = scene(623).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        phone.reflect_envelope(&author.group_create(GroupDelta::create(
            harness::ME,
            NOTES,
            "synthetic notes",
            &[],
            START_MS,
        )));
        phone.reflect_envelope(&author.group_create(GroupDelta::create(
            harness::ME,
            SHARED,
            "synthetic shared",
            &[PEER],
            START_MS,
        )));
    });
    let created = events(&mut h, 2).await;
    assert!(
        created
            .iter()
            .all(|e| matches!(e, BackendEvent::ConversationUpsert(_))),
        "a group create is not its chat"
    );
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.outgoing(
                &Conv::group(harness::ME, NOTES),
                &Msg::text([0x81; 8], START_MS + 1, "synthetic note")
                    .in_group(harness::ME, NOTES)
                    .nonce([0x81; 24]),
            ),
        );
        phone.reflect_envelope(
            &author.outgoing(
                &Conv::group(harness::ME, SHARED),
                &Msg::text([0x82; 8], START_MS + 2, "synthetic shared text")
                    .in_group(harness::ME, SHARED)
                    .nonce([0x82; 24]),
            ),
        );
    });
    let got = h.messages(2).await;
    let notes = group_key_text(harness::ME, NOTES);
    let shared = group_key_text(harness::ME, SHARED);
    assert_eq!(got[0].conv, notes);
    assert_eq!(got[0].origin, Origin::OwnOtherDevice { device: None });
    assert!(got[0].server_at.is_some(), "no server time");
    assert_eq!(
        got[0].initial_out_state,
        Some(OutgoingState::Sent {
            server_at: got[0].server_at
        }),
        "the notes group's message is not Sent at its arrival"
    );
    assert_eq!(got[1].conv, shared);
    assert_eq!(
        got[1].initial_out_state,
        Some(OutgoingState::Sending { progress: None }),
        "a group of ours with a member is not Sending"
    );
    // ONE sent update over both messages: the shared group's state is the row, the notes part is skipped.
    h.net().act(|_| {
        phone.reflect_envelope(&author.sent(&[
            (Conv::group(harness::ME, NOTES), [0x81; 8]),
            (Conv::group(harness::ME, SHARED), [0x82; 8]),
        ]));
    });
    let (row, ack) = next_unacked(&mut h).await;
    assert!(
        row == vec![BackendEvent::OwnSendState {
            local: None,
            target: target_of(&shared, "8282828282828282", TargetAuthor::Own),
            per_recipient: vec![(user(PEER), RecipientState::Sent { sealed: false })],
        }],
        "the sent update is not the shared group's state alone"
    );
    ack.complete(Ok(()));
    // An update of the notes message alone: acked, no row, nothing for the core.
    h.net().act(|_| {
        phone.reflect_envelope(&author.sent(&[(Conv::group(harness::ME, NOTES), [0x81; 8])]));
    });
    wait_acks(&h, 6).await;
    h.sync().await;
    h.nothing_for_the_core();
    // Six D2D nonces (two creates, two texts, two updates) and two CSP nonces, nothing deferred.
    assert_eq!(counts(&h), (0, 6, 2), "not the six envelopes' nonces alone");
    let lines = warns.at(Level::WARN);
    assert!(
        !lines.iter().any(|w| w.contains("updates skipped")),
        "the notes group's Sent update was warned: {lines:?}"
    );
    h.finish().await;
}

#[test]
fn a_group_file_and_a_group_location_bring_an_archived_group_back() {
    support::offline::run_child(
        module_path!(),
        "a_group_file_and_a_group_location_bring_an_archived_group_back_child",
    );
}

/// The stored message of a group (a second reader of the file; the id is stored as the `i64` of its bits).
fn stored_group(h: &super::harness::H, creator: &str, id: u64) -> d2d_sync::Group {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let bits = i64::from_le_bytes(id.to_le_bytes());
    let bytes: Vec<u8> = db
        .read(|c: &Connection| {
            c.query_row(
                "SELECT sync FROM grp WHERE creator = ?1 AND group_id = ?2",
                (creator, bits),
                |r| r.get(0),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("read the group");
    d2d_sync::Group::decode(bytes.as_slice()).expect("a stored group")
}

/// FIX-P (P-L4; H5 for the GROUP forms): the phone creates two groups ARCHIVED (CREATOR's, with PEER); a group
/// file from the member into the first and our own other device's group location into the second each bring
/// their group back: `ConversationUpsert { archived: Some(false) }` FIRST in the message's row (under the group's
/// key) and the stored group says NORMAL when the core gets that row; a second group file into the first brings
/// nothing back.
#[tokio::test]
async fn a_group_file_and_a_group_location_bring_an_archived_group_back_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(624).with_contacts(&[PEER, CREATOR]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    h.net().act(|_| {
        for (id, name) in [
            (GROUP, "synthetic first"),
            (OTHER_GROUP, "synthetic second"),
        ] {
            phone.reflect_envelope(
                &author.group_create(
                    GroupDelta::create(CREATOR, id, name, &[PEER], START_MS).archived(),
                ),
            );
        }
    });
    let created = events(&mut h, 2).await;
    assert!(
        created
            .iter()
            .all(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(true))),
        "a created archived group is not stated archived"
    );
    let archived = d2d_sync::ConversationVisibility::Archived as i32;
    let normal = d2d_sync::ConversationVisibility::Normal as i32;
    for id in [GROUP, OTHER_GROUP] {
        assert_eq!(
            stored_group(&h, CREATOR, id).conversation_visibility,
            Some(archived),
            "the group was not created archived"
        );
    }
    let unarchive = |id: u64| {
        let mut patch = ConversationPatch::new(group_key_text(CREATOR, id));
        patch.archived = Some(false);
        BackendEvent::ConversationUpsert(patch)
    };
    let picture = FileSpec::picture("synthetic.jpg", None);
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::file([1; 8], START_MS + 1, &picture)
                    .in_group(CREATOR, GROUP)
                    .nonce([1; 24]),
            ),
        );
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        events.len() == 2 && events[0] == unarchive(GROUP),
        "the group file into the archived group does not bring it back first"
    );
    assert!(
        matches!(&events[1], BackendEvent::Message(m)
            if matches!(m.body, Body::Media { .. }) && m.conv == group_key_text(CREATOR, GROUP)),
        "the group file is not the row's second event"
    );
    assert_eq!(
        stored_group(&h, CREATOR, GROUP).conversation_visibility,
        Some(normal),
        "H5: the stored group is still archived when the core gets the row"
    );
    ack.complete(Ok(()));

    // Our own other device's group location into the second archived group: the same.
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.outgoing(
                &Conv::group(CREATOR, OTHER_GROUP),
                &Msg::location([2; 8], START_MS + 2, "1", "2", None, &Place::Nothing)
                    .in_group(CREATOR, OTHER_GROUP)
                    .nonce([2; 24]),
            ),
        );
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        events.len() == 2 && events[0] == unarchive(OTHER_GROUP),
        "our own group location into the archived group does not bring it back first"
    );
    assert!(
        matches!(&events[1], BackendEvent::Message(m)
            if matches!(m.body, Body::Location(_)) && m.conv == group_key_text(CREATOR, OTHER_GROUP)),
        "the group location is not the row's second event"
    );
    assert_eq!(
        stored_group(&h, CREATOR, OTHER_GROUP).conversation_visibility,
        Some(normal)
    );
    ack.complete(Ok(()));

    // A second group file into the group that is back: the message alone.
    h.net().act(|_| {
        phone.reflect_envelope(
            &author.incoming(
                PEER,
                &Msg::file([3; 8], START_MS + 3, &picture)
                    .in_group(CREATOR, GROUP)
                    .nonce([3; 24]),
            ),
        );
    });
    let (events, ack) = next_unacked(&mut h).await;
    assert!(
        matches!(events.as_slice(), [BackendEvent::Message(_)]),
        "a group file into a group that is not archived brings something back"
    );
    ack.complete(Ok(()));
    h.sync().await;
    h.nothing_for_the_core();
    h.finish().await;
}

#[test]
fn an_ephemeral_typing_reaches_the_core_unacked_and_one_without_the_flag_states_nothing() {
    support::offline::run_child(
        module_path!(),
        "an_ephemeral_typing_reaches_the_core_unacked_and_one_without_the_flag_states_nothing_child",
    );
}

/// TH5b B3 (R12): the phone's EPHEMERAL typing from the peer (1, then 0) reaches the core as ONE ephemeral
/// `Typing` each (no ack on the envelope, `on` then off, 15 s, the peer's direct chat), and the mediator gets NO
/// ack for them (the fake would record `reflected-ack-for-an-unknown-id`); an ephemeral typing with the byte 2,
/// one of a sender that is no ID and our own device's typing state nothing; the same typing WITHOUT the flag is
/// acked, writes its D2D nonce and states nothing (M5); a deferred vote after them closes the order.
#[tokio::test]
async fn an_ephemeral_typing_reaches_the_core_unacked_and_one_without_the_flag_states_nothing_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(622).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let typing = |n: u8, byte: u8| Msg::typing([n; 8], START_MS, byte);
    let vote = Msg::raw([7; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, 7)).nonce([7; 24]);
    let mut flags = Vec::new();
    h.net().act(|_| {
        flags.push(
            phone
                .reflect_envelope(&author.incoming(PEER, &typing(1, 1)))
                .flags,
        );
        phone.reflect_with(&author.incoming(PEER, &typing(2, 1)), 0);
        flags.push(
            phone
                .reflect_envelope(&author.incoming(PEER, &typing(3, 2)))
                .flags,
        );
        phone.reflect_with(
            &author.outgoing(&Conv::contact(PEER), &typing(4, 1)),
            EPHEMERAL,
        );
        flags.push(
            phone
                .reflect_envelope(&author.incoming("peer0000", &typing(5, 1)))
                .flags,
        );
        flags.push(
            phone
                .reflect_envelope(&author.incoming(PEER, &typing(6, 0)))
                .flags,
        );
        phone.reflect_envelope(&author.incoming(PEER, &vote));
    });
    assert!(
        flags.iter().all(|f| f & EPHEMERAL != 0),
        "the phone's typing was not ephemeral"
    );
    let acks = wait_acks(&h, 2).await;
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    for on in [true, false] {
        let envelope = h.envelope().await;
        assert!(
            envelope.ack.is_none(),
            "a typing went to the core as durable"
        );
        assert!(
            envelope.events
                == vec![BackendEvent::Typing {
                    conv: ConvKey::Direct(user(PEER)),
                    by: user(PEER),
                    device: None,
                    on,
                    ttl: std::time::Duration::from_secs(15),
                }],
            "not the peer's one typing (on {on})"
        );
    }
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(
        h.s.world.mediator().reflected_acks().len(),
        2,
        "an ephemeral typing was acked"
    );
    // The flagless typing's and the vote's D2D nonces, the vote's row and CSP nonce.
    assert_eq!(counts(&h), (1, 2, 1), "not the two acked envelopes");
    h.finish().await;
}
