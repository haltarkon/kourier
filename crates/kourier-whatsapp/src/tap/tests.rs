//! W2c C2: the first line's verdict on the tap and the one signal that gets past a refused gate.
//! Numbers and ids are the fictitious 555-01xx range.

use kourier_backend_api::TakenIdentity;
use kourier_model::{Redacted, UserId};

use super::*;
use crate::link::{Rules, Tell};

const PN: &str = "15550100123";
const LID: &str = "99900100123";
const OTHER_PN: &str = "15550100456";
const OTHER_LID: &str = "99900100456";

fn paired(pn: &str, lid: &str) -> Paired {
    Paired {
        pn: Redacted::new(pn.to_owned()),
        lid: Redacted::new(lid.to_owned()),
        device: 7,
    }
}

fn tap_with(rules: Rules) -> (Tap, mpsc::UnboundedReceiver<WaSignal>, Arc<Gate>) {
    let gate = Arc::new(Gate::new(rules));
    let (tap, signals) = Tap::channel(Some(gate.clone()));
    (tap, signals, gate)
}

fn tail(digits: &str) -> Rules {
    Rules::new(Vec::new(), None, Some(Redacted::new(digits.to_owned())))
}

/// T-U4: a refusal sets the flag, a pass leaves it; the claimed signal gets past the flag and every
/// other signal does not. Mutants: the flag not set on a refusal; `deliver_claimed` replaced by
/// `deliver` (the refused flag drops the signal).
#[test]
fn a_refusal_sets_the_flag_a_pass_leaves_it_and_the_claimed_signal_gets_past_it() {
    // A pass: the tail matches.
    let (tap, _signals, gate) = tap_with(tail("0123"));
    assert_eq!(tap.judge_pairing(&[Some(paired(PN, LID))]), Verdict::Pass);
    assert!(!gate.is_refused());

    // A refusal: another account's number.
    let (tap, mut signals, gate) = tap_with(tail("9999"));
    let stranger = paired(PN, LID);
    let Verdict::Refuse(refused) = tap.judge_pairing(&[Some(stranger.clone())]) else {
        panic!("the stranger was not refused");
    };
    assert_eq!(refused.token, "expected-number");
    assert_eq!(refused.tell, Tell::Login(Some(stranger.clone())));
    assert!(gate.is_refused(), "the refusal did not set the flag");
    // Every ordinary signal is dropped from now on...
    assert_eq!(tap.deliver(WaSignal::Connected), Delivered::Dropped);
    assert_eq!(
        tap.deliver(WaSignal::PairSuccess(stranger.clone())),
        Delivered::Dropped
    );
    assert!(signals.try_recv().is_err());
    // ...except the claim's own, which ends the login.
    assert_eq!(tap.deliver_claimed(Some(stranger.clone())), Delivered::Sent);
    assert_eq!(
        signals.try_recv().expect("the claimed signal"),
        WaSignal::PairClaimed(Some(stranger))
    );
    assert!(signals.try_recv().is_err());
}

/// T-U6 (L-a), the client with no login: nothing may pair on a client that only connects, logs a
/// device out or was rebuilt, and nobody is told. Mutant: no gate -> pass.
#[test]
fn a_client_with_no_login_refuses_every_pairing_and_tells_nobody() {
    let (tap, _signals) = Tap::channel(None);
    assert_eq!(
        tap.judge_pairing(&[Some(paired(PN, LID))]),
        Verdict::Refuse(Refused::no_login())
    );
    assert_eq!(Refused::no_login().tell, Tell::Nobody);
    assert_eq!(Refused::no_login().token, "no-login");
}

/// T-U6 (L-a), the login that ended without a device: the gate is closed, so a `pair-success` that
/// lands while the client is still shutting down is refused. Mutant: `close` does nothing.
#[test]
fn a_closed_gate_refuses_every_pairing() {
    let (tap, _signals, gate) = tap_with(Rules::new(Vec::new(), None, None));
    gate.close();
    assert_eq!(
        tap.judge_pairing(&[Some(paired(PN, LID))]),
        Verdict::Refuse(Refused::no_login())
    );
}

/// A gate that is no longer open does not make the signal path any wider: a closed gate has not set
/// the flag, so the other signals still pass (the login machine ended and the actor drops them).
#[test]
fn closing_the_gate_does_not_set_the_flag() {
    let (tap, _signals, gate) = tap_with(Rules::new(Vec::new(), None, None));
    gate.close();
    assert!(!gate.is_refused());
    assert_eq!(tap.deliver(WaSignal::Connected), Delivered::Sent);
}

/// F1: after a pairing was let through, another identity is answered and goes no further: it does NOT
/// set the flag (the account would go deaf, a ban or a removal on the phone unseen) and nobody is told
/// (the login would fail while the first pairing completes). The same identity passes again.
/// Mutants: the flag is set after an accept; a second identity passes.
#[test]
fn after_a_pairing_stood_another_identity_is_refused_without_the_flag() {
    let (tap, mut signals, gate) = tap_with(Rules::new(
        vec![TakenIdentity {
            user: UserId::from_parts("lid", "99900100777").unwrap(),
            logged_out: false,
        }],
        None,
        None,
    ));
    let first = paired(PN, LID);
    assert_eq!(tap.judge_pairing(&[Some(first.clone())]), Verdict::Pass);
    // A redelivery of the same pairing (the device number is the server's, per pairing).
    let mut again = first.clone();
    again.device = 9;
    assert_eq!(tap.judge_pairing(&[Some(again)]), Verdict::Pass);
    // Another account, one the rules would accept as well, and one they would refuse.
    for other in [
        paired(OTHER_PN, OTHER_LID),
        paired("15550100777", "99900100777"),
    ] {
        assert_eq!(
            tap.judge_pairing(&[Some(other)]),
            Verdict::Refuse(Refused {
                token: "second-pairing",
                tell: Tell::Nobody
            })
        );
    }
    // The same number with another LID is another identity too, and so is a pairing with two children.
    assert!(matches!(
        tap.judge_pairing(&[Some(paired(PN, OTHER_LID))]),
        Verdict::Refuse(_)
    ));
    assert!(matches!(
        tap.judge_pairing(&[Some(first.clone()), Some(first)]),
        Verdict::Refuse(_)
    ));
    assert!(!gate.is_refused(), "a second pairing set the flag");
    // The account hears its connection: the signals are delivered.
    assert_eq!(tap.deliver(WaSignal::Connected), Delivered::Sent);
    assert_eq!(signals.try_recv().expect("delivered"), WaSignal::Connected);
}

/// The second line's accept is remembered as well (a pin bump moved the claim, or the claim passed
/// it): the first line then knows which pairing stands. Mutant: `decide` forgets an accept.
#[test]
fn an_accept_of_the_second_line_is_the_pairing_that_stands() {
    let (tap, _signals, gate) = tap_with(Rules::new(Vec::new(), None, None));
    assert!(matches!(
        gate.decide(&paired(PN, LID)),
        crate::link::Decision::Accept(_)
    ));
    // Another identity is refused first: it is the second line's accept that makes the first stand.
    assert!(matches!(
        tap.judge_pairing(&[Some(paired(OTHER_PN, OTHER_LID))]),
        Verdict::Refuse(Refused {
            token: "second-pairing",
            ..
        })
    ));
    assert_eq!(tap.judge_pairing(&[Some(paired(PN, LID))]), Verdict::Pass);
    assert!(!gate.is_refused());
}

/// W2c M2 (review), the claim's own window: a stranger was refused (the flag is set, the login is not yet
/// failed, so the gate is not yet closed), and an identity the rules ACCEPT arrives before any close. It is
/// refused (`no-login`, nobody told) and the gate keeps no accepted pairing: otherwise upstream would sign
/// it while the login is about to fail. Mutant: `Gate::claim` without `|| self.is_refused()` (link.rs).
#[test]
fn after_a_stranger_was_refused_an_accepted_identity_is_refused_too_before_any_close() {
    let rules = || {
        Rules::new(
            vec![TakenIdentity {
                user: UserId::from_parts("lid", "99900100777").unwrap(),
                logged_out: false,
            }],
            None,
            None,
        )
    };
    let stranger = paired("15550100777", "99900100777");
    let (tap, mut signals, gate) = tap_with(rules());
    let Verdict::Refuse(refused) = tap.judge_pairing(&[Some(stranger.clone())]) else {
        panic!("the stranger was not refused");
    };
    assert_eq!(refused.token, "taken");
    assert!(gate.is_refused(), "the refusal did not set the flag");
    // Nobody closed the gate (the login is still waiting for its failure): only the flag stands.
    assert_eq!(
        tap.judge_pairing(&[Some(paired(PN, LID))]),
        Verdict::Refuse(Refused::no_login())
    );
    assert_eq!(Refused::no_login().tell, Tell::Nobody);
    assert!(!gate.has_accepted(), "a pairing passed after the refusal");
    assert!(signals.try_recv().is_err(), "a signal was delivered");

    // The second line's refusal (`decide` on the finished pairing) sets the same flag: the same.
    let (tap, mut signals, gate) = tap_with(rules());
    assert!(matches!(
        gate.decide(&stranger),
        crate::link::Decision::Refuse(_)
    ));
    assert!(gate.is_refused());
    assert_eq!(
        tap.judge_pairing(&[Some(paired(PN, LID))]),
        Verdict::Refuse(Refused::no_login())
    );
    assert!(!gate.has_accepted(), "a pairing passed after the refusal");
    assert!(signals.try_recv().is_err(), "a signal was delivered");
}
