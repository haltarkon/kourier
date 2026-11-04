//! A1: a new message brings an archived chat back (`receive/unarchive.rs`), and what leaves it
//! archived. The loop with the phone's next record (the diff base) is in `storage/tests.rs`.

use kourier_backend_api::IncomingMessage;
use kourier_model::{CallDirection, CallLine, CallState, Placeholder, SystemEvent};

use super::*;
use crate::groups::{GroupKeys, SnapMember, Snapshot};
use crate::peers::{self, Roaming};
use crate::receive::KIND_CONTENT;
use crate::receive::dispatch::{Ctx, Outcome, dispatch};
use crate::receive::stage2::Row;
use crate::receive::unarchive::bring_back;

/// The wall clock of the rows below.
pub(crate) const NOW: i64 = T as i64 + 60_000;

/// The sent time of the first message of a test.
pub(crate) const AT: u64 = T;

pub(crate) fn peer() -> Aci {
    Aci::from(Uuid::from_u128(0x3e6a_63a4_6d94_46f0_9ab4_8a35_c8f4_f5a1))
}

pub(crate) fn row(sender: ServiceId, timestamp: u64, content: &proto::Content) -> Row {
    Row {
        arrival: 7,
        received_at: 100,
        kind: KIND_CONTENT.to_owned(),
        sender: sender.service_id_string(),
        sender_device: 1,
        timestamp,
        server_timestamp: Some(i64::try_from(timestamp).unwrap() + 5),
        content: Some(content.encode_to_vec()),
        attempts: 0,
        sealed: false,
        decrypt_error: None,
        content_hint: None,
        group_id: None,
        delivered_at: None,
    }
}

pub(crate) fn data(message: proto::DataMessage) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::DataMessage(message)),
        ..Default::default()
    }
}

/// Our phone's transcript of a text it sent to `to` (our own account for Note to Self).
pub(crate) fn transcript_row(own: Aci, to: ServiceId, body: &str, at: u64) -> Row {
    row(
        own.into(),
        at,
        &proto::Content {
            content: Some(proto::content::Content::SyncMessage(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        destination_service_id_binary: Some(to.service_id_binary()),
                        timestamp: Some(at),
                        message: Some(proto::DataMessage {
                            body: Some(body.to_owned()),
                            timestamp: Some(at),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })),
            ..Default::default()
        },
    )
}

/// A text of `sender` in the 1:1 chat with `sender` (or, with `group`, in that group).
pub(crate) fn text_row(sender: Aci, body: &str, at: u64, group: Option<&GroupKeys>) -> Row {
    row(
        sender.into(),
        at,
        &data(proto::DataMessage {
            body: Some(body.to_owned()),
            timestamp: Some(at),
            group_v2: group.map(|keys| proto::GroupContextV2 {
                master_key: Some(keys.master.to_vec()),
                revision: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        }),
    )
}

/// What `dispatch` made of `row`: the durable events, the features of the chats included.
pub(crate) fn dispatched(fixture: &Fixture, row: &Row) -> Vec<BackendEvent> {
    dispatched_both(fixture, row).0
}

fn dispatched_both(fixture: &Fixture, row: &Row) -> (Vec<BackendEvent>, Vec<BackendEvent>) {
    let outcome = fixture
        .db
        .zone(|tx| {
            dispatch(
                tx,
                &Ctx {
                    account: &fixture.account,
                    catch_up: false,
                    now: NOW,
                    long_text: None,
                },
                row,
            )
        })
        .unwrap();
    match outcome {
        Outcome::Handled {
            events, ephemeral, ..
        } => (events, ephemeral),
        failed @ Outcome::Failed { .. } => panic!("not handled: {failed:?}"),
    }
}

/// How many times `events` say that `conv` is not archived.
pub(crate) fn unarchives(events: &[BackendEvent], conv: &ConvKey) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(event, BackendEvent::ConversationUpsert(patch)
                if &patch.key == conv && patch.archived == Some(false))
        })
        .count()
}

fn position(events: &[BackendEvent], wanted: impl Fn(&BackendEvent) -> bool) -> usize {
    events
        .iter()
        .position(wanted)
        .unwrap_or_else(|| panic!("not there: {events:?}"))
}

/// The 1:1 chat with `who` is archived on the phone as the core was last told (`mute` for the
/// phone's mute).
pub(crate) fn archive_direct(fixture: &Fixture, who: Aci, mute: Option<i64>) {
    fixture
        .db
        .zone(|tx| {
            peers::set_roaming(
                tx,
                &ServiceId::from(who).service_id_string(),
                Roaming {
                    archived: true,
                    muted_until: mute,
                },
                1,
            )
        })
        .unwrap();
}

pub(crate) fn base_of(fixture: &Fixture, who: Aci) -> Option<Roaming> {
    fixture
        .db
        .read(|c| peers::load(c, &ServiceId::from(who).service_id_string()))
        .unwrap()
        .roaming
}

/// A group of `members` (and us) the core has, archived on the phone and muted as `mute` says.
pub(crate) fn archived_group(
    fixture: &Fixture,
    master: [u8; 32],
    members: &[Aci],
    mute: Option<i64>,
) -> GroupKeys {
    let keys = GroupKeys::from_slice(&master).unwrap();
    let snapshot = Snapshot {
        revision: 1,
        members: members
            .iter()
            .chain([&fixture.account.aci])
            .map(|aci| SnapMember {
                aci: (*aci).into(),
                admin: false,
                joined_at: 0,
            })
            .collect(),
        ..Snapshot::default()
    };
    fixture
        .db
        .zone(|tx| -> crate::error::Result<()> {
            tx.execute(
                "INSERT INTO groups(master_key, group_id, updated_at, revision, state, membership,
                   features_hash, archived, muted_until)
                 VALUES (?1, ?2, 0, 1, ?3, 'member', 42, 1, ?4)",
                (&keys.master[..], &keys.id[..], snapshot.to_json()?, mute),
            )?;
            Ok(())
        })
        .unwrap();
    keys
}

pub(crate) fn group_archived(fixture: &Fixture, keys: &GroupKeys) -> bool {
    fixture
        .db
        .read(|c| crate::groups::archive_state(c, &keys.id))
        .unwrap()
        .expect("the group is stored")
        .0
}

/// The phone's "Keep muted chats archived".
pub(crate) fn keep_muted(fixture: &Fixture, on: bool) {
    fixture
        .db
        .zone(|tx| -> crate::error::Result<()> {
            tx.execute("UPDATE account SET keep_muted_archived = ?1", [on])?;
            Ok(())
        })
        .unwrap();
}

/// A person's message, a contact's chat: the chat comes back with it, ahead of the message
/// (so the core has the chat out of the archive when the message lands), once, and the base
/// follows in the row's zone; a group's the same with `groups.archived`.
#[test]
fn an_archived_chat_comes_back_with_a_new_message() {
    let fixture = fixture();
    let ada = peer();
    let conv = ConvKey::Direct(aci_user(ada));
    archive_direct(&fixture, ada, None);
    let events = dispatched(&fixture, &text_row(ada, "hello", T, None));
    assert_eq!(unarchives(&events, &conv), 1, "{events:?}");
    let back = position(
        &events,
        |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(false)),
    );
    let message = position(&events, |e| matches!(e, BackendEvent::Message(_)));
    assert!(back < message, "the chat comes back first: {events:?}");
    let BackendEvent::ConversationUpsert(patch) = &events[back] else {
        unreachable!()
    };
    assert!(
        patch.features.is_none(),
        "only the archive: the features have their own upsert"
    );
    assert_eq!(
        base_of(&fixture, ada),
        Some(Roaming {
            archived: false,
            muted_until: None
        }),
        "the base the Storage diff compares with follows the unarchive"
    );

    // A group, a message of a member.
    let bruno = Aci::from(Uuid::from_u128(0xb0b));
    let keys = archived_group(&fixture, [7; 32], &[bruno], None);
    assert!(group_archived(&fixture, &keys));
    let events = dispatched(&fixture, &text_row(bruno, "hello all", T + 1, Some(&keys)));
    assert_eq!(unarchives(&events, &keys.conv_key()), 1, "{events:?}");
    assert!(
        !group_archived(&fixture, &keys),
        "the group's flag is its base"
    );
}

/// The phone's mute keeps a chat archived only with "Keep muted chats archived" on and only
/// while the mute runs (`i64::MAX` is for ever); a 1:1 chat's and a group's alike.
#[test]
fn a_muted_archived_chat_comes_back_unless_the_phone_keeps_muted_chats_archived() {
    // (the phone's mute, keep muted chats archived, comes back)
    let cases = [
        (Some(i64::MAX), false, true),
        (Some(i64::MAX), true, false),
        (Some(NOW + 1), true, false),
        (Some(NOW), true, true),
        (Some(NOW - 1), true, true),
        (None, true, true),
    ];
    for (mute, keep, comes_back) in cases {
        let why = format!("muted until {mute:?}, keep muted chats archived {keep}");
        // A contact's chat.
        let fixture = fixture();
        keep_muted(&fixture, keep);
        let ada = peer();
        archive_direct(&fixture, ada, mute);
        let events = dispatched(&fixture, &text_row(ada, "hello", T, None));
        let conv = ConvKey::Direct(aci_user(ada));
        assert_eq!(
            unarchives(&events, &conv) == 1,
            comes_back,
            "{why}: {events:?}"
        );
        assert_eq!(
            base_of(&fixture, ada),
            Some(Roaming {
                archived: !comes_back,
                muted_until: mute
            }),
            "{why}: the mute stays what the phone said"
        );
        // A group.
        let bruno = Aci::from(Uuid::from_u128(0xb0b));
        let keys = archived_group(&fixture, [7; 32], &[bruno], mute);
        let events = dispatched(&fixture, &text_row(bruno, "hello all", T + 1, Some(&keys)));
        assert_eq!(
            unarchives(&events, &keys.conv_key()) == 1,
            comes_back,
            "{why} (group): {events:?}"
        );
        assert_eq!(
            group_archived(&fixture, &keys),
            !comes_back,
            "{why} (group)"
        );
    }
}

/// Only the first message of an archived chat says so: the base is 0 after it.
#[test]
fn a_second_message_sends_no_second_unarchive() {
    let fixture = fixture();
    let ada = peer();
    let conv = ConvKey::Direct(aci_user(ada));
    archive_direct(&fixture, ada, None);
    let first = dispatched(&fixture, &text_row(ada, "one", T, None));
    assert_eq!(unarchives(&first, &conv), 1);
    let second = dispatched(&fixture, &text_row(ada, "two", T + 1, None));
    assert_eq!(unarchives(&second, &conv), 0, "{second:?}");
    // The phone archives it again (the base is 1 once more): the next message brings it back.
    archive_direct(&fixture, ada, None);
    let third = dispatched(&fixture, &text_row(ada, "three", T + 2, None));
    assert_eq!(unarchives(&third, &conv), 1, "{third:?}");
}

/// A reaction, an edit, a delete for everyone, a receipt, typing, a timer change, a profile
/// key update and a null message are not a new message: the chat stays archived. A text then
/// brings it back (the control: the chat could have come back at any of them).
#[test]
fn reactions_edits_deletes_receipts_typing_and_timer_changes_leave_it_archived() {
    let fixture = fixture();
    let ada = peer();
    let me = fixture.account.aci;
    let conv = ConvKey::Direct(aci_user(ada));
    fixture
        .db
        .zone(|tx| crate::db::store_configuration(tx, None, Some(true)))
        .unwrap();
    archive_direct(&fixture, ada, None);
    let content = |content: proto::content::Content| proto::Content {
        content: Some(content),
        ..Default::default()
    };
    let contents = [
        (
            data(proto::DataMessage {
                timestamp: Some(T),
                reaction: Some(proto::data_message::Reaction {
                    emoji: Some("👍".to_owned()),
                    target_author_aci_binary: Some(Uuid::from(me).as_bytes().to_vec()),
                    target_sent_timestamp: Some(41),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            "a reaction",
        ),
        (
            data(proto::DataMessage {
                timestamp: Some(T),
                delete: Some(proto::data_message::Delete {
                    target_sent_timestamp: Some(41),
                }),
                ..Default::default()
            }),
            "a delete for everyone",
        ),
        (
            content(proto::content::Content::EditMessage(proto::EditMessage {
                target_sent_timestamp: Some(39),
                data_message: Some(proto::DataMessage {
                    body: Some("fixed".to_owned()),
                    timestamp: Some(T),
                    ..Default::default()
                }),
            })),
            "an edit",
        ),
        (
            content(proto::content::Content::ReceiptMessage(
                proto::ReceiptMessage {
                    r#type: Some(proto::receipt_message::Type::Read.into()),
                    timestamp: vec![10],
                },
            )),
            "a read receipt",
        ),
        (
            content(proto::content::Content::TypingMessage(
                proto::TypingMessage {
                    timestamp: Some(T),
                    action: Some(proto::typing_message::Action::Started.into()),
                    group_id: None,
                },
            )),
            "typing",
        ),
        (
            data(proto::DataMessage {
                timestamp: Some(T),
                flags: Some(2),
                expire_timer: Some(60),
                ..Default::default()
            }),
            "a timer change",
        ),
        (
            data(proto::DataMessage {
                timestamp: Some(T),
                flags: Some(4),
                profile_key: Some(vec![8; 32]),
                ..Default::default()
            }),
            "a profile key update",
        ),
        (
            content(proto::content::Content::NullMessage(
                proto::NullMessage::default(),
            )),
            "a null message",
        ),
    ];
    for (content, what) in contents {
        let (events, ephemeral) = dispatched_both(&fixture, &row(ada.into(), T, &content));
        assert_eq!(unarchives(&events, &conv), 0, "{what}: {events:?}");
        assert_eq!(unarchives(&ephemeral, &conv), 0, "{what}: {ephemeral:?}");
        assert_eq!(
            base_of(&fixture, ada).map(|r| r.archived),
            Some(true),
            "{what} left the base alone"
        );
    }
    let events = dispatched(&fixture, &text_row(ada, "hello", T + 9, None));
    assert_eq!(unarchives(&events, &conv), 1, "{events:?}");
}

fn message(conv: &ConvKey, origin: Origin, body: Body) -> IncomingMessage {
    IncomingMessage::new(conv.clone(), "m", origin, body, NOW, NOW)
}

/// The rule by the message alone: who wrote it and what it is.
#[test]
fn what_counts_as_a_message_that_brings_a_chat_back() {
    let fixture = fixture();
    let ada = peer();
    let ada_user = aci_user(ada);
    let direct = ConvKey::Direct(ada_user.clone());
    let keys = archived_group(&fixture, [7; 32], &[ada], None);
    let group = keys.conv_key();
    let remote = Origin::Remote {
        author: ada_user.clone(),
        device: Some(1),
    };
    let other_device = Origin::OwnOtherDevice { device: Some(1) };
    let text = || Body::text("hello");
    let call = || {
        Body::System(SystemEvent::Call(CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Started,
            NOW,
        )))
    };
    let mut read_elsewhere = message(&direct, remote.clone(), text());
    read_elsewhere.read = true;
    let story = |kind: &str| Body::Unsupported {
        kind_hint: kind.to_owned(),
        summary: None,
    };
    // (what, the message, whether it brings its chat back)
    let table = [
        (
            "a person's text",
            message(&direct, remote.clone(), text()),
            true,
        ),
        ("a text read elsewhere first", read_elsewhere, false),
        (
            "our phone's transcript",
            message(&direct, other_device.clone(), text()),
            true,
        ),
        (
            "our own send (brought back where it is recorded)",
            message(&direct, Origin::OwnThisDevice, text()),
            false,
        ),
        (
            "a call line of the other side",
            message(&direct, remote.clone(), call()),
            true,
        ),
        (
            "a call line of our phone",
            message(&direct, other_device.clone(), call()),
            true,
        ),
        (
            "a notice",
            message(
                &direct,
                remote.clone(),
                Body::System(SystemEvent::SessionRefreshed {
                    user: ada_user.clone(),
                }),
            ),
            false,
        ),
        (
            "a notice of our phone",
            message(
                &direct,
                other_device.clone(),
                Body::System(SystemEvent::SessionRefreshed {
                    user: ada_user.clone(),
                }),
            ),
            false,
        ),
        (
            "a stand-in for what did not decrypt",
            message(
                &direct,
                remote.clone(),
                Body::Placeholder(Placeholder::Undecryptable { resendable: true }),
            ),
            false,
        ),
        (
            "a story reply in a 1:1 chat",
            message(&direct, remote.clone(), story("story-reply")),
            true,
        ),
        (
            "a story reply in a group",
            message(&group, remote.clone(), story("story-reply")),
            false,
        ),
        (
            "a payment in a group",
            message(&group, remote.clone(), story("payment")),
            true,
        ),
        (
            "a text in a group",
            message(&group, remote.clone(), text()),
            true,
        ),
        (
            "a group text of our phone",
            message(&group, other_device, text()),
            true,
        ),
    ];
    for (what, incoming, comes_back) in table {
        archive_direct(&fixture, ada, None);
        fixture
            .db
            .zone(|tx| crate::groups::set_archived(tx, &keys.id, true, 1))
            .unwrap();
        let conv = incoming.conv.clone();
        let out = fixture
            .db
            .zone(|tx| {
                bring_back(
                    tx,
                    &Ctx {
                        account: &fixture.account,
                        catch_up: false,
                        now: NOW,
                        long_text: None,
                    },
                    vec![BackendEvent::Message(incoming)],
                )
            })
            .unwrap();
        assert_eq!(unarchives(&out, &conv) == 1, comes_back, "{what}: {out:?}");
        assert_eq!(
            out.len(),
            1 + usize::from(comes_back),
            "{what}: the message itself is kept"
        );
    }
}

/// A person known only by a number has no row to hold a base and no archive state: nothing
/// happens, and nothing fails.
#[test]
fn a_chat_with_a_number_alone_is_left_as_it_is() {
    let fixture = fixture();
    let number = crate::service_id::e164_user_id("+15550123").unwrap();
    let conv = ConvKey::Direct(number.clone());
    let events = fixture
        .db
        .zone(|tx| {
            bring_back(
                tx,
                &Ctx {
                    account: &fixture.account,
                    catch_up: false,
                    now: NOW,
                    long_text: None,
                },
                vec![BackendEvent::Message(message(
                    &conv,
                    Origin::Remote {
                        author: number,
                        device: None,
                    },
                    Body::text("hello"),
                ))],
            )
        })
        .unwrap();
    assert_eq!(unarchives(&events, &conv), 0);
    assert_eq!(events.len(), 1);
}

/// Desktop runs the phone's transcripts of what it sent through the path of its own sends, so
/// a message our phone sent to a chat brings the chat back: a contact's, a group's (the
/// transcript names the group in the message, not a destination).
#[test]
fn a_sync_transcript_brings_an_archived_chat_back() {
    let fixture = fixture();
    let ada = peer();
    let own = fixture.account.aci;
    let direct = ConvKey::Direct(aci_user(ada));
    archive_direct(&fixture, ada, None);
    let events = dispatched(&fixture, &transcript_row(own, ada.into(), "from phone", AT));
    assert_eq!(unarchives(&events, &direct), 1, "{events:?}");
    assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(false));
    let again = dispatched(
        &fixture,
        &transcript_row(own, ada.into(), "once more", AT + 1),
    );
    assert_eq!(unarchives(&again, &direct), 0, "{again:?}");

    let keys = archived_group(&fixture, [7; 32], &[ada], None);
    let group_transcript = row(
        own.into(),
        AT + 2,
        &proto::Content {
            content: Some(proto::content::Content::SyncMessage(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        timestamp: Some(AT + 2),
                        message: Some(proto::DataMessage {
                            body: Some("to the group".to_owned()),
                            timestamp: Some(AT + 2),
                            group_v2: Some(proto::GroupContextV2 {
                                master_key: Some(keys.master.to_vec()),
                                revision: Some(1),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })),
            ..Default::default()
        },
    );
    let events = dispatched(&fixture, &group_transcript);
    assert_eq!(unarchives(&events, &keys.conv_key()), 1, "{events:?}");
    assert!(!group_archived(&fixture, &keys));
}

/// A call line is a saved call-history entry: the caller's Offer (a ringing line) and the
/// caller giving up (a missed one) bring the chat back, through the real mapping. What makes
/// no line (an answer, an ICE update) leaves it archived.
#[test]
fn a_call_line_brings_an_archived_chat_back() {
    use proto::call_message::{Answer, Hangup, Offer, hangup, offer};
    let fixture = fixture();
    let ada = peer();
    let direct = ConvKey::Direct(aci_user(ada));
    let call = |message: proto::CallMessage| {
        row(
            ada.into(),
            AT,
            &proto::Content {
                content: Some(proto::content::Content::CallMessage(message)),
                ..Default::default()
            },
        )
    };
    let offer = call(proto::CallMessage {
        offer: Some(Offer {
            id: Some(77),
            r#type: Some(offer::Type::OfferAudioCall.into()),
            opaque: None,
        }),
        ..Default::default()
    });
    let hangup = call(proto::CallMessage {
        hangup: Some(Hangup {
            id: Some(78),
            r#type: Some(hangup::Type::HangupNormal.into()),
            device_id: None,
        }),
        ..Default::default()
    });
    let answer = call(proto::CallMessage {
        answer: Some(Answer {
            id: Some(77),
            opaque: Some(vec![1, 2, 3]),
        }),
        ..Default::default()
    });
    archive_direct(&fixture, ada, None);
    let events = dispatched(&fixture, &answer);
    assert_eq!(unarchives(&events, &direct), 0, "{events:?}");
    assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(true));
    for (what, row) in [("an Offer", offer), ("a missed call", hangup)] {
        archive_direct(&fixture, ada, None);
        let events = dispatched(&fixture, &row);
        assert_eq!(unarchives(&events, &direct), 1, "{what}: {events:?}");
        let back = position(
            &events,
            |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(false)),
        );
        let line = position(&events, |e| matches!(e, BackendEvent::Message(_)));
        assert!(back < line, "{what}: the chat comes back first: {events:?}");
        assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(false));
    }
}
