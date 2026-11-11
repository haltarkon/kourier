//! OFFLINE BY CONSTRUCTION: the source check of the Threema crate (WhatsApp's form, Threema's names).
//!
//! 1. The traits a client is made with are this crate's own (`src/parts.rs`); only ONE module,
//!    the directory [`DIAL_PREFIX`] (TH3 call C6; `src/dial.rs` must not exist), may name a socket type (TCP, UDP, Unix), a WebSocket, TLS or HTTP crate, the
//!    name resolver or a Threema host ([`network_names`]), in `src/` and `tests/` alike (comments
//!    included). The brace forms count (a group below the standard library's or tokio's root names
//!    the same module as the plain path): [`flat_paths`] writes a group `a::{b, c::{d}}` out as the
//!    paths `a::b` and `a::c::d` before the names are looked for. The crate needles are a fixed list
//!    AND every package of the workspace's `Cargo.lock` whose name has an HTTP, TLS, WebSocket,
//!    QUIC or socket word in it ([`lock_names`], TH3 fix S-M4): a crate the lock holds already
//!    (through another backend) must not be nameable outside the dial module at the cost of one
//!    crate-local dependency line.
//!    Every `.rs` file under the crate root is scanned (`build.rs`, `examples/` and `benches/` too),
//!    `target/` aside.
//! 2. The application's entry to the real parts ([`entry_names`]) is named only by that module and by
//!    `kourier-app`'s `src/` ([`ENTRY_PREFIXES`]): scanned over every file of this crate, of every
//!    other crate and of xtask.
//! 3. Test code that makes a network hands it the guards (`guards::`) or the offline fake
//!    (`mock::`, TH2), never a variable that could hold anything.
//! 4. A TEST FUNCTION that spawns an actor (itself, or through a function of its file that does)
//!    runs it in an `unshare -rn` child: it calls `offline::run_child(`, `offline::run_child_output(`
//!    or `offline::child_command(` (the parent), or begins the child with `offline::in_child()`.
//!    Checked per function, not per file: a local helper that happens to be called `run_child` does
//!    not count. A test that calls the actor tests' harness ([`HARNESS_START`], TH3 critic M2: the
//!    spawn lives in `tests/connection/harness.rs`) spawns an actor too.
//! 5. Test code is: a file outside `src/`, `src/**/tests.rs`, anything below a `tests/` directory of
//!    `src/`, and the `#[cfg(test)]` blocks of the other files of `src/`.
//! 6. THE MERGE GUARD: no file of this crate names WhatsApp's entry to its real parts
//!    ([`whatsapp_entry_names`]); WhatsApp's own check scans every other crate for them, so a Threema
//!    file that named one would fail WhatsApp's tests after the lanes merge.
//! 7. THE COMMAND MATCH OF THE ACTOR (`src/actor.rs`, `match command {`) has no wildcard arm (`_`, a
//!    bare binding, `mut x`, `ref x`, `x @ _`) and no `{ .. }` in a pattern, lane decision M3: a new
//!    `Command` variant must not compile there until it has an arm of its own, and a field a variant
//!    gains later (a `reply`) must not compile there until the arm names it, so a reply is never
//!    dropped silently.
//! 8. libthreema is given NO feature, ever (`cli` would bring a real HTTPS and WebSocket client,
//!    `uniffi` and `wasm` their bindings): no `libthreema` dependency entry of any manifest of the
//!    workspace has a `features` key, no `[features]` item forwards one of libthreema's features
//!    (`libthreema/x`, `libthreema?/x`), the root's row has `default-features = false`, and the
//!    vendored manifest's `default` feature reaches none of `cli`, `uniffi`, `wasm`
//!    ([`libthreema_dependency_violations`], [`libthreema_default_violations`]). `deny.toml`'s ban on
//!    the OpenSSL crates stays a second line.
//! 9. THE OFFLINE FAKE (`tests/support/mock/`, TH2) names no socket type ([`network_names`], with no
//!    exemption: the dial module's licence never reaches it) and takes from libthreema ONLY its
//!    generated protobuf types (`libthreema::protobuf::...`): never its protocol, crypto or task
//!    code, so the real client against the fake stays a check by two implementations (lane plan
//!    §2.4). Refused: any other `libthreema::` path (brace groups written out, `::libthreema`
//!    too), `libthreema as`, `extern crate libthreema`, a `crate::` path outside
//!    `crate::support::mock::` and the named [`MOCK_MAY_USE`] (the test modules use libthreema's
//!    client; going through them, or through a re-export in `tests/support/`, would launder it), a
//!    `super::super` path in a file of the fake (one `super::` reaches the fake's own `mod.rs`, two
//!    leave the directory; `mod.rs` itself takes no `super::`), `#[path` and `include!` (and
//!    `include_str!`/`include_bytes!` of a `.rs` file), Kourier's own join code
//!    (`kourier_threema::join`, `kourier_threema::proto`: TH4's fake decodes the QR with its own
//!    struct), the follower's (`kourier_threema::reflect`: TH5's phone writes the envelopes and bodies
//!    from the spec) and the send core's (`kourier_threema::send`: TH6a's peer opens every box on its own
//!    crypto and the phone builds every body from the spec); and in EVERY file under `tests/support/` a
//!    `pub use` or `pub type` of anything of libthreema but its protobuf types
//!    ([`mock_violations`], [`support_reexport_violations`]).
//! 10. THE REQUEST FOR MESSAGES LIVES IN THE LEADER (TH3, TH6 L4b): no file of `src/` but [`LEADER_PREFIX`] names
//!     the CSP payload that asks the chat server to deliver ([`unblock_name`], comments included): the leader's
//!     stage builds it (`leader::unblock_payload`) and the connection only passes `CspSend::Unblock` on, once per
//!     socket, when it leads and can take messages; `src/conn/csp.rs` itself does not name it (the test reads the real
//!     files); `tests/` may build one for a fake test.
//! 11. THE DIAL MODULE'S OWN TESTS (TH3, [`dial_test_violations`]): the test code of `src/dial/`
//!     (its `tests.rs`, `tests/`, `#[cfg(test)]` blocks) names no socket or resolver
//!     ([`socket_names`]), no entry to the real parts and no real dial ([`dial_test_names`]): those
//!     tests reach the connectors only over `tokio::io::duplex`, in an `unshare -rn` child. THE
//!     METHOD FORMS ARE CLOSED TOO (TH3 fix S-M1, [`dial_function_violations`]): the real
//!     connectors' constructors `production(` are not called in a dial test (the data of the
//!     production pins, verifier and TLS configurations are: [`PRODUCTION_DATA`]; a connector is
//!     built with `new(.., test_pins())`, and a function that names one does not name the
//!     production pin set); every test function but the parent ([`DIAL_PARENT`]) is named
//!     `offline_*` and begins with the `in_child()` guard, so no dial test can run in the parent
//!     process, outside the `unshare -rn` child.
//! 12. ONLY THE CONNECTION AND THE JOIN DIAL (TH4 J3, [`connect_violations`]): outside [`DIALLERS`]
//!     (`src/conn/`, `src/join/`) no production code of `src/` opens a WebSocket through the parts'
//!     connector ([`WS_CONNECT`]: `WsConnector::connect`, `.connect(WsRequest`, `.ws.connect(`);
//!     the actor makes the transports and hands them on, it never dials itself. Comment lines and the
//!     `#[cfg(test)]` blocks are left out (a unit test may call a connector it built).
//! 13. THE TEST DOORS STAY IN THE CRATE (TH4 round B, S5, [`door_violations`]): the doc-hidden
//!     `pub` items that exist for the crate's own integration tests (`Import::checked_for_tests`,
//!     `store::import_join`, `ready_join`, `mark_join_ready`, `is_join_ready`, `stated_events`,
//!     `import_events`, `record_link`, `set_slot_existing`, `add_nonce_batches`, `emoji_of_path_hash`: they write a
//!     joined account, or state it to the core, without the rules of `src/actor.rs` (allow-list, relink,
//!     taken); TH5 A4: `add_deferred`, `add_deferred_batch`, `deferred_page`, `delete_deferred`,
//!     `set_deferred_version`, `deferred_total`, `expire_deferred`, `remap_deferred_steps`: they write or
//!     read the plaintext `deferred` rows outside the reflection loop and the re-map; TH5 A5a: `reflected_zones`,
//!     `fail_deferred_of_length`: they count the reflection loop's zones and make one fail (with a SQLite code
//!     of the test's choice, round A); TH5 A8: `remap_with`: it runs the re-map under a version of the test's
//!     choice; round A (S5): `remap_with_cancel`: the same under the actor's cancel token; TH5b B5: `fail_deferred_of_lengths`
//!     (R16, N-2: the door with two (length, code) pairs, so one page fails by its content and by the store) and
//!     `clear_deferred` (R1: the user's clear, which deletes plaintext rows and states `CommandPersisted`); TH6a S1: `prepare_send`,
//!     `prepare_send_zone`, `mark_send_reflected`, `mark_send_acks`, `mark_send_done`, `count_failed_run`, `give_up_send_job`, `sweep_sends`, `next_send`,
//!     `send_task`, `send_ledger`, `send_tasks`, `send_receivers`: they write or read the send queue's sealed rows outside the send core; and
//!     `fail_send_zone` (a send zone of the test's choice fails with a SQLite code of its choice); TH6 L1: `receive_inbound`, `mark_inbound_reflected`, `sweep_inbound`, `inbound_row`,
//!     `inbound_rows`, `fail_inbound_zone`: they write or read the leader's `csp_inbound` rows outside the socket's leader
//!     stage, or make one of its zones fail) are named ONLY under this crate's `src/` and `tests/` (and its build script): no file of
//!     any other crate or of xtask names one of them ([`DOORS`], whole names, comments included: the
//!     names belong to this crate alone), none names the module path `kourier_threema::store` (the brace
//!     forms written out by [`flat_paths`]), and no other manifest renames the package
//!     (`package = "kourier-threema"` would hide the crate's name from the scan). What another crate may
//!     use is the crate's root API: `ThreemaNetwork`, `ThreemaParts`, `LINKING_BUILT`, `logs`
//!     and the dial module's one entry (rule 2).
//! 14. THE P5 CRYPTO HAS ONE HOME (TH5 A10, [`crypto_violations`]): in the production code of `src/`
//!     (comment lines and `#[cfg(test)]` blocks left out; every file under a `tests/` directory is
//!     test code) only [`REFLECT_PREFIX`] (`src/reflect/`) names libthreema's patch P5 open and seal
//!     functions ([`P5_BOXES`]: `open_reflected`, `seal_reflected`, `open_transaction_scope`,
//!     `seal_transaction_scope`, `open_device_info` and the four CSP boxes `encrypt_message`,
//!     `decrypt_message`, `encrypt_metadata`, `decrypt_metadata`), and only [`REFLECT_PREFIX`] and
//!     [`STORE_FILE`] (`src/store.rs`) name `hash_with` (the nonce rule: the stores keep the nonce
//!     hashed with the own ID, made in one place). Whole names; `src/reflect.rs` or `src/reflection.rs`
//!     is not the directory. TH6a (S2) gives the SEND side ONE file besides: [`SEND_ENCODE_FILE`]
//!     (`src/send/encode.rs`, that path exactly, not a directory or a longer name) may name the three
//!     SEALS of a sent message ([`SEND_BOXES`]: `seal_reflected`, `encrypt_message`,
//!     `encrypt_metadata`) and nothing else of the list: every open and decrypt stays refused in
//!     `src/send/`, and `hash_with` is not named there (the send core hashes through the keys of
//!     `src/reflect/`). TH6 (L3) gives the LEADER'S RECEIVE its own list: under [`LEADER_PREFIX`]
//!     (`src/leader/`, the directory) the two OPENS of an incoming CSP message ([`LEADER_BOXES`]:
//!     `decrypt_message`, `decrypt_metadata`), read with the sender's key; every seal and every other open
//!     stays refused there, and `hash_with` too (the leader hashes through `src/reflect/`'s keys); the opens
//!     stay refused everywhere else (`src/send/`, `src/conn/`, `src/leader.rs`). The transactions (TH9) widen
//!     the list on purpose, with their own prefix, when they need a box.
//! 15. (the send core's test door: gone with `CannotSend::NotYetBuilt`, TH8).
//!
//! 16. ONLY THE DIRECTORY TALKS HTTPS (TH6 L4b, [`https_violations`]): in the production code of `src/` (comment
//!     lines and `#[cfg(test)]` blocks left out) only [`HTTPS_HOMES`] name the parts' HTTPS client ([`https_names`]:
//!     the trait, a call through the parts' field, a call with a request, and (TH6 FIX-B, P-L8) the request type
//!     itself, so a call through a local binding of the client, `c.execute(req)`, still shows where `req` is built):
//!     `src/leader/directory.rs` (the leader's
//!     `fetch_bulk`, TH6 L5), `src/parts.rs` (the trait), `src/lib.rs` (its re-export) and the dial module (the real
//!     client). So no other module talks to a Threema host by HTTPS, whatever the allow-list gate lets through.
//!
//! Names are assembled at run time with `concat()`, so this file does not contain them.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The directory (relative to the crate root) that may name the real network: the ONE module that
/// makes the application's parts (TH3 call C6: a directory; `src/dial.rs` must not exist), and
/// nothing else.
const DIAL_PREFIX: &str = "src/dial/";

/// Whether `path` (relative to the crate root) is a file of the dial module.
fn is_dial(path: &str) -> bool {
    path.starts_with(DIAL_PREFIX)
}

/// The one place of `src/` that may name the request for messages (rule 10, TH6).
const LEADER_PREFIX: &str = "src/leader/";

/// The CSP payload that asks the chat server to start delivering (rule 10).
fn unblock_name() -> String {
    ["Unblock", "IncomingMessages"].concat()
}

/// Rule 10: a file of `src/` outside [`LEADER_PREFIX`] that names the request for messages.
fn unblock_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with("src/") || path.starts_with(LEADER_PREFIX) {
        return Vec::new();
    }
    let name = unblock_name();
    if names(text, &name) {
        vec![format!(
            "{path} names {name}; only {LEADER_PREFIX} may (rule 10)"
        )]
    } else {
        Vec::new()
    }
}

/// The files of `src/` that may name the parts' HTTPS client (rule 16): the directory's call, the trait, its
/// re-export; the dial module (the real client) is [`DIAL_PREFIX`].
const HTTPS_HOMES: &[&str] = &[
    "src/leader/directory.rs",
    "src/parts.rs",
    "src/lib.rs",
    "src/blob.rs",
];

/// The names of the parts' HTTPS client (rule 16): the trait, a call through the parts' field, a call with a request,
/// and the request type (P-L8: building a request is the act; a call through a local binding names no field).
fn https_names() -> [String; 4] {
    [
        ["Https", "Client"].concat(),
        ["https", ".execute"].concat(),
        ["execute(", "HttpsRequest"].concat(),
        ["Https", "Request"].concat(),
    ]
}

/// Rule 16: production code of `src/` outside [`HTTPS_HOMES`] and [`DIAL_PREFIX`] that names the HTTPS client.
fn https_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with("src/") || is_dial(path) || HTTPS_HOMES.contains(&path) {
        return Vec::new();
    }
    let code = production_code(path, text);
    https_names()
        .iter()
        .filter(|name| names(&code, name))
        .map(|name| {
            format!("{path} names {name}; only {HTTPS_HOMES:?} and {DIAL_PREFIX} may (rule 16)")
        })
        .collect()
}

/// What a test of the actor tests' directories calls to spawn an actor (rule 4, TH3 critic M2).
const HARNESS_START: &str = "harness::start(";
/// Where the actor tests and their harnesses live: the connection (TH3) and the join (TH4 J3).
const HARNESS_DIRS: [&str; 2] = ["tests/connection/", "tests/join/"];

/// The directories of `src/` whose code may open a WebSocket through the parts' connector (rule 12):
/// the connection (the mediator) and the join (the rendezvous relay). The actor never dials itself.
const DIALLERS: [&str; 2] = ["src/conn/", "src/join/"];

/// What opening a WebSocket through the parts looks like (rule 12).
const WS_CONNECT: [&str; 3] = ["WsConnector::connect", ".connect(WsRequest", ".ws.connect("];

/// The code of `path` outside its tests (comment lines left out): nothing of a test file, and of
/// the other files of `src/` everything but the `#[cfg(test)]` blocks (rule 5's split).
fn production_code(path: &str, text: &str) -> String {
    if is_test_file(path) {
        return String::new();
    }
    let mut code = without_comments(text);
    let attribute = "#[cfg(test)]";
    while let Some(at) = code.find(attribute) {
        let rest = &code[at + attribute.len()..];
        let end = match (rest.find('{'), rest.find(';')) {
            (Some(open), semicolon) if semicolon.is_none_or(|s| s > open) => {
                let offset = at + attribute.len() + open;
                offset + block_after(&code, offset).len() + 2
            }
            (_, Some(semicolon)) => at + attribute.len() + semicolon + 1,
            (_, None) => code.len(),
        };
        code.replace_range(at..end.min(code.len()), "");
    }
    code
}

/// Rule 12: a file of `src/` outside [`DIALLERS`] whose production code opens a WebSocket through the
/// parts' connector.
fn connect_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with("src/") || DIALLERS.iter().any(|d| path.starts_with(d)) {
        return Vec::new();
    }
    let code = production_code(path, text);
    WS_CONNECT
        .iter()
        .filter(|needle| code.contains(**needle))
        .map(|needle| {
            format!("{path} opens a WebSocket ({needle}); only {DIALLERS:?} may (rule 12)")
        })
        .collect()
}

/// The doc-hidden `pub` doors of this crate that exist for its integration tests (rule 13, S5). Whole
/// names; each is unique to this crate in the workspace (a test checks that the others do not use it).
const DOORS: [&str; 51] = [
    "checked_for_tests",
    "import_join",
    "ready_join",
    "mark_join_ready",
    "is_join_ready",
    "stated_events",
    "import_events",
    "record_link",
    "set_slot_existing",
    "add_nonce_batches",
    "emoji_of_path_hash",
    "add_deferred",
    "add_deferred_batch",
    "deferred_page",
    "delete_deferred",
    "set_deferred_version",
    "deferred_total",
    "expire_deferred",
    "remap_deferred_steps",
    "reflected_zones",
    "fail_deferred_of_length",
    "remap_with",
    "remap_with_cancel",
    "fail_deferred_of_lengths",
    "clear_deferred",
    "prepare_send",
    "prepare_send_zone",
    "mark_send_reflected",
    "mark_send_acks",
    "mark_send_done",
    "count_failed_run",
    "give_up_send_job",
    "sweep_sends",
    "next_send",
    "send_task",
    "send_ledger",
    "send_tasks",
    "send_receivers",
    "fail_send_zone",
    "seal_outgoing",
    "prepare_outgoing",
    "seal_sent_update",
    "enqueue_job",
    "receive_inbound",
    "mark_inbound_reflected",
    "sweep_inbound",
    "inbound_row",
    "inbound_rows",
    "fail_inbound_zone",
    "open_inbound",
    "write_body",
];

/// The doc-hidden module whose path no other crate names (rule 13).
fn store_path() -> String {
    ["kourier_threema", "::store"].concat()
}

/// Rule 13: a file of ANOTHER crate (`../<crate>/...`, xtask `../../xtask/...`) that names one of the
/// [`DOORS`], or the path `kourier_threema::store` (also written as a brace group). This crate's own
/// files are not looked at.
fn door_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with("../") {
        return Vec::new();
    }
    let flat = flat_paths(text);
    let store = store_path();
    let mut found: Vec<String> = DOORS
        .iter()
        .filter(|name| names(text, name))
        .map(|name| {
            format!(
                "{path} names the test door {name}; only this crate's src/ and tests/ may (rule 13)"
            )
        })
        .collect();
    if names(text, &store) || flat.iter().any(|p| names(p, &store)) {
        found.push(format!(
            "{path} names {store}; only this crate's src/ and tests/ may (rule 13)"
        ));
    }
    found
}

/// Rule 13: a manifest of another crate that renames the package (the crate name would not be in
/// its sources).
fn rename_violations(path: &str, text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| {
            let line = line.split('#').next().unwrap_or_default();
            line.contains("package") && line.contains("kourier-threema")
        })
        .map(|_| {
            format!(
                "{path} renames the package kourier-threema; the scan of rule 13 would not see it"
            )
        })
        .collect()
}

/// The one directory of `src/` whose production code may name the P5 crypto (rule 14, TH5 A10): the
/// follower's receive path opens what the phone reflected and hashes its nonces there.
const REFLECT_PREFIX: &str = "src/reflect/";

/// The one file besides [`REFLECT_PREFIX`] that may name `hash_with` (rule 14): the store keeps the
/// hashed nonces.
const STORE_FILE: &str = "src/store.rs";

/// libthreema's patch P5 open and seal functions (rule 14, whole names): the reflection box (DGRK),
/// the transaction scope box (DGTSK), the device info box (DGDIK), and the CSP boxes of a message
/// and of its metadata.
const P5_BOXES: [&str; 9] = [
    "open_reflected",
    "seal_reflected",
    "open_transaction_scope",
    "seal_transaction_scope",
    "open_device_info",
    "encrypt_message",
    "decrypt_message",
    "encrypt_metadata",
    "decrypt_metadata",
];

/// The nonce hash of patch P5 (rule 14): `Nonce::hash_with`.
const NONCE_HASH: &str = "hash_with";

/// The one file besides [`REFLECT_PREFIX`] that may name the seals of a sent message (rule 14, TH6a S2): that
/// path exactly. The pure encode of the send core.
const SEND_ENCODE_FILE: &str = "src/send/encode.rs";

/// The P5 boxes [`SEND_ENCODE_FILE`] may name (rule 14): the SEALS of a sent message, the device group's reflect
/// box and the two CSP boxes. Every open and decrypt of [`P5_BOXES`] stays out of `src/send/`.
const SEND_BOXES: &[&str] = &["seal_reflected", "encrypt_message", "encrypt_metadata"];

/// The P5 boxes [`LEADER_PREFIX`] may name (rule 14, TH6 L3): the two OPENS of an incoming CSP message, the leader's
/// steps 6 and 7 with the sender's key. Every seal and every other open of [`P5_BOXES`] stays out of `src/leader/`.
const LEADER_BOXES: &[&str] = &["decrypt_message", "decrypt_metadata"];

/// Rule 14: a file of `src/` outside [`REFLECT_PREFIX`] whose production code names a P5 box (but
/// [`SEND_ENCODE_FILE`], which may name [`SEND_BOXES`]), or outside [`REFLECT_PREFIX`] and [`STORE_FILE`]
/// names `hash_with`. Comment lines and `#[cfg(test)]` blocks are not production code; a file below a
/// `tests/` directory is none at all.
fn crypto_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with("src/") || path.starts_with(REFLECT_PREFIX) {
        return Vec::new();
    }
    let code = production_code(path, text);
    let mut found: Vec<String> = P5_BOXES
        .iter()
        .filter(|name| names(&code, name))
        .filter(|name| !(path == SEND_ENCODE_FILE && SEND_BOXES.contains(name)))
        .filter(|name| !(path.starts_with(LEADER_PREFIX) && LEADER_BOXES.contains(name)))
        .map(|name| {
            format!(
                "{path} names {name}; only {REFLECT_PREFIX} may, {SEND_ENCODE_FILE} the seals of a sent message and {LEADER_PREFIX} the opens of a received one (rule 14)"
            )
        })
        .collect();
    if path != STORE_FILE && names(&code, NONCE_HASH) {
        found.push(format!(
            "{path} names {NONCE_HASH}; only {REFLECT_PREFIX} and {STORE_FILE} may (rule 14)"
        ));
    }
    found
}

/// The file prefixes of OTHER crates that may call the entry to the real parts: the composition
/// root registers the network there (C8).
const ENTRY_PREFIXES: &[&str] = &["../kourier-app/src/"];

/// What only the dial module may name: the WebSocket, HTTP and TLS crates ([`crate_names`]), the
/// socket types, the low-level socket crates and calls, the name resolver, the standard and tokio
/// network modules ([`socket_names`]), and Threema's hosts (libthreema's own `common/config.rs` holds
/// those as data; the vendored tree is not scanned).
fn network_names() -> Vec<String> {
    let mut all = crate_names();
    all.extend(socket_names());
    all.extend([["threema", ".ch"].concat(), ["threema", ".com"].concat()]);
    all
}

/// The WebSocket, HTTP and TLS crates: TH3 L1 (tokio's TLS crate and the Mozilla roots crate by
/// their own names, as the TLS crate's needle does not match after an `_`), TH3 fix S-M4 (the HTTP
/// stack's other crates, the other WebSocket and QUIC crates, the system-roots crates, the other
/// HTTP clients, the event-loop crate), and every package of `Cargo.lock` with such a word in its
/// name ([`lock_names`]).
fn crate_names() -> Vec<String> {
    static ALL: OnceLock<Vec<String>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<String> = [
            ["tokio_tung", "stenite"],
            ["tung", "stenite"],
            ["req", "west"],
            ["rus", "tls"],
            ["tokio_rus", "tls"],
            ["webpki_", "roots"],
            ["hy", "per"],
            ["ur", "eq"],
            ["hyper_", "util"],
            ["hyper_rus", "tls"],
            ["h", "2"],
            ["tokio_web", "sockets"],
            ["async_tung", "stenite"],
            ["m", "io"],
            ["quin", "n"],
            ["rustls_platform_", "verifier"],
            ["rustls_native_", "certs"],
            ["isa", "hc"],
            ["cu", "rl"],
        ]
        .iter()
        .map(|parts| parts.concat())
        .collect();
        all.extend(lock_names());
        // A socket crate is a socket name already (rule 11 keeps it out of the dial tests): not twice.
        let sockets = socket_names();
        all.retain(|name| !sockets.contains(name));
        all.sort();
        all.dedup();
        all
    })
    .clone()
}

/// The packages of the workspace's `Cargo.lock` whose name holds an HTTP, TLS, WebSocket, QUIC or
/// socket word, `-` written `_` (TH3 fix S-M4: derived at run time, so a crate that the lock gets
/// later, or already holds through another backend, is a needle without an edit here). The lock
/// is read whole; a lock that does not parse, or that gives no such package, fails the test.
fn lock_names() -> Vec<String> {
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let text = std::fs::read_to_string(&lock).expect("the workspace's Cargo.lock");
    let table: toml::Table = text.parse().expect("Cargo.lock is TOML");
    let words = [
        ["ht", "tp"].concat(),
        ["hy", "per"].concat(),
        ["tl", "s"].concat(),
        ["tung", "stenite"].concat(),
        ["web", "socket"].concat(),
        ["ur", "eq"].concat(),
        ["qu", "ic"].concat(),
        ["soc", "ket"].concat(),
    ];
    table
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|package| package.get("name").and_then(toml::Value::as_str))
        .filter(|name| words.iter().any(|word| name.contains(word.as_str())))
        .map(|name| name.replace('-', "_"))
        .collect()
}

/// The socket types (TCP, UDP, Unix), the low-level socket crates and calls, the name resolver and
/// the standard and tokio network modules: what rule 1 keeps in the dial module and rule 11 keeps
/// out of the dial module's own tests.
fn socket_names() -> [String; 14] {
    [
        ["Tcp", "Stream"].concat(),
        ["Tcp", "Listener"].concat(),
        ["Udp", "Socket"].concat(),
        ["Unix", "Stream"].concat(),
        ["Unix", "Listener"].concat(),
        ["Unix", "Datagram"].concat(),
        ["lookup_", "host"].concat(),
        ["ToSocket", "Addrs"].concat(),
        ["std::", "net"].concat(),
        ["tokio::", "net"].concat(),
        ["unix::", "net"].concat(),
        ["socket", "2"].concat(),
        ["libc::", "socket"].concat(),
        ["libc::", "connect"].concat(),
    ]
}

/// What the dial module's TEST code may not name besides [`socket_names`] (rule 11): the entry to
/// the real parts and the real connector's dial, so its tests reach only `connect_over` over an
/// in-memory pipe.
fn dial_test_names() -> [String; 2] {
    [
        ["live_", "parts("].concat(),
        ["LiveWs::", "connect("].concat(),
    ]
}

/// The names by which the real parts are asked for (critic L6): the entry and the module's path.
fn entry_names() -> [String; 2] {
    [
        ["live_", "parts"].concat(),
        ["kourier_threema::", "dial"].concat(),
    ]
}

/// WhatsApp's entry to its real parts, as its own check (`kourier-whatsapp/tests/builder.rs`)
/// scans for them in every other crate.
fn whatsapp_entry_names() -> [String; 3] {
    [
        ["net_", "parts"].concat(),
        ["re", "al::"].concat(),
        ["::", "re", "al"].concat(),
    ]
}

/// The call that makes a network.
fn network_call() -> String {
    ["Threema", "Network::new("].concat()
}

/// Whether `text` holds `needle` as a whole name: an identifier character does not touch it where
/// the needle begins or ends with one (a longer name that merely contains the needle is not it).
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

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn skip_blanks(chars: &[char], i: &mut usize) {
    while chars.get(*i).is_some_and(|c| c.is_whitespace()) {
        *i += 1;
    }
}

/// One path from `*i` (an identifier's first character) with `prefix` before it; a group after `::`
/// is written out path by path. Blanks around `::`, `,`, `{` and `}` are ignored; `x as y` keeps `x`.
fn tree(chars: &[char], i: &mut usize, prefix: &str, out: &mut Vec<String>) {
    let mut path = prefix.to_owned();
    loop {
        let start = *i;
        while chars.get(*i).copied().is_some_and(is_ident_char) {
            *i += 1;
        }
        path.extend(&chars[start..*i]);
        skip_blanks(chars, i);
        if chars.get(*i) == Some(&':') && chars.get(*i + 1) == Some(&':') {
            *i += 2;
            path.push_str("::");
            skip_blanks(chars, i);
            if chars.get(*i) == Some(&'{') {
                *i += 1;
                loop {
                    skip_blanks(chars, i);
                    match chars.get(*i) {
                        Some(&c) if c.is_alphabetic() || c == '_' => tree(chars, i, &path, out),
                        Some(&'}') => {
                            *i += 1;
                            return;
                        }
                        // A comma, a glob, a nested brace of another kind: step over it.
                        Some(_) => *i += 1,
                        None => return,
                    }
                }
            }
            continue;
        }
        out.push(path);
        // `as alias`: the alias is not a path.
        if chars.get(*i) == Some(&'a')
            && chars.get(*i + 1) == Some(&'s')
            && chars.get(*i + 2).is_some_and(|c| c.is_whitespace())
        {
            *i += 2;
            skip_blanks(chars, i);
            while chars.get(*i).copied().is_some_and(is_ident_char) {
                *i += 1;
            }
        }
        return;
    }
}

/// Every path a text names, with a group written out (`a::{b, c::d}` gives `a::b` and `a::c::d`).
/// Blanks between the parts of a path are ignored.
fn flat_paths(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let continues = i > 0 && (is_ident_char(chars[i - 1]) || chars[i - 1] == ':');
        if (chars[i].is_alphabetic() || chars[i] == '_') && !continues {
            tree(&chars, &mut i, "", &mut out);
        } else {
            i += 1;
        }
    }
    out
}

/// Whether `path` (relative to the crate root) is test code in whole: everything outside `src/`, and
/// in `src/` the files named `tests.rs` and anything below a `tests/` directory (rule 5).
fn is_test_file(path: &str) -> bool {
    !path.starts_with("src/") || path.ends_with("/tests.rs") || path.contains("/tests/")
}

/// The text without its comment lines.
fn without_comments(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text between the `{` at `open` and its closing brace (exclusive), or to the end.
fn block_after(text: &str, open: usize) -> &str {
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    &text[open + 1..]
}

/// The code of `path` that rules 3 and 4 look at (comment lines left out): a test file's code in
/// whole, and of the other files the `#[cfg(test)]` blocks (an attribute on an item with a body; one
/// on an item that ends in `;` has no block) - rule 5.
fn test_code(path: &str, text: &str) -> String {
    let code = without_comments(text);
    if is_test_file(path) {
        return code;
    }
    let attribute = "#[cfg(test)]";
    let mut found = String::new();
    for (at, _) in code.match_indices(attribute) {
        let rest = &code[at + attribute.len()..];
        let (Some(open), semicolon) = (rest.find('{'), rest.find(';')) else {
            continue;
        };
        if semicolon.is_some_and(|s| s < open) {
            continue;
        }
        let offset = at + attribute.len() + open;
        found.push_str(block_after(&code, offset));
        found.push('\n');
    }
    found
}

/// Rule 2: a file (path relative to this crate's root, `../<crate>/...` for another crate) that asks
/// for the real parts, though it is neither the dial module nor the composition root.
fn entry_violations(path: &str, text: &str) -> Vec<String> {
    if is_dial(path) || ENTRY_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Vec::new();
    }
    entry_names()
        .iter()
        .filter(|name| names(text, name))
        .map(|name| format!("{path} asks for the real parts ({name})"))
        .collect()
}

/// The problems of one file of this crate (`path` relative to the crate root): rules 1, 2, 6 and 7 on
/// its whole text, rules 3 and 4 on its test code.
fn violations(path: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let flat = flat_paths(text);
    let named = |name: &str| names(text, name) || flat.iter().any(|p| names(p, name));
    if !is_dial(path) {
        for name in network_names() {
            if named(&name) {
                found.push(format!("{path} names {name}; only {DIAL_PREFIX} may"));
            }
        }
    }
    found.extend(dial_test_violations(path, text));
    found.extend(unblock_violations(path, text));
    found.extend(connect_violations(path, text));
    found.extend(door_violations(path, text));
    found.extend(crypto_violations(path, text));
    found.extend(https_violations(path, text));
    found.extend(entry_violations(path, text));
    found.extend(wildcard_arms(path, text));
    for name in whatsapp_entry_names() {
        if names(text, &name) {
            found.push(format!(
                "{path} names {name}, WhatsApp's entry (its check would fail after the merge)"
            ));
        }
    }
    found.extend(network_violations(path, &test_code(path, text)));
    found.extend(mock_violations(path, text));
    found
}

/// The only test of the dial module's `tests.rs` that is not an `offline_*` child: the parent that
/// starts the child under `unshare -rn` (rule 11).
const DIAL_PARENT: &str = "the_dial_tests_pass_in_an_offline_child";

/// The `production(` constructors a dial test may call: they make DATA (the production pin set, the
/// verifier, the two TLS configurations) and no connector, so none of them can dial (rule 11).
const PRODUCTION_DATA: [&str; 3] = ["PinSet", "PinnedVerifier", "TlsConfigs"];

/// What a function that names a real connector may not name in the same body: the production pin
/// set, the only one that covers Threema's hosts (a connector built with the test pins reaches
/// `*.kourier.invalid` at most, which never resolves).
fn production_pins() -> [String; 2] {
    [
        ["PinSet::", "production("].concat(),
        ["THREEMA_CH_", "PINS"].concat(),
    ]
}

/// Rule 11: the TEST code of the dial module (its `tests.rs`, `tests/` and `#[cfg(test)]` blocks)
/// names no socket, no resolver ([`socket_names`]), no entry to the real parts and no real dial
/// ([`dial_test_names`]): the dial tests run over `tokio::io::duplex` only. And the method forms
/// ([`dial_function_violations`]).
fn dial_test_violations(path: &str, text: &str) -> Vec<String> {
    if !is_dial(path) {
        return Vec::new();
    }
    // A test file is read whole, comments included (as rule 1 reads it); a production file of the
    // module gives its `#[cfg(test)]` blocks.
    let code = if is_test_file(path) {
        text.to_owned()
    } else {
        test_code(path, text)
    };
    let flat = flat_paths(&code);
    let mut found: Vec<String> = socket_names()
        .into_iter()
        .chain(dial_test_names())
        .filter(|name| names(&code, name) || flat.iter().any(|p| names(p, name)))
        .map(|name| format!("{path}: a dial test names {name} (rule 11: in-memory pipes only)"))
        .collect();
    found.extend(dial_function_violations(path, &code));
    found
}

/// Rule 11, the method forms (TH3 fix S-M1): in dial test code (comments aside) (1) `production(` is
/// called only as `PinSet::`, `PinnedVerifier::` or `TlsConfigs::` ([`PRODUCTION_DATA`]): a real
/// connector is never built with the production pins, so a call through a method (`ws.connect(..)`,
/// `.execute(..)`) cannot reach a host; (2) every test function but the parent is named `offline_*`
/// and begins with `if !in_child()` (a test of another name is not in the child's filter and runs in
/// the PARENT process, with the network up; one without the guard runs there as well); (3) a function
/// that names `LiveWs` or `LiveHttps` does not name the production pin set ([`production_pins`]).
fn dial_function_violations(path: &str, code: &str) -> Vec<String> {
    let code = without_comments(code);
    let mut found = Vec::new();
    for (at, _) in code.match_indices("production(") {
        if code[..at].chars().next_back().is_some_and(is_ident_char) {
            continue;
        }
        let owner = code[..at]
            .trim_end()
            .strip_suffix("::")
            .map(str::trim_end)
            .and_then(|before| before.rsplit(|c: char| !is_ident_char(c)).next());
        if !owner.is_some_and(|owner| PRODUCTION_DATA.contains(&owner)) {
            found.push(format!(
                "{path}: a dial test calls production( outside {PRODUCTION_DATA:?} (rule 11: a connector is built with the test pins)"
            ));
        }
    }
    for (name, is_test, body) in functions(&code) {
        if is_test && name != DIAL_PARENT {
            if !name.starts_with("offline_") {
                found.push(format!(
                    "{path}: the dial test `{name}` is not named offline_* (rule 11: it would run outside the unshare child)"
                ));
            }
            if !body.trim_start().starts_with("if !in_child()") {
                found.push(format!(
                    "{path}: the dial test `{name}` does not begin with the in_child() guard (rule 11)"
                ));
            }
        }
        let connector = ["Live", "Ws"].concat();
        let https = ["Live", "Https"].concat();
        if names(&body, &connector) || names(&body, &https) {
            for pins in production_pins() {
                if body.contains(&pins) {
                    found.push(format!(
                        "{path}: `{name}` names a real connector and the production pins (rule 11)"
                    ));
                }
            }
        }
    }
    found
}

/// The directory of the offline fake (rule 9).
const MOCK_DIR: &str = "tests/support/mock/";

/// The directory of the shared test support, whose files may not re-export libthreema (rule 9).
const SUPPORT_DIR: &str = "tests/support/";

/// The `crate::` paths the fake may name: itself, and the support modules that are named here (each
/// is checked by [`support_reexport_violations`] not to re-export libthreema).
const MOCK_MAY_USE: &[&str] = &["crate::support::mock::", "crate::support::guards::"];

/// The modules of Kourier's own join code the fake may not name (TH4 J2: the fake decodes the QR
/// with its own struct and runs the rendezvous on its own crypto).
const MOCK_REFUSED_OWN: &[&str] = &[
    "kourier_threema::join",
    "kourier_threema::proto",
    // TH5 A2: the phone's builders are the oracle of the follower receive and write the bodies and
    // envelopes from the spec, never through Kourier's parser.
    "kourier_threema::reflect",
    // TH6a S2: the send core's encode, bodies and queue. The fake's peer opens every box on its own crypto and the
    // phone builds the bodies from the spec; neither goes through the code that wrote them.
    "kourier_threema::send",
    // TH6 L2a: the leader receive. The fake's peer seals what it sends to the user on its own crypto, and the
    // guard oracle opens the leader's reflections with the phone's keys; neither goes through the leader's code.
    "kourier_threema::leader",
];

/// Rule 9, the re-export half: a `pub use` or `pub type` (any visibility, `pub(crate)` too) in a file
/// under `tests/support/` of anything of libthreema but its generated protobuf types would hand the
/// fake libthreema's protocol code through `crate::support`.
fn support_reexport_violations(path: &str, text: &str) -> Vec<String> {
    if !path.starts_with(SUPPORT_DIR) {
        return Vec::new();
    }
    // `::libthreema::x` is the same path as `libthreema::x`.
    let code = without_comments(text).replace("::libthreema", " libthreema");
    let mut found = Vec::new();
    for (at, _) in code.match_indices("pub") {
        let before = code[..at].chars().next_back();
        if before.is_some_and(is_ident_char) {
            continue;
        }
        let mut rest = code[at + 3..].trim_start();
        if rest.starts_with('(') {
            let Some(close) = rest.find(')') else {
                continue;
            };
            rest = rest[close + 1..].trim_start();
        }
        let item = ["use", "type"]
            .into_iter()
            .find(|k| rest.starts_with(k) && rest[k.len()..].starts_with(char::is_whitespace));
        if item.is_none() {
            continue;
        }
        let statement = rest.split(';').next().unwrap_or(rest);
        for p in flat_paths(statement) {
            let compact: String = p.split_whitespace().collect();
            let lib_path = compact == "libthreema" || compact.starts_with("libthreema::");
            let protobuf =
                compact == "libthreema::protobuf" || compact.starts_with("libthreema::protobuf::");
            if lib_path && !protobuf {
                found.push(format!(
                    "{path} re-exports {compact}; only libthreema's protobuf types may be (rule 9)"
                ));
            }
        }
    }
    found
}

/// Rule 9: a file of the fake (`path` relative to the crate root) that names a socket, takes
/// anything but protobuf types from libthreema, or reaches into the test modules.
fn mock_violations(path: &str, text: &str) -> Vec<String> {
    let mut found = support_reexport_violations(path, text);
    if !path.starts_with(MOCK_DIR) {
        return found;
    }
    let lib = "libthreema";
    // `::libthreema::x` is the same path as `libthreema::x`.
    let text = text
        .replace(&format!("::{lib}"), &format!(" {lib}"))
        .replace("::kourier_threema", " kourier_threema");
    let flat = flat_paths(&text);
    for name in network_names() {
        if names(&text, &name) || flat.iter().any(|p| names(p, &name)) {
            found.push(format!(
                "{path} names {name}; the fake names no socket (rule 9)"
            ));
        }
    }
    let allowed = format!("{lib}::protobuf");
    for p in &flat {
        let compact: String = p.split_whitespace().collect();
        if compact.starts_with(&format!("{lib}::"))
            && compact != allowed
            && !compact.starts_with(&format!("{allowed}::"))
        {
            found.push(format!(
                "{path} takes {compact} from {lib}; the fake takes only its protobuf types (rule 9)"
            ));
        }
        if compact.starts_with("crate::") && !MOCK_MAY_USE.iter().any(|m| compact.starts_with(m)) {
            found.push(format!(
                "{path} reaches {compact}; the fake uses only {MOCK_MAY_USE:?} (rule 9)"
            ));
        }
        // TH4: the fake never takes Kourier's own join code (it is the oracle of that code).
        let own = compact.trim_start_matches("::");
        for refused in MOCK_REFUSED_OWN {
            if own == *refused || own.starts_with(&format!("{refused}::")) {
                found.push(format!(
                    "{path} takes {own}; the fake has its own join code (rule 9)"
                ));
            }
        }
    }
    // A relative path that leaves the fake's directory, the file-inclusion forms, and (in the
    // fake's own `mod.rs`, whose `super` is `tests/support/`) any `super::` at all.
    let squeezed: String = text.split_whitespace().collect();
    if squeezed.contains("super::super") {
        found.push(format!(
            "{path} climbs out of the fake with super::super (rule 9)"
        ));
    }
    if path == "tests/support/mock/mod.rs" && squeezed.contains("super::") {
        found.push(format!(
            "{path} is the fake's root: its super is tests/support/ (rule 9)"
        ));
    }
    for form in ["#[path", "include!("] {
        if squeezed.contains(form) {
            found.push(format!(
                "{path} pulls a file in with {form}; the fake is its own files (rule 9)"
            ));
        }
    }
    for form in ["include_str!(", "include_bytes!("] {
        for (at, _) in text.match_indices(form) {
            let argument = text[at..].split(')').next().unwrap_or_default();
            if argument.contains(".rs") {
                found.push(format!(
                    "{path} includes a Rust file with {form}; the fake is its own files (rule 9)"
                ));
            }
        }
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    for pair in words.windows(2) {
        let alias = pair[0] == lib && pair[1] == "as";
        let extern_crate = pair[0] == "crate" && pair[1].trim_end_matches(';') == lib;
        if alias || extern_crate {
            found.push(format!(
                "{path} names {lib} under another name; the fake takes only its protobuf types (rule 9)"
            ));
        }
    }
    found
}

/// The file whose command match rule 7 checks, and the head of that match.
const ACTOR_MODULE: &str = "src/actor.rs";
const COMMAND_MATCH: &str = "match command {";

/// Whether a pattern takes any value: `_`, a bare binding (`other`), a binding with `mut`, `ref` or
/// `ref mut`, and a binding `name @ <any of these>`.
fn takes_anything(pattern: &str) -> bool {
    let last = pattern.rsplit('@').next().unwrap_or(pattern).trim();
    let mut rest = last;
    for keyword in ["ref ", "mut "] {
        rest = rest.strip_prefix(keyword).map_or(rest, str::trim_start);
    }
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !rest.starts_with(|c: char| c.is_ascii_digit())
}

/// Rule 7: the arms of the actor's command match that would take any command (see
/// [`takes_anything`]), and the struct patterns that leave fields out with `..`. Every line of the
/// match's block is looked at, nested matches included (stricter than needed: the actor answers
/// commands without wildcards anywhere in that match).
fn wildcard_arms(path: &str, text: &str) -> Vec<String> {
    if path != ACTOR_MODULE {
        return Vec::new();
    }
    let Some(at) = text.find(COMMAND_MATCH) else {
        return vec![format!(
            "{path} has no `{COMMAND_MATCH}`: rule 7 cannot see the command match"
        )];
    };
    let block = block_after(text, at + COMMAND_MATCH.len() - 1);
    let mut found = Vec::new();
    let mut lines = String::new();
    for line in block.lines() {
        let line = line.trim_start();
        if line.starts_with("//") {
            continue;
        }
        lines.push_str(line);
        lines.push('\n');
        // Every arm on the line: its pattern is what stands between the last `{`, `,` or `|` (or
        // the line's start) and its `=>`.
        for (arrow, _) in line.match_indices("=>") {
            let before = &line[..arrow];
            let start = before.rfind(['{', ',', '|']).map_or(0, |i| i + 1);
            let pattern = before[start..].trim().trim_start_matches('|').trim();
            let pattern = pattern.split(" if ").next().unwrap_or(pattern).trim();
            if takes_anything(pattern) {
                found.push(format!(
                    "{path}'s command match has a wildcard arm `{pattern} =>` (rule 7: every variant gets its own arm)"
                ));
            }
        }
    }
    // `..` after `{` or `,` and before `}`: a pattern that leaves fields out (a range or a struct
    // update is followed by an expression, not by the closing brace).
    let chars: Vec<char> = lines.chars().collect();
    for (i, pair) in chars.windows(2).enumerate() {
        if pair != ['.', '.'] || chars.get(i + 2) == Some(&'.') || (i > 0 && chars[i - 1] == '.') {
            continue;
        }
        let before = chars[..i].iter().rev().find(|c| !c.is_whitespace());
        let after = chars[i + 2..].iter().find(|c| !c.is_whitespace());
        if matches!(before, Some('{' | ',')) && after == Some(&'}') {
            found.push(format!(
                "{path}'s command match leaves fields out with `..` (rule 7: name every field, `field: _`)"
            ));
        }
    }
    found
}

/// The functions of `code` as `(name, is a test, body)`: a test is a `fn` preceded, since the end of
/// the item before it, by `#[test]` or `#[tokio::test`. A function without a body is left out.
fn functions(code: &str) -> Vec<(String, bool, String)> {
    let mut found = Vec::new();
    for (at, _) in code.match_indices("fn ") {
        if code[..at].chars().next_back().is_some_and(is_ident_char) {
            continue;
        }
        let rest = &code[at + 3..];
        let name: String = rest.chars().take_while(|c| is_ident_char(*c)).collect();
        if name.is_empty() {
            continue;
        }
        // The body starts at the first `{` after the parameter list's closing parenthesis.
        let Some(open_paren) = rest.find('(') else {
            continue;
        };
        let mut depth = 0usize;
        let mut after_params = None;
        for (i, c) in rest[open_paren..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        after_params = Some(open_paren + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(after_params) = after_params else {
            continue;
        };
        let tail = &rest[after_params..];
        let (Some(open), semicolon) = (tail.find('{'), tail.find(';')) else {
            continue;
        };
        if semicolon.is_some_and(|s| s < open) {
            continue;
        }
        let body = block_after(rest, after_params + open).to_owned();
        let item_start = code[..at].rfind(['}', ';']).map_or(0, |i| i + 1);
        let before = &code[item_start..at];
        let is_test = before.contains("#[test]") || before.contains("#[tokio::test");
        found.push((name, is_test, body));
    }
    found
}

/// What a parent or a child of the offline child calls (rule 4).
const CHILD_CALLS: [&str; 4] = [
    "offline::run_child(",
    "offline::run_child_output(",
    "offline::child_command(",
    "offline::in_child()",
];

/// Rules 3 and 4 on test code: every call that makes a network hands it the guards or the fake
/// (`guards::`, `support::guards::`, `mock::`, `support::mock::`), and every test function that
/// spawns an actor (itself, or through a function of the file that does) is the parent or the child
/// of the `unshare` child ([`CHILD_CALLS`]).
fn network_violations(path: &str, code: &str) -> Vec<String> {
    let call = network_call();
    let mut found = Vec::new();
    let mut calls = 0;
    for (at, _) in code.match_indices(call.as_str()) {
        calls += 1;
        let argument = code[at + call.len()..].trim_start();
        let allowed = ["guards::", "support::guards::", "mock::", "support::mock::"];
        if !allowed.iter().any(|prefix| argument.starts_with(prefix)) {
            found.push(format!(
                "{path} makes a network from something other than the guards or the fake"
            ));
        }
    }
    // The harness is the actor tests' own (`tests/connection/`, `tests/join/`): elsewhere the words
    // are only text.
    let harness = HARNESS_DIRS.iter().any(|d| path.starts_with(d)) && code.contains(HARNESS_START);
    if calls == 0 && !harness {
        return found;
    }
    let all = functions(code);
    let mut spawners: BTreeSet<&str> = all
        .iter()
        .filter(|(_, _, body)| {
            (calls > 0 && body.contains(".spawn(")) || (harness && body.contains(HARNESS_START))
        })
        .map(|(name, _, _)| name.as_str())
        .collect();
    loop {
        let before = spawners.len();
        for (name, _, body) in &all {
            if spawners
                .iter()
                .any(|spawner| names(body, &format!("{spawner}(")))
            {
                spawners.insert(name);
            }
        }
        if spawners.len() == before {
            break;
        }
    }
    for (name, is_test, body) in &all {
        if *is_test
            && spawners.contains(name.as_str())
            && !CHILD_CALLS.iter().any(|word| body.contains(word))
        {
            found.push(format!(
                "{path}'s test `{name}` spawns an actor outside an `unshare -rn` child"
            ));
        }
    }
    found
}

/// Every `.rs` file below `dir`, except a directory called `target`.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The Rust sources (`src/` and `tests/`) of the other crates of the workspace and of xtask, as
/// `(path relative to this crate's root, file)`.
fn other_crates_sources(root: &Path) -> Vec<(String, PathBuf)> {
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

/// Every file of this crate keeps rules 1-7 and 9, and no other crate asks for the real parts.
#[test]
fn no_source_file_names_the_real_network_or_asks_for_the_real_parts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(root, &mut files);
    let mut found = Vec::new();
    let (mut sources, mut tests) = (0, 0);
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        let relative = path
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        sources += usize::from(relative.starts_with("src/"));
        tests += usize::from(relative.starts_with("tests/"));
        found.extend(violations(&relative, &text));
    }
    assert!(
        sources >= 8 && tests >= 7,
        "the sources were not found ({sources} in src/, {tests} in tests/)"
    );
    let others = other_crates_sources(root);
    for needed in [
        "../kourier-app/src/lib.rs",
        "../kourier-whatsapp/tests/builder.rs",
        "../../xtask/src/main.rs",
    ] {
        assert!(
            others.iter().any(|(p, _)| p == needed),
            "the other crates were not found ({needed})"
        );
    }
    for (relative, path) in others {
        let text = std::fs::read_to_string(&path).expect("read");
        found.extend(entry_violations(&relative, &text));
        found.extend(door_violations(&relative, &text));
    }
    // Rule 13: no other manifest renames the package.
    let beside = root.parent().expect("crates dir");
    let (mut manifests, mut dirs) = (
        0,
        vec![beside.parent().expect("workspace root").join("xtask")],
    );
    for entry in std::fs::read_dir(beside).expect("read crates dir") {
        dirs.push(entry.expect("entry").path());
    }
    for dir in dirs {
        let manifest = dir.join("Cargo.toml");
        if dir != root && manifest.exists() {
            manifests += 1;
            let text = std::fs::read_to_string(&manifest).expect("read");
            found.extend(rename_violations(&manifest.to_string_lossy(), &text));
        }
    }
    assert!(
        manifests >= 10,
        "the manifests were not found ({manifests})"
    );
    assert!(found.is_empty(), "{found:#?}");
}

/// The dependency tables of a manifest: `(label, table)` for `[dependencies]`, `[dev-dependencies]`,
/// `[build-dependencies]`, the same under `[target.<cfg>]`, and the root's `[workspace.dependencies]`.
fn dependency_tables(manifest: &toml::Table) -> Vec<(String, &toml::Table)> {
    fn add<'a>(
        sections: &mut Vec<(String, &'a toml::Table)>,
        label: String,
        value: Option<&'a toml::Value>,
    ) {
        if let Some(table) = value.and_then(toml::Value::as_table) {
            sections.push((label, table));
        }
    }
    let kinds = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut sections = Vec::new();
    for kind in kinds {
        add(&mut sections, kind.to_owned(), manifest.get(kind));
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (cfg, target) in targets {
            for kind in kinds {
                add(
                    &mut sections,
                    format!("target.{cfg}.{kind}"),
                    target.get(kind),
                );
            }
        }
    }
    if let Some(workspace) = manifest.get("workspace").and_then(toml::Value::as_table) {
        add(
            &mut sections,
            "workspace.dependencies".to_owned(),
            workspace.get("dependencies"),
        );
    }
    sections
}

/// Rule 8 (a): the problems of one manifest (`path`, its `text`) and how many libthreema entries it
/// has. An entry is libthreema by its key or by `package = "libthreema"`. None may have a `features`
/// key (an empty list too); the one in `[workspace.dependencies]` must say `default-features = false`.
fn libthreema_dependency_violations(path: &str, text: &str) -> (Vec<String>, usize) {
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return (
            vec![format!(
                "{path} is not TOML: rule 8 cannot see its dependencies"
            )],
            0,
        );
    };
    let (mut found, mut rows) = (Vec::new(), 0);
    // The keys libthreema goes by in this manifest (its own name, or a `package =` rename).
    let mut keys = BTreeSet::from(["libthreema".to_owned()]);
    for (label, table) in dependency_tables(&manifest) {
        for (key, spec) in table {
            let package = spec
                .get("package")
                .and_then(toml::Value::as_str)
                .unwrap_or(key);
            if package != "libthreema" {
                continue;
            }
            keys.insert(key.clone());
            rows += 1;
            let spec = spec.as_table();
            if spec.is_some_and(|s| s.contains_key("features")) {
                found.push(format!(
                    "{path} [{label}] gives libthreema a `features` key (rule 8: none, ever)"
                ));
            }
            if label == "workspace.dependencies" {
                let no_defaults = spec.is_some_and(|s| {
                    s.get("default-features").and_then(toml::Value::as_bool) == Some(false)
                });
                if !no_defaults {
                    found.push(format!(
                        "{path} [{label}] has libthreema without `default-features = false` (rule 8)"
                    ));
                }
            }
        }
    }
    // A feature of this crate that forwards one of libthreema's (`libthreema/cli`, `libthreema?/cli`)
    // gives it that feature without a `features` key.
    if let Some(features) = manifest.get("features").and_then(toml::Value::as_table) {
        for (feature, list) in features {
            let items = list.as_array().into_iter().flatten();
            for item in items.filter_map(toml::Value::as_str) {
                let crate_part = item.split('/').next().unwrap_or("").trim_end_matches('?');
                if item.contains('/') && keys.contains(crate_part) {
                    found.push(format!(
                        "{path} [features] `{feature}` forwards a libthreema feature (rule 8: none, ever)"
                    ));
                }
            }
        }
    }
    (found, rows)
}

/// The features a feature `start` switches on in a `[features]` table (itself included), through the
/// features of the same crate; `dep:x` and `x/feature` name other crates.
fn feature_closure(features: &toml::Table, start: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut todo = vec![start];
    while let Some(name) = todo.pop() {
        if !seen.insert(name.to_owned()) {
            continue;
        }
        let Some(list) = features.get(name).and_then(toml::Value::as_array) else {
            continue;
        };
        for item in list.iter().filter_map(toml::Value::as_str) {
            if !item.starts_with("dep:") && !item.contains('/') {
                todo.push(item);
            }
        }
    }
    seen
}

/// The features whose switching on is never allowed (rule 8): libthreema's CLI and bindings.
const BANNED_FEATURES: [&str; 3] = ["cli", "uniffi", "wasm"];

/// Rule 8 (b): the vendored manifest's `default` feature reaches none of [`BANNED_FEATURES`].
fn libthreema_default_violations(path: &str, text: &str) -> Vec<String> {
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return vec![format!(
            "{path} is not TOML: rule 8 cannot see its features"
        )];
    };
    let Some(features) = manifest.get("features").and_then(toml::Value::as_table) else {
        return vec![format!(
            "{path} has no [features] table: rule 8 cannot see the defaults"
        )];
    };
    let on = feature_closure(features, "default");
    BANNED_FEATURES
        .iter()
        .filter(|banned| on.contains(**banned))
        .map(|banned| format!("{path}: the `default` feature switches on `{banned}` (rule 8)"))
        .collect()
}

/// libthreema is given no feature anywhere in the workspace, and its own defaults switch on none of
/// the CLI and the bindings (rule 8).
#[test]
fn libthreema_is_given_no_feature_and_has_no_dangerous_default() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = root
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let mut manifests = vec![
        workspace.join("Cargo.toml"),
        workspace.join("xtask/Cargo.toml"),
    ];
    for entry in std::fs::read_dir(workspace.join("crates")).expect("read crates") {
        manifests.push(entry.expect("entry").path().join("Cargo.toml"));
    }
    manifests.retain(|path| path.exists());
    let (mut found, mut rows, mut root_rows) = (Vec::new(), 0, 0);
    for path in &manifests {
        let relative = path
            .strip_prefix(workspace)
            .expect("under the workspace")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path).expect("read the manifest");
        let (problems, count) = libthreema_dependency_violations(&relative, &text);
        found.extend(problems);
        rows += count;
        if relative == "Cargo.toml" {
            root_rows = count;
        }
    }
    assert!(
        manifests.len() >= 10 && root_rows == 1 && rows >= 2,
        "the manifests were not found, or rule 8 sees no libthreema row ({} manifests, {rows} rows, {root_rows} in the root)",
        manifests.len()
    );
    let vendored = "third_party/libthreema/lib/Cargo.toml";
    let text = std::fs::read_to_string(workspace.join(vendored)).expect("the vendored manifest");
    found.extend(libthreema_default_violations(vendored, &text));
    // Not vacuous: the feature the rule is about is still there.
    let manifest = text.parse::<toml::Table>().expect("TOML");
    let features = manifest.get("features").and_then(toml::Value::as_table);
    assert!(
        features.is_some_and(|f| f.contains_key("cli")),
        "the vendored manifest has no `cli` feature any more: update rule 8"
    );
    assert!(found.is_empty(), "{found:#?}");
}

/// A use declaration of `items` below `prefix`, written with braces (the brace forms of rule 1).
/// Built here so that this file does not hold them as text.
fn brace(prefix: &str, items: &[&str]) -> String {
    format!("use {prefix}::{{{}}};", items.join(", "))
}

/// The check itself: one case per rule, so that a weakened rule fails here.
#[test]
fn the_source_check_refuses_what_it_is_for() {
    // Rule 1: a network name in a test, in src/ or in a module that is not the listed one; the dial
    // module may; a longer name is not the name; a comment counts (the whole text is scanned).
    assert_eq!(DIAL_PREFIX, "src/dial/");
    for name in network_names() {
        for path in [
            "tests/a.rs",
            "src/a.rs",
            "src/net/dial.rs",
            "src/dial.rs",
            "src/dialx/a.rs",
            "src/net/dial/a.rs",
            "src/a/tests.rs",
            "build.rs",
            "examples/a.rs",
            "benches/a.rs",
        ] {
            let v = violations(path, &format!("use {name};"));
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
        for dial in ["src/dial/mod.rs", "src/dial/pins.rs", "src/dial/tests.rs"] {
            let v = violations(dial, &format!("use {name};"));
            // Rule 11 still holds the dial module's tests off sockets and resolvers.
            let socket = socket_names().contains(&name);
            assert_eq!(
                v.len(),
                usize::from(socket && dial.ends_with("tests.rs")),
                "{dial} {name}: {v:?}"
            );
        }
        assert_eq!(
            violations("src/a.rs", &format!("// {name}")).len(),
            1,
            "{name}"
        );
    }
    let host = ["api.", "threema", ".ch"].concat();
    assert_eq!(
        violations("src/a.rs", &format!("let h = \"{host}\";")).len(),
        1
    );
    let tls = ["rus", "tls"].concat();
    assert!(violations("src/a.rs", &format!("let x = my_{tls}_thing;")).is_empty());
    let hy = ["hy", "per"].concat();
    assert!(violations("src/a.rs", &format!("a {hy}link")).is_empty());
    // Rule 2: the entry and the module path, from this crate and from any other but the app's src/.
    let entry = ["live_", "parts"].concat();
    let module = ["kourier_threema::", "dial"].concat();
    let call = format!("let p = {module}::{entry}();");
    assert!(violations("src/dial/mod.rs", &format!("pub fn {entry}() {{}}")).is_empty());
    assert!(entry_violations("../kourier-app/src/threema.rs", &call).is_empty());
    for path in [
        "tests/a.rs",
        "src/a.rs",
        "src/network.rs",
        "../kourier-app/tests/a.rs",
        "../kourier/src/main.rs",
        "../kourier-e2e/src/lib.rs",
        "../kourier-whatsapp/src/real.rs",
        "../../xtask/src/main.rs",
    ] {
        assert_eq!(
            entry_violations(path, &call).len(),
            2,
            "{path} may ask for the real parts"
        );
    }
    assert_eq!(
        entry_violations("tests/a.rs", &format!("use {module};")).len(),
        1
    );
    assert_eq!(
        entry_violations("tests/a.rs", &format!("let f = {entry};")).len(),
        1
    );
    assert!(entry_violations("tests/a.rs", &format!("let f = un{entry};")).is_empty());
    assert!(entry_violations("tests/a.rs", &format!("let f = {entry}_x;")).is_empty());
    assert!(entry_violations("tests/a.rs", "use kourier_threema::dialect;").is_empty());
    // Rule 3: the guards or the fake, nothing else.
    let network = network_call();
    for ok in [
        "guards::parts())",
        "support::guards::parts())",
        "guards::counting_parts(&made))",
        "mock::parts(&world))",
        "support::mock::parts(&world))",
    ] {
        assert!(
            violations("tests/a.rs", &format!("{network}{ok};")).is_empty(),
            "{ok}"
        );
    }
    for bad in [
        format!("{network}parts);"),
        format!("{network}other::guards::parts());"),
        format!("{network}{module}::{entry}());"),
        format!("{network}ThreemaParts::refusing(made));"),
    ] {
        assert!(!violations("tests/a.rs", &bad).is_empty(), "{bad}");
    }
    // A comment that makes a network is nothing; src/ production code may (the app does, C8).
    assert!(violations("tests/a.rs", &format!("// {network}parts);")).is_empty());
    assert!(violations("src/a.rs", &format!("{network}parts);")).is_empty());
    // Rule 5: the unit tests of src/ are tests; a production file of src/ is not.
    let spawn = format!(
        "#[test]\nfn spawns() {{\n    {network}guards::parts());\n    network.spawn(ctx);\n}}"
    );
    for path in ["src/actor/tests.rs", "src/x/tests/y.rs"] {
        assert!(is_test_file(path), "{path}");
        assert_eq!(
            violations(path, &format!("{network}parts);")).len(),
            1,
            "{path}"
        );
        assert_eq!(violations(path, &spawn).len(), 1, "{path}");
    }
    for path in ["src/actor.rs", "src/testsuite.rs", "src/network.rs"] {
        assert!(!is_test_file(path), "{path}");
    }
    assert!(is_test_file("tests/builder.rs") && is_test_file("tests/support/guards.rs"));
    // Rule 6: WhatsApp's entry in any file of this crate, the dial module included, comments too.
    let wa_entry = ["net_", "parts"].concat();
    assert_eq!(
        violations("src/dial/mod.rs", &format!("fn {wa_entry}() {{}}")).len(),
        1
    );
    assert_eq!(
        violations("tests/support/mod.rs", &format!("// {wa_entry}")).len(),
        1
    );
    let wa_module = ["re", "al::"].concat();
    // `crate::` + the module holds both of WhatsApp's path needles.
    assert_eq!(
        violations("tests/a.rs", &format!("use crate::{wa_module}x;")).len(),
        2
    );
    assert_eq!(
        violations("tests/a.rs", &format!("use {wa_module}x;")).len(),
        1
    );
    let wa_path = ["::", "re", "al"].concat();
    assert_eq!(
        violations("src/a.rs", &format!("use super{wa_path};")).len(),
        1
    );
    // A longer name is not WhatsApp's entry (L5: `names()` matches at identifier borders).
    assert!(violations("src/a.rs", &format!("let x = un{wa_module}y;")).is_empty());
    assert!(violations("src/a.rs", &format!("use super{wa_path}_parts;")).is_empty());
}

/// Rule 1 sees a socket type or module however it is written: the brace forms, a Unix socket by its
/// path, the low-level calls.
#[test]
fn rule_one_sees_the_brace_forms_and_the_other_sockets() {
    let std_ = ["st", "d"].concat();
    let tokio_ = ["tok", "io"].concat();
    let net = ["n", "et"].concat();
    let unix_stream = ["Unix", "Stream"].concat();
    let tcp_listener = ["Tcp", "Listener"].concat();
    let one = |text: &str| violations("src/a.rs", text).len();
    // A group of the standard library or tokio: alone, with others before or after, with a path
    // below it, over several lines, nested.
    assert_eq!(one(&brace(&std_, &[&net])), 1);
    assert_eq!(one(&brace(&std_, &["io", &net])), 1);
    assert_eq!(one(&brace(&std_, &["io", &format!("{net}::Foo")])), 1);
    assert_eq!(one(&brace(&std_, &["io", "fmt", &net])), 1);
    assert_eq!(
        one(&format!(
            "use {std_}::{{\n    io,\n    {net}::{{Foo}},\n}};"
        )),
        1
    );
    assert_eq!(one(&brace(&tokio_, &["io", &net])), 1);
    assert_eq!(one(&brace(&tokio_, &[&format!("{net}::Foo"), "io"])), 1);
    let unix = ["un", "ix"].concat();
    assert_eq!(
        one(&format!("use {std_}::{{io, os::{unix}::{{{net}::Foo}}}};")),
        1
    );
    // Blanks inside the path.
    assert_eq!(one(&format!("use {std_} :: {net};")), 1);
    // A Unix socket by its full path or by a use, and the calls under libc.
    assert_eq!(
        one(&format!(
            "let s = {std_}::os::{unix}::{net}::{unix_stream}::connect(p);"
        )),
        2,
        "the module and the type"
    );
    assert_eq!(one(&format!("use {std_}::os::{unix}::{{{net}::Foo}};")), 1);
    assert_eq!(one(&format!("use x::{tcp_listener};")), 1);
    for name in [
        ["Unix", "Listener"].concat(),
        ["Unix", "Datagram"].concat(),
        ["socket", "2"].concat(),
        ["libc::", "socket"].concat(),
        ["libc::", "connect"].concat(),
    ] {
        assert_eq!(one(&format!("let x = {name};")), 1, "{name}");
    }
    assert_eq!(
        one(&brace("libc", &[&["sock", "et"].concat(), "close"])),
        1,
        "a group below libc"
    );
    // Not names: renamed items, prefixes, other modules.
    assert_eq!(one(&format!("use {std_}::{{io as {net}}};")), 0);
    assert_eq!(one(&brace(&std_, &["io", "fmt"])), 0);
    assert_eq!(one(&format!("use {std_}::{net}work;")), 0);
    assert_eq!(one(&brace("other", &[&net])), 0);
    assert_eq!(one(&format!("use unix_{net};")), 0);
    assert!(violations("src/dial/ws.rs", &brace(&std_, &[&net])).is_empty());
    // TH3 L1: the TLS crates under their own names.
    for name in [["tokio_rus", "tls"].concat(), ["webpki_", "roots"].concat()] {
        assert_eq!(one(&format!("use {name}::X;")), 1, "{name}");
        assert_eq!(one(&format!("let c = {name}::f();")), 1, "{name}");
    }
    // TH3 fix S-M4: the fixed list (the crates the lock holds already through another backend, and
    // the ones it may get) and every package of the lock with an HTTP, TLS, WebSocket, QUIC or
    // socket word in its name.
    let all = crate_names();
    for fixed in [
        ["ur", "eq"].concat(),
        ["hyper_", "util"].concat(),
        ["hyper_rus", "tls"].concat(),
        ["h", "2"].concat(),
        ["tokio_web", "sockets"].concat(),
        ["async_tung", "stenite"].concat(),
        ["m", "io"].concat(),
        ["quin", "n"].concat(),
        ["rustls_platform_", "verifier"].concat(),
        ["rustls_native_", "certs"].concat(),
        ["isa", "hc"].concat(),
        ["cu", "rl"].concat(),
    ] {
        assert!(all.contains(&fixed), "{fixed} is not a needle");
    }
    let derived = lock_names();
    assert!(
        derived.len() >= 10 && derived.contains(&["tokio_tung", "stenite"].concat()),
        "the lock gave {} names: rule 1 reads no Cargo.lock",
        derived.len()
    );
    for name in &all {
        assert_eq!(one(&format!("use {name}::X;")), 1, "{name}");
        assert_eq!(one(&format!("let c = {name}::f();")), 1, "{name}");
    }
    // A longer name that merely holds a needle is not it.
    assert_eq!(one(&format!("use {}_lite::X;", ["ur", "eq"].concat())), 0);
}

/// The files rules 1-7 read are every `.rs` file below the crate root but `target/`.
#[test]
fn the_scan_covers_every_rust_file_but_target() {
    let root = std::env::temp_dir().join(format!("th1-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in [
        "src",
        "tests/support",
        "examples",
        "benches",
        "target/debug",
    ] {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
    }
    for file in [
        "build.rs",
        "src/a.rs",
        "tests/support/b.rs",
        "examples/c.rs",
        "benches/d.rs",
        "target/debug/e.rs",
        "notes.txt",
    ] {
        std::fs::write(root.join(file), "").expect("file");
    }
    let mut files = Vec::new();
    collect(&root, &mut files);
    let mut relative: Vec<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(&root)
                .expect("under")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    relative.sort();
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        relative,
        [
            "benches/d.rs",
            "build.rs",
            "examples/c.rs",
            "src/a.rs",
            "tests/support/b.rs"
        ]
    );
}

/// Rule 4 is per test function, and `#[cfg(test)]` blocks of `src/` are test code (rules 3, 4, 5).
#[test]
fn rule_four_is_checked_per_test_function() {
    let network = network_call();
    let make = format!("{network}guards::parts())");
    let file = |tests: &str| {
        format!(
            "fn start() {{\n    let n = {make};\n    n.spawn(ctx);\n}}\nfn run_child(a: u8) {{}}\n{tests}"
        )
    };
    let refused = |tests: &str| violations("tests/a.rs", &file(tests));
    // A test that starts an actor, directly or through a helper, async or not.
    for tests in [
        "#[test]\nfn t() {\n    start();\n}",
        "#[tokio::test]\nasync fn t() {\n    start().await;\n}",
        "#[tokio::test(flavor = \"multi_thread\")]\nasync fn t() {\n    start().await;\n}",
        "#[test]\nfn t() {\n    let n = NETWORK.spawn(ctx);\n}",
    ] {
        let v = refused(tests);
        assert_eq!(v.len(), 1, "{tests}: {v:?}");
        assert!(v[0].contains("spawns an actor"), "{v:?}");
    }
    // A LOCAL helper called run_child (like tests/kill.rs has) is not the offline one.
    let v = refused("#[test]\nfn t() {\n    start();\n    run_child(1);\n}");
    assert_eq!(v.len(), 1, "{v:?}");
    // The second helper level counts too.
    let v = refused("fn again() {\n    start();\n}\n#[test]\nfn t() {\n    again();\n}");
    assert_eq!(v.len(), 1, "{v:?}");
    // The offline parent and child are fine, each form once.
    for tests in [
        "#[test]\nfn parent() {\n    support::offline::run_child(module_path!(), \"c\");\n}\n#[tokio::test]\nasync fn c() {\n    if !support::offline::in_child() {\n        return;\n    }\n    start().await;\n}",
        "#[test]\nfn p() {\n    let _ = offline::run_child_output(m, \"c\", &[]);\n    start();\n}",
        "#[test]\nfn p() {\n    let c = offline::child_command(m, \"c\");\n    start();\n}",
    ] {
        assert!(refused(tests).is_empty(), "{tests}");
    }
    // Only a test is held to it: a helper that spawns, a test that spawns nothing.
    assert!(refused("#[test]\nfn quiet() {\n    let _ = 1;\n}").is_empty());
    assert!(violations("tests/a.rs", "fn only_a_helper() { x.spawn(1); }").is_empty());
    // A file that makes no network is not looked at.
    assert!(violations("tests/a.rs", "#[test]\nfn t() {\n    pool.spawn(1);\n}").is_empty());
    // Two tests, one fine: only the other one is named.
    let v = refused(
        "#[test]\nfn good() {\n    offline::run_child(m, \"c\");\n    start();\n}\n#[test]\nfn bad() {\n    start();\n}",
    );
    assert_eq!(v.len(), 1, "{v:?}");
    assert!(v[0].contains("`bad`"), "{v:?}");

    // The cfg(test) blocks of src/ files.
    let unit = format!(
        "fn production() {{ {network}parts); }}\n#[cfg(test)]\nmod tests {{\n    use super::*;\n    #[test]\n    fn t() {{\n        let n = {make};\n        n.spawn(ctx);\n    }}\n}}"
    );
    let v = violations("src/a.rs", &unit);
    assert!(
        v.iter()
            .any(|m| m.contains("`t`") && m.contains("spawns an actor")),
        "{v:?}"
    );
    let from_variable =
        format!("#[cfg(test)]\nmod tests {{\n    fn t() {{\n        {network}parts);\n    }}\n}}");
    assert_eq!(violations("src/a.rs", &from_variable).len(), 1);
    // ... and the production code around them is not test code, an attribute on an item without a
    // body takes no block with it.
    let around = format!(
        "#[cfg(test)]\nconst DRAIN: u8 = 1;\nfn production() {{\n    {network}parts);\n    n.spawn(ctx);\n}}"
    );
    assert!(violations("src/a.rs", &around).is_empty());
    let ok_unit = format!(
        "#[cfg(test)]\nmod tests {{\n    #[test]\n    fn parent() {{\n        offline::run_child(m, \"c\");\n        let n = {make};\n        n.spawn(ctx);\n    }}\n}}"
    );
    assert!(violations("src/a.rs", &ok_unit).is_empty());
}

/// Rule 7: the actor's command match lists every variant and names every field; `_`, a bare
/// binding, `mut x`, `ref x`, `x @ _` and `{ .. }` are refused, in the actor only, and a renamed
/// match is refused too (the rule would see nothing).
#[test]
fn rule_seven_refuses_wildcards_bindings_and_left_out_fields() {
    let arms = |last: &str| {
        format!(
            "fn command(c: Command) {{\n    match command {{\n        Command::Logout => return,\n        Command::Connect | Command::Disconnect => {{\n            let x = 1;\n        }}\n        {last}\n    }}\n}}\nfn later() {{ match y {{ _ => {{}} }} }}"
        )
    };
    let explicit = arms("Command::Shutdown { grace: _ } => {}");
    assert!(
        violations("src/actor.rs", &explicit).is_empty(),
        "{explicit}"
    );
    for wildcard in [
        "_ => {}",
        "other => {}",
        "| _ => {}",
        "rest if true => {}",
        "mut other => {}",
        "ref other => {}",
        "ref mut other => {}",
        "other @ _ => {}",
        "other @ rest => {}",
        "mut other @ _ => {}",
    ] {
        let v = violations("src/actor.rs", &arms(wildcard));
        assert_eq!(v.len(), 1, "{wildcard}: {v:?}");
        assert!(v[0].contains("wildcard arm"), "{v:?}");
        assert!(violations("src/network.rs", &arms(wildcard)).is_empty());
    }
    // Left-out fields: every spelling, inside or after a reply binding, one line or several.
    for rest in [
        "Command::Shutdown { .. } => {}",
        "Command::Shutdown {..} => {}",
        "Command::FetchHistory { conv: _, .. } => {}",
        "Command::Transmit { reply, .. } => { let _ = reply; }",
        "Command::Transmit { reply,.. } => { let _ = reply; }",
        "Command::SendReceipts {\n            cmd: _,\n            ..\n        } => {}",
        "Command::Other { a: _ }\n        | Command::Another { .. } => {}",
    ] {
        let v = violations("src/actor.rs", &arms(rest));
        assert_eq!(v.len(), 1, "{rest}: {v:?}");
        assert!(v[0].contains("leaves fields out"), "{v:?}");
        assert!(violations("src/network.rs", &arms(rest)).is_empty());
    }
    // Not left-out fields: a range, a struct update, a method chain, the fields all named.
    for fine in [
        "Command::Shutdown { grace: _ } => { let r = 0..3; }",
        "Command::Shutdown { grace: _ } => { let s = S { a: 1, ..base() }; }",
        "Command::Shutdown { grace: _ } => { for i in 0..=2 {} }",
        "Command::Shutdown { grace: _ } => { let v = [1..2, 3..4]; }",
        "Command::Transmit { local: _, req: _, reply } => { let _ = reply; }",
    ] {
        assert!(
            violations("src/actor.rs", &arms(fine)).is_empty(),
            "{fine}: {:?}",
            violations("src/actor.rs", &arms(fine))
        );
    }
    // A wildcard in a commented line is nothing; a nested match's wildcard inside the block counts.
    assert!(violations("src/actor.rs", &arms("// _ => {}")).is_empty());
    assert!(violations("src/actor.rs", &arms("// Command::X { .. } => {}")).is_empty());
    assert_eq!(
        violations("src/actor.rs", &arms("Command::X => match y { _ => {} },")).len(),
        1
    );
    let renamed = explicit.replace("match command {", "match cmd {");
    assert_eq!(violations("src/actor.rs", &renamed).len(), 1);
    // The bindings in `takes_anything` themselves.
    for yes in [
        "_",
        "x",
        "mut x",
        "ref x",
        "ref mut x",
        "y @ _",
        "mut y @ z",
    ] {
        assert!(takes_anything(yes), "{yes}");
    }
    for no in [
        "Command::A",
        "Command::A { x: _ }",
        "x @ Command::A",
        "Some(_)",
        "1",
        "A | B",
    ] {
        assert!(!takes_anything(no), "{no}");
    }
}

/// Rule 8 refuses every way to give libthreema a feature, and each default that switches the CLI or
/// the bindings on; a clean manifest passes (and a dependency with features that is not libthreema).
#[test]
fn rule_eight_refuses_what_it_is_for() {
    let lib = "libthreema";
    let dependency = |text: &str| libthreema_dependency_violations("Cargo.toml", text);
    // Refused, in every section and spelling.
    for text in [
        format!("[dependencies]\n{lib} = {{ workspace = true, features = [\"cli\"] }}"),
        format!("[dependencies]\n{lib} = {{ workspace = true, features = [] }}"),
        format!("[dependencies.{lib}]\nworkspace = true\nfeatures = [\"cli\"]"),
        format!("[dev-dependencies]\n{lib} = {{ workspace = true, features = [\"uniffi\"] }}"),
        format!("[build-dependencies]\n{lib}.workspace = true\n{lib}.features = [\"wasm\"]"),
        format!(
            "[target.'cfg(unix)'.dependencies]\n{lib} = {{ workspace = true, features = [\"cli\"] }}"
        ),
        format!(
            "[target.'cfg(unix)'.dev-dependencies]\n{lib} = {{ workspace = true, features = [\"cli\"] }}"
        ),
        format!(
            "[dependencies]\nrenamed = {{ package = \"{lib}\", workspace = true, features = [\"cli\"] }}"
        ),
        format!(
            "[workspace.dependencies]\n{lib} = {{ path = \"x\", default-features = false, features = [\"cli\"] }}"
        ),
    ] {
        let (found, rows) = dependency(&text);
        assert_eq!((found.len(), rows), (1, 1), "{text}: {found:?}");
    }
    // Refused: a feature of the crate that forwards one of libthreema's, under either name.
    for text in [
        format!("[dependencies]\n{lib}.workspace = true\n[features]\nforward = [\"{lib}/cli\"]"),
        format!(
            "[dependencies]\n{lib}.workspace = true\n[features]\ndefault = [\"{lib}?/slow_tests\"]"
        ),
        format!(
            "[dependencies]\nrenamed = {{ package = \"{lib}\", workspace = true }}\n[features]\nforward = [\"renamed/cli\"]"
        ),
    ] {
        let (found, rows) = dependency(&text);
        assert_eq!((found.len(), rows), (1, 1), "{text}: {found:?}");
        assert!(found[0].contains("forwards"), "{found:?}");
    }
    // The root's row: default-features = false, or it is refused (a string version, `true`, none).
    for text in [
        format!("[workspace.dependencies]\n{lib} = {{ path = \"x\" }}"),
        format!("[workspace.dependencies]\n{lib} = {{ path = \"x\", default-features = true }}"),
        format!("[workspace.dependencies]\n{lib} = \"0.1\""),
    ] {
        let (found, rows) = dependency(&text);
        assert_eq!((found.len(), rows), (1, 1), "{text}: {found:?}");
        assert!(found[0].contains("default-features = false"), "{found:?}");
    }
    // Fine: the member row, the root row, other crates with features, a name that merely starts alike.
    for (text, rows) in [
        (format!("[dependencies]\n{lib}.workspace = true"), 1),
        (format!("[dependencies]\n{lib} = {{ workspace = true }}"), 1),
        (
            format!(
                "[workspace.dependencies]\n{lib} = {{ path = \"x\", default-features = false }}"
            ),
            1,
        ),
        (
            "[dependencies]\ntokio = { workspace = true, features = [\"net\"] }".to_owned(),
            0,
        ),
        (
            format!("[dependencies]\n{lib}-macros = {{ workspace = true, features = [\"x\"] }}"),
            0,
        ),
        (
            format!(
                "[dependencies]\n{lib} = {{ workspace = true, optional = true }}\n[features]\nthreema = [\"dep:{lib}\", \"tokio/net\", \"{lib}-macros/x\"]"
            ),
            1,
        ),
        ("[package]\nname = \"x\"".to_owned(), 0),
    ] {
        let (found, count) = dependency(&text);
        assert!(
            found.is_empty() && count == rows,
            "{text}: {found:?} {count}"
        );
    }
    // A manifest that does not parse is a finding, not a pass.
    assert_eq!(dependency("[dependencies\n").0.len(), 1);
    // The vendored manifest's defaults: the CLI and the bindings, directly or through another feature.
    let defaults = |text: &str| libthreema_default_violations("lib/Cargo.toml", text);
    for banned in BANNED_FEATURES {
        let direct = format!("[features]\ndefault = [\"{banned}\"]\n{banned} = []");
        assert_eq!(defaults(&direct).len(), 1, "{direct}");
        let through =
            format!("[features]\ndefault = [\"more\"]\nmore = [\"{banned}\"]\n{banned} = []");
        assert_eq!(defaults(&through).len(), 1, "{through}");
        // Declared but not on: fine.
        let off = format!("[features]\n{banned} = [\"dep:x\"]\nslow = []");
        assert!(defaults(&off).is_empty(), "{off}");
    }
    assert_eq!(
        defaults("[features]\ndefault = [\"cli\", \"uniffi\"]\ncli = []\nuniffi = []").len(),
        2
    );
    assert!(
        defaults("[features]\ndefault = [\"slow\"]\nslow = []\ncli = [\"dep:x\", \"y/cli\"]")
            .is_empty()
    );
    assert!(defaults("[features]\ndefault = [\"y/cli\", \"dep:cli\"]").is_empty());
    assert!(
        defaults("[features]\nslow = []").is_empty(),
        "no default feature at all"
    );
    assert_eq!(
        defaults("[package]\nname = \"x\"").len(),
        1,
        "no [features] table"
    );
    assert_eq!(defaults("[features\n").len(), 1);
}

/// The dial module is a DIRECTORY (TH3 call C6): `src/dial.rs` does not exist (a file there would be
/// outside [`DIAL_PREFIX`], and Rust would refuse the pair). Its entry (`mod.rs`) hands out MAKERS
/// and the production data and dials nothing itself: no connect, request, maker call, task or thread
/// there, and no network name (the connectors live in `ws.rs` and `https.rs`; TH3 N3).
#[test]
fn the_dial_module_is_a_directory_whose_entry_dials_nothing() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("src/dial.rs").exists(),
        "src/dial.rs exists: the dial module is the directory src/dial/"
    );
    let text = std::fs::read_to_string(root.join("src/dial/mod.rs")).expect("src/dial/mod.rs");
    let code = without_comments(&text);
    assert!(
        code.contains(&format!("pub fn {}() -> ThreemaParts", entry_names()[0])),
        "src/dial/mod.rs does not hand out the application's parts"
    );
    assert!(
        code.contains("ThreemaParts::from_makers("),
        "src/dial/mod.rs does not hand out makers"
    );
    let mut forbidden = network_names();
    forbidden.extend([
        ["con", "nect("].concat(),
        ["exec", "ute("].concat(),
        [".", "make("].concat(),
        ["tokio::", "spawn"].concat(),
        ["std::", "thread"].concat(),
    ]);
    for word in forbidden {
        assert!(!names(&code, &word), "src/dial/mod.rs names {word}");
    }
}

/// Rule 11 refuses what it is for: a socket, a resolver, the entry or the real dial in the dial
/// module's test code (a `tests.rs`, a file below `tests/`, a `#[cfg(test)]` block), and lets the
/// same module's production code and the TLS crates in its tests through.
#[test]
fn rule_eleven_keeps_the_dial_tests_on_in_memory_pipes() {
    let tests_files = ["src/dial/tests.rs", "src/dial/tests/certs.rs"];
    let mut refused = Vec::from(socket_names());
    refused.extend(dial_test_names());
    for name in &refused {
        for path in tests_files {
            let v = violations(path, &format!("let x = {name};"));
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
            // A comment counts (rules 1 and 11 read the whole text of a test file).
            let v = violations(path, &format!("// {name}"));
            assert_eq!(v.len(), 1, "{path} {name}: {v:?}");
        }
        // A #[cfg(test)] block of a production file of the module is test code.
        let in_block = format!(
            "fn real() {{}}\n#[cfg(test)]\nmod tests {{\n    fn t() {{ let x = {name}; }}\n}}"
        );
        assert_eq!(violations("src/dial/ws.rs", &in_block).len(), 1, "{name}");
    }
    // The brace form of a socket module is seen in a dial test too.
    let net = ["n", "et"].concat();
    assert_eq!(
        violations("src/dial/tests.rs", &brace("tokio", &["io", &net])).len(),
        1
    );
    // The module's production code may name sockets and the real dial (rule 1's licence).
    for name in &refused {
        assert!(
            violations("src/dial/ws.rs", &format!("let x = {name};")).is_empty(),
            "{name}"
        );
    }
    // The TLS and WebSocket crates and the in-memory pipe are what the dial tests are for.
    for name in crate_names() {
        assert!(
            violations("src/dial/tests.rs", &format!("use {name}::X;")).is_empty(),
            "{name}"
        );
    }
    assert!(violations("src/dial/tests.rs", "let (a, b) = tokio::io::duplex(64);").is_empty());
    // `connect_over` is not the real dial.
    let over = ["LiveWs::", "connect_over("].concat();
    assert!(violations("src/dial/tests.rs", &format!("let s = {over}s);")).is_empty());
}

/// Rule 11, the method forms (TH3 fix S-M1): the real connectors' `production(` constructors are
/// refused in dial test code however the connector is used afterwards, every dial test but the parent
/// is `offline_*` with the `in_child()` guard first, and a function with a connector does not hold the
/// production pins.
#[test]
fn rule_eleven_closes_the_method_forms() {
    let tests_rs = "src/dial/tests.rs";
    let (ws, https) = (["Live", "Ws"].concat(), ["Live", "Https"].concat());
    let one = |path: &str, text: &str| violations(path, text).len();
    // (1) `production(`: the connectors, an alias, a bare call, a path; not in a comment.
    for owner in [ws.as_str(), https.as_str(), "W", "super::ws::Alias"] {
        let call = format!("let c = {owner}::production();");
        assert_eq!(one(tests_rs, &call), 1, "{owner}");
        assert_eq!(one(tests_rs, &format!("fn f() {{ {call} }}")), 1, "{owner}");
        assert_eq!(one("src/dial/tests/certs.rs", &call), 1, "{owner}");
        assert_eq!(one(tests_rs, &format!("// {call}")), 0, "{owner}");
    }
    // The reviewer's mutant: a plain function that dials through the method form.
    let method = format!(
        "#[allow(dead_code)]\nfn x() {{ let live = super::ws::{ws}::production(); live.connect(r).await; {https}::production().execute(q).await; }}"
    );
    assert_eq!(one(tests_rs, &method), 2);
    assert_eq!(one(tests_rs, "let c = production();"), 1);
    for owner in PRODUCTION_DATA {
        assert_eq!(one(tests_rs, &format!("let c = {owner}::production();")), 0);
    }
    assert_eq!(one(tests_rs, "let p = super::production_params();"), 0);
    // A production file of the module: its test blocks count, its other code does not.
    let in_block = |body: &str| {
        format!("fn real() {{ {ws}::production(); }}\n#[cfg(test)]\nmod tests {{\n{body}\n}}")
    };
    assert_eq!(one("src/dial/ws.rs", &in_block("fn t() {}")), 0);
    assert_eq!(
        one(
            "src/dial/ws.rs",
            &in_block(&format!("fn t() {{ {https}::production(); }}"))
        ),
        1
    );
    // (2) the test functions.
    let guard = "if !in_child() {\n        return;\n    }";
    let tokio = format!("#[tokio::test]\nasync fn offline_x() {{\n    {guard}\n    let a = 1;\n}}");
    let plain = format!("#[test]\nfn offline_x() {{\n    {guard}\n    let a = 1;\n}}");
    assert_eq!(one(tests_rs, &tokio), 0);
    assert_eq!(one(tests_rs, &plain), 0);
    assert_eq!(
        one(
            tests_rs,
            &tokio.replace("tokio::test", "tokio::test(flavor = \"current_thread\")")
        ),
        0
    );
    // Another name: it runs in the parent process.
    assert_eq!(one(tests_rs, &plain.replace("offline_x", "x")), 1);
    assert_eq!(
        one(tests_rs, &plain.replace("offline_x", "the_offline_x")),
        1
    );
    // No guard, or a guard that is not first.
    assert_eq!(
        one(tests_rs, "#[test]\nfn offline_x() {\n    let a = 1;\n}"),
        1
    );
    assert_eq!(
        one(
            tests_rs,
            "#[test]\nfn offline_x() {\n    let a = 1;\n    if !in_child() { return; }\n}"
        ),
        1
    );
    assert_eq!(one(tests_rs, "#[tokio::test]\nasync fn x() {}"), 2);
    // The parent is the one exception; a helper that is no test needs no guard.
    let parent = format!("#[test]\nfn {DIAL_PARENT}() {{\n    if in_child() {{ return; }}\n}}");
    assert_eq!(one(tests_rs, &parent), 0);
    assert_eq!(one(tests_rs, "fn helper() {\n    let a = 1;\n}"), 0);
    assert_eq!(one("src/dial/ws.rs", &in_block("#[test]\nfn t() {}")), 2);
    // (3) a connector with the production pins in one function.
    let pins = production_pins();
    for pins in &pins {
        assert_eq!(
            one(
                tests_rs,
                &format!("fn f() {{ let w = {ws}::new(c, {pins}); }}")
            ),
            1
        );
        assert_eq!(
            one(
                tests_rs,
                &format!("fn f() {{ let w = {https}::new(c, {pins}); }}")
            ),
            1
        );
        assert_eq!(one(tests_rs, &format!("fn f() {{ let p = {pins}; }}")), 0);
    }
    assert_eq!(
        one(
            tests_rs,
            &format!("fn f() {{ let w = {ws}::new(c, test_pins()); }}")
        ),
        0
    );
    // Only the dial module's tests are held to it.
    assert_eq!(one("src/a.rs", &method), 0);
}

/// Rule 9 refuses what it is for, and the fake as it stands keeps it.
#[test]
fn rule_nine_keeps_the_fake_off_sockets_and_off_libthreemas_protocol_code() {
    let lib = "libthreema";
    let mock = |text: &str| mock_violations("tests/support/mock/d2m.rs", text);
    for text in [
        format!("use {lib}::protobuf::{{d2m, d2d}};"),
        format!("use {lib}::protobuf::d2m::ServerHello;"),
        format!("fn f() -> {lib}::protobuf::d2m::ServerInfo {{ todo!() }}"),
        format!("use {lib}::protobuf;"),
        format!("//! Never {lib}'s protocol code; {lib} is the client."),
        "use crate::support::mock::crypto;".to_owned(),
        "use crate::support::guards::counting_parts;".to_owned(),
        "use super::crypto::KeyPair;".to_owned(),
        "let text = include_str!(\"data.txt\");".to_owned(),
        "use kourier_threema::WsEvent;".to_owned(),
        // Only the whole segments `join`, `proto`, `reflect` and `send` are refused.
        "use kourier_threema::joined::X;".to_owned(),
        "use kourier_threema::protocols::X;".to_owned(),
        "use kourier_threema::reflected::X;".to_owned(),
        "use kourier_threema::sender::X;".to_owned(),
        "use kourier_threema::sends::X;".to_owned(),
    ] {
        assert!(mock(&text).is_empty(), "refused: {text}: {:?}", mock(&text));
    }
    for text in [
        format!("use {lib}::d2m::D2mProtocol;"),
        format!("use {lib}::{{protobuf::d2m, common::keys::DeviceGroupKey}};"),
        format!("use {lib}::{{\n    protobuf,\n    crypto,\n}};"),
        format!("use ::{lib}::csp::CspProtocol;"),
        format!("let k = {lib} :: common::keys::DeviceGroupKey::random();"),
        format!("use {lib}::protobufs::x;"),
        format!("use {lib} as lt;"),
        format!("extern crate {lib};"),
        "use crate::mock::crypto::unhex;".to_owned(),
        format!("use {};", ["std::", "net::", "Tcp", "Stream"].concat()),
        format!("use tokio::{{io, {}}};", ["n", "et"].concat()),
        // The three forms of the tests review (G3): a re-export through `crate::support`, a
        // `super::super::super` climb to the test modules, and a file pulled in.
        "fn f() -> usize { std::mem::size_of::<crate::support::LaunderedD2m>() }".to_owned(),
        "fn f() { let _ = &super::super::super::mock::unhex; }".to_owned(),
        "fn f() { let _ = &super :: super::mock::unhex; }".to_owned(),
        "use super::super::mock::unhex;".to_owned(),
        "include!(\"../../mock/d2m.rs\");".to_owned(),
        "#[path = \"../../mock/d2m.rs\"]\nmod laundered;".to_owned(),
        "const T: &str = include_str!(\"../../mock/d2m.rs\");".to_owned(),
        "const T: &[u8] = include_bytes!(\"../../mock/d2m.rs\");".to_owned(),
        "use crate::support::offline::run_child;".to_owned(),
        // TH4: Kourier's own join code, in every form.
        "use kourier_threema::join::qr::Offer;".to_owned(),
        "use kourier_threema::join;".to_owned(),
        "use kourier_threema::{proto::url, WsEvent};".to_owned(),
        "use ::kourier_threema::proto;".to_owned(),
        "fn f() { let _ = kourier_threema::join::emoji::emoji_of; }".to_owned(),
        // TH5 A2: Kourier's reflect module (the follower's parsers), in every form.
        "use kourier_threema::reflect::classify::Class;".to_owned(),
        "use kourier_threema::reflect;".to_owned(),
        "use kourier_threema::{reflect::body, WsEvent};".to_owned(),
        "use ::kourier_threema::reflect;".to_owned(),
        "fn f() { let _ = kourier_threema::reflect::body::text; }".to_owned(),
        // TH6a S2: Kourier's send core (its encode, bodies and queue), in every form.
        "use kourier_threema::send::encode::seal_envelope;".to_owned(),
        "use kourier_threema::send;".to_owned(),
        "use kourier_threema::{send::body, WsEvent};".to_owned(),
        "use ::kourier_threema::send;".to_owned(),
        "fn f() { let _ = kourier_threema::send::props::props; }".to_owned(),
        // TH6 L2a: Kourier's leader receive, in every form.
        "use kourier_threema::leader::zone::receive_inbound;".to_owned(),
        "use kourier_threema::leader;".to_owned(),
        "use ::kourier_threema::leader;".to_owned(),
        "fn f() { let _ = kourier_threema::leader::zone::inbound_row; }".to_owned(),
    ] {
        assert!(!mock(&text).is_empty(), "not refused: {text}");
    }
    // The fake's root takes no `super::` at all (it is `tests/support/`).
    assert!(
        !mock_violations("tests/support/mock/mod.rs", "use super::guards::x;").is_empty(),
        "super:: in the fake's mod.rs"
    );
    // A re-export of libthreema in any file under `tests/support/` (G3): the support module that
    // would launder it, whatever its visibility, and a type alias.
    for text in [
        format!("pub use {lib}::d2m::D2mProtocol as LaunderedD2m;"),
        format!("pub(crate) use {lib}::{{protobuf::d2m, d2m::D2mProtocol}};"),
        format!("pub(super) use {lib}::csp::CspProtocol;"),
        format!("pub type Laundered = {lib}::d2m::D2mProtocol;"),
        format!("mod m {{ pub use ::{lib}::common::keys::DeviceGroupKey; }}"),
    ] {
        for file in ["tests/support/mod.rs", "tests/support/guards.rs"] {
            assert!(
                !mock_violations(file, &text).is_empty(),
                "not refused in {file}: {text}"
            );
        }
    }
    for text in [
        format!("pub use {lib}::protobuf::d2m;"),
        format!("use {lib}::d2m::D2mProtocol;"),
        format!("pub fn f(p: &{lib}::d2m::D2mProtocol) {{}}"),
        "pub use std::sync::Arc;".to_owned(),
        "pub const PUBLIC_USE_OF_NOTHING: u8 = 1;".to_owned(),
    ] {
        assert!(
            mock_violations("tests/support/guards.rs", &text).is_empty(),
            "refused in support: {text}: {:?}",
            mock_violations("tests/support/guards.rs", &text)
        );
    }
    // Only the fake's directory: a test module may drive libthreema's client.
    assert!(
        mock_violations(
            "tests/mock/crypto.rs",
            &format!("use {lib}::d2m::D2mProtocol;")
        )
        .is_empty(),
        "rule 9 reached a test module"
    );
    // The fake as it stands: found, and clean.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join(MOCK_DIR), &mut files);
    assert!(
        files.len() >= 3,
        "the fake's files were not found ({})",
        files.len()
    );
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read");
        let relative = file
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        found.extend(mock_violations(&relative, &text));
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// Rule 10 (TH3): the request for messages is named nowhere in `src/` but `src/leader/`, comments
/// included; `tests/` may name it. Rule 4 sees a test that spawns through the harness (critic M2).
#[test]
fn rule_ten_keeps_the_request_for_messages_out_of_src_and_rule_four_sees_the_harness() {
    let name = unblock_name();
    for (path, text) in [
        ("src/conn/csp.rs", format!("    {name},\n")),
        ("src/a.rs", format!("// sends {name} later\n")),
        ("src/leaderx/a.rs", format!("x::{name}")),
    ] {
        let v = violations(path, &text);
        assert!(v.iter().any(|m| m.contains("rule 10")), "{path}: {v:?}");
    }
    assert!(violations("src/leader/mod.rs", &format!("x::{name}")).is_empty());
    assert!(violations("tests/mock/csp.rs", &format!("x::{name}")).is_empty());
    // TH6 L4b: the real leader names it (it builds the payload), the real connection does not (it passes it on).
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let read = |path: &str| std::fs::read_to_string(root.join(path)).expect("read");
    assert!(
        names(&read("src/leader/mod.rs"), &name),
        "src/leader/mod.rs no longer builds the unblock: the scan sees nothing there"
    );
    for path in ["src/conn/csp.rs", "src/conn/mod.rs"] {
        assert!(!names(&read(path), &name), "{path} names the unblock");
    }

    let spawn = "#[tokio::test]\nasync fn t() {\n    let h = harness::start(seed()).await;\n}";
    let v = violations("tests/connection/up.rs", spawn);
    assert!(
        v.len() == 1 && v[0].contains("spawns an actor"),
        "a harness test outside the child: {v:?}"
    );
    let via_helper = "async fn begin() -> H {\n    harness::start(seed()).await\n}\n#[tokio::test]\nasync fn t() {\n    begin().await;\n}";
    assert_eq!(violations("tests/connection/up.rs", via_helper).len(), 1);
    // Outside the actor tests' directory the words are only text (this file names them).
    assert!(violations("tests/a.rs", spawn).is_empty());
    let child = "#[tokio::test]\nasync fn t() {\n    if !support::offline::in_child() {\n        return;\n    }\n    let h = harness::start(seed()).await;\n}";
    assert!(violations("tests/connection/up.rs", child).is_empty());
    // TH4 J3: the join tests' harness is seen the same way.
    let v = violations("tests/join/steps.rs", spawn);
    assert!(
        v.len() == 1 && v[0].contains("spawns an actor"),
        "a join harness test outside the child: {v:?}"
    );
    assert!(violations("tests/join/steps.rs", child).is_empty());
}

/// Rule 16 (TH6 L4b): only the directory's file (and the trait, its re-export and the dial module) names the parts'
/// HTTPS client in the production code of `src/`; a comment, a test block and a test file may.
#[test]
fn rule_sixteen_lets_only_the_directory_talk_https() {
    for name in https_names() {
        let call = format!("fn f(&self) {{\n    let x = self.parts.{name};\n}}\n");
        for path in [
            "src/actor.rs",
            "src/conn/mod.rs",
            "src/leader/mod.rs",
            "src/leader/steps.rs",
            "src/leader/directory/call.rs",
            "src/send/attempt.rs",
            "src/join/mod.rs",
        ] {
            let v = violations(path, &call);
            assert!(
                v.iter().any(|m| m.contains("rule 16") && m.contains(&name)),
                "{path} {name}: {v:?}"
            );
        }
        for path in [
            "src/leader/directory.rs",
            "src/parts.rs",
            "src/lib.rs",
            "src/dial/https.rs",
        ] {
            assert!(
                https_violations(path, &call).is_empty(),
                "{path} {name} refused"
            );
        }
        let comment = format!("// {name}\n");
        assert!(https_violations("src/actor.rs", &comment).is_empty());
        let test_block = format!("#[cfg(test)]\nmod tests {{\n    fn f() {{ {name} }}\n}}\n");
        assert!(https_violations("src/actor.rs", &test_block).is_empty());
        assert!(https_violations("tests/connection/leader.rs", &call).is_empty());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let parts = std::fs::read_to_string(root.join("src/parts.rs")).expect("read");
    assert!(
        names(&production_code("src/parts.rs", &parts), &https_names()[0]),
        "src/parts.rs no longer names the trait: the needle finds nothing"
    );
    assert!(
        names(&production_code("src/parts.rs", &parts), &https_names()[3]),
        "src/parts.rs no longer names the request type: the needle finds nothing"
    );
}

/// Rule 16, P-L8 (TH6 FIX-B): an ordinary call that names neither the trait nor the field nor `execute(` with a request:
/// the client cloned into a local binding, the request built in a line of its own, `c.execute(req)`. Only the request
/// type's name shows it; a path, an alias and a `use` all name it. Home files, tests and comments stay free.
#[test]
fn rule_sixteen_sees_a_call_through_a_local_binding() {
    let names_of = https_names();
    let request = names_of[3].clone();
    let through_binding = format!(
        "async fn go(setup: &Setup) {{\n    let c = setup.transports.https.clone();\n    let req = crate::{request} {{ url, method, headers, body }};\n    c.execute(req).await;\n}}\n"
    );
    // The first three needles miss this text: that is the finding.
    let code = production_code("src/actor.rs", &through_binding);
    for old in &names_of[..3] {
        assert!(!names(&code, old), "{old} found it: no gap to close");
    }
    for path in [
        "src/actor.rs",
        "src/conn/mod.rs",
        "src/leader/mod.rs",
        "src/leader/steps.rs",
        "src/send/attempt.rs",
        "src/join/mod.rs",
    ] {
        let v = https_violations(path, &through_binding);
        assert!(
            v.iter()
                .any(|m| m.contains("rule 16") && m.contains(&request)),
            "{path}: {v:?}"
        );
        assert!(
            violations(path, &through_binding)
                .iter()
                .any(|m| m.contains("rule 16")),
            "{path}: the crate-wide check missed it"
        );
    }
    // An alias or a `use` names the type as well.
    for text in [
        format!("use crate::{request} as Q;\n"),
        format!("use crate::{{{request}, NetError}};\n"),
        format!("fn f() -> Option<crate::parts::{request}> {{ None }}\n"),
    ] {
        assert!(
            !https_violations("src/conn/mod.rs", &text).is_empty(),
            "{text}"
        );
    }
    // A longer name that merely contains the needle is not it.
    assert!(
        https_violations("src/conn/mod.rs", &format!("fn f(x: My{request}) {{}}\n")).is_empty()
    );
    // The homes, the dial module, a comment, a test block and a test file stay free.
    for path in [
        "src/leader/directory.rs",
        "src/parts.rs",
        "src/lib.rs",
        "src/dial/https.rs",
        "tests/connection/leader.rs",
    ] {
        assert!(
            https_violations(path, &through_binding).is_empty(),
            "{path}"
        );
    }
    assert!(https_violations("src/actor.rs", &format!("// crate::{request}\n")).is_empty());
    let in_test =
        format!("#[cfg(test)]\nmod tests {{\n    fn f() {{ crate::{request} {{ url }}; }}\n}}\n");
    assert!(https_violations("src/actor.rs", &in_test).is_empty());
}

/// Rule 12 (TH4 J3): only `src/conn/` and `src/join/` open a WebSocket through the parts' connector;
/// comment lines, the `#[cfg(test)]` blocks and test files may name it.
#[test]
fn rule_twelve_lets_only_the_connection_and_the_join_dial() {
    let actor =
        "fn f(&mut self) {\n    let x = self.transports.ws.connect(WsRequest { url, kind });\n}";
    for (path, text) in [
        ("src/actor.rs", actor),
        (
            "src/network.rs",
            "WsConnector::connect(&*ws, request).await",
        ),
        ("src/joiner/a.rs", "ws.connect(WsRequest {"),
        ("src/connx.rs", "t.ws.connect(request)"),
        (
            "src/parts.rs",
            "#[cfg(test)]\nfn helper();\nfn live() {\n    ws.connect(WsRequest {\n}",
        ),
    ] {
        let v = violations(path, text);
        assert!(v.iter().any(|m| m.contains("rule 12")), "{path}: {v:?}");
    }
    for (path, text) in [
        ("src/conn/mod.rs", actor),
        ("src/join/mod.rs", actor),
        (
            "src/actor.rs",
            "// the join calls .connect(WsRequest { .. })\nfn f() {}",
        ),
        ("src/actor.rs", "fn f(&mut self) {\n    self.connect();\n}"),
        (
            "src/parts.rs",
            "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n        Refused.connect(WsRequest {\n        });\n    }\n}\nfn b() {}",
        ),
        ("src/dial/tests.rs", "connector.connect(WsRequest {"),
        ("tests/join/steps.rs", "ws.connect(WsRequest {"),
    ] {
        let v = violations(path, text);
        assert!(!v.iter().any(|m| m.contains("rule 12")), "{path}: {v:?}");
    }
}

#[test]
fn rule_thirteen_keeps_the_test_doors_in_the_crate() {
    let store = store_path();
    // Refused: another crate names a door (as a path, a `use`, a brace group, or a bare name), or the
    // doc-hidden module.
    for (path, text) in [
        (
            "../kourier-app/src/threema.rs",
            format!("fn f() {{ {store}::ready_join(db, 1, w); }}"),
        ),
        (
            "../kourier-app/src/x.rs",
            format!("use {store}::{{import_join, load_identity}};"),
        ),
        (
            "../kourier-app/src/x.rs",
            "use kourier_threema::{ThreemaNetwork, store};".to_owned(),
        ),
        (
            "../kourier-app/tests/y.rs",
            "let i = Import::checked_for_tests(&bytes, vec![], 7);".to_owned(),
        ),
        (
            "../kourier-app/src/z.rs",
            "use kourier_threema::emoji_of_path_hash;".to_owned(),
        ),
        (
            "../kourier-core/src/w.rs",
            "// calls record_link once\nfn g() {}".to_owned(),
        ),
        (
            "../../xtask/src/main.rs",
            "stated_events(conn)".to_owned(),
        ),
        (
            "../kourier-app/src/v.rs",
            "mark_join_ready(tx, 1); is_join_ready(c); set_slot_existing(&db); add_nonce_batches(&db, &[], &[]);"
                .to_owned(),
        ),
        (
            "../kourier-app/src/u.rs",
            "let a = import_events(&i);".to_owned(),
        ),
        // TH5b B5: the door of two pairs and the clear.
        (
            "../kourier-core/src/t.rs",
            "fail_deferred_of_lengths(&[(1, 13)]);".to_owned(),
        ),
        (
            "../kourier-app/src/s.rs",
            "let n = clear_deferred(&db, &conv, cmd, 1, &wake);".to_owned(),
        ),
    ] {
        let v = violations(path, &text);
        assert!(v.iter().any(|m| m.contains("rule 13")), "{path}: {v:?}");
    }
    // Allowed: this crate's own files, and what another crate may use of the root API.
    for (path, text) in [
        (
            "src/actor.rs",
            "fn f() { store::ready_join(&self.db, 1, &self.wake); store::import_join(a, b, c, d); }"
                .to_owned(),
        ),
        (
            "tests/join/harness.rs",
            format!("{store}::ready_join(db, 1, w); {store}::mark_join_ready(tx, 1);"),
        ),
        (
            "tests/store.rs",
            "Import::checked_for_tests(&e, b, 1);".to_owned(),
        ),
        ("build.rs", "fn main() {}".to_owned()),
        // What the composition root may use of the root API (the names of rule 2 and 3 are built, as above).
        (
            "../kourier-app/src/threema.rs",
            format!(
                "kourier_threema::{}parts); kourier_threema::LINKING_BUILT;",
                network_call()
            ),
        ),
        (
            "../kourier-app/src/app.rs",
            format!(
                "Box::new({}); use kourier_threema::logs as threema;",
                entry_names().join(" ")
            ),
        ),
        // A longer name that merely contains a door's name is not it.
        (
            "../kourier-core/src/ok.rs",
            "fn my_ready_joined() {} fn import_join_x() {} let record_links = 1;".to_owned(),
        ),
        // The other crates' own `save_identity` (Signal) is no door of this crate.
        (
            "../kourier-signal/src/store/identity.rs",
            "fn save_identity(&self) {}".to_owned(),
        ),
    ] {
        let v = violations(path, &text);
        assert!(!v.iter().any(|m| m.contains("rule 13")), "{path}: {v:?}");
    }
    // A manifest that renames the package hides the crate's name from the scan.
    assert!(
        rename_violations(
            "../kourier-app/Cargo.toml",
            "kourier-threema = { workspace = true, optional = true }\n"
        )
        .is_empty()
    );
    assert!(
        rename_violations(
            "../kourier-app/Cargo.toml",
            "kt = { package = \"kourier-threema\", path = \"../kourier-threema\" }\n"
        )
        .len()
            == 1
    );
    assert!(
        rename_violations(
            "../kourier-app/Cargo.toml",
            "# package = \"kourier-threema\"\nserde = \"1\"\n"
        )
        .is_empty()
    );
}

/// Rule 14 (TH5 A10): the P5 open and seal functions are named by `src/reflect/` alone, `hash_with` by
/// `src/reflect/` and `src/store.rs` alone, in production code (comments and `#[cfg(test)]` blocks left
/// out). The refused cases cover every name of [`P5_BOXES`] in the places that are most likely to want
/// them, the near misses of the path (`src/reflect.rs`, `src/reflection.rs`, `src/storage.rs`) and the
/// code after a test block; the allowed cases cover the home, comments, test blocks, test files and
/// longer names. The last part reads the real `src/reflect/mod.rs`: the check must SEE the names where
/// they live, or an allowed place proves nothing.
///
/// TH6a S2 adds the send side: `src/send/encode.rs`, that path exactly, may name the three SEALS
/// ([`SEND_BOXES`]) and nothing else of the list (every open and decrypt is refused there, `hash_with` too);
/// the seals are refused in every other file of `src/send/`, in the near misses of the path
/// (`src/send/encoder.rs`, `src/send/encode/mod.rs`, `src/send.rs`) and in the connection and the actor; and
/// the real `src/send/encode.rs` is read like the real `src/reflect/mod.rs`, so the allowed place is real.
/// Rule 15 refuses the flag in the rest of `src/`, also in a comment, and the real homes name it, so the scan sees the names
/// where they live: the door opens only where the actor asks, and the parts of a real build do not name it.
#[test]
fn rule_fourteen_keeps_the_p5_crypto_in_the_reflect_directory() {
    let rule = |path: &str, text: &str| -> Vec<String> {
        violations(path, text)
            .into_iter()
            .filter(|m| m.contains("rule 14"))
            .collect()
    };
    // Refused: a P5 box outside `src/reflect/`, whatever the file (also `src/store.rs`, which may hash).
    for name in P5_BOXES {
        let call = format!("fn f(k: &Key) {{ let x = k.{name}(&bytes)?; }}");
        for path in [
            "src/actor.rs",
            "src/conn/mod.rs",
            "src/join/mod.rs",
            "src/store.rs",
            "src/network.rs",
            "src/reflect.rs",
            "src/reflection.rs",
            "src/leader/mod.rs",
        ] {
            if path.starts_with(LEADER_PREFIX) && LEADER_BOXES.contains(&name) {
                continue;
            }
            let v = rule(path, &call);
            assert!(
                v.len() == 1 && v[0].contains(name) && v[0].contains(path),
                "{path} {name}: {v:?}"
            );
        }
    }
    // Refused: `hash_with` outside `src/reflect/` and `src/store.rs`.
    for path in [
        "src/actor.rs",
        "src/conn/mod.rs",
        "src/conn/state.rs",
        "src/join/events.rs",
        "src/storage.rs",
        "src/reflect.rs",
        "src/reflection/x.rs",
    ] {
        let v = rule(path, "let h = nonce.hash_with(own).0;");
        assert!(v.len() == 1 && v[0].contains("hash_with"), "{path}: {v:?}");
    }
    // Refused: a name as a path, in a `use`, a bare call, after a test block, on a code line that ends
    // in a comment, and several names in one file (each is its own finding).
    for (path, text, count) in [
        (
            "src/actor.rs",
            "use libthreema::common::keys::DeviceGroupKey;\nfn f() { DeviceGroupKey::open_reflected(&k, b) }",
            1,
        ),
        (
            "src/conn/mod.rs",
            "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { k.open_reflected(b); }\n}\nfn live() { k.open_reflected(b); }",
            1,
        ),
        (
            "src/conn/mod.rs",
            "fn a() { n.hash_with(own); } // the nonce",
            1,
        ),
        (
            "src/conn/mod.rs",
            "let (n, p) = k.open_reflected(b)?; let h = n.hash_with(own); k.seal_reflected(p);",
            3,
        ),
        (
            "src/join/events.rs",
            "fn m() { k.decrypt_message(&n, &b); k.decrypt_metadata(&n, &b); }",
            2,
        ),
    ] {
        let v = rule(path, text);
        assert!(v.len() == count, "{path}: {v:?}");
    }
    // Allowed: the home of both, the store's hash, comments, test blocks, test files and longer names.
    for (path, text) in [
        (
            "src/reflect/mod.rs",
            "fn f() { k.open_reflected(b); n.hash_with(own); k.seal_reflected(p); k.decrypt_message(&n, &b); }",
        ),
        (
            "src/reflect/classify.rs",
            "k.open_device_info(b); k.seal_transaction_scope(p);",
        ),
        (
            "src/reflect/deep/x.rs",
            "k.encrypt_metadata(&n, p); k.open_transaction_scope(b);",
        ),
        (
            "src/store.rs",
            "fn h(n: &Nonce) -> [u8; 32] { n.hash_with(own).0 }",
        ),
        (
            "src/actor.rs",
            "// the loop opens it with open_reflected and hashes with hash_with\n/// seal_reflected, decrypt_message\nfn f() {}",
        ),
        (
            "src/conn/mod.rs",
            "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { k.open_reflected(b); n.hash_with(i); k.encrypt_message(&n, p); }\n}\nfn b() {}",
        ),
        ("src/dial/tests.rs", "k.open_reflected(b); n.hash_with(i);"),
        (
            "src/conn/tests/x.rs",
            "k.open_reflected(b); n.hash_with(i);",
        ),
        (
            "tests/mock/crypto.rs",
            "k.open_reflected(b); n.hash_with(i); k.seal_reflected(p);",
        ),
        (
            "tests/connection/reflect.rs",
            "key.open_reflected(&r.envelope); n.hash_with(own);",
        ),
        ("../kourier-app/src/threema.rs", "k.open_reflected(b);"),
        (
            "src/actor.rs",
            "fn reopen_reflected_x() {} let hash_with_key = 1; fn unhash_with() {} fn my_decrypt_messages() {} let open_reflecteds = 1;",
        ),
    ] {
        let v = rule(path, text);
        assert!(v.is_empty(), "{path}: {v:?}");
    }
    // The check sees the real home: the follower's module names both, so the allowed place is real.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = std::fs::read_to_string(root.join("src/reflect/mod.rs")).expect("read");
    let code = production_code("src/reflect/mod.rs", &home);
    assert!(
        names(&code, "open_reflected") && names(&code, NONCE_HASH),
        "src/reflect/mod.rs no longer names the P5 open and hash: the scan sees nothing there"
    );
    assert!(
        rule("src/actor.rs", &code).len() >= 2,
        "the real code, moved out, is not refused"
    );
    assert!(rule("src/reflect/mod.rs", &code).is_empty());

    // THE SEND SIDE (TH6a S2). The list of seals is exactly the three, all in the list of P5.
    assert_eq!(
        SEND_BOXES,
        ["seal_reflected", "encrypt_message", "encrypt_metadata"]
    );
    assert!(SEND_BOXES.iter().all(|name| P5_BOXES.contains(name)));
    // Refused: a seal anywhere in `src/` but the encode file and `src/reflect/`: the other files of `src/send/`,
    // the near misses of the encode file's path, the connection and the actor.
    for name in SEND_BOXES {
        let call = format!("fn f(k: &Key) {{ let x = k.{name}(&bytes)?; }}");
        for path in [
            "src/send/queue.rs",
            "src/send/mod.rs",
            "src/send/body.rs",
            "src/send/props.rs",
            "src/send/machine.rs",
            "src/send/prepare.rs",
            "src/send/encoder.rs",
            "src/send/encode/mod.rs",
            "src/send/encode_more.rs",
            "src/send/sub/encode.rs",
            "src/send.rs",
            "src/conn/mod.rs",
            "src/actor.rs",
        ] {
            let v = rule(path, &call);
            assert!(
                v.len() == 1 && v[0].contains(name) && v[0].contains(path),
                "{path} {name}: {v:?}"
            );
        }
    }
    // Refused in the encode file itself: every open and decrypt of the list, and `hash_with`.
    for name in P5_BOXES.iter().filter(|name| !SEND_BOXES.contains(name)) {
        let call = format!("fn f(k: &Key) {{ let x = k.{name}(&bytes)?; }}");
        let v = rule(SEND_ENCODE_FILE, &call);
        assert!(
            v.len() == 1 && v[0].contains(name) && v[0].contains(SEND_ENCODE_FILE),
            "{SEND_ENCODE_FILE} {name}: {v:?}"
        );
    }
    for name in ["decrypt_message", "decrypt_metadata", "open_reflected"] {
        assert!(!SEND_BOXES.contains(&name), "{name} is not a seal");
    }
    let v = rule(SEND_ENCODE_FILE, "let h = nonce.hash_with(own).0;");
    assert!(v.len() == 1 && v[0].contains("hash_with"), "{v:?}");
    // One seal named next to an open is one finding (the open), not none.
    let v = rule(
        SEND_ENCODE_FILE,
        "fn f() { k.encrypt_message(&n, p); k.decrypt_message(&n, b); k.seal_reflected(p); }",
    );
    assert!(v.len() == 1 && v[0].contains("decrypt_message"), "{v:?}");
    // Allowed: the three seals in the encode file, in code, after a comment, in a test block; and a longer name.
    for text in [
        "fn f() { k.encrypt_message(&n, p); k.encrypt_metadata(&n, m); k.seal_reflected(e); }",
        "// k.decrypt_message(&n, b)\nfn f() { k.seal_reflected(e); }",
        "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { k.decrypt_message(&n, b); k.open_reflected(r); }\n}\nfn b() {}",
        "fn f() { k.my_decrypt_messages(); let encrypt_messages = 1; }",
    ] {
        let v = rule(SEND_ENCODE_FILE, text);
        assert!(v.is_empty(), "{text}: {v:?}");
    }
    // The check sees the real encode file: it names all three seals (or the allowed place proves nothing), names no
    // open, decrypt or `hash_with`, and moved to another file of `src/send/` it is refused once per seal.
    let encode = std::fs::read_to_string(root.join(SEND_ENCODE_FILE)).expect("read");
    let code = production_code(SEND_ENCODE_FILE, &encode);
    for name in SEND_BOXES {
        assert!(
            names(&code, name),
            "{SEND_ENCODE_FILE} no longer names {name}: the scan sees nothing there"
        );
    }
    assert!(rule(SEND_ENCODE_FILE, &code).is_empty());
    assert_eq!(
        rule("src/send/queue.rs", &code).len(),
        SEND_BOXES.len(),
        "the real encode code, moved out, is refused once per seal"
    );
    // And the rest of `src/send/` as it stands is clean, whatever it names.
    let mut send_files = Vec::new();
    collect(&root.join("src/send"), &mut send_files);
    assert!(
        send_files.len() >= 4,
        "the send core's files were not found ({})",
        send_files.len()
    );
    for file in send_files {
        let relative = file
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(rule(&relative, &text).is_empty(), "{relative}");
    }

    // THE LEADER SIDE (TH6 L3): the opens of a received message, under `src/leader/` only.
    assert_eq!(LEADER_BOXES, ["decrypt_message", "decrypt_metadata"]);
    assert!(LEADER_BOXES.iter().all(|name| P5_BOXES.contains(name)));
    assert!(LEADER_BOXES.iter().all(|name| !SEND_BOXES.contains(name)));
    // Refused: each open anywhere in `src/` but `src/leader/` and `src/reflect/`: the send core (its encode file too),
    // the connection, the actor, the store, and the near misses of the prefix.
    for name in LEADER_BOXES {
        let call = format!("fn f(k: &Key) {{ let x = k.{name}(&n, &bytes)?; }}");
        for path in [
            SEND_ENCODE_FILE,
            "src/send/prepare.rs",
            "src/conn/mod.rs",
            "src/conn/csp.rs",
            "src/actor.rs",
            "src/store.rs",
            "src/leader.rs",
            "src/leaders/open.rs",
            "src/leader_open.rs",
            "src/sub/leader/open.rs",
        ] {
            let v = rule(path, &call);
            assert!(
                v.len() == 1 && v[0].contains(name) && v[0].contains(path),
                "{path} {name}: {v:?}"
            );
        }
        // Allowed: in any file of the leader's directory, in code.
        for path in [
            "src/leader/open.rs",
            "src/leader/mod.rs",
            "src/leader/deep/x.rs",
        ] {
            assert!(rule(path, &call).is_empty(), "{path} {name}");
        }
    }
    // Refused in `src/leader/`: every seal and every other open of the list, and `hash_with`.
    for name in P5_BOXES.iter().filter(|name| !LEADER_BOXES.contains(name)) {
        let call = format!("fn f(k: &Key) {{ let x = k.{name}(&bytes)?; }}");
        for path in [
            "src/leader/open.rs",
            "src/leader/steps.rs",
            "src/leader/mod.rs",
        ] {
            let v = rule(path, &call);
            assert!(
                v.len() == 1 && v[0].contains(name) && v[0].contains(path),
                "{path} {name}: {v:?}"
            );
        }
    }
    let v = rule("src/leader/open.rs", "let h = nonce.hash_with(own).0;");
    assert!(v.len() == 1 && v[0].contains("hash_with"), "{v:?}");
    // The check sees the real open file: it names both opens (or the allowed place proves nothing), names no seal and no
    // `hash_with`, and moved out of the directory it is refused once per open.
    let open_file = "src/leader/open.rs";
    let text = std::fs::read_to_string(root.join(open_file)).expect("read");
    let code = production_code(open_file, &text);
    for name in LEADER_BOXES {
        assert!(names(&code, name), "{open_file} no longer names {name}");
    }
    assert!(rule(open_file, &code).is_empty());
    assert_eq!(
        rule("src/conn/mod.rs", &code).len(),
        LEADER_BOXES.len(),
        "the real open code, moved out, is refused once per open"
    );
    // And the whole of `src/leader/` as it stands is clean.
    let mut leader_files = Vec::new();
    collect(&root.join("src/leader"), &mut leader_files);
    assert!(leader_files.len() >= 5, "the leader's files were not found");
    for file in leader_files {
        let relative = file
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(rule(&relative, &text).is_empty(), "{relative}");
    }
}

/// THE LOG RULE'S LIST OF TRANSPORT CRATES (TH4 J7): `kourier_threema::logs` makes a record of the dial's TLS,
/// WebSocket and HTTP crates upstream's (written from WARN up, message only), in a build without WhatsApp
/// too. The list lives in `src/dial/modules.rs` (this file may not write the names), so the checks that need
/// the names live here: every name in the list is a library of a package in the lock, and every direct
/// dependency of this crate that rule 1 counts as a transport crate is named by the rule or listed here with
/// the reason it logs nothing. A new dependency of that kind (QUIC, SOCKS, another client) stops this test
/// until someone decides.
#[test]
fn the_log_rule_names_the_transport_crates_the_dial_links() {
    use kourier_threema::logs::{TRANSPORT_MODULES, is_upstream};

    // The library name of the TLS crate's web-PKI verifier is its last word.
    let last_word = ["web", "pki"].concat();
    let module_of = |package: &str| {
        let module = package.replace('-', "_");
        if module.ends_with(&format!("_{last_word}")) {
            last_word.clone()
        } else {
            module
        }
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock: toml::Table = std::fs::read_to_string(root.join("Cargo.lock"))
        .expect("the workspace's Cargo.lock")
        .parse()
        .expect("Cargo.lock is TOML");
    let modules: Vec<String> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .expect("the lock lists its packages")
        .iter()
        .filter_map(|package| package.get("name").and_then(toml::Value::as_str))
        .map(module_of)
        .collect();
    for name in TRANSPORT_MODULES {
        assert!(
            modules.iter().any(|module| module == name),
            "{name} is in the transport list but no package of the lock has that library name"
        );
    }

    // The dependencies of this crate that rule 1 counts as transport crates, and why two are not in the list.
    let not_logging: [(String, &str); 2] = [
        (
            ["ht", "tp"].concat(),
            "the request and header types: no log call",
        ),
        (
            ["webpki_", "roots"].concat(),
            "the Mozilla roots as data: no log call",
        ),
    ];
    let manifest: toml::Table =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("this crate's manifest")
            .parse()
            .expect("a manifest is TOML");
    let needles = crate_names();
    let mut checked = 0;
    for dependency in manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .expect("a dependencies table")
        .keys()
    {
        let module = dependency.replace('-', "_");
        if !needles.contains(&module) || not_logging.iter().any(|(name, _)| *name == module) {
            continue;
        }
        assert!(
            is_upstream(&module),
            "the dependency `{dependency}` is a transport crate and the log rule does not name it: add it to \
             src/dial/modules.rs or list it here with the reason it logs nothing"
        );
        checked += 1;
    }
    // Not vacuous: TLS, the TLS stream, the WebSocket client and the HTTPS client are the dial's own.
    assert!(
        checked >= 4,
        "only {checked} transport dependencies were found"
    );
    for (name, why) in &not_logging {
        assert!(!is_upstream(name), "{name} ({why}) is named by the rule");
    }
}
