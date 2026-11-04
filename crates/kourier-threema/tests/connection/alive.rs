//! N7: keep-alive, idle and the system's hints (brief §2.2, §2.8; decision C9; critic M4, M5).
//! After the CSP login the actor asks the chat server for an idle timeout of 30 SECONDS once per
//! connection and sends an echo every 10 s, so a connection stays up for as long as the test runs;
//! an echo not answered within 10 s ends the socket (client close 1000, then the 5 s backoff). A
//! `NetworkChanged` on an open socket sends one probe whose answer is due in 5 s; `Resumed` does
//! the same, unless the server was silent for over five minutes: that socket is replaced at once,
//! without a probe. Both clocks move to the deadlines the test expects (literals).

use std::time::Duration;

use kourier_backend_api::Command;
use kourier_model::ConnState;

use super::harness::{self, DEVICE, H, scene};
use crate::support;
use crate::support::mock::START_MS;

/// The upper bound of a wait for the fake (never a measure).
const WALL: Duration = Duration::from_secs(10);

fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

/// Connects and waits for `Online` (the login is at `START_MS`: the fake answers at once).
async fn connect_online(h: &mut H) {
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.now(), START_MS, "the clock moved during the login");
}

/// Waits (a wall BOUND) until the chat server counted `n` echo requests in all, and not more.
async fn wait_echoes(h: &H, n: usize) {
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.csp_echo_requests() >= n, WALL).await,
        "the chat server never saw {n} echo requests (it saw {})",
        m.csp_echo_requests()
    );
    assert_eq!(m.csp_echo_requests(), n, "more echo requests than expected");
}

/// Waits (a wall BOUND) until the clients asked for the idle timeouts `want`.
async fn wait_idle_requests(h: &H, want: &[u16]) {
    let m = h.s.world.mediator();
    assert!(
        h.net()
            .wait(|| m.csp_idle_requests().len() >= want.len(), WALL)
            .await,
        "the idle timeout was never asked for"
    );
    assert_eq!(m.csp_idle_requests(), want, "the idle timeouts asked for");
}

#[test]
fn the_idle_timeout_is_thirty_seconds_once_per_connection() {
    support::offline::run_child(
        module_path!(),
        "the_idle_timeout_is_thirty_seconds_once_per_connection_child",
    );
}

/// `set-connection-idle-timeout` = 30 (seconds, C9) after each login, once per connection, and the
/// chat server took it.
#[tokio::test]
async fn the_idle_timeout_is_thirty_seconds_once_per_connection_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(701));
    connect_online(&mut h).await;
    wait_idle_requests(&h, &[30]).await;
    let first = h.conn();
    assert_eq!(
        h.s.world.mediator().csp_idle_timeout(first),
        Some(Duration::from_secs(30)),
        "the chat server's timeout of the connection"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no connection to close");
    let at = START_MS + 5_000;
    h.wait_state("Offline with the retry", |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(at),
        }
    })
    .await;
    h.advance_to(at).await;
    h.wait_state("Online again", online).await;
    wait_idle_requests(&h, &[30, 30]).await;
    h.finish().await;
}

#[test]
fn echoes_keep_the_connection_up_for_thirty_minutes() {
    support::offline::run_child(
        module_path!(),
        "echoes_keep_the_connection_up_for_thirty_minutes_child",
    );
}

/// Thirty virtual minutes after the login: an echo every 10 s (180), each answered, no close of
/// either idle timer (the chat server's 30 s, the mediator's 4013), one connection; then `Resumed`
/// on that old socket that heard the server 10 s ago is a probe, not a replacement.
#[tokio::test]
async fn echoes_keep_the_connection_up_for_thirty_minutes_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(702));
    connect_online(&mut h).await;
    let conn = h.conn();
    for n in 1..=180_usize {
        let at = START_MS + 10_000 * u64::try_from(n).expect("fits");
        h.advance_to(at).await;
        wait_echoes(&h, n).await;
    }
    assert_eq!(h.now(), START_MS + 30 * 60_000);
    let m = h.s.world.mediator();
    assert_eq!(m.closed(conn), None, "the connection was closed");
    assert_eq!(h.net().opened().len(), 1, "a second connection");
    assert!(online(&h.state()), "not Online after 30 minutes");
    // The socket is 30 minutes old but heard the server 10 s ago: `Resumed` probes, no replacement.
    h.send(Command::Resumed).await;
    wait_echoes(&h, 181).await;
    h.sync().await;
    assert_eq!(
        h.net().opened().len(),
        1,
        "a socket that hears the server was replaced"
    );
    h.finish().await;
}

#[test]
fn an_unanswered_echo_ends_the_socket_after_ten_seconds() {
    support::offline::run_child(
        module_path!(),
        "an_unanswered_echo_ends_the_socket_after_ten_seconds_child",
    );
}

/// The chat server withholds the answers: the echo at +10 s stays unanswered, at +20 s the client
/// closes with 1000 and waits the first backoff step (5 s), then connects again.
#[tokio::test]
async fn an_unanswered_echo_ends_the_socket_after_ten_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(703));
    connect_online(&mut h).await;
    let first = h.conn();
    h.s.world.mediator().knobs(|k| k.csp.withhold_echo = true);
    h.advance_to(START_MS + 10_000).await;
    wait_echoes(&h, 1).await;
    h.advance_to(START_MS + 20_000).await;
    let at = START_MS + 25_000;
    h.wait_state("Offline with the retry", |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(at),
        }
    })
    .await;
    let m = h.s.world.mediator();
    assert_eq!(
        m.closed(first),
        Some(1000),
        "the client's close of the dead socket"
    );
    assert_eq!(m.csp_echo_requests(), 1, "an echo after the dead one");
    m.knobs(|k| k.csp.withhold_echo = false);
    h.advance_to(at).await;
    h.wait_state("Online again", online).await;
    assert_eq!(h.net().opened().len(), 2);
    h.finish().await;
}

#[test]
fn resumed_replaces_a_socket_silent_for_six_minutes_without_a_probe() {
    support::offline::run_child(
        module_path!(),
        "resumed_replaces_a_socket_silent_for_six_minutes_without_a_probe_child",
    );
}

/// The link went silent (a black hole: nothing comes, no timer closes it) and both clocks jump six
/// minutes in ONE step (a sleep): `Resumed` closes the old socket with 1000 and connects again at
/// once, the clock unmoved (a probe would need its 5 s).
#[tokio::test]
async fn resumed_replaces_a_socket_silent_for_six_minutes_without_a_probe_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(704));
    connect_online(&mut h).await;
    let first = h.conn();
    h.s.world.mediator().black_hole(first);
    let woke = START_MS + 6 * 60_000;
    h.net().act(|m| m.advance(Duration::from_secs(6 * 60)));
    h.clock.set(woke);
    h.send(Command::Resumed).await;
    h.wait_opened(2).await;
    h.wait_state("Online on the new socket", online).await;
    assert_eq!(h.now(), woke, "the clock moved");
    assert_eq!(
        h.s.world.mediator().closed(first),
        Some(1000),
        "the client's close of the silent socket"
    );
    assert_ne!(h.conn(), first);
    h.finish().await;
}

#[test]
fn resumed_on_a_fresh_socket_sends_one_echo_and_keeps_it() {
    support::offline::run_child(
        module_path!(),
        "resumed_on_a_fresh_socket_sends_one_echo_and_keeps_it_child",
    );
}

/// The last frame is 0 s old: `Resumed` sends ONE echo at once and keeps the socket; the schedule
/// goes on (the next echo at +20 s).
#[tokio::test]
async fn resumed_on_a_fresh_socket_sends_one_echo_and_keeps_it_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(705));
    connect_online(&mut h).await;
    h.advance_to(START_MS + 10_000).await;
    wait_echoes(&h, 1).await;
    h.send(Command::Resumed).await;
    wait_echoes(&h, 2).await;
    assert_eq!(h.now(), START_MS + 10_000, "the clock moved");
    h.advance_to(START_MS + 20_000).await;
    wait_echoes(&h, 3).await;
    assert_eq!(h.net().opened().len(), 1, "the socket was replaced");
    assert!(online(&h.state()));
    h.finish().await;
}

#[test]
fn network_changed_sends_a_probe_due_in_five_seconds() {
    support::offline::run_child(
        module_path!(),
        "network_changed_sends_a_probe_due_in_five_seconds_child",
    );
}

/// On a link that went silent, `NetworkChanged` sends one probe; its answer is due in 5 s (before
/// the next echo at +10 s): at +5 s the client closes with 1000, then connects after 5 s.
#[tokio::test]
async fn network_changed_sends_a_probe_due_in_five_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(706));
    connect_online(&mut h).await;
    let first = h.conn();
    h.s.world.mediator().black_hole(first);
    h.send(Command::NetworkChanged).await;
    h.advance_to(START_MS + 5_000).await;
    let at = START_MS + 10_000;
    h.wait_state("Offline with the retry", |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(at),
        }
    })
    .await;
    assert_eq!(
        h.s.world.mediator().closed(first),
        Some(1000),
        "the client's close after the probe"
    );
    assert!(
        harness::traced(&h.s.world, first, "in", "black-holed") >= 1,
        "the probe never went out"
    );
    h.advance_to(at).await;
    h.wait_state("Online again", online).await;
    assert_eq!(h.net().opened().len(), 2);
    h.finish().await;
}
