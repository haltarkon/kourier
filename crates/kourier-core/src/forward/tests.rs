use super::*;
use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{ContactCard, FileLimits, Identifier, IdentifierKind, Location, Poll, Support};

fn user(n: &str) -> UserId {
    UserId::parse(&format!("test:{n}")).unwrap()
}

fn feats(fmt: &[FormatKind], files: &[(AttachmentKind, u64, bool)]) -> ConversationFeatures {
    let mut f = ConversationFeatures::default();
    for k in fmt {
        f.formatting.insert(*k, Support::Full);
    }
    for (k, max, caption) in files {
        f.files.insert(
            *k,
            FileLimits {
                max_size: *max,
                caption: *caption,
            },
        );
    }
    f
}

fn names(u: &UserId) -> String {
    match u.as_str() {
        "test:olena" => "Олена".to_owned(),
        _ => "Bob".to_owned(),
    }
}

fn go(
    body: &Body,
    f: &ConversationFeatures,
    cards: bool,
    native: bool,
    look: ForwardLook,
    header: Option<&Text>,
) -> Converted {
    convert(
        body,
        &TargetFacts {
            features: Some(f),
            contact_cards: cards,
        },
        How {
            native,
            look,
            header,
            names: &names,
        },
    )
}

fn plain(body: &Body, f: &ConversationFeatures) -> Converted {
    go(body, f, false, false, ForwardLook::Plain, None)
}

fn range(start: u32, len: u32, kind: RangeKind) -> TextRange {
    TextRange { start, len, kind }
}

fn only_text(c: &Converted) -> &Text {
    match c.parts.as_slice() {
        [ForwardPart::Text(t)] => t,
        other => panic!("expected one text, got {other:?}"),
    }
}

fn att(id: i64, kind: AttachmentKind, size: u64) -> Attachment {
    Attachment {
        id: Some(AttachmentId(id)),
        kind,
        size,
        ..Default::default()
    }
}

#[test]
fn ranges_are_kept_only_where_the_target_takes_their_kind() {
    let t = Text {
        body: "bold mono".into(),
        ranges: vec![range(0, 4, RangeKind::Bold), range(5, 4, RangeKind::Mono)],
    };
    let c = plain(&Body::Text(t), &feats(&[FormatKind::Bold], &[]));
    let out = only_text(&c);
    assert_eq!(out.ranges, vec![range(0, 4, RangeKind::Bold)]);
    assert_eq!(c.changes, vec![ForwardChange::FormattingDropped(1)]);
}

#[test]
fn a_mention_becomes_at_name_and_later_ranges_shift() {
    // "hi " + placeholder + " bold" : the placeholder is 3 bytes.
    let body = format!("hi {} bold", MENTION_PLACEHOLDER);
    let bold_at = body.find("bold").unwrap() as u32;
    let t = Text {
        body,
        ranges: vec![
            range(3, 3, RangeKind::Mention(user("olena"))),
            range(bold_at, 4, RangeKind::Bold),
        ],
    };
    let f = feats(&[FormatKind::Bold, FormatKind::Mention], &[]);
    // Not native: the mention means nothing on another network, even where it is supported.
    let c = plain(&Body::Text(t.clone()), &f);
    let out = only_text(&c);
    assert_eq!(out.body, "hi @Олена bold");
    let at = out.body.find("bold").unwrap() as u32;
    assert_eq!(out.ranges, vec![range(at, 4, RangeKind::Bold)]);
    assert_eq!(c.changes, vec![ForwardChange::MentionsAsText(1)]);
    // Native keeps it where the target takes it, and ignores the look.
    let c = go(&Body::Text(t), &f, false, true, ForwardLook::Quoted, None);
    let out = only_text(&c);
    assert!(out.body.starts_with("hi \u{FFFC}"));
    assert_eq!(out.ranges.len(), 2);
    assert!(c.changes.is_empty());
}

#[test]
fn a_mention_of_everyone_is_worded_and_native_without_support_too() {
    let body = format!("{} x", MENTION_PLACEHOLDER);
    let t = Text {
        body,
        ranges: vec![range(0, 3, RangeKind::MentionAll)],
    };
    let c = go(
        &Body::Text(t),
        &feats(&[FormatKind::Mention], &[]),
        false,
        true,
        ForwardLook::Plain,
        None,
    );
    assert_eq!(only_text(&c).body, "@everyone x");
}

#[test]
fn a_link_the_target_cannot_carry_gets_its_url() {
    let url = "https://example.org/a";
    let t = Text {
        body: "see here now".into(),
        ranges: vec![
            range(4, 4, RangeKind::Link(url.into())),
            range(9, 3, RangeKind::Bold),
        ],
    };
    let f = feats(&[FormatKind::Bold], &[]);
    let c = plain(&Body::Text(t), &f);
    let out = only_text(&c);
    assert_eq!(out.body, format!("see here ({url}) now"));
    let bold = out.body.find("now").unwrap() as u32;
    assert_eq!(out.ranges, vec![range(bold, 3, RangeKind::Bold)]);
    assert_eq!(c.changes, vec![ForwardChange::LinksAsText(1)]);
    // The text is the URL: nothing is added.
    let t = Text {
        body: url.into(),
        ranges: vec![range(0, url.len() as u32, RangeKind::Link(url.into()))],
    };
    let c = plain(&Body::Text(t), &f);
    assert_eq!(only_text(&c).body, url);
    // A target that carries links keeps the range.
    let f = feats(&[FormatKind::Link], &[]);
    let t = Text {
        body: "see here".into(),
        ranges: vec![range(4, 4, RangeKind::Link(url.into()))],
    };
    let c = plain(&Body::Text(t), &f);
    assert_eq!(only_text(&c).ranges.len(), 1);
    assert!(c.changes.is_empty());
}

#[test]
fn the_header_is_the_first_line_and_italic_where_supported() {
    let h = Text::plain("Forwarded from Ann");
    let t = Body::text("hello");
    let f = feats(&[FormatKind::Italic], &[]);
    let c = go(&t, &f, false, false, ForwardLook::Header, Some(&h));
    let out = only_text(&c);
    assert_eq!(out.body, "Forwarded from Ann\nhello");
    assert_eq!(out.ranges, vec![range(0, 18, RangeKind::Italic)]);
    let c = go(
        &t,
        &feats(&[], &[]),
        false,
        false,
        ForwardLook::Header,
        Some(&h),
    );
    assert!(only_text(&c).ranges.is_empty());
    // Plain look and native: no header.
    let c = go(&t, &f, false, false, ForwardLook::Plain, Some(&h));
    assert_eq!(only_text(&c).body, "hello");
    let c = go(&t, &f, false, true, ForwardLook::Header, Some(&h));
    assert_eq!(only_text(&c).body, "hello");
}

#[test]
fn the_header_is_its_own_text_before_a_card() {
    let h = Text::plain("Forwarded from Ann");
    let card = ContactCard {
        name: Some("Zed".into()),
        identifiers: vec![Identifier {
            kind: IdentifierKind::Phone,
            value: "+4915".into(),
        }],
        ..Default::default()
    };
    let f = feats(&[], &[]);
    let c = go(
        &Body::Contact(card.clone()),
        &f,
        true,
        false,
        ForwardLook::Header,
        Some(&h),
    );
    assert_eq!(c.parts.len(), 2);
    assert!(matches!(&c.parts[0], ForwardPart::Text(t) if t.body == "Forwarded from Ann"));
    assert_eq!(c.parts[1], ForwardPart::Contact(card.clone()));
    assert!(c.changes.is_empty());
    // Without cards: one text.
    let c = go(
        &Body::Contact(card),
        &f,
        false,
        false,
        ForwardLook::Plain,
        None,
    );
    assert_eq!(only_text(&c).body, "Zed, +4915");
    assert_eq!(c.changes, vec![ForwardChange::ContactAsText(1)]);
}

#[test]
fn a_quoted_body_has_every_line_prefixed_and_ranges_shifted() {
    let t = Text {
        body: "one\ntwo\nthree".into(),
        ranges: vec![range(4, 3, RangeKind::Bold)],
    };
    let h = Text::plain("From Ann");
    let f = feats(&[FormatKind::Bold], &[]);
    let c = go(
        &Body::Text(t),
        &f,
        false,
        false,
        ForwardLook::Quoted,
        Some(&h),
    );
    let out = only_text(&c);
    assert_eq!(out.body, "From Ann\n> one\n> two\n> three");
    let at = out.body.find("two").unwrap() as u32;
    assert_eq!(out.ranges, vec![range(at, 3, RangeKind::Bold)]);
    assert!(out.body.starts_with("From Ann"));
}

#[test]
fn media_refused_kind_goes_as_file_oversize_is_left_out_caption_converted() {
    let f = feats(
        &[FormatKind::Bold],
        &[
            (AttachmentKind::Image, 100, true),
            (AttachmentKind::File, 1000, true),
        ],
    );
    let body = Body::Media {
        attachments: vec![
            att(1, AttachmentKind::Image, 10),
            att(2, AttachmentKind::Video, 10),
            att(3, AttachmentKind::Image, 500),
            att(4, AttachmentKind::File, 5000),
        ],
        caption: Some(Text {
            body: "cap".into(),
            ranges: vec![range(0, 3, RangeKind::Bold), range(0, 1, RangeKind::Mono)],
        }),
    };
    let c = plain(&body, &f);
    let [ForwardPart::Files { files, caption }] = c.parts.as_slice() else {
        panic!("{:?}", c.parts)
    };
    assert_eq!(
        files,
        &vec![
            ForwardFile {
                attachment: AttachmentId(1),
                as_file: false
            },
            ForwardFile {
                attachment: AttachmentId(2),
                as_file: true
            },
        ]
    );
    assert_eq!(caption.as_ref().unwrap().ranges.len(), 1);
    assert!(
        c.changes
            .contains(&ForwardChange::FileAsGeneric(vec![AttachmentId(2)]))
    );
    assert!(c.changes.contains(&ForwardChange::FileTooLarge(vec![
        AttachmentId(3),
        AttachmentId(4)
    ])));
    // Nothing takes files: all refused, no caption: no part.
    let body = Body::Media {
        attachments: vec![att(1, AttachmentKind::Image, 10)],
        caption: None,
    };
    let c = plain(&body, &feats(&[], &[]));
    assert!(c.parts.is_empty());
    assert_eq!(
        c.changes,
        vec![ForwardChange::FileRefused(vec![AttachmentId(1)])]
    );
}

#[test]
fn a_caption_the_kind_does_not_take_goes_first_as_text() {
    let f = feats(&[], &[(AttachmentKind::Image, 0, false)]);
    let body = Body::Media {
        attachments: vec![att(1, AttachmentKind::Image, 10)],
        caption: Some(Text::plain("cap")),
    };
    let c = plain(&body, &f);
    assert!(matches!(&c.parts[0], ForwardPart::Text(t) if t.body == "cap"));
    assert!(matches!(
        &c.parts[1],
        ForwardPart::Files { caption: None, .. }
    ));
}

#[test]
fn a_sticker_is_a_picture_and_an_animated_one_the_target_cannot_draw_a_file() {
    let f = feats(
        &[],
        &[
            (AttachmentKind::Image, 0, false),
            (AttachmentKind::File, 0, false),
        ],
    );
    let c = plain(
        &Body::Sticker(Box::new(att(1, AttachmentKind::Image, 5))),
        &f,
    );
    assert_eq!(
        c.parts,
        vec![ForwardPart::Files {
            files: vec![ForwardFile {
                attachment: AttachmentId(1),
                as_file: false
            }],
            caption: None
        }]
    );
    assert_eq!(c.changes, vec![ForwardChange::StickerAsPicture(1)]);
    let mut a = att(2, AttachmentKind::Image, 5);
    a.mime = "image/webp".into();
    a.duration_ms = Some(900);
    let c = plain(&Body::Sticker(Box::new(a)), &f);
    assert!(matches!(
        &c.parts[0],
        ForwardPart::Files { files, .. } if files[0].as_file
    ));
}

#[test]
fn a_location_is_text_with_an_exact_map_link() {
    let l = Location {
        lat_e7: -338_688_000,
        lon_e7: 1_512_093_000,
        name: Some("Opera".into()),
        address: Some("Bennelong Point".into()),
    };
    let c = plain(&Body::Location(l), &feats(&[], &[]));
    assert_eq!(
        only_text(&c).body,
        "Opera\nBennelong Point\nhttps://www.openstreetmap.org/?mlat=-33.8688000&mlon=151.2093000#map=16/-33.8688000/151.2093000"
    );
    let l = Location {
        lat_e7: -1,
        lon_e7: -1,
        ..Default::default()
    };
    let c = plain(&Body::Location(l), &feats(&[], &[]));
    assert_eq!(
        only_text(&c).body,
        "https://www.openstreetmap.org/?mlat=-0.0000001&mlon=-0.0000001#map=16/-0.0000001/-0.0000001"
    );
    assert_eq!(c.changes, vec![ForwardChange::LocationAsText(1)]);
}

#[test]
fn a_poll_is_a_question_and_bullets() {
    let p = Poll {
        question: "Lunch?".into(),
        options: vec!["Pizza".into(), "Soup".into()],
        multiple: false,
    };
    let c = plain(&Body::Poll(p), &feats(&[], &[]));
    assert_eq!(only_text(&c).body, "Lunch?\n\u{2022} Pizza\n\u{2022} Soup");
    assert_eq!(c.changes, vec![ForwardChange::PollAsText(1)]);
}

#[test]
fn changes_merge_per_kind() {
    let mut into = vec![
        ForwardChange::FormattingDropped(1),
        ForwardChange::FileTooLarge(vec![AttachmentId(1)]),
    ];
    merge_changes(
        &mut into,
        vec![
            ForwardChange::FormattingDropped(2),
            ForwardChange::FileTooLarge(vec![AttachmentId(1), AttachmentId(2)]),
            ForwardChange::PollAsText(1),
        ],
    );
    assert_eq!(
        into,
        vec![
            ForwardChange::FormattingDropped(3),
            ForwardChange::FileTooLarge(vec![AttachmentId(1), AttachmentId(2)]),
            ForwardChange::PollAsText(1),
        ]
    );
}

#[test]
fn a_chat_without_stated_features_takes_the_files_as_they_are_and_no_formatting() {
    let body = Body::Media {
        attachments: vec![
            att(1, AttachmentKind::Video, 900_000_000),
            att(2, AttachmentKind::File, 10),
        ],
        caption: Some(Text {
            body: "bold".into(),
            ranges: vec![range(0, 4, RangeKind::Bold)],
        }),
    };
    let c = convert(
        &body,
        &TargetFacts {
            features: None,
            contact_cards: false,
        },
        How {
            native: false,
            look: ForwardLook::Plain,
            header: None,
            names: &names,
        },
    );
    let [ForwardPart::Files { files, caption }] = c.parts.as_slice() else {
        panic!("expected one media part, got {:?}", c.parts);
    };
    assert_eq!(
        files,
        &vec![
            ForwardFile {
                attachment: AttachmentId(1),
                as_file: false
            },
            ForwardFile {
                attachment: AttachmentId(2),
                as_file: false
            },
        ]
    );
    let caption = caption.as_ref().expect("the caption stays");
    assert_eq!(caption.body, "bold");
    assert!(caption.ranges.is_empty());
    assert_eq!(c.changes, vec![ForwardChange::FormattingDropped(1)]);
}
