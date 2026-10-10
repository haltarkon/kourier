//! A measurement, not a routine test (docs/architecture.md §1.3 "Idle CPU": no periodic
//! wakeups besides the chat keepalive and timers that have work): how often a linked, connected
//! and idle Signal account wakes a thread, on the mock server with the real 30 s keepalive.
//! `cargo xtask perf` measures the app on the fake network, which has no connection to keep
//! alive; this is the Signal part of that number.
//!
//! Run it by hand (it idles for 100 seconds):
//!
//! ```text
//! KOURIER_REQUIRE_MOCK_HARNESS=1 cargo test -p kourier-e2e --test signal_idle -- \
//!     --ignored --nocapture
//! ```
//!
//! It prints the voluntary context switches of every thread of the test process over the
//! idle time (the core, the Signal account actor, the network runtime and the harness driver
//! share the process; the mock server is another process and is not counted) and fails when
//! the account's own threads wake more often than once in two seconds, which a polling loop
//! or a ticker left running would do.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use support::{App, World};

/// How long the account idles while it is measured: three keepalives (30 s apart).
const IDLE: Duration = Duration::from_secs(90);
/// How long it settles after the link first (post-link jobs, profile fetches).
const SETTLE: Duration = Duration::from_secs(10);

/// Voluntary context switches by thread id, with the thread's name.
fn wakeups() -> BTreeMap<u32, (String, u64)> {
    let mut threads = BTreeMap::new();
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return threads;
    };
    for task in tasks.flatten() {
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let name = std::fs::read_to_string(task.path().join("comm"))
            .map(|n| n.trim().to_owned())
            .unwrap_or_default();
        let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
        let switches = status
            .lines()
            .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|n| n.trim().parse::<u64>().ok())
            .unwrap_or(0);
        threads.insert(tid, (name, switches));
    }
    threads
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement that idles for 100 s; run it by hand (see the file's doc)"]
async fn an_idle_account_wakes_for_its_keepalive_and_little_else() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut app = App::start(world.tmp.path(), &world.env).await;
    let (account, _device) = app.link(&world).await;
    app.wait_online(account).await;
    tokio::time::sleep(SETTLE).await;

    let before = wakeups();
    // The measuring thread itself sleeps on the OS, not on the runtime's timer, so that it
    // adds one wakeup and no timer traffic.
    tokio::task::spawn_blocking(|| std::thread::sleep(IDLE))
        .await
        .expect("idle");
    let after = wakeups();

    let mut by_name: BTreeMap<String, (u32, u64)> = BTreeMap::new();
    for (tid, (name, switches)) in &after {
        let earlier = before.get(tid).map_or(0, |(_, n)| *n);
        let entry = by_name.entry(name.clone()).or_default();
        entry.0 += 1;
        entry.1 += switches.saturating_sub(earlier);
    }
    let secs = IDLE.as_secs_f64();
    let total: u64 = by_name.values().map(|(_, n)| *n).sum();
    println!("idle Signal account on the mock, {secs:.0} s, keepalive every 30 s:");
    println!(
        "{:<24} {:>7} {:>9} {:>10}",
        "thread", "threads", "wakeups", "per second"
    );
    for (name, (threads, switches)) in &by_name {
        println!(
            "{name:<24} {threads:>7} {switches:>9} {:>10.3}",
            *switches as f64 / secs
        );
    }
    println!(
        "{:<24} {:>7} {total:>9} {:>10.3}",
        "all threads",
        after.len(),
        total as f64 / secs
    );
    // The account actor's thread is named `signal-<uuid>`: its own share is the keepalive
    // (send, answer) and the job queue's timers.
    let actor: u64 = by_name
        .iter()
        .filter(|(name, _)| name.starts_with("signal-"))
        .map(|(_, (_, n))| *n)
        .sum();
    println!("account actor: {:.3} wakeups/s", actor as f64 / secs);
    assert!(
        actor as f64 / secs <= 0.5,
        "the account actor woke {actor} times in {secs:.0} s: something polls"
    );
    assert!(
        total as f64 / secs <= 2.0,
        "the process woke {total} times in {secs:.0} s: something polls"
    );
    app.stop().await;
    world.shutdown().await;
}
