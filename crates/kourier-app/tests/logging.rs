//! W2b B1: the app's log and the lines of whatsapp-rust (architecture §9.1; `src/logging/upstream.rs`);
//! TH4 chunk L: and the lines of libthreema.
//!
//! `logging::init` installs a GLOBAL subscriber and the `log` bridge, so every case runs in a child
//! process: this binary again, with `--exact logging::the_child_logs` and the directory of the log in
//! its environment (without it the child "test" returns at once, and under `cargo test`, where tests
//! are threads of one process, nothing global is touched by the parent). The child emits `log::Record`s
//! built the way upstream's `log` macros build them (a module path of its own crate, free-word targets
//! such as "Client/AppState"), and the parent reads `kourier.log` and the child's stderr. libthreema's
//! child (`logging::libthreema::the_child_logs`) emits through callsites built by hand, see its module.
//!
//! Each network's cases sit under its own feature.
#![cfg(any(feature = "whatsapp", feature = "threema"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use kourier_app::logging;

/// The directory the child writes its log to; set by the parent, absent in a normal run.
const CHILD_DIR: &str = "KOURIER_LOGGING_TEST_DIR";

/// Emits one `log` record the way a `log::warn!(target: ..)` call of a crate does.
#[cfg(feature = "whatsapp")]
fn emit(module_path: Option<&'static str>, target: &str, level: log::Level, message: &str) {
    log::logger().log(
        &log::Record::builder()
            .args(format_args!("{message}"))
            .level(level)
            .target(target)
            .module_path_static(module_path)
            .file_static(Some("lib.rs"))
            .line(Some(7))
            .build(),
    );
}

#[cfg(feature = "whatsapp")]
const LEVELS: [(log::Level, &str); 5] = [
    (log::Level::Trace, "trace"),
    (log::Level::Debug, "debug"),
    (log::Level::Info, "info"),
    (log::Level::Warn, "warn"),
    (log::Level::Error, "error"),
];

/// What the child does: not a test of its own (it returns at once unless the parent started it).
#[cfg(feature = "whatsapp")]
#[test]
fn the_child_logs() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    assert!(logging::init(Some(Path::new(&dir))), "a fresh process");

    // libsignal first: the shared `log` callsites must not have been decided by what follows.
    emit(
        Some("libsignal_net::chat"),
        "libsignal_net::chat",
        log::Level::Info,
        "ls-info-before",
    );
    // Upstream, at every level, under the module paths of three of its crates and the free-word
    // targets its macros use. Each message holds what must never be written: a phone number, a JID,
    // the pair code and a stanza with a token.
    for (module, target) in [
        ("whatsapp_rust::client::lid_pn", "Client/AppState"),
        ("whatsapp_rust::pair_code", "Bot/PairCode"),
        ("wacore::net::noise", "wacore::net::noise"),
        ("ureq::run", "ureq::run"),
    ] {
        for (level, name) in LEVELS {
            emit(
                Some(module),
                target,
                level,
                &format!(
                    "up-{name}-{}: Pair code generated: WXYZABCD for phone: 4915123456789 jid 4915123456789:12@s.whatsapp.net <failure reason=\"401\"><locked appeal_token=\"tok-QWERTY\"/></failure>",
                    module.split("::").next().unwrap_or("")
                ),
            );
        }
    }
    // A record with no module path counts as upstream's.
    for (level, name) in LEVELS {
        emit(
            None,
            "Client/Unknown",
            level,
            &format!("nomod-{name} 4915123456789"),
        );
    }
    // Everyone else is written as the filters say: libsignal's INFO and WARN, redacted by the
    // writer's own rule (a whole UUID keeps four digits); DEBUG and TRACE only if `KOURIER_LOG` asks.
    for (level, name) in LEVELS {
        emit(
            Some("libsignal_net::chat"),
            "libsignal_net::chat",
            level,
            &format!("ls-{name} peer 9d0652a3-dcc3-4d11-975f-74d61598733f.2"),
        );
    }
    emit(
        Some("libsignal_net::chat"),
        "libsignal_net::chat",
        log::Level::Info,
        "ls-info-after",
    );
    tracing::info!("native-info");
    tracing::debug!("native-debug");
}

/// What a child wrote: the log file and stderr.
struct Run {
    file: String,
    stderr: String,
}

/// Runs the child with `KOURIER_LOG` set to `filter` (unset: the default).
#[cfg(feature = "whatsapp")]
fn run_child(filter: Option<&str>) -> Run {
    run_test_child("logging::the_child_logs", filter)
}

/// Runs the test `test` of this binary as the child (`logging::<module>::<fn>`) with `KOURIER_LOG` set to
/// `filter` (unset: the default).
fn run_test_child(test: &str, filter: Option<&str>) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.args(["--exact", test, "--nocapture"])
        .env(CHILD_DIR, dir.path())
        .env_remove("KOURIER_LOG")
        .env_remove("RUST_LOG");
    if let Some(filter) = filter {
        cmd.env("KOURIER_LOG", filter);
    }
    let out = cmd.output().expect("run the child");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "the child failed:\n{stderr}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!("test {test} ... ok")),
        "the child did not run its test:\n{}\n{stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    let file = std::fs::read_to_string(dir.path().join("kourier.log")).expect("the log file");
    Run { file, stderr }
}

/// The lines of `text` that hold `marker`.
fn lines<'a>(text: &'a str, marker: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.contains(marker)).collect()
}

/// What a line says after its time, level and target (the time has six digits of microseconds).
#[cfg(feature = "whatsapp")]
fn message_of(line: &str) -> &str {
    line.split_once(": ").map_or(line, |(_, message)| message)
}

/// A run of `n` or more ASCII digits.
#[cfg(feature = "whatsapp")]
fn has_digit_run(line: &str, n: usize) -> bool {
    let mut run = 0;
    for c in line.chars() {
        run = if c.is_ascii_digit() { run + 1 } else { 0 };
        if run >= n {
            return true;
        }
    }
    false
}

/// What the rule says, for one text (the file and stderr must agree).
#[cfg(feature = "whatsapp")]
fn check(text: &str, widened: bool, what: &str) {
    // Upstream: nothing below WARN, whatever KOURIER_LOG says; WARN and ERROR once per case, redacted.
    for module in ["whatsapp_rust", "wacore", "ureq"] {
        for name in ["trace", "debug", "info"] {
            let marker = format!("up-{name}-{module}:");
            assert!(
                lines(text, &marker).is_empty(),
                "{what}: an upstream {name} line of {module} was written:\n{text}"
            );
        }
        for name in ["warn", "error"] {
            let marker = format!("up-{name}-{module}:");
            let found = lines(text, &marker);
            let expected = if module == "whatsapp_rust" { 2 } else { 1 };
            assert_eq!(found.len(), expected, "{what}: {marker}\n{text}");
            for line in found {
                assert!(
                    line.contains(&name.to_uppercase()),
                    "{what}: the level is in the line: {line}"
                );
                assert!(
                    !has_digit_run(message_of(line), 6),
                    "{what}: a number is in the line: {line}"
                );
                for secret in ["WXYZABCD", "tok-QWERTY", "4915123456789", "401"] {
                    assert!(
                        !message_of(line).contains(secret),
                        "{what}: {secret} is in the line: {line}"
                    );
                }
                assert!(line.contains('…'), "{what}: redacted: {line}");
            }
        }
    }
    // The free-word targets are kept as the line's target.
    assert!(
        text.contains(" WARN Client/AppState: up-warn-whatsapp_rust:"),
        "{what}: {text}"
    );
    assert!(
        text.contains(" ERROR Bot/PairCode: up-error-whatsapp_rust:"),
        "{what}: {text}"
    );
    // No module path: upstream's rule.
    for name in ["trace", "debug", "info"] {
        assert!(
            lines(text, &format!("nomod-{name}")).is_empty(),
            "{what}: a record without a module path was written at {name}:\n{text}"
        );
    }
    for name in ["warn", "error"] {
        let found = lines(text, &format!("nomod-{name}"));
        assert_eq!(found.len(), 1, "{what}: nomod-{name}\n{text}");
        assert!(
            !has_digit_run(message_of(found[0]), 6),
            "{what}: {}",
            found[0]
        );
    }
    // Everyone else: libsignal's INFO and WARN are there, before and after upstream's records, and the
    // writer's own redaction still holds; DEBUG and TRACE only when the user's filter asks for them.
    for marker in [
        "ls-info-before",
        "ls-info-after",
        "ls-info ",
        "ls-warn ",
        "ls-error ",
    ] {
        assert_eq!(lines(text, marker).len(), 1, "{what}: {marker}\n{text}");
    }
    let ls_warn = lines(text, "ls-warn ")[0];
    assert!(
        ls_warn.ends_with("peer …733f.2"),
        "{what}: the UUID is cut to its last four digits: {ls_warn}"
    );
    for marker in ["ls-debug ", "ls-trace "] {
        assert_eq!(
            lines(text, marker).len(),
            usize::from(widened),
            "{what}: {marker}\n{text}"
        );
    }
    assert_eq!(lines(text, "native-info").len(), 1, "{what}");
    assert_eq!(
        lines(text, "native-debug").len(),
        usize::from(widened),
        "{what}"
    );
}

/// Upstream's lines below WARN are never written and the rest is redacted, in the file and on
/// stderr; libsignal's records are not silenced by the callsites they share with upstream's.
#[cfg(feature = "whatsapp")]
#[test]
fn upstream_lines_are_cut_to_warn_and_redacted() {
    let run = run_child(None);
    check(&run.file, false, "the log file");
    check(&run.stderr, false, "stderr");
}

/// The user's filter cannot widen the rule: `KOURIER_LOG=trace` writes everything of the others and
/// still nothing of upstream's below WARN.
#[cfg(feature = "whatsapp")]
#[test]
fn a_filter_that_asks_for_everything_does_not_widen_upstream() {
    let run = run_child(Some("trace"));
    check(&run.file, true, "the log file");
    check(&run.stderr, true, "stderr");
}

/// TH4 chunk L (decision C11, H2): libthreema's records. They come through `tracing` natively, with
/// INFO spans that carry fields, and a `tracing` macro in THIS file would carry this file's module path
/// (`it::logging`), which no rule treats as libthreema's. So the child emits through callsites built by
/// hand, the way the macros build theirs, with `module_path: Some("libthreema::d2d_rendezvous")`: a span at
/// INFO with a field, an event at TRACE, DEBUG and INFO each with a marker field and a marker message, and
/// a WARN and an ERROR event with a message and a field. The parent holds the file and stderr to the rule:
/// nothing below WARN under the default filter and under `KOURIER_LOG=trace`, no span field, and of the WARN
/// and the ERROR the message alone, as it is. A control record of a crate whose name only starts like
/// libthreema's is emitted INSIDE the span and written whole, but without the span's context: a span that
/// the rule let through would be printed in front of it (an upstream line prints no span, so only a record
/// of another crate can show one). No hook in the production code: the callsites are the test's.
#[cfg(feature = "threema")]
mod libthreema {
    use tracing::callsite::{Callsite as _, DefaultCallsite, Identifier};
    use tracing::field::{FieldSet, Value};
    use tracing::level_filters::LevelFilter;
    use tracing::metadata::Kind;
    use tracing::{Event, Level, Metadata, Span};

    use super::*;

    /// The module path (and target) of the hand-built callsites: libthreema's.
    const MODULE: &str = "libthreema::d2d_rendezvous";
    /// A crate whose name only starts like libthreema's: not upstream.
    const OTHER_MODULE: &str = "libthreemax::y";
    /// What the WARN says; the quotes and the digit run are what WhatsApp's redaction would hide, so a line
    /// that ends with it as written was not given to it.
    const WARN_MESSAGE: &str = "th-warn-message \"quoted\" 4096000";
    const ERROR_MESSAGE: &str = "th-error-message";
    const CONTROL_MESSAGE: &str = "th-control-message";
    /// The messages of the events below WARN: they must never be written.
    const TRACE_MESSAGE: &str = "th-trace-message";
    const DEBUG_MESSAGE: &str = "th-debug-message";
    const INFO_MESSAGE: &str = "th-info-message";

    /// A static callsite and its metadata, built by hand (`tracing::callsite2!` is not public).
    macro_rules! callsite {
        ($meta:ident, $site:ident, $module:expr, $kind:expr, $level:expr, $fields:expr) => {
            static $meta: Metadata<'static> = Metadata::new(
                "hand-built",
                $module,
                $level,
                Some("lib/src/d2d_rendezvous/mod.rs"),
                Some(1018),
                Some($module),
                FieldSet::new($fields, Identifier(&$site)),
                $kind,
            );
            static $site: DefaultCallsite = DefaultCallsite::new(&$meta);
        };
    }

    callsite!(
        SPAN_META,
        SPAN_SITE,
        MODULE,
        Kind::SPAN,
        Level::INFO,
        &["th_span_field"]
    );
    callsite!(
        TRACE_META,
        TRACE_SITE,
        MODULE,
        Kind::EVENT,
        Level::TRACE,
        &["message", "th_trace_field"]
    );
    callsite!(
        DEBUG_META,
        DEBUG_SITE,
        MODULE,
        Kind::EVENT,
        Level::DEBUG,
        &["message", "th_debug_field"]
    );
    callsite!(
        INFO_META,
        INFO_SITE,
        MODULE,
        Kind::EVENT,
        Level::INFO,
        &["message", "th_info_field"]
    );
    callsite!(
        WARN_META,
        WARN_SITE,
        MODULE,
        Kind::EVENT,
        Level::WARN,
        &["message", "th_warn_field"]
    );
    callsite!(
        ERROR_META,
        ERROR_SITE,
        MODULE,
        Kind::EVENT,
        Level::ERROR,
        &["message", "th_error_field"]
    );
    callsite!(
        CONTROL_META,
        CONTROL_SITE,
        OTHER_MODULE,
        Kind::EVENT,
        Level::INFO,
        &["message", "th_control_field"]
    );

    /// What `tracing`'s macros ask before they build a span or an event: the level against the
    /// subscriber's maximum, the callsite's interest (the filters' answer when it registered) and, unless
    /// that is `always`, the subscriber itself.
    fn enabled(site: &'static DefaultCallsite) -> bool {
        let meta = site.metadata();
        if *meta.level() > LevelFilter::current() {
            return false;
        }
        let interest = site.interest();
        !interest.is_never()
            && (interest.is_always()
                || tracing::dispatcher::get_default(|subscriber| subscriber.enabled(meta)))
    }

    /// A span of `site` with one field, entered by the caller; `Span::none()` when the filters say no, as
    /// the `span!` macro does.
    fn span(site: &'static DefaultCallsite, field: &str, value: &str) -> Span {
        if !enabled(site) {
            return Span::none();
        }
        let meta = site.metadata();
        let field = meta.fields().field(field).expect("the span's field");
        Span::new(
            meta,
            &meta
                .fields()
                .value_set(&[(&field, Some(&value as &dyn Value))]),
        )
    }

    /// An event of `site` with a field and, when the callsite has the field `message`, a message (as the
    /// `warn!` macro writes it: `format_args!`).
    fn event(site: &'static DefaultCallsite, field: &str, value: &str, message: Option<&str>) {
        if !enabled(site) {
            return;
        }
        let meta = site.metadata();
        let fields = meta.fields();
        let field = fields.field(field).expect("the event's field");
        match message {
            Some(text) => {
                let message = fields.field("message").expect("the event's message");
                Event::dispatch(
                    meta,
                    &fields.value_set(&[
                        (&message, Some(&format_args!("{text}") as &dyn Value)),
                        (&field, Some(&value as &dyn Value)),
                    ]),
                );
            }
            None => Event::dispatch(
                meta,
                &fields.value_set(&[(&field, Some(&value as &dyn Value))]),
            ),
        }
    }

    /// What the child does: not a test of its own (it returns at once unless the parent started it).
    #[test]
    fn the_child_logs() {
        let Some(dir) = std::env::var_os(CHILD_DIR) else {
            return;
        };
        assert!(logging::init(Some(Path::new(&dir))), "a fresh process");

        // libthreema's records, the events inside its INFO span as its `instrument`ed functions have them.
        {
            let span = span(&SPAN_SITE, "th_span_field", "th-span-value");
            let _inside = span.enter();
            event(
                &TRACE_SITE,
                "th_trace_field",
                "th-trace-value",
                Some(TRACE_MESSAGE),
            );
            event(
                &DEBUG_SITE,
                "th_debug_field",
                "th-debug-value",
                Some(DEBUG_MESSAGE),
            );
            event(
                &INFO_SITE,
                "th_info_field",
                "th-info-value",
                Some(INFO_MESSAGE),
            );
            event(
                &WARN_SITE,
                "th_warn_field",
                "th-warn-value",
                Some(WARN_MESSAGE),
            );
            event(
                &ERROR_SITE,
                "th_error_field",
                "th-error-value",
                Some(ERROR_MESSAGE),
            );
            // Not libthreema's, though inside its span: written as the filters say, fields and all, and
            // with the span's context only if the span exists.
            event(
                &CONTROL_SITE,
                "th_control_field",
                "th-control-value",
                Some(CONTROL_MESSAGE),
            );
        }
    }

    /// What the rule says, for one text (the file and stderr must agree).
    fn check(text: &str, what: &str) {
        // Nothing below WARN, whatever the filter says, and no span: not a message, not a field name,
        // not a field value, not the span's own name (the control line, inside the span, would carry it).
        for gone in [
            "th_span_field",
            "th-span-value",
            "hand-built",
            "th_trace_field",
            "th-trace-value",
            TRACE_MESSAGE,
            "th_debug_field",
            "th-debug-value",
            DEBUG_MESSAGE,
            "th_info_field",
            "th-info-value",
            INFO_MESSAGE,
        ] {
            assert!(!text.contains(gone), "{what}: {gone} was written:\n{text}");
        }
        // The WARN and the ERROR: once each, as time, level, target and the message alone, as written
        // (WhatsApp's redaction is not applied), with no field.
        for (level, message) in [("WARN", WARN_MESSAGE), ("ERROR", ERROR_MESSAGE)] {
            let marker = message.split(' ').next().expect("a marker");
            let found = lines(text, marker);
            assert_eq!(found.len(), 1, "{what}: {marker}\n{text}");
            let tail = format!(" {level} {MODULE}: {message}");
            assert!(
                found[0].ends_with(&tail),
                "{what}: the line is not time, {level}, target and the message alone: {}",
                found[0]
            );
        }
        for gone in [
            "th_warn_field",
            "th-warn-value",
            "th_error_field",
            "th-error-value",
        ] {
            assert!(
                !text.contains(gone),
                "{what}: the field {gone} was written:\n{text}"
            );
        }
        assert_eq!(
            lines(text, MODULE).len(),
            2,
            "{what}: libthreema wrote more than its WARN and its ERROR:\n{text}"
        );
        // The control is written whole: its message and its field.
        let control = lines(text, CONTROL_MESSAGE);
        assert_eq!(control.len(), 1, "{what}: {CONTROL_MESSAGE}\n{text}");
        assert!(
            control[0].contains(&format!(" INFO {OTHER_MODULE}: {CONTROL_MESSAGE} "))
                && control[0].contains("th_control_field=\"th-control-value\""),
            "{what}: {}",
            control[0]
        );
    }

    /// libthreema's lines below WARN are never written, no span field is, and of a WARN or an ERROR
    /// only the message, in the file and on stderr.
    #[test]
    fn lines_are_cut_to_warn_and_carry_their_message_only() {
        let run = run_test_child("logging::libthreema::the_child_logs", None);
        check(&run.file, "the log file");
        check(&run.stderr, "stderr");
    }

    /// The user's filter cannot widen the rule: `KOURIER_LOG=trace` writes everything of the others and
    /// still nothing of libthreema's below WARN, and no span.
    #[test]
    fn a_filter_that_asks_for_everything_does_not_widen_libthreema() {
        let run = run_test_child("logging::libthreema::the_child_logs", Some("trace"));
        check(&run.file, "the log file");
        check(&run.stderr, "stderr");
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `BackendConfig::log_filter` is a field with no reader: the app's filters are `KOURIER_LOG` and the
/// rule above, so it cannot widen anything. A reader that is added must go through the rule; this
/// test says so when it finds one.
#[test]
fn the_backend_configs_log_filter_has_no_reader() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates").flatten() {
        rust_files(&entry.path().join("src"), &mut files);
    }
    assert!(files.len() > 100, "found the sources: {}", files.len());
    let readers: Vec<_> = files
        .iter()
        .filter(|p| !p.ends_with("kourier-backend-api/src/context.rs"))
        .filter(|p| {
            std::fs::read_to_string(p)
                .expect("source")
                .lines()
                .any(|l| l.contains("log_filter") && !l.trim_start().starts_with("//"))
        })
        .collect();
    assert!(
        readers.is_empty(),
        "BackendConfig::log_filter is read in {readers:?}: whatever reads it must keep upstream's records to \
         kourier_whatsapp::logs::keeps (logging/upstream.rs), and this test must say how"
    );
}
