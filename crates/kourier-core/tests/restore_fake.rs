//! S6 against the fake network's backfill knob ([`kourier_fake::BackfillScript`]): a file the
//! CDN lost is asked from the account's phone on a click, and the phone's scripted answer
//! ends the wait: the file downloads, or the note says why not. An automatic download's 404
//! asks nothing; an account without the capability is refused.

mod support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::UiCommand;
use kourier_fake::attachments::{Pointer, expired_attachment};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{BackfillScript, Flavor, Script};
use kourier_model::{AskFailure, AttachmentId, AttachmentKind, AttachmentState, Body, ConvKey};
use support::send::{ME, ME_PNI, alice, d_alice, setup, uid};
use support::{Env, WAIT, add_linked};

/// Alice's message with one file the CDN no longer has (1000 bytes, as the answer's).
fn script(backfill: Option<BackfillScript>) -> Script {
    let mut m = remote(&d_alice(), &alice(), &(T0 + 1).to_string(), T0 + 1, "");
    m.body = Body::Media {
        attachments: vec![expired_attachment(AttachmentKind::File)],
        caption: None,
    };
    let s = Script::new("restore", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(vec![BackendEvent::Message(m)]);
    match backfill {
        Some(b) => s.with_backfill(b),
        None => s,
    }
}

/// `(id, state, has file)` of the one attachment.
fn row(env: &Env) -> Option<(AttachmentId, AttachmentState, bool)> {
    env.peek(|c| {
        c.query_row(
            "SELECT id, state, file_id IS NOT NULL FROM attachments ORDER BY id LIMIT 1",
            [],
            |r| {
                let state: String = r.get(1)?;
                Ok((
                    AttachmentId(r.get(0)?),
                    serde_json::from_str(&state).expect("state json"),
                    r.get(2)?,
                ))
            },
        )
        .ok()
    })
}

async fn wait_for(env: &Env, what: &str, done: impl Fn(&AttachmentState, bool) -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT * 3;
    loop {
        if let Some((_, s, file)) = row(env)
            && done(&s, file)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            row(env).map(|r| r.1)
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Starts the account and waits until the automatic download of the lost file found it gone
/// ("No longer available") without asking the phone.
async fn start(backfill: Option<BackfillScript>) -> (Env, AttachmentId) {
    let mut env = Env::new(Flavor::SignalLike, script(backfill)).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    wait_for(&env, "the automatic download's 404", |s, _| {
        *s == AttachmentState::Expired
    })
    .await;
    let asked: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM attachment_requests", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(asked, 0, "an automatic download's 404 asks nothing");
    let id = row(&env).expect("row").0;
    (env, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_lost_file_asks_the_phone_and_downloads_its_answer() {
    let (env, id) = start(Some(BackfillScript::Files(vec![Some(Pointer::Bytes {
        len: 1000,
        seed: 9,
    })])))
    .await;
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: id })
        .expect("send");
    wait_for(&env, "the phone's file", |s, file| {
        *s == AttachmentState::Local && file
    })
    .await;
    let left: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM attachment_requests", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(left, 0, "the answer ends the request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_phone_without_the_message_ends_the_wait_with_that_note() {
    let (env, id) = start(Some(BackfillScript::NotFound)).await;
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: id })
        .expect("send");
    wait_for(&env, "the note", |s, _| {
        *s == AttachmentState::AskFailed {
            why: AskFailure::NoMessage,
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_phone_without_the_file_ends_the_wait_with_that_note() {
    let (env, id) = start(Some(BackfillScript::Files(vec![None]))).await;
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: id })
        .expect("send");
    wait_for(&env, "the note", |s, _| {
        *s == AttachmentState::AskFailed {
            why: AskFailure::NoFile,
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_capability_a_click_asks_nothing() {
    let (env, id) = start(None).await;
    env.handle
        .send(UiCommand::DownloadAttachment { attachment: id })
        .expect("send");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let asked: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM attachment_requests", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(asked, 0);
    assert_eq!(row(&env).expect("row").1, AttachmentState::Expired);
}

/// Every attachment as `(id, message's conversation key, state)`, oldest first.
fn all_rows(env: &Env) -> Vec<(AttachmentId, String, AttachmentState)> {
    env.peek(|c| {
        let mut stmt = c
            .prepare(
                "SELECT a.id, c.conv_key, a.state FROM attachments a
                   JOIN messages m ON m.id = a.message_id
                   JOIN conversations c ON c.id = m.conversation_id ORDER BY a.id",
            )
            .expect("prepare");
        stmt.query_map([], |r| {
            let state: String = r.get(2)?;
            Ok((
                AttachmentId(r.get(0)?),
                r.get(1)?,
                serde_json::from_str(&state).expect("state json"),
            ))
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
    })
}

/// The attachments the core says a click on a row of chat `key` asks for.
async fn asks(env: &Env, key: &str) -> Vec<AttachmentId> {
    let conv: i64 = env.peek(|c| {
        c.query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key],
            |r| r.get(0),
        )
        .expect("conversation")
    });
    env.timeline(kourier_model::ConversationId(conv))
        .await
        .iter()
        .flat_map(|row| row.actions.ask_attachments.clone())
        .collect()
}

/// What the timeline row says a click asks for is the core's answer per attachment, and a chat
/// known by phone number alone is read-only: nothing is offered there and a click is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_row_lists_what_a_click_asks_and_a_number_only_chat_asks_nothing() {
    let number = uid("e164:+15550100");
    let chat = ConvKey::Direct(number.clone());
    let lost = || Body::Media {
        attachments: vec![expired_attachment(AttachmentKind::File)],
        caption: None,
    };
    let mut theirs = remote(&d_alice(), &alice(), &(T0 + 1).to_string(), T0 + 1, "");
    theirs.body = lost();
    let mut by_number = remote(&chat, &alice(), &(T0 + 2).to_string(), T0 + 2, "");
    by_number.body = lost();
    // A number-only author in a chat with a stable identity: not asked either.
    let mut stranger = remote(&d_alice(), &number, &(T0 + 3).to_string(), T0 + 3, "");
    stranger.body = lost();
    let script = Script::new("restore", vec![uid(ME), uid(ME_PNI)])
        .deliver(setup())
        .deliver(vec![
            BackendEvent::Message(theirs),
            BackendEvent::Message(by_number),
            BackendEvent::Message(stranger),
        ])
        .with_backfill(BackfillScript::Silent);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    env.wait_finished(account).await;
    let deadline = tokio::time::Instant::now() + WAIT * 3;
    while !all_rows(&env)
        .iter()
        .all(|r| r.2 == AttachmentState::Expired)
        || all_rows(&env).len() < 3
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "both lose their file"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let rows = all_rows(&env);
    let in_chat = |key: &str| {
        let (id, ..) = rows.iter().find(|r| r.1 == key).expect("attachment");
        *id
    };
    let (alice_att, number_att) = (in_chat(&d_alice().encode()), in_chat(&chat.encode()));
    assert_eq!(
        asks(&env, &d_alice().encode()).await,
        vec![alice_att],
        "not the one by a number-only author in the same chat"
    );
    assert!(
        asks(&env, &chat.encode()).await.is_empty(),
        "a number-only chat is read-only"
    );
    // The click on the number-only chat's file is refused; Alice's asks and is then not offered.
    env.handle
        .send(UiCommand::DownloadAttachment {
            attachment: number_att,
        })
        .expect("send");
    env.handle
        .send(UiCommand::DownloadAttachment {
            attachment: alice_att,
        })
        .expect("send");
    let deadline = tokio::time::Instant::now() + WAIT * 3;
    while !all_rows(&env)
        .iter()
        .any(|r| r.0 == alice_att && r.2 == AttachmentState::Asking)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Alice's file is asked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(asks(&env, &d_alice().encode()).await.is_empty());
    let number_state = all_rows(&env)
        .into_iter()
        .find(|r| r.0 == number_att)
        .expect("row")
        .2;
    assert_eq!(number_state, AttachmentState::Expired, "nothing was asked");
    let asked: i64 = env.peek(|c| {
        c.query_row("SELECT count(*) FROM attachment_requests", [], |r| r.get(0))
            .expect("count")
    });
    assert_eq!(asked, 1, "one request, for Alice's message");
}
