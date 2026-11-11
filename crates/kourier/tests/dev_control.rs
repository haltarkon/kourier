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
        Self::start_args(dir, &[mode, "--dev-control", "--size=1000x700"], env, false)
    }

    /// The binary with exactly `args`; `offline` starts it under `unshare -rn` (an empty
    /// network namespace: no interface, no route), so whatever it does it reaches no network.
    fn start_args(dir: &Path, args: &[&str], env: &[(&str, &str)], offline: bool) -> Self {
        let mut command = if offline {
            let mut command = Command::new("unshare");
            command.args(["-rn", env!("CARGO_BIN_EXE_kourier")]);
            command
        } else {
            Command::new(env!("CARGO_BIN_EXE_kourier"))
        };
        let mut child = command
            .args(args)
            .envs(env.iter().copied())
            // Whatever the test run inherited, a number tail is the test's own business.
            .env_remove("KOURIER_DEV_EXPECT_NUMBER_TAIL")
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
    // The number check a run tool reads before it says "scan now": a fixture run has no tail,
    // and the key is there (a missing key must not read as "off" by accident).
    assert_eq!(state["expected_number_check"], json!(false), "{state}");
    // BQ11 (A10d D8): a fixture run has no real Signal backend, so its Storage Service counts
    // are not measured: the key is there and `null`, never a row of zeros. Mutant: a default of 0.
    assert!(
        state.get("storage").is_some_and(Value::is_null),
        "the storage counts are not null in a fixture run: {state}"
    );

    // Nothing can be attached before a conversation is open.
    let notes = dir.join("Route notes.txt");
    std::fs::write(&notes, "08:00 trailhead\n").expect("a file to attach");
    let r = k.request("attach", json!({"path": notes.to_string_lossy()}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");

    // T2 (T-1, M12; folded in here from a test of its own, C8: no extra app start in this file):
    // `chat_facts` reads the CORE's row, so it sees the archived chat the shown inbox cannot (the
    // fixture has exactly one), and for every shown chat it says what `chats` says of the phone's
    // settings (`pinned`, `muted`, `marked_unread`); the fixture has a pinned chat, so the two
    // agree on a true as well as on false. `chat_facts` of a chat the core does not have is
    // `notFound`, and it takes a conversation id and nothing else. No chat is open yet, so the
    // shown view and the core's row cannot be in the middle of an open chat's read mark.
    // Mutant: `archived` read from the shown view (the archived chat then reads false).
    {
        let chats = k.ok("chats", json!({}));
        let shown: Vec<i64> = chats["chats"]
            .as_array()
            .expect("chats")
            .iter()
            .map(|c| c["conversation"].as_i64().expect("id"))
            .collect();
        let mut pinned_shown = 0;
        for chat in chats["chats"].as_array().expect("chats") {
            let id = chat["conversation"].as_i64().expect("id");
            let facts = k.ok("chat_facts", json!({"conversation": id}));
            for key in ["pinned", "muted", "marked_unread"] {
                assert!(chat[key].is_boolean(), "chats has {key}: {chat}");
                assert_eq!(facts[key], chat[key], "{key}: {facts} {chat}");
            }
            // The newest thing's time is the shown row's own (T2 T-1c: how a driver sees that
            // something arrived in a chat the list hides).
            assert!(facts["last_activity"].is_i64(), "{facts}");
            assert_eq!(
                facts["last_activity"], chat["last_activity"],
                "{facts} {chat}"
            );
            assert_eq!(
                facts["archived"],
                json!(false),
                "a shown chat is no archived one: {facts}"
            );
            pinned_shown += usize::from(chat["pinned"] == json!(true));
        }
        assert!(pinned_shown > 0, "the fixture shows a pinned chat: {chats}");

        // The ids of the profile are small row ids; the archived chat is among the ones `chats`
        // does not list.
        let mut archived = Vec::new();
        for id in 1..=40_i64 {
            if shown.contains(&id) {
                continue;
            }
            let r = k.request("chat_facts", json!({"conversation": id}));
            if r["ok"] == json!(true) && r["result"]["archived"] == json!(true) {
                archived.push(id);
            }
        }
        assert_eq!(
            archived.len(),
            1,
            "the fixture's one archived chat: {archived:?}"
        );

        let r = k.request("chat_facts", json!({"conversation": 999_999}));
        assert_eq!(r["error"]["code"], "notFound", "{r}");
        for args in [
            json!({}),
            json!({"conversation": "7"}),
            json!({"title": "x"}),
        ] {
            let r = k.request("chat_facts", args);
            assert_eq!(r["error"]["code"], "badRequest", "{r}");
        }
    }

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
                "marked_unread",
                "muted",
                "newest_own",
                "pinned",
                "self",
                "unread"
            ]
        );
        assert!(chat["conversation"].as_i64().is_some_and(|id| id > 0));
        assert!(["direct", "group", "other"].contains(&chat["kind"].as_str().unwrap_or("")));
        // The phone's settings are marks too (T2): booleans, no names.
        for key in ["pinned", "muted", "marked_unread"] {
            assert!(chat[key].is_boolean(), "{key}: {chat}");
        }
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

    // T2 (C7): the guards of `send` and `act`, decided on the Qt thread in the call that sends or
    // acts. `other` is a chat that is NOT the open one: a send or an act that names it is refused
    // and nothing is sent or done; an act that names a message the open chat does not have, or
    // one of the other kind than `on` says, is refused too; bad ids are bad requests (a typo never
    // turns a guard off).
    let own_rows = |state: &Value| {
        state["rows"].as_array().map_or(0, |rows| {
            rows.iter().filter(|r| r["own"] == json!(true)).count()
        })
    };
    let before = k.ok("state", json!({}));
    let refused = |r: &Value, what: &str| {
        assert_eq!(r["ok"], json!(false), "{what}: {r}");
        // The guard's own code (T2 G1): the run tool settles a refused frame on it, never on the words.
        assert_eq!(r["error"]["code"], "guardRefused", "{what}: {r}");
        let why = r["error"]["message"].as_str().unwrap_or_default();
        assert!(why.starts_with("refused"), "{what}: {r}");
    };
    let r = k.request(
        "send",
        json!({"text": "this text must not be sent", "conversation": other}),
    );
    refused(&r, "a send naming a chat that is not open");
    let r = k.request(
        "act",
        json!({"on": "own", "action": "react", "emoji": "👍", "conversation": other}),
    );
    refused(&r, "an act naming a chat that is not open");
    let r = k.request(
        "act",
        json!({"on": "own", "action": "react", "emoji": "👍", "message": 999_999_999}),
    );
    refused(&r, "an act naming a message the open chat does not have");
    // A message of the open chat, but not of the kind `on` says.
    let a_peer_message = before["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["own"] == json!(false)))
        .map(|r| r["message"].clone())
        .expect("the open chat holds a message of someone else");
    let r = k.request(
        "act",
        json!({"on": "own", "action": "react", "emoji": "👍", "message": a_peer_message}),
    );
    refused(&r, "an act on a peer's message with on: own");
    for bad in [
        json!({"text": "x", "conversation": 0}),
        json!({"text": "x", "conversation": "7"}),
    ] {
        let r = k.request("send", bad);
        assert_eq!(r["error"]["code"], "badRequest", "{r}");
    }
    // Nothing was sent or done by any of them.
    k.ok(
        "wait",
        json!({"until": "none_pending", "timeout_ms": 30_000}),
    );
    let after = k.ok("state", json!({}));
    assert_eq!(own_rows(&after), own_rows(&before), "{after}");
    assert_eq!(after["newest_own"], before["newest_own"], "{after}");
    assert_eq!(
        after["rows"], before["rows"],
        "a refused command changed a row"
    );

    // The same commands naming the open chat go through.
    k.ok(
        "send",
        json!({"text": "sent through the dev channel", "conversation": opened["conversation"]}),
    );
    let state = k.ok(
        "wait",
        json!({"until": "outgoing_state:Sent", "timeout_ms": 30_000}),
    );
    assert!(["sent", "delivered", "read"].contains(&state["observed"].as_str().unwrap_or("")));

    // F3 (SENDS-3): the composer is the user's. A send that names a chat goes only from a PLAIN composer: with
    // a reply he picked in it, it is refused in the very call and nothing is sent (the reply would ride along
    // with the tool's text, quoting a message of his); an edit of the tool's own message is refused the same way.
    // An unguarded send (the demo's, the smoke tool's) goes as ever. A text of one control character is no text
    // once normalised, so the reply cannot be sent and its banner stays up: that is how the test picks a reply.
    let plain = k.ok("state", json!({}));
    assert_eq!(plain["composer"]["replying"], json!(false), "{plain}");
    assert_eq!(plain["composer"]["editing"], json!(false), "{plain}");
    let a_peer_message = plain["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["own"] == json!(false)))
        .map(|r| r["message"].clone())
        .expect("the open chat holds a message of someone else");
    let an_own_message = plain["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["own"] == json!(true)))
        .map(|r| r["message"].clone())
        .expect("the open chat holds a message of ours");
    let r = k.request(
        "act",
        json!({"on": "other", "action": "reply", "text": "\u{1}", "message": a_peer_message}),
    );
    assert_eq!(r["ok"], json!(false), "a reply with no text was sent: {r}");
    let replying = k.ok("state", json!({}));
    assert_eq!(replying["composer"]["replying"], json!(true), "{replying}");
    for (what, cmd, args) in [
        (
            "a send from a composer that is replying",
            "send",
            json!({"text": "this text must not ride along with a reply", "conversation": opened["conversation"]}),
        ),
        (
            "an edit while a reply is picked",
            "act",
            json!({"on": "own", "action": "edit", "text": "this edit must not go",
                "conversation": opened["conversation"], "message": an_own_message}),
        ),
    ] {
        let r = k.request(cmd, args);
        refused(&r, what);
        let why = r["error"]["message"].as_str().unwrap_or_default();
        assert!(why.contains("composer"), "{what}: {r}");
    }
    k.ok(
        "wait",
        json!({"until": "none_pending", "timeout_ms": 30_000}),
    );
    let still = k.ok("state", json!({}));
    assert_eq!(
        still["rows"], replying["rows"],
        "a refused command changed a row"
    );
    assert_eq!(still["composer"]["replying"], json!(true), "{still}");
    // The same text without a chat named goes (the old rule) and closes the banner.
    k.ok("send", json!({"text": "the reply goes out unguarded"}));
    k.ok(
        "wait",
        json!({"until": "none_pending", "timeout_ms": 30_000}),
    );
    let closed = k.ok("state", json!({}));
    assert_eq!(closed["composer"]["replying"], json!(false), "{closed}");
    assert_eq!(
        own_rows(&closed),
        own_rows(&still) + 1,
        "the unguarded send did not go: {closed}"
    );

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
    // F3: the refused file stays in the strip, and a guarded send from a composer that holds a file is refused
    // too (the files would go with the text as their caption): nothing is sent.
    let r = k.request(
        "send",
        json!({"text": "no file rides along", "conversation": opened["conversation"]}),
    );
    refused(&r, "a send from a composer that holds a file");
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("files")),
        "{r}"
    );
    let held = k.ok("state", json!({}));
    assert_eq!(held["rows"], st["rows"], "a refused send changed a row");
    // T2 (C7): with a `message` the act takes exactly that message, not the newest own one: the
    // middle one of the three just sent (rows[1], an image) gets the reaction, the newest (rows[0])
    // does not.
    let (middle, newest) = (rows[1]["message"].clone(), rows[0]["message"].clone());
    let named = k.ok(
        "act",
        json!({"on": "own", "action": "react", "emoji": "🎉",
            "conversation": opened["conversation"], "message": middle}),
    );
    assert_eq!(named["message"], middle, "{named}");
    let deadline = Instant::now() + WAIT;
    loop {
        let st = k.ok("state", json!({}));
        let row = |id: &Value| {
            st["rows"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| &r["message"] == id).cloned())
                .expect("the row")
        };
        if row(&middle)["reactions"] == json!([{"emoji": "🎉", "count": 1, "mine": true}]) {
            assert_eq!(
                row(&newest)["reactions"],
                json!([]),
                "the newest own message took the named one's reaction: {st}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the named message's reaction never showed: {st}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// `state` until `ready` accepts it (polled; the state of a fixture page changes a moment after
/// the window is up).
fn state_when(k: &mut Kourier, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state = k.ok("state", json!({}));
        if ready(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {state}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// T1a (A4): the history a network pushes after a link is `state.transfer.phase == "history"`.
/// The STATE banner (`state.banner`) never says so, which is why the run tool reads the
/// transfer's own field: it waits on this phase for the end of the push. The page `wahistory` is
/// the fixture account of WhatsApp while the phone is still sending. A plain run shows no
/// transfer (`phase` is `""`, `progress` null), no usage-statistics counters (`wam` null: the
/// real backend is not in it) and no rehearsal. Mutant: the phase read from the state banner.
#[test]
fn the_state_reports_the_history_phase() {
    let dir = scratch("dev-control-history-phase");
    let mut k = Kourier::start_args(
        &dir,
        &[
            "--fixture",
            "--page=wahistory",
            "--dev-control",
            "--size=1000x700",
        ],
        &[],
        false,
    );
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    let state = state_when(&mut k, "the history phase", |s| {
        s["transfer"]["phase"] == "history"
    });
    assert_ne!(
        state["banner"], "history",
        "the state banner has no such kind: {state}"
    );
    let progress = &state["transfer"]["progress"];
    assert!(
        progress.is_null() || progress.as_f64().is_some_and(|p| (0.0..=1.0).contains(&p)),
        "progress is a fraction or null: {state}"
    );
    assert_eq!(state["rehearsal"], json!(false), "{state}");
    assert!(state["wam"].is_null(), "{state}");
    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
}

/// T1a (A4): `rehearsal_arrive` exists only in a run that plays WhatsApp with the rehearsal
/// fake. A plain run (here the fixture) refuses it with `failed` and a message that names the
/// missing network, says `rehearsal: false` and shows no transfer; a kind that is neither
/// `direct` nor `group` is a bad request. Mutant: `AppHandle::rehearsal_arrive` without its check.
#[test]
fn rehearsal_arrive_is_refused_without_the_rehearsal_network() {
    let dir = scratch("dev-control-no-rehearsal");
    let mut k = Kourier::start(&dir);
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    k.ok("wait", json!({"until": "conversations_at_least:3"}));
    let state = k.ok("state", json!({}));
    assert_eq!(state["rehearsal"], json!(false), "{state}");
    assert_eq!(state["transfer"]["phase"], json!(""), "{state}");
    assert!(state["transfer"]["progress"].is_null(), "{state}");
    assert!(state["wam"].is_null(), "{state}");
    // T2 (C3): "not measured" outside a rehearsal, never a 0.
    assert!(state["frames"].is_null(), "{state}");
    for kind in ["direct", "group"] {
        let r = k.request("rehearsal_arrive", json!({"kind": kind}));
        assert_eq!(r["ok"], json!(false), "{kind}: {r}");
        assert_eq!(r["error"]["code"], "failed", "{kind}: {r}");
        let message = r["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("no rehearsal network"), "{kind}: {r}");
    }
    let r = k.request("rehearsal_arrive", json!({"kind": "other"}));
    assert_eq!(r["error"]["code"], "badRequest", "{r}");
    // T2 (T-5): every act of the phone is refused too, in a run that plays no rehearsal.
    for act in [
        "archive",
        "unarchive",
        "pin",
        "unpin",
        "mute",
        "unmute",
        "call",
        "peer_react",
        "peer_edit",
        "peer_delete",
    ] {
        let r = k.request("rehearsal_act", json!({"kind": "direct", "act": act}));
        assert_eq!(r["ok"], json!(false), "{act}: {r}");
        assert_eq!(r["error"]["code"], "failed", "{act}: {r}");
        let message = r["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("no rehearsal network"), "{act}: {r}");
    }
    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
}

/// Whether `unshare -rn` works here (user namespaces can be disabled).
fn unshare_works() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

/// T1a (A4): a REHEARSAL run, the way `cargo xtask wa-run rehearse` starts the app: the normal
/// launch (no fixture) on a throwaway profile, in an empty network namespace, with
/// `KOURIER_DEV_REHEARSAL=whatsapp` and WhatsApp turned on in the preferences. The rehearsal
/// network plays WhatsApp (no real client exists; nothing could reach a network anyway): the
/// account links by QR, the phone pushes its history (`transfer.phase == "history"`, which ends),
/// and `rehearsal_arrive` puts one live text into the first chat of a kind: its `unread` grows by
/// one each time, a second arrival in the same chat works, and a refused arrival before the
/// account runs says so. `wam` is null here (the real backend is not in the run).
/// Skipped where `unshare -rn` is unavailable, FAILS there when `KOURIER_REQUIRE_UNSHARE=1`.
#[test]
fn a_rehearsal_run_takes_one_arrival() {
    if !unshare_works() {
        assert!(
            std::env::var("KOURIER_REQUIRE_UNSHARE").as_deref() != Ok("1"),
            "`unshare -rn` is not available and KOURIER_REQUIRE_UNSHARE=1: this test must run"
        );
        eprintln!("SKIPPED: `unshare -rn` is not available (user namespaces)");
        return;
    }
    let dir = scratch("dev-control-rehearsal");
    let home = dir.join("home");
    let prefs = home.join("kourier/config/kourier.toml");
    std::fs::create_dir_all(prefs.parent().expect("config dir")).expect("config dir");
    std::fs::write(&prefs, "networks-on = [\"WhatsApp\"]\n").expect("prefs");
    let home_text = home.to_string_lossy().into_owned();
    let mut k = Kourier::start_args(
        &dir,
        &["--dev-control", "--size=1000x700"],
        &[
            ("KOURIER_HOME", home_text.as_str()),
            ("KOURIER_DEV_KEYRING", "passphrase"),
            ("KOURIER_DEV_REHEARSAL", "whatsapp"),
            ("KOURIER_DESKTOP", "0"),
            ("KOURIER_DEV_QR_URL", "0"),
            ("KOURIER_DEV_NO_DUMP", "1"),
        ],
        true,
    );
    k.line("the channel event", |v| v["event"] == "channel");
    k.ok("wait", json!({"until": "loaded", "timeout_ms": 60_000}));
    let state = k.ok("state", json!({}));
    assert_eq!(state["rehearsal"], json!(true), "{state}");
    assert!(state["wam"].is_null(), "{state}");
    assert_eq!(state["expected_number_check"], json!(false), "{state}");
    // T2 (C3): measured in a rehearsal, and nothing has been sent.
    assert_eq!(state["frames"], json!(0), "{state}");

    // No account runs yet: the phone has nobody to write to.
    let r = k.request("rehearsal_arrive", json!({"kind": "direct"}));
    assert_eq!(r["error"]["code"], "failed", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("refused: ")),
        "{r}"
    );

    k.ok("add_account", json!({"network": "WhatsApp"}));
    let qr = k.event("login_qr", |_| true);
    assert_eq!(qr["data"]["shown"], json!(true), "{qr}");
    k.ok(
        "wait",
        json!({"until": "account_active", "timeout_ms": 90_000}),
    );
    k.ok(
        "wait",
        json!({"until": "conversations_at_least:2", "timeout_ms": 60_000}),
    );
    // The phone pushes its history for about twenty seconds.
    state_when(&mut k, "the history phase", |s| {
        s["transfer"]["phase"] == "history"
    });

    let unread = |k: &mut Kourier, kind: &str| -> u64 {
        let chats = k.ok("chats", json!({}));
        chats["chats"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|c| c["kind"] == kind && c["self"] == false)
            })
            .and_then(|c| c["unread"].as_u64())
            .unwrap_or_else(|| panic!("no {kind} chat: {chats}"))
    };
    for (kind, times) in [("direct", 2_u64), ("group", 1)] {
        let before = unread(&mut k, kind);
        for n in 1..=times {
            let arrived = k.ok("rehearsal_arrive", json!({"kind": kind}));
            assert_eq!(arrived["kind"], json!(kind), "{arrived}");
            let deadline = Instant::now() + Duration::from_secs(30);
            while unread(&mut k, kind) < before + n {
                assert!(Instant::now() < deadline, "{kind} arrival {n} did not show");
                std::thread::sleep(Duration::from_millis(200));
            }
            assert_eq!(unread(&mut k, kind), before + n, "{kind} arrival {n}");
        }
    }

    // T2 (T-1): the core's row of each chat says what the shown list says, the group states its
    // members (the rehearsal's peer is its one admin), a chat the core does not have is `notFound`,
    // and the arrivals sent nothing: the fake took no frame.
    let chats = k.ok("chats", json!({}));
    for chat in chats["chats"].as_array().expect("chats") {
        let id = chat["conversation"].as_i64().expect("id");
        let facts = k.ok("chat_facts", json!({"conversation": id}));
        assert_eq!(facts["conversation"], json!(id), "{facts}");
        assert_eq!(facts["kind"], chat["kind"], "{facts} {chat}");
        for key in ["pinned", "muted", "marked_unread"] {
            assert!(chat[key].is_boolean(), "chats has {key}: {chat}");
            assert_eq!(facts[key], chat[key], "{key}: {facts} {chat}");
        }
        // The newest thing's time is the shown row's own (T2 T-1c: how a driver sees that
        // something arrived in a chat the list hides).
        assert!(facts["last_activity"].is_i64(), "{facts}");
        assert_eq!(
            facts["last_activity"], chat["last_activity"],
            "{facts} {chat}"
        );
        assert_eq!(facts["archived"], json!(false), "{facts}");
        assert_eq!(facts["calls"], json!([]), "{facts}");
        if chat["kind"] == "group" {
            assert_eq!(facts["admins"], json!(1), "{facts}");
            assert!(facts["members"].as_u64() >= Some(2), "{facts}");
        } else {
            assert!(facts.get("members").is_none(), "{facts}");
        }
    }
    let r = k.request("chat_facts", json!({"conversation": 999_999}));
    assert_eq!(r["error"]["code"], "notFound", "{r}");
    let state = k.ok("state", json!({}));
    assert_eq!(
        state["frames"],
        json!(0),
        "the arrivals sent nothing: {state}"
    );

    // T2 (T-5, C1): the phone's acts on cue. The shapes first: an unknown act is a bad request,
    // a call into a group is refused, and none of them is a frame.
    let r = k.request("rehearsal_act", json!({"kind": "direct", "act": "send"}));
    assert_eq!(r["error"]["code"], "badRequest", "{r}");
    let r = k.request("rehearsal_act", json!({"kind": "group", "act": "call"}));
    assert_eq!(r["error"]["code"], "failed", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("refused: ") && m.contains("call")),
        "{r}"
    );
    let chats = k.ok("chats", json!({}));
    let id_of = |kind: &str| -> i64 {
        chats["chats"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|c| c["kind"] == kind && c["self"] == false)
            })
            .and_then(|c| c["conversation"].as_i64())
            .unwrap_or_else(|| panic!("no {kind} chat: {chats}"))
    };
    let (direct, group) = (id_of("direct"), id_of("group"));
    let facts_of = |k: &mut Kourier, id: i64| k.ok("chat_facts", json!({"conversation": id}));
    let facts_when = |k: &mut Kourier, id: i64, what: &str, ready: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let facts = facts_of(k, id);
            if ready(&facts) {
                return facts;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {facts}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };

    // The settings: the direct chat pinned and muted, the group archived. The core's row says so
    // and the shown list drops the archived group.
    for (kind, act) in [("direct", "pin"), ("direct", "mute"), ("group", "archive")] {
        let r = k.ok("rehearsal_act", json!({"kind": kind, "act": act}));
        assert_eq!(r["act"], json!(act), "{r}");
    }
    facts_when(&mut k, direct, "the pin and the mute", &|f| {
        f["pinned"] == json!(true) && f["muted"] == json!(true)
    });
    facts_when(&mut k, group, "the archive", &|f| {
        f["archived"] == json!(true)
    });
    let chats = k.ok("chats", json!({}));
    let shown = |id: i64| {
        chats["chats"]
            .as_array()
            .and_then(|rows| rows.iter().find(|c| c["conversation"] == json!(id)))
            .cloned()
    };
    let row = shown(direct).expect("the pinned chat is shown");
    assert_eq!(
        (row["pinned"].clone(), row["muted"].clone()),
        (json!(true), json!(true)),
        "{row}"
    );
    assert!(shown(group).is_none(), "the archived group is still shown");
    for (kind, act) in [
        ("direct", "unpin"),
        ("direct", "unmute"),
        ("group", "unarchive"),
    ] {
        k.ok("rehearsal_act", json!({"kind": kind, "act": act}));
    }
    facts_when(&mut k, direct, "the pin and the mute taken off", &|f| {
        f["pinned"] == json!(false) && f["muted"] == json!(false)
    });
    facts_when(&mut k, group, "the group out of the archive", &|f| {
        f["archived"] == json!(false)
    });

    // A call rings, then it is answered on the phone: one line of the chat, first ringing, then
    // answered, incoming, 1:1.
    k.ok("rehearsal_act", json!({"kind": "direct", "act": "call"}));
    let ringing = facts_when(&mut k, direct, "the ring", &|f| {
        f["calls"].as_array().is_some_and(|c| !c.is_empty())
    });
    assert_eq!(ringing["calls"].as_array().map(Vec::len), Some(1));
    let answered = facts_when(&mut k, direct, "the answer", &|f| {
        f["calls"][0]["state"] == json!("answered")
    });
    assert_eq!(
        answered["calls"],
        json!([{"state": "answered", "direction": "incoming", "group": false}]),
        "{answered}"
    );

    // The other person reacts to, edits and deletes the text that arrived last in the direct
    // chat, each seen on the open chat's rows.
    k.ok("open_id", json!({"conversation": direct}));
    let rows_when = |k: &mut Kourier, what: &str, ready: &dyn Fn(&[Value]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = k.ok("state", json!({}));
            let rows = state["rows"].as_array().cloned().unwrap_or_default();
            if state["open"] == json!(direct) && ready(&rows) {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {state}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    k.ok(
        "rehearsal_act",
        json!({"kind": "direct", "act": "peer_react"}),
    );
    let rows = rows_when(&mut k, "the peer's reaction", &|rows| {
        rows.iter().any(|r| {
            r["reactions"].as_array().is_some_and(|chips| {
                chips
                    .iter()
                    .any(|c| c["count"] == json!(1) && c["mine"] == json!(false))
            })
        })
    });
    let reacted = rows
        .iter()
        .find(|r| r["reactions"].as_array().is_some_and(|c| !c.is_empty()))
        .and_then(|r| r["message"].as_i64())
        .expect("a reacted row");
    k.ok(
        "rehearsal_act",
        json!({"kind": "direct", "act": "peer_edit"}),
    );
    let rows = rows_when(&mut k, "the peer's edit", &|rows| {
        rows.iter().any(|r| r["edited"] == json!(true))
    });
    let edited = rows
        .iter()
        .find(|r| r["edited"] == json!(true))
        .and_then(|r| r["message"].as_i64());
    assert_eq!(edited, Some(reacted), "the edit is on the reacted text");
    k.ok(
        "rehearsal_act",
        json!({"kind": "direct", "act": "peer_delete"}),
    );
    let rows = rows_when(&mut k, "the peer's delete", &|rows| {
        rows.iter().any(|r| r["deleted"] == json!(true))
    });
    let deleted = rows
        .iter()
        .find(|r| r["deleted"] == json!(true))
        .and_then(|r| r["message"].as_i64());
    assert_eq!(deleted, Some(reacted), "the delete is on the same text");

    let state = k.ok("state", json!({}));
    assert_eq!(
        state["frames"],
        json!(0),
        "the phone's acts sent nothing: {state}"
    );
    k.ok("quit", json!({}));
    assert!(k.wait_exit().success());
}
