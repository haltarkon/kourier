//! `SendFiles` with a very large selection (architecture §4.8 step 1, §5.8), through the real
//! core on the fake backend: thousands of files in one go.
//!
//! - the import streams: files are copied one after another, and what the core keeps per
//!   file is a few hundred bytes of bookkeeping, so memory does not follow the selection;
//! - the writer is not held for the whole send: the messages are stored in transactions of
//!   [`kourier_core::ENQUEUE_CHUNK`], and a write of another chat waits for one of them at
//!   most, not for all;
//! - other chats are not delayed: a text sent in another chat right behind the files goes
//!   out while the files are still being copied;
//! - the chat itself keeps its order: a text typed behind the files is sent behind them;
//! - sending the stored messages does not slow down with the number that waits.
//!
//! The numbers are printed (`--nocapture`); `KOURIER_SCALE_FILES` sets the selection's size
//! (1,500 by default) and `TMPDIR` where the profile lives (a tmpfs has no fsync cost).

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use kourier_core::facade::{Progress, RequestId, UiCommand, UiEvent};
use kourier_fake::{Flavor, Script};
use kourier_model::{AccountId, AccountUuid, ConvKey, ConversationId, OutgoingState, Text};
use support::send::*;
use support::{Env, add_linked, eventually};

const LONG: Duration = Duration::from_secs(600);

async fn linked(script: Script) -> (Env, AccountId, AccountUuid) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    (env, account, uuid)
}

async fn conv(env: &Env, key: &ConvKey) -> ConversationId {
    let (db, k) = (env.core_db(), key.clone());
    eventually("the conversation", LONG, || {
        let (db, k) = (db.clone(), k.clone());
        async move { conv_id(&db, &k).is_some() }
    })
    .await;
    conv_id(&env.core_db(), key).expect("conversation")
}

/// A field of `/proc/self/status` in kB (`VmRSS`, `VmHWM`).
fn status_kb(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn count(env: &Env, sql: &str) -> i64 {
    core_db(&env.core_db(), |c| {
        c.query_row(sql, [], |r| r.get(0)).expect(sql)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_of_thousands_of_files_holds_up_nothing_else() {
    let files: usize = std::env::var("KOURIER_SCALE_FILES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_500);
    let (mut env, account, uuid) = linked(script("scale")).await;
    env.wait_finished(account).await;
    let (a, b) = (conv(&env, &d_alice()).await, conv(&env, &d_bob()).await);
    // The user's files: small documents (each travels alone on the fake network, so the
    // send becomes as many messages as it has files).
    let src = env.dir.path().join("src");
    std::fs::create_dir_all(&src).expect("dir");
    let paths: Vec<PathBuf> = (0..files)
        .map(|i| {
            let p = src.join(format!("doc-{i:05}.txt"));
            std::fs::write(&p, format!("document number {i}, a few bytes of text")).expect("write");
            p
        })
        .collect();
    let (rss_before, peak_before) = (status_kb("VmRSS:"), status_kb("VmHWM:"));

    let request = RequestId(7);
    let started = Instant::now();
    env.handle
        .send(UiCommand::SendFiles {
            conv: a,
            request,
            paths: paths.into(),
            as_file: Vec::new(),
            caption: None,
            reply_to: None,
        })
        .expect("send");
    // Right behind it: a text in another chat, and one in the same chat.
    let text = |conv, body: &str| UiCommand::SendText {
        conv,
        text: Text::plain(body),
        reply_to: None,
    };
    env.handle
        .send(text(b, "meanwhile, elsewhere"))
        .expect("send");
    env.handle.send(text(a, "behind the files")).expect("send");

    // The other chat's text is sent while the files are still being copied.
    let control = env.control.clone();
    let elsewhere = tokio::spawn(async move {
        control
            .wait_for(|c| c.transmits(uuid).iter().any(|t| t.req.conv == d_bob()))
            .await;
        Instant::now()
    });

    // Follow the send: the copy, then the storing. While it is stored, another chat keeps
    // writing (drafts), one write after the other, and each write is timed.
    let mut copied: Option<Instant> = None;
    let mut probes: Vec<Duration> = Vec::new();
    let mut probe: Option<Instant> = None;
    let mut seen_progress = 0usize;
    let (stored, messages) = loop {
        let event = tokio::time::timeout(LONG, env.events.recv())
            .await
            .expect("the send goes on")
            .expect("events");
        match event {
            UiEvent::Progress(Progress::Import {
                request: r,
                files_done,
                files: total,
                ..
            }) if r == request => {
                seen_progress += 1;
                assert_eq!(total as usize, files);
                if files_done == total && copied.is_none() {
                    copied = Some(Instant::now());
                    probe = Some(Instant::now());
                    env.handle
                        .send(UiCommand::SaveDraft {
                            conv: b,
                            draft: Some(Text::plain("typing in the other chat")),
                        })
                        .expect("send");
                }
            }
            UiEvent::ConversationsChanged { ids } if ids == [b] && copied.is_some() => {
                if let Some(sent) = probe.take() {
                    probes.push(sent.elapsed());
                    probe = Some(Instant::now());
                    env.handle
                        .send(UiCommand::SaveDraft {
                            conv: b,
                            draft: Some(Text::plain(format!("draft {}", probes.len()))),
                        })
                        .expect("send");
                }
            }
            UiEvent::FilesStored {
                request: r,
                messages: m,
                ..
            } if r == request => break (Instant::now(), m),
            UiEvent::ActionFailed { action, error, .. } => panic!("{action} failed: {error}"),
            _ => {}
        }
    };
    let copied = copied.expect("the copy reported its end");
    let (copy, store) = (copied - started, stored - copied);
    let elsewhere = tokio::time::timeout(LONG, elsewhere)
        .await
        .expect("the other chat's text is sent")
        .expect("join");
    let (rss_stored, peak_stored) = (status_kb("VmRSS:"), status_kb("VmHWM:"));

    assert_eq!(messages.len(), files, "one message per document");
    assert!(seen_progress >= 2, "the copy reported its progress");
    assert_eq!(
        count(&env, "SELECT count(*) FROM outbox WHERE state = 'Staged'"),
        0
    );

    // Other chats were not delayed behind the send.
    assert!(
        elsewhere < stored,
        "the other chat's text went out {:?} after the files were stored",
        elsewhere.duration_since(stored)
    );
    // The writer was never held for the whole send: a write of another chat waited for a
    // part of it at most. (One transaction for everything would make the first of these
    // writes wait for about the whole storing time.)
    let slowest = probes.iter().copied().max().unwrap_or_default();
    let median = {
        let mut sorted = probes.clone();
        sorted.sort();
        sorted.get(sorted.len() / 2).copied().unwrap_or_default()
    };
    let parts = files.div_ceil(kourier_core::ENQUEUE_CHUNK);
    if files >= 500 {
        assert!(
            probes.len() >= 3,
            "writes of another chat got through while the send was stored: {probes:?}"
        );
        // (The median: one write can wait for a slow disk, on any design.)
        assert!(
            median < store / 3,
            "a write waited {median:?} of {store:?}: {probes:?}"
        );
    }

    // Everything is sent, in the user's order, the chat's own text last.
    let db = env.core_db();
    eventually("everything is sent", LONG, || {
        let db = db.clone();
        async move { outbox_len(&db) == 0 }
    })
    .await;
    let sent = Instant::now() - stored;
    let own = own_messages(&db, a);
    assert_eq!(own.len(), files + 1);
    assert!(
        own.iter()
            .all(|m| matches!(m.out_state, Some(OutgoingState::Sent { .. })))
    );
    assert!(own.windows(2).all(|w| w[0].sent_at < w[1].sent_at));
    let ids: Vec<_> = own.iter().map(|m| m.id).collect();
    assert_eq!(
        &ids[..files],
        messages.as_slice(),
        "in the order of the send"
    );
    let to_alice: Vec<String> = env
        .control
        .transmits(uuid)
        .iter()
        .filter(|t| t.req.conv == d_alice())
        .map(|t| t.req.local.to_string())
        .collect();
    let stored_ids: Vec<String> = own.iter().map(|m| m.local.clone()).collect();
    assert_eq!(to_alice, stored_ids, "each once, the text behind the files");

    let (rss_end, peak_end) = (status_kb("VmRSS:"), status_kb("VmHWM:"));
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / files as f64;
    eprintln!(
        "scale: {files} files in {parts} transactions\n\
         copy    {copy:?} ({:.3} ms per file)\n\
         store   {store:?} ({:.3} ms per message)\n\
         writes of another chat while it was stored: {} probes, median {:?}, slowest {slowest:?}\n\
         the other chat's text was sent {:?} after the command ({:?} before the files were stored)\n\
         send    {sent:?} ({:.0} messages/s on the fake network)\n\
         memory  RSS {rss_before} kB before, {rss_stored} kB stored, {rss_end} kB sent; \
         peak {peak_before} kB before, {peak_stored} kB stored, {peak_end} kB sent",
        per(copy),
        per(store),
        probes.len(),
        median,
        elsewhere - started,
        stored.saturating_duration_since(elsewhere),
        files as f64 / sent.as_secs_f64().max(0.001),
    );
    // Memory does not follow the selection's content, only its count: well under 20 kB
    // per file of bookkeeping (measured: about 2 kB), whatever the files hold.
    let grew = peak_stored.saturating_sub(peak_before);
    assert!(
        grew < 20 * files as u64 + 16_384,
        "the peak grew by {grew} kB for {files} files"
    );
    env.stop().await;
}

/// A measurement, not a check (`--ignored --nocapture`): a history of texts with 8 % pictures
/// is delivered by the fake network and taken in by the real core, as when the reference
/// profile of `cargo xtask perf` is seeded. `KOURIER_SCALE_MESSAGES` sets the number (3,000
/// by default); with `KOURIER_SCALE_NO_DOWNLOADS=1` the chat is an unanswered message
/// request, from which nothing is fetched, so the same rows are stored without any download
/// or thumbnail: the difference between the two runs is what the files cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run with --ignored --nocapture"]
async fn measure_catch_up_with_media() {
    use kourier_backend_api::BackendEvent;
    use kourier_fake::attachments::synthetic_image;
    use kourier_fake::scripts::{T0, remote};
    use kourier_model::{Body, ConversationPatch};

    let messages: i64 = std::env::var("KOURIER_SCALE_MESSAGES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3_000);
    let no_downloads = std::env::var("KOURIER_SCALE_NO_DOWNLOADS").is_ok();
    let mut s = script("catch-up-media");
    if no_downloads {
        let mut pending = ConversationPatch::new(d_alice());
        pending.request_pending = Some(true);
        s = s.deliver(vec![BackendEvent::ConversationUpsert(pending)]);
    }
    let mut pictures = 0i64;
    // Deliveries of 25 messages, as a backend's stage 2 hands them on.
    for batch in 0..messages / 25 {
        let events = (0..25)
            .map(|i| {
                let n = batch * 25 + i + 1;
                let mut m = remote(&d_alice(), &alice(), &(T0 + n).to_string(), T0 + n, "hello");
                if n % 12 == 0 {
                    pictures += 1;
                    m.body = Body::Media {
                        attachments: vec![synthetic_image(640, 480, u64::try_from(n).unwrap_or(1))],
                        caption: None,
                    };
                }
                BackendEvent::Message(m)
            })
            .collect();
        s = s.deliver(events);
    }
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let started = Instant::now();
    let account = add_linked(&mut env).await;
    let control = env.control.clone();
    let uuid = env.account(account).await.expect("account").uuid;
    tokio::time::timeout(LONG, control.wait_for(|c| c.is_finished(uuid)))
        .await
        .expect("script finished");
    let ingested = started.elapsed();
    // Then until every picture is here with its thumbnail.
    let db = env.core_db();
    if !no_downloads {
        eventually("every picture is derived", LONG, || {
            let db = db.clone();
            async move {
                core_db(&db, |c| {
                    c.query_row(
                        "SELECT count(*) FROM attachments WHERE derived_at IS NULL",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .is_ok_and(|n| n == 0)
                })
            }
        })
        .await;
    }
    let done = started.elapsed();
    let rate = |d: Duration| messages as f64 / d.as_secs_f64();
    eprintln!(
        "catch-up: {messages} messages, {pictures} pictures, downloads {}\n         delivered and committed in {ingested:?} ({:.0} messages/s)\n         every picture downloaded and thumbnailed after {done:?} ({:.0} messages/s)",
        if no_downloads { "off" } else { "on" },
        rate(ingested),
        rate(done),
    );
    env.stop().await;
}

/// A measurement, not a check (`--ignored --nocapture`, `TMPDIR` on a real disk): what the
/// import's way of making files durable costs, against a sync after every file. Both ways
/// are run in turns, so a busy disk hurts both alike. `KOURIER_SCALE_FILES` sets the number
/// of files per round (1,000 by default).
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_durable_import() {
    let files: usize = std::env::var("KOURIER_SCALE_FILES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let store = kourier_blob::BlobStore::open(&dir.path().join("core")).expect("store");
    let content = b"document, a few bytes of text".to_vec();
    let (mut each, mut batched) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        // A sync of the file and of its directory after every file.
        let started = Instant::now();
        for _ in 0..files {
            store.import(&mut content.as_slice()).expect("import");
        }
        each.push(started.elapsed());
        // Written without sync, then every file and every directory synced once.
        let started = Instant::now();
        let mut paths = Vec::with_capacity(files);
        for _ in 0..files {
            let (id, _, _) = store
                .import_volatile(&mut content.as_slice())
                .expect("import");
            paths.push(store.path(&id));
        }
        let mut dirs = std::collections::BTreeSet::new();
        for p in &paths {
            std::fs::File::open(p)
                .and_then(|f| f.sync_all())
                .expect("sync");
            dirs.extend(p.parent().map(std::path::Path::to_owned));
        }
        for d in &dirs {
            std::fs::File::open(d)
                .and_then(|f| f.sync_all())
                .expect("sync");
        }
        std::fs::File::open(store.root())
            .and_then(|f| f.sync_all())
            .expect("sync");
        batched.push(started.elapsed());
    }
    let per = |d: &Duration| d.as_secs_f64() * 1000.0 / files as f64;
    eprintln!(
        "durable import of {files} small files, three rounds each\n\
         a sync after every file: {:?} ({:.2} / {:.2} / {:.2} ms per file)\n\
         synced as one batch:     {:?} ({:.2} / {:.2} / {:.2} ms per file)",
        each,
        per(&each[0]),
        per(&each[1]),
        per(&each[2]),
        batched,
        per(&batched[0]),
        per(&batched[1]),
        per(&batched[2]),
    );
}
