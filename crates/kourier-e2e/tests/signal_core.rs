//! The real `kourier-core` runtime with the real Signal backend against
//! `@signalapp/mock-server` (docs/architecture.md §10.1, §10.3, §11 S6): everything the UI does
//! goes through the facade (`UiCommand`, `UiEvent`, queries), the core supervises the
//! `SignalNetwork` actor with a `DirVault` test vault, and the harness plays the phones.
//!
//! - [`link_receive_and_restart`]: `AddAccount(Signal)` → login steps → the QR URL to the phone
//!   (`completeProvision`) → account `Active` and `Online`; a contact's message appears in the
//!   facade's timeline exactly once with the right author; after a core restart it is still
//!   there once, the account reconnects and receives again.
//! - [`reply_through_the_core`]: `UiCommand::SendText` reaches the contact's phone. Until the
//!   core's outbox (S5) lands, `SendText` answers `NotImplemented` and the test skips with a
//!   message.
//! - [`messages_queued_while_offline_are_summarized_once`] (S7): 500 messages queued while
//!   Kourier is offline arrive as catch-up messages: no notification each, one
//!   `CatchUpSummary{500, 1}` when the account leaves `Syncing`, every message once.
//! - [`five_thousand_queued_messages_ingest_at_500_per_second`] (S7): measures the catch-up
//!   ingest from connect to the summary (all 5,000 committed in `core.db`).
//! - [`killing_the_app_at_every_receive_failpoint_gives_the_uncrashed_state`] (S7, §10.4): the
//!   whole app (core and backend) runs in a helper process armed with one failpoint of the
//!   receive path (backend and core ingest); after the abort and a restart, the timeline holds
//!   each message of the stream exactly once, as in the uncrashed run.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::io::Write as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use kourier_backend_api::BackendEnv;
use kourier_core::facade::{
    MessageRow, OriginKind, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_model::{AccountId, Body, ConnState, ConversationId, Text, UserId};
use serde::{Deserialize, Serialize};
use support::harness::{DeviceInfo, SendText, SendTexts};
use support::{App, WAIT, World, init_tracing, user};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::broadcast;

/// Asserts that `rows` is one message from `author` sent at `sent_at`.
fn assert_once_from(rows: &[MessageRow], author: &UserId, sent_at: u64, device: u32) {
    assert_eq!(rows.len(), 1, "exactly once: {rows:?}");
    let row = &rows[0];
    assert_eq!(row.author.as_ref(), Some(author), "author");
    assert_eq!(row.origin, OriginKind::Remote);
    assert_eq!(row.device, Some(device));
    assert_eq!(u64::try_from(row.sent_at).ok(), Some(sent_at));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_receive_and_restart() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = user(&world.alice);

    let text = "hello Kourier, from Alice";
    let sent_at = world.alice_says(&device, text).await;
    let conv = app.conversation_with(account, &alice).await;
    app.wait_text(conv, text).await;
    // Nothing addressed to us is left, so no second copy can still be on its way.
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("deliveries settled");
    assert_once_from(
        &app.rows_with_text(conv, text).await,
        &alice,
        sent_at,
        world.alice.device_id,
    );

    // Restart: the account is still Active, reconnects, and the message is still there once.
    app.stop().await;
    let mut app = App::start(world.tmp.path(), &world.env).await;
    app.event("ConnState Online after the restart", |e| match e {
        UiEvent::ConnState {
            account: a,
            state: ConnState::Online { .. },
        } if *a == account => Some(()),
        _ => None,
    })
    .await;
    app.wait_online(account).await;
    assert_eq!(app.conversation_with(account, &alice).await, conv);
    assert_once_from(
        &app.rows_with_text(conv, text).await,
        &alice,
        sent_at,
        world.alice.device_id,
    );
    let again = "still there after the restart";
    let again_at = world.alice_says(&device, again).await;
    app.wait_text(conv, again).await;
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("deliveries settled");
    assert_once_from(
        &app.rows_with_text(conv, again).await,
        &alice,
        again_at,
        world.alice.device_id,
    );
    assert_once_from(
        &app.rows_with_text(conv, text).await,
        &alice,
        sent_at,
        world.alice.device_id,
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_through_the_core() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = user(&world.alice);
    world.alice_says(&device, "write back").await;
    let conv = app.conversation_with(account, &alice).await;
    // Sends need a chat the phone accepted (§6.7 M1 rule).
    assert_eq!(
        app.accept(&world, account, &device, &world.alice).await,
        conv
    );

    let reply = "a reply through the core";
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain(reply),
        reply_to: None,
    });
    let failed = async {
        app.event("SendText failure", |e| match e {
            UiEvent::ActionFailed {
                action: "SendText",
                error,
                ..
            } => Some(format!("{error:?}")),
            _ => None,
        })
        .await
    };
    tokio::select! {
        error = failed => {
            if error.contains("NotImplemented") {
                // The core's outbox arrives with S5; rerun this test once it has landed.
                let _ = writeln!(
                    std::io::stderr(),
                    "skipping: UiCommand::SendText is not implemented in kourier-core yet ({error})"
                );
                app.stop().await;
                world.shutdown().await;
                return;
            }
            panic!("SendText failed: {error}");
        }
        got = world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT) => {
            let got = got.expect("Alice got the reply");
            assert_eq!(got.text, reply);
            assert_eq!(got.source_device, device.device_id);
        }
    }
    // The own message is in the timeline once.
    let rows = app.wait_text(conv, reply).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].origin, OriginKind::OwnThisDevice);
    app.stop().await;
    world.shutdown().await;
}

/// What the core told the UI from an app start until the catch-up summary (plus `settle`).
struct CatchUp {
    /// `(messages, conversations)` of every `CatchUpSummary`.
    summaries: Vec<(u32, u32)>,
    /// `Notify` events.
    notifications: usize,
    /// From the start until the account was `Syncing` (connected).
    connected: Duration,
    /// From the start until the summary.
    summarized: Duration,
}

/// Starts the app over `world`'s profile and records the catch-up of `account`.
async fn catch_up(world: &World, account: AccountId, settle: Duration) -> (App, CatchUp) {
    let started = tokio::time::Instant::now();
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let mut seen = CatchUp {
        summaries: Vec::new(),
        notifications: 0,
        connected: Duration::ZERO,
        summarized: Duration::ZERO,
    };
    let mut settle_until = None;
    let deadline = started + Duration::from_secs(300);
    loop {
        let wait_until = settle_until.unwrap_or(deadline);
        match tokio::time::timeout_at(wait_until, app.events.recv()).await {
            Ok(Ok(UiEvent::ConnState {
                account: a,
                state: ConnState::Syncing { .. },
            })) if a == account && seen.connected.is_zero() => {
                seen.connected = started.elapsed();
            }
            Ok(Ok(UiEvent::CatchUpSummary {
                account: a,
                messages,
                conversations,
            })) if a == account => {
                if seen.summaries.is_empty() {
                    seen.summarized = started.elapsed();
                    settle_until = Some(tokio::time::Instant::now() + settle);
                }
                seen.summaries.push((messages, conversations));
            }
            Ok(Ok(UiEvent::Notify(_))) => seen.notifications += 1,
            Ok(Ok(_)) => {}
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                panic!("the test lagged {n} UI events; it cannot prove a single summary")
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => panic!("the core stopped"),
            Err(_) if settle_until.is_some() => return (app, seen),
            Err(_) => panic!("no catch-up summary within 300 s"),
        }
    }
}

/// The texts of the conversation's newest `limit` rows, oldest first.
async fn texts(app: &App, conv: ConversationId, limit: u32) -> Vec<String> {
    let query = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit,
    };
    match app
        .handle
        .query(Query::Timeline(query))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(page) => page
            .rows
            .into_iter()
            .filter_map(|row| match row.body {
                Body::Text(text) => Some(text.body),
                _ => None,
            })
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

/// Links, stops the app, queues `count` texts from Alice while Kourier is offline, and returns
/// the account, the device and the sent timestamps.
async fn queue_while_offline(world: &World, count: u32, prefix: &str) -> (AccountId, Vec<u64>) {
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(world).await;
    app.stop().await;
    let sent = world
        .harness
        .send_texts(&SendTexts {
            from: world.alice.aci.clone(),
            to: device.device_ref.clone(),
            count,
            prefix: Some(prefix.to_owned()),
            ..SendTexts::default()
        })
        .await
        .expect("sendTexts");
    assert_eq!(sent.timestamps.len(), usize::try_from(count).unwrap_or(0));
    (account, sent.timestamps)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_queued_while_offline_are_summarized_once() {
    let Some(world) = World::start().await else {
        return;
    };
    let (account, _sent) = queue_while_offline(&world, 500, "offline").await;
    let (app, seen) = catch_up(&world, account, Duration::from_secs(2)).await;
    assert_eq!(
        seen.summaries,
        [(500, 1)],
        "one summary of every queued message, in one conversation"
    );
    assert_eq!(
        seen.notifications, 0,
        "catch-up messages never notify one by one"
    );
    app.wait_online(account).await;
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    let texts = texts(&app, conv, 600).await;
    let expected: Vec<String> = (0..500).map(|i| format!("offline {i}")).collect();
    assert_eq!(texts, expected, "every message once, in order");
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_thousand_queued_messages_ingest_at_500_per_second() {
    const COUNT: u32 = 5_000;
    let Some(world) = World::start().await else {
        return;
    };
    let (account, _sent) = queue_while_offline(&world, COUNT, "bulk").await;
    let (app, seen) = catch_up(&world, account, Duration::from_millis(500)).await;
    assert_eq!(seen.summaries, [(COUNT, 1)]);
    let ingest = seen.summarized.saturating_sub(seen.connected);
    let rate = f64::from(COUNT) / ingest.as_secs_f64().max(f64::EPSILON);
    let _ = writeln!(
        std::io::stderr(),
        "S7 throughput: {COUNT} queued messages committed in core.db {:.2} s after connect \
         ({rate:.0} messages/s; app start to connect {:.2} s; {} build)",
        ingest.as_secs_f64(),
        seen.connected.as_secs_f64(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    assert!(
        rate >= 500.0,
        "catch-up ingest at {rate:.0} messages/s (target ≥ 500/s)"
    );
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    assert_eq!(
        texts(&app, conv, 10).await.last().map(String::as_str),
        Some("bulk 4999")
    );
    app.stop().await;
    world.shutdown().await;
}

/// Set (to a JSON [`ChildSpec`]) in the helper process of the kill test;
/// [`child_app_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_E2E_TEST_CHILD";
/// Prefix of the helper process's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-E2E-CHILD";

/// What the helper process runs: the app over this profile and environment.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    env: BackendEnv,
    account: i64,
}

/// The kill points of the receive path, backend and core (§10.4), with the hit counts the
/// three-message stream of a run reaches.
const KILL_POINTS: [(&str, u32); 8] = [
    (kourier_signal::failpoints::AFTER_INBOX_COMMIT, 1),
    (kourier_signal::failpoints::BEFORE_ACK, 2),
    (kourier_signal::failpoints::AFTER_BACKEND_EFFECTS_COMMIT, 1),
    (kourier_core::FAILPOINT_BEFORE_COMMIT, 1),
    (kourier_core::FAILPOINT_BEFORE_COMMIT, 2),
    (kourier_core::FAILPOINT_BEFORE_ACK, 1),
    (kourier_core::FAILPOINT_BEFORE_ACK, 2),
    (kourier_signal::failpoints::AFTER_CORE_COMMIT, 1),
];

/// Sends the stream of run `run` from Alice (unsealed, sealed, unsealed), 300 ms apart so each
/// message has its own stage-1 batch, lane step, forwarder batch and core transaction.
async fn send_stream(world: &World, device: &DeviceInfo, run: usize) -> Vec<String> {
    let mut texts = Vec::new();
    for (suffix, sealed) in [("a", false), ("b", true), ("c", false)] {
        let text = format!("run {run} {suffix}");
        world
            .harness
            .send_text(&SendText {
                sealed,
                ..SendText::new(&world.alice.aci, &device.device_ref, &text)
            })
            .await
            .expect("sendText");
        texts.push(text);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    texts
}

/// Waits until the app's timeline has every text of `texts` and nothing is in flight, and
/// returns the run's rows as (suffix, author, origin, device), oldest first.
async fn stream_state(
    app: &App,
    world: &World,
    device: &DeviceInfo,
    account: AccountId,
    run: usize,
    texts: &[String],
) -> Vec<(String, Option<UserId>, OriginKind, Option<u32>)> {
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    for text in texts {
        app.wait_text(conv, text).await;
    }
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("deliveries settled");
    let prefix = format!("run {run} ");
    let query = TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    };
    let rows = match app
        .handle
        .query(Query::Timeline(query))
        .await
        .expect("timeline")
    {
        QueryData::Timeline(page) => page.rows,
        other => panic!("unexpected {other:?}"),
    };
    rows.into_iter()
        .filter_map(|row| match &row.body {
            Body::Text(t) if t.body.starts_with(&prefix) => Some((
                t.body[prefix.len()..].to_owned(),
                row.author.clone(),
                row.origin,
                row.device,
            )),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_app_at_every_receive_failpoint_gives_the_uncrashed_state() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;

    // The uncrashed run.
    let texts = send_stream(&world, &device, 0).await;
    let reference = stream_state(&app, &world, &device, account, 0, &texts).await;
    app.stop().await;
    let alice = user(&world.alice);
    assert_eq!(
        reference,
        ["a", "b", "c"]
            .map(|s| (
                s.to_owned(),
                Some(alice.clone()),
                OriginKind::Remote,
                Some(world.alice.device_id)
            ))
            .to_vec(),
        "each message once, in order"
    );

    for (run, (failpoint, hit)) in (1..).zip(KILL_POINTS) {
        let point = format!("{failpoint}:{hit}");
        let spec = ChildSpec {
            dir: world.tmp.path().to_owned(),
            env: world.env.clone(),
            account: account.0,
        };
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "child_app_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
            .env("KOURIER_FAILPOINT", &point)
            // Warnings of the helper app reach this test's stderr (diagnosis of a stuck start).
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_owned()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn the helper process");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let online = tokio::time::timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.expect("child stdout") {
                if line.trim() == format!("{CHILD_LINE} online") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(online, Ok(true), "{point}: the helper app came online");

        let (texts, status) = tokio::join!(send_stream(&world, &device, run), async {
            tokio::time::timeout(WAIT, child.wait())
                .await
                .unwrap_or_else(|_| panic!("{point}: the helper app did not end"))
                .expect("helper status")
        });
        assert_eq!(
            status.signal(),
            Some(6),
            "{point}: the helper aborted at the failpoint ({status})"
        );

        // The next start finishes what was committed; redeliveries are absorbed.
        let app = App::start(world.tmp.path(), &world.env).await;
        let state = stream_state(&app, &world, &device, account, run, &texts).await;
        app.wait_online(account).await;
        app.stop().await;
        assert_eq!(state, reference, "{point}: the uncrashed state");
    }
    world.shutdown().await;
}

/// The helper process of the kill test: the app over the given profile, connected, printing
/// `online` once the account is `Online`, until the armed failpoint aborts it.
#[test]
fn child_app_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    init_tracing();
    let spec: ChildSpec = serde_json::from_str(&spec).expect("child spec");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let app = App::start(&spec.dir, &spec.env).await;
        app.wait_online(AccountId(spec.account)).await;
        println!("{CHILD_LINE} online");
        tokio::time::sleep(Duration::from_secs(120)).await;
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        app.stop().await;
    });
}
