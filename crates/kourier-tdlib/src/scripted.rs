//! A scripted TDLib for tests (feature `scripted`): answers requests by rules keyed on their
//! `@type`, records every call, and models what TDLib does that the protocol relies on.
//!
//! Modelled after TDLib 1.8.68, read again at 1.8.69 (BSL-1.0, read for behaviour: `td/telegram/Td.cpp:189-247,
//! 300-356, 1043-1069, 940-963`, the line cites are a94b83d's):
//!
//! - A request for a client id the engine never handed out is answered `400 "Invalid TDLib
//!   instance specified"` (`Client.cpp:493-496`).
//! - SYNCHRONOUS requests (`getOption` of `version`, `commit_hash` or `kourier_patch_level`, the log calls, the text
//!   helpers: [`SYNCHRONOUS`]) sent with `send` are answered at once in EVERY state, the parameters
//!   being applied and a closing client included (`Td.cpp:292-295`), the way `execute` answers.
//! - A client starts in `authorizationStateWaitTdlibParameters`. In that state a
//!   PRE-INITIALIZATION request (the test calls, `getCurrentState`, `setAlarm`, `testProxy`) is
//!   answered at once; a PRE-AUTHENTICATION request (`addProxy`, `getOption`, `setNetworkType`,
//!   the proxy and language-pack calls, ...) is PARKED and not answered; any other request is
//!   answered `400 "Initialization parameters are needed: call setTdlibParameters first"`.
//!   `getAuthorizationState` and `close` are answered in every state (a closing client answers
//!   `getAuthorizationState` with `authorizationStateClosing` or `Closed`, and `close` with `ok`).
//! - `setTdlibParameters` answers `ok`, then the parked requests in TDLib's order (first the
//!   network and proxy ones: `setNetworkType`, `addProxy`, `editProxy`, `enableProxy`,
//!   `disableProxy`, `removeProxy`, `getProxies`; then the rest), each in arrival order, then the
//!   requests that arrived while the parameters were applied, then the next authorization state of
//!   the script. (The real library sends the parameters' `ok` before the parked proxy answer
//!   although it applied the proxy first.) Two switches model what TDLib does around it:
//!   [`Script::refuse_first_parameters`] (the first `setTdlibParameters` of a client is answered
//!   400 and the client stays in WaitTdlibParameters WITH its parked requests, `Td.cpp:327-331`; a
//!   second one is taken) and [`Script::hold_parameters`] (the parameters are being applied until
//!   the test calls [`ScriptedTd::finish_parameters`]: every request meanwhile, `getAuthorizationState`
//!   and `close` included, is parked and replayed after them, `Td.cpp:301-304, 822-836`). Valid
//!   parameters are not checked (a bare `setTdlibParameters` is taken).
//! - A client that has not reached `authorizationStateReady` answers every request outside the
//!   pre-initialization, pre-authentication and authentication lists `401 "Unauthorized"`
//!   (`Td.cpp:365-369`). A script reaches Ready by an update (`after_parameters`, or a rule's
//!   reply).
//! - Proxies: `addProxy` of a proxy that differs from every stored one gets a NEW id, as TDLib's
//!   binlog does; `enable: true` disables the others; `getProxies`, `removeProxy`, `enableProxy`,
//!   `disableProxy` act on that list. The list outlives a client of the same engine.
//! - `close`: `ok`, then `authorizationStateClosing`, then the script's `close_updates`, then (when
//!   the script says so, TDLib's quirk) ONE `None` from `receive`, then `authorizationStateClosed`;
//!   with `hang_on_close` the client never closes.
//! - Two threads inside `receive` at once abort the process, like TDLib.
//! - TG-3 C10a: with [`Script::processed_log`] every pushed update is appended to a synced file
//!   before `receive` hands it out (TDLib saves its pts before the client sees the update), so a
//!   kill test knows after a SIGKILL what "TDLib" processed.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::td::{Engine, FatalCallback};

/// TDLib's pre-initialization requests (`Td::is_preinitialization_request`, in its order; 14 at
/// 1.8.69, whose last two are the web-proxy carrier calls the helper refuses before TDLib sees
/// them: `serve.rs` `PROXY_REQUESTS`).
pub const PRE_INITIALIZATION: [&str; 14] = [
    "getCurrentState",
    "setAlarm",
    "testUseUpdate",
    "testCallEmpty",
    "testSquareInt",
    "testCallString",
    "testCallBytes",
    "testCallVectorInt",
    "testCallVectorIntObject",
    "testCallVectorString",
    "testCallVectorStringObject",
    "testProxy",
    "processWebProxyCarrierMessage",
    "reportWebProxyCarrierError",
];

/// TDLib's pre-authentication requests (`Td::is_preauthentication_request`).
pub const PRE_AUTHENTICATION: [&str; 36] = [
    "getInternalLink",
    "getInternalLinkType",
    "getLocalizationTargetInfo",
    "getLanguagePackInfo",
    "getLanguagePackStrings",
    "synchronizeLanguagePack",
    "addCustomServerLanguagePack",
    "setCustomLanguagePack",
    "editCustomLanguagePackInfo",
    "setCustomLanguagePackString",
    "deleteLanguagePack",
    "processPushNotification",
    "getOption",
    "setOption",
    "getStorageStatistics",
    "getStorageStatisticsFast",
    "getDatabaseStatistics",
    "setNetworkType",
    "getNetworkStatistics",
    "addNetworkStatistics",
    "resetNetworkStatistics",
    "setApplicationVerificationToken",
    "getCountries",
    "getCountryCode",
    "getPhoneNumberInfo",
    "getDeepLinkInfo",
    "getApplicationConfig",
    "saveApplicationLogEvent",
    "addProxy",
    "editProxy",
    "enableProxy",
    "disableProxy",
    "removeProxy",
    "getProxies",
    "pingProxy",
    "testNetwork",
];

/// TDLib's synchronous requests (`SynchronousRequests::is_synchronous_request`; `getOption` only
/// for `version`, `commit_hash` and, with Kourier's patch 0002, `kourier_patch_level`:
/// `OptionManager::is_synchronous_option`): answered at once, in every state, by `td_execute` and
/// by `td_send` alike.
pub const SYNCHRONOUS: [&str; 27] = [
    "searchQuote",
    "getTextEntities",
    "parseTextEntities",
    "parseMarkdown",
    "getMarkdownText",
    "searchStringsByPrefix",
    "checkQuickReplyShortcutName",
    "getCountryFlagEmoji",
    "getFileMimeType",
    "getFileExtension",
    "cleanFileName",
    "getLanguagePackString",
    "getPhoneNumberInfoSync",
    "getChatFolderDefaultIconName",
    "getJsonValue",
    "getJsonString",
    "getThemeParametersJsonString",
    "getPushReceiverId",
    "setLogStream",
    "getLogStream",
    "setLogVerbosityLevel",
    "getLogVerbosityLevel",
    "getLogTags",
    "setLogTagVerbosityLevel",
    "getLogTagVerbosityLevel",
    "addLogMessage",
    "testReturnError",
];

/// TDLib's authentication requests (`Td::is_authentication_request`, all 26 of them): a client
/// that is not authorized still takes these.
pub const AUTHENTICATION: [&str; 26] = [
    "setTdlibParameters",
    "getAuthorizationState",
    "setAuthenticationPhoneNumber",
    "checkAuthenticationPremiumPurchase",
    "setAuthenticationPremiumPurchaseTransaction",
    "sendAuthenticationFirebaseSms",
    "reportAuthenticationCodeMissing",
    "setAuthenticationEmailAddress",
    "resendAuthenticationCode",
    "checkAuthenticationEmailCode",
    "checkAuthenticationCode",
    "registerUser",
    "requestQrCodeAuthentication",
    "getAuthenticationPasskeyParameters",
    "checkAuthenticationPasskey",
    "checkAuthenticationWebToken",
    "resetAuthenticationEmailAddress",
    "checkAuthenticationPassword",
    "requestAuthenticationPasswordRecovery",
    "checkAuthenticationPasswordRecoveryCode",
    "recoverAuthenticationPassword",
    "deleteAccount",
    "logOut",
    "close",
    "destroy",
    "checkAuthenticationBotToken",
];

/// The parked requests TDLib replays first, right after creating its connection creator.
const NETWORK_FIRST: [&str; 7] = [
    "setNetworkType",
    "addProxy",
    "editProxy",
    "enableProxy",
    "disableProxy",
    "removeProxy",
    "getProxies",
];

/// What a test's script says.
#[derive(Clone, Debug)]
pub struct Script {
    /// Answers by `@type`, used once the parameters are set (and for parked requests).
    pub rules: Vec<Rule>,
    /// `getOption("version")` through `execute`.
    pub version: String,
    /// `getOption("commit_hash")` through `execute`.
    pub commit: String,
    /// `getOption("kourier_patch_level")` through `execute` (the run-time marker of Kourier's
    /// patch 0002, pre3 K5); empty = a library without the patch, which answers the error every
    /// other asynchronous option gets there.
    pub patch_level: String,
    /// TDLib returns one NULL from `td_receive` while a client closes (the spike's trap).
    pub null_before_closed: bool,
    /// A client that never reaches `authorizationStateClosed`.
    pub hang_on_close: bool,
    /// Updates a closing client still hands out after `authorizationStateClosing` and before
    /// `authorizationStateClosed` (TG-3 Q-D3: what TDLib had not delivered yet when it was asked to
    /// close).
    pub close_updates: Vec<Value>,
    /// A request of this `@type` makes TDLib report a fatal error and abort the process.
    pub abort_on: Option<String>,
    /// Lines a fake helper process writes to its stderr (layer (b), TG-1b).
    pub stderr_lines: Vec<String>,
    /// The authorization state after the parameters (`authorizationStateWaitPhoneNumber`).
    pub after_parameters: String,
    /// `execute` answers these `@type`s with `{"@type":"ok"}` (the log calls by default).
    pub execute_ok: Vec<String>,
    /// The first `setTdlibParameters` of each client is refused with 400 (a bad api_id): the client
    /// stays in WaitTdlibParameters with the requests it parked (`Td.cpp:327-331`).
    pub refuse_first_parameters: bool,
    /// `setTdlibParameters` is being applied until [`ScriptedTd::finish_parameters`]: every request
    /// meanwhile is parked (`Td.cpp:301-304`).
    pub hold_parameters: bool,
    /// TDLib answers an `addProxy` with a 500 (its binlog failed) and stores nothing.
    pub fail_add_proxy: bool,
    /// TDLib answers a `setOption store_all_files_in_files_directory` with a 500 and stores nothing
    /// (pre3 K2: the helper must close that client).
    pub fail_files_option: bool,
    /// Every `setTdlibParameters` is answered with this error (code, message) and the client stays
    /// in WaitTdlibParameters (a database that cannot be opened: TDLib's "Can't lock file ...").
    pub parameters_error: Option<(i64, String)>,
    /// A request of this `@type` wedges TDLib: it is not answered, and `receive` never returns
    /// again (a deadlock inside the library: the helper's receiver and its heartbeat stop; M4).
    pub wedge_on: Option<String>,
    /// TG-3 C10a (critic M8): TDLib's "processed" log, a file that outlives a SIGKILL of the
    /// process. Every update the test PUSHES ([`ScriptedTd::push_update`], the script's stream)
    /// is appended to it as one JSON line and synced to disk inside `receive`, BEFORE `receive`
    /// hands the line out: what TDLib saved (its pts) before the client could see the update. A
    /// rule's reply is not logged. `None`: no log. [`processed`] reads it back.
    pub processed_log: Option<std::path::PathBuf>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            // The pinned TDLib (`third_party/tdlib/tdlib.pin`, `kourier_telegram::PIN`). This crate cannot name
            // the pin; `kourier-telegram`'s `pin::tests::the_scripted_default_is_the_pin` compares the two, and
            // this crate's own tests read these two fields from `Script::default()`, so a bump edits only here.
            version: "1.8.69".to_owned(),
            commit: "0e88653dcbf4e44c73cb655a05b58cd749a91169".to_owned(),
            patch_level: "0002".to_owned(),
            null_before_closed: true,
            hang_on_close: false,
            close_updates: Vec::new(),
            abort_on: None,
            stderr_lines: Vec::new(),
            after_parameters: "authorizationStateWaitPhoneNumber".to_owned(),
            execute_ok: vec!["setLogStream".to_owned(), "setLogVerbosityLevel".to_owned()],
            refuse_first_parameters: false,
            hold_parameters: false,
            fail_add_proxy: false,
            fail_files_option: false,
            parameters_error: None,
            wedge_on: None,
            processed_log: None,
        }
    }
}

/// One rule: a request of type `on` is answered by `reply`. A reply that is not an update gets
/// the request's `@extra`; every reply gets its `@client_id`.
///
/// TG-2 (stateful rules): a request is answered by the FIRST rule that matches it. A rule matches
/// when its `on` is the request's `@type`, its `when` (a JSON pointer and a value) is `None` or the
/// request's value at that pointer equals it, and its `nth` is `None` or equals the rule's own
/// count. The count of a rule is how many requests matched its `on` and `when` so far, this one
/// included: every rule whose `on` and `when` match a request counts it, whichever rule answers.
/// So `nth: Some(2)` answers only the second such request (1-based). A `deferred` rule's reply is
/// held until [`ScriptedTd::release`] (a request in flight while the test does something else).
/// `execute` (and a synchronous `send`) still takes the first rule by `on` alone.
///
/// TG-3 C1: `also` adds more pointer-value pairs that must ALL match with `when` (a recorded
/// `getChatHistory` is keyed on its `chat_id` and its `from_message_id`); [`Rule::when_all`].
#[derive(Clone, Debug)]
pub struct Rule {
    pub on: String,
    pub reply: Vec<Value>,
    /// A JSON pointer into the request (`/code`) and the value it must have.
    pub when: Option<(String, Value)>,
    /// Further JSON pointers and values the request must have, besides `when` (TG-3 C1).
    pub also: Vec<(String, Value)>,
    /// Only the n-th matching request (1-based).
    pub nth: Option<usize>,
    /// The reply is queued only when the test calls [`ScriptedTd::release`].
    pub deferred: bool,
}

impl Rule {
    pub fn new(on: &str, reply: Vec<Value>) -> Self {
        Self {
            on: on.to_owned(),
            reply,
            when: None,
            also: Vec::new(),
            nth: None,
            deferred: false,
        }
    }

    /// A rule for the requests of type `on` whose value at `pointer` is `value`.
    pub fn when(on: &str, pointer: &str, value: Value, reply: Vec<Value>) -> Self {
        Self {
            when: Some((pointer.to_owned(), value)),
            ..Self::new(on, reply)
        }
    }

    /// A rule for the requests of type `on` that have EVERY `(pointer, value)` of `all` (TG-3 C1:
    /// a recorded answer keyed on several arguments). With an empty `all` it is [`Rule::new`].
    pub fn when_all(on: &str, all: &[(&str, Value)], reply: Vec<Value>) -> Self {
        let mut pairs = all.iter().map(|(p, v)| ((*p).to_owned(), v.clone()));
        Self {
            when: pairs.next(),
            also: pairs.collect(),
            ..Self::new(on, reply)
        }
    }

    /// A rule for the `n`-th request of type `on` (1-based).
    pub fn nth(on: &str, n: usize, reply: Vec<Value>) -> Self {
        Self {
            nth: Some(n),
            ..Self::new(on, reply)
        }
    }

    /// A rule whose reply waits for [`ScriptedTd::release`].
    pub fn deferred(on: &str, reply: Vec<Value>) -> Self {
        Self::new(on, reply).deferring()
    }

    /// This rule, its reply held until [`ScriptedTd::release`] (combines with `when` and `nth`).
    pub fn deferring(mut self) -> Self {
        self.deferred = true;
        self
    }

    /// Whether the rule's `on`, `when` and `also` match a request of type `ty`.
    fn matches(&self, ty: &str, req: &Value) -> bool {
        self.on == ty
            && self
                .when
                .iter()
                .chain(&self.also)
                .all(|(pointer, value)| req.pointer(pointer) == Some(value))
    }
}

impl Script {
    /// Rules from JSON lines `{"on": "<@type>", "reply": [<object>, ...]}`, each with the optional
    /// keys `"when": ["<JSON pointer>", <value>]`, `"nth": <n>` (1-based) and `"deferred": true`;
    /// other lines (blank, `#` comments) are skipped. Panics on a malformed line: a fixture is
    /// test data.
    pub fn from_fixture(jsonl: &str) -> Script {
        let mut script = Script::default();
        for (n, line) in jsonl.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("fixture line {} is not JSON", n + 1));
            let on = v["on"]
                .as_str()
                .unwrap_or_else(|| panic!("fixture line {} has no `on`", n + 1));
            let reply = v["reply"]
                .as_array()
                .unwrap_or_else(|| panic!("fixture line {} has no `reply` list", n + 1))
                .clone();
            let mut rule = Rule::new(on, reply);
            if let Some(w) = v.get("when") {
                let pair = w
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .unwrap_or_else(|| panic!("fixture line {}: `when` is no pair", n + 1));
                let pointer = pair[0]
                    .as_str()
                    .unwrap_or_else(|| panic!("fixture line {}: `when` names no pointer", n + 1));
                rule.when = Some((pointer.to_owned(), pair[1].clone()));
            }
            if let Some(nth) = v.get("nth") {
                let nth = nth
                    .as_u64()
                    .filter(|&k| k >= 1)
                    .unwrap_or_else(|| panic!("fixture line {}: `nth` is no count", n + 1));
                rule.nth = Some(nth as usize);
            }
            if let Some(d) = v.get("deferred") {
                rule.deferred = d
                    .as_bool()
                    .unwrap_or_else(|| panic!("fixture line {}: `deferred` is no bool", n + 1));
            }
            script.rules.push(rule);
        }
        script
    }
}

/// A call the scripted TDLib saw, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `execute` of a request of this `@type`.
    Execute(String),
    Create,
    Send {
        client: i32,
        ty: String,
    },
    SetFatalCallback,
    /// A thread that called `receive` (recorded when it differs from the last one that did).
    ReceiveThread(ThreadId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientState {
    WaitParameters,
    /// `setTdlibParameters` is being applied (`hold_parameters`).
    Applying,
    Ready,
    Closing,
    Closed,
}

struct Client {
    state: ClientState,
    started: bool,
    /// Pre-authentication requests waiting for the parameters.
    parked: Vec<Value>,
    /// The first `setTdlibParameters` was refused (`refuse_first_parameters`).
    refused_first: bool,
    /// The `@extra` of the `setTdlibParameters` being applied.
    applying_extra: Option<Value>,
    /// Requests that arrived while the parameters were applied.
    pending: Vec<(String, Value)>,
    /// `authorizationStateReady` was told.
    authorized: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredProxy {
    id: i64,
    server: String,
    port: i64,
    ty: Value,
    enabled: bool,
}

enum Item {
    Line(String),
    /// A pushed update that goes to [`Script::processed_log`] before it is handed out.
    Processed(String),
    /// TDLib's NULL while a client closes.
    Null,
}

struct State {
    script: Script,
    calls: Vec<Call>,
    queue: VecDeque<Item>,
    clients: BTreeMap<i32, Client>,
    next_client: i32,
    proxies: Vec<StoredProxy>,
    next_proxy: i64,
    /// The stored `store_all_files_in_files_directory` (K2); `None` = never set (TDLib's default,
    /// false). One value for the engine, as TDLib keeps it in the database.
    files_option: Option<bool>,
    last_receiver: Option<ThreadId>,
    /// How many times `receive` was called.
    receive_calls: usize,
    fatal: Option<Arc<FatalCallback>>,
    /// `Script::wedge_on` was sent: `receive` blocks for good.
    wedged: bool,
    /// Every `send` with its client, as received (`Value::Null` for a line that is no JSON).
    requests: Vec<(i32, Value)>,
    /// How many requests matched each rule's `on` and `when` (by rule index).
    rule_counts: Vec<usize>,
    /// Replies of deferred rules, waiting for [`ScriptedTd::release`]: client, replies, `@extra`.
    held: VecDeque<(i32, Vec<Value>, Option<Value>)>,
}

/// The scripted TDLib. Clones share one engine (a test keeps one to read [`ScriptedTd::calls`]).
#[derive(Clone)]
pub struct ScriptedTd {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    receiving: AtomicBool,
}

impl ScriptedTd {
    pub fn new(script: Script) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    script,
                    calls: Vec::new(),
                    queue: VecDeque::new(),
                    clients: BTreeMap::new(),
                    next_client: 1,
                    proxies: Vec::new(),
                    next_proxy: 1,
                    files_option: None,
                    last_receiver: None,
                    receive_calls: 0,
                    fatal: None,
                    wedged: false,
                    requests: Vec::new(),
                    rule_counts: Vec::new(),
                    held: VecDeque::new(),
                }),
                wake: Condvar::new(),
                receiving: AtomicBool::new(false),
            }),
        }
    }

    /// How many times `receive` was called so far (a helper that polls shows here).
    pub fn receive_calls(&self) -> usize {
        self.lock().receive_calls
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The `@type`s of every `Send`, in order (for asserting an order of requests).
    pub fn sent_types(&self) -> Vec<String> {
        self.lock()
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::Send { ty, .. } => Some(ty.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every request sent so far with its client, in order, the JSON as received (TG-2: a test
    /// asserts what the app sent, e.g. ONE `setAuthenticationPhoneNumber`). Test data only: a
    /// test never prints it.
    pub fn requests(&self) -> Vec<(i32, Value)> {
        self.lock().requests.clone()
    }

    /// Queues the replies of every deferred rule that answered so far, in the order the requests
    /// came. How many requests were released.
    pub fn release(&self) -> usize {
        let mut st = self.lock();
        let held = std::mem::take(&mut st.held);
        let n = held.len();
        for (client, replies, extra) in held {
            for r in replies {
                st.push(client, r, extra.as_ref());
            }
        }
        drop(st);
        self.shared.wake.notify_all();
        n
    }

    /// How many deferred replies wait for [`ScriptedTd::release`].
    pub fn held(&self) -> usize {
        self.lock().held.len()
    }

    /// The value of `store_all_files_in_files_directory` TDLib holds now (pre3 K2): `None` where
    /// nobody set it (`optionValueEmpty`, which TDLib reads as false).
    pub fn files_option(&self) -> Option<bool> {
        self.lock().files_option
    }

    /// The stored proxies as `(id, server, port, enabled)`.
    pub fn proxies(&self) -> Vec<(i64, String, i64, bool)> {
        self.lock()
            .proxies
            .iter()
            .map(|p| (p.id, p.server.clone(), p.port, p.enabled))
            .collect()
    }

    /// Whether a thread waits inside `receive` now.
    pub fn is_receiving(&self) -> bool {
        self.shared.receiving.load(Ordering::SeqCst)
    }

    /// The script's stderr lines (a fake helper process writes them).
    pub fn stderr_lines(&self) -> Vec<String> {
        self.lock().script.stderr_lines.clone()
    }

    /// Ends the application of `client`'s parameters (`Script::hold_parameters`): the parameters'
    /// `ok`, the parked answers, the requests that came meanwhile and the next state follow.
    /// Whether the client was applying them.
    pub fn finish_parameters(&self, client: i32) -> bool {
        let mut st = self.lock();
        let extra = match st.clients.get_mut(&client) {
            Some(c) if c.state == ClientState::Applying => c.applying_extra.take(),
            _ => return false,
        };
        st.apply_parameters(client, extra.filter(|e| !e.is_null()));
        drop(st);
        self.shared.wake.notify_all();
        true
    }

    /// Puts an update into the queue as if TDLib had sent it. A pushed
    /// `authorizationStateReady` authorizes the client it names by `@client_id` (without one:
    /// every client), as TDLib is authorized once it says Ready, whoever caused it (TG-2 C9: a QR
    /// login's Ready comes from the other device, not from an answered request).
    pub fn push_update(&self, update: Value) {
        let mut st = self.lock();
        let ready = update["@type"] == "updateAuthorizationState"
            && update["authorization_state"]["@type"] == "authorizationStateReady";
        if ready {
            let named = update["@client_id"]
                .as_i64()
                .and_then(|c| i32::try_from(c).ok());
            for (id, c) in st.clients.iter_mut() {
                if named.is_none_or(|n| n == *id) {
                    c.authorized = true;
                }
            }
        }
        let line = update.to_string();
        let item = if st.script.processed_log.is_some() {
            Item::Processed(line)
        } else {
            Item::Line(line)
        };
        st.queue.push_back(item);
        drop(st);
        self.shared.wake.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A test thread that panicked while holding it leaves the data usable.
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Appends `line` to the processed log and syncs it (TDLib's pts save before the emit). A log
/// that cannot be written aborts: a test must not go on with a log that lies.
fn log_processed(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_data()
        });
    if written.is_err() {
        abort_quietly();
    }
}

/// The updates a [`Script::processed_log`] holds, in order (a torn last line, from a kill inside
/// the write, is dropped: TDLib had not saved it). A missing file holds none.
pub fn processed(path: &std::path::Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn error(code: i64, message: &str) -> Value {
    json!({"@type": "error", "code": code, "message": message})
}

fn auth_state(ty: &str) -> Value {
    json!({
        "@type": "updateAuthorizationState",
        "authorization_state": {"@type": ty},
    })
}

impl State {
    fn push(&mut self, client: i32, mut v: Value, extra: Option<&Value>) {
        if let Some(obj) = v.as_object_mut() {
            let is_update = obj
                .get("@type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("update"));
            if !is_update && let Some(e) = extra {
                obj.insert("@extra".to_owned(), e.clone());
            }
            obj.insert("@client_id".to_owned(), json!(client));
        }
        let ready = v["@type"] == "updateAuthorizationState"
            && v["authorization_state"]["@type"] == "authorizationStateReady";
        if ready && let Some(c) = self.clients.get_mut(&client) {
            c.authorized = true;
        }
        self.queue.push_back(Item::Line(v.to_string()));
    }

    fn answer_by_rules(&mut self, client: i32, ty: &str, req: &Value) {
        let extra = req.get("@extra");
        if let Some(reply) = self.proxy_answer(ty, req) {
            self.push(client, reply, extra);
            return;
        }
        if let Some(reply) = self.files_option_answer(ty, req) {
            self.push(client, reply, extra);
            return;
        }
        // Every rule whose `on` and `when` match counts the request; the first whose `nth` (if
        // any) equals its count answers.
        let n_rules = self.script.rules.len();
        if self.rule_counts.len() < n_rules {
            self.rule_counts.resize(n_rules, 0);
        }
        let mut chosen = None;
        for (i, rule) in self.script.rules.iter().enumerate() {
            if !rule.matches(ty, req) {
                continue;
            }
            self.rule_counts[i] += 1;
            if chosen.is_none() && rule.nth.is_none_or(|n| n == self.rule_counts[i]) {
                chosen = Some((rule.reply.clone(), rule.deferred));
            }
        }
        match chosen {
            Some((replies, true)) => {
                self.held.push_back((client, replies, extra.cloned()));
            }
            Some((replies, false)) => {
                for r in replies {
                    self.push(client, r, extra);
                }
            }
            None => self.push(
                client,
                error(400, "The script has no rule for this request"),
                extra,
            ),
        }
    }

    /// `setOption` / `getOption` of `store_all_files_in_files_directory` (K2): a boolean option,
    /// parked until the parameters like every pre-authentication request and answered in order.
    fn files_option_answer(&mut self, ty: &str, req: &Value) -> Option<Value> {
        if req["name"].as_str() != Some("store_all_files_in_files_directory") {
            return None;
        }
        match ty {
            "setOption" => {
                if self.script.fail_files_option {
                    return Some(error(500, "The option could not be saved"));
                }
                match req["value"]["@type"].as_str() {
                    Some("optionValueBoolean") => {
                        self.files_option = req["value"]["value"].as_bool();
                        Some(json!({"@type": "ok"}))
                    }
                    Some("optionValueEmpty") => {
                        self.files_option = None;
                        Some(json!({"@type": "ok"}))
                    }
                    _ => Some(error(400, "Can't set option as another type")),
                }
            }
            // `get_option_value_object`: a never-set option is `optionValueEmpty`.
            "getOption" => Some(match self.files_option {
                Some(v) => json!({"@type": "optionValueBoolean", "value": v}),
                None => json!({"@type": "optionValueEmpty"}),
            }),
            _ => None,
        }
    }

    fn proxy_json(p: &StoredProxy) -> Value {
        json!({
            "@type": "addedProxy",
            "id": p.id,
            "last_used_date": 0,
            "is_enabled": p.enabled,
            "comment": "",
            "proxy": {"@type": "proxy", "server": p.server, "port": p.port, "type": p.ty},
        })
    }

    /// TDLib's proxy list (ConnectionCreator): a different proxy gets a new id.
    fn proxy_answer(&mut self, ty: &str, req: &Value) -> Option<Value> {
        let ok = json!({"@type": "ok"});
        match ty {
            "addProxy" => {
                if self.script.fail_add_proxy {
                    return Some(error(500, "The proxy list could not be saved"));
                }
                let proxy = &req["proxy"];
                let server = proxy["server"].as_str().unwrap_or_default().to_owned();
                let port = proxy["port"].as_i64().unwrap_or_default();
                let ptype = proxy["type"].clone();
                // `Proxy::create_proxy` (`Proxy.cpp:15-34`) in its order. TDLib takes a SOCKS5 login
                // of 128 bytes or more here and fails every connection with it later
                // (`Socks5.cpp:59-63`); the script refuses it at once, the side a test can lean on.
                let too_long = |field: &str| ptype[field].as_str().is_some_and(|f| f.len() >= 128);
                let refusal = if ptype.is_null() {
                    Some("Proxy type must be non-empty")
                } else if server.is_empty() {
                    Some("Server name must be non-empty")
                } else if server.len() > 255 {
                    Some("Server name is too long")
                } else if port <= 0 || port > 65535 {
                    Some("Wrong port number")
                } else if too_long("username") || too_long("password") {
                    Some("The proxy login is too long")
                } else {
                    None
                };
                if let Some(message) = refusal {
                    return Some(error(400, message));
                }
                let enable = req["enable"].as_bool().unwrap_or(false);
                let pos = self
                    .proxies
                    .iter()
                    .position(|p| p.server == server && p.port == port && p.ty == ptype);
                let pos = match pos {
                    Some(i) => i,
                    None => {
                        let id = self.next_proxy;
                        self.next_proxy += 1;
                        self.proxies.push(StoredProxy {
                            id,
                            server,
                            port,
                            ty: ptype,
                            enabled: false,
                        });
                        self.proxies.len() - 1
                    }
                };
                if enable {
                    for (i, p) in self.proxies.iter_mut().enumerate() {
                        p.enabled = i == pos;
                    }
                }
                Some(Self::proxy_json(&self.proxies[pos]))
            }
            "getProxies" => Some(json!({
                "@type": "addedProxies",
                "proxies": self.proxies.iter().map(Self::proxy_json).collect::<Vec<_>>(),
            })),
            "removeProxy" | "enableProxy" | "disableProxy" => {
                let id = req["proxy_id"].as_i64();
                match ty {
                    "removeProxy" => {
                        let before = self.proxies.len();
                        self.proxies.retain(|p| Some(p.id) != id);
                        if self.proxies.len() == before {
                            return Some(error(400, "Unknown proxy identifier"));
                        }
                    }
                    "enableProxy" => {
                        if !self.proxies.iter().any(|p| Some(p.id) == id) {
                            return Some(error(400, "Unknown proxy identifier"));
                        }
                        for p in &mut self.proxies {
                            p.enabled = Some(p.id) == id;
                        }
                    }
                    _ => {
                        for p in &mut self.proxies {
                            p.enabled = false;
                        }
                    }
                }
                Some(ok)
            }
            _ => None,
        }
    }

    fn close(&mut self, client: i32) {
        self.push(client, auth_state("authorizationStateClosing"), None);
        if let Some(c) = self.clients.get_mut(&client) {
            c.state = ClientState::Closing;
        }
        for update in self.script.close_updates.clone() {
            self.push(client, update, None);
        }
        if self.script.null_before_closed {
            self.queue.push_back(Item::Null);
        }
        if !self.script.hang_on_close {
            self.push(client, auth_state("authorizationStateClosed"), None);
            if let Some(c) = self.clients.get_mut(&client) {
                c.state = ClientState::Closed;
            }
        }
    }

    /// The answer of `td_execute` (and of a synchronous `td_send`), before an `@extra` is added.
    fn synchronous_answer(&self, ty: &str, req: &Value) -> Value {
        if self.script.execute_ok.iter().any(|t| t == ty) {
            json!({"@type": "ok"})
        } else if ty == "getOption" {
            match req["name"].as_str() {
                Some("version") => {
                    json!({"@type": "optionValueString", "value": self.script.version})
                }
                Some("commit_hash") => {
                    json!({"@type": "optionValueString", "value": self.script.commit})
                }
                Some("kourier_patch_level") if !self.script.patch_level.is_empty() => {
                    json!({"@type": "optionValueString", "value": self.script.patch_level})
                }
                _ => error(400, "The option can't be get synchronously"),
            }
        } else if ty == "testReturnError" {
            // `SynchronousRequests.cpp:389-395`: the error it was given, else 404.
            match req.get("error") {
                Some(e) if e.is_object() => e.clone(),
                _ => error(404, "Not Found"),
            }
        } else if let Some(r) = self.script.rules.iter().find(|r| r.on == ty) {
            r.reply.first().cloned().unwrap_or(Value::Null)
        } else {
            error(400, "The method can't be executed synchronously")
        }
    }

    fn send(&mut self, client: i32, req: Value) {
        let ty = req["@type"].as_str().unwrap_or_default().to_owned();
        self.calls.push(Call::Send {
            client,
            ty: ty.clone(),
        });
        let extra = req.get("@extra").cloned();
        if !self.clients.contains_key(&client) {
            // The client manager (`Client.cpp:493-496`) answers an id it never handed out.
            self.push(
                client,
                error(400, "Invalid TDLib instance specified"),
                extra.as_ref(),
            );
            return;
        }
        let started = std::mem::replace(
            &mut self.clients.get_mut(&client).expect("client").started,
            true,
        );
        if !started {
            // TDLib creates the client at its first request and tells its first state.
            self.push(
                client,
                auth_state("authorizationStateWaitTdlibParameters"),
                None,
            );
        }
        // Synchronous requests are answered before the state is looked at (`Td.cpp:292-295`).
        let synchronous = SYNCHRONOUS.contains(&ty.as_str())
            || (ty == "getOption"
                && match req["name"].as_str() {
                    Some("version" | "commit_hash") => true,
                    Some("kourier_patch_level") => !self.script.patch_level.is_empty(),
                    _ => false,
                });
        if synchronous {
            let answer = self.synchronous_answer(&ty, &req);
            self.push(client, answer, extra.as_ref());
            return;
        }
        self.run_request(client, ty, req);
    }

    /// `Td::run_request`: what a client does with a request, by its state.
    fn run_request(&mut self, client: i32, ty: String, req: Value) {
        let extra = req.get("@extra").cloned();
        let state = self.clients[&client].state;
        match (state, ty.as_str()) {
            (ClientState::Applying, _) => {
                // The parameters are being applied: everything waits (`Td.cpp:301-304`).
                self.clients
                    .get_mut(&client)
                    .expect("client")
                    .pending
                    .push((ty.clone(), req));
            }
            (ClientState::Closing | ClientState::Closed, "getAuthorizationState") => {
                let now = if state == ClientState::Closed {
                    "authorizationStateClosed"
                } else {
                    "authorizationStateClosing"
                };
                self.push(client, json!({"@type": now}), extra.as_ref());
            }
            (ClientState::Closing | ClientState::Closed, "close") => {
                self.push(client, json!({"@type": "ok"}), extra.as_ref());
            }
            (ClientState::Closing | ClientState::Closed, _) => {
                self.push(client, error(500, "Request aborted"), extra.as_ref());
            }
            (_, "getAuthorizationState") => {
                let now = match state {
                    ClientState::WaitParameters => {
                        "authorizationStateWaitTdlibParameters".to_owned()
                    }
                    _ => self.script.after_parameters.clone(),
                };
                self.push(client, json!({"@type": now}), extra.as_ref());
            }
            (_, "close") => {
                self.push(client, json!({"@type": "ok"}), extra.as_ref());
                self.close(client);
            }
            (ClientState::WaitParameters, "setTdlibParameters") => {
                let first = !self.clients[&client].refused_first;
                if let Some((code, message)) = self.script.parameters_error.clone() {
                    self.push(client, error(code, &message), extra.as_ref());
                } else if self.script.refuse_first_parameters && first {
                    // `Td::get_parameters` fails (`Td.cpp:1441-1456`): the state and the parked
                    // requests stay.
                    self.clients.get_mut(&client).expect("client").refused_first = true;
                    self.push(
                        client,
                        error(400, "Valid api_id must be provided"),
                        extra.as_ref(),
                    );
                } else if self.script.hold_parameters {
                    let c = self.clients.get_mut(&client).expect("client");
                    c.state = ClientState::Applying;
                    c.applying_extra = Some(extra.unwrap_or(Value::Null));
                } else {
                    self.apply_parameters(client, extra);
                }
            }
            (ClientState::WaitParameters, t) if PRE_INITIALIZATION.contains(&t) => {
                self.answer_by_rules(client, t, &req);
            }
            (ClientState::WaitParameters, t) if PRE_AUTHENTICATION.contains(&t) => {
                self.clients
                    .get_mut(&client)
                    .expect("client")
                    .parked
                    .push(req);
            }
            (ClientState::WaitParameters, _) => {
                self.push(
                    client,
                    error(
                        400,
                        "Initialization parameters are needed: call setTdlibParameters first",
                    ),
                    extra.as_ref(),
                );
            }
            (ClientState::Ready, t) => {
                let t = t.to_owned();
                let known = PRE_AUTHENTICATION.contains(&t.as_str())
                    || PRE_INITIALIZATION.contains(&t.as_str())
                    || AUTHENTICATION.contains(&t.as_str());
                if !self.clients[&client].authorized && !known {
                    self.push(client, error(401, "Unauthorized"), extra.as_ref());
                } else {
                    self.answer_by_rules(client, &t, &req);
                }
            }
        }
    }

    /// The parameters are in: `ok`, the parked requests in TDLib's order, the requests that came
    /// while they were applied, then the script's next authorization state.
    fn apply_parameters(&mut self, client: i32, extra: Option<Value>) {
        self.push(client, json!({"@type": "ok"}), extra.as_ref());
        let parked = std::mem::take(&mut self.clients.get_mut(&client).expect("client").parked);
        let (first, rest): (Vec<Value>, Vec<Value>) = parked
            .into_iter()
            .partition(|r| NETWORK_FIRST.contains(&r["@type"].as_str().unwrap_or_default()));
        let c = self.clients.get_mut(&client).expect("client");
        c.state = ClientState::Ready;
        let pending = std::mem::take(&mut c.pending);
        for r in first.into_iter().chain(rest) {
            let t = r["@type"].as_str().unwrap_or_default().to_owned();
            self.answer_by_rules(client, &t, &r);
        }
        let next = self.script.after_parameters.clone();
        self.push(client, auth_state(&next), None);
        for (t, r) in pending {
            self.run_request(client, t, r);
        }
    }
}

impl Engine for ScriptedTd {
    fn create_client(&self) -> i32 {
        let mut st = self.lock();
        st.calls.push(Call::Create);
        let id = st.next_client;
        st.next_client += 1;
        st.clients.insert(
            id,
            Client {
                state: ClientState::WaitParameters,
                started: false,
                parked: Vec::new(),
                refused_first: false,
                applying_extra: None,
                pending: Vec::new(),
                authorized: false,
            },
        );
        id
    }

    fn send(&self, client: i32, json: &CStr) {
        let req: Value = match json
            .to_str()
            .ok()
            .and_then(|s| serde_json::from_str(s).ok())
        {
            Some(v) => v,
            None => {
                let mut st = self.lock();
                st.requests.push((client, Value::Null));
                st.calls.push(Call::Send {
                    client,
                    ty: String::new(),
                });
                st.push(client, error(400, "Failed to parse JSON object"), None);
                drop(st);
                self.shared.wake.notify_all();
                return;
            }
        };
        let ty = req["@type"].as_str().unwrap_or_default().to_owned();
        let fatal = {
            let mut st = self.lock();
            st.requests.push((client, req.clone()));
            if st.script.wedge_on.as_deref() == Some(ty.as_str()) {
                st.calls.push(Call::Send { client, ty });
                st.wedged = true;
                return;
            }
            if st.script.abort_on.as_deref() == Some(ty.as_str()) {
                st.calls.push(Call::Send { client, ty });
                st.fatal.clone()
            } else {
                st.send(client, req);
                drop(st);
                self.shared.wake.notify_all();
                return;
            }
        };
        // TDLib calls the fatal callback, then aborts; the callback cannot prevent it.
        if let Some(cb) = fatal {
            (**cb)();
        }
        abort_quietly();
    }

    fn execute(&self, json: &CStr) -> Option<String> {
        let req: Value = json
            .to_str()
            .ok()
            .and_then(|s| serde_json::from_str(s).ok())?;
        let ty = req["@type"].as_str().unwrap_or_default().to_owned();
        let mut st = self.lock();
        st.calls.push(Call::Execute(ty.clone()));
        let mut answer = st.synchronous_answer(&ty, &req);
        if let (Some(obj), Some(e)) = (answer.as_object_mut(), req.get("@extra")) {
            obj.insert("@extra".to_owned(), e.clone());
        }
        Some(answer.to_string())
    }

    fn receive(&self, timeout_s: f64) -> Option<String> {
        if self.shared.receiving.swap(true, Ordering::SeqCst) {
            // TDLib: "Td::receive must not be called from several threads at once" -> abort.
            abort_quietly();
        }
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.max(0.0));
        let mut st = self.lock();
        st.receive_calls += 1;
        let me = std::thread::current().id();
        if st.last_receiver != Some(me) {
            st.last_receiver = Some(me);
            st.calls.push(Call::ReceiveThread(me));
        }
        let out = loop {
            if st.wedged {
                // A wedged library never returns from `td_receive` (only a kill ends this).
                st = self.shared.wake.wait(st).unwrap_or_else(|p| p.into_inner());
                continue;
            }
            match st.queue.pop_front() {
                Some(Item::Line(l)) => break Some(l),
                Some(Item::Processed(l)) => {
                    if let Some(path) = &st.script.processed_log {
                        log_processed(path, &l);
                    }
                    break Some(l);
                }
                Some(Item::Null) => break None,
                None => {
                    let now = Instant::now();
                    if now >= deadline {
                        break None;
                    }
                    st = self
                        .shared
                        .wake
                        .wait_timeout(st, deadline - now)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        };
        drop(st);
        self.shared.receiving.store(false, Ordering::SeqCst);
        out
    }

    fn set_fatal_callback(&self, cb: FatalCallback) {
        let mut st = self.lock();
        st.calls.push(Call::SetFatalCallback);
        st.fatal = Some(Arc::new(cb));
    }
}

/// Aborts the process without leaving a core dump (decisions D3 H5: test children that abort on
/// purpose set the core limit first: `{1, 1}`, not 0, which a pipe `core_pattern` such as
/// systemd-coredump ignores, pre3 K1).
fn abort_quietly() -> ! {
    let no_core = libc::rlimit {
        rlim_cur: crate::harden::CORE_LIMIT,
        rlim_max: crate::harden::CORE_LIMIT,
    };
    // SAFETY: setrlimit with a valid pointer to a local struct.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
    }
    std::process::abort()
}
