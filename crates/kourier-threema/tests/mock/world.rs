//! The fake's world (`support/mock/mod.rs`): seeded, a virtual clock, the identity table, the
//! violations and the trace.

use std::time::Duration;

use crate::support::mock::{Dir, START_MS, is_threema_id, world};

#[test]
fn one_seed_gives_one_world_and_another_seed_another() {
    let (a, b, c) = (world(7), world(7), world(8));
    assert!(
        a.server_key().public == b.server_key().public,
        "same seed, other SK"
    );
    assert!(
        a.server_key().public != c.server_key().public,
        "other seed, same SK"
    );
    let (ia, ib) = (a.add_identity("TH2AAAA1"), b.add_identity("TH2AAAA1"));
    assert!(ia.ck.public == ib.ck.public, "same seed, other CK");
    assert!(ia.ck.public != a.server_key().public, "a CK equals the SK");
    assert!(
        a.random::<16>() == b.random::<16>(),
        "same seed, other bytes"
    );
    assert!(
        a.random::<16>() != a.random::<16>(),
        "the RNG repeats itself"
    );
}

#[test]
fn the_identity_table_holds_what_was_added() {
    let w = world(1);
    let me = w.add_identity("TH2BBBB2");
    w.add_identity("*TH2GATE");
    assert_eq!(w.identity_count(), 2);
    let found = w
        .identity("TH2BBBB2")
        .expect("the identity is in the table");
    assert!(found.ck.public == me.ck.public, "another key came back");
    assert!(w.identity("TH2NONE0").is_none(), "an unknown ID was found");
    assert!(
        format!("{me:?}") == "TestIdentity",
        "Debug shows the ID or the key"
    );
    for bad in ["", "TH2CCC3", "TH2CCCC33", "th2cccc3", "TH2-CCC3"] {
        assert!(!is_threema_id(bad), "a malformed ID passed the form check");
    }
}

#[test]
#[should_panic(expected = "a test identity was added twice")]
fn an_identity_cannot_be_added_twice() {
    let w = world(2);
    w.add_identity("TH2DDDD4");
    w.add_identity("TH2DDDD4");
}

#[test]
fn the_clock_moves_only_when_the_test_moves_it() {
    let w = world(3);
    assert_eq!(w.clock().now_ms(), START_MS);
    assert_eq!(w.clock().now_ms(), START_MS, "the clock moved by itself");
    w.clock().advance(Duration::from_secs(90));
    assert_eq!(w.clock().now_ms(), START_MS + 90_000);
}

#[test]
fn violations_are_kept_in_order_with_their_ids() {
    let w = world(4);
    w.violations().assert_empty();
    w.violations().record("drop-least-recent", &[("device", 9)]);
    w.violations().record("unblock-as-follower", &[]);
    assert_eq!(
        w.violations().rules(),
        ["drop-least-recent", "unblock-as-follower"]
    );
    assert_eq!(w.violations().list()[0].ids, [("device", 9)]);
}

#[test]
#[should_panic(expected = "the client broke 1 rule(s) of the fake: [\"repeated-reflect-id\"]")]
fn a_violation_fails_the_test_with_its_rule() {
    let w = world(5);
    w.violations()
        .record("repeated-reflect-id", &[("reflect", 3)]);
    w.violations().assert_empty();
}

/// One line per frame, numbered, types and numbers only; with a file each line is on disk when
/// `frame` returns (fsynced).
#[test]
fn the_trace_writes_one_synthetic_line_per_frame() {
    let w = world(6);
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("trace.log");
    w.trace().frame(Dir::Out, 1, "d2m.server-hello", &[]);
    w.trace().to_file(&path).expect("open the trace file");
    w.trace().frame(
        Dir::In,
        1,
        "d2m.client-hello",
        &[("device", 18_446_744_073_709_551_615)],
    );
    let on_disk = std::fs::read_to_string(&path).expect("read the trace");
    assert_eq!(
        on_disk, "2 1 in d2m.client-hello device=18446744073709551615\n",
        "the file holds the lines from `to_file` on"
    );
    w.trace()
        .frame(Dir::Out, 2, "d2m.reflect-ack", &[("reflect", 5), ("ts", 7)]);
    assert_eq!(
        w.trace().lines(),
        [
            "1 1 out d2m.server-hello",
            "2 1 in d2m.client-hello device=18446744073709551615",
            "3 2 out d2m.reflect-ack reflect=5 ts=7",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .count(),
        2,
        "every line after `to_file` is written"
    );
}
