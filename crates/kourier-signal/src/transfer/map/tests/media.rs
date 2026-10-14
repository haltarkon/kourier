//! Attachment pointers (step S5): what each shape of an archive's `FilePointer` becomes, the
//! age rule, the kinds from the archive's flag ENUM, long texts and stickers, the caps.
//! Synthetic data; nothing of a row is printed.

use super::*;
use crate::archive::writer::{AttachmentSpec, IntegritySpec, ItemSpec, LocatorSpec, PointerSpec};
use crate::attachment::{ARCHIVE_POINTER_BLOB_V, Integrity, Pointer};

const DAY: u64 = 24 * 60 * 60 * 1000;
/// Twice the 30-day floor of the queue time: the window when no remote config is stored.
const WINDOW: u64 = 60 * DAY;
/// A message of two days before the import.
const YOUNG: u64 = NOW as u64 - 2 * DAY;

fn locator(upload: Option<u64>) -> LocatorSpec {
    LocatorSpec {
        key: vec![6; 64],
        integrity: IntegritySpec::PlaintextHash(vec![4; 32]),
        size: 1_000,
        cdn_key: Some("cdn-key_1".to_owned()),
        cdn_number: Some(3),
        upload_timestamp: upload,
        media_tier_cdn_number: None,
    }
}

fn pointer(content_type: &str, locator: Option<LocatorSpec>) -> PointerSpec {
    PointerSpec {
        content_type: Some(content_type.to_owned()),
        locator,
        ..PointerSpec::default()
    }
}

fn attachment(pointer: PointerSpec) -> AttachmentSpec {
    AttachmentSpec {
        pointer: Some(pointer),
        ..AttachmentSpec::default()
    }
}

/// An incoming message sent at `sent_at` with these attachments and no caption.
fn with(attachments: Vec<AttachmentSpec>, sent_at: u64) -> TextSpec {
    TextSpec {
        attachments,
        ..incoming("", sent_at, true)
    }
}

/// The attachments of the one message of the chat.
fn attachments_of(staged: &[Staged]) -> Vec<Attachment> {
    match &messages(staged)[0].body {
        Body::Media { attachments, .. } => attachments.clone(),
        Body::Sticker(image) => vec![(**image).clone()],
        other => panic!("attachments: {other:?}"),
    }
}

fn map_rules(spec: &HistorySpec, rules: MediaRules) -> (Vec<Staged>, MapEnd) {
    let mut mapper = mapper_with(MapLimits::default()).with_media(rules);
    let mut out = Vec::new();
    for frame in history_frames(spec) {
        mapper.frame(frame, &mut out);
    }
    (out, mapper.finish())
}

/// One attachment of `locator` on a young message: its state, whether it has a `remote`,
/// and the counts.
fn one(locator: Option<LocatorSpec>) -> (AttachmentState, bool, MapEnd) {
    let (staged, end) = map_spec(&one_chat(vec![with(
        vec![attachment(pointer("image/png", locator))],
        YOUNG,
    )]));
    let [a] = &attachments_of(&staged)[..] else {
        panic!("one attachment");
    };
    (a.state, a.remote.is_some(), end)
}

/// Every shape of a locator is its state. A valid place on the transit tier (by plaintext
/// hash or by digest) is a download on click with the stored pointer (`v` 3) that reads
/// back as the place it names; everything else is "not available on this device", with no
/// pointer (and so no key), never a request.
#[test]
fn each_locator_shape_is_its_state() {
    let (state, remote, end) = one(Some(locator(Some(YOUNG))));
    assert_eq!((state, remote), (AttachmentState::Remote, true));
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    let (staged, _) = map_spec(&one_chat(vec![with(
        vec![attachment(pointer("image/png", Some(locator(Some(YOUNG)))))],
        YOUNG,
    )]));
    let remote = attachments_of(&staged)[0].remote.clone().expect("remote");
    assert_eq!(remote.v, ARCHIVE_POINTER_BLOB_V);
    let back = Pointer::from_blob(&remote).expect("reads back");
    assert_eq!(
        (back.cdn, back.cdn_key.as_str(), back.size),
        (3, "cdn-key_1", 1_000)
    );
    assert_eq!(back.integrity, Integrity::PlaintextHash([4; 32]));

    let digest = LocatorSpec {
        integrity: IntegritySpec::EncryptedDigest(vec![5; 32]),
        cdn_number: Some(0),
        ..locator(Some(YOUNG))
    };
    let (state, remote, _) = one(Some(digest));
    assert_eq!(
        (state, remote),
        (AttachmentState::Remote, true),
        "by digest, CDN 0"
    );
    let cdn2 = LocatorSpec {
        cdn_number: Some(2),
        ..locator(Some(YOUNG))
    };
    assert_eq!(one(Some(cdn2)).0, AttachmentState::Remote, "CDN 2");
    // An empty file is legal (`backup.proto`), as on the live path and in Desktop.
    let empty = LocatorSpec {
        size: 0,
        ..locator(Some(YOUNG))
    };
    let (state, remote, end) = one(Some(empty));
    assert_eq!((state, remote), (AttachmentState::Remote, true), "size 0");
    assert!(end.counts.is_empty(), "{:?}", end.counts);

    let l = || locator(Some(YOUNG));
    let not_available: Vec<(&str, Option<LocatorSpec>)> = vec![
        ("no locator", None),
        ("an empty locator", Some(LocatorSpec::default())),
        (
            "a hash without a place",
            Some(LocatorSpec {
                cdn_key: None,
                cdn_number: None,
                ..l()
            }),
        ),
        (
            "a place without hash or digest",
            Some(LocatorSpec {
                integrity: IntegritySpec::Absent,
                ..l()
            }),
        ),
        (
            "a key without its number",
            Some(LocatorSpec {
                cdn_number: None,
                ..l()
            }),
        ),
        (
            "a number without its key",
            Some(LocatorSpec {
                cdn_key: None,
                ..l()
            }),
        ),
        (
            "a key of 63 bytes",
            Some(LocatorSpec {
                key: vec![6; 63],
                ..l()
            }),
        ),
        (
            "a hash of 33 bytes",
            Some(LocatorSpec {
                integrity: IntegritySpec::PlaintextHash(vec![4; 33]),
                ..l()
            }),
        ),
        (
            "a digest of 31 bytes",
            Some(LocatorSpec {
                integrity: IntegritySpec::EncryptedDigest(vec![4; 31]),
                ..l()
            }),
        ),
        (
            "an unsafe CDN key",
            Some(LocatorSpec {
                cdn_key: Some("a/../b".into()),
                ..l()
            }),
        ),
        (
            "a CDN key of 257 bytes",
            Some(LocatorSpec {
                cdn_key: Some("k".repeat(257)),
                ..l()
            }),
        ),
        (
            "an unknown CDN",
            Some(LocatorSpec {
                cdn_number: Some(1),
                ..l()
            }),
        ),
        (
            "the media tier only",
            Some(LocatorSpec {
                cdn_key: None,
                cdn_number: None,
                media_tier_cdn_number: Some(3),
                ..l()
            }),
        ),
    ];
    for (what, shape) in not_available {
        let (state, remote, end) = one(shape);
        assert_eq!(
            (state, remote),
            (AttachmentState::Unavailable, false),
            "{what}"
        );
        assert_eq!(count(&end, reason::ATTACHMENT_NOT_AVAILABLE), 1, "{what}");
    }
    // Old: not available, counted apart.
    let (state, remote, end) = one(Some(locator(Some(NOW as u64 - WINDOW - DAY))));
    assert_eq!((state, remote), (AttachmentState::Unavailable, false));
    assert_eq!(count(&end, reason::ATTACHMENT_TOO_OLD), 1);
    // Larger than the server lets this account receive.
    let big = LocatorSpec {
        size: u32::MAX,
        ..locator(Some(YOUNG))
    };
    let (state, remote, end) = one(Some(big));
    assert_eq!((state, remote), (AttachmentState::Unavailable, false));
    assert_eq!(count(&end, reason::ATTACHMENT_TOO_LARGE), 1);
    // An attachment without any pointer is nothing to show: dropped and counted, and the
    // message keeps the rest.
    let (staged, end) = map_spec(&one_chat(vec![with(
        vec![
            AttachmentSpec::default(),
            attachment(pointer("image/png", Some(locator(Some(YOUNG))))),
        ],
        YOUNG,
    )]));
    assert_eq!(attachments_of(&staged).len(), 1);
    assert_eq!(count(&end, reason::ATTACHMENT_NOT_AVAILABLE), 1);
}

/// The age counts from the pointer's upload time, else from the message's ORIGINAL sent time
/// (Desktop's `uploadTimestamp || sentAt`: an upload time of 0 is none). A young upload on an
/// old message is a download, an old upload on a young message is not, and an upload time
/// ahead of the import (a sender's claim) is young.
#[test]
fn the_age_counts_from_the_upload_time_and_else_from_the_sent_time() {
    let old = NOW as u64 - WINDOW - DAY;
    let state = |upload: Option<u64>, sent: u64| {
        let (staged, _) = map_spec(&one_chat(vec![with(
            vec![attachment(pointer("image/png", Some(locator(upload))))],
            sent,
        )]));
        attachments_of(&staged)[0].state
    };
    assert_eq!(
        state(Some(YOUNG), old),
        AttachmentState::Remote,
        "young upload, old message"
    );
    assert_eq!(
        state(Some(old), YOUNG),
        AttachmentState::Unavailable,
        "old upload, young message"
    );
    assert_eq!(state(None, YOUNG), AttachmentState::Remote);
    assert_eq!(state(None, old), AttachmentState::Unavailable);
    assert_eq!(
        state(Some(0), YOUNG),
        AttachmentState::Remote,
        "0 is no upload time"
    );
    assert_eq!(state(Some(0), old), AttachmentState::Unavailable);
    assert_eq!(
        state(Some(NOW as u64 + 365 * DAY), old),
        AttachmentState::Remote
    );

    // An edited message counts from its ORIGINAL time: the item's `dateSent` is its last
    // edit's.
    let edited = TextSpec {
        revisions: vec![old],
        ..with(
            vec![attachment(pointer("image/png", Some(locator(None))))],
            YOUNG,
        )
    };
    let (staged, _) = map_spec(&one_chat(vec![edited]));
    assert_eq!(
        attachments_of(&staged)[0].state,
        AttachmentState::Unavailable,
        "the original was sent long ago"
    );
}

/// The bound is Desktop's `isOlderThan`: at exactly twice the queue time before the reference
/// a place is old; a millisecond later it is a download.
#[test]
fn exactly_twice_the_queue_time_is_old_and_a_millisecond_less_is_a_download() {
    let rules = MediaRules::new(NOW, 0, None, u64::MAX);
    let edge = NOW - i64::try_from(WINDOW).unwrap();
    assert!(rules.is_old(edge));
    assert!(!rules.is_old(edge + 1));
    let state = |upload: u64| {
        let (staged, _) = map_spec(&one_chat(vec![with(
            vec![attachment(pointer(
                "image/png",
                Some(locator(Some(upload))),
            ))],
            YOUNG,
        )]));
        attachments_of(&staged)[0].state
    };
    assert_eq!(state(NOW as u64 - WINDOW), AttachmentState::Unavailable);
    assert_eq!(state(NOW as u64 - WINDOW + 1), AttachmentState::Remote);
}

/// The window is twice the server's queue time and never under sixty days (the 30-day floor,
/// also when no remote config is stored), measured from the PHONE's export time bounded to
/// within a day of the import.
#[test]
fn the_age_window_is_twice_the_servers_queue_time_and_never_under_sixty_days() {
    let days = |rules: MediaRules| rules.window_ms / i64::try_from(DAY).unwrap();
    assert_eq!(days(MediaRules::new(NOW, 0, None, 1)), 60);
    assert_eq!(
        days(MediaRules::new(
            NOW,
            0,
            Some(Duration::from_secs(7 * 86_400)),
            1
        )),
        60,
        "the floor"
    );
    assert_eq!(
        days(MediaRules::new(
            NOW,
            0,
            Some(Duration::from_secs(45 * 86_400)),
            1
        )),
        90
    );
    // The reference: the export time, within a day of the import; none is the import.
    let day = i64::try_from(DAY).unwrap();
    let reference = |backup: u64| MediaRules::new(NOW, backup, None, 1).reference_ms;
    assert_eq!(reference(0), NOW);
    assert_eq!(reference(NOW as u64 - 3_600_000), NOW - 3_600_000);
    assert_eq!(reference(NOW as u64 + 3_600_000), NOW + 3_600_000);
    assert_eq!(reference(NOW as u64 - 30 * DAY), NOW - day);
    assert_eq!(reference(NOW as u64 + 30 * DAY), NOW + day);

    // A 120-day queue time: a message 100 days old is a download, one 250 days old is not.
    let rules = MediaRules::new(
        NOW,
        NOW as u64,
        Some(Duration::from_secs(120 * 86_400)),
        u64::MAX,
    );
    let state = |sent: u64| {
        let (staged, _) = map_rules(
            &one_chat(vec![with(
                vec![attachment(pointer("image/png", Some(locator(None))))],
                sent,
            )]),
            rules,
        );
        attachments_of(&staged)[0].state
    };
    assert_eq!(state(NOW as u64 - 100 * DAY), AttachmentState::Remote);
    assert_eq!(state(NOW as u64 - 250 * DAY), AttachmentState::Unavailable);
    // A receive limit of the server's own.
    let small = MediaRules::new(NOW, 0, None, 999);
    let (staged, end) = map_rules(
        &one_chat(vec![with(
            vec![attachment(pointer("image/png", Some(locator(None))))],
            YOUNG,
        )]),
        small,
    );
    assert_eq!(
        attachments_of(&staged)[0].state,
        AttachmentState::Unavailable
    );
    assert_eq!(count(&end, reason::ATTACHMENT_TOO_LARGE), 1);
}

/// The kinds come from the archive's flag ENUM (GIF is 3 there, the wire's bit 8): a GIF is a
/// GIF and never a voice note, a voice note is a voice note (every flagged one, as live), a
/// borderless picture is a picture, an unknown flag is none. The fields are bounded as the
/// live path bounds them, the names are the client UUID and the hash or digest, and a
/// message of attachments without a caption is no empty message.
#[test]
fn a_message_of_attachments_keeps_kinds_fields_and_names_as_live() {
    let l = || Some(locator(Some(YOUNG)));
    let flagged = |content_type: &str, flag: i32| AttachmentSpec {
        pointer: Some(pointer(content_type, l())),
        flag,
        client_uuid: None,
    };
    let voice = AttachmentSpec {
        pointer: Some(PointerSpec {
            audio_waveform: Some(vec![7; 150]),
            audio_duration_seconds: Some(2.5),
            ..pointer("audio/aac", l())
        }),
        flag: 1,
        client_uuid: Some([0xab; 16]),
    };
    let file = AttachmentSpec {
        pointer: Some(PointerSpec {
            file_name: Some("report.pdf".into()),
            caption: Some("the caption".into()),
            blur_hash: Some("b".repeat(129)),
            width: Some(0),
            height: Some(10),
            locator: Some(LocatorSpec {
                integrity: IntegritySpec::EncryptedDigest(vec![0xcd; 32]),
                ..locator(Some(YOUNG))
            }),
            ..pointer("application/pdf", None)
        }),
        flag: 0,
        client_uuid: None,
    };
    let picture = AttachmentSpec {
        pointer: Some(PointerSpec {
            width: Some(640),
            height: Some(480),
            blur_hash: Some("LEHV6nWB2yk8".into()),
            file_name: Some("n".repeat(256)),
            ..pointer("image/jpeg", l())
        }),
        flag: 2,
        client_uuid: None,
    };
    let untyped = AttachmentSpec {
        pointer: Some(PointerSpec {
            content_type: Some("x".repeat(256)),
            ..pointer("", l())
        }),
        flag: 77,
        client_uuid: None,
    };
    let (staged, end) = map_spec(&one_chat(vec![with(
        vec![
            flagged("image/gif", 3),
            flagged("video/mp4", 3),
            voice,
            file,
            picture,
            untyped,
            flagged("audio/mpeg", 0),
        ],
        YOUNG,
    )]));
    assert!(end.counts.is_empty(), "{:?}", end.counts);
    let msg = messages(&staged)[0];
    let Body::Media {
        attachments,
        caption,
    } = &msg.body
    else {
        panic!("media");
    };
    assert_eq!(caption, &None, "no caption, and not an empty message");
    let kinds: Vec<AttachmentKind> = attachments.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        [
            AttachmentKind::Gif,
            AttachmentKind::Gif,
            AttachmentKind::Voice,
            AttachmentKind::File,
            AttachmentKind::Image,
            AttachmentKind::File,
            AttachmentKind::Audio,
        ]
    );
    let voice = &attachments[2];
    assert_eq!(voice.duration_ms, Some(2_500));
    assert_eq!(voice.waveform.as_ref().map(Vec::len), Some(100));
    assert_eq!(
        voice.net_ids,
        [
            format!("uuid:{}", "ab".repeat(16)),
            format!("plaintext:{}", "04".repeat(32))
        ]
    );
    let file = &attachments[3];
    assert_eq!(file.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(
        file.caption.as_ref().map(|c| c.body.as_str()),
        Some("the caption")
    );
    assert_eq!((file.blurhash.as_ref(), file.dims), (None, None));
    assert_eq!(file.net_ids, [format!("digest:{}", "cd".repeat(32))]);
    assert_eq!(file.size, 1_000);
    let picture = &attachments[4];
    assert_eq!(
        (picture.dims, picture.file_name.as_ref()),
        (Some((640, 480)), None)
    );
    assert_eq!(picture.blurhash.as_deref(), Some("LEHV6nWB2yk8"));
    assert_eq!(attachments[5].mime, "application/octet-stream");
    assert!(
        attachments
            .iter()
            .all(|a| !a.long_text && a.state == AttachmentState::Remote)
    );
    // The kind function alone: the archive's enum, never the wire's bits.
    assert_eq!(
        crate::receive::media::archive_kind("image/png", 3),
        AttachmentKind::Gif
    );
    assert_eq!(
        crate::receive::media::archive_kind("image/png", 1),
        AttachmentKind::Voice
    );
    assert_eq!(
        crate::receive::media::archive_kind("image/png", 2),
        AttachmentKind::Image
    );
    assert_eq!(
        crate::receive::media::archive_kind("image/png", 8),
        AttachmentKind::Image,
        "8 is no flag here"
    );
    assert_eq!(
        crate::receive::media::archive_kind("image/gif", 0),
        AttachmentKind::Gif
    );
}

/// A long text's pointer is the long-text attachment behind the inline body, which stays the
/// text; while young it has its pointer (the core fetches it by itself), when old it is not
/// available and the message keeps its first 2 KiB.
#[test]
fn a_long_text_pointer_is_the_long_text_attachment_behind_the_inline_body() {
    let inline = "the first two kilobytes".to_owned();
    let long = |upload: u64| TextSpec {
        long_text: Some(pointer("text/x-signal-plain", Some(locator(Some(upload))))),
        ..incoming(&inline, YOUNG, true)
    };
    let (staged, end) = map_spec(&one_chat(vec![
        long(YOUNG),
        long(NOW as u64 - WINDOW - DAY),
    ]));
    let rows = messages(&staged);
    for (row, young) in rows.iter().zip([true, false]) {
        let Body::Media {
            attachments,
            caption,
        } = &row.body
        else {
            panic!("media");
        };
        assert_eq!(
            caption.as_ref().map(|c| c.body.as_str()),
            Some(inline.as_str())
        );
        let [text] = &attachments[..] else {
            panic!("one attachment");
        };
        assert!(text.long_text);
        assert_eq!(text.mime, "text/x-signal-plain");
        assert_eq!(text.remote.is_some(), young);
        assert_eq!(
            text.state,
            if young {
                AttachmentState::Remote
            } else {
                AttachmentState::Unavailable
            }
        );
    }
    assert_eq!(count(&end, reason::ATTACHMENT_TOO_OLD), 1);
    // Beside attachments, it goes last.
    let both = TextSpec {
        attachments: vec![attachment(pointer("image/png", Some(locator(Some(YOUNG)))))],
        ..long(YOUNG)
    };
    let (staged, _) = map_spec(&one_chat(vec![both]));
    let longs: Vec<bool> = attachments_of(&staged)
        .iter()
        .map(|a| a.long_text)
        .collect();
    assert_eq!(longs, [false, true]);
}

/// A sticker's image follows the rules of every attachment.
#[test]
fn a_sticker_with_a_young_pointer_is_a_download_and_an_old_one_is_not() {
    let sticker = |upload: u64| TextSpec {
        kind: ItemSpec::StickerWith {
            pointer: pointer("image/webp", Some(locator(Some(upload)))),
        },
        ..incoming("", YOUNG, true)
    };
    let (staged, end) = map_spec(&one_chat(vec![
        sticker(YOUNG),
        sticker(NOW as u64 - WINDOW),
    ]));
    let rows = messages(&staged);
    let states: Vec<(AttachmentState, bool)> = rows
        .iter()
        .map(|row| match &row.body {
            Body::Sticker(image) => (image.state, image.remote.is_some()),
            other => panic!("a sticker: {other:?}"),
        })
        .collect();
    assert_eq!(
        states,
        [
            (AttachmentState::Remote, true),
            (AttachmentState::Unavailable, false)
        ]
    );
    assert_eq!(count(&end, reason::ATTACHMENT_TOO_OLD), 1);
    assert!(
        rows.iter().all(|row| row.raw.is_none()),
        "a sticker with an image keeps no raw item"
    );
}

/// An edited message takes the attachments of its newest version (the top item) once; the
/// earlier versions' copies in `revisions` add nothing.
#[test]
fn an_edited_message_keeps_the_pointers_of_its_newest_version_only() {
    let edited = TextSpec {
        revisions: vec![YOUNG - 1_000, YOUNG - 500],
        ..with(
            vec![
                attachment(pointer("image/png", Some(locator(None)))),
                attachment(pointer("image/jpeg", Some(locator(None)))),
            ],
            YOUNG,
        )
    };
    let (staged, _) = map_spec(&one_chat(vec![edited]));
    assert_eq!(messages(&staged).len(), 1);
    assert_eq!(attachments_of(&staged).len(), 2);
}

/// More attachments than the cap: the first ones stay, the cut is counted once.
#[test]
fn more_attachments_than_the_cap_are_cut_and_counted() {
    let many = with(
        (0..5)
            .map(|_| attachment(pointer("image/png", Some(locator(Some(YOUNG))))))
            .collect(),
        YOUNG,
    );
    let (staged, end) = map_with(
        history_frames(&one_chat(vec![many])),
        MapLimits {
            max_attachments: 3,
            ..MapLimits::default()
        },
    );
    assert_eq!(attachments_of(&staged).len(), 3);
    assert_eq!(count(&end, reason::ATTACHMENTS_CUT), 1);
    assert_eq!(MapLimits::default().max_attachments, 1_000);
}

/// Only a message's attachments, its long text and a sticker's image become attachments
/// with a pointer. A quote's thumbnail, a view-once message's pointer and a contact card's
/// avatar never give a `remote` or an attachment, even when their place is young and valid
/// (the model has no quote thumbnail from a backend; view-once stays a placeholder until
/// its own step).
#[test]
fn view_once_quote_thumbnails_and_contact_avatars_keep_no_pointer() {
    use backup::chat_item::Item;
    let young = pointer("image/jpeg", Some(locator(Some(YOUNG)))).to_proto();
    let spec = one_chat(vec![
        TextSpec {
            quote: Some(crate::archive::writer::QuoteSpec {
                target: Some(YOUNG - 10),
                author: Some(crate::archive::writer::Who::Peer),
                text: "the quoted".to_owned(),
                attachment: Some(("image/jpeg".to_owned(), "q.jpg".to_owned())),
                ..Default::default()
            }),
            ..incoming("a reply", YOUNG, true)
        },
        TextSpec {
            kind: ItemSpec::ViewOnce,
            ..incoming("", YOUNG + 1, true)
        },
        TextSpec {
            kind: ItemSpec::ContactCard {
                name: "A Person".to_owned(),
            },
            ..incoming("", YOUNG + 2, true)
        },
    ]);
    let mut frames = history_frames(&spec);
    for frame in &mut frames {
        let Frame::ChatItem(item) = frame else {
            continue;
        };
        match &mut item.item {
            Some(Item::StandardMessage(message)) => {
                let quote = message.quote.as_mut().expect("a quote");
                for quoted in &mut quote.attachments {
                    quoted.thumbnail = Some(backup::MessageAttachment {
                        pointer: Some(young.clone()),
                        ..Default::default()
                    });
                }
            }
            Some(Item::ViewOnceMessage(message)) => {
                message.attachment = Some(backup::MessageAttachment {
                    pointer: Some(young.clone()),
                    ..Default::default()
                });
            }
            Some(Item::ContactMessage(message)) => {
                message.contact.as_mut().expect("a card").avatar = Some(young.clone());
            }
            _ => {}
        }
    }
    let (staged, end) = map(frames);
    let rows = messages(&staged);
    assert_eq!(rows.len(), 3);
    assert!(
        matches!(&rows[0].body, Body::Text(_)),
        "the reply is a text"
    );
    let snapshot = rows[0]
        .reply_to
        .as_ref()
        .and_then(|reply| reply.snapshot.as_ref())
        .expect("its quote");
    assert_eq!(snapshot.thumb, None);
    for row in &rows[1..] {
        assert!(
            matches!(&row.body, Body::Unsupported { .. }),
            "a placeholder"
        );
    }
    assert_eq!(rows[1].raw, None, "view-once keeps nothing of its pointer");
    assert!(end.counts.is_empty(), "{:?}", end.counts);
}
