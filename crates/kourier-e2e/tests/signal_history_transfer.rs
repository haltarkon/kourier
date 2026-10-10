//! The history transfer of a fresh link through the real core (docs/architecture.md §14.2
//! item 14): what a UI sees (`UiEvent::HistoryTransfer`: waiting for the phone, downloading,
//! reading, done or skipped; an end again after a restart, until
//! `UiCommand::DismissHistoryTransfer`), that `UiCommand::SkipHistoryTransfer` ends a wait, and that a
//! message which waited on the server reaches the timeline only after the transfer is over.
//! The protocol itself (restarts, damaged archives, the long-poll) is tested in
//! `kourier-signal/tests/history_transfer.rs`.

mod support;

use std::time::Duration;

use kourier_core::facade::{UiCommand, UiEvent};
use kourier_model::{AccountId, HistoryTransfer, LoginStepKind};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{ArchiveSpec, write_archive};
use support::harness::{DeviceInfo, SendText};
use support::{App, WAIT, World, user};

/// The core on a Signal backend whose long-poll for the archive lasts a second.
async fn start(world: &World) -> App {
    // The app keeps the offer of the transfer off until the import exists; here it is on.
    let network = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
        .with_history_transfer_frame_delay(Duration::from_millis(1));
    let media = world.tmp.path().join("media");
    App::start_tuned(world.tmp.path(), &world.env, network, &media).await
}

/// Links with a phone whose user chose "Transfer message history"; returns once the device
/// exists (the login is not done: the transfer comes first).
async fn link_with_transfer(app: &mut App, world: &World) -> (AccountId, DeviceInfo, [u8; 32]) {
    let key = world
        .harness
        .set_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let (account, url) = app.begin_link().await;
    assert!(
        url.ends_with("&capabilities=backup5"),
        "a fresh link asks for the history"
    );
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(device.sent_transfer_key);
    (account, device, key)
}

/// The next state of the account's transfer that `pred` accepts.
async fn transfer_state(
    app: &mut App,
    account: AccountId,
    what: &str,
    pred: impl Fn(&HistoryTransfer) -> bool,
) -> HistoryTransfer {
    app.event(what, |e| match e {
        UiEvent::HistoryTransfer { account: a, state } if *a == account && pred(state) => {
            Some(*state)
        }
        UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
            LoginStepKind::Done => panic!("the login was done before the transfer was {what}"),
            LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
            _ => None,
        },
        _ => None,
    })
    .await
}

async fn login_done(app: &mut App, account: AccountId) {
    app.event("the login's Done", |e| match e {
        UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
            LoginStepKind::Done => Some(()),
            LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
            _ => None,
        },
        _ => None,
    })
    .await;
}

/// Alice's text, queued on the server for our device.
async fn alice_queues(world: &World, device: &DeviceInfo, text: &str) {
    world
        .harness
        .send_text(&SendText::new(&world.alice.aci, &device.device_ref, text))
        .await
        .expect("sendText");
}

/// No conversation with Alice exists: nothing of her message reached the core.
async fn nothing_from_alice(app: &App, account: AccountId, world: &World) {
    use kourier_core::facade::{ConversationListQuery, Query, QueryData};
    let query = ConversationListQuery {
        account: Some(account),
        limit: 100,
        ..ConversationListQuery::default()
    };
    match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => assert!(
            page.rows
                .iter()
                .all(|row| row.peer.as_ref() != Some(&user(&world.alice))),
            "a message reached the core while the transfer held receiving back"
        ),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ui_follows_a_history_transfer_and_receives_after_it() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    transfer_state(&mut app, account, "Waiting", |s| {
        *s == HistoryTransfer::Waiting
    })
    .await;

    let text = "sent before the history arrived";
    alice_queues(&world, &device, text).await;
    // The server has delivered it; nothing takes it off the socket.
    let unacked = world
        .harness
        .wait_for_deliveries(&device.device_ref, Duration::from_millis(1500))
        .await;
    assert!(
        unacked.is_err(),
        "an envelope was acknowledged during the transfer"
    );
    nothing_from_alice(&app, account, &world).await;

    // The phone uploads; the UI sees the download with its size, the read and the end.
    let path = world.tmp.path().join("phone-archive");
    let spec = ArchiveSpec {
        extra_messages: 1_000,
        ..ArchiveSpec::default()
    };
    write_archive(&path, &key, device.aci.parse().expect("ACI"), &spec).expect("archive");
    let size = std::fs::metadata(&path).expect("archive").len();
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
    let downloading = transfer_state(&mut app, account, "Downloading with its size", |s| {
        matches!(s, HistoryTransfer::Downloading { total: Some(_), .. })
    })
    .await;
    assert!(
        matches!(downloading, HistoryTransfer::Downloading { total: Some(total), .. } if total == size),
        "{downloading:?}"
    );
    transfer_state(&mut app, account, "Reading", |s| {
        *s == HistoryTransfer::Reading
    })
    .await;
    nothing_from_alice(&app, account, &world).await;
    transfer_state(&mut app, account, "Done", |s| *s == HistoryTransfer::Done).await;

    // Then the login finishes and the message that waited arrives.
    login_done(&mut app, account).await;
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    app.wait_text(conv, text).await;
    app.wait_online(account).await;
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_ui_skips_a_history_transfer_that_waits() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = start(&world).await;
    let (account, device, _key) = link_with_transfer(&mut app, &world).await;
    let waiting = transfer_state(&mut app, account, "Waiting", |s| {
        *s == HistoryTransfer::Waiting
    })
    .await;
    assert!(waiting.can_skip());
    let text = "sent while the phone was asked";
    alice_queues(&world, &device, text).await;
    nothing_from_alice(&app, account, &world).await;

    app.send(UiCommand::SkipHistoryTransfer { account });
    let skipped = transfer_state(&mut app, account, "Skipped", HistoryTransfer::is_over).await;
    assert_eq!(skipped, HistoryTransfer::Skipped);
    login_done(&mut app, account).await;
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    app.wait_text(conv, text).await;
    app.wait_online(account).await;

    // The end is not told once: after a restart the UI hears again how the transfer of
    // this account ended (a UI that was not there when it ended would otherwise never know).
    app.stop().await;
    let mut app = start(&world).await;
    let again = transfer_state(&mut app, account, "Skipped, after the restart", |s| {
        s.is_over()
    })
    .await;
    assert_eq!(again, HistoryTransfer::Skipped);
    app.wait_online(account).await;

    // The user has seen it: dismissed. The next start reports nothing.
    app.send(UiCommand::DismissHistoryTransfer { account });
    tokio::time::sleep(Duration::from_secs(1)).await;
    app.stop().await;
    let mut app = start(&world).await;
    app.wait_online(account).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(event) = app.events.try_recv() {
        assert!(
            !matches!(event, UiEvent::HistoryTransfer { .. }),
            "a dismissed transfer is not reported again"
        );
    }
    app.stop().await;
    world.shutdown().await;
}
