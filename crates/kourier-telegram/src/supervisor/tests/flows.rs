//! TG-2 C9: the actor's login and connection flows over the scripted TDLib, in process (the brief
//! §6 C9 and §7 layer (a'); the critic's B1, H1, H2, H4, M4, M6, M7, M8 cases).
//!
//! Each test runs the REAL account actor (`actor::run_with`) on its own thread over the serve loop
//! of a `ScriptedTd` (the parent module's `in_process`), driven through the core's ends like the
//! core drives it: commands in, envelopes out, durable envelopes acked by the test (the core's
//! commit). The rules come from `testdata/login-rules.jsonl` (the scrubbed fixture's login, C0)
//! plus each test's own rules BEFORE them (the first matching rule answers). Only synthetic values:
//! the fixture's number `+999068204719`, the code `12345`, `synthetic hint`, `ada@example.invalid`,
//! the API pair `ApiCredentials::DUMMY`; nothing dials (the parts' dialer refuses), no real TDLib.
//! Nothing a test asserts on is printed: a failing assertion names counts and step ids only.

use std::sync::atomic::AtomicUsize;

use kourier_backend_api::{
    AccountContext, Ack, BackendConfig, BackendEvent, Command, ContextParams, CoreEnds, Exit,
    FakeClock, StartMode, TakenIdentity,
};
use kourier_model::{
    AccountId, ConnState, ErrorKind, InputKind, LoginStep, LoginStepKind, NetworkNote, Redacted,
    UserId,
};
use serde_json::json;

use super::*;
use crate::db;
use crate::login::{QR_BOUND_MS, QR_MAX_LINKS, field, step};

/// The fixture's synthetic number, as typed and as digits.
const NUMBER: &str = "+999068204719";
const DIGITS: &str = "999068204719";
/// The fixture's code, and a wrong one.
const CODE: &str = "12345";
const WRONG_CODE: &str = "54321";
/// A synthetic 2FA password holding `"`, `\`, a newline and non-ASCII (critic M10: the request
/// is JSON-escaped; the scripted rule matches only the exact decoded value).
const PASSWORD: &str = "synthetic \"pass\" \\ line\nzwei \u{e9}";
const HINT: &str = "synthetic hint";
const EMAIL: &str = "ada@example.invalid";
const EMAIL_CODE: &str = "67890";
/// Synthetic QR links (the token is base64 of a word).
const QR_LINK: &str = "tg://login?token=c3ludGhldGlj";
/// The own user of the fixture.
const OWN_ID: i64 = 100;
/// The account's database key in these tests.
const KEY: [u8; 32] = [7; 32];

/// One actor over the scripted TDLib and what the test looks at.
struct Run {
    rt: tokio::runtime::Runtime,
    dir: tempfile::TempDir,
    uuid: AccountUuid,
    backend_db: PathBuf,
    clock: Arc<FakeClock>,
    td: ScriptedTd,
    spawns: Arc<AtomicUsize>,
    ends: CoreEnds,
    actor: Option<std::thread::JoinHandle<Exit>>,
}

/// How a [`Run`] starts.
struct Setup {
    script: Script,
    start: StartMode,
    row: Option<db::AccountRow>,
    env: TgEnv,
    timings: Timings,
    suffix: Option<&'static str>,
    lines: Option<Lines>,
    /// An `AccountReady` a crash left in `core_outbox` (written with the row, before the start).
    ready_left: bool,
    /// Layer (b), TG-2 C10: the helpers are the fake helper PROCESS of these scenarios
    /// (`child_parts_logged`, the n-th helper the n-th scenario), each one's process id recorded;
    /// `script` (and `Run::td`) is then not used by any helper.
    child: Option<(&'static [&'static str], Pids)>,
    /// TG-3 C10a: the account directory to use (its `backend.db` outlives the run: the kill
    /// harness's children share it); `None` = a fresh one in the run's own temporary directory.
    at: Option<PathBuf>,
    /// Fix round TF5: fills the backend.db (after the account row) before the actor starts.
    prepare: Option<fn(&kourier_db::rusqlite::Connection)>,
}

/// The process ids of the fake helpers a scenario started, in order.
type Pids = Arc<Mutex<Vec<u32>>>;

impl Setup {
    /// A `Fresh` actor (a login) over `script`.
    fn login(script: Script) -> Self {
        Self {
            script,
            start: StartMode::Fresh,
            row: None,
            env: TgEnv::Production,
            timings: short(),
            suffix: None,
            lines: None,
            ready_left: false,
            child: None,
            at: None,
            prepare: None,
        }
    }

    /// A `Resume` of a linked production account of `user_id` over `script`.
    fn resume(script: Script, user_id: i64) -> Self {
        Self {
            start: StartMode::Resume,
            row: Some(db::AccountRow {
                env: TgEnv::Production,
                user_id: Some(user_id),
                pin: PIN.commit.to_owned(),
                created_ms: 1,
            }),
            ..Self::login(script)
        }
    }
}

/// The rules file, with `extra` rules BEFORE its own (the first matching rule answers).
fn login_script(extra: Vec<Rule>) -> Script {
    let mut script = Script::from_fixture(include_str!("../../../testdata/login-rules.jsonl"));
    let mut rules = extra;
    rules.append(&mut script.rules);
    script.rules = rules;
    script
}

/// A script whose start ends in `authorizationStateReady` (a resumed, linked TDLib) with `rules`.
fn ready_script(rules: Vec<Rule>) -> Script {
    Script {
        rules,
        after_parameters: "authorizationStateReady".to_owned(),
        ..Script::default()
    }
}

fn auth_update(state: serde_json::Value) -> serde_json::Value {
    json!({"@type": "updateAuthorizationState", "authorization_state": state})
}

fn auth_state(ty: &str) -> serde_json::Value {
    auth_update(json!({"@type": ty}))
}

fn conn_update(ty: &str) -> serde_json::Value {
    json!({"@type": "updateConnectionState", "state": {"@type": ty}})
}

fn qr_update(link: &str) -> serde_json::Value {
    auth_update(json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": link}))
}

/// `logOut` as TDLib answers it: `ok`, then LoggingOut, Closing, Closed.
fn closing_logout() -> Rule {
    Rule::new(
        "logOut",
        vec![
            ok(),
            auth_state("authorizationStateLoggingOut"),
            auth_state("authorizationStateClosing"),
            auth_state("authorizationStateClosed"),
        ],
    )
}

fn ok() -> serde_json::Value {
    json!({"@type": "ok"})
}

fn td_error(code: i64, message: &str) -> serde_json::Value {
    json!({"@type": "error", "code": code, "message": message})
}

/// Starts the actor of `setup` on its own thread (with `setup.lines` as its log subscriber).
fn open(setup: Setup) -> Run {
    let p = in_process(setup.script);
    let dir = tempfile::tempdir().expect("tempdir");
    let backend_db = match &setup.at {
        Some(at) => at.join("backend.db"),
        None => dir.path().join("account").join("backend.db"),
    };
    std::fs::create_dir_all(backend_db.parent().expect("parent")).expect("account dir");
    if let Some(row) = &setup.row {
        let conn = db::open(&backend_db, &DbKey::from_bytes(KEY)).expect("open");
        db::create_account(&conn, row).expect("row");
        if setup.ready_left {
            crate::outbox::append_ready(&conn, OWN_ID, &own_ready_event(), 1).expect("append");
        }
        if let Some(prepare) = setup.prepare {
            prepare(&conn);
        }
    }
    let rt = rt();
    let clock = Arc::new(FakeClock::new(T0));
    let uuid = AccountUuid::new_random();
    let config = BackendConfig {
        expect_number_suffix: setup.suffix.map(|s| Redacted::new(s.to_owned())),
        ..BackendConfig::default()
    };
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start: setup.start,
        backend_db: backend_db.clone(),
        db_key: kourier_backend_api::DbKey::new(KEY),
        blob_dir: dir.path().join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: clock.clone(),
        config,
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let (helper_parts, spawns) = match setup.child {
        Some((scenarios, pids)) => {
            let spawns = Arc::new(AtomicUsize::new(0));
            (child_parts_logged(scenarios, spawns.clone(), pids), spawns)
        }
        None => (p.parts.clone(), p.spawns.clone()),
    };
    let parts = helper_parts
        .with_api(ApiCredentials::DUMMY)
        .with_env(setup.env)
        .with_files_root(dir.path().join("media"));
    let timings = setup.timings;
    let lines = setup.lines;
    let actor = std::thread::spawn(move || match lines {
        Some(lines) => {
            let _guard = tracing::subscriber::set_default(lines);
            crate::actor::run_with(ctx, parts, timings)
        }
        None => crate::actor::run_with(ctx, parts, timings),
    });
    Run {
        rt,
        dir,
        uuid,
        backend_db,
        clock,
        td: p.td,
        spawns,
        ends,
        actor: Some(actor),
    }
}

impl Run {
    /// Queues `command` (10 s at most).
    fn send(&self, command: Command) {
        self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), self.ends.commands.send(command))
                .await
                .expect("the command queue stayed full")
                .expect("the actor is gone");
        });
    }

    /// Events until one matches `stop` (10 s at most), all of them pushed to `seen`; every durable
    /// envelope is acked `Ok` (the core's commit) except the one holding the match, whose ack is
    /// returned unanswered.
    fn until(
        &mut self,
        seen: &mut Vec<BackendEvent>,
        stop: impl Fn(&BackendEvent) -> bool,
    ) -> Option<Ack> {
        let Self { rt, ends, .. } = self;
        rt.block_on(async {
            let by = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let envelope = tokio::time::timeout_at(by, ends.events.recv())
                    .await
                    .expect("the awaited event did not come in time")
                    .expect("events ended");
                let hit = envelope.events.iter().any(&stop);
                seen.extend(envelope.events);
                match (hit, envelope.ack) {
                    (true, ack) => return ack,
                    (false, Some(ack)) => ack.complete(Ok(())),
                    (false, None) => {}
                }
            }
        })
    }

    /// The next step with id `id` (other events go to `seen`; durable envelopes acked).
    fn step(&mut self, seen: &mut Vec<BackendEvent>, id: &str) -> LoginStep {
        if let Some(ack) = self.until(seen, |e| is_step(e, id)) {
            ack.complete(Ok(()));
        }
        seen.iter()
            .rev()
            .find_map(|e| match e {
                BackendEvent::Login(s) if s.step_id == id => Some(s.clone()),
                _ => None,
            })
            .expect("the step was seen")
    }

    /// Events for `ms` milliseconds (real time), acking durable envelopes.
    fn quiet_for(&mut self, ms: u64, seen: &mut Vec<BackendEvent>) {
        let Self { rt, ends, .. } = self;
        rt.block_on(async {
            let by = tokio::time::Instant::now() + Duration::from_millis(ms);
            while let Ok(Some(envelope)) = tokio::time::timeout_at(by, ends.events.recv()).await {
                seen.extend(envelope.events);
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
        });
    }

    /// Waits (10 s at most, real time) until `f` holds.
    fn wait(&self, what: &str, f: impl FnMut() -> bool) {
        self.rt.block_on(until(what, f));
    }

    /// How many requests of type `ty` the scripted TDLib got.
    fn sent(&self, ty: &str) -> usize {
        self.td.sent_types().iter().filter(|t| *t == ty).count()
    }

    /// The requests of type `ty` as received.
    fn requests_of(&self, ty: &str) -> Vec<serde_json::Value> {
        self.td
            .requests()
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| r["@type"] == ty)
            .collect()
    }

    /// No request Kourier never makes reached TDLib (the helper refuses them; C3).
    fn assert_nothing_forbidden(&self) {
        let forbidden = self
            .td
            .sent_types()
            .iter()
            .filter(|t| kourier_tdlib::serve::is_forbidden_request(t))
            .count();
        assert_eq!(forbidden, 0, "a forbidden request reached TDLib");
    }

    /// Ends the actor with `command` (Shutdown, Logout) and returns its exit (30 s at most).
    fn end_with(&mut self, command: Command) -> Exit {
        self.send(command);
        self.join()
    }

    fn join(&mut self) -> Exit {
        let actor = self.actor.take().expect("the actor runs");
        let by = StdInstant::now() + Duration::from_secs(30);
        while !actor.is_finished() {
            assert!(StdInstant::now() < by, "the actor did not end in time");
            std::thread::sleep(Duration::from_millis(20));
        }
        actor.join().expect("actor")
    }

    /// The account row (read after the actor ended).
    fn row(&self) -> Option<db::AccountRow> {
        let conn = db::open(&self.backend_db, &DbKey::from_bytes(KEY)).expect("open");
        db::account(&conn).expect("read")
    }

    fn tdlib_dir(&self) -> PathBuf {
        self.backend_db.parent().expect("parent").join("tdlib")
    }

    fn files_dir(&self) -> PathBuf {
        self.dir
            .path()
            .join("media")
            .join(files_dir_name(self.uuid))
    }
}

fn is_step(e: &BackendEvent, id: &str) -> bool {
    matches!(e, BackendEvent::Login(s) if s.step_id == id)
}

/// The step ids of every login step in `seen`, in order.
fn step_ids(seen: &[BackendEvent]) -> Vec<String> {
    seen.iter()
        .filter_map(|e| match e {
            BackendEvent::Login(s) => Some(s.step_id.clone()),
            _ => None,
        })
        .collect()
}

/// Whether any login step in `seen` is `Failed`.
fn any_failed(seen: &[BackendEvent]) -> bool {
    seen.iter().any(|e| {
        matches!(
            e,
            BackendEvent::Login(LoginStep {
                kind: LoginStepKind::Failed { .. },
                ..
            })
        )
    })
}

/// The error of a `Failed` step (panics on another kind).
fn failure(step: &LoginStep) -> &kourier_model::BackendError {
    match &step.kind {
        LoginStepKind::Failed { error } => error,
        _ => panic!("the step {} is not a failure", step.step_id),
    }
}

/// The ids of the input fields of an `Input` step (panics on another kind).
fn field_ids(step: &LoginStep) -> Vec<String> {
    match &step.kind {
        LoginStepKind::Input { fields } => fields.iter().map(|f| f.id.clone()).collect(),
        _ => panic!("the step {} is not an input step", step.step_id),
    }
}

/// `SubmitLoginInput` for the step `txn`.
fn input(txn: u64, pairs: &[(&str, &str)]) -> Command {
    Command::SubmitLoginInput {
        txn,
        values: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Redacted::new((*v).to_owned())))
            .collect(),
    }
}

fn start_flow(flow: &str, taken: Vec<TakenIdentity>) -> Command {
    Command::StartLogin {
        flow: flow.to_owned(),
        relink: None,
        taken,
    }
}

fn taken(scheme: &str, value: &str) -> TakenIdentity {
    TakenIdentity {
        user: UserId::from_parts(scheme, value).expect("id"),
        logged_out: false,
    }
}

/// The fixture's phone login up to the `code` step: the `phone` step answered with the number.
/// Returns the `code` step.
fn to_the_code_step(run: &mut Run, seen: &mut Vec<BackendEvent>) -> LoginStep {
    run.send(start_flow("phone", Vec::new()));
    let phone = run.step(seen, step::PHONE);
    assert_eq!(field_ids(&phone), [field::PHONE]);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    run.step(seen, step::CODE)
}

/// From `linking` on: the `AccountReady` (checked, then acked), then `done`.
fn ready_then_done(run: &mut Run, seen: &mut Vec<BackendEvent>) {
    let ack = run
        .until(seen, |e| matches!(e, BackendEvent::AccountReady { .. }))
        .expect("the AccountReady is not durable");
    match seen.last() {
        Some(BackendEvent::AccountReady {
            own_ids,
            number,
            display_name,
            ..
        }) => {
            assert_eq!(own_ids.len(), 2, "not the two own ids");
            assert_eq!(
                own_ids[0],
                UserId::from_parts("tg", &OWN_ID.to_string()).expect("id")
            );
            assert_eq!(own_ids[1], UserId::from_parts("pn", DIGITS).expect("id"));
            assert!(
                number.as_ref().is_some_and(|n| n.expose() == NUMBER),
                "the number is not the fixture's"
            );
            assert!(
                display_name
                    .as_ref()
                    .is_some_and(|n| n.expose() == "Ada Test"),
                "the display name is not the fixture's (L3)"
            );
        }
        _ => panic!("the AccountReady is not the last event"),
    }
    // `done` only after the core's ack.
    let mut before = Vec::new();
    run.quiet_for(300, &mut before);
    assert!(
        !before.iter().any(|e| is_step(e, step::DONE)),
        "done came before the core acked the AccountReady"
    );
    seen.extend(before);
    ack.complete(Ok(()));
    run.step(seen, step::DONE);
}

/// The brief's C9 `phone_code_login_from_the_fixture`: phone, code, linking, done; ONE number and
/// ONE code sent, `getMe` once, the account row holds the user id after the ack.
#[test]
fn phone_code_login_from_the_fixture() {
    let mut run = open(Setup::login(login_script(Vec::new())));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    assert_eq!(field_ids(&code), [field::CODE]);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(step_ids(&seen), ["phone", "code", "linking", "done"]);
    let txns: Vec<u64> = seen
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Login(s) => Some(s.txn),
            _ => None,
        })
        .collect();
    assert_eq!(txns, [1, 2, 3, 4], "the txns do not rise by one per step");
    assert_eq!(run.sent("setAuthenticationPhoneNumber"), 1);
    assert_eq!(run.sent("checkAuthenticationCode"), 1);
    assert_eq!(run.sent("getMe"), 1);
    let numbers = run.requests_of("setAuthenticationPhoneNumber");
    assert!(
        numbers[0]["phone_number"] == NUMBER,
        "the number sent is not the typed one made digits"
    );
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(run.row().expect("row").user_id, Some(OWN_ID));
}

/// Q-8: an `AccountReady` the core refuses for good (quarantined) ends the login `Failed{Internal}`
/// at `linking`, never `done`: the ack's result decides, not the end of the shipping pass (the
/// builder run's M-C9-6 survived every ack-in-time test; this one kills it).
#[test]
fn a_refused_account_ready_fails_the_login_at_linking() {
    let mut run = open(Setup::login(login_script(Vec::new())));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    let ack = run
        .until(&mut seen, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .expect("the AccountReady is not durable");
    ack.complete(Err(kourier_model::PersistError {
        retryable: false,
        detail: "synthetic refusal".to_owned(),
    }));
    run.until(&mut seen, |e| {
        matches!(
            e,
            BackendEvent::Login(LoginStep {
                kind: LoginStepKind::Failed { .. } | LoginStepKind::Done,
                ..
            })
        )
    });
    match seen.last() {
        Some(BackendEvent::Login(s)) => {
            assert_eq!(s.step_id, step::LINKING);
            assert_eq!(failure(s).kind, ErrorKind::Internal);
        }
        _ => panic!("the last event is not the failed step"),
    }
    assert!(
        !seen.iter().any(|e| is_step(e, step::DONE)),
        "done came after a refused AccountReady"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// The brief's C9 `qr_login_reaches_done_after_the_core_acks` (critic M6's once-only request):
/// `requestQrCodeAuthentication` ONCE with the `tg:` ids of the profile's other accounts (a `pn:`
/// id is not one), every link a new `qr` step, then Ready (from the other device) -> linking ->
/// the `AccountReady` -> done only after the ack.
#[test]
fn qr_login_reaches_done_after_the_core_acks() {
    let script = login_script(vec![Rule::new(
        "requestQrCodeAuthentication",
        vec![ok(), qr_update(QR_LINK)],
    )]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow(
        "qr",
        vec![taken("tg", "4242"), taken("pn", "999000111222")],
    ));
    let first = run.step(&mut seen, step::QR);
    match &first.kind {
        LoginStepKind::ShowQr { data, .. } => {
            assert!(data.expose() == QR_LINK, "the QR step is not TDLib's link");
        }
        _ => panic!("the qr step shows no QR code"),
    }
    let second_link = "tg://login?token=c2Vjb25k";
    run.td.push_update(qr_update(second_link));
    let second = loop {
        let s = run.step(&mut seen, step::QR);
        if s.txn > first.txn {
            break s;
        }
    };
    match &second.kind {
        LoginStepKind::ShowQr { data, .. } => {
            assert!(data.expose() == second_link, "the new link is not shown");
        }
        _ => panic!("the second qr step shows no QR code"),
    }
    run.td.push_update(auth_state("authorizationStateReady"));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(step_ids(&seen), ["qr", "qr", "linking", "done"]);
    assert_eq!(run.sent("requestQrCodeAuthentication"), 1);
    let asked = run.requests_of("requestQrCodeAuthentication");
    assert_eq!(asked[0]["other_user_ids"], json!([4242]));
    assert_eq!(run.sent("setAuthenticationPhoneNumber"), 0);
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Critic B1: a wrong code shows the SAME code step again as `code-again` (txn + 1, no `Failed`
/// step, no new number: no new code is sent), and the right one logs in. `requests` holds exactly
/// ONE `setAuthenticationPhoneNumber`.
#[test]
fn a_wrong_code_then_the_right_one_logs_in_with_one_set_number() {
    let mut run = open(Setup::login(login_script(Vec::new())));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, WRONG_CODE)]));
    let again = run.step(&mut seen, step::CODE_AGAIN);
    assert_eq!(again.txn, code.txn + 1);
    assert_eq!(field_ids(&again), [field::CODE]);
    assert!(!any_failed(&seen), "a wrong code ended the login");
    run.send(input(again.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(
        step_ids(&seen),
        ["phone", "code", "code-again", "linking", "done"]
    );
    assert_eq!(
        run.sent("setAuthenticationPhoneNumber"),
        1,
        "a second number was sent"
    );
    assert_eq!(run.sent("checkAuthenticationCode"), 2);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Critic H1: a command that comes while the code check waits for TDLib (its answer held by a
/// deferred rule) is served, and the answer is NOT lost: once released, `code-again` comes.
#[test]
fn a_command_during_a_code_check_keeps_its_answer() {
    let script = login_script(vec![
        Rule::when(
            "checkAuthenticationCode",
            "/code",
            json!(WRONG_CODE),
            vec![td_error(400, "PHONE_CODE_INVALID")],
        )
        .deferring(),
    ]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, WRONG_CODE)]));
    let td = run.td.clone();
    run.wait("the code check to be held", || td.held() == 1);
    // Commands while the answer waits: a hint and a request with a reply (answered at once).
    run.send(Command::NetworkChanged);
    let (reply, answer) = tokio::sync::oneshot::channel();
    run.send(Command::ListOwnDevices { reply });
    let answered = run.rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("the request waited for the code check")
            .expect("the request was dropped")
    });
    assert!(answered.is_err(), "the request was not refused");
    assert_eq!(run.td.release(), 1);
    let again = run.step(&mut seen, step::CODE_AGAIN);
    assert_eq!(again.txn, code.txn + 1);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// His U-2 answer and critic M1/M10: the 2FA password step carries Telegram's hint as the field's
/// note (foreign text, one line), a wrong password shows `password-again`, and the right one (with
/// `"`, `\`, a newline and non-ASCII) reaches TDLib exactly as typed (the rule matches only the
/// decoded value) and logs in. No recovery request is ever made.
#[test]
fn password_login_with_the_hint_as_note() {
    let script = login_script(vec![
        Rule::when(
            "checkAuthenticationCode",
            "/code",
            json!(CODE),
            vec![
                auth_update(json!({
                    "@type": "authorizationStateWaitPassword",
                    "password_hint": HINT,
                    "has_recovery_email_address": true,
                    "recovery_email_address_pattern": "a***@example.invalid",
                })),
                ok(),
            ],
        ),
        Rule::when(
            "checkAuthenticationPassword",
            "/password",
            json!(PASSWORD),
            vec![auth_state("authorizationStateReady"), ok()],
        ),
        Rule::new(
            "checkAuthenticationPassword",
            vec![td_error(400, "PASSWORD_HASH_INVALID")],
        ),
    ]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    let password = run.step(&mut seen, step::PASSWORD);
    match &password.kind {
        LoginStepKind::Input { fields } => {
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].id, field::PASSWORD);
            assert_eq!(fields[0].kind, InputKind::Secret);
            assert_eq!(
                fields[0].note,
                Some(NetworkNote {
                    explanation: Some(HINT.to_owned()),
                    appeal_url: None,
                }),
                "the hint is not the field's note"
            );
        }
        _ => panic!("the password step is not an input step"),
    }
    run.send(input(password.txn, &[(field::PASSWORD, "synthetic-wrong")]));
    let again = run.step(&mut seen, step::PASSWORD_AGAIN);
    assert_eq!(again.txn, password.txn + 1);
    run.send(input(again.txn, &[(field::PASSWORD, PASSWORD)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(
        step_ids(&seen),
        [
            "phone",
            "code",
            "password",
            "password-again",
            "linking",
            "done"
        ]
    );
    assert!(
        run.requests_of("checkAuthenticationPassword")
            .iter()
            .any(|r| r["password"] == PASSWORD),
        "the password did not reach TDLib as typed"
    );
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// His U-3 answer: the login e-mail is typed in Kourier: phone, email, email-code, linking, done.
#[test]
fn email_login() {
    let script = login_script(vec![
        Rule::when(
            "setAuthenticationPhoneNumber",
            "/phone_number",
            json!(NUMBER),
            vec![
                auth_update(json!({
                    "@type": "authorizationStateWaitEmailAddress",
                    "allow_apple_id": true,
                    "allow_google_id": true,
                })),
                ok(),
            ],
        ),
        Rule::when(
            "setAuthenticationEmailAddress",
            "/email_address",
            json!(EMAIL),
            vec![
                auth_update(json!({
                    "@type": "authorizationStateWaitEmailCode",
                    "allow_apple_id": false,
                    "allow_google_id": false,
                    "code_info": {
                        "@type": "emailAddressAuthenticationCodeInfo",
                        "email_address_pattern": "a***@example.invalid",
                        "length": 5,
                    },
                })),
                ok(),
            ],
        ),
        Rule::when(
            "checkAuthenticationEmailCode",
            "/code/code",
            json!(EMAIL_CODE),
            vec![auth_state("authorizationStateReady"), ok()],
        ),
    ]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow("phone", Vec::new()));
    let phone = run.step(&mut seen, step::PHONE);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    let email = run.step(&mut seen, step::EMAIL);
    assert_eq!(field_ids(&email), [field::EMAIL]);
    run.send(input(email.txn, &[(field::EMAIL, EMAIL)]));
    let email_code = run.step(&mut seen, step::EMAIL_CODE);
    assert_eq!(field_ids(&email_code), [field::CODE]);
    run.send(input(email_code.txn, &[(field::CODE, EMAIL_CODE)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(
        step_ids(&seen),
        ["phone", "email", "email-code", "linking", "done"]
    );
    assert_eq!(run.sent("resetAuthenticationEmailAddress"), 0);
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// §3.4: a number that is another account of the profile is refused BEFORE
/// `setAuthenticationPhoneNumber` (no code is sent to it): `Failed{AlreadyAdded}` at `phone`; the
/// core's `Logout` that follows ends the actor with no device to remove.
#[test]
fn a_number_of_another_account_is_refused_before_the_code() {
    let mut run = open(Setup::login(login_script(vec![closing_logout()])));
    let mut seen = Vec::new();
    run.send(start_flow("phone", vec![taken("pn", DIGITS)]));
    let phone = run.step(&mut seen, step::PHONE);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    let failed = run.step(&mut seen, step::PHONE);
    assert_eq!(
        failure(&failed).kind,
        ErrorKind::AlreadyAdded { logged_out: false }
    );
    assert_eq!(
        run.sent("setAuthenticationPhoneNumber"),
        0,
        "the number was sent"
    );
    run.send(Command::Logout);
    let mut after = Vec::new();
    run.until(&mut after, |e| matches!(e, BackendEvent::Logout(_)));
    match after.last() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, kourier_model::DeviceLogout::NoDevice);
        }
        _ => panic!("no logout report"),
    }
    assert!(matches!(run.join(), Exit::LoggedOut { .. }));
}

/// A state Kourier refuses after the number (`state`): `Failed{Unsupported}` at the step of its own
/// (`at`: `registration`, `purchase`; KT-1b), and the request that state asks for is never sent (the
/// helper would refuse it too: C3).
fn a_refused_state(state: &str, never: &str, at: &str) {
    let script = login_script(vec![Rule::when(
        "setAuthenticationPhoneNumber",
        "/phone_number",
        json!(NUMBER),
        vec![auth_state(state), ok()],
    )]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow("phone", Vec::new()));
    let phone = run.step(&mut seen, step::PHONE);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    let failed = run.step(&mut seen, at);
    assert!(
        failed.txn > phone.txn,
        "the failure is a step after the number's"
    );
    assert_eq!(failure(&failed).kind, ErrorKind::Unsupported);
    let mut more = Vec::new();
    run.quiet_for(300, &mut more);
    assert!(more.is_empty(), "the login went on after its failed step");
    assert_eq!(run.sent(never), 0, "the refused request was sent");
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// TSC H6: Kourier never registers a new Telegram account.
#[test]
fn registration_is_refused_and_never_sent() {
    a_refused_state(
        "authorizationStateWaitRegistration",
        "registerUser",
        step::REGISTRATION,
    );
}

/// A login that asks for a purchase is refused; nothing is bought.
#[test]
fn premium_purchase_is_refused() {
    a_refused_state(
        "authorizationStateWaitPremiumPurchase",
        "checkAuthenticationPremiumPurchase",
        step::PURCHASE,
    );
}

/// Critic M6: a QR login ends `Failed{Expired}` at the eleventh link (ten shown), and
/// `requestQrCodeAuthentication` was sent once.
#[test]
fn a_qr_login_ends_after_ten_links() {
    let script = login_script(vec![Rule::new(
        "requestQrCodeAuthentication",
        vec![ok(), qr_update(QR_LINK)],
    )]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow("qr", Vec::new()));
    run.step(&mut seen, step::QR);
    for n in 2..=(QR_MAX_LINKS + 1) {
        run.td
            .push_update(qr_update(&format!("tg://login?token=synthetic{n}")));
    }
    run.until(&mut seen, |e| {
        matches!(
            e,
            BackendEvent::Login(LoginStep {
                kind: LoginStepKind::Failed { .. },
                ..
            })
        )
    });
    let shown = seen
        .iter()
        .filter(|e| {
            matches!(
                e,
                BackendEvent::Login(LoginStep {
                    kind: LoginStepKind::ShowQr { .. },
                    ..
                })
            )
        })
        .count();
    assert_eq!(shown, QR_MAX_LINKS as usize, "not ten links shown");
    match seen.last() {
        Some(BackendEvent::Login(s)) => {
            assert_eq!(s.step_id, step::QR);
            assert_eq!(failure(s).kind, ErrorKind::Expired);
        }
        _ => panic!("the last event is not the failed step"),
    }
    assert_eq!(run.sent("requestQrCodeAuthentication"), 1);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Critic M6: a QR login nobody scans ends `Failed{Expired}` five minutes (the actor's clock)
/// after its one request, and not before.
#[test]
fn a_qr_login_ends_after_five_minutes() {
    let script = login_script(vec![Rule::new(
        "requestQrCodeAuthentication",
        vec![ok(), qr_update(QR_LINK)],
    )]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow("qr", Vec::new()));
    let qr = run.step(&mut seen, step::QR);
    run.clock.set(T0 + QR_BOUND_MS - 1);
    let mut early = Vec::new();
    run.quiet_for(300, &mut early);
    assert!(!any_failed(&early), "the QR login ended before its bound");
    run.clock.set(T0 + QR_BOUND_MS);
    let ended = loop {
        let s = run.step(&mut seen, step::QR);
        if s.txn > qr.txn {
            break s;
        }
    };
    assert_eq!(failure(&ended).kind, ErrorKind::Expired);
    assert_eq!(run.sent("requestQrCodeAuthentication"), 1);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Fix J1: the QR bound covers the wait for a SCAN only. A QR login scanned just before its bound
/// whose account has a password stays open while the password is typed: the clock passes the bound
/// and nothing ends; the password then logs in.
#[test]
fn a_scanned_qr_login_is_not_ended_by_the_bound_while_the_password_is_typed() {
    let script = login_script(vec![
        Rule::new(
            "requestQrCodeAuthentication",
            vec![ok(), qr_update(QR_LINK)],
        ),
        Rule::when(
            "checkAuthenticationPassword",
            "/password",
            json!(PASSWORD),
            vec![auth_state("authorizationStateReady"), ok()],
        ),
    ]);
    let mut run = open(Setup::login(script));
    let mut seen = Vec::new();
    run.send(start_flow("qr", Vec::new()));
    run.step(&mut seen, step::QR);
    run.clock.set(T0 + QR_BOUND_MS - 10_000);
    run.td.push_update(auth_update(json!({
        "@type": "authorizationStateWaitPassword",
        "password_hint": HINT,
        "has_recovery_email_address": false,
        "recovery_email_address_pattern": "",
    })));
    let password = run.step(&mut seen, step::PASSWORD);
    run.clock.set(T0 + QR_BOUND_MS + 5_000);
    let mut late = Vec::new();
    run.quiet_for(400, &mut late);
    assert!(
        !any_failed(&late),
        "the QR bound ended a login that was scanned"
    );
    run.send(input(password.txn, &[(field::PASSWORD, PASSWORD)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(step_ids(&seen), ["qr", "password", "linking", "done"]);
    assert_eq!(run.sent("requestQrCodeAuthentication"), 1);
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Fix J6 (the builder run's MU1): a login cancelled after TDLib said Ready (the AccountReady is
/// not acked yet) leaves a Telegram session; the `Logout` that follows must `logOut` and report
/// it removed, not `NoDevice`.
#[test]
fn a_cancel_after_ready_logs_the_telegram_session_out() {
    let mut run = open(Setup::login(login_script(vec![closing_logout()])));
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    // The AccountReady is shipped; the core has not acked it.
    let ack = run
        .until(&mut seen, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .expect("the AccountReady is not durable");
    run.send(Command::CancelLogin);
    run.send(Command::Logout);
    run.until(&mut seen, |e| matches!(e, BackendEvent::Logout(_)));
    match seen.last() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, kourier_model::DeviceLogout::Removed);
        }
        _ => panic!("no logout report"),
    }
    drop(ack);
    assert!(matches!(run.join(), Exit::LoggedOut { .. }));
    assert_eq!(run.sent("logOut"), 1, "no logOut for a session that exists");
}

/// Fix J6: the same cancel after Ready, but the helper died while `getMe` waited and the restart's
/// back-off has not ended: no helper runs, the actor's user id is not known yet, and the report
/// is `NotReached` (a Telegram session exists), not `NoDevice`.
#[test]
fn a_cancel_after_ready_across_a_helper_death_reports_not_reached() {
    logout_after_ready_across_a_helper_death(true);
}

/// Fix J6 (R-J6-d): the same, but with NO `CancelLogin` (the core sends a `Logout` alone after a
/// refusal that follows Ready): the login session is still there, and `logout()` itself must read
/// that TDLib said Ready once. Without that read the report is `NoDevice`.
#[test]
fn a_plain_logout_after_ready_across_a_helper_death_reports_not_reached() {
    logout_after_ready_across_a_helper_death(false);
}

/// TDLib says Ready, the helper dies while `getMe` waits, and in the back-off (no helper runs, the
/// user id is not known) the core logs out, after a `CancelLogin` or without one.
fn logout_after_ready_across_a_helper_death(cancel_first: bool) {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let mut setup = Setup::login(login_script(Vec::new()));
    setup.child = Some((&["login-ready-no-me"], pids.clone()));
    let mut run = open(setup);
    let mut seen = Vec::new();
    run.send(start_flow("phone", Vec::new()));
    let phone = run.step(&mut seen, step::PHONE);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    let code = run.step(&mut seen, step::CODE);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    let first = pids_of(&pids);
    assert_eq!(first.len(), 1, "not exactly one helper before the death");
    kill_helper(first[0]);
    {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(next_retry(ends, T0));
    }
    // The clock stays: the back-off waits, no helper runs.
    if cancel_first {
        run.send(Command::CancelLogin);
    }
    run.send(Command::Logout);
    run.until(&mut seen, |e| matches!(e, BackendEvent::Logout(_)));
    match seen.last() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, kourier_model::DeviceLogout::NotReached);
        }
        _ => panic!("no logout report"),
    }
    assert!(matches!(run.join(), Exit::LoggedOut { .. }));
    assert_eq!(
        run.spawns.load(Ordering::SeqCst),
        1,
        "a helper was started during the back-off"
    );
    run.wait("the helper reaped", || reaped(first[0]));
}

// ----- a linked account: resume, connection, freeze, logout, presence, hints -----

/// The fixture's own identity as an `AccountReady`.
fn own_ready_event() -> BackendEvent {
    BackendEvent::AccountReady {
        own_ids: vec![
            UserId::from_parts("tg", &OWN_ID.to_string()).expect("id"),
            UserId::from_parts("pn", DIGITS).expect("id"),
        ],
        device: None,
        display_name: None,
        number: None,
    }
}

impl Run {
    /// Waits (10 s at most) until the account's connection state satisfies `f`.
    fn conn_until(&mut self, what: &str, f: impl FnMut(&ConnState) -> bool) -> ConnState {
        let Self { rt, ends, .. } = self;
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), ends.conn.wait_for(f))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .expect("the actor ended")
                .clone()
        })
    }

    /// Events until the account shows `Online` (10 s at most), every durable envelope acked
    /// (TG-3 C7b, L-6: Online follows the core's ack of the start sync's rows), all of them
    /// pushed to `seen`.
    fn online(&mut self, seen: &mut Vec<BackendEvent>) {
        let Self { rt, ends, .. } = self;
        rt.block_on(async {
            let by = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if matches!(*ends.conn.borrow_and_update(), ConnState::Online { .. }) {
                    return;
                }
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let envelope = envelope.expect("events ended");
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                    }
                    changed = ends.conn.changed() => changed.expect("the actor ended"),
                    () = tokio::time::sleep_until(by) => panic!("timed out waiting for Online"),
                }
            }
        });
    }

    /// The `online` values of every `setOption online` TDLib got, in order.
    fn online_told(&self) -> Vec<bool> {
        self.requests_of("setOption")
            .iter()
            .filter(|r| r["name"] == "online")
            .filter_map(|r| r["value"]["value"].as_bool())
            .collect()
    }

    /// No request that downloads (a file, a part of one) reached TDLib.
    fn assert_nothing_downloads(&self) {
        let downloads = self
            .td
            .sent_types()
            .iter()
            .filter(|t| {
                matches!(
                    t.as_str(),
                    "downloadFile"
                        | "addFileToDownloads"
                        | "readFilePart"
                        | "getRemoteFile"
                        | "preliminaryUploadFile"
                )
            })
            .count();
        assert_eq!(downloads, 0, "a download request reached TDLib");
    }

    /// The events queued now (no wait), acking durable envelopes.
    fn drain(&mut self) -> Vec<BackendEvent> {
        let mut seen = Vec::new();
        while let Ok(envelope) = self.ends.events.try_recv() {
            seen.extend(envelope.events);
            if let Some(ack) = envelope.ack {
                ack.complete(Ok(()));
            }
        }
        seen
    }
}

fn holds(seen: &[BackendEvent]) -> Vec<Option<kourier_model::AccountHold>> {
    seen.iter()
        .filter_map(|e| match e {
            BackendEvent::AccountHold(h) => Some(*h),
            _ => None,
        })
        .collect()
}

/// §3.3 and critic M4: a resumed account shows `Connecting` at its start, then TDLib's
/// connection states mapped (`Updating` -> `Syncing`, `Ready` -> `Online`); the hold is stated
/// once, `AccountHold(None)` (no freeze seen), BEFORE the first `Syncing`. No login request.
#[test]
fn a_resume_connects_and_maps_connection_states() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.conn_until("Connecting", |c| *c == ConnState::Connecting);
    run.td.push_update(conn_update("connectionStateConnecting"));
    run.td.push_update(conn_update("connectionStateUpdating"));
    run.conn_until("Syncing", |c| matches!(c, ConnState::Syncing { .. }));
    let before_online = run.drain();
    assert_eq!(
        holds(&before_online),
        [None],
        "the hold was not stated once before Syncing"
    );
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    let mut later = Vec::new();
    run.quiet_for(200, &mut later);
    assert!(
        holds(&later).is_empty(),
        "the hold was stated twice in one connect"
    );
    assert_eq!(run.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(run.sent("setAuthenticationPhoneNumber"), 0);
    assert_eq!(run.sent("getMe"), 0);
    run.assert_nothing_downloads();
    run.assert_nothing_forbidden();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// A request answered 401 is never a logout (only TDLib's own authorization state is): the
/// watchdog's probe answered `401 Unauthorized` over and over leaves the account online.
#[test]
fn a_401_answer_is_not_a_logout() {
    let rules = vec![Rule::new(
        "testCallEmpty",
        vec![td_error(401, "Unauthorized")],
    )];
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.timings = Timings {
        probe_every: Duration::from_millis(200),
        ..short()
    };
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    let td = run.td.clone();
    run.wait("three probes", || {
        td.sent_types()
            .iter()
            .filter(|t| *t == "testCallEmpty")
            .count()
            >= 3
    });
    assert!(
        matches!(*run.ends.conn.borrow(), ConnState::Online { .. }),
        "a 401 answer changed the account's state"
    );
    let seen = run.drain();
    assert!(
        !seen.iter().any(|e| matches!(e, BackendEvent::Logout(_))),
        "a 401 answer was told as a logout"
    );
    assert_eq!(
        run.spawns.load(Ordering::SeqCst),
        1,
        "a 401 answer restarted the helper"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Critic M7: TDLib's OWN logout after Ready (the session ended on another device) is
/// `LoggedOut{cause: Unknown}` and `Exit::LoggedOut`, and NO `BackendEvent::Logout` (that one
/// answers the core's `Logout` only).
#[test]
fn tdlibs_own_logging_out_after_ready_is_a_logout() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    assert!(
        run.tdlib_dir().exists(),
        "the start made no tdlib directory"
    );
    assert!(
        run.files_dir().exists(),
        "the start made no files directory"
    );
    run.td
        .push_update(auth_state("authorizationStateLoggingOut"));
    let state = run.conn_until("LoggedOut", |c| matches!(c, ConnState::LoggedOut { .. }));
    match state {
        ConnState::LoggedOut { cause, reason, .. } => {
            assert_eq!(cause, kourier_model::LogoutCause::Unknown);
            assert_eq!(reason, crate::states::TELEGRAM_ENDED);
        }
        _ => unreachable!("waited for LoggedOut"),
    }
    assert_eq!(
        run.join(),
        Exit::LoggedOut {
            reason: crate::states::TELEGRAM_ENDED.to_owned()
        }
    );
    let seen = run.drain();
    assert!(
        !seen.iter().any(|e| matches!(e, BackendEvent::Logout(_))),
        "TDLib's own logout was told as a Logout event"
    );
    // J8: TDLib's unfinished logout must not greet the next relink: its data is removed before
    // the exit is reported.
    assert!(
        !run.tdlib_dir().exists(),
        "tdlib/ stayed after a remote logout"
    );
    assert!(
        !run.files_dir().exists(),
        "the files directory stayed after a remote logout"
    );
}

/// L-8 with M7: a resumed account whose TDLib no longer knows it (its first state is a login
/// state) is logged out the same way, without any login request.
#[test]
fn a_resumed_account_tdlib_no_longer_knows_is_logged_out() {
    let mut run = open(Setup::resume(Script::default(), OWN_ID));
    run.send(Command::Connect);
    assert_eq!(
        run.join(),
        Exit::LoggedOut {
            reason: crate::states::SESSION_GONE.to_owned()
        }
    );
    assert_eq!(run.sent("setAuthenticationPhoneNumber"), 0);
    assert_eq!(run.sent("requestQrCodeAuthentication"), 0);
    let seen = run.drain();
    assert!(!seen.iter().any(|e| matches!(e, BackendEvent::Logout(_))));
}

/// Critic H4 (and the brief's `kouriers_logout_logs_out_then_destroys_offline_and_removes_tdlib`):
/// Kourier's logout of a linked account whose TDLib never closes (offline: every TG-2 build)
/// asks `logOut`, waits 4 s, asks `destroy`, waits 2 s, and REPORTS `NotReached` within 8 s of
/// the command, inside the core's 10 s window; then `tdlib/` and the files directory are gone.
#[test]
fn the_logout_reports_within_eight_seconds_offline() {
    let rules = vec![
        Rule::new("logOut", vec![ok()]),
        Rule::new("destroy", vec![ok()]),
    ];
    let mut run = open(Setup::resume(ready_script(rules), OWN_ID));
    run.send(Command::Connect);
    run.conn_until("Connecting", |c| *c == ConnState::Connecting);
    assert!(
        run.tdlib_dir().exists(),
        "the start made no tdlib directory"
    );
    assert!(
        run.files_dir().exists(),
        "the start made no files directory"
    );
    let t0 = StdInstant::now();
    run.send(Command::Logout);
    let mut seen = Vec::new();
    run.until(&mut seen, |e| matches!(e, BackendEvent::Logout(_)));
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(8), "the logout took {took:?}");
    assert!(
        took >= crate::actor::LOGOUT_WAIT,
        "the logout did not wait for TDLib's close"
    );
    match seen.last() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, kourier_model::DeviceLogout::NotReached);
        }
        _ => panic!("no logout report"),
    }
    assert!(matches!(run.join(), Exit::LoggedOut { .. }));
    let sent = run.td.sent_types();
    let log_out = sent.iter().position(|t| t == "logOut").expect("no logOut");
    let destroy = sent
        .iter()
        .position(|t| t == "destroy")
        .expect("no destroy");
    assert!(log_out < destroy, "destroy came before logOut");
    assert_eq!(run.sent("logOut"), 1);
    assert!(!run.tdlib_dir().exists(), "tdlib/ was not removed");
    assert!(
        !run.files_dir().exists(),
        "the files directory was not removed"
    );
}

/// His U-5 answer: a freeze holds the account (`Frozen`, no end, no link), stated at once and
/// counting as the connect's statement (no `None` before Online); a thaw lifts it.
#[test]
fn a_freeze_holds_and_a_thaw_lifts() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.conn_until("Connecting", |c| *c == ConnState::Connecting);
    run.td
        .push_update(json!({"@type": "updateFreezeState", "is_frozen": true,
        "freezing_date": 1, "deletion_date": 2, "appeal_link": "https://example.invalid/"}));
    let mut seen = Vec::new();
    run.until(&mut seen, |e| matches!(e, BackendEvent::AccountHold(_)));
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut seen);
    run.td
        .push_update(json!({"@type": "updateFreezeState", "is_frozen": false,
        "freezing_date": 0, "deletion_date": 0, "appeal_link": ""}));
    run.until(&mut seen, |e| matches!(e, BackendEvent::AccountHold(None)));
    assert_eq!(
        holds(&seen),
        [
            Some(kourier_model::AccountHold {
                until_ms: None,
                reason: kourier_model::HoldReason::Frozen,
            }),
            None,
        ]
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// Critic H6 and T-3 (his "only the open account" answer), at the actor: nothing is told during
/// the login although the window has focus and this account's chat is open; `online true` at
/// once after `done`; `false` 5 s (the actor's clock) after the focus went; `true` again after the
/// 2 s gap; and a stop of an account shown online tells `false` BEFORE the helper is quit.
#[test]
fn online_follows_focus_after_ready_only() {
    let mut run = open(Setup::login(login_script(Vec::new())));
    let open_chat = kourier_backend_api::UiPresence {
        focused: true,
        visible: Some(kourier_model::ConvKey::Group(Arc::from("tg:-1"))),
        idle: false,
    };
    run.ends.ui.send_replace(open_chat.clone());
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    run.step(&mut seen, step::LINKING);
    assert!(
        run.online_told().is_empty(),
        "online was told during the login"
    );
    ready_then_done(&mut run, &mut seen);
    let td = run.td.clone();
    let told = |n: usize| {
        let td = td.clone();
        move || {
            td.requests()
                .iter()
                .filter(|(_, r)| r["@type"] == "setOption" && r["name"] == "online")
                .count()
                >= n
        }
    };
    run.wait("online after done", told(1));
    assert_eq!(run.online_told(), [true]);
    run.ends
        .ui
        .send_replace(kourier_backend_api::UiPresence::default());
    run.quiet_for(200, &mut seen);
    assert_eq!(run.online_told(), [true], "offline came before 5 s");
    run.clock.set(T0 + crate::presence::OFF_AFTER_MS);
    run.wait("offline after 5 s", told(2));
    assert_eq!(run.online_told(), [true, false]);
    run.ends.ui.send_replace(open_chat);
    run.quiet_for(200, &mut seen);
    assert_eq!(
        run.online_told(),
        [true, false],
        "online came inside the 2 s gap"
    );
    run.clock
        .set(T0 + crate::presence::OFF_AFTER_MS + crate::presence::MIN_GAP_MS);
    run.wait("online after the gap", told(3));
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(run.online_told(), [true, false, true, false]);
    let sent = run.td.sent_types();
    let last_online = sent
        .iter()
        .rposition(|t| t == "setOption")
        .expect("no setOption");
    let close = sent.iter().position(|t| t == "close").expect("no close");
    assert!(
        last_online < close,
        "offline was told after the helper's quit"
    );
}

/// Critic M8: `NetworkChanged` sets TDLib's network type only while TDLib is not connected,
/// `Resumed` always, and at most once per 10 s (the actor's clock).
#[test]
fn a_network_hint_sets_the_network_type() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.conn_until("Connecting", |c| *c == ConnState::Connecting);
    run.send(Command::NetworkChanged);
    let td = run.td.clone();
    run.wait("the network type", || {
        td.sent_types().iter().any(|t| t == "setNetworkType")
    });
    run.send(Command::NetworkChanged);
    run.quiet_for(200, &mut Vec::new());
    assert_eq!(
        run.sent("setNetworkType"),
        1,
        "a second hint inside 10 s was passed on"
    );
    run.clock.set(T0 + crate::presence::HINT_GAP_MS);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    run.send(Command::NetworkChanged);
    run.quiet_for(200, &mut Vec::new());
    assert_eq!(
        run.sent("setNetworkType"),
        1,
        "a network change while Ready was passed on"
    );
    run.send(Command::Resumed);
    let td = run.td.clone();
    run.wait("the network type after a resume", || {
        td.sent_types()
            .iter()
            .filter(|t| *t == "setNetworkType")
            .count()
            == 2
    });
    for r in run.requests_of("setNetworkType") {
        assert_eq!(r["type"]["@type"], "networkTypeOther");
    }
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// The brief's `nothing_downloads`: a linked account that TDLib tells of files and messages
/// asks for no download (TG-2 has no receive path; the files directory stays empty).
#[test]
fn nothing_downloads() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    run.td.push_update(
        json!({"@type": "updateFile", "file": {"@type": "file", "id": 7,
        "size": 10, "expected_size": 10, "local": {"@type": "localFile", "path": ""},
        "remote": {"@type": "remoteFile", "id": "synthetic", "unique_id": "synthetic"}}}),
    );
    run.quiet_for(300, &mut Vec::new());
    run.assert_nothing_downloads();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// The brief's `nothing_typed_reaches_a_log_line`, grown (the TG-1b `actor::tests` case, which
/// never starts a helper): a whole password login over the scripted TDLib, every line of the
/// actor's thread and of the link threads it starts captured at trace: the number, the code, both
/// passwords, the hint and the own name never reach a line.
#[test]
fn nothing_typed_reaches_a_log_line() {
    let script = login_script(vec![
        Rule::when(
            "checkAuthenticationCode",
            "/code",
            json!(CODE),
            vec![
                auth_update(json!({
                    "@type": "authorizationStateWaitPassword",
                    "password_hint": HINT,
                })),
                ok(),
            ],
        ),
        Rule::when(
            "checkAuthenticationPassword",
            "/password",
            json!(PASSWORD),
            vec![auth_state("authorizationStateReady"), ok()],
        ),
        Rule::new(
            "checkAuthenticationPassword",
            vec![td_error(400, "PASSWORD_HASH_INVALID")],
        ),
    ]);
    let lines = Lines::default();
    let mut setup = Setup::login(script);
    setup.lines = Some(lines.clone());
    let mut run = open(setup);
    let mut seen = Vec::new();
    let code = to_the_code_step(&mut run, &mut seen);
    run.send(input(code.txn, &[(field::CODE, CODE)]));
    let password = run.step(&mut seen, step::PASSWORD);
    run.send(input(password.txn, &[(field::PASSWORD, "synthetic-wrong")]));
    let again = run.step(&mut seen, step::PASSWORD_AGAIN);
    run.send(input(again.txn, &[(field::PASSWORD, PASSWORD)]));
    run.step(&mut seen, step::LINKING);
    ready_then_done(&mut run, &mut seen);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    let all = lines.all();
    assert!(
        all.iter().any(|l| l.contains("files directory")),
        "the capture saw none of the actor's lines, so it proves nothing"
    );
    for secret in [
        NUMBER,
        DIGITS,
        CODE,
        PASSWORD,
        "synthetic-wrong",
        HINT,
        "Ada Test",
    ] {
        assert!(
            !all.iter().any(|l| l.contains(secret)),
            "a typed or foreign value reached a log line"
        );
    }
}

/// An `AccountReady` whose ack a crash lost ships FIRST at the next start, before any command is
/// read and with no helper started; acked, it goes from the outbox (a second start ships nothing).
#[test]
fn an_account_ready_left_by_a_crash_ships_first_at_the_next_start() {
    let mut setup = Setup::resume(ready_script(Vec::new()), OWN_ID);
    setup.ready_left = true;
    let mut run = open(setup);
    let mut seen = Vec::new();
    let ack = run
        .until(&mut seen, |e| {
            matches!(e, BackendEvent::AccountReady { .. })
        })
        .expect("the AccountReady is not durable");
    assert_eq!(seen.len(), 1, "something came before the AccountReady");
    assert_eq!(seen[0], own_ready_event());
    ack.complete(Ok(()));
    assert_eq!(
        run.spawns.load(Ordering::SeqCst),
        0,
        "a helper started to ship it"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    let conn = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM core_outbox", [], |r| r.get(0))
        .expect("count");
    assert_eq!(left, 0, "the acked row stayed");
}

// ----- layer (b), TG-2 C10: the actor over the fake helper PROCESS -----

/// Kills the helper process `pid` at once (SIGKILL), as a crash of TDLib would end it.
fn kill_helper(pid: u32) {
    let pid = libc::pid_t::try_from(pid).expect("a process id");
    // SAFETY: `kill` takes two plain integers; `pid` is a child this test started.
    let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    assert_eq!(rc, 0, "the helper could not be killed");
}

/// The process ids the child parts recorded so far.
fn pids_of(pids: &Pids) -> Vec<u32> {
    pids.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Critic B1 in layer (b): a helper that DIES while the login waits for the code (scenario
/// `login-wait-code`) shows no step and no `Failed`; after the back-off the next helper comes up
/// in WaitCode (`in-wait-code`, TDLib's binlog) and the login shows `code` again with a newer
/// txn, WITHOUT a second number (that helper aborts on one: a third helper would follow).
#[test]
fn a_helper_that_dies_in_wait_code_resumes_the_code_step_without_a_request() {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let mut setup = Setup::login(login_script(Vec::new()));
    setup.child = Some((&["login-wait-code", "in-wait-code"], pids.clone()));
    let mut run = open(setup);
    let mut seen = Vec::new();
    run.send(start_flow("phone", Vec::new()));
    let phone = run.step(&mut seen, step::PHONE);
    run.send(input(phone.txn, &[(field::PHONE, NUMBER)]));
    let code = run.step(&mut seen, step::CODE);
    let first = pids_of(&pids);
    assert_eq!(first.len(), 1, "not exactly one helper before the death");
    kill_helper(first[0]);
    let at = {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(next_retry(ends, T0))
    };
    run.clock.set(at);
    let again = run.step(&mut seen, step::CODE);
    assert!(
        again.txn > code.txn,
        "the code step came back with an old txn"
    );
    run.quiet_for(1_000, &mut seen);
    assert_eq!(
        run.spawns.load(Ordering::SeqCst),
        2,
        "not exactly two helpers (a third means the second got a number)"
    );
    assert_eq!(step_ids(&seen), ["phone", "code", "code"]);
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            BackendEvent::Login(LoginStep {
                kind: LoginStepKind::Failed { .. },
                ..
            })
        )),
        "the death showed a failed step"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    let both = pids_of(&pids);
    run.wait("the helpers reaped", || both.iter().all(|p| reaped(*p)));
}

/// Critic H4 in layer (b): Kourier's logout of a linked account whose helper answers `logOut`
/// and `destroy` but never closes, and whose close hangs (`logout-never-closes`), REPORTS
/// `NotReached` within 8 s of the command, ends `LoggedOut`, kills and reaps the helper (no 17 s
/// stop: the kill comes after the report) and removes `tdlib/` and the files directory.
#[test]
fn a_logout_that_never_closes_reports_kills_and_removes_tdlib() {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let mut setup = Setup::resume(Script::default(), OWN_ID);
    setup.child = Some((&["logout-never-closes"], pids.clone()));
    let mut run = open(setup);
    run.send(Command::Connect);
    run.conn_until("Connecting", |c| *c == ConnState::Connecting);
    assert!(
        run.tdlib_dir().exists(),
        "the start made no tdlib directory"
    );
    let t0 = StdInstant::now();
    run.send(Command::Logout);
    let mut seen = Vec::new();
    run.until(&mut seen, |e| matches!(e, BackendEvent::Logout(_)));
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(8), "the logout took {took:?}");
    match seen.last() {
        Some(BackendEvent::Logout(report)) => {
            assert_eq!(report.outcome, kourier_model::DeviceLogout::NotReached);
        }
        _ => panic!("no logout report"),
    }
    assert!(matches!(run.join(), Exit::LoggedOut { .. }));
    let ended = t0.elapsed();
    assert!(
        ended < Duration::from_secs(12),
        "the actor waited out the helper's stop: {ended:?}"
    );
    let helpers = pids_of(&pids);
    assert_eq!(helpers.len(), 1, "not exactly one helper");
    run.wait("the helper reaped", || reaped(helpers[0]));
    assert!(!run.tdlib_dir().exists(), "tdlib/ was not removed");
    assert!(
        !run.files_dir().exists(),
        "the files directory was not removed"
    );
}

// ---- TG-3 C7b: the group commit, the shipping arm, downloads -------------------------------

/// The private chat of the C7b cases (the scrub's synthetic values).
const PEER: i64 = 77;

fn peer_chat() -> serde_json::Value {
    json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": PEER,
        "type": {"@type": "chatTypePrivate", "user_id": PEER}, "title": "Bo Peer", "photo": null,
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "9000000000000000000", "is_pinned": false}],
        "has_protected_content": false, "is_marked_as_unread": false, "unread_count": 0,
        "last_read_inbox_message_id": 0, "last_read_outbox_message_id": 0,
        "unread_mention_count": 0, "notification_settings": {"@type": "chatNotificationSettings",
        "use_default_mute_for": true, "mute_for": 0}, "message_auto_delete_time": 0}})
}

fn peer_user() -> serde_json::Value {
    json!({"@type": "updateUser", "user": {"@type": "user", "id": PEER, "first_name": "Bo",
        "last_name": "Peer", "phone_number": "", "is_contact": true,
        "type": {"@type": "userTypeRegular"}}})
}

fn peer_message(id: i64) -> serde_json::Value {
    json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": id,
        "sender_id": {"@type": "messageSenderUser", "user_id": PEER}, "chat_id": PEER,
        "is_outgoing": false, "can_be_saved": true, "date": 1_700_000_100, "edit_date": 0,
        "media_album_id": "0", "content": {"@type": "messageText", "text":
        {"@type": "formattedText", "text": "synthetic text 951", "entities": []}}}})
}

fn message_locals(seen: &[BackendEvent]) -> Vec<String> {
    seen.iter()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m.local.to_string()),
            _ => None,
        })
        .collect()
}

/// L-4: shipping is a wake arm. While the core withholds the ack of the first receive envelope,
/// 10,000 pushed updates are still read, committed and stated (the last one, a connection state,
/// shows), with ONE envelope in flight; after the ack every message reaches the core once, in
/// order. The test waits on no wall time of its own: it withholds the ack until the last update
/// showed (the refresh's "acks after 2 s" is that point).
///
/// U11 (strike 1, load 153: the whole run was ONE 30 s no-progress window, 10,000 messages took
/// 31 s): the stream carries a MARKER after every [`MARK`] messages, a connection state that
/// flips between `Updating` (Syncing) and `Ready` (Online). The actor shows it only after
/// everything pushed before it was read, so each flip is progress the test sees while the ack is
/// still withheld, and the 30 s are counted from the last marker: the run may take as long as the
/// machine needs, a stop of 30 s between two markers (1,000 messages) is the failure.
#[test]
fn inbound_keeps_flowing_while_the_core_withholds_an_ack() {
    const COUNT: i64 = 10_000;
    /// Messages between two markers (about 3 s at the worst load seen).
    const MARK: i64 = 1_000;
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.online(&mut seen);
    run.td.push_update(peer_chat());
    run.td.push_update(peer_user());
    for k in 1..=COUNT {
        run.td.push_update(peer_message(k << 20));
        if k % MARK == 0 && k < COUNT {
            let state = if (k / MARK) % 2 == 1 {
                "connectionStateUpdating"
            } else {
                "connectionStateReady"
            };
            run.td.push_update(conn_update(state));
        }
    }
    run.td.push_update(conn_update("connectionStateConnecting"));
    let mut held = Vec::new();
    {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(async {
            // Progress-based (TF8, U11): it fails only after 30 s WITHOUT a new envelope or a
            // new state (every `MARK` messages one comes), so a slow machine cannot turn the
            // 10,000 messages into a timeout.
            loop {
                if *ends.conn.borrow_and_update() == ConnState::Connecting {
                    // What is already queued for the core counts too.
                    while let Ok(envelope) = ends.events.try_recv() {
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            held.push(ack);
                        }
                    }
                    return;
                }
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let envelope = envelope.expect("events ended");
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            held.push(ack);
                        }
                    }
                    changed = ends.conn.changed() => changed.expect("the actor ended"),
                    () = tokio::time::sleep(Duration::from_secs(30)) => {
                        panic!("the inbound stopped while the core withheld an ack")
                    }
                }
            }
        });
    }
    assert_eq!(
        held.len(),
        1,
        "one receive envelope in flight while its ack is withheld"
    );
    for ack in held {
        ack.complete(Ok(()));
    }
    {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(async {
            while message_locals(&seen).len() < usize::try_from(COUNT).expect("count") {
                let envelope = tokio::time::timeout(Duration::from_secs(30), ends.events.recv())
                    .await
                    .expect("every message reached the core")
                    .expect("events ended");
                seen.extend(envelope.events);
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
        });
    }
    let locals = message_locals(&seen);
    let want: Vec<String> = (1..=COUNT).map(|k| (k << 20).to_string()).collect();
    assert_eq!(locals.len(), want.len(), "each message once");
    assert!(locals == want, "the messages in order, each once");
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

/// L-9: until T6 a `DownloadAttachment` is answered with a durable, RETRYABLE `AttachmentDone`
/// failure for that request (the core asks again later); nothing is downloaded.
#[test]
fn a_download_is_answered_retryable_until_t6() {
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    run.send(Command::DownloadAttachment {
        attachment: kourier_model::AttachmentId(7),
        request: kourier_model::DownloadId(70),
        remote: kourier_model::BackendBlob::new(1, b"{}".to_vec()),
        dest: kourier_backend_api::BlobTarget {
            path: run.dir.path().join("blobs").join("file"),
            key: kourier_model::BlobKey::new([0x42; 32]),
        },
        net_ids: Vec::new().into(),
    });
    let mut seen = Vec::new();
    let ack = run.until(&mut seen, |e| {
        matches!(e, BackendEvent::AttachmentDone { .. })
    });
    assert!(ack.is_some(), "the answer is durable");
    let answer = seen.iter().find_map(|e| match e {
        BackendEvent::AttachmentDone {
            attachment,
            request,
            result,
        } => Some((*attachment, *request, result.clone())),
        _ => None,
    });
    let Some((attachment, request, Err(error))) = answer else {
        panic!("no failed AttachmentDone");
    };
    assert_eq!(attachment, kourier_model::AttachmentId(7));
    assert_eq!(request, Some(kourier_model::DownloadId(70)));
    assert!(error.is_retryable(), "the core must ask again");
    assert!(matches!(error.kind, ErrorKind::Transient { .. }));
    if let Some(ack) = ack {
        ack.complete(Ok(()));
    }
    run.assert_nothing_downloads();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

/// G-8 (RC4, `rec-reconnect.jsonl`): after a short drop TDLib tells NO connection state; the
/// recorded updates of the reconnect (both queued messages) are rows at once, each message once,
/// with no sync request, and the account stays Online throughout. The recording holds no
/// recorded request: the test asserts that none was answered from it (Q-L11: 0 used). Its
/// closing authorization states (the recorder's end) are not pushed.
#[test]
fn a_short_drop_keeps_online_and_each_message_once() {
    let rec = crate::replay::Recording::load("rec-reconnect.jsonl");
    let part = rec.part("drop");
    let mut run = open(Setup::resume(ready_script(Vec::new()), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    // The start sync's own `loadChats` (C8) came before the drop.
    let sync_types = [
        "getChat",
        "getUser",
        "getMessage",
        "getChatHistory",
        "loadChats",
    ];
    let before: Vec<usize> = sync_types.iter().map(|ty| run.sent(ty)).collect();
    let mut pushed = 0;
    for u in part.updates() {
        assert_ne!(
            u["@type"], "updateConnectionState",
            "RC4 holds no connection state"
        );
        if u["@type"] == "updateAuthorizationState" {
            continue;
        }
        run.td.push_update(u.clone());
        pushed += 1;
    }
    assert!(pushed > 2, "the recording's updates were pushed");
    let recorded: Vec<i64> = part
        .updates()
        .filter(|u| u["@type"] == "updateNewMessage")
        .filter_map(|u| u["message"]["id"].as_i64())
        .collect();
    assert_eq!(recorded.len(), 2, "RC4 holds the two queued messages");
    let mut seen = Vec::new();
    {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(async {
            let by = tokio::time::Instant::now() + Duration::from_secs(10);
            while message_locals(&seen).len() < 2 {
                let envelope = tokio::time::timeout_at(by, ends.events.recv())
                    .await
                    .expect("the queued messages reached the core")
                    .expect("events ended");
                seen.extend(envelope.events);
                if let Some(ack) = envelope.ack {
                    ack.complete(Ok(()));
                }
            }
        });
    }
    let want: Vec<String> = recorded.iter().map(i64::to_string).collect();
    assert_eq!(message_locals(&seen), want, "each message once, in order");
    run.quiet_for(200, &mut seen);
    assert_eq!(message_locals(&seen).len(), 2, "no message twice");
    assert!(
        matches!(*run.ends.conn.borrow(), ConnState::Online { .. }),
        "the account left Online"
    );
    assert!(
        !run.ends.conn.has_changed().expect("the actor runs"),
        "a state was shown during the drop"
    );
    for (ty, before) in sync_types.iter().zip(before) {
        assert_eq!(run.sent(ty), before, "a sync request after the drop");
    }
    let requests: Vec<serde_json::Value> = run.td.requests().into_iter().map(|(_, r)| r).collect();
    assert_eq!(crate::replay::used([part], &requests), 0);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

// ---- TG-3 Q-D3: a clean stop drains TDLib --------------------------------------------------

/// `chat_state.delivered_max_id` of [`PEER`] and `run_state.clean_end`, read after the actor
/// ended.
fn stop_state(run: &Run) -> (Option<i64>, Option<i64>) {
    use kourier_db::rusqlite::OptionalExtension as _;
    let conn = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    let cursor = conn
        .query_row(
            "SELECT delivered_max_id FROM chat_state WHERE chat_id = ?1",
            [PEER],
            |r| r.get(0),
        )
        .optional()
        .expect("read chat_state");
    let clean = conn
        .query_row("SELECT clean_end FROM run_state WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()
        .expect("read run_state");
    (cursor, clean)
}

/// An Online run of a resumed account over `script` whose chat 77 delivered message 1 (in the
/// core, so committed) before the test stops it.
fn stop_after_one(script: Script) -> Run {
    stop_after_one_with(script, short().close_drain)
}

/// [`stop_after_one`] with the stop's wait for TDLib's `Closed` set to `drain`.
fn stop_after_one_with(script: Script, drain: Duration) -> Run {
    let mut setup = Setup::resume(script, OWN_ID);
    setup.timings.close_drain = drain;
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.online(&mut seen);
    for u in [peer_chat(), peer_user(), peer_message(1 << 20)] {
        run.td.push_update(u);
    }
    let first = (1_i64 << 20).to_string();
    let ack = run.until(
        &mut seen,
        |e| matches!(e, BackendEvent::Message(m) if *m.local == *first),
    );
    if let Some(ack) = ack {
        ack.complete(Ok(()));
    }
    run
}

/// Decision Q-D3: a clean stop sends TDLib `close` itself and reads what TDLib still hands out
/// (message 2, after `Closing` and before `Closed`) into the receive path; its row is in the
/// LAST commit, and that commit says the run ended cleanly (`Closed` was seen). Synthetic
/// (0 recorded answers).
#[test]
fn a_clean_stop_commits_what_tdlib_hands_out_while_closing() {
    let mut script = ready_script(Vec::new());
    script.close_updates = vec![peer_message(2 << 20)];
    // A bound far above the product's 3 s: a loaded machine must not turn this success into the
    // bound's case (TF8); the bound itself is the next test's.
    let mut run = stop_after_one_with(script, Duration::from_secs(60));
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
    assert!(run.sent("close") >= 1, "the stop sent no close");
    assert_eq!(
        stop_state(&run),
        (Some(2 << 20), Some(1)),
        "(cursor, clean_end) after the drain"
    );
}

/// Decision Q-D3: when TDLib's `Closed` never comes within `CLOSE_DRAIN`, the stop still writes
/// what it read, but the run stays UNCLEAN (`clean_end` 0: the next start's check gaps run).
#[test]
fn a_stop_whose_close_never_ends_stays_unclean() {
    let mut script = ready_script(Vec::new());
    script.close_updates = vec![peer_message(2 << 20)];
    script.hang_on_close = true;
    let mut run = stop_after_one(script);
    let t0 = StdInstant::now();
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
    assert!(
        t0.elapsed() >= crate::actor::CLOSE_DRAIN,
        "the stop did not wait for Closed"
    );
    assert_eq!(
        stop_state(&run),
        (Some(2 << 20), Some(0)),
        "(cursor, clean_end) after the bound"
    );
}

// ---- TG-3 fix round TF1: a failed receive commit is a helper failure ------------------------

/// Every message (live or history) in `seen`, by local id.
fn all_locals(seen: &[BackendEvent]) -> Vec<String> {
    let mut out = Vec::new();
    for e in seen {
        match e {
            BackendEvent::Message(m) => out.push(m.local.to_string()),
            BackendEvent::HistoryBatch { messages, .. } => {
                out.extend(messages.iter().map(|m| m.local.to_string()));
            }
            _ => {}
        }
    }
    out
}

/// Review dF1 / decision TF1: TDLib has no ack, so a receive commit that FAILS (here: a trigger
/// on `core_outbox` that refuses the insert, as a full disk would) must not leave the machines
/// ahead of the file. The helper run ends at once (`Retry::Abort`, the back-off); the next run
/// is seeded from the store, reads `Unclean`, and B1's check gap finds message 2 below the first
/// live message (3): the core holds 1, 2 and 3 once each. (Mutant: the old "log and continue":
/// the machines' cursor is 2, 3 commits, 2 is lost for good, and no restart is scheduled.)
#[test]
fn a_failed_receive_commit_restarts_the_helper_and_loses_nothing() {
    let rules = vec![
        Rule::when_all(
            "getChatHistory",
            &[
                ("/chat_id", json!(PEER)),
                ("/from_message_id", json!(3_i64 << 20)),
            ],
            vec![json!({"@type": "messages", "total_count": 2, "messages": [
                peer_message(2 << 20)["message"].clone(),
                peer_message(1 << 20)["message"].clone()]})],
        ),
        Rule::new(
            "getChatHistory",
            vec![json!({"@type": "messages", "total_count": 0, "messages": []})],
        ),
    ];
    let mut run = stop_after_one(ready_script(rules));
    // From here on the file refuses every receive commit.
    let side = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    side.execute_batch(
        "CREATE TRIGGER refuse_receive BEFORE INSERT ON core_outbox
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .expect("trigger");
    run.td.push_update(peer_message(2 << 20));
    let at = {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(next_retry(ends, T0))
    };
    side.execute_batch("DROP TRIGGER refuse_receive")
        .expect("drop");
    run.clock.set(at);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.online(&mut seen);
    assert_eq!(run.spawns.load(Ordering::SeqCst), 2, "no second helper");
    for u in [peer_chat(), peer_user(), peer_message(3 << 20)] {
        run.td.push_update(u);
    }
    let (two, three) = ((2_i64 << 20).to_string(), (3_i64 << 20).to_string());
    run.pump_seen_until("messages 2 and 3", &mut seen, |seen| {
        let all = all_locals(seen);
        all.contains(&two) && all.contains(&three)
    });
    run.quiet_for(300, &mut seen);
    let mut got = all_locals(&seen);
    got.sort();
    assert_eq!(got, [two, three], "each message once");
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
    assert_eq!(stop_state(&run).0, Some(3 << 20), "the cursor");
}

/// A trigger that refuses the `core_outbox` row holding message 2 of `rec-fresh-start.jsonl`'s
/// chat 201 (the start sync's last page), as a full disk would at that one write.
fn refuse_the_last_page(conn: &kourier_db::rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TRIGGER refuse_last_page BEFORE INSERT ON core_outbox
         WHEN instr(NEW.envelope, CAST('2097152' AS BLOB)) > 0
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .expect("trigger");
}

/// D1 of the re-check (TF1), extended by TG-4 K5: the commit of the Act::Synced arm is the one
/// that carries the start sync's LAST page, which is now chat 201's WHOLE fresh-link batch (K5:
/// its last message 4 and the unread rows 1, 2 and 3 are one `HistoryBatch`, one row, one
/// transaction). When the file refuses it, that is a helper failure like any other: the helper
/// run ends (the back-off), the account is not Online, the next run starts a second helper and
/// reaches Online after its own start sync, and a row the file took is not delivered twice.
/// (Mutant: the Synced arm ignores the failed commit: no restart is scheduled, the account goes
/// Online with the page dropped.) What D1 could NOT assert before the one-commit load, it does
/// now: the failed commit left chat 201 with NO cursor (its last message was committed WITH the
/// page, not before it), so the second run loads it fresh again and EVERY unread row of the
/// recording's fresh chat (1, 2, 3 and the last, 4) reaches the core exactly once. (Mutant:
/// today's early last message back: the file took message 4 first, the second run treats the chat
/// as delivered from, rows 1 to 3 are missing.)
#[test]
fn a_failed_commit_at_the_end_of_the_start_sync_restarts_the_helper() {
    let rec = crate::replay::Recording::load("rec-fresh-start.jsonl");
    // The recording's answers TWICE (one scripted TDLib serves both helper runs, its rules count
    // across them): the second run loads the chat fresh again.
    let rules = crate::replay::Recording::rules(rec.parts.iter().chain(rec.parts.iter()));
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.prepare = Some(refuse_the_last_page);
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.pump_until("the restart after the refused commit", &mut seen, |r| {
        matches!(
            *r.ends.conn.borrow(),
            ConnState::Offline {
                retry_at_ms: Some(_)
            }
        )
    });
    let at = match *run.ends.conn.borrow() {
        ConnState::Offline {
            retry_at_ms: Some(at),
        } => at,
        _ => unreachable!("waited for a retry time"),
    };
    assert!(
        !matches!(*run.ends.conn.borrow(), ConnState::Online { .. }),
        "Online after a refused commit"
    );
    let side = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    side.execute_batch("DROP TRIGGER refuse_last_page")
        .expect("drop");
    run.clock.set(at);
    run.td.push_update(conn_update("connectionStateReady"));
    run.pump_until("Online of the second run", &mut seen, |r| {
        matches!(*r.ends.conn.borrow(), ConnState::Online { .. })
    });
    assert_eq!(run.spawns.load(Ordering::SeqCst), 2, "no second helper");
    run.quiet_for(300, &mut seen);
    // Chat 201's rows (other chats of the recording hold a message 1 of their own).
    let chat = kourier_model::ConvKey::Direct(UserId::from_parts("tg", "201").expect("id"));
    let mut chat_201 = Vec::new();
    for e in &seen {
        match e {
            BackendEvent::Message(m) if m.conv == chat => chat_201.push(m.local.to_string()),
            BackendEvent::HistoryBatch { conv, messages, .. } if *conv == chat => {
                chat_201.extend(messages.iter().map(|m| m.local.to_string()));
            }
            _ => {}
        }
    }
    chat_201.sort();
    let id = |k: i64| (k << 20).to_string();
    assert_eq!(
        chat_201.iter().filter(|l| **l == id(4)).count(),
        1,
        "the row the file took is delivered once"
    );
    let mut once = chat_201.clone();
    once.dedup();
    assert_eq!(once, chat_201, "a row was delivered twice");
    let mut want = vec![id(1), id(2), id(3), id(4)];
    want.sort();
    assert_eq!(
        chat_201, want,
        "every unread row of the fresh chat and its last message reach the core once"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

/// D2 of the re-check (TF1): a Shutdown that comes during the back-off after a dropped batch
/// writes nothing of the receive machines that are ahead of the file: the stop's last commit
/// finds no machines, so the cursor stays at the last row the file took and the run stays
/// unclean (the file would take the write now: the trigger is gone). The session-level test
/// `a_dropped_receive_flushes_nothing_and_holds_nothing` pins `Session::drop_receive` itself.
#[test]
fn a_stop_during_the_backoff_of_a_dropped_batch_writes_nothing_more() {
    let mut run = stop_after_one(ready_script(Vec::new()));
    let side = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    side.execute_batch(
        "CREATE TRIGGER refuse_receive BEFORE INSERT ON core_outbox
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .expect("trigger");
    run.td.push_update(peer_message(2 << 20));
    {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(next_retry(ends, T0));
    }
    side.execute_batch("DROP TRIGGER refuse_receive")
        .expect("drop");
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    let (cursor, clean) = stop_state(&run);
    assert_eq!(cursor, Some(1 << 20), "the cursor moved to the dropped row");
    assert_ne!(clean, Some(1), "a stop in the back-off counted clean");
}

/// A `SyncReadMarker` of command `cmd` for the private chat `chat` up to message `local`.
fn read_marker_command(chat: i64, local: i64, cmd: i64) -> Command {
    let user = UserId::from_parts("tg", &chat.to_string()).expect("id");
    let conv = kourier_model::ConvKey::Direct(user.clone());
    let target = kourier_model::MsgTarget {
        conv: Some(conv.clone()),
        author: kourier_model::TargetAuthor::User(user),
        local: Arc::from(local.to_string()),
        alternates: Vec::new(),
    };
    Command::SyncReadMarker {
        cmd: kourier_model::CmdId(cmd),
        conv,
        up_to: target.clone(),
        read: vec![target],
    }
}

/// The commands the core was told are persisted, in `seen`.
fn persisted_in(seen: &[BackendEvent]) -> Vec<i64> {
    seen.iter()
        .filter_map(|e| match e {
            BackendEvent::CommandPersisted { cmd } => Some(cmd.0),
            _ => None,
        })
        .collect()
}

/// TF6 (review dF6): a `SyncReadMarker` that comes while no helper is live (the back-off after a
/// helper failure) is kept and served when the helper is back: one `viewMessages`, then
/// `CommandPersisted`. The core sends a pending command again only at the backend's start, not at
/// a helper restart. (Mutant: the old return, the marker is forgotten.)
#[test]
fn a_read_marker_during_the_backoff_is_served_when_the_helper_is_back() {
    let rules = vec![Rule::new("viewMessages", vec![ok()])];
    let mut run = stop_after_one(ready_script(rules));
    // A failed receive commit (TF1) sends the helper into its back-off.
    let side = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    side.execute_batch(
        "CREATE TRIGGER refuse_receive BEFORE INSERT ON core_outbox
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .expect("trigger");
    run.td.push_update(peer_message(2 << 20));
    let at = {
        let Run { rt, ends, .. } = &mut run;
        rt.block_on(next_retry(ends, T0))
    };
    side.execute_batch("DROP TRIGGER refuse_receive")
        .expect("drop");
    run.send(read_marker_command(PEER, 1 << 20, 5));
    let mut seen = Vec::new();
    run.quiet_for(200, &mut seen);
    assert_eq!(run.sent("viewMessages"), 0, "sent without a helper");
    assert!(persisted_in(&seen).is_empty(), "confirmed without a helper");
    run.clock.set(at);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut seen);
    run.pump_seen_until("the marker's confirmation", &mut seen, |seen| {
        persisted_in(seen) == [5]
    });
    assert_eq!(run.sent("viewMessages"), 1, "one viewMessages");
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

/// TF7 (review pF1, "ghost mode"): TDLib refuses the read act's `viewMessages` (400). The peer
/// was not told, so the command is NOT confirmed: no `CommandPersisted` (the row stays pending in
/// the core and is sent again at the next start). (Mutant: any answer confirms.)
#[test]
fn a_refused_view_page_leaves_the_marker_pending() {
    let rules = vec![Rule::new(
        "viewMessages",
        vec![td_error(400, "Message not found")],
    )];
    let mut run = stop_after_one(ready_script(rules));
    run.send(read_marker_command(PEER, 1 << 20, 6));
    let mut seen = Vec::new();
    run.pump_until("viewMessages", &mut seen, |r| r.sent("viewMessages") > 0);
    run.quiet_for(300, &mut seen);
    assert_eq!(run.sent("viewMessages"), 1);
    assert!(
        persisted_in(&seen).is_empty(),
        "a refused read act was confirmed"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

/// 300 old, read rows of chat 77 in `delivered` (and the chat's state: all read).
fn old_read_rows(conn: &kourier_db::rusqlite::Connection) {
    conn.execute(
        "INSERT INTO chat_state(chat_id, kind, delivered_max_id, verified_max_id, low_id,
                                read_inbox_max_id, updated_ms)
         VALUES (77, 'direct', ?1, ?1, ?2, ?1, 1000)",
        [300_i64 << 20, 1_i64 << 20],
    )
    .expect("chat_state");
    let mut insert = conn
        .prepare("INSERT INTO delivered(chat_id, msg_id, author, outgoing, at_ms) VALUES (77, ?1, 'tg:77', 0, 1000)")
        .expect("prepare");
    for n in 1..=300_i64 {
        insert.execute([n << 20]).expect("delivered");
    }
}

fn delivered_count(run: &Run) -> i64 {
    let conn = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    conn.query_row(
        "SELECT COUNT(*) FROM delivered WHERE chat_id = 77",
        [],
        |r| r.get(0),
    )
    .expect("count")
}

/// TF5 (review dF5): `store::sweep` runs once per helper run, after the start sync's barrier:
/// 300 old read rows of a chat are 200 once the account is Online (rule L-5: the newest 200
/// stay). (Mutant: no sweep call.)
#[test]
fn the_delivered_table_is_swept_after_the_start_sync() {
    let mut setup = Setup::resume(ready_script(Vec::new()), OWN_ID);
    setup.prepare = Some(old_read_rows);
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    run.online(&mut Vec::new());
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(delivered_count(&run), 200, "rows left after the sweep");
}

/// TF3(c): a message of a chat TDLib never named waits for its `getChat`, whose answer is held.
/// A clean stop reads `Closed`, but a waiting list hides what it holds from the next start's
/// checks, so the run is NOT clean (`clean_end` 0). (Mutant: clean whatever waits.)
#[test]
fn a_stop_with_a_message_waiting_for_its_chat_stays_unclean() {
    let script = ready_script(vec![Rule::deferred(
        "getChat",
        vec![json!({"@type": "ok"})],
    )]);
    let mut run = stop_after_one(script);
    let mut stranger = peer_message(2 << 20);
    stranger["message"]["chat_id"] = json!(88);
    stranger["message"]["sender_id"]["user_id"] = json!(88);
    run.td.push_update(stranger);
    run.wait("the getChat", || run.sent("getChat") >= 1);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert!(run.sent("close") >= 1, "the stop sent no close");
    assert_eq!(
        stop_state(&run),
        (Some(1 << 20), Some(0)),
        "(cursor, clean_end) with a message waiting"
    );
}

/// TG-3 C8 through the actor (ScriptedTd answering from `rec-fresh-start.jsonl`): a resumed
/// account with an empty backend.db pages `loadChats` main then archive until each 404, one page
/// per second on the actor's clock (advanced here, no wall-time pace), then the fresh-link load:
/// the last messages as history rows and chat 201's ONE unread page (from its last id, 3); Online
/// only after all of it is acked. Recorded answers used: the 6 list pages and the page (Q-L11).
#[test]
fn a_fresh_start_loads_both_lists_and_the_unread_page_before_online() {
    let rec = crate::replay::Recording::load("rec-fresh-start.jsonl");
    let rules = crate::replay::Recording::rules(rec.parts.iter());
    let mut run = open(Setup::resume(ready_script(rules), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    {
        let Run {
            rt, ends, clock, ..
        } = &mut run;
        rt.block_on(async {
            let by = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                if matches!(*ends.conn.borrow_and_update(), ConnState::Online { .. }) {
                    return;
                }
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let envelope = envelope.expect("events ended");
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                    }
                    changed = ends.conn.changed() => changed.expect("the actor ended"),
                    // Idle: the sync lane waits for its pace on the fake clock.
                    () = tokio::time::sleep(Duration::from_millis(20)) => {
                        clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
                    }
                    () = tokio::time::sleep_until(by) => panic!("timed out waiting for Online"),
                }
            }
        });
    }
    let lists: Vec<String> = run
        .requests_of("loadChats")
        .iter()
        .map(|r| r["chat_list"]["@type"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(
        lists,
        [
            "chatListMain",
            "chatListMain",
            "chatListMain",
            "chatListArchive",
            "chatListArchive",
            "chatListArchive"
        ]
    );
    let pages: Vec<(i64, i64, i64)> = run
        .requests_of("getChatHistory")
        .iter()
        .map(|r| {
            (
                r["chat_id"].as_i64().unwrap_or(0),
                r["from_message_id"].as_i64().unwrap_or(0) >> 20,
                r["limit"].as_i64().unwrap_or(0),
            )
        })
        .collect();
    // The 2 unread messages below the last one (U5: the recording's 3 unread count the last one;
    // the recorded answer, replayed by chat and `from`, still holds the recorded read message 1).
    assert_eq!(pages, [(201, 4, 2)]);
    let requests: Vec<serde_json::Value> = run.td.requests().into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        crate::replay::used(&rec.parts, &requests),
        7,
        "recorded answers used"
    );
    let batches: Vec<Vec<String>> = seen
        .iter()
        .filter_map(|e| match e {
            BackendEvent::HistoryBatch { conv, messages, .. }
                if *conv
                    == kourier_model::ConvKey::Direct(
                        UserId::from_parts("tg", "201").expect("id"),
                    ) =>
            {
                Some(messages.iter().map(|m| m.local.to_string()).collect())
            }
            _ => None,
        })
        .collect();
    let id = |k: i64| (k << 20).to_string();
    // ONE batch (TG-4 K5): the page's rows and the last message, in id order, never the last
    // message first.
    assert_eq!(batches, [vec![id(1), id(2), id(3), id(4)]]);
    assert!(
        !seen.iter().any(|e| matches!(e, BackendEvent::Message(_))),
        "a fresh-link row was a live message"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();
}

// ---- TG-4 fix round U1: a lost page answer through the actor ------------------------------------

/// `chat_state.delivered_max_id` of chat `chat_id` (server ids), read while the actor runs (WAL):
/// 0 where the chat has no row or its cursor is 0.
fn stored_cursor(run: &Run, chat_id: i64) -> i64 {
    let conn = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    kourier_db::rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT delivered_max_id FROM chat_state WHERE chat_id = ?1",
        [chat_id],
        |r| r.get::<_, i64>(0),
    ))
    .expect("read chat_state")
    .map_or(0, |max| max >> 20)
}

/// U1 (durability F1) through the actor and the store: the fresh page of chat 201 (the recorded
/// start) is held past the scripted 5 s `request_timeout`, so the link answers it with
/// `Err(Timeout)`. Nothing of the chat may commit (its cursor stays 0, no batch reaches the
/// core), and the SAME page is asked again (the second request carries the same chat, offset and
/// limit; it is held too, so that nothing can commit while the test looks). Released, the page's
/// answer completes the plan in one batch. (Mutant: the old `Done::Fetch` for a lost fetch: the
/// timeout completes the plan with the last message alone, the cursor moves, no second page is
/// asked.)
#[test]
fn a_fresh_page_that_times_out_commits_nothing_and_is_asked_again() {
    let rec = crate::replay::Recording::load("rec-fresh-start.jsonl");
    let mut rules = crate::replay::Recording::rules(rec.parts.iter());
    // The recorded page of chat 201 from its last message (4), the one this start asks.
    let has = |r: &Rule, pointer: &str, value: serde_json::Value| {
        r.when
            .iter()
            .chain(&r.also)
            .any(|(p, v)| p == pointer && *v == value)
    };
    let at = rules
        .iter()
        .position(|r| {
            r.on == "getChatHistory"
                && has(r, "/chat_id", json!(201))
                && has(r, "/from_message_id", json!(4 << 20))
        })
        .expect("the recorded page");
    let page = rules.remove(at);
    // Request 1 is held for good (its reply would come after the timeout); request 2 holds the
    // recorded page until the test releases it.
    let held_first = Rule {
        nth: Some(1),
        deferred: true,
        reply: vec![ok()],
        ..page.clone()
    };
    let held_again = Rule {
        nth: Some(2),
        deferred: true,
        ..page
    };
    let mut all = vec![held_first, held_again];
    all.extend(rules);
    let mut run = open(Setup::resume(ready_script(all), OWN_ID));
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    let td = run.td.clone();
    let asked = |n: usize| {
        let td = td.clone();
        move |_: &[BackendEvent]| {
            td.sent_types()
                .iter()
                .filter(|t| *t == "getChatHistory")
                .count()
                >= n
        }
    };
    run.pump_seen_until("the first page", &mut seen, asked(1));
    run.pump_seen_until("the page asked again", &mut seen, asked(2));
    let pages = run.requests_of("getChatHistory");
    assert_eq!(pages.len(), 2);
    assert_eq!(
        (
            &pages[0]["chat_id"],
            &pages[0]["from_message_id"],
            &pages[0]["limit"]
        ),
        (
            &pages[1]["chat_id"],
            &pages[1]["from_message_id"],
            &pages[1]["limit"]
        ),
        "another page was asked"
    );
    assert_eq!(pages[0]["chat_id"], 201);
    let direct_201 = kourier_model::ConvKey::Direct(UserId::from_parts("tg", "201").expect("id"));
    let batch_of_201 = |seen: &[BackendEvent]| -> Vec<Vec<String>> {
        seen.iter()
            .filter_map(|e| match e {
                BackendEvent::HistoryBatch { conv, messages, .. } if *conv == direct_201 => {
                    Some(messages.iter().map(|m| m.local.to_string()).collect())
                }
                _ => None,
            })
            .collect()
    };
    assert!(
        batch_of_201(&seen).is_empty(),
        "a lost page answer committed the chat"
    );
    assert_eq!(
        stored_cursor(&run, 201),
        0,
        "a lost page answer moved the chat's cursor"
    );
    assert!(td.held() >= 1, "the second page was not held");
    td.release();
    run.online(&mut seen);
    let id = |k: i64| (k << 20).to_string();
    assert_eq!(
        batch_of_201(&seen),
        [vec![id(1), id(2), id(3), id(4)]],
        "one batch: the page's rows and the last message"
    );
    assert_eq!(run.requests_of("getChatHistory").len(), 2);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(stored_cursor(&run, 201), 4);
    run.assert_t3_requests();
}

// ---- TG-4 K4: the read act's confirm stores the position; the read re-assert --------------------

/// Chat 77 as a store left it after a run that delivered messages 1-3 (incoming) and 4 (our own,
/// outgoing) and read up to 1.
fn read_up_to_one_with_an_own_message(conn: &kourier_db::rusqlite::Connection) {
    conn.execute(
        "INSERT INTO chat_state(chat_id, kind, delivered_max_id, verified_max_id, low_id,
                                read_inbox_max_id, updated_ms)
         VALUES (77, 'direct', ?1, ?1, ?2, ?2, 1000)",
        [4_i64 << 20, 1_i64 << 20],
    )
    .expect("chat_state");
    for (n, outgoing) in [(1_i64, 0), (2, 0), (3, 0), (4, 1)] {
        conn.execute(
            "INSERT INTO delivered(chat_id, msg_id, author, outgoing, at_ms) VALUES (77, ?1, ?2, ?3, 1000)",
            (n << 20, if outgoing == 1 { "tg:100" } else { "tg:77" }, outgoing),
        )
        .expect("delivered");
    }
}

/// `chat_state.read_inbox_max_id` of chat 77, read after the actor ended.
fn stored_read_position(run: &Run) -> i64 {
    let conn = db::open(&run.backend_db, &DbKey::from_bytes(KEY)).expect("open");
    conn.query_row(
        "SELECT read_inbox_max_id FROM chat_state WHERE chat_id = 77",
        [],
        |r| r.get(0),
    )
    .expect("the chat's row")
}

/// TG-4 K4 (D-2, critic M3, M4), through the actor and the store: TDLib's `ok` of a read act
/// raises `chat_state.read_inbox_max_id` to the act's highest INCOMING id in the commit of the
/// command's `CommandPersisted` (the scripted TDLib sends no `updateChatReadInbox`: the confirm
/// is the only source, the postponed-echo path). The marker's own target is our message 4: the
/// request holds only the incoming ids 1-3 (U2) and the position stored is 3. (Mutants: the fact
/// left out of the confirm -> 1 stays; the own id back in the request -> the request names 4.)
#[test]
fn a_confirmed_read_raises_the_stored_read_position_in_its_commit() {
    let rules = vec![Rule::new("viewMessages", vec![ok()])];
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.prepare = Some(read_up_to_one_with_an_own_message);
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.online(&mut seen);
    run.send(read_marker_command(PEER, 4 << 20, 5));
    run.pump_seen_until("the marker's confirmation", &mut seen, |seen| {
        persisted_in(seen) == [5]
    });
    let views = run.requests_of("viewMessages");
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0]["message_ids"],
        json!([1 << 20, 2 << 20, 3 << 20]),
        "the act's pages hold every INCOMING delivered id up to the marker, sorted (U2: the own \
         message 4 is not told)"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(
        stored_read_position(&run),
        3 << 20,
        "the highest incoming id of the act"
    );
    run.assert_t3_requests();
}

/// The lists of a resumed run that tells chat 77 (its user, its read position at `told`; the main
/// list ends 404, the archive ends 404).
fn peer_lists(told: i64) -> Vec<Rule> {
    let mut chat = peer_chat();
    chat["chat"]["last_read_inbox_message_id"] = json!(told);
    vec![
        Rule::nth(
            "loadChats",
            1,
            vec![peer_user(), chat, td_error(404, "Not Found")],
        ),
        Rule::nth("loadChats", 2, vec![td_error(404, "Not Found")]),
    ]
}

/// The account shows Online (a [`Run::pump_until`] condition).
fn is_online(run: &Run) -> bool {
    matches!(*run.ends.conn.borrow(), ConnState::Online { .. })
}

/// `updateChatReadInbox` of chat 77 at `id`, as TDLib echoes a read.
fn read_echo(id: i64) -> serde_json::Value {
    json!({"@type": "updateChatReadInbox", "chat_id": PEER, "last_read_inbox_message_id": id,
        "unread_count": 0})
}

/// U2 (honesty HIGH-1), through the actor and the store, in TDLib's NORMAL order: the echo
/// `updateChatReadInbox(max of the request's ids)` comes BEFORE the `ok` (critic M3), so the
/// position the receive path stores is whatever the request's newest id is. With the own message 4
/// not in the request (only the incoming ids 1-3 go), the echo is 3 and the column holds the
/// INCOMING id 3 (the scripted TDLib would echo 4 for a request that held the own message: TDLib
/// takes the maximum with no test of the direction). A restart whose TDLib tells the highest
/// incoming id (3) then sends NO re-assert and the column stays 3. (Mutants: the own id back in the
/// request -> the request names 4 and the column holds 4; the compare against the raw stored
/// position -> see the machine's test.)
#[test]
fn the_echo_of_a_read_act_stores_the_incoming_id_and_the_next_start_asks_nothing() {
    let ids = |ids: &[i64]| json!(ids.iter().map(|id| id << 20).collect::<Vec<i64>>());
    let mut rules = peer_lists(1 << 20);
    rules.push(Rule::when(
        "viewMessages",
        "/message_ids",
        ids(&[1, 2, 3]),
        vec![read_echo(3 << 20), ok()],
    ));
    rules.push(Rule::when(
        "viewMessages",
        "/message_ids",
        ids(&[1, 2, 3, 4]),
        vec![read_echo(4 << 20), ok()],
    ));
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.prepare = Some(read_up_to_one_with_an_own_message);
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.pump_until("Online", &mut seen, is_online);
    run.send(read_marker_command(PEER, 4 << 20, 5));
    run.pump_seen_until("the marker's confirmation", &mut seen, |seen| {
        persisted_in(seen) == [5]
    });
    let views = run.requests_of("viewMessages");
    assert_eq!(views.len(), 1);
    assert_eq!(
        views[0]["message_ids"],
        ids(&[1, 2, 3]),
        "an own message was told"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(
        stored_read_position(&run),
        3 << 20,
        "the column holds the echo's id: the highest incoming one"
    );
    run.assert_t3_requests();

    // The next start over the same store: TDLib tells the highest incoming id.
    let dir = run
        .backend_db
        .parent()
        .expect("the account dir")
        .to_path_buf();
    let mut again = Setup {
        at: Some(dir),
        start: StartMode::Resume,
        ..Setup::login(ready_script(peer_lists(3 << 20)))
    };
    again
        .script
        .rules
        .push(Rule::new("viewMessages", vec![ok()]));
    let mut second = open(again);
    second.send(Command::Connect);
    second.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    second.pump_until("Online", &mut seen, is_online);
    second.quiet_for(300, &mut seen);
    assert_eq!(second.sent("viewMessages"), 0, "a re-assert went out");
    assert_eq!(second.end_with(Command::Shutdown), Exit::Shutdown);
    assert_eq!(stored_read_position(&second), 3 << 20);
}

/// Chat 77 read up to 5 in the store (message 5 delivered), as a run that confirmed a read of 5
/// and was killed before TDLib's request left.
fn read_up_to_five(conn: &kourier_db::rusqlite::Connection) {
    conn.execute(
        "INSERT INTO chat_state(chat_id, kind, delivered_max_id, verified_max_id, low_id,
                                read_inbox_max_id, updated_ms)
         VALUES (77, 'direct', ?1, ?1, ?1, ?1, 1000)",
        [5_i64 << 20],
    )
    .expect("chat_state");
    conn.execute(
        "INSERT INTO delivered(chat_id, msg_id, author, outgoing, at_ms) VALUES (77, ?1, 'tg:77', 0, 1000)",
        [5_i64 << 20],
    )
    .expect("delivered");
}

/// A resumed run whose lists TDLib loads in two answers (the main list tells chat 77 and its
/// user, with its read position at `told`, and ends 404; the archive ends 404) and whose
/// `viewMessages` is answered `ok`; pumped until Online.
fn lost_read_run(told: i64) -> (Run, Vec<BackendEvent>) {
    let mut chat = peer_chat();
    chat["chat"]["last_read_inbox_message_id"] = json!(told);
    let rules = vec![
        Rule::nth(
            "loadChats",
            1,
            vec![peer_user(), chat, td_error(404, "Not Found")],
        ),
        Rule::nth("loadChats", 2, vec![td_error(404, "Not Found")]),
        Rule::new("viewMessages", vec![ok()]),
    ];
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.prepare = Some(read_up_to_five);
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.pump_until("Online", &mut seen, |r| {
        matches!(*r.ends.conn.borrow(), ConnState::Online { .. })
    });
    (run, seen)
}

/// TG-4 K4 (D-2), through the actor: the store says the person read chat 77 up to message 5, TDLib
/// tells it read only up to 3 in this run (the read was lost to a kill: with the message database
/// off nothing replays it): at the lists' load ONE `viewMessages(77, [5], force_read)` goes
/// ahead, and a second tell in the run (a lower one) sends no second. Told AT 5 (TDLib and the
/// store agree): no request at all. (Mutants: `>` -> `>=` sends at equality; the comparison
/// against the seeded `told_in` sends none; the once-per-run set not filled.) Synthetic (0
/// recorded answers; the recorded restart is `the_recorded_restart_gets_the_lost_read_again`).
#[test]
fn a_read_tdlib_lost_is_sent_again_once_at_the_next_start() {
    let (mut run, mut seen) = lost_read_run(3 << 20);
    run.pump_until("the re-assert", &mut seen, |r| r.sent("viewMessages") >= 1);
    let views = run.requests_of("viewMessages");
    assert_eq!(views.len(), 1, "one re-assert");
    assert_eq!(views[0]["chat_id"], PEER);
    assert_eq!(views[0]["message_ids"], json!([5 << 20]));
    assert_eq!(views[0]["force_read"], true);
    assert!(views[0]["source"].is_null());
    // A second tell in the run, below the stored position, sends nothing more.
    run.td
        .push_update(json!({"@type": "updateChatReadInbox", "chat_id": PEER,
        "last_read_inbox_message_id": 2 << 20, "unread_count": 3}));
    run.quiet_for(300, &mut seen);
    assert_eq!(run.sent("viewMessages"), 1, "a second tell sent it again");
    assert!(
        persisted_in(&seen).is_empty(),
        "a re-assert confirmed a command"
    );
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    run.assert_t3_requests();

    // TDLib and the store agree: nothing to send.
    let (mut run, mut seen) = lost_read_run(5 << 20);
    run.quiet_for(300, &mut seen);
    assert_eq!(run.sent("viewMessages"), 0, "a re-assert at equality");
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
}

// ---- TG-3 C11: what the receive path logs and asks ------------------------------------------

/// Every request type T3's receive path makes (refresh C11; Q-B1's `getUser`, Q-D3's `close`).
const T3_REQUESTS: [&str; 9] = [
    "loadChats",
    "getChatHistory",
    "getMessage",
    "getChat",
    "getUser",
    "getSupergroup",
    "getBasicGroup",
    "viewMessages",
    "close",
];

/// Every request type of TG-2 (the helper's own start and lock-down, the login, the session's
/// presence, hints, watchdog and logout) a scripted run may show.
const TG2_REQUESTS: [&str; 22] = [
    "setTdlibParameters",
    "getAuthorizationState",
    "getOption",
    "setOption",
    "addProxy",
    "getProxies",
    "removeProxy",
    "setNetworkType",
    "testCallEmpty",
    "getMe",
    "setAuthenticationPhoneNumber",
    "resendAuthenticationCode",
    "checkAuthenticationCode",
    "checkAuthenticationPassword",
    "requestQrCodeAuthentication",
    "setAuthenticationEmailAddress",
    "checkAuthenticationEmailCode",
    "logOut",
    "destroy",
    "close",
    "setLogStream",
    "setLogVerbosityLevel",
];

impl Run {
    /// Pumps the core's end (acking every durable envelope; the fake clock moves one lane pace per
    /// idle 20 ms) until `done` holds (20 s at most).
    fn pump_until(
        &mut self,
        what: &str,
        seen: &mut Vec<BackendEvent>,
        done: impl Fn(&Self) -> bool,
    ) {
        let by = StdInstant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(StdInstant::now() < by, "timed out waiting for {what}");
            let Self {
                rt, ends, clock, ..
            } = self;
            rt.block_on(async {
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let envelope = envelope.expect("events ended");
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(20)) => {
                        clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
                    }
                }
            });
        }
    }

    /// [`Self::pump_until`] whose condition sees the events collected so far.
    fn pump_seen_until(
        &mut self,
        what: &str,
        seen: &mut Vec<BackendEvent>,
        done: impl Fn(&[BackendEvent]) -> bool,
    ) {
        let by = StdInstant::now() + Duration::from_secs(20);
        while !done(seen) {
            assert!(StdInstant::now() < by, "timed out waiting for {what}");
            let Self {
                rt, ends, clock, ..
            } = self;
            rt.block_on(async {
                tokio::select! {
                    envelope = ends.events.recv() => {
                        let envelope = envelope.expect("events ended");
                        seen.extend(envelope.events);
                        if let Some(ack) = envelope.ack {
                            ack.complete(Ok(()));
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(20)) => {
                        clock.advance(Duration::from_millis(crate::session::lane::FETCH_PACE_MS));
                    }
                }
            });
        }
    }

    /// No request type outside T3's and TG-2's reached TDLib (the helper's own included).
    fn assert_t3_requests(&self) {
        let other: Vec<String> = self
            .td
            .sent_types()
            .into_iter()
            .filter(|t| !T3_REQUESTS.contains(&t.as_str()) && !TG2_REQUESTS.contains(&t.as_str()))
            .collect();
        assert!(other.is_empty(), "requests T3 never makes: {other:?}");
    }
}

/// The synthetic content kinds of `testdata/receive-contents.jsonl` a debug line could print (a
/// captioned photo, a document, a venue, a shared contact, a poll), re-pointed to the peer chat:
/// the updates and their locals (TF12).
fn peer_contents() -> Vec<(serde_json::Value, String)> {
    let lines: Vec<&str> = include_str!("../../../testdata/receive-contents.jsonl")
        .lines()
        .collect();
    [1_usize, 4, 12, 13, 15]
        .iter()
        .zip(3_i64..)
        .map(|(&line, n)| {
            let mut u: serde_json::Value = serde_json::from_str(lines[line - 1]).expect("a line");
            let id = n << 20;
            u["message"]["id"] = json!(id);
            u["message"]["chat_id"] = json!(PEER);
            u["message"]["sender_id"]["user_id"] = json!(PEER);
            (u, id.to_string())
        })
        .collect()
}

/// The peer's user with a public username (synthetic): a name that must reach no log line.
fn peer_username() -> serde_json::Value {
    json!({"@type": "updateUser", "user": {"@type": "user", "id": PEER, "first_name": "Bo",
        "last_name": "Peer", "phone_number": "", "is_contact": true,
        "usernames": {"@type": "usernames", "active_usernames": ["synthetic_9"],
            "disabled_usernames": [], "editable_username": "synthetic_9"},
        "type": {"@type": "userTypeRegular"}}})
}

/// A receive run over the recording `rec-fresh-start.jsonl` (its answers as rules) plus
/// synthetic live updates: the fresh link's load, a private chat's message, its edit, a message
/// of a chat TDLib never named (`getChat`), a read marker (`viewMessages`), then a clean stop
/// (`close`). `lines` captures the log at every level. The recorded answers used: 7 (Q-L11).
fn receive_run(lines: Option<Lines>) -> (Run, Vec<BackendEvent>, crate::replay::Recording) {
    let rec = crate::replay::Recording::load("rec-fresh-start.jsonl");
    let rules = crate::replay::Recording::rules(rec.parts.iter());
    let mut setup = Setup::resume(ready_script(rules), OWN_ID);
    setup.lines = lines;
    let mut run = open(setup);
    run.send(Command::Connect);
    run.td.push_update(conn_update("connectionStateReady"));
    let mut seen = Vec::new();
    run.pump_until("Online", &mut seen, |r| {
        matches!(*r.ends.conn.borrow(), ConnState::Online { .. })
    });
    let first = 1_i64 << 20;
    let edited = json!({"@type": "updateMessageContent", "chat_id": PEER, "message_id": first,
        "new_content": {"@type": "messageText", "text": {"@type": "formattedText",
            "text": "synthetic text 952", "entities": []}}});
    let edit = json!({"@type": "updateMessageEdited", "chat_id": PEER, "message_id": first,
        "edit_date": 1_700_000_200, "reply_markup": null});
    let mut stranger = peer_message(2 << 20);
    stranger["message"]["chat_id"] = json!(88);
    let mut pushed = vec![
        peer_chat(),
        peer_user(),
        peer_message(first),
        edited,
        edit,
        stranger,
    ];
    // TF12: the content kinds a debug line could print (a captioned photo, a document with a file
    // name, a venue, a shared contact, a poll) and the peer's public username.
    let kinds = peer_contents();
    pushed.extend(kinds.iter().map(|(u, _)| u.clone()));
    pushed.push(peer_username());
    for u in pushed {
        run.td.push_update(u);
    }
    let local = first.to_string();
    // Whatever the run asks about chat 88 (T3: `getChat`; the request set is checked after).
    run.pump_until("a request about chat 88", &mut seen, |r| {
        r.td.requests().iter().any(|(_, q)| q["chat_id"] == 88)
    });
    let shown = |seen: &[BackendEvent]| {
        seen.iter()
            .any(|e| matches!(e, BackendEvent::Message(m) if *m.local == *local))
    };
    let by = StdInstant::now() + Duration::from_secs(20);
    while !shown(&seen) {
        assert!(StdInstant::now() < by, "the live message never came");
        run.quiet_for(20, &mut seen);
    }
    for (_, local) in &kinds {
        let by = StdInstant::now() + Duration::from_secs(20);
        while !message_locals(&seen).contains(local) {
            assert!(StdInstant::now() < by, "a content message never came");
            run.quiet_for(20, &mut seen);
        }
    }
    let conv = kourier_model::ConvKey::Direct(UserId::from_parts("tg", "77").expect("id"));
    let target = kourier_model::MsgTarget {
        conv: Some(conv.clone()),
        author: kourier_model::TargetAuthor::User(UserId::from_parts("tg", "77").expect("id")),
        local: Arc::from(first.to_string()),
        alternates: Vec::new(),
    };
    run.send(Command::SyncReadMarker {
        cmd: kourier_model::CmdId(1),
        conv,
        up_to: target.clone(),
        read: vec![target],
    });
    run.pump_until("viewMessages", &mut seen, |r| r.sent("viewMessages") > 0);
    assert_eq!(run.end_with(Command::Shutdown), Exit::Shutdown);
    (run, seen, rec)
}

/// Every string a recording's objects hold under a key that names a person, a chat or a text.
fn private_strings(rec: &crate::replay::Recording) -> Vec<String> {
    const KEYS: [&str; 9] = [
        "title",
        "first_name",
        "last_name",
        "text",
        "phone_number",
        "username",
        "editable_username",
        "active_usernames",
        "bio",
    ];
    fn walk(v: &serde_json::Value, under: bool, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) if under => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, under, out)),
            serde_json::Value::Object(o) => {
                for (k, x) in o {
                    walk(x, under || KEYS.contains(&k.as_str()), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for part in &rec.parts {
        for u in part.updates() {
            walk(u, false, &mut out);
        }
        for r in part.requests() {
            for reply in &r.reply {
                walk(reply, false, &mut out);
            }
        }
    }
    // Short words ("Ada") are kept (TF12): [`shows`] matches them as whole words.
    out.retain(|s| s.chars().count() >= 2);
    out.sort();
    out.dedup();
    out
}

/// Every string of the synthetic content updates and the username update the receive run adds,
/// but the `@type`s (TF12): captions, file names, names, titles, addresses, a poll's question and
/// options, the username.
fn content_strings() -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            serde_json::Value::Object(o) => {
                o.iter()
                    .filter(|(k, _)| *k != "@type")
                    .for_each(|(_, x)| walk(x, out));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (u, _) in peer_contents() {
        walk(&u, &mut out);
    }
    walk(&peer_username(), &mut out);
    out.retain(|s| s.chars().count() >= 3);
    out.sort();
    out.dedup();
    out
}

/// Whether a log `line` shows `value`: a long value anywhere in it, a short one (a first name like
/// "Bo") only as a whole word, so ordinary log text does not match it (TF12).
fn shows(line: &str, value: &str) -> bool {
    if value.chars().count() >= 5 {
        return line.contains(value);
    }
    let word = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(value).any(|(at, _)| {
        !line[..at].chars().next_back().is_some_and(word)
            && !line[at + value.len()..].chars().next().is_some_and(word)
    })
}

/// Refresh C11 (architecture §9.1, the lane's log rule): a whole receive run (the fresh link's
/// load, live messages, an edit, an unknown chat, a read marker, the clean stop) captured at
/// `trace` on every thread the actor starts: no chat title, message text, person's name, number
/// or username of the recording or of the live updates reaches a line. The assertion names no
/// value. Recorded answers used: 7 (Q-L11).
#[test]
fn no_title_text_name_or_number_reaches_a_log_line() {
    let lines = Lines::default();
    let (run, _, rec) = receive_run(Some(lines.clone()));
    let all = lines.all();
    assert!(
        all.iter().any(|l| l.contains("files directory")),
        "the capture saw none of the actor's lines, so it proves nothing"
    );
    let mut values = private_strings(&rec);
    assert!(values.len() > 5, "the recording held no private string");
    // The own account's name and its parts (the account is not in this recording), the live messages' texts, and
    // the content kinds and the username the run adds (TF12).
    values.extend(
        [
            "Bo Peer",
            "Ada Test",
            "Ada",
            "Test",
            "synthetic text 951",
            "synthetic text 952",
        ]
        .iter()
        .map(|s| (*s).to_owned()),
    );
    values.extend(content_strings());
    for short in ["Bo", "Cy", "Di", "Ada", "Peer", "Test"] {
        assert!(
            values.iter().any(|v| v == short),
            "the short value {short} is not searched"
        );
    }
    let leaked = values
        .iter()
        .filter(|v| all.iter().any(|l| shows(l, v.as_str())))
        .count();
    assert_eq!(
        leaked, 0,
        "values of a chat, a person or a text reached a log line"
    );
    let requests: Vec<serde_json::Value> = run.td.requests().into_iter().map(|(_, r)| r).collect();
    assert_eq!(
        crate::replay::used(&rec.parts, &requests),
        7,
        "recorded answers used"
    );
}

/// Refresh C11 (`plan.md` T3's list, brief §2.4): the receive run asks TDLib only T3's requests
/// (`loadChats`, `getChatHistory`, `getMessage`, `getChat`, `getUser`, `getSupergroup`,
/// `getBasicGroup`, `viewMessages`, `close`) besides TG-2's, never `openChat`, a download or sponsored messages; the receive flow tests of
/// this file check the same over their own runs.
#[test]
fn t3_sends_only_its_requests() {
    let (run, _, _) = receive_run(None);
    run.assert_t3_requests();
    for ty in [
        "loadChats",
        "getChatHistory",
        "getChat",
        "viewMessages",
        "close",
    ] {
        assert!(run.sent(ty) > 0, "the run never sent {ty}: it proves less");
    }
    run.assert_nothing_forbidden();
}

// ---- TG-3 C10a: the kill harness (a child module: it reuses `open` and `Run`) ---------------

mod kill;
