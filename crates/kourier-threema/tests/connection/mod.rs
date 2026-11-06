//! The account's connection (TH3) against the offline fake: the real actor, its real `D2mProtocol`
//! and `CspProtocol`, the fake's parts (`mock::parts`). Each test is a PAIR (rule 4, call C13): the
//! parent test starts the child in an `unshare -rn` process, the child (a tokio test) does the
//! work with the fake living beside the actor and ends with the harness's `finish` (the fake's
//! violations are its last check; the sweep `mock::every_connection_test_ends_with_the_violations_check`
//! holds it). No wall clock and no sleep: the actor's `FakeClock` and the fake's virtual clock move
//! together, to deadlines the test expects (critic M5); waits for a state have a wall BOUND only.

mod alive;
mod boundary;
mod clear;
mod clock;
mod close;
mod deadline;
mod desktop_cases;
mod harness;
mod kill;
mod leader;
mod leader_contacts;
mod leader_desktop;
mod leader_jobs;
mod leader_kill;
mod lines;
mod logout;
mod receive;
mod reflect;
mod seed;
mod send;
mod send_desktop;
mod send_kill;
mod slot;
mod sync;
mod up;
