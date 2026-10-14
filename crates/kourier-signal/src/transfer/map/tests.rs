//! The mapper on frames alone: what each kind of frame becomes, what is skipped and under
//! which reason, and the bounds. Synthetic data; nothing of a row is printed.

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{ContactPatch, ConversationPatch};
use uuid::Uuid;

use super::*;
use crate::archive::writer::{
    ChatSpec, ChatWith, ContactSpec, GeneratedSpec, HistorySpec, SendStatusSpec, TextSpec,
    history_frames, sample_frames,
};

/// The time of the import in these tests.
const NOW: i64 = 1_760_000_000_000;

fn own_aci() -> Uuid {
    Uuid::from_bytes([0x11; 16])
}

fn own_pni() -> Uuid {
    Uuid::from_bytes([0x12; 16])
}

fn own_user() -> UserId {
    UserId::parse(&format!("aci:{}", own_aci().hyphenated())).unwrap()
}

fn aci(i: u8) -> Uuid {
    Uuid::from_bytes([i; 16])
}

fn user(i: u8) -> UserId {
    UserId::parse(&format!("aci:{}", aci(i).hyphenated())).unwrap()
}

fn mapper_with(limits: MapLimits) -> Mapper {
    Mapper::new(own_aci().into(), Pni::from(own_pni()), NOW, limits).unwrap()
}

/// Everything the frames make, and the mapper's end.
fn map_with(frames: Vec<Frame>, limits: MapLimits) -> (Vec<Staged>, MapEnd) {
    let mut mapper = mapper_with(limits);
    let mut out = Vec::new();
    for frame in frames {
        mapper.frame(frame, &mut out);
    }
    (out, mapper.finish())
}

fn map(frames: Vec<Frame>) -> (Vec<Staged>, MapEnd) {
    map_with(frames, MapLimits::default())
}

fn map_spec(spec: &HistorySpec) -> (Vec<Staged>, MapEnd) {
    map(history_frames(spec))
}

fn messages(staged: &[Staged]) -> Vec<&IncomingMessage> {
    staged
        .iter()
        .filter_map(|row| match row {
            Staged::Item { msg, .. } => Some(&**msg),
            _ => None,
        })
        .collect()
}

fn contacts(staged: &[Staged]) -> Vec<&StagedContact> {
    staged
        .iter()
        .filter_map(|row| match row {
            Staged::Contact { contact, .. } => Some(&**contact),
            _ => None,
        })
        .collect()
}

fn chats(staged: &[Staged]) -> Vec<(u32, &ConvKey, &StagedChat)> {
    staged
        .iter()
        .filter_map(|row| match row {
            Staged::Chat {
                idx, conv, chat, ..
            } => Some((*idx, conv, chat)),
            _ => None,
        })
        .collect()
}

fn body(msg: &IncomingMessage) -> &str {
    match &msg.body {
        Body::Text(text) => &text.body,
        _ => panic!("a text"),
    }
}

fn count(end: &MapEnd, reason: &str) -> u64 {
    end.counts.get(reason).copied().unwrap_or(0)
}

fn person(i: u8) -> ContactSpec {
    ContactSpec {
        aci: Some(aci(i)),
        profile_name: format!("Person {i}"),
        profile_sharing: true,
        ..ContactSpec::default()
    }
}

fn incoming(body: &str, sent_at: u64, read: bool) -> TextSpec {
    TextSpec {
        body: body.to_owned(),
        sent_at,
        received_at: sent_at + 7,
        server_at: Some(sent_at + 3),
        read,
        ..TextSpec::default()
    }
}

fn outgoing(body: &str, sent_at: u64, statuses: &[SendStatusSpec]) -> TextSpec {
    TextSpec {
        outgoing: true,
        body: body.to_owned(),
        sent_at,
        received_at: sent_at,
        statuses: statuses.to_vec(),
        ..TextSpec::default()
    }
}

fn one_chat(texts: Vec<TextSpec>) -> HistorySpec {
    HistorySpec {
        contacts: vec![person(0x21)],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts,
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    }
}

/// A history with every shape the test writer has: people with and without an ACI, chats
/// interleaved item by item, Note to Self, a person without a chat, a chat without an
/// item, the release notes with a chat, items that name unknown ids, generated messages.
fn rich() -> HistorySpec {
    let t0 = 1_700_000_000_000;
    HistorySpec {
        contacts: vec![
            person(0x21),
            ContactSpec {
                pni: Some(aci(0x22)),
                e164: Some(15_550_000_022),
                system_name: "By Pni".to_owned(),
                ..ContactSpec::default()
            },
            ContactSpec {
                e164: Some(15_550_000_023),
                system_name: "By Number".to_owned(),
                ..ContactSpec::default()
            },
            // Nobody writes to this one, and this one has a chat without an item.
            person(0x24),
            person(0x25),
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![
                    incoming("a1", t0 + 1, true),
                    outgoing("a2", t0 + 4, &[SendStatusSpec::Read]),
                    incoming("a3", t0 + 7, false),
                    TextSpec {
                        unknown_author: true,
                        ..incoming("from nobody", t0 + 8, true)
                    },
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![
                    incoming("b1", t0 + 2, false),
                    outgoing("b2", t0 + 5, &[SendStatusSpec::Delivered]),
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("c1", t0 + 3, true)],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", t0 + 6, &[SendStatusSpec::Read])],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(4),
                texts: Vec::new(),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Nobody,
                texts: vec![incoming("in a chat with nobody", t0 + 9, true)],
                ..ChatSpec::default()
            },
        ],
        interleaved: true,
        release_notes_items: Some(2),
        orphan_items: 3,
        generated: Some(GeneratedSpec {
            chats: 4,
            messages: 30,
            text: "generated".to_owned(),
            first_sent_at: t0 + 100_000,
        }),
        ..HistorySpec::default()
    }
}

#[test]
fn a_text_item_becomes_a_history_message_with_its_author_times_and_read_state() {
    let t = 1_700_000_000_000u64;
    let (staged, end) = map_spec(&one_chat(vec![
        incoming("read on the phone", t, true),
        incoming("not read yet", t + 10, false),
        outgoing("mine", t + 20, &[SendStatusSpec::Delivered]),
    ]));
    let rows = messages(&staged);
    assert_eq!(rows.len(), 3);
    let conv = ConvKey::Direct(user(0x21));

    let first = rows[0];
    assert_eq!(first.conv, conv);
    assert_eq!(&*first.local, t.to_string());
    assert_eq!(
        first.origin,
        Origin::Remote {
            author: user(0x21),
            device: None
        }
    );
    // The id is the live path's: the author's user id and the sent time in decimal.
    assert_eq!(first.net_id().author, Some(user(0x21)));
    assert_eq!(body(first), "read on the phone");
    assert_eq!(first.sent_at, i64::try_from(t).unwrap());
    assert_eq!(first.server_at, Some(i64::try_from(t + 3).unwrap()));
    assert_eq!(first.received_at, i64::try_from(t + 7).unwrap());
    assert!(first.read);
    assert!(first.silent, "history never notifies");
    assert!(!first.catch_up);
    assert_eq!(first.arrival, None);
    assert_eq!(first.initial_out_state, None);
    assert_eq!(first.expiry, None);

    assert!(!rows[1].read);
    assert!(rows[1].silent);

    let mine = rows[2];
    assert_eq!(mine.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(mine.net_id().author, None, "an own message has no author");
    assert_eq!(&*mine.local, (t + 20).to_string());
    assert_eq!(mine.initial_out_state, Some(OutgoingState::Delivered));
    assert_eq!(mine.server_at, None);

    // The chat's counters: three rows, all loud, the newest sort time. (What is unread is
    // each row's `read`: no count of it is staged or delivered.)
    assert_eq!(
        end.chats,
        [ChatCounters {
            idx: 0,
            n_rows: 3,
            n_loud: 3,
            last_at: i64::try_from(t + 20).unwrap(),
        }]
    );
    assert!(end.counts.is_empty(), "{:?}", end.counts);
}

#[test]
fn an_own_text_item_carries_the_minimum_of_its_send_states() {
    use SendStatusSpec as S;
    let state_of = |statuses: &[S]| -> OutgoingState {
        let (staged, _) = map_spec(&one_chat(vec![outgoing("x", 1_700_000_000_000, statuses)]));
        messages(&staged)[0].initial_out_state.clone().unwrap()
    };
    assert_eq!(
        state_of(&[S::Sent]),
        OutgoingState::Sent { server_at: None }
    );
    assert_eq!(state_of(&[S::Delivered]), OutgoingState::Delivered);
    assert_eq!(state_of(&[S::Read]), OutgoingState::Read);
    assert_eq!(state_of(&[S::Viewed]), OutgoingState::Viewed);
    assert_eq!(
        state_of(&[S::Pending]),
        OutgoingState::Sending { progress: None }
    );
    // The minimum over the entries.
    assert_eq!(
        state_of(&[S::Read, S::Delivered, S::Viewed]),
        OutgoingState::Delivered
    );
    assert_eq!(
        state_of(&[S::Viewed, S::Sent]),
        OutgoingState::Sent { server_at: None }
    );
    assert_eq!(
        state_of(&[S::Read, S::Pending]),
        OutgoingState::Sending { progress: None }
    );
    // "Skipped" does not count; with nothing left the message was sent.
    assert_eq!(state_of(&[S::Skipped, S::Read]), OutgoingState::Read);
    assert_eq!(
        state_of(&[S::Skipped]),
        OutgoingState::Sent { server_at: None }
    );
    assert_eq!(state_of(&[]), OutgoingState::Sent { server_at: None });
    // A failure beside a success is a partial failure that names who failed; alone it is a
    // failure with the phone's reason, and nothing here offers a retry.
    assert_eq!(
        state_of(&[S::Delivered, S::FailedNetwork]),
        OutgoingState::PartiallyFailed {
            failed: vec![user(0x21)]
        }
    );
    assert_eq!(
        state_of(&[S::FailedNetwork]),
        OutgoingState::Failed {
            reason: FailReason::Network,
            retryable: false
        }
    );
    assert_eq!(
        state_of(&[S::FailedIdentity, S::Skipped]),
        OutgoingState::Failed {
            reason: FailReason::UntrustedIdentity,
            retryable: false
        }
    );

    // Note to Self: the only entry is our own, which does not count.
    let (staged, _) = map_spec(&HistorySpec {
        chats: vec![ChatSpec {
            with: ChatWith::NoteToSelf,
            texts: vec![outgoing("note", 1_700_000_000_000, &[S::Pending])],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    });
    let note = messages(&staged)[0];
    assert_eq!(note.conv, ConvKey::Direct(own_user()));
    assert_eq!(
        note.initial_out_state,
        Some(OutgoingState::Sent { server_at: None })
    );
    let (_, conv, chat) = chats(&staged)[0];
    assert_eq!(conv, &ConvKey::Direct(own_user()));
    assert!(chat.is_self() && chat.peer().is_none());
    assert!(contacts(&staged).is_empty(), "we are nobody's contact");
}

#[test]
fn an_item_of_an_unknown_chat_or_author_is_skipped_and_counted() {
    let (staged, end) = map_spec(&rich());
    assert_eq!(
        count(&end, reason::UNKNOWN_CHAT),
        1 + 1 + 3,
        "the chat with nobody, its item, and the three orphans"
    );
    assert_eq!(count(&end, reason::UNKNOWN_AUTHOR), 1);
    assert_eq!(
        count(&end, reason::RELEASE_NOTES),
        1 + 2,
        "the chat and its two items"
    );
    let texts: Vec<&str> = messages(&staged).into_iter().map(body).collect();
    for gone in [
        "from nobody",
        "in a chat with nobody",
        "orphan",
        "release notes",
    ] {
        assert!(!texts.contains(&gone));
    }
    // Everything else arrived: 7 texts of the four chats and the 30 generated ones.
    assert_eq!(texts.len(), 7 + 30);

    // An incoming item whose author is a person, but not the chat's: not shown in a chat
    // it does not belong to. An incoming item that names us, or a group, has no author.
    let mut frames = history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![incoming("fine", 1_700_000_000_000, true)],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    });
    let stray = |author_id: u64, sent: u64| {
        let Some(Frame::ChatItem(item)) = frames.last().cloned() else {
            panic!("the last frame is the item");
        };
        let mut item = *item;
        item.author_id = author_id;
        item.date_sent = sent;
        Frame::ChatItem(Box::new(item))
    };
    let (other_person, us) = (stray(3, 1_700_000_000_100), stray(1, 1_700_000_000_200));
    frames.push(other_person);
    frames.push(us);
    let (staged, end) = map(frames);
    assert_eq!(messages(&staged).len(), 1);
    assert_eq!(count(&end, reason::AUTHOR_NOT_IN_CHAT), 1);
    assert_eq!(count(&end, reason::UNKNOWN_AUTHOR), 1);
    assert_eq!(
        contacts(&staged).len(),
        1,
        "the stray author was not staged"
    );
}

#[test]
fn a_chat_without_a_loud_row_is_not_staged_as_shown() {
    // The rule itself: the four notices Signal Desktop does not surface a chat for.
    let u = user(0x21);
    for quiet in [
        SystemEvent::IdentityKeyChanged { user: u.clone() },
        SystemEvent::NumberChanged { user: u.clone() },
        SystemEvent::MessageRequestAccepted,
        SystemEvent::MemberLeft { user: u.clone() },
    ] {
        assert!(!is_loud(&Body::System(quiet)));
    }
    assert!(is_loud(&Body::text("a message")));
    assert!(is_loud(&Body::System(SystemEvent::SessionRefreshed {
        user: u.clone()
    })));
    assert!(is_loud(&Body::Unsupported {
        kind_hint: "poll".into(),
        summary: None
    }));

    // A chat whose only row is quiet is staged (the row belongs to it) with no loud row, a
    // chat without any row is not staged at all, and a loud row makes a chat shown. This
    // step maps no notice from the archive, so the quiet row is given to the mapper's own
    // row step directly.
    let mut mapper = mapper_with(MapLimits::default());
    let mut out = Vec::new();
    for frame in history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22), person(0x23)],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: Vec::new(),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: Vec::new(),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("loud", 1_700_000_000_000, true)],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    }) {
        mapper.frame(frame, &mut out);
    }
    let quiet_conv = ConvKey::Direct(user(0x21));
    let conv = mapper.by_key[&quiet_conv];
    let mut notice = IncomingMessage::new(
        quiet_conv.clone(),
        "h:99:0",
        Origin::Remote {
            author: user(0x21),
            device: None,
        },
        Body::System(SystemEvent::IdentityKeyChanged { user: user(0x21) }),
        1_700_000_000_500,
        1_700_000_000_500,
    );
    notice.silent = true;
    mapper.row(conv, 99, notice, &mut out);
    let end = mapper.finish();
    let staged_convs: Vec<&ConvKey> = chats(&out).into_iter().map(|(_, conv, _)| conv).collect();
    assert_eq!(
        staged_convs,
        [&ConvKey::Direct(user(0x23)), &quiet_conv],
        "the chat without a row is not staged"
    );
    let quiet = end.chats.iter().find(|chat| chat.idx == 1).unwrap();
    assert_eq!((quiet.n_rows, quiet.n_loud), (1, 0));
    let loud = end.chats.iter().find(|chat| chat.idx == 0).unwrap();
    assert_eq!((loud.n_rows, loud.n_loud), (1, 1));
    assert_eq!(end.chats.iter().filter(|chat| chat.n_loud > 0).count(), 1);
}

#[test]
fn a_time_far_in_the_future_does_not_become_the_sort_time() {
    let now = u64::try_from(NOW).unwrap();
    let day = 24 * 60 * 60 * 1000;
    let far = now + 400 * day;
    let (staged, end) = map_spec(&one_chat(vec![
        // A sender whose clock is a year ahead; the phone received it last week.
        TextSpec {
            received_at: now - 7 * day,
            ..incoming("from next year", far, true)
        },
        // The same, and the received time is no better.
        TextSpec {
            received_at: far + 5,
            ..incoming("no sane time at all", far + 1, true)
        },
        // Ahead by less than a day: a clock that is a little off is kept as it is.
        TextSpec {
            received_at: now - 1,
            ..incoming("an hour ahead", now + 60 * 60 * 1000, true)
        },
        // Our own, far ahead, never received anywhere.
        TextSpec {
            received_at: 0,
            ..outgoing("mine from next year", far + 2, &[SendStatusSpec::Sent])
        },
    ]));
    let rows = messages(&staged);
    assert_eq!(rows.len(), 4);
    let i = |n: u64| i64::try_from(n).unwrap();
    // The id keeps the true value: it is what the other side knows the message by.
    assert_eq!(&*rows[0].local, far.to_string());
    assert_eq!(rows[0].sent_at, i(now - 7 * day));
    assert_eq!(rows[0].received_at, i(now - 7 * day));
    assert_eq!(&*rows[1].local, (far + 1).to_string());
    assert_eq!(rows[1].sent_at, NOW);
    assert_eq!(
        rows[1].received_at, NOW,
        "nothing was received in the future"
    );
    assert_eq!(rows[2].sent_at, i(now + 60 * 60 * 1000));
    assert_eq!(rows[2].received_at, i(now - 1));
    assert_eq!(rows[3].sent_at, NOW);
    assert_eq!(rows[3].received_at, NOW);
    assert_eq!(count(&end, reason::FUTURE_TIME), 3);
    // The chat does not stand in the future either.
    assert_eq!(end.chats[0].last_at, i(now + 60 * 60 * 1000));

    // An item without a sent time is not imported.
    let (staged, end) = map_spec(&one_chat(vec![incoming("no time", 0, true)]));
    assert!(messages(&staged).is_empty());
    assert_eq!(count(&end, reason::NO_TIME), 1);
}

#[test]
fn every_mapped_row_passes_the_contract_check() {
    let mut frames = history_frames(&rich());
    frames.extend(sample_frames());
    // Every content kind of step S3b, in the chat with the rich archive's first contact
    // (the frames of its people and chat are the rich archive's again: counted twice).
    frames.extend(history_frames(&one_chat(content::every_kind())));
    let (staged, end) = map(frames);
    assert!(messages(&staged).len() > 30);
    let rows = messages(&staged);
    let has = |what: &dyn Fn(&IncomingMessage) -> bool| rows.iter().any(|m| what(m));
    assert!(has(&|m| m.mentions_me
        && m.reply_to.is_some()
        && !m.edits.is_empty()));
    assert!(has(&|m| m.reactions.len() == 2));
    assert!(has(&|m| matches!(m.body, Body::Sticker(_))));
    for hint in [
        "sticker",
        "view-once",
        "contact",
        "payment",
        "gift-badge",
        "poll",
        "story-reply",
        "poll-ended",
        "pin",
        "newer-protocol",
    ] {
        assert!(
            has(&|m| matches!(&m.body, Body::Unsupported { kind_hint, .. } if kind_hint == hint)),
            "{hint}"
        );
    }
    assert!(has(&|m| matches!(
        m.body,
        Body::System(SystemEvent::TimerChanged { .. })
    )));
    let calls = rows
        .iter()
        .filter(|m| matches!(m.body, Body::System(SystemEvent::Call(_))))
        .count();
    assert_eq!(
        calls,
        1 + 10,
        "every 1:1 call; the group call is not of this chat"
    );
    let mut checked = 0;
    for row in &staged {
        let events = match row {
            Staged::Contact { contact, .. } => {
                vec![BackendEvent::ContactUpsert(ContactPatch::new(
                    contact.user.clone(),
                ))]
            }
            Staged::Chat { conv, .. } => {
                vec![BackendEvent::ConversationUpsert(ConversationPatch::new(
                    conv.clone(),
                ))]
            }
            Staged::Item { msg, .. } => vec![BackendEvent::HistoryBatch {
                conv: msg.conv.clone(),
                messages: vec![(**msg).clone()],
                has_more: false,
                progress: None,
            }],
        };
        let (envelope, _waiter) = Envelope::durable(events);
        assert!(
            envelope.validate().is_ok(),
            "a mapped row breaks the contract (row {checked})"
        );
        checked += 1;
    }
    // A row is staged after what it needs: its chat, and the chat after its peer.
    let mut seen_chats = std::collections::HashSet::new();
    let mut seen_people = std::collections::HashSet::new();
    for row in &staged {
        match row {
            Staged::Contact { n, .. } => assert!(seen_people.insert(*n), "a person twice"),
            Staged::Chat { idx, chat, .. } => {
                assert!(seen_chats.insert(*idx), "a chat twice");
                assert!(chat.peer().is_none_or(|peer| seen_people.contains(&peer)));
            }
            Staged::Item { chat, .. } => assert!(seen_chats.contains(chat)),
        }
    }
    // Every staged chat has its counters, and they add up to the rows.
    assert_eq!(end.chats.len(), seen_chats.len());
    assert_eq!(
        end.chats.iter().map(|chat| chat.n_rows).sum::<u64>(),
        messages(&staged).len() as u64
    );
    println!("{checked} mapped rows pass the contract check");
}

#[test]
fn people_and_chats_are_staged_only_when_a_row_uses_them() {
    let (staged, end) = map_spec(&rich());
    let users: Vec<&str> = contacts(&staged)
        .into_iter()
        .map(|contact| contact.user.as_str())
        .collect();
    // The three people with a row, in the order of their first row (the items interleave:
    // a1, b1, c1, …), then the four generated ones. Not the person without a chat, and not
    // the one whose chat has no item.
    let pni = format!("pni:{}", aci(0x22).hyphenated());
    assert_eq!(users[0], user(0x21).as_str());
    assert_eq!(users[1], pni);
    assert_eq!(users[2], "e164:+15550000023");
    assert_eq!(users.len(), 3 + 4);
    assert!(!users.contains(&user(0x24).as_str()));
    assert!(!users.contains(&user(0x25).as_str()));
    // Four chats with rows (three people and Note to Self) and the four generated ones.
    assert_eq!(chats(&staged).len(), 4 + 4);
    assert_eq!(end.chats.len(), 8);
    assert!(
        end.chats
            .iter()
            .all(|chat| chat.n_rows > 0 && chat.n_loud == chat.n_rows)
    );
    // Unread: a3 and b1 (the rows say so; the core counts them).
    let unread = messages(&staged)
        .into_iter()
        .filter(|m| !m.read && !m.origin.is_own())
        .count();
    assert_eq!(unread, 2);

    // What a person's row carries.
    let people = contacts(&staged);
    assert_eq!(people[0].aci, Some(aci(0x21)));
    assert_eq!(people[0].profile_name.as_deref(), Some("Person 33"));
    assert!(people[0].profile_sharing);
    assert_eq!(people[1].aci, None);
    assert_eq!(people[1].pni, Some(aci(0x22)));
    assert_eq!(people[1].e164.as_deref(), Some("+15550000022"));
    assert_eq!(people[1].system_name.as_deref(), Some("By Pni"));
    assert_eq!(people[2].e164.as_deref(), Some("+15550000023"));
    assert_eq!((people[2].aci, people[2].pni), (None, None));
}

#[test]
fn a_contact_frame_with_one_of_our_ids_is_us_and_one_without_any_id_is_nobody() {
    let t = 1_700_000_000_000;
    let spec = HistorySpec {
        contacts: vec![
            // The phone writes `self` for its own row; a contact frame with our ACI, or
            // with our PNI, is us all the same.
            ContactSpec {
                aci: Some(own_aci()),
                ..ContactSpec::default()
            },
            ContactSpec {
                pni: Some(own_pni()),
                e164: Some(15_550_000_001),
                ..ContactSpec::default()
            },
            // No ACI, no PNI, and a number that is none.
            ContactSpec {
                e164: Some(0),
                profile_name: "Nobody".to_owned(),
                ..ContactSpec::default()
            },
            // The same person as the first real one, in a second frame.
            person(0x21),
            person(0x21),
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![outgoing("to myself", t, &[])],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![outgoing("to myself again", t + 1, &[])],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("from nobody", t + 2, true)],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(3),
                texts: vec![incoming("from the person", t + 3, true)],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(4),
                texts: vec![incoming("from the second frame", t + 4, true)],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    let (staged, end) = map_spec(&spec);
    // Both chats with "us" are the one Note to Self; the first frame stands.
    let own = ConvKey::Direct(own_user());
    let person = ConvKey::Direct(user(0x21));
    let rows = messages(&staged);
    assert_eq!(
        rows.iter().map(|m| (body(m), &m.conv)).collect::<Vec<_>>(),
        [
            ("to myself", &own),
            ("to myself again", &own),
            ("from the person", &person),
            // The second frame of the person leads to the same person: its chat is the
            // same conversation, and nothing of it is lost.
            ("from the second frame", &person),
        ]
    );
    assert_eq!(chats(&staged).len(), 2, "one conversation per person");
    assert_eq!(contacts(&staged).len(), 1);
    assert_eq!(count(&end, reason::UNKNOWN_RECIPIENT), 1);
    assert_eq!(count(&end, reason::DUPLICATE_CONTACT), 1);
    assert_eq!(
        count(&end, reason::UNKNOWN_CHAT),
        1 + 1,
        "the chat of nobody and its item"
    );
    let note = end.chats.iter().find(|chat| chat.idx == 0).unwrap();
    assert_eq!(note.n_rows, 2);
    let theirs = end.chats.iter().find(|chat| chat.idx == 1).unwrap();
    assert_eq!(theirs.n_rows, 2);
}

#[test]
fn what_this_step_does_not_import_is_counted_by_its_reason() {
    use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, Item};
    use backup::recipient::Destination;
    let t = 1_700_000_000_000u64;
    let mut frames = history_frames(&one_chat(vec![incoming("kept", t, true)]));
    let item = |chat_id: u64, sent: u64, content: Option<Item>| {
        Frame::ChatItem(Box::new(backup::ChatItem {
            chat_id,
            author_id: 2,
            date_sent: sent,
            directional_details: Some(DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: sent + 1,
                read: true,
                ..Default::default()
            })),
            item: content,
            ..Default::default()
        }))
    };
    let standard = |message: backup::StandardMessage| Some(Item::StandardMessage(message));
    let text = |body: &str| {
        Some(backup::Text {
            body: body.to_owned(),
            body_ranges: Vec::new(),
        })
    };
    // A group, a story list and a call link as recipients; a chat on each.
    frames.push(Frame::Recipient(backup::Recipient {
        id: 50,
        destination: Some(Destination::Group(backup::Group {
            master_key: vec![9; 32],
            ..Default::default()
        })),
    }));
    frames.push(Frame::Recipient(backup::Recipient {
        id: 51,
        destination: Some(Destination::DistributionList(Default::default())),
    }));
    frames.push(Frame::Recipient(backup::Recipient {
        id: 52,
        destination: None,
    }));
    for (id, recipient_id) in [(50, 50), (51, 51), (52, 52)] {
        frames.push(Frame::Chat(backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        }));
    }
    frames.push(item(
        50,
        t + 1,
        standard(backup::StandardMessage {
            text: text("in a group"),
            ..Default::default()
        }),
    ));
    frames.push(item(
        51,
        t + 2,
        standard(backup::StandardMessage {
            text: text("in a story list"),
            ..Default::default()
        }),
    ));
    // Kinds of item in the direct chat (chat 1) that this step does not import.
    let kinds: Vec<(Option<Item>, &str)> = vec![
        (
            Some(Item::RemoteDeletedMessage(Default::default())),
            reason::DELETED,
        ),
        (
            Some(Item::AdminDeletedMessage(Default::default())),
            reason::DELETED,
        ),
        (
            Some(Item::UpdateMessage(Default::default())),
            reason::UNKNOWN_ITEM,
        ),
        (
            Some(Item::DirectStoryReplyMessage(Default::default())),
            reason::EMPTY,
        ),
        (None, reason::UNKNOWN_ITEM),
        (standard(backup::StandardMessage::default()), reason::EMPTY),
        (
            standard(backup::StandardMessage {
                text: text(""),
                ..Default::default()
            }),
            reason::EMPTY,
        ),
        // Attachments of which nothing is left (no pointer at all) and no text: nothing.
        (
            standard(backup::StandardMessage {
                text: text(""),
                attachments: vec![Default::default()],
                ..Default::default()
            }),
            reason::EMPTY,
        ),
    ];
    let mut expected: BTreeMap<&str, u64> = BTreeMap::new();
    // The attachment without a pointer of the last kind is counted on its own.
    expected.insert(reason::ATTACHMENT_NOT_AVAILABLE, 1);
    for (i, (content, why)) in kinds.into_iter().enumerate() {
        frames.push(item(1, t + 10 + i as u64, content));
        *expected.entry(why).or_insert(0) += 1;
    }
    // A text with a quote, a reaction and a range that name nothing: imported with the
    // quote of an unknown message, without the reaction and the range (counted).
    frames.push(item(
        1,
        t + 100,
        standard(backup::StandardMessage {
            text: Some(backup::Text {
                body: "with everything".to_owned(),
                body_ranges: vec![Default::default()],
            }),
            quote: Some(Default::default()),
            reactions: vec![Default::default()],
            ..Default::default()
        }),
    ));
    // An item without a direction, and a message that claims to be a notice.
    let mut no_direction = item(
        1,
        t + 101,
        standard(backup::StandardMessage {
            text: text("no direction"),
            ..Default::default()
        }),
    );
    if let Frame::ChatItem(item) = &mut no_direction {
        item.directional_details = None;
    }
    frames.push(no_direction);
    let mut directionless = item(
        1,
        t + 102,
        standard(backup::StandardMessage {
            text: text("directionless"),
            ..Default::default()
        }),
    );
    if let Frame::ChatItem(item) = &mut directionless {
        item.directional_details = Some(DirectionalDetails::Directionless(Default::default()));
    }
    frames.push(directionless);
    *expected.entry(reason::UNKNOWN_ITEM).or_insert(0) += 2;
    // Frames of kinds the model has nothing for.
    frames.push(Frame::Unknown);
    frames.push(Frame::ChatFolder(Default::default()));

    let (staged, end) = map(frames);
    let texts: Vec<&str> = messages(&staged).into_iter().map(body).collect();
    // Since step S4b a group's chat and its items are imported.
    assert_eq!(texts, ["kept", "in a group", "with everything"]);
    for (why, n) in &expected {
        assert_eq!(count(&end, why), *n, "{why}");
    }
    assert_eq!(count(&end, reason::UNKNOWN_RECIPIENT), 1);
    assert_eq!(
        count(&end, reason::UNKNOWN_CHAT),
        2 + 1,
        "two chats, one item"
    );
    assert_eq!(count(&end, reason::REACTION_UNKNOWN_AUTHOR), 1);
    assert_eq!(count(&end, reason::RANGE_DROPPED), 1);
    let quoted = messages(&staged)[2].reply_to.as_ref().expect("the quote");
    assert_eq!(
        (&quoted.target.author, &*quoted.target.local),
        (&TargetAuthor::Unknown, "0")
    );
    assert_eq!(
        chats(&staged).len(),
        2,
        "the group became a chat, no list did"
    );
}

/// D7: the mapper never drops a row because this computer's clock says it is due. A row
/// carries its timer and its start; what is due is decided where the clock is known.
#[test]
fn nothing_is_skipped_for_being_due_by_this_computers_clock() {
    let now = u64::try_from(NOW).unwrap();
    let hour = 60 * 60 * 1000;
    let timed = |spec: TextSpec, expires_in: u64, start: Option<u64>| TextSpec {
        expires_in_ms: Some(expires_in),
        expire_start: start,
        ..spec
    };
    let (staged, end) = map_spec(&one_chat(vec![
        // Ran out an hour ago by this clock.
        timed(
            incoming("due long ago", now - 3 * hour, true),
            hour,
            Some(now - 2 * hour),
        ),
        // Still running.
        timed(
            incoming("running", now - hour, true),
            8 * hour,
            Some(now - hour),
        ),
        // Unread: it starts when it is read here.
        timed(incoming("unread", now - hour + 1, false), hour, None),
        // Read on the phone, which did not say when the timer started.
        timed(
            incoming("read without a start", now - hour + 2, true),
            hour,
            None,
        ),
        // A start more than a day ahead is no time a timer started at: the import's time.
        timed(
            incoming("started the day after tomorrow", now - hour + 3, true),
            hour,
            Some(now + 24 * hour + 1),
        ),
        // Our own: running since it was sent, and due long ago.
        timed(
            outgoing("mine, due", now - 5 * hour, &[SendStatusSpec::Read]),
            hour,
            Some(now - 5 * hour),
        ),
        // Our own, not sent yet: its timer never started.
        timed(
            outgoing("mine, pending", now - hour + 4, &[SendStatusSpec::Pending]),
            hour,
            None,
        ),
        // A timer longer than any Signal allows is cut to the bound.
        timed(
            incoming("for ever", now - hour + 5, true),
            u64::MAX,
            Some(now - hour),
        ),
        // No timer.
        incoming("plain", now - hour + 6, true),
        // Our own, failed: its timer never started either.
        timed(
            outgoing(
                "mine, failed",
                now - hour + 7,
                &[SendStatusSpec::FailedNetwork],
            ),
            hour,
            None,
        ),
        // Our own, sent as the phone says, and yet without a start.
        timed(
            outgoing("mine, sent", now - 3 * hour, &[SendStatusSpec::Delivered]),
            hour,
            None,
        ),
    ]));
    let rows = messages(&staged);
    assert_eq!(rows.len(), 11, "no row was dropped for being due");
    let i = |n: u64| i64::try_from(n).unwrap();
    let hour_timer = Duration::from_millis(hour);
    let expiry = |kind, timer, started_at| {
        Some(Expiry {
            kind,
            timer,
            started_at,
        })
    };
    use DisappearKind::{AfterRead, AfterSend};
    assert_eq!(
        rows[0].expiry,
        expiry(AfterRead, hour_timer, Some(i(now - 2 * hour)))
    );
    assert!(
        rows[0].expiry.unwrap().expires_at().unwrap() < NOW,
        "it is due, and it is kept"
    );
    assert_eq!(
        rows[1].expiry,
        expiry(
            AfterRead,
            Duration::from_millis(8 * hour),
            Some(i(now - hour))
        )
    );
    assert_eq!(rows[2].expiry, expiry(AfterRead, hour_timer, None));
    assert!(!rows[2].read);
    assert_eq!(
        rows[3].expiry,
        expiry(AfterRead, hour_timer, Some(rows[3].received_at)),
        "never without a start: the core would start it at the import"
    );
    assert_eq!(rows[4].expiry, expiry(AfterRead, hour_timer, Some(NOW)));
    assert_eq!(
        rows[5].expiry,
        expiry(AfterSend, hour_timer, Some(i(now - 5 * hour)))
    );
    // D5: not sent, so the timer is kept WITHOUT a start (the core leaves it alone until
    // the message's echo says it was sent). Not dropped, and not started by the mapper.
    assert_eq!(rows[6].expiry, expiry(AfterSend, hour_timer, None));
    assert!(matches!(
        rows[6].initial_out_state,
        Some(OutgoingState::Sending { .. })
    ));
    assert_eq!(rows[9].expiry, expiry(AfterSend, hour_timer, None));
    assert!(matches!(
        rows[9].initial_out_state,
        Some(OutgoingState::Failed { .. })
    ));
    // Sent, the phone says, with a timer that never started: the timer is kept (a
    // disappearing message never stays for ever) and starts at the IMPORT's time. Given
    // without a start the core would count from the sent time and erase what the phone
    // still shows.
    assert_eq!(rows[10].expiry, expiry(AfterSend, hour_timer, Some(NOW)));
    assert!(
        rows[10].expiry.unwrap().expires_at().unwrap() > NOW,
        "it is not due at the import"
    );
    assert_eq!(rows[10].initial_out_state, Some(OutgoingState::Delivered));
    assert_eq!(
        rows[7].expiry,
        expiry(
            AfterRead,
            Duration::from_secs(i32::MAX as u64),
            Some(i(now - hour))
        )
    );
    assert_eq!(rows[8].expiry, None);
    assert_eq!(count(&end, reason::TIMER_NOT_STARTED), 3);
    assert_eq!(count(&end, reason::FUTURE_TIMER_START), 1);
    assert_eq!(end.chats[0].n_rows, 11);
}

/// The lead's decision on the timer of an own item that the phone calls SENT and has no
/// start for: the timer is kept and starts at the IMPORT's time (never without a timer, which
/// would leave a disappearing message for ever; never from the sent time, which would erase
/// what the phone still shows). It is still counted `timer_not_started`. An item that was not
/// sent keeps its timer without a start; one with a start keeps the phone's.
#[test]
fn an_own_item_the_phone_calls_sent_keeps_its_timer_started_at_the_import() {
    let now = u64::try_from(NOW).unwrap();
    let hour = 60 * 60 * 1000;
    let spec = |at: u64| {
        let timed = |spec: TextSpec, start: Option<u64>| TextSpec {
            expires_in_ms: Some(hour),
            expire_start: start,
            ..spec
        };
        one_chat(vec![
            timed(outgoing("sent", at, &[SendStatusSpec::Sent]), None),
            timed(
                outgoing("delivered", at + 1, &[SendStatusSpec::Delivered]),
                None,
            ),
            timed(outgoing("read", at + 2, &[SendStatusSpec::Read]), None),
            timed(outgoing("viewed", at + 3, &[SendStatusSpec::Viewed]), None),
            // Nobody it was meant for is left (the entries say "skipped"): sent.
            timed(
                outgoing("to nobody", at + 4, &[SendStatusSpec::Skipped]),
                None,
            ),
            // Not sent: the timer without a start.
            timed(
                outgoing("pending", at + 5, &[SendStatusSpec::Pending]),
                None,
            ),
            timed(
                outgoing("failed", at + 6, &[SendStatusSpec::FailedIdentity]),
                None,
            ),
            // Sent with the phone's start: the phone's.
            timed(
                outgoing("started", at + 7, &[SendStatusSpec::Read]),
                Some(at + 8),
            ),
        ])
    };
    let history = spec(now - 3 * hour);
    let start_of = |staged: &[Staged]| -> Vec<Option<i64>> {
        messages(staged)
            .iter()
            .map(|m| {
                let expiry = m.expiry.expect("every item keeps its timer");
                assert_eq!(expiry.kind, DisappearKind::AfterSend);
                assert_eq!(expiry.timer, Duration::from_millis(hour));
                expiry.started_at
            })
            .collect()
    };
    let (staged, end) = map_spec(&history);
    let sent_at = |n: u64| i64::try_from(now - 3 * hour + n).unwrap();
    assert_eq!(
        start_of(&staged),
        [
            Some(NOW),
            Some(NOW),
            Some(NOW),
            Some(NOW),
            Some(NOW),
            None,
            None,
            Some(sent_at(8)),
        ]
    );
    assert_eq!(
        count(&end, reason::TIMER_NOT_STARTED),
        7,
        "all seven that had no start are counted, the one with a start is not"
    );
    for row in messages(&staged).iter().take(5) {
        let at = row.expiry.unwrap().expires_at().unwrap();
        assert!(at > NOW, "it is not due at the import, however old it is");
    }

    // The archive read again an hour later: the mapper is pure and its start is the time of
    // THAT import. (The core ignores the `expiry` of a copy of a row it has, so the first
    // import's start stands: `kourier-core/tests/history_rules.rs`.)
    let mut later = Mapper::new(
        own_aci().into(),
        Pni::from(own_pni()),
        NOW + hour as i64,
        MapLimits::default(),
    )
    .unwrap();
    let mut again = Vec::new();
    for frame in history_frames(&history) {
        later.frame(frame, &mut again);
    }
    let starts = start_of(&again);
    assert_eq!(starts[0], Some(NOW + hour as i64));
    assert_eq!(starts[5], None);
    assert_eq!(starts[7], Some(sent_at(8)));
}

#[test]
fn the_id_of_an_edited_item_is_the_time_of_its_first_version() {
    let t = 1_700_000_000_000u64;
    let mut frames = history_frames(&one_chat(vec![incoming("the last version", t + 500, true)]));
    let Some(Frame::ChatItem(item)) = frames.last_mut() else {
        panic!("the item");
    };
    // The archive lists the earlier versions oldest first; the item states its last edit.
    let earlier = |sent: u64| backup::ChatItem {
        date_sent: sent,
        ..(**item).clone()
    };
    item.revisions = vec![earlier(t), earlier(t + 200)];
    let (staged, end) = map(frames);
    let row = messages(&staged)[0];
    assert_eq!(
        &*row.local,
        t.to_string(),
        "the id the live path gave the message"
    );
    assert_eq!(row.sent_at, i64::try_from(t).unwrap());
    assert_eq!(body(row), "the last version");
    // The later versions' ids, oldest first: the second version and the item itself.
    let edits: Vec<(&str, i64)> = row.edits.iter().map(|e| (&*e.local, e.at)).collect();
    let (second, last) = ((t + 200).to_string(), (t + 500).to_string());
    assert_eq!(
        edits,
        [
            (second.as_str(), i64::try_from(t + 200).unwrap()),
            (last.as_str(), i64::try_from(t + 500).unwrap())
        ]
    );
    assert!(end.counts.is_empty(), "{:?}", end.counts);
}

#[test]
fn ids_that_collide_and_frames_over_the_caps_are_counted() {
    let t = 1_700_000_000_000u64;
    // The same recipient id and the same chat id twice, and the id 0: the first stands.
    let mut frames = history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![incoming("to the first", t, true)],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    });
    let again = |frames: &[Frame], kind: &str, id: u64| -> Frame {
        let mut frame = frames
            .iter()
            .rev()
            .find(|frame| frame.kind() == kind)
            .cloned()
            .unwrap();
        match &mut frame {
            Frame::Recipient(recipient) => recipient.id = id,
            Frame::Chat(chat) => chat.id = id,
            _ => unreachable!(),
        }
        frame
    };
    // Person 0x22's frame again under the first person's id, and under 0; the chat's frame
    // again under its own id (now with the other person), and under 0.
    let item = frames.pop().unwrap();
    frames.push(again(&frames, "recipient", 2));
    frames.push(again(&frames, "recipient", 0));
    let mut second_chat = again(&frames, "chat", 1);
    if let Frame::Chat(chat) = &mut second_chat {
        chat.recipient_id = 3;
    }
    frames.push(second_chat);
    frames.push(again(&frames, "chat", 0));
    frames.push(item);
    let (staged, end) = map(frames);
    assert_eq!(count(&end, reason::DUPLICATE_ID), 4);
    assert_eq!(messages(&staged)[0].conv, ConvKey::Direct(user(0x21)));

    // Our own recipient and two people fit; the third and fourth person are not kept, and
    // what names them is unknown. Then the same for chats.
    let small = MapLimits {
        max_recipients: 3,
        ..MapLimits::default()
    };
    let spec = HistorySpec {
        contacts: vec![person(0x21), person(0x22), person(0x23), person(0x24)],
        chats: (0..4)
            .map(|i| ChatSpec {
                with: ChatWith::Contact(i),
                texts: vec![incoming("hello", t + i as u64, true)],
                ..ChatSpec::default()
            })
            .collect(),
        ..HistorySpec::default()
    };
    let (staged, end) = map_with(history_frames(&spec), small);
    // Our own recipient and two people are kept.
    assert_eq!(count(&end, reason::RECIPIENT_OVER_CAP), 2);
    assert_eq!(messages(&staged).len(), 2);
    assert_eq!(count(&end, reason::UNKNOWN_CHAT), 2 + 2);
    let (staged, end) = map_with(
        history_frames(&spec),
        MapLimits {
            max_chats: 2,
            ..MapLimits::default()
        },
    );
    assert_eq!(count(&end, reason::CHAT_OVER_CAP), 2);
    assert_eq!(messages(&staged).len(), 2);
    assert_eq!(count(&end, reason::UNKNOWN_CHAT), 2);
    // The memory bound counts too: with room for no person at all, none is kept.
    let (staged, end) = map_with(
        history_frames(&spec),
        MapLimits {
            max_recipient_bytes: 200,
            ..MapLimits::default()
        },
    );
    assert!(messages(&staged).is_empty());
    assert!(count(&end, reason::RECIPIENT_OVER_CAP) >= 4);
}

#[test]
fn a_text_and_a_name_over_their_bounds_are_cut_on_a_character() {
    let limits = MapLimits {
        max_text_bytes: 10,
        max_name_bytes: 5,
        ..MapLimits::default()
    };
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            // Two bytes a letter: five bytes are two letters and a half.
            profile_name: "ééééé".to_owned(),
            ..person(0x21)
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                incoming("abcdefgh€€", 1_700_000_000_000, true),
                incoming("short", 1_700_000_000_001, true),
            ],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    };
    let (staged, end) = map_with(history_frames(&spec), limits);
    let rows = messages(&staged);
    // "abcdefgh" is eight bytes, the euro sign three: it does not fit into ten.
    assert_eq!(body(rows[0]), "abcdefgh");
    assert_eq!(body(rows[1]), "short");
    assert_eq!(count(&end, reason::TEXT_CUT), 1);
    assert_eq!(contacts(&staged)[0].profile_name.as_deref(), Some("éé"));
    // The default bounds are the design's.
    let default = MapLimits::default();
    assert_eq!(default.max_text_bytes, 128 * 1024);
    assert_eq!(default.max_name_bytes, 256);
    assert_eq!(
        (default.max_recipients, default.max_chats),
        (200_000, 100_000)
    );
    assert_eq!(default.max_recipient_bytes, 128 * 1024 * 1024);
    assert_eq!(default.max_ahead_ms, 24 * 60 * 60 * 1000);
}

/// The sample archive of the link-time tests: one chat, an own message, two incoming ones
/// (the unread one has a reaction, which this step leaves out).
#[test]
fn the_sample_archive_becomes_one_chat_of_three_texts() {
    let (staged, end) = map(sample_frames());
    let rows = messages(&staged);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(
        rows[0].initial_out_state,
        Some(OutgoingState::Sent { server_at: None })
    );
    assert!(rows[1].read && !rows[2].read);
    assert_eq!(end.chats.len(), 1);
    assert_eq!(end.chats[0].n_rows, 3);
    // Our own reaction on the third text.
    assert_eq!(
        rows[2].reactions,
        [HistoryReaction {
            by: None,
            emoji: "\u{1f44d}".to_owned(),
            at: 3300
        }]
    );
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    assert_eq!(contacts(&staged).len(), 1);
    assert_eq!(contacts(&staged)[0].profile_key, Some([2; 32]));
    assert_eq!(
        contacts(&staged)[0].profile_name.as_deref(),
        Some("Sample Contact")
    );
}

/// What a person's staged row takes from the contact frame, and what it leaves.
#[test]
fn a_staged_person_carries_what_the_contact_frame_says() {
    use backup::recipient::Destination;
    let t = 1_700_000_000_000u64;
    let mut frames = history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![incoming("one", t, true)],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![incoming("two", t + 1, true)],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    });
    for frame in &mut frames {
        let Frame::Recipient(backup::Recipient {
            id,
            destination: Some(Destination::Contact(contact)),
        }) = frame
        else {
            continue;
        };
        if *id == 2 {
            contact.blocked = true;
            contact.visibility = backup::contact::Visibility::HiddenMessageRequest as i32;
            contact.registration = Some(backup::contact::Registration::NotRegistered(
                backup::contact::NotRegistered {
                    unregistered_timestamp: 1_650_000_000_000,
                },
            ));
            contact.profile_key = Some(vec![5; 32]);
            contact.profile_given_name = Some("  Given ".to_owned());
            contact.profile_family_name = Some("Family".to_owned());
            contact.system_given_name = String::new();
            contact.system_nickname = "Nick".to_owned();
            contact.nickname = Some(backup::contact::Name {
                given: "Pet".to_owned(),
                family: "Name".to_owned(),
            });
            contact.e164 = Some(15_550_000_021);
            // Never taken: the identity key and its state come with the Storage Service.
            contact.identity_key = Some(vec![5; 33]);
            contact.identity_state = backup::contact::IdentityState::Verified as i32;
        } else {
            // A profile key of the wrong length, a visibility and a registration this
            // version does not know: none of them is taken for something else.
            contact.profile_key = Some(vec![5; 31]);
            contact.visibility = 77;
            contact.registration = None;
            contact.profile_sharing = false;
        }
    }
    let (staged, end) = map(frames);
    let people = contacts(&staged);
    assert_eq!(people.len(), 2);
    let first = people[0];
    assert!(first.blocked && first.hidden && first.profile_sharing);
    assert_eq!(first.unregistered_at, Some(1_650_000_000_000));
    assert_eq!(first.profile_key, Some([5; 32]));
    assert_eq!(first.profile_name.as_deref(), Some("Given Family"));
    assert_eq!(first.system_name.as_deref(), Some("Nick"));
    assert_eq!(first.nickname.as_deref(), Some("Pet Name"));
    assert_eq!(first.e164.as_deref(), Some("+15550000021"));
    assert_eq!(
        first.user,
        user(0x21),
        "the ACI names the person, not the number"
    );
    let second = people[1];
    assert!(!second.blocked && !second.hidden && !second.profile_sharing);
    assert_eq!(second.unregistered_at, None);
    assert_eq!(second.profile_key, None);
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    // The staged row has no place for an identity key at all.
    let json = serde_json::to_string(first).unwrap();
    assert!(!json.contains("identity"));
}

/// Probes of the independent review of step S1.
mod content;
mod fixes;
mod frames;
mod groups;
mod media;
mod review;
