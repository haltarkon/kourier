//! The glimpse a quote shows of a quoted file (W3b, chunk C4a).

use kourier_model::{AttachmentKind, AttachmentState};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{
    CAPTIONED_DOCUMENT, FileFacts, MAX_FILE_NAME, MAX_MIME, MAX_WAVEFORM, MediaPointer, POINTER_V,
    file_message, file_of, glimpse, hex, kept_file_name, kept_mime,
};
use crate::map::class::Content;

#[test]
fn a_content_type_and_a_file_name_are_kept_only_within_the_core_s_bounds() {
    assert_eq!(kept_mime(Some("image/jpeg")).as_deref(), Some("image/jpeg"));
    assert_eq!(kept_mime(Some("")), None);
    assert_eq!(kept_mime(None), None);
    assert_eq!(kept_mime(Some(&"a".repeat(MAX_MIME + 1))), None);
    assert!(kept_mime(Some(&"a".repeat(MAX_MIME))).is_some());
    assert_eq!(kept_mime(Some("image/\njpeg")), None, "a control character");
    assert_eq!(kept_file_name(Some("a.pdf")).as_deref(), Some("a.pdf"));
    assert_eq!(kept_file_name(Some("")), None);
    assert_eq!(kept_file_name(Some(&"n".repeat(MAX_FILE_NAME + 1))), None);
}

#[test]
fn the_file_of_a_message_is_named_by_its_kind() {
    let mut image = wa::Message::default();
    let mut m = wa::message::ImageMessage::default();
    m.mimetype = Some("image/jpeg".into());
    image.image_message = MessageField::some(m);
    let got = glimpse(&image).expect("a picture");
    assert_eq!(got.kind, AttachmentKind::Image);
    assert_eq!(got.mime.as_deref(), Some("image/jpeg"));

    let mut gif = wa::Message::default();
    let mut m = wa::message::VideoMessage::default();
    m.gif_playback = Some(true);
    gif.video_message = MessageField::some(m);
    assert_eq!(glimpse(&gif).expect("gif").kind, AttachmentKind::Gif);
    let mut video = wa::Message::default();
    video.video_message = MessageField::some(wa::message::VideoMessage::default());
    assert_eq!(glimpse(&video).expect("video").kind, AttachmentKind::Video);
    let mut note = wa::Message::default();
    note.ptv_message = MessageField::some(wa::message::VideoMessage::default());
    assert_eq!(glimpse(&note).expect("note").kind, AttachmentKind::Video);

    let mut voice = wa::Message::default();
    let mut m = wa::message::AudioMessage::default();
    m.ptt = Some(true);
    voice.audio_message = MessageField::some(m);
    assert_eq!(glimpse(&voice).expect("voice").kind, AttachmentKind::Voice);
    let mut audio = wa::Message::default();
    audio.audio_message = MessageField::some(wa::message::AudioMessage::default());
    assert_eq!(glimpse(&audio).expect("audio").kind, AttachmentKind::Audio);

    let mut document = wa::Message::default();
    let mut m = wa::message::DocumentMessage::default();
    m.file_name = Some("report.pdf".into());
    m.mimetype = Some("application/pdf".into());
    document.document_message = MessageField::some(m);
    let got = glimpse(&document).expect("document");
    assert_eq!(got.kind, AttachmentKind::File);
    assert_eq!(got.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(got.mime.as_deref(), Some("application/pdf"));

    let mut sticker = wa::Message::default();
    sticker.sticker_message = MessageField::some(wa::message::StickerMessage::default());
    assert_eq!(
        glimpse(&sticker).expect("sticker").kind,
        AttachmentKind::Image
    );

    // A text, a contact, a location and a poll carry no file.
    let mut text = wa::Message::default();
    text.conversation = Some("hello".into());
    let mut contact = wa::Message::default();
    contact.contact_message = MessageField::some(wa::message::ContactMessage::default());
    assert!(glimpse(&text).is_none() && glimpse(&contact).is_none());
}

// ---- C4b: the file of a message as a remote pointer ------------------------------------------------------

const KEY: [u8; 32] = [0xA5; 32];

fn picture() -> wa::message::ImageMessage {
    let mut m = wa::message::ImageMessage::default();
    m.mimetype = Some("image/jpeg".into());
    m.direct_path = Some("/v/t62.7118-24/synthetic".into());
    m.media_key = Some(KEY.to_vec());
    m.file_sha256 = Some(vec![1; 32]);
    m.file_enc_sha256 = Some(vec![2; 32]);
    m.file_length = Some(54_321);
    m.media_key_timestamp = Some(1_700_000_000);
    m.static_url = Some("https://example.invalid/static".into());
    m.jpeg_thumbnail = Some(vec![9; 10]);
    m.width = Some(640);
    m.height = Some(480);
    m.caption = Some("a caption".into());
    m
}

fn message_of(image: wa::message::ImageMessage) -> wa::Message {
    let mut message = wa::Message::default();
    message.image_message = MessageField::some(image);
    message
}

#[test]
fn a_picture_is_a_remote_pointer_that_decodes_to_the_same_fields_and_hides_its_key() {
    let file = file_of(&message_of(picture()), Content::Image).expect("a picture");
    let a = &file.attachment;
    assert_eq!(a.kind, AttachmentKind::Image);
    assert_eq!(a.mime, "image/jpeg");
    assert_eq!(a.size, 54_321);
    assert_eq!(a.dims, Some((640, 480)));
    assert_eq!(a.state, AttachmentState::Remote);
    assert_eq!(file.caption.as_deref(), Some("a caption"));
    assert_eq!(a.net_ids, [format!("sha256:{}", hex(&[1; 32]))]);

    let blob = a.remote.as_ref().expect("a pointer");
    assert_eq!(blob.v, POINTER_V);
    let pointer = MediaPointer::from_blob(blob).expect("decodes");
    assert_eq!(pointer.media_type, "image");
    assert_eq!(pointer.direct_path, "/v/t62.7118-24/synthetic");
    assert_eq!(pointer.media_key, hex(&KEY));
    assert_eq!(pointer.file_sha256.as_deref(), Some(hex(&[1; 32]).as_str()));
    assert_eq!(
        pointer.file_enc_sha256.as_deref(),
        Some(hex(&[2; 32]).as_str())
    );
    assert_eq!(pointer.file_length, 54_321);
    assert_eq!(pointer.media_key_timestamp, Some(1_700_000_000));
    assert_eq!(
        pointer.static_url.as_deref(),
        Some("https://example.invalid/static")
    );
    assert_eq!(pointer.thumb_jpeg.as_deref(), Some(hex(&[9; 10]).as_str()));

    // A media key is a secret: neither the pointer's nor the attachment's Debug holds a byte of it.
    for shown in [format!("{pointer:?}"), format!("{a:?}")] {
        assert!(!shown.contains(&hex(&KEY)), "the key in {shown}");
        assert!(
            !shown.contains("165"),
            "the key as decimal bytes in {shown}"
        );
        assert!(!shown.contains("synthetic"), "the path in {shown}");
    }
    assert_eq!(
        format!("{pointer:?}"),
        "MediaPointer { media_type: image, file_length: 54321 }"
    );
}

#[test]
fn a_file_that_cannot_be_downloaded_is_unavailable_and_has_no_pointer() {
    let none = |change: &dyn Fn(&mut wa::message::ImageMessage)| {
        let mut image = picture();
        change(&mut image);
        file_of(&message_of(image), Content::Image)
            .expect("still a picture")
            .attachment
    };
    for a in [
        none(&|i| i.direct_path = None),
        none(&|i| i.direct_path = Some(String::new())),
        none(&|i| i.media_key = None),
        none(&|i| i.media_key = Some(Vec::new())),
        none(&|i| i.media_key = Some(vec![1; 65])),
        none(&|i| i.direct_path = Some("p".repeat(2049))),
    ] {
        assert_eq!(a.state, AttachmentState::Unavailable);
        assert!(a.remote.is_none(), "no pointer to download from");
        // What the message says of the file stays.
        assert_eq!(a.size, 54_321);
        assert_eq!(a.dims, Some((640, 480)));
    }
}

#[test]
fn the_pointer_keeps_only_values_in_bounds() {
    let mut image = picture();
    image.jpeg_thumbnail = Some(vec![7; 64 * 1024 + 1]);
    image.file_sha256 = Some(vec![1; 31]);
    image.file_enc_sha256 = Some(vec![2; 33]);
    image.width = Some(0);
    image.mimetype = Some("image/\njpeg".into());
    let a = file_of(&message_of(image), Content::Image)
        .expect("picture")
        .attachment;
    let pointer = MediaPointer::from_blob(a.remote.as_ref().expect("pointer")).expect("decodes");
    assert!(pointer.thumb_jpeg.is_none(), "a thumbnail past 64 KiB");
    assert!(pointer.file_sha256.is_none() && pointer.file_enc_sha256.is_none());
    assert!(a.net_ids.is_empty(), "no hash, no name");
    assert_eq!(a.dims, None, "a zero side is no size");
    assert_eq!(a.mime, "application/octet-stream", "a control character");
    // An unknown version is not read.
    let mut blob = a.remote.expect("pointer");
    blob.v = POINTER_V + 1;
    assert!(MediaPointer::from_blob(&blob).is_none());
}

#[test]
fn a_voice_note_is_a_voice_attachment_with_its_waveform_and_an_audio_file_is_not() {
    let mut voice = wa::message::AudioMessage::default();
    voice.mimetype = Some("audio/ogg; codecs=opus".into());
    voice.ptt = Some(true);
    voice.seconds = Some(7);
    voice.direct_path = Some("/v/t62.7117-24/voice".into());
    voice.media_key = Some(KEY.to_vec());
    voice.file_length = Some(9_000);
    voice.waveform = Some((0..64).collect());
    let mut message = wa::Message::default();
    message.audio_message = MessageField::some(voice.clone());
    let file = file_of(&message, Content::Audio).expect("a voice note");
    let a = file.attachment;
    assert_eq!(a.kind, AttachmentKind::Voice);
    assert_eq!(a.mime, "audio/ogg; codecs=opus");
    assert_eq!(a.duration_ms, Some(7_000));
    assert_eq!(a.waveform, Some((0..64).collect()));
    assert_eq!(a.state, AttachmentState::Remote);
    assert!(file.caption.is_none());
    let pointer = MediaPointer::from_blob(a.remote.as_ref().expect("pointer")).expect("decodes");
    assert_eq!(pointer.media_type, "audio");

    // A waveform longer than the bars the core takes is cut.
    voice.waveform = Some(vec![3; MAX_WAVEFORM + 20]);
    message.audio_message = MessageField::some(voice.clone());
    let cut = file_of(&message, Content::Audio).expect("voice").attachment;
    assert_eq!(cut.waveform.map(|w| w.len()), Some(MAX_WAVEFORM));

    // An audio file (not `ptt`) is an audio attachment with no waveform.
    voice.ptt = None;
    voice.waveform = Some(vec![3; 64]);
    message.audio_message = MessageField::some(voice);
    let audio = file_of(&message, Content::Audio).expect("audio").attachment;
    assert_eq!(audio.kind, AttachmentKind::Audio);
    assert_eq!(audio.waveform, None);
}

#[test]
fn videos_gifs_notes_documents_and_stickers_are_named_by_their_kind() {
    let mut video = wa::message::VideoMessage::default();
    video.mimetype = Some("video/mp4".into());
    video.seconds = Some(12);
    video.width = Some(1920);
    video.height = Some(1080);
    video.caption = Some("watch".into());
    video.direct_path = Some("/v/video".into());
    video.media_key = Some(KEY.to_vec());
    let mut message = wa::Message::default();
    message.video_message = MessageField::some(video.clone());
    let got = file_of(&message, Content::Video).expect("video");
    assert_eq!(got.attachment.kind, AttachmentKind::Video);
    assert_eq!(got.attachment.duration_ms, Some(12_000));
    assert_eq!(got.attachment.dims, Some((1920, 1080)));
    assert_eq!(got.caption.as_deref(), Some("watch"));

    // A video note is a video; its pointer keeps that it was a note.
    let mut note = wa::Message::default();
    note.ptv_message = MessageField::some(video.clone());
    let got = file_of(&note, Content::VideoNote).expect("note");
    assert_eq!(got.attachment.kind, AttachmentKind::Video);
    let pointer =
        MediaPointer::from_blob(got.attachment.remote.as_ref().expect("pointer")).expect("reads");
    assert_eq!(pointer.media_type, "ptv");

    video.gif_playback = Some(true);
    message.video_message = MessageField::some(video);
    let gif = file_of(&message, Content::Video).expect("gif");
    assert_eq!(gif.attachment.kind, AttachmentKind::Gif);

    let mut document = wa::message::DocumentMessage::default();
    document.mimetype = Some("application/pdf".into());
    document.file_name = Some("report.pdf".into());
    document.caption = Some("see".into());
    document.direct_path = Some("/v/doc".into());
    document.media_key = Some(KEY.to_vec());
    document.file_length = Some(123);
    let mut message = wa::Message::default();
    message.document_message = MessageField::some(document);
    let got = file_of(&message, Content::Document).expect("document");
    assert_eq!(got.attachment.kind, AttachmentKind::File);
    assert_eq!(got.attachment.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(got.attachment.mime, "application/pdf");
    assert_eq!(got.caption.as_deref(), Some("see"));

    let mut sticker = wa::message::StickerMessage::default();
    sticker.direct_path = Some("/v/sticker".into());
    sticker.media_key = Some(KEY.to_vec());
    sticker.width = Some(512);
    sticker.height = Some(512);
    sticker.png_thumbnail = Some(vec![5; 4]);
    let mut message = wa::Message::default();
    message.sticker_message = MessageField::some(sticker);
    let got = file_of(&message, Content::Sticker).expect("sticker");
    assert_eq!(got.attachment.kind, AttachmentKind::Image);
    assert_eq!(
        got.attachment.mime, "image/webp",
        "a sticker that states no type"
    );
    assert_eq!(got.attachment.dims, Some((512, 512)));
    let pointer =
        MediaPointer::from_blob(got.attachment.remote.as_ref().expect("pointer")).expect("reads");
    assert_eq!(pointer.media_type, "sticker");
    assert_eq!(pointer.thumb_jpeg.as_deref(), Some(hex(&[5; 4]).as_str()));

    // A kind with no file, or a message that lacks the field its class names.
    assert!(file_of(&wa::Message::default(), Content::Image).is_none());
    assert!(file_of(&message, Content::Contact).is_none());
}

// ---- W5b B10: the one builder of a file message ----------------------------------------------------------------

/// A pointer of a file Kourier sent, of `shape`.
fn sent(shape: &str, media_type: &str, tail: u8) -> MediaPointer {
    MediaPointer {
        media_type: media_type.into(),
        direct_path: format!("/v/t62.7118-24/synthetic-sent-9{tail:07}"),
        media_key: hex(&[tail; 32]),
        file_sha256: Some(hex(&[tail ^ 0x11; 32])),
        file_enc_sha256: Some(hex(&[tail ^ 0x22; 32])),
        file_length: 40_000 + u64::from(tail),
        media_key_timestamp: Some(1_761_400_000 + i64::from(tail)),
        url: Some(format!("https://synthetic.invalid/sent-8{tail:07}")),
        shape: Some(shape.into()),
        ..MediaPointer::default()
    }
}

#[test]
fn every_built_file_maps_back() {
    let mut picture = sent("image", "image", 1);
    picture.mimetype = Some("image/jpeg".into());
    picture.width = Some(1600);
    picture.height = Some(900);
    picture.thumb_jpeg = Some(hex(&[0xFF, 0xD8, 0x01, 0xFF, 0xD9]));
    let mut video = sent("video", "video", 2);
    video.mimetype = Some("video/mp4".into());
    video.width = Some(720);
    video.height = Some(1280);
    video.seconds = Some(93);
    video.streaming_sidecar = Some(hex(&[7; 20]));
    let mut audio = sent("audio", "audio", 3);
    audio.mimetype = Some("audio/mp4".into());
    audio.seconds = Some(241);
    audio.streaming_sidecar = Some(hex(&[8; 10]));
    let mut document = sent("document", "document", 4);
    document.mimetype = Some("application/pdf".into());
    let mut captioned = sent(CAPTIONED_DOCUMENT, "document", 5);
    captioned.mimetype = Some("text/plain".into());
    // The core's facts of each (its own type and dims lose to the pointer's; a document's name is the core's).
    let facts = |kind, mime, file_name| FileFacts {
        kind,
        mime,
        file_name,
        dims: Some((4000, 3000)),
        duration_ms: None,
    };
    for (case, pointer, facts, caption, content, kind) in [
        (
            "picture",
            picture,
            facts(AttachmentKind::Image, "image/png", None),
            Some("synthetic picture caption"),
            Content::Image,
            AttachmentKind::Image,
        ),
        (
            "video",
            video,
            facts(AttachmentKind::Video, "video/mp4", None),
            Some("synthetic video caption"),
            Content::Video,
            AttachmentKind::Video,
        ),
        (
            "audio",
            audio,
            facts(AttachmentKind::Audio, "audio/x-m4a", None),
            None,
            Content::Audio,
            AttachmentKind::Audio,
        ),
        (
            "document",
            document,
            facts(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-plain-61820394.pdf"),
            ),
            None,
            Content::Document,
            AttachmentKind::File,
        ),
        (
            "captioned document",
            captioned,
            facts(
                AttachmentKind::Gif,
                "image/gif",
                Some("synthetic-moving-72039415.gif"),
            ),
            Some("synthetic document caption"),
            Content::Document,
            AttachmentKind::File,
        ),
    ] {
        let message = file_message(&pointer, &facts, caption.map(str::to_owned)).expect(case);
        // A caption that is a document's goes inside the wrapper and nowhere else.
        assert_eq!(
            message.document_with_caption_message.as_option().is_some(),
            case == "captioned document",
            "{case}: the wrapper"
        );
        let base = crate::map::class::peel(&message);
        assert_eq!(
            crate::map::class::class_of(base),
            crate::map::class::Class::Content(content),
            "{case}: the receive map's class"
        );
        let back = file_of(base, content).expect(case);
        let a = &back.attachment;
        assert_eq!(a.kind, kind, "{case}: kind");
        assert_eq!(Some(&a.mime), pointer.mimetype.as_ref(), "{case}: mime");
        assert_eq!(a.size, pointer.file_length, "{case}: size");
        assert_eq!(a.dims, pointer.width.zip(pointer.height), "{case}: dims");
        assert_eq!(
            a.duration_ms,
            pointer.seconds.map(|s| s * 1000),
            "{case}: seconds"
        );
        assert_eq!(a.file_name.as_deref(), facts.file_name, "{case}: file name");
        assert_eq!(back.caption.as_deref(), caption, "{case}: caption");
        let read = MediaPointer::from_blob(a.remote.as_ref().expect("a pointer")).expect(case);
        assert_eq!(read.media_type, pointer.media_type, "{case}");
        assert_eq!(read.direct_path, pointer.direct_path, "{case}");
        assert_eq!(read.media_key, pointer.media_key, "{case}");
        assert_eq!(read.file_sha256, pointer.file_sha256, "{case}");
        assert_eq!(read.file_enc_sha256, pointer.file_enc_sha256, "{case}");
        assert_eq!(read.file_length, pointer.file_length, "{case}");
        assert_eq!(
            read.media_key_timestamp, pointer.media_key_timestamp,
            "{case}"
        );
        assert_eq!(read.thumb_jpeg, pointer.thumb_jpeg, "{case}");
    }
    // The fields a receiver needs that the receive map does not keep.
    let mut video = sent("video", "video", 6);
    video.streaming_sidecar = Some(hex(&[9; 20]));
    let m = file_message(
        &video,
        &facts(AttachmentKind::Video, "video/mp4", None),
        None,
    )
    .expect("a video");
    let v = m.video_message.as_option().expect("a videoMessage");
    assert_eq!(v.streaming_sidecar.as_deref(), Some(&[9; 20][..]));
    assert_eq!(v.url, video.url);
    // An audio message carries no caption: no message rather than a lost caption.
    let audio = sent("audio", "audio", 7);
    assert!(
        file_message(
            &audio,
            &facts(AttachmentKind::Audio, "audio/mpeg", None),
            Some("synthetic lost".into())
        )
        .is_none()
    );
}

#[test]
fn an_old_pointer_reads_as_before_and_a_sent_one_keeps_its_new_fields_secret() {
    // W3b's JSON: no new field.
    let old = br#"{"media_type":"image","direct_path":"/v/synthetic-old-30192847","media_key":"ab","file_length":5}"#;
    let blob = kourier_model::BackendBlob::new(POINTER_V, old.to_vec());
    let read = MediaPointer::from_blob(&blob).expect("an old pointer");
    assert_eq!(read.shape, None);
    assert_eq!(read.url, None);
    assert_eq!(read.direct_path, "/v/synthetic-old-30192847");
    // A sent pointer survives the JSON whole.
    let mut pointer = sent(CAPTIONED_DOCUMENT, "document", 9);
    pointer.streaming_sidecar = Some(hex(&[1; 10]));
    pointer.mimetype = Some("application/pdf".into());
    pointer.width = Some(3);
    pointer.height = Some(4);
    pointer.seconds = Some(5);
    let blob = pointer.blob();
    assert_eq!(MediaPointer::from_blob(&blob).as_ref(), Some(&pointer));
    // Debug prints the type and the length only.
    let printed = format!("{pointer:?}");
    assert!(!printed.contains("synthetic"), "{printed}");
    assert!(!printed.contains(&pointer.media_key), "{printed}");
}
