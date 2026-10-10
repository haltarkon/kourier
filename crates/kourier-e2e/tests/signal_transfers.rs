//! Transfers that do not go smoothly, through the real core (docs/architecture.md §5.8, §6.11):
//! the app's core with the Signal backend against `@signalapp/mock-server`, whose CDN 3 the
//! harness serves with ranges and breaks on request (`breakDownloads`).
//!
//! [`a_download_that_breaks_off_goes_on_and_reports_its_progress`]:
//!
//! - A file from Alice whose transfer is reset part way, then once more before a single
//!   further byte, arrives byte-identical: the backend asks the CDN for the rest from the byte
//!   it stopped at (`Range`), the UI is told how far the download is while it waits, and the
//!   blob is verified as a whole at the end.
//! - The same against a CDN that knows no ranges and answers a range request with the whole
//!   blob: the part Kourier already has is skipped, the file is the same.
//! - A transfer that gets no further (five requests in a row without a byte) fails as the
//!   network failure it is: the attachment offers "try again", and a new download, from the
//!   first byte, brings the file.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::time::Duration;

use kourier_core::facade::{MessageRow, Progress, UiCommand, UiEvent};
use kourier_model::{Attachment, AttachmentId, AttachmentState, Body, ConversationId};
use kourier_signal::SignalNetwork;
use serde_json::{Value, json};
use support::harness::{OutgoingFile, SendAttachments};
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

/// The row whose caption is `caption`, once its attachment is in `state`.
async fn wait_file(
    app: &App,
    conv: ConversationId,
    caption: &str,
    done: impl Fn(&AttachmentState) -> bool,
) -> (MessageRow, Attachment) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows(conv).await;
        let found = rows.iter().find(
            |row| matches!(&row.body, Body::Media { caption: Some(c), .. } if c.body == caption),
        );
        if let Some(row) = found
            && let Some(attachment) = attachments_of(&row.body).into_iter().next()
            && done(&attachment.state)
        {
            return (row.clone(), attachment);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{caption}: {:?}",
            found.map(|row| attachments_of(&row.body))
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

/// The downloads the CDN was asked for since the last call: `(range header, status, start)`.
async fn downloads(world: &World) -> Vec<(Option<String>, u64, u64)> {
    let stats = world
        .harness
        .request_value("downloadStats", json!({ "reset": true }))
        .await
        .expect("downloadStats");
    stats["downloads"]
        .as_array()
        .expect("downloads")
        .iter()
        .map(|d| {
            (
                d["range"].as_str().map(str::to_owned),
                d["status"].as_u64().expect("status"),
                d["start"].as_u64().expect("start"),
            )
        })
        .collect()
}

/// Every `Progress::Download` of `attachment` the UI was told so far: `(done, total)`.
fn download_progress(app: &mut App, attachment: AttachmentId) -> Vec<(u64, u64)> {
    let mut seen = Vec::new();
    while let Ok(event) = app.events.try_recv() {
        if let UiEvent::Progress(Progress::Download {
            attachment: a,
            done,
            total,
        }) = event
            && a == attachment
        {
            seen.push((done, total));
        }
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_that_breaks_off_goes_on_and_reports_its_progress() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    let media = tempfile::tempdir().expect("media dir");
    // A request that brought nothing is asked again after 700 ms instead of a second: long
    // enough for the progress ticker (250 ms) to report the part that arrived.
    let network = SignalNetwork::new().with_download_resume_pause(Duration::from_millis(700));
    let mut app = App::start_tuned(world.tmp.path(), &world.env, network, media.path()).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let from_alice = |bytes: &[u8], caption: &str| SendAttachments {
        from: world.alice.aci.clone(),
        to: me.clone(),
        attachments: vec![OutgoingFile {
            file_name: Some("survey.bin".to_owned()),
            ..OutgoingFile::new(bytes, "application/octet-stream")
        }],
        text: Some(caption.to_owned()),
        await_delivery: true,
        timeout: Some(60_000),
        ..SendAttachments::default()
    };
    let break_at = |args: Value| async {
        world
            .harness
            .request_value("breakDownloads", args)
            .await
            .expect("breakDownloads");
    };
    let _ = downloads(&world).await;

    // --- The transfer is reset after 300,000 bytes, then again before any further byte.
    let file = noise(900_000, 41);
    break_at(json!({ "afters": [300_000, 0] })).await;
    world
        .harness
        .send_attachments(&from_alice(&file, "first survey"))
        .await
        .expect("file");
    let (_, attachment) = wait_file(&app, conv, "first survey", |state| {
        *state == AttachmentState::Local
    })
    .await;
    let id = attachment.id.expect("stored attachment");
    assert_eq!(attachment.size, file.len() as u64);
    // (Read before anything else takes events from the queue.)
    let progress = download_progress(&mut app, id);
    assert_eq!(
        opened(&mut app, id).await,
        file,
        "the file arrived whole, byte for byte"
    );
    let asked = downloads(&world).await;
    assert_eq!(
        asked.len(),
        3,
        "the first request and two that go on: {asked:?}"
    );
    assert_eq!((asked[0].0.as_deref(), asked[0].1), (None, 200));
    let resumed_at = asked[1].2;
    assert!(
        resumed_at > 0 && resumed_at <= 300_000,
        "the rest from where the transfer stopped: {asked:?}"
    );
    assert_eq!(
        asked[1],
        (Some(format!("bytes={resumed_at}-")), 206, resumed_at)
    );
    assert_eq!(
        asked[2], asked[1],
        "nothing came with the second request: the same range again"
    );
    // The UI was told the real progress: nothing, then the part that had arrived when the
    // transfer hung, then all of it; never backwards.
    let total = file.len() as u64;
    assert_eq!(progress.first(), Some(&(0, total)), "{progress:?}");
    assert_eq!(progress.last(), Some(&(total, total)), "{progress:?}");
    assert!(
        progress.windows(2).all(|pair| pair[0].0 <= pair[1].0),
        "{progress:?}"
    );
    assert!(
        progress
            .iter()
            .any(|(done, _)| *done > 0 && *done <= resumed_at),
        "a share between nothing and everything, while the download waited: {progress:?}"
    );

    // --- A CDN that knows no ranges: it answers the range request with the whole blob, of
    // which the part Kourier has is skipped.
    let file = noise(400_000, 42);
    break_at(json!({ "afters": [150_000], "ignoreRange": true })).await;
    world
        .harness
        .send_attachments(&from_alice(&file, "second survey"))
        .await
        .expect("file");
    let (_, attachment) = wait_file(&app, conv, "second survey", |state| {
        *state == AttachmentState::Local
    })
    .await;
    assert_eq!(
        opened(&mut app, attachment.id.expect("stored")).await,
        file,
        "the same file from a CDN without ranges"
    );
    let asked = downloads(&world).await;
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(
        asked[1]
            .0
            .as_deref()
            .is_some_and(|r| r.starts_with("bytes="))
    );
    assert_eq!((asked[1].1, asked[1].2), (200, 0), "answered whole");
    break_at(json!({ "stop": true })).await;

    // --- A transfer that gets no further: after five requests in a row without a byte it
    // fails as a network failure, the attachment can be asked for again, and the new
    // download starts from the first byte.
    let file = noise(500_000, 43);
    break_at(json!({ "afters": [200_000, 0, 0, 0, 0, 0] })).await;
    world
        .harness
        .send_attachments(&from_alice(&file, "third survey"))
        .await
        .expect("file");
    let (_, attachment) = wait_file(&app, conv, "third survey", |state| {
        matches!(state, AttachmentState::Failed { .. })
    })
    .await;
    assert_eq!(
        attachment.state,
        AttachmentState::Failed { retryable: true },
        "the network's failure, not the file's"
    );
    let asked = downloads(&world).await;
    assert_eq!(asked.len(), 6, "{asked:?}");
    assert!(
        asked[1..]
            .iter()
            .all(|d| d.1 == 206 && d.2 == asked[1].2 && d.2 > 0),
        "five requests for the same rest: {asked:?}"
    );
    break_at(json!({ "stop": true })).await;
    let id = attachment.id.expect("stored");
    app.send(UiCommand::DownloadAttachment { attachment: id });
    let (_, attachment) = wait_file(&app, conv, "third survey", |state| {
        *state == AttachmentState::Local
    })
    .await;
    assert_eq!(opened(&mut app, attachment.id.expect("stored")).await, file);
    assert_eq!(
        downloads(&world).await,
        [(None, 200, 0)],
        "the new attempt starts from the first byte"
    );

    app.stop().await;
    world.shutdown().await;
}
