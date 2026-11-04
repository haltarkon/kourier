//! RP-b: THE OWN DEVICE'S DROP at `Logout` against the fake (`src/conn/drop_own.rs`, the actor's THE LOGOUT). A
//! connection past `ServerInfo` writes `DropDevice` of the own id on its socket; without one the actor opens a D2M-only
//! socket that expects the slot EXISTING and proxies no CSP; the ack, the 4113 after it or a 4115 is `Removed` (a first
//! 4115 while the file says `new` only after one more socket, P-M2), no answer within 8 s of the actor's clock (every
//! wait ends one second earlier, P-L1) is `NotReached`, and nothing registers a slot on the way. THE WAYS INTO THE
//! D2M-ONLY SOCKET (P-M1): a `Logout` during a connect, in a backoff, in a hold, before `ServerInfo` and after a live drop
//! closed with another code each end `Removed` on that socket, WITHOUT the clock moving toward 8 s, so a path that waits
//! for the deadline hangs at the wall bound instead of passing. Each test ends with the harness's `finish*` (the fake's
//! violations last).

use kourier_backend_api::{Command, Exit};
use kourier_model::{ConnState, DeviceLogout, LogoutReport, UserAction};
use kourier_threema::NetError;
use kourier_threema::store::{NewBody, NewJob, SlotState, To};
use libthreema::protobuf::d2m::DeviceSlotState;

use super::harness::{self, DEVICE, H, LABEL, scene, traced};
use crate::support;
use crate::support::mock::d2m::ConnId;

/// The actor's bound of the drop (`conn::drop_own::DROP_WITHIN`), ms.
const DROP_WITHIN: u64 = 8_000;
/// The closing second inside it (`conn::CLOSE_WITHIN`): every wait of the drop ends this much before the bound (P-L1).
const CLOSE_WITHIN: u64 = 1_000;
/// Where the drop's waits end, from the logout: the bound less the closing second.
const WAITS_END: u64 = DROP_WITHIN - CLOSE_WITHIN;
/// The pause before the one more socket after a first 4115 while the file says `new` (`conn::drop_own::RETRY_AFTER`), ms.
const RETRY_AFTER: u64 = 1_000;
/// A wall BOUND of a wait for the fake (never a measure).
const WALL: std::time::Duration = std::time::Duration::from_secs(10);
/// The contact of the send queue's test.
const ANN: &str = "PEER0001";
/// The reason of the actor's exit after a logout.
const LOGGED_OUT: &str = "logged out on request";

/// The report of a logout that `outcome` the stored device.
fn report(outcome: DeviceLogout) -> LogoutReport {
    LogoutReport {
        outcome,
        device_name: Some(LABEL.to_owned()),
    }
}

/// The drops the fake took: the connection and the id named.
fn drops(h: &H) -> Vec<(ConnId, u64)> {
    h.s.world
        .mediator()
        .drops()
        .iter()
        .map(|d| (d.conn, d.device))
        .collect()
}

/// The device ids the fake holds a slot for in this device's group.
fn slots(h: &H) -> Vec<u64> {
    h.s.world.mediator().slots(&h.s.keys.path_public)
}

/// Connects and waits for `Online`.
async fn online(h: &mut H) {
    h.send(Command::Connect).await;
    h.wait_state("Online", |s| matches!(s, ConnState::Online { .. }))
        .await;
}

#[test]
fn a_logout_online_drops_the_device_on_its_live_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_online_drops_the_device_on_its_live_socket_child",
    );
}

/// The connection is `Online`: `Logout` writes `DropDevice` with the own id on THAT socket, the fake acks it, frees the
/// slot and closes 4113: `Removed` with the label, no second socket.
#[tokio::test]
async fn a_logout_online_drops_the_device_on_its_live_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(331));
    online(&mut h).await;
    let conn = h.conn();
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    assert_eq!(
        drops(&h),
        vec![(conn, DEVICE)],
        "not the own id on the live socket"
    );
    assert!(slots(&h).is_empty(), "the device's slot stayed");
    assert_eq!(
        h.s.world.mediator().closed(conn),
        Some(4113),
        "the fake did not close the dropped device 4113"
    );
    assert_eq!(h.net().opened().len(), 1, "a second socket for the drop");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_logout_offline_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_offline_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// The device was linked and online once; a NEW actor (the core's restart, never told to connect) gets `Logout`: one
/// socket of its own, its ClientHello expects EXISTING, no Proxy (no CSP login) goes on it, `DropDevice` of the own id,
/// the ack and the 4113: `Removed`, the slot gone.
#[tokio::test]
async fn a_logout_offline_drops_the_device_on_a_d2m_only_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(332));
    online(&mut h).await;
    let mut h = h.restart().await;
    assert_eq!(slots(&h), vec![DEVICE], "the device has no slot to drop");
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not one socket of the drop's own");
    let alone = opened[1];
    let m = h.s.world.mediator();
    assert_eq!(
        m.hello(alone).map(|hello| hello.expected),
        Some(DeviceSlotState::Existing),
        "the drop's socket did not expect EXISTING"
    );
    assert_eq!(
        traced(&h.s.world, alone, "in", "proxy"),
        0,
        "the drop's socket proxied the chat server"
    );
    assert_eq!(drops(&h), vec![(alone, DEVICE)], "not the own id");
    assert_eq!(
        m.closed(alone),
        Some(4113),
        "the drop's socket was not closed 4113"
    );
    assert!(slots(&h).is_empty(), "the device's slot stayed");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_4115_on_the_d2m_only_socket_is_removed_and_makes_no_slot() {
    support::offline::run_child(
        module_path!(),
        "a_4115_on_the_d2m_only_socket_is_removed_and_makes_no_slot_child",
    );
}

/// The file says `existing` (the slot was there and the phone removed it), the mediator holds no slot: the drop's socket
/// expects EXISTING, the fake closes 4115 before any registration: `Removed` (the network no longer knew it) at the FIRST
/// 4115 (P-M2 asks once more only while the file says `new`), no slot made, no drop sent.
#[tokio::test]
async fn a_4115_on_the_d2m_only_socket_is_removed_and_makes_no_slot_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut s = scene(335);
    s.slot = SlotState::Existing;
    let mut h = harness::start(s);
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 1, "not one socket of the drop's own");
    let m = h.s.world.mediator();
    assert_eq!(
        m.hello(opened[0]).map(|hello| hello.expected),
        Some(DeviceSlotState::Existing),
        "the drop's socket did not expect EXISTING"
    );
    assert_eq!(m.closed(opened[0]), Some(4115), "not closed 4115");
    assert!(slots(&h).is_empty(), "a slot was made");
    assert!(drops(&h).is_empty(), "a drop went without a slot");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_mediator_refused_hanging_or_silent_is_not_reached_within_eight_seconds() {
    support::offline::run_child(
        module_path!(),
        "a_mediator_refused_hanging_or_silent_is_not_reached_within_eight_seconds_child",
    );
}

/// No connection runs. A refused connect: `NotReached` at once (no retry). A connect that hangs, and a mediator that
/// takes the socket and says nothing: `NotReached` when the drop's waits end, one closing second before 8 s of the
/// actor's clock (the fake's virtual clock with it), the silent socket closed 1000 (P-L1: so the close ends by 8 s). No
/// ClientHello reached the fake, so no slot was made.
#[tokio::test]
async fn a_mediator_refused_hanging_or_silent_is_not_reached_within_eight_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(333));
    h.net().fail_connects(Some(NetError::Failed("websocket")));
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::NotReached));
    assert_eq!(
        h.wait_exit().await,
        Exit::LoggedOut {
            reason: LOGGED_OUT.to_owned()
        }
    );
    assert_eq!(h.net().attempts(), 1, "the refused connect was tried again");
    h.net().fail_connects(None);

    let mut h = h.restart_after_exit();
    h.net().hold_connects(true);
    let by = h.now() + WAITS_END;
    h.send(Command::Logout).await;
    h.advance_to(by).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::NotReached));
    assert_eq!(
        h.wait_exit().await,
        Exit::LoggedOut {
            reason: LOGGED_OUT.to_owned()
        }
    );
    assert_eq!(h.net().attempts(), 2, "not one hanging connect");
    h.net().hold_connects(false);

    let mut h = h.restart_after_exit();
    h.s.world
        .mediator()
        .knobs(|k| k.withhold_server_hello = true);
    let by = h.now() + WAITS_END;
    h.send(Command::Logout).await;
    h.advance_to(by).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::NotReached));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 1, "not one silent socket");
    assert_eq!(
        h.s.world.mediator().closed(opened[0]),
        Some(1000),
        "the silent socket was not closed 1000"
    );
    assert!(slots(&h).is_empty(), "a slot was made");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_silent_drop_on_the_live_socket_is_not_reached_at_eight_seconds() {
    support::offline::run_child(
        module_path!(),
        "a_silent_drop_on_the_live_socket_is_not_reached_at_eight_seconds_child",
    );
}

/// `Online`, and the fake takes the drop and says nothing (`withhold_drop_ack`): when the drop's waits end, one closing
/// second before 8 s of the actor's clock, the connection is stopped (closed 1000) and the report is `NotReached`; the
/// slot stays, no second socket is tried after the time ran out.
#[tokio::test]
async fn a_silent_drop_on_the_live_socket_is_not_reached_at_eight_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(334);
    s.world.mediator().knobs(|k| k.withhold_drop_ack = true);
    let mut h = harness::start(s);
    online(&mut h).await;
    let conn = h.conn();
    let by = h.now() + WAITS_END;
    h.send(Command::Logout).await;
    h.advance_to(by).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::NotReached));
    assert_eq!(
        drops(&h),
        vec![(conn, DEVICE)],
        "not the own id on the live socket"
    );
    assert_eq!(
        h.s.world.mediator().closed(conn),
        Some(1000),
        "the live socket was not closed 1000"
    );
    assert_eq!(slots(&h), vec![DEVICE], "the slot went without an answer");
    assert_eq!(
        h.net().opened().len(),
        1,
        "a second socket after the time ran out"
    );
    h.finish_logged_out(LOGGED_OUT).await;
}

/// The drop went on ONE socket of its own, `alone`: its ClientHello expected EXISTING, no Proxy went on it (no CSP
/// login), the fake saw the own id named on it and closed it 4113, and no slot is left.
fn dropped_alone(h: &H, alone: ConnId) {
    let m = h.s.world.mediator();
    assert_eq!(
        m.hello(alone).map(|hello| hello.expected),
        Some(DeviceSlotState::Existing),
        "the drop's socket did not expect EXISTING"
    );
    assert_eq!(
        traced(&h.s.world, alone, "in", "proxy"),
        0,
        "the drop's socket proxied the chat server"
    );
    assert_eq!(drops(h), vec![(alone, DEVICE)], "not the own id on it");
    assert_eq!(m.closed(alone), Some(4113), "not closed 4113");
    assert!(slots(h).is_empty(), "the device's slot stayed");
}

/// The device was linked and online once and a NEW actor of the same file runs (the mediator holds its slot, the file
/// says `existing`): the start of the entry tests that need no first connection of their own.
async fn restarted(seed: u64) -> H {
    let mut h = harness::start(scene(seed));
    online(&mut h).await;
    let h = h.restart().await;
    assert_eq!(slots(&h), vec![DEVICE], "the device has no slot to drop");
    h
}

#[test]
fn a_logout_during_a_connect_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_during_a_connect_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// P-M1, the connect select: the connection's connect hangs (the first attempt of the new actor); `Logout` ends it
/// without a socket and the actor's own connect (the second attempt, once the hold ends) opens THE D2M-ONLY SOCKET:
/// `Removed`, the drop seen by the fake. The clock never moves.
#[tokio::test]
async fn a_logout_during_a_connect_drops_the_device_on_a_d2m_only_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = restarted(336).await;
    h.net().hold_connects(true);
    h.send(Command::Connect).await;
    h.wait_attempts(2).await;
    h.send(Command::Logout).await;
    h.wait_attempts(3).await;
    h.net().hold_connects(false);
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "the hanging connect opened a socket");
    dropped_alone(&h, opened[1]);
    assert_eq!(h.net().attempts(), 3, "a third connect");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_logout_in_a_backoff_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_in_a_backoff_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// P-M1, the backoff select (`Next::After`): the server closed 4000, the connection waits its five seconds; `Logout`
/// ends the wait (no socket at the backoff time) and the actor opens THE D2M-ONLY SOCKET: `Removed`. The clock never
/// moves, so a task that waited for its backoff would hang at the wall bound.
#[tokio::test]
async fn a_logout_in_a_backoff_drops_the_device_on_a_d2m_only_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(337));
    online(&mut h).await;
    assert!(h.net().close_device(DEVICE, 4000), "no connection to close");
    h.wait_state("Offline with a retry", |s| {
        matches!(
            s,
            ConnState::Offline {
                retry_at_ms: Some(_)
            }
        )
    })
    .await;
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not one socket of the drop's own");
    dropped_alone(&h, opened[1]);
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_logout_in_a_hold_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_in_a_hold_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// P-M1, the hold (`Next::Hold`): a 4111 holds the connection for the user (`NeedsUserAction`), it waits for a nudge;
/// `Logout` ends the hold and the actor opens THE D2M-ONLY SOCKET: `Removed`. The clock never moves.
#[tokio::test]
async fn a_logout_in_a_hold_drops_the_device_on_a_d2m_only_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(338));
    online(&mut h).await;
    assert!(h.net().close_device(DEVICE, 4111), "no connection to close");
    h.wait_state("held", |s| {
        *s == ConnState::NeedsUserAction(UserAction::Reconnect)
    })
    .await;
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not one socket of the drop's own");
    dropped_alone(&h, opened[1]);
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_logout_before_server_info_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_before_server_info_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// P-M1, a socket before `ServerInfo`: the new actor's connection said its ClientHello and the fake keeps `ServerInfo`
/// back (`withhold_server_info`: the device is registered there, the client does not know); `Logout` closes that socket
/// 1000 and the actor opens THE D2M-ONLY SOCKET (the third socket): `Removed`, the drop seen by the fake. No third connect
/// after it; the clock never moves.
#[tokio::test]
async fn a_logout_before_server_info_drops_the_device_on_a_d2m_only_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = restarted(339).await;
    h.s.world
        .mediator()
        .knobs(|k| k.withhold_server_info = true);
    h.send(Command::Connect).await;
    h.wait_opened(2).await;
    let before = h.net().opened()[1];
    let m = h.s.world.mediator();
    assert!(
        h.net().wait(|| m.hello(before).is_some(), WALL).await,
        "the fake never took the ClientHello"
    );
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 3, "not one socket of the drop's own");
    assert_eq!(
        h.s.world.mediator().closed(before),
        Some(1000),
        "the socket before ServerInfo was not closed 1000"
    );
    dropped_alone(&h, opened[2]);
    assert_eq!(h.net().attempts(), 3, "a fourth connect");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_logout_after_a_live_drop_closed_with_another_code_drops_the_device_on_a_d2m_only_socket() {
    support::offline::run_child(
        module_path!(),
        "a_logout_after_a_live_drop_closed_with_another_code_drops_the_device_on_a_d2m_only_socket_child",
    );
}

/// P-M1, a live drop that ends another way: `Online`, `Logout` writes `DropDevice` on the live socket and the fake closes
/// it 4000 instead of answering (`close_at_next_drop`: no ack, the slot kept); the actor goes on with THE D2M-ONLY SOCKET,
/// which the fake answers (the knob is once): `Removed`, both drops seen, the slot gone, no reconnect in between. The
/// clock never moves.
#[tokio::test]
async fn a_logout_after_a_live_drop_closed_with_another_code_drops_the_device_on_a_d2m_only_socket_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(340);
    s.world
        .mediator()
        .knobs(|k| k.close_at_next_drop = Some(4000));
    let mut h = harness::start(s);
    online(&mut h).await;
    let live = h.conn();
    h.send(Command::Logout).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not one socket of the drop's own");
    let alone = opened[1];
    let m = h.s.world.mediator();
    assert_eq!(
        m.closed(live),
        Some(4000),
        "the live socket was not closed 4000"
    );
    assert_eq!(
        drops(&h),
        vec![(live, DEVICE), (alone, DEVICE)],
        "the drops the fake took"
    );
    assert_eq!(
        m.hello(alone).map(|hello| hello.expected),
        Some(DeviceSlotState::Existing),
        "the drop's socket did not expect EXISTING"
    );
    assert_eq!(
        traced(&h.s.world, alone, "in", "proxy"),
        0,
        "the drop's socket proxied the chat server"
    );
    assert_eq!(m.closed(alone), Some(4113), "not closed 4113");
    assert!(slots(&h).is_empty(), "the device's slot stayed");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_first_4115_while_the_file_says_new_is_asked_once_more_and_a_second_is_removed() {
    support::offline::run_child(
        module_path!(),
        "a_first_4115_while_the_file_says_new_is_asked_once_more_and_a_second_is_removed_child",
    );
}

/// P-M2: the file says `new` (a join's hello may be in flight) and the mediator holds no slot: the first EXISTING socket
/// is told 4115, which is NOT final; ONE more EXISTING socket one second of the actor's clock later is told 4115 too:
/// `Removed`, no slot made, no drop sent.
#[tokio::test]
async fn a_first_4115_while_the_file_says_new_is_asked_once_more_and_a_second_is_removed_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(342));
    let at = h.now() + RETRY_AFTER;
    h.send(Command::Logout).await;
    h.advance_to(at).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not two sockets of the drop's own");
    let m = h.s.world.mediator();
    for socket in &opened {
        assert_eq!(
            m.hello(*socket).map(|hello| hello.expected),
            Some(DeviceSlotState::Existing),
            "a drop's socket did not expect EXISTING"
        );
        assert_eq!(m.closed(*socket), Some(4115), "not closed 4115");
        assert_eq!(traced(&h.s.world, *socket, "in", "proxy"), 0, "proxied");
    }
    assert!(slots(&h).is_empty(), "a slot was made");
    assert!(drops(&h).is_empty(), "a drop went without a slot");
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn a_late_new_hello_that_registers_after_the_first_4115_is_dropped_by_the_second_socket() {
    support::offline::run_child(
        module_path!(),
        "a_late_new_hello_that_registers_after_the_first_4115_is_dropped_by_the_second_socket_child",
    );
}

/// P-M2, the race it closes: the file says `new`, the mediator holds no slot when the first EXISTING hello comes (the
/// join's NEW hello is still in flight there), and registers the slot right after it told 4115
/// (`register_slot_after_next_4115`). The first 4115 is not final: one second later the second EXISTING socket finds the
/// slot, drops it and is acked: `Removed` with NO orphan slot left, which the old "4115 is `Removed`" would have left.
#[tokio::test]
async fn a_late_new_hello_that_registers_after_the_first_4115_is_dropped_by_the_second_socket_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(343);
    s.world
        .mediator()
        .knobs(|k| k.register_slot_after_next_4115 = true);
    let mut h = harness::start(s);
    let at = h.now() + RETRY_AFTER;
    h.send(Command::Logout).await;
    h.advance_to(at).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::Removed));
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not two sockets of the drop's own");
    assert_eq!(
        h.s.world.mediator().closed(opened[0]),
        Some(4115),
        "the first socket was not told 4115"
    );
    dropped_alone(&h, opened[1]);
    h.finish_logged_out(LOGGED_OUT).await;
}

#[test]
fn the_send_queue_writes_no_frame_after_the_drop() {
    support::offline::run_child(
        module_path!(),
        "the_send_queue_writes_no_frame_after_the_drop_child",
    );
}

/// S-4 (`Session::may_write`'s `!dropping`): a queued row's reflect went and its ack is held; `Logout` writes
/// `DropDevice` and the fake stays silent on it; the held reflect-ack is released AFTER the drop: the row would send its
/// box now, and does not (not one box reached the chat server), the connection is stopped when the drop's waits end.
#[tokio::test]
async fn the_send_queue_writes_no_frame_after_the_drop_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(344).with_peer(ANN).job(NewJob {
        kind: "test-job",
        to: To::Contact(ANN.to_owned()),
        body: NewBody::Text("synthetic".to_owned()),
        only: None,
    });
    let m = s.world.mediator().clone();
    m.knobs(|k| {
        k.withhold_reflect_acks = true;
        k.withhold_drop_ack = true;
    });
    let mut h = harness::start(s);
    online(&mut h).await;
    let conn = h.conn();
    assert!(
        h.net().wait(|| m.reflect_acks_held(conn) == 1, WALL).await,
        "the reflect never came"
    );
    assert_eq!(m.csp_sent().len(), 0, "a box before the reflect-ack");
    let by = h.now() + WAITS_END;
    h.send(Command::Logout).await;
    assert!(
        h.net().wait(|| m.drops().len() == 1, WALL).await,
        "the drop never came"
    );
    // The reflect-ack comes behind the drop: the row would box now.
    m.knobs(|k| k.withhold_reflect_acks = false);
    h.net().act(|m| m.release_reflect_acks(conn));
    h.advance_to(by).await;
    assert_eq!(h.logout_report().await, report(DeviceLogout::NotReached));
    assert_eq!(drops(&h), vec![(conn, DEVICE)], "the drops the fake took");
    assert_eq!(m.csp_sent().len(), 0, "a box went after the drop");
    assert_eq!(m.closed(conn), Some(1000), "the live socket was not closed");
    h.finish_logged_out(LOGGED_OUT).await;
}
