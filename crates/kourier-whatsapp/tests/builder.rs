//! whatsapp-rust's own client builder accepts the store: with an inbound durability hook set it PROBES
//! the store (`src/client/builder.rs` `probe_durability_backend`: a bounded read, a conditional insert
//! of a probe row, a bounded read back, a group read, a conditional delete) and refuses a backend
//! that fails any step with `UnsupportedDurabilityBackend`. A store that left the pending-inbound
//! family at the trait's fail-closed defaults cannot be used by Kourier; this test is the proof that
//! ours can.
//!
//! OFFLINE BY CONSTRUCTION (W1 fix F1), in three layers:
//! 1. whatsapp-rust is built WITHOUT its `tokio-transport` and `ureq-client` features, so its builder
//!    does not pre-fill the real WebSocket transport and HTTPS client: `build()` exists only after
//!    `with_transport_factory` AND `with_http_client`. `the_builder_requires_both_slots` is a
//!    COMPILE-TIME proof (the file does not compile if a feature brings a default back).
//! 2. The builder here gets a transport factory and an HTTP client that refuse every call and count
//!    it (`support::guards`), `build()` is the last thing called (never `run`, `connect` or
//!    `Bot::run`), and the body runs in a CHILD process under `unshare -rn` (an empty network
//!    namespace: no interface, not even a route), so that no code path of the client could reach a
//!    network even by accident. Where user namespaces are unavailable the test is skipped with a
//!    message and never falls back to the real network.
//! 3. A source check ([`violations`]) over `src/` and `tests/` (and, for the entry points of the real
//!    network, every other crate of the workspace) keeps every file from naming the real transport or
//!    HTTP client (outside [`REAL_CLIENT_MODULES`]), from asking for the real parts (the entry point
//!    of the real parts is named only by that module and by kourier-app's `src/`), and demands, of a test
//!    file that builds a client, a transport factory AND an HTTP client for EVERY builder, and the
//!    `unshare` child; of a test file that makes a network, guard parts and, when it spawns an actor,
//!    the child (or the fake session). Unit tests are tests (review F4): `src/**/tests.rs` and
//!    `src/**/tests/` get the same child and network rules, and a call of `build_bot(` counts as a
//!    client builder there (a unit test may BUILD a client, only in the child, and never runs one
//!    without it). In `src/` only [`BUILDER_MODULES`] may hold a client builder, and
//!    nothing there may name the noise-certificate bypass or the usage-statistics plugin (W2, WAM never).
//!    The bypass is named by ONE test file, the scripted server (`DANGER_TEST_MODULES`), which is also
//!    the only caller of `NetParts::with_builder_hook`, the seam that applies it (`HOOK_MODULES`; the
//!    name is refused in every other file of every crate).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use wacore::types::events::InboundMessage;
use whatsapp_rust::Client;
use whatsapp_rust::bot::{BotBuilder, MissingHttpClient, MissingTransport, Provided};
use whatsapp_rust::prelude::*;
use whatsapp_rust::types::durability_hook::InboundDurabilityHook;

use crate::support::guards::{self, NoDial, NoHttp};
use crate::support::{self, db_path, key};

const CHILD_DIR: &str = "WA_BUILDER_DIR";

/// The files (relative to the crate root, `/`-separated) that may name the real transport and HTTP
/// client: the ONE module that makes the application's parts, and nothing else.
const REAL_CLIENT_MODULES: &[&str] = &["src/real.rs"];

/// The files of `src/` that may hold a client builder (a later chunk adds `session.rs`; none yet).
const BUILDER_MODULES: &[&str] = &["src/session.rs"];

/// The file prefixes of OTHER crates that may call the entry point of the real parts: the
/// composition root registers the network there (W2b).
const ENTRY_PREFIXES: &[&str] = &["../kourier-app/src/"];

/// The only test file that may name the noise-certificate bypass: the scripted server (W2 C5).
const DANGER_TEST_MODULES: &[&str] = &["tests/support/upstream/wire.rs"];

/// The files that may name the seam of the scripted server, `NetParts::with_builder_hook`: its
/// definition and its one caller. No other file of any crate does (the seam would let a client skip the
/// server's certificate check).
const HOOK_MODULES: &[&str] = &["src/parts.rs", "tests/support/upstream/wire.rs"];

/// The files of `src/` that may name the usage-statistics stanza `w:stats` (the decoder that counts
/// what leaves the client, a later chunk); no file may name the plugin or its runtime.
const STATS_MODULES: &[&str] = &["src/wam.rs"];

/// A hook that commits nothing: setting it is what makes the builder probe the store.
struct NoopHook;

#[async_trait::async_trait]
impl InboundDurabilityHook for NoopHook {
    async fn on_messages(
        &self,
        _client: Arc<Client>,
        _batch: &[InboundMessage],
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Where the child runs; a no-op when this test is run as an ordinary test.
#[tokio::test]
async fn child() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let owner = kourier_whatsapp::StoreOwner::open(&db_path(Path::new(&dir)), &key(), "wa-builder")
        .expect("open the store");
    let marker = Arc::new(());
    let bot = Bot::builder()
        .with_backend(owner.store())
        .with_transport_factory(NoDial(marker.clone()))
        .with_http_client(NoHttp(marker.clone()))
        .with_runtime(TokioRuntime)
        .with_inbound_durability_hook(NoopHook)
        .build()
        .await
        .expect("the builder accepts the store: the probe of the pending-inbound family passed");
    assert_eq!(guards::dials(), 0, "a dial was attempted");
    assert_eq!(guards::http_calls(), 0, "an HTTP request was attempted");
    // The probe left no row behind.
    let rows: i64 = kourier_db::open(&db_path(Path::new(&dir)), &key())
        .expect("raw")
        .query_row("SELECT count(*) FROM pending_inbound_messages", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert_eq!(rows, 0, "the probe row was not removed");
    // The built client holds both guards: the test's own clone of the marker and one inside each.
    assert_eq!(
        Arc::strong_count(&marker),
        3,
        "the client does not hold both guards: it has some other transport or HTTP client"
    );
    drop(bot);
    owner.close();
}

#[test]
fn the_client_builder_accepts_the_store() {
    if !support::offline::unshare_or_skip("this test never builds a client on the real network") {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let exe = std::env::current_exe().expect("test binary");
    let out = Command::new("unshare")
        .args(["-rn"])
        .arg(exe)
        .arg("--exact")
        .arg(support::child_filter(module_path!(), "child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_DIR, dir.path())
        .output()
        .expect("spawn the child under unshare");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "the child failed ({:?}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// COMPILE-TIME proof that the real transport and HTTP client are not defaults: a builder that has
/// its backend still MISSES the transport factory and the HTTP client (the types name the slots).
/// With whatsapp-rust's `tokio-transport` or `ureq-client` feature on, the slot would be `Provided`,
/// `Bot::builder().with_backend(store).build()` would compile and the client could dial without any
/// code naming the real types, and this function would not compile. (It is never called.)
#[test]
fn the_builder_requires_both_slots() {
    fn after_the_backend(
        store: kourier_whatsapp::KourierStore,
    ) -> BotBuilder<Provided, MissingTransport, MissingHttpClient, Provided> {
        Bot::builder().with_backend(store)
    }
    let _ = after_the_backend;
}

/// The names of the real network (assembled at run time so that this file does not contain them).
fn real_network_names() -> [String; 6] {
    [
        ["Ureq", "HttpClient"].concat(),
        ["TokioWebSocket", "TransportFactory"].concat(),
        ["whatsapp_rust_", "tokio_transport"].concat(),
        ["whatsapp_rust_", "ureq_http_client"].concat(),
        ["whatsapp-rust-", "tokio-transport"].concat(),
        ["whatsapp-rust-", "ureq-http-client"].concat(),
    ]
}

/// The names by which the real parts are asked for: the entry point and the path of its module.
fn entry_names() -> [String; 3] {
    [
        ["net_", "parts"].concat(),
        ["re", "al::"].concat(),
        ["::", "re", "al"].concat(),
    ]
}

/// What only the usage-statistics machinery is called, and the noise-certificate bypass (tests only).
fn danger_names() -> [String; 3] {
    [
        ["DangerSkip", "CertChainVerify"].concat(),
        ["with_noise_", "cert_policy"].concat(),
        ["Noise", "CertPolicy"].concat(),
    ]
}

/// The seam of the scripted server.
fn hook_name() -> String {
    ["with_", "builder_hook"].concat()
}

/// A file (any crate) that names the seam of the scripted server though it is neither its definition
/// nor its one caller (comment lines left out: the docs name it).
fn hook_violations(path: &str, text: &str) -> Vec<String> {
    if HOOK_MODULES.contains(&path) {
        return Vec::new();
    }
    let code = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    if names(&code, &hook_name()) {
        vec![format!(
            "{path} names the scripted server's seam ({})",
            hook_name()
        )]
    } else {
        Vec::new()
    }
}

fn wam_names() -> [String; 2] {
    [["plugin_", "wam"].concat(), ["Wam", "Runtime"].concat()]
}

fn stats_name() -> String {
    ["w:", "stats"].concat()
}

/// The closure callbacks of upstream's `BotBuilder` (`on_event`, `on_message`, ...). Their delivery
/// policies drop events when the callback falls behind, and upstream acknowledges a stanza whether
/// or not such a callback has run: nothing durable may hang on one (W3a, decision D2). The client has
/// ONE raw event handler (`with_event_handler`) and the durability hook, in `src/session.rs`.
fn closure_names() -> Vec<String> {
    [
        "event",
        "event_for",
        "message",
        "qr_code",
        "pair_code",
        "pair_code_error",
        "pair_code_refresh",
        "connected",
        "logged_out",
    ]
    .iter()
    .map(|name| [".on_", name, "("].concat())
    .collect()
}

/// The registrations of the one raw handler and of the durability hook.
fn handler_name() -> String {
    ["with_", "event_handler("].concat()
}

fn durability_name() -> String {
    ["with_", "inbound_durability_hook("].concat()
}

/// The registration of the history capture hook (W4; also matches the `_arc` form).
fn capture_name() -> String {
    ["with_", "history_sync_capture_hook"].concat()
}

fn cache_config_name() -> String {
    ["with_", "cache_config("].concat()
}

/// The problems of `code` (the file without its comment lines): a closure callback anywhere, and the
/// raw handler or the durability hook registered in a file of `src/` other than the builder module
/// (tests build their own clients, in the child, and register what they need).
fn registration_violations(path: &str, code: &str, test_file: bool) -> Vec<String> {
    let mut found = Vec::new();
    for name in closure_names() {
        if code.contains(name.as_str()) {
            found.push(format!(
                "{path} registers a closure callback ({name}); the client has one handler"
            ));
        }
    }
    if !test_file && !BUILDER_MODULES.contains(&path) {
        for name in [handler_name(), durability_name(), capture_name()] {
            if code.contains(name.as_str()) {
                found.push(format!(
                    "{path} registers {name}; only {BUILDER_MODULES:?} may"
                ));
            }
        }
    }
    found
}

/// The files of `src/` that may parse a `<call>` stanza: calls are shown, never taken (W3b), and the one
/// module that reads one is the interceptor's (its unit tests build upstream's own event from one).
const CALL_PARSER_MODULES: &[&str] = &["src/calls.rs", "src/calls/tests.rs"];

/// The files of `src/` that may register a stanza interceptor, once: the builder module (the call
/// interceptor is registered right after the build and before the client starts).
const INTERCEPTOR_MODULES: &[&str] = &["src/session.rs"];

/// The parser of a `<call>` stanza.
fn call_parser_name() -> String {
    ["parse_", "call_stanza"].concat()
}

/// The only file of the crate that names upstream's builder of the pair error (W2c): the pairing is refused
/// with the frame whatsmeow and upstream themselves send, and nothing else in the crate writes one.
const PAIR_ERROR_MODULES: &[&str] = &["src/prepair.rs"];

/// The builder of the pair error.
fn pair_error_name() -> String {
    ["build_pair_", "error_node"].concat()
}

/// The problems of `code` about the pairing's answer: upstream's pair-error builder named outside the claim.
fn pairing_violations(path: &str, code: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !PAIR_ERROR_MODULES.contains(&path) && names(code, &pair_error_name()) {
        found.push(format!(
            "{path} builds a pair error; only {PAIR_ERROR_MODULES:?} may"
        ));
    }
    found
}

/// The registration of a stanza interceptor.
/// W2c L4: `tests/prepair.rs` holds the control (a pairing the rules accept still gets its
/// `pair-device-sign`) and the refusals (a wrong scan gets none). A refusal test only means something
/// while the control runs with it, so no test of that file may be ignored (and `xtask`'s `SLOW_TESTS` names
/// all of them or none: its unit test `the_pre_pair_wire_tests_stay_in_one_tier`). An attribute line that
/// names `ignore` (`#[ignore]`, `#[cfg_attr(..., ignore)]`) is a violation there; `code` has no comment
/// lines.
fn prepair_ignore_violations(path: &str, code: &str) -> Vec<String> {
    if path != "tests/prepair.rs" {
        return Vec::new();
    }
    code.lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("#[") && names(l, "ignore"))
        .map(|_| {
            format!("{path} ignores a test: the control and the refusals stay in one tier for ever")
        })
        .collect()
}

fn interceptor_name() -> String {
    ["add_stanza_", "interceptor("].concat()
}

/// The names by which a call would be answered, accepted, rejected or ended, or media would be started:
/// nothing is ever sent for a call (a reply from one of the user's devices can end the call or stop the
/// phone ringing), so no file of the crate names them.
fn call_answer_names() -> [String; 7] {
    [
        ["build_offer_", "ack_receipt"].concat(),
        ["send_offer_", "ack_receipt"].concat(),
        ["build_", "accept"].concat(),
        ["build_", "reject"].concat(),
        ["build_", "preaccept"].concat(),
        ["build_", "terminate"].concat(),
        ["vo", "ip("].concat(),
    ]
}

/// The problems of `code` (the file without its comment lines) about calls: the parser named outside its
/// module, an interceptor registered outside the builder module (in a test too: a test registers none),
/// or any name by which a call is answered.
fn call_violations(path: &str, code: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !CALL_PARSER_MODULES.contains(&path) && names(code, &call_parser_name()) {
        found.push(format!(
            "{path} parses a call stanza; only {CALL_PARSER_MODULES:?} may"
        ));
    }
    if !INTERCEPTOR_MODULES.contains(&path) && names(code, &interceptor_name()) {
        found.push(format!(
            "{path} registers a stanza interceptor; only {INTERCEPTOR_MODULES:?} may"
        ));
    }
    for name in call_answer_names() {
        if names(code, &name) {
            found.push(format!(
                "{path} names {name}: nothing is ever sent for a call"
            ));
        }
    }
    found
}

/// The files of `src/` that may call one of upstream's WRITES to the phone's app state ([`app_state_write_names`]):
/// none until W6b, which names its one writer here (behind the per-account switch). W6a1 and W6a2 only READ app state
/// (`resync_app_state` in snapshot mode, the typed events): a patch that reached the phone from a build that has no
/// switch yet would be an account-risk of the class D15 and H7 guard against (W6a1 fix F10, interop review INTEROP-10).
const APP_STATE_WRITE_MODULES: &[&str] = &[];

/// The names of upstream's app-state writers and of the calls that rebuild its state by hand: the typed verbs of
/// `chat_actions()` (archive, pin, mute, mark as read, delete and clear a chat, delete a message for me), the contact
/// and label writes, the push-name and status-mute writes, the raw `send_app_state_action` and
/// `remove_app_state_action`, and the engine's own `clean_dirty_bits`, `resync_app_state_collection` and
/// `process_sync_task`. (`resync_app_state(` is the READ Kourier makes and is not here.)
fn app_state_write_names() -> [String; 20] {
    [
        ["chat_", "actions("].concat(),
        ["archive_", "chat"].concat(),
        ["pin_", "chat"].concat(),
        ["mute_", "chat"].concat(),
        ["mark_chat_", "as_read"].concat(),
        ["delete_", "chat"].concat(),
        ["clear_", "chat"].concat(),
        ["delete_message_", "for_me"].concat(),
        ["save_", "contact"].concat(),
        ["remove_", "contact"].concat(),
        ["send_app_state_", "action"].concat(),
        ["remove_app_state_", "action"].concat(),
        ["set_user_status_", "mute"].concat(),
        ["labels", "()"].concat(),
        ["app_state_", "settings()"].concat(),
        ["set_push_", "name"].concat(),
        ["sync_push_", "name"].concat(),
        ["clean_dirty_", "bits"].concat(),
        ["resync_app_state_", "collection"].concat(),
        ["process_sync_", "task"].concat(),
    ]
}

/// The files of `src/` that may call one of upstream's WRITES to a group or to the phone's blocklist
/// ([`group_write_names`]): none. W6a2 only READS groups and the blocklist (the four reads of
/// [`group_read_names`]); the group's own changes are K8c's (an admin action) and a block action is a later step. A step
/// that builds one names its ONE module here, with its switch and its limits (fix F6, PROTOCOL-10).
const GROUP_WRITE_MODULES: &[&str] = &[];

/// The method calls of upstream's group and blocking WRITES and of its 1:1 timer setter, each built from pieces so that
/// this file does not name them: the title and the description, leaving, adding, removing, promoting and demoting
/// participants, a group's timer, announcement and lock modes, the member-add and approval modes, creating a group,
/// blocking and unblocking, and `Client::set_chat_disappearing_timer` (Kourier's one timer frame goes through the ONE
/// `Client::send`, `send/timer.rs`). The short names are matched as method calls (`.leave(`), so that Kourier's own
/// `block_on` or `blocked` are not them.
fn group_write_names() -> [String; 18] {
    [
        [".set_", "subject("].concat(),
        [".set_", "description("].concat(),
        [".lea", "ve("].concat(),
        [".add_", "participants("].concat(),
        [".add_participants_", "with_history("].concat(),
        [".remove_", "participants("].concat(),
        [".remove_participants_", "including_linked_groups("].concat(),
        [".promote_", "participants("].concat(),
        [".demote_", "participants("].concat(),
        [".set_", "ephemeral("].concat(),
        [".set_", "announce("].concat(),
        [".set_", "locked("].concat(),
        [".set_member_", "add_mode("].concat(),
        [".set_membership_", "approval("].concat(),
        [".create_", "group("].concat(),
        [".blo", "ck("].concat(),
        [".unblo", "ck("].concat(),
        [".set_chat_", "disappearing_timer("].concat(),
    ]
}

/// The problems of `code` (the file without its comment lines) about writes to a group or the blocklist: a file of `src/`
/// naming one of [`group_write_names`] outside [`GROUP_WRITE_MODULES`]. The tests (`tests/`) are not scanned: the scripted
/// server answers such requests there.
fn group_write_violations(path: &str, code: &str) -> Vec<String> {
    if !path.starts_with("src/") || GROUP_WRITE_MODULES.contains(&path) {
        return Vec::new();
    }
    group_write_names()
        .iter()
        .filter(|name| names(code, name))
        .map(|name| {
            format!(
                "{path} calls {name}: no group or blocklist write is built yet \
                 ({GROUP_WRITE_MODULES:?} may)"
            )
        })
        .collect()
}

/// The problems of `code` (the file without its comment lines) about writes to the phone's app state: a file of
/// `src/` naming one of [`app_state_write_names`] outside [`APP_STATE_WRITE_MODULES`]. The tests (`tests/`) are not
/// scanned: the scripted phone builds its own patches there.
fn app_state_write_violations(path: &str, code: &str) -> Vec<String> {
    if !path.starts_with("src/") || APP_STATE_WRITE_MODULES.contains(&path) {
        return Vec::new();
    }
    app_state_write_names()
        .iter()
        .filter(|name| names(code, name))
        .map(|name| {
            format!(
                "{path} names {name}: nothing is written to the phone's app state before W6b \
                 ({APP_STATE_WRITE_MODULES:?} may)"
            )
        })
        .collect()
}

/// What makes a source file hold a client builder: the builder itself and the lower-level one. (The
/// crate's own function that builds the real client, `session::build_bot`, counts as a builder in the
/// unit tests of `src/`: see [`build_bot_calls`].)
fn builder_names() -> [String; 2] {
    [
        ["Bot::", "builder("].concat(),
        ["Client", "Builder::"].concat(),
    ]
}

/// The crate's own function that builds the real client (`session::build_bot`).
fn build_bot_name() -> String {
    ["build", "_bot("].concat()
}

/// How many times `code` calls the crate's `build_bot` (its definition does not count).
fn build_bot_calls(code: &str) -> usize {
    let name = build_bot_name();
    code.match_indices(name.as_str())
        .filter(|(at, _)| !code[..*at].ends_with("fn "))
        .count()
}

/// Whether `path` (relative to the crate root) is test code: everything outside `src/`, and in `src/`
/// the files named `tests.rs` and anything below a `tests/` directory (review F4).
fn is_test_file(path: &str) -> bool {
    !path.starts_with("src/") || path.ends_with("/tests.rs") || path.contains("/tests/")
}

/// The call that makes a client of a builder.
fn build_call() -> String {
    [".", "build()"].concat()
}

/// The call that makes a network.
fn network_call() -> String {
    ["WhatsApp", "Network::new("].concat()
}

/// Whether `text` holds `needle` as a whole name: an identifier character does not touch it where the
/// needle begins or ends with one (a longer name that merely ends in the needle is not the needle).
fn names(text: &str, needle: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let first = needle.chars().next().is_some_and(ident);
    let last = needle.chars().next_back().is_some_and(ident);
    text.match_indices(needle).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + needle.len()..].chars().next();
        (!first || !before.is_some_and(ident)) && (!last || !after.is_some_and(ident))
    })
}

/// A file (path relative to the crate root, `../<crate>/...` for another crate) that asks for the real
/// parts, though it is neither the module that makes them nor the composition root.
fn entry_violations(path: &str, text: &str) -> Vec<String> {
    if REAL_CLIENT_MODULES.contains(&path) || ENTRY_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Vec::new();
    }
    entry_names()
        .iter()
        .filter(|name| names(text, name))
        .map(|name| format!("{path} asks for the real parts ({name})"))
        .collect()
}

/// The problems of one source file of this crate (`path` relative to the crate root): it names the
/// real transport or HTTP client outside [`REAL_CLIENT_MODULES`]; it asks for the real parts
/// ([`entry_violations`]); in `src/` it names the noise-certificate bypass, the statistics plugin, or
/// `w:stats` outside [`STATS_MODULES`]; it holds a client builder in `src/` outside
/// [`BUILDER_MODULES`]; holding a builder, it makes N clients (N calls of `build()`, comment lines
/// left out) and sets fewer than N transport factories or fewer than N HTTP clients, or (a test) has
/// no `unshare` child; a test that makes a network gives it parts that are not the guards, or spawns
/// an actor without the `unshare` child or the fake session. (A builder handed to another file to be
/// built there is not seen; no file does that.)
fn violations(path: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !REAL_CLIENT_MODULES.contains(&path) {
        for name in real_network_names() {
            if text.contains(&name) {
                found.push(format!("{path} names {name}"));
            }
        }
    }
    found.extend(entry_violations(path, text));
    found.extend(hook_violations(path, text));
    let in_src = path.starts_with("src/");
    let test_file = is_test_file(path);
    if in_src {
        for name in danger_names().iter().chain(wam_names().iter()) {
            if text.contains(name.as_str()) {
                found.push(format!("{path} names {name}, which src/ never does"));
            }
        }
        if !STATS_MODULES.contains(&path) && text.contains(&stats_name()) {
            found.push(format!("{path} names the statistics stanza"));
        }
    } else if !DANGER_TEST_MODULES.contains(&path) {
        for name in danger_names() {
            if names(text, &name) {
                found.push(format!(
                    "{path} names {name}; only {DANGER_TEST_MODULES:?} may (the scripted server)"
                ));
            }
        }
    }
    let code = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    found.extend(registration_violations(path, &code, test_file));
    found.extend(call_violations(path, &code));
    found.extend(app_state_write_violations(path, &code));
    found.extend(group_write_violations(path, &code));
    found.extend(pairing_violations(path, &code));
    found.extend(prepair_ignore_violations(path, &code));
    let holds_builder = builder_names().iter().any(|b| code.contains(b.as_str()));
    let builds = if holds_builder {
        code.matches(build_call().as_str()).count()
    } else {
        0
    };
    // A unit test that calls the crate's `build_bot` makes a client too: its two slots are arguments
    // (the types enforce them), the child is the rule. The production caller is not a test.
    let bot_calls = if test_file { build_bot_calls(&code) } else { 0 };
    if builds > 0 {
        for guard in [
            ["with_", "transport_factory"].concat(),
            ["with_", "http_client"].concat(),
        ] {
            let set = code.matches(guard.as_str()).count();
            if set < builds {
                found.push(format!(
                    "{path} makes {builds} client(s) but sets {guard} {set} time(s)"
                ));
            }
        }
        if in_src && !BUILDER_MODULES.contains(&path) {
            found.push(format!(
                "{path} makes a client; only {BUILDER_MODULES:?} may"
            ));
        }
    }
    // The `unshare` child is the rule of the tests (`tests/` and the unit tests of `src/`); in
    // `src/` the production builder takes its two slots from the parts it is given.
    if test_file && (builds > 0 || bot_calls > 0) && !code.contains("unshare") {
        found.push(format!(
            "{path} makes a client without an `unshare -rn` child"
        ));
    }
    if test_file {
        found.extend(network_violations(path, &code));
    }
    found
}

/// A test file that makes a network: every call hands it the guard parts (`guards::parts()` or
/// `guards::counting_parts(..)`, never a variable that could hold anything), and a file that also
/// spawns an actor runs it in the `unshare` child (`run_child(`) or over the fake session.
fn network_violations(path: &str, code: &str) -> Vec<String> {
    let call = network_call();
    let mut found = Vec::new();
    let mut calls = 0;
    for (at, _) in code.match_indices(call.as_str()) {
        calls += 1;
        let argument = code[at + call.len()..].trim_start();
        // The guards, or the scripted server's parts (an in-process pipe, never a socket).
        let allowed = ["guards::", "support::guards::", "wire::", "support::wire::"];
        if !allowed.iter().any(|prefix| argument.starts_with(prefix)) {
            found.push(format!(
                "{path} makes a network from something other than the guard parts or the scripted server"
            ));
        }
    }
    let spawns = code.contains(".spawn(");
    let contained = ["unshare", "FakeSession", "run_child("]
        .iter()
        .any(|word| code.contains(word));
    if calls > 0 && spawns && !contained {
        found.push(format!(
            "{path} spawns an actor outside an `unshare -rn` child or the fake session"
        ));
    }
    found
}

/// No file of the crate may name the real transport or HTTP client, and a file that builds a client
/// must hand EVERY builder a transport factory and an HTTP client of its own, in an `unshare` child.
#[test]
fn no_source_file_names_the_real_network_or_builds_a_client_without_guards() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    let sources = files.len();
    collect(&root.join("tests"), &mut files);
    assert!(
        sources >= 8 && files.len() - sources >= 10,
        "the sources were not found: {files:?}"
    );
    let mut found = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        let relative = path
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        found.extend(violations(&relative, &text));
    }
    // The entry point of the real parts, in every other crate of the workspace: only kourier-app's
    // `src/` may call it (W2b registers the network there).
    let others = other_crates_sources(root);
    assert!(
        others.iter().any(|(p, _)| p == "../kourier-app/src/lib.rs"),
        "the other crates were not found: {:?}",
        others.iter().map(|(p, _)| p).take(5).collect::<Vec<_>>()
    );
    for (relative, path) in others {
        let text = std::fs::read_to_string(&path).expect("read");
        found.extend(entry_violations(&relative, &text));
        found.extend(hook_violations(&relative, &text));
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// The Rust sources (`src/` and `tests/`) of the other crates of the workspace and of xtask, as
/// `(path relative to this crate's root, file)`.
fn other_crates_sources(root: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut crates = Vec::new();
    let beside = root.parent().expect("crates dir");
    for entry in std::fs::read_dir(beside).expect("read crates dir") {
        let dir = entry.expect("entry").path();
        if dir != root && dir.join("Cargo.toml").exists() {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            crates.push((format!("../{name}"), dir));
        }
    }
    let xtask = beside.parent().expect("workspace root").join("xtask");
    if xtask.join("Cargo.toml").exists() {
        crates.push(("../../xtask".to_owned(), xtask));
    }
    let mut found = Vec::new();
    for (prefix, dir) in crates {
        for sub in ["src", "tests"] {
            let mut files = Vec::new();
            if dir.join(sub).is_dir() {
                collect(&dir.join(sub), &mut files);
            }
            for file in files {
                let rest = file.strip_prefix(&dir).expect("under the crate");
                let relative = format!("{prefix}/{}", rest.to_string_lossy().replace('\\', "/"));
                found.push((relative, file));
            }
        }
    }
    found
}

/// The check itself: one case per rule, so that a weakened rule fails here.
#[test]
fn the_source_check_refuses_what_it_is_for() {
    let start = ["Bot::", "builder()"].concat();
    let build = build_call();
    let factory = ["with_", "transport_factory"].concat();
    let http = ["with_", "http_client"].concat();
    let both = format!("{start}.with_backend(s).{factory}(a).{http}(b){build}");
    // A guarded build in a file with an unshare child is fine.
    assert!(violations("tests/a.rs", &format!("unshare {both}")).is_empty());
    // No client at all is fine, and so is a builder that is never built (the slots check).
    assert!(violations("src/a.rs", "fn nothing() {}").is_empty());
    assert!(violations("tests/a.rs", &format!("{start}.with_backend(s)")).is_empty());
    // A real type, in a test or in src/, is refused (a module that is not the listed one too).
    for name in real_network_names() {
        for path in ["tests/a.rs", "src/a.rs", "src/net/real.rs"] {
            let v = violations(path, &format!("use {name};"));
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
    }
    // The one module that may name the real types is src/real.rs, and no other.
    assert_eq!(REAL_CLIENT_MODULES, &["src/real.rs"]);
    for name in real_network_names() {
        assert!(violations("src/real.rs", &format!("use {name};")).is_empty());
    }
    // Without the HTTP client, without the transport factory, without either.
    let no_http = format!("unshare {start}.with_backend(s).{factory}(a){build}");
    let v = violations("tests/a.rs", &no_http);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains(&http), "{v:?}");
    let no_transport = format!("unshare {start}.with_backend(s).{http}(b){build}");
    let v = violations("tests/a.rs", &no_transport);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains(&factory), "{v:?}");
    let none = format!("unshare {start}.with_backend(s){build}");
    assert_eq!(violations("tests/a.rs", &none).len(), 2);
    // One guarded build does not excuse an unguarded one in the same file.
    let two = format!("unshare {both}\n{start}.with_backend(s){build}");
    assert_eq!(violations("tests/a.rs", &two).len(), 2);
    // A build outside an unshare child.
    let v = violations("tests/a.rs", &both);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("unshare"), "{v:?}");
    // The lower-level builder counts as a builder too.
    let low = ["Client", "Builder::new()"].concat();
    assert_eq!(
        violations("src/session.rs", &format!("unshare {low}{build}")).len(),
        2
    );
    // Comment lines count for nothing (but a real type named in one is still refused above).
    let comment = format!("// {both}\nunshare");
    assert!(violations("tests/a.rs", &comment).is_empty());
}

/// Review F4: the unit tests under `src/` (`tests.rs`, `tests/`) are tests to the source check - a
/// call of `build_bot(` there needs the `unshare` child, a network there needs the guard parts - while
/// the production caller of `build_bot(` and its definition are not tests.
#[test]
fn the_source_check_treats_the_unit_tests_of_src_as_tests() {
    let bot = build_bot_name();
    let call = format!("let b = super::super::{bot}store, a, h, f, None).await;");
    for path in [
        "src/session/client/tests.rs",
        "src/actor/tests.rs",
        "src/x/tests/y.rs",
    ] {
        assert!(is_test_file(path), "{path}");
        let v = violations(path, &call);
        assert_eq!(v.len(), 1, "{path}: {v:?}");
        assert!(v[0].contains("unshare"), "{v:?}");
        assert!(
            violations(path, &format!("{call}\nCommand::new(\"unshare\");")).is_empty(),
            "{path}"
        );
        // The network rule applies to them as to `tests/`.
        let network = network_call();
        assert_eq!(
            violations(path, &format!("{network}parts);")).len(),
            1,
            "{path}"
        );
        assert!(violations(path, &format!("{network}guards::parts());")).is_empty());
    }
    // Production files of `src/` are not tests: the real session calls it, the module defines it.
    for path in [
        "src/session/client.rs",
        "src/session.rs",
        "src/actor.rs",
        "src/testsuite.rs",
    ] {
        assert!(!is_test_file(path), "{path}");
        assert!(violations(path, &call).is_empty(), "{path}");
    }
    assert!(
        violations(
            "src/session.rs",
            &format!("pub(crate) async fn {bot}store: S) {{}}")
        )
        .is_empty()
    );
    // Its definition is not a call even in a test file, and a comment is nothing.
    assert_eq!(build_bot_calls(&format!("async fn {bot}x) {{}}")), 0);
    assert!(violations("src/a/tests.rs", &format!("// {call}")).is_empty());
    // Everything outside `src/` stays a test.
    assert!(is_test_file("tests/builder.rs") && is_test_file("tests/support/mod.rs"));
}

/// The rules of W2 C2, one case each: who may ask for the real parts, what `src/` never names, who may
/// hold a client builder, and what a test that makes a network must do.
#[test]
fn the_source_check_keeps_the_real_parts_and_the_forbidden_names_out() {
    let entry = ["net_", "parts"].concat();
    let module = ["re", "al::"].concat();
    let call = format!("let p = kourier_whatsapp::{module}{entry}();");
    // Only the module that makes them and the composition root's `src/` may ask for the real parts.
    assert!(violations("src/real.rs", &format!("pub fn {entry}() {{}}")).is_empty());
    assert!(entry_violations("../kourier-app/src/app.rs", &call).is_empty());
    for path in [
        "tests/a.rs",
        "src/a.rs",
        "../kourier-app/tests/a.rs",
        "../kourier-e2e/src/lib.rs",
        "../../xtask/src/main.rs",
    ] {
        assert!(
            !entry_violations(path, &call).is_empty(),
            "{path} may ask for the real parts"
        );
    }
    // The module path alone is enough, and so is the bare name; a longer name is not the name.
    assert!(!entry_violations("tests/a.rs", &format!("use crate::{module}x;")).is_empty());
    assert!(!entry_violations("tests/a.rs", &format!("let f = {entry};")).is_empty());
    assert!(entry_violations("tests/a.rs", &format!("let f = un{module}x;")).is_empty());
    assert!(entry_violations("tests/a.rs", &format!("let f = x{entry};")).is_empty());
    // src/ never names the noise-certificate bypass or the statistics plugin; of the tests only the
    // scripted server's file names the bypass.
    for name in danger_names().iter().chain(wam_names().iter()) {
        assert_eq!(
            violations("src/a.rs", &format!("use {name};")).len(),
            1,
            "{name}"
        );
    }
    assert_eq!(DANGER_TEST_MODULES, &["tests/support/upstream/wire.rs"]);
    for name in danger_names() {
        let text = format!("use {name};");
        assert!(
            violations("tests/support/upstream/wire.rs", &text).is_empty(),
            "{name}"
        );
        for path in ["tests/a.rs", "tests/support/guards.rs", "tests/wire.rs"] {
            let v = violations(path, &text);
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
    }
    // The seam of the scripted server: its definition and its one caller, no other file of any crate
    // (a comment that explains it is nothing).
    assert_eq!(
        HOOK_MODULES,
        &["src/parts.rs", "tests/support/upstream/wire.rs"]
    );
    let hook = hook_name();
    let call = format!("let p = parts.{hook}(|b| b);");
    for path in HOOK_MODULES {
        assert!(hook_violations(path, &call).is_empty(), "{path}");
    }
    for path in [
        "tests/a.rs",
        "tests/wire.rs",
        "src/session.rs",
        "src/actor.rs",
        "../kourier-app/src/lib.rs",
        "../kourier-e2e/tests/a.rs",
        "../../xtask/src/main.rs",
    ] {
        assert_eq!(hook_violations(path, &call).len(), 1, "{path}");
    }
    assert!(hook_violations("src/session.rs", &format!("// {hook}\n/// {hook}")).is_empty());
    // A longer name is not the name.
    assert!(hook_violations("tests/a.rs", &format!("let x = un{hook}();")).is_empty());
    assert!(hook_violations("tests/a.rs", &format!("let x = {hook}_x();")).is_empty());
    // The statistics stanza is named by the decoder module only.
    let stats = format!("let tag = \"{}\";", stats_name());
    assert_eq!(violations("src/a.rs", &stats).len(), 1);
    assert!(violations("src/wam.rs", &stats).is_empty());
    assert_eq!(STATS_MODULES, &["src/wam.rs"]);
    // A client builder in src/ only in the listed module, which needs no `unshare` child but both slots.
    let build = build_call();
    let both = format!(
        "{}.with_backend(s).{}(a).{}(b){build}",
        ["Bot::", "builder()"].concat(),
        ["with_", "transport_factory"].concat(),
        ["with_", "http_client"].concat()
    );
    assert!(violations("src/session.rs", &both).is_empty());
    let v = violations("src/actor.rs", &both);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("only"), "{v:?}");
    assert_eq!(BUILDER_MODULES, &["src/session.rs"]);
    // A network in a test: guard parts only, and an actor only in the child or over the fake session.
    let network = network_call();
    let ok = format!("{network}guards::parts());");
    assert!(violations("tests/a.rs", &ok).is_empty());
    assert!(
        violations(
            "tests/a.rs",
            &format!("{network}support::guards::parts());")
        )
        .is_empty()
    );
    // The scripted server's parts count as the guards: an in-process pipe, never a socket.
    for scripted in ["wire::parts(&server))", "support::wire::parts(&server))"] {
        assert!(
            violations("tests/a.rs", &format!("{network}{scripted};")).is_empty(),
            "{scripted}"
        );
    }
    for bad in [
        format!("{network}parts);"),
        format!("{network}other::wire::parts(&s));"),
        format!("{network}{module}x());"),
        format!("{network} real_parts());"),
    ] {
        assert!(!violations("tests/a.rs", &bad).is_empty(), "{bad}");
    }
    let spawn = format!("{ok}\nnetwork.spawn(ctx);");
    let v = violations("tests/a.rs", &spawn);
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("spawns an actor"), "{v:?}");
    for contained in [
        "unshare",
        "FakeSession",
        "run_child(module_path!(), \"x\");",
    ] {
        let text = format!("{spawn}\n{contained}");
        assert!(violations("tests/a.rs", &text).is_empty(), "{contained}");
    }
    // A comment that mentions the call is nothing.
    assert!(violations("tests/a.rs", &format!("// {network}parts);\n")).is_empty());
}

/// The real module names the two constructors and nothing that builds, connects or runs a client.
///
/// CHANGED ON PURPOSE in W5b (decision G13): `src/real.rs` now DEFINES Kourier's own HTTP client
/// (`RealHttp`: an agent with timeouts, no proxy, no redirect; its upload with a per-request body
/// time), so `execute(` and the agent's `.build()` are allowed there. What still holds: it builds
/// NOTHING at load, because the entry point hands the two makers over as plain paths (`from_makers(` is
/// its only call: an agent or a client made in the entry point would be shared by every client and made
/// before any account connects); upstream's client is made only over Kourier's agent
/// (`with_agent`, never its default agent without timeouts); and no client builder, `connect`,
/// `run` or transport is named.
#[test]
fn the_real_module_names_the_two_constructors_and_builds_nothing() {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/real.rs"))
        .expect("src/real.rs");
    let code: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for needed in [
        ["TokioWebSocket", "TransportFactory::new"].concat(),
        ["Ureq", "HttpClient::with_agent"].concat(),
    ] {
        assert!(code.contains(&needed), "src/real.rs does not make {needed}");
    }
    let default_agent = ["Ureq", "HttpClient::new"].concat();
    assert!(
        !code.contains(&default_agent),
        "src/real.rs makes upstream's agent without timeouts ({default_agent})"
    );
    let mut forbidden = Vec::from(builder_names());
    forbidden.extend([
        build_bot_name(),
        ["con", "nect("].concat(),
        [".", "run("].concat(),
        ["Bot", "Builder"].concat(),
        ["create_", "transport"].concat(),
    ]);
    for word in forbidden {
        assert!(!names(&code, &word), "src/real.rs names {word}");
    }
    // The entry point makes nothing: its body is ONE call, `from_makers(`, of two paths.
    let head = ["pub fn net_", "parts() -> NetParts {"].concat();
    let at = code
        .find(&head)
        .expect("src/real.rs defines its entry point")
        + head.len();
    let body = &code[at..at + code[at..].find("\n}").expect("the end of the entry point")];
    assert!(
        body.contains(&["from_", "makers("].concat()) && body.matches('(').count() == 1,
        "the entry point of src/real.rs calls more than `from_makers`: {body}"
    );
}

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// W3a (decisions D2, D3): no closure callback is registered anywhere, and only the builder module
/// registers the raw handler and the durability hook.
#[test]
fn the_source_check_refuses_closure_callbacks_and_registrations_outside_the_builder() {
    for name in closure_names() {
        for path in ["src/a.rs", "tests/a.rs", "src/session.rs", "src/a/tests.rs"] {
            let v = violations(path, &format!("let b = bot{name}handler);"));
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
    }
    // A comment may name them (the docs do).
    let named = format!("// {}\n/// {}", closure_names()[0], handler_name());
    assert!(violations("src/a.rs", &named).is_empty());
    // The raw handler and the hooks belong to the builder module, in src/; a test registers its own.
    for name in [handler_name(), durability_name(), capture_name()] {
        let code = format!("b.{name}h);");
        assert_eq!(violations("src/a.rs", &code).len(), 1, "{name}");
        assert!(violations("src/session.rs", &code).is_empty(), "{name}");
        assert!(violations("tests/a.rs", &code).is_empty(), "{name}");
        assert!(violations("src/a/tests.rs", &code).is_empty(), "{name}");
    }
}

/// The builder module registers the durability hook, the history capture hook, the one raw handler and the
/// cache configuration once each: a client without the hook would acknowledge messages before they are
/// durable, and one without the capture hook would answer the phone for a history chunk it never kept.
#[test]
fn the_builder_registers_the_hook_the_one_handler_and_the_cache_configuration_once() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("src/session.rs")).expect("read");
    let code = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for name in [
        handler_name(),
        durability_name(),
        capture_name(),
        cache_config_name(),
    ] {
        assert_eq!(
            code.matches(name.as_str()).count(),
            1,
            "src/session.rs registers {name} {} times",
            code.matches(name.as_str()).count()
        );
    }
}

/// W6a1 fix F10 (INTEROP-10): no file of `src/` names an upstream app-state WRITE until W6b lists its one writer; the
/// list is empty today. Kills "the check skipped", "a name dropped from the list" and "a module listed that is not W6b's".
#[test]
fn the_source_check_refuses_an_app_state_write_before_w6b() {
    assert!(
        APP_STATE_WRITE_MODULES.is_empty(),
        "W6b lists its one writer here, with its switch"
    );
    // The list itself is pinned: a name leaves it only by a decision of the lane lead (this file is not scanned, so
    // the plain words may stand here).
    assert_eq!(
        app_state_write_names().to_vec(),
        [
            "chat_actions(",
            "archive_chat",
            "pin_chat",
            "mute_chat",
            "mark_chat_as_read",
            "delete_chat",
            "clear_chat",
            "delete_message_for_me",
            "save_contact",
            "remove_contact",
            "send_app_state_action",
            "remove_app_state_action",
            "set_user_status_mute",
            "labels()",
            "app_state_settings()",
            "set_push_name",
            "sync_push_name",
            "clean_dirty_bits",
            "resync_app_state_collection",
            "process_sync_task",
        ]
    );
    for name in app_state_write_names() {
        let call = format!("client.{name};");
        // Anywhere in `src/` (a unit test of `src/` too), once per name.
        for path in ["src/a.rs", "src/session.rs", "src/appstate/tests.rs"] {
            let v = violations(path, &call);
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
        // A comment may name it (the docs do); the scripted phone of `tests/` builds its own patches.
        assert!(violations("src/a.rs", &format!("// {call}")).is_empty());
        assert!(violations("tests/a.rs", &call).is_empty(), "{name}");
    }
    // A whole name only: Kourier's own `unarchive_chats` flag and the READ it makes are not writes.
    let reads = format!(
        "let unarchive_chats = true; client.{}(names, mode);",
        ["resync_app_state"].concat()
    );
    assert!(violations("src/a.rs", &reads).is_empty());
    assert!(
        violations(
            "src/a.rs",
            "const UNARCHIVE_CHATS: &str = \"unarchive_chats\";"
        )
        .is_empty()
    );
}

/// W6a2 fix F6 (PROTOCOL-10): no file of `src/` calls an upstream group or blocking write, or its 1:1 timer setter; the
/// list of modules that may is empty. Kills "the check skipped", "a name dropped from the list" and "a module listed".
#[test]
fn the_source_check_refuses_a_group_or_blocking_write() {
    assert!(
        GROUP_WRITE_MODULES.is_empty(),
        "the step that builds a group or block action lists its one module here"
    );
    // The list itself is pinned (this file is not scanned, so the plain words may stand here).
    assert_eq!(
        group_write_names().to_vec(),
        [
            ".set_subject(",
            ".set_description(",
            ".leave(",
            ".add_participants(",
            ".add_participants_with_history(",
            ".remove_participants(",
            ".remove_participants_including_linked_groups(",
            ".promote_participants(",
            ".demote_participants(",
            ".set_ephemeral(",
            ".set_announce(",
            ".set_locked(",
            ".set_member_add_mode(",
            ".set_membership_approval(",
            ".create_group(",
            ".block(",
            ".unblock(",
            ".set_chat_disappearing_timer(",
        ]
    );
    for name in group_write_names() {
        let call = format!("client.groups(){name}jid);");
        for path in ["src/a.rs", "src/session/client.rs", "src/groups/tests.rs"] {
            let v = violations(path, &call);
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
        // A comment may name it (the docs do); the scripted server of `tests/` answers such requests.
        assert!(violations("src/a.rs", &format!("// {call}")).is_empty());
        assert!(violations("tests/a.rs", &call).is_empty(), "{name}");
    }
    // Method calls only: Kourier's own words are not upstream's writes.
    let own = "tokio::task::block_in_place(f); let blocked = self.blocked(); self.leave_marker(); \
               let groups = blocklist.unblocked(); fn block(&self) {}";
    assert!(violations("src/a.rs", own).is_empty(), "{own}");
}

/// W3b (brief 3.9): calls are shown, never taken. The parser of a `<call>` stanza is named by the
/// interceptor's module only, a stanza interceptor is registered by the builder module only, and no
/// file names a function by which a call is answered.
#[test]
fn the_source_check_refuses_what_could_answer_a_call() {
    let parser = call_parser_name();
    let register = interceptor_name();
    // The parser: its module may, nothing else may (a test file included).
    let call = format!("let c = {parser}(node);");
    for path in CALL_PARSER_MODULES {
        assert!(violations(path, &call).is_empty(), "{path}");
    }
    for path in ["src/a.rs", "src/session.rs", "tests/a.rs", "src/a/tests.rs"] {
        let v = violations(path, &call);
        assert_eq!(v.len(), 1, "{path}: {v:?}");
        assert!(v[0].contains("parses a call stanza"), "{v:?}");
    }
    // The registration: the builder module may, nothing else may.
    let call = format!("let h = client.{register}x);");
    assert!(violations("src/session.rs", &call).is_empty());
    for path in [
        "src/a.rs",
        "src/calls.rs",
        "src/session/client.rs",
        "tests/a.rs",
    ] {
        let v = violations(path, &call);
        assert_eq!(v.len(), 1, "{path}: {v:?}");
        assert!(v[0].contains("registers a stanza interceptor"), "{v:?}");
    }
    // The names of an answer: no file at all (the planted call of the brief's test).
    for name in call_answer_names() {
        let call = if name.ends_with('(') {
            format!("let r = {name}x);")
        } else {
            format!("let r = {name}(x);")
        };
        for path in ["src/a.rs", "src/calls.rs", "src/session.rs", "tests/a.rs"] {
            let v = violations(path, &call);
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
            assert!(v[0].contains("nothing is ever sent for a call"), "{v:?}");
        }
    }
    // A comment may name them (the docs do), and a longer name is not the name.
    let named = format!("// {}\n/// {parser}", call_answer_names()[0]);
    assert!(violations("src/a.rs", &named).is_empty());
    assert!(
        violations(
            "src/a.rs",
            &format!("let x = {}ed(y);", call_answer_names()[2])
        )
        .is_empty()
    );
    assert!(violations("src/a.rs", &format!("let x = my_{parser}(y);")).is_empty());
}

/// W2c: upstream's builder of the pair error is named by the claim's module only (test files included).
#[test]
fn the_source_check_keeps_the_pair_error_builder_in_the_claim() {
    let name = pair_error_name();
    let call = format!("let n = PairUtils::{name}(id, 500, \"x\");");
    assert!(violations("src/prepair.rs", &call).is_empty());
    for path in [
        "src/a.rs",
        "src/session/client.rs",
        "src/prepair/tests.rs",
        "tests/a.rs",
    ] {
        let v = violations(path, &call);
        assert_eq!(v.len(), 1, "{path}: {v:?}");
        assert!(v[0].contains("builds a pair error"), "{v:?}");
    }
    // A comment may name it (the docs do), and a longer name is not the name.
    assert!(violations("src/a.rs", &format!("// {name}")).is_empty());
    assert!(violations("src/a.rs", &format!("let x = my_{name}(y);")).is_empty());
}

/// W2c L4: an ignored test of `tests/prepair.rs` is refused (the control stays with the refusals), and
/// the real file has none. Mutant: `prepair_ignore_violations` returning nothing.
#[test]
fn the_source_check_refuses_an_ignored_pre_pair_wire_test() {
    for ignored in [
        "#[ignore]\nfn a() {}",
        "#[test]\n  #[ignore = \"slow\"]\nfn a() {}",
        "#[cfg_attr(not(slow), ignore)]\nfn a() {}",
    ] {
        let v = violations("tests/prepair.rs", ignored);
        assert_eq!(v.len(), 1, "{ignored}: {v:?}");
        assert!(v[0].contains("ignores a test"), "{v:?}");
    }
    // Another file may ignore a measurement; a comment and a name that only contains the word are no attribute.
    assert!(violations("tests/a.rs", "#[ignore]\nfn a() {}").is_empty());
    assert!(violations("tests/prepair.rs", "// #[ignore]\nfn a() {}").is_empty());
    assert!(violations("tests/prepair.rs", "#[test]\nfn ignored_nothing() {}").is_empty());
    // The real file: no test of it is ignored.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(root.join("tests/prepair.rs")).expect("read");
    assert!(
        violations("tests/prepair.rs", &text)
            .iter()
            .all(|v| !v.contains("ignores a test")),
        "tests/prepair.rs ignores a test"
    );
}

/// The stanza interceptor is registered once, in the builder module, and the client module calls that
/// before it starts the bot: a registration after `spawn` would miss the calls of the offline queue and let
/// a pairing reach upstream's own handler unjudged, and a client whose `Running` lost the handle would hand
/// the next call to upstream's own handler and the next pairing too (W2c: the pairing's claim and the call's
/// are ONE interceptor).
#[test]
fn the_stanza_interceptor_is_registered_once_before_the_client_starts_and_its_handle_is_kept() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let code_of = |file: &str| {
        std::fs::read_to_string(root.join(file))
            .expect("read")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let session = code_of("src/session.rs");
    let register = interceptor_name();
    assert_eq!(
        session.matches(register.as_str()).count(),
        1,
        "src/session.rs registers the interceptor {} times",
        session.matches(register.as_str()).count()
    );
    let client = code_of("src/session/client.rs");
    let register_call = "register_stanzas(";
    assert_eq!(
        client.matches(register_call).count(),
        1,
        "src/session/client.rs"
    );
    let registered = client.find(register_call).expect("registered");
    let spawned = client.find("bot.spawn()").expect("spawned");
    assert!(
        registered < spawned,
        "the interceptor is registered after the bot started"
    );
    let built = client.find("build_bot(").expect("built");
    assert!(
        built < registered,
        "the interceptor is registered before the bot is built"
    );
    // The handle lives in the running client (dropping it un-registers the interceptor).
    assert!(
        client.contains("stanzas: InterceptorHandle"),
        "the running client does not keep the interceptor's handle"
    );
    assert!(
        client.contains("stanzas,\n"),
        "the handle is not put into `Running`"
    );
}

/// Fix F10 (privacy review LOW 3): the debug switch of the call claim is pinned at its release arm. Every
/// test is a debug build, where `cfg!(debug_assertions)` is true anyway, so a call site that hands the build
/// kind in (`claim(true, ..)`) would pass every test. `calls::claim` therefore takes the switch's text only
/// and reads the kind itself; this check holds that shape: `claim` has one parameter and passes
/// `cfg!(debug_assertions)` to `claim_for_build`, the private rule that takes a kind is named in `src/calls.rs` and
/// nowhere else in `src/`, and the one call of `claim` outside the module passes the switch's text alone.
#[test]
fn the_call_claim_reads_the_build_kind_itself() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let code_of = |file: &str| {
        std::fs::read_to_string(root.join(file))
            .expect("read")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let calls = code_of("src/calls.rs");
    let claim = calls
        .find("pub(crate) fn claim(")
        .map(|at| &calls[at..])
        .expect("calls::claim");
    let body = &claim[..claim.find("\n}\n").expect("the end of claim")];
    assert!(
        body.starts_with("pub(crate) fn claim(env: Option<&str>) -> Claim {"),
        "claim takes the switch's text only: {body}"
    );
    assert!(
        body.contains("claim_for_build(cfg!(debug_assertions), env)"),
        "claim reads the build kind itself: {body}"
    );
    assert_eq!(
        calls.matches("cfg!(debug_assertions)").count(),
        1,
        "the build kind is read in one place"
    );
    // The rule that takes a kind is private to the calls module.
    assert!(
        calls.contains("\nfn claim_for_build("),
        "claim_for_build is private"
    );
    let mut callers = Vec::new();
    for dir in ["src", "src/session", "src/actor"] {
        for entry in std::fs::read_dir(root.join(dir)).expect("dir").flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "rs") {
                let rel = path.strip_prefix(root).expect("rel").display().to_string();
                let code = code_of(&rel);
                if rel != "src/calls.rs" && code.contains("claim_for_build(") {
                    callers.push(rel.clone());
                }
                if code.contains("calls::claim(") {
                    let at = code.find("calls::claim(").expect("call");
                    let argument = &code[at + "calls::claim(".len()..];
                    let argument = &argument[..argument.find(");").expect("the end of the call")];
                    assert!(
                        argument.contains("calls::BUILTIN_ENV") && !argument.contains("debug"),
                        "{rel} passes more than the switch's text to claim: {argument}"
                    );
                }
            }
        }
    }
    assert!(callers.is_empty(), "{callers:?} name the private rule");
}

/// The names of upstream's send shortcuts Kourier never calls (decision D2): each mints or borrows an id, or
/// keeps no retry-cache row for it.
fn send_shortcut_names() -> [String; 5] {
    [
        ["edit", "_message"].concat(),
        ["revoke", "_message"].concat(),
        ["send", "_reaction"].concat(),
        ["send", "_text"].concat(),
        ["send", "_message"].concat(),
    ]
}

/// The send call sites of a code text (comment lines left out): every `SendRequest::new(`, `Client::send(`,
/// `client.send(` and `.send(SendRequest`. A oneshot's `reply.send(` is none.
fn send_call_sites(code: &str) -> usize {
    [
        ["Send", "Request::new("].concat(),
        ["Client::", "send("].concat(),
        ["client.", "send("].concat(),
        [".send(", "SendRequest"].concat(),
    ]
    .iter()
    .map(|needle| code.matches(needle.as_str()).count())
    .sum()
}

/// W5a decision D2 (critic L2): every kind leaves through ONE `Client::send` with the core's local as the id,
/// in `src/session/client.rs`, and nothing in `src/` names upstream's shortcuts (`edit_message` and its kin).
#[test]
fn one_send_call_site() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    let mut sites = Vec::new();
    let mut shortcuts = Vec::new();
    for path in files {
        let rel = path.strip_prefix(root).expect("rel").display().to_string();
        if is_test_file(&rel) {
            continue;
        }
        let code = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let found = send_call_sites(&code);
        if found > 0 {
            sites.push((rel.clone(), found));
        }
        for name in send_shortcut_names() {
            if names(&code, &name) {
                shortcuts.push(format!("{rel} names {name}"));
            }
        }
    }
    // One request built and one `client.send(` of it: two matches, both in the session's client.
    assert!(
        sites == vec![(String::from("src/session/client.rs"), 2)],
        "the send call sites of src/ are {sites:?}, not the one of src/session/client.rs"
    );
    assert!(shortcuts.is_empty(), "{shortcuts:?}");
    let client = std::fs::read_to_string(root.join("src/session/client.rs")).expect("read");
    let request = client
        .find(&["Send", "Request::new("].concat())
        .expect("the request");
    let line = &client[client[..request].rfind("let ").expect("the binding")..];
    let line = &line[..line.find(';').expect("the end of the binding")];
    let options = &client[client[..request]
        .rfind("SendOptions::default()")
        .expect("options")..request];
    assert!(
        options.contains(".with_message_id(id)") && line.contains(".with_options(options)"),
        "the one send does not name the core's local as its id"
    );
}

#[test]
fn the_send_check_counts_what_it_is_for() {
    assert_eq!(send_call_sites("let _ = reply.send(answer);"), 0);
    assert_eq!(send_call_sites("tx.send(request_ish)"), 0);
    assert_eq!(
        send_call_sites("let r = SendRequest::new(&to, m); client.send(r).await"),
        2
    );
    assert_eq!(send_call_sites("whatsapp_rust::Client::send(&c, r)"), 1);
    assert_eq!(
        send_call_sites("bot.client().send(SendRequest::new(&to, m))"),
        2
    );
}

/// W5b decision G14: the upload is upstream's `upload_stream`, called in ONE place, `src/session/client.rs`, and
/// only as a task of the client's runtime that is ABORTED when its caller goes (`on_runtime_or_abort`, whose
/// `Aborting` guard aborts on drop): a stall, a stop or a quit drops the attempt's future, and a task left running
/// on the client's runtime would go on reading the source and posting after the attempt ended. A plain
/// `on_runtime` (the spawned task outlives its caller) fails this.
#[test]
fn the_upload_runs_only_as_a_task_that_ends_with_its_caller() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    let mut sites = Vec::new();
    let needle = ["upload", "_stream("].concat();
    for path in files {
        let rel = path.strip_prefix(root).expect("rel").display().to_string();
        if is_test_file(&rel) {
            continue;
        }
        let code = std::fs::read_to_string(&path).expect("read");
        for line in code.lines() {
            if !line.trim_start().starts_with("//") && line.contains(&needle) {
                sites.push(rel.clone());
            }
        }
    }
    assert_eq!(
        sites,
        vec![String::from("src/session/client.rs")],
        "the upload's call sites in src/"
    );
    let client = std::fs::read_to_string(root.join("src/session/client.rs")).expect("read");
    let code: String = client
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let at = code.find(&needle).expect("the call");
    let before = &code[..at];
    let task = before
        .rfind(".on_runtime")
        .expect("the upload runs as a task of the client's runtime");
    assert!(
        before[task..].starts_with(".on_runtime_or_abort(async move {")
            && !before[task..].contains(';'),
        "the upload is not inside `on_runtime_or_abort`"
    );
    let helper = &code[code
        .find("async fn on_runtime_or_abort")
        .expect("the helper")..];
    let helper = &helper[..helper.find("\n    }").expect("its end")];
    assert!(
        helper.contains("Aborting(") && helper.contains(".spawn(work)"),
        "the helper does not keep its task in the aborting guard"
    );
    let guard = &code[code
        .find("impl<T> Drop for Aborting<T>")
        .expect("the guard's drop")..];
    let guard = &guard[..guard.find("\n}").expect("its end")];
    assert!(
        guard.contains("self.0.abort();"),
        "the guard does not abort"
    );
}

/// The names of upstream's reads of the groups and of the blocklist (W6a2, B3, B6, D9, D22, M1), each built from two pieces so
/// that this file does not name them: the group overview, the batch of whole group metadata, the single query of one
/// group, the blocklist.
fn group_read_names() -> [String; 4] {
    [
        ["list_", "participating("].concat(),
        ["fetch_metadata_", "batch("].concat(),
        ["fetch_", "metadata("].concat(),
        ["get_", "blocklist("].concat(),
    ]
}

/// The call sites of `needle` in the code of `text` (comment lines left out).
fn call_sites_of(text: &str, needle: &str) -> usize {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .map(|line| line.matches(needle).count())
        .sum()
}

/// W6a2 (B3, M1): each of upstream's reads of the groups and of the blocklist has ONE call site in `src/`, in
/// `src/session/client.rs` (the `Session` methods `participating`, `fetch_groups`, `fetch_group` and `blocklist` are the ways
/// to them), so the fetch policy of B6 is the only thing that decides how often a group IQ goes out. And (H4, D7) NOTHING in this crate,
/// tests included, calls upstream's `add_lid_pn_mapping`: it spawns a usync reconcile of its own and makes Kourier the
/// authority of a row upstream routes sends by; the merges Kourier states come from the store's pairs it vouches for.
#[test]
fn the_group_and_blocklist_reads_have_one_call_site_each_and_the_pair_writer_has_none() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files);
    collect(&root.join("tests"), &mut files);
    let writer = ["add_lid_", "pn_mapping"].concat();
    let mut sites: Vec<(String, String, usize)> = Vec::new();
    let mut writers = Vec::new();
    for path in files {
        let rel = path.strip_prefix(root).expect("rel").display().to_string();
        let text = std::fs::read_to_string(&path).expect("read");
        if call_sites_of(&text, &writer) > 0 {
            writers.push(rel.clone());
        }
        if is_test_file(&rel) {
            continue;
        }
        for needle in group_read_names() {
            let found = call_sites_of(&text, &needle);
            if found > 0 {
                sites.push((needle, rel.clone(), found));
            }
        }
    }
    let expected: Vec<_> = group_read_names()
        .into_iter()
        .map(|needle| (needle, "src/session/client.rs".to_owned(), 1))
        .collect();
    sites.sort();
    let mut want = expected;
    want.sort();
    assert_eq!(
        sites, want,
        "the call sites of the group and blocklist reads in src/"
    );
    assert!(
        writers.is_empty(),
        "{writers:?} names the pair writer: Kourier never calls it (W6a2, H4)"
    );
}

#[test]
fn the_group_read_check_counts_what_it_is_for() {
    let batch = ["fetch_metadata_", "batch("].concat();
    assert_eq!(
        call_sites_of(&format!("let r = api.{batch}&groups).await?;"), &batch),
        1
    );
    assert_eq!(call_sites_of(&format!("// api.{batch}&g)"), &batch), 0);
    // The single query's name is no part of the batch's, and the batch's is no part of the single query's.
    let single = ["fetch_", "metadata("].concat();
    assert_eq!(
        call_sites_of(&format!("api.{single}&group).await?"), &single),
        1
    );
    assert_eq!(call_sites_of(&format!("api.{batch}&groups)"), &single), 0);
    assert_eq!(call_sites_of(&format!("api.{single}&group)"), &batch), 0);
    assert_eq!(call_sites_of("session.fetch_groups(g).await", &batch), 0);
    let writer = ["add_lid_", "pn_mapping"].concat();
    assert_eq!(
        call_sites_of(&format!("client.{writer}(&l, &p).await"), &writer),
        1
    );
}
