//! Resume and network hints (§4.7) against mock logind and NetworkManager on a private bus:
//! which signals become hints, that a missing service is quiet and is picked up when it
//! appears, that only the owner of the service name counts, that a bus that goes away and
//! comes back is listened to again, and that a bus that is not there at the start is waited
//! for.

use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use kourier_desktop::Bus;
use kourier_desktop::hints::{Hint, SystemHints};
use kourier_desktop::testing::{MockSystem, PrivateBus};

/// Long enough for a signal to cross the bus, short enough to wait for "nothing came".
const QUIET: Duration = Duration::from_millis(400);
const SOON: Duration = Duration::from_secs(10);

async fn watch(bus: &PrivateBus) -> (SystemHints, Receiver<Hint>) {
    let (tx, rx) = channel();
    let conn = bus.connect().await.expect("connect");
    let hints = SystemHints::watch(&conn, move |hint| {
        let _ = tx.send(hint);
    })
    .await
    .expect("watch");
    (hints, rx)
}

fn next(rx: &Receiver<Hint>) -> Hint {
    rx.recv_timeout(SOON).expect("a hint")
}

fn nothing(rx: &Receiver<Hint>) {
    assert_eq!(rx.recv_timeout(QUIET), Err(RecvTimeoutError::Timeout));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_and_network_changes_become_hints() {
    let bus = PrivateBus::start().expect("bus");
    let system = MockSystem::start(bus.address()).await.expect("mock");
    let (_hints, rx) = watch(&bus).await;

    // Going to sleep is no hint; waking up is.
    system.prepare_for_sleep(true).await.expect("emit");
    nothing(&rx);
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);

    // Every state change of NetworkManager is one (the core collects a burst).
    system.network_state(20).await.expect("emit");
    system.network_state(70).await.expect("emit");
    assert_eq!(next(&rx), Hint::NetworkChanged);
    assert_eq!(next(&rx), Hint::NetworkChanged);

    // Another connection became the primary one (Wi-Fi to cable): the state stays
    // "connected", the route changed.
    system
        .property_changed("PrimaryConnection")
        .await
        .expect("emit");
    assert_eq!(next(&rx), Hint::NetworkChanged);
    // Other properties change all the time and say nothing about reachability.
    system
        .property_changed("WirelessEnabled")
        .await
        .expect("emit");
    nothing(&rx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_services_are_quiet_and_count_once_they_appear() {
    // No logind, no NetworkManager (no systemd, a container): watching works and stays quiet.
    let bus = PrivateBus::start().expect("bus");
    let (_hints, rx) = watch(&bus).await;
    nothing(&rx);

    // Somebody who does not own the names cannot fake a resume or a network change.
    let impostor = MockSystem::impostor(bus.address()).await.expect("impostor");
    impostor.prepare_for_sleep(false).await.expect("emit");
    impostor.network_state(70).await.expect("emit");
    impostor
        .property_changed("PrimaryConnection")
        .await
        .expect("emit");
    nothing(&rx);

    // The services start later (NetworkManager restarts): their signals arrive without the
    // watcher doing anything.
    let system = MockSystem::start(bus.address()).await.expect("mock");
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);
    system.network_state(70).await.expect("emit");
    assert_eq!(next(&rx), Hint::NetworkChanged);
    // The impostor still does not count.
    impostor.prepare_for_sleep(false).await.expect("emit");
    nothing(&rx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_watcher_stops_and_no_bus_is_an_error_not_a_loop() {
    let bus = PrivateBus::start().expect("bus");
    let system = MockSystem::start(bus.address()).await.expect("mock");
    let (hints, rx) = watch(&bus).await;
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);
    drop(hints);
    // (The abort takes effect at the task's next await.)
    tokio::time::sleep(Duration::from_millis(100)).await;
    system.prepare_for_sleep(false).await.expect("emit");
    assert!(rx.recv_timeout(QUIET).is_err(), "nothing after the drop");

    // No bus at the address: one error for the caller, and `watch` itself never retries.
    let gone = Bus::at("unix:path=/nonexistent/kourier-no-bus").await;
    assert!(gone.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bus_that_is_not_there_at_the_start_is_waited_for() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    // A machine whose system bus is not up when Kourier starts (its daemon is restarting,
    // or comes later): nothing is at the address yet.
    let address = Arc::new(Mutex::new(String::from(
        "unix:path=/nonexistent/kourier-no-bus-yet",
    )));
    let tries = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = channel();
    let connect = {
        let (address, tries) = (address.clone(), tries.clone());
        move || {
            tries.fetch_add(1, Ordering::SeqCst);
            let at = address.lock().expect("address").clone();
            async move { Bus::at(&at).await }
        }
    };
    let hints = SystemHints::watch_reconnecting(connect, move |hint| {
        let _ = tx.send(hint);
    })
    .await;
    // It says so (the caller notes it once) and is not an error: the watcher lives.
    assert!(!hints.connected_at_start());
    assert_eq!(tries.load(Ordering::SeqCst), 1);
    // It tries again by itself, and reports nothing while there is nothing.
    let deadline = std::time::Instant::now() + SOON;
    while tries.load(Ordering::SeqCst) < 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "it goes on trying to connect"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    nothing(&rx);

    // The bus appears: one hint (something may have been missed), and signals count.
    let bus = PrivateBus::start().expect("bus");
    *address.lock().expect("address") = bus.address().to_owned();
    assert_eq!(
        next(&rx),
        Hint::NetworkChanged,
        "there now: check the connection"
    );
    let system = MockSystem::start(bus.address()).await.expect("mock");
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);
    // Nothing is tried while the bus is there.
    let after = tries.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(tries.load(Ordering::SeqCst), after);
    nothing(&rx);

    // A dropped watcher that is still waiting for a bus stops trying.
    let tries = Arc::new(AtomicUsize::new(0));
    let counting = tries.clone();
    let waiting = SystemHints::watch_reconnecting(
        move || {
            counting.fetch_add(1, Ordering::SeqCst);
            async { Bus::at("unix:path=/nonexistent/kourier-never").await }
        },
        |_| {},
    )
    .await;
    drop(waiting);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = tries.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        tries.load(Ordering::SeqCst),
        after,
        "no tries after the drop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bus_that_comes_back_is_listened_to_again() {
    use std::sync::{Arc, Mutex};
    // Where "the system bus" is. A restarted bus daemon is a new process at the same
    // well-known address; here a second private bus takes the first one's place.
    let first = PrivateBus::start().expect("bus");
    let address = Arc::new(Mutex::new(first.address().to_owned()));
    let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tx, rx) = channel();
    let connect = {
        let (address, tries) = (address.clone(), tries.clone());
        move || {
            tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let at = address.lock().expect("address").clone();
            async move { Bus::at(&at).await }
        }
    };
    let hints = SystemHints::watch_reconnecting(connect, move |hint| {
        let _ = tx.send(hint);
    })
    .await;
    assert!(hints.connected_at_start());
    let system = MockSystem::start(first.address()).await.expect("mock");
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);
    assert_eq!(
        tries.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "nothing is tried while the bus is there"
    );

    // The bus goes away. Nothing can be heard, and nothing is reported yet.
    drop(system);
    drop(first);
    nothing(&rx);
    let deadline = std::time::Instant::now() + SOON;
    while tries.load(std::sync::atomic::Ordering::SeqCst) < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "it tries to connect again"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    nothing(&rx);

    // It comes back (with its services): one hint says that something may have been missed,
    // and signals count again.
    let second = PrivateBus::start().expect("bus");
    *address.lock().expect("address") = second.address().to_owned();
    assert_eq!(
        next(&rx),
        Hint::NetworkChanged,
        "back: check the connection"
    );
    let system = MockSystem::start(second.address()).await.expect("mock");
    system.prepare_for_sleep(false).await.expect("emit");
    assert_eq!(next(&rx), Hint::Resumed);
    system.network_state(70).await.expect("emit");
    assert_eq!(next(&rx), Hint::NetworkChanged);
    let after = tries.load(std::sync::atomic::Ordering::SeqCst);
    // (Longer than the longest wait between two tries so far.)
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        tries.load(std::sync::atomic::Ordering::SeqCst),
        after,
        "connected again: the retries stop"
    );

    // (A bus that is not there from the start: `a_bus_that_is_not_there_at_the_start_is_waited_for`.)
}
