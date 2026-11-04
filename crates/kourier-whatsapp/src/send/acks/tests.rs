//! The ack registry (W5a, D5): an answer reaches every wait of its id and nothing else, a second wait of a local
//! on the live connection is not first, the end of a connection and a withdrawn frame end their waits, and what
//! the ledger records of an answer. Synthetic ids only.

use std::time::Duration;

use super::{Ack, Acks, Answer, Heard, Nack, RATE_LIMITED_FLOOR, TRANSIENT_FLOOR, Waited};
use crate::send::ledger::Outcome;

const LOCAL_A: &str = "3EB0C4F7A91D58263104";
const LOCAL_B: &str = "3EB0D2E6B83C47195820";

fn ack(id: &str, answer: Answer) -> Ack {
    Ack {
        id: id.into(),
        answer,
    }
}

#[tokio::test]
async fn an_answer_reaches_every_wait_of_its_id_and_a_second_wait_on_the_connection_is_not_first() {
    let acks = Acks::default();
    let a1 = acks.wait(LOCAL_A);
    let a2 = acks.wait(LOCAL_A);
    let b = acks.wait(LOCAL_B);
    assert!(a1.first(), "the first wait of a local writes the frame");
    assert!(
        !a2.first(),
        "a second wait of a local whose frame is not answered on this connection must write nothing (D5)"
    );
    assert!(b.first(), "another local is first on its own");
    let answer = Answer::Acked {
        server_t: Some(1_760_000_123),
    };
    assert_eq!(acks.answer(ack(LOCAL_A, answer)), Heard::Waiter);
    assert_eq!(a1.answered().await, Waited::Answered(answer));
    assert_eq!(
        a2.answered().await,
        Waited::Answered(answer),
        "the second wait hears the first frame's answer"
    );
    assert_eq!(
        acks.answer(ack(LOCAL_A, answer)),
        Heard::Nobody,
        "an answered id has no wait left"
    );
    let again = acks.wait(LOCAL_A);
    assert!(
        !again.first(),
        "an acked local writes no second frame on the connection (D21)"
    );
    assert_eq!(
        again.answered().await,
        Waited::Answered(answer),
        "a later wait of an acked local hears the ack at once"
    );
    // B was not answered by A's ack: it is still waiting, and its own answer reaches it.
    let refused = Answer::Nacked(Nack::Code(463));
    assert_eq!(acks.answer(ack(LOCAL_B, refused)), Heard::Waiter);
    assert_eq!(b.answered().await, Waited::Answered(refused));
    assert!(
        !acks.wait(LOCAL_B).first(),
        "a refused local writes no second frame on the connection"
    );
}

/// Fix F1 (amends D21): EVERY answer is remembered for its connection. An ack for the rest of it (even when it came
/// after the wait gave up), a retryable nack only for its floor (429 five minutes, 421/500/503 thirty seconds;
/// then ONE new frame may go out), the other nacks for the rest of the connection; the connection's end forgets
/// all. Mutants: a nack not remembered (the core's retry of a 429 made a burst of frames); a floor of 0; a floor
/// that never ends; the memory kept across connections.
#[tokio::test]
async fn a_nack_is_remembered_for_its_floor_and_an_ack_for_the_connection() {
    let acks = Acks::default();
    // A 500: no frame for thirty seconds, then one.
    let first = acks.wait(LOCAL_A);
    assert!(first.first());
    let nacked = Answer::Nacked(Nack::Code(500));
    assert_eq!(acks.answer(ack(LOCAL_A, nacked)), Heard::Waiter);
    assert_eq!(first.answered().await, Waited::Answered(nacked));
    let held = acks.wait(LOCAL_A);
    assert!(
        !held.first(),
        "inside the floor of a 500 the local writes no second frame (F1)"
    );
    let left = held.left().expect("a remembered nack says what is left");
    assert!(
        left <= TRANSIENT_FLOOR && left > TRANSIENT_FLOOR - Duration::from_secs(10),
        "left {left:?}"
    );
    assert_eq!(held.answered().await, Waited::Answered(nacked));
    acks.advance(TRANSIENT_FLOOR - Duration::from_secs(10));
    assert!(
        !acks.wait(LOCAL_A).first(),
        "ten seconds before the floor's end it still holds"
    );
    acks.advance(Duration::from_secs(10));
    let second = acks.wait(LOCAL_A);
    assert!(second.first(), "after the floor ONE new frame may go out");
    assert!(
        !second.refresh_groups(),
        "only a 421 asks for fresh group metadata"
    );
    assert!(
        !acks.wait(LOCAL_A).first(),
        "the new frame is in the air: nobody else writes it"
    );
    // Its ack is remembered for the rest of the connection, however long.
    let acked = Answer::Acked { server_t: Some(9) };
    assert_eq!(acks.answer(ack(LOCAL_A, acked)), Heard::Waiter);
    acks.advance(Duration::from_secs(86_400));
    let after = acks.wait(LOCAL_A);
    assert!(!after.first(), "an acked local writes no frame again");
    assert_eq!(after.left(), None);
    assert_eq!(after.answered().await, Waited::Answered(acked));
    // A nack does not take the ack's place.
    acks.answer(ack(LOCAL_A, nacked));
    acks.advance(TRANSIENT_FLOOR);
    assert!(!acks.wait(LOCAL_A).first(), "a nack never undoes an ack");

    // A 429: five minutes.
    let rate = Answer::Nacked(Nack::Code(429));
    let wait = acks.wait(LOCAL_B);
    assert_eq!(acks.answer(ack(LOCAL_B, rate)), Heard::Waiter);
    drop(wait);
    let held = acks.wait(LOCAL_B);
    assert!(!held.first());
    let left = held.left().expect("left");
    assert!(
        left <= RATE_LIMITED_FLOOR && left > RATE_LIMITED_FLOOR - Duration::from_secs(30),
        "left {left:?}"
    );
    acks.advance(RATE_LIMITED_FLOOR - Duration::from_secs(60));
    assert!(!acks.wait(LOCAL_B).first(), "a 429 holds for five minutes");
    acks.advance(Duration::from_secs(60));
    assert!(acks.wait(LOCAL_B).first(), "then one frame");

    // The connection's end forgets everything.
    acks.connection_ended();
    assert!(acks.wait(LOCAL_A).first() && acks.wait(LOCAL_B).first());
}

/// Fix F1: a nack without a floor (the server refused the frame itself: 400, 404, 406, 415, 463, 475, 479, an
/// unknown or unreadable code) holds for the rest of the connection; a late answer that nobody waited for is
/// remembered too. Mutants: every nack given the 30 s floor; the memory kept across connections.
#[tokio::test]
async fn a_nack_without_a_floor_holds_for_the_rest_of_the_connection() {
    let acks = Acks::default();
    for (i, nack) in [
        Nack::Code(400),
        Nack::Code(404),
        Nack::Code(406),
        Nack::Code(415),
        Nack::Code(463),
        Nack::Code(475),
        Nack::Code(479),
        Nack::Code(999),
        Nack::Unreadable,
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("3EB0C4F7A91D5826{i:04}");
        assert_eq!(
            acks.answer(ack(&id, Answer::Nacked(nack))),
            Heard::Nobody,
            "nobody waits"
        );
        acks.advance(Duration::from_secs(86_400));
        let wait = acks.wait(&id);
        assert!(
            !wait.first(),
            "{nack:?} holds for the rest of the connection"
        );
        assert_eq!(wait.left(), None);
        assert_eq!(
            wait.answered().await,
            Waited::Answered(Answer::Nacked(nack))
        );
    }
    acks.connection_ended();
    assert!(
        acks.wait("3EB0C4F7A91D58260000").first(),
        "the next connection remembers nothing of the last one"
    );
}

/// Fix F1: a 421 (the group's addressing was stale) asks the FIRST frame after its floor for fresh group
/// metadata, and keeps asking until a frame is written. Mutant: the refresh never asked.
#[tokio::test]
async fn the_first_frame_after_a_421_asks_for_fresh_group_metadata() {
    let acks = Acks::default();
    let stale = Answer::Nacked(Nack::Code(421));
    drop(acks.wait(LOCAL_A));
    acks.answer(ack(LOCAL_A, stale));
    assert!(!acks.wait(LOCAL_A).first(), "the floor holds");
    acks.advance(TRANSIENT_FLOOR);
    let retry = acks.wait(LOCAL_A);
    assert!(retry.first() && retry.refresh_groups());
    assert!(
        !acks.wait(LOCAL_A).refresh_groups(),
        "only the frame that is written asks"
    );
    acks.withdraw(LOCAL_A, retry.connection());
    let again = acks.wait(LOCAL_A);
    assert!(
        again.first() && again.refresh_groups(),
        "a frame that never left leaves the refresh to the next one"
    );
    // Its 421 holds again for a floor.
    acks.answer(ack(LOCAL_A, stale));
    assert!(!acks.wait(LOCAL_A).first());
}

/// Fix F1: the floors of the table.
#[test]
fn the_floors_are_five_minutes_for_a_429_and_thirty_seconds_for_421_500_and_503() {
    assert_eq!(Nack::Code(429).floor(), Some(Duration::from_secs(300)));
    for code in [421, 500, 503] {
        assert_eq!(Nack::Code(code).floor(), Some(Duration::from_secs(30)));
    }
    for code in [400, 404, 406, 415, 463, 475, 479, 999] {
        assert_eq!(Nack::Code(code).floor(), None, "code {code}");
    }
    assert_eq!(Nack::Unreadable.floor(), None);
}

#[tokio::test]
async fn the_end_of_a_connection_ends_its_waits_and_a_withdrawn_frame_ends_only_its_own() {
    let acks = Acks::default();
    let old = acks.wait(LOCAL_A);
    assert_eq!(old.connection(), 0);
    acks.connection_ended();
    assert_eq!(acks.connection(), 1);
    assert_eq!(
        old.answered().await,
        Waited::Ended,
        "no answer comes on a connection that ended"
    );
    let live = acks.wait(LOCAL_A);
    assert!(
        live.first(),
        "a resend may go on a LATER connection (D5): its wait is first"
    );
    assert_eq!(live.connection(), 1);
    acks.withdraw(LOCAL_A, 0);
    assert!(
        !acks.wait(LOCAL_A).first(),
        "a withdrawal for an older connection must leave the live connection's wait"
    );
    acks.withdraw(LOCAL_A, 1);
    assert_eq!(live.answered().await, Waited::Ended);
    assert!(
        acks.wait(LOCAL_A).first(),
        "a withdrawn frame never left: the next wait writes it"
    );
    assert_eq!(
        acks.answer(ack(LOCAL_B, Answer::Acked { server_t: None })),
        Heard::Nobody,
        "an ack nobody waits for is said to be heard by nobody"
    );
}

#[test]
fn the_ledger_records_an_ack_and_a_463_and_no_other_nack() {
    assert_eq!(
        Answer::Acked { server_t: Some(7) }.recorded(),
        Some(Outcome::Acked { server_t: Some(7) })
    );
    assert_eq!(
        Answer::Nacked(Nack::Code(463)).recorded(),
        Some(Outcome::Refused)
    );
    for nack in [
        Nack::Code(479),
        Nack::Code(400),
        Nack::Code(429),
        Nack::Code(503),
        Nack::Unreadable,
    ] {
        assert_eq!(
            Answer::Nacked(nack).recorded(),
            None,
            "a nack other than 463 must not be recorded (critic H2)"
        );
    }
}

/// D5 (chunk 9b): the end of a connection that heard no ack in time is claimed by ONE wait, and the claim ends only
/// its own connection: one that ended meanwhile is not ended again, so the live one's waits go on. Mutants: a
/// second claim granted; the end of an older connection ends the live one.
#[tokio::test]
async fn the_end_of_a_connection_is_claimed_once_and_ends_only_that_connection() {
    let acks = Acks::default();
    let mut lost = acks.wait(LOCAL_A);
    assert!(
        acks.claim_end(0),
        "the first wait that gives up ends its connection"
    );
    assert!(
        !acks.claim_end(0),
        "a second wait of the same connection leaves the end to the first"
    );
    acks.end(0);
    assert_eq!(acks.connection(), 1);
    assert_eq!(lost.hear().await, Waited::Ended);
    assert_eq!(
        lost.hear().await,
        Waited::Ended,
        "a wait that ended stays ended"
    );
    let live = acks.wait(LOCAL_B);
    assert!(
        !acks.claim_end(0),
        "a connection that ended is not claimed again"
    );
    acks.end(0);
    assert_eq!(
        acks.connection(),
        1,
        "the end of an older connection ends nothing of the live one"
    );
    let answer = Answer::Acked { server_t: Some(5) };
    assert_eq!(acks.answer(ack(LOCAL_B, answer)), Heard::Waiter);
    assert_eq!(live.answered().await, Waited::Answered(answer));
    assert!(
        acks.claim_end(1),
        "the next connection's end can be claimed"
    );
}
