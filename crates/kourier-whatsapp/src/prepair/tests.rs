//! W2c C1: the pure claim. Numbers and ids are the fictitious 555-01xx range; nothing names a person.

use bytes::Bytes;
use kourier_model::Redacted;
use wacore::types::events::{Event, PairSuccess};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::node::ValueRef;
use wacore_binary::{Jid, Node, OwnedNodeRef, Server};

use super::*;
use crate::translate::{self, WaSignal};

const PN: &str = "15550100123";
const LID: &str = "99900100123";
const OTHER_PN: &str = "15550100456";
const OTHER_LID: &str = "99900100456";
const ID: &str = "reqid-1";

/// The node as the receive path hands it to an interceptor (marshalled and decoded again, so a
/// JID-typed value comes back as one).
fn owned(node: &Node) -> OwnedNodeRef {
    let packed = wacore_binary::marshal(node).expect("marshal");
    let bytes = wacore_binary::util::unpack(&packed)
        .expect("unpack")
        .into_owned();
    OwnedNodeRef::new(Bytes::from(bytes)).expect("a node")
}

fn read_of(node: &Node) -> Option<Claim> {
    read(owned(node).get())
}

fn device(pn: &str, lid: &str, number: u16) -> Node {
    NodeBuilder::new("device")
        .attr("jid", Jid::pn_device(pn, number))
        .attr("lid", Jid::lid_device(lid, number))
        .build()
}

/// A `pair-success` child naming the device `pn` / `lid` (device number 7). The HMAC and the
/// signature are not here: the claim refuses or passes before they are read.
fn success(pn: &str, lid: &str) -> Node {
    NodeBuilder::new("pair-success")
        .children([
            device(pn, lid, 7),
            NodeBuilder::new("platform").attr("name", "android").build(),
        ])
        .build()
}

fn server() -> Jid {
    Jid::new("", Server::Pn)
}

/// An `<iq>` from the server (a string value, as most of the wire's are) with the given type and
/// children.
fn iq(kind: Option<&'static str>, children: Vec<Node>) -> Node {
    let mut builder = NodeBuilder::new("iq")
        .attr("from", "s.whatsapp.net")
        .attr("id", ID);
    if let Some(kind) = kind {
        builder = builder.attr("type", kind);
    }
    builder.children(children).build()
}

fn paired(pn: &str, lid: &str) -> Paired {
    Paired {
        pn: Redacted::new(pn.to_owned()),
        lid: Redacted::new(lid.to_owned()),
        device: 7,
    }
}

/// Upstream's gate, row by row (R1 of the brief): what the claim takes and what it leaves to
/// upstream. The brief's mutants: require `type=set`, skip the `from` check, compare `from` as a
/// string only, require exactly one child, read only the last `pair-success`.
#[test]
fn the_claim_takes_what_upstream_would_pair_on_and_nothing_else() {
    let pair_device = NodeBuilder::new("pair-device")
        .children([NodeBuilder::new("ref").string_content("r1").build()])
        .build();
    let other_child = NodeBuilder::new("something").build();

    // Claimed, with the ids of every pair-success child in order.
    let claimed: Vec<(&str, Node, Vec<Option<Paired>>)> = vec![
        (
            "type=set",
            iq(Some("set"), vec![success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "no type",
            iq(None, vec![success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "type=get",
            iq(Some("get"), vec![success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "type=result",
            iq(Some("result"), vec![success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "pair-success as the second child",
            iq(Some("set"), vec![other_child.clone(), success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "pair-device first (fail closed: refused or passed on the pair-success)",
            iq(Some("set"), vec![pair_device.clone(), success(PN, LID)]),
            vec![Some(paired(PN, LID))],
        ),
        (
            "two pair-success children, both read in order (F3)",
            iq(
                Some("set"),
                vec![success(OTHER_PN, OTHER_LID), success(PN, LID)],
            ),
            vec![Some(paired(OTHER_PN, OTHER_LID)), Some(paired(PN, LID))],
        ),
        (
            "a pair-success without a device names nobody",
            iq(Some("set"), vec![NodeBuilder::new("pair-success").build()]),
            vec![None],
        ),
    ];
    for (name, node, pairs) in claimed {
        let claim = read_of(&node).unwrap_or_else(|| panic!("{name}: not claimed"));
        assert_eq!(claim.pairs, pairs, "{name}");
        assert_eq!(
            claim.id.as_ref().map(Redacted::expose).map(String::as_str),
            Some(ID),
            "{name}"
        );
    }

    // Left to upstream.
    let left: Vec<(&str, Node)> = vec![
        (
            "another sender",
            NodeBuilder::new("iq")
                .attr("from", "g.us")
                .attr("id", ID)
                .children([success(PN, LID)])
                .build(),
        ),
        (
            "a user as sender",
            NodeBuilder::new("iq")
                .attr("from", Jid::pn("15550100999"))
                .attr("id", ID)
                .children([success(PN, LID)])
                .build(),
        ),
        (
            "no sender",
            NodeBuilder::new("iq")
                .attr("id", ID)
                .children([success(PN, LID)])
                .build(),
        ),
        ("a pair-device alone", iq(Some("set"), vec![pair_device])),
        (
            "a ping",
            iq(Some("get"), vec![NodeBuilder::new("ping").build()]),
        ),
        ("an iq with no child", iq(Some("set"), vec![])),
        (
            "a call",
            NodeBuilder::new("call")
                .attr("from", "s.whatsapp.net")
                .attr("id", ID)
                .children([success(PN, LID)])
                .build(),
        ),
        (
            "a message from the server",
            NodeBuilder::new("message")
                .attr("from", "s.whatsapp.net")
                .attr("id", ID)
                .children([success(PN, LID)])
                .build(),
        ),
    ];
    for (name, node) in left {
        assert_eq!(read_of(&node), None, "{name}");
    }
}

/// F9: upstream compares `from` with `!=` against the server's name, which matches a string AND a
/// JID value that prints as it. A claim that looks at strings only would miss a JID-typed `from`.
#[test]
fn a_sender_written_as_a_jid_is_the_server_too() {
    let node = NodeBuilder::new("iq")
        .attr("from", server())
        .attr("id", ID)
        .attr("type", "set")
        .children([success(PN, LID)])
        .build();
    let packed = owned(&node);
    assert!(
        matches!(packed.get().get_attr("from"), Some(ValueRef::Jid(_))),
        "the test must hand the claim a JID-typed value"
    );
    let claim = read(packed.get()).expect("claimed");
    assert_eq!(claim.pairs, vec![Some(paired(PN, LID))]);
}

/// A stanza without an `id` is still claimed (upstream would send nothing either); the answer has
/// nothing to echo.
#[test]
fn a_pairing_without_an_id_is_claimed_without_one() {
    let node = NodeBuilder::new("iq")
        .attr("from", "s.whatsapp.net")
        .children([success(PN, LID)])
        .build();
    let claim = read_of(&node).expect("claimed");
    assert!(claim.id.is_none());
    assert_eq!(claim.pairs.len(), 1);
}

/// F9: a device with a `jid` and no `lid` reads as the empty LID (upstream's `unwrap_or_default`),
/// which is what the rules refuse as unreadable.
#[test]
fn a_device_without_a_lid_has_the_empty_lid() {
    let device = NodeBuilder::new("device")
        .attr("jid", Jid::pn_device(PN, 7))
        .build();
    let node = iq(
        Some("set"),
        vec![NodeBuilder::new("pair-success").children([device]).build()],
    );
    let claim = read_of(&node).expect("claimed");
    assert_eq!(claim.pairs, vec![Some(paired(PN, ""))]);
}

/// T-U2: the answer is upstream's own pair error, byte for byte what its marshal makes of the
/// frame whatsmeow sends: `to`, `type`, `id`, and one `error` child with the code and the text.
/// Mutants: another text; another code.
#[test]
fn the_answer_is_upstreams_pair_error() {
    let expected = NodeBuilder::new("iq")
        .attrs([
            ("to", "s.whatsapp.net".to_owned()),
            ("type", "error".to_owned()),
            ("id", ID.to_owned()),
        ])
        .children([NodeBuilder::new("error")
            .attrs([
                ("code", "500".to_owned()),
                ("text", "internal-error".to_owned()),
            ])
            .build()])
        .build();
    let made = answer(ID);
    assert_eq!(made, expected);
    assert_eq!(
        wacore_binary::marshal(&made).expect("marshal"),
        wacore_binary::marshal(&expected).expect("marshal")
    );
    // Nothing of either account is in it.
    let printed = format!("{made:?}");
    for secret in [PN, LID, OTHER_PN, OTHER_LID] {
        assert!(!printed.contains(secret));
    }
}

/// T-U3 (R9): the claim and the event handler read the SAME ids from the same JIDs, so the
/// first line and the second line of the checks can never disagree about who was paired. The
/// event is what upstream dispatches for a pair-success with these `<device>` values.
#[test]
fn the_claim_reads_the_ids_the_event_carries() {
    let event_of = |pn: Jid, lid: Jid| {
        translate::translate(
            &Event::PairSuccess(
                PairSuccess::builder()
                    .id(pn)
                    .lid(lid)
                    .business_name(String::new())
                    .platform("android".to_owned())
                    .build(),
            ),
            0,
        )
        .expect("a signal")
    };
    // Whole, a number-device that differs from the LID's, and a device with no LID.
    let cases: Vec<(&str, Node, WaSignal)> = vec![
        (
            "whole",
            iq(Some("set"), vec![success(PN, LID)]),
            event_of(Jid::pn_device(PN, 7), Jid::lid_device(LID, 7)),
        ),
        (
            "another pair",
            iq(Some("set"), vec![success(OTHER_PN, OTHER_LID)]),
            event_of(Jid::pn_device(OTHER_PN, 7), Jid::lid_device(OTHER_LID, 7)),
        ),
        (
            "no lid",
            iq(
                Some("set"),
                vec![
                    NodeBuilder::new("pair-success")
                        .children([NodeBuilder::new("device")
                            .attr("jid", Jid::pn_device(PN, 7))
                            .build()])
                        .build(),
                ],
            ),
            event_of(Jid::pn_device(PN, 7), Jid::default()),
        ),
    ];
    for (name, node, signal) in cases {
        let WaSignal::PairSuccess(from_event) = signal else {
            panic!("{name}: not a pairing");
        };
        let claim = read_of(&node).unwrap_or_else(|| panic!("{name}: not claimed"));
        assert_eq!(claim.pairs, vec![Some(from_event)], "{name}");
    }
}

// ----- the interceptor (W2c C3a) -----

mod interceptor {
    use std::fmt::Debug;
    use std::sync::{Arc, Mutex};

    use kourier_backend_api::TakenIdentity;
    use tokio::sync::mpsc;
    use tracing::field::{Field, Visit};
    use tracing::{Event as TracingEvent, Metadata, Subscriber, span};
    use whatsapp_rust::{Interception, StanzaInterceptor};

    use super::*;
    use crate::link::{Gate, Rules};
    use crate::tap::Tap;

    type Done = Box<dyn FnOnce(bool) + Send>;

    /// What the dispatcher was asked for: the reply and the `done` to run (the test decides whether
    /// the iq "left").
    #[derive(Default)]
    struct Asked(Mutex<Vec<(Option<Node>, Done)>>);

    impl Asked {
        fn dispatch(self: &Arc<Self>) -> Dispatch {
            let asked = self.clone();
            Arc::new(move |reply, done| asked.0.lock().expect("lock").push((reply, done)))
        }

        fn count(&self) -> usize {
            self.0.lock().expect("lock").len()
        }

        fn take(&self) -> (Option<Node>, Done) {
            self.0.lock().expect("lock").remove(0)
        }
    }

    struct World {
        pre: PrePair,
        asked: Arc<Asked>,
        signals: mpsc::UnboundedReceiver<WaSignal>,
        gate: Option<Arc<Gate>>,
    }

    fn world(rules: Option<Rules>) -> World {
        let gate = rules.map(|rules| Arc::new(Gate::new(rules)));
        let (tap, signals) = Tap::channel(gate.clone());
        let asked = Arc::new(Asked::default());
        World {
            pre: PrePair::new(tap, asked.dispatch()),
            asked,
            signals,
            gate,
        }
    }

    fn tail(digits: &str) -> Rules {
        Rules::new(Vec::new(), None, Some(Redacted::new(digits.to_owned())))
    }

    fn pairing() -> OwnedNodeRef {
        owned(&iq(Some("set"), vec![success(PN, LID)]))
    }

    /// A refused pairing is claimed, the error iq is handed to the dispatcher FIRST, and the login hears of
    /// it only when the dispatcher says it is done. Mutants: `Pass` instead of `Handled`; the reply skipped;
    /// the login told before the reply left.
    #[test]
    fn a_refused_pairing_is_claimed_answered_and_then_told_to_the_login() {
        let mut w = world(Some(tail("9999")));
        assert_eq!(w.pre.intercept(&pairing()), Interception::Handled);
        assert_eq!(w.asked.count(), 1);
        assert!(
            w.signals.try_recv().is_err(),
            "the login was told before the answer left"
        );
        assert!(w.gate.as_ref().is_some_and(|gate| gate.is_refused()));
        let (reply, done) = w.asked.take();
        assert_eq!(reply, Some(answer(ID)));
        done(true);
        assert_eq!(
            w.signals.try_recv().expect("told"),
            WaSignal::PairClaimed(Some(paired(PN, LID)))
        );
        assert!(w.signals.try_recv().is_err());
    }

    /// An answer that could not leave (no client, no socket, a timeout) still ends the login.
    #[test]
    fn a_refusal_whose_answer_could_not_leave_still_ends_the_login() {
        let mut w = world(Some(tail("9999")));
        assert_eq!(w.pre.intercept(&pairing()), Interception::Handled);
        let (_, done) = w.asked.take();
        done(false);
        assert_eq!(
            w.signals.try_recv().expect("told"),
            WaSignal::PairClaimed(Some(paired(PN, LID)))
        );
    }

    /// A pairing the rules accept is upstream's: nothing is sent, nothing is told, the flag stays down.
    /// (T-W3 proves over the wire that upstream then completes it.) Mutant: claim every pairing.
    #[test]
    fn a_pairing_the_rules_accept_is_left_to_upstream() {
        let mut w = world(Some(tail("0123")));
        assert_eq!(w.pre.intercept(&pairing()), Interception::Pass);
        assert_eq!(w.asked.count(), 0);
        assert!(w.signals.try_recv().is_err());
        assert!(w.gate.as_ref().is_some_and(|gate| !gate.is_refused()));
    }

    /// What is no pairing is none of this interceptor's business, whoever the sender claims to be.
    #[test]
    fn what_is_no_pairing_is_left_alone() {
        let mut w = world(Some(tail("9999")));
        let ping = iq(Some("get"), vec![NodeBuilder::new("ping").build()]);
        let call = NodeBuilder::new("call")
            .attr("from", "s.whatsapp.net")
            .build();
        for node in [ping, call] {
            assert_eq!(w.pre.intercept(&owned(&node)), Interception::Pass);
        }
        assert_eq!(w.asked.count(), 0);
        assert!(w.signals.try_recv().is_err());
    }

    /// A pairing with no `id` has nothing to be answered to (upstream would send nothing either); it is
    /// still claimed and the login still hears of it.
    #[test]
    fn a_pairing_without_an_id_is_claimed_and_nothing_is_sent() {
        let mut w = world(Some(tail("9999")));
        let node = NodeBuilder::new("iq")
            .attr("from", "s.whatsapp.net")
            .children([success(PN, LID)])
            .build();
        assert_eq!(w.pre.intercept(&owned(&node)), Interception::Handled);
        let (reply, done) = w.asked.take();
        assert_eq!(reply, None);
        done(false);
        assert!(matches!(
            w.signals.try_recv(),
            Ok(WaSignal::PairClaimed(Some(_)))
        ));
    }

    /// L-a: a pairing that meets a client with no login is answered and refused; nobody is told (no login
    /// waits). The device is never made.
    #[test]
    fn a_pairing_on_a_client_with_no_login_is_answered_and_nobody_is_told() {
        let mut w = world(None);
        assert_eq!(w.pre.intercept(&pairing()), Interception::Handled);
        let (reply, done) = w.asked.take();
        assert_eq!(reply, Some(answer(ID)));
        done(true);
        assert!(w.signals.try_recv().is_err());
    }

    /// F1: a second phone after the first pairing was let through is answered, but the login is not told
    /// and the gate stays up. Mutant: the second pairing is told to the login.
    #[test]
    fn a_second_pairing_after_the_first_was_let_through_is_answered_and_nobody_is_told() {
        let mut w = world(Some(Rules::new(Vec::new(), None, None)));
        assert_eq!(w.pre.intercept(&pairing()), Interception::Pass);
        let second = owned(&iq(Some("set"), vec![success(OTHER_PN, OTHER_LID)]));
        assert_eq!(w.pre.intercept(&second), Interception::Handled);
        let (reply, done) = w.asked.take();
        assert_eq!(reply, Some(answer(ID)));
        done(true);
        assert!(w.signals.try_recv().is_err());
        assert!(w.gate.as_ref().is_some_and(|gate| !gate.is_refused()));
        // The same phone again (a redelivery) is upstream's.
        assert_eq!(w.pre.intercept(&pairing()), Interception::Pass);
    }

    /// The refusal's one log line: a fixed word and whether the answer left. Never the id, an id of either
    /// account, digits or the tail. Mutant: the line carries the number.
    #[test]
    fn the_refusal_logs_a_fixed_word_and_no_id() {
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<String>>>);
        impl Subscriber for Lines {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
                span::Id::from_u64(1)
            }
            fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
            fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
            fn event(&self, event: &TracingEvent<'_>) {
                struct All(Vec<String>);
                impl Visit for All {
                    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                        self.0.push(format!("{}={value:?}", field.name()));
                    }
                }
                let mut all = All(Vec::new());
                event.record(&mut all);
                self.0.lock().expect("lock").push(all.0.join(" "));
            }
            fn enter(&self, _: &span::Id) {}
            fn exit(&self, _: &span::Id) {}
        }

        let w = world(Some(Rules::new(
            vec![TakenIdentity {
                user: kourier_model::UserId::from_parts("lid", LID).unwrap(),
                logged_out: false,
            }],
            None,
            Some(Redacted::new("90210417".to_owned())),
        )));
        let lines = Lines::default();
        tracing::subscriber::with_default(lines.clone(), || {
            assert_eq!(w.pre.intercept(&pairing()), Interception::Handled);
            let (_, done) = w.asked.take();
            done(true);
        });
        let lines = lines.0.lock().expect("lock").clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("refusal=\"taken\""), "{lines:?}");
        assert!(lines[0].contains("answered=true"), "{lines:?}");
        for secret in [PN, LID, OTHER_PN, OTHER_LID, ID, "90210417"] {
            assert!(!lines[0].contains(secret), "{secret} in {lines:?}");
        }
    }
}
