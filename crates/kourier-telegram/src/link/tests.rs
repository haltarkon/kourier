//! The link over a fake helper (the test plays the helper's side of the socket) and over the
//! scripted TDLib's serve loop (TG-1 plan §5.4 T-b5, T-b6; decisions D3 H3).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use kourier_tdlib::protocol::{
    FromHelper, Hello, LineReader, MAX_LINE, ToHelper, decode_to_helper,
};
use kourier_tdlib::scripted::{Rule, Script, ScriptedTd};
use kourier_tdlib::serve::{ServeOptions, serve};
use serde_json::{Value, json};

use super::*;

/// The helper's side of a link, played by the test.
struct Fake {
    lines: LineReader<UnixStream>,
    out: UnixStream,
}

impl Fake {
    fn write(&mut self, frame: &FromHelper) {
        let mut buf = Vec::new();
        frame.encode(&mut buf).expect("encode");
        self.out.write_all(&buf).expect("the link stopped reading");
    }

    /// The next frame of the app; `None` at the end of its stream.
    fn next(&mut self) -> Option<ToHelper> {
        match self.lines.next_line() {
            Ok(Some(line)) => Some(decode_to_helper(&line).expect("the app wrote a bad frame")),
            Ok(None) => None,
            Err(_) => panic!("the app wrote nothing in time"),
        }
    }

    /// The next `S` frame's request.
    fn request(&mut self) -> Value {
        match self.next() {
            Some(ToHelper::Send { json, .. }) => serde_json::from_str(&json).expect("json"),
            _ => panic!("the app did not send a request"),
        }
    }

    fn answer(&mut self, v: &Value) {
        self.write(&FromHelper::Update(v.to_string()));
    }
}

fn pair() -> (UnixStream, Fake) {
    let (app, helper) = UnixStream::pair().expect("socketpair");
    helper
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let lines = LineReader::new(helper.try_clone().expect("clone"));
    (app, Fake { lines, out: helper })
}

fn hello(commit: &str, version: &str) -> FromHelper {
    hello_tagged(commit, version, PIN.patch_level)
}

fn hello_tagged(commit: &str, version: &str, patch_level: &str) -> FromHelper {
    FromHelper::Hello(Hello {
        protocol: PROTOCOL,
        tdlib: version.to_owned(),
        commit: commit.to_owned(),
        patch_level: patch_level.to_owned(),
        landlock: "off".to_owned(),
    })
}

fn short() -> LinkOptions {
    LinkOptions {
        hello_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_millis(400),
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn a_helper_of_another_commit_is_refused_before_any_client() {
    let other = "0".repeat(40);
    let (app, mut fake) = pair();
    let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
    fake.write(&FromHelper::Heartbeat);
    fake.write(&hello(&other, PIN.version));
    let opened = rt().block_on(link.open(&mut inbound));
    assert!(
        matches!(opened, Err(LinkError::VersionMismatch)),
        "a helper of another TDLib commit was accepted"
    );
    drop(link);
    let mut created = 0;
    while let Some(frame) = fake.next() {
        if matches!(frame, ToHelper::Create { .. }) {
            created += 1;
        }
    }
    assert_eq!(
        created, 0,
        "a client was created on a helper of another build"
    );
    // Each of the three must match.
    let ok = Hello {
        protocol: PROTOCOL,
        tdlib: PIN.version.to_owned(),
        commit: PIN.commit.to_owned(),
        patch_level: PIN.patch_level.to_owned(),
        landlock: "off".to_owned(),
    };
    assert!(check_pin(&ok, &PIN).is_ok(), "the pinned hello is refused");
    // G5: the protocol was 2 (`--gate-port`, the G-port rule); TG-2 made it 3 (the forbidden
    // requests, M5); TG-2b made it 4 (the directory arguments and refusal, the call requests);
    // LC-20 made it 5 (the account classes in the forbidden list, the two web-proxy-carrier
    // requests in the proxy list): a TG-1a (1), TG-1b (2), TG-2 (3) or TG-2b (4) helper is not
    // run, whatever else its hello says.
    assert_eq!(PROTOCOL, 5, "the protocol version moved");
    for bad in [
        Hello {
            protocol: 1,
            ..ok.clone()
        },
        Hello {
            protocol: 2,
            ..ok.clone()
        },
        Hello {
            protocol: 3,
            ..ok.clone()
        },
        Hello {
            protocol: 4,
            ..ok.clone()
        },
        Hello {
            protocol: PROTOCOL + 1,
            ..ok.clone()
        },
        // A version that differs from the pinned one whatever the pin is (derived, never a literal:
        // a literal becomes the right version at the next bump and the case would fail).
        Hello {
            tdlib: format!("{}.1", PIN.version),
            ..ok.clone()
        },
        Hello {
            commit: other.clone(),
            ..ok.clone()
        },
        // K5: the pinned commit and version but no tag (a stock or pre-0002 library), the 0001
        // tag, or a newer one.
        Hello {
            patch_level: String::new(),
            ..ok.clone()
        },
        Hello {
            patch_level: "0001".to_owned(),
            ..ok.clone()
        },
        Hello {
            patch_level: "0003".to_owned(),
            ..ok.clone()
        },
    ] {
        assert!(
            check_pin(&bad, &PIN).is_err(),
            "a hello of another build passed"
        );
    }
}

/// K5: a hello of the pinned commit and version whose library carries no patch tag (a stock,
/// 0001-only or pre-0002 library answers no `kourier_patch_level`) or another one is refused
/// before any client exists; a hello line without the field decodes to the empty tag and is the
/// same refusal, not a bad frame.
#[test]
fn a_hello_without_the_patch_tag_or_with_another_is_refused_before_any_client() {
    let raw = format!(
        r#"H {{"protocol":{PROTOCOL},"tdlib":"{}","commit":"{}","landlock":"off"}}"#,
        PIN.version, PIN.commit
    );
    let untagged = match kourier_tdlib::protocol::decode_from_helper(raw.as_bytes()) {
        Ok(FromHelper::Hello(h)) => h,
        other => panic!("a hello without the tag did not decode: {other:?}"),
    };
    assert_eq!(untagged.patch_level, "", "a missing tag is the empty tag");
    assert!(
        check_pin(&untagged, &PIN).is_err(),
        "a hello without a tag passed"
    );
    for tag in ["", "0001", "0003"] {
        let (app, mut fake) = pair();
        let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
        fake.write(&hello_tagged(PIN.commit, PIN.version, tag));
        let opened = rt().block_on(link.open(&mut inbound));
        assert!(
            matches!(opened, Err(LinkError::VersionMismatch)),
            "a helper with the patch tag {tag:?} was accepted"
        );
        drop(link);
        let mut created = 0;
        while let Some(frame) = fake.next() {
            if matches!(frame, ToHelper::Create { .. }) {
                created += 1;
            }
        }
        assert_eq!(created, 0, "a client was made on a helper tagged {tag:?}");
    }
}

#[test]
fn the_pinned_helper_gets_one_client() {
    let (app, mut fake) = pair();
    let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
    let helper = thread::spawn(move || {
        fake.write(&hello(PIN.commit, PIN.version));
        let Some(ToHelper::Create { seq }) = fake.next() else {
            panic!("the app did not ask for a client");
        };
        fake.write(&FromHelper::Created { seq, client: 7 });
        let mut more = 0;
        while fake.next().is_some() {
            more += 1;
        }
        more
    });
    let opened = rt().block_on(link.open(&mut inbound));
    assert!(matches!(opened, Ok(7)), "the pinned helper got no client");
    drop(link);
    assert_eq!(
        helper.join().expect("fake"),
        0,
        "the app wrote more than one C"
    );
}

#[test]
fn answers_find_their_request_by_extra_and_strays_are_counted() {
    let (app, mut fake) = pair();
    let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
    let helper = thread::spawn(move || {
        let mut by_who = std::collections::HashMap::new();
        for _ in 0..2 {
            let req = fake.request();
            by_who.insert(
                req["who"].as_str().unwrap_or_default().to_owned(),
                req["@extra"].clone(),
            );
        }
        // Interleaved with updates, in the reverse order of the requests, then an unknown one.
        fake.answer(&json!({"@type": "updateX"}));
        fake.answer(&json!({"@type": "ok", "who": "b", "@extra": by_who["b"]}));
        fake.answer(&json!({"@type": "updateX"}));
        fake.answer(&json!({"@type": "ok", "who": "a", "@extra": by_who["a"]}));
        fake.answer(&json!({"@type": "ok", "@extra": 999_999}));
        // A request answered after its timeout.
        let late = fake.request();
        thread::sleep(Duration::from_millis(900));
        fake.answer(&json!({"@type": "ok", "@extra": late["@extra"]}));
        fake.answer(&json!({"@type": "updateDone"}));
        // A TDLib error: only its code reaches the caller.
        let failing = fake.request();
        fake.answer(
            &json!({"@type": "error", "code": 404, "message": "x", "@extra": failing["@extra"]}),
        );
        while fake.next().is_some() {}
    });
    rt().block_on(async {
        let (a, b) = tokio::join!(
            link.request(1, json!({"@type": "getMe", "who": "a"})),
            link.request(1, json!({"@type": "getMe", "who": "b"})),
        );
        let who = |r: &Result<Value, LinkError>| {
            r.as_ref()
                .ok()
                .and_then(|v| v["who"].as_str().map(str::to_owned))
        };
        assert_eq!(
            who(&a).as_deref(),
            Some("a"),
            "request a got another answer"
        );
        assert_eq!(
            who(&b).as_deref(),
            Some("b"),
            "request b got another answer"
        );
        let late = link.request(1, json!({"@type": "getMe", "who": "c"})).await;
        assert!(
            matches!(late, Err(LinkError::Timeout)),
            "the late request did not time out"
        );
        let mut updates = 0;
        loop {
            match inbound.recv().await {
                Some(Inbound::Update(u)) if u.contains("updateDone") => break,
                Some(Inbound::Update(_)) => updates += 1,
                Some(_) => {}
                None => panic!("the link closed"),
            }
        }
        assert_eq!(
            updates, 2,
            "the updates between the answers were not forwarded"
        );
        assert_eq!(
            link.strays(),
            2,
            "the unknown and the late answer are not counted as strays"
        );
        let failed = link.request(1, json!({"@type": "getMe", "who": "d"})).await;
        assert!(
            matches!(failed, Err(LinkError::Td { code: 404 })),
            "a TDLib error did not come back as its code"
        );
    });
    drop(link);
    helper.join().expect("fake");
}

fn h3_update(frame: &Inbound) -> usize {
    match frame {
        Inbound::Update(u) => usize::from(u.contains("\"name\":\"h3\"")),
        _ => 0,
    }
}

/// D3 H3 (4): 10,000 updates while the actor's 2,000 `S` and `E` requests go out, over the real
/// link and the serve loop of the scripted TDLib, within 30 s.
#[test]
fn ten_thousand_updates_during_two_thousand_requests_do_not_block() {
    let burst: Vec<Value> = (0..10)
        .map(|i| json!({"@type": "updateOption", "name": "h3", "value": {"@type": "optionValueInteger", "value": i}}))
        .chain([json!({"@type": "ok"})])
        .collect();
    let td = ScriptedTd::new(Script {
        rules: vec![Rule::new("testCallEmpty", burst)],
        ..Script::default()
    });
    let (app, helper) = UnixStream::pair().expect("socketpair");
    let serving = thread::spawn(move || serve(td, helper, ServeOptions::default()));
    let (link, mut inbound) = HelperLink::start(app, LinkOptions::default()).expect("start");
    rt().block_on(async {
        let client = link.open(&mut inbound).await.expect("open");
        let mut updates = 0usize;
        let work = async {
            for i in 0..2000 {
                let ok = if i % 2 == 0 {
                    drive(
                        &mut inbound,
                        link.request(client, json!({"@type": "testCallEmpty"})),
                        |f| updates += h3_update(&f),
                    )
                    .await
                    .is_ok()
                } else {
                    drive(
                        &mut inbound,
                        link.execute(&json!({"@type": "getOption", "name": "version"})),
                        |f| updates += h3_update(&f),
                    )
                    .await
                    .is_ok()
                };
                assert!(ok, "a request failed");
            }
            while updates < 10_000 {
                match inbound.recv().await {
                    Some(f) => updates += h3_update(&f),
                    None => break,
                }
            }
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(30), work)
                .await
                .is_ok(),
            "the link blocked: 2,000 requests during 10,000 updates did not end in 30 s"
        );
        assert_eq!(updates, 10_000, "updates were lost");
    });
    link.quit();
    drop(link);
    drop(inbound);
    serving.join().expect("serve");
}

/// 1 for the two updates of the M5 script (TDLib's own state updates are not counted).
fn scripted_update(frame: &Inbound) -> usize {
    match frame {
        Inbound::Update(u) => {
            usize::from(u.contains("\"updateUser\"") || u.contains("\"updateNewMessage\""))
        }
        _ => 0,
    }
}

/// Decisions D3 M5, layer (a): what TDLib sends (a phone number, names, a message text, an error
/// message) never reaches a log line of the link, at `trace`, on any of its threads.
#[test]
fn no_frame_content_reaches_a_log_line() {
    const PHONE: &str = "15550001234";
    const FIRST: &str = "Quentin";
    const LAST: &str = "Synthetic-Lastname";
    const TEXT: &str = "a synthetic message text 2718";
    const ERROR: &str = "SYNTHETIC_ERROR_TEXT_1618";
    const SECRET: &str = "synthetic-password-3141";
    let reply = vec![
        json!({"@type": "updateUser", "user": {"@type": "user", "id": 77, "first_name": FIRST,
            "last_name": LAST, "phone_number": PHONE}}),
        json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": 1048576,
            "chat_id": 77, "content": {"@type": "messageText", "text": {"@type": "formattedText",
            "text": TEXT}}}}),
        json!({"@type": "error", "code": 400, "message": ERROR}),
    ];
    let td = ScriptedTd::new(Script {
        rules: vec![Rule::new("testCallEmpty", reply)],
        ..Script::default()
    });
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let (app, helper) = UnixStream::pair().expect("socketpair");
    let serving = thread::spawn(move || serve(td, helper, ServeOptions::default()));
    let (link, mut inbound) = HelperLink::start(app, LinkOptions::default()).expect("start");
    rt().block_on(async {
        let client = link.open(&mut inbound).await.expect("open");
        let mut updates = 0;
        let answer = drive(
            &mut inbound,
            link.request(client, json!({"@type": "testCallEmpty"})),
            |f| updates += scripted_update(&f),
        )
        .await;
        assert!(
            matches!(answer, Err(LinkError::Td { code: 400 })),
            "the error did not come back as its code"
        );
        while updates < 2 {
            match inbound.recv().await {
                Some(f) => updates += scripted_update(&f),
                None => break,
            }
        }
        assert_eq!(updates, 2, "the updates did not arrive");
        // TG-2 M10: the same through the secret path, with a typed value in the request.
        let secret = secret_json(&json_of_pad("testCallEmpty", SECRET));
        let answer = drive(&mut inbound, link.request_secret(client, secret), |f| {
            updates += scripted_update(&f)
        })
        .await;
        assert!(
            matches!(answer, Err(LinkError::Td { code: 400 })),
            "the secret request's error did not come back as its code"
        );
        while updates < 4 {
            match inbound.recv().await {
                Some(f) => updates += scripted_update(&f),
                None => break,
            }
        }
        assert_eq!(updates, 4, "the secret request's updates did not arrive");
    });
    link.quit();
    drop(link);
    // The reader logs its end; draining the inbound queue lets it get there.
    while inbound.blocking_recv().is_some() {}
    serving.join().expect("serve");
    let all = lines.all();
    assert!(
        all.iter()
            .any(|l| l.contains("the TDLib helper link closed")),
        "the capture saw nothing of the link's reader thread, so it proves nothing"
    );
    for secret in [PHONE, FIRST, LAST, TEXT, ERROR, SECRET] {
        assert!(
            !all.iter().any(|l| l.contains(secret)),
            "a value of a TDLib frame reached a log line"
        );
    }
}

/// Fix F2 (decisions D3 B1): the gate's `addedProxy` answer is the helper's own request's answer,
/// so no request waits for it. Over the real serve loop of the scripted TDLib it reaches the actor as
/// its own typed frame, and it is no stray.
#[test]
fn the_gate_proxy_answer_reaches_the_actor_typed_and_is_no_stray() {
    let td = ScriptedTd::new(Script::default());
    let (app, helper) = UnixStream::pair().expect("socketpair");
    let serving = thread::spawn(move || serve(td, helper, ServeOptions::default()));
    let (link, mut inbound) = HelperLink::start(app, LinkOptions::default()).expect("start");
    let found = rt().block_on(async {
        let client = link.open(&mut inbound).await.expect("open");
        link.gate(client, 40001, "u".to_owned(), "p".to_owned())
            .expect("the gate");
        let mut seen = Vec::new();
        let params = drive(
            &mut inbound,
            link.request(client, json!({"@type": "setTdlibParameters"})),
            |f| seen.push(f),
        )
        .await;
        assert!(params.is_ok(), "the parameters were not answered");
        let gate = |f: &Inbound| match f {
            Inbound::GateProxy { client, ok, port } => Some((*client, *ok, *port)),
            _ => None,
        };
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(g) = seen.iter().find_map(gate) {
                    return g;
                }
                match inbound.recv().await {
                    Some(f) => seen.push(f),
                    None => panic!("the link ended before the gate's answer"),
                }
            }
        })
        .await
        .expect("the gate's answer never reached the actor");
        (client, found)
    });
    assert_eq!(found.1, (found.0, true, Some(40001)));
    assert_eq!(link.strays(), 0, "the gate's answer was counted as a stray");
    link.quit();
    drop(link);
    while inbound.blocking_recv().is_some() {}
    serving.join().expect("serve");
}

/// Fix F2: the typed gate answer reads the type, `is_enabled`, the server and the port; an error
/// or another server is `ok: false`; an answer in the helper's range that is not the gate's, or that
/// names no client, is a protocol fault (the reader stops, `Garbled`).
#[test]
fn the_gate_answer_is_typed_and_other_helper_answers_are_protocol_faults() {
    let gate = |extra: u64, body: Value| {
        let mut v = json!({"@extra": extra, "@client_id": 3});
        v.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        FromHelper::Update(v.to_string())
    };
    let good = json!({"@type": "addedProxy", "id": 1, "is_enabled": true,
        "proxy": {"server": "127.0.0.1", "port": 41000}});
    let cases = [
        (
            gate(GATE_PROXY_EXTRA, good.clone()),
            Some((3, true, Some(41000))),
        ),
        (
            gate(
                GATE_PROXY_EXTRA,
                json!({"@type": "addedProxy", "is_enabled": false,
                    "proxy": {"server": "127.0.0.1", "port": 41000}}),
            ),
            Some((3, false, Some(41000))),
        ),
        (
            gate(
                GATE_PROXY_EXTRA,
                json!({"@type": "addedProxy", "is_enabled": true,
                    "proxy": {"server": "10.0.0.2", "port": 41000}}),
            ),
            Some((3, false, Some(41000))),
        ),
        (
            gate(
                GATE_PROXY_EXTRA,
                json!({"@type": "error", "code": 500, "message": "x"}),
            ),
            Some((3, false, None)),
        ),
        // Not the gate's: any other `@extra` of the helper's range never reaches the app.
        (gate(GATE_PROXY_EXTRA + 1, json!({"@type": "ok"})), None),
        (gate(HELPER_EXTRA_MIN + 9, json!({"@type": "ok"})), None),
        // The gate's answer without a client.
        (
            FromHelper::Update(
                json!({"@type": "addedProxy", "@extra": GATE_PROXY_EXTRA}).to_string(),
            ),
            None,
        ),
    ];
    for (frame, want) in cases {
        let (app, mut fake) = pair();
        let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
        fake.write(&frame);
        let got = rt().block_on(inbound.recv());
        match (got, want) {
            (Some(Inbound::GateProxy { client, ok, port }), Some(w)) => {
                assert_eq!((client, ok, port), w)
            }
            (Some(Inbound::Garbled), None) => {}
            (other, _) => panic!("a helper answer was routed as {other:?}"),
        }
        assert_eq!(
            link.strays(),
            0,
            "an answer of the helper's range was a stray"
        );
    }
}

/// Fix F14: a request that cannot be encoded (a line over the limit, a gate the protocol refuses)
/// fails only that request, typed `Frame`; the link goes on and the next request is written.
#[test]
fn a_request_that_cannot_be_encoded_fails_only_itself() {
    let (app, mut fake) = pair();
    let (link, inbound) = HelperLink::start(app, short()).expect("start");
    let huge = "x".repeat(MAX_LINE);
    let (request, execute, gate) = rt().block_on(async {
        let request = link
            .request(1, json!({"@type": "getMe", "pad": huge}))
            .await;
        let execute = link
            .execute(&json!({"@type": "getOption", "pad": huge}))
            .await;
        (
            request,
            execute,
            link.gate(1, 0, "u".to_owned(), "p".to_owned()),
        )
    });
    assert!(matches!(request, Err(LinkError::Frame)), "{request:?}");
    assert!(matches!(execute, Err(LinkError::Frame)), "{execute:?}");
    assert!(matches!(gate, Err(LinkError::Frame)), "{gate:?}");
    drop(huge);
    // The link is whole: the next request is the FIRST frame the helper sees, and its answer comes.
    let helper = thread::spawn(move || {
        let req = fake.request();
        fake.answer(&json!({"@type": "ok", "@extra": req["@extra"]}));
        fake
    });
    let ok = rt().block_on(link.request(1, json!({"@type": "getMe"})));
    assert!(
        ok.is_ok(),
        "the request after an unencodable one failed: {ok:?}"
    );
    assert_eq!(link.strays(), 0);
    drop(helper.join().expect("fake"));
    drop(inbound);
}

/// Fix F14: a helper line over the limit, or one that is not UTF-8, is a fault of the protocol
/// (`Garbled`), not the helper's end (`Closed`).
#[test]
fn a_helper_line_over_the_limit_or_not_utf8_is_garbled_not_closed() {
    let mut too_long = b"U ".to_vec();
    too_long.resize(MAX_LINE + 2, b'x');
    too_long.push(b'\n');
    for (what, bytes) in [
        ("a line over the limit", too_long),
        ("a line that is not UTF-8", b"U \xff\xfe\n".to_vec()),
    ] {
        let (app, fake) = pair();
        let (link, mut inbound) = HelperLink::start(app, short()).expect("start");
        let Fake { mut out, .. } = fake;
        // A line over the limit blocks the writer until the link is gone: write from a thread.
        let writer = thread::spawn(move || {
            let _ = out.write_all(&bytes);
        });
        let last = rt().block_on(async {
            loop {
                match inbound.recv().await {
                    Some(Inbound::Garbled) => return "garbled",
                    Some(Inbound::Closed) | None => return "closed",
                    Some(_) => {}
                }
            }
        });
        assert_eq!(last, "garbled", "{what} ended the link as a plain close");
        drop(link);
        writer.join().expect("writer");
    }
}

/// Fix F12 (decisions D3 H3): 200 concurrent `execute`s against a helper that holds its answers:
/// exactly `MAX_CONTROL_IN_FLIGHT` (64) `E` frames are out before the first answer, never more, and
/// all 200 are answered in the end as the answers free the room. (A helper's control queue is
/// never bounded, so the app's cap is what bounds it.)
#[test]
fn at_most_sixty_four_executes_are_out_before_the_first_answer() {
    const TOTAL: usize = 200;
    let (app, mut fake) = pair();
    let (link, inbound) = HelperLink::start(app, LinkOptions::default()).expect("start");
    let helper = thread::spawn(move || {
        let mut answered = 0;
        let mut batches = Vec::new();
        while answered < TOTAL {
            // The next batch: as many as the cap lets out, or what is left.
            let want = MAX_CONTROL_IN_FLIGHT.min(TOTAL - answered);
            let mut seqs = Vec::new();
            for _ in 0..want {
                match fake.next() {
                    Some(ToHelper::Execute { seq, .. }) => seqs.push(seq),
                    other => panic!("expected an E frame, got {other:?}"),
                }
            }
            // Nothing more may come until an answer frees room.
            fake.out
                .set_read_timeout(Some(Duration::from_millis(300)))
                .expect("timeout");
            if let Ok(Some(more)) = fake.lines.next_line() {
                let _ = more;
                panic!("more than {MAX_CONTROL_IN_FLIGHT} E frames were out before an answer");
            }
            fake.out
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("timeout");
            batches.push(seqs.len());
            for seq in seqs {
                fake.write(&FromHelper::Executed {
                    seq,
                    json: r#"{"@type":"ok"}"#.to_owned(),
                });
            }
            answered += want;
        }
        batches
    });
    let oks = rt().block_on(async {
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..TOTAL {
            let link = link.clone();
            set.spawn(async move {
                link.execute(&json!({"@type": "getOption", "name": "version"}))
                    .await
                    .is_ok()
            });
        }
        let mut oks = 0;
        while let Some(done) = set.join_next().await {
            oks += usize::from(done.expect("task"));
        }
        oks
    });
    assert_eq!(oks, TOTAL, "an execute failed");
    let batches = helper
        .join()
        .expect("the helper side saw more than the cap");
    assert_eq!(
        batches[0], MAX_CONTROL_IN_FLIGHT,
        "the first batch was not the cap"
    );
    drop(inbound);
}

/// L8: TDLib's "Can't lock file" (another helper still holds the account's database) is the fixed
/// class `DatabaseLocked`, whatever its code; nothing of the message (a path) is in the error; any
/// other error keeps only its code.
#[test]
fn a_locked_database_is_its_own_class_and_keeps_no_text() {
    let path = "/home/someone/accounts/abc/tdlib/td.binlog";
    for (code, message) in [
        (
            400,
            format!(
                "Can't lock file \"{path}\", because it is already in use; check for another program instance running"
            ),
        ),
        (
            11,
            format!("Can't lock file \"{path}\", because it is already in use by current program"),
        ),
    ] {
        let failed = td_result(json!({"@type": "error", "code": code, "message": message}));
        assert_eq!(failed, Err(LinkError::DatabaseLocked));
        let text = format!("{} {:?}", LinkError::DatabaseLocked, failed);
        assert!(!text.contains("td.binlog") && !text.contains("someone"));
    }
    assert_eq!(
        td_result(
            json!({"@type": "error", "code": 400, "message": "Valid api_id must be provided"})
        ),
        Err(LinkError::Td { code: 400 })
    );
    // G12: the bare "Can't lock file" (any other failed lock: no lock service on the file system)
    // is permanent, so it is not the locked-database class.
    for message in [
        "Can't lock file".to_owned(),
        format!("Can't lock file \"{path}\": Error 37 : No locks available"),
    ] {
        assert_eq!(
            td_result(json!({"@type": "error", "code": 500, "message": message})),
            Err(LinkError::Td { code: 500 })
        );
    }
    assert!(td_result(json!({"@type": "ok"})).is_ok());
}

/// TG-2 (brief §2.8): each known error text is its fixed class, whatever else TDLib said; the
/// message is in neither the error nor its texts.
#[test]
fn a_classified_error_keeps_its_class_and_drops_its_message() {
    for (code, message, class) in [
        (400, "PHONE_NUMBER_INVALID", TdClass::PhoneNumberInvalid),
        (400, "PHONE_NUMBER_BANNED", TdClass::PhoneNumberBanned),
        (400, "PHONE_NUMBER_FLOOD", TdClass::PhoneNumberFlood),
        (400, "PHONE_CODE_INVALID", TdClass::PhoneCodeInvalid),
        (400, "PHONE_CODE_EXPIRED", TdClass::PhoneCodeExpired),
        (400, "PHONE_CODE_EMPTY", TdClass::PhoneCodeEmpty),
        (400, "PASSWORD_HASH_INVALID", TdClass::PasswordHashInvalid),
        (400, "EMAIL_INVALID", TdClass::EmailInvalid),
        (400, "EMAIL_CODE_INVALID", TdClass::EmailCodeInvalid),
        (400, "EMAIL_NOT_ALLOWED", TdClass::EmailNotAllowed),
        (400, "CODE_INVALID", TdClass::CodeInvalid),
        (406, "FROZEN_METHOD_INVALID", TdClass::FrozenMethodInvalid),
        (400, "Invalid recovery code", TdClass::InvalidRecoveryCode),
        (
            400,
            "Application update is needed to log in",
            TdClass::UpdateNeeded,
        ),
        (
            400,
            "Another authorization query has started",
            TdClass::AnotherQuery,
        ),
        (
            400,
            "Call to requestQrCodeAuthentication unexpected",
            TdClass::Unexpected,
        ),
    ] {
        let failed = td_result(json!({"@type": "error", "code": code, "message": message}));
        assert_eq!(
            failed,
            Err(LinkError::Auth {
                code,
                class,
                retry_after_s: None
            })
        );
        let err = failed.unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(
            !text.contains(message),
            "the message reached the error's text ({class:?})"
        );
    }
}

/// A 429 keeps TDLib's wait (a positive number after the one fixed prefix) and nothing else; a 429
/// without a readable wait is the class without one.
#[test]
fn a_429_keeps_its_wait() {
    let auth = |message: &str| {
        td_result(json!({"@type": "error", "code": 429, "message": message})).unwrap_err()
    };
    let with_wait = |s: Option<u32>| LinkError::Auth {
        code: 429,
        class: TdClass::TooManyRequests,
        retry_after_s: s,
    };
    assert_eq!(
        auth("Too Many Requests: retry after 61"),
        with_wait(Some(61))
    );
    assert_eq!(auth("Too Many Requests: retry after 0"), with_wait(None));
    assert_eq!(auth("Too Many Requests: retry after -5"), with_wait(None));
    assert_eq!(auth("Too Many Requests: retry after 61x"), with_wait(None));
    assert_eq!(auth("FLOOD_WAIT_61"), with_wait(None));
    // The wait is a number of seconds only: no other text of the message is in the error.
    let text = format!("{}", auth("Too Many Requests: retry after 61"));
    assert!(text.contains("61") && !text.contains("Too Many"));
}

/// A message of no known class (and a near miss of one) is a plain `Td { code }`, as before TG-2.
#[test]
fn an_unknown_message_is_a_plain_td_code() {
    for message in [
        "SYNTHETIC_ERROR_TEXT_1618",
        "phone_number_invalid",
        "PHONE_NUMBER_INVALID ",
        "Call to requestQrCodeAuthentication",
        "Unauthorized",
        "",
    ] {
        assert_eq!(
            td_result(json!({"@type": "error", "code": 400, "message": message})),
            Err(LinkError::Td { code: 400 }),
            "a near miss was classified (length {})",
            message.len()
        );
    }
    // A 401 before Ready is not a class (and not a logout, the session's rule).
    assert_eq!(
        td_result(json!({"@type": "error", "code": 401, "message": "Unauthorized"})),
        Err(LinkError::Td { code: 401 })
    );
    assert_eq!(
        classify(400, "PHONE_CODE_INVALID"),
        Some((TdClass::PhoneCodeInvalid, None))
    );
    assert_eq!(classify(400, "nothing known"), None);
}

/// A borrowed request of one type and one string field `pad`, as the secret path wants it.
#[derive(serde::Serialize)]
struct Padded<'a> {
    #[serde(rename = "@type")]
    ty: &'a str,
    pad: &'a str,
}

fn json_of_pad<'a>(ty: &'a str, pad: &'a str) -> Padded<'a> {
    Padded { ty, pad }
}

/// TG-2 M10: the secret path's text is serde_json's escaping of the borrowed struct (a `"`, a `\`,
/// a newline and non-ASCII come back as typed), on ONE line, and nothing else.
#[test]
fn secret_json_escapes_a_typed_value_and_keeps_it_on_one_line() {
    let typed = "p\"a\\s\ns\tw\u{00fc}rd \u{1f511} \u{0007}";
    let text = secret_json(&json_of_pad("checkAuthenticationPassword", typed));
    assert!(!text.is_empty(), "the text is empty");
    assert!(
        !text.contains('\n') && !text.contains('\r'),
        "the text holds a raw newline"
    );
    let back: Value = serde_json::from_str(text.as_str()).expect("the text is no JSON");
    assert_eq!(back["@type"], "checkAuthenticationPassword");
    assert!(
        back["pad"].as_str() == Some(typed),
        "the typed value did not come back as typed"
    );
    assert_eq!(
        back.as_object().map(serde_json::Map::len),
        Some(2),
        "the text holds more than the struct"
    );
    // Exactly the plain serializer's text: the measured size held it all.
    let plain =
        serde_json::to_string(&json_of_pad("checkAuthenticationPassword", typed)).expect("plain");
    assert!(
        text.as_str() == plain,
        "the measured text differs from serde_json's"
    );
}

/// TG-2 M10: the frame of a secret request is `S <client> <json>` with the `@extra` put in by text
/// before the last `}` (no comma for an empty object), ending in ONE newline; anything that is not
/// one object ending in `}`, or holds a raw newline, is `Frame`.
#[test]
fn a_secret_frame_inserts_its_extra_by_text_and_refuses_what_is_no_object() {
    let frame = |json: &str| secret_frame(5, json, 9).map(|f| f.as_slice().to_vec());
    assert_eq!(
        frame(r#"{"@type":"x"}"#),
        Ok(b"S 5 {\"@type\":\"x\",\"@extra\":9}\n".to_vec())
    );
    assert_eq!(frame("{}"), Ok(b"S 5 {\"@extra\":9}\n".to_vec()));
    assert_eq!(frame("{ }"), Ok(b"S 5 { \"@extra\":9}\n".to_vec()));
    assert_eq!(
        secret_frame(-3, r#"{"a":1}"#, 12).map(|f| f.as_slice().to_vec()),
        Ok(b"S -3 {\"a\":1,\"@extra\":12}\n".to_vec())
    );
    // The frame decodes as the helper reads it, and the JSON carries the extra as a number.
    let bytes = frame(r#"{"@type":"x","pad":"a\"b"}"#).expect("frame");
    let Ok(ToHelper::Send { client, json }) = decode_to_helper(&bytes[..bytes.len() - 1]) else {
        panic!("the helper cannot read the frame");
    };
    assert_eq!(client, 5);
    let v: Value = serde_json::from_str(&json).expect("json");
    assert_eq!(v["@extra"], 9);
    assert_eq!(v["pad"], "a\"b");
    for bad in [
        "",
        "}",
        "{",
        "[]",
        "\"text\"",
        "{\"a\":1} ",
        "{\"a\":1}\n",
        "{\"a\":\"x\ny\"}",
        "{\"a\":\"x\ry\"}",
        " {\"a\":1}",
    ] {
        assert_eq!(frame(bad), Err(LinkError::Frame), "a bad text was framed");
    }
    // A line over the limit, counted with the frame's own parts.
    let huge = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_LINE));
    assert_eq!(frame(&huge), Err(LinkError::Frame));
}

/// TG-2 M10: a secret request is answered like any other (its answer found by its `@extra`), the
/// helper side reads the typed value back as typed, and a text the secret path refuses fails only
/// itself: the next request is the FIRST frame the helper sees.
#[test]
fn a_secret_request_is_answered_like_any_other_and_a_bad_one_fails_only_itself() {
    let typed = "p\"a\\s\nsw\u{00fc}rd";
    let (app, mut fake) = pair();
    let (link, inbound) = HelperLink::start(app, short()).expect("start");
    let refused = rt().block_on(async {
        let mut out = Vec::new();
        for bad in ["", "[1]", "{\"a\":1}\n", "{\"a\":\"x\ny\"}"] {
            out.push(link.request_secret(1, Zeroizing::new(bad.to_owned())).await);
        }
        out
    });
    for r in &refused {
        assert!(matches!(r, Err(LinkError::Frame)), "{r:?}");
    }
    let helper = thread::spawn(move || {
        let req = fake.request();
        let same = req["@type"] == "checkAuthenticationPassword"
            && req["pad"].as_str() == Some("p\"a\\s\nsw\u{00fc}rd");
        fake.answer(&json!({"@type": "ok", "@extra": req["@extra"]}));
        (same, fake)
    });
    let ok = rt().block_on(link.request_secret(
        7,
        secret_json(&json_of_pad("checkAuthenticationPassword", typed)),
    ));
    assert!(
        matches!(&ok, Ok(v) if v["@type"] == "ok"),
        "the secret request got no answer: {ok:?}"
    );
    let (same, fake) = helper.join().expect("fake");
    assert!(
        same,
        "the helper did not read the secret request as written (or a refused one reached it first)"
    );
    assert_eq!(link.strays(), 0);
    drop(fake);
    drop(inbound);
}
