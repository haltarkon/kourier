//! The import of a history transfer's archive through the REAL core (docs/architecture.md
//! §14.2 item 14; the delivery is `kourier-signal/src/transfer/deliver.rs`): a freshly
//! linked account shows the chats of the phone's archive after the transfer (their order,
//! their rows, the read and send states, the unread count the phone had) and the login is
//! done after that; an archive that fails leaves the account without chats; a message that
//! is in the archive AND in the server's queue is one row; an import notifies nobody; a
//! stage that is lost during the delivery is read again and doubles nothing; an own
//! message that was still pending on the phone stays "sending" over a restart and is
//! raised, not doubled, by the phone's later transcript of it, which also starts its
//! disappearing timer. THE ROWS ARE THE TRUTH about what is unread (step S3a): a chat
//! shows the phone's unread count because its rows say so; reading it here clears the
//! count for good, also while the rest of the chat is still delivered and when every row
//! is delivered a second time; a read sync of the phone lowers it; reading an imported
//! chat tells our other devices and never the sender of an imported message.
//!
//! The kills, the bounds and the cursor are tested against the mock in
//! `kourier-signal/tests/history_import.rs`. One measurement, not a test:
//! `cargo test --release -p kourier-e2e --test signal_history_import -- --ignored --nocapture`.
//!
//! Tests print counts and times only.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kourier_core::facade::{
    ConversationListQuery, ConversationRow, Query, QueryData, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, Body, CannotSend, ConversationId, DisappearKind, Expiry, HistoryTransfer,
    HistoryTransferFailure, LoginStepKind, OutgoingState, ReadState, UserId,
};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{
    ChatSpec, ChatWith, ContactSpec, GeneratedSpec, HistorySpec, SendStatusSpec, TextSpec,
    generated_aci, write_history_archive,
};
use support::harness::{DeviceInfo, SendText};
use support::{App, WAIT, World, user};

/// A person of the archive nobody on the mock server is: the generated contact of this
/// number.
const BOB: u32 = 900;
/// Another such person.
const CAROL: u32 = 901;
/// The number of a person the archive knows only by a number (made up).
const EVE_NUMBER: u64 = 15_550_100_044;
/// How long nothing must happen.
const QUIET: Duration = Duration::from_millis(1500);

fn network() -> SignalNetwork {
    // The app keeps the offer of the transfer off until it is switched on; here it is on.
    SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
}

async fn start(world: &World, network: SignalNetwork) -> App {
    let media = world.tmp.path().join("media");
    App::start_tuned(world.tmp.path(), &world.env, network, &media).await
}

/// Links with a phone whose user chose "Transfer message history".
async fn link_with_transfer(app: &mut App, world: &World) -> (AccountId, DeviceInfo, [u8; 32]) {
    let key = world
        .harness
        .set_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let (account, url) = app.begin_link().await;
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(device.sent_transfer_key);
    (account, device, key)
}

/// What the UI heard while a transfer ran to its end.
#[derive(Default)]
struct Heard {
    /// How the transfer ended.
    end: Option<HistoryTransfer>,
    /// `UiEvent::Notify` events.
    notifications: usize,
    /// The fractions of `UiEvent::HistoryProgress`, in the order they came.
    progress: Vec<f32>,
    /// How many times a chat was said to have no more rows to come (`has_more: false`).
    ends: usize,
    /// When the first fraction came.
    first_progress: Option<Instant>,
    /// When the read began.
    reading: Option<Instant>,
}

/// Follows the account's transfer to its end. The login must not be done before it.
async fn follow(app: &mut App, account: AccountId, heard: &mut Heard) -> HistoryTransfer {
    let deadline = tokio::time::Instant::now() + 10 * WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, app.events.recv())
            .await
            .expect("the transfer came to an end");
        match event {
            Ok(UiEvent::HistoryTransfer { account: a, state }) if a == account => {
                if state == HistoryTransfer::Reading && heard.reading.is_none() {
                    heard.reading = Some(Instant::now());
                }
                if state.is_over() {
                    heard.end = Some(state);
                    return state;
                }
            }
            Ok(UiEvent::HistoryProgress {
                progress, has_more, ..
            }) => {
                heard.first_progress.get_or_insert_with(Instant::now);
                heard.progress.extend(progress);
                heard.ends += usize::from(!has_more);
            }
            Ok(UiEvent::Notify(_)) => heard.notifications += 1,
            Ok(UiEvent::LoginStep { account: a, step }) if a == account => match step.kind {
                LoginStepKind::Done => panic!("the login was done before the transfer ended"),
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the core stopped"),
        }
    }
}

/// The login's `Done`; counts what is announced on the way.
async fn login_done(app: &mut App, account: AccountId, heard: &mut Heard) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match tokio::time::timeout_at(deadline, app.events.recv())
            .await
            .expect("the login's Done")
        {
            Ok(UiEvent::LoginStep { account: a, step }) if a == account => match step.kind {
                LoginStepKind::Done => return,
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            Ok(UiEvent::Notify(_)) => heard.notifications += 1,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the core stopped"),
        }
    }
}

async fn upload(world: &World, device: &DeviceInfo, key: &[u8; 32], spec: &HistorySpec) -> PathBuf {
    let path = world.tmp.path().join("phone-archive");
    write_history_archive(&path, key, device.aci.parse().expect("ACI"), spec).expect("archive");
    let place = world
        .harness
        .store_transfer_archive(&path)
        .await
        .expect("storeTransferArchive");
    world
        .harness
        .provide_transfer_archive(&device.device_ref, &place)
        .await
        .expect("provideTransferArchive");
    path
}

async fn conversations(app: &App, account: AccountId) -> Vec<ConversationRow> {
    let query = ConversationListQuery {
        account: Some(account),
        limit: 500,
        ..ConversationListQuery::default()
    };
    match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => page.rows,
        other => panic!("unexpected {other:?}"),
    }
}

/// The user id of the generated contact `i` of the archive writer.
fn generated_user(i: u32) -> UserId {
    UserId::parse(&format!("aci:{}", generated_aci(i).hyphenated())).expect("user id")
}

fn text_of(body: &Body) -> &str {
    match body {
        Body::Text(text) => &text.body,
        _ => panic!("a text"),
    }
}

fn incoming(body: &str, sent_at: u64, read: bool) -> TextSpec {
    TextSpec {
        body: body.to_owned(),
        sent_at,
        received_at: sent_at + 2,
        read,
        ..TextSpec::default()
    }
}

fn outgoing(body: &str, sent_at: u64, statuses: &[SendStatusSpec]) -> TextSpec {
    TextSpec {
        outgoing: true,
        body: body.to_owned(),
        sent_at,
        received_at: sent_at,
        statuses: statuses.to_vec(),
        ..TextSpec::default()
    }
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("ms")
}

/// The first file named `name` below `dir`.
fn find(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
    }
    None
}

/// The whole path with the real core: the chats of the archive are in the list in the
/// phone's order, with their rows, read and send states and the unread counts the phone
/// had; the login is done after the transfer; nobody was notified; a message that is in
/// the archive and still in the server's queue is one row; the chat of a person known only
/// by a number cannot be sent to; and an own message that was pending on the phone stays
/// "sending" over a restart until the phone's transcript of it raises it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_linked_account_shows_the_archives_chats_after_the_transfer() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    // Alice's newest message is still in the server's queue when the phone exports.
    let queued_text = "in the archive and in the queue";
    let queued_at = world
        .harness
        .send_text(&SendText::new(
            &world.alice.aci,
            &device.device_ref,
            queued_text,
        ))
        .await
        .expect("sendText")
        .timestamp;
    let base = queued_at - 3_600_000;
    let pending_at = base + 50;
    let queued_at_ms = i64::try_from(queued_at).expect("ms");
    let alice_aci = world.alice.aci.parse().expect("Alice's ACI");
    let spec = HistorySpec {
        contacts: vec![
            ContactSpec {
                aci: Some(alice_aci),
                profile_name: "Alice".to_owned(),
                profile_sharing: true,
                ..ContactSpec::default()
            },
            ContactSpec {
                aci: Some(generated_aci(BOB)),
                profile_name: "Bob".to_owned(),
                profile_sharing: true,
                ..ContactSpec::default()
            },
            ContactSpec {
                e164: Some(EVE_NUMBER),
                system_name: "Eve".to_owned(),
                ..ContactSpec::default()
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![
                    incoming("old hello", base + 1, true),
                    outgoing("old reply", base + 2, &[SendStatusSpec::Read]),
                    // Not sent yet, in a chat with a timer of an hour: the timer has not
                    // started on the phone.
                    TextSpec {
                        expires_in_ms: Some(3_600_000),
                        ..outgoing("still pending", pending_at, &[SendStatusSpec::Pending])
                    },
                    incoming(queued_text, queued_at, false),
                ],
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![
                    // Sent, as the phone says, with a timer of a day whose start it never
                    // wrote: the timer is kept and starts at the import.
                    TextSpec {
                        expires_in_ms: Some(86_400_000),
                        ..outgoing("to Bob", base + 9, &[SendStatusSpec::Delivered])
                    },
                    incoming("b1", base + 10, false),
                    incoming("b2", base + 11, false),
                ],
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("e1", base + 5, true)],
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", base + 3, &[])],
            },
        ],
        interleaved: true,
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );

    // At `Done` the core has all of it (the last row was acknowledged before `done`).
    let list = conversations(&app, account).await;
    let own = UserId::parse(&format!("aci:{}", device.aci)).expect("own id");
    let eve = UserId::parse(&format!("e164:+{EVE_NUMBER}")).expect("user id");
    let peers: Vec<Option<UserId>> = list.iter().map(|row| row.peer.clone()).collect();
    assert_eq!(
        peers,
        [
            Some(user(&world.alice)),
            Some(generated_user(BOB)),
            Some(eve.clone()),
            Some(own),
        ],
        "the chats of the archive, the most recent first"
    );
    assert!(list[3].is_self);
    let unread: Vec<u32> = list.iter().map(|row| row.unread).collect();
    assert_eq!(unread, [1, 2, 0, 0], "the unread counts the phone had");
    let alice = list[0].id;
    let rows = app.rows(alice).await;
    // Sorted by sent time, whatever order the archive had them in.
    let texts: Vec<&str> = rows.iter().map(|row| text_of(&row.body)).collect();
    assert_eq!(
        texts,
        ["old hello", "old reply", "still pending", queued_text]
    );
    assert!(rows.iter().all(|row| row.history));
    let row = |text: &str| {
        rows.iter()
            .find(|row| text_of(&row.body) == text)
            .expect("the row")
    };
    assert_eq!(row("old hello").read_state, Some(ReadState::Read));
    assert_eq!(row(queued_text).read_state, Some(ReadState::Unread));
    assert_eq!(row("old reply").out_state, Some(OutgoingState::Read));
    assert!(
        matches!(
            row("still pending").out_state,
            Some(OutgoingState::Sending { .. })
        ),
        "pending on the phone: {:?}",
        row("still pending").out_state
    );
    // D5: its timer came along and has not started (it was not sent).
    let unstarted = Some(Expiry {
        kind: DisappearKind::AfterSend,
        timer: Duration::from_secs(3_600),
        started_at: None,
    });
    assert_eq!(row("still pending").expiry, unstarted);
    let bob_rows = app.rows(list[1].id).await;
    assert_eq!(bob_rows.len(), 3);
    let to_bob = bob_rows
        .iter()
        .find(|row| text_of(&row.body) == "to Bob")
        .expect("own row");
    assert_eq!(to_bob.out_state, Some(OutgoingState::Delivered));
    // The lead's decision: a message the phone calls sent whose timer has no start keeps its
    // timer, started at the IMPORT's time: not without a timer (it would stay for ever), and
    // not from its sent time an hour ago (the core would have counted from there).
    let timer = to_bob.expiry.expect("the timer came along");
    assert_eq!(
        (timer.kind, timer.timer),
        (DisappearKind::AfterSend, Duration::from_secs(86_400))
    );
    let imported_at = timer.started_at.expect("the import started it");
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis(),
    )
    .expect("ms");
    assert!(
        (queued_at_ms..=now_ms).contains(&imported_at),
        "started at the import ({imported_at}), after the test began ({queued_at_ms}) and \
         not at the message's sent time"
    );
    // D6: nothing can be sent to a person known only by a number; Alice's chat is open.
    assert_eq!(
        app.features(list[2].id).await.cannot_send,
        Some(CannotSend::NotAccepted)
    );
    app.wait_cannot_send(alice, None).await;

    // The login is done after the transfer, then receiving starts: the queued copy of the
    // message the archive already had makes no second row, and nobody is notified.
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("the queued envelope was taken and acknowledged");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(app.rows_with_text(alice, queued_text).await.len(), 1);
    assert_eq!(app.rows(alice).await.len(), 4);
    while let Ok(event) = app.events.try_recv() {
        heard.notifications += usize::from(matches!(event, UiEvent::Notify(_)));
    }
    assert_eq!(heard.notifications, 0, "an import notifies nobody");
    assert_eq!(conversations(&app, account).await[0].unread, 1);

    // An own row that was pending on the phone, at a start of the core: it has no outbox
    // item here, so the core neither sends it nor fails it. It stays "sending".
    app.stop().await;
    let mut app = start(&world, network()).await;
    app.wait_online(account).await;
    let pending = app.rows_with_text(alice, "still pending").await;
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(pending[0].out_state, Some(OutgoingState::Sending { .. })),
        "after a restart: {:?}",
        pending[0].out_state
    );
    assert_eq!(pending[0].expiry, unstarted, "still not started");
    // The phone sends it at last: its transcript names the same message (same sent time).
    // The row is raised, not doubled.
    world
        .harness
        .request_value(
            "sendSyncSent",
            serde_json::json!({
                "from": world.phone.device_ref, "to": device.device_ref,
                "destination": world.alice.aci, "text": "still pending",
                "timestamp": pending_at, "awaitDelivery": true, "timeout": 60_000,
            }),
        )
        .await
        .expect("sendSyncSent");
    let sent = app
        .wait_row(alice, "still pending", "raised by the transcript", |row| {
            matches!(row.out_state, Some(OutgoingState::Sent { .. }))
        })
        .await;
    // The echo says it was sent: now its timer runs.
    let started = sent.expiry.and_then(|e| e.started_at).expect("started");
    assert!(
        started >= queued_at_ms,
        "the timer starts with the echo, not at the time the message was written (an hour \
         before the test began) nor at the import"
    );
    assert!(
        sent.expiry
            .is_some_and(|e| e.timer == Duration::from_secs(3_600))
    );
    assert_eq!(app.rows_with_text(alice, "still pending").await.len(), 1);
    assert_eq!(app.rows(alice).await.len(), 4);
    let _ = app.event("drain", |_| Some(())).await;
    app.stop().await;
    world.shutdown().await;
}

/// All or nothing up to the commit: an authentic archive whose read fails near its end
/// leaves the account without a chat, and the account is usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_that_fails_leaves_the_account_without_chats() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let mut spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(generated_aci(BOB)),
            profile_name: "Bob".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: (0..200)
                .map(|i| incoming(&format!("b{i}"), base + i, true))
                .collect(),
        }],
        ..HistorySpec::default()
    };
    // The frame that does not decode stands in front of the last three messages.
    let whole = {
        let path = world.tmp.path().join("whole");
        write_history_archive(&path, &key, device.aci.parse().expect("ACI"), &spec)
            .expect("archive")
    };
    spec.broken_after = Some(u32::try_from(whole - 3).expect("frames"));
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Failed(HistoryTransferFailure::Corrupt)
    );
    assert!(heard.progress.is_empty(), "nothing was delivered");
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    assert!(
        conversations(&app, account).await.is_empty(),
        "an import that failed before its commit leaves no chat"
    );
    app.stop().await;
    world.shutdown().await;
}

/// D2 with the real core: the stage is lost in the middle of the delivery. The archive is
/// read again and delivered from its first row; the core's ingest is idempotent on the
/// message id, so no chat holds a row twice and the unread count is the phone's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_re_read_after_a_lost_stage_doubles_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    // Small rows, so the delivery lasts long enough to lose the stage in the middle of it.
    let mut app = start(&world, network().with_history_delivery_rows(2, 1024 * 1024)).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let (chats, messages) = (5u32, 2_000u32);
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(generated_aci(BOB)),
            profile_name: "Bob".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                incoming("b1", base + 1, false),
                incoming("b2", base + 2, false),
            ],
        }],
        generated: Some(GeneratedSpec {
            chats,
            messages,
            text: "generated ".to_owned(),
            first_sent_at: base + 1_000,
        }),
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    // D10: at the first batch every chat is in the list already, in the order it will
    // have when it is full (the phone's order of recency), although all but one are empty.
    app.event("the first batch", |e| match e {
        UiEvent::HistoryProgress { .. } => Some(()),
        _ => None,
    })
    .await;
    let early: Vec<Option<UserId>> = conversations(&app, account)
        .await
        .into_iter()
        .map(|row| row.peer)
        .collect();
    assert_eq!(
        early.len(),
        1 + chats as usize,
        "the whole list stands before it fills"
    );
    // The first rows are in the core: now the stage loses its content.
    app.event("the first delivered rows", |e| match e {
        UiEvent::HistoryProgress { progress, .. } => progress.filter(|p| *p > 0.05),
        _ => None,
    })
    .await;
    let stage = find(world.tmp.path(), "stage.db").expect("the stage file");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stage)
        .and_then(|file| file.set_len(4096))
        .expect("cut the stage");
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    assert!(
        heard.progress.windows(2).any(|w| w[1] < w[0]),
        "the delivery started over (its fraction stepped back)"
    );
    login_done(&mut app, account, &mut heard).await;
    let list = conversations(&app, account).await;
    assert_eq!(list.len(), 1 + chats as usize);
    let place = |peer: &Option<UserId>| {
        (0..chats)
            .find(|i| *peer == Some(generated_user(*i)))
            .map_or(-1, i64::from)
    };
    let (early, last): (Vec<i64>, Vec<i64>) = (
        early.iter().map(place).collect(),
        list.iter().map(|row| place(&row.peer)).collect(),
    );
    assert_eq!(
        early, last,
        "the list was in its final order before it filled"
    );
    let per_chat = (messages / chats) as usize;
    let mut total = 0;
    for i in 0..chats {
        let conv = app.conversation_with(account, &generated_user(i)).await;
        let rows = app.rows(conv).await;
        assert_eq!(rows.len(), per_chat, "a chat holds every row once");
        let mut ids: Vec<_> = rows.iter().map(|row| (row.sent_at, row.id)).collect();
        ids.dedup_by_key(|(sent_at, _)| *sent_at);
        assert_eq!(ids.len(), per_chat, "no sent time twice");
        total += rows.len();
    }
    assert_eq!(total, messages as usize);
    let bob = list
        .iter()
        .find(|row| row.peer == Some(generated_user(BOB)))
        .expect("Bob's chat");
    assert_eq!(bob.unread, 2, "the phone's count, once");
    assert_eq!(app.rows(bob.id).await.len(), 2);
    assert_eq!(heard.notifications, 0);
    println!(
        "{} fractions heard; {} messages in {} chats, each once",
        heard.progress.len(),
        total,
        list.len()
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// A measurement, not a test (run it in a release test build, alone): 50,000 text messages
/// over 200 chats through the real core. Prints messages per second of reading (the
/// archive's check, the mapping and the stage, up to the first delivered batch) and of
/// delivery (up to `Done`), and the sizes of the archive, the stage and `core.db`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run it alone, in a release test build"]
async fn import_rate_of_fifty_thousand_messages() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let (chats, messages) = (200u32, 50_000u32);
    let spec = HistorySpec {
        generated: Some(GeneratedSpec {
            chats,
            messages,
            text: "A message of the usual length, some eighty characters of plain text, no. "
                .to_owned(),
            first_sent_at: now_ms() - 30 * 24 * 3_600_000,
        }),
        ..HistorySpec::default()
    };
    let archive = upload(&world, &device, &key, &spec).await;
    let archive_bytes = std::fs::metadata(&archive).expect("archive").len();
    // The stage's size is looked at while it is delivered (the end removes it).
    let tmp = world.tmp.path().to_owned();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        tokio::task::spawn_blocking(move || {
            let size = |name: &str| {
                find(&tmp, name)
                    .and_then(|path| std::fs::metadata(path).ok())
                    .map_or(0, |meta| meta.len())
            };
            let (mut stage, mut backend, mut backend_wal) = (0, 0, 0);
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                stage = stage.max(size("stage.db"));
                backend = backend.max(size("backend.db"));
                backend_wal = backend_wal.max(size("backend.db-wal"));
                std::thread::sleep(Duration::from_millis(20));
            }
            (stage, backend, backend_wal)
        })
    };
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    let done = Instant::now();
    stop.store(true, std::sync::atomic::Ordering::Release);
    let (stage_bytes, backend_bytes, backend_wal_bytes) = watcher.await.expect("watcher");
    let reading = heard.reading.expect("the read began");
    let first = heard.first_progress.expect("a first batch");
    let read_s = first.duration_since(reading).as_secs_f64();
    let deliver_s = done.duration_since(first).as_secs_f64();
    login_done(&mut app, account, &mut heard).await;
    assert_eq!(conversations(&app, account).await.len(), chats as usize);
    app.stop().await;
    let core_bytes = std::fs::metadata(world.tmp.path().join("data/core.db"))
        .expect("core.db")
        .len();
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    println!("import rate, {messages} messages over {chats} chats:");
    println!(
        "  reading (check, map, stage; to the first delivered batch): {read_s:.2} s, {:.0} messages/s",
        f64::from(messages) / read_s
    );
    println!(
        "  delivery (to Done; {} batches heard): {deliver_s:.2} s, {:.0} messages/s",
        heard.progress.len(),
        f64::from(messages) / deliver_s
    );
    println!(
        "  files: archive {:.1} MiB, stage {:.1} MiB, core.db {:.1} MiB; backend.db at most \
         {:.1} MiB with a log of at most {:.1} MiB",
        mib(archive_bytes),
        mib(stage_bytes),
        mib(core_bytes),
        mib(backend_bytes),
        mib(backend_wal_bytes)
    );
    world.shutdown().await;
}

// Probes of the independent review of step S2 (`target/s2-context/review.md`).

/// D2 at `StartMode::Rebuild`, with the REAL core (the builder's test of it runs against
/// the fake core): the application is quit in the middle of a delivery, `core.db` is lost,
/// and the next start rebuilds the account from its `key.wrap`. The new core must get the
/// whole import: every chat with every row once, the phone's unread count, the chats'
/// features (a chat that can be sent to), the list in the phone's order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_a_rebuilt_core_db_gets_the_whole_import() {
    let Some(world) = World::start().await else {
        return;
    };
    let small_rows = || network().with_history_delivery_rows(2, 1024 * 1024);
    let mut app = start(&world, small_rows()).await;
    let (_, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let (chats, messages) = (5u32, 2_000u32);
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(generated_aci(BOB)),
            profile_name: "Bob".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                incoming("b1", base + 1, false),
                incoming("b2", base + 2, false),
            ],
        }],
        generated: Some(GeneratedSpec {
            chats,
            messages,
            text: "generated ".to_owned(),
            first_sent_at: base + 1_000,
        }),
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    app.event("a part of the import is in the core", |e| match e {
        UiEvent::HistoryProgress { progress, .. } => progress.filter(|p| *p > 0.1),
        _ => None,
    })
    .await;
    app.stop().await;
    assert!(
        find(world.tmp.path(), "stage.db").is_some(),
        "the quit fell into the delivery"
    );
    // core.db is lost; the account's directory with its key.wrap and backend.db stays.
    let data = world.tmp.path().join("data");
    std::fs::remove_file(data.join("core.db")).expect("core.db");
    for side in ["core.db-wal", "core.db-shm"] {
        let _ = std::fs::remove_file(data.join(side));
    }

    let mut app = start(&world, small_rows()).await;
    // The rebuilt account has a new id: its transfer says which.
    let deadline = tokio::time::Instant::now() + 10 * WAIT;
    let (account, end) = loop {
        match tokio::time::timeout_at(deadline, app.events.recv())
            .await
            .expect("the transfer of the rebuilt account came to an end")
        {
            Ok(UiEvent::HistoryTransfer { account, state }) if state.is_over() => {
                break (account, state);
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => panic!("the core stopped"),
        }
    };
    assert_eq!(end, HistoryTransfer::Done);
    let list = conversations(&app, account).await;
    assert_eq!(
        list.len(),
        1 + chats as usize,
        "every chat of the archive is in the rebuilt core"
    );
    let per_chat = (messages / chats) as usize;
    for i in 0..chats {
        let conv = app.conversation_with(account, &generated_user(i)).await;
        let rows = app.rows(conv).await;
        assert_eq!(rows.len(), per_chat, "a chat holds every row once");
        assert!(rows.iter().all(|row| row.history));
    }
    let bob = list
        .iter()
        .find(|row| row.peer == Some(generated_user(BOB)))
        .expect("Bob's chat");
    assert_eq!(bob.unread, 2, "the phone's count");
    assert_eq!(app.rows(bob.id).await.len(), 2);
    assert_eq!(
        list.last().map(|row| row.peer.clone()),
        Some(Some(generated_user(BOB))),
        "the least recent chat is the last of the list"
    );
    // The new core was told the chats' features again: Bob's chat can be sent to.
    app.wait_cannot_send(bob.id, None).await;
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// Review F5 with the real core. The last row of a chat is due (its timer ran out long ago),
/// so the delivery may leave it out when the server has confirmed the clock; the chat is told
/// that nothing follows all the same (an empty batch, which the real core takes), and the
/// rows before it are there once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_end_of_a_chat_is_heard_when_its_last_row_is_due() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network().with_history_delivery_rows(1, 1024 * 1024)).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(generated_aci(BOB)),
            profile_name: "Bob".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                incoming("b1", base + 1, true),
                incoming("b2", base + 2, true),
                TextSpec {
                    expires_in_ms: Some(1_000),
                    expire_start: Some(base + 3),
                    ..incoming("b3, long gone", base + 3, true)
                },
            ],
        }],
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    assert_eq!(heard.ends, 1, "the chat was told that nothing follows");
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    let conv = app.conversation_with(account, &generated_user(BOB)).await;
    let texts: Vec<String> = app
        .rows(conv)
        .await
        .iter()
        .filter_map(|row| match &row.body {
            Body::Text(text) => Some(text.body.clone()),
            _ => None,
        })
        .collect();
    assert!(
        texts.iter().filter(|t| t.starts_with("b1")).count() == 1
            && texts.iter().filter(|t| t.starts_with("b2")).count() == 1,
        "the rows before it are there once"
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// The unread count of `peer`'s chat as the list shows it.
async fn unread_of(app: &App, account: AccountId, peer: &UserId) -> Option<(ConversationId, u32)> {
    conversations(app, account)
        .await
        .into_iter()
        .find(|row| row.peer.as_ref() == Some(peer))
        .map(|row| (row.id, row.unread))
}

/// Waits until `peer`'s chat shows `unread`.
async fn wait_unread(app: &App, account: AccountId, peer: &UserId, unread: u32) -> ConversationId {
    let deadline = Instant::now() + WAIT;
    loop {
        let now = unread_of(app, account, peer).await;
        if let Some((conv, n)) = now
            && n == unread
        {
            return conv;
        }
        assert!(Instant::now() < deadline, "unread {unread}: {now:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// THE ROWS ARE THE TRUTH (step S3a, rule 2) with the real core. A chat shows the phone's
/// unread count as soon as its unread rows are there (no count is sent beside them).
/// Reading it here, while the rest of the chat is still on its way, clears the count, and
/// it does not come back: not with the rest of the chat, not with the chat's end, and not
/// when the stage is lost and EVERY row is delivered a second time. Another chat keeps its
/// count through all of it; a read sync of the phone then lowers that one by the messages
/// it names. Nobody is notified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_unread_count_of_an_imported_chat_is_its_unread_rows() {
    let Some(world) = World::start().await else {
        return;
    };
    // Rows of two messages: Bob's chat fills over 1,500 of them.
    let mut app = start(&world, network().with_history_delivery_rows(2, 1024 * 1024)).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let (bob, carol) = (generated_user(BOB), generated_user(CAROL));
    let older = 3_000u64;
    // Bob's chat is the most recent, so it fills first. The two messages the phone had not
    // read stand FIRST in the archive (the core sorts by sent time, whatever the order),
    // then the rest of the chat, which the phone had read.
    let mut bob_texts = vec![
        incoming("b unread 1", base + 2_000_001, false),
        incoming("b unread 2", base + 2_000_002, false),
    ];
    bob_texts.extend((0..older).map(|i| incoming("b older", base + 1_000_000 + i, true)));
    let carol_at = base + 500_000;
    let spec = HistorySpec {
        contacts: vec![
            ContactSpec {
                aci: Some(generated_aci(BOB)),
                profile_name: "Bob".to_owned(),
                profile_sharing: true,
                ..ContactSpec::default()
            },
            ContactSpec {
                aci: Some(generated_aci(CAROL)),
                profile_name: "Carol".to_owned(),
                profile_sharing: true,
                ..ContactSpec::default()
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: bob_texts,
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![
                    incoming("c read", carol_at, true),
                    outgoing("c mine", carol_at + 1, &[SendStatusSpec::Read]),
                    incoming("c unread 1", carol_at + 2, false),
                    incoming("c unread 2", carol_at + 3, false),
                    incoming("c unread 3", carol_at + 4, false),
                ],
            },
        ],
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();

    // Bob's first row of two: the count the phone had, while the chat is still filling.
    let bob_chat = wait_unread(&app, account, &bob, 2).await;
    app.send(UiCommand::MarkRead { conv: bob_chat });
    wait_unread(&app, account, &bob, 0).await;
    let there = app.rows(bob_chat).await.len();
    assert!(
        there < 500,
        "the chat was read while most of it was still to come ({there} rows of {})",
        older + 2
    );
    // Now the stage is lost: the archive is read again, and every row is delivered again,
    // the two unread ones too (they say "unread" again; here they are read).
    let stage = find(world.tmp.path(), "stage.db").expect("the stage file");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stage)
        .and_then(|file| file.set_len(4096))
        .expect("cut the stage");
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    assert!(
        heard.progress.windows(2).any(|w| w[1] < w[0]),
        "the delivery started over (its fraction stepped back)"
    );
    assert_eq!(
        unread_of(&app, account, &bob).await,
        Some((bob_chat, 0)),
        "read here: it did not come back with the rest of the chat or with the second delivery"
    );
    let unread_rows = app.rows_with_text(bob_chat, "b unread 1").await;
    assert_eq!(unread_rows.len(), 1, "once");
    assert_eq!(unread_rows[0].read_state, Some(ReadState::Read));
    assert_eq!(app.rows(bob_chat).await.len(), 500, "the chat is full");
    // Carol's chat was never read here: the phone's count, once, after two deliveries.
    let carol_chat = wait_unread(&app, account, &carol, 3).await;
    assert_eq!(app.rows(carol_chat).await.len(), 5);

    // Receiving starts. The phone reads two of Carol's three: its read sync lowers the
    // count by the rows it names (a count sent beside the rows could not follow it).
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    let carol_aci = generated_aci(CAROL).hyphenated().to_string();
    world
        .harness
        .send_sync(serde_json::json!({
            "from": world.phone.aci, "to": device.device_ref,
            "read": [
                { "sender": carol_aci, "timestamp": carol_at + 2 },
                { "sender": carol_aci, "timestamp": carol_at + 3 },
            ],
            "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("sendSync");
    wait_unread(&app, account, &carol, 1).await;
    let still = app.rows_with_text(carol_chat, "c unread 3").await;
    assert_eq!(still[0].read_state, Some(ReadState::Unread));
    assert_eq!(unread_of(&app, account, &bob).await, Some((bob_chat, 0)));

    while let Ok(event) = app.events.try_recv() {
        heard.notifications += usize::from(matches!(event, UiEvent::Notify(_)));
    }
    assert_eq!(heard.notifications, 0, "an import notifies nobody");
    println!(
        "read here with {there} of {} rows delivered; {} fractions heard",
        older + 2,
        heard.progress.len()
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// The `(sender ACI as base64, timestamp)` entries of a read sync the phone got.
fn read_entries(sync: &serde_json::Value) -> Vec<(String, u64)> {
    sync["read"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    Some((
                        e["senderAciBinary"].as_str()?.to_owned(),
                        e["timestamp"].as_u64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The 16 bytes of an ACI as the harness prints binary fields.
fn aci_b64(aci: &str) -> String {
    use base64::Engine as _;
    let hex: String = aci.chars().filter(char::is_ascii_hexdigit).collect();
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// D4 and D9 on the mock server, through the real core: reading an imported chat with
/// unread messages sends ONE read sync to our own devices, which names EACH of them (not
/// only the newest), and NO read receipt to their sender, although read receipts are on
/// (the messages were never sent to this device). The proof that receipts are on: a live
/// message of the same sender, read right after, gets its read receipt, and the read sync
/// for it names it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_an_imported_chat_tells_our_devices_and_not_the_sender() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let me = device.device_ref.clone();
    let base = now_ms() - 3_600_000;
    let unread_at = [base + 2, base + 4];
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
            profile_name: "Alice".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                incoming("seen on the phone", base + 1, true),
                incoming("not seen on the phone", unread_at[0], false),
                outgoing("mine", base + 3, &[SendStatusSpec::Read]),
                incoming("not seen either", unread_at[1], false),
            ],
        }],
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    let alice = user(&world.alice);
    let conv = wait_unread(&app, account, &alice, 2).await;

    // Read here: the phone is told, Alice is not.
    app.send(UiCommand::MarkRead { conv });
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "read", WAIT)
        .await
        .expect("the read sync to our phone");
    let alice_b64 = aci_b64(&world.alice.aci);
    assert_eq!(
        read_entries(&sync.sync_message),
        [
            (alice_b64.clone(), unread_at[0]),
            (alice_b64.clone(), unread_at[1])
        ],
        "every imported message that was read here"
    );
    wait_unread(&app, account, &alice, 0).await;
    assert!(
        world
            .harness
            .wait_for_receipt(&world.alice.aci, Some(&me), QUIET)
            .await
            .is_err(),
        "no receipt to the sender of an imported message"
    );
    assert!(
        world
            .harness
            .wait_for_sync_message(&world.phone.aci, Some(&me), "read", QUIET)
            .await
            .is_err(),
        "one read sync, not two"
    );

    // The control: read receipts ARE on. A live message of Alice, read here, gets one.
    let live = world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(
                &world.alice.aci,
                &device.device_ref,
                "live, after the import",
            )
        })
        .await
        .expect("sendText")
        .timestamp;
    wait_unread(&app, account, &alice, 1).await;
    app.send(UiCommand::MarkRead { conv });
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "read", WAIT)
        .await
        .expect("the read sync for the live message");
    assert_eq!(read_entries(&sync.sync_message), [(alice_b64, live)]);
    let deadline = Instant::now() + WAIT;
    let read_receipt = loop {
        let receipt = world
            .harness
            .wait_for_receipt(&world.alice.aci, Some(&me), WAIT)
            .await
            .expect("a receipt to Alice");
        // The delivery receipt of the sealed message comes first.
        if receipt.kind == "read" {
            break receipt;
        }
        assert!(Instant::now() < deadline, "no read receipt");
    };
    assert_eq!(
        read_receipt.timestamps,
        [live],
        "the live message, and not the imported one"
    );
    while let Ok(event) = app.events.try_recv() {
        heard.notifications += usize::from(matches!(event, UiEvent::Notify(_)));
    }
    // (The live message may be announced: only the import must not be.)
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}
