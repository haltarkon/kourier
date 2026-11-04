//! N6: close codes and reconnect pacing (brief §2.10, §2.11; decisions QA, C3-C5, C8, C10; critic
//! H3, M9, M11). Every close code of the spec ends the actor's connection as the close table says:
//! `1xxx`, `40xx` and an unknown code reconnect after the backoff; `4110` holds `UpdateApp`,
//! `4111`, `4112` and an unknown `41xx` hold `Reconnect` (a hint never lifts them, `Connect` tries
//! once); `4113` ends the link as `RemovedOnPhone`, `4114` / `4115` as `Unknown`, every note
//! `NetworkNote::default()`. A chat server's `close-error` decides alone. The backoff is TH2's
//! table (QA b): 5 s more per failure in a row, a connection up a minute starts the count again,
//! the eleventh failure asks, and a hint then tries ONCE. An untrusted server holds `UpdateApp`
//! with one attempt per hint and no loop. The transports are made once over all of it. The
//! expectations are literals, never computed from the crate's table.

use kourier_backend_api::Command;
use kourier_model::{ConnState, LogoutCause, UserAction};
use kourier_threema::{NetError, UntrustedKind};

use super::harness::{self, DEVICE, H, scene, traced};
use crate::support;

/// Ten virtual minutes: what "no second connection by itself" is checked over.
const TEN_MIN: u64 = 10 * 60_000;

fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

fn held(action: UserAction) -> impl Fn(&ConnState) -> bool {
    move |s| *s == ConnState::NeedsUserAction(action.clone())
}

fn retry_at(at: u64) -> impl Fn(&ConnState) -> bool {
    move |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(at),
        }
    }
}

/// Connects and waits for `Online`.
async fn connect_online(h: &mut H) {
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
}

/// The mediator closes this device's connection with `code`.
fn close(h: &H, code: u16) {
    assert!(h.net().close_device(DEVICE, code), "no connection to close");
}

/// Waits for `Offline { retry_at_ms: now + delay }`, moves both clocks there and waits for
/// `Online` again.
async fn reconnects_after(h: &mut H, delay_ms: u64) {
    let at = h.now() + delay_ms;
    h.wait_state("Offline with the retry", retry_at(at)).await;
    h.advance_to(at).await;
    h.wait_state("Online again", online).await;
}

/// Both hints and ten virtual minutes: nothing connects by itself.
async fn hints_and_ten_minutes_connect_nothing(h: &H, action: &UserAction, opened: usize) {
    h.send(Command::NetworkChanged).await;
    h.send(Command::Resumed).await;
    h.advance_idle(TEN_MIN).await;
    assert_eq!(h.net().opened().len(), opened, "a connection by itself");
    assert_eq!(
        h.state(),
        ConnState::NeedsUserAction(action.clone()),
        "the hold went away by itself"
    );
}

#[test]
fn the_codes_that_reconnect_do_so_after_five_seconds() {
    support::offline::run_child(
        module_path!(),
        "the_codes_that_reconnect_do_so_after_five_seconds_child",
    );
}

/// The spec's 1xxx and 40xx, an unknown 40xx and a code outside every class (4999: a WARN with the
/// code): `Offline { now + 5 s }`, then a second connection; the transports made once.
#[tokio::test]
async fn the_codes_that_reconnect_do_so_after_five_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let codes: [u16; 12] = [
        1000, 1001, 1011, 4000, 4001, 4009, 4010, 4011, 4012, 4013, 4099, 4999,
    ];
    for (n, code) in (0_u64..).zip(codes) {
        let mut h = harness::start(scene(601 + n));
        connect_online(&mut h).await;
        close(&h, code);
        reconnects_after(&mut h, 5_000).await;
        assert_eq!(h.net().opened().len(), 2, "after {code}");
        assert_eq!(h.net().made(), 1, "after {code}: the transports made again");
        h.finish().await;
    }
}

#[test]
fn the_codes_that_hold_wait_for_connect() {
    support::offline::run_child(module_path!(), "the_codes_that_hold_wait_for_connect_child");
}

/// 4110 holds `UpdateApp`; 4111 (C3), 4112, an unknown 41xx (C4) hold `Reconnect`: no hint and no
/// time connects; `Connect` connects once.
#[tokio::test]
async fn the_codes_that_hold_wait_for_connect_child() {
    if !support::offline::in_child() {
        return;
    }
    let table: [(u16, UserAction); 5] = [
        (4110, UserAction::UpdateApp),
        (4111, UserAction::Reconnect),
        (4112, UserAction::Reconnect),
        (4100, UserAction::Reconnect),
        (4199, UserAction::Reconnect),
    ];
    for (n, (code, action)) in (0_u64..).zip(table) {
        let mut h = harness::start(scene(621 + n));
        connect_online(&mut h).await;
        close(&h, code);
        h.wait_state("held", held(action.clone())).await;
        hints_and_ten_minutes_connect_nothing(&h, &action, 1).await;
        connect_online(&mut h).await;
        assert_eq!(h.net().opened().len(), 2, "after {code} and Connect");
        assert_eq!(h.net().made(), 1);
        h.finish().await;
    }
}

#[test]
fn the_codes_that_end_the_link_log_out() {
    support::offline::run_child(module_path!(), "the_codes_that_end_the_link_log_out_child");
}

/// C5, H3, H4: 4113 is `RemovedOnPhone`, 4114 and 4115 `Unknown`; the note is the default (the
/// words are the viewmodel's); the actor ends `Exit::LoggedOut` with the log-safe reason; one
/// connection.
#[tokio::test]
async fn the_codes_that_end_the_link_log_out_child() {
    if !support::offline::in_child() {
        return;
    }
    let table: [(u16, &str, LogoutCause); 3] = [
        (
            4113,
            "4113 dropped by another device",
            LogoutCause::RemovedOnPhone,
        ),
        (4114, "4114 dropped by the mediator", LogoutCause::Unknown),
        (
            4115,
            "4115 device slot state mismatch",
            LogoutCause::Unknown,
        ),
    ];
    for (n, (code, reason, cause)) in (0_u64..).zip(table) {
        let mut h = harness::start(scene(641 + n));
        connect_online(&mut h).await;
        close(&h, code);
        let want = ConnState::LoggedOut {
            reason: reason.to_owned(),
            cause,
            note: Default::default(),
        };
        let got = h
            .wait_state("LoggedOut", |s| matches!(s, ConnState::LoggedOut { .. }))
            .await;
        assert_eq!(got, want, "after {code}");
        assert_eq!(h.net().opened().len(), 1, "after {code}: a reconnect");
        h.finish_logged_out(reason).await;
    }
}

#[test]
fn a_close_error_decides_alone() {
    support::offline::run_child(module_path!(), "a_close_error_decides_alone_child");
}

/// `close-error` with can-reconnect 0, then 1000: held `Reconnect` (C4), no hint lifts it; with 1,
/// then 4110: a reconnect after 5 s.
#[tokio::test]
async fn a_close_error_decides_alone_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(651));
    connect_online(&mut h).await;
    let conn = h.conn();
    h.net().act(|m| m.csp_close_error(conn, false, 1000));
    h.wait_state("held", held(UserAction::Reconnect)).await;
    hints_and_ten_minutes_connect_nothing(&h, &UserAction::Reconnect, 1).await;
    h.finish().await;

    let mut h = harness::start(scene(652));
    connect_online(&mut h).await;
    let conn = h.conn();
    h.net().act(|m| m.csp_close_error(conn, true, 4110));
    reconnects_after(&mut h, 5_000).await;
    assert_eq!(h.net().opened().len(), 2);
    h.finish().await;
}

#[test]
fn what_libthreema_refuses_is_closed_with_4010_and_reconnects() {
    support::offline::run_child(
        module_path!(),
        "what_libthreema_refuses_is_closed_with_4010_and_reconnects_child",
    );
}

/// Critic M9: a datagram libthreema cannot decode (one byte: no header) -> the CLIENT closes with
/// 4010 and reconnects after 5 s. A server offering a version above V0 is no error: the client
/// chooses V0 (`MINIMUM_PROTOCOL_VERSION` is 0, so the client's own 4110 cannot happen today).
#[tokio::test]
async fn what_libthreema_refuses_is_closed_with_4010_and_reconnects_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(661);
    s.world.mediator().knobs(|k| k.server_version += 1);
    let mut h = harness::start(s);
    connect_online(&mut h).await;
    let first = h.conn();
    h.net().act(|m| m.send_raw(first, vec![0x20]));
    reconnects_after(&mut h, 5_000).await;
    assert_eq!(h.s.world.mediator().closed(first), Some(4010));
    assert_eq!(
        traced(&h.s.world, first, "in", "close"),
        1,
        "not the client's close"
    );
    assert_eq!(h.net().opened().len(), 2);
    h.finish().await;
}

#[test]
fn the_backoff_grows_by_five_seconds_and_a_minute_up_starts_again() {
    support::offline::run_child(
        module_path!(),
        "the_backoff_grows_by_five_seconds_and_a_minute_up_starts_again_child",
    );
}

/// M11: three failed connects (server not reached, certificate dates (C8), not reached) wait 5,
/// 10, 15 s; a login closed at once is no proof (G8): 20 s; a connection up 70 s starts the count
/// again: 5 s. One maker call over all.
#[tokio::test]
async fn the_backoff_grows_by_five_seconds_and_a_minute_up_starts_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(671));
    let net = h.net().clone();
    net.fail_connects(Some(NetError::Failed("websocket")));
    h.send(Command::Connect).await;
    let mut at = h.now();
    for (n, (delay, next_error)) in [
        (5_000, Some(NetError::CertificateDates)),
        (10_000, Some(NetError::Failed("websocket"))),
        (15_000, None),
    ]
    .into_iter()
    .enumerate()
    {
        h.wait_attempts(n + 1).await;
        at += delay;
        h.wait_state("Offline with the retry", retry_at(at)).await;
        net.fail_connects(next_error);
        h.advance_to(at).await;
    }
    h.wait_state("Online", online).await;
    assert_eq!(net.opened().len(), 1);
    close(&h, 4000);
    reconnects_after(&mut h, 20_000).await;
    let up = h.now();
    for n in 1..=7 {
        h.advance_to(up + n * 10_000).await;
    }
    h.sync().await;
    assert!(online(&h.state()), "the connection did not stay up");
    close(&h, 4000);
    reconnects_after(&mut h, 5_000).await;
    assert_eq!(net.opened().len(), 3);
    assert_eq!(net.made(), 1);
    h.finish().await;
}

#[test]
fn ten_failures_ask_and_a_hint_then_tries_once() {
    support::offline::run_child(
        module_path!(),
        "ten_failures_ask_and_a_hint_then_tries_once_child",
    );
}

/// QA (b): ten failures in a row wait 5 ... 50 s; the eleventh holds `Reconnect`; ten minutes
/// connect nothing; a `NetworkChanged` tries once (it fails: held again), a `Resumed` once more
/// (the server is back: `Online`).
#[tokio::test]
async fn ten_failures_ask_and_a_hint_then_tries_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(681));
    let net = h.net().clone();
    net.fail_connects(Some(NetError::Failed("websocket")));
    h.send(Command::Connect).await;
    let mut at = h.now();
    for n in 1..=10_u64 {
        h.wait_attempts(usize::try_from(n).expect("small")).await;
        at += 5_000 * n;
        h.wait_state("Offline with the retry", retry_at(at)).await;
        h.advance_to(at).await;
    }
    h.wait_attempts(11).await;
    h.wait_state("held", held(UserAction::Reconnect)).await;
    h.advance_idle(TEN_MIN).await;
    assert_eq!(net.attempts(), 11, "a connect while held");
    h.send(Command::NetworkChanged).await;
    h.wait_attempts(12).await;
    h.wait_state("held again", held(UserAction::Reconnect))
        .await;
    h.sync().await;
    assert_eq!(net.attempts(), 12, "the hint tried more than once");
    net.fail_connects(None);
    h.send(Command::Resumed).await;
    h.wait_state("Online", online).await;
    assert_eq!(net.attempts(), 13);
    assert_eq!(net.made(), 1);
    h.finish().await;
}

#[test]
fn a_hint_during_the_wait_connects_at_once() {
    support::offline::run_child(
        module_path!(),
        "a_hint_during_the_wait_connects_at_once_child",
    );
}

/// `NetworkChanged` while the backoff waits: connected at once, the clocks not moved.
#[tokio::test]
async fn a_hint_during_the_wait_connects_at_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(691));
    let net = h.net().clone();
    net.fail_connects(Some(NetError::Failed("websocket")));
    h.send(Command::Connect).await;
    let start = h.now();
    h.wait_state("Offline with the retry", retry_at(start + 5_000))
        .await;
    net.fail_connects(None);
    h.send(Command::NetworkChanged).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.now(), start, "the clock moved");
    assert_eq!(net.attempts(), 2);
    h.finish().await;
}

#[test]
fn an_untrusted_server_holds_and_each_hint_tries_once() {
    support::offline::run_child(
        module_path!(),
        "an_untrusted_server_holds_and_each_hint_tries_once_child",
    );
}

/// QB, C8: `Untrusted(PinMismatch)` holds `UpdateApp`: no loop over 30 virtual minutes; each
/// `NetworkChanged` / `Resumed` tries once (another network may not intercept); the pins hold
/// again: `Online`. One maker call.
#[tokio::test]
async fn an_untrusted_server_holds_and_each_hint_tries_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(695));
    let net = h.net().clone();
    net.fail_connects(Some(NetError::Untrusted(UntrustedKind::PinMismatch)));
    h.send(Command::Connect).await;
    h.wait_attempts(1).await;
    h.wait_state("held", held(UserAction::UpdateApp)).await;
    h.advance_idle(30 * 60_000).await;
    assert_eq!(net.attempts(), 1, "a loop of attempts");
    for (n, hint) in [Command::NetworkChanged, Command::Resumed]
        .into_iter()
        .enumerate()
    {
        h.send(hint).await;
        h.wait_attempts(n + 2).await;
        h.wait_state("held again", held(UserAction::UpdateApp))
            .await;
    }
    h.sync().await;
    assert_eq!(net.attempts(), 3);
    net.fail_connects(None);
    h.send(Command::NetworkChanged).await;
    h.wait_state("Online", online).await;
    assert_eq!(net.attempts(), 4);
    assert_eq!(net.made(), 1);
    h.finish().await;
}

#[test]
fn the_users_connect_after_the_question_starts_a_new_series() {
    support::offline::run_child(
        module_path!(),
        "the_users_connect_after_the_question_starts_a_new_series_child",
    );
}

/// P-L6 (protocol review F7; his QA: the account "stops and shows Reconnect"; the spec: "user
/// interaction should be required to continue reconnecting"): ten failures ask. A hint then makes ONE
/// attempt and a failure asks again at once. The user's own `Connect` starts a NEW series: its failure
/// waits 5 s (not the question again), the next 10 s.
#[tokio::test]
async fn the_users_connect_after_the_question_starts_a_new_series_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(821));
    let net = h.net().clone();
    net.fail_connects(Some(NetError::Failed("websocket")));
    h.send(Command::Connect).await;
    let mut at = h.now();
    for n in 1..=10_u64 {
        h.wait_attempts(usize::try_from(n).expect("small")).await;
        at += 5_000 * n;
        h.wait_state("Offline with the retry", retry_at(at)).await;
        h.advance_to(at).await;
    }
    h.wait_attempts(11).await;
    h.wait_state("held", held(UserAction::Reconnect)).await;
    // A hint: one attempt, and the question again at once.
    h.send(Command::NetworkChanged).await;
    h.wait_attempts(12).await;
    h.wait_state("held again", held(UserAction::Reconnect))
        .await;
    h.sync().await;
    assert_eq!(net.attempts(), 12, "the hint tried more than once");
    // The user's own word: a new series.
    h.send(Command::Connect).await;
    h.wait_attempts(13).await;
    let first = h.now() + 5_000;
    h.wait_state("the first step of a new series", retry_at(first))
        .await;
    h.advance_to(first).await;
    h.wait_attempts(14).await;
    let second = h.now() + 10_000;
    h.wait_state("the second step", retry_at(second)).await;
    net.fail_connects(None);
    h.advance_to(second).await;
    h.wait_state("Online", online).await;
    assert_eq!(net.attempts(), 15);
    assert_eq!(net.made(), 1);
    h.finish().await;
}
