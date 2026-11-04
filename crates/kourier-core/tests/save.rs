//! F3a, "Save As…" below the UI (architecture §5.8.2): the core's flag
//! (`MessageActions::save_attachments`, the rule `actions::save_attachments`) and its command
//! (`UiCommand::SaveAttachment`) on a core over the fake that PLAYS WhatsApp. WhatsApp is the
//! one network whose view-once messages keep an attachment (Signal maps them with none), so
//! only here does the view-once clause of the rule have something to refuse.

use crate::support;

use std::path::Path;
use std::sync::Arc;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{
    ActionSubject, CoreError, MessageRow, Query, QueryData, Refusal, SaveError, SaveFacts,
    SaveMode, SaveTarget, UiCommand, UiEvent,
};
use kourier_fake::Script;
use kourier_fake::attachments::{Pointer, synthetic_file, synthetic_image};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{
    AccountCaps, Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, ConversationId,
    NetworkKind,
};
use std::os::unix::fs::PermissionsExt;
use support::send::{ME, alice, d_alice, setup, uid};
use support::{Env, add_linked_as};

const RECEIVE_LIMIT: u64 = 1 << 20;

fn media(local: i64, attachments: Vec<Attachment>, view_once: bool) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "");
    m.body = Body::Media {
        attachments,
        caption: None,
    };
    m.view_once = view_once;
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

/// Alice sends a picture, a view-once picture, a video that is not downloaded (a video waits
/// for a click), a file and a picture above the receive limit (its download failed for good).
fn script() -> Script {
    let mut first = setup();
    first.insert(
        0,
        BackendEvent::AccountCaps(AccountCaps {
            max_attachment_bytes: Some(4 << 20),
            max_receive_attachment_bytes: Some(RECEIVE_LIMIT),
            ..Default::default()
        }),
    );
    Script::new("save", vec![uid(ME)])
        .deliver(first)
        .deliver(vec![
            media(T0 + 1, vec![synthetic_image(900, 600, 11)], false),
            media(T0 + 2, vec![synthetic_image(640, 480, 12)], true),
            media(T0 + 3, vec![video()], false),
            media(
                T0 + 4,
                vec![synthetic_file(
                    "notes.bin",
                    "application/octet-stream",
                    3000,
                    5,
                )],
                false,
            ),
            media(
                T0 + 5,
                vec![synthetic_file(
                    "big.iso",
                    "application/x-iso9660-image",
                    2 << 20,
                    6,
                )],
                false,
            ),
        ])
}

/// The ids of the attachments of a row.
fn ids(row: &MessageRow) -> Vec<AttachmentId> {
    match &row.body {
        Body::Media { attachments, .. } => attachments.iter().filter_map(|a| a.id).collect(),
        Body::Sticker(a) => a.id.into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The rows that carry files (the script's own first message is a text).
fn media_rows(rows: Vec<MessageRow>) -> Vec<MessageRow> {
    rows.into_iter()
        .filter(|r| matches!(r.body, Body::Media { .. }))
        .collect()
}

struct Scene {
    env: Env,
    conv: ConversationId,
    photo: AttachmentId,
    once: AttachmentId,
    video: AttachmentId,
    file: AttachmentId,
    big: AttachmentId,
}

impl Scene {
    async fn new() -> Self {
        let mut env = Env::playing(NetworkKind::WhatsApp, script()).await;
        env.handle.send(UiCommand::UiReady).expect("send");
        let account = add_linked_as(&mut env, NetworkKind::WhatsApp).await;
        env.wait_finished(account).await;
        // The downloads the policy allows end: the picture, the view-once picture and the file
        // are here, the video waits for a click, the big picture failed for good.
        let deadline = tokio::time::Instant::now() + support::WAIT * 3;
        let mut seen = String::from("no conversation");
        let (conv, rows) = loop {
            let conv: Option<ConversationId> = env.peek(|c| {
                c.query_row(
                    "SELECT id FROM conversations WHERE conv_key = ?1",
                    [d_alice().encode()],
                    |r| r.get(0).map(ConversationId),
                )
                .ok()
            });
            if let Some(conv) = conv {
                let rows = media_rows(env.timeline(conv).await);
                let local = |row: &MessageRow| {
                    matches!(&row.body, Body::Media { attachments, .. }
                        if attachments.iter().all(|a| a.state == AttachmentState::Local))
                };
                if rows.len() == 5 && [0, 1, 3].iter().all(|i| local(&rows[*i])) {
                    break (conv, rows);
                }
                seen = format!(
                    "{} rows, attachment states {:?}",
                    rows.len(),
                    rows.iter().map(|r| ids(r).len()).collect::<Vec<_>>()
                );
                seen.push_str(&format!(
                    " {:?}",
                    rows.iter()
                        .map(|r| match &r.body {
                            Body::Media { attachments, .. } =>
                                attachments.iter().map(|a| a.state).collect::<Vec<_>>(),
                            _ => Vec::new(),
                        })
                        .collect::<Vec<_>>()
                ));
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the downloads did not end: {seen}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let one = |i: usize| ids(&rows[i])[0];
        Self {
            conv,
            photo: one(0),
            once: one(1),
            video: one(2),
            file: one(3),
            big: one(4),
            env,
        }
    }

    /// Sends a save and waits for its answer: the event, or the failure's error.
    async fn save(
        &mut self,
        attachment: AttachmentId,
        dir: &Path,
        name: &str,
        mode: SaveMode,
    ) -> Result<u64, Arc<CoreError>> {
        self.env
            .handle
            .send(UiCommand::SaveAttachment {
                attachment,
                target: SaveTarget {
                    dir: dir.to_owned(),
                    name: name.to_owned(),
                },
                mode,
            })
            .expect("send");
        self.env
            .event("the answer to the save", |e| match e {
                UiEvent::AttachmentSaved {
                    attachment: a,
                    bytes,
                } if *a == attachment => Some(Ok(*bytes)),
                UiEvent::ActionFailed {
                    action: "SaveAttachment",
                    error,
                    subject: ActionSubject::Attachment(a),
                    ..
                } if *a == attachment => Some(Err(error.clone())),
                _ => None,
            })
            .await
    }

    async fn flags(&self) -> Vec<Vec<AttachmentId>> {
        self.env
            .timeline(self.conv)
            .await
            .iter()
            .map(|r| r.actions.save_attachments.clone())
            .collect()
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf-8"))
        .collect();
    names.sort();
    names
}

/// The table of the flag on the real core: the file that is here is offered, the view-once one
/// (whose attachment WhatsApp keeps) is not, and neither is what was never downloaded or failed
/// for good. The view-once row is the one only this network can prove (critic M9).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_flag_offers_what_is_here_and_never_a_view_once_file() {
    let s = Scene::new().await;
    let all = s.env.timeline(s.conv).await;
    assert!(
        all.iter()
            .filter(|r| !matches!(r.body, Body::Media { .. }))
            .all(|r| r.actions.save_attachments.is_empty()),
        "a text has nothing to save"
    );
    let rows = media_rows(all);
    assert_eq!(
        rows.iter()
            .map(|r| r.actions.save_attachments.clone())
            .collect::<Vec<_>>(),
        [vec![s.photo], vec![], vec![], vec![s.file], vec![]],
        "picture, view-once picture, video not here, file, failed download"
    );
    // The row of the view-once message has its attachment, and it IS here: only the clause
    // keeps it from the flag.
    assert!(rows[1].view_once);
    assert_eq!(ids(&rows[1]), [s.once]);
    assert!(matches!(&rows[1].body, Body::Media { attachments, .. }
        if attachments[0].state == AttachmentState::Local));
    let state = |row: &MessageRow| match &row.body {
        Body::Media { attachments, .. } => attachments[0].state,
        _ => unreachable!(),
    };
    assert_eq!(state(&rows[2]), AttachmentState::Remote);
    assert!(matches!(state(&rows[4]), AttachmentState::Failed { .. }));
    // The other rows' view-once flag is off.
    assert!(!rows[0].view_once && !rows[3].view_once);
}

/// The command writes the plaintext the sender sent, without an executable bit, answers a taken
/// name in the no-replace mode, replaces in the other, and refuses on the facts again: a
/// view-once file, one that is not here, a name that is no single path component, a folder that
/// is not there, and a message that was deleted after the dialog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_writes_a_copy_and_refuses_on_the_facts_again() {
    let mut s = Scene::new().await;
    let dir = tempfile::tempdir().expect("folder");
    let plain = Pointer::from_blob(&synthetic_image(900, 600, 11).remote.expect("remote"))
        .expect("pointer")
        .plaintext()
        .expect("bytes")
        .expect("ok");

    let (photo, once, video, file, big) = (s.photo, s.once, s.video, s.file, s.big);
    let bytes = s
        .save(photo, dir.path(), "photo.jpg", SaveMode::NoReplace)
        .await
        .expect("saved");
    assert_eq!(bytes, plain.len() as u64);
    let saved = dir.path().join("photo.jpg");
    assert_eq!(std::fs::read(&saved).expect("copy"), plain);
    let mode = std::fs::metadata(&saved)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "no executable bit: {mode:o}");
    assert_eq!(listing(dir.path()), ["photo.jpg"]);

    // A name that is taken: the no-replace mode answers `Exists`, the file is as it was.
    std::fs::write(&saved, b"changed since").expect("write");
    let err = s
        .save(photo, dir.path(), "photo.jpg", SaveMode::NoReplace)
        .await
        .expect_err("taken");
    assert!(
        matches!(&*err, CoreError::Save(SaveError::Exists)),
        "{err:?}"
    );
    assert_eq!(std::fs::read(&saved).expect("copy"), b"changed since");
    s.save(photo, dir.path(), "photo.jpg", SaveMode::Replace)
        .await
        .expect("replaced");
    assert_eq!(std::fs::read(&saved).expect("copy"), plain);
    // The other file saves too, under the name the dialog gave it.
    s.save(file, dir.path(), "notes.bin", SaveMode::NoReplace)
        .await
        .expect("file");
    assert_eq!(listing(dir.path()), ["notes.bin", "photo.jpg"]);

    // Refused on the facts, nothing written.
    let err = s
        .save(once, dir.path(), "once.jpg", SaveMode::Replace)
        .await
        .expect_err("view-once");
    assert!(
        matches!(
            &*err,
            CoreError::Refused {
                reason: Refusal::WrongKind,
                ..
            }
        ),
        "a view-once message is never saved: {err:?}"
    );
    for (attachment, why) in [(video, "not downloaded"), (big, "failed for good")] {
        let err = s
            .save(attachment, dir.path(), "x.bin", SaveMode::Replace)
            .await
            .expect_err(why);
        assert!(
            matches!(&*err, CoreError::InvalidInput(_)),
            "{why}: {err:?}"
        );
    }
    for bad in ["../escape.jpg", "a/b.jpg", "", "..", "x\0y"] {
        let err = s
            .save(photo, dir.path(), bad, SaveMode::Replace)
            .await
            .expect_err("bad name");
        assert!(
            matches!(&*err, CoreError::Save(SaveError::BadName)),
            "{bad:?}: {err:?}"
        );
    }
    let missing = dir.path().join("no-such-folder");
    let err = s
        .save(photo, &missing, "photo.jpg", SaveMode::Replace)
        .await
        .expect_err("no folder");
    assert!(
        matches!(&*err, CoreError::Save(SaveError::NoFolder)),
        "{err:?}"
    );
    assert!(!missing.exists());
    assert_eq!(listing(dir.path()), ["notes.bin", "photo.jpg"]);
    // A relative folder is never opened against the working directory, not even one that leads
    // to a folder that exists (the way up to the root, then down to the real folder).
    let cwd = std::env::current_dir().expect("working directory");
    let relative: std::path::PathBuf = std::iter::repeat_n("..", cwd.components().count() - 1)
        .collect::<std::path::PathBuf>()
        .join(dir.path().strip_prefix("/").expect("an absolute folder"));
    assert!(relative.is_relative() && relative.is_dir(), "{relative:?}");
    let err = s
        .save(photo, &relative, "rel.jpg", SaveMode::Replace)
        .await
        .expect_err("relative folder");
    assert!(
        matches!(&*err, CoreError::Save(SaveError::NoFolder)),
        "{err:?}"
    );
    assert_eq!(listing(dir.path()), ["notes.bin", "photo.jpg"]);

    // The message is deleted while the dialog is open: the save is refused as for a message
    // that is gone and writes nothing.
    let message = media_rows(s.env.timeline(s.conv).await)[0].id;
    s.env
        .handle
        .send(UiCommand::DeleteMessage {
            message,
            for_everyone: false,
        })
        .expect("send");
    let deadline = tokio::time::Instant::now() + support::WAIT;
    while s.flags().await.iter().any(|f| f.contains(&photo)) {
        assert!(tokio::time::Instant::now() < deadline, "the row stays");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let other = tempfile::tempdir().expect("folder");
    let err = s
        .save(photo, other.path(), "photo.jpg", SaveMode::Replace)
        .await
        .expect_err("deleted");
    assert!(
        matches!(
            &*err,
            CoreError::Refused {
                reason: Refusal::Gone,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(listing(other.path()).is_empty());
}

/// What the name of a saved file is made of (F3b): the sender, the time, the file's place among
/// the message's files, the stored name and type, and what the first bytes say it is (the stored
/// type is the sender's claim). A file that is not here has everything but the sniffed type; an
/// attachment that does not exist has no facts. The gallery says the same as the flag: its
/// items carry `save` for the files that are here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_facts_of_a_name_say_who_when_where_and_what_the_content_is() {
    let s = Scene::new().await;
    let facts = |id: AttachmentId| {
        let handle = s.env.handle.clone();
        async move {
            match handle.query(Query::SaveFacts(id)).await.expect("the query") {
                QueryData::SaveFacts(f) => f,
                other => panic!("another answer: {other:?}"),
            }
        }
    };
    let photo: SaveFacts = facts(s.photo).await.expect("the picture's facts");
    assert_eq!(photo.author, Some(alice()), "the sender");
    assert_eq!(photo.sent_at, T0 + 1, "the time of the message");
    assert_eq!(photo.position, 1);
    assert_eq!(photo.file_name, None);
    assert!(
        photo
            .sniffed
            .as_deref()
            .is_some_and(|m| m.starts_with("image/")),
        "the content says a picture: {:?}",
        photo.sniffed
    );
    let file = facts(s.file).await.expect("the file's facts");
    assert_eq!(file.file_name.as_deref(), Some("notes.bin"));
    assert_eq!(file.sent_at, T0 + 4);
    assert_eq!(file.sniffed, None, "bytes the sniffer does not know");
    let video = facts(s.video).await.expect("the video's facts");
    assert_eq!(video.sniffed, None, "not here: nothing to read");
    assert_eq!(video.mime.as_deref(), Some("video/mp4"));
    assert_eq!(facts(AttachmentId(9_999_999)).await, None);
    // A view-once picture whose file IS here: the rule refuses it, so its name and type are not
    // given out and its first chunk is not decrypted (the dialog answers "gone" before it opens).
    assert_eq!(
        facts(s.once).await,
        None,
        "no facts for a file the save rule refuses"
    );
    // The gallery's items carry the same answer as the flag.
    let gallery = match s
        .env
        .handle
        .query(Query::ChatMedia(kourier_core::facade::ChatMediaQuery {
            conv: s.conv,
            kind: kourier_core::facade::MediaKind::Visual,
            before: None,
            limit: 50,
        }))
        .await
        .expect("the gallery")
    {
        QueryData::ChatMedia(page) => page,
        other => panic!("another answer: {other:?}"),
    };
    let saves: Vec<(AttachmentId, bool)> = gallery
        .items
        .iter()
        .map(|i| (i.attachment.id.expect("id"), i.save))
        .collect();
    assert!(saves.contains(&(s.photo, true)), "{saves:?}");
    assert!(
        !saves.iter().any(|(id, _)| *id == s.once),
        "the gallery lists no view-once picture: {saves:?}"
    );
    assert!(
        saves.iter().all(|(id, save)| *save == (*id == s.photo)),
        "only the picture that is here: {saves:?}"
    );
}
