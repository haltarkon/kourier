//! The REAL library offline (TG-1b B-5: the critic's C14 with H1, H4, M2 and the re-check's N1).
//!
//! Every body runs in ONE offline child: this test binary under `unshare -rn`, with `lo` up and
//! every IPv4 and IPv6 address routed to it (`kourier_tdlib::spawn::run_self_offline`), so a
//! connection or a DNS query that TDLib makes PAST the gate lands on a TRAP listener here and is
//! counted (TCP 443, 80, 5222 and 53, UDP 53, on every address; the resolvers of the child's
//! `resolv.conf` included). Before anything is loaded the child checks it is in such a namespace
//! (`in_offline_child`: no interface but `lo`), and the traps prove themselves armed with a probe of
//! their own. The library comes from `KOURIER_TDJSON` (`cargo xtask tdlib path`): unset, the tests
//! print a `SKIPPED:` line, or fail under `KOURIER_REQUIRE_TDLIB=1`.
//!
//! The helper is the REAL `kourier-tdlib` binary through `kourier_tdlib::spawn` (H4), except in two
//! cases that need a helper the shipped binary cannot be (it has no switch): the N1 case (TDLib
//! refuses the helper's `addProxy`, forced by a test engine around the real library) and the
//! UNPATCHED measurement (no lock-down, the library without the patch); there THIS binary is the
//! helper (`real_helper`), the serve loop over the real library. Nothing is printed but counts and
//! fixed words: no frame, no address, no port, no credential.

use std::ffi::CString;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicUsize;

use kourier_tdlib::lockdown;
use kourier_tdlib::spawn::{SpawnArgs, in_offline_child, offline_namespace, spawn};
use kourier_tdlib::traps::{NetCounters, Traps, resolvers};

use super::*;
use crate::gate::dialer::{DialFuture, Dialer, GateStream};

/// The library of the real-library tests (`cargo xtask tdlib path`).
const TDJSON_ENV: &str = "KOURIER_TDJSON";
/// The library WITHOUT the patch (`cargo xtask tdlib path --unpatched`), for the measurement only.
const UNPATCHED_ENV: &str = "KOURIER_TDJSON_UNPATCHED";
/// `1`: a missing library fails the tests instead of skipping them.
pub(super) const REQUIRE_TDLIB_ENV: &str = "KOURIER_REQUIRE_TDLIB";
/// `1`: a missing `unshare -rn` fails the tests instead of skipping them.
pub(super) const REQUIRE_UNSHARE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";
/// `1`: the lock-down must report Landlock on.
pub(super) const REQUIRE_LANDLOCK_ENV: &str = "KOURIER_REQUIRE_LANDLOCK";
/// The case the offline child runs.
const CASE_ENV: &str = "KOURIER_TG_REAL_CASE";
/// The test-binary helper's `<mode>|<gate port>|<library>`.
pub(super) const HELPER_ENV: &str = "KOURIER_TG_REAL_HELPER";

const CHILD: &str = "supervisor::tests::real_lib::real_child";
pub(super) const HELPER: &str = "supervisor::tests::real_lib::real_helper";

pub(super) fn required(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| v == "1")
}

/// The parent half: runs `case` in the offline child, or skips (a `SKIPPED:` line) where the
/// library or `unshare -rn` is missing and nobody required them. `library` names the variable that
/// must hold the library (the unpatched one is never required).
fn offline_case(case: &str, library: &str) {
    let has_library = std::env::var_os(library).is_some_and(|v| !v.is_empty());
    if !has_library {
        if library == TDJSON_ENV && required(REQUIRE_TDLIB_ENV) {
            panic!("{TDJSON_ENV} is unset and {REQUIRE_TDLIB_ENV}=1: this test must run");
        }
        println!("SKIPPED: {library} is unset (`cargo xtask tdlib path`)");
        return;
    }
    if !kourier_tdlib::spawn::offline_available() {
        assert!(
            !required(REQUIRE_UNSHARE_ENV),
            "`unshare -rn` (or `ip`) is unavailable and {REQUIRE_UNSHARE_ENV}=1"
        );
        println!(
            "SKIPPED: `unshare -rn` or `ip` is unavailable: the real library is never loaded outside it"
        );
        return;
    }
    let run = kourier_tdlib::spawn::run_self_offline(
        CHILD,
        &[(CASE_ENV, case)],
        Duration::from_secs(150),
    )
    .expect("start the offline child");
    for line in run.output.lines().filter(|l| l.starts_with("REAL ")) {
        println!("{line}");
    }
    assert!(
        run.status.success() && run.output.contains("1 passed"),
        "the offline child of {case} failed ({:?}):\n{}",
        run.status,
        run.output
    );
}

/// The helper binary beside this test binary's directory (`target/<profile>/kourier-tdlib`: cargo
/// builds every member's binaries there; the workspace's test build makes it).
pub(super) fn helper_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe.parent().and_then(Path::parent).expect("target dir");
    let helper = dir.join("kourier-tdlib");
    assert!(
        helper.is_file(),
        "the helper binary is not built beside the tests"
    );
    helper
}

fn library(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).expect("the library"))
}

// ----- the test dialers (dial only inside the namespace) -----

/// Accepts the dial and never completes it: TDLib's CONNECT waits at the gate (critic H1: the
/// state is `Connecting` once the login succeeded).
struct HoldingDialer(Arc<AtomicUsize>);

impl Dialer for HoldingDialer {
    fn dial(&self, _to: SocketAddr) -> DialFuture {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

/// Connects every dial to a loopback listener of this child that accepts and never speaks:
/// MTProto never comes up.
struct BlackHoleDialer(SocketAddr, Arc<AtomicUsize>);

impl Dialer for BlackHoleDialer {
    fn dial(&self, _to: SocketAddr) -> DialFuture {
        let to = self.0;
        // Each dial is one TCP connection the namespace's counters will show.
        self.1.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let s = tokio::net::TcpStream::connect(to).await?;
            Ok(Box::new(s) as Box<dyn GateStream>)
        })
    }
}

/// The black hole: accepts, holds every connection open, writes nothing.
fn black_hole() -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("black hole");
    let at = l.local_addr().expect("addr");
    let n = Arc::new(AtomicUsize::new(0));
    let seen = n.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in l.incoming().flatten() {
            seen.fetch_add(1, Ordering::SeqCst);
            held.push(s);
        }
    });
    (at, n)
}

// ----- the parts -----

pub(super) fn process_of(s: kourier_tdlib::spawn::Spawned) -> HelperProcess {
    HelperProcess {
        sock: s.sock,
        stderr: s.stderr.map(|e| Box::new(e) as Box<dyn Read + Send>),
        child: Box::new(s.child),
    }
}

/// What the relay saw, per name of its needles (requests of the app, frames of the helper).
pub(super) type Sent = Arc<Vec<AtomicUsize>>;

/// The relay's work in one direction: copies whole LINES (the protocol's frames end in `\n`)
/// from `from` to `to`, counts every line holding one of `needles` per needle, and replaces
/// `rewrite.0` with `rewrite.1` (the same length) in every line (counted in the LAST slot of
/// `seen`). A frame is written on as soon as its newline came. Nothing is kept or printed but
/// the counts.
fn relay_lines(
    mut from: UnixStream,
    mut to: UnixStream,
    needles: &'static [&'static str],
    rewrite: Option<(&'static str, &'static str)>,
    seen: Sent,
) {
    use std::io::Read as _;
    use std::net::Shutdown;

    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    'copy: while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(at) = pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = pending.drain(..=at).collect();
            for (i, needle) in needles.iter().enumerate() {
                if line.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    seen[i].fetch_add(1, Ordering::SeqCst);
                }
            }
            if let Some((old, new)) = rewrite {
                let (old, new) = (old.as_bytes(), new.as_bytes());
                let mut i = 0;
                while i + old.len() <= line.len() {
                    if &line[i..i + old.len()] == old {
                        line[i..i + old.len()].copy_from_slice(new);
                        seen[needles.len()].fetch_add(1, Ordering::SeqCst);
                        i += old.len();
                    } else {
                        i += 1;
                    }
                }
            }
            if to.write_all(&line).is_err() {
                break 'copy;
            }
        }
    }
    let _ = to.write_all(&pending);
    let _ = to.shutdown(Shutdown::Write);
}

/// A relay between the app's end of the helper's socket and the real helper's: counts how often
/// the app wrote each of `needles` (request `@type`s; fix J16) and the helper each of `back`
/// (frames), and applies `rewrite` to the app's frames (TG-2b's parameters-paths case): the test
/// sees what the actor SENT and what the helper ANSWERED without a hook in the product.
pub(super) fn counting_relay(
    helper_end: UnixStream,
    needles: &'static [&'static str],
    back: &'static [&'static str],
    rewrite: Option<(&'static str, &'static str)>,
    seen: (Sent, Sent),
) -> UnixStream {
    let (app_end, relay_end) = UnixStream::pair().expect("socket pair");
    let (from_app, to_app) = (relay_end.try_clone().expect("clone"), relay_end);
    let (from_helper, to_helper) = (helper_end.try_clone().expect("clone"), helper_end);
    let (sent, answered) = seen;
    std::thread::spawn(move || relay_lines(from_app, to_helper, needles, rewrite, sent));
    std::thread::spawn(move || relay_lines(from_helper, to_app, back, None, answered));
    app_end
}

/// A fresh counter per name, and one more slot (the relay's rewrites).
pub(super) fn counters(names: &[&str]) -> Sent {
    Arc::new((0..=names.len()).map(|_| AtomicUsize::new(0)).collect())
}

/// [`real_parts`], with the app's writes to every helper it starts counted per name in `needles`,
/// the helper's frames per name in `back`, and `rewrite` applied to the app's frames
/// ([`counting_relay`]).
fn counted_real_parts(
    dialer: Arc<dyn Dialer>,
    needles: &'static [&'static str],
    back: &'static [&'static str],
    rewrite: Option<(&'static str, &'static str)>,
) -> (TgParts, Sent, Sent) {
    let (helper, tdjson) = (helper_binary(), library(TDJSON_ENV));
    let (sent, answered) = (counters(needles), counters(back));
    let counted = (sent.clone(), answered.clone());
    let parts = TgParts::from_makers(
        move |args| {
            let spawned = spawn(&SpawnArgs {
                helper: helper.clone(),
                tdjson: tdjson.clone(),
                no_dump: true,
                gate_port: args.gate_port,
                database_dir: args.database_dir.clone(),
                files_dir: args.files_dir.clone(),
            })?;
            let sock = counting_relay(
                spawned.sock,
                needles,
                back,
                rewrite,
                (counted.0.clone(), counted.1.clone()),
            );
            Ok(HelperProcess {
                sock,
                stderr: spawned.stderr.map(|e| Box::new(e) as Box<dyn Read + Send>),
                child: Box::new(spawned.child),
            })
        },
        dialer,
    )
    .requiring_lockdown(LockdownNeed::OnOrUnavailable);
    (parts, sent, answered)
}

/// The REAL binary through the real spawner, with `dialer`.
fn real_parts(dialer: Arc<dyn Dialer>) -> TgParts {
    let (helper, tdjson) = (helper_binary(), library(TDJSON_ENV));
    TgParts::from_makers(
        move |args| {
            spawn(&SpawnArgs {
                helper: helper.clone(),
                tdjson: tdjson.clone(),
                no_dump: true,
                gate_port: args.gate_port,
                database_dir: args.database_dir.clone(),
                files_dir: args.files_dir.clone(),
            })
            .map(process_of)
        },
        dialer,
    )
    // As the application's own helper parts do (G5): the real binary reports its lock-down.
    .requiring_lockdown(LockdownNeed::OnOrUnavailable)
}

/// THIS binary as the helper (`real_helper` in `mode`) over the library in `var`.
pub(super) fn test_helper_parts(mode: &'static str, var: &str, dialer: Arc<dyn Dialer>) -> TgParts {
    let tdjson = library(var);
    TgParts::from_makers(
        move |args| {
            let spec = format!(
                "{mode}|{}|{}|{}|{}",
                args.gate_port,
                args.database_dir.display(),
                args.files_dir.display(),
                tdjson.display()
            );
            kourier_tdlib::spawn::spawn_self(HELPER, (HELPER_ENV, &spec)).map(process_of)
        },
        dialer,
    )
}

pub(super) fn real_timings() -> Timings {
    Timings {
        start: Duration::from_secs(30),
        gate: GateOptions {
            dial_timeout: Duration::from_secs(60),
            ..GateOptions::default()
        },
        ..Timings::default()
    }
}

fn lockdown_ok(word: Lockdown) {
    if required(REQUIRE_LANDLOCK_ENV) {
        assert_eq!(word, Lockdown::On, "the helper's Landlock is not on");
    } else {
        assert_ne!(word, Lockdown::Other, "the hello names no lock-down");
    }
}

/// One start of `parts` up to the phone-number step, `watch` of TDLib running behind the gate,
/// the stop. Returns what the traps, the gate and the namespace's own counters saw.
fn run_start(parts: &TgParts, t: &Timings, watch: Duration, traps: &Traps) -> Watched {
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let helper = rt
        .block_on(start(parts, &account, rt.handle(), t, &cancel))
        .unwrap_or_else(|e| panic!("the start failed: {e:?}"));
    let lockdown = helper.lockdown;
    std::thread::sleep(watch);
    let gate = helper.gate_counts().expect("the gate");
    let hits = traps.hits();
    let report = rt.block_on(helper.stop(t, &cancel));
    // After the helper was reaped and the gate stopped: nothing can still be connecting.
    let opens = traps.opens();
    Watched {
        lockdown,
        gate,
        hits,
        opens,
        report,
    }
}

struct Watched {
    lockdown: Lockdown,
    gate: crate::gate::GateCounts,
    hits: (usize, usize),
    /// The namespace's counters' rise over the whole case (G3).
    opens: NetCounters,
    report: StopReport,
}

/// G3: the namespace's own counters, which see EVERY connection and datagram on any port (the
/// traps hold four ports). TCP connections started == the gate's accepted connections + the test
/// dialer's own onward dials (`onward`); no UDP datagram and no ICMP message at all; every started
/// connection reached a listener (a connection to a port nobody holds would rise only the first).
pub(super) fn assert_namespace_quiet(case: &str, opens: NetCounters, accepted: u64, onward: usize) {
    assert_eq!(
        opens.tcp_active_opens,
        accepted + onward as u64,
        "{case}: a TCP connection other than the gate's and the test dialer's was started ({opens:?})"
    );
    assert_eq!(
        opens.datagrams(),
        0,
        "{case}: a UDP datagram or an ICMP message left ({opens:?})"
    );
    assert_eq!(
        opens.tcp_active_opens, opens.tcp_passive_opens,
        "{case}: a connection went to a port nobody holds ({opens:?})"
    );
}

/// The counters as one line of numbers.
pub(super) fn opens_line(opens: NetCounters) -> String {
    format!(
        "tcp_opens={} tcp_passive={} udp_out={} icmp_out={} udp6_out={} icmp6_out={}",
        opens.tcp_active_opens,
        opens.tcp_passive_opens,
        opens.udp_out,
        opens.icmp_out,
        opens.udp6_out,
        opens.icmp6_out
    )
}

/// The connections the gate accepted over the whole case.
fn accepted(w: &Watched) -> u64 {
    w.report.gate.map_or(0, |g| g.accepted)
}

fn exit_code(report: &StopReport) -> Option<i32> {
    report.status.and_then(|s| s.code())
}

fn print(case: &str, w: &Watched) {
    println!(
        "REAL {case}: lockdown={} gate_accepted={} gate_dials={} gate_rule={} gate_port_refusals={} tcp_traps={} dns_traps={} {} stderr_lines={} exit={:?} killed={}",
        w.lockdown.as_str(),
        w.report.gate.map_or(0, |g| g.accepted),
        w.gate.dials,
        w.gate.rule,
        w.gate.port,
        w.hits.0,
        w.hits.1,
        opens_line(w.opens),
        w.report.stderr_lines,
        exit_code(&w.report),
        w.report.killed,
    );
}

// ----- the cases (inside the offline child) -----

/// T-r1: the real binary and library behind a gate whose dialer REFUSES (the application's own).
fn case_refusing(traps: &Traps) {
    let refused = Arc::new(AtomicUsize::new(0));
    let parts = real_parts(Arc::new(RefusingDialer::new(refused.clone(), false)));
    let w = run_start(&parts, &real_timings(), Duration::from_secs(8), traps);
    print("refusing", &w);
    lockdown_ok(w.lockdown);
    assert!(w.gate.dials >= 1, "TDLib never went through the gate");
    assert_eq!(refused.load(Ordering::SeqCst) as u64, w.gate.dials);
    assert_eq!(
        (w.gate.rule, w.gate.port),
        (0, 0),
        "TDLib asked for a destination off the list"
    );
    assert_eq!(w.hits, (0, 0), "TDLib went past the gate");
    assert_namespace_quiet("refusing", w.opens, accepted(&w), 0);
    assert_eq!(
        w.report.stderr_lines, 0,
        "TDLib wrote to stderr: the log is not hardened"
    );
    assert_eq!((exit_code(&w.report), w.report.killed), (Some(0), false));
}

/// H1: the gate accepts TDLib's CONNECT and never completes it, 15 s.
fn case_holding(traps: &Traps) {
    let dials = Arc::new(AtomicUsize::new(0));
    let parts = real_parts(Arc::new(HoldingDialer(dials.clone())));
    let w = run_start(&parts, &real_timings(), Duration::from_secs(15), traps);
    print("holding", &w);
    lockdown_ok(w.lockdown);
    assert!(
        dials.load(Ordering::SeqCst) >= 1,
        "TDLib never went through the gate"
    );
    assert_eq!(w.hits, (0, 0), "TDLib went past the gate");
    assert_namespace_quiet("holding", w.opens, accepted(&w), 0);
}

/// H1: the gate connects TDLib to a loopback black hole that never speaks MTProto, 15 s.
fn case_black_hole(traps: &Traps) {
    let (hole, reached) = black_hole();
    let onward = Arc::new(AtomicUsize::new(0));
    let parts = real_parts(Arc::new(BlackHoleDialer(hole, onward.clone())));
    let w = run_start(&parts, &real_timings(), Duration::from_secs(15), traps);
    print("black-hole", &w);
    lockdown_ok(w.lockdown);
    assert!(
        reached.load(Ordering::SeqCst) >= 1,
        "nothing reached the black hole"
    );
    assert_eq!(w.hits, (0, 0), "TDLib went past the gate");
    // The dialer's own connections to the black hole are TCP connections of the namespace too.
    assert_namespace_quiet(
        "black-hole",
        w.opens,
        accepted(&w),
        onward.load(Ordering::SeqCst),
    );
}

/// N1: TDLib REFUSES the helper's `addProxy` (the test engine sends it port 0) and keeps the
/// client it has the parameters for (the engine drops the helper's `close`): TDLib is without a
/// proxy for the whole stop (10 s here). The lock-down alone keeps it from the network.
fn case_refused_proxy(traps: &Traps) {
    let parts = test_helper_parts(
        "refused-proxy",
        TDJSON_ENV,
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
    );
    let t = Timings {
        kill_after: Duration::from_secs(10),
        ..real_timings()
    };
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let began = StdInstant::now();
    let failed = rt.block_on(start(&parts, &account, rt.handle(), &t, &cancel));
    let took = began.elapsed();
    let hits = traps.hits();
    let opens = traps.opens();
    println!(
        "REAL refused-proxy: start={:?} after_s={} tcp_traps={} dns_traps={} {}",
        failed.as_ref().err(),
        took.as_secs(),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    assert!(
        matches!(failed, Err(StartError::GateProxy)),
        "the refused proxy was not seen"
    );
    assert!(
        took >= Duration::from_secs(8),
        "TDLib was not left without its proxy long enough"
    );
    assert_eq!(hits, (0, 0), "TDLib went past the lock-down");
    // No proxy, so no gate connection: the lock-down refused every direct one before a SYN left.
    assert_namespace_quiet("refused-proxy", opens, 0, 0);
}

/// G12: two helpers on ONE database directory. TDLib retries the lock for about 10 s, then answers
/// the second one's parameters with "Can't lock file ... already in use": the link types it
/// `DatabaseLocked` (class transient). This pins the text to the pinned TDLib. Once the first helper
/// is stopped the database can be taken again.
fn case_database_locked(traps: &Traps) {
    let parts = real_parts(Arc::new(RefusingDialer::new(
        Arc::new(AtomicUsize::new(0)),
        false,
    )));
    let t = real_timings();
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let first = rt
        .block_on(start(&parts, &account, rt.handle(), &t, &cancel))
        .unwrap_or_else(|e| panic!("the first start failed: {e:?}"));
    let began = StdInstant::now();
    // LOW D2: the failed start's report too, so its gate's connections are counted below.
    let (second, second_report) =
        rt.block_on(start_reporting(&parts, &account, rt.handle(), &t, &cancel));
    let took = began.elapsed();
    let hits = traps.hits();
    let opens = traps.opens();
    println!(
        "REAL database-locked: second={:?} after_s={} tcp_traps={} dns_traps={} {}",
        second.as_ref().err(),
        took.as_secs(),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    let failed = second.err();
    assert_eq!(
        failed,
        Some(StartError::Parameters(LinkError::DatabaseLocked)),
        "the second helper on the database was not typed as a locked database"
    );
    assert_eq!(
        failed.map(StartError::retry),
        Some(Retry::Transient),
        "a locked database is not retried"
    );
    // The first helper's stop frees the database for the next start.
    let first_report = rt.block_on(first.stop(&t, &cancel));
    let third = rt
        .block_on(start(&parts, &account, rt.handle(), &t, &cancel))
        .unwrap_or_else(|e| panic!("the database stayed locked: {e:?}"));
    let third_report = rt.block_on(third.stop(&t, &cancel));
    assert_eq!(traps.hits(), (0, 0), "TDLib went past the gate");
    let opens = traps.opens();
    assert_eq!(opens.datagrams(), 0, "a datagram left ({opens:?})");
    assert_eq!(
        opens.tcp_active_opens, opens.tcp_passive_opens,
        "a connection went to a port nobody holds ({opens:?})"
    );
    // LOW D2: every TCP open of the namespace is one of the three gates' accepted connections (the
    // refusing dialer dials nothing onward).
    let accepted_of = |r: &StopReport| r.gate.map_or(0, |g| g.accepted);
    assert!(
        second_report.is_some(),
        "the failed start handed back no report"
    );
    let sum = accepted_of(&first_report)
        + accepted_of(&third_report)
        + second_report.as_ref().map_or(0, accepted_of);
    assert_namespace_quiet("database-locked", opens, sum, 0);
}

/// The measurement (B-5): the UNPATCHED library, no lock-down, the holding gate, 15 s: the traps
/// must see TDLib's config fallback, or the trap test would be blind to it.
fn case_unpatched(traps: &Traps) {
    let dials = Arc::new(AtomicUsize::new(0));
    let parts = test_helper_parts(
        "open",
        UNPATCHED_ENV,
        Arc::new(HoldingDialer(dials.clone())),
    );
    let w = run_start(&parts, &real_timings(), Duration::from_secs(15), traps);
    print("unpatched-no-lockdown", &w);
    assert!(
        dials.load(Ordering::SeqCst) >= 1,
        "TDLib never went through the gate"
    );
    assert!(
        w.hits.0 + w.hits.1 > 0,
        "the traps saw no fallback: the trap test is blind"
    );
    // The measurement of the namespace's own counters: they see the fallback too (recorded in the
    // REAL line above; the DNS traps hold 53, these hold everything).
    assert!(
        w.opens.tcp_active_opens + w.opens.datagrams() > accepted(&w),
        "the namespace's counters saw nothing beyond the gate: they are blind ({:?})",
        w.opens
    );
}

/// T-r2: a raw SOCKS5 client in the namespace asks the gate for a destination off the list.
fn case_gate_refuses(traps: &Traps) {
    let rt = rt();
    let dials = Arc::new(AtomicUsize::new(0));
    let gate = gate::start(
        rt.handle(),
        Arc::new(HoldingDialer(dials.clone())),
        GateOptions::default(),
        CancellationToken::new(),
    )
    .expect("gate");
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, gate.port())).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut two = [0u8; 2];
    s.write_all(&[5, 1, 2]).expect("greeting");
    s.read_exact(&mut two).expect("method");
    assert_eq!(two, [5, 2]);
    let (user, pass) = (gate.user().as_bytes(), gate.pass().as_bytes());
    let mut login = vec![1, user.len() as u8];
    login.extend_from_slice(user);
    login.push(pass.len() as u8);
    login.extend_from_slice(pass);
    s.write_all(&login).expect("login");
    s.read_exact(&mut two).expect("login answer");
    assert_eq!(two, [1, 0]);
    // 192.0.2.1:443, a documentation address on an allowed port.
    s.write_all(&[5, 1, 0, 1, 192, 0, 2, 1, 1, 187])
        .expect("connect");
    let mut reply = [0u8; 10];
    s.read_exact(&mut reply).expect("reply");
    let counts = gate.counts();
    println!(
        "REAL gate-refuses: rep={} dials={} rule={}",
        reply[1],
        dials.load(Ordering::SeqCst),
        counts.rule
    );
    assert_eq!(reply[1], 2, "not REP 02");
    assert_eq!((dials.load(Ordering::SeqCst), counts.rule), (0, 1));
    // The raw client's own connection is the only TCP connection of the namespace.
    let opens = traps.opens();
    println!("REAL gate-refuses: {}", opens_line(opens));
    assert_namespace_quiet("gate-refuses", opens, counts.accepted, 0);
}

/// T-r3: `Q` while the client applies its parameters: exit 0 or 4, never a signal (SIGABRT).
fn case_quit_mid_start(traps: &Traps) {
    let rt = rt();
    let gate = gate::start(
        rt.handle(),
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
        GateOptions::default(),
        CancellationToken::new(),
    )
    .expect("gate");
    let a = account();
    let account = start_of(&a, TgEnv::Production);
    let s = spawn(&SpawnArgs {
        helper: helper_binary(),
        tdjson: library(TDJSON_ENV),
        no_dump: true,
        gate_port: gate.port(),
        database_dir: account.database_dir.clone(),
        files_dir: account.files_dir.clone(),
    })
    .expect("spawn");
    let mut child = s.child;
    let (link, mut inbound) = HelperLink::start(s.sock, LinkOptions::default()).expect("link");
    let client = rt
        .block_on(link.open(&mut inbound))
        .expect("hello and client");
    link.gate(
        client,
        gate.port(),
        gate.user().to_owned(),
        gate.pass().to_owned(),
    )
    .expect("gate frame");
    // The parameters are written, and the quit follows while TDLib applies them.
    let _ = rt.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(150),
            link.request_secret(client, parameters_secret(&account)),
        )
        .await
    });
    let quit_at = StdInstant::now();
    link.quit();
    drop(link);
    let until = StdInstant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(st) = child.try_wait().expect("wait") {
            break Some(st);
        }
        if StdInstant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect("the helper outlived its deadline");
    // TP: a clean end comes well inside the 15 s close deadline: the real library's answer to a
    // client that does not exist ends the receiver's wait (without it the join takes 15.75 s).
    assert!(
        status.code() != Some(0) || quit_at.elapsed() < Duration::from_secs(12),
        "a clean quit waited for the receiver"
    );
    println!(
        "REAL quit-mid-start: exit={:?} signal={:?} tcp_traps={} dns_traps={}",
        status.code(),
        status.signal(),
        traps.hits().0,
        traps.hits().1
    );
    assert_eq!(status.signal(), None, "the helper died of a signal");
    assert!(
        matches!(status.code(), Some(0 | 4)),
        "exit {:?}",
        status.code()
    );
    assert_eq!(traps.hits(), (0, 0));
    // The gate's accept loop has taken what the helper started before it died.
    std::thread::sleep(Duration::from_millis(200));
    let opens = traps.opens();
    println!("REAL quit-mid-start: {}", opens_line(opens));
    assert_namespace_quiet("quit-mid-start", opens, gate.counts().accepted, 0);
}

/// TG-2 C10: the ACTOR's login over the real binary and library behind the application's
/// refusing dialer. `phone`: the first step is the `phone` input and nothing follows it (no number
/// is ever typed: the real library never goes past `authorizationStateWaitPhoneNumber`). `qr`:
/// `requestQrCodeAuthentication` is sent, TDLib cannot reach Telegram, so no `qr` step (no link)
/// and no failed step comes within 8 s. Both: 0 trap hits, no datagram, every TCP connection
/// reached a listener (the gate; the dialer dials nothing).
fn case_actor_login(traps: &Traps, flow: &str) {
    use kourier_backend_api::{BackendEvent, Command, Exit, FakeClock};
    use kourier_model::LoginStepKind;

    let refused = Arc::new(AtomicUsize::new(0));
    // What the actor SENDS is counted on the way to the real helper (fix J16): the QR flow must
    // have asked for a QR code, once; the phone flow, where no number is typed, asks for no QR code
    // and sends no number.
    let (parts, sent, _) = counted_real_parts(
        Arc::new(RefusingDialer::new(refused.clone(), false)),
        &[
            "requestQrCodeAuthentication",
            "setAuthenticationPhoneNumber",
        ],
        &[],
        None,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock, &rt, dir.path(), real_timings());
    let steps = rt.block_on(async {
        ends.commands
            .send(Command::StartLogin {
                flow: flow.to_owned(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("queued");
        let mut steps = Vec::new();
        // The phone flow: its first step within the start's bound (34 s), then 4 s of quiet. The
        // QR flow: the request must leave the actor within the same bound, then 8 s in which a
        // link would have to come (J16: both stop that long after what they wait for, not at a
        // fixed 34 or 38 s).
        let now = tokio::time::Instant::now;
        let mut by = now() + Duration::from_secs(34);
        let mut settled = false;
        loop {
            let slice = (now() + Duration::from_millis(100)).min(by);
            match tokio::time::timeout_at(slice, ends.events.recv()).await {
                Ok(Some(envelope)) => {
                    for event in envelope.events {
                        if let BackendEvent::Login(step) = event {
                            steps.push(step);
                        }
                    }
                    if let Some(ack) = envelope.ack {
                        ack.complete(Ok(()));
                    }
                }
                Ok(None) => break,
                Err(_) if now() >= by => break,
                Err(_) => {}
            }
            let arrived = if flow == "phone" {
                !steps.is_empty()
            } else {
                sent[0].load(Ordering::SeqCst) > 0
            };
            if arrived && !settled {
                settled = true;
                by = now() + Duration::from_secs(if flow == "phone" { 4 } else { 8 });
            }
        }
        ends.commands.send(Command::Shutdown).await.expect("queued");
        steps
    });
    let exit = actor.join().expect("actor");
    // What TDLib still did after the stop reaches the traps too.
    std::thread::sleep(Duration::from_millis(500));
    let hits = traps.hits();
    let opens = traps.opens();
    println!(
        "REAL actor-{flow}: steps={} tcp_traps={} dns_traps={} dials_refused={} {}",
        steps.len(),
        hits.0,
        hits.1,
        refused.load(Ordering::SeqCst),
        opens_line(opens)
    );
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
    assert_eq!(
        opens.tcp_active_opens, opens.tcp_passive_opens,
        "a TCP connection went to a port nobody holds"
    );
    assert!(
        !steps
            .iter()
            .any(|s| matches!(s.kind, LoginStepKind::Failed { .. })),
        "the login failed offline"
    );
    let (qr_requests, numbers) = (
        sent[0].load(Ordering::SeqCst),
        sent[1].load(Ordering::SeqCst),
    );
    println!("REAL actor-{flow}: qr_requests={qr_requests} numbers_sent={numbers}");
    assert_eq!(numbers, 0, "a number was sent");
    match flow {
        "phone" => {
            assert_eq!(steps.len(), 1, "not exactly one login step");
            assert!(is_phone_step(&steps[0]), "the first step is not `phone`");
            assert_eq!(qr_requests, 0, "the phone flow asked for a QR code");
        }
        _ => {
            assert_eq!(
                qr_requests, 1,
                "the QR flow did not send `requestQrCodeAuthentication` exactly once"
            );
            assert!(
                !steps
                    .iter()
                    .any(|s| s.step_id == "qr" || matches!(s.kind, LoginStepKind::ShowQr { .. })),
                "a QR link came offline"
            );
        }
    }
}

/// The entries of a directory (0 when it is gone).
fn entries(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| d.count())
}

/// TG-2b A3: the actor's phone step, then `Logout` offline: TDLib's `logOut` (and, if it does not
/// close, `destroy`) runs UNDER the filesystem rules and unlinks its own files in `tdlib/`; no F
/// code comes from the helper (no fatal), the logout is reported (`NoDevice`: never authorized),
/// the actor ends `LoggedOut`, `tdlib/` and the files directory are gone; 0 trap hits. The helper
/// is killed after the report by design (`Actor::logout`), so its exit code is not this case's.
fn case_actor_logout(traps: &Traps) {
    use kourier_backend_api::{BackendEvent, Command, Exit, FakeClock};
    use kourier_model::{DeviceLogout, LoginStepKind};

    let refused = Arc::new(AtomicUsize::new(0));
    let (parts, sent, answered) = counted_real_parts(
        Arc::new(RefusingDialer::new(refused, false)),
        &["\"logOut\"", "\"destroy\""],
        &["F ", "\"authorizationStateClosed\""],
        None,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let tdlib = dir.path().join("tdlib");
    let media = dir.path().join("media");
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock, &rt, dir.path(), real_timings());
    let (steps, written, logouts) = rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let mut steps = Vec::new();
        let mut logouts = Vec::new();
        let mut written = None;
        let by = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            match tokio::time::timeout_at(by, ends.events.recv()).await {
                Ok(Some(envelope)) => {
                    for event in envelope.events {
                        match event {
                            BackendEvent::Login(step) => steps.push(step),
                            BackendEvent::Logout(report) => logouts.push(report),
                            _ => {}
                        }
                    }
                    if let Some(ack) = envelope.ack {
                        ack.complete(Ok(()));
                    }
                }
                Ok(None) | Err(_) => break,
            }
            if written.is_none() && !steps.is_empty() {
                // TDLib wrote its files in `tdlib/` under the rules before the logout.
                written = Some(entries(&tdlib));
                ends.commands.send(Command::Logout).await.expect("queued");
            }
        }
        (steps, written, logouts)
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let hits = traps.hits();
    let opens = traps.opens();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    println!(
        "REAL actor-logout: steps={} tdlib_entries_before={:?} logouts={} logout_sent={} destroy_sent={} f_frames={} closed_seen={} tdlib_entries_after={} tcp_traps={} dns_traps={} {}",
        steps.len(),
        written,
        logouts.len(),
        count(&sent, 0),
        count(&sent, 1),
        count(&answered, 0),
        count(&answered, 1),
        entries(&tdlib),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert!(
        is_phone_step(steps.first().expect("a login step")),
        "not the phone step"
    );
    assert!(
        !steps
            .iter()
            .any(|s| matches!(s.kind, LoginStepKind::Failed { .. })),
        "the login failed offline"
    );
    assert!(
        written.is_some_and(|n| n > 0),
        "TDLib wrote nothing in tdlib/"
    );
    assert_eq!(logouts.len(), 1, "not exactly one logout report");
    assert_eq!(logouts[0].outcome, DeviceLogout::NoDevice);
    assert_eq!(count(&sent, 0), 1, "the actor did not send `logOut` once");
    assert_eq!(count(&answered, 0), 0, "the helper wrote an F frame");
    assert!(!tdlib.exists(), "tdlib/ is left after the logout");
    assert_eq!(
        entries(&media),
        0,
        "the files directory is left after the logout"
    );
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
    assert_eq!(
        opens.tcp_active_opens, opens.tcp_passive_opens,
        "a TCP connection went to a port nobody holds"
    );
}

/// TG-2b A3: a relay rewrites the parameters' `database_directory` to a SIBLING of `tdlib/`
/// (`tdlix`, made 0700 beside it): the REAL helper refuses them `F refused-parameters-paths`
/// before TDLib sees them, the start fails `Helper(RefusedParametersPaths)` (class Never), the
/// sibling stays empty. With the helper's check removed (the mutant) TDLib gets the sibling, which
/// the lock-down denies: the case fails on the missing F code and prints TDLib's answer.
fn case_paths_mismatch(traps: &Traps) {
    use std::os::unix::fs::DirBuilderExt as _;

    let (parts, sent, answered) = counted_real_parts(
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
        &[],
        &["F refused-parameters-paths"],
        Some(("/tdlib\"", "/tdlix\"")),
    );
    let t = real_timings();
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let sibling = account.database_dir.with_file_name("tdlix");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&sibling)
        .expect("the sibling");
    let (failed, report) = rt.block_on(start_reporting(&parts, &account, rt.handle(), &t, &cancel));
    let failed = failed.err();
    let report = report.expect("the failed start's report");
    // The app-side counters hold no needle: their one slot is the relay's rewrites.
    let (refusals, rewrites) = (
        answered[0].load(Ordering::SeqCst),
        sent[0].load(Ordering::SeqCst),
    );
    let hits = traps.hits();
    let opens = traps.opens();
    println!(
        "REAL paths-mismatch: start={failed:?} rewrites={rewrites} f_refused_paths={refusals} sibling_entries={} exit={:?} signal={:?} tcp_traps={} dns_traps={} {}",
        entries(&sibling),
        exit_code(&report),
        report.status.and_then(|s| s.signal()),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    assert_eq!(rewrites, 1, "the relay did not rewrite the parameters once");
    assert_eq!(
        refusals, 1,
        "the helper did not answer F refused-parameters-paths"
    );
    assert_eq!(
        failed,
        Some(StartError::Helper(FailureCode::RefusedParametersPaths))
    );
    assert_eq!(failed.map(StartError::retry), Some(Retry::Never));
    assert_eq!(entries(&sibling), 0, "TDLib wrote into the sibling");
    assert_eq!(
        report.status.and_then(|s| s.signal()),
        None,
        "the helper died of a signal"
    );
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
}

/// TG-2b A3 (P-3): THIS binary as the helper in mode `fatal` (the shipped lock-down, the real
/// library, `exit_on_fatal`) calls `td_receive` from a SECOND thread once a client exists, which
/// TDLib answers with its fatal path: the app sees `F tdlib-fatal` (`Helper(TdlibFatal)`, class
/// Abort) and the helper ends with exit 7, killed by no signal (no SIGABRT, no core). Mutant:
/// `exit_on_fatal` ignored -> SIGABRT.
fn case_fatal_exits(traps: &Traps) {
    let parts = test_helper_parts(
        "fatal",
        TDJSON_ENV,
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
    );
    let t = real_timings();
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let (failed, report) = rt.block_on(start_reporting(&parts, &account, rt.handle(), &t, &cancel));
    let failed = failed.err();
    let report = report.expect("the failed start's report");
    let signal = report.status.and_then(|s| s.signal());
    let hits = traps.hits();
    println!(
        "REAL fatal-exits: start={failed:?} exit={:?} signal={signal:?} killed={} tcp_traps={} dns_traps={}",
        exit_code(&report),
        report.killed,
        hits.0,
        hits.1,
    );
    assert_eq!(failed, Some(StartError::Helper(FailureCode::TdlibFatal)));
    assert_eq!(failed.map(StartError::retry), Some(Retry::Abort));
    assert_eq!(signal, None, "the helper died of a signal");
    assert_eq!(exit_code(&report), Some(kourier_tdlib::serve::FATAL_EXIT));
    assert!(!report.killed, "the helper had to be killed");
    assert_eq!(hits, (0, 0), "a connection went past the gate");
}

/// K2 (review H2): after the start the REAL library holds `store_all_files_in_files_directory` =
/// true (the helper set it between the gate's proxy and the parameters, so every download lives in
/// the files directory that has REFER and is emptied at every start), and the app cannot change it:
/// its own `setOption` of that name is refused by the real helper, any value, and the option stays.
/// Mutant: the helper does not send it -> `optionValueEmpty`.
fn case_files_option(traps: &Traps) {
    let parts = real_parts(Arc::new(RefusingDialer::new(
        Arc::new(AtomicUsize::new(0)),
        false,
    )));
    let t = real_timings();
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Production);
    let helper = rt
        .block_on(start(&parts, &account, rt.handle(), &t, &cancel))
        .unwrap_or_else(|e| panic!("the start failed: {e:?}"));
    let client = helper.client;
    let ask = |req: serde_json::Value| rt.block_on(helper.link().request(client, req));
    let get = || {
        ask(json!({"@type": "getOption", "name": "store_all_files_in_files_directory"}))
            .map(|v| (v["@type"].as_str().map(str::to_owned), v["value"].as_bool()))
    };
    let before = get();
    let refused_false = ask(json!({
        "@type": "setOption", "name": "store_all_files_in_files_directory",
        "value": {"@type": "optionValueBoolean", "value": false},
    }));
    let refused_empty = ask(json!({
        "@type": "setOption", "name": "store_all_files_in_files_directory",
        "value": {"@type": "optionValueEmpty"},
    }));
    let after = get();
    let hits = traps.hits();
    let report = rt.block_on(helper.stop(&t, &cancel));
    let opens = traps.opens();
    println!(
        "REAL files-option: before={before:?} refused_false={:?} refused_empty={:?} after={after:?} tcp_traps={} dns_traps={} {}",
        refused_false.as_ref().err(),
        refused_empty.as_ref().err(),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    let on = Ok((Some("optionValueBoolean".to_owned()), Some(true)));
    assert_eq!(before, on, "the helper did not set the files option");
    for refused in [refused_false, refused_empty] {
        assert_eq!(
            refused.err(),
            Some(LinkError::Refused(FailureCode::RefusedRequest)),
            "the app's own files option was not refused"
        );
    }
    assert_eq!(after, on, "the app changed the files option");
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
    assert_eq!(
        report.status.and_then(|s| s.signal()),
        None,
        "the helper died of a signal"
    );
}

/// The offline child's body: refuses to run outside the namespace, arms the traps, runs the case.
#[test]
fn real_child() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    assert!(
        in_offline_child(),
        "not in the offline child: the real library is not loaded"
    );
    let traps = Traps::arm();
    println!(
        "REAL traps armed: ipv6={} resolvers={}",
        traps.v6(),
        resolvers().len()
    );
    match case.as_str() {
        "refusing" => case_refusing(&traps),
        "holding" => case_holding(&traps),
        "black-hole" => case_black_hole(&traps),
        "refused-proxy" => case_refused_proxy(&traps),
        "unpatched" => case_unpatched(&traps),
        "gate-refuses" => case_gate_refuses(&traps),
        "quit-mid-start" => case_quit_mid_start(&traps),
        "database-locked" => case_database_locked(&traps),
        "actor-phone" => case_actor_login(&traps, "phone"),
        "actor-qr" => case_actor_login(&traps, "qr"),
        "actor-logout" => case_actor_logout(&traps),
        "paths-mismatch" => case_paths_mismatch(&traps),
        "fatal-exits" => case_fatal_exits(&traps),
        "files-option" => case_files_option(&traps),
        _ => panic!("no such case"),
    }
}

// ----- the test-binary helper -----

/// The real library with the helper's `addProxy` sent with port 0 (TDLib refuses it) and the
/// helper's `close` dropped (TDLib keeps the client that has its parameters): N1's worst case.
struct RefusedProxy(kourier_tdlib::dlopen::Dlopen);

impl Engine for RefusedProxy {
    fn create_client(&self) -> i32 {
        self.0.create_client()
    }

    fn send(&self, client: i32, json: &CStr) {
        let Ok(mut v) = serde_json::from_slice::<Value>(json.to_bytes()) else {
            return self.0.send(client, json);
        };
        match v.get("@type").and_then(Value::as_str) {
            Some("addProxy") => {
                v["proxy"]["port"] = json!(0);
                let line = CString::new(v.to_string()).expect("no NUL");
                self.0.send(client, &line);
            }
            Some("close") => {}
            _ => self.0.send(client, json),
        }
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        self.0.execute(json)
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        self.0.receive(timeout_s)
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        self.0.set_fatal_callback(cb)
    }
}

/// The real library with a SECOND `td_receive` loop, started on a thread of its own at the first
/// request the app sends (the gate's `addProxy` before the parameters): TDLib's fatal path (two
/// receivers at once) for `case_fatal_exits`. Not at the client's creation: a fatal while the
/// app's `create` waits ends that wait as `Closed` before the link reads the F frame (3 in 30
/// runs, pre3 build 1), so the start would not show the code this case is about.
struct TwoReceivers {
    lib: Arc<kourier_tdlib::dlopen::Dlopen>,
    started: std::sync::Once,
}

impl Engine for TwoReceivers {
    fn create_client(&self) -> i32 {
        self.lib.create_client()
    }

    fn send(&self, client: i32, json: &CStr) {
        self.lib.send(client, json);
        self.started.call_once(|| {
            let lib = Arc::clone(&self.lib);
            std::thread::spawn(move || {
                loop {
                    let _ = lib.receive(1.0);
                }
            });
        });
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        self.lib.execute(json)
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        self.lib.receive(timeout_s)
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        self.lib.set_fatal_callback(cb)
    }
}

/// The verbose log's name inside `tdlib/` (TD-4a, P-14).
pub(super) const VERBOSE_LOG: &str = "tdlib-verbose.log";

/// The real library with TDLib's log turned to a FILE at verbosity 3 at the first client, after
/// the serve loop hardened it (the shipped helper never does this; a test-binary helper only,
/// synthetic data only, never printed).
struct Verbose {
    lib: kourier_tdlib::dlopen::Dlopen,
    log: PathBuf,
    set: std::sync::Once,
}

impl Engine for Verbose {
    fn create_client(&self) -> i32 {
        self.set.call_once(|| {
            let stream = json!({
                "@type": "setLogStream",
                "log_stream": {
                    "@type": "logStreamFile",
                    "path": self.log.to_string_lossy(),
                    "max_file_size": 64 << 20,
                    "redirect_stderr": false,
                },
            });
            let level = json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 3});
            for request in [stream, level] {
                let line = CString::new(request.to_string()).expect("no NUL");
                let _ = self.lib.execute(&line);
            }
        });
        self.lib.create_client()
    }

    fn send(&self, client: i32, json: &CStr) {
        self.lib.send(client, json);
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        self.lib.execute(json)
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        self.lib.receive(timeout_s)
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        self.lib.set_fatal_callback(cb);
    }
}

fn leave(code: i32) -> ! {
    // SAFETY: `_exit` ends the process at once (TDLib's threads may be alive).
    unsafe { libc::_exit(code) }
}

/// THIS binary as the helper (runs only when re-executed with [`HELPER_ENV`], and only inside the
/// offline namespace): `refused-proxy` = the shipped lock-down + [`RefusedProxy`]; `open` = no
/// lock-down, the plain library (the unpatched measurement); `fatal` = the shipped lock-down +
/// [`TwoReceivers`] with `exit_on_fatal` (the helper binary's fatal exit).
#[test]
fn real_helper() {
    let Ok(spec) = std::env::var(HELPER_ENV) else {
        return;
    };
    if !offline_namespace() {
        leave(9); // not 7: that is the fatal exit (P-3)
    }
    // `<mode>|<gate port>|<database dir>|<files dir>|<library>`.
    let mut parts = spec.splitn(5, '|');
    let (Some(mode), Some(port), Some(database_dir), Some(files_dir), Some(path)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        leave(2);
    };
    let dirs = lockdown::FsPaths {
        database_dir: Path::new(database_dir),
        files_dir: Path::new(files_dir),
    };
    let Ok(port) = port.parse::<u16>() else {
        leave(2)
    };
    let Some(sock) = kourier_tdlib::spawn::helper_socket() else {
        leave(2);
    };
    // {1, 1}, not 0: a pipe `core_pattern` (systemd-coredump) ignores a limit of 0 (pre3 K1).
    let no_core = libc::rlimit {
        rlim_cur: kourier_tdlib::harden::CORE_LIMIT,
        rlim_max: kourier_tdlib::harden::CORE_LIMIT,
    };
    // SAFETY: setrlimit with a pointer to a local struct.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
    }
    let Ok(lib) = kourier_tdlib::dlopen::open(Path::new(path)) else {
        leave(3);
    };
    let landlock = match mode {
        // libtest runs this on its own thread: the lock-down of that thread reaches every thread
        // TDLib starts from it (seccomp is synchronised to all).
        "refused-proxy" | "fatal" | "verbose" => {
            match lockdown::apply_on_this_thread(port, &dirs) {
                Ok(state) => state.as_str(),
                Err(_) => leave(6),
            }
        }
        _ => "off",
    };
    let opts = if mode == "fatal" {
        // K9: the shipped helper's own options (its constructor sets `exit_on_fatal`), so this
        // mode proves the product's setting.
        ServeOptions::helper(
            landlock,
            port,
            database_dir.to_owned(),
            files_dir.to_owned(),
        )
    } else {
        ServeOptions {
            landlock,
            gate_port: Some(port),
            dirs: Some((database_dir.to_owned(), files_dir.to_owned())),
            exit_on_fatal: false,
            ..ServeOptions::default()
        }
    };
    let end = match mode {
        "refused-proxy" => serve(RefusedProxy(lib), sock, opts),
        "open" => serve(lib, sock, opts),
        // TD-4a (P-14, critic M9): TDLib's own log at verbosity 3 in a file INSIDE `tdlib/`
        // (the lock-down lets TDLib write only there), for the fake-DC cases' development.
        "verbose" => serve(
            Verbose {
                lib,
                log: Path::new(database_dir).join(VERBOSE_LOG),
                set: std::sync::Once::new(),
            },
            sock,
            opts,
        ),
        "fatal" => serve(
            TwoReceivers {
                lib: Arc::new(lib),
                started: std::sync::Once::new(),
            },
            sock,
            opts,
        ),
        _ => leave(2),
    };
    leave(match end {
        ServeEnd::AllClosed => 0,
        ServeEnd::Forced => 4,
        ServeEnd::Fatal(_) => 5,
    });
}

// ----- the tests (parent halves) -----

/// T-r1 (+ the M2 mutant: no `harden_log` -> stderr lines > 0): the real binary and library reach
/// `authorizationStateWaitPhoneNumber` (the hello's version and commit checked), the gate (whose
/// dialer refuses) sees TDLib's CONNECTs, all on the list; 8 s, 0 trap hits; stderr 0 lines; exit 0.
#[test]
fn the_real_library_hardens_its_log_starts_and_closes() {
    offline_case("refusing", TDJSON_ENV);
}

/// H1: a gate that holds TDLib's CONNECT for 15 s: 0 trap hits (the patch and the lock-down).
#[test]
fn a_holding_gate_leaves_tdlib_no_way_around_it() {
    offline_case("holding", TDJSON_ENV);
}

/// H1: a gate that connects TDLib to a black hole for 15 s: 0 trap hits.
#[test]
fn a_black_hole_behind_the_gate_leaves_tdlib_no_way_around_it() {
    offline_case("black-hole", TDJSON_ENV);
}

/// N1: TDLib without its proxy (it refused the helper's `addProxy`) for 10 s: the lock-down keeps
/// the trap hits at 0, and the start fails typed.
#[test]
fn a_refused_proxy_leaves_tdlib_locked_down() {
    offline_case("refused-proxy", TDJSON_ENV);
}

/// The measurement: the unpatched library without the lock-down behind the holding gate: the traps
/// see the fallback (never required: `KOURIER_TDJSON_UNPATCHED`).
#[test]
fn the_traps_see_the_unpatched_fallback() {
    offline_case("unpatched", UNPATCHED_ENV);
}

/// T-r2: a non-Telegram destination through the gate in the namespace: REP 02, no dial.
#[test]
fn a_non_telegram_destination_through_the_gate_is_refused_in_the_namespace() {
    offline_case("gate-refuses", TDJSON_ENV);
}

/// G12: two helpers on one database directory: the second gets TDLib's "in use" error, typed
/// `DatabaseLocked`, after its ~10 s of retries (inside the offline child).
#[test]
fn a_second_helper_on_one_database_is_typed_as_a_locked_database() {
    offline_case("database-locked", TDJSON_ENV);
}

/// T-r3: `Q` while the client applies its parameters: exit 0 or 4, never a signal.
#[test]
fn quitting_with_a_live_client_does_not_abort() {
    offline_case("quit-mid-start", TDJSON_ENV);
}

/// TG-2 C10: the actor's phone login over the real library reaches its FIRST step `phone` and
/// nothing more (no number is typed, no QR code asked for), offline, 0 trap hits; the case stops 4 s
/// after the step (fix J16).
#[test]
fn the_real_library_reaches_the_phone_step_through_the_login() {
    offline_case("actor-phone", TDJSON_ENV);
}

/// TG-2 C10: the actor's QR login over the real library SENDS `requestQrCodeAuthentication` (once;
/// counted on the way to the helper, fix J16) and gets no link offline (no `qr` step within 8 s of
/// the request), 0 trap hits.
#[test]
fn a_qr_login_offline_never_gets_a_link() {
    offline_case("actor-qr", TDJSON_ENV);
}

/// TG-2b A3: the actor's offline `Logout` under the filesystem rules: TDLib unlinks its own files,
/// no F code, the logout reported, `tdlib/` and the files directory gone.
#[test]
fn an_offline_logout_under_the_filesystem_rules_removes_everything() {
    offline_case("actor-logout", TDJSON_ENV);
}

/// TG-2b A3: parameters naming a sibling of `tdlib/` are refused by the REAL helper
/// (`F refused-parameters-paths`) before TDLib sees them.
#[test]
fn parameters_naming_another_directory_are_refused_by_the_real_helper() {
    offline_case("paths-mismatch", TDJSON_ENV);
}

/// TG-2b A3 (P-3): TDLib's fatal path in a helper with `exit_on_fatal`: `F tdlib-fatal`, exit 7, no
/// signal.
#[test]
fn tdlibs_fatal_path_ends_the_helper_with_exit_7_and_no_signal() {
    offline_case("fatal-exits", TDJSON_ENV);
}

/// K2 (review H2): the real library keeps every download in the files directory (the option the
/// helper sets) and the app cannot undo it.
#[test]
fn the_real_library_keeps_every_download_in_the_files_directory() {
    offline_case("files-option", TDJSON_ENV);
}
