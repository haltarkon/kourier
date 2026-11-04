//! T-8: the account actor over the REAL client (W2, chunk C4b), without a server.
//!
//! Every body runs in ONE offline child under `unshare -rn` (lane decision L3): an empty network
//! namespace, on top of guards that refuse every dial and request and count them, so the client the
//! actor builds can reach nothing. The bodies run one after the other; the guards' counters are
//! process-wide, so each case compares them with their value at its start.
//!
//! What this proves, and what it cannot. It proves what the real client does when nothing answers:
//! the connect fails where Kourier watches the transport and the HTTP client, the account says
//! `Connecting` and then `Offline` (upstream's run loop publishes nothing for a failed connect),
//! upstream tries again by itself, `Disconnect` and `Connect` pause and resume it, one client is
//! made and never a second, and the shutdown ends with the store released. The server's side (a
//! pairing, a ban, a logout) needs the scripted fixture of chunk C5.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{
    AccountContext, BackendEvent, Command, ContextParams, CoreEnds, DbKey, Exit, Network,
    StartMode, SystemClock,
};
use kourier_model::{AccountId, AccountUuid, ConnState, DeviceLogout, LoginStepKind};
use kourier_whatsapp::{StoreOwner, WhatsAppNetwork};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wacore::store::traits::DeviceStore;

use crate::support::guards;
use crate::support::{self, TEST_KEY, db_path, key};

/// How long the client waits for a connection before a logout asks the server (tests only).
const LOGOUT_WAIT: Duration = Duration::from_millis(300);

/// Everything the guards saw so far: version fetches and dials.
fn attempts() -> usize {
    guards::http_calls() + guards::dials()
}

/// A directory with `backend.db` that holds a paired device (what a finished link leaves).
pub(crate) async fn paired_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_into(dir.path()).await;
    dir
}

async fn pair_into(dir: &Path) {
    let owner = StoreOwner::open(&db_path(dir), &key(), "wa-test").expect("open");
    owner
        .store()
        .save(&crate::device::fixture_device())
        .await
        .expect("a paired device");
    owner.close();
}

pub(crate) struct Running {
    pub(crate) commands: tokio::sync::mpsc::Sender<Command>,
    pub(crate) ends: CoreEnds,
    pub(crate) handle: kourier_backend_api::AccountHandle,
}

fn start_actor(dir: &Path, start: StartMode, made: &Arc<AtomicUsize>) -> Running {
    start_actor_waiting(dir, start, made, LOGOUT_WAIT)
}

/// As [`start_actor`], with the time a logout waits for a connection chosen.
fn start_actor_waiting(
    dir: &Path,
    start: StartMode,
    made: &Arc<AtomicUsize>,
    logout_wait: Duration,
) -> Running {
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start,
        backend_db: db_path(dir),
        db_key: DbKey::new(TEST_KEY),
        blob_dir: dir.join("blobs"),
        net_rt: tokio::runtime::Handle::current(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = WhatsAppNetwork::new(guards::counting_parts(made).with_logout_wait(logout_wait))
        .spawn(ctx)
        .expect("the actor starts");
    Running {
        commands: ends.commands.clone(),
        ends,
        handle,
    }
}

impl Running {
    pub(crate) async fn send(&self, command: Command) {
        self.commands.send(command).await.expect("send");
    }

    /// The exit, and how long the actor took to give it.
    pub(crate) async fn exit(&mut self) -> (Exit, Duration) {
        let started = Instant::now();
        let exited = tokio::time::timeout(Duration::from_secs(15), &mut self.handle.exited)
            .await
            .expect("the actor ends");
        let exit = Exit::from_channel(exited);
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        (exit, started.elapsed())
    }

    pub(crate) fn conn(&self) -> watch::Receiver<ConnState> {
        self.ends.conn.clone()
    }

    /// What the logout the core asked for reached (the one `LogoutReport` of the run).
    async fn logout_report(&mut self) -> DeviceLogout {
        loop {
            let envelope = tokio::time::timeout(Duration::from_secs(20), self.ends.events.recv())
                .await
                .expect("the logout is reported")
                .expect("an envelope");
            for event in envelope.events {
                if let BackendEvent::Logout(report) = event {
                    return report.outcome;
                }
            }
        }
    }
}

/// Every state the connection takes, in order, from now on (the watch channel coalesces, so a test
/// that polls it could miss the short `Connecting`).
pub(crate) fn record(conn: watch::Receiver<ConnState>) -> Arc<Mutex<Vec<ConnState>>> {
    let seen = Arc::new(Mutex::new(vec![conn.borrow().clone()]));
    let sink = seen.clone();
    let mut conn = conn;
    tokio::spawn(async move {
        while conn.changed().await.is_ok() {
            let state = conn.borrow_and_update().clone();
            sink.lock().expect("lock").push(state);
        }
    });
    seen
}

/// Waits (at most `secs`) until the recorded states hold `wanted` as a subsequence.
pub(crate) async fn wait_for_states(
    seen: &Arc<Mutex<Vec<ConnState>>>,
    secs: u64,
    wanted: &[ConnState],
) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let states = seen.lock().expect("lock").clone();
        let mut next = wanted.iter();
        let mut want = next.next();
        for state in &states {
            if want == Some(state) {
                want = next.next();
            }
        }
        if want.is_none() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the states never came: wanted {wanted:?}, saw {states:?} (http {}, dials {})",
            guards::http_calls(),
            guards::dials()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub(crate) const OFFLINE: ConnState = ConnState::Offline { retry_at_ms: None };

/// Waits until the guards have counted `more` attempts beyond `from`.
async fn wait_for_attempts(from: usize, more: usize, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while attempts() < from + more {
        assert!(
            Instant::now() < deadline,
            "the client made {} attempts, wanted {more} more",
            attempts() - from
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A resumed account makes ONE client at its first `Connect`, its connect fails (the guards refuse),
/// Kourier says `Offline` and the client tries again - upstream's loop publishes nothing for a failed
/// connect. The shutdown that follows releases the store at once.
async fn a_resumed_account_goes_offline_tries_again_and_shuts_down_cleanly() {
    let dir = paired_dir().await;
    let made = Arc::new(AtomicUsize::new(0));
    let from = attempts();
    let mut actor = start_actor(dir.path(), StartMode::Resume, &made);
    let seen = record(actor.conn());
    assert_eq!(made.load(Ordering::SeqCst), 0, "no client before Connect");
    actor.send(Command::Connect).await;

    // The connect fails (the guards refuse): Kourier sees it where it owns the seam and says so.
    wait_for_states(&seen, 10, &[ConnState::Connecting, OFFLINE]).await;
    assert_eq!(made.load(Ordering::SeqCst), 1);

    // The client tries again by itself (upstream's schedule): more than one attempt, still ONE client.
    wait_for_attempts(from, 3, 10).await;
    assert_eq!(made.load(Ordering::SeqCst), 1, "a second client was made");
    assert!(
        !seen
            .lock()
            .expect("lock")
            .iter()
            .any(|s| matches!(s, ConnState::NeedsUserAction(_))),
        "an unreachable network is not an outdated app"
    );

    actor.send(Command::Shutdown).await;
    let (exit, took) = actor.exit().await;
    assert_eq!(exit, Exit::Shutdown);
    assert!(
        took < Duration::from_secs(4),
        "the store was not released in time ({took:?}): the shutdown waited out its bound"
    );
    assert!(
        !dir.path().join("backend.db-wal").exists(),
        "the writer thread outlived the actor"
    );
}

/// `Disconnect` pauses the client (no attempt while paused, however long) and `Connect` resumes it.
async fn disconnect_pauses_the_client_and_connect_resumes_it() {
    let dir = paired_dir().await;
    let made = Arc::new(AtomicUsize::new(0));
    let mut actor = start_actor(dir.path(), StartMode::Resume, &made);
    actor.send(Command::Connect).await;
    let first = attempts();
    wait_for_attempts(first, 3, 10).await;

    actor.send(Command::Disconnect).await;
    // An attempt that was already under way may still be counted; after that, silence.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let paused_at = attempts();
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(attempts(), paused_at, "a paused client made an attempt");

    actor.send(Command::Connect).await;
    wait_for_attempts(paused_at, 1, 10).await;
    assert_eq!(
        made.load(Ordering::SeqCst),
        1,
        "Connect made a second client"
    );

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// A QR login makes the client at `StartLogin` (never earlier), shows no code while nothing
/// answers, and a cancelled login that paired nothing reports `NoDevice`.
async fn a_qr_login_makes_its_client_at_the_start_and_a_cancelled_one_pairs_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let made = Arc::new(AtomicUsize::new(0));
    let mut actor = start_actor(dir.path(), StartMode::Fresh, &made);
    let seen = record(actor.conn());
    assert_eq!(made.load(Ordering::SeqCst), 0);
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    wait_for_states(&seen, 10, &[ConnState::Connecting, OFFLINE]).await;
    assert_eq!(made.load(Ordering::SeqCst), 1);
    // No QR code: the server never spoke.
    while let Ok(envelope) = actor.ends.events.try_recv() {
        for event in &envelope.events {
            if let BackendEvent::Login(step) = event {
                assert!(
                    !matches!(step.kind, LoginStepKind::ShowQr { .. }),
                    "a QR code without a server"
                );
            }
        }
    }

    actor.send(Command::CancelLogin).await;
    actor.send(Command::Logout).await;
    let mut reported = None;
    while reported.is_none() {
        let envelope = tokio::time::timeout(Duration::from_secs(10), actor.ends.events.recv())
            .await
            .expect("the logout is reported")
            .expect("an envelope");
        for event in envelope.events {
            if let BackendEvent::Logout(report) = event {
                reported = Some(report.outcome);
            }
        }
    }
    assert_eq!(reported, Some(DeviceLogout::NoDevice));
    assert!(matches!(actor.exit().await.0, Exit::LoggedOut { .. }));
    assert_eq!(made.load(Ordering::SeqCst), 1);
}

/// `Logout` of an account whose client has tried and been refused (the guards refuse) waits for a connection and
/// reports `NotReached` - the device was not removed from the phone's list - then ends `LoggedOut`. It is not
/// "Removed" because a local stop happened.
///
/// THE CLIENT'S FIRST ATTEMPT is awaited BEFORE the logout is asked (a progress condition with a generous bound, not a
/// timing assumption): the logout's own wait for a connection is short here (`LOGOUT_WAIT`), and under load (the slow
/// set's strike of 2026-10-29, load ~26) the real client's first request reached the guards after that wait had ended,
/// the actor had reported `NotReached` and stopped the client, and the case judged an outcome nobody had tried to
/// reach. That a logout of an account with NO client makes one, and reports the same, is the kill test's second child
/// (`kill_child_logout_again`) and its first (`kill_child_logout_waits`, which waits for the first attempt the same
/// way).
async fn a_logout_that_cannot_reach_the_server_reports_not_reached() {
    let dir = paired_dir().await;
    let made = Arc::new(AtomicUsize::new(0));
    let from = attempts();
    let mut actor = start_actor(dir.path(), StartMode::Resume, &made);
    actor.send(Command::Connect).await;
    wait_for_attempts(from, 1, 60).await;
    actor.send(Command::Logout).await;
    assert_eq!(actor.logout_report().await, DeviceLogout::NotReached);
    let (exit, _) = actor.exit().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        made.load(Ordering::SeqCst),
        1,
        "the logout used the client that was there and made no second"
    );
}

/// The directory the kill test's two children share.
const KILL_DIR: &str = "WA_KILL_DIR";

/// Child 1 of the kill test: an account with a device is asked to log out, and the client waits
/// (the full 20 s the real session gives a connection) for a session the guards will not give. It
/// says so on stdout and then waits to be killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_child_logout_waits() {
    if !support::offline::in_child() {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var(KILL_DIR).expect("the shared directory"));
    pair_into(&dir).await;
    let made = Arc::new(AtomicUsize::new(0));
    let from = attempts();
    let actor = start_actor_waiting(&dir, StartMode::Resume, &made, Duration::from_secs(20));
    actor.send(Command::Logout).await;
    // The client was made and tried to connect: the logout is now waiting for a session. A PROGRESS condition with a
    // generous bound, not a deadline that load can pass (the slow set's strike 1 of 2026-10-29, many kill children side by
    // side: the real client's first request reached the guards after the old 15 s): the logout's own wait is the 20 s above
    // and the parent kills this child the moment it speaks, so nothing here depends on how soon the client tries.
    wait_for_attempts(from, 1, 60).await;
    println!("LOGOUT_IS_WAITING");
    tokio::time::sleep(Duration::from_secs(120)).await;
    drop(actor);
}

/// Child 2 of the kill test: the next start of the same account.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_child_logout_again() {
    if !support::offline::in_child() {
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var(KILL_DIR).expect("the shared directory"));
    let made = Arc::new(AtomicUsize::new(0));
    let mut actor = start_actor(&dir, StartMode::Resume, &made);
    actor.send(Command::Logout).await;
    // The device the killed run was removing is still there, so there is something to remove:
    // `NoDevice` would mean the kill had lost it.
    assert_eq!(actor.logout_report().await, DeviceLogout::NotReached);
    let (exit, _) = actor.exit().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
}

/// A kill while a logout waits for the server leaves the account as it was (the device is still in
/// the store, nothing half-written), and the next start can ask for the logout again - the core
/// does that for a removal whose outcome it never heard (Signal's way).
#[test]
fn a_kill_while_a_logout_waits_leaves_the_device_and_the_next_logout_goes_on() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    if !support::offline::unshare_or_skip("no actor runs on the real network") {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let mut first = support::offline::child_command(module_path!(), "kill_child_logout_waits")
        .env(KILL_DIR, dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child 1");
    let stdout = first.stdout.take().expect("piped");
    // Reads until the child says the logout waits; a child that dies first ends the stream.
    let waited = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains("LOGOUT_IS_WAITING"));
    first.kill().expect("kill child 1");
    let status = first.wait().expect("reap child 1");
    assert!(
        waited,
        "the first child never reached the wait ({status:?})"
    );
    assert!(
        !status.success(),
        "the first child was killed, not finished"
    );

    // Child 2 sees the same directory and logs out again.
    let out = support::offline::child_command(module_path!(), "kill_child_logout_again")
        .env(KILL_DIR, dir.path())
        .output()
        .expect("spawn child 2");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "the second child failed ({:?}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The parent: the cases run in the offline child below.
#[test]
fn the_real_client_runs_offline() {
    support::offline::run_child(module_path!(), "real_child");
}

/// The child (a no-op as an ordinary test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_child() {
    if !support::offline::in_child() {
        return;
    }
    eprintln!("a_resumed_account_goes_offline_tries_again_and_shuts_down_cleanly");
    a_resumed_account_goes_offline_tries_again_and_shuts_down_cleanly().await;
    eprintln!("disconnect_pauses_the_client_and_connect_resumes_it");
    disconnect_pauses_the_client_and_connect_resumes_it().await;
    eprintln!("a_qr_login_makes_its_client_at_the_start_and_a_cancelled_one_pairs_nothing");
    a_qr_login_makes_its_client_at_the_start_and_a_cancelled_one_pairs_nothing().await;
    eprintln!("a_logout_that_cannot_reach_the_server_reports_not_reached");
    a_logout_that_cannot_reach_the_server_reports_not_reached().await;
    // Nothing ever got through the guards: every attempt was refused.
    assert!(
        attempts() > 0,
        "the guards saw nothing: the client never tried"
    );
}
