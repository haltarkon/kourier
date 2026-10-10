//! S8 through the real core (docs/architecture.md §6.7, §4.8, §11 S8): the app's core with the
//! Signal backend against `@signalapp/mock-server`, driven through the facade as the UI drives
//! it, the harness playing our phone ("Myself") and a contact ("Alice").
//!
//! - [`receipts_and_read_syncs_through_the_core`]: a chat the phone has not accepted says
//!   "start this chat on your phone" and refuses sends; once accepted, Kourier's text reaches
//!   Alice sealed with a transcript to our phone; Alice's delivery and read receipts move it to
//!   Delivered and Read; reading on Kourier sends a read sync to our phone and a read receipt to
//!   Alice.
//! - [`a_read_reaches_the_phone_across_a_kill`]: the app is killed between the core's commit of
//!   the read and the backend's job (and once more right after the backend persisted the job);
//!   after a restart the phone gets the read sync exactly once.
//! - [`a_device_added_by_the_contact_still_gets_the_message`]: Alice links a device; the send
//!   repairs the device list (409) and both devices get it.
//! - [`typing_goes_both_ways`]: Alice's typing shows in the core, Kourier's typing reaches Alice.
//! - [`a_message_sent_from_the_phone_is_an_own_message`]: a sync transcript from our phone is an
//!   own message from another device, sent, in an accepted chat.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::BackendEnv;
use kourier_core::facade::{OriginKind, UiCommand, UiEvent};
use kourier_model::{AccountId, CannotSend, ConversationId, OutgoingState, Text};
use serde::{Deserialize, Serialize};
use support::harness::{DeviceInfo, SendText};
use support::{App, WAIT, World, init_tracing, user};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

/// The base64 of an ACI's 16 bytes, as `*Binary` fields appear in the harness's JSON.
fn aci_b64(aci: &str) -> String {
    let hex: String = aci.chars().filter(|c| *c != '-').collect();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    STANDARD.encode(bytes)
}

/// Alice writes to our device sealed and with her profile key (so we can seal back).
async fn alice_says_sealed(world: &World, device: &DeviceInfo, text: &str) -> u64 {
    world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&world.alice.aci, &device.device_ref, text)
        })
        .await
        .expect("sendText")
        .timestamp
}

/// The `(sender ACI base64, timestamp)` entries of a read sync's JSON.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipts_and_read_syncs_through_the_core() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = user(&world.alice);
    let me = device.device_ref.clone();

    let hello = alice_says_sealed(&world, &device, "hello from Alice").await;
    let conv = app.conversation_with(account, &alice).await;
    app.wait_text(conv, "hello from Alice").await;

    // Not accepted on the phone: the chat says so, a send fails, and Alice's sealed message
    // owes no delivery receipt (accepted chats only).
    app.wait_cannot_send(conv, Some(CannotSend::NotAccepted))
        .await;
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("too early"),
        reply_to: None,
    });
    app.wait_row(conv, "too early", "a refused send", |row| {
        matches!(
            row.out_state,
            Some(OutgoingState::Failed {
                retryable: false,
                ..
            })
        )
    })
    .await;
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&me), QUIET)
            .await
            .is_err(),
        "nothing reached Alice"
    );
    assert!(
        world
            .harness
            .wait_for_receipt(&world.alice.aci, Some(&me), QUIET)
            .await
            .is_err(),
        "no delivery receipt in an unaccepted chat"
    );

    // The phone accepts: a sealed reply, and its transcript to the phone.
    app.accept(&world, account, &device, &world.alice).await;
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("a sealed reply"),
        reply_to: None,
    });
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("Alice got the reply");
    assert_eq!(got.text, "a sealed reply");
    assert!(got.sealed, "sealed with Alice's access key");
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "sent", WAIT)
        .await
        .expect("transcript");
    let sent = &sync.sync_message["content"]["sent"];
    assert_eq!(sent["message"]["body"], "a sealed reply");
    assert_eq!(sent["timestamp"], got.timestamp);
    assert_eq!(
        sent["destinationServiceIdBinary"].as_str(),
        Some(aci_b64(&world.alice.aci).as_str())
    );
    let row = app
        .wait_row(conv, "a sealed reply", "sent", |row| {
            matches!(row.out_state, Some(OutgoingState::Sent { .. }))
        })
        .await;
    assert_eq!(row.origin, OriginKind::OwnThisDevice);
    assert_eq!(u64::try_from(row.sent_at).ok(), Some(got.timestamp));

    // Alice's phone acknowledges: Delivered, then Read.
    world
        .harness
        .send_receipt(&world.alice.aci, &me, "delivery", &[got.timestamp], true)
        .await
        .expect("delivery receipt");
    app.wait_row(conv, "a sealed reply", "delivered", |row| {
        row.out_state == Some(OutgoingState::Delivered)
    })
    .await;
    world
        .harness
        .send_receipt(&world.alice.aci, &me, "read", &[got.timestamp], true)
        .await
        .expect("read receipt");
    app.wait_row(conv, "a sealed reply", "read", |row| {
        row.out_state == Some(OutgoingState::Read)
    })
    .await;

    // A read on Kourier: a read sync to the phone, a read receipt to Alice (the phone's
    // configuration has read receipts on).
    app.send(UiCommand::MarkRead { conv });
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "read", WAIT)
        .await
        .expect("read sync");
    assert_eq!(
        read_entries(&sync.sync_message),
        [(aci_b64(&world.alice.aci), hello)]
    );
    let receipt = world
        .harness
        .wait_for_receipt(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("read receipt to Alice");
    assert_eq!(receipt.kind, "read");
    assert_eq!(receipt.timestamps, [hello]);
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_added_by_the_contact_still_gets_the_message() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    alice_says_sealed(&world, &device, "hi").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let added = world
        .harness
        .add_device(&world.alice.aci)
        .await
        .expect("addDevice");

    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("to both of Alice's devices"),
        reply_to: None,
    });
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Alice's phone");
    assert_eq!(got.text, "to both of Alice's devices");
    let envelope = world
        .harness
        .wait_for_envelope(&added.device_ref, WAIT)
        .await
        .expect("Alice's new device");
    assert_eq!(envelope.timestamp, got.timestamp);
    assert!(envelope.sealed);
    app.wait_row(conv, "to both of Alice's devices", "sent", |row| {
        matches!(row.out_state, Some(OutgoingState::Sent { .. }))
    })
    .await;
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_goes_both_ways() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    // The phone turns typing indicators on (the mock's own answer has them off).
    world
        .harness
        .send_configuration(&world.phone.device_ref, &me, true, true)
        .await
        .expect("configuration");
    alice_says_sealed(&world, &device, "hi").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let alice = user(&world.alice);

    // Alice types. The configuration and her typing take different stage-2 lanes, so she
    // repeats it (as clients do every 10 s) until Kourier shows it.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        world
            .harness
            .send_typing(&world.alice.aci, &me, "started", true)
            .await
            .expect("typing");
        let shown = tokio::time::timeout(
            Duration::from_millis(1000),
            app.event("Alice typing", |e| match e {
                UiEvent::Typing {
                    account: a,
                    conv: c,
                    user,
                    on: true,
                    ..
                } if *a == account && *c == conv && *user == alice => Some(()),
                _ => None,
            }),
        )
        .await;
        if shown.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Alice's typing never showed"
        );
    }

    // Kourier types; the core gates it on the synced setting it may still be ingesting.
    let deadline = tokio::time::Instant::now() + WAIT;
    let typing = loop {
        app.send(UiCommand::SetTyping { conv, on: true });
        match world
            .harness
            .wait_for_typing(&world.alice.aci, Some(&me), Duration::from_millis(1000))
            .await
        {
            Ok(typing) => break typing,
            Err(_) => assert!(
                tokio::time::Instant::now() < deadline,
                "Kourier's typing never reached Alice"
            ),
        }
    };
    assert_eq!(typing.action, "started");
    assert!(typing.sealed, "typing goes sealed when possible");
    assert_eq!(typing.group_id, None);
    app.send(UiCommand::SetTyping { conv, on: false });
    let stopped = world
        .harness
        .wait_for_typing(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("stopped");
    let stopped = if stopped.action == "started" {
        // A started indicator of the retry loop may still be on its way.
        world
            .harness
            .wait_for_typing(&world.alice.aci, Some(&me), WAIT)
            .await
            .expect("stopped")
    } else {
        stopped
    };
    assert_eq!(stopped.action, "stopped");
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_from_the_phone_is_an_own_message() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let sent = world
        .harness
        .send_sync_sent(
            &world.phone.device_ref,
            &device.device_ref,
            &world.alice.aci,
            "sent from my phone",
        )
        .await
        .expect("sync transcript")
        .timestamp;
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    let row = app
        .wait_row(conv, "sent from my phone", "own message", |row| {
            matches!(row.out_state, Some(OutgoingState::Sent { .. }))
        })
        .await;
    assert_eq!(row.origin, OriginKind::OwnOtherDevice);
    assert_eq!(row.author, None, "our own");
    assert_eq!(row.device, Some(world.phone.device_id));
    assert_eq!(u64::try_from(row.sent_at).ok(), Some(sent));
    // The phone's message carried our profile key: the chat is accepted.
    app.wait_cannot_send(conv, None).await;
    assert_eq!(
        app.rows_with_text(conv, "sent from my phone").await.len(),
        1
    );
    app.stop().await;
    world.shutdown().await;
}

/// Set (to a JSON [`ChildSpec`]) in the helper process of the kill test; [`read_child_process`]
/// does nothing without it.
const CHILD_ENV: &str = "KOURIER_E2E_READ_CHILD";
/// Prefix of the helper process's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-E2E-READ";

/// What the helper process runs: the app over this profile, then a read of `conv` once `text`
/// is in its timeline.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    env: BackendEnv,
    account: i64,
    conv: i64,
    text: String,
}

/// The read syncs from `me` the phone gets within `wait` that contain `entry`: the first one
/// (waiting up to `wait`), plus any other that arrives within [`QUIET`] after it. Read syncs
/// without `entry` are skipped.
async fn read_syncs_with(
    world: &World,
    me: &str,
    entry: &(String, u64),
    wait: Duration,
) -> Vec<Vec<(String, u64)>> {
    let mut found = Vec::new();
    let mut deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return found;
        }
        let Ok(sync) = world
            .harness
            .wait_for_sync_message(&world.phone.aci, Some(me), "read", left)
            .await
        else {
            return found;
        };
        let entries = read_entries(&sync.sync_message);
        if entries.contains(entry) {
            found.push(entries);
            deadline = tokio::time::Instant::now() + QUIET;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_reaches_the_phone_across_a_kill() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    world.alice_says(&device, "before").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    app.wait_text(conv, "before").await;
    app.send(UiCommand::MarkRead { conv });
    world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "read", WAIT)
        .await
        .expect("the first read sync");
    app.stop().await;

    // Between the core's commit and the backend's job; then right after the backend persisted
    // the job (its `CommandPersisted` still unsent).
    for (run, point) in [
        (1, kourier_core::FAILPOINT_AFTER_PENDING_COMMAND),
        (2, kourier_signal::failpoints::AFTER_COMMAND_PERSISTED),
    ] {
        let text = format!("read me {run}");
        // Queued while Kourier is offline; the helper app receives it.
        let sent = world
            .harness
            .send_text(&SendText::new(&world.alice.aci, &me, &text))
            .await
            .expect("sendText")
            .timestamp;
        let spec = ChildSpec {
            dir: world.tmp.path().to_owned(),
            env: world.env.clone(),
            account: account.0,
            conv: conv.0,
            text: text.clone(),
        };
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "read_child_process",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
            .env("KOURIER_FAILPOINT", format!("{point}:1"))
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
        let read = tokio::time::timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.expect("child stdout") {
                if line.trim() == format!("{CHILD_LINE} reading") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(read, Ok(true), "{point}: the helper app read the chat");
        let status = tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap_or_else(|_| panic!("{point}: the helper app did not end"))
            .expect("helper status");
        assert_eq!(
            status.signal(),
            Some(6),
            "{point}: aborted at the failpoint"
        );
        let entry = (aci_b64(&world.alice.aci), sent);
        assert!(
            read_syncs_with(&world, &me, &entry, QUIET).await.is_empty(),
            "{point}: the read did not leave before the kill"
        );

        // The next start finishes the read: the phone hears of it exactly once. (Jobs run at
        // least once: one cut off by a stop between its send and its commit runs again, so
        // repeats of earlier reads may arrive too; they are idempotent and skipped here.)
        let app = App::start(world.tmp.path(), &world.env).await;
        app.wait_online(account).await;
        let syncs = read_syncs_with(&world, &me, &entry, WAIT).await;
        assert_eq!(syncs.len(), 1, "{point}: read sync after the restart");
        assert_eq!(syncs[0], std::slice::from_ref(&entry), "{point}");
        // Alice gets her read receipt too (skipping repeats of earlier ones, as above).
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let receipt = world
                .harness
                .wait_for_receipt(&world.alice.aci, Some(&me), left)
                .await
                .unwrap_or_else(|e| panic!("{point}: read receipt after the restart: {e:?}"));
            if receipt.timestamps.contains(&sent) {
                assert_eq!(receipt.kind, "read", "{point}");
                assert_eq!(receipt.timestamps, [sent], "{point}");
                break;
            }
        }
        assert!(
            read_syncs_with(&world, &me, &entry, QUIET).await.is_empty(),
            "{point}: exactly once"
        );
        app.stop().await;
    }
    world.shutdown().await;
}

/// The helper process of the kill test: the app over the given profile, online, then a read
/// of the chat once the new message is there, until the armed failpoint aborts it.
#[test]
fn read_child_process() {
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
        let conv = ConversationId(spec.conv);
        app.wait_text(conv, &spec.text).await;
        println!("{CHILD_LINE} reading");
        app.send(UiCommand::MarkRead { conv });
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        app.stop().await;
    });
}
