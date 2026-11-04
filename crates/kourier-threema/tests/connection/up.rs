//! N4: the connection up and down. `Connect` with a stored identity reaches `Online`; `Syncing` holds
//! while the mediator's Dry is held (critic H2: stable states only, never a sequence read from the
//! watch); the ClientHello and the CSP login carry the stored device; a reflected envelope is acked
//! once it is durable and not sent again (TH5 A5a; TH3 dropped it un-acked); a promoted Kourier never asks for messages; `Disconnect` and `Shutdown` close with 1000;
//! an account without an identity makes nothing.

use kourier_backend_api::Command;
use kourier_model::ConnState;
use libthreema::protobuf::{d2d, d2m};
use prost::Message as _;

use super::harness::{self, DEVICE, H, LABEL, ME, SERVER_GROUP, scene, traced};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body;
use crate::support::mock::crypto::open_nonce_ahead;
use crate::support::mock::csp::SeenCookie;
use crate::support::mock::d2m::HelloSeen;
use crate::support::mock::envelope::{Author, Msg};

/// The first echo of a connection that logged in at the start goes out 10 s later.
const FIRST_ECHO: u64 = START_MS + 10_000;

fn online(detail: &str) -> impl Fn(&ConnState) -> bool + '_ {
    move |s| matches!(s, ConnState::Online { detail: Some(d) } if d == detail)
}

fn syncing(s: &ConnState) -> bool {
    matches!(s, ConnState::Syncing { .. })
}

/// Connects and waits for `Online` with `detail`.
async fn connect(h: &mut H, detail: &str) {
    h.send(Command::Connect).await;
    h.wait_state(detail, online(detail)).await;
}

#[test]
fn a_connect_reaches_online_as_a_follower() {
    support::offline::run_child(
        module_path!(),
        "a_connect_reaches_online_as_a_follower_child",
    );
}

/// Critic M3: no automatic leader, so the only device stays a follower.
#[tokio::test]
async fn a_connect_reaches_online_as_a_follower_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(301);
    s.world.mediator().knobs(|k| k.auto_leader = false);
    let mut h = harness::start(s);
    connect(&mut h, "follower").await;
    assert_eq!(h.net().made(), 1, "the transports were made once");
    assert_eq!(h.net().opened().len(), 1, "one connection");
    h.finish().await;
}

#[test]
fn syncing_holds_while_dry_is_held_then_the_promotion_after_it_leads() {
    support::offline::run_child(
        module_path!(),
        "syncing_holds_while_dry_is_held_then_the_promotion_after_it_leads_child",
    );
}

/// H2: with Dry held the login-ack gives `Syncing`, and it STAYS `Syncing` while both clocks move
/// 30 s (three echoes); the release sends Dry and then the promotion: `Online { leader }`.
#[tokio::test]
async fn syncing_holds_while_dry_is_held_then_the_promotion_after_it_leads_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(302);
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    for n in 1..=3 {
        h.advance_to(START_MS + n * 10_000).await;
        h.sync().await;
        assert!(syncing(&h.state()), "not Syncing {n}0 s after the login");
    }
    let conn = h.conn();
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state("Online leader", online("leader")).await;
    h.finish().await;
}

#[test]
fn a_promotion_before_dry_waits_for_dry() {
    support::offline::run_child(module_path!(), "a_promotion_before_dry_waits_for_dry_child");
}

/// L4: the promotion comes first; the state stays `Syncing` (the role remembered) until Dry.
#[tokio::test]
async fn a_promotion_before_dry_waits_for_dry_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(303);
    s.world.mediator().knobs(|k| {
        k.hold_dry = true;
        k.promote_before_dry = true;
    });
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    let conn = h.conn();
    assert!(
        h.s.world.mediator().is_leader(conn),
        "the fake did not promote"
    );
    h.advance_to(FIRST_ECHO).await;
    h.sync().await;
    assert!(syncing(&h.state()), "promoted but not Dry: not Syncing");
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state("Online leader", online("leader")).await;
    h.finish().await;
}

#[test]
fn the_hello_and_the_login_carry_the_stored_device() {
    support::offline::run_child(
        module_path!(),
        "the_hello_and_the_login_carry_the_stored_device_child",
    );
}

/// The ClientHello: REJECT, PERSISTENT, the STORED expected state (`new`), the stored label and
/// P4's client info in the device info; the CSP login: Kourier's client info with the fixed
/// locale (C14), the CSP device id and the device cookie.
#[tokio::test]
async fn the_hello_and_the_login_carry_the_stored_device_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(304));
    connect(&mut h, "leader").await;
    let conn = h.conn();
    let m = h.s.world.mediator();
    assert_eq!(
        m.hello(conn),
        Some(HelloSeen {
            device: DEVICE,
            expected: d2m::DeviceSlotState::New,
            exhausted: d2m::client_hello::DeviceSlotsExhaustedPolicy::Reject,
            expiration: d2m::DeviceSlotExpirationPolicy::Persistent,
        })
    );
    let sealed = m
        .device_info(&h.s.keys.path_public, DEVICE)
        .expect("the slot keeps the device info");
    let plain = open_nonce_ahead(&h.s.keys.device_info, &sealed).expect("it opens with DGDIK");
    let info = d2d::DeviceInfo::decode(plain.as_slice()).expect("a DeviceInfo");
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        info.label == LABEL,
        "the device info's label is not the stored one"
    );
    assert!(
        info.platform_details == format!("Kourier {version} (linux)")
            && info.app_version == version,
        "the device info does not name Kourier"
    );
    let login = m.csp_login(conn).expect("the CSP login came");
    assert!(
        login.client_info
            == Some(format!(
                "Kourier/{version};L;en/US;linux;{}",
                std::env::consts::ARCH
            )),
        "the CSP client info is not Kourier's with the fixed locale"
    );
    assert!(
        login.csp_device_id == Some(h.s.csp_device),
        "the CSP device id is not the stored one"
    );
    assert!(login.sent_device_cookie, "no device cookie at the login");
    // P-L3: the VALUE (a cookie that differs from the stored one is what tells the phone that
    // another device may use the ID). Compared in code, never printed.
    assert!(
        login.device_cookie == Some(SeenCookie(h.s.cookie)),
        "the device cookie is not the stored one"
    );
    assert!(
        m.csp_session(conn).is_some() && h.s.world.identity(ME).is_some(),
        "no CSP session"
    );
    h.finish().await;
}

#[test]
fn a_reflected_envelope_is_acked_once_durable_and_not_sent_again() {
    support::offline::run_child(
        module_path!(),
        "a_reflected_envelope_is_acked_once_durable_and_not_sent_again_child",
    );
}

/// TH5 A5a (was TH3's "not acked, comes again"): the phone's reflected poll vote is stored (one deferred row)
/// and then acked, the device's queue is empty, and the next connection does not get it again.
#[tokio::test]
async fn a_reflected_envelope_is_acked_once_durable_and_not_sent_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(305);
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, 9);
    phone.connect();
    let mut h = harness::start(s);
    connect(&mut h, "follower").await;
    let first = h.conn();
    // A poll vote: a kind that is DEFERRED (kept whole), whatever the sender (a text from a sender the file
    // does not know is dropped, decision M2).
    let text = Author::new(9).incoming(
        "PEERPEER",
        &Msg::raw([1; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, 1)).nonce([7; 24]),
    );
    h.net().act(|_| {
        phone.reflect(&text);
    });
    // The echo's round trip: the actor has read everything before its answer.
    h.advance_to(FIRST_ECHO).await;
    h.sync().await;
    let path = h.s.keys.path_public;
    assert_eq!(traced(&h.s.world, first, "out", "reflected"), 1);
    // The ack follows the zone's commit in the connection task, which neither the clocks' move nor `sync`
    // waits for: a wall-bounded wait (A5b's selection run under load once saw no ack here yet).
    assert!(
        h.net()
            .wait(
                || traced(&h.s.world, first, "in", "reflected-ack") >= 1,
                std::time::Duration::from_secs(10)
            )
            .await,
        "not acked"
    );
    assert_eq!(
        traced(&h.s.world, first, "in", "reflected-ack"),
        1,
        "acked more than once"
    );
    assert_eq!(h.s.world.mediator().queue_len(&path, DEVICE), Some(0));
    assert_eq!(h.file_rows("deferred"), 1, "not stored");
    assert!(h.net().close_device(DEVICE, 4000), "no connection to close");
    let retry = FIRST_ECHO + 5_000;
    h.wait_state("Offline with a retry", |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(retry),
        }
    })
    .await;
    h.advance_to(retry).await;
    h.wait_state("Online again", online("follower")).await;
    let second = h.conn();
    assert!(second != first, "no second connection");
    assert_eq!(
        traced(&h.s.world, second, "out", "reflected"),
        0,
        "sent again"
    );
    assert_eq!(h.s.world.mediator().queue_len(&path, DEVICE), Some(0));
    h.finish().await;
}

#[test]
fn a_promoted_kourier_never_asks_for_messages() {
    support::offline::run_child(
        module_path!(),
        "a_promoted_kourier_never_asks_for_messages_child",
    );
}

/// Leader mode on (the fake would take the request), Kourier promoted for 10 virtual minutes: no
/// request for messages, one connection all along (the echoes keep both idle timers away).
#[tokio::test]
async fn a_promoted_kourier_never_asks_for_messages_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(306);
    s.world.mediator().knobs(|k| k.leader_mode = true);
    let mut h = harness::start(s);
    connect(&mut h, "leader").await;
    let conn = h.conn();
    for n in 1..=60 {
        h.advance_to(START_MS + n * 10_000).await;
    }
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
        0,
        "Kourier asked the chat server for messages"
    );
    assert!(
        h.state()
            == ConnState::Online {
                detail: Some("leader".to_owned())
            }
    );
    assert_eq!(h.net().opened().len(), 1, "the connection did not stay");
    h.finish().await;
}

#[test]
fn a_disconnect_closes_with_1000_and_a_hint_does_not_reconnect() {
    support::offline::run_child(
        module_path!(),
        "a_disconnect_closes_with_1000_and_a_hint_does_not_reconnect_child",
    );
}

#[tokio::test]
async fn a_disconnect_closes_with_1000_and_a_hint_does_not_reconnect_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(307));
    connect(&mut h, "leader").await;
    let conn = h.conn();
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", |s| {
        *s == ConnState::Offline { retry_at_ms: None }
    })
    .await;
    assert_eq!(h.s.world.mediator().closed(conn), Some(1000));
    assert_eq!(
        traced(&h.s.world, conn, "in", "close"),
        1,
        "not the client's close"
    );
    h.send(Command::NetworkChanged).await;
    h.send(Command::Resumed).await;
    h.sync().await;
    assert!(h.state() == ConnState::Offline { retry_at_ms: None });
    assert!(h.clock.deadlines().is_empty(), "a reconnect is scheduled");
    assert_eq!(h.net().opened().len(), 1, "a hint reconnected");
    h.finish().await;
}

#[test]
fn a_shutdown_closes_the_socket_with_1000() {
    support::offline::run_child(
        module_path!(),
        "a_shutdown_closes_the_socket_with_1000_child",
    );
}

/// The harness's `finish` checks that every socket open at the shutdown was closed with 1000 by
/// the client before `Exit::Shutdown`.
#[tokio::test]
async fn a_shutdown_closes_the_socket_with_1000_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(308));
    connect(&mut h, "leader").await;
    assert_eq!(h.net().open_connections(), vec![h.conn()]);
    h.finish().await;
}

#[test]
fn an_account_without_an_identity_makes_nothing() {
    support::offline::run_child(
        module_path!(),
        "an_account_without_an_identity_makes_nothing_child",
    );
}

/// A `Fresh` start (no identity before TH4's link): `Connect` leaves it `Offline` and the parts'
/// maker is never called.
#[tokio::test]
async fn an_account_without_an_identity_makes_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let h = harness::start_fresh(scene(309));
    h.send(Command::Connect).await;
    h.send(Command::NetworkChanged).await;
    h.sync().await;
    assert_eq!(h.net().made(), 0, "the maker was called");
    assert!(h.state() == ConnState::Offline { retry_at_ms: None });
    h.finish().await;
}

#[test]
fn the_chat_servers_own_echo_request_is_answered() {
    support::offline::run_child(
        module_path!(),
        "the_chat_servers_own_echo_request_is_answered_child",
    );
}

/// P-L5 (protocol review F6): the login declares that the client answers echo requests, so the chat
/// server may send one; Online, then `csp_echo_request(conn, [7, 7])`: ONE echo response with the
/// same payload reaches the chat server (and no violation).
#[tokio::test]
async fn the_chat_servers_own_echo_request_is_answered_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(811));
    connect(&mut h, "leader").await;
    let conn = h.conn();
    let m = h.s.world.mediator();
    assert!(
        m.csp_echo_responses().is_empty(),
        "an echo response already"
    );
    h.net().act(|m| m.csp_echo_request(conn, &[7, 7]));
    assert!(
        h.net()
            .wait(
                || !m.csp_echo_responses().is_empty(),
                std::time::Duration::from_secs(10),
            )
            .await,
        "the echo request was not answered"
    );
    h.sync().await;
    assert_eq!(m.csp_echo_responses(), vec![vec![7_u8, 7]]);
    assert_eq!(traced(&h.s.world, conn, "in", "csp-echo-response"), 1);
    h.finish().await;
}

#[test]
fn a_panicked_connection_task_fails_the_actor() {
    support::offline::run_child(
        module_path!(),
        "a_panicked_connection_task_fails_the_actor_child",
    );
}

/// P-L8 (protocol review F9): the transport panics in the connection task (the fake connector is told
/// to): the actor ends `Exit::Failed`, so the core's restart policy hears of it, instead of a silent
/// stop that leaves `Connecting` standing. No hook in the crate's code: the panic comes from the
/// fake connector, inside the task's own future.
#[tokio::test]
async fn a_panicked_connection_task_fails_the_actor_child() {
    if !support::offline::in_child() {
        return;
    }
    let h = harness::start(scene(812));
    h.net().panic_next_connect();
    h.send(Command::Connect).await;
    h.finish_failed().await;
}
