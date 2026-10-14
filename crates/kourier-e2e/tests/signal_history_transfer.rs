//! The history transfer of a fresh link through the real core (docs/architecture.md §14.2
//! item 14): what a UI sees (`UiEvent::HistoryTransfer`: waiting for the phone, downloading,
//! reading, done or skipped; an end again after a restart, until
//! `UiCommand::DismissHistoryTransfer`), that `UiCommand::SkipHistoryTransfer` ends a wait, and that a
//! message which waited on the server reaches the timeline only after the transfer is over.
//! The protocol itself (restarts, damaged archives, the long-poll) is tested in
//! `kourier-signal/tests/history_transfer.rs`.
//!
//! Link feedback L5: the phone removes the device (Linked Devices → remove) in each state of
//! the transfer (waiting, downloading, reading, delivery, the link's tail); the account ends
//! `LoggedOut`, nothing of its backend stays on disk, nothing more is told, the app starts
//! again as logged out, and "Link Again" works and receives.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_core::facade::{UiCommand, UiEvent};
use kourier_model::{AccountId, AccountState, ConnState, HistoryTransfer, LoginStepKind, Redacted};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{
    ArchiveSpec, ChatSpec, ChatWith, ContactSpec, GeneratedSpec, HistorySpec, TextSpec,
    write_archive, write_history_archive,
};
use support::harness::{DeviceInfo, SendText};
use support::{App, WAIT, World, user};

/// The core on a Signal backend whose long-poll for the archive lasts a second.
async fn start(world: &World) -> App {
    // The library's default is no offer (the app turns it on); a test of the transfer does.
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

// ---------------------------------------------------------------------------------------------
// The phone removes the device in each state of the transfer (link feedback L5)
// ---------------------------------------------------------------------------------------------

/// Where the transfer is when the phone removes the device.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// The phone has not uploaded anything yet.
    Waiting,
    /// The download stands at half the file.
    Downloading,
    /// The archive is read, slowly.
    Reading,
    /// The stage is delivered to the core, one message a row.
    Delivery,
    /// The transfer is done and the login too; the profile fetches the link queued are held
    /// by the server's rate limit.
    Tail,
}

/// The directory of the one account under `data/accounts/`.
fn account_dir(profile: &Path) -> PathBuf {
    let accounts = profile.join("data").join("accounts");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&accounts)
        .expect("accounts directory")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(dirs.len(), 1, "one account: {dirs:?}");
    dirs.remove(0)
}

/// The files that hold credentials and keys of an account.
fn secrets(dir: &Path) -> Vec<PathBuf> {
    ["backend.db", "backend.db-wal", "backend.db-shm", "key.wrap"]
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
}

/// How many messages the core has stored for the account: the newest message id of its
/// chats. A fresh account's ids run 1, 2, 3 … in the order the rows are stored and an
/// archive lists a chat's messages oldest first, so the highest "newest message" is the
/// count (a timeline page holds at most 500 rows, which would cap a sum).
async fn messages_held(app: &App, account: AccountId) -> i64 {
    use kourier_core::facade::{ConversationListQuery, Query, QueryData};
    let query = ConversationListQuery {
        account: Some(account),
        limit: 500,
        ..ConversationListQuery::default()
    };
    let QueryData::Conversations(page) = app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    else {
        panic!("unexpected answer");
    };
    page.rows
        .iter()
        .filter_map(|row| row.preview.as_ref().map(|p| p.message.0))
        .max()
        .unwrap_or(0)
}

/// "Link again" (§4.3 step 6) through the facade, the phone scanning the new code.
async fn relink(app: &mut App, world: &World, account: AccountId) -> DeviceInfo {
    app.send(UiCommand::Relink {
        account,
        flow: "qr".to_owned(),
    });
    let (txn, field) = app
        .event("the device-name step", |e| match e {
            UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                LoginStepKind::Input { fields } => Some((step.txn, fields[0].id.clone())),
                LoginStepKind::Failed { error } => panic!("relink failed: {error:?}"),
                _ => None,
            },
            _ => None,
        })
        .await;
    app.send(UiCommand::SubmitLoginInput {
        account,
        txn,
        values: vec![(field, Redacted::new("Kourier e2e".to_owned()))],
    });
    let url = app
        .event("a QR code", |e| match e {
            UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                LoginStepKind::ShowQr { data, .. } => Some(data.expose().clone()),
                LoginStepKind::Failed { error } => panic!("relink failed: {error:?}"),
                _ => None,
            },
            _ => None,
        })
        .await;
    let (device, ()) = tokio::join!(
        world
            .harness
            .complete_provision(&url, &world.phone.device_ref, WAIT),
        login_done(app, account)
    );
    let device = device.expect("completeProvision");
    app.wait_online(account).await;
    device
}

/// Waits until the account is logged out, in its row and in its connection.
async fn wait_logged_out(app: &App, account: AccountId) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let row = app.account(account).await.expect("account row");
        if matches!(row.state, AccountState::LoggedOut { .. })
            && matches!(row.conn, ConnState::LoggedOut { .. })
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never LoggedOut: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The phone removes the device while the transfer is in `phase`: the account ends
/// `LoggedOut` without a hang, the credentials, the key of the transfer and the archive leave
/// the disk, nothing is told afterwards (no `Done`, no more history), the app starts again as
/// logged out, and "Link Again" brings the account back and it receives.
async fn the_phone_removes_the_device(phase: Phase) {
    let Some(world) = World::start().await else {
        return;
    };
    if matches!(phase, Phase::Tail) {
        // Every profile fetch of the link is held for ten minutes.
        world
            .harness
            .limit_profile_fetches(Some(600))
            .await
            .expect("limitProfileFetches");
    }
    let mut network = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
        .with_history_transfer_frame_delay(match phase {
            Phase::Reading => Duration::from_millis(20),
            _ => Duration::from_millis(1),
        });
    if matches!(phase, Phase::Delivery) {
        network = network.with_history_delivery_rows(1, 1024 * 1024);
    }
    let media = world.tmp.path().join("media");
    let mut app = App::start_tuned(world.tmp.path(), &world.env, network, &media).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let aci = device.aci.parse().expect("ACI");
    let upload = |path: PathBuf| {
        let world = &world;
        let device = &device;
        async move {
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
        }
    };
    let path = world.tmp.path().join("phone-archive");
    let mut total = 0i64;
    match phase {
        Phase::Waiting => {
            transfer_state(&mut app, account, "Waiting", |s| {
                *s == HistoryTransfer::Waiting
            })
            .await;
        }
        Phase::Downloading => {
            let spec = ArchiveSpec {
                extra_messages: 400,
                ..ArchiveSpec::default()
            };
            write_archive(&path, &key, aci, &spec).expect("archive");
            let half = std::fs::metadata(&path).expect("archive").len() / 2;
            // The download breaks off after half the file and brings nothing more.
            let mut afters = vec![half];
            afters.extend(std::iter::repeat_n(0, 500));
            world
                .harness
                .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
                .await
                .expect("breakDownloads");
            upload(path.clone()).await;
            transfer_state(
                &mut app,
                account,
                "half the file",
                |s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done == half),
            )
            .await;
        }
        Phase::Reading => {
            let spec = ArchiveSpec {
                extra_messages: 3_000,
                ..ArchiveSpec::default()
            };
            write_archive(&path, &key, aci, &spec).expect("archive");
            upload(path.clone()).await;
            transfer_state(&mut app, account, "Reading", |s| {
                *s == HistoryTransfer::Reading
            })
            .await;
        }
        Phase::Delivery => {
            total = 4_000;
            let spec = HistorySpec {
                generated: Some(GeneratedSpec {
                    chats: 3,
                    messages: u32::try_from(total).expect("count"),
                    text: "A message of the generated history.".to_owned(),
                    first_sent_at: 1_700_000_000_000,
                }),
                ..HistorySpec::default()
            };
            write_history_archive(&path, &key, aci, &spec).expect("archive");
            upload(path.clone()).await;
            // The first rows are in the chats of the core.
            let deadline = tokio::time::Instant::now() + WAIT;
            while messages_held(&app, account).await < 10 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "nothing was delivered"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Phase::Tail => {
            // Ann's profile key in the archive queues a fetch of her profile.
            let spec = HistorySpec {
                contacts: vec![ContactSpec {
                    aci: Some(kourier_signal::archive_testing::generated_aci(7)),
                    profile_name: "Ann".to_owned(),
                    profile_sharing: true,
                    profile_key: Some([7; 32]),
                    ..ContactSpec::default()
                }],
                chats: vec![ChatSpec {
                    with: ChatWith::Contact(0),
                    texts: vec![TextSpec {
                        body: "hello".to_owned(),
                        sent_at: 1_700_000_000_000,
                        received_at: 1_700_000_000_002,
                        read: true,
                        ..TextSpec::default()
                    }],
                    ..ChatSpec::default()
                }],
                ..HistorySpec::default()
            };
            write_history_archive(&path, &key, aci, &spec).expect("archive");
            upload(path.clone()).await;
            transfer_state(&mut app, account, "Done", |s| *s == HistoryTransfer::Done).await;
            login_done(&mut app, account).await;
            // The tail: a profile fetch met the limit and is held for ten minutes.
            let deadline = tokio::time::Instant::now() + WAIT;
            while world
                .harness
                .limit_profile_fetches(Some(600))
                .await
                .expect("limitProfileFetches")
                == 0
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no profile fetch met the limit: the tail is not held"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    // The phone removes this device.
    let dir = account_dir(world.tmp.path());
    assert!(
        !secrets(&dir).is_empty(),
        "{phase:?}: a linked account has its backend.db"
    );
    world
        .harness
        .unlink(&world.phone.device_ref, &device.device_ref)
        .await
        .expect("unlink");
    wait_logged_out(&app, account).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !secrets(&dir).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{phase:?}: not wiped: {:?}",
            secrets(&dir)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !dir.join("transfer").exists(),
        "{phase:?}: the archive and the stage are still on disk"
    );

    // Nothing more comes: the delivery stopped where it was, no end is told, the login is
    // not done.
    while app.events.try_recv().is_ok() {}
    let held = messages_held(&app, account).await;
    if matches!(phase, Phase::Delivery) {
        println!("{phase:?}: {held} of {total} messages were delivered");
        assert!(
            held > 0 && held < total,
            "the unlink must come in the middle of the delivery: {held} of {total}"
        );
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while let Ok(event) = app.events.try_recv() {
        match event {
            UiEvent::HistoryTransfer { state, .. } => {
                assert!(
                    !matches!(state, HistoryTransfer::Done),
                    "{phase:?}: a transfer that was cut off is told as done"
                );
            }
            UiEvent::LoginStep { step, .. } => assert!(
                !matches!(step.kind, LoginStepKind::Done),
                "{phase:?}: the login is done after the unlink"
            ),
            _ => {}
        }
    }
    assert_eq!(
        messages_held(&app, account).await,
        held,
        "{phase:?}: rows arrived after the unlink"
    );

    // The app starts again: logged out, no transfer is told, nothing of the account's
    // backend is back.
    app.stop().await;
    let mut app = start(&world).await;
    wait_logged_out(&app, account).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(event) = app.events.try_recv() {
        assert!(
            !matches!(event, UiEvent::HistoryTransfer { .. }),
            "{phase:?}: a transfer is told after the restart"
        );
    }
    assert!(secrets(&dir).is_empty());

    // "Link Again": the account comes back, and it receives (receiving is not held back by
    // the transfer that was cut off).
    let relinked = relink(&mut app, &world, account).await;
    let row = app.account(account).await.expect("account row");
    assert_eq!(row.state, AccountState::Active);
    assert_eq!(row.own_device, Some(relinked.device_id));
    let text = "after the link again";
    alice_queues(&world, &relinked, text).await;
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    app.wait_text(conv, text).await;
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_removes_the_device_while_the_transfer_waits() {
    the_phone_removes_the_device(Phase::Waiting).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_removes_the_device_while_the_archive_downloads() {
    the_phone_removes_the_device(Phase::Downloading).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_removes_the_device_while_the_archive_is_read() {
    the_phone_removes_the_device(Phase::Reading).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_removes_the_device_while_the_history_is_delivered() {
    the_phone_removes_the_device(Phase::Delivery).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_removes_the_device_in_the_tail_of_the_link() {
    the_phone_removes_the_device(Phase::Tail).await;
}

/// Link feedback L6, "send at once": a message sent into an imported chat while the history's
/// delivery still fills it goes out at once (the account is `Syncing` through the whole
/// transfer), reaches Alice once, lands after every imported row of the chat and is not
/// doubled by the import.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_during_the_delivery_goes_out_once_after_the_imported_rows() {
    let Some(world) = World::start().await else {
        return;
    };
    let network = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
        .with_history_transfer_frame_delay(Duration::from_millis(1))
        .with_history_delivery_rows(1, 1024 * 1024);
    let media = world.tmp.path().join("media");
    let mut app = App::start_tuned(world.tmp.path(), &world.env, network, &media).await;
    let (account, device, key) = link_with_transfer(&mut app, &world).await;
    let me = device.device_ref.clone();
    // (The timeline query returns at most 500 rows.)
    let imported = 450u64;
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(world.alice.aci.parse().expect("Alice's ACI")),
            profile_name: "Alice".to_owned(),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: (0..imported)
                .map(|i| TextSpec {
                    body: format!("imported {i}"),
                    sent_at: 1_700_000_000_000 + i * 1000,
                    received_at: 1_700_000_000_000 + i * 1000 + 2,
                    read: true,
                    ..TextSpec::default()
                })
                .collect(),
            ..ChatSpec::default()
        }],
        generated: Some(GeneratedSpec {
            chats: 3,
            messages: 4_000,
            text: "A message of the generated history.".to_owned(),
            first_sent_at: 1_600_000_000_000,
        }),
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

    // Alice's chat is in the core while the delivery still fills it.
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    let text = "sent during the delivery";
    app.send(UiCommand::SendText {
        conv,
        text: kourier_model::Text::plain(text),
        reply_to: None,
    });
    // Our row is in the core: the rows the chat holds now (ours among them) bound what had
    // arrived when the send was made. Fewer than all the imported ones, so at least one
    // imported row of THIS chat came after the send (the delivery is one row at a time and
    // the send follows the chat's first row by milliseconds; when the machine is so fast that
    // the whole chat is in first, this fails instead of passing without proof).
    app.wait_text(conv, text).await;
    let at_send = u64::try_from(app.rows(conv).await.len()).expect("count");
    eprintln!(
        "L6: {at_send} of {imported} imported rows (and ours) were in when the send was made"
    );
    assert!(
        at_send <= imported,
        "the send was made while the chat still filled: {at_send} rows (ours and {} imported \
         of {imported}) when it was in",
        at_send - 1
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("Alice got it during the delivery");
    assert_eq!(got.text, text);
    let account_row = app.account(account).await.expect("account");
    assert!(
        matches!(account_row.conn, ConnState::Syncing { .. }),
        "sent while the account still catches up: {:?}",
        account_row.conn
    );

    transfer_state(&mut app, account, "Done", |s| *s == HistoryTransfer::Done).await;
    login_done(&mut app, account).await;
    app.wait_online(account).await;
    let rows = app.rows(conv).await;
    let ours: Vec<_> = rows
        .iter()
        .filter(|r| matches!(&r.body, kourier_model::Body::Text(t) if t.body == text))
        .collect();
    assert_eq!(ours.len(), 1, "one own row, not doubled by the import");
    assert_eq!(
        u64::try_from(rows.len()).expect("count"),
        imported + 1,
        "every imported row (the {} that came after the send among them) and ours",
        imported + 1 - at_send
    );
    // Ours sorts after every row of the chat, also those that came after it was sent.
    assert!(
        rows.iter()
            .filter(|r| r.id != ours[0].id)
            .all(|r| r.sort_key < ours[0].sort_key),
        "ours lands after the imported rows"
    );
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&me), Duration::from_millis(1500))
            .await
            .is_err(),
        "Alice got it once"
    );
    app.stop().await;
    world.shutdown().await;
}
