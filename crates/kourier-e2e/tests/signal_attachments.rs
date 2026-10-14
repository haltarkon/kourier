//! S12 through the real core (docs/architecture.md §5.8, §6.7 step 2, §6.11, §11 S12): the
//! app's core with the Signal backend against `@signalapp/mock-server`, driven through the
//! facade as the UI drives it, the harness playing our phone ("Myself") and a contact ("Alice").
//!
//! [`attachments_travel_both_ways_byte_identical_and_never_rest_in_plaintext`]:
//!
//! - Alice sends a photo with a caption, a voice note, a sticker and a long text (> 2 KiB, as
//!   Desktop sends it: trimmed body + `text/x-signal-plain` attachment). The core downloads
//!   them by its policy and each opens byte-identical through `OpenMedia`; the long text shows
//!   whole.
//! - Kourier sends a file with a caption: Alice's phone decrypts it byte-identical from the
//!   mock's CDN 3 (TUS upload, MAC and digest checked harness-side), our phone's sync
//!   transcript carries the same pointer, and the UI is told the upload's progress. A large
//!   photo arrives re-encoded at Signal's "High" level (a JPEG of its own size up to 4096 px,
//!   not the 1600 px of Desktop's default; no EXIF, no file name); an MP4 carries a valid
//!   incremental MAC (checked with libsignal-client); a long text arrives trimmed with the
//!   whole text attached.
//! - A send the server rate-limits is retried by the core's outbox with the pointer of the
//!   first attempt: one upload, not two.
//! - Afterwards no file in the profile directory contains any of the plaintexts.
//!
//! [`hostile_pointers_fail_for_good_and_leave_nothing_behind`]: a contact's pointer with a wrong
//! digest, a size smaller than its blob, a size over the receive limit, a CDN number that does
//! not exist or a CDN key that is a path ends as a failed attachment that no retry is offered
//! for; nothing of it rests in the profile (no plaintext, no temporary file), and the next
//! honest photo arrives as usual.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_core::facade::{MessageRow, Progress, UiCommand, UiEvent};
use kourier_model::{
    Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, ConversationId, OutgoingState,
    Text,
};
use serde_json::Value;
use support::harness::{ForgedPointer, OutgoingFile, SendAttachments};
use support::{App, WAIT, World, init_tracing};

/// Deterministic bytes that occur in no other test input.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The newest row whose body satisfies `pred`, once every attachment of it is `Local`.
async fn wait_media(
    app: &App,
    conv: ConversationId,
    what: &str,
    pred: impl Fn(&Body) -> bool,
) -> MessageRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows(conv).await;
        if let Some(row) = rows.iter().find(|row| pred(&row.body)) {
            let all_local = attachments_of(&row.body)
                .iter()
                .all(|a| a.state == AttachmentState::Local);
            if all_local {
                return row.clone();
            }
            assert!(
                attachments_of(&row.body)
                    .iter()
                    .all(|a| !matches!(a.state, AttachmentState::Failed { .. })),
                "{what}: a download failed: {:?}",
                row.body
            );
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}: {rows:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn attachments_of(body: &Body) -> Vec<Attachment> {
    match body {
        Body::Media { attachments, .. } => attachments.clone(),
        Body::Sticker(a) => vec![(**a).clone()],
        _ => Vec::new(),
    }
}

fn caption_is(body: &Body, text: &str) -> bool {
    matches!(body, Body::Media { caption: Some(c), .. } if c.body == text)
}

/// The plaintext of a downloaded attachment, through the facade's `OpenMedia`.
async fn opened(app: &mut App, attachment: AttachmentId) -> Vec<u8> {
    app.send(UiCommand::OpenMedia { attachment });
    let path = app
        .event("MediaOpened", |e| match e {
            UiEvent::MediaOpened {
                attachment: a,
                path,
            } if *a == attachment => Some(path.clone()),
            UiEvent::ActionFailed { .. } => panic!("OpenMedia failed: {e:?}"),
            _ => None,
        })
        .await;
    let bytes = std::fs::read(&path).expect("media file");
    app.send(UiCommand::CloseMedia { attachment });
    bytes
}

/// Every `"attachments"` array inside `value` (pointers in a DataMessage or a transcript).
fn pointers_in(value: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if key == "attachments"
                    && let Value::Array(items) = item
                {
                    found.extend(items.iter().cloned());
                } else {
                    found.extend(pointers_in(item));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                found.extend(pointers_in(item));
            }
        }
        _ => {}
    }
    found
}

/// Waits for Alice's next message from our device and returns its DataMessage.
async fn alice_receives(world: &World, me: &str) -> Value {
    world
        .harness
        .wait_for_message(&world.alice.aci, Some(me), WAIT)
        .await
        .expect("Alice gets the message")
        .data_message
}

/// Every regular file below `dir`.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => out.extend(files_below(&path)),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
    out
}

/// No file below `dir` contains a 64-byte window from the middle of any of `plaintexts`.
fn assert_no_plaintext(dir: &Path, plaintexts: &[(&str, &[u8])]) {
    let files = files_below(dir);
    assert!(!files.is_empty(), "the profile has files");
    for file in files {
        let bytes = std::fs::read(&file).unwrap_or_default();
        for (name, plaintext) in plaintexts {
            let mid = plaintext.len() / 2;
            let needle = &plaintext[mid.saturating_sub(32)..(mid + 32).min(plaintext.len())];
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "plaintext of {name} found in {}",
                file.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachments_travel_both_ways_byte_identical_and_never_rest_in_plaintext() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    // The app's media cache is the tmpfs `$XDG_RUNTIME_DIR`, and the user's files live
    // elsewhere: neither is in the profile directory.
    let media = tempfile::tempdir().expect("media dir");
    let picked = tempfile::tempdir().expect("picked files");
    let mut app = App::start_with_media(world.tmp.path(), &world.env, media.path()).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let features = app.features(conv).await;
    for kind in [
        AttachmentKind::Image,
        AttachmentKind::Video,
        AttachmentKind::Voice,
        AttachmentKind::File,
    ] {
        let limits = features.files.get(&kind).expect("Signal takes every kind");
        // The largest file whose padded, encrypted upload is within the server's 100 MiB
        // (`global.attachments.maxBytes` counts the upload, not the file).
        assert_eq!(limits.max_size, 102_224_512, "{kind:?}");
    }
    assert_eq!(features.max_text_bytes, Some(64 * 1024));

    // --- From Alice's phone to Kourier.
    let photo = kourier_media::sample_image(640, 480, 21, kourier_media::SampleFormat::Png)
        .expect("sample photo");
    let voice = noise(24_000, 1);
    let sticker = kourier_media::sample_image(128, 128, 5, kourier_media::SampleFormat::Png)
        .expect("sample sticker");
    let from_alice =
        |attachments: Vec<OutgoingFile>, text: Option<&str>, is_sticker: bool| SendAttachments {
            from: world.alice.aci.clone(),
            to: me.clone(),
            attachments,
            text: text.map(str::to_owned),
            sticker: is_sticker,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendAttachments::default()
        };
    world
        .harness
        .send_attachments(&from_alice(
            vec![OutgoingFile {
                width: Some(640),
                height: Some(480),
                file_name: Some("field.png".to_owned()),
                ..OutgoingFile::new(&photo, "image/png")
            }],
            Some("Field photo"),
            false,
        ))
        .await
        .expect("photo");
    let row = wait_media(&app, conv, "the photo", |b| caption_is(b, "Field photo")).await;
    let [image] = attachments_of(&row.body)
        .try_into()
        .expect("one attachment");
    assert_eq!(image.kind, AttachmentKind::Image);
    assert_eq!(image.dims, Some((640, 480)));
    assert_eq!(image.size, photo.len() as u64);
    let id = image.id.expect("stored attachment");
    assert_eq!(
        opened(&mut app, id).await,
        photo,
        "the photo is byte-identical"
    );

    world
        .harness
        .send_attachments(&from_alice(
            vec![OutgoingFile {
                flags: Some(1),
                duration_seconds: Some(3.0),
                ..OutgoingFile::new(&voice, "audio/aac")
            }],
            None,
            false,
        ))
        .await
        .expect("voice note");
    let row = wait_media(&app, conv, "the voice note", |b| {
        attachments_of(b)
            .first()
            .is_some_and(|a| a.kind == AttachmentKind::Voice)
    })
    .await;
    // (The mock's protobufs predate `audioDurationSeconds`; its mapping is unit-tested.)
    let note = &attachments_of(&row.body)[0];
    let id = note.id.expect("stored voice note");
    assert_eq!(
        opened(&mut app, id).await,
        voice,
        "the voice note is byte-identical"
    );

    world
        .harness
        .send_attachments(&from_alice(
            vec![OutgoingFile::new(&sticker, "image/png")],
            None,
            true,
        ))
        .await
        .expect("sticker");
    let row = wait_media(&app, conv, "the sticker", |b| matches!(b, Body::Sticker(_))).await;
    let id = attachments_of(&row.body)[0].id.expect("stored sticker");
    assert_eq!(
        opened(&mut app, id).await,
        sticker,
        "the sticker is byte-identical"
    );

    let long_from_alice: String = (0..400).map(|i| format!("line {i} from Alice. ")).collect();
    assert!(long_from_alice.len() > 4096);
    world
        .harness
        .send_attachments(&from_alice(Vec::new(), Some(&long_from_alice), false))
        .await
        .expect("long text");
    app.wait_text(conv, &long_from_alice).await;

    // --- From Kourier to Alice (and our phone's transcript).
    let uploads_before = world.harness.send_stats().await.expect("stats").uploads;
    let file = noise(300_000, 2);
    let file_path = picked.path().join("report.bin");
    std::fs::write(&file_path, &file).expect("write the picked file");
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![file_path].into(),
        caption: Some(Text::plain("the report")),
        reply_to: None,
    });
    // The UI hears about the upload: shares of the blob below 1, then 1 once it is uploaded.
    let mut shares = Vec::new();
    let mut steps = Vec::new();
    app.event("the upload's progress", |e| match e {
        UiEvent::Progress(Progress::Transmit {
            progress, phase, ..
        }) => {
            shares.push(*progress);
            steps.push(*phase);
            (*progress >= 1.0).then_some(())
        }
        _ => None,
    })
    .await;
    assert!(
        shares.is_sorted() && shares.iter().rev().skip(1).all(|p| *p < 1.0),
        "{shares:?}"
    );
    // The line under the message says "Uploading files" while the blobs go up and no longer
    // once they are up: a 1:1 send reports no step after this one, so the end of the uploads
    // must clear the last (link feedback L6).
    let (done, uploading) = steps.split_last().expect("a report");
    assert!(
        uploading
            .iter()
            .all(|s| *s == Some(kourier_model::SendPhase::Uploading)),
        "{steps:?}"
    );
    assert_eq!(*done, None, "the uploads are over: no step ({steps:?})");
    let message = alice_receives(&world, &me).await;
    assert_eq!(message["body"], "the report");
    let pointers = pointers_in(&message);
    assert_eq!(pointers.len(), 1, "{message}");
    let pointer = &pointers[0];
    assert_eq!(pointer["fileName"], "report.bin");
    assert_eq!(pointer["cdnNumber"], 3);
    let fetched = world
        .harness
        .fetch_attachment(pointer)
        .await
        .expect("Alice downloads it");
    assert!(fetched.mac_valid && fetched.digest_valid, "{fetched:?}");
    assert_eq!(
        fetched.bytes(),
        file,
        "the file is byte-identical on the phone"
    );
    // Padded to Desktop's next 5 % bucket (307 632 bytes) plus IV, PKCS7 and MAC.
    assert_eq!(fetched.blob_length, 307_680, "padded to Desktop's buckets");
    let transcript = world
        .harness
        .wait_for_sync_message(&world.phone.device_ref, Some(&me), "sent", WAIT)
        .await
        .expect("our phone gets the transcript");
    let synced = pointers_in(&transcript.sync_message);
    assert_eq!(synced.len(), 1);
    assert_eq!(synced[0]["digest"], pointer["digest"], "the same upload");
    let row = app
        .wait_row_where(conv, "the sent file", |row| {
            caption_is(&row.body, "the report")
                && matches!(
                    row.out_state,
                    Some(
                        OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read
                    )
                )
        })
        .await;
    assert!(row.out_state.is_some());

    // A large photo is re-encoded before it leaves: without metadata, and at the best quality
    // an official client sends (level three, "High"), so 2400 × 1800 arrives as 2400 × 1800
    // and not as the 1600 × 1200 of Desktop's default level.
    let big = kourier_media::sample_image(2400, 1800, 3, kourier_media::SampleFormat::Png)
        .expect("big photo");
    let big_path = picked.path().join("big.png");
    std::fs::write(&big_path, &big).expect("write the big photo");
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![big_path].into(),
        caption: None,
        reply_to: None,
    });
    let message = alice_receives(&world, &me).await;
    let pointer = pointers_in(&message).remove(0);
    assert_eq!(pointer["contentType"], "image/jpeg");
    assert_eq!(
        (pointer["width"].clone(), pointer["height"].clone()),
        (2400.into(), 1800.into()),
        "the photo keeps its size"
    );
    assert!(
        pointer.get("fileName").is_none_or(Value::is_null),
        "{pointer}"
    );
    let fetched = world
        .harness
        .fetch_attachment(&pointer)
        .await
        .expect("photo");
    let jpeg = fetched.bytes();
    assert_eq!(&jpeg[..2], [0xff, 0xd8], "a JPEG");
    assert!(!jpeg.windows(4).any(|w| w == b"Exif"), "no metadata");

    // An MP4 carries the incremental MAC Desktop's players stream-verify.
    let mut mp4 = vec![0, 0, 0, 0x18];
    mp4.extend_from_slice(b"ftypmp42\0\0\0\0mp42isom");
    mp4.extend(noise(200_000, 3));
    let mp4_path = picked.path().join("clip.mp4");
    std::fs::write(&mp4_path, &mp4).expect("write the clip");
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![mp4_path].into(),
        caption: None,
        reply_to: None,
    });
    let message = alice_receives(&world, &me).await;
    let pointer = pointers_in(&message).remove(0);
    assert_eq!(pointer["contentType"], "video/mp4");
    let fetched = world
        .harness
        .fetch_attachment(&pointer)
        .await
        .expect("clip");
    assert_eq!(fetched.incremental_mac_valid, Some(true), "{fetched:?}");
    assert_eq!(fetched.bytes(), mp4);

    // A long text goes out trimmed with the whole text attached.
    let long_from_me: String = (0..300).map(|i| format!("Kourier line {i}. ")).collect();
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain(long_from_me.clone()),
        reply_to: None,
    });
    let message = alice_receives(&world, &me).await;
    let body = message["body"].as_str().expect("body");
    assert!(
        body.len() <= 2048 && long_from_me.starts_with(body),
        "trimmed"
    );
    let pointer = pointers_in(&message).remove(0);
    assert_eq!(pointer["contentType"], "text/x-signal-plain");
    let fetched = world
        .harness
        .fetch_attachment(&pointer)
        .await
        .expect("long text");
    assert_eq!(fetched.bytes(), long_from_me.as_bytes());

    // A send the server rate-limits is retried by the outbox (§4.8); the retried Transmit
    // reuses the pointer of the upload that already happened (§6.7 step 2).
    let stats = world.harness.send_stats().await.expect("stats");
    assert_eq!(
        stats.uploads - uploads_before,
        4,
        "one upload each: the file, the photo, the clip, the long text"
    );
    world
        .harness
        .rate_limit(&world.alice.device_ref, Some(1), 1)
        .await
        .expect("rate limit");
    let notes = noise(40_000, 4);
    let notes_path = picked.path().join("notes.bin");
    std::fs::write(&notes_path, &notes).expect("write the notes");
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![notes_path].into(),
        caption: Some(Text::plain("second try")),
        reply_to: None,
    });
    let message = alice_receives(&world, &me).await;
    assert_eq!(message["body"], "second try");
    let pointer = pointers_in(&message).remove(0);
    let fetched = world
        .harness
        .fetch_attachment(&pointer)
        .await
        .expect("notes");
    assert_eq!(fetched.bytes(), notes);
    let after = world.harness.send_stats().await.expect("stats");
    assert_eq!(
        after.single_to(&world.alice.aci) - stats.single_to(&world.alice.aci),
        2,
        "the refused send and its retry"
    );
    assert_eq!(
        after.uploads - stats.uploads,
        1,
        "uploaded once, sent twice"
    );

    // --- Nothing rests in plaintext in the profile.
    app.stop().await;
    assert_no_plaintext(
        world.tmp.path(),
        &[
            ("the photo", &photo),
            ("the voice note", &voice),
            ("the sticker", &sticker),
            ("Alice's long text", long_from_alice.as_bytes()),
            ("the file", &file),
            ("the big photo", &big),
            ("the clip", &mp4),
            ("our long text", long_from_me.as_bytes()),
            ("the re-encoded photo", &jpeg),
            ("the notes", &notes),
        ],
    );
    world.shutdown().await;
}

/// The attachment of the row captioned `caption`, once its download failed or the backend
/// said at receipt that it can never be fetched (`Unavailable`).
async fn wait_failed(app: &App, conv: ConversationId, caption: &str) -> Attachment {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows(conv).await;
        let found = rows
            .iter()
            .find(|row| caption_is(&row.body, caption))
            .and_then(|row| attachments_of(&row.body).into_iter().next());
        if let Some(attachment) = &found {
            assert_ne!(
                attachment.state,
                AttachmentState::Local,
                "{caption}: a hostile attachment was accepted"
            );
            if matches!(
                attachment.state,
                AttachmentState::Failed { .. } | AttachmentState::Unavailable
            ) {
                return attachment.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{caption}: {found:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hostile_pointers_fail_for_good_and_leave_nothing_behind() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    let media = tempfile::tempdir().expect("media dir");
    let mut app = App::start_with_media(world.tmp.path(), &world.env, media.path()).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let conv = app.accept(&world, account, &device, &world.alice).await;

    // Every hostile message carries the same bytes, as a picture so the core fetches it by
    // its policy; the blob on the CDN is honest, the pointer is not.
    let bait = noise(50_000, 11);
    let from_alice = |caption: &str, file: OutgoingFile| SendAttachments {
        from: world.alice.aci.clone(),
        to: me.clone(),
        attachments: vec![file],
        text: Some(caption.to_owned()),
        await_delivery: true,
        timeout: Some(60_000),
        ..SendAttachments::default()
    };
    let forged = |forge: ForgedPointer| OutgoingFile {
        forge: Some(forge),
        ..OutgoingFile::new(&bait, "image/png")
    };
    // A pointer that fails only when its download is checked ends Failed for good; one the
    // backend can tell at receipt it can never fetch (a CDN this build has no address for, a
    // CDN key that is not a safe token) is "not available on this device" at once, with no
    // request and no dead "Retry Download".
    let failed = AttachmentState::Failed { retryable: false };
    for (caption, forge, expected) in [
        (
            "a wrong digest",
            ForgedPointer {
                digest: true,
                ..ForgedPointer::default()
            },
            failed,
        ),
        (
            // 100 bytes may take 592 on the CDN; the blob has over 50 000.
            "a size smaller than the blob",
            ForgedPointer {
                size: Some(100),
                ..ForgedPointer::default()
            },
            failed,
        ),
        (
            "a CDN that does not exist",
            ForgedPointer {
                cdn_number: Some(77),
                ..ForgedPointer::default()
            },
            AttachmentState::Unavailable,
        ),
        (
            "a path as the CDN key",
            ForgedPointer {
                cdn_key: Some("../../v1/config".to_owned()),
                ..ForgedPointer::default()
            },
            AttachmentState::Unavailable,
        ),
    ] {
        world
            .harness
            .send_attachments(&from_alice(caption, forged(forge)))
            .await
            .expect(caption);
        let attachment = wait_failed(&app, conv, caption).await;
        assert_eq!(attachment.state, expected, "{caption}: no retry can help");
        assert!(attachment.local.is_none(), "{caption}");
    }

    // A size over the receive limit is not fetched by the policy; asked for by hand, the
    // backend refuses it before it requests anything.
    let caption = "a size over the limit";
    world
        .harness
        .send_attachments(&from_alice(
            caption,
            forged(ForgedPointer {
                size: Some(u32::MAX),
                ..ForgedPointer::default()
            }),
        ))
        .await
        .expect(caption);
    let row = app
        .wait_row_where(conv, caption, |row| caption_is(&row.body, caption))
        .await;
    let huge = attachments_of(&row.body).remove(0);
    if !matches!(huge.state, AttachmentState::Failed { .. }) {
        app.send(UiCommand::DownloadAttachment {
            attachment: huge.id.expect("stored attachment"),
        });
    }
    let attachment = wait_failed(&app, conv, caption).await;
    assert_eq!(
        attachment.state,
        AttachmentState::Failed { retryable: false }
    );

    // The backend is none the worse: an honest photo still arrives and opens.
    let photo = kourier_media::sample_image(320, 240, 8, kourier_media::SampleFormat::Png)
        .expect("sample photo");
    world
        .harness
        .send_attachments(&from_alice(
            "an honest photo",
            OutgoingFile::new(&photo, "image/png"),
        ))
        .await
        .expect("photo");
    let row = wait_media(&app, conv, "the honest photo", |b| {
        caption_is(b, "an honest photo")
    })
    .await;
    let id = attachments_of(&row.body)[0].id.expect("stored attachment");
    assert_eq!(opened(&mut app, id).await, photo);

    app.stop().await;
    assert_no_plaintext(
        world.tmp.path(),
        &[("the bait", &bait), ("the honest photo", &photo)],
    );
    let leftovers: Vec<PathBuf> = files_below(world.tmp.path())
        .into_iter()
        .filter(|path| path.extension().is_some_and(|e| e == "tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    world.shutdown().await;
}
