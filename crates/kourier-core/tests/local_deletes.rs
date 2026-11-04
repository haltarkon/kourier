//! Deleting from Kourier (§3.5.2, §4.8 durable commands), through the real core on the fake
//! network: clearing a conversation's history, deleting a conversation, and deleting one
//! attachment of a message for this account. Each is applied at once and handed to the backend
//! as a durable command, so the account's other devices follow; and the attachments of
//! messages Kourier sent carry the names the network has for them, so the same delete made on
//! the phone finds them here.

use crate::support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, DurableCommand, IncomingMessage, OutgoingKind};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ActionSubject, Query, QueryData, Refusal, RequestId, UiCommand, UiEvent,
};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::{T0, attachment_name, own, remote, target};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{
    AccountUuid, Body, ConvKey, ConversationId, ConversationPatch, MessageId, MsgIdScope,
    NetworkKind, SystemEvent, Text, UserId,
};
use support::send::{
    ME, ME_PNI, alice, conv_id, core_db, d_alice, d_bob, own_messages, setup, transient, uid,
};
use support::{Env, WAIT, add_linked, eventually};

/// How long the phone waits before it deletes (on the account clock).
const PHONE_PAUSE: Duration = Duration::from_secs(40);

async fn start(script: Script) -> (Env, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let uuid = env.account(account).await.expect("row").uuid;
    (env, uuid)
}

fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    conv_id(&env.core_db(), key).expect("conversation")
}

/// Alice's chat with three more messages of hers.
fn chatty(name: &str) -> Script {
    let mut first = setup();
    for n in 1..=3 {
        first.push(BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            &(T0 - 50_000 + n * 1000).to_string(),
            T0 - 50_000 + n * 1000,
            &format!("message {n}"),
        )));
    }
    Script::new(name, vec![uid(ME), uid(ME_PNI)]).deliver(first)
}

async fn texts(env: &Env, conv: ConversationId) -> Vec<String> {
    env.timeline(conv)
        .await
        .into_iter()
        .map(|r| match r.body {
            Body::Text(t) => t.body,
            Body::Media { attachments, .. } => attachments
                .iter()
                .map(|a| a.file_name.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("+"),
            other => other.kind_name().to_owned(),
        })
        .collect()
}

async fn listed(env: &Env, conv: ConversationId) -> bool {
    env.conversations().await.iter().any(|c| c.id == conv)
}

fn pending_commands(env: &Env) -> i64 {
    core_db(&env.core_db(), |c| {
        c.query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))
            .expect("count")
    })
}

async fn wait_persisted(env: &Env, uuid: AccountUuid, n: usize) -> Vec<DurableCommand> {
    let control = env.control.clone();
    let deletes = |c: &kourier_fake::FakeControl| -> Vec<DurableCommand> {
        c.durable_commands(uuid)
            .into_iter()
            .filter(|d| {
                matches!(
                    d,
                    DurableCommand::ClearConversation { .. }
                        | DurableCommand::DeleteAttachment { .. }
                )
            })
            .collect()
    };
    tokio::time::timeout(WAIT, control.wait_for(|c| deletes(c).len() >= n))
        .await
        .expect("the backend has the command");
    // Its `CommandPersisted` is with the core: nothing is left to send again.
    eventually("the command is confirmed", WAIT, || {
        let db = env.core_db();
        async move {
            core_db(&db, |c| {
                c.query_row("SELECT count(*) FROM pending_commands", [], |r| {
                    r.get::<_, i64>(0)
                })
                .is_ok_and(|n| n == 0)
            })
        }
    })
    .await;
    deletes(&env.control)
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

async fn sent_messages(env: &Env, uuid: AccountUuid, key: &ConvKey, n: usize) {
    let (control, key) = (env.control.clone(), key.clone());
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| {
            c.transmits(uuid)
                .iter()
                .filter(|t| t.req.conv == key && matches!(t.req.kind, OutgoingKind::Message { .. }))
                .count()
                >= n
        }),
    )
    .await
    .expect("sent");
    eventually("the outbox is empty", WAIT, || {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 0 }
    })
    .await;
}

/// Clearing a chat: its messages are gone at once, the chat stays in the list, and the
/// backend gets one durable command naming the newest messages, which a restart does not send
/// again. What still waits in the outbox stays and is not named.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_a_chat_is_applied_at_once_and_reaches_the_backend_once() {
    // Sends to bob keep failing for a while: his message stays queued.
    let script = chatty("clear-here")
        .fail_transmit(TransmitRule::attempts(transient(), u32::MAX).in_conv(d_bob()));
    let (mut env, uuid) = start(script).await;
    let (a, b) = (conv(&env, &d_alice()), conv(&env, &d_bob()));
    env.handle
        .send(UiCommand::SendText {
            conv: a,
            text: Text::plain("my answer"),
            reply_to: None,
        })
        .expect("send");
    sent_messages(&env, uuid, &d_alice(), 1).await;
    let mine = own_messages(&env.core_db(), a)[0].local.clone();
    assert_eq!(texts(&env, a).await.len(), 5);

    env.handle
        .send(UiCommand::ClearConversation { conv: a })
        .expect("send");
    eventually("the chat is empty", WAIT, || {
        let env = &env;
        async move { texts(env, a).await.is_empty() }
    })
    .await;
    assert!(listed(&env, a).await, "cleared, not deleted");
    let got = wait_persisted(&env, uuid, 1).await;
    match got.as_slice() {
        [
            DurableCommand::ClearConversation {
                conv,
                newest,
                newest_non_expiring,
                remove,
                ..
            },
        ] => {
            assert_eq!(*conv, d_alice());
            assert!(!remove);
            let locals: Vec<String> = newest.iter().map(|t| t.local.to_string()).collect();
            assert_eq!(locals.len(), 5, "at most five, here all there were");
            assert_eq!(locals[0], mine, "newest first: the own answer");
            assert_eq!(locals[1], (T0 - 47_000).to_string());
            assert_eq!(newest, newest_non_expiring, "none of them disappears");
        }
        other => panic!("unexpected {other:?}"),
    }

    // A message that still waits in the outbox survives a clear of its chat and is not among
    // the messages named to the other devices (they do not have it).
    env.handle
        .send(UiCommand::SendText {
            conv: b,
            text: Text::plain("still on its way"),
            reply_to: None,
        })
        .expect("send");
    eventually("the message is queued", WAIT, || {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 1 }
    })
    .await;
    env.handle
        .send(UiCommand::ClearConversation { conv: b })
        .expect("send");
    // Nothing a device could delete up to: no command. The next one proves the clear ran.
    env.handle
        .send(UiCommand::DeleteConversation { conv: a })
        .expect("send");
    let got = wait_persisted(&env, uuid, 2).await;
    assert_eq!(got.len(), 2, "the clear of bob's chat told nobody: {got:?}");
    assert!(matches!(
        &got[1],
        DurableCommand::ClearConversation { conv, newest, remove: true, .. }
            if *conv == d_alice() && newest.is_empty()
    ));
    assert_eq!(texts(&env, b).await, ["still on its way"]);
    assert_eq!(support::send::outbox_len(&env.core_db()), 1);

    // After a restart the chat is still empty and nothing is sent again.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    assert!(texts(&env, a).await.is_empty());
    assert_eq!(pending_commands(&env), 0);
    assert_eq!(
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "ClearConversation")
            .count(),
        2,
        "each command reached the backend once"
    );
    env.stop().await;
}

/// Deleting a chat: it leaves the list with an event, its row says it is hidden when asked for
/// by id, and writing there brings it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_chat_hides_it_until_the_user_writes_there() {
    let (mut env, uuid) = start(chatty("delete-here")).await;
    let a = conv(&env, &d_alice());
    let row = |env: &Env| {
        let h = env.handle.clone();
        async move {
            match h.query(Query::Conversation(a)).await.expect("detail") {
                QueryData::Conversation(Some(d)) => d.row,
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    assert!(!row(&env).await.hidden);
    env.handle
        .send(UiCommand::DeleteConversation { conv: a })
        .expect("send");
    let removed = env
        .event("the chat leaves the list", |e| match e {
            UiEvent::ConversationsRemoved { ids } => Some(ids.clone()),
            _ => None,
        })
        .await;
    assert_eq!(removed, [a]);
    assert!(!listed(&env, a).await);
    assert!(row(&env).await.hidden);
    assert!(texts(&env, a).await.is_empty());
    let got = wait_persisted(&env, uuid, 1).await;
    assert!(matches!(
        &got[0],
        DurableCommand::ClearConversation { remove: true, newest, .. } if newest.len() == 4
    ));

    // Still hidden after a restart; a message written there brings the chat back.
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    assert!(!listed(&env, a).await && row(&env).await.hidden);
    env.handle
        .send(UiCommand::SendText {
            conv: a,
            text: Text::plain("hello again"),
            reply_to: None,
        })
        .expect("send");
    eventually("the chat is listed again", WAIT, || {
        let h = env.handle.clone();
        async move {
            matches!(h.query(Query::Conversations(Default::default())).await,
                Ok(QueryData::Conversations(p)) if p.rows.iter().any(|c| c.id == a && !c.hidden))
        }
    })
    .await;
    assert_eq!(texts(&env, a).await, ["hello again"]);
    // An unknown conversation is an error with its subject.
    let nowhere = ConversationId(987_654);
    env.handle
        .send(UiCommand::ClearConversation { conv: nowhere })
        .expect("send");
    let subject = env
        .event("the failure", |e| match e {
            UiEvent::ActionFailed {
                action: "ClearConversation",
                subject,
                ..
            } => Some(subject.clone()),
            _ => None,
        })
        .await;
    assert_eq!(subject, ActionSubject::Conversation(nowhere));
    env.stop().await;
}

struct Pictures {
    dir: tempfile::TempDir,
}

impl Pictures {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("sources"),
        }
    }

    fn picture(&self, name: &str, seed: u64) -> PathBuf {
        let p = self.dir.path().join(name);
        let jpeg = sample_image(48, 36, seed, SampleFormat::Jpeg).expect("jpeg");
        std::fs::write(&p, jpeg).expect("write");
        p
    }
}

/// The attachments of a message Kourier sent have the names the network gives them, so the
/// phone can delete one of them here; and deleting one here tells the phone by the same names.
/// A message that is not sent yet keeps its files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_attachments_are_named_so_either_device_can_delete_one() {
    // The message Kourier sends gets the account clock's time as its id (the clock stands
    // still until the test moves it), so the script can name it: the phone deletes its second
    // picture after a pause.
    let local = T0.to_string();
    let phone_delete = BackendEvent::AttachmentDeleted {
        target: target(Some(&d_alice()), None, &local),
        net_ids: vec![attachment_name(&local, 1)].into(),
        at: T0 + 40_000,
    };
    let script = Script::new("own-names", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .delay_reply(d_bob(), Duration::from_secs(60))
        .step(Step::Sleep(PHONE_PAUSE))
        .deliver_ordered(vec![phone_delete]);
    let (mut env, uuid) = start(script).await;
    let (a, b) = (conv(&env, &d_alice()), conv(&env, &d_bob()));
    let blobs = env.vault.blob_dir(uuid);
    let f = Pictures::new();
    let paths = vec![
        f.picture("one.jpg", 1),
        f.picture("two.jpg", 2),
        f.picture("three.jpg", 3),
    ];
    env.handle
        .send(UiCommand::SendFiles {
            conv: a,
            request: RequestId(1),
            paths: paths.clone().into(),
            as_file: Vec::new(),
            with_location: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let stored = env
        .event("stored", |e| match e {
            UiEvent::FilesStored { messages, .. } => Some(messages.clone()),
            _ => None,
        })
        .await;
    assert_eq!(stored.len(), 1, "three pictures, one album");
    sent_messages(&env, uuid, &d_alice(), 1).await;
    let own = own_messages(&env.core_db(), a);
    assert_eq!(own[0].local, local);
    // The request told the backend the names, and the stored message keeps them.
    let transmit = &env.control.transmits(uuid)[0];
    let names: Vec<Vec<String>> = match &transmit.req.kind {
        OutgoingKind::Message {
            body: Body::Media { attachments, .. },
            ..
        } => attachments.iter().map(|a| a.net_ids.clone()).collect(),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        names,
        [
            vec![attachment_name(&local, 0)],
            vec![attachment_name(&local, 1)],
            vec![attachment_name(&local, 2)]
        ]
    );
    let body: String = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT body FROM messages WHERE id = ?1",
            [stored[0].0],
            |r| r.get(0),
        )
        .expect("body")
    });
    assert!(
        body.contains(&attachment_name(&local, 2)),
        "kept with the message"
    );
    // They never reach the UI.
    let row = env.timeline(a).await.pop().expect("row");
    let Body::Media { attachments, .. } = &row.body else {
        panic!("unexpected {:?}", row.body)
    };
    assert!(attachments.iter().all(|a| a.net_ids.is_empty()));
    assert!(row.actions.delete_attachment);
    // Three pictures with their thumbnails.
    assert_eq!(blob_count(&blobs), 6);

    // The phone deletes the second picture.
    let clock = env.clock.clone();
    let pause = u64::try_from(PHONE_PAUSE.as_millis()).expect("fits");
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + pause) }
    })
    .await;
    env.clock.advance(PHONE_PAUSE);
    eventually("the picture is gone", WAIT, || {
        let env = &env;
        async move { texts(env, a).await == ["are you there?", "one.jpg+three.jpg"] }
    })
    .await;
    eventually("its files are unlinked", WAIT, || {
        let blobs = blobs.clone();
        async move { blob_count(&blobs) == 4 }
    })
    .await;

    // Deleting one here: the third picture, now the second of the message.
    let third = match &env.timeline(a).await[1].body {
        Body::Media { attachments, .. } => attachments[1].id.expect("id"),
        other => panic!("unexpected {other:?}"),
    };
    env.handle
        .send(UiCommand::DeleteAttachment { attachment: third })
        .expect("send");
    let got = wait_persisted(&env, uuid, 1).await;
    match got.as_slice() {
        [
            DurableCommand::DeleteAttachment {
                conv,
                target,
                index,
                net_ids,
            },
        ] => {
            assert_eq!(*conv, d_alice());
            assert_eq!(target.local.as_ref(), local);
            assert_eq!(*index, 1, "where it was when it went");
            assert_eq!(net_ids.expose(), &[attachment_name(&local, 2)]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(texts(&env, a).await, ["are you there?", "one.jpg"]);
    eventually("its files are unlinked", WAIT, || {
        let blobs = blobs.clone();
        async move { blob_count(&blobs) == 2 }
    })
    .await;
    // The same attachment again: it is gone.
    env.handle
        .send(UiCommand::DeleteAttachment { attachment: third })
        .expect("send");
    let (reason, subject) = failure(&mut env).await;
    assert_eq!(reason, Some(Refusal::Gone));
    assert_eq!(subject, ActionSubject::Attachment(third));

    // A message whose send is still on its way keeps its files: bob's answer is held back.
    env.handle
        .send(UiCommand::SendFiles {
            conv: b,
            request: RequestId(2),
            paths: vec![paths[0].clone()].into(),
            as_file: Vec::new(),
            with_location: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let waiting: Vec<MessageId> = env
        .event("stored", |e| match e {
            UiEvent::FilesStored {
                request: RequestId(2),
                messages,
                ..
            } => Some(messages.clone()),
            _ => None,
        })
        .await;
    let row = env.timeline(b).await.pop().expect("row");
    assert_eq!(row.id, waiting[0]);
    assert!(!row.actions.delete_attachment, "not while it is being sent");
    let Body::Media { attachments, .. } = &row.body else {
        panic!("unexpected {:?}", row.body)
    };
    let unsent = attachments[0].id.expect("id");
    env.handle
        .send(UiCommand::DeleteAttachment { attachment: unsent })
        .expect("send");
    assert_eq!(failure(&mut env).await.0, Some(Refusal::BeingSent));
    // (The answer is held from the moment the fake server has the request.)
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.transmits(uuid).iter().any(|t| t.req.conv == d_bob())),
    )
    .await
    .expect("transmitted");
    env.clock.advance(Duration::from_secs(60));
    eventually("the message is sent", WAIT, || {
        let env = &env;
        async move {
            env.timeline(b)
                .await
                .pop()
                .is_some_and(|r| r.actions.delete_attachment)
        }
    })
    .await;
    // Its only picture: the message has nothing left and is hidden like a delete for me.
    env.handle
        .send(UiCommand::DeleteAttachment { attachment: unsent })
        .expect("send");
    eventually("the message is hidden", WAIT, || {
        let env = &env;
        async move { texts(env, b).await.is_empty() }
    })
    .await;
    assert_eq!(wait_persisted(&env, uuid, 2).await.len(), 2);
    env.stop().await;
}

/// A message that reached some of its recipients and failed for the others for good takes
/// the delete of one attachment, like any sent message (and like the same delete made on the
/// phone): its outbox row is only kept for "Retry", nothing sends it by itself. A retry then
/// sends the message as it is now, without the deleted file, to those who did not get it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partly_failed_message_takes_the_delete_of_an_attachment() {
    use support::send::{bob, group, unregistered};
    let script = Script::new("partly-failed", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .fail_transmit(TransmitRule::member(bob(), unregistered(), 1).in_conv(group()));
    let (mut env, uuid) = start(script).await;
    let g = conv(&env, &group());
    let blobs = env.vault.blob_dir(uuid);
    let f = Pictures::new();
    env.handle
        .send(UiCommand::SendFiles {
            conv: g,
            request: RequestId(1),
            paths: vec![f.picture("one.jpg", 1), f.picture("two.jpg", 2)].into(),
            as_file: Vec::new(),
            with_location: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    let message = env
        .event("stored", |e| match e {
            UiEvent::FilesStored { messages, .. } => Some(messages[0]),
            _ => None,
        })
        .await;
    let send_row = |env: &Env| -> Option<String> {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
                [message.0],
                |r| r.get(0),
            )
            .ok()
        })
    };
    eventually("alice has it, bob does not", WAIT, || {
        let env = &env;
        async move {
            env.timeline(g).await.pop().is_some_and(|r| {
                matches!(
                    r.out_state,
                    Some(kourier_model::OutgoingState::PartiallyFailed { .. })
                )
            })
        }
    })
    .await;
    assert_eq!(
        send_row(&env).as_deref(),
        Some("Failed"),
        "kept for a retry"
    );
    let row = env.timeline(g).await.pop().expect("row");
    assert!(
        row.actions.delete_attachment,
        "nothing sends it by itself: the files are not held"
    );
    let Body::Media { attachments, .. } = &row.body else {
        panic!("unexpected {:?}", row.body)
    };
    assert_eq!(blob_count(&blobs), 4, "two pictures, two thumbnails");
    let second = attachments[1].id.expect("id");
    env.handle
        .send(UiCommand::DeleteAttachment { attachment: second })
        .expect("send");
    eventually("the second picture is gone", WAIT, || {
        let env = &env;
        async move { texts(env, g).await == ["one.jpg"] }
    })
    .await;
    eventually("its files are unlinked", WAIT, || {
        let blobs = blobs.clone();
        async move { blob_count(&blobs) == 2 }
    })
    .await;
    let told = wait_persisted(&env, uuid, 1).await;
    assert!(
        matches!(
            told.as_slice(),
            [DurableCommand::DeleteAttachment { index: 1, .. }]
        ),
        "{told:?}"
    );
    assert_eq!(send_row(&env).as_deref(), Some("Failed"), "still there");

    // "Retry": bob gets the message as it is now. (The fake reads every blob a request
    // names, so a request that still named the deleted picture would fail.)
    env.handle
        .send(UiCommand::RetryMessage { message })
        .expect("send");
    sent_messages(&env, uuid, &group(), 2).await;
    let retried = env
        .control
        .transmits(uuid)
        .into_iter()
        .rfind(|t| t.req.conv == group())
        .expect("the retry");
    assert_eq!(retried.req.only.as_deref(), Some(&[bob()][..]));
    match &retried.req.kind {
        OutgoingKind::Message {
            body: Body::Media { attachments, .. },
            ..
        } => {
            let names: Vec<_> = attachments.iter().map(|a| a.file_name.clone()).collect();
            assert_eq!(names, [Some("one.jpg".to_owned())]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        env.timeline(g).await.pop().expect("row").out_state,
        Some(kourier_model::OutgoingState::Sent { .. })
    ));
    env.stop().await;
}

async fn failure(env: &mut Env) -> (Option<Refusal>, ActionSubject) {
    env.event("the refusal", |e| match e {
        UiEvent::ActionFailed {
            action: "DeleteAttachment",
            error,
            subject,
            ..
        } => Some((error.refusal(), subject.clone())),
        _ => None,
    })
    .await
}

/// A logged-out account deletes locally: there is no device to tell, and nothing is queued
/// for later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logged_out_account_clears_locally() {
    let script = chatty("clear-logged-out")
        .step(Step::Sleep(PHONE_PAUSE))
        .step(Step::Unlink {
            reason: "unlinked by the phone".into(),
        });
    let (mut env, uuid) = start(script).await;
    let clock = env.clock.clone();
    let pause = u64::try_from(PHONE_PAUSE.as_millis()).expect("fits");
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + pause) }
    })
    .await;
    env.clock.advance(PHONE_PAUSE);
    eventually("the account is logged out", WAIT, || {
        let env = &env;
        async move {
            env.accounts()
                .await
                .iter()
                .any(|a| matches!(a.state, kourier_model::AccountState::LoggedOut { .. }))
        }
    })
    .await;
    let a = conv(&env, &d_alice());
    assert_eq!(texts(&env, a).await.len(), 4);
    env.handle
        .send(UiCommand::ClearConversation { conv: a })
        .expect("send");
    eventually("the chat is empty", WAIT, || {
        let env = &env;
        async move { texts(env, a).await.is_empty() }
    })
    .await;
    assert_eq!(pending_commands(&env), 0);
    assert!(!env.control.commands(uuid).contains(&"ClearConversation"));
    env.stop().await;
}

// ---- B7 (K-B1): a clear by time, kept as a cut ----------------------------------------------
//
// WhatsApp's clear carries a TIME, not anchors, and its history keeps coming after it (a chunk
// is a new key, so the memory of removed keys does not stop it). The core keeps the time per
// conversation KEY (`conversation_cuts`) and drops the HISTORY rows at or below it. These tests
// run the ingest alone (`IngestHarness`, per-sender ids like WhatsApp's) on a harness clock
// that is later than every time they use, unless a test is about the clock.

const WA_ME: &str = "lid:900000001";
const WA_PEER: &str = "lid:1111";
const WA_PEER_PN: &str = "pn:15550000111";
/// Before the harness clock (1_700_000_000_000): none of the times below is in the future.
const WA_T: i64 = 1_699_990_000_000;
const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
/// `CLEAR_CLOCK_TOLERANCE` in ms.
const TOLERANCE: i64 = HOUR;

fn wa_user(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn wa() -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::ConversationAuthor,
        &[wa_user(WA_ME)],
    )
    .expect("harness");
    (dir, h)
}

/// The n-th second after `WA_T`.
fn t(n: i64) -> i64 {
    WA_T + n * 1000
}

fn peer_chat() -> ConvKey {
    ConvKey::Direct(wa_user(WA_PEER))
}

fn pn_chat() -> ConvKey {
    ConvKey::Direct(wa_user(WA_PEER_PN))
}

/// A text of the peer of `chat` (whose author is the chat's own user), sent at `at`.
fn from_peer(chat: &ConvKey, local: &str, at: i64) -> IncomingMessage {
    let ConvKey::Direct(peer) = chat else {
        panic!("a direct chat")
    };
    remote(chat, peer, local, at, local)
}

fn live(chat: &ConvKey, local: &str, at: i64) -> BackendEvent {
    BackendEvent::Message(from_peer(chat, local, at))
}

fn history(chat: &ConvKey, rows: Vec<IncomingMessage>) -> BackendEvent {
    BackendEvent::HistoryBatch {
        conv: chat.clone(),
        messages: rows,
        has_more: false,
        progress: None,
    }
}

fn cleared(chat: &ConvKey, before_ms: Option<i64>) -> BackendEvent {
    BackendEvent::ConversationCleared {
        conv: chat.clone(),
        up_to: Vec::new(),
        remove: false,
        before_ms,
        at: t(0),
    }
}

/// The `sent_at` of the rows of `chat`, oldest first.
fn sent_ats(h: &IngestHarness, chat: &ConvKey) -> Vec<i64> {
    h.connection()
        .prepare(
            "SELECT m.sent_at FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.conv_key = ?1 ORDER BY m.sent_at",
        )
        .expect("prepare")
        .query_map([chat.encode()], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn cut_of(h: &IngestHarness, chat: &ConvKey) -> Option<i64> {
    h.connection()
        .query_row(
            "SELECT before_ms FROM conversation_cuts WHERE conv_key = ?1",
            [chat.encode()],
            |r| r.get(0),
        )
        .ok()
}

fn count(h: &IngestHarness, table: &str) -> i64 {
    h.connection()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

fn conv_of(h: &IngestHarness, chat: &ConvKey) -> ConversationId {
    ConversationId(
        h.connection()
            .query_row(
                "SELECT id FROM conversations WHERE conv_key = ?1",
                [chat.encode()],
                |r| r.get(0),
            )
            .expect("the conversation"),
    )
}

/// T17: a chat cleared by time (the phone's `clearChatAction`) stays cleared while history keeps
/// coming: the rows at or below the cut are dropped, a newer one is kept, and the cut is
/// remembered at the time the event carried. Without the refusal, or when the cut is only
/// applied and not kept, the old rows come back (their keys are new: the memory of removed keys
/// does not know them).
#[test]
fn a_history_chunk_after_a_clear_stays_cleared() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(
        (1..=5)
            .map(|n| live(&chat, &format!("L{n}"), t(n)))
            .collect(),
    )
    .expect("live rows");
    assert_eq!(sent_ats(&h, &chat), [t(1), t(2), t(3), t(4), t(5)]);
    h.apply(vec![cleared(&chat, Some(t(5)))]).expect("clear");
    assert!(sent_ats(&h, &chat).is_empty(), "the clear took the rows");
    assert_eq!(cut_of(&h, &chat), Some(t(5)));
    let chunk = history(
        &chat,
        vec![
            from_peer(&chat, "H2", t(2)),
            from_peer(&chat, "H4", t(4)),
            from_peer(&chat, "H5", t(5)),
            from_peer(&chat, "H9", t(9)),
        ],
    );
    h.apply(vec![chunk.clone()]).expect("history");
    assert_eq!(sent_ats(&h, &chat), [t(9)], "only the row after the clear");
    // A redelivery of the chunk changes nothing, and a lower cut later does not lower the cut.
    h.apply(vec![chunk, cleared(&chat, Some(t(1)))])
        .expect("again");
    assert_eq!(sent_ats(&h, &chat), [t(9)]);
    assert_eq!(cut_of(&h, &chat), Some(t(5)), "a cut only rises");
}

/// T17b (critic M2, F3): a time far after the core's clock is read as the clock plus the
/// tolerance (an hour). A phone's clock a day ahead (or a hostile claim) would otherwise delete the
/// live messages that arrived after the clear and refuse the chat's history for good, and nothing
/// could lower the cut.
#[test]
fn a_clear_by_time_far_in_the_future_is_read_as_the_clock_plus_the_tolerance() {
    let (_dir, mut h) = wa();
    let now = h.now();
    let chat = peer_chat();
    h.apply(vec![
        live(&chat, "L1", t(1)),
        live(&chat, "L2", t(2)),
        // A message that claims a time a day ahead.
        live(&chat, "F1", now + DAY),
    ])
    .expect("live rows");
    h.apply(vec![cleared(&chat, Some(now + 10 * DAY))])
        .expect("clear");
    assert_eq!(
        sent_ats(&h, &chat),
        [now + DAY],
        "what the clock has not reached yet is not covered"
    );
    assert_eq!(
        cut_of(&h, &chat),
        Some(now + TOLERANCE),
        "the cut is the clock plus the tolerance, not the claim"
    );
    h.apply(vec![history(
        &chat,
        vec![
            from_peer(&chat, "H3", t(3)),
            from_peer(&chat, "H9", now + 2 * DAY),
        ],
    )])
    .expect("history");
    assert_eq!(
        sent_ats(&h, &chat),
        [now + DAY, now + 2 * DAY],
        "a row below the cut is dropped, a row after the clock is kept"
    );
}

/// T17b, the user's side: a clear made here names at most the clock plus the tolerance, in the
/// stored cut and in the command for the network.
#[test]
fn a_clear_made_here_never_names_the_future() {
    let (_dir, mut h) = wa();
    let now = h.now();
    let chat = peer_chat();
    h.apply(vec![
        live(&chat, "L1", t(1)),
        live(&chat, "F1", now + 3 * DAY),
    ])
    .expect("live rows");
    let commands = h
        .clear_conversation(conv_of(&h, &chat), false)
        .expect("clear");
    assert!(sent_ats(&h, &chat).is_empty());
    assert_eq!(cut_of(&h, &chat), Some(now + TOLERANCE));
    match commands.as_slice() {
        [DurableCommand::ClearConversation { before_ms, .. }] => {
            assert_eq!(*before_ms, Some(now + TOLERANCE));
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// A clear made HERE always raises the local cut to its own time, also when the cut's CLAIM is the
/// larger (follow-up of the K-B1 fix round). The phone's claim a long way ahead was clamped at the
/// clock of its day (cut now + 1 h, claim now + 10 days); hours later the user clears the chat,
/// whose newest message is stamped after that cut, and the history of that stretch that still
/// arrives must be refused as the user's clear says. The claim is untouched, so the phone's replay
/// of its own clear still changes nothing.
#[test]
fn a_clear_made_here_raises_the_cut_though_a_larger_claim_is_stored() {
    let (_dir, mut h) = wa();
    let now = h.now();
    let chat = peer_chat();
    let claim = |h: &IngestHarness| -> i64 {
        h.connection()
            .query_row("SELECT claimed_ms FROM conversation_cuts", [], |r| r.get(0))
            .expect("the cut")
    };
    h.apply(vec![
        live(&chat, "L1", t(1)),
        cleared(&chat, Some(now + 10 * DAY)),
    ])
    .expect("the phone's clear, far ahead");
    assert_eq!(cut_of(&h, &chat), Some(now + TOLERANCE));
    assert_eq!(claim(&h), now + 10 * DAY);

    // Hours later a message the network stamped three hours after the first clock arrives (above
    // the cut, so it stays), and the user clears the chat.
    h.set_now(now + 5 * HOUR);
    h.apply(vec![live(&chat, "M2", now + 3 * HOUR)])
        .expect("a live message");
    assert_eq!(sent_ats(&h, &chat), [now + 3 * HOUR]);
    h.clear_conversation(conv_of(&h, &chat), false)
        .expect("the user's clear");
    assert!(sent_ats(&h, &chat).is_empty());
    assert_eq!(
        cut_of(&h, &chat),
        Some(now + 3 * HOUR),
        "the cut rose to the user's clear"
    );
    assert_eq!(claim(&h), now + 10 * DAY, "the claim is untouched");

    // History between the two cuts is refused now; history above the user's cut stays.
    h.apply(vec![history(
        &chat,
        vec![
            from_peer(&chat, "H1", now + 2 * HOUR),
            from_peer(&chat, "H2", now + 4 * HOUR),
        ],
    )])
    .expect("history");
    assert_eq!(sent_ats(&h, &chat), [now + 4 * HOUR], "only H2 is left");

    // The phone's own clear delivered again changes nothing.
    h.apply(vec![cleared(&chat, Some(now + 10 * DAY))])
        .expect("replay");
    assert_eq!(sent_ats(&h, &chat), [now + 4 * HOUR]);
    assert_eq!(cut_of(&h, &chat), Some(now + 3 * HOUR));
}

/// T18: a clear by time can come BEFORE its chat exists (an app-state patch before the history
/// that makes the chat). The cut is kept by the conversation's key, so the chunk that follows
/// is judged by it, and a chunk of only old rows makes no empty chat.
#[test]
fn a_clear_by_time_before_its_chat_exists_holds_for_the_chat() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![cleared(&chat, Some(t(5)))]).expect("clear");
    assert_eq!(count(&h, "conversations"), 0);
    assert_eq!(cut_of(&h, &chat), Some(t(5)));
    h.apply(vec![history(
        &chat,
        vec![from_peer(&chat, "H1", t(1)), from_peer(&chat, "H3", t(3))],
    )])
    .expect("old chunk");
    assert_eq!(count(&h, "conversations"), 0, "no empty chat");
    h.apply(vec![history(
        &chat,
        vec![from_peer(&chat, "H4", t(4)), from_peer(&chat, "H9", t(9))],
    )])
    .expect("chunk");
    assert_eq!(sent_ats(&h, &chat), [t(9)]);
}

/// T19: only HISTORY is refused. A live message and a catch-up message with a `sent_at` below
/// the cut are kept: a sender's clock may be a day behind, and what arrived after the clear
/// came stays, as it does for anchors.
#[test]
fn a_live_message_older_than_a_cut_is_kept() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![live(&chat, "L1", t(1)), cleared(&chat, Some(t(5)))])
        .expect("clear");
    let mut caught_up = from_peer(&chat, "C3", t(3));
    caught_up.catch_up = true;
    h.apply(vec![
        live(&chat, "M2", t(2)),
        BackendEvent::Message(caught_up),
    ])
    .expect("live and catch-up");
    assert_eq!(sent_ats(&h, &chat), [t(2), t(3)]);
}

/// T20: a chat cleared HERE stays cleared while history chunks still arrive, and the durable
/// command carries the newest `sent_at` that went (`W6` sends it as the range's time).
#[test]
fn a_clear_made_here_keeps_out_the_history_that_comes_later() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![
        live(&chat, "L1", t(1)),
        live(&chat, "L2", t(2)),
        live(&chat, "L3", t(3)),
        BackendEvent::Message(own(&chat, "O4", t(4), "mine")),
    ])
    .expect("live rows");
    let commands = h
        .clear_conversation(conv_of(&h, &chat), false)
        .expect("clear");
    match commands.as_slice() {
        [
            DurableCommand::ClearConversation {
                before_ms, remove, ..
            },
        ] => {
            assert!(!remove);
            assert_eq!(*before_ms, Some(t(4)), "the newest that went");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(sent_ats(&h, &chat).is_empty());
    assert_eq!(cut_of(&h, &chat), Some(t(4)));
    h.apply(vec![history(
        &chat,
        vec![
            from_peer(&chat, "H2", t(2)),
            from_peer(&chat, "H4", t(4)),
            from_peer(&chat, "H9", t(9)),
        ],
    )])
    .expect("history");
    assert_eq!(sent_ats(&h, &chat), [t(9)]);
}

/// F9: a clear made here that removed nothing names no time (the command, which a deleted chat
/// gets even when it was empty, says `None` twice), but the cut is NOW: the history of that chat
/// that arrives later is older than the clear and is refused, as on the phone.
#[test]
fn a_clear_that_removed_nothing_names_no_time_but_cuts_at_now() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![BackendEvent::ConversationUpsert(
        ConversationPatch::new(chat.clone()),
    )])
    .expect("chat");
    let commands = h
        .clear_conversation(conv_of(&h, &chat), true)
        .expect("delete");
    match commands.as_slice() {
        [
            DurableCommand::ClearConversation {
                before_ms,
                system_before_ms,
                ..
            },
        ] => assert_eq!((*before_ms, *system_before_ms), (None, None)),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(cut_of(&h, &chat), Some(h.now()), "now");
    // A clear (not a delete) of a listed chat that has no rows yet: the chunks that keep coming
    // are older than the clear and are refused; a live message is news and stays.
    let other = pn_chat();
    h.apply(vec![BackendEvent::ConversationUpsert(
        ConversationPatch::new(other.clone()),
    )])
    .expect("chat");
    let commands = h
        .clear_conversation(conv_of(&h, &other), false)
        .expect("clear");
    assert!(commands.is_empty(), "nothing to name, nothing is told");
    assert_eq!(cut_of(&h, &other), Some(h.now()));
    h.apply(vec![history(
        &other,
        vec![from_peer(&other, "H1", t(1)), from_peer(&other, "H2", t(2))],
    )])
    .expect("history");
    assert!(sent_ats(&h, &other).is_empty(), "the history is refused");
    h.apply(vec![live(&other, "L3", t(3))]).expect("live");
    assert_eq!(sent_ats(&h, &other), [t(3)]);
}

/// T21: the cut follows an identity merge, like the keys of expired messages do. (a) Only the
/// phone-number chat exists: its conversation is re-keyed and the cut moves to the new key.
/// (b) Both chats exist and both have a cut, the larger on either side: one cut is left, the
/// larger one (a plain re-key would meet the primary key, and a constraint error fails the
/// envelope for good).
#[test]
fn a_cut_follows_an_identity_merge() {
    let merge = BackendEvent::IdentityMerged {
        from: wa_user(WA_PEER_PN),
        into: wa_user(WA_PEER),
    };
    let (pn, lid) = (pn_chat(), peer_chat());
    {
        let (_dir, mut h) = wa();
        h.apply(vec![
            live(&pn, "P1", t(1)),
            live(&pn, "P2", t(2)),
            cleared(&pn, Some(t(2))),
        ])
        .expect("clear under the phone number");
        assert_eq!(cut_of(&h, &pn), Some(t(2)));
        h.apply(vec![merge.clone()]).expect("merge");
        assert_eq!(cut_of(&h, &lid), Some(t(2)), "the cut is under the new key");
        assert_eq!(cut_of(&h, &pn), None);
        assert_eq!(count(&h, "conversation_cuts"), 1);
        h.apply(vec![history(
            &lid,
            vec![from_peer(&lid, "H1", t(1)), from_peer(&lid, "H3", t(3))],
        )])
        .expect("history");
        assert_eq!(sent_ats(&h, &lid), [t(3)]);
    }
    for (from_cut, into_cut) in [(t(7), t(3)), (t(3), t(7))] {
        let (_dir, mut h) = wa();
        h.apply(vec![
            live(&pn, "P1", t(1)),
            live(&lid, "Q1", t(1)),
            cleared(&pn, Some(from_cut)),
            cleared(&lid, Some(into_cut)),
        ])
        .expect("both chats cleared");
        assert_eq!(count(&h, "conversation_cuts"), 2);
        h.apply(vec![merge.clone()])
            .expect("a merge of two cuts must not be refused");
        assert_eq!(count(&h, "conversation_cuts"), 1, "{from_cut} / {into_cut}");
        assert_eq!(cut_of(&h, &lid), Some(t(7)), "the larger cut stays");
        h.apply(vec![history(
            &lid,
            vec![from_peer(&lid, "H5", t(5)), from_peer(&lid, "H9", t(9))],
        )])
        .expect("history");
        assert_eq!(sent_ats(&h, &lid), [t(9)], "{from_cut} / {into_cut}");
    }
}

/// T21b (critic H2): a cut made before ITS CHAT exists is all the alias owns when the merge
/// comes: without `conversation_cuts` among the things `ALIAS_OWNS` asks for, the merge returns
/// early and the cut stays under the phone-number key for good, so the history that makes the
/// chat under the LID is not refused.
#[test]
fn a_cut_made_before_its_chat_follows_an_identity_merge() {
    let (_dir, mut h) = wa();
    let (pn, lid) = (pn_chat(), peer_chat());
    h.apply(vec![cleared(&pn, Some(t(5)))]).expect("clear");
    assert_eq!(count(&h, "conversations"), 0);
    h.apply(vec![BackendEvent::IdentityMerged {
        from: wa_user(WA_PEER_PN),
        into: wa_user(WA_PEER),
    }])
    .expect("merge");
    assert_eq!(cut_of(&h, &pn), None);
    assert_eq!(cut_of(&h, &lid), Some(t(5)));
    h.apply(vec![history(
        &lid,
        vec![from_peer(&lid, "H2", t(2)), from_peer(&lid, "H9", t(9))],
    )])
    .expect("history");
    assert_eq!(sent_ats(&h, &lid), [t(9)]);
}

/// T22: a clear by time spares what still has to go out, as the anchors do: an own message that
/// nobody has yet (queued) stays, and the row of a message whose delete for everyone still waits
/// stays hidden (it carries the delete); the others below the cut go.
#[test]
fn a_clear_by_time_spares_what_still_has_to_go_out() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![
        live(&chat, "L1", t(1)),
        BackendEvent::Message(own(&chat, "Q2", t(2), "queued")),
        BackendEvent::Message(own(&chat, "D3", t(3), "deleted")),
        live(&chat, "L4", t(4)),
    ])
    .expect("rows");
    let id = |local: &str| -> i64 {
        h.connection()
            .query_row(
                "SELECT id FROM messages WHERE net_local = ?1",
                [local],
                |r| r.get(0),
            )
            .expect("message")
    };
    let (queued, deleted) = (id("Q2"), id("D3"));
    let conv = conv_of(&h, &chat);
    for (message, kind) in [(queued, "Message"), (deleted, "Delete")] {
        h.connection()
            .execute(
                "INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
                 VALUES (?1, ?2, ?3, '{}', 'Queued', 0)",
                (message, conv.0, kind),
            )
            .expect("outbox row");
    }
    h.connection()
        .execute(
            "UPDATE messages SET deleted = 'ForEveryone' WHERE id = ?1",
            [deleted],
        )
        .expect("tombstone");
    h.apply(vec![cleared(&chat, Some(t(5)))]).expect("clear");
    let rows: Vec<(String, Option<String>)> = h
        .connection()
        .prepare("SELECT net_local, deleted FROM messages ORDER BY sent_at")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        rows,
        [
            ("Q2".to_owned(), None),
            ("D3".to_owned(), Some("ForMe".to_owned()))
        ],
        "the queued message stays, the delete's row stays hidden, the rest went"
    );
    assert_eq!(cut_of(&h, &chat), Some(t(5)));
}

/// F2 (storage review E1, contract review F6): a clear by time is idempotent. The cut keeps the
/// CLAIM it came from, and a delivery whose claim is not larger than the stored one changes
/// nothing: a live message that came after the clear ("what arrived after the clear came
/// stays") is not removed when the phone's app-state resync, or a redelivery after a crash,
/// delivers the clear again. A LARGER claim is a new clear and covers it.
#[test]
fn a_clear_by_time_delivered_again_changes_nothing() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    h.apply(vec![live(&chat, "L1", t(1)), cleared(&chat, Some(t(5)))])
        .expect("clear");
    h.apply(vec![live(&chat, "M2", t(2))])
        .expect("a live message after the clear");
    assert_eq!(sent_ats(&h, &chat), [t(2)]);
    h.apply(vec![cleared(&chat, Some(t(5)))]).expect("again");
    assert_eq!(sent_ats(&h, &chat), [t(2)], "the replay removed nothing");
    h.apply(vec![cleared(&chat, Some(t(4)))]).expect("older");
    assert_eq!(
        sent_ats(&h, &chat),
        [t(2)],
        "an older claim is a replay too"
    );
    assert_eq!(cut_of(&h, &chat), Some(t(5)));
    h.apply(vec![cleared(&chat, Some(t(6)))]).expect("newer");
    assert!(
        sent_ats(&h, &chat).is_empty(),
        "a larger claim is a new clear"
    );
    assert_eq!(cut_of(&h, &chat), Some(t(6)));
}

/// F2, the future form: a claim far ahead is clamped at the first delivery, and the replay later
/// (the clock has moved on) must not slide the cut to the new "now" and delete what arrived in
/// between, nor again at every resync for days.
#[test]
fn a_clear_in_the_future_applied_twice_does_not_move_the_cut() {
    let (_dir, mut h) = wa();
    let now = h.now();
    let chat = peer_chat();
    let claim = now + 10 * DAY;
    h.apply(vec![live(&chat, "L1", t(1)), cleared(&chat, Some(claim))])
        .expect("clear");
    assert!(sent_ats(&h, &chat).is_empty());
    assert_eq!(cut_of(&h, &chat), Some(now + TOLERANCE));
    // A message the network stamped half an hour ahead of this clock, arriving after the clear.
    h.apply(vec![live(&chat, "M2", now + HOUR / 2)])
        .expect("a live message");
    h.set_now(now + 2 * HOUR);
    h.apply(vec![cleared(&chat, Some(claim))]).expect("replay");
    assert_eq!(sent_ats(&h, &chat), [now + HOUR / 2], "the message stays");
    assert_eq!(
        cut_of(&h, &chat),
        Some(now + TOLERANCE),
        "the cut did not slide to the later clock"
    );
}

/// F3: this computer's clock may run minutes behind the network's. A clear whose time stands two
/// minutes ahead of it is honoured as stated: it removes a message stamped one minute ahead and
/// refuses the history row with that stamp (the exact local clock would have left both).
#[test]
fn a_clear_a_little_ahead_of_this_computers_clock_is_honoured() {
    let (_dir, mut h) = wa();
    let now = h.now();
    let chat = peer_chat();
    h.apply(vec![live(&chat, "L1", now + 60_000)])
        .expect("a message the network stamped a minute ahead");
    h.apply(vec![cleared(&chat, Some(now + 120_000))])
        .expect("clear");
    assert!(sent_ats(&h, &chat).is_empty(), "removed");
    assert_eq!(cut_of(&h, &chat), Some(now + 120_000));
    h.apply(vec![history(
        &chat,
        vec![from_peer(&chat, "H1", now + 60_000)],
    )])
    .expect("history");
    assert!(sent_ats(&h, &chat).is_empty(), "refused");
}

/// F4: a history row the cut refuses is REMEMBERED like one a clear removed, so whether the history
/// chunk or the clear came first does not decide if a later copy of the message shows. The
/// events: a chunk (H2, H4 below the cut, H9 above), the clear, and the chunk delivered again, in
/// every order; then a live copy of H2. In every order: only H9 is left, the copy is dropped,
/// each key is remembered once (a redelivered chunk adds nothing), and the dumps agree.
#[test]
fn a_late_copy_of_a_cleared_history_row_is_dropped_whatever_the_order() {
    let chat = peer_chat();
    let chunk = history(
        &chat,
        vec![
            from_peer(&chat, "H2", t(2)),
            from_peer(&chat, "H4", t(4)),
            from_peer(&chat, "H9", t(9)),
        ],
    );
    let clear = cleared(&chat, Some(t(5)));
    let mut dumps: Vec<(String, String)> = Vec::new();
    for order in [
        [&chunk, &chunk, &clear],
        [&chunk, &clear, &chunk],
        [&clear, &chunk, &chunk],
        [&clear, &clear, &chunk],
    ] {
        let name = format!(
            "{:?}",
            order
                .iter()
                .map(|e| if std::ptr::eq(*e, &clear) {
                    "clear"
                } else {
                    "chunk"
                })
                .collect::<Vec<_>>()
        );
        let (_dir, mut h) = wa();
        for e in order {
            h.apply(vec![e.clone()]).expect("apply");
        }
        assert_eq!(sent_ats(&h, &chat), [t(9)], "{name}");
        h.apply(vec![live(&chat, "H2", t(2))]).expect("a late copy");
        assert_eq!(sent_ats(&h, &chat), [t(9)], "{name}: the copy is dropped");
        assert_eq!(count(&h, "expired_messages"), 2, "{name}: H2 and H4, once");
        assert_eq!(cut_of(&h, &chat), Some(t(5)), "{name}");
        dumps.push((
            name,
            h.dump(kourier_core::testing::DumpMode::OrderInsensitive)
                .expect("dump"),
        ));
    }
    for (name, dump) in &dumps[1..] {
        let lines = |d: &str| -> std::collections::BTreeSet<String> {
            d.lines().map(str::to_owned).collect()
        };
        assert_eq!(
            lines(dump),
            lines(&dumps[0].1),
            "{name} against {}",
            dumps[0].0
        );
    }
}

/// F5: a merge that joins two cuts clears the HISTORY rows of the merged chat at or below the
/// joined cut (their keys remembered), whichever side the cut and the history came from and
/// whether the other chat existed: the end state does not depend on whether the merge came before
/// the history, between it and the clear, or after both. A live message that comes after
/// everything stays (the live message is last in every run).
#[test]
fn a_merge_clears_the_history_a_joined_cut_covers_whatever_the_order() {
    let (pn, lid) = (pn_chat(), peer_chat());
    let merge = BackendEvent::IdentityMerged {
        from: wa_user(WA_PEER_PN),
        into: wa_user(WA_PEER),
    };
    // (name, where the cut is, where the history is, whether the chat of the other exists)
    let cases = [
        ("cut on the number, history under the LID", &pn, &lid, false),
        ("cut on the LID, history under the number", &lid, &pn, false),
        (
            "both chats, cut on the LID, history under the number",
            &lid,
            &pn,
            true,
        ),
        (
            "both chats, cut on the number, history under the LID",
            &pn,
            &lid,
            true,
        ),
    ];
    for (name, cut_on, history_in, both) in cases {
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let (_dir, mut h) = wa();
            if both {
                let other = if history_in == &lid { &pn } else { &lid };
                h.apply(vec![live(other, "Q1", t(1))])
                    .expect("the other chat");
            }
            let events = [
                history(
                    history_in,
                    vec![
                        from_peer(history_in, "H2", t(2)),
                        from_peer(history_in, "H4", t(4)),
                    ],
                ),
                cleared(cut_on, Some(t(5))),
                merge.clone(),
            ];
            for i in order {
                h.apply(vec![events[i].clone()])
                    .unwrap_or_else(|e| panic!("{name} {order:?}: {e}"));
            }
            h.apply(vec![live(&lid, "L6", t(6))]).expect("live");
            assert_eq!(sent_ats(&h, &lid), [t(6)], "{name}, order {order:?}");
            assert_eq!(count(&h, "conversations"), 1, "{name}, order {order:?}");
            assert_eq!(count(&h, "conversation_cuts"), 1, "{name}, order {order:?}");
            assert_eq!(cut_of(&h, &lid), Some(t(5)), "{name}, order {order:?}");
        }
    }
}

/// F6: the command a clear made here stores keeps the two times apart as the network's range
/// does: the newest message a person sent (`before_ms`, the time of `newest[0]`) and the newest
/// notice (`system_before_ms`); a group whose newest row is a notice must not name the notice's
/// time as the time of its newest message. The cut kept for refusing history is the larger.
#[test]
fn a_clear_made_here_tells_the_time_of_the_messages_and_of_the_notices_apart() {
    let (_dir, mut h) = wa();
    let chat = peer_chat();
    let mut notice = from_peer(&chat, "N2", t(2));
    notice.body = Body::System(SystemEvent::TitleChanged {
        by: None,
        title: "x".into(),
    });
    h.apply(vec![live(&chat, "L1", t(1)), BackendEvent::Message(notice)])
        .expect("rows");
    let commands = h
        .clear_conversation(conv_of(&h, &chat), false)
        .expect("clear");
    match commands.as_slice() {
        [
            DurableCommand::ClearConversation {
                newest,
                before_ms,
                system_before_ms,
                ..
            },
        ] => {
            assert_eq!(newest.len(), 1, "the notice is no anchor");
            assert_eq!(*before_ms, Some(t(1)), "the newest message");
            assert_eq!(*system_before_ms, Some(t(2)), "the newest notice");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(cut_of(&h, &chat), Some(t(2)), "the larger of both");
}
