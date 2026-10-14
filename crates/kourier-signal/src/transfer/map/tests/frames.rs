//! Step S4a: what a chat frame carries (archived, marked unread, muted until, the 1:1 timer
//! and its version, the pinned order), staged with its conversation. Synthetic data.

use super::*;

const T: u64 = 1_700_000_000_000;

/// The staged chat of `conv`, with its pinned order.
fn staged_chat<'a>(staged: &'a [Staged], conv: &ConvKey) -> Option<(&'a StagedChat, Option<u32>)> {
    staged.iter().find_map(|row| match row {
        Staged::Chat {
            conv: c,
            chat,
            pinned,
            ..
        } if c == conv => Some((chat, *pinned)),
        _ => None,
    })
}

fn chat_with(contact: usize, spec: ChatSpec) -> ChatSpec {
    ChatSpec {
        with: ChatWith::Contact(contact),
        texts: vec![incoming("hello", T + contact as u64, true)],
        ..spec
    }
}

#[test]
fn a_chat_frame_gives_its_conversation_the_flags_the_mute_and_the_timer() {
    let (staged, _) = map_spec(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        chats: vec![
            chat_with(
                0,
                ChatSpec {
                    archived: true,
                    marked_unread: true,
                    mute_until_ms: Some(T + 3_600_000),
                    expiration_timer_ms: Some(86_400_000),
                    expire_timer_version: 3,
                    pinned_order: Some(2),
                    ..ChatSpec::default()
                },
            ),
            chat_with(1, ChatSpec::default()),
        ],
        ..HistorySpec::default()
    });
    let (chat, pinned) = staged_chat(&staged, &ConvKey::Direct(user(0x21))).unwrap();
    assert_eq!(chat.peer(), Some(0));
    assert!(!chat.is_self());
    assert_eq!(
        chat.frame,
        ChatFrame {
            archived: true,
            marked_unread: true,
            muted_until: Some(i64::try_from(T).unwrap() + 3_600_000),
            timer_secs: Some(86_400),
            timer_version: 3,
        }
    );
    assert_eq!(pinned, Some(2));
    // A frame that says nothing: no flag, no mute, no timer, the first version.
    let (chat, pinned) = staged_chat(&staged, &ConvKey::Direct(user(0x22))).unwrap();
    assert_eq!(chat.frame, ChatFrame::default());
    assert_eq!(chat.frame.timer_version, 1);
    assert_eq!(pinned, None);
}

#[test]
fn a_mute_for_ever_and_a_mute_in_the_past_are_converted_as_the_storage_sync_does() {
    for (given, want) in [
        // Android's "for ever" (`Long.MAX_VALUE`), and anything above it.
        (i64::MAX as u64, Some(i64::MAX)),
        (u64::MAX, Some(i64::MAX)),
        // A mute that ran out stays a time in the past: nothing is judged by this clock.
        (1_000, Some(1_000)),
        (0, None),
    ] {
        let (staged, _) = map_spec(&HistorySpec {
            contacts: vec![person(0x21)],
            chats: vec![chat_with(
                0,
                ChatSpec {
                    mute_until_ms: Some(given),
                    ..ChatSpec::default()
                },
            )],
            ..HistorySpec::default()
        });
        let (chat, _) = staged_chat(&staged, &ConvKey::Direct(user(0x21))).unwrap();
        assert_eq!(chat.frame.muted_until, want, "{given}");
        assert_eq!(
            chat.frame.muted_until,
            crate::storage::muted_until(given),
            "one conversion for the archive and the sync"
        );
    }
}

#[test]
fn the_timer_version_is_kept_without_a_timer_and_the_timer_is_bounded() {
    for (ms, version, want) in [
        (None, 5, (None, 5)),
        (Some(0), 2, (None, 2)),
        // Less than a second is no timer of whole seconds.
        (Some(999), 2, (None, 2)),
        (Some(30_000), 0, (Some(30), 1)),
        // At most `i32::MAX` seconds, the live path's bound.
        (Some(u64::MAX), 9, (Some(i32::MAX as u32), 9)),
    ] {
        let (staged, _) = map_spec(&HistorySpec {
            contacts: vec![person(0x21)],
            chats: vec![chat_with(
                0,
                ChatSpec {
                    expiration_timer_ms: ms,
                    expire_timer_version: version,
                    ..ChatSpec::default()
                },
            )],
            ..HistorySpec::default()
        });
        let (chat, _) = staged_chat(&staged, &ConvKey::Direct(user(0x21))).unwrap();
        assert_eq!(
            (chat.frame.timer_secs, chat.frame.timer_version),
            want,
            "{ms:?} ms, version {version}"
        );
    }
}

#[test]
fn the_first_chat_frame_of_a_conversation_wins() {
    let (staged, end) = map_spec(&HistorySpec {
        contacts: vec![person(0x21)],
        chats: vec![
            chat_with(
                0,
                ChatSpec {
                    archived: true,
                    expiration_timer_ms: Some(60_000),
                    expire_timer_version: 2,
                    ..ChatSpec::default()
                },
            ),
            chat_with(
                0,
                ChatSpec {
                    marked_unread: true,
                    mute_until_ms: Some(i64::MAX as u64),
                    pinned_order: Some(1),
                    expiration_timer_ms: Some(120_000),
                    expire_timer_version: 7,
                    ..ChatSpec::default()
                },
            ),
        ],
        ..HistorySpec::default()
    });
    let chats = chats(&staged);
    assert_eq!(chats.len(), 1, "one conversation");
    let (chat, pinned) = staged_chat(&staged, &ConvKey::Direct(user(0x21))).unwrap();
    assert_eq!(
        chat.frame,
        ChatFrame {
            archived: true,
            timer_secs: Some(60),
            timer_version: 2,
            ..ChatFrame::default()
        }
    );
    assert_eq!(pinned, None, "the second frame's pin is not taken either");
    assert_eq!(end.chats[0].n_rows, 2, "the rows of both frames");
}

#[test]
fn a_pinned_chat_is_staged_and_shown_without_a_loud_row_or_any_row() {
    let (staged, end) = map_spec(&HistorySpec {
        contacts: vec![person(0x21), person(0x22), person(0x23)],
        chats: vec![
            // Pinned, no item: staged at its frame, with its person.
            ChatSpec {
                with: ChatWith::Contact(0),
                pinned_order: Some(4),
                ..ChatSpec::default()
            },
            // Not pinned (0 is no order), no item: not staged.
            ChatSpec {
                with: ChatWith::Contact(1),
                pinned_order: Some(0),
                ..ChatSpec::default()
            },
            // Note to Self, pinned, no item.
            ChatSpec {
                with: ChatWith::NoteToSelf,
                pinned_order: Some(1),
                ..ChatSpec::default()
            },
            chat_with(2, ChatSpec::default()),
        ],
        ..HistorySpec::default()
    });
    let (chat, pinned) = staged_chat(&staged, &ConvKey::Direct(user(0x21))).unwrap();
    assert_eq!(pinned, Some(4));
    let peer = chat.peer().unwrap();
    assert!(
        staged
            .iter()
            .any(|row| matches!(row, Staged::Contact { n, .. } if *n == peer)),
        "its person is staged before it"
    );
    assert!(staged_chat(&staged, &ConvKey::Direct(user(0x22))).is_none());
    let (own, pinned) = staged_chat(&staged, &ConvKey::Direct(own_user())).unwrap();
    assert!(own.is_self());
    assert_eq!(pinned, Some(1));
    assert_eq!(chats(&staged).len(), 3);
    assert!(
        end.chats
            .iter()
            .any(|chat| chat.n_rows == 0 && chat.n_loud == 0),
        "a staged chat without a row"
    );
    assert_eq!(
        contacts(&staged).len(),
        2,
        "not the person of the chat that is not staged"
    );
}

#[test]
fn only_the_first_pins_in_the_phones_order_keep_their_pin() {
    // Five pinned chats whose orders are not in the archive's order and have gaps, and a
    // bound of three: the pins 10, 20 and 30 stay; the chats with 40 and 50 stay chats and
    // lose the pin, counted once each.
    let orders = [50, 10, 40, 20, 30];
    let spec = HistorySpec {
        contacts: (0..5).map(|i| person(0x21 + i)).collect(),
        chats: orders
            .iter()
            .enumerate()
            .map(|(i, order)| {
                chat_with(
                    i,
                    ChatSpec {
                        pinned_order: Some(*order),
                        ..ChatSpec::default()
                    },
                )
            })
            .collect(),
        ..HistorySpec::default()
    };
    let (staged, end) = map_with(
        history_frames(&spec),
        MapLimits {
            max_pins: 3,
            ..MapLimits::default()
        },
    );
    let idx_of = |i: u8| {
        staged
            .iter()
            .find_map(|row| match row {
                Staged::Chat { idx, conv, .. } if *conv == ConvKey::Direct(user(0x21 + i)) => {
                    Some(*idx)
                }
                _ => None,
            })
            .unwrap()
    };
    let mut gone: Vec<u32> = end.unpinned.clone();
    gone.sort_unstable();
    let mut want = vec![idx_of(0), idx_of(2)];
    want.sort_unstable();
    assert_eq!(gone, want, "the pins 50 and 40 go");
    assert_eq!(count(&end, reason::PINS_OVER_CAP), 2);
    // At the default bound nothing goes.
    let (_, end) = map_spec(&spec);
    assert!(end.unpinned.is_empty());
    assert_eq!(count(&end, reason::PINS_OVER_CAP), 0);
}
