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
//! chat tells our other devices and never the sender of an imported message. The content
//! of 1:1 chats (step S3b): every kind the mapper maps arrives as its row (formatting,
//! quotes, reactions, edits, a sticker, placeholders, notices, call lines), and a reaction
//! the phone sends later for an imported edited message's newest version lands on it. What
//! roams for direct chats and Note to Self (step S4a): the list shows the phone's pins in its
//! order (a pinned chat with no message among them), the archived chat on the archive's page,
//! the mute (for ever, or over), the marked unread dot and the blocked chat; a live message
//! into a chat muted for ever is silent, and one into a chat whose mute is over is announced;
//! a message sent into an imported chat carries the archive's timer and its version. A chat
//! deleted while its history is still delivered stays deleted and the delivery goes on to its
//! end (link feedback L5).
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
use support::harness::{DeviceInfo, Primary, SendText};
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
    // The library's default is no offer (the app turns it on); a test of the import does.
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
                ..ChatSpec::default()
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
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("e1", base + 5, true)],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", base + 3, &[])],
                ..ChatSpec::default()
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
            ..ChatSpec::default()
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
            ..ChatSpec::default()
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
            ..ChatSpec::default()
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
            ..ChatSpec::default()
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
                ..ChatSpec::default()
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
                ..ChatSpec::default()
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
            ..ChatSpec::default()
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

/// What a row of the timeline is, as one line of a dump: its kind and the parts of it the
/// mapper decides (never a text of the user's: these are the test's own).
fn dump(row: &kourier_core::facade::MessageRow) -> String {
    use kourier_model::SystemEvent;
    let kind = match &row.body {
        Body::Text(text) => format!("text {:?} ranges {}", text.body, text.ranges.len()),
        Body::Sticker(image) => format!("sticker {:?}", image.state),
        Body::Unsupported { kind_hint, .. } => format!("placeholder {kind_hint}"),
        Body::System(SystemEvent::Call(line)) => {
            format!("call {:?} {:?}", line.direction, line.state)
        }
        Body::System(SystemEvent::TimerChanged { timer, .. }) => format!("timer {timer:?}"),
        Body::System(SystemEvent::IdentityKeyChanged { .. }) => "safety number".to_owned(),
        other => format!("{other:?}"),
    };
    let reply = row
        .reply
        .as_ref()
        .map(|reply| format!(" reply {:?}", reply.state))
        .unwrap_or_default();
    let reactions: Vec<String> = row
        .reactions
        .iter()
        .map(|group| format!("{}x{}", group.emoji, group.users.len()))
        .collect();
    format!(
        "{kind}{reply} reactions {reactions:?} edited {} read {:?}",
        row.revision > 0,
        row.read_state
    )
}

/// Step S3b with the real core: every content kind of a 1:1 chat that the mapper maps
/// arrives as the row the spec says (formatting and a mention of us, a quote that was found
/// and one that was not, reactions, an edited message, a sticker (young: a download on a
/// click), the placeholders, the notices, the call lines and their timers), the chat counts the unread
/// message and the unread missed call, and a chat of quiet notices only is not listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rich_archive_becomes_these_chats() {
    use kourier_model::SystemEvent;
    use kourier_signal::archive_testing::{
        CallSpec, IndividualCallSpec, ItemSpec, NoticeSpec, QuoteSpec, RangeKindSpec, RangeSpec,
        ReactionSpec, SimpleSpec, Who,
    };
    let Some(world) = World::start().await else {
        return;
    };
    let test_began = i64::try_from(now_ms()).expect("ms");
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let own_aci = device.aci.parse().expect("own ACI");
    let range = |start, length, kind| RangeSpec {
        start,
        length,
        kind,
    };
    let of_kind = |kind: ItemSpec, sent_at: u64, outgoing: bool| TextSpec {
        kind,
        outgoing,
        statuses: if outgoing {
            vec![SendStatusSpec::Delivered]
        } else {
            Vec::new()
        },
        ..incoming("", sent_at, true)
    };
    let notice =
        |notice: NoticeSpec, sent_at: u64| of_kind(ItemSpec::Notice(notice), sent_at, false);
    let call = |state, outgoing, sent_at: u64| {
        of_kind(
            ItemSpec::Call(CallSpec::Individual {
                call_id: Some(sent_at),
                video: false,
                outgoing,
                state,
                read: false,
                started_at: sent_at,
            }),
            sent_at,
            false,
        )
    };
    let alice_items = vec![
        TextSpec {
            ranges: vec![
                range(0, 1, RangeKindSpec::Mention(own_aci)),
                range(2, 4, RangeKindSpec::Bold),
            ],
            ..incoming("\u{fffc} look here", base + 1, false)
        },
        TextSpec {
            quote: Some(QuoteSpec {
                target: Some(base + 1),
                author: Some(Who::Peer),
                text: "\u{fffc} look here".to_owned(),
                ..QuoteSpec::default()
            }),
            ..outgoing("the reply", base + 2, &[SendStatusSpec::Read])
        },
        TextSpec {
            quote: Some(QuoteSpec {
                target: None,
                author: Some(Who::Own),
                text: "gone".to_owned(),
                ..QuoteSpec::default()
            }),
            ..incoming("about something lost", base + 3, true)
        },
        TextSpec {
            reactions: vec![
                ReactionSpec {
                    by: Who::Own,
                    emoji: "\u{1f44d}".to_owned(),
                    sent_at: base + 100,
                    sort_order: 1,
                },
                ReactionSpec {
                    by: Who::Peer,
                    emoji: "\u{1f600}".to_owned(),
                    sent_at: base + 101,
                    sort_order: 2,
                },
            ],
            ..incoming("liked", base + 4, true)
        },
        TextSpec {
            revisions: vec![base + 5, base + 6],
            ..incoming("newest words", base + 7, true)
        },
        of_kind(ItemSpec::Sticker { data: true }, base + 8, false),
        of_kind(ItemSpec::ViewOnce, base + 9, false),
        of_kind(
            ItemSpec::Poll {
                question: "Lunch?".to_owned(),
            },
            base + 10,
            true,
        ),
        notice(
            NoticeSpec::TimerChange {
                expires_in_ms: 3_600_000,
            },
            base + 11,
        ),
        notice(NoticeSpec::Simple(SimpleSpec::IdentityUpdate), base + 12),
        call(IndividualCallSpec::Missed, false, base + 13),
        TextSpec {
            expires_in_ms: Some(86_400_000),
            ..call(IndividualCallSpec::Accepted, true, base + 14)
        },
    ];
    let spec = HistorySpec {
        contacts: vec![
            ContactSpec {
                aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
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
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: alice_items,
                ..ChatSpec::default()
            },
            // Quiet notices only: staged, not listed.
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![
                    notice(NoticeSpec::Simple(SimpleSpec::IdentityUpdate), base + 20),
                    notice(
                        NoticeSpec::Simple(SimpleSpec::MessageRequestAccepted),
                        base + 21,
                    ),
                ],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    let list = conversations(&app, account).await;
    let peers: Vec<Option<UserId>> = list.iter().map(|row| row.peer.clone()).collect();
    assert_eq!(
        peers,
        [Some(user(&world.alice))],
        "Bob's chat has no loud row"
    );
    assert_eq!(
        list[0].unread, 2,
        "the unread message and the unread missed call"
    );
    let mut rows = app.rows(list[0].id).await;
    rows.sort_by_key(|row| row.sent_at);
    assert!(rows.iter().all(|row| row.history));
    let lines: Vec<String> = rows.iter().map(dump).collect();
    let read = "read Some(Read)";
    let unread = "read Some(Unread)";
    let mine = "read None";
    assert_eq!(
        lines,
        [
            format!("text \"\u{fffc} look here\" ranges 2 reactions [] edited false {unread}"),
            format!("text \"the reply\" ranges 0 reply Known reactions [] edited false {mine}"),
            format!(
                "text \"about something lost\" ranges 0 reply Missing reactions [] edited false {read}"
            ),
            format!(
                "text \"liked\" ranges 0 reactions [\"\u{1f44d}x1\", \"\u{1f600}x1\"] edited false {read}"
            ),
            format!("text \"newest words\" ranges 0 reactions [] edited true {read}"),
            format!("sticker Remote reactions [] edited false {read}"),
            format!("placeholder view-once reactions [] edited false {read}"),
            format!("placeholder poll reactions [] edited false {mine}"),
            "timer Some(3600s) reactions [] edited false read None".to_owned(),
            "safety number reactions [] edited false read None".to_owned(),
            "call Incoming Missed reactions [] edited false read Some(Unread)".to_owned(),
            "call Outgoing Answered reactions [] edited false read None".to_owned(),
        ],
        "the rows of the spec, in sent order"
    );
    let row = |i: usize| &rows[i];
    assert!(row(0).mentions_me, "a mention of us");
    assert!(row(6).view_once);
    // The quote that was found names the message it quotes.
    let quoted = row(1).reply.as_ref().and_then(|reply| reply.target);
    assert_eq!(quoted, Some(row(0).id));
    assert_eq!(
        row(4).edited_at,
        Some(i64::try_from(base + 7).expect("ms")),
        "the newest version's time"
    );
    assert_eq!(
        row(4).sent_at,
        i64::try_from(base + 5).expect("ms"),
        "the original's"
    );
    let by_me = row(3)
        .reactions
        .iter()
        .find(|group| group.emoji == "\u{1f44d}")
        .expect("ours");
    assert!(by_me.by_me());
    // The timer-change notice has no timer of its own (Android never starts it).
    assert!(matches!(
        row(8).body,
        Body::System(SystemEvent::TimerChanged { .. })
    ));
    assert_eq!(row(8).expiry, None);
    // The outgoing call line without a start: its timer starts at the import, not at the
    // call an hour ago (which would erase it at once with a shorter timer).
    let timer = row(11).expiry.expect("the call's timer");
    assert_eq!(
        (timer.kind, timer.timer),
        (DisappearKind::AfterSend, Duration::from_secs(86_400))
    );
    assert!(timer.started_at.expect("started") >= test_began);
    login_done(&mut app, account, &mut heard).await;
    while let Ok(event) = app.events.try_recv() {
        heard.notifications += usize::from(matches!(event, UiEvent::Notify(_)));
    }
    assert_eq!(heard.notifications, 0, "an import notifies nobody");
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// An imported message that was edited on the phone keeps the ids of its versions: a
/// reaction the phone sends afterwards for the NEWEST version (its id is the edit's time)
/// lands on the imported row, and so does one of the contact for the original.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reaction_from_the_phone_lands_on_an_imported_edited_message() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
            profile_name: "Alice".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![TextSpec {
                revisions: vec![base + 1, base + 2],
                ..incoming("edited twice on the phone", base + 3, true)
            }],
            ..ChatSpec::default()
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
    let conv = conversations(&app, account).await[0].id;
    let text = "edited twice on the phone";
    let rows = app.rows_with_text(conv, text).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].revision > 0, "shown as edited");

    // The phone reacts to the message as it stands now: the newest version's id.
    world
        .harness
        .send_data(serde_json::json!({
            "from": world.phone.aci, "to": device.device_ref,
            "reaction": {
                "emoji": "\u{1f44d}", "targetAuthor": world.alice.aci,
                "targetTimestamp": base + 3,
            },
            "sync": { "destination": world.alice.aci },
            "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("sendData");
    let row = app
        .wait_row(
            conv,
            text,
            "the phone's reaction on the imported row",
            |row| row.reactions.iter().any(|group| group.by_me()),
        )
        .await;
    assert_eq!(row.id, rows[0].id);
    // Alice reacts to the original (its id is the message's own).
    world
        .harness
        .send_data(serde_json::json!({
            "from": world.alice.aci, "to": device.device_ref, "sealed": true,
            "withProfileKey": true,
            "reaction": {
                "emoji": "\u{1f600}", "targetAuthor": world.alice.aci,
                "targetTimestamp": base + 1,
            },
            "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("sendData");
    let alice = user(&world.alice);
    let row = app
        .wait_row(conv, text, "Alice's reaction on the same row", |row| {
            row.reactions
                .iter()
                .any(|group| group.users.contains(&Some(alice.clone())))
        })
        .await;
    assert_eq!(row.reactions.len(), 2);
    assert_eq!(app.rows(conv).await.len(), 1, "no second row");
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// A person of the archive that nobody pins, archives or mutes (made up, not on the server).
const DAVE: u32 = 902;

/// One page of the list (the inbox, or the archived chats), in its order.
async fn page(app: &App, account: AccountId, archived: bool) -> Vec<ConversationRow> {
    let query = ConversationListQuery {
        account: Some(account),
        archived,
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

/// A live message of `from` into `conv`, sealed, after the import: the notifications the UI
/// hears until the row is there and a quiet moment after it.
async fn notifications_of_a_live_message(
    app: &mut App,
    world: &World,
    device: &DeviceInfo,
    from: &Primary,
    conv: ConversationId,
    text: &str,
) -> usize {
    // What came before is not this message's.
    while app.events.try_recv().is_ok() {}
    world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&from.aci, &device.device_ref, text)
        })
        .await
        .expect("sendText");
    app.wait_text(conv, text).await;
    tokio::time::sleep(QUIET).await;
    let mut heard = 0;
    loop {
        match app.events.try_recv() {
            Ok(UiEvent::Notify(_)) => heard += 1,
            Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return heard,
        }
    }
}

/// Step S4a with the real core: the list after an import shows what the phone's chat frames
/// say. Its inbox has the pinned chats first, in the phone's order (a pinned chat without
/// any message among them), then the rest; a muted chat (for ever, or until a time that has
/// passed), a chat marked unread and a blocked one say so; the archived chat is on the
/// archive's page.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_list_after_an_import_has_the_phones_pins_archive_mute_and_marks() {
    let Some(world) = World::start().await else {
        return;
    };
    // A second person on the mock, who writes live after the import.
    let zed = world
        .harness
        .create_primary(&support::harness::CreatePrimary::named("Zed"))
        .await
        .expect("Zed");
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let person = |i: u32, name: &str| ContactSpec {
        aci: Some(generated_aci(i)),
        profile_name: name.to_owned(),
        profile_sharing: true,
        ..ContactSpec::default()
    };
    let past_mute = base - 1_000;
    let spec = HistorySpec {
        contacts: vec![
            person(BOB, "Bob"),
            person(CAROL, "Carol"),
            ContactSpec {
                blocked: true,
                ..person(DAVE, "Dave")
            },
            ContactSpec {
                aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
                ..person(0, "Alice")
            },
            ContactSpec {
                aci: Some(zed.aci.parse().expect("Zed's ACI")),
                ..person(0, "Zed")
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![incoming("b1", base + 1, true)],
                pinned_order: Some(30),
                mute_until_ms: Some(i64::MAX as u64),
                ..ChatSpec::default()
            },
            // Pinned, and not a single message.
            ChatSpec {
                with: ChatWith::Contact(1),
                pinned_order: Some(4),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", base + 2, &[])],
                pinned_order: Some(7),
                marked_unread: true,
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(2),
                texts: vec![incoming("d1", base + 3, true)],
                mute_until_ms: Some(past_mute),
                ..ChatSpec::default()
            },
            // Archived and muted for ever, with a message.
            ChatSpec {
                with: ChatWith::Contact(3),
                texts: vec![incoming("a1", base + 4, true)],
                archived: true,
                mute_until_ms: Some(i64::MAX as u64),
                ..ChatSpec::default()
            },
            // Muted until a time that has passed (the oldest message: the last place).
            ChatSpec {
                with: ChatWith::Contact(4),
                texts: vec![incoming("z1", base, true)],
                mute_until_ms: Some(past_mute),
                ..ChatSpec::default()
            },
        ],
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

    let own = UserId::parse(&format!("aci:{}", device.aci)).expect("own id");
    let alice = user(&world.alice);
    let names = [
        (generated_user(BOB), "Bob"),
        (generated_user(CAROL), "Carol"),
        (generated_user(DAVE), "Dave"),
        (alice.clone(), "Alice"),
        (user(&zed), "Zed"),
        (own, "Note to Self"),
    ];
    let line = |row: &ConversationRow| -> String {
        let name = names
            .iter()
            .find(|(user, _)| row.peer.as_ref() == Some(user))
            .map_or("?", |(_, name)| *name);
        let muted = match row.muted_until {
            None => "-".to_owned(),
            Some(i64::MAX) => "for ever".to_owned(),
            Some(at) if at == i64::try_from(past_mute).unwrap() => "until the past".to_owned(),
            Some(_) => "until another time".to_owned(),
        };
        format!(
            "{name} pin {:?} archived {} muted {muted} marked {} blocked {}",
            row.pinned_order, row.archived, row.marked_unread, row.blocked
        )
    };
    let inbox: Vec<String> = page(&app, account, false).await.iter().map(line).collect();
    let archived: Vec<String> = page(&app, account, true).await.iter().map(line).collect();
    assert_eq!(
        inbox,
        [
            "Carol pin Some(0) archived false muted - marked false blocked false",
            "Note to Self pin Some(1) archived false muted - marked true blocked false",
            "Bob pin Some(2) archived false muted for ever marked false blocked false",
            "Dave pin None archived false muted until the past marked false blocked true",
            "Zed pin None archived false muted until the past marked false blocked false",
        ],
        "the inbox"
    );
    assert_eq!(
        archived,
        ["Alice pin None archived true muted for ever marked false blocked false"],
        "the archive"
    );
    assert_eq!(heard.notifications, 0, "the import notified nobody");

    // The mute outlives the import. A live message into the chat muted for ever is silent;
    // one into the chat whose mute ran out on the phone is announced like any other.
    let alice_conv = page(&app, account, true).await[0].id;
    assert_eq!(
        notifications_of_a_live_message(
            &mut app,
            &world,
            &device,
            &world.alice,
            alice_conv,
            "live, into a chat muted for ever"
        )
        .await,
        0,
        "a chat muted for ever stays silent"
    );
    let zed_conv = app.conversation_with(account, &user(&zed)).await;
    assert_eq!(
        notifications_of_a_live_message(
            &mut app,
            &world,
            &device,
            &zed,
            zed_conv,
            "live, into a chat whose mute is over"
        )
        .await,
        1,
        "a mute in the past is no mute"
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// Step S4a: the 1:1 timer is the one chat field only the archive has (the Storage Service
/// does not carry it). A message Kourier sends into an imported chat carries the archive's
/// timer and its version. The person is one the phone's contact sync does not name (the
/// initial contact sync states each contact's timer and wins for those).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_into_an_imported_chat_carries_the_archives_timer() {
    let Some(world) = World::start().await else {
        return;
    };
    let zed = world
        .harness
        .create_primary(&support::harness::CreatePrimary::named("Zed"))
        .await
        .expect("Zed");
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let base = now_ms() - 3_600_000;
    let zed_aci = zed.aci.parse().expect("Zed's ACI");
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(zed_aci),
            profile_name: "Zed".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![incoming("z1", base + 1, true)],
            expiration_timer_ms: Some(3_600_000),
            expire_timer_version: 5,
            ..ChatSpec::default()
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
    let conv = app.conversation_with(account, &user(&zed)).await;
    app.send(UiCommand::SendText {
        conv,
        text: kourier_model::Text::plain("after the import"),
        reply_to: None,
    });
    let got = world
        .harness
        .wait_for_data(&zed.aci, "message", Some(&device.device_ref), None, WAIT)
        .await
        .expect("Zed got it");
    assert_eq!(
        got.data_message["expireTimer"], 3_600,
        "the archive's timer"
    );
    assert_eq!(got.data_message["expireTimerVersion"], 5, "and its version");
    let row = app
        .wait_row(conv, "after the import", "sent", |row| {
            row.out_state.is_some()
        })
        .await;
    assert_eq!(
        row.expiry.map(|e| e.timer),
        Some(Duration::from_secs(3_600)),
        "the core gave the message the chat's timer"
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// The group conversation of `key` in the list, once there.
async fn group_row(app: &App, account: AccountId, conv_key: &str) -> ConversationRow {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = conversations(app, account).await;
        if let Some(row) = rows
            .into_iter()
            .find(|row| matches!(row.kind, kourier_model::ConvKind::Group))
        {
            return row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group {conv_key} is not listed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn detail(app: &App, conv: ConversationId) -> kourier_core::facade::ConversationDetail {
    match app
        .handle
        .query(Query::Conversation(conv))
        .await
        .expect("detail")
    {
        QueryData::Conversation(Some(detail)) => *detail,
        other => panic!("unexpected {other:?}"),
    }
}

/// Step S4b with the real core, e2e tests A, C, D and G: an imported group is listed with the
/// archive's title and members (with their roles) before its first fetch and cannot be sent
/// to ("still loading"); its rows are the archive's (members' texts, a quote, a reaction, a
/// mention of us, the mapped notices, a missed group call) and its unread count is the
/// unread rows plus the unread missed call; a group of quiet notices only is not listed; the
/// import notifies nobody. Then the server's group: a member's live message joins the same
/// conversation, the chat can be sent to, and a send reaches the server's members and not
/// the member the archive still names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_group_is_listed_from_the_archive_and_a_send_reaches_the_servers_members() {
    use base64::Engine as _;
    use kourier_model::{MemberRole, SystemEvent};
    use kourier_signal::archive_testing::{
        CallSpec, GroupCallSpec, GroupChangeSpec, GroupSpec, ItemSpec, NoticeSpec, Party,
        QuoteSpec, RangeKindSpec, RangeSpec, ReactionSpec, SnapshotSpec, Who,
    };
    use support::harness::{CreateGroup, CreatePrimary, ModifyGroup, SendGroupText};
    let Some(world) = World::start().await else {
        return;
    };
    let mut people = Vec::new();
    for name in ["Ada", "Bo", "Cal"] {
        people.push(
            world
                .harness
                .create_primary(&CreatePrimary::named(name))
                .await
                .expect(name),
        );
    }
    let (ada, bo, cal) = (&people[0], &people[1], &people[2]);
    let group = world
        .harness
        .create_group(&CreateGroup {
            from: world.phone.device_ref.clone(),
            title: "Server title".to_owned(),
            members: vec![
                ada.device_ref.clone(),
                bo.device_ref.clone(),
                cal.device_ref.clone(),
            ],
            ..CreateGroup::default()
        })
        .await
        .expect("createGroup");
    // Cal left on the server after the phone's export.
    world
        .harness
        .modify_group(&ModifyGroup {
            from: world.phone.device_ref.clone(),
            group: group.id.clone(),
            remove_members: vec![cal.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("modifyGroup");
    let quiet = [0x5e_u8; 32];
    let base = now_ms() - 3_600_000;
    let contact = |p: &Primary| ContactSpec {
        aci: Some(p.aci.parse().expect("ACI")),
        profile_name: p.profile_name.clone(),
        profile_sharing: true,
        ..ContactSpec::default()
    };
    let own = world.phone.aci.parse().expect("our ACI");
    let spec = HistorySpec {
        contacts: vec![contact(ada), contact(bo), contact(cal)],
        groups: vec![
            GroupSpec {
                master_key: base64::engine::general_purpose::STANDARD
                    .decode(&group.master_key)
                    .expect("master key"),
                whitelisted: true,
                blocked: false,
                snapshot: Some(SnapshotSpec {
                    title: Some("Archive title".to_owned()),
                    members: vec![
                        (Party::Own, false),
                        (Party::Contact(0), true),
                        (Party::Contact(1), false),
                        (Party::Contact(2), false),
                    ],
                    ..SnapshotSpec::default()
                }),
            },
            // A group whose only item is a member leaving: quiet, not listed.
            GroupSpec {
                master_key: quiet.to_vec(),
                snapshot: Some(SnapshotSpec {
                    title: Some("Quiet".to_owned()),
                    members: vec![(Party::Own, false), (Party::Contact(0), false)],
                    ..SnapshotSpec::default()
                }),
                ..GroupSpec::default()
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Group(0),
                texts: vec![
                    TextSpec {
                        author: Some(Who::Contact(0)),
                        ..incoming("from Ada", base + 1, false)
                    },
                    TextSpec {
                        author: Some(Who::Contact(1)),
                        quote: Some(QuoteSpec {
                            target: Some(base + 1),
                            author: Some(Who::Contact(0)),
                            text: "from Ada".to_owned(),
                            ..QuoteSpec::default()
                        }),
                        reactions: vec![ReactionSpec {
                            by: Who::Contact(2),
                            emoji: "\u{1f44d}".to_owned(),
                            sent_at: base + 3,
                            sort_order: 1,
                        }],
                        ranges: vec![RangeSpec {
                            start: 0,
                            length: 1,
                            kind: RangeKindSpec::Mention(own),
                        }],
                        body: "\u{fffc} look".to_owned(),
                        ..incoming("", base + 2, true)
                    },
                    TextSpec {
                        outgoing: true,
                        kind: ItemSpec::Notice(NoticeSpec::GroupChange(vec![
                            GroupChangeSpec::Name {
                                by: Party::Contact(0),
                                name: Some("Archive title".to_owned()),
                            },
                            GroupChangeSpec::Avatar { by: Party::Absent },
                        ])),
                        sent_at: base + 4,
                        ..TextSpec::default()
                    },
                    TextSpec {
                        kind: ItemSpec::Call(CallSpec::Group {
                            call_id: Some(31),
                            state: GroupCallSpec::Missed,
                            ringer: Some(Who::Contact(1)),
                            started_by: None,
                            read: false,
                            started_at: base + 5,
                        }),
                        sent_at: base + 5,
                        ..TextSpec::default()
                    },
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Group(1),
                texts: vec![TextSpec {
                    kind: ItemSpec::Notice(NoticeSpec::GroupChange(vec![
                        GroupChangeSpec::MemberLeft {
                            who: Party::Contact(0),
                        },
                    ])),
                    sent_at: base + 6,
                    ..TextSpec::default()
                }],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    // The first fetch waits until the test lets it.
    world
        .harness
        .fail_group_fetch(&group.id, true)
        .await
        .expect("failGroupFetch");
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    login_done(&mut app, account, &mut heard).await;
    assert_eq!(heard.notifications, 0, "an import notifies nobody");

    // Before its first fetch: the archive's group.
    let row = group_row(&app, account, "imported").await;
    assert_eq!(row.title.as_deref(), Some("Archive title"));
    let groups = conversations(&app, account)
        .await
        .into_iter()
        .filter(|row| matches!(row.kind, kourier_model::ConvKind::Group))
        .count();
    assert_eq!(groups, 1, "the quiet group is not listed");
    assert_eq!(row.unread, 2, "Ada's unread text and the missed call");
    let conv = row.id;
    let before = detail(&app, conv).await;
    let roles: Vec<(UserId, MemberRole)> = before
        .members
        .iter()
        .map(|m| (m.user.clone(), m.role))
        .collect();
    assert!(roles.contains(&(user(ada), MemberRole::Admin)), "{roles:?}");
    assert!(roles.contains(&(user(cal), MemberRole::Member)));
    assert_eq!(before.features.cannot_send, Some(CannotSend::StillLoading));
    let rows = app.rows(conv).await;
    let lines: Vec<String> = rows.iter().map(dump).collect();
    assert_eq!(rows.len(), 4, "{lines:#?}");
    assert_eq!(rows[0].author.as_ref(), Some(&user(ada)));
    assert_eq!(rows[1].author.as_ref(), Some(&user(bo)));
    assert!(rows[1].reply.is_some(), "the quote");
    assert_eq!(rows[1].reactions.len(), 1, "Cal's reaction");
    assert!(matches!(
        &rows[2].body,
        Body::System(SystemEvent::TitleChanged { by: Some(by), .. }) if *by == user(ada)
    ));
    assert!(matches!(&rows[3].body, Body::System(SystemEvent::Call(_))));

    // The server's group: a live message of Ada's makes it fetch, in the same chat.
    world
        .harness
        .fail_group_fetch(&group.id, false)
        .await
        .expect("failGroupFetch");
    app.wait_online(account).await;
    world
        .harness
        .send_group_text(&SendGroupText {
            from: ada.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "live from Ada".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("sendGroupText");
    app.wait_text(conv, "live from Ada").await;
    app.wait_cannot_send(conv, None).await;
    assert_eq!(
        conversations(&app, account)
            .await
            .into_iter()
            .filter(|row| matches!(row.kind, kourier_model::ConvKind::Group))
            .count(),
        1,
        "still one group conversation"
    );
    let after = detail(&app, conv).await;
    assert!(
        !after.members.iter().any(|m| m.user == user(cal)),
        "the server's members replace the archive's"
    );
    let stats = world.harness.send_stats().await.expect("sendStats");
    let to_cal_before = stats.single.get(&cal.aci).copied().unwrap_or(0);
    app.send(UiCommand::SendText {
        conv,
        text: kourier_model::Text::plain("to the group"),
        reply_to: None,
    });
    for member in [ada, bo] {
        world
            .harness
            .wait_for_group_message(&member.aci, &group.id, None, WAIT)
            .await
            .expect("a member got it");
    }
    let stats = world.harness.send_stats().await.expect("sendStats");
    assert_eq!(
        stats.single.get(&cal.aci).copied().unwrap_or(0),
        to_cal_before,
        "nothing went to the member the archive still named"
    );
    assert!(
        !stats.multi_recipients.iter().any(|r| r.contains(&cal.aci)),
        "nor in a multi-recipient send"
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// Step S4b with the real core: a group we are INVITED to is listed with the request to
/// accept it and a reason that says what to do (accept the invitation on the phone), before
/// its first fetch, not "still loading" and not "no longer a member". The invitation is the
/// group's one item and is no notice, so the chat lists after every chat that has a row.
/// When the first fetch finds the invitation over (the server's state does not list us), the
/// request is gone from the chat and the reason is the true one: we are not in the group.
/// A group we asked to join says that its request waits for an administrator.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_invitation_is_offered_with_its_reason_until_the_first_fetch_finds_it_over() {
    use base64::Engine as _;
    use kourier_signal::archive_testing::{
        GroupChangeSpec, GroupSpec, ItemSpec, NoticeSpec, Party, SnapshotSpec, Who,
    };
    use support::harness::{CreateGroup, CreatePrimary};
    let Some(world) = World::start().await else {
        return;
    };
    let mut people = Vec::new();
    for name in ["Ada", "Bo"] {
        people.push(
            world
                .harness
                .create_primary(&CreatePrimary::named(name))
                .await
                .expect(name),
        );
    }
    let (ada, bo) = (&people[0], &people[1]);
    // On the server we are in neither group (Ada made them), and the first fetch of the
    // first waits until the test lets it.
    let mut groups = Vec::new();
    for title in ["Invited", "Asked"] {
        groups.push(
            world
                .harness
                .create_group(&CreateGroup {
                    from: ada.device_ref.clone(),
                    title: title.to_owned(),
                    members: vec![bo.device_ref.clone()],
                    ..CreateGroup::default()
                })
                .await
                .expect("createGroup"),
        );
    }
    for group in &groups {
        world
            .harness
            .fail_group_fetch(&group.id, true)
            .await
            .expect("failGroupFetch");
    }
    let base = now_ms() - 3_600_000;
    let contact = |p: &Primary| ContactSpec {
        aci: Some(p.aci.parse().expect("ACI")),
        profile_name: p.profile_name.clone(),
        profile_sharing: true,
        ..ContactSpec::default()
    };
    let key_of = |group: &support::harness::GroupInfo| {
        base64::engine::general_purpose::STANDARD
            .decode(&group.master_key)
            .expect("master key")
    };
    let invitation = |inviter: usize| TextSpec {
        outgoing: true,
        kind: ItemSpec::Notice(NoticeSpec::GroupChange(vec![
            GroupChangeSpec::SelfInvited {
                inviter: Party::Contact(inviter),
            },
        ])),
        sent_at: base + 1,
        ..TextSpec::default()
    };
    let spec = HistorySpec {
        contacts: vec![contact(ada), contact(bo)],
        groups: vec![
            GroupSpec {
                master_key: key_of(&groups[0]),
                whitelisted: false,
                snapshot: Some(SnapshotSpec {
                    title: Some("Invited".to_owned()),
                    members: vec![(Party::Contact(0), true), (Party::Contact(1), false)],
                    pending: vec![Party::Own],
                    ..SnapshotSpec::default()
                }),
                ..GroupSpec::default()
            },
            GroupSpec {
                master_key: key_of(&groups[1]),
                whitelisted: false,
                snapshot: Some(SnapshotSpec {
                    title: Some("Asked".to_owned()),
                    members: vec![(Party::Contact(0), true)],
                    requesting: vec![Party::Own],
                    ..SnapshotSpec::default()
                }),
                ..GroupSpec::default()
            },
        ],
        chats: vec![
            // A chat with a row, to see where the invitation lists.
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![TextSpec {
                    author: Some(Who::Contact(0)),
                    ..incoming("hello from Ada", base + 2, true)
                }],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Group(0),
                texts: vec![invitation(0)],
                ..ChatSpec::default()
            },
            // A request to join has nothing to show either, and is pinned on the phone.
            ChatSpec {
                with: ChatWith::Group(1),
                pinned_order: Some(1),
                texts: Vec::new(),
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    upload(&world, &device, &key, &spec).await;
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    login_done(&mut app, account, &mut heard).await;
    assert_eq!(heard.notifications, 0, "an import notifies nobody");

    // Before the first fetch of either.
    let find = |rows: &[ConversationRow], title: &str| {
        rows.iter()
            .position(|row| row.title.as_deref() == Some(title))
    };
    let rows = conversations(&app, account).await;
    let invited_at = find(&rows, "Invited").expect("the invitation is listed");
    let asked_at = find(&rows, "Asked").expect("the request to join is listed (pinned)");
    assert!(
        asked_at == 0 && rows[asked_at].pinned_order.is_some(),
        "the pinned chat is on top"
    );
    let ada_at = rows
        .iter()
        .position(|row| row.peer.as_ref() == Some(&user(ada)))
        .expect("the chat with Ada");
    assert!(
        ada_at < invited_at,
        "the invitation has no row and lists after a chat that has one"
    );
    let invited = rows[invited_at].clone();
    assert!(invited.request_pending, "the invitation is offered");
    assert!(!rows[asked_at].request_pending, "a request to join is not");
    assert_eq!(
        detail(&app, invited.id).await.features.cannot_send,
        Some(CannotSend::InvitationPending)
    );
    assert_eq!(
        detail(&app, rows[asked_at].id).await.features.cannot_send,
        Some(CannotSend::JoinRequestPending)
    );

    // The first fetch of the invited group: the server's state does not list us.
    world
        .harness
        .fail_group_fetch(&groups[0].id, false)
        .await
        .expect("failGroupFetch");
    app.wait_online(account).await;
    app.wait_cannot_send(invited.id, Some(CannotSend::NotMember))
        .await;
    let rows = conversations(&app, account).await;
    let invited = rows
        .iter()
        .find(|row| row.title.as_deref() == Some("Invited"))
        .expect("still listed");
    assert!(
        !invited.request_pending,
        "the invitation is over: nothing to accept"
    );
    // The request to join is still waiting for its first fetch, and is no member either.
    let asked = rows
        .iter()
        .find(|row| row.title.as_deref() == Some("Asked"))
        .expect("listed");
    assert_eq!(
        detail(&app, asked.id).await.features.cannot_send,
        Some(CannotSend::JoinRequestPending)
    );
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    world.shutdown().await;
}

/// The keys of the CDN objects asked for so far, without the archive's.
async fn asked_of_the_cdn(world: &World, archive: &str) -> Vec<String> {
    let mut keys: Vec<String> = world
        .harness
        .downloads()
        .await
        .expect("downloadStats")
        .into_iter()
        .filter(|d| d.key != archive)
        .map(|d| d.key)
        .collect();
    keys.sort();
    keys
}

/// Step S5 with the real core: an import starts no download by itself except the rest of a
/// YOUNG long text (user decision, 2026-10-10: "fetch automatically"), which completes its
/// message, also an edited one (its newest version shows the whole text); an old long text
/// is not fetched and its message keeps the inline part; a young file is a download on a
/// click (one request, the bytes as sent), an old one is "not available on this device" at
/// once and a click on it asks nothing; a young file the server lost becomes "no longer
/// available" at its click (one request, 404) and a second click asks nothing; a restart asks
/// nothing again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_import_fetches_its_young_long_texts_and_nothing_else_by_itself() {
    use kourier_model::{Attachment, AttachmentState};
    use kourier_signal::archive_testing::{
        AttachmentSpec, LocatorSpec, PointerSpec, encrypt_attachment,
    };
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network()).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let now = now_ms();
    let hour = 3_600_000;
    let old = now - 200 * 24 * hour;
    // Objects on CDN 3 under keys as the server makes them (route words would be logged).
    let cdn3 = world
        .harness
        .handshake()
        .state_dir
        .join("cdn3")
        .join("attachments");
    std::fs::create_dir_all(&cdn3).expect("cdn3");
    let object_key = [0x3c; 64];
    let store = |name: &str, plain: &[u8]| {
        let encrypted = encrypt_attachment(plain, &object_key, [2; 16]).expect("encrypt");
        std::fs::write(cdn3.join(name), &encrypted.blob).expect("store");
        PointerSpec {
            content_type: Some("text/x-signal-plain".to_owned()),
            locator: Some(LocatorSpec::of(
                &encrypted,
                &object_key,
                3,
                name,
                None,
                true,
            )),
            ..PointerSpec::default()
        }
    };
    let whole = |n: &str| format!("{n}: ") + &"every word of the whole text. ".repeat(120);
    let inline = |text: &str| text[..2048].to_owned();
    let (young, edited, aged) = (whole("young"), whole("edited"), whole("old"));
    let file_bytes: Vec<u8> = (0..20_000u32).map(|i| (i % 241) as u8).collect();
    let file = |name: &str| AttachmentSpec {
        pointer: Some(PointerSpec {
            content_type: Some("application/pdf".to_owned()),
            file_name: Some("plan.pdf".to_owned()),
            ..store(name, &file_bytes)
        }),
        flag: 0,
        client_uuid: None,
    };
    let texts = vec![
        TextSpec {
            long_text: Some(store("S5LongYoung", young.as_bytes())),
            ..incoming(&inline(&young), now - hour, true)
        },
        TextSpec {
            revisions: vec![now - hour + 10],
            long_text: Some(store("S5LongEdited", edited.as_bytes())),
            ..incoming(&inline(&edited), now - hour + 20, true)
        },
        TextSpec {
            long_text: Some(store("S5LongOld", aged.as_bytes())),
            ..incoming(&inline(&aged), old, true)
        },
        TextSpec {
            attachments: vec![file("S5FileYoung")],
            ..incoming("the young file", now - hour + 30, true)
        },
        TextSpec {
            attachments: vec![file("S5FileOld")],
            ..incoming("the old file", old + 1, true)
        },
        TextSpec {
            attachments: vec![file("S5FileGone")],
            ..incoming("the gone file", now - hour + 40, true)
        },
    ];
    // The server lost this one although the archive's pointer is young.
    std::fs::remove_file(cdn3.join("S5FileGone")).expect("gone");
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
            profile_name: "Alice".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts,
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    };
    let path = world.tmp.path().join("phone-archive");
    write_history_archive(&path, &key, device.aci.parse().expect("ACI"), &spec).expect("archive");
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
    let mut heard = Heard::default();
    assert_eq!(
        follow(&mut app, account, &mut heard).await,
        HistoryTransfer::Done
    );
    login_done(&mut app, account, &mut heard).await;
    app.wait_online(account).await;
    let chat = conversations(&app, account).await[0].id;

    // The young long texts complete their messages by themselves.
    let by_time = |rows: &[kourier_core::facade::MessageRow], at: u64| {
        rows.iter()
            .find(|row| row.sent_at == i64::try_from(at).expect("ms"))
            .cloned()
            .unwrap_or_else(|| panic!("the row of {at}"))
    };
    let deadline = tokio::time::Instant::now() + WAIT;
    let rows = loop {
        let rows = app.rows(chat).await;
        if rows.len() == 6
            && [now - hour, now - hour + 10]
                .iter()
                .all(|at| matches!(by_time(&rows, *at).body, Body::Text(_)))
        {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the long texts were not joined"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(text_of(&by_time(&rows, now - hour).body), young);
    let edited_row = by_time(&rows, now - hour + 10);
    assert_eq!(
        text_of(&edited_row.body),
        edited,
        "the newest version is whole"
    );
    assert!(edited_row.revision > 0, "and it is the edited message");
    let attachments = |row: &kourier_core::facade::MessageRow| -> Vec<Attachment> {
        match &row.body {
            Body::Media { attachments, .. } => attachments.clone(),
            other => panic!("media: {}", other.kind_name()),
        }
    };
    let aged_row = by_time(&rows, old);
    let Body::Media { caption, .. } = &aged_row.body else {
        panic!("the old long text keeps its pointer-less attachment");
    };
    assert_eq!(
        caption.as_ref().map(|c| c.body.as_str()),
        Some(&inline(&aged)[..])
    );
    let states = |rows: &[kourier_core::facade::MessageRow]| -> Vec<AttachmentState> {
        [old, now - hour + 30, old + 1, now - hour + 40]
            .iter()
            .map(|at| attachments(&by_time(rows, *at))[0].state)
            .collect()
    };
    assert_eq!(
        states(&rows),
        [
            AttachmentState::Unavailable,
            AttachmentState::Remote,
            AttachmentState::Unavailable,
            AttachmentState::Remote
        ]
    );
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        asked_of_the_cdn(&world, &place.key).await,
        ["S5LongEdited", "S5LongYoung"],
        "the young long texts and nothing else"
    );

    // A click on the young file: one request, the bytes as the phone had them.
    let young_file = attachments(&by_time(&rows, now - hour + 30))[0]
        .id
        .expect("an id");
    app.send(UiCommand::DownloadAttachment {
        attachment: young_file,
    });
    let deadline = tokio::time::Instant::now() + WAIT;
    while attachments(&by_time(&app.rows(chat).await, now - hour + 30))[0].state
        != AttachmentState::Local
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the file did not arrive"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    app.send(UiCommand::OpenMedia {
        attachment: young_file,
    });
    let opened = app
        .event("MediaOpened", |e| match e {
            UiEvent::MediaOpened { attachment, path } if *attachment == young_file => {
                Some(path.clone())
            }
            UiEvent::ActionFailed { .. } => panic!("OpenMedia failed"),
            _ => None,
        })
        .await;
    assert!(
        std::fs::read(&opened).expect("media file") == file_bytes,
        "the bytes as sent"
    );
    app.send(UiCommand::CloseMedia {
        attachment: young_file,
    });

    // A click on the old file asks the CDN nothing: it asks the phone (S6).
    let old_file = attachments(&by_time(&rows, old + 1))[0].id.expect("an id");
    app.send(UiCommand::DownloadAttachment {
        attachment: old_file,
    });
    let deadline = tokio::time::Instant::now() + WAIT;
    while attachments(&by_time(&app.rows(chat).await, old + 1))[0].state != AttachmentState::Asking
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old file was not asked of the phone"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(QUIET).await;
    let asked = ["S5FileYoung", "S5LongEdited", "S5LongYoung"];
    assert_eq!(asked_of_the_cdn(&world, &place.key).await, asked);

    // A click on a young file the server lost: one request, answered 404, and the user's
    // click then asks the phone at once (S6); a second click joins that ask and asks the CDN
    // nothing.
    let gone_at = now - hour + 40;
    let gone_file = attachments(&by_time(&rows, gone_at))[0].id.expect("an id");
    assert_eq!(
        attachments(&by_time(&rows, gone_at))[0].state,
        AttachmentState::Remote
    );
    app.send(UiCommand::DownloadAttachment {
        attachment: gone_file,
    });
    let deadline = tokio::time::Instant::now() + WAIT;
    while attachments(&by_time(&app.rows(chat).await, gone_at))[0].state != AttachmentState::Asking
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lost file was not asked of the phone after its 404"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let asked = ["S5FileGone", "S5FileYoung", "S5LongEdited", "S5LongYoung"];
    assert_eq!(asked_of_the_cdn(&world, &place.key).await, asked);
    app.send(UiCommand::DownloadAttachment {
        attachment: gone_file,
    });
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        asked_of_the_cdn(&world, &place.key).await,
        asked,
        "the second click asks nothing"
    );

    // A restart asks the CDN nothing again and keeps the file; the two asks still wait (the
    // mock's phone does not answer within the test).
    app.send(UiCommand::DismissHistoryTransfer { account });
    app.stop().await;
    let app = start(&world, network()).await;
    app.wait_online(account).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(asked_of_the_cdn(&world, &place.key).await, asked);
    assert_eq!(
        states(&app.rows(chat).await),
        [
            AttachmentState::Unavailable,
            AttachmentState::Local,
            AttachmentState::Asking,
            AttachmentState::Asking
        ]
    );
    app.stop().await;
    world.shutdown().await;
}

/// Link feedback L5 (`target/link-feedback/buttons.md` 1.5): a chat is deleted while its
/// history is still being delivered (the UI refuses it while a chat fills, but the core does
/// not: the phone can delete a chat as well). The rows of that chat still to come must neither
/// bring it back to the list with the old messages nor fail the delivery: the transfer ends
/// `Done`, the other chats hold every row, the deleted one is not listed and holds none.
/// Rows of one message each make the delivery a long run of single round trips, so the chat is
/// deleted well before its last row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_deleted_during_its_delivery_does_not_come_back_and_the_delivery_goes_on() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world, network().with_history_delivery_rows(1, 1024 * 1024)).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let (chats, messages) = (3u32, 900u32);
    let per_chat = (messages / chats) as usize;
    let spec = HistorySpec {
        generated: Some(GeneratedSpec {
            chats,
            messages,
            text: "A message of the generated history.".to_owned(),
            first_sent_at: now_ms() - 3_600_000,
        }),
        ..HistorySpec::default()
    };
    upload(&world, &device, &key, &spec).await;

    // The first chat of the list, as soon as it has a few rows and is far from its last.
    let deadline = tokio::time::Instant::now() + WAIT;
    let (victim, victim_peer, rows_then) = loop {
        let listed = conversations(&app, account).await;
        if let Some(row) = listed.first() {
            let rows = app.rows(row.id).await.len();
            if rows >= 3 {
                break (row.id, row.peer.clone().expect("a direct chat"), rows);
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no chat with rows in the list: {listed:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        rows_then < per_chat - 20,
        "the chat was already whole ({rows_then} of {per_chat} rows): the test proves nothing"
    );
    app.send(UiCommand::DeleteConversation { conv: victim });

    let mut heard = Heard::default();
    let end = follow(&mut app, account, &mut heard).await;
    assert_eq!(
        end,
        HistoryTransfer::Done,
        "the delivery went on to its end"
    );
    login_done(&mut app, account, &mut heard).await;
    let listed = conversations(&app, account).await;
    println!(
        "deleted at {rows_then} of {per_chat} rows; listed at the end: {} chats",
        listed.len()
    );
    assert!(
        listed
            .iter()
            .all(|row| row.peer.as_ref() != Some(&victim_peer)),
        "the deleted chat is back in the list: {listed:?}"
    );
    assert_eq!(listed.len(), chats as usize - 1);
    for row in &listed {
        assert_eq!(app.rows(row.id).await.len(), per_chat, "{:?}", row.peer);
    }
    assert!(
        app.rows(victim).await.is_empty(),
        "rows of the deleted chat were stored after the delete"
    );
    app.stop().await;
    world.shutdown().await;
}
