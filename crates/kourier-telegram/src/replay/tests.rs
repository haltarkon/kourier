//! The recording replayer (TG-3 C1): the split at the steps, the pairing by `@extra`, what is never
//! handed on, and the rules answering the scripted TDLib with a count of the recorded answers used.
//! Nothing here prints a recorded value.

use std::ffi::CString;

use kourier_tdlib::scripted::{Rule, Script, ScriptedTd};
use kourier_tdlib::td::Engine;
use serde_json::{Value, json};

use super::*;

/// Every recording of `testdata/`, by name, read now.
pub(crate) fn recording_names() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("testdata/")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("rec-") && n.ends_with(".jsonl"))
        .collect();
    names.sort();
    names
}

/// `rec-channel-difference.jsonl`: three steps after the first part, eighteen requests each with
/// its answer last (paired by `@extra`: the loadChats pages, the 404 that ends a list, the
/// history pages).
#[test]
fn the_replayer_splits_at_steps_and_pairs_by_extra() {
    let rec = Recording::load("rec-channel-difference.jsonl");
    assert_eq!(rec.steps(), ["too-long-401", "too-long-402", "history-402"]);
    assert_eq!(rec.parts.len(), 4);
    assert_eq!(rec.parts[0].step, None);
    let requests: Vec<&Recorded> = rec.requests().collect();
    assert_eq!(requests.len(), 18);
    let types = |ty: &str| requests.iter().filter(|r| r.ty == ty).count();
    assert_eq!((types("loadChats"), types("getChatHistory")), (7, 11));
    for r in &requests {
        let answer = r.answer()["@type"].as_str().unwrap_or_default();
        assert!(
            ["ok", "error", "messages"].contains(&answer),
            "a {} is answered by {answer}",
            r.ty
        );
        assert!(
            r.answer().get("@extra").is_none(),
            "the recorded @extra is removed"
        );
        assert_eq!(r.args["@type"].as_str(), Some(r.ty.as_str()));
    }
    // The archive's end is its third page (a 404 pairs with its own marker).
    let archive: Vec<&&Recorded> = requests
        .iter()
        .filter(|r| r.ty == "loadChats" && r.args["chat_list"]["@type"] == "chatListArchive")
        .collect();
    assert_eq!(archive.len(), 3);
    assert_eq!(archive[2].answer()["code"], 404);
    // The too-long step of c402 streams its newest message as an update of the step.
    let newest: Vec<i64> = rec
        .part("too-long-402")
        .updates()
        .filter(|u| u["@type"] == "updateNewMessage")
        .filter_map(|u| u["message"]["id"].as_i64())
        .collect();
    assert_eq!(newest, [6 << 20]);
    // The last step holds the history request only (and TDLib's closing states).
    assert_eq!(rec.part("history-402").requests().count(), 1);
    assert_eq!(rec.dropped, 0);
}

/// No marker, step line, `addedProxy` or `proxies` object reaches an update or a reply, in any
/// recording; the kill recordings' `addedProxy` (the helper's own `addProxy` of the second run)
/// and the answers to unmarked requests are dropped.
#[test]
fn the_replayer_never_hands_on_a_marker_or_a_proxy() {
    let names = recording_names();
    assert_eq!(
        names.len(),
        19,
        "the nineteen recordings (TG-4 added rec-read-act, the two rec-first-live-* and rec-difference-too-long)"
    );
    let mut dropped = 0;
    for name in &names {
        let rec = Recording::load(name);
        dropped += rec.dropped;
        for part in &rec.parts {
            for item in &part.items {
                let objects: Vec<&Value> = match item {
                    Item::Update(v) => vec![v],
                    Item::Request(r) => r.reply.iter().collect(),
                };
                for o in objects {
                    let ty = o["@type"].as_str().unwrap_or_default();
                    assert!(
                        !NEVER_HANDED_ON.contains(&ty),
                        "{name}: a {ty} is handed on"
                    );
                    assert!(
                        o.get("@client_id").is_none(),
                        "{name}: a recorded @client_id"
                    );
                }
            }
        }
    }
    // Seven recordings start a second helper run (its addProxy answer and a setOption's ok).
    assert!(dropped >= 7, "the second runs' own answers are dropped");
}

fn send(td: &ScriptedTd, client: i32, v: &Value) {
    td.send(client, &CString::new(v.to_string()).expect("no NUL"));
}

fn drain(td: &ScriptedTd) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(line) = td.receive(0.0) {
        out.push(serde_json::from_str(&line).expect("JSON"));
    }
    out
}

/// The rules answer each recorded request with its recorded reply (the k-th of a key the k-th),
/// a request outside the recording falls to the synthetic rule after them, and [`used`] counts
/// exactly the recorded answers taken (Q-L11).
#[test]
fn the_rules_answer_recorded_requests_and_count_what_was_used() {
    let rec = Recording::load("rec-channel-difference.jsonl");
    let mut rules = Recording::rules(&rec.parts);
    rules.push(Rule::new(
        "getChatHistory",
        vec![json!({"@type": "messages", "total_count": 0, "messages": []})],
    ));
    let td = ScriptedTd::new(Script {
        rules,
        ..Script::default()
    });
    let c = td.create_client();
    send(&td, c, &json!({"@type": "setTdlibParameters", "@extra": 1}));
    td.push_update(json!({"@type": "updateAuthorizationState",
        "authorization_state": {"@type": "authorizationStateReady"}}));
    drain(&td);
    let recorded: Vec<Recorded> = rec.requests().cloned().collect();
    for (n, r) in recorded.iter().enumerate() {
        let mut req = r.args.clone();
        req["@extra"] = json!(100 + n);
        send(&td, c, &req);
        let got = drain(&td);
        assert_eq!(got.len(), r.reply.len(), "request {n}: the whole reply");
        let answer = got.last().expect("an answer");
        assert_eq!(answer["@type"], r.answer()["@type"], "request {n}");
        assert_eq!(answer["@extra"], 100 + n, "request {n}: its own @extra");
    }
    // One more history page of c402 from its newest id: not recorded, the synthetic rule.
    send(
        &td,
        c,
        &json!({"@type": "getChatHistory", "chat_id": -1_000_000_000_402_i64,
                "from_message_id": 6 << 20, "offset": 0, "limit": 20, "only_local": false,
                "@extra": 999}),
    );
    assert_eq!(drain(&td).last().expect("an answer")["total_count"], 0);
    let asked: Vec<Value> = td.requests().into_iter().map(|(_, v)| v).collect();
    assert_eq!(used(&rec.parts, &asked), 18);
    assert_eq!(
        used(&rec.parts[..1], &asked),
        17,
        "the first part's requests"
    );
    // The main list was loaded twice (three pages, then one 404): four recorded main pages, the
    // k-th answered by the k-th rule of that key.
    let mains = rec
        .requests()
        .filter(|r| r.ty == "loadChats" && r.args["chat_list"]["@type"] == "chatListMain")
        .count();
    assert_eq!(mains, 4);
}

/// Pushing a part hands its updates on, in order, and nothing else.
#[test]
fn a_pushed_part_is_its_updates_in_order() {
    let rec = Recording::load("rec-reconnect.jsonl");
    let td = ScriptedTd::new(Script::default());
    let pushed: usize = rec.parts.iter().map(|p| push(&td, p)).sum();
    let got = drain(&td);
    assert_eq!(got.len(), pushed);
    let want: Vec<&Value> = rec.parts.iter().flat_map(Part::updates).collect();
    let types = |vs: Vec<&Value>| -> Vec<String> {
        vs.iter()
            .map(|v| v["@type"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(types(got.iter().collect()), types(want));
    assert_eq!(rec.requests().count(), 0);
}
