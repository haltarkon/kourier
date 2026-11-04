//! N5: the clock rule (brief §2.7, decision C2, critic H1). A server whose time is more than 20
//! minutes from the actor's clock closes the connection with 1000 and holds
//! `NeedsUserAction(FixClock { skew_ms })` with `skew_ms` = local minus server (the fake ADDS its
//! knob to the server's time: knob +21 min is a skew of -21 min). The slot state is written first.
//! Exactly 20 minutes is fine. A plain `Connect` and a hint measure again; only the process-wide
//! "Connect Anyway" of the account (`kourier_threema::connect_anyway`, set by nothing in TH3 but
//! these tests) connects through a wrong clock, for every later connection and actor of the
//! account in this process, not for another account.

use kourier_backend_api::Command;
use kourier_model::{ConnState, UserAction};
use kourier_threema::store::SlotState;

use super::harness::{self, H, scene};
use crate::support;

const MIN: i64 = 60_000;

fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

fn fix_clock(s: &ConnState) -> bool {
    matches!(s, ConnState::NeedsUserAction(UserAction::FixClock { .. }))
}

/// The skew of the held state (panics on another state).
fn held_skew(h: &H) -> i64 {
    match h.state() {
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms }) => skew_ms,
        other => panic!("not held for the clock: {other:?}"),
    }
}

fn assert_skew_near(got: i64, want: i64) {
    assert!(
        (got - want).abs() <= 1_000,
        "the skew {got} ms is not {want} ms +- 1 s"
    );
}

/// How many CSP logins reached the fake on the connection (TH3 fix P-L2: the mediator leg's clock
/// check closes BEFORE the client logs into the chat server, so a skew of the mediator alone
/// reaches no chat server login; the chat server leg's check comes after the login).
fn csp_logins(h: &H, conn: crate::support::mock::d2m::ConnId) -> usize {
    harness::traced(&h.s.world, conn, "in", "csp-login")
}

/// Every connection the fake saw was closed by the CLIENT with 1000.
fn all_closed_by_the_client(h: &H) {
    let m = h.s.world.mediator();
    for conn in h.net().opened() {
        assert_eq!(m.closed(conn), Some(1000), "not closed with 1000");
        assert_eq!(
            harness::traced(&h.s.world, conn, "in", "close"),
            1,
            "not the client's close"
        );
    }
}

/// Connects (the `n`th connection in all) and waits for the hold.
async fn connect_held(h: &mut H, n: usize) {
    h.send(Command::Connect).await;
    h.wait_opened(n).await;
    h.wait_state("FixClock", fix_clock).await;
}

#[test]
fn a_server_21_minutes_ahead_holds_with_a_negative_skew() {
    support::offline::run_child(
        module_path!(),
        "a_server_21_minutes_ahead_holds_with_a_negative_skew_child",
    );
}

/// H1: knob +21 min -> -1_260_000; closed 1000; the file `existing` (written before the rule
/// closes, critic L14); 30 virtual minutes go by with nothing armed and no second connection.
#[tokio::test]
async fn a_server_21_minutes_ahead_holds_with_a_negative_skew_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(511);
    s.world.mediator().knobs(|k| k.clock_skew_ms = 21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    assert_skew_near(held_skew(&h), -21 * MIN);
    all_closed_by_the_client(&h);
    let conn = h.net().opened()[0];
    assert_eq!(
        csp_logins(&h, conn),
        0,
        "the client logged into the chat server with the mediator's clock off"
    );
    assert_eq!(h.file_slot(), SlotState::Existing, "the slot state first");
    h.advance_idle(30 * 60_000).await;
    assert_eq!(h.net().opened().len(), 1, "a connection while held");
    assert!(fix_clock(&h.state()), "the hold went away by itself");
    h.finish().await;
}

#[test]
fn a_server_21_minutes_behind_holds_with_a_positive_skew() {
    support::offline::run_child(
        module_path!(),
        "a_server_21_minutes_behind_holds_with_a_positive_skew_child",
    );
}

#[tokio::test]
async fn a_server_21_minutes_behind_holds_with_a_positive_skew_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(512);
    s.world.mediator().knobs(|k| k.clock_skew_ms = -21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    assert_skew_near(held_skew(&h), 21 * MIN);
    all_closed_by_the_client(&h);
    let conn = h.net().opened()[0];
    assert_eq!(
        csp_logins(&h, conn),
        0,
        "the client logged into the chat server with the mediator's clock off"
    );
    h.finish().await;
}

#[test]
fn exactly_twenty_minutes_is_fine() {
    support::offline::run_child(module_path!(), "exactly_twenty_minutes_is_fine_child");
}

/// Both servers exactly 20 minutes off (each way once): `Online`.
#[tokio::test]
async fn exactly_twenty_minutes_is_fine_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(513);
    s.world.mediator().knobs(|k| {
        k.clock_skew_ms = 20 * MIN;
        k.csp.clock_skew_ms = -40 * MIN;
    });
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.net().opened().len(), 1);
    h.finish().await;
}

#[test]
fn the_chat_servers_clock_alone_also_holds() {
    support::offline::run_child(
        module_path!(),
        "the_chat_servers_clock_alone_also_holds_child",
    );
}

/// The mediator's time right, the login-ack's 21 minutes behind: held at the login-ack, never
/// `Syncing`.
#[tokio::test]
async fn the_chat_servers_clock_alone_also_holds_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(514);
    s.world
        .mediator()
        .knobs(|k| k.csp.clock_skew_ms = -21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    assert_skew_near(held_skew(&h), 21 * MIN);
    all_closed_by_the_client(&h);
    // The contrast of P-L2: this check comes AFTER the login, which reached the fake.
    let conn = h.net().opened()[0];
    assert_eq!(csp_logins(&h, conn), 1, "no CSP login before the login-ack");
    h.finish().await;
}

#[test]
fn connect_measures_again_and_only_connect_anyway_goes_through() {
    support::offline::run_child(
        module_path!(),
        "connect_measures_again_and_only_connect_anyway_goes_through_child",
    );
}

/// C2: a plain `Connect` measures again (held again); with the account's "Connect Anyway" it is
/// `Online`, stays `Online` across a 4000 close and its reconnect, and across a NEW actor of the
/// same account; another account in the same process is still held.
#[tokio::test]
async fn connect_measures_again_and_only_connect_anyway_goes_through_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(515);
    s.world.mediator().knobs(|k| k.clock_skew_ms = 21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    connect_held(&mut h, 2).await;
    assert_skew_near(held_skew(&h), -21 * MIN);
    kourier_threema::connect_anyway(h.uuid());
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.net().opened().len(), 3);
    assert!(h.net().close_device(harness::DEVICE, 4000), "no connection");
    let retry = h.now() + 5_000;
    h.wait_state("Offline with a retry", |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(retry),
        }
    })
    .await;
    h.advance_to(retry).await;
    h.wait_state("Online again", online).await;
    assert_eq!(h.net().opened().len(), 4);
    let mut h = h.restart().await;
    h.send(Command::Connect).await;
    h.wait_state("Online after a restart", online).await;
    assert_eq!(h.net().opened().len(), 5);
    h.finish().await;
    let other = scene(516);
    other.world.mediator().knobs(|k| k.clock_skew_ms = 21 * MIN);
    let mut o = harness::start(other);
    connect_held(&mut o, 1).await;
    o.finish().await;
}

#[test]
fn a_new_actor_measures_again() {
    support::offline::run_child(module_path!(), "a_new_actor_measures_again_child");
}

/// Without "Connect Anyway" a restarted actor of the same file is held again.
#[tokio::test]
async fn a_new_actor_measures_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(517);
    s.world.mediator().knobs(|k| k.clock_skew_ms = 21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    let mut h = h.restart().await;
    connect_held(&mut h, 2).await;
    assert_skew_near(held_skew(&h), -21 * MIN);
    h.finish().await;
}

#[test]
fn a_wake_while_held_measures_once() {
    support::offline::run_child(module_path!(), "a_wake_while_held_measures_once_child");
}

/// `Resumed` while held: one more connection (still held while the clock is wrong); with the
/// server's clock right again the next `NetworkChanged` reaches `Online`.
#[tokio::test]
async fn a_wake_while_held_measures_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(518);
    s.world.mediator().knobs(|k| k.clock_skew_ms = 21 * MIN);
    let mut h = harness::start(s);
    connect_held(&mut h, 1).await;
    h.send(Command::Resumed).await;
    h.wait_opened(2).await;
    h.wait_state("FixClock", fix_clock).await;
    h.sync().await;
    assert_eq!(h.net().opened().len(), 2, "more than one attempt");
    h.s.world.mediator().knobs(|k| k.clock_skew_ms = 0);
    h.send(Command::NetworkChanged).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.net().opened().len(), 3);
    h.finish().await;
}
