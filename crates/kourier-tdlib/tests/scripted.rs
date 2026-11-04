//! The scripted TDLib: TDLib's request parking before the parameters (decisions D3 B1), its proxy
//! list and the proxies it refuses, its close quirk, and its abort on two receivers (TG-1 plan §5.4
//! T-a8, scripted half); and, from the TG-1a fix round (F4), the places where it must not differ
//! from TDLib: a refused first `setTdlibParameters`, synchronous requests, requests during the
//! application of the parameters, `401` before Ready, an unknown client id, a closing client.

use std::ffi::CString;
use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

use kourier_tdlib::scripted::{Call, Rule, Script, ScriptedTd};
use kourier_tdlib::td::{Engine, split};
use serde_json::{Value, json};

const CHILD_ENV: &str = "KOURIER_TDLIB_TEST_CHILD";

fn send(td: &ScriptedTd, client: i32, v: Value) {
    td.send(client, &CString::new(v.to_string()).unwrap());
}

/// Everything in the queue now, parsed.
fn drain(td: &ScriptedTd) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(line) = td.receive(0.0) {
        out.push(serde_json::from_str(&line).unwrap());
    }
    out
}

fn types(vs: &[Value]) -> Vec<String> {
    vs.iter()
        .map(|v| {
            let t = v["@type"].as_str().unwrap_or_default();
            match v["authorization_state"]["@type"].as_str() {
                Some(s) => format!("{t}:{s}"),
                None => t.to_owned(),
            }
        })
        .collect()
}

fn gate_proxy(port: i64, extra: u64) -> Value {
    json!({
        "@type": "addProxy",
        "proxy": {"@type": "proxy", "server": "127.0.0.1", "port": port,
                  "type": {"@type": "proxyTypeSocks5", "username": "u", "password": "p"}},
        "enable": true,
        "comment": "",
        "@extra": extra,
    })
}

/// Before `setTdlibParameters` TDLib answers a pre-authentication request (`addProxy`) only AFTER
/// the parameters, in its own order, and refuses everything outside both lists with 400.
#[test]
fn pre_authentication_requests_wait_for_the_parameters_in_tdlibs_order() {
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule::new(
                "getCountryCode",
                vec![json!({"@type": "text", "text": "XX"})],
            ),
            Rule::new("testCallEmpty", vec![json!({"@type": "ok"})]),
        ],
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, json!({"@type": "getCountryCode", "@extra": 1}));
    send(&td, c, gate_proxy(40000, 2));
    send(&td, c, json!({"@type": "getMe", "@extra": 3}));
    send(&td, c, json!({"@type": "testCallEmpty", "@extra": 4}));
    let before = drain(&td);
    assert_eq!(
        types(&before),
        [
            "updateAuthorizationState:authorizationStateWaitTdlibParameters",
            "error",
            "ok"
        ],
        "only the refusal and the pre-initialization answer come before the parameters"
    );
    assert_eq!(before[1]["@extra"], 3);
    assert_eq!(before[1]["code"], 400);
    assert_eq!(before[2]["@extra"], 4);
    assert!(
        before.iter().all(|v| v["@client_id"] == c),
        "every line carries its client"
    );
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 5}));
    let after = drain(&td);
    assert_eq!(
        types(&after),
        [
            "ok",
            "addedProxy",
            "text",
            "updateAuthorizationState:authorizationStateWaitPhoneNumber"
        ],
        "the parameters' ok, then the parked proxy request first, then the rest, then the state"
    );
    let extras: Vec<&Value> = after.iter().map(|v| &v["@extra"]).collect();
    assert_eq!(extras[..3], [&json!(5), &json!(2), &json!(1)]);
    assert_eq!(after[1]["is_enabled"], true);
    assert_eq!(after[1]["proxy"]["port"], 40000);
    assert!(after[3].get("@extra").is_none(), "an update has no @extra");
}

/// Waiting for the `addProxy` answer before sending the parameters waits forever (the B1
/// deadlock): nothing answers it.
#[test]
fn an_add_proxy_alone_is_never_answered() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    send(&td, c, gate_proxy(40000, 1));
    let got = drain(&td);
    assert_eq!(
        types(&got),
        ["updateAuthorizationState:authorizationStateWaitTdlibParameters"]
    );
    assert_eq!(
        td.receive(0.2),
        None,
        "addProxy was answered before the parameters"
    );
}

/// A proxy that differs from every stored one gets a new id (TDLib's binlog), so every start with
/// fresh gate credentials adds one; `getProxies` and `removeProxy` see the list.
#[test]
fn every_new_proxy_gets_a_new_id_and_the_list_outlives_a_client() {
    let td = ScriptedTd::new(Script::default());
    for (port, start) in [(40001, 0), (40002, 10), (40001, 20)] {
        let c = td.create_client();
        send(&td, c, gate_proxy(port, start + 1));
        send(
            &td,
            c,
            json!({"@type": "setTdlibParameters", "@extra": start + 2}),
        );
        drain(&td);
    }
    let list = td.proxies();
    assert_eq!(list.len(), 2, "the same proxy twice is one entry");
    assert_eq!(list.iter().filter(|p| p.3).count(), 1, "one enabled");
    assert!(
        list.iter().any(|p| p.2 == 40001 && p.3),
        "the last one added is enabled"
    );
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    send(
        &td,
        c,
        json!({"@type": "removeProxy", "proxy_id": list[1].0, "@extra": 2}),
    );
    send(&td, c, json!({"@type": "getProxies", "@extra": 3}));
    let got = drain(&td);
    let proxies = got.iter().find(|v| v["@type"] == "addedProxies").unwrap();
    assert_eq!(proxies["proxies"].as_array().unwrap().len(), 1);
}

/// `close`: ok, Closing, ONE None (TDLib's quirk), then Closed; with `hang_on_close` never Closed.
#[test]
fn close_returns_one_null_before_closed() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    drain(&td);
    send(&td, c, json!({"@type": "close", "@extra": 2}));
    let first = drain(&td);
    assert_eq!(
        types(&first),
        ["ok", "updateAuthorizationState:authorizationStateClosing"]
    );
    let rest = drain(&td);
    assert_eq!(
        types(&rest),
        ["updateAuthorizationState:authorizationStateClosed"],
        "Closed comes after the one NULL"
    );
    let hang = ScriptedTd::new(Script {
        hang_on_close: true,
        ..Script::default()
    });
    let c = hang.create_client();
    send(&hang, c, json!({"@type": "close", "@extra": 1}));
    let all: Vec<String> = types(&drain(&hang))
        .into_iter()
        .chain(types(&drain(&hang)))
        .collect();
    assert!(
        !all.iter().any(|t| t.ends_with("authorizationStateClosed")),
        "a hanging client closed: {all:?}"
    );
}

#[test]
fn execute_answers_the_log_calls_and_the_version() {
    let td = ScriptedTd::new(Script::default());
    let ex = |v: Value| -> Value {
        serde_json::from_str(&td.execute(&CString::new(v.to_string()).unwrap()).unwrap()).unwrap()
    };
    assert_eq!(
        ex(json!({"@type":"setLogStream","log_stream":{"@type":"logStreamEmpty"}}))["@type"],
        "ok"
    );
    assert_eq!(
        ex(json!({"@type":"getOption","name":"commit_hash"}))["value"],
        Script::default().commit
    );
    // K5: patch 0002's synchronous option; a script without the patch answers the error every
    // other asynchronous option gets.
    assert_eq!(
        ex(json!({"@type":"getOption","name":"kourier_patch_level"}))["value"],
        "0002"
    );
    assert_eq!(ex(json!({"@type":"getMe"}))["@type"], "error");
    assert_eq!(
        td.calls(),
        [
            Call::Execute("setLogStream".into()),
            Call::Execute("getOption".into()),
            Call::Execute("getOption".into()),
            Call::Execute("getMe".into())
        ]
    );
    let stock = ScriptedTd::new(Script {
        patch_level: String::new(),
        ..Script::default()
    });
    let answer = stock
        .execute(
            &CString::new(json!({"@type":"getOption","name":"kourier_patch_level"}).to_string())
                .unwrap(),
        )
        .unwrap();
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!(
        answer["@type"], "error",
        "a stock library knows no such option"
    );
}

/// The child half of `two_receivers_abort_like_tdlib`: two threads in `receive` at once.
#[test]
fn two_receivers_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let td = ScriptedTd::new(Script::default());
    let (_handle, mut rx) = split(td.clone());
    let t = std::thread::spawn(move || {
        rx.receive(10.0);
    });
    while !td.is_receiving() {
        std::thread::sleep(Duration::from_millis(5));
    }
    // A second receiver: what `split` forbids by type, done here through a clone of the engine.
    td.receive(1.0);
    t.join().unwrap();
    // Not reached: the second receive aborts.
    std::process::exit(0);
}

/// Two threads inside TDLib's receive at once abort the process (spike §4): the scripted engine
/// does the same, so a second receiver fails a test instead of passing it.
#[test]
fn two_receivers_abort_like_tdlib() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scripted::two_receivers_child", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("start the child");
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGABRT),
        "the child did not abort: {:?}",
        out.status
    );
}

/// The variable that makes [`processed_log_child`] run, holding its log's path.
const PROCESSED_LOG_ENV: &str = "KOURIER_TDLIB_TEST_PROCESSED_LOG";
/// How many updates the child pushes and receives before its SIGKILL.
const PROCESSED: i64 = 3;

/// A synthetic pushed update (an id only).
fn pushed(k: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": k << 20}})
}

/// The child half of `the_processed_log_survives_a_sigkill_of_the_child`: pushes and receives
/// [`PROCESSED`] updates with a processed log, then SIGKILLs itself the moment it holds the last
/// one (no destructor, no flush runs).
#[test]
fn processed_log_child() {
    let Some(path) = std::env::var_os(PROCESSED_LOG_ENV) else {
        return;
    };
    let td = ScriptedTd::new(Script {
        processed_log: Some(path.into()),
        ..Script::default()
    });
    for k in 1..=PROCESSED {
        td.push_update(pushed(k));
    }
    for _ in 1..=PROCESSED {
        assert!(td.receive(5.0).is_some(), "a pushed update");
    }
    // SAFETY: a signal to this very process; nothing runs after it.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    std::process::exit(0);
}

/// TG-3 C10a (critic M8): the processed log holds every pushed update the child had RECEIVED when
/// it was SIGKILLed, the last one included (written and synced before `receive` hands it out:
/// TDLib's pts save before the emit), in order; a rule's reply is not in it. A missing log reads
/// as empty.
#[test]
fn the_processed_log_survives_a_sigkill_of_the_child() {
    // No tempfile here (no new dependency): a per-process name under the test's TMPDIR.
    let path = std::env::temp_dir().join(format!(
        "kourier-processed-log-{}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    assert!(kourier_tdlib::scripted::processed(&path).is_empty());
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scripted::processed_log_child", "--nocapture"])
        .env(PROCESSED_LOG_ENV, &path)
        .output()
        .expect("start the child");
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGKILL),
        "the child was not killed: {:?}",
        out.status
    );
    let logged = kourier_tdlib::scripted::processed(&path);
    let _ = std::fs::remove_file(&path);
    let want: Vec<Value> = (1..=PROCESSED).map(pushed).collect();
    assert_eq!(
        logged, want,
        "the processed log lost an update the client held"
    );
}

fn gate_proxy_with(server: &str, port: i64, user: &str, pass: &str, extra: u64) -> Value {
    json!({
        "@type": "addProxy",
        "proxy": {"@type": "proxy", "server": server, "port": port,
                  "type": {"@type": "proxyTypeSocks5", "username": user, "password": pass}},
        "enable": true,
        "comment": "",
        "@extra": extra,
    })
}

/// Fix F1: TDLib's `Proxy::create_proxy` refuses port 0 (and the ones over 65535), an empty server
/// and a proxy without a type; the script also refuses a login of 128 bytes, which TDLib takes and
/// then fails every connection with. A good proxy is still stored.
#[test]
fn an_add_proxy_tdlib_would_refuse_is_answered_400_and_stores_nothing() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    let long = "x".repeat(128);
    let bad = [
        gate_proxy_with("127.0.0.1", 0, "u", "p", 1),
        gate_proxy_with("127.0.0.1", 65536, "u", "p", 2),
        gate_proxy_with("", 40000, "u", "p", 3),
        gate_proxy_with("127.0.0.1", 40000, &long, "p", 4),
        gate_proxy_with("127.0.0.1", 40000, "u", &long, 5),
        json!({"@type": "addProxy", "proxy": null, "enable": true, "@extra": 6}),
    ];
    for r in bad {
        send(&td, c, r);
    }
    send(&td, c, gate_proxy_with("127.0.0.1", 40000, "u", "p", 7));
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 8}));
    let got = drain(&td);
    let answers: Vec<(u64, String)> = got
        .iter()
        .filter(|v| v["@type"] != "updateAuthorizationState" && v["@extra"].as_u64() != Some(8))
        .map(|v| {
            (
                v["@extra"].as_u64().unwrap(),
                v["@type"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        answers,
        [
            (1, "error".to_owned()),
            (2, "error".to_owned()),
            (3, "error".to_owned()),
            (4, "error".to_owned()),
            (5, "error".to_owned()),
            (6, "error".to_owned()),
            (7, "addedProxy".to_owned())
        ]
    );
    assert!(
        got.iter()
            .filter(|v| v["@type"] == "error")
            .all(|v| v["code"] == 400),
        "a refusal is not a 400"
    );
    assert_eq!(td.proxies().len(), 1, "a refused proxy was stored");
}

/// Fix F4: TDLib refuses a bad `setTdlibParameters` and stays in WaitTdlibParameters WITH the
/// requests it parked (`Td.cpp:327-331`); the next parameters are taken and answer them.
#[test]
fn refused_parameters_keep_the_parked_requests_for_the_retry() {
    let td = ScriptedTd::new(Script {
        refuse_first_parameters: true,
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, gate_proxy(40000, 2));
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 5}));
    let first = drain(&td);
    assert_eq!(
        types(&first),
        [
            "updateAuthorizationState:authorizationStateWaitTdlibParameters",
            "error"
        ],
        "the refusal also answered the parked proxy"
    );
    assert_eq!(
        (first[1]["@extra"].clone(), first[1]["code"].clone()),
        (json!(5), json!(400))
    );
    assert!(
        td.proxies().is_empty(),
        "the proxy was applied by refused parameters"
    );
    // Still waiting for parameters: an ordinary request is refused as before.
    send(&td, c, json!({"@type": "getMe", "@extra": 6}));
    assert_eq!(drain(&td)[0]["code"], 400);
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 7}));
    let retry = drain(&td);
    assert_eq!(
        types(&retry),
        [
            "ok",
            "addedProxy",
            "updateAuthorizationState:authorizationStateWaitPhoneNumber"
        ],
        "the retry did not take the parked proxy with it"
    );
    assert_eq!(
        (retry[0]["@extra"].clone(), retry[1]["@extra"].clone()),
        (json!(7), json!(2))
    );
    // The refusal is once per client: another client is refused on its own first parameters.
    let other = td.create_client();
    send(
        &td,
        other,
        json!({"@type": "setTdlibParameters", "@extra": 1}),
    );
    assert_eq!(drain(&td).last().unwrap()["code"], 400);
}

/// Fix F4: `td_send` of a synchronous request is answered at once in every state (`Td.cpp:292-295`):
/// before the parameters (where `getOption` of any other name is parked), while they are applied
/// and for a client that is closing.
#[test]
fn synchronous_requests_are_answered_at_once_in_every_state() {
    let td = ScriptedTd::new(Script {
        hold_parameters: true,
        hang_on_close: true,
        ..Script::default()
    });
    let c = td.create_client();
    let version = |extra: u64| json!({"@type": "getOption", "name": "version", "@extra": extra});
    // Before the parameters.
    send(
        &td,
        c,
        json!({"@type": "getOption", "name": "language_pack_database_path", "@extra": 1}),
    );
    send(&td, c, version(2));
    let waiting = drain(&td);
    assert_eq!(
        types(&waiting),
        [
            "updateAuthorizationState:authorizationStateWaitTdlibParameters",
            "optionValueString"
        ]
    );
    assert_eq!(
        (waiting[1]["@extra"].clone(), waiting[1]["value"].clone()),
        (json!(2), json!(Script::default().version))
    );
    // While they are applied: the sync request is answered, the rest waits.
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 3}));
    send(&td, c, version(4));
    send(
        &td,
        c,
        json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 0, "@extra": 5}),
    );
    let applying = drain(&td);
    assert_eq!(types(&applying), ["optionValueString", "ok"]);
    assert_eq!(applying[0]["@extra"], 4);
    assert_eq!(applying[1]["@extra"], 5);
    assert!(td.finish_parameters(c));
    drain(&td);
    // A closing client.
    send(&td, c, json!({"@type": "close", "@extra": 6}));
    drain(&td);
    send(&td, c, version(7));
    let closing = drain(&td);
    assert_eq!(types(&closing), ["optionValueString"]);
    assert_eq!(closing[0]["@extra"], 7);
}

/// N3: `testReturnError` is one of TDLib's synchronous requests (`SynchronousRequests.cpp:107`):
/// answered at once before the parameters with the error it carries, or 404 without one.
#[test]
fn test_return_error_is_answered_at_once_before_the_parameters() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    send(
        &td,
        c,
        json!({"@type": "testReturnError", "error": {"@type": "error", "code": 418, "message": "m"}, "@extra": 1}),
    );
    send(&td, c, json!({"@type": "testReturnError", "@extra": 2}));
    let got = drain(&td);
    assert_eq!(
        types(&got),
        [
            "updateAuthorizationState:authorizationStateWaitTdlibParameters",
            "error",
            "error"
        ]
    );
    assert_eq!(
        (got[1]["code"].clone(), got[1]["@extra"].clone()),
        (json!(418), json!(1))
    );
    assert_eq!(
        (got[2]["code"].clone(), got[2]["@extra"].clone()),
        (json!(404), json!(2))
    );
}

/// Fix F4: while the parameters apply TDLib parks EVERY request, `getAuthorizationState` and
/// `close` included, and runs them after the parameters in arrival order (`Td.cpp:301-304,
/// 822-836`).
#[test]
fn while_the_parameters_apply_every_request_waits_and_is_replayed_after_them() {
    let td = ScriptedTd::new(Script {
        hold_parameters: true,
        rules: vec![Rule::new("testCallEmpty", vec![json!({"@type": "ok"})])],
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    send(
        &td,
        c,
        json!({"@type": "getAuthorizationState", "@extra": 2}),
    );
    send(&td, c, json!({"@type": "testCallEmpty", "@extra": 3}));
    send(&td, c, json!({"@type": "close", "@extra": 4}));
    let during = drain(&td);
    assert_eq!(
        types(&during),
        ["updateAuthorizationState:authorizationStateWaitTdlibParameters"],
        "a request was answered while the parameters were applied"
    );
    assert!(td.finish_parameters(c));
    assert!(!td.finish_parameters(c), "the parameters finished twice");
    // The close ends at TDLib's one NULL (the engine's quirk), so the Closed update is not in this
    // drain.
    let after = drain(&td);
    assert_eq!(
        types(&after),
        [
            "ok",
            "updateAuthorizationState:authorizationStateWaitPhoneNumber",
            "authorizationStateWaitPhoneNumber",
            "ok",
            "ok",
            "updateAuthorizationState:authorizationStateClosing",
        ],
    );
    let extras: Vec<Option<u64>> = after.iter().map(|v| v["@extra"].as_u64()).collect();
    assert_eq!(extras[..5], [Some(1), None, Some(2), Some(3), Some(4)]);
}

/// Fix F4: a client that is not authorized answers `401` to a request outside the
/// pre-initialization, pre-authentication and authentication lists (`Td.cpp:365-369`); once
/// `authorizationStateReady` is told it answers by the script.
#[test]
fn a_request_that_needs_authorization_gets_401_before_ready() {
    let rules = vec![
        Rule::new("getMe", vec![json!({"@type": "user", "id": 1})]),
        Rule::new("testCallEmpty", vec![json!({"@type": "ok"})]),
    ];
    let td = ScriptedTd::new(Script {
        rules: rules.clone(),
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    send(&td, c, json!({"@type": "getMe", "@extra": 2}));
    send(&td, c, json!({"@type": "testCallEmpty", "@extra": 3}));
    send(
        &td,
        c,
        json!({"@type": "getAuthorizationState", "@extra": 4}),
    );
    let got = drain(&td);
    let by_extra = |e: u64| got.iter().find(|v| v["@extra"] == e).unwrap().clone();
    assert_eq!(
        by_extra(2)["code"],
        401,
        "getMe was answered by an unauthorized client"
    );
    assert_eq!(
        by_extra(3)["@type"],
        "ok",
        "a pre-initialization request was refused"
    );
    assert_eq!(by_extra(4)["@type"], "authorizationStateWaitPhoneNumber");
    // Authorized after the parameters (the script says Ready): the same request is answered.
    let ready = ScriptedTd::new(Script {
        rules,
        after_parameters: "authorizationStateReady".to_owned(),
        ..Script::default()
    });
    let c = ready.create_client();
    send(
        &ready,
        c,
        json!({"@type": "setTdlibParameters", "@extra": 1}),
    );
    send(&ready, c, json!({"@type": "getMe", "@extra": 2}));
    let got = drain(&ready);
    assert!(
        got.iter().any(|v| v["@extra"] == 2 && v["@type"] == "user"),
        "an authorized client did not answer getMe: {:?}",
        types(&got)
    );
}

/// Fix F4: a request for a client id the engine never handed out is answered `400 "Invalid TDLib
/// instance specified"` for that id (`Client.cpp:493-496`), not dropped.
#[test]
fn a_request_for_an_unknown_client_is_answered_400() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    send(&td, c + 41, json!({"@type": "getMe", "@extra": 4}));
    let got = drain(&td);
    assert_eq!(got.len(), 1, "{:?}", types(&got));
    assert_eq!(got[0]["@type"], "error");
    assert_eq!(got[0]["code"], 400);
    assert_eq!(got[0]["@extra"], 4);
    assert_eq!(got[0]["@client_id"], c + 41);
}

/// Fix F4 (the review's last note): a closing client tells its state to `getAuthorizationState`
/// (`Td.cpp:307-311`) and takes a second `close` with `ok`; anything else is `500`.
#[test]
fn a_closing_client_tells_its_state_and_takes_close_again() {
    let td = ScriptedTd::new(Script {
        hang_on_close: true,
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    send(&td, c, json!({"@type": "close", "@extra": 2}));
    drain(&td);
    drain(&td);
    send(
        &td,
        c,
        json!({"@type": "getAuthorizationState", "@extra": 3}),
    );
    send(&td, c, json!({"@type": "close", "@extra": 4}));
    send(&td, c, json!({"@type": "getMe", "@extra": 5}));
    let got = drain(&td);
    assert_eq!(types(&got), ["authorizationStateClosing", "ok", "error"]);
    assert_eq!(got[2]["code"], 500);
    // A client that closed says Closed.
    let done = ScriptedTd::new(Script::default());
    let c = done.create_client();
    send(&done, c, json!({"@type": "close", "@extra": 1}));
    drain(&done);
    drain(&done);
    send(
        &done,
        c,
        json!({"@type": "getAuthorizationState", "@extra": 2}),
    );
    assert_eq!(types(&drain(&done)), ["authorizationStateClosed"]);
}

// ---- TG-2: stateful rules (`when`, `nth`, `deferred`), `requests`, `release` ----

/// A client past its parameters (in WaitPhoneNumber), its queue drained.
fn ready_client(td: &ScriptedTd) -> i32 {
    let c = td.create_client();
    send(td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    drain(td);
    c
}

fn err(message: &str) -> Value {
    json!({"@type": "error", "code": 400, "message": message})
}

/// A rule with `when` answers only a request with its value; the rule after it takes the rest.
#[test]
fn a_rule_with_when_answers_only_its_value() {
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule::when(
                "checkAuthenticationCode",
                "/code",
                json!("12345"),
                vec![json!({"@type": "ok"})],
            ),
            Rule::new("checkAuthenticationCode", vec![err("PHONE_CODE_INVALID")]),
        ],
        ..Script::default()
    });
    let c = ready_client(&td);
    send(
        &td,
        c,
        json!({"@type": "checkAuthenticationCode", "code": "54321", "@extra": 2}),
    );
    send(
        &td,
        c,
        json!({"@type": "checkAuthenticationCode", "code": "12345", "@extra": 3}),
    );
    // The value must be equal, not only present: a number 12345 is not the string.
    send(
        &td,
        c,
        json!({"@type": "checkAuthenticationCode", "code": 12345, "@extra": 4}),
    );
    let got = drain(&td);
    assert_eq!(types(&got), ["error", "ok", "error"]);
    assert_eq!(got[0]["@extra"], 2);
    assert_eq!(got[1]["@extra"], 3);
    assert_eq!(got[2]["@extra"], 4);
}

/// `nth: Some(2)` answers only the second matching request; the others fall to the next rule.
#[test]
fn the_nth_rule_answers_the_nth_call() {
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule::nth("resendAuthenticationCode", 2, vec![err("second")]),
            Rule::new("resendAuthenticationCode", vec![json!({"@type": "ok"})]),
        ],
        ..Script::default()
    });
    let c = ready_client(&td);
    for extra in 2..5 {
        send(
            &td,
            c,
            json!({"@type": "resendAuthenticationCode", "@extra": extra}),
        );
    }
    let got = drain(&td);
    assert_eq!(types(&got), ["ok", "error", "ok"]);
    assert_eq!(got[1]["message"], "second");
    assert_eq!(got[1]["@extra"], 3);
}

/// `when` and `nth` combine: the count is of the requests that match `when`.
#[test]
fn nth_counts_only_the_requests_its_when_matches() {
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule {
                nth: Some(2),
                ..Rule::when(
                    "checkAuthenticationPassword",
                    "/password",
                    json!("right"),
                    vec![json!({"@type": "ok"})],
                )
            },
            Rule::new(
                "checkAuthenticationPassword",
                vec![err("PASSWORD_HASH_INVALID")],
            ),
        ],
        ..Script::default()
    });
    let c = ready_client(&td);
    for (extra, pw) in [(2, "wrong"), (3, "right"), (4, "wrong"), (5, "right")] {
        send(
            &td,
            c,
            json!({"@type": "checkAuthenticationPassword", "password": pw, "@extra": extra}),
        );
    }
    let got = drain(&td);
    assert_eq!(types(&got), ["error", "error", "error", "ok"]);
    assert_eq!(got[3]["@extra"], 5);
}

/// TG-3 C1: `when_all` answers only a request with EVERY pointer's value (a recorded
/// `getChatHistory` keyed on its chat and its `from_message_id`); `nth` counts within that key.
#[test]
fn when_all_needs_every_pointer() {
    let page = |n: i64| json!({"@type": "messages", "total_count": n, "messages": []});
    let key = |from: i64| [("/chat_id", json!(201)), ("/from_message_id", json!(from))];
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule::when_all("getChatHistory", &key(0), vec![page(1)]),
            Rule {
                nth: Some(2),
                ..Rule::when_all("getChatHistory", &key(7), vec![page(2)])
            },
            Rule::new("getChatHistory", vec![err("fallback")]),
        ],
        ..Script::default()
    });
    let c = ready_client(&td);
    td.push_update(json!({"@type": "updateAuthorizationState",
        "authorization_state": {"@type": "authorizationStateReady"}}));
    drain(&td);
    for (extra, chat, from) in [(2, 201, 0), (3, 202, 0), (4, 201, 7), (5, 201, 7)] {
        send(
            &td,
            c,
            json!({"@type": "getChatHistory", "chat_id": chat, "from_message_id": from,
                   "@extra": extra}),
        );
    }
    let got = drain(&td);
    assert_eq!(types(&got), ["messages", "error", "error", "messages"]);
    assert_eq!(got[0]["total_count"], 1);
    assert_eq!(got[3]["total_count"], 2);
    assert_eq!(got[3]["@extra"], 5);
    // An empty list is the plain rule.
    let plain = Rule::when_all("getChat", &[], vec![]);
    assert!(plain.when.is_none() && plain.also.is_empty());
}

/// A deferred rule's reply waits for `release`; other requests are answered meanwhile.
#[test]
fn a_deferred_rule_answers_only_on_release() {
    let td = ScriptedTd::new(Script {
        rules: vec![
            Rule::deferred("checkAuthenticationCode", vec![err("PHONE_CODE_INVALID")]),
            Rule::new("testCallEmpty", vec![json!({"@type": "ok"})]),
        ],
        ..Script::default()
    });
    let c = ready_client(&td);
    send(
        &td,
        c,
        json!({"@type": "checkAuthenticationCode", "code": "1", "@extra": 2}),
    );
    send(&td, c, json!({"@type": "testCallEmpty", "@extra": 3}));
    let before = drain(&td);
    assert_eq!(
        types(&before),
        ["ok"],
        "only the probe's answer before the release"
    );
    assert_eq!(before[0]["@extra"], 3);
    assert_eq!(td.held(), 1);
    assert_eq!(td.release(), 1);
    assert_eq!(td.held(), 0);
    let after = drain(&td);
    assert_eq!(types(&after), ["error"]);
    assert_eq!(after[0]["@extra"], 2);
    assert_eq!(after[0]["@client_id"], c);
    assert_eq!(td.release(), 0, "nothing is released twice");
}

/// The fixture format's `when`, `nth` and `deferred` keys are read into the rule.
#[test]
fn the_fixture_parser_reads_when_and_nth() {
    let script = Script::from_fixture(concat!(
        "# a comment\n",
        r#"{"on":"setAuthenticationPhoneNumber","when":["/phone_number","+999068204719"],"reply":[{"@type":"ok"}]}"#,
        "\n",
        r#"{"on":"resendAuthenticationCode","nth":3,"reply":[]}"#,
        "\n",
        r#"{"on":"checkAuthenticationCode","deferred":true,"reply":[{"@type":"ok"}]}"#,
        "\n",
        r#"{"on":"getMe","reply":[]}"#,
        "\n",
    ));
    assert_eq!(script.rules.len(), 4);
    let r = &script.rules;
    assert_eq!(
        r[0].when,
        Some(("/phone_number".to_owned(), json!("+999068204719")))
    );
    assert_eq!((r[0].nth, r[0].deferred), (None, false));
    assert_eq!(
        (r[1].when.clone(), r[1].nth, r[1].deferred),
        (None, Some(3), false)
    );
    assert_eq!(
        (r[2].when.clone(), r[2].nth, r[2].deferred),
        (None, None, true)
    );
    assert_eq!(
        (r[3].when.clone(), r[3].nth, r[3].deferred),
        (None, None, false)
    );
}

/// `requests` holds every `send` with its client and its JSON, in order, the forwarded and the
/// unparsable ones too.
#[test]
fn requests_records_what_the_app_sent() {
    let td = ScriptedTd::new(Script::default());
    let c = td.create_client();
    let first = json!({"@type": "getOption", "name": "version", "@extra": 1});
    let second = json!({"@type": "setAuthenticationPhoneNumber", "phone_number": "+999068204719", "@extra": 2});
    send(&td, c, first.clone());
    send(&td, c + 7, second.clone());
    td.send(c, &CString::new("not json").unwrap());
    assert_eq!(
        td.requests(),
        vec![(c, first), (c + 7, second), (c, Value::Null)]
    );
}

/// TG-2 C9: a PUSHED `authorizationStateReady` (a QR login's Ready comes from the other device,
/// not from an answered request) authorizes the client it names, so `getMe` is answered by the
/// script; a Ready pushed for ANOTHER client leaves this one unauthorized (401).
#[test]
fn a_pushed_ready_authorizes_the_client_it_names() {
    let rules = vec![Rule::new("getMe", vec![json!({"@type": "user", "id": 1})])];
    let td = ScriptedTd::new(Script {
        rules,
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, json!({"@type": "setTdlibParameters", "@extra": 1}));
    drain(&td);
    td.push_update(json!({
        "@type": "updateAuthorizationState",
        "authorization_state": {"@type": "authorizationStateReady"},
        "@client_id": c + 1,
    }));
    send(&td, c, json!({"@type": "getMe", "@extra": 2}));
    let got = drain(&td);
    assert!(
        got.iter().any(|v| v["@extra"] == 2 && v["code"] == 401),
        "a Ready of another client authorized this one"
    );
    td.push_update(json!({
        "@type": "updateAuthorizationState",
        "authorization_state": {"@type": "authorizationStateReady"},
        "@client_id": c,
    }));
    send(&td, c, json!({"@type": "getMe", "@extra": 3}));
    let got = drain(&td);
    assert!(
        got.iter().any(|v| v["@extra"] == 3 && v["@type"] == "user"),
        "a pushed Ready did not authorize its client"
    );
}
