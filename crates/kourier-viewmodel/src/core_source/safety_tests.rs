//! `CoreSource`: the safety-number level of the details panel (P5a), against the simulated core
//! of [`super::tests`]: one query in flight, an equal answer hands nothing over, a contact
//! change of the level's account asks again, a refused click asks again with its line, and
//! closing ends it.

use kourier_core::facade::{ActionSubject, CoreError, Query, QueryData, QueryToken};
use kourier_model::{
    AccountId, IdentityFingerprint, Redacted, SafetyAnswer, SafetyNumber, SafetyUnavailable,
    VerifyState,
};

use super::tests::{Sent, Store, result, settle, source, take_queries, user};
use super::*;

fn opened(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, store);
    (src, sent)
}

fn number(key: u8, state: VerifyState) -> SafetyAnswer {
    Ok(SafetyNumber {
        digits: Redacted::new("1".repeat(60)),
        scannable: Redacted::new(vec![8, 2, key]),
        key: IdentityFingerprint(vec![key; 33]),
        state,
    })
}

/// The safety queries sent so far (others are taken out and ignored).
fn safety_queries(sent: &Sent) -> Vec<QueryToken> {
    take_queries(sent)
        .into_iter()
        .filter(|(_, q)| matches!(q, Query::SafetyNumber { .. }))
        .map(|(t, _)| t)
        .collect()
}

fn answer(src: &mut CoreSource, token: QueryToken, a: SafetyAnswer) -> Changes {
    answer_waits(src, token, a, false)
}

fn answer_waits(src: &mut CoreSource, token: QueryToken, a: SafetyAnswer, waits: bool) -> Changes {
    src.on_event(&result(token, QueryData::SafetyNumber { answer: a, waits }))
}

/// The first answer is handed over with the key it names; the same answer again hands
/// nothing over; another state does.
#[test]
fn equal_answer_hands_nothing() {
    let who = user(7);
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    assert!(src.request_safety(AccountId(1), who.clone()));
    let t = safety_queries(&sent);
    assert_eq!(t.len(), 1, "one ask");
    assert!(answer(&mut src, t[0], number(1, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the level");
    assert!(v.key == Some(IdentityFingerprint(vec![1; 33])));
    assert_eq!(v.groups.len(), 12);
    assert!(src.take_safety().is_none(), "taken once");
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    let t = safety_queries(&sent);
    assert_eq!(t.len(), 1);
    assert!(!answer(&mut src, t[0], number(1, VerifyState::Default)).safety);
    assert!(src.take_safety().is_none(), "the same answer: nothing new");
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(1, VerifyState::Verified)).safety);
    assert_eq!(
        src.take_safety().expect("the level").state,
        Some(VerifyState::Verified)
    );
}

/// Critic F16: 50 contact changes of the level's account while one ask is in flight make at
/// most ONE more ask (two in all); a change of ANOTHER account asks nothing.
#[test]
fn contacts_changed_reasks() {
    let who = user(7);
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    assert!(src.request_safety(AccountId(1), who));
    let first = safety_queries(&sent);
    for _ in 0..50 {
        src.on_event(&UiEvent::ContactsChanged {
            account: AccountId(1),
        });
    }
    assert!(safety_queries(&sent).is_empty(), "one ask in flight");
    answer(&mut src, first[0], number(1, VerifyState::Default));
    let after = safety_queries(&sent);
    assert_eq!(after.len(), 1, "50 changes: one more ask, 2 in all");
    answer(&mut src, after[0], number(1, VerifyState::Default));
    assert!(safety_queries(&sent).is_empty(), "nothing more");
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(2),
    });
    assert!(
        safety_queries(&sent).is_empty(),
        "another account asks nothing"
    );
}

/// A `SetVerified` / `ConfirmIdentity` failure for `user`, as the core reports it.
fn failed(action: &'static str, error: CoreError, user: UserId) -> UiEvent {
    UiEvent::ActionFailed {
        account: Some(AccountId(1)),
        action,
        error: std::sync::Arc::new(error),
        subject: ActionSubject::Person(user),
    }
}

/// The backend's refusal of a stale key (`InvalidInput`), as `runtime.rs` hands it over.
fn stale_key() -> CoreError {
    CoreError::Backend(Box::new(kourier_model::BackendError::new(
        kourier_model::ErrorKind::InvalidInput,
        "stale",
    )))
}

/// A level on screen for user 7 with its first answer taken.
fn shown(store: &Store) -> (CoreSource, Sent, UserId) {
    let who = user(7);
    let (mut src, sent) = opened(store);
    assert!(src.request_safety(AccountId(1), who.clone()));
    let t = safety_queries(&sent);
    answer(&mut src, t[0], number(1, VerifyState::Default));
    src.take_safety();
    (src, sent, who)
}

/// A Mark the backend refused (the stale key: `InvalidInput`) for the shown person asks again
/// and the next answer carries the refusal's line (even when the number is the same); a
/// refusal for another person does nothing (U1: the wording of a changed number is for this
/// refusal only).
#[test]
fn a_refused_mark_reasks_with_its_line() {
    let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
    src.on_event(&failed("SetVerified", stale_key(), user(8)));
    assert!(safety_queries(&sent).is_empty(), "another person's refusal");
    src.on_event(&failed("SetVerified", stale_key(), who));
    let t = safety_queries(&sent);
    assert_eq!(t.len(), 1, "asked again");
    assert!(answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the new number");
    assert!(v.key == Some(IdentityFingerprint(vec![2; 33])));
    assert_eq!(
        v.notice,
        crate::safety::refused_text(crate::safety::SafetyAction::Mark)
    );
    assert!(v.notice.contains("before you mark it"), "{}", v.notice);
}

/// U1: a refused CONFIRM has its own line: it says "confirm", never "mark".
#[test]
fn a_refused_confirm_says_confirm() {
    let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
    src.on_event(&failed("ConfirmIdentity", stale_key(), who));
    let t = safety_queries(&sent);
    assert_eq!(t.len(), 1, "asked again");
    assert!(answer(&mut src, t[0], number(2, VerifyState::Changed)).safety);
    let v = src.take_safety().expect("the new number");
    assert!(v.notice.contains("before you confirm it"), "{}", v.notice);
    assert!(!v.notice.contains("mark"), "{}", v.notice);
}

/// U1: any other failure of a Mark, Clear or Confirm (the account is not running, the store
/// failed) is worded neutrally: it never says the number changed. The level still asks again.
#[test]
fn any_other_failure_says_the_change_was_not_saved() {
    let not_saved = crate::safety::not_saved_text();
    assert!(!not_saved.contains("changed"), "{not_saved}");
    for (action, error) in [
        (
            "SetVerified",
            CoreError::InvalidState("the account is not running"),
        ),
        (
            "ConfirmIdentity",
            CoreError::InvalidState("the account is not running"),
        ),
        (
            "SetVerified",
            CoreError::Backend(Box::new(kourier_model::BackendError::transient("store"))),
        ),
        (
            "ConfirmIdentity",
            CoreError::Backend(Box::new(kourier_model::BackendError::new(
                kourier_model::ErrorKind::Internal,
                "store",
            ))),
        ),
    ] {
        let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
        src.on_event(&failed(action, error, who));
        let t = safety_queries(&sent);
        assert_eq!(t.len(), 1, "{action}: asked again");
        assert!(answer(&mut src, t[0], number(1, VerifyState::Default)).safety);
        let v = src.take_safety().expect("the level");
        assert_eq!(v.notice, not_saved, "{action}: the neutral line");
    }
}

/// U2: a number that turned into another one while the level was open (no click) says so and
/// keeps saying so through equal re-answers and unrelated contact changes, until the next click
/// or the close; the same number again does not.
#[test]
fn a_number_that_changed_under_the_level_says_so_until_the_next_click() {
    let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
    let changed = || UiEvent::ContactsChanged {
        account: AccountId(1),
    };
    // K1 -> K2: the line, once.
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the new number");
    assert!(v.key == Some(IdentityFingerprint(vec![2; 33])));
    assert_eq!(v.notice, crate::safety::swapped_text());
    // K2 again: nothing new, and the line is not dropped by the next real change.
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(!answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    assert!(src.take_safety().is_none(), "an equal answer hands nothing");
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Verified)).safety);
    let v = src.take_safety().expect("the new state");
    assert_eq!(v.notice, crate::safety::swapped_text(), "kept");
    // The next click drops it.
    assert!(src.safety_action(
        AccountId(1),
        who,
        crate::safety::SafetyAction::Clear,
        IdentityFingerprint(vec![2; 33]),
    ));
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    assert_eq!(src.take_safety().expect("the level").notice, "");
}

/// V3: a number that turned into another across an answer WITHOUT a number (the account
/// stopped, a query failed) is a swap too: the key last SHOWN is held through it. A click sets it
/// to the key it acts on, so a swap after a click says so too (W3); a new request starts over.
#[test]
fn a_swap_across_an_answer_without_a_number_still_says_so() {
    let changed = || UiEvent::ContactsChanged {
        account: AccountId(1),
    };
    let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
    // K1 -> no number: nothing to compare, no line, and no buttons.
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], Err(SafetyUnavailable::NotRunning)).safety);
    let v = src.take_safety().expect("the answer without a number");
    assert!(v.key.is_none() && v.buttons.is_empty());
    assert_eq!(v.notice, "", "no line for a number that left");
    // ... -> K2: the number on screen was K1, so this IS a swap.
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the new number");
    assert!(v.key == Some(IdentityFingerprint(vec![2; 33])));
    assert_eq!(v.notice, crate::safety::swapped_text());
    // A click keeps the key it acted on (W3): K2 was on screen, the click, no number, K3 IS a
    // swap, and the click's own notice (none) does not hide it.
    assert!(src.safety_action(
        AccountId(1),
        who.clone(),
        crate::safety::SafetyAction::Mark,
        IdentityFingerprint(vec![2; 33]),
    ));
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], Err(SafetyUnavailable::NotRunning)).safety);
    let v = src.take_safety().expect("the answer without a number");
    assert_eq!(
        v.notice, "",
        "a click that is answered by nothing says nothing"
    );
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(3, VerifyState::Default)).safety);
    assert_eq!(
        src.take_safety().expect("the level").notice,
        crate::safety::swapped_text(),
        "the number that followed a click on K2 is not K2"
    );
    // The same number after a click is no swap (the accepted click's own answer).
    assert!(src.safety_action(
        AccountId(1),
        who,
        crate::safety::SafetyAction::Clear,
        IdentityFingerprint(vec![3; 33]),
    ));
    src.on_event(&changed());
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(3, VerifyState::Verified)).safety);
    assert_eq!(
        src.take_safety().expect("the level").notice,
        "",
        "K3 acted on, K3 shown"
    );
    // A new request starts over too: the first number of the next level is no swap.
    let again = user(8);
    assert!(src.request_safety(AccountId(1), again));
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(4, VerifyState::Default)).safety);
    assert_eq!(src.take_safety().expect("the level").notice, "");
}

/// U2: a refusal's line also survives an unrelated contact change (the equal re-answer carries
/// it again), until the next click.
#[test]
fn a_refusal_line_survives_an_unrelated_contact_change() {
    let (mut src, sent, who) = shown(&Store::new(&[(1, 3)]));
    src.on_event(&failed("SetVerified", stale_key(), who));
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the new number");
    assert!(!v.notice.is_empty());
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    let t = safety_queries(&sent);
    assert!(
        !answer(&mut src, t[0], number(2, VerifyState::Default)).safety,
        "the equal answer carries the same line: nothing new, nothing lost"
    );
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    let t = safety_queries(&sent);
    assert!(answer(&mut src, t[0], number(2, VerifyState::Verified)).safety);
    assert_eq!(
        src.take_safety().expect("the level").notice,
        v.notice,
        "kept through another state"
    );
}

/// Closing ends the level: an answer still on its way is dropped, a contact change asks
/// nothing.
#[test]
fn close_ends() {
    let who = user(7);
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    assert!(src.request_safety(AccountId(1), who));
    let t = safety_queries(&sent);
    src.close_safety();
    assert!(!answer(&mut src, t[0], number(1, VerifyState::Default)).safety);
    assert!(src.take_safety().is_none());
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    assert!(safety_queries(&sent).is_empty());
}

/// BQ-3 / critic F2: a send to the person that waits (the core's `waits`) makes the page offer
/// "Confirm New Safety Number" also for a person who was never verified; the core's
/// `SafetyWaitChanged` for THAT person asks again (another person's asks nothing), and the
/// answer without the wait drops the Confirm.
#[test]
fn a_waiting_send_offers_confirm_and_its_end_asks_again() {
    let who = user(7);
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    assert!(src.request_safety(AccountId(1), who.clone()));
    let t = safety_queries(&sent);
    assert!(answer_waits(&mut src, t[0], number(1, VerifyState::Default), true).safety);
    let v = src.take_safety().expect("the level");
    assert_eq!(
        v.buttons.first().map(|b| b.action),
        Some(crate::safety::SafetyAction::Confirm),
        "a waiting send: Confirm first"
    );
    src.on_event(&UiEvent::SafetyWaitChanged {
        account: AccountId(1),
        user: user(8),
    });
    assert!(
        safety_queries(&sent).is_empty(),
        "another person's wait asks nothing"
    );
    src.on_event(&UiEvent::SafetyWaitChanged {
        account: AccountId(1),
        user: who.clone(),
    });
    let t = safety_queries(&sent);
    assert_eq!(t.len(), 1, "the wait of the shown person asks again");
    assert!(answer(&mut src, t[0], number(1, VerifyState::Default)).safety);
    let v = src.take_safety().expect("the level");
    assert!(
        !v.buttons
            .iter()
            .any(|b| b.action == crate::safety::SafetyAction::Confirm),
        "no wait, never verified: Mark only"
    );
}

/// The level's buttons send what they name with the key they are given (the shown one):
/// Mark and Clear a `SetVerified`, Confirm a `ConfirmIdentity` and NEVER a Clear (BQ-2b).
#[test]
fn the_buttons_send_their_command_with_the_shown_key() {
    use crate::safety::SafetyAction;
    let who = user(7);
    let (mut src, sent) = opened(&Store::new(&[(1, 3)]));
    sent.borrow_mut().clear();
    let key = IdentityFingerprint(vec![5; 33]);
    for action in [
        SafetyAction::Mark,
        SafetyAction::Clear,
        SafetyAction::Confirm,
    ] {
        assert!(src.safety_action(AccountId(1), who.clone(), action, key.clone()));
    }
    let cmds = std::mem::take(&mut *sent.borrow_mut());
    assert_eq!(cmds.len(), 3, "one command per click");
    assert!(
        matches!(&cmds[0], UiCommand::SetVerified { account: AccountId(1), user, verified: true, key: k } if *user == who && *k == key),
        "Mark"
    );
    assert!(
        matches!(&cmds[1], UiCommand::SetVerified { account: AccountId(1), user, verified: false, key: k } if *user == who && *k == key),
        "Clear"
    );
    assert!(
        matches!(&cmds[2], UiCommand::ConfirmIdentity { account: AccountId(1), user, key: k } if *user == who && *k == key),
        "Confirm is ConfirmIdentity"
    );
}
