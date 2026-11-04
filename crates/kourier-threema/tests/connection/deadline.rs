//! TH3 fix round: the handshake's deadlines (critic M10, security review S4, protocol review F8, F10;
//! decisions P-L7, P-L9, S-M3) and a hint before the login-ack (P-L4). The connect and the D2M
//! handshake up to `ServerInfo` have 20 s of the actor's clock; from `ServerInfo` on the CSP login
//! has its own 20 s that every frame heard pushes out. A deadline that passes closes an open socket
//! with 1000 (the client's close), counts as a failure (the first backoff step is 5 s) and makes
//! exactly one attempt. The expectations are LITERALS (20_000, 25_000), never the crate's constants.

use kourier_backend_api::Command;
use kourier_model::ConnState;

use super::harness::{self, H, scene};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::d2m::{datagram, kind};

fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

fn retry_at(at: u64) -> impl Fn(&ConnState) -> bool {
    move |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(at),
        }
    }
}

/// Every connection the fake opened was closed by the CLIENT with 1000 (the first `n` of them).
fn closed_by_the_client(h: &H, n: usize) {
    let m = h.s.world.mediator();
    for conn in h.net().opened().into_iter().take(n) {
        assert_eq!(m.closed(conn), Some(1000), "not closed with 1000");
        assert_eq!(
            harness::traced(&h.s.world, conn, "in", "close"),
            1,
            "not the client's close"
        );
    }
}

#[test]
fn a_connect_that_hangs_ends_at_twenty_seconds() {
    support::offline::run_child(
        module_path!(),
        "a_connect_that_hangs_ends_at_twenty_seconds_child",
    );
}

/// DNS, TCP or TLS that never answers: at +20 s the attempt is dropped, `Offline` with the first
/// backoff step (+25 s), exactly one attempt and no socket; the retry at +25 s connects.
#[tokio::test]
async fn a_connect_that_hangs_ends_at_twenty_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(801));
    let net = h.net().clone();
    net.hold_connects(true);
    h.send(Command::Connect).await;
    h.wait_attempts(1).await;
    h.advance_to(START_MS + 20_000).await;
    h.wait_state("Offline with the first retry", retry_at(START_MS + 25_000))
        .await;
    h.sync().await;
    assert_eq!(net.attempts(), 1, "more than one attempt");
    assert!(net.opened().is_empty(), "a socket opened");
    net.hold_connects(false);
    h.advance_to(START_MS + 25_000).await;
    h.wait_state("Online", online).await;
    assert_eq!(net.attempts(), 2);
    assert_eq!(net.opened().len(), 1, "the dropped connect opened a socket");
    assert_eq!(net.made(), 1);
    h.finish().await;
}

#[test]
fn a_mediator_that_sends_no_server_hello_ends_at_twenty_seconds() {
    support::offline::run_child(
        module_path!(),
        "a_mediator_that_sends_no_server_hello_ends_at_twenty_seconds_child",
    );
}

/// The upgrade worked, the ServerHello never comes: at +20 s the client closes with 1000, `Offline`
/// with the first backoff step, one attempt; the mediator speaks again and the retry is `Online`.
#[tokio::test]
async fn a_mediator_that_sends_no_server_hello_ends_at_twenty_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(802);
    s.world.mediator().knobs(|k| k.withhold_server_hello = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_opened(1).await;
    h.advance_to(START_MS + 20_000).await;
    h.wait_state("Offline with the first retry", retry_at(START_MS + 25_000))
        .await;
    h.sync().await;
    assert_eq!(h.net().attempts(), 1, "more than one attempt");
    closed_by_the_client(&h, 1);
    h.s.world
        .mediator()
        .knobs(|k| k.withhold_server_hello = false);
    h.advance_to(START_MS + 25_000).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.net().attempts(), 2);
    assert_eq!(h.net().opened().len(), 2);
    h.finish().await;
}

#[test]
fn a_chat_server_that_never_acks_the_login_ends_twenty_seconds_after_server_info() {
    support::offline::run_child(
        module_path!(),
        "a_chat_server_that_never_acks_the_login_ends_twenty_seconds_after_server_info_child",
    );
}

/// `ServerInfo` at the start, the login-ack never: the login's own deadline, +20 s, closes with 1000;
/// `Offline` with the first backoff step; the state was never `Syncing` or `Online`; the chat server
/// answers again and the retry is `Online`.
#[tokio::test]
async fn a_chat_server_that_never_acks_the_login_ends_twenty_seconds_after_server_info_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(803);
    s.world
        .mediator()
        .knobs(|k| k.csp.withhold_login_ack = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_connected().await;
    h.advance_to(START_MS + 20_000).await;
    h.wait_state("Offline with the first retry", retry_at(START_MS + 25_000))
        .await;
    h.sync().await;
    assert_eq!(h.net().attempts(), 1, "more than one attempt");
    closed_by_the_client(&h, 1);
    h.s.world
        .mediator()
        .knobs(|k| k.csp.withhold_login_ack = false);
    h.advance_to(START_MS + 25_000).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.net().attempts(), 2);
    h.finish().await;
}

#[test]
fn the_logins_deadline_starts_at_server_info() {
    support::offline::run_child(
        module_path!(),
        "the_logins_deadline_starts_at_server_info_child",
    );
}

/// P-L7: the connect's 20 s end at `ServerInfo` and the login's own 20 s start there. The
/// `ServerHello` comes at +10 s (so does `ServerInfo`), and NOTHING follows it (Dry held, the chat
/// server silent), so no later frame moves the deadline: it is +30 s, not the connect's +20 s, and
/// then the first backoff step is +35 s.
#[tokio::test]
async fn the_logins_deadline_starts_at_server_info_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(806);
    s.world.mediator().knobs(|k| {
        k.withhold_server_hello = true;
        k.hold_dry = true;
        k.chat_server = false;
    });
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_opened(1).await;
    let conn = h.net().opened()[0];
    h.wait_deadline(START_MS + 20_000).await;
    h.advance_through(START_MS + 10_000).await;
    assert!(
        h.net().act(|m| m.release_server_hello(conn)),
        "no ServerHello was held"
    );
    h.wait_connected().await;
    h.wait_deadline(START_MS + 30_000).await;
    assert!(
        !h.clock.deadlines().contains(&(START_MS + 20_000)),
        "the connect's deadline stands after ServerInfo"
    );
    h.advance_to(START_MS + 30_000).await;
    h.wait_state("Offline with the first retry", retry_at(START_MS + 35_000))
        .await;
    h.sync().await;
    assert_eq!(h.net().attempts(), 1, "more than one attempt");
    closed_by_the_client(&h, 1);
    h.finish().await;
}

#[test]
fn every_frame_heard_during_the_login_pushes_its_deadline_out() {
    support::offline::run_child(
        module_path!(),
        "every_frame_heard_during_the_login_pushes_its_deadline_out_child",
    );
}

/// P-L7: a frame at +15 s moves the login's deadline from +20 s to +35 s (a reflection backlog in
/// progress is progress); the login-ack at +30 s is in time: `Online`, ONE connection, no failure.
#[tokio::test]
async fn every_frame_heard_during_the_login_pushes_its_deadline_out_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(804);
    s.world
        .mediator()
        .knobs(|k| k.csp.withhold_login_ack = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    h.wait_deadline(START_MS + 20_000).await;
    h.advance_through(START_MS + 15_000).await;
    h.net()
        .act(|m| m.send_raw(conn, datagram(kind::REFLECTION_QUEUE_DRY, &[])));
    h.wait_deadline(START_MS + 35_000).await;
    assert!(
        !h.clock.deadlines().contains(&(START_MS + 20_000)),
        "the old deadline stands"
    );
    h.advance_through(START_MS + 30_000).await;
    h.sync().await;
    assert!(
        !online(&h.state()),
        "online before the login-ack was released"
    );
    assert!(h.net().act(|m| m.release_login_ack(conn)), "nothing held");
    h.wait_state("Online", online).await;
    assert_eq!(h.net().attempts(), 1, "a second attempt");
    assert_eq!(h.net().opened().len(), 1);
    assert_eq!(h.s.world.mediator().closed(conn), None, "the socket closed");
    h.finish().await;
}

#[test]
fn a_hint_before_the_login_ack_changes_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_hint_before_the_login_ack_changes_nothing_child",
    );
}

/// P-L4 (protocol review F5): `NetworkChanged` and `Resumed` while the login-ack is held send no
/// echo (libthreema refuses a post-handshake payload before the login: 4010 would close the socket
/// and back off for a hint that should change nothing); released: `Online`, ONE connection, never
/// closed.
#[tokio::test]
async fn a_hint_before_the_login_ack_changes_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(805);
    s.world
        .mediator()
        .knobs(|k| k.csp.withhold_login_ack = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    h.wait_deadline(START_MS + 20_000).await;
    h.send(Command::NetworkChanged).await;
    h.send(Command::Resumed).await;
    h.sync().await;
    h.advance_through(START_MS + 10_000).await;
    h.sync().await;
    assert_eq!(
        h.s.world.mediator().closed(conn),
        None,
        "a hint before the login-ack closed the socket"
    );
    assert_eq!(h.s.world.mediator().csp_echo_requests(), 0, "an echo");
    assert!(h.net().act(|m| m.release_login_ack(conn)), "nothing held");
    h.wait_state("Online", online).await;
    assert_eq!(h.net().attempts(), 1);
    assert_eq!(h.net().opened().len(), 1);
    assert_eq!(h.s.world.mediator().closed(conn), None, "the socket closed");
    h.finish().await;
}
