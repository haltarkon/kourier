//! The supervisor (TG-1b B-4; TG-1 plan §5.4 T-s2, T-s3, T-s5..T-s8 as the critic rewrote them).
//!
//! In-process: the scripted TDLib's serve loop on a thread behind a socket pair. "Child": the fake
//! helper process, THIS test binary re-executed by `kourier_tdlib::spawn::spawn_self` with the
//! socket on fd 3 exactly as the real spawner sets it up (`fake_helper` below is its body).

use std::ffi::CStr;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant as StdInstant;

use kourier_db::DbKey;
use kourier_tdlib::protocol::FailureCode;
use kourier_tdlib::scripted::{Rule, Script, ScriptedTd};
use kourier_tdlib::serve::{ServeEnd, ServeOptions, serve};
use kourier_tdlib::td::{Engine, FatalCallback};

use super::*;
use crate::keys::tdlib_key_base64;
use crate::parts::RefusingDialer;
use crate::test_log::Lines;

/// The real helper over the test-only library against the fake DC (TD-4a), in the same child.
mod fakedc;
/// TG-2 C9: the actor's login and connection flows over the scripted TDLib.
mod flows;
/// The real library offline (TG-1b B-5), in its `unshare -rn` child.
mod real_lib;

/// The variable that makes `fake_helper` run (and names its scenario).
const CHILD_ENV: &str = "KOURIER_TG_FAKE_HELPER";
/// The fake helper's default heartbeat (the shipped one is 25 s); the watchdog tests name theirs.
const CHILD_BEAT: Duration = Duration::from_millis(200);
/// The fake helper's synthetic stderr text (T-s7): never in a log line.
const SECRET_LINE: &str = "+99966 secret text of the helper";

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn short() -> Timings {
    Timings {
        link: LinkOptions {
            hello_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        },
        hello: Duration::from_secs(5),
        start: Duration::from_secs(10),
        ..Timings::default()
    }
}

/// The scripted engine, recording the JSON of every `setTdlibParameters` it is sent.
#[derive(Clone)]
struct Recording {
    td: ScriptedTd,
    params: Arc<Mutex<Vec<String>>>,
}

impl Engine for Recording {
    fn create_client(&self) -> i32 {
        self.td.create_client()
    }

    fn send(&self, client: i32, json: &CStr) {
        let text = json.to_string_lossy();
        let is_params = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("@type").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|t| t == "setTdlibParameters");
        if is_params {
            self.params
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(text.into_owned());
        }
        self.td.send(client, json);
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        self.td.execute(json)
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        self.td.receive(timeout_s)
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        self.td.set_fatal_callback(cb)
    }
}

/// An in-process helper: the serve loop's thread.
struct ThreadChild(Option<JoinHandle<ServeEnd>>, Option<ExitStatus>);

fn status_of(end: ServeEnd) -> ExitStatus {
    let code = match end {
        ServeEnd::AllClosed => 0,
        ServeEnd::Forced => 4,
        ServeEnd::Fatal(_) => 5,
    };
    ExitStatus::from_raw(code << 8)
}

impl HelperChild for ThreadChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.1.is_none() && self.0.as_ref().is_some_and(JoinHandle::is_finished) {
            self.1 = self.0.take().and_then(|h| h.join().ok()).map(status_of);
        }
        Ok(self.1)
    }

    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(h) = self.0.take() {
            self.1 = h.join().ok().map(status_of);
        }
        self.1
            .ok_or_else(|| io::Error::other("the serve thread panicked"))
    }
}

/// What an in-process start leaves to look at.
struct InProcess {
    parts: TgParts,
    td: ScriptedTd,
    params: Arc<Mutex<Vec<String>>>,
    gate_port: Arc<Mutex<Option<u16>>>,
    spawns: Arc<AtomicUsize>,
}

fn in_process(script: Script) -> InProcess {
    in_process_saying(script, "off")
}

/// [`in_process`] whose helper's hello names `landlock` as its lock-down word (a serve loop under
/// a test ran none: it says `off`).
/// A script whose client never closes (`hang_on_close`) gets a helper whose own close gives up
/// after 500 ms (TG-3 Q-D3's bound test: the helper's 15 s would only make it slow).
fn in_process_saying(script: Script, landlock: &'static str) -> InProcess {
    let close_deadline = if script.hang_on_close {
        Duration::from_millis(500)
    } else {
        ServeOptions::default().close_deadline
    };
    let td = ScriptedTd::new(script);
    let params = Arc::new(Mutex::new(Vec::new()));
    let gate_port = Arc::new(Mutex::new(None));
    let spawns = Arc::new(AtomicUsize::new(0));
    let engine = Recording {
        td: td.clone(),
        params: params.clone(),
    };
    let (seen_port, counted) = (gate_port.clone(), spawns.clone());
    let parts = TgParts::from_makers(
        move |args| {
            counted.fetch_add(1, Ordering::SeqCst);
            *seen_port.lock().unwrap_or_else(|p| p.into_inner()) = Some(args.gate_port);
            let (app, helper) = UnixStream::pair()?;
            let engine = engine.clone();
            let opts = ServeOptions {
                gate_port: Some(args.gate_port),
                landlock,
                close_deadline,
                ..ServeOptions::default()
            };
            let serving = std::thread::spawn(move || serve(engine, helper, opts));
            Ok(HelperProcess {
                sock: app,
                stderr: None,
                child: Box::new(ThreadChild(Some(serving), None)),
            })
        },
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
    );
    InProcess {
        parts,
        td,
        params,
        gate_port,
        spawns,
    }
}

/// Parts whose n-th helper is the fake helper process of `scenarios[n]` (the last one repeats). A
/// scenario is `<name>` or `<name>@<heartbeat in ms>` (the fake helper's beat, 200 ms by default).
fn child_parts(scenarios: &'static [&'static str], spawns: Arc<AtomicUsize>) -> TgParts {
    child_parts_logged(scenarios, spawns, Arc::new(Mutex::new(Vec::new())))
}

/// [`child_parts`] that also records the process id of each helper it starts in `pids`.
fn child_parts_logged(
    scenarios: &'static [&'static str],
    spawns: Arc<AtomicUsize>,
    pids: Arc<Mutex<Vec<u32>>>,
) -> TgParts {
    TgParts::from_makers(
        move |_args| {
            let n = spawns.fetch_add(1, Ordering::SeqCst);
            let scenario = scenarios[n.min(scenarios.len() - 1)];
            let s = kourier_tdlib::spawn::spawn_self(
                "supervisor::tests::fake_helper",
                (CHILD_ENV, scenario),
            )?;
            pids.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(s.child.id());
            Ok(HelperProcess {
                sock: s.sock,
                stderr: s.stderr.map(|e| Box::new(e) as Box<dyn Read + Send>),
                child: Box::new(s.child),
            })
        },
        Arc::new(RefusingDialer::new(Arc::new(AtomicUsize::new(0)), false)),
    )
}

/// The fake helper process (runs only when re-executed with `CHILD_ENV`).
#[test]
fn fake_helper() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let (scenario, beat) = match spec.split_once('@') {
        Some((name, ms)) => (
            name.to_owned(),
            Duration::from_millis(ms.parse().unwrap_or(200)),
        ),
        None => (spec.clone(), CHILD_BEAT),
    };
    let Some(sock) = kourier_tdlib::spawn::helper_socket() else {
        std::process::exit(2);
    };
    let mut opts = ServeOptions {
        close_deadline: Duration::from_secs(2),
        heartbeat: beat,
        ..ServeOptions::default()
    };
    let mut script = Script::default();
    match scenario.as_str() {
        "clean" => {}
        // G14: the hello is slow (the start can be told things meanwhile), or never comes.
        "late" => std::thread::sleep(Duration::from_millis(1500)),
        "hang-hello" => {
            std::thread::sleep(Duration::from_secs(120));
            std::process::exit(2);
        }
        // G15: another TDLib than the pinned one.
        "wrong" => script.commit = "0000000000000000000000000000000000000000".to_owned(),
        // T-s4: TDLib aborts (fatal callback, SIGABRT, no core) at the parameters.
        "abort" => script.abort_on = Some("setTdlibParameters".to_owned()),
        // M4: TDLib deadlocks at the watchdog's probe: no answer, no heartbeat any more.
        "wedge" => script.wedge_on = Some("testCallEmpty".to_owned()),
        // M4: TDLib lives (heartbeats go on) but never answers the probe.
        "mute" => script.rules.push(Rule::new("testCallEmpty", Vec::new())),
        "hang" => {
            script.hang_on_close = true;
            opts.close_deadline = Duration::from_secs(60);
        }
        "stderr" => {
            for n in 0..3 {
                eprintln!("{SECRET_LINE} {n}");
            }
        }
        // TG-2 C10: the number is answered with WaitCode (the test then kills this helper while
        // the login waits for the code).
        "login-wait-code" => script.rules.push(Rule::new(
            "setAuthenticationPhoneNumber",
            vec![
                serde_json::json!({
                    "@type": "updateAuthorizationState",
                    "authorization_state": {
                        "@type": "authorizationStateWaitCode",
                        "code_info": {
                            "@type": "authenticationCodeInfo",
                            "type": {"@type": "authenticationCodeTypeSms", "length": 5},
                        },
                    },
                }),
                serde_json::json!({"@type": "ok"}),
            ],
        )),
        // TG-2 C10: what TDLib's binlog brings back after that death: WaitCode at once; a second
        // number would abort this helper (the login must not send one).
        "in-wait-code" => {
            script.after_parameters = "authorizationStateWaitCode".to_owned();
            script.abort_on = Some("setAuthenticationPhoneNumber".to_owned());
        }
        // TG-2 C10 (critic H4): a linked account whose TDLib answers `logOut` and `destroy` but
        // never closes (offline: every TG-2 build), and whose close hangs.
        "logout-never-closes" => {
            script.after_parameters = "authorizationStateReady".to_owned();
            script.rules.push(Rule::new(
                "logOut",
                vec![serde_json::json!({"@type": "ok"})],
            ));
            script.rules.push(Rule::new(
                "destroy",
                vec![serde_json::json!({"@type": "ok"})],
            ));
            script.hang_on_close = true;
            opts.close_deadline = Duration::from_secs(60);
        }
        // TG-2 fix J6: a login that reaches Ready and whose `getMe` is never answered (the test
        // kills this helper meanwhile and then cancels the login).
        "login-ready-no-me" => {
            let auth = |state: &str| {
                serde_json::json!({
                    "@type": "updateAuthorizationState",
                    "authorization_state": {"@type": state},
                })
            };
            script.rules.push(Rule::new(
                "setAuthenticationPhoneNumber",
                vec![
                    serde_json::json!({
                        "@type": "updateAuthorizationState",
                        "authorization_state": {
                            "@type": "authorizationStateWaitCode",
                            "code_info": {
                                "@type": "authenticationCodeInfo",
                                "type": {"@type": "authenticationCodeTypeSms", "length": 5},
                            },
                        },
                    }),
                    serde_json::json!({"@type": "ok"}),
                ],
            ));
            script.rules.push(Rule::new(
                "checkAuthenticationCode",
                vec![
                    auth("authorizationStateReady"),
                    serde_json::json!({"@type": "ok"}),
                ],
            ));
            script.rules.push(Rule::new("getMe", Vec::new()));
        }
        "nolib" => {
            kourier_tdlib::refuse::announce_and_wait(
                sock,
                FailureCode::LibOpenFailed,
                Duration::from_secs(5),
            );
            std::process::exit(3);
        }
        _ => std::process::exit(2),
    }
    let end = serve(ScriptedTd::new(script), sock, opts);
    std::process::exit(match end {
        ServeEnd::AllClosed => 0,
        ServeEnd::Forced => 4,
        ServeEnd::Fatal(_) => 5,
    });
}

struct Account {
    _dir: tempfile::TempDir,
    uuid: AccountUuid,
    db_key: DbKey,
    key: Zeroizing<String>,
    database_dir: PathBuf,
    files_root: PathBuf,
}

fn account() -> Account {
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let db_key = DbKey::from_bytes([0x33; 32]);
    let key = tdlib_key_base64(&db_key, uuid);
    let backend_db = dir.path().join("accounts").join("a").join("backend.db");
    std::fs::create_dir_all(backend_db.parent().expect("parent")).expect("account dir");
    let files_root = dir.path().join("media").join("telegram");
    let (database_dir, _) = account_dirs(&backend_db, &files_root, uuid).expect("dirs");
    Account {
        _dir: dir,
        uuid,
        db_key,
        key,
        database_dir,
        files_root,
    }
}

fn start_of(a: &Account, env: TgEnv) -> AccountStart<'_> {
    AccountStart {
        uuid: a.uuid,
        key: &a.key,
        database_dir: a.database_dir.clone(),
        files_dir: a.files_root.join(files_dir_name(a.uuid)),
        env,
        api: ApiCredentials::DUMMY,
        device_model: "Kourier (synthetic-host)",
    }
}

/// T-s2 (rewritten per B1): the supervisor writes the gate (`G`) and the parameters back to back,
/// so the helper writes `addProxy` right before `setTdlibParameters`; the faithful scripted TDLib
/// answers `addedProxy` only after the parameters, so a supervisor that waited in between would
/// never get there. The proxy is the gate: 127.0.0.1, the gate's port, enabled, and the only one.
#[test]
fn the_gate_goes_in_right_before_the_parameters_and_nothing_waits_between() {
    let p = in_process(Script::default());
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let report = rt.block_on(async {
        let helper = start(
            &p.parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        .expect("the start did not reach the phone-number step");
        assert_eq!(
            helper.lockdown,
            Lockdown::Other,
            "a serve loop under a test ran no lock-down"
        );
        helper.stop(&short(), &cancel).await
    });
    let types = p.td.sent_types();
    let proxy = types.iter().position(|t| t == "addProxy");
    let params = types.iter().position(|t| t == "setTdlibParameters");
    // K2: the gate's proxy, the files option, the parameters: back to back.
    assert!(
        proxy.is_some()
            && params == proxy.map(|p| p + 2)
            && proxy.is_some_and(|p| types[p + 1] == "setOption"),
        "addProxy and the files option are not written right before the parameters: {types:?}"
    );
    let port = p
        .gate_port
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expect("the helper got a gate port");
    assert_eq!(
        p.td.proxies()
            .into_iter()
            .map(|(_, s, port, on)| (s, port, on))
            .collect::<Vec<_>>(),
        vec![("127.0.0.1".to_owned(), i64::from(port), true)],
        "TDLib's proxy is not the gate alone"
    );
    assert_eq!(p.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(
        report.status.and_then(|s| s.code()),
        Some(0),
        "the helper did not end cleanly"
    );
    assert_eq!(report.forced, Some(false));
    assert!(!report.killed);
}

/// T-s2 (c) and re-check N2: TDLib refusing the gate's proxy (here: its binlog failed) leaves the
/// client without a proxy: the start fails typed and the helper is stopped and reaped.
#[test]
fn a_proxy_tdlib_refuses_fails_the_start_and_stops_the_helper() {
    let p = in_process(Script {
        fail_add_proxy: true,
        ..Script::default()
    });
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let started = rt.block_on(start(
        &p.parts,
        &start_of(&a, TgEnv::Production),
        rt.handle(),
        &short(),
        &cancel,
    ));
    assert!(
        matches!(started, Err(StartError::GateProxy)),
        "a refused gate proxy did not fail the start: {:?}",
        started.err()
    );
    assert!(p.td.proxies().is_empty(), "a refused proxy was stored");
    assert_eq!(p.spawns.load(Ordering::SeqCst), 1);
}

/// One start of `p`'s helper with the lock-down `need` required: the word the helper's hello said,
/// or the typed failure (the helper is stopped and reaped either way).
fn start_needing(p: &InProcess, need: LockdownNeed) -> Result<Lockdown, StartError> {
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let parts = p.parts.clone().requiring_lockdown(need);
    rt.block_on(async {
        match start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        {
            Ok(helper) => {
                let word = helper.lockdown;
                helper.stop(&short(), &cancel).await;
                Ok(word)
            }
            Err(e) => Err(e),
        }
    })
}

/// G5: the application's parts REQUIRE the helper's lock-down: a hello that says `off` (a test's
/// serve loop, or a stale TG-1a binary that ignores `--gate-port`) fails the start typed
/// `NoLockdown`, class Never, and the helper is stopped and reaped; `on` and `unavailable` pass. A
/// test's own parts (no requirement) still start with `off`.
#[test]
fn the_real_parts_refuse_a_helper_that_reports_no_lock_down() {
    let off = in_process(Script::default());
    let refused = start_needing(&off, LockdownNeed::OnOrUnavailable);
    assert_eq!(
        refused.as_ref().err(),
        Some(&StartError::NoLockdown),
        "a helper without a lock-down was accepted: {refused:?}"
    );
    assert_eq!(StartError::NoLockdown.retry(), Retry::Never);
    assert_eq!(off.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(
        off.td
            .sent_types()
            .iter()
            .filter(|t| *t == "setTdlibParameters")
            .count(),
        0,
        "the parameters went to a helper without a lock-down"
    );
    assert_eq!(
        start_needing(&off, LockdownNeed::Off),
        Ok(Lockdown::Other),
        "parts without a requirement refused a helper"
    );
    for (word, want) in [("on", Lockdown::On), ("unavailable", Lockdown::Unavailable)] {
        let p = in_process_saying(Script::default(), word);
        assert_eq!(
            start_needing(&p, LockdownNeed::OnOrUnavailable),
            Ok(want),
            "{word} was refused"
        );
    }
}

/// TR R0a D-11, T3: parts that dial need the lock-down `on`: a hello that says `unavailable` (a
/// kernel without Landlock) or `off` fails the start typed `NoLockdown` (class Never), the helper
/// gets no parameters and no gate frame, while `on` passes; the refusing parts still accept
/// `unavailable`. Mutant: the `On` arm accepts `unavailable`.
#[test]
fn a_hello_that_says_unavailable_fails_the_parts_that_need_the_lock_down_on() {
    for word in ["unavailable", "off"] {
        let p = in_process_saying(Script::default(), word);
        let refused = start_needing(&p, LockdownNeed::On);
        assert_eq!(
            refused.as_ref().err(),
            Some(&StartError::NoLockdown),
            "a hello saying {word} was accepted by parts that need it on: {refused:?}"
        );
        assert_eq!(p.spawns.load(Ordering::SeqCst), 1);
        let sent = p.td.sent_types();
        assert!(
            !sent
                .iter()
                .any(|t| t == "setTdlibParameters" || t == "addProxy"),
            "the helper was given its gate or parameters before the lock-down check: {sent:?}"
        );
    }
    let on = in_process_saying(Script::default(), "on");
    assert_eq!(start_needing(&on, LockdownNeed::On), Ok(Lockdown::On));
    let unavailable = in_process_saying(Script::default(), "unavailable");
    assert_eq!(
        start_needing(&unavailable, LockdownNeed::OnOrUnavailable),
        Ok(Lockdown::Unavailable),
        "the refusing parts refused an unavailable lock-down"
    );
}

/// T-s3: the parameters carry the rules of plan §2.7 (message DB off, chat-info and file DB on,
/// no secret chats, the stored environment, `<account>/tdlib`, a files directory below the root,
/// the HKDF key in base64, the dummy API pair).
#[test]
fn tdlib_parameters_carry_the_rules() {
    let p = in_process(Script::default());
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    rt.block_on(async {
        let helper = start(
            &p.parts,
            &start_of(&a, TgEnv::Test),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        .expect("the start did not reach the phone-number step");
        helper.stop(&short(), &cancel).await;
    });
    let sent = p.params.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(sent.len(), 1, "not exactly one setTdlibParameters");
    let v: Value = serde_json::from_str(&sent[0]).expect("json");
    for (field, want) in [
        ("use_message_database", false),
        ("use_chat_info_database", true),
        ("use_file_database", true),
        ("use_secret_chats", false),
        ("use_test_dc", true),
    ] {
        assert_eq!(v[field].as_bool(), Some(want), "{field} breaks the rule");
    }
    assert_eq!(
        v["database_directory"].as_str().map(PathBuf::from),
        Some(a.database_dir.clone())
    );
    assert!(a.database_dir.ends_with("tdlib"));
    let files = v["files_directory"]
        .as_str()
        .map(PathBuf::from)
        .expect("files");
    assert!(
        files.starts_with(&a.files_root) && files != a.files_root,
        "the files directory is not below the root"
    );
    // G15: named with the whole uuid (32 hex digits), not its first 8.
    assert_eq!(
        files.file_name().and_then(|n| n.to_str()),
        Some(a.uuid.0.simple().to_string().as_str()),
        "the files directory is not named by the whole uuid"
    );
    assert_eq!(files_dir_name(a.uuid).len(), 32);
    // Compared without printing either value.
    let key = tdlib_key_base64(&a.db_key, a.uuid);
    assert!(
        v["database_encryption_key"].as_str() == Some(key.as_str()),
        "the key is not the HKDF key of the account"
    );
    assert_eq!(
        v["api_id"].as_i64(),
        Some(1),
        "the api_id is not the dummy's"
    );
    assert_eq!(v["api_hash"].as_str(), Some(ApiCredentials::DUMMY.hash()));
    // His answer U-4: the device model is the caller's (the app makes `Kourier (<host name>)`).
    assert_eq!(v["device_model"].as_str(), Some("Kourier (synthetic-host)"));
    assert_eq!(v["system_version"].as_str(), Some("Linux"));
    // TG-2 M10: the text the secret path built is the whole object, nothing more.
    assert_eq!(v["@type"].as_str(), Some("setTdlibParameters"));
    assert!(
        v["@extra"].is_u64(),
        "the parameters went without their @extra"
    );
    let mode = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o777)
            .ok()
    };
    assert_eq!(mode(&a.database_dir), Some(0o700));
}

/// T-s5 (child, M3): the stop sends `Q` and waits; the helper closes its client and exits 0 with
/// `B forced:false`, well inside the grace. (Mutant: kill at once without waiting -> by signal.)
#[test]
fn shutdown_closes_and_the_helper_exits_cleanly() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["clean"], spawns.clone());
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let (report, took) = rt.block_on(async {
        let helper = start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        .expect("the fake helper did not reach the phone-number step");
        let t0 = StdInstant::now();
        let report = helper.stop(&short(), &cancel).await;
        (report, t0.elapsed())
    });
    assert_eq!(report.status.and_then(|s| s.code()), Some(0), "{report:?}");
    assert_eq!(report.forced, Some(false), "{report:?}");
    assert!(!report.killed);
    assert!(took < Duration::from_secs(2), "the stop took {took:?}");
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

/// T-s6 (child, M3): a helper whose client never closes (and whose own deadline is 60 s here) is
/// SIGKILLed at the grace (scaled to 1 s) and reaped. (Mutant: no kill -> the bound fails.)
#[test]
fn a_hanging_helper_is_killed_after_the_grace() {
    let parts = child_parts(&["hang"], Arc::new(AtomicUsize::new(0)));
    let a = account();
    let cancel = CancellationToken::new();
    let t = Timings {
        kill_after: Duration::from_secs(1),
        ..short()
    };
    let rt = rt();
    let (report, took) = rt.block_on(async {
        let helper = start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &t,
            &cancel,
        )
        .await
        .expect("the fake helper did not reach the phone-number step");
        let t0 = StdInstant::now();
        let report = helper.stop(&t, &cancel).await;
        (report, t0.elapsed())
    });
    assert!(report.killed, "{report:?}");
    assert_eq!(
        report.status.and_then(|s| s.signal()),
        Some(9 /* SIGKILL */),
        "not reaped by signal"
    );
    assert!(took < Duration::from_secs(5), "the stop took {took:?}");
}

/// M3: a cancelled token during the stop kills and reaps at once (not after the 17 s grace).
#[test]
fn a_cancelled_stop_still_kills_and_reaps() {
    let parts = child_parts(&["hang"], Arc::new(AtomicUsize::new(0)));
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let (report, took) = rt.block_on(async {
        let helper = start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        .expect("the fake helper did not reach the phone-number step");
        cancel.cancel();
        let t0 = StdInstant::now();
        let report = helper.stop(&Timings::default(), &cancel).await;
        (report, t0.elapsed())
    });
    assert!(report.killed, "{report:?}");
    assert!(report.status.is_some(), "the helper was not reaped");
    assert!(took < Duration::from_secs(3), "the stop took {took:?}");
}

/// T-s7 (child): the helper's stderr is drained and its lines counted; the count reaches the log,
/// the text never does.
#[test]
fn helper_stderr_is_counted_never_logged() {
    let parts = child_parts(&["stderr"], Arc::new(AtomicUsize::new(0)));
    let a = account();
    let cancel = CancellationToken::new();
    let lines = Lines::default();
    let rt = rt();
    let report = {
        let _guard = tracing::subscriber::set_default(lines.clone());
        rt.block_on(async {
            let helper = start(
                &parts,
                &start_of(&a, TgEnv::Production),
                rt.handle(),
                &short(),
                &cancel,
            )
            .await
            .expect("the fake helper did not reach the phone-number step");
            helper.stop(&short(), &cancel).await
        })
    };
    assert!(
        report.stderr_lines >= 3,
        "the stderr lines were not counted"
    );
    let all = lines.all();
    assert!(
        all.iter()
            .any(|l| l.contains("the TDLib helper stopped") && l.contains("stderr_lines")),
        "the count did not reach the log"
    );
    assert!(
        !all.iter()
            .any(|l| l.contains("99966") || l.contains("secret text")),
        "a stderr line reached the log"
    );
}

/// T-s8, first half (the no-restart half comes with the back-off): a helper that cannot open its
/// library (`F lib-open-failed`, exit 3) is a typed "missing library", and it is reaped at once
/// (the app's end closes, so it does not sit out its 5 s).
#[test]
fn a_missing_library_is_typed_and_the_helper_reaped() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["nolib"], spawns.clone());
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let t0 = StdInstant::now();
    let started = rt.block_on(start(
        &parts,
        &start_of(&a, TgEnv::Production),
        rt.handle(),
        &short(),
        &cancel,
    ));
    let err = started.err().expect("a helper without its library started");
    assert!(
        err.is_missing_library(),
        "not typed as a missing library: {err:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "the refusing helper was not ended early"
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

/// K10 (the security review's L4): a call of the start that failed because the helper's stream
/// ended is typed `Helper(TdlibFatal)` when the helper's `F tdlib-fatal` is queued behind it, and
/// stays `Link(..)` / `Parameters(..)` otherwise. Scripted helper end: the frames the reader would
/// have queued.
#[test]
fn a_tdlib_fatal_behind_a_failed_start_call_types_the_start() {
    use kourier_tdlib::protocol::Bye;

    let queue = |frames: Vec<Inbound>| {
        let (tx, rx) = mpsc::channel(8);
        for f in frames {
            tx.try_send(f).expect("room");
        }
        rx
    };
    let fatal = || Inbound::Fatal(FailureCode::TdlibFatal);
    for error in [LinkError::Closed, LinkError::Frame] {
        // The fatal frame and the end of the stream are queued (the reader's order).
        for frames in [
            vec![fatal(), Inbound::Closed],
            vec![Inbound::Heartbeat, fatal()],
        ] {
            let mut rx = queue(frames);
            assert_eq!(
                typed_after_link_failure(error, &mut rx, StartError::Link),
                StartError::Helper(FailureCode::TdlibFatal),
                "{error:?}: the fatal frame behind the failure was not found"
            );
            let mut rx = queue(vec![fatal()]);
            assert_eq!(
                typed_after_link_failure(error, &mut rx, StartError::Parameters),
                StartError::Helper(FailureCode::TdlibFatal)
            );
        }
        // No fatal frame: the call's own class.
        let mut rx = queue(vec![Inbound::Closed]);
        assert_eq!(
            typed_after_link_failure(error, &mut rx, StartError::Link),
            StartError::Link(error)
        );
        let mut rx = queue(vec![Inbound::Bye(Bye {
            closed: Vec::new(),
            forced: false,
        })]);
        assert_eq!(
            typed_after_link_failure(error, &mut rx, StartError::Parameters),
            StartError::Parameters(error)
        );
        // Another fatal code keeps its old typing (it is not TDLib's own fatal path).
        let mut rx = queue(vec![Inbound::Fatal(FailureCode::BadFrame)]);
        assert_eq!(
            typed_after_link_failure(error, &mut rx, StartError::Link),
            StartError::Link(error)
        );
    }
    // A failure that is not the stream's end is never looked behind.
    let mut rx = queue(vec![fatal()]);
    assert_eq!(
        typed_after_link_failure(LinkError::Timeout, &mut rx, StartError::Link),
        StartError::Link(LinkError::Timeout)
    );
    // Both are Retry::Abort: the behaviour does not change.
    assert_eq!(
        StartError::Helper(FailureCode::TdlibFatal).retry(),
        Retry::Abort
    );
}

/// K10, end to end: the fake helper process aborts at the parameters through TDLib's fatal path
/// (`F tdlib-fatal`, then the end of its stream); the start is `Helper(TdlibFatal)`, class Abort.
#[test]
fn a_helper_that_aborts_at_the_parameters_types_the_start_as_tdlibs_fatal() {
    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["abort"], spawns.clone());
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let started = rt.block_on(start(
        &parts,
        &start_of(&a, TgEnv::Production),
        rt.handle(),
        &short(),
        &cancel,
    ));
    let err = started.err().expect("an aborting helper started");
    assert_eq!(err, StartError::Helper(FailureCode::TdlibFatal));
    assert_eq!(err.retry(), Retry::Abort);
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

/// T-s1 (TG-2): the helper starts at a login, not at the actor's start or a `Connect`; the login
/// runs TDLib's start up to the phone-number step and shows the `phone` input step (txn 1), and
/// nothing more is sent (no number was typed).
#[test]
fn the_helper_starts_at_start_login_not_at_spawn() {
    use kourier_backend_api::{
        AccountContext, Command, ContextParams, Exit, StartMode, SystemClock,
    };
    use kourier_model::AccountId;

    let p = in_process(Script::default());
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (ctx, mut ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
        backend_db: dir.path().join("backend.db"),
        db_key: kourier_backend_api::DbKey::new([7; 32]),
        blob_dir: dir.path().join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(SystemClock),
        config: Default::default(),
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let parts = p
        .parts
        .clone()
        .with_api(ApiCredentials::DUMMY)
        .with_files_root(dir.path().join("media"));
    let actor = std::thread::spawn(move || crate::actor::run_with(ctx, parts, short()));
    rt.block_on(async {
        ends.commands.send(Command::Connect).await.expect("queued");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            p.spawns.load(Ordering::SeqCst),
            0,
            "a helper started before a login"
        );
        ends.commands
            .send(Command::StartLogin {
                flow: "phone".to_owned(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("queued");
        let step = first_step(&mut ends).await;
        assert!(
            is_phone_step(&step),
            "the start did not get to the phone step"
        );
        assert_eq!(step.txn, 1);
        assert_eq!(p.spawns.load(Ordering::SeqCst), 1, "not exactly one helper");
        assert!(
            p.td.sent_types()
                .iter()
                .all(|t| t != "setAuthenticationPhoneNumber"),
            "a phone number was sent"
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// The supervisor's times for the watchdog tests: the fake helper's `beat` (its scenario names the
/// same), a probe every 300 ms. A stall of the test machine up to twice the beat is tolerated, so
/// a test that wants no false alarm names a long beat (G11).
fn watched(request_timeout: Duration, beat: Duration) -> Timings {
    Timings {
        link: LinkOptions {
            hello_timeout: Duration::from_secs(5),
            request_timeout,
        },
        beat,
        probe_every: Duration::from_millis(300),
        kill_after: Duration::from_secs(1),
        ..short()
    }
}

/// Starts the fake helper of `scenario` up to the phone-number step and runs the watchdog on it
/// (at most 8 s): what it said and after how long.
fn watch_child(scenario: &'static [&'static str], t: Timings) -> Option<(Wedged, Duration)> {
    let parts = child_parts(scenario, Arc::new(AtomicUsize::new(0)));
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    rt.block_on(async {
        let mut helper = start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &t,
            &cancel,
        )
        .await
        .expect("the helper did not start");
        let t0 = StdInstant::now();
        let why = tokio::time::timeout(Duration::from_secs(8), helper.watch(&t, &cancel))
            .await
            .ok();
        let took = t0.elapsed();
        let report = helper.stop(&t, &cancel).await;
        assert!(report.status.is_some(), "the helper was not reaped");
        why.map(|w| (w, took))
    })
}

/// M4: a helper whose TDLib deadlocks (its receiver and so its heartbeat stop) is given up after
/// two missed beats, long before the probe's own 20 s timeout.
#[test]
fn a_wedged_helper_is_found_by_its_missing_heartbeats() {
    // The 200 ms beat is the point here (Silent after 400 ms); the bound below is an upper bound
    // with 2.6 s of slack.
    let (why, took) = watch_child(&["wedge"], watched(Duration::from_secs(20), CHILD_BEAT))
        .expect("the watchdog did not give the wedged helper up");
    assert_eq!(why, Wedged::Silent);
    assert!(took < Duration::from_secs(3), "found only after {took:?}");
}

/// M4: a helper that beats but whose TDLib never answers `testCallEmpty` is given up at the
/// probe's timeout.
#[test]
fn an_unanswered_probe_gives_the_helper_up() {
    // A 5 s beat (Silent only at 10 s): the answer that comes is the probe's, at ~1.3 s, with no
    // 400 ms gap of the test machine able to turn it into Silent.
    let (why, took) = watch_child(
        &["mute@5000"],
        watched(Duration::from_secs(1), Duration::from_secs(5)),
    )
    .expect("the watchdog did not give the mute helper up");
    assert_eq!(why, Wedged::Unanswered);
    assert!(took < Duration::from_secs(4), "found only after {took:?}");
}

/// M4, no false alarm: a live helper whose TDLib answers the probe (with an error: still an
/// answer) is kept through several probes and beats.
#[test]
fn a_live_helper_is_kept_by_the_watchdog() {
    // U11 (strike 1: `stop` took the whole 1 s `kill_after` of `watched` under a load of 20, the
    // helper was killed and its status had no code). What this test checks is that no frame, no
    // probe answer and no beat is taken for a missing one, over ~5 s of a helper that lives; so
    // every other time here is a bound that only a failure reaches, none a window the pass
    // depends on:
    // - the beat is 2 s (Silent after 4 s without a frame, a stall of the helper's beat thread up
    //   to 2 s is no false alarm); the 5 s window is longer than that, so a watchdog that did not
    //   count the beats WOULD say Silent inside it (mutant);
    // - the probes (every 300 ms) are answered with an error within 10 s, far over the window: only
    //   an error answer taken for silence ends the watch early (an unanswered probe has its own
    //   test, `an_unanswered_probe_gives_the_helper_up`);
    // - the stop waits for the helper's exit, `Q` to SIGKILL 15 s (the shipped 17 s, the helper's
    //   own deadline 15 s), a clean helper is gone in milliseconds.
    let t = Timings {
        kill_after: Duration::from_secs(15),
        ..watched(Duration::from_secs(10), Duration::from_secs(2))
    };
    let parts = child_parts(&["clean@2000"], Arc::new(AtomicUsize::new(0)));
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    rt.block_on(async {
        let mut helper = start(
            &parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &t,
            &cancel,
        )
        .await
        .expect("the helper did not start");
        let kept = tokio::time::timeout(Duration::from_secs(5), helper.watch(&t, &cancel))
            .await
            .is_err();
        assert!(kept, "the watchdog gave a live helper up");
        let report = helper.stop(&t, &cancel).await;
        assert_eq!(report.status.and_then(|s| s.code()), Some(0));
    });
}

/// The restart back-off: 5 s doubling to 5 min; a helper that lived 120 s starts over at 5 s.
#[test]
fn the_back_off_doubles_to_five_minutes_and_resets_after_a_stable_run() {
    let mut b = Backoff::default();
    let waits: Vec<u64> = (0..9)
        .map(|i| {
            b.failed(Retry::Transient, Duration::from_secs(1), i * 1000)
                .0
                .as_secs()
        })
        .collect();
    assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    assert_eq!(
        b.failed(Retry::Abort, Backoff::STABLE, 10_000).0,
        Backoff::MIN
    );
    assert_eq!(
        b.failed(Retry::Abort, Duration::from_secs(119), 11_000).0,
        Duration::from_secs(10)
    );
}

/// Five aborts within ten minutes tell the person once per burst; the first four transient failures
/// (L8) in a row never count (the cap is `transient_failures_in_a_row_are_capped_...`); aborts three
/// minutes apart never make a burst.
#[test]
fn five_aborts_in_ten_minutes_tell_once_per_burst() {
    const MIN: u64 = 60_000;
    let life = Duration::from_secs(1);
    let mut b = Backoff::default();
    for i in 0..4 {
        assert!(
            !b.failed(Retry::Transient, life, i).1,
            "a transient failure counted"
        );
    }
    let tells: Vec<bool> = (0..7)
        .map(|i| b.failed(Retry::Abort, life, 100 * MIN + i * MIN).1)
        .collect();
    assert_eq!(tells, [false, false, false, false, true, false, false]);
    // A later burst is told again.
    let tells: Vec<bool> = (0..5)
        .map(|i| b.failed(Retry::Abort, life, 200 * MIN + i * MIN).1)
        .collect();
    assert_eq!(tells, [false, false, false, false, true]);
    let mut b = Backoff::default();
    assert!(
        (0..10).all(|i| !b.failed(Retry::Abort, life, i * 3 * MIN).1),
        "aborts three minutes apart made a burst"
    );
}

/// G12: transient failures (a locked database) in a row are free for four tries; the fifth and each
/// one after it counts as an abort, and the person is told once when the cap is reached; a start
/// that is an abort (or that worked) starts the run over. G13: a start that reached the phone-number
/// step starts the back-off over.
#[test]
fn transient_failures_in_a_row_are_capped_and_a_start_that_worked_starts_over() {
    let life = Duration::from_secs(1);
    let mut b = Backoff::default();
    let tells: Vec<bool> = (0..9)
        .map(|i| b.failed(Retry::Transient, life, i * 1000).1)
        .collect();
    // The cap's notice at the fifth; the ninth is the fifth abort (the fifth to the ninth).
    assert_eq!(
        tells,
        [false, false, false, false, true, false, false, false, true]
    );
    assert_eq!(b.transients(), 9);

    // An abort between two runs: the run starts over (the fifth transient is the first after it).
    let mut b = Backoff::default();
    for i in 0..4 {
        assert!(!b.failed(Retry::Transient, life, i).1);
    }
    assert!(!b.failed(Retry::Abort, life, 10).1);
    assert_eq!(b.transients(), 0, "an abort did not end the run");
    for i in 0..4 {
        assert!(!b.failed(Retry::Transient, life, 20 + i).1);
    }
    assert!(
        b.failed(Retry::Transient, life, 30).1,
        "the cap was not reached"
    );

    // G13: waits 5, 10, 20 s; a start that worked; the next failure waits 5 s again.
    let mut b = Backoff::default();
    let waits: Vec<u64> = (0..3)
        .map(|i| b.failed(Retry::Abort, life, i).0.as_secs())
        .collect();
    assert_eq!(waits, [5, 10, 20]);
    b.reached();
    assert_eq!(b.failed(Retry::Abort, life, 10).0, Backoff::MIN);
    assert_eq!(b.transients(), 0);
}

/// Which failed starts are tried again: an abort, a locked database (transient), never a property
/// of the build or a refusal.
#[test]
fn start_errors_are_classed_for_the_restart() {
    use StartError as E;
    for e in [
        E::Ended,
        E::Timeout,
        E::Helper(FailureCode::TdlibFatal),
        E::Link(LinkError::Closed),
        E::Link(LinkError::HelloTimeout),
        E::Parameters(LinkError::Closed),
    ] {
        assert_eq!(e.retry(), Retry::Abort, "{e:?}");
    }
    assert_eq!(
        E::Parameters(LinkError::DatabaseLocked).retry(),
        Retry::Transient
    );
    // G15: a passing shortage of processes or memory is tried again; a missing file is not.
    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::OutOfMemory] {
        assert_eq!(E::Spawn(kind).retry(), Retry::Abort, "{kind:?}");
    }
    // G15, G5: a helper that is not this build's is unsupported, not "transient".
    assert!(E::Link(LinkError::VersionMismatch).is_wrong_helper());
    assert!(E::NoLockdown.is_wrong_helper());
    for e in [
        E::GateProxy,
        E::Ended,
        E::Gate,
        E::Spawn(io::ErrorKind::NotFound),
    ] {
        assert!(!e.is_wrong_helper(), "{e:?}");
    }
    for e in [
        E::Helper(FailureCode::LibOpenFailed),
        E::Helper(FailureCode::LibSymbolMissing),
        E::Helper(FailureCode::LockDownFailed),
        E::Helper(FailureCode::LogHardeningFailed),
        E::Helper(FailureCode::BadFrame),
        // TG-2b: parameters naming other directories (a bug of this build), and a directory the
        // helper's command line cannot carry (refused before the spawn).
        E::Helper(FailureCode::RefusedParametersPaths),
        E::Spawn(io::ErrorKind::InvalidInput),
        E::GateProxy,
        E::Link(LinkError::VersionMismatch),
        E::Parameters(LinkError::Td { code: 400 }),
        E::Gate,
        E::Spawn(io::ErrorKind::NotFound),
        E::UnexpectedState,
        E::Cancelled,
    ] {
        assert_eq!(e.retry(), Retry::Never, "{e:?}");
    }
}

/// An actor over `parts` on `clock`, its core ends, and its thread.
fn actor_on(
    parts: TgParts,
    clock: Arc<kourier_backend_api::FakeClock>,
    rt: &tokio::runtime::Runtime,
    dir: &Path,
) -> (
    std::thread::JoinHandle<kourier_backend_api::Exit>,
    kourier_backend_api::CoreEnds,
) {
    actor_timed(parts, clock, rt, dir, short())
}

/// [`actor_on`] with the actor's `timings` (the real library's start needs longer bounds).
fn actor_timed(
    parts: TgParts,
    clock: Arc<kourier_backend_api::FakeClock>,
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    timings: Timings,
) -> (
    std::thread::JoinHandle<kourier_backend_api::Exit>,
    kourier_backend_api::CoreEnds,
) {
    use kourier_backend_api::{AccountContext, ContextParams, StartMode};
    use kourier_model::AccountId;

    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid: AccountUuid::new_random(),
        start: StartMode::Fresh,
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
    let actor = std::thread::spawn(move || crate::actor::run_with(ctx, parts, timings));
    (actor, ends)
}

fn start_login() -> kourier_backend_api::Command {
    kourier_backend_api::Command::StartLogin {
        flow: "phone".to_owned(),
        relink: None,
        taken: Vec::new(),
    }
}

/// The detail of the one login step of the next envelope (10 s at most).
async fn login_detail(ends: &mut kourier_backend_api::CoreEnds) -> String {
    use kourier_backend_api::BackendEvent;
    use kourier_model::LoginStepKind;
    let envelope = tokio::time::timeout(Duration::from_secs(10), ends.events.recv())
        .await
        .expect("no login step in time")
        .expect("events ended");
    match &envelope.events[..] {
        [BackendEvent::Login(step)] => match &step.kind {
            LoginStepKind::Failed { error } => error.detail.clone(),
            _ => panic!("the login step is not a failure"),
        },
        _ => panic!("not exactly one login step"),
    }
}

/// The one login step of the next envelope (10 s at most), whatever its kind (TG-2: a login that
/// reached TDLib's phone-number step shows the `phone` input step and goes on).
async fn first_step(ends: &mut kourier_backend_api::CoreEnds) -> kourier_model::LoginStep {
    use kourier_backend_api::BackendEvent;
    let envelope = tokio::time::timeout(Duration::from_secs(10), ends.events.recv())
        .await
        .expect("no login step in time")
        .expect("events ended");
    let mut events = envelope.events;
    match (events.len(), events.pop()) {
        (1, Some(BackendEvent::Login(step))) => step,
        _ => panic!("not exactly one login step"),
    }
}

/// Whether `step` is the phone-number input step of a login (what a start that reached
/// `authorizationStateWaitPhoneNumber` shows in TG-2).
fn is_phone_step(step: &kourier_model::LoginStep) -> bool {
    step.step_id == "phone" && matches!(step.kind, kourier_model::LoginStepKind::Input { .. })
}

const T0: u64 = 1_800_000_000_000;

/// T-s4 (child): TDLib aborts during the start (SIGABRT at the parameters): the account is
/// `Offline` with a retry at least 5 s ahead on the actor's clock, NO second helper starts before
/// that clock gets there, then the second helper reaches the phone-number step.
#[test]
fn an_abort_restarts_the_helper_after_the_back_off() {
    use kourier_backend_api::{Command, Exit, FakeClock};
    use kourier_model::ConnState;

    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["abort", "clean"], spawns.clone());
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(parts, clock.clone(), &rt, dir.path());
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let at = tokio::time::timeout(
            Duration::from_secs(10),
            ends.conn.wait_for(|c| {
                matches!(
                    c,
                    ConnState::Offline {
                        retry_at_ms: Some(_)
                    }
                )
            }),
        )
        .await
        .expect("no restart was scheduled")
        .map(|c| match *c {
            ConnState::Offline {
                retry_at_ms: Some(at),
            } => at,
            _ => 0,
        })
        .expect("the actor ended");
        assert!(
            at >= T0 + 5_000,
            "the restart is only {} ms ahead",
            at.saturating_sub(T0)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "a helper was restarted before the back-off"
        );
        clock.set(at);
        let step = first_step(&mut ends).await;
        assert!(
            is_phone_step(&step),
            "the second helper did not reach the phone step"
        );
        assert_eq!(spawns.load(Ordering::SeqCst), 2, "not exactly two helpers");
        assert_eq!(
            *ends.conn.borrow(),
            ConnState::Offline { retry_at_ms: None }
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// T-s8, the no-loop half: a helper without its library is answered at once as unsupported and
/// is NOT restarted (30 s of the actor's clock later there is still one helper, no retry).
#[test]
fn a_missing_library_is_typed_and_does_not_loop() {
    use kourier_backend_api::{Command, Exit, FakeClock};
    use kourier_model::ConnState;

    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["nolib"], spawns.clone());
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(parts, clock.clone(), &rt, dir.path());
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        assert_eq!(login_detail(&mut ends).await, crate::actor::NO_LIBRARY);
        clock.advance(Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "the helper was restarted");
        assert!(
            !matches!(
                *ends.conn.borrow(),
                ConnState::Offline {
                    retry_at_ms: Some(_)
                }
            ),
            "a restart was scheduled"
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

// ----- G12..G15: the actor over the fake helper -----

/// The step id and the detail of the next login step (10 s at most).
async fn login_step(ends: &mut kourier_backend_api::CoreEnds) -> (String, String) {
    use kourier_backend_api::BackendEvent;
    use kourier_model::LoginStepKind;
    let envelope = tokio::time::timeout(Duration::from_secs(10), ends.events.recv())
        .await
        .expect("no login step in time")
        .expect("events ended");
    match &envelope.events[..] {
        [BackendEvent::Login(step)] => match &step.kind {
            LoginStepKind::Failed { error } => (step.step_id.clone(), error.detail.clone()),
            _ => panic!("the login step is not a failure"),
        },
        _ => panic!("not exactly one login step"),
    }
}

/// Waits (10 s at most) until `f` holds.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let by = StdInstant::now() + Duration::from_secs(10);
    while !f() {
        assert!(StdInstant::now() < by, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether the helper of `pid` is gone: reaped, so `/proc/<pid>` does not exist.
fn reaped(pid: u32) -> bool {
    !Path::new(&format!("/proc/{pid}")).exists()
}

/// The next retry time (the clock's ms) after `after`, from the account's connection state.
async fn next_retry(ends: &mut kourier_backend_api::CoreEnds, after: u64) -> u64 {
    use kourier_model::ConnState;
    let seen = tokio::time::timeout(
        Duration::from_secs(10),
        ends.conn
            .wait_for(|c| matches!(c, ConnState::Offline { retry_at_ms: Some(at) } if *at > after)),
    )
    .await
    .expect("no restart was scheduled")
    .expect("the actor ended");
    match *seen {
        ConnState::Offline {
            retry_at_ms: Some(at),
        } => at,
        _ => unreachable!("waited for a retry time"),
    }
}

/// G14: a Shutdown during a start (the helper never says hello) ends the actor at once, and the
/// helper is killed and reaped; it does not wait out the hello's 5 s. (Mutant: the commands are
/// not served during the start.)
#[test]
fn a_shutdown_during_a_start_ends_the_actor_and_reaps_the_helper() {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let spawns = Arc::new(AtomicUsize::new(0));
    let pids = Arc::new(Mutex::new(Vec::new()));
    let parts = child_parts_logged(&["hang-hello"], spawns.clone(), pids.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, ends) = actor_on(parts, Arc::new(FakeClock::new(T0)), &rt, dir.path());
    let t0 = rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        until("the helper to start", || spawns.load(Ordering::SeqCst) == 1).await;
        let t0 = StdInstant::now();
        ends.commands.send(Command::Shutdown).await.expect("queued");
        t0
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "the shutdown waited for the start: {:?}",
        t0.elapsed()
    );
    let pids = pids.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(pids.len(), 1);
    assert!(pids.iter().all(|p| reaped(*p)), "the helper was not reaped");
}

/// G14: a Logout during a start is told (the Logout event) and ends the actor logged out, the helper
/// killed and reaped, at once.
#[test]
fn a_logout_during_a_start_ends_the_actor_logged_out_and_reaps_the_helper() {
    use kourier_backend_api::{BackendEvent, Command, Exit, FakeClock};

    let spawns = Arc::new(AtomicUsize::new(0));
    let pids = Arc::new(Mutex::new(Vec::new()));
    let parts = child_parts_logged(&["hang-hello"], spawns.clone(), pids.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(parts, Arc::new(FakeClock::new(T0)), &rt, dir.path());
    let (t0, event) = rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        until("the helper to start", || spawns.load(Ordering::SeqCst) == 1).await;
        let t0 = StdInstant::now();
        ends.commands.send(Command::Logout).await.expect("queued");
        let envelope = tokio::time::timeout(Duration::from_secs(10), ends.events.recv())
            .await
            .expect("no event in time")
            .expect("events ended");
        (t0, envelope.events.into_iter().next())
    });
    assert!(
        matches!(event, Some(BackendEvent::Logout(_))),
        "the logout was not told"
    );
    assert!(matches!(
        actor.join().expect("actor"),
        Exit::LoggedOut { .. }
    ));
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "the logout waited for the start: {:?}",
        t0.elapsed()
    );
    let pids = pids.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(pids.iter().all(|p| reaped(*p)), "the helper was not reaped");
}

/// G14: a CancelLogin during a start cancels it WITHOUT a step (the helper is reaped), and the
/// actor goes on: the next login starts a new helper and is answered.
#[test]
fn a_cancel_login_during_a_start_ends_it_without_a_step() {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let spawns = Arc::new(AtomicUsize::new(0));
    let pids = Arc::new(Mutex::new(Vec::new()));
    let parts = child_parts_logged(&["hang-hello", "clean"], spawns.clone(), pids.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(parts, Arc::new(FakeClock::new(T0)), &rt, dir.path());
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        until("the helper to start", || spawns.load(Ordering::SeqCst) == 1).await;
        // `spawns` counts the maker's call, the pid is pushed only after the child exists (U11,
        // strike 1: at a load of 176 `pids[0]` ran on an empty list): wait for the pid itself.
        until("the helper's pid", || {
            !pids.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
        })
        .await;
        ends.commands
            .send(Command::CancelLogin)
            .await
            .expect("queued");
        let first = pids.lock().unwrap_or_else(|p| p.into_inner())[0];
        until("the helper to be reaped", || reaped(first)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            ends.events.try_recv().is_err(),
            "the cancelled login was answered"
        );
        // The actor lives: a new login starts a new helper and shows its first step.
        ends.commands.send(start_login()).await.expect("queued");
        let step = first_step(&mut ends).await;
        assert!(
            is_phone_step(&step),
            "the new login did not reach the phone step"
        );
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// G14: the other commands are served during a start: a request with a reply is answered at once
/// (not after the start), and a second login during the start is DROPPED (TG-2 C8b: the first
/// login's session goes on): no second helper, the first login's `phone` step comes.
#[test]
fn the_other_commands_are_served_during_a_start() {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let spawns = Arc::new(AtomicUsize::new(0));
    let parts = child_parts(&["late"], spawns.clone());
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let wiped_before = crate::login::wiped();
    let (actor, mut ends) = actor_on(parts, Arc::new(FakeClock::new(T0)), &rt, dir.path());
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        until("the helper to start", || spawns.load(Ordering::SeqCst) == 1).await;
        let t0 = StdInstant::now();
        let (reply, answer) = tokio::sync::oneshot::channel();
        ends.commands
            .send(Command::ListOwnDevices { reply })
            .await
            .expect("queued");
        let answered = tokio::time::timeout(Duration::from_millis(1000), answer)
            .await
            .expect("the request waited for the start")
            .expect("the request was dropped");
        assert!(answered.is_err(), "the request was not refused");
        assert!(t0.elapsed() < Duration::from_millis(1000));
        // An input during the start is dropped unread, and wiped (J14).
        ends.commands
            .send(typed_input("12345"))
            .await
            .expect("queued");
        ends.commands
            .send(Command::StartLogin {
                flow: "qr".to_owned(),
                relink: None,
                taken: Vec::new(),
            })
            .await
            .expect("queued");
        let step = first_step(&mut ends).await;
        assert!(
            is_phone_step(&step),
            "the second login during the start was not dropped"
        );
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "a second helper started");
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
    assert!(
        crate::login::wiped() > wiped_before,
        "an input during the start was not wiped"
    );
}

/// A `SubmitLoginInput` of step 1 carrying a synthetic code.
fn typed_input(code: &str) -> kourier_backend_api::Command {
    kourier_backend_api::Command::SubmitLoginInput {
        txn: 1,
        values: vec![(
            "code".to_owned(),
            kourier_model::Redacted::new(code.to_owned()),
        )],
    }
}

/// G14, during a back-off: a Shutdown ends the actor at once; a CancelLogin drops the restart (the
/// state is `Offline` without a retry time and no helper starts when the clock gets there).
#[test]
fn commands_during_a_back_off_are_served() {
    use kourier_backend_api::{Command, Exit, FakeClock};
    use kourier_model::ConnState;

    // Shutdown.
    let spawns = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(
        child_parts(&["abort", "clean"], spawns.clone()),
        clock,
        &rt,
        dir.path(),
    );
    let before = crate::login::wiped();
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        next_retry(&mut ends, 0).await;
        // An input while the restart waits is dropped unread, and wiped (J14).
        ends.commands
            .send(typed_input("12345"))
            .await
            .expect("queued");
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
    assert!(
        crate::login::wiped() > before,
        "an input during the back-off was not wiped"
    );

    // CancelLogin.
    let spawns = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let (actor, mut ends) = actor_on(
        child_parts(&["abort", "clean"], spawns.clone()),
        clock.clone(),
        &rt,
        dir.path(),
    );
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let at = next_retry(&mut ends, 0).await;
        ends.commands
            .send(Command::CancelLogin)
            .await
            .expect("queued");
        tokio::time::timeout(
            Duration::from_secs(5),
            ends.conn
                .wait_for(|c| *c == ConnState::Offline { retry_at_ms: None }),
        )
        .await
        .expect("the restart was not dropped")
        .expect("the actor ended");
        clock.set(at);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "a dropped restart ran");
        assert!(ends.events.try_recv().is_err(), "a step was answered");
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// G13: abort, a start that worked, abort: the second abort waits 5 s again, not 10.
#[test]
fn a_start_that_worked_starts_the_back_off_over() {
    use kourier_backend_api::{Command, Exit, FakeClock};

    let spawns = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(
        child_parts(&["abort", "clean", "abort", "clean"], spawns.clone()),
        clock.clone(),
        &rt,
        dir.path(),
    );
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let first = next_retry(&mut ends, 0).await;
        assert_eq!(first, T0 + 5_000, "the first wait is not 5 s");
        clock.set(first);
        assert!(is_phone_step(&first_step(&mut ends).await));
        // TG-2: the login's session runs on (a second StartLogin would be ignored): cancel it, and
        // the next login kills its idle helper first. The third start aborts: the wait is 5 s
        // again (not the 10 s of the second abort).
        ends.commands
            .send(Command::CancelLogin)
            .await
            .expect("queued");
        ends.commands.send(start_login()).await.expect("queued");
        let second = next_retry(&mut ends, first).await;
        assert_eq!(second, first + 5_000, "the back-off did not start over");
        clock.set(second);
        assert!(is_phone_step(&first_step(&mut ends).await));
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// TDLib's text for a database another helper holds (`FileFd.cpp`), with a synthetic path.
const IN_USE_TEXT: &str = "Can't lock file \"/synthetic/tdlib/td.binlog\", because it is already in use; check for another program instance running";

/// G12 (TG-2: critic L2 and B1): a database that stays in use is told to the person at the SECOND
/// try in a row as a NOTICE (a `Failed` step would end the login), not at the first, nor again;
/// the restarts go on; the fifth in a row uses the login's restart budget up: the login ends
/// `Failed{Transient}` "the library keeps stopping", and no restart follows. A text without
/// "already in use" is no such database: the login is answered at once, not retried.
#[test]
fn a_database_in_use_is_told_at_the_second_try_and_the_cap_ends_the_login() {
    use kourier_backend_api::{BackendEvent, BackendNotice, Command, Exit, FakeClock};

    let p = in_process(Script {
        parameters_error: Some((400, IN_USE_TEXT.to_owned())),
        ..Script::default()
    });
    let clock = Arc::new(FakeClock::new(T0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(p.parts.clone(), clock.clone(), &rt, dir.path());
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let mut at = 0;
        let mut told = Vec::new();
        for attempt in 1..=4 {
            at = next_retry(&mut ends, at).await;
            // What this attempt told (events are queued before the next restart is awaited).
            tokio::time::sleep(Duration::from_millis(150)).await;
            let mut events = Vec::new();
            while let Ok(envelope) = ends.events.try_recv() {
                events.extend(envelope.events);
            }
            told.push((attempt, events));
            clock.set(at);
        }
        for (attempt, events) in &told {
            if *attempt == 2 {
                match events.as_slice() {
                    [BackendEvent::Notice(BackendNotice::Error(error))] => {
                        assert_eq!(error.detail, crate::actor::DATABASE_IN_USE);
                        assert!(
                            matches!(error.kind, kourier_model::ErrorKind::Transient { .. }),
                            "the notice is not transient"
                        );
                    }
                    other => panic!("attempt 2 told {} events, not the notice", other.len()),
                }
            } else {
                assert!(
                    events.is_empty(),
                    "attempt {attempt} told {} events",
                    events.len()
                );
            }
        }
        // The fifth try in a row: the login ends; no restart is scheduled.
        let step = first_step(&mut ends).await;
        match &step.kind {
            kourier_model::LoginStepKind::Failed { error } => {
                assert_eq!(error.detail, crate::actor::KEEPS_STOPPING);
                assert!(
                    matches!(error.kind, kourier_model::ErrorKind::Transient { .. }),
                    "the end is not transient"
                );
            }
            _ => panic!("the fifth try did not end the login"),
        }
        assert_eq!(p.spawns.load(Ordering::SeqCst), 5);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            *ends.conn.borrow(),
            kourier_model::ConnState::Offline { retry_at_ms: None },
            "a restart was scheduled after the login ended"
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);

    // The bare "Can't lock file" is a permanent condition: answered at once, never retried.
    let p = in_process(Script {
        parameters_error: Some((
            500,
            "Can't lock file \"/synthetic/td.binlog\": No locks".to_owned(),
        )),
        ..Script::default()
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let (actor, mut ends) = actor_on(
        p.parts.clone(),
        Arc::new(FakeClock::new(T0)),
        &rt,
        dir.path(),
    );
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let (_, detail) = login_step(&mut ends).await;
        assert!(!detail.contains("synthetic"), "the path reached the answer");
        assert_eq!(
            p.spawns.load(Ordering::SeqCst),
            1,
            "a permanent lock was retried"
        );
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// G15: a helper that is not this build's (another TDLib commit in its hello) is answered as
/// unsupported, with a fixed text, and is not retried.
#[test]
fn a_helper_of_another_build_is_unsupported_not_transient() {
    use kourier_backend_api::{Command, Exit, FakeClock};
    use kourier_model::ErrorKind;

    let spawns = Arc::new(AtomicUsize::new(0));
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = rt();
    let (actor, mut ends) = actor_on(
        child_parts(&["wrong"], spawns.clone()),
        Arc::new(FakeClock::new(T0)),
        &rt,
        dir.path(),
    );
    rt.block_on(async {
        ends.commands.send(start_login()).await.expect("queued");
        let envelope = tokio::time::timeout(Duration::from_secs(10), ends.events.recv())
            .await
            .expect("no login step in time")
            .expect("events ended");
        match &envelope.events[..] {
            [kourier_backend_api::BackendEvent::Login(step)] => match &step.kind {
                kourier_model::LoginStepKind::Failed { error } => {
                    assert_eq!(error.kind, ErrorKind::Unsupported);
                    assert_eq!(error.detail, crate::actor::WRONG_HELPER);
                }
                _ => panic!("not a failed step"),
            },
            _ => panic!("not exactly one login step"),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "the helper was retried");
        ends.commands.send(Command::Shutdown).await.expect("queued");
    });
    assert_eq!(actor.join().expect("actor"), Exit::Shutdown);
}

/// A start of the in-process helper with `script`, short timings and the start bound `bound`.
fn start_with(script: Script, bound: Duration) -> (Result<FirstState, StartError>, InProcess) {
    let p = in_process(script);
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let t = Timings {
        start: bound,
        ..short()
    };
    let first = rt.block_on(async {
        match start(
            &p.parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &t,
            &cancel,
        )
        .await
        {
            Ok(mut helper) => {
                let first = helper.take_first();
                let again = helper.take_first();
                helper.stop(&t, &cancel).await;
                assert!(again.is_none(), "the first state was handed over twice");
                first.ok_or(StartError::UnexpectedState)
            }
            Err(e) => Err(e),
        }
    });
    (first, p)
}

/// The name of an update's authorization state, `None` for any other update (tests only).
fn state_name(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    (v["@type"] == "updateAuthorizationState")
        .then(|| {
            v["authorization_state"]["@type"]
                .as_str()
                .map(str::to_owned)
        })
        .flatten()
}

/// TG-2 C5 (TDLib resumes a login from its binlog, `AuthManager.cpp:98-160, 304-335`): the start
/// hands over ANY login state, `Ready` and `LoggingOut` as its first state, with the state's own
/// update among the frames it kept, and only once.
#[test]
fn a_start_hands_over_any_first_state() {
    for (name, want) in [
        (
            "authorizationStateWaitPhoneNumber",
            AuthKind::WaitPhoneNumber,
        ),
        ("authorizationStateWaitCode", AuthKind::WaitCode),
        ("authorizationStateWaitPassword", AuthKind::WaitPassword),
        (
            "authorizationStateWaitEmailAddress",
            AuthKind::WaitEmailAddress,
        ),
        ("authorizationStateWaitEmailCode", AuthKind::WaitEmailCode),
        (
            "authorizationStateWaitOtherDeviceConfirmation",
            AuthKind::WaitOtherDevice,
        ),
        (
            "authorizationStateWaitRegistration",
            AuthKind::WaitRegistration,
        ),
        (
            "authorizationStateWaitPremiumPurchase",
            AuthKind::WaitPremiumPurchase,
        ),
        ("authorizationStateReady", AuthKind::Ready),
        ("authorizationStateLoggingOut", AuthKind::LoggingOut),
    ] {
        let (first, _p) = start_with(
            Script {
                after_parameters: name.to_owned(),
                ..Script::default()
            },
            Duration::from_secs(10),
        );
        let first = first.unwrap_or_else(|e| panic!("{name}: the start failed: {e:?}"));
        assert_eq!(first.state, want, "{name}");
        let states: Vec<String> = first.frames.iter().filter_map(|f| state_name(f)).collect();
        assert_eq!(
            states,
            vec![name.to_owned()],
            "{name}: the kept frames do not hold the first state once (and no parameters state)"
        );
        assert_eq!(first.dropped, 0);
    }
}

/// TG-2 C5: `Closing` (and `Closed`) at the first state end the start (an abort: the next start
/// sees what TDLib left); a state this build does not know is unexpected (never retried); a client
/// that stays in WaitTdlibParameters keeps the start waiting until its bound.
#[test]
fn a_closing_first_state_is_an_ended_start() {
    for (name, want) in [
        ("authorizationStateClosing", StartError::Ended),
        ("authorizationStateClosed", StartError::Ended),
        (
            "authorizationStateSomethingNew",
            StartError::UnexpectedState,
        ),
    ] {
        let (first, _p) = start_with(
            Script {
                after_parameters: name.to_owned(),
                ..Script::default()
            },
            Duration::from_secs(10),
        );
        assert_eq!(first.err(), Some(want), "{name}");
    }
    assert_eq!(StartError::Ended.retry(), Retry::Abort);
    assert_eq!(StartError::UnexpectedState.retry(), Retry::Never);
    let began = StdInstant::now();
    let (first, _p) = start_with(
        Script {
            after_parameters: "authorizationStateWaitTdlibParameters".to_owned(),
            ..Script::default()
        },
        Duration::from_secs(1),
    );
    assert_eq!(first.err(), Some(StartError::Timeout));
    assert!(
        began.elapsed() >= Duration::from_secs(1),
        "the start gave up before its bound"
    );
}

/// TG-2 M3: the updates TDLib sends during the start (an early connection state, a stored freeze,
/// arriving while the parameters are applied) are handed over with the first state, in order, so
/// the session does not sit in `Connecting` or miss the freeze.
#[test]
fn the_start_hands_over_the_updates_it_saw() {
    let script = Script {
        hold_parameters: true,
        after_parameters: "authorizationStateReady".to_owned(),
        ..Script::default()
    };
    let p = in_process(script);
    let td = p.td.clone();
    let finisher = std::thread::spawn(move || {
        let until = StdInstant::now() + Duration::from_secs(10);
        let mut pushed = false;
        while StdInstant::now() < until {
            let client = td
                .requests()
                .iter()
                .find(|(_, r)| r["@type"] == "setTdlibParameters")
                .map(|(c, _)| *c);
            if let Some(client) = client {
                if !pushed {
                    td.push_update(json!({"@type": "updateConnectionState",
                        "state": {"@type": "connectionStateReady"}}));
                    td.push_update(json!({"@type": "updateFreezeState", "is_frozen": true,
                        "freezing_date": 1, "deletion_date": 2,
                        "appeal_link": "https://example.invalid/appeal"}));
                    pushed = true;
                    // The burst goes out before the parameters' `ok`.
                    std::thread::sleep(Duration::from_millis(100));
                }
                if td.finish_parameters(client) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    });
    let a = account();
    let cancel = CancellationToken::new();
    let rt = rt();
    let first = rt.block_on(async {
        let mut helper = start(
            &p.parts,
            &start_of(&a, TgEnv::Production),
            rt.handle(),
            &short(),
            &cancel,
        )
        .await
        .expect("the start failed");
        let first = helper.take_first().expect("no first state");
        helper.stop(&short(), &cancel).await;
        first
    });
    assert!(
        finisher.join().expect("finisher"),
        "the parameters were never held"
    );
    assert_eq!(first.state, AuthKind::Ready);
    let updates: Vec<crate::tdapi::Update> = first
        .frames
        .iter()
        .filter_map(|f| crate::tdapi::parse_update(f))
        .collect();
    let conn = updates.iter().position(|u| {
        *u == crate::tdapi::Update::ConnectionState(crate::tdapi::ConnectionState::Ready)
    });
    let freeze = updates
        .iter()
        .position(|u| matches!(u, crate::tdapi::Update::FreezeState(f) if f.is_frozen));
    let ready = updates.iter().position(|u| {
        matches!(
            u,
            crate::tdapi::Update::AuthorizationState(crate::tdapi::AuthorizationState::Ready)
        )
    });
    assert!(
        conn.is_some() && freeze.is_some() && ready.is_some(),
        "the start's updates were not handed over"
    );
    assert!(
        conn < freeze && freeze < ready,
        "the kept updates are out of order"
    );
    // Nothing of the frames in the Debug text.
    let text = format!("{first:?}");
    assert!(
        !text.contains("example.invalid") && !text.contains("update"),
        "{text}"
    );
}

/// TG-2 M3: the kept updates are bounded; the rest is counted.
#[test]
fn the_kept_updates_are_bounded_and_the_rest_counted() {
    let mut progress = Progress::default();
    for n in 0..(FIRST_FRAMES + 3) {
        progress.see(
            1,
            Inbound::Update(format!("{{\"@type\":\"updateX\",\"n\":{n}}}")),
        );
    }
    progress.see(
        1,
        Inbound::Update(
            json!({"@type": "updateAuthorizationState",
                "authorization_state": {"@type": "authorizationStateWaitTdlibParameters"}})
            .to_string(),
        ),
    );
    progress.see(
        1,
        Inbound::Update(
            json!({"@type": "updateAuthorizationState",
                "authorization_state": {"@type": "authorizationStateWaitCode"}})
            .to_string(),
        ),
    );
    assert_eq!(progress.state, Some(AuthKind::WaitCode));
    let first = progress.into_first().expect("first");
    assert_eq!(first.frames.len(), FIRST_FRAMES);
    // The parameters state is not kept or counted; the code state is counted (the bound was met).
    assert_eq!(first.dropped, 4);
    assert_eq!(first.state, AuthKind::WaitCode);
}
