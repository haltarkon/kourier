//! The actor over a [`FakeFactory`] (W2, chunk C4a): no client, no socket. A test plays the
//! client through the [`Tap`] the factory kept, and the core through the account's channels.
//! Numbers and ids are the fictitious 555-01xx range; nothing names a person.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::Clock as _;
use kourier_backend_api::{
    AccountContext, AccountHandle, BackendConfig, BackendEnv, BackendEvent, BackendNotice, Command,
    ContextParams, CoreEnds, Envelope, Exit, FakeClock, LocalTestEnv, StartMode,
    spawn_account_thread,
};
use kourier_model::{
    AccountHold, AccountId, AccountUuid, BackendError, BanReason, ConnState, DeviceLogout,
    ErrorKind, HoldReason, LoginStep, LoginStepKind, LogoutCause, NetworkNote, Patch, Redacted,
    Restriction, UserAction, UserId,
};
use tokio_util::sync::CancellationToken;
use wacore::store::device::Device;
use wacore::store::traits::DeviceStore;

use super::run_with;
use crate::facts::Facts;
use crate::link::Ready;
use crate::session::fake::{Call, FactoryView, FakeFactory, FakeSession, Made};
use crate::states::WaState;
use crate::store::{KourierStore, StoreOwner};
use crate::translate::{Banned, HoldState, Logout, LogoutShape, Paired, RetryWhy, WaBan, WaSignal};

const KEY: [u8; 32] = [0x57; 32];
const PN: &str = "15550100123";
const LID: &str = "99900100123";
const NOW: u64 = 1_800_000_000_000;

/// What the actor told [`Facts`].
#[derive(Debug, Clone, PartialEq)]
enum Fact {
    State(WaState),
    Hold(Option<AccountHold>),
}

struct Recorder(Arc<Mutex<Vec<Fact>>>);

impl Facts for Recorder {
    fn state(&mut self, state: &WaState) {
        self.0.lock().unwrap().push(Fact::State(state.clone()));
    }

    fn hold(&mut self, hold: Option<AccountHold>) {
        self.0.lock().unwrap().push(Fact::Hold(hold));
    }
}

/// Whether `envelope` is the actor's statement of the reach-out lock (ephemeral `AccountHold`).
fn is_hold_statement(envelope: &Envelope) -> bool {
    envelope.ack.is_none() && matches!(envelope.events.as_slice(), [BackendEvent::AccountHold(_)])
}

/// A running actor with the core's ends of its channels.
struct H {
    ends: CoreEnds,
    handle: AccountHandle,
    view: FactoryView,
    facts: Arc<Mutex<Vec<Fact>>>,
    clock: FakeClock,
    dir: tempfile::TempDir,
}

impl H {
    fn start(mode: StartMode) -> Self {
        Self::start_in(tempfile::tempdir().expect("temp dir"), mode)
    }

    fn start_in(dir: tempfile::TempDir, mode: StartMode) -> Self {
        Self::start_with(dir, mode, BackendConfig::default())
    }

    fn start_with(dir: tempfile::TempDir, mode: StartMode, config: BackendConfig) -> Self {
        Self::start_with_clock(dir, mode, config, FakeClock::new(NOW))
    }

    fn start_with_clock(
        dir: tempfile::TempDir,
        mode: StartMode,
        config: BackendConfig,
        clock: FakeClock,
    ) -> Self {
        let cancel = CancellationToken::new();
        let (ctx, ends) = AccountContext::new(ContextParams {
            account: AccountId(1),
            uuid: AccountUuid::new_random(),
            start: mode,
            backend_db: dir.path().join("backend.db"),
            db_key: kourier_backend_api::DbKey::new(KEY),
            blob_dir: dir.path().join("blobs"),
            net_rt: tokio::runtime::Handle::current(),
            cancel: cancel.clone(),
            clock: Arc::new(clock.clone()),
            config,
            ui: Default::default(),
            local_prefs: Default::default(),
        });
        let (factory, view) = FakeFactory::new();
        let facts = Arc::new(Mutex::new(Vec::new()));
        let recorder = Recorder(facts.clone());
        let handle = spawn_account_thread("wa-test".to_owned(), move || {
            run_with(ctx, Box::new(factory), Box::new(recorder))
        })
        .expect("the actor starts");
        Self {
            ends,
            handle,
            view,
            facts,
            clock,
            dir,
        }
    }

    async fn send(&self, command: Command) {
        self.ends.commands.send(command).await.expect("send");
    }

    /// The next envelope that is not the hold's statement (the actor states the reach-out lock at every connect, D8;
    /// [`Self::raw_envelope`] sees it).
    async fn envelope(&mut self) -> Envelope {
        loop {
            let envelope = self.raw_envelope().await;
            if !is_hold_statement(&envelope) {
                return envelope;
            }
        }
    }

    async fn raw_envelope(&mut self) -> Envelope {
        tokio::time::timeout(Duration::from_secs(10), self.ends.events.recv())
            .await
            .expect("the actor sends")
            .expect("an envelope")
    }

    /// Whether nothing but the hold's statement reaches the core within `wait`.
    async fn quiet_for(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.ends.events.recv()).await {
                Err(_) => return true,
                Ok(Some(envelope)) if is_hold_statement(&envelope) => {}
                Ok(_) => return false,
            }
        }
    }

    /// The next envelope, which must be an ephemeral login step.
    async fn step(&mut self) -> LoginStep {
        let mut envelope = self.envelope().await;
        assert!(envelope.ack.is_none(), "{envelope:?}");
        match envelope.events.pop() {
            Some(BackendEvent::Login(step)) => step,
            other => panic!("not a login step: {other:?}"),
        }
    }

    /// The next envelope, which must be the durable `AccountReady`; it is acked.
    async fn ready(&mut self) -> BackendEvent {
        let mut envelope = self.envelope().await;
        let ack = envelope.ack.take().expect("a durable envelope");
        let event = envelope.events.pop().expect("an event");
        assert!(
            matches!(event, BackendEvent::AccountReady { .. }),
            "{event:?}"
        );
        ack.complete(Ok(()));
        event
    }

    /// The `n`-th client the factory made (0-based), once it exists.
    async fn client(&self, n: usize) -> Made {
        eventually(|| self.view.made().len() > n).await;
        self.view.made()[n].clone()
    }

    fn conn(&self) -> ConnState {
        self.ends.conn.borrow().clone()
    }

    /// The connection of `made` comes up the way upstream's does at the pin: the drain completes
    /// (nothing to wait for), then `Connected`; the account is `Online` when the machine says so.
    async fn go_online(&self, made: &Made) {
        made.tap.deliver(WaSignal::OfflineDone { count: 0 });
        made.tap.deliver(WaSignal::Connected);
        self.conn_is(ConnState::Online { detail: None }).await;
    }

    async fn conn_is(&self, want: ConnState) {
        let mut conn = self.ends.conn.clone();
        tokio::time::timeout(Duration::from_secs(10), conn.wait_for(|c| *c == want))
            .await
            .unwrap_or_else(|_| panic!("the state never became {want:?}, it is {:?}", self.conn()))
            .expect("the state channel");
    }

    fn facts(&self) -> Vec<Fact> {
        self.facts.lock().unwrap().clone()
    }

    async fn exit(self) -> Exit {
        self.exit_keeping_the_files().await.0
    }

    /// The exit, and the directory (the files stay until it is dropped).
    async fn exit_keeping_the_files(mut self) -> (Exit, tempfile::TempDir) {
        let exited = tokio::time::timeout(Duration::from_secs(10), &mut self.handle.exited)
            .await
            .expect("the actor ends");
        let exit = Exit::from_channel(exited);
        if let Some(thread) = self.handle.thread.take() {
            thread.join().expect("the actor's thread joins");
        }
        (exit, self.dir)
    }
}

/// Polls `cond` until it holds (10 s at most); a failure names the line that waited.
#[track_caller]
fn eventually(cond: impl Fn() -> bool) -> impl std::future::Future<Output = ()> {
    let at = std::panic::Location::caller();
    async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "it never happened (waited at {at})"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// A store with a paired device (what a finished link leaves).
async fn pair_device(dir: &Path) {
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-pair",
    )
    .expect("open");
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    owner.store().save(&device).await.expect("save");
    owner.close();
}

fn paired(pn: &str, lid: &str) -> WaSignal {
    WaSignal::PairSuccess(Paired {
        pn: Redacted::new(pn.to_owned()),
        lid: Redacted::new(lid.to_owned()),
        device: 3,
    })
}

fn qr(code: &str) -> WaSignal {
    WaSignal::QrCode {
        code: Redacted::new(code.to_owned()),
        valid_for: Duration::from_secs(60),
    }
}

fn qr_login() -> Command {
    Command::StartLogin {
        flow: "qr".to_owned(),
        relink: None,
        taken: Vec::new(),
    }
}

fn core_rows(dir: &Path) -> usize {
    let conn =
        kourier_db::open(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    usize::try_from(kourier_db::outbox::len(&conn).expect("count")).expect("count")
}

#[tokio::test]
async fn a_qr_link_is_done_only_after_the_core_acked_and_the_first_connect() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert_eq!(client.session.calls(), [Call::Start]);

    client.tap.deliver(qr("code-one"));
    let step = h.step().await;
    assert_eq!(step.step_id, "qr");
    assert!(matches!(step.kind, LoginStepKind::ShowQr { .. }));

    client.tap.deliver(paired(PN, LID));
    // The checks passed: a progress step, and AccountReady on the durable path (lid first).
    assert_eq!(h.step().await.step_id, "linking");
    let ready = h.ready().await;
    let BackendEvent::AccountReady {
        own_ids, device, ..
    } = ready
    else {
        unreachable!()
    };
    assert_eq!(
        own_ids,
        [
            UserId::from_parts("lid", LID).unwrap(),
            UserId::from_parts("pn", PN).unwrap()
        ]
    );
    assert_eq!(device, Some(3));
    // Acked, but not connected yet: not done.
    assert!(
        h.quiet_for(Duration::from_millis(200)).await,
        "done before the first connect"
    );
    client.tap.deliver(WaSignal::Connected);
    assert_eq!(h.step().await.step_id, "done");
    // A connection alone is `Syncing`: `Online` is the catch-up machine's, once the drain is done.
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn done_waits_for_the_ack_when_the_connect_comes_first() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    // The envelope is there but not acked yet; the connect comes first.
    let mut envelope = h.envelope().await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    // The drain is over, but the core has not acknowledged `AccountReady`, which was written before
    // it: the account is not `Online`, and the login is not done.
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    assert!(
        h.quiet_for(Duration::from_millis(200)).await,
        "done before the core acked AccountReady"
    );
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    envelope.ack.take().expect("durable").complete(Ok(()));
    assert_eq!(h.step().await.step_id, "done");
    h.conn_is(ConnState::Online { detail: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn a_refused_pairing_logs_out_once_records_nothing_and_the_logout_reports_it() {
    let mut h = H::start(StartMode::Fresh);
    h.send(Command::StartLogin {
        flow: "qr".to_owned(),
        relink: None,
        taken: vec![kourier_backend_api::TakenIdentity {
            user: UserId::from_parts("pn", PN).unwrap(),
            logged_out: false,
        }],
    })
    .await;
    let client = h.client(0).await;
    client.session.script_logout(DeviceLogout::NotReached);
    // Through the tap: the gate decides in the handler, before the actor has seen anything.
    assert_eq!(
        client.tap.deliver(paired(PN, LID)),
        crate::tap::Delivered::Refused
    );
    // Every later event of that client is dropped.
    assert_eq!(
        client.tap.deliver(WaSignal::Connected),
        crate::tap::Delivered::Dropped
    );
    let step = h.step().await;
    assert!(matches!(
        step.kind,
        LoginStepKind::Failed { ref error } if matches!(error.kind, ErrorKind::AlreadyAdded { .. })
    ));
    // The actor waits for the logout in its loop (not inside the signal) and stops the client when
    // it ends (review F6).
    eventually(|| client.session.count(&Call::Logout) == 1).await;
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    // The core's Logout follows a refused link: what the logout reached is reported, once.
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, DeviceLogout::NotReached)
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(client.session.count(&Call::Logout), 1, "a second logout");
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        core_rows(dir.path()),
        0,
        "a refused link recorded AccountReady"
    );
}

fn refused_link() -> Command {
    Command::StartLogin {
        flow: "qr".to_owned(),
        relink: None,
        taken: vec![kourier_backend_api::TakenIdentity {
            user: UserId::from_parts("pn", PN).unwrap(),
            logged_out: false,
        }],
    }
}

/// Review F6 / the lead's decision F3: the logout of a refused pairing is awaited by the actor's LOOP,
/// not inside the handling of the signal. With a logout that is slow to reach the server the actor
/// still serves a quit: it gives the logout a short grace, stops the client and exits.
#[tokio::test]
async fn a_slow_logout_of_a_refused_pairing_does_not_hold_up_a_quit() {
    let mut h = H::start(StartMode::Fresh);
    h.send(refused_link()).await;
    let client = h.client(0).await;
    let _held = client.session.hold_logouts();
    assert_eq!(
        client.tap.deliver(paired(PN, LID)),
        crate::tap::Delivered::Refused
    );
    assert!(matches!(h.step().await.kind, LoginStepKind::Failed { .. }));
    eventually(|| client.session.count(&Call::Logout) == 1).await;
    // The client is not stopped while its logout is under way.
    assert_eq!(client.session.count(&Call::Stop), 0);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
    assert_eq!(client.session.count(&Call::Stop), 1, "stopped at the end");
}

/// ...and the core's `Logout`, which follows a refused link, picks up what the slow logout reached
/// (here: the logout answers after the command arrived), once.
#[tokio::test]
async fn the_logout_command_picks_up_the_outcome_of_a_logout_still_under_way() {
    let mut h = H::start(StartMode::Fresh);
    h.send(refused_link()).await;
    let client = h.client(0).await;
    client.session.script_logout(DeviceLogout::Removed);
    let gate = client.session.hold_logouts();
    client.tap.deliver(paired(PN, LID));
    assert!(matches!(h.step().await.kind, LoginStepKind::Failed { .. }));
    eventually(|| client.session.count(&Call::Logout) == 1).await;
    assert_eq!(client.session.count(&Call::Stop), 0);
    h.send(Command::Logout).await;
    // The command waits for the logout; releasing it lets the report out.
    gate.notify_one();
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::Removed),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.session.count(&Call::Logout), 1, "one logout in all");
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    let (exit, _dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
}

/// W2c T-A1: a pairing the claim refused BEFORE its device existed. The claim's two steps are played by
/// hand (the verdict on the tap, then the signal): the login ends with the refusal's own failure, the client
/// is stopped, NOTHING is logged out (there is no device), and the core's `Logout` that follows finds none.
/// Mutant: the login kept as a refused one (the report waits for a logout that never comes).
#[tokio::test]
async fn a_claimed_refusal_ends_the_login_logs_nothing_out_and_the_logout_reports_no_device() {
    let mut h = H::start(StartMode::Fresh);
    h.send(refused_link()).await;
    let client = h.client(0).await;
    let stranger = Paired {
        pn: Redacted::new(PN.to_owned()),
        lid: Redacted::new(LID.to_owned()),
        device: 3,
    };
    assert!(matches!(
        client.tap.judge_pairing(&[Some(stranger.clone())]),
        crate::link::Verdict::Refuse(_)
    ));
    assert_eq!(
        client.tap.deliver_claimed(Some(stranger)),
        crate::tap::Delivered::Sent
    );
    let step = h.step().await;
    assert!(matches!(
        step.kind,
        LoginStepKind::Failed { ref error } if matches!(error.kind, ErrorKind::AlreadyAdded { .. })
    ));
    // The client is stopped at once (the login is over; there is no logout to wait for).
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    // The core's Logout follows a failed link: there is no device.
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::NoDevice),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.session.count(&Call::Logout), 0, "a logout was made");
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        core_rows(dir.path()),
        0,
        "a refused link recorded AccountReady"
    );
}

/// W2c T-A2 (critic F1): a second phone that scans a rotated code while the first pairing completes is
/// answered and goes no further. The login still finishes and the account still hears its connection.
/// Mutant: the second refusal sets the gate's flag (the `Connected` below is dropped for ever).
#[tokio::test]
async fn a_second_pairing_after_the_first_stood_does_not_end_the_login_or_deafen_the_account() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    let first = Paired {
        pn: Redacted::new(PN.to_owned()),
        lid: Redacted::new(LID.to_owned()),
        device: 3,
    };
    // The claim lets the first pairing through; upstream completes it.
    assert_eq!(
        client.tap.judge_pairing(&[Some(first)]),
        crate::link::Verdict::Pass
    );
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let mut envelope = h.envelope().await;
    // Another phone, another account.
    let second = Paired {
        pn: Redacted::new("15550100456".to_owned()),
        lid: Redacted::new("99900100456".to_owned()),
        device: 4,
    };
    assert!(matches!(
        client.tap.judge_pairing(&[Some(second)]),
        crate::link::Verdict::Refuse(_)
    ));
    envelope.ack.take().expect("durable").complete(Ok(()));
    assert_eq!(
        client.tap.deliver(WaSignal::Connected),
        crate::tap::Delivered::Sent
    );
    assert_eq!(h.step().await.step_id, "done");
    assert_eq!(client.session.count(&Call::Logout), 0);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// What the rules of a login with no tail and nothing taken ACCEPT.
fn accepted_by_the_rules() -> Paired {
    Paired {
        pn: Redacted::new(PN.to_owned()),
        lid: Redacted::new(LID.to_owned()),
        device: 3,
    }
}

/// W2c M1 (review), a quit: a QR login waits for its scan and the core stops the account. The gate of that
/// login is closed with the client's stop, so a `<pair-success>` of an identity the rules ACCEPT that is
/// still on its way is claimed and refused (`no-login`, nobody told, so nothing reaches the actor) instead
/// of completed into a device nobody holds. Mutant: `close_login_gate` with an empty body (actor.rs).
#[tokio::test]
async fn a_quit_while_a_login_waits_closes_its_gate_so_a_late_pairing_is_refused() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(qr("code-one"));
    assert_eq!(h.step().await.step_id, "qr");
    // While the login waits, the rules would take this phone.
    assert_eq!(
        client.tap.judge_pairing(&[Some(accepted_by_the_rules())]),
        crate::link::Verdict::Pass,
        "the control: an open gate lets this identity through"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
    assert_eq!(client.session.count(&Call::Stop), 1);
    let refused = crate::link::Refused::no_login();
    assert_eq!(refused.tell, crate::link::Tell::Nobody);
    assert_eq!(
        client.tap.judge_pairing(&[Some(accepted_by_the_rules())]),
        crate::link::Verdict::Refuse(refused)
    );
}

/// W2c M1 (review), a withdrawal: the person withdraws the login (the core's `Logout` with no device
/// yet). The gate is closed BEFORE the report goes out, so the verdict right behind the report is
/// `no-login` with nobody told. Mutant: `close_login_gate` with an empty body (actor.rs).
#[tokio::test]
async fn a_logout_that_withdraws_a_login_closes_its_gate_so_a_late_pairing_is_refused() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(qr("code-one"));
    assert_eq!(h.step().await.step_id, "qr");
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::NoDevice),
        other => panic!("{other:?}"),
    }
    let refused = crate::link::Refused::no_login();
    assert_eq!(refused.tell, crate::link::Tell::Nobody);
    assert_eq!(
        client.tap.judge_pairing(&[Some(accepted_by_the_rules())]),
        crate::link::Verdict::Refuse(refused)
    );
    assert_eq!(client.session.count(&Call::Logout), 0, "a logout was made");
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        core_rows(dir.path()),
        0,
        "a withdrawn login recorded AccountReady"
    );
}

fn relink_login(own: Vec<UserId>) -> Command {
    Command::StartLogin {
        flow: "qr".to_owned(),
        relink: Some(kourier_backend_api::Relink {
            account: AccountId(1),
            expect_own: own,
        }),
        taken: Vec::new(),
    }
}

/// Relink is a login on a fresh start (the core hands an empty database): the identity that pairs
/// must be one the account had (brief 3.6). The same phone relinks and finishes like a first link.
#[tokio::test]
async fn a_relink_by_the_same_account_finishes_like_a_link() {
    let mut h = H::start(StartMode::Fresh);
    h.send(relink_login(vec![
        UserId::from_parts("pn", PN).unwrap(),
        UserId::from_parts("lid", LID).unwrap(),
    ]))
    .await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let mut envelope = h.envelope().await;
    assert!(matches!(
        envelope.events.pop(),
        Some(BackendEvent::AccountReady { .. })
    ));
    envelope.ack.take().expect("durable").complete(Ok(()));
    client.tap.deliver(WaSignal::Connected);
    assert_eq!(h.step().await.step_id, "done");
    assert_eq!(client.session.count(&Call::Logout), 0);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// ...and another account's phone is logged out at once, nothing is recorded, and the core's
/// `Logout` that follows reports what that reached, once (critic H7, L4).
#[tokio::test]
async fn a_relink_by_another_account_is_logged_out_at_once_and_recorded_nowhere() {
    let mut h = H::start(StartMode::Fresh);
    h.send(relink_login(vec![
        UserId::from_parts("pn", "15550100999").unwrap(),
        UserId::from_parts("lid", "99900100999").unwrap(),
    ]))
    .await;
    let client = h.client(0).await;
    client.session.script_logout(DeviceLogout::Removed);
    assert_eq!(
        client.tap.deliver(paired(PN, LID)),
        crate::tap::Delivered::Refused
    );
    let step = h.step().await;
    assert!(
        matches!(
            step.kind,
            LoginStepKind::Failed { ref error } if error.kind == ErrorKind::InvalidInput
        ),
        "{step:?}"
    );
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::Removed),
        other => panic!("{other:?}"),
    }
    assert_eq!(client.session.count(&Call::Logout), 1, "a second logout");
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        core_rows(dir.path()),
        0,
        "a refused relink recorded AccountReady"
    );
}

#[tokio::test]
async fn shutdown_stops_the_client_while_the_store_is_open_and_reports_after() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.send(Command::Shutdown).await;
    let path = h.dir.path().join("backend.db-wal");
    let session = client.session.clone();
    let exit = h.exit().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(session.calls(), [Call::Start, Call::Pause, Call::Stop]);
    assert_eq!(
        session.store_open_at_stop(),
        [true],
        "the owner was closed before the client released the store"
    );
    assert!(!path.exists(), "the writer thread outlived the actor");
}

/// Collections whose version moved and whose flags nobody cleared (what a crash leaves).
async fn plant_dirty(dir: &Path, collections: &[(&str, u64)]) {
    use wacore::appstate::hash::HashState;
    use wacore::store::traits::AppSyncStore;
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-dirty",
    )
    .expect("open");
    for (name, version) in collections {
        let state = HashState {
            version: *version,
            ..HashState::default()
        };
        owner.store().set_version(name, state).await.expect("set");
    }
    owner.close();
}

async fn dirty_names(dir: &Path) -> Vec<String> {
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-dirty",
    )
    .expect("open");
    let names = owner
        .store()
        .dirty_collections()
        .await
        .expect("dirty")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    owner.close();
    names
}

/// Ledger rows planted before the actor starts: `(arrival, id, received_at)`, all finished.
async fn plant_ledger(dir: &Path, rows: &[(i64, &str, i64)]) {
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-ledger",
    )
    .expect("open");
    let rows: Vec<(i64, String, i64)> = rows
        .iter()
        .map(|(arrival, id, at)| (*arrival, (*id).to_owned(), *at))
        .collect();
    owner
        .store()
        .write("test: plant", move |tx| {
            for (arrival, id, at) in &rows {
                tx.execute(
                    "INSERT INTO inbox(arrival, chat, sender, id, origin, received_at, catch_up, state) \
                     VALUES (?1, '100@g.us', '200@lid', ?2, 'e2e', ?3, 0, 'done')",
                    kourier_db::rusqlite::params![arrival, id, at],
                )
                .map_err(crate::error::sql("plant"))?;
            }
            Ok(())
        })
        .await
        .expect("plant");
    owner.close();
}

fn ledger_ids(dir: &Path) -> Vec<String> {
    let conn =
        kourier_db::open(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    let mut stmt = conn
        .prepare("SELECT id FROM inbox ORDER BY arrival")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .expect("ids")
}

/// D7 through the actor: the connection that begins after the old rows were committed and whose drain
/// is complete and durable sweeps the ledger rows older than the floor; a young row stays. Nothing is
/// swept before that: the planted rows are still there while the connection is only `Syncing`.
#[tokio::test]
async fn a_later_connection_whose_drain_is_durable_sweeps_the_old_ledger_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let old = i64::try_from(NOW).expect("time") - crate::inbox::LEDGER_AGE_MS - 1;
    let young = i64::try_from(NOW).expect("time") - 1000;
    plant_ledger(
        dir.path(),
        &[
            (1, "old-one", old),
            (2, "young", young),
            (3, "old-two", old),
        ],
    )
    .await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    // `Connected` alone: the drain is not known to be over, so the connection only syncs.
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    assert_eq!(ledger_ids(h.dir.path()), ["old-one", "young", "old-two"]);
    // The drain's completion comes after `Connected` (a second `Connected` would be a new
    // connection's, fix F4).
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    eventually(|| ledger_ids(h.dir.path()) == ["young"]).await;
}

/// Rule (b), the stop point (fix F2, W6a1 critic B1): a clean quit pauses the client and stops it, and the flags the
/// store held when it OPENED stay: their resync never ran, and the flag is the only record that the mirror may miss
/// events. Kills "the quit uses the plain stop" (no pause), "the quit clears the flags" and "the owed set is not
/// kept" (the flags are gone).
#[tokio::test]
async fn a_clean_quit_before_the_resync_ran_keeps_the_flags_the_store_held_at_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    plant_dirty(dir.path(), &[("regular", 4), ("critical_block", 7)]).await;
    assert_eq!(dirty_names(dir.path()).await, ["critical_block", "regular"]);
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.send(Command::Shutdown).await;
    let session = client.session.clone();
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        session.calls(),
        [Call::Start, Call::Pause, Call::Stop],
        "never online: nothing was asked"
    );
    assert_eq!(
        dirty_names(dir.path()).await,
        ["critical_block", "regular"],
        "a quit vouches for no event the handler did not write"
    );
}

/// D4: all five collections are on the list, so a flag the SESSION set (an incremental sync) is cleared by a quiet
/// quit. Kills "the list stays empty" and "the owed set holds everything".
#[tokio::test]
async fn a_clean_quit_clears_the_flags_this_session_set() {
    let (h, client) = resumed_on_the_wall_clock().await;
    moved(&client, "regular", 3).await;
    assert_eq!(flags(&client).await, ["regular"]);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert!(dirty_names(dir.path()).await.is_empty());
}

/// A sync moves `name` to `version` on the client's store (what upstream's patch commit does): its flag is set.
async fn moved(made: &Made, name: &str, version: u64) {
    use wacore::appstate::hash::HashState;
    use wacore::store::traits::AppSyncStore;
    let state = HashState {
        version,
        ..HashState::default()
    };
    made.store.set_version(name, state).await.expect("set");
}

/// The dirty collections of the running account.
async fn flags(made: &Made) -> Vec<String> {
    made.store
        .dirty_collections()
        .await
        .expect("dirty")
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// `calls` without the reads the start makes at `Online` beside the resync (W6a2: the blocklist, B3; the groups' overview and
/// batch and a single query, B6), whose place among the calls of a quit is another test's business.
fn without_the_start_reads(calls: Vec<Call>) -> Vec<Call> {
    calls
        .into_iter()
        .filter(|call| {
            !matches!(
                call,
                Call::Blocklist | Call::Participating | Call::FetchGroups(_) | Call::FetchGroup
            )
        })
        .collect()
}

/// What the resync asked, collection by collection (the double records one call per request).
fn resyncs(made: &Made) -> Vec<Vec<String>> {
    made.session
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Resync(collections) => Some(collections),
            _ => None,
        })
        .collect()
}

/// A store that a kill left with flags, on the wall clock (the books are stamped by the actor's clock).
async fn killed_with(flags: &[(&str, u64)]) -> (H, Made) {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    plant_dirty(dir.path(), flags).await;
    let clock = FakeClock::new(u64::try_from(kourier_db::now_ms()).expect("a time"));
    let h = H::start_with_clock(dir, StartMode::Resume, BackendConfig::default(), clock);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    (h, client)
}

/// D2: the dirty set READ AT OPEN (what a kill left) is asked once `Online`: nothing before, one request per
/// collection in name order after, and the flag of each synced one goes. A reconnect asks nothing again in the same
/// start. Kills "asked before Online", "all in one request" and "asked at every Online".
#[tokio::test]
async fn what_a_kill_left_is_asked_once_after_online_and_never_again_in_that_start() {
    let (h, client) = killed_with(&[("regular", 4), ("critical_block", 7)]).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(resyncs(&client).is_empty(), "not before Online");

    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    eventually(|| resyncs(&client).len() == 2).await;
    assert_eq!(resyncs(&client), [["critical_block"], ["regular"]]);
    // The fake called both synced: the quiet look and the clear follow.
    for _ in 0..200 {
        if flags(&client).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(flags(&client).await.is_empty(), "synced = cleared");

    // The connection ends and comes back: nothing is asked a second time in this start.
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    eventually(|| !matches!(h.conn(), ConnState::Online { .. })).await;
    moved(&client, "regular", 9).await;
    client.tap.deliver(WaSignal::Connected);
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        resyncs(&client).len(),
        2,
        "a flag the first connection of this start set is no reason either"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Critic B1: a flag set in THIS session is no reason. An incremental sync in the catch-up marks its collection dirty
/// after the store opened; no snapshot follows it. Kills "the reason set read at Online".
#[tokio::test]
async fn an_incremental_sync_in_the_catch_up_asks_for_no_snapshot() {
    let (h, client) = resumed_on_the_wall_clock().await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    moved(&client, "regular_low", 3).await;
    moved(&client, "regular_high", 2).await;
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(resyncs(&client).is_empty());
    assert_eq!(flags(&client).await, ["regular_high", "regular_low"]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Critic B1 point 4: a fresh link asks no snapshot of Kourier's in its session, although the link's full sync moves
/// every collection (the flags are set when it ends), and it stamps the books. Kills "the reason set read at Online"
/// at the link and "the link stamps nothing".
#[tokio::test]
async fn a_fresh_link_asks_no_snapshot_in_its_session_and_stamps_the_books() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(qr("code-one"));
    let _ = h.step().await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let _ready = h.ready().await;
    client.tap.deliver(WaSignal::Connected);
    assert_eq!(h.step().await.step_id, "done");
    // The link's full sync: every collection moves.
    for (name, version) in [
        ("critical_block", 2),
        ("critical_unblock_low", 2),
        ("regular_high", 2),
        ("regular_low", 2),
        ("regular", 2),
    ] {
        moved(&client, name, version).await;
    }
    // The link's stamp alone would keep the snapshots away for 6 hours: the session runs longer than that, so it is
    // the rule that a flag of this session is no reason that does.
    h.clock.advance(Duration::from_secs(7 * 3600));
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(resyncs(&client).is_empty(), "the link session asks nothing");
    let stamped = client
        .store
        .read(|conn| crate::store::resyncs::book(conn, "regular"))
        .await
        .expect("books")
        .expect("a row: stamped at the link");
    assert!(stamped.asked_at > 0 && stamped.fatal_at.is_none());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// M7 through the actor: a collection the server answers fatally is recorded (`fatal_at`), keeps its flag and is
/// not asked again in this start; the one after it is still asked. Kills "`fatal_at` not recorded".
#[tokio::test]
async fn a_fatal_answer_is_recorded_and_the_next_collection_is_still_asked() {
    let (h, client) = killed_with(&[("regular", 4), ("critical_block", 7)]).await;
    // critical_block is asked first (name order): refused outright.
    client
        .session
        .script_resync(Ok(crate::appstate::ResyncReport {
            synced: Vec::new(),
            fatal: vec!["critical_block".into()],
        }));
    h.go_online(&client).await;
    eventually(|| resyncs(&client).len() == 2).await;
    for _ in 0..200 {
        if flags(&client).await == ["critical_block"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        flags(&client).await,
        ["critical_block"],
        "regular synced, the refused one keeps its flag"
    );
    let book = client
        .store
        .read(|conn| crate::store::resyncs::book(conn, "critical_block"))
        .await
        .expect("books")
        .expect("a row");
    assert!(book.fatal_at.is_some());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// F6 (durability review DUR-6): the link's books carry the time of the PAIRING, not of the moment the link page was
/// opened. A link page open for seven hours, a scan, a kill a minute later: the next start (eight hours after the
/// page was opened, one hour after the scan) asks no snapshot, because the books are within 6 hours of the SCAN.
/// Kills "the books stamped when the actor starts".
#[tokio::test]
async fn the_link_stamps_the_books_at_the_pairing_so_a_kill_after_the_scan_asks_nothing_for_six_hours()
 {
    let wall = u64::try_from(kourier_db::now_ms()).expect("a time");
    let mut h = H::start_with_clock(
        tempfile::tempdir().expect("temp dir"),
        StartMode::Fresh,
        BackendConfig::default(),
        FakeClock::new(wall),
    );
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(qr("code-one"));
    let _ = h.step().await;
    // The page stays open for seven hours; then the phone scans.
    h.clock.advance(Duration::from_secs(7 * 3600));
    let scan = i64::try_from(h.clock.now_ms()).expect("a time");
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let _ready = h.ready().await;
    let stamped = client
        .store
        .read(|conn| crate::store::resyncs::book(conn, "regular"))
        .await
        .expect("books")
        .expect("a row: stamped at the link");
    assert_eq!(
        stamped.asked_at, scan,
        "the books carry the time of the scan"
    );
    // The link's own time (F13: a call before it adds no chat) is the scan's too, kept apart from the books.
    let linked = client
        .store
        .read(crate::store::resyncs::linked_at)
        .await
        .expect("the link's time");
    assert_eq!(linked, Some(scan), "the pairing's time is the scan's");
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    // The next start, an hour after the scan (eight after the page was opened), finds the link session's flags (the
    // fake client wrote no device row; a real pairing has).
    pair_device(dir.path()).await;
    plant_dirty(dir.path(), &[("regular", 4), ("critical_block", 7)]).await;
    let clock = FakeClock::new(wall + 8 * 3600 * 1000);
    let h = H::start_with_clock(dir, StartMode::Resume, BackendConfig::default(), clock);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.go_online(&client).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        resyncs(&client).is_empty(),
        "an hour after the scan nothing is asked: {:?}",
        resyncs(&client)
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// F7 (durability review DUR-7): ONE quit budget. A quit with a resync that never answers AND a history chunk that
/// never arrives waits for both AT THE SAME TIME: while the capture wait (15 s on the actor's clock) is still open,
/// the resync's own wait (1 s in tests) runs out and the task is cut. Serial waits cut it only after the capture wait
/// ends. The cut is safe: the flag stands, the next start asks again. Kills "the waits added up again".
#[tokio::test]
async fn a_quit_waits_for_the_resync_beside_the_announced_chunk_not_after_it() {
    let (h, client) = killed_with(&[("regular", 4)]).await;
    let _gate = client.session.hold_resyncs();
    h.go_online(&client).await;
    eventually(|| resyncs(&client).len() == 1).await;
    announce(&client.store, "n1", h.clock.now_ms()).await;
    h.send(Command::Shutdown).await;
    bound_is_armed(&h).await;
    eventually(|| client.session.resyncs_cut() == 1).await;
    assert!(
        !ended(&h),
        "the capture wait is still open when the resync is cut"
    );
    h.clock.advance(Duration::from_secs(15));
    let session = client.session.clone();
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        without_the_start_reads(session.calls()),
        [
            Call::Start,
            Call::FetchHold,
            Call::Resync(vec!["regular".into()]),
            Call::Pause,
            Call::Stop
        ]
    );
    assert_eq!(
        dirty_names(dir.path()).await,
        ["regular"],
        "a replay cut by the quit leaves its flag"
    );
}

/// G6 (DUR-7's residual): when the quit's joined waits have used their 15 s (the announced chunk never came, the resync
/// never answered) less than one look window is left of the 20 s grace before the store's release, so the quiet look is
/// skipped and the flags stay: a collection whose flag was set during the session, which a look would have cleared,
/// keeps it. Without the skip the worst case ends past the grace. Kills "a look whatever is left".
#[tokio::test]
async fn a_quit_that_used_its_waits_has_no_time_for_the_quiet_look_and_keeps_the_flags() {
    let (h, client) = killed_with(&[("regular", 4)]).await;
    let _gate = client.session.hold_resyncs();
    h.go_online(&client).await;
    eventually(|| resyncs(&client).len() == 1).await;
    // A flag of this session (no reason for a resync, nothing owed for it): a quiet look would clear it.
    moved(&client, "critical_block", 9).await;
    assert_eq!(flags(&client).await, ["critical_block", "regular"]);
    announce(&client.store, "n1", h.clock.now_ms()).await;
    h.send(Command::Shutdown).await;
    bound_is_armed(&h).await;
    eventually(|| client.session.resyncs_cut() == 1).await;
    // The chunk never comes: the capture wait runs its whole 15 s.
    h.clock.advance(Duration::from_secs(15));
    let session = client.session.clone();
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        without_the_start_reads(session.calls()),
        [
            Call::Start,
            Call::FetchHold,
            Call::Resync(vec!["regular".into()]),
            Call::Pause,
            Call::Stop
        ]
    );
    assert_eq!(
        dirty_names(dir.path()).await,
        ["critical_block", "regular"],
        "no look, no clearing: both flags stay for the next start"
    );
}

/// M7: a quiet quit WAITS (bounded) for a resync that is running before it pauses the client, so a replay is not cut
/// in the middle: the resync is through, its flag gone, and only then the pause. Kills "the quit pauses at once".
#[tokio::test]
async fn a_quiet_quit_waits_for_the_resync_that_is_running() {
    let (h, client) = killed_with(&[("regular", 4)]).await;
    let gate = client.session.hold_resyncs();
    h.go_online(&client).await;
    eventually(|| resyncs(&client).len() == 1).await;
    h.send(Command::Shutdown).await;
    // The replay takes a moment; the quit is waiting for it, so the client is not paused yet.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!client.session.calls().contains(&Call::Pause));
    gate.notify_one();
    let session = client.session.clone();
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        without_the_start_reads(session.calls()),
        [
            Call::Start,
            Call::FetchHold,
            Call::Resync(vec!["regular".into()]),
            Call::Pause,
            Call::Stop
        ]
    );
    assert!(
        dirty_names(dir.path()).await.is_empty(),
        "the resync finished before the quit"
    );
}

/// The wait is bounded: a resync that never answers does not hold the quit. The task is ended, the client paused and
/// stopped, and the flag stands for the next start. Kills "an unbounded wait".
#[tokio::test]
async fn a_quiet_quit_does_not_wait_longer_than_its_bound_for_a_resync() {
    let (h, client) = killed_with(&[("regular", 4)]).await;
    let _gate = client.session.hold_resyncs();
    h.go_online(&client).await;
    eventually(|| resyncs(&client).len() == 1).await;
    h.send(Command::Shutdown).await;
    let session = client.session.clone();
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        without_the_start_reads(session.calls()),
        [
            Call::Start,
            Call::FetchHold,
            Call::Resync(vec!["regular".into()]),
            Call::Pause,
            Call::Stop
        ]
    );
    assert_eq!(
        dirty_names(dir.path()).await,
        ["regular"],
        "an unfinished resync leaves its flag"
    );
}

#[tokio::test]
async fn a_rebuild_states_account_ready_again_from_the_stored_device() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let mut h = H::start_in(dir, StartMode::Rebuild);
    let BackendEvent::AccountReady {
        own_ids,
        device,
        number,
        ..
    } = h.ready().await
    else {
        unreachable!()
    };
    assert_eq!(own_ids.len(), 2);
    assert_eq!(own_ids[0].scheme(), "lid");
    assert_eq!(own_ids[1].scheme(), "pn");
    assert_eq!(device, Some(3));
    assert_eq!(
        number.as_ref().map(|n| n.expose().as_str()),
        Some("+15550100123")
    );
    // No client was made for it.
    assert!(h.view.made().is_empty());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn a_row_a_previous_run_left_ships_at_the_next_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    // What a kill right after the append leaves: the row, not acked.
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-row",
        )
        .expect("open");
        let ready = Ready::of(
            UserId::from_parts("pn", PN).unwrap(),
            Some(UserId::from_parts("lid", LID).unwrap()),
            3,
        );
        crate::outbox::append_ready(&owner.store(), &ready, 1)
            .await
            .expect("append");
        owner.close();
    }
    assert_eq!(core_rows(dir.path()), 1);
    let mut h = H::start_in(dir, StartMode::Resume);
    h.ready().await;
    // Acked: the row is gone.
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(core_rows(dir.path()), 0);
}

#[tokio::test]
async fn an_inbox_row_a_previous_run_left_becomes_an_event_at_the_next_start_and_ships() {
    use crate::inbox::{Arrival, Inbox, InboxMeta, Origin};
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    // What a kill between the hook's commit and stage 2 leaves: a `new` row, no event.
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-inbox",
        )
        .expect("open");
        let mut message = waproto::whatsapp::Message::default();
        message.conversation = Some("synthetic text".to_owned());
        let inbox = Inbox::new(owner.store(), Arc::new(FakeClock::new(NOW)));
        inbox
            .commit(
                vec![Arrival {
                    chat: "15550100101@s.whatsapp.net".into(),
                    sender: "15550100101@s.whatsapp.net".into(),
                    id: "3EB0AA01".into(),
                    origin: Origin::E2e,
                    message: waproto::codec::message_to_vec(&message),
                    meta: InboxMeta {
                        v: InboxMeta::VERSION,
                        timestamp: 1_700_000_000,
                        from_me: false,
                        is_group: false,
                        push_name: String::new(),
                        lid_addressing: false,
                        sender_alt: None,
                        recipient_alt: None,
                        ephemeral_expiration: None,
                        is_offline: false,
                    },
                }],
                Vec::new(),
            )
            .await
            .expect("commit");
        owner.close();
    }
    assert_eq!(core_rows(dir.path()), 0);
    let mut h = H::start_in(dir, StartMode::Resume);
    // Stage 2 ran at the start; the forwarder was woken for its row and ships it.
    let mut envelope = h.envelope().await;
    let ack = envelope.ack.take().expect("a durable envelope");
    assert!(
        matches!(&envelope.events[..], [s, BackendEvent::Message(m)]
            if crate::features::is_statement(s) && &*m.local == "3EB0AA01"),
        "{envelope:?}"
    );
    ack.complete(Ok(()));
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(core_rows(dir.path()), 0, "acked rows are gone");
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let (done, wiped): (i64, i64) = conn
        .query_row(
            "SELECT count(*) FILTER (WHERE state = 'done'), count(*) FILTER (WHERE message IS NULL) FROM inbox",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("inbox");
    assert_eq!((done, wiped), (1, 1));
}

#[tokio::test]
async fn disconnect_pauses_connect_resumes_and_hints_do_not_revive_a_held_account() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    // A hint before any Connect makes no client.
    h.send(Command::NetworkChanged).await;
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.conn_is(ConnState::Connecting).await;
    h.send(Command::NetworkChanged).await;
    h.send(Command::Disconnect).await;
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    h.send(Command::NetworkChanged).await;
    h.send(Command::Resumed).await;
    h.send(Command::Connect).await;
    h.conn_is(ConnState::Connecting).await;
    h.send(Command::Resumed).await;
    h.send(Command::Shutdown).await;
    let session = client.session.clone();
    let view = h.view.clone();
    assert_eq!(h.exit().await, Exit::Shutdown);
    assert_eq!(
        session.calls(),
        [
            Call::Start,
            Call::ReconnectNow,
            Call::Pause,
            Call::Resume,
            Call::ReconnectNow,
            // A clean quit of a client that is running pauses it before the stop (rule (b),
            // `appstate::stop_quietly`); the earlier Pause was the Disconnect's.
            Call::Pause,
            Call::Stop
        ],
        "a pause is not a stop, and a hint never revives a held account"
    );
    assert_eq!(view.made().len(), 1);
}

#[tokio::test]
async fn a_terminal_verdict_releases_the_client_before_a_new_one_is_made() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let first = h.client(0).await;
    first.tap.deliver(WaSignal::Banned(Banned {
        ban: WaBan::SameMessageToMany,
        until_ms: NOW + 86_400_000,
        explanation: Some("WhatsApp's words".to_owned()),
        appeal_url: None,
    }));
    h.conn_is(ConnState::Restricted {
        until_ms: Some(NOW + 86_400_000),
        reason: "temporary ban 104".to_owned(),
        kind: Restriction::Banned(BanReason::SameMessageToMany),
        note: NetworkNote {
            explanation: Some("WhatsApp's words".to_owned()),
            appeal_url: None,
        },
    })
    .await;
    eventually(|| first.session.count(&Call::Stop) == 1).await;
    let facts = h.facts();
    assert!(facts.iter().any(|f| matches!(
        f,
        Fact::State(s) if s.terminal && matches!(
            &s.conn,
            ConnState::Restricted { kind: Restriction::Banned(BanReason::SameMessageToMany), .. }
        )
    )));
    // Hints do not revive it.
    h.send(Command::NetworkChanged).await;
    h.send(Command::Resumed).await;
    // The person asks again: a NEW client, made only after the first released the store.
    h.send(Command::Connect).await;
    let second = h.client(1).await;
    h.conn_is(ConnState::Connecting).await;
    let log = h.view.log();
    let stop_first = log
        .iter()
        .position(|e| *e == (0, Call::Stop))
        .expect("stop");
    let start_second = log
        .iter()
        .position(|e| *e == (1, Call::Start))
        .expect("start");
    assert!(
        stop_first < start_second,
        "two clients on one store: {log:?}"
    );
    assert_eq!(first.session.count(&Call::ReconnectNow), 0);
    h.go_online(&second).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn a_logout_from_the_network_ends_the_actor_and_one_we_asked_for_is_ignored() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.go_online(&client).await;
    // Our own logout is not an event: nothing changes.
    client.tap.deliver(WaSignal::LocalLogout);
    client
        .tap
        .deliver(WaSignal::WillRetry(RetryWhy::ServerError));
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    h.go_online(&client).await;
    client.tap.deliver(WaSignal::LoggedOut(Logout {
        shape: LogoutShape::RemovedByPhone,
        violation: None,
        explanation: None,
    }));
    let facts = h.facts.clone();
    let session = client.session.clone();
    assert!(matches!(h.exit().await, Exit::LoggedOut { .. }));
    assert!(facts.lock().unwrap().iter().any(|f| matches!(
        f,
        Fact::State(s) if matches!(
            &s.conn,
            ConnState::LoggedOut { cause: LogoutCause::RemovedOnPhone, .. }
        )
    )));
    assert_eq!(
        session.count(&Call::Stop),
        1,
        "the client was stopped on the way out"
    );
}

#[tokio::test]
async fn the_reach_out_lock_is_stated_at_every_connect_and_a_failed_pull_says_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    // The first pull fails, the second finds a lock.
    let scripted = Arc::new(FakeSession::new());
    scripted.script_hold(Err(BackendError::transient("no answer")));
    scripted.script_hold(Ok(HoldState::Active {
        until_ms: Some(NOW + 60_000),
        kind: None,
    }));
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| client.session.count(&Call::FetchHold) == 1).await;
    assert!(
        !h.facts().iter().any(|f| matches!(f, Fact::Hold(_))),
        "a failed pull stated something"
    );
    // The retry waits on the actor's clock.
    h.clock.advance(Duration::from_secs(6));
    eventually(|| client.session.count(&Call::FetchHold) == 2).await;
    let hold = Some(AccountHold {
        until_ms: Some(NOW + 60_000),
        reason: HoldReason::ReachOutLocked,
    });
    eventually(|| h.facts().contains(&Fact::Hold(hold))).await;
    // A later connect states it again without asking again.
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().iter().filter(|f| **f == Fact::Hold(hold)).count() == 2).await;
    assert_eq!(client.session.count(&Call::FetchHold), 2);
    // The network's push lifts it; an update that asserts nothing says nothing.
    client.tap.deliver(WaSignal::Hold(HoldState::Unasserted));
    client.tap.deliver(WaSignal::Hold(HoldState::Lifted));
    eventually(|| h.facts().last() == Some(&Fact::Hold(None))).await;
    let held: Vec<_> = h
        .facts()
        .into_iter()
        .filter(|f| matches!(f, Fact::Hold(_)))
        .collect();
    assert_eq!(held.len(), 3, "{held:?}");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn a_configuration_the_client_cannot_honour_starts_nothing() {
    let local = BackendEnv::LocalTest(LocalTestEnv {
        host: "localhost".to_owned(),
        port: 1,
        ca_cert: None,
        trust_root: None,
        zk_params: None,
    });
    let config = BackendConfig {
        env: local,
        ..Default::default()
    };
    let mut h = H::start_with(tempfile::tempdir().expect("dir"), StartMode::Fresh, config);
    h.send(qr_login()).await;
    let step = h.step().await;
    assert!(matches!(
        step.kind,
        LoginStepKind::Failed { ref error } if matches!(error.kind, ErrorKind::Unsupported)
    ));
    assert!(
        h.view.made().is_empty(),
        "a client was made against a test environment"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn a_logout_without_a_device_removes_nothing_and_starts_no_client() {
    let mut h = H::start(StartMode::Fresh);
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::NoDevice),
        other => panic!("{other:?}"),
    }
    assert!(h.view.made().is_empty());
    assert!(matches!(h.exit().await, Exit::LoggedOut { .. }));
}

#[tokio::test]
async fn a_logout_with_a_device_asks_the_client_once_and_reports_what_it_reached() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let mut h = H::start_in(dir, StartMode::Resume);
    let scripted = Arc::new(FakeSession::new());
    scripted.script_logout(DeviceLogout::Removed);
    h.view.queue(scripted);
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, DeviceLogout::Removed);
            assert_eq!(
                report.device_name.as_deref(),
                Some("Kourier (testhost)"),
                "the logout report names the device the way the phone lists it"
            );
        }
        other => panic!("{other:?}"),
    }
    let client = h.client(0).await;
    assert!(matches!(h.exit().await, Exit::LoggedOut { .. }));
    // Asked once, then the client is stopped on the way out.
    assert_eq!(
        client.session.calls(),
        [Call::Start, Call::Logout, Call::Stop]
    );
}

#[tokio::test]
async fn a_cancelled_login_stops_the_client_and_forgets_the_pair_code_request() {
    let mut h = H::start(StartMode::Fresh);
    h.send(Command::StartLogin {
        flow: "pair-code".to_owned(),
        relink: None,
        taken: Vec::new(),
    })
    .await;
    let step = h.step().await;
    assert_eq!(step.step_id, "phone");
    let client = h.client(0).await;
    h.send(Command::CancelLogin).await;
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    assert_eq!(
        client.session.calls(),
        [Call::Start, Call::CancelPairCode, Call::Stop]
    );
    h.send(Command::Logout).await;
    let mut envelope = h.envelope().await;
    match envelope.events.pop() {
        Some(BackendEvent::Logout(report)) => assert_eq!(report.outcome, DeviceLogout::NoDevice),
        other => panic!("{other:?}"),
    }
    assert!(matches!(h.exit().await, Exit::LoggedOut { .. }));
    let _ = UserAction::Reconnect;
}

#[tokio::test]
async fn a_row_the_core_refuses_for_good_is_quarantined_and_the_account_goes_on() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let mut envelope = h.envelope().await;
    envelope
        .ack
        .take()
        .expect("durable")
        .complete(Err(kourier_model::PersistError {
            retryable: false,
            detail: "refused for the test".to_owned(),
        }));
    // The core's Logout follows a link it refused; the actor is still serving.
    h.send(Command::Logout).await;
    let mut report = h.envelope().await;
    assert!(matches!(report.events.pop(), Some(BackendEvent::Logout(_))));
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .unwrap();
    assert_eq!(kourier_db::outbox::len(&conn).unwrap(), 0);
    assert_eq!(kourier_db::outbox::poison_len(&conn).unwrap(), 1);
}

/// The kill tests (T-8's `Rebuild`/append): a child process of this test binary appends
/// `AccountReady` with a failpoint armed and is aborted; the parent looks at the file.
mod kill {
    use super::*;

    const CHILD: &str = "WA_ACTOR_KILL_CHILD";

    /// The child: appends `AccountReady` to the store in the directory it is given.
    #[tokio::test]
    async fn append_child() {
        let Some(dir) = std::env::var_os(CHILD) else {
            return;
        };
        let owner = StoreOwner::open(
            &Path::new(&dir).join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-kill",
        )
        .expect("open");
        let ready = Ready::of(
            UserId::from_parts("pn", PN).unwrap(),
            Some(UserId::from_parts("lid", LID).unwrap()),
            3,
        );
        crate::outbox::append_ready(&owner.store(), &ready, 1)
            .await
            .expect("append");
        owner.close();
    }

    /// Runs the child with `point` armed; whether it was aborted.
    fn run_child(dir: &Path, point: Option<&str>) -> bool {
        let mut cmd = std::process::Command::new(std::env::current_exe().expect("test binary"));
        cmd.args(["--exact", "actor::tests::kill::append_child", "--nocapture"])
            .env(CHILD, dir)
            .env_remove("KOURIER_FAILPOINT");
        if let Some(point) = point {
            cmd.env("KOURIER_FAILPOINT", point);
        }
        !cmd.output().expect("spawn the child").status.success()
    }

    #[test]
    fn a_kill_inside_the_append_leaves_no_row() {
        let dir = tempfile::tempdir().expect("dir");
        pair_device_blocking(dir.path());
        assert!(
            run_child(dir.path(), Some(crate::failpoints::INSIDE_ACCOUNT_READY)),
            "the failpoint did not abort the child"
        );
        assert_eq!(core_rows(dir.path()), 0);
    }

    #[tokio::test]
    async fn a_kill_after_the_append_leaves_the_row_and_the_next_start_ships_it() {
        let dir = tempfile::tempdir().expect("dir");
        pair_device(dir.path()).await;
        assert!(
            run_child(dir.path(), Some(crate::failpoints::AFTER_ACCOUNT_READY)),
            "the failpoint did not abort the child"
        );
        assert_eq!(core_rows(dir.path()), 1);
        let mut h = H::start_in(dir, StartMode::Resume);
        h.ready().await;
        h.send(Command::Shutdown).await;
        assert_eq!(h.exit().await, Exit::Shutdown);
    }

    /// A store with a paired device, made without a runtime of the caller's.
    fn pair_device_blocking(dir: &Path) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(pair_device(dir));
    }
}

#[tokio::test]
async fn a_client_that_does_not_start_ends_the_login_and_leaves_the_account_offline() {
    let mut h = H::start(StartMode::Fresh);
    let scripted = Arc::new(FakeSession::new());
    scripted.fail_start(BackendError::transient("no network namespace"));
    h.view.queue(scripted);
    h.send(qr_login()).await;
    let step = h.step().await;
    assert_eq!(step.step_id, "qr");
    assert!(matches!(
        step.kind,
        LoginStepKind::Failed { ref error } if matches!(error.kind, ErrorKind::Transient { .. })
    ));
    let client = h.client(0).await;
    // Nothing is left running: the half-made client was stopped, the account is offline.
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- a failed write replaces the client (W3a, chunk C6, decision D5) -----

/// The client ends itself on a failed write (`signal_shutdown_sync` is terminal): the actor stops it,
/// shows when the next one comes, makes a NEW client after a backoff that doubles with each failure
/// that follows a short run and starts over at 5 s after a run of two minutes. A full disk must not
/// loop hot.
#[tokio::test]
async fn a_failed_write_replaces_the_client_after_a_backoff_that_doubles_and_starts_over() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let first = h.client(0).await;
    h.go_online(&first).await;

    // The first failure: stopped (the store released), the next try 5 s away, nothing before.
    first.tap.deliver(WaSignal::WriteFailed);
    eventually(|| first.session.count(&Call::Stop) == 1).await;
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 5_000),
    })
    .await;
    h.clock.advance(Duration::from_secs(4));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.view.made().len(),
        1,
        "a new client before the wait was over"
    );
    h.clock.advance(Duration::from_secs(1));
    let second = h.client(1).await;
    assert_eq!(second.session.calls(), [Call::Start]);

    // Failed again at once: twice as long.
    second.tap.deliver(WaSignal::WriteFailed);
    eventually(|| second.session.count(&Call::Stop) == 1).await;
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 5_000 + 10_000),
    })
    .await;
    h.clock.advance(Duration::from_secs(10));
    let third = h.client(2).await;

    // A client that stayed up for three minutes: its failure is a first one again.
    h.clock.advance(Duration::from_secs(180));
    third.tap.deliver(WaSignal::WriteFailed);
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 15_000 + 180_000 + 5_000),
    })
    .await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F8: a write that fails in the link's last stage. AFTER the core acked `AccountReady` the
/// account exists: the failure is a lost connection, the login is not ended, a new client is made
/// after the backoff and its `Connected` finishes the link.
#[tokio::test]
async fn a_failed_write_after_the_ack_is_a_lost_connection_and_the_link_goes_on() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    // What upstream's pairing wrote into the store before it told the handler.
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    client.store.save(&device).await.expect("the device");
    h.ready().await;
    // The actor takes the core's ack as one more event of its loop; nothing outside shows it, so the
    // failure waits a moment (a failure before the ack is the other test).
    tokio::time::sleep(Duration::from_millis(300)).await;

    client.tap.deliver(WaSignal::WriteFailed);
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 5_000),
    })
    .await;
    // Nothing that follows ends the login (the connection state is not a login step).
    while let Ok(Some(envelope)) =
        tokio::time::timeout(Duration::from_millis(300), h.ends.events.recv()).await
    {
        assert!(
            !envelope.events.iter().any(|event| matches!(
                event,
                BackendEvent::Login(LoginStep {
                    kind: LoginStepKind::Failed { .. },
                    ..
                })
            )),
            "the login was ended although the core had the account: {envelope:?}"
        );
    }
    h.clock.advance(Duration::from_secs(5));
    let second = h.client(1).await;
    second.tap.deliver(WaSignal::Connected);
    assert_eq!(h.step().await.step_id, "done");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F8, the other way: BEFORE the ack the failure still ends the login as failed (the core logs
/// the device out), and no new client is made around it.
#[tokio::test]
async fn a_failed_write_before_the_ack_ends_the_link() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    // `AccountReady` is in the channel and is NOT acked.
    let unacked = h.envelope().await;
    assert!(unacked.ack.is_some());

    client.tap.deliver(WaSignal::WriteFailed);
    let envelope = h.envelope().await;
    assert!(
        matches!(
            envelope.events.last(),
            Some(BackendEvent::Login(LoginStep {
                kind: LoginStepKind::Failed { .. },
                ..
            }))
        ),
        "{envelope:?}"
    );
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    h.clock.advance(Duration::from_secs(600));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.view.made().len(),
        1,
        "a client was made around a failed link"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The wait is the person's to end: a `Disconnect` cancels the new client (and the time of the try is
/// gone), a `Connect` makes it at once.
#[tokio::test]
async fn a_disconnect_ends_the_wait_for_a_new_client_and_a_connect_does_not_wait_it_out() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let first = h.client(0).await;
    h.go_online(&first).await;
    first.tap.deliver(WaSignal::WriteFailed);
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 5_000),
    })
    .await;

    h.send(Command::Disconnect).await;
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    h.clock.advance(Duration::from_secs(600));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.view.made().len(), 1, "a held account made a client");

    h.send(Command::Connect).await;
    let second = h.client(1).await;
    assert_eq!(second.session.calls(), [Call::Start]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- the catch-up machine (W3a, chunk C7) -----

/// A text message as the hook commits it; `offline` says whether its stanza came from the
/// offline queue.
fn arrival(id: &str, offline: bool) -> crate::inbox::Arrival {
    use crate::inbox::{Arrival, InboxMeta, Origin};
    let mut message = waproto::whatsapp::Message::default();
    message.conversation = Some("synthetic text".to_owned());
    Arrival {
        chat: "15550100101@s.whatsapp.net".into(),
        sender: "15550100101@s.whatsapp.net".into(),
        id: id.into(),
        origin: Origin::E2e,
        message: waproto::codec::message_to_vec(&message),
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: 1_700_000_000,
            from_me: false,
            is_group: false,
            push_name: String::new(),
            lid_addressing: false,
            sender_alt: None,
            recipient_alt: None,
            ephemeral_expiration: None,
            is_offline: offline,
        },
    }
}

/// What the durability hook does for a batch: the inbox commit, then the note that wakes stage 2.
async fn hook_commit(made: &Made, ids: &[&str], offline: bool) {
    use crate::hook::Notes;
    let done = made
        .receive
        .inbox
        .commit(
            ids.iter().map(|id| arrival(id, offline)).collect(),
            Vec::new(),
        )
        .await
        .expect("commit");
    made.receive.stage2.committed(&done);
}

/// The next durable envelope's messages, as `(id, catch_up)`; the envelope is returned with them,
/// unacked.
async fn next_messages(h: &mut H) -> (Envelope, Vec<(String, bool)>) {
    let envelope = h.envelope().await;
    assert!(envelope.ack.is_some(), "a durable envelope: {envelope:?}");
    // The chats' features ride with their first rows (W5a, D9): `features::tests` reads them.
    let messages = envelope
        .events
        .iter()
        .filter(|event| !crate::features::is_statement(event))
        .map(|event| match event {
            BackendEvent::Message(m) => (m.local.to_string(), m.catch_up),
            other => panic!("not a message: {other:?}"),
        })
        .collect();
    (envelope, messages)
}

async fn resumed_with_a_client() -> (H, Made) {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    (h, client)
}

/// [`resumed_with_a_client`] on a clock that reads the wall clock's time instead of [`NOW`]. A chunk is stamped with
/// the wall clock when it is captured (`kourier_db::now_ms`), and the delivery judges its push by the actor's clock
/// (`progress::Facts::account_over`: a push whose newest capture is more than `PUSH_QUIET_MS` (6 h) old is over). On
/// `NOW`, which is months ahead of the wall clock, every chunk is that old the moment it is captured: the delivery
/// ends every chat's push right behind the chunk, however little of the history the phone has sent. A test that needs
/// a push still running between two chunks starts here.
async fn resumed_on_the_wall_clock() -> (H, Made) {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let clock = FakeClock::new(u64::try_from(kourier_db::now_ms()).expect("a time"));
    let h = H::start_with_clock(dir, StartMode::Resume, BackendConfig::default(), clock);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    (h, client)
}

/// D2: the account is `Syncing` from `Connected` until the drain is durable (Kourier's own boundary),
/// stage 2 is through it and the core acknowledged what stage 2 wrote; the rows of the drain carry
/// `catch_up`, a live message after `Online` does not.
#[tokio::test]
async fn online_waits_for_stage2_and_the_core() {
    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;

    // A drain of three, committed the way the hook does; the completion arrives with the rows still
    // in flight. (No handler is behind this client, so the machine takes the boundary itself.)
    hook_commit(&client, &["A", "B", "C"], true).await;
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(
        messages,
        [("A".into(), true), ("B".into(), true), ("C".into(), true)]
    );
    // Stage 2 is through; the core has not acknowledged: still `Syncing`.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    envelope.ack.take().expect("durable").complete(Ok(()));
    h.conn_is(ConnState::Online { detail: None }).await;

    // A live message after `Online` is no catch-up, and the account stays online.
    hook_commit(&client, &["LIVE"], false).await;
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("LIVE".into(), false)]);
    envelope.ack.take().expect("durable").complete(Ok(()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.conn(), ConnState::Online { detail: None });
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The drain was announced, but its marker never came: not caught up. The account stays `Syncing`
/// (also after `Connected`), and the next connection starts over.
#[tokio::test]
async fn an_interrupted_drain_stays_syncing_and_the_next_connection_starts_over() {
    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    client.tap.deliver(WaSignal::OfflinePreview { total: 0 });
    hook_commit(&client, &["A"], true).await;
    let (mut envelope, _) = next_messages(&mut h).await;
    envelope.ack.take().expect("durable").complete(Ok(()));
    client.tap.deliver(WaSignal::OfflineInterrupted);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.conn(),
        ConnState::Syncing { fraction: None },
        "an interrupted drain is no completed one"
    );

    // The connection goes away; upstream reconnects and the whole thing starts over.
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    client.tap.deliver(WaSignal::OfflinePreview { total: 0 });
    hook_commit(&client, &["B"], true).await;
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    client.tap.deliver(WaSignal::Connected);
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("B".into(), true)]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    envelope.ack.take().expect("durable").complete(Ok(()));
    h.conn_is(ConnState::Online { detail: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Decision D5: upstream's completion is no proof of durability - it also comes after a held-back
/// tail and when its own 60 s wait ran out - so what the drain still brings after Kourier's boundary
/// is catch-up too, and the account is neither held back by it nor sent back to `Syncing`.
#[tokio::test]
async fn a_tail_after_the_boundary_is_catch_up_and_does_not_hold_the_account() {
    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    hook_commit(&client, &["A"], true).await;
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("A".into(), true)]);
    envelope.ack.take().expect("durable").complete(Ok(()));
    h.conn_is(ConnState::Online { detail: None }).await;

    // The rest of the drain, after the boundary: still the offline queue's, so still catch-up.
    hook_commit(&client, &["B", "C"], true).await;
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("B".into(), true), ("C".into(), true)]);
    assert_eq!(h.conn(), ConnState::Online { detail: None });
    envelope.ack.take().expect("durable").complete(Ok(()));
    hook_commit(&client, &["LIVE"], false).await;
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("LIVE".into(), false)]);
    envelope.ack.take().expect("durable").complete(Ok(()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.conn(), ConnState::Online { detail: None });
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// If upstream's completion never comes the account does not stay `Syncing` for ever: after the
/// backlog wait (90 s after the connection is up, on the actor's clock) the machine takes the boundary
/// itself and goes on. Not before.
#[tokio::test]
async fn an_offline_sync_that_never_completes_is_gone_on_without_after_the_backlog_wait() {
    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    hook_commit(&client, &["A"], true).await;
    let (mut envelope, _) = next_messages(&mut h).await;
    envelope.ack.take().expect("durable").complete(Ok(()));
    h.conn_is(ConnState::Syncing { fraction: None }).await;

    h.clock.advance(Duration::from_secs(89));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None }, "too early");
    h.clock.advance(Duration::from_secs(1));
    h.conn_is(ConnState::Online { detail: None }).await;
    // A completion that comes late changes nothing.
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.conn(), ConnState::Online { detail: None });
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A connection that ends, or a client that stops, forgets the machine: a stale completion does not
/// make the next connection `Online`, and a paused account shows nothing of a drain.
#[tokio::test]
async fn a_new_connection_does_not_inherit_the_drain_of_the_old_one() {
    let (h, client) = resumed_with_a_client().await;
    h.go_online(&client).await;
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    // Upstream is back, and `Connected` comes before this connection's drain is done.
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;

    // A pause: whatever the paused client still reports does not bring `Syncing` back.
    h.send(Command::Disconnect).await;
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    client.tap.deliver(WaSignal::OfflinePreview { total: 0 });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Offline { retry_at_ms: None });
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F4: upstream reconnects by itself (its keepalive watchdog, a critical app-state sync with no
/// answer) and then dispatches NO `Disconnected`. The next drain's completion is a new connection's:
/// the account does not stay `Online` across it, it is `Syncing` until that drain is durable, in
/// stage 2 and in the core. (With the old guard "a second completion of the same drain" the
/// completion was dropped and the account stayed `Online` throughout.)
#[tokio::test]
async fn a_reconnect_upstream_makes_by_itself_starts_the_catch_up_over() {
    let (mut h, client) = resumed_with_a_client().await;
    h.go_online(&client).await;

    // The watchdog reconnect: no `Disconnected`, no preview; the new connection's drain.
    hook_commit(&client, &["A", "B"], true).await;
    // What the handler does at the completion, in the writer's order, before the signal is handed on.
    crate::catchup::take_boundary(&client.store)
        .await
        .expect("the handler's boundary");
    client.tap.deliver(WaSignal::OfflineDone { count: 2 });
    client.tap.deliver(WaSignal::Connected);
    let (mut envelope, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("A".into(), true), ("B".into(), true)]);
    // Stage 2 is through and the core has not acknowledged: `Syncing`, not the old connection's
    // `Online`.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    envelope.ack.take().expect("durable").complete(Ok(()));
    h.conn_is(ConnState::Online { detail: None }).await;

    // The same for a new connection that announces a drain: the preview after an ended drain.
    client.tap.deliver(WaSignal::OfflinePreview { total: 1 });
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F4, the preview clause (the re-check's survivor): a preview after a drain's end is a NEW
/// connection's, so the machine starts over at the preview and the new connection is not `Online` on
/// the old one's `Connected`. Upstream's completion does not come here; the backlog wait takes the
/// boundary and stage 2 and the core are through, and still the account waits for the new
/// connection's own `Connected`. (Without the clause the preview only restarted the drain, the stale
/// `Connected` carried over and the account was `Online` as soon as the drain was durable.)
///
/// Not over the Wire: the scripted server can make upstream's teardown without a `Disconnected` (a
/// `<stream:error code="515"/>` sets `expected_disconnect` like `reconnect_immediately`), but this clause
/// shows only at the machine's backlog wait, 90 s on the actor's clock, which the Wire tests run on the
/// system clock without a seam for it.
#[tokio::test]
async fn a_preview_after_an_ended_drain_is_a_new_connection() {
    let (h, client) = resumed_with_a_client().await;
    h.go_online(&client).await;

    // The watchdog reconnect announces its drain; no `Disconnected` came, no `Connected` yet.
    client.tap.deliver(WaSignal::OfflinePreview { total: 1 });
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The wait for upstream's completion ends: the boundary is taken, stage 2 and the core are
    // through (the store holds nothing).
    h.clock.advance(Duration::from_secs(90));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        h.conn(),
        ConnState::Syncing { fraction: None },
        "the new connection has not said Connected: the old one's does not count"
    );
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Online { detail: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F4, the `Connected` clause (the re-check's second survivor): a second `Connected` while the
/// first stands is a NEW connection's, even when no preview or completion came before it. The account
/// is `Syncing` until that connection's drain has ended and is durable, not `Online` on the old
/// connection's end. (Without the clause the machine stayed at the old connection's end and the
/// account `Online`.)
///
/// Not over the Wire: at the pin upstream dispatches `OfflineSyncCompleted` before the notification
/// that lets the post-login task dispatch `Connected` (`sessions.rs`, `publish_offline_sync_live_state`),
/// so no script of the server makes a `Connected` that no end of drain preceded; the signals are fed
/// to the actor.
#[tokio::test]
async fn a_second_connected_while_connected_is_a_new_connection() {
    let (h, client) = resumed_with_a_client().await;
    h.go_online(&client).await;

    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.conn(), ConnState::Syncing { fraction: None });
    // Its drain ends (nothing in it): `Online` again, not before.
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F11: the actor does not write the catch-up boundary when it handles a signal. The handler
/// wrote it in the writer's order; a `Disconnected` handled after a fast reconnect's drain took its
/// boundary must not erase that boundary (every live row would be catch-up until the next one).
#[tokio::test]
async fn a_disconnect_handled_late_keeps_the_boundary_the_next_drain_took() {
    let (h, client) = resumed_with_a_client().await;
    h.go_online(&client).await;
    // The machine took boundary 0 for the first drain (no handler is behind this client). The next
    // connection's drain took another, in the writer's order, before the actor saw the old
    // connection end.
    hook_commit(&client, &["A"], false).await;
    let taken = crate::catchup::take_boundary(&client.store)
        .await
        .expect("the handler's boundary");
    assert_eq!(taken, 1);
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    h.conn_is(ConnState::Offline { retry_at_ms: None }).await;
    assert_eq!(
        crate::catchup::boundary(&client.store).await.expect("read"),
        Some(1),
        "the signal's reset left the stored boundary alone"
    );
    // A client that is paused (the person's Disconnect) or made again does clear it.
    h.send(Command::Disconnect).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while crate::catchup::boundary(&client.store)
        .await
        .expect("read")
        .is_some()
    {
        assert!(tokio::time::Instant::now() < deadline, "it never cleared");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Fix F7 through the actor: a drain upstream gave up waiting for (its completion counts fewer
/// stanzas than the preview announced) is not complete, so the ledger of the earlier connections is
/// not swept at its end. Without the preview's total the completion counts, as in the sweep test above.
#[tokio::test]
async fn a_drain_that_timed_out_upstream_sweeps_no_ledger_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let old = i64::try_from(NOW).expect("time") - crate::inbox::LEDGER_AGE_MS - 1;
    plant_ledger(dir.path(), &[(1, "old-one", old)]).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client.tap.deliver(WaSignal::OfflinePreview { total: 10 });
    client.tap.deliver(WaSignal::OfflineDone { count: 3 });
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Online { detail: None }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        ledger_ids(h.dir.path()),
        ["old-one"],
        "a timed-out drain is no reason to sweep"
    );
}

/// Decision D2 / critic M7: nothing but `AccountReady` reaches the core first. A message that is
/// stored before the core acknowledged `AccountReady` waits behind it - the forwarder ships one
/// envelope at a time and `AccountReady` has the lowest `seq` - so the core's linking rule (a
/// durable envelope leads with `AccountReady`) holds.
#[tokio::test]
async fn a_message_stored_during_a_link_ships_behind_account_ready() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let mut first = h.envelope().await;
    assert!(matches!(
        first.events.first(),
        Some(BackendEvent::AccountReady { .. })
    ));
    assert_eq!(first.validate_linking(), Ok(()));
    // The message is stored while the core has not acknowledged `AccountReady`.
    hook_commit(&client, &["EARLY"], false).await;
    assert!(
        h.quiet_for(Duration::from_millis(300)).await,
        "something reached the core before it acknowledged AccountReady"
    );
    first.ack.take().expect("durable").complete(Ok(()));
    let (mut second, messages) = next_messages(&mut h).await;
    assert_eq!(messages, [("EARLY".into(), true)]);
    second.ack.take().expect("durable").complete(Ok(()));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Collects `n` events from the next durable envelopes, acking each envelope.
async fn events_acked(h: &mut H, n: usize) -> Vec<BackendEvent> {
    let mut seen = Vec::new();
    while seen.len() < n {
        let mut envelope = h.envelope().await;
        let ack = envelope.ack.take().expect("a durable envelope");
        seen.extend(envelope.events);
        ack.complete(Ok(()));
    }
    seen
}

/// Collects the events of the next durable envelopes, acking each, until `n` of them are NOT a features statement (the
/// statements ride with a chat's first row, so a test about its rows must not count them: fix F19, the strike of main's
/// ci). Every event seen is returned, statements included. The wait for each envelope is the harness's (10 s).
async fn rows_acked(h: &mut H, n: usize) -> Vec<BackendEvent> {
    let mut seen = Vec::new();
    while seen
        .iter()
        .filter(|e| !crate::features::is_statement(e))
        .count()
        < n
    {
        let mut envelope = h.envelope().await;
        let ack = envelope.ack.take().expect("a durable envelope");
        seen.extend(envelope.events);
        ack.complete(Ok(()));
    }
    seen
}

/// Fix F5: a link holds the receive path until it has an account. The new connection's traffic lands
/// before the actor took the pairing's signal: a message the hook committed (stage 2 is asked to step)
/// and a receipt the handler writes (it waits); neither may lead `AccountReady` in `core_outbox`, the
/// core refuses such a row for good. Held, they ship right behind it, and nothing is lost.
#[tokio::test]
async fn nothing_of_the_new_connection_can_lead_account_ready() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert!(
        client.store.hold().is_closed(),
        "a login holds the receive path"
    );
    assert_eq!(
        client.session.hold_closed_at_start(),
        [true],
        "the hold closes before the client exists"
    );
    hook_commit(&client, &["EARLY"], false).await;
    let writer = client.receive.writer.clone();
    let receipt = std::thread::spawn(move || {
        writer.write_and_wait(&[BackendEvent::ConversationRemoved {
            conv: kourier_model::ConvKey::Group("wag:7".into()),
        }])
    });
    eventually(|| client.store.hold().parked() == 1).await;
    // Stage 2 is asked for a step on purpose; the actor has not recorded the account.
    let step = client.receive.stage2.step().await.expect("a step");
    assert_eq!(
        step.taken, 0,
        "stage 2 stepped before the link had an account"
    );
    assert_eq!(core_rows(h.dir.path()), 0, "a row reached core_outbox");

    // The pairing reaches the actor.
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    let mut first = h.envelope().await;
    assert!(
        matches!(
            first.events.first(),
            Some(BackendEvent::AccountReady { .. })
        ),
        "{first:?}"
    );
    assert_eq!(first.validate_linking(), Ok(()));
    let ack = first.ack.take().expect("durable");
    ack.complete(Ok(()));
    // Two held ROWS follow, however many statements ride with them (fix F19: the EARLY row carries its chat's features, so
    // two EVENTS could end before the parked receipt's row).
    let behind = rows_acked(&mut h, 2).await;
    assert!(
        behind
            .iter()
            .any(|e| matches!(e, BackendEvent::Message(m) if &*m.local == "EARLY"))
            && behind
                .iter()
                .any(|e| matches!(e, BackendEvent::ConversationRemoved { .. })),
        "the held rows ship behind AccountReady: {behind:?}"
    );
    assert_eq!(receipt.join().expect("handler thread"), Ok(()));
    assert!(!client.store.hold().is_closed());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F5: a login that ends without an account lets the receive path go (the hold follows the login).
#[tokio::test]
async fn a_login_that_ends_without_an_account_opens_the_receive_hold() {
    let h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert!(client.store.hold().is_closed());
    h.send(Command::CancelLogin).await;
    eventually(|| !client.store.hold().is_closed()).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F5, the refused pairing (the re-check's gap; the lead's decision: the hold stays CLOSED). The
/// client of a refused pairing runs on until its logout has reached the server, and the link never has
/// an account, so nothing of that connection may reach `core_outbox`: the hold stays closed after the
/// refusal and after the client was stopped, stage 2 leaves the rows the hook committed alone, a write
/// of the handler waits at the hold and then fails `Held` (its stanza is not acked, the logout ends the
/// connection anyway), and the core is told nothing but the login's failure.
#[tokio::test]
async fn a_refused_pairing_keeps_the_receive_hold_closed_and_nothing_of_it_reaches_the_core() {
    let mut h = H::start(StartMode::Fresh);
    h.send(refused_link()).await;
    let client = h.client(0).await;
    client.session.script_logout(DeviceLogout::NotReached);
    assert!(
        client.store.hold().is_closed(),
        "a login holds the receive path"
    );
    // Traffic of the connection, before the refusal reached the actor: committed by the hook.
    hook_commit(&client, &["EARLY"], false).await;
    assert_eq!(
        client.tap.deliver(paired(PN, LID)),
        crate::tap::Delivered::Refused
    );
    assert!(matches!(h.step().await.kind, LoginStepKind::Failed { .. }));
    // The actor has handled the refusal when it has stopped the client after the logout ended.
    eventually(|| client.session.count(&Call::Stop) == 1).await;
    assert!(
        client.store.hold().is_closed(),
        "a refused pairing opened the receive hold"
    );
    // More of the same connection after the refusal: a message the hook commits, and a receipt the
    // handler writes (it waits at the hold).
    hook_commit(&client, &["LATE"], false).await;
    let writer = client.receive.writer.clone();
    let receipt = std::thread::spawn(move || {
        writer.write_and_wait(&[BackendEvent::ConversationRemoved {
            conv: kourier_model::ConvKey::Group("wag:7".into()),
        }])
    });
    // Stage 2 is asked for a step on purpose.
    let step = client.receive.stage2.step().await.expect("a step");
    assert_eq!(step.taken, 0, "stage 2 stepped for a refused pairing");
    assert_eq!(
        receipt.join().expect("handler thread"),
        Err(crate::events::WriteFailed::Held),
        "the handler's write went through for a refused pairing"
    );
    assert_eq!(core_rows(h.dir.path()), 0, "a row reached core_outbox");
    assert!(
        h.quiet_for(Duration::from_millis(300)).await,
        "something reached the core"
    );
    assert!(client.store.hold().is_closed());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F10: a `Rebuild` states `AccountReady` before the forwarder and stage 2 exist, so an inbox row
/// a previous run left becomes an event behind it, never before it.
#[tokio::test]
async fn a_rebuild_states_account_ready_before_the_rows_a_previous_run_left() {
    use crate::inbox::Inbox;
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-left",
        )
        .expect("open");
        Inbox::new(owner.store(), Arc::new(FakeClock::new(NOW)))
            .commit(vec![arrival("LEFT", false)], Vec::new())
            .await
            .expect("commit");
        owner.close();
    }
    let mut h = H::start_in(dir, StartMode::Rebuild);
    let events = events_acked(&mut h, 3).await;
    assert!(
        matches!(events.first(), Some(BackendEvent::AccountReady { .. })),
        "{events:?}"
    );
    // The chat's features ride with its first message (W5a, D9): not this test's subject.
    let events: Vec<_> = events
        .into_iter()
        .filter(|e| !crate::features::is_statement(e))
        .collect();
    assert!(
        matches!(events.get(1), Some(BackendEvent::Message(m)) if &*m.local == "LEFT"),
        "{events:?}"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F9: a client whose stop did not see the store released is still a user of it. The automatic
/// rebuild waits for the release (a new look every 5 s) and makes no second client until it came.
#[tokio::test]
async fn a_new_client_waits_for_the_store_the_last_one_still_holds() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let stuck = Arc::new(FakeSession::new());
    stuck.hang_release();
    h.view.queue(stuck);
    h.send(Command::Connect).await;
    let first = h.client(0).await;
    h.go_online(&first).await;

    first.tap.deliver(WaSignal::WriteFailed);
    eventually(|| first.session.count(&Call::Stop) == 1).await;
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 5_000),
    })
    .await;
    // The backoff is over; the store is still held: no second client, a new look 5 s later, twice.
    // Each move of the clock waits for the actor's next look to be ARMED: the state is published
    // before the sleep registers, and a move that comes first is lost to it (the FakeClock trap).
    look_is_armed(&h).await;
    h.clock.advance(Duration::from_secs(5));
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 10_000),
    })
    .await;
    look_is_armed(&h).await;
    h.clock.advance(Duration::from_secs(5));
    h.conn_is(ConnState::Offline {
        retry_at_ms: Some(NOW + 15_000),
    })
    .await;
    assert_eq!(h.view.made().len(), 1, "a second client on a held store");
    // The old client lets go: the next look makes the new one.
    first.session.let_go();
    look_is_armed(&h).await;
    h.clock.advance(Duration::from_secs(5));
    let second = h.client(1).await;
    assert_eq!(second.session.calls(), [Call::Start]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Waits until the actor's next look at a held store (5 s from now on its clock) is armed.
async fn look_is_armed(h: &H) {
    let look = h.clock.now_ms() + 5_000;
    eventually(|| h.clock.deadlines().contains(&look)).await;
}

#[tokio::test]
async fn a_download_request_without_a_client_is_answered_as_one_to_try_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let mut h = H::start_in(dir, StartMode::Resume);
    let blobs = h.dir.path().join("blobs");
    let download = |attachment: i64, request: i64| Command::DownloadAttachment {
        attachment: kourier_model::AttachmentId(attachment),
        request: kourier_model::DownloadId(request),
        remote: whole_pointer(),
        dest: kourier_backend_api::BlobTarget {
            path: blobs.join("file"),
            key: kourier_model::BlobKey::new([0x42; 32]),
        },
        net_ids: Vec::new().into(),
    };
    h.send(download(7, 70)).await;
    // One DURABLE `AttachmentDone` under the request's id; the error leaves the attachment retryable (the
    // core turns a transient error into `Failed { retryable: true }`).
    let mut envelope = h.envelope().await;
    let ack = envelope.ack.take().expect("a durable envelope");
    let [
        BackendEvent::AttachmentDone {
            attachment,
            request,
            result: Err(error),
        },
    ] = envelope.events.as_slice()
    else {
        panic!("one failed download: {:?}", envelope.events);
    };
    assert_eq!(*attachment, kourier_model::AttachmentId(7));
    assert_eq!(*request, Some(kourier_model::DownloadId(70)));
    assert!(error.is_retryable(), "{error:?}");
    ack.complete(Ok(()));
    // A second request is a second answer.
    h.send(download(8, 80)).await;
    let mut envelope = h.envelope().await;
    let ack = envelope.ack.take().expect("durable again");
    assert!(matches!(
        envelope.events.as_slice(),
        [BackendEvent::AttachmentDone { request: Some(r), .. }] if *r == kourier_model::DownloadId(80)
    ));
    ack.complete(Ok(()));
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(core_rows(dir.path()), 0, "both answers were acked");
}

/// The `remote` of a file a download can ask for.
fn whole_pointer() -> kourier_model::BackendBlob {
    let pointer = crate::media::MediaPointer {
        media_type: "image".into(),
        direct_path: "/v/t62.7118-24/synthetic-path-30491827".into(),
        media_key: "11".repeat(32),
        file_sha256: Some("22".repeat(32)),
        file_enc_sha256: Some("33".repeat(32)),
        file_length: 5,
        ..crate::media::MediaPointer::default()
    };
    kourier_model::BackendBlob::new(
        crate::media::POINTER_V,
        serde_json::to_vec(&pointer).expect("json"),
    )
}

/// Asks for the file of [`whole_pointer`] at `path`.
async fn ask_for_a_file(h: &H, path: &std::path::Path) {
    h.send(Command::DownloadAttachment {
        attachment: kourier_model::AttachmentId(7),
        request: kourier_model::DownloadId(70),
        remote: whole_pointer(),
        dest: kourier_backend_api::BlobTarget {
            path: path.to_owned(),
            key: kourier_model::BlobKey::new([0x42; 32]),
        },
        net_ids: Vec::new().into(),
    })
    .await;
}

/// The next `AttachmentDone`, acked.
async fn download_answer(h: &mut H) -> Result<kourier_model::AttachmentMeta, BackendError> {
    loop {
        let mut envelope = h.envelope().await;
        let found = envelope.events.iter().find_map(|event| match event {
            BackendEvent::AttachmentDone { result, .. } => Some(result.clone()),
            _ => None,
        });
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        if let Some(found) = found {
            return found;
        }
    }
}

/// W7: the file arrives sealed at the core's target, under the core's key, and the answer says how long it is.
#[tokio::test]
async fn a_downloaded_file_is_sealed_at_its_target() {
    let (mut h, client) = resumed_with_a_client().await;
    client.session.script_download(Ok(b"hello".to_vec()));
    let path = h.dir.path().join("blobs/core/ab/file");
    ask_for_a_file(&h, &path).await;
    let meta = download_answer(&mut h).await.expect("downloaded");
    assert_eq!(meta.size, 5);
    let key = kourier_blob::FileKey::from_bytes([0x42; 32]);
    assert_eq!(
        &*kourier_blob::read_all(&path, &key).expect("a sealed file"),
        b"hello"
    );
    assert_eq!(
        client
            .session
            .calls()
            .iter()
            .filter(|call| **call == crate::session::fake::Call::Download)
            .count(),
        1
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W7: a file the servers no longer have is `Expired`, not something to try again, and leaves nothing at the target.
#[tokio::test]
async fn a_file_the_servers_no_longer_have_is_expired() {
    let (mut h, client) = resumed_with_a_client().await;
    client
        .session
        .script_download(Err(crate::session::DownloadFailure::Gone));
    let path = h.dir.path().join("blobs/core/ab/file");
    ask_for_a_file(&h, &path).await;
    let error = download_answer(&mut h).await.expect_err("gone");
    assert!(matches!(error.kind, kourier_model::ErrorKind::Expired));
    assert!(!path.exists());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W7: a host that could not be reached is worth another click.
#[tokio::test]
async fn a_file_no_host_gave_can_be_tried_again() {
    let (mut h, client) = resumed_with_a_client().await;
    client
        .session
        .script_download(Err(crate::session::DownloadFailure::Unreachable));
    let path = h.dir.path().join("blobs/core/ab/file");
    ask_for_a_file(&h, &path).await;
    let error = download_answer(&mut h).await.expect_err("unreachable");
    assert!(error.is_retryable(), "{error:?}");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

const JPEG: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0];
const PERSON: &str = "direct:lid:77700172839405";
const OTHER_PERSON: &str = "direct:lid:77700172839406";

/// [`resumed_with_a_client`] for an account with the chats `convs`, online, `spent` lookups already made today.
async fn online_with_chats(convs: &[&str], spent: usize) -> (H, Made) {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-chats",
    )
    .expect("open");
    let convs: Vec<String> = convs.iter().map(|conv| (*conv).to_owned()).collect();
    owner
        .store()
        .write("test chats", move |tx| {
            for conv in &convs {
                // As this build states them, or the start sweep would state them again and the catch-up wait for the acks.
                tx.execute(
                    "INSERT INTO features_stated(conv, version, started) VALUES (?1, ?2, 0)",
                    kourier_db::rusqlite::params![conv, crate::features::VERSION],
                )
                .map_err(crate::error::sql("test chats"))?;
            }
            for n in 0..spent {
                tx.execute(
                    "INSERT INTO avatars(conv, checked_at) VALUES (?1, ?2)",
                    kourier_db::rusqlite::params![format!("group:g:{n}"), NOW as i64],
                )
                .map_err(crate::error::sql("test chats"))?;
            }
            Ok(())
        })
        .await
        .expect("chats");
    owner.close();
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    h.go_online(&client).await;
    (h, client)
}

/// The ids named by the lookups made so far, in order.
fn pictures_named(client: &Made) -> Vec<Option<String>> {
    client
        .session
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Picture(known) => Some(known),
            _ => None,
        })
        .collect()
}

/// Moves the clock on, a gap at a time, until `n` lookups were made.
async fn pictures_asked(h: &H, client: &Made, n: usize) -> Vec<Option<String>> {
    for _ in 0..300 {
        let named = pictures_named(client);
        if named.len() >= n {
            return named;
        }
        h.clock.advance(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{n} lookups never came: {:?}", pictures_named(client));
}

/// Time passes on the clock, and the tasks get to look at it.
async fn time_passes(h: &H, by: Duration) {
    h.clock.advance(by);
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.clock.advance(Duration::from_millis(1));
    }
}

/// The next statement of a picture, every envelope on the way acked.
async fn picture_stated(h: &mut H) -> Patch<kourier_model::BlobRef> {
    loop {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        for event in envelope.events {
            let avatar = match event {
                BackendEvent::ContactUpsert(patch) => patch.avatar,
                BackendEvent::ConversationUpsert(patch) => patch.avatar,
                _ => Patch::Unchanged,
            };
            if !matches!(avatar, Patch::Unchanged) {
                return avatar;
            }
        }
    }
}

fn found(id: &str) -> Result<crate::session::PictureAnswer, crate::session::AskFailure> {
    Ok(crate::session::PictureAnswer::Found {
        id: id.to_owned(),
        bytes: JPEG.to_vec(),
    })
}

/// W8: a picture is sealed and stated once; the chat is asked about again only when it is due, naming what it holds.
#[tokio::test]
async fn a_picture_is_sealed_and_stated_once() {
    let (mut h, client) = online_with_chats(&[PERSON], 0).await;
    client.session.script_picture(found("p1"));
    assert_eq!(pictures_asked(&h, &client, 1).await, vec![None]);
    let Patch::Set(blob) = picture_stated(&mut h).await else {
        panic!("a picture was expected");
    };
    assert!(h.dir.path().join("blobs").join(&blob.path).is_file());

    time_passes(&h, Duration::from_secs(24 * 3600)).await;
    assert_eq!(pictures_named(&client).len(), 1, "not yet due");

    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::Unchanged));
    time_passes(&h, Duration::from_secs(7 * 24 * 3600)).await;
    assert_eq!(
        pictures_asked(&h, &client, 2).await,
        vec![None, Some("p1".to_owned())]
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W8: a picture taken away is stated as gone and its file removed.
#[tokio::test]
async fn a_picture_taken_away_is_cleared() {
    let (mut h, client) = online_with_chats(&[PERSON], 0).await;
    client.session.script_picture(found("p1"));
    pictures_asked(&h, &client, 1).await;
    let Patch::Set(blob) = picture_stated(&mut h).await else {
        panic!("a picture was expected");
    };
    let file = h.dir.path().join("blobs").join(&blob.path);

    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::None));
    time_passes(&h, Duration::from_secs(8 * 24 * 3600)).await;
    pictures_asked(&h, &client, 2).await;
    assert!(matches!(picture_stated(&mut h).await, Patch::Clear));
    assert!(!file.exists());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W8: lookups keep their distance, whatever the number of chats.
#[tokio::test]
async fn picture_lookups_keep_their_distance() {
    let (h, client) = online_with_chats(&[PERSON, OTHER_PERSON], 0).await;
    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::None));
    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::None));
    time_passes(&h, Duration::from_millis(crate::avatars::GAP_MS - 100)).await;
    assert!(pictures_named(&client).is_empty(), "too soon");
    pictures_asked(&h, &client, 1).await;
    assert_eq!(pictures_named(&client).len(), 1);
    time_passes(&h, Duration::from_millis(crate::avatars::GAP_MS - 100)).await;
    assert_eq!(pictures_named(&client).len(), 1, "too soon after the first");
    pictures_asked(&h, &client, 2).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W8: a 429 closes the walk for as long as the server said.
#[tokio::test]
async fn a_refused_lookup_waits_for_the_delay_named() {
    let (h, client) = online_with_chats(&[PERSON], 0).await;
    client
        .session
        .script_picture(Err(crate::session::AskFailure::Refused {
            code: 429,
            backoff: Some(Duration::from_secs(3600)),
        }));
    pictures_asked(&h, &client, 1).await;
    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::None));
    time_passes(&h, Duration::from_secs(1800)).await;
    assert_eq!(pictures_named(&client).len(), 1, "still closed");
    time_passes(&h, Duration::from_secs(1900)).await;
    pictures_asked(&h, &client, 2).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W8: a day's lookups are counted from the rows, so a restart does not begin the count again.
#[tokio::test]
async fn the_days_lookups_survive_a_restart() {
    let spent = usize::try_from(crate::avatars::PER_DAY).expect("a count");
    let (h, client) = online_with_chats(&[PERSON], spent).await;
    client
        .session
        .script_picture(Ok(crate::session::PictureAnswer::None));
    time_passes(&h, Duration::from_secs(600)).await;
    assert!(pictures_named(&client).is_empty(), "the day is spent");
    time_passes(&h, Duration::from_secs(24 * 3600)).await;
    pictures_asked(&h, &client, 1).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W8: a lookup the connection could not carry ends the walk until the next connection.
#[tokio::test]
async fn a_lookup_without_a_connection_ends_the_walk() {
    let (h, client) = online_with_chats(&[PERSON], 0).await;
    client
        .session
        .script_picture(Err(crate::session::AskFailure::NotConnected));
    pictures_asked(&h, &client, 1).await;
    time_passes(&h, Duration::from_secs(3600)).await;
    assert_eq!(pictures_named(&client).len(), 1);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W5b (G2): an abort of a message no attempt runs for is answered `TooLate`, at once (the reply is never dropped).
#[tokio::test]
async fn an_abort_for_an_unknown_local_is_too_late() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    assert_eq!(
        abort(&h, 5).await,
        kourier_backend_api::AbortAnswer::TooLate
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The actor's answer to an `AbortTransmit` of the message row `local`.
async fn abort(h: &H, local: i64) -> kourier_backend_api::AbortAnswer {
    let (reply, answer) = tokio::sync::oneshot::channel();
    h.send(Command::AbortTransmit {
        local: kourier_model::MessageId(local),
        reply,
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), answer)
        .await
        .expect("answered in time")
        .expect("the reply was not dropped")
}

/// A `Transmit` of `kind` for the message row `local` (the request's own id is synthetic).
fn transmit_of(
    local: i64,
    kind: kourier_backend_api::OutgoingKind,
) -> (
    Command,
    tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome>,
) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let req = kourier_backend_api::OutgoingRequest {
        conv: kourier_model::ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap()),
        local: SEND_LOCAL.into(),
        sent_at: i64::try_from(NOW).expect("time") - 1000,
        kind,
        only: None,
        is_recipient_update: false,
    };
    let command = Command::Transmit {
        local: kourier_model::MessageId(local),
        req: Box::new(req),
        reply,
    };
    (command, answer)
}

/// W5b (G2): the stop state is registered in `Actor::transmit` BEFORE the attempt is spawned, so an `AbortTransmit`
/// queued right behind the `Transmit` of a message with files finds it and is answered `Stopping`; the attempt then
/// answers `Failed` with no frame, and once it ended the message is unknown again (`TooLate`). Mutants: the old
/// `TooLate` arm; the stop registered where the abort cannot find it.
#[tokio::test]
async fn an_abort_right_behind_the_transmit_finds_the_attempt() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let (command, answer) = transmit_of(
        7,
        kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Media {
                attachments: vec![kourier_model::Attachment {
                    kind: kourier_model::AttachmentKind::Image,
                    mime: "image/jpeg".into(),
                    size: 4096,
                    ..Default::default()
                }],
                caption: None,
            },
            reply_to: None,
            expiry: None,
        },
    );
    h.send(command).await;
    assert_eq!(
        abort(&h, 7).await,
        kourier_backend_api::AbortAnswer::Stopping,
        "the attempt was registered before it was spawned"
    );
    let outcome = answered(answer).await;
    assert!(
        matches!(outcome, kourier_backend_api::TransmitOutcome::Failed(_)),
        "a stopped attempt fails: {outcome:?}"
    );
    assert_eq!(
        abort(&h, 7).await,
        kourier_backend_api::AbortAnswer::TooLate,
        "the attempt's guard forgot it when it ended"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W5b (G2): only a new message with files can be stopped: a text, an edit, a delete and a reaction have no upload
/// to report, so an abort right behind them is `TooLate`. Mutant: `open()` for every kind.
#[tokio::test]
async fn text_edits_and_reactions_cannot_be_stopped() {
    use kourier_backend_api::OutgoingKind;
    use kourier_model::{Body, MsgTarget, TargetAuthor, Text};

    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let target = || MsgTarget {
        conv: None,
        author: TargetAuthor::Own,
        local: "3EB0C8E15A2D97F40316".into(),
        alternates: Vec::new(),
    };
    let kinds = [
        OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic unstoppable")),
            reply_to: None,
            expiry: None,
        },
        OutgoingKind::Edit {
            target: target(),
            body: Body::Text(Text::plain("synthetic edited")),
        },
        OutgoingKind::Delete {
            target: target(),
            for_everyone: true,
        },
        OutgoingKind::React {
            target: target(),
            emoji: "\u{1F44D}".into(),
            remove: false,
        },
    ];
    for (n, kind) in (20..).zip(kinds) {
        let name = kind.name();
        let (command, answer) = transmit_of(n, kind);
        h.send(command).await;
        assert_eq!(
            abort(&h, n).await,
            kourier_backend_api::AbortAnswer::TooLate,
            "{name} cannot be stopped"
        );
        answered(answer).await;
    }
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W5b (G3): a process killed in the middle of an upload left its sealed temporary file; the next start of the
/// account's actor removes it (its key died with the process). Mutant: the sweep in `run_with` removed.
#[tokio::test]
async fn a_start_removes_the_temporary_upload_files_a_dead_process_left() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let tmp = crate::send::tmp::dir_beside(&dir.path().join("backend.db"));
    std::fs::create_dir_all(&tmp).expect("dir");
    let left = tmp.join("0f3c9a7e51d24b86");
    std::fs::write(&left, b"sealed bytes of a dead process").expect("plant");
    let h = H::start_in(dir, StartMode::Resume);
    // Any answer means the actor runs, so its start is over.
    abort(&h, 3).await;
    assert!(!left.exists(), "the dead process's file is gone");
    assert!(!tmp.exists(), "and its directory");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- typing (W3b, chunk C6a) -----

fn typing_signal(user: &str, on: bool) -> WaSignal {
    WaSignal::Typing(crate::translate::Typing {
        conv: kourier_model::ConvKey::Direct(UserId::from_parts("pn", "31000000001").unwrap()),
        by: UserId::from_parts("pn", user).unwrap(),
        device: Some(2),
        on,
    })
}

/// The next envelope must be ephemeral typing (no ack), and the account's `core_outbox` stays empty.
async fn expect_typing(h: &mut H, on: bool) {
    let envelope = h.envelope().await;
    assert!(
        envelope.ack.is_none(),
        "typing is ephemeral: it has no ack, so it is no durable row: {envelope:?}"
    );
    let [
        BackendEvent::Typing {
            conv,
            by,
            device,
            on: got,
            ttl,
        },
    ] = envelope.events.as_slice()
    else {
        panic!("not one Typing event: {:?}", envelope.events);
    };
    assert_eq!(
        *conv,
        kourier_model::ConvKey::Direct(UserId::from_parts("pn", "31000000001").unwrap())
    );
    assert_eq!(*by, UserId::from_parts("pn", "31000000001").unwrap());
    assert_eq!(*device, Some(2));
    assert_eq!(*got, on);
    assert_eq!(*ttl, super::TYPING_TTL);
    assert_eq!(core_rows(h.dir.path()), 0, "typing reached core_outbox");
}

/// Typing is a fact of seconds: it reaches the core as an ephemeral envelope and never as a
/// `core_outbox` row (a mutant that writes it like a receipt, through the group commit, makes a
/// durable envelope with an ack and a row).
#[tokio::test]
async fn typing_is_ephemeral_and_never_in_core_outbox() {
    let (mut h, client) = resumed_with_a_client().await;
    let peer = "31000000001";
    assert_eq!(
        client.tap.deliver(typing_signal(peer, true)),
        crate::tap::Delivered::Sent
    );
    expect_typing(&mut h, true).await;
    // And the stop.
    client.tap.deliver(typing_signal(peer, false));
    expect_typing(&mut h, false).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Our own device's typing is no news, and typing of a connection without an account is dropped while
/// the link hold is closed. The signals of a client are handled in order: the peer's typing that
/// arrives FIRST proves the earlier ones gave nothing.
#[tokio::test]
async fn our_own_typing_is_dropped() {
    let (mut h, client) = resumed_with_a_client().await;
    // Our number and our LID, from the device row `pair_device` stored.
    client.tap.deliver(typing_signal(PN, true));
    client
        .tap
        .deliver(WaSignal::Typing(crate::translate::Typing {
            conv: kourier_model::ConvKey::Direct(UserId::from_parts("pn", "31000000001").unwrap()),
            by: UserId::from_parts("lid", LID).unwrap(),
            device: None,
            on: true,
        }));
    client.tap.deliver(typing_signal("31000000001", true));
    expect_typing(&mut h, true).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

#[tokio::test]
async fn typing_is_dropped_while_the_link_hold_is_closed() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert!(client.store.hold().is_closed());
    client.tap.deliver(typing_signal("31000000001", true));
    // The QR step behind it is the first thing the core hears.
    client.tap.deliver(qr("code-one"));
    assert!(matches!(h.step().await.kind, LoginStepKind::ShowQr { .. }));
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(
        matches!(exit, Exit::Shutdown | Exit::LoggedOut { .. }),
        "{exit:?}"
    );
    assert_eq!(core_rows(dir.path()), 0);
}

// ----- the handler's knowledge of our ids (W3b, chunks C6b and C6c) -----

fn our_ids() -> crate::map::OwnIds {
    crate::map::OwnIds {
        lid: Some(UserId::from_parts("lid", LID).unwrap()),
        pn: Some(UserId::from_parts("pn", PN).unwrap()),
    }
}

/// The handler (a thread that cannot wait for a store job) tells our own acts, a group notice we caused,
/// by `Stage2::own_known`. A client made on a paired device finds it filled: the actor read the device
/// row before it made the client.
#[tokio::test]
async fn the_handler_knows_our_ids_when_a_client_is_made_on_a_paired_device() {
    let (h, client) = resumed_with_a_client().await;
    assert_eq!(client.receive.stage2.own_known(), our_ids());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// F4 (interop review INTEROP-5): a resumed start finds the call log's first record with our ids known. The phone's
/// record of a 1:1 call WE placed names the other person AND us among its participants; the handler (the writer's
/// zone, with `own_known()`) files the line in the other person's chat and never in a chat with ourselves. Kills "the
/// ids are read only at the first mapped row" (`start_client` without its early read of the device row).
#[tokio::test]
async fn the_first_call_log_record_of_a_resumed_start_is_filed_in_the_peers_chat_not_with_ourselves()
 {
    use crate::calls::log::Seen;
    use kourier_model::ConvKey;
    use waproto::whatsapp::call_log_record::CallResult;

    let (mut h, client) = resumed_with_a_client().await;
    let peer = UserId::from_parts("lid", "99900100577").unwrap();
    let seen = Seen {
        call_id: "CALLF4A1".to_owned(),
        creator: Some(UserId::from_parts("lid", LID).unwrap()),
        from_me: true,
        group: None,
        others: vec![UserId::from_parts("pn", PN).unwrap(), peer.clone()],
        video: Some(false),
        result: Some(CallResult::CONNECTED),
        started_s: Some(1_799_000_000),
        written_ms: 1_799_000_100_000,
    };
    let (writer, own) = (
        client.receive.writer.clone(),
        client.receive.stage2.own_known(),
    );
    let write = tokio::task::spawn_blocking(move || writer.write_call_log_and_wait(seen, own));
    let mut told = Vec::new();
    while !told
        .iter()
        .any(|e| matches!(e, BackendEvent::HistoryBatch { .. }))
    {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        told.extend(envelope.events);
    }
    write.await.expect("joined").expect("the zone committed");
    let chats: Vec<ConvKey> = told
        .iter()
        .filter_map(|event| match event {
            BackendEvent::HistoryBatch { conv, .. } => Some(conv.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        chats,
        [ConvKey::Direct(peer)],
        "one line, in the peer's chat"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A fresh link has no device row when its client is made: the ids come with `AccountReady`.
#[tokio::test]
async fn the_handler_knows_our_ids_once_a_link_states_them() {
    let mut h = H::start(StartMode::Fresh);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert_eq!(
        client.receive.stage2.own_known(),
        crate::map::OwnIds::default(),
        "nothing is known before the pairing"
    );
    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    h.ready().await;
    assert_eq!(client.receive.stage2.own_known(), our_ids());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- the history driver (W4, chunk C16) -----

use crate::history::testing::{Chat, Chunk, SyncType};

const PEER: &str = "15550100101@s.whatsapp.net";

/// What a captured chunk of one chat with `count` texts is.
fn chunk_of(count: usize) -> Vec<u8> {
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 100)
        .chat(Chat::new(PEER).texts("A", 1_700_000_100, count))
        .blob()
}

/// The rows of `core_outbox`, oldest first, as what they carry: `"history"` for a row of the chunk, `"live"`
/// for anything else.
fn core_kinds(dir: &Path) -> Vec<&'static str> {
    let conn =
        kourier_db::open(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    kourier_db::outbox::peek(&conn, 1000)
        .expect("peek")
        .into_iter()
        .map(|row| {
            let events = kourier_backend_api::decode_outbox_row(&row.envelope).expect("decodes");
            let history = events.iter().all(|event| {
                matches!(
                    event,
                    BackendEvent::HistoryBatch { .. }
                        | BackendEvent::ConversationUpsert(_)
                        | BackendEvent::ContactUpsert(_)
                        | BackendEvent::IdentityMerged { .. }
                )
            });
            if history { "history" } else { "live" }
        })
        .collect()
}

/// A device row of a login that did not finish: an address of ours and no pairing. The account has no
/// account yet (a login may begin), and the stage is named by that address.
async fn save_unpaired_device(dir: &Path) {
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-unpaired",
    )
    .expect("open");
    let mut device = Device::new();
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    owner.store().save(&device).await.expect("save");
    owner.close();
}

/// A link has no account yet: a chunk captured meanwhile is not delivered until `AccountReady` is recorded,
/// and `AccountReady` leads what the core is given (it refuses, for good, a row that leads a link).
#[tokio::test]
async fn nothing_of_the_history_reaches_the_core_before_account_ready() {
    let dir = tempfile::tempdir().expect("temp dir");
    save_unpaired_device(dir.path()).await;
    let mut h = H::start_in(dir, StartMode::Resume);
    h.send(qr_login()).await;
    let client = h.client(0).await;
    assert!(
        client.store.hold().is_closed(),
        "a login holds the receive path"
    );

    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(5))
        .await
        .expect("captured");
    // The driver has been told of the chunk. It must leave `core_outbox` alone.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        core_rows(h.dir.path()),
        0,
        "history reached core_outbox before AccountReady"
    );

    client.tap.deliver(paired(PN, LID));
    assert_eq!(h.step().await.step_id, "linking");
    // The first durable envelope is `AccountReady`; the chunk follows it.
    h.ready().await;
    let mut messages = 0;
    while messages < 5 {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            if let BackendEvent::HistoryBatch {
                messages: batch, ..
            } = event
            {
                messages += batch.len();
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert_eq!(messages, 5, "every message of the chunk, once");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The history never fills the core's door: with the core answering nothing, two history rows are in
/// `core_outbox` and no more, and a live row appended meanwhile stands behind those two.
#[tokio::test]
async fn a_live_message_waits_behind_two_history_rows_at_most() {
    let (mut h, client) = resumed_with_a_client().await;
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(1_300))
        .await
        .expect("captured");
    let dir = h.dir.path().to_path_buf();
    eventually(|| core_kinds(&dir).len() == 2).await;
    // Nothing is answered: the window holds however long the driver is given.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(core_kinds(&dir), ["history", "history"]);

    // A live fact arrives (a download answered): it stands behind exactly those two rows.
    h.send(Command::DownloadAttachment {
        attachment: kourier_model::AttachmentId(7),
        request: kourier_model::DownloadId(70),
        remote: kourier_model::BackendBlob::new(1, b"{}".to_vec()),
        dest: kourier_backend_api::BlobTarget {
            path: h.dir.path().join("blobs").join("file"),
            key: kourier_model::BlobKey::new([0x42; 32]),
        },
        net_ids: Vec::new().into(),
    })
    .await;
    eventually(|| core_kinds(&dir).len() >= 3).await;
    assert_eq!(core_kinds(&dir), ["history", "history", "live"]);

    // The core answers everything it was given: the history goes on behind the live row.
    let mut seen_live = false;
    let mut history_after = 0;
    while history_after < 2 {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            match event {
                BackendEvent::AttachmentDone { .. } => seen_live = true,
                BackendEvent::HistoryBatch { .. } if seen_live => history_after += 1,
                _ => {}
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert!(seen_live);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The end of the actor ends the history driver and closes the stage file: a capture that comes later (a task of
/// a client that was replaced) fails, and its receipt is withheld.
#[tokio::test]
async fn a_quit_stops_the_history_driver_and_closes_the_stage() {
    let (mut h, client) = resumed_with_a_client().await;
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(5))
        .await
        .expect("captured");
    // The driver delivers; the core answers.
    let mut messages = 0;
    while messages < 5 {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            if let BackendEvent::HistoryBatch {
                messages: batch, ..
            } = event
            {
                messages += batch.len();
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert!(
        dir.path().join("history").join("stage.db").exists(),
        "a quit keeps the stage: the next start goes on from it"
    );
    let late = client
        .receive
        .capture
        .commit(LID, "n2", Some(0), chunk_of(1))
        .await;
    assert!(late.is_err(), "a capture after the actor ended was kept");
}

// ----- retention (W4, chunk C18) -----

/// Captures a chunk of `count` messages into the account's stage and answers the rows the driver hands the core
/// until every message was given to it.
async fn capture_and_deliver(h: &mut H, client: &Made, count: usize) {
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(count))
        .await
        .expect("captured");
    let mut messages = 0;
    while messages < count {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            if let BackendEvent::HistoryBatch {
                messages: batch, ..
            } = event
            {
                messages += batch.len();
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert!(
        h.dir.path().join("history").join("stage.db").exists(),
        "the stage is there before the account is logged out"
    );
}

/// The logout Kourier asks for ends the account for good: the stage with its chunks goes (the key goes with the
/// core's wipe, and a chunk is the account's own messages). A quit keeps it (above).
#[tokio::test]
async fn a_logout_removes_the_stage() {
    let (mut h, client) = resumed_with_a_client().await;
    capture_and_deliver(&mut h, &client, 2).await;
    h.send(Command::Logout).await;
    // The logout report, then the exit.
    loop {
        let mut envelope = h.envelope().await;
        let report = envelope
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::Logout(_)));
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        if report {
            break;
        }
    }
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert!(
        !dir.path().join("history").exists(),
        "a logged-out account kept its history stage"
    );
}

/// The same when the phone or the server ends the link.
#[tokio::test]
async fn a_server_logout_removes_the_stage() {
    let (mut h, client) = resumed_with_a_client().await;
    capture_and_deliver(&mut h, &client, 2).await;
    client.tap.deliver(WaSignal::LoggedOut(Logout {
        shape: LogoutShape::RemovedByPhone,
        violation: None,
        explanation: None,
    }));
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert!(
        !dir.path().join("history").exists(),
        "a logged-out account kept its history stage"
    );
}

/// A fresh link starts with no stage: what a logout whose removal never finished left in the directory is not
/// this account's.
#[tokio::test]
async fn a_fresh_link_does_not_inherit_a_stage_a_logout_left_behind() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("history")).expect("a leftover directory");
    std::fs::write(dir.path().join("history").join("stage.db"), b"left behind")
        .expect("a leftover");
    let h = H::start_in(dir, StartMode::Fresh);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert!(
        !dir.path().join("history").exists(),
        "a fresh link found a stage"
    );
}

// ----- reads here (W4, chunk C17) -----

/// What the unread hint of each chat of the rows the core is given says, by the chat's encoded key.
fn hints_of(events: &[BackendEvent], hints: &mut std::collections::BTreeMap<String, Option<u32>>) {
    for event in events {
        if let BackendEvent::ConversationUpsert(patch) = event {
            hints.insert(patch.key.encode(), patch.unread_hint);
        }
    }
}

/// Waits until the chat's `history_chats` row reads `(read_here, hint_stated, more)` = `want`. One read connection for
/// the whole wait; a failure says what the row was.
async fn history_chat_is(dir: &Path, conv: &kourier_model::ConvKey, want: (i64, i64, i64)) {
    let conn = kourier_db::open_read_only(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY))
        .expect("open");
    let key = conv.encode();
    let row = || {
        conn.query_row(
            "SELECT read_here, hint_stated, more FROM history_chats WHERE conv = ?1",
            [&key],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .ok()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while row() != Some(want) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chat's row is {:?}, not (read_here, hint_stated, more) = {want:?}",
            row()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// D6: a chat the user read on this computer before the history brought it gets no unread hint, though the phone's
/// count of it is above zero; a chat nobody read still gets it (the control), and a chunk that came before the
/// read stated its own once. The read is the core's `Command::SyncReadMarker`.
#[tokio::test]
async fn a_chat_read_between_two_chunks_gets_no_hint() {
    const READ: &str = "15550100102@s.whatsapp.net";
    const UNREAD: &str = "15550100103@s.whatsapp.net";
    let key_of = |jid: &str| match crate::history::map::Place::from_jid(jid).target {
        crate::history::map::Target::Conv(key) => key,
        other => panic!("a person's chat is placed as a conversation: {other:?}"),
    };
    // The wall clock: the first chunk is the phone's 50 %, so its push must still be running behind it.
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let mut hints = std::collections::BTreeMap::new();

    // Chunk 1: only PEER. Its hint is stated (the phone counts 3).
    let first = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 50)
        .chat(Chat::new(PEER).unread(3).texts("A", 1_700_000_100, 2))
        .blob();
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), first)
        .await
        .expect("captured");
    while !hints.contains_key(&key_of(PEER).encode()) {
        let mut envelope = h.envelope().await;
        hints_of(&envelope.events, &mut hints);
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert_eq!(
        hints[&key_of(PEER).encode()],
        Some(3),
        "the first chunk states its chat's count"
    );

    // The user reads READ here. The core tells the backend; the backend notes it.
    let read = key_of(READ);
    let target = kourier_model::MsgTarget::to_id(
        Some(read.clone()),
        &kourier_model::NetMsgId::remote(
            UserId::from_parts("pn", "15550100102").expect("a number"),
            "3EB0READ",
        ),
    );
    h.send(Command::SyncReadMarker {
        cmd: kourier_model::CmdId(1),
        conv: read.clone(),
        up_to: target.clone(),
        read: vec![target],
    })
    .await;
    let dir = h.dir.path().to_path_buf();
    // A row made by the note alone: read here, no hint stated, and no end owed (the history has not touched the chat).
    history_chat_is(&dir, &read, (1, 0, 0)).await;

    // A chat the first chunk touched is read too: its row, with the hint it stated, gains the note.
    let peer = key_of(PEER);
    let peer_target = kourier_model::MsgTarget::to_id(
        Some(peer.clone()),
        &kourier_model::NetMsgId::remote(
            UserId::from_parts("pn", "15550100101").expect("a number"),
            "3EB0PEER",
        ),
    );
    h.send(Command::SyncReadMarker {
        cmd: kourier_model::CmdId(2),
        conv: peer.clone(),
        up_to: peer_target.clone(),
        read: vec![peer_target],
    })
    .await;
    // Read here, the hint it stated, and its end still owed (the first chunk is the phone's 50 %).
    history_chat_is(&dir, &peer, (1, 1, 1)).await;

    // Chunk 2: READ (the phone counts 4) and UNREAD (counts 6).
    let second = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 100)
        .chat(Chat::new(READ).unread(4).texts("B", 1_700_000_200, 2))
        .chat(Chat::new(UNREAD).unread(6).texts("C", 1_700_000_300, 2))
        .blob();
    client
        .receive
        .capture
        .commit(LID, "n2", Some(0), second)
        .await
        .expect("captured");
    while !hints.contains_key(&key_of(UNREAD).encode()) || !hints.contains_key(&read.encode()) {
        let mut envelope = h.envelope().await;
        hints_of(&envelope.events, &mut hints);
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert_eq!(hints[&read.encode()], None, "a chat read here gets no hint");
    assert_eq!(
        hints[&key_of(UNREAD).encode()],
        Some(6),
        "a chat nobody read does"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- the lost history (W4, chunk C20) -----

/// Stage 2 recorded the notice of a chunk (by `id`): the phone announced it at `at` (ms), the harness clock's now for
/// a chunk of this run (the quit waits only for those).
async fn announce(store: &KourierStore, id: &'static str, at: u64) {
    store
        .write("test announce", move |tx| {
            tx.execute(
                "INSERT INTO history_announced(msg_id, announced_at) VALUES (?1, ?2)",
                (id, i64::try_from(at).unwrap_or(i64::MAX)),
            )
            .map_err(crate::error::sql("test announce"))?;
            Ok(())
        })
        .await
        .expect("announce");
}

/// Whether the actor's thread has ended.
fn ended(h: &H) -> bool {
    h.handle.thread.as_ref().is_none_or(|t| t.is_finished())
}

/// Waits until the quit's wait for the announced chunks has armed its bound on the actor's clock.
async fn bound_is_armed(h: &H) {
    let bound = h.clock.now_ms() + 15_000;
    eventually(|| h.clock.deadlines().contains(&bound)).await;
}

/// D2: a quit waits while a chunk the phone announced is not captured, until it is, and no longer than the bound; a
/// quit with nothing announced waits for nothing.
#[tokio::test]
async fn a_quit_waits_for_an_announced_chunk_and_no_longer_than_the_bound() {
    // The capture arrives during the wait: the quit goes on at once, long before the bound.
    let (h, client) = resumed_with_a_client().await;
    announce(&client.store, "n1", h.clock.now_ms()).await;
    h.send(Command::Shutdown).await;
    bound_is_armed(&h).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!ended(&h), "the quit did not wait for the announced chunk");
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(1))
        .await
        .expect("captured");
    assert_eq!(h.exit().await, Exit::Shutdown);

    // The chunk never comes: the quit ends at the bound, not before.
    let (h, client) = resumed_with_a_client().await;
    announce(&client.store, "n1", h.clock.now_ms()).await;
    h.send(Command::Shutdown).await;
    bound_is_armed(&h).await;
    h.clock.advance(Duration::from_secs(14));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!ended(&h), "14 s of the 15 s bound");
    h.clock.advance(Duration::from_secs(1));
    assert_eq!(h.exit().await, Exit::Shutdown);

    // Nothing announced: no wait, no bound armed.
    let (h, _client) = resumed_with_a_client().await;
    h.send(Command::Shutdown).await;
    let clock = h.clock.clone();
    assert_eq!(h.exit().await, Exit::Shutdown);
    assert!(
        !clock.deadlines().contains(&(clock.now_ms() + 15_000)),
        "a quit with nothing in flight armed a wait"
    );
}

/// The quit's wait is for a quit only: an account that is logged out leaves at once (its stage goes with it).
#[tokio::test]
async fn a_logout_does_not_wait_for_an_announced_chunk() {
    let (h, client) = resumed_with_a_client().await;
    announce(&client.store, "n1", h.clock.now_ms()).await;
    h.send(Command::Logout).await;
    let clock = h.clock.clone();
    let exit = h.exit().await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert!(!clock.deadlines().contains(&(clock.now_ms() + 15_000)));
}

/// A device row with the notices of an earlier run: the earlier run announced two chunks and captured one.
async fn pair_device_after_a_run_that_announced(dir: &Path) {
    pair_device(dir).await;
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-announced",
    )
    .expect("open");
    owner
        .store()
        .write("test announced", |tx| {
            tx.execute_batch(
                "INSERT INTO history_announced(msg_id, announced_at) VALUES ('old1', 1), ('old2', 1);",
            )
            .map_err(crate::error::sql("test announced"))
        })
        .await
        .expect("announced");
    owner.close();
}

/// D2: what an earlier run announced and never captured is counted lost when the next actor starts (once, before its
/// client exists); with no stage file every announced chunk is such.
#[tokio::test]
async fn an_announced_chunk_never_captured_is_counted_lost_at_the_next_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device_after_a_run_that_announced(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let lost = client
        .store
        .read(|conn| {
            conn.query_row("SELECT lost FROM history_counts", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(lost, 2);
    // A chunk announced in THIS run is not lost: its capture is on the way.
    announce(&client.store, "new1", h.clock.now_ms()).await;
    h.send(Command::Shutdown).await;
    bound_is_armed(&h).await;
    h.clock.advance(Duration::from_secs(15));
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let (lost, marked): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT lost FROM history_counts), (SELECT count(*) FROM history_announced WHERE lost = 1)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("count");
    assert_eq!(
        (lost, marked),
        (2, 2),
        "this run's announcement is not counted by its own quit"
    );
}

// ----- the notice of the lost history (W4, chunk C20b) -----

/// The counts of the `HistoryLost` notices in `events`.
fn lost_counts(events: &[BackendEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::Notice(BackendNotice::HistoryLost { count }) => Some(*count),
            _ => None,
        })
        .collect()
}

/// `(lost, lost_told)` of a stopped account's books.
fn lost_books(dir: &Path) -> (i64, i64) {
    let conn =
        kourier_db::open(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    conn.query_row("SELECT lost, lost_told FROM history_counts", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
    .expect("books")
}

/// R-Q2: what was lost is told to the core once per start, until the core has taken the notice; a start with
/// nothing owed tells nothing.
#[tokio::test]
async fn the_lost_history_is_told_once_per_start_until_the_core_took_the_notice() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device_after_a_run_that_announced(dir.path()).await;

    // Start 1: the notice is the first thing the core is given; the core does not take it.
    let mut h = H::start_in(dir, StartMode::Resume);
    let first = h.envelope().await;
    assert!(first.ack.is_some(), "a notice is durable");
    assert_eq!(lost_counts(&first.events), [2]);
    drop(first);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        lost_books(dir.path()),
        (2, 0),
        "a notice nobody took is not told"
    );

    // Start 2: it is told again (the left row and the new one); the core takes it this time.
    let mut h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let mut seen = 0;
    while seen < 2 {
        let mut envelope = h.envelope().await;
        seen += lost_counts(&envelope.events).len();
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    eventually_async(|| async {
        client
            .store
            .read(|conn| {
                conn.query_row("SELECT lost_told FROM history_counts", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(crate::error::sql("count"))
            })
            .await
            .is_ok_and(|told| told == 2)
    })
    .await;
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(lost_books(dir.path()), (2, 2));

    // Start 3: told, so nothing is owed and nothing is sent.
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let _client = h.client(0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(
        core_rows(dir.path()),
        0,
        "nothing was told: nothing was owed"
    );
}

/// Polls an async condition (10 s at most); a failure names the line that waited.
#[track_caller]
fn eventually_async<F, Fut>(cond: F) -> impl std::future::Future<Output = ()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let at = std::panic::Location::caller();
    async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cond().await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "it never happened (waited at {at})"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// The books of a running account: how many lost chunks the person was told of.
async fn lost_told_of(store: &KourierStore) -> i64 {
    store
        .read(|conn| {
            conn.query_row("SELECT lost_told FROM history_counts", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count")
}

/// The person counts as told when the core took THE notice's row, not when it took an earlier row: the left row of a
/// start before is shipped and taken first (the core refused the pair, so the rows go one by one), the new notice's row
/// is held back, and nothing is told until that one is taken too.
#[tokio::test]
async fn an_earlier_row_the_core_took_does_not_tell_the_person() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device_after_a_run_that_announced(dir.path()).await;
    // Start 1 leaves a notice row the core never took.
    let mut h = H::start_in(dir, StartMode::Resume);
    let first = h.envelope().await;
    assert_eq!(lost_counts(&first.events), [2]);
    drop(first);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    // Start 2 puts a second one behind it.
    let mut h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let mut both = h.envelope().await;
    assert_eq!(lost_counts(&both.events), [2, 2]);
    both.ack
        .take()
        .expect("durable")
        .complete(Err(kourier_model::PersistError {
            retryable: false,
            detail: "the pair is refused, row by row then".to_owned(),
        }));
    // The older row is taken alone.
    let mut older = h.envelope().await;
    assert_eq!(lost_counts(&older.events), [2]);
    older.ack.take().expect("durable").complete(Ok(()));
    // The newer one, this start's notice, is not answered (a retryable failure: it ships again).
    let held = h.envelope().await;
    assert_eq!(lost_counts(&held.events), [2]);
    drop(held);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        lost_told_of(&client.store).await,
        0,
        "the core took an EARLIER row, not the notice of this start"
    );
    let mut again = h.envelope().await;
    assert_eq!(lost_counts(&again.events), [2]);
    again.ack.take().expect("durable").complete(Ok(()));
    eventually_async(|| async { lost_told_of(&client.store).await == 2 }).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- the send (W5a, chunk 9a) -----

const SEND_PEER: &str = "15550194736028";
const SEND_LOCAL: &str = "3EB0D9A37B1C64E82057";

/// Plants a send outcome (conv = the chat with [`SEND_PEER`]) recorded at `at`.
async fn plant_outcome(dir: &Path, local: &str, at: i64) {
    let owner = StoreOwner::open(
        &dir.join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test-outcome",
    )
    .expect("open");
    let entry = crate::send::ledger::Entry {
        conv: kourier_model::ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap()),
        local: local.to_owned(),
        kind: crate::send::ledger::Kind::Message,
        target: None,
        outcome: crate::send::ledger::Outcome::Acked {
            server_t: Some(1_799_999_999),
        },
    };
    owner
        .store()
        .write("test: plant", move |tx| {
            crate::send::ledger::record(tx, &entry, at)
        })
        .await
        .expect("plant");
    owner.close();
}

fn outcome_locals(dir: &Path) -> Vec<String> {
    let conn =
        kourier_db::open(&dir.join("backend.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    let mut stmt = conn
        .prepare("SELECT local FROM send_outcomes ORDER BY local")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .expect("locals")
}

/// D4: the send outcomes older than 14 days go at a start; a younger one stays. Mutant: the start sweep removed.
#[tokio::test]
async fn a_start_forgets_the_send_outcomes_older_than_fourteen_days() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let now = i64::try_from(NOW).expect("time");
    plant_outcome(
        dir.path(),
        "3EB0OLD0000048392761",
        now - crate::send::ledger::KEEP_MS - 1,
    )
    .await;
    plant_outcome(dir.path(), "3EB0YOUNG00059403872", now - 1000).await;
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    assert_eq!(outcome_locals(dir.path()), ["3EB0YOUNG00059403872"]);
}

/// W5b (G4): the upload records older than 30 days go at a start; a younger one stays. Mutant: the start sweep removed.
#[tokio::test]
async fn a_start_forgets_the_upload_records_older_than_thirty_days() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let now = i64::try_from(NOW).expect("time");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test",
    )
    .expect("open");
    owner
        .store()
        .write("test: plant", move |tx| {
            let conv = kourier_model::ConvKey::Direct(
                UserId::from_parts("lid", "77700172839405").unwrap(),
            );
            let pointer = crate::media::MediaPointer {
                media_type: "document".into(),
                direct_path: "/v/t62.7119-24/synthetic-path-30491827".into(),
                media_key: "0".repeat(64),
                file_sha256: None,
                file_enc_sha256: None,
                file_length: 3,
                media_key_timestamp: None,
                static_url: None,
                thumb_jpeg: None,
                ..crate::media::MediaPointer::default()
            };
            for (local, at) in [
                (
                    "3EB0OLD0000061728394",
                    now - crate::send::record::KEEP_MS - 1,
                ),
                ("3EB0YOUNG00072839405", now - 1000),
            ] {
                crate::send::record::put(tx, &conv, local, &pointer, at)?;
            }
            Ok(())
        })
        .await
        .expect("plant");
    owner.close();
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let mut stmt = conn
        .prepare("SELECT local FROM media_uploads ORDER BY local")
        .expect("prepare");
    let locals: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .expect("locals");
    assert_eq!(locals, ["3EB0YOUNG00072839405"]);
}

/// W6a1 (A6, critic L6): the rows that say which chat a call's line went to are kept 30 days (by `created_at`); older
/// ones go at a start, a younger one stays. Mutant: the start sweep removed.
#[tokio::test]
async fn a_start_forgets_the_call_lines_older_than_thirty_days() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let now = i64::try_from(NOW).expect("time");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test",
    )
    .expect("open");
    owner
        .store()
        .write("test: plant", move |tx| {
            for (id, written) in [
                (
                    "OLD-CALL-61830001",
                    now - crate::store::call_lines::KEEP_MS - 1,
                ),
                ("YOUNG-CALL-61830002", now - 1000),
            ] {
                crate::store::call_lines::note(
                    tx,
                    id,
                    "direct:lid:77700172839405",
                    written,
                    written,
                )?;
            }
            Ok(())
        })
        .await
        .expect("plant");
    owner.close();
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let mut stmt = conn
        .prepare("SELECT call_id FROM call_lines ORDER BY call_id")
        .expect("prepare");
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .expect("ids");
    assert_eq!(ids, ["YOUNG-CALL-61830002"]);
}

/// Fix F14: the receipt jobs older than 14 days (by `at`) go at a start; a younger one stays. Mutant: the sweep not
/// wired into the start.
#[tokio::test]
async fn a_start_forgets_the_receipt_jobs_older_than_fourteen_days() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let now = i64::try_from(NOW).expect("time");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
        "wa-test",
    )
    .expect("open");
    owner
        .store()
        .write("test: plant", move |tx| {
            for (cmd, at) in [
                (1_i64, now - crate::send::receipts::JOB_KEEP_MS - 1),
                (2, now - 1000),
            ] {
                tx.execute(
                    "INSERT INTO receipt_jobs(cmd, conv, kind, targets, at) VALUES (?1, 'direct:pn:x', 'read', x'5b5d', ?2)",
                    kourier_db::rusqlite::params![cmd, at],
                )
                .map_err(crate::error::sql("test: plant"))?;
            }
            Ok(())
        })
        .await
        .expect("plant");
    owner.close();
    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let mut stmt = conn
        .prepare("SELECT cmd FROM receipt_jobs ORDER BY cmd")
        .expect("prepare");
    let left: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .and_then(|rows| rows.collect())
        .expect("jobs");
    assert_eq!(left, [2], "the old job went, the young one stayed");
}

/// The actor answers a `Transmit` from its attempt (`send::attempt`), off its loop: while the frame waits for its
/// ack the actor serves other commands, a `Disconnect` ends the connection and its wait (transient, D5), and after
/// the `Connect` the resend under the same id is written again and answered `Sent` once acked. Mutants: the arm
/// answers `Unsupported` again; the disconnect does not end the registry's connection.
#[tokio::test]
async fn a_transmit_runs_off_the_loop_and_a_disconnect_ends_its_wait() {
    use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
    use kourier_model::{Body, ConvKey, Text};

    let (h, client) = resumed_with_a_client().await;
    // The lock is known (no hold): sends are not held back (D8).
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap());
    let started = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &started, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
    let request = OutgoingRequest {
        conv,
        local: SEND_LOCAL.into(),
        sent_at: i64::try_from(NOW).expect("time") - 1000,
        kind: OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic hello")),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    };
    let transmit = |request: &OutgoingRequest| {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let command = Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(request.clone()),
            reply,
        };
        (command, answer)
    };
    let answered = |answer: tokio::sync::oneshot::Receiver<TransmitOutcome>| async move {
        tokio::time::timeout(Duration::from_secs(10), answer)
            .await
            .expect("the actor answers")
            .expect("an answer")
    };
    let (command, answer) = transmit(&request);
    h.send(command).await;
    eventually(|| client.session.sent().len() == 1).await;
    assert_eq!(client.session.sent()[0].id, SEND_LOCAL);
    h.send(Command::Disconnect).await;
    eventually(|| client.session.count(&Call::Pause) == 1).await;
    let first = answered(answer).await;
    assert!(
        matches!(first, TransmitOutcome::Failed(ref e) if e.kind == ErrorKind::Transient { retry_after: None }),
        "the disconnect ended the connection before any ack: {first:?}"
    );
    h.send(Command::Connect).await;
    eventually(|| client.session.count(&Call::Resume) == 1).await;
    let (command, answer) = transmit(&request);
    h.send(command).await;
    eventually(|| client.session.sent().len() == 2).await;
    assert_eq!(
        client.session.sent()[1].id,
        SEND_LOCAL,
        "the resend on the later connection keeps the id"
    );
    client.receive.acks.answer(crate::send::acks::Ack {
        id: SEND_LOCAL.to_owned(),
        answer: crate::send::acks::Answer::Acked {
            server_t: Some(1_800_000_002),
        },
    });
    assert_eq!(
        answered(answer).await,
        TransmitOutcome::Sent {
            server_at: Some(1_800_000_002_000),
            per_recipient: Vec::new(),
        }
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D19 (critic M1): a normal quit waits for the ack of a frame already written (here held back 1 s) and commits it
/// to the ledger before the client stops; the core's resend at the next start is answered `Sent` from the ledger
/// with NO frame. Mutant: the quit's wait for the attempts removed.
#[tokio::test]
async fn a_quit_waits_for_the_ack_of_a_frame_in_the_air_and_the_next_start_sends_nothing() {
    use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
    use kourier_model::{Body, ConvKey, Text};

    let (h, client) = resumed_with_a_client().await;
    // The lock is known (no hold): sends are not held back (D8).
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap());
    let started = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &started, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
    let request = OutgoingRequest {
        conv,
        local: SEND_LOCAL.into(),
        sent_at: i64::try_from(NOW).expect("time") - 1000,
        kind: OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic hello")),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    };
    let transmit = |request: &OutgoingRequest| {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let command = Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(request.clone()),
            reply,
        };
        (command, answer)
    };
    let acked = TransmitOutcome::Sent {
        server_at: Some(1_800_000_007_000),
        per_recipient: Vec::new(),
    };
    let (command, answer) = transmit(&request);
    h.send(command).await;
    eventually(|| client.session.sent().len() == 1).await;
    let acks = client.receive.acks.clone();
    let late = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        acks.answer(crate::send::acks::Ack {
            id: SEND_LOCAL.to_owned(),
            answer: crate::send::acks::Answer::Acked {
                server_t: Some(1_800_000_007),
            },
        });
    });
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    late.await.expect("the ack was given");
    assert_eq!(
        answer.await.expect("the quit let the attempt answer"),
        acked
    );
    let conn = kourier_db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let outcome: String = conn
        .query_row(
            "SELECT outcome FROM send_outcomes WHERE local = ?1",
            [SEND_LOCAL],
            |r| r.get(0),
        )
        .expect("the quit committed the ack to the ledger");
    assert_eq!(outcome, "acked");
    drop(conn);

    let h = H::start_in(dir, StartMode::Resume);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let (command, answer) = transmit(&request);
    h.send(command).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), answer)
            .await
            .expect("the actor answers")
            .expect("an answer"),
        acked
    );
    assert!(
        client.session.sent().is_empty(),
        "the resend after the quit must write no frame"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A chat the phone started and a lock that is known, on a connected client: what an actor test needs before a send
/// goes out (D8).
async fn ready_to_send(h: &H, client: &Made) -> kourier_model::ConvKey {
    use kourier_model::ConvKey;

    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap());
    let started = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &started, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
    conv
}

/// A synthetic picture in the core's blob store, as the one file of a message: the `OutgoingKind` that names it by
/// path and key, the way the core hands a file to the backend.
fn a_picture_in_the_blobs(h: &H) -> kourier_backend_api::OutgoingKind {
    use kourier_model::{Attachment, AttachmentKind, BlobKey, BlobRef, Body};
    use std::io::Write as _;

    let blobs = h.dir.path().join("blobs");
    std::fs::create_dir_all(blobs.join("ab")).expect("blob dir");
    let rel = std::path::PathBuf::from("ab").join("core-file-60839415");
    let bytes =
        kourier_media::sample_image(320, 240, 52_830_174, kourier_media::SampleFormat::Jpeg)
            .expect("sample");
    let mut writer = kourier_blob::BlobWriter::create(
        &blobs.join(&rel),
        &kourier_blob::FileKey::from_bytes([0x5D; 32]),
    )
    .expect("writer");
    writer.write_all(&bytes).expect("write");
    writer.finish_volatile().expect("finish");
    kourier_backend_api::OutgoingKind::Message {
        body: Body::Media {
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                size: bytes.len() as u64,
                local: Some(BlobRef {
                    path: rel,
                    key: BlobKey::new([0x5D; 32]),
                }),
                ..Attachment::default()
            }],
            caption: None,
        },
        reply_to: None,
        expiry: None,
    }
}

/// Fix F5, the wiring (re-check N2): the actor hands every attempt a clone of ONE `Lingering`, so the second `Transmit`
/// of a local finds the POST thread the first one left behind (the attempt-level test shares the set by hand and cannot
/// see this). The first upload leaves its thread running and answers `Transient{STALL_RETRY}`; the second `Transmit`
/// of the same local answers the same floor with no second upload call and no frame. Mutant: every attempt gets its own
/// `Lingering::default()` (the second one uploads and writes a frame).
#[tokio::test]
async fn a_second_transmit_of_a_local_waits_while_the_first_ones_post_thread_runs() {
    use crate::send::route::Shape;
    use kourier_backend_api::TransmitOutcome;

    let (h, client) = resumed_with_a_client().await;
    let _conv = ready_to_send(&h, &client).await;
    let end = client.session.linger_next_upload();
    let floor = ErrorKind::Transient {
        retry_after: Some(crate::send::upload::STALL_RETRY),
    };
    let picture = a_picture_in_the_blobs(&h);
    let (command, first) = transmit_of(1, picture.clone());
    h.send(command).await;
    let TransmitOutcome::Failed(e) = answered(first).await else {
        panic!("an upload that did not complete does not succeed");
    };
    assert_eq!(e.kind, floor, "the first upload waits its floor");
    assert_eq!(client.session.count(&Call::Upload(Shape::Image)), 1);

    let (command, second) = transmit_of(1, picture);
    h.send(command).await;
    let TransmitOutcome::Failed(e) = answered(second).await else {
        panic!("a message whose earlier POST still runs does not succeed");
    };
    assert_eq!(e.kind, floor, "the resend waits for the thread");
    assert_eq!(
        client.session.count(&Call::Upload(Shape::Image)),
        1,
        "no second upload while the first thread runs"
    );
    assert!(client.session.sent().is_empty(), "no frame");
    end.send(()).expect("the parked thread listens");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F16 (robustness MEDIUM, Q-B16): a quit while a picture uploads (the CDN holds the POST) does not wait for the
/// attempt's ack bound, which the attempt would never use (it has written nothing, and waits for nothing the quit
/// needs). The attempt ends at the quit with `Failed(Transient)` in the quit's words (the core sends the message again
/// at the next start; the person's `Internal` stop would fail it for good), BEFORE the exit, and the exit comes far
/// inside `QUIT_ACK_WAIT`. Mutants: the quit that does not tell the attempts (`quit_all` removed: the exit waits the
/// whole bound and the reply is dropped); the answer `stopped()`.
#[tokio::test]
async fn a_quit_ends_an_upload_under_way_at_once_with_a_transient_answer() {
    use crate::session::fake::UploadHold;
    use kourier_backend_api::TransmitOutcome;

    let (h, client) = resumed_with_a_client().await;
    let _conv = ready_to_send(&h, &client).await;
    let kind = a_picture_in_the_blobs(&h);
    let _gate = client.session.hold_uploads(UploadHold::AfterBytes(1024));
    let (command, answer) = transmit_of(1, kind);
    h.send(command).await;
    eventually(|| {
        client
            .session
            .count(&Call::Upload(crate::send::route::Shape::Image))
            == 1
    })
    .await;
    let quit = std::time::Instant::now();
    h.send(Command::Shutdown).await;
    let (exit, _dir) = h.exit_keeping_the_files().await;
    let took = quit.elapsed();
    assert_eq!(exit, Exit::Shutdown);
    assert!(
        took < super::QUIT_ACK_WAIT,
        "the quit waited for an attempt that had written nothing ({took:?})"
    );
    let TransmitOutcome::Failed(e) = answered(answer).await else {
        panic!("a quit upload does not succeed");
    };
    assert_eq!(e.kind, ErrorKind::Transient { retry_after: None });
    assert_eq!(e.detail, crate::send::stop::quitting().detail);
    assert!(client.session.sent().is_empty(), "no frame after the quit");
}

/// Fix F16 (4): what the quit gave up waiting for is ended BEFORE the client stops, so no task of the attempt holds the
/// client into the store's release wait. Here a text's frame is written, its ack never comes, and after the transient
/// answer (`ACK_WAIT`) the attempt's late listener hangs in `drop_connection` for longer than `QUIT_ACK_WAIT`: the quit
/// gives up on it and ends it, so when `stop` runs the listener's wait on the gate (its clone of the gate) is gone.
/// Mutant: the `abort_all` after the wait removed (the attempt lives until the actor is dropped).
#[tokio::test]
async fn a_quit_ends_the_attempts_it_gave_up_waiting_for_before_the_client_stops() {
    use kourier_backend_api::OutgoingKind;
    use kourier_model::{Body, Text};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (h, client) = resumed_with_a_client().await;
    let _conv = ready_to_send(&h, &client).await;
    let (command, _answer) = transmit_of(
        1,
        OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic hello")),
            reply_to: None,
            expiry: None,
        },
    );
    h.send(command).await;
    eventually(|| client.session.sent().len() == 1).await;
    // No ack: at `ACK_WAIT` the attempt answers transient and its late listener ends the connection, which the
    // double holds.
    let drops = client.session.hold_drops();
    eventually(|| client.session.count(&Call::DropConnection) == 1).await;
    let alive = Arc::strong_count(&drops);
    assert!(alive >= 3, "the listener waits on the gate: {alive}");
    let seen = Arc::new(AtomicUsize::new(usize::MAX));
    client.session.on_stop({
        let (gate, seen) = (Arc::downgrade(&drops), seen.clone());
        move || seen.store(gate.strong_count(), Ordering::SeqCst)
    });
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
    assert_eq!(
        seen.load(Ordering::SeqCst),
        alive - 1,
        "the attempt was still alive when the client stopped"
    );
}

/// D10 (brief §2.7, critic L2): `RequestAttachments` (W7) keeps its `pending_commands` row: the backend confirms it
/// (no `CommandPersisted`) and reports no restore, so the core sends it again to the step that carries it out. (The
/// clear was split off in W6a1: `a_clear_is_confirmed_in_one_zone_that_forgets_the_chats_upload_records`.)
/// Mutant: its arm confirms.
#[tokio::test]
async fn request_attachments_stays_pending() {
    use kourier_model::{CmdId, ConvKey, MsgTarget, TargetAuthor};

    let (mut h, _client) = resumed_on_the_wall_clock().await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", "15550127604913").expect("a number"));
    let target = MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::Own,
        local: "3EB0C2E86A4F19D73350".into(),
        alternates: Vec::new(),
    };
    h.send(Command::RequestAttachments {
        cmd: CmdId(42),
        conv,
        target,
    })
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    while let Ok(Some(mut envelope)) = tokio::time::timeout_at(deadline, h.ends.events.recv()).await
    {
        for event in &envelope.events {
            assert!(
                !matches!(
                    event,
                    BackendEvent::CommandPersisted { .. } | BackendEvent::AttachmentRestore { .. }
                ),
                "a command W5a does not carry out must stay pending, but the backend said {}",
                event.name()
            );
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W6a1 (D14): `ClearConversation` is confirmed (`CommandPersisted`) in ONE zone that also deletes the chat's
/// `media_uploads` rows (and only that chat's); the phone is told nothing. A delete of the chat (`remove: true`) is
/// the same. The confirmation reaches the core only after the delete committed. The zone notes the chat's CUT too
/// (F3, INTEROP-4): the larger of the command's two times, else the time of the confirmation. Mutants: the arm confirms
/// without the delete; the delete takes every chat's rows; no cut is noted; the cut is the first time only.
#[tokio::test]
async fn a_clear_is_confirmed_in_one_zone_that_forgets_the_chats_upload_records() {
    use kourier_model::{CmdId, ConvKey, MsgTarget, TargetAuthor};

    let started_ms = kourier_db::now_ms();
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let chat = ConvKey::Direct(UserId::from_parts("pn", "15550127604913").expect("a number"));
    let other = ConvKey::Direct(UserId::from_parts("pn", "15550163829047").expect("a number"));
    let rows: Vec<(String, String)> = vec![
        ("3EB0C2E86A4F19D73350".into(), chat.encode()),
        ("3EB0C2E86A4F19D73351".into(), chat.encode()),
        ("3EB0C2E86A4F19D73352".into(), other.encode()),
    ];
    client
        .store
        .write("test: seed upload records", move |tx| {
            for (local, conv) in &rows {
                tx.execute(
                    "INSERT INTO media_uploads(local, conv, pointer, at) VALUES (?1, ?2, x'7b7d', 1)",
                    (local, conv),
                )
                .map_err(crate::error::sql("seed"))?;
            }
            Ok(())
        })
        .await
        .expect("seed");
    let target = MsgTarget {
        conv: Some(chat.clone()),
        author: TargetAuthor::Own,
        local: "3EB0C2E86A4F19D73350".into(),
        alternates: Vec::new(),
    };
    let left = |store: crate::store::KourierStore| async move {
        store
            .read(|conn| {
                let mut stmt = conn
                    .prepare("SELECT local FROM media_uploads ORDER BY local")
                    .map_err(crate::error::sql("read"))?;
                stmt.query_map([], |r| r.get::<_, String>(0))
                    .map_err(crate::error::sql("read"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(crate::error::sql("read"))
            })
            .await
            .expect("read")
    };
    let clear = |cmd: i64, conv: &ConvKey, remove: bool, times: (Option<i64>, Option<i64>)| {
        Command::ClearConversation {
            cmd: CmdId(cmd),
            conv: conv.clone(),
            newest: vec![target.clone()],
            newest_non_expiring: vec![target.clone()],
            remove,
            before_ms: times.0,
            system_before_ms: times.1,
        }
    };
    let cut_of = |conv: ConvKey| {
        let store = client.store.clone();
        async move {
            store
                .read(move |conn| crate::store::chat_state::cut_of(conn, &conv))
                .await
                .expect("cut_of")
        }
    };

    h.send(clear(
        41,
        &chat,
        false,
        (Some(1_761_400_000_000), Some(1_761_500_000_000)),
    ))
    .await;
    let mut envelope = h.envelope().await;
    assert!(
        matches!(
            envelope.events.as_slice(),
            [BackendEvent::CommandPersisted { cmd }] if *cmd == CmdId(41)
        ),
        "the clear is confirmed"
    );
    assert_eq!(
        left(client.store.clone()).await,
        ["3EB0C2E86A4F19D73352"],
        "and when the core hears it the chat's records are already gone, the other chat's kept"
    );
    if let Some(ack) = envelope.ack.take() {
        ack.complete(Ok(()));
    }

    // INTEROP-4 (F3): the chat's cut is the larger of the command's two times, the core's own rule.
    assert_eq!(
        cut_of(chat.clone()).await,
        Some(1_761_500_000_000),
        "the larger of the two times is the chat's cut"
    );
    h.send(clear(43, &other, true, (None, None))).await;
    let mut envelope = h.envelope().await;
    assert!(
        matches!(
            envelope.events.as_slice(),
            [BackendEvent::CommandPersisted { cmd }] if *cmd == CmdId(43)
        ),
        "a delete of a chat is confirmed the same way"
    );
    assert!(left(client.store.clone()).await.is_empty());
    // Nothing went (both times None): the cut is the time of the confirmation, as the core's own cut is "now".
    assert!(
        cut_of(other.clone())
            .await
            .is_some_and(|cut| cut >= started_ms),
        "a clear that removed nothing cuts at the time it was confirmed"
    );
    if let Some(ack) = envelope.ack.take() {
        ack.complete(Ok(()));
    }
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- the reach-out lock before Syncing and the send gate (W5a chunk 10, decision D8) -----

const HOLD_PEER: &str = "15550138462951";
const HOLD_LOCAL: &str = "3EB0E4C82A6F19D37046";

/// A chat the phone started with [`HOLD_PEER`] (D7 lets it send) and a text into it.
async fn started_hold_chat(client: &Made) -> kourier_backend_api::OutgoingRequest {
    use kourier_backend_api::{OutgoingKind, OutgoingRequest};
    use kourier_model::{Body, ConvKey, Text};

    let conv = ConvKey::Direct(UserId::from_parts("pn", HOLD_PEER).unwrap());
    let started = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &started, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
    OutgoingRequest {
        conv,
        local: HOLD_LOCAL.into(),
        sent_at: i64::try_from(NOW).expect("time") - 1000,
        kind: OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic held text")),
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    }
}

/// The actor's answer to a `Transmit` of `request`.
async fn transmit_answer(
    h: &H,
    request: &kourier_backend_api::OutgoingRequest,
) -> tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    h.send(Command::Transmit {
        local: kourier_model::MessageId(1),
        req: Box::new(request.clone()),
        reply,
    })
    .await;
    answer
}

async fn answered(
    answer: tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome>,
) -> kourier_backend_api::TransmitOutcome {
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("the actor answers")
        .expect("an answer")
}

/// The answer a send gets while the lock holds it back (D8).
fn held_back(outcome: &kourier_backend_api::TransmitOutcome) -> bool {
    matches!(
        outcome,
        kourier_backend_api::TransmitOutcome::Failed(e)
            if e.kind == ErrorKind::Transient { retry_after: Some(crate::send::attempt::HOLD_RETRY) }
    )
}

/// D8 (the contract's K-A fix C1): at the first connect of a run the pulled hold is stated, to the core as the
/// ephemeral `AccountHold` and to the log, BEFORE the connection shows `Syncing`. Mutants: the state published
/// first; the statement only logged (`LogFacts` kept).
#[tokio::test]
async fn the_hold_is_stated_before_syncing() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let mut h = H::start_in(dir, StartMode::Resume);
    let until_ms = Some(NOW + 3_600_000);
    let scripted = Arc::new(FakeSession::new());
    scripted.script_hold(Ok(HoldState::Active {
        until_ms,
        kind: None,
    }));
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    let hold = AccountHold {
        until_ms,
        reason: HoldReason::ReachOutLocked,
    };
    // The actor publishes the state (`set_conn`, which wakes `conn_is`) and records it as a fact on the NEXT
    // line (`signal`): a read of the facts right after `conn_is` can come in between and find no Syncing fact
    // (strike 2 of this test, 2026-10-28, a loaded server). Wait for the fact (bounded: `eventually` polls for
    // 10 s), then read the facts ONCE: the vector is appended in order, so a snapshot that holds the Syncing
    // fact holds everything stated before it.
    let is_syncing =
        |f: &Fact| matches!(f, Fact::State(s) if matches!(s.conn, ConnState::Syncing { .. }));
    eventually(|| h.facts().iter().any(is_syncing)).await;
    let facts = h.facts();
    let stated = facts.iter().position(|f| *f == Fact::Hold(Some(hold)));
    let syncing = facts
        .iter()
        .position(is_syncing)
        .expect("the connection shows Syncing");
    assert!(
        stated.is_some_and(|stated| stated < syncing),
        "the hold must be stated before Syncing (the core may send from Syncing on): it stands at {stated:?}, Syncing at {syncing}, the facts {facts:?}"
    );
    loop {
        let envelope = h.raw_envelope().await;
        if is_hold_statement(&envelope) {
            assert!(
                matches!(envelope.events.as_slice(), [BackendEvent::AccountHold(Some(got))] if *got == hold),
                "the core is told the pulled hold"
            );
            break;
        }
    }
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D8: a pull that does not answer holds `Syncing` back for at most `HOLD_FIRST_WAIT` (10 s on the actor's clock),
/// then the connection goes on with nothing stated, and a `Transmit` is answered `Transient{HOLD_RETRY}` with NO
/// frame until the pull answers; after the answer (no hold) the send is written. Mutants: `Syncing` without the
/// wait; the gate open while the lock is unknown.
#[tokio::test]
async fn a_transmit_waits_while_the_lock_is_unknown() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let scripted = Arc::new(FakeSession::new());
    let pull = scripted.hold_pulls();
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let request = started_hold_chat(&client).await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| client.session.count(&Call::FetchHold) == 1).await;
    eventually(|| h.clock.deadlines().contains(&(NOW + 10_000))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !matches!(h.conn(), ConnState::Syncing { .. }),
        "Syncing must wait for the pull (up to 10 s)"
    );
    h.clock.advance(Duration::from_secs(10));
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    assert!(
        !h.facts().iter().any(|f| matches!(f, Fact::Hold(_))),
        "nothing is stated before the pull answers"
    );
    let first = answered(transmit_answer(&h, &request).await).await;
    assert!(
        held_back(&first),
        "a send into an unknown lock must wait: {first:?}"
    );
    assert!(
        client.session.sent().is_empty(),
        "no frame while the lock is unknown"
    );
    // The pull answers (no hold): the send goes out.
    pull.notify_one();
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let answer = transmit_answer(&h, &request).await;
    eventually(|| client.session.sent().len() == 1).await;
    client.receive.acks.answer(crate::send::acks::Ack {
        id: HOLD_LOCAL.to_owned(),
        answer: crate::send::acks::Answer::Acked { server_t: None },
    });
    assert!(matches!(
        answered(answer).await,
        kourier_backend_api::TransmitOutcome::Sent { .. }
    ));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D8, THE END OF "UNKNOWN" (critic H3): a pull that fails for ever holds the sends back for 10 minutes since the
/// run first asked, then "unknown" counts as "no hold" (nothing is stated; the retry goes on). On the actor's
/// clock. Mutant: the end removed.
#[tokio::test]
async fn a_pull_that_fails_for_ever_holds_sends_ten_minutes_and_then_lets_them_go() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let scripted = Arc::new(FakeSession::new());
    for _ in 0..20 {
        scripted.script_hold(Err(BackendError::transient("no answer")));
    }
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let request = started_hold_chat(&client).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    let first = answered(transmit_answer(&h, &request).await).await;
    assert!(held_back(&first), "the lock is unknown: {first:?}");
    h.clock.advance(Duration::from_secs(540));
    eventually(|| client.session.count(&Call::FetchHold) >= 2).await;
    let nine = answered(transmit_answer(&h, &request).await).await;
    assert!(
        held_back(&nine),
        "nine minutes of failed pulls still hold the send: {nine:?}"
    );
    h.clock.advance(Duration::from_secs(61));
    let answer = transmit_answer(&h, &request).await;
    eventually(|| client.session.sent().len() == 1).await;
    client.receive.acks.answer(crate::send::acks::Ack {
        id: HOLD_LOCAL.to_owned(),
        answer: crate::send::acks::Answer::Acked { server_t: None },
    });
    assert!(matches!(
        answered(answer).await,
        kourier_backend_api::TransmitOutcome::Sent { .. }
    ));
    assert!(
        !h.facts().iter().any(|f| matches!(f, Fact::Hold(_))),
        "the end of unknown states no hold and no lift"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// The lock the pushes of the F6 tests state: in force for an hour of the actor's clock.
fn pushed_lock(now: u64) -> (HoldState, AccountHold) {
    let until_ms = Some(now + 3_600_000);
    (
        HoldState::Active {
            until_ms,
            kind: None,
        },
        AccountHold {
            until_ms,
            reason: HoldReason::ReachOutLocked,
        },
    )
}

/// Fix F6 (privacy LOW): an attempt passed the gate while no lock was known; while it awaits the account's mode a lock
/// is PUSHED (another frame earned a 463): its frame is not written and it is answered `Transient{HOLD_RETRY}` (D8).
/// Mutants: the second look removed from the attempt; the actor's view not updated by a push.
#[tokio::test]
async fn a_lock_pushed_while_an_attempt_is_underway_stops_its_frame() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let scripted = Arc::new(FakeSession::new());
    let slow = scripted.hold_migrated();
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let request = started_hold_chat(&client).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let answer = transmit_answer(&h, &request).await;
    eventually(|| client.session.count(&Call::LidMigrated) == 1).await;
    let (state, hold) = pushed_lock(NOW);
    client.tap.deliver(WaSignal::Hold(state));
    eventually(|| h.facts().contains(&Fact::Hold(Some(hold)))).await;
    slow.notify_one();
    let outcome = answered(answer).await;
    assert!(
        held_back(&outcome),
        "the attempt met a lock pushed meanwhile: {outcome:?}"
    );
    assert!(
        client.session.sent().is_empty(),
        "a frame was written into a lock the account knew"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F6: the same for a typing state: it passed the actor's gate, its task awaits the account's mode, a lock is
/// pushed: nothing is written. The first state (no push) goes out, so the check can fail. Mutants: the second look
/// removed from `typing`.
#[tokio::test]
async fn a_lock_pushed_while_a_typing_state_is_underway_drops_it() {
    use kourier_model::ConvKey;

    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let wall = u64::try_from(kourier_db::now_ms()).expect("a time");
    let h = H::start_with_clock(
        dir,
        StartMode::Resume,
        BackendConfig::default(),
        FakeClock::new(wall),
    );
    let scripted = Arc::new(FakeSession::new());
    let slow = scripted.hold_migrated();
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", TYPING_PEER).expect("a number"));
    start_chat(&client, &conv).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    // No push: the state is written.
    h.send(Command::SetTyping {
        conv: conv.clone(),
        on: true,
    })
    .await;
    eventually(|| client.session.count(&Call::LidMigrated) == 1).await;
    slow.notify_one();
    eventually(|| client.session.typed().len() == 1).await;
    // A push while the next one awaits the mode: dropped.
    h.send(Command::SetTyping { conv, on: false }).await;
    eventually(|| client.session.count(&Call::LidMigrated) == 2).await;
    let (state, hold) = pushed_lock(wall);
    client.tap.deliver(WaSignal::Hold(state));
    eventually(|| h.facts().contains(&Fact::Hold(Some(hold)))).await;
    slow.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        client.session.typed().len(),
        1,
        "a typing state was written into a lock the account knew"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F6: a pull asked BEFORE a push and answered after it is older than the push: its "no lock" must not overwrite
/// the pushed lock (the select of the loop may take them in either order). The gate stays closed, the core is not told
/// "no lock", and a Transmit writes no frame. Mutants: the pull answer overwrites unconditionally; the pushes are not
/// counted.
#[tokio::test]
async fn a_pull_answer_older_than_a_push_does_not_open_the_gate() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let h = H::start_in(dir, StartMode::Resume);
    let scripted = Arc::new(FakeSession::new());
    let pull = scripted.hold_pulls();
    scripted.script_hold(Ok(HoldState::Lifted));
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let request = started_hold_chat(&client).await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| client.session.count(&Call::FetchHold) == 1).await;
    eventually(|| h.clock.deadlines().contains(&(NOW + 10_000))).await;
    h.clock.advance(Duration::from_secs(10));
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    let (state, hold) = pushed_lock(NOW);
    client.tap.deliver(WaSignal::Hold(state));
    eventually(|| h.facts().contains(&Fact::Hold(Some(hold)))).await;
    // The older pull answers "no lock" now.
    pull.notify_one();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !h.facts().contains(&Fact::Hold(None)),
        "a pull older than the push told the core the lock was gone"
    );
    let outcome = answered(transmit_answer(&h, &request).await).await;
    assert!(
        held_back(&outcome),
        "the older pull opened the gate: {outcome:?}"
    );
    assert!(
        client.session.sent().is_empty(),
        "a frame went into the lock"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ----- receipts and typing (W5a chunk 11, decisions D10, D8, D7) -----

const RECEIPT_PEER: &str = "15550153820476";
const RECEIPT_MSG: &str = "3EB0E7D24A9C16B53057";
const TYPING_PEER: &str = "15550169407238";

/// Notes `conv` as started on the phone (D7), as stage 2 does for a peer's message.
async fn start_chat(client: &Made, conv: &kourier_model::ConvKey) {
    let conv = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &conv, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
}

/// D10: `SendReceipts` is confirmed at once (the job is durable) and the receipt goes out only once the connection is
/// up; the job is gone after the write. Mutant: the runner sends without a live connection.
#[tokio::test]
async fn a_receipt_job_is_sent_once_connected() {
    use kourier_model::{CmdId, ConvKey, MsgTarget, ReceiptKind, TargetAuthor};

    let (mut h, client) = resumed_on_the_wall_clock().await;
    let peer = UserId::from_parts("pn", RECEIPT_PEER).expect("a number");
    let conv = ConvKey::Direct(peer.clone());
    start_chat(&client, &conv).await;
    h.send(Command::SendReceipts {
        cmd: CmdId(57),
        conv,
        targets: vec![MsgTarget {
            conv: None,
            author: TargetAuthor::User(peer),
            local: RECEIPT_MSG.into(),
            alternates: Vec::new(),
        }],
        kind: ReceiptKind::Read,
    })
    .await;
    let mut envelope = h.envelope().await;
    assert!(
        matches!(
            envelope.events.as_slice(),
            [BackendEvent::CommandPersisted { cmd }] if *cmd == CmdId(57)
        ),
        "the receipt command is confirmed once its job is kept"
    );
    if let Some(ack) = envelope.ack.take() {
        ack.complete(Ok(()));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        client.session.receipts().is_empty(),
        "no receipt goes out before the connection is up"
    );
    client.tap.deliver(WaSignal::Connected);
    eventually(|| client.session.receipts().len() == 1).await;
    assert_eq!(
        client.session.receipts()[0].ids,
        vec![RECEIPT_MSG.to_owned()]
    );
    let store = client.store.clone();
    eventually_async(|| {
        let store = store.clone();
        async move {
            store
                .read(|conn| {
                    conn.query_row("SELECT count(*) FROM receipt_jobs", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map_err(crate::error::sql("test: jobs"))
                })
                .await
                .expect("jobs")
                == 0
        }
    })
    .await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D10: `SyncReadMarker` and `DeleteAttachment` are confirmed (their rows leave the core's pending list).
/// Mutant: either arm confirms nothing.
#[tokio::test]
async fn sync_read_marker_and_delete_attachment_are_confirmed() {
    use kourier_model::{CmdId, ConvKey, MsgTarget, TargetAuthor};

    let (mut h, _client) = resumed_on_the_wall_clock().await;
    let peer = UserId::from_parts("pn", RECEIPT_PEER).expect("a number");
    let conv = ConvKey::Direct(peer.clone());
    let target = MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::User(peer),
        local: RECEIPT_MSG.into(),
        alternates: Vec::new(),
    };
    h.send(Command::SyncReadMarker {
        cmd: CmdId(58),
        conv: conv.clone(),
        up_to: target.clone(),
        read: vec![target.clone()],
    })
    .await;
    h.send(Command::DeleteAttachment {
        cmd: CmdId(59),
        conv,
        target,
        index: 0,
        net_ids: Redacted::new(Vec::new()),
    })
    .await;
    let mut confirmed = Vec::new();
    while confirmed.len() < 2 {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            if let BackendEvent::CommandPersisted { cmd } = event {
                confirmed.push(cmd.0);
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert_eq!(confirmed, vec![58, 59]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A10a: the core's own durable change of a chat (the clearing of a mark when a chat is read) is confirmed and
/// dropped, so its row leaves the core's pending list, and the roaming switch is refused like every request of a
/// network that cannot write roaming settings. Mutants: the durable arm confirms nothing; the switch answers `Ok`.
#[tokio::test]
async fn a_durable_conversation_change_is_confirmed_and_the_roaming_switch_is_refused() {
    use kourier_model::{CmdId, ConvKey, ConversationChange};

    let (mut h, _client) = resumed_on_the_wall_clock().await;
    let peer = UserId::from_parts("pn", RECEIPT_PEER).expect("a number");
    let (reply, answer) = tokio::sync::oneshot::channel();
    h.send(Command::SetRoamingWrites { on: true, reply }).await;
    let refused = answer.await.expect("the switch is answered");
    assert!(
        matches!(&refused, Err(e) if e.kind == ErrorKind::Unsupported),
        "the switch was not refused as unsupported"
    );
    h.send(Command::ChangeConversationDurable {
        cmd: CmdId(63),
        conv: ConvKey::Direct(peer),
        change: ConversationChange::SetMarkedUnread {
            marked_unread: false,
        },
        asked_at: 1_700_000_000_000,
    })
    .await;
    let mut confirmed = Vec::new();
    while confirmed.is_empty() {
        let mut envelope = h.envelope().await;
        for event in &envelope.events {
            if let BackendEvent::CommandPersisted { cmd } = event {
                confirmed.push(cmd.0);
            }
        }
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
    }
    assert_eq!(confirmed, vec![63]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D8: no typing state goes out while a hold is known (the peer would see typing and then nothing for days); once
/// the hold is lifted it does. Mutant: the typing's gate removed.
#[tokio::test]
async fn no_typing_while_held() {
    use kourier_model::ConvKey;

    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let wall = u64::try_from(kourier_db::now_ms()).expect("a time");
    let mut h = H::start_with_clock(
        dir,
        StartMode::Resume,
        BackendConfig::default(),
        FakeClock::new(wall),
    );
    let until_ms = Some(wall + 3_600_000);
    let scripted = Arc::new(FakeSession::new());
    scripted.script_hold(Ok(HoldState::Active {
        until_ms,
        kind: None,
    }));
    h.view.queue(scripted);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", TYPING_PEER).expect("a number"));
    start_chat(&client, &conv).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    assert!(
        h.facts().iter().any(|f| matches!(f, Fact::Hold(Some(_)))),
        "the hold is known"
    );
    h.send(Command::SetTyping {
        conv: conv.clone(),
        on: true,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        client.session.typed().is_empty(),
        "no typing state may go out while a hold is known"
    );
    client.tap.deliver(WaSignal::Hold(HoldState::Lifted));
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    h.send(Command::SetTyping { conv, on: true }).await;
    eventually(|| client.session.typed().len() == 1).await;
    assert!(
        client.session.typed()[0].1,
        "the lifted hold lets typing go"
    );
    let _ = h.quiet_for(Duration::from_millis(1)).await;
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W6a1 (H5): a `Rebuild` of the core also forgets what it was told of the phone's settings, and the start sweep
/// states them again right behind each chat's features: the rebuilt core hears the archive of a chat the old one had.
#[tokio::test]
async fn a_rebuild_states_what_the_phone_said_of_every_chat_again() {
    use crate::store::chat_state::{self, Field, Put};
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let chat =
        kourier_model::ConvKey::Direct(UserId::from_parts("pn", "15550175930264").expect("a user"));
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-settings",
        )
        .expect("open");
        let stated = chat.clone();
        owner
            .store()
            .write("stated before", move |tx| {
                crate::send::started::mark_stated(tx, &stated, crate::features::VERSION, false)?;
                chat_state::put(
                    tx,
                    &Put {
                        chat: &stated,
                        field: Field::Archived,
                        value: 1,
                        action_ms: 10,
                        generation: 0,
                        collection: "regular_low",
                        stated: true,
                    },
                )
                .map(drop)
            })
            .await
            .expect("mark");
        owner.close();
    }
    let mut h = H::start_in(dir, StartMode::Rebuild);
    let events = events_acked(&mut h, 3).await;
    assert!(
        matches!(events.first(), Some(BackendEvent::AccountReady { .. })),
        "{events:?}"
    );
    assert!(
        matches!(events.get(1), Some(e) if crate::features::is_statement(e)),
        "{events:?}"
    );
    assert!(
        matches!(events.get(2), Some(BackendEvent::ConversationUpsert(p))
            if p.key == chat && p.archived == Some(true) && p.features.is_none()),
        "the rebuilt core is told the chat is archived: {events:?}"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// F5 (durability review DUR-2): a `Rebuild` brings the call log back. The rebuilt core has none of the call lines;
/// the history's call-log records were kept for exactly this case (taken up once), so the zone that states
/// `AccountReady` again makes them due, the first quiet step places each ONCE more (its line in the peer's chat), and
/// the call log's collection (`appstate::map::CALL_LOG`) is marked dirty so the start's resync replays what the phone's log holds.
#[tokio::test]
async fn a_rebuild_places_the_kept_call_records_again_and_marks_the_call_log_dirty() {
    use wacore::appstate::hash::HashState;
    use wacore::store::traits::AppSyncStore as _;
    use waproto::buffa::Message as _;
    use waproto::whatsapp as wa;

    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let mut record = wa::CallLogRecord::default();
    record.call_id = Some("CALLF5A1".to_owned());
    record.call_creator_jid = Some(PEER.to_owned());
    record.start_time = Some(1_799_000_000);
    record.call_result = Some(wa::call_log_record::CallResult::MISSED);
    let bytes = record.encode_to_vec();
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-f5",
        )
        .expect("open");
        let store = owner.store();
        // The call log's collection has moved (a version above 0), so it can be marked dirty; its flag is clear.
        store
            .set_version(
                crate::appstate::map::CALL_LOG,
                HashState {
                    version: 4,
                    ..HashState::default()
                },
            )
            .await
            .expect("version");
        store
            .write("test: a taken-up call record", move |tx| {
                tx.execute(
                    "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) \
                     VALUES ('n0', 0, '', ?1, ?2, 1)",
                    kourier_db::rusqlite::params![bytes, crate::history::deliver::TAKEN_UP],
                )
                .map(drop)
                .map_err(crate::error::sql("test: plant"))?;
                // The map version is the build's: the start's own "the map grew" marking must not be what sets the flag.
                crate::store::resyncs::set_map_version(tx, crate::appstate::startup::MAP_VERSION)?;
                tx.execute("DELETE FROM appstate_dirty", [])
                    .map(drop)
                    .map_err(crate::error::sql("test: plant"))
            })
            .await
            .expect("plant");
        owner.close();
    }
    let mut h = H::start_in(dir, StartMode::Rebuild);
    // The history driver starts from the stage file, which a chunk's capture makes (the earlier run had one): one
    // arrives, and the kept record is placed once the push is over.
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client
        .receive
        .capture
        .commit(LID, "n1", Some(0), chunk_of(1))
        .await
        .expect("captured");
    // AccountReady first; then the record's line, once, in the chat of the person who called.
    let mut told = Vec::new();
    let mut lines = 0;
    while lines == 0 {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        for event in &envelope.events {
            if let BackendEvent::HistoryBatch { messages, .. } = event {
                lines += messages
                    .iter()
                    .filter(|m| m.local.to_string() == "call:CALLF5A1")
                    .count();
            }
        }
        told.extend(envelope.events);
    }
    assert!(
        matches!(told.first(), Some(BackendEvent::AccountReady { .. })),
        "{told:?}"
    );
    assert_eq!(lines, 1, "the line comes back once");
    let conn = kourier_db::open(
        &h.dir.path().join("backend.db"),
        &kourier_db::DbKey::from(KEY),
    )
    .expect("open");
    let dirty: Vec<String> = conn
        .prepare("SELECT name FROM appstate_dirty ORDER BY name")
        .and_then(|mut stmt| stmt.query_map([], |r| r.get(0))?.collect())
        .expect("dirty");
    assert_eq!(
        dirty,
        [crate::appstate::map::CALL_LOG],
        "the call log's collection is read again at this start"
    );
    drop(conn);
    // Placed once: the record is marked again and nothing more is told.
    assert!(
        h.quiet_for(Duration::from_millis(500)).await,
        "the line came twice"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Critic M4 (W5a, D9): a `Rebuild` forgets every chat's stated features in the zone that states `AccountReady`
/// again, and the start sweep states them right behind it; a `Resume` of the same file states nothing.
#[tokio::test]
async fn a_rebuild_states_every_chat_again_and_a_resume_does_not() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    let chat =
        kourier_model::ConvKey::Direct(UserId::from_parts("pn", "15550175930264").expect("a user"));
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-stated",
        )
        .expect("open");
        let stated = chat.clone();
        owner
            .store()
            .write("stated before", move |tx| {
                crate::send::started::mark_stated(tx, &stated, crate::features::VERSION, false)
            })
            .await
            .expect("mark");
        owner.close();
    }
    let mut h = H::start_in(dir, StartMode::Resume);
    assert!(
        h.quiet_for(Duration::from_millis(500)).await,
        "a resume states a chat that was stated"
    );
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);
    let mut h = H::start_in(dir, StartMode::Rebuild);
    let events = events_acked(&mut h, 2).await;
    assert!(
        matches!(events.first(), Some(BackendEvent::AccountReady { .. })),
        "{events:?}"
    );
    assert!(
        matches!(events.get(1), Some(e @ BackendEvent::ConversationUpsert(p))
            if crate::features::is_statement(e) && p.key == chat
                && p.features.as_ref().is_some_and(|f| f.cannot_send == Some(kourier_model::CannotSend::NotAccepted))),
        "the rebuilt core is told the chat's features again"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ---- W6a2 (B3): the phone's blocklist, read once per start after `Online` (D22) ----

const BLOCKED_PN: &str = "15550170411";
const BLOCKED_LID: &str = "99900170433";
const KEPT_PN: &str = "15550170422";

fn listed(text: &str) -> wacore_binary::Jid {
    text.parse().expect("a jid")
}

/// The `(person, blocked)` pairs of the `ContactUpsert`s among `events`, in order.
fn blocked_flags(events: &[BackendEvent]) -> Vec<(UserId, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ContactUpsert(patch) => Some((patch.user.clone(), patch.blocked?)),
            _ => None,
        })
        .collect()
}

/// The connection ends and comes back to `Online`.
async fn reconnect(h: &H, client: &Made) {
    client.tap.deliver(WaSignal::Disconnected { clean: false });
    eventually(|| !matches!(h.conn(), ConnState::Online { .. })).await;
    client.tap.deliver(WaSignal::Connected);
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
}

/// The files of an earlier start resumed with a clock `hours` after the wall clock's now (the stamp of the blocklist is read
/// against the actor's clock; the earlier start stamped at the wall clock's time).
async fn resumed_in(dir: tempfile::TempDir, hours: u64) -> (H, Made) {
    let wall = u64::try_from(kourier_db::now_ms()).expect("a time");
    let clock = FakeClock::new(wall + hours * 3_600_000);
    let h = H::start_with_clock(dir, StartMode::Resume, BackendConfig::default(), clock);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    (h, client)
}

fn user(scheme: &str, digits: &str) -> UserId {
    UserId::from_parts(scheme, digits).expect("a user")
}

/// D22: the blocklist is asked ONCE per start, only after `Online`, and what it names is stated to the core as
/// `blocked`; a reconnect of the same start asks nothing again. Kills "asked before Online" and "asked at every Online".
#[tokio::test]
async fn the_blocklist_is_asked_once_per_start_after_online_and_stated_to_the_core() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client.session.script_blocklist(Ok(vec![
        listed(&format!("{BLOCKED_PN}@s.whatsapp.net")),
        listed(&format!("{BLOCKED_LID}@lid")),
    ]));
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        client.session.count(&Call::Blocklist),
        0,
        "not before Online"
    );

    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    let events = events_acked(&mut h, 2).await;
    assert_eq!(
        blocked_flags(&events),
        [
            (user("lid", BLOCKED_LID), true),
            (user("pn", BLOCKED_PN), true)
        ],
        "{events:?}"
    );
    assert_eq!(client.session.count(&Call::Blocklist), 1);

    reconnect(&h, &client).await;
    assert!(
        h.quiet_for(Duration::from_millis(300)).await,
        "a second connection of the start stated something"
    );
    assert_eq!(
        client.session.count(&Call::Blocklist),
        1,
        "once per start, not once per connection"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D22 + M1: a read that never reached the server is asked again at the NEXT `Online` of the start, and only there; an
/// error answer is final for the start (no retry loop, one IQ). Kills "a dropped read counts as done" and "a refusal is
/// asked again".
#[tokio::test]
async fn a_blocklist_that_did_not_come_back_is_asked_at_the_next_online_and_a_refusal_is_not_asked_again()
 {
    use crate::session::AskFailure;
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client
        .session
        .script_blocklist(Err(AskFailure::NotConnected));
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{BLOCKED_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    eventually(|| client.session.count(&Call::Blocklist) == 1).await;
    assert!(
        h.quiet_for(Duration::from_millis(300)).await,
        "a read that failed stated something"
    );
    assert_eq!(
        client.session.count(&Call::Blocklist),
        1,
        "no retry inside one connection"
    );
    reconnect(&h, &client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), true)],
        "{events:?}"
    );
    reconnect(&h, &client).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(client.session.count(&Call::Blocklist), 2);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);

    // The server answered with an error: not asked again in this start, however often the connection comes back.
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client.session.script_blocklist(Err(AskFailure::Refused {
        code: 500,
        backoff: None,
    }));
    h.go_online(&client).await;
    eventually(|| client.session.count(&Call::Blocklist) == 1).await;
    reconnect(&h, &client).await;
    reconnect(&h, &client).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(client.session.count(&Call::Blocklist), 1);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D22: a person who was blocked at one start and is gone from the list at the next is stated unblocked, once, from the
/// books the first start left.
#[tokio::test]
async fn a_person_unblocked_on_the_phone_is_stated_unblocked_at_the_next_start() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client.session.script_blocklist(Ok(vec![
        listed(&format!("{BLOCKED_PN}@s.whatsapp.net")),
        listed(&format!("{KEPT_PN}@s.whatsapp.net")),
    ]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 2).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), true), (user("pn", KEPT_PN), true)]
    );
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    // A day later: within 24 hours the list is not asked again (Q1).
    let (mut h, client) = resumed_in(dir, 25).await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{KEPT_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), false)],
        "{events:?}"
    );
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A `Rebuild` made the core again, so what the old one was told is forgotten in the zone that states `AccountReady`
/// again, and the start's blocklist is stated to the new core. Kills "the rebuild keeps `blocked_stated`".
#[tokio::test]
async fn a_rebuild_states_the_blocklist_to_the_new_core_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    pair_device(dir.path()).await;
    {
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from(KEY),
            "wa-test-blocked",
        )
        .expect("open");
        owner
            .store()
            .write("stated before", |tx| {
                // A stamp from a moment ago: the list is not due by the clock, only by the rebuild (Q1).
                crate::store::people::stamp_blocklist(tx, kourier_db::now_ms())?;
                crate::store::people::put_blocked(tx, user("pn", BLOCKED_PN).as_str()).map(drop)
            })
            .await
            .expect("mark");
        owner.close();
    }
    let clock = FakeClock::new(u64::try_from(kourier_db::now_ms()).expect("a time"));
    let mut h = H::start_with_clock(dir, StartMode::Rebuild, BackendConfig::default(), clock);
    let _ready = h.ready().await;
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{BLOCKED_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), true)],
        "the new core is told: {events:?}"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Q1 (the lead's call after part 1): the blocklist is asked at most once per 24 hours per account. A second start within the
/// day asks nothing (and states nothing, however often the connection comes back); a start a day after the stamp asks again.
/// Kills "the stamp is ignored" (the second start asks) and "the day is shorter than the stamp's age" (the third does not).
#[tokio::test]
async fn the_blocklist_is_asked_at_most_once_per_24_hours_across_starts() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{BLOCKED_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(blocked_flags(&events), [(user("pn", BLOCKED_PN), true)]);
    assert_eq!(client.session.count(&Call::Blocklist), 1);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    // An hour later: the stamp is fresh, nothing is asked, even over a reconnect.
    let (mut h, client) = resumed_in(dir, 1).await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{KEPT_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    reconnect(&h, &client).await;
    assert!(
        h.quiet_for(Duration::from_millis(400)).await,
        "a start within the day stated something"
    );
    assert_eq!(
        client.session.count(&Call::Blocklist),
        0,
        "the list is not asked twice in a day"
    );
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    // A day and an hour after the stamp: asked, and the difference stated (the same stamp: the second start asked nothing).
    let (mut h, client) = resumed_in(dir, 25).await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{KEPT_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 2).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), false), (user("pn", KEPT_PN), true)],
        "{events:?}"
    );
    assert_eq!(client.session.count(&Call::Blocklist), 1);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Q1: a read with no answer stamps nothing, so the next start (an hour later) asks again: an error from the server, and a
/// read that never reached it. Kills "the stamp is written for a read that got no answer".
#[tokio::test]
async fn a_read_that_got_no_answer_stamps_nothing_and_the_next_start_asks_again() {
    use crate::session::AskFailure;
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client.session.script_blocklist(Err(AskFailure::Refused {
        code: 500,
        backoff: None,
    }));
    h.go_online(&client).await;
    eventually(|| client.session.count(&Call::Blocklist) == 1).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    let (mut h, client) = resumed_in(dir, 1).await;
    client
        .session
        .script_blocklist(Err(AskFailure::NotConnected));
    h.go_online(&client).await;
    eventually(|| client.session.count(&Call::Blocklist) == 1).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    let (mut h, client) = resumed_in(dir, 2).await;
    client
        .session
        .script_blocklist(Ok(vec![listed(&format!("{BLOCKED_PN}@s.whatsapp.net"))]));
    h.go_online(&client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(
        blocked_flags(&events),
        [(user("pn", BLOCKED_PN), true)],
        "{events:?}"
    );
    assert_eq!(client.session.count(&Call::Blocklist), 1);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

// ---- W6a2 (B6): the group fetch policy (D9, M1): two IQs at the start after `Online`, single queries under ONE bucket ----

const GROUP_A: &str = "120363000071529611";
const GROUP_B: &str = "120363000071529622";
const GROUP_C: &str = "120363000071529633";
const GROUP_D: &str = "120363000071529644";
const ADMIN_LID: &str = "99000071555";

fn group_jid(digits: &str) -> wacore_binary::Jid {
    format!("{digits}@g.us").parse().expect("a jid")
}

/// What a group's invite link asks of the client, and its answer.
async fn invite(
    h: &H,
    action: kourier_model::GroupInviteAction,
) -> Result<Option<kourier_model::Redacted<String>>, kourier_model::BackendError> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    h.send(Command::GroupInvite {
        group: group_key(GROUP_A),
        action,
        reply,
    })
    .await;
    answer.await.expect("answered")
}

/// A group's invite link: Copy reads it, Reset asks for a new one, the link cannot be turned off
/// and an unanswered question is a failure.
#[tokio::test]
async fn a_group_invite_link_is_read_and_reset() {
    use kourier_model::GroupInviteAction as Action;
    let (h, client) = online_with_chats(&[PERSON], 0).await;
    let session = &client.session;
    session.script_invite_link(Ok("https://chat.whatsapp.com/first".to_owned()));
    session.script_invite_link(Ok("https://chat.whatsapp.com/second".to_owned()));

    let read = invite(&h, Action::Read)
        .await
        .expect("read")
        .expect("a link");
    assert_eq!(read.expose(), "https://chat.whatsapp.com/first");
    let reset = invite(&h, Action::Reset)
        .await
        .expect("reset")
        .expect("a link");
    assert_eq!(reset.expose(), "https://chat.whatsapp.com/second");
    assert!(invite(&h, Action::TurnOn).await.is_err());
    assert!(invite(&h, Action::TurnOff).await.is_err());
    assert_eq!(
        session
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::GroupInvite { reset } => Some(reset),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![false, true],
        "turning on or off asks the server nothing"
    );

    // (Nothing more is scripted: the fake says it is not connected.)
    assert!(invite(&h, Action::Read).await.is_err());
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

fn group_key(digits: &str) -> kourier_model::ConvKey {
    kourier_model::ConvKey::Group(format!("wag:{digits}").into())
}

/// A whole group of one admin, as the server answers a fetch.
fn whole_group(digits: &str) -> whatsapp_rust::GroupMetadataResult {
    let mut metadata = whatsapp_rust::GroupMetadata::new(group_jid(digits));
    metadata.subject = Some("A synthetic group".into());
    metadata.subject_time = Some(1_700_000_100);
    let mut admin =
        whatsapp_rust::GroupParticipant::new(format!("{ADMIN_LID}@lid").parse().expect("a jid"));
    admin.participant_type = whatsapp_rust::ParticipantType::Admin;
    metadata.participants = vec![admin];
    whatsapp_rust::GroupMetadataResult::Found(Box::new(metadata))
}

/// The conversations whose member lists the next envelopes state (acking each), until `n` were seen.
async fn groups_stated(h: &mut H, n: usize) -> Vec<kourier_model::ConvKey> {
    let mut seen = Vec::new();
    while seen.len() < n {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        for event in envelope.events {
            if let BackendEvent::ConversationUpsert(patch) = event
                && patch.members.is_some()
            {
                seen.push(patch.key);
            }
        }
    }
    seen
}

/// `(overviews, batches, single queries)` the session was asked for.
fn group_asks(made: &Made) -> (usize, usize, usize) {
    let calls = made.session.calls();
    (
        calls.iter().filter(|c| **c == Call::Participating).count(),
        calls
            .iter()
            .filter(|c| matches!(c, Call::FetchGroups(_)))
            .count(),
        calls.iter().filter(|c| **c == Call::FetchGroup).count(),
    )
}

/// Marks `groups` for a fetch the way a notice or a `groups_dirty` does.
async fn mark_for_fetch(made: &Made, groups: &[&str]) {
    let writer = made.receive.writer.clone();
    let groups: Vec<_> = groups.iter().map(|digits| group_key(digits)).collect();
    tokio::task::spawn_blocking(move || writer.write_groups_dirty_and_wait(groups))
        .await
        .expect("join")
        .expect("marked");
}

/// D9: nothing about a group is asked before `Online`; then ONE overview and ONE batch of the groups it lists, whose answer
/// is stated in one zone (the members with the features, M4); a second connection of the same start asks nothing.
/// Mutants: a group read before `Online`; the start's reads at every `Online`.
#[tokio::test]
async fn the_start_asks_two_iqs_after_online_and_states_the_groups_once() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client
        .session
        .script_participating(Ok(vec![group_jid(GROUP_A), group_jid(GROUP_B)]));
    client
        .session
        .script_batch(Ok(vec![whole_group(GROUP_A), whole_group(GROUP_B)]));
    mark_for_fetch(&client, &[GROUP_C]).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        group_asks(&client),
        (0, 0, 0),
        "not before Online, mark or no mark"
    );

    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    let stated = groups_stated(&mut h, 2).await;
    assert_eq!(stated, [group_key(GROUP_A), group_key(GROUP_B)]);
    assert_eq!(group_asks(&client).0, 1);
    assert_eq!(
        client.session.fetched(),
        [vec![group_jid(GROUP_A), group_jid(GROUP_B)]],
        "one batch of every group the overview listed"
    );

    reconnect(&h, &client).await;
    h.clock.advance(Duration::from_secs(120));
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(
        (group_asks(&client).0, group_asks(&client).1),
        (1, 1),
        "once per start, not once per connection, whatever the bucket would allow"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// M1: after the start's pair, the groups owed a single query (a mark, a truncated answer) get ONE each, at most one per
/// 30 s on the account's clock, marks before truncated groups; each is stated whole. The first single waits for the floor
/// after the start's own fetch.
/// Mutants: the floor 0; the truncated group is not queried.
#[tokio::test]
async fn a_marked_group_and_a_truncated_one_get_one_single_query_each_at_most_every_thirty_seconds()
{
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    client
        .session
        .script_participating(Ok(vec![group_jid(GROUP_A), group_jid(GROUP_B)]));
    client.session.script_batch(Ok(vec![
        whole_group(GROUP_A),
        whatsapp_rust::GroupMetadataResult::Truncated {
            id: group_jid(GROUP_B),
            participant_count: 1500,
        },
    ]));
    client.session.script_group(Ok(whole_group(GROUP_D)));
    client.session.script_group(Ok(whole_group(GROUP_B)));
    mark_for_fetch(&client, &[GROUP_D]).await;
    h.go_online(&client).await;
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_A)]);

    // The floor: the start took the bucket's slot at t0, the first single waits until t0 + 30 s.
    eventually(|| h.clock.deadlines().contains(&(t0 + 30_000))).await;
    assert_eq!(
        group_asks(&client).2,
        0,
        "the floor holds the first single back"
    );
    h.clock.advance(Duration::from_secs(30));
    assert_eq!(
        groups_stated(&mut h, 1).await,
        [group_key(GROUP_D)],
        "the mark first"
    );
    eventually(|| h.clock.deadlines().contains(&(t0 + 60_000))).await;
    assert_eq!(group_asks(&client).2, 1);
    h.clock.advance(Duration::from_secs(30));
    assert_eq!(
        groups_stated(&mut h, 1).await,
        [group_key(GROUP_B)],
        "then the truncated group"
    );
    assert_eq!(
        client.session.fetched_one(),
        [group_jid(GROUP_D), group_jid(GROUP_B)]
    );

    // Each once per start: another connection asks for neither.
    reconnect(&h, &client).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    h.clock.advance(Duration::from_secs(120));
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(group_asks(&client), (1, 1, 2));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F17 (DUR-3): a session of days gets a second notice for a group it fetched on its own earlier: the mark is newer than the
/// group's last single query, so the group is asked again in the same start (under the same bucket), not at the next app start.
/// A group with no newer mark is not asked a second time.
/// Mutant: a group tried once is never asked again in the start (the old `BTreeSet`).
#[tokio::test]
async fn a_mark_made_after_a_groups_single_query_is_served_in_the_same_start() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    client
        .session
        .script_participating(Ok(vec![group_jid(GROUP_A)]));
    client.session.script_batch(Ok(vec![whole_group(GROUP_A)]));
    client.session.script_group(Ok(whole_group(GROUP_D)));
    client.session.script_group(Ok(whole_group(GROUP_D)));
    mark_for_fetch(&client, &[GROUP_D]).await;
    h.go_online(&client).await;
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_A)]);
    eventually(|| h.clock.deadlines().contains(&(t0 + 30_000))).await;
    h.clock.advance(Duration::from_secs(30));
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_D)]);
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(group_asks(&client).2, 1, "no newer mark: not asked again");

    // Later in the same session another notice marks the group.
    h.clock.advance(Duration::from_secs(120));
    mark_for_fetch(&client, &[GROUP_D]).await;
    assert_eq!(
        groups_stated(&mut h, 1).await,
        [group_key(GROUP_D)],
        "the newer mark is served in this start"
    );
    assert_eq!(
        client.session.fetched_one(),
        [group_jid(GROUP_D), group_jid(GROUP_D)]
    );
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(group_asks(&client), (1, 1, 2));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// M1: thirty groups marked at once (a burst of notices, a `groups_dirty`) are fetched one per 30 s, and no more than ten
/// fetches (the start's included) in ten minutes, however many are owed: the ninth single is the last until the window frees
/// its oldest slot, and the marks that were not fetched stay owed.
/// Mutants: no cap per window; the bucket per group instead of global.
#[tokio::test]
async fn thirty_marked_groups_are_fetched_within_the_buckets_numbers() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    let groups: Vec<String> = (0..30).map(|n| format!("1203630000715300{n:02}")).collect();
    let digits: Vec<&str> = groups.iter().map(String::as_str).collect();
    mark_for_fetch(&client, &digits).await;
    h.go_online(&client).await;
    for single in 1..=9_u64 {
        eventually(|| h.clock.deadlines().contains(&(t0 + 30_000 * single))).await;
        assert_eq!(group_asks(&client).2 as u64, single - 1);
        h.clock.advance(Duration::from_secs(30));
        eventually(|| group_asks(&client).2 as u64 == single).await;
    }
    // The window is full (the start's fetch and nine singles): the next waits for the oldest to leave it, 600 s after t0.
    eventually(|| h.clock.deadlines().contains(&(t0 + 600_000))).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(group_asks(&client).2, 9);
    let owed = client
        .store
        .read(crate::store::people::dirty)
        .await
        .expect("marks")
        .len();
    assert_eq!(
        owed, 21,
        "the marks that were not fetched stay owed (an answer clears its own)"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// T4 + D9: a batch the server refused is final for the start: no single query follows it (no fallback to one IQ per
/// group), nothing is asked again at the next connection; an overview that never reached the server is asked again at the
/// next `Online`, but not before the bucket's floor.
/// Mutants: single queries after a refused batch; a read that never arrived counts as done.
#[tokio::test]
async fn a_refused_batch_is_final_for_the_start_and_one_that_never_arrived_is_asked_at_the_next_online()
 {
    use crate::session::AskFailure;
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    mark_for_fetch(&client, &[GROUP_C]).await;
    client
        .session
        .script_participating(Err(AskFailure::NotConnected));
    client
        .session
        .script_participating(Ok(vec![group_jid(GROUP_A)]));
    client.session.script_batch(Err(AskFailure::Refused {
        code: 500,
        backoff: None,
    }));
    h.go_online(&client).await;
    eventually(|| group_asks(&client).0 == 1).await;
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(
        group_asks(&client),
        (1, 0, 0),
        "a failed overview is not retried inside one connection"
    );

    // The next Online asks again, but the floor holds it back until t0 + 30 s.
    reconnect(&h, &client).await;
    eventually(|| h.clock.deadlines().contains(&(t0 + 30_000))).await;
    assert_eq!(group_asks(&client).0, 1);
    h.clock.advance(Duration::from_secs(30));
    eventually(|| group_asks(&client).1 == 1).await;

    // The batch was refused: no single query for the mark, not now, not at a later connection.
    h.clock.advance(Duration::from_secs(90));
    reconnect(&h, &client).await;
    h.clock.advance(Duration::from_secs(90));
    assert!(h.quiet_for(Duration::from_millis(400)).await);
    assert_eq!(group_asks(&client), (2, 1, 0));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// Fix F7b (PROTOCOL-5): the account's clock is set back three hours after the start's fetch; the next mark is fetched after
/// the 30 seconds of the floor, not after the whole skew (the policy sleeps, asks the bucket again and sleeps again). The
/// sleep it makes is the floor (the fake clock's deadline is the stepped-back time plus 30 s), and one advance of 30 s is
/// enough.
/// Mutants: the bucket reads the wall clock as it is.
#[tokio::test]
async fn a_clock_set_back_does_not_hold_the_next_fetch_for_the_skew() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    client
        .session
        .script_participating(Ok(vec![group_jid(GROUP_A)]));
    client.session.script_batch(Ok(vec![whole_group(GROUP_A)]));
    client.session.script_group(Ok(whole_group(GROUP_D)));
    h.go_online(&client).await;
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_A)]);
    // The start's fetch is behind us and nothing is owed: the policy idles. The clock is stepped back three hours.
    eventually(|| group_asks(&client) == (1, 1, 0)).await;
    let back = t0 - 3 * 3600 * 1000;
    h.clock.set(back);
    mark_for_fetch(&client, &[GROUP_D]).await;
    eventually(|| h.clock.deadlines().contains(&(back + 30_000))).await;
    assert_eq!(group_asks(&client).2, 0, "the floor holds it");
    h.clock.advance(Duration::from_secs(30));
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_D)]);
    assert_eq!(group_asks(&client), (1, 1, 1));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// M1 + T5: a 429 closes the bucket for ten minutes (a delay the server names closes it for that delay); the group it
/// refused is not asked again in this start; the next group waits for the closed bucket, not for the floor.
/// Mutant: the refusal's delay is ignored.
#[tokio::test]
async fn a_429_closes_the_bucket_for_ten_minutes_and_the_refused_group_is_not_asked_again() {
    use crate::session::AskFailure;
    let (mut h, client) = resumed_on_the_wall_clock().await;
    let t0 = h.clock.now_ms();
    mark_for_fetch(&client, &[GROUP_A, GROUP_B]).await;
    client.session.script_group(Err(AskFailure::Refused {
        code: 429,
        backoff: None,
    }));
    h.go_online(&client).await;
    eventually(|| h.clock.deadlines().contains(&(t0 + 30_000))).await;
    h.clock.advance(Duration::from_secs(30));
    eventually(|| group_asks(&client).2 == 1).await;
    // The floor would allow the next at t0 + 60 s; the server's answer closed the bucket until t0 + 30 s + 600 s.
    eventually(|| h.clock.deadlines().contains(&(t0 + 630_000))).await;
    h.clock.advance(Duration::from_secs(300));
    assert!(h.quiet_for(Duration::from_millis(300)).await);
    assert_eq!(group_asks(&client).2, 1);
    h.clock.advance(Duration::from_secs(300));
    eventually(|| group_asks(&client).2 == 2).await;
    assert_eq!(
        client.session.fetched_one(),
        [group_jid(GROUP_A), group_jid(GROUP_B)],
        "the refused group is not asked again"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// D9: a mark is durable: a start that was killed (here: quit) between the mark and the fetch asks for the group at its next
/// start, after the start's own pair and the floor.
/// Mutant: the marks are forgotten at the start.
#[tokio::test]
async fn a_mark_left_by_the_last_start_is_fetched_at_the_next() {
    let (h, client) = resumed_on_the_wall_clock().await;
    mark_for_fetch(&client, &[GROUP_C]).await;
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    let clock = FakeClock::new(u64::try_from(kourier_db::now_ms()).expect("a time"));
    let t0 = clock.now_ms();
    let mut h = H::start_with_clock(dir, StartMode::Resume, BackendConfig::default(), clock);
    h.send(Command::Connect).await;
    let client = h.client(0).await;
    client.session.script_group(Ok(whole_group(GROUP_C)));
    h.go_online(&client).await;
    eventually(|| h.clock.deadlines().contains(&(t0 + 30_000))).await;
    h.clock.advance(Duration::from_secs(30));
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_C)]);
    assert_eq!(client.session.fetched_one(), [group_jid(GROUP_C)]);
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// M1: the notices of a reconnect's offline catch-up mark their groups, and nothing is asked for them before the
/// connection is `Online` again, whatever the bucket would allow; then the mark is fetched. (A task that was waiting for a mark
/// from the connection before must not wake into the catch-up.)
/// Mutants: the policy is not told the connection left `Online`; the task does not look before it asks.
#[tokio::test]
async fn a_mark_made_in_a_reconnects_catch_up_waits_for_online() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    h.go_online(&client).await;
    eventually(|| group_asks(&client).0 == 1).await;

    client.tap.deliver(WaSignal::Disconnected { clean: false });
    eventually(|| !matches!(h.conn(), ConnState::Online { .. })).await;
    client.tap.deliver(WaSignal::Connected);
    h.conn_is(ConnState::Syncing { fraction: None }).await;
    mark_for_fetch(&client, &[GROUP_C]).await;
    // Past the bucket's floor (the start's fetch was at t0) and short of the catch-up's own 90 s wait.
    h.clock.advance(Duration::from_secs(31));
    assert!(h.quiet_for(Duration::from_millis(400)).await);
    assert_eq!(
        group_asks(&client),
        (1, 0, 0),
        "nothing is asked in the catch-up"
    );

    client.session.script_group(Ok(whole_group(GROUP_C)));
    client.tap.deliver(WaSignal::OfflineDone { count: 0 });
    h.conn_is(ConnState::Online { detail: None }).await;
    assert_eq!(groups_stated(&mut h, 1).await, [group_key(GROUP_C)]);
    assert_eq!(group_asks(&client), (1, 0, 1));
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// A `ChangeConversation` of `change` in `conv`, with the id the core would allocate for a timer.
fn conversation_change(
    conv: kourier_model::ConvKey,
    change: kourier_model::ConversationChange,
    id: Option<&str>,
) -> (
    Command,
    tokio::sync::oneshot::Receiver<Result<(), BackendError>>,
) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let command = Command::ChangeConversation {
        conv,
        change,
        id: id.map(|local| kourier_backend_api::AllocatedId {
            local: local.into(),
            sent_at: i64::try_from(NOW).expect("time") - 500,
        }),
        reply,
    };
    (command, answer)
}

async fn change_answered(
    answer: tokio::sync::oneshot::Receiver<Result<(), BackendError>>,
) -> Result<(), BackendError> {
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("the actor answers")
        .expect("an answer")
}

/// W6a2 (B7b, D22): the 1:1 timer set runs as an attempt beside the sends: ONE frame, the setting under the id the core
/// allocated, written only because the user asked; the command is answered after the ack, and the core hears the chat's
/// timer and the notice (ours) in the writer's zone. Mutants: the arm answers `NOT_DONE` again; the frame without the
/// setting; the confirmation before the ack.
#[tokio::test]
async fn a_timer_set_in_a_direct_chat_sends_one_frame_and_is_confirmed_after_its_ack() {
    use kourier_model::{ConvKey, ConversationChange, Patch, SystemEvent};

    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let conv = ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap());
    let started = conv.clone();
    client
        .store
        .write("test: start", move |tx| {
            crate::send::started::note(tx, &started, crate::send::started::How::Peer)
        })
        .await
        .expect("note");
    let (command, answer) = conversation_change(
        conv.clone(),
        ConversationChange::SetTimer {
            timer: Some(Duration::from_secs(604_800)),
        },
        Some(SEND_LOCAL),
    );
    h.send(command).await;
    eventually(|| client.session.sent().len() == 1).await;
    let frame = client.session.sent().remove(0);
    assert_eq!(frame.id, SEND_LOCAL);
    let at_s = i64::try_from(NOW / 1000).expect("time");
    let setting = frame
        .message
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(setting.ephemeral_expiration, Some(604_800));
    assert_eq!(
        setting.ephemeral_setting_timestamp,
        Some(at_s),
        "the actor's clock is the setting's version"
    );
    client.receive.acks.answer(crate::send::acks::Ack {
        id: SEND_LOCAL.to_owned(),
        answer: crate::send::acks::Answer::Acked {
            server_t: Some(at_s + 1),
        },
    });
    change_answered(answer)
        .await
        .expect("accepted after the ack");
    let mut told = Vec::new();
    while !told
        .iter()
        .any(|event| matches!(event, BackendEvent::Message(_)))
    {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        told.extend(
            envelope
                .events
                .into_iter()
                .filter(|event| !crate::features::is_statement(event)),
        );
    }
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = told.as_slice()
    else {
        panic!("the confirmation: {told:?}");
    };
    assert_eq!(patch.timer, Patch::Set(Duration::from_secs(604_800)));
    assert_eq!(&*notice.local, SEND_LOCAL);
    assert_eq!(
        notice.body,
        kourier_model::Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: Some(Duration::from_secs(604_800)),
        })
    );
    assert_eq!(client.session.sent().len(), 1, "one frame, no second");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W6a2 (B7b): what the backend does NOT write stays refused with no frame: a group's timer (K8c), a timer that WhatsApp's
/// apps do not offer, a timer with no id from the core, and every other conversation change (W6b). Mutants: a group
/// accepted; every value accepted; `SetArchived` accepted.
#[tokio::test]
async fn a_group_timer_an_unoffered_value_and_the_other_changes_are_refused_with_no_frame() {
    use kourier_model::{ConvKey, ConversationChange};

    let (h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let direct = ConvKey::Direct(UserId::from_parts("pn", SEND_PEER).unwrap());
    let group = ConvKey::Group("wag:120363000083920471".into());
    let week = Some(Duration::from_secs(604_800));
    for (conv, change, id, wanted) in [
        (
            group,
            ConversationChange::SetTimer { timer: week },
            Some(SEND_LOCAL),
            ErrorKind::Unsupported,
        ),
        (
            direct.clone(),
            ConversationChange::SetTimer {
                timer: Some(Duration::from_secs(3_600)),
            },
            Some(SEND_LOCAL),
            ErrorKind::InvalidInput,
        ),
        (
            direct.clone(),
            ConversationChange::SetTimer { timer: week },
            None,
            ErrorKind::InvalidInput,
        ),
        (
            direct,
            ConversationChange::SetArchived { archived: true },
            None,
            ErrorKind::Unsupported,
        ),
    ] {
        let (command, answer) = conversation_change(conv, change.clone(), id);
        h.send(command).await;
        let refused = change_answered(answer).await.expect_err("refused");
        assert_eq!(refused.kind, wanted, "{change:?}");
    }
    assert!(client.session.sent().is_empty(), "no frame for any of them");
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W6a2 (B8b): the core's `RefreshContact` asks nothing of the network: it states again what the backend holds of the person,
/// the self-chosen name as the core was told it and, for a person the core knows, the address-book name and number from the
/// mirror (statements taken back first, so the core hears them again). Mutants: the arm is a no-op again; the address-book
/// statements are not taken back; a person the core does not know is named.
#[tokio::test]
async fn a_refresh_of_a_contact_states_again_what_the_books_hold_and_sends_nothing() {
    use kourier_model::Patch;

    let (mut h, client) = resumed_with_a_client().await;
    client.tap.deliver(WaSignal::Connected);
    eventually(|| h.facts().contains(&Fact::Hold(None))).await;
    let known = UserId::from_parts("lid", "99900481501").unwrap();
    let stranger = UserId::from_parts("lid", "99900481502").unwrap();
    let (known_id, stranger_id) = (known.clone(), stranger.clone());
    client
        .store
        .write("test: the books", move |tx| {
            use crate::store::{book, people};
            for person in [&known_id, &stranger_id] {
                people::put_name(
                    tx,
                    person.as_str(),
                    people::NameKind::Profile,
                    "Self Chosen",
                )?;
                // No number in the row: the name statement is the only thing a refresh can say again.
                book::put(
                    tx,
                    &book::Row {
                        contact: person.as_str().to_owned(),
                        name: "Saved As".to_owned(),
                        pn: None,
                        lid: Some(person.as_str().to_owned()),
                    },
                )?;
            }
            book::know(tx, known_id.as_str())?;
            // The core was told Ann's saved name before: the refresh says it again.
            people::put_name(tx, known_id.as_str(), people::NameKind::System, "Saved As")
                .map(|_| ())
        })
        .await
        .expect("books");

    for user in [known.clone(), stranger.clone()] {
        h.send(Command::RefreshContact { user }).await;
    }
    let mut told = Vec::new();
    while told.len() < 3 {
        let mut envelope = h.envelope().await;
        if let Some(ack) = envelope.ack.take() {
            ack.complete(Ok(()));
        }
        told.extend(envelope.events.into_iter().filter_map(|event| match event {
            BackendEvent::ContactUpsert(patch) => Some(patch),
            _ => None,
        }));
    }
    let saved = |user: &UserId| {
        told.iter().find(|patch| {
            &patch.user == user && patch.system_name == Patch::Set("Saved As".to_owned())
        })
    };
    let chosen = |user: &UserId| {
        told.iter().find(|patch| {
            &patch.user == user && patch.profile_name == Patch::Set("Self Chosen".to_owned())
        })
    };
    assert!(
        chosen(&known).is_some() && saved(&known).is_some(),
        "everything for the known person"
    );
    assert!(
        chosen(&stranger).is_some(),
        "his own name, as the core was told it"
    );
    assert!(
        saved(&stranger).is_none(),
        "the address book names only people the core knows"
    );
    assert_eq!(told.len(), 3, "nothing else");
    assert!(
        client.session.sent().is_empty(),
        "no frame: nothing is asked of the network"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

/// W6a2, B9 (D7, H4): when the connection comes `Online` the store's vouched pairs of people the core knows are told to
/// the core as ONE `IdentityMerged` each, in the order number, LID; a pair of a stranger and one whose only source does not
/// vouch are not; a later start (and a reconnect) tells nothing again, because the pair is recorded in the zone that stated
/// it. Kills "a pair stated twice", "the scope ignored" and "any source".
#[tokio::test]
async fn a_pair_the_store_vouches_for_is_stated_once_at_online_and_not_again_at_a_later_start() {
    let (mut h, client) = resumed_on_the_wall_clock().await;
    client
        .store
        .write("test: the store's pairs", |tx| {
            use crate::store::{book, exec};
            for (lid, phone, source) in [
                ("99900485001", "15550485001", "usync"),
                ("99900485002", "15550485002", "usync"),
                ("99900485003", "15550485003", "other"),
            ] {
                exec(
                    tx,
                    "test",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source)
                     VALUES (?1, ?2, 1, 2, ?3)",
                    kourier_db::rusqlite::params![lid, phone, source],
                )?;
            }
            // The core knows the first and the third; nobody told it of the second.
            book::know(tx, "pn:15550485001")?;
            book::know(tx, "pn:15550485003").map(|_| ())
        })
        .await
        .expect("the store");
    h.go_online(&client).await;
    let events = events_acked(&mut h, 1).await;
    assert_eq!(
        merged_pairs(&events),
        [(user("pn", "15550485001"), user("lid", "99900485001"))],
        "{events:?}"
    );
    reconnect(&h, &client).await;
    assert!(
        h.quiet_for(Duration::from_millis(400)).await,
        "a reconnect of the same start stated a pair again"
    );
    h.send(Command::Shutdown).await;
    let (exit, dir) = h.exit_keeping_the_files().await;
    assert_eq!(exit, Exit::Shutdown);

    let (mut h, client) = resumed_in(dir, 1).await;
    h.go_online(&client).await;
    assert!(
        h.quiet_for(Duration::from_millis(400)).await,
        "a later start stated a pair again"
    );
    h.send(Command::Shutdown).await;
    assert_eq!(h.exit().await, Exit::Shutdown);
}

fn merged_pairs(events: &[BackendEvent]) -> Vec<(UserId, UserId)> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::IdentityMerged { from, into } => Some((from.clone(), into.clone())),
            _ => None,
        })
        .collect()
}
