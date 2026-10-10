//! What our phone decides for the whole account, and what the server asks of it, through the
//! real core (docs/architecture.md §6.13, §4.6): the app's core with the Signal backend against
//! `@signalapp/mock-server`, the harness playing our phone ("Myself") and a contact ("Alice").
//!
//! - [`the_phone_deletes_an_attachment_then_the_chat`]: `SyncMessage.deleteForMe` with an
//!   attachment delete removes that one attachment of a message; a conversation delete clears
//!   the history up to the messages the phone names and keeps what arrived later; a full
//!   delete takes the chat off the list until the contact writes again.
//! - [`deletes_made_here_reach_the_phone_and_the_phone_deletes_what_kourier_sent`]: the phone
//!   deletes one picture of an album Kourier sent, naming it by the client UUID Kourier gave
//!   its pointer; the user deletes the other picture, clears the chat and deletes it, and
//!   each time our phone gets the `SyncMessage.deleteForMe` Desktop would send (the
//!   attachment by its client UUID, the chat with its newest messages, a full delete, a
//!   local-only delete for a chat that held nothing).
//! - [`a_blocked_contact_is_not_heard_and_gets_no_receipt`]: after the phone blocked the
//!   contact nothing of theirs shows (a text, an edit, a reaction) and no delivery receipt
//!   goes to them; unblocked, both work again.
//! - [`a_challenge_met_by_a_receipt_shows_on_the_account`]: the server refuses a delivery
//!   receipt with 428. The account asks the user to verify, as for a challenged message; the
//!   state ends by itself at the server's retry time, when the receipt goes out; an answer
//!   the server accepts ends it at once and the receipt that waited is sent without waiting
//!   for its retry time.
//!
//! - [`one_challenge_is_one_refusal_and_its_answer_leaves_a_rate_limit_alone`]: three read
//!   receipts wait for one contact while the server challenges sends to them. The server
//!   refuses one request, not three (the others are held), the accepted answer sends all
//!   three at once, and a read sync that waits out a rate limit of its own is not sent any
//!   earlier for it.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::time::Duration;

use kourier_core::facade::{ConversationListQuery, Query, QueryData, UiCommand};
use kourier_model::{AccountId, Body, CannotSend, ConnState, ConversationId, Redacted, UserAction};
use serde_json::json;
use support::harness::{DeviceInfo, OutgoingFile, SendAttachments};
use support::{App, WAIT, World};

/// How long nothing must happen.
const QUIET: Duration = Duration::from_millis(1500);

/// A sealed text from Alice to our device, delivery awaited; returns its timestamp.
async fn alice_says_sealed(world: &World, device: &DeviceInfo, text: &str) -> u64 {
    world
        .harness
        .send_data(json!({
            "from": world.alice.aci, "to": device.device_ref, "text": text, "sealed": true,
            "withProfileKey": true, "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("sendData")
        .timestamp
}

/// A sync message from our phone to our device, delivery awaited.
async fn phone_syncs(world: &World, device: &DeviceInfo, mut args: serde_json::Value) {
    args["from"] = json!(world.phone.aci);
    args["to"] = json!(device.device_ref);
    args["awaitDelivery"] = json!(true);
    args["timeout"] = json!(60_000);
    world.harness.send_sync(args).await.expect("sendSync");
}

/// The texts of the conversation's rows, oldest first (`<files:n>` for a message with files).
async fn shown(app: &App, conv: ConversationId) -> Vec<String> {
    app.rows(conv)
        .await
        .iter()
        .filter_map(|row| match &row.body {
            Body::Text(text) => Some(text.body.clone()),
            Body::Media { attachments, .. } => Some(format!("<files:{}>", attachments.len())),
            _ => None,
        })
        .collect()
}

async fn wait_shown(app: &App, conv: ConversationId, expected: &[&str]) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = shown(app, conv).await;
        if rows == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {expected:?}, the timeline shows {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether the conversation list (inbox and archive) has `conv`.
async fn listed(app: &App, account: AccountId, conv: ConversationId) -> bool {
    for archived in [false, true] {
        let query = ConversationListQuery {
            account: Some(account),
            archived,
            limit: 100,
            ..ConversationListQuery::default()
        };
        let rows = match app
            .handle
            .query(Query::Conversations(query))
            .await
            .expect("conversations")
        {
            QueryData::Conversations(page) => page.rows,
            other => panic!("unexpected {other:?}"),
        };
        if rows.iter().any(|row| row.id == conv) {
            return true;
        }
    }
    false
}

/// A base64 string (`BASE64`, standard alphabet) as bytes in hex, for comparing ids.
fn b64_hex(value: &serde_json::Value) -> String {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_str().expect("base64 string"))
        .expect("base64");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An ACI (`xxxxxxxx-xxxx-…`) as the hex of its 16 bytes.
fn aci_hex(aci: &str) -> String {
    aci.chars().filter(|c| *c != '-').collect()
}

/// The next `deleteForMe` our phone got from our device.
async fn phone_gets_delete(world: &World, me: &str) -> serde_json::Value {
    world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(me), "deleteForMe", WAIT)
        .await
        .expect("a deleteForMe sync")
        .sync_message["content"]["deleteForMe"]
        .clone()
}

/// `(author ACI hex, sent timestamp)` of each addressed message in `list`.
fn addressed(list: &serde_json::Value) -> Vec<(String, u64)> {
    list.as_array()
        .expect("addressed messages")
        .iter()
        .map(|message| {
            (
                b64_hex(&message["author"]["authorServiceIdBinary"]),
                message["sentTimestamp"].as_u64().expect("sentTimestamp"),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deletes_made_here_reach_the_phone_and_the_phone_deletes_what_kourier_sent() {
    let Some(world) = World::start().await else {
        return;
    };
    let picked = tempfile::tempdir().expect("picked files");
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let alice = json!({ "aci": world.alice.aci });
    let alice_hex = aci_hex(&world.alice.aci);
    let own_hex = aci_hex(&world.phone.aci);

    let one = world.alice_says(&device, "one").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let two = world.alice_says(&device, "two").await;
    wait_shown(&app, conv, &["one", "two"]).await;
    app.send(UiCommand::SendText {
        conv,
        text: kourier_model::Text::plain("three"),
        reply_to: None,
    });
    let three = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("Alice gets the text")
        .timestamp;

    // --- Kourier sends two pictures as one album; the phone deletes the second, naming it
    // by the client UUID of its pointer (the name Kourier gave it before the send).
    let mut paths = Vec::new();
    for (n, (w, h)) in [(64u32, 48u32), (80, 60)].into_iter().enumerate() {
        let picture =
            kourier_media::sample_image(w, h, 7 + n as u64, kourier_media::SampleFormat::Png)
                .expect("picture");
        let path = picked.path().join(format!("picture-{n}.png"));
        std::fs::write(&path, picture).expect("write the picture");
        paths.push(path);
    }
    app.send(UiCommand::SendFiles {
        conv,
        request: Default::default(),
        paths: paths.into(),
        as_file: Vec::new(),
        caption: None,
        reply_to: None,
    });
    let album = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("Alice gets the album");
    let pointers = album.data_message["attachments"]
        .as_array()
        .expect("pointers")
        .clone();
    assert_eq!(pointers.len(), 2, "one album");
    wait_shown(&app, conv, &["one", "two", "three", "<files:2>"]).await;
    app.wait_row_where(conv, "the album", |row| {
        matches!(row.body, Body::Media { .. })
            && matches!(
                row.out_state,
                Some(kourier_model::OutgoingState::Sent { .. })
            )
    })
    .await;
    phone_syncs(
        &world,
        &device,
        json!({ "deleteForMe": { "attachments": [{
            "conversation": alice, "author": world.phone.aci, "timestamp": album.timestamp,
            "clientUuid": pointers[1]["clientUuid"],
        }]}}),
    )
    .await;
    wait_shown(&app, conv, &["one", "two", "three", "<files:1>"]).await;
    let row = app
        .wait_row_where(conv, "the album", |row| {
            matches!(row.body, Body::Media { .. })
        })
        .await;
    let Body::Media { attachments, .. } = &row.body else {
        panic!("{:?}", row.body);
    };
    assert_eq!(
        attachments[0].dims,
        Some((64, 48)),
        "the first picture stays"
    );
    assert!(row.actions.delete_attachment, "the row offers the delete");

    // --- The user deletes the other picture here: the phone is told which, by the same name.
    app.send(UiCommand::DeleteAttachment {
        attachment: attachments[0].id.expect("stored attachment"),
    });
    let delete = phone_gets_delete(&world, &me).await;
    let [attachment] = &delete["attachmentDeletes"].as_array().expect("deletes")[..] else {
        panic!("{delete}");
    };
    assert_eq!(
        b64_hex(&attachment["clientUuid"]),
        b64_hex(&pointers[0]["clientUuid"]),
        "the client UUID Alice's phone got in the pointer"
    );
    assert_eq!(
        b64_hex(&attachment["conversation"]["identifier"]["threadServiceIdBinary"]),
        alice_hex
    );
    assert_eq!(
        (
            b64_hex(&attachment["targetMessage"]["author"]["authorServiceIdBinary"]),
            attachment["targetMessage"]["sentTimestamp"].as_u64()
        ),
        (own_hex.clone(), Some(album.timestamp)),
        "our own message, by our ACI"
    );
    wait_shown(&app, conv, &["one", "two", "three"]).await;

    // --- The user clears the chat: the phone gets its newest messages, newest first, and
    // the chat stays.
    app.send(UiCommand::ClearConversation { conv });
    let delete = phone_gets_delete(&world, &me).await;
    let [chat] = &delete["conversationDeletes"].as_array().expect("deletes")[..] else {
        panic!("{delete}");
    };
    assert_eq!(
        b64_hex(&chat["conversation"]["identifier"]["threadServiceIdBinary"]),
        alice_hex
    );
    assert_eq!(
        addressed(&chat["mostRecentMessages"]),
        [
            (own_hex.clone(), three),
            (alice_hex.clone(), two),
            (alice_hex.clone(), one)
        ],
        "the album is gone already and names nothing"
    );
    // False, and present on the wire (the harness leaves an unset field out, which would
    // read as null here): false keeps the chat on Desktop, Android and iOS, and iOS drops
    // an entry whose flag is unset (`kourier-signal` `delete_sync.rs` has the sources).
    assert_eq!(chat["isFullDelete"], json!(false), "emptied, not deleted");
    assert_eq!(
        chat["mostRecentNonExpiringMessages"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "nothing disappears here: the second list would say the same"
    );
    wait_shown(&app, conv, &[]).await;
    assert!(listed(&app, account, conv).await);

    // --- Alice writes again and the user deletes the chat: a full delete up to her message.
    let four = world.alice_says(&device, "four").await;
    wait_shown(&app, conv, &["four"]).await;
    app.send(UiCommand::DeleteConversation { conv });
    let delete = phone_gets_delete(&world, &me).await;
    let chat = &delete["conversationDeletes"][0];
    assert_eq!(
        addressed(&chat["mostRecentMessages"]),
        [(alice_hex.clone(), four)]
    );
    assert_eq!(chat["isFullDelete"], json!(true));
    let deadline = tokio::time::Instant::now() + WAIT;
    while listed(&app, account, conv).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat is still listed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // --- Deleting the chat that holds nothing any more: a local-only delete, as Desktop
    // sends for a conversation without a message to name.
    app.send(UiCommand::DeleteConversation { conv });
    let delete = phone_gets_delete(&world, &me).await;
    assert_eq!(
        delete["conversationDeletes"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(
        b64_hex(
            &delete["localOnlyConversationDeletes"][0]["conversation"]["identifier"]["threadServiceIdBinary"]
        ),
        alice_hex
    );

    // Nothing waits to be sent again: after a restart the phone hears no delete twice. (The
    // job that sent the last one gets a moment to record that it is done; a job cut off
    // between the server's answer and that record runs again, which the phone tolerates.)
    tokio::time::sleep(QUIET).await;
    app.stop().await;
    let app = App::start(world.tmp.path(), &world.env).await;
    app.wait_online(account).await;
    assert!(
        world
            .harness
            .wait_for_sync_message(&world.phone.aci, Some(&me), "deleteForMe", QUIET)
            .await
            .is_err(),
        "every delete was confirmed and is not repeated"
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_deletes_an_attachment_then_the_chat() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = json!({ "aci": world.alice.aci });

    let first = world.alice_says(&device, "first").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let one = kourier_media::sample_image(64, 48, 1, kourier_media::SampleFormat::Png)
        .expect("picture one");
    let two = kourier_media::sample_image(80, 60, 2, kourier_media::SampleFormat::Png)
        .expect("picture two");
    let album = world
        .harness
        .send_attachments(&SendAttachments {
            from: world.alice.aci.clone(),
            to: device.device_ref.clone(),
            attachments: vec![
                OutgoingFile::new(&one, "image/png"),
                OutgoingFile::new(&two, "image/png"),
            ],
            await_delivery: true,
            timeout: Some(60_000),
            ..SendAttachments::default()
        })
        .await
        .expect("Alice's album");
    assert_eq!(album.pointers.len(), 2);
    let last = world.alice_says(&device, "last").await;
    wait_shown(&app, conv, &["first", "<files:2>", "last"]).await;

    // The phone deletes the second picture of the album, naming it by its digest.
    phone_syncs(
        &world,
        &device,
        json!({ "deleteForMe": { "attachments": [{
            "conversation": alice, "author": world.alice.aci, "timestamp": album.timestamp,
            "digest": album.pointers[1]["digest"],
        }]}}),
    )
    .await;
    wait_shown(&app, conv, &["first", "<files:1>", "last"]).await;
    let row = app
        .wait_row_where(conv, "the album", |row| {
            matches!(row.body, Body::Media { .. })
        })
        .await;
    let Body::Media { attachments, .. } = &row.body else {
        panic!("{:?}", row.body);
    };
    assert_eq!(
        attachments[0].dims,
        Some((64, 48)),
        "the first picture stays"
    );
    // The same delete again, and one that names nothing here, change nothing.
    phone_syncs(
        &world,
        &device,
        json!({ "deleteForMe": { "attachments": [
            { "conversation": alice, "author": world.alice.aci, "timestamp": album.timestamp,
              "digest": album.pointers[1]["digest"] },
            { "conversation": alice, "author": world.alice.aci, "timestamp": album.timestamp,
              "clientUuid": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] },
        ]}}),
    )
    .await;
    let newer = world.alice_says(&device, "newer").await;
    wait_shown(&app, conv, &["first", "<files:1>", "last", "newer"]).await;

    // The phone clears the chat as it knew it (up to "last"): what arrived later stays, and
    // so does the chat.
    phone_syncs(
        &world,
        &device,
        json!({ "deleteForMe": { "conversations": [{
            "conversation": alice, "isFullDelete": false,
            "mostRecent": [{ "author": world.alice.aci, "timestamp": last }],
            "mostRecentNonExpiring": [{ "author": world.alice.aci, "timestamp": first }],
        }]}}),
    )
    .await;
    wait_shown(&app, conv, &["newer"]).await;
    assert!(listed(&app, account, conv).await);

    // The phone deletes the chat: nothing is left and it leaves the list, with its title and
    // features kept for when it is used again.
    phone_syncs(
        &world,
        &device,
        json!({ "deleteForMe": { "conversations": [{
            "conversation": alice, "isFullDelete": true,
            "mostRecent": [{ "author": world.alice.aci, "timestamp": newer }],
        }]}}),
    )
    .await;
    wait_shown(&app, conv, &[]).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while listed(&app, account, conv).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat is still listed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(app.features(conv).await.cannot_send, None);

    // Alice writes again: the chat is back.
    world.alice_says(&device, "back again").await;
    wait_shown(&app, conv, &["back again"]).await;
    assert!(listed(&app, account, conv).await);

    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_contact_is_not_heard_and_gets_no_receipt() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let harness = &world.harness;

    world.alice_says(&device, "hello").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let one = alice_says_sealed(&world, &device, "one").await;
    wait_shown(&app, conv, &["hello", "one"]).await;
    let receipt = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("a delivery receipt for the sealed message");
    assert_eq!(
        (receipt.kind.as_str(), receipt.timestamps.as_slice()),
        ("delivery", &[one][..])
    );

    // The phone blocks Alice.
    harness
        .send_message_request_response(
            &world.phone.device_ref,
            &device.device_ref,
            &world.alice.aci,
            "block",
        )
        .await
        .expect("block");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let detail = match app
            .handle
            .query(Query::Conversation(conv))
            .await
            .expect("conversation")
        {
            QueryData::Conversation(Some(detail)) => detail,
            other => panic!("unexpected {other:?}"),
        };
        if detail.row.blocked {
            // A blocked chat cannot be written to either (the phone stopped sharing our
            // profile with it).
            assert_eq!(detail.features.cannot_send, Some(CannotSend::NotAccepted));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat never showed as blocked"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Whatever she sends now is dropped: a text, an edit of what we have, a reaction to it.
    alice_says_sealed(&world, &device, "two").await;
    harness
        .send_data(json!({
            "from": world.alice.aci, "to": me, "sealed": true, "text": "one, edited",
            "edit": { "targetTimestamp": one }, "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("edit");
    harness
        .send_data(json!({
            "from": world.alice.aci, "to": me, "sealed": true,
            "reaction": { "emoji": "👍", "targetAuthor": world.alice.aci, "targetTimestamp": one },
            "awaitDelivery": true, "timeout": 60_000,
        }))
        .await
        .expect("reaction");
    harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("the device took the envelopes");
    let silent = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), QUIET)
        .await;
    assert!(
        silent.is_err(),
        "a receipt for a blocked sender: {silent:?}"
    );
    assert_eq!(shown(&app, conv).await, ["hello", "one"]);
    let row = app.wait_row(conv, "one", "unchanged", |_| true).await;
    assert_eq!(row.revision, 0, "the edit was dropped");
    assert!(row.reactions.is_empty(), "the reaction was dropped");

    // Unblocked: she is heard again and gets her receipt, for the new message only.
    harness
        .send_message_request_response(
            &world.phone.device_ref,
            &device.device_ref,
            &world.alice.aci,
            "accept",
        )
        .await
        .expect("accept");
    app.wait_cannot_send(conv, None).await;
    let three = alice_says_sealed(&world, &device, "three").await;
    wait_shown(&app, conv, &["hello", "one", "three"]).await;
    let receipt = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("a delivery receipt again");
    assert_eq!(receipt.timestamps, [three], "nothing for what was dropped");

    app.stop().await;
    world.shutdown().await;
}

/// Waits until the account's connection state satisfies `pred`; returns how long it took.
async fn wait_conn(
    app: &App,
    account: AccountId,
    what: &str,
    pred: impl Fn(&ConnState) -> bool,
) -> Duration {
    let start = tokio::time::Instant::now();
    loop {
        let row = app.account(account).await.expect("account row");
        if pred(&row.conn) {
            return start.elapsed();
        }
        assert!(start.elapsed() < WAIT, "{what}: {:?}", row.conn);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn asks_to_verify(conn: &ConnState) -> bool {
    matches!(
        conn,
        ConnState::NeedsUserAction(UserAction::SolveChallenge { url }) if url.starts_with("https://")
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_challenge_met_by_a_receipt_shows_on_the_account() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let harness = &world.harness;
    world.alice_says(&device, "hello").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;

    // The server wants a person to vouch before it takes another send to Alice, for three
    // seconds. The delivery receipt of her next message is such a send.
    harness
        .challenge_for(&world.alice.device_ref, "receipt-challenge", 3)
        .await
        .expect("challenge");
    let one = alice_says_sealed(&world, &device, "one").await;
    wait_shown(&app, conv, &["hello", "one"]).await;
    wait_conn(&app, account, "the challenge shows", asks_to_verify).await;
    // The server lifts it; nobody answers. The state ends at the server's retry time, not
    // before, and the receipt goes out then.
    harness
        .stop_challenge(&world.alice.device_ref)
        .await
        .expect("lift");
    let early = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_millis(1200))
        .await;
    assert!(
        early.is_err(),
        "the job waits for the retry time: {early:?}"
    );
    let receipt = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), WAIT)
        .await
        .expect("the receipt after the retry time");
    assert_eq!(receipt.timestamps, [one]);
    wait_conn(
        &app,
        account,
        "online again by itself",
        ConnState::is_online,
    )
    .await;

    // Again, with a minute to wait: the user verifies, the server accepts, and the receipt
    // that waited goes out at once.
    harness
        .challenge(&world.alice.device_ref, "second-challenge", Some("solved"))
        .await
        .expect("challenge");
    let two = alice_says_sealed(&world, &device, "two").await;
    wait_conn(&app, account, "the second challenge shows", asks_to_verify).await;
    app.send(UiCommand::SubmitChallenge {
        account,
        answer: Redacted::new("signalcaptcha://solved".to_owned()),
    });
    let receipt = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_secs(20))
        .await
        .expect("the receipt right after the answer, not a minute later");
    assert_eq!(receipt.timestamps, [two]);
    wait_conn(
        &app,
        account,
        "online after the answer",
        ConnState::is_online,
    )
    .await;
    let answers = harness.challenge_answers().await.expect("answers");
    assert!(
        answers
            .answers
            .iter()
            .any(|a| a.accepted && a.token == "second-challenge"),
        "{answers:?}"
    );

    app.stop().await;
    world.shutdown().await;
}

/// A challenge a job met, and the app is closed and opened again while the server still wants
/// its captcha (it said to come back in ten minutes). The core takes its notice up from the
/// last run and shows it at the first `Online`; the backend has kept only the hold of its
/// jobs, not the challenge or its token. The held receipt is therefore tried once at that
/// `Online`: the refusal brings a token, so the banner the user sees can be answered, and
/// the receipt goes out with the answer. Before, the jobs stayed held with no token: the
/// answer was refused with "nothing is waiting", and the receipt waited the ten minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_challenge_can_be_answered_after_a_restart() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let harness = &world.harness;
    world.alice_says(&device, "hello").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let refused = async || {
        harness
            .challenge_answers()
            .await
            .expect("challengeAnswers")
            .open
            .get(&world.alice.aci)
            .map_or(0, |open| open.refused)
    };

    harness
        .challenge_for(&world.alice.device_ref, "restart-challenge", 600)
        .await
        .expect("challenge");
    let one = alice_says_sealed(&world, &device, "one").await;
    wait_shown(&app, conv, &["hello", "one"]).await;
    wait_conn(&app, account, "the challenge shows", asks_to_verify).await;
    assert_eq!(refused().await, 1, "the delivery receipt was refused");

    // The app is closed and started again.
    app.stop().await;
    let app = App::start(world.tmp.path(), &world.env).await;
    wait_conn(
        &app,
        account,
        "the challenge shows again after the restart",
        asks_to_verify,
    )
    .await;
    // The held receipt asks the server once more, which names the challenge anew.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while refused().await < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the held job was not tried after the restart"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        harness
            .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_millis(500))
            .await
            .is_err(),
        "nothing goes out while the challenge stands"
    );
    // The user verifies: the answer is taken, and the receipt goes out at once.
    app.send(UiCommand::SubmitChallenge {
        account,
        answer: Redacted::new("signalcaptcha://solved".to_owned()),
    });
    let receipt = harness
        .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_secs(20))
        .await
        .expect("the receipt right after the answer, not ten minutes later");
    assert_eq!(receipt.timestamps, [one]);
    wait_conn(
        &app,
        account,
        "online after the answer",
        ConnState::is_online,
    )
    .await;
    let answers = harness.challenge_answers().await.expect("answers");
    assert!(
        answers
            .answers
            .iter()
            .any(|a| a.accepted && a.token == "restart-challenge"),
        "{answers:?}"
    );
    assert_eq!(refused().await, 0, "the challenge is lifted");
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_challenge_is_one_refusal_and_its_answer_leaves_a_rate_limit_alone() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let me = device.device_ref.clone();
    let harness = &world.harness;
    // The phone has read receipts on (the mock's own answer has them off).
    harness
        .send_configuration(&world.phone.device_ref, &me, true, true)
        .await
        .expect("configuration");
    world.alice_says(&device, "hello").await;
    let conv = app.accept(&world, account, &device, &world.alice).await;
    wait_shown(&app, conv, &["hello"]).await;
    // Reading "hello" tells Alice and the phone; once both arrived the setting is known to
    // be on (the configuration and the message take different lanes, so it is asked again
    // until it shows).
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        app.send(UiCommand::MarkRead { conv });
        if harness
            .wait_for_receipt(&world.alice.aci, Some(&me), QUIET)
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no read receipt: the setting never arrived"
        );
        world.alice_says(&device, "hello again").await;
        app.wait_text(conv, "hello again").await;
    }
    while harness
        .wait_for_sync_message(&world.phone.aci, Some(&me), "read", QUIET)
        .await
        .is_ok()
    {}

    // The server challenges sends to Alice (a minute, unless answered), and the next send to
    // our own devices is rate-limited for eight seconds.
    harness
        .challenge(&world.alice.device_ref, "one-challenge", Some("solved"))
        .await
        .expect("challenge");
    harness
        .rate_limit(&world.phone.device_ref, Some(1), 8)
        .await
        .expect("rate limit");
    let limited_at = tokio::time::Instant::now();
    // Three messages, each read on its own: three receipt jobs for Alice, three read syncs.
    let mut stamps = Vec::new();
    for text in ["one", "two", "three"] {
        stamps.push(world.alice_says(&device, text).await);
        app.wait_text(conv, text).await;
        app.send(UiCommand::MarkRead { conv });
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    wait_conn(&app, account, "the challenge shows", asks_to_verify).await;
    tokio::time::sleep(QUIET).await;
    let refused = |answers: &support::harness::ChallengeAnswers| {
        answers
            .open
            .values()
            .map(|challenge| challenge.refused)
            .sum::<u64>()
    };
    let open = harness.challenge_answers().await.expect("answers");
    assert_eq!(
        refused(&open),
        1,
        "one challenge, one refused request: the other receipts are held, not sent into it: \
         {open:?}"
    );
    assert!(
        harness
            .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_millis(200))
            .await
            .is_err(),
        "nothing reached Alice"
    );

    // The user verifies and the server accepts: the three receipts go out at once.
    app.send(UiCommand::SubmitChallenge {
        account,
        answer: Redacted::new("signalcaptcha://solved".to_owned()),
    });
    let mut read = Vec::new();
    while read.len() < 3 {
        let receipt = harness
            .wait_for_receipt(&world.alice.aci, Some(&me), Duration::from_secs(10))
            .await
            .expect("the receipts right after the answer, not a minute later");
        assert_eq!(receipt.kind, "read");
        read.extend(receipt.timestamps);
    }
    read.sort_unstable();
    assert_eq!(read, stamps, "each message's receipt, once");
    wait_conn(
        &app,
        account,
        "online after the answer",
        ConnState::is_online,
    )
    .await;
    let answered_after = limited_at.elapsed();
    assert!(
        answered_after < Duration::from_secs(6),
        "the answer came while the rate limit still ran: {answered_after:?}"
    );

    // The read sync that met the rate limit waits out its own eight seconds: the accepted
    // answer was for the challenge, not for it. The two others went through at once.
    let first = stamps[0].to_string();
    let deadline = tokio::time::Instant::now() + WAIT;
    let waited = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sync = harness
            .wait_for_sync_message(&world.phone.aci, Some(&me), "read", left)
            .await
            .expect("the read sync of the first message");
        if sync.sync_message["read"].to_string().contains(&first) {
            break limited_at.elapsed();
        }
    };
    assert!(
        waited >= Duration::from_millis(7500),
        "the rate-limited read sync was sent after {waited:?}, before the server's eight \
         seconds were over"
    );

    app.stop().await;
    world.shutdown().await;
}
