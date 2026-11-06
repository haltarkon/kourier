//! TH6 C1 (= L4a): THE LINE CONTRACT and THE H1 GUARD against the offline fake. The run tool (TR-lite) reads the lines
//! these tests pin in their exact words: the count line of every socket (every 60 s of the actor's clock while it
//! stands, `when=running`, and once at its end, `when=end`, its numbers those of the fake's trace, a send row's frames
//! by origin), one kind line per unhandled kind and socket, and the guard's WARN: a chat server message on a socket that
//! may not take one (not the leader, or a leader before its unblock: TH6 L4b sends it once Dry is counted) closes it
//! 1000 at once, nothing acked, through the plain backoff. Every line of the crate holds counts and names only: each test
//! searches what it captured for the own ID and a peer's. The words are written here as literals on purpose: the
//! crate's consts (`src/conn/lines.rs`) are pinned by its unit test, these by what the actor printed.

use std::collections::BTreeMap;
use std::time::Duration;

use kourier_backend_api::Command;
use kourier_model::ConnState;
use kourier_threema::store::{self, NewBody, NewJob, To};
use libthreema::protobuf::d2m as d2m_proto;
use prost::Message as _;
use tracing::Level;

use super::harness::{self, DEVICE, H, ME, scene, traced};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::d2m::{ConnId, datagram, kind};
use crate::support::warns::{Line, Warns};

/// The upper bound of a wait (never a measure).
pub(super) const WALL: Duration = Duration::from_secs(10);

/// L-COUNT, L-KIND, L-UNASKED as the actor prints them.
pub(super) const COUNT: &str = "socket frame counts";
const KIND: &str = "a server payload Kourier does not handle";
const UNASKED: &str = "a chat server message came unasked; closing";

/// A peer of the scenes (a world identity with a real key where a job is sent to it).
const ANN: &str = "PEER0001";

/// The count line's counters, in the contract's order (after `when`, before `held` and `held_by`).
const COUNTERS: [&str; 26] = [
    "in_reflected",
    "in_reflect_ack",
    "in_dry",
    "in_promoted",
    "in_drop_ack",
    "in_tx",
    "in_csp_message",
    "in_message_ack",
    "in_queue_complete",
    "in_echo",
    "in_close_error",
    "in_other",
    "out_reflected_ack",
    "out_reflect_job",
    "out_reflect_transmit",
    "out_reflect_inbound",
    "out_ephemeral",
    "out_box_job",
    "out_box_transmit",
    "out_message_ack",
    "out_unblock",
    "out_tx",
    "out_echo",
    "out_idle",
    "out_drop",
    "out_other",
];

/// What the client READ, by the fake's trace: the counter and the kinds the fake traced as SENT (`out`) for it.
const READ: [(&str, &[&str]); 12] = [
    ("in_reflected", &["reflected"]),
    ("in_reflect_ack", &["reflect-ack"]),
    ("in_dry", &["reflection-queue-dry"]),
    ("in_promoted", &["role-promoted-to-leader"]),
    ("in_drop_ack", &["drop-device-ack"]),
    (
        "in_tx",
        &[
            "begin-transaction-ack",
            "commit-transaction-ack",
            "transaction-rejected",
            "transaction-ended",
        ],
    ),
    ("in_csp_message", &["csp-incoming-message"]),
    ("in_message_ack", &["csp-outgoing-message-ack"]),
    ("in_queue_complete", &["csp-queue-send-complete"]),
    ("in_echo", &["csp-echo-request", "csp-echo-response"]),
    ("in_close_error", &["csp-close-error"]),
    (
        "in_other",
        &[
            "devices-info",
            "unknown",
            "csp-alert",
            "csp-device-cookie-change-indication",
            "csp-unknown",
        ],
    ),
];

/// What the client WROTE, by the fake's trace: the counters (summed) and the kinds the fake traced as TAKEN (`in`).
const WROTE: [(&[&str], &[&str]); 10] = [
    (&["out_reflected_ack"], &["reflected-ack"]),
    (
        &[
            "out_reflect_job",
            "out_reflect_transmit",
            "out_reflect_inbound",
            "out_ephemeral",
        ],
        &["reflect"],
    ),
    (
        &["out_box_job", "out_box_transmit"],
        &["csp-outgoing-message"],
    ),
    (&["out_message_ack"], &["csp-incoming-message-ack"]),
    (&["out_unblock"], &["csp-unblock-incoming-messages"]),
    (&["out_tx"], &["begin-transaction", "commit-transaction"]),
    (&["out_echo"], &["csp-echo-request", "csp-echo-response"]),
    (&["out_idle"], &["csp-set-connection-idle-timeout"]),
    (&["out_drop"], &["drop-device"]),
    (
        &["out_other"],
        &[
            "get-devices-info",
            "set-shared-device-data",
            "csp-set-push-token",
            "csp-delete-push-token",
            "csp-clear-device-cookie-change-indication",
        ],
    ),
];

/// `s` seconds after both clocks' start.
pub(super) fn at(s: u64) -> u64 {
    START_MS + s * 1_000
}

pub(super) fn online_as(detail: &'static str) -> impl Fn(&ConnState) -> bool {
    move |s| {
        *s == ConnState::Online {
            detail: Some(detail.to_owned()),
        }
    }
}

pub(super) fn retry_at(when: u64) -> impl Fn(&ConnState) -> bool {
    move |s| {
        *s == ConnState::Offline {
            retry_at_ms: Some(when),
        }
    }
}

/// Waits (a wall BOUND) until `n` captured lines are `what`, and not more; those lines.
pub(super) async fn wait_lines(warns: &Warns, n: usize, what: impl Fn(&Line) -> bool) -> Vec<Line> {
    let found = || -> Vec<Line> { warns.lines().into_iter().filter(|l| what(l)).collect() };
    let reached = tokio::time::timeout(WALL, async {
        while found().len() < n {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let lines = found();
    assert!(
        reached.is_ok(),
        "{} of {n} lines came: {:?}",
        lines.len(),
        lines.iter().map(Line::text).collect::<Vec<_>>()
    );
    assert_eq!(lines.len(), n, "more lines than expected");
    lines
}

pub(super) fn count_line(when: &'static str) -> impl Fn(&Line) -> bool {
    move |l| l.message == COUNT && l.field("when") == Some(when)
}

/// The counters of a count line, after checking its shape: INFO, the fields in the contract's order, every counter a
/// number, nothing held.
pub(super) fn counters(line: &Line) -> BTreeMap<&'static str, u64> {
    assert_eq!(line.field("held"), Some("0"));
    assert_eq!(line.field("held_by"), Some("none"));
    counts_of(line)
}

/// The counters of a count line, after checking its shape (INFO, the fields in the contract's order, every counter a
/// number); `held` and `held_by` are the caller's.
pub(super) fn counts_of(line: &Line) -> BTreeMap<&'static str, u64> {
    assert_eq!(line.level, Level::INFO, "the count line's level");
    let names: Vec<&str> = line.fields.iter().map(|(n, _)| n.as_str()).collect();
    let mut want = vec!["when"];
    want.extend(COUNTERS);
    want.extend(["held", "held_by"]);
    assert_eq!(names, want, "the count line's fields");
    COUNTERS
        .iter()
        .map(|name| {
            let value = line.field(name).expect("a counter");
            (*name, value.parse().expect("a counter is a number"))
        })
        .collect()
}

/// A count line's numbers are the fake's trace of `conn`: what the fake sent is what the client read, by kind, and what
/// the fake took is what the client wrote.
pub(super) fn equals_the_trace(h: &H, conn: ConnId, line: &Line) {
    let counted = counts_of(line);
    let world = &h.s.world;
    let traced_sum = |dir: &str, kinds: &[&str]| -> u64 {
        kinds
            .iter()
            .map(|k| u64::try_from(traced(world, conn, dir, k)).expect("fits"))
            .sum()
    };
    for (counter, kinds) in READ {
        assert_eq!(
            counted[counter],
            traced_sum("out", kinds),
            "{counter} against the fake's trace"
        );
    }
    for (fields, kinds) in WROTE {
        let sum: u64 = fields.iter().map(|f| counted[f]).sum();
        assert_eq!(
            sum,
            traced_sum("in", kinds),
            "{fields:?} against the fake's trace"
        );
    }
}

/// Counts and names only: no captured line of the crate names the own ID or a peer's.
pub(super) fn no_id(warns: &Warns) {
    let ours: Vec<Line> = warns
        .lines()
        .into_iter()
        .filter(|l| l.target.starts_with("kourier_threema"))
        .collect();
    assert!(!ours.is_empty(), "nothing of the crate was captured");
    for line in ours {
        let text = line.text();
        for id in [ME, ANN] {
            assert!(!text.contains(id), "an ID in the line {:?}", line.message);
        }
    }
}

/// The client's close of `conn` with 1000 and no CSP ack on it.
fn closed_unacked(h: &H, conn: ConnId) {
    let m = h.s.world.mediator().clone();
    assert_eq!(m.closed(conn), Some(1000), "not closed with 1000");
    assert_eq!(
        traced(&h.s.world, conn, "in", "close"),
        1,
        "not the client's close"
    );
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-incoming-message-ack"),
        0,
        "an incoming message was acked"
    );
    assert_eq!(
        traced(&h.s.world, conn, "out", "csp-incoming-message"),
        1,
        "the message did not reach the client"
    );
}

/// The place of `conn`'s first trace line of `kind` that went `dir` in the fake's whole trace (a test's order check).
#[track_caller]
fn first_traced(h: &H, conn: ConnId, dir: &str, kind: &str) -> usize {
    let conn = conn.0.to_string();
    h.s.world
        .trace()
        .lines()
        .iter()
        .position(|line| {
            let mut parts = line.split(' ').skip(1);
            parts.next() == Some(conn.as_str())
                && parts.next() == Some(dir)
                && parts.next() == Some(kind)
        })
        .unwrap_or_else(|| panic!("no {dir} {kind} on the socket"))
}

#[test]
fn an_unasked_message_on_a_follower_socket_closes_it_and_acks_nothing() {
    support::offline::run_child(
        module_path!(),
        "an_unasked_message_on_a_follower_socket_closes_it_and_acks_nothing_child",
    );
}

/// THE GUARD (THE MUTANT "no close on an unasked message" dies here): no leader in the group, Kourier online as
/// follower; the evil chat server pushes a message: the client closes 1000 at once and acks nothing, one WARN, the
/// socket's end line counts the message read and no ack written; the retry is the backoff's first step (+5 s), as after
/// a lost network; the server still holds the message. The next socket (an honest server) stays online.
#[tokio::test]
async fn an_unasked_message_on_a_follower_socket_closes_it_and_acks_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_401);
    s.world.mediator().knobs(|k| k.auto_leader = false);
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Online as follower", online_as("follower"))
        .await;
    let first = h.conn();
    let m = h.s.world.mediator().clone();
    m.knobs(|k| k.csp.deliver_unasked = true);
    assert!(
        h.net().act(|m| m.csp_incoming(ME, ANN, 7, 0, &[0x5a; 48])),
        "the message was not queued"
    );
    h.wait_state("Offline with the first retry", retry_at(at(5)))
        .await;
    closed_unacked(&h, first);
    assert_eq!(m.csp_queue_len(ME), 1, "the server dropped the message");
    let warned = warns
        .at(Level::WARN)
        .into_iter()
        .filter(|w| w == UNASKED)
        .count();
    assert_eq!(warned, 1, "the guard's WARN");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counters(&end[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_message_ack"],
            counted["out_unblock"]
        ),
        (1, 0, 0),
        "the end line of the closed socket"
    );
    equals_the_trace(&h, first, &end[0]);
    m.knobs(|k| k.csp.deliver_unasked = false);
    h.advance_to(at(5)).await;
    h.wait_state("Online again", online_as("follower")).await;
    assert_eq!(h.net().opened().len(), 2, "not one new socket");
    no_id(&warns);
    h.finish().await;
}

#[test]
fn an_unasked_message_closes_a_leader_socket_before_its_unblock_at_every_login() {
    support::offline::run_child(
        module_path!(),
        "an_unasked_message_closes_a_leader_socket_before_its_unblock_at_every_login_child",
    );
}

/// THE GUARD on a LEADER socket whose unblock cannot go yet (TH6 L4b sends it once Dry is counted; C1's open point): the
/// mediator promotes Kourier BEFORE Dry and holds Dry, so the leader is logged in and `Syncing` with no unblock. The
/// evil server pushes a message: closed 1000, nothing acked. It keeps the message and pushes it again right after the
/// next socket's login: closed again, the backoff's second step (+5 s, then +15 s), as a lost network goes; after both
/// closes the server STILL holds it and nobody asked the directory. With an honest server and Dry the third socket
/// unblocks once and leads, and only THEN takes the message: its sender is no contact and the directory does not know
/// it (L5's lookup, one call), so it is discarded and acked once, on the third socket, after its unblock, never
/// reflected. Two WARNs, two end lines. (Until L5 such a sender was held `csp-sender-lookup` and the message stayed at
/// the server to the end; the guard's proof is the server's queue after the two closes and the order of the third
/// socket's frames.)
#[tokio::test]
async fn an_unasked_message_closes_a_leader_socket_before_its_unblock_at_every_login_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_402);
    s.world.mediator().knobs(|k| {
        k.promote_before_dry = true;
        k.hold_dry = true;
    });
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", |s| matches!(s, ConnState::Syncing { .. }))
        .await;
    let first = h.conn();
    assert!(
        h.s.world.mediator().is_leader(first),
        "the socket does not lead"
    );
    let m = h.s.world.mediator().clone();
    m.knobs(|k| k.csp.deliver_unasked = true);
    assert!(
        h.net().act(|m| m.csp_incoming(ME, ANN, 9, 0, &[0x6b; 40])),
        "the message was not queued"
    );
    h.wait_state("Offline with the first retry", retry_at(at(5)))
        .await;
    closed_unacked(&h, first);
    h.advance_to(at(5)).await;
    h.wait_opened(2).await;
    h.wait_state("Offline with the second retry", retry_at(at(15)))
        .await;
    let second = h.net().opened()[1];
    closed_unacked(&h, second);
    for conn in [first, second] {
        assert_eq!(
            traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
            0,
            "an unblock before Dry"
        );
    }
    assert_eq!(
        m.csp_queue_len(ME),
        1,
        "the server dropped the unasked message"
    );
    assert!(
        h.net().directory().asked().is_empty(),
        "a closed socket asked the directory"
    );
    // The third socket takes it after its unblock: the directory does not know ANN, so it is a declared discard.
    m.declare_discard(ANN, 9);
    m.knobs(|k| {
        k.csp.deliver_unasked = false;
        k.hold_dry = false;
    });
    h.advance_to(at(15)).await;
    h.wait_state("Online again as leader", online_as("leader"))
        .await;
    let third = h.conn();
    assert_eq!(h.net().opened().len(), 3, "not one more socket");
    assert_eq!(
        traced(&h.s.world, third, "in", "csp-unblock-incoming-messages"),
        1,
        "the leader's one unblock"
    );
    assert_eq!(
        traced(&h.s.world, third, "in", "csp-incoming-message-ack"),
        1,
        "the leader did not ack the discarded message once"
    );
    let unblock = first_traced(&h, third, "in", "csp-unblock-incoming-messages");
    let delivered = first_traced(&h, third, "out", "csp-incoming-message");
    let acked = first_traced(&h, third, "in", "csp-incoming-message-ack");
    assert!(
        unblock < delivered && delivered < acked,
        "the third socket's order is not unblock, message, ack: {unblock:?} {delivered:?} {acked:?}"
    );
    assert_eq!(
        traced(&h.s.world, third, "in", "reflect"),
        0,
        "a discarded message was reflected"
    );
    let asked = h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert!(asked[0].ids == [ANN], "the lookup asked for someone else");
    assert_eq!(m.csp_queue_len(ME), 0, "the leader left the message queued");
    let warned = warns
        .at(Level::WARN)
        .into_iter()
        .filter(|w| w == UNASKED)
        .count();
    assert_eq!(warned, 2, "one WARN per closed socket");
    let ends = wait_lines(&warns, 2, count_line("end")).await;
    equals_the_trace(&h, first, &ends[0]);
    equals_the_trace(&h, second, &ends[1]);
    no_id(&warns);
    h.finish().await;
}

#[test]
fn one_kind_line_per_unhandled_kind_and_socket() {
    support::offline::run_child(
        module_path!(),
        "one_kind_line_per_unhandled_kind_and_socket_child",
    );
}

/// Five kinds Kourier does not handle (a cookie change indication, two alerts, an unknown chat server payload, the
/// mediator's devices info and an unknown mediator payload): ONE INFO line per kind with its server and libthreema's
/// name, nothing of what it says; the socket stays up; its end line counts all six payloads `in_other`. On the next
/// socket an alert is told again, once.
#[tokio::test]
async fn one_kind_line_per_unhandled_kind_and_socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install_info();
    let mut h = harness::start(scene(9_403));
    h.send(Command::Connect).await;
    h.wait_state("Online", online_as("leader")).await;
    let first = h.conn();
    let devices = d2m_proto::DevicesInfo::default().encode_to_vec();
    h.net().act(|m| {
        m.csp_raw(first, 0xd2, &[]);
        m.csp_alert(first);
        m.csp_alert(first);
        m.csp_raw(first, 0x99, b"synthetic payload");
        m.send_raw(first, datagram(kind::DEVICES_INFO, &devices));
        m.send_raw(first, datagram(0x7e, b"synthetic payload"));
    });
    let told = |l: &Line| l.message == KIND;
    let lines = wait_lines(&warns, 5, told).await;
    h.sync().await;
    let mut kinds: Vec<(String, String)> = lines
        .iter()
        .map(|l| {
            assert_eq!(l.level, Level::INFO, "the kind line's level");
            let names: Vec<&str> = l.fields.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, ["server", "kind"], "the kind line's fields");
            (
                l.field("server").expect("a server").to_owned(),
                l.field("kind").expect("a kind").to_owned(),
            )
        })
        .collect();
    kinds.sort();
    let want: Vec<(String, String)> = [
        ("chat", "DeviceCookieChangeIndication"),
        ("chat", "ServerAlert"),
        ("chat", "UnknownPayload"),
        ("mediator", "DevicesInfo"),
        ("mediator", "UnknownPayload"),
    ]
    .iter()
    .map(|(s, k)| ((*s).to_owned(), (*k).to_owned()))
    .collect();
    assert_eq!(kinds, want, "the kinds told");
    assert_eq!(
        h.s.world.mediator().closed(first),
        None,
        "a kind closed the socket"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    h.wait_state("Offline with the retry", retry_at(at(5)))
        .await;
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(counters(&end[0])["in_other"], 6, "the six payloads");
    equals_the_trace(&h, first, &end[0]);
    h.advance_to(at(5)).await;
    h.wait_state("Online again", online_as("leader")).await;
    let second = h.conn();
    h.net().act(|m| m.csp_alert(second));
    let lines = wait_lines(&warns, 6, told).await;
    assert_eq!(
        (lines[5].field("server"), lines[5].field("kind")),
        (Some("chat"), Some("ServerAlert")),
        "the new socket's alert"
    );
    no_id(&warns);
    h.finish().await;
}

#[test]
fn the_count_line_is_the_fakes_trace_at_sixty_seconds_and_at_the_end() {
    support::offline::run_child(
        module_path!(),
        "the_count_line_is_the_fakes_trace_at_sixty_seconds_and_at_the_end_child",
    );
}

/// D3 and D5: the socket opens at the start, the chat server acks the login at +5 s (so the echoes, every 10 s from the
/// login, never meet the line at +60 s); a seeded job (a text to a peer) runs at `Online`: two reflects and one box,
/// counted as the JOB's. At +60 s of the actor's clock the running line: every number the fake's trace, nothing else
/// at that moment. The mediator then closes the socket: the end line, the same numbers.
#[tokio::test]
async fn the_count_line_is_the_fakes_trace_at_sixty_seconds_and_at_the_end_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_404).with_peer(ANN).job(NewJob {
        kind: "test-job",
        to: To::Contact(ANN.to_owned()),
        body: NewBody::Text("synthetic text".to_owned()),
        only: None,
    });
    s.world
        .mediator()
        .knobs(|k| k.csp.withhold_login_ack = true);
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    h.wait_deadline(at(20)).await;
    h.advance_through(at(5)).await;
    assert!(h.net().act(|m| m.release_login_ack(conn)), "nothing held");
    h.wait_state("Online", online_as("leader")).await;
    let db = h.db();
    let done = tokio::time::timeout(WALL, async {
        while !db.read(store::send_tasks).expect("the tasks").is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(done.is_ok(), "the job did not finish");
    let m = h.s.world.mediator().clone();
    for n in 1..=5_u64 {
        h.advance_to(at(5 + 10 * n)).await;
        let echoes = usize::try_from(n).expect("fits");
        assert!(
            h.net().wait(|| m.csp_echo_requests() >= echoes, WALL).await,
            "the echo of +{} s",
            5 + 10 * n
        );
    }
    h.advance_to(at(60)).await;
    let running = wait_lines(&warns, 1, count_line("running")).await;
    let counted = counters(&running[0]);
    assert_eq!(
        (
            counted["out_reflect_job"],
            counted["out_box_job"],
            counted["out_reflect_transmit"],
            counted["out_box_transmit"],
        ),
        (2, 1, 0, 0),
        "the job's frames by origin"
    );
    assert_eq!(
        (counted["out_echo"], counted["in_echo"], counted["out_idle"]),
        (5, 5, 1),
        "the keep-alive"
    );
    equals_the_trace(&h, conn, &running[0]);
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    h.wait_state("Offline with the retry", retry_at(at(65)))
        .await;
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(counters(&end[0]), counted, "the end line moved");
    equals_the_trace(&h, conn, &end[0]);
    no_id(&warns);
    h.finish().await;
}
