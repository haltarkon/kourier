//! The fake rendezvous relay (`support/mock/relay.rs`, TH4 J2) on its own, sans-I/O: the two sides of
//! a path are driven by hand (the joiner's tests drive the real libthreema RID through it). Every
//! test ends with the violations check.

use std::time::Duration;

use crate::support::mock::d2m::Out;
use crate::support::mock::relay::{DEFAULT_INIT_TIMEOUT, Role, SideId};
use crate::support::mock::{MockWorld, world};

/// A relay URL of the 32-byte path `first, 1, 2, ...` (two-digit prefix8, as P11 fills it).
fn url(first: u8) -> String {
    let hex: String = std::iter::once(first)
        .chain(1..32u8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("wss://rendezvous-{}.invalid/{}/{hex}", &hex[..1], &hex[..2])
}

fn pair(w: &MockWorld, first: u8) -> (SideId, SideId) {
    let rid = w.relay().connect(&url(first), Role::Rid);
    let rrd = w.relay().connect(&url(first), Role::Rrd);
    (rid, rrd)
}

/// Bytes pass unchanged, one message for one, both ways.
#[test]
fn bytes_pass_unchanged_both_ways() {
    let w = world(401);
    let r = w.relay();
    let (rid, rrd) = pair(&w, 0xab);
    r.receive(rrd, vec![1, 2, 3]);
    r.receive(rrd, vec![4]);
    r.receive(rid, vec![9; 70_000]);
    assert_eq!(
        r.take(rid),
        vec![Out::Datagram(vec![1, 2, 3]), Out::Datagram(vec![4])],
        "the RID got other bytes"
    );
    assert_eq!(
        r.take(rrd),
        vec![Out::Datagram(vec![9; 70_000])],
        "the RRD got other bytes"
    );
    assert_eq!(r.passed().len(), 3, "three messages passed");
    w.violations().assert_empty();
}

/// `1000` and every `41xx` are reflected to the other side, from either side.
#[test]
fn a_normal_close_and_4100_are_reflected() {
    let w = world(402);
    let r = w.relay();
    for (n, code) in [1000u16, 4100, 4111, 4199].into_iter().enumerate() {
        let (rid, rrd) = pair(&w, 0x20 + u8::try_from(n).expect("small"));
        r.client_close(rid, code);
        assert_eq!(
            r.take(rrd),
            vec![Out::Close(code)],
            "not reflected to the RRD"
        );
        assert_eq!(r.take(rid), vec![], "the closing side got something");
        let (rid, rrd) = pair(&w, 0x30 + u8::try_from(n).expect("small"));
        r.client_close(rrd, code);
        assert_eq!(
            r.take(rid),
            vec![Out::Close(code)],
            "not reflected to the RID"
        );
    }
    w.violations().assert_empty();
}

/// A side that goes away with any other code gives the other `4004`; a message after the end is
/// the client's breach.
#[test]
fn a_side_that_goes_away_gives_the_other_4004() {
    let w = world(403);
    let r = w.relay();
    let (rid, rrd) = pair(&w, 0x41);
    r.client_close(rrd, 1001);
    assert_eq!(r.take(rid), vec![Out::Close(4004)], "the RID was not told");
    let (rid, rrd) = pair(&w, 0x42);
    r.client_close(rid, 4000);
    assert_eq!(r.take(rrd), vec![Out::Close(4004)], "the RRD was not told");
    assert_eq!(r.closed(rid), Some(4000), "the RID's own code");
    r.receive(rid, vec![7]);
    assert_eq!(
        w.violations().rules(),
        vec!["rendezvous-frame-after-close"],
        "the RID's message after its close"
    );
}

/// A second RID gets `4000` (recorded); a second RRD and a third side after one left get `4000`.
#[test]
fn a_second_rid_and_a_third_side_get_4000() {
    let w = world(404);
    let r = w.relay();
    let (rid, rrd) = pair(&w, 0x51);
    let again = r.connect(&url(0x51), Role::Rid);
    assert_eq!(
        r.take(again),
        vec![Out::Close(4000)],
        "a second RID was let in"
    );
    let rrd2 = r.connect(&url(0x51), Role::Rrd);
    assert_eq!(
        r.take(rrd2),
        vec![Out::Close(4000)],
        "a second RRD was let in"
    );
    assert_eq!(r.peers(rid), (Some(rid), Some(rrd)), "the path's two sides");
    r.client_close(rrd, 1000);
    assert_eq!(r.take(rid), vec![Out::Close(1000)]);
    // The path is gone with both sides: a new RID opens a new one.
    let fresh = r.connect(&url(0x51), Role::Rid);
    assert_eq!(r.take(fresh), vec![], "a new path was closed");
    assert_eq!(
        w.violations().rules(),
        vec!["rendezvous-second-rid"],
        "the second RID"
    );
}

/// A one-digit prefix8 (the form before P9/P11), a short path and upper-case hex are recorded with their
/// own rules; the connect is UPGRADED and the side closed with `4000` at once, as a real relay closes
/// an invalid path (P10), instead of the connect being refused.
#[test]
fn a_one_digit_prefix8_and_a_wrong_path_are_closed_4000_after_the_upgrade() {
    let w = world(405);
    let r = w.relay();
    let good = url(0x0a);
    assert!(
        good.contains("/0a/"),
        "the two-digit form of a path below 0x10"
    );
    let one_digit = good.replacen("/0a/", "/a/", 1);
    let short = good[..good.len() - 2].to_owned();
    let upper = good.replace("0a0102", "0A0102");
    for (n, bad) in [one_digit, short, upper].into_iter().enumerate() {
        let side = r.connect(&bad, Role::Rid);
        assert_eq!(
            r.take(side),
            vec![Out::Close(4000)],
            "bad URL {n}: not closed 4000 after the upgrade"
        );
        assert_eq!(r.closed(side), Some(4000), "bad URL {n}: the side's end");
        assert_eq!(
            r.peers(side),
            (None, None),
            "bad URL {n}: the side is on no path"
        );
    }
    assert_eq!(r.side_count(), 3, "a bad URL made no side");
    let fine = r.connect(&good, Role::Rid);
    assert_eq!(r.take(fine), vec![], "the good URL was closed");
    assert_eq!(
        w.violations().rules(),
        vec![
            "rendezvous-url-prefix8-one-digit",
            "rendezvous-url-path-length",
            "rendezvous-connect-refused"
        ],
        "the bad URLs' rules"
    );
}

/// A path no RRD joins is closed with `4003` at the init timeout (default 10 min, H3), not before;
/// the knob sets it lower; a joined path never times out.
#[test]
fn the_init_timeout_closes_a_lonely_path_with_4003_at_its_time() {
    let w = world(406);
    let r = w.relay();
    assert_eq!(
        DEFAULT_INIT_TIMEOUT,
        Duration::from_secs(600),
        "H3's default"
    );
    let lonely = r.connect(&url(0x61), Role::Rid);
    let (rid, _rrd) = pair(&w, 0x62);
    w.advance(DEFAULT_INIT_TIMEOUT - Duration::from_millis(1));
    assert_eq!(r.take(lonely), vec![], "closed before the timeout");
    w.advance(Duration::from_millis(1));
    assert_eq!(
        r.take(lonely),
        vec![Out::Close(4003)],
        "no 4003 at the timeout"
    );
    assert_eq!(r.take(rid), vec![], "a joined path timed out");
    r.knobs(|k| k.init_timeout = Duration::from_secs(30));
    let short = r.connect(&url(0x63), Role::Rid);
    w.advance(Duration::from_secs(29));
    assert_eq!(r.take(short), vec![], "closed before the knob's time");
    w.advance(Duration::from_secs(1));
    assert_eq!(
        r.take(short),
        vec![Out::Close(4003)],
        "no 4003 at the knob's time"
    );
    w.violations().assert_empty();
}
