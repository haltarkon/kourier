//! Export Selected below the UI (architecture §5.8.3): the plan query and the archive command
//! on a core over the fake that plays WhatsApp (the one network whose view-once messages keep
//! an attachment). Synthetic data only.

use crate::support;

use std::io::Read;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{
    ExportDocument, ExportFailure, ExportFile, ExportFileRefusal, ExportHead, ExportOutcome,
    ExportPlan, ExportStamp, ExportVerdict, LeftOut, MessageRow, Progress, Query, QueryData,
    Refusal, RequestId, SaveMode, SaveTarget, UiCommand, UiEvent,
};
use kourier_fake::Script;
use kourier_fake::attachments::{Pointer, synthetic_file, synthetic_image};
use kourier_fake::scripts::{T0, remote, target};
use kourier_model::{
    Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, ChatExport, ContactCard,
    ConversationId, DisappearKind, Expiry, Location, MessageId, NetworkKind, Placeholder,
    QuoteSnapshot, ReplyTo, SystemEvent, Text,
};
use sha2::{Digest, Sha256};
use support::send::{ME, alice, d_alice, d_bob, setup, uid};
use support::{Env, add_linked_as};

fn event(local: i64, body: Body, view_once: bool, timer: bool) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "");
    m.body = body;
    m.view_once = view_once;
    if timer {
        m.expiry = Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: std::time::Duration::from_secs(3600),
            started_at: None,
        });
    }
    BackendEvent::Message(m)
}

fn media(local: i64, a: Attachment, view_once: bool, timer: bool) -> BackendEvent {
    event(
        local,
        Body::Media {
            attachments: vec![a],
            caption: None,
        },
        view_once,
        timer,
    )
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

/// Local ids of the script's messages, in the order of the timeline (`T0 + n`).
const TEXT: i64 = 1;
const PHOTO: i64 = 2;
const VIDEO: i64 = 3;
const FILE: i64 = 4;
const VOICE: i64 = 5;
const STICKER: i64 = 6;
const CONTACT: i64 = 7;
const PLACE: i64 = 8;
const NOTICE: i64 = 9;
const ONCE: i64 = 10;
const TIMED: i64 = 11;
const DELETED: i64 = 12;
/// A reply that quotes the view-once picture, the disappearing picture, a message the core
/// remembers as expired, a message that is hidden later, and one it never had.
const QUOTE_ONCE: i64 = 13;
const QUOTE_TIMED: i64 = 14;
const QUOTE_GONE: i64 = 15;
const ORIGINAL: i64 = 16;
const QUOTE_HIDDEN: i64 = 17;
const QUOTE_UNKNOWN: i64 = 18;
/// A file of several MiB (a cancel must land in the middle of it).
const BIG: i64 = 19;
/// The local id of a message that expired here (not in core.db, remembered).
const EXPIRED: i64 = 900;
/// The local id of a message the core never had.
const NEVER: i64 = 901;
/// Messages in the scene.
const COUNT: usize = 19;
/// The quote snapshot's text: it must never reach a left-out quote's document.
const SNAPSHOT: &str = "the sender's snapshot";
const BIG_SIZE: u64 = 6 << 20;

fn quote(local: i64, original: i64) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "a reply");
    m.reply_to = Some(ReplyTo {
        target: target(Some(&d_alice()), Some(&alice()), &original.to_string()),
        author: Some(alice()),
        snapshot: Some(QuoteSnapshot {
            text: Some(Text::plain(SNAPSHOT)),
            ..QuoteSnapshot::default()
        }),
    });
    BackendEvent::Message(m)
}

fn script(state: Option<ChatExport>) -> Script {
    let mut voice = synthetic_file("v.ogg", "audio/ogg", 2000, 8);
    voice.kind = AttachmentKind::Voice;
    let mut sticker = synthetic_image(128, 128, 9);
    sticker.mime = "image/webp".into();
    let mut s = Script::new("export", vec![uid(ME)])
        .deliver(setup())
        .deliver(vec![
            event(T0 + TEXT, Body::text("hello"), false, false),
            media(T0 + PHOTO, synthetic_image(900, 600, 11), false, false),
            media(T0 + VIDEO, video(), false, false),
            media(
                T0 + FILE,
                synthetic_file("notes.bin", "application/octet-stream", 3000, 5),
                false,
                false,
            ),
            media(T0 + VOICE, voice, false, false),
            event(T0 + STICKER, Body::Sticker(Box::new(sticker)), false, false),
            event(
                T0 + CONTACT,
                Body::Contact(ContactCard {
                    name: Some("Somebody".into()),
                    ..ContactCard::default()
                }),
                false,
                false,
            ),
            event(
                T0 + PLACE,
                Body::Location(Location {
                    lat_e7: 1,
                    lon_e7: 2,
                    name: None,
                    address: None,
                }),
                false,
                false,
            ),
            event(
                T0 + NOTICE,
                Body::System(SystemEvent::TimerChanged {
                    by: Some(alice()),
                    timer: None,
                }),
                false,
                false,
            ),
            event(
                T0 + ONCE,
                Body::Media {
                    attachments: vec![synthetic_image(640, 480, 12)],
                    caption: Some(Text::plain("a view-once caption")),
                },
                true,
                false,
            ),
            media(T0 + TIMED, synthetic_image(320, 200, 13), false, true),
            event(T0 + DELETED, Body::text("to be deleted"), false, false),
            quote(T0 + QUOTE_ONCE, T0 + ONCE),
            quote(T0 + QUOTE_TIMED, T0 + TIMED),
            quote(T0 + QUOTE_GONE, EXPIRED),
            event(T0 + ORIGINAL, Body::text("the original"), false, false),
            quote(T0 + QUOTE_HIDDEN, T0 + ORIGINAL),
            quote(T0 + QUOTE_UNKNOWN, NEVER),
            media(
                T0 + BIG,
                synthetic_file("big.bin", "application/octet-stream", BIG_SIZE, 3),
                false,
                false,
            ),
        ])
        .deliver(vec![BackendEvent::Deleted {
            target: target(
                Some(&d_alice()),
                Some(&alice()),
                &(T0 + DELETED).to_string(),
            ),
            by: Some(alice()),
            for_everyone: true,
            at: T0 + 50_000,
        }]);
    if let Some(state) = state {
        s = s.with_chat_export(d_alice(), state);
    }
    s
}

struct Scene {
    env: Env,
    conv: ConversationId,
    rows: Vec<MessageRow>,
}

fn files(row: &MessageRow) -> Vec<&Attachment> {
    match &row.body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => Vec::new(),
    }
}

impl Scene {
    async fn new(state: Option<ChatExport>) -> Self {
        let mut env = Env::playing(NetworkKind::WhatsApp, script(state)).await;
        env.handle.send(UiCommand::UiReady).expect("send");
        let account = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
        env.wait_finished(account).await;
        let deadline = tokio::time::Instant::now() + support::WAIT * 3;
        loop {
            let conv: Option<ConversationId> = env.peek(|c| {
                c.query_row(
                    "SELECT id FROM conversations WHERE conv_key = ?1",
                    [d_alice().encode()],
                    |r| r.get(0).map(ConversationId),
                )
                .ok()
            });
            if let Some(conv) = conv {
                let rows: Vec<MessageRow> = env
                    .timeline(conv)
                    .await
                    .into_iter()
                    .filter(|r| r.sent_at >= T0)
                    .collect();
                // The downloads the policy allows are over: every picture of the script and the
                // file are here (the video waits for a click).
                let here = |n: i64| {
                    rows.iter()
                        .find(|r| r.sent_at == T0 + n)
                        .is_some_and(|r| files(r).iter().all(|a| a.state == AttachmentState::Local))
                };
                let gone = rows
                    .iter()
                    .any(|r| r.sent_at == T0 + DELETED && r.deleted.is_some());
                if rows.len() == COUNT
                    && gone
                    && [PHOTO, FILE, ONCE, TIMED, BIG].iter().all(|n| here(*n))
                {
                    return Self { env, conv, rows };
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scene did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn row(&self, n: i64) -> &MessageRow {
        self.rows.iter().find(|r| r.sent_at == T0 + n).expect("row")
    }

    fn all_ids(&self) -> Vec<MessageId> {
        self.rows.iter().map(|r| r.id).collect()
    }

    async fn plan(&self, ids: Vec<MessageId>) -> ExportPlan {
        match self
            .env
            .handle
            .query(Query::ExportPlan {
                conv: self.conv,
                ids,
            })
            .await
        {
            Ok(QueryData::ExportPlan(p)) => *p,
            other => panic!("{other:?}"),
        }
    }

    /// The command of an export with the two documents every test uses.
    fn command(
        &self,
        request: RequestId,
        dir: &std::path::Path,
        name: &str,
        with_disappearing: bool,
        messages: Vec<ExportStamp>,
        files: Vec<AttachmentId>,
    ) -> UiCommand {
        UiCommand::ExportArchive {
            request,
            conv: self.conv,
            target: SaveTarget {
                dir: dir.to_owned(),
                name: name.to_owned(),
            },
            mode: SaveMode::NoReplace,
            with_disappearing,
            messages,
            files: files
                .into_iter()
                .enumerate()
                .map(|(i, attachment)| ExportFile {
                    attachment,
                    entry: format!("files/f{i}"),
                })
                .collect(),
            documents: vec![
                ExportDocument {
                    entry: "index.html".into(),
                    bytes: b"<html></html>".to_vec(),
                },
                ExportDocument {
                    entry: "messages.jsonl".into(),
                    bytes: b"{}\n".to_vec(),
                },
            ],
            manifest: ExportHead(
                serde_json::json!({"format": 1})
                    .as_object()
                    .expect("object")
                    .clone(),
            ),
        }
    }

    /// The stamps the plan would give these rows (the scene's rows are settled).
    fn stamps(&self, ids: &[MessageId]) -> Vec<ExportStamp> {
        ids.iter()
            .map(|id| match self.rows.iter().find(|r| r.id == *id) {
                Some(r) => ExportStamp {
                    id: *id,
                    deleted: r.deleted.is_some(),
                    revision: r.revision,
                },
                None => ExportStamp {
                    id: *id,
                    deleted: false,
                    revision: 0,
                },
            })
            .collect()
    }

    /// Sends the command and returns its outcome and the progress events seen before it.
    async fn export(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        with_disappearing: bool,
        messages: Vec<MessageId>,
        files: Vec<AttachmentId>,
        cancel_now: bool,
    ) -> (ExportOutcome, Vec<(u64, u64)>) {
        let stamps = self.stamps(&messages);
        let cancel = if cancel_now { Cancel::Now } else { Cancel::No };
        self.export_with(dir, name, with_disappearing, stamps, files, cancel)
            .await
    }

    async fn export_with(
        &mut self,
        dir: &std::path::Path,
        name: &str,
        with_disappearing: bool,
        messages: Vec<ExportStamp>,
        files: Vec<AttachmentId>,
        cancel: Cancel,
    ) -> (ExportOutcome, Vec<(u64, u64)>) {
        let request = RequestId(7);
        let command = self.command(request, dir, name, with_disappearing, messages, files);
        self.env.handle.send(command).expect("send");
        if cancel == Cancel::Now {
            // Right behind the command. (The flag stands from the command, so the job ends at its
            // first look: this is NOT a cancel in the middle of a write; see
            // `a_cancel_in_the_middle_of_a_large_file_leaves_nothing`.)
            self.env
                .handle
                .send(UiCommand::CancelExport { request })
                .expect("send");
        }
        let mut progress = Vec::new();
        let handle = self.env.handle.clone();
        let mut cancelled = false;
        let outcome = self
            .env
            .event("the end of the export", |e| match e {
                UiEvent::Progress(Progress::Export {
                    request: r,
                    done,
                    total,
                }) if *r == request => {
                    progress.push((*done, *total));
                    if cancel == Cancel::AfterProgress && !cancelled {
                        cancelled = true;
                        handle
                            .send(UiCommand::CancelExport { request })
                            .expect("send");
                    }
                    None
                }
                UiEvent::ExportEnded {
                    request: r,
                    outcome,
                } if *r == request => Some(outcome.clone()),
                _ => None,
            })
            .await;
        (outcome, progress)
    }
}

/// When a test cancels.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cancel {
    No,
    /// Right behind the command.
    Now,
    /// Behind the first [`Progress::Export`] event.
    AfterProgress,
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut n: Vec<String> = std::fs::read_dir(dir)
        .expect("folder")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    n.sort();
    n
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_plan_states_every_kind_with_its_verdict_and_file_state() {
    let s = Scene::new(None).await;
    let mut ids = s.all_ids();
    // An id nobody has and a message of another chat are not in the plan.
    ids.push(MessageId(9_999_999));
    let other = s.env.timeline(support_other_chat(&s).await).await;
    ids.extend(other.iter().map(|r| r.id));
    let plan = s.plan(ids).await;
    assert_eq!(plan.chat, Ok(()));
    assert_eq!(
        plan.messages.len(),
        COUNT,
        "unknown and foreign ids are absent"
    );
    let order: Vec<i64> = plan.messages.iter().map(|m| m.row.sort_key).collect();
    assert!(order.windows(2).all(|w| w[0] <= w[1]), "timeline order");
    let by = |n: i64| {
        plan.messages
            .iter()
            .find(|m| m.row.sent_at == T0 + n)
            .expect("planned")
    };
    for n in [
        TEXT, CONTACT, PLACE, NOTICE, PHOTO, VIDEO, FILE, VOICE, STICKER,
    ] {
        assert_eq!(by(n).verdict, ExportVerdict::Included, "kind {n}");
    }
    assert_eq!(
        by(DELETED).verdict,
        ExportVerdict::Included,
        "a deleted line"
    );
    assert!(by(DELETED).files.is_empty());
    assert_eq!(by(ONCE).verdict, ExportVerdict::LeftOut(LeftOut::ViewOnce));
    assert_eq!(by(TIMED).verdict, ExportVerdict::NeedsTick);
    // Files: here -> accepted (with the sniffed type of a picture), not here -> listed, never
    // fetched, view-once -> the message's verdict.
    let photo = &by(PHOTO).files[0];
    assert_eq!(photo.accepted, Ok(()));
    assert_eq!(photo.sniffed.as_deref(), Some("image/jpeg"));
    assert_eq!(by(VIDEO).files[0].accepted, Err(ExportFileRefusal::NotHere));
    assert_eq!(by(VIDEO).files[0].sniffed, None);
    assert_eq!(by(FILE).files[0].accepted, Ok(()));
    assert_eq!(by(FILE).files[0].file_name.as_deref(), Some("notes.bin"));
    assert_eq!(by(FILE).files[0].size, 3000);
    // A left-out message carries nothing of its content (M1): the neutral placeholder, no
    // caption, no file, no reply, no revision.
    let once = by(ONCE);
    assert_eq!(once.row.body, Body::Placeholder(Placeholder::Expired));
    assert!(once.files.is_empty() && once.revisions.is_empty() && once.row.reply.is_none());
    assert!(once.row.reactions.is_empty() && once.quote.is_none());
    assert!(once.row.view_once, "the fact that it was view-once stays");
    assert_eq!(by(TIMED).files[0].accepted, Ok(()));
    assert_eq!(by(VOICE).files[0].kind, AttachmentKind::Voice);
    assert_eq!(by(STICKER).files.len(), 1);
}

/// The conversation of the setup's other direct chat (the one with Bob).
async fn support_other_chat(s: &Scene) -> ConversationId {
    s.env.peek(|c| {
        c.query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [d_bob().encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("bob's chat")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_archive_has_the_manifest_last_and_only_what_may_leave() {
    let mut s = Scene::new(None).await;
    let dir = tempfile::tempdir().expect("folder");
    let plan = s.plan(s.all_ids()).await;
    // The chat's header fact.
    let detail = match s.env.handle.query(Query::Conversation(s.conv)).await {
        Ok(QueryData::Conversation(Some(d))) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(detail.export(), Ok(()));
    let accepted = |n: i64| -> Vec<AttachmentId> {
        plan.messages
            .iter()
            .filter(|m| m.row.sent_at == T0 + n)
            .flat_map(|m| m.files.iter())
            .filter(|f| f.accepted.is_ok())
            .map(|f| f.attachment)
            .collect()
    };
    let included: Vec<MessageId> = plan
        .messages
        .iter()
        .filter(|m| m.verdict == ExportVerdict::Included)
        .map(|m| m.row.id)
        .collect();
    let mut to_copy = accepted(PHOTO);
    to_copy.extend(accepted(FILE));
    let (outcome, progress) = s
        .export(
            dir.path(),
            "out.zip",
            false,
            included,
            to_copy.clone(),
            false,
        )
        .await;
    let ExportOutcome::Done {
        files,
        failed,
        bytes,
        ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!((files, failed.len()), (2, 0));
    assert_eq!(listing(dir.path()), ["out.zip"]);
    assert_eq!(
        bytes,
        std::fs::metadata(dir.path().join("out.zip"))
            .expect("meta")
            .len()
    );
    assert!(!progress.is_empty(), "progress events arrive");
    assert!(
        progress.windows(2).all(|w| w[0].0 <= w[1].0),
        "never back: {progress:?}"
    );
    assert!(
        progress.last().is_some_and(|(d, t)| d == t),
        "ends at the total: {progress:?}"
    );
    let mut zip =
        zip::ZipArchive::new(std::fs::File::open(dir.path().join("out.zip")).expect("zip"))
            .expect("archive");
    let mut read = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).expect("entry");
        let mut b = Vec::new();
        f.read_to_end(&mut b).expect("read");
        read.push((f.name().unwrap().into_owned(), b));
    }
    let names: Vec<&str> = read.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "files/f0",
            "files/f1",
            "index.html",
            "messages.jsonl",
            "manifest.json"
        ],
        "no file of a view-once, a timed or a missing message, and the manifest is last"
    );
    let manifest: serde_json::Value = serde_json::from_slice(&read[4].1).expect("json");
    assert_eq!(manifest["format"], 1);
    assert_eq!(manifest["files_failed"], serde_json::json!([]));
    let listed = manifest["entries"].as_array().expect("entries");
    assert_eq!(listed.len(), 4, "every entry before the manifest");
    for (entry, (name, bytes)) in listed.iter().zip(&read) {
        assert_eq!(entry["path"], name.as_str());
        assert_eq!(entry["size"], bytes.len());
        assert_eq!(entry["sha256"], hex(Sha256::digest(bytes).as_slice()));
    }
    // With the tick the disappearing message's file is in.
    let dir2 = tempfile::tempdir().expect("folder");
    let mut with_tick: Vec<MessageId> = plan
        .messages
        .iter()
        .filter(|m| m.verdict != ExportVerdict::LeftOut(LeftOut::ViewOnce))
        .map(|m| m.row.id)
        .collect();
    with_tick.sort();
    let mut copy = to_copy.clone();
    copy.extend(accepted(TIMED));
    let (outcome, _) = s
        .export(dir2.path(), "t.zip", true, with_tick.clone(), copy, false)
        .await;
    assert!(
        matches!(outcome, ExportOutcome::Done { files: 3, .. }),
        "{outcome:?}"
    );
    // ... and without it the same request is Changed, nothing written.
    let dir3 = tempfile::tempdir().expect("folder");
    let (outcome, _) = s
        .export(
            dir3.path(),
            "n.zip",
            false,
            with_tick,
            to_copy.clone(),
            false,
        )
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    assert!(listing(dir3.path()).is_empty());
    // A file of a message that is not listed, a view-once file, a file that is not here.
    let once_file = self::files(s.row(ONCE))[0].id.expect("the view-once file");
    let video = plan
        .messages
        .iter()
        .find(|m| m.row.sent_at == T0 + VIDEO)
        .expect("video");
    let listed_text = vec![plan.messages[0].row.id];
    for (what, file) in [
        ("view-once", once_file),
        ("not here", video.files[0].attachment),
        ("not listed", to_copy[0]),
    ] {
        let d = tempfile::tempdir().expect("folder");
        let (outcome, _) = s
            .export(
                d.path(),
                "x.zip",
                true,
                listed_text.clone(),
                vec![file],
                false,
            )
            .await;
        assert_eq!(
            outcome,
            ExportOutcome::Failed(ExportFailure::Changed),
            "{what}"
        );
        assert!(listing(d.path()).is_empty(), "{what}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_deleted_after_the_plan_fails_the_whole_export() {
    let mut s = Scene::new(None).await;
    let plan = s.plan(s.all_ids()).await;
    let included: Vec<MessageId> = plan
        .messages
        .iter()
        .filter(|m| m.verdict == ExportVerdict::Included)
        .map(|m| m.row.id)
        .collect();
    let victim = s.row(TEXT).id;
    s.env
        .handle
        .send(UiCommand::DeleteMessage {
            message: victim,
            for_everyone: false,
        })
        .expect("send");
    // The delete is stored on the compose queue and the export runs on the saves queue: the core
    // orders nothing across them, so the export goes only once the delete is in the store (a
    // `MessagesChanged` of anything else could come first).
    let (scene, ids) = (&s, s.all_ids());
    support::eventually("the delete in the store", support::WAIT, || {
        let ids = ids.clone();
        async move {
            scene
                .plan(ids)
                .await
                .messages
                .iter()
                .all(|m| m.row.id != victim)
        }
    })
    .await;
    let dir = tempfile::tempdir().expect("folder");
    let (outcome, _) = s
        .export(dir.path(), "d.zip", false, included, Vec::new(), false)
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    assert!(listing(dir.path()).is_empty());
    // The plan no longer shows it.
    let plan = s.plan(s.all_ids()).await;
    assert!(plan.messages.iter().all(|m| m.row.id != victim));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restricted_or_unstated_chat_is_refused_by_the_plan_and_the_command() {
    for (state, reason) in [
        (ChatExport::Restricted, Refusal::ExportRestricted),
        (ChatExport::Unstated, Refusal::ExportUnstated),
    ] {
        let mut s = Scene::new(Some(state)).await;
        let plan = s.plan(s.all_ids()).await;
        assert_eq!(plan.chat, Err(reason));
        assert!(plan.messages.is_empty());
        let detail = match s.env.handle.query(Query::Conversation(s.conv)).await {
            Ok(QueryData::Conversation(Some(d))) => d,
            other => panic!("{other:?}"),
        };
        assert_eq!(detail.export(), Err(reason));
        let dir = tempfile::tempdir().expect("folder");
        let ids = s.all_ids();
        let (outcome, _) = s
            .export(dir.path(), "r.zip", true, ids, Vec::new(), false)
            .await;
        assert_eq!(
            outcome,
            ExportOutcome::Failed(ExportFailure::ChatRefused(reason))
        );
        assert!(listing(dir.path()).is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_ends_cancelled_and_leaves_nothing() {
    let mut s = Scene::new(None).await;
    let plan = s.plan(s.all_ids()).await;
    let photo = plan
        .messages
        .iter()
        .find(|m| m.row.sent_at == T0 + PHOTO)
        .expect("photo");
    let dir = tempfile::tempdir().expect("folder");
    // Many copies of one file, so the writer is still going when the cancel arrives.
    let (outcome, _) = s
        .export(
            dir.path(),
            "c.zip",
            false,
            vec![photo.row.id],
            vec![photo.files[0].attachment; 300],
            true,
        )
        .await;
    assert_eq!(outcome, ExportOutcome::Cancelled);
    assert!(listing(dir.path()).is_empty(), "nothing is left");
    // A cancel of an unknown request does nothing, and the next export still runs.
    s.env
        .handle
        .send(UiCommand::CancelExport {
            request: RequestId(404),
        })
        .expect("send");
    let (outcome, _) = s
        .export(
            dir.path(),
            "c.zip",
            false,
            vec![photo.row.id],
            Vec::new(),
            false,
        )
        .await;
    assert!(matches!(outcome, ExportOutcome::Done { .. }), "{outcome:?}");
}

/// A writable connection to core.db: the fake network cannot deliver a late event, so a test
/// states what such an event leaves in the store (a tombstone, a revision, a hidden row).
fn rw(s: &Scene) -> kourier_db::rusqlite::Connection {
    kourier_db::open(
        &s.env.core_db(),
        &kourier_db::DbKey::from(support::CORE_KEY),
    )
    .expect("open core.db")
}

fn included(plan: &ExportPlan) -> Vec<ExportStamp> {
    plan.messages
        .iter()
        .filter(|m| m.verdict == ExportVerdict::Included)
        .map(|m| m.stamp)
        .collect()
}

/// Waits until the plan of `ids` says `ok`.
async fn plan_shows(s: &Scene, what: &str, ok: impl Fn(&ExportPlan) -> bool) {
    let ids = s.all_ids();
    support::eventually(what, support::WAIT, || {
        let ids = ids.clone();
        let ok = &ok;
        async move { ok(&s.plan(ids).await) }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_deleted_for_everyone_or_edited_after_the_plan_fails_the_whole_export() {
    let mut s = Scene::new(None).await;
    let old = included(&s.plan(s.all_ids()).await);
    let (text, contact) = (s.row(TEXT).id, s.row(CONTACT).id);

    // (a) Deleted FOR EVERYONE after the plan (the stored tombstone): the verdict is the same
    // before and after, the stamp is what tells.
    rw(&s)
        .execute(
            "UPDATE messages SET deleted = 'ForEveryone', deleted_by = NULL WHERE id = ?1",
            [text.0],
        )
        .expect("tombstone");
    plan_shows(&s, "the tombstone in the plan", |p| {
        p.messages
            .iter()
            .any(|m| m.row.id == text && m.row.deleted.is_some())
    })
    .await;
    let dir = tempfile::tempdir().expect("folder");
    let (outcome, _) = s
        .export_with(
            dir.path(),
            "a.zip",
            false,
            old.clone(),
            Vec::new(),
            Cancel::No,
        )
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    assert!(listing(dir.path()).is_empty());

    // (b) Edited after the plan.
    rw(&s)
        .execute(
            "UPDATE messages SET revision = revision + 1 WHERE id = ?1",
            [contact.0],
        )
        .expect("edit");
    plan_shows(&s, "the edit in the plan", |p| {
        p.messages
            .iter()
            .any(|m| m.row.id == contact && m.row.revision == 1)
    })
    .await;
    let mut tombstone_only = old.clone();
    for stamp in &mut tombstone_only {
        if stamp.id == text {
            stamp.deleted = true;
        }
    }
    let (outcome, _) = s
        .export_with(
            dir.path(),
            "b.zip",
            false,
            tombstone_only,
            Vec::new(),
            Cancel::No,
        )
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    assert!(listing(dir.path()).is_empty());

    // Two stamps of one id that differ are a changed export too.
    let mut twice = included(&s.plan(s.all_ids()).await);
    let mut other = twice[0];
    other.revision += 1;
    twice.push(other);
    let (outcome, _) = s
        .export_with(dir.path(), "c.zip", false, twice, Vec::new(), Cancel::No)
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));

    // The stamps of a fresh plan go through; an id listed twice counts once (L5).
    let mut fresh = included(&s.plan(s.all_ids()).await);
    let distinct = fresh.len();
    fresh.push(fresh[0]);
    let (outcome, _) = s
        .export_with(dir.path(), "d.zip", false, fresh, Vec::new(), Cancel::No)
        .await;
    let ExportOutcome::Done { messages, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(messages as usize, distinct);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quote_follows_the_verdict_of_its_original_and_is_stripped_when_left_out() {
    let s = Scene::new(None).await;
    // The core remembers the key of a message that expired here (it is not in core.db).
    {
        let conn = rw(&s);
        let account: i64 = conn
            .query_row(
                "SELECT account_id FROM conversations WHERE id = ?1",
                [s.conv.0],
                |r| r.get(0),
            )
            .expect("account");
        for scope in 0..3 {
            conn.execute(
                "INSERT INTO expired_messages(account_id, net_scope, conv_key, net_author,
                   net_local, expired_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    account,
                    scope,
                    d_alice().encode(),
                    alice().as_str(),
                    EXPIRED.to_string(),
                    i64::MAX / 2,
                ),
            )
            .expect("remember");
        }
    }
    let plan = s.plan(s.all_ids()).await;
    let by = |p: &ExportPlan, n: i64| {
        p.messages
            .iter()
            .find(|m| m.row.sent_at == T0 + n)
            .expect("planned")
            .clone()
    };
    let stripped = |m: &kourier_core::facade::PlannedMessage| {
        let r = m.row.reply.as_ref().expect("a quote stays");
        assert!(r.author.is_some(), "the quote keeps its author");
        assert!(
            r.text.is_none()
                && r.snapshot.is_none()
                && r.thumb.is_none()
                && r.attachment.is_none()
                && r.file_name.is_none(),
            "nothing of a left-out quote's content: {r:?}"
        );
    };
    assert_eq!(by(&plan, TEXT).quote, None, "no quote");
    // View-once original: stripped, though the original's row is in core.db.
    let m = by(&plan, QUOTE_ONCE);
    assert_eq!(m.quote, Some(ExportVerdict::LeftOut(LeftOut::ViewOnce)));
    stripped(&m);
    // Disappearing original: kept whole, flagged.
    let m = by(&plan, QUOTE_TIMED);
    assert_eq!(m.quote, Some(ExportVerdict::NeedsTick));
    let r = m.row.reply.as_ref().expect("quote");
    assert_eq!(r.text.as_ref().map(|t| t.body.as_str()), Some(SNAPSHOT));
    assert!(r.thumb.is_some());
    // An original the core remembers as expired: Gone, stripped.
    let m = by(&plan, QUOTE_GONE);
    assert_eq!(m.quote, Some(ExportVerdict::LeftOut(LeftOut::Gone)));
    stripped(&m);
    // An original it never had: the sender's snapshot stays, as the timeline shows it.
    let m = by(&plan, QUOTE_UNKNOWN);
    assert_eq!(m.quote, Some(ExportVerdict::Included));
    assert_eq!(
        m.row
            .reply
            .as_ref()
            .and_then(|r| r.text.as_ref())
            .map(|t| t.body.as_str()),
        Some(SNAPSHOT)
    );
    // A known original: included, then hidden (a timer that ran out on an unconfirmed clock):
    // the lookup finds the HIDDEN row (the timeline's own does not) and strips the quote.
    assert_eq!(by(&plan, QUOTE_HIDDEN).quote, Some(ExportVerdict::Included));
    rw(&s)
        .execute(
            "UPDATE messages SET expiry_hidden = 1 WHERE net_local = ?1",
            [(T0 + ORIGINAL).to_string()],
        )
        .expect("hide");
    let plan = s.plan(s.all_ids()).await;
    assert!(
        plan.messages.iter().all(|m| m.row.sent_at != T0 + ORIGINAL),
        "the hidden original is not in the plan"
    );
    let m = by(&plan, QUOTE_HIDDEN);
    assert_eq!(m.quote, Some(ExportVerdict::LeftOut(LeftOut::Gone)));
    stripped(&m);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_chat_is_refused_by_the_plan_the_detail_and_the_command() {
    let mut s = Scene::new(None).await;
    // The plan of an open chat is made first: the chat turns protected only afterwards.
    let ids = s.all_ids();
    let stamps = s.stamps(&[s.row(TEXT).id]);
    assert_eq!(s.plan(ids.clone()).await.chat, Ok(()));
    rw(&s)
        .execute(
            "UPDATE conversations SET protected = 1 WHERE id = ?1",
            [s.conv.0],
        )
        .expect("protect");
    // A chat that turned protected after the plan fails the command and writes no file.
    let dir = tempfile::tempdir().expect("folder");
    let (outcome, _) = s
        .export_with(dir.path(), "p.zip", false, stamps, Vec::new(), Cancel::No)
        .await;
    assert_eq!(
        outcome,
        ExportOutcome::Failed(ExportFailure::ChatRefused(Refusal::ExportProtected))
    );
    assert!(listing(dir.path()).is_empty());
    // The plan of a protected chat is refused and plans no message.
    let plan = s.plan(ids).await;
    assert_eq!(plan.chat, Err(Refusal::ExportProtected));
    assert!(plan.messages.is_empty());
    // The detail says the same (the header's button).
    let detail = match s.env.handle.query(Query::Conversation(s.conv)).await {
        Ok(QueryData::Conversation(Some(d))) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(detail.export(), Err(Refusal::ExportProtected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chat_without_features_is_refused_by_the_plan_and_the_command() {
    let mut s = Scene::new(None).await;
    rw(&s)
        .execute(
            "UPDATE conversations SET features = NULL WHERE id = ?1",
            [s.conv.0],
        )
        .expect("no features");
    let plan = s.plan(s.all_ids()).await;
    assert!(plan.chat.is_err() && plan.messages.is_empty());
    let dir = tempfile::tempdir().expect("folder");
    let ids = s.all_ids();
    let (outcome, _) = s
        .export(dir.path(), "n.zip", true, ids, Vec::new(), false)
        .await;
    assert!(
        matches!(
            outcome,
            ExportOutcome::Failed(ExportFailure::ChatRefused(_))
        ),
        "{outcome:?}"
    );
    assert!(listing(dir.path()).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_refuses_what_the_rules_refuse_even_when_it_is_listed() {
    let mut s = Scene::new(None).await;
    let dir = tempfile::tempdir().expect("folder");
    // (M5 a) A listed message that is in ANOTHER chat now (a merge or a move between the plan
    // and the command): the plan's stamp is still what the row says, only the chat differs.
    let moved = s.row(PLACE).id;
    let stamp = s.stamps(&[moved]);
    let bob = support_other_chat(&s).await;
    rw(&s)
        .execute(
            "UPDATE messages SET conversation_id = ?2 WHERE id = ?1",
            (moved.0, bob.0),
        )
        .expect("move");
    let (outcome, _) = s
        .export_with(dir.path(), "a.zip", false, stamp, Vec::new(), Cancel::No)
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    // (M5 b) A listed message, listed with a file the save rule refuses (not on this computer).
    let video = files(s.row(VIDEO))[0].id.expect("video");
    let ids = vec![s.row(VIDEO).id];
    let (outcome, _) = s
        .export(dir.path(), "b.zip", false, ids, vec![video], false)
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    // (M5 j) A file the rule accepts (the stored state says it is here) that the media index
    // does not resolve.
    let local = serde_json::to_string(&AttachmentState::Local).expect("json");
    rw(&s)
        .execute(
            "UPDATE attachments SET state = ?2, file_id = NULL WHERE id = ?1",
            (video.0, local),
        )
        .expect("lost blob");
    let ids = vec![s.row(VIDEO).id];
    let (outcome, _) = s
        .export(dir.path(), "j.zip", false, ids, vec![video], false)
        .await;
    assert_eq!(outcome, ExportOutcome::Failed(ExportFailure::Changed));
    assert!(listing(dir.path()).is_empty(), "nothing was written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_in_the_middle_of_a_large_file_leaves_nothing() {
    let mut s = Scene::new(None).await;
    let big = files(s.row(BIG))[0].id.expect("big file");
    let ids = vec![s.row(BIG).id];
    let stamps = s.stamps(&ids);
    let dir = tempfile::tempdir().expect("folder");
    // The cancel comes AFTER the first progress event of the file (a few MiB: the writer is
    // inside it).
    let (outcome, progress) = s
        .export_with(
            dir.path(),
            "big.zip",
            false,
            stamps,
            vec![big],
            Cancel::AfterProgress,
        )
        .await;
    assert_eq!(outcome, ExportOutcome::Cancelled);
    assert!(!progress.is_empty(), "the file was being written");
    assert!(listing(dir.path()).is_empty(), "nothing is left");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_export_with_a_running_request_id_is_refused_and_the_first_keeps_its_cancel() {
    let mut s = Scene::new(None).await;
    let big = files(s.row(BIG))[0].id.expect("big file");
    let stamps = s.stamps(&[s.row(BIG).id]);
    let dir = tempfile::tempdir().expect("folder");
    let request = RequestId(7);
    // Three copies of the large file: the first export runs for a while.
    let first = s.command(
        request,
        dir.path(),
        "one.zip",
        false,
        stamps.clone(),
        vec![big; 3],
    );
    s.env.handle.send(first).expect("send");
    let second = s.command(request, dir.path(), "two.zip", false, stamps, Vec::new());
    s.env.handle.send(second).expect("send");
    // The cancel goes right behind the duplicate, before anything is awaited: the commands are
    // taken in order, so it reaches the first export while it reads or copies, however fast the
    // machine (waiting for the refusal first could let a fast copy end `Done`).
    s.env
        .handle
        .send(UiCommand::CancelExport { request })
        .expect("send");
    // The duplicate ends at once with `Io`.
    let first_end = s
        .env
        .event("the refusal", |e| match e {
            UiEvent::ExportEnded {
                request: r,
                outcome,
            } if *r == request => Some(outcome.clone()),
            _ => None,
        })
        .await;
    assert_eq!(first_end, ExportOutcome::Failed(ExportFailure::Io));
    // ... and the first still had its flag: the cancel reached it.
    let end = s
        .env
        .event("the first end", |e| match e {
            UiEvent::ExportEnded {
                request: r,
                outcome,
            } if *r == request => Some(outcome.clone()),
            _ => None,
        })
        .await;
    assert_eq!(end, ExportOutcome::Cancelled);
    assert!(listing(dir.path()).is_empty());
}
