//! What a delete for me on another own device can carry besides single messages (§3.5.2;
//! Signal `SyncMessage.deleteForMe`): a conversation's history cleared up to a point
//! (`ConversationCleared`), the chat itself deleted (`remove`), and one attachment of a message
//! deleted (`AttachmentDeleted`). The core deletes rows and files (the files after the commit),
//! keeps what arrived later and what the user still has in the outbox, hides a deleted chat
//! until it is used again, and does all of it idempotently. The fake plays the phone
//! (`kourier_fake::scripts::delete_for_me`).

mod support;

use std::path::Path;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, OutgoingKind};
use kourier_core::AccountVault;
use kourier_core::facade::{
    ConversationListQuery, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::attachments::{synthetic_file, synthetic_image};
use kourier_fake::scripts::{
    DELETE_FOR_ME_PAUSE, T0, attachment_name, delete_for_me, delete_for_me_chats, own, remote,
    signal_features, target,
};
use kourier_fake::{Flavor, Script, Step, TransmitRule};
use kourier_model::{
    Attachment, Body, ConvKey, ConversationId, ConversationPatch, MsgIdScope, NetworkKind, Patch,
    SystemEvent, Text,
};
use support::send::{
    ALICE, BOB, ME, ME_PNI, alice, conv_id, d_alice, d_bob, own_messages, rate_limited, setup,
    transient, uid,
};
use support::{Env, WAIT, add_linked, eventually};

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

async fn listed(env: &Env) -> Vec<ConversationId> {
    env.conversations()
        .await
        .into_iter()
        .map(|c| c.id)
        .collect()
}

/// What a timeline shows: per row its text, or the file names of its attachments.
async fn shown(env: &Env, conv: ConversationId) -> Vec<String> {
    env.timeline(conv)
        .await
        .into_iter()
        .map(|r| match r.body {
            Body::Text(t) => t.body,
            Body::Media { attachments, .. } => attachments
                .iter()
                .map(|a| a.file_name.clone().unwrap_or_else(|| "picture".into()))
                .collect::<Vec<_>>()
                .join("+"),
            other => other.kind_name().to_owned(),
        })
        .collect()
}

async fn wait_for_script_sleep(env: &Env) {
    let clock = env.clock.clone();
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        let pause = u64::try_from(DELETE_FOR_ME_PAUSE.as_millis()).expect("fits");
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + pause) }
    })
    .await;
}

/// The whole path through the real core: the phone clears one chat up to a point, deletes
/// another and deletes single attachments. Rows go, files are unlinked, what came later stays,
/// the deleted chat leaves the list and comes back, intact, with its next message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_phone_clears_a_chat_deletes_another_and_deletes_attachments() {
    let [dana, eli, fay] = delete_for_me_chats().expect("chats");
    let mut env = Env::new(Flavor::SignalLike, delete_for_me().expect("script")).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let uuid = env.account(account).await.expect("row").uuid;
    let blobs = env.vault.blob_dir(uuid);

    // Everything is downloaded and thumbnailed first: two pictures with thumbnails and three
    // files, seven blobs.
    eventually("the downloads settled", WAIT, || {
        let db = env.core_db();
        async move {
            support::send::core_db(&db, |c| {
                c.query_row(
                    "SELECT (SELECT count(*) FROM files), (SELECT count(*) FROM attachment_downloads),
                       (SELECT count(*) FROM attachments WHERE derived_at IS NULL)",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
                )
                .is_ok_and(|n| n == (7, 0, 0))
            })
        }
    })
    .await;
    assert_eq!(blob_count(&blobs), 7);
    let db = env.core_db();
    let (d, e, f) = (
        conv_id(&db, &dana).expect("dana"),
        conv_id(&db, &eli).expect("eli"),
        conv_id(&db, &fay).expect("fay"),
    );
    assert_eq!(shown(&env, d).await.len(), 3);
    assert_eq!(shown(&env, e).await.len(), 3);
    assert_eq!(shown(&env, f).await, ["a.bin+b.bin", "picture"]);
    let before = match env.handle.query(Query::Conversation(e)).await {
        Ok(QueryData::Conversation(Some(detail))) => detail,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(before.row.title.as_deref(), Some("Eli Test"));

    // The phone deletes.
    wait_for_script_sleep(&env).await;
    env.clock.advance(DELETE_FOR_ME_PAUSE);
    let removed = env
        .event("the deleted chat leaves the list", |ev| match ev {
            UiEvent::ConversationsRemoved { ids } => Some(ids.clone()),
            _ => None,
        })
        .await;
    assert_eq!(removed, [e]);
    eventually("the attachment deletes are applied", WAIT, || {
        let env = &env;
        async move { shown(env, f).await == ["b.bin"] }
    })
    .await;

    // Dana: cleared up to our answer; the message that came after the delete stays.
    assert_eq!(shown(&env, d).await, ["This one came after the delete"]);
    // Eli: gone from the list, nothing left in it, but the conversation itself is intact.
    let list = listed(&env).await;
    assert!(
        list.contains(&d) && list.contains(&f) && !list.contains(&e),
        "{list:?}"
    );
    assert!(shown(&env, e).await.is_empty());
    let hidden = match env.handle.query(Query::Conversation(e)).await {
        Ok(QueryData::Conversation(Some(detail))) => detail,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(hidden.row.title.as_deref(), Some("Eli Test"));
    assert_eq!(
        hidden.features, before.features,
        "its features are still there"
    );
    assert_eq!(hidden.row.unread, 0);
    // Fay: one of two files is gone, and the message whose only picture was deleted with it.
    let fay_rows = env.timeline(f).await;
    assert_eq!(fay_rows.len(), 1);
    let Body::Media { attachments, .. } = &fay_rows[0].body else {
        panic!("not media");
    };
    assert_eq!(attachments.len(), 1);
    assert!(attachments[0].id.is_some(), "the row behind moved up");
    assert!(
        attachments[0].net_ids.is_empty(),
        "the network's names of an attachment stay in the core"
    );
    // The files went after the commit: only b.bin is left on disk and in the table.
    eventually("the files are unlinked", WAIT, || {
        let blobs = blobs.clone();
        async move { blob_count(&blobs) == 1 }
    })
    .await;
    let files: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .expect("files")
    });
    assert_eq!(files, 1);

    // Eli writes again: the chat is back in the list with its title and one unread message.
    wait_for_script_sleep(&env).await;
    env.clock.advance(DELETE_FOR_ME_PAUSE);
    eventually("the chat is listed again", WAIT, || {
        let env = &env;
        async move { listed(env).await.contains(&e) }
    })
    .await;
    let back = env
        .conversations()
        .await
        .into_iter()
        .find(|c| c.id == e)
        .expect("eli");
    assert_eq!(back.title.as_deref(), Some("Eli Test"));
    assert_eq!(back.unread, 1);
    assert_eq!(shown(&env, e).await, ["Are you still there?"]);
    env.stop().await;
}

/// A message the user wrote here that is still in the outbox is not lost to a delete made on
/// the phone, which never had it; and a chat with such a message is not hidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_still_in_the_outbox_survives_the_clear() {
    let later = T0 + 5000;
    let script = Script::new("clear-keeps-queued", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .fail_transmit(TransmitRule::attempts(transient(), u32::MAX).in_conv(d_alice()))
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver_ordered(vec![
            BackendEvent::Message(remote(
                &d_alice(),
                &alice(),
                &later.to_string(),
                later,
                "never mind",
            )),
            BackendEvent::ConversationCleared {
                conv: d_alice(),
                up_to: vec![target(Some(&d_alice()), Some(&alice()), &later.to_string())],
                remove: true,
                at: later + 1,
            },
        ]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let a = {
        let db = env.core_db();
        eventually("alice's chat", WAIT, || {
            let db = db.clone();
            async move { conv_id(&db, &d_alice()).is_some() }
        })
        .await;
        conv_id(&db, &d_alice()).expect("alice")
    };
    env.handle
        .send(UiCommand::SendText {
            conv: a,
            text: Text::plain("written here, not sent yet"),
            reply_to: None,
        })
        .expect("send");
    eventually("the message waits in the outbox", WAIT, || {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 1 }
    })
    .await;
    let clock = env.clock.clone();
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + 5000) }
    })
    .await;
    env.clock.advance(Duration::from_secs(5));
    env.wait_finished(account).await;
    eventually("the clear is applied", WAIT, || {
        let env = &env;
        async move { shown(env, a).await.len() == 1 }
    })
    .await;
    assert_eq!(shown(&env, a).await, ["written here, not sent yet"]);
    assert_eq!(own_messages(&env.core_db(), a).len(), 1);
    assert_eq!(
        support::send::outbox_len(&env.core_db()),
        1,
        "still to be sent"
    );
    assert!(
        listed(&env).await.contains(&a),
        "a chat with a message is not hidden"
    );
    env.stop().await;
}

/// The deletes for everyone (`kind`, id) transmitted so far for the message with id `local`.
fn deletes_for_everyone(env: &Env, uuid: kourier_model::AccountUuid, local: &str) -> usize {
    env.control
        .transmits(uuid)
        .iter()
        .filter(|t| {
            matches!(
                &t.req.kind,
                OutgoingKind::Delete { target, for_everyone: true } if target.local.as_ref() == local
            )
        })
        .count()
}

/// A delete for everyone the user asked for here is still sent when the phone deletes the chat
/// before it went out (§4.8 step 7: a queued delete always stays). The message's row carries
/// its outbox rows, so the clear leaves that one row, but hides it: the phone deleted the
/// chat, so nothing of it shows any more, the chat leaves the list, and when it is used
/// again later the old tombstone does not come back as "deleted". Everything else of the
/// chats goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_for_everyone_that_still_waits_survives_the_clear() {
    let later = T0 + 5000;
    let wait = Duration::from_secs(600);
    let held = |conv: ConvKey| {
        TransmitRule::attempts(rate_limited(Some(wait)), 1)
            .of_kind("Delete")
            .in_conv(conv)
    };
    let script = Script::new("clear-keeps-queued-delete", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .fail_transmit(held(d_alice()))
        .fail_transmit(held(d_bob()))
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(5)))
        .deliver_ordered(vec![
            BackendEvent::Message(remote(
                &d_alice(),
                &alice(),
                &later.to_string(),
                later,
                "never mind",
            )),
            // The phone deletes both chats: alice's up to her newest message, bob's (where
            // nothing shows any more) as a whole.
            BackendEvent::ConversationCleared {
                conv: d_alice(),
                up_to: vec![target(Some(&d_alice()), Some(&alice()), &later.to_string())],
                remove: true,
                at: later + 1,
            },
            BackendEvent::ConversationCleared {
                conv: d_bob(),
                up_to: Vec::new(),
                remove: true,
                at: later + 2,
            },
        ])
        // Later alice writes again: her chat comes back.
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(700)))
        .deliver(vec![BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            &(later + 900_000).to_string(),
            later + 900_000,
            "hello again",
        ))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let uuid = env.account(account).await.expect("row").uuid;
    let db = env.core_db();
    eventually("the chats", WAIT, || {
        let db = db.clone();
        async move { conv_id(&db, &d_alice()).is_some() && conv_id(&db, &d_bob()).is_some() }
    })
    .await;
    let (a, b) = (
        conv_id(&db, &d_alice()).expect("alice"),
        conv_id(&db, &d_bob()).expect("bob"),
    );
    // In each chat: a message that is sent, then deleted for everyone. The delete is told to
    // wait ten minutes by the server and sits in the outbox.
    let mut mine = Vec::new();
    for conv in [a, b] {
        env.handle
            .send(UiCommand::SendText {
                conv,
                text: Text::plain("said too much"),
                reply_to: None,
            })
            .expect("send");
        eventually("the message is sent", WAIT, || {
            let db = db.clone();
            async move {
                own_messages(&db, conv).first().is_some_and(|m| {
                    matches!(m.out_state, Some(kourier_model::OutgoingState::Sent { .. }))
                })
            }
        })
        .await;
        let own = own_messages(&db, conv).remove(0);
        env.handle
            .send(UiCommand::DeleteMessage {
                message: own.id,
                for_everyone: true,
            })
            .expect("send");
        eventually("the delete waits out the server's time", WAIT, || {
            let (db, id) = (db.clone(), own.id);
            async move {
                support::send::core_db(&db, |c| {
                    c.query_row(
                        "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Delete'
                           AND state = 'Queued' AND next_attempt_at IS NOT NULL",
                        [id.0],
                        |r| r.get::<_, i64>(0),
                    )
                    .is_ok_and(|n| n == 1)
                })
            }
        })
        .await;
        mine.push(own);
    }
    // In bob's chat the user also deletes the tombstone for themselves: nothing of it shows.
    env.handle
        .send(UiCommand::DeleteMessage {
            message: mine[1].id,
            for_everyone: false,
        })
        .expect("send");
    eventually("the tombstone is hidden", WAIT, || {
        let env = &env;
        async move { shown(env, b).await.is_empty() }
    })
    .await;
    for own in &mine {
        assert_eq!(deletes_for_everyone(&env, uuid, &own.local), 1);
    }

    // The phone's deletes arrive while both deletes for everyone still wait.
    let clock = env.clock.clone();
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + 5000) }
    })
    .await;
    env.clock.advance(Duration::from_secs(5));
    eventually("the clears are applied", WAIT, || {
        let env = &env;
        async move {
            let listed = listed(env).await;
            shown(env, a).await.is_empty() && !listed.contains(&a) && !listed.contains(&b)
        }
    })
    .await;
    // (The script now waits for the time alice writes again.)
    wait_for_sleep(&env, Duration::from_secs(700)).await;
    let rows = |conv: ConversationId| {
        support::send::core_db(&db, |c| {
            c.query_row(
                "SELECT count(*) FROM messages WHERE conversation_id = ?1",
                [conv.0],
                |r| r.get::<_, i64>(0),
            )
            .expect("count")
        })
    };
    assert_eq!(
        (rows(a), rows(b)),
        (1, 1),
        "only the rows that carry a delete are left"
    );
    assert!(shown(&env, b).await.is_empty(), "bob's chat shows nothing");
    let hidden: i64 = support::send::core_db(&db, |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE deleted = 'ForMe'",
            [],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(hidden, 2, "both rows are kept unseen");
    for own in &mine {
        let waiting: i64 = support::send::core_db(&db, |c| {
            c.query_row(
                "SELECT count(*) FROM outbox WHERE message_id = ?1 AND kind = 'Delete'",
                [own.id.0],
                |r| r.get(0),
            )
            .expect("count")
        });
        assert!(waiting >= 1, "the delete for everyone is still to be sent");
    }

    // The server's time is up: both deletes for everyone go out.
    env.clock.advance(wait);
    for own in &mine {
        eventually("the delete for everyone is sent", WAIT, || {
            let (env, local) = (&env, own.local.clone());
            async move { deletes_for_everyone(env, uuid, &local) == 2 }
        })
        .await;
    }
    eventually("the outbox is empty", WAIT, || {
        let db = db.clone();
        async move { support::send::outbox_len(&db) == 0 }
    })
    .await;

    // Alice writes again: her chat is listed again and shows her message, not the old
    // tombstone.
    env.clock.advance(Duration::from_secs(700) - wait);
    env.wait_finished(account).await;
    eventually("the chat is back", WAIT, || {
        let env = &env;
        async move { listed(env).await.contains(&a) }
    })
    .await;
    assert_eq!(shown(&env, a).await, ["hello again"]);
    assert!(!listed(&env).await.contains(&b));
    env.stop().await;
}

/// Waits until something sleeps for `pause` from now on the fake clock.
async fn wait_for_sleep(env: &Env, pause: Duration) {
    let clock = env.clock.clone();
    eventually("the script waits", WAIT, || {
        let c = clock.clone();
        let pause = u64::try_from(pause.as_millis()).expect("fits");
        async move { c.deadlines().iter().any(|d| *d == c.now_ms() + pause) }
    })
    .await;
}

// ---------- ingest rules, on the harness ----------

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([5; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn chat(key: &ConvKey) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(false));
    p.title = Patch::Set("A chat".into());
    BackendEvent::ConversationUpsert(p)
}

fn at(n: i64) -> String {
    (T0 + n * 1000).to_string()
}

fn msg(key: &ConvKey, n: i64, text: &str) -> BackendEvent {
    BackendEvent::Message(remote(key, &alice(), &at(n), T0 + n * 1000, text))
}

fn notice(key: &ConvKey, n: i64) -> BackendEvent {
    let mut m = remote(key, &alice(), &at(n), T0 + n * 1000, "");
    m.body = Body::System(SystemEvent::TimerChanged {
        by: Some(alice()),
        timer: None,
    });
    BackendEvent::Message(m)
}

fn cleared(key: &ConvKey, up_to: &[i64], remove: bool) -> BackendEvent {
    BackendEvent::ConversationCleared {
        conv: key.clone(),
        up_to: up_to
            .iter()
            .map(|n| target(Some(key), Some(&alice()), &at(*n)))
            .collect(),
        remove,
        at: T0 + 99_000,
    }
}

fn texts(h: &IngestHarness, conv: ConversationId) -> Vec<String> {
    h.timeline(&TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 100,
    })
    .expect("timeline")
    .rows
    .into_iter()
    .map(|r| match r.body {
        Body::Text(t) => t.body,
        Body::Media {
            attachments,
            caption,
        } => format!(
            "{} file(s){}",
            attachments.len(),
            caption.map(|c| format!(": {}", c.body)).unwrap_or_default()
        ),
        other => other.kind_name().to_owned(),
    })
    .collect()
}

fn conv_of(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("conversation")
}

fn in_list(h: &IngestHarness, conv: ConversationId) -> bool {
    h.conversations(&ConversationListQuery::default())
        .expect("list")
        .rows
        .iter()
        .any(|c| c.id == conv)
}

#[test]
fn a_clear_deletes_up_to_the_newest_anchor_that_is_here_and_only_in_its_chat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let other = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-000000000b0b"));
    h.apply(vec![
        chat(&d_alice()),
        chat(&other),
        msg(&d_alice(), 1, "one"),
        msg(&d_alice(), 2, "two"),
        msg(&other, 3, "elsewhere"),
        msg(&d_alice(), 4, "four"),
        msg(&d_alice(), 5, "five"),
    ])
    .expect("apply");
    let (a, o) = (conv_of(&h, &d_alice()), conv_of(&h, &other));

    // No anchor is here (a clear that arrives before its messages, or names messages that
    // expired here): nothing is deleted.
    h.apply(vec![cleared(&d_alice(), &[70, 71], false)])
        .expect("apply");
    assert_eq!(texts(&h, a).len(), 4);
    // An anchor that is a message of another chat is no anchor (ids are account-wide here).
    h.apply(vec![cleared(&d_alice(), &[3], false)])
        .expect("apply");
    assert_eq!(texts(&h, a).len(), 4);
    assert_eq!(texts(&h, o), ["elsewhere"]);

    // The newest anchor that is here decides, whatever the order of the list; an unknown one
    // among them is skipped.
    h.apply(vec![cleared(&d_alice(), &[2, 70, 4, 1], false)])
        .expect("apply");
    assert_eq!(texts(&h, a), ["five"]);
    assert_eq!(texts(&h, o), ["elsewhere"], "the other chat is untouched");
    assert!(in_list(&h, a));

    // Applying it again changes nothing (at-least-once delivery).
    let once = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![cleared(&d_alice(), &[2, 70, 4, 1], false)])
        .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), once);
    let unread: i64 = h
        .connection()
        .query_row(
            "SELECT unread FROM conversations WHERE id = ?1",
            [a.0],
            |r| r.get(0),
        )
        .expect("unread");
    assert_eq!(unread, 1, "the counter follows the rows");
}

#[test]
fn a_deleted_chat_is_hidden_only_when_nothing_a_person_sent_is_left() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        notice(&d_alice(), 2),
        msg(&d_alice(), 3, "three"),
        notice(&d_alice(), 4),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());

    // A full delete whose newest message is older than what is here: the newer message keeps
    // the chat, with the notice behind it.
    h.apply(vec![cleared(&d_alice(), &[1], true)])
        .expect("apply");
    assert_eq!(texts(&h, a), ["System", "three", "System"]);
    assert!(in_list(&h, a));
    // Signal's local-only delete (no anchors): refused while a message is left.
    h.apply(vec![cleared(&d_alice(), &[], true)])
        .expect("apply");
    assert_eq!(texts(&h, a).len(), 3);
    assert!(in_list(&h, a));

    // Up to the last message: only a notice is left, it goes too and the chat is hidden.
    h.apply(vec![cleared(&d_alice(), &[3], true)])
        .expect("apply");
    assert!(texts(&h, a).is_empty());
    assert!(!in_list(&h, a));
    let rows: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 0);
    // Idempotent, and the row with its title and features is still there.
    let once = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![cleared(&d_alice(), &[3], true)])
        .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), once);
    let (title, features): (Option<String>, Option<String>) = h
        .connection()
        .query_row(
            "SELECT title, features FROM conversations WHERE id = ?1",
            [a.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert!(title.is_some() && features.is_some());

    // Its next message brings it back, and a delete without `remove` of an emptied chat
    // leaves it listed.
    h.apply(vec![msg(&d_alice(), 6, "six")]).expect("apply");
    assert!(in_list(&h, a));
    assert_eq!(texts(&h, a), ["six"]);
    h.apply(vec![cleared(&d_alice(), &[6], false)])
        .expect("apply");
    assert!(texts(&h, a).is_empty());
    assert!(in_list(&h, a), "emptied, not deleted");
    // Only notices: a local-only delete hides it.
    h.apply(vec![notice(&d_alice(), 7), cleared(&d_alice(), &[], true)])
        .expect("apply");
    assert!(!in_list(&h, a));
}

fn media(key: &ConvKey, n: i64, files: Vec<Attachment>, caption: Option<&str>) -> BackendEvent {
    let mut m = remote(key, &alice(), &at(n), T0 + n * 1000, "");
    let files = files
        .into_iter()
        .enumerate()
        .map(|(i, mut a)| {
            a.net_ids = vec![attachment_name(&at(n), i), format!("digest:{n}:{i}")];
            a
        })
        .collect();
    m.body = Body::Media {
        attachments: files,
        caption: caption.map(Text::plain),
    };
    BackendEvent::Message(m)
}

fn attachment_deleted(key: &ConvKey, n: i64, names: &[&str]) -> BackendEvent {
    BackendEvent::AttachmentDeleted {
        target: target(Some(key), Some(&alice()), &at(n)),
        net_ids: names
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into(),
        at: T0 + 99_000,
    }
}

fn attachment_rows(h: &IngestHarness) -> Vec<(i64, Option<String>)> {
    let mut stmt = h
        .connection()
        .prepare(
            "SELECT a.idx, a.file_name FROM attachments a JOIN messages m ON m.id = a.message_id
             ORDER BY m.id, a.idx",
        )
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

#[test]
fn one_attachment_of_a_message_is_deleted_by_any_of_its_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let file = |name: &str, seed| synthetic_file(name, "application/octet-stream", 100, seed);
    h.apply(vec![
        chat(&d_alice()),
        media(
            &d_alice(),
            1,
            vec![file("a.bin", 1), file("b.bin", 2), file("c.bin", 3)],
            Some("three files"),
        ),
        media(&d_alice(), 2, vec![synthetic_image(64, 64, 4)], None),
        media(&d_alice(), 3, vec![file("d.bin", 5)], Some("with words")),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    // The caption of the first message is edited: the delete must reach every stored version.
    h.apply(vec![BackendEvent::Edited {
        target: target(Some(&d_alice()), Some(&alice()), &at(1)),
        by: Some(alice()),
        new_local: Some(at(10).into()),
        body: Body::text("three files, edited"),
        edited_at: T0 + 10_000,
        keeps_quote: None,
    }])
    .expect("apply");

    // Names nobody has: nothing changes. A message that is not here: the delete waits for it
    // (two days), and changes nothing either.
    let before = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![
        attachment_deleted(&d_alice(), 1, &["digest:9:9"]),
        attachment_deleted(&d_alice(), 2, &["digest:1:0"]),
    ])
    .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);
    h.apply(vec![attachment_deleted(&d_alice(), 77, &["digest:1:0"])])
        .expect("apply");
    let waiting: Vec<String> = h
        .dump(DumpMode::Exact)
        .expect("dump")
        .lines()
        .filter(|l| !before.contains(*l))
        .map(str::to_owned)
        .collect();
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert!(waiting[0].starts_with("pending|") && waiting[0].contains("AttachmentDeleted"));

    // The middle file, named by its second name: its row goes, the one behind moves up, in
    // the rows and in both stored versions of the message.
    let target_of_b: String = h
        .connection()
        .query_row(
            "SELECT d.path FROM attachment_downloads d JOIN attachments a ON a.id = d.attachment_id
             WHERE a.file_name = 'b.bin'",
            [],
            |r| r.get(0),
        )
        .expect("the download in flight");
    let unlinked = h
        .apply_unlinking(vec![attachment_deleted(
            &d_alice(),
            1,
            &["unknown", "digest:1:1"],
        )])
        .expect("apply");
    assert_eq!(
        unlinked,
        [std::path::PathBuf::from(target_of_b)],
        "what its download had written so far is unlinked after the commit, and nothing else"
    );
    let downloads: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM attachment_downloads", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert_eq!(downloads, 4, "the other four downloads go on");
    assert_eq!(
        attachment_rows(&h),
        [
            (0, Some("a.bin".to_owned())),
            (1, Some("c.bin".to_owned())),
            (0, None),
            (0, Some("d.bin".to_owned()))
        ]
    );
    assert_eq!(
        texts(&h, a),
        [
            "2 file(s): three files, edited",
            "1 file(s)",
            "1 file(s): with words"
        ]
    );
    let first = h
        .timeline(&TimelineQuery {
            conv: a,
            anchor: TimelineAnchor::Newest,
            limit: 10,
        })
        .expect("timeline")
        .rows[0]
        .id;
    let versions = h.revisions(first).expect("revisions").expect("message");
    assert_eq!(
        versions
            .revisions
            .iter()
            .map(|r| r.text.as_ref().map(|t| t.body.clone()))
            .collect::<Vec<_>>(),
        [
            Some("three files".to_owned()),
            Some("three files, edited".to_owned())
        ],
        "the captions of both versions are kept"
    );
    // Once more: the name is gone with the attachment, nothing else goes.
    let once = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![attachment_deleted(&d_alice(), 1, &["digest:1:1"])])
        .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), once);

    // The only attachment of a message with a caption: the caption stays, as a text.
    h.apply(vec![attachment_deleted(&d_alice(), 3, &["digest:3:0"])])
        .expect("apply");
    // The only attachment of a message without one: nothing is left to show, the message is
    // hidden like a delete for me.
    h.apply(vec![attachment_deleted(&d_alice(), 2, &["digest:2:0"])])
        .expect("apply");
    assert_eq!(
        texts(&h, a),
        ["2 file(s): three files, edited", "with words"]
    );
    let deleted: Option<String> = h
        .connection()
        .query_row(
            "SELECT deleted FROM messages WHERE net_local = ?1",
            [at(2)],
            |r| r.get(0),
        )
        .expect("row");
    assert_eq!(deleted.as_deref(), Some("ForMe"));
    assert_eq!(attachment_rows(&h).len(), 2);
}

/// The files of what a clear deletes are handed to the unlink that runs after the commit.
#[test]
fn a_clear_hands_the_files_of_its_messages_to_the_unlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&d_alice()),
        media(&d_alice(), 1, vec![synthetic_image(64, 64, 4)], None),
        BackendEvent::Message(own(&d_alice(), &at(2), T0 + 2000, "nice")),
    ])
    .expect("apply");
    // A file of the attachment, as a finished download records it.
    h.connection()
        .execute_batch(
            "INSERT INTO files(account_id, path, key, size, mime, purpose)
               VALUES (1, 'core/ab/photo', x'00', 10, 'image/jpeg', 'attachment');
             UPDATE attachments SET file_id = last_insert_rowid(), state = '\"Local\"';",
        )
        .expect("file");
    let unlinked = h
        .apply_unlinking(vec![BackendEvent::ConversationCleared {
            conv: d_alice(),
            up_to: vec![target(Some(&d_alice()), None, &at(2))],
            remove: true,
            at: T0 + 3000,
        }])
        .expect("apply");
    assert!(
        unlinked.iter().any(|p| p == Path::new("core/ab/photo")),
        "{unlinked:?}"
    );
    let left: (i64, i64, i64) = h
        .connection()
        .query_row(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM attachments),
               (SELECT count(*) FROM files)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("counts");
    assert_eq!(left, (0, 0, 0));
}

/// A chat deleted on the phone under one identity of a contact is hidden. When the contact's
/// two identities turn out to be one person, the chat under the other identity joins it: its
/// messages are still here, so the joined chat is listed. Only two deleted chats stay deleted.
#[test]
fn a_hidden_chat_that_takes_in_a_listed_one_is_listed() {
    let carol = uid("aci:00000000-0000-4000-8000-0000000ca202");
    let carol_pni = uid("pni:00000000-0000-4000-8000-0000000ca201");
    let (by_aci, by_pni) = (
        ConvKey::Direct(carol.clone()),
        ConvKey::Direct(carol_pni.clone()),
    );
    let said = |key: &ConvKey, by: &kourier_model::UserId, n: i64, text: &str| {
        BackendEvent::Message(remote(key, by, &at(n), T0 + n * 1000, text))
    };
    let deleted_on_the_phone = |key: &ConvKey, n: i64| BackendEvent::ConversationCleared {
        conv: key.clone(),
        up_to: vec![target(Some(key), None, &at(n))],
        remove: true,
        at: T0 + 50_000,
    };
    let merge = || BackendEvent::IdentityMerged {
        from: carol_pni.clone(),
        into: carol.clone(),
    };

    // Her chat under the account identity was deleted on the phone; the one under the number
    // identity still has a message.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&by_aci),
        chat(&by_pni),
        BackendEvent::Message(own(&by_aci, &at(1), T0 + 1000, "hello")),
        said(&by_pni, &carol_pni, 2, "who is this?"),
        deleted_on_the_phone(&by_aci, 1),
    ])
    .expect("apply");
    let (aci, pni) = (conv_of(&h, &by_aci), conv_of(&h, &by_pni));
    assert!(!in_list(&h, aci) && in_list(&h, pni));
    h.apply(vec![merge()]).expect("merge");
    let joined = conv_of(&h, &by_aci);
    assert_eq!(texts(&h, joined), ["who is this?"]);
    assert!(
        in_list(&h, joined),
        "a chat with a message is in the list, whichever side was deleted"
    );

    // Both were deleted: the joined chat stays out of the list.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&by_aci),
        chat(&by_pni),
        BackendEvent::Message(own(&by_aci, &at(1), T0 + 1000, "hello")),
        BackendEvent::Message(own(&by_pni, &at(2), T0 + 2000, "hello again")),
        deleted_on_the_phone(&by_aci, 1),
        deleted_on_the_phone(&by_pni, 2),
        merge(),
    ])
    .expect("apply");
    let joined = conv_of(&h, &by_aci);
    assert!(texts(&h, joined).is_empty());
    assert!(!in_list(&h, joined));
}

// ---------- phase I: deletes that overtake their messages, memory of what a clear removed ----------

/// What is parked, as `(kind, target id)` pairs.
fn parked(h: &IngestHarness) -> Vec<(String, String)> {
    let mut stmt = h
        .connection()
        .prepare("SELECT kind, target_local FROM pending_modifiers ORDER BY target_local, kind")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// A clear can overtake the messages it names (a backend holds messages back while the sync
/// message passes). It waits for them: when none of its anchors was here, what the
/// conversation held when it came goes with the first anchor that arrives, and every anchor
/// goes as it arrives; what arrived after the clear stays. The outcome is the one of the
/// in-order arrival.
#[test]
fn a_clear_that_overtakes_its_anchors_is_finished_when_they_arrive() {
    let late = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&late);
    h.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        msg(&d_alice(), 2, "two"),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    // The phone cleared up to 4 and 5, which this device has not been given yet.
    h.apply(vec![cleared(&d_alice(), &[5, 4], false)])
        .expect("apply");
    assert_eq!(texts(&h, a), ["one", "two"], "nothing to delete up to yet");
    assert_eq!(
        parked(&h),
        [
            ("ConversationCleared".to_owned(), at(4)),
            ("ConversationCleared".to_owned(), at(5))
        ]
    );
    // A replay of the clear waits once.
    let once = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![cleared(&d_alice(), &[5, 4], false)])
        .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), once);

    // A message the phone got after its clear arrives here in between: it stays.
    h.apply(vec![msg(&d_alice(), 6, "after the clear")])
        .expect("apply");
    // The first anchor arrives: it goes, and with it what the clear found here.
    h.apply(vec![msg(&d_alice(), 4, "four")]).expect("apply");
    assert_eq!(texts(&h, a), ["after the clear"]);
    assert_eq!(parked(&h), [("ConversationCleared".to_owned(), at(5))]);
    // The second goes alone.
    h.apply(vec![msg(&d_alice(), 5, "five")]).expect("apply");
    assert_eq!(texts(&h, a), ["after the clear"]);
    assert!(parked(&h).is_empty());
    assert!(in_list(&h, a));
    let unread: i64 = h
        .connection()
        .query_row(
            "SELECT unread FROM conversations WHERE id = ?1",
            [a.0],
            |r| r.get(0),
        )
        .expect("unread");
    assert_eq!(
        unread, 1,
        "an anchor deleted on arrival is not counted unread"
    );

    // The same events in the order they happened on the phone: the same messages are left.
    let ordered = tempfile::tempdir().expect("tempdir");
    let mut o = harness(&ordered);
    o.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        msg(&d_alice(), 2, "two"),
        msg(&d_alice(), 4, "four"),
        msg(&d_alice(), 5, "five"),
        cleared(&d_alice(), &[5, 4], false),
        msg(&d_alice(), 6, "after the clear"),
    ])
    .expect("apply");
    let oa = conv_of(&o, &d_alice());
    assert_eq!(texts(&o, oa), texts(&h, a));
    let rows = |h: &IngestHarness| -> Vec<String> {
        h.dump(DumpMode::OrderInsensitive)
            .expect("dump")
            .lines()
            .filter(|l| l.starts_with("msg|") || l.starts_with("pending|"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(rows(&o), rows(&h), "both orders end with the same messages");
}

/// With an anchor here the clear deletes up to it at once; an anchor that comes later goes
/// alone, and what arrived between the two stays (the phone kept it too).
#[test]
fn a_late_anchor_of_a_clear_that_was_applied_goes_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        msg(&d_alice(), 2, "two"),
        msg(&d_alice(), 3, "three, not on the phone when it cleared"),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    h.apply(vec![cleared(&d_alice(), &[5, 2], false)])
        .expect("apply");
    assert_eq!(texts(&h, a), ["three, not on the phone when it cleared"]);
    h.apply(vec![msg(&d_alice(), 5, "five")]).expect("apply");
    assert_eq!(
        texts(&h, a),
        ["three, not on the phone when it cleared"],
        "only the anchor went"
    );
    assert!(parked(&h).is_empty());
}

/// A deleted chat whose messages come late: the chat is hidden once its anchor arrived and
/// went, and a conversation the clear did not even find yet is handled the same way.
#[test]
fn a_chat_delete_that_overtakes_its_messages_hides_the_chat_when_they_arrive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let bob_chat = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-000000000b0b"));
    h.apply(vec![
        chat(&d_alice()),
        notice(&d_alice(), 1),
        cleared(&d_alice(), &[2], true),
        // A chat this device has never heard of.
        cleared(&bob_chat, &[3], true),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    // Only a notice is left, so the chat is hidden at once; its anchor is still waited for.
    assert!(!in_list(&h, a));
    assert_eq!(parked(&h).len(), 2);
    h.apply(vec![msg(&d_alice(), 2, "two"), msg(&bob_chat, 3, "three")])
        .expect("apply");
    assert!(texts(&h, a).is_empty());
    assert!(
        !in_list(&h, a),
        "the late anchor does not bring the chat back"
    );
    let b = conv_of(&h, &bob_chat);
    assert!(texts(&h, b).is_empty());
    assert!(!in_list(&h, b));
    assert!(parked(&h).is_empty());
    // A message that is not an anchor does bring a chat back.
    h.apply(vec![msg(&d_alice(), 7, "seven")]).expect("apply");
    assert!(in_list(&h, a));
}

/// A waiting clear is kept across a restart, only for its own chat, and for two days.
#[test]
fn a_waiting_clear_survives_a_restart_and_ends_after_two_days() {
    let dir = tempfile::tempdir().expect("tempdir");
    let other = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-000000000b0b"));
    {
        let mut h = harness(&dir);
        h.apply(vec![
            chat(&d_alice()),
            chat(&other),
            msg(&d_alice(), 1, "one"),
            cleared(&d_alice(), &[4, 5], false),
        ])
        .expect("apply");
        assert_eq!(parked(&h).len(), 2);
    }
    // The process is gone; the next one finds the clear in core.db.
    let mut h = harness(&dir);
    let a = conv_of(&h, &d_alice());
    assert_eq!(texts(&h, a), ["one"]);
    // The id of an anchor in another chat is another message: the clear does not touch it.
    h.apply(vec![msg(&other, 4, "four, elsewhere")])
        .expect("apply");
    assert_eq!(texts(&h, conv_of(&h, &other)), ["four, elsewhere"]);
    assert_eq!(texts(&h, a), ["one"]);
    // Two days later the clear is given up: its anchor is a message like any other.
    h.set_now(1_700_000_000_000 + 2 * 24 * 3600 * 1000 + 1);
    h.apply(vec![msg(&d_alice(), 5, "five")]).expect("apply");
    assert_eq!(texts(&h, a), ["one", "five"]);
    assert!(parked(&h).is_empty());
}

/// What a clear removed does not come back: a later copy of a removed message (a redelivered
/// envelope, a history import) is dropped, also after a restart, and leaves no empty chat.
#[test]
fn a_late_copy_of_a_cleared_message_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let mut h = harness(&dir);
        h.apply(vec![
            chat(&d_alice()),
            msg(&d_alice(), 1, "one"),
            msg(&d_alice(), 2, "two"),
            msg(&d_alice(), 3, "three"),
            cleared(&d_alice(), &[2], false),
        ])
        .expect("apply");
        let a = conv_of(&h, &d_alice());
        assert_eq!(texts(&h, a), ["three"]);
        h.apply(vec![msg(&d_alice(), 1, "one"), msg(&d_alice(), 2, "two")])
            .expect("apply");
        assert_eq!(texts(&h, a), ["three"], "the copies are dropped");
    }
    let mut h = harness(&dir);
    let a = conv_of(&h, &d_alice());
    // After a restart, as a live copy and in a history import.
    let BackendEvent::Message(old) = msg(&d_alice(), 1, "one") else {
        unreachable!()
    };
    h.apply(vec![
        msg(&d_alice(), 2, "two"),
        BackendEvent::HistoryBatch {
            conv: d_alice(),
            messages: vec![old],
            has_more: false,
            progress: None,
        },
    ])
    .expect("apply");
    assert_eq!(texts(&h, a), ["three"]);
    // A message the clear did not remove is taken as ever.
    h.apply(vec![msg(&d_alice(), 4, "four")]).expect("apply");
    assert_eq!(texts(&h, a), ["three", "four"]);
    // The memory ends with the server's retention, like that of expired messages.
    let day = 24 * 3600 * 1000;
    h.set_now(1_700_000_000_000 + 46 * day);
    h.apply(vec![msg(&d_alice(), 1, "one")]).expect("apply");
    assert_eq!(
        texts(&h, a).len(),
        3,
        "nothing that old is still on a server"
    );
}

/// A delete of one attachment waits for a message that is not here yet, like other modifiers.
#[test]
fn an_attachment_delete_that_overtakes_its_message_is_applied_on_arrival() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let file = |name: &str, seed| synthetic_file(name, "application/octet-stream", 100, seed);
    let name = attachment_name(&at(1), 0);
    h.apply(vec![
        chat(&d_alice()),
        attachment_deleted(&d_alice(), 1, &[name.as_str()]),
    ])
    .expect("apply");
    assert_eq!(parked(&h), [("AttachmentDeleted".to_owned(), at(1))]);
    // A replay waits once.
    h.apply(vec![attachment_deleted(&d_alice(), 1, &[name.as_str()])])
        .expect("apply");
    assert_eq!(parked(&h).len(), 1);
    h.apply(vec![media(
        &d_alice(),
        1,
        vec![file("a.bin", 1), file("b.bin", 2)],
        Some("two files"),
    )])
    .expect("apply");
    assert_eq!(attachment_rows(&h), [(0, Some("b.bin".to_owned()))]);
    assert_eq!(texts(&h, conv_of(&h, &d_alice())), ["1 file(s): two files"]);
    assert!(parked(&h).is_empty());
    // It waits two days at most.
    let other = attachment_name(&at(2), 0);
    h.apply(vec![attachment_deleted(&d_alice(), 2, &[other.as_str()])])
        .expect("apply");
    h.set_now(1_700_000_000_000 + 2 * 24 * 3600 * 1000 + 1);
    h.apply(vec![media(&d_alice(), 2, vec![file("c.bin", 3)], None)])
        .expect("apply");
    assert_eq!(attachment_rows(&h).len(), 2, "the delete was given up");
    assert!(parked(&h).is_empty());
}

// ---------- phase I: deletes made on this device ----------

fn clear_command(
    c: &kourier_backend_api::DurableCommand,
) -> (ConvKey, Vec<String>, Vec<String>, bool) {
    match c {
        kourier_backend_api::DurableCommand::ClearConversation {
            conv,
            newest,
            newest_non_expiring,
            remove,
        } => (
            conv.clone(),
            newest.iter().map(|t| t.local.to_string()).collect(),
            newest_non_expiring
                .iter()
                .map(|t| t.local.to_string())
                .collect(),
            *remove,
        ),
        other => panic!("unexpected {other:?}"),
    }
}

/// Clearing a chat here: every message goes with its files, the chat stays listed, the
/// backend is handed the newest messages (five at most, newest first, the lasting ones apart)
/// as a durable command stored with the delete, and what went stays gone.
#[test]
fn clearing_a_chat_here_deletes_it_and_tells_the_other_devices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut events = vec![chat(&d_alice())];
    for n in 1..=7 {
        events.push(msg(&d_alice(), n, &format!("message {n}")));
    }
    // The two newest disappear after a while; a notice is no message a device can name.
    for n in [6, 7] {
        let BackendEvent::Message(m) = &mut events[usize::try_from(n).expect("index")] else {
            unreachable!()
        };
        m.expiry = Some(kourier_model::Expiry {
            kind: kourier_model::DisappearKind::AfterRead,
            timer: Duration::from_secs(3600),
            started_at: None,
        });
    }
    events.push(notice(&d_alice(), 8));
    events.push(BackendEvent::Message(own(
        &d_alice(),
        &at(9),
        T0 + 9_000,
        "from the phone",
    )));
    h.apply(events).expect("apply");
    let a = conv_of(&h, &d_alice());
    assert_eq!(texts(&h, a).len(), 9);

    let commands = h.clear_conversation(a, false).expect("clear");
    assert_eq!(commands.len(), 1);
    let (conv, newest, lasting, remove) = clear_command(&commands[0]);
    assert_eq!(conv, d_alice());
    assert!(!remove);
    assert_eq!(
        newest,
        [at(9), at(7), at(6), at(5), at(4)],
        "the five newest, newest first, no notice"
    );
    assert_eq!(
        lasting,
        [at(9), at(5), at(4), at(3), at(2)],
        "the five newest without a timer"
    );
    assert!(texts(&h, a).is_empty());
    assert!(in_list(&h, a), "cleared, not deleted");
    let stored: Vec<String> = {
        let mut stmt = h
            .connection()
            .prepare("SELECT command FROM pending_commands")
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    assert_eq!(stored.len(), 1, "the command is in core.db with the delete");
    assert!(stored[0].contains("ClearConversation"));

    // A copy of a cleared message does not bring it back; a new message is taken.
    h.apply(vec![
        msg(&d_alice(), 3, "message 3"),
        msg(&d_alice(), 10, "new"),
    ])
    .expect("apply");
    assert_eq!(texts(&h, a), ["new"]);
    // Clearing an empty chat has nothing to tell anybody.
    h.clear_conversation(a, false).expect("clear");
    assert!(h.clear_conversation(a, false).expect("again").is_empty());
}

/// Clearing a chat here when it holds what is still in the outbox. A message that reached
/// some recipients and failed for the others (its row is only kept for a "Retry") goes
/// like any sent message and is named to the other devices, which have it. A message nobody
/// has stays, queued or failed, and is not named (no other device ever saw it). A tombstone
/// whose delete for everyone still waits is gone for the user, while its row carries the
/// delete on.
#[test]
fn clearing_a_chat_here_takes_what_was_sent_and_leaves_what_was_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut events = vec![chat(&d_alice()), msg(&d_alice(), 1, "from alice")];
    for (n, text) in [
        (2, "partly failed"),
        (3, "queued"),
        (4, "failed for all"),
        (5, "deleted"),
    ] {
        events.push(BackendEvent::Message(own(
            &d_alice(),
            &at(n),
            T0 + n * 1000,
            text,
        )));
    }
    h.apply(events).expect("apply");
    let a = conv_of(&h, &d_alice());
    let id = |h: &IngestHarness, n: i64| -> i64 {
        h.connection()
            .query_row(
                "SELECT id FROM messages WHERE net_local = ?1",
                [at(n)],
                |r| r.get(0),
            )
            .expect("message")
    };
    let (partly, queued, failed, deleted) = (id(&h, 2), id(&h, 3), id(&h, 4), id(&h, 5));
    let outbox = |h: &IngestHarness, message: i64, kind: &str, state: &str| {
        h.connection()
            .execute(
                "INSERT INTO outbox(message_id, conversation_id, kind, request, state,
                   deadline_at) VALUES (?1, ?2, ?3, '{}', ?4, 0)",
                (message, a.0, kind, state),
            )
            .expect("outbox row");
    };
    let recipient = |h: &IngestHarness, message: i64, state: &str| {
        h.connection()
            .execute(
                "INSERT OR REPLACE INTO send_state(message_id, user_id, state, at)
                 VALUES (?1, ?2, ?3, 0)",
                (message, ALICE, state),
            )
            .expect("send state");
    };
    // Sent to one recipient, failed for good for another (here: the same chat's peer has
    // it; the row stays `Failed` for the other).
    outbox(&h, partly, "Message", "Failed");
    recipient(&h, partly, r#"{"Sent":{"sealed":true}}"#);
    h.connection()
        .execute(
            "INSERT INTO send_state(message_id, user_id, state, at)
             VALUES (?1, ?2, '{\"Failed\":{\"retryable\":false}}', 0)",
            (partly, BOB),
        )
        .expect("send state");
    outbox(&h, queued, "Message", "Queued");
    recipient(&h, queued, r#""Pending""#);
    outbox(&h, failed, "Message", "Failed");
    recipient(&h, failed, r#"{"Failed":{"retryable":false}}"#);
    // Deleted for everyone here; the delete has not gone out yet.
    h.connection()
        .execute(
            "UPDATE messages SET deleted = 'ForEveryone' WHERE id = ?1",
            [deleted],
        )
        .expect("tombstone");
    outbox(&h, deleted, "Delete", "Queued");

    let commands = h.clear_conversation(a, false).expect("clear");
    let (_, newest, _, _) = clear_command(&commands[0]);
    assert_eq!(
        newest,
        [at(5), at(2), at(1)],
        "what the other devices have: the tombstone, the partly sent message, alice's"
    );
    assert_eq!(
        texts(&h, a),
        ["queued", "failed for all"],
        "what nobody has yet stays"
    );
    let rows: Vec<(i64, Option<String>)> = {
        let mut stmt = h
            .connection()
            .prepare("SELECT id, deleted FROM messages ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    assert_eq!(
        rows,
        [
            (queued, None),
            (failed, None),
            (deleted, Some("ForMe".to_owned()))
        ],
        "the tombstone's row carries its delete on, unseen"
    );
    let left: Vec<(i64, String)> = {
        let mut stmt = h
            .connection()
            .prepare("SELECT message_id, kind FROM outbox ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    assert_eq!(
        left,
        [
            (queued, "Message".to_owned()),
            (failed, "Message".to_owned()),
            (deleted, "Delete".to_owned())
        ]
    );

    // The same from the phone, as a delete of the chat: with the two unsent messages gone
    // (the user cancels them) nothing a person sent is left, and the chat leaves the list
    // although the hidden row is still there.
    h.connection()
        .execute(
            "DELETE FROM messages WHERE id IN (?1, ?2)",
            [queued, failed],
        )
        .expect("cancel");
    h.apply(vec![cleared(&d_alice(), &[], true)])
        .expect("apply");
    assert!(!in_list(&h, a), "nothing shows: the chat is hidden");
    assert_eq!(
        h.connection()
            .query_row(
                "SELECT count(*) FROM outbox WHERE kind = 'Delete'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("count"),
        1,
        "the delete for everyone is still to be sent"
    );
    // Somebody writes again: the chat is back and shows that message alone.
    h.apply(vec![msg(&d_alice(), 20, "hello again")])
        .expect("apply");
    assert!(in_list(&h, a));
    assert_eq!(texts(&h, a), ["hello again"]);
}

/// Deleting a chat here: it leaves the list and comes back with its next message; an empty
/// chat is deleted too, and the other devices hear of it either way.
#[test]
fn deleting_a_chat_here_hides_it_until_it_is_used_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        notice(&d_alice(), 2),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    h.connection()
        .execute(
            "UPDATE conversations SET draft = '{\"body\":\"half a thought\"}' WHERE id = ?1",
            [a.0],
        )
        .expect("draft");
    let hidden = |h: &IngestHarness| {
        h.conversation(a)
            .expect("query")
            .expect("the row is still there")
            .row
            .hidden
    };
    assert!(!hidden(&h));

    let commands = h.clear_conversation(a, true).expect("delete");
    let (_, newest, _, remove) = clear_command(&commands[0]);
    assert!(remove);
    assert_eq!(newest, [at(1)]);
    assert!(!in_list(&h, a));
    assert!(hidden(&h), "the row says so when it is asked for by id");
    let detail = h.conversation(a).expect("query").expect("row");
    assert_eq!(detail.row.title.as_deref(), Some("A chat"));
    assert!(detail.draft.is_none(), "the draft went with the chat");
    assert!(texts(&h, a).is_empty(), "the notice went too");

    // Deleting it again (it is empty now) still tells the other devices: they may hold it.
    let again = h.clear_conversation(a, true).expect("delete");
    let (_, newest, lasting, remove) = clear_command(&again[0]);
    assert!(remove && newest.is_empty() && lasting.is_empty());

    h.apply(vec![msg(&d_alice(), 5, "five")]).expect("apply");
    assert!(in_list(&h, a) && !hidden(&h));
    assert_eq!(texts(&h, a), ["five"]);
}

/// A logged-out account, and a network that does not sync deletes for me, delete locally and
/// queue nothing.
#[test]
fn a_local_delete_without_a_device_to_tell_stays_local() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let quiet = ConvKey::Direct(uid("aci:00000000-0000-4000-8000-000000000b0b"));
    let mut p = ConversationPatch::new(quiet.clone());
    let mut features = signal_features(false);
    features.delete_for_me = false;
    p.features = Some(features);
    h.apply(vec![
        chat(&d_alice()),
        msg(&d_alice(), 1, "one"),
        BackendEvent::ConversationUpsert(p),
        BackendEvent::Message(remote(
            &quiet,
            &uid(support::send::BOB),
            &at(2),
            T0 + 2_000,
            "two",
        )),
    ])
    .expect("apply");
    let (a, q) = (conv_of(&h, &d_alice()), conv_of(&h, &quiet));
    assert!(h.clear_conversation(q, false).expect("clear").is_empty());
    assert!(texts(&h, q).is_empty());
    h.connection()
        .execute("UPDATE accounts SET state = 'LoggedOut'", [])
        .expect("logged out");
    assert!(h.clear_conversation(a, true).expect("delete").is_empty());
    assert!(texts(&h, a).is_empty() && !in_list(&h, a));
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0);
}

fn attachment_ids(h: &IngestHarness) -> Vec<kourier_model::AttachmentId> {
    let mut stmt = h
        .connection()
        .prepare("SELECT a.id FROM attachments a ORDER BY a.message_id, a.idx")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0).map(kourier_model::AttachmentId))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// Deleting one attachment here: its row and files go, the rest of the message stays, the
/// backend gets the names the attachment had and where it was, and the refusals say why.
#[test]
fn deleting_an_attachment_here_tells_the_other_devices_its_names() {
    use kourier_core::facade::Refusal;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let file = |name: &str, seed| synthetic_file(name, "application/octet-stream", 100, seed);
    h.apply(vec![
        chat(&d_alice()),
        media(
            &d_alice(),
            1,
            vec![file("a.bin", 1), file("b.bin", 2), file("c.bin", 3)],
            Some("three files"),
        ),
        media(&d_alice(), 2, vec![synthetic_image(64, 64, 4)], None),
        msg(&d_alice(), 3, "a text"),
    ])
    .expect("apply");
    let a = conv_of(&h, &d_alice());
    let ids = attachment_ids(&h);
    assert_eq!(ids.len(), 4);
    let row = |h: &IngestHarness, n: usize| {
        h.timeline(&TimelineQuery {
            conv: a,
            anchor: TimelineAnchor::Newest,
            limit: 10,
        })
        .expect("timeline")
        .rows
        .remove(n)
    };
    assert!(row(&h, 0).actions.delete_attachment && row(&h, 1).actions.delete_attachment);
    assert!(!row(&h, 2).actions.delete_attachment, "a text has none");

    // The middle file of the first message.
    let commands = h.delete_attachment(ids[1]).expect("delete");
    match commands.as_slice() {
        [
            kourier_backend_api::DurableCommand::DeleteAttachment {
                conv,
                target,
                index,
                net_ids,
            },
        ] => {
            assert_eq!(*conv, d_alice());
            assert_eq!(target.local.as_ref(), at(1));
            assert_eq!(*index, 1);
            assert_eq!(
                net_ids.expose(),
                &[attachment_name(&at(1), 1), "digest:1:1".to_owned()],
                "every name the attachment had"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        attachment_rows(&h),
        [
            (0, Some("a.bin".to_owned())),
            (1, Some("c.bin".to_owned())),
            (0, None)
        ],
        "the files behind it moved up"
    );
    assert_eq!(texts(&h, a)[0], "2 file(s): three files");
    // It is gone: a second delete of the same attachment finds nothing.
    let e = h.delete_attachment(ids[1]).expect_err("gone");
    assert_eq!(e.refusal(), Some(Refusal::Gone));

    // The only picture of a message without a caption: the message is hidden like a delete
    // for me, and the command still names the picture.
    assert_eq!(h.delete_attachment(ids[3]).expect("delete").len(), 1);
    assert_eq!(texts(&h, a), ["2 file(s): three files", "a text"]);
    // Nothing is left of a tombstone to delete from.
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&d_alice()), Some(&alice()), &at(1)),
        by: None,
        for_everyone: false,
        at: T0 + 50_000,
    }])
    .expect("apply");
    let e = h.delete_attachment(ids[0]).expect_err("deleted");
    assert_eq!(
        e.refusal(),
        Some(Refusal::Gone),
        "its rows went with the tombstone"
    );
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_commands", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 2);
}
