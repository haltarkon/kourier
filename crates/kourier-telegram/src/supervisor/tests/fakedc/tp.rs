//! TP (plan critic M7, decisions G-7, Q-L8, H6): what a Telegram account costs, measured. The REAL
//! actor (on the real clock) over the REAL helper and the test-only library on the fake DC, as the
//! app runs them but for the core: a task that acks every envelope. The helper's socket goes
//! straight to the actor (no counting relay: its threads would count as the app's), and the helper
//! stays dumpable, so its `smaps_rollup` can be read.
//!
//! Every case is `#[ignore]`d: `cargo xtask tdlib fakedc perf` runs them, one at a time. A wake-up is
//! a voluntary context switch, the unit of `xtask perf`'s idle rows. Only counts, fixed words,
//! thread names and durations are printed. What this cannot show: real servers (the fake sends
//! nothing unasked, so a real DC's traffic adds to the idle rows; R1 measures that) and the app's
//! own rows, because a release app has no Telegram yet (`own` here is a debug test binary).

use std::collections::BTreeMap;
use std::time::Instant;

use kourier_backend_api::{
    AccountContext, BackendEvent, Command, ContextParams, CoreEnds, Envelope, Exit, LocalPrefsView,
    StartMode, SystemClock, UiPresence,
};
use kourier_model::{AccountId, ConnState};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::{mpsc, watch};

use super::*;

/// The longest a case may take (nextest ends a test after 300 s).
const BOUND: Duration = Duration::from_secs(290);
/// What follows `Online` before anything is measured: TDLib's own work after a start.
const SETTLE: Duration = Duration::from_secs(20);
/// One idle window.
const WINDOW: Duration = Duration::from_secs(120);
/// The chats a growth or a burst pushes into, in turn (a private chat, a group, a supergroup).
const CHATS: [&str; 4] = ["u201", "u202", "g301", "c401"];
/// What one `push-many` holds: the control socket's answer must come within its 10 s.
const PUSHES_PER_LINE: usize = 500;
/// The start sync, and a restart's: the lane's real pace (one fetch a second), on a loaded machine.
const ONLINE_BOUND: Duration = Duration::from_secs(250);

// ----- what a process costs -----

/// A process's memory at one instant, in KiB.
struct Memory {
    rss: u64,
    anon: u64,
    file: u64,
    /// `None`: `smaps_rollup` was not readable.
    pss: Option<u64>,
    threads: u64,
}

fn status_field(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn memory_of(pid: u32) -> Memory {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let pss = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .map(|t| status_field(&t, "Pss"));
    Memory {
        rss: status_field(&status, "VmRSS"),
        anon: status_field(&status, "RssAnon"),
        file: status_field(&status, "RssFile"),
        pss,
        threads: status_field(&status, "Threads"),
    }
}

impl Memory {
    fn words(&self) -> String {
        format!(
            "rss_kb={} anon_kb={} file_kb={} pss_kb={} threads={}",
            self.rss,
            self.anon,
            self.file,
            self.pss.map_or("unread".to_owned(), |p| p.to_string()),
            self.threads
        )
    }
}

/// One thread at one instant.
struct Thread {
    name: String,
    voluntary: u64,
    involuntary: u64,
    ticks: u64,
}

fn threads_of(pid: u32) -> BTreeMap<String, Thread> {
    let mut out = BTreeMap::new();
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return out;
    };
    for task in tasks.flatten() {
        let path = task.path();
        let Ok(status) = std::fs::read_to_string(path.join("status")) else {
            continue;
        };
        let name = status
            .lines()
            .find_map(|l| l.strip_prefix("Name:"))
            .map_or("?", str::trim)
            .to_owned();
        // After the name, which may hold spaces: state (3), ...; utime is field 14, stime 15.
        let stat = std::fs::read_to_string(path.join("stat")).unwrap_or_default();
        let rest = stat.rfind(')').map_or("", |at| &stat[at + 1..]);
        let mut fields = rest.split_whitespace();
        let utime: u64 = fields.nth(11).and_then(|f| f.parse().ok()).unwrap_or(0);
        let stime: u64 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
        out.insert(
            task.file_name().to_string_lossy().into_owned(),
            Thread {
                name,
                voluntary: status_field(&status, "voluntary_ctxt_switches"),
                involuntary: status_field(&status, "nonvoluntary_ctxt_switches"),
                ticks: utime + stime,
            },
        );
    }
    out
}

/// What a process did between two samples.
fn activity_words(
    before: &BTreeMap<String, Thread>,
    after: &BTreeMap<String, Thread>,
    took: Duration,
) -> String {
    // SAFETY: sysconf reads a constant of the system.
    let ticks_per_s = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
        .unwrap_or(100)
        .max(1);
    let (mut wakeups, mut involuntary, mut ticks) = (0, 0, 0);
    let mut by_name: BTreeMap<&str, u64> = BTreeMap::new();
    for (tid, t) in after {
        let earlier = before.get(tid);
        let woke = t.voluntary - earlier.map_or(0, |e| e.voluntary);
        wakeups += woke;
        involuntary += t.involuntary - earlier.map_or(0, |e| e.involuntary);
        ticks += t.ticks - earlier.map_or(0, |e| e.ticks);
        *by_name.entry(&t.name).or_default() += woke;
    }
    let mut busiest: Vec<(&str, u64)> = by_name.into_iter().filter(|(_, n)| *n > 0).collect();
    busiest.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let busiest: Vec<String> = busiest.iter().map(|(a, n)| format!("{a}:{n}")).collect();
    let secs = took.as_secs_f64();
    format!(
        "wakeups_per_s={:.2} involuntary_per_s={:.2} cpu_ms_per_s={:.2} by_thread={}",
        wakeups as f64 / secs,
        involuntary as f64 / secs,
        ticks as f64 * 1000.0 / ticks_per_s as f64 / secs,
        busiest.join(",")
    )
}

/// The machine's one-minute load (the lanes beside this run are named by it in the report).
fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| t.split_whitespace().next().map(str::to_owned))
        .unwrap_or_default()
}

// ----- the dialers -----

/// [`RemapDialer`] behind a one-way delay in each direction (a latency, no bandwidth limit). Test
/// code, offline: the remap below it refuses outside the namespace.
struct DelayedDialer {
    remap: RemapDialer,
    one_way: Duration,
}

impl Dialer for DelayedDialer {
    fn dial(&self, to: SocketAddr) -> DialFuture {
        let far = self.remap.dial(to);
        let one_way = self.one_way;
        Box::pin(async move {
            let far = far.await?;
            let (near, there) = tokio::io::duplex(1 << 20);
            let (far_read, far_write) = tokio::io::split(far);
            let (there_read, there_write) = tokio::io::split(there);
            tokio::spawn(delayed(far_read, there_write, one_way));
            tokio::spawn(delayed(there_read, far_write, one_way));
            Ok(Box::new(near) as Box<dyn GateStream>)
        })
    }
}

async fn delayed<R, W>(mut from: R, mut to: W, one_way: Duration)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (due, mut coming) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
    let writer = tokio::spawn(async move {
        while let Some((at, bytes)) = coming.recv().await {
            tokio::time::sleep_until(at).await;
            if to.write_all(&bytes).await.is_err() {
                return;
            }
        }
        let _ = to.shutdown().await;
    });
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(n) = from.read(&mut buffer).await {
        if n == 0
            || due
                .send((tokio::time::Instant::now() + one_way, buffer[..n].to_vec()))
                .is_err()
        {
            break;
        }
    }
    drop(due);
    let _ = writer.await;
}

fn dialer(fakedc: SocketAddr, dials: Arc<AtomicUsize>, one_way: Duration) -> Arc<dyn Dialer> {
    let remap = RemapDialer { fakedc, dials };
    if one_way.is_zero() {
        Arc::new(remap)
    } else {
        Arc::new(DelayedDialer { remap, one_way })
    }
}

// ----- one account -----

/// See the module's text: the app's wiring, the helper dumpable.
fn tp_parts(dialer: Arc<dyn Dialer>) -> (TgParts, Pids) {
    let (helper, tdjson) = (helper_binary(), var(LIB_ENV).expect("the library"));
    let pids: Pids = Arc::default();
    let started = pids.clone();
    let parts = TgParts::from_makers(
        move |args| {
            let spawned = spawn(&SpawnArgs {
                helper: helper.clone(),
                tdjson: tdjson.clone(),
                no_dump: false,
                gate_port: args.gate_port,
                database_dir: args.database_dir.clone(),
                files_dir: args.files_dir.clone(),
            })?;
            started.lock().expect("pids").push(spawned.child.id());
            Ok(HelperProcess {
                sock: spawned.sock,
                stderr: spawned.stderr.map(|e| Box::new(e) as Box<dyn Read + Send>),
                child: Box::new(spawned.child),
            })
        },
        dialer,
    )
    .requiring_lockdown(LockdownNeed::OnOrUnavailable)
    .expecting_pin(&FAKEDC_PIN)
    .with_env(TgEnv::Test);
    (parts, pids)
}

fn actor_on_the_real_clock(
    parts: TgParts,
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    (start, uuid): (StartMode, AccountUuid),
) -> (JoinHandle<Exit>, CoreEnds) {
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start,
        backend_db: dir.join("backend.db"),
        db_key: kourier_backend_api::DbKey::new([7; 32]),
        blob_dir: dir.join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let parts = parts
        .with_api(ApiCredentials::DUMMY)
        .with_files_root(dir.join("media"));
    let timings = real_timings();
    let actor = std::thread::spawn(move || crate::actor::run_with(ctx, parts, timings));
    (actor, ends)
}

/// What the core's side saw.
#[derive(Default)]
struct Seen {
    /// When each live message reached the core.
    live: Vec<Instant>,
    envelopes: usize,
}

/// The core's side: every envelope acked at once. It waits on the channel alone, so it adds no
/// wake-up to an idle window (the tests' 50 ms pump would add twenty a second).
async fn core_side(mut events: mpsc::Receiver<Envelope>, seen: Arc<Mutex<Seen>>) {
    while let Some(envelope) = events.recv().await {
        let now = Instant::now();
        {
            let mut seen = seen.lock().expect("seen");
            seen.envelopes += 1;
            let lives = envelope
                .events
                .iter()
                .filter(|e| matches!(e, BackendEvent::Message(_)));
            seen.live.extend(lives.map(|_| now));
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
    }
}

/// The parts of [`CoreEnds`] a measurement keeps once [`core_side`] has the events.
struct Ends {
    commands: mpsc::Sender<Command>,
    conn: watch::Receiver<ConnState>,
    ui: watch::Sender<UiPresence>,
    _prefs: watch::Sender<LocalPrefsView>,
}

fn split_ends(ends: CoreEnds, rt: &tokio::runtime::Runtime, seen: &Arc<Mutex<Seen>>) -> Ends {
    rt.spawn(core_side(ends.events, seen.clone()));
    Ends {
        commands: ends.commands,
        conn: ends.conn,
        ui: ends.ui,
        _prefs: ends.local_prefs,
    }
}

/// What a case that failed saw: the connection state and the requests the fake DC got, by name.
fn stuck_words(fake: &FakeDc, conn: &watch::Receiver<ConnState>, seen: &Mutex<Seen>) -> String {
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    for (dir, name) in fake.lines() {
        if dir == "in" {
            *by_name.entry(name).or_default() += 1;
        }
    }
    let seen = seen.lock().expect("seen");
    format!(
        "conn={} envelopes={} live={} requests={by_name:?}",
        conn_name(&conn.borrow()),
        seen.envelopes,
        seen.live.len()
    )
}

fn wait_online(rt: &tokio::runtime::Runtime, conn: &mut watch::Receiver<ConnState>) -> bool {
    rt.block_on(async {
        tokio::time::timeout(
            ONLINE_BOUND,
            conn.wait_for(|c| matches!(c, ConnState::Online { .. })),
        )
        .await
        .is_ok_and(|seen| seen.is_ok())
    })
}

/// One account: its directories, its fake DC, its actor and helper.
struct Run {
    dir: tempfile::TempDir,
    fake: FakeDc,
    dials: Arc<AtomicUsize>,
    one_way: Duration,
    uuid: AccountUuid,
    pids: Pids,
    actor: JoinHandle<Exit>,
    ends: Ends,
    seen: Arc<Mutex<Seen>>,
    /// From the start to `done`, and to `Online`.
    login_ms: u128,
    online_ms: u128,
}

impl Run {
    /// A fresh link by code on `world`, up to `Online`.
    fn link(rt: &tokio::runtime::Runtime, world: &str, one_way: Duration) -> Run {
        let started = StdInstant::now();
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = start_fakedc(dir.path());
        fake.control(&format!("world {world}"));
        let dials = Arc::new(AtomicUsize::new(0));
        let (parts, pids) = tp_parts(dialer(fake.at, dials.clone(), one_way));
        let uuid = AccountUuid::new_random();
        let (actor, mut ends) =
            actor_on_the_real_clock(parts, rt, dir.path(), (StartMode::Fresh, uuid));
        let login = rt.block_on(login_by_code(&mut ends, started));
        let ids: Vec<&str> = login.steps.iter().map(|s| s.step_id.as_str()).collect();
        assert_eq!(ids, ["phone", "code", "linking", "done"], "not logged in");
        let login_ms = started.elapsed().as_millis();
        let seen = Arc::default();
        let mut ends = split_ends(ends, rt, &seen);
        let online = wait_online(rt, &mut ends.conn);
        assert!(
            online,
            "never Online: {}",
            stuck_words(&fake, &ends.conn, &seen)
        );
        Run {
            dir,
            fake,
            dials,
            one_way,
            uuid,
            pids,
            actor,
            ends,
            seen,
            login_ms,
            online_ms: started.elapsed().as_millis(),
        }
    }

    /// The running helper's process id.
    fn helper(&self) -> u32 {
        *self.pids.lock().expect("pids").last().expect("a helper")
    }

    fn live(&self) -> usize {
        self.seen.lock().expect("seen").live.len()
    }

    /// Ends the actor (its helper with it); the account stays linked on its directories.
    fn shut_down(&mut self, rt: &tokio::runtime::Runtime) {
        rt.block_on(self.ends.commands.send(Command::Shutdown))
            .expect("queued");
        // (the next start puts its own in; this one only holds the place)
        let ended = std::thread::spawn(|| Exit::Shutdown);
        let actor = std::mem::replace(&mut self.actor, ended);
        assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
    }

    /// A new actor and helper on the same directories, up to `Online`: what the app does at a start,
    /// and what a recycled helper would do. Returns the milliseconds that took.
    fn restart(&mut self, rt: &tokio::runtime::Runtime) -> u128 {
        let started = StdInstant::now();
        let (parts, pids) = tp_parts(dialer(self.fake.at, self.dials.clone(), self.one_way));
        self.pids = pids;
        let (actor, ends) =
            actor_on_the_real_clock(parts, rt, self.dir.path(), (StartMode::Resume, self.uuid));
        self.actor = actor;
        self.ends = split_ends(ends, rt, &self.seen);
        rt.block_on(self.ends.commands.send(Command::Connect))
            .expect("queued");
        let online = wait_online(rt, &mut self.ends.conn);
        assert!(
            online,
            "never Online after the restart: {}",
            stuck_words(&self.fake, &self.ends.conn, &self.seen)
        );
        started.elapsed().as_millis()
    }

    /// Pushes `count` messages into [`CHATS`] in turn and waits until the core has them all.
    fn push(&self, count: usize) -> Duration {
        let want = self.live() + count;
        let started = Instant::now();
        let mut left = count;
        for chat in CHATS.iter().cycle() {
            if left == 0 {
                break;
            }
            let n = left.min(PUSHES_PER_LINE);
            self.fake.control(&format!("push-many {n} new {chat}"));
            left -= n;
        }
        let by = Instant::now() + Duration::from_secs(150);
        while self.live() < want {
            assert!(
                Instant::now() < by,
                "the core did not get every message: {}",
                stuck_words(&self.fake, &self.ends.conn, &self.seen)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        started.elapsed()
    }

    /// The fake DC's requests after its first `from` wire-log lines, by name, most first.
    fn requests_since(&self, from: usize) -> String {
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for (dir, name) in self.fake.lines().into_iter().skip(from) {
            if dir == "in" {
                *by_name.entry(name).or_default() += 1;
            }
        }
        let mut names: Vec<(String, usize)> = by_name.into_iter().collect();
        names.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let names: Vec<String> = names.iter().map(|(a, n)| format!("{a}:{n}")).collect();
        names.join(",")
    }
}

fn end_of(name: &str, traps: &Traps, runs: Vec<Run>, rt: &tokio::runtime::Runtime) {
    let mut ended = Vec::new();
    for mut run in runs {
        run.shut_down(rt);
        ended.push(run);
    }
    std::thread::sleep(Duration::from_millis(500));
    for run in &ended {
        net_end(
            name,
            traps,
            &run.fake,
            run.dials.load(Ordering::SeqCst),
            None,
        );
    }
}

// ----- the cases -----

/// One idle window over `helpers` and this process: one line each.
fn idle_rows(name: &str, runs: &[&Run]) {
    let own = std::process::id();
    let helpers: Vec<u32> = runs.iter().map(|r| r.helper()).collect();
    let wire: Vec<usize> = runs.iter().map(|r| r.fake.lines().len()).collect();
    let before: Vec<_> = helpers.iter().map(|&p| threads_of(p)).collect();
    let own_before = threads_of(own);
    let at = Instant::now();
    let load_before = load();
    std::thread::sleep(WINDOW);
    let took = at.elapsed();
    let window = took.as_secs();
    let load_after = load();
    for (i, (&pid, before)) in helpers.iter().zip(&before).enumerate() {
        println!(
            "FAKEDC {name}: helper{i} window_s={window} load={load_before}..{load_after} {} {} wire_in={}",
            memory_of(pid).words(),
            activity_words(before, &threads_of(pid), took),
            runs[i].requests_since(wire[i]),
        );
    }
    println!(
        "FAKEDC {name}: own window_s={window} load={load_before}..{load_after} threads={} {}",
        memory_of(own).threads,
        activity_words(&own_before, &threads_of(own), took),
    );
}

/// The baseline: this process with a fake DC beside it and no account.
fn case_none(traps: &Traps) {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    let rt = rt();
    let _guard = rt.enter();
    idle_rows("tp-none", &[]);
    net_end("tp-none", traps, &fake, 0, None);
}

/// `accounts` accounts linked, unfocused (or, `focused`, the window with the focus: TDLib's `online`
/// option is on). The focused case ends with the focus leaving: what one change costs on the wire.
fn case_idle(name: &str, accounts: usize, focused: bool, traps: &Traps) {
    let rt = rt();
    let runs: Vec<Run> = (0..accounts)
        .map(|_| Run::link(&rt, "standard", Duration::ZERO))
        .collect();
    for (i, run) in runs.iter().enumerate() {
        println!(
            "FAKEDC {name}: account{i} login_ms={} online_ms={} at_online {}",
            run.login_ms,
            run.online_ms,
            memory_of(run.helper()).words()
        );
    }
    std::thread::sleep(SETTLE);
    // The account shows online only with its conversation open (`presence::online_of`).
    let presence = |focused: bool| UiPresence {
        focused,
        visible: focused.then(|| direct(201)),
        idle: false,
    };
    if focused {
        for run in &runs {
            let _ = run.ends.ui.send(presence(true));
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    let refs: Vec<&Run> = runs.iter().collect();
    idle_rows(name, &refs);
    if focused {
        let from = runs[0].fake.lines().len();
        let _ = runs[0].ends.ui.send(presence(false));
        // The focus leaves: the option goes off 5 s later (H6).
        std::thread::sleep(Duration::from_secs(8));
        println!(
            "FAKEDC {name}: focus_left wire_in={}",
            runs[0].requests_since(from)
        );
    }
    end_of(name, traps, runs, &rt);
}

/// G-7: what the helper holds as messages arrive (TDLib keeps them without its database), and what a
/// restart gives back: the number a recycling of the helper would be decided on.
fn case_growth(traps: &Traps) {
    const NAME: &str = "tp-growth";
    let rt = rt();
    let mut run = Run::link(&rt, "standard", Duration::ZERO);
    std::thread::sleep(SETTLE);
    let row = |run: &Run, at: &str| {
        println!(
            "FAKEDC {NAME}: {at} load={} helper {} own_rss_kb={}",
            load(),
            memory_of(run.helper()).words(),
            memory_of(std::process::id()).rss,
        );
    };
    row(&run, "messages=0");
    let mut total = 0;
    for step in [2_000, 8_000, 20_000] {
        let took = run.push(step);
        total += step;
        // What TDLib frees or still does right after a burst.
        std::thread::sleep(Duration::from_secs(5));
        row(
            &run,
            &format!("messages={total} took_ms={}", took.as_millis()),
        );
    }
    run.shut_down(&rt);
    let restart_ms = run.restart(&rt);
    std::thread::sleep(SETTLE);
    row(&run, &format!("restarted restart_ms={restart_ms}"));
    end_of(NAME, traps, vec![run], &rt);
}

/// `COMMIT_AFTER_MS` and `COMMIT_FRAMES`: a message alone (the queue is empty, so nothing should wait)
/// and a burst (the group commit's own case). Latency is from the fake DC's answer to the command,
/// which comes after the push was written, to the core's ack-side task.
fn case_burst(traps: &Traps) {
    const NAME: &str = "tp-burst";
    const ALONE: usize = 50;
    const BURST: usize = 2_000;
    let rt = rt();
    let run = Run::link(&rt, "standard", Duration::ZERO);
    std::thread::sleep(SETTLE);
    let mut latencies = Vec::new();
    for _ in 0..ALONE {
        let want = run.live() + 1;
        run.fake.control("push new u201");
        let at = Instant::now();
        let by = at + Duration::from_secs(20);
        while run.live() < want {
            assert!(
                Instant::now() < by,
                "a message did not reach the core: {}",
                stuck_words(&run.fake, &run.ends.conn, &run.seen)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        latencies.push(at.elapsed());
        std::thread::sleep(Duration::from_millis(300));
    }
    latencies.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "FAKEDC {NAME}: alone count={ALONE} load={} p50_ms={:.1} p95_ms={:.1} max_ms={:.1}",
        load(),
        ms(latencies[ALONE / 2]),
        ms(latencies[ALONE * 95 / 100]),
        ms(latencies[ALONE - 1]),
    );
    let envelopes = run.seen.lock().expect("seen").envelopes;
    let took = run.push(BURST);
    let envelopes = run.seen.lock().expect("seen").envelopes - envelopes;
    println!(
        "FAKEDC {NAME}: burst count={BURST} load={} took_ms={} per_s={:.0} envelopes={envelopes}",
        load(),
        took.as_millis(),
        BURST as f64 / took.as_secs_f64(),
    );
    end_of(NAME, traps, vec![run], &rt);
}

/// Q-L8: how long after the chat lists TDLib's channel differences run, against `QUIET_MS`. First the
/// fresh link; then a restart with a message waiting in every supergroup, the path the wait exists
/// for (a wrong wait only costs a duplicate fetch: the history requests are printed).
fn case_quiet(name: &str, world: &str, supergroups: &[String], one_way_ms: u64, traps: &Traps) {
    let rt = rt();
    let mut run = Run::link(&rt, world, Duration::from_millis(one_way_ms));
    println!(
        "FAKEDC {name}: account login_ms={} online_ms={} at_online {}",
        run.login_ms,
        run.online_ms,
        memory_of(run.helper()).words()
    );
    // A late difference must have the time to show.
    std::thread::sleep(SETTLE);
    let quiet = crate::sync::reconcile::QUIET_MS;
    let row = |run: &Run, at: &str, from: usize| {
        let lines = run.fake.lines_timed();
        let lines = &lines[from..];
        let of = |want: &str| -> Vec<u64> {
            lines
                .iter()
                .filter(|(_, dir, n)| dir == "in" && n == want)
                .map(|(t, _, _)| *t)
                .collect()
        };
        let lists = of("messages.getDialogs").into_iter().max().unwrap_or(0);
        let differences = of("updates.getChannelDifference");
        let after = |t: &u64| i64::try_from(*t).unwrap_or(0) - i64::try_from(lists).unwrap_or(0);
        let last = differences.iter().map(after).max().unwrap_or(0);
        let first = differences.iter().map(after).min().unwrap_or(0);
        let past = differences.iter().filter(|t| after(t) > quiet).count();
        println!(
            "FAKEDC {name}: {at} one_way_ms={one_way_ms} load={} supergroups={} differences={} first_after_lists_ms={first} last_after_lists_ms={last} past_quiet={past} history_requests={} live_rows={}",
            load(),
            supergroups.len(),
            differences.len(),
            of("messages.getHistory").len(),
            run.live(),
        );
    };
    row(&run, "link", 0);
    run.shut_down(&rt);
    for chat in supergroups {
        run.fake.control(&format!("queue new {chat}"));
    }
    let from = run.fake.lines().len();
    let restart_ms = run.restart(&rt);
    std::thread::sleep(SETTLE);
    row(&run, &format!("restart restart_ms={restart_ms}"), from);
    end_of(name, traps, vec![run], &rt);
}

/// A fresh link with an unread message in each of fifty supergroups: how long the lane's pace keeps
/// the account from `Online`.
fn case_link_unread(traps: &Traps) {
    const NAME: &str = "tp-link-unread";
    let rt = rt();
    let run = Run::link(&rt, "many-channels-unread", Duration::ZERO);
    println!(
        "FAKEDC {NAME}: account load={} login_ms={} online_ms={} at_online {} history_requests={}",
        load(),
        run.login_ms,
        run.online_ms,
        memory_of(run.helper()).words(),
        run.fake
            .lines()
            .iter()
            .filter(|(dir, n)| dir == "in" && n == "messages.getHistory")
            .count(),
    );
    end_of(NAME, traps, vec![run], &rt);
}

/// The offline child's cases of this file; false for another name.
pub(super) fn run(case: &str, traps: &Traps) -> bool {
    let standard = || vec!["c401".to_owned(), "c402".to_owned()];
    let many = || {
        (0..50)
            .map(|i| format!("c{}", 1000 + i))
            .collect::<Vec<_>>()
    };
    match case {
        "tp-none" => case_none(traps),
        "tp-one" => case_idle(case, 1, false, traps),
        "tp-one-focused" => case_idle(case, 1, true, traps),
        "tp-two" => case_idle(case, 2, false, traps),
        "tp-growth" => case_growth(traps),
        "tp-burst" => case_burst(traps),
        "tp-quiet-standard" => case_quiet(case, "standard", &standard(), 0, traps),
        "tp-quiet-many" => case_quiet(case, "many-channels", &many(), 0, traps),
        "tp-quiet-many-delay" => case_quiet(case, "many-channels", &many(), 100, traps),
        "tp-link-unread" => case_link_unread(traps),
        _ => return false,
    }
    true
}

// ----- the tests (parent halves), `cargo xtask tdlib fakedc perf` -----

macro_rules! measured {
    ($($test:ident $case:literal,)*) => {$(
        #[test]
        #[ignore = "TP: a measurement, run by `cargo xtask tdlib fakedc perf`"]
        fn $test() {
            fakedc_case_within($case, BOUND);
        }
    )*};
}

measured! {
    tp_none "tp-none",
    tp_one "tp-one",
    tp_one_focused "tp-one-focused",
    tp_two "tp-two",
    tp_growth "tp-growth",
    tp_burst "tp-burst",
    tp_quiet_standard "tp-quiet-standard",
    tp_quiet_many "tp-quiet-many",
    tp_quiet_many_delay "tp-quiet-many-delay",
    tp_link_unread "tp-link-unread",
}
