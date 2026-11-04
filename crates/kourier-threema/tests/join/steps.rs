//! The join's steps up to `EssentialData` (TH4 J3): B1's gate, `qr` -> `emoji` -> `linking`, the
//! cancels, the deadlines, the close codes, the bounds on arrival and the protocol errors.

use kourier_backend_api::Command;
use kourier_model::ErrorKind;
use kourier_threema::{NetError, UntrustedKind};
use libthreema::protobuf::{common, d2d_join};
use prost::Message as _;

use super::harness::{
    self, BEGIN_WITHIN, CONNECT_WITHIN, ESSENTIAL_WITHIN, NOMINATE_WITHIN, SCAN_WITHIN,
    SEND_WITHIN, Setup, start_login,
};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::essential::Spec;
use crate::support::mock::joiner::{JoinStep, JoinerKnobs, decode_qr};

const TRANSIENT: ErrorKind = ErrorKind::Transient { retry_after: None };
/// The ID is another account of the profile (`Setup::taken`): the data passed its own checks.
const TAKEN: ErrorKind = ErrorKind::AlreadyAdded { logged_out: false };
/// Kourier's bounds on the phone's blobs (decisions C5, C7, H4).
const MAX_BLOBS: usize = 1_000;
const MAX_BLOB_BYTES: usize = 32 * 1_048_576;
/// A blob id is 16 bytes (S7).
const BLOB_ID_LEN: usize = 16;

/// One `EdToNd` of the phone, encoded.
fn ed_to_nd(content: d2d_join::ed_to_nd::Content) -> Vec<u8> {
    d2d_join::EdToNd {
        content: Some(content),
    }
    .encode_to_vec()
}

fn begin() -> Vec<u8> {
    ed_to_nd(d2d_join::ed_to_nd::Content::Begin(d2d_join::Begin {}))
}

#[test]
fn an_empty_list_fails_at_qr_before_anything_is_made() {
    support::offline::run_child(
        module_path!(),
        "an_empty_list_fails_at_qr_before_anything_is_made_child",
    );
}

/// B1's gate (E2): with an EMPTY allow-list `StartLogin` fails at `qr` with `NotTheExpectedAccount`
/// at once: the parts' maker is not called, no connect is asked for, no relay side exists, no QR.
#[tokio::test]
async fn an_empty_list_fails_at_qr_before_anything_is_made_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        allowed: false,
        ..Setup::new(401)
    });
    j.send(start_login()).await;
    j.expect_failed("qr", 1, &ErrorKind::NotTheExpectedAccount)
        .await;
    j.no_more_steps().await;
    assert_eq!(j.net().made(), 0, "the transports were made");
    assert_eq!(j.net().attempts(), 0, "a connect was asked for");
    assert_eq!(j.world.relay().side_count(), 0, "a relay side exists");
    assert!(!j.identity_exists(), "an identity was written");
    j.finish().await;
}

#[test]
fn the_steps_go_qr_emoji_linking_and_a_taken_id_imports_nothing() {
    support::offline::run_child(
        module_path!(),
        "the_steps_go_qr_emoji_linking_and_a_taken_id_imports_nothing_child",
    );
}

/// `qr` (txn 1, one code that does not rotate, a text the phone decodes), the scan, `emoji` (txn 2:
/// the three glyphs of the PHONE's own path hash, computed by the fake's crypto: equal emoji are equal
/// hashes), `linking` (txn 3, at `Begin`); then `EssentialData` of an ID the profile has already
/// (`taken`): the login fails `AlreadyAdded` at `linking` (txn 4), the rendezvous is closed 4100 and
/// nothing is written (the whole join is `whole.rs`).
#[tokio::test]
async fn the_steps_go_qr_emoji_linking_and_a_taken_id_imports_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        taken: true,
        ..Setup::new(402)
    });
    let qr = j.shown().await;
    assert_eq!(j.net().made(), 1, "the transports were made once");
    let offer = decode_qr(&qr);
    assert!(offer.is_ok(), "the phone cannot read the QR text");
    j.scan(&qr);
    let emoji = match j.expect_step("emoji", 2).await.kind {
        kourier_model::LoginStepKind::ConfirmEmoji { emoji } => emoji,
        _ => panic!("the emoji step shows no emoji"),
    };
    let rph = j.joiner.rph().expect("the phone knows its path hash");
    let phone: Vec<String> = kourier_threema::emoji_of_path_hash(&rph)
        .iter()
        .map(|g| (*g).to_owned())
        .collect();
    assert_eq!(emoji.len(), 3, "not three emoji");
    assert!(emoji == phone, "the emoji differ from the phone's");
    let step = j.expect_step("linking", 3).await;
    assert!(
        matches!(
            step.kind,
            kourier_model::LoginStepKind::Progress { ref label, fraction: None } if label == "Linking"
        ),
        "the linking step is not the Linking progress"
    );
    j.expect_failed("linking", 4, &TAKEN).await;
    assert_eq!(
        j.joiner.step(),
        JoinStep::Closed,
        "the phone's path stays open"
    );
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    assert!(!j.identity_exists(), "an identity was written");
    j.only_the_rendezvous();
    j.finish().await;
}

#[test]
fn a_cancel_at_each_step_closes_with_1000_and_writes_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_cancel_at_each_step_closes_with_1000_and_writes_nothing_child",
    );
}

/// `CancelLogin` at `qr`, `emoji` and `linking`: the rendezvous is closed with 1000 (the phone sees
/// the 1000 the relay reflects), no identity is in the file, no mediator socket was asked for; the
/// core's `Logout` reports no device.
#[tokio::test]
async fn a_cancel_at_each_step_closes_with_1000_and_writes_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, at) in [(403, "qr"), (404, "emoji"), (405, "linking")] {
        let mut j = harness::start(Setup {
            joiner: JoinerKnobs {
                withhold_begin: true,
                ..JoinerKnobs::default()
            },
            ..Setup::new(seed)
        });
        match at {
            "qr" => {
                j.shown().await;
            }
            "emoji" => {
                j.nominated().await;
            }
            _ => {
                j.nominated().await;
                assert!(
                    j.joiner.send_ulp(&begin()),
                    "the phone could not send Begin"
                );
                j.net().flush();
                j.expect_step("linking", 3).await;
            }
        }
        j.send(Command::CancelLogin).await;
        j.no_more_steps().await;
        assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000 at {at}");
        if at != "qr" {
            assert_eq!(
                j.joiner.closed(),
                Some(1000),
                "the phone did not see 1000 at {at}"
            );
        }
        assert!(!j.identity_exists(), "an identity was written at {at}");
        j.only_the_rendezvous();
        j.finish_logged_out().await;
    }
}

#[test]
fn nobody_scans_and_the_code_expires_at_the_scan_deadline() {
    support::offline::run_child(
        module_path!(),
        "nobody_scans_and_the_code_expires_at_the_scan_deadline_child",
    );
}

/// Nobody scans: at `SCAN_WITHIN` (the relay's init timeout, 10 min by default, lies after it) the
/// login fails `Expired` at `qr`, the rendezvous closed with 1000.
#[tokio::test]
async fn nobody_scans_and_the_code_expires_at_the_scan_deadline_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(406));
    j.shown().await;
    j.advance_to(START_MS + SCAN_WITHIN).await;
    j.expect_failed("qr", 2, &ErrorKind::Expired).await;
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    j.only_the_rendezvous();
    j.finish().await;
}

#[test]
fn the_relays_init_timeout_before_the_scan_deadline_expires_the_code() {
    support::offline::run_child(
        module_path!(),
        "the_relays_init_timeout_before_the_scan_deadline_expires_the_code_child",
    );
}

/// Critic H3: the relay's init timeout set BELOW `SCAN_WITHIN` closes the path with 4003 before the
/// phone's `Hello`: `Expired` at `qr`, while the actor's own deadline was still ahead.
#[tokio::test]
async fn the_relays_init_timeout_before_the_scan_deadline_expires_the_code_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(407));
    j.world
        .relay()
        .knobs(|k| k.init_timeout = std::time::Duration::from_secs(60));
    j.shown().await;
    j.wait_deadline(START_MS + SCAN_WITHIN).await;
    j.step_to(START_MS + 60_000).await;
    j.expect_failed("qr", 2, &ErrorKind::Expired).await;
    assert_eq!(
        j.rendezvous_closed(),
        Some(4003),
        "the relay did not close 4003"
    );
    j.only_the_rendezvous();
    j.finish().await;
}

#[test]
fn a_phone_that_never_nominates_fails_at_qr_after_the_nomination_deadline() {
    support::offline::run_child(
        module_path!(),
        "a_phone_that_never_nominates_fails_at_qr_after_the_nomination_deadline_child",
    );
}

/// The phone scans and finishes the handshake but never nominates: `NOMINATE_WITHIN` after its
/// `Hello` the login fails at `qr` (the emoji were never shown), the rendezvous closed with 1000.
#[tokio::test]
async fn a_phone_that_never_nominates_fails_at_qr_after_the_nomination_deadline_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            never_nominate: true,
            ..JoinerKnobs::default()
        },
        ..Setup::new(408)
    });
    let qr = j.shown().await;
    j.scan(&qr);
    j.wait_joiner(JoinStep::Authenticated).await;
    j.advance_to(START_MS + NOMINATE_WITHIN).await;
    j.expect_failed("qr", 2, &ErrorKind::Unauthorized).await;
    assert_eq!(j.joiner.closed(), Some(1000), "the phone did not see 1000");
    j.finish().await;
}

#[test]
fn a_phone_that_withholds_begin_fails_at_emoji_after_the_begin_deadline() {
    support::offline::run_child(
        module_path!(),
        "a_phone_that_withholds_begin_fails_at_emoji_after_the_begin_deadline_child",
    );
}

/// The emoji are shown and the person never confirms: `BEGIN_WITHIN` after the nomination the login
/// fails at `emoji`, the rendezvous closed with 4100 (an upper-layer error).
#[tokio::test]
async fn a_phone_that_withholds_begin_fails_at_emoji_after_the_begin_deadline_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            withhold_begin: true,
            ..JoinerKnobs::default()
        },
        ..Setup::new(409)
    });
    j.nominated().await;
    j.advance_to(START_MS + BEGIN_WITHIN).await;
    j.expect_failed("emoji", 3, &ErrorKind::Unauthorized).await;
    assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
    j.finish().await;
}

#[test]
fn a_4100_of_the_phone_fails_the_login_at_the_step_it_came() {
    support::offline::run_child(
        module_path!(),
        "a_4100_of_the_phone_fails_the_login_at_the_step_it_came_child",
    );
}

/// The phone closes with 4100 after its `Hello` (`qr`), after its nomination (`emoji`) and after
/// `Begin` (`linking`): `Unauthorized` at that step.
#[tokio::test]
async fn a_4100_of_the_phone_fails_the_login_at_the_step_it_came_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, at, step, txn) in [
        (410, JoinStep::Scanned, "qr", 2),
        (411, JoinStep::Nominated, "emoji", 3),
        (412, JoinStep::Begun, "linking", 4),
    ] {
        let mut j = harness::start(Setup {
            joiner: JoinerKnobs {
                close_with: Some((at, 4100)),
                ..JoinerKnobs::default()
            },
            ..Setup::new(seed)
        });
        let qr = j.shown().await;
        j.scan(&qr);
        if txn >= 3 {
            j.expect_step("emoji", 2).await;
        }
        if txn >= 4 {
            j.expect_step("linking", 3).await;
        }
        j.expect_failed(step, txn, &ErrorKind::Unauthorized).await;
        assert_eq!(
            j.rendezvous_closed(),
            Some(4100),
            "the relay did not reflect 4100"
        );
        j.finish().await;
    }
}

/// One run per case: a phone that floods `flood` (count, bytes each) unreferenced blobs before the
/// referenced ones, for an ID the profile has (`taken`); the login fails with `kind` at `linking`
/// (`AlreadyAdded`: the data passed on to the rules), the rendezvous closed 4100, nothing written.
async fn flooded(seed: u64, flood: (usize, usize), kind: &ErrorKind) -> harness::J {
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            blob_flood: Some(flood),
            ..JoinerKnobs::default()
        },
        taken: true,
        ..Setup::new(seed)
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, kind).await;
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    assert!(!j.identity_exists(), "an identity was written");
    j
}

#[test]
fn blobs_at_the_count_bound_pass_on_and_one_more_fails_on_arrival() {
    support::offline::run_child(
        module_path!(),
        "blobs_at_the_count_bound_pass_on_and_one_more_fails_on_arrival_child",
    );
}

/// Critic H4: 1,000 blobs reach `EssentialData` (and the rules: `AlreadyAdded` at `linking`); 1,001
/// fail `Unsupported` at `linking` on arrival (before `EssentialData`).
#[tokio::test]
async fn blobs_at_the_count_bound_pass_on_and_one_more_fails_on_arrival_child() {
    if !support::offline::in_child() {
        return;
    }
    flooded(413, (MAX_BLOBS, 1), &TAKEN).await.finish().await;
    flooded(415, (MAX_BLOBS + 1, 1), &ErrorKind::Unsupported)
        .await
        .finish()
        .await;
}

#[test]
fn blobs_at_the_byte_bound_pass_on_and_one_byte_more_fails_on_arrival() {
    support::offline::run_child(
        module_path!(),
        "blobs_at_the_byte_bound_pass_on_and_one_byte_more_fails_on_arrival_child",
    );
}

/// Critic H4, S7: 32 MiB of blobs (ids and data) in all reach `EssentialData`; two bytes more (two
/// blobs of 16 MiB - 16 + 1 data bytes) fail `Unsupported` at `linking` on the second blob's arrival.
#[tokio::test]
async fn blobs_at_the_byte_bound_pass_on_and_one_byte_more_fails_on_arrival_child() {
    if !support::offline::in_child() {
        return;
    }
    // The sum counts each blob's 16-byte id with its data (S7): two blobs of `half - 16` data bytes
    // are exactly the bound.
    let data = MAX_BLOB_BYTES / 2 - BLOB_ID_LEN;
    flooded(414, (2, data), &TAKEN).await.finish().await;
    flooded(416, (2, data + 1), &ErrorKind::Unsupported)
        .await
        .finish()
        .await;
}

#[test]
fn data_before_begin_and_a_second_begin_are_protocol_errors() {
    support::offline::run_child(
        module_path!(),
        "data_before_begin_and_a_second_begin_are_protocol_errors_child",
    );
}

/// `BlobData` before `Begin` and `EssentialData` before `Begin` fail at `emoji`; a second `Begin`
/// fails at `linking`: each a protocol error (`Unauthorized`, closed 4100).
#[tokio::test]
async fn data_before_begin_and_a_second_begin_are_protocol_errors_child() {
    if !support::offline::in_child() {
        return;
    }
    let blob = ed_to_nd(d2d_join::ed_to_nd::Content::BlobData(common::BlobData {
        id: vec![1; 16],
        data: vec![2; 8],
    }));
    let data = ed_to_nd(d2d_join::ed_to_nd::Content::EssentialData(
        d2d_join::EssentialData::default(),
    ));
    for (seed, frames, step, txn) in [
        (417, vec![blob], "emoji", 3),
        (418, vec![data], "emoji", 3),
        (419, vec![begin(), begin()], "linking", 4),
    ] {
        let mut j = harness::start(Setup {
            joiner: JoinerKnobs {
                withhold_begin: true,
                ..JoinerKnobs::default()
            },
            ..Setup::new(seed)
        });
        j.nominated().await;
        for frame in &frames {
            assert!(j.joiner.send_ulp(frame), "the phone could not send");
        }
        j.net().flush();
        if txn == 4 {
            j.expect_step("linking", 3).await;
        }
        j.expect_failed(step, txn, &ErrorKind::Unauthorized).await;
        assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
        j.finish().await;
    }
}

#[test]
fn a_relay_close_before_the_scan_is_transient_at_qr() {
    support::offline::run_child(
        module_path!(),
        "a_relay_close_before_the_scan_is_transient_at_qr_child",
    );
}

/// Critic H3: the relay closes the path before anyone scanned with 4000 (not its init timeout):
/// `Transient` at `qr`.
#[tokio::test]
async fn a_relay_close_before_the_scan_is_transient_at_qr_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(420));
    j.shown().await;
    j.world.relay().close(j.side(), 4000);
    j.net().flush();
    j.expect_failed("qr", 2, &TRANSIENT).await;
    j.finish().await;
}

fn blob(id: u8) -> Vec<u8> {
    ed_to_nd(d2d_join::ed_to_nd::Content::BlobData(common::BlobData {
        id: vec![id; 16],
        data: vec![2; 8],
    }))
}

#[test]
fn every_message_after_begin_pushes_the_essential_deadline_out() {
    support::offline::run_child(
        module_path!(),
        "every_message_after_begin_pushes_the_essential_deadline_out_child",
    );
}

/// P3(a), C8: `EssentialData` is due `ESSENTIAL_WITHIN` after the LAST message heard, not after `Begin`:
/// four messages 90 s apart (270 s in all, each under 2 min after the one before) keep the join alive;
/// then 2 min of silence fail it `Transient` at `linking`, the rendezvous closed 4100.
#[tokio::test]
async fn every_message_after_begin_pushes_the_essential_deadline_out_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            withhold_begin: true,
            ..JoinerKnobs::default()
        },
        ..Setup::new(421)
    });
    j.nominated().await;
    assert!(
        j.joiner.send_ulp(&begin()),
        "the phone could not send Begin"
    );
    j.net().flush();
    j.expect_step("linking", 3).await;
    j.wait_deadline(j.now() + ESSENTIAL_WITHIN).await;
    for id in 1..=3 {
        j.step_to(j.now() + 90_000).await;
        j.no_more_steps().await;
        assert!(j.joiner.send_ulp(&blob(id)), "the phone could not send");
        j.net().flush();
        j.wait_deadline(j.now() + ESSENTIAL_WITHIN).await;
    }
    j.no_more_steps().await;
    j.advance_to(START_MS + 270_000 + ESSENTIAL_WITHIN).await;
    j.expect_failed("linking", 4, &TRANSIENT).await;
    assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    assert!(!j.identity_exists(), "an identity was written");
    j.finish().await;
}

#[test]
fn a_relay_connect_that_hangs_fails_at_qr_at_the_connect_deadline() {
    support::offline::run_child(
        module_path!(),
        "a_relay_connect_that_hangs_fails_at_qr_at_the_connect_deadline_child",
    );
}

/// P3(b): a connect to the relay that never completes fails `Transient` at `qr` after `CONNECT_WITHIN`
/// (20 s of the actor's clock), with no relay side and no QR.
#[tokio::test]
async fn a_relay_connect_that_hangs_fails_at_qr_at_the_connect_deadline_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(422));
    j.net().hold_connects(true);
    j.send(start_login()).await;
    j.advance_to(START_MS + CONNECT_WITHIN).await;
    j.expect_failed("qr", 1, &TRANSIENT).await;
    assert_eq!(j.net().attempts(), 1, "not exactly one connect");
    assert_eq!(j.world.relay().side_count(), 0, "a relay side exists");
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn the_phone_ending_the_path_is_unauthorized_at_the_step_it_came() {
    support::offline::run_child(
        module_path!(),
        "the_phone_ending_the_path_is_unauthorized_at_the_step_it_came_child",
    );
}

/// P3(c), P7: the phone's side of the path ends: it closes 1000 (reflected), or leaves with a code the
/// relay cannot reflect (1001: the relay tells Kourier 4004), after its `Hello` or its nomination; the
/// relay itself closes 4004 before anyone scanned. Each is `Unauthorized` at the step it came.
#[tokio::test]
async fn the_phone_ending_the_path_is_unauthorized_at_the_step_it_came_child() {
    if !support::offline::in_child() {
        return;
    }
    // (seed, the phone's close, the step, its txn)
    for (seed, close, step, txn) in [
        (423, Some((JoinStep::Nominated, 1000)), "emoji", 3),
        (424, Some((JoinStep::Nominated, 1001)), "emoji", 3),
        (425, Some((JoinStep::Scanned, 1001)), "qr", 2),
        (426, Some((JoinStep::Scanned, 1000)), "qr", 2),
        (427, None, "qr", 2),
    ] {
        let mut j = harness::start(Setup {
            joiner: JoinerKnobs {
                close_with: close,
                ..JoinerKnobs::default()
            },
            ..Setup::new(seed)
        });
        let qr = j.shown().await;
        match close {
            Some(_) => {
                j.scan(&qr);
                if txn == 3 {
                    j.expect_step("emoji", 2).await;
                }
            }
            None => {
                // 4004 BEFORE the phone's Hello (P7): the relay ends the RID's side.
                j.world.relay().close(j.side(), 4004);
                j.net().flush();
            }
        }
        j.expect_failed(step, txn, &ErrorKind::Unauthorized).await;
        j.finish().await;
    }
}

#[test]
fn a_rendezvous_that_breaks_closes_4000_and_the_phone_sees_4004() {
    support::offline::run_child(
        module_path!(),
        "a_rendezvous_that_breaks_closes_4000_and_the_phone_sees_4004_child",
    );
}

/// P3(d): the phone runs on another authentication key, so its `Hello` does not open: `Unauthorized` at
/// `qr`, Kourier closes the path with 4000 (the rendezvous protocol broken), the phone sees the relay's
/// 4004.
#[tokio::test]
async fn a_rendezvous_that_breaks_closes_4000_and_the_phone_sees_4004_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            wrong_ak: true,
            ..JoinerKnobs::default()
        },
        ..Setup::new(428)
    });
    let qr = j.shown().await;
    j.scan(&qr);
    j.expect_failed("qr", 2, &ErrorKind::Unauthorized).await;
    assert_eq!(j.rendezvous_closed(), Some(4000), "not closed 4000");
    assert_eq!(j.joiner.closed(), Some(4004), "the phone did not see 4004");
    j.finish().await;
}

#[test]
fn a_blob_cut_over_several_messages_is_put_together() {
    support::offline::run_child(
        module_path!(),
        "a_blob_cut_over_several_messages_is_put_together_child",
    );
}

/// P3(e): the phone cuts each `BlobData` frame (the real profile picture's) over 7 WebSocket messages:
/// the frame is put together (the data goes on to the rules: `AlreadyAdded` for a `taken` ID), not
/// refused as broken.
#[tokio::test]
async fn a_blob_cut_over_several_messages_is_put_together_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            blob_split: Some(7),
            ..JoinerKnobs::default()
        },
        spec: Spec {
            profile_picture: true,
            ..Setup::new(0).spec
        },
        taken: true,
        ..Setup::new(429)
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, &TAKEN).await;
    let cuts: Vec<usize> = j
        .joiner
        .sent()
        .iter()
        .filter(|(name, _)| *name == "blob-data")
        .map(|(_, n)| *n)
        .collect();
    assert_eq!(cuts, vec![7], "the picture's blob over seven messages");
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    j.finish().await;
}

#[test]
fn a_second_essential_data_fails_the_link_after_zone_one() {
    support::offline::run_child(
        module_path!(),
        "a_second_essential_data_fails_the_link_after_zone_one_child",
    );
}

/// P3(f): the phone sends `EssentialData` twice. The first is imported (zone 1: the file keeps the
/// identity, as after any failure past zone 1), the second is a protocol error of the join task: the
/// link fails `Unauthorized` at `linking`, the rendezvous is closed 4100 and the phone never gets
/// `Registered`, nothing reaches the core.
#[tokio::test]
async fn a_second_essential_data_fails_the_link_after_zone_one_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        joiner: JoinerKnobs {
            essential_twice: true,
            ..JoinerKnobs::default()
        },
        ..Setup::new(430)
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, &ErrorKind::Unauthorized)
        .await;
    assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    assert!(j.identity_exists(), "zone 1 was not written");
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    j.no_more_steps().await;
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "a core_outbox row was written"
    );
    j.finish().await;
}

#[test]
fn an_untrusted_relay_is_unsupported_and_an_unreached_one_transient_at_qr() {
    support::offline::run_child(
        module_path!(),
        "an_untrusted_relay_is_unsupported_and_an_unreached_one_transient_at_qr_child",
    );
}

/// P3(g): the relay's certificate is not the pinned one: `Unsupported` at `qr` (no loop, the app must
/// be updated); a relay that is only not reached is `Transient`. No relay side either way.
#[tokio::test]
async fn an_untrusted_relay_is_unsupported_and_an_unreached_one_transient_at_qr_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, error, kind) in [
        (
            431,
            NetError::Untrusted(UntrustedKind::PinMismatch),
            ErrorKind::Unsupported,
        ),
        (432, NetError::Failed("websocket"), TRANSIENT),
    ] {
        let mut j = harness::start(Setup::new(seed));
        j.net().fail_relay_connects(Some(error));
        j.send(start_login()).await;
        j.expect_failed("qr", 1, &kind).await;
        assert_eq!(j.net().attempts(), 1, "not exactly one connect");
        assert_eq!(j.world.relay().side_count(), 0, "a relay side exists");
        j.no_more_steps().await;
        j.finish().await;
    }
}

#[test]
fn a_blob_id_that_is_not_16_bytes_fails_on_arrival() {
    support::offline::run_child(
        module_path!(),
        "a_blob_id_that_is_not_16_bytes_fails_on_arrival_child",
    );
}

/// S7: a `BlobData` whose id is not 16 bytes (17, 15, none, a long one) is refused ON ARRIVAL, before
/// it is kept: `Unsupported` at `linking`, the rendezvous closed 4100, nothing written. The same join
/// with a 16-byte extra blob goes on to the rules (`AlreadyAdded`), so it is the length that decides.
#[tokio::test]
async fn a_blob_id_that_is_not_16_bytes_fails_on_arrival_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, len, kind) in [
        (433, 17, ErrorKind::Unsupported),
        (434, 15, ErrorKind::Unsupported),
        (435, 0, ErrorKind::Unsupported),
        (436, 4_096, ErrorKind::Unsupported),
        (437, 16, TAKEN),
    ] {
        let mut j = harness::start(Setup {
            joiner: JoinerKnobs {
                blob_id_len: Some(len),
                ..JoinerKnobs::default()
            },
            taken: true,
            ..Setup::new(seed)
        });
        j.nominated().await;
        j.expect_step("linking", 3).await;
        j.expect_failed("linking", 4, &kind).await;
        assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
        assert!(!j.identity_exists(), "an identity was written");
        j.finish().await;
    }
}

#[test]
fn a_stalled_write_to_the_relay_does_not_hold_the_cancel() {
    support::offline::run_child(
        module_path!(),
        "a_stalled_write_to_the_relay_does_not_hold_the_cancel_child",
    );
}

/// P8: the relay stops reading, so Kourier's `AuthHello` write pends. `CancelLogin` waits for the join,
/// and the join gives the write up after `SEND_WITHIN` of the actor's clock, then closes the path
/// 1000: the cancel is not held for good, the core's `Logout` ends the actor.
#[tokio::test]
async fn a_stalled_write_to_the_relay_does_not_hold_the_cancel_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(438));
    let qr = j.shown().await;
    j.net().stall_relay_sends(true);
    j.scan(&qr);
    let net = j.net().clone();
    assert!(
        net.wait(
            || net.stalled_sends() == 1,
            std::time::Duration::from_secs(60)
        )
        .await,
        "the write to the relay never pended"
    );
    j.send(Command::CancelLogin).await;
    j.advance_to(START_MS + SEND_WITHIN).await;
    j.no_more_steps().await;
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    j.finish_logged_out().await;
}
