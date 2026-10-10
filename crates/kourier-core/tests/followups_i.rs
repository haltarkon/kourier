//! Phase I follow-ups in the core (architecture §4.3, §4.8, §4.9, §5.8), through the real
//! core on the fake backend with the fake clock:
//!
//! - a message that reached some recipients and failed for the others goes when the phone
//!   deletes its chat, and the chat comes back clean; one nobody has stays;
//! - the file of a download that finishes after its message was deleted is removed when the
//!   backend answers, not at the next start;
//! - a "Retry" after the phone deleted one file of a partly sent message sends the message
//!   as it is now;
//! - the long text of a message that arrived in an unanswered message request is fetched
//!   when the request is accepted;
//! - a login cancelled, and an account removed, right after the link finished on the network
//!   logs the device out again;
//! - an account id is not handed out again, also not after a restart;
//! - a thumbnail that a power loss cut off or lost (thumbnails are written without fsync) is
//!   made again after the next start.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, OutgoingKind};
use kourier_core::AccountVault;
use kourier_core::facade::{UiCommand, UiEvent};
use kourier_fake::attachments::{synthetic_image, synthetic_long_text};
use kourier_fake::scripts::{T0, attachment_name, remote, target};
use kourier_fake::{Flavor, LoginScript, Script, Step, TransmitRule};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountId, AccountState, AccountUuid, Attachment, Body, ConvKey, ConversationId,
    ConversationPatch, MessageId, NetworkKind, OutgoingState, Text,
};
use support::send::*;
use support::{Env, WAIT, add_account, add_linked, eventually};

const T0U: u64 = T0 as u64;

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let (db, k) = (env.core_db(), key.clone());
    eventually("the conversation", WAIT, || {
        let (db, k) = (db.clone(), k.clone());
        async move { conv_id(&db, &k).is_some() }
    })
    .await;
    conv_id(&env.core_db(), key).expect("conversation")
}

fn send(env: &Env, conv: ConversationId, body: &str) {
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain(body),
            reply_to: None,
        })
        .expect("send");
}

async fn queued(env: &Env, conv: ConversationId, n: usize) -> Own {
    let db = env.core_db();
    eventually("the queued message", WAIT, || {
        let db = db.clone();
        async move { own_messages(&db, conv).len() >= n }
    })
    .await;
    own_messages(&db, conv)[n - 1].clone()
}

async fn sleeping_until(env: &Env, at: u64) {
    let clock = env.clock.clone();
    eventually(&format!("a sleep until {at}"), WAIT, || {
        let clock = clock.clone();
        async move { clock.deadlines().contains(&at) }
    })
    .await;
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(200)).await;
}

fn send_row(env: &Env, message: MessageId) -> Option<String> {
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
            [message.0],
            |r| r.get(0),
        )
        .ok()
    })
}

fn count(env: &Env, sql: &str) -> i64 {
    core_db(&env.core_db(), |c| {
        c.query_row(sql, [], |r| r.get(0)).expect(sql)
    })
}

async fn listed(env: &Env) -> Vec<ConversationId> {
    env.conversations()
        .await
        .into_iter()
        .map(|c| c.id)
        .collect()
}

/// The texts a timeline shows.
async fn shown(env: &Env, conv: ConversationId) -> Vec<String> {
    env.timeline(conv)
        .await
        .into_iter()
        .map(|r| match r.body {
            Body::Text(t) => t.body,
            other => other.kind_name().to_owned(),
        })
        .collect()
}

/// Every file below `dir`.
fn files_below(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_below(&p));
        } else {
            out.push(p);
        }
    }
    out
}

// ---------- a chat cleared from the phone ----------

/// A group message reached bob and failed for good for alice: it shows as partly failed,
/// and its row is kept so the user can retry alice. The phone, which has the message,
/// deletes the chat. The message goes like any sent message (its row was only kept for a
/// retry nobody asked for), the chat leaves the list, and when somebody writes there again
/// it comes back with that message alone. A message nobody has is different: the phone
/// never had it, so a delete made there does not take the user's text away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partly_failed_message_goes_when_its_chat_is_deleted_on_the_phone() {
    // The core clock stands at T0 until the sends are stored: the group message's id is T0.
    let local = T0.to_string();
    let s = script("clear-partly-failed")
        .fail_transmit(
            TransmitRule::member(alice(), unregistered(), 1)
                .of_kind("Message")
                .in_conv(group()),
        )
        .fail_transmit(
            TransmitRule::attempts(unregistered(), 1)
                .of_kind("Message")
                .in_conv(d_bob()),
        )
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver_ordered(vec![
            BackendEvent::ConversationCleared {
                conv: group(),
                up_to: vec![target(Some(&group()), None, &local)],
                remove: true,
                at: T0 + 5_000,
            },
            BackendEvent::ConversationCleared {
                conv: d_bob(),
                up_to: Vec::new(),
                remove: true,
                at: T0 + 5_001,
            },
        ])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver(vec![BackendEvent::Message(remote(
            &group(),
            &bob(),
            &(T0 + 10_000).to_string(),
            T0 + 10_000,
            "anyone there?",
        ))]);
    let (mut env, account, _uuid) = linked(s).await;
    let (g, b) = (conv(&env, &group()).await, conv(&env, &d_bob()).await);
    sleeping_until(&env, T0U + 5_000).await;
    send(&env, g, "to the group");
    let partly = queued(&env, g, 1).await;
    assert_eq!(partly.local, local);
    let db = env.core_db();
    eventually("partly failed", WAIT, || {
        let db = db.clone();
        async move {
            matches!(
                own_messages(&db, g)[0].out_state,
                Some(OutgoingState::PartiallyFailed { .. })
            )
        }
    })
    .await;
    assert_eq!(
        send_row(&env, partly.id).as_deref(),
        Some("Failed"),
        "kept for a retry of alice"
    );
    send(&env, b, "to nobody");
    let unsent = queued(&env, b, 1).await;
    eventually("refused for everyone", WAIT, || {
        let db = db.clone();
        async move {
            matches!(
                own_messages(&db, b)[0].out_state,
                Some(OutgoingState::Failed { .. })
            )
        }
    })
    .await;

    // The phone deletes both chats.
    env.clock.advance(Duration::from_secs(5));
    eventually("the group is gone from the list", WAIT, || {
        let env = &env;
        async move { !listed(env).await.contains(&g) }
    })
    .await;
    assert_eq!(
        count(
            &env,
            &format!(
                "SELECT count(*) FROM messages WHERE conversation_id = {}",
                g.0
            )
        ),
        0,
        "the partly failed message went with the chat"
    );
    assert_eq!(send_row(&env, partly.id), None);
    assert!(
        listed(&env).await.contains(&b),
        "what the user wrote and nobody has is not lost to a delete made elsewhere"
    );
    assert_eq!(shown(&env, b).await, ["to nobody"]);
    assert_eq!(send_row(&env, unsent.id).as_deref(), Some("Failed"));

    // Bob writes to the group again: it is back, with his message alone.
    sleeping_until(&env, T0U + 10_000).await;
    env.clock.advance(Duration::from_secs(5));
    env.wait_finished(account).await;
    eventually("the group is back", WAIT, || {
        let env = &env;
        async move { listed(env).await.contains(&g) }
    })
    .await;
    assert_eq!(shown(&env, g).await, ["anyone there?"]);
    env.stop().await;
}

// ---------- attachments ----------

fn media(local: i64, attachments: Vec<Attachment>, caption: Option<&str>) -> BackendEvent {
    let mut m = remote(&d_alice(), &alice(), &local.to_string(), local, "");
    m.body = Body::Media {
        attachments,
        caption: caption.map(Text::plain),
    };
    BackendEvent::Message(m)
}

/// Alice sends a photo and takes it back a second later, while the download is still
/// running (the network needs five seconds for it). The row and its download target go at
/// once; the backend finishes the file after that and reports it. The file is removed when
/// that answer arrives: nothing unreadable stays on disk until the next start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_that_ends_after_its_message_was_deleted_leaves_no_file() {
    let photo = T0 + 1;
    let s = script("late-download")
        .slow_downloads(Duration::from_secs(5))
        .step(Step::WaitAcked)
        .deliver(vec![media(photo, vec![synthetic_image(640, 480, 4)], None)])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(1)))
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&d_alice()), Some(&alice()), &photo.to_string()),
            by: Some(alice()),
            for_everyone: true,
            at: photo + 1_000,
        }]);
    let (mut env, _account, uuid) = linked(s).await;
    let blobs = env.vault.blob_dir(uuid).join("core");
    // The download is asked for and the fake network takes its time.
    eventually("the download runs", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM attachment_downloads") == 1 }
    })
    .await;
    sleeping_until(&env, T0U + 5_000).await;
    sleeping_until(&env, T0U + 1_000).await;
    assert!(files_below(&blobs).is_empty(), "nothing written yet");

    // Alice deletes the message; the download is still on its way.
    env.clock.advance(Duration::from_secs(1));
    eventually("the attachment row is gone", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM attachments") == 0 }
    })
    .await;
    assert_eq!(count(&env, "SELECT count(*) FROM attachment_downloads"), 0);
    assert_eq!(
        count(&env, "SELECT count(*) FROM download_orphans"),
        1,
        "the target is remembered until the backend answers"
    );

    // The backend finishes the file and reports it.
    env.clock.advance(Duration::from_secs(4));
    eventually("the answer was taken", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM download_orphans") == 0 }
    })
    .await;
    eventually("the late file is removed", WAIT, || {
        let blobs = blobs.clone();
        async move { files_below(&blobs).is_empty() }
    })
    .await;
    assert_eq!(count(&env, "SELECT count(*) FROM files"), 0);
    env.stop().await;
}

/// The same race, and Kourier is closed before the backend's answer was taken: the next
/// start's sweep removes whatever file was left, and the remembered target is forgotten
/// (its answer cannot leave a file behind any more).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_target_dropped_before_a_restart_is_gone_after_it() {
    let photo = T0 + 1;
    let s = script("late-download-restart")
        .slow_downloads(Duration::from_secs(5))
        .step(Step::WaitAcked)
        .deliver(vec![media(photo, vec![synthetic_image(640, 480, 4)], None)])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(1)))
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&d_alice()), Some(&alice()), &photo.to_string()),
            by: Some(alice()),
            for_everyone: true,
            at: photo + 1_000,
        }]);
    let (mut env, _account, uuid) = linked(s).await;
    let blobs = env.vault.blob_dir(uuid).join("core");
    eventually("the download runs", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM attachment_downloads") == 1 }
    })
    .await;
    sleeping_until(&env, T0U + 5_000).await;
    sleeping_until(&env, T0U + 1_000).await;
    env.clock.advance(Duration::from_secs(1));
    eventually("the target is remembered", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM download_orphans") == 1 }
    })
    .await;
    // The backend finishes the file while Kourier closes: its answer may or may not be
    // taken before the stop.
    env.clock.advance(Duration::from_secs(4));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    eventually("nothing is left of it", WAIT, || {
        let (env, blobs) = (&env, blobs.clone());
        async move {
            count(env, "SELECT count(*) FROM download_orphans") == 0
                && files_below(&blobs).is_empty()
        }
    })
    .await;
    assert_eq!(count(&env, "SELECT count(*) FROM attachments"), 0);
    assert_eq!(count(&env, "SELECT count(*) FROM files"), 0);
    env.stop().await;
}

fn picture(dir: &Path, name: &str, seed: u64) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(
        &p,
        sample_image(64, 48, seed, SampleFormat::Jpeg).expect("jpeg"),
    )
    .expect("write");
    p
}

/// Two pictures go to the group as one message. Bob gets it, alice does not (the row stays
/// for a retry). On the phone the user deletes the first picture from the message. A
/// "Retry" then sends alice the message as it is now, with the one picture that is left,
/// instead of failing on a file that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_file_was_deleted_elsewhere_sends_what_is_left() {
    let local = T0.to_string();
    let s = script("retry-after-attachment-delete")
        .fail_transmit(
            TransmitRule::member(alice(), unregistered(), 1)
                .of_kind("Message")
                .in_conv(group()),
        )
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver(vec![BackendEvent::AttachmentDeleted {
            target: target(Some(&group()), None, &local),
            net_ids: vec![attachment_name(&local, 0)].into(),
            at: T0 + 5_000,
        }]);
    let (mut env, account, uuid) = linked(s).await;
    let g = conv(&env, &group()).await;
    sleeping_until(&env, T0U + 5_000).await;
    let src = env.dir.path().join("src");
    std::fs::create_dir_all(&src).expect("dir");
    let paths = vec![picture(&src, "one.jpg", 1), picture(&src, "two.jpg", 2)];
    env.handle
        .send(UiCommand::SendFiles {
            conv: g,
            request: Default::default(),
            paths: paths.into(),
            as_file: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let album = queued(&env, g, 1).await;
    assert_eq!(album.local, local);
    let db = env.core_db();
    eventually("partly failed", WAIT, || {
        let db = db.clone();
        async move {
            matches!(
                own_messages(&db, g)[0].out_state,
                Some(OutgoingState::PartiallyFailed { .. })
            )
        }
    })
    .await;
    assert_eq!(count(&env, "SELECT count(*) FROM attachments"), 2);

    // The phone deletes the first picture.
    env.clock.advance(Duration::from_secs(5));
    env.wait_finished(account).await;
    eventually("one picture is left", WAIT, || {
        let env = &env;
        async move { count(env, "SELECT count(*) FROM attachments") == 1 }
    })
    .await;
    assert_eq!(send_row(&env, album.id).as_deref(), Some("Failed"));

    env.handle
        .send(UiCommand::RetryMessage { message: album.id })
        .expect("send");
    eventually("alice has it", WAIT, || {
        let db = db.clone();
        async move {
            matches!(
                own_messages(&db, g)[0].out_state,
                Some(OutgoingState::Sent { .. })
            )
        }
    })
    .await;
    let sent = env.control.transmits(uuid);
    assert_eq!(sent.len(), 2);
    let names = |kind: &OutgoingKind| match kind {
        OutgoingKind::Message {
            body: Body::Media { attachments, .. },
            ..
        } => attachments
            .iter()
            .map(|a| a.file_name.clone().unwrap_or_default())
            .collect::<Vec<_>>(),
        other => panic!("not a media message: {}", other.name()),
    };
    assert_eq!(names(&sent[0].req.kind), ["one.jpg", "two.jpg"]);
    assert_eq!(
        names(&sent[1].req.kind),
        ["two.jpg"],
        "the retry carries what the message is now"
    );
    assert_eq!(sent[1].req.only, Some(vec![alice()]));
    assert_eq!(sent[1].req.local.as_ref(), local);
    assert_eq!(support::send::outbox_len(&db), 0);
    env.stop().await;
}

/// A stranger's long message arrives as a message request: its text is cut off and the rest
/// is an attachment, which is not fetched (nothing is fetched from a stranger). When the
/// user accepts the request, the rest of the text is fetched and the message is whole,
/// without a click.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_text_from_a_message_request_is_completed_when_it_is_accepted() {
    let whole = "every word of a long first message. ".repeat(200);
    let mut pending = ConversationPatch::new(d_alice());
    pending.request_pending = Some(true);
    let mut accepted = ConversationPatch::new(d_alice());
    accepted.request_pending = Some(false);
    let s = script("long-text-request")
        .deliver(vec![
            BackendEvent::ConversationUpsert(pending),
            media(
                T0 + 1,
                vec![synthetic_long_text(&whole)],
                Some(&whole[..2048]),
            ),
        ])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver(vec![BackendEvent::ConversationUpsert(accepted)]);
    let (mut env, account, _uuid) = linked(s).await;
    let a = conv(&env, &d_alice()).await;
    sleeping_until(&env, T0U + 5_000).await;
    settle().await;
    assert_eq!(
        count(&env, "SELECT count(*) FROM attachment_downloads"),
        0,
        "nothing is fetched from an unanswered request"
    );
    let cut = env
        .timeline(a)
        .await
        .into_iter()
        .find(|r| r.sent_at == T0 + 1)
        .expect("the message");
    assert!(
        matches!(&cut.body, Body::Media { caption: Some(t), .. } if t.body.len() == 2048),
        "cut off"
    );

    // The user accepts the request (on the phone, or here: the backend reports it).
    env.clock.advance(Duration::from_secs(5));
    env.wait_finished(account).await;
    eventually("the message is whole", WAIT, || {
        let (env, whole) = (&env, whole.clone());
        async move {
            env.timeline(a)
                .await
                .iter()
                .any(|r| r.body == Body::text(whole.clone()))
        }
    })
    .await;
    assert_eq!(count(&env, "SELECT count(*) FROM attachments"), 0);
    env.stop().await;
}

/// The thumbnail blobs `core.db` names: (files row, absolute path, plaintext size).
fn thumbnails(env: &Env, uuid: AccountUuid) -> Vec<(i64, PathBuf, u64)> {
    let dir = env.vault.blob_dir(uuid);
    core_db(&env.core_db(), |c| {
        let mut stmt = c
            .prepare("SELECT id, path, size FROM files WHERE purpose = 'thumb' ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .expect("query")
        .map(|r| {
            let (id, path, size) = r.expect("row");
            (id, dir.join(path), u64::try_from(size).expect("size"))
        })
        .collect()
    })
}

/// Thumbnails are written without fsync: they can be made again from the picture, which is
/// written durably. After a power loss a thumbnail's file can be cut short or missing
/// although its row was committed. The next start finds that by the file's length and
/// derives the thumbnail again; a thumbnail that is whole is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thumbnail_lost_to_a_power_cut_is_made_again_after_the_next_start() {
    let s = script("thumb-repair").step(Step::WaitAcked).deliver(vec![
        media(T0 + 1, vec![synthetic_image(640, 480, 1)], None),
        media(T0 + 2, vec![synthetic_image(640, 480, 2)], None),
        media(T0 + 3, vec![synthetic_image(640, 480, 3)], None),
    ]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    eventually("three thumbnails", WAIT, || {
        let env = &env;
        async move {
            count(
                env,
                "SELECT count(*) FROM attachments WHERE thumb_file_id IS NOT NULL",
            ) == 3
        }
    })
    .await;
    let before = thumbnails(&env, uuid);
    assert_eq!(before.len(), 3);
    for (_, path, size) in &before {
        assert_eq!(
            std::fs::metadata(path).expect("thumbnail").len(),
            kourier_blob::sealed_len(*size),
            "a complete blob"
        );
    }
    env.stop().await;

    // The power went: the first thumbnail was cut off, the second never reached the disk.
    let cut = std::fs::OpenOptions::new()
        .write(true)
        .open(&before[0].1)
        .expect("open");
    cut.set_len(100).expect("truncate");
    drop(cut);
    std::fs::remove_file(&before[1].1).expect("remove");

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    eventually("the thumbnails are made again", WAIT, || {
        let (env, kept) = (&env, before[2].0);
        async move {
            let now = thumbnails(env, uuid);
            now.len() == 3
                && now.iter().filter(|(id, _, _)| *id == kept).count() == 1
                && now.iter().all(|(_, path, size)| {
                    std::fs::metadata(path)
                        .is_ok_and(|m| m.len() == kourier_blob::sealed_len(*size))
                })
        }
    })
    .await;
    let after = thumbnails(&env, uuid);
    assert!(
        after
            .iter()
            .all(|(id, _, _)| *id != before[0].0 && *id != before[1].0),
        "the broken ones were replaced"
    );
    assert!(!before[0].1.exists(), "the cut-off file is gone");
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM attachments WHERE thumb_file_id IS NULL OR derived_at IS NULL"
        ),
        0
    );
    assert_eq!(count(&env, "SELECT count(*) FROM files"), 6);
    env.stop().await;
}

// ---------- a link that finished a moment too early ----------

/// The link finished on the network (the fake server has the device) but its `AccountReady`
/// has not reached the core yet, which still shows the QR screen. The user cancels, or
/// removes the account. Either way the device must not stay linked to their account with
/// nobody behind it: the backend is told to log it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_or_removal_right_after_the_link_logs_the_device_out() {
    let slow = |name: &str| {
        let mut s = Script::new(name, vec![uid(ME), uid(ME_PNI)]).deliver(setup());
        s.login = LoginScript::Auto;
        // The envelope with `AccountReady` is held back for a minute on the account clock.
        s.forward_delay = Some(Duration::from_secs(60));
        s
    };
    let mut env = Env::new(Flavor::SignalLike, slow("cancel-after-link")).await;
    for remove in [false, true] {
        if remove {
            env.control.push_script(slow("remove-after-link"));
        }
        let account = add_account(&mut env).await;
        let uuid = env.account(account).await.expect("row").uuid;
        let control = env.control.clone();
        tokio::time::timeout(WAIT, control.wait_for(|c| c.has_device(uuid)))
            .await
            .expect("the link finished on the network");
        assert_eq!(
            env.account(account).await.expect("row").state,
            AccountState::Linking,
            "the core has not heard of it yet"
        );
        let command = if remove {
            UiCommand::RemoveAccount { account }
        } else {
            UiCommand::CancelLogin { account }
        };
        env.handle.send(command).expect("send");
        tokio::time::timeout(
            WAIT,
            control.wait_for(|c| c.commands(uuid).contains(&"Logout")),
        )
        .await
        .expect("the backend is told to log out");
        // What the backend held back is let go now, row by row (each a minute later on
        // its clock): the core must not take the account in after all.
        let clock = env.clock.clone();
        let time = tokio::spawn(async move {
            loop {
                clock.advance(Duration::from_secs(60));
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        env.event("AccountRemoved", |e| match e {
            UiEvent::AccountRemoved { account: a } if *a == account => Some(()),
            _ => None,
        })
        .await;
        time.abort();
        assert!(
            !env.control.has_device(uuid),
            "the device was logged out of the network"
        );
        assert!(env.accounts().await.is_empty());
        assert!(!env.account_dir(uuid).exists(), "directory deleted");
        assert!(!env.secret_exists(uuid), "secret deleted");
    }
    env.stop().await;
}

// ---------- account ids ----------

/// An account id is never handed out twice: what is kept per account id without a foreign
/// key (the id allocator, a stored challenge, sets in memory) can then never meet another
/// account. Also not after a restart, and not when the removed account was the only one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_id_is_not_handed_out_again() {
    let mut env = Env::new(Flavor::SignalLike, script("ids-one")).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let first = add_linked(&mut env).await;
    env.wait_finished(first).await;
    let remove = |env: &Env, account| {
        env.handle
            .send(UiCommand::RemoveAccount { account })
            .expect("send");
    };
    remove(&env, first);
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account } if *account == first => Some(()),
        _ => None,
    })
    .await;
    assert!(env.accounts().await.is_empty());

    env.control.push_script(script("ids-two"));
    let second = add_linked(&mut env).await;
    assert!(second.0 > first.0, "{second:?} after {first:?}");
    env.wait_finished(second).await;
    remove(&env, second);
    env.event("AccountRemoved", |e| match e {
        UiEvent::AccountRemoved { account } if *account == second => Some(()),
        _ => None,
    })
    .await;

    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.control.push_script(script("ids-three"));
    env.handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Fake,
            flow: "qr".into(),
        })
        .expect("send");
    let third = env
        .event("AccountAdded", |e| match e {
            UiEvent::AccountAdded { account } => Some(*account),
            _ => None,
        })
        .await;
    assert!(third.0 > second.0, "{third:?} after {second:?}");
    env.stop().await;
}
