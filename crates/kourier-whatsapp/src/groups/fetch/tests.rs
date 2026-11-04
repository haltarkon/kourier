//! W6a2, chunk B6: the fetch policy's bucket (M1). Pure: the time is the test's. The task and its decisions are tested at the
//! actor (`actor::tests`, on the account's clock).

use std::time::Duration;

use whatsapp_rust::{GroupMetadata, GroupMetadataResult};

use super::{Bucket, REFUSED_PAUSE, Steady, Tally, say_fetched, tally};

const T0: u64 = 1_800_000_000_000;

fn secs(s: u64) -> u64 {
    s * 1000
}

/// A fresh bucket lets a fetch go; the next one waits for the floor, however little time passed; after the floor it goes.
/// Kills "the floor is 0".
#[test]
fn the_floor_is_thirty_seconds_between_two_fetches() {
    let mut bucket = Bucket::default();
    assert_eq!(bucket.wait(T0), Duration::ZERO);
    bucket.take(T0);
    assert_eq!(bucket.wait(T0), Duration::from_secs(30), "no time passed");
    assert_eq!(bucket.wait(T0 + secs(12)), Duration::from_secs(18));
    assert_eq!(bucket.wait(T0 + secs(30)), Duration::ZERO);
}

/// Ten fetches inside ten minutes are the most: the eleventh waits until the oldest of the last ten leaves the window, and
/// then goes. Kills "no cap per window" (the floor alone would let it go at 300 s) and "the window counts from the last
/// fetch".
#[test]
fn ten_fetches_per_ten_minutes_is_the_most() {
    let mut bucket = Bucket::default();
    for k in 0..10_u64 {
        let now = T0 + secs(30 * k);
        assert_eq!(bucket.wait(now), Duration::ZERO, "fetch {k}");
        bucket.take(now);
    }
    // 270 s after the first: the floor is long over, the window is full.
    let now = T0 + secs(300);
    assert_eq!(bucket.wait(now), Duration::from_secs(300));
    assert_eq!(bucket.wait(T0 + secs(599)), Duration::from_secs(1));
    assert_eq!(bucket.wait(T0 + secs(600)), Duration::ZERO);
}

/// A server that asked for a delay closes the bucket for it, and a delay never shortens a longer one. A 429 with no
/// delay of its own closes it for `REFUSED_PAUSE`. Kills "the delay is ignored".
#[test]
fn a_servers_delay_closes_the_bucket() {
    let mut bucket = Bucket::default();
    bucket.close_for(T0, Duration::from_secs(120));
    assert_eq!(bucket.wait(T0), Duration::from_secs(120));
    bucket.close_for(T0, Duration::from_secs(10));
    assert_eq!(bucket.wait(T0 + secs(20)), Duration::from_secs(100));
    assert_eq!(bucket.wait(T0 + secs(120)), Duration::ZERO);
    assert_eq!(REFUSED_PAUSE, Duration::from_secs(600));
}

/// A clock that was set back cannot close the bucket for longer than the window: fetches stamped in the future are
/// answered with at most `WINDOW`.
#[test]
fn a_clock_set_back_closes_the_bucket_for_no_more_than_the_window() {
    let mut bucket = Bucket::default();
    bucket.take(T0 + secs(3 * 3600));
    assert_eq!(bucket.wait(T0), Duration::from_secs(600));
}

/// Fix F7b (PROTOCOL-5): the bucket counts on a reading of the clock that never goes back. The clock is set back three hours
/// after a fetch: nothing moves (the reading stays), and from there the floor is 30 seconds of the time that passes, every
/// wait asserted (the policy asks `wait` again after each sleep, so one `wait()` proves nothing). A step forward is read as
/// it is.
/// Mutants: the reading not steadied (the wall clock as it is).
#[test]
fn a_clock_set_back_does_not_shut_the_bucket_for_the_skew() {
    let mut steady = Steady::default();
    let mut bucket = Bucket::default();
    let first = steady.read(T0);
    assert_eq!(first, T0);
    bucket.take(first);

    let back = T0 - secs(3 * 3600);
    assert_eq!(steady.read(back), T0, "a step back moves nothing");
    let waits: Vec<Duration> = [0_u64, 10, 29, 30, 31]
        .into_iter()
        .map(|passed| bucket.wait(steady.read(back + secs(passed))))
        .collect();
    assert_eq!(
        waits,
        [
            Duration::from_secs(30),
            Duration::from_secs(20),
            Duration::from_secs(1),
            Duration::ZERO,
            Duration::ZERO,
        ],
        "the floor runs from the time that passed after the step"
    );

    // A second step back later is steadied the same way, and a step forward counts as it reads.
    let again = back + secs(31) - secs(600);
    assert_eq!(steady.read(again), back_plus(31));
    assert_eq!(steady.read(again + secs(5)), back_plus(36));
    assert_eq!(steady.read(again + secs(7200)), back_plus(36) + secs(7195));
}

/// The steadied reading `passed` seconds after the first step back, as the test's arithmetic has it: the fetch at `T0`, the
/// step back, `passed` seconds of the clock.
fn back_plus(passed: u64) -> u64 {
    T0 + secs(passed)
}

fn jid(digits: &str) -> wacore_binary::Jid {
    format!("{digits}@g.us").parse().expect("a jid")
}

fn answer(digits: &str, flagged: bool) -> GroupMetadataResult {
    let mut metadata = GroupMetadata::new(jid(digits));
    metadata.has_missing_participant_identification = flagged;
    GroupMetadataResult::Found(Box::new(metadata))
}

/// T2 C4: the tally counts the answers that state no whole member list, and only those: a `Found` answer flagged
/// `missing_participant_identification` and a `Truncated` one; a whole `Found`, a `Forbidden` and a `NotFound` are none.
/// Kills "a whole answer counts as flagged", "a truncated answer is not counted" and "a refusal counts".
#[test]
fn the_tally_counts_the_flagged_and_the_truncated_answers_and_no_other() {
    let answers = vec![
        answer("120363999000000011", false),
        answer("120363999000000012", true),
        answer("120363999000000013", true),
        GroupMetadataResult::Truncated {
            id: jid("120363999000000014"),
            participant_count: 1500,
        },
        GroupMetadataResult::Forbidden(jid("120363999000000015")),
        GroupMetadataResult::NotFound(jid("120363999000000016")),
    ];
    assert_eq!(
        tally(&answers),
        Tally {
            flagged: 2,
            truncated: 1
        }
    );
    assert_eq!(tally(&[]), Tally::default());
}

/// T2 C4: the start's fetch says ONE info line with the four counts under the words the run tool reads, and nothing else:
/// no id, no title, no name. The run tool's `START_FETCH_LINE` test compares these words with this file's source.
/// Kills "a field of the line is dropped or renamed" and "an id is added".
#[test]
fn the_start_fetch_line_has_the_fixed_words_and_the_four_counts() {
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    say_fetched(
        7,
        6,
        Tally {
            flagged: 2,
            truncated: 1,
        },
    );
    let all = lines.all();
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(
        all[0].contains("the groups were fetched at the start"),
        "{all:?}"
    );
    for field in ["asked=7", "answered=6", "flagged=2", "truncated=1"] {
        assert!(all[0].contains(field), "{field} missing in {all:?}");
    }
    assert!(
        !all[0].contains("@g.us") && !all[0].contains("120363"),
        "an id in the line: {all:?}"
    );
}
