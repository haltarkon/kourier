//! S6 through the real core (docs/architecture.md §6.11 "Asking the phone"): the app's core
//! with the Signal backend against `@signalapp/mock-server`, driven through the facade as the
//! UI drives it, the harness playing our phone ("Myself") and a contact ("Alice").
//!
//! [`a_lost_file_is_asked_of_the_phone_and_arrives_byte_identical`]: Alice's picture is gone
//! from the CDN (its pointer names an object nobody stored), so the automatic download ends
//! "No longer available" and asks nothing. A click on Download asks the phone (one request,
//! naming the chat, the author and the message's time); the attachment says it is asking; the
//! phone answers "still uploading" first (the wait goes on) and then with the file it uploaded
//! again; the core downloads it from the phone's new pointer and it opens byte-identical.
//!
//! [`a_phone_that_does_not_answer_ends_as_no_answer_and_is_asked_again_on_a_click`]: a phone
//! that says nothing ends the wait as "did not answer" after the shortened wait (a note the
//! user can act on), and the next click asks again.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::time::Duration;

use kourier_core::facade::{UiCommand, UiEvent};
use kourier_model::{AskFailure, Attachment, AttachmentId, AttachmentState, Body, ConversationId};
use serde_json::json;
use support::harness::{
    BackfillAnswer, BackfillEntry, ForgedPointer, OutgoingFile, SendAttachments,
};
use support::{App, WAIT, World, init_tracing};

/// Deterministic bytes that occur in no other test input.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn attachments_of(body: &Body) -> Vec<Attachment> {
    match body {
        Body::Media { attachments, .. } => attachments.clone(),
        _ => Vec::new(),
    }
}

fn caption_is(body: &Body, text: &str) -> bool {
    matches!(body, Body::Media { caption: Some(c), .. } if c.body == text)
}

/// The attachment of the row captioned `caption`, once it is in a state `done` accepts.
async fn wait_state(
    app: &App,
    conv: ConversationId,
    caption: &str,
    what: &str,
    done: impl Fn(&AttachmentState) -> bool,
) -> Attachment {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows(conv).await;
        let found = rows
            .iter()
            .find(|row| caption_is(&row.body, caption))
            .and_then(|row| attachments_of(&row.body).into_iter().next());
        if let Some(attachment) = found.as_ref().filter(|a| done(&a.state)) {
            return attachment.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: {:?}",
            found.map(|a| a.state)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The plaintext of a downloaded attachment, through the facade's `OpenMedia`.
async fn opened(app: &mut App, attachment: AttachmentId) -> Vec<u8> {
    app.send(UiCommand::OpenMedia { attachment });
    let path = app
        .event("MediaOpened", |e| match e {
            UiEvent::MediaOpened {
                attachment: a,
                path,
            } if *a == attachment => Some(path.clone()),
            UiEvent::ActionFailed { .. } => panic!("OpenMedia failed: {e:?}"),
            _ => None,
        })
        .await;
    let bytes = std::fs::read(&path).expect("media file");
    app.send(UiCommand::CloseMedia { attachment });
    bytes
}

/// Alice's picture whose pointer names an object the CDN never had.
fn lost_picture(
    world: &World,
    me: &str,
    caption: &str,
    key: &str,
    bytes: &[u8],
) -> SendAttachments {
    SendAttachments {
        from: world.alice.aci.clone(),
        to: me.to_owned(),
        attachments: vec![OutgoingFile {
            forge: Some(ForgedPointer {
                cdn_key: Some(key.to_owned()),
                ..ForgedPointer::default()
            }),
            ..OutgoingFile::new(bytes, "image/png")
        }],
        text: Some(caption.to_owned()),
        await_delivery: true,
        timeout: Some(60_000),
        ..SendAttachments::default()
    }
}

/// The answer of our phone about Alice's message `at`.
fn answer(world: &World, at: u64, entry: BackfillEntry) -> BackfillAnswer {
    BackfillAnswer {
        conversation: json!({ "aci": world.alice.aci }),
        author: world.alice.aci.clone(),
        timestamp: at,
        attachments: Some(vec![entry]),
        ..BackfillAnswer::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_file_is_asked_of_the_phone_and_arrives_byte_identical() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    let media = tempfile::tempdir().expect("media dir");
    let mut app = App::start_with_media(world.tmp.path(), &world.env, media.path()).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let conv = app.accept(&world, account, &device, &world.alice).await;

    let picture = noise(50_000, 21);
    let caption = "gone from the server";
    let at = world
        .harness
        .send_attachments(&lost_picture(
            &world,
            &me,
            caption,
            "lostbytheserver01",
            &picture,
        ))
        .await
        .expect("Alice's picture")
        .timestamp;
    // The server has lost it: "No longer available", and the core asked nobody.
    let gone = wait_state(&app, conv, caption, "the file is gone", |s| {
        *s == AttachmentState::Expired
    })
    .await;
    assert!(
        world
            .harness
            .wait_for_backfill_request(
                &world.phone.device_ref,
                Some(&device.device_ref),
                Duration::from_millis(1500)
            )
            .await
            .is_err(),
        "an automatic download that finds the file gone asks nothing"
    );
    let id = gone.id.expect("stored attachment");

    // A click on Download asks the phone: one request for the message.
    app.send(UiCommand::DownloadAttachment { attachment: id });
    let request = world
        .harness
        .wait_for_backfill_request(&world.phone.device_ref, Some(&device.device_ref), WAIT)
        .await
        .expect("the request reached the phone");
    assert_eq!(request.sent_timestamp, Some(at), "{}", request.raw);
    wait_state(&app, conv, caption, "asking your phone", |s| {
        *s == AttachmentState::Asking
    })
    .await;

    // The phone is still uploading: the wait goes on; then the file comes.
    world
        .harness
        .send_backfill_answer(
            &world.phone.device_ref,
            &device.device_ref,
            &answer(&world, at, BackfillEntry::pending()),
            true,
        )
        .await
        .expect("PENDING");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still = wait_state(&app, conv, caption, "still asking", |_| true).await;
    assert_eq!(
        still.state,
        AttachmentState::Asking,
        "PENDING keeps the wait"
    );
    world
        .harness
        .send_backfill_answer(
            &world.phone.device_ref,
            &device.device_ref,
            &answer(
                &world,
                at,
                BackfillEntry::upload(&picture, "image/png", None),
            ),
            true,
        )
        .await
        .expect("the file");
    let here = wait_state(&app, conv, caption, "the phone's file", |s| {
        *s == AttachmentState::Local
    })
    .await;
    assert_eq!(opened(&mut app, here.id.expect("id")).await, picture);
    assert!(
        world
            .harness
            .wait_for_backfill_request(
                &world.phone.device_ref,
                Some(&device.device_ref),
                Duration::from_millis(1500)
            )
            .await
            .is_err(),
        "one request in all"
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_that_does_not_answer_ends_as_no_answer_and_is_asked_again_on_a_click() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    let media = tempfile::tempdir().expect("media dir");
    let mut app = App::start_with_restore_waits(
        world.tmp.path(),
        &world.env,
        media.path(),
        Duration::from_secs(3),
        Duration::from_secs(3),
    )
    .await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let conv = app.accept(&world, account, &device, &world.alice).await;

    let caption = "the phone is away";
    world
        .harness
        .send_attachments(&lost_picture(
            &world,
            &me,
            caption,
            "lostbytheserver02",
            &noise(40_000, 22),
        ))
        .await
        .expect("Alice's picture");
    let gone = wait_state(&app, conv, caption, "the file is gone", |s| {
        *s == AttachmentState::Expired
    })
    .await;
    let id = gone.id.expect("stored attachment");

    for round in 1..=2 {
        app.send(UiCommand::DownloadAttachment { attachment: id });
        world
            .harness
            .wait_for_backfill_request(&world.phone.device_ref, Some(&device.device_ref), WAIT)
            .await
            .unwrap_or_else(|e| panic!("request {round}: {e:?}"));
        // Nobody answers: the wait ends as a note the user can act on.
        let ended = wait_state(&app, conv, caption, "no answer", |s| {
            matches!(
                s,
                AttachmentState::AskFailed {
                    why: AskFailure::NoAnswer
                }
            )
        })
        .await;
        assert_eq!(
            ended.state,
            AttachmentState::AskFailed {
                why: AskFailure::NoAnswer
            },
            "round {round}"
        );
    }
    app.stop().await;
    world.shutdown().await;
}
