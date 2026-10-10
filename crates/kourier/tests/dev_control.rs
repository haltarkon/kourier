//! The development control channel end to end (debug builds): the real binary in fixture mode,
//! offscreen, driven through `--dev-control` JSON lines: wait for data, open a conversation by
//! title, send from the composer and wait for the Sent state, attach several files in one call
//! (one of them missing) and send them as the messages the network needs, read the rows' marks
//! from `state` (send state, attachment kinds, a reaction), find no challenge to answer, grab the
//! window at two sizes, switch pages, get errors for bad requests, quit; closing stdin quits
//! too. With
//! `--fixture-link`: add the demo account, type the device name and link it through its rotating
//! QR codes, with the login events on the way.

#![cfg(debug_assertions)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous: a cold offscreen start of Qt and the fixture data takes a few seconds.
const WAIT: Duration = Duration::from_secs(90);

struct Kourier {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    /// Events read while looking for something else.
    events: Vec<Value>,
    next_id: u64,
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

impl Kourier {
    fn start(dir: &Path) -> Self {
        Self::start_with(dir, "--fixture")
    }

    /// `mode` is `--fixture` or `--fixture-link`.
    fn start_with(dir: &Path, mode: &str) -> Self {
        Self::start_env(dir, mode, &[])
    }

    /// [`Self::start_with`] with more environment variables.
    fn start_env(dir: &Path, mode: &str, env: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kourier"))
            .args([mode, "--dev-control", "--size=1000x700"])
            .envs(env.iter().copied())
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_QUICK_BACKEND", "software")
            .env("QT_SCALE_FACTOR", "1")
            .env("QT_FORCE_STDERR_LOGGING", "1")
            // Fixture mode's temporary profile goes below the test's scratch directory.
            .env("TMPDIR", dir)
            .env("LANG", "en_US.UTF-8")
            .env("LANGUAGE", "en")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                std::fs::File::create(dir.join("stderr.log")).expect("log file"),
            ))
            .spawn()
            .expect("start kourier");
        let stdout = child.stdout.take().expect("stdout");
        let stdin = child.stdin.take();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Only protocol lines are JSON objects; anything else is ignored.
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            events: Vec::new(),
            next_id: 0,
        }
    }

    fn write(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush");
    }

    /// The next line `f` accepts (others are skipped; skipped events are kept for
    /// [`Self::event`]).
    fn line(&mut self, what: &str, mut f: impl FnMut(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(v) if f(&v) => return v,
                Ok(v) => {
                    if v["type"] == "event" {
                        self.events.push(v);
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
                Err(RecvTimeoutError::Disconnected) => panic!("kourier exited before {what}"),
            }
        }
    }

    /// The first event `name` (already read or still to come) whose data `f` accepts.
    fn event(&mut self, name: &str, f: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self
            .events
            .iter()
            .position(|e| e["event"] == name && f(&e["data"]))
        {
            return self.events.remove(i);
        }
        self.line(name, |v| {
            v["type"] == "event" && v["event"] == name && f(&v["data"])
        })
    }

    /// Sends a request and returns its response.
    fn request(&mut self, cmd: &str, args: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"id": id, "cmd": cmd, "args": args}).to_string());
        self.line(cmd, |v| v["type"] == "response" && v["id"] == id)
    }

    /// A request that must succeed; returns its result.
    fn ok(&mut self, cmd: &str, args: Value) -> Value {
        let r = self.request(cmd, args);
        assert_eq!(r["ok"], true, "{cmd}: {r}");
        r["result"].clone()
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "kourier did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Kourier {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A valid PNG of one transparent pixel.
fn tiny_png() -> Vec<u8> {
    const HEX: &str = "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489\
0000000a49444154789c63000100000500010d0a2db40000000049454e44ae426082";
    (0..HEX.len() / 2)
        .map(|i| u8::from_str_radix(&HEX[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

fn is_png(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|b| b.len() > 1000 && b.starts_with(b"\x89PNG\r\n\x1a\n"))
}

#[test]
fn drives_the_fixture_ui() {
    let dir = scratch("dev-control-drive");
    let mut k = Kourier::start(&dir);
    k.line("the channel event", |v| v["event"] == "channel");

    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    let got = k.ok("wait", json!({"until": "conversations_at_least:3"}));
    assert!(got["observed"].as_u64().is_some_and(|n| n >= 3), "{got}");
    k.ok("wait", json!({"until": "account_active"}));

    // No conversation is open yet: there is no timer to report, and "off" is not claimed.
    let state = k.ok("state", json!({}));
    assert_eq!(state["open"], 0, "{state}");
    assert!(state["timer_secs"].is_null(), "{state}");

    // Nothing can be attached before a conversation is open.
    let notes = dir.join("Route notes.txt");
    std::fs::write(&notes, "08:00 trailhead\n").expect("a file to attach");
    let r = k.request("attach", json!({"path": notes.to_string_lossy()}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");

    let opened = k.ok("open", json!({"title": "BOOK club"}));
    assert!(opened["conversation"].as_i64().is_some_and(|id| id > 0));
    k.ok("wait", json!({"until": "messages_at_least:5"}));

    // The list as marks, for a driver that never handles names: ids, kinds and counts, and
    // nothing a title or a preview could be read from.
    let chats = k.ok("chats", json!({}));
    let list = chats["chats"].as_array().expect("chats");
    assert!(list.len() >= 3, "{chats}");
    for chat in list {
        let mut keys: Vec<&str> = chat
            .as_object()
            .expect("a chat")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "conversation",
                "kind",
                "last_activity",
                "newest_own",
                "self",
                "unread"
            ]
        );
        assert!(chat["conversation"].as_i64().is_some_and(|id| id > 0));
        assert!(["direct", "group", "other"].contains(&chat["kind"].as_str().unwrap_or("")));
    }
    assert!(
        list.iter().any(|c| c["kind"] == "group") && list.iter().any(|c| c["kind"] == "direct")
    );
    // A chat opened by its id, and the open chat's timer among the marks.
    let other = list
        .iter()
        .find(|c| c["conversation"] != opened["conversation"])
        .expect("another chat")["conversation"]
        .clone();
    let by_id = k.ok("open_id", json!({"conversation": other}));
    assert_eq!(by_id["conversation"], other);
    // The timer is a number only once the chat's header is loaded: until then it is `null`,
    // never a 0 that would read as "off" (a driver compares it before a send).
    let deadline = Instant::now() + Duration::from_secs(20);
    let state = loop {
        let state = k.ok("state", json!({}));
        assert_eq!(state["open"], other, "{state}");
        if !state["timer_secs"].is_null() {
            break state;
        }
        assert!(
            Instant::now() < deadline,
            "the header never loaded: {state}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        state["timer_secs"].as_i64().is_some_and(|s| s >= 0),
        "{state}"
    );
    let r = k.request("open_id", json!({"conversation": 999_999}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");
    k.ok("open_id", json!({"conversation": opened["conversation"]}));
    k.ok("wait", json!({"until": "messages_at_least:5"}));

    let wide = dir.join("chat-wide.png");
    let shot = k.ok(
        "screenshot",
        json!({"path": wide.to_string_lossy(), "size": "1100x720", "settle_ms": 300}),
    );
    assert_eq!(
        (shot["width"].as_u64(), shot["height"].as_u64()),
        (Some(1100), Some(720))
    );
    assert!(is_png(&wide));

    k.ok("send", json!({"text": "sent through the dev channel"}));
    let state = k.ok(
        "wait",
        json!({"until": "outgoing_state:Sent", "timeout_ms": 30_000}),
    );
    assert!(["sent", "delivered", "read"].contains(&state["observed"].as_str().unwrap_or("")));

    // Files go through the composer's list like picked ones: any number, of any kinds, in
    // one call. A missing file is taken too and marked once the core has looked; it stays
    // behind when the others are sent.
    let photo = dir.join("Trail.png");
    std::fs::write(&photo, tiny_png()).expect("a picture");
    let attached = k.ok(
        "attach",
        json!({"paths": [
            notes.to_string_lossy(),
            dir.join("no such file.txt").to_string_lossy(),
            photo.to_string_lossy(),
            // The same file again is one more file: nothing is left out.
            notes.to_string_lossy(),
        ]}),
    );
    assert_eq!(attached["added"], 4, "{attached}");
    assert_eq!(attached["attachments"], 4, "{attached}");
    let planned = k.ok(
        "wait",
        json!({"until": "files_planned", "timeout_ms": 30_000}),
    );
    let composer = &planned["observed"];
    assert_eq!(composer["files"], 4, "{planned}");
    assert_eq!(composer["refused"], 1, "the missing file: {planned}");
    assert_eq!(composer["sendable"], 3, "{planned}");
    // The fake network sends files that are no pictures or videos one per message.
    assert_eq!(composer["messages"], 3, "{planned}");
    let before = k.ok("state", json!({}));
    let rows_before = before["rows"].as_array().map_or(0, Vec::len);
    k.ok("send", json!({"text": "the route notes"}));
    k.ok(
        "wait",
        json!({"until": "none_pending", "timeout_ms": 30_000}),
    );
    let st = k.ok("state", json!({}));
    assert_eq!(st["composer"]["files"], 1, "the refused file stays: {st}");
    assert_eq!(st["composer"]["sendable"], 0, "{st}");
    // Three messages from one send, newest first, each with its own state; the caption is
    // on the first of them.
    let rows = st["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), rows_before + 3, "{st}");
    for row in &rows[..3] {
        assert_eq!(row["own"], true, "{row}");
        assert_eq!(row["attachments"], 1, "{row}");
        assert!(
            ["sent", "delivered", "read"].contains(&row["state"].as_str().unwrap_or("")),
            "{row}"
        );
        assert_eq!(
            (&row["edited"], &row["deleted"], &row["quote"]),
            (&json!(false), &json!(false), &Value::Null),
            "{row}"
        );
    }
    assert_eq!(rows[2]["kinds"], json!(["file"]), "{st}");
    assert_eq!(rows[1]["kinds"], json!(["image"]), "{st}");
    assert_eq!(rows[0]["kinds"], json!(["file"]), "{st}");
    assert_eq!(
        (&rows[2]["text"], &rows[1]["text"], &rows[0]["text"]),
        (&json!(true), &json!(false), &json!(false)),
        "the caption goes on the first message: {st}"
    );
    assert_eq!(st["challenge"], Value::Null, "{st}");
    // The marks of a message the user acts on.
    let acted = k.ok(
        "act",
        json!({"on": "own", "action": "react", "emoji": "👍"}),
    );
    let message = acted["message"].as_i64().expect("message id");
    let deadline = Instant::now() + WAIT;
    loop {
        let st = k.ok("state", json!({}));
        let row = st["rows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["message"] == message).cloned())
            .expect("the row");
        if row["reactions"] == json!([{"emoji": "👍", "count": 1, "mine": true}]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reaction never showed: {row}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // No account waits for a challenge: there is none to answer.
    let r = k.request("challenge", json!({"answer": "signalcaptcha://x"}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");
    k.ok("wait", json!({"until": "no_challenge"}));
    // What stayed behind is cleared by attaching nothing new and sending a text.
    k.ok("send", json!({"text": "and a text after the files"}));
    k.ok(
        "wait",
        json!({"until": "none_pending", "timeout_ms": 30_000}),
    );

    let narrow = dir.join("chat-narrow.png");
    k.ok(
        "screenshot",
        json!({"path": narrow.to_string_lossy(), "size": [420, 760], "settle_ms": 300}),
    );
    assert!(is_png(&narrow));
    for page in ["list", "settings", "chat", "drawer", "welcome"] {
        k.ok("page", json!({"name": page}));
    }
    let st = k.ok("state", json!({}));
    assert_eq!(st["window"], json!([420, 760]), "{st}");
    assert_eq!(st["newest_own"], "sent", "{st}");

    // Errors answer with the request's id and leave the channel usable.
    let r = k.request("open", json!({"title": "no such conversation title"}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");
    let r = k.request("fly", json!({}));
    assert_eq!(r["error"]["code"], "unknownCommand", "{r}");
    let r = k.request(
        "wait",
        json!({"until": "messages_at_least:100000", "timeout_ms": 300}),
    );
    assert_eq!(r["error"]["code"], "timeout", "{r}");
    let r = k.request("submit_login", json!({}));
    assert_eq!(r["error"]["code"], "failed", "no login step: {r}");
    k.write("this is not json");
    let r = k.line("a bad-request answer", |v| {
        v["type"] == "response" && v["id"].is_null()
    });
    assert_eq!(r["error"]["code"], "badRequest", "{r}");

    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
}

#[test]
fn closing_stdin_quits() {
    let dir = scratch("dev-control-eof");
    let mut k = Kourier::start(&dir);
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    drop(k.stdin.take());
    assert!(k.wait_exit().success());
}

#[test]
fn links_through_the_login_commands() {
    let dir = scratch("dev-control-link");
    let mut k = Kourier::start_with(&dir, "--fixture-link");
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));

    // Nothing to submit before an account is being added.
    let r = k.request("fill_login", json!({"values": ["x"]}));
    assert_eq!(r["error"]["code"], "failed", "{r}");

    let started = k.ok("add_account", json!({"network": "Fake"}));
    assert!(started["kind"].is_string(), "{started}");
    k.ok("wait", json!({"until": "login:input"}));
    k.event("login_step", |d| d["kind"] == "input");
    // Unknown field ids and too many values are refused; nothing is submitted.
    let r = k.request("fill_login", json!({"values": {"no-such-field": "x"}}));
    assert_eq!(r["error"]["code"], "failed", "{r}");
    let r = k.request("fill_login", json!({"values": ["a", "b"]}));
    assert_eq!(r["error"]["code"], "failed", "{r}");
    k.ok(
        "fill_login",
        json!({"values": {"device-name": "Dev channel test"}}),
    );
    let shot = dir.join("device-name.png");
    k.ok(
        "screenshot",
        json!({"path": shot.to_string_lossy(), "settle_ms": 300}),
    );
    assert!(is_png(&shot));
    k.ok("submit_login", json!({}));

    let qr = k.event("login_qr", |_| true);
    assert!(
        qr["data"]["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("fake-link://")),
        "{qr}"
    );
    k.ok("wait", json!({"until": "login:qr"}));
    // The fixture's phone scans the last of its rotating codes.
    k.event("login_step", |d| d["kind"] == "done");
    k.event("account_state", |d| d["state"] == "active");
    k.ok(
        "wait",
        json!({"until": "account_active", "timeout_ms": 60_000}),
    );
    let r = k.request("submit_login", json!({}));
    assert_eq!(r["error"]["code"], "failed", "linked, no input step: {r}");

    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
}

/// A run whose code is scanned from the window (a real account) keeps the link inside the
/// process: with `KOURIER_DEV_QR_URL=0` the event says only that a code is shown.
#[test]
fn the_qr_link_stays_in_the_process_when_told_to() {
    let dir = scratch("dev-control-qr");
    let mut k = Kourier::start_env(&dir, "--fixture-link", &[("KOURIER_DEV_QR_URL", "0")]);
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    k.ok("add_account", json!({"network": "Fake"}));
    k.ok("wait", json!({"until": "login:input"}));
    k.ok("submit_login", json!({"values": ["No link please"]}));
    let qr = k.event("login_qr", |_| true);
    assert_eq!(qr["data"]["shown"], true, "{qr}");
    assert!(qr["data"].get("url").is_none(), "{qr}");
    k.event("login_step", |d| d["kind"] == "done");
    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
    // No protocol line carried the link.
    let seen: Vec<String> = k.events.iter().map(Value::to_string).collect();
    assert!(
        seen.iter().all(|line| !line.contains("fake-link://")),
        "{seen:?}"
    );
}
