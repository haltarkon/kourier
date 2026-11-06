//! The security fix round (TR-lite, the Opus security review of 2026-11-04): one test for each finding
//! that is not already a table of another module (the judge's MEDIUM-2 and MEDIUM-1 tables and LOW-9's
//! flag are in `judge.rs`, `sync`'s FAIL in `steps.rs`, an end step giving the removal back in
//! `steps/part2.rs`, LOW-4's signal in `launch.rs`, LOW-5's lock in `profile.rs`). The apps are shell
//! scripts under the real supervisor; every forbidden value is built from parts; nothing contacts a
//! host, no window, no sound. Every wait is on an outcome; a test that says "nothing happened" waits
//! for a count of lines or a state first, and a fixed sleep only where the rule under test is that a
//! thing does NOT happen. Mutants of the round: the watchdog off and the job-box budget off.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::lines::count_line;
use super::prompt::raw_op;
use super::supervise::{Rig, Running, quick};
use super::{World, digest_shape, id_shape};
use crate::threema_run::preflight::{Lock, app_lock_file, probe_lock};
use crate::threema_run::profile::{
    ProfileDir, State, ensure, open_lock_file, remove_leftover_list, wipe_in,
};
use crate::threema_run::prompt::{list_text, remove_the_list, write_the_list};
use crate::threema_run::supervise::{
    StopTiming, Timing, is_the_step_cmdline, proc_file_readable, readable_files, state_in, stop_in,
};

// ---- The stand-in app ----------------------------------------------------------------------------------

/// The app: its dev channel is up and the account online at once. `FIX_CASE` says what its log does:
/// `silent-online` prints no count line (the log never reaches the tool), `counting-online` prints one
/// every 50 ms (`FIX_COUNT`, a line of the contract).
const FIX_APP: &str = r##"#!/bin/sh
say() { printf '%s\n' "$1"; }
say '{"type":"event","event":"channel","data":{"protocol":1}}'
say '{"type":"event","event":"account_state","data":{"account":1,"network":"Threema","state":"active","conn":"online","device":2}}'
BG=
if [ "$FIX_CASE" = counting-online ]; then
  ( while :; do printf '%s\n' "$FIX_COUNT" >&2; sleep 0.05; done ) &
  BG=$!
fi
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  cmd=$(printf '%s' "$line" | sed 's/.*"cmd":"\([a-z_]*\)".*/\1/')
  case "$cmd" in
    quit) [ -n "$BG" ] && kill "$BG" 2>/dev/null
      printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id"; exit 0 ;;
    *) printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id" ;;
  esac
done
[ -n "$BG" ] && kill "$BG" 2>/dev/null
exit 0
"##;

fn fix_env(case: &str) -> Vec<(&'static str, String)> {
    vec![
        ("FIX_CASE", case.to_owned()),
        ("FIX_COUNT", count_line("running", &[], &[])),
    ]
}

fn start_app(rig: &Rig, case: &str, timing: Timing) -> Running {
    rig.run_script(FIX_APP, fix_env(case), false, timing)
}

fn watch(ms: u64) -> Timing {
    Timing {
        count_watch: Duration::from_millis(ms),
        ..quick()
    }
}

fn halted_kind(rig: &Rig) -> Value {
    rig.state()["halted"]["kind"].clone()
}

// ---- MEDIUM-1: the blind judge ------------------------------------------------------------------------

/// The app is online and no count line ever reaches the tool: in a REAL place the supervisor halts the
/// run `count-lines-missing` after the bound, the app is asked to quit and the switch is off after
/// its exit. Mutant: the watchdog off (the loop's `count_lines_missing` call) kills this test.
#[test]
fn an_online_account_with_no_count_line_halts_the_run_in_a_real_place() {
    let rig = Rig::new("f-blind");
    let run = start_app(&rig, "silent-online", watch(400));
    let code = run
        .join_within(Duration::from_secs(60))
        .expect("the watchdog ended the run")
        .expect("ended");
    assert_eq!(code, 1);
    assert_eq!(halted_kind(&rig), json!("count-lines-missing"));
    rig.assert_ended("a blind judge");
}

/// The twin: the same account with a count line every 50 ms does not halt, however long it stands
/// (60 lines are three bounds); the run ends by its stop, with no halt.
#[test]
fn an_online_account_that_prints_its_count_lines_is_not_halted() {
    let rig = Rig::new("f-sighted");
    let mut run = start_app(&rig, "counting-online", watch(1000));
    run.ready();
    let info = run.wait_for("sixty count lines", |i| {
        i["lines"]["L_COUNT"].as_u64() >= Some(60)
    });
    assert!(info["halted"].is_null(), "{info}");
    assert_eq!(info["online"], json!(true));
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
    assert!(rig.state()["halted"].is_null());
    rig.assert_ended("a sighted run");
}

/// A rehearsal's fake prints no count line: no halt there, however long the account is online.
#[test]
fn a_rehearsal_never_asks_for_count_lines() {
    let rig = Rig::new("f-blind-rehearsal").rehearsing();
    let mut run = start_app(&rig, "silent-online", watch(150));
    run.wait_for("online", |i| i["online"] == json!(true));
    // Six bounds: a real place would have halted at the first.
    std::thread::sleep(Duration::from_millis(900));
    let info = run.info().expect("info");
    assert!(info["halted"].is_null(), "{info}");
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
    rig.assert_ended("a rehearsal");
}

// ---- LOW-1: the list's temporary file -----------------------------------------------------------------

fn keeper_files_hold(w: &World, needle: &str) -> Vec<String> {
    std::fs::read_dir(w.keeper())
        .expect("the keeper root")
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| {
            std::fs::read_to_string(e.path()).is_ok_and(|text| text.contains(needle))
                || e.file_name().to_string_lossy().contains(needle)
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

fn profile_of(w: &World) -> ProfileDir {
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    ensure(&dir, &State::new(1)).expect("the profile");
    dir
}

/// The file the list is written through is the one every cleanup knows, and a write that cannot
/// finish leaves no file with the digest: a directory at the list's name is refused BEFORE the
/// temporary file exists.
#[test]
fn a_list_that_cannot_be_written_leaves_no_file_with_the_digest() {
    let w = World::new("f-tmp-refused");
    let dir = profile_of(&w);
    // The tmp name is `write_private`'s (it adds the extension `tmp`).
    assert_eq!(dir.allow_tmp(), dir.allow_file().with_extension("tmp"));
    std::fs::create_dir(dir.allow_file()).expect("a directory where the list goes");
    let error = format!(
        "{:#}",
        write_the_list(&dir, &digest_shape()).expect_err("refused")
    );
    assert!(error.contains("not a plain file"), "{error}");
    assert!(!dir.allow_tmp().exists());
    assert_eq!(keeper_files_hold(&w, &digest_shape()), Vec::<String>::new());
    // A plain file there is replaced (the leftover of an earlier run), and the lock is the caller's.
    std::fs::remove_dir(dir.allow_file()).expect("removed");
    std::fs::write(dir.allow_file(), "old\n").expect("a leftover");
    let lock = write_the_list(&dir, &digest_shape()).expect("written");
    assert_eq!(
        std::fs::read_to_string(dir.allow_file()).expect("the list"),
        list_text(&digest_shape())
    );
    assert!(!dir.allow_tmp().exists(), "renamed away");
    assert!(remove_the_list(&dir, Some(lock)).expect("removed"));
    assert!(!dir.allow_file().exists());
}

/// A temporary file a killed supervisor left (between the create and the rename) holds the digest:
/// every cleanup removes it: the end of a run (`remove_the_list`), the start's leftover check, `wipe`.
#[test]
fn every_cleanup_removes_the_temporary_file_of_the_list_too() {
    let planted = |w: &World, dir: &ProfileDir| {
        std::fs::write(dir.allow_tmp(), list_text(&digest_shape())).expect("a temporary list");
        assert_eq!(
            keeper_files_hold(w, &digest_shape()),
            ["threema-allowed-ids.tmp"]
        );
    };
    // The end of a run, with the list and with the temporary file alone.
    let w = World::new("f-tmp-end");
    let dir = profile_of(&w);
    planted(&w, &dir);
    assert!(
        remove_the_list(&dir, None).expect("removed"),
        "the file alone is a removal"
    );
    assert!(keeper_files_hold(&w, &digest_shape()).is_empty());
    planted(&w, &dir);
    std::fs::write(dir.allow_file(), list_text(&digest_shape())).expect("a list");
    assert!(remove_the_list(&dir, None).expect("removed"));
    assert!(keeper_files_hold(&w, &digest_shape()).is_empty());
    assert!(
        !remove_the_list(&dir, None).expect("nothing"),
        "nothing left"
    );
    // The start's leftover check says what it removed.
    let w = World::new("f-tmp-leftover");
    let dir = profile_of(&w);
    planted(&w, &dir);
    let words = remove_leftover_list(&dir).expect("removed").join("\n");
    assert!(
        words.contains("temporary file") && words.contains("left in the keeper root"),
        "{words}"
    );
    assert!(keeper_files_hold(&w, &digest_shape()).is_empty());
    assert!(remove_leftover_list(&dir).expect("nothing").is_empty());
    // A run that holds the keeper lock keeps its file.
    planted(&w, &dir);
    let held = open_lock_file(&dir.keeper_lock_file()).expect("the lock file");
    held.try_lock().expect("the test is the other run");
    let words = remove_leftover_list(&dir).expect("left").join("\n");
    assert!(
        words.contains("another run holds the keeper lock"),
        "{words}"
    );
    assert!(dir.allow_tmp().exists());
    drop(held);
    // `wipe` takes it with the profile.
    let (lines, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(
        lines.iter().any(|l| l.contains("temporary file")),
        "{lines:?}"
    );
    assert!(keeper_files_hold(&w, &digest_shape()).is_empty());
    assert!(!dir.dir.exists());
}

// ---- LOW-2: a list a killed supervisor left ------------------------------------------------------------

fn short_stop() -> StopTiming {
    StopTiming {
        supervisor: Duration::from_millis(300),
        app: Duration::from_millis(300),
        poll: Duration::from_millis(10),
    }
}

/// `state` shows a list (or its temporary file) that stands with no run, and `stop` removes it, with
/// words; a list under another run's live keeper lock stays and the words say whose it is.
#[test]
fn stop_removes_the_list_a_killed_supervisor_left_and_state_shows_it() {
    let rig = Rig::new("f-stop-list");
    let (list, tmp) = (rig.dir.allow_file(), rig.dir.allow_tmp());
    let text = |rig: &Rig| state_in(&rig.dir, None).expect("state").join("\n");
    assert!(!text(&rig).contains("a list of test IDs"), "{}", text(&rig));
    std::fs::write(&list, list_text(&digest_shape())).expect("a list");
    std::fs::write(&tmp, list_text(&digest_shape())).expect("a temporary list");
    let shown = text(&rig);
    assert!(
        shown.contains("list:") && shown.contains("stands in the keeper root"),
        "{shown}"
    );
    assert!(!shown.contains(&digest_shape()));
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {
        panic!("no signal")
    })
    .expect("stop");
    let words = words.join("\n");
    assert_eq!(status, 0, "{words}");
    assert!(
        words.contains("left in the keeper root") && words.contains("removed"),
        "{words}"
    );
    assert!(!list.exists() && !tmp.exists());
    assert_eq!(rig.config(), "networks-on = []\n");
    // Another run's live list: left to it, in words, and the status is still 0 (nothing of ours).
    std::fs::write(&list, list_text(&digest_shape())).expect("a list");
    let held = open_lock_file(&rig.dir.keeper_lock_file()).expect("the lock file");
    held.try_lock().expect("the test is the other run");
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {
        panic!("no signal")
    })
    .expect("stop");
    let words = words.join("\n");
    assert_eq!(status, 0, "{words}");
    assert!(
        words.contains("another run holds the keeper lock"),
        "{words}"
    );
    assert!(list.exists());
    drop(held);
}

// ---- LOW-3: the other profiles, looked at again when the list is written ------------------------------

fn hold(path: &Path) -> std::fs::File {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("directories");
    let file = open_lock_file(path).expect("a lock file");
    file.try_lock().expect("the test is the other process");
    file
}

/// The list is read by the app of ANY direct child of the keeper root (a hand-made `x.y` too), so
/// `allow` looks at every one of them right before it writes, and refuses while a Kourier or a
/// supervisor runs on one: nothing is written, the wait goes on, and it takes the digest once they are
/// gone.
#[test]
fn the_list_is_not_written_while_a_kourier_runs_on_any_child_of_the_keeper_root() {
    let rig = Rig::new("f-children");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let allow = json!({"op": "test_allow", "digest": digest_shape()});
    let refused = |run: &Running, why: &str| {
        let error = format!("{:#}", run.call(&allow).expect_err(why));
        assert!(error.contains("otherKourierRunning"), "{why}: {error}");
        assert!(!error.contains(&digest_shape()), "echoed: {error}");
        assert!(!rig.dir.allow_file().exists() && !rig.dir.allow_tmp().exists());
        assert_eq!(run.info().expect("info")["id"], json!("waiting"), "{why}");
    };
    // The app of a child with a name no profile may have.
    let hand_made = ProfileDir {
        dir: rig.w.keeper().join("x.y"),
        rehearsal: false,
    };
    let app = hold(&app_lock_file(&hand_made));
    refused(&run, "an app on x.y");
    drop(app);
    // The supervisor of another profile.
    let other = ProfileDir {
        dir: rig.w.keeper().join("q"),
        rehearsal: false,
    };
    let tool = hold(&other.lock_file());
    refused(&run, "a supervisor on q");
    drop(tool);
    // The app of THIS profile's own directory would be another run's too (the app starts after this).
    let own = hold(&app_lock_file(&rig.dir));
    refused(&run, "an app on this profile");
    drop(own);
    // Nothing runs: the digest is taken, the list written, the app starts and answers.
    run.call(&allow).expect("taken");
    run.ready();
    run.stop();
    run.join().expect("ended");
    assert!(
        !rig.dir.allow_file().exists(),
        "the list went at the first answer"
    );
}

// ---- LOW-6: a list that stays -----------------------------------------------------------------------

/// The list cannot be removed at the app's first answer (here: a directory stands at its name, so
/// the unlink fails whoever runs this): the run halts `list-not-removed` and ends, fail closed.
#[test]
fn a_list_that_cannot_be_removed_at_the_first_answer_halts_the_run() {
    let rig = Rig::new("f-list-stays");
    std::fs::create_dir(rig.dir.allow_file()).expect("a directory where the list goes");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    let code = run
        .join_within(Duration::from_secs(60))
        .expect("the halt ended the run")
        .expect("ended");
    assert_eq!(code, 1);
    assert_eq!(halted_kind(&rig), json!("list-not-removed"));
    rig.assert_ended("a list that stays");
    assert!(rig.dir.allow_file().is_dir(), "nothing was deleted blindly");
}

// ---- LOW-7: the app's /proc files --------------------------------------------------------------------

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn sleeper(program: &Path) -> Reaped {
    Reaped(
        Command::new(program)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts"),
    )
}

fn the_sleep_program() -> PathBuf {
    ["/usr/bin/sleep", "/bin/sleep"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .expect("a sleep program")
}

/// wa-run's check, ported: a program this user started and did not hide shows both files; one the
/// kernel hides from its first instruction (an execute-only copy) shows none.
#[test]
fn the_apps_proc_files_are_looked_at_as_wa_run_looks() {
    let child = sleeper(&the_sleep_program());
    assert_eq!(
        readable_files(child.0.id(), &proc_file_readable),
        ["environ", "smaps_rollup"]
    );
    assert!(readable_files(child.0.id(), &|_| false).is_empty());
    assert_eq!(
        readable_files(child.0.id(), &|p| p.ends_with("environ")),
        ["environ"]
    );
    // The half only a machine that hides an execute-only program's environment can show.
    let dumpable = std::fs::read_to_string("/proc/sys/fs/suid_dumpable").unwrap_or_default();
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if dumpable.trim() == "1" || unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped the execute-only half: fs.suid_dumpable is 1 or this runs as root");
        return;
    }
    let scratch = super::Scratch::new("f-xo");
    let hidden = scratch.0.join("sleep");
    std::fs::copy(the_sleep_program(), &hidden).expect("copy");
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o100)).expect("mode");
    let child = sleeper(&hidden);
    assert!(
        readable_files(child.0.id(), &proc_file_readable).is_empty(),
        "an execute-only program is protected"
    );
}

/// An app whose files are readable halts the run `app-readable` in a real place (and `info` says so);
/// a rehearsal never asks.
#[test]
fn a_readable_app_halts_the_run_in_a_real_place_and_a_rehearsal_never_asks() {
    let rig = Rig::new("f-readable").exposing();
    let run = start_app(&rig, "silent-online", quick());
    let code = run
        .join_within(Duration::from_secs(60))
        .expect("the halt ended the run")
        .expect("ended");
    assert_eq!(code, 1);
    assert_eq!(halted_kind(&rig), json!("app-readable"));
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events.jsonl");
    assert!(
        events.contains("\"app_readable\"") && events.contains("environ"),
        "{events}"
    );
    rig.assert_ended("a readable app");

    let rig = Rig::new("f-readable-rehearsal").exposing().rehearsing();
    let mut run = start_app(&rig, "silent-online", quick());
    let info = run.ready();
    assert_eq!(info["app_readable"], json!(false), "{info}");
    assert!(info["halted"].is_null());
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
    // And the protected app of a real place is not halted: `info` says false.
    let rig = Rig::new("f-protected");
    let mut run = start_app(&rig, "silent-online", quick());
    let info = run.ready();
    assert_eq!(info["app_readable"], json!(false), "{info}");
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
}

// ---- LOW-8: the digest's child -----------------------------------------------------------------------

/// The `sha256sum` child gets no core file: the command the hash is made by has `RLIMIT_CORE` 0
/// before its exec, whatever the parent's limit is.
#[test]
fn the_hashing_child_has_no_core_limit() {
    let mut cmd = crate::threema_run::launch::no_core_command("sh");
    cmd.args(["-c", "ulimit -c"]).stdout(Stdio::piped());
    // SAFETY: both calls only read or write a plain struct of this process's limit; the soft limit is
    // raised to the hard one for the check and put back.
    let before = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_CORE, &raw mut limit), 0);
        let raised = libc::rlimit {
            rlim_cur: limit.rlim_max,
            rlim_max: limit.rlim_max,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_CORE, &raised), 0);
        limit
    };
    let out = cmd.output().expect("sh");
    // SAFETY: puts back what was read above.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &before);
    }
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

// ---- LOW-9: only the step asks for a removal ---------------------------------------------------------

fn cmdline(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in args {
        out.extend_from_slice(arg.as_bytes());
        out.push(0);
    }
    out
}

/// A removal asked tells the judge that 4113, `logged_out` and the drop frame are no halts: only a
/// process that is `<program> threema-run step <that word>` may say so. The table of command lines,
/// then a real client of another command line (a child process) over the real socket, then the
/// client in the test (the step runs in this process in a unit test).
#[test]
fn only_the_step_of_that_name_asks_for_a_removal() {
    let step = cmdline(&[
        "/x/xtask",
        "threema-run",
        "step",
        "unlink",
        "--profile",
        "p",
    ]);
    assert!(is_the_step_cmdline(&step, "unlink"));
    assert!(!is_the_step_cmdline(&step, "removed-on-phone"));
    assert!(is_the_step_cmdline(
        &cmdline(&["xtask", "threema-run", "step", "removed-on-phone"]),
        "removed-on-phone"
    ));
    for not in [
        cmdline(&["/x/xtask", "threema-run", "state", "unlink"]),
        cmdline(&["/x/xtask", "step", "unlink"]),
        cmdline(&["/x/xtask", "threema-run", "step"]),
        cmdline(&["sh", "-c", "threema-run step unlink"]),
        cmdline(&["threema-run", "step", "unlink"]),
        cmdline(&["/x/xtask", "threema-run", "steps", "unlink"]),
        cmdline(&["/x/xtask", "x", "threema-run", "step", "unlink"]),
        Vec::new(),
    ] {
        assert!(
            !is_the_step_cmdline(&not, "unlink"),
            "{:?}",
            String::from_utf8_lossy(&not)
        );
    }
    let rig = Rig::new("f-removal");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    // A process of another command line (the test binary again, at the raw writer) is refused, for
    // both operations, and nothing changes.
    for op in [
        json!({"op": "removal", "what": "unlink"}),
        json!({"op": "removal_over", "what": "unlink"}),
    ] {
        let line = raw_op(&rig, &op, "raw-op", None);
        assert!(line.contains("no-step"), "{line}");
    }
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    // The step's own client (this process in a unit test) sets it, and gives it back.
    run.call(&json!({"op": "removal", "what": "unlink"}))
        .expect("asked");
    assert_eq!(run.info().expect("info")["removal"], json!(true));
    let line = raw_op(
        &rig,
        &json!({"op": "removal_over", "what": "unlink"}),
        "raw-op",
        None,
    );
    assert!(line.contains("no-step"), "{line}");
    assert_eq!(
        run.info().expect("info")["removal"],
        json!(true),
        "a stranger could not give it back"
    );
    run.call(&json!({"op": "removal_over", "what": "unlink"}))
        .expect("given back");
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    for what in ["", "nonsense"] {
        let e = run
            .call(&json!({"op": "removal_over", "what": what}))
            .expect_err("refused");
        assert!(format!("{e:#}").contains("badRequest"), "{e:#}");
    }
    run.stop();
    run.join().expect("ended");
}

// ---- LOW-1 once more: the digest's own pipe ---------------------------------------------------------

/// The ID's shape and the digest never appear in a name the cleanups report: the words of the
/// leftover check name no path and no value.
#[test]
fn the_cleanups_words_hold_no_path_and_no_value() {
    let w = World::new("f-words");
    let dir = profile_of(&w);
    std::fs::write(dir.allow_tmp(), list_text(&digest_shape())).expect("a temporary list");
    let words = remove_leftover_list(&dir).expect("removed").join("\n");
    assert!(!words.contains(&digest_shape()) && !words.contains(&id_shape()));
    assert!(
        !words.contains(w.scratch.0.to_str().expect("utf-8")),
        "{words}"
    );
    // A probe of the keeper lock afterwards: the check let go of it.
    assert_eq!(
        probe_lock(&dir.keeper_lock_file(), Duration::ZERO).expect("a probe"),
        Lock::Free
    );
}
