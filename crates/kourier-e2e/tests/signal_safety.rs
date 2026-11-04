//! P5a Verify through the real core (refresh §5, critic H4 and B5): the app's core with the
//! Signal backend against `@signalapp/mock-server`, the harness playing our phone ("Myself")
//! and a contact ("Alice").
//!
//! - [`the_digits_equal_the_phone_side`]: Kourier's safety number of Alice equals the one her
//!   phone computes with the harness's libsignal-client from the server's keys (an oracle
//!   independent of Kourier's code), and her QR code matches the bytes of ours.
//! - [`the_verified_sync_reaches_the_phone_with_the_shown_key`]: Mark as Verified sends our
//!   phone a Verified sync naming Alice's ACI (16 bytes) and exactly the shown key (33 bytes,
//!   type byte first); Clear Verification sends DEFAULT.
//! - [`safety_gate_with_real_decryption`]: the phone's Storage record says Alice's key is
//!   UNVERIFIED (verified, then changed): a text to her waits and nothing reaches her; a
//!   Confirm that names another key than the stored one releases nothing (lead's RC2); Confirm
//!   New Safety Number with the shown key sends it and she decrypts it, and our phone gets a
//!   Verified DEFAULT sync (lead's BQ-2b: Confirm on an unverified contact clears the state).
//! - [`a_real_key_change_pauses_and_confirm_releases`]: Alice, verified, gets a new identity
//!   key on the server (a new install); Kourier learns it from a profile fetch, her state is
//!   Changed, a text to her pauses on the old session's key, and Confirm sends it to the new
//!   key with a fresh session (critic B1: the old session is archived at the refusal).
//! - [`a_default_contacts_key_change_pauses_and_confirm_only_approves`] (lead's BQ-2b, R7): the
//!   same key change for a contact who was never verified: the text pauses (Desktop's "send
//!   anyway" stop), Confirm releases it to the new key, her state stays Default and our phone
//!   hears NO Verified sync.
//!
//! Safety numbers and keys are compared in code; a failure says what differed in words.
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

use crate::support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::BackendNotice;
use kourier_core::facade::{Query, QueryData, UiCommand, UiEvent};
use kourier_model::{
    AccountId, ConversationId, IdentityFingerprint, OutgoingState, SafetyNumber, Text, UserId,
    VerifyState,
};
use support::harness::{DeviceInfo, SendText, StorageContact, StorageSpec};
use support::{App, WAIT, World, user};

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(2000);

/// The base64 of an ACI's 16 bytes, as `*Binary` fields appear in the harness's JSON.
fn aci_b64(aci: &str) -> String {
    let hex: String = aci.chars().filter(|c| *c != '-').collect();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect();
    STANDARD.encode(bytes)
}

/// A linked app whose chat with Alice is accepted and holds a session with her (she wrote
/// first, sealed, with her profile key).
async fn linked_with_alice(world: &World) -> (App, AccountId, DeviceInfo, ConversationId) {
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(world).await;
    world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&world.alice.aci, &device.device_ref, "hello from Alice")
        })
        .await
        .expect("sendText");
    let conv = app.conversation_with(account, &user(&world.alice)).await;
    app.wait_text(conv, "hello from Alice").await;
    let conv = app.accept(world, account, &device, &world.alice).await;
    (app, account, device, conv)
}

/// Kourier's answer to `Query::SafetyNumber`, asked again until `pred` holds.
async fn safety_where(
    app: &App,
    account: AccountId,
    person: &UserId,
    what: &str,
    pred: impl Fn(&SafetyNumber) -> bool,
) -> SafetyNumber {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let answer = app
            .handle
            .query(Query::SafetyNumber {
                account,
                user: person.clone(),
            })
            .await
            .expect("the safety number query");
        match answer {
            QueryData::SafetyNumber {
                answer: Ok(number), ..
            } if pred(&number) => return number,
            QueryData::SafetyNumber { .. } => {}
            other => panic!("not a safety number answer: {other:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Kourier's safety number never became {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The next Verified sync our phone got from `device`: (destination ACI binary, identity key,
/// state), all as the JSON carries them (bytes in base64; state absent = DEFAULT 0).
async fn verified_sync(world: &World, device: &DeviceInfo) -> (String, String, u64) {
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(&device.device_ref), "verified", WAIT)
        .await
        .expect("a Verified sync reached the phone");
    let verified = &sync.sync_message["content"]["verified"];
    (
        verified["destinationAciBinary"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        verified["identityKey"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        verified["state"].as_u64().unwrap_or(0),
    )
}

/// The text's own row.
async fn sent(app: &App, conv: ConversationId, text: &str) -> bool {
    app.rows_with_text(conv, text).await.iter().any(|row| {
        matches!(
            row.out_state,
            Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read)
        )
    })
}

/// Waits for the core's notice that a send waits on `person`'s new safety number.
async fn paused_on(app: &mut App, person: &UserId) {
    let person = person.clone();
    app.event("the send waits on Alice's safety number", |e| match e {
        UiEvent::Notice {
            notice: BackendNotice::IdentityChanged { user },
            ..
        } if *user == person => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_digits_equal_the_phone_side() {
    let Some(world) = World::start().await else {
        return;
    };
    let (app, account, _device, _conv) = linked_with_alice(&world).await;
    let alice = user(&world.alice);
    let ours = safety_where(&app, account, &alice, "Default", |n| {
        n.state == VerifyState::Default
    })
    .await;
    let phone_side = world
        .harness
        .safety_number(
            &world.alice.device_ref,
            &world.phone.device_ref,
            Some(ours.scannable.expose()),
        )
        .await
        .expect("Alice's side of the safety number");
    assert_eq!(ours.digits.expose().len(), 60, "60 digits");
    assert!(
        ours.digits.expose() == &phone_side.digits,
        "Kourier's 60 digits differ from those Alice's phone computes for our account"
    );
    assert_eq!(
        phone_side.matches,
        Some(true),
        "Alice's phone does not accept the QR code bytes of Kourier's side"
    );
    let ours_scannable = STANDARD.encode(ours.scannable.expose());
    assert!(
        ours_scannable != phone_side.scannable,
        "Kourier's QR code must be OUR side (local = us), not a copy of Alice's"
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_verified_sync_reaches_the_phone_with_the_shown_key() {
    let Some(world) = World::start().await else {
        return;
    };
    let (app, account, device, _conv) = linked_with_alice(&world).await;
    let alice = user(&world.alice);
    let shown = safety_where(&app, account, &alice, "Default", |n| {
        n.state == VerifyState::Default
    })
    .await;
    assert_eq!(shown.key.0.len(), 33, "the key with its type byte");
    let shown_b64 = STANDARD.encode(&shown.key.0);

    app.send(UiCommand::SetVerified {
        account,
        user: alice.clone(),
        verified: true,
        key: shown.key.clone(),
    });
    let (aci, key, state) = verified_sync(&world, &device).await;
    assert_eq!(aci, aci_b64(&world.alice.aci), "the sync names Alice's ACI");
    assert!(
        key == shown_b64,
        "the Verified sync carries another key than the one shown"
    );
    assert_eq!(state, 1, "VERIFIED");
    safety_where(&app, account, &alice, "Verified", |n| {
        n.state == VerifyState::Verified && n.key == shown.key
    })
    .await;

    app.send(UiCommand::SetVerified {
        account,
        user: alice.clone(),
        verified: false,
        key: shown.key.clone(),
    });
    let (aci, key, state) = verified_sync(&world, &device).await;
    assert_eq!(aci, aci_b64(&world.alice.aci));
    assert!(key == shown_b64, "the clear names the shown key too");
    assert_eq!(state, 0, "DEFAULT");
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), QUIET)
            .await
            .is_err(),
        "the syncs go to our phone, nothing to Alice"
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn safety_gate_with_real_decryption() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut app, account, device, conv) = linked_with_alice(&world).await;
    let alice = user(&world.alice);
    // The phone says Alice's CURRENT key was verified, then changed (UNVERIFIED).
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact {
                identity_state: Some("unverified".to_owned()),
                ..StorageContact::of(&world.alice.device_ref)
            }],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    world
        .harness
        .send_fetch_latest(
            &world.phone.device_ref,
            &device.device_ref,
            "storageManifest",
        )
        .await
        .expect("fetch latest");
    let shown = safety_where(&app, account, &alice, "Changed", |n| {
        n.state == VerifyState::Changed
    })
    .await;

    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("after the change"),
        reply_to: None,
    });
    paused_on(&mut app, &alice).await;
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), QUIET)
            .await
            .is_err(),
        "nothing reaches Alice before the confirm"
    );
    assert!(
        !sent(&app, conv, "after the change").await,
        "the text waits"
    );

    // The lead's RC2: a Confirm that names another key than the stored one (the number on
    // screen was out of date) confirms nothing: the text still waits, her state is still
    // Changed with the same key, and our phone is told nothing.
    app.send(UiCommand::ConfirmIdentity {
        account,
        user: alice.clone(),
        key: IdentityFingerprint([5].into_iter().chain([0xee; 32]).collect()),
    });
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(&device.device_ref), QUIET)
            .await
            .is_err(),
        "a confirm of a number nobody was shown releases nothing"
    );
    assert!(
        !sent(&app, conv, "after the change").await,
        "the text waits"
    );
    assert!(
        world
            .harness
            .wait_for_sync_message(
                &world.phone.aci,
                Some(&device.device_ref),
                "verified",
                Duration::from_millis(500)
            )
            .await
            .is_err(),
        "a refused confirm sends no Verified sync"
    );
    safety_where(&app, account, &alice, "still Changed", |n| {
        n.state == VerifyState::Changed && n.key == shown.key
    })
    .await;

    app.send(UiCommand::ConfirmIdentity {
        account,
        user: alice.clone(),
        key: shown.key.clone(),
    });
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Alice decrypts the text after the confirm");
    assert_eq!(got.text, "after the change");
    let (aci, key, state) = verified_sync(&world, &device).await;
    assert_eq!(aci, aci_b64(&world.alice.aci));
    assert!(
        key == STANDARD.encode(&shown.key.0),
        "the DEFAULT sync names the confirmed key"
    );
    assert_eq!(
        state, 0,
        "Confirm on an unverified contact clears to DEFAULT"
    );
    safety_where(&app, account, &alice, "Default", |n| {
        n.state == VerifyState::Default
    })
    .await;
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_key_change_pauses_and_confirm_releases() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut app, account, device, conv) = linked_with_alice(&world).await;
    let alice = user(&world.alice);
    let old = safety_where(&app, account, &alice, "Default", |n| {
        n.state == VerifyState::Default
    })
    .await;
    app.send(UiCommand::SetVerified {
        account,
        user: alice.clone(),
        verified: true,
        key: old.key.clone(),
    });
    let (_, _, state) = verified_sync(&world, &device).await;
    assert_eq!(state, 1, "Alice is verified");

    // A new install of Alice: the server answers a new identity key; the phone's record
    // carries it, and Kourier fetches her profile for it.
    let new_key = world
        .harness
        .replace_identity_key(&world.alice.device_ref)
        .await
        .expect("a new identity key for Alice");
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact::of(&world.alice.device_ref)],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    world
        .harness
        .send_fetch_latest(
            &world.phone.device_ref,
            &device.device_ref,
            "storageManifest",
        )
        .await
        .expect("fetch latest");
    let changed = safety_where(&app, account, &alice, "Changed with the new key", |n| {
        n.state == VerifyState::Changed && STANDARD.encode(&n.key.0) == new_key
    })
    .await;
    assert!(
        changed.digits.expose() != old.digits.expose(),
        "a new key gives a new safety number"
    );

    // The session still holds her old key: the send is refused and waits.
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("to the new phone"),
        reply_to: None,
    });
    paused_on(&mut app, &alice).await;
    tokio::time::sleep(QUIET).await;
    assert!(
        !sent(&app, conv, "to the new phone").await,
        "the text waits"
    );

    // Confirm: the old session was archived at the refusal, so the retry fetches her new
    // prekeys and the server takes the message (her old harness device cannot decrypt it).
    app.send(UiCommand::ConfirmIdentity {
        account,
        user: alice.clone(),
        key: changed.key.clone(),
    });
    let row = app
        .wait_row(
            conv,
            "to the new phone",
            "the text goes after the confirm",
            |row| {
                matches!(
                    row.out_state,
                    Some(OutgoingState::Sent { .. } | OutgoingState::Delivered)
                )
            },
        )
        .await;
    let failure = world
        .harness
        .wait_for_decrypt_failure(&world.alice.aci, WAIT)
        .await
        .expect("the message reached Alice's queue");
    assert_eq!(
        i64::try_from(failure.timestamp).ok(),
        Some(row.sent_at),
        "the envelope in her queue is the confirmed text"
    );
    let (aci, key, state) = verified_sync(&world, &device).await;
    assert_eq!(aci, aci_b64(&world.alice.aci));
    assert!(key == new_key, "the DEFAULT sync names the NEW key");
    assert_eq!(state, 0, "DEFAULT");
    safety_where(&app, account, &alice, "Default with the new key", |n| {
        n.state == VerifyState::Default && n.key == changed.key
    })
    .await;
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_contacts_key_change_pauses_and_confirm_only_approves() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut app, account, device, conv) = linked_with_alice(&world).await;
    let alice = user(&world.alice);
    // No Mark: Alice stays DEFAULT through the whole test.
    let old = safety_where(&app, account, &alice, "Default", |n| {
        n.state == VerifyState::Default
    })
    .await;

    // A new install of Alice: the server answers a new identity key; the phone's Storage
    // record carries it, and Kourier fetches her profile for it.
    let new_key = world
        .harness
        .replace_identity_key(&world.alice.device_ref)
        .await
        .expect("a new identity key for Alice");
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact::of(&world.alice.device_ref)],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    world
        .harness
        .send_fetch_latest(
            &world.phone.device_ref,
            &device.device_ref,
            "storageManifest",
        )
        .await
        .expect("fetch latest");
    let changed = safety_where(&app, account, &alice, "Default with the new key", |n| {
        n.state == VerifyState::Default && STANDARD.encode(&n.key.0) == new_key
    })
    .await;
    assert!(
        changed.digits.expose() != old.digits.expose(),
        "a new key gives a new safety number"
    );

    // The session still holds her old key: the send is refused and waits for the user.
    app.send(UiCommand::SendText {
        conv,
        text: Text::plain("to the new phone"),
        reply_to: None,
    });
    paused_on(&mut app, &alice).await;
    tokio::time::sleep(QUIET).await;
    assert!(
        !sent(&app, conv, "to the new phone").await,
        "the text waits for the confirm"
    );

    // Confirm on a DEFAULT contact is an approval of the new key (Desktop's `setApproved`): the
    // text goes to the new key with a fresh session.
    app.send(UiCommand::ConfirmIdentity {
        account,
        user: alice.clone(),
        key: changed.key.clone(),
    });
    let row = app
        .wait_row(
            conv,
            "to the new phone",
            "the text goes after the confirm",
            |row| {
                matches!(
                    row.out_state,
                    Some(OutgoingState::Sent { .. } | OutgoingState::Delivered)
                )
            },
        )
        .await;
    let failure = world
        .harness
        .wait_for_decrypt_failure(&world.alice.aci, WAIT)
        .await
        .expect("the message reached Alice's queue");
    assert_eq!(
        i64::try_from(failure.timestamp).ok(),
        Some(row.sent_at),
        "the envelope in her queue is the confirmed text"
    );
    // The lead's BQ-2b: an approval only. Her state stays DEFAULT (with the new key), and our
    // phone is told nothing: no Verified sync, neither DEFAULT nor any other.
    assert!(
        world
            .harness
            .wait_for_sync_message(
                &world.phone.aci,
                Some(&device.device_ref),
                "verified",
                QUIET
            )
            .await
            .is_err(),
        "an approval of a DEFAULT contact's new key sends no Verified sync"
    );
    safety_where(&app, account, &alice, "Default with the new key", |n| {
        n.state == VerifyState::Default && n.key == changed.key
    })
    .await;
    app.stop().await;
    world.shutdown().await;
}
