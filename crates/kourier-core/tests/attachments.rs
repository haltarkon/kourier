//! S12, core side (§5.8, §8.6): the auto-download policy against the fake's synthetic CDN,
//! `AttachmentDone`, thumbnails and blurhashes, the media handle table, plaintext playback
//! files, `SendFiles` read back by the backend, ids that never repeat, and a plaintext scan of
//! the whole profile after the flows.

mod support;

use std::path::{Path, PathBuf};

use kourier_backend_api::BackendEvent;
use kourier_core::AccountVault;
use kourier_core::facade::{
    MediaVariant, MessageRow, Progress, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand,
    UiEvent,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::attachments::{Pointer, expired_attachment, synthetic_file, synthetic_image};
use kourier_fake::scripts::{T0, remote, signal_features};
use kourier_fake::{Flavor, Script, TransmitRule};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountCaps, Attachment, AttachmentId, AttachmentKind, AttachmentState, BackendError, Body,
    ConvKey, ConversationId, ConversationPatch, ErrorKind, FileLimits, MsgIdScope, NetworkKind,
    OutgoingState, Text,
};
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{Env, WAIT, add_linked, eventually};

const RECEIVE_LIMIT: u64 = 1 << 20;

fn text(s: &str) -> Option<Text> {
    Some(Text {
        body: s.into(),
        ranges: Vec::new(),
    })
}

fn media(local: i64, attachments: Vec<Attachment>, caption: Option<Text>) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "");
    m.body = Body::Media {
        attachments,
        caption,
    };
    BackendEvent::Message(m)
}

fn video() -> Attachment {
    Attachment {
        kind: AttachmentKind::Video,
        mime: "video/mp4".into(),
        size: 5000,
        remote: Some(Pointer::Bytes { len: 5000, seed: 7 }.to_blob()),
        ..Attachment::default()
    }
}

/// Alice sends a photo, a sticker, a small file, a file above the receive limit, a video and a
/// photo the CDN no longer has.
fn incoming_script() -> Script {
    let mut first = setup();
    first.insert(
        0,
        BackendEvent::AccountCaps(AccountCaps {
            max_attachment_bytes: Some(4 << 20),
            max_receive_attachment_bytes: Some(RECEIVE_LIMIT),
            ..Default::default()
        }),
    );
    let mut sticker = remote(&d_alice(), &alice(), &(T0 + 2).to_string(), T0 + 2, "");
    sticker.body = Body::Sticker(Box::new(synthetic_image(128, 128, 3)));
    Script::new("attachments", vec![uid(ME), uid(ME_PNI)])
        .deliver(first)
        .deliver(vec![
            media(T0 + 1, vec![synthetic_image(900, 600, 11)], text("look")),
            BackendEvent::Message(sticker),
            media(
                T0 + 3,
                vec![
                    synthetic_file("notes.bin", "application/octet-stream", 3000, 5),
                    synthetic_file("big.iso", "application/x-iso9660-image", 2 << 20, 6),
                    video(),
                    expired_attachment(AttachmentKind::Image),
                ],
                None,
            ),
        ])
}

/// `(id, state, has file, has thumb, derived)` of every attachment, by id.
fn rows(env: &Env) -> Vec<(AttachmentId, AttachmentState, bool, bool, bool)> {
    env.peek(|c| {
        let mut stmt = c
            .prepare(
                "SELECT id, state, file_id IS NOT NULL, thumb_file_id IS NOT NULL,
                        derived_at IS NOT NULL FROM attachments ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([], |r| {
            let state: String = r.get(1)?;
            Ok((
                AttachmentId(r.get(0)?),
                serde_json::from_str(&state).expect("state json"),
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
    })
}

fn conv_of(env: &Env) -> ConversationId {
    env.peek(|c| {
        c.query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [d_alice().encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("conversation")
    })
}

fn attachments_of(row: &MessageRow) -> Vec<Attachment> {
    match &row.body {
        Body::Media { attachments, .. } => attachments.clone(),
        Body::Sticker(a) => vec![a.as_ref().clone()],
        _ => Vec::new(),
    }
}

async fn wait_rows(
    env: &Env,
    what: &str,
    mut done: impl FnMut(&[(AttachmentId, AttachmentState, bool, bool, bool)]) -> bool,
) {
    let deadline = tokio::time::Instant::now() + WAIT * 3;
    loop {
        let r = rows(env);
        if done(&r) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {r:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Every attachment of the incoming script ends where the policy and the CDN say.
async fn settle_incoming(env: &Env) {
    use AttachmentState::*;
    wait_rows(env, "downloads and thumbnails", |r| {
        r.len() == 6
            && matches!(r[0], (_, Local, true, true, true))
            && matches!(r[1], (_, Local, true, true, true))
            && matches!(r[2], (_, Local, true, false, true))
            && matches!(r[3], (_, Failed { retryable: false }, false, false, _))
            && matches!(r[4], (_, Remote, false, false, _))
            && matches!(r[5], (_, Expired, false, false, _))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_attachments_follow_the_policy_and_reach_the_ui_through_media_handles() {
    let mut env = Env::new(Flavor::SignalLike, incoming_script()).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    settle_incoming(&env).await;
    let r = rows(&env);
    let (photo, sticker, file, big, video, gone) = (r[0].0, r[1].0, r[2].0, r[3].0, r[4].0, r[5].0);
    // Downloads are answered: no target is left in flight.
    let in_flight: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM attachment_downloads", [], |r| {
            r.get(0)
        })
        .expect("count")
    });
    assert_eq!(in_flight, 0);

    // Timeline rows: ids, states, the dimensions and blurhash the core derived; never keys or
    // network pointers.
    let conv = conv_of(&env);
    let timeline = env.timeline(conv).await;
    let all: Vec<Attachment> = timeline.iter().flat_map(attachments_of).collect();
    assert_eq!(all.len(), 6);
    assert!(
        all.iter()
            .all(|a| a.local.is_none() && a.thumb.is_none() && a.remote.is_none())
    );
    let p = all.iter().find(|a| a.id == Some(photo)).expect("photo");
    assert_eq!(
        (p.state, p.dims),
        (AttachmentState::Local, Some((900, 600)))
    );
    assert!(p.blurhash.as_deref().is_some_and(|b| b.len() >= 6));
    assert!(p.size > 0);

    // The handle table answers without a database: file, thumbnail and type.
    let handles = env.handle.media();
    let e = handles.get(photo).expect("photo handle");
    let plain = Pointer::from_blob(&synthetic_image(900, 600, 11).remote.expect("remote"))
        .expect("pointer")
        .plaintext()
        .expect("bytes")
        .expect("ok");
    let full = handles
        .read(e.blob(MediaVariant::Full).expect("file"))
        .expect("decrypt");
    assert_eq!(*full, plain);
    let thumb = handles
        .read(e.blob(MediaVariant::Thumb).expect("thumb"))
        .expect("decrypt thumb");
    let info = kourier_media::probe(&thumb).expect("thumbnail decodes");
    assert_eq!((info.width, info.height), (400, 267));
    let blur = e.blurhash.clone().expect("blurhash");
    assert_eq!(
        handles.placeholder(&blur, 16, 12).expect("pixels").len(),
        16 * 12 * 4
    );
    if let Some(out) = std::env::var_os("KOURIER_MEDIA_SHOT") {
        contact_sheet(
            &full,
            &thumb,
            &handles.placeholder(&blur, 64, 43).expect("blur"),
        )
        .save(PathBuf::from(out))
        .expect("save shot");
    }
    let s = handles.get(sticker).expect("sticker");
    assert!(s.thumb.is_some());
    assert!(handles.get(file).expect("file").thumb.is_none());
    assert!(handles.get(gone).expect("gone").file.is_none());

    // On click: the video downloads, with progress; the oversized file is refused.
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: video })
        .expect("send");
    env.event("download progress", |e| match e {
        UiEvent::Progress(Progress::Download { attachment, .. }) if *attachment == video => {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_rows(&env, "the clicked video", |r| {
        matches!(r[4], (_, AttachmentState::Local, true, false, true))
    })
    .await;
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: big })
        .expect("send");
    let err = env
        .event("refused download", |e| match e {
            UiEvent::ActionFailed {
                action: "DownloadAttachment",
                error,
                ..
            } => Some(error.to_string()),
            _ => None,
        })
        .await;
    assert!(err.contains("larger than the network allows"), "{err}");

    // Playback: a plaintext copy in the media cache while a view has it open.
    for _ in 0..2 {
        env.handle
            .send(UiCommand::OpenMedia { attachment: video })
            .expect("send");
    }
    let mut opened = Vec::new();
    for _ in 0..2 {
        opened.push(
            env.event("media opened", |e| match e {
                UiEvent::MediaOpened { attachment, path } if *attachment == video => {
                    Some(path.clone())
                }
                _ => None,
            })
            .await,
        );
    }
    assert_eq!(opened[0], opened[1], "one plaintext file per attachment");
    let path = opened[0].clone();
    assert!(path.starts_with(env.media_dir()));
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mp4"));
    let video_plain = Pointer::Bytes { len: 5000, seed: 7 }
        .plaintext()
        .expect("bytes")
        .expect("ok");
    assert_eq!(std::fs::read(&path).expect("plaintext"), video_plain);
    for _ in 0..2 {
        env.handle
            .send(UiCommand::CloseMedia { attachment: video })
            .expect("send");
    }
    let p2 = path.clone();
    eventually("the last close deletes the plaintext", WAIT, || {
        let p = p2.clone();
        async move { !p.exists() }
    })
    .await;

    // Leftovers a kill could leave: an unreferenced core blob (an import cut off before its
    // transaction) goes at the next start; a backend's own blob beside the core subtree stays.
    let blob_dir = env
        .vault
        .blob_dir(env.account(account).await.expect("row").uuid);
    let core_store = kourier_blob::BlobStore::open(&blob_dir.join("core")).expect("core store");
    let (orphan, _, _) = core_store.import(&mut &b"orphan"[..]).expect("orphan");
    let backend_store = kourier_blob::BlobStore::open(&blob_dir).expect("store");
    let (avatar, _, _) = backend_store.import(&mut &b"avatar"[..]).expect("avatar");

    // After a restart the table is empty; a miss resolves on the media read connection.
    env.restart().await;
    assert!(
        !core_store.path(&orphan).exists(),
        "the sweep removed the orphan"
    );
    assert!(
        backend_store.path(&avatar).exists(),
        "backend blobs are not the core's"
    );
    let handles = env.handle.media();
    assert!(handles.get(photo).is_none());
    let e = handles
        .resolve(photo)
        .await
        .expect("resolve")
        .expect("entry");
    assert!(e.file.is_some() && e.thumb.is_some());
    assert!(handles.get(photo).is_some(), "a resolved miss is kept");
    assert!(
        handles
            .resolve(AttachmentId(999_999))
            .await
            .expect("resolve")
            .is_none()
    );
    // The image provider's worker threads resolve a miss without an async runtime; a runtime
    // thread is refused instead of stalled.
    assert!(handles.get(sticker).is_none());
    assert!(handles.resolve_blocking(sticker).is_err());
    let worker = {
        let handles = handles.clone();
        std::thread::spawn(move || handles.resolve_blocking(sticker))
    };
    let found = tokio::task::spawn_blocking(move || worker.join())
        .await
        .expect("join")
        .expect("worker")
        .expect("resolve")
        .expect("entry");
    assert!(found.file.is_some() && found.thumb.is_some());
    assert!(handles.get(sticker).is_some());

    // The startup sweep kept every referenced blob.
    assert_eq!(
        blob_files(&blob_dir).len(),
        6,
        "photo + thumb, sticker + thumb, file, video"
    );
    // The app's own cache beside the core's (decrypted avatars) survives the core's exit.
    let avatars = env.media_dir().join("avatars");
    std::fs::create_dir_all(&avatars).expect("avatars");
    std::fs::write(avatars.join("a.png"), b"avatar").expect("avatar");
    env.stop().await;
    let mut left = Vec::new();
    walk(&env.media_dir(), &mut left);
    assert_eq!(
        left,
        [avatars.join("a.png")],
        "exit wipes only the core's files"
    );
}

/// A direct chat whose features take only small pictures without captions.
fn strict_chat() -> ConvKey {
    ConvKey::Direct(uid("aci:00000000-0000-4000-8000-00000000ca01"))
}

/// Every complete blob file the core wrote under the account's blob directory `dir` (its own
/// `core/` subtree).
fn blob_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for shard in std::fs::read_dir(dir.join("core"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for f in std::fs::read_dir(shard.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if f.path().extension().is_none() {
                out.push(f.path());
            }
        }
    }
    out
}

fn random_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sent_files_are_imported_and_read_back_byte_identical_by_the_backend() {
    // The first attempt fails for good, so the first message can be cancelled.
    let mut first = setup();
    first.push(BackendEvent::AccountCaps(AccountCaps {
        max_attachment_bytes: Some(4 << 20),
        ..Default::default()
    }));
    // A chat whose features take only small pictures, without captions (§3.8).
    let mut strict = ConversationPatch::new(strict_chat());
    let mut features = signal_features(false);
    features.files = [(
        AttachmentKind::Image,
        FileLimits {
            max_size: 1000,
            caption: false,
        },
    )]
    .into();
    strict.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(strict));
    let script = Script::new("send-files", vec![uid(ME), uid(ME_PNI)])
        .deliver(first)
        .fail_transmit(TransmitRule::attempts(
            BackendError::new(ErrorKind::InvalidInput, "refused"),
            1,
        ));
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    let conv = conv_of(&env);
    let uuid = env.account(account).await.expect("row").uuid;
    let blob_dir = env.vault.blob_dir(uuid);
    let src = tempfile::tempdir().expect("sources");
    let jpeg = sample_image(1200, 900, 21, SampleFormat::Jpeg).expect("jpeg");
    let doc = random_bytes(5000, 99);
    let (pjpeg, pdoc) = (
        src.path().join("holiday.jpg"),
        src.path().join("report.pdf"),
    );
    std::fs::write(&pjpeg, &jpeg).expect("write");
    std::fs::write(&pdoc, &doc).expect("write");

    let cmd = UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        conv,
        paths: vec![pjpeg.clone()].into(),
        caption: None,
        reply_to: None,
    };
    assert!(!format!("{cmd:?}").contains("holiday"), "paths never print");

    // 1. Refused for good, then cancelled: the message and its blobs go.
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv,
            paths: vec![pdoc.clone()].into(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("the first send failed", WAIT, || {
        let h = h.clone();
        async move {
            let rows = match h
                .query(Query::Timeline(TimelineQuery {
                    conv,
                    anchor: TimelineAnchor::Newest,
                    limit: 50,
                }))
                .await
            {
                Ok(QueryData::Timeline(p)) => p.rows,
                _ => Vec::new(),
            };
            rows.iter()
                .any(|r| matches!(r.out_state, Some(OutgoingState::Failed { .. })))
        }
    })
    .await;
    assert_eq!(blob_files(&blob_dir).len(), 1, "the imported document");
    let failed = env
        .timeline(conv)
        .await
        .into_iter()
        .find(|r| matches!(r.out_state, Some(OutgoingState::Failed { .. })))
        .expect("failed row");
    env.handle
        .send(UiCommand::CancelMessage { message: failed.id })
        .expect("send");
    let d = blob_dir.clone();
    eventually("cancel unlinks the blobs", WAIT, || {
        let d = d.clone();
        async move { blob_files(&d).is_empty() }
    })
    .await;

    // 2. Refusals: an unreadable path, a file above the send limit and files the chat's features
    // do not take leave nothing behind.
    let huge = src.path().join("huge.bin");
    std::fs::write(&huge, vec![0u8; (4 << 20) + 1]).expect("write");
    let tiny = src.path().join("tiny.png");
    std::fs::write(
        &tiny,
        sample_image(4, 4, 1, SampleFormat::Png).expect("png"),
    )
    .expect("write");
    let strict = env.peek(|c| {
        c.query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [strict_chat().encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("strict chat")
    });
    for (to, paths, caption, why) in [
        (
            conv,
            vec![src.path().join("missing.png")],
            None,
            "could not be read",
        ),
        (
            conv,
            vec![pdoc.clone(), huge],
            None,
            "larger than the network allows",
        ),
        (
            strict,
            vec![pdoc.clone()],
            None,
            "does not take this kind of file",
        ),
        (
            strict,
            vec![pjpeg.clone()],
            None,
            "larger than this chat allows",
        ),
    ] {
        env.handle
            .send(UiCommand::SendFiles {
                request: Default::default(),
                as_file: Vec::new(),
                conv: to,
                paths: paths.into(),
                caption,
                reply_to: None,
            })
            .expect("send");
        let err = env
            .event("refused send", |e| match e {
                UiEvent::ActionFailed {
                    action: "SendFiles",
                    error,
                    ..
                } => Some(error.to_string()),
                _ => None,
            })
            .await;
        assert!(err.contains(why), "{err}");
        assert!(blob_files(&blob_dir).is_empty(), "{why}");
    }
    // A caption the chat does not take with this kind of file is no refusal: the caption
    // goes first as a text of its own, the picture follows without one.
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: strict,
            paths: vec![tiny].into(),
            caption: text("hi"),
            reply_to: None,
        })
        .expect("send");
    let h = env.handle.clone();
    eventually("the caption and the picture are sent", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Timeline(TimelineQuery { conv: strict, anchor: TimelineAnchor::Newest, limit: 50 })).await,
                Ok(QueryData::Timeline(p)) if p.rows.len() == 2 && p.rows.iter().all(|r| matches!(r.out_state, Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read))))
        }
    })
    .await;
    let adapted = env.timeline(strict).await;
    assert!(matches!(&adapted[0].body, Body::Text(t) if t.body == "hi"));
    assert!(matches!(
        &adapted[1].body,
        Body::Media { attachments, caption: None } if attachments.len() == 1
    ));
    let strict_blobs = blob_files(&blob_dir).len();
    assert!(strict_blobs >= 1, "the tiny picture (and its thumbnail)");

    // 3. Two files with a caption: the fake network shows a picture and a document only in
    // messages of their own, so two messages are sent, the caption on the first, and the
    // backend read both blobs back.
    env.handle
        .send(UiCommand::SaveDraft {
            conv,
            draft: text("draft"),
        })
        .expect("send");
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv,
            paths: vec![pjpeg.clone(), pdoc.clone()].into(),
            caption: text("two files"),
            reply_to: None,
        })
        .expect("send");
    let control = env.control.clone();
    let in_conv = |t: &kourier_fake::TransmitRecord| {
        matches!(t.outcome, kourier_backend_api::TransmitOutcome::Sent { .. })
            && matches!(
                &t.req.kind,
                kourier_backend_api::OutgoingKind::Message {
                    body: Body::Media { .. },
                    ..
                }
            )
            && t.req.conv == d_alice()
    };
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.transmits(uuid).iter().filter(|t| in_conv(t)).count() == 2),
    )
    .await
    .expect("transmitted");
    let sent: Vec<_> = env
        .control
        .transmits(uuid)
        .into_iter()
        .filter(in_conv)
        .collect();
    let media = |t: &kourier_fake::TransmitRecord| match &t.req.kind {
        kourier_backend_api::OutgoingKind::Message {
            body: Body::Media {
                attachments,
                caption,
            },
            ..
        } => (attachments.clone(), caption.clone()),
        other => panic!("not media: {other:?}"),
    };
    let (first, caption) = media(&sent[0]);
    let (second, no_caption) = media(&sent[1]);
    assert_eq!(caption.as_ref().map(|c| c.body.as_str()), Some("two files"));
    assert!(
        no_caption.is_none(),
        "the caption goes with the first message only"
    );
    assert_eq!((first.len(), second.len()), (1, 1));
    assert!(
        sent[0].req.sent_at < sent[1].req.sent_at,
        "in the user's order"
    );
    let attachments = [first[0].clone(), second[0].clone()];
    for (a, want) in attachments.iter().zip([&jpeg, &doc]) {
        let r = a.local.as_ref().expect("blob ref");
        let got = kourier_blob::read_all(
            &blob_dir.join(&r.path),
            &kourier_blob::FileKey::from_bytes(*r.key.expose()),
        )
        .expect("decrypt");
        assert_eq!(&*got, want, "byte-identical");
    }
    assert_eq!(
        (
            attachments[0].kind,
            attachments[0].mime.as_str(),
            attachments[0].dims
        ),
        (AttachmentKind::Image, "image/jpeg", Some((1200, 900)))
    );
    assert!(attachments[0].thumb.is_some() && attachments[0].blurhash.is_some());
    assert_eq!(
        (attachments[1].kind, attachments[1].mime.as_str()),
        (AttachmentKind::File, "application/pdf")
    );
    assert_eq!(attachments[1].file_name.as_deref(), Some("report.pdf"));

    // The UI row: own, sent, local files with metadata, no keys; handles have the thumbnail.
    let h = env.handle.clone();
    eventually("sent state", WAIT, || {
        let h = h.clone();
        async move {
            matches!(h.query(Query::Timeline(TimelineQuery { conv, anchor: TimelineAnchor::Newest, limit: 50 })).await,
                Ok(QueryData::Timeline(p)) if p.rows.iter().any(|r| matches!(r.out_state, Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read))))
        }
    })
    .await;
    let row = env
        .timeline(conv)
        .await
        .into_iter()
        .find(|r| {
            matches!(
                &r.body,
                Body::Media {
                    caption: Some(_),
                    ..
                }
            )
        })
        .expect("media row");
    let atts = attachments_of(&row);
    assert!(
        atts.iter()
            .all(|a| a.state == AttachmentState::Local && a.local.is_none() && a.thumb.is_none())
    );
    let e = env
        .handle
        .media()
        .get(atts[0].id.expect("id"))
        .expect("handle");
    assert!(e.thumb.is_some() && e.file.is_some());
    assert_eq!(
        blob_files(&blob_dir).len(),
        strict_blobs + 3,
        "jpeg + thumbnail + document"
    );
    let draft = match env.handle.query(Query::Conversation(conv)).await {
        Ok(QueryData::Conversation(Some(d))) => d.draft,
        other => panic!("unexpected {other:?}"),
    };
    assert!(draft.is_none(), "a sent caption clears the draft");
    env.stop().await;
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Windows of `plain` (from its middle) that must appear nowhere on disk.
fn needles(plain: &[u8]) -> Vec<Vec<u8>> {
    let n = plain.len();
    [n / 4, n / 2, 3 * n / 4]
        .into_iter()
        .filter(|o| o + 24 <= n)
        .map(|o| plain[o..o + 24].to_vec())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plaintext_scan_of_the_profile_finds_no_attachment_bytes() {
    let script = incoming_script();
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    settle_incoming(&env).await;
    let uuid = env.account(account).await.expect("row").uuid;
    let conv = conv_of(&env);
    // Outgoing too, from a directory outside the profile.
    let src = tempfile::tempdir().expect("sources");
    let jpeg = sample_image(700, 500, 77, SampleFormat::Jpeg).expect("jpeg");
    let doc = random_bytes(6000, 5);
    std::fs::write(src.path().join("a.jpg"), &jpeg).expect("write");
    std::fs::write(src.path().join("b.bin"), &doc).expect("write");
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv,
            paths: vec![src.path().join("a.jpg"), src.path().join("b.bin")].into(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.transmits(uuid)
                .iter()
                .any(|t| matches!(t.outcome, kourier_backend_api::TransmitOutcome::Sent { .. }))
        }),
    )
    .await
    .expect("transmitted");
    // A playback file that is opened and closed again.
    let r = rows(&env);
    let photo = r[0].0;
    env.handle
        .send(UiCommand::OpenMedia { attachment: photo })
        .expect("send");
    let opened = env
        .event("media opened", |e| match e {
            UiEvent::MediaOpened { path, .. } => Some(path.clone()),
            _ => None,
        })
        .await;
    env.handle
        .send(UiCommand::CloseMedia { attachment: photo })
        .expect("send");
    eventually("closed", WAIT, || {
        let p = opened.clone();
        async move { !p.exists() }
    })
    .await;
    // Every plaintext the flows handled: downloads, thumbnails, outgoing files.
    let mut plains: Vec<Vec<u8>> = vec![jpeg, doc];
    for p in [
        Pointer::from_blob(&synthetic_image(900, 600, 11).remote.expect("r")).expect("p"),
        Pointer::Bytes { len: 3000, seed: 5 },
    ] {
        plains.push(p.plaintext().expect("bytes").expect("ok"));
    }
    let handles = env.handle.media();
    let conv_rows = env.timeline(conv).await;
    for a in conv_rows.iter().flat_map(attachments_of) {
        if let Some(e) = a.id.and_then(|id| handles.get(id))
            && let Some(t) = &e.thumb
        {
            plains.push(handles.read(t).expect("thumb").to_vec());
        }
    }
    assert!(plains.len() >= 6, "{}", plains.len());
    env.stop().await;

    let mut files = Vec::new();
    walk(env.dir.path(), &mut files);
    assert!(files.len() >= 8, "{files:?}");
    // The scan works: the sources outside the profile do contain them.
    let source = std::fs::read(src.path().join("a.jpg")).expect("source");
    assert!(
        needles(&plains[0])
            .iter()
            .all(|n| source.windows(n.len()).any(|w| w == n.as_slice()))
    );
    let needles: Vec<Vec<u8>> = plains.iter().flat_map(|p| needles(p)).collect();
    for f in &files {
        let bytes = std::fs::read(f).expect("read");
        for n in &needles {
            assert!(
                !bytes.windows(n.len()).any(|w| w == n.as_slice()),
                "plaintext attachment bytes in {}",
                f.display()
            );
        }
    }
}

#[test]
fn pending_command_and_attachment_ids_never_repeat_after_the_table_empties() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([1; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let account = h.account().0;
    let c = h.connection();
    let insert = || -> i64 {
        c.execute(
            "INSERT INTO pending_commands(account_id, command, created_at) VALUES (?1, '{}', 0)",
            [account],
        )
        .expect("insert");
        c.last_insert_rowid()
    };
    assert_eq!((insert(), insert()), (1, 2));
    c.execute("DELETE FROM pending_commands", [])
        .expect("delete");
    assert_eq!(insert(), 3, "a CmdId is never handed out twice");

    // An attachment id of a deleted message never comes back either.
    let file = |n: i64| {
        let mut a = Attachment {
            kind: AttachmentKind::File,
            mime: "text/plain".into(),
            size: 1,
            ..Attachment::default()
        };
        a.file_name = Some(format!("{n}.txt"));
        a
    };
    let ids = |h: &IngestHarness| -> Vec<i64> {
        let mut s = h
            .connection()
            .prepare("SELECT id FROM attachments ORDER BY id")
            .expect("prepare");
        s.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("ids")
    };
    h.apply(vec![media(T0 + 10, vec![file(1), file(2)], None)])
        .expect("apply");
    assert_eq!(ids(&h), [1, 2]);
    h.apply(vec![BackendEvent::Deleted {
        target: kourier_fake::scripts::target(
            Some(&d_alice()),
            Some(&alice()),
            &(T0 + 10).to_string(),
        ),
        by: Some(alice()),
        for_everyone: true,
        at: T0 + 20,
    }])
    .expect("delete");
    assert!(ids(&h).is_empty(), "the tombstone dropped its attachments");
    h.apply(vec![media(T0 + 30, vec![file(3)], None)])
        .expect("apply");
    assert_eq!(ids(&h), [3]);
}

/// A `core.db` of the previous release (schema 1) keeps its pending commands and attachments,
/// with their ids, and continues above them.
#[test]
fn migration_2_keeps_rows_and_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([2; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v1 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..1]);
    kourier_db::migrate(&mut conn, &v1).expect("v1");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'note', 'Direct');
         INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (1, 1, 1, 0, '', '1', 'OwnThisDevice', 1, 1, 1, 'Text', '{}');
         INSERT INTO attachments(id, message_id, idx, kind, state) VALUES (7, 1, 0, 'File', '\"Remote\"');
         INSERT INTO pending_commands(id, account_id, command, created_at) VALUES (5, 1, '{}', 0);",
    )
    .expect("rows");
    drop(conn);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v2 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..2]);
    let out = kourier_db::migrate(&mut conn, &v2).expect("migrate");
    assert_eq!((out.from, out.to), (1, 2));
    let (att, cmd): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT id FROM attachments), (SELECT id FROM pending_commands)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("rows kept");
    assert_eq!((att, cmd), (7, 5));
    conn.execute("DELETE FROM attachments", []).expect("delete");
    conn.execute(
        "INSERT INTO attachments(message_id, idx, kind, state) VALUES (1, 0, 'File', '\"Remote\"')",
        [],
    )
    .expect("insert");
    assert_eq!(conn.last_insert_rowid(), 8);
    conn.execute(
        "INSERT INTO pending_commands(account_id, command, created_at) VALUES (1, '{}', 0)",
        [],
    )
    .expect("insert");
    assert_eq!(conn.last_insert_rowid(), 6);
    let downloads: i64 = conn
        .query_row("SELECT count(*) FROM attachment_downloads", [], |r| {
            r.get(0)
        })
        .expect("new table");
    assert_eq!(downloads, 0);
}

/// What the image provider gets for one photo, side by side: the downloaded file, the core's
/// thumbnail and the blurhash placeholder (scaled up like the UI does).
fn contact_sheet(full: &[u8], thumb: &[u8], blur_rgba: &[u8]) -> image::RgbaImage {
    let full = image::load_from_memory(full).expect("full").to_rgba8();
    let thumb = image::load_from_memory(thumb).expect("thumb").to_rgba8();
    let blur = image::RgbaImage::from_raw(64, 43, blur_rgba.to_vec()).expect("blur");
    let blur = image::imageops::resize(
        &blur,
        thumb.width(),
        thumb.height(),
        image::imageops::FilterType::Triangle,
    );
    let gap = 24;
    let (w, h) = (
        full.width() + thumb.width() * 2 + gap * 4,
        full.height() + gap * 2,
    );
    let mut sheet = image::RgbaImage::from_pixel(w, h, image::Rgba([239, 240, 241, 255]));
    let mut x = i64::from(gap);
    for img in [&full, &thumb, &blur] {
        image::imageops::overlay(&mut sheet, img, x, i64::from(gap));
        x += i64::from(img.width() + gap);
    }
    sheet
}

/// UI fixture mode (`kourier --fixture`) gets real downloaded photos with thumbnails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_photos_are_downloaded_and_thumbnailed() {
    use kourier_fake::fixture::{FixtureSpec, fixture};
    let spec = FixtureSpec {
        contacts: 5,
        groups: 0,
        messages_per_conversation: 8,
        ..FixtureSpec::default()
    };
    let mut env = Env::new(Flavor::SignalLike, fixture(&spec).expect("fixture")).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    wait_rows(&env, "three fixture photos", |r| {
        r.len() == 3
            && r.iter()
                .all(|a| matches!(a, (_, AttachmentState::Local, true, true, true)))
    })
    .await;
    env.stop().await;
}
