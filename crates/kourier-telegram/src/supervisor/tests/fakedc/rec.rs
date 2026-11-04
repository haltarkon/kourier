//! TD-4a C5 (brief §3.6-§3.7, critic H3/M5, decisions P-4/P-10): the RECORDING relay and the
//! recording cases RC1-RC5. Each case logs in by code over the helper's link (the recordings'
//! interest starts at Ready; the login's own stream is TG-2's `login-flow.jsonl`), then sends the
//! requests TG-3 will send and drives the fake DC's control socket. Every case ALWAYS asserts its
//! golden facts (a regression test of TDLib at the pin); it WRITES `<name>.raw.jsonl` (plus the fake
//! DC's `world.json` and wire log beside it) only under `KOURIER_TG_FAKEDC_RECORD`. Only counts,
//! fixed words and durations are printed.

use std::time::Instant;

use tokio::sync::oneshot;

use super::*;
use crate::link::{HelperLink, Inbound, drive};
use crate::receive::Fetch;
use crate::session::lane::LaneAsk;
use crate::supervisor::{AuthKind, Helper, StopReport};
use crate::sync::reconcile::PAGE_LIMIT;

/// The directory the raw recordings go to (`cargo xtask tdlib fakedc record`).
const RECORD_ENV: &str = "KOURIER_TG_FAKEDC_RECORD";

/// The requests whose marker also carries the request itself (P-10, critic M5): none of them holds
/// a key, a code, a password or a token. Every other request's marker holds only its `@type`.
pub(super) const ALLOW_LISTED: &[&str] = &[
    "loadChats",
    "getChatHistory",
    "getMessage",
    "getChat",
    "getChats",
    "viewMessages",
    "getMe",
];

/// Requests that never get a marker at all (P-10): the parameters (the database key) and every
/// login input (a number, a code, a password, a token).
fn unmarked(kind: &str) -> bool {
    kind == "setTdlibParameters"
        || kind.starts_with("setAuthentication")
        || kind.starts_with("checkAuthentication")
        || kind.starts_with("requestQrCode")
        || kind.contains("Password")
        || kind.contains("Token")
}

/// What the relay saw while recording: TDLib's own JSON lines (`U` frames) as they came, the
/// markers of the app's requests, and step lines of the case; each with its arrival time.
#[derive(Default)]
pub(super) struct Tape {
    pub(super) on: bool,
    pub(super) lines: Vec<(Instant, String)>,
    /// The end signals of the helper-to-app threads of the recording relays made so far, oldest
    /// first (`recording_relay` pushes one; `Drv::start` takes the newest: the relay of the helper
    /// it just started).
    relay_ends: Vec<oneshot::Receiver<()>>,
}

pub(super) type TapeRef = Arc<Mutex<Tape>>;

/// The stop reports of the helpers a case started (their gates' counts).
type Reports = Arc<Mutex<Vec<StopReport>>>;

impl Tape {
    fn push(&mut self, line: String) {
        if self.on {
            self.lines.push((Instant::now(), line));
        }
    }
}

/// The `@type` and `@extra` of a request, read without keeping anything else of it.
#[derive(serde::Deserialize)]
struct Probe {
    #[serde(rename = "@type")]
    kind: String,
    #[serde(rename = "@extra")]
    extra: Option<u64>,
}

/// The app's frames (`S <client> <json>`, `E <seq> <json>`) as marker lines (P-10).
fn marker_of(line: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?.trim_end();
    let rest = text
        .strip_prefix("S ")
        .or_else(|| text.strip_prefix("E "))?;
    let json = rest.split_once(' ')?.1;
    let probe: Probe = serde_json::from_str(json).ok()?;
    if unmarked(&probe.kind) {
        return None;
    }
    let mut marker = serde_json::json!({
        "@type": "kourierRequest",
        "request": probe.kind,
        "@extra": probe.extra,
    });
    if ALLOW_LISTED.contains(&probe.kind.as_str()) {
        let mut request: Value = serde_json::from_str(json).ok()?;
        if let Some(o) = request.as_object_mut() {
            o.remove("@extra");
        }
        marker["args"] = request;
    }
    Some(marker.to_string())
}

/// One direction of the recording relay: whole lines copied byte for byte; `record` turns a line
/// into what the tape keeps (or nothing). `ended`, when given, is sent as the LAST thing the
/// thread does, after every line is on the tape and passed on and the far end is shut down (the
/// signal [`Drv::stop`] waits for).
fn tape_lines(
    mut from: UnixStream,
    mut to: UnixStream,
    tape: TapeRef,
    record: fn(&[u8]) -> Option<String>,
    ended: Option<oneshot::Sender<()>>,
) {
    use std::io::Read as _;
    use std::net::Shutdown;

    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    'copy: while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        while let Some(at) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            {
                let mut t = tape.lock().expect("tape");
                if t.on
                    && let Some(kept) = record(&line)
                {
                    t.push(kept);
                }
            }
            if to.write_all(&line).is_err() {
                break 'copy;
            }
        }
    }
    let _ = to.write_all(&pending);
    let _ = to.shutdown(Shutdown::Write);
    if let Some(ended) = ended {
        let _ = ended.send(());
    }
}

/// TDLib's JSON of a `U` frame; the helper's protocol frames (`H`, `K`, `X`, `F`, `P`, `B`) are not
/// recorded.
fn update_of(line: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(line).ok()?.trim_end();
    text.strip_prefix("U ").map(str::to_owned)
}

/// The recording relay (brief §3.6): beside [`counting_relay`], the same byte-for-byte copy. The
/// helper-to-app thread's end is kept on the tape for [`Drv::start`] (the app-to-helper thread's
/// is not: a request marker is put on the tape before the helper can answer it).
pub(super) fn recording_relay(helper_end: UnixStream, tape: TapeRef) -> UnixStream {
    let (app_end, relay_end) = UnixStream::pair().expect("socket pair");
    let (from_app, to_app) = (relay_end.try_clone().expect("clone"), relay_end);
    let (from_helper, to_helper) = (helper_end.try_clone().expect("clone"), helper_end);
    let (ended, relay_ended) = oneshot::channel();
    tape.lock().expect("tape").relay_ends.push(relay_ended);
    let t = tape.clone();
    std::thread::spawn(move || tape_lines(from_app, to_helper, t, marker_of, None));
    std::thread::spawn(move || tape_lines(from_helper, to_app, tape, update_of, Some(ended)));
    app_end
}

/// The parts of the recording cases: [`fakedc_parts`] with the recording relay.
/// The ONE pre-0002 run's library (decisions P-9, brief §4.5): the test-only library made from the
/// pre-0002 base (`cargo xtask tdlib fakedc lib --base <target/tdlib-pre0002> --mutant-base`, which
/// `env` never lists). Only [`rc10_incoming_call_pre0002`] reads it; it is SKIPPED without it.
const PRE0002_LIB_ENV: &str = "KOURIER_TDJSON_FAKEDC_PRE0002";

/// What the pre-0002 test-only library's hello names: the patch tag is MISSING (the empty tag; the
/// library lacks patch 0002's run-time marker). Test code only: no real parts can name it.
const FAKEDC_PRE0002_PIN: crate::pin::Pin = crate::pin::Pin {
    patch_level: "",
    ..FAKEDC_PIN
};

fn rec_parts(dialer: Arc<dyn Dialer>, tape: TapeRef, pre0002: bool) -> (TgParts, Pids) {
    let lib_env = if pre0002 { PRE0002_LIB_ENV } else { LIB_ENV };
    let (helper, tdjson) = (helper_binary(), var(lib_env).expect("the library"));
    let verbose = required(VERBOSE_ENV);
    let pids: Pids = Arc::default();
    let started = pids.clone();
    let parts = TgParts::from_makers(
        move |args| {
            let spawned = if verbose {
                let spec = format!(
                    "verbose|{}|{}|{}|{}",
                    args.gate_port,
                    args.database_dir.display(),
                    args.files_dir.display(),
                    tdjson.display()
                );
                kourier_tdlib::spawn::spawn_self(HELPER, (HELPER_ENV, &spec))?
            } else {
                spawn(&SpawnArgs {
                    helper: helper.clone(),
                    tdjson: tdjson.clone(),
                    no_dump: true,
                    gate_port: args.gate_port,
                    database_dir: args.database_dir.clone(),
                    files_dir: args.files_dir.clone(),
                })?
            };
            started.lock().expect("pids").push(spawned.child.id());
            Ok(HelperProcess {
                sock: recording_relay(spawned.sock, tape.clone()),
                stderr: spawned.stderr.map(|e| Box::new(e) as Box<dyn Read + Send>),
                child: Box::new(spawned.child),
            })
        },
        dialer,
    )
    .requiring_lockdown(LockdownNeed::OnOrUnavailable)
    .expecting_pin(if pre0002 {
        &FAKEDC_PRE0002_PIN
    } else {
        &FAKEDC_PIN
    })
    .with_env(TgEnv::Test);
    (parts, pids)
}

// ----- the driver of one helper -----

/// One running helper driven over its link.
struct Drv {
    /// Where this helper's stop report goes (S-M2: the gate's counts of the whole case).
    reports: Reports,
    helper: Helper,
    link: HelperLink,
    client: i32,
    /// The first authorization state of this helper.
    first: Option<AuthKind>,
    /// Updates that came while a request waited (`wait_for` looks at them first).
    pending: std::collections::VecDeque<String>,
    /// The end of the recording relay's helper-to-app thread of this helper ([`Drv::stop`] waits
    /// for it).
    relay_ended: oneshot::Receiver<()>,
}

/// How long [`Drv::stop`] waits for the recording relay of the stopped helper to end: a guard
/// against a hang, never a timing fact (a relay ends within milliseconds of its helper's death).
const RELAY_END_BOUND: Duration = Duration::from_secs(30);

fn state_of(v: &Value) -> Option<&str> {
    (v["@type"] == "updateAuthorizationState")
        .then(|| v["authorization_state"]["@type"].as_str())
        .flatten()
}

impl Drv {
    async fn start(
        parts: &TgParts,
        a: &Account,
        rt: &tokio::runtime::Handle,
        reports: Reports,
        tape: &TapeRef,
    ) -> Drv {
        let cancel = CancellationToken::new();
        let (result, _) = start_reporting(
            parts,
            &start_of(a, TgEnv::Test),
            rt,
            &real_timings(),
            &cancel,
        )
        .await;
        let mut helper = match result {
            Ok(h) => h,
            Err(e) => panic!("the helper did not start: {e:?}"),
        };
        let first = helper.take_first().map(|f| f.state);
        let link = helper.link().clone();
        let client = helper.client;
        // The maker made this helper's recording relay just now: its end signal is the newest.
        let relay_ended = tape
            .lock()
            .expect("tape")
            .relay_ends
            .pop()
            .expect("the recording relay of the started helper");
        Drv {
            reports,
            helper,
            link,
            client,
            first,
            pending: Default::default(),
            relay_ended,
        }
    }

    /// One request; the updates that come meanwhile are kept for [`Self::wait_for`].
    async fn ask(&mut self, req: Value) -> Result<Value, LinkError> {
        let link = self.link.clone();
        let client = self.client;
        let pending = &mut self.pending;
        tokio::time::timeout(
            Duration::from_secs(30),
            drive(self.helper.inbound(), link.request(client, req), |f| {
                if let Inbound::Update(s) = f {
                    pending.push_back(s);
                }
            }),
        )
        .await
        .unwrap_or(Err(LinkError::Timeout))
    }

    /// Waits up to `secs` for an update (kept or inbound) that `pred` accepts.
    async fn wait_for(&mut self, secs: u64, pred: impl Fn(&Value) -> bool) -> bool {
        let accepts = |s: &str| serde_json::from_str::<Value>(s).is_ok_and(|v| pred(&v));
        while let Some(s) = self.pending.pop_front() {
            if accepts(&s) {
                return true;
            }
        }
        let by = tokio::time::Instant::now() + Duration::from_secs(secs);
        while let Ok(Some(frame)) = tokio::time::timeout_at(by, self.helper.inbound().recv()).await
        {
            if let Inbound::Update(s) = frame
                && accepts(&s)
            {
                return true;
            }
        }
        false
    }

    /// Drains the inbound frames for `secs`.
    async fn idle(&mut self, secs: f32) {
        self.pending.clear();
        let by = tokio::time::Instant::now() + Duration::from_secs_f32(secs);
        while let Ok(Some(_)) = tokio::time::timeout_at(by, self.helper.inbound().recv()).await {}
    }

    /// Drains the kept updates and the inbound ones until no UPDATE has come for `quiet` seconds
    /// (a heartbeat is not one), at most `most` seconds in all: a settle that waits for the
    /// stream to stop, not for a time. A step that waits for "the first update of this kind" must
    /// not meet one the load left behind (U11, rc9: `kept` by `ask` or still on the link).
    async fn quiet(&mut self, quiet: f32, most: f32) {
        self.pending.clear();
        let now = tokio::time::Instant::now;
        let (quiet, end) = (
            Duration::from_secs_f32(quiet),
            now() + Duration::from_secs_f32(most),
        );
        let mut last = now();
        loop {
            let by = (last + quiet).min(end);
            match tokio::time::timeout_at(by, self.helper.inbound().recv()).await {
                Ok(Some(Inbound::Update(_))) => last = now(),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    }

    /// Stops the helper and returns only after the recording relay of its frames ended: every
    /// frame it wrote is on the tape (`tape_lines` puts a line on the tape before it passes it
    /// on and signals its end after the last one). `Helper::stop` alone does not give that: it
    /// reads on for the link's end for at most one second and does not report an expiry. A relay
    /// that does not end within [`RELAY_END_BOUND`] is a hang: the case panics.
    async fn stop(self) {
        let report = self
            .helper
            .stop(&real_timings(), &CancellationToken::new())
            .await;
        self.reports.lock().expect("reports").push(report);
        // (a dropped sender: the relay thread died, it no longer runs)
        let ended = tokio::time::timeout(RELAY_END_BOUND, self.relay_ended).await;
        assert!(
            ended.is_ok(),
            "the recording relay of the stopped helper did not end"
        );
    }

    /// The login by code over the link (a fresh `tdlib/`): the number, the code, Ready.
    async fn login(&mut self) {
        assert_eq!(
            self.first,
            Some(AuthKind::WaitPhoneNumber),
            "not a fresh start"
        );
        let r = self
            .ask(serde_json::json!({
                "@type": "setAuthenticationPhoneNumber",
                "phone_number": NUMBER,
                "settings": {"@type": "phoneNumberAuthenticationSettings"},
            }))
            .await;
        assert!(r.is_ok(), "the number was refused: {:?}", r.err());
        assert!(
            self.wait_for(30, |v| state_of(v) == Some("authorizationStateWaitCode"))
                .await,
            "no code state"
        );
        let r = self
            .ask(serde_json::json!({"@type": "checkAuthenticationCode", "code": CODE}))
            .await;
        assert!(r.is_ok(), "the code was refused: {:?}", r.err());
        assert!(
            self.wait_for(30, |v| state_of(v) == Some("authorizationStateReady"))
                .await,
            "no Ready"
        );
    }

    /// `loadChats(list, 100)` until TDLib answers 404; the pages that answered `ok`.
    async fn load_all(&mut self, list: &str) -> (usize, bool) {
        let mut pages = 0;
        for _ in 0..50 {
            match self
                .ask(serde_json::json!({
                    "@type": "loadChats",
                    "chat_list": {"@type": list},
                    "limit": 100,
                }))
                .await
            {
                Ok(_) => pages += 1,
                Err(LinkError::Td { code: 404 }) => return (pages, true),
                Err(LinkError::Auth { code: 404, .. }) => return (pages, true),
                Err(_) => return (pages, false),
            }
        }
        (pages, false)
    }

    /// `getChatHistory(chat, from, 0, 20)`: the message ids it answered, newest first.
    async fn history(&mut self, chat: i64, from: i64) -> Vec<i64> {
        let r = self
            .ask(serde_json::json!({
                "@type": "getChatHistory",
                "chat_id": chat,
                "from_message_id": from,
                "offset": 0,
                "limit": 20,
                "only_local": false,
            }))
            .await
            .unwrap_or(Value::Null);
        r["messages"]
            .as_array()
            .map(|m| m.iter().filter_map(|x| x["id"].as_i64()).collect())
            .unwrap_or_default()
    }

    /// The read act's request, built by the PRODUCT's own [`LaneAsk::View`] (`viewMessages(chat,
    /// ids, source: null, force_read: true)`): what the recording proves TDLib accepts is what
    /// Kourier sends. The answer, or why none came.
    async fn view(&mut self, chat: i64, ids: &[i64]) -> Result<Value, LinkError> {
        let ask = LaneAsk::View {
            chat_id: chat,
            ids: ids.to_vec(),
        };
        self.ask(ask.request()).await
    }

    /// One page of the RECONCILE the way the PRODUCT asks for it (`Fetch::Gap`: `getChatHistory(chat,
    /// from, offset 0, limit PAGE_LIMIT, only_local: false)`): the message ids it answered, newest
    /// first. A refused page fails the case (the failure names a code, never a value).
    async fn page(&mut self, chat: i64, from: i64) -> Vec<i64> {
        let ask = Fetch::Gap {
            chat_id: chat,
            above: 0,
            from,
            limit: PAGE_LIMIT,
        };
        let answer = self.ask(ask.request()).await;
        assert!(
            answer.is_ok(),
            "a history page was refused: {:?}",
            answer.as_ref().err()
        );
        answer
            .map(|r| {
                r["messages"]
                    .as_array()
                    .map(|m| m.iter().filter_map(|x| x["id"].as_i64()).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// W1's pages: from `from` (the first live message after a restart), one page after the other,
    /// each from the OLDEST id the last one held, until an id at or below `s` (Kourier's stale
    /// cursor) came, a page was empty or [`PAGE_CALLS`] calls ran. Per call: its ids and the
    /// `messages.getHistory` requests the fake DC's wire saw during it (printed, never asserted:
    /// which call reaches the server is TDLib's timing; TDLib loads a channel's history through
    /// `messages.getHistory` too, `channels.getMessages` is by id).
    async fn pages_from(
        &mut self,
        fake: &FakeDc,
        chat: i64,
        from: i64,
        s: i64,
    ) -> Vec<(Vec<i64>, usize)> {
        let mut pages = Vec::new();
        let mut at = from;
        for _ in 0..PAGE_CALLS {
            let before = wire_count(&fake.names(), 0, "messages.getHistory");
            let ids = self.page(chat, at).await;
            let server = wire_count(&fake.names(), 0, "messages.getHistory") - before;
            let reached = ids.iter().any(|id| *id <= s);
            let oldest = ids.last().copied();
            pages.push((ids, server));
            match oldest {
                Some(oldest) if !reached => at = oldest,
                _ => break,
            }
        }
        pages
    }
}

/// How many pages [`Drv::pages_from`] asks at most (brief §3 W1).
const PAGE_CALLS: usize = 4;

// ----- reading the tape -----

/// The tape's updates (markers and steps included) as values, with their arrival times.
pub(super) fn values(tape: &TapeRef) -> Vec<(Instant, Value)> {
    tape.lock()
        .expect("tape")
        .lines
        .iter()
        .filter_map(|(at, l)| serde_json::from_str(l).ok().map(|v| (*at, v)))
        .collect()
}

/// A step line of the case (`{"@type":"kourierStep","step":<label>}`): the phases of the drift
/// test and of TG-3's replay (a restart, a kill, a control push); also a wire-log mark.
fn step_line(tape: &TapeRef, fake: &FakeDc, label: &str) {
    let mut t = tape.lock().expect("tape");
    let on = t.on;
    t.on = true;
    t.push(serde_json::json!({"@type": "kourierStep", "step": label}).to_string());
    t.on = on;
    drop(t);
    fake.control(&format!("mark {label}"));
}

/// Every chat id an object names (any key `chat_id`, at any depth).
fn chat_ids(v: &Value, out: &mut Vec<i64>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                if k == "chat_id"
                    && let Some(id) = x.as_i64()
                {
                    out.push(id);
                }
                chat_ids(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| chat_ids(x, out)),
        _ => {}
    }
}

/// RC1's ORDER fact, per helper run (a `kourierStep` `restart`/`kill` starts a new run; such a step
/// is stamped after the old run's driver stopped, see [`starts_a_run`]): every object naming a chat
/// comes after that chat's `updateNewChat` in the same run. The first offending `@type`, if any.
fn chat_before_reference(vs: &[(Instant, Value)]) -> Option<String> {
    let mut known = std::collections::HashSet::new();
    for (_, v) in vs {
        let kind = v["@type"].as_str().unwrap_or("");
        if kind == "kourierStep" {
            if starts_a_run(v["step"].as_str().unwrap_or("")) {
                known.clear();
            }
            continue;
        }
        if kind == "kourierRequest" {
            continue;
        }
        if kind == "updateNewChat" {
            if let Some(id) = v["chat"]["id"].as_i64() {
                known.insert(id);
            }
            continue;
        }
        let mut ids = Vec::new();
        chat_ids(v, &mut ids);
        if ids.iter().any(|id| *id != 0 && !known.contains(id)) {
            return Some(kind.to_owned());
        }
    }
    None
}

/// The `updateNewMessage`s of `chat` after the tape index `from`: (index, message id, time).
fn new_messages(vs: &[(Instant, Value)], from: usize, chat: i64) -> Vec<(usize, i64, Instant)> {
    vs.iter()
        .enumerate()
        .skip(from)
        .filter(|(_, (_, v))| {
            v["@type"] == "updateNewMessage" && v["message"]["chat_id"].as_i64() == Some(chat)
        })
        .filter_map(|(i, (at, v))| v["message"]["id"].as_i64().map(|id| (i, id, *at)))
        .collect()
}

/// The tape index of the step `label`.
fn step_at(vs: &[(Instant, Value)], label: &str) -> usize {
    vs.iter()
        .position(|(_, v)| v["@type"] == "kourierStep" && v["step"] == label)
        .unwrap_or(vs.len())
}

/// The unread chats TDLib reported (the last `unread_count` per chat).
fn unread_chats(vs: &[(Instant, Value)]) -> Vec<i64> {
    let mut unread: std::collections::BTreeMap<i64, i64> = Default::default();
    for (_, v) in vs {
        match v["@type"].as_str() {
            Some("updateNewChat") => {
                if let (Some(id), Some(n)) =
                    (v["chat"]["id"].as_i64(), v["chat"]["unread_count"].as_i64())
                {
                    unread.insert(id, n);
                }
            }
            Some("updateChatReadInbox") => {
                if let (Some(id), Some(n)) = (v["chat_id"].as_i64(), v["unread_count"].as_i64()) {
                    unread.insert(id, n);
                }
            }
            _ => {}
        }
    }
    unread
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(id, _)| id)
        .collect()
}

/// The wire log's names after the `n`-th entry.
fn wire_count(names: &[String], from: usize, name: &str) -> usize {
    names
        .iter()
        .skip(from)
        .filter(|n| n.as_str() == name)
        .count()
}

/// The types TDLib emits by itself, on its own timing (start-up lists, counters, statuses): left
/// out of the drift comparison (brief §3.10).
const DRIFT_IGNORED: &[&str] = &[
    "updateOption",
    "updateDefaultBackground",
    "updateUserStatus",
    "updateConnectionState",
    "updateUnreadChatCount",
    "updateUnreadMessageCount",
    "updateHavePendingNotifications",
    "updateScopeNotificationSettings",
    "updateReactionNotificationSettings",
    "updateAnimationSearchParameters",
    "updateAttachmentMenuBots",
    "updateSuggestedActions",
    "updateDiceEmojis",
    "updateActiveEmojiReactions",
    "updateAvailableMessageEffects",
    "updateDefaultReactionType",
    "updateDefaultPaidReactionType",
    "updateChatThemes",
    "updateAccentColors",
    "updateProfileAccentColors",
    "updateStoryStealthMode",
    "updateSpeechRecognitionTrial",
    "updateChatFolders",
    "updateFileDownloads",
    "updateOwnedStarCount",
    "updateTermsOfService",
    "updateWebBrowserSettings",
    "updateTextCompositionStyles",
    "updateGroupCallMessageLevels",
];

/// Object refreshes TDLib sends when ITS OWN background fetches return (getFullChannel,
/// getFullUser, ...), so the phase they land in depends on timing: compared as one set per HELPER
/// RUN (H-M3: a phase of their own per run at the end, runs split at the `restart*`/`kill*`
/// steps), not per phase and not over the whole file, so a refresh that stops after a restart is
/// still seen. Seen 2026-10-27 (C8): `updateSupergroup` landed in RC9's `read-inbox-c401` phase in
/// one run and before it in the committed one; loosened per his rule, not re-run to green.
const DRIFT_ANY_PHASE: &[&str] = &[
    "updateSupergroup",
    "updateSupergroupFullInfo",
    "updateBasicGroupFullInfo",
    "updateUser",
    "updateUserFullInfo",
];

/// The label of the phase that holds [`DRIFT_ANY_PHASE`] types (followed by ` run <n>`).
const ANY_PHASE: &str = "(any phase)";

/// The recordings left out of the drift comparison, with the reason: RC3 at once has BOTH outcomes
/// of P-4 at the pin (the re-delivery through the difference or none), so its types differ run to
/// run by design.
const DRIFT_SKIPPED: &[&str] = &["kill-at-once", "incoming-call-pre0002"];

/// Whether a step label starts a new helper run (the chat order rule and the drift's runs).
///
/// A run-starting step is stamped AFTER the old run's driver stopped (`Drv::stop`), a `kill*` step
/// as the `restart*` steps, so every frame of the old run is on the tape before it and none
/// counts as the new run's first object (`chat_before_reference` clears its chats at this step: a
/// killed helper's last frame, put on the tape one line after a step stamped right after `kill`,
/// was taken for the new run's first object, a flap under load).
///
/// GUARANTEED for the `restart*` and the `kill*` steps alike: `Drv::stop` returns only after the
/// recording relay of the helper's frames ended. The relay (`tape_lines`) puts a line on the tape
/// BEFORE it passes it on and signals its end after the last line, and `Drv::stop` waits for that
/// signal (a hang guard of 30 s, `RELAY_END_BOUND`; on expiry it panics). It does not rely on
/// `Helper::stop`, which reads on for the link's end for at most one second and does not report
/// an expiry.
fn starts_a_run(label: &str) -> bool {
    label.starts_with("restart") || label.starts_with("kill")
}

/// A recording's phases (split at the case's `kourierStep` lines, the label first; `start` before
/// the first) with the SET of `@type` names TDLib emitted in each, minus [`DRIFT_IGNORED`], and
/// last, per helper run, the [`ANY_PHASE`] set of the [`DRIFT_ANY_PHASE`] types. A set, not the
/// multiset of the brief: how many `updateChatPosition`/`updateChatLastMessage` TDLib sends for one
/// change depends on its batching (seen differ between runs of the same case). In a phase after a
/// KILL the killed message's own `updateNewMessage` (the P-4 re-delivery, which a quiet 1 s may or
/// may not have saved the pts of) is left out (H-M4).
pub(super) fn drift_phases(lines: &[String]) -> Vec<(String, std::collections::BTreeSet<String>)> {
    let mut out = vec![("start".to_owned(), std::collections::BTreeSet::new())];
    let mut any = vec![std::collections::BTreeSet::new()];
    let mut after_kill = false;
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let kind = v["@type"].as_str().unwrap_or_default();
        if kind == "kourierStep" {
            let label = v["step"].as_str().unwrap_or_default();
            if starts_a_run(label) {
                any.push(Default::default());
            }
            after_kill = label.starts_with("kill");
            out.push((label.to_owned(), Default::default()));
        } else if DRIFT_ANY_PHASE.contains(&kind) {
            any.last_mut().expect("a run").insert(kind.to_owned());
        } else if after_kill && kind == "updateNewMessage" {
            // the re-delivery of the killed message (or none): either is the golden fact
        } else if kind != "kourierRequest" && !DRIFT_IGNORED.contains(&kind) {
            out.last_mut().expect("a phase").1.insert(kind.to_owned());
        }
    }
    for (i, set) in any.into_iter().enumerate() {
        out.push((format!("{ANY_PHASE} run {}", i + 1), set));
    }
    out
}

/// What differs between two recordings' phases: labels and type NAMES only (never a value). In the
/// [`ANY_PHASE`] sets only a type the run lost counts.
pub(super) fn drift_between(now: &[String], committed: &[String]) -> Vec<String> {
    let (a, b) = (drift_phases(now), drift_phases(committed));
    let mut out = Vec::new();
    let la: Vec<&str> = a.iter().map(|p| p.0.as_str()).collect();
    let lb: Vec<&str> = b.iter().map(|p| p.0.as_str()).collect();
    if la != lb {
        out.push(format!("phases {la:?} != committed {lb:?}"));
        return out;
    }
    for ((label, x), (_, y)) in a.iter().zip(&b) {
        let missing: Vec<&String> = y.difference(x).collect();
        // A refresh of TDLib's own background fetch that a run has and the recording lacks is not
        // drift (F14's held-frames experiment: an optional updateBasicGroupFullInfo came in run 1);
        // a refresh type a run LOST is (H-M3: what TG-3's H1 needs after every helper start).
        let extra: Vec<&String> = if label.starts_with(ANY_PHASE) {
            Vec::new()
        } else {
            x.difference(y).collect()
        };
        if !missing.is_empty() || !extra.is_empty() {
            out.push(format!("{label}: missing {missing:?} new {extra:?}"));
        }
    }
    out
}

/// The committed (scrubbed) recording of `name`, if `testdata/` holds it.
fn committed_recording(name: &str) -> Option<Vec<String>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(format!("rec-{name}.jsonl"));
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.lines().map(str::to_owned).collect())
}

/// The drift check of brief §3.10, run by every recording case on the stream it just saw: the
/// phases and their type sets equal the committed recording's (LIBRARY drift: a pin bump must
/// re-record, `cargo xtask tdlib fakedc record`).
fn drift_check(name: &str, tape: &TapeRef) {
    if DRIFT_SKIPPED.contains(&name) {
        println!("FAKEDC {name}: drift not compared (two outcomes by design)");
        return;
    }
    let Some(committed) = committed_recording(name) else {
        println!("FAKEDC {name}: no committed recording, drift not compared");
        return;
    };
    let now: Vec<String> = tape
        .lock()
        .expect("tape")
        .lines
        .iter()
        .map(|(_, l)| l.clone())
        .collect();
    let diff = drift_between(&now, &committed);
    assert!(
        diff.is_empty(),
        "{name}: the recording drifted from testdata/rec-{name}.jsonl: {diff:?}"
    );
    println!("FAKEDC {name}: drift none");
}

/// Writes the raw recording and the fake DC's files beside it, under [`RECORD_ENV`] only.
fn write_recording(name: &str, tape: &TapeRef, fake: &FakeDc) {
    let Some(dir) = var(RECORD_ENV) else {
        return;
    };
    std::fs::create_dir_all(&dir).expect("the record directory");
    let mut out = String::new();
    for (_, l) in &tape.lock().expect("tape").lines {
        out.push_str(l);
        out.push('\n');
    }
    std::fs::write(dir.join(format!("{name}.raw.jsonl")), out).expect("write the recording");
    std::fs::copy(&fake.world_json, dir.join(format!("{name}.world.json"))).expect("world.json");
    std::fs::copy(&fake.wire_log, dir.join(format!("{name}.wire.jsonl"))).expect("wire log");
    println!("FAKEDC {name}: recorded");
}

/// The LAST step of every recording case (after its golden asserts, so a failed golden fact is
/// reported as itself and not as the drift it causes): without [`RECORD_ENV`], compares the stream
/// with the committed recording ([`drift_check`]).
fn drift_end(name: &str, tape: &TapeRef) {
    if var(RECORD_ENV).is_none() {
        drift_check(name, tape);
    }
}

/// The network end of EVERY recording case, the pre-0002 run's too (S-M1): the shared
/// [`net_end`] with the gate's accepted connections of the helpers this case started (their stop
/// reports) and the remap's dials.
fn net_end_of(traps: &Traps, c: &Case, name: &str) -> Value {
    let gate: u64 = c
        .reports
        .lock()
        .expect("reports")
        .iter()
        .map(|r| r.gate.map_or(0, |g| g.accepted))
        .sum();
    net_end(
        name,
        traps,
        &c.fake,
        c.dials.load(Ordering::SeqCst),
        Some(gate),
    )
}

/// The common end of every regular case: the network end, no `phone.*` request (per branch: the
/// pre-0002 run sees them by design), and RC1's ORDER fact (H-L3: a chat's `updateNewChat` before
/// every object naming it, per helper run).
fn common_end(traps: &Traps, c: &Case, name: &str) {
    let stats = net_end_of(traps, c, name);
    println!("FAKEDC {name}: phone_requests={}", stats["phone_requests"]);
    assert_eq!(
        stats["phone_requests"].as_u64(),
        Some(0),
        "a phone.* request reached the fake DC"
    );
    assert_eq!(
        chat_before_reference(&values(&c.tape)),
        None,
        "{name}: an object named a chat before its updateNewChat"
    );
}

/// The world's chat ids as TDLib's.
const U201: i64 = 201;
const U202: i64 = 202;
const G301: i64 = -301;

/// One case's set-up: the fake DC with the standard world, the parts, the account, a runtime.
struct Case {
    dir: tempfile::TempDir,
    fake: FakeDc,
    tape: TapeRef,
    parts: TgParts,
    pids: Pids,
    account: Account,
    rt: tokio::runtime::Runtime,
    /// The remap's dials (the equation of [`net_end`]).
    dials: Arc<AtomicUsize>,
    reports: Reports,
}

fn case_setup() -> Case {
    case_setup_with(false)
}

/// [`case_setup`] over the regular test-only library, or (`pre0002`) the pre-0002 one.
fn case_setup_with(pre0002: bool) -> Case {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = start_fakedc(dir.path());
    fake.control("world standard");
    let tape: TapeRef = Arc::default();
    let dials = Arc::new(AtomicUsize::new(0));
    let (parts, pids) = rec_parts(
        Arc::new(RemapDialer {
            fakedc: fake.at,
            dials: dials.clone(),
        }),
        tape.clone(),
        pre0002,
    );
    Case {
        dir,
        fake,
        tape,
        parts,
        pids,
        account: account(),
        rt: rt(),
        dials,
        reports: Arc::default(),
    }
}

impl Case {
    fn record(&self, on: bool) {
        self.tape.lock().expect("tape").on = on;
    }

    async fn start(&self) -> Drv {
        Drv::start(
            &self.parts,
            &self.account,
            self.rt.handle(),
            self.reports.clone(),
            &self.tape,
        )
        .await
    }

    /// A start on the SAME directories: TDLib comes back Ready from its saved state.
    async fn restart(&self) -> Drv {
        let d = self.start().await;
        assert_eq!(d.first, Some(AuthKind::Ready), "the restart is not Ready");
        d
    }

    /// RC1's load: both lists until 404, then each unread chat's history twice (from 0, then from
    /// the oldest id returned). (main pages, main 404, archive pages, archive 404, first and
    /// second history lengths per unread chat).
    async fn load(&self, d: &mut Drv) -> Load {
        let main = d.load_all("chatListMain").await;
        let archive = d.load_all("chatListArchive").await;
        let mut histories = Vec::new();
        let mut server = Vec::new();
        let history_calls = || wire_count(&self.fake.names(), 0, "messages.getHistory");
        for chat in unread_chats(&values(&self.tape)) {
            let before = history_calls();
            let first = d.history(chat, 0).await;
            let between = history_calls();
            let second = match first.last() {
                Some(oldest) => d.history(chat, *oldest).await,
                None => Vec::new(),
            };
            let after = history_calls();
            histories.push((first.len(), second.len()));
            // (the first call's wire getHistory, the second call's): H-M2, the wire log counts
            // around each call (TDLib's own background fetches can only add to them).
            server.push((between - before, after - between));
        }
        Load {
            main,
            archive,
            histories,
            server,
        }
    }
}

#[derive(Debug)]
struct Load {
    main: (usize, bool),
    archive: (usize, bool),
    histories: Vec<(usize, usize)>,
    /// Per unread chat: the wire's `messages.getHistory` during the first and the second call.
    server: Vec<(usize, usize)>,
}

fn kill(pid: Option<u32>) -> bool {
    pid.is_some_and(kill_pid)
}

// ----- the cases -----

/// RC1 `fresh-start`: the standard world, the login, then TG-3's load.
fn case_fresh_start(traps: &Traps) {
    let c = case_setup();
    let started = StdInstant::now();
    let load = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        let load = c.load(&mut d).await;
        d.idle(1.0).await;
        d.stop().await;
        load
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let order = chat_before_reference(&vs);
    let new_chats = vs
        .iter()
        .filter(|(_, v)| v["@type"] == "updateNewChat")
        .count();
    println!(
        "FAKEDC fresh-start: lines={} new_chats={new_chats} load={load:?} order_breach={order:?} get_dialogs={} pinned={} get_history={} full_channel={} secs={:.1}",
        vs.len(),
        wire_count(&names, 0, "messages.getDialogs"),
        wire_count(&names, 0, "messages.getPinnedDialogs"),
        wire_count(&names, 0, "messages.getHistory"),
        wire_count(&names, 0, "channels.getFullChannel"),
        started.elapsed().as_secs_f32()
    );
    write_recording("fresh-start", &c.tape, &c.fake);
    common_end(traps, &c, "fresh-start");
    assert!(load.main.1 && load.archive.1, "a list did not end in 404");
    assert!(load.main.0 >= 1, "the main list loaded no page");
    assert_eq!(
        order, None,
        "an object named a chat before its updateNewChat"
    );
    assert!(!load.histories.is_empty(), "no unread chat was reported");
    // H-M2: TDLib's first page (from 0) holds at least what it keeps itself, and one of the two
    // calls goes to the server and the server's answer reaches the app. WHICH call reaches the
    // server is TDLib's timing (a first call can fetch and the second be served from memory: seen
    // once in 14 runs of the re-check), so it is asserted per chat over both calls.
    assert!(
        load.histories.iter().all(|h| h.0 >= 1),
        "a first getChatHistory(from 0) page was empty: {:?}",
        load.histories
    );
    assert!(
        load.server.iter().all(|s| s.0 + s.1 >= 1),
        "neither getChatHistory of a chat reached the server: {:?}",
        load.server
    );
    assert!(
        load.histories.iter().any(|h| h.1 >= 1),
        "no server page reached the app: {:?}",
        load.histories
    );
    assert!(
        wire_count(&names, 0, "messages.getHistory") >= 1,
        "no history request reached the fake DC"
    );
    drift_end("fresh-start", &c.tape);
    case_dir("fresh-start", c.dir);
}

/// RC2 `restart-saved-pts`: RC1's load, a clean stop, changes queued while it is down (three new
/// messages, then an edit of an old one), a start on the same directories, RC1's load again, then
/// one live push (the continuity step).
fn case_restart_saved_pts(traps: &Traps) {
    let c = case_setup();
    let (load, edit_seen, wire_from, diff_from) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        c.load(&mut d).await;
        d.stop().await;
        step_line(&c.tape, &c.fake, "restart-queued");
        let wire_from = c.fake.names().len();
        c.fake.control("queue new u201");
        c.fake.control("queue new u202");
        c.fake.control("queue new g301");
        // The edit is the LAST queued change on purpose: a difference whose final state stopped
        // at the last new message would leave TDLib one pts behind, and the continuity push below
        // would then be a gap (H-L2: the proof that the edit's pts was applied).
        c.fake.control("queue edit u201 1");
        let mut d = c.restart().await;
        let load = c.load(&mut d).await;
        d.idle(2.0).await;
        let vs = values(&c.tape);
        let at = step_at(&vs, "restart-queued");
        let edits = vs
            .iter()
            .skip(at)
            .filter(|(_, v)| {
                matches!(
                    v["@type"].as_str(),
                    Some("updateMessageEdited" | "updateMessageContent")
                )
            })
            .count();
        // H-L2: the edit of a message TDLib no longer holds reached the app as nothing; its pts
        // WAS applied if a later push arrives at once, once, with no difference after it.
        step_line(&c.tape, &c.fake, "continuity");
        let diff_from = c.fake.names().len();
        c.fake.control("push new u201");
        d.wait_for(20, |v| {
            v["@type"] == "updateNewMessage" && v["message"]["chat_id"] == U201
        })
        .await;
        d.idle(1.0).await;
        d.stop().await;
        (load, edits, wire_from, diff_from)
    });
    let vs = values(&c.tape);
    let at = step_at(&vs, "restart-queued");
    let cont_at = step_at(&vs, "continuity");
    let names = c.fake.names();
    let news: Vec<usize> = [U201, U202, G301]
        .iter()
        .map(|chat| {
            new_messages(&vs, at, *chat)
                .iter()
                .filter(|m| m.0 < cont_at)
                .count()
        })
        .collect();
    let continuity = (
        new_messages(&vs, cont_at, U201).len(),
        wire_count(&names, diff_from, "updates.getDifference"),
    );
    let order = chat_before_reference(&vs);
    println!(
        "FAKEDC restart-saved-pts: lines={} new_after_restart={news:?} edit_updates={edit_seen} continuity(new u201, getDifference)={continuity:?} load={load:?} order_breach={order:?} get_difference={} get_state={}",
        vs.len(),
        wire_count(
            &names[..diff_from.min(names.len())],
            wire_from,
            "updates.getDifference"
        ),
        wire_count(
            &names[..diff_from.min(names.len())],
            wire_from,
            "updates.getState"
        ),
    );
    write_recording("restart-saved-pts", &c.tape, &c.fake);
    common_end(traps, &c, "restart-saved-pts");
    assert!(
        wire_count(
            &names[..diff_from.min(names.len())],
            wire_from,
            "updates.getDifference"
        ) >= 1,
        "the restart asked no difference"
    );
    assert_eq!(
        news,
        [1, 1, 1],
        "the queued messages did not arrive once each"
    );
    assert_eq!(
        edit_seen, 0,
        "the edit of a message TDLib no longer holds reached the app"
    );
    assert_eq!(
        continuity,
        (1, 0),
        "after the restart's difference a new u201 message arrives once with no difference \
         (new u201, getDifference)"
    );
    assert_eq!(
        order, None,
        "an object named a chat before its updateNewChat"
    );
    drift_end("restart-saved-pts", &c.tape);
    case_dir("restart-saved-pts", c.dir);
}

/// RC3 `kill-after-processing` (P-4: two variants): one pushed private message; SIGKILL the helper
/// at once after its `updateNewMessage` passed the relay, or after 1 s of quiet; a start on the same
/// directories, then `loadChats(main)` and that chat's history. Golden: the message reaches the app
/// at most once more after the restart, only through the difference, never twice in one run.
fn case_kill_after_processing(traps: &Traps, quiet: bool) {
    let name = if quiet {
        "kill-after-quiet"
    } else {
        "kill-at-once"
    };
    let c = case_setup();
    let (killed, wire_from, history) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        step_line(&c.tape, &c.fake, "push-new");
        c.fake.control("push new u201");
        let seen = d
            .wait_for(20, |v| {
                v["@type"] == "updateNewMessage" && v["message"]["chat_id"] == U201
            })
            .await;
        assert!(seen, "the pushed message did not arrive");
        if quiet {
            d.idle(1.0).await;
        }
        let killed = kill(c.pids.lock().expect("pids").last().copied());
        d.stop().await;
        // Stamped AFTER the killed driver stopped ([`starts_a_run`]): the dead helper's last
        // frames (the burst that carried the message) are on the tape before the step.
        step_line(&c.tape, &c.fake, "kill");
        let wire_from = c.fake.names().len();
        let mut d = c.restart().await;
        d.load_all("chatListMain").await;
        let history = d.history(U201, 0).await;
        d.idle(2.0).await;
        d.stop().await;
        (killed, wire_from, history)
    });
    let vs = values(&c.tape);
    let (push_at, kill_at) = (step_at(&vs, "push-new"), step_at(&vs, "kill"));
    let before: Vec<_> = new_messages(&vs, push_at, U201)
        .into_iter()
        .filter(|(i, _, _)| *i < kill_at)
        .collect();
    let after = new_messages(&vs, kill_at, U201);
    let id = before.first().map(|b| b.1);
    let again = after.iter().filter(|a| Some(a.1) == id).count();
    let names = c.fake.names();
    let differences = wire_count(&names, wire_from, "updates.getDifference");
    let in_history = id.is_some_and(|id| history.contains(&id));
    // H-L4: "only through the difference" is a fact of the wire: the fake pushed nothing after the
    // kill (a `differences >= 1` alone holds at every start).
    let pushes_after = c
        .fake
        .lines()
        .iter()
        .skip(wire_from)
        .filter(|l| l.0 == "push")
        .count();
    // The tape heard the restarted helper (a deaf tape would pass "at most once more"): TDLib,
    // its message DB off, tells the chat again before its history.
    let chat_again = vs[kill_at..]
        .iter()
        .any(|(_, v)| v["@type"] == "updateNewChat" && v["chat"]["id"] == U201);
    println!(
        "FAKEDC {name}: killed={killed} before_kill={} again_after_restart={again} differences_after={differences} pushes_after_kill={pushes_after} in_history={in_history} chat_told_again={chat_again} lines={}",
        before.len(),
        vs.len()
    );
    write_recording(name, &c.tape, &c.fake);
    common_end(traps, &c, name);
    assert!(killed, "the helper was not killed");
    assert_eq!(
        before.len(),
        1,
        "not exactly one updateNewMessage before the kill"
    );
    assert!(
        chat_again,
        "the tape heard no updateNewChat of the chat after the restart"
    );
    assert!(again <= 1, "the message came twice after the restart");
    assert_eq!(pushes_after, 0, "the fake pushed after the kill");
    if again == 1 {
        assert!(differences >= 1, "a re-delivery without a difference");
    }
    assert!(
        in_history,
        "the message is not in the chat's history after the restart"
    );
    drift_end(name, &c.tape);
    case_dir(name, c.dir);
}

/// RC4 `reconnect`: while Ready, two messages QUEUED, then every client socket dropped; TDLib
/// reconnects through the gate (the same auth key) and the messages arrive once each.
fn case_reconnect(traps: &Traps) {
    let c = case_setup();
    let (arrived, wire_from, took, session_facts) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        d.idle(1.0).await;
        c.record(true);
        let wire_from = c.fake.names().len();
        let before = c.fake.control("stats");
        c.fake.control("queue new u201");
        c.fake.control("queue new u201");
        step_line(&c.tape, &c.fake, "drop");
        let t0 = Instant::now();
        c.fake.control("drop");
        let mut arrived = 0;
        let by = Instant::now() + Duration::from_secs(40);
        while arrived < 2 && Instant::now() < by {
            d.idle(0.5).await;
            arrived = new_messages(&values(&c.tape), 0, U201).len();
        }
        let took = t0.elapsed();
        d.idle(1.0).await;
        let after = c.fake.control("stats");
        d.stop().await;
        // What the fake DC's session book says of the drop (H-M1): sessions it knows, how many it
        // announced, how many connections came back on a kept session, the held changes it resent.
        let d_of = |k: &str| after[k].as_u64().unwrap_or(0) - before[k].as_u64().unwrap_or(0);
        let facts = format!(
            "sessions={} announced_after_drop={} kept_after_drop={} resent={} accepted_after_drop={}",
            after["sessions"],
            d_of("announced"),
            d_of("kept"),
            d_of("resent"),
            d_of("accepted"),
        );
        (arrived, wire_from, took, facts)
    });
    let vs = values(&c.tape);
    let msgs = new_messages(&vs, 0, U201);
    let mut ids: Vec<i64> = msgs.iter().map(|m| m.1).collect();
    ids.dedup();
    let states: Vec<String> = vs
        .iter()
        .filter(|(_, v)| v["@type"] == "updateConnectionState")
        .filter_map(|(_, v)| v["state"]["@type"].as_str().map(str::to_owned))
        .collect();
    let names = c.fake.names();
    let after: Vec<&str> = names[wire_from.min(names.len())..]
        .iter()
        .map(String::as_str)
        .collect();
    println!("FAKEDC reconnect: wire after the drop {after:?}");
    println!("FAKEDC reconnect: {session_facts}");
    println!(
        "FAKEDC reconnect: arrived={arrived} secs={:.1} states={states:?} get_difference={} handshakes={} lines={}",
        took.as_secs_f32(),
        wire_count(&names, wire_from, "updates.getDifference"),
        wire_count(&names, wire_from, "req_pq_multi"),
        vs.len()
    );
    write_recording("reconnect", &c.tape, &c.fake);
    common_end(traps, &c, "reconnect");
    assert_eq!(
        msgs.len(),
        2,
        "the queued messages did not arrive exactly once each"
    );
    assert_eq!(ids.len(), 2, "a queued message came twice");
    // MEASURED, not golden (P-5): whether TDLib shows a connection state at all after the drop
    // (the brief's [I] "Connecting -> Updating -> Ready" did not show in build 3's runs) and how
    // many handshakes the reconnect made (temporary keys) depend on the fake DC and on timing.
    if let Some(last) = states.last() {
        assert_eq!(
            last, "connectionStateReady",
            "not Ready after the reconnect"
        );
    }
    drift_end("reconnect", &c.tape);
    case_dir("reconnect", c.dir);
}

/// What one gap case saw after its push step (RC5 a-d, H-H1): the new messages of `chat` in the
/// push phase, whether they came in id order and once each, the time until the last of the three,
/// the getDifference / getChannelDifference asked after the push, and the objects that tell a chat
/// or a user of `known`'s kind in that phase.
struct Gap {
    ids: Vec<i64>,
    in_order: bool,
    waited: Duration,
    differences: usize,
    channel_differences: usize,
    new_chat: bool,
    new_user: bool,
}

/// Runs a gap case: login, `before` (what TDLib is told first), then the control `push` command
/// and a bounded wait (20 s) for three new messages of `chat`, then 1 s of quiet. `fill`: the
/// `push` is the control command (`gap`, or `gap-fill`: the missing two pushed right after).
fn run_gap(c: &Case, push: &str, chat: i64, user: i64, before: Option<&str>) -> Gap {
    let (waited, wire_from) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        if let Some(list) = before {
            d.load_all(list).await;
        }
        d.idle(1.0).await;
        let wire_from = c.fake.names().len();
        step_line(&c.tape, &c.fake, "push-gap");
        let t0 = Instant::now();
        c.fake.control(push);
        let by = Instant::now() + Duration::from_secs(20);
        let at = step_at(&values(&c.tape), "push-gap");
        while new_messages(&values(&c.tape), at, chat).len() < 3 && Instant::now() < by {
            d.idle(0.1).await;
        }
        let waited = t0.elapsed();
        d.idle(1.0).await;
        d.stop().await;
        (waited, wire_from)
    });
    let vs = values(&c.tape);
    let at = step_at(&vs, "push-gap");
    let ids: Vec<i64> = new_messages(&vs, at, chat).iter().map(|m| m.1).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    let names = c.fake.names();
    Gap {
        in_order: sorted == ids,
        ids,
        waited,
        differences: wire_count(&names, wire_from, "updates.getDifference"),
        channel_differences: wire_count(&names, wire_from, "updates.getChannelDifference"),
        new_chat: vs[at..]
            .iter()
            .any(|(_, v)| v["@type"] == "updateNewChat" && v["chat"]["id"] == chat),
        new_user: vs[at..]
            .iter()
            .any(|(_, v)| v["@type"] == "updateUser" && v["user"]["id"] == user),
    }
}

fn print_gap(name: &str, g: &Gap, lines: usize) {
    println!(
        "FAKEDC {name}: messages={} in_order={} waited_s={:.2} get_difference={} get_channel_difference={} new_chat_in_push_phase={} new_user_in_push_phase={} lines={lines}",
        g.ids.len(),
        g.in_order,
        g.waited.as_secs_f32(),
        g.differences,
        g.channel_differences,
        g.new_chat,
        g.new_user,
    );
}

/// RC5a `gap-known-sender` (H-H1, the pending-pts path): `chatListMain` loaded first, so TDLib
/// knows user 201 and his chat; then a private message pushed with two pts skipped. TDLib holds it
/// for its gap wait (0.7 s at the pin, `MAX_UNFILLED_GAP_TIME`; the wait is printed, NOT asserted),
/// then asks ONE difference; the three messages arrive once each in pts order and the push phase
/// tells no chat and no user of 201 (so the unknown-sender path did not run).
fn case_gap_known_sender(traps: &Traps) {
    let c = case_setup();
    let g = run_gap(&c, "push gap u201", U201, 201, Some("chatListMain"));
    let lines = values(&c.tape).len();
    print_gap("gap-known-sender", &g, lines);
    write_recording("gap-known-sender", &c.tape, &c.fake);
    common_end(traps, &c, "gap-known-sender");
    assert_eq!(
        g.differences, 1,
        "a gap of a known sender asks exactly one difference"
    );
    assert_eq!(g.ids.len(), 3, "not three messages");
    assert!(g.in_order, "not in pts order, or one came twice");
    assert!(
        !g.new_chat && !g.new_user,
        "the push phase told the chat or the user of 201: the unknown-sender path ran, not the gap"
    );
    drift_end("gap-known-sender", &c.tape);
    case_dir("gap-known-sender", c.dir);
}

/// RC5b `gap-filled-by-push` (H-H1, the buffer): the gapped message pushed, then the two missing
/// ones right after. Order and once are asserted; whether TDLib asked a difference anyway is timing
/// (printed, NOT asserted).
fn case_gap_filled_by_push(traps: &Traps) {
    let c = case_setup();
    let g = run_gap(&c, "push gap-fill u201", U201, 201, Some("chatListMain"));
    let lines = values(&c.tape).len();
    print_gap("gap-filled-by-push", &g, lines);
    write_recording("gap-filled-by-push", &c.tape, &c.fake);
    common_end(traps, &c, "gap-filled-by-push");
    assert_eq!(g.ids.len(), 3, "not three messages");
    assert!(g.in_order, "not in pts order, or one came twice");
    assert!(
        !g.new_chat && !g.new_user,
        "the push phase told the chat or the user of 201"
    );
    drift_end("gap-filled-by-push", &c.tape);
    case_dir("gap-filled-by-push", c.dir);
}

/// RC5c `gap-channel` (H-H1): a loaded supergroup gets a post with the channel's pts skipped by
/// two: the three posts arrive once each in order; TDLib's `getChannelDifference` count is printed
/// and must be at least one (the fake never fills a channel gap).
fn case_gap_channel(traps: &Traps) {
    let c = case_setup();
    let g = run_gap(&c, "push gap c401", C401, 203, Some("chatListMain"));
    let lines = values(&c.tape).len();
    print_gap("gap-channel", &g, lines);
    write_recording("gap-channel", &c.tape, &c.fake);
    common_end(traps, &c, "gap-channel");
    assert_eq!(g.ids.len(), 3, "not three channel messages");
    assert!(g.in_order, "not in order, or one came twice");
    assert!(
        g.channel_differences >= 1,
        "a channel gap that nobody fills asked no channel difference"
    );
    assert!(!g.new_chat, "the push phase told the supergroup's chat");
    drift_end("gap-channel", &c.tape);
    case_dir("gap-channel", c.dir);
}

/// RC5d `gap-unknown-sender` (H-H1, the first RC5 kept as its own golden fact): NO list loaded, so
/// TDLib knows no peer when the gapped short message arrives: an unacceptable update, and TDLib asks
/// the difference AT ONCE (`UpdatesManager.cpp` `on_pending_updates`); the user and the chat are
/// learnt from the difference. The three messages arrive once each in order.
fn case_gap_unknown_sender(traps: &Traps) {
    let c = case_setup();
    let g = run_gap(&c, "push gap u201", U201, 201, None);
    let lines = values(&c.tape).len();
    print_gap("gap-unknown-sender", &g, lines);
    write_recording("gap-unknown-sender", &c.tape, &c.fake);
    common_end(traps, &c, "gap-unknown-sender");
    assert_eq!(g.ids.len(), 3, "not three messages");
    assert!(g.in_order, "not in pts order, or one came twice");
    assert!(g.differences >= 1, "the unknown sender asked no difference");
    assert!(
        g.new_chat,
        "the chat of an unknown sender was not told from the difference"
    );
    drift_end("gap-unknown-sender", &c.tape);
    case_dir("gap-unknown-sender", c.dir);
}

// ----- C6: RC6-RC11 (brief §3.7, §4.5, critic L-h) -----

/// TDLib's chat ids of the world's channels (`-100…` + the channel id).
const C401: i64 = -1_000_000_000_401;
const C402: i64 = -1_000_000_000_402;
const C404: i64 = -1_000_000_000_404;

/// TDLib's id of the server message `id` (the server id shifted by 20 bits).
fn td_msg(id: i64) -> i64 {
    id << 20
}

/// The `@type`s of the tape's objects after `from` that name message `id` of `chat`
/// (`message_id`, `message.id` or a `message_ids` entry, with `chat_id` or `message.chat_id`).
fn naming(vs: &[(Instant, Value)], from: usize, chat: i64, id: i64) -> Vec<String> {
    vs.iter()
        .skip(from)
        .filter(|(_, v)| {
            let chat_ok = v["chat_id"] == chat || v["message"]["chat_id"] == chat;
            let id_ok = v["message_id"] == id
                || v["message"]["id"] == id
                || v["message_ids"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| *x == id));
            chat_ok && id_ok
        })
        .filter_map(|(_, v)| v["@type"].as_str().map(str::to_owned))
        .collect()
}

/// The first tape object after `from` of type `kind` for `chat` (by `chat_id`).
fn first_of<'v>(
    vs: &'v [(Instant, Value)],
    from: usize,
    kind: &str,
    chat: i64,
) -> Option<&'v Value> {
    vs.iter()
        .skip(from)
        .map(|(_, v)| v)
        .find(|v| v["@type"] == kind && v["chat_id"] == chat)
}

/// Every wire name after `from` that starts with `prefix`.
fn wire_prefixed(names: &[String], from: usize, prefix: &str) -> usize {
    names
        .iter()
        .skip(from)
        .filter(|n| n.starts_with(prefix))
        .count()
}

/// RC6 `channel-difference`: after RC1's load, five channel messages QUEUED and
/// `updateChannelTooLong` pushed for the supergroup (TDLib asks the channel difference and gets
/// them); then the public supergroup with a too-long threshold of 2 (MODEL, P-5): the channel
/// difference answers `channelDifferenceTooLong` with the dialog and the latest messages.
fn case_channel_difference(traps: &Traps) {
    let c = case_setup();
    let (wire_a, wire_b, history_b) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        c.load(&mut d).await;
        d.load_all("chatListMain").await;
        let wire_a = c.fake.names().len();
        for _ in 0..5 {
            c.fake.control("queue new c401");
        }
        step_line(&c.tape, &c.fake, "too-long-401");
        c.fake.control("push too-long c401");
        let by = Instant::now() + Duration::from_secs(30);
        while new_messages(&values(&c.tape), 0, C401).len() < 5 && Instant::now() < by {
            d.idle(0.25).await;
        }
        let wire_b = c.fake.names().len();
        c.fake.control("queue threshold c402 2");
        for _ in 0..5 {
            c.fake.control("queue new c402");
        }
        step_line(&c.tape, &c.fake, "too-long-402");
        c.fake.control("push too-long c402");
        // H-M5: a positive fact waits for its event (the supergroup's newest message as its last
        // message), bounded; the idle after it is for the NEGATIVE part (the other four never
        // arrive).
        d.wait_for(30, |v| {
            v["@type"] == "updateChatLastMessage"
                && v["chat_id"] == C402
                && v["last_message"]["id"] == td_msg(6)
        })
        .await;
        d.idle(2.0).await;
        step_line(&c.tape, &c.fake, "history-402");
        let history_b = d.history(C402, 0).await;
        d.idle(1.0).await;
        d.stop().await;
        (wire_a, wire_b, history_b)
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let a_at = step_at(&vs, "too-long-401");
    let b_at = step_at(&vs, "too-long-402");
    let h_at = step_at(&vs, "history-402");
    let a_ids: Vec<i64> = new_messages(&vs, a_at, C401).iter().map(|m| m.1).collect();
    let mut a_sorted = a_ids.clone();
    a_sorted.sort_unstable();
    a_sorted.dedup();
    let b_news = new_messages(&vs, b_at, C402)
        .iter()
        .filter(|m| m.0 < h_at)
        .count();
    let newest_b = td_msg(6);
    let b_last = vs[b_at..h_at.min(vs.len())]
        .iter()
        .filter(|(_, v)| v["@type"] == "updateChatLastMessage" && v["chat_id"] == C402)
        .filter_map(|(_, v)| v["last_message"]["id"].as_i64())
        .collect::<Vec<_>>();
    let b_kinds: std::collections::BTreeSet<String> = vs[b_at..h_at.min(vs.len())]
        .iter()
        .filter(|(_, v)| v["chat_id"] == C402 || v["message"]["chat_id"] == C402)
        .filter_map(|(_, v)| v["@type"].as_str().map(str::to_owned))
        .collect();
    let order = chat_before_reference(&vs);
    println!(
        "FAKEDC channel-difference: a_messages={} a_in_order={} a_channel_diff={} b_new_messages={b_news} b_last_message_ids={:?} b_kinds={b_kinds:?} b_channel_diff={} b_history_has_newest={} b_history_len={} order_breach={order:?} lines={}",
        a_ids.len(),
        a_sorted == a_ids,
        wire_count(
            &names[..wire_b.min(names.len())],
            wire_a,
            "updates.getChannelDifference"
        ),
        b_last.iter().map(|id| id >> 20).collect::<Vec<_>>(),
        wire_count(&names, wire_b, "updates.getChannelDifference"),
        history_b.contains(&newest_b),
        history_b.len(),
        vs.len()
    );
    write_recording("channel-difference", &c.tape, &c.fake);
    common_end(traps, &c, "channel-difference");
    assert_eq!(a_ids.len(), 5, "not the five channel messages");
    assert_eq!(a_sorted, a_ids, "not in order, or one came twice");
    assert!(
        wire_count(
            &names[..wire_b.min(names.len())],
            wire_a,
            "updates.getChannelDifference"
        ) >= 1,
        "updateChannelTooLong asked no channel difference"
    );
    assert!(
        wire_count(&names, wire_b, "updates.getChannelDifference") >= 1,
        "the second too-long asked no channel difference"
    );
    // MEASURED at the pin (build 3 chunk 4; the brief's [I] expected a SILENT reload, no message):
    // after `channelDifferenceTooLong` TDLib tells the dialog's newest message as ONE
    // `updateNewMessage`, then `updateChatLastMessage` with it (and `updateChatReadInbox`); the
    // four messages before it never reach the app. The threshold is MODEL (P-5); this shape is
    // TDLib's.
    assert_eq!(
        b_news, 1,
        "not exactly the newest message after the too-long difference"
    );
    assert_eq!(
        b_last,
        [newest_b],
        "updateChatLastMessage is not the newest message"
    );
    assert!(
        history_b.contains(&newest_b),
        "the supergroup's newest message is not in its history after the too-long difference"
    );
    assert_eq!(
        order, None,
        "an object named a chat before its updateNewChat"
    );
    drift_end("channel-difference", &c.tape);
    case_dir("channel-difference", c.dir);
}

/// RC7 `edit`: the lists loaded only (no history: the older messages stay UNLOADED); the private
/// chat's last message (loaded) and an older one (unloaded) edited, then the same in the
/// supergroup (two more posts queued before the login); `getMessage` after each
/// `updateMessageEdited`.
fn case_edit(traps: &Traps) {
    let c = case_setup();
    c.fake.control("queue new c401");
    c.fake.control("queue new c401");
    let (wire_from, got, diff_from) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        d.load_all("chatListArchive").await;
        let wire_from = c.fake.names().len();
        let mut got = Vec::new();
        for (chat, td, loaded, unloaded) in [("u201", U201, 4, 2), ("c401", C401, 3, 1)] {
            step_line(&c.tape, &c.fake, &format!("edit-loaded-{chat}"));
            c.fake.control(&format!("push edit {chat} {loaded}"));
            let id = td_msg(loaded);
            let seen = d
                .wait_for(20, |v| {
                    v["@type"] == "updateMessageEdited"
                        && v["chat_id"] == td
                        && v["message_id"] == id
                })
                .await;
            let r = d
                .ask(serde_json::json!({"@type": "getMessage", "chat_id": td, "message_id": id}))
                .await
                .unwrap_or(Value::Null);
            got.push((seen, r["edit_date"].as_i64().unwrap_or(0) > 0));
            step_line(&c.tape, &c.fake, &format!("edit-unloaded-{chat}"));
            c.fake.control(&format!("push edit {chat} {unloaded}"));
            d.idle(3.0).await;
        }
        // The unloaded edits' "nothing" is only a fact if TDLib HEARD them: each edit moved
        // the chat's pts, so a following push arrives without a difference only if the
        // edit's push arrived too (a fake DC that stored but never pushed it leaves a gap).
        step_line(&c.tape, &c.fake, "edit-continuity");
        let diff_from = c.fake.names().len();
        c.fake.control("push new u201");
        c.fake.control("push new c401");
        // H-L1: ONE live long-form push (the container with users and chats) in the known chat.
        c.fake.control("push long-new u201");
        d.idle(4.0).await;
        d.stop().await;
        (wire_from, got, diff_from)
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let cont_at = step_at(&vs, "edit-continuity");
    let cont_u201: Vec<i64> = new_messages(&vs, cont_at, U201)
        .iter()
        .map(|m| m.1)
        .collect();
    let mut cont_sorted = cont_u201.clone();
    cont_sorted.sort_unstable();
    cont_sorted.dedup();
    let continuity = (
        cont_u201.len(),
        new_messages(&vs, cont_at, C401).len(),
        wire_count(&names, diff_from, "updates.getDifference"),
        wire_count(&names, diff_from, "updates.getChannelDifference"),
    );
    let mut facts = Vec::new();
    for (chat, td, loaded, unloaded) in [("u201", U201, 4, 2), ("c401", C401, 3, 1)] {
        let l_at = step_at(&vs, &format!("edit-loaded-{chat}"));
        let u_at = step_at(&vs, &format!("edit-unloaded-{chat}"));
        let loaded_kinds: Vec<String> = naming(&vs[..u_at], l_at, td, td_msg(loaded));
        let unloaded_kinds = naming(&vs, u_at, td, td_msg(unloaded));
        facts.push((chat, loaded_kinds, unloaded_kinds));
    }
    println!(
        "FAKEDC edit: facts={facts:?} seen_and_get_message_edited={got:?} get_messages={} channels_get_messages={} lines={} continuity={continuity:?}",
        wire_count(&names, wire_from, "messages.getMessages"),
        wire_count(&names, wire_from, "channels.getMessages"),
        vs.len()
    );
    write_recording("edit", &c.tape, &c.fake);
    common_end(traps, &c, "edit");
    for (chat, loaded_kinds, unloaded_kinds) in &facts {
        let content = loaded_kinds
            .iter()
            .position(|k| k == "updateMessageContent");
        let edited = loaded_kinds.iter().position(|k| k == "updateMessageEdited");
        assert!(
            content.is_some() && edited.is_some() && content < edited,
            "{chat}: loaded edit not updateMessageContent then updateMessageEdited: {loaded_kinds:?}"
        );
        assert!(
            unloaded_kinds.is_empty(),
            "{chat}: the unloaded edit reached the app: {unloaded_kinds:?}"
        );
    }
    assert!(
        got.iter().all(|g| g.0 && g.1),
        "getMessage after an edit: {got:?}"
    );
    assert_eq!(
        continuity,
        (2, 1, 0, 0),
        "after the unloaded edits a new message each (u201 short and long, c401) without a \
         difference (new u201, new c401, getDifference, getChannelDifference)"
    );
    assert_eq!(
        cont_sorted, cont_u201,
        "the short and the long push of u201 did not arrive once each in order"
    );
    drift_end("edit", &c.tape);
    case_dir("edit", c.dir);
}

/// RC8 `delete`: the lists loaded only; an older (unloaded) private message deleted, then the
/// chat's last (loaded) one; the same in the supergroup.
fn case_delete(traps: &Traps) {
    let c = case_setup();
    c.fake.control("queue new c401");
    c.fake.control("queue new c401");
    let wire_from = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        d.load_all("chatListArchive").await;
        let wire_from = c.fake.names().len();
        for (chat, td, loaded, unloaded) in [("u201", U201, 4, 2), ("c401", C401, 3, 1)] {
            step_line(&c.tape, &c.fake, &format!("delete-unloaded-{chat}"));
            c.fake.control(&format!("push delete {chat} {unloaded}"));
            if td == C401 {
                // H-M5: the supergroup's unloaded delete DOES reach the app (a positive fact):
                // wait for it; the idle below is the NEGATIVE part (the private chat's nothing).
                let uid = td_msg(unloaded);
                d.wait_for(30, |v| {
                    v["@type"] == "updateDeleteMessages"
                        && v["chat_id"] == td
                        && v["message_ids"]
                            .as_array()
                            .is_some_and(|a| a.iter().any(|x| *x == uid))
                })
                .await;
            }
            d.idle(3.0).await;
            step_line(&c.tape, &c.fake, &format!("delete-loaded-{chat}"));
            c.fake.control(&format!("push delete {chat} {loaded}"));
            let id = td_msg(loaded);
            d.wait_for(20, |v| {
                v["@type"] == "updateDeleteMessages"
                    && v["chat_id"] == td
                    && v["message_ids"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|x| *x == id))
            })
            .await;
            d.idle(1.0).await;
        }
        d.stop().await;
        wire_from
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let mut facts = Vec::new();
    for (chat, td, loaded, unloaded) in [("u201", U201, 4, 2), ("c401", C401, 3, 1)] {
        let u_at = step_at(&vs, &format!("delete-unloaded-{chat}"));
        let l_at = step_at(&vs, &format!("delete-loaded-{chat}"));
        // The negative fact scans to the end of the tape (H-M5).
        let unloaded_kinds = naming(&vs, u_at, td, td_msg(unloaded));
        let id = td_msg(loaded);
        let del = vs[l_at..].iter().map(|(_, v)| v).find(|v| {
            v["@type"] == "updateDeleteMessages"
                && v["chat_id"] == td
                && v["message_ids"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| *x == id))
        });
        let flags = del.map(|v| (v["is_permanent"] == true, v["from_cache"] == true));
        let uid = td_msg(unloaded);
        let unloaded_flags = vs[u_at..l_at.min(vs.len())]
            .iter()
            .map(|(_, v)| v)
            .find(|v| {
                v["@type"] == "updateDeleteMessages"
                    && v["message_ids"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|x| *x == uid))
            })
            .map(|v| (v["is_permanent"] == true, v["from_cache"] == true));
        facts.push((chat, unloaded_kinds, flags, unloaded_flags));
    }
    println!(
        "FAKEDC delete: facts(chat, unloaded kinds, loaded (is_permanent, from_cache), unloaded (is_permanent, from_cache))={facts:?} wire_after={} lines={}",
        names.len().saturating_sub(wire_from),
        vs.len()
    );
    write_recording("delete", &c.tape, &c.fake);
    common_end(traps, &c, "delete");
    for (chat, unloaded_kinds, flags, _) in &facts {
        assert_eq!(
            *flags,
            Some((true, false)),
            "{chat}: the loaded delete is not updateDeleteMessages{{is_permanent, !from_cache}}"
        );
        // MEASURED at the pin (build 3 chunk 4; the brief/critic M2 expected "nothing" for both):
        // a private chat's unloaded delete reaches the app as nothing, a SUPERGROUP's as
        // `updateDeleteMessages` (TDLib deletes channel messages by id whether it holds them or
        // not). A regression test of what TDLib DOES; TG-3 must park both shapes.
        let expected: &[&str] = if *chat == "c401" {
            &["updateDeleteMessages"]
        } else {
            &[]
        };
        assert_eq!(
            unloaded_kinds, expected,
            "{chat}: the unloaded delete's updates changed"
        );
    }
    drift_end("delete", &c.tape);
    case_dir("delete", c.dir);
}

/// RC9 `read-elsewhere`: after RC1's load, his other device reads the private chat and the
/// supergroup; the peer reads his message.
fn case_read_elsewhere(traps: &Traps) {
    let c = case_setup();
    c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        c.load(&mut d).await;
        // U11 (rc9's drift, two sightings at a load of 15-196): the load's own updates (every
        // chat's `updateChatReadInbox` among them) are kept in `pending` or still on the link when
        // the first step asks for "the first `updateChatReadInbox` of u201"; such a stale update
        // satisfied that wait at once, the next step's label went in before the REAL update came
        // and the phase `read-inbox-u201` held no update (a drift, the golden asserts passed). The
        // load is settled first: nothing kept, no update for 0.5 s (at most 10 s).
        d.quiet(0.5, 10.0).await;
        for (label, command, kind, td) in [
            (
                "read-inbox-u201",
                "push read-inbox u201 4",
                "updateChatReadInbox",
                U201,
            ),
            (
                "read-inbox-c401",
                "push read-inbox c401 1",
                "updateChatReadInbox",
                C401,
            ),
            (
                "read-outbox-u201",
                "push read-outbox u201 1",
                "updateChatReadOutbox",
                U201,
            ),
        ] {
            step_line(&c.tape, &c.fake, label);
            c.fake.control(command);
            d.wait_for(20, |v| v["@type"] == kind && v["chat_id"] == td)
                .await;
        }
        d.idle(1.0).await;
        d.stop().await;
    });
    let vs = values(&c.tape);
    let at = |l: &str| step_at(&vs, l);
    let inbox_u = first_of(&vs, at("read-inbox-u201"), "updateChatReadInbox", U201).map(|v| {
        (
            v["last_read_inbox_message_id"].as_i64(),
            v["unread_count"].as_i64(),
        )
    });
    let inbox_c = first_of(&vs, at("read-inbox-c401"), "updateChatReadInbox", C401).map(|v| {
        (
            v["last_read_inbox_message_id"].as_i64(),
            v["unread_count"].as_i64(),
        )
    });
    let outbox = first_of(&vs, at("read-outbox-u201"), "updateChatReadOutbox", U201)
        .map(|v| v["last_read_outbox_message_id"].as_i64());
    println!(
        "FAKEDC read-elsewhere: inbox_u201={inbox_u:?} inbox_c401={inbox_c:?} outbox_u201={outbox:?} lines={}",
        vs.len()
    );
    write_recording("read-elsewhere", &c.tape, &c.fake);
    common_end(traps, &c, "read-elsewhere");
    assert_eq!(
        inbox_u,
        Some((Some(td_msg(4)), Some(0))),
        "the private read"
    );
    assert_eq!(
        inbox_c,
        Some((Some(td_msg(1)), Some(0))),
        "the supergroup read"
    );
    assert_eq!(outbox, Some(Some(td_msg(1))), "the peer's read");
    drift_end("read-elsewhere", &c.tape);
    case_dir("read-elsewhere", c.dir);
}

/// One RC13 phase that sent a read act: what the wire and the fake DC say after it.
struct ReadPhase {
    label: &'static str,
    /// (`messages.readHistory`, `channels.readHistory`) requests on the wire during the phase.
    reads: (usize, usize),
    /// The fake DC's `read_requests` counter right after the phase (held requests included).
    counted: u64,
    /// The chat's inbox read mark in the fake's world right after the phase (MODEL: the fake stores
    /// exactly the id a request names; K1's unit tests pin that).
    inbox: i64,
}

/// The fake DC's counter of the read requests that reached it.
fn read_requests(c: &Case) -> u64 {
    c.fake.control("stats")["read_requests"]
        .as_u64()
        .expect("the stats' read_requests")
}

/// (`messages.readHistory`, `channels.readHistory`) names on the wire after the entry `from`.
fn read_names(names: &[String], from: usize) -> (usize, usize) {
    (
        wire_count(names, from, "messages.readHistory"),
        wire_count(names, from, "channels.readHistory"),
    )
}

/// The read act's `viewMessages` of `ids` in `chat` (TDLib's id; `word` is the fake's name for
/// it), then `settle` seconds for TDLib's timer tick (nothing is waited for: a request that never
/// comes is a fact too).
async fn view_phase(
    c: &Case,
    d: &mut Drv,
    label: &'static str,
    (chat, word): (i64, &str),
    ids: &[i64],
    settle: f32,
) -> ReadPhase {
    step_line(&c.tape, &c.fake, label);
    let from = c.fake.names().len();
    let answer = d.view(chat, ids).await;
    assert!(
        answer.is_ok(),
        "{label}: viewMessages was refused: {:?}",
        answer.err()
    );
    d.idle(settle).await;
    ReadPhase {
        label,
        reads: read_names(&c.fake.names(), from),
        counted: read_requests(c),
        inbox: c.fake.read_state(word).0,
    }
}

/// A new message pushed into `chat` (`word` in the fake's control language), waited for; its
/// TDLib id.
async fn push_new(c: &Case, d: &mut Drv, word: &str, chat: i64) -> i64 {
    c.fake.control(&format!("push new {word}"));
    let seen = d
        .wait_for(20, |v| {
            v["@type"] == "updateNewMessage" && v["message"]["chat_id"] == chat
        })
        .await;
    assert!(seen, "the message pushed into {word} did not arrive");
    new_messages(&values(&c.tape), 0, chat)
        .last()
        .map(|m| m.1)
        .expect("the message on the tape")
}

/// The tape index of the app's `viewMessages` marker for `chat` after `from`, and of its answer
/// (the first later line that carries the marker's `@extra`).
fn view_and_answer(vs: &[(Instant, Value)], from: usize, chat: i64) -> Option<(usize, usize)> {
    let marker = vs.iter().enumerate().skip(from).find_map(|(i, (_, v))| {
        (v["@type"] == "kourierRequest"
            && v["request"] == "viewMessages"
            && v["args"]["chat_id"] == chat)
            .then_some(i)
    })?;
    let extra = &vs[marker].1["@extra"];
    if extra.is_null() {
        return None;
    }
    let answer = vs
        .iter()
        .enumerate()
        .skip(marker + 1)
        .find_map(|(i, (_, v))| {
            (v["@extra"] == *extra && v["@type"] != "kourierRequest").then_some(i)
        })?;
    Some((marker, answer))
}

/// The milliseconds since the epoch now (the fake DC's wire log keeps its own clock in them).
fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// PRINTED only: milliseconds from the relay time of the answer to the `viewMessages` of the step
/// `label` to the wire log's `in` line of the read request that followed (TDLib's timer tick; a
/// negative number: the request was on the wire first). `base` pairs an `Instant` with its epoch ms.
fn tick_ms(
    vs: &[(Instant, Value)],
    wire: &[(u64, String, String)],
    label: &str,
    chat: i64,
    base: (Instant, i64),
) -> Option<i64> {
    let (_, answer) = view_and_answer(vs, step_at(vs, label), chat)?;
    let answered = base.1 + i64::try_from(vs[answer].0.duration_since(base.0).as_millis()).ok()?;
    let mark = wire
        .iter()
        .position(|(_, dir, name)| dir == "mark" && name == label)?;
    let (t, _, _) = wire
        .iter()
        .skip(mark)
        .find(|(_, dir, name)| dir == "in" && name.ends_with(".readHistory"))?;
    Some(i64::try_from(*t).ok()? - answered)
}

/// TDLib's last told inbox read position of `chat` (`updateNewChat` or `updateChatReadInbox`) in
/// the tape range `from..to`.
fn told_inbox(vs: &[(Instant, Value)], from: usize, to: usize, chat: i64) -> Option<i64> {
    vs[from..to]
        .iter()
        .filter_map(|(_, v)| match v["@type"].as_str() {
            Some("updateNewChat") if v["chat"]["id"] == chat => {
                v["chat"]["last_read_inbox_message_id"].as_i64()
            }
            Some("updateChatReadInbox") if v["chat_id"] == chat => {
                v["last_read_inbox_message_id"].as_i64()
            }
            _ => None,
        })
        .next_back()
}

/// RC13 `read-act` (TG-4 W2, Before R1 (1)): what TDLib does with the read act's
/// `viewMessages(source: null, force_read: true)` while its message database is off, and what a
/// killed helper loses. The request is built by the product's own `LaneAsk::View`.
///
/// Run 1 (login, `loadChats(main)`, `getChatHistory` of u201 and c401): `view-private` (u201's
/// three unread ids), `view-supergroup` (c401's one), `view-again` (u201's ids once more); then
/// `view-held`: `hold-reads on` (the fake DC neither applies nor answers a read request: a request
/// lost in flight), one new message in u202 and two in g301 (the second is g301's top), 1 s of
/// quiet so TDLib saves its pts (RC3b), `viewMessages` of u202's message and of g301's FIRST one,
/// and SIGKILL as soon as the last `ok` passed the relay (`kill-after-view`). Run 2: after 1 s
/// the fake is quiet, `hold-reads off` (drops what was held), `read-state`, restart, `loadChats(main)`,
/// 1.5 s of quiet; `reassert`: u202's id again (the dialog's top, which TDLib holds after
/// `getDialogs`); `reassert-below-top`: g301's first id (below the top, which TDLib may not hold:
/// `MessagesManager.cpp:16786-16791`, printed which way it went).
///
/// GOLDEN (TDLib's own code at the pin, asserted on every run): `view-private` -> ONE
/// `messages.readHistory` and the `updateChatReadInbox` BEFORE the call's answer on the relay;
/// `view-supergroup` -> ONE `channels.readHistory`; `view-again` -> no read request; after the
/// restart no read request until the first re-assert (no replay with the DB off); each re-assert
/// -> ONE request; no `readMessageContents`, views or reactions request on the wire.
/// GOLDEN-VIA-MODEL: the `max_id` values (read back through `read-state`: the fake applied the
/// request and stores exactly its id). MODEL: the read position TDLib is told after the restart
/// (below the viewed id: the fake never applied the held read), the answer shapes, no echo push.
fn case_read_act(traps: &Traps) {
    let c = case_setup();
    let base = (Instant::now(), epoch_ms());
    let run = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        d.history(U201, 0).await;
        d.history(C401, 0).await;
        let start = (c.fake.read_state("u201"), c.fake.read_state("c401"));
        let unread = [td_msg(2), td_msg(3), td_msg(4)];
        let mut phases = vec![
            view_phase(&c, &mut d, "view-private", (U201, "u201"), &unread, 1.0).await,
            view_phase(
                &c,
                &mut d,
                "view-supergroup",
                (C401, "c401"),
                &[td_msg(1)],
                1.0,
            )
            .await,
            view_phase(&c, &mut d, "view-again", (U201, "u201"), &unread, 1.0).await,
        ];
        step_line(&c.tape, &c.fake, "view-held");
        c.fake.control("hold-reads on");
        let held_from = c.fake.names().len();
        let u_id = push_new(&c, &mut d, "u202", U202).await;
        let g_first = push_new(&c, &mut d, "g301", G301).await;
        let g_top = push_new(&c, &mut d, "g301", G301).await;
        d.idle(1.0).await;
        for (chat, id) in [(U202, u_id), (G301, g_first)] {
            let r = d.view(chat, &[id]).await;
            assert!(
                r.is_ok(),
                "view-held: viewMessages was refused: {:?}",
                r.err()
            );
        }
        let killed = kill(c.pids.lock().expect("pids").last().copied());
        d.stop().await;
        // Stamped after the killed driver stopped ([`starts_a_run`]).
        step_line(&c.tape, &c.fake, "kill-after-view");
        // Whatever the dead helper had in flight reached the fake DC long ago: it is quiet now.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let names = c.fake.names();
        let window = (
            read_names(&names, held_from),
            wire_count(&names, held_from, "held"),
        );
        let after_kill = (c.fake.read_state("u202").0, c.fake.read_state("g301").0);
        let counted_at_kill = read_requests(&c);
        c.fake.control("hold-reads off");
        let restart_from = c.fake.names().len();
        let mut d = c.restart().await;
        d.load_all("chatListMain").await;
        d.idle(1.5).await;
        let restart_reads = read_names(&c.fake.names(), restart_from);
        let counted_after_idle = read_requests(&c);
        let after_idle = (c.fake.read_state("u202").0, c.fake.read_state("g301").0);
        phases.push(view_phase(&c, &mut d, "reassert", (U202, "u202"), &[u_id], 1.0).await);
        phases.push(
            view_phase(
                &c,
                &mut d,
                "reassert-below-top",
                (G301, "g301"),
                &[g_first],
                2.0,
            )
            .await,
        );
        d.stop().await;
        ReadRun {
            phases,
            start,
            ids: (u_id, g_first, g_top),
            killed,
            window,
            after_kill,
            counted_at_kill,
            restart_reads,
            counted_after_idle,
            after_idle,
        }
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let wire = c.fake.lines_timed();
    let at = |label: &str| step_at(&vs, label);
    let phase = |label: &str| {
        run.phases
            .iter()
            .find(|p| p.label == label)
            .expect("a phase")
    };
    let (u_id, g_first, g_top) = run.ids;
    let (private, supergroup, again) = (
        phase("view-private"),
        phase("view-supergroup"),
        phase("view-again"),
    );
    let (reassert, below) = (phase("reassert"), phase("reassert-below-top"));
    // The order fact, from the tape: TDLib's `updateChatReadInbox` of the read position the call
    // names comes BEFORE the call's answer (the same actor's mailbox, FIFO).
    let before_answer = |label: &str, chat: i64, id: i64| {
        view_and_answer(&vs, at(label), chat).is_some_and(|(marker, answer)| {
            vs[marker..answer].iter().any(|(_, v)| {
                v["@type"] == "updateChatReadInbox"
                    && v["chat_id"] == chat
                    && v["last_read_inbox_message_id"] == id
            })
        })
    };
    let private_before_answer = before_answer("view-private", U201, td_msg(4));
    let held_before_answer = before_answer("view-held", U202, u_id);
    // The position TDLib told the app after the restart, before the first re-assert (MODEL).
    let told = (
        told_inbox(&vs, at("kill-after-view"), at("reassert"), U202),
        told_inbox(&vs, at("kill-after-view"), at("reassert"), G301),
    );
    // Which way the id below g301's top went: TDLib held it again through the difference (the
    // held branch) or not (the id counted because it is at or below the top).
    let below_redelivered = new_messages(&vs, at("kill-after-view"), G301)
        .iter()
        .any(|m| m.1 == g_first);
    let side: Vec<&String> = names
        .iter()
        .filter(|n| {
            [
                "readMessageContents",
                "getMessagesViews",
                "readReactions",
                "readMentions",
            ]
            .iter()
            .any(|k| n.contains(k))
        })
        .collect();
    let total_reads = read_names(&names, 0);
    let counted = read_requests(&c);
    let ticks: Vec<(&str, Option<i64>)> = [
        ("view-private", U201),
        ("view-supergroup", C401),
        ("reassert", U202),
        ("reassert-below-top", G301),
    ]
    .iter()
    .map(|(label, chat)| (*label, tick_ms(&vs, &wire, label, *chat, base)))
    .collect();
    println!(
        "FAKEDC read-act: reads private={:?} supergroup={:?} again={:?} reassert={:?} below_top={:?} update_before_answer private={private_before_answer} held={held_before_answer}",
        private.reads, supergroup.reads, again.reads, reassert.reads, below.reads,
    );
    println!(
        "FAKEDC read-act: kill window (after the pushes) wire_reads={:?} held_answers={} restart_reads={:?} told_below_viewed=({}, {}) below_top_redelivered={below_redelivered} side_requests={} timer_tick_ms={ticks:?} lines={}",
        run.window.0,
        run.window.1,
        run.restart_reads,
        told.0.is_some_and(|t| t < u_id),
        told.1.is_some_and(|t| t < g_first),
        side.len(),
        vs.len()
    );
    write_recording("read-act", &c.tape, &c.fake);
    common_end(traps, &c, "read-act");
    // The set-up: u201's own message read its chat, c401 is unread (K1's case pins the same).
    assert_eq!(
        run.start,
        ((1, 0), (0, 0)),
        "the standard world's read state"
    );
    // GOLDEN: the read act of three unread ids is ONE request, after TDLib's own update.
    assert_eq!(private.reads, (1, 0), "view-private: the requests");
    assert!(
        private_before_answer,
        "view-private: the updateChatReadInbox did not come before the answer"
    );
    // GOLDEN-VIA-MODEL: the request named the newest id's server id.
    assert_eq!(private.inbox, 4, "view-private: the read mark");
    assert_eq!(supergroup.reads, (0, 1), "view-supergroup: the requests");
    assert_eq!(supergroup.inbox, 1, "view-supergroup: the read mark");
    // GOLDEN: a repeat at or below TDLib's position costs no request (`MessagesManager.cpp:16918`).
    assert_eq!(again.reads, (0, 0), "view-again: the requests");
    assert_eq!(
        again.counted, supergroup.counted,
        "view-again: the fake's count"
    );
    assert_eq!(again.inbox, 4, "view-again: the read mark");
    assert!(run.killed, "the helper was not killed");
    assert!(
        u_id < g_first && g_first < g_top,
        "the pushed messages are not in order"
    );
    // MODEL (the premise of K4's replay): the fake never applied the held reads, and what it got
    // late did not apply either; TDLib is told the older position after the restart.
    let (u_server, g_server) = (u_id >> 20, g_first >> 20);
    assert!(
        run.after_kill.0 < u_server && run.after_kill.1 < g_server,
        "the fake applied a held read: {:?}",
        run.after_kill
    );
    assert!(
        told.0.is_some_and(|t| t < u_id) && told.1.is_some_and(|t| t < g_first),
        "after the restart TDLib was told a position at or above the viewed id: {told:?}"
    );
    // GOLDEN: with the DB off nothing is replayed after the restart.
    assert_eq!(
        run.restart_reads,
        (0, 0),
        "a read request after the restart"
    );
    assert_eq!(
        run.counted_after_idle, run.counted_at_kill,
        "the fake counted a request after the restart"
    );
    assert_eq!(
        run.after_idle, run.after_kill,
        "a held read was applied after hold-reads off"
    );
    // GOLDEN: each re-assert is ONE request (the held branch and the id below the top alike);
    // GOLDEN-VIA-MODEL: with the id it named.
    assert_eq!(reassert.reads, (1, 0), "reassert: the requests");
    assert_eq!(reassert.inbox, u_server, "reassert: the read mark");
    assert_eq!(below.reads, (1, 0), "reassert-below-top: the requests");
    assert_eq!(below.inbox, g_server, "reassert-below-top: the read mark");
    assert!(
        side.is_empty(),
        "a content, views or reactions request went with a read: {side:?}"
    );
    assert_eq!(
        counted as usize,
        total_reads.0 + total_reads.1,
        "the fake's counter and the wire log disagree"
    );
    drift_end("read-act", &c.tape);
    case_dir("read-act", c.dir);
}

/// What [`case_read_act`] collects while its helpers run.
struct ReadRun {
    phases: Vec<ReadPhase>,
    /// u201's and c401's read marks before the first act.
    start: ((i64, i64), (i64, i64)),
    /// TDLib's ids: u202's pushed message, g301's first and second (its top).
    ids: (i64, i64, i64),
    killed: bool,
    /// After the pushes of `view-held` to the end of the quiet second after the kill: the read
    /// requests that reached the wire and the answers the fake held.
    window: ((usize, usize), usize),
    /// The fake's read marks (u202, g301) once it was quiet after the kill, and its counter then.
    after_kill: (i64, i64),
    counted_at_kill: u64,
    /// After `hold-reads off`, the restart, the lists and 1.5 s: the read requests on the wire,
    /// the fake's counter and its marks.
    restart_reads: (usize, usize),
    counted_after_idle: u64,
    after_idle: (i64, i64),
}

/// A new message pushed into `chat` (`word` in the fake's control language) whose TDLib id is
/// above `above` (so a re-delivery of an older one does not count), waited for; its id.
async fn push_new_above(c: &Case, d: &mut Drv, word: &str, chat: i64, above: i64) -> i64 {
    c.fake.control(&format!("push new {word}"));
    let got = std::cell::Cell::new(0_i64);
    let seen = d
        .wait_for(20, |v| {
            let id = v["message"]["id"].as_i64().unwrap_or(0);
            let hit =
                v["@type"] == "updateNewMessage" && v["message"]["chat_id"] == chat && id > above;
            if hit {
                got.set(id);
            }
            hit
        })
        .await;
    assert!(seen, "the message pushed into {word} did not arrive");
    got.get()
}

/// The id of the last message TDLib told for `chat` first in the tape range `vs` (its
/// `updateChatLastMessage`: `updateNewChat` carries no `last_message` with the message DB off).
fn chat_last(vs: &[(Instant, Value)], chat: i64) -> Option<i64> {
    vs.iter()
        .find(|(_, v)| v["@type"] == "updateChatLastMessage" && v["chat_id"] == chat)
        .and_then(|(_, v)| v["last_message"]["id"].as_i64())
}

/// The id of the last message TDLib told for `chat` in the tape range `from..to` (the LAST of its
/// `updateChatLastMessage`s and of the `last_message` an `updateNewChat` carries): where its
/// history ends as far as TDLib knows.
fn last_told(vs: &[(Instant, Value)], from: usize, to: usize, chat: i64) -> Option<i64> {
    vs[from..to]
        .iter()
        .filter_map(|(_, v)| match v["@type"].as_str() {
            Some("updateChatLastMessage") if v["chat_id"] == chat => {
                v["last_message"]["id"].as_i64()
            }
            Some("updateNewChat") if v["chat"]["id"] == chat => {
                v["chat"]["last_message"]["id"].as_i64()
            }
            _ => None,
        })
        .next_back()
}

/// Whether the JSON value holds the number `id` anywhere (compared by value, never as text).
fn names_id(v: &Value, id: i64) -> bool {
    match v {
        Value::Number(n) => n.as_i64() == Some(id),
        Value::Array(a) => a.iter().any(|x| names_id(x, id)),
        Value::Object(o) => o.values().any(|x| names_id(x, id)),
        _ => false,
    }
}

/// How many objects TDLib told in the tape range `from..` (the app's markers and the case's steps
/// are not TDLib's) name one of the message ids `ids` anywhere.
fn objects_naming(vs: &[(Instant, Value)], from: usize, ids: &[i64]) -> usize {
    vs.iter()
        .skip(from)
        .filter(|(_, v)| !matches!(v["@type"].as_str(), Some("kourierRequest" | "kourierStep")))
        .filter(|(_, v)| ids.iter().any(|id| names_id(v, *id)))
        .count()
}

/// W1's golden check over one chat's pages (RC12): what is wrong with them, by name. Empty: the
/// pages hold the lost tail (`tail`, the messages TDLib processed before the kill) exactly once,
/// newest first, BEFORE any id at or below Kourier's stale cursor `s`, and not the message `m`
/// the first page started from: no page bridges `m` to the stale cursor.
fn tail_problems(pages: &[Vec<i64>], tail: &[i64], s: i64, m: i64) -> Vec<&'static str> {
    let flat: Vec<i64> = pages.iter().flatten().copied().collect();
    let mut problems = Vec::new();
    if flat.contains(&m) {
        problems.push("a page holds the message it started from");
    }
    if flat.windows(2).any(|w| w[0] <= w[1]) {
        problems.push("not newest first, or an id twice");
    }
    let old = flat.iter().position(|id| *id <= s);
    for t in tail {
        match flat.iter().position(|id| id == t) {
            None => {
                problems.push("a tail message is missing");
                break;
            }
            Some(at) if old.is_some_and(|o| at > o) => {
                problems.push("a tail message comes after an id at or below the cursor");
                break;
            }
            Some(_) => {}
        }
    }
    problems
}

/// One chat of RC12: the world's newest message before the tail (Kourier's stale cursor `s`), the
/// tail TDLib processed before the kill, the first live message after the restart and the pages
/// asked from it.
struct LiveChat {
    word: &'static str,
    chat: i64,
    s: i64,
    tail: Vec<i64>,
    m: i64,
    pages: Vec<(Vec<i64>, usize)>,
}

/// RC12a `first-live-after-load` and RC12b `first-live-before-load` (TG-4 W1, Before R1 (4); B1's
/// case and the id trigger's golden question): what TDLib's pages hold after a restart (message
/// database off) when the FIRST live message `M` of a chat comes, and whether any page bridges `M`
/// to the cursor Kourier stored before the kill.
///
/// Run 1: login, `loadChats(main)`, step `tail`: three new messages in each of two chats pushed
/// one by one and waited for (u201 and c401 in (a), u201 and g301 in (b): the "lost tail" TDLib
/// processed; the newest message before them is the stale cursor `s`), 1 s of quiet (TDLib saves
/// its pts, RC3b), SIGKILL (`kill-quiet`). Run 2: (a) step `chats` (`loadChats(main)`), 1 s of
/// quiet, step `first-live` (`M` pushed into each chat, waited for), 1 s, step `pages`; (b) step
/// `first-live` straight after the restart (TDLib has not been told its chats: it knows the peers
/// from its chat-info database, not the dialogs), step `pages`, step `chats`; then 2 s and stop.
/// `pages` is, per chat, `getChatHistory(chat, from = M, offset 0, limit 50)` (the product's own
/// `Fetch::Gap` request), then again from the oldest id of the last page until an id at or below
/// `s` came, a page was empty or four calls ran.
///
/// GOLDEN (TDLib's own code at the pin, asserted on every run): in (a) the FIRST page from `M` is
/// TDLib's local answer, the chat's top message ALONE (U10: after the restart TDLib holds nothing
/// older than the top it was told, so no local page bridges `M` to the stale cursor; in (b) it holds
/// no top at all and the first page is the server's whole page, not asserted); across a chat's pages
/// every message of the tail comes exactly once, newest first, before any id at or below `s`; `M` is
/// in no page from `M`; after the restart a tail message is told at most once more (never twice) and
/// `M` exactly once; a chat's `updateNewChat` comes before `M`'s `updateNewMessage` in run 2.
/// GOLDEN-VIA-MODEL (a fact read back through the fake's behaviour, valid because a fakedc unit test
/// pins the fake: `the_history_pages_from_the_newest_by_offset_id_and_add_offset` over
/// `World::history_ids`): the SERVER pages (the second page of (a), the only page of (b)) hold the
/// tail exactly once, because the fake answers every stored message below the id. PRINTED, not
/// asserted (timing or MODEL): each page's length and whether its call reached the server, the
/// `updates.getDifference`, `messages.getPeerDialogs` and `updates.getChannelDifference` requests of
/// `first-live`.
fn case_first_live(traps: &Traps, before_load: bool) {
    let name = if before_load {
        "first-live-before-load"
    } else {
        "first-live-after-load"
    };
    let c = case_setup();
    let second = if before_load {
        ("g301", G301, td_msg(8))
    } else {
        ("c401", C401, td_msg(1))
    };
    let mut chats: Vec<LiveChat> = [("u201", U201, td_msg(4)), second]
        .into_iter()
        .map(|(word, chat, s)| LiveChat {
            word,
            chat,
            s,
            tail: Vec::new(),
            m: 0,
            pages: Vec::new(),
        })
        .collect();
    let (killed, live) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        d.idle(1.0).await;
        step_line(&c.tape, &c.fake, "tail");
        for _ in 0..3 {
            for l in chats.iter_mut() {
                let id = push_new(&c, &mut d, l.word, l.chat).await;
                l.tail.push(id);
            }
        }
        d.idle(1.0).await;
        let killed = kill(c.pids.lock().expect("pids").last().copied());
        d.stop().await;
        // Stamped after the killed driver stopped ([`starts_a_run`]).
        step_line(&c.tape, &c.fake, "kill-quiet");
        let mut d = c.restart().await;
        if !before_load {
            step_line(&c.tape, &c.fake, "chats");
            d.load_all("chatListMain").await;
            d.idle(1.0).await;
        }
        step_line(&c.tape, &c.fake, "first-live");
        let live_from = c.fake.names().len();
        for l in chats.iter_mut() {
            let above = l.tail.last().copied().unwrap_or(0);
            l.m = push_new_above(&c, &mut d, l.word, l.chat, above).await;
        }
        d.idle(1.0).await;
        let live_to = c.fake.names().len();
        step_line(&c.tape, &c.fake, "pages");
        for l in chats.iter_mut() {
            l.pages = d.pages_from(&c.fake, l.chat, l.m, l.s).await;
        }
        if before_load {
            step_line(&c.tape, &c.fake, "chats");
            d.load_all("chatListMain").await;
        }
        d.idle(2.0).await;
        d.stop().await;
        (killed, (live_from, live_to))
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let kill_at = step_at(&vs, "kill-quiet");
    let first_live_wire = &names[..live.1.min(names.len())];
    let counts = (
        wire_count(first_live_wire, live.0, "updates.getDifference"),
        wire_count(first_live_wire, live.0, "messages.getPeerDialogs"),
        wire_count(first_live_wire, live.0, "updates.getChannelDifference"),
    );
    // Per chat: the tail's re-deliveries after the restart (at most once each), `M`'s arrivals,
    // whether the chat was told before `M`, the golden problems of the pages.
    let facts: Vec<_> = chats
        .iter()
        .map(|l| {
            let after = new_messages(&vs, kill_at, l.chat);
            let again = l
                .tail
                .iter()
                .map(|t| after.iter().filter(|m| m.1 == *t).count())
                .collect::<Vec<_>>();
            let m_count = after.iter().filter(|m| m.1 == l.m).count();
            let chat_at = vs[kill_at..]
                .iter()
                .position(|(_, v)| v["@type"] == "updateNewChat" && v["chat"]["id"] == l.chat);
            let m_at = vs[kill_at..]
                .iter()
                .position(|(_, v)| v["@type"] == "updateNewMessage" && v["message"]["id"] == l.m);
            let told_first = matches!((chat_at, m_at), (Some(a), Some(b)) if a < b);
            let ids: Vec<Vec<i64>> = l.pages.iter().map(|p| p.0.clone()).collect();
            let problems = tail_problems(&ids, &l.tail, l.s, l.m);
            (again, m_count, told_first, problems)
        })
        .collect();
    for (l, f) in chats.iter().zip(&facts) {
        println!(
            "FAKEDC {name}: {} tail={} redelivered={:?} m_once={} chat_told_before_m={} pages(len, server_calls)={:?} problems={:?}",
            l.word,
            l.tail.len(),
            f.0,
            f.1 == 1,
            f.2,
            l.pages.iter().map(|p| (p.0.len(), p.1)).collect::<Vec<_>>(),
            f.3,
        );
    }
    println!(
        "FAKEDC {name}: killed={killed} first_page_is_the_top_alone={} first_live wire: get_difference={} get_peer_dialogs={} get_channel_difference={} lines={}",
        chats.first().is_some_and(|l| {
            l.pages
                .first()
                .is_some_and(|p| p.0.len() == 1 && l.tail.last() == p.0.first())
        }),
        counts.0,
        counts.1,
        counts.2,
        vs.len()
    );
    write_recording(name, &c.tape, &c.fake);
    common_end(traps, &c, name);
    assert!(killed, "the helper was not killed");
    for (l, f) in chats.iter().zip(&facts) {
        // The set-up: TDLib told the chat's newest message before the tail (the stale cursor).
        assert_eq!(
            chat_last(&vs[..kill_at], l.chat),
            Some(l.s),
            "{}: the world's newest message before the tail",
            l.word
        );
        assert_eq!(l.tail.len(), 3, "{}: the tail", l.word);
        // GOLDEN: no page bridges `M` to the stale cursor, and none holds `M`.
        assert!(f.3.is_empty(), "{name}: {}: the pages: {:?}", l.word, f.3);
        // GOLDEN: a message TDLib processed before the kill comes again at most once.
        assert!(
            f.0.iter().all(|n| *n <= 1),
            "{name}: {}: a tail message came twice after the restart: {:?}",
            l.word,
            f.0
        );
        assert_eq!(f.1, 1, "{name}: {}: the first live message", l.word);
        // GOLDEN: TDLib tells the chat before the message that names it (M1), per helper run.
        assert!(f.2, "{name}: {}: M before its updateNewChat", l.word);
        // GOLDEN (U10, the TDLib-local half of gate (4)): with the chats loaded before `M`, the
        // first page from `M` is the chat's top message alone. The assert above (`tail_problems`)
        // also passes when the page is the fake's whole server page (that is -before-load's case),
        // so it cannot tell a local page from a server page; this one can.
        if !before_load {
            assert_eq!(
                l.pages.first().map(|p| p.0.as_slice()),
                l.tail.last().map(std::slice::from_ref),
                "{name}: {}: the first page from M is not TDLib's top message alone",
                l.word
            );
        }
    }
    drift_end(name, &c.tape);
    case_dir(name, c.dir);
}

/// One chat of RC14: the world's newest message before the queued ones (Kourier's stale cursor
/// `s`), the messages queued while the helper was off, the last message TDLib told after the
/// restart and the pages asked from it.
struct TooLongChat {
    word: &'static str,
    chat: i64,
    s: i64,
    queued: Vec<i64>,
    told: i64,
    pages: Vec<(Vec<i64>, usize)>,
}

/// What [`case_difference_too_long`] collects while its helpers run.
struct TooLongRun {
    /// The wire log's length at the start of the restart, at its end, at the live gap's start and at its end.
    wire: (usize, usize, usize, usize),
    /// The first live message after the gap (`M'`) and the local page asked from it.
    live: (i64, Vec<i64>),
}

/// RC14 `difference-too-long` (TG-4 W3, Q-L12's known limit): what TDLib does with the COMMON box's
/// `updates.differenceTooLong` with its message database off, at a restart and in a live run.
///
/// Run 1: login, `loadChats(main)`, 1 s, step `stop`: a CLEAN stop (TDLib saves its pts). The fake
/// DC's common threshold is set to 2 (`queue threshold common 2`: MODEL) and four changes are QUEUED
/// (three new messages of u201, one of g301: more than the threshold, so the difference from the
/// saved pts is too long); step `restart-too-long`: run 2 starts, `loadChats(main)`, 1 s, then per
/// chat the pages the start decision would ask: `getChatHistory(chat, from = the last message
/// TDLib told)` (the product's own `Fetch::Gap` request; critic L2: `replay::key` is the chat and
/// that id) and again from the oldest id until an id at or below the stale cursor `s` came; 2 s.
/// The live part, in the same run: `queue threshold common 1`, step `live-gap`: `push gap u201` (two
/// messages stored, a third pushed: a pts gap of two), 3 s; step `live-next`: `push new u201` (`M'`,
/// waited for), 1 s; step `live-pages`: `getChatHistory(u201, from M', 0, 20)` once, 1 s, stop.
///
/// GOLDEN (TDLib's own code at the pin, asserted on every run): AT THE RESTART the difference is
/// asked more than once (the too-long answer sets the pts and TDLib asks again: `>= 2`), NONE of
/// the four queued messages reaches the app as `updateNewMessage` (the window is dropped, nothing
/// else is done for it), and after `loadChats` the chat's last message (what TDLib told) is the
/// newest queued message of each chat (`getDialogs` is applied after the difference); the pages
/// from it hold the queued messages exactly once, newest first, before any id at or below `s`.
/// LIVE: a difference is asked after the push (`>= 1`, the count is printed), NO object TDLib
/// tells in the rest of the run names one of the three `gap` messages (not as `updateNewMessage`,
/// not as the chat's last message: the revealing push does not reach the app either), `M'` comes
/// exactly once, and the local first page from `M'` starts at the message that was the chat's last
/// BEFORE the gap, with none of the three in it (TDLib bridges the dropped window in its local
/// history, `OrderedMessage.cpp:156-180`: the hole the known limit states). MODEL: the thresholds,
/// the window sizes, the page sizes, the ids.
fn case_difference_too_long(traps: &Traps) {
    let name = "difference-too-long";
    let c = case_setup();
    let mut chats = vec![
        TooLongChat {
            word: "u201",
            chat: U201,
            s: td_msg(4),
            queued: vec![td_msg(9), td_msg(10), td_msg(11)],
            told: 0,
            pages: Vec::new(),
        },
        TooLongChat {
            word: "g301",
            chat: G301,
            s: td_msg(8),
            queued: vec![td_msg(12)],
            told: 0,
            pages: Vec::new(),
        },
    ];
    let run = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        d.load_all("chatListMain").await;
        d.idle(1.0).await;
        step_line(&c.tape, &c.fake, "stop");
        d.stop().await;
        c.fake.control("queue threshold common 2");
        for l in &chats {
            for _ in &l.queued {
                c.fake.control(&format!("queue new {}", l.word));
            }
        }
        step_line(&c.tape, &c.fake, "restart-too-long");
        let restart_from = c.fake.names().len();
        let mut d = c.restart().await;
        d.load_all("chatListMain").await;
        d.idle(1.0).await;
        let at = step_at(&values(&c.tape), "restart-too-long");
        for l in chats.iter_mut() {
            let vs = values(&c.tape);
            l.told = last_told(&vs, at, vs.len(), l.chat)
                .unwrap_or_else(|| panic!("{}: TDLib told no last message", l.word));
            l.pages = d.pages_from(&c.fake, l.chat, l.told, l.s).await;
        }
        d.idle(2.0).await;
        let restart_to = c.fake.names().len();
        c.fake.control("queue threshold common 1");
        step_line(&c.tape, &c.fake, "live-gap");
        let gap_from = c.fake.names().len();
        c.fake.control("push gap u201");
        d.idle(3.0).await;
        let gap_to = c.fake.names().len();
        step_line(&c.tape, &c.fake, "live-next");
        let above = chats[0].told;
        let m = push_new_above(&c, &mut d, "u201", U201, above).await;
        d.idle(1.0).await;
        step_line(&c.tape, &c.fake, "live-pages");
        let local = d.history(U201, m).await;
        d.idle(1.0).await;
        d.stop().await;
        TooLongRun {
            wire: (restart_from, restart_to, gap_from, gap_to),
            live: (m, local),
        }
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let (restart_at, gap_at, next_at) = (
        step_at(&vs, "restart-too-long"),
        step_at(&vs, "live-gap"),
        step_at(&vs, "live-next"),
    );
    let (m, local) = (run.live.0, &run.live.1);
    // The three messages the live gap stored and skipped: the world's ids between its newest
    // message (the last one queued) and `M'` (premise: nothing else was made in between).
    let top = chats.iter().map(|l| l.told).max().unwrap_or(0) >> 20;
    let gap_ids: Vec<i64> = (top + 1..top + 4).map(td_msg).collect();
    let restart_wire = &names[..run.wire.1.min(names.len())];
    let gap_wire = &names[..run.wire.3.min(names.len())];
    let restart_diffs = wire_count(restart_wire, run.wire.0, "updates.getDifference");
    let gap_diffs = wire_count(gap_wire, run.wire.2, "updates.getDifference");
    let arrived_at_restart = vs[restart_at..gap_at]
        .iter()
        .filter(|(_, v)| v["@type"] == "updateNewMessage")
        .count();
    let gap_arrived = vs[gap_at..]
        .iter()
        .filter(|(_, v)| {
            v["@type"] == "updateNewMessage"
                && v["message"]["id"]
                    .as_i64()
                    .is_some_and(|id| gap_ids.contains(&id))
        })
        .count();
    let gap_named = objects_naming(&vs, gap_at, &gap_ids);
    let m_count = vs[gap_at..]
        .iter()
        .filter(|(_, v)| v["@type"] == "updateNewMessage" && v["message"]["id"] == m)
        .count();
    let m_in_next = vs[next_at..]
        .iter()
        .any(|(_, v)| v["@type"] == "updateNewMessage" && v["message"]["id"] == m);
    let problems: Vec<_> = chats
        .iter()
        .map(|l| {
            let tail: Vec<i64> = l.queued.iter().copied().filter(|q| *q != l.told).collect();
            let ids: Vec<Vec<i64>> = l.pages.iter().map(|p| p.0.clone()).collect();
            tail_problems(&ids, &tail, l.s, l.told)
        })
        .collect();
    for (l, p) in chats.iter().zip(&problems) {
        println!(
            "FAKEDC {name}: {} queued={} told_is_the_newest_queued={} pages(len, server_calls)={:?} problems={p:?}",
            l.word,
            l.queued.len(),
            l.queued.last() == Some(&l.told),
            l.pages.iter().map(|p| (p.0.len(), p.1)).collect::<Vec<_>>(),
        );
    }
    println!(
        "FAKEDC {name}: restart: get_difference={restart_diffs} get_state={} get_peer_dialogs={} new_messages={arrived_at_restart}",
        wire_count(restart_wire, run.wire.0, "updates.getState"),
        wire_count(restart_wire, run.wire.0, "messages.getPeerDialogs"),
    );
    println!(
        "FAKEDC {name}: live: get_difference={gap_diffs} gap_ids_as_new_messages={gap_arrived} objects_naming_a_gap_message={gap_named} m_once={} m_in_live_next={m_in_next} local_page_len={} local_page_starts_at_the_old_last={} lines={}",
        m_count == 1,
        local.len(),
        local.first() == chats.first().map(|l| &l.told),
        vs.len()
    );
    write_recording(name, &c.tape, &c.fake);
    common_end(traps, &c, name);
    // The set-up (MODEL ids): what was queued is what TDLib told the chat's last message.
    for l in &chats {
        assert_eq!(
            l.queued.last(),
            Some(&l.told),
            "{}: TDLib's last message after the restart is not the newest queued one",
            l.word
        );
    }
    assert_eq!(
        m >> 20,
        top + 4,
        "M' is not the world's fourth message after the queued ones"
    );
    // GOLDEN at the restart: the difference is asked, answered too long and asked once more; the
    // window never reaches the app; the pages from the told last hold the queued messages.
    assert!(
        restart_diffs >= 2,
        "the restart asked {restart_diffs} differences (a too-long one is asked again)"
    );
    assert_eq!(
        arrived_at_restart, 0,
        "a queued message reached the app as an updateNewMessage"
    );
    for (l, p) in chats.iter().zip(&problems) {
        assert!(p.is_empty(), "{name}: {}: the pages: {p:?}", l.word);
    }
    // GOLDEN live: a difference after the push, the window dropped, no object names it, `M'` once,
    // and the local page bridges the window.
    assert!(gap_diffs >= 1, "the live gap asked no difference");
    assert_eq!(
        gap_arrived, 0,
        "a message of the dropped window reached the app"
    );
    assert_eq!(
        gap_named, 0,
        "an object after the gap names a message of the dropped window"
    );
    assert_eq!(m_count, 1, "the first live message after the gap: not once");
    assert!(m_in_next, "M' did not arrive in the step live-next");
    assert_eq!(
        local.first(),
        chats.first().map(|l| &l.told),
        "the local page from M' does not start at the chat's last message before the gap"
    );
    assert!(
        !local.iter().any(|id| gap_ids.contains(id) || *id == m),
        "the local page holds M' or a message of the dropped window"
    );
    drift_end(name, &c.tape);
    case_dir(name, c.dir);
}

/// RC10 `incoming-call` (§4.5, the 0002 proof; critic L-h): while Ready, `phoneCallRequested`
/// from the peer, then the discard (missed) with the call line. The regular case discards after
/// `wait` 2.5 s (no call actor with 0002: nothing to time out; the 2.5 s are two of TDLib's 1 s
/// retry periods for a push it cannot parse, the positive witness below, with a second to spare
/// for a slow builder); `wait` 25 s is the pre-0002 run's (past TDLib's 20 s receive timeout,
/// `CallActor.cpp`).
fn case_incoming_call(traps: &Traps, wait: f32, pre0002: bool) {
    let name = if pre0002 {
        "incoming-call-pre0002"
    } else {
        "incoming-call"
    };
    let c = case_setup_with(pre0002);
    let (line_seen, call_from) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        d.idle(1.0).await;
        c.record(true);
        let call_from = c.fake.names().len();
        step_line(&c.tape, &c.fake, "call");
        c.fake.control("push call u201");
        d.idle(wait).await;
        step_line(&c.tape, &c.fake, "call-missed");
        c.fake.control("push call-missed u201");
        let seen = d
            .wait_for(20, |v| {
                v["@type"] == "updateNewMessage"
                    && v["message"]["chat_id"] == U201
                    && v["message"]["content"]["@type"] == "messageCall"
            })
            .await;
        d.idle(1.0).await;
        d.stop().await;
        (seen, call_from)
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let phone = wire_prefixed(&names, 0, "phone.");
    let dh = wire_count(&names, 0, "messages.getDhConfig");
    let received = wire_count(&names, 0, "phone.receivedCall");
    // The POSITIVE WITNESS (LC-20, critic MEDIUM-2): a push TDLib cannot parse is dropped
    // (`SessionProxy.cpp` -> `nullptr`) and answered with a scheduled `getDifference`
    // (`Td::on_update`, "failed to fetch updates", the retry timer starts at 1 s). So no
    // `updates.getDifference` from the call push to the end of the run (at least 2.5 s after the
    // push, 25 s with the pre-0002 library: the case idles that long before the discard, so the
    // scheduled request would be on the wire under any load) shows that the library PARSED the
    // push, and the absences below are not a vacuous pass of a fake DC whose layer TDLib cannot
    // read.
    let diffs_after_push = wire_count(&names, call_from, "updates.getDifference");
    let update_calls = vs
        .iter()
        .filter(|(_, v)| v["@type"] == "updateCall")
        .count();
    let reason = vs
        .iter()
        .map(|(_, v)| v)
        .find(|v| {
            v["@type"] == "updateNewMessage" && v["message"]["content"]["@type"] == "messageCall"
        })
        .and_then(|v| {
            v["message"]["content"]["discard_reason"]["@type"]
                .as_str()
                .map(str::to_owned)
        });
    println!(
        "FAKEDC {name}: wait_s={wait} phone_requests_on_wire={phone} phone_received_call={received} get_dh_config={dh} get_difference_after_push={diffs_after_push} update_call={update_calls} message_call={line_seen} discard_reason={reason:?} lines={}",
        vs.len()
    );
    if pre0002 {
        let wire: Vec<&str> = names
            .iter()
            .filter(|n| n.starts_with("phone.") || n.as_str() == "messages.getDhConfig")
            .map(String::as_str)
            .collect();
        println!("FAKEDC {name}: the wire's call lines {wire:?}");
        // S-M1: the one run that let TDLib's call code run reads the same network end.
        net_end_of(traps, &c, name);
        assert!(
            received >= 1,
            "the pre-0002 library sent no phone.receivedCall"
        );
        assert_eq!(
            diffs_after_push, 0,
            "updates.getDifference after the call push: TDLib did not parse it"
        );
        case_dir(name, c.dir);
        return;
    }
    write_recording(name, &c.tape, &c.fake);
    common_end(traps, &c, name);
    assert_eq!(phone, 0, "a phone.* request on the wire");
    assert_eq!(dh, 0, "messages.getDhConfig on the wire");
    assert_eq!(update_calls, 0, "an updateCall reached the app");
    assert_eq!(
        diffs_after_push, 0,
        "updates.getDifference after the call push: TDLib did not parse it"
    );
    assert!(line_seen, "the call line's messageCall did not arrive");
    drift_end(name, &c.tape);
    case_dir(name, c.dir);
}

/// RC11 `basic-group-upgrade`: after RC1's load the basic group becomes a new supergroup (both
/// service lines, `updateChannel`); then `loadChats(main)` until 404.
fn case_basic_group_upgrade(traps: &Traps) {
    let c = case_setup();
    let (wire_from, main) = c.rt.block_on(async {
        let mut d = c.start().await;
        d.login().await;
        c.record(true);
        c.load(&mut d).await;
        let wire_from = c.fake.names().len();
        step_line(&c.tape, &c.fake, "upgrade");
        c.fake.control("push upgrade g301 c404");
        d.wait_for(20, |v| {
            v["@type"] == "updateBasicGroup" && v["basic_group"]["upgraded_to_supergroup_id"] == 404
        })
        .await;
        d.idle(2.0).await;
        step_line(&c.tape, &c.fake, "load-after-upgrade");
        let main = d.load_all("chatListMain").await;
        d.idle(1.0).await;
        d.stop().await;
        (wire_from, main)
    });
    let vs = values(&c.tape);
    let names = c.fake.names();
    let at = step_at(&vs, "upgrade");
    let upgraded = vs[at..].iter().any(|(_, v)| {
        v["@type"] == "updateBasicGroup" && v["basic_group"]["upgraded_to_supergroup_id"] == 404
    });
    let new_chat = vs[at..]
        .iter()
        .any(|(_, v)| v["@type"] == "updateNewChat" && v["chat"]["id"] == C404);
    let contents: std::collections::BTreeSet<String> = vs[at..]
        .iter()
        .filter(|(_, v)| v["@type"] == "updateNewMessage")
        .filter_map(|(_, v)| v["message"]["content"]["@type"].as_str().map(str::to_owned))
        .collect();
    let order = chat_before_reference(&vs);
    println!(
        "FAKEDC basic-group-upgrade: upgraded={upgraded} new_supergroup_chat={new_chat} contents={contents:?} main={main:?} get_channels={} get_full_channel={} order_breach={order:?} lines={}",
        wire_count(&names, wire_from, "channels.getChannels"),
        wire_count(&names, wire_from, "channels.getFullChannel"),
        vs.len()
    );
    write_recording("basic-group-upgrade", &c.tape, &c.fake);
    common_end(traps, &c, "basic-group-upgrade");
    assert!(
        upgraded,
        "no updateBasicGroup with upgraded_to_supergroup_id"
    );
    assert!(new_chat, "no updateNewChat of the new supergroup");
    for k in ["messageChatUpgradeTo", "messageChatUpgradeFrom"] {
        assert!(contents.contains(k), "no {k} content: {contents:?}");
    }
    assert!(main.1, "the main list did not end in 404");
    assert_eq!(
        order, None,
        "an object named a chat before its updateNewChat"
    );
    drift_end("basic-group-upgrade", &c.tape);
    case_dir("basic-group-upgrade", c.dir);
}

/// The recording cases by name (the offline child's dispatch); false: no such case.
pub(super) fn run(case: &str, traps: &Traps) -> bool {
    match case {
        "fresh-start" => case_fresh_start(traps),
        "restart-saved-pts" => case_restart_saved_pts(traps),
        "kill-at-once" => case_kill_after_processing(traps, false),
        "kill-after-quiet" => case_kill_after_processing(traps, true),
        "reconnect" => case_reconnect(traps),
        "gap-known-sender" => case_gap_known_sender(traps),
        "gap-filled-by-push" => case_gap_filled_by_push(traps),
        "gap-channel" => case_gap_channel(traps),
        "gap-unknown-sender" => case_gap_unknown_sender(traps),
        "channel-difference" => case_channel_difference(traps),
        "edit" => case_edit(traps),
        "delete" => case_delete(traps),
        "read-elsewhere" => case_read_elsewhere(traps),
        "read-act" => case_read_act(traps),
        "first-live-after-load" => case_first_live(traps, false),
        "first-live-before-load" => case_first_live(traps, true),
        "difference-too-long" => case_difference_too_long(traps),
        "incoming-call" => case_incoming_call(traps, 2.5, false),
        "incoming-call-pre0002" => case_incoming_call(traps, 25.0, true),
        "basic-group-upgrade" => case_basic_group_upgrade(traps),
        _ => return false,
    }
    true
}

// ----- the tests (parent halves) -----

/// Brief §3.10 (H-L6, renamed from `the_recordings_match_the_library`, which compared no stream
/// with the library): every recording case compares the stream it saw with its committed recording
/// ([`drift_check`], run by the cases RC1-RC11 above under the fake-DC environment). THIS test
/// checks what makes that comparison mean something, without the environment: every recording the
/// record command writes (but the skipped one) is committed in `testdata/`, and the comparison
/// catches a recording with one type removed. Under `KOURIER_REQUIRE_FAKEDC=1` it also fails when
/// the fake-DC variables are missing: a lane run without them compared nothing.
#[test]
fn the_recordings_are_committed_and_the_comparator_sees_a_removed_type() {
    if required(REQUIRE_FAKEDC_ENV) {
        let missing: Vec<&str> = [LIB_ENV, BIN_ENV, KEY_ENV]
            .into_iter()
            .filter(|v| var(v).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "{missing:?} unset and {REQUIRE_FAKEDC_ENV}=1: the recording cases compared nothing"
        );
    }
    let names = [
        "fresh-start",
        "restart-saved-pts",
        "kill-at-once",
        "kill-after-quiet",
        "reconnect",
        "gap-known-sender",
        "gap-filled-by-push",
        "gap-channel",
        "gap-unknown-sender",
        "channel-difference",
        "edit",
        "delete",
        "read-elsewhere",
        "read-act",
        "first-live-after-load",
        "first-live-before-load",
        "difference-too-long",
        "incoming-call",
        "basic-group-upgrade",
    ];
    for name in names {
        let Some(lines) = committed_recording(name) else {
            panic!("testdata/rec-{name}.jsonl is not committed");
        };
        assert!(
            drift_between(&lines, &lines).is_empty(),
            "{name}: differs from itself"
        );
        // A type TDLib emitted (not an ignored one) removed: the comparison sees it.
        let kinds = drift_phases(&lines);
        let Some(kind) = kinds.iter().flat_map(|p| p.1.iter()).next().cloned() else {
            panic!("{name}: no compared type");
        };
        let removed: Vec<String> = lines
            .iter()
            .filter(|l| {
                serde_json::from_str::<Value>(l).map_or(true, |v| v["@type"] != kind.as_str())
            })
            .cloned()
            .collect();
        assert!(
            !drift_between(&removed, &lines).is_empty(),
            "{name}: one type removed is not seen"
        );
    }
}

/// S-M1: every recording case, the one pre-0002 run's body included, reaches the network end
/// (`common_end`, or `net_end_of` where the phone-request assert is not wanted), so none can pass
/// without reading the traps, the datagrams and the connection equation. The pre-0002 library is
/// deleted (P-9), so the proof for that branch is this read of the source.
#[test]
fn every_recording_case_reaches_the_network_end() {
    let source = include_str!("rec.rs");
    let mut cases = 0;
    for part in source.split("\nfn case_").skip(1) {
        let name = part.split('(').next().unwrap_or_default();
        let body = part.split("\n}\n").next().unwrap_or_default();
        if name.starts_with("setup") {
            continue;
        }
        cases += 1;
        assert!(
            body.contains("common_end(") || body.contains("net_end_of("),
            "case_{name} does not reach the network end"
        );
        if name == "incoming_call" {
            assert!(
                body.contains("net_end_of(") && body.contains("common_end("),
                "the pre-0002 branch and the regular one of RC10 each read the network end"
            );
        }
    }
    assert!(cases >= 13, "found only {cases} recording cases");
}

/// TG-4 W1's golden check over the pages of RC12 (pure, no environment needed): the right pages
/// pass, and a missing tail message, a page that bridges past the tail, an id twice and the message
/// the first page started from are each seen by name.
#[test]
fn the_tail_check_sees_a_bridge_a_duplicate_a_missing_message_and_the_start() {
    let (s, tail, m) = (4, [5, 6, 7], 8);
    // The tail newest first, then the stale cursor and below: one page, or the top alone and the rest.
    assert!(tail_problems(&[vec![7, 6, 5, 4, 3, 2, 1]], &tail, s, m).is_empty());
    assert!(tail_problems(&[vec![7], vec![6, 5, 4, 3]], &tail, s, m).is_empty());
    // A second page that skips the tail's rest (from the top straight to the cursor).
    let bridged = tail_problems(&[vec![7], vec![4, 3, 2]], &tail, s, m);
    assert!(
        bridged.contains(&"a tail message is missing"),
        "{bridged:?}"
    );
    // The pages in the wrong order: the tail after the cursor.
    let late = tail_problems(&[vec![7, 4], vec![6, 5]], &tail, s, m);
    assert!(
        late.contains(&"a tail message comes after an id at or below the cursor"),
        "{late:?}"
    );
    assert!(
        late.contains(&"not newest first, or an id twice"),
        "{late:?}"
    );
    // An id twice, and the message the first page started from.
    let twice = tail_problems(&[vec![7, 6], vec![6, 5, 4]], &tail, s, m);
    assert!(
        twice.contains(&"not newest first, or an id twice"),
        "{twice:?}"
    );
    let own = tail_problems(&[vec![8, 7, 6, 5, 4]], &tail, s, m);
    assert!(
        own.contains(&"a page holds the message it started from"),
        "{own:?}"
    );
}

/// TG-4 W3's readers of the tape (pure, no environment needed): the last message TDLib told for a
/// chat comes from its LAST `updateChatLastMessage` (or `updateNewChat`'s own) in the range, and a
/// number is found anywhere in an object by value, not as a piece of another number or a string.
#[test]
fn the_too_long_readers_find_the_told_last_message_and_a_named_id() {
    let at = Instant::now();
    let tape = |lines: &[&str]| -> Vec<(Instant, Value)> {
        lines
            .iter()
            .map(|l| (at, serde_json::from_str(l).expect("a sample line")))
            .collect()
    };
    let vs = tape(&[
        r#"{"@type":"updateNewChat","chat":{"id":201,"last_message":{"id":5}}}"#,
        r#"{"@type":"updateChatLastMessage","chat_id":201,"last_message":{"id":7}}"#,
        r#"{"@type":"updateChatLastMessage","chat_id":301,"last_message":{"id":9}}"#,
        r#"{"@type":"updateChatLastMessage","chat_id":201,"last_message":{"id":8}}"#,
        r#"{"@type":"updateChatLastMessage","chat_id":201}"#,
    ]);
    assert_eq!(last_told(&vs, 0, 5, 201), Some(8));
    assert_eq!(last_told(&vs, 0, 2, 201), Some(7));
    assert_eq!(last_told(&vs, 0, 1, 201), Some(5));
    assert_eq!(last_told(&vs, 0, 5, 301), Some(9));
    assert_eq!(last_told(&vs, 2, 3, 201), None);
    let lines = tape(&[
        r#"{"@type":"updateNewMessage","message":{"id":1234,"chat_id":201}}"#,
        r#"{"@type":"kourierRequest","request":"getChatHistory","args":{"from_message_id":1234}}"#,
        r#"{"@type":"updateX","list":[{"deep":{"n":99}}],"text":"1234","big":12345}"#,
    ]);
    // Only the first line names 1234: a marker is not TDLib's, a string and a longer number are not it.
    assert_eq!(objects_naming(&lines, 0, &[1234]), 1);
    assert_eq!(objects_naming(&lines, 0, &[99]), 1);
    assert_eq!(objects_naming(&lines, 1, &[1234]), 0);
    assert_eq!(objects_naming(&lines, 0, &[1234, 99]), 2);
    assert_eq!(objects_naming(&lines, 0, &[]), 0);
}

/// H-M3 and H-M4 on the committed `rec-kill-after-quiet.jsonl`: a refresh type that is lost in the
/// SECOND helper run is seen although the first run still holds it; the killed message's
/// `updateNewMessage` appearing after the kill is not drift; a lost `updateNewChat` still is.
#[test]
fn the_drift_check_compares_per_helper_run_and_ignores_the_killed_messages_redelivery() {
    let Some(committed) = committed_recording("kill-after-quiet") else {
        panic!("testdata/rec-kill-after-quiet.jsonl is not committed");
    };
    let kind = |l: &String| {
        serde_json::from_str::<Value>(l)
            .ok()
            .and_then(|v| v["@type"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let kill_at = committed
        .iter()
        .position(|l| l.contains("\"kourierStep\"") && l.contains("\"kill\""))
        .expect("the kill step");
    assert!(
        committed[kill_at..].iter().any(|l| kind(l) == "updateUser")
            && committed[..kill_at].iter().any(|l| kind(l) == "updateUser"),
        "the precondition: updateUser in both helper runs"
    );
    // (a) updateUser lost after the kill: seen (today's whole-file set would still hold it).
    let lost: Vec<String> = committed
        .iter()
        .enumerate()
        .filter(|(i, l)| *i < kill_at || kind(l) != "updateUser")
        .map(|(_, l)| l.clone())
        .collect();
    let diff = drift_between(&lost, &committed);
    assert!(
        diff.iter().any(|d| d.contains("run 2")),
        "a refresh lost in the second run is not seen: {diff:?}"
    );
    // (b) one updateNewMessage after the kill step: not drift.
    let mut extra = committed.clone();
    extra.insert(
        kill_at + 1,
        r#"{"@type":"updateNewMessage","message":{"@type":"message","id":1,"chat_id":201}}"#
            .to_owned(),
    );
    assert!(
        drift_between(&extra, &committed).is_empty(),
        "the killed message's re-delivery is reported as drift"
    );
    // (c) an updateNewChat lost after the kill is still seen.
    let no_chat: Vec<String> = committed
        .iter()
        .enumerate()
        .filter(|(i, l)| *i < kill_at || kind(l) != "updateNewChat")
        .map(|(_, l)| l.clone())
        .collect();
    assert!(
        !drift_between(&no_chat, &committed).is_empty(),
        "a lost updateNewChat is not seen"
    );
    // (d) a refresh type a run has and the recording lacks is not drift (F14: only a lost one is).
    let absent = DRIFT_ANY_PHASE
        .iter()
        .find(|k| !committed.iter().any(|l| kind(l) == **k))
        .expect("a refresh type rec-kill-after-quiet.jsonl never holds");
    let mut more = committed.clone();
    more.insert(kill_at, format!(r#"{{"@type":"{absent}"}}"#));
    assert!(
        drift_between(&more, &committed).is_empty(),
        "an extra refresh of TDLib's own is reported as drift"
    );
}

/// RC1 (brief §3.7).
#[test]
fn rc1_fresh_start() {
    fakedc_case("fresh-start");
}

/// RC2.
#[test]
fn rc2_restart_saved_pts() {
    fakedc_case("restart-saved-pts");
}

/// RC3, the kill at once after the frame (P-4).
#[test]
fn rc3_kill_at_once() {
    fakedc_case("kill-at-once");
}

/// RC3, the kill after 1 s of quiet (P-4).
#[test]
fn rc3_kill_after_quiet() {
    fakedc_case("kill-after-quiet");
}

/// RC4.
#[test]
fn rc4_reconnect() {
    fakedc_case("reconnect");
}

/// RC5a, a gap of a known sender (H-H1).
#[test]
fn rc5a_gap_known_sender() {
    fakedc_case("gap-known-sender");
}

/// RC5b, a gap filled by the next pushes (H-H1).
#[test]
fn rc5b_gap_filled_by_push() {
    fakedc_case("gap-filled-by-push");
}

/// RC5c, a channel's pts gap (H-H1).
#[test]
fn rc5c_gap_channel() {
    fakedc_case("gap-channel");
}

/// RC5d, a gap behind an unknown sender: the unacceptable-update path (H-H1).
#[test]
fn rc5d_gap_unknown_sender() {
    fakedc_case("gap-unknown-sender");
}

/// RC6.
#[test]
fn rc6_channel_difference() {
    fakedc_case("channel-difference");
}

/// RC7.
#[test]
fn rc7_edit() {
    fakedc_case("edit");
}

/// RC8.
#[test]
fn rc8_delete() {
    fakedc_case("delete");
}

/// RC9.
#[test]
fn rc9_read_elsewhere() {
    fakedc_case("read-elsewhere");
}

/// RC13 (TG-4 W2): the read act with the message database off, and the read a killed helper loses.
#[test]
fn rc13_read_act() {
    fakedc_case("read-act");
}

/// RC12a (TG-4 W1): the first live message after a restart, the chats loaded first.
#[test]
fn rc12a_first_live_after_load() {
    fakedc_case("first-live-after-load");
}

/// RC12b (TG-4 W1): the first live message after a restart, before the chats are loaded.
#[test]
fn rc12b_first_live_before_load() {
    fakedc_case("first-live-before-load");
}

/// RC14 (TG-4 W3): the common box's `updates.differenceTooLong`, at a restart and live.
#[test]
fn rc14_difference_too_long() {
    fakedc_case("difference-too-long");
}

/// RC10, the 0002 proof (§4.5): no `phone.*`, no `messages.getDhConfig`, no `updateCall`.
#[test]
fn rc10_incoming_call() {
    fakedc_case("incoming-call");
}

/// RC11.
#[test]
fn rc11_basic_group_upgrade() {
    fakedc_case("basic-group-upgrade");
}

/// RC10 with the PRE-0002 test-only library (§4.5, P-9): the same case with the 25 s wait shows
/// `phone.receivedCall` on the wire log (what the regular case guards against). Run ONCE by hand
/// with [`PRE0002_LIB_ENV`]; SKIPPED without it (never by `KOURIER_REQUIRE_FAKEDC`).
#[test]
fn rc10_incoming_call_pre0002() {
    if var(PRE0002_LIB_ENV).is_none() {
        println!("SKIPPED: {PRE0002_LIB_ENV} unset (the one pre-0002 run only)");
        return;
    }
    fakedc_case("incoming-call-pre0002");
}
