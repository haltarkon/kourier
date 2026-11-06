//! Calls (W3b, chunk C7; brief 5.4): one test per row of the table, the claim, and the interceptor over
//! a real store. The Wire tests that prove what upstream's client does with a claimed stanza are
//! `tests/calls.rs`. Numbers and ids are the fictitious 555-01xx range.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use kourier_backend_api::{BackendEvent, Envelope, FakeClock, IncomingMessage, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::{
    Body, CallDirection, CallLine, CallMedia, CallState, ConvKey, Origin, SystemEvent, UserId,
};
use tokio::sync::Notify;
use wacore::types::events::Event;
use wacore_binary::builder::NodeBuilder;
use wacore_binary::{Node, OwnedNodeRef};

use super::*;
use crate::fault;
use crate::map::OwnIds;
use crate::stage2::{Stage2, Timing};
use crate::store::{KourierStore, StoreOwner};
use crate::translate;

const NOW: u64 = 1_700_000_300_000;
/// The server's time on every stanza (seconds).
const T: i64 = 1_700_000_000;
const AT: i64 = T * 1000;
const PEER_PN: &str = "15550100101@s.whatsapp.net";
const PEER_LID: &str = "99900100101@lid";
const GROUP: &str = "120363000000000001@g.us";
const OWN_PN: &str = "15550100999";
const OWN_LID: &str = "99900100999";
const CALL: &str = "CALL0A1B2C3D";
/// A text that is no jid (a bare number: it reads as a server name and names none) and carries a number.
/// It marshals as a plain string, which a jid with an unknown server would not.
pub(crate) const BAD_JID: &str = "15550100777";

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user id")
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: Some(user("pn", OWN_PN)),
    }
}

/// The node of a `<call>` stanza from `from` holding `action`.
fn call_node(from: &str, offline: bool, action: Node) -> Node {
    let mut builder = NodeBuilder::new("call")
        .attr("from", from)
        .attr("id", "STANZA-1")
        .attr("t", T.to_string());
    if offline {
        builder = builder.attr("offline", "0");
    }
    builder.children([action]).build()
}

fn offer(creator: &str, video: bool, group: Option<&str>) -> NodeBuilder {
    let mut builder = NodeBuilder::new("offer")
        .attr("call-id", CALL)
        .attr("call-creator", creator);
    if let Some(group) = group {
        builder = builder.attr("group-jid", group);
    }
    let mut kids = vec![
        NodeBuilder::new("audio")
            .attr("enc", "opus")
            .attr("rate", "16000")
            .build(),
    ];
    if video {
        kids.push(NodeBuilder::new("video").build());
    }
    builder.children(kids)
}

fn terminate(creator: &str, reason: Option<&str>, duration: Option<u32>) -> Node {
    let mut builder = NodeBuilder::new("terminate")
        .attr("call-id", CALL)
        .attr("call-creator", creator);
    if let Some(reason) = reason {
        builder = builder.attr("reason", reason);
    }
    if let Some(duration) = duration {
        builder = builder.attr("duration", duration.to_string());
    }
    builder.build()
}

/// The node as the receive path hands it to an interceptor.
fn owned(node: &Node) -> OwnedNodeRef {
    let packed = wacore_binary::marshal(node).expect("marshal");
    let bytes = wacore_binary::util::unpack(&packed)
        .expect("unpack")
        .into_owned();
    OwnedNodeRef::new(Bytes::from(bytes)).expect("a node")
}

/// The store of a device that knows no LID: nothing is mapped.
fn nothing_mapped(_lid: &str) -> Option<String> {
    None
}

/// The store of a device that filed no call line.
fn nothing_filed(_call_id: &str, _person: &UserId) -> Option<ConvKey> {
    None
}

fn read_of(node: &Node, ids: &OwnIds) -> Read {
    read(
        owned(node).get(),
        NOW,
        ids,
        Lookups {
            number_of: &nothing_mapped,
            filed_chat: &nothing_filed,
        },
    )
}

/// The one line `node` stands for, which must pass the contract's own check.
fn line_of(node: &Node, ids: &OwnIds) -> Option<IncomingMessage> {
    line_with(node, ids, &nothing_mapped)
}

/// [`line_of`] on a device whose store maps LIDs as `number_of` says.
fn line_with(node: &Node, ids: &OwnIds, number_of: NumberOf<'_>) -> Option<IncomingMessage> {
    line_in(
        node,
        ids,
        Lookups {
            number_of,
            filed_chat: &nothing_filed,
        },
    )
}

/// [`line_of`] on a device whose store answers as `store` says.
fn line_in(node: &Node, ids: &OwnIds, store: Lookups<'_>) -> Option<IncomingMessage> {
    match read(owned(node).get(), NOW, ids, store) {
        Read::Line(event) => {
            let (envelope, _waiter) = Envelope::durable(vec![(*event).clone()]);
            envelope.validate().expect("the row is a valid envelope");
            match *event {
                BackendEvent::Message(message) => Some(message),
                other => panic!("not a message: {other:?}"),
            }
        }
        Read::Nothing => None,
        Read::Unreadable => panic!("the stanza did not parse"),
    }
}

fn call_line(message: &IncomingMessage) -> &CallLine {
    match &message.body {
        Body::System(SystemEvent::Call(line)) => line,
        other => panic!("not a call line: {other:?}"),
    }
}

#[test]
fn a_live_one_to_one_offer_rings_for_the_whole_ring_time_by_its_caller() {
    for (video, media) in [(false, CallMedia::Voice), (true, CallMedia::Video)] {
        let node = call_node(PEER_PN, false, offer(PEER_PN, video, None).build());
        let m = line_of(&node, &own()).expect("a line");
        assert_eq!(m.conv, ConvKey::Direct(user("pn", "15550100101")));
        assert_eq!(&*m.local, format!("call:{CALL}"));
        assert_eq!(
            m.origin,
            Origin::Remote {
                author: user("pn", "15550100101"),
                device: None
            }
        );
        let line = call_line(&m);
        assert_eq!(line.direction, CallDirection::Incoming);
        assert!(!line.group);
        assert_eq!(
            line.state,
            CallState::Ringing {
                deadline: AT + RING_MS
            }
        );
        assert_eq!(line.media, Some(media));
        assert_eq!(line.by, Some(user("pn", "15550100101")));
        assert_eq!(
            (m.sent_at, m.server_at, m.received_at),
            (AT, Some(AT), NOW as i64)
        );
        assert!(!m.catch_up && !m.silent);
    }
}

/// The creator's device is the origin's device, and the person is the creator's, the same in every action of
/// one call whatever the device or the namespace the stanza came from.
#[test]
fn a_caller_who_is_the_sender_is_placed_with_his_device() {
    for from in [PEER_LID, "99900100101:2@lid", "99900100101@hosted.lid"] {
        let node = call_node(from, false, offer("99900100101:7@lid", false, None).build());
        let m = line_of(&node, &own()).expect("a line");
        assert_eq!(
            m.conv,
            ConvKey::Direct(user("lid", "99900100101")),
            "{from}"
        );
        assert_eq!(
            m.origin,
            Origin::Remote {
                author: user("lid", "99900100101"),
                device: Some(7)
            }
        );
    }
    // A hosted device of a number is the number's person.
    let node = call_node(
        "15550100101@hosted",
        false,
        offer(PEER_PN, false, None).build(),
    );
    let m = line_of(&node, &own()).expect("a line");
    assert_eq!(m.conv, ConvKey::Direct(user("pn", "15550100101")));
}

/// Fix F1 (privacy review HIGH 1): `call-creator` is a claim of the sending device. A line is placed only
/// for a creator who IS the sender, or whose other id the store's mapping ties to the sender. Everybody
/// else gets nothing, and so does an offer whose own `caller_pn` names the sender's number: the caller
/// writes it, so it vouches for nothing (the re-check's probe: a stranger B names A's LID as the creator
/// and his OWN number as `caller_pn`, the store maps nothing). The rows are (what the stanza says, what
/// the store maps, the line or nothing); the old rule placed all of them.
#[test]
fn a_creator_the_sender_does_not_prove_to_be_himself_gets_no_line() {
    const OTHER_PN: &str = "15550100102@s.whatsapp.net";
    let number_of_peer = |lid: &str| (lid == "99900100101").then(|| "15550100101".to_owned());
    let number_of_other = |lid: &str| (lid == "99900100101").then(|| "15550100102".to_owned());
    let offer_with_pn =
        |creator: &str, pn: &str| offer(creator, false, None).attr("caller_pn", pn).build();

    struct Row<'a> {
        what: &'static str,
        from: &'static str,
        action: Node,
        map: NumberOf<'a>,
        /// The conversation of the line, `None` for nothing.
        line: Option<ConvKey>,
    }
    let pn_peer = ConvKey::Direct(user("pn", "15550100101"));
    let lid_peer = ConvKey::Direct(user("lid", "99900100101"));
    let rows = [
        Row {
            what: "the same id",
            from: PEER_PN,
            action: offer(PEER_PN, false, None).build(),
            map: &nothing_mapped,
            line: Some(pn_peer.clone()),
        },
        Row {
            what: "the sender's number, a creator LID, nothing mapped",
            from: PEER_PN,
            action: offer(PEER_LID, false, None).build(),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "the other id of the same person, through the mapping (a LID creator)",
            from: PEER_PN,
            action: offer(PEER_LID, false, None).build(),
            map: &number_of_peer,
            line: Some(lid_peer.clone()),
        },
        Row {
            what: "the other id of the same person, through the mapping (a number creator)",
            from: PEER_LID,
            action: offer(PEER_PN, false, None).build(),
            map: &number_of_peer,
            line: Some(pn_peer.clone()),
        },
        Row {
            what: "a number creator, a sender LID the store does not know",
            from: PEER_LID,
            action: offer(PEER_PN, false, None).build(),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "a stranger's number as the creator",
            from: PEER_PN,
            action: offer(OTHER_PN, false, None).build(),
            map: &number_of_peer,
            line: None,
        },
        Row {
            what: "a creator LID the store maps to ANOTHER number",
            from: PEER_PN,
            action: offer(PEER_LID, false, None).build(),
            map: &number_of_other,
            line: None,
        },
        Row {
            what: "a number creator whose number the store gives to another LID",
            from: PEER_LID,
            action: offer(OTHER_PN, false, None).build(),
            map: &number_of_peer,
            line: None,
        },
        Row {
            what: "a stranger B names A's LID as the creator and his own number as caller_pn, nothing mapped",
            from: OTHER_PN,
            action: offer_with_pn(PEER_LID, OTHER_PN),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "the offer's caller_pn is the sender's number, nothing mapped: the caller wrote it",
            from: PEER_PN,
            action: offer_with_pn(PEER_LID, PEER_PN),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "the offer's caller_pn is the sender's number, the store maps the LID to another",
            from: PEER_PN,
            action: offer_with_pn(PEER_LID, PEER_PN),
            map: &number_of_other,
            line: None,
        },
        Row {
            what: "the offer's caller_pn is another number than the sender's, nothing mapped",
            from: PEER_PN,
            action: offer_with_pn(PEER_LID, OTHER_PN),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "the mapping decides whatever caller_pn says",
            from: PEER_PN,
            action: offer_with_pn(PEER_LID, OTHER_PN),
            map: &number_of_peer,
            line: Some(lid_peer.clone()),
        },
        Row {
            what: "a terminate of a creator LID, nothing mapped",
            from: PEER_PN,
            action: terminate(PEER_LID, None, None),
            map: &nothing_mapped,
            line: None,
        },
        Row {
            what: "a terminate of the same person through the mapping",
            from: PEER_PN,
            action: terminate(PEER_LID, None, None),
            map: &number_of_peer,
            line: Some(lid_peer.clone()),
        },
        Row {
            what: "a stranger's terminate for a contact's call",
            from: OTHER_PN,
            action: terminate(PEER_PN, None, None),
            map: &number_of_peer,
            line: None,
        },
    ];
    for row in rows {
        let node = call_node(row.from, false, row.action);
        let got = line_with(&node, &own(), row.map);
        assert_eq!(got.map(|m| m.conv), row.line, "{}", row.what);
    }
    // Where a stranger cannot ring: a group offer by a creator who is not the member the stanza comes
    // from, and a group's terminate whose participant is not the creator (the routed sender is the
    // participant, as upstream's own check reads it).
    let node = call_node(OTHER_PN, false, offer(PEER_PN, true, Some(GROUP)).build());
    assert!(line_of(&node, &own()).is_none(), "a stranger's group ring");
    let node = call_node(PEER_PN, false, offer(PEER_PN, true, Some(GROUP)).build());
    assert!(line_of(&node, &own()).is_some());
    let in_group = |participant: Option<&str>| {
        let mut node = call_node(
            GROUP,
            false,
            terminate(PEER_PN, Some("group_call_ended"), None),
        );
        if let Some(participant) = participant {
            node.attrs.insert("participant", participant);
        }
        node
    };
    assert!(line_of(&in_group(None), &own()).is_none(), "nobody vouches");
    assert!(line_of(&in_group(Some(OTHER_PN)), &own()).is_none());
    let m = line_of(&in_group(Some(PEER_PN)), &own()).expect("the creator's own terminate");
    assert_eq!(m.conv.encode(), "group:wag:120363000000000001");
    // The participant is the sender when the stanza names one, whatever `from` says.
    let m = line_of(&in_group(Some("15550100101:3@s.whatsapp.net")), &own()).expect("a line");
    assert!(call_line(&m).group);
    // A stranger's stanza that names one of OUR ids as the creator reports nothing, with or without
    // the mapping.
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    let node = call_node(PEER_PN, false, terminate(&mine, None, None));
    assert!(line_with(&node, &own(), &number_of_peer).is_none());
}

#[test]
fn an_offer_from_the_offline_queue_has_no_time_left_and_is_a_catch_up() {
    let node = call_node(PEER_PN, true, offer(PEER_PN, false, None).build());
    let m = line_of(&node, &own()).expect("a line");
    let line = call_line(&m);
    // A deadline at the signal's own time: nothing left, the core makes it a missed call at once.
    assert_eq!(line.state, CallState::Ringing { deadline: AT });
    assert!(m.catch_up);
}

#[test]
fn a_group_offer_rings_in_the_group_and_names_the_ringer_but_no_kind() {
    let node = call_node(PEER_PN, false, offer(PEER_PN, true, Some(GROUP)).build());
    let m = line_of(&node, &own()).expect("a line");
    assert_eq!(m.conv.encode(), "group:wag:120363000000000001");
    assert_eq!(&*m.local, format!("gcall:{CALL}"));
    let line = call_line(&m);
    assert!(line.group);
    assert_eq!(
        line.state,
        CallState::Ringing {
            deadline: AT + RING_MS
        }
    );
    assert_eq!(line.media, None);
    assert_eq!(line.by, Some(user("pn", "15550100101")));
    // A group offer that names no group is not placed.
    let node = call_node(
        PEER_PN,
        false,
        offer(PEER_PN, false, Some("15550100102@s.whatsapp.net")).build(),
    );
    assert!(line_of(&node, &own()).is_none());
}

#[test]
fn the_caller_giving_up_or_the_ring_running_out_is_a_missed_call() {
    for reason in [None, Some("timeout"), Some("group_call_ended")] {
        let node = call_node(PEER_PN, false, terminate(PEER_PN, reason, None));
        let m = line_of(&node, &own()).expect("a line");
        assert_eq!(
            m.conv,
            ConvKey::Direct(user("pn", "15550100101")),
            "{reason:?}"
        );
        assert_eq!(&*m.local, format!("call:{CALL}"));
        let line = call_line(&m);
        assert_eq!(line.state, CallState::Missed, "{reason:?}");
        assert_eq!(line.direction, CallDirection::Incoming);
        assert_eq!(line.media, None);
        assert_eq!(line.by, Some(user("pn", "15550100101")));
        assert!(matches!(m.origin, Origin::Remote { .. }), "{reason:?}");
        assert!(!m.catch_up);
    }
    // From the offline queue it is a catch-up.
    let node = call_node(PEER_PN, true, terminate(PEER_PN, None, None));
    assert!(line_of(&node, &own()).expect("a line").catch_up);
    // A terminate from a group, sent by its creator (the participant), is a missed group call in
    // that group.
    let mut node = call_node(
        GROUP,
        false,
        terminate(PEER_PN, Some("group_call_ended"), None),
    );
    node.attrs.insert("participant", PEER_PN);
    let m = line_of(&node, &own()).expect("a line");
    assert_eq!(m.conv.encode(), "group:wag:120363000000000001");
    assert_eq!(&*m.local, format!("gcall:{CALL}"));
    assert!(call_line(&m).group);
}

/// D2 (his words: "missed until the phone says"): the caller's dismiss of the callee's other devices is
/// the CALLER's word. It reports nothing, and nothing the caller says is ever labelled as one of our own
/// devices speaking.
#[test]
fn a_call_answered_or_declined_elsewhere_reports_nothing() {
    for reason in ["accepted_elsewhere", "rejected_elsewhere"] {
        let node = call_node(PEER_PN, false, terminate(PEER_PN, Some(reason), None));
        assert!(
            matches!(read_of(&node, &own()), Read::Nothing),
            "{reason} reported something"
        );
    }
    // Every other reason is some device's word, not the caller giving up; a connected call was not
    // missed.
    for (reason, duration) in [
        (Some("busy"), None),
        (Some("a reason from the future"), None),
        (None, Some(31)),
        (Some("timeout"), Some(1)),
    ] {
        let node = call_node(PEER_PN, false, terminate(PEER_PN, reason, duration));
        assert!(
            matches!(read_of(&node, &own()), Read::Nothing),
            "{reason:?} {duration:?}"
        );
    }
    // A duration of nought is no call.
    let node = call_node(PEER_PN, false, terminate(PEER_PN, None, Some(0)));
    assert!(line_of(&node, &own()).is_some());
}

/// An `<accept>` of the call `CALL` by `creator`.
fn accept(creator: &str) -> Node {
    NodeBuilder::new("accept")
        .attr("call-id", CALL)
        .attr("call-creator", creator)
        .children([NodeBuilder::new("audio")
            .attr("enc", "opus")
            .attr("rate", "16000")
            .build()])
        .build()
}

/// A store that filed the line of `CALL` in the peer's number chat: the chat for the peer in either form, nothing for
/// anybody else or another call. It counts how often it is asked.
fn peer_filed(asked: &AtomicUsize) -> impl Fn(&str, &UserId) -> Option<ConvKey> + '_ {
    move |call_id: &str, person: &UserId| {
        asked.fetch_add(1, Ordering::SeqCst);
        let peer = [user("pn", "15550100101"), user("lid", "99900100101")];
        (call_id == CALL && peer.contains(person))
            .then(|| ConvKey::Direct(user("pn", "15550100101")))
    }
}

/// THE OWN ANSWER (R1's finding: a call answered on the test phone ended `missed`): an accept that one of OUR devices
/// sent (every person among `from` and `participant` is one of our ids, the server's routing) answers the 1:1 line
/// Kourier filed for the call, from that device; the caller's later terminate cannot take it back, and a call never
/// answered by our own word still ends missed at its deadline (D2 unchanged). Mutant: the accept's arm of `line` removed
/// (an accept then goes the creator's way and says nothing).
#[test]
fn an_accept_of_our_own_phone_answers_the_filed_line_on_another_device() {
    let asked = AtomicUsize::new(0);
    let filed = peer_filed(&asked);
    let store = Lookups {
        number_of: &nothing_mapped,
        filed_chat: &filed,
    };
    let peer = user("pn", "15550100101");
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    for (from, participant, device) in [
        (mine.as_str(), None, None),
        ("99900100999@lid", None, None),
        ("99900100999:4@lid", None, Some(4)),
        // Routed through a participant: both ours, the device the participant's.
        (mine.as_str(), Some("99900100999:2@lid"), Some(2)),
        // A call-scoped wrapper: the call's own address is no person, the participant is our device.
        ("CALL0A1B2C3D@call", Some("99900100999:2@lid"), Some(2)),
    ] {
        let mut node = call_node(from, false, accept(PEER_PN));
        if let Some(participant) = participant {
            node.attrs.insert("participant", participant);
        }
        let m = line_in(&node, &own(), store).expect("our phone answered");
        assert_eq!(m.conv, ConvKey::Direct(peer.clone()), "{from}");
        assert_eq!(&*m.local, format!("call:{CALL}"));
        assert_eq!(m.origin, Origin::OwnOtherDevice { device }, "{from}");
        let line = call_line(&m);
        assert_eq!(line.state, CallState::Answered);
        assert_eq!(line.direction, CallDirection::Incoming);
        assert!(!line.group);
        assert_eq!(line.by, Some(peer.clone()));
        assert_eq!((m.sent_at, m.server_at), (AT, Some(AT)));
        assert!(!m.catch_up);
    }
    // From the offline queue it is a catch-up, still answered.
    let m = line_in(&call_node(&mine, true, accept(PEER_PN)), &own(), store).expect("a line");
    assert!(m.catch_up);

    // The whole call: the ring, our phone's accept, then the caller's dismiss and hang-up in any order. Answered is the
    // top of the order: the dismiss says nothing, a hang-up without a duration says "missed" and cannot lower it, and
    // the ring's deadline passing changes nothing.
    let ring = line_of(
        &call_node(PEER_PN, false, offer(PEER_PN, false, None).build()),
        &own(),
    )
    .expect("the ring");
    let answer = line_in(&call_node(&mine, false, accept(PEER_PN)), &own(), store).expect("ours");
    assert!(matches!(
        read_of(
            &call_node(
                PEER_PN,
                false,
                terminate(PEER_PN, Some("accepted_elsewhere"), None)
            ),
            &own()
        ),
        Read::Nothing
    ));
    // The hang-up after a connected call carries its duration: nothing more to say (the contract has no duration).
    assert!(matches!(
        read_of(
            &call_node(PEER_PN, false, terminate(PEER_PN, None, Some(20))),
            &own()
        ),
        Read::Nothing
    ));
    let hang_up = line_of(
        &call_node(PEER_PN, false, terminate(PEER_PN, None, None)),
        &own(),
    )
    .expect("a hang-up with no duration");
    let (ring, answer, hang_up) = (
        call_line(&ring).clone(),
        call_line(&answer).clone(),
        call_line(&hang_up).clone(),
    );
    for joined in [
        ring.join(&answer).join(&hang_up),
        hang_up.join(&ring).join(&answer),
        answer.join(&hang_up).join(&ring),
    ] {
        assert_eq!(joined.state, CallState::Answered);
        assert_eq!(joined.settled(AT + RING_MS + 1).state, CallState::Answered);
        assert_eq!(joined.media, Some(CallMedia::Voice));
    }

    // D2 unchanged: with no word of ours, the caller's "accepted elsewhere" leaves the ring to its deadline: missed.
    assert_eq!(
        ring.settled(AT + RING_MS).state,
        CallState::Missed,
        "the caller's dismiss is not our word"
    );
}

/// Only OUR device's accept says anything, and only for a 1:1 line Kourier filed in the creator's chat: the caller's or
/// a stranger's accept, an accept routed for a person who is not us, an accept while our ids are not known, of a call
/// we made, or for a call with no filed line (none seen, a group's, another person's chat) shows nothing, and the store
/// is asked only for ours. Mutant: `own_sender` takes any sender (its `is_own` check dropped).
#[test]
fn an_accept_of_anybody_else_or_of_a_call_kourier_filed_no_line_of_says_nothing() {
    let asked = AtomicUsize::new(0);
    let filed = peer_filed(&asked);
    let store = Lookups {
        number_of: &nothing_mapped,
        filed_chat: &filed,
    };
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    const OTHER_PN: &str = "15550100102@s.whatsapp.net";
    // The caller's own accept (as on a call WE made, but the creator is the peer), a stranger's, and our id routed beside
    // somebody else's.
    let mut beside = call_node(&mine, false, accept(PEER_PN));
    beside.attrs.insert("participant", PEER_PN);
    let mut routed_for_peer = call_node(PEER_PN, false, accept(PEER_PN));
    routed_for_peer
        .attrs
        .insert("participant", "99900100999:2@lid");
    for (what, node) in [
        ("the caller", call_node(PEER_PN, false, accept(PEER_PN))),
        ("a stranger", call_node(OTHER_PN, false, accept(PEER_PN))),
        ("ours beside the peer", beside),
        ("the peer beside ours", routed_for_peer),
    ] {
        assert!(line_in(&node, &own(), store).is_none(), "{what}");
    }
    assert_eq!(
        asked.load(Ordering::SeqCst),
        0,
        "the store is asked only for ours"
    );
    // Our ids not known yet: nothing can be told apart, nothing is said.
    assert!(
        line_in(
            &call_node(&mine, false, accept(PEER_PN)),
            &OwnIds::default(),
            store
        )
        .is_none()
    );
    // A call we made: the creator is us.
    assert!(line_in(&call_node(&mine, false, accept(&mine)), &own(), store).is_none());
    // Ours, but Kourier filed no line in this creator's chat (the store answers for the peer only).
    assert!(line_in(&call_node(&mine, false, accept(OTHER_PN)), &own(), store).is_none());
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    // And ours with no filed line at all.
    assert!(
        line_with(
            &call_node(&mine, false, accept(PEER_PN)),
            &own(),
            &nothing_mapped
        )
        .is_none()
    );
}

#[test]
fn nothing_says_it_was_one_of_our_own_devices() {
    // Whatever the caller sends, no line ever has the origin of our own device.
    let actions = [
        offer(PEER_PN, false, None).build(),
        offer(PEER_PN, false, Some(GROUP)).build(),
        terminate(PEER_PN, None, None),
        terminate(PEER_PN, Some("accepted_elsewhere"), None),
        terminate(PEER_PN, Some("rejected_elsewhere"), None),
    ];
    for action in actions {
        for offline in [false, true] {
            let node = call_node(PEER_PN, offline, action.clone());
            if let Some(m) = line_of(&node, &own()) {
                assert!(
                    matches!(m.origin, Origin::Remote { .. }),
                    "{:?} was labelled as ours",
                    action.tag
                );
            }
        }
    }
}

#[test]
fn a_call_our_own_phone_made_or_one_of_our_ids_sent_reports_nothing() {
    // The creator is us: an outgoing call.
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    for creator in [mine.as_str(), "99900100999:3@lid"] {
        for action in [
            offer(creator, false, None).build(),
            offer(creator, false, Some(GROUP)).build(),
            terminate(creator, None, None),
        ] {
            let node = call_node(PEER_PN, false, action);
            assert!(matches!(read_of(&node, &own()), Read::Nothing), "{creator}");
        }
    }
    // The terminate comes from our own id for somebody else's call.
    let node = call_node(&mine, false, terminate(PEER_PN, None, None));
    assert!(matches!(read_of(&node, &own()), Read::Nothing));
    // Before the ids are known nothing can be told apart (the actor reads them before any client runs):
    // our own phone's call is written as a stranger's. A stranger's stanza that names our number is
    // refused all the same, since its creator is not its sender.
    let node = call_node(&mine, false, terminate(&mine, None, None));
    assert!(line_of(&node, &OwnIds::default()).is_some());
    let node = call_node(PEER_PN, false, terminate(&mine, None, None));
    assert!(line_of(&node, &OwnIds::default()).is_none());
}

#[test]
fn a_group_calls_own_address_and_a_server_place_no_terminate() {
    for from in ["CALLID123@call", "s.whatsapp.net", "status@broadcast"] {
        let node = call_node(from, false, terminate(PEER_PN, None, None));
        // The address may not even parse as a jid with this shape; either way nothing is shown.
        assert!(
            matches!(read_of(&node, &own()), Read::Nothing | Read::Unreadable),
            "{from}"
        );
    }
}

#[test]
fn the_rest_of_a_calls_signalling_shows_nothing_and_is_still_read() {
    let kinds = [
        ("preaccept", None),
        // The caller's accept (only OUR device's says something: THE OWN ANSWER).
        ("accept", None),
        ("reject", Some("busy")),
        ("transport", None),
        ("relaylatency", None),
    ];
    for (tag, reason) in kinds {
        let mut builder = NodeBuilder::new(tag)
            .attr("call-id", CALL)
            .attr("call-creator", PEER_PN);
        if let Some(reason) = reason {
            builder = builder.attr("reason", reason);
        }
        let node = call_node(PEER_PN, false, builder.build());
        assert!(matches!(read_of(&node, &own()), Read::Nothing), "{tag}");
    }
    // The group fan-out names no group: it cannot be placed in one.
    let notice = NodeBuilder::new("offer_notice")
        .attr("call-id", CALL)
        .attr("call-creator", PEER_PN)
        .attr("media", "audio")
        .attr("type", "group")
        .build();
    assert!(matches!(
        read_of(&call_node(PEER_PN, false, notice), &own()),
        Read::Nothing
    ));
    // A mid-call video change, and an action this version has never heard of.
    let video = NodeBuilder::new("video")
        .attr("call-id", CALL)
        .attr("call-creator", PEER_PN)
        .attr("state", "3")
        .build();
    assert!(matches!(
        read_of(&call_node(PEER_PN, false, video), &own()),
        Read::Nothing
    ));
    let future = NodeBuilder::new("hologram").attr("call-id", CALL).build();
    assert!(matches!(
        read_of(&call_node(PEER_PN, false, future), &own()),
        Read::Nothing
    ));
}

#[test]
fn a_stanza_that_does_not_parse_is_unreadable_and_keeps_nothing_of_its_error() {
    // No `from`.
    let node = NodeBuilder::new("call")
        .attr("id", "STANZA-1")
        .attr("t", T.to_string())
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert!(matches!(read_of(&node, &own()), Read::Unreadable));
    // No `t`.
    let node = NodeBuilder::new("call")
        .attr("from", PEER_PN)
        .attr("id", "STANZA-1")
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert!(matches!(read_of(&node, &own()), Read::Unreadable));
    // An attribute that is a jid and is not one fails the whole stanza, and upstream's error for it prints
    // the text (`Invalid JID: <text>`): this is the case that must keep its error to itself.
    let node = NodeBuilder::new("call")
        .attr("from", PEER_PN)
        .attr("id", "STANZA-1")
        .attr("t", T.to_string())
        .attr("participant", BAD_JID)
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert!(matches!(read_of(&node, &own()), Read::Unreadable));
    // (An unknown attribute is no failure: the parser's `finish` looks at the errors it recorded only.)
    let node = NodeBuilder::new("call")
        .attr("from", PEER_PN)
        .attr("id", "STANZA-1")
        .attr("t", T.to_string())
        .attr("a_new_attribute", "x")
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert!(line_of(&node, &own()).is_some());
    // `Read` has nothing to print an error with.
    assert_eq!(format!("{:?}", Read::Unreadable), "Unreadable");
}

#[test]
fn a_hostile_call_id_gives_no_line() {
    for id in ["", &"x".repeat(300), "a\u{7}b"] {
        let action = NodeBuilder::new("offer")
            .attr("call-id", id)
            .attr("call-creator", PEER_PN)
            .build();
        let node = call_node(PEER_PN, false, action);
        assert!(
            matches!(read_of(&node, &own()), Read::Nothing | Read::Unreadable),
            "{id:?}"
        );
    }
}

/// Critic M10: an offer and its terminate are processed on tasks of their own, so the terminate can be
/// written first. The core merges by `CallLine::join`, so both orders end in the same line.
#[test]
fn a_terminate_before_its_offer_ends_in_the_same_line() {
    let offer_node = call_node(PEER_PN, false, offer(PEER_PN, true, None).build());
    let terminate_node = call_node(PEER_PN, false, terminate(PEER_PN, None, None));
    let a = line_of(&offer_node, &own()).expect("the offer");
    let b = line_of(&terminate_node, &own()).expect("the terminate");
    assert_eq!(a.conv, b.conv);
    assert_eq!(a.local, b.local);
    let (a, b) = (call_line(&a).clone(), call_line(&b).clone());
    let one = a.join(&b);
    let other = b.join(&a);
    assert_eq!(one, other);
    assert_eq!(one.state, CallState::Missed);
    assert_eq!(one.media, Some(CallMedia::Video));
    assert_eq!(one.by, Some(user("pn", "15550100101")));
}

#[test]
fn the_built_in_answer_is_out_of_reach_of_a_release_build() {
    assert_eq!(claim_for_build(false, None), Claim::Calls);
    assert_eq!(claim_for_build(false, Some("1")), Claim::Calls);
    assert_eq!(claim_for_build(true, None), Claim::Calls);
    assert_eq!(claim_for_build(true, Some("1")), Claim::LetBuiltInAnswer);
    // Exactly `1`.
    for other in ["0", "", "true", "yes", "11", " 1"] {
        assert_eq!(
            claim_for_build(true, Some(other)),
            Claim::Calls,
            "{other:?}"
        );
    }
}

/// Fix F10: `claim` takes the build kind from `cfg!(debug_assertions)` itself, so the switch works in a
/// debug test build and in no other. That `claim` hands the build kind to `claim_for_build` and nothing
/// else, and that its one caller passes the switch's text alone, are pinned by the source check in
/// `tests/builder.rs` (`the_call_claim_reads_the_build_kind_itself`).
#[test]
fn the_claim_of_this_build_follows_its_own_kind() {
    let with_the_switch = if cfg!(debug_assertions) {
        Claim::LetBuiltInAnswer
    } else {
        Claim::Calls
    };
    assert_eq!(claim(Some("1")), with_the_switch);
    assert_eq!(claim(Some("0")), Claim::Calls);
    assert_eq!(claim(None), Claim::Calls);
}

#[test]
fn upstreams_own_call_events_say_nothing_to_this_backend() {
    let node = call_node(PEER_PN, false, offer(PEER_PN, false, None).build());
    let owned = owned(&node);
    let parsed = parse_call_stanza(owned.get())
        .expect("parses")
        .expect("a known action");
    let event = Event::IncomingCall(Box::new(parsed));
    assert!(translate::translate(&event, NOW).is_none());
    assert!(translate::durable(&event, NOW, &own()).is_none());
    assert!(translate::unhooked(&event).is_empty());
}

// The interceptor over a real store.

struct World {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    interceptor: CallInterceptor,
    failures: Arc<AtomicUsize>,
}

fn world(claim: Claim) -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([9; 32]),
        "wa-calls",
    )
    .expect("open");
    let clock = FakeClock::new(NOW);
    let wake = Arc::new(Notify::new());
    let writer = EventWriter::new(owner.store(), Arc::new(clock.clone()), wake.clone());
    let stage2 = Arc::new(Stage2::new(
        owner.store(),
        Arc::new(clock.clone()),
        wake,
        Timing::default(),
    ));
    stage2.set_own(own());
    let failures = Arc::new(AtomicUsize::new(0));
    let counted = failures.clone();
    let interceptor = CallInterceptor::new(
        writer,
        Arc::new(clock),
        stage2,
        Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
        claim,
    );
    World {
        _dir: dir,
        owner,
        interceptor,
        failures,
    }
}

/// The events of `core_outbox`, in row order.
fn outbox_events(store: &KourierStore) -> Vec<BackendEvent> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(store.read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                // A stranger's call line states its chat's features (W5a, D7): `events::tests` reads them.
                out.extend(
                    decode_outbox_row(&row.envelope)
                        .expect("decodes")
                        .into_iter()
                        .filter(|e| !crate::features::is_statement(e)),
                );
            }
            Ok(out)
        }))
        .expect("read the outbox")
}

fn stanza(tag: &'static str) -> Node {
    NodeBuilder::new(tag)
        .attr("from", PEER_PN)
        .attr("id", "STANZA-1")
        .attr("t", T.to_string())
        .build()
}

#[test]
fn the_interceptor_claims_only_calls() {
    let w = world(Claim::Calls);
    for tag in [
        "message",
        "receipt",
        "notification",
        "iq",
        "presence",
        "ack",
    ] {
        assert_eq!(
            w.interceptor.intercept(&owned(&stanza(tag))),
            Interception::Pass,
            "<{tag}>"
        );
    }
    assert!(outbox_events(&w.owner.store()).is_empty());
    // A call is claimed whether or not it shows a line.
    let ring = call_node(PEER_PN, false, offer(PEER_PN, false, None).build());
    assert_eq!(
        w.interceptor.intercept(&owned(&ring)),
        Interception::Handled
    );
    let transport = NodeBuilder::new("transport")
        .attr("call-id", CALL)
        .attr("call-creator", PEER_PN)
        .build();
    let silent = call_node(PEER_PN, false, transport);
    assert_eq!(
        w.interceptor.intercept(&owned(&silent)),
        Interception::Handled
    );
    // A call that does not parse is claimed too, and writes nothing.
    let bad = NodeBuilder::new("call")
        .attr("id", "STANZA-1")
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert_eq!(w.interceptor.intercept(&owned(&bad)), Interception::Handled);
    // An empty call.
    assert_eq!(
        w.interceptor.intercept(&owned(&stanza("call"))),
        Interception::Handled
    );
    assert_eq!(w.failures.load(Ordering::SeqCst), 0);
    // One row, the ring's line, and nothing else.
    let events = outbox_events(&w.owner.store());
    assert_eq!(events.len(), 1, "{events:?}");
    let BackendEvent::Message(message) = &events[0] else {
        panic!("not a message: {events:?}");
    };
    assert_eq!(&*message.local, format!("call:{CALL}"));
    w.owner.close();
}

/// Writes a `lid_pn_mapping` row as the store's own `put_lid_mapping` does (a LID has one number).
fn learn(store: &KourierStore, lid: &str, phone: &str) {
    let (lid, phone) = (lid.to_owned(), phone.to_owned());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(store.write("learn a mapping", move |tx| {
            crate::store::exec(
                tx,
                "learn a mapping",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'test') \
                 ON CONFLICT(lid) DO UPDATE SET phone = excluded.phone",
                kourier_db::rusqlite::params![lid, phone],
            )
            .map(drop)
        }))
        .expect("learn");
}

/// Fix F1 over the real store: the interceptor reads `lid_pn_mapping` (on its own thread, through the
/// writer) only for a creator the stanza does not settle, and reads what `put_lid_mapping` wrote. The unit
/// table feeds `NumberOf` by hand; this is the SQL.
#[test]
fn the_interceptor_asks_the_store_which_number_a_creator_has() {
    let w = world(Claim::Calls);
    let store = w.owner.store();
    // A number sends an offer that names a LID as its creator.
    let from_number = call_node(PEER_PN, false, offer(PEER_LID, false, None).build());
    let ring = |node: &Node| w.interceptor.intercept(&owned(node));
    // Nothing is mapped: claimed, nothing shown.
    assert_eq!(ring(&from_number), Interception::Handled);
    assert!(outbox_events(&store).is_empty(), "an unproven creator");
    // The store knows that LID as another person's number: still nothing.
    learn(&store, "99900100101", "15550100102");
    assert_eq!(ring(&from_number), Interception::Handled);
    assert!(outbox_events(&store).is_empty(), "a LID of another number");
    // The store knows it as the sender's number: the line, in the LID's chat.
    learn(&store, "99900100101", "15550100101");
    assert_eq!(ring(&from_number), Interception::Handled);
    let events = outbox_events(&store);
    let [BackendEvent::Message(message)] = events.as_slice() else {
        panic!("one line: {events:?}")
    };
    assert_eq!(message.conv, ConvKey::Direct(user("lid", "99900100101")));
    // The other way round: a LID sends an offer that names a number; the store knows the sender.
    let from_lid = call_node(PEER_LID, false, offer(PEER_PN, true, None).build());
    assert_eq!(ring(&from_lid), Interception::Handled);
    let events = outbox_events(&store);
    let Some(BackendEvent::Message(message)) = events.last() else {
        panic!("{events:?}")
    };
    assert_eq!(events.len(), 2);
    // The same call (the helper's one id) in the person's other form: the store maps him, so the second signal goes to the
    // chat the first one was filed under (DUR-9), one call in one chat.
    assert_eq!(message.conv, ConvKey::Direct(user("lid", "99900100101")));
    // A stranger's number as the creator is never proven by that mapping.
    let stranger = call_node(
        PEER_LID,
        false,
        offer("15550100103@s.whatsapp.net", false, None).build(),
    );
    assert_eq!(ring(&stranger), Interception::Handled);
    assert_eq!(outbox_events(&store).len(), 2, "no third line");
    assert_eq!(w.failures.load(Ordering::SeqCst), 0);
    w.owner.close();
}

/// A `<call>` from `from` whose one action `tag` names call `id` by `creator`, with the action's `extra` attributes.
fn signal_of(
    from: &str,
    tag: &'static str,
    id: &str,
    creator: &str,
    extra: &[(&'static str, &str)],
) -> Node {
    let mut action = NodeBuilder::new(tag)
        .attr("call-id", id)
        .attr("call-creator", creator);
    for &(key, value) in extra {
        action = action.attr(key, value);
    }
    call_node(from, false, action.build())
}

/// THE OWN ANSWER over the real store: the interceptor's own line of the ring is filed (`call_lines`), and our phone's
/// accept is read against it: answered in the SAME chat under the same id, in either form of the caller (the store's
/// mapping); nothing for a call with no filed line, a group call's line, or a line in another person's chat; the
/// caller's dismiss writes nothing. Mutant: `filed_chat_of` takes the filed chat whoever it belongs to.
#[test]
fn the_interceptor_answers_the_filed_line_with_our_own_phones_accept() {
    let w = world(Claim::Calls);
    let store = w.owner.store();
    let send =
        |node: &Node| assert_eq!(w.interceptor.intercept(&owned(node)), Interception::Handled);
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    // The call lines written so far (a chat's statement beside one is left out).
    let lines = || -> Vec<IncomingMessage> {
        outbox_events(&store)
            .into_iter()
            .filter_map(|event| match event {
                BackendEvent::Message(message) if message.body.call_line().is_some() => {
                    Some(message)
                }
                _ => None,
            })
            .collect()
    };
    // The ring (from the peer's LID, the creator his LID), then our phone's accept, which names the creator by his
    // NUMBER: the store maps the two, so the accept goes to the LID chat the ring was filed in.
    learn(&store, "99900100101", "15550100101");
    send(&signal_of(
        PEER_LID,
        "offer",
        "CALL-A-71100011",
        PEER_LID,
        &[],
    ));
    assert_eq!(lines().len(), 1, "the ring");
    send(&signal_of(
        "99900100999@lid",
        "accept",
        "CALL-A-71100011",
        PEER_PN,
        &[],
    ));
    let all = lines();
    assert_eq!(all.len(), 2, "{all:?}");
    let answer = &all[1];
    assert_eq!(answer.conv, ConvKey::Direct(user("lid", "99900100101")));
    assert_eq!(&*answer.local, "call:CALL-A-71100011");
    assert_eq!(answer.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(call_line(answer).state, CallState::Answered);
    // The caller's dismiss after it writes nothing.
    send(&signal_of(
        PEER_LID,
        "terminate",
        "CALL-A-71100011",
        PEER_LID,
        &[("reason", "accepted_elsewhere")],
    ));
    assert_eq!(lines().len(), 2, "the caller's word writes nothing");

    // Nothing filed for this id: nothing.
    send(&signal_of(&mine, "accept", "CALL-B-71100012", PEER_PN, &[]));
    assert_eq!(lines().len(), 2, "no line seen, none made");
    // A group call's line: the accept names no group and makes no line of its own.
    send(&signal_of(
        PEER_PN,
        "offer",
        "CALL-C-71100013",
        PEER_PN,
        &[("group-jid", GROUP)],
    ));
    assert_eq!(lines().len(), 3, "the group's ring");
    send(&signal_of(&mine, "accept", "CALL-C-71100013", PEER_PN, &[]));
    assert_eq!(lines().len(), 3, "no answer for a group's line");
    // A line filed in another person's chat: the id is the caller's choice, the creator our accept names is someone else.
    send(&signal_of(
        "15550100102@s.whatsapp.net",
        "offer",
        "CALL-D-71100014",
        "15550100102@s.whatsapp.net",
        &[],
    ));
    assert_eq!(lines().len(), 4, "the other person's ring");
    send(&signal_of(&mine, "accept", "CALL-D-71100014", PEER_PN, &[]));
    assert_eq!(
        lines().len(),
        4,
        "no answer outside a filed 1:1 line of its creator"
    );
    assert_eq!(w.failures.load(Ordering::SeqCst), 0);
    w.owner.close();
}

/// The interceptor's two DEBUG lines say each signal's kind, reason and sender in fixed words (R1 could not tell which
/// kinds came), never an id, a number or the network's own text. Mutant: the sender's word `own` for every sender.
#[test]
fn the_log_says_each_signals_kind_reason_and_sender_in_fixed_words() {
    let mine = format!("{OWN_PN}@s.whatsapp.net");
    let words = |node: &Node| {
        let call = parse_call_stanza(owned(node).get())
            .expect("parses")
            .expect("a known action");
        Words::of(&call, &own())
    };
    let w =
        |kind: &'static str, reason: &'static str, sender: &'static str, connected: bool| Words {
            kind,
            reason,
            sender,
            connected,
        };
    let odd = "15550100777 a reason with a number";
    for (node, want) in [
        (
            call_node(PEER_PN, false, offer(PEER_PN, false, None).build()),
            w("offer", "-", "creator", false),
        ),
        (
            call_node(
                PEER_PN,
                false,
                terminate(PEER_PN, Some("accepted_elsewhere"), None),
            ),
            w("terminate", "accepted_elsewhere", "creator", false),
        ),
        (
            call_node(PEER_PN, false, terminate(PEER_PN, None, Some(20))),
            w("terminate", "none", "creator", true),
        ),
        (
            call_node(PEER_PN, false, terminate(PEER_PN, Some(odd), None)),
            w("terminate", "other", "creator", false),
        ),
        (
            call_node(&mine, false, accept(PEER_PN)),
            w("accept", "-", "own", false),
        ),
        (
            call_node("15550100102@s.whatsapp.net", false, accept(PEER_PN)),
            w("accept", "-", "other", false),
        ),
    ] {
        assert_eq!(words(&node), want);
    }

    // What the interceptor logs: the words, the count, nothing of the ids.
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let calls = world(Claim::Calls);
    for node in [
        call_node(PEER_PN, false, offer(PEER_PN, false, None).build()),
        call_node(&mine, false, accept(PEER_PN)),
        call_node(PEER_PN, false, terminate(PEER_PN, Some(odd), None)),
    ] {
        assert_eq!(
            calls.interceptor.intercept(&owned(&node)),
            Interception::Handled
        );
    }
    calls.owner.close();
    let said: Vec<String> = lines
        .all()
        .into_iter()
        .filter(|l| l.contains("a call line is durable") || l.contains("shows nothing"))
        .collect();
    assert_eq!(said.len(), 3, "{said:?}");
    assert!(said[0].contains("kind=\"offer\"") && said[0].contains("sender=\"creator\""));
    assert!(said[1].contains("kind=\"accept\"") && said[1].contains("sender=\"own\""));
    assert!(said[1].contains("lines=1"), "our phone's accept is a line");
    assert!(said[2].contains("reason=\"other\""));
    for line in &said {
        for secret in ["15550100", "99900100", CALL, "a reason with"] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

#[test]
fn a_claimed_call_is_durable_before_the_interceptor_returns() {
    let w = world(Claim::Calls);
    let node = call_node(PEER_PN, false, terminate(PEER_PN, None, None));
    assert_eq!(
        w.interceptor.intercept(&owned(&node)),
        Interception::Handled
    );
    // The call returned: the row is there, without a wait.
    assert_eq!(outbox_events(&w.owner.store()).len(), 1);
    w.owner.close();
}

#[test]
fn the_debug_switch_lets_upstream_answer_after_the_line_is_written() {
    let w = world(Claim::LetBuiltInAnswer);
    let node = call_node(PEER_PN, false, offer(PEER_PN, false, None).build());
    assert_eq!(w.interceptor.intercept(&owned(&node)), Interception::Pass);
    assert_eq!(
        outbox_events(&w.owner.store()).len(),
        1,
        "the line is written first"
    );
    w.owner.close();
}

/// T2 F7: the debug switch's start says, in one fixed line, that an OFFER was handed to upstream's handler (the run
/// tool counts them); another action does not, and a claim that keeps upstream out says nothing. Mutants: the line
/// dropped; the line said for every action; the line said under `Claim::Calls`.
#[test]
fn an_offer_given_to_upstreams_handler_says_so_in_one_fixed_line() {
    let words = "upstream's call handler is given a call offer (a debug test switch)";
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let offer_node = call_node(PEER_PN, false, offer(PEER_PN, false, None).build());
    let terminate_node = call_node(PEER_PN, false, terminate(PEER_PN, None, None));
    // The claim that keeps upstream out says nothing, for an offer or for any other action.
    let calls = world(Claim::Calls);
    assert_eq!(
        calls.interceptor.intercept(&owned(&offer_node)),
        Interception::Handled
    );
    calls.owner.close();
    assert!(lines.containing("given a call offer").is_empty());
    // The switch's start: an offer is handed over and said once, a terminate is handed over and not said.
    let builtin = world(Claim::LetBuiltInAnswer);
    assert_eq!(
        builtin.interceptor.intercept(&owned(&offer_node)),
        Interception::Pass
    );
    let said = lines.containing("given a call offer");
    assert_eq!(said.len(), 1, "{:?}", lines.all());
    assert_eq!(said[0], format!("message={words}"), "fixed words only");
    assert_eq!(
        builtin.interceptor.intercept(&owned(&terminate_node)),
        Interception::Pass
    );
    assert_eq!(lines.containing("given a call offer").len(), 1);
    builtin.owner.close();
}

#[test]
fn a_failed_write_ends_the_connection_and_claims_the_stanza_whatever_the_switch_says() {
    let _serial = fault::serial();
    for claim in [Claim::Calls, Claim::LetBuiltInAnswer] {
        let w = world(claim);
        let node = call_node(PEER_PN, false, offer(PEER_PN, false, None).build());
        fault::arm(fault::EVENT_WRITE, 1);
        // Claimed: upstream's handler must not answer a call whose line is not durable.
        assert_eq!(
            w.interceptor.intercept(&owned(&node)),
            Interception::Handled,
            "{claim:?}"
        );
        assert_eq!(w.failures.load(Ordering::SeqCst), 1, "{claim:?}");
        assert!(outbox_events(&w.owner.store()).is_empty(), "{claim:?}");
        // The server sends the stanza again: it is written then.
        fault::disarm();
        let _ = w.interceptor.intercept(&owned(&node));
        assert_eq!(outbox_events(&w.owner.store()).len(), 1, "{claim:?}");
        w.owner.close();
    }
    fault::disarm();
}

#[test]
fn a_call_that_shows_nothing_or_does_not_parse_does_not_end_the_connection() {
    let w = world(Claim::Calls);
    let accept = NodeBuilder::new("accept")
        .attr("call-id", CALL)
        .attr("call-creator", PEER_PN)
        .build();
    // Claimed, and the connection goes on.
    assert_eq!(
        w.interceptor
            .intercept(&owned(&call_node(PEER_PN, false, accept))),
        Interception::Handled
    );
    // An action and no `from`: the stanza does not parse.
    let unreadable = NodeBuilder::new("call")
        .attr("id", "STANZA-1")
        .attr("t", T.to_string())
        .children([offer(PEER_PN, false, None).build()])
        .build();
    assert!(matches!(read_of(&unreadable, &own()), Read::Unreadable));
    assert_eq!(
        w.interceptor.intercept(&owned(&unreadable)),
        Interception::Handled
    );
    // A call with no action at all.
    assert_eq!(
        w.interceptor.intercept(&owned(&stanza("call"))),
        Interception::Handled
    );
    assert_eq!(w.failures.load(Ordering::SeqCst), 0);
    assert!(outbox_events(&w.owner.store()).is_empty());
    w.owner.close();
}
