//! Review probes of the mapper (step S1's independent review): odd and hostile archives.
//! Synthetic data; counts only are printed. The three probes the review left ignored (the
//! send-status list, the partial failure for nobody, the server's time of the future) run
//! since the fixes that followed it; `fixes.rs` beside this file has the fixes' own tests.

use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, Item, OutgoingMessageDetails};
use backup::send_status::DeliveryStatus;

use super::*;

const T: u64 = 1_700_000_000_000;

/// The frames of one chat with person 0x21 (recipient 2, chat 1) and no item; person 0x22
/// (recipient 3) has no chat.
pub(super) fn base() -> Vec<Frame> {
    history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: Vec::new(),
        }],
        ..HistorySpec::default()
    })
}

pub(super) fn text_item(
    direction: DirectionalDetails,
    author: u64,
    sent: u64,
    body: &str,
) -> backup::ChatItem {
    backup::ChatItem {
        chat_id: 1,
        author_id: author,
        date_sent: sent,
        directional_details: Some(direction),
        item: Some(Item::StandardMessage(backup::StandardMessage {
            text: Some(backup::Text {
                body: body.to_owned(),
                body_ranges: Vec::new(),
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub(super) fn status(recipient_id: u64, delivery: DeliveryStatus) -> backup::SendStatus {
    backup::SendStatus {
        recipient_id,
        timestamp: 0,
        delivery_status: Some(delivery),
    }
}

pub(super) fn failed() -> DeliveryStatus {
    DeliveryStatus::Failed(backup::send_status::Failed { reason: 1 })
}

/// A send-status list is as long as a frame lets it be (the reader allows 100,000 fields in
/// a frame). The row names a person once however often the list does, and nobody but the
/// chat's own contact (the review found one user id per failed entry, and a person of
/// another chat who is not staged: 860 KB of JSON for a text of one byte).
#[test]
fn review_a_send_status_list_does_not_grow_the_row() {
    let mut frames = base();
    let mut statuses = vec![status(2, DeliveryStatus::Delivered(Default::default()))];
    // 20,000 failures for the chat's own contact, and one for a person of another chat.
    statuses.extend((0..20_000).map(|_| status(2, failed())));
    statuses.push(status(3, failed()));
    frames.push(Frame::ChatItem(Box::new(text_item(
        DirectionalDetails::Outgoing(OutgoingMessageDetails {
            send_status: statuses,
            date_received: T,
        }),
        1,
        T,
        "x",
    ))));
    let (staged, _) = map(frames);
    let rows = messages(&staged);
    let Some(OutgoingState::PartiallyFailed { failed }) = &rows[0].initial_out_state else {
        panic!("a partial failure");
    };
    let json = serde_json::to_vec(rows[0]).unwrap().len();
    println!(
        "{} entries in the failed list of one text of 1 byte; the row is {json} bytes of JSON",
        failed.len()
    );
    let staged_people: Vec<&UserId> = contacts(&staged).iter().map(|c| &c.user).collect();
    assert!(
        failed.iter().all(|user| staged_people.contains(&user)),
        "the row names a person the stage does not hold"
    );
    assert!(failed.len() <= 2, "one entry per person at most");
    assert_eq!(failed, &[user(0x21)], "the chat's own contact, once");
    assert!(json < 1024, "the row does not grow with the status list");
}

/// A failure that names nobody the mapper knows, beside a success: the row says "partly
/// failed" and names nobody, which the core's own rule never makes.
#[test]
fn review_a_partial_failure_names_somebody() {
    let mut frames = base();
    frames.push(Frame::ChatItem(Box::new(text_item(
        DirectionalDetails::Outgoing(OutgoingMessageDetails {
            send_status: vec![
                status(2, DeliveryStatus::Delivered(Default::default())),
                status(999, failed()),
            ],
            date_received: T,
        }),
        1,
        T,
        "x",
    ))));
    let (staged, _) = map(frames);
    let state = messages(&staged)[0].initial_out_state.clone().unwrap();
    assert!(
        !matches!(&state, OutgoingState::PartiallyFailed { failed } if failed.is_empty()),
        "partly failed, for nobody"
    );
    // The failure names nobody of this chat: what is left is the contact's own entry.
    assert_eq!(state, OutgoingState::Delivered);
}

/// The largest numbers every time field can hold: no panic, no overflow, and no time of
/// the future in a field the core orders or measures by.
#[test]
fn review_no_time_of_the_future_reaches_a_row() {
    let mut frames = base();
    let mut item = text_item(
        DirectionalDetails::Incoming(IncomingMessageDetails {
            date_received: u64::MAX,
            date_server_sent: Some(u64::MAX),
            read: true,
            sealed_sender: true,
        }),
        2,
        u64::MAX,
        "x",
    );
    item.expires_in_ms = Some(u64::MAX);
    item.expire_start_date = Some(u64::MAX);
    frames.push(Frame::ChatItem(Box::new(item)));
    let mut own = text_item(
        DirectionalDetails::Outgoing(OutgoingMessageDetails {
            send_status: Vec::new(),
            date_received: u64::MAX,
        }),
        1,
        u64::MAX - 1,
        "y",
    );
    own.expires_in_ms = Some(1);
    own.expire_start_date = Some(u64::MAX);
    frames.push(Frame::ChatItem(Box::new(own)));
    let (staged, end) = map(frames);
    let rows = messages(&staged);
    assert_eq!(rows.len(), 2);
    assert_eq!(count(&end, reason::FUTURE_TIME), 2);
    for row in &rows {
        assert_eq!(row.sent_at, NOW);
        assert_eq!(row.received_at, NOW);
        let expiry = row.expiry.unwrap();
        assert_eq!(expiry.started_at, Some(NOW));
        assert!(expiry.expires_at().is_some());
    }
    assert_eq!(&*rows[0].local, u64::MAX.to_string());
    println!(
        "server_at of the incoming row is {} days after the import",
        rows[0].server_at.map_or(0, |at| (at - NOW) / 86_400_000)
    );
    assert!(
        rows[0].server_at.is_none_or(|at| at <= NOW + 86_400_000),
        "the server's time is in the future"
    );
    assert_eq!(rows[0].server_at, None);
    assert_eq!(count(&end, reason::FUTURE_SERVER_TIME), 1);
    assert_eq!(count(&end, reason::FUTURE_TIMER_START), 2);
}

/// Ids at the edges: recipient and chat ids of `u64::MAX`, an item for chat 0 and by author
/// 0, a chat frame in front of its recipient, an item in front of its chat.
#[test]
fn review_ids_at_the_edges_and_frames_out_of_order() {
    use backup::recipient::Destination;
    let contact = |id: u64, i: u8| {
        Frame::Recipient(backup::Recipient {
            id,
            destination: Some(Destination::Contact(backup::Contact {
                aci: Some(aci(i).as_bytes().to_vec()),
                ..Default::default()
            })),
        })
    };
    let chat = |id: u64, recipient_id: u64| {
        Frame::Chat(backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        })
    };
    let incoming = |chat_id: u64, author: u64, sent: u64| {
        let mut item = text_item(
            DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: sent,
                read: true,
                ..Default::default()
            }),
            author,
            sent,
            "x",
        );
        item.chat_id = chat_id;
        Frame::ChatItem(Box::new(item))
    };
    let frames = vec![
        Frame::Recipient(backup::Recipient {
            id: 1,
            destination: Some(Destination::Self_(backup::Self_::default())),
        }),
        // An item before its chat, a chat before its recipient.
        incoming(u64::MAX, u64::MAX, T),
        chat(u64::MAX, u64::MAX),
        contact(u64::MAX, 0x31),
        // Now in order: the chat frame comes again and is taken (the first named nobody).
        chat(u64::MAX, u64::MAX),
        incoming(u64::MAX, u64::MAX, T + 1),
        // Chat 0 and author 0 name nothing.
        incoming(0, u64::MAX, T + 2),
        incoming(u64::MAX, 0, T + 3),
    ];
    let (staged, end) = map(frames);
    let rows = messages(&staged);
    assert_eq!(rows.len(), 1);
    assert_eq!(&*rows[0].local, (T + 1).to_string());
    assert_eq!(count(&end, reason::UNKNOWN_CHAT), 1 + 1 + 1);
    assert_eq!(count(&end, reason::UNKNOWN_AUTHOR), 1);
}

/// Our own recipient as the author of an incoming item, and an incoming item in Note to
/// Self: skipped and counted, never a row by "us" that the core takes for a stranger's.
#[test]
fn review_an_incoming_item_by_us_is_never_a_row() {
    let mut frames = history_frames(&HistorySpec {
        contacts: vec![person(0x21)],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: Vec::new(),
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: Vec::new(),
            },
        ],
        ..HistorySpec::default()
    });
    let incoming = |chat_id: u64, author: u64, sent: u64| {
        let mut item = text_item(
            DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: sent,
                read: false,
                ..Default::default()
            }),
            author,
            sent,
            "x",
        );
        item.chat_id = chat_id;
        Frame::ChatItem(Box::new(item))
    };
    frames.push(incoming(1, 1, T)); // by us, in the contact's chat
    frames.push(incoming(2, 1, T + 1)); // by us, in Note to Self
    frames.push(incoming(2, 2, T + 2)); // by the contact, in Note to Self
    let (staged, end) = map(frames);
    assert!(messages(&staged).is_empty());
    assert_eq!(count(&end, reason::UNKNOWN_AUTHOR), 2);
    assert_eq!(count(&end, reason::AUTHOR_NOT_IN_CHAT), 1);
    assert!(chats(&staged).is_empty(), "no chat without a row");
}

/// A megabyte of text, a text of control characters, and names of megabytes: cut or kept,
/// and every row still passes the contract check.
#[test]
fn review_large_and_odd_texts_stay_within_their_bounds() {
    let mut frames = history_frames(&HistorySpec {
        contacts: vec![ContactSpec {
            profile_name: "n".repeat(3 * 1024 * 1024),
            system_name: "\u{202e}\u{0}\u{7}".repeat(100_000),
            ..person(0x21)
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: Vec::new(),
        }],
        ..HistorySpec::default()
    });
    let incoming = |sent: u64, body: &str| {
        Frame::ChatItem(Box::new(text_item(
            DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: sent,
                read: true,
                ..Default::default()
            }),
            2,
            sent,
            body,
        )))
    };
    frames.push(incoming(T, &"é".repeat(2 * 1024 * 1024)));
    frames.push(incoming(T + 1, "\u{0}\u{1}\u{202e}\u{feff}\r\n\t"));
    frames.push(incoming(T + 2, " "));
    let (staged, end) = map(frames);
    let rows = messages(&staged);
    assert_eq!(rows.len(), 3);
    assert_eq!(body(rows[0]).len(), 128 * 1024);
    assert_eq!(count(&end, reason::TEXT_CUT), 1);
    let person = contacts(&staged)[0];
    assert!(person.profile_name.as_ref().unwrap().len() <= 256);
    assert!(person.system_name.as_ref().unwrap().len() <= 256);
    for row in &staged {
        if let Staged::Item { msg, .. } = row {
            let (envelope, _waiter) = Envelope::durable(vec![BackendEvent::HistoryBatch {
                conv: msg.conv.clone(),
                messages: vec![(**msg).clone()],
                has_more: false,
                progress: None,
            }]);
            assert!(envelope.validate().is_ok());
        }
    }
}

/// An edited item whose list of earlier versions is odd: the first is not the oldest, or
/// has no time. The id must still be one the live path could have given.
#[test]
fn review_the_id_of_an_edited_item_with_an_odd_revision_list() {
    let incoming = || IncomingMessageDetails {
        date_received: T + 900,
        read: true,
        ..Default::default()
    };
    let item = |revisions: Vec<u64>| {
        let mut item = text_item(DirectionalDetails::Incoming(incoming()), 2, T + 500, "last");
        item.revisions = revisions
            .into_iter()
            .map(|sent| text_item(DirectionalDetails::Incoming(incoming()), 2, sent, "old"))
            .collect();
        item
    };
    let local_of = |revisions: Vec<u64>| -> String {
        let mut frames = base();
        frames.push(Frame::ChatItem(Box::new(item(revisions))));
        let (staged, _) = map(frames);
        messages(&staged)[0].local.to_string()
    };
    // A first version without a time: the item's own time is all there is.
    assert_eq!(local_of(vec![0, T + 100]), (T + 500).to_string());
    // The list as the format says, oldest first.
    assert_eq!(local_of(vec![T, T + 100]), T.to_string());
    // Newest first (a producer that writes them the other way round): the mapper takes the
    // first entry, which is then NOT the original time.
    println!(
        "revisions newest first: the id is the time of revision {}",
        if local_of(vec![T + 100, T]) == T.to_string() {
            "oldest"
        } else {
            "first listed"
        }
    );
}

/// Many recipients and chats: the caps hold before memory grows, and the mapper's kept
/// state stays under its estimate.
#[test]
fn review_recipients_and_chats_over_the_caps_are_cut_before_memory_grows() {
    use backup::recipient::Destination;
    const RECIPIENTS: u64 = 220_000;
    const CHATS: u64 = 120_000;
    let mut mapper = mapper_with(MapLimits::default());
    let mut out = Vec::new();
    let name = "N".repeat(4_000);
    for id in 1..=RECIPIENTS {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&id.to_be_bytes());
        bytes[15] = 1;
        mapper.frame(
            Frame::Recipient(backup::Recipient {
                id,
                destination: Some(Destination::Contact(backup::Contact {
                    aci: Some(bytes.to_vec()),
                    profile_given_name: Some(name.clone()),
                    system_given_name: name.clone(),
                    nickname: Some(backup::contact::Name {
                        given: name.clone(),
                        family: name.clone(),
                    }),
                    e164: Some(15_550_000_000 + id),
                    ..Default::default()
                })),
            }),
            &mut out,
        );
    }
    for id in 1..=CHATS {
        mapper.frame(
            Frame::Chat(backup::Chat {
                id,
                recipient_id: id,
                ..Default::default()
            }),
            &mut out,
        );
    }
    let kept = mapper.recipients.len();
    let estimate = mapper.recipient_bytes;
    let chats_kept = mapper.chats.len();
    let end = mapper.finish();
    println!(
        "{RECIPIENTS} recipients with names of 4 KB: {kept} kept by an estimate of {} MiB; \
         {CHATS} chats: {chats_kept} kept",
        estimate / (1024 * 1024)
    );
    assert!(out.is_empty(), "nothing is staged without a row");
    assert!(kept <= 200_000);
    assert!(chats_kept <= 100_000);
    assert!(estimate <= 128 * 1024 * 1024);
    assert_eq!(
        count(&end, reason::RECIPIENT_OVER_CAP),
        RECIPIENTS - kept as u64
    );
}
