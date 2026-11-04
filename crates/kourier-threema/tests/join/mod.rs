//! The device join (TH4 J3) against the offline fake: the real actor, its join task over libthreema's
//! real `RendezvousProtocol` as the RID, the fake's relay and its scripted joining phone (`joiner`).
//! Each test is a PAIR (rule 4): the parent starts the child in an `unshare -rn` process, the child
//! (a tokio test) does the work and ends with the harness's `finish*` (the fake's violations are its
//! last check; the sweep `mock::every_join_test_ends_with_the_violations_check` holds it). No wall
//! clock and no sleep: the actor's `FakeClock` and the fake's virtual clock move together, to the
//! deadlines the test expects; waits for a step have a wall BOUND only.

mod chats;
mod harness;
mod reflect;
mod refused;
mod steps;
mod whole;
