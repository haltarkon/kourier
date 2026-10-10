//! S15 through the real core (docs/architecture.md §4.3, §4.8, §6.4, §11 S15): the app's core
//! with the Signal backend against `@signalapp/mock-server`, driven through the facade as the
//! UI drives it, the harness playing our phone ("Myself") and a contact ("Alice").
//!
//! - [`an_unlink_wipes_the_backend_and_a_relink_keeps_the_history`]: the phone unlinks the
//!   device; the account shows `LoggedOut`, `backend.db` and the wrapped key are gone, the
//!   conversations stay readable and sending is refused; "Link again" with the same phone
//!   brings the same account back, with its history, and it decrypts what the phone and the
//!   contact send next.
//! - [`a_send_challenge_pauses_the_outbox_until_it_is_answered`]: the server asks for a
//!   captcha (428); the account shows the challenge, nothing more is tried, a wrong answer
//!   changes nothing, the right one sends everything that waited, in order.
//! - [`a_second_socket_shows_reconnect_and_system_hints_leave_it_alone`]: another client
//!   connects with our credentials (4409); the account asks the user to reconnect, a network
//!   change does not reconnect behind their back, the banner's "Reconnect" does, and so does
//!   the next start of the app.
//! - [`a_link_cancelled_at_its_last_step_leaves_no_device_on_the_phone`]: the user cancels
//!   while the link request is on its way and the server has linked the device already; the
//!   account is gone from the app and the device from the phone's list.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_core::facade::{OriginKind, UiCommand, UiEvent};
use kourier_model::{
    AccountId, AccountState, ConnState, LoginStepKind, OutgoingState, Redacted, Text, UserAction,
};
use support::harness::{DeviceInfo, SendText};
use support::{App, WAIT, World, user};

/// How long a test waits to be sure something did not happen.
const QUIET: Duration = Duration::from_millis(1500);

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

/// The files that hold credentials and keys of an account (§4.3 step 5).
fn secrets(dir: &Path) -> Vec<PathBuf> {
    ["backend.db", "backend.db-wal", "backend.db-shm", "key.wrap"]
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.exists())
        .collect()
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
        app.event("Done", |e| match e {
            UiEvent::LoginStep { account: a, step } if *a == account => match &step.kind {
                LoginStepKind::Done => Some(()),
                LoginStepKind::Failed { error } => panic!("relink failed: {error:?}"),
                _ => None,
            },
            _ => None,
        })
    );
    let device = device.expect("completeProvision");
    app.wait_online(account).await;
    world
        .harness
        .wait_for_sync(&world.phone.device_ref, &device.device_ref, WAIT)
        .await
        .expect("sync requests reached the phone");
    device
}

/// §11 S15 "Done when": mock `unlink(device)` wipes `backend.db` and shows `LoggedOut`;
/// relinking the same account decrypts `phone.sendText` and keeps old history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unlink_wipes_the_backend_and_a_relink_keeps_the_history() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    world.alice_says(&device, "before the unlink").await;
    app.wait_text(conv, "before the unlink").await;
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("my own words"),
        reply_to: None,
    });
    app.wait_row(conv, "my own words", "sent", |row| {
        matches!(row.out_state, Some(OutgoingState::Sent { .. }))
    })
    .await;
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Alice got it");
    assert_eq!(got.text, "my own words");
    let dir = account_dir(world.tmp.path());
    assert_eq!(
        secrets(&dir).len(),
        4,
        "a linked account has its backend.db (with WAL files) and wrapped key"
    );

    // The phone unlinks this device.
    world
        .harness
        .unlink(&world.phone.device_ref, &device.device_ref)
        .await
        .expect("unlink");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let row = app.account(account).await.expect("account row");
        if matches!(row.state, AccountState::LoggedOut { .. })
            && matches!(row.conn, ConnState::LoggedOut { .. })
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never LoggedOut: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Credentials, identity keys, sessions and prekeys are gone (obligation 8) …
    let deadline = tokio::time::Instant::now() + WAIT;
    while !secrets(&dir).is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "not wiped: {:?}",
            secrets(&dir)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // … the history is not.
    assert_eq!(app.rows_with_text(conv, "before the unlink").await.len(), 1);
    assert_eq!(app.rows_with_text(conv, "my own words").await.len(), 1);
    // Nothing can be sent from a device that is not linked: said at once, nothing queued.
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("into the void"),
        reply_to: None,
    });
    app.event("the refusal", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "SendText" => Some(()),
        _ => None,
    })
    .await;
    assert!(app.rows_with_text(conv, "into the void").await.is_empty());
    // What Alice writes meanwhile waits for nobody: the server has no such device.
    let lost = world
        .harness
        .send_text(&SendText::new(
            &world.alice.aci,
            &device.device_ref,
            "to a device that is gone",
        ))
        .await;
    assert!(lost.is_err(), "the unlinked device is no destination");

    // The state survives a restart of the app (no backend runs for a logged-out account).
    app.stop().await;
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let row = app.account(account).await.expect("account row");
    assert!(matches!(row.state, AccountState::LoggedOut { .. }));
    assert!(matches!(row.conn, ConnState::LoggedOut { .. }), "{row:?}");
    assert!(secrets(&dir).is_empty());

    // "Link again" with the same phone: the same account, linked anew (the mock hands the
    // freed device id out again; the keys and the password are new either way).
    let relinked = relink(&mut app, &world, account).await;
    assert_eq!(relinked.aci, world.phone.aci);
    let row = app.account(account).await.expect("account row");
    assert_eq!(row.state, AccountState::Active);
    assert_eq!(row.own_device, Some(relinked.device_id));
    assert_eq!(secrets(&dir).len(), 4, "fresh credentials and keys");

    // Old history is where it was, in the same conversation …
    assert_eq!(
        app.conversation_with(account, &user(&world.alice)).await,
        conv
    );
    assert_eq!(app.rows_with_text(conv, "before the unlink").await.len(), 1);
    let own = app.rows_with_text(conv, "my own words").await;
    assert_eq!(own.len(), 1);
    assert!(matches!(own[0].out_state, Some(OutgoingState::Sent { .. })));

    // … and new messages decrypt with the new keys: a contact's text (the mock's
    // `PrimaryDevice.sendText`, the "phone.sendText" of §11) and our own phone's transcript of
    // a message it sent to Alice.
    world.alice_says(&relinked, "after the relink").await;
    app.wait_text(conv, "after the relink").await;
    world
        .harness
        .send_sync_sent(
            &world.phone.device_ref,
            &relinked.device_ref,
            &world.alice.aci,
            "sent from the phone",
        )
        .await
        .expect("sendSyncSent");
    let from_phone = app.wait_text(conv, "sent from the phone").await;
    assert_eq!(from_phone[0].origin, OriginKind::OwnOtherDevice);

    // The relinked device sends as well.
    app.wait_cannot_send(conv, None).await;
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("back again"),
        reply_to: None,
    });
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&relinked.device_ref), WAIT)
        .await
        .expect("Alice got it");
    assert_eq!(got.text, "back again");
    app.stop().await;
    world.shutdown().await;
}

/// §11 S15 "Done when": a mock rate limit pauses the outbox until the challenge is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_challenge_pauses_the_outbox_until_it_is_answered() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    world
        .harness
        .challenge(
            &world.alice.device_ref,
            "tok-e2e",
            Some("a-person-did-this"),
        )
        .await
        .expect("challenge");

    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("first"),
        reply_to: None,
    });
    // The account says what it waits for, and where the user solves it.
    let deadline = tokio::time::Instant::now() + WAIT;
    let url = loop {
        let row = app.account(account).await.expect("account row");
        if let ConnState::NeedsUserAction(UserAction::SolveChallenge { url }) = &row.conn {
            break url.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no challenge shown: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(url, "https://signalcaptchas.org/challenge/generate.html");
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("second"),
        reply_to: None,
    });
    tokio::time::sleep(QUIET).await;
    // Paused: one refused attempt, no retries, nothing reached Alice, both messages wait.
    let answers = world
        .harness
        .challenge_answers()
        .await
        .expect("challengeAnswers");
    assert_eq!(
        answers.open.get(&world.alice.aci).map(|c| c.refused),
        Some(1),
        "the outbox does not knock again while the challenge stands"
    );
    assert!(answers.answers.is_empty());
    for text in ["first", "second"] {
        let rows = app.rows_with_text(conv, text).await;
        assert_eq!(rows.len(), 1, "{text}");
        assert!(
            matches!(
                rows[0].out_state,
                Some(OutgoingState::Queued | OutgoingState::Sending { .. })
            ),
            "{text} waits: {:?}",
            rows[0].out_state
        );
    }
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), QUIET)
            .await
            .is_err(),
        "nothing left this device"
    );

    // A wrong answer: reported, and the challenge stays.
    app.send(UiCommand::SubmitChallenge {
        account,
        answer: Redacted::new("signalcaptcha://a-robot-did-this".to_owned()),
    });
    app.event("the refused answer", |e| match e {
        UiEvent::ActionFailed { action, .. } if *action == "SubmitChallenge" => Some(()),
        _ => None,
    })
    .await;
    let row = app.account(account).await.expect("account row");
    assert!(
        matches!(
            row.conn,
            ConnState::NeedsUserAction(UserAction::SolveChallenge { .. })
        ),
        "{row:?}"
    );

    // The link the browser hands back after the captcha: accepted, and everything goes out.
    app.send(UiCommand::SubmitChallenge {
        account,
        answer: Redacted::new("signalcaptcha://a-person-did-this".to_owned()),
    });
    for text in ["first", "second"] {
        let got = world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
            .await
            .expect("Alice got it");
        assert_eq!(got.text, text, "in the order they were written");
        app.wait_row(conv, text, "sent", |row| {
            matches!(row.out_state, Some(OutgoingState::Sent { .. }))
        })
        .await;
    }
    app.wait_online(account).await;
    let answers = world
        .harness
        .challenge_answers()
        .await
        .expect("challengeAnswers");
    assert!(answers.open.is_empty());
    let accepted: Vec<_> = answers.answers.iter().filter(|a| a.accepted).collect();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].token, "tok-e2e");
    assert_eq!(accepted[0].captcha, "a-person-did-this");
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), QUIET)
            .await
            .is_err(),
        "each message once"
    );
    app.stop().await;
    world.shutdown().await;
}

/// §11 S15 "Done when": a second socket for the device gets the 4409 handling, seen through
/// the core: the account row says `NeedsUserAction(Reconnect)`, the core's network hint does
/// not make the backend reconnect, the banner's "Reconnect" (`UiCommand::RestartAccount`) does
/// and brings what was sent meanwhile, and an app that is started while the account waits
/// connects as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_socket_shows_reconnect_and_system_hints_leave_it_alone() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    assert!(
        world
            .harness
            .second_socket(&device.device_ref)
            .await
            .expect("secondSocket"),
        "the second socket took our place"
    );
    let held = ConnState::NeedsUserAction(UserAction::Reconnect);
    app.event("the reconnect request", |e| match e {
        UiEvent::ConnState { account: a, state } if *a == account && *state == held => Some(()),
        _ => None,
    })
    .await;
    let connects = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats")
        .connects;

    // The network changes: the core passes the hint on, the backend stays as it is.
    app.send(UiCommand::NetworkChanged);
    world
        .harness
        .send_text(&SendText::new(
            &world.alice.aci,
            &device.device_ref,
            "while someone else was connected",
        ))
        .await
        .expect("sendText");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let stats = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats");
    assert_eq!(
        stats.connects, connects,
        "no reconnect behind the user's back"
    );
    assert!(!stats.open);
    assert_eq!(app.account(account).await.expect("account row").conn, held);
    assert!(
        app.rows_with_text(conv, "while someone else was connected")
            .await
            .is_empty()
    );

    // The user presses "Reconnect" in the banner: one new socket, and the message is there.
    app.send(UiCommand::RestartAccount { account });
    app.wait_online(account).await;
    app.wait_text(conv, "while someone else was connected")
        .await;
    assert_eq!(
        world
            .harness
            .connection_stats(&device.device_ref)
            .await
            .expect("stats")
            .connects,
        connects + 1,
        "one reconnect"
    );
    // Pressed again while connected: nothing happens.
    app.send(UiCommand::RestartAccount { account });
    tokio::time::sleep(QUIET).await;
    let stats = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats");
    assert_eq!(stats.connects, connects + 1, "the live socket stays");
    assert!(stats.open);

    // Taken over once more, and the app is closed instead: its next start connects.
    assert!(
        world
            .harness
            .second_socket(&device.device_ref)
            .await
            .expect("secondSocket")
    );
    let deadline = tokio::time::Instant::now() + WAIT;
    while app.account(account).await.expect("account row").conn != held {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no reconnect request after the second takeover"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world
        .harness
        .send_text(&SendText::new(
            &world.alice.aci,
            &device.device_ref,
            "while the app was closed",
        ))
        .await
        .expect("sendText");
    app.stop().await;
    let app = App::start(world.tmp.path(), &world.env).await;
    app.wait_online(account).await;
    app.wait_text(conv, "while the app was closed").await;
    assert_eq!(
        app.rows_with_text(conv, "while someone else was connected")
            .await
            .len(),
        1,
        "once"
    );
    app.stop().await;
    world.shutdown().await;
}

/// §4.3 step 2: the user cancels a login in the moment it succeeds. The link request is on
/// its way, the server has linked the device, its answer is still out. The core turns the
/// actor away (`CancelLogin`, then `Logout`) and removes the account; the backend waits for
/// the answer, which alone names the device, and removes it from the phone's account. Before,
/// the cancelled task was dropped and the device stayed linked with nobody behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_cancelled_at_its_last_step_leaves_no_device_on_the_phone() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    let devices = async || {
        harness
            .list_devices(&world.phone.device_ref)
            .await
            .expect("listDevices")
            .len()
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    harness
        .delay_link(Duration::from_millis(1500))
        .await
        .expect("delayLink");
    let (account, url) = app.begin_link().await;
    {
        // The phone scans; the harness's own wait (for the device's one-time keys) is kept
        // running and must not end: the device is stopped before it uploads any.
        let provision =
            harness.complete_provision(&url, &world.phone.device_ref, Duration::from_secs(20));
        tokio::pin!(provision);
        let linked = async {
            let deadline = tokio::time::Instant::now() + WAIT;
            while devices().await != 2 {
                assert!(tokio::time::Instant::now() < deadline, "never linked");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            done = &mut provision => panic!("the device finished its link: {done:?}"),
            () = linked => {}
        }
    }

    // The server has the device, the app does not know yet: the user gives up.
    let started = tokio::time::Instant::now();
    app.send(UiCommand::CancelLogin { account });
    let deadline = started + Duration::from_secs(12);
    while app.account(account).await.is_some() || devices().await != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "account still listed: {}, devices on the phone's account: {}",
            app.account(account).await.is_some(),
            devices().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Nothing of the account is left on disk either.
    let accounts = world.tmp.path().join("data").join("accounts");
    let left: Vec<PathBuf> = std::fs::read_dir(&accounts)
        .map(|entries| entries.filter_map(|e| Some(e.ok()?.path())).collect())
        .unwrap_or_default();
    assert_eq!(left, Vec::<PathBuf>::new(), "the account directory is gone");

    // The same phone links for real afterwards.
    harness.delay_link(Duration::ZERO).await.expect("delayLink");
    let (again, device) = app.link(&world).await;
    assert_ne!(again, account);
    assert_eq!(devices().await, 2);
    assert_eq!(device.aci, world.phone.aci);
    app.stop().await;
    world.shutdown().await;
}
