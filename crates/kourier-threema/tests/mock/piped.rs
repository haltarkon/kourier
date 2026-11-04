//! The parent-resident fake (TH5a A3, `support/mock/pipe.rs`, harness `support/piped.rs`): the fake
//! lives in THIS (parent) test process, the real actor in an `unshare -rn` child that reaches it
//! over two inherited pipes. Each parent test ends with its world's violations check; each child
//! (it begins with `offline::in_child()`) ends with the actor's `finish*()`. No wall clock and no
//! sleep: waits are wall bounds only.

use std::time::Duration;

use libthreema::protobuf::d2m::DeviceSlotState;

use crate::support;
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::{Author, Msg, Reflect as _};
use crate::support::mock::pipe::{CHILD_READS, CHILD_WRITES, pipe_fds};
use crate::support::mock::{MockWorld, START_MS};
use crate::support::piped::{self, DEVICE, Mode, Parent, SERVER_GROUP};
use kourier_threema::failpoints as fp;
use kourier_threema::store::SlotState;

/// The phone's D2M device id.
const PHONE: u64 = 0x0FA5_0000_0000_0001;

/// The trace lines of `conn` of `kind` the fake sent ("out") or took ("in").
fn traced(world: &MockWorld, conn: ConnId, dir: &str, kind: &str) -> usize {
    world
        .trace()
        .lines()
        .iter()
        .filter(|line| {
            let mut parts = line.split(' ').skip(1);
            parts.next() == Some(conn.0.to_string().as_str())
                && parts.next() == Some(dir)
                && parts.next() == Some(kind)
        })
        .count()
}

/// The expected slot state of every `ClientHello` the fake saw, in order.
fn hellos(p: &Parent) -> Vec<Option<DeviceSlotState>> {
    let m = p.world.mediator();
    p.world
        .net()
        .opened()
        .into_iter()
        .map(|conn| m.hello(conn).map(|hello| hello.expected))
        .collect()
}

/// The fake's connection of the device that finished its CSP login.
fn logged_in(p: &Parent) -> Option<ConnId> {
    let m = p.world.mediator();
    p.world
        .net()
        .opened()
        .into_iter()
        .rev()
        .find(|c| m.device_of(*c) == Some(DEVICE) && m.csp_logged_in(*c))
}

/// CHILD: nothing but its stdio and the two pipes reached it (fds 3 and 4, two different pipes),
/// then `Connect` reaches `Online` through the pipes, then `Shutdown`.
#[tokio::test]
async fn online_child() {
    if !support::offline::in_child() {
        return;
    }
    let fds = pipe_fds();
    let others: Vec<i32> = fds.iter().copied().filter(|fd| *fd > 2).collect();
    assert_eq!(
        others,
        vec![CHILD_READS, CHILD_WRITES],
        "pipe fds beyond stdio and the two the parent gave"
    );
    let link = |fd: i32| std::fs::read_link(format!("/proc/self/fd/{fd}")).expect("a link");
    assert!(
        link(CHILD_READS) != link(CHILD_WRITES),
        "fds 3 and 4 are the same pipe"
    );
    let mut c = piped::child().await;
    c.online().await;
    c.finish().await;
}

/// CHILD (TH5 A5b): `Connect` through the pipes reaches `Syncing` over a file with a row the core never
/// acks (the kill children never ack, M4), then `Shutdown`: the catch-up boundary holds `Online` back.
#[tokio::test]
async fn syncing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.syncing().await;
    c.finish().await;
}

/// CHILD: reaches `Online`, then waits for the parent's SIGKILL.
#[tokio::test]
async fn killed_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.online().await;
    c.finish_killed().await;
}

/// CHILD: a `Rebuild` start (killed by the parent's failpoint in `store::ready_join`).
#[tokio::test]
async fn rebuild_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.online().await;
    c.finish().await;
}

/// A child reaches `Online` through the pipes as with the in-process connector: one connection,
/// the ClientHello expects `new`, the file says `existing` afterwards, the child's `Shutdown`
/// closes with 1000, and only the two pipes reached the child (its own check).
#[tokio::test]
async fn a_child_reaches_online_through_the_pipes() {
    let p = piped::parent(701);
    let Some(run) = p.spawn(module_path!(), "online_child", Mode::Resume, None) else {
        return;
    };
    let ended = run.wait();
    assert!(
        ended.passed(),
        "the child failed: {ended:?}\n{}",
        ended.stderr
    );
    let opened = p.world.net().opened();
    assert_eq!(opened.len(), 1, "one connection");
    assert_eq!(hellos(&p), vec![Some(DeviceSlotState::New)], "the hello");
    assert!(
        p.world.mediator().csp_logged_in(opened[0]),
        "the CSP login reached the fake"
    );
    assert_eq!(p.world.mediator().closed(opened[0]), Some(1000), "Shutdown");
    assert_eq!(p.file_slot(), SlotState::Existing, "the slot write");
    assert_eq!(p.outbox_rows(), 0, "a Resume states nothing");
    p.world.violations().assert_empty();
}

/// THE STATE OUTLIVES THE CHILD: a child online, the phone reflects one envelope (the actor drops it
/// un-acked, TH3), the test SIGKILLs the child: the fake ends its connection with 1006 (an EOF, no
/// close frame) and keeps the slot and its queue; a SECOND child on the same world reconnects to the
/// SAME slot (its hello expects `existing`, two hellos in all) and gets the un-acked reflection again.
#[tokio::test]
async fn a_killed_child_leaves_its_slot_and_queue_and_the_next_one_reconnects() {
    let p = piped::parent(702);
    let mut phone = p.world.phone(&p.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    let Some(first) = p.spawn(module_path!(), "killed_child", Mode::Resume, None) else {
        return;
    };
    p.wait("the first child's CSP login", || logged_in(&p).is_some())
        .await;
    let one = logged_in(&p).expect("logged in");
    let env = Author::new(PHONE).incoming("PEERAAAA", &Msg::text([7; 8], START_MS, "hi"));
    phone.reflect_envelope(&env);
    p.wait("the reflection to the first child", || {
        traced(&p.world, one, "out", "reflected") == 1
    })
    .await;
    let ended = first.kill();
    assert!(ended.killed(), "the first child was not killed: {ended:?}");
    let m = p.world.mediator();
    assert_eq!(m.closed(one), Some(1006), "a dead child is 1006");
    assert_eq!(traced(&p.world, one, "in", "lost"), 1, "lost once");
    assert_eq!(
        m.queue_len(&p.keys.path_public, DEVICE),
        Some(1),
        "the un-acked reflection stays queued"
    );
    assert_eq!(
        p.file_slot(),
        SlotState::Existing,
        "the killed child's file"
    );
    let Some(second) = p.spawn(module_path!(), "online_child", Mode::Resume, None) else {
        return;
    };
    let ended = second.wait();
    assert!(
        ended.passed(),
        "the second child failed: {ended:?}\n{}",
        ended.stderr
    );
    assert_eq!(
        hellos(&p),
        vec![Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)],
        "two hellos, the second to the existing slot"
    );
    let two = p.world.net().opened()[1];
    assert_eq!(
        traced(&p.world, two, "out", "reflected"),
        1,
        "the un-acked reflection again to the second child"
    );
    assert_eq!(m.closed(two), Some(1000), "the second child's Shutdown");
    assert_eq!(p.outbox_rows(), 0, "nothing stated, nothing acked");
    assert_eq!(p.children(), 2, "two children on one world");
    p.world.violations().assert_empty();
}

/// A FAILPOINT KILL: a `Rebuild` child dies at `threema_after_ready:1` (before it connects); the
/// parent reads the file (M4: the rows counted in `core_outbox` + `core_outbox_poison`), then a
/// `Resume` child connects on the same world and stays `Syncing` (A5b: the row is never acked).
#[tokio::test]
async fn a_child_killed_at_a_failpoint_leaves_its_file_to_the_parent() {
    let p = piped::parent(703);
    let point = format!("{}:1", fp::AFTER_READY);
    let Some(first) = p.spawn(module_path!(), "rebuild_child", Mode::Rebuild, Some(&point)) else {
        return;
    };
    let ended = first.wait();
    assert!(
        ended.aborted_at_failpoint(),
        "the child did not die at the failpoint: {ended:?}"
    );
    assert_eq!(
        p.outbox_rows(),
        1,
        "AccountReady, committed before the kill"
    );
    assert!(
        p.world.net().opened().is_empty(),
        "no connect before the kill"
    );
    p.advance(Duration::from_secs(1));
    // A5b: AccountReady is never acked (no core in a child), so the resumed account stays `Syncing`.
    let Some(second) = p.spawn(module_path!(), "syncing_child", Mode::Resume, None) else {
        return;
    };
    let ended = second.wait();
    assert!(
        ended.passed(),
        "the second child failed: {ended:?}\n{}",
        ended.stderr
    );
    assert_eq!(hellos(&p), vec![Some(DeviceSlotState::New)], "one hello");
    assert_eq!(p.outbox_rows(), 1, "never acked, never deleted");
    p.world.violations().assert_empty();
}
