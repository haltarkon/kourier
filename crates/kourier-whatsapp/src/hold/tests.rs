use std::sync::Arc;
use std::time::{Duration, Instant};

use super::LinkHold;

#[test]
fn an_open_hold_lets_a_waiter_through_at_once() {
    let hold = LinkHold::default();
    assert!(!hold.is_closed());
    let started = Instant::now();
    assert!(hold.wait_open(Duration::from_secs(30)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_closed_hold_gives_up_when_the_limit_passes() {
    let hold = LinkHold::default();
    hold.set_closed(true);
    assert!(hold.is_closed());
    assert!(!hold.wait_open(Duration::from_millis(30)));
    assert_eq!(hold.parked(), 0, "a waiter that left is not counted");
}

#[test]
fn opening_releases_a_parked_waiter() {
    let hold = Arc::new(LinkHold::default());
    hold.set_closed(true);
    let waiter = {
        let hold = hold.clone();
        std::thread::spawn(move || hold.wait_open(Duration::from_secs(30)))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while hold.parked() == 0 {
        assert!(Instant::now() < deadline, "the waiter never parked");
        std::thread::sleep(Duration::from_millis(2));
    }
    hold.set_closed(false);
    assert!(waiter.join().expect("the waiter ends"));
    assert_eq!(hold.parked(), 0);
}
