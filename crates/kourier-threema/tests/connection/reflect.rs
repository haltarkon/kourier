//! TH5 A5a: THE REFLECTION LOOP against the offline fake. A reflected envelope is opened, replay-checked,
//! made durable in ONE zone (its D2D nonce, its CSP nonce, a `deferred` row with the plaintext as
//! decrypted) and only THEN acked; a backlog is drained into batches of 30 (two zones for 31); a replay
//! is acked with nothing new; a box that does not open, a short one and a plaintext that is no envelope
//! are acked and dropped with one WARN each; an EPHEMERAL typing is never acked, one without the flag is;
//! an envelope whose own zone fails (H3, the test door) is acked and dropped while the others stay when the
//! failure is the CONTENT's, and is neither acked nor dropped when it is the STORE's (round A, H-1: the
//! connection closes 1000 and the redelivery is applied once); a server close found while draining is applied
//! and not acked (L-2).
//! Each test is a parent/child `unshare -rn` pair; both clocks stay where they are (nothing here waits
//! for a deadline); waits are wall bounds.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::Command;
use kourier_db::rusqlite::Connection;
use kourier_model::ConnState;
use kourier_threema::store;
use libthreema::protobuf::d2d;
use prost::Message as _;
use tracing::Level;

use super::harness::{self, H, SERVER_GROUP, scene};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::body;
use crate::support::mock::d2m::AckSeen;
use crate::support::mock::envelope::{Author, Msg, Reflect as _, Reflection, tampered, too_short};
use crate::support::mock::phone::Phone;
use crate::support::warns::Warns;

/// The phone's device id (the other device of the group).
pub(super) const PHONE: u64 = 9;
/// The sender of the incoming texts (synthetic).
pub(super) const PEER: &str = "PEERAAAA";
/// The upper bound of a wait (never a measure).
const WALL: Duration = Duration::from_secs(10);
/// SQLite's extended result codes the doors fail with (`sqlite.org/rescode.html`): a failure that is the
/// CONTENT's, and two that are the STORE's.
const CONSTRAINT_CHECK: i32 = 275;
const FULL: i32 = 13;
const IOERR_WRITE: i32 = 778;

pub(super) fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

pub(super) async fn connect(h: &mut H) {
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
}

/// A connected phone of the scene's group.
pub(super) fn phone_of(s: &harness::Scene) -> Phone {
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone
}

/// A message of a DEFERRED kind with the CSP nonce `[n; 24]`: a poll vote, kept whole (TH5b maps the reaction, the
/// edit and the delete, so a test that needs a row in `deferred` takes a vote; its body is opaque filler).
fn vote_msg(n: u8) -> Msg {
    Msg::raw([n; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, n)).nonce([n; 24])
}

/// An incoming envelope of a DEFERRED kind from the phone's side (A6: a text from a sender the file does not
/// know is dropped, decision M2): a poll vote.
fn vote(n: u8) -> d2d::Envelope {
    Author::new(PHONE).incoming(PEER, &vote_msg(n))
}

/// Waits (a wall bound) until the fake holds `n` reflected-acks in all.
pub(super) async fn wait_acks(h: &H, n: usize) -> Vec<AckSeen> {
    let net = h.net();
    let m = h.s.world.mediator();
    assert!(
        net.wait(|| m.reflected_acks().len() >= n, WALL).await,
        "the fake never saw {n} reflected-acks (it saw {})",
        m.reflected_acks().len()
    );
    let acks = m.reflected_acks();
    assert_eq!(acks.len(), n, "more reflected-acks than expected");
    acks
}

/// `count(*)` of `table` through a second reader of the file.
pub(super) fn rows(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count the rows")
}

/// The file's (deferred, d2d_nonce, csp_nonce) counts.
pub(super) fn counts(h: &H) -> (i64, i64, i64) {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        Ok::<_, kourier_db::Error>((
            rows(c, "deferred"),
            rows(c, "d2d_nonce"),
            rows(c, "csp_nonce"),
        ))
    })
    .expect("read the counts")
}

/// The deferred rows (kind, map_version, source, envelope) in id order.
pub(super) fn deferred(h: &H) -> Vec<(String, i64, String, Vec<u8>)> {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    db.read(|c: &Connection| {
        let mut stmt =
            c.prepare("SELECT kind, map_version, source, envelope FROM deferred ORDER BY id")?;
        let got = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, kourier_db::Error>(got)
    })
    .expect("read the deferred rows")
}

#[test]
fn a_reflected_text_is_in_the_file_when_its_ack_comes() {
    support::offline::run_child(
        module_path!(),
        "a_reflected_text_is_in_the_file_when_its_ack_comes_child",
    );
}

/// The ack follows the commit: the fake's ack hook reads the file AT the ack and finds the deferred row
/// and both nonces; the row is the plaintext exactly as the phone sealed it, kind `poll-vote` (A6: a deferred kind),
/// version 2 (the build's own `MAP_VERSION`, TH5b B1), source `reflected`.
#[tokio::test]
async fn a_reflected_text_is_in_the_file_when_its_ack_comes_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(511);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    let seen: Arc<Mutex<Vec<(i64, i64, i64)>>> = Arc::default();
    let (file, at_ack) = (h.db_file(), seen.clone());
    h.s.world.mediator().on_reflected_ack(move |_ack| {
        let db = store::open(&file, &key()).expect("open the file at the ack");
        let got = db
            .read(|c: &Connection| {
                Ok::<_, kourier_db::Error>((
                    rows(c, "deferred"),
                    rows(c, "d2d_nonce"),
                    rows(c, "csp_nonce"),
                ))
            })
            .expect("read at the ack");
        at_ack.lock().expect("the hook's list").push(got);
    });
    connect(&mut h).await;
    let envelope = vote(1);
    h.net().act(|_| {
        phone.reflect_envelope(&envelope);
    });
    let acks = wait_acks(&h, 1).await;
    assert!(acks[0].known, "the ack named an id the slot did not hold");
    assert_eq!(
        *seen.lock().expect("the hook's list"),
        vec![(1, 1, 1)],
        "the file did not hold the row and both nonces when the ack came"
    );
    let got = deferred(&h);
    assert_eq!(got.len(), 1, "not one deferred row");
    let (kind, version, source, bytes) = &got[0];
    assert!(
        kind == "poll-vote" && *version == 2 && source == "reflected",
        "the deferred row is not the vote's"
    );
    assert!(
        *bytes == envelope.encode_to_vec(),
        "the deferred row is not the plaintext as sealed"
    );
    h.finish().await;
}

#[test]
fn thirty_one_queued_reflections_are_two_zones_and_thirty_one_acks_in_order() {
    support::offline::run_child(
        module_path!(),
        "thirty_one_queued_reflections_are_two_zones_and_thirty_one_acks_in_order_child",
    );
}

/// H1, C4: 31 reflections queued for the slot while it was away arrive as one backlog; the drain makes
/// them two zones (30 datagrams per pass at most) and 31 acks, each a known id, in the slot's order.
#[tokio::test]
async fn thirty_one_queued_reflections_are_two_zones_and_thirty_one_acks_in_order_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(512);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", |s| matches!(s, ConnState::Offline { .. }))
        .await;
    h.net().act(|_| {
        for n in 1..=31u8 {
            phone.reflect_envelope(&vote(n));
        }
    });
    let zones = store::reflected_zones();
    connect(&mut h).await;
    let acks = wait_acks(&h, 31).await;
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    let ids: Vec<u32> = acks.iter().map(|a| a.reflected_id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "the acks are not in the slot's order"
    );
    assert_eq!(
        store::reflected_zones() - zones,
        2,
        "31 queued reflections were not two zones"
    );
    assert_eq!(counts(&h), (31, 31, 31), "not 31 rows and nonces");
    h.finish().await;
}

#[test]
fn a_replay_is_acked_and_writes_nothing_new() {
    support::offline::run_child(
        module_path!(),
        "a_replay_is_acked_and_writes_nothing_new_child",
    );
}

/// The same sealed bytes again (a redelivery after a kill between the commit and the ack): acked, and
/// the file holds the one row and the one nonce of the first time.
#[tokio::test]
async fn a_replay_is_acked_and_writes_nothing_new_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(513);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let mut first: Option<Reflection> = None;
    h.net().act(|_| {
        first = Some(phone.reflect_envelope(&vote(3)));
    });
    wait_acks(&h, 1).await;
    assert_eq!(counts(&h), (1, 1, 1), "the first time wrote other rows");
    let sealed = first.expect("the first reflection").sealed;
    h.net().act(|_| {
        phone.reflect_bytes(&sealed, 0);
    });
    let acks = wait_acks(&h, 2).await;
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    assert_eq!(counts(&h), (1, 1, 1), "the replay wrote something");
    h.finish().await;
}

#[test]
fn a_tampered_a_short_and_an_undecodable_one_are_acked_dropped_and_warned() {
    support::offline::run_child(
        module_path!(),
        "a_tampered_a_short_and_an_undecodable_one_are_acked_dropped_and_warned_child",
    );
}

/// C6: a box with a flipped byte (it does not open), 39 bytes (no nonce and tag), and a box that opens
/// to bytes that are no `d2d.Envelope`: each acked, nothing in the file, one WARN by its rule.
#[tokio::test]
async fn a_tampered_a_short_and_an_undecodable_one_are_acked_dropped_and_warned_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(514);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.net().act(|_| {
        let sealed = phone.seal(&vote(4));
        phone.reflect_bytes(&tampered(&sealed, 30), 0);
        phone.reflect_bytes(&too_short(), 0);
        // Field 1 with wire type 7: no protobuf.
        let garbage = phone.seal_bytes(&[0x0f; 40]);
        phone.reflect_bytes(&garbage, 0);
    });
    let acks = wait_acks(&h, 3).await;
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    assert_eq!(counts(&h), (0, 0, 0), "a dropped envelope wrote something");
    let warned = warns.at(Level::WARN);
    for rule in [
        "reflected-undecryptable",
        "reflected-too-short",
        "reflected-undecodable",
    ] {
        let line = format!("reflected envelopes acked and dropped: 1 by {rule}");
        assert_eq!(
            warned.iter().filter(|w| **w == line).count(),
            1,
            "not one WARN for {rule}"
        );
    }
    h.finish().await;
}

#[test]
fn an_ephemeral_typing_is_never_acked_and_one_without_the_flag_is() {
    support::offline::run_child(
        module_path!(),
        "an_ephemeral_typing_is_never_acked_and_one_without_the_flag_is_child",
    );
}

/// C5, M5: the phone's typing indicator with the EPHEMERAL flag: no ack (the fake would record
/// `reflected-ack-for-an-unknown-id` and close 4012), no row, no nonce; the same typing WITHOUT the flag
/// is an ordinary envelope: stored and acked. A deferred vote after both closes the order.
#[tokio::test]
async fn an_ephemeral_typing_is_never_acked_and_one_without_the_flag_is_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(515);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let author = Author::new(PHONE);
    let typing = author.incoming(PEER, &Msg::typing([5; 8], START_MS, 1));
    let plain_typing = author.incoming(PEER, &Msg::typing([6; 8], START_MS, 1));
    let mut ephemeral = None;
    h.net().act(|_| {
        ephemeral = Some(phone.reflect_envelope(&typing));
        phone.reflect_with(&plain_typing, 0);
        phone.reflect_envelope(&vote(7));
    });
    assert!(
        ephemeral.expect("the typing reflection").flags & 1 == 1,
        "the phone's typing was not ephemeral"
    );
    let acks = wait_acks(&h, 2).await;
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    h.sync().await;
    assert_eq!(
        h.s.world.mediator().reflected_acks().len(),
        2,
        "the ephemeral typing was acked"
    );
    // The typing without the flag is EPHEMERAL by its kind (A6): acked, its D2D nonce written, no row; the
    // deferred vote has its row and its CSP nonce (a typing carries none).
    assert_eq!(counts(&h), (1, 2, 1), "not the two acked envelopes");
    h.finish().await;
}

#[test]
fn an_envelope_whose_zone_fails_is_acked_and_dropped_and_the_others_stay() {
    support::offline::run_child(
        module_path!(),
        "an_envelope_whose_zone_fails_is_acked_and_dropped_and_the_others_stay_child",
    );
}

/// H3 (round A, H-1: the CONTENT branch): the door fails the deferred insert of ONE envelope with
/// `SQLITE_CONSTRAINT_CHECK` (its plaintext's length, made unique by its padding); the batch's zone fails,
/// the envelopes go again one per zone: the two others are stored, the failing one is acked and dropped
/// (its D2D nonce written alone, one WARN `zone-failed`), all three are acked and the connection stays.
#[tokio::test]
async fn an_envelope_whose_zone_fails_is_acked_and_dropped_and_the_others_stay_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let s = scene(516);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let bad = Author::new(PHONE).padding(300).incoming(PEER, &vote_msg(8));
    let (one, three) = (vote(1), vote(3));
    let length = bad.encode_to_vec().len();
    assert!(
        length != one.encode_to_vec().len() && length != three.encode_to_vec().len(),
        "the failing envelope's length is not unique"
    );
    store::fail_deferred_of_length(length, CONSTRAINT_CHECK);
    h.net().act(|_| {
        phone.reflect_envelope(&one);
        phone.reflect_envelope(&bad);
        phone.reflect_envelope(&three);
    });
    let acks = wait_acks(&h, 3).await;
    store::fail_deferred_of_length(0, 0);
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
    let kept: Vec<Vec<u8>> = deferred(&h).into_iter().map(|row| row.3).collect();
    assert!(
        kept == vec![one.encode_to_vec(), three.encode_to_vec()],
        "the two good envelopes are not the deferred rows"
    );
    // Three D2D nonces (the failing one's alone), the two good ones' CSP nonces.
    assert_eq!(counts(&h), (2, 3, 2), "not the rows of H3's retry");
    assert_eq!(
        warns
            .at(Level::WARN)
            .iter()
            .filter(|w| *w == "reflected envelopes acked and dropped: 1 by zone-failed")
            .count(),
        1,
        "not one WARN zone-failed"
    );
    assert_eq!(h.net().opened().len(), 1, "the connection did not stay");
    h.finish().await;
}

#[test]
fn a_store_failure_acks_nothing_closes_1000_and_the_redelivery_is_applied_once() {
    support::offline::run_child(
        module_path!(),
        "a_store_failure_acks_nothing_closes_1000_and_the_redelivery_is_applied_once_child",
    );
}

/// H3 (round A, H-1 / S1 / L-1: the STORE branch): the door fails the zone of ONE envelope with a code that
/// is the STORE's (`SQLITE_FULL`, then `SQLITE_IOERR_WRITE`), the probe of its nonce would commit: the
/// envelope is NOT acked and NOT dropped (a full disk must not turn a message into a lost one), nothing is
/// written, the client closes the socket 1000 and connects again after the first backoff step; on the
/// next socket (the door off) the mediator's redelivery is applied once and acked once.
#[tokio::test]
async fn a_store_failure_acks_nothing_closes_1000_and_the_redelivery_is_applied_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    for (seed, code) in [(5101, FULL), (5102, IOERR_WRITE)] {
        let s = scene(seed);
        let mut phone = phone_of(&s);
        let mut h = harness::start(s);
        connect(&mut h).await;
        let bad = Author::new(PHONE).padding(300).incoming(PEER, &vote_msg(8));
        store::fail_deferred_of_length(bad.encode_to_vec().len(), code);
        let first = h.net().opened()[0];
        h.net().act(|_| phone.reflect_envelope(&bad));
        let m = h.s.world.mediator();
        assert!(
            h.net().wait(|| m.closed(first) == Some(1000), WALL).await,
            "code {code}: the socket was not closed 1000"
        );
        assert!(
            m.reflected_acks().is_empty(),
            "code {code}: the store failed and the envelope was acked"
        );
        assert_eq!(
            counts(&h),
            (0, 0, 0),
            "code {code}: the failed envelope left rows (its nonce would make the redelivery a replay)"
        );
        assert!(
            warns
                .at(Level::WARN)
                .iter()
                .all(|w| !w.contains("zone-failed")),
            "code {code}: the store's failure was dropped by the content rule"
        );
        // The door off; the first backoff step; the mediator delivers the envelope again.
        store::fail_deferred_of_length(0, 0);
        let at = h.now() + 5_000;
        h.wait_state("Offline with the retry", |s| {
            *s == ConnState::Offline {
                retry_at_ms: Some(at),
            }
        })
        .await;
        h.advance_to(at).await;
        let acks = wait_acks(&h, 1).await;
        assert!(acks.iter().all(|a| a.known), "code {code}: an unknown id");
        assert_eq!(
            counts(&h),
            (1, 1, 1),
            "code {code}: the redelivery was not applied exactly once"
        );
        assert_eq!(h.net().opened().len(), 2, "code {code}: not two sockets");
        h.finish().await;
    }
}

#[test]
fn a_close_found_while_draining_is_applied_and_not_acked() {
    support::offline::run_child(
        module_path!(),
        "a_close_found_while_draining_is_applied_and_not_acked_child",
    );
}

/// L-2: three reflections and the server's close reach the client in ONE drain (the ack hook of the first
/// reflection holds the client's thread until the test has queued them): the pass is applied (four deferred
/// rows) and the connection ends `Closed`, but the three envelopes are not acked: the client never tried to
/// send a frame on the connection after the server's close (L13's rule for a close found with a batch).
#[tokio::test]
async fn a_close_found_while_draining_is_applied_and_not_acked_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(5103);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    let conn = h.conn();
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let first = std::sync::atomic::AtomicBool::new(true);
    h.s.world.mediator().on_reflected_ack(move |_| {
        // Runs on the client's thread, inside its send of the FIRST ack: it holds the client there.
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let _ = reached_tx.lock().expect("the hook's channel").send(());
            let _ = go_rx.lock().expect("the hook's channel").recv_timeout(WALL);
        }
    });
    h.net().act(|_| phone.reflect_envelope(&vote(1)));
    reached_rx
        .recv_timeout(WALL)
        .expect("the first ack never reached the mediator");
    // Everything below is queued while the client's thread is held: it reads it in one drain.
    h.net().act(|m| {
        for n in 2..=4 {
            phone.reflect_envelope(&vote(n));
        }
        m.close(conn, 1000);
    });
    go_tx.send(()).expect("the hook is waiting");
    h.wait_state("Offline", |s| {
        matches!(
            s,
            ConnState::Offline {
                retry_at_ms: Some(_)
            }
        )
    })
    .await;
    assert_eq!(
        counts(&h),
        (4, 4, 4),
        "the batch found with the close was not applied"
    );
    assert_eq!(
        h.net().late_sends(conn),
        0,
        "the client sent after the server's close"
    );
    assert_eq!(
        h.s.world.mediator().reflected_acks().len(),
        1,
        "the batch found with the close was acked"
    );
    h.finish().await;
}

/// The kept sum of `deferred` (`deferred_total`) through a second reader of the file.
fn kept_sum(h: &H) -> (i64, i64) {
    let db = store::open(&h.db_file(), &key()).expect("open the file");
    let total = db.read(store::deferred_total).expect("the kept sum");
    (total.rows, total.bytes)
}

/// A deferred text's plaintext (kind `envelope`: TH5a maps texts, so a re-map of its version would map it; the
/// age-bound tests seed it at the CURRENT version, 2, so the start leaves it and only the bound can take it).
fn deferred_text(n: u8) -> Vec<u8> {
    Author::new(PHONE)
        .incoming(PEER, &Msg::text([n; 8], START_MS - 1_000, "kept back"))
        .encode_to_vec()
}

#[test]
fn the_age_bound_runs_at_the_first_server_info_that_proves_the_clock() {
    support::offline::run_child(
        module_path!(),
        "the_age_bound_runs_at_the_first_server_info_that_proves_the_clock_child",
    );
}

/// Round B, S9 + S11: a deferred row 46 days old at the build's own version (2: TH5b B1 raised it, so a version-1
/// row would be mapped at the start); the actor's START (Resume) leaves it (no socket has said what time it is;
/// and a build maps no row of its own version again: nothing for the core); the first
/// `ServerInfo` whose clock check passes runs the age bound: the row is gone and the kept sum is (0, 0).
#[tokio::test]
async fn the_age_bound_runs_at_the_first_server_info_that_proves_the_clock_child() {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4808).with_contacts(&[PEER]);
    s.deferred = vec![(start - 46 * 86_400_000, deferred_text(1))];
    s.deferred_version = 2;
    let mut h = harness::start(s);
    h.sync().await;
    assert_eq!(
        h.file_rows("deferred"),
        1,
        "the start dropped a row before any server proved the clock"
    );
    assert_eq!(
        h.file_rows("core_outbox"),
        0,
        "the start mapped a row of its own version"
    );
    h.nothing_for_the_core();
    connect(&mut h).await;
    assert_eq!(
        kept_sum(&h),
        (0, 0),
        "the first good ServerInfo did not drop the 46-day-old row"
    );
    assert_eq!(h.file_rows("deferred"), 0);
    h.finish().await;
}

#[test]
fn a_skewed_clock_keeps_the_rows_past_45_days_also_when_connected_anyway() {
    support::offline::run_child(
        module_path!(),
        "a_skewed_clock_keeps_the_rows_past_45_days_also_when_connected_anyway_child",
    );
}

/// Round B, S9: the same 46-day-old row under a clock the mediator says is 21 minutes off: held for the clock
/// (no age bound); then "Connect Anyway" goes through the wrong clock, and still no age bound runs (the
/// start's nor a batch zone's: a reflected vote is deferred beside the old row).
#[tokio::test]
async fn a_skewed_clock_keeps_the_rows_past_45_days_also_when_connected_anyway_child() {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4810).with_contacts(&[PEER]);
    s.deferred = vec![(start - 46 * 86_400_000, deferred_text(1))];
    s.deferred_version = 2;
    s.world.mediator().knobs(|k| k.clock_skew_ms = 21 * 60_000);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("FixClock", |s| {
        matches!(
            s,
            ConnState::NeedsUserAction(kourier_model::UserAction::FixClock { .. })
        )
    })
    .await;
    assert_eq!(
        h.file_rows("deferred"),
        1,
        "a skewed clock dropped the 46-day-old row"
    );
    kourier_threema::connect_anyway(h.uuid());
    connect(&mut h).await;
    h.net().act(|_| phone.reflect_envelope(&vote(1)));
    wait_acks(&h, 1).await;
    let kinds: Vec<String> = deferred(&h).into_iter().map(|(kind, ..)| kind).collect();
    assert_eq!(
        kinds,
        ["envelope", "poll-vote"],
        "connected anyway, an age bound took the 46-day-old row"
    );
    h.finish().await;
}

#[test]
fn rows_a_newer_version_maps_again_reach_the_core_as_history_rows() {
    support::offline::run_child(
        module_path!(),
        "rows_a_newer_version_maps_again_reach_the_core_as_history_rows_child",
    );
}

/// TH5 A8 (decision M3) + round B (S6): a text deferred under version 1, mapped again by `remap_with(2)`
/// (what a version-2 build's start runs), reaches the CORE SIDE through the forwarder as a HISTORY row (one
/// `HistoryBatch` of its chat, `has_more: false`), never a catch-up `Message`, so the core's cut of a chat
/// cleared meanwhile refuses it; once the core took it, the file holds neither the row nor its `core_outbox`
/// row.
#[tokio::test]
async fn rows_a_newer_version_maps_again_reach_the_core_as_history_rows_child() {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4809).with_contacts(&[PEER]);
    s.deferred = vec![(start - 60_000, deferred_text(9))];
    s.remap_to = Some(2);
    let mut h = harness::start(s);
    let envelope = h.envelope().await;
    let ack = envelope.ack.expect("a durable envelope");
    let peer = kourier_model::UserId::from_parts("threema", PEER).expect("a user");
    match envelope.events.as_slice() {
        [
            kourier_backend_api::BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more: false,
                progress: None,
            },
        ] => {
            assert_eq!(
                *conv,
                kourier_model::ConvKey::Direct(peer),
                "the history batch is of another chat"
            );
            match messages.as_slice() {
                [m] => {
                    assert_eq!(m.conv, *conv, "the history row is in another chat");
                    assert_eq!(
                        m.body,
                        kourier_model::Body::text("kept back"),
                        "the re-mapped message lost its text"
                    );
                    assert_eq!(
                        m.received_at,
                        start - 60_000,
                        "received_at is not the row's"
                    );
                }
                _ => panic!("not the one history row"),
            }
        }
        _ => panic!("the re-mapped text did not reach the core as one history batch"),
    }
    ack.complete(Ok(()));
    h.sync().await;
    assert_eq!(
        h.file_rows("deferred"),
        0,
        "the re-mapped row is still deferred"
    );
    h.finish().await;
}

/// TH5b R17 (N-3): the version-1 rows of a TH5a file, written by the store's door before the start: a text, a
/// reaction to it, a poll vote and a second text, in that order.
fn n3_rows() -> Vec<Vec<u8>> {
    let author = Author::new(PHONE);
    vec![
        deferred_text(21),
        author
            .incoming(
                PEER,
                &Msg::reaction([23; 8], START_MS - 900, [21; 8], "\u{1f44d}"),
            )
            .encode_to_vec(),
        vote(24).encode_to_vec(),
        deferred_text(22),
    ]
}

#[test]
fn version_one_rows_of_several_kinds_are_mapped_by_a_real_start() {
    support::offline::run_child(
        module_path!(),
        "version_one_rows_of_several_kinds_are_mapped_by_a_real_start_child",
    );
}

/// TH5b R17 (N-3): a START of this build (no `remap_to`: the actor's own `Actor::start`, MAP_VERSION 2 over rows of
/// version 1) maps the rows before any connection: the core side gets ONE envelope holding the two texts as ONE
/// history batch of their chat (where the first text was) and the reaction as the live modifier after it, dated by
/// its own `created_at`; the vote is kept, deferred again at version 2.
#[tokio::test]
async fn version_one_rows_of_several_kinds_are_mapped_by_a_real_start_child() {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4811).with_contacts(&[PEER]);
    s.deferred = n3_rows().into_iter().map(|e| (start - 60_000, e)).collect();
    let mut h = harness::start(s);
    the_n3_rows_reach_the_core(&mut h).await;
    h.finish().await;
}

/// What a start that maps [`n3_rows`] ships and leaves (the literal events of
/// `version_one_rows_of_several_kinds_are_mapped_by_a_real_start`): ONE envelope with the two texts as ONE history
/// batch of their chat and the reaction as the live modifier after it, acked; then only the vote is left, at
/// version 2.
async fn the_n3_rows_reach_the_core(h: &mut H) {
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let envelope = h.envelope().await;
    let ack = envelope.ack.expect("a durable envelope");
    let peer = kourier_model::UserId::from_parts("threema", PEER).expect("a user");
    let chat = kourier_model::ConvKey::Direct(peer.clone());
    match envelope.events.as_slice() {
        [
            kourier_backend_api::BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more: false,
                progress: None,
            },
            reaction,
        ] => {
            assert_eq!(*conv, chat, "the history batch is of another chat");
            let locals: Vec<&str> = messages.iter().map(|m| &*m.local).collect();
            assert_eq!(
                locals,
                ["1515151515151515", "1616161616161616"],
                "the batch is not the two texts in row order"
            );
            for m in messages {
                assert_eq!(m.conv, chat, "a history row is in another chat");
                assert_eq!(m.body, kourier_model::Body::text("kept back"));
                assert_eq!(
                    m.received_at,
                    start - 60_000,
                    "received_at is not the row's"
                );
            }
            assert_eq!(
                *reaction,
                kourier_backend_api::BackendEvent::Reaction {
                    target: kourier_model::MsgTarget {
                        conv: Some(chat.clone()),
                        author: kourier_model::TargetAuthor::Unknown,
                        local: "1515151515151515".into(),
                        alternates: Vec::new(),
                    },
                    by: Some(peer.clone()),
                    emoji: "\u{1f44d}".to_owned(),
                    remove: false,
                    at: start - 900,
                },
                "the reaction is not the live modifier of the first text"
            );
        }
        _ => panic!(
            "the start's re-map did not reach the core as one history batch and the reaction"
        ),
    }
    ack.complete(Ok(()));
    h.sync().await;
    let left: Vec<(String, i64)> = deferred(h)
        .into_iter()
        .map(|(kind, version, ..)| (kind, version))
        .collect();
    assert_eq!(
        left,
        [("poll-vote".to_owned(), 2)],
        "the start did not keep exactly the vote at version 2"
    );
}

#[test]
fn a_start_whose_remap_the_store_stops_fails_connects_nothing_and_the_restart_maps_the_rows() {
    support::offline::run_child(
        module_path!(),
        "a_start_whose_remap_the_store_stops_fails_connects_nothing_and_the_restart_maps_the_rows_child",
    );
}

/// TH5b S-M1: the same version-1 rows, and the store's door makes the zone of the re-map's page fail with the
/// STORE's code (13, SQLITE_FULL) at the vote's row. The start FAILS (`Exit::Failed` with its fixed words, so the
/// core restarts it after its back-off) although the core queued `Connect` at once: the connector was asked for
/// nothing, every row stays exactly as it was (no zone committed), nothing reached the core. Then, the door off,
/// the core's restart over the same file maps them as a real start does and connects: no socket before the rows
/// the stopped re-map left are mapped (a newer sync delta applied live would be overwritten by an older row).
#[tokio::test]
async fn a_start_whose_remap_the_store_stops_fails_connects_nothing_and_the_restart_maps_the_rows_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4813).with_contacts(&[PEER]);
    s.deferred = n3_rows().into_iter().map(|e| (start - 60_000, e)).collect();
    let rows = n3_rows();
    let vote_length = rows[2].len();
    assert_eq!(
        rows.iter().filter(|e| e.len() == vote_length).count(),
        1,
        "the door's length names another row too"
    );
    // The door is read only by the batch and re-map zones (the seed's own inserts pass it).
    store::fail_deferred_of_length(vote_length, FULL);
    let mut h = harness::start(s);
    h.queue(Command::Connect);
    let exit = h.wait_exit().await;
    store::fail_deferred_of_length(0, 0);
    assert_eq!(
        exit,
        kourier_backend_api::Exit::Failed(kourier_model::BackendError::internal(
            "the deferred rows could not be mapped again"
        )),
        "the start whose re-map the store stopped did not fail"
    );
    assert_eq!(
        h.net().attempts(),
        0,
        "the failed start asked the connector for a connect"
    );
    let seeded: Vec<(String, i64, String, Vec<u8>)> = n3_rows()
        .into_iter()
        .map(|e| ("envelope".to_owned(), 1, "reflected".to_owned(), e))
        .collect();
    assert!(
        deferred(&h) == seeded,
        "a row of the stopped re-map is not as it was"
    );
    assert_eq!(
        h.file_rows("core_outbox"),
        0,
        "the stopped re-map wrote a core_outbox row"
    );
    h.nothing_for_the_core();

    let mut h = h.restart_after_exit();
    the_n3_rows_reach_the_core(&mut h).await;
    connect(&mut h).await;
    h.finish().await;
}

#[test]
fn a_start_under_a_cancelled_token_leaves_the_version_one_rows() {
    support::offline::run_child(
        module_path!(),
        "a_start_under_a_cancelled_token_leaves_the_version_one_rows_child",
    );
}

/// TH5b R17 (N-3; round A S5's wiring): the same file started with the core's cancel token ALREADY cancelled. The
/// start hands THAT token to the re-map, which stops before its first page: every row stays exactly as it was
/// (kind `envelope`, version 1, the same bytes), no `core_outbox` row was written (no re-map zone ran), nothing
/// reaches the core, and the actor ends `Shutdown` by itself.
#[tokio::test]
async fn a_start_under_a_cancelled_token_leaves_the_version_one_rows_child() {
    if !support::offline::in_child() {
        return;
    }
    let start = i64::try_from(START_MS).expect("ms fit i64");
    let mut s = scene(4812).with_contacts(&[PEER]);
    s.deferred = n3_rows().into_iter().map(|e| (start - 60_000, e)).collect();
    let mut h = harness::start_cancelled(s);
    h.wait_shutdown().await;
    let seeded: Vec<(String, i64, String, Vec<u8>)> = n3_rows()
        .into_iter()
        .map(|e| ("envelope".to_owned(), 1, "reflected".to_owned(), e))
        .collect();
    assert!(
        deferred(&h) == seeded,
        "a re-map zone ran under the cancelled token"
    );
    assert_eq!(
        h.file_rows("core_outbox"),
        0,
        "a re-map zone wrote a row under the cancelled token"
    );
    h.nothing_for_the_core();
    h.finish_after_shutdown();
}
