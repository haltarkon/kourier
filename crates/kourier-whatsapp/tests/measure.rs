//! W3a (chunk C11): the MEASUREMENT of a drain through the receive path. A measurement, not a test:
//! the parent is `#[ignore]` and prints; run it by hand, once, on a quiet machine:
//!
//! ```text
//! cargo nextest run --workspace --exclude kourier-qt -E 'test(=measure::a_drain_of_400_beside_the_shared_runtime)' \
//!     --run-ignored only --no-capture
//! ```
//!
//! One account over the scripted server, in an offline child (`unshare -rn`), takes in the order
//! below with a 1 ms ticker running on the test's runtime, which is the stand-in for the shared
//! `net_rt` that Signal's connections live on (the account's client runs on its own two-worker
//! runtime, decision D1):
//!  1. 2 s of nothing: the ticker's own lateness is the baseline;
//!  2. a burst of 400 `<receipt>` stanzas (the handler's durable kind): every one waits for its
//!     COMMIT before upstream acknowledges it. Printed: stanzas per second until the 400th `<ack>`
//!     left, how many group commits wrote them and how big the groups were (a waiter is a parked
//!     worker of the client runtime, so the largest group is the most workers parked at once; two
//!     exist), and the ticker's lateness during the burst;
//!  3. an offline drain of 400 encrypted messages and `<ib><offline count="400"/>`: messages per
//!     second until every row is in the inbox (the hook's commit, durable), until stage 2 has
//!     turned every row into an event (`done`), and until the batched delivery receipt for all of
//!     them left; the ticker's lateness again.
//!
//! What is NOT in the numbers: the core's consumption of the envelopes (nothing acknowledges them
//! here) and the Wire's own work, which shares the test runtime and the CPU (the server side
//! encrypts and decrypts every frame in the same process). The profile is the test profile
//! (dependencies optimised, Kourier's own crates not), the file is on this machine's SSD with
//! `synchronous = FULL`-class durability of kourier-db.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{Command, Exit, StartMode};
use kourier_db::rusqlite::Connection;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

use crate::receive::{account_dir, peer_receipt};
use crate::support::peer::{self, Peer};
use crate::support::wire::{self, Wire};
use crate::support::{self, db_path, key};
use crate::wire::start_actor;

/// Messages and receipts of the drain.
const N: usize = 400;

/// Collects the `rows` of the handler's group-commit line (`events.rs`, debug level) and nothing
/// else: every other callsite is refused at registration, so the cost beside the drain is the
/// same as with no subscriber for all but that one line.
#[derive(Clone, Default)]
struct Groups {
    sizes: Arc<Mutex<Vec<u64>>>,
}

struct Rows(Option<u64>);

impl Visit for Rows {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "rows" {
            self.0 = Some(value);
        }
    }

    fn record_debug(&mut self, _: &Field, _: &dyn Debug) {}
}

impl Subscriber for Groups {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.is_event()
            && metadata.target() == "kourier_whatsapp::events"
            && *metadata.level() <= Level::DEBUG
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut rows = Rows(None);
        event.record(&mut rows);
        if let Some(rows) = rows.0 {
            self.sizes.lock().expect("lock").push(rows);
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// The ticker: sleeps 1 ms in a loop and notes how much later than that it woke, with the time of
/// the sleep's start.
async fn tick(stop: Arc<AtomicBool>, lags: Arc<Mutex<Vec<(Instant, u64)>>>) {
    while !stop.load(Ordering::Relaxed) {
        let before = Instant::now();
        tokio::time::sleep(Duration::from_millis(1)).await;
        let late = before.elapsed().saturating_sub(Duration::from_millis(1));
        lags.lock()
            .expect("lock")
            .push((before, u64::try_from(late.as_micros()).unwrap_or(u64::MAX)));
    }
}

/// `n ticks p50 p99 max` (microseconds late) of the ticks that began in `[from, to)`.
fn lateness(lags: &Mutex<Vec<(Instant, u64)>>, from: Instant, to: Instant) -> String {
    let mut late: Vec<u64> = lags
        .lock()
        .expect("lock")
        .iter()
        .filter(|(at, _)| *at >= from && *at < to)
        .map(|(_, late)| *late)
        .collect();
    late.sort_unstable();
    if late.is_empty() {
        return "no ticks".to_owned();
    }
    let at = |p: usize| late[(late.len() * p / 100).min(late.len() - 1)];
    format!(
        "ticks={} p50={}us p99={}us max={}us",
        late.len(),
        at(50),
        at(99),
        late[late.len() - 1]
    )
}

/// The `<ack>` frames the clients have sent.
fn acks(server: &Wire) -> usize {
    server.frames().iter().filter(|f| f.tag == "ack").count()
}

/// The messages the clients' `<receipt>` frames answer (a batched one names the first in its `id`
/// and the others as items).
fn receipted(server: &Wire) -> usize {
    server
        .frames()
        .iter()
        .filter(|f| f.tag == "receipt")
        .map(|f| 1 + f.items.len())
        .sum()
}

fn count(conn: &Connection, sql: &str) -> usize {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
        .map(|n| usize::try_from(n).unwrap_or(0))
        .expect(sql)
}

/// Polls `reached` every 5 ms and returns when it first held; panics after 40 s.
async fn until(what: &str, mut reached: impl FnMut() -> bool) -> Instant {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if reached() {
            return Instant::now();
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn per_second(n: usize, took: Duration) -> f64 {
    n as f64 / took.as_secs_f64()
}

/// The parent: prints what the child measured (nothing is asserted on the numbers).
#[test]
#[ignore = "a measurement, run by hand: see the module doc"]
fn a_drain_of_400_beside_the_shared_runtime() {
    let out = support::offline::run_child_output(module_path!(), "measure_child", &[]);
    for line in out.lines().filter(|l| l.starts_with("MEASURE")) {
        println!("{line}");
    }
}

/// The child (a no-op as an ordinary test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn measure_child() {
    if !support::offline::in_child() {
        return;
    }
    let (dir, device) = account_dir().await;
    let groups = Groups::default();
    tracing::subscriber::set_global_default(groups.clone()).expect("the child's subscriber");

    let server = wire::server();
    server.answering();
    server.hold_offline_end();
    let actor = start_actor(dir.path(), StartMode::Resume, &server);
    actor.send(Command::Connect).await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed"
    );
    server.send(&wire::success(peer::ACCOUNT_LID));

    let stop = Arc::new(AtomicBool::new(false));
    let lags = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(tick(stop.clone(), lags.clone()));
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle_end = Instant::now();

    // The handler's kind: receipts.
    let ids: Vec<String> = (0..N)
        .map(|i| format!("3EB0{:012X}", 0xA0_0000_0000_u64 + i as u64))
        .collect();
    let burst: Vec<_> = ids.iter().map(|id| peer_receipt(id, "read")).collect();
    let t0 = Instant::now();
    for node in &burst {
        server.send(node);
    }
    let t_acked = until("the 400th ack", || acks(&server) >= N).await;
    let receipts_took = t_acked - t0;
    let sizes = std::mem::take(&mut *groups.sizes.lock().expect("lock"));
    let commits = sizes.len();
    let rows: u64 = sizes.iter().sum();
    let biggest = sizes.iter().copied().max().unwrap_or(0);
    eprintln!(
        "MEASURE receipts n={N} took={}ms per_s={:.0} group_commits={commits} rows={rows} \
         rows_per_commit={:.2} largest_group={biggest} (parked workers at most {biggest} of 2) \
         per_1000={:.1}s",
        receipts_took.as_millis(),
        per_second(N, receipts_took),
        rows as f64 / commits.max(1) as f64,
        receipts_took.as_secs_f64() * 1000.0 / N as f64,
    );
    eprintln!("MEASURE ticker idle {}", lateness(&lags, started, idle_end));
    eprintln!(
        "MEASURE ticker during receipts {}",
        lateness(&lags, t0, t_acked)
    );

    // The hook's kind: an offline drain of messages.
    let mut peer = Peer::new(&device, None).await;
    let mut stanzas = Vec::new();
    for i in 0..N {
        stanzas.push(peer.offline_text(&format!("synthetic {i}")).await);
    }
    let raw = kourier_db::open(&db_path(dir.path()), &key()).expect("raw open");
    let t1 = Instant::now();
    for node in &stanzas {
        server.send(node);
    }
    server.send(&wire::offline_end(u32::try_from(N).expect("small")));
    let t_inbox = until("400 inbox rows", || {
        count(&raw, "SELECT count(*) FROM inbox") >= N
    })
    .await;
    let t_done = until("400 done rows", || {
        count(&raw, "SELECT count(*) FROM inbox WHERE state = 'done'") >= N
    })
    .await;
    eprintln!(
        "MEASURE messages n={N} inbox_durable={}ms ({:.0}/s) stage2_done={}ms ({:.0}/s)",
        (t_inbox - t1).as_millis(),
        per_second(N, t_inbox - t1),
        (t_done - t1).as_millis(),
        per_second(N, t_done - t1),
    );
    let t_receipt = until("the receipts of the 400", || receipted(&server) >= N).await;
    eprintln!(
        "MEASURE messages all_receipted={}ms ({:.0}/s)",
        (t_receipt - t1).as_millis(),
        per_second(N, t_receipt - t1),
    );
    eprintln!(
        "MEASURE ticker during the message drain {}",
        lateness(&lags, t1, t_receipt)
    );
    stop.store(true, Ordering::Relaxed);
    drop(raw);
    let mut actor = actor;
    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}
