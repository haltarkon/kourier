//! Tests of what the review of step S1 found in the mapper and what was changed for it: a
//! second frame of one person, the send state of an own item, the two times that followed
//! this computer's clock. Synthetic data; nothing of a row is printed.

use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, OutgoingMessageDetails};
use backup::recipient::Destination;
use backup::send_status::DeliveryStatus;

use super::review::{base, failed, status, text_item};
use super::*;

const T: u64 = 1_700_000_000_000;
const HOUR: u64 = 60 * 60 * 1000;

fn contact_frame(id: u64, i: u8, name: &str) -> Frame {
    Frame::Recipient(backup::Recipient {
        id,
        destination: Some(Destination::Contact(backup::Contact {
            aci: Some(aci(i).as_bytes().to_vec()),
            profile_given_name: Some(name.to_owned()),
            profile_sharing: true,
            ..Default::default()
        })),
    })
}

fn chat_frame(id: u64, recipient_id: u64) -> Frame {
    Frame::Chat(backup::Chat {
        id,
        recipient_id,
        ..Default::default()
    })
}

fn incoming_item(chat_id: u64, author: u64, sent: u64, body: &str) -> backup::ChatItem {
    let mut item = text_item(
        DirectionalDetails::Incoming(IncomingMessageDetails {
            date_received: sent + 1,
            date_server_sent: Some(sent),
            read: true,
            sealed_sender: true,
        }),
        author,
        sent,
        body,
    );
    item.chat_id = chat_id;
    item
}

fn outgoing_item(chat_id: u64, sent: u64, statuses: Vec<backup::SendStatus>) -> backup::ChatItem {
    let mut item = text_item(
        DirectionalDetails::Outgoing(OutgoingMessageDetails {
            send_status: statuses,
            date_received: sent,
        }),
        1,
        sent,
        "mine",
    );
    item.chat_id = chat_id;
    item
}

fn delivered() -> DeliveryStatus {
    DeliveryStatus::Delivered(Default::default())
}

/// The state of one own item of chat 1 (with person 0x21, recipient 2) for these entries.
fn state_of(statuses: Vec<backup::SendStatus>) -> OutgoingState {
    let mut frames = base();
    frames.push(Frame::ChatItem(Box::new(outgoing_item(1, T, statuses))));
    let (staged, _) = map(frames);
    messages(&staged)[0].initial_out_state.clone().unwrap()
}

/// F1 of the review. A person described by two contact frames is one person: a chat on
/// either recipient id is the one conversation with them, an item by either id is theirs,
/// a send status for either id is their state. The first frame's fields stand. The loss the
/// review found: the second frame was dropped, and with it its chat and every item of it.
#[test]
fn a_second_contact_frame_of_one_person_leads_to_the_same_conversation() {
    let frames = vec![
        Frame::Recipient(backup::Recipient {
            id: 1,
            destination: Some(Destination::Self_(backup::Self_::default())),
        }),
        contact_frame(2, 0x21, "First"),
        contact_frame(3, 0x21, "Second"),
        contact_frame(4, 0x22, "Other"),
        chat_frame(1, 2),
        chat_frame(2, 3),
        chat_frame(3, 4),
        Frame::ChatItem(Box::new(incoming_item(1, 2, T, "in the first chat"))),
        Frame::ChatItem(Box::new(incoming_item(2, 3, T + 1, "in the second chat"))),
        // The person under their other id, in either chat: theirs all the same.
        Frame::ChatItem(Box::new(incoming_item(
            1,
            3,
            T + 2,
            "second id, first chat",
        ))),
        Frame::ChatItem(Box::new(incoming_item(
            2,
            2,
            T + 3,
            "first id, second chat",
        ))),
        // Our own, in the second chat, with the state the phone has for the second id.
        Frame::ChatItem(Box::new(outgoing_item(
            2,
            T + 4,
            vec![status(3, DeliveryStatus::Read(Default::default()))],
        ))),
        // Another person is still not this chat's: the rule that keeps one person's text
        // out of another's chat stands.
        Frame::ChatItem(Box::new(incoming_item(2, 4, T + 5, "a stranger's"))),
    ];
    let (staged, end) = map(frames);
    let conv = ConvKey::Direct(user(0x21));
    let rows = messages(&staged);
    assert_eq!(
        rows.iter().map(|m| body(m)).collect::<Vec<_>>(),
        [
            "in the first chat",
            "in the second chat",
            "second id, first chat",
            "first id, second chat",
            "mine",
        ]
    );
    assert!(rows.iter().all(|m| m.conv == conv), "one conversation");
    assert!(rows[..4].iter().all(|m| m.origin
        == Origin::Remote {
            author: user(0x21),
            device: None
        }));
    assert_eq!(rows[4].initial_out_state, Some(OutgoingState::Read));
    assert_eq!(chats(&staged).len(), 1);
    let people = contacts(&staged);
    assert_eq!(people.len(), 1, "one person, staged once");
    assert_eq!(people[0].profile_name.as_deref(), Some("First"));
    assert_eq!(end.chats.len(), 1);
    assert_eq!(end.chats[0].n_rows, 5);
    assert_eq!(count(&end, reason::DUPLICATE_CONTACT), 1);
    assert_eq!(count(&end, reason::UNKNOWN_CHAT), 0, "nothing was lost");
    assert_eq!(count(&end, reason::AUTHOR_NOT_IN_CHAT), 1);
}

/// A second frame of a person that falls over the memory cap is not kept, and it takes
/// nobody with it: the frame made no person, so the person it names stays.
#[test]
fn a_second_contact_frame_over_the_cap_does_not_take_the_person_back() {
    let mut mapper = mapper_with(MapLimits::default());
    let mut out = Vec::new();
    mapper.frame(contact_frame(2, 0x21, "First"), &mut out);
    // From here on not one more frame fits.
    mapper.limits.max_recipient_bytes = mapper.recipient_bytes;
    mapper.frame(contact_frame(3, 0x21, "Second"), &mut out);
    assert_eq!(mapper.people.len(), 1, "the person is still known");
    mapper.frame(chat_frame(1, 2), &mut out);
    mapper.frame(
        Frame::ChatItem(Box::new(incoming_item(1, 2, T, "still theirs"))),
        &mut out,
    );
    let end = mapper.finish();
    assert_eq!(messages(&out).len(), 1);
    assert_eq!(contacts(&out).len(), 1);
    assert_eq!(count(&end, reason::DUPLICATE_CONTACT), 1);
    assert_eq!(count(&end, reason::RECIPIENT_OVER_CAP), 1);
}

/// F2 of the review. In a 1:1 chat only the entries of the chat's own contact say how an
/// own message went; the row names nobody else, a person once, and a partial failure never
/// nobody.
#[test]
fn only_the_chats_own_contact_counts_for_the_send_state() {
    let peer = 2;
    let other = 3; // person 0x22, who has no chat
    let unknown = 999;
    let own = 1;
    let read = || DeliveryStatus::Read(Default::default());
    let pending = || DeliveryStatus::Pending(Default::default());

    // Entries of anybody else do not lower or raise the state.
    assert_eq!(
        state_of(vec![
            status(peer, read()),
            status(other, pending()),
            status(unknown, pending()),
            status(own, pending()),
        ]),
        OutgoingState::Read
    );
    // A failure for somebody else is no failure of this message.
    assert_eq!(
        state_of(vec![status(peer, delivered()), status(other, failed())]),
        OutgoingState::Delivered
    );
    assert_eq!(
        state_of(vec![status(other, failed()), status(unknown, failed())]),
        OutgoingState::Sent { server_at: None },
        "no entry of the chat's contact is left: sent"
    );
    // The contact's own failure, however often the list has it: named once.
    assert_eq!(
        state_of(vec![
            status(peer, delivered()),
            status(peer, failed()),
            status(peer, failed()),
            status(peer, failed()),
        ]),
        OutgoingState::PartiallyFailed {
            failed: vec![user(0x21)]
        }
    );
    assert_eq!(
        state_of(vec![status(peer, failed()), status(peer, failed())]),
        OutgoingState::Failed {
            reason: FailReason::Network,
            retryable: false
        }
    );

    // Whoever a row names is in the stage: over a list that names everybody.
    let mut frames = base();
    frames.push(Frame::ChatItem(Box::new(outgoing_item(
        1,
        T,
        vec![
            status(peer, delivered()),
            status(peer, failed()),
            status(other, failed()),
            status(unknown, failed()),
            status(own, failed()),
        ],
    ))));
    let (staged, _) = map(frames);
    let staged_people: Vec<&UserId> = contacts(&staged).iter().map(|c| &c.user).collect();
    assert_eq!(staged_people, [&user(0x21)]);
    let Some(OutgoingState::PartiallyFailed { failed }) = &messages(&staged)[0].initial_out_state
    else {
        panic!("a partial failure");
    };
    assert_eq!(failed, &[user(0x21)]);
}

/// F3 of the review, the server's time: within a day of the import it is kept as the
/// archive has it, beyond that the row has none (the core then measures its edit and delete
/// windows from the sort time, which is bounded the same way).
#[test]
fn a_server_time_far_ahead_is_not_what_the_core_measures_from() {
    let now = u64::try_from(NOW).unwrap();
    let item = |sent: u64, server: Option<u64>| {
        let mut item = incoming_item(1, 2, sent, "x");
        let Some(DirectionalDetails::Incoming(details)) = &mut item.directional_details else {
            panic!("incoming");
        };
        details.date_server_sent = server;
        Frame::ChatItem(Box::new(item))
    };
    let mut frames = base();
    frames.push(item(T, Some(T + 3)));
    frames.push(item(T + 1, Some(now + 24 * HOUR)));
    frames.push(item(T + 2, Some(now + 24 * HOUR + 1)));
    frames.push(item(T + 3, Some(u64::MAX)));
    frames.push(item(T + 4, Some(0)));
    frames.push(item(T + 5, None));
    let (staged, end) = map(frames);
    let i = |n: u64| i64::try_from(n).unwrap();
    assert_eq!(
        messages(&staged)
            .iter()
            .map(|m| m.server_at)
            .collect::<Vec<_>>(),
        [
            Some(i(T + 3)),
            Some(i(now + 24 * HOUR)),
            None,
            None,
            None,
            None
        ]
    );
    assert_eq!(count(&end, reason::FUTURE_SERVER_TIME), 2);
    // The time a window is measured from (the server's, else the sort time) is never more
    // than a day after the import.
    for row in messages(&staged) {
        assert!(row.server_at.unwrap_or(row.sent_at) <= NOW + i(24 * HOUR));
    }
}

/// F3 of the review, the timer's start (D7): this computer's clock never moves a timer's
/// start back. With the clock an hour behind, a timer the phone started ten minutes ago is
/// "in the future" here; moved to the import's time it would run out fifty minutes before
/// the phone's. Only a start more than a day ahead is replaced.
#[test]
fn a_timer_start_is_the_phones_also_when_this_clock_is_behind() {
    let now = u64::try_from(NOW).unwrap();
    let i = |n: u64| i64::try_from(n).unwrap();
    let timed = |mut item: backup::ChatItem, start: Option<u64>| {
        item.expires_in_ms = Some(8 * HOUR);
        item.expire_start_date = start;
        Frame::ChatItem(Box::new(item))
    };
    let read_at = |mut item: backup::ChatItem, received: u64| {
        let Some(DirectionalDetails::Incoming(details)) = &mut item.directional_details else {
            panic!("incoming");
        };
        details.date_received = received;
        item
    };
    let mut frames = base();
    // Started fifty minutes "ahead" of this clock: kept to the millisecond.
    let ahead = now + 50 * 60 * 1000;
    frames.push(timed(
        incoming_item(1, 2, now - HOUR, "theirs"),
        Some(ahead),
    ));
    frames.push(timed(
        outgoing_item(1, now - HOUR + 1, vec![status(2, delivered())]),
        Some(ahead),
    ));
    // The bound itself is still the phone's time; one millisecond more is not.
    frames.push(timed(
        incoming_item(1, 2, now - HOUR + 2, "at the bound"),
        Some(now + 24 * HOUR),
    ));
    frames.push(timed(
        incoming_item(1, 2, now - HOUR + 3, "over the bound"),
        Some(now + 24 * HOUR + 1),
    ));
    frames.push(timed(
        outgoing_item(1, now - HOUR + 4, vec![status(2, delivered())]),
        Some(now + 24 * HOUR + 1),
    ));
    // Read on the phone without a stated start: it started when the phone received it at
    // the latest, and that time is the phone's too.
    frames.push(timed(
        read_at(incoming_item(1, 2, now - HOUR + 5, "read, no start"), ahead),
        None,
    ));
    let (staged, end) = map(frames);
    let starts: Vec<Option<i64>> = messages(&staged)
        .iter()
        .map(|m| m.expiry.and_then(|expiry| expiry.started_at))
        .collect();
    assert_eq!(
        starts,
        [
            Some(i(ahead)),
            Some(i(ahead)),
            Some(i(now + 24 * HOUR)),
            Some(NOW),
            Some(NOW),
            Some(i(ahead)),
        ]
    );
    assert_eq!(count(&end, reason::FUTURE_TIMER_START), 2);
    // What the row is due at is the phone's: its start and its timer.
    let first = messages(&staged)[0].expiry.unwrap();
    assert_eq!(first.expires_at(), Some(i(ahead + 8 * HOUR)));
}
