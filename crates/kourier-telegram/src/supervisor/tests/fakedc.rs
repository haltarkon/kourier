//! The REAL helper and the TEST-ONLY library against the lane's fake DC (TD-4a, brief §3.5-§3.6,
//! §3.9): TG-2's real login machine past the phone step, offline.
//!
//! Every body runs in ONE offline child (`run_self_offline`, as `real_lib.rs`): `unshare -rn`,
//! `lo` up, every address routed to `lo`, the traps armed. In it the fake DC (`tg-fakedc serve`,
//! `KOURIER_TG_FAKEDC_BIN`) is started by `spawn_offline_companion` on 127.0.0.1 and a port the
//! kernel picks; the helper is the real `kourier-tdlib` binary over the test-only library
//! (`KOURIER_TDJSON_FAKEDC`: both handshake keys replaced by the fake DC's own, the commit 40
//! zeros); its gate's dialer is [`RemapDialer`], which hands each allowed CONNECT to the fake DC
//! with the line `KFDC1 <ip> <port>`. The three variables come from `cargo xtask tdlib fakedc env`:
//! unset, the cases print `SKIPPED:`, or fail under `KOURIER_REQUIRE_FAKEDC=1`. Only counts, fixed
//! words and durations are printed: no frame, no number, no code, no key.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicUsize;

use kourier_tdlib::spawn::{
    Companion, SpawnArgs, in_offline_child, offline_namespace, spawn, spawn_offline_companion,
};
use kourier_tdlib::traps::{Traps, resolvers};

use super::real_lib::{
    HELPER, HELPER_ENV, REQUIRE_LANDLOCK_ENV, REQUIRE_UNSHARE_ENV, Sent, VERBOSE_LOG,
    assert_namespace_quiet, counters, counting_relay, helper_binary, opens_line, process_of,
    real_timings, required,
};
use super::*;
use crate::gate::dialer::{DialFuture, Dialer, GateStream};
use crate::login::{field, step};
use crate::pin::Pin;

mod rec;
mod tp;

/// The test-only library (`cargo xtask tdlib fakedc env`).
const LIB_ENV: &str = "KOURIER_TDJSON_FAKEDC";
/// The fake DC's binary (absolute, `target/tg-fakedc/release/tg-fakedc`).
const BIN_ENV: &str = "KOURIER_TG_FAKEDC_BIN";
/// The fake DC's private test key (`target/tdlib-fakedc/test-key.pem`, 0600).
const KEY_ENV: &str = "KOURIER_TG_FAKEDC_KEY";
/// `1`: missing fake-DC variables fail the cases instead of skipping them.
const REQUIRE_FAKEDC_ENV: &str = "KOURIER_REQUIRE_FAKEDC";
/// `1`: the helper is THIS binary in mode `verbose` (TDLib's log at verbosity 3 in
/// `tdlib/tdlib-verbose.log`, P-14), for developing the fake DC; never in ci.
const VERBOSE_ENV: &str = "KOURIER_TG_FAKEDC_VERBOSE";
/// `1`: the case's temporary directory (the wire log, the verbose log) is kept and its path printed.
const KEEP_ENV: &str = "KOURIER_TG_FAKEDC_KEEP";
/// The case the offline child runs.
const CASE_ENV: &str = "KOURIER_TG_FAKEDC_CASE";

const CHILD: &str = "supervisor::tests::fakedc::fakedc_child";

/// What the test-only library's hello names: the pinned version, the pinned patch tag (the byte
/// patch keeps it: the library is the 0002 one) and the commit the library patch replaced by 40
/// zeros (`cargo xtask tdlib fakedc lib`). Only these tests' parts expect it.
pub(super) const FAKEDC_PIN: Pin = Pin {
    commit: "0000000000000000000000000000000000000000",
    version: crate::pin::PIN.version,
    patch_level: crate::pin::PIN.patch_level,
};

/// The login's synthetic number (a +99966-style test number of the scripted fixtures) and code.
const NUMBER: &str = "+999068204719";
const DIGITS: &str = "999068204719";
const CODE: &str = "12345";
/// The account the fake DC logs in (`fakedc/src/rpc.rs` `OWN_USER_ID`).
const FAKE_OWN_ID: i64 = 100;
/// The fake DC's own phone after a QR login (`fakedc/src/rpc.rs` `QR_OWN_PHONE`; an original).
const QR_DIGITS: &str = "999061000100";

/// The test-only library is the one its manifest vouches for (S-M3), checked once per test
/// binary: the sha256 of the bytes `KOURIER_TDJSON_FAKEDC` names against `manifest.toml` beside
/// it (which `cargo xtask tdlib fakedc lib` wrote with the base's checks). `env`/`record` check the
/// whole manifest; this is the half a test binary can do without xtask, so a shell that exported
/// the variables before a rebuild cannot run a library whose bytes moved.
fn library_vouched() -> &'static Result<(), String> {
    use sha2::{Digest, Sha256};
    static VOUCHED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    VOUCHED.get_or_init(|| {
        let lib = var(LIB_ENV).ok_or("no test-only library")?;
        let text = std::fs::read_to_string(lib.with_file_name("manifest.toml"))
            .map_err(|_| "the test-only library has no manifest.toml beside it".to_owned())?;
        let want = text
            .lines()
            .find_map(|l| l.strip_prefix("sha256 = \"")?.strip_suffix('"'))
            .ok_or("the manifest has no sha256")?;
        let bytes = std::fs::read(&lib).map_err(|_| "the test-only library is unreadable")?;
        let got: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if got == want {
            Ok(())
        } else {
            Err("the test-only library's sha256 differs from its manifest".to_owned())
        }
    })
}

fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The parent half: runs `case` in the offline child, or skips where a fake-DC variable or
/// `unshare -rn` is missing and nobody required them.
fn fakedc_case(case: &str) {
    fakedc_case_within(case, Duration::from_secs(150));
}

/// [`fakedc_case`] with its own bound on the child (TP's measuring cases are longer).
fn fakedc_case_within(case: &str, bound: Duration) {
    let missing: Vec<&str> = [LIB_ENV, BIN_ENV, KEY_ENV]
        .into_iter()
        .filter(|v| var(v).is_none())
        .collect();
    if !missing.is_empty() {
        assert!(
            !required(REQUIRE_FAKEDC_ENV),
            "{missing:?} unset and {REQUIRE_FAKEDC_ENV}=1: this test must run"
        );
        println!(
            "SKIPPED: {missing:?} unset (`v=$(cargo xtask tdlib fakedc env) || exit 1; eval \"$v\"`)"
        );
        return;
    }
    if let Err(e) = library_vouched() {
        panic!("{case}: {e}");
    }
    if !kourier_tdlib::spawn::offline_available() {
        assert!(
            !required(REQUIRE_UNSHARE_ENV) && !required(REQUIRE_FAKEDC_ENV),
            "`unshare -rn` (or `ip`) is unavailable and a REQUIRE flag is set"
        );
        println!("SKIPPED: `unshare -rn` or `ip` is unavailable");
        return;
    }
    let run = kourier_tdlib::spawn::run_self_offline(CHILD, &[(CASE_ENV, case)], bound)
        .expect("start the offline child");
    for line in run.output.lines().filter(|l| l.starts_with("FAKEDC ")) {
        println!("{line}");
    }
    assert!(
        run.status.success() && run.output.contains("1 passed"),
        "the offline child of {case} failed ({:?}):\n{}",
        run.status,
        run.output
    );
}

// ----- the remap dialer and the fake DC -----

/// The gate's dialer of these tests (lead call L7: test code, no product line): every dial the
/// gate allowed goes to the fake DC, prefixed by `KFDC1 <ip> <port>\n` (the destination TDLib
/// asked for, so the fake answers as that DC). Refuses outside the offline namespace; counts.
struct RemapDialer {
    fakedc: SocketAddr,
    dials: Arc<AtomicUsize>,
}

impl Dialer for RemapDialer {
    fn dial(&self, to: SocketAddr) -> DialFuture {
        if !offline_namespace() {
            return Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the remap dials only inside the offline namespace",
                ))
            });
        }
        self.dials.fetch_add(1, Ordering::SeqCst);
        let fakedc = self.fakedc;
        Box::pin(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut s = tokio::net::TcpStream::connect(fakedc).await?;
            s.write_all(format!("KFDC1 {} {}\n", to.ip(), to.port()).as_bytes())
                .await?;
            Ok(Box::new(s) as Box<dyn GateStream>)
        })
    }
}

/// A running fake DC: killed on drop (and by its parent-death signal).
struct FakeDc {
    _companion: Companion,
    at: SocketAddr,
    wire_log: PathBuf,
    /// The control socket (`fakedc/src/control.rs`).
    control: PathBuf,
    /// The world's originals (rewritten after every control command; the scrub reads it).
    world_json: PathBuf,
}

impl FakeDc {
    /// One control command; its answer's counts (`ok <json>`), or a panic naming the command's
    /// first word and the fake DC's error word (commands carry no value).
    fn control(&self, command: &str) -> Value {
        let verb = command.split_whitespace().next().unwrap_or("");
        self.try_control(command)
            .unwrap_or_else(|word| panic!("the fake DC refused `{verb}`: {word:?}"))
    }

    /// One control command; its answer's JSON (`ok <json>`), or the fake DC's error word.
    fn try_control(&self, command: &str) -> Result<Value, String> {
        use std::io::{BufRead as _, BufReader};
        let mut s =
            std::os::unix::net::UnixStream::connect(&self.control).expect("the control socket");
        s.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        writeln!(s, "{command}").expect("send the command");
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).expect("the answer");
        match line.trim_end().split_once(' ') {
            Some(("ok", counts)) => Ok(serde_json::from_str(counts).expect("the counts")),
            Some((_, word)) => Err(word.to_owned()),
            None => Err(line.trim_end().to_owned()),
        }
    }

    /// `read-state <chat>` (`u201`, `g301`, `c401`): the chat's inbox and outbox read marks in the
    /// fake DC's world (MODEL: the fake stores exactly the id a read request names).
    fn read_state(&self, chat: &str) -> (i64, i64) {
        let v = self.control(&format!("read-state {chat}"));
        (
            v["inbox"].as_i64().expect("the inbox mark"),
            v["outbox"].as_i64().expect("the outbox mark"),
        )
    }

    /// The wire log's (direction, constructor name) pairs, in order: `in` a request, `out` an
    /// answer, `push` an `Updates` object the fake sent unasked.
    fn lines(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.wire_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| {
                Some((
                    v["dir"].as_str()?.to_owned(),
                    v["name"].as_str()?.to_owned(),
                ))
            })
            .collect()
    }

    /// [`FakeDc::lines`] with each line's time in ms since the epoch (the fake DC's own clock).
    fn lines_timed(&self) -> Vec<(u64, String, String)> {
        std::fs::read_to_string(&self.wire_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| {
                Some((
                    v["t"].as_u64()?,
                    v["dir"].as_str()?.to_owned(),
                    v["name"].as_str()?.to_owned(),
                ))
            })
            .collect()
    }

    /// The wire log's constructor names (the fake DC writes names only), in order.
    fn names(&self) -> Vec<String> {
        std::fs::read_to_string(&self.wire_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["name"].as_str().map(str::to_owned))
            .collect()
    }
}

/// Starts the fake DC in `dir` (from the test's own thread: its parent-death signal follows the
/// spawning thread) and waits up to 5 s for its port file.
fn start_fakedc(dir: &Path) -> FakeDc {
    let (bin, key) = (
        var(BIN_ENV).expect("the fake DC"),
        var(KEY_ENV).expect("the key"),
    );
    let port_file = dir.join("fakedc.port");
    let wire_log = dir.join("wire.jsonl");
    let control = dir.join("fakedc.control");
    let world_json = dir.join("world.json");
    let args: Vec<&std::ffi::OsStr> = vec![
        "serve".as_ref(),
        "--key".as_ref(),
        key.as_os_str(),
        "--port-file".as_ref(),
        port_file.as_os_str(),
        "--wire-log".as_ref(),
        wire_log.as_os_str(),
        "--control".as_ref(),
        control.as_os_str(),
        "--world-json".as_ref(),
        world_json.as_os_str(),
    ];
    let companion = spawn_offline_companion(&bin, &args).expect("start the fake DC");
    let by = StdInstant::now() + Duration::from_secs(5);
    let port = loop {
        if let Some(p) = std::fs::read_to_string(&port_file)
            .ok()
            .and_then(|t| t.trim().parse::<u16>().ok())
        {
            break p;
        }
        assert!(StdInstant::now() < by, "the fake DC wrote no port in 5 s");
        std::thread::sleep(Duration::from_millis(20));
    };
    FakeDc {
        _companion: companion,
        at: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        wire_log,
        control,
        world_json,
    }
}

/// The parts of these tests: the real helper binary (or, with [`VERBOSE_ENV`], this binary in mode
/// `verbose`) over the test-only library, the app's frames counted per `needles` and the helper's
/// per `back`, the lock-down required, the hello's pin [`FAKEDC_PIN`], Telegram's TEST
/// environment (TDLib dials its test DCs, which the gate allows and the remap hands to the fake).
fn fakedc_parts(
    dialer: Arc<dyn Dialer>,
    needles: &'static [&'static str],
    back: &'static [&'static str],
) -> (TgParts, Sent, Sent, Pids) {
    fakedc_parts_taped(dialer, needles, back, None)
}

/// [`fakedc_parts`] with, when `tape` is given, the recording relay in front of the counting one:
/// the helper's lines in ORDER (G2 reads TDLib's connection states from it).
fn fakedc_parts_taped(
    dialer: Arc<dyn Dialer>,
    needles: &'static [&'static str],
    back: &'static [&'static str],
    tape: Option<rec::TapeRef>,
) -> (TgParts, Sent, Sent, Pids) {
    let (helper, tdjson) = (helper_binary(), var(LIB_ENV).expect("the library"));
    let verbose = required(VERBOSE_ENV);
    let pids: Pids = Arc::default();
    let started = pids.clone();
    let (sent, answered) = (counters(needles), counters(back));
    let counted = (sent.clone(), answered.clone());
    let parts = TgParts::from_makers(
        move |args| {
            let spawned = if verbose {
                let spec = format!(
                    "verbose|{}|{}|{}|{}",
                    args.gate_port,
                    args.database_dir.display(),
                    args.files_dir.display(),
                    tdjson.display()
                );
                kourier_tdlib::spawn::spawn_self(HELPER, (HELPER_ENV, &spec))?
            } else {
                spawn(&SpawnArgs {
                    helper: helper.clone(),
                    tdjson: tdjson.clone(),
                    no_dump: true,
                    gate_port: args.gate_port,
                    database_dir: args.database_dir.clone(),
                    files_dir: args.files_dir.clone(),
                })?
            };
            started.lock().expect("pids").push(spawned.child.id());
            let helper_end = match &tape {
                Some(t) => rec::recording_relay(spawned.sock, t.clone()),
                None => spawned.sock,
            };
            let sock = counting_relay(
                helper_end,
                needles,
                back,
                None,
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
    .requiring_lockdown(LockdownNeed::OnOrUnavailable)
    .expecting_pin(&FAKEDC_PIN)
    .with_env(TgEnv::Test);
    (parts, sent, answered, pids)
}

/// The process ids of the helpers the parts started, in order.
type Pids = Arc<Mutex<Vec<u32>>>;

/// TG-4 K1: the fake DC's read commands (`read-state`, `hold-reads`, `threshold common`) through
/// the real binary's control socket, no helper.
#[test]
fn the_fake_dc_control_socket_speaks_the_read_commands() {
    fakedc_case("control-reads");
}

/// The case's temporary directory; kept (its path printed) under [`KEEP_ENV`].
fn case_dir(case: &str, dir: tempfile::TempDir) {
    if required(KEEP_ENV) {
        println!("FAKEDC {case}: kept {}", dir.keep().display());
    }
}

/// The fake DC's counts once it has accepted every connection the remap dialled (the listener's
/// accept can lag a dial by a moment; a count that stays ABOVE the dials never settles).
fn settled_stats(fake: &FakeDc, dials: usize) -> Value {
    let mut stats = fake.control("stats");
    for _ in 0..30 {
        if stats["accepted"].as_u64() >= Some(dials as u64) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        stats = fake.control("stats");
    }
    stats
}

/// The end every fake-DC case shares (S-M1, S-M2, S-L5): no trap hit, no datagram, every TCP
/// connection reached a listener, and the ActiveOpens EQUATION. The fake DC counts the
/// connections it accepted: they are exactly the remap's `dials` (a second connection to it, from
/// the remap or from anything else, breaks the equation). Where the gate's counts are reachable
/// (`gate_accepted`: the stop reports of the helpers) the namespace's active opens are the gate's
/// accepted connections plus the dials; the actor cases cannot reach them, so every dial is at
/// least a helper -> gate connection plus the remap's own (`active >= 2 * dials`). Prints the
/// numbers; returns the fake DC's counts.
fn net_end(
    name: &str,
    traps: &Traps,
    fake: &FakeDc,
    dials: usize,
    gate_accepted: Option<u64>,
) -> Value {
    let hits = traps.hits();
    let opens = traps.opens();
    let stats = settled_stats(fake, dials);
    println!(
        "FAKEDC {name}: net tcp_traps={} dns_traps={} fakedc_accepted={} dials={dials} gate_accepted={gate_accepted:?} {}",
        hits.0,
        hits.1,
        stats["accepted"],
        opens_line(opens)
    );
    assert_eq!(hits, (0, 0), "{name}: a connection went past the gate");
    assert_eq!(
        stats["accepted"].as_u64(),
        Some(dials as u64),
        "{name}: the fake DC accepted a connection the remap did not dial"
    );
    assert_eq!(
        opens.datagrams(),
        0,
        "{name}: a datagram left the namespace"
    );
    assert_eq!(
        opens.tcp_active_opens, opens.tcp_passive_opens,
        "{name}: a TCP connection went to a port nobody holds"
    );
    match gate_accepted {
        Some(accepted) => assert_eq!(
            opens.tcp_active_opens,
            accepted + dials as u64,
            "{name}: a TCP connection other than the gate's and the remap's was started"
        ),
        None => assert!(
            opens.tcp_active_opens >= 2 * dials as u64,
            "{name}: fewer TCP connections than the remap's dials need"
        ),
    }
    stats
}

/// SIGKILL to the helper this case started: never `kill(0)` (the test binary's process group) or
/// `kill(-1)` (S-L4): a pid that is 0 or does not fit `pid_t` is not killed.
fn kill_pid(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(2) on the helper this case started (its own process group), a positive pid.
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}

/// What [`login_by_code`] saw.
struct Login {
    steps: Vec<kourier_model::LoginStep>,
    readies: Vec<Vec<kourier_model::UserId>>,
    ready_at: Option<Duration>,
    done_at: Option<Duration>,
}

/// The login by code through the actor (`StartLogin` phone, the number, the code), every envelope
/// acked, until `done` or a `Failed` step (90 s at most).
async fn login_by_code(ends: &mut kourier_backend_api::CoreEnds, started: StdInstant) -> Login {
    use kourier_backend_api::{BackendEvent, Command};
    use kourier_model::{LoginStepKind, Redacted};

    let input = |txn: u64, k: &str, v: &str| Command::SubmitLoginInput {
        txn,
        values: [(k.to_owned(), Redacted::new(v.to_owned()))]
            .into_iter()
            .collect(),
    };
    ends.commands.send(start_login()).await.expect("queued");
    let mut steps: Vec<kourier_model::LoginStep> = Vec::new();
    let mut readies = Vec::new();
    let (mut ready_at, mut done_at) = (None, None);
    let by = tokio::time::Instant::now() + Duration::from_secs(90);
    'run: loop {
        let envelope = match tokio::time::timeout_at(by, ends.events.recv()).await {
            Ok(Some(envelope)) => envelope,
            Ok(None) | Err(_) => break,
        };
        let mut reply = None;
        for event in envelope.events {
            match event {
                BackendEvent::Login(s) => {
                    if s.step_id == step::PHONE {
                        reply = Some(input(s.txn, field::PHONE, NUMBER));
                    } else if s.step_id == step::CODE {
                        reply = Some(input(s.txn, field::CODE, CODE));
                    }
                    let end =
                        s.step_id == step::DONE || matches!(s.kind, LoginStepKind::Failed { .. });
                    if s.step_id == step::DONE {
                        done_at = Some(started.elapsed());
                    }
                    steps.push(s);
                    if end {
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                        break 'run;
                    }
                }
                BackendEvent::AccountReady { own_ids, .. } => {
                    ready_at = Some(started.elapsed());
                    readies.push(own_ids);
                }
                _ => {}
            }
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
        if let Some(c) = reply {
            ends.commands.send(c).await.expect("queued");
        }
    }
    Login {
        steps,
        readies,
        ready_at,
        done_at,
    }
}

// ----- the cases (inside the offline child) -----

/// G1: the login by code through the ACTOR and TG-2's login machine: `phone` -> the number ->
/// `code` -> the code -> `linking` -> TDLib Ready -> `getMe` -> ONE `AccountReady` with
/// `[tg:<fake's own id>, pn:<digits>]` through the outbox -> `done` after the core's ack. The number
/// and the code are sent once each; no request is refused by the helper; Landlock on (under
/// `KOURIER_REQUIRE_LANDLOCK=1`) with the filesystem rules; 0 trap hits, no datagram, every TCP
/// connection reached a listener, the remap dialled.
fn case_login(traps: &Traps) {
    use kourier_backend_api::{Command, Exit, FakeClock};
    use kourier_model::{LoginStepKind, UserId};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    // The control socket answers before any client exists (the served world: the account alone).
    let before = fake.control("world alone");
    assert_eq!(
        (before["conns"].as_u64(), before["pts"].as_u64()),
        (Some(0), Some(1))
    );
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, sent, answered, _) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &[
            "setAuthenticationPhoneNumber",
            "checkAuthenticationCode",
            "\"getMe\"",
        ],
        &["F refused-request", "\"landlock\":\"on\""],
    );
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock, &rt, dir.path(), real_timings());
    let Login {
        steps,
        readies,
        ready_at,
        done_at,
    } = rt.block_on(async {
        let login = login_by_code(&mut ends, started).await;
        ends.commands.send(Command::Shutdown).await.expect("queued");
        login
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let stats = fake.control("stats");
    let world_json_written = std::fs::metadata(&fake.world_json).is_ok_and(|m| m.len() > 0);
    let hits = traps.hits();
    let opens = traps.opens();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let ids: Vec<&str> = steps.iter().map(|s| s.step_id.as_str()).collect();
    println!("FAKEDC login: control stats {stats}");
    println!(
        "FAKEDC login: steps={ids:?} ready_s={:?} done_s={:?} total_s={:.1} readies={} numbers_sent={} codes_sent={} get_me={} refused={} landlock_on={} dials={} tcp_traps={} dns_traps={} {}",
        ready_at.map(|d| d.as_secs_f32()),
        done_at.map(|d| d.as_secs_f32()),
        started.elapsed().as_secs_f32(),
        readies.len(),
        count(&sent, 0),
        count(&sent, 1),
        count(&sent, 2),
        count(&answered, 0),
        count(&answered, 1),
        dials.load(Ordering::SeqCst),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    if ids != ["phone", "code", "linking", "done"] {
        // The fake DC's constructor names (names only) say where TDLib stopped.
        let names = fake.names();
        let tail = &names[names.len().saturating_sub(60)..];
        println!("FAKEDC login: wire tail {tail:?}");
        if required(VERBOSE_ENV) {
            println!(
                "FAKEDC login: verbose log {}",
                dir.path().join("tdlib").join(VERBOSE_LOG).display()
            );
        }
    }
    let failed = steps
        .iter()
        .find_map(|s| match &s.kind {
            LoginStepKind::Failed { error } => Some(format!("{:?} {}", error.kind, error.detail)),
            _ => None,
        })
        .unwrap_or_default();
    net_end("login", traps, &fake, dials.load(Ordering::SeqCst), None);
    case_dir("login", dir);
    // First: a dial that left for the original destination lands on a trap (mutant C3a).
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        ids,
        ["phone", "code", "linking", "done"],
        "not the login by code (failed: {failed})"
    );
    assert_eq!(readies.len(), 1, "not exactly one AccountReady");
    assert_eq!(
        readies[0],
        [
            UserId::from_parts("tg", &FAKE_OWN_ID.to_string()).expect("id"),
            UserId::from_parts("pn", DIGITS).expect("id"),
        ],
        "the own ids are not the fake account's"
    );
    assert!(ready_at < done_at, "done came before the AccountReady");
    assert_eq!(count(&sent, 0), 1, "the number was not sent exactly once");
    assert_eq!(count(&sent, 1), 1, "the code was not sent exactly once");
    assert_eq!(count(&sent, 2), 1, "`getMe` was not sent exactly once");
    assert_eq!(count(&answered, 0), 0, "the helper refused a request");
    if required(REQUIRE_LANDLOCK_ENV) {
        assert!(count(&answered, 1) >= 1, "the helper's Landlock is not on");
    }
    assert!(dials.load(Ordering::SeqCst) > 0, "the remap never dialled");
    assert_eq!(
        stats["phone_requests"].as_u64(),
        Some(0),
        "a phone.* request reached the fake DC"
    );
    assert!(world_json_written, "the fake DC wrote no world.json");
}

/// G5: the APPLICATION's expectation ([`PIN`], as `real.rs`'s parts) refuses the test-only
/// library at the hello: `WRONG_HELPER` (`VersionMismatch`), before any client: no parameters
/// ever reach the helper. The guarantee that the test-only library can never run as the app's.
fn case_wrong_helper(traps: &Traps) {
    let started = StdInstant::now();
    let (helper, tdjson) = (helper_binary(), var(LIB_ENV).expect("the library"));
    let sent = counters(&["setTdlibParameters"]);
    let counted = sent.clone();
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
                &["setTdlibParameters"],
                &[],
                None,
                (counted.clone(), counters(&[])),
            );
            Ok(process_of(kourier_tdlib::spawn::Spawned {
                sock,
                ..spawned
            }))
        },
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
    )
    .requiring_lockdown(LockdownNeed::OnOrUnavailable)
    .with_env(TgEnv::Test);
    assert_eq!(parts.pin(), &crate::pin::PIN, "not the application's pin");
    let rt = rt();
    let a = account();
    let cancel = CancellationToken::new();
    let account = start_of(&a, TgEnv::Test);
    let (result, report) = rt.block_on(start_reporting(
        &parts,
        &account,
        rt.handle(),
        &real_timings(),
        &cancel,
    ));
    let hits = traps.hits();
    let opens = traps.opens();
    let refused = matches!(result, Err(StartError::Link(LinkError::VersionMismatch)));
    println!(
        "FAKEDC wrong-helper: refused_at_hello={refused} secs={:.1} parameters_sent={} stopped={} tcp_traps={} dns_traps={}",
        started.elapsed().as_secs_f32(),
        sent[0].load(Ordering::SeqCst),
        report.is_some(),
        hits.0,
        hits.1
    );
    if let Ok(helper) = result {
        let _ = rt.block_on(helper.stop(&real_timings(), &cancel));
        panic!("the application's parts started over the test-only library");
    }
    assert!(refused, "not refused as WRONG_HELPER: {:?}", result.err());
    assert_eq!(
        sent[0].load(Ordering::SeqCst),
        0,
        "parameters reached the helper"
    );
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    // S-L5: the helper and its gate ran, so the namespace's own counters are read too: no
    // datagram, every connection reached a listener, and the only TCP connections are the gate's.
    let accepted = report.and_then(|r| r.gate).map_or(0, |g| g.accepted);
    println!(
        "FAKEDC wrong-helper: gate_accepted={accepted} {}",
        opens_line(opens)
    );
    assert_namespace_quiet("wrong-helper", opens, accepted, 0);
}

/// A4 (P-2, critic H1(b)) and G4: the login by code, then `Logout` after Ready under the
/// filesystem rules: TDLib's `logOut` (answered by the fake) and its own unlinks in `tdlib/`; no F
/// frame, ONE `Logout` report, the actor ends `LoggedOut`, `tdlib/` and the files directory gone,
/// the logout under 8 s; 0 trap hits.
fn case_logout(traps: &Traps) {
    use kourier_backend_api::{BackendEvent, Command, Exit, FakeClock};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, sent, answered, _) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &["\"logOut\"", "\"destroy\""],
        &["F "],
    );
    let tdlib = dir.path().join("tdlib");
    let media = dir.path().join("media");
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock, &rt, dir.path(), real_timings());
    let (login, logouts, logout_took) = rt.block_on(async {
        let login = login_by_code(&mut ends, started).await;
        // Let TDLib settle after Ready (its first writes after the login), then log out.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let asked = StdInstant::now();
        ends.commands.send(Command::Logout).await.expect("queued");
        let mut logouts = Vec::new();
        let by = tokio::time::Instant::now() + Duration::from_secs(20);
        while let Ok(Some(envelope)) = tokio::time::timeout_at(by, ends.events.recv()).await {
            for event in envelope.events {
                if let BackendEvent::Logout(report) = event {
                    logouts.push(report);
                }
            }
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
        (login, logouts, asked.elapsed())
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let hits = traps.hits();
    let opens = traps.opens();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let ids: Vec<&str> = login.steps.iter().map(|s| s.step_id.as_str()).collect();
    let wire = fake.names();
    let wire_logout = wire.iter().filter(|n| n.contains("logOut")).count();
    println!(
        "FAKEDC logout: steps={ids:?} logouts={} outcome={:?} logout_s={:.2} logout_sent={} destroy_sent={} f_frames={} wire_logout={wire_logout} tdlib_left={} files_left={} dials={} tcp_traps={} dns_traps={} {}",
        logouts.len(),
        logouts.first().map(|r| r.outcome),
        logout_took.as_secs_f32(),
        count(&sent, 0),
        count(&sent, 1),
        count(&answered, 0),
        tdlib.exists(),
        entries_of(&media),
        dials.load(Ordering::SeqCst),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    if logouts.first().map(|r| r.outcome) != Some(kourier_model::DeviceLogout::Removed) {
        // The fake DC's constructor names after the login (names only) say what TDLib waited for.
        let at = wire
            .iter()
            .rposition(|n| n.contains("getMe") || n.contains("signIn"));
        println!(
            "FAKEDC logout: wire after the login {:?}",
            &wire[at.unwrap_or(0)..]
        );
    }
    net_end("logout", traps, &fake, dials.load(Ordering::SeqCst), None);
    case_dir("logout", dir);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(ids, ["phone", "code", "linking", "done"], "not logged in");
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(logouts.len(), 1, "not exactly one logout report");
    assert_eq!(
        logouts[0].outcome,
        kourier_model::DeviceLogout::Removed,
        "a logout after Ready that TDLib finished is not `Removed`"
    );
    assert!(wire_logout >= 1, "no `auth.logOut` reached the fake DC");
    assert_eq!(count(&sent, 0), 1, "the actor did not send `logOut` once");
    assert_eq!(count(&answered, 0), 0, "the helper wrote an F frame");
    assert!(
        logout_took < Duration::from_secs(8),
        "the logout took 8 s or more"
    );
    assert!(!tdlib.exists(), "tdlib/ is left after the logout");
    assert_eq!(entries_of(&media), 0, "the files directory is left");
}

/// A4 (P-2, critic H1(b)): the login by code, then the helper KILLED (SIGKILL) at `done`, while
/// TDLib still writes what the login brought (binlog, SQLite); the actor restarts it on the SAME
/// `tdlib/` (its clock moved past the back-off) and TDLib, under the filesystem rules, reopens
/// what the kill left (a torn binlog tail is truncated there) and comes back Online: two helpers,
/// no F frame (no fatal), 0 trap hits.
fn case_kill_restart(traps: &Traps) {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, _, answered, pids) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &[],
        &["F ", "authorizationStateReady"],
    );
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock.clone(), &rt, dir.path(), real_timings());
    let (login, online, killed) = rt.block_on(async {
        let login = login_by_code(&mut ends, started).await;
        let first = pids.lock().expect("pids").last().copied();
        let killed = first.is_some_and(kill_pid);
        // The restart waits on the actor's clock: move it past every back-off step.
        let mut online = false;
        let by = StdInstant::now() + Duration::from_secs(60);
        let mut now = T0;
        while StdInstant::now() < by {
            now += 6_000;
            clock.set(now);
            // TG-3 C7b (L-6): Online follows the core's ack of the start sync's rows.
            while let Ok(envelope) = ends.events.try_recv() {
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
            let state = ends.conn.borrow().clone();
            if pids.lock().expect("pids").len() >= 2
                && matches!(state, kourier_model::ConnState::Online { .. })
            {
                online = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        ends.commands.send(Command::Shutdown).await.expect("queued");
        (login, online, killed)
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let hits = traps.hits();
    let opens = traps.opens();
    let helpers = pids.lock().expect("pids").len();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let ids: Vec<&str> = login.steps.iter().map(|s| s.step_id.as_str()).collect();
    println!(
        "FAKEDC kill-restart: steps={ids:?} killed={killed} helpers={helpers} online_again={online} f_frames={} ready_frames={} total_s={:.1} dials={} tcp_traps={} dns_traps={} {}",
        count(&answered, 0),
        count(&answered, 1),
        started.elapsed().as_secs_f32(),
        dials.load(Ordering::SeqCst),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    net_end(
        "kill-restart",
        traps,
        &fake,
        dials.load(Ordering::SeqCst),
        None,
    );
    case_dir("kill-restart", dir);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(ids, ["phone", "code", "linking", "done"], "not logged in");
    assert!(killed, "the helper was not killed");
    assert!(helpers >= 2, "no second helper after the kill");
    assert!(online, "TDLib did not come back Online after the restart");
    assert_eq!(count(&answered, 0), 0, "a helper wrote an F frame");
    assert_eq!(exit, Exit::Shutdown);
}

/// [`actor_timed`] with a chosen start mode and account uuid (G2's second actor resumes the
/// first one's account on the same directories).
fn actor_mode(
    parts: TgParts,
    clock: Arc<kourier_backend_api::FakeClock>,
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    (start, uuid): (kourier_backend_api::StartMode, AccountUuid),
) -> (
    std::thread::JoinHandle<kourier_backend_api::Exit>,
    kourier_backend_api::CoreEnds,
) {
    use kourier_backend_api::{AccountContext, ContextParams};
    use kourier_model::AccountId;

    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start,
        backend_db: dir.join("backend.db"),
        db_key: kourier_backend_api::DbKey::new([7; 32]),
        blob_dir: dir.join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock,
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

/// A connection state's name (G2's timeline).
fn conn_name(c: &kourier_model::ConnState) -> &'static str {
    use kourier_model::ConnState;
    match c {
        ConnState::Offline { .. } => "offline",
        ConnState::Connecting => "connecting",
        ConnState::Syncing { .. } => "syncing",
        ConnState::Online { .. } => "online",
        _ => "other",
    }
}

/// G2's observer: the events of one envelope in the timeline (acked).
fn note_events(envelope: kourier_backend_api::Envelope, timeline: &mut Vec<&'static str>) {
    use kourier_backend_api::BackendEvent;
    for event in envelope.events {
        match event {
            BackendEvent::AccountHold(None) => timeline.push("hold-none"),
            BackendEvent::AccountHold(Some(_)) => timeline.push("hold-some"),
            BackendEvent::Login(_) => timeline.push("login-step"),
            _ => {}
        }
    }
    if let Some(ack) = envelope.ack {
        ack.complete(Ok(()));
    }
}

/// G2 (brief §3.9): the LINKED restart through the actor. G1's login by code and `Shutdown`, then
/// a second actor (`Resume`, the same account and directories) gets `Connect`: TDLib's first
/// authorization state after the parameters is Ready (no `WaitPhoneNumber`, no login request),
/// `AccountHold(None)` comes before the first `Syncing`/`Online`, the actor's connection states
/// (a watch: a short one may be folded) in the order Connecting -> Syncing -> Online and TDLib's
/// own `connectionState*` only going forward and ending Ready (H-M6),
/// no F frame, 0 trap hits.
fn case_linked_restart(traps: &Traps) {
    use kourier_backend_api::{Command, Exit, FakeClock, StartMode};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    let dials = Arc::new(AtomicUsize::new(0));
    let uuid = AccountUuid::new_random();
    let rt = rt();
    let (parts, _, _, _) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &[],
        &[],
    );
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_mode(
        parts,
        clock.clone(),
        &rt,
        dir.path(),
        (StartMode::Fresh, uuid),
    );
    let login = rt.block_on(async {
        let login = login_by_code(&mut ends, started).await;
        ends.commands.send(Command::Shutdown).await.expect("queued");
        login
    });
    let first_exit = actor.join().expect("actor");
    let ids: Vec<&str> = login.steps.iter().map(|s| s.step_id.as_str()).collect();
    // The second actor: the account is linked, its `tdlib/` holds the session.
    let tape: rec::TapeRef = Arc::default();
    tape.lock().expect("tape").on = true;
    let (parts, sent, answered, pids) = fakedc_parts_taped(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &[
            "setAuthenticationPhoneNumber",
            "requestQrCodeAuthentication",
            "checkAuthenticationCode",
        ],
        &[
            "\"authorizationStateWaitPhoneNumber\"",
            "\"authorizationStateReady\"",
            "F ",
            "\"connectionStateConnecting\"",
            "\"connectionStateUpdating\"",
            "\"connectionStateReady\"",
        ],
        Some(tape.clone()),
    );
    let resumed_at = StdInstant::now();
    let (actor, mut ends) = actor_mode(
        parts,
        clock.clone(),
        &rt,
        dir.path(),
        (StartMode::Resume, uuid),
    );
    let (timeline, online_at) = rt.block_on(async {
        ends.commands.send(Command::Connect).await.expect("queued");
        let mut timeline: Vec<&'static str> = vec![conn_name(&ends.conn.borrow())];
        let mut online_at = None;
        let mut quiet_until = None;
        let by = tokio::time::Instant::now() + Duration::from_secs(45);
        loop {
            let until = quiet_until.unwrap_or(by).min(by);
            tokio::select! {
                biased;
                e = tokio::time::timeout_at(until, ends.events.recv()) => match e {
                    Ok(Some(envelope)) => note_events(envelope, &mut timeline),
                    _ => break,
                },
                // TG-3 C8: the start sync's `loadChats` pages are paced on the actor's (fake)
                // clock; it moves on by the pace while the actor is idle.
                () = tokio::time::sleep(Duration::from_millis(50)) => {
                    clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
                }
                c = ends.conn.changed() => {
                    if c.is_err() {
                        break;
                    }
                    // The actor queues an event BEFORE it changes the state (the hold, then the
                    // state): what is queued belongs before the state seen now. (A run showed
                    // Syncing ahead of the hold when the two channels were read in the other
                    // order, 2026-10-27, lane run 1.)
                    while let Ok(envelope) = ends.events.try_recv() {
                        note_events(envelope, &mut timeline);
                    }
                    let name = conn_name(&ends.conn.borrow_and_update());
                    if timeline.last() != Some(&name) {
                        timeline.push(name);
                    }
                    if name == "online" && online_at.is_none() {
                        online_at = Some(resumed_at.elapsed());
                        // 1 s of what follows Online (a second hold would show here).
                        quiet_until = Some(tokio::time::Instant::now() + Duration::from_secs(1));
                    }
                }
            }
        }
        ends.commands.send(Command::Shutdown).await.expect("queued");
        (timeline, online_at)
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let hits = traps.hits();
    let opens = traps.opens();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let helpers = pids.lock().expect("pids").len();
    println!(
        "FAKEDC linked-restart: login={ids:?} timeline={timeline:?} online_s={:?} helpers={helpers} numbers_sent={} qr_sent={} codes_sent={} wait_phone_frames={} ready_frames={} f_frames={} td_connecting={} td_updating={} td_ready={} total_s={:.1} tcp_traps={} dns_traps={} {}",
        online_at.map(|d| d.as_secs_f32()),
        count(&sent, 0),
        count(&sent, 1),
        count(&sent, 2),
        count(&answered, 0),
        count(&answered, 1),
        count(&answered, 2),
        count(&answered, 3),
        count(&answered, 4),
        count(&answered, 5),
        started.elapsed().as_secs_f32(),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    net_end(
        "linked-restart",
        traps,
        &fake,
        dials.load(Ordering::SeqCst),
        None,
    );
    case_dir("linked-restart", dir);
    let at = |name: &str| timeline.iter().position(|t| *t == name);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(first_exit, Exit::Shutdown);
    assert_eq!(ids, ["phone", "code", "linking", "done"], "not logged in");
    assert_eq!(helpers, 1, "not exactly one helper for the resumed account");
    assert!(online_at.is_some(), "the resumed account never came Online");
    assert_eq!(
        count(&answered, 0),
        0,
        "TDLib asked for a phone number again"
    );
    assert!(count(&answered, 1) >= 1, "TDLib's state never became Ready");
    assert_eq!(count(&answered, 2), 0, "the helper wrote an F frame");
    assert_eq!(
        (count(&sent, 0), count(&sent, 1), count(&sent, 2)),
        (0, 0, 0),
        "the resumed account sent a login request"
    );
    assert!(
        at("login-step").is_none(),
        "the resumed account showed a login step"
    );
    assert_eq!(
        timeline.iter().filter(|t| t.starts_with("hold")).count(),
        1,
        "the hold was not stated exactly once"
    );
    let hold = at("hold-none").expect("no AccountHold(None)");
    let first_sync = at("syncing").or(at("online")).expect("no Syncing/Online");
    assert!(
        hold < first_sync,
        "the hold came after the first Syncing/Online"
    );
    // The actor's states come through a watch (the latest value): a short Connecting or Syncing
    // may be folded (seen in a run: ["offline", "hold-none", "online"]). What the watch shows is
    // in the order Offline, Connecting, Syncing, Online, ending Online. TDLib's own states on the
    // relay (H-M6): TDLib folds an Updating shorter than 50 ms and a worse state shorter than
    // 300 ms (`StateManager`), so which states show is timing; what must hold is that they only
    // go forward (Connecting* -> Updating? -> Ready) and END in Ready.
    let order = ["offline", "connecting", "syncing", "online"];
    let ranks: Vec<usize> = timeline
        .iter()
        .filter_map(|t| order.iter().position(|o| o == t))
        .collect();
    assert!(
        ranks.windows(2).all(|w| w[0] < w[1]) && ranks.last() == Some(&3),
        "the connection states are not in the order Connecting -> Syncing -> Online"
    );
    let td_states: Vec<usize> = rec::values(&tape)
        .iter()
        .filter(|(_, v)| v["@type"] == "updateConnectionState")
        .filter_map(|(_, v)| match v["state"]["@type"].as_str()? {
            "connectionStateWaitingForNetwork" => Some(0),
            "connectionStateConnectingToProxy" => Some(1),
            "connectionStateConnecting" => Some(2),
            "connectionStateUpdating" => Some(3),
            "connectionStateReady" => Some(4),
            _ => None,
        })
        .collect();
    println!(
        "FAKEDC linked-restart: tdlib connection states (0 waiting, 1 proxy, 2 connecting, 3 updating, 4 ready) {td_states:?}"
    );
    // Up to the FIRST Ready only: a later getDifference may show Updating again (the re-check's
    // watch item), and that is no step back of the restart.
    let first_ready = td_states.iter().position(|s| *s == 4);
    assert!(
        first_ready.is_some_and(|r| td_states[..=r].windows(2).all(|w| w[0] <= w[1]))
            && td_states.last() == Some(&4),
        "TDLib's connection states went back before Ready or did not end in Ready: {td_states:?}"
    );
    assert_eq!(exit, Exit::Shutdown);
}

/// G3 (brief §3.9, moved to C5 by P-13): the QR login through the actor: `StartLogin` qr ->
/// ONE `requestQrCodeAuthentication` -> a `qr` step with a link -> the control's `scan` (the fake
/// DC pushes `updateLoginToken`, TDLib exports again and gets the authorization) -> Ready ->
/// `getMe` -> ONE `AccountReady` with `[tg:<own id>, pn:<the fake's QR phone>]` -> `done`. The
/// link itself is never printed.
fn case_qr_login(traps: &Traps) {
    use kourier_backend_api::{BackendEvent, Command, Exit, FakeClock};
    use kourier_model::{LoginStepKind, UserId};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    fake.control("world alone");
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, sent, answered, _) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &[
            "requestQrCodeAuthentication",
            "setAuthenticationPhoneNumber",
            "\"getMe\"",
        ],
        &["F refused-request"],
    );
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let (actor, mut ends) = actor_timed(parts, clock, &rt, dir.path(), real_timings());
    let (steps, links, readies, scanned) = rt.block_on(async {
        ends.commands
            .send(Command::StartLogin {
                flow: "qr".to_owned(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("queued");
        let (mut steps, mut readies) = (Vec::new(), Vec::new());
        let (mut links, mut scanned) = (0usize, false);
        let by = tokio::time::Instant::now() + Duration::from_secs(90);
        'run: while let Ok(Some(envelope)) = tokio::time::timeout_at(by, ends.events.recv()).await
        {
            for event in envelope.events {
                match event {
                    BackendEvent::Login(s) => {
                        let link = matches!(&s.kind, LoginStepKind::ShowQr { data, .. } if !data.expose().is_empty());
                        if link {
                            links += 1;
                        }
                        let end = s.step_id == step::DONE
                            || matches!(s.kind, LoginStepKind::Failed { .. });
                        steps.push(s.step_id.clone());
                        if link && !scanned {
                            fake.control("scan");
                            scanned = true;
                        }
                        if end {
                            if let Some(ack) = envelope.ack {
                                ack.complete(Ok(()));
                            }
                            break 'run;
                        }
                    }
                    BackendEvent::AccountReady { own_ids, .. } => readies.push(own_ids),
                    _ => {}
                }
            }
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
        ends.commands.send(Command::Shutdown).await.expect("queued");
        (steps, links, readies, scanned)
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let stats = fake.control("stats");
    let hits = traps.hits();
    let opens = traps.opens();
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let wire = fake.names();
    let exports = wire
        .iter()
        .filter(|n| n.contains("exportLoginToken"))
        .count();
    println!(
        "FAKEDC qr-login: steps={steps:?} links={links} scanned={scanned} readies={} qr_requests={} numbers_sent={} get_me={} refused={} wire_exports={exports} total_s={:.1} dials={} tcp_traps={} dns_traps={} {}",
        readies.len(),
        count(&sent, 0),
        count(&sent, 1),
        count(&sent, 2),
        count(&answered, 0),
        started.elapsed().as_secs_f32(),
        dials.load(Ordering::SeqCst),
        hits.0,
        hits.1,
        opens_line(opens)
    );
    net_end("qr-login", traps, &fake, dials.load(Ordering::SeqCst), None);
    case_dir("qr-login", dir);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        steps.first().map(String::as_str),
        Some(step::QR),
        "not a QR step first"
    );
    assert_eq!(
        steps.last().map(String::as_str),
        Some(step::DONE),
        "the QR login did not end done"
    );
    assert_eq!(links, 1, "not exactly one QR link before the scan's end");
    assert_eq!(
        count(&sent, 0),
        1,
        "`requestQrCodeAuthentication` was not sent exactly once"
    );
    assert_eq!(count(&sent, 1), 0, "the QR login sent a phone number");
    assert_eq!(count(&sent, 2), 1, "`getMe` was not sent exactly once");
    assert_eq!(count(&answered, 0), 0, "the helper refused a request");
    assert!(
        exports >= 2,
        "TDLib did not export the token again after the scan"
    );
    assert_eq!(readies.len(), 1, "not exactly one AccountReady");
    assert_eq!(
        readies[0],
        [
            UserId::from_parts("tg", &FAKE_OWN_ID.to_string()).expect("id"),
            UserId::from_parts("pn", QR_DIGITS).expect("id"),
        ],
        "the own ids are not the fake account's"
    );
    assert_eq!(stats["phone_requests"].as_u64(), Some(0));
}

/// TG-4 K1: the fake DC ALONE (no helper, no dial) speaks its read commands through the real
/// binary's control socket: `read-state` (numbers only, an unknown chat refused), `hold-reads
/// <on|off>`, `threshold common <pts>`, the stats' `read_requests`, and the wire log's `t` that
/// [`FakeDc::lines_timed`] keeps. The wire side of a read (TDLib's request, the answer) is
/// recorded by `rc13_read_act`.
fn case_control_reads(traps: &Traps) {
    let dir = tempfile::tempdir().expect("a case directory");
    let fake = start_fakedc(dir.path());
    fake.control("world standard");
    assert_eq!(
        fake.read_state("u201"),
        (1, 0),
        "the account's own message read its chat"
    );
    assert_eq!(fake.read_state("c401"), (0, 0));
    assert_eq!(
        fake.read_state("g301"),
        (8, 0),
        "the group's last message is the account's own"
    );
    assert_eq!(fake.try_control("read-state u999"), Err("no-chat".into()));
    assert_eq!(fake.try_control("read-state x1"), Err("bad-chat".into()));
    assert_eq!(fake.try_control("read-state"), Err("bad-chat".into()));
    assert_eq!(fake.control("stats")["read_requests"], 0);
    fake.control("hold-reads on");
    fake.control("hold-reads off");
    assert_eq!(
        fake.try_control("hold-reads maybe"),
        Err("bad-switch".into())
    );
    fake.control("queue threshold common 2");
    assert_eq!(
        fake.try_control("queue threshold common x"),
        Err("bad-number".into())
    );
    assert_eq!(
        fake.try_control("queue threshold u201 2"),
        Err("not-a-channel".into())
    );
    fake.control("queue threshold c401 5");
    fake.control("mark control-reads-a");
    std::thread::sleep(Duration::from_millis(30));
    fake.control("mark control-reads-b");
    let marks: Vec<(u64, String)> = fake
        .lines_timed()
        .into_iter()
        .filter(|(_, dir, _)| dir == "mark")
        .map(|(t, _, name)| (t, name))
        .collect();
    let [(ta, a), (tb, b)] = marks.as_slice() else {
        panic!("two marks in the wire log, got {}", marks.len());
    };
    assert_eq!(
        (a.as_str(), b.as_str()),
        ("control-reads-a", "control-reads-b")
    );
    assert!(
        *ta > 1_600_000_000_000 && *tb >= *ta + 25,
        "the log's time is ms since the epoch"
    );
    println!("FAKEDC control-reads: ok marks_apart_ms={}", tb - ta);
    let opens = traps.opens();
    assert_eq!(traps.hits(), (0, 0), "a connection went past the gate");
    assert_namespace_quiet("control-reads", opens, 0, 0);
}

// ----- G6: the receive, the read act and the re-assert through the actor (TG-4 K5b) -----

/// TDLib's id of the fake DC's message `n` (TDLib shifts a server id by 20 bits).
const fn td_id(n: i64) -> i64 {
    n << 20
}

/// The conversation of the private chat with user `user` (the lane's `tg:<id>`).
fn direct(user: i64) -> kourier_model::ConvKey {
    kourier_model::ConvKey::Direct(
        kourier_model::UserId::from_parts("tg", &user.to_string()).expect("id"),
    )
}

/// What G6's pump saw of the core's end: ids and counts, nothing that could be printed as a
/// value.
#[derive(Default)]
struct CoreSeen {
    /// Every `HistoryBatch`: (the conversation, its messages' ids), in order.
    batches: Vec<(kourier_model::ConvKey, Vec<i64>)>,
    /// Every live `Message`: (the conversation, its id, `catch_up`), in order.
    lives: Vec<(kourier_model::ConvKey, i64, bool)>,
    /// The commands the core was told are persisted, in order.
    persisted: Vec<i64>,
    /// The actor's connection is `Online` (read after the queued events were taken).
    online: bool,
}

impl CoreSeen {
    /// Takes the batches, the live messages and the persisted commands of one envelope and acks it.
    fn note(&mut self, envelope: kourier_backend_api::Envelope) {
        use kourier_backend_api::BackendEvent;
        for event in envelope.events {
            match event {
                BackendEvent::HistoryBatch { conv, messages, .. } => {
                    let ids = messages
                        .iter()
                        .map(|m| m.local.parse().expect("a numeric id"))
                        .collect();
                    self.batches.push((conv, ids));
                }
                BackendEvent::Message(m) => {
                    let id = m.local.parse().expect("a numeric id");
                    self.lives.push((m.conv.clone(), id, m.catch_up));
                }
                BackendEvent::CommandPersisted { cmd } => self.persisted.push(cmd.0),
                _ => {}
            }
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
    }

    /// The ids of every row (batches and live) delivered so far for `conv`.
    fn ids_of(&self, conv: &kourier_model::ConvKey) -> Vec<i64> {
        let batched = self.batches.iter().filter(|(c, _)| c == conv);
        let mut ids: Vec<i64> = batched.flat_map(|(_, ids)| ids.iter().copied()).collect();
        ids.extend(self.lives.iter().filter(|l| l.0 == *conv).map(|l| l.1));
        ids
    }
}

/// The core's side of the actor for up to `limit`: every envelope acked and noted, the connection
/// state read after the queued events (the actor queues an event BEFORE it changes the state), and
/// with `ticking` the actor's fake clock moved on by the lane's pace whenever it is idle for 50 ms
/// (G2: the chat load's pages and the restart's back-off wait on that clock). Returns at once when
/// `until` holds, or false at `limit` (or when the actor ended).
async fn pump_core(
    ends: &mut kourier_backend_api::CoreEnds,
    clock: &kourier_backend_api::FakeClock,
    seen: &mut CoreSeen,
    (ticking, limit): (bool, Duration),
    mut until: impl FnMut(&CoreSeen) -> bool,
) -> bool {
    let by = tokio::time::Instant::now() + limit;
    loop {
        while let Ok(envelope) = ends.events.try_recv() {
            seen.note(envelope);
        }
        seen.online = matches!(*ends.conn.borrow(), kourier_model::ConnState::Online { .. });
        if until(seen) {
            return true;
        }
        if tokio::time::Instant::now() >= by {
            return false;
        }
        match tokio::time::timeout(Duration::from_millis(50), ends.events.recv()).await {
            Ok(Some(envelope)) => seen.note(envelope),
            Ok(None) => return false,
            Err(_) if ticking => {
                clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
            }
            Err(_) => {}
        }
    }
}

/// A `SyncReadMarker` of command `cmd` for the private chat with `user` up to message `local`.
fn read_marker_for(user: i64, local: i64, cmd: i64) -> kourier_backend_api::Command {
    let conv = direct(user);
    let target = kourier_model::MsgTarget {
        conv: Some(conv.clone()),
        author: kourier_model::TargetAuthor::User(
            kourier_model::UserId::from_parts("tg", &user.to_string()).expect("id"),
        ),
        local: Arc::from(local.to_string()),
        alternates: Vec::new(),
    };
    kourier_backend_api::Command::SyncReadMarker {
        cmd: kourier_model::CmdId(cmd),
        conv,
        up_to: target.clone(),
        read: vec![target],
    }
}

/// The inbox read position the lane's store holds for the private chat `chat`
/// (`chat_state.read_inbox_max_id`), read over a side connection of the test.
fn stored_read(dir: &Path, chat: i64) -> i64 {
    let conn = crate::db::open(&dir.join("backend.db"), &DbKey::from_bytes([7; 32]))
        .expect("open the store");
    conn.query_row(
        "SELECT read_inbox_max_id FROM chat_state WHERE chat_id = ?1",
        [chat],
        |r| r.get(0),
    )
    .expect("the chat's state")
}

/// How many requests named `name` the fake DC logged (the `in` lines) after its first `from`
/// wire-log lines.
fn wire_in(fake: &FakeDc, from: usize, name: &str) -> usize {
    fake.lines()
        .iter()
        .skip(from)
        .filter(|(dir, n)| dir == "in" && n == name)
        .count()
}

/// G6 (TG-4 K5b, critic H2): the TG-3 receive and TG-4's read act, one-commit load and read
/// re-assert through the REAL actor, the real helper and the test-only library on the fake DC's
/// standard world. Nothing here is a recording: the facts about TDLib are the recordings'
/// (RC1, RC13); this proves that the actor's own requests and rows are what they say, end to end.
///
/// 1. The login by code goes on into the fresh link's start sync: u201 (three unread messages, the
///    last one counted in them) reaches the core as ONE `HistoryBatch` of its two unread messages
///    below the last one and the last message (ids 2..4, the last one NOT ahead of the page; the
///    read message 1 stays out, U5), no chat comes twice, nothing live, and the account is
///    `Online` after the acks.
/// 2. `push new u201`: ONE live `Message` (not a catch-up row), and still one after a quiet second.
/// 3. A `SyncReadMarker` for u201 up to that message: `CommandPersisted` once, ONE `viewMessages`
///    sent, ONE `messages.readHistory` on the wire, `read-state u201` raised to exactly that id
///    (MODEL: the fake stores what it is told), and the store holds the position.
/// 4. `hold-reads on`, `push new u202`, a marker for it: persisted, the store holds the position,
///    the held request is on the wire and never answered; SIGKILL of the helper; `hold-reads off`
///    (MODEL: the fake's mark for u202 stays below).
/// 5. The restart on the same directories comes `Online`; TDLib tells u202 at the fake's older mark,
///    the lane's re-assert sends ONE `viewMessages` and the fake gets ONE `messages.readHistory`
///    (`read-state u202` raised to the id: K4's `lost_reads`); u201 is not re-asserted (TDLib told
///    its raised mark); a second of quiet brings no second request, and no id the fake did not
///    already hand out reaches the core as a row.
///
/// Golden: that each pushed message reaches the core once, that the actor's `viewMessages` is
/// accepted and becomes ONE read request, that the restart replays nothing of its own. MODEL: the
/// fake's read state and its read answers. Offline, 0 trap hits, no `phone.*` request.
fn case_reads_through_actor(traps: &Traps) {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let started = StdInstant::now();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    fake.control("world standard");
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, sent, answered, pids) = fakedc_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        &["\"viewMessages\"", "\"getChatHistory\""],
        &["F "],
    );
    let rt = rt();
    // The actor's clock starts at the wall clock: the fake DC dates its messages by it, and the
    // message machine calls a message catch-up when it is dated 5 minutes before the helper's
    // start (`T0` would put every message of this case in that past).
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(T0);
    let clock = Arc::new(FakeClock::new(wall));
    let (actor, mut ends) = actor_timed(parts, clock.clone(), &rt, dir.path(), real_timings());
    let count = |s: &Sent, i: usize| s[i].load(Ordering::SeqCst);
    let (u201, u202) = (direct(201), direct(202));
    let mut seen = CoreSeen::default();

    let (login, synced) = rt.block_on(async {
        let login = login_by_code(&mut ends, started).await;
        // 1. The fresh link's start sync: Online after the core acked every row.
        let synced = pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (true, Duration::from_secs(90)),
            |s| s.online,
        )
        .await;
        (login, synced)
    });
    let ids: Vec<&str> = login.steps.iter().map(|s| s.step_id.as_str()).collect();
    assert_eq!(ids, ["phone", "code", "linking", "done"], "not logged in");
    assert!(synced, "the fresh link's start sync never came Online");
    let sync_batches = seen.batches.len();
    let sync_convs = {
        let mut convs: Vec<&kourier_model::ConvKey> = seen.batches.iter().map(|b| &b.0).collect();
        convs.sort();
        convs.dedup();
        convs.len()
    };
    let history_asked = count(&sent, 1);
    // A message is (its conversation, its id): a channel counts its ids from 1 like a chat.
    let mut all: Vec<(&kourier_model::ConvKey, i64)> = seen
        .batches
        .iter()
        .flat_map(|(c, ids)| ids.iter().map(move |id| (c, *id)))
        .collect();
    all.sort_unstable();
    let rows_at_sync = all.len();
    all.dedup();
    // The unread chat's last message and its page are ONE batch, in id order (K5: the last
    // message is not delivered ahead of the page).
    let u201_batches: Vec<Vec<i64>> = seen
        .batches
        .iter()
        .filter(|(c, _)| *c == u201)
        .map(|(_, ids)| ids.clone())
        .collect();
    assert_eq!(
        u201_batches,
        [(2..=4).map(td_id).collect::<Vec<i64>>()],
        "u201's start load is not ONE batch of its unread page and its last message"
    );
    assert_eq!(
        sync_batches, sync_convs,
        "a chat reached the core in more than one batch"
    );
    // Every other chat has at most one unread message: its last message alone, at once.
    assert!(
        seen.batches
            .iter()
            .filter(|(c, _)| *c != u201)
            .all(|(_, ids)| ids.len() == 1),
        "a chat with at most one unread message delivered more than its last message"
    );
    assert_eq!(
        seen.ids_of(&u202),
        [td_id(5)],
        "u202's start load is not its one message"
    );
    assert_eq!(rows_at_sync, all.len(), "a message reached the core twice");
    assert!(seen.lives.is_empty(), "a live message in a quiet world");

    // 2. One live message.
    fake.control("push new u201");
    let (live1, quiet_one) = rt.block_on(async {
        let got = pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(20)),
            |s| !s.lives.is_empty(),
        )
        .await;
        // A second of quiet: the message must not come again.
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(1)),
            |_| false,
        )
        .await;
        (got, seen.lives.len() == 1)
    });
    assert!(live1, "the pushed message did not reach the core");
    assert!(
        quiet_one,
        "the pushed message reached the core more than once"
    );
    let (conv, l1, catch_up) = seen.lives[0].clone();
    assert_eq!(conv, u201, "the live message is not u201's");
    assert!(l1 > td_id(4), "the live message is not newer than the page");
    assert!(!catch_up, "a live message after Online came as catch-up");

    // 3. The read act: one request on the wire, the position raised, persisted, stored.
    let wire_mark = fake.lines().len();
    let views_before = count(&sent, 0);
    rt.block_on(async {
        ends.commands
            .send(read_marker_for(201, l1, 7))
            .await
            .expect("queued");
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(20)),
            |s| s.persisted == [7],
        )
        .await;
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(20)),
            |_| fake.read_state("u201").0 == l1 >> 20,
        )
        .await;
    });
    assert_eq!(
        seen.persisted,
        [7],
        "the read marker was not persisted once"
    );
    assert_eq!(
        fake.read_state("u201"),
        (l1 >> 20, 0),
        "the fake's read mark of u201 is not the pushed message's"
    );
    assert_eq!(
        count(&sent, 0) - views_before,
        1,
        "the read act of u201 was not ONE viewMessages"
    );
    assert_eq!(
        wire_in(&fake, wire_mark, "messages.readHistory"),
        1,
        "the read act of u201 was not ONE messages.readHistory"
    );
    assert_eq!(
        stored_read(dir.path(), 201),
        l1,
        "the store does not hold u201's confirmed read position"
    );

    // 4. A read the helper loses: held on the wire, the helper killed after the `ok`.
    fake.control("hold-reads on");
    fake.control("push new u202");
    let l2 = rt.block_on(async {
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(20)),
            |s| s.lives.len() == 2,
        )
        .await;
        let l2 = seen.lives.get(1).map(|l| (l.0.clone(), l.1));
        let (conv, id) = l2.expect("the message pushed into u202 did not reach the core");
        assert_eq!(conv, u202, "the second live message is not u202's");
        ends.commands
            .send(read_marker_for(202, id, 8))
            .await
            .expect("queued");
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(20)),
            |s| s.persisted == [7, 8],
        )
        .await;
        // The held request is on the wire (it is logged before it is held) when the kill comes.
        let mut waited = 0;
        while wire_in(&fake, wire_mark, "messages.readHistory") < 2 && waited < 200 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        id
    });
    assert_eq!(
        seen.persisted,
        [7, 8],
        "the marker of u202 was not persisted"
    );
    assert_eq!(
        stored_read(dir.path(), 202),
        l2,
        "the store does not hold u202's confirmed read position"
    );
    assert_eq!(
        wire_in(&fake, wire_mark, "messages.readHistory"),
        2,
        "the held read of u202 never reached the wire"
    );
    let killed = pids
        .lock()
        .expect("pids")
        .last()
        .copied()
        .is_some_and(kill_pid);
    assert!(killed, "the helper was not killed");
    fake.control("hold-reads off");
    let below = fake.read_state("u202").0;
    assert!(
        below < l2 >> 20,
        "the fake applied the held read of u202: {below}"
    );

    // 5. The restart: Online again, ONE re-assert for u202 and none for u201.
    let wire_restart = fake.lines().len();
    let views_at_kill = count(&sent, 0);
    let (batches_at, lives_at) = (seen.batches.len(), seen.lives.len());
    let (back, reasserted, quiet_after) = rt.block_on(async {
        let back = pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (true, Duration::from_secs(90)),
            |s| s.online && pids.lock().expect("pids").len() >= 2,
        )
        .await;
        let reasserted = pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(30)),
            |_| fake.read_state("u202").0 == l2 >> 20,
        )
        .await;
        // A second of quiet: no second request, no row.
        pump_core(
            &mut ends,
            &clock,
            &mut seen,
            (false, Duration::from_secs(1)),
            |_| false,
        )
        .await;
        let quiet_after = (
            count(&sent, 0) - views_at_kill,
            wire_in(&fake, wire_restart, "messages.readHistory"),
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
        (back, reasserted, quiet_after)
    });
    let exit = actor.join().expect("actor");
    std::thread::sleep(Duration::from_millis(500));
    let helpers = pids.lock().expect("pids").len();
    let (u201_after, u202_after) = (fake.read_state("u201"), fake.read_state("u202"));
    // Every row the core got after the restart is one it already had (a repeat the core drops).
    let had: std::collections::HashSet<(kourier_model::ConvKey, i64)> = seen.batches[..batches_at]
        .iter()
        .flat_map(|(c, ids)| ids.iter().map(move |id| (c.clone(), *id)))
        .chain(seen.lives[..lives_at].iter().map(|l| (l.0.clone(), l.1)))
        .collect();
    let after: Vec<(kourier_model::ConvKey, i64)> = seen.batches[batches_at..]
        .iter()
        .flat_map(|(c, ids)| ids.iter().map(move |id| (c.clone(), *id)))
        .chain(seen.lives[lives_at..].iter().map(|l| (l.0.clone(), l.1)))
        .collect();
    let rows_after = after.len();
    let new_rows = after.iter().filter(|row| !had.contains(row)).count();
    let stats = fake.control("stats");
    let hits = traps.hits();
    let opens = traps.opens();
    println!(
        "FAKEDC reads-through-actor: start_batches={sync_batches} start_convs={sync_convs} start_rows={rows_at_sync} get_history_sent={history_asked} u201_batches={} lives={} killed={killed} helpers={helpers} restart_views={} restart_reads={} rows_after_restart={rows_after} new_rows_after_restart={new_rows} f_frames={} read_requests={} total_s={:.1} tcp_traps={} dns_traps={}",
        u201_batches.len(),
        seen.lives.len(),
        quiet_after.0,
        quiet_after.1,
        count(&answered, 0),
        stats["read_requests"],
        started.elapsed().as_secs_f32(),
        hits.0,
        hits.1,
    );
    net_end(
        "reads-through-actor",
        traps,
        &fake,
        dials.load(Ordering::SeqCst),
        None,
    );
    case_dir("reads-through-actor", dir);
    assert_eq!(hits, (0, 0), "a connection went past the gate");
    assert_eq!(opens.datagrams(), 0, "a datagram left the namespace");
    assert!(back, "the restarted helper never came Online");
    assert_eq!(helpers, 2, "not exactly two helpers (one kill)");
    assert!(
        reasserted,
        "the restart did not raise u202's read mark (no re-assert reached the fake DC)"
    );
    assert_eq!(u202_after, (l2 >> 20, 0), "u202's mark after the re-assert");
    assert_eq!(
        u201_after,
        (l1 >> 20, 0),
        "u201's mark changed after the restart"
    );
    assert_eq!(
        quiet_after,
        (1, 1),
        "the restart did not send exactly ONE viewMessages and ONE messages.readHistory"
    );
    assert_eq!(seen.persisted, [7, 8], "the re-assert confirmed a command");
    assert_eq!(
        new_rows, 0,
        "a row the core had not got reached it after the restart"
    );
    // U8 (durability F4): each message ONCE, not only "no new one": the restart's start sync
    // ships nothing it shipped before (14 recorded runs printed `rows_after_restart=0`).
    assert_eq!(
        rows_after, 0,
        "the restart delivered a row the core had already got"
    );
    assert_eq!(
        stats["read_requests"].as_u64(),
        Some(3),
        "the fake DC did not count exactly three read requests (u201's, u202's held one, the re-assert)"
    );
    assert_eq!(count(&answered, 0), 0, "a helper wrote an F frame");
    assert_eq!(
        stats["phone_requests"].as_u64(),
        Some(0),
        "a phone.* request reached the fake DC"
    );
    assert_eq!(exit, Exit::Shutdown);
}

/// The entries of a directory (0 when it is gone).
fn entries_of(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| d.count())
}

/// The offline child's body: refuses to run outside the namespace, arms the traps, runs the case.
#[test]
fn fakedc_child() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    assert!(
        in_offline_child(),
        "not in the offline child: the test-only library is not loaded"
    );
    let traps = Traps::arm();
    println!(
        "FAKEDC traps armed: ipv6={} resolvers={}",
        traps.v6(),
        resolvers().len()
    );
    let _ = std::io::stdout().flush();
    match case.as_str() {
        "login" => case_login(&traps),
        "wrong-helper" => case_wrong_helper(&traps),
        "logout" => case_logout(&traps),
        "kill-restart" => case_kill_restart(&traps),
        "linked-restart" => case_linked_restart(&traps),
        "qr-login" => case_qr_login(&traps),
        "control-reads" => case_control_reads(&traps),
        "reads-through-actor" => case_reads_through_actor(&traps),
        other => assert!(
            rec::run(other, &traps) || tp::run(other, &traps),
            "no such case"
        ),
    }
}

// ----- the tests (parent halves) -----

/// G1 (brief §3.9): the login by code through the actor over the real helper, the test-only
/// library and the fake DC reaches Ready, one `AccountReady`, `done`; offline, 0 trap hits.
#[test]
fn the_fake_dc_logs_in_by_code_through_the_actor() {
    fakedc_case("login");
}

/// G5 (brief §3.9): the application's parts refuse the test-only library at the hello
/// (`WRONG_HELPER`), before any client.
#[test]
fn the_real_parts_refuse_the_test_only_library() {
    fakedc_case("wrong-helper");
}

/// A4 / G4: a logout after Ready under the filesystem rules removes everything, under 8 s.
#[test]
fn the_fake_dc_logout_after_ready_removes_everything() {
    fakedc_case("logout");
}

/// A4: a helper killed right after the login is restarted on the same `tdlib/` and comes back
/// Online, no fatal under the filesystem rules.
#[test]
fn the_fake_dc_login_survives_a_kill_and_a_restart() {
    fakedc_case("kill-restart");
}

/// G2 (brief §3.9): the linked restart through the actor resumes without a login step.
#[test]
fn the_fake_dc_resumes_a_linked_account() {
    fakedc_case("linked-restart");
}

/// G3 (brief §3.9): the QR login through the actor, scanned by the fake DC's control socket.
#[test]
fn the_fake_dc_logs_in_by_qr() {
    fakedc_case("qr-login");
}

/// G6 (TG-4 K5b, critic H2): the receive, the read act and the read re-assert through the actor
/// over the real helper and the test-only library: the fresh link's one-commit load, one live
/// message once, a read act as ONE request, and a read lost to a killed helper sent again once.
#[test]
fn the_fake_dc_receives_reads_and_reasserts_through_the_actor() {
    fakedc_case("reads-through-actor");
}
