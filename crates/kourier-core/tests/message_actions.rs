//! Phase G contract groundwork, message side (architecture §3.5, §3.8, §4.8): action flags that
//! are the commands' own checks and end on the clock, captions edited as revisions (own and
//! incoming), replies from `SendFiles` and quotes resolved for display, failures that name
//! their subject, previews with attachment kinds and system events, per-author and account
//! avatars, timer changes with their timeline row and version, and long texts joined once
//! their file is here.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, OutgoingKind};
use kourier_core::facade::{
    ActionSubject, MessageRow, Query, QueryData, QuoteState, TimelineAnchor, TimelinePage,
    TimelineQuery, UiCommand, UiEvent,
};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::attachments::{Pointer, synthetic_file, synthetic_image, synthetic_long_text};
use kourier_fake::scripts::{T0, remote, signal_features};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountId, AccountUuid, Attachment, AttachmentId, AttachmentKind, BackendError, BlobKey,
    BlobRef, Body, ConnState, ContactPatch, ConvKey, ConversationId, ConversationPatch, ErrorKind,
    FileLimits, MessageId, MsgIdScope, MsgTarget, NetMsgId, NetworkKind, OutgoingState, Patch,
    QuoteSnapshot, ReplyTo, SystemEvent, TargetAuthor, Text,
};
use support::send::*;
use support::{CORE_KEY, Env, WAIT, add_linked, eventually};

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

/// Waits until `f` holds on `core.db`.
async fn until(env: &Env, what: &str, f: impl Fn(&kourier_db::rusqlite::Connection) -> bool) {
    let db = env.core_db();
    let f = std::sync::Arc::new(f);
    eventually(what, WAIT, || {
        let (db, f) = (db.clone(), f.clone());
        async move { core_db(&db, |c| f(c)) }
    })
    .await;
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let k = key.clone();
    until(env, "the conversation", move |c| {
        c.query_row(
            "SELECT 1 FROM conversations WHERE conv_key = ?1",
            [k.encode()],
            |_| Ok(()),
        )
        .is_ok()
    })
    .await;
    conv_id(&env.core_db(), key).expect("conversation")
}

/// The message with network id `local` (waits for it).
async fn message(env: &Env, local: i64) -> MessageId {
    let l = local.to_string();
    until(env, "the message", {
        let l = l.clone();
        move |c| {
            c.query_row("SELECT 1 FROM messages WHERE net_local = ?1", [&l], |_| {
                Ok(())
            })
            .is_ok()
        }
    })
    .await;
    core_db(&env.core_db(), |c| {
        c.query_row("SELECT id FROM messages WHERE net_local = ?1", [&l], |r| {
            r.get(0).map(MessageId)
        })
        .expect("message")
    })
}

async fn page(env: &Env, conv: ConversationId) -> TimelinePage {
    let q = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    };
    match env
        .handle
        .query(Query::Timeline(q))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(p) => p,
        other => panic!("unexpected {other:?}"),
    }
}

async fn row(env: &Env, conv: ConversationId, id: MessageId) -> MessageRow {
    page(env, conv)
        .await
        .rows
        .into_iter()
        .find(|r| r.id == id)
        .expect("row")
}

/// Waits until the row of `id` satisfies `f` and returns it.
async fn row_when(
    env: &Env,
    conv: ConversationId,
    id: MessageId,
    what: &str,
    f: impl Fn(&MessageRow) -> bool,
) -> MessageRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let found = page(env, conv).await.rows.into_iter().find(|r| r.id == id);
        if let Some(r) = found
            && f(&r)
        {
            return r;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The error text and the subject of the next `ActionFailed` of `action`.
async fn failed(env: &mut Env, action: &'static str) -> (String, ActionSubject) {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a,
            error,
            subject,
            ..
        } if *a == action => Some((error.to_string(), subject.clone())),
        _ => None,
    })
    .await
}

fn send(env: &Env, conv: ConversationId, text: &str, reply_to: Option<MessageId>) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(text),
            reply_to,
        })
        .expect("send");
}

/// The `n`-th own message of `conv`, once it is `Sent`.
async fn sent(env: &Env, conv: ConversationId, n: usize) -> Own {
    let db = env.core_db();
    eventually("the sent message", WAIT, || {
        let db = db.clone();
        async move {
            own_messages(&db, conv)
                .get(n - 1)
                .is_some_and(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
        }
    })
    .await;
    own_messages(&db, conv)[n - 1].clone()
}

fn edit(env: &Env, message: MessageId, body: &str) {
    env.handle
        .send(UiCommand::EditMessage {
            message,
            text: Text::plain(body),
        })
        .expect("send");
}

fn delete(env: &Env, message: MessageId, for_everyone: bool) {
    env.handle
        .send(UiCommand::DeleteMessage {
            message,
            for_everyone,
        })
        .expect("send");
}

fn text(s: &str) -> Option<Text> {
    Some(Text::plain(s))
}

fn media(local: i64, attachments: Vec<Attachment>, caption: Option<Text>) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "");
    m.body = Body::Media {
        attachments,
        caption,
    };
    BackendEvent::Message(m)
}

fn caption_of(body: &Body) -> Option<&str> {
    match body {
        Body::Media { caption, .. } => caption.as_ref().map(|t| t.body.as_str()),
        _ => None,
    }
}

fn attachments_of(body: &Body) -> &[Attachment] {
    match body {
        Body::Media { attachments, .. } => attachments,
        _ => &[],
    }
}

/// A JPEG file to send.
fn jpeg_file(dir: &tempfile::TempDir, name: &str, seed: u64) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let bytes = sample_image(320, 240, seed, SampleFormat::Jpeg).expect("jpeg");
    std::fs::write(&path, bytes).expect("write");
    path
}

fn send_files(
    env: &Env,
    conv: ConversationId,
    paths: Vec<std::path::PathBuf>,
    caption: Option<&str>,
    reply_to: Option<MessageId>,
) {
    env.handle
        .send(UiCommand::SendFiles {
            request: Default::default(),
            as_file: Vec::new(),
            conv,
            paths: paths.into(),
            caption: caption.map(Text::plain),
            reply_to,
        })
        .expect("send");
}

const DAY: i64 = 24 * 3600 * 1000;

/// Item 1. The flags of a row are what its commands would answer: offered one millisecond
/// before the window ends and accepted, not offered at its end and refused; `until` and the
/// page's `now` tell the UI when to look again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_flags_are_the_commands_checks_and_end_on_the_clock() {
    let (mut env, account, _) = linked(script("flags")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "mine", None);
    let mine = sent(&env, c, 1).await;

    let p = page(&env, c).await;
    assert_eq!(p.now, T0, "the page says which instant its flags are for");
    let theirs = p.rows.iter().find(|r| r.author.is_some()).expect("theirs");
    let a = theirs.actions.clone();
    assert!(a.reply && a.react && a.copy && a.delete_for_me);
    assert!(!a.edit && !a.delete_for_everyone && !a.take_back);
    assert_eq!(a.until, None, "nothing here depends on time");
    let own = p.rows.iter().find(|r| r.id == mine.id).expect("own");
    let a = own.actions.clone();
    assert!(a.reply && a.react && a.copy && a.edit && a.delete_for_everyone && a.delete_for_me);
    assert!(!a.take_back, "the recipient has it");
    // Signal's windows in the fake: 24 h to edit, one day to delete for everyone; an age of
    // exactly the limit is still allowed, so both end one second later.
    let end = mine.sent_at + DAY + 1000;
    assert_eq!(a.until, Some(end));

    // One millisecond before the end: offered, and the command is accepted.
    env.clock.set(u64::try_from(end - 1).expect("time"));
    let before = row(&env, c, mine.id).await;
    assert!(before.actions.edit && before.actions.delete_for_everyone);
    edit(&env, mine.id, "edited in time");
    let edited = row_when(&env, c, mine.id, "the edit", |r| r.revision == 1).await;
    assert_eq!(edited.body, Body::text("edited in time"));

    // At the end: not offered, and the command is refused with the same reason.
    env.clock.set(u64::try_from(end).expect("time"));
    let p = page(&env, c).await;
    assert_eq!(p.now, end);
    let after = p
        .rows
        .iter()
        .find(|r| r.id == mine.id)
        .expect("own")
        .actions
        .clone();
    assert!(!after.edit && !after.delete_for_everyone);
    assert!(after.delete_for_me && after.reply && after.react && after.copy);
    assert_eq!(after.until, None);
    edit(&env, mine.id, "too late");
    let (error, subject) = failed(&mut env, "EditMessage").await;
    assert!(error.contains("too old to edit"), "{error}");
    assert_eq!(subject, ActionSubject::Message(mine.id));
    delete(&env, mine.id, true);
    let (error, subject) = failed(&mut env, "DeleteMessage").await;
    assert!(error.contains("too old to delete"), "{error}");
    assert_eq!(subject.message(), Some(mine.id));
    assert_eq!(
        row(&env, c, mine.id).await.body,
        Body::text("edited in time")
    );
}

/// Item 1. A message nobody has yet says so (`take_back`): both deletes simply remove it, far
/// outside any window, and an edit changes it in place. Neither depends on time: nothing of
/// them reaches the network, so the chat's windows are not theirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_message_is_taken_back() {
    let s = script("queued").step(Step::Conn(ConnState::Offline { retry_at_ms: None }));
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "never left", None);
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, c).len() == 1 }
    })
    .await;
    let id = own_messages(&db, c)[0].id;
    let a = row(&env, c, id).await.actions;
    assert!(a.take_back && a.delete_for_everyone && a.delete_for_me && a.edit);
    assert_eq!(a.until, None, "nothing here depends on time");
    // Days later, far outside every window: still ours to change and to take back.
    env.clock.advance(Duration::from_secs(3 * 24 * 3600));
    let a = row(&env, c, id).await.actions;
    assert!(a.take_back && a.delete_for_everyone && a.delete_for_me && a.edit);
    assert_eq!(a.until, None);
    edit(&env, id, "never left, reworded");
    let reworded = row_when(&env, c, id, "the edit in place", |r| {
        r.body == Body::text("never left, reworded")
    })
    .await;
    assert_eq!(reworded.revision, 0, "the message itself, not a revision");
    delete(&env, id, true);
    until(&env, "the message is gone", move |conn| {
        conn.query_row("SELECT count(*) FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, i64>(0)
        })
        .expect("count")
            == 0
    })
    .await;
    assert_eq!(outbox_len(&db), 0);
    assert!(env.control.transmits(uuid).is_empty(), "nothing was sent");
}

/// Item 1. When the chat's features change, the rows are read again: the core invalidates the
/// timeline, and the flags follow the new features.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_features_invalidate_the_timeline_and_its_flags() {
    let mut read_only = ConversationPatch::new(d_alice());
    let mut features = signal_features(false);
    features.edit = None;
    features.reactions = kourier_model::Support::Unsupported;
    read_only.features = Some(features);
    let s = script("features")
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(10)))
        .deliver(vec![BackendEvent::ConversationUpsert(read_only)]);
    let (mut env, _, _) = linked(s).await;
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "mine", None);
    let mine = sent(&env, c, 1).await;
    let a = row(&env, c, mine.id).await.actions;
    assert!(a.edit && a.react);

    // Everything so far is old news (the first features invalidated the timeline as well).
    while env.events.try_recv().is_ok() {}
    env.clock.advance(Duration::from_secs(10));
    env.event("TimelineInvalidated", |e| match e {
        UiEvent::TimelineInvalidated { conv } if *conv == c => Some(()),
        _ => None,
    })
    .await;
    let a = row(&env, c, mine.id).await.actions;
    assert!(!a.edit && !a.react, "the new features decide");
    assert!(a.delete_for_everyone && a.reply);
    edit(&env, mine.id, "no longer");
    let (error, _) = failed(&mut env, "EditMessage").await;
    assert!(error.contains("does not allow editing"), "{error}");
}

/// Item 2. An own message with attachments: the edit replaces its caption, keeps its
/// attachment row and file, goes to the backend as a `Media` body with the file to send again,
/// and every revision keeps its caption. A chat that takes no caption with the file offers no
/// edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_own_caption_is_edited_and_revisions_keep_the_earlier_ones() {
    let mut first = setup();
    let mut strict = ConversationPatch::new(d_bob());
    let mut features = signal_features(false);
    features.files = [(
        AttachmentKind::Image,
        FileLimits {
            max_size: 0,
            caption: false,
        },
    )]
    .into();
    strict.features = Some(features);
    first.push(BackendEvent::ConversationUpsert(strict));
    let s = Script::new("own-caption", vec![uid(ME), uid(ME_PNI)]).deliver(first);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (c, strict) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    let src = tempfile::tempdir().expect("sources");

    send_files(
        &env,
        c,
        vec![jpeg_file(&src, "one.jpg", 1)],
        Some("first caption"),
        None,
    );
    let mine = sent(&env, c, 1).await;
    let before = row(&env, c, mine.id).await;
    assert!(before.actions.edit, "a caption can be edited");
    let attachment = attachments_of(&before.body)[0].id.expect("attachment id");

    edit(&env, mine.id, "second caption");
    let after = row_when(&env, c, mine.id, "the caption", |r| r.revision == 1).await;
    assert_eq!(caption_of(&after.body), Some("second caption"));
    assert_eq!(attachments_of(&after.body).len(), 1);
    assert_eq!(
        attachments_of(&after.body)[0].id,
        Some(attachment),
        "the attachment row stays"
    );
    assert!(after.edited_at.is_some());

    // The backend gets the message's files again, with the new caption.
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.transmits(uuid)
                .iter()
                .any(|t| t.req.kind.name() == "Edit")
        }),
    )
    .await
    .expect("the edit is transmitted");
    let transmits = env.control.transmits(uuid);
    let sent_edit = transmits
        .iter()
        .find(|t| t.req.kind.name() == "Edit")
        .expect("edit");
    let OutgoingKind::Edit { target, body } = &sent_edit.req.kind else {
        panic!("not an edit");
    };
    assert_eq!(&*target.local, mine.local.as_str());
    assert_eq!(caption_of(body), Some("second caption"));
    let files = attachments_of(body);
    assert_eq!(files.len(), 1);
    assert!(files[0].local.is_some(), "the blob to upload again");
    assert_eq!(files[0].kind, AttachmentKind::Image);

    // An empty text removes the caption; the same edit again changes nothing.
    edit(&env, mine.id, "");
    let cleared = row_when(&env, c, mine.id, "no caption", |r| r.revision == 2).await;
    assert_eq!(caption_of(&cleared.body), None);
    assert_eq!(attachments_of(&cleared.body)[0].id, Some(attachment));
    edit(&env, mine.id, "  ");
    let (error, _) = failed(&mut env, "EditMessage").await;
    assert!(error.contains("did not change"), "{error}");

    // Every revision keeps its caption.
    let captions: Vec<Option<String>> = env.peek(|conn| {
        let mut stmt = conn
            .prepare("SELECT body FROM message_revisions WHERE message_id = ?1 ORDER BY revision")
            .expect("prepare");
        stmt.query_map([mine.id.0], |r| r.get::<_, String>(0))
            .expect("query")
            .map(|b| {
                let body: Body = serde_json::from_str(&b.expect("row")).expect("body");
                assert_eq!(
                    attachments_of(&body).len(),
                    1,
                    "every revision has the file"
                );
                caption_of(&body).map(str::to_owned)
            })
            .collect()
    });
    assert_eq!(
        captions,
        [
            Some("first caption".to_owned()),
            Some("second caption".to_owned()),
            None
        ]
    );
    assert_eq!(
        env.peek(|conn| conn
            .query_row(
                "SELECT count(*) FROM attachments WHERE message_id = ?1",
                [mine.id.0],
                |r| r.get::<_, i64>(0)
            )
            .expect("count")),
        1
    );

    // Where the chat takes no caption with a picture there is nothing to edit.
    send_files(
        &env,
        strict,
        vec![jpeg_file(&src, "two.jpg", 2)],
        None,
        None,
    );
    let bare = sent(&env, strict, 1).await;
    assert!(!row(&env, strict, bare.id).await.actions.edit);
    edit(&env, bare.id, "a caption after all");
    let (error, subject) = failed(&mut env, "EditMessage").await;
    assert!(error.contains("no caption"), "{error}");
    assert_eq!(subject, ActionSubject::Message(bare.id));
}

/// Item 2. An incoming edit of a message with attachments is its new caption, whether the
/// backend sends it as a text or as a media body: the attachment row, its download and its
/// file stay; an empty edit removes the caption; revisions keep the earlier ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incoming_edit_of_a_media_message_replaces_only_the_caption() {
    let photo = NetMsgId {
        author: Some(alice()),
        local: (T0 + 1).to_string().into(),
    };
    let edited = |n: i64, body: Body| BackendEvent::Edited {
        target: MsgTarget::to_id(Some(d_alice()), &photo),
        by: Some(alice()),
        new_local: Some((T0 + n).to_string().into()),
        body,
        edited_at: T0 + n,
        keeps_quote: None,
    };
    let s = script("incoming-caption")
        .deliver(vec![media(
            T0 + 1,
            vec![synthetic_image(640, 480, 5)],
            text("first caption"),
        )])
        .step(Step::WaitAcked)
        // As a media body that lists some other file: only its caption counts.
        .deliver(vec![edited(
            60_000,
            Body::Media {
                attachments: vec![synthetic_file("other.bin", "application/x-other", 10, 1)],
                caption: text("second caption"),
            },
        )])
        // As a plain text.
        .deliver(vec![edited(120_000, Body::text("third caption"))]);
    let (env, account, _) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let id = message(&env, T0 + 1).await;
    let r = row_when(&env, c, id, "both edits", |r| r.revision == 2).await;
    assert_eq!(caption_of(&r.body), Some("third caption"));
    let files = attachments_of(&r.body);
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].kind,
        AttachmentKind::Image,
        "the photo, not the edit's file"
    );
    assert!(files[0].id.is_some());
    let captions: Vec<String> = env.peek(|conn| {
        let mut stmt = conn
            .prepare("SELECT body FROM message_revisions WHERE message_id = ?1 ORDER BY revision")
            .expect("prepare");
        stmt.query_map([id.0], |r| r.get::<_, String>(0))
            .expect("query")
            .map(|b| {
                let body: Body = serde_json::from_str(&b.expect("row")).expect("body");
                assert_eq!(attachments_of(&body)[0].kind, AttachmentKind::Image);
                caption_of(&body).unwrap_or_default().to_owned()
            })
            .collect()
    });
    assert_eq!(
        captions,
        ["first caption", "second caption", "third caption"]
    );
    // One attachment row, and the photo the policy fetched is still (or becomes) local.
    until(&env, "the photo is local", move |conn| {
        conn.query_row(
            "SELECT count(*), max(file_id IS NOT NULL) FROM attachments WHERE message_id = ?1",
            [id.0],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
        )
        .expect("row")
            == (1, true)
    })
    .await;
}

/// Item 2, in any order and twice: an edit that arrives before its media message, an empty
/// edit and a replay all end in the same rows.
#[test]
fn media_edits_converge() {
    let photo = NetMsgId {
        author: Some(alice()),
        local: "100".into(),
    };
    let edit = |local: &str, at: i64, body: Body| BackendEvent::Edited {
        target: MsgTarget::to_id(Some(d_alice()), &photo),
        by: Some(alice()),
        new_local: Some(local.into()),
        body,
        edited_at: at,
        keeps_quote: None,
    };
    let mut message = remote(&d_alice(), &alice(), "100", 100, "");
    message.body = Body::Media {
        attachments: vec![synthetic_image(64, 64, 1)],
        caption: text("one"),
    };
    let events = [
        BackendEvent::Message(message),
        edit("200", 200, Body::text("two")),
        edit("300", 300, Body::text("")),
    ];
    let dir = tempfile::tempdir().expect("dir");
    let run = |name: &str, order: &[usize], twice: bool| {
        let mut h = IngestHarness::open(
            &dir.path().join(name),
            &kourier_db::DbKey::from(CORE_KEY),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid(ME)],
        )
        .expect("harness");
        for _ in 0..if twice { 2 } else { 1 } {
            for i in order {
                h.apply(vec![events[*i].clone()]).expect("apply");
            }
        }
        let (body, revision): (String, i64) = h
            .connection()
            .query_row("SELECT body, revision FROM messages", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("row");
        let body: Body = serde_json::from_str(&body).expect("body");
        assert_eq!(
            caption_of(&body),
            None,
            "{name}: the empty edit is the latest"
        );
        assert_eq!(attachments_of(&body).len(), 1, "{name}");
        assert_eq!(revision, 2, "{name}");
        h.dump(DumpMode::OrderInsensitive).expect("dump")
    };
    let straight = run("a.db", &[0, 1, 2], false);
    assert_eq!(run("b.db", &[2, 1, 0], false), straight);
    assert_eq!(run("c.db", &[1, 0, 2], true), straight);
}

/// Item 3. `SendFiles` quotes like `SendText`; a row's quote is resolved for display: who,
/// what text, which kind of attachment with a thumbnail handle, and whether the original is
/// here, missing or deleted (with a row to jump to only when one is visible).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_carry_a_quote_the_ui_can_draw() {
    let mut named = ContactPatch::new(alice());
    named.profile_name = Patch::Set("Alice Arden".into());
    let mut missing = remote(
        &d_alice(),
        &alice(),
        &(T0 + 5).to_string(),
        T0 + 5,
        "found it",
    );
    missing.reply_to = Some(ReplyTo {
        target: MsgTarget {
            conv: Some(d_alice()),
            author: TargetAuthor::Own,
            local: "1".into(),
            alternates: Vec::new(),
        },
        author: None,
        snapshot: Some(QuoteSnapshot {
            text: text("long gone"),
            attachment: Some(AttachmentKind::Voice),
            ..QuoteSnapshot::default()
        }),
    });
    let s = script("replies").deliver(vec![
        BackendEvent::ContactUpsert(named),
        media(T0 + 1, vec![synthetic_image(640, 480, 9)], text("look")),
        BackendEvent::Message(missing),
    ]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let asked = message(&env, T0 - 60_000).await;
    let photo = message(&env, T0 + 1).await;
    let src = tempfile::tempdir().expect("sources");

    // Files that quote a text.
    send_files(
        &env,
        c,
        vec![jpeg_file(&src, "here.jpg", 3)],
        Some("here"),
        Some(asked),
    );
    let files = sent(&env, c, 1).await;
    let quote = row(&env, c, files.id).await.reply.expect("quote");
    assert_eq!(quote.state, QuoteState::Known);
    assert_eq!(quote.target, Some(asked), "a row to jump to");
    assert_eq!(quote.author, Some(alice()));
    assert!(!quote.own);
    assert_eq!(
        quote.author_name.as_ref().map(|n| n.expose().as_str()),
        Some("Alice Arden")
    );
    assert_eq!(
        quote.text.as_ref().map(|t| t.body.as_str()),
        Some("are you there?")
    );
    assert_eq!((quote.attachment, quote.thumb), (None, None));
    let transmits = env.control.transmits(uuid);
    let OutgoingKind::Message { reply_to, body, .. } = &transmits[0].req.kind else {
        panic!("not a message");
    };
    assert!(matches!(body, Body::Media { .. }));
    let sent_quote = reply_to.as_ref().expect("the request carries the quote");
    assert_eq!(
        &*sent_quote.target.local,
        (T0 - 60_000).to_string().as_str()
    );
    assert_eq!(sent_quote.target.author, TargetAuthor::User(alice()));
    assert_eq!(
        sent_quote
            .snapshot
            .as_ref()
            .and_then(|s| s.text.as_ref())
            .map(|t| t.body.as_str()),
        Some("are you there?")
    );

    // A text that quotes a photo: kind, MIME type and (once derived) the thumbnail, which
    // only the request carries.
    until(&env, "the photo's thumbnail", move |conn| {
        conn.query_row(
            "SELECT thumb_file_id IS NOT NULL FROM attachments WHERE message_id = ?1",
            [photo.0],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
    })
    .await;
    send(&env, c, "nice one", Some(photo));
    let nice = sent(&env, c, 2).await;
    let quote = row(&env, c, nice.id).await.reply.expect("quote");
    assert_eq!(quote.state, QuoteState::Known);
    assert_eq!(quote.target, Some(photo));
    assert_eq!(quote.attachment, Some(AttachmentKind::Image));
    assert_eq!(quote.text.as_ref().map(|t| t.body.as_str()), Some("look"));
    let photo_attachment: i64 = env.peek(|conn| {
        conn.query_row(
            "SELECT id FROM attachments WHERE message_id = ?1",
            [photo.0],
            |r| r.get(0),
        )
        .expect("attachment")
    });
    assert_eq!(quote.thumb, Some(AttachmentId(photo_attachment)));
    assert!(
        env.handle
            .media()
            .get(AttachmentId(photo_attachment))
            .is_some(),
        "the image provider has the handle"
    );
    let transmits = env.control.transmits(uuid);
    let OutgoingKind::Message { reply_to, .. } = &transmits[1].req.kind else {
        panic!("not a message");
    };
    let snapshot = reply_to
        .as_ref()
        .and_then(|r| r.snapshot.as_ref())
        .expect("snapshot");
    assert_eq!(snapshot.attachment, Some(AttachmentKind::Image));
    assert_eq!(snapshot.mime.as_deref(), Some("image/jpeg"));
    assert!(
        snapshot.thumb.is_some(),
        "the thumbnail blob for the network"
    );
    let stored: String = env.peek(|conn| {
        conn.query_row(
            "SELECT reply_to FROM messages WHERE id = ?1",
            [nice.id.0],
            |r| r.get(0),
        )
        .expect("reply_to")
    });
    assert!(!stored.contains("thumb"), "no blob key in the message row");

    // A quote of a message that is not here: the sender's snapshot is all there is.
    let orphan = message(&env, T0 + 5).await;
    let quote = row(&env, c, orphan).await.reply.expect("quote");
    assert_eq!(quote.state, QuoteState::Missing);
    assert_eq!(quote.target, None);
    assert!(quote.own, "it claims to quote us");
    assert_eq!(
        quote.text.as_ref().map(|t| t.body.as_str()),
        Some("long gone")
    );
    assert_eq!(quote.attachment, Some(AttachmentKind::Voice));
    assert_eq!(quote.thumb, None);

    // The original is deleted for everyone: the tombstone is still a row to jump to.
    send(&env, c, "about my files", Some(files.id));
    let about = sent(&env, c, 3).await;
    assert_eq!(
        row(&env, c, about.id).await.reply.expect("quote").state,
        QuoteState::Known
    );
    delete(&env, files.id, true);
    let r = row_when(&env, c, about.id, "the deleted original", |r| {
        r.reply
            .as_ref()
            .is_some_and(|q| q.state == QuoteState::Deleted)
    })
    .await;
    let quote = r.reply.expect("quote");
    assert_eq!(quote.target, Some(files.id));
    assert!(quote.own);
    assert_eq!(quote.thumb, None);
    // Deleted for me as well: nothing to jump to.
    delete(&env, files.id, false);
    row_when(&env, c, about.id, "the hidden original", |r| {
        r.reply
            .as_ref()
            .is_some_and(|q| q.state == QuoteState::Deleted && q.target.is_none())
    })
    .await;

    // A deleted message cannot be quoted, and its flag said so.
    delete(&env, photo, false);
    until(&env, "the photo is deleted", move |conn| {
        conn.query_row(
            "SELECT deleted IS NOT NULL FROM messages WHERE id = ?1",
            [photo.0],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
    })
    .await;
    send(&env, c, "too late", Some(photo));
    let (error, subject) = failed(&mut env, "SendText").await;
    assert!(error.contains("quoted message is deleted"), "{error}");
    assert_eq!(subject, ActionSubject::Conversation(c));
    assert_eq!(
        own_messages(&env.core_db(), c).len(),
        3,
        "nothing was queued"
    );
}

/// Item 3. A quote is resolved in its own conversation only. A member of a group who names a
/// message of another chat (its author and id are all a quote needs) gets a quote of something
/// that is not here: no row to jump to, and nothing of that message (its text, its picture)
/// shown in the quote block. The same quote inside the chat of the message resolves.
#[test]
fn a_quote_of_a_message_in_another_chat_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from(CORE_KEY),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let quoting = |conv: &ConvKey, by: &kourier_model::UserId, local: &str| {
        let mut m = remote(conv, by, local, 2_000, "what about this");
        m.reply_to = Some(ReplyTo {
            target: MsgTarget {
                conv: Some(conv.clone()),
                author: TargetAuthor::User(alice()),
                local: "1000".into(),
                alternates: Vec::new(),
            },
            author: Some(alice()),
            // No snapshot: the block would be filled from the original.
            snapshot: None,
        });
        BackendEvent::Message(m)
    };
    h.apply(vec![
        media(1_000, vec![synthetic_image(64, 48, 3)], text("between us")),
        quoting(&group(), &bob(), "2000"),
        quoting(&d_alice(), &alice(), "2001"),
    ])
    .expect("apply");
    let quote_in = |key: &ConvKey| {
        let conv: i64 = h
            .connection()
            .query_row(
                "SELECT id FROM conversations WHERE conv_key = ?1",
                [key.encode()],
                |r| r.get(0),
            )
            .expect("conversation");
        let page = h
            .timeline(&TimelineQuery {
                conv: ConversationId(conv),
                anchor: TimelineAnchor::Newest,
                limit: 10,
            })
            .expect("timeline");
        page.rows
            .into_iter()
            .find_map(|r| r.reply)
            .expect("the quoting row")
    };

    let elsewhere = quote_in(&group());
    assert_eq!(elsewhere.state, QuoteState::Missing);
    assert_eq!(elsewhere.target, None, "no jump into another chat");
    assert_eq!(
        (elsewhere.text, elsewhere.attachment, elsewhere.thumb),
        (None, None, None),
        "nothing of the other chat's message is shown"
    );
    assert_eq!(elsewhere.file_name, None);

    let here = quote_in(&d_alice());
    assert_eq!(here.state, QuoteState::Known);
    assert!(here.target.is_some());
    assert_eq!(
        here.text.as_ref().map(|t| t.body.as_str()),
        Some("between us")
    );
    assert_eq!(here.attachment, Some(AttachmentKind::Image));
    assert!(here.thumb.is_some());
}

/// Item 4. Failures name what they are about: the attachment of an open or a download, the
/// message of an edit, and a refused `SendFiles` hands its whole request back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_name_their_subject() {
    let (mut env, account, _) = linked(script("subjects")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let asked = message(&env, T0 - 60_000).await;

    env.handle
        .send(UiCommand::OpenMedia {
            attachment: AttachmentId(9999),
        })
        .expect("send");
    let (_, subject) = failed(&mut env, "OpenMedia").await;
    assert_eq!(subject, ActionSubject::Attachment(AttachmentId(9999)));
    assert_eq!(subject.attachment(), Some(AttachmentId(9999)));

    env.handle
        .send(UiCommand::DownloadAttachment {
            attachment: AttachmentId(9998),
        })
        .expect("send");
    let (_, subject) = failed(&mut env, "DownloadAttachment").await;
    assert_eq!(subject, ActionSubject::Attachment(AttachmentId(9998)));

    let src = tempfile::tempdir().expect("sources");
    let paths = vec![
        jpeg_file(&src, "holiday.jpg", 4),
        src.path().join("not-there.bin"),
    ];
    send_files(&env, c, paths.clone(), Some("for you"), Some(asked));
    let event = env
        .event("SendFiles", |e| match e {
            UiEvent::ActionFailed {
                action: "SendFiles",
                ..
            } => Some(e.clone()),
            _ => None,
        })
        .await;
    let shown = format!("{event:?}");
    assert!(
        !shown.contains("holiday") && !shown.contains("for you"),
        "paths and captions never print: {shown}"
    );
    let UiEvent::ActionFailed { subject, .. } = event else {
        unreachable!()
    };
    let ActionSubject::Files(refused) = subject else {
        panic!("not the files: {subject:?}");
    };
    assert_eq!(refused.conv, c);
    assert_eq!(refused.paths.expose(), &paths, "the file strip comes back");
    assert_eq!(
        refused.caption.as_ref().map(|t| t.body.as_str()),
        Some("for you")
    );
    assert_eq!(refused.reply_to, Some(asked));
    assert!(
        own_messages(&env.core_db(), c).is_empty(),
        "nothing was queued"
    );
}

/// Item 4. A reaction the network refuses is reported with its message; when the user deleted
/// that message while the reaction was on the way, nothing is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_for_a_message_the_user_deleted_is_not_reported() {
    let refused = BackendError::new(ErrorKind::InvalidInput, "refused by the server");
    let wait = Duration::from_secs(60);
    let s = script("late-refusal")
        .fail_transmit(TransmitRule::attempts(refused, 2).of_kind("React"))
        .delay_reply(d_alice(), wait);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let asked = message(&env, T0 - 60_000).await;
    let react = |env: &Env, message: MessageId| {
        env.handle
            .send(UiCommand::React {
                message,
                emoji: "👍".into(),
                remove: false,
            })
            .expect("send");
    };
    // The fake holds every answer in this chat for a minute on its clock.
    let answer = |env: &Env, n: usize| {
        let (control, clock) = (env.control.clone(), env.clock.clone());
        async move {
            tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= n))
                .await
                .expect("the transmit");
            eventually("the held answer", WAIT, || {
                let clock = clock.clone();
                async move { clock.sleepers() > 0 }
            })
            .await;
            clock.advance(wait);
        }
    };

    // Reported, with the message it was about.
    react(&env, asked);
    answer(&env, 1).await;
    let (error, subject) = failed(&mut env, "React").await;
    assert!(error.contains("refused by the server"), "{error}");
    assert_eq!(subject, ActionSubject::Message(asked));

    // Deleted while the second reaction is on the way: its refusal is not reported.
    react(&env, asked);
    tokio::time::timeout(WAIT, env.control.wait_for(|c| c.transmits(uuid).len() >= 2))
        .await
        .expect("the reaction is on the way");
    delete(&env, asked, false);
    until(&env, "the tombstone", move |conn| {
        conn.query_row(
            "SELECT deleted = 'ForMe' FROM messages WHERE id = ?1",
            [asked.0],
            |r| r.get::<_, bool>(0),
        )
        .unwrap_or(false)
    })
    .await;
    answer(&env, 2).await;
    until(&env, "the refused reaction is recorded", |conn| {
        conn.query_row(
            "SELECT count(*) FROM outbox WHERE kind = 'React'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
            == 0
    })
    .await;
    // The next failure the UI hears of is this marker, not the reaction.
    env.handle
        .send(UiCommand::OpenMedia {
            attachment: AttachmentId(4242),
        })
        .expect("send");
    let next = env
        .event("the next failure", |e| match e {
            UiEvent::ActionFailed { action, .. } => Some(*action),
            _ => None,
        })
        .await;
    assert_eq!(next, "OpenMedia");
}

/// Item 5. The list preview says what kind of attachment the newest message is and which
/// system event happened; the account and the authors of a group have their own avatars when
/// the backend supplied them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn previews_say_what_happened_and_people_have_their_avatars() {
    let avatar = |name: &str, key: u8| BlobRef {
        path: format!("avatars/{name}").into(),
        key: BlobKey::new([key; 32]),
    };
    let with_avatar = |user, name: &str, key: u8| {
        let mut p = ContactPatch::new(user);
        p.avatar = Patch::Set(avatar(name, key));
        BackendEvent::ContactUpsert(p)
    };
    let voice = Attachment {
        kind: AttachmentKind::Voice,
        mime: "audio/aac".into(),
        size: 4000,
        duration_ms: Some(3000),
        remote: Some(Pointer::Bytes { len: 4000, seed: 3 }.to_blob()),
        ..Attachment::default()
    };
    let mut once = remote(&d_bob(), &bob(), &(T0 + 3).to_string(), T0 + 3, "");
    once.body = Body::Unsupported {
        kind_hint: "view-once".into(),
        summary: None,
    };
    let said = remote(
        &group(),
        &alice(),
        &(T0 + 4).to_string(),
        T0 + 4,
        "hello all",
    );
    let mut removed = remote(&group(), &alice(), &(T0 + 5).to_string(), T0 + 5, "");
    removed.body = Body::System(SystemEvent::MembersRemoved {
        by: Some(alice()),
        users: vec![bob()],
    });
    let s = script("previews").deliver(vec![
        with_avatar(alice(), "alice", 1),
        with_avatar(uid(ME), "me", 2),
        media(T0 + 2, vec![voice, synthetic_image(64, 64, 2)], None),
        BackendEvent::Message(once),
        BackendEvent::Message(said),
        BackendEvent::Message(removed),
    ]);
    let (env, account, _) = linked(s).await;
    env.wait_finished(account).await;

    let me = env.account(account).await.expect("account");
    assert_eq!(
        me.avatar,
        Some(avatar("me", 2)),
        "the account's own picture"
    );

    let g = conv(&env, &group()).await;
    let said = message(&env, T0 + 4).await;
    let r = row(&env, g, said).await;
    assert_eq!(
        r.author_avatar,
        Some(avatar("alice", 1)),
        "the author's own picture"
    );

    let rows = env.conversations().await;
    let preview = |key: &ConvKey| {
        let id = conv_id(&env.core_db(), key).expect("conversation");
        rows.iter()
            .find(|r| r.id == id)
            .and_then(|r| r.preview.clone())
            .expect("preview")
    };
    let p = preview(&d_alice());
    assert_eq!(p.body_kind, "Media");
    assert_eq!(
        p.attachment,
        Some(AttachmentKind::Voice),
        "a voice note, not a file"
    );
    assert_eq!(p.attachments, 2);
    assert_eq!((p.system.clone(), p.kind_hint.clone()), (None, None));
    let p = preview(&group());
    assert_eq!(
        p.system,
        Some(SystemEvent::MembersRemoved {
            by: Some(alice()),
            users: vec![bob()],
        }),
        "which event, not just that something changed"
    );
    assert_eq!(p.attachment, None);
    let p = preview(&d_bob());
    assert_eq!(p.kind_hint.as_deref(), Some("view-once"));
    // Our own messages have no author avatar.
    let c = conv(&env, &d_alice()).await;
    send(&env, c, "mine", None);
    let mine = sent(&env, c, 1).await;
    assert_eq!(row(&env, c, mine.id).await.author_avatar, None);
}

/// Item 6. `SetConversationTimer` reaches the backend as a `ChangeConversation` with an id of
/// the core's allocator; the backend confirms with the patch (timer and version) and the
/// timeline row of the change; messages sent after it carry the timer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timer_change_is_confirmed_with_its_timeline_row() {
    let (mut env, account, uuid) = linked(script("timer")).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let timer_of = |env: &Env| {
        let h = env.handle.clone();
        async move {
            match h.query(Query::Conversation(c)).await.expect("detail") {
                QueryData::Conversation(Some(d)) => d.timer,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    let set = |env: &Env, timer: Option<u64>| {
        env.handle
            .send(UiCommand::SetConversationTimer {
                conv: c,
                timer: timer.map(Duration::from_secs),
            })
            .expect("send");
    };
    let notices = |rows: &[MessageRow]| -> Vec<(Option<Duration>, MessageId)> {
        rows.iter()
            .filter_map(|r| match &r.body {
                Body::System(SystemEvent::TimerChanged { by: None, timer }) => Some((*timer, r.id)),
                _ => None,
            })
            .collect()
    };

    // A message of the same millisecond (the clock stands still) keeps its own id: the timer
    // row gets the next one from the same allocator.
    send(&env, c, "before", None);
    let before = sent(&env, c, 1).await;
    set(&env, Some(3600));
    eventually("the timer", WAIT, || {
        let t = timer_of(&env);
        async move { t.await == Some(Duration::from_secs(3600)) }
    })
    .await;
    let rows = page(&env, c).await.rows;
    let found = notices(&rows);
    assert_eq!(found.len(), 1, "one timeline row for our change");
    assert_eq!(found[0].0, Some(Duration::from_secs(3600)));
    assert_ne!(found[0].1, before.id);
    let notice = rows.iter().find(|r| r.id == found[0].1).expect("notice");
    assert_eq!(notice.out_state, None, "a notice is not a sent message");
    let a = notice.actions.clone();
    assert!(a.delete_for_me && !a.delete_for_everyone && !a.edit && !a.react && !a.reply);
    let version = |env: &Env| -> Option<i64> {
        env.peek(|conn| {
            conn.query_row(
                "SELECT timer_version FROM conversations WHERE id = ?1",
                [c.0],
                |r| r.get(0),
            )
            .expect("version")
        })
    };
    assert_eq!(version(&env), Some(1), "the version the backend stated");

    // The next message carries the timer, in its row and in its request.
    send(&env, c, "after", None);
    // (The notice is the second row of our own in this chat.)
    let after = sent(&env, c, 3).await;
    let r = row(&env, c, after.id).await;
    assert_eq!(r.expiry.map(|e| e.timer), Some(Duration::from_secs(3600)));
    let transmits = env.control.transmits(uuid);
    let expiries: Vec<Option<Duration>> = transmits
        .iter()
        .filter_map(|t| match &t.req.kind {
            OutgoingKind::Message { expiry, .. } => Some(*expiry),
            _ => None,
        })
        .collect();
    assert_eq!(expiries, [None, Some(Duration::from_secs(3600))]);

    // Off again: a second row, the next version.
    set(&env, None);
    eventually("no timer", WAIT, || {
        let t = timer_of(&env);
        async move { t.await.is_none() }
    })
    .await;
    let found = notices(&page(&env, c).await.rows);
    assert_eq!(
        found.iter().map(|n| n.0).collect::<Vec<_>>(),
        [Some(Duration::from_secs(3600)), None]
    );
    assert_eq!(version(&env), Some(2));
    assert_eq!(
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "ChangeConversation")
            .count(),
        2
    );

    // A timer the chat does not take (longer than its range) never reaches the backend.
    set(
        &env,
        Some(u64::from(kourier_fake::scripts::TIMER_MAX_SECS) + 1),
    );
    let (error, subject) = failed(&mut env, "SetConversationTimer").await;
    assert!(error.contains("does not offer"), "{error}");
    assert_eq!(subject, ActionSubject::Conversation(c));
    assert_eq!(notices(&page(&env, c).await.rows).len(), 2);
}

/// Item 6. Timer changes that arrive out of order end in the same state: the network's version
/// decides, and the row of a stale change is still a row.
#[test]
fn timer_versions_order_timer_changes() {
    let change = |secs: Option<u64>, version: Option<u32>, local: &str| {
        let mut patch = ConversationPatch::new(d_alice());
        patch.timer = secs.map_or(Patch::Clear, |s| Patch::Set(Duration::from_secs(s)));
        patch.timer_version = version;
        let at = local.parse::<i64>().expect("timestamp");
        let mut notice = remote(&d_alice(), &alice(), local, at, "");
        notice.body = Body::System(SystemEvent::TimerChanged {
            by: Some(alice()),
            timer: secs.map(Duration::from_secs),
        });
        vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(notice),
        ]
    };
    let dir = tempfile::tempdir().expect("dir");
    let open = |name: &str| {
        IngestHarness::open(
            &dir.path().join(name),
            &kourier_db::DbKey::from(CORE_KEY),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid(ME)],
        )
        .expect("harness")
    };
    let timer = |h: &IngestHarness| -> (Option<i64>, Option<i64>) {
        h.connection()
            .query_row(
                "SELECT timer_secs, timer_version FROM conversations",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("timer")
    };
    let (older, newer) = (
        change(Some(86_400), Some(3), "1000"),
        change(Some(30), Some(5), "2000"),
    );

    let mut a = open("a.db");
    a.apply(older.clone()).expect("apply");
    a.apply(newer.clone()).expect("apply");
    assert_eq!(timer(&a), (Some(30), Some(5)));
    let mut b = open("b.db");
    b.apply(newer).expect("apply");
    b.apply(older).expect("apply");
    assert_eq!(
        timer(&b),
        (Some(30), Some(5)),
        "the stale change does not win"
    );
    assert_eq!(
        a.dump(DumpMode::OrderInsensitive).expect("dump"),
        b.dump(DumpMode::OrderInsensitive).expect("dump"),
        "both orders end the same, with both timeline rows"
    );

    // The same version again (a redelivery) applies; a newer one clears; without a version
    // the last patch wins and the version stays.
    b.apply(change(Some(60), Some(5), "3000")).expect("apply");
    assert_eq!(timer(&b), (Some(60), Some(5)));
    b.apply(change(None, Some(6), "4000")).expect("apply");
    assert_eq!(timer(&b), (None, Some(6)));
    b.apply(change(Some(3600), None, "5000")).expect("apply");
    assert_eq!(timer(&b), (Some(3600), Some(6)));

    // A change from another own device is a notice too, not a message we sent: it has no
    // outgoing state, whatever send states its transcript carries.
    let mut ours = kourier_fake::scripts::own(&d_alice(), "6000", 6000, "");
    ours.body = Body::System(SystemEvent::TimerChanged {
        by: None,
        timer: None,
    });
    let target = MsgTarget::to_id(Some(d_alice()), &ours.net_id());
    b.apply(vec![
        BackendEvent::Message(ours),
        BackendEvent::OwnSendState {
            local: None,
            target,
            per_recipient: vec![(
                alice(),
                kourier_model::RecipientState::Sent { sealed: true },
            )],
        },
    ])
    .expect("apply");
    let state: Option<String> = b
        .connection()
        .query_row(
            "SELECT out_state FROM messages WHERE net_local = '6000'",
            [],
            |r| r.get(0),
        )
        .expect("row");
    assert_eq!(state, None);
}

/// Item 7. A message whose body arrived trimmed, with the whole text as an attachment the
/// backend could not fetch: the core downloads it (also with file downloads off), the body
/// becomes whole and the attachment goes; other attachments keep their place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_text_downloaded_later_becomes_the_body() {
    let whole = "all of it, word for word. ".repeat(300);
    let captioned = "a long caption under a photo. ".repeat(200);
    let trimmed = |s: &str| text(&s[..2048]);
    let s = script("long-text").deliver(vec![
        media(T0 + 1, vec![synthetic_long_text(&whole)], trimmed(&whole)),
        media(
            T0 + 2,
            vec![
                synthetic_long_text(&captioned),
                synthetic_image(320, 240, 6),
            ],
            trimmed(&captioned),
        ),
    ]);
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    let c = conv(&env, &d_alice()).await;
    let (plain, photo) = (message(&env, T0 + 1).await, message(&env, T0 + 2).await);

    let r = row_when(
        &env,
        c,
        plain,
        "the whole text",
        |r| matches!(&r.body, Body::Text(t) if t.body.len() > 2048),
    )
    .await;
    assert_eq!(r.body, Body::text(whole.clone()), "a text message again");
    assert!(r.actions.copy);

    let r = row_when(&env, c, photo, "the whole caption", |r| {
        caption_of(&r.body).is_some_and(|c| c.len() > 2048)
    })
    .await;
    assert_eq!(caption_of(&r.body), Some(captioned.as_str()));
    let files = attachments_of(&r.body);
    assert_eq!(files.len(), 1, "only the photo is left");
    assert_eq!(files[0].kind, AttachmentKind::Image);
    assert!(!files[0].long_text);
    assert!(files[0].id.is_some(), "the photo's row moved up with it");

    // No row and no file is left of either long text.
    until(&env, "the long texts are gone", move |conn| {
        let rows: (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM attachments),
                        (SELECT count(*) FROM attachments WHERE long_text = 1 OR idx <> 0),
                        (SELECT count(*) FROM files WHERE purpose = 'attachment')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("counts");
        rows == (1, 0, 1)
    })
    .await;
    let row = env
        .conversations()
        .await
        .into_iter()
        .find(|r| r.id == c)
        .expect("conversation");
    let preview = row.preview.expect("preview");
    assert_eq!(
        (preview.attachment, preview.attachments),
        (Some(AttachmentKind::Image), 1)
    );
}
