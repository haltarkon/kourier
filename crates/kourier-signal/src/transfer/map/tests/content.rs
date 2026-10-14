//! The content kinds of step S3b, one test per row of the design's tables 2.7, 2.9 and 2.10
//! (`notes/research/history-import-mapping.md`) and the caps of its section 4 that belong to
//! them. Synthetic data; nothing of a row is printed.

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{TextRange, Utf16Map};

use super::*;
use crate::archive::writer::{
    CallSpec, GroupCallSpec, IndividualCallSpec, ItemSpec, NoticeSpec, QuoteSpec, RangeKindSpec,
    RangeSpec, ReactionSpec, SimpleSpec, Who,
};

const T: u64 = 1_700_000_000_000;
const HOUR: u64 = 60 * 60 * 1000;

fn range(start: u32, length: u32, kind: RangeKindSpec) -> RangeSpec {
    RangeSpec {
        start,
        length,
        kind,
    }
}

fn reaction(by: Who, emoji: &str, sort_order: u64) -> ReactionSpec {
    ReactionSpec {
        by,
        emoji: emoji.to_owned(),
        sent_at: T + 1_000 + sort_order,
        sort_order,
    }
}

/// An item of `kind` from the chat's contact (or from us).
fn of_kind(kind: ItemSpec, sent_at: u64, outgoing: bool) -> TextSpec {
    TextSpec {
        kind,
        outgoing,
        statuses: if outgoing {
            vec![SendStatusSpec::Delivered]
        } else {
            Vec::new()
        },
        ..incoming("", sent_at, true)
    }
}

fn notice(spec: NoticeSpec, sent_at: u64) -> TextSpec {
    of_kind(ItemSpec::Notice(spec), sent_at, false)
}

fn individual(
    state: IndividualCallSpec,
    outgoing: bool,
    video: bool,
    call_id: Option<u64>,
    read: bool,
) -> CallSpec {
    CallSpec::Individual {
        call_id,
        video,
        outgoing,
        state,
        read,
        started_at: T - 5,
    }
}

fn call(spec: CallSpec, sent_at: u64) -> TextSpec {
    of_kind(ItemSpec::Call(spec), sent_at, false)
}

fn unsupported(msg: &IncomingMessage) -> &str {
    match &msg.body {
        Body::Unsupported { kind_hint, .. } => kind_hint,
        _ => panic!("a placeholder"),
    }
}

fn line(msg: &IncomingMessage) -> &CallLine {
    match &msg.body {
        Body::System(SystemEvent::Call(line)) => line,
        _ => panic!("a call line"),
    }
}

/// The number of the frame a staged row was made from.
fn frame_of(staged: &[Staged], row: &IncomingMessage) -> u64 {
    staged
        .iter()
        .find_map(|s| match s {
            Staged::Item { n, msg, .. } if std::ptr::eq(&**msg, row) => {
                Some(*n / super::ROWS_PER_FRAME)
            }
            _ => None,
        })
        .expect("a staged row")
}

fn system(msg: &IncomingMessage) -> &SystemEvent {
    match &msg.body {
        Body::System(event) => event,
        _ => panic!("a notice"),
    }
}

fn text_of(msg: &IncomingMessage) -> &Text {
    match &msg.body {
        Body::Text(text) => text,
        _ => panic!("a text"),
    }
}

/// Each staged message passes the contract's check, in a batch of its own chat.
fn assert_contract(rows: &[&IncomingMessage]) {
    for (i, msg) in rows.iter().enumerate() {
        let (envelope, _waiter) = Envelope::durable(vec![BackendEvent::HistoryBatch {
            conv: msg.conv.clone(),
            messages: vec![(*msg).clone()],
            has_more: false,
            progress: None,
        }]);
        assert!(envelope.validate().is_ok(), "row {i} breaks the contract");
    }
}

/// The live mapper's own conversion: the expected UTF-8 range of a UTF-16 one.
fn utf8(body: &str, start: u32, length: u32) -> (u32, u32) {
    Utf16Map::new(body).range_to_utf8(start, length).unwrap()
}

#[test]
fn ranges_move_from_utf16_units_to_utf8_bytes() {
    // The emoji is two UTF-16 units and four bytes, the "é" one unit and two bytes.
    let body = "\u{1f600} bold \u{e9} ital mono";
    let (staged, end) = map_spec(&one_chat(vec![TextSpec {
        ranges: vec![
            range(0, 2, RangeKindSpec::Spoiler),
            range(3, 4, RangeKindSpec::Bold),
            range(10, 4, RangeKindSpec::Italic),
            range(15, 4, RangeKindSpec::Monospace),
            range(8, 1, RangeKindSpec::Strikethrough),
            // Inside the surrogate pair, past the end, a style of no kind, a mention of no
            // ACI: dropped and counted.
            range(1, 2, RangeKindSpec::Bold),
            range(15, 10, RangeKindSpec::Bold),
            range(3, 4, RangeKindSpec::NoStyle),
            range(0, 1, RangeKindSpec::BadMention),
        ],
        ..incoming(body, T, true)
    }]));
    let rows = messages(&staged);
    let text = text_of(rows[0]);
    assert_eq!(text.body, body);
    let at = |start, length, kind| {
        let (start, len) = utf8(body, start, length);
        TextRange { start, len, kind }
    };
    assert_eq!(
        text.ranges,
        [
            at(0, 2, RangeKind::Spoiler),
            at(3, 4, RangeKind::Bold),
            at(10, 4, RangeKind::Italic),
            at(15, 4, RangeKind::Mono),
            at(8, 1, RangeKind::Strike),
        ]
    );
    // In bytes: the emoji 0..4, "bold" 5..9, "é" 10..12, "ital" 13..17, "mono" 18..22.
    assert_eq!(
        text.ranges
            .iter()
            .map(|r| &body[r.start as usize..(r.start + r.len) as usize])
            .collect::<Vec<_>>(),
        ["\u{1f600}", "bold", "ital", "mono", "\u{e9}"]
    );
    assert_eq!(count(&end, reason::RANGE_DROPPED), 4);
    assert!(!rows[0].mentions_me);
}

#[test]
fn a_mention_of_us_sets_mentions_me() {
    let mention = |aci| range(3, 1, RangeKindSpec::Mention(aci));
    let (staged, end) = map_spec(&one_chat(vec![
        TextSpec {
            ranges: vec![mention(own_aci())],
            ..incoming("hi \u{fffc}!", T, false)
        },
        TextSpec {
            ranges: vec![mention(aci(0x21))],
            ..incoming("hi \u{fffc}!", T + 1, false)
        },
        // A mention range that does not cover its placeholder names nobody.
        TextSpec {
            ranges: vec![range(0, 2, RangeKindSpec::Mention(own_aci()))],
            ..incoming("hi \u{fffc}!", T + 2, false)
        },
    ]));
    let rows = messages(&staged);
    assert!(rows[0].mentions_me);
    assert_eq!(
        text_of(rows[0]).ranges,
        [TextRange {
            start: 3,
            len: 3,
            kind: RangeKind::Mention(own_user())
        }]
    );
    assert!(!rows[1].mentions_me, "a mention of somebody else");
    assert_eq!(
        text_of(rows[1]).ranges[0].kind,
        RangeKind::Mention(user(0x21))
    );
    assert!(!rows[2].mentions_me);
    assert!(text_of(rows[2]).ranges.is_empty());
    assert_eq!(count(&end, reason::RANGE_DROPPED), 1);
}

/// A text cut to its cap keeps the formatting of what is left: a range that runs past the
/// cut is shortened to it, and one with nothing left before the cut is dropped and counted
/// (the quote's text is cut the same way).
#[test]
fn a_range_that_crosses_the_cut_is_shortened_to_it() {
    // "\u{e9}" is two bytes and one UTF-16 unit: 25 of them are 50 bytes, and a cap of 40
    // keeps 20 of them.
    let body = "\u{e9}".repeat(25);
    let kept = "\u{e9}".repeat(20);
    let limits = MapLimits {
        max_text_bytes: 40,
        max_quote_bytes: 40,
        ..MapLimits::default()
    };
    let (staged, end) = map_with(
        history_frames(&one_chat(vec![TextSpec {
            ranges: vec![
                // Inside the cut: as it stands.
                range(0, 3, RangeKindSpec::Strikethrough),
                // Crosses it (15..25 against 20): shortened to 15..20.
                range(15, 10, RangeKindSpec::Bold),
                // Ends exactly at it: as it stands.
                range(18, 2, RangeKindSpec::Italic),
                // Starts at it, starts after it: nothing left.
                range(20, 5, RangeKindSpec::Monospace),
                range(22, 1, RangeKindSpec::Spoiler),
                // Longer than any text: shortened, not dropped.
                range(19, u32::MAX, RangeKindSpec::Bold),
            ],
            quote: Some(QuoteSpec {
                target: Some(T),
                author: Some(Who::Peer),
                text: body.clone(),
                ranges: vec![
                    range(10, 15, RangeKindSpec::Bold),
                    range(21, 2, RangeKindSpec::Italic),
                ],
                ..QuoteSpec::default()
            }),
            ..incoming(&body, T + 1, true)
        }])),
        limits,
    );
    let rows = messages(&staged);
    let text = text_of(rows[0]);
    assert_eq!(text.body, kept);
    let at = |start, length, kind| {
        let (start, len) = utf8(&kept, start, length);
        TextRange { start, len, kind }
    };
    assert_eq!(
        text.ranges,
        [
            at(0, 3, RangeKind::Strike),
            at(15, 5, RangeKind::Bold),
            at(18, 2, RangeKind::Italic),
            at(19, 1, RangeKind::Bold),
        ]
    );
    let quoted = rows[0]
        .reply_to
        .as_ref()
        .and_then(|reply| reply.snapshot.as_ref())
        .and_then(|snapshot| snapshot.text.as_ref())
        .expect("the quote's text");
    assert_eq!(quoted.body, kept);
    // The quote's range from 10 to 25 is cut to 10..20; the one from 21 has nothing left.
    assert_eq!(quoted.ranges, [at(10, 10, RangeKind::Bold)]);
    assert_eq!(count(&end, reason::TEXT_CUT), 1);
    assert_eq!(count(&end, reason::QUOTE_TEXT_CUT), 1);
    assert_eq!(count(&end, reason::RANGE_DROPPED), 3);
    assert_contract(&rows);
}

#[test]
fn a_quote_keeps_its_text_when_the_target_is_not_in_the_archive() {
    let quoted = |target, author| QuoteSpec {
        target,
        author,
        text: "the quoted words".to_owned(),
        ranges: vec![range(4, 6, RangeKindSpec::Bold)],
        attachment: Some(("image/jpeg".to_owned(), "a.jpg".to_owned())),
    };
    let mut spec = one_chat(vec![
        TextSpec {
            quote: Some(quoted(None, Some(Who::Peer))),
            ..incoming("not found", T, true)
        },
        TextSpec {
            quote: Some(quoted(Some(T - 10), Some(Who::Own))),
            ..incoming("ours", T + 1, true)
        },
        TextSpec {
            quote: Some(quoted(Some(T - 20), Some(Who::Unknown))),
            ..incoming("by nobody known", T + 2, true)
        },
        TextSpec {
            quote: Some(quoted(Some(T - 30), Some(Who::Contact(1)))),
            ..outgoing(
                "by somebody of another chat",
                T + 3,
                &[SendStatusSpec::Sent],
            )
        },
        // The quoted attachment's type is longer than any type is kept (the live path's
        // bound, `receive::media::MAX_MIME`): the quote names the file without a type.
        TextSpec {
            quote: Some(QuoteSpec {
                attachment: Some((format!("image/{}", "x".repeat(250)), "b.jpg".to_owned())),
                ..quoted(Some(T - 40), Some(Who::Peer))
            }),
            ..incoming("a quote of a file of a long type", T + 4, true)
        },
    ]);
    spec.contacts.push(person(0x22));
    let (staged, end) = map_spec(&spec);
    let rows = messages(&staged);
    let conv = ConvKey::Direct(user(0x21));
    let reply = rows[0].reply_to.as_ref().expect("the quote");
    assert_eq!(reply.target.conv.as_ref(), Some(&conv));
    assert_eq!(&*reply.target.local, "0", "names nothing");
    assert_eq!(reply.target.author, TargetAuthor::User(user(0x21)));
    assert_eq!(reply.author, Some(user(0x21)));
    let snapshot = reply.snapshot.as_ref().expect("the quote's own copy");
    let text = snapshot.text.as_ref().expect("its text");
    assert_eq!(text.body, "the quoted words");
    assert_eq!(
        text.ranges,
        [TextRange {
            start: 4,
            len: 6,
            kind: RangeKind::Bold
        }]
    );
    assert_eq!(
        snapshot.attachment,
        Some(kourier_model::AttachmentKind::Image)
    );
    assert_eq!(snapshot.file_name.as_deref(), Some("a.jpg"));
    assert_eq!(snapshot.mime.as_deref(), Some("image/jpeg"));
    assert_eq!(snapshot.thumb, None);

    let ours = rows[1].reply_to.as_ref().unwrap();
    assert_eq!(
        (&ours.target.author, &*ours.target.local, &ours.author),
        (&TargetAuthor::Own, (T - 10).to_string().as_str(), &None)
    );
    // An author the chat does not know is left to the core, which finds the target by its
    // `local` inside the chat; the row names nobody the stage does not hold.
    for row in &rows[2..4] {
        let reply = row.reply_to.as_ref().unwrap();
        assert_eq!(
            (&reply.target.author, &reply.author),
            (&TargetAuthor::Unknown, &None)
        );
    }
    let long_type = rows[4]
        .reply_to
        .as_ref()
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert_eq!(long_type.mime, None);
    assert_eq!(long_type.file_name.as_deref(), Some("b.jpg"));
    assert_eq!(
        long_type.attachment,
        Some(kourier_model::AttachmentKind::File),
        "a file of no known type"
    );
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    assert_contract(&rows);
}

#[test]
fn reactions_come_in_their_order_one_per_author() {
    let mut spec = one_chat(vec![TextSpec {
        reactions: vec![
            reaction(Who::Peer, "\u{1f44d}", 5),
            reaction(Who::Own, "\u{2764}\u{fe0f}", 3),
            // The contact's later reaction replaces the earlier one.
            reaction(Who::Peer, "\u{1f600}", 9),
            // Not one emoji; nobody known; somebody of another chat.
            reaction(Who::Own, "hello", 11),
            reaction(Who::Unknown, "\u{1f44d}", 12),
            reaction(Who::Contact(1), "\u{1f44d}", 13),
        ],
        ..incoming("liked", T, true)
    }]);
    spec.contacts.push(person(0x22));
    let (staged, end) = map_spec(&spec);
    let rows = messages(&staged);
    assert_eq!(
        rows[0].reactions,
        [
            HistoryReaction {
                by: None,
                emoji: "\u{2764}\u{fe0f}".to_owned(),
                at: i64::try_from(T + 1_003).unwrap()
            },
            HistoryReaction {
                by: Some(user(0x21)),
                emoji: "\u{1f600}".to_owned(),
                at: i64::try_from(T + 1_009).unwrap()
            },
        ]
    );
    assert_eq!(count(&end, reason::REACTION_NOT_ONE_EMOJI), 1);
    assert_eq!(count(&end, reason::REACTION_UNKNOWN_AUTHOR), 2);
}

#[test]
fn an_edited_item_is_keyed_by_its_first_version() {
    let (staged, end) = map_spec(&one_chat(vec![
        TextSpec {
            revisions: vec![T, T + 100, T + 200],
            ..incoming("the newest version", T + 300, true)
        },
        TextSpec {
            revisions: vec![T + 1_000],
            ..outgoing("mine, edited once", T + 1_500, &[SendStatusSpec::Read])
        },
    ]));
    let rows = messages(&staged);
    assert_eq!(&*rows[0].local, T.to_string(), "the original's id");
    assert_eq!(rows[0].sent_at, i64::try_from(T).unwrap());
    assert_eq!(text_of(rows[0]).body, "the newest version");
    let edits: Vec<String> = rows[0].edits.iter().map(|e| e.local.to_string()).collect();
    assert_eq!(
        edits,
        [T + 100, T + 200, T + 300].map(|at| at.to_string()),
        "oldest first, the newest last"
    );
    assert_eq!(rows[0].edits[2].at, i64::try_from(T + 300).unwrap());
    assert_eq!(&*rows[1].local, (T + 1_000).to_string());
    assert_eq!(rows[1].edits.len(), 1);
    assert_eq!(&*rows[1].edits[0].local, (T + 1_500).to_string());
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    assert_contract(&rows);
}

/// The contract wants `edits` oldest first with the newest version last, whatever order the
/// archive lists its revisions in: sorted by time, a repeated id or time kept once, the
/// original's id never among them, at most 100 and the newest of them.
#[test]
fn the_edits_come_sorted_once_each_and_the_newest_last() {
    let at = |ms: u64| i64::try_from(ms).unwrap();
    let ids = |row: &IncomingMessage| -> Vec<String> {
        row.edits.iter().map(|e| e.local.to_string()).collect()
    };
    let ahead = |days: u64| NOW as u64 + days * 24 * HOUR;
    let (staged, end) = map_spec(&one_chat(vec![
        // Misordered and repeated, the original again among the later ones.
        TextSpec {
            revisions: vec![T, T + 300, T + 100, T + 100, T, T + 200],
            ..incoming("misordered", T + 300, true)
        },
        // The item's own time is not the last one listed.
        TextSpec {
            revisions: vec![T + 1_000, T + 1_400, T + 1_200],
            ..incoming("the item is the newest", T + 1_500, true)
        },
        // Two versions so far ahead that both stand at the same moment: one entry, the
        // newer id.
        TextSpec {
            revisions: vec![T + 2_000, ahead(3), ahead(2)],
            ..incoming("two in the far future", T + 2_500, true)
        },
        // A version at the time of the original is no later version.
        TextSpec {
            revisions: vec![T + 3_000, T + 3_000],
            ..incoming("one version only", T + 3_000, true)
        },
        // More than the cap, listed from the newest to the oldest.
        TextSpec {
            revisions: std::iter::once(T + 4_000)
                .chain((0..=100u64).rev().map(|i| T + 40_000 + i))
                .collect(),
            ..incoming("many versions", T + 90_000, true)
        },
    ]));
    let rows = messages(&staged);
    assert_eq!(
        ids(rows[0]),
        [T + 100, T + 200, T + 300].map(|t| t.to_string())
    );
    assert_eq!(
        rows[0].edits[2].at,
        at(T + 300),
        "edited at the newest version"
    );
    assert_eq!(&*rows[0].local, T.to_string(), "the original's id");
    assert_eq!(
        ids(rows[1]),
        [T + 1_200, T + 1_400, T + 1_500].map(|t| t.to_string())
    );
    assert_eq!(rows[1].edits[2].at, at(T + 1_500));
    // Both are more than a day ahead of the import: the newer id stands alone, at the
    // import's time, as every time of the mapper that is that far ahead.
    assert_eq!(ids(rows[2])[..1], [(T + 2_500).to_string()]);
    assert_eq!(
        rows[2].edits.iter().map(|e| e.at).collect::<Vec<_>>(),
        [at(T + 2_500), NOW],
    );
    assert_eq!(ids(rows[2])[1], ahead(3).to_string(), "the newest version");
    assert!(rows[3].edits.is_empty());
    // 102 later versions of one message: the newest 100, the newest last.
    assert_eq!(rows[4].edits.len(), 100);
    assert_eq!(&*rows[4].edits[0].local, (T + 40_002).to_string());
    assert_eq!(&*rows[4].edits[99].local, (T + 90_000).to_string());
    assert!(
        rows[4].edits.windows(2).all(|w| w[0].at < w[1].at),
        "oldest first"
    );
    assert_eq!(count(&end, reason::EDITS_CUT), 1);
    assert_contract(&rows);
}

#[test]
fn a_view_once_item_keeps_neither_pointer_nor_raw_payload() {
    let (staged, end) = map_spec(&one_chat(vec![TextSpec {
        reactions: vec![reaction(Who::Own, "\u{1f525}", 1)],
        ..of_kind(ItemSpec::ViewOnce, T, false)
    }]));
    let rows = messages(&staged);
    assert_eq!(unsupported(rows[0]), "view-once");
    assert!(rows[0].view_once);
    assert_eq!(
        rows[0].raw, None,
        "its pointer and key would outlive the one view"
    );
    assert!(!matches!(
        rows[0].body,
        Body::Media { .. } | Body::Sticker(_)
    ));
    assert_eq!(rows[0].reactions.len(), 1);
    assert!(end.counts.is_empty(), "{:?}", end.counts);
}

#[test]
fn each_unsupported_kind_is_its_placeholder() {
    let kinds = [
        (
            ItemSpec::ContactCard {
                name: "Card".to_owned(),
            },
            "contact",
        ),
        (ItemSpec::Payment, "payment"),
        (ItemSpec::GiftBadge, "gift-badge"),
        (
            ItemSpec::Poll {
                question: "Lunch?".to_owned(),
            },
            "poll",
        ),
        (ItemSpec::StoryReply, "story-reply"),
        (ItemSpec::Sticker { data: false }, "sticker"),
    ];
    let mut texts: Vec<TextSpec> = kinds
        .iter()
        .enumerate()
        .map(|(i, (kind, _))| TextSpec {
            body: "a reply".to_owned(),
            reactions: vec![reaction(Who::Peer, "\u{1f44d}", 1)],
            ..of_kind(kind.clone(), T + i as u64, i % 2 == 1)
        })
        .collect();
    texts.push(of_kind(ItemSpec::Sticker { data: true }, T + 100, false));
    // Update items that are messages on the wire.
    texts.push(notice(
        NoticeSpec::Simple(SimpleSpec::UnsupportedProtocolMessage),
        T + 200,
    ));
    texts.push(notice(
        NoticeSpec::Simple(SimpleSpec::PaymentsActivated),
        T + 201,
    ));
    texts.push(notice(
        NoticeSpec::Simple(SimpleSpec::PaymentActivationRequest),
        T + 202,
    ));
    texts.push(notice(NoticeSpec::PollTerminate { target: T }, T + 203));
    texts.push(notice(NoticeSpec::PinMessage { target: T }, T + 204));
    let (staged, end) = map_spec(&one_chat(texts));
    let rows = messages(&staged);
    assert_eq!(rows.len(), kinds.len() + 6);
    for (i, (_, hint)) in kinds.iter().enumerate() {
        let row = rows[i];
        assert_eq!(unsupported(row), *hint);
        assert!(!row.view_once);
        assert_eq!(
            &*row.local,
            (T + i as u64).to_string(),
            "the message's real id"
        );
        // The raw item: the backup's own `ChatItem`, as the frame had it.
        let raw = row.raw.as_ref().expect("the raw item");
        assert_eq!(raw.v, RAW_CHAT_ITEM);
        let item = backup::ChatItem::decode(&raw.bytes[..]).expect("a chat item");
        assert_eq!((item.chat_id, item.date_sent), (1, T + i as u64));
        // Reactions on any of these kinds are imported as on a text (a payment and a gift
        // badge carry none in the archive).
        let reactions = usize::from(!matches!(*hint, "payment" | "gift-badge"));
        assert_eq!(row.reactions.len(), reactions, "{hint}");
    }
    // A sticker with its image: its pointer has no upload time and its message is far older
    // than twice the queue time before the import, so it is not available on this device.
    let Body::Sticker(image) = &rows[kinds.len()].body else {
        panic!("a sticker");
    };
    assert_eq!(image.kind, kourier_model::AttachmentKind::Image);
    assert_eq!(image.mime, "image/webp");
    assert_eq!((image.dims, image.size), (Some((512, 512)), 2_048));
    assert_eq!(
        (&image.state, &image.remote),
        (&AttachmentState::Unavailable, &None)
    );
    assert_eq!(image.net_ids, [format!("plaintext:{}", "04".repeat(32))]);
    assert_eq!(rows[kinds.len()].raw, None);
    // The update placeholders: under the message's real id, read, from their author; only
    // the poll's end and the pin keep their raw item (a simple update holds nothing more).
    let updates = &rows[kinds.len() + 1..];
    let hints: Vec<&str> = updates.iter().map(|row| unsupported(row)).collect();
    assert_eq!(
        hints,
        ["newer-protocol", "payment", "payment", "poll-ended", "pin"]
    );
    for (i, row) in updates.iter().enumerate() {
        assert_eq!(&*row.local, (T + 200 + i as u64).to_string());
        assert!(row.read);
        assert_eq!(
            row.origin,
            Origin::Remote {
                author: user(0x21),
                device: None
            }
        );
        assert_eq!(row.raw.is_some(), i >= 3, "{}", hints[i]);
    }
    // The writer's sticker image is long older than the age window: counted, nothing else.
    assert_eq!(
        end.counts,
        BTreeMap::from([(reason::ATTACHMENT_TOO_OLD, 1)]),
        "{:?}",
        end.counts
    );
    assert_contract(&rows);
}

/// A sticker's content type is kept as the live path keeps it (`receive::media::sticker`):
/// none or an empty one is `image/webp`, one of at most 255 bytes stands, a longer one is
/// `application/octet-stream`. The test asks the live helper about the same pointer.
#[test]
fn a_stickers_content_type_is_kept_as_the_live_path_keeps_it() {
    let long = format!("image/{}", "x".repeat(250));
    let edge = format!("image/{}", "x".repeat(249));
    assert_eq!((long.len(), edge.len()), (256, 255));
    let cases = [
        (Some(long), "application/octet-stream"),
        (Some(edge.clone()), edge.as_str()),
        (Some(String::new()), "image/webp"),
        (None, "image/webp"),
        (Some("image/png".to_owned()), "image/png"),
    ];
    let (staged, end) = map_spec(&one_chat(
        cases
            .iter()
            .enumerate()
            .map(|(i, (content_type, _))| {
                of_kind(
                    ItemSpec::StickerTyped {
                        content_type: content_type.clone(),
                    },
                    T + i as u64,
                    false,
                )
            })
            .collect(),
    ));
    let rows = messages(&staged);
    assert_eq!(rows.len(), cases.len());
    for (row, (content_type, want)) in rows.iter().zip(&cases) {
        let Body::Sticker(image) = &row.body else {
            panic!("a sticker");
        };
        let live = crate::receive::media::sticker(&crate::proto::data_message::Sticker {
            data: Some(crate::proto::AttachmentPointer {
                content_type: content_type.clone(),
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("the live sticker");
        assert_eq!(image.mime, *want);
        assert_eq!(image.mime, live.mime, "the live path's type");
    }
    // Every sticker image of the writer is older than the age window.
    assert!(
        end.counts
            .keys()
            .all(|why| *why == reason::ATTACHMENT_TOO_OLD),
        "{:?}",
        end.counts
    );
    assert_contract(&rows);
}

#[test]
fn each_one_to_one_notice_is_its_row_or_is_counted() {
    let mapped = [
        (
            SimpleSpec::IdentityUpdate,
            SystemEvent::IdentityKeyChanged { user: user(0x21) },
        ),
        (
            SimpleSpec::ChangeNumber,
            SystemEvent::NumberChanged { user: user(0x21) },
        ),
        (
            SimpleSpec::EndSession,
            SystemEvent::SessionRefreshed { user: user(0x21) },
        ),
        (
            SimpleSpec::ChatSessionRefresh,
            SystemEvent::SessionRefreshed { user: user(0x21) },
        ),
        (
            SimpleSpec::MessageRequestAccepted,
            SystemEvent::MessageRequestAccepted,
        ),
    ];
    let not_in_contract = [
        NoticeSpec::Simple(SimpleSpec::JoinedSignal),
        NoticeSpec::Simple(SimpleSpec::IdentityVerified),
        NoticeSpec::Simple(SimpleSpec::IdentityDefault),
        NoticeSpec::Simple(SimpleSpec::ReleaseChannelDonationRequest),
        NoticeSpec::Simple(SimpleSpec::BadDecrypt),
        NoticeSpec::Simple(SimpleSpec::ReportedSpam),
        NoticeSpec::Simple(SimpleSpec::Blocked),
        NoticeSpec::Simple(SimpleSpec::Unblocked),
        NoticeSpec::Simple(SimpleSpec::Number(0)),
        NoticeSpec::Simple(SimpleSpec::Number(99)),
        NoticeSpec::ProfileChange,
        NoticeSpec::ThreadMerge,
        NoticeSpec::SessionSwitchover,
        NoticeSpec::LearnedProfile,
    ];
    let mut texts: Vec<TextSpec> = mapped
        .iter()
        .enumerate()
        .map(|(i, (kind, _))| TextSpec {
            // A notice whose timer the phone started keeps it; the others have none.
            expires_in_ms: Some(HOUR),
            expire_start: (i == 0).then_some(T),
            ..notice(NoticeSpec::Simple(*kind), T + i as u64)
        })
        .collect();
    texts.extend(not_in_contract.iter().map(|n| notice(n.clone(), T + 50)));
    texts.push(notice(NoticeSpec::Unset, T + 60));
    // A safety-number notice by us, and one of an author nobody knows.
    texts.push(TextSpec {
        outgoing: true,
        ..notice(NoticeSpec::Simple(SimpleSpec::IdentityUpdate), T + 70)
    });
    texts.push(TextSpec {
        unknown_author: true,
        ..notice(NoticeSpec::Simple(SimpleSpec::IdentityUpdate), T + 71)
    });
    let (staged, end) = map_spec(&one_chat(texts));
    let rows = messages(&staged);
    assert_eq!(rows.len(), mapped.len());
    for (i, (row, (_, event))) in rows.iter().zip(&mapped).enumerate() {
        assert_eq!(system(row), event);
        let frame = staged
            .iter()
            .find_map(|s| match s {
                Staged::Item { n, msg, .. } if std::ptr::eq(&**msg, *row) => {
                    Some(*n / super::ROWS_PER_FRAME)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(&*row.local, format!("h:{frame}:0"), "an id of its own");
        assert!(row.read && row.silent);
        assert_eq!(
            row.origin,
            Origin::Remote {
                author: user(0x21),
                device: None
            }
        );
        let expected = (i == 0).then_some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_millis(HOUR),
            started_at: Some(i64::try_from(T).unwrap()),
        });
        assert_eq!(row.expiry, expected, "{event:?}");
    }
    assert_eq!(
        count(&end, reason::NOTICE_NOT_IN_CONTRACT),
        not_in_contract.len() as u64 + 1,
        "and the safety number of ours"
    );
    assert_eq!(count(&end, reason::UNKNOWN_ITEM), 1);
    assert_eq!(count(&end, reason::UNKNOWN_AUTHOR), 1);
    assert_contract(&rows);

    // In Note to Self a notice about the chat's contact has nobody to name.
    let (staged, end) = map_spec(&HistorySpec {
        chats: vec![ChatSpec {
            with: ChatWith::NoteToSelf,
            texts: vec![TextSpec {
                outgoing: true,
                ..notice(NoticeSpec::Simple(SimpleSpec::EndSession), T)
            }],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    });
    assert!(messages(&staged).is_empty());
    assert_eq!(count(&end, reason::NOTICE_NOT_IN_CONTRACT), 1);
}

#[test]
fn a_timer_change_notice_gets_no_expiry() {
    let (staged, end) = map_spec(&one_chat(vec![
        TextSpec {
            // The item states the chat's new timer, and even a start: no timer for the row.
            expires_in_ms: Some(HOUR),
            expire_start: Some(T),
            ..notice(
                NoticeSpec::TimerChange {
                    expires_in_ms: HOUR,
                },
                T,
            )
        },
        TextSpec {
            outgoing: true,
            expires_in_ms: Some(HOUR),
            ..notice(NoticeSpec::TimerChange { expires_in_ms: 0 }, T + 1)
        },
    ]));
    let rows = messages(&staged);
    assert_eq!(
        system(rows[0]),
        &SystemEvent::TimerChanged {
            by: Some(user(0x21)),
            timer: Some(Duration::from_millis(HOUR))
        }
    );
    assert_eq!(
        system(rows[1]),
        &SystemEvent::TimerChanged {
            by: None,
            timer: None
        }
    );
    assert_eq!(rows[1].origin, Origin::OwnOtherDevice { device: None });
    for row in &rows {
        assert_eq!(row.expiry, None, "Android never starts it");
        assert!(row.local.starts_with("h:"));
    }
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    assert_contract(&rows);
}

#[test]
fn each_call_state_is_its_line() {
    use IndividualCallSpec as S;
    let states = [
        // (state, outgoing, video, id) → (direction, state, media, by the peer)
        (S::Accepted, false, false, Some(11), CallState::Answered),
        (S::Unknown, false, true, Some(12), CallState::Answered),
        (S::NotAccepted, false, false, Some(13), CallState::Declined),
        (S::Missed, false, true, Some(14), CallState::Missed),
        (
            S::MissedNotificationProfile,
            false,
            false,
            None,
            CallState::Missed,
        ),
        (S::Accepted, true, true, Some(16), CallState::Answered),
        (S::NotAccepted, true, false, Some(17), CallState::Unanswered),
        (S::Missed, true, false, Some(18), CallState::Unanswered),
    ];
    let mut texts: Vec<TextSpec> = states
        .iter()
        .enumerate()
        .map(|(i, (state, outgoing, video, id, _))| {
            call(
                individual(*state, *outgoing, *video, *id, false),
                T + i as u64,
            )
        })
        .collect();
    // A state, a direction and a type this version does not know; a group call in a 1:1
    // chat.
    texts.push(call(
        individual(S::Number(9), false, false, Some(30), true),
        T + 50,
    ));
    let mut started_at_zero = call(individual(S::Missed, false, false, Some(33), true), T + 53);
    if let ItemSpec::Call(CallSpec::Individual { started_at, .. }) = &mut started_at_zero.kind {
        *started_at = 0;
    }
    texts.push(started_at_zero);
    texts.push(call(
        CallSpec::Group {
            call_id: Some(40),
            state: GroupCallSpec::Generic,
            ringer: None,
            started_by: Some(Who::Peer),
            read: true,
            started_at: T,
        },
        T + 54,
    ));
    let mut frames = history_frames(&one_chat(texts));
    frames.push(bad_direction_frame(&frames, 3));
    frames.push(bad_direction_frame(&frames, 2));
    let (staged, end) = map(frames);
    let rows = messages(&staged);
    assert_eq!(rows.len(), states.len() + 1);
    let conv = ConvKey::Direct(user(0x21));
    for (i, (_, outgoing, video, id, state)) in states.iter().enumerate() {
        let row = rows[i];
        let line = line(row);
        assert_eq!(row.conv, conv);
        assert_eq!(
            row.origin,
            Origin::OwnOtherDevice { device: None },
            "what our phone says"
        );
        let frame = frame_of(&staged, row);
        let call_id = id.map_or_else(|| format!("h{frame}"), |id| id.to_string());
        assert_eq!(&*row.local, CallLine::local(false, &call_id));
        let direction = if *outgoing {
            CallDirection::Outgoing
        } else {
            CallDirection::Incoming
        };
        assert_eq!(
            (line.direction, line.group, line.state),
            (direction, false, *state)
        );
        let media = if *video {
            CallMedia::Video
        } else {
            CallMedia::Voice
        };
        assert_eq!(line.media, Some(media));
        assert_eq!(line.by, (!outgoing).then(|| user(0x21)));
        // The time the call started: the original, never the import's.
        let at = i64::try_from(T - 5).unwrap();
        assert_eq!((row.sent_at, row.received_at), (at, at));
        assert!(row.silent);
        // Only a missed call can be unread, and this one was not seen on the phone.
        assert_eq!(row.read, *state != CallState::Missed, "{state:?}");
        assert_eq!(row.expiry, None);
    }
    // A call without its start time is dated by the item.
    assert_eq!(rows[states.len()].sent_at, i64::try_from(T + 53).unwrap());
    assert!(rows[states.len()].read, "seen on the phone");
    assert_eq!(count(&end, reason::UNKNOWN_CALL), 3);
    assert_eq!(count(&end, reason::CALL_NOT_IN_ITS_CHAT), 1);
    assert_contract(&rows);

    // In Note to Self a call has no peer.
    let (staged, end) = map_spec(&HistorySpec {
        chats: vec![ChatSpec {
            with: ChatWith::NoteToSelf,
            texts: vec![call(individual(S::Accepted, true, false, Some(1), true), T)],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    });
    assert!(messages(&staged).is_empty());
    assert_eq!(count(&end, reason::CALL_NOT_IN_ITS_CHAT), 1);
}

/// A copy of the call item `n` frames from the end with an unknown direction (`which` 3) or
/// type (2), as a newer phone could write it.
fn bad_direction_frame(frames: &[Frame], which: u8) -> Frame {
    use backup::chat_update_message::Update;
    let Some(Frame::ChatItem(item)) = frames
        .iter()
        .rev()
        .find(|f| matches!(f, Frame::ChatItem(i) if i.date_sent == T + 50))
    else {
        panic!("the call");
    };
    let mut item = item.clone();
    item.date_sent = T + 60 + u64::from(which);
    if let Some(backup::chat_item::Item::UpdateMessage(backup::ChatUpdateMessage {
        update: Some(Update::IndividualCall(call)),
    })) = &mut item.item
    {
        call.state = backup::individual_call::State::Accepted as i32;
        if which == 3 {
            call.direction = 7;
        } else {
            call.r#type = 7;
        }
    }
    Frame::ChatItem(item)
}

/// A `callId` of 0 is no id (Desktop's importer takes it so, and a writer that fills the
/// field with its default writes it): every such call is a line of its own, under the
/// frame's id, and none joins the line of another.
#[test]
fn a_call_id_of_zero_is_no_id() {
    use IndividualCallSpec as S;
    let (staged, end) = map_spec(&one_chat(vec![
        call(individual(S::Accepted, false, false, Some(0), true), T),
        call(individual(S::Missed, false, true, Some(0), false), T + 10),
        call(individual(S::Accepted, true, false, None, true), T + 20),
        call(individual(S::Accepted, true, false, Some(5), true), T + 30),
    ]));
    let rows = messages(&staged);
    assert_eq!(rows.len(), 4);
    let locals: Vec<String> = rows.iter().map(|row| row.local.to_string()).collect();
    for (row, id) in rows.iter().zip(&locals).take(3) {
        assert_eq!(
            *id,
            CallLine::local(false, &format!("h{}", frame_of(&staged, row))),
            "the frame's own id"
        );
    }
    assert_eq!(locals[3], CallLine::local(false, "5"));
    let mut distinct = locals.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 4, "four calls are four lines: {locals:?}");
    // The two calls of id 0 differ in what they say, as two lines do.
    assert_eq!(line(rows[0]).state, CallState::Answered);
    assert_eq!(line(rows[1]).state, CallState::Missed);
    assert_contract(&rows);

    assert!(end.counts.is_empty(), "{:?}", end.counts);

    // A group call of id 0 is no different.
    let (mut mapper, mut out) = group_mapper();
    let (item, group_call) = group_call_item(group_spec(GroupCallSpec::Generic, 0, T, true));
    let msg = mapper
        .group_call(group(), &item, &group_call, &mut out)
        .expect("a line");
    assert_ne!(&*msg.local, CallLine::local(true, "0"));
    assert!(
        msg.local.starts_with(&CallLine::local(true, "h")),
        "the frame's own id"
    );
}

#[test]
fn an_outgoing_call_without_a_timer_start_gets_the_imports_time() {
    use IndividualCallSpec as S;
    let timed = |spec: TextSpec, start: Option<u64>| TextSpec {
        expires_in_ms: Some(HOUR),
        expire_start: start,
        ..spec
    };
    let (staged, end) = map_spec(&one_chat(vec![
        timed(
            call(individual(S::Accepted, true, false, Some(1), true), T),
            None,
        ),
        timed(
            call(individual(S::Accepted, true, false, Some(2), true), T + 1),
            Some(T + 9),
        ),
        timed(
            call(individual(S::Missed, false, false, Some(3), false), T + 2),
            None,
        ),
        timed(
            call(individual(S::Missed, false, false, Some(4), true), T + 3),
            Some(T + 8),
        ),
    ]));
    let rows = messages(&staged);
    let expiry = |kind, start: Option<u64>| {
        Some(Expiry {
            kind,
            timer: Duration::from_millis(HOUR),
            started_at: start.map(|at| i64::try_from(at).unwrap()),
        })
    };
    // Given without a start, the core would count an outgoing line from the call's time and
    // erase an old call at once.
    assert_eq!(
        rows[0].expiry,
        expiry(DisappearKind::AfterSend, Some(u64::try_from(NOW).unwrap()))
    );
    assert_eq!(
        rows[1].expiry,
        expiry(DisappearKind::AfterSend, Some(T + 9))
    );
    // An incoming line: its after-read timer starts when it is read here.
    assert_eq!(rows[2].expiry, expiry(DisappearKind::AfterRead, None));
    assert_eq!(
        rows[3].expiry,
        expiry(DisappearKind::AfterRead, Some(T + 8))
    );
    assert_eq!(count(&end, reason::TIMER_NOT_STARTED), 1);
    assert_contract(&rows);
}

/// The conversation of the tests of `Mapper::group_call`: S4 will stage the group; here
/// there is none, and the call is mapped into the key it is given.
fn group() -> ConvKey {
    ConvKey::Group("gv2:AAAA".into())
}

/// A mapper that has read the frames of two people, and what it staged: the set-up of the
/// tests of `Mapper::group_call`.
fn group_mapper() -> (Mapper, Vec<Staged>) {
    let mut mapper = mapper_with(MapLimits::default());
    let mut out = Vec::new();
    for frame in history_frames(&HistorySpec {
        contacts: vec![person(0x21), person(0x22)],
        ..HistorySpec::default()
    }) {
        mapper.frame(frame, &mut out);
    }
    (mapper, out)
}

/// The item and the group call the writer makes of `spec` (a [`CallSpec::Group`]); the
/// item stands in the chat of `one_chat`, only its content matters to the mapper.
fn group_call_item(spec: CallSpec) -> (backup::ChatItem, backup::GroupCall) {
    let frames = history_frames(&one_chat(vec![call(spec, T + 5)]));
    let Some(Frame::ChatItem(item)) = frames.into_iter().last() else {
        panic!("the item");
    };
    let Some(backup::chat_item::Item::UpdateMessage(backup::ChatUpdateMessage {
        update: Some(backup::chat_update_message::Update::GroupCall(call)),
    })) = item.item.clone()
    else {
        panic!("a group call");
    };
    (*item, call)
}

/// A group call of `state` that the contact of `one_chat` rang, started at `started_at`.
fn group_spec(state: GroupCallSpec, call_id: u64, started_at: u64, read: bool) -> CallSpec {
    CallSpec::Group {
        call_id: Some(call_id),
        state,
        ringer: Some(Who::Peer),
        started_by: Some(Who::Peer),
        read,
        started_at,
    }
}

/// The group call's line (step S4 shows it in its group), on a group conversation the test
/// makes: every state, and the ring of the moment of the export reported unread.
#[test]
fn a_ringing_group_call_is_reported_unread() {
    use GroupCallSpec as G;
    let (mut mapper, mut out) = group_mapper();
    let group = group();
    let group_call = |state, read, ringer, started_by| CallSpec::Group {
        call_id: Some(77),
        state,
        ringer,
        started_by,
        read,
        started_at: T,
    };
    // The ringer of `one_chat` is its contact, recipient 2: Person 0x21.
    let cases = [
        (G::Ringing, true, Some(Who::Peer), None),
        (G::Generic, false, None, Some(Who::Peer)),
        (G::Unknown, true, None, Some(Who::Own)),
        (G::Joined, false, None, None),
        (G::Accepted, false, Some(Who::Peer), None),
        (G::Declined, false, Some(Who::Peer), None),
        (G::Missed, false, Some(Who::Peer), None),
        (G::MissedNotificationProfile, true, Some(Who::Unknown), None),
        (G::OutgoingRing, false, None, None),
    ];
    let at = i64::try_from(T).unwrap();
    let ringing = |deadline| CallState::Ringing { deadline };
    let expected = [
        // No time left: it is a missed call the moment the core takes it.
        (ringing(at), Some(user(0x21)), false),
        (CallState::Started, Some(user(0x21)), false),
        (CallState::Started, None, true),
        (CallState::Answered, None, true),
        (CallState::Answered, None, true),
        (CallState::Declined, None, true),
        (CallState::Missed, Some(user(0x21)), false),
        (CallState::Missed, None, true),
        (CallState::Answered, None, true),
    ];
    let mut rows = Vec::new();
    for ((state, read, ringer, started_by), (want, by, want_read)) in
        cases.into_iter().zip(expected)
    {
        let (item, call) = group_call_item(group_call(state, read, ringer, started_by));
        let msg = mapper
            .group_call(group.clone(), &item, &call, &mut out)
            .expect("a line");
        let line = line(&msg);
        assert_eq!(
            (line.direction, line.group, line.state, line.by.clone()),
            (CallDirection::Incoming, true, want, by),
            "{state:?}"
        );
        assert_eq!(msg.read, want_read, "{state:?}");
        assert_eq!(&*msg.local, CallLine::local(true, "77"));
        assert_eq!(msg.origin, Origin::OwnOtherDevice { device: None });
        assert_eq!(msg.sent_at, at);
        rows.push(msg);
    }
    // A state this version does not know.
    let (item, call) = group_call_item(group_call(G::Number(42), true, None, None));
    assert!(
        mapper
            .group_call(group.clone(), &item, &call, &mut out)
            .is_none()
    );
    assert_eq!(count(&mapper.finish(), reason::UNKNOWN_CALL), 1);
    // The person who rang was staged before a row names them.
    assert!(
        out.iter()
            .any(|s| matches!(s, Staged::Contact { contact, .. } if contact.user == user(0x21)))
    );
    assert_contract(&rows.iter().collect::<Vec<_>>());
}

/// A call that rings in the archive must never ring in Kourier: the phone rang, or is
/// still ringing, and this computer is no phone. The core keeps a ring by the time it has
/// LEFT (`deadline - sent_at`), counted from the time the line was received here
/// ([`CallLine::on_local_clock`]), so a ring is safe only when it has none left, however
/// near the import it started and whichever clock is ahead.
#[test]
fn a_ringing_group_call_has_no_time_left() {
    let starts = [
        ("long before the import", NOW - 3_600_000),
        ("just before the import", NOW - 10_000),
        ("59 s before the import", NOW - 59_000),
        ("at the import", NOW),
        // The phone's clock is ahead of this computer's (less than the day the mapper takes).
        ("a minute ahead", NOW + 60_000),
        ("an hour ahead", NOW + 3_600_000),
    ];
    let (mut mapper, mut out) = group_mapper();
    for (when, start) in starts {
        let start = u64::try_from(start).unwrap();
        let (item, group_call) =
            group_call_item(group_spec(GroupCallSpec::Ringing, 78, start, true));
        let msg = mapper
            .group_call(group(), &item, &group_call, &mut out)
            .expect("a line");
        let ring = line(&msg);
        let CallState::Ringing { deadline } = ring.state else {
            panic!("{when}: the archive's ring stays a ring until the core settles it");
        };
        assert!(
            deadline <= NOW,
            "{when}: the deadline {deadline} is the future"
        );
        // The core's own steps: the ring on this computer's clock, then at once.
        let kept = ring.clone().on_local_clock(msg.sent_at, msg.received_at);
        assert_eq!(kept.state, CallState::Missed, "{when}: it rings here");
        assert_eq!(kept.deadline(), None, "{when}");
        assert!(kept.counts_unread(), "{when}: an unread missed call");
        assert_eq!(ring.settled(NOW).state, CallState::Missed, "{when}");
        assert!(!msg.read && msg.silent, "{when}");
    }
}

#[test]
fn every_cap_cuts_and_counts() {
    let limits = MapLimits::default();
    let long = "x".repeat(limits.max_text_bytes + 10);
    let long_quote = "q".repeat(limits.max_quote_bytes + 10);
    let many_ranges: Vec<RangeSpec> = (0..=limits.max_ranges as u32)
        .map(|i| range(i % 8, 1, RangeKindSpec::Bold))
        .collect();
    let many_versions: Vec<u64> = (0..=limits.max_edits as u64)
        .map(|i| T + 20_000 + i)
        .collect();
    let big_card = "n".repeat(limits.max_raw_bytes + 1);
    let (staged, end) = map_spec(&one_chat(vec![
        incoming(&long, T, true),
        TextSpec {
            quote: Some(QuoteSpec {
                target: Some(T),
                author: Some(Who::Peer),
                text: long_quote,
                ..QuoteSpec::default()
            }),
            ..incoming("with a long quote", T + 1, true)
        },
        TextSpec {
            ranges: many_ranges,
            ..incoming("eight letters here", T + 2, true)
        },
        TextSpec {
            revisions: many_versions,
            ..incoming("edited many times", T + 30_000, true)
        },
        TextSpec {
            body: String::new(),
            ..of_kind(ItemSpec::ContactCard { name: big_card }, T + 3, false)
        },
    ]));
    let rows = messages(&staged);
    assert_eq!(rows.len(), 5, "a cut never drops the row");
    assert_eq!(text_of(rows[0]).body.len(), limits.max_text_bytes);
    let quote = rows[1]
        .reply_to
        .as_ref()
        .unwrap()
        .snapshot
        .as_ref()
        .unwrap();
    assert_eq!(
        quote.text.as_ref().unwrap().body.len(),
        limits.max_quote_bytes
    );
    assert_eq!(text_of(rows[2]).ranges.len(), limits.max_ranges);
    // 101 later versions: the newest 100, the newest last.
    assert_eq!(rows[3].edits.len(), limits.max_edits);
    assert_eq!(&*rows[3].edits[99].local, (T + 30_000).to_string());
    assert_eq!(&*rows[3].edits[0].local, (T + 20_002).to_string());
    assert_eq!(unsupported(rows[4]), "contact");
    assert_eq!(
        rows[4].raw, None,
        "the placeholder stays, without its raw item"
    );
    for why in [
        reason::TEXT_CUT,
        reason::QUOTE_TEXT_CUT,
        reason::RANGES_CUT,
        reason::EDITS_CUT,
        reason::RAW_TOO_LARGE,
    ] {
        assert_eq!(count(&end, why), 1, "{why}");
    }

    // Reactions: a 1:1 chat has two authors, so the cap is shown with a lower one.
    let (staged, end) = map_with(
        history_frames(&one_chat(vec![TextSpec {
            reactions: vec![
                reaction(Who::Peer, "\u{1f44d}", 7),
                reaction(Who::Own, "\u{1f600}", 3),
            ],
            ..incoming("liked twice", T, true)
        }])),
        MapLimits {
            max_reactions: 1,
            ..MapLimits::default()
        },
    );
    let rows = messages(&staged);
    assert_eq!(rows[0].reactions.len(), 1);
    assert_eq!(
        rows[0].reactions[0].by,
        Some(user(0x21)),
        "the newest stays"
    );
    assert_eq!(count(&end, reason::REACTIONS_CUT), 1);
}

/// One item of every kind this step maps, and of the kinds it counts, for the chat of
/// `one_chat` (its contact is the peer): the rich part of the archive of
/// `every_mapped_row_passes_the_contract_check`.
pub(super) fn every_kind() -> Vec<TextSpec> {
    use IndividualCallSpec as S;
    let t = T + 500_000;
    let mut items = vec![
        TextSpec {
            ranges: vec![
                range(0, 1, RangeKindSpec::Mention(own_aci())),
                range(2, 4, RangeKindSpec::Bold),
                range(2, 4, RangeKindSpec::Spoiler),
            ],
            quote: Some(QuoteSpec {
                target: Some(t),
                author: Some(Who::Own),
                text: "quoted".to_owned(),
                ranges: vec![range(0, 6, RangeKindSpec::Italic)],
                attachment: Some(("video/mp4".to_owned(), "clip.mp4".to_owned())),
            }),
            reactions: vec![
                reaction(Who::Own, "\u{1f44d}", 1),
                reaction(Who::Peer, "\u{1f600}", 2),
            ],
            revisions: vec![t + 1, t + 2],
            ..incoming("\u{fffc} rich \u{1f600}", t + 3, false)
        },
        TextSpec {
            quote: Some(QuoteSpec {
                target: None,
                author: Some(Who::Unknown),
                ..QuoteSpec::default()
            }),
            ..outgoing("a quote of nothing found", t + 4, &[SendStatusSpec::Read])
        },
        of_kind(ItemSpec::Sticker { data: true }, t + 5, false),
        of_kind(ItemSpec::Sticker { data: false }, t + 6, true),
        of_kind(ItemSpec::ViewOnce, t + 7, false),
        of_kind(
            ItemSpec::ContactCard {
                name: "Card".to_owned(),
            },
            t + 8,
            false,
        ),
        of_kind(ItemSpec::Payment, t + 9, true),
        of_kind(ItemSpec::GiftBadge, t + 10, false),
        of_kind(
            ItemSpec::Poll {
                question: "Lunch?".to_owned(),
            },
            t + 11,
            true,
        ),
        TextSpec {
            body: "a reply to a story".to_owned(),
            ..of_kind(ItemSpec::StoryReply, t + 12, false)
        },
        notice(
            NoticeSpec::TimerChange {
                expires_in_ms: HOUR,
            },
            t + 13,
        ),
        notice(NoticeSpec::PollTerminate { target: t + 11 }, t + 14),
        notice(NoticeSpec::PinMessage { target: t + 3 }, t + 15),
        notice(NoticeSpec::ProfileChange, t + 16),
        notice(NoticeSpec::Unset, t + 17),
        call(
            CallSpec::Group {
                call_id: Some(9),
                state: GroupCallSpec::Ringing,
                ringer: Some(Who::Peer),
                started_by: None,
                read: true,
                started_at: t,
            },
            t + 18,
        ),
        TextSpec {
            expires_in_ms: Some(HOUR),
            ..call(individual(S::Accepted, true, true, Some(1), true), t + 19)
        },
    ];
    for (i, simple) in [
        SimpleSpec::IdentityUpdate,
        SimpleSpec::ChangeNumber,
        SimpleSpec::EndSession,
        SimpleSpec::ChatSessionRefresh,
        SimpleSpec::MessageRequestAccepted,
        SimpleSpec::UnsupportedProtocolMessage,
        SimpleSpec::PaymentsActivated,
        SimpleSpec::PaymentActivationRequest,
        SimpleSpec::JoinedSignal,
        SimpleSpec::BadDecrypt,
    ]
    .into_iter()
    .enumerate()
    {
        items.push(notice(NoticeSpec::Simple(simple), t + 100 + i as u64));
    }
    for (i, state) in [
        S::Unknown,
        S::Accepted,
        S::NotAccepted,
        S::Missed,
        S::MissedNotificationProfile,
    ]
    .into_iter()
    .enumerate()
    {
        for outgoing in [false, true] {
            let at = t + 200 + 2 * i as u64 + u64::from(outgoing);
            items.push(call(
                individual(state, outgoing, i % 2 == 0, (i > 0).then_some(at), i == 3),
                at,
            ));
        }
    }
    items
}
