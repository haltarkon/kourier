//! Deleting from Kourier (§3.5.2, §4.8 durable commands), through the real core on the fake
//! network: clearing a conversation's history, deleting a conversation, and deleting one
//! attachment of a message for this account. Each is applied at once and handed to the backend
//! as a durable command, so the account's other devices follow; and the attachments of
//! messages Kourier sent carry the names the network has for them, so the same delete made on
//! the phone finds them here.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, DurableCommand, OutgoingKind};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ActionSubject, Query, QueryData, Refusal, RequestId, UiCommand, UiEvent,
};
use kourier_fake::scripts::{T0, attachment_name, remote, target};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_media::{SampleFormat, sample_image};
use kourier_model::{AccountUuid, Body, ConvKey, ConversationId, MessageId, Text};
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
