//! W3b (chunk C8b): CALLS over the scripted server. Nothing is ever sent for a call but the transport
//! `<ack>`: a reply from one of the user's devices can end the call or stop the phone ringing, and
//! upstream's own call handler (an offer-ack `<receipt>`, a ringing state, identity learning) answers
//! every `<call>` it is handed. This backend registers a stanza interceptor that claims every `<call>`
//! (`src/calls.rs`); what only a real client over the Wire can show is what the client then sends: the
//! `<ack class="call">` once, after the call line is durable, and nothing else.
//!
//! Every body runs in ONE offline child under `unshare -rn` (lane decision L3) with the synthetic
//! numbers of `support::peer`; nothing here touches a network. The child records every log line of the
//! process and the parent refuses any that holds a number, a LID, a name, a text or a call id (D10).
//!
//! Wire mutants of the chunk (each one is killed by the case named): no interceptor registered
//! (`nothing_but_the_transport_ack_follows_a_call`: an offer-ack `<receipt>` appears), the handle
//! dropped right after registering (the same case), a failed write answered with `Pass`
//! (`a_failed_call_write_ends_the_connection_and_the_redelivery_lands_once`), the interceptor that
//! submits the write and returns (`no_ack_of_a_call_before_its_line`), the interceptor that formats the
//! upstream parse error (`a_call_that_does_not_parse_is_acked_and_logs_nothing_of_it`).

use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{
    Body, CallDirection, CallLine, CallMedia, CallState, ConvKey, SystemEvent, UserId,
};
use wacore_binary::Node;
use wacore_binary::builder::NodeBuilder;

use crate::receive::{self, Rig, peer_receipt, rig, rig_holding_offline};
use crate::support::capture::Capture;
use crate::support::peer;
use crate::support::{self, wire};

/// The server's time on every stanza (seconds) and in milliseconds.
const T: i64 = 1_700_000_100;
const AT: i64 = T * 1000;
/// How long a live offer rings (`calls::RING_MS`; the contract's core caps any ring at two minutes).
const RING_MS: i64 = 60_000;

// Stanza ids all start with the 15 characters the receive tests' secret list names (`3EB000000000000`),
// call ids with `CALLSYN`: a log line holding either is a leak.
const OFFER: &str = "3EB0000000000005";
const TERMINATE: &str = "3EB0000000000006";
const BARRIER: &str = "3EB0000000000007";
const OFFLINE: &str = "3EB0000000000008";
const UNREADABLE: &str = "3EB0000000000009";
const FAILING: &str = "3EB000000000000A";
const CALL_1: &str = "CALLSYN000000001";
const CALL_2: &str = "CALLSYN000000002";
const CALL_3: &str = "CALLSYN000000003";
const CALL_4: &str = "CALLSYN000000004";

/// What the logs and the output of the cases must never hold: the receive tests' list and the call ids.
fn secrets() -> Vec<&'static str> {
    let mut all = receive::secrets();
    all.push("CALLSYN");
    all
}

/// The caller as an offer names it: the peer's LID with a device.
fn creator() -> String {
    format!("{}:7@lid", peer::PEER_LID)
}

/// A `<call>` stanza from the peer holding `action`.
fn call(id: &str, action: Node) -> Node {
    NodeBuilder::new("call")
        .attr("from", format!("{}@lid", peer::PEER_LID))
        .attr("id", id)
        .attr("t", T.to_string())
        .children([action])
        .build()
}

/// A voice `<offer>` of call `call_id` by the peer.
fn offer(call_id: &str) -> Node {
    NodeBuilder::new("offer")
        .attr("call-id", call_id)
        .attr("call-creator", creator())
        .children([NodeBuilder::new("audio")
            .attr("enc", "opus")
            .attr("rate", "16000")
            .build()])
        .build()
}

/// The caller giving up (no reason: the ring ran out or he hung up).
fn terminate(call_id: &str) -> Node {
    NodeBuilder::new("terminate")
        .attr("call-id", call_id)
        .attr("call-creator", creator())
        .build()
}

/// The stanza as the server's offline delivery marks it.
fn offline(mut node: Node) -> Node {
    node.attrs.insert("offline", "1");
    node
}

/// A stanza that does not parse: an attribute that is a JID and is none. Upstream's error for it
/// prints the text (`Invalid JID: ..`), and the text is a synthetic number the grep knows.
fn unreadable(id: &str) -> Node {
    let mut node = call(id, offer(CALL_3));
    node.attrs.insert("participant", peer::ACCOUNT_PN);
    node
}

/// The call lines among the next envelopes, acknowledged as the core would; stops once `n` were
/// seen. Panics when they do not come.
async fn call_lines(rig: &mut Rig, n: usize) -> Vec<IncomingMessage> {
    let mut lines = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while lines.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        let envelope = tokio::time::timeout(left, rig.actor.ends.events.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} of {n} call lines came (frames {:?})",
                    lines.len(),
                    rig.server.frames()
                )
            })
            .expect("an envelope");
        for event in &envelope.events {
            if let BackendEvent::Message(m) = event
                && matches!(m.body, Body::System(SystemEvent::Call(_)))
            {
                lines.push(m.clone());
            }
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
    }
    lines
}

/// Nothing more than the lines already read comes within `window`: the call lines among the
/// envelopes of that time (every envelope is acknowledged).
async fn call_lines_within(rig: &mut Rig, window: Duration) -> usize {
    let mut seen = 0;
    let deadline = Instant::now() + window;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rig.actor.ends.events.recv()).await {
            Err(_) | Ok(None) => return seen,
            Ok(Some(envelope)) => {
                seen += envelope
                    .events
                    .iter()
                    .filter(|e| {
                        matches!(e, BackendEvent::Message(m)
                            if matches!(m.body, Body::System(SystemEvent::Call(_))))
                    })
                    .count();
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
        }
    }
}

fn line_of(message: &IncomingMessage) -> &CallLine {
    match &message.body {
        Body::System(SystemEvent::Call(line)) => line,
        other => panic!("not a call line: {other:?}"),
    }
}

/// What the client may send once a call (or any stanza) has come, beyond the transport acks the test
/// names: the kinds its own quiet window before showed, minus every kind that could answer a stanza
/// (`ack`, `receipt`, `call`), and the keepalive ping (an `iq` of `w:p`).
fn allowed_kinds(quiet: &[wire::Seen]) -> Vec<(String, Option<String>)> {
    let mut kinds: Vec<(String, Option<String>)> = quiet
        .iter()
        .filter(|f| !matches!(f.tag.as_str(), "ack" | "receipt" | "call"))
        .map(|f| (f.tag.clone(), f.xmlns.clone()))
        .collect();
    kinds.push(("iq".to_owned(), Some("w:p".to_owned())));
    kinds
}

/// What the client sent after the frames of `quiet`, none of which may answer a call: the acks named
/// by `acks` (each exactly once, with `class`), and otherwise only kinds of the quiet window.
fn assert_only_transport_acks(all: &[wire::Seen], quiet: &[wire::Seen], acks: &[(&str, &str)]) {
    assert_eq!(&all[..quiet.len()], quiet, "frames only ever append");
    let after = &all[quiet.len()..];
    let kinds = allowed_kinds(quiet);
    for (id, class) in acks {
        let mine: Vec<_> = after
            .iter()
            .filter(|f| f.tag == "ack" && f.attr("id") == Some(id))
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "exactly one <ack> for stanza {id}: {after:?}"
        );
        assert_eq!(mine[0].attr("class"), Some(*class), "the ack's class");
    }
    for frame in after {
        assert!(
            frame.tag != "call" && frame.tag != "receipt",
            "a <{}> followed the call(s): {after:?}",
            frame.tag
        );
        assert!(
            !frame.children.iter().any(|c| c == "offer"),
            "an offer-ack followed the call: {after:?}"
        );
        let named = frame.tag == "ack" && acks.iter().any(|(id, _)| frame.attr("id") == Some(*id));
        assert!(
            named
                || kinds
                    .iter()
                    .any(|(tag, xmlns)| *tag == frame.tag && *xmlns == frame.xmlns),
            "the client sent something no call explains: {frame:?} (of {after:?})"
        );
    }
}

/// The live offer: the peer rings, the client acknowledges the transport and nothing else, and the
/// line is shown. A barrier (a read receipt of the peer, answered by its own ack) after the call
/// and two seconds of quiet make "nothing else" a statement about the time after the call too.
async fn nothing_but_the_transport_ack_follows_a_call() {
    let mut rig = rig().await;
    rig.online().await;
    // The client's own quiet window: whatever it sends by itself is not an answer to the call.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let quiet = rig.server.frames();
    rig.server.send(&call(OFFER, offer(CALL_1)));
    rig.frame_with("ack", OFFER).await;
    rig.server.send(&peer_receipt(BARRIER, "read"));
    rig.frame_with("ack", BARRIER).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_only_transport_acks(
        &rig.server.frames(),
        &quiet,
        &[(OFFER, "call"), (BARRIER, "receipt")],
    );
    let lines = call_lines(&mut rig, 1).await;
    let m = &lines[0];
    assert_eq!(
        m.conv,
        ConvKey::Direct(UserId::from_parts("lid", peer::PEER_LID).expect("a user id"))
    );
    assert_eq!(&*m.local, format!("call:{CALL_1}"));
    assert!(!m.catch_up, "a live offer is no catch-up");
    let line = line_of(m);
    assert_eq!(line.direction, CallDirection::Incoming);
    assert!(!line.group);
    assert_eq!(line.media, Some(CallMedia::Voice));
    assert_eq!(
        line.state,
        CallState::Ringing {
            deadline: AT + RING_MS
        }
    );
    assert_eq!(
        call_lines_within(&mut rig, Duration::from_millis(300)).await,
        0
    );
    let _ = rig.finish().await;
}

/// Rule (a) for a call: while the account's file is write-locked the interceptor's write waits, and
/// the client sends no `<ack>` for the call until the line is committed. A claim that submitted the
/// write and returned would acknowledge at once.
async fn no_ack_of_a_call_before_its_line() {
    let mut rig = rig().await;
    rig.online().await;
    let holder = receive::raw(rig.dir.path());
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("the write lock of the file");
    rig.server.send(&call(OFFER, offer(CALL_1)));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        rig.frames_with("ack", OFFER),
        0,
        "the call was acknowledged while its line waited for the file"
    );
    holder.execute_batch("ROLLBACK").expect("release");
    rig.frame_with("ack", OFFER).await;
    assert_eq!(call_lines(&mut rig, 1).await.len(), 1);
    assert_eq!(rig.frames_with("receipt", OFFER), 0);
    let _ = rig.finish().await;
}

/// An offer from the server's offline delivery (the drain): a missed call at once (no time left),
/// marked `catch_up`, and again nothing but the transport ack. The interceptor is registered before
/// the client starts, so the queue's calls are claimed too.
async fn an_offline_offer_is_a_missed_line_and_nothing_else_is_sent() {
    let mut rig = rig_holding_offline().await;
    rig.server.send(&offline(call(OFFLINE, offer(CALL_2))));
    rig.server.send(&wire::offline_end(1));
    rig.frame_with("ack", OFFLINE).await;
    // The core's row of the drain must be acknowledged before the account is `Online`.
    let lines = call_lines(&mut rig, 1).await;
    rig.online().await;
    let m = &lines[0];
    assert_eq!(&*m.local, format!("call:{CALL_2}"));
    assert!(m.catch_up, "a call of the drain is a catch-up line");
    assert_eq!(
        line_of(m).state,
        CallState::Ringing { deadline: AT },
        "a replayed offer has no ring time left"
    );
    rig.server.send(&peer_receipt(BARRIER, "read"));
    rig.frame_with("ack", BARRIER).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let frames = rig.server.frames();
    for frame in &frames {
        assert!(
            frame.tag != "call" && frame.tag != "receipt",
            "a <{}> was sent: {frames:?}",
            frame.tag
        );
        assert!(!frame.children.iter().any(|c| c == "offer"));
    }
    let acks: Vec<_> = frames.iter().filter(|f| f.tag == "ack").collect();
    assert_eq!(acks.len(), 2, "the call's ack and the barrier's: {acks:?}");
    let _ = rig.finish().await;
}

/// Critic M10: a `<call>` runs on a task of its own, so a caller's hang-up can be handled before his
/// offer. Both stanzas are sent back to back; each is acknowledged once, nothing else is sent, and
/// the two lines (written in whatever order) join to the same missed call either way.
async fn a_terminate_before_its_offer_is_two_lines_that_join_to_a_missed_call() {
    let mut rig = rig().await;
    rig.online().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let quiet = rig.server.frames();
    rig.server.send(&call(TERMINATE, terminate(CALL_4)));
    rig.server.send(&call(OFFER, offer(CALL_4)));
    rig.frame_with("ack", TERMINATE).await;
    rig.frame_with("ack", OFFER).await;
    rig.server.send(&peer_receipt(BARRIER, "read"));
    rig.frame_with("ack", BARRIER).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_only_transport_acks(
        &rig.server.frames(),
        &quiet,
        &[(TERMINATE, "call"), (OFFER, "call"), (BARRIER, "receipt")],
    );
    let lines = call_lines(&mut rig, 2).await;
    assert_eq!(&*lines[0].local, format!("call:{CALL_4}"));
    assert_eq!(lines[0].local, lines[1].local);
    let (a, b) = (line_of(&lines[0]), line_of(&lines[1]));
    let states = [a.state, b.state];
    assert!(
        states
            .iter()
            .any(|s| matches!(s, CallState::Ringing { .. }))
            && states.iter().any(|s| matches!(s, CallState::Missed)),
        "one ringing line and one missed line: {states:?}"
    );
    assert_eq!(a.join(b), b.join(a), "the order does not matter");
    assert_eq!(a.join(b).state, CallState::Missed);
    let _ = rig.finish().await;
}

/// D10 and critic M9a: a `<call>` that does not parse (an attribute that is a JID and is none; the
/// text is the account's synthetic number) is claimed all the same: the transport ack, no line, and
/// no log line of the child holds the text of the upstream error that names the number.
async fn a_call_that_does_not_parse_is_acked_and_logs_nothing_of_it() {
    let mut rig = rig().await;
    rig.online().await;
    rig.server.send(&unreadable(UNREADABLE));
    rig.frame_with("ack", UNREADABLE).await;
    rig.server.send(&peer_receipt(BARRIER, "read"));
    rig.frame_with("ack", BARRIER).await;
    assert_eq!(
        call_lines_within(&mut rig, Duration::from_millis(500)).await,
        0,
        "a call that does not parse shows no line"
    );
    assert_eq!(rig.frames_with("receipt", UNREADABLE), 0);
    assert!(
        rig.server
            .frames()
            .iter()
            .all(|f| f.tag != "receipt" && f.tag != "call"),
        "something answered the call: {:?}",
        rig.server.frames()
    );
    let _ = rig.finish().await;
}

/// The write of a call line FAILS (the crate's own seam, as a full disk would): the connection ends,
/// no `<ack>` and no offer-ack leaves for the call, and the server's redelivery lands once on the
/// next connection. An interceptor that let upstream answer after a failed write (`Pass`) would put
/// upstream's own offer-ack on the Wire.
async fn a_failed_call_write_ends_the_connection_and_the_redelivery_lands_once() {
    let mut rig = rig().await;
    rig.online().await;
    let before = rig.states.lock().expect("lock").len();
    kourier_whatsapp::fault::arm(kourier_whatsapp::fault::EVENT_WRITE, 1);
    let stanza = call(FAILING, offer(CALL_1));
    rig.server.send(&stanza);
    assert!(
        rig.server.wait_opened(2, Duration::from_secs(30)).await,
        "no new connection after the failed write (frames {:?})",
        rig.server.frames()
    );
    assert_eq!(
        rig.frames_with("ack", FAILING),
        0,
        "the call was acknowledged before its line was durable"
    );
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    rig.server.send(&stanza);
    rig.frame_with("ack", FAILING).await;
    assert_eq!(call_lines(&mut rig, 1).await.len(), 1);
    assert_eq!(
        call_lines_within(&mut rig, Duration::from_millis(500)).await,
        0
    );
    assert_eq!(rig.frames_with("ack", FAILING), 1, "one ack for one call");
    assert!(
        rig.server
            .frames()
            .iter()
            .all(|f| f.tag != "receipt" && f.tag != "call"),
        "something answered the call: {:?}",
        rig.server.frames()
    );
    kourier_whatsapp::fault::disarm();
    let _ = rig.finish().await;
}

/// The case the child runs (set by the parent).
const CASE: &str = "WA_CALLS_CASE";

/// One parent per case: the case runs in the offline child below, and nothing the child printed holds
/// the fixture's number, LID, name, texts or call ids (D10).
fn parent(case: &str) {
    let output = support::offline::run_child_output(module_path!(), "calls_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn nothing_but_the_transport_ack_follows_a_call_over_the_wire() {
    parent("ack");
}

#[test]
fn no_ack_of_a_call_before_its_line_over_the_wire() {
    parent("park");
}

#[test]
fn an_offline_offer_is_a_missed_line_and_nothing_else_is_sent_over_the_wire() {
    parent("offline");
}

#[test]
fn a_terminate_before_its_offer_is_two_lines_that_join_to_a_missed_call_over_the_wire() {
    parent("terminate_first");
}

#[test]
fn a_call_that_does_not_parse_is_acked_and_logs_nothing_of_it_over_the_wire() {
    parent("unreadable");
}

#[test]
fn a_failed_call_write_ends_the_connection_and_the_redelivery_lands_once_over_the_wire() {
    parent("write");
}

/// The child (a no-op as an ordinary test): the case the parent named. Every log line of the process
/// is recorded, and the case may have made none that holds a secret of the fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "ack" => nothing_but_the_transport_ack_follows_a_call().await,
        "park" => no_ack_of_a_call_before_its_line().await,
        "offline" => an_offline_offer_is_a_missed_line_and_nothing_else_is_sent().await,
        "terminate_first" => {
            a_terminate_before_its_offer_is_two_lines_that_join_to_a_missed_call().await;
        }
        "unreadable" => a_call_that_does_not_parse_is_acked_and_logs_nothing_of_it().await,
        "write" => a_failed_call_write_ends_the_connection_and_the_redelivery_lands_once().await,
        other => panic!("unknown case {other}"),
    }
    // Every case logs at least the connection's states: a check that saw no line proves nothing. The
    // same for upstream's own records (fix F6): the `log` bridge is installed and heard some, so the
    // grep below covers what whatsapp-rust writes, not only what Kourier does.
    assert!(
        log.own_lines() >= 1,
        "the recorder saw {} lines of the crate",
        log.own_lines()
    );
    assert!(
        log.upstream_records() >= 1,
        "the recorder heard no record of upstream's crates: the `log` bridge is not working"
    );
    assert_eq!(
        log.leaks(&secrets()),
        Vec::<String>::new(),
        "a log line holds a secret of the fixture (the targets are named)"
    );
}
