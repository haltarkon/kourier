//! The walk of `connect` (X13, X14, X15 and the driver): the confirmation, the app's command, the
//! file, and the dev-control channel driven over a program that plays the app (a shell script that
//! prints events, answers requests and logs the lines the real app logs).

use std::ffi::OsString;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use super::report::{counts_line, dialer_line, good_stop, lockdown_line};
use super::{POLL, Scratch, StepClock, args, error_of, executable, mode_of, profile_in};
use crate::tg_run::connect::{
    Bounds, NonDumpable, Outcome, Run, acquire_lock, app_command, data_home_of, describe_launch,
    dial_file_text, fresh_nonce, make_non_dumpable, order_confirmed, remove_dial_file, walk_app,
    write_dial_file,
};
use crate::tg_run::preflight::{Cleared, sha256_bytes};
use crate::tg_run::profile::{ProfileDir, State, wipe_in};
use crate::tg_run::report::{Confirmation, ExitHow, RunKind, verdict};
use crate::tg_run::stage::Clock;
use crate::tg_run::{KEEPER_DIR, REHEARSAL_DIR};

/// The nonce a test's random source gives: 32 bytes of 0xab.
fn the_nonce() -> String {
    "ab".repeat(32)
}

fn witness() -> NonDumpable {
    make_non_dumpable().expect("non-dumpable")
}

// ---- X13 ----------------------------------------------------------------------------------------

/// X13: the order is confirmed only with `--confirmed-by-user`. Mutant: `confirmed` always true.
#[test]
fn an_order_is_confirmed_only_with_the_flag() {
    let with = args(&["connect", "--confirmed-by-user", "--env", "test"]).expect("parses");
    assert!(order_confirmed(&with));
    let without = args(&["connect", "--env", "test"]).expect("parses");
    assert!(!order_confirmed(&without));
    let place = format!("/t/{REHEARSAL_DIR}/1/x/{KEEPER_DIR}/p");
    let rehearsal =
        args(&["connect", "--env", "test", "--rehearsal-place", &place]).expect("parses");
    assert!(
        !order_confirmed(&rehearsal),
        "a rehearsal without the flag is not confirmed"
    );
    let both = args(&[
        "connect",
        "--confirmed-by-user",
        "--env",
        "test",
        "--rehearsal-place",
        &place,
    ])
    .expect("parses");
    assert!(order_confirmed(&both));
}

// ---- X14 ----------------------------------------------------------------------------------------

fn env_of(cmd: &std::process::Command, key: &str) -> Option<Option<OsString>> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.map(OsString::from))
}

fn value(cmd: &std::process::Command, key: &str) -> Option<String> {
    env_of(cmd, key)
        .flatten()
        .map(|v| v.to_string_lossy().into_owned())
}

/// X14: the app's command. The program is the staged copy itself; every inherited `KOURIER_*` variable
/// is removed; then exactly the profile, the test servers, the sentinel tail, no QR link, no dump, no
/// desktop services, the log filter, offscreen and silent; the dial variable ONLY for a confirmed order
/// and the rehearsal switch ONLY in a rehearsal; the description names variables, never values or
/// paths. Mutant: the dial variable set unconditionally.
#[test]
fn the_app_command_is_direct_cleared_offscreen_and_confirmed_only_when_confirmed() {
    let bin = Path::new("/staged/tg-run/bin/kourier");
    let home = Path::new("/data/kourier-tg-run/p");
    let data = Path::new("/data");
    let inherited: Vec<OsString> = [
        "KOURIER_DEV_TELEGRAM_DIAL",
        "KOURIER_DEV_REHEARSAL",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_QR_URL",
        "KOURIER_SIGNAL_ENV",
        "PATH",
        "HOME",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let nonce = the_nonce();
    let real_unconfirmed = app_command(bin, home, data, None, false, &inherited);
    let real_confirmed = app_command(bin, home, data, Some(&nonce), false, &inherited);
    let rehearsal_confirmed = app_command(bin, home, data, Some(&nonce), true, &inherited);

    for cmd in [&real_unconfirmed, &real_confirmed, &rehearsal_confirmed] {
        // Direct: the staged program itself, never kcap, env, timeout, a shell or systemd-run.
        assert_eq!(cmd.get_program(), bin.as_os_str());
        assert_eq!(
            cmd.get_args()
                .next()
                .map(|a| a.to_string_lossy().into_owned()),
            Some("--dev-control".to_owned())
        );
        // Everything inherited that starts with KOURIER_ is removed or overridden; others are left.
        assert_eq!(env_of(cmd, "KOURIER_FAILPOINT"), Some(None));
        assert_eq!(env_of(cmd, "KOURIER_SIGNAL_ENV"), Some(None));
        assert_eq!(env_of(cmd, "PATH"), None);
        assert_eq!(env_of(cmd, "HOME"), None);
        // What is set.
        assert_eq!(
            value(cmd, "KOURIER_HOME").as_deref(),
            Some("/data/kourier-tg-run/p")
        );
        assert_eq!(value(cmd, "XDG_DATA_HOME").as_deref(), Some("/data"));
        assert_eq!(
            value(cmd, "KOURIER_DEV_TELEGRAM_ENV").as_deref(),
            Some("test")
        );
        assert_eq!(
            value(cmd, "KOURIER_DEV_EXPECT_NUMBER_TAIL").as_deref(),
            Some("000000000000000"),
            "the sentinel tail: no number ends in 15 zeros"
        );
        assert_eq!(value(cmd, "KOURIER_DEV_QR_URL").as_deref(), Some("0"));
        assert_eq!(value(cmd, "KOURIER_DEV_NO_DUMP").as_deref(), Some("1"));
        assert_eq!(value(cmd, "KOURIER_DESKTOP").as_deref(), Some("0"));
        assert_eq!(
            value(cmd, "KOURIER_DEV_KEYRING").as_deref(),
            Some("passphrase")
        );
        assert_eq!(
            value(cmd, "KOURIER_LOG").as_deref(),
            Some("info,kourier_telegram=info,kourier_app=info")
        );
        assert_eq!(value(cmd, "QT_QPA_PLATFORM").as_deref(), Some("offscreen"));
        assert!(value(cmd, "PULSE_SERVER").is_some_and(|v| v.contains("/nonexistent/")));
        assert_eq!(env_of(cmd, "PULSE_RUNTIME_PATH"), Some(None));
        assert!(value(cmd, "PIPEWIRE_REMOTE").is_some());
    }
    // The dial variable: the nonce for a confirmed order, NOTHING for an unconfirmed one (and an
    // inherited one is removed).
    assert_eq!(
        value(&real_confirmed, "KOURIER_DEV_TELEGRAM_DIAL"),
        Some(nonce.clone())
    );
    assert_eq!(
        value(&rehearsal_confirmed, "KOURIER_DEV_TELEGRAM_DIAL"),
        Some(nonce.clone())
    );
    assert_eq!(
        env_of(&real_unconfirmed, "KOURIER_DEV_TELEGRAM_DIAL"),
        Some(None),
        "an unconfirmed order hands the app no nonce, and an inherited one is removed"
    );
    // The fake's switch: only in a rehearsal.
    assert_eq!(
        value(&rehearsal_confirmed, "KOURIER_DEV_REHEARSAL").as_deref(),
        Some("telegram")
    );
    assert_eq!(env_of(&real_confirmed, "KOURIER_DEV_REHEARSAL"), Some(None));
    assert_eq!(
        env_of(&real_unconfirmed, "KOURIER_DEV_REHEARSAL"),
        Some(None)
    );
    // The set KOURIER_* names of an unconfirmed real order are exactly these.
    let mut set: Vec<String> = real_unconfirmed
        .get_envs()
        .filter(|(k, v)| k.to_string_lossy().starts_with("KOURIER_") && v.is_some())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    set.sort();
    assert_eq!(
        set,
        [
            "KOURIER_DESKTOP",
            "KOURIER_DEV_EXPECT_NUMBER_TAIL",
            "KOURIER_DEV_KEYRING",
            "KOURIER_DEV_NO_DUMP",
            "KOURIER_DEV_QR_URL",
            "KOURIER_DEV_TELEGRAM_ENV",
            "KOURIER_HOME",
            "KOURIER_LOG",
        ]
    );
    // The description names the variables and no value, no path.
    let text = describe_launch(&real_confirmed);
    assert!(text.contains("KOURIER_DEV_TELEGRAM_DIAL"), "{text}");
    assert!(text.starts_with("kourier --dev-control"), "{text}");
    for secret in [
        nonce.as_str(),
        "/data",
        "/staged",
        "passphrase",
        "000000000000000",
    ] {
        assert!(!text.contains(secret), "{secret} in: {text}");
    }
    let plain = describe_launch(&real_unconfirmed);
    assert!(!plain.contains("KOURIER_DEV_TELEGRAM_DIAL"), "{plain}");
}

// ---- X15 ----------------------------------------------------------------------------------------

/// X15 (a): the file holds the nonce's HASH and never the nonce; exactly the fields the app reads.
/// The known vector pins the hash. Mutant: the raw nonce written.
#[test]
fn the_dial_file_holds_the_hash_of_the_nonce_and_never_the_nonce() {
    // SHA-256 of "abc" (FIPS 180-2's first test vector).
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let text = dial_file_text(1234, "abc").expect("text");
    let v: Value = serde_json::from_str(&text).expect("json");
    let keys: Vec<&str> = v
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["env", "issued_ms", "nonce_sha256", "version"]);
    assert_eq!(v["version"], 1);
    assert_eq!(v["issued_ms"], 1234);
    assert_eq!(v["env"], "test");
    assert_eq!(v["nonce_sha256"], abc);
    assert!(
        !text.contains("\"abc\"") && !text.contains(": \"abc"),
        "{text}"
    );
    let nonce = the_nonce();
    let text = dial_file_text(1, &nonce).expect("text");
    assert!(!text.contains(&nonce), "the raw nonce is in the file");
    assert!(text.contains(&sha256_bytes(nonce.as_bytes()).expect("sum")));
    assert_eq!(text.matches("\"nonce_sha256\"").count(), 1);

    // Written 0600 in the tool's directory through a temporary file and a rename.
    let scratch = Scratch::new("dialfile");
    let dir = profile_in(&scratch);
    write_dial_file(&dir, 99, &nonce).expect("written");
    assert_eq!(mode_of(&dir.dial_file()), 0o600);
    let on_disk = std::fs::read_to_string(dir.dial_file()).expect("file");
    assert!(!on_disk.contains(&nonce) && on_disk.contains("\"issued_ms\": 99"));
    let names: Vec<String> = std::fs::read_dir(dir.tool())
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
    // Written again: replaced, not appended.
    write_dial_file(&dir, 100, &nonce).expect("written");
    assert_eq!(
        std::fs::read_to_string(dir.dial_file())
            .expect("file")
            .matches("issued_ms")
            .count(),
        1
    );
    remove_dial_file(&dir);
    assert!(!dir.dial_file().exists());
    remove_dial_file(&dir);
}

/// The nonce is 32 bytes of the random source as 64 lower-case hex digits; a short source is an error;
/// the real source gives different nonces.
#[test]
fn the_nonce_is_thirty_two_random_bytes() {
    let w = witness();
    let mut source = Cursor::new(vec![0xabu8; 32]);
    assert_eq!(fresh_nonce(w, &mut source).expect("nonce"), the_nonce());
    let mut short = Cursor::new(vec![0xabu8; 31]);
    assert!(error_of(fresh_nonce(w, &mut short)).contains("random source"));
    let mut real = std::fs::File::open("/dev/urandom").expect("urandom");
    let a = fresh_nonce(w, &mut real).expect("nonce");
    let b = fresh_nonce(w, &mut real).expect("nonce");
    assert_ne!(a, b);
    for n in [&a, &b] {
        assert!(
            n.len() == 64
                && n.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
    }
    // The tool made itself non-dumpable before any of this.
    // SAFETY: `PR_GET_DUMPABLE` only reads this process's flag.
    assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }, 0);
}

// ---- The driver over a program that plays the app ---------------------------------------------------

/// The dev control's events as it writes them (`kourier-qt/src/dev/mod.rs`): a login step with its step
/// id and its kind, the `login_qr` of a code that was shown (the link is switched off), the same event
/// with the link, an error.
fn step_event(step: &str, kind: &str) -> String {
    format!(
        "{{\"type\":\"event\",\"event\":\"login_step\",\"data\":{{\"account\":1,\"step\":\"{step}\",\"kind\":\"{kind}\"}}}}"
    )
}

fn qr_shown_event() -> String {
    "{\"type\":\"event\",\"event\":\"login_qr\",\"data\":{\"account\":1,\"shown\":true}}".to_owned()
}

/// A `login_qr` that carries a link: a synthetic marker, never a real link.
const LINK_MARKER: &str = "https://example.invalid/qr-marker-9d3c1f";

fn qr_link_event() -> String {
    format!(
        "{{\"type\":\"event\",\"event\":\"login_qr\",\"data\":{{\"account\":1,\"url\":\"{LINK_MARKER}\"}}}}"
    )
}

fn error_event(source: &str) -> String {
    format!(
        "{{\"type\":\"event\",\"event\":\"error\",\"data\":{{\"source\":\"{source}\",\"account\":1,\"message\":\"a message the tool never reads\"}}}}"
    )
}

/// One line of the playing program's script that prints `line`.
fn echo(line: &str) -> String {
    format!("echo '{line}'\n")
}

/// What the playing program does.
struct Fake {
    /// The dialer line it logs (mode, reason), if any.
    dialer: Option<(&'static str, &'static str)>,
    /// Whether it shows the QR step, and any steps after it.
    qr: bool,
    after_qr: &'static [&'static str],
    /// The accounts its `state` names, and whether the page change is answered ok.
    accounts: usize,
    page_ok: bool,
    /// Never answers and never ends (a hung app).
    deaf: bool,
    /// Removes the confirmation file at its start, as the app does.
    consume: bool,
    /// Writes a confirmation file of its own at its start (an app that must not).
    writes_file: bool,
    /// Lines it logs to stderr as they are.
    stderr: Vec<String>,
    /// Event lines it prints as they are, after the QR code and the steps after it.
    events: Vec<String>,
}

impl Fake {
    fn good(dialer: (&'static str, &'static str)) -> Self {
        Self {
            dialer: Some(dialer),
            qr: true,
            after_qr: &[],
            accounts: 0,
            page_ok: true,
            deaf: false,
            consume: true,
            writes_file: false,
            stderr: Vec::new(),
            events: Vec::new(),
        }
    }

    fn script(&self) -> String {
        let mut s = String::from("#!/bin/sh\nf=\"$KOURIER_HOME/tg-run/dial-go.json\"\n");
        s.push_str("if [ -e \"$f\" ]; then echo \"fake: dial file present mode $(stat -c %a \"$f\")\" >&2; else echo \"fake: dial file absent\" >&2; fi\n");
        s.push_str("echo \"fake: dial variable ${KOURIER_DEV_TELEGRAM_DIAL:+set}\" >&2\n");
        s.push_str("echo \"fake: rehearsal switch ${KOURIER_DEV_REHEARSAL:-none}\" >&2\n");
        if self.consume {
            s.push_str("rm -f \"$f\"\n");
        }
        if self.writes_file {
            s.push_str("echo '{}' > \"$f\"\n");
        }
        for line in &self.stderr {
            s.push_str(&format!("echo '{line}' >&2\n"));
        }
        if let Some((mode, reason)) = self.dialer {
            s.push_str(&format!("echo '{}' >&2\n", dialer_line(mode, reason)));
        }
        if self.qr {
            s.push_str(&echo(&step_event("qr", "qr")));
            s.push_str(&echo(&qr_shown_event()));
        }
        for step in self.after_qr {
            s.push_str(&echo(&step_event(step, step)));
        }
        for event in &self.events {
            s.push_str(&echo(event));
        }
        if self.deaf {
            s.push_str("exec sleep 600\n");
            return s;
        }
        let accounts = if self.accounts == 0 {
            "[]".to_owned()
        } else {
            format!("[{}]", vec!["\\\"active\\\""; self.accounts].join(","))
        };
        s.push_str(
            r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"cmd":"quit"'*) echo "{\"type\":\"response\",\"id\":$id,\"ok\":true,\"result\":{}}"; exit 0;;
    *'"cmd":"state"'*) echo "{\"type\":\"response\",\"id\":$id,\"ok\":true,\"result\":{\"accounts\":ACCOUNTS}}";;
    *) echo "{\"type\":\"response\",\"id\":$id,\"ok\":PAGEOK,\"result\":{}}";;
  esac
done
"#
            .replace("ACCOUNTS", &accounts)
            .replace("PAGEOK", if self.page_ok { "true" } else { "false" })
            .as_str(),
        );
        s
    }
}

/// A scratch profile and the machinery of a walk.
struct Harness {
    scratch: Scratch,
    dir: ProfileDir,
    tools: PathBuf,
    clock: StepClock,
}

fn bounds(first_qr: u64, answer: u64, exit: u64) -> Bounds {
    Bounds {
        first_qr: Duration::from_secs(first_qr),
        answer: Duration::from_secs(answer),
        exit: Duration::from_secs(exit),
        drain: Duration::from_secs(10),
        poll: POLL,
    }
}

impl Harness {
    fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let dir = profile_in(&scratch);
        let tools = scratch.0.join("tools");
        std::fs::create_dir_all(&tools).expect("tools");
        Self {
            scratch,
            dir,
            tools,
            clock: StepClock::new(POLL),
        }
    }

    fn program(&self, name: &str, fake: &Fake) -> PathBuf {
        executable(&self.tools, name, &fake.script())
    }

    fn walk(&self, program: &Path, confirmed: bool, kind: RunKind, b: &Bounds) -> Result<Outcome> {
        let now = || 1_700_000_000_000u64;
        let random = || -> Result<Box<dyn Read>> { Ok(Box::new(Cursor::new(vec![0xabu8; 32]))) };
        let data = self.scratch.0.join("data");
        let run = Run {
            dir: &self.dir,
            kind,
            confirmed,
            program,
            home: &self.dir.dir,
            data_home: &data,
            inherited: &[],
            bounds: b,
            clock: &self.clock,
            now_ms: &now,
            random: &random,
        };
        walk_app(&run, witness())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.app_log()).unwrap_or_default()
    }
}

/// A confirmed order: the file and the nonce reach the app (the file 0600, written atomically), the walk
/// is the one a connect should make, the dialer line equals the order, and nothing of the confirmation
/// is left.
#[test]
fn a_confirmed_walk_hands_over_the_file_and_the_nonce_and_leaves_neither() {
    let h = Harness::new("walk-confirmed");
    let app = h.program("app", &Fake::good(("telegram", "confirmed")));
    let out = h
        .walk(&app, true, RunKind::Rehearsal, &bounds(30, 20, 20))
        .expect("walked");
    assert_eq!(out.walk.words(), ["qr"]);
    assert!(out.walk.qr_after_ms.is_some() && out.walk.steps[0].shown);
    assert_eq!(out.walk.confirmation, Some(Confirmation::TakenByApp));
    assert_eq!(
        out.walk.raw_needles,
        Some(0),
        "a rehearsal searches the raw stderr"
    );
    assert_eq!(out.walk.closed, Some(true));
    assert_eq!(out.walk.accounts, Some(0));
    assert_eq!(out.walk.exit, ExitHow::Quit(0));
    assert!(
        out.walk
            .memory_kb
            .is_some_and(|(rss, peak)| rss > 0 && peak >= rss),
        "the app's memory was not read: {:?}",
        out.walk.memory_kb
    );
    assert_eq!(
        out.log.dialer,
        [("telegram".to_owned(), "confirmed".to_owned())]
    );
    let log = h.log();
    assert!(log.contains("fake: dial file present mode 600"), "{log}");
    assert!(log.contains("fake: dial variable set"), "{log}");
    assert!(log.contains("fake: rehearsal switch telegram"), "{log}");
    assert!(!log.contains(&the_nonce()), "the nonce is in the app log");
    assert!(!h.dir.dial_file().exists());
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(v.pass, "{:?}", v.failures);
}

/// X15 (b): an order that is not confirmed gets no file and no variable; the app (here: the playing
/// program) says it refuses, and the verdict compares that with the order. Mutant: the file written for
/// an unconfirmed order.
#[test]
fn an_unconfirmed_walk_writes_no_file_and_hands_over_no_nonce() {
    let h = Harness::new("walk-unconfirmed");
    let app = h.program("app", &Fake::good(("refusing", "no-confirmation")));
    let out = h
        .walk(&app, false, RunKind::Rehearsal, &bounds(30, 20, 20))
        .expect("walked");
    let log = h.log();
    assert!(log.contains("fake: dial file absent"), "{log}");
    assert!(
        log.contains("fake: dial variable \n") || log.contains("fake: dial variable \r"),
        "{log}"
    );
    assert!(!log.contains("dial variable set"), "{log}");
    assert!(!h.dir.dial_file().exists());
    assert!(
        !std::fs::read_dir(h.dir.tool())
            .expect("dir")
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("dial-go")),
        "nothing of a confirmation was written"
    );
    assert_eq!(
        out.log.dialer,
        [("refusing".to_owned(), "no-confirmation".to_owned())]
    );
    assert!(verdict(&out.walk, &out.log, false, RunKind::Rehearsal).pass);
    // The same program against a CONFIRMED order is a FAIL of the mode compare: the app did not dial
    // where the order said to.
    assert!(!verdict(&out.walk, &out.log, true, RunKind::Rehearsal).pass);
}

/// X15 (c): a leftover file is removed at the start (the unconfirmed order below writes none of its
/// own), after a start that failed, and at the end (the playing program leaves the file alone).
/// Mutants: each removal skipped.
#[test]
fn a_leftover_confirmation_is_removed_at_the_start_after_a_failed_start_and_at_the_end() {
    // At the start: a stale file from an earlier run, an unconfirmed order.
    let h = Harness::new("walk-leftover");
    std::fs::write(h.dir.dial_file(), "{\"stale\":true}").expect("a leftover");
    let app = h.program("app", &Fake::good(("refusing", "no-confirmation")));
    h.walk(&app, false, RunKind::Rehearsal, &bounds(30, 20, 20))
        .expect("walked");
    let log = h.log();
    assert!(
        log.contains("fake: dial file absent"),
        "the stale file reached the app: {log}"
    );
    assert!(!h.dir.dial_file().exists());

    // After a failed start: the file was written for the order, the program cannot be started.
    let h = Harness::new("walk-failed-start");
    let missing = h.tools.join("no-such-program");
    let err = error_of(
        h.walk(&missing, true, RunKind::Rehearsal, &bounds(30, 20, 20))
            .map(|_| ()),
    );
    assert!(err.contains("start the app"), "{err}");
    assert!(
        !h.dir.dial_file().exists(),
        "a confirmation outlived a failed start"
    );

    // At the end: the program does not take the file (the app always does), the tool removes it.
    let h = Harness::new("walk-end");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.consume = false;
    let app = h.program("app", &fake);
    h.walk(&app, true, RunKind::Rehearsal, &bounds(30, 20, 20))
        .expect("walked");
    assert!(h.log().contains("fake: dial file present mode 600"));
    assert!(
        !h.dir.dial_file().exists(),
        "a confirmation outlived the walk"
    );
}

/// A walk that finds no QR step within its bound still asks the app to quit and judges FAIL.
#[test]
fn a_walk_without_a_qr_step_fails_and_still_quits_the_app() {
    let h = Harness::new("walk-no-qr");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.qr = false;
    let app = h.program("app", &fake);
    let out = h
        .walk(&app, true, RunKind::Rehearsal, &bounds(2, 20, 20))
        .expect("walked");
    assert!(out.walk.steps.is_empty() && out.walk.qr_after_ms.is_none());
    assert_eq!((out.walk.closed, out.walk.accounts), (None, None));
    assert_eq!(out.walk.exit, ExitHow::Quit(0), "the app was asked to quit");
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        !v.pass
            && v.failures
                .iter()
                .any(|f| f.contains("no QR code was shown")),
        "{:?}",
        v.failures
    );
}

/// A step after the QR step, an account, a refused page change: each is seen and fails the verdict.
#[test]
fn a_login_that_went_on_or_made_an_account_is_seen() {
    let b = bounds(30, 20, 20);
    let h = Harness::new("walk-step-after");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.after_qr = &["password"];
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.words(), ["qr", "password"]);
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        v.failures.iter().any(|f| f.contains("after the QR code")),
        "{:?}",
        v.failures
    );

    let h = Harness::new("walk-account");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.accounts = 1;
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.accounts, Some(1));
    assert!(!verdict(&out.walk, &out.log, true, RunKind::Rehearsal).pass);

    let h = Harness::new("walk-page-refused");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.page_ok = false;
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.closed, Some(false));
    assert!(!verdict(&out.walk, &out.log, true, RunKind::Rehearsal).pass);
}

/// H-1 (the review's F1, a BLOCKER): a login that FAILS at the step `qr` (a flood wait on the QR
/// request, an error of the library) is no QR code. The playing app sends `login_step {qr, failed}` and
/// an `error` event, no `login_qr`, then answers the page change, `state` and `quit` cleanly; its log has
/// the dialer line, the lock-down word and a clean final counts line. The verdict FAILS with the new
/// sentence and `first QR step: never`, and the walk does not wait out the QR bound for a code that
/// cannot come. Mutants: the QR read from the step word alone; the failed check dropped; the error
/// count dropped.
#[test]
fn a_login_that_failed_at_the_qr_step_fails_the_walk() {
    let h = Harness::new("walk-failed-at-qr");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.qr = false;
    fake.events = vec![step_event("qr", "failed"), error_event("login")];
    fake.stderr = vec![lockdown_line("on"), good_stop()];
    let out = h
        .walk(
            &h.program("app", &fake),
            true,
            RunKind::Real,
            &bounds(30, 20, 20),
        )
        .expect("walked");
    assert_eq!(out.walk.words(), ["qr"]);
    assert_eq!(out.walk.steps[0].kind, "failed");
    assert!(out.walk.qr_after_ms.is_none());
    assert_eq!(out.walk.errors, 1);
    assert_eq!(
        (out.walk.closed, out.walk.accounts, out.walk.exit),
        (Some(true), Some(0), ExitHow::Quit(0)),
        "the page change, `state` and `quit` were still asked"
    );
    assert!(
        h.clock.now_ms() < 30_000,
        "the walk waited out the QR bound for a code that could not come"
    );
    let v = verdict(&out.walk, &out.log, true, RunKind::Real);
    assert!(!v.pass);
    for why in [
        "the login failed at qr before the QR code",
        "no QR code was shown",
        "the app reported 1 login or account errors",
    ] {
        assert!(
            v.failures.iter().any(|f| f.contains(why)),
            "{why}: {:?}",
            v.failures
        );
    }
    let text = report_text(&out, true, RunKind::Real, &v);
    assert!(text.contains("connect: FAIL"), "{text}");
    assert!(text.contains("first QR step: never"), "{text}");
}

/// H-1: a refreshed code is a new link, a new `qr` step and a new `login_qr`: it is allowed after the
/// first; any other step is not, and a `qr` step the app never followed with `login_qr` is no code.
#[test]
fn a_refreshed_qr_code_passes_and_another_step_after_it_does_not() {
    let b = bounds(30, 20, 20);
    let h = Harness::new("walk-refresh");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.events = vec![step_event("qr", "qr"), qr_shown_event()];
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.words(), ["qr", "qr"]);
    assert!(out.walk.steps.iter().all(|s| s.shown));
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(v.pass, "{:?}", v.failures);

    for (what, events) in [
        (
            "a phone step after the code",
            vec![step_event("phone", "input")],
        ),
        ("a code that was never shown", vec![step_event("qr", "qr")]),
        (
            "a refreshed code of another kind",
            vec![step_event("qr", "input"), qr_shown_event()],
        ),
    ] {
        let h = Harness::new("walk-refresh-no");
        let mut fake = Fake::good(("telegram", "confirmed"));
        fake.events = events;
        let out = h
            .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
            .expect("walked");
        let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
        assert!(
            !v.pass && v.failures.iter().any(|f| f.contains("after the QR code")),
            "{what}: {:?}",
            v.failures
        );
    }
}

/// R-1: the walk keeps 64 login steps; a step past them is counted, never judged, so ANY drop fails the
/// walk. The playing app shows one code and 63 refreshed codes (64 steps, allowed: the walk passes), and
/// then a `password` step: the 65th, which would pass unseen if the verdict looked only at the kept steps.
/// Mutant: the drop check removed from the verdict.
#[test]
fn a_walk_that_dropped_login_steps_fails_because_not_every_step_was_judged() {
    let b = bounds(30, 20, 20);
    let refreshes = |n: usize| -> Vec<String> {
        (0..n)
            .flat_map(|_| [step_event("qr", "qr"), qr_shown_event()])
            .collect()
    };

    // 64 steps in all: all kept, none dropped, a pass.
    let h = Harness::new("walk-64-steps");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.events = refreshes(63);
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.steps.len(), 64);
    assert_eq!(out.walk.steps_dropped, 0);
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(v.pass, "{:?}", v.failures);

    // The 65th step is a `password` step after the code: dropped, and the walk FAILS with the sentence.
    let h = Harness::new("walk-65-steps");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.events = refreshes(63);
    fake.events.push(step_event("password", "input"));
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.steps.len(), 64);
    assert_eq!(out.walk.steps_dropped, 1);
    assert!(
        out.walk.steps.iter().all(|s| s.word == "qr"),
        "the step past the cap was kept"
    );
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(!v.pass);
    assert_eq!(
        v.failures,
        ["more than 64 login steps: not every step was judged"],
        "the dropped step is the only reason"
    );
    let text = report_text(&out, true, RunKind::Rehearsal, &v);
    assert!(text.contains("connect: FAIL"), "{text}");
    assert!(
        text.contains("FAILED: more than 64 login steps: not every step was judged"),
        "{text}"
    );
}

/// Every file below `dir` that holds `needle`.
fn files_holding(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_holding(&path, needle));
        } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(needle)) {
            found.push(path);
        }
    }
    found
}

/// The report's text for a walk.
fn report_text(
    out: &Outcome,
    confirmed: bool,
    kind: RunKind,
    v: &crate::tg_run::report::Verdict,
) -> String {
    crate::tg_run::report::lines(&crate::tg_run::report::Report {
        profile: "p",
        env: "test",
        build: None,
        confirmed,
        kind,
        walk: &out.walk,
        log: &out.log,
        verdict: v,
    })
    .join("\n")
}

/// H-1: the tool starts the app with the QR link switched off, so a `login_qr` that carries one is a
/// leak of a login token: the walk FAILS, and the link (a synthetic marker here) is in no line, no
/// report and no file the tool wrote. Mutant: the check dropped.
#[test]
fn a_qr_link_on_the_apps_output_fails_the_walk_and_is_kept_nowhere() {
    let h = Harness::new("walk-link");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.qr = false;
    fake.events = vec![step_event("qr", "qr"), qr_link_event()];
    let out = h
        .walk(
            &h.program("app", &fake),
            true,
            RunKind::Rehearsal,
            &bounds(2, 20, 20),
        )
        .expect("walked");
    assert!(out.walk.qr_link_printed);
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        v.failures.iter().any(|f| f.contains("printed the QR link")),
        "{:?}",
        v.failures
    );
    let text = report_text(&out, true, RunKind::Rehearsal, &v);
    assert!(text.contains("QR link in the app's output: yes"), "{text}");
    assert!(
        !text.contains(LINK_MARKER) && !text.contains("qr-marker"),
        "{text}"
    );
    assert!(
        !format!("{:?}", out.walk).contains("marker"),
        "the walk keeps the link"
    );
    // No file below the profile (the app's log, the stamp, the dial file) holds it either.
    assert!(
        files_holding(&h.dir.dir, "qr-marker").is_empty(),
        "{:?}",
        files_holding(&h.dir.dir, "qr-marker")
    );
}

/// H-1: an `error` event of a login or an account is counted by its source; the message is never read
/// and an action's error is not one of them. Mutant: the count dropped.
#[test]
fn the_errors_of_a_login_or_an_account_are_counted_by_their_source() {
    let h = Harness::new("walk-errors");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.events = vec![
        error_event("login"),
        error_event("account"),
        error_event("action"),
        error_event("something else"),
    ];
    let out = h
        .walk(
            &h.program("app", &fake),
            true,
            RunKind::Rehearsal,
            &bounds(30, 20, 20),
        )
        .expect("walked");
    assert_eq!(out.walk.errors, 2);
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        v.failures
            .iter()
            .any(|f| f.contains("the app reported 2 login or account errors")),
        "{:?}",
        v.failures
    );
}

/// H-5 (the review's F5): the walk looks at the confirmation file BEFORE its own removal. An app that
/// does not take the file of a confirmed order FAILS the walk ("the app did not take its confirmation");
/// an app that writes a file for an order that was not confirmed FAILS; the tool still removes the file
/// afterwards. Mutant: the look moved after `remove_dial_file`.
#[test]
fn the_walk_sees_who_took_the_confirmation_before_it_removes_the_file() {
    let b = bounds(30, 20, 20);
    let h = Harness::new("walk-not-taken");
    let mut fake = Fake::good(("telegram", "confirmed"));
    fake.consume = false;
    let out = h
        .walk(&h.program("app", &fake), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.confirmation, Some(Confirmation::Left));
    assert!(
        !h.dir.dial_file().exists(),
        "the tool removed it afterwards"
    );
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        !v.pass
            && v.failures
                .iter()
                .any(|f| f.contains("the app did not take its confirmation")),
        "{:?}",
        v.failures
    );
    let text = report_text(&out, true, RunKind::Rehearsal, &v);
    assert!(
        text.contains("confirmation: left (removed by the tool)"),
        "{text}"
    );

    let h = Harness::new("walk-written");
    let mut fake = Fake::good(("refusing", "no-confirmation"));
    fake.writes_file = true;
    fake.consume = false;
    let out = h
        .walk(&h.program("app", &fake), false, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.walk.confirmation, Some(Confirmation::Left));
    assert!(!h.dir.dial_file().exists());
    let v = verdict(&out.walk, &out.log, false, RunKind::Rehearsal);
    assert!(
        v.failures
            .iter()
            .any(|f| f.contains("stood although the order was not confirmed")),
        "{:?}",
        v.failures
    );

    // The good walks: taken by the app / none written.
    let h = Harness::new("walk-taken");
    let out = h
        .walk(
            &h.program("app", &Fake::good(("telegram", "confirmed"))),
            true,
            RunKind::Rehearsal,
            &b,
        )
        .expect("walked");
    assert_eq!(out.walk.confirmation, Some(Confirmation::TakenByApp));
    let out = h
        .walk(
            &h.program("app2", &Fake::good(("refusing", "no-confirmation"))),
            false,
            RunKind::Rehearsal,
            &b,
        )
        .expect("walked");
    assert_eq!(out.walk.confirmation, Some(Confirmation::NoneWritten));
}

/// H-9 (the review's F9): in a rehearsal the stderr reader searches each RAW line, before the
/// redaction, for the walk's needles: the nonce, its hash, the sentinel tail and the fake's constants.
/// The redaction would turn a printed nonce into `[hex]`, so a search that came after it would see
/// nothing. A real run keeps no needle list. Mutant: the search after `redact`.
#[test]
fn a_rehearsal_counts_the_walks_needles_in_the_apps_raw_stderr() {
    use crate::tg_run::rehearse::FAKE_CONSTANTS;
    use crate::tg_run::stage::SENTINEL_TAIL;
    let b = bounds(30, 20, 20);
    let walk_with = |name: &str, stderr: Vec<String>, confirmed: bool, kind: RunKind| {
        let h = Harness::new(name);
        let mut fake = Fake::good(if confirmed {
            ("telegram", "confirmed")
        } else {
            ("refusing", "no-confirmation")
        });
        fake.stderr = stderr;
        let out = h
            .walk(&h.program("app", &fake), confirmed, kind, &b)
            .expect("walked");
        (out, h.log())
    };
    // A clean app: searched, nothing found.
    let (out, _) = walk_with(
        "raw-clean",
        vec!["nothing to see".to_owned()],
        true,
        RunKind::Rehearsal,
    );
    assert_eq!(out.walk.raw_needles, Some(0));
    // The nonce itself: found raw, and the redacted log holds `[hex]` instead.
    let (out, log) = walk_with(
        "raw-nonce",
        vec![format!("leaked {}", the_nonce())],
        true,
        RunKind::Rehearsal,
    );
    assert_eq!(out.walk.raw_needles, Some(1), "the nonce in a raw line");
    assert!(
        log.contains("leaked [hex]") && !log.contains(&the_nonce()),
        "{log}"
    );
    // Its hash, a sentinel tail, a constant of the fake: found too.
    let hash = sha256_bytes(the_nonce().as_bytes()).expect("hash");
    for (what, line) in [
        ("the hash of the nonce", format!("leaked {hash}")),
        ("the sentinel tail", format!("tail {SENTINEL_TAIL}")),
        (
            "a constant of the fake",
            format!("name {}", FAKE_CONSTANTS[0].1),
        ),
    ] {
        let (out, _) = walk_with("raw-other", vec![line], true, RunKind::Rehearsal);
        assert_eq!(out.walk.raw_needles, Some(1), "{what}");
    }
    // An unconfirmed walk has no nonce to look for, but the constants still count.
    let (out, _) = walk_with(
        "raw-unconfirmed",
        vec![
            format!("name {}", FAKE_CONSTANTS[1].1),
            format!("leaked {}", the_nonce()),
        ],
        false,
        RunKind::Rehearsal,
    );
    assert_eq!(
        out.walk.raw_needles,
        Some(1),
        "the nonce of another walk is no needle here"
    );
    // A real run keeps no list: nothing is searched, nothing is counted.
    let (out, _) = walk_with(
        "raw-real",
        vec![format!("leaked {}", the_nonce())],
        true,
        RunKind::Real,
    );
    assert_eq!(out.walk.raw_needles, None);
}

/// Q2: the real walk's bounds. The first QR code has 180 s (the rehearsal's confirmed walk took 57 s at a
/// server load of 174 with the fake alone; the real walk adds TDLib's connect on the PC at low CPU
/// weight): a timeout fails safe but spends the user's go. The app's end has 40 s. Mutant: 120 s back.
#[test]
fn the_real_walk_waits_180_seconds_for_the_first_qr_code() {
    assert_eq!(Bounds::REAL.first_qr, Duration::from_secs(180));
    assert_eq!(Bounds::REAL.exit, Duration::from_secs(40));
    assert_eq!(Bounds::REAL.answer, Duration::from_secs(20));
    assert_eq!(Bounds::REAL.drain, Duration::from_secs(5));
}

/// An app that ends by itself before any QR step: seen, FAIL. An app that never answers and never ends:
/// its waits run out on the clock, it is killed, FAIL.
#[test]
fn an_app_that_ends_early_or_hangs_is_bounded_and_fails() {
    let h = Harness::new("walk-early");
    let early = executable(
        &h.tools,
        "early",
        "#!/bin/sh\necho 'fake: gone' >&2\nexit 0\n",
    );
    let out = h
        .walk(&early, false, RunKind::Rehearsal, &bounds(30, 20, 20))
        .expect("walked");
    assert!(out.walk.steps.is_empty());
    assert_eq!(out.walk.exit, ExitHow::Quit(0));
    assert!(!verdict(&out.walk, &out.log, false, RunKind::Rehearsal).pass);

    let h = Harness::new("walk-hung");
    let mut deaf = Fake::good(("telegram", "confirmed"));
    deaf.deaf = true;
    let app = h.program("deaf", &deaf);
    // The answers and the app's end have 1 s of silence each (ten idle polls of 100 ms): the page change,
    // `state`, `quit` and the wait for the end run out one after the other, 4 s of real time in all.
    let out = h
        .walk(&app, true, RunKind::Rehearsal, &bounds(30, 1, 1))
        .expect("walked");
    assert_eq!(out.walk.words(), ["qr"]);
    assert_eq!((out.walk.closed, out.walk.accounts), (None, None));
    assert_eq!(out.walk.exit, ExitHow::Killed);
    let v = verdict(&out.walk, &out.log, true, RunKind::Rehearsal);
    assert!(
        v.failures.iter().any(|f| f.contains("killed")),
        "{:?}",
        v.failures
    );
}

/// The app's stderr goes into `app.log` through the redaction; only the lines after the latest start
/// mark are judged (an earlier walk's lines are not).
#[test]
fn the_app_log_is_redacted_and_only_the_last_walk_is_judged() {
    let h = Harness::new("walk-log");
    let b = bounds(30, 20, 20);
    let secrets = [
        ["tg:", &"7".repeat(10)].concat(),
        "+380501234567".to_owned(),
        "9f1c2b3a-1111-2222-3333-444455556666".to_owned(),
    ];
    let mut first = Fake::good(("telegram", "confirmed"));
    first.stderr = vec![
        format!("secrets {} {} {}", secrets[0], secrets[1], secrets[2]),
        good_stop(),
        lockdown_line("on"),
    ];
    let out = h
        .walk(&h.program("first", &first), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(
        out.log.counts.len(),
        1,
        "the first walk's own lines are read"
    );
    assert_eq!(out.log.lockdown, ["on"]);
    let log = h.log();
    assert!(log.contains("secrets tg:[digits] +[number] …6666"), "{log}");
    for secret in &secrets {
        assert!(!log.contains(secret.as_str()), "{secret} in the app log");
    }
    assert_eq!(mode_of(&h.dir.app_log()), 0o600);
    // A second walk of the same profile: the first one's counts and lock-down lines are not its own.
    let mut second = Fake::good(("telegram", "confirmed"));
    second.stderr = vec![counts_line("running", &[("dials", 1)])];
    let out = h
        .walk(&h.program("second", &second), true, RunKind::Rehearsal, &b)
        .expect("walked");
    assert_eq!(out.log.counts.len(), 1);
    assert_eq!(out.log.counts[0].when, "running");
    assert!(out.log.lockdown.is_empty());
    assert!(h.log().matches("==== tg-run: app started").count() == 2);
}

// ---- The lock, the report and the state --------------------------------------------------------------

/// One connect per profile: the lock is exclusive, and `wipe` refuses while it is held.
#[test]
fn a_profile_runs_one_connect_at_a_time_and_wipe_waits_for_none() {
    let scratch = Scratch::new("lock");
    let dir = profile_in(&scratch);
    State::new("test").save(&dir).expect("state");
    let first = acquire_lock(&dir).expect("the first lock");
    let err = error_of(acquire_lock(&dir).map(|_| ()));
    assert!(err.contains("another connect runs"), "{err}");
    let err = error_of(wipe_in(&dir, true, Duration::ZERO).map(|_| ()));
    assert!(err.contains("a connect is running"), "{err}");
    assert!(dir.dir.exists());
    drop(first);
    acquire_lock(&dir).expect("the lock is free again");
    // The keeper root's grandparent is the data home the app derives its run profile from.
    assert_eq!(
        data_home_of(Path::new("/data/kourier-tg-run/r0")).expect("data"),
        Path::new("/data")
    );
    assert!(data_home_of(Path::new("/")).is_err());
}

/// The report: written 0600 beside the profile's other files, the same words as the printed ones; the
/// state keeps the verdict and nothing else of the run.
#[test]
fn the_report_and_the_state_keep_words_and_the_verdict_only() {
    let scratch = Scratch::new("finish");
    let dir = profile_in(&scratch);
    let mut cleared = Cleared {
        dir: dir.clone(),
        state: State::new("test"),
        warnings: Vec::new(),
        encrypted: Some(true),
        head: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_owned()),
        ranges_verified: Some(true),
        library: None,
    };
    let walk = super::report::good_walk();
    let log = crate::tg_run::report::read_log(
        &[
            dialer_line("telegram", "confirmed"),
            lockdown_line("on"),
            good_stop(),
        ]
        .join("\n"),
    );
    let outcome = Outcome { walk, log };
    let v = verdict(&outcome.walk, &outcome.log, true, RunKind::Real);
    assert!(v.pass);
    crate::tg_run::connect::finish(&mut cleared, RunKind::Real, true, &outcome, &v).expect("done");
    let report = std::fs::read_to_string(dir.report_file()).expect("report");
    assert_eq!(mode_of(&dir.report_file()), 0o600);
    assert!(report.starts_with("connect: PASS"), "{report}");
    for fact in [
        "profile p (env test)",
        "build a1b2c3d4e5",
        "dialer: mode=telegram reason=confirmed",
        "lock-down: on",
        "gate counts (stop): accepted=3",
        "first QR step: after 4210 ms; steps after it: 0; accounts after the close: 0",
        "findings: none",
        "confirmation: taken by the app",
        "sends: not measured",
    ] {
        assert!(report.contains(fact), "{fact} missing: {report}");
    }
    // Words and numbers: no path, no hash of anything.
    let scratch_path = scratch.0.to_string_lossy().into_owned();
    assert!(!report.contains(&scratch_path), "{report}");
    assert!(
        !report.contains("a1b2c3d4e5f60718"),
        "only the short commit: {report}"
    );
    let state = State::load(&dir).expect("state").expect("there");
    assert_eq!(state.connect_verdicts(), ["PASS"]);
    let text = std::fs::read_to_string(dir.state_file()).expect("state file");
    assert!(!text.contains(&scratch_path), "{text}");
    // A second connect adds one entry.
    crate::tg_run::connect::finish(&mut cleared, RunKind::Real, true, &outcome, &v).expect("done");
    assert_eq!(
        State::load(&dir)
            .expect("state")
            .expect("there")
            .connect_verdicts(),
        ["PASS", "PASS"]
    );
}
