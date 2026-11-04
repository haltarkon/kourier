//! The route table (W5b, G6/G7/G9): every row of the module doc, its limits on both sides, and the one type for
//! upload and encryption.

use kourier_model::AttachmentKind::{self, Audio, File, Gif, Image, Video, Voice};

use super::super::mp4::{Facts, VideoTrack};
use super::*;

fn video_facts(codec: &[u8; 4]) -> Probe {
    Probe::Mp4(Some(Facts {
        seconds: Some(9),
        video: Some(VideoTrack {
            codec: Some(*codec),
            dims: Some((640, 360)),
        }),
        audio: Some(*b"mp4a"),
    }))
}

fn audio_facts(codec: &[u8; 4]) -> Probe {
    Probe::Mp4(Some(Facts {
        seconds: Some(200),
        video: None,
        audio: Some(*codec),
    }))
}

/// The route's shape and its mimetype for a given file type.
fn go(kind: AttachmentKind, mime: &str, size: u64, probe: Probe) -> Option<(Shape, String)> {
    route(kind, mime, size, &probe).map(|r| (r.shape, r.mimetype(mime).to_owned()))
}

fn shape(kind: AttachmentKind, mime: &str, size: u64, probe: Probe) -> Option<Shape> {
    go(kind, mime, size, probe).map(|(s, _)| s)
}

const SMALL: u64 = 2_000_000;

#[test]
fn the_table() {
    use Shape::{Audio as A, Document as D, Image as I, Video as V};
    // Pictures: re-encoded when they decode and the re-encode fits; else a document.
    assert_eq!(
        shape(Image, "image/jpeg", SMALL, Probe::Still(Some(900_000))),
        Some(I)
    );
    assert_eq!(
        shape(Image, "image/png", SMALL, Probe::Still(Some(MEDIA_LIMIT))),
        Some(I)
    );
    assert_eq!(
        shape(
            Image,
            "image/png",
            SMALL,
            Probe::Still(Some(MEDIA_LIMIT + 1))
        ),
        Some(D)
    );
    assert_eq!(
        shape(Image, "image/heic", SMALL, Probe::Still(None)),
        Some(D),
        "does not decode"
    );
    assert_eq!(look(Image, "image/jpeg", PICTURE_LIMIT), Some(Look::Still));
    assert_eq!(
        look(Image, "image/bmp", PICTURE_LIMIT + 1),
        Some(Look::Nothing(D)),
        "a picture over 64 MiB is streamed as a document, never read whole"
    );
    assert_eq!(
        shape(Image, "image/gif", SMALL, Probe::Nothing),
        Some(D),
        "a GIF the core called a picture"
    );
    // GIFs and "send as file".
    assert_eq!(shape(Gif, "image/gif", SMALL, Probe::Nothing), Some(D));
    assert_eq!(
        look(Gif, "image/gif", SMALL),
        Some(Look::Nothing(D)),
        "a GIF is never re-encoded"
    );
    assert_eq!(look(Image, "image/gif", SMALL), Some(Look::Nothing(D)));
    assert_eq!(
        shape(File, "image/jpeg", SMALL, Probe::Still(Some(10))),
        Some(D)
    );
    assert_eq!(
        shape(File, "video/mp4", SMALL, video_facts(b"avc1")),
        Some(D)
    );
    assert_eq!(
        shape(File, "application/pdf", 3_000_000_000, Probe::Nothing),
        Some(D)
    );
    // MP4 by its tracks, up to the media limit.
    assert_eq!(look(Video, "video/mp4", MEDIA_LIMIT), Some(Look::Mp4));
    assert_eq!(
        shape(Video, "video/mp4", MEDIA_LIMIT, video_facts(b"avc1")),
        Some(V)
    );
    assert_eq!(
        shape(Video, "video/mp4", SMALL, video_facts(b"avc3")),
        Some(V)
    );
    assert_eq!(
        look(Video, "video/mp4", MEDIA_LIMIT + 1),
        Some(Look::Nothing(D))
    );
    assert_eq!(
        shape(Video, "video/mp4", MEDIA_LIMIT + 1, video_facts(b"avc1")),
        Some(D)
    );
    assert_eq!(
        shape(Video, "video/mp4", SMALL, video_facts(b"hvc1")),
        Some(D),
        "HEVC"
    );
    assert_eq!(
        shape(Video, "video/mp4", SMALL, video_facts(b"av01")),
        Some(D),
        "AV1"
    );
    assert_eq!(
        shape(Video, "video/mp4", SMALL, Probe::Mp4(None)),
        Some(D),
        "no moov"
    );
    let no_codec = Probe::Mp4(Some(Facts {
        video: Some(VideoTrack {
            codec: None,
            dims: None,
        }),
        ..Facts::default()
    }));
    assert_eq!(shape(Video, "video/mp4", SMALL, no_codec), Some(D));
    assert_eq!(
        go(Video, "video/mp4", SMALL, audio_facts(b"mp4a")),
        Some((A, "audio/mp4".to_owned())),
        "an audio-only MP4 the sniffer called a video"
    );
    assert_eq!(
        go(Audio, "audio/mp4", SMALL, audio_facts(b"mp4a")),
        Some((A, "audio/mp4".to_owned()))
    );
    // Fix F10: an H.264 MP4 the core typed as audio is a video with a video type, not `audio/mp4`.
    for typed in ["audio/mp4", "audio/x-m4a", "audio/m4a"] {
        assert_eq!(
            go(Audio, typed, SMALL, video_facts(b"avc1")),
            Some((V, "video/mp4".to_owned())),
            "{typed}"
        );
    }
    assert_eq!(
        shape(Audio, "audio/mp4", SMALL, audio_facts(b"alac")),
        Some(D)
    );
    assert_eq!(
        shape(
            Audio,
            "audio/mp4",
            SMALL,
            Probe::Mp4(Some(Facts::default()))
        ),
        Some(D)
    );
    // Other containers.
    assert_eq!(shape(Video, "video/webm", SMALL, Probe::Nothing), Some(D));
    assert_eq!(
        shape(Video, "video/quicktime", SMALL, Probe::Nothing),
        Some(D)
    );
    // Audio: Opus in Ogg and MP3 as audio; the rest as documents.
    assert_eq!(
        go(Audio, "audio/ogg", SMALL, Probe::Ogg { opus: true }),
        Some((A, "audio/ogg; codecs=opus".to_owned()))
    );
    assert_eq!(
        shape(Audio, "audio/ogg", SMALL, Probe::Ogg { opus: false }),
        Some(D),
        "Vorbis"
    );
    assert_eq!(
        go(Audio, "audio/mpeg", MEDIA_LIMIT, Probe::Mpeg { mp3: true }),
        Some((A, "audio/mpeg".to_owned()))
    );
    // Fix F9: the label alone is not enough; an empty or mislabelled file is a document, as is an unanswered look.
    assert_eq!(look(Audio, "audio/mpeg", 0), Some(Look::Mpeg));
    assert_eq!(
        shape(Audio, "audio/mpeg", 0, Probe::Mpeg { mp3: false }),
        Some(D),
        "a zero-byte .mp3"
    );
    assert_eq!(
        shape(Audio, "audio/mp3", SMALL, Probe::Mpeg { mp3: false }),
        Some(D)
    );
    assert_eq!(shape(Audio, "audio/mpeg", SMALL, Probe::Nothing), Some(D));
    assert_eq!(
        shape(Audio, "audio/mpeg", MEDIA_LIMIT + 1, Probe::Nothing),
        Some(D)
    );
    assert_eq!(shape(Audio, "audio/flac", SMALL, Probe::Nothing), Some(D));
    assert_eq!(shape(Audio, "audio/wav", SMALL, Probe::Nothing), Some(D));
    assert_eq!(
        shape(Audio, "audio/aac", SMALL, Probe::Nothing),
        Some(D),
        "raw AAC"
    );
    // A MIME type with parameters or capitals is read by its essence.
    assert_eq!(look(Audio, "Audio/MPEG; x=1", SMALL), Some(Look::Mpeg));
    // A probe that does not answer the look is a document, never a guess.
    assert_eq!(shape(Video, "video/mp4", SMALL, Probe::Nothing), Some(D));
    assert_eq!(shape(Image, "image/jpeg", SMALL, Probe::Nothing), Some(D));
    // Voice notes are not built (G9).
    assert_eq!(look(Voice, "audio/ogg", SMALL), None);
    assert_eq!(
        route(Voice, "audio/ogg", SMALL, &Probe::Ogg { opus: true }),
        None
    );
}

#[test]
fn each_shape_uploads_and_encrypts_under_its_own_type() {
    use wacore::download::MediaType;
    let pairs = [
        (
            Shape::Image,
            MediaType::Image,
            "WhatsApp Image Keys",
            "image",
        ),
        (
            Shape::Video,
            MediaType::Video,
            "WhatsApp Video Keys",
            "video",
        ),
        (
            Shape::Audio,
            MediaType::Audio,
            "WhatsApp Audio Keys",
            "audio",
        ),
        (
            Shape::Document,
            MediaType::Document,
            "WhatsApp Document Keys",
            "document",
        ),
    ];
    for (shape, media_type, info, path) in pairs {
        let route = Route { shape, mime: None };
        assert_eq!(route.media_type(), media_type);
        assert_eq!(route.media_type().app_info(), info);
        assert_eq!(route.media_type().mms_type(), path);
    }
}

#[test]
fn an_ogg_is_opus_only_when_its_first_packet_says_so() {
    let page = |flags: u8, lacing: &[u8], packet: &[u8]| {
        let mut out = b"OggS".to_vec();
        out.push(0);
        out.push(flags);
        out.extend_from_slice(&[0u8; 20]);
        out.push(u8::try_from(lacing.len()).expect("small"));
        out.extend_from_slice(lacing);
        out.extend_from_slice(packet);
        out
    };
    assert!(opus_head(&page(0x02, &[19], b"OpusHead\x01\x02\x38\x01")));
    assert!(
        !opus_head(&page(0x02, &[30], b"\x01vorbis\0\0\0\0\x02")),
        "Vorbis"
    );
    assert!(
        !opus_head(&page(0x02, &[42], b"\x80theora\x03\x02")),
        "Theora"
    );
    assert!(
        !opus_head(&page(0x00, &[19], b"OpusHead")),
        "not a first page"
    );
    assert!(
        !opus_head(&page(0x02, &[19, 1, 1], b"\x00\x00OpusHead")),
        "the packet starts after every lacing value"
    );
    assert!(!opus_head(&page(0x02, &[255; 255], b"Opus")), "cut short");
    assert!(!opus_head(b"OggS"));
    assert!(!opus_head(&[]));
    assert!(page(0x02, &[255; 255], b"OpusHead").len() <= OGG_HEAD);
}

/// Fix F9: an MP3 starts with an `ID3` tag or an MPEG audio frame header; an empty file, text, a raw AAC stream (ADTS
/// has layer 0) and the reserved version and layer values are not MP3. Mutant: the sniff removed (always true).
#[test]
fn an_mp3_starts_with_an_id3_tag_or_a_frame_sync() {
    assert!(mpeg_head(b"ID3\x04\0\0"));
    assert!(mpeg_head(b"ID3"));
    for second in [0xFB, 0xFA, 0xF3, 0xE3, 0xF5, 0xFD] {
        assert!(mpeg_head(&[0xFF, second, 0x90]), "{second:#x}");
    }
    assert!(!mpeg_head(b""));
    assert!(!mpeg_head(b"I"));
    assert!(!mpeg_head(b"ID"));
    assert!(!mpeg_head(&[0xFF]));
    assert!(!mpeg_head(b"synthetic text that is no audio"));
    assert!(!mpeg_head(&[0xFE, 0xFB, 0x90]), "no sync");
    assert!(!mpeg_head(&[0xFF, 0xF1, 0x50]), "ADTS (raw AAC)");
    assert!(!mpeg_head(&[0xFF, 0xF9, 0x50]), "ADTS (raw AAC)");
    assert!(!mpeg_head(&[0xFF, 0xEB, 0x90]), "reserved version");
    assert!(
        !mpeg_head(&[0xFF, 0x1B, 0x90]),
        "three bits of sync are not eleven"
    );
}
