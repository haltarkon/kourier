//! W2c: the PRE-PAIR REFUSAL over the scripted server. A pairing is judged by the stanza claim
//! (`src/prepair.rs`) BEFORE the device exists: upstream's `<pair-success>` handler writes the other
//! account's ids into the store and signs the pairing (`pair-device-sign`), which is what makes the
//! phone list "Kourier". A claimed pairing never reaches that handler.
//!
//! The scripted server plays the phone: it reads the device's identity key and ADV secret off the QR
//! step the actor shows and signs a `<pair-success>` the way a phone does (`wire::signed_pair_success`),
//! so upstream WOULD complete it. That is the point of THE CONTROL, which runs first: a pairing the
//! rules accept still gets its `pair-device-sign`, with no error iq. Without the control a refusal
//! test could pass for the wrong reason: upstream answers a pair-success it cannot verify with the
//! very same `error 500 internal-error` iq the claim sends (a signer that is subtly wrong would look
//! like a working claim). Keep every test of this file in the same tier as the control: never one in
//! `SLOW_TESTS` or `#[ignore]` without the other. Two checks hold that: `xtask`'s unit test
//! `slow::tests::the_pre_pair_wire_tests_stay_in_one_tier` and `tests/builder.rs`'s source check.
//!
//! Every body runs in ONE offline child under `unshare -rn` (lane decision L3), with synthetic values
//! only: the number, the LIDs, the stanza id and the tail below are made up, a tail of at least eight
//! digits that is a substring of no other synthetic value (so a leak check cannot hit a timestamp or a
//! port). The child records every log line of the process; the parent refuses any output of the child
//! that holds one of them.

use std::path::Path;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendConfig, BackendEvent, Command, Exit, StartMode, TakenIdentity};
use kourier_model::{DeviceLogout, ErrorKind, LoginStepKind, Redacted, UserId};
use wacore_binary::builder::NodeBuilder;

use crate::actor_real::Running;
use crate::support;
use crate::support::capture::Capture;
use crate::support::wire::{self, Seen, Wire};
use crate::wire::start_actor_with;

/// The server, as the root of every pairing iq.
const SERVER_JID: &str = "s.whatsapp.net";

/// The digits the run expects the number to end in.
const TAIL: &str = "52840163";
/// The phone the run expects: its number ends in [`TAIL`].
const PN_EXPECTED: &str = "4915752840163";
/// A phone the run does not expect.
const PN_STRANGER: &str = "4917309876254";
/// The LIDs of the two phones.
const LID_EXPECTED: &str = "100200300400501";
const LID_STRANGER: &str = "100200300400502";
/// The id of the pairing iq: made up and without a digit.
const PAIR_ID: &str = "synthetic-pairing-id";

/// What the logs and the output of the cases must never hold.
fn secrets() -> Vec<&'static str> {
    vec![
        TAIL,
        PN_EXPECTED,
        PN_STRANGER,
        LID_EXPECTED,
        LID_STRANGER,
        PAIR_ID,
    ]
}

/// The run's configuration: the number must end in the tail.
fn expecting(tail: &str) -> BackendConfig {
    BackendConfig {
        expect_number_suffix: Some(Redacted::new(tail.to_owned())),
        ..BackendConfig::default()
    }
}

/// What the login told the core so far.
#[derive(Default)]
struct Told {
    /// `(step id, the step's kind)` in order.
    steps: Vec<(String, LoginStepKind)>,
    /// `(step id, the step's transaction number)` in order (an `Input` step is answered by it).
    txns: Vec<(String, u64)>,
    /// `AccountReady` came (a pairing the rules accepted).
    ready: bool,
}

impl Told {
    fn has_step(&self, id: &str) -> bool {
        self.steps.iter().any(|(s, _)| s == id)
    }

    /// The transaction number of the last step called `id`.
    fn txn_of(&self, id: &str) -> Option<u64> {
        self.txns
            .iter()
            .rev()
            .find(|(s, _)| s == id)
            .map(|(_, t)| *t)
    }

    /// The `Failed` step, when there is one: its id and its error's kind.
    fn failed(&self) -> Option<(&str, &kourier_model::ErrorKind)> {
        self.steps.iter().find_map(|(id, kind)| match kind {
            LoginStepKind::Failed { error } => Some((id.as_str(), &error.kind)),
            _ => None,
        })
    }
}

/// Reads the actor's envelopes (acknowledging each, as the core would) into `told` until `done`
/// holds or `within` has passed; whether it held.
async fn listen(
    actor: &mut Running,
    told: &mut Told,
    within: Duration,
    done: impl Fn(&Told) -> bool,
) -> bool {
    let deadline = Instant::now() + within;
    while !done(told) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(envelope)) = tokio::time::timeout(left, actor.ends.events.recv()).await else {
            return false;
        };
        for event in &envelope.events {
            match event {
                BackendEvent::Login(step) => {
                    told.steps.push((step.step_id.clone(), step.kind.clone()));
                    told.txns.push((step.step_id.clone(), step.txn));
                }
                BackendEvent::AccountReady { .. } => told.ready = true,
                _ => {}
            }
        }
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
    }
    true
}

/// Starts a QR login on a fresh profile, lets the server offer its refs and returns the actor and
/// the code its first QR step shows (the secret the phone scans: it carries the device's keys).
/// `taken` is what the profile's other accounts own.
async fn waiting_for_the_scan(
    server: &Wire,
    dir: &Path,
    config: BackendConfig,
    taken: Vec<TakenIdentity>,
) -> (Running, Told, String) {
    let mut actor = start_actor_with(dir, StartMode::Fresh, server, config);
    actor
        .send(Command::StartLogin {
            flow: "qr".to_owned(),
            relink: None,
            taken,
        })
        .await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed (dials {})",
        server.dials()
    );
    let refs = (0..3).map(|i| {
        NodeBuilder::new("ref")
            .bytes(format!("2@synthetic{i}").into_bytes())
            .build()
    });
    server.send(
        &NodeBuilder::new("iq")
            .attrs([
                ("from", SERVER_JID),
                ("type", "set"),
                ("id", "pair-1"),
                ("xmlns", "md"),
            ])
            .children([NodeBuilder::new("pair-device").children(refs).build()])
            .build(),
    );
    let mut told = Told::default();
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(15), |t| t
            .has_step("qr"))
        .await,
        "no QR step came"
    );
    let code = told
        .steps
        .iter()
        .find_map(|(_, kind)| match kind {
            LoginStepKind::ShowQr { data, .. } => Some(data.expose().clone()),
            _ => None,
        })
        .expect("the QR step carries a code");
    (actor, told, code)
}

/// The phone scans the code: the server sends the pair-success of the account `pn`/`lid` (device 1),
/// signed so that upstream would complete it.
fn scan(server: &Wire, code: &str, pn: &str, lid: &str) {
    server.send(&wire::signed_pair_success(
        code,
        PAIR_ID,
        &format!("{pn}:1@{SERVER_JID}"),
        &format!("{lid}:1@lid"),
    ));
}

/// Waits (at most `within`) for a frame the client sent that satisfies `want`; the frames so far.
async fn wait_for_frame(server: &Wire, within: Duration, want: impl Fn(&Seen) -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if server.frames().iter().any(&want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Whether `frame` is the iq that completes a pairing.
fn is_pair_device_sign(frame: &Seen) -> bool {
    frame.tag == "iq" && frame.children.iter().any(|c| c == "pair-device-sign")
}

/// Whether `frame` is an iq of type error.
fn is_error_iq(frame: &Seen) -> bool {
    frame.tag == "iq" && frame.attr("type") == Some("error")
}

/// The values the log line of the number check carried (`expected_number_check="on"`): the fixed
/// token and nothing else.
fn number_check_lines(log: &Capture) -> Vec<String> {
    log.lines()
        .into_iter()
        .filter(|l| l.target.starts_with("kourier_whatsapp"))
        .filter_map(|l| {
            let at = l.text.find("expected_number_check=")?;
            l.text[at..].split_whitespace().next().map(str::to_owned)
        })
        .collect()
}

/// THE CONTROL (T-W3): a pairing the rules accept is left to upstream. The scan of the expected
/// phone gets its `pair-device-sign` (the frame that completes a link), no error iq leaves, the login
/// says `linking` and `AccountReady` is recorded. It passes without W2c too: it guards the claim's
/// narrowness and proves the signer of this file makes a pairing upstream completes. Mutant: the claim
/// takes every pairing.
async fn a_scan_that_passes_the_rules_is_left_to_upstream(log: &Capture) {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let (mut actor, mut told, code) =
        waiting_for_the_scan(&server, dir.path(), expecting(TAIL), Vec::new()).await;
    scan(&server, &code, PN_EXPECTED, LID_EXPECTED);

    // FIRST: the frame that completes the link left. A signer that upstream cannot verify makes the
    // refusal tests below pass for the wrong reason; this is the line that fails then.
    assert!(
        wait_for_frame(&server, Duration::from_secs(15), is_pair_device_sign).await,
        "no pair-device-sign left the client: the pairing was not completed (frames {:?})",
        server.frames()
    );
    let signed = server
        .frames()
        .into_iter()
        .find(is_pair_device_sign)
        .expect("the frame");
    assert_eq!(signed.attr("type"), Some("result"));
    assert_eq!(
        signed.attr("id"),
        Some(PAIR_ID),
        "the answer names its pairing"
    );
    assert!(
        !server.frames().iter().any(is_error_iq),
        "an error iq left the client (frames {:?})",
        server.frames()
    );
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(20), |t| t.ready
            && t.has_step("linking"))
        .await,
        "the login did not reach linking with AccountReady recorded (ready {}, steps {:?})",
        told.ready,
        told.steps
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        told.failed().is_none(),
        "the login failed: {:?}",
        told.failed()
    );
    assert_eq!(
        number_check_lines(log),
        vec![r#"expected_number_check="on""#.to_owned()],
        "the login did not say that the number is checked"
    );

    actor.send(Command::Shutdown).await;
    assert_eq!(actor.exit().await.0, Exit::Shutdown);
}

/// What the core's `Logout` reached: the one `LogoutReport` of the run.
async fn logout_outcome(actor: &mut Running) -> DeviceLogout {
    actor.send(Command::Logout).await;
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(20), actor.ends.events.recv())
            .await
            .expect("the logout is reported")
            .expect("an envelope");
        let outcome = envelope.events.iter().find_map(|event| match event {
            BackendEvent::Logout(report) => Some(report.outcome),
            _ => None,
        });
        if let Some(ack) = envelope.ack {
            ack.complete(Ok(()));
        }
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

/// What the device row of the profile holds of an account: `(pn, lid, account)` set or not. A
/// profile whose device never paired has none of the three (the row may not exist at all).
fn device_holds_an_account(dir: &Path) -> (bool, bool, bool) {
    let conn = kourier_db::open(&support::db_path(dir), &support::key()).expect("open the store");
    let record: Option<Vec<u8>> = conn
        .query_row("SELECT record FROM device", [], |r| r.get(0))
        .ok();
    let Some(record) = record else {
        return (false, false, false);
    };
    let value: serde_json::Value = serde_json::from_slice(&record).expect("the device is JSON");
    let set = |key: &str| value.get(key).is_some_and(|v| !v.is_null());
    (set("pn"), set("lid"), set("account"))
}

/// The WARN lines of a claimed refusal.
fn refusal_lines(log: &Capture) -> Vec<String> {
    log.lines()
        .into_iter()
        .filter(|l| l.target.starts_with("kourier_whatsapp"))
        .filter(|l| {
            l.text
                .contains("a pairing was refused before the device was made")
        })
        .map(|l| l.text)
        .collect()
}

/// What a refused scan looks like, whatever the reason (T-W1, T-W2): on the wire the pairing gets the
/// pair error and NO `pair-device-sign`, the login fails at `qr` with `kind`, the client is gone for
/// good, the core's `Logout` finds no device, and nothing of the other account is in the profile or
/// in a log line. `token` is the word of the one WARN line.
async fn assert_refused_before_the_device_exists(
    server: &Wire,
    mut actor: Running,
    mut told: Told,
    dir: &Path,
    log: &Capture,
    kind: ErrorKind,
    token: &str,
) {
    // FIRST, before anything about the error: the frame that completes a link must not have left.
    // Whichever of the two frames comes first ends the wait; a client that signed the pairing fails
    // here, whatever else it did.
    assert!(
        wait_for_frame(server, Duration::from_secs(15), |f| {
            is_error_iq(f) || is_pair_device_sign(f)
        })
        .await,
        "the client answered the pairing with nothing (frames {:?})",
        server.frames()
    );
    assert!(
        !server.frames().iter().any(is_pair_device_sign),
        "a pair-device-sign left the client: the device was signed (frames {:?})",
        server.frames()
    );
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(15), |t| t
            .failed()
            .is_some())
        .await,
        "the login did not fail (steps {:?})",
        told.steps
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        told.failed(),
        Some(("qr", &kind)),
        "the failure's step or kind"
    );
    assert!(
        !told.ready,
        "AccountReady was recorded for a refused pairing"
    );
    assert!(!told.has_step("linking"), "the login reached linking");

    // The client is stopped for good: it does not dial again.
    let dials = server.dials();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(server.dials(), dials, "the client dialled again");

    // The frames, whole: no signed pairing, ONE pair error that echoes the pairing's id.
    let frames = server.frames();
    assert!(
        !frames.iter().any(is_pair_device_sign),
        "a pair-device-sign left the client"
    );
    let errors: Vec<&Seen> = frames.iter().filter(|f| is_error_iq(f)).collect();
    assert_eq!(errors.len(), 1, "error iqs sent: {}", errors.len());
    let error = errors[0];
    assert_eq!(
        error.attr("id"),
        Some(PAIR_ID),
        "the answer names its pairing"
    );
    assert_eq!(error.attr("to"), Some(SERVER_JID));
    assert_eq!(error.children, vec!["error".to_owned()]);
    assert_eq!(error.first_child_attr("code"), Some("500"));
    assert_eq!(error.first_child_attr("text"), Some("internal-error"));

    // The core's Logout after the failed link finds nothing to remove, and the actor ends.
    assert_eq!(logout_outcome(&mut actor).await, DeviceLogout::NoDevice);
    assert!(matches!(actor.exit().await.0, Exit::LoggedOut { .. }));

    // Nothing of the other account was stored: no number, no LID, no signed identity.
    assert_eq!(
        device_holds_an_account(dir),
        (false, false, false),
        "the profile holds the other account (pn, lid, account)"
    );

    // The logs: the number check was on, ONE fixed WARN line said what happened, nothing identifies.
    assert_eq!(
        number_check_lines(log),
        vec![r#"expected_number_check="on""#.to_owned()],
        "the login did not say that the number is checked"
    );
    let lines = refusal_lines(log);
    assert_eq!(lines.len(), 1, "refusal lines: {}", lines.len());
    assert!(
        lines[0].contains(&format!(r#"refusal="{token}""#)) && lines[0].contains("answered=true"),
        "the refusal line is not the fixed one: {}",
        lines[0]
    );
    assert_eq!(
        log.leaks(&secrets()),
        Vec::<String>::new(),
        "a log line holds a synthetic number, LID, id or the tail"
    );
}

/// T-W1: the scan of ANOTHER number than the run expects (the QR flow). Without W2c the pairing is
/// signed (`pair-device-sign` leaves) and the other account's number is written into the profile; with
/// it the pairing gets the pair error and nothing else. Mutants: the claim passes every pairing;
/// the answer is not sent; its code is 401; the login is told through `deliver` (the refused flag
/// drops it); a log line names the number.
async fn a_scan_by_another_number_is_refused_before_the_device_exists(log: &Capture) {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let (actor, told, code) =
        waiting_for_the_scan(&server, dir.path(), expecting(TAIL), Vec::new()).await;
    scan(&server, &code, PN_STRANGER, LID_STRANGER);
    assert_refused_before_the_device_exists(
        &server,
        actor,
        told,
        dir.path(),
        log,
        ErrorKind::NotTheExpectedAccount,
        "expected-number",
    )
    .await;
}

/// T-W2: the scan of an account the profile already has. Its number passes the tail; ONLY its LID is
/// in `taken` (WhatsApp's own id is the LID), so the LID decides. Mutant: the judge compares the
/// number only.
async fn a_scan_by_an_account_already_in_the_profile_is_refused_before_the_device_exists(
    log: &Capture,
) {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let taken = vec![TakenIdentity {
        user: UserId::from_parts("lid", LID_EXPECTED).expect("a LID"),
        logged_out: false,
    }];
    let (actor, told, code) =
        waiting_for_the_scan(&server, dir.path(), expecting(TAIL), taken).await;
    scan(&server, &code, PN_EXPECTED, LID_EXPECTED);
    assert_refused_before_the_device_exists(
        &server,
        actor,
        told,
        dir.path(),
        log,
        ErrorKind::AlreadyAdded { logged_out: false },
        "taken",
    )
    .await;
}

/// The phone flow: a pairing that names no readable device arrives while the code is on its way.
/// There is no tail here (the check is OFF, the log says so), so the identity rules cannot refuse a
/// number: the kind is the check. The claim fails the login at `code` with `InvalidInput`
/// (`Unreadable`); without it upstream answers the same 500 itself but reports a pairing failure,
/// which the machine takes for a transient one. Mutant: the claim passes every pairing.
async fn a_pairing_without_a_device_in_the_phone_flow_is_refused_at_the_code_step(log: &Capture) {
    let server = wire::server();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut actor = start_actor_with(
        dir.path(),
        StartMode::Fresh,
        &server,
        BackendConfig::default(),
    );
    actor
        .send(Command::StartLogin {
            flow: "pair-code".to_owned(),
            relink: None,
            taken: Vec::new(),
        })
        .await;
    assert!(
        server.wait_opened(1, Duration::from_secs(15)).await,
        "the handshake never completed (dials {})",
        server.dials()
    );
    let mut told = Told::default();
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(15), |t| t
            .has_step("phone"))
        .await,
        "no number step came"
    );
    actor
        .send(Command::SubmitLoginInput {
            txn: told
                .txn_of("phone")
                .expect("the number step has a transaction"),
            values: vec![("phone".to_owned(), Redacted::new(format!("+{PN_STRANGER}")))],
        })
        .await;
    // The code request (stage 1) is on the wire: the server answers it with a pairing ref, and the
    // login shows the code.
    assert!(
        wait_for_frame(&server, Duration::from_secs(15), |f| {
            f.tag == "iq" && f.children.iter().any(|c| c == "link_code_companion_reg")
        })
        .await,
        "no code request left the client (frames {:?})",
        server.frames()
    );
    let hello = server
        .frames()
        .into_iter()
        .find(|f| f.children.iter().any(|c| c == "link_code_companion_reg"))
        .expect("the request");
    server.send(
        &NodeBuilder::new("iq")
            .attrs([
                ("from", SERVER_JID),
                ("type", "result"),
                ("id", hello.attr("id").expect("the request has an id")),
            ])
            .children([NodeBuilder::new("link_code_companion_reg")
                .children([NodeBuilder::new("link_code_pairing_ref")
                    .bytes(b"3@synthetic-ref".to_vec())
                    .build()])
                .build()])
            .build(),
    );
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(15), |t| t
            .has_step("code"))
        .await,
        "no code step came"
    );
    // The pairing names a device-identity that is no identity and no `<device>`.
    server.send(
        &NodeBuilder::new("iq")
            .attrs([("from", SERVER_JID), ("type", "set"), ("id", PAIR_ID)])
            .children([NodeBuilder::new("pair-success")
                .children([NodeBuilder::new("device-identity")
                    .bytes(vec![0xde, 0xad])
                    .build()])
                .build()])
            .build(),
    );
    assert!(
        wait_for_frame(&server, Duration::from_secs(15), |f| {
            is_error_iq(f) || is_pair_device_sign(f)
        })
        .await,
        "the client answered the pairing with nothing"
    );
    assert!(
        !server.frames().iter().any(is_pair_device_sign),
        "a pair-device-sign left the client"
    );
    assert!(
        listen(&mut actor, &mut told, Duration::from_secs(15), |t| t
            .failed()
            .is_some())
        .await,
        "the login did not fail"
    );
    assert_eq!(
        told.failed(),
        Some(("code", &ErrorKind::InvalidInput)),
        "the failure's step or kind"
    );
    assert_eq!(logout_outcome(&mut actor).await, DeviceLogout::NoDevice);
    assert!(matches!(actor.exit().await.0, Exit::LoggedOut { .. }));
    assert_eq!(device_holds_an_account(dir.path()), (false, false, false));
    assert_eq!(
        number_check_lines(log),
        vec![r#"expected_number_check="off""#.to_owned()],
        "the login did not say that the number is not checked"
    );
    let lines = refusal_lines(log);
    assert_eq!(lines.len(), 1, "refusal lines: {}", lines.len());
    assert!(
        lines[0].contains(r#"refusal="unreadable""#) && lines[0].contains("answered=true"),
        "the refusal line is not the fixed one: {}",
        lines[0]
    );
    assert_eq!(log.leaks(&secrets()), Vec::<String>::new());
}

// ----- the parents and the child -----

/// The case the child runs (set by the parent).
const CASE: &str = "WA_PREPAIR_CASE";

/// One parent per case: the case runs in the offline child below, and nothing the child printed holds
/// a synthetic number, LID, id or the tail.
fn parent(case: &str) {
    let output =
        support::offline::run_child_output(module_path!(), "prepair_child", &[(CASE, case)]);
    for secret in secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the fixture's secrets"
        );
    }
}

#[test]
fn a_scan_that_passes_the_rules_is_left_to_upstream_over_the_wire() {
    parent("passes");
}

#[test]
fn a_scan_by_another_number_is_refused_before_the_device_exists_over_the_wire() {
    parent("stranger");
}

#[test]
fn a_scan_by_an_account_already_in_the_profile_is_refused_before_the_device_exists_over_the_wire() {
    parent("taken");
}

#[test]
fn a_pairing_without_a_device_in_the_phone_flow_is_refused_at_the_code_step_over_the_wire() {
    parent("phone");
}

/// The child (a no-op as an ordinary test): the case the parent named, with every log line of the
/// process recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepair_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "passes" => a_scan_that_passes_the_rules_is_left_to_upstream(&log).await,
        "stranger" => a_scan_by_another_number_is_refused_before_the_device_exists(&log).await,
        "taken" => {
            a_scan_by_an_account_already_in_the_profile_is_refused_before_the_device_exists(&log)
                .await
        }
        "phone" => {
            a_pairing_without_a_device_in_the_phone_flow_is_refused_at_the_code_step(&log).await
        }
        other => panic!("unknown case {other}"),
    }
    // A check that saw no line of the crate proves nothing.
    assert!(
        log.own_lines() >= 1,
        "the recorder saw {} lines of the crate",
        log.own_lines()
    );
}
