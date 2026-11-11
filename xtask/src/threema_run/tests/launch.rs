//! The launch of `start` (T3a): the stamped copy, the app's environment (pinned whole, for the real
//! order and the rehearsal's), the probe, the control socket's place (a 200-byte checkout), the
//! launch order, the profile lock, the switch and its guard (critic M3: every exit after the write
//! puts it back) and the whole `start_in` over the preflight's canned world with a stand-in
//! supervisor. Nothing starts the real app, nothing contacts a host, no window, no sound. No mutant.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::preflight::{HEAD, World, current_uid, start};
use super::{Scratch, args, digest_shape, error_of, mode_of};
use crate::threema_run::launch::{
    APP_LOG_FILTER, INFO_ID_WAITING, Order, QUESTION_KIND, Seams, SwitchGuard, UpTiming, Wait,
    WaitClock, app_command, check_copy, copy_checked, describe_launch, info_at, judge_probe,
    listen_at, lock_is_free, open_question, order_json, probe, probe_command, rehearsal_socket_dir,
    reuse_copy, sha256_bytes, socket_for_client, socket_name, socket_path, stamp_copy, start_in,
    switch_is_on, switch_off, take_launch, take_profile_lock,
};
use crate::threema_run::preflight::{SOCKET_NAME_LEN, SOCKET_PATH_MAX};
use crate::threema_run::profile::{ProfileDir, State, private_dir, read_json};
use crate::threema_run::{ALLOW_FILE, KEEPER_DIR, OPEN_QUESTION_FILE};

const NOW: u64 = 1_700_000_000_000;

fn fake_dir(rehearsal: bool) -> ProfileDir {
    ProfileDir {
        dir: PathBuf::from("/fake/data").join(KEEPER_DIR).join("p"),
        rehearsal,
    }
}

fn order(rehearsal: bool) -> Order {
    Order {
        socket: PathBuf::from("/fake/run/kthr-0123456789abcdef.sock"),
        rehearsal,
        for_link: false,
    }
}

/// What `get_envs` says: `Some(value)` set, `None` taken out.
fn envs_of(cmd: &Command) -> BTreeMap<String, Option<String>> {
    cmd.get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

fn set(pairs: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
        .collect()
}

fn inherited() -> Vec<OsString> {
    [
        "KOURIER_DEV_REHEARSAL",
        "KOURIER_SIGNAL_ENV",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_EXPECT_NUMBER_TAIL",
        "KOURIER_DEV_TELEGRAM_DIAL",
        "PATH",
        "HOME",
        "XDG_RUNTIME_DIR",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

const NO_AUDIO: &str = "unix:/nonexistent/kourier-threema-run-no-audio";

// ---- The app's environment ----------------------------------------------------------------------------

/// Critic R-H1 and tr.md §2.7: the WHOLE environment of a real start is this list. The three the
/// brief's list lacked (`KOURIER_DEV_NO_DUMP`, `KOURIER_DESKTOP`, `QT_FORCE_STDERR_LOGGING`) are in
/// it; the app is the stamped copy itself with `--dev-control` and nothing else; every inherited
/// `KOURIER_*` is out (the fake network, a failpoint, another network's gate), and no data home is
/// set (the keeper root was derived from the one the user's environment has).
#[test]
fn a_real_start_gets_exactly_this_environment() {
    let dir = fake_dir(false);
    let copy = dir.copy_file();
    let cmd = app_command(&copy, &dir, &order(false), &inherited());
    // Started directly, never through a wrapper that would hold the environment of its own.
    assert_eq!(Path::new(cmd.get_program()), copy.as_path());
    for wrapper in ["kcap", "systemd-run", "env", "timeout", "sh", "bash"] {
        assert_ne!(
            Path::new(cmd.get_program())
                .file_name()
                .and_then(|n| n.to_str()),
            Some(wrapper)
        );
    }
    assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--dev-control"]);
    assert_eq!(
        APP_LOG_FILTER,
        "info,kourier_threema=info,kourier_core=info,kourier_app=info"
    );
    let mut want = set(&[
        ("KOURIER_HOME", "/fake/data/kourier-threema-run/p"),
        ("KOURIER_DEV_KEYRING", "passphrase"),
        ("KOURIER_DEV_NO_DUMP", "1"),
        ("KOURIER_DEV_QR_URL", "0"),
        ("KOURIER_DESKTOP", "0"),
        ("KOURIER_THREEMA_RUN", "1"),
        (
            "KOURIER_LOG",
            "info,kourier_threema=info,kourier_core=info,kourier_app=info",
        ),
        ("QT_FORCE_STDERR_LOGGING", "1"),
        ("PULSE_SERVER", NO_AUDIO),
        ("PIPEWIRE_REMOTE", "kourier-threema-run-no-audio"),
    ]);
    for gone in [
        "KOURIER_DEV_REHEARSAL",
        "KOURIER_SIGNAL_ENV",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_EXPECT_NUMBER_TAIL",
        "KOURIER_DEV_TELEGRAM_DIAL",
        "PULSE_RUNTIME_PATH",
    ] {
        want.insert(gone.to_owned(), None);
    }
    assert_eq!(envs_of(&cmd), want, "the whole list");
    // The user's own variables are left alone: a real run has a window on his session.
    let env = envs_of(&cmd);
    for name in [
        "PATH",
        "HOME",
        "XDG_RUNTIME_DIR",
        "XDG_DATA_HOME",
        "QT_QPA_PLATFORM",
    ] {
        assert!(!env.contains_key(name), "{name}");
    }
}

/// The rehearsal: the same list, plus the fake's switch, the data home the keeper root is derived
/// from, offscreen and silent.
#[test]
fn the_rehearsals_environment_adds_the_fake_offscreen_and_silence() {
    let dir = fake_dir(true);
    let cmd = app_command(&dir.copy_file(), &dir, &order(true), &inherited());
    assert_eq!(
        cmd.get_args().collect::<Vec<_>>(),
        ["--dev-control", "--size=1200x800"]
    );
    let mut want = set(&[
        ("KOURIER_HOME", "/fake/data/kourier-threema-run/p"),
        ("KOURIER_DEV_KEYRING", "passphrase"),
        ("KOURIER_DEV_NO_DUMP", "1"),
        ("KOURIER_DEV_QR_URL", "0"),
        ("KOURIER_DESKTOP", "0"),
        ("KOURIER_THREEMA_RUN", "1"),
        (
            "KOURIER_LOG",
            "info,kourier_threema=info,kourier_core=info,kourier_app=info",
        ),
        ("QT_FORCE_STDERR_LOGGING", "1"),
        ("PULSE_SERVER", NO_AUDIO),
        ("PIPEWIRE_REMOTE", "kourier-threema-run-no-audio"),
        ("XDG_DATA_HOME", "/fake/data"),
        ("KOURIER_DEV_REHEARSAL", "threema"),
        ("QT_QPA_PLATFORM", "offscreen"),
        ("QT_QUICK_BACKEND", "software"),
        ("QT_SCALE_FACTOR", "1"),
        ("LANG", "en_US.UTF-8"),
        ("LANGUAGE", "en"),
    ]);
    for gone in [
        "KOURIER_SIGNAL_ENV",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_EXPECT_NUMBER_TAIL",
        "KOURIER_DEV_TELEGRAM_DIAL",
        "PULSE_RUNTIME_PATH",
    ] {
        want.insert(gone.to_owned(), None);
    }
    assert_eq!(envs_of(&cmd), want, "the whole list");
}

/// What the person reads names the program's file and the NAMES of the variables, never a value or
/// a path.
#[test]
fn what_the_start_says_of_the_app_names_variables_and_no_values_or_paths() {
    let dir = fake_dir(false);
    let cmd = app_command(&dir.copy_file(), &dir, &order(false), &inherited());
    let words = describe_launch(&cmd);
    assert!(
        words.starts_with("kourier --dev-control (variables set by the tool: "),
        "{words}"
    );
    for name in [
        "KOURIER_DEV_NO_DUMP",
        "KOURIER_DESKTOP",
        "QT_FORCE_STDERR_LOGGING",
    ] {
        assert!(words.contains(name), "{words}");
    }
    for value in ["passphrase", "/fake", "nonexistent"] {
        assert!(!words.contains(value), "{words}");
    }
}

// ---- The probe ----------------------------------------------------------------------------------------

/// The probe is the stamped app over `--print-networks` with the profile and no other `KOURIER_*`;
/// every inherited one is out; only a rehearsal carries the fake's switch; it is offscreen and
/// silent in a real run too.
#[test]
fn the_probe_carries_the_profile_the_marker_and_no_other_kourier_variable() {
    for rehearsal in [false, true] {
        let dir = fake_dir(rehearsal);
        let cmd = probe_command(&dir.copy_file(), &dir, rehearsal, &inherited());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--print-networks"]);
        let env = envs_of(&cmd);
        let mut kourier: Vec<&str> = env
            .iter()
            .filter(|(k, v)| k.starts_with("KOURIER_") && v.is_some())
            .map(|(k, _)| k.as_str())
            .collect();
        kourier.sort_unstable();
        assert_eq!(
            kourier,
            if rehearsal {
                vec![
                    "KOURIER_DEV_REHEARSAL",
                    "KOURIER_HOME",
                    "KOURIER_THREEMA_RUN",
                ]
            } else {
                vec!["KOURIER_HOME", "KOURIER_THREEMA_RUN"]
            }
        );
        for gone in [
            "KOURIER_FAILPOINT",
            "KOURIER_DEV_EXPECT_NUMBER_TAIL",
            "KOURIER_DEV_TELEGRAM_DIAL",
            "KOURIER_SIGNAL_ENV",
            "PULSE_RUNTIME_PATH",
        ] {
            assert_eq!(env.get(gone), Some(&None), "{gone}");
        }
        assert_eq!(env["QT_QPA_PLATFORM"].as_deref(), Some("offscreen"));
        assert_eq!(env["PULSE_SERVER"].as_deref(), Some(NO_AUDIO));
        assert_eq!(!rehearsal, !env.contains_key("XDG_DATA_HOME"));
    }
}

/// The probe's answer must name Threema in `registered=` and in `on=`; the refusal says what the
/// line said (kinds only).
#[test]
fn the_probe_must_name_threema_registered_and_on() {
    judge_probe("networks registered=Threema off= on=Threema\n").expect("registered");
    judge_probe("noise\nnetworks registered=Signal,Threema off=WhatsApp on=Threema,Signal\n")
        .expect("a list");
    let e = error_of(judge_probe("networks registered=Signal off=Threema on=\n"));
    assert!(e.contains("the switch did not take"), "{e}");
    let e = error_of(judge_probe(
        "networks registered=Signal off=Threema on=Threema\n",
    ));
    assert!(e.contains("would not register Threema"), "{e}");
    assert!(
        e.contains("registered=`Signal`") && e.contains("off=`Threema`"),
        "{e}"
    );
    for line in [
        "networks registered=ThreemaX off= on=Threema",
        "networks registered=threema off= on=Threema",
        "networks off=Threema registered= on=Threema",
    ] {
        assert!(
            error_of(judge_probe(line)).contains("would not register Threema"),
            "{line}"
        );
    }
    assert!(error_of(judge_probe("something else\n")).contains("printed no `networks` line"));
    assert!(error_of(judge_probe("")).contains("printed no `networks` line"));
    // Whatever else the line carries is cut to kinds.
    let e = error_of(judge_probe(
        "networks registered=Sig;rm$(x) off= on=Threema",
    ));
    assert!(e.contains("registered=`Sigrmx`"), "{e}");
}

/// A clock that moves only when a wait polls and finds nothing.
struct StepClock {
    now: Cell<u64>,
    step_ms: u64,
}

impl WaitClock for StepClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
    fn idle(&self) {
        self.now.set(self.now.get() + self.step_ms);
    }
}

/// A script that plays a program.
fn executable(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, script).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The probe, run: a program that prints the line passes; one that fails says its status and how
/// many lines it wrote to stderr (never the lines); one that does not end is killed at its bound.
#[test]
fn the_probe_runs_the_stamped_app_bounded() {
    let scratch = Scratch::new("l-probe");
    let tools = scratch.0.join("tools");
    private_dir(&tools).expect("tools");
    let dir = fake_dir(false);
    let clock = StepClock {
        now: Cell::new(0),
        step_ms: 5,
    };
    let run_for = |program: &Path, bound_ms: u64| {
        probe(
            program,
            &dir,
            false,
            &[],
            &Wait {
                bound: Duration::from_millis(bound_ms),
                poll: Duration::from_millis(5),
                clock: &clock,
            },
        )
    };
    let good = executable(
        &tools,
        "good",
        "#!/bin/sh\necho 'networks registered=Threema off= on=Threema'\n",
    );
    run_for(&good, 30_000).expect("the probe passes");
    let wrong = executable(
        &tools,
        "wrong",
        "#!/bin/sh\necho 'networks registered=Signal off= on=Threema'\n",
    );
    assert!(error_of(run_for(&wrong, 30_000)).contains("would not register Threema"));
    let failing = executable(
        &tools,
        "failing",
        "#!/bin/sh\necho 'boom words of a name' >&2\necho 'second' >&2\nexit 3\n",
    );
    let e = error_of(run_for(&failing, 30_000));
    assert!(
        e.contains("exit status 3") && e.contains("2 line(s)"),
        "{e}"
    );
    assert!(!e.contains("boom") && !e.contains("second"), "{e}");
    // 0.5 s of silence on the clock: a hundred idle polls.
    let hung = executable(&tools, "hung", "#!/bin/sh\nexec sleep 600\n");
    let e = error_of(run_for(&hung, 500));
    assert!(e.contains("did not finish its probe in time"), "{e}");
}

// ---- The stamped copy ---------------------------------------------------------------------------------

/// A built app (a script: nothing starts it) at `<target>/debug/kourier`.
fn built_app(w: &World) -> PathBuf {
    let binary = w.target().join("debug").join("kourier");
    std::fs::create_dir_all(binary.parent().expect("parent")).expect("target");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("binary");
    binary
}

#[test]
fn sha256_is_computed_by_sha256sum_on_stdin() {
    let abc = [
        "ba7816bf8f01cfea414140de5dae2223",
        "b00361a396177a9cb410ff61f20015ad",
    ]
    .concat();
    assert_eq!(sha256_bytes(b"abc").expect("a hash"), abc);
}

/// The copy is execute-only, the stamp is private, both agree; `--no-build` reuses the copy only at
/// the commit it was built at; a copy that changed is refused.
#[test]
fn a_stamped_copy_is_execute_only_and_is_reused_only_at_its_commit() {
    let w = World::new("l-stamp");
    let source = built_app(&w);
    let cleared = w.run(&start(&["--for-link"])).expect("the good world");
    let dir = cleared.dir.clone();
    let stamp = stamp_copy(&cleared, &source, 5).expect("a stamp");
    assert_eq!(mode_of(&dir.copy_file()), 0o100);
    assert_eq!(mode_of(&dir.stamp_file()), 0o600);
    assert_eq!(mode_of(&dir.bin_dir()), 0o700);
    assert_eq!(stamp.commit.as_deref(), Some(HEAD));
    assert!(stamp.clean);
    assert_eq!(stamp.build.as_deref(), Some("debug"));
    assert_eq!(stamp.migrations.len(), 6);
    assert_eq!(
        stamp.sha256,
        sha256_bytes(b"#!/bin/sh\nexit 0\n").expect("a hash"),
        "the hash of the copy is the hash of the built bytes"
    );
    assert_eq!(check_copy(&dir).expect("intact"), stamp);
    assert_eq!(reuse_copy(&dir, Some(HEAD)).expect("same commit"), stamp);
    let e = error_of(reuse_copy(&dir, Some("0123456789abcdef")));
    assert!(
        e.contains("built at a1b2c3d4e5, and this start runs at 0123456789"),
        "{e}"
    );
    assert!(error_of(reuse_copy(&dir, None)).contains("runs at no commit"));

    // A copy somebody made readable.
    std::fs::set_permissions(dir.copy_file(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod");
    assert!(error_of(check_copy(&dir)).contains("mode 700"));
    std::fs::set_permissions(dir.copy_file(), std::fs::Permissions::from_mode(0o100))
        .expect("chmod");
    check_copy(&dir).expect("intact again");
    // Another file under the same name: the stamp was made for another inode.
    let other = dir.bin_dir().join("other");
    std::fs::write(&other, "x").expect("other");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o100)).expect("chmod");
    std::fs::rename(&other, dir.copy_file()).expect("replace");
    assert!(error_of(check_copy(&dir)).contains("not the file that was stamped"));
    // No stamp.
    std::fs::remove_file(dir.stamp_file()).expect("remove");
    assert!(error_of(check_copy(&dir)).contains("no stamped copy"));
}

/// A rehearsal's copy has no commit, and `--no-build` of the other kind refuses it.
#[test]
fn a_rehearsals_copy_has_no_commit() {
    let w = World::new("l-stamp2");
    let source = built_app(&w);
    let place = w
        .target()
        .join("thr")
        .join("1")
        .join("x")
        .join(KEEPER_DIR)
        .join("p");
    let a = args(&[
        "start",
        "--for-link",
        "--rehearsal-place",
        place.to_str().expect("utf-8"),
    ])
    .expect("a command line");
    let cleared = w.run(&a).expect("a rehearsal");
    let stamp = stamp_copy(&cleared, &source, 5).expect("a stamp");
    assert_eq!((stamp.commit.clone(), stamp.clean), (None, false));
    reuse_copy(&cleared.dir, None).expect("a rehearsal reuses its copy");
    assert!(error_of(reuse_copy(&cleared.dir, Some(HEAD))).contains("built at no commit"));
}

/// A binary that changes while it is copied (a build beside) is refused, and the half copy goes.
#[test]
fn a_binary_that_changes_while_it_is_copied_is_refused() {
    let scratch = Scratch::new("l-copy");
    let source = scratch.0.join("src");
    let tmp = scratch.0.join("tmp");
    std::fs::write(&source, "one").expect("source");
    let calls = Cell::new(0);
    let honest = |_: &Path| -> Result<String> { Ok("h".to_owned()) };
    assert_eq!(copy_checked(&source, &tmp, &honest).expect("copied"), "h");
    assert_eq!(std::fs::read_to_string(&tmp).expect("copy"), "one");
    let moving = |_: &Path| -> Result<String> {
        calls.set(calls.get() + 1);
        Ok(format!("h{}", calls.get()))
    };
    let e = error_of(copy_checked(&source, &tmp, &moving));
    assert!(e.contains("changed while it was copied"), "{e}");
    assert!(!tmp.exists());
    let e = error_of(copy_checked(&scratch.0.join("missing"), &tmp, &honest));
    assert!(e.contains("run without --no-build"), "{e}");
}

// ---- The control socket -------------------------------------------------------------------------------

/// A stand-in supervisor: serves `answer` to the next `n` connections, one line in, one line out.
fn serve_answers(listener: UnixListener, answer: Value, n: usize) {
    std::thread::spawn(move || {
        for stream in listener.incoming().take(n).flatten() {
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            let mut stream = stream;
            let _ = writeln!(stream, "{answer}");
        }
    });
}

/// The socket's name is `kthr-<16 hex>.sock`, stable for a profile's path and different for another.
#[test]
fn the_socket_name_is_short_and_follows_the_profile() {
    let a = socket_name(Path::new("/x/kourier-threema-run/a")).expect("a name");
    let again = socket_name(Path::new("/x/kourier-threema-run/a")).expect("a name");
    let b = socket_name(Path::new("/x/kourier-threema-run/b")).expect("a name");
    assert_eq!(a, again);
    assert_ne!(a, b);
    assert!(a.starts_with("kthr-") && a.ends_with(".sock"), "{a}");
    assert_eq!(a.len() + 1, SOCKET_NAME_LEN);
    assert!(
        a["kthr-".len()..a.len() - ".sock".len()]
            .bytes()
            .all(|c| c.is_ascii_hexdigit())
    );
}

/// Critic C7: a checkout path of 200 bytes and more. The old place (a socket in the profile) would
/// not bind there; the socket in the runtime directory is 41 bytes' worth whatever the checkout's
/// length, binds, serves and is found again.
#[test]
fn a_checkout_place_of_200_bytes_does_not_matter_to_the_socket() {
    let scratch = Scratch::new("l-long");
    let run = scratch.0.join("r");
    private_dir(&run).expect("the runtime directory");
    let mut long = scratch.0.clone();
    for part in ["a", "b", "c"] {
        long.push(part.repeat(60));
    }
    let profile = long.join("data").join(KEEPER_DIR).join("p");
    private_dir(&profile).expect("a deep profile");
    assert!(
        profile.as_os_str().len() >= 200,
        "{}",
        profile.as_os_str().len()
    );
    let dir = ProfileDir {
        dir: profile.clone(),
        rehearsal: false,
    };
    // The trap: a socket below the profile does not fit.
    private_dir(&dir.tool()).expect("tool");
    let inside = profile.join("threema-run").join("ctl.sock");
    assert!(inside.as_os_str().len() > SOCKET_PATH_MAX);
    assert!(UnixListener::bind(&inside).is_err());
    // The tool's place does.
    let socket = socket_path(&run, &dir.dir).expect("a short socket path");
    assert!(socket.as_os_str().len() <= SOCKET_PATH_MAX);
    assert_eq!(socket.parent(), Some(run.as_path()));
    let listener = listen_at(&socket).expect("bound");
    serve_answers(listener, json!({"ok": true, "pid": 7}), 3);
    assert_eq!(
        info_at(&socket, Duration::from_secs(5)).expect("served")["pid"],
        json!(7)
    );
    // A client finds it from the runtime directory, or from what the supervisor recorded.
    let vars = |k: &str| (k == "XDG_RUNTIME_DIR").then(|| run.to_str().expect("utf-8").to_owned());
    assert_eq!(socket_for_client(&dir, &vars).expect("computed"), socket);
    let recorded = scratch.0.join("elsewhere.sock");
    std::fs::write(
        dir.supervisor_file(),
        format!("{}\n", json!({"socket": recorded})),
    )
    .expect("supervisor.json");
    assert_eq!(
        socket_for_client(&dir, &|_| None).expect("recorded"),
        recorded
    );
    std::fs::remove_file(dir.supervisor_file()).expect("remove");
    assert!(error_of(socket_for_client(&dir, &|_| None)).contains("XDG_RUNTIME_DIR is not set"));
}

/// The directory bound: 80 bytes fit, 81 do not (`SOCKET_NAME_LEN` is 27 of 107).
#[test]
fn a_socket_directory_that_is_too_long_is_refused() {
    let profile = Path::new("/x/kourier-threema-run/p");
    let ok = PathBuf::from(format!("/{}", "d".repeat(79)));
    assert_eq!(ok.as_os_str().len(), 80);
    socket_path(&ok, profile).expect("80 bytes fit");
    let long = PathBuf::from(format!("/{}", "d".repeat(80)));
    let e = error_of(socket_path(&long, profile));
    assert!(e.contains("would not fit"), "{e}");
}

/// A socket that answers is another supervisor's: refused. A stale one is replaced. Mode 0600.
#[test]
fn a_live_socket_is_refused_and_a_stale_one_is_replaced() {
    let scratch = Scratch::new("l-listen");
    let run = scratch.0.join("r");
    private_dir(&run).expect("run");
    let socket = socket_path(&run, Path::new("/x/kourier-threema-run/p")).expect("a path");
    let listener = listen_at(&socket).expect("bound");
    assert_eq!(mode_of(&socket), 0o600);
    assert!(error_of(listen_at(&socket)).contains("already listens"));
    drop(listener);
    // The file is still there but nobody listens: stale.
    assert!(socket.exists());
    listen_at(&socket).expect("a stale socket is replaced");
    // A path past the limit is refused with a number, not a bind error.
    let long = PathBuf::from(format!("/{}", "d".repeat(120)));
    assert!(error_of(listen_at(&long)).contains("too long"));
}

/// `info` is one line in and one line out; a refusal is an error; a dead socket is an error.
#[test]
fn info_is_one_line_in_and_one_line_out() {
    let scratch = Scratch::new("l-info");
    let socket = scratch.0.join("s.sock");
    assert!(info_at(&socket, Duration::from_millis(200)).is_err());
    let listener = listen_at(&socket).expect("bound");
    serve_answers(listener, json!({"ok": false}), 1);
    assert!(error_of(info_at(&socket, Duration::from_secs(5))).contains("refused"));
}

/// A rehearsal's socket directory: `<base>/kthr-<uid>-<secs>` (the place's own name), 0700; a place
/// outside `<target>/thr/` or without a short name of its own is refused.
#[test]
fn a_rehearsals_socket_directory_is_named_by_its_place() {
    let scratch = Scratch::new("l-shm");
    let target = scratch.0.join("target");
    let base = scratch.0.join("shm");
    let uid = current_uid();
    let place = |name: &str| ProfileDir {
        dir: target
            .join("thr")
            .join(name)
            .join("x")
            .join(KEEPER_DIR)
            .join("p"),
        rehearsal: true,
    };
    let got = rehearsal_socket_dir(&place("1700000000"), &target, &base, uid).expect("a directory");
    assert_eq!(got, base.join(format!("kthr-{uid}-1700000000")));
    assert_eq!(mode_of(&got), 0o700);
    for bad in ["a b", "a.b", &"z".repeat(25)] {
        assert!(
            rehearsal_socket_dir(&place(bad), &target, &base, uid).is_err(),
            "{bad:?}"
        );
    }
    let outside = ProfileDir {
        dir: scratch.0.join("elsewhere").join(KEEPER_DIR).join("p"),
        rehearsal: true,
    };
    assert!(rehearsal_socket_dir(&outside, &target, &base, uid).is_err());
    // Not this user's directory of mode 0700: refused (the same rule as the real place).
    std::fs::set_permissions(&got, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // `private_dir` tightens it again: a directory the tool makes is the tool's.
    rehearsal_socket_dir(&place("1700000000"), &target, &base, uid).expect("tightened");
    assert_eq!(mode_of(&got), 0o700);
}

// ---- The launch order ---------------------------------------------------------------------------------

/// The profile whose order is read, the runtime directory and the order as `start` writes it.
fn order_world(name: &str) -> (World, ProfileDir, Order) {
    let w = World::new(name);
    let dir = w.profile();
    private_dir(&dir.tool()).expect("tool");
    let order = Order {
        socket: socket_path(&w.run_dir(), &dir.dir).expect("a socket"),
        rehearsal: false,
        for_link: true,
    };
    (w, dir, order)
}

fn write_order(dir: &ProfileDir, value: &Value) {
    std::fs::write(dir.launch_file(), format!("{value}\n")).expect("order");
}

/// One order, read once: taken away, fresh, this profile's copy and socket, a private directory.
#[test]
fn the_launch_order_is_taken_once_and_judged() {
    let (w, dir, order) = order_world("l-order");
    let uid = current_uid();
    write_order(&dir, &order_json(&dir, &order, NOW));
    assert_eq!(
        take_launch(&dir, NOW + 5_000, uid).expect("an order"),
        order
    );
    assert!(!dir.launch_file().exists(), "taken away");
    assert!(error_of(take_launch(&dir, NOW, uid)).contains("no launch order"));
    assert_eq!(mode_of(&w.run_dir()), 0o700);

    // (what the order says, what is refused with)
    let good = order_json(&dir, &order, NOW);
    let with = |key: &str, value: Value| {
        let mut v = good.clone();
        v[key] = value;
        v
    };
    let rows: Vec<(Value, u64, &str)> = vec![
        (good.clone(), NOW + 121_000, "stale"),
        (good.clone(), NOW - 121_000, "stale"),
        (with("bin", json!("/other/kourier")), NOW, "another program"),
        (
            with("rehearsal", json!(true)),
            NOW,
            "disagree about the rehearsal",
        ),
        (with("socket", json!("rel.sock")), NOW, "no directory"),
        (with("socket", Value::Null), NOW, "no control socket"),
        (
            with(
                "socket",
                json!(w.run_dir().join("kthr-0000000000000000.sock")),
            ),
            NOW,
            "another control socket",
        ),
    ];
    for (value, now, refused) in rows {
        write_order(&dir, &value);
        let e = error_of(take_launch(&dir, now, uid));
        assert!(e.contains(refused), "{refused}: {e}");
        assert!(
            !dir.launch_file().exists(),
            "{refused}: taken away before it is judged"
        );
    }
    // A socket directory that is not private is refused (the preflight's rule for the place).
    std::fs::set_permissions(w.run_dir(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    write_order(&dir, &good);
    assert!(error_of(take_launch(&dir, NOW, uid)).contains("not your private directory"));
}

// ---- The profile's lock -------------------------------------------------------------------------------

#[test]
fn one_supervisor_holds_a_profile() {
    let scratch = Scratch::new("l-lock");
    let dir = ProfileDir {
        dir: scratch.0.join(KEEPER_DIR).join("p"),
        rehearsal: false,
    };
    private_dir(&dir.dir).expect("profile");
    let lock = take_profile_lock(&dir).expect("the first supervisor");
    assert!(!lock_is_free(&dir).expect("a probe"));
    assert!(error_of(take_profile_lock(&dir)).contains("another threema-run supervisor"));
    drop(lock);
    assert!(lock_is_free(&dir).expect("a probe"));
    let _again = take_profile_lock(&dir).expect("free again");
    assert_eq!(mode_of(&dir.lock_file()), 0o600);
}

// ---- The switch and its guard -------------------------------------------------------------------------

fn write_config(dir: &ProfileDir, text: &str) {
    std::fs::create_dir_all(dir.config_file().parent().expect("parent")).expect("config dir");
    std::fs::write(dir.config_file(), text).expect("config");
}

fn config_of(dir: &ProfileDir) -> String {
    std::fs::read_to_string(dir.config_file()).expect("kourier.toml")
}

/// `switch_off` reads the file as it is NOW (the app may have rewritten it), changes only the
/// `networks-on` entry, and refuses what it cannot read.
#[test]
fn the_switch_goes_off_in_the_file_as_it_is_then() {
    let scratch = Scratch::new("l-off");
    let dir = ProfileDir {
        dir: scratch.0.join(KEEPER_DIR).join("p"),
        rehearsal: false,
    };
    assert!(
        !switch_off(&dir).expect("no file"),
        "a missing file is off already"
    );
    write_config(
        &dir,
        "theme = \"x\"\nnetworks-on = [\"Threema\"]\n[ui]\nsize = 3\n",
    );
    assert!(switch_is_on(&dir));
    assert!(switch_off(&dir).expect("off"));
    assert_eq!(
        config_of(&dir),
        "theme = \"x\"\nnetworks-on = []\n[ui]\nsize = 3\n"
    );
    assert!(!switch_is_on(&dir));
    assert!(!switch_off(&dir).expect("again"), "nothing to change");
    // An app that rewrote the file with a missing entry: the entry goes in front.
    write_config(&dir, "theme = \"x\"\n");
    assert!(switch_off(&dir).expect("written"));
    assert_eq!(config_of(&dir), "networks-on = []\ntheme = \"x\"\n");
    // Never rewrites what it does not read.
    let doubled = "networks-on = [\"Threema\"]\nnetworks-on = []\n";
    write_config(&dir, doubled);
    assert!(switch_off(&dir).is_err());
    assert_eq!(config_of(&dir), doubled);
}

/// The guard puts the switch back when it is dropped armed; with a supervisor that may live it does
/// not (and says so); `disarm` leaves it on.
#[test]
fn the_guard_puts_the_switch_back_only_when_nobody_runs() {
    let scratch = Scratch::new("l-guard");
    let dir = ProfileDir {
        dir: scratch.0.join(KEEPER_DIR).join("p"),
        rehearsal: false,
    };
    let on = "networks-on = [\"Threema\"]\nx = 1\n";
    {
        let _guard = SwitchGuard::arm(&dir, on).expect("armed");
        assert!(switch_is_on(&dir));
    }
    assert_eq!(
        config_of(&dir),
        "networks-on = []\nx = 1\n",
        "dropped armed"
    );
    let guard = SwitchGuard::arm(&dir, on).expect("armed");
    guard.disarm();
    assert!(
        switch_is_on(&dir),
        "a disarmed guard leaves it to the supervisor"
    );
    // A supervisor that may live: the switch stays on, and the words say to stop.
    let mut guard = SwitchGuard::arm(&dir, on).expect("armed");
    guard.supervisor_started(|| false);
    let words = guard.put_back();
    assert_eq!(words.len(), 1);
    assert!(words[0].contains("stays switched ON") && words[0].contains("threema-run stop"));
    assert!(switch_is_on(&dir));
    drop(guard);
    assert!(switch_is_on(&dir), "put_back took the guard's arm away");
    // A supervisor that is gone: off.
    let mut guard = SwitchGuard::arm(&dir, on).expect("armed");
    guard.supervisor_started(|| true);
    let words = guard.put_back();
    assert!(
        words[0].contains("switched off in the profile again"),
        "{words:?}"
    );
    assert!(!switch_is_on(&dir));
}

// ---- start --------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Spawn {
    /// A stand-in supervisor binds the order's socket and answers `info` with these fields.
    Answers(Value),
    /// The supervisor cannot be started.
    Fails,
    /// Nothing answers.
    Silent,
    /// Nothing answers, but a supervisor holds the profile's lock.
    HoldsTheLock,
}

/// What the stand-ins saw, in order.
#[derive(Default)]
struct Seen {
    events: RefCell<Vec<&'static str>>,
    /// At the probe: the config text, whether the keeper root's list stood, the program it ran.
    probe: RefCell<Option<(String, bool, PathBuf, bool)>>,
    locks: RefCell<Vec<File>>,
}

struct Rig {
    probe_ok: bool,
    spawn: Spawn,
    timing: UpTiming,
    /// The stand-in supervisor keeps the profile's lock (it is alive) after it answered.
    holds_lock: bool,
    /// LOW-4: the probe raises this signal in the start's own process (the lead's shell call ends, a
    /// Ctrl-C) and then goes on as if nothing happened.
    probe_raises: Option<libc::c_int>,
    /// LOW-4: the same, right after the supervisor was spawned.
    spawn_raises: Option<libc::c_int>,
}

impl Rig {
    fn answering(info: Value) -> Self {
        Self {
            probe_ok: true,
            spawn: Spawn::Answers(info),
            timing: UpTiming {
                come_up: Duration::from_secs(20),
                gone_after: Duration::from_secs(5),
                poll: Duration::from_millis(10),
            },
            holds_lock: true,
            probe_raises: None,
            spawn_raises: None,
        }
    }

    fn failing(spawn: Spawn) -> Self {
        Self {
            probe_ok: true,
            spawn,
            timing: UpTiming {
                come_up: Duration::from_millis(400),
                gone_after: Duration::from_millis(100),
                poll: Duration::from_millis(10),
            },
            holds_lock: true,
            probe_raises: None,
            spawn_raises: None,
        }
    }

    fn run(
        &self,
        w: &World,
        a: &crate::threema_run::Args,
    ) -> (Result<crate::threema_run::launch::Up>, Seen) {
        let seen = Seen::default();
        let binary = w.target().join("debug").join("kourier");
        let build = || {
            seen.events.borrow_mut().push("build");
            std::fs::create_dir_all(binary.parent().expect("parent"))?;
            std::fs::write(&binary, "#!/bin/sh\nexit 0\n")?;
            Ok(())
        };
        let env = inherited();
        let probe_it = |dir: &ProfileDir, bin: &Path, rehearsal: bool| -> Result<()> {
            seen.events.borrow_mut().push("probe");
            *seen.probe.borrow_mut() = Some((
                std::fs::read_to_string(dir.config_file()).unwrap_or_default(),
                std::fs::symlink_metadata(dir.allow_file()).is_ok(),
                bin.to_owned(),
                rehearsal,
            ));
            if let Some(signal) = self.probe_raises {
                // SAFETY: `raise` only sends a signal to this very thread; the start's handler is the
                // one under test (without it the default action would end this test process).
                unsafe {
                    libc::raise(signal);
                }
            }
            if self.probe_ok {
                Ok(())
            } else {
                bail!("the probe failed")
            }
        };
        let spawn = |dir: &ProfileDir| -> Result<()> {
            seen.events.borrow_mut().push("spawn");
            match &self.spawn {
                Spawn::Fails => bail!("cannot start the supervisor"),
                Spawn::Silent => Ok(()),
                Spawn::HoldsTheLock => {
                    seen.locks
                        .borrow_mut()
                        .push(take_profile_lock(dir).expect("the stand-in's lock"));
                    Ok(())
                }
                Spawn::Answers(info) => {
                    // What the real supervisor does first: read the order, bind its socket.
                    let order = read_json(&dir.launch_file())?.expect("the order stands");
                    let socket = PathBuf::from(order["socket"].as_str().expect("a socket"));
                    let listener = listen_at(&socket)?;
                    let mut answer = info.clone();
                    answer["ok"] = json!(true);
                    serve_answers(listener, answer, 400);
                    if self.holds_lock {
                        seen.locks
                            .borrow_mut()
                            .push(take_profile_lock(dir).expect("the stand-in's lock"));
                    }
                    if let Some(signal) = self.spawn_raises {
                        // SAFETY: as the probe's: a signal to this thread, handled by the start.
                        unsafe {
                            libc::raise(signal);
                        }
                    }
                    Ok(())
                }
            }
        };
        let shm = w.scratch.0.join("s");
        let seams = Seams {
            build: &build,
            binary: &binary,
            inherited: &env,
            probe: &probe_it,
            spawn: &spawn,
            shm_base: &shm,
            up: &self.timing,
            id_line: "xtask threema-run id --profile p",
        };
        let up = w.with_ctx(crate::threema_run::preflight::LINES, |ctx| {
            start_in(a, ctx, &seams)
        });
        (up, seen)
    }
}

/// The list of test IDs in the keeper root (not made).
fn allow_file(w: &World) -> PathBuf {
    w.keeper().join(ALLOW_FILE)
}

fn waiting() -> Value {
    json!({"pid": 4242, "ready": false, "id": INFO_ID_WAITING, "halted": null, "self_dumpable": false})
}

/// The good start: the order of what is done (build, probe, spawn), the switch on and held by the
/// supervisor, the stamped copy, the state, the order the supervisor can take, the probe over the
/// stamped copy with the switch on and no list, and a line for him that carries no ID.
#[test]
fn a_real_start_brings_the_supervisor_up_and_leaves_the_switch_on() {
    let w = World::new("l-good");
    let rig = Rig::answering(waiting());
    let (up, seen) = rig.run(&w, &start(&["--for-link"]));
    let up = up.expect("a start");
    assert!(up.ok);
    assert_eq!(*seen.events.borrow(), ["build", "probe", "spawn"]);
    let dir = w.profile();
    let text = up.lines.join("\n");
    assert!(text.contains("supervisor: up (pid 4242)"), "{text}");
    assert!(text.contains("xtask threema-run id --profile p"), "{text}");
    assert!(text.contains("the TEST phone's ID"), "{text}");
    assert!(text.contains("profile:    p\n"), "{text}");
    assert!(text.contains("source:     main at a1b2c3d4e5"), "{text}");
    // Names, never a path: the scratch place holds the user's home in a real run.
    let scratch = w.scratch.0.to_str().expect("utf-8");
    assert!(!text.contains(scratch), "{text}");
    // The switch is on and the supervisor owns it now.
    assert_eq!(config_of(&dir), "networks-on = [\"Threema\"]\n");
    // The probe saw the switch on, no list, and the STAMPED copy.
    let (config, list_stood, bin, rehearsal) = seen.probe.borrow().clone().expect("the probe ran");
    assert_eq!(config, "networks-on = [\"Threema\"]\n");
    assert!(!list_stood && !rehearsal);
    assert_eq!(bin, dir.copy_file());
    // The files: the copy (execute-only), the state, the order.
    assert_eq!(mode_of(&dir.copy_file()), 0o100);
    let state = State::load(&dir).expect("state").expect("a state file");
    assert_eq!(state.num("starts"), Some(1));
    assert_eq!(state.get("build_commit"), &json!(HEAD));
    assert_eq!(mode_of(&dir.dir), 0o700);
    assert_eq!(mode_of(&dir.tool()), 0o700);
    let order = take_launch(&dir, NOW, current_uid()).expect("an order the supervisor can take");
    assert_eq!(
        order.socket,
        socket_path(&w.run_dir(), &dir.dir).expect("a path")
    );
    assert!(order.for_link && !order.rehearsal);
    // The ID and its digest are nowhere: the output, the state and the order have only words.
    for file in [dir.state_file(), dir.stamp_file()] {
        let held = std::fs::read_to_string(file).expect("file");
        assert!(!held.contains(&digest_shape()));
    }
}

/// A start of a linked profile (no `--for-link`): ready, not waiting for the ID; it says what is next.
#[test]
fn a_plain_start_of_a_linked_profile_says_the_app_is_up() {
    let w = World::new("l-plain");
    w.link_account();
    let rig = Rig::answering(json!({"pid": 9, "ready": true, "app_pid": 10, "halted": null}));
    let (up, _) = rig.run(&w, &start(&[]));
    let text = up.expect("a start").lines.join("\n");
    assert!(
        text.contains("app:        dev channel up (pid 10)"),
        "{text}"
    );
    assert!(text.contains("step reconnect"), "{text}");
    assert!(!text.contains("types the TEST phone's ID"), "{text}");
    assert!(switch_is_on(&w.profile()));
}

/// A refusal BEFORE the switch is written leaves no switch behind (nothing of kourier.toml): the
/// reuse of a copy that is not there.
#[test]
fn a_refusal_before_the_switch_writes_no_switch() {
    let w = World::new("l-early");
    let rig = Rig::answering(waiting());
    let (up, seen) = rig.run(&w, &start(&["--for-link", "--no-build"]));
    assert!(error_of(up).contains("no stamped copy"));
    assert!(
        seen.events.borrow().is_empty(),
        "no build, no probe, no spawn"
    );
    assert!(!w.profile().config_file().exists());
    assert!(!w.profile().launch_file().exists());
}

/// Critic M3: every exit after the write puts the switch back off, and each leaves the state as it
/// found it where it can. One world per exit.
#[test]
fn every_exit_after_the_switch_write_puts_it_back() {
    // The probe fails.
    let w = World::new("l-m3-probe");
    let mut rig = Rig::answering(waiting());
    rig.probe_ok = false;
    let (up, seen) = rig.run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("the probe failed"));
    assert_eq!(*seen.events.borrow(), ["build", "probe"]);
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");
    assert!(!switch_is_on(&w.profile()));
    assert!(
        !w.profile().launch_file().exists(),
        "no order before the probe passed"
    );

    // The supervisor cannot be started.
    let w = World::new("l-m3-spawn");
    let (up, _) = Rig::failing(Spawn::Fails).run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("cannot start the supervisor"));
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");

    // The order cannot be written: the state is as it was (the halt is not spent).
    let w = World::new("l-m3-order");
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set("halted", json!({"kind": "clock", "at_ms": 7}));
    });
    std::fs::create_dir(w.profile().launch_file()).expect("a directory where the order goes");
    let (up, seen) = Rig::answering(waiting()).run(&w, &start(&["--after-halt"]));
    assert!(up.is_err());
    assert_eq!(*seen.events.borrow(), ["build", "probe"]);
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");
    let state = State::load(&w.profile()).expect("state").expect("a file");
    assert_eq!(
        state.halt_kind(),
        Some("clock"),
        "a refused start spends no halt"
    );
    assert_eq!(state.num("starts"), Some(0));

    // The state cannot be saved: the order goes again, the switch with it.
    let w = World::new("l-m3-state");
    w.link_account();
    std::fs::create_dir(w.profile().tool().join("state.tmp"))
        .expect("a directory where the copy goes");
    let (up, _) = Rig::answering(waiting()).run(&w, &start(&[]));
    assert!(up.is_err());
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");
    assert!(
        !w.profile().launch_file().exists(),
        "the order did not outlive the refused start"
    );

    // The supervisor halted as it started and has let go of the profile: the switch goes off; the
    // error says the kind.
    let w = World::new("l-m3-halt");
    let info = json!({"pid": 4242, "ready": false, "halted": "clock"});
    let mut rig = Rig::answering(info);
    rig.holds_lock = false;
    let (up, _) = rig.run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("halted as it started (clock)"));
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");

    // The supervisor never came up and holds nothing.
    let w = World::new("l-m3-silent");
    let (up, _) = Rig::failing(Spawn::Silent).run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("did not come up"));
    assert_eq!(config_of(&w.profile()), "networks-on = []\n");
}

/// A start that fails while a supervisor holds the profile (it may be running the app) leaves the
/// switch ON: it is written off only after the app's process is gone, by that supervisor's end or
/// `stop` (critic M3: the app's own preference save could write it back).
#[test]
fn a_failed_start_with_a_live_supervisor_leaves_the_switch_for_its_end() {
    let w = World::new("l-m3-alive");
    let (up, seen) = Rig::failing(Spawn::HoldsTheLock).run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("did not come up"));
    assert_eq!(seen.locks.borrow().len(), 1);
    assert!(
        switch_is_on(&w.profile()),
        "the supervisor's end writes it off"
    );
    // The same with a halt the live supervisor reported.
    let w = World::new("l-m3-alive2");
    let rig = Rig::answering(json!({"pid": 1, "halted": "clock"}));
    let (up, seen) = rig.run(&w, &start(&["--for-link"]));
    assert!(error_of(up).contains("halted as it started"));
    assert_eq!(
        seen.locks.borrow().len(),
        1,
        "the stand-in still holds the lock"
    );
    assert!(switch_is_on(&w.profile()));
}

/// A list the last run left in the keeper root is removed BEFORE the probe (the probe reads the
/// list too) and the start says so.
#[test]
fn a_leftover_list_is_removed_before_the_probe() {
    let w = World::new("l-left");
    std::fs::write(allow_file(&w), format!("sha256:{}\n", digest_shape())).expect("a list");
    std::fs::set_permissions(allow_file(&w), std::fs::Permissions::from_mode(0o600))
        .expect("chmod");
    let (up, seen) = Rig::answering(waiting()).run(&w, &start(&["--for-link"]));
    let text = up.expect("a start").lines.join("\n");
    assert!(
        text.contains("was left by a run that did not end cleanly: the start removes it"),
        "{text}"
    );
    assert!(
        text.contains("left in the keeper root by a run that did not end cleanly; removed"),
        "{text}"
    );
    assert!(!allow_file(&w).exists());
    let (_, list_stood, _, _) = seen.probe.borrow().clone().expect("the probe ran");
    assert!(!list_stood, "the probe saw no list");
    assert!(!text.contains(&digest_shape()));
}

/// A login that stood at `linking` and was not seen to end: the start opens the question in the keeper
/// root (the kind and the time, nothing else, 0600), spends `link_at`, and goes on so the
/// supervisor can serve the answer.
#[test]
fn a_login_that_stood_at_linking_opens_the_question() {
    let w = World::new("l-question");
    w.save_state(|s| s.set("link_at", json!(5)));
    let (up, _) = Rig::answering(waiting()).run(&w, &start(&["--for-link"]));
    let text = up.expect("a start").lines.join("\n");
    assert!(
        text.contains("QUESTION:") && text.contains("a device may be registered"),
        "{text}"
    );
    let file = w.keeper().join(OPEN_QUESTION_FILE);
    assert_eq!(mode_of(&file), 0o600);
    let question: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("file")).expect("json");
    assert_eq!(question, json!({"kind": QUESTION_KIND, "at_ms": NOW}));
    let state = State::load(&w.profile()).expect("state").expect("a file");
    assert!(
        !state.link_stood(),
        "link_at is spent: the question carries it now"
    );
    // The question's own writer, alone.
    let dir = w.profile();
    std::fs::remove_file(&file).expect("remove");
    open_question(&dir, 9).expect("written");
    assert_eq!(mode_of(&file), 0o600);
    assert!(
        std::fs::read_to_string(&file)
            .expect("file")
            .contains("\"at_ms\": 9")
    );
}

/// The rehearsal: its socket is in a directory of its own below the shm base, the order says so,
/// the copy has no commit and the probe is told it is a rehearsal.
#[test]
fn a_rehearsal_start_keeps_its_socket_in_its_own_short_directory() {
    let w = World::new("l-rh");
    let place = w
        .target()
        .join("thr")
        .join("1700")
        .join("x")
        .join(KEEPER_DIR)
        .join("p");
    let a = args(&[
        "start",
        "--for-link",
        "--rehearsal-place",
        place.to_str().expect("utf-8"),
    ])
    .expect("a command line");
    let (up, seen) = Rig::answering(waiting()).run(&w, &a);
    let up = up.expect("a rehearsal start");
    assert!(up.ok);
    let (config, _, bin, rehearsal) = seen.probe.borrow().clone().expect("the probe ran");
    assert!(rehearsal);
    assert_eq!(config, "networks-on = [\"Threema\"]\n");
    let dir = ProfileDir {
        dir: place.clone(),
        rehearsal: true,
    };
    assert_eq!(bin, dir.copy_file());
    let order = take_launch(&dir, NOW, current_uid()).expect("an order");
    let socket_dir = w
        .scratch
        .0
        .join("s")
        .join(format!("kthr-{}-1700", current_uid()));
    assert_eq!(
        order.socket,
        socket_path(&socket_dir, &place).expect("a path")
    );
    assert!(order.rehearsal);
    let text = up.lines.join("\n");
    assert!(text.contains("profile:    p (rehearsal)"), "{text}");
    assert!(text.contains("stamped copy at no commit"), "{text}");
    assert!(!text.contains("source:"), "{text}");
}

/// LOW-4: a SIGINT, SIGTERM or SIGHUP that comes while the start holds the switch (the lead's shell
/// call ends while the probe runs, a Ctrl-C) does not kill it where it stands with Threema switched
/// ON and no supervisor: the start ends with an error, the guard puts the switch back, nothing is
/// spawned, the order is not left, and the signals have their earlier meaning again afterwards.
#[test]
fn a_signal_during_the_start_puts_the_switch_back_and_spawns_nothing() {
    for (name, signal) in [
        ("l-sig-term", libc::SIGTERM),
        ("l-sig-int", libc::SIGINT),
        ("l-sig-hup", libc::SIGHUP),
    ] {
        let w = World::new(name);
        let mut rig = Rig::answering(waiting());
        rig.probe_raises = Some(signal);
        let (up, seen) = rig.run(&w, &start(&["--for-link"]));
        let error = error_of(up);
        assert!(error.contains("a signal came"), "{name}: {error}");
        assert!(
            error.contains("switched off in the profile again"),
            "{name}: {error}"
        );
        assert_eq!(
            *seen.events.borrow(),
            ["build", "probe"],
            "{name}: the supervisor was never spawned"
        );
        assert_eq!(config_of(&w.profile()), "networks-on = []\n", "{name}");
        assert!(!switch_is_on(&w.profile()), "{name}");
        assert!(!w.profile().launch_file().exists(), "{name}: no order left");
    }
    // The signals mean again what they meant before the start (the default: they end the process).
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `sigaction` with no new action only reads the current one into a plain struct.
        let current = unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(signal, std::ptr::null(), &raw mut old), 0);
            old
        };
        assert_eq!(current.sa_sigaction, libc::SIG_DFL, "signal {signal}");
    }
    // A signal AFTER the supervisor was spawned ends the wait too, and says the supervisor owns the
    // switch (it is alive: its stand-in holds the profile's lock).
    let w = World::new("l-sig-after");
    let mut rig = Rig::answering(waiting());
    rig.spawn_raises = Some(libc::SIGTERM);
    let (up, seen) = rig.run(&w, &start(&["--for-link"]));
    let error = error_of(up);
    assert!(
        error.contains("supervisor was started and owns the switch"),
        "{error}"
    );
    assert!(seen.events.borrow().contains(&"spawn"));
    assert_eq!(
        config_of(&w.profile()),
        "networks-on = [\"Threema\"]\n",
        "a live supervisor owns the switch; the guard leaves it"
    );
}
