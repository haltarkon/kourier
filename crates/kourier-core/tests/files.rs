//! Sending many files in one go (§1.1 goal 2, §3.8, §4.8): a selection of any size and any mix
//! of kinds becomes the messages the network's packing rule needs, in the user's order, each
//! with its own network id and outbox row, the caption and the quote on the first; a file the
//! send cannot take is named and nothing is queued; the composer's preview is the plan the send
//! then follows.
//!
//! The fake network packs like this (`kourier_fake::scripts::file_packing`): pictures, GIFs and
//! videos share a message, three at most; every other file travels alone.

mod support;

use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendEvent, OutgoingKind, TransmitOutcome};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ActionSubject, FilePlanQuery, FileRefusal, FilesPreview, Progress, Query, QueryData,
    RefusedFile, RequestId, UiCommand, UiEvent,
};
use kourier_fake::scripts::{ALBUM_MAX, signal_features};
use kourier_fake::{Flavor, Script, TransmitRecord};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountCaps, AttachmentKind, Body, ConvKey, ConversationId, ConversationPatch, FileAlbum,
    FileLimits, FilePacking, MessageId, OutgoingState, Text,
};
use support::send::{ME, ME_PNI, d_alice, group, server_log, setup, uid};
use support::{Env, WAIT, add_linked, eventually};

const SEND_LIMIT: u64 = 1 << 20;

/// A chat where a document takes no caption and videos cannot be sent.
fn plain_chat() -> ConvKey {
    ConvKey::Direct(uid("aci:00000000-0000-4000-8000-00000000c0de"))
}

/// A chat on a network whose messages with files cannot quote (unlike Signal).
fn quoteless_chat() -> ConvKey {
    ConvKey::Direct(uid("aci:00000000-0000-4000-8000-0000000000f0"))
}

/// A chat whose messages carry 40 bytes of text.
fn terse_chat() -> ConvKey {
    ConvKey::Direct(uid("aci:00000000-0000-4000-8000-00000000007e"))
}

/// A chat on a network that states its albums by format, as Signal's clients need it:
/// pictures and GIFs share a message, HEIC and HEIF pictures do not, and of the videos only
/// MP4 clips do.
fn formats_chat() -> ConvKey {
    ConvKey::Direct(uid("aci:00000000-0000-4000-8000-0000000f02a7"))
}

fn script(name: &str) -> Script {
    let mut first = setup();
    first.push(BackendEvent::AccountCaps(AccountCaps {
        max_attachment_bytes: Some(SEND_LIMIT),
        ..Default::default()
    }));
    let mut plain = ConversationPatch::new(plain_chat());
    let mut features = signal_features(false);
    features.files.insert(
        AttachmentKind::File,
        FileLimits {
            max_size: 0,
            caption: false,
        },
    );
    features.files.remove(&AttachmentKind::Video);
    plain.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(plain));
    let mut quoteless = ConversationPatch::new(quoteless_chat());
    let mut features = signal_features(false);
    features.file_packing.reply_needs_text = true;
    quoteless.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(quoteless));
    let mut terse = ConversationPatch::new(terse_chat());
    let mut features = signal_features(false);
    features.max_text_bytes = Some(40);
    terse.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(terse));
    let mut formats = ConversationPatch::new(formats_chat());
    let mut features = signal_features(false);
    features.file_packing = FilePacking {
        albums: vec![FileAlbum {
            kinds: [AttachmentKind::Image, AttachmentKind::Gif].into(),
            types: ["video/mp4".to_owned()].into(),
            except: ["image/heic".to_owned(), "image/heif".to_owned()].into(),
            max: 4,
        }],
        reply_needs_text: false,
    };
    formats.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(formats));
    Script::new(name, vec![uid(ME), uid(ME_PNI)]).deliver(first)
}

struct Files {
    dir: tempfile::TempDir,
}

impl Files {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("sources"),
        }
    }

    /// A small JPEG that differs by `seed`.
    fn picture(&self, name: &str, seed: u64) -> PathBuf {
        let p = self.dir.path().join(name);
        let jpeg = sample_image(48, 36, seed, SampleFormat::Jpeg).expect("jpeg");
        std::fs::write(&p, jpeg).expect("write");
        p
    }

    /// A document whose content names it.
    fn document(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::write(&p, format!("the content of {name}")).expect("write");
        p
    }

    /// A file of the ISO media family whose first bytes state `brand` (`heic`, `isom`,
    /// `qt  `): enough for the core to read its format, whatever the name says.
    fn iso_media(&self, name: &str, brand: &[u8; 4]) -> PathBuf {
        let p = self.dir.path().join(name);
        let mut bytes = vec![0, 0, 0, 24];
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0; 12]);
        bytes.extend_from_slice(name.as_bytes());
        std::fs::write(&p, bytes).expect("write");
        p
    }
}

async fn start(name: &str) -> (Env, kourier_model::AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script(name)).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    let uuid = env.account(account).await.expect("row").uuid;
    (env, uuid)
}

fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    support::send::conv_id(&env.core_db(), key).expect("conversation")
}

fn blob_count(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
            }
        }
    }
    n
}

/// The accepted message transmits into `key`, in the order the fake server took them.
fn sent(env: &Env, uuid: kourier_model::AccountUuid, key: &ConvKey) -> Vec<TransmitRecord> {
    env.control
        .transmits(uuid)
        .into_iter()
        .filter(|t| {
            t.req.conv == *key
                && matches!(t.req.kind, OutgoingKind::Message { .. })
                && matches!(t.outcome, TransmitOutcome::Sent { .. })
        })
        .collect()
}

async fn wait_sent(env: &Env, uuid: kourier_model::AccountUuid, key: &ConvKey, n: usize) {
    let control = env.control.clone();
    let key = key.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.transmits(uuid)
                .iter()
                .filter(|t| {
                    t.req.conv == key
                        && matches!(t.req.kind, OutgoingKind::Message { .. })
                        && matches!(t.outcome, TransmitOutcome::Sent { .. })
                })
                .count()
                >= n
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("{n} messages sent"));
}

/// What a transmitted message carried: file names, caption, whether it quoted.
fn carried(t: &TransmitRecord) -> (Vec<String>, Option<String>, bool) {
    let OutgoingKind::Message { body, reply_to, .. } = &t.req.kind else {
        panic!("not a message");
    };
    match body {
        Body::Media {
            attachments,
            caption,
        } => (
            attachments
                .iter()
                .map(|a| a.file_name.clone().unwrap_or_default())
                .collect(),
            caption.as_ref().map(|c| c.body.clone()),
            reply_to.is_some(),
        ),
        Body::Text(t) => (Vec::new(), Some(t.body.clone()), reply_to.is_some()),
        other => panic!("unexpected body {other:?}"),
    }
}

async fn preview(
    env: &Env,
    conv: ConversationId,
    paths: &[PathBuf],
    caption: bool,
    reply: bool,
) -> FilesPreview {
    preview_of(
        env,
        FilePlanQuery {
            as_file: Vec::new(),
            caption_text: None,
            conv,
            paths: paths.to_vec().into(),
            caption,
            reply,
        },
    )
    .await
}

async fn preview_of(env: &Env, query: FilePlanQuery) -> FilesPreview {
    match env
        .handle
        .query(Query::FilePlan(query))
        .await
        .expect("preview")
    {
        QueryData::FilePlan(p) => p,
        other => panic!("unexpected {other:?}"),
    }
}

/// What the core said about the send `request` until it ended: the import progress it
/// reported, and the messages of `FilesStored` (`Err`: the refusal's request, positions and
/// `as_file` instead).
type Ended = Result<Vec<MessageId>, (RequestId, Vec<RefusedFile>, Vec<bool>)>;

async fn followed(env: &mut Env, request: RequestId) -> (Vec<(u32, u32, u64, u64)>, Ended) {
    let mut progress = Vec::new();
    let ended = env
        .event("the end of the send", |e| match e {
            UiEvent::Progress(Progress::Import {
                request: r,
                files_done,
                files,
                bytes_done,
                bytes,
                ..
            }) if *r == request => {
                progress.push((*files_done, *files, *bytes_done, *bytes));
                None
            }
            UiEvent::FilesStored {
                request: r,
                messages,
                ..
            } if *r == request => Some(Ok(messages.clone())),
            UiEvent::ActionFailed {
                action: "SendFiles",
                subject: ActionSubject::Files(f),
                ..
            } => Some(Err((f.request, f.refused.clone(), f.as_file.clone()))),
            _ => None,
        })
        .await;
    (progress, ended)
}

async fn refusal(env: &mut Env) -> (String, Vec<RefusedFile>, usize) {
    env.event("refused send", |e| match e {
        UiEvent::ActionFailed {
            action: "SendFiles",
            error,
            subject: ActionSubject::Files(f),
            ..
        } => Some((error.to_string(), f.refused.clone(), f.paths.expose().len())),
        _ => None,
    })
    .await
}

fn names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.file_name().expect("name").to_string_lossy().into_owned())
        .collect()
}

fn own_ids(env: &Env, conv: ConversationId) -> Vec<MessageId> {
    support::send::own_messages(&env.core_db(), conv)
        .into_iter()
        .map(|m| m.id)
        .collect()
}

/// Seven files of mixed kinds, a caption and a quote: five messages, in the user's order, the
/// caption and the quote on the first, every file in exactly one of them, each message with
/// its own id and outbox row, each recipient served once per message; the preview said so
/// before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_files_become_the_messages_the_network_can_show() {
    let (mut env, uuid) = start("files-mixed").await;
    let g = conv(&env, &group());
    let f = Files::new();
    let paths = vec![
        f.picture("1.jpg", 1),
        f.picture("2.jpg", 2),
        f.picture("3.jpg", 3),
        f.picture("4.jpg", 4),
        f.document("5.pdf"),
        f.picture("6.jpg", 6),
        f.document("7.txt"),
    ];
    // The message the send quotes: alice's, in her chat, so the group send has none to quote;
    // an own text in the group serves.
    env.handle
        .send(UiCommand::SendText {
            conv: g,
            text: Text::plain("which ones?"),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &group(), 1).await;
    let quoted = own_ids(&env, g)[0];

    let plan = preview(&env, g, &paths, true, true).await;
    assert!(plan.refused.is_empty());
    assert_eq!(
        plan.kinds,
        [
            Some(AttachmentKind::Image),
            Some(AttachmentKind::Image),
            Some(AttachmentKind::Image),
            Some(AttachmentKind::Image),
            Some(AttachmentKind::File),
            Some(AttachmentKind::Image),
            Some(AttachmentKind::File),
        ]
    );
    assert_eq!(
        plan.plan.messages,
        [vec![0, 1, 2], vec![3], vec![4], vec![5], vec![6]],
        "seven files, five messages"
    );
    assert_eq!(plan.plan.message_count(), 5);
    assert!(!plan.plan.lead_text);
    assert!(own_ids(&env, g).len() == 1, "a preview queues nothing");

    env.handle
        .send(UiCommand::SaveDraft {
            conv: g,
            draft: Some(Text::plain("draft")),
        })
        .expect("send");
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: g,
            paths: paths.clone().into(),
            caption: Some(Text::plain("from the trip")),
            reply_to: Some(quoted),
        })
        .expect("send");
    wait_sent(&env, uuid, &group(), 6).await;
    let out = sent(&env, uuid, &group());
    let got: Vec<_> = out[1..].iter().map(carried).collect();
    let all = names(&paths);
    assert_eq!(
        got,
        [
            (all[0..3].to_vec(), Some("from the trip".to_owned()), true),
            (all[3..4].to_vec(), None, false),
            (all[4..5].to_vec(), None, false),
            (all[5..6].to_vec(), None, false),
            (all[6..7].to_vec(), None, false),
        ],
        "the user's order, the caption and the quote on the first message only"
    );
    // Every message has its own id, later ones later ids.
    let ids: Vec<(String, i64)> = out
        .iter()
        .map(|t| (t.req.local.to_string(), t.req.sent_at))
        .collect();
    assert!(ids.windows(2).all(|w| w[0].1 < w[1].1), "{ids:?}");
    let rows = own_ids(&env, g);
    assert_eq!(rows.len(), 6);
    let targets: Vec<MessageId> = out.iter().map(|t| t.message).collect();
    assert_eq!(targets, rows, "one outbox row per message, sent in order");
    // The timeline shows them in the same order, all sent, and nothing waits.
    eventually("everything is sent", WAIT, || {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 0 }
    })
    .await;
    let timeline = env.timeline(g).await;
    let shown: Vec<Vec<String>> = timeline
        .iter()
        .filter_map(|r| match &r.body {
            Body::Media { attachments, .. } => Some(
                attachments
                    .iter()
                    .map(|a| a.file_name.clone().unwrap_or_default())
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            all[0..3].to_vec(),
            all[3..4].to_vec(),
            all[4..5].to_vec(),
            all[5..6].to_vec(),
            all[6..7].to_vec()
        ]
    );
    assert!(
        timeline
            .iter()
            .all(|r| matches!(r.out_state, Some(OutgoingState::Sent { .. })))
    );
    let first = timeline
        .iter()
        .find(|r| matches!(r.body, Body::Media { .. }))
        .expect("first media row");
    assert_eq!(
        first.reply.as_ref().and_then(|r| r.target),
        Some(quoted),
        "the quote is on the first message"
    );
    let draft = match env.handle.query(Query::Conversation(g)).await {
        Ok(QueryData::Conversation(Some(d))) => d.draft,
        other => panic!("unexpected {other:?}"),
    };
    assert!(draft.is_none(), "a sent caption clears the draft");
    // Each member got each message exactly once (the fake server's log, read once the actor
    // has closed its database).
    env.stop().await;
    let key = env.vault.unwrap(uuid).await.expect("backend.db key");
    let log = server_log(&env.vault.backend_db(uuid), &key);
    for (local, _) in &ids {
        let mut to: Vec<&str> = log
            .iter()
            .filter(|d| d.local == *local)
            .map(|d| d.recipient.as_str())
            .collect();
        to.sort_unstable();
        let mut both = [support::send::ALICE, support::send::BOB];
        both.sort_unstable();
        assert_eq!(to, both, "{local}");
    }
}

/// Signal Desktop's composer stops at 32 attachments. Forty pictures are forty pictures here:
/// fourteen albums, every picture in one of them, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_files_than_desktop_takes_are_all_sent() {
    let (mut env, uuid) = start("files-many").await;
    let a = conv(&env, &d_alice());
    let f = Files::new();
    let paths: Vec<PathBuf> = (0..40)
        .map(|i| f.picture(&format!("p{i:02}.jpg"), i + 1))
        .collect();
    let album = usize::from(ALBUM_MAX);
    let want = 40usize.div_ceil(album);
    assert_eq!(
        preview(&env, a, &paths, false, false)
            .await
            .plan
            .message_count(),
        want
    );
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: a,
            paths: paths.clone().into(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &d_alice(), want).await;
    let out = sent(&env, uuid, &d_alice());
    assert_eq!(out.len(), want);
    assert!(
        out.iter().all(|t| carried(t).0.len() <= album),
        "no message carries more than every client shows"
    );
    let flat: Vec<String> = out.iter().flat_map(|t| carried(t).0).collect();
    assert_eq!(flat, names(&paths), "all forty, in the user's order");
    assert_eq!(own_ids(&env, a).len(), want);
    env.stop().await;
}

/// A file the send cannot take is named with its position and its reason, every such file at
/// once; nothing is queued and no blob stays. The rest is sendable: the same command without
/// the named files goes through. The preview names the same files and plans the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_files_are_named_and_the_rest_can_be_sent() {
    let (mut env, uuid) = start("files-refused").await;
    let a = conv(&env, &d_alice());
    let plain = conv(&env, &plain_chat());
    let blobs = env.vault.blob_dir(uuid);
    let f = Files::new();
    let huge = f.dir.path().join("huge.bin");
    std::fs::write(
        &huge,
        vec![0u8; usize::try_from(SEND_LIMIT).expect("fits") + 1],
    )
    .expect("write");
    let empty = f.dir.path().join("empty.txt");
    std::fs::write(&empty, b"").expect("write");
    let paths = vec![
        f.picture("ok1.jpg", 1),
        f.dir.path().join("gone.jpg"),
        huge,
        f.document("ok2.pdf"),
        empty,
        f.dir.path().to_owned(),
        f.picture("ok3.jpg", 3),
    ];
    let want = vec![
        RefusedFile {
            index: 1,
            reason: FileRefusal::Unreadable,
        },
        RefusedFile {
            index: 2,
            reason: FileRefusal::TooLarge {
                max: SEND_LIMIT,
                chat: false,
            },
        },
        RefusedFile {
            index: 5,
            reason: FileRefusal::NotAFile,
        },
    ];
    let plan = preview(&env, a, &paths, true, false).await;
    assert_eq!(plan.refused, want, "an empty file is not refused");
    assert_eq!(
        plan.plan.messages,
        [vec![0], vec![3], vec![4], vec![6]],
        "the rest, planned in the positions of the selection"
    );

    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: a,
            paths: paths.clone().into(),
            caption: Some(Text::plain("some of these")),
            reply_to: None,
        })
        .expect("send");
    let (error, refused, handed_back) = refusal(&mut env).await;
    assert_eq!(refused, want, "every refused file, each with its reason");
    assert_eq!(handed_back, paths.len(), "the whole request comes back");
    assert!(error.contains("could not be read"), "{error}");
    assert!(
        !error.contains("gone.jpg") && !error.contains("huge"),
        "the error names no file: {error}"
    );
    assert!(own_ids(&env, a).is_empty(), "nothing was queued");
    assert_eq!(blob_count(&blobs), 0, "nothing was copied");

    // The UI's choice: send the rest.
    let rest: Vec<PathBuf> = paths
        .iter()
        .enumerate()
        .filter(|(i, _)| !refused.iter().any(|r| r.index == *i))
        .map(|(_, p)| p.clone())
        .collect();
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: a,
            paths: rest.clone().into(),
            caption: Some(Text::plain("some of these")),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &d_alice(), 4).await;
    let records = sent(&env, uuid, &d_alice());
    let got: Vec<_> = records.iter().map(carried).collect();
    let rest = names(&rest);
    assert_eq!(
        got,
        [
            (rest[0..1].to_vec(), Some("some of these".to_owned()), false),
            (rest[1..2].to_vec(), None, false),
            (rest[2..3].to_vec(), None, false),
            (rest[3..4].to_vec(), None, false),
        ]
    );
    // The empty file went out as a file of no bytes, with its blob for the backend to read.
    assert_eq!(rest[2], "empty.txt");
    let OutgoingKind::Message {
        body: Body::Media { attachments, .. },
        ..
    } = &records[2].req.kind
    else {
        panic!("not a media message");
    };
    assert_eq!(attachments[0].size, 0);
    assert!(
        attachments[0].local.is_some(),
        "a blob, though an empty one"
    );

    // A kind the chat does not take is refused by kind, read from the content.
    let clip = f.dir.path().join("clip.bin");
    std::fs::write(&clip, b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom").expect("write");
    let sniffed = preview(&env, plain, std::slice::from_ref(&clip), false, false).await;
    if sniffed.kinds == [Some(AttachmentKind::Video)] {
        let before = blob_count(&blobs);
        env.handle
            .send(UiCommand::SendFiles {
                request: Default::default(),
                as_file: Vec::new(),
                conv: plain,
                paths: vec![f.picture("fine.jpg", 9), clip].into(),
                caption: None,
                reply_to: None,
            })
            .expect("send");
        let (error, refused, _) = refusal(&mut env).await;
        assert_eq!(
            refused,
            [RefusedFile {
                index: 1,
                reason: FileRefusal::KindNotAccepted {
                    kind: AttachmentKind::Video
                }
            }]
        );
        assert!(error.contains("does not take this kind of file"), "{error}");
        assert_eq!(blob_count(&blobs), before);
        assert!(own_ids(&env, plain).is_empty());
    } else {
        panic!("the clip was not read as a video: {:?}", sniffed.kinds);
    }
    env.stop().await;
}

/// The caption goes on the first message with files; where that message can carry none, it
/// goes first as a text of its own and takes the quote with it. A quote that files cannot
/// carry and no caption can carry either is the one thing refused, and it names no file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caption_the_first_file_cannot_carry_goes_first_as_a_text() {
    let (mut env, uuid) = start("files-caption").await;
    let plain = conv(&env, &plain_chat());
    let f = Files::new();
    let paths = vec![f.document("notes.txt"), f.picture("view.jpg", 5)];
    env.handle
        .send(UiCommand::SendText {
            conv: plain,
            text: Text::plain("send me the notes"),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &plain_chat(), 1).await;
    let quoted = own_ids(&env, plain)[0];

    let plan = preview(&env, plain, &paths, true, true).await;
    assert!(plan.plan.lead_text && !plan.plan.reply_unsendable);
    assert_eq!(plan.plan.message_count(), 3, "two files, three messages");
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: plain,
            paths: paths.clone().into(),
            caption: Some(Text::plain("here they are")),
            reply_to: Some(quoted),
        })
        .expect("send");
    wait_sent(&env, uuid, &plain_chat(), 4).await;
    let got: Vec<_> = sent(&env, uuid, &plain_chat())[1..]
        .iter()
        .map(carried)
        .collect();
    assert_eq!(
        got,
        [
            (Vec::new(), Some("here they are".to_owned()), true),
            (vec!["notes.txt".to_owned()], None, false),
            (vec!["view.jpg".to_owned()], None, false),
        ]
    );

    // A picture first: the caption stays with it.
    let reversed: Vec<PathBuf> = paths.iter().rev().cloned().collect();
    let plan = preview(&env, plain, &reversed, true, false).await;
    assert!(!plan.plan.lead_text);
    assert_eq!(plan.plan.message_count(), 2);

    // A network whose files cannot quote: with a caption the quote rides on it (the caption
    // goes first as a text), without one the send is refused for the quote alone.
    let q = conv(&env, &quoteless_chat());
    env.handle
        .send(UiCommand::SendText {
            conv: q,
            text: Text::plain("and the view?"),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &quoteless_chat(), 1).await;
    let quoted = own_ids(&env, q)[0];
    let plan = preview(&env, q, &reversed, false, true).await;
    assert!(plan.plan.reply_unsendable);
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: q,
            paths: reversed.clone().into(),
            caption: None,
            reply_to: Some(quoted),
        })
        .expect("send");
    let (error, refused, _) = refusal(&mut env).await;
    assert!(error.contains("cannot quote"), "{error}");
    assert!(refused.is_empty(), "no file is to blame");
    assert_eq!(own_ids(&env, q).len(), 1, "nothing was queued");
    assert_eq!(
        blob_count(&env.vault.blob_dir(uuid)),
        stored_files(&env),
        "and the imported copies are gone again"
    );
    let plan = preview(&env, q, &reversed, true, true).await;
    assert!(plan.plan.lead_text && !plan.plan.reply_unsendable);
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: q,
            paths: reversed.clone().into(),
            caption: Some(Text::plain("both")),
            reply_to: Some(quoted),
        })
        .expect("send");
    wait_sent(&env, uuid, &quoteless_chat(), 4).await;
    let got: Vec<_> = sent(&env, uuid, &quoteless_chat())[1..]
        .iter()
        .map(carried)
        .collect();
    assert_eq!(
        got,
        [
            (Vec::new(), Some("both".to_owned()), true),
            (vec!["view.jpg".to_owned()], None, false),
            (vec!["notes.txt".to_owned()], None, false),
        ]
    );
    env.stop().await;
}

/// A caption is never refused for its length. One that is longer than a message of the chat
/// carries goes first, as the texts it splits into (the quote on the first of them), and the
/// files follow without a caption, all stored together; before, the first file message went to
/// the backend with the whole caption and failed there. A caption that fits stays on the files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caption_too_long_for_one_message_goes_first_as_texts() {
    let (mut env, uuid) = start("files-long-caption").await;
    let terse = conv(&env, &terse_chat());
    env.handle
        .send(UiCommand::SendText {
            conv: terse,
            text: Text::plain("which ones?"),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &terse_chat(), 1).await;
    let quoted = own_ids(&env, terse)[0];
    let f = Files::new();
    let paths = vec![
        f.picture("a.jpg", 1),
        f.picture("b.jpg", 2),
        f.document("list.txt"),
    ];
    let caption = Text::plain(
        "these two from the walk on the hill, and the list of the others I still have to sort",
    );
    let pieces: Vec<String> = caption.split(40).into_iter().map(|t| t.body).collect();
    assert!(pieces.len() >= 2 && pieces.iter().all(|p| p.len() <= 40));
    assert_eq!(pieces.concat(), caption.body);

    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: terse,
            paths: paths.clone().into(),
            caption: Some(caption.clone()),
            reply_to: Some(quoted),
        })
        .expect("send");
    let all = 1 + pieces.len() + 2;
    wait_sent(&env, uuid, &terse_chat(), all).await;
    let got: Vec<_> = sent(&env, uuid, &terse_chat())[1..]
        .iter()
        .map(carried)
        .collect();
    let mut want: Vec<(Vec<String>, Option<String>, bool)> = pieces
        .iter()
        .enumerate()
        .map(|(i, p)| (Vec::new(), Some(p.clone()), i == 0))
        .collect();
    want.push((vec!["a.jpg".to_owned(), "b.jpg".to_owned()], None, false));
    want.push((vec!["list.txt".to_owned()], None, false));
    assert_eq!(got, want, "the texts first, then the files, the quote once");
    let own = support::send::own_messages(&env.core_db(), terse);
    assert_eq!(own.len(), all);
    assert!(own.windows(2).all(|w| w[0].sent_at < w[1].sent_at));
    // (The fake server has the last message before the core has recorded its answer.)
    eventually("everything is sent", WAIT, || {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 0 }
    })
    .await;

    // A caption that fits rides on the first message with files, as everywhere.
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv: terse,
            paths: paths.clone().into(),
            caption: Some(Text::plain("the same again")),
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &terse_chat(), all + 2).await;
    let got: Vec<_> = sent(&env, uuid, &terse_chat())[all..]
        .iter()
        .map(carried)
        .collect();
    assert_eq!(
        got,
        [
            (
                vec!["a.jpg".to_owned(), "b.jpg".to_owned()],
                Some("the same again".to_owned()),
                false
            ),
            (vec!["list.txt".to_owned()], None, false),
        ]
    );
    while let Ok(e) = env.events.try_recv() {
        assert!(!matches!(e, UiEvent::ActionFailed { .. }), "{e:?}");
    }
    env.stop().await;
}

/// The blobs the stored messages account for: what is on disk when nothing of a refused send
/// stayed behind.
fn stored_files(env: &Env) -> usize {
    env.peek(|c| {
        c.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .map(|n| usize::try_from(n).unwrap_or(0))
            .expect("files")
    })
}

// ---------- phase I: packing by format, traceable sends, send as file ----------

/// A network states its albums by format, and the core packs by what it reads from each
/// file: HEIC pictures travel alone although they are pictures, MP4 clips share an album
/// although they are videos, other videos do not. The preview and the send agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_pack_by_the_format_the_network_states() {
    let (mut env, uuid) = start("files-formats").await;
    let c = conv(&env, &formats_chat());
    let f = Files::new();
    let paths = vec![
        f.picture("1.jpg", 1),
        f.iso_media("2.mp4", b"isom"),
        // The name lies: the content is HEIC.
        f.iso_media("3.jpg", b"heic"),
        f.picture("4.jpg", 4),
        f.iso_media("5.mov", b"qt  "),
        f.iso_media("6.mp4", b"mp42"),
        f.picture("7.jpg", 7),
        f.iso_media("8.heif", b"mif1"),
    ];
    let plan = preview(&env, c, &paths, false, false).await;
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    use AttachmentKind::{Image, Video};
    assert_eq!(
        plan.kinds,
        [Image, Video, Image, Image, Video, Video, Image, Image].map(Some)
    );
    let want: [&[usize]; 6] = [&[0, 1], &[2], &[3], &[4], &[5, 6], &[7]];
    assert_eq!(plan.plan.messages, want, "by format, not by kind");

    env.handle
        .send(UiCommand::SendFiles {
            conv: c,
            request: RequestId(1),
            paths: paths.clone().into(),
            as_file: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    wait_sent(&env, uuid, &formats_chat(), want.len()).await;
    let all = names(&paths);
    let got: Vec<Vec<String>> = sent(&env, uuid, &formats_chat())
        .iter()
        .map(|t| carried(t).0)
        .collect();
    let expected: Vec<Vec<String>> = want
        .iter()
        .map(|m| m.iter().map(|i| all[*i].clone()).collect())
        .collect();
    assert_eq!(got, expected, "the send follows the plan");
    // The backend is told each file's format as the core read it.
    let mimes: Vec<String> = sent(&env, uuid, &formats_chat())
        .iter()
        .flat_map(|t| match &t.req.kind {
            OutgoingKind::Message {
                body: Body::Media { attachments, .. },
                ..
            } => attachments.iter().map(|a| a.mime.clone()).collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(
        mimes,
        [
            "image/jpeg",
            "video/mp4",
            "image/heic",
            "image/jpeg",
            "video/quicktime",
            "video/mp4",
            "image/jpeg",
            "image/heif"
        ]
    );
    env.stop().await;
}

/// The plan counts every message of the send: a caption longer than one message of the chat
/// carries goes first as several texts, and the composer is told how many before it sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plan_counts_the_texts_a_long_caption_becomes() {
    let (mut env, uuid) = start("files-caption-count").await;
    let c = conv(&env, &terse_chat());
    let f = Files::new();
    let paths = vec![f.picture("1.jpg", 1), f.picture("2.jpg", 2)];
    let long = Text::plain(
        "this caption is a good deal longer than the forty bytes one message of this chat takes",
    );
    let pieces = long.split(40).len();
    assert!(pieces >= 3, "{pieces}");
    let query = |caption_text: Option<Text>, caption: bool| FilePlanQuery {
        conv: c,
        paths: paths.clone().into(),
        as_file: Vec::new(),
        caption,
        caption_text,
        reply: false,
    };
    // Without the text the plan can only count the caption once (it does not know it is long).
    let rough = preview_of(&env, query(None, true)).await;
    assert_eq!(
        (rough.plan.lead_text, rough.plan.message_count()),
        (false, 1)
    );
    // With it the count is exact.
    let exact = preview_of(&env, query(Some(long.clone()), false)).await;
    assert!(exact.plan.lead_text);
    assert_eq!(exact.plan.caption_messages, pieces);
    assert_eq!(exact.plan.message_count(), pieces + 1);
    // A short caption rides on the pictures; a blank one is none.
    let short = preview_of(&env, query(Some(Text::plain("short")), false)).await;
    assert_eq!(
        (short.plan.caption_messages, short.plan.message_count()),
        (0, 1)
    );
    let blank = preview_of(&env, query(Some(Text::plain("  ")), true)).await;
    assert!(!blank.plan.lead_text);

    env.handle
        .send(UiCommand::SendFiles {
            conv: c,
            request: RequestId(7),
            paths: paths.clone().into(),
            as_file: Vec::new(),
            caption: Some(long),
            reply_to: None,
        })
        .expect("send");
    let (_, ended) = followed(&mut env, RequestId(7)).await;
    let messages = ended.expect("stored");
    assert_eq!(
        messages.len(),
        exact.plan.message_count(),
        "the send became as many messages as the plan said"
    );
    wait_sent(&env, uuid, &terse_chat(), pieces + 1).await;
    env.stop().await;
}

/// A send is traceable: the core reports the import under the request's id, says when the
/// messages exist and which they are, and a refusal names its request. Two sends in a row
/// are told apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_of_files_reports_progress_and_the_messages_it_became() {
    let (mut env, uuid) = start("files-traced").await;
    let a = conv(&env, &d_alice());
    let f = Files::new();
    let first = vec![
        f.picture("1.jpg", 1),
        f.picture("2.jpg", 2),
        f.document("3.pdf"),
        f.picture("4.jpg", 4),
    ];
    let second = vec![f.document("5.txt")];
    let total: u64 = first
        .iter()
        .map(|p| std::fs::metadata(p).expect("meta").len())
        .sum();
    for (request, paths) in [(41, &first), (42, &second)] {
        env.handle
            .send(UiCommand::SendFiles {
                conv: a,
                request: RequestId(request),
                paths: paths.clone().into(),
                as_file: Vec::new(),
                caption: None,
                reply_to: None,
            })
            .expect("send");
    }
    let (progress, ended) = followed(&mut env, RequestId(41)).await;
    let stored = ended.expect("stored");
    // Two pictures, a document, a picture: three messages on the fake network.
    assert_eq!(stored.len(), 3);
    assert_eq!(
        stored,
        own_ids(&env, a)[..3],
        "the messages of the send, in sending order"
    );
    assert_eq!(progress.first(), Some(&(0, 4, 0, total)), "{progress:?}");
    assert_eq!(progress.last(), Some(&(4, 4, total, total)), "{progress:?}");
    assert!(
        progress
            .windows(2)
            .all(|w| w[0].0 <= w[1].0 && w[0].2 <= w[1].2),
        "{progress:?}"
    );
    // The rows exist when the event arrives.
    let rows = env.timeline(a).await;
    assert!(stored.iter().all(|id| rows.iter().any(|r| r.id == *id)));
    let (progress, ended) = followed(&mut env, RequestId(42)).await;
    assert_eq!(ended.expect("stored").len(), 1);
    assert_eq!(progress.first().map(|p| (p.0, p.1)), Some((0, 1)));
    wait_sent(&env, uuid, &d_alice(), 4).await;

    // A refused send names its request and hands back what was asked, `as_file` included.
    let missing = f.dir.path().join("missing.bin");
    env.handle
        .send(UiCommand::SendFiles {
            conv: a,
            request: RequestId(43),
            paths: vec![first[0].clone(), missing].into(),
            as_file: vec![true, false],
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let (progress, ended) = followed(&mut env, RequestId(43)).await;
    let (request, refused, as_file) = ended.expect_err("refused");
    assert_eq!(request, RequestId(43));
    assert_eq!(
        refused,
        [RefusedFile {
            index: 1,
            reason: FileRefusal::Unreadable
        }]
    );
    assert_eq!(as_file, [true, false]);
    assert!(progress.is_empty(), "nothing was copied for it");
    assert_eq!(own_ids(&env, a).len(), 4, "and nothing was queued");
    env.stop().await;
}

/// "Send as file": a picture the user wants to arrive as the file it is leaves untouched,
/// byte for byte, as a generic file with its real type and name; it packs like a document
/// (alone), the pictures around it still share albums, and the preview says so before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picture_sent_as_a_file_leaves_untouched_and_alone() {
    let (mut env, uuid) = start("files-as-file").await;
    let a = conv(&env, &d_alice());
    let f = Files::new();
    let paths = vec![
        f.picture("1.jpg", 1),
        f.picture("original.jpg", 2),
        f.picture("3.jpg", 3),
        f.picture("4.jpg", 4),
    ];
    let as_file = vec![false, true];
    let original = std::fs::read(&paths[1]).expect("read");
    let plan = preview_of(
        &env,
        FilePlanQuery {
            conv: a,
            paths: paths.clone().into(),
            as_file: as_file.clone(),
            caption: false,
            caption_text: None,
            reply: false,
        },
    )
    .await;
    assert_eq!(
        plan.kinds,
        [Some(AttachmentKind::Image); 4],
        "what the files hold: the composer keeps offering the choice"
    );
    let want: [&[usize]; 3] = [&[0], &[1], &[2, 3]];
    assert_eq!(plan.plan.messages, want, "the original travels alone");
    // Without the choice the four pictures are two albums.
    assert_eq!(
        preview(&env, a, &paths, false, false).await.plan.messages,
        [vec![0, 1, 2], vec![3]]
    );

    env.handle
        .send(UiCommand::SendFiles {
            conv: a,
            request: RequestId(5),
            paths: paths.clone().into(),
            as_file,
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let (_, ended) = followed(&mut env, RequestId(5)).await;
    let stored = ended.expect("stored");
    assert_eq!(stored.len(), 3);
    wait_sent(&env, uuid, &d_alice(), 3).await;
    let out = sent(&env, uuid, &d_alice());
    let attachments = |t: &TransmitRecord| match &t.req.kind {
        OutgoingKind::Message {
            body: Body::Media { attachments, .. },
            ..
        } => attachments.clone(),
        other => panic!("unexpected {other:?}"),
    };
    let alone = attachments(&out[1]);
    assert_eq!(alone.len(), 1);
    let file = &alone[0];
    assert_eq!(
        (file.kind, file.mime.as_str(), file.file_name.as_deref()),
        (AttachmentKind::File, "image/jpeg", Some("original.jpg")),
        "a generic file for the backend, which never re-encodes one"
    );
    assert!(file.thumb.is_none() && file.dims.is_none() && file.blurhash.is_none());
    assert_eq!(file.size, original.len() as u64);
    // The blob the backend reads is the file the user picked, byte for byte.
    let blob = file.local.as_ref().expect("blob");
    let dir = env.vault.blob_dir(uuid);
    let key = kourier_blob::FileKey::from_bytes(*blob.key.expose());
    assert_eq!(
        *kourier_blob::read_all(&dir.join(&blob.path), &key).expect("read"),
        original
    );
    // The pictures beside it are pictures, with thumbnails, in their album.
    let album = attachments(&out[2]);
    assert_eq!(album.len(), 2);
    assert!(
        album
            .iter()
            .all(|a| a.kind == AttachmentKind::Image && a.thumb.is_some())
    );
    // The timeline shows it as a file.
    let rows = env.timeline(a).await;
    let row = rows.iter().find(|r| r.id == stored[1]).expect("row");
    match &row.body {
        Body::Media { attachments, .. } => {
            assert_eq!(attachments[0].kind, AttachmentKind::File);
            assert_eq!(attachments[0].mime, "image/jpeg");
        }
        other => panic!("unexpected {other:?}"),
    }

    // The chat's rules for generic files apply to it: where a document takes no caption, the
    // caption of an original goes first as a text, while the same picture carries it.
    let plain = conv(&env, &plain_chat());
    let one = |as_file: Vec<bool>| FilePlanQuery {
        conv: plain,
        paths: vec![paths[1].clone()].into(),
        as_file,
        caption: true,
        caption_text: None,
        reply: false,
    };
    assert!(preview_of(&env, one(vec![true])).await.plan.lead_text);
    assert!(!preview_of(&env, one(Vec::new())).await.plan.lead_text);
    env.stop().await;
}
