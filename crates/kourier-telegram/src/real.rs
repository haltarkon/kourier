//! THE one module of this crate that builds the application's helper parts: the real helper
//! spawner (through `kourier_tdlib::spawn`, the one file that names a process command) and the
//! gate's dialer. Lazy: calling [`helper_parts`] starts nothing and opens nothing; an account
//! thread calls the maker when it first needs TDLib.
//!
//! Two dialers, chosen by a [`DialMode`] the application passes (TR R0a, decisions D-11):
//!
//! - [`DialMode::Refuse`], the default of every build: the refusing dialer. Every dial is counted
//!   and refused, so a debug start with Telegram on keeps TDLib at "connecting to proxy"; nothing
//!   contacts Telegram (decisions D3 H2).
//! - [`DialMode::Telegram`]: the [`TcpDialer`], the ONLY non-test TCP connect of this crate (a
//!   source check keeps every other file free of one). Only the run tool's confirmation, read by the
//!   application in ONE function, may choose it; the parts then need a lock-down that is `on`.
//!
//! `kourier-app` is the only crate that calls [`helper_parts`].

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_tdlib::spawn::{SpawnArgs, spawn};

use crate::gate::dialer::{DialFuture, Dialer, GateStream, RefusingDialer, tcp_permission};
use crate::parts::{HelperFiles, HelperProcess, LockdownNeed, TgParts};

/// Which dialer the application's parts carry. The application chooses once per start, from the
/// run tool's confirmation; the name of [`DialMode::Telegram`] is written in exactly one place of
/// the application (a source check).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DialMode {
    /// Refuse every dial (counted, told once). The mode of every start that was not confirmed, and
    /// the `Default`: a test that needs "some mode" asks for the default and so names nothing.
    #[default]
    Refuse,
    /// Dial Telegram through [`TcpDialer`]: only inside the address list and its ports, and only
    /// with a lock-down that is `on`.
    Telegram,
}

impl DialMode {
    /// The mode as the one fixed word the application logs: `refusing` or `telegram`.
    pub const fn word(self) -> &'static str {
        match self {
            Self::Refuse => "refusing",
            Self::Telegram => "telegram",
        }
    }
}

/// The parts of the real application: the helper binary and the library from `files`, and the
/// dialer `mode` names. Builds nothing and runs nothing.
pub fn helper_parts(files: HelperFiles, mode: DialMode) -> TgParts {
    let (dialer, need, dials): (Arc<dyn Dialer>, LockdownNeed, bool) = match mode {
        DialMode::Refuse => (
            Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), true)),
            LockdownNeed::OnOrUnavailable,
            false,
        ),
        DialMode::Telegram => (
            Arc::new(TcpDialer::new(Arc::new(AtomicUsize::new(0)))),
            LockdownNeed::On,
            true,
        ),
    };
    let parts = TgParts::from_makers(
        move |helper| {
            spawn_helper(&SpawnArgs {
                helper: files.helper.clone(),
                tdjson: files.tdjson.clone(),
                no_dump: files.no_dump,
                gate_port: helper.gate_port,
                database_dir: helper.database_dir.clone(),
                files_dir: helper.files_dir.clone(),
            })
        },
        dialer,
    )
    // The real helper locks itself down; a hello that says otherwise is refused (G5), and the
    // parts that dial accept nothing but `on` (D-11).
    .requiring_lockdown(need);
    // Parts that dial are marked, and the supervisor refuses them with the production servers or
    // the test-only API pair (G-3: the crate's own belt, whatever the application decided).
    if dials { parts.dialing() } else { parts }
}

/// The dialer of [`DialMode::Telegram`]: a plain TCP connect, behind a second wall. The gate has
/// already checked the destination against the list; this checks it again, so a bug in the gate
/// (or a caller that skips it) still cannot open a socket to anywhere else. Logs nothing per dial;
/// the gate counts them.
pub(crate) struct TcpDialer {
    refused: Arc<AtomicUsize>,
}

impl TcpDialer {
    pub(crate) fn new(refused: Arc<AtomicUsize>) -> Self {
        Self { refused }
    }

    /// Whether a dial to `to` may open a socket ([`tcp_permission`]: the list, its ports and, in
    /// every test build, the offline tripwire). A refusal is counted and is `PermissionDenied`; no
    /// socket exists before this answers.
    fn permit(&self, to: SocketAddr) -> io::Result<()> {
        tcp_permission(to).inspect_err(|_| {
            self.refused.fetch_add(1, Ordering::SeqCst);
        })
    }
}

impl Dialer for TcpDialer {
    fn dial(&self, to: SocketAddr) -> DialFuture {
        if let Err(refusal) = self.permit(to) {
            return Box::pin(std::future::ready(Err(refusal)));
        }
        Box::pin(async move {
            let stream = tokio::net::TcpStream::connect(to).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream) as Box<dyn GateStream>)
        })
    }
}

/// Starts one helper process (socket pair on fd 3, environment cleared, stdin and stdout
/// `/dev/null`, stderr piped, other descriptors closed: `kourier_tdlib::spawn`).
fn spawn_helper(args: &SpawnArgs) -> io::Result<HelperProcess> {
    let spawned = spawn(args)?;
    Ok(HelperProcess {
        sock: spawned.sock,
        stderr: spawned
            .stderr
            .map(|e| Box::new(e) as Box<dyn io::Read + Send>),
        child: Box::new(spawned.child),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::path::PathBuf;

    use kourier_model::AccountUuid;
    use kourier_tdlib::spawn::in_offline_child;
    use tokio_util::sync::CancellationToken;
    use zeroize::Zeroizing;

    use super::*;
    use crate::api::ApiCredentials;
    use crate::gate::dialer::NOT_CONTACTED;
    use crate::gate::ranges;
    use crate::parts::TgEnv;
    use crate::supervisor::{AccountStart, Retry, StartError, Timings, start_reporting};

    /// Files that cannot exist: building parts looks at none of them.
    fn files() -> HelperFiles {
        HelperFiles {
            helper: PathBuf::from("/nonexistent/kourier-tdlib"),
            tdjson: PathBuf::from("/nonexistent/libtdjson.so"),
            no_dump: true,
        }
    }

    /// An address inside the first IPv4 block of the list, derived from the list itself (a test
    /// never writes a Telegram address).
    fn listed_v4() -> Ipv4Addr {
        Ipv4Addr::from_bits(ranges::TELEGRAM_V4[0].0.to_bits() + 1)
    }

    /// An address inside the first IPv6 block of the list.
    fn listed_v6() -> Ipv6Addr {
        Ipv6Addr::from_bits(ranges::TELEGRAM_V6[0].0.to_bits() + 1)
    }

    /// G5 and D-11: the application's parts require the helper's lock-down (a stale or test helper
    /// that reports none is refused); the refusing mode takes `unavailable` too, the mode that
    /// dials takes `on` only (T3). Mutant: the Telegram mode keeps `OnOrUnavailable`.
    #[test]
    fn the_telegram_mode_needs_a_lock_down_that_is_on() {
        assert_eq!(
            helper_parts(files(), DialMode::Refuse).lockdown_need(),
            LockdownNeed::OnOrUnavailable,
            "the refusing parts do not require the helper's lock-down"
        );
        assert_eq!(
            helper_parts(files(), DialMode::Telegram).lockdown_need(),
            LockdownNeed::On,
            "the parts that dial accept a lock-down that is not on"
        );
    }

    /// One start of `parts` for `env` and `api`, over files that cannot exist: the typed failure,
    /// and whether the start got as far as a gate (a failed start after the gate reports; one
    /// refused before it has no report). Nothing is dialled: the gate only listens on loopback, the
    /// maker fails with "not found" and the refused starts never reach either.
    fn start_once(parts: &TgParts, env: TgEnv, api: ApiCredentials) -> (Option<StartError>, bool) {
        let key = Zeroizing::new(String::from("a-key-that-is-never-used"));
        let account = AccountStart {
            uuid: AccountUuid::new_random(),
            key: &key,
            database_dir: PathBuf::from("/nonexistent/kourier-telegram-dials/tdlib"),
            files_dir: PathBuf::from("/nonexistent/kourier-telegram-dials/files"),
            env,
            api,
            device_model: "Kourier (synthetic-host)",
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let (started, report) = rt.block_on(start_reporting(
            parts,
            &account,
            rt.handle(),
            &Timings::default(),
            &CancellationToken::new(),
        ));
        (started.err(), report.is_some())
    }

    /// G-3, review F3: the parts that dial refuse Telegram's PRODUCTION servers in the supervisor's
    /// start, typed, BEFORE the gate starts (no report) and before the helper maker is called (the
    /// files cannot exist: a call would say `NotFound`), whatever the application decided. Tests
    /// hand a start the dummy pair only (re-check N-1: no other pair is built in a test), and the
    /// production check is judged first, so the dummy pair does not hide it. Mutants: the
    /// production check removed; the Telegram arm not marking its parts as dialing.
    #[test]
    fn the_parts_that_dial_refuse_the_production_servers() {
        let dialing = helper_parts(files(), DialMode::Telegram);
        let (err, gate_started) = start_once(&dialing, TgEnv::Production, ApiCredentials::DUMMY);
        assert_eq!(
            err,
            Some(StartError::DialsProduction),
            "dialing parts started for the production servers"
        );
        assert!(!gate_started, "a gate was started before the refusal");
        // The answer is the existing Never arm of the actor: no retry, not a wrong helper, not a
        // missing library, fixed words.
        let refusal = StartError::DialsProduction;
        assert_eq!(refusal.retry(), Retry::Never);
        assert!(!refusal.is_wrong_helper() && !refusal.is_missing_library());
        assert_eq!(
            refusal.to_string(),
            "this build does not dial Telegram's production servers"
        );
        // The test servers pass the production check (the next refusal is the pair's).
        let (err, gate_started) = start_once(&dialing, TgEnv::Test, ApiCredentials::DUMMY);
        assert_eq!(
            err,
            Some(StartError::DialsDummyKey),
            "the test servers were refused as production, or the dummy pair passed"
        );
        assert!(!gate_started, "a gate was started before the refusal");
        // Controls: parts that do not dial are not judged at all, and the start that is not
        // refused DOES reach the gate and then meets the missing helper (so "no gate" above is
        // read from a start that can start one).
        let refusing = helper_parts(files(), DialMode::Refuse);
        let (err, gate_started) = start_once(&refusing, TgEnv::Production, ApiCredentials::DUMMY);
        assert_eq!(
            err,
            Some(StartError::Spawn(io::ErrorKind::NotFound)),
            "parts that do not dial were judged for the production servers"
        );
        assert!(gate_started, "the control never reached the gate");
    }

    /// G-3, review F7: the parts that dial refuse the test-only API pair the same way. Production
    /// is judged first. Mutant: the dummy check removed.
    #[test]
    fn the_parts_that_dial_refuse_the_test_only_api_pair() {
        let dialing = helper_parts(files(), DialMode::Telegram);
        let (err, gate_started) = start_once(&dialing, TgEnv::Test, ApiCredentials::DUMMY);
        assert_eq!(
            err,
            Some(StartError::DialsDummyKey),
            "dialing parts started with the dummy pair"
        );
        assert!(!gate_started, "a gate was started before the refusal");
        let refusal = StartError::DialsDummyKey;
        assert_eq!(refusal.retry(), Retry::Never);
        assert!(!refusal.is_wrong_helper() && !refusal.is_missing_library());
        assert_eq!(
            refusal.to_string(),
            "this build does not dial Telegram with the test API key"
        );
        assert_eq!(
            start_once(&dialing, TgEnv::Production, ApiCredentials::DUMMY).0,
            Some(StartError::DialsProduction),
            "production is judged first"
        );
        let refusing = helper_parts(files(), DialMode::Refuse);
        assert_eq!(
            start_once(&refusing, TgEnv::Test, ApiCredentials::DUMMY).0,
            Some(StartError::Spawn(io::ErrorKind::NotFound)),
            "parts that do not dial were judged for the dummy pair"
        );
    }

    /// G-3: only the Telegram mode's parts dial; the refusing mode's and a test's never do.
    /// Mutant: the Telegram arm leaves its parts unmarked (the two tests above fail too).
    #[test]
    fn only_the_telegram_mode_marks_its_parts_as_dialing() {
        assert!(helper_parts(files(), DialMode::Telegram).dials());
        assert!(!helper_parts(files(), DialMode::Refuse).dials());
        assert!(!TgParts::refusing(Arc::default()).dials());
        assert_eq!(DialMode::default(), DialMode::Refuse);
    }

    /// The mode's word is what the application logs.
    #[test]
    fn the_modes_have_their_words() {
        assert_eq!(DialMode::Refuse.word(), "refusing");
        assert_eq!(DialMode::Telegram.word(), "telegram");
    }

    /// T1: the refusing mode dials nothing: every dial, also one inside the list, is refused with
    /// the refusing dialer's own words (so a TCP dialer behind this mode is seen, whatever its
    /// own answer would be). Building the parts starts nothing either. Mutant: the `Refuse` arm
    /// builds the TCP dialer.
    #[test]
    fn the_refusing_mode_refuses_every_dial() {
        let parts = helper_parts(files(), DialMode::Refuse);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        for to in [
            SocketAddr::from(([127, 0, 0, 1], 443)),
            SocketAddr::from(([10, 0, 0, 1], 443)),
            SocketAddr::new(IpAddr::V4(listed_v4()), 443),
            SocketAddr::new(IpAddr::V6(listed_v6()), 443),
        ] {
            let refusal = rt
                .block_on(parts.dialer().dial(to))
                .err()
                .expect("a dial got through");
            assert_eq!(
                refusal.kind(),
                io::ErrorKind::PermissionDenied,
                "the refusal is not a refusal"
            );
            assert_eq!(
                refusal.to_string(),
                NOT_CONTACTED,
                "something other than the refusing dialer answered"
            );
        }
    }

    /// The tripwire (critic 3): outside the offline child this crate's TCP dialer refuses even an
    /// address inside the list and counts it. Only the permission is asked, never a dial: with the
    /// tripwire gone the answer is `Ok` and the test fails without a socket. Mutant: the tripwire
    /// block of `tcp_permission` (`gate/dialer.rs`) removed.
    #[test]
    fn the_telegram_dialer_refuses_outside_the_offline_child() {
        assert!(
            !in_offline_child(),
            "this test is for the plain test process"
        );
        let refused = Arc::new(AtomicUsize::new(0));
        let dialer = TcpDialer::new(refused.clone());
        let to = SocketAddr::new(IpAddr::V4(listed_v4()), 443);
        let kind = dialer.permit(to).err().map(|e| e.kind());
        assert_eq!(
            kind,
            Some(io::ErrorKind::PermissionDenied),
            "a test dial outside the offline child was permitted"
        );
        assert_eq!(refused.load(Ordering::SeqCst), 1, "the refusal not counted");
    }

    /// T2: the TCP dialer in the offline child. The child holds AnyIP listeners on 443 AND 8443; the
    /// dialer connects to an address of the list on 443 and to nothing else.
    mod offline {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;

        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        use super::*;

        /// Set in the child (by the parent); an unset variable makes the child's test a no-op.
        const CASE_ENV: &str = "KOURIER_TG_DIALER_CHILD";
        /// `1`: a missing `unshare -rn` fails the test instead of skipping it.
        const REQUIRE_UNSHARE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";
        /// What the child prints when every assertion held.
        const MARKER: &str = "DIALER ok: connected=1 refused=5 accepted_443=1 accepted_8443=0";

        /// The child's path as `--exact` names it: this module's path without the crate's name.
        fn child_name() -> String {
            let here = module_path!();
            let inside = here.split_once("::").map_or(here, |(_, rest)| rest);
            format!("{inside}::the_dialer_child")
        }

        /// Whether `listener` has a connection waiting (one the dialer made and nobody accepted).
        fn waiting(listener: &TcpListener) -> bool {
            listener.set_nonblocking(true).expect("nonblocking");
            let waiting = listener.accept().is_ok();
            listener.set_nonblocking(false).expect("blocking");
            waiting
        }

        /// The parent half: the child under `unshare -rn`, its exit and its marker line.
        #[test]
        fn the_telegram_dialer_connects_only_inside_the_list() {
            if !kourier_tdlib::spawn::offline_available() {
                assert!(
                    !std::env::var(REQUIRE_UNSHARE_ENV).is_ok_and(|v| v == "1"),
                    "`unshare -rn` (or `ip`) is unavailable and {REQUIRE_UNSHARE_ENV}=1"
                );
                println!(
                    "SKIPPED: `unshare -rn` or `ip` is unavailable: no dial is made outside it"
                );
                return;
            }
            let run = kourier_tdlib::spawn::run_self_offline(
                &child_name(),
                &[(CASE_ENV, "1")],
                Duration::from_secs(60),
            )
            .expect("start the offline child");
            assert!(
                run.status.success() && run.output.contains("1 passed"),
                "the offline child failed ({:?}):\n{}",
                run.status,
                run.output
            );
            assert!(
                run.output.contains(MARKER),
                "the offline child did not reach its end:\n{}",
                run.output
            );
        }

        /// The child's body; a no-op in the plain test process.
        #[test]
        fn the_dialer_child() {
            if std::env::var_os(CASE_ENV).is_none() {
                return;
            }
            assert!(
                in_offline_child(),
                "not in the offline child: nothing is dialled"
            );
            let on_443 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 443)).expect("listener 443");
            let on_8443 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 8443)).expect("listener 8443");
            let refused = Arc::new(AtomicUsize::new(0));
            let dialer = TcpDialer::new(refused.clone());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let listed = SocketAddr::new(IpAddr::V4(listed_v4()), 443);

            // What the permission says, with no dial: the list and its ports, both families, the
            // mapped spelling of a listed IPv4 address (out) and the neighbours of the block.
            let block = ranges::TELEGRAM_V4[0];
            let outside = Ipv4Addr::from_bits(block.0.to_bits() - 1);
            for port in ranges::PORTS {
                for ip in [IpAddr::V4(listed_v4()), IpAddr::V6(listed_v6())] {
                    assert!(
                        dialer.permit(SocketAddr::new(ip, port)).is_ok(),
                        "a listed address on a listed port is refused"
                    );
                }
            }
            assert_eq!(refused.load(Ordering::SeqCst), 0);

            // Refused dials: the error KIND is a refusal, and no connection reached a listener.
            let mut refusals = 0;
            for to in [
                SocketAddr::from(([10, 0, 0, 1], 443)),
                SocketAddr::new(IpAddr::V4(outside), 443),
                SocketAddr::new(IpAddr::V6(listed_v4().to_ipv6_mapped()), 443),
                SocketAddr::new(IpAddr::V4(listed_v4()), 8443),
                SocketAddr::new(IpAddr::V4(listed_v4()), 22),
            ] {
                let verdict = rt.block_on(dialer.dial(to));
                assert_eq!(
                    verdict.err().map(|e| e.kind()),
                    Some(io::ErrorKind::PermissionDenied),
                    "a dial outside the list or its ports did not end in a refusal"
                );
                refusals += 1;
                assert_eq!(refused.load(Ordering::SeqCst), refusals);
            }
            assert!(!waiting(&on_443), "a refused dial reached the 443 listener");
            assert!(
                !waiting(&on_8443),
                "a refused dial reached the 8443 listener"
            );

            // The one dial that is allowed connects and carries bytes both ways.
            let mut stream = rt
                .block_on(dialer.dial(listed))
                .unwrap_or_else(|e| panic!("a listed dial failed: {:?}", e.kind()));
            let (mut peer, _) = on_443
                .accept()
                .expect("the listed dial reached the listener");
            rt.block_on(async {
                stream.write_all(b"ping").await.expect("write");
                let mut got = [0u8; 4];
                peer.read_exact(&mut got).expect("peer read");
                assert_eq!(&got, b"ping");
                peer.write_all(b"pong").expect("peer write");
                stream.read_exact(&mut got).await.expect("read");
                assert_eq!(&got, b"pong");
            });
            assert!(!waiting(&on_443), "one dial made two connections");
            assert!(
                !waiting(&on_8443),
                "the listed dial reached the 8443 listener"
            );
            drop((stream, peer));
            // A witness: the listeners are alive, so an empty queue above meant no connection.
            for (listener, port) in [(&on_443, 443u16), (&on_8443, 8443)] {
                let _probe = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("probe");
                assert!(
                    waiting(listener),
                    "a listener on {port} does not see a connection"
                );
            }
            assert_eq!(refused.load(Ordering::SeqCst), refusals);
            println!("{MARKER}");
        }
    }
}
