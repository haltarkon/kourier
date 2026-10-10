//! The facade's revision query (`Query::Revisions`, §3.5.3): the versions of a message with
//! their times, for the edit history behind the "edited" mark; and the account's own number
//! (`AccountReady.number`), which follows a number change.

mod support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{MessageRevisions, Query, QueryData, UiCommand};
use kourier_core::testing::IngestHarness;
use kourier_fake::Flavor;
use kourier_fake::attachments::synthetic_image;
use kourier_fake::scripts::{T0, remote, signal_features, target};
use kourier_model::{
    AccountState, Body, ConvKey, ConversationPatch, MessageId, MsgIdScope, NetworkKind, Redacted,
    Text,
};
use support::send::{ME, ME_PNI, alice, conv_id, d_alice, own_messages, script, uid};
use support::{Env, WAIT, add_linked, eventually};

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn chat(key: &ConvKey) -> BackendEvent {
    let mut p = ConversationPatch::new(key.clone());
    p.features = Some(signal_features(false));
    BackendEvent::ConversationUpsert(p)
}

fn edited(of: i64, new_local: i64, at: i64, body: Body) -> BackendEvent {
    BackendEvent::Edited {
        target: target(Some(&d_alice()), Some(&alice()), &(T0 + of).to_string()),
        by: Some(alice()),
        new_local: Some((T0 + new_local).to_string().into()),
        body,
        edited_at: T0 + at,
        keeps_quote: None,
    }
}

fn id_of(h: &IngestHarness, local: i64) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [(T0 + local).to_string()],
            |r| r.get(0).map(MessageId),
        )
        .expect("message")
}

/// (revision, text, time) of every version.
fn versions(r: &MessageRevisions) -> Vec<(u16, Option<String>, i64)> {
    r.revisions
        .iter()
        .map(|v| (v.revision, v.text.as_ref().map(|t| t.body.clone()), v.at))
        .collect()
}

/// An edit can take the quote off a message, never change it (Signal Desktop
/// `handleEditMessage`: "quote can be removed but not modified"). An edit that says it has
/// no quote removes the quote of the message it edits; one that says it kept it, or says
/// nothing, leaves it; and once a quote is gone no later edit brings it back. The order the
/// edits and the message arrive in does not matter.
#[test]
fn an_edit_without_a_quote_takes_the_quote_off_the_message() {
    let reply = |local: i64, text: &str| {
        let mut m = remote(
            &d_alice(),
            &alice(),
            &(T0 + local).to_string(),
            T0 + local,
            text,
        );
        m.reply_to = Some(kourier_model::ReplyTo {
            target: target(Some(&d_alice()), Some(&alice()), &(T0 + 1000).to_string()),
            author: Some(alice()),
            snapshot: None,
        });
        BackendEvent::Message(m)
    };
    let edit = |of: i64, new_local: i64, text: &str, keeps_quote: Option<bool>| {
        let BackendEvent::Edited {
            target,
            by,
            new_local,
            body,
            edited_at,
            ..
        } = edited(of, new_local, new_local, Body::text(text))
        else {
            unreachable!()
        };
        BackendEvent::Edited {
            target,
            by,
            new_local,
            body,
            edited_at,
            keeps_quote,
        }
    };
    // (text, has a quote) of the message sent at `local`.
    let row = |h: &IngestHarness, local: i64| -> (String, bool) {
        let conv = h
            .connection()
            .query_row("SELECT id FROM conversations", [], |r| {
                r.get(0).map(kourier_model::ConversationId)
            })
            .expect("conversation");
        let page = h
            .timeline(&kourier_core::facade::TimelineQuery {
                conv,
                anchor: kourier_core::facade::TimelineAnchor::Newest,
                limit: 50,
            })
            .expect("timeline");
        let r = page
            .rows
            .into_iter()
            .find(|r| r.id == id_of(h, local))
            .expect("the row");
        let text = match r.body {
            Body::Text(t) => t.body,
            other => other.kind_name().to_owned(),
        };
        (text, r.reply.is_some())
    };
    let original = BackendEvent::Message(remote(
        &d_alice(),
        &alice(),
        &(T0 + 1000).to_string(),
        T0 + 1000,
        "the question",
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        chat(&d_alice()),
        original.clone(),
        reply(2000, "yes"),
        reply(3000, "no"),
        reply(4000, "maybe"),
    ])
    .expect("apply");
    assert_eq!(row(&h, 2000), ("yes".to_owned(), true));

    // The edit kept the quote, or its network says nothing about quotes: it stays.
    h.apply(vec![
        edit(2000, 2100, "yes!", Some(true)),
        edit(3000, 3100, "no!", None),
    ])
    .expect("apply");
    assert_eq!(row(&h, 2000), ("yes!".to_owned(), true));
    assert_eq!(row(&h, 3000), ("no!".to_owned(), true));

    // The edit has no quote: the message loses its quote, and keeps its new text.
    h.apply(vec![edit(2000, 2200, "yes, definitely", Some(false))])
        .expect("apply");
    assert_eq!(row(&h, 2000), ("yes, definitely".to_owned(), false));
    assert_eq!(row(&h, 3000), ("no!".to_owned(), true), "only that message");
    // Applied twice it changes nothing, and a later edit cannot bring the quote back.
    h.apply(vec![
        edit(2000, 2200, "yes, definitely", Some(false)),
        edit(2000, 2300, "yes, absolutely", Some(true)),
    ])
    .expect("apply");
    assert_eq!(row(&h, 2000), ("yes, absolutely".to_owned(), false));

    // Arrival order: the edit without a quote comes before an older edit of the same
    // message, and before the message itself in another profile. The end is the same.
    h.apply(vec![
        edit(4000, 4200, "maybe not", Some(false)),
        edit(4000, 4100, "maybe so", Some(true)),
    ])
    .expect("apply");
    assert_eq!(row(&h, 4000), ("maybe not".to_owned(), false));

    let dir2 = tempfile::tempdir().expect("tempdir");
    let mut late = harness(&dir2);
    late.apply(vec![
        chat(&d_alice()),
        edit(4000, 4200, "maybe not", Some(false)),
        edit(4000, 4100, "maybe so", Some(true)),
        original,
        reply(4000, "maybe"),
    ])
    .expect("apply");
    assert_eq!(row(&late, 4000), ("maybe not".to_owned(), false));
}

#[test]
fn the_versions_of_a_message_come_oldest_first_with_their_times() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut photo = remote(
        &d_alice(),
        &alice(),
        &(T0 + 2000).to_string(),
        T0 + 2000,
        "",
    );
    photo.body = Body::Media {
        attachments: vec![synthetic_image(64, 64, 1)],
        caption: None,
    };
    h.apply(vec![
        chat(&d_alice()),
        BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            &(T0 + 1000).to_string(),
            T0 + 1000,
            "teh first",
        )),
        BackendEvent::Message(photo),
        BackendEvent::Message(remote(
            &d_alice(),
            &alice(),
            &(T0 + 3000).to_string(),
            T0 + 3000,
            "never touched",
        )),
    ])
    .expect("apply");
    let (text, photo, plain) = (id_of(&h, 1000), id_of(&h, 2000), id_of(&h, 3000));

    // Never edited: the one version it has, dated when it was sent.
    let one = h.revisions(plain).expect("query").expect("message");
    assert_eq!(one.message, plain);
    assert_eq!(
        versions(&one),
        [(0, Some("never touched".to_owned()), T0 + 3000)]
    );
    assert_eq!(
        versions(&h.revisions(photo).expect("query").expect("message")),
        [(0, None, T0 + 2000)],
        "a picture without a caption has a version without text"
    );

    // Two edits of the text, arriving newest first: the history is by edit time.
    h.apply(vec![edited(1000, 6000, 6000, Body::text("the first!"))])
        .expect("apply");
    h.apply(vec![edited(1000, 5000, 5000, Body::text("the first"))])
        .expect("apply");
    let history = h.revisions(text).expect("query").expect("message");
    assert_eq!(
        versions(&history),
        [
            (0, Some("teh first".to_owned()), T0 + 1000),
            (1, Some("the first".to_owned()), T0 + 5000),
            (2, Some("the first!".to_owned()), T0 + 6000),
        ]
    );
    assert_eq!(
        history.conv,
        h.connection()
            .query_row("SELECT id FROM conversations", [], |r| r
                .get(0)
                .map(kourier_model::ConversationId))
            .expect("conversation")
    );
    assert!(
        !format!("{history:?}").contains("first"),
        "Debug never prints a text"
    );

    // A picture that gets a caption, then another: every caption is a version, the files are
    // the same throughout.
    h.apply(vec![
        edited(2000, 7000, 7000, Body::text("at the lake")),
        edited(7000, 8000, 8000, Body::text("at the lake, in May")),
    ])
    .expect("apply");
    assert_eq!(
        versions(&h.revisions(photo).expect("query").expect("message")),
        [
            (0, None, T0 + 2000),
            (1, Some("at the lake".to_owned()), T0 + 7000),
            (2, Some("at the lake, in May".to_owned()), T0 + 8000),
        ]
    );

    // A deleted message has no versions left to show; an unknown one is `None`.
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&d_alice()), Some(&alice()), &(T0 + 1000).to_string()),
        by: Some(alice()),
        for_everyone: true,
        at: T0 + 9000,
    }])
    .expect("apply");
    let gone = h.revisions(text).expect("query").expect("the tombstone");
    assert!(gone.revisions.is_empty());
    assert!(h.revisions(MessageId(9999)).expect("query").is_none());
}

/// The same query through the facade, for the user's own edits: the text as first sent and
/// each edit, in order; the last one is what the timeline row shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_edits_show_in_the_history_through_the_facade() {
    let mut env = Env::new(Flavor::SignalLike, script("revisions")).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    let a = conv_id(&env.core_db(), &d_alice()).expect("alice");
    env.handle
        .send(UiCommand::SendText {
            conv: a,
            text: Text::plain("see you at sven"),
            reply_to: None,
        })
        .expect("send");
    let sent = |env: &Env| {
        let db = env.core_db();
        async move { support::send::outbox_len(&db) == 0 && own_messages(&db, a).len() == 1 }
    };
    eventually("the message is sent", WAIT, || sent(&env)).await;
    let message = own_messages(&env.core_db(), a)[0].id;
    for (n, text) in ["see you at seven", "see you at seven sharp"]
        .into_iter()
        .enumerate()
    {
        env.clock.advance(std::time::Duration::from_secs(60));
        env.handle
            .send(UiCommand::EditMessage {
                message,
                text: Text::plain(text),
            })
            .expect("send");
        // Stored (the row's revision moved) and sent (nothing waits in the outbox).
        let want = i64::try_from(n).expect("small") + 1;
        eventually("the edit is stored and sent", WAIT, || {
            let (db, sent) = (env.core_db(), sent(&env));
            async move {
                sent.await
                    && support::send::core_db(&db, |c| {
                        c.query_row(
                            "SELECT revision FROM messages WHERE id = ?1",
                            [message.0],
                            |r| r.get::<_, i64>(0),
                        )
                        .is_ok_and(|revision| revision == want)
                    })
            }
        })
        .await;
    }
    let history = match env.handle.query(Query::Revisions(message)).await {
        Ok(QueryData::Revisions(Some(h))) => h,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!((history.message, history.conv), (message, a));
    let texts: Vec<_> = history
        .revisions
        .iter()
        .map(|r| (r.revision, r.text.as_ref().map(|t| t.body.as_str())))
        .collect();
    assert_eq!(
        texts,
        [
            (0, Some("see you at sven")),
            (1, Some("see you at seven")),
            (2, Some("see you at seven sharp"))
        ]
    );
    let times: Vec<i64> = history.revisions.iter().map(|r| r.at).collect();
    assert!(
        times.windows(2).all(|w| w[1] - w[0] >= 60_000),
        "each edit a minute after the one before: {times:?}"
    );
    let row = env
        .timeline(a)
        .await
        .into_iter()
        .find(|r| r.id == message)
        .expect("row");
    assert_eq!(row.revision, 2);
    assert_eq!(row.edited_at, times.last().copied());
    assert!(matches!(&row.body, Body::Text(t) if t.body == "see you at seven sharp"));
    assert!(matches!(
        env.handle.query(Query::Revisions(MessageId(424_242))).await,
        Ok(QueryData::Revisions(None))
    ));
    env.stop().await;
}

/// `AccountReady.number`: stated at the link, left alone by an `AccountReady` without one,
/// replaced (with the own ids) after a number change on the phone; never printed.
#[test]
fn the_accounts_number_follows_a_number_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let ready = |pni: &str, number: Option<&str>| BackendEvent::AccountReady {
        own_ids: vec![uid(ME), uid(pni)],
        device: Some(2),
        display_name: Some(Redacted::new("Me".into())),
        number: number.map(|n| Redacted::new(n.to_owned())),
    };
    let row = |h: &IngestHarness| h.accounts().expect("accounts").remove(0);
    assert_eq!(row(&h).number, None, "no backend has said yet");
    h.apply(vec![ready(ME_PNI, Some("+15550100"))])
        .expect("apply");
    assert_eq!(row(&h).number.as_deref(), Some("+15550100"));
    // A backend that restates the account without the number (a rebuild, a restart).
    h.apply(vec![ready(ME_PNI, None)]).expect("apply");
    assert_eq!(row(&h).number.as_deref(), Some("+15550100"));
    // The number changed on the phone: new PNI, new number, in one event.
    let new_pni = "pni:00000000-0000-4000-8000-0000000000ac";
    h.apply(vec![ready(new_pni, Some("+15550199"))])
        .expect("apply");
    let after = row(&h);
    assert_eq!(after.number.as_deref(), Some("+15550199"));
    assert_eq!(after.state, AccountState::Active);
    let own: String = h
        .connection()
        .query_row("SELECT own_ids FROM accounts", [], |r| r.get(0))
        .expect("own ids");
    assert!(own.contains(new_pni) && !own.contains(ME_PNI), "{own}");
    assert!(
        !format!("{after:?}").contains("5550199"),
        "the number is personal data: {after:?}"
    );
}
