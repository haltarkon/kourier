//! Calls through the whole app: the real core, the real Signal backend, the mock server
//! (docs/architecture.md §10.1, §14.2 item 15). Kourier shows a call and never takes it.
//!
//! - [`a_call_answered_on_the_phone_is_one_read_row`]: a contact's Offer makes one row that
//!   rings; the phone's `CallEvent` sync INCOMING ACCEPTED turns that row "answered": still one
//!   row, read, the chat has nothing unread and nothing was announced.
//! - [`a_call_the_caller_gave_up_is_one_unread_row_and_one_notification`]: an Offer and then the
//!   caller's Hangup NORMAL: one row, missed, unread, one notification; reading the chat sends
//!   no read receipt and no read sync for it.
//!
//! In both the mock server has seen no request of ours to send anything to anybody from before
//! the Offer to the end (`sendStats.sendRequests`: the 1:1 route of messages, receipts, call
//! signalling and sync messages, and every multi-recipient send).
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::time::Duration;

use kourier_core::facade::{
    ConversationListQuery, MessageRow, Query, QueryData, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, CallDirection, CallLine, CallMedia, CallState, ConversationId, ReadState,
};
use serde_json::json;
use support::harness::DeviceInfo;
use support::{App, WAIT, World, user};

/// How long the server is watched for a send after the last signal of a call.
const QUIET: Duration = Duration::from_millis(1500);

/// The call line of a row, if it is one.
fn call_line(row: &MessageRow) -> Option<&CallLine> {
    row.body.call_line()
}

/// The rows of `conv` that are call lines.
async fn call_rows(app: &App, conv: ConversationId) -> Vec<MessageRow> {
    app.rows(conv)
        .await
        .into_iter()
        .filter(|row| call_line(row).is_some())
        .collect()
}

/// The unread count of `conv` in the conversation list.
async fn unread(app: &App, account: AccountId, conv: ConversationId) -> u32 {
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
        QueryData::Conversations(page) => page
            .rows
            .iter()
            .find(|row| row.id == conv)
            .map(|row| row.unread)
            .expect("the conversation"),
        other => panic!("unexpected {other:?}"),
    }
}

/// Waits until our device has stopped sending (the link and the accepted request leave work),
/// and returns the number of send requests the server has seen so far.
async fn at_rest(world: &World) -> u64 {
    let requests = || async {
        world
            .harness
            .send_stats()
            .await
            .expect("sendStats")
            .send_requests
    };
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let before = requests().await;
        tokio::time::sleep(QUIET).await;
        if requests().await == before {
            return before;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the device never stopped sending"
        );
    }
}

/// Call signalling from Alice, in our device's hands before this returns.
async fn alice_signals(world: &World, device: &DeviceInfo, mut args: serde_json::Value) {
    args["from"] = json!(world.alice.aci);
    args["to"] = json!(device.device_ref);
    args["awaitDelivery"] = json!(true);
    args["timeout"] = json!(60_000);
    world.harness.send_call(args).await.expect("sendCall");
}

/// The notifications for `conv` among the UI events that have come so far.
fn notifications(app: &mut App, conv: ConversationId) -> usize {
    let mut found = 0;
    while let Ok(event) = app.events.try_recv() {
        if matches!(event, UiEvent::Notify(n) if n.conv == conv) {
            found += 1;
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_answered_on_the_phone_is_one_read_row() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = user(&world.alice);
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let sent_before = at_rest(&world).await;
    let _ = notifications(&mut app, conv);

    // Alice calls: one row that rings, by her, a video call. No notification: the phone rings.
    alice_signals(
        &world,
        &device,
        json!({ "offer": { "id": 501, "video": true } }),
    )
    .await;
    let ringing = app
        .wait_row_where(conv, "a ringing line", |row| {
            call_line(row).is_some_and(|line| matches!(line.state, CallState::Ringing { .. }))
        })
        .await;
    let line = call_line(&ringing).expect("call line");
    assert_eq!(line.direction, CallDirection::Incoming);
    assert_eq!(line.media, Some(CallMedia::Video));
    assert_eq!(line.by.as_ref(), Some(&alice));
    assert!(!line.group);
    let deadline = line.deadline().expect("ringing");
    let left = deadline - ringing.received_at;
    assert!(
        (55_000..=60_000).contains(&left),
        "about a minute from its receipt, on this computer's clock: {left}"
    );
    assert_eq!(unread(&app, account, conv).await, 0, "a ring is not unread");

    // The user answers on the phone: the same row, answered, read.
    world
        .harness
        .send_sync(json!({
            "from": world.phone.aci, "to": device.device_ref, "awaitDelivery": true,
            "callEvent": {
                "conversation": { "aci": world.alice.aci }, "callId": 501,
                "timestamp": ringing.sent_at + 4_000, "type": "video",
                "direction": "incoming", "event": "accepted",
            },
        }))
        .await
        .expect("sendSync callEvent");
    let answered = app
        .wait_row_where(conv, "an answered line", |row| {
            call_line(row).is_some_and(|line| line.state == CallState::Answered)
        })
        .await;
    assert_eq!(answered.id, ringing.id, "the row of the ring");
    let rows = call_rows(&app, conv).await;
    assert_eq!(rows.len(), 1, "one row for the call: {rows:?}");
    let line = call_line(&rows[0]).expect("call line");
    assert_eq!(line.direction, CallDirection::Incoming);
    assert_eq!(line.media, Some(CallMedia::Video));
    assert_eq!(line.by.as_ref(), Some(&alice));
    assert_ne!(rows[0].read_state, Some(ReadState::Unread), "read");
    assert_eq!(unread(&app, account, conv).await, 0);

    // Nothing was announced and nothing was sent: no Busy, no Hangup, no receipt, no sync.
    tokio::time::sleep(QUIET).await;
    assert_eq!(notifications(&mut app, conv), 0, "an answered call");
    assert_eq!(
        at_rest(&world).await,
        sent_before,
        "Kourier sent something for a call"
    );
    app.stop().await;
    world.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_the_caller_gave_up_is_one_unread_row_and_one_notification() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, device) = app.link(&world).await;
    let alice = user(&world.alice);
    let conv = app.accept(&world, account, &device, &world.alice).await;
    let sent_before = at_rest(&world).await;
    let _ = notifications(&mut app, conv);

    alice_signals(&world, &device, json!({ "offer": { "id": 502 } })).await;
    let ringing = app
        .wait_row_where(conv, "a ringing line", |row| {
            call_line(row).is_some_and(|line| matches!(line.state, CallState::Ringing { .. }))
        })
        .await;
    assert_eq!(notifications(&mut app, conv), 0, "a ring is not announced");

    // Alice gives up before anybody answers.
    alice_signals(
        &world,
        &device,
        json!({ "hangup": { "id": 502, "type": "normal" } }),
    )
    .await;
    let missed = app
        .wait_row_where(conv, "a missed line", |row| {
            call_line(row).is_some_and(|line| line.state == CallState::Missed)
        })
        .await;
    assert_eq!(missed.id, ringing.id, "the row of the ring");
    let rows = call_rows(&app, conv).await;
    assert_eq!(rows.len(), 1, "one row for the call: {rows:?}");
    let line = call_line(&rows[0]).expect("call line");
    assert_eq!(line.media, Some(CallMedia::Voice), "the Offer's kind stays");
    assert_eq!(line.by.as_ref(), Some(&alice));
    assert_eq!(rows[0].read_state, Some(ReadState::Unread));
    assert_eq!(unread(&app, account, conv).await, 1);

    // Announced once, as a message would be.
    let notified = app
        .event("the missed call's notification", |e| match e {
            UiEvent::Notify(n) if n.conv == conv => Some(n.message),
            _ => None,
        })
        .await;
    assert_eq!(notified, missed.id);
    tokio::time::sleep(QUIET).await;
    assert_eq!(notifications(&mut app, conv), 0, "once");

    // The user reads the chat: the line is read, and no read receipt or read sync goes out
    // for it (a call line is no message of the caller's).
    app.send(UiCommand::MarkRead { conv });
    app.wait_row_where(conv, "the line read", |row| {
        call_line(row).is_some() && row.read_state != Some(ReadState::Unread)
    })
    .await;
    assert_eq!(unread(&app, account, conv).await, 0);
    assert_eq!(
        at_rest(&world).await,
        sent_before,
        "Kourier sent something for a call"
    );
    app.stop().await;
    world.shutdown().await;
}
