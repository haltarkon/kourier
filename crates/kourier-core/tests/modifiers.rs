//! S13 on the core side (architecture §3.5.1-3.5.3, §3.8, §4.8, §11 S13): own edits, deletes
//! and reactions through the outbox with feature windows, optimistic state and rollback;
//! incoming modifiers before their target and outside the receive windows; the disappearing
//! sweeper on a fake clock (also across a restart); message ids that are never reused; download
//! answers that name their request; downloads resumed only after the catch-up boundary; the
//! conversation timer.

mod support;

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, OutgoingKind};
use kourier_core::AccountVault;
use kourier_core::facade::{MessageRow, UiCommand, UiEvent};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::{T0, own, remote, signal_features};
use kourier_fake::{Flavor, Step, TransmitRule};
use kourier_model::{
    AccountId, AccountUuid, AttachmentState, BackendError, Body, ConnState, ConvKey,
    ConversationId, ConversationPatch, Deletion, DisappearKind, DownloadId, ErrorKind, Expiry,
    MessageId, MsgIdScope, NetworkKind, Patch, TargetAuthor, Text,
};
use support::send::*;
use support::{CORE_KEY, Env, WAIT, add_linked, eventually};

const HOUR: i64 = 3600 * 1000;

async fn linked(script: kourier_fake::Script) -> (Env, AccountId, AccountUuid) {
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

/// The message with network id `local` (waits for it).
async fn message(env: &Env, local: &str) -> (ConversationId, MessageId) {
    let l = local.to_owned();
    until(env, "the message", move |c| {
        c.query_row("SELECT 1 FROM messages WHERE net_local = ?1", [&l], |_| {
            Ok(())
        })
        .is_ok()
    })
    .await;
    core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT conversation_id, id FROM messages WHERE net_local = ?1",
            [local],
            |r| Ok((ConversationId(r.get(0)?), MessageId(r.get(1)?))),
        )
        .expect("message")
    })
}

async fn row(env: &Env, conv: ConversationId, id: MessageId) -> Option<MessageRow> {
    env.timeline(conv).await.into_iter().find(|r| r.id == id)
}

fn text(body: &Body) -> &str {
    match body {
        Body::Text(t) => &t.body,
        _ => "",
    }
}

/// The error of the next `ActionFailed` of `action`.
async fn failed(env: &mut Env, action: &'static str) -> String {
    env.event(action, |e| match e {
        UiEvent::ActionFailed {
            action: a, error, ..
        } if *a == action => Some(error.to_string()),
        _ => None,
    })
    .await
}

fn edit(env: &Env, message: MessageId, body: &str) {
    env.handle
        .send(UiCommand::EditMessage {
            message,
            text: Text::plain(body),
        })
        .expect("send");
}

fn react(env: &Env, message: MessageId, emoji: &str, remove: bool) {
    env.handle
        .send(UiCommand::React {
            message,
            emoji: emoji.into(),
            remove,
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

fn deleted(env: &Env, id: MessageId) -> Option<String> {
    core_db(&env.core_db(), |c| {
        c.query_row("SELECT deleted FROM messages WHERE id = ?1", [id.0], |r| {
            r.get(0)
        })
        .expect("row")
    })
}

/// Own reactions as stored: (emoji, removed).
fn own_reactions(env: &Env, id: MessageId) -> Vec<(String, bool)> {
    core_db(&env.core_db(), |c| {
        let mut s = c
            .prepare(
                "SELECT emoji, removed FROM reactions WHERE message_id = ?1 AND user_id = ''
                 ORDER BY emoji",
            )
            .expect("prepare");
        s.query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    })
}

fn refused() -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, "refused by the server")
}

/// An own message sent `age_ms` before T0 from another device.
fn own_at(conv: &ConvKey, age_ms: i64) -> BackendEvent {
    let at = T0 - age_ms;
    BackendEvent::Message(own(conv, &at.to_string(), at, "first version"))
}

// ---------- own edits, deletes, reactions ----------

/// An own edit is a revision at once, then a network message of its own: a fresh id, the
/// previous revision as the target (Signal's rule), the text in the body. The next edit
/// targets the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_edits_are_revisions_sent_with_their_own_ids() {
    let s = script("edit").deliver(vec![own_at(&d_alice(), 90_000)]);
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let original = (T0 - 90_000).to_string();
    let (conv, id) = message(&env, &original).await;

    edit(&env, id, "second version");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the edit is sent");
    let first = env.control.transmits(uuid)[0].clone();
    assert_eq!(first.message, id, "the outbox row belongs to its target");
    let OutgoingKind::Edit { target, body } = &first.req.kind else {
        panic!("an edit: {:?}", first.req.kind);
    };
    assert_eq!(target.author, TargetAuthor::Own);
    assert_eq!(target.local.as_ref(), original);
    assert_eq!(text(body), "second version");
    assert_ne!(first.req.local.as_ref(), original, "an edit has its own id");
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    let r = row(&env, conv, id).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (1, "second version"));
    assert_eq!(r.edited_at, Some(first.req.sent_at));

    edit(&env, id, "third version");
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 2))
        .await
        .expect("the second edit is sent");
    let second = env.control.transmits(uuid)[1].clone();
    let OutgoingKind::Edit { target, .. } = &second.req.kind else {
        panic!("an edit");
    };
    assert_eq!(
        target.local, first.req.local,
        "targets the previous revision"
    );
    assert!(second.req.sent_at > first.req.sent_at);
    let r = row(&env, conv, id).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (2, "third version"));
    // Every revision key stays indexed (§3.5.3).
    let n: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT count(*) FROM message_revisions WHERE message_id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(n, 3, "original + two edits");
}

/// The send windows of §3.8 (Signal values from the fake's features): edits ≤ 24 h and ≤ 10
/// times, deletes for everyone ≤ 1 day, only own messages; a delete for me always works; the
/// per-user reaction limit replaces (1) or refuses (> 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_windows_and_limits_are_enforced() {
    let mut limited = ConversationPatch::new(d_bob());
    let mut f = signal_features(false);
    f.reactions_per_user = Some(2);
    limited.features = Some(f);
    let bob_msg = remote(
        &d_bob(),
        &bob(),
        &(T0 - 1_000).to_string(),
        T0 - 1_000,
        "hi",
    );
    let s = script("windows").deliver(vec![
        own_at(&d_alice(), 25 * HOUR),
        own_at(&d_alice(), 90_000),
        BackendEvent::ConversationUpsert(limited),
        BackendEvent::Message(bob_msg),
    ]);
    let (mut env, account, _) = linked(s).await;
    env.wait_finished(account).await;
    let (_, old) = message(&env, &(T0 - 25 * HOUR).to_string()).await;
    let (_, fresh) = message(&env, &(T0 - 90_000).to_string()).await;
    let (bob_conv, theirs) = message(&env, &(T0 - 1_000).to_string()).await;

    edit(&env, old, "too late");
    assert!(failed(&mut env, "EditMessage").await.contains("too old"));
    delete(&env, old, true);
    assert!(failed(&mut env, "DeleteMessage").await.contains("too old"));
    delete(&env, old, false);
    until(&env, "deleted for me", move |c| {
        c.query_row("SELECT deleted FROM messages WHERE id = ?1", [old.0], |r| {
            r.get::<_, Option<String>>(0)
        })
        .is_ok_and(|d| d.as_deref() == Some("ForMe"))
    })
    .await;

    for i in 0..11 {
        edit(&env, fresh, &format!("version {i}"));
    }
    assert!(failed(&mut env, "EditMessage").await.contains("too often"));
    let revision: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT revision FROM messages WHERE id = ?1",
            [fresh.0],
            |r| r.get(0),
        )
        .expect("revision")
    });
    assert_eq!(revision, 10);

    edit(&env, theirs, "not mine");
    assert!(failed(&mut env, "EditMessage").await.contains("own"));
    delete(&env, theirs, true);
    assert!(failed(&mut env, "DeleteMessage").await.contains("own"));

    react(&env, theirs, "👍", false);
    react(&env, theirs, "❤️", false);
    react(&env, theirs, "😂", false);
    assert!(
        failed(&mut env, "React")
            .await
            .contains("as many reactions")
    );
    react(&env, theirs, "👍", true);
    react(&env, theirs, "😂", false);
    let db = env.core_db();
    eventually("👍 replaced by 😂", WAIT, || {
        let db = db.clone();
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT group_concat(emoji || removed, ',') FROM (SELECT emoji, removed
                       FROM reactions WHERE message_id = ?1 AND user_id = '' ORDER BY emoji)",
                    [theirs.0],
                    |r| r.get::<_, String>(0),
                )
                .is_ok_and(|s| {
                    s.split(',').filter(|e| e.ends_with('0')).count() == 2 && s.contains("👍1")
                })
            })
        }
    })
    .await;
    let shown = row(&env, bob_conv, theirs).await.expect("row").reactions;
    let mut emoji: Vec<&str> = shown.iter().map(|g| g.emoji.as_str()).collect();
    emoji.sort_unstable();
    assert_eq!(emoji, ["❤️", "😂"]);
    assert!(shown.iter().all(|g| g.by_me()));
}

/// A change the network refuses for good is rolled back and reported: the edit's text, the
/// reaction, and the "for everyone" of a delete (the content stays gone here).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_changes_roll_back() {
    let s = script("rollback")
        .deliver(vec![own_at(&d_alice(), 90_000)])
        .fail_transmit(TransmitRule::attempts(refused(), 1).of_kind("Edit"))
        .fail_transmit(TransmitRule::attempts(refused(), 1).of_kind("React"))
        .fail_transmit(TransmitRule::attempts(refused(), 2).of_kind("Delete"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (conv, mine) = message(&env, &(T0 - 90_000).to_string()).await;
    // Alice's "are you there?" from the setup delivery.
    let (_, alice_msg) = message(&env, &(T0 - 60_000).to_string()).await;

    edit(&env, mine, "changed my mind");
    assert!(failed(&mut env, "EditMessage").await.contains("refused"));
    let r = row(&env, conv, mine).await.expect("row");
    assert_eq!((r.revision, text(&r.body)), (0, "first version"));
    assert_eq!(r.edited_at, None);
    let revisions: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT count(*) FROM message_revisions WHERE message_id = ?1",
            [mine.0],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(revisions, 0, "unedited again");

    react(&env, alice_msg, "🎉", false);
    assert!(failed(&mut env, "React").await.contains("refused"));
    assert!(own_reactions(&env, alice_msg).is_empty());
    assert!(
        row(&env, conv, alice_msg)
            .await
            .expect("row")
            .reactions
            .is_empty()
    );

    delete(&env, mine, true);
    assert!(failed(&mut env, "DeleteMessage").await.contains("refused"));
    assert_eq!(deleted(&env, mine).as_deref(), Some("ForMe"));
    assert!(
        row(&env, conv, mine).await.is_none(),
        "hidden like a delete for me"
    );
    let kinds: Vec<&'static str> = env
        .control
        .transmits(uuid)
        .iter()
        .map(|t| t.req.kind.name())
        .collect();
    assert_eq!(kinds, ["Edit", "React", "Delete"]);
    assert_eq!(outbox_len(&env.core_db()), 0);

    // A refused delete-for-me sync is best effort: the local delete stands, and the user is
    // not told about it (the next ActionFailed is the edit refused below).
    delete(&env, alice_msg, false);
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 4))
        .await
        .expect("the sync is attempted");
    until(&env, "the sync row is gone", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(deleted(&env, alice_msg).as_deref(), Some("ForMe"));
    edit(&env, alice_msg, "not mine");
    let next = env
        .event("the next ActionFailed", |e| match e {
            UiEvent::ActionFailed { action, .. } => Some(*action),
            _ => None,
        })
        .await;
    assert_eq!(next, "EditMessage");
}

/// A change some group members got on a partial attempt stands when the resend to the others
/// is refused for good: it is not rolled back (alice keeps the reaction, so do we) and not
/// reported as failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_that_reached_some_members_is_not_rolled_back() {
    let s = script("partial-change")
        .deliver(vec![own_at(&group(), 90_000)])
        .fail_transmit(TransmitRule::member(bob(), transient(), 1).of_kind("React"))
        .fail_transmit(TransmitRule::member(bob(), refused(), 1).of_kind("React"));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (conv, mine) = message(&env, &(T0 - 90_000).to_string()).await;

    react(&env, mine, "🎉", false);
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the reaction is sent");
    // The resend to bob waits for its backoff on the core clock.
    env.clock.advance(Duration::from_secs(1));
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 2))
        .await
        .expect("the reaction is resent to bob");
    let t = env.control.transmits(uuid);
    assert_eq!(t[1].req.only, Some(vec![bob()]), "resent to bob only");
    assert!(
        matches!(&t[1].outcome, kourier_backend_api::TransmitOutcome::Partial { delivered, failed }
            if delivered.is_empty() && failed.len() == 1),
        "{:?}",
        t[1].outcome
    );
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(own_reactions(&env, mine), [("🎉".to_owned(), false)]);
    let shown = row(&env, conv, mine).await.expect("row").reactions;
    assert!(shown.iter().any(|g| g.emoji == "🎉" && g.by_me()));
    // Not reported: the next ActionFailed is the one provoked here.
    edit(&env, mine, "");
    let next = env
        .event("the next ActionFailed", |e| match e {
            UiEvent::ActionFailed { action, .. } => Some(*action),
            _ => None,
        })
        .await;
    assert_eq!(next, "EditMessage");
}

/// §4.8 step 5 while offline: an edit of a message nobody has yet replaces it in place, a
/// delete cancels it; a reaction waits in the outbox and a delete for me of its target drops
/// it. Online again, only the delete-for-me sync goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_messages_are_edited_in_place_and_deleting_cancels_them() {
    let s = kourier_fake::Script::new("offline", vec![uid(ME), uid(ME_PNI)])
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .deliver(setup());
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let conv = conv_id(&env.core_db(), &d_bob()).expect("conversation");
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("draft one"),
            reply_to: None,
        })
        .expect("send");
    until(&env, "queued", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE conversation_id = ?1",
            [conv.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    let queued = own_messages(&env.core_db(), conv)[0].clone();
    edit(&env, queued.id, "draft two");
    let db = env.core_db();
    eventually("edited in place", WAIT, || {
        let db = db.clone();
        async move {
            core_db(&db, |c| {
                c.query_row(
                    "SELECT body, revision, (SELECT request FROM outbox WHERE message_id = m.id)
                     FROM messages m WHERE id = ?1",
                    [queued.id.0],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .is_ok_and(|(body, rev, req)| {
                    body.contains("draft two") && rev == 0 && req.contains("draft two")
                })
            })
        }
    })
    .await;
    assert_eq!(outbox_len(&env.core_db()), 1, "no separate edit row");
    delete(&env, queued.id, true);
    until(&env, "cancelled", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE id = ?1",
            [queued.id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(outbox_len(&env.core_db()), 0);

    let (alice_conv, alices) = message(&env, &(T0 - 60_000).to_string()).await;
    react(&env, alices, "👍", false);
    until(&env, "the reaction waits", |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind = 'React'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    delete(&env, alices, false);
    until(&env, "only the delete-for-me sync is left", |c| {
        c.query_row("SELECT group_concat(kind) FROM outbox", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .is_ok_and(|k| k.as_deref() == Some("Delete"))
    })
    .await;
    assert!(row(&env, alice_conv, alices).await.is_none());
    assert!(
        env.control.transmits(uuid).is_empty(),
        "nothing was sent offline"
    );
}

/// A queued message that waits for a retry (its earlier attempts failed temporarily, or got no
/// answer) may be on the server already: a delete for everyone does not just cancel it, it
/// leaves a tombstone and sends the delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_for_everyone_after_an_unanswered_attempt_is_sent() {
    let s = script("unanswered").fail_transmit(
        TransmitRule::attempts(transient(), 2)
            .in_conv(d_alice())
            .of_kind("Message"),
    );
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let conv = conv_id(&env.core_db(), &d_alice()).expect("conversation");
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("maybe sent"),
            reply_to: None,
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 2))
        .await
        .expect("two attempts");
    let id = own_messages(&env.core_db(), conv)[0].id;
    until(&env, "waiting for the next attempt", move |c| {
        c.query_row(
            "SELECT state, attempts FROM outbox WHERE message_id = ?1",
            [id.0],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .is_ok_and(|(s, n)| s == "Queued" && n == 2)
    })
    .await;

    delete(&env, id, true);
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 3))
        .await
        .expect("the delete is sent");
    let t = env.control.transmits(uuid);
    assert!(
        matches!(&t[2].req.kind, OutgoingKind::Delete { for_everyone: true, target }
            if target.local.as_ref() == t[0].req.local.as_ref()),
        "{:?}",
        t[2].req.kind
    );
    assert_eq!(t.len(), 3, "the message itself is not sent again");
    assert_eq!(deleted(&env, id).as_deref(), Some("ForEveryone"));
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
}

/// A group message deleted for everyone from our phone while its resend to a member is still
/// waiting is not sent to that member any more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_deleted_elsewhere_is_not_resent() {
    // The core clock starts at T0 and does not move before the send: its id is T0.
    let local = T0.to_string();
    let gone = BackendEvent::Deleted {
        target: kourier_fake::scripts::target(Some(&group()), None, &local),
        by: None,
        for_everyone: true,
        at: T0 + 100,
    };
    let s = script("deleted-elsewhere")
        .fail_transmit(TransmitRule::member(bob(), transient(), 2))
        .step(Step::Sleep(Duration::from_millis(100)))
        .deliver(vec![gone]);
    let (env, _account, uuid) = linked(s).await;
    // The script sleeps before its last delivery, so it does not finish yet.
    let key = group().encode();
    until(&env, "the group with its members", move |c| {
        c.query_row(
            "SELECT count(*) FROM members m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.conv_key = ?1 AND c.features IS NOT NULL",
            [&key],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 3)
    })
    .await;
    let conv = conv_id(&env.core_db(), &group()).expect("group");
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("oops, wrong group"),
            reply_to: None,
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| c.transmits(uuid).len() >= 2))
        .await
        .expect("sent, and resent to bob once");
    let id = own_messages(&env.core_db(), conv)[0].id;
    assert_eq!(env.control.transmits(uuid)[0].req.local.as_ref(), local);

    // The phone's delete arrives before the next resend is due (190 ms backoff).
    env.clock.advance(Duration::from_millis(100));
    until(&env, "deleted by the phone", move |c| {
        c.query_row("SELECT deleted FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, Option<String>>(0)
        })
        .is_ok_and(|d| d.as_deref() == Some("ForEveryone"))
    })
    .await;
    env.clock.advance(Duration::from_secs(1));
    until(&env, "the resend is dropped", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(env.control.transmits(uuid).len(), 2, "bob never gets it");
}

/// An own delete for everyone is a tombstone at once: body gone, attachments deleted with
/// their files, and the delete goes out targeting the original id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_deletes_are_tombstones_without_files() {
    let at = T0 - 90_000;
    let mut photo = own(&d_alice(), &at.to_string(), at, "");
    photo.body = Body::Media {
        attachments: vec![kourier_fake::attachments::synthetic_image(64, 48, 5)],
        caption: Some(Text::plain("look")),
    };
    let s = script("tombstone").deliver(vec![BackendEvent::Message(photo)]);
    let (env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (conv, id) = message(&env, &at.to_string()).await;
    until(&env, "downloaded and derived", move |c| {
        c.query_row(
            "SELECT count(*) FROM attachments WHERE message_id = ?1 AND derived_at IS NOT NULL",
            [id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    let files: Vec<String> = core_db(&env.core_db(), |c| {
        let mut s = c
            .prepare(
                "SELECT f.path FROM files f JOIN attachments a
                   ON f.id = a.file_id OR f.id = a.thumb_file_id WHERE a.message_id = ?1",
            )
            .expect("prepare");
        s.query_map([id.0], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("paths")
    });
    let blobs = env.vault.blob_dir(uuid);
    assert!(!files.is_empty() && files.iter().all(|p| blobs.join(p).exists()));

    delete(&env, id, true);
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("the delete is sent");
    let sent = env.control.transmits(uuid)[0].clone();
    let OutgoingKind::Delete {
        target,
        for_everyone: true,
    } = &sent.req.kind
    else {
        panic!("a delete for everyone: {:?}", sent.req.kind);
    };
    assert_eq!(
        (target.author.clone(), target.local.to_string()),
        (TargetAuthor::Own, at.to_string())
    );
    let r = row(&env, conv, id).await.expect("the tombstone is shown");
    assert!(matches!(
        r.deleted,
        Some(Deletion::ForEveryone { by: None })
    ));
    assert_eq!(text(&r.body), "");
    let left: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT (SELECT count(*) FROM attachments WHERE message_id = ?1)
                  + (SELECT count(*) FROM files)",
            [id.0],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(left, 0);
    let (blobs2, files2) = (blobs.clone(), files.clone());
    eventually("the files are unlinked", WAIT, || {
        let (blobs, files) = (blobs2.clone(), files2.clone());
        async move { files.iter().all(|p| !blobs.join(p).exists()) }
    })
    .await;
    until(&env, "the outbox is empty", |c| {
        c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
            .is_ok_and(|n| n == 0)
    })
    .await;
    assert_eq!(deleted(&env, id).as_deref(), Some("ForEveryone"));
}

// ---------- disappearing messages ----------

/// Timers start per kind (`AfterRead` at reading, `AfterSend` at the send) and the sweeper
/// deletes messages with their files on time, on the core's fake clock; a message that expires
/// while the core is stopped goes at the next start. An unread `AfterRead` message stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disappearing_messages_go_on_time_with_their_files_and_across_restarts() {
    let minute = Duration::from_secs(60);
    let after_read = Some(Expiry {
        kind: DisappearKind::AfterRead,
        timer: minute,
        started_at: None,
    });
    let mut photo = remote(&d_bob(), &bob(), &(T0 - 5_000).to_string(), T0 - 5_000, "");
    photo.body = Body::Media {
        attachments: vec![kourier_fake::attachments::synthetic_image(64, 48, 7)],
        caption: None,
    };
    photo.expiry = after_read;
    let mut unread = remote(
        &d_alice(),
        &alice(),
        &(T0 - 4_000).to_string(),
        T0 - 4_000,
        "x",
    );
    unread.expiry = after_read;
    let mut timer = ConversationPatch::new(d_alice());
    timer.timer = Patch::Set(Duration::from_secs(120));
    let s = script("expiry").deliver(vec![
        BackendEvent::Message(photo),
        BackendEvent::Message(unread),
        BackendEvent::ConversationUpsert(timer),
    ]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (bob_conv, photo_id) = message(&env, &(T0 - 5_000).to_string()).await;
    let (alice_conv, unread_id) = message(&env, &(T0 - 4_000).to_string()).await;
    until(&env, "the photo is downloaded and derived", move |c| {
        c.query_row(
            "SELECT count(*) FROM attachments WHERE message_id = ?1 AND file_id IS NOT NULL
               AND derived_at IS NOT NULL",
            [photo_id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    let files: Vec<String> = core_db(&env.core_db(), |c| {
        let mut s = c
            .prepare(
                "SELECT f.path FROM files f JOIN attachments a
                   ON f.id = a.file_id OR f.id = a.thumb_file_id WHERE a.message_id = ?1",
            )
            .expect("prepare");
        s.query_map([photo_id.0], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("paths")
    });
    let blobs = env.vault.blob_dir(uuid);
    assert!(!files.is_empty() && files.iter().all(|p| blobs.join(p).exists()));
    let expires = |id: MessageId| -> Option<i64> {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT expires_at FROM messages WHERE id = ?1",
                [id.0],
                |r| r.get(0),
            )
            .expect("row")
        })
    };
    assert_eq!(expires(photo_id), None, "AfterRead waits for reading");

    env.handle
        .send(UiCommand::MarkRead { conv: bob_conv })
        .expect("send");
    let read_at = env.clock.now_ms() as i64;
    until(&env, "the timer started", move |c| {
        c.query_row(
            "SELECT expires_at FROM messages WHERE id = ?1",
            [photo_id.0],
            |r| r.get::<_, Option<i64>>(0),
        )
        .is_ok_and(|e| e == Some(read_at + 60_000))
    })
    .await;
    // Our own message in a chat with a two-minute timer: AfterSend, from the send.
    env.handle
        .send(UiCommand::SendText {
            conv: alice_conv,
            text: Text::plain("gone in two minutes"),
            reply_to: None,
        })
        .expect("send");
    let sent_at = env.clock.now_ms() as i64;
    until(&env, "sent with a timer", move |c| {
        c.query_row(
            "SELECT expires_at FROM messages WHERE conversation_id = ?1 AND net_author = ''",
            [alice_conv.0],
            |r| r.get::<_, Option<i64>>(0),
        )
        .is_ok_and(|e| e == Some(sent_at + 120_000))
    })
    .await;
    let own_id = own_messages(&env.core_db(), alice_conv)[0].id;

    env.clock.advance(Duration::from_secs(59));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(expires(photo_id).is_some(), "not yet");
    env.clock.advance(Duration::from_secs(1));
    env.event("the photo's timeline reloads", |e| match e {
        UiEvent::TimelineInvalidated { conv } if *conv == bob_conv => Some(()),
        _ => None,
    })
    .await;
    until(&env, "the photo is gone", move |c| {
        c.query_row(
            "SELECT (SELECT count(*) FROM messages WHERE id = ?1)
                  + (SELECT count(*) FROM attachments WHERE message_id = ?1)",
            [photo_id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    let blobs2 = blobs.clone();
    let files2 = files.clone();
    eventually("its files are unlinked", WAIT, || {
        let (blobs, files) = (blobs2.clone(), files2.clone());
        async move { files.iter().all(|p| !blobs.join(p).exists()) }
    })
    .await;
    assert!(row(&env, alice_conv, own_id).await.is_some(), "not yet");

    // The own message expires while the core is stopped. It goes at the next start, once
    // the backend is connected (which is its word on the clock, §9.6).
    env.stop().await;
    env.clock.advance(Duration::from_secs(120));
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    until(&env, "swept at the next start", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE id = ?1",
            [own_id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    env.clock.advance(Duration::from_secs(24 * 3600));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        row(&env, alice_conv, unread_id).await.is_some(),
        "an unread AfterRead message never starts its timer"
    );
}

/// An own message's timer starts when the first recipient has it (Signal Desktop), not at
/// the enqueue: queued while offline for longer than its timer, it is still sent, and only
/// then disappears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_message_starts_its_timer_when_it_is_sent() {
    let mut timer = ConversationPatch::new(d_alice());
    timer.timer = Patch::Set(Duration::from_secs(60));
    let mut deliveries = setup();
    deliveries.push(BackendEvent::ConversationUpsert(timer));
    let s = kourier_fake::Script::new("queued-timer", vec![uid(ME), uid(ME_PNI)])
        .step(Step::Conn(ConnState::Offline { retry_at_ms: None }))
        .deliver(deliveries)
        .step(Step::Sleep(Duration::from_secs(300)))
        .step(Step::Conn(ConnState::Online { detail: None }));
    let (env, _account, uuid) = linked(s).await;
    until(&env, "the timer is known", |c| {
        c.query_row(
            "SELECT timer_secs FROM conversations WHERE timer_secs IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|t| t == 60)
    })
    .await;
    let conv = conv_id(&env.core_db(), &d_alice()).expect("conversation");
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("see you"),
            reply_to: None,
        })
        .expect("send");
    until(&env, "queued", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE conversation_id = ?1",
            [conv.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    let own_id = own_messages(&env.core_db(), conv)[0].id;
    let timer_of = |id: MessageId| -> (Option<i64>, Option<i64>) {
        core_db(&env.core_db(), |c| {
            c.query_row(
                "SELECT expiry_timer_ms, expires_at FROM messages WHERE id = ?1",
                [id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row")
        })
    };
    assert_eq!(
        timer_of(own_id),
        (Some(60_000), None),
        "not started while queued"
    );

    env.clock.advance(Duration::from_secs(120));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        timer_of(own_id),
        (Some(60_000), None),
        "still queued, still here"
    );
    assert!(env.control.transmits(uuid).is_empty(), "offline");

    env.clock.advance(Duration::from_secs(180));
    let sent_at = env.clock.now_ms() as i64;
    let control = env.control.clone();
    tokio::time::timeout(WAIT, control.wait_for(|c| !c.transmits(uuid).is_empty()))
        .await
        .expect("sent once online");
    until(&env, "the timer started at the send", move |c| {
        c.query_row(
            "SELECT expires_at FROM messages WHERE id = ?1",
            [own_id.0],
            |r| r.get::<_, Option<i64>>(0),
        )
        .is_ok_and(|e| e == Some(sent_at + 60_000))
    })
    .await;
    env.clock.advance(Duration::from_secs(60));
    until(&env, "gone a minute after the send", move |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE id = ?1",
            [own_id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
}

// ---------- tombstones ----------

/// The tombstone a delete for everyone left can be deleted for me (it is hidden then), but
/// not edited, reacted to or deleted again for everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tombstone_can_be_deleted_for_me_only() {
    let at = T0 - 5_000;
    let theirs = remote(&d_alice(), &alice(), &at.to_string(), at, "regret");
    let gone = BackendEvent::Deleted {
        target: kourier_fake::scripts::target(Some(&d_alice()), Some(&alice()), &at.to_string()),
        by: Some(alice()),
        for_everyone: true,
        at: at + 1_000,
    };
    let s = script("tombstone-for-me").deliver(vec![BackendEvent::Message(theirs), gone]);
    let (mut env, account, _) = linked(s).await;
    env.wait_finished(account).await;
    let (conv, id) = message(&env, &at.to_string()).await;
    until(&env, "deleted by its author", move |c| {
        c.query_row("SELECT deleted FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, Option<String>>(0)
        })
        .is_ok_and(|d| d.as_deref() == Some("ForEveryone"))
    })
    .await;
    assert!(
        row(&env, conv, id).await.is_some(),
        "the tombstone is shown"
    );

    react(&env, id, "👍", false);
    assert!(failed(&mut env, "React").await.contains("deleted"));
    delete(&env, id, true);
    assert!(failed(&mut env, "DeleteMessage").await.contains("deleted"));

    delete(&env, id, false);
    until(&env, "deleted for me", move |c| {
        c.query_row("SELECT deleted FROM messages WHERE id = ?1", [id.0], |r| {
            r.get::<_, Option<String>>(0)
        })
        .is_ok_and(|d| d.as_deref() == Some("ForMe"))
    })
    .await;
    assert!(row(&env, conv, id).await.is_none(), "hidden");
}

// ---------- downloads ----------

/// A download an earlier actor left unanswered is sent again only once the backend is past
/// its catch-up (`Online`), with its stored request id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downloads_resume_only_after_the_catch_up_boundary() {
    let mut photo = remote(&d_bob(), &bob(), &(T0 - 5_000).to_string(), T0 - 5_000, "");
    photo.body = Body::Media {
        attachments: vec![kourier_fake::attachments::synthetic_image(64, 48, 9)],
        caption: None,
    };
    let mut s = script("resume").deliver(vec![BackendEvent::Message(photo)]);
    s.catch_up_on_resume = Some(Duration::from_secs(60));
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (_, photo_id) = message(&env, &(T0 - 5_000).to_string()).await;
    until(&env, "downloaded", move |c| {
        c.query_row(
            "SELECT count(*) FROM attachments WHERE message_id = ?1 AND derived_at IS NOT NULL",
            [photo_id.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 1)
    })
    .await;
    env.stop().await;
    // As if the actor had died before answering: the request is still in flight.
    let request: i64 = {
        let conn = kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY))
            .expect("open core.db");
        conn.execute_batch(&format!(
            "DELETE FROM files WHERE id IN (SELECT file_id FROM attachments WHERE message_id = {m}
               UNION SELECT thumb_file_id FROM attachments WHERE message_id = {m});
             UPDATE attachments SET state = '\"Remote\"', file_id = NULL, thumb_file_id = NULL,
               derived_at = NULL WHERE message_id = {m};
             INSERT INTO attachment_downloads(attachment_id, account_id, path, key, requested_at)
               SELECT id, {a}, 'core/ab/ab0123456789abcdef0123456789abcd', randomblob(32), 0
               FROM attachments WHERE message_id = {m};",
            m = photo_id.0,
            a = account.0
        ))
        .expect("simulate");
        conn.query_row("SELECT id FROM attachment_downloads", [], |r| r.get(0))
            .expect("request")
    };
    let downloads = |env: &Env| {
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "DownloadAttachment")
            .count()
    };
    let before = downloads(&env);
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let clock = env.clock.clone();
    eventually("the fake catches up", WAIT, || {
        let clock = clock.clone();
        async move { clock.sleepers() > 0 }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(downloads(&env), before, "nothing resumed while syncing");
    env.clock.advance(Duration::from_secs(60));
    env.event("online", |e| match e {
        UiEvent::ConnState {
            state: ConnState::Online { .. },
            ..
        } => Some(()),
        _ => None,
    })
    .await;
    until(&env, "the resent download landed", move |c| {
        c.query_row(
            "SELECT state FROM attachments WHERE message_id = ?1",
            [photo_id.0],
            |r| r.get::<_, String>(0),
        )
        .is_ok_and(|s| s == "\"Local\"")
    })
    .await;
    assert_eq!(downloads(&env), before + 1);
    let left: i64 = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT count(*) FROM attachment_downloads WHERE id = ?1",
            [request],
            |r| r.get(0),
        )
        .expect("count")
    });
    assert_eq!(left, 0, "answered under its request id");
}

// ---------- refusals ----------

/// Every refusal of a message action says why in a form the UI can map (`Refusal`), and
/// keeps its sentence for the log: too old, edited too often, deleted, being sent, unchanged,
/// not allowed here, not ours, the wrong kind of message, empty, gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_name_a_reason_the_ui_can_map() {
    use kourier_core::facade::{CoreError, Refusal};
    // Bob's chat: no reactions, one edit per message, and its sends are answered late.
    let mut strict = ConversationPatch::new(d_bob());
    let mut f = signal_features(false);
    f.reactions = kourier_model::Support::Unsupported;
    f.edit = Some(kourier_model::Window {
        max_age_secs: Some(24 * 3600),
        max_count: Some(1),
    });
    strict.features = Some(f);
    let s = script("reasons")
        .delay_reply(d_bob(), Duration::from_secs(60))
        .deliver(vec![
            own_at(&d_alice(), 25 * HOUR),
            own_at(&d_alice(), 90_000),
            BackendEvent::ConversationUpsert(strict),
            BackendEvent::Message(remote(
                &d_bob(),
                &bob(),
                &(T0 - 1_000).to_string(),
                T0 - 1_000,
                "hi",
            )),
        ]);
    let (mut env, account, uuid) = linked(s).await;
    env.wait_finished(account).await;
    let (_, old) = message(&env, &(T0 - 25 * HOUR).to_string()).await;
    let (_, fresh) = message(&env, &(T0 - 90_000).to_string()).await;
    let (bob_conv, theirs) = message(&env, &(T0 - 1_000).to_string()).await;
    async fn why(env: &mut Env, action: &'static str) -> (Option<Refusal>, String) {
        env.event(action, |e| match e {
            UiEvent::ActionFailed {
                action: a, error, ..
            } if *a == action => {
                assert!(
                    matches!(**error, CoreError::Refused { .. }),
                    "a refusal with a reason, not {error:?}"
                );
                Some((error.refusal(), error.to_string()))
            }
            _ => None,
        })
        .await
    }

    // Too old: the edit window and the window of a delete for everyone.
    edit(&env, old, "too late");
    let (reason, text) = why(&mut env, "EditMessage").await;
    assert_eq!(reason, Some(Refusal::TooOld));
    assert_eq!(text, "invalid request: the message is too old to edit");
    delete(&env, old, true);
    assert_eq!(
        why(&mut env, "DeleteMessage").await.0,
        Some(Refusal::TooOld)
    );
    // Unchanged, and empty: what the row's flags cannot know.
    edit(&env, fresh, "first version");
    assert_eq!(
        why(&mut env, "EditMessage").await.0,
        Some(Refusal::Unchanged)
    );
    edit(&env, fresh, "   ");
    assert_eq!(why(&mut env, "EditMessage").await.0, Some(Refusal::Empty));
    // Not ours, and not allowed in this chat.
    edit(&env, theirs, "not mine");
    assert_eq!(why(&mut env, "EditMessage").await.0, Some(Refusal::NotOwn));
    react(&env, theirs, "👍", false);
    assert_eq!(
        why(&mut env, "React").await.0,
        Some(Refusal::NotAllowedHere)
    );

    // Being sent: bob's answer is held back, the transmit is in flight.
    env.handle
        .send(UiCommand::SendText {
            conv: bob_conv,
            text: Text::plain("on its way"),
            reply_to: None,
        })
        .expect("send");
    let control = env.control.clone();
    tokio::time::timeout(
        WAIT,
        control.wait_for(|c| c.transmits(uuid).iter().any(|t| t.req.conv == d_bob())),
    )
    .await
    .expect("transmitted");
    let mine = own_messages(&env.core_db(), bob_conv)
        .into_iter()
        .next_back()
        .expect("queued")
        .id;
    delete(&env, mine, false);
    assert_eq!(
        why(&mut env, "DeleteMessage").await.0,
        Some(Refusal::BeingSent)
    );
    env.clock.advance(Duration::from_secs(60));
    until(&env, "sent", move |c| {
        c.query_row(
            "SELECT count(*) FROM outbox WHERE message_id = ?1",
            [mine.0],
            |r| r.get::<_, i64>(0),
        )
        .is_ok_and(|n| n == 0)
    })
    .await;
    // Edited too often: this chat takes one edit per message.
    edit(&env, mine, "second wording");
    edit(&env, mine, "third wording");
    assert_eq!(
        why(&mut env, "EditMessage").await.0,
        Some(Refusal::EditedTooOften)
    );

    // Deleted: nothing but a delete for me applies to a tombstone.
    delete(&env, fresh, false);
    until(&env, "deleted for me", move |c| {
        c.query_row(
            "SELECT deleted FROM messages WHERE id = ?1",
            [fresh.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .is_ok_and(|d| d.is_some())
    })
    .await;
    edit(&env, fresh, "after the delete");
    assert_eq!(why(&mut env, "EditMessage").await.0, Some(Refusal::Deleted));
    react(&env, fresh, "👍", false);
    assert_eq!(why(&mut env, "React").await.0, Some(Refusal::Deleted));
    // Gone: a message that does not exist (any more).
    edit(&env, MessageId(9_999_999), "nobody");
    assert_eq!(why(&mut env, "EditMessage").await.0, Some(Refusal::Gone));
    // An empty text is refused before anything is queued.
    env.handle
        .send(UiCommand::SendText {
            conv: bob_conv,
            text: Text::plain(" \n "),
            reply_to: None,
        })
        .expect("send");
    assert_eq!(why(&mut env, "SendText").await.0, Some(Refusal::Empty));
    env.stop().await;
}

// ---------- conversation timer ----------

/// `SetConversationTimer` takes the timers the chat offers by name and any value of the range
/// it states (a custom time); the list is a menu, not a limit. What neither holds never
/// reaches the network, and the refusal says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_conversation_timer_takes_offered_values_and_those_of_its_range() {
    use kourier_core::facade::Refusal;
    use kourier_fake::scripts::TIMER_MAX_SECS;
    // Bob's chat offers a menu and no custom time.
    let mut listed = ConversationPatch::new(d_bob());
    let mut features = signal_features(false);
    features.disappearing_custom.clear();
    listed.features = Some(features);
    let mut first = setup();
    first.push(BackendEvent::ConversationUpsert(listed));
    let script = kourier_fake::Script::new("timer", vec![uid(ME), uid(ME_PNI)]).deliver(first);
    let (mut env, account, uuid) = linked(script).await;
    env.wait_finished(account).await;
    let conv = conv_id(&env.core_db(), &d_alice()).expect("conversation");
    let menu_only = conv_id(&env.core_db(), &d_bob()).expect("conversation");
    let set = |env: &Env, conv: ConversationId, secs: Option<u64>| {
        env.handle
            .send(UiCommand::SetConversationTimer {
                conv,
                timer: secs.map(Duration::from_secs),
            })
            .expect("send");
    };
    let timer_is = |env: &Env, conv: ConversationId, secs: Option<i64>| {
        let db = env.core_db();
        async move {
            eventually("the timer", WAIT, || {
                let db = db.clone();
                async move {
                    core_db(&db, |c| {
                        c.query_row(
                            "SELECT timer_secs FROM conversations WHERE id = ?1",
                            [conv.0],
                            |r| r.get::<_, Option<i64>>(0),
                        )
                        .is_ok_and(|t| t == secs)
                    })
                }
            })
            .await;
        }
    };
    async fn refused(env: &mut Env) -> (Option<Refusal>, String) {
        env.event("SetConversationTimer", |e| match e {
            UiEvent::ActionFailed {
                action: "SetConversationTimer",
                error,
                ..
            } => Some((error.refusal(), error.to_string())),
            _ => None,
        })
        .await
    }

    // 45 seconds is in nobody's menu: a custom time, taken because the chat states a range.
    set(&env, conv, Some(45));
    timer_is(&env, conv, Some(45)).await;
    // An offered value, the longest of the range, and off.
    set(&env, conv, Some(3600));
    timer_is(&env, conv, Some(3600)).await;
    set(&env, conv, Some(u64::from(TIMER_MAX_SECS)));
    timer_is(&env, conv, Some(i64::from(TIMER_MAX_SECS))).await;
    set(&env, conv, None);
    timer_is(&env, conv, None).await;
    let changes = |env: &Env| {
        env.control
            .commands(uuid)
            .iter()
            .filter(|c| **c == "ChangeConversation")
            .count()
    };
    assert_eq!(changes(&env), 4);

    // Beyond the range, and a fraction of a second: refused, with the reason.
    set(&env, conv, Some(u64::from(TIMER_MAX_SECS) + 1));
    let (reason, text) = refused(&mut env).await;
    assert_eq!(reason, Some(Refusal::NotAllowedHere));
    assert!(text.contains("does not offer"), "{text}");
    env.handle
        .send(UiCommand::SetConversationTimer {
            conv,
            timer: Some(Duration::from_millis(1500)),
        })
        .expect("send");
    assert_eq!(refused(&mut env).await.0, Some(Refusal::NotAllowedHere));
    // A chat with a menu and no range takes the menu's values only.
    set(&env, menu_only, Some(45));
    assert_eq!(refused(&mut env).await.0, Some(Refusal::NotAllowedHere));
    set(&env, menu_only, Some(30));
    timer_is(&env, menu_only, Some(30)).await;
    assert_eq!(changes(&env), 5, "no refused timer reached the backend");
}

// ---------- ingest (no runtime) ----------

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn features() -> BackendEvent {
    let mut p = ConversationPatch::new(d_alice());
    p.features = Some(signal_features(false));
    BackendEvent::ConversationUpsert(p)
}

fn timeline_of(h: &IngestHarness) -> Vec<MessageRow> {
    let conv = h
        .connection()
        .query_row("SELECT id FROM conversations", [], |r| {
            r.get(0).map(ConversationId)
        })
        .expect("conversation");
    h.timeline(&kourier_core::facade::TimelineQuery {
        conv,
        anchor: kourier_core::facade::TimelineAnchor::Newest,
        limit: 100,
    })
    .expect("timeline")
    .rows
}

/// S13 "done when": an edit, a reaction and a delete that arrive before their message apply
/// when it comes, and give the same rows as the other order.
#[test]
fn modifiers_before_their_target_apply_on_arrival() {
    let target =
        |local: &str| kourier_fake::scripts::target(Some(&d_alice()), Some(&alice()), local);
    let m1 = BackendEvent::Message(remote(&d_alice(), &alice(), "1000", T0, "typo"));
    let m2 = BackendEvent::Message(remote(&d_alice(), &alice(), "2000", T0 + 1, "oops"));
    let edited = BackendEvent::Edited {
        target: target("1000"),
        by: Some(alice()),
        new_local: Some("1500".into()),
        body: Body::text("fixed"),
        edited_at: T0 + HOUR,
        keeps_quote: None,
    };
    let reaction = BackendEvent::Reaction {
        target: target("1500"),
        by: Some(bob()),
        emoji: "👍".into(),
        remove: false,
        at: T0 + 2 * HOUR,
    };
    let deleted = BackendEvent::Deleted {
        target: target("2000"),
        by: Some(alice()),
        for_everyone: true,
        at: T0 + HOUR,
    };
    let orders = [
        vec![
            features(),
            m1.clone(),
            m2.clone(),
            edited.clone(),
            reaction.clone(),
            deleted.clone(),
        ],
        vec![
            features(),
            reaction.clone(),
            edited.clone(),
            deleted.clone(),
            m2,
            m1,
        ],
    ];
    let mut results = Vec::new();
    for events in orders {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir);
        for e in events {
            h.apply(vec![e]).expect("apply");
        }
        let rows = timeline_of(&h);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let typo = rows.iter().find(|r| r.sent_at == T0).expect("m1");
        assert_eq!((typo.revision, text(&typo.body)), (1, "fixed"));
        assert_eq!(typo.reactions.len(), 1, "the reaction named the edit's id");
        let oops = rows.iter().find(|r| r.sent_at == T0 + 1).expect("m2");
        assert!(matches!(oops.deleted, Some(Deletion::ForEveryone { .. })));
        assert_eq!(text(&oops.body), "");
        results.push(
            h.dump(kourier_core::testing::DumpMode::OrderInsensitive)
                .expect("dump"),
        );
    }
    assert_eq!(results[0], results[1]);
}

/// Receive windows (§3.8, Signal: edits ≤ 48 h, deletes ≤ 2 days) and authorship: late or
/// foreign edits and deletes are ignored.
#[test]
fn late_or_foreign_edits_and_deletes_are_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let target = || kourier_fake::scripts::target(Some(&d_alice()), Some(&alice()), "1000");
    h.apply(vec![
        features(),
        BackendEvent::Message(remote(&d_alice(), &alice(), "1000", T0, "original")),
    ])
    .expect("apply");
    h.apply(vec![
        BackendEvent::Edited {
            target: target(),
            by: Some(alice()),
            new_local: Some("late".into()),
            body: Body::text("49 hours later"),
            edited_at: T0 + 49 * HOUR,
            keeps_quote: None,
        },
        BackendEvent::Edited {
            target: target(),
            by: Some(bob()),
            new_local: Some("foreign".into()),
            body: Body::text("not the author"),
            edited_at: T0 + HOUR,
            keeps_quote: None,
        },
        BackendEvent::Deleted {
            target: target(),
            by: Some(alice()),
            for_everyone: true,
            at: T0 + 49 * HOUR,
        },
        BackendEvent::Deleted {
            target: target(),
            by: Some(bob()),
            for_everyone: true,
            at: T0 + HOUR,
        },
    ])
    .expect("apply");
    let rows = timeline_of(&h);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].revision, text(&rows[0].body)), (0, "original"));
    assert!(rows[0].deleted.is_none());
    // Within the window it applies.
    h.apply(vec![BackendEvent::Edited {
        target: target(),
        by: Some(alice()),
        new_local: Some("ok".into()),
        body: Body::text("47 hours later"),
        edited_at: T0 + 47 * HOUR,
        keeps_quote: None,
    }])
    .expect("apply");
    assert_eq!(text(&timeline_of(&h)[0].body), "47 hours later");
}

/// Messages that become read through a merge of two chats of one person (the surviving chat
/// was read past them on the phone; a copy of the message was read there) start their
/// `AfterRead` timers like after any other read, so they still disappear.
#[test]
fn a_merge_that_marks_messages_read_starts_their_timers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let pni = uid("pni:00000000-0000-4000-8000-0000000a11ce");
    let d_pni = ConvKey::Direct(pni.clone());
    let secret = |local: &str, at: i64| {
        let mut m = remote(&d_pni, &pni, local, at, "burn after reading");
        m.expiry = Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_secs(60),
            started_at: None,
        });
        BackendEvent::Message(m)
    };
    // Two unread messages from alice's phone-number identity.
    h.apply(vec![secret("100", 100), secret("150", 150)])
        .expect("apply");
    // The second one also came under her account identity, and was read on the phone.
    h.apply(vec![
        BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            "150",
            150,
            "burn after reading",
        )),
        BackendEvent::ReadMarker {
            conv: None,
            up_to: kourier_fake::scripts::target(None, Some(&alice()), "150"),
        },
    ])
    .expect("apply");
    let state = |h: &IngestHarness, local: &str| -> (Option<String>, Option<i64>) {
        h.connection()
            .query_row(
                "SELECT read_state, expires_at FROM messages WHERE net_local = ?1
                   AND expiry_timer_ms IS NOT NULL",
                [local],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("row")
    };
    for local in ["100", "150"] {
        assert_eq!(state(&h, local), (Some("Unread".to_owned()), None));
    }

    let now = 1_700_000_300_000;
    h.set_now(now);
    h.apply(vec![BackendEvent::IdentityMerged {
        from: pni,
        into: alice(),
    }])
    .expect("apply");
    let count = |sql: &str| -> i64 {
        h.connection()
            .query_row(sql, [], |r| r.get(0))
            .expect("count")
    };
    assert_eq!(count("SELECT count(*) FROM conversations"), 1, "one chat");
    assert_eq!(
        count("SELECT count(*) FROM messages"),
        2,
        "the copy merged into its original"
    );
    for local in ["100", "150"] {
        assert_eq!(
            state(&h, local),
            (Some("Read".to_owned()), Some(now + 60_000)),
            "{local}"
        );
    }
}

/// `AttachmentDone` names its request: a late answer to an earlier request (a resend that ran
/// twice) cannot fail the retry in flight, and rows written before the field existed still
/// apply to the download in flight.
#[test]
fn stale_download_answers_cannot_fail_a_newer_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut photo = remote(&d_alice(), &alice(), "1000", T0, "");
    photo.body = Body::Media {
        attachments: vec![kourier_fake::attachments::synthetic_image(64, 48, 3)],
        caption: None,
    };
    h.apply(vec![features(), BackendEvent::Message(photo)])
        .expect("apply");
    let current = |h: &IngestHarness| -> Option<(i64, i64)> {
        h.connection()
            .query_row(
                "SELECT id, attachment_id FROM attachment_downloads",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
    };
    let state = |h: &IngestHarness| -> AttachmentState {
        let s: String = h
            .connection()
            .query_row("SELECT state FROM attachments", [], |r| r.get(0))
            .expect("state");
        serde_json::from_str(&s).expect("json")
    };
    let (first, att) = current(&h).expect("the policy asked for it");
    let done = |request: Option<i64>, ok: bool| BackendEvent::AttachmentDone {
        attachment: kourier_model::AttachmentId(att),
        request: request.map(DownloadId),
        result: if ok {
            Ok(kourier_model::AttachmentMeta {
                size: 10,
                ..Default::default()
            })
        } else {
            Err(BackendError::transient("connection reset"))
        },
    };
    h.apply(vec![done(Some(first), false)]).expect("apply");
    assert_eq!(state(&h), AttachmentState::Failed { retryable: true });
    assert!(current(&h).is_none());
    // The user retries: a new request with a new id.
    h.connection()
        .execute(
            "INSERT INTO attachment_downloads(attachment_id, account_id, path, key, requested_at)
             VALUES (?1, ?2, 'core/cd/cd0123456789abcdef0123456789abcd', randomblob(32), 1)",
            (att, h.account().0),
        )
        .expect("retry");
    let (second, _) = current(&h).expect("in flight");
    assert!(second > first, "request ids are never reused");
    // The first attempt's duplicate answer arrives late: ignored.
    h.apply(vec![done(Some(first), false)]).expect("apply");
    assert_eq!(
        current(&h).map(|c| c.0),
        Some(second),
        "the retry is still in flight"
    );
    // An answer without a request id (an old row) applies to the download in flight.
    h.apply(vec![done(None, true)]).expect("apply");
    assert_eq!(state(&h), AttachmentState::Local);
    assert!(current(&h).is_none());
    // Its own answer replayed later changes nothing.
    h.apply(vec![done(Some(second), false)]).expect("apply");
    assert_eq!(state(&h), AttachmentState::Local);
}

/// Message and outbox ids are never handed out twice, also after the newest row was deleted,
/// and a schema-2 `core.db` keeps its rows, ids and foreign keys through the rebuilds.
#[test]
fn message_ids_are_never_reused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([5; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v2 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..2]);
    kourier_db::migrate(&mut conn, &v2).expect("v2");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'note', 'Direct');
         INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (1, 1, 1, 0, '', '1', 'OwnThisDevice', 1, 1, 1, 'Text', '{}'),
                  (2, 1, 1, 0, '', '2', 'OwnThisDevice', 2, 2, 2, 'Text', '{}');
         UPDATE messages SET linked_message_id = 1 WHERE id = 2;
         INSERT INTO reactions(message_id, user_id, emoji, at) VALUES (1, 'aci:x', 'a', 1);
         INSERT INTO message_revisions(message_id, account_id, net_author, net_local, body)
           VALUES (1, 1, '', '1', '{}');
         INSERT INTO send_state(message_id, user_id, state, at) VALUES (1, 'aci:x', '\"Pending\"', 1);
         INSERT INTO attachments(id, message_id, idx, kind, state) VALUES (7, 1, 0, 'File', '\"Remote\"');
         INSERT INTO attachment_downloads(attachment_id, account_id, path, key, requested_at)
           VALUES (7, 1, 'core/ab/x', x'00', 0);
         INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
           VALUES (1, 1, 'Message', '{}', 'Queued', 0);",
    )
    .expect("rows");
    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!((out.from, out.to), (2, 12));
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).expect(sql) };
    assert_eq!(count("SELECT count(*) FROM messages"), 2);
    for table in [
        "reactions",
        "message_revisions",
        "send_state",
        "attachments",
        "outbox",
    ] {
        assert_eq!(
            count(&format!("SELECT count(*) FROM {table}")),
            1,
            "{table} kept"
        );
    }
    assert_eq!(
        count("SELECT linked_message_id FROM messages WHERE id = 2"),
        1
    );
    assert_eq!(
        count("SELECT id FROM attachment_downloads"),
        1,
        "request id assigned"
    );
    assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
    assert_eq!(count("PRAGMA foreign_keys"), 1);
    // Children still follow their message.
    conn.execute("DELETE FROM messages WHERE id = 1", [])
        .expect("delete");
    for table in [
        "reactions",
        "message_revisions",
        "send_state",
        "attachments",
        "outbox",
    ] {
        assert_eq!(
            count(&format!("SELECT count(*) FROM {table}")),
            0,
            "{table} cascaded"
        );
    }
    assert_eq!(
        count("SELECT count(*) FROM messages WHERE linked_message_id IS NULL"),
        1
    );
    // The newest id (2) goes, and is not handed out again.
    conn.execute("DELETE FROM messages WHERE id = 2", [])
        .expect("delete");
    conn.execute(
        "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
         VALUES (1, 1, 0, '', '4', 'OwnThisDevice', 4, 4, 4, 'Text', '{}')",
        [],
    )
    .expect("insert");
    assert_eq!(conn.last_insert_rowid(), 3, "2 is not handed out again");
    conn.execute("DELETE FROM messages", []).expect("delete");
    conn.execute(
        "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
         VALUES (1, 1, 0, '', '5', 'OwnThisDevice', 5, 5, 5, 'Text', '{}')",
        [],
    )
    .expect("insert");
    assert_eq!(conn.last_insert_rowid(), 4, "not even after every row went");
    // Outbox rows too (the dispatcher keys attempts in flight by them): the newest goes, its id
    // is not handed out again.
    let outbox = |conn: &kourier_db::rusqlite::Connection| {
        conn.execute(
            "INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
             VALUES (4, 1, 'Message', '{}', 'Queued', 0)",
            [],
        )
        .expect("outbox row");
        conn.last_insert_rowid()
    };
    let first = outbox(&conn);
    assert_eq!(
        first, 2,
        "the id of the migrated row (1) is not handed out again"
    );
    conn.execute("DELETE FROM outbox", []).expect("delete");
    assert_eq!(outbox(&conn), 3, "outbox ids are not reused");
}

/// Migration 0008 rebuilds `accounts` with AUTOINCREMENT: every account keeps its row, its
/// id and everything that hangs on it (the rebuild runs with foreign keys off, so dropping
/// the old table cascades nowhere), ids go on after the highest one, and an id that was
/// freed is not handed out again. The same step adds the index the dispatcher walks.
#[test]
fn the_accounts_rebuild_keeps_every_row_and_never_reuses_an_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([5; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v7 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..7]);
    kourier_db::migrate(&mut conn, &v7).expect("v7");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, state_reason, own_ids, own_device,
           display_name, settings, local_prefs, caps, sort_order, created_at, number)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', NULL,
                   '[\"aci:x\"]', 2, 'One', '{\"a\":1}', '{\"b\":2}', '{\"c\":3}', 4, 5, '+100'),
                  (2, '7c4f1d2f-2e3b-4d6c-8f90-1b2c3d4e5f60', 'fake', 'LoggedOut', 'gone',
                   '[]', NULL, NULL, '{}', '{}', NULL, NULL, 6, NULL);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES
           (1, 1, 'note', 'Direct'), (2, 2, 'note', 'Direct');
         INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (1, 1, 1, 0, '', '1', 'OwnThisDevice', 1, 1, 1, 'Text', '{}'),
                  (2, 2, 2, 0, '', '2', 'OwnThisDevice', 2, 2, 2, 'Text', '{}');
         INSERT INTO contacts(account_id, user_id) VALUES (1, 'aci:x'), (2, 'aci:y');
         INSERT INTO pending_commands(account_id, command, created_at) VALUES (2, '{}', 0);
         INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
           VALUES (1, 1, 'Message', '{}', 'Queued', 0);",
    )
    .expect("rows");
    let rows = |conn: &kourier_db::rusqlite::Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT json_array(id, uuid, network, state, state_reason, own_ids, own_device,
                   display_name, settings, local_prefs, caps, sort_order, created_at, number)
                 FROM accounts ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    let before = rows(&conn);
    assert_eq!(before.len(), 2);

    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!((out.from, out.to), (7, 12));
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).expect(sql) };
    assert_eq!(rows(&conn), before, "every column of every account");
    for table in [
        "conversations",
        "messages",
        "contacts",
        "pending_commands",
        "outbox",
    ] {
        assert!(
            count(&format!("SELECT count(*) FROM {table}")) >= 1,
            "{table} kept its rows"
        );
    }
    assert_eq!(count("SELECT count(*) FROM messages"), 2);
    assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
    assert_eq!(count("PRAGMA foreign_keys"), 1);
    assert_eq!(
        count(
            "SELECT count(*) FROM sqlite_master WHERE name = 'accounts'
               AND sql LIKE '%id INTEGER PRIMARY KEY AUTOINCREMENT%'"
        ),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE name LIKE 'accounts_v2%'"),
        0
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'outbox_waiting'"
        ),
        1
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'download_orphans'"
        ),
        1
    );
    // The uuid is still unique.
    assert!(
        conn.execute(
            "INSERT INTO accounts(uuid, network, state, created_at)
             VALUES ('6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Linking', 0)",
            [],
        )
        .is_err()
    );

    // Removing the newest account takes its rows along (the children still reference
    // `accounts`), and its id is not handed out again.
    conn.execute("DELETE FROM accounts WHERE id = 2", [])
        .expect("delete");
    assert_eq!(count("SELECT count(*) FROM conversations"), 1);
    assert_eq!(count("SELECT count(*) FROM messages"), 1);
    assert_eq!(count("SELECT count(*) FROM pending_commands"), 0);
    let add = |conn: &kourier_db::rusqlite::Connection, uuid: &str| {
        conn.execute(
            "INSERT INTO accounts(uuid, network, state, created_at) VALUES (?1, 'fake', 'Linking', 0)",
            [uuid],
        )
        .expect("account");
        conn.last_insert_rowid()
    };
    assert_eq!(add(&conn, "00000000-0000-4000-8000-000000000003"), 3);
    conn.execute("DELETE FROM accounts", []).expect("delete");
    assert_eq!(
        add(&conn, "00000000-0000-4000-8000-000000000004"),
        4,
        "not 1 again, although no account is left"
    );
    drop(conn);
    // A database that is already at the newest version opens as it is.
    let mut again = kourier_db::open(&path, &key).expect("open");
    let out = kourier_db::migrate(&mut again, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!((out.from, out.to), (12, 12));
}

/// Migration 4 alone, as a schema-3 `core.db` gets it (a plain step, foreign keys enforced):
/// the outbox keeps its rows, ids and values, the table differs from before only in
/// AUTOINCREMENT (same columns, defaults, keys and indexes), its parents still cascade, and the
/// ids continue after the highest one that was there.
#[test]
fn the_outbox_rebuild_keeps_its_rows_and_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([6; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v3 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..3]);
    kourier_db::migrate(&mut conn, &v3).expect("v3");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind)
           VALUES (1, 1, 'a', 'Direct'), (2, 1, 'b', 'Direct');
         INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (1, 1, 1, 0, '', '1', 'OwnThisDevice', 1, 1, 1, 'Text', '{}'),
                  (2, 1, 1, 0, '', '2', 'OwnThisDevice', 2, 2, 2, 'Text', '{}'),
                  (3, 1, 2, 0, '', '3', 'OwnThisDevice', 3, 3, 3, 'Text', '{}');
         INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
           next_attempt_at, deadline_at, last_error, undo)
           VALUES (3, 1, 1, 'Message', '{\"n\":1}', 'Failed', 4, NULL, 100, 'Transient', NULL),
                  (7, 2, 1, 'Edit', '{\"n\":2}', 'Queued', 1, 55, 200, NULL, '\"Reached\"'),
                  (8, 3, 2, 'Message', '{\"n\":3}', 'Queued', 0, NULL, 300, NULL, NULL),
                  (9, 3, 2, 'React', '{\"n\":4}', 'Transmitting', 2, NULL, 400, NULL, NULL);
         DELETE FROM outbox WHERE id = 9;",
    )
    .expect("rows");
    let list = |conn: &kourier_db::rusqlite::Connection, sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(sql).expect(sql);
        stmt.query_map([], |r| r.get(0))
            .expect(sql)
            .collect::<Result<_, _>>()
            .expect(sql)
    };
    // Columns (type, NOT NULL, default, key), foreign keys and indexes of the table.
    let shape = |conn: &kourier_db::rusqlite::Connection| {
        [
            "SELECT name || ' ' || type || ' ' || \"notnull\" || ' ' || ifnull(dflt_value, '-')
               || ' ' || pk FROM pragma_table_info('outbox') ORDER BY cid",
            "SELECT \"from\" || ' ' || \"table\" || ' ' || on_delete
             FROM pragma_foreign_key_list('outbox') ORDER BY \"from\"",
            // (`outbox_waiting` came with a later step, 0008.)
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'outbox'
               AND name <> 'outbox_waiting' ORDER BY name",
        ]
        .map(|sql| list(conn, sql))
    };
    let rows = |conn: &kourier_db::rusqlite::Connection| {
        list(
            conn,
            "SELECT json_array(id, message_id, conversation_id, kind, request, state, attempts,
               next_attempt_at, deadline_at, last_error, undo) FROM outbox ORDER BY id",
        )
    };
    let (shape_before, rows_before) = (shape(&conn), rows(&conn));
    assert_eq!(shape_before[0].len(), 11, "{shape_before:?}");
    assert_eq!(shape_before[1].len(), 2, "{shape_before:?}");
    assert_eq!(shape_before[2].len(), 2, "{shape_before:?}");
    assert_eq!(rows_before.len(), 3);
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).expect(sql) };
    assert_eq!(count("PRAGMA foreign_keys"), 1, "enforced during the step");

    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!((out.from, out.to), (3, 12));
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).expect(sql) };
    assert_eq!(shape(&conn), shape_before);
    assert_eq!(rows(&conn), rows_before);
    assert_eq!(
        count(
            "SELECT count(*) FROM sqlite_master WHERE name = 'outbox'
               AND sql LIKE '%id INTEGER PRIMARY KEY AUTOINCREMENT%'"
        ),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE name LIKE 'outbox_v2%'"),
        0
    );
    assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
    assert_eq!(count("PRAGMA foreign_keys"), 1);
    // The parents kept their rows (dropping the old table deleted nothing else).
    assert_eq!(count("SELECT count(*) FROM messages"), 3);
    assert_eq!(count("SELECT count(*) FROM conversations"), 2);

    // Ids continue after the highest row that was there (9 went before the migration, when
    // nothing could be in flight), and from now on none comes back.
    let insert = |conn: &kourier_db::rusqlite::Connection| {
        conn.execute(
            "INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
             VALUES (3, 2, 'Message', '{}', 'Queued', 0)",
            [],
        )
        .expect("outbox row");
        conn.last_insert_rowid()
    };
    assert_eq!(insert(&conn), 9);
    conn.execute("DELETE FROM outbox WHERE id = 9", [])
        .expect("delete");
    assert_eq!(insert(&conn), 10, "9 is not handed out again");
    assert_eq!(
        count("SELECT attempts FROM outbox WHERE id = 10"),
        0,
        "the default"
    );
    // A message takes its rows along, and so does a conversation.
    conn.execute("DELETE FROM messages WHERE id = 1", [])
        .expect("delete");
    assert_eq!(count("SELECT count(*) FROM outbox WHERE id = 3"), 0);
    assert_eq!(count("SELECT count(*) FROM outbox"), 3);
    conn.execute("DELETE FROM conversations WHERE id = 2", [])
        .expect("delete");
    assert_eq!(
        list(&conn, "SELECT CAST(id AS TEXT) FROM outbox ORDER BY id"),
        ["7"]
    );
    conn.execute("DELETE FROM outbox", []).expect("delete");
    conn.execute(
        "INSERT INTO outbox(message_id, conversation_id, kind, request, state, deadline_at)
         VALUES (2, 1, 'Message', '{}', 'Queued', 0)",
        [],
    )
    .expect("outbox row");
    assert_eq!(
        conn.last_insert_rowid(),
        11,
        "not even after every row went"
    );
}
