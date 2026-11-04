//! T-10 (W2, chunk C6a): what upstream's lines come out as. The lines are the SHAPES of the real ones
//! (the format strings are quoted from the pinned upstream source: `node_io.rs` for the connect failures,
//! the stream error and `Received unhandled IQ`, `pair.rs` for the own LID-PN mapping, `pair_code.rs` and
//! `bot.rs` for the pair code and number, `lid_pn.rs` for the migrated identity, `retry.rs`, `polls.rs`
//! and `sender_key_device_cache.rs` for the JIDs, `session_cipher.rs` for the identity key), with
//! invented data: nothing in this file is a real number, name or token.

use super::*;

const PHONE: &str = "15550100123";
const LID: &str = "123456789012345";
const NAME: &str = "Alice Example";
const TOKEN: &str = "dGVzdC1hcHBlYWwtdG9rZW4tMTIzNDU2Nzg5MA";
const CODE: &str = "WXYZPQRS";
const BYTES: &str = "[5, 161, 178, 19, 200, 12, 77, 90, 3, 8, ...]";
const MSG_ID: &str = "3EB0A1B2C3D4E5F67890";
const KEY_HEX: &str = "05a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// The lines and what each comes out as.
fn corpus() -> Vec<(String, String)> {
    let line = |text: String, expected: &str| (text, expected.to_owned());
    vec![
        // node_io.rs: an account lock's one-time appeal token, in a stanza dump.
        line(
            format!(
                "Got AccountLocked connect failure, logging out: <failure location=\"rva\" reason=\"403\"><locked appeal_token=\"{TOKEN}\" violation_reason=\"other_harm\" vt=\"1700000000\"/></failure>"
            ),
            "Got AccountLocked connect failure, logging out: <failure><locked>",
        ),
        line(
            format!(
                "Temporary ban connect failure: <failure code=\"101\" expire=\"60\" reason=\"402\" url=\"https://www.example.invalid/support?token={TOKEN}\"/>"
            ),
            "Temporary ban connect failure: <failure>",
        ),
        line(
            "Unknown connect failure: <failure location=\"rva\" reason=\"500\"/>".to_owned(),
            "Unknown connect failure: <failure>",
        ),
        line(
            format!(
                "Unknown stream error: <stream:error code=\"515\"><text>session expired for {PHONE}</text></stream:error>"
            ),
            "Unknown stream error: <stream:error><text>",
        ),
        // handlers/iq.rs: the sender's push name is the `notify` attribute.
        line(
            format!(
                "Received unhandled IQ: <iq from=\"s.whatsapp.net\" id=\"1234.5678-9\" notify=\"{NAME}\" type=\"get\" xmlns=\"urn:xmpp:ping\"><ping/></iq>"
            ),
            "Received unhandled IQ: <iq><ping>",
        ),
        // pair.rs: the raw users of the own LID and phone JIDs.
        line(
            format!("Failed to persist own LID-PN mapping {LID} <-> {PHONE}: database is locked"),
            "Failed to persist own LID-PN mapping … <-> …: database is locked",
        ),
        // pair_code.rs and bot.rs: the number and the code, at INFO.
        line(
            format!("Starting pair code authentication for phone: {PHONE}"),
            "Starting pair code authentication for phone: …",
        ),
        line(
            format!("Stage 1 complete, waiting for phone confirmation. Code: {CODE}"),
            "Stage 1 complete, waiting for phone confirmation. Code: …",
        ),
        line(
            format!("Pair code generated: {CODE}"),
            "Pair code generated: …",
        ),
        // lid_pn.rs: ProtocolAddress, `<name>.<device>`.
        line(
            format!("Migrated identity {PHONE}.0 -> {LID}.0"),
            "Migrated identity ….0 -> ….0",
        ),
        line(
            format!("Failed to decrypt vote from {PHONE}:7@s.whatsapp.net: invalid mac"),
            "Failed to decrypt vote from …@s.whatsapp.net: invalid mac",
        ),
        line(
            format!("Skipping malformed device JID '{PHONE}@c.us': bad device"),
            "Skipping malformed device JID '…@c.us': bad device",
        ),
        line(
            format!("TypedCache[tc_token]: get({LID}@lid) error: timeout"),
            "TypedCache[tc_token]: get(…@lid) error: timeout",
        ),
        line(
            format!("handle_retry_receipt: device not found for device=3, user={PHONE}"),
            "handle_retry_receipt: device not found for device=3, user=…",
        ),
        // session_cipher.rs: an identity key in hex and a ProtocolAddress.
        line(
            format!("Identity key {KEY_HEX} is not trusted for remote address {LID}.2"),
            "Identity key … is not trusted for remote address ….2",
        ),
        // Review F3: a key printed as a byte list, and a name printed unquoted (a format string that
        // says "name"). Neither shape is in the pinned upstream; they are the guard for a pin bump.
        line(
            format!("identity changed: {BYTES}"),
            "identity changed: […]",
        ),
        line(
            format!("push name {NAME} rejected for {PHONE}"),
            "push name …",
        ),
        // A server text that holds a name and a number, quoted.
        line(
            format!(
                "Group history send outcome is indeterminate: unexpected response \"{NAME} (+{PHONE}) left\""
            ),
            "Group history send outcome is indeterminate: unexpected response \"…\"",
        ),
        // history_sync.rs (W4, the capture path): the first carries the notice's message id and what the
        // capture hook returned (`{:?}` of its error, a static text of ours), the second the download's error
        // (a status, or a transport error whose text can hold the signed media URL).
        line(
            format!(
                "History sync {MSG_ID} was not captured; withholding its receipt: the history chunk was not kept"
            ),
            "History sync … was not captured; withholding its receipt: the history chunk was not kept",
        ),
        line(
            "Failed to download history sync blob: Download failed with status: 410".to_owned(),
            "Failed to download history sync blob: Download failed with status: 410",
        ),
        line(
            format!(
                "Failed to download history sync blob: error sending request for url (https://mmg.example.invalid/v/t62.7118-24/12345678_9876543210?ccb=11-4&oh={TOKEN}&oe=6532ABCD&_nc_sid=5e03e0): connection reset"
            ),
            "Failed to download history sync blob: error sending request for url (https://mmg.example.invalid/v/t62.…?ccb=11-4&oh=…&oe=6532ABCD&_nc_sid=5e03e0): connection reset",
        ),
    ]
}

/// What no output of [`redact_upstream`] may hold, whatever the rules did.
fn assert_nothing_to_read(out: &str) {
    for secret in [
        PHONE,
        LID,
        NAME,
        TOKEN,
        CODE,
        KEY_HEX,
        "161, 178",
        "appeal_token",
        "notify",
    ] {
        assert!(!out.contains(secret), "{out:?} still holds {secret:?}");
    }
    let digits = out
        .split(|c: char| !c.is_ascii_digit())
        .map(str::len)
        .max()
        .unwrap_or(0);
    assert!(digits < 6, "{out:?} holds a run of {digits} digits");
    for (at, _) in out.match_indices('@') {
        let before = out[..at].chars().next_back();
        assert!(
            !before.is_some_and(|c| c.is_ascii_alphanumeric()),
            "{out:?} holds a JID user"
        );
    }
}

#[test]
fn the_real_line_shapes_come_out_hidden_and_stay_readable() {
    for (line, expected) in corpus() {
        let out = redact_upstream(&line);
        assert_eq!(out, expected, "from {line:?}");
        assert_nothing_to_read(&out);
        // Applied twice it changes nothing: a line may pass a second filter.
        assert_eq!(redact_upstream(&out), out, "not stable: {out:?}");
    }
}

#[test]
fn a_stanza_dump_is_cut_to_the_names_of_its_open_tags() {
    let cut = |text: &str| redact_upstream(text);
    // Everything from the first tag on goes: attributes (with a `>` inside a quote), text, closing tags.
    assert_eq!(
        cut(r#"x: <a b="1 > 2" c="<d>"><e>text &gt; <f/></e></a>"#),
        "x: <a><e><f>"
    );
    // A dump without a closing tag, one that stops short, and a stanza at the start of the line.
    assert_eq!(cut(r#"<iq id="7" type="get""#), "<iq>");
    assert_eq!(cut("<ack/>"), "<ack>");
    // The prefix before the dump is kept as it is.
    assert_eq!(
        cut(r#"Dropping stanza (3 attrs): <message id="9"/>"#),
        "Dropping stanza (3 attrs): <message>"
    );
    // Text that only has a `<` in it is none: generic types, arrows, comparisons, a lone bracket.
    for plain in [
        "expected Option<Jid>, got None: bad",
        "Vec<u8> was too short",
        "a < b and c <-> d and e <= f",
        "ends with <",
        "<3 hearts",
        "Result<(), Error> failed",
    ] {
        assert_eq!(cut(plain), plain);
    }
}

#[test]
fn quoted_strings_and_attribute_values_are_hidden() {
    let hide = |text: &str| redact_upstream(text);
    assert_eq!(
        hide(r#"reason: "other_harm", code: 7"#),
        r#"reason: "…", code: 7"#
    );
    // An escaped quote does not end the string; an empty string is hidden as well.
    assert_eq!(hide(r#"a "b \" c" d "" e"#), r#"a "…" d "…" e"#);
    // A string that is not closed runs to the end of the line.
    assert_eq!(
        hide(r#"before "Alice Exam"#),
        r#"before ""#.to_owned() + "…"
    );
    // Single quotes are not hidden (an apostrophe is not a string): the JID rule covers 'jid'.
    assert_eq!(hide("it's fine, isn't it"), "it's fine, isn't it");
}

#[test]
fn the_pair_code_and_the_number_after_their_labels_are_hidden_whatever_they_are_made_of() {
    let hide = |text: &str| redact_upstream(text);
    // Letters alone: no digit rule and no token rule sees this one.
    assert_eq!(
        hide("Pair code generated: ABCDEFGH"),
        "Pair code generated: …"
    );
    assert_eq!(
        hide("Pair code generated: ABCD-EFGH, ok"),
        "Pair code generated: … ok"
    );
    // Case does not matter, spaces after the label are kept, an end of line right after it is fine.
    assert_eq!(
        hide("PAIR CODE GENERATED:   abcdefgh"),
        "PAIR CODE GENERATED:   …"
    );
    assert_eq!(hide("Pair code generated:"), "Pair code generated:");
    assert_eq!(hide("Pair code generated: "), "Pair code generated: ");
    // A number of any length after "for phone:".
    assert_eq!(
        hide("authentication for phone: 12345"),
        "authentication for phone: …"
    );
    // A word of another line that merely has "code" in it is left.
    assert_eq!(hide("exit code: 515"), "exit code: 515");
    // Lines with non-ASCII text before a label, and a label cut by a multi-byte character.
    assert_eq!(
        hide("Ключ: Pair code generated: ABCD"),
        "Ключ: Pair code generated: …"
    );
    assert_eq!(hide("Pair code generatedé: AB"), "Pair code generatedé: AB");
}

#[test]
fn the_user_of_a_jid_is_hidden_for_every_server_and_device() {
    let hide = |text: &str| redact_upstream(text);
    for (jid, expected) in [
        (format!("{PHONE}@s.whatsapp.net"), "…@s.whatsapp.net"),
        (format!("{PHONE}:7@s.whatsapp.net"), "…@s.whatsapp.net"),
        (format!("{PHONE}.0:7@s.whatsapp.net"), "…@s.whatsapp.net"),
        (format!("{PHONE}@c.us"), "…@c.us"),
        (format!("{LID}@lid"), "…@lid"),
        (format!("{LID}:3@lid"), "…@lid"),
        (format!("{PHONE}-1700000000@g.us"), "…@g.us"),
        ("120363000000000000@newsletter".to_owned(), "…@newsletter"),
        ("status@broadcast".to_owned(), "…@broadcast"),
        ("x@hosted.lid".to_owned(), "…@hosted.lid"),
        ("alice.example@example.org".to_owned(), "…@example.org"),
        ("a@b".to_owned(), "…@b"),
    ] {
        assert_eq!(hide(&jid), expected, "from {jid:?}");
        assert_eq!(
            hide(&format!("from {jid}, retry")),
            format!("from {expected}, retry")
        );
    }
    // An at sign with no user before it or no server after it is not a JID.
    assert_eq!(hide("ask @lid now"), "ask @lid now");
    assert_eq!(hide("name@ now"), "name@ now");
    assert_eq!(hide("name@"), "name@");
    // A long user with a device mark is one mark.
    assert_eq!(hide(&format!("{LID}0000000000000000:5@lid")), "…@lid");
    assert_eq!(hide("…@lid"), "…@lid");
}

#[test]
fn a_run_of_six_digits_is_hidden_and_a_shorter_one_is_not() {
    let hide = |text: &str| redact_upstream(text);
    assert_eq!(hide("12345"), "12345");
    assert_eq!(hide("123456"), "…");
    assert_eq!(hide("1234567890"), "…");
    assert_eq!(
        hide("port 8080, device 12, 99999 items"),
        "port 8080, device 12, 99999 items"
    );
    assert_eq!(hide("1,234,567 rows"), "1,234,567 rows");
    assert_eq!(hide("user=123456."), "user=….");
    assert_eq!(hide("a1234567b"), "a…b");
    assert_eq!(hide("123456.7 and 12345.6"), "….7 and 12345.6");
}

#[test]
fn a_long_run_that_may_be_a_token_or_a_key_is_hidden_and_an_identifier_is_not() {
    let hide = |text: &str| redact_upstream(text);
    // 15 characters never; 16 with a digit, a plus, a slash or an equals sign yes; 16 letters no.
    assert_eq!(hide("abcdefghij12345"), "abcdefghij12345");
    assert_eq!(hide("abcdefghijk12345"), "…");
    assert_eq!(hide("abcdefghijklmno+"), "…");
    assert_eq!(hide("abcdefghijklmno/"), "…");
    assert_eq!(hide("abcdefghijklmno="), "…");
    assert_eq!(hide("abcdefghijklmnop"), "abcdefghijklmnop");
    // 32 characters yes, whatever they are; 31 letters no.
    assert_eq!(hide(&"q".repeat(31)), "q".repeat(31));
    assert_eq!(hide(&"q".repeat(32)), "…");
    // Base64 with padding and base64url, hex, and one inside text.
    assert_eq!(hide("tok dGVzdC1hcHBlYWwtdG9rZW4= end"), "tok … end");
    assert_eq!(hide("tok dGVzdC1hcHBl-WwtdG9r_W4x end"), "tok … end");
    assert_eq!(hide(&format!("hash {KEY_HEX}.")), "hash ….");
    assert_eq!(hide("short dGVzdA== ok"), "short dGVzdA== ok");
    // A function's name is readable.
    assert_eq!(
        hide("migrate_identities_pairing failed"),
        "migrate_identities_pairing failed"
    );
}

#[test]
fn a_line_that_names_nobody_is_left_as_it_is() {
    for plain in [
        "Pre-key upload failed, retrying in 5s: connection reset",
        "prekey window [12, 40) fully consumed; generating fresh",
        "Client lifecycle: shutdown timed out after 5 s",
        "wanted_pre_key_count 5000 out of range, clamped to 812",
        "",
    ] {
        assert_eq!(redact_upstream(plain), plain);
    }
}

#[test]
fn the_module_path_decides_what_is_upstream() {
    for name in UPSTREAM_MODULES {
        assert!(is_upstream(name), "{name}");
        assert!(is_upstream(&format!("{name}::a::b")), "{name}::a::b");
    }
    // The family of the workspace's own crates, without naming the real ones here.
    assert!(is_upstream(&format!("{UPSTREAM_FAMILY}something")));
    assert!(is_upstream(&format!("{UPSTREAM_FAMILY}something::a::b")));
    for other in [
        "",
        "kourier_whatsapp",
        "kourier_whatsapp::actor",
        "kourier_signal::connection",
        "libsignal_net::chat",
        "whatsapp_rustic::x",
        "whatsapp_rust2",
        "wacore_other",
        "wacore2::x",
        "ureq2",
        "tokio::runtime",
        "hyper::client",
        "std::io",
        // A custom target is not a module path.
        "Client/AppState",
        "Bot/PairCode",
        // The name is the first segment, not any of them.
        "kourier_whatsapp::wacore",
        "x::whatsapp_rust",
    ] {
        assert!(!is_upstream(other), "{other}");
    }
}

#[test]
fn upstream_is_written_from_warn_up_and_everyone_else_as_the_filters_say() {
    // tracing orders by verbosity: ERROR is the least, TRACE the most.
    assert!(Level::ERROR < Level::WARN && Level::WARN < Level::INFO && Level::INFO < Level::TRACE);
    let upstream = format!("{UPSTREAM_FAMILY}something::a");
    for module in [
        "whatsapp_rust::client::lid_pn",
        "wacore::iq::usync",
        "ureq::run",
        &upstream,
    ] {
        assert!(keeps(module, Level::ERROR), "{module}");
        assert!(keeps(module, Level::WARN), "{module}");
        for level in [Level::INFO, Level::DEBUG, Level::TRACE] {
            assert!(!keeps(module, level), "{module} {level}");
        }
    }
    for module in [
        "kourier_whatsapp::actor",
        "libsignal_net::chat",
        "",
        "Client/AppState",
    ] {
        for level in [
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ] {
            assert!(keeps(module, level), "{module} {level}");
        }
    }
}

/// `observe_str` is upstream's own redaction for the lines it writes with a JID in a field: it must
/// hide a phone number. Its feature `tracing-pii` prints raw numbers; if any crate of the build turned
/// it on (feature unification would do it for the whole build), this fails.
#[test]
fn upstream_hides_a_phone_jid_unless_a_crate_of_the_build_turned_its_numbers_on() {
    let shown = wacore_binary::jid::observe_str(&format!("{PHONE}@s.whatsapp.net"));
    assert!(!shown.contains(PHONE), "tracing-pii is on: {shown}");
    assert!(shown.starts_with("pn#"), "{shown}");
    assert!(shown.ends_with("@s.whatsapp.net"), "{shown}");
    // A string that is no JID gets a token, never itself.
    let token = wacore_binary::jid::observe_str(PHONE);
    assert!(!token.contains(PHONE), "{token}");
    assert!(token.starts_with("?#"), "{token}");
    // A LID is pseudonymous and is shown as it is.
    assert_eq!(
        wacore_binary::jid::observe_str(&format!("{LID}@lid")),
        format!("{LID}@lid")
    );
}

/// The packages of the build, from `cargo metadata` of this workspace (offline, from the lock): the
/// library name of each, and the source it comes from.
fn graph() -> Vec<(String, String)> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).expect("metadata is JSON");
    let mut libs = Vec::new();
    for package in meta["packages"].as_array().expect("packages") {
        let source = package["source"].as_str().unwrap_or("").to_owned();
        for target in package["targets"].as_array().expect("targets") {
            let kinds = target["kind"].as_array().expect("kind");
            let is_lib = kinds
                .iter()
                .any(|k| matches!(k.as_str(), Some("lib" | "rlib" | "proc-macro")));
            if is_lib {
                let name = target["name"].as_str().expect("name").replace('-', "_");
                libs.push((name, source.clone()));
            }
        }
    }
    libs
}

#[test]
fn the_module_list_matches_the_crates_of_the_build() {
    let libs = graph();
    // The workspace of whatsapp-rust is the source of the crate `whatsapp_rust`; every crate that comes
    // from it is upstream's. A crate a pin bump adds fails here until the list names it.
    let source = libs
        .iter()
        .find(|(name, _)| name == "whatsapp_rust")
        .map(|(_, source)| source.clone())
        .expect("whatsapp_rust is in the graph");
    assert!(source.starts_with("git+"), "{source}");
    let theirs: Vec<&str> = libs
        .iter()
        .filter(|(_, s)| *s == source)
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(theirs.len() >= 8, "only {theirs:?} come from {source}");
    for name in &theirs {
        assert!(
            is_upstream(name),
            "the crate {name} of whatsapp-rust is not in UPSTREAM_MODULES"
        );
    }
    // The HTTP, TLS and WebSocket crates under them (lead's decision L7) are named, whatever else is.
    for name in [
        "ureq",
        "ureq_proto",
        "rustls",
        "tokio_rustls",
        "tokio_websockets",
        "webpki",
    ] {
        assert!(
            UPSTREAM_MODULES.contains(&name),
            "{name} must be in UPSTREAM_MODULES"
        );
    }
    // ... and a name in the list is a crate of the build: a crate that left does not stay in it.
    for name in UPSTREAM_MODULES {
        assert!(
            libs.iter().any(|(n, _)| n == name),
            "{name} is in UPSTREAM_MODULES but no crate of the build has that library name"
        );
    }
}

/// Review F3: where the two pin-bump guards begin and end.
#[test]
fn a_byte_list_of_eight_is_hidden_and_a_shorter_list_or_a_range_is_not() {
    let hide = |text: &str| redact_upstream(text);
    assert_eq!(hide("k [1, 2, 3, 4, 5, 6, 7, 8] end"), "k […] end");
    assert_eq!(hide("k [1,2,3,4,5,6,7,8,9,10,11] end"), "k […] end");
    assert_eq!(hide("k [1, 2, 3, 4, 5, 6, 7, 8, ..] end"), "k […] end");
    assert_eq!(hide("k [0x05, 0xa1, 3, 4, 5, 6, 7, 8] end"), "k […] end");
    // Not closed, or multi-line as pretty Debug prints it.
    assert_eq!(hide("k [1, 2, 3, 4, 5, 6, 7, 8,"), "k […]");
    assert_eq!(
        hide("k [\n  1,\n  2,\n  3,\n  4,\n  5,\n  6,\n  7,\n  8,\n]"),
        "k […]"
    );
    // Seven are a list of something else; so are values that are no bytes, and ranges.
    assert_eq!(
        hide("k [1, 2, 3, 4, 5, 6, 7] end"),
        "k [1, 2, 3, 4, 5, 6, 7] end"
    );
    assert_eq!(
        hide("k [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000] end"),
        "k [1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000] end"
    );
    assert_eq!(
        hide("prekey window [12, 40) used"),
        "prekey window [12, 40) used"
    );
    // Stable.
    assert_eq!(hide("k […] end"), "k […] end");
}

#[test]
fn what_follows_a_word_that_says_name_is_hidden_unless_it_is_quoted_or_absent() {
    let hide = |text: &str| redact_upstream(text);
    assert_eq!(hide("push name Ann Lee rejected"), "push name …");
    assert_eq!(hide("push_name: Ann Lee"), "push_name: …");
    assert_eq!(hide("username=ann.lee failed"), "username=…");
    assert_eq!(hide("Names Ann and Lee"), "Names …");
    // Quoted (the quote rule has it), at the end, an at sign or a longer word are not this rule's.
    assert_eq!(hide("name \"Ann Lee\" left"), "name \"…\" left");
    assert_eq!(hide("push name"), "push name");
    assert_eq!(hide("name@ now"), "name@ now");
    assert_eq!(hide("nameless thing"), "nameless thing");
    assert_eq!(hide("rename failed"), "rename failed");
    assert_eq!(hide("hostname is up"), "hostname is up");
    // Stable.
    assert_eq!(hide("push name …"), "push name …");
}
