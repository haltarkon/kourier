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
//! [`an_upload_reports_each_file_and_its_end`]: an album of two pictures and a 3 MB file, each
//! its own message, upload through the real core and the Signal backend: the UI hears the
//! progress of EVERY attachment by its id (the core maps the backend's position in the request to
//! the row), each rising and ending full, and nothing for an attachment after its message is sent.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

use crate::support;

use std::time::Duration;

use kourier_core::facade::{MessageRow, Progress, UiCommand, UiEvent};
use kourier_model::{
    Attachment, AttachmentId, AttachmentState, Body, ConversationId, MessageId, OutgoingState,
};
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

/// Every attachment id of the own messages with files in `rows`: (message, ids in order).
fn own_files(rows: &[MessageRow]) -> Vec<(MessageId, Vec<AttachmentId>)> {
    rows.iter()
        .filter(|row| row.author.is_none())
        .filter_map(|row| {
            let ids: Vec<AttachmentId> = attachments_of(&row.body)
                .iter()
                .filter_map(|a| a.id)
                .collect();
            (!ids.is_empty()).then_some((row.id, ids))
        })
        .collect()
}

/// An upload reports each file by its attachment id (not by its position in the request), the
/// reports of a file only rise and end full, and nothing is reported for a file once its
/// message is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upload_reports_each_file_and_its_end() {
    init_tracing();
    let Some(world) = World::start().await else {
        return;
    };
    let media = tempfile::tempdir().expect("media dir");
    let network = SignalNetwork::new();
    let mut app = App::start_tuned(world.tmp.path(), &world.env, network, media.path()).await;
    let (account, device) = app.link(&world).await;
    let conv = app.accept(&world, account, &device, &world.alice).await;

    // Two pictures (one album message) and a 3 MB file (its own message).
    let picked = tempfile::tempdir().expect("picked dir");
    let mut paths = Vec::new();
    for (name, seed) in [("one.png", 11), ("two.png", 12)] {
        let png = kourier_media::sample_image(900, 700, seed, kourier_media::SampleFormat::Png)
            .expect("picture");
        let path = picked.path().join(name);
        std::fs::write(&path, png).expect("write the picture");
        paths.push(path);
    }
    let file = noise(3_000_000, 5);
    let file_path = picked.path().join("survey.bin");
    std::fs::write(&file_path, &file).expect("write the file");
    paths.push(file_path);
    app.send(UiCommand::SendFiles {
        request: Default::default(),
        as_file: Vec::new(),
        with_location: Vec::new(),
        conv,
        paths: paths.into(),
        caption: None,
        reply_to: None,
    });

    // The reports, until the uploads of both messages are over.
    let mut reports: Vec<(MessageId, AttachmentId, u64, u64)> = Vec::new();
    let mut over = std::collections::BTreeSet::new();
    app.event("the uploads of both messages", |e| {
        match e {
            UiEvent::Progress(Progress::Upload {
                message,
                attachment,
                done,
                total,
            }) => reports.push((*message, *attachment, *done, *total)),
            UiEvent::Progress(Progress::Transmit {
                message, progress, ..
            }) if *progress >= 1.0 => {
                over.insert(*message);
            }
            _ => {}
        }
        (over.len() == 2).then_some(())
    })
    .await;
    let sent = |rows: &[MessageRow]| {
        let own: Vec<&MessageRow> = rows
            .iter()
            .filter(|r| r.author.is_none() && !attachments_of(&r.body).is_empty())
            .collect();
        own.len() == 2
            && own
                .iter()
                .all(|r| matches!(r.out_state, Some(OutgoingState::Sent { .. })))
    };
    let deadline = tokio::time::Instant::now() + WAIT;
    while !sent(&app.rows(conv).await) {
        assert!(tokio::time::Instant::now() < deadline, "both messages sent");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let messages = own_files(&app.rows(conv).await);
    assert_eq!(messages.len(), 2, "an album and a file: {messages:?}");

    // Every attachment of both messages was reported, by its id, and only those.
    let mut want: Vec<AttachmentId> = messages.iter().flat_map(|(_, ids)| ids.clone()).collect();
    want.sort();
    assert_eq!(want.len(), 3, "two pictures and a file: {messages:?}");
    let mut heard: Vec<AttachmentId> = reports.iter().map(|r| r.1).collect();
    heard.sort();
    heard.dedup();
    assert_eq!(
        heard, want,
        "the ids of the rows, not positions: {reports:?}"
    );
    for id in &want {
        let own: Vec<(u64, u64)> = reports
            .iter()
            .filter(|r| r.1 == *id)
            .map(|r| (r.2, r.3))
            .collect();
        let owner = messages
            .iter()
            .find(|(_, ids)| ids.contains(id))
            .map(|(m, _)| *m);
        assert!(
            reports
                .iter()
                .filter(|r| r.1 == *id)
                .all(|r| Some(r.0) == owner),
            "reported under its own message: {reports:?}"
        );
        assert!(own.windows(2).all(|w| w[0].0 <= w[1].0), "{id:?}: {own:?}");
        // (A quick upload on this machine is told once, full, when the uploads are over.)
        assert!(
            own.iter().all(|(_, total)| *total == own[0].1),
            "the total is final from the first report: {id:?}: {own:?}"
        );
        assert!(
            own.last().is_some_and(|(done, total)| done == total),
            "ends full: {id:?}: {own:?}"
        );
    }
    let file_id = messages
        .iter()
        .find(|(_, ids)| ids.len() == 1)
        .map(|(_, ids)| ids[0])
        .expect("the file");
    assert!(
        reports
            .iter()
            .any(|r| r.1 == file_id && r.3 == file.len() as u64),
        "the file's total is its size: {reports:?}"
    );

    // Nothing is reported for a file after its message was sent.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut late = Vec::new();
    while let Ok(event) = app.events.try_recv() {
        if let UiEvent::Progress(Progress::Upload { attachment, .. }) = event {
            late.push(attachment);
        }
    }
    assert!(late.is_empty(), "reports after the send: {late:?}");

    app.stop().await;
    world.shutdown().await;
}
