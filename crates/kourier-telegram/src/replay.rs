//! TG-3 C1 (test only): replays a recording of TDLib (`testdata/rec-<name>.jsonl`, TD-4a) into the
//! scripted TDLib.
//!
//! - A recording is split into PARTS at its `kourierStep` lines (the part before the first step has
//!   no label).
//! - Each `kourierRequest` marker opens a recorded request; every object after it up to the line
//!   whose `@extra` is the marker's (the answer, paired by `@extra`) is that request's reply. Every
//!   other object is an update of its part, handed to [`ScriptedTd::push_update`] by [`push`].
//! - NEVER handed on (neither pushed nor in a reply): a marker, a step line, an `addedProxy` or a
//!   `proxies` object (the gate's pair, secret-bearing), and an answer to a request the recording
//!   has no marker for (the helper's own `addProxy`, `setOption`): counted in
//!   [`Recording::dropped`].
//! - [`Recording::rules`] turns the recorded requests into [`Rule`]s keyed on the request type and
//!   its identifying arguments ([`key`]: `chat_list/@type`; `chat_id` + `from_message_id`, ...);
//!   the k-th recorded request of one key answers the k-th such request (`nth`). A test appends
//!   its synthetic fallbacks AFTER them and asserts with [`used`] how many recorded answers the
//!   code under test took (Q-L11: a synthetic fallback never carries a test alone).
//! - The answers' recorded `@extra` and `@client_id` are removed (the scripted TDLib sets them).
//!   Nothing here prints a recorded value.

use std::collections::BTreeMap;

use kourier_tdlib::scripted::{Rule, ScriptedTd};
use serde_json::Value;

/// The objects of a recording that are never handed on.
const NEVER_HANDED_ON: [&str; 4] = ["kourierRequest", "kourierStep", "addedProxy", "proxies"];

/// One request of the recording with its reply (the objects up to and with its answer).
#[derive(Clone)]
pub(crate) struct Recorded {
    /// The request's `@type`.
    pub(crate) ty: String,
    /// The request's arguments as the marker holds them (`Value::Null` when it has none).
    pub(crate) args: Value,
    /// The objects after the marker up to the answer, the answer last.
    pub(crate) reply: Vec<Value>,
}

impl Recorded {
    /// The answer (the reply's last object).
    pub(crate) fn answer(&self) -> &Value {
        self.reply
            .last()
            .expect("a recorded request has its answer")
    }
}

/// One object of a part, in the recording's order.
#[derive(Clone)]
pub(crate) enum Item {
    Update(Value),
    Request(Recorded),
}

/// The lines between two step markers.
#[derive(Clone)]
pub(crate) struct Part {
    /// The step label that opens the part (`None` for the part before the first step).
    pub(crate) step: Option<String>,
    pub(crate) items: Vec<Item>,
}

impl Part {
    /// The updates of the part (what [`push`] hands on), in order.
    pub(crate) fn updates(&self) -> impl Iterator<Item = &Value> {
        self.items.iter().filter_map(|i| match i {
            Item::Update(v) => Some(v),
            Item::Request(_) => None,
        })
    }

    /// The recorded requests of the part, in order.
    pub(crate) fn requests(&self) -> impl Iterator<Item = &Recorded> {
        self.items.iter().filter_map(|i| match i {
            Item::Request(r) => Some(r),
            Item::Update(_) => None,
        })
    }
}

/// A recording, split and paired.
pub(crate) struct Recording {
    pub(crate) parts: Vec<Part>,
    /// Objects never handed on that were not markers (an `addedProxy`, a `proxies` object, an
    /// answer without a marker).
    pub(crate) dropped: usize,
}

impl Recording {
    /// `testdata/<name>`, read now. Panics when it is missing or malformed (test data).
    pub(crate) fn load(name: &str) -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        let text = std::fs::read_to_string(&path).expect("a recording in testdata/");
        Self::parse(&text)
    }

    /// A recording's text (JSON lines). Panics on a malformed one: a request opened while another
    /// waits for its answer, a step inside a request, a request without its answer.
    pub(crate) fn parse(text: &str) -> Self {
        let mut parts = vec![Part {
            step: None,
            items: Vec::new(),
        }];
        let mut open: Option<(Value, Recorded)> = None;
        let mut dropped = 0;
        for (n, line) in text.lines().map(str::trim).enumerate() {
            if line.is_empty() {
                continue;
            }
            let mut v: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("recording line {} is not JSON", n + 1));
            let ty = v["@type"].as_str().unwrap_or_default().to_owned();
            match ty.as_str() {
                "kourierStep" => {
                    assert!(open.is_none(), "line {}: a step inside a request", n + 1);
                    parts.push(Part {
                        step: Some(v["step"].as_str().unwrap_or_default().to_owned()),
                        items: Vec::new(),
                    });
                }
                "kourierRequest" => {
                    assert!(open.is_none(), "line {}: two requests in flight", n + 1);
                    let recorded = Recorded {
                        ty: v["request"].as_str().unwrap_or_default().to_owned(),
                        args: v.get("args").cloned().unwrap_or(Value::Null),
                        reply: Vec::new(),
                    };
                    open = Some((v["@extra"].clone(), recorded));
                }
                "addedProxy" | "proxies" => dropped += 1,
                _ => {
                    let extra = v.get("@extra").cloned();
                    if let Some(obj) = v.as_object_mut() {
                        obj.remove("@extra");
                        obj.remove("@client_id");
                    }
                    let answers =
                        matches!((&open, &extra), (Some((want, _)), Some(e)) if want == e);
                    if answers {
                        let (_, mut done) = open.take().expect("open");
                        done.reply.push(v);
                        parts
                            .last_mut()
                            .expect("a part")
                            .items
                            .push(Item::Request(done));
                    } else if extra.is_some() {
                        // An answer to a request the recording has no marker for.
                        dropped += 1;
                    } else if let Some((_, recorded)) = &mut open {
                        recorded.reply.push(v);
                    } else {
                        parts
                            .last_mut()
                            .expect("a part")
                            .items
                            .push(Item::Update(v));
                    }
                }
            }
        }
        assert!(open.is_none(), "a request without its answer");
        Self { parts, dropped }
    }

    /// The part that the step `label` opens. Panics when there is none.
    pub(crate) fn part(&self, label: &str) -> &Part {
        self.parts
            .iter()
            .find(|p| p.step.as_deref() == Some(label))
            .expect("a step of the recording")
    }

    /// The step labels in order (the unlabelled first part left out).
    pub(crate) fn steps(&self) -> Vec<&str> {
        self.parts
            .iter()
            .filter_map(|p| p.step.as_deref())
            .collect()
    }

    /// Every recorded request, in order.
    pub(crate) fn requests(&self) -> impl Iterator<Item = &Recorded> {
        self.parts.iter().flat_map(Part::requests)
    }

    /// The rules answering the recorded requests of `parts` (see the module docs).
    pub(crate) fn rules<'a>(parts: impl IntoIterator<Item = &'a Part>) -> Vec<Rule> {
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut rules = Vec::new();
        for r in parts.into_iter().flat_map(Part::requests) {
            let pairs = key(&r.ty, &r.args);
            let k = seen.entry(key_id(&r.ty, &pairs)).or_insert(0);
            *k += 1;
            let pairs: Vec<(&str, Value)> = pairs.into_iter().collect();
            rules.push(Rule {
                nth: Some(*k),
                ..Rule::when_all(&r.ty, &pairs, r.reply.clone())
            });
        }
        rules
    }
}

/// The identifying arguments of a request type: the JSON pointers a recorded answer is keyed on,
/// with the request's values there (a missing value is `null`).
pub(crate) fn key(ty: &str, req: &Value) -> Vec<(&'static str, Value)> {
    let pointers: &[&'static str] = match ty {
        "loadChats" => &["/chat_list/@type"],
        "getChatHistory" => &["/chat_id", "/from_message_id"],
        "getMessage" => &["/chat_id", "/message_id"],
        "getChat" | "viewMessages" => &["/chat_id"],
        _ => &[],
    };
    pointers
        .iter()
        .map(|p| (*p, req.pointer(p).cloned().unwrap_or(Value::Null)))
        .collect()
}

fn key_id(ty: &str, pairs: &[(&'static str, Value)]) -> (String, String) {
    let values: Vec<String> = pairs.iter().map(|(_, v)| v.to_string()).collect();
    (ty.to_owned(), values.join(","))
}

/// How many of `requests` (as the scripted TDLib saw them, in order) the recorded requests of
/// `parts` answered: the k-th request of a key counts while the parts hold at least k of it.
pub(crate) fn used<'a>(parts: impl IntoIterator<Item = &'a Part>, requests: &[Value]) -> usize {
    let mut recorded: BTreeMap<(String, String), usize> = BTreeMap::new();
    for r in parts.into_iter().flat_map(Part::requests) {
        *recorded
            .entry(key_id(&r.ty, &key(&r.ty, &r.args)))
            .or_insert(0) += 1;
    }
    let mut asked: BTreeMap<(String, String), usize> = BTreeMap::new();
    requests
        .iter()
        .filter(|req| {
            let ty = req["@type"].as_str().unwrap_or_default();
            let id = key_id(ty, &key(ty, req));
            let n = asked.entry(id.clone()).or_insert(0);
            *n += 1;
            recorded.get(&id).is_some_and(|have| *n <= *have)
        })
        .count()
}

/// Hands the updates of `part` to the scripted TDLib, in order. How many it pushed.
pub(crate) fn push(td: &ScriptedTd, part: &Part) -> usize {
    let mut n = 0;
    for u in part.updates() {
        debug_assert!(!NEVER_HANDED_ON.contains(&u["@type"].as_str().unwrap_or_default()));
        td.push_update(u.clone());
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests;
