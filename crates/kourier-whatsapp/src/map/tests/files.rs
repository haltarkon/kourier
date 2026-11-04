//! The map beside the files (W3b, chunk C4b): a picture, a voice note, a sticker and an album child become
//! messages with remote pointers. Numbers and ids are the fictitious 555-01xx range.

use kourier_model::{AttachmentKind, AttachmentState, Body, RangeKind};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::content::{MEMBER_PN, ctx, map, message_of};
use super::{PEER, meta, row_with};
use crate::media::{MediaPointer, hex};

const KEY: [u8; 32] = [0xA5; 32];

fn picture(caption: &str, fill: impl FnOnce(&mut wa::ContextInfo)) -> wa::message::ImageMessage {
    let mut context = wa::ContextInfo::default();
    fill(&mut context);
    let mut m = wa::message::ImageMessage::default();
    m.mimetype = Some("image/jpeg".into());
    m.direct_path = Some("/v/t62.7118-24/synthetic".into());
    m.media_key = Some(KEY.to_vec());
    m.file_sha256 = Some(vec![1; 32]);
    m.file_length = Some(1_000);
    m.width = Some(30);
    m.height = Some(20);
    m.caption = Some(caption.into());
    m.context_info = MessageField::some(context);
    m
}

fn message_with(image: wa::message::ImageMessage) -> wa::Message {
    let mut message = wa::Message::default();
    message.image_message = MessageField::some(image);
    message
}

#[test]
fn a_picture_is_a_remote_pointer_and_no_file_with_its_caption_read_like_a_text() {
    let image = picture("look *here* @15550100102", |c| {
        c.mentioned_jid = vec![MEMBER_PN.to_owned()];
        c.stanza_id = Some("3EB0QUOTED2".into());
    });
    let got = message_of(&map(&row_with(&message_with(image), &meta(), PEER, PEER)));
    let Body::Media {
        attachments,
        caption,
    } = &got.body
    else {
        panic!("a media message: {:?}", got.body);
    };
    assert_eq!(attachments.len(), 1);
    let a = &attachments[0];
    assert_eq!(a.kind, AttachmentKind::Image);
    assert_eq!(a.state, AttachmentState::Remote);
    assert!(a.local.is_none(), "no download before W7");
    let pointer = MediaPointer::from_blob(a.remote.as_ref().expect("pointer")).expect("decodes");
    assert_eq!(pointer.media_key, hex(&KEY));
    assert_eq!(pointer.direct_path, "/v/t62.7118-24/synthetic");
    // The caption goes through WhatsApp's formatting and mentions, with the message's own context.
    let caption = caption.as_ref().expect("a caption");
    assert!(!caption.body.contains('*'), "the markers are gone");
    assert_eq!(caption.body.matches('\u{FFFC}').count(), 1);
    assert!(caption.ranges.iter().any(|r| r.kind == RangeKind::Bold));
    assert!(got.reply_to.is_some(), "the picture quotes a message");
    assert!(got.raw.is_none(), "a mapped picture keeps no raw payload");
    // The event's Debug, which the logs may print, holds neither the key nor the path.
    let shown = format!("{got:?}");
    assert!(
        !shown.contains(&hex(&KEY)) && !shown.contains("synthetic"),
        "{shown}"
    );
}

#[test]
fn a_picture_that_mentions_us_in_its_caption_sets_mentions_me() {
    let image = picture("hey @99900100999", |c| {
        c.mentioned_jid = vec!["99900100999@lid".to_owned()];
    });
    let got = message_of(&map(&row_with(&message_with(image), &meta(), PEER, PEER)));
    assert!(got.mentions_me);
    assert!(ctx().own.lid.is_some(), "the fixture has our ids");
}

#[test]
fn a_picture_without_a_caption_has_none_and_a_view_once_picture_says_so() {
    let mut image = picture("", |_| {});
    image.view_once = Some(true);
    // Nearly every message carries a secret in its context info; a picture keeps no `raw` for it.
    let mut message = message_with(image);
    let mut context = wa::MessageContextInfo::default();
    context.message_secret = Some(vec![4; 32]);
    message.message_context_info = MessageField::some(context);
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    assert!(matches!(&got.body, Body::Media { caption: None, .. }));
    assert!(got.view_once);
    assert!(got.raw.is_none());
}

#[test]
fn an_album_child_is_a_picture_and_not_an_unsupported_bubble() {
    // Two photos sent at once arrive as children of an album: a FutureProof wrapper around the picture, with
    // the association on the OUTER message (critic H2).
    let mut wrapper = wa::message::FutureProofMessage::default();
    wrapper.message = MessageField::some(message_with(picture("", |_| {})));
    let mut message = wa::Message::default();
    message.associated_child_message = MessageField::some(wrapper);
    let mut context = wa::MessageContextInfo::default();
    let mut association = wa::MessageAssociation::default();
    association.association_type = Some(wa::message_association::AssociationType::MEDIA_ALBUM);
    association.message_index = Some(1);
    let mut parent = wa::MessageKey::default();
    parent.id = Some("3EB0ALBUMPARENT".into());
    association.parent_message_key = MessageField::some(parent);
    context.message_association = MessageField::some(association);
    context.message_secret = Some(vec![4; 32]);
    context.padding_bytes = Some(vec![0; 40]);
    message.message_context_info = MessageField::some(context);
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    assert!(
        matches!(&got.body, Body::Media { attachments, .. } if attachments.len() == 1),
        "{:?}",
        got.body
    );
    // The association rides in `raw` for the album grid (K-B3 C5); the rest of the context info does not.
    let raw = got.raw.expect("the association is kept");
    let kept = waproto::codec::message_decode(&raw.bytes).expect("an encoded message");
    let context = kept.message_context_info.as_option().expect("a context");
    let association = context
        .message_association
        .as_option()
        .expect("association");
    assert_eq!(
        association.association_type,
        Some(wa::message_association::AssociationType::MEDIA_ALBUM)
    );
    assert_eq!(association.message_index, Some(1));
    assert_eq!(
        association
            .parent_message_key
            .as_option()
            .and_then(|k| k.id.as_deref()),
        Some("3EB0ALBUMPARENT")
    );
    assert!(
        context.message_secret.is_none() && context.padding_bytes.is_none(),
        "only the association of a picture"
    );
}

#[test]
fn a_sticker_is_a_sticker() {
    let mut sticker = wa::message::StickerMessage::default();
    sticker.direct_path = Some("/v/t62.15575-24/sticker".into());
    sticker.media_key = Some(KEY.to_vec());
    sticker.width = Some(512);
    sticker.height = Some(512);
    let mut message = wa::Message::default();
    message.sticker_message = MessageField::some(sticker);
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let Body::Sticker(a) = &got.body else {
        panic!("a sticker: {:?}", got.body);
    };
    assert_eq!(a.kind, AttachmentKind::Image);
    assert_eq!(a.state, AttachmentState::Remote);
    assert_eq!(a.dims, Some((512, 512)));
}

#[test]
fn a_voice_note_is_a_voice_attachment_with_its_waveform_in_the_message() {
    let mut voice = wa::message::AudioMessage::default();
    voice.mimetype = Some("audio/ogg; codecs=opus".into());
    voice.ptt = Some(true);
    voice.seconds = Some(3);
    voice.direct_path = Some("/v/t62.7117-24/voice".into());
    voice.media_key = Some(KEY.to_vec());
    voice.waveform = Some(vec![10; 64]);
    let mut message = wa::Message::default();
    message.audio_message = MessageField::some(voice);
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let Body::Media { attachments, .. } = &got.body else {
        panic!("a voice note: {:?}", got.body);
    };
    assert_eq!(attachments[0].kind, AttachmentKind::Voice);
    assert_eq!(attachments[0].duration_ms, Some(3_000));
    assert_eq!(attachments[0].waveform.as_deref(), Some(&[10u8; 64][..]));
}

// ---- C4c: a contact, a place, a poll -----------------------------------------------------------------

use kourier_model::{IdentifierKind, Location};

const VCARD: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:;Alex Example;;;\r\nFN:Alex Example\r\nORG:Example Ltd;Sales\r\n\
item1.TEL;type=CELL;type=VOICE;waid=15550100102:+1 555-0100-102\r\nTEL;type=HOME:555 0100 777\r\nEND:VCARD\r\n";

fn contact(vcard: &str, display_name: &str) -> wa::Message {
    let mut m = wa::message::ContactMessage::default();
    m.vcard = Some(vcard.into());
    m.display_name = Some(display_name.into());
    let mut message = wa::Message::default();
    message.contact_message = MessageField::some(m);
    message
}

#[test]
fn a_contact_card_is_a_contact_with_its_name_organisation_and_numbers() {
    let got = message_of(&map(&row_with(
        &contact(VCARD, "shown name"),
        &meta(),
        PEER,
        PEER,
    )));
    let Body::Contact(card) = &got.body else {
        panic!("a contact: {:?}", got.body);
    };
    assert_eq!(card.name.as_deref(), Some("Alex Example"), "the card's FN");
    assert_eq!(
        card.organization.as_deref(),
        Some("Example Ltd"),
        "the first part of ORG"
    );
    let numbers: Vec<(IdentifierKind, &str)> = card
        .identifiers
        .iter()
        .map(|i| (i.kind, i.value.as_str()))
        .collect();
    assert_eq!(
        numbers,
        [
            (IdentifierKind::Phone, "+15550100102"),
            (IdentifierKind::Phone, "5550100777"),
        ],
        "the waid number in E.164, the plain one as typed without separators"
    );
    assert!(got.raw.is_none());
}

#[test]
fn a_contact_without_a_card_falls_back_to_the_name_shown_and_one_with_nothing_is_unsupported() {
    // No FN in the card: the display name the sender's chat showed.
    let got = message_of(&map(&row_with(
        &contact("BEGIN:VCARD\nTEL:+15550100103\nEND:VCARD", "shown name"),
        &meta(),
        PEER,
        PEER,
    )));
    let Body::Contact(card) = &got.body else {
        panic!("a contact: {:?}", got.body);
    };
    assert_eq!(card.name.as_deref(), Some("shown name"));
    assert_eq!(card.identifiers[0].value, "+15550100103");
    // A card that says nothing at all keeps the payload instead.
    let row = row_with(&contact("garbage \u{0}\n:::", ""), &meta(), PEER, PEER);
    let got = message_of(&map(&row));
    assert!(
        matches!(&got.body, Body::Unsupported { kind_hint, .. } if kind_hint == "contact"),
        "{:?}",
        got.body
    );
    assert_eq!(got.raw.expect("raw").bytes, row.message);
}

fn place(lat: f64, lon: f64) -> wa::Message {
    let mut m = wa::message::LocationMessage::default();
    m.degrees_latitude = Some(lat);
    m.degrees_longitude = Some(lon);
    m.name = Some("Harbour".into());
    m.address = Some("1 Quay Street".into());
    let mut message = wa::Message::default();
    message.location_message = MessageField::some(m);
    message
}

#[test]
fn a_location_is_degrees_times_ten_million_with_its_name_and_address() {
    let got = message_of(&map(&row_with(
        &place(48.8584, -122.4194),
        &meta(),
        PEER,
        PEER,
    )));
    assert_eq!(
        got.body,
        Body::Location(Location {
            lat_e7: 488_584_000,
            lon_e7: -1_224_194_000,
            name: Some("Harbour".into()),
            address: Some("1 Quay Street".into()),
        })
    );
    // The edges of the globe fit.
    let edge = message_of(&map(&row_with(&place(-90.0, 180.0), &meta(), PEER, PEER)));
    assert!(
        matches!(&edge.body, Body::Location(l) if l.lat_e7 == -900_000_000 && l.lon_e7 == 1_800_000_000)
    );
}

#[test]
fn a_live_location_is_its_last_position_and_a_position_off_the_globe_is_unsupported() {
    let mut live = wa::message::LiveLocationMessage::default();
    live.degrees_latitude = Some(1.5);
    live.degrees_longitude = Some(2.5);
    live.caption = Some("on my way".into());
    let mut message = wa::Message::default();
    message.live_location_message = MessageField::some(live);
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    assert_eq!(
        got.body,
        Body::Location(Location {
            lat_e7: 15_000_000,
            lon_e7: 25_000_000,
            name: Some("on my way".into()),
            address: None,
        })
    );
    for bad in [place(91.0, 0.0), place(0.0, -181.0), place(f64::NAN, 0.0)] {
        let row = row_with(&bad, &meta(), PEER, PEER);
        let got = message_of(&map(&row));
        assert!(
            matches!(&got.body, Body::Unsupported { kind_hint, .. } if kind_hint == "location"),
            "{:?}",
            got.body
        );
        assert_eq!(got.raw.expect("raw").bytes, row.message);
    }
}

fn poll_message(count: Option<u32>, options: &[&str]) -> wa::Message {
    let mut m = wa::message::PollCreationMessage::default();
    m.name = Some("Lunch?".into());
    m.selectable_options_count = count;
    m.options = options
        .iter()
        .map(|name| {
            let mut o = wa::message::poll_creation_message::Option::default();
            o.option_name = Some((*name).into());
            o
        })
        .collect();
    let mut message = wa::Message::default();
    message.poll_creation_message_v3 = MessageField::some(m);
    message
}

#[test]
fn a_poll_is_a_poll_and_its_vote_is_deferred() {
    let single = message_of(&map(&row_with(
        &poll_message(Some(1), &["Pizza", "Soup", ""]),
        &meta(),
        PEER,
        PEER,
    )));
    assert_eq!(
        single.body,
        Body::Poll(kourier_model::Poll {
            question: "Lunch?".into(),
            options: vec!["Pizza".into(), "Soup".into()],
            multiple: false,
        }),
        "an empty option is dropped"
    );
    for count in [Some(0), Some(3), None] {
        let many = message_of(&map(&row_with(
            &poll_message(count, &["a", "b"]),
            &meta(),
            PEER,
            PEER,
        )));
        assert!(
            matches!(&many.body, Body::Poll(p) if p.multiple),
            "count {count:?}"
        );
    }
    // The poll's secret (it decrypts the votes of K8a) is kept in `raw`.
    let mut with_secret = poll_message(Some(1), &["a", "b"]);
    let mut context = wa::MessageContextInfo::default();
    context.message_secret = Some(vec![7; 32]);
    with_secret.message_context_info = MessageField::some(context);
    let got = message_of(&map(&row_with(&with_secret, &meta(), PEER, PEER)));
    let kept = waproto::codec::message_decode(&got.raw.expect("the secret is kept").bytes)
        .expect("an encoded message");
    assert_eq!(
        kept.message_context_info
            .as_option()
            .and_then(|c| c.message_secret.clone()),
        Some(vec![7; 32])
    );
    // A poll with no option is not a poll; the payload is kept.
    let row = row_with(&poll_message(Some(1), &[]), &meta(), PEER, PEER);
    let got = message_of(&map(&row));
    assert!(matches!(&got.body, Body::Unsupported { kind_hint, .. } if kind_hint == "poll"));
    // The vote is another kind: kept, with its content, for K8a.
    let mut vote = wa::Message::default();
    vote.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    let mapped =
        super::super::map_row(&row_with(&vote, &meta(), PEER, PEER), &ctx()).expect("maps");
    assert!(mapped.deferred && mapped.events.is_empty());
}

/// Fix F7 (the lead's Q8.2): the companion of a picture or a video (the HD or HEVC upload sent beside it,
/// a motion photo's video, a sticker's annotation) is kept for K-B3, never shown as a second message; an
/// album child, which has the same shape and another association, still is a message.
#[test]
fn a_companion_upload_is_deferred_not_a_second_message() {
    use wa::message_association::AssociationType as Type;
    let companion = |kind: Type| {
        let mut message = message_with(picture("", |_| {}));
        let mut context = wa::MessageContextInfo::default();
        let mut association = wa::MessageAssociation::default();
        association.association_type = Some(kind);
        let mut parent = wa::MessageKey::default();
        parent.id = Some("3EB0PARENT1".into());
        association.parent_message_key = MessageField::some(parent);
        context.message_association = MessageField::some(association);
        message.message_context_info = MessageField::some(context);
        message
    };
    for kind in [
        Type::HD_IMAGE_DUAL_UPLOAD,
        Type::HD_VIDEO_DUAL_UPLOAD,
        Type::HEVC_VIDEO_DUAL_UPLOAD,
        Type::MOTION_PHOTO,
        Type::STICKER_ANNOTATION,
    ] {
        let mapped = map(&row_with(&companion(kind), &meta(), PEER, PEER));
        assert!(
            mapped.deferred && mapped.events.is_empty(),
            "{kind:?} was shown: {mapped:?}"
        );
    }
    // The same picture as an album child (or with no association) is a message.
    let shown = map(&row_with(
        &companion(Type::MEDIA_ALBUM),
        &meta(),
        PEER,
        PEER,
    ));
    assert!(!shown.deferred && shown.events.len() == 1, "{shown:?}");
    let plain = map(&row_with(
        &message_with(picture("", |_| {})),
        &meta(),
        PEER,
        PEER,
    ));
    assert!(!plain.deferred && plain.events.len() == 1, "{plain:?}");
}
