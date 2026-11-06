//! T4: `id`, `answer` and the list of test IDs' whole life. The prompts run as THEIR PRODUCTION CODE in
//! a child process (this test binary again, at `child_entry`) on a pseudo-terminal the test plays
//! (wa_run.rs's pty tests); the supervisor is the real one on the stand-in app of `supervise.rs`'s rig.
//! Nothing contacts a host, nothing is shown or heard, and the typed IDs are synthetic, built from
//! parts.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use super::supervise::{Rig, quick};
use super::{digest_shape, id_shape, mode_of};
use crate::threema_run::launch::{QUESTION_KIND, open_question};
use crate::threema_run::preflight::{Lock, probe_lock};
use crate::threema_run::profile::{ProfileDir, State, open_lock_file};
use crate::threema_run::prompt::{
    LINKING_KIND, PROMPT_AGAIN, PROMPT_FIRST, digest_of_id, foreground_of_a_terminal,
    is_digest_hex, list_text, question_kind, type_answer, type_id, valid_id,
};
use crate::threema_run::supervise::{LIST_AT_END, LIST_TAKEN};

// ---- The children: the production prompts on a terminal of their own ----------------------------------

const CHILD_MODE: &str = "THREEMA_RUN_TEST_CHILD";
const CHILD_PROFILE: &str = "THREEMA_RUN_TEST_PROFILE";
const CHILD_SOCKET: &str = "THREEMA_RUN_TEST_SOCKET";

fn child_args() -> Vec<String> {
    let module = module_path!();
    let name = module.split_once("::").map_or(module, |(_, rest)| rest);
    vec![
        "--exact".to_owned(),
        format!("{name}::child_entry"),
        "--nocapture".to_owned(),
        "--test-threads=1".to_owned(),
    ]
}

/// The hidden entry the tests below run in a child process: without `THREEMA_RUN_TEST_CHILD` it does
/// nothing and passes.
#[test]
fn child_entry() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let dir = ProfileDir {
        dir: PathBuf::from(std::env::var(CHILD_PROFILE).expect("a profile")),
        rehearsal: false,
    };
    let socket = PathBuf::from(std::env::var(CHILD_SOCKET).expect("a socket"));
    let result = match mode.as_str() {
        "id" => type_id(&dir, &socket),
        "answer" => type_answer(&dir, Some(&socket)),
        // A process that is not `threema-run` at all writes an operation to the socket (the one in
        // `op.json` beside the profile).
        "raw-op" => {
            let op = std::fs::read_to_string(dir.dir.with_file_name("op.json")).expect("the op");
            let mut stream = UnixStream::connect(&socket).expect("connect");
            writeln!(stream, "{}", op.trim()).expect("write the op");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("the answer");
            Ok(line)
        }
        background => {
            let mode = background
                .strip_suffix("-in-a-background-group")
                .expect("a known mode");
            let out = Command::new(std::env::current_exe().expect("test binary"))
                .args(child_args())
                .env(CHILD_MODE, mode)
                .process_group(0)
                .output()
                .expect("the background child");
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            print!("{}", String::from_utf8_lossy(&out.stdout));
            std::process::exit(out.status.code().unwrap_or(4));
        }
    };
    match result {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(3);
        }
    }
}

/// A child process that is killed and reaped when the test ends, whatever way it ends.
struct Spawned(Option<std::process::Child>);

impl Spawned {
    /// Waits for the child to end (killing it after `secs`) and returns what it printed.
    fn finish(&mut self, secs: u64) -> std::process::Output {
        let mut child = self.0.take().expect("a running child");
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while child.try_wait().expect("wait").is_none() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().expect("output")
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A pseudo-terminal: the test is the terminal emulator (the master), the child's terminal is the
/// slave.
pub(super) struct Pty {
    master: std::fs::File,
    slave: std::fs::File,
}

fn open_pty() -> Pty {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: `openpty` fills two descriptors; the null pointers ask for no name, termios or size.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    // SAFETY: both descriptors are new and owned here; the master must not leak into children.
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        Pty {
            master: std::fs::File::from_raw_fd(master),
            slave: std::fs::File::from_raw_fd(slave),
        }
    }
}

/// Starts the test binary at `child_entry` in `mode` for the rig's profile and socket. With a pty
/// the child is a session leader whose controlling terminal is its slave; without one it is a
/// session leader with no terminal at all.
fn spawn_child(mode: &str, rig: &Rig, pty: Option<&Pty>) -> Spawned {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.args(child_args())
        .env(CHILD_MODE, mode)
        .env(CHILD_PROFILE, &rig.dir.dir)
        .env(CHILD_SOCKET, rig.socket())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let slave = pty.map(|p| p.slave.as_raw_fd());
    // SAFETY: only `setsid` and `ioctl` run between fork and exec, both async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(fd) = slave
                && libc::ioctl(fd, libc::TIOCSCTTY, 0) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Spawned(Some(cmd.spawn().expect("spawn the child")))
}

/// Reads what the child shows on the terminal until `marker` has appeared (or `secs` pass).
fn read_until(master: &std::fs::File, seen: &mut Vec<u8>, marker: &str, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if String::from_utf8_lossy(seen).contains(marker) {
            return true;
        }
        read_some(master, seen, 200);
    }
    String::from_utf8_lossy(seen).contains(marker)
}

fn read_some(master: &std::fs::File, seen: &mut Vec<u8>, wait_ms: i32) {
    let mut poll = libc::pollfd {
        fd: master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`.
    if unsafe { libc::poll(&mut poll, 1, wait_ms) } > 0 && poll.revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is valid for its length.
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if let Ok(n) = usize::try_from(n) {
            seen.extend_from_slice(&buf[..n]);
        }
    }
}

/// What is left on the terminal after the child ended.
fn drain(master: &std::fs::File, seen: &mut Vec<u8>) {
    for _ in 0..5 {
        read_some(master, seen, 100);
    }
}

fn type_into(pty: &Pty, bytes: &[u8]) {
    (&pty.master).write_all(bytes).expect("type");
}

/// The terminal's local modes, as the slave has them now.
fn lflag_of(pty: &Pty) -> libc::tcflag_t {
    // SAFETY: an all-zero termios is a valid buffer for `tcgetattr` to fill.
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: the master is open; `attrs` is valid.
    assert_eq!(
        unsafe { libc::tcgetattr(pty.master.as_raw_fd(), &mut attrs) },
        0
    );
    attrs.c_lflag
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Another synthetic ID, built from parts.
fn other_id() -> String {
    ["SYN", "TH", "T02"].concat()
}

/// What a finished child printed, and whether any stream or the screen holds a value it must not.
struct Seen {
    ok: bool,
    stdout: String,
    stderr: String,
    screen: String,
}

impl Seen {
    fn holds(&self, value: &str) -> bool {
        [&self.stdout, &self.stderr, &self.screen]
            .iter()
            .any(|t| t.contains(value) || t.contains(&value.to_ascii_lowercase()))
    }
}

/// `id` on a terminal of its own: the entries typed after their prompts (one entry when the child
/// refuses at once), what it showed.
fn run_id(rig: &Rig, entries: &[&str]) -> Seen {
    let pty = open_pty();
    let mut child = spawn_child("id", rig, Some(&pty));
    let mut screen = Vec::new();
    let prompts = [PROMPT_FIRST, PROMPT_AGAIN];
    for (entry, prompt) in entries.iter().zip(prompts) {
        assert!(
            read_until(&pty.master, &mut screen, prompt, 60),
            "no prompt: {:?}",
            text(&screen)
        );
        type_into(&pty, format!("{entry}\n").as_bytes());
    }
    let out = child.finish(60);
    drain(&pty.master, &mut screen);
    Seen {
        ok: out.status.success(),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        screen: text(&screen),
    }
}

/// `answer` on a terminal of its own: the key typed after the question (none: the child refuses
/// before it asks), what it showed.
fn run_answer(rig: &Rig, key: Option<u8>) -> Seen {
    let pty = open_pty();
    let mut child = spawn_child("answer", rig, Some(&pty));
    let mut screen = Vec::new();
    if let Some(key) = key {
        assert!(
            read_until(&pty.master, &mut screen, "[y/n]: ", 60),
            "no question: {:?}",
            text(&screen)
        );
        type_into(&pty, &[key]);
    }
    let out = child.finish(60);
    drain(&pty.master, &mut screen);
    Seen {
        ok: out.status.success(),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        screen: text(&screen),
    }
}

/// A process that is not on a terminal writes `op` to the socket; its one answer line.
pub(super) fn raw_op(rig: &Rig, op: &Value, mode: &str, pty: Option<&Pty>) -> String {
    std::fs::write(rig.dir.dir.with_file_name("op.json"), op.to_string()).expect("the op");
    let mut child = spawn_child(mode, rig, pty);
    let out = child.finish(60);
    text(&out.stdout)
}

/// The keeper lock as another process would see it. A lock that looks held is asked again for a
/// moment: a program another thread is about to `exec` carries a copy of the descriptor
/// (`.claude/rules/testing.md`), so only "held" is read at once.
fn keeper_lock(rig: &Rig) -> Lock {
    let path = rig.dir.keeper_lock_file();
    match probe_lock(&path, Duration::ZERO).expect("a probe") {
        Lock::Free => Lock::Free,
        _ => probe_lock(&path, Duration::from_secs(2)).expect("a probe"),
    }
}

// ---- The digest --------------------------------------------------------------------------------------

/// The digest `id` sends is the one `kourier-threema`'s `allow.rs` tests (its own vector, read from
/// that file: the ID is the one in its `printf` line, the digest the constant below it), and the line
/// the tool writes is the line the app's reader parses (its test constant of that very digest).
#[test]
fn the_digest_is_allow_rs_own_test_vector_and_the_line_the_app_reads() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the workspace");
    let allow = std::fs::read_to_string(root.join("crates/kourier-threema/src/allow.rs"))
        .expect("allow.rs");
    let (_, after) = allow
        .split_once("printf 'kourier-threema-allow-v1:")
        .expect("the vector's printf line");
    let id = after.split('\'').next().expect("the vector's ID");
    let hex = after
        .split('"')
        .find(|part| part.len() == 64 && part.bytes().all(|b| b.is_ascii_hexdigit()))
        .expect("the vector's digest");
    assert!(
        valid_id(id.as_bytes()),
        "the vector is an ID of the alphabet"
    );
    let digest = digest_of_id(id.as_bytes()).expect("hashed");
    assert_eq!(digest, hex, "the digest of allow.rs's own vector");
    assert!(is_digest_hex(&digest));
    let line = list_text(&digest);
    assert_eq!(line, format!("sha256:{hex}\n"));
    let app = std::fs::read_to_string(root.join("crates/kourier-app/src/threema.rs"))
        .expect("the app's reader");
    assert!(
        app.contains(&format!("\"{}\"", line.trim_end())),
        "the app's reader tests this very line"
    );
    assert!(app.contains("const LINE_PREFIX: &str = \"sha256:\";"));
    // Another ID, another digest; the case of an ID is part of it, so the prompt upper-cases.
    let other = digest_of_id(other_id().as_bytes()).expect("hashed");
    assert_ne!(other, digest);
    assert!(digest_of_id(id.to_ascii_lowercase().as_bytes()).is_err());
}

#[test]
fn an_id_is_eight_of_the_alphabet_and_a_digest_64_lower_hex_digits() {
    let id = id_shape();
    assert!(valid_id(id.as_bytes()));
    for bad in [
        id[..7].to_owned(),
        format!("{id}X"),
        format!("*{}", &id[1..]),
        id.to_ascii_lowercase(),
        format!("{}-", &id[..7]),
        String::new(),
    ] {
        assert!(!valid_id(bad.as_bytes()), "taken: {} characters", bad.len());
    }
    assert!(is_digest_hex(&digest_shape()));
    for bad in [
        digest_shape().to_ascii_uppercase(),
        digest_shape()[1..].to_owned(),
        format!("{}0", digest_shape()),
        format!("sha256:{}", digest_shape()),
    ] {
        assert!(!is_digest_hex(&bad));
    }
    // The terminal rule's reading of `/proc/<pid>/stat` (a name with spaces and a bracket).
    assert!(foreground_of_a_terminal("77 (x) y) S 1 77 77 34817 77 0 0"));
    assert!(!foreground_of_a_terminal("77 (x) S 1 77 77 0 -1 0"));
    assert!(!foreground_of_a_terminal("77 (x) S 1 77 77 34817 80 0"));
    assert!(!foreground_of_a_terminal("garbage"));
}

// ---- `id` and the list ------------------------------------------------------------------------------------

/// `id` names the TEST ID, asks twice with the echo off, takes the entry in either case, prints only
/// `taken (8 characters)`, puts the terminal back; the supervisor writes ONLY the digest, in the app's
/// form, 0600 in the 0700 keeper root, and holds the keeper lock while the list stands; the list and
/// the lock go at the end of a run whose app never answered.
#[test]
fn id_asks_twice_shows_nothing_and_the_list_is_the_digest_under_the_keeper_lock() {
    let rig = Rig::new("p-id");
    let mut run = rig.run("silent", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let id = id_shape();
    let pty = open_pty();
    let before = lflag_of(&pty);
    let modes = libc::ECHO | libc::ICANON | libc::ISIG;
    assert_eq!(before & modes, modes, "a fresh terminal has echo on");
    let mut child = spawn_child("id", &rig, Some(&pty));
    let mut screen = Vec::new();
    assert!(read_until(&pty.master, &mut screen, PROMPT_FIRST, 60));
    assert!(
        text(&screen).contains("TEST ID") && text(&screen).contains("Never your personal ID"),
        "the prompt names the TEST ID: {:?}",
        text(&screen)
    );
    assert_eq!(lflag_of(&pty) & modes, 0, "nothing is shown while he types");
    type_into(&pty, format!(" {} \n", id.to_ascii_lowercase()).as_bytes());
    assert!(read_until(&pty.master, &mut screen, PROMPT_AGAIN, 60));
    type_into(&pty, format!("{id}\n").as_bytes());
    let out = child.finish(60);
    drain(&pty.master, &mut screen);
    let seen = Seen {
        ok: out.status.success(),
        stdout: text(&out.stdout),
        stderr: text(&out.stderr),
        screen: text(&screen),
    };
    assert!(seen.ok, "{} {}", seen.stdout, seen.stderr);
    assert!(
        seen.stdout.contains("taken (8 characters)"),
        "{}",
        seen.stdout
    );
    let digest = digest_of_id(id.as_bytes()).expect("hashed");
    assert!(!seen.holds(&id), "a stream or the screen holds the ID");
    assert!(
        !seen.holds(&digest),
        "a stream or the screen holds the digest"
    );
    assert_eq!(lflag_of(&pty), before, "the terminal is back as it was");
    // The list: the digest alone, the app's form, private, under the keeper lock.
    let list = rig.dir.allow_file();
    assert_eq!(
        std::fs::read_to_string(&list).expect("the list"),
        list_text(&digest)
    );
    assert_eq!(mode_of(&list), 0o600);
    assert_eq!(mode_of(rig.dir.keeper()), 0o700);
    assert_eq!(
        keeper_lock(&rig),
        Lock::Held,
        "the keeper lock while it stands"
    );
    let info = run.info().expect("info");
    assert_eq!(info["id"], json!("taken"));
    assert_eq!(info["allow_list"], json!("standing"));
    // Nothing of it in the tool's own files, and no other file in the keeper root holds it.
    assert!(!rig.written_text().contains(&id) && !rig.written_text().contains(&digest));
    for entry in std::fs::read_dir(rig.dir.keeper()).expect("the keeper root") {
        let path = entry.expect("an entry").path();
        if path != list && path.is_file() {
            let bytes = std::fs::read(&path).expect("a file");
            assert!(!text(&bytes).contains(&digest), "{}", path.display());
        }
    }
    // A second `id` in the same start is refused before it asks.
    let again = run_id(&rig, &[]);
    assert!(
        !again.ok && again.stderr.contains("does not wait for the ID"),
        "{}",
        again.stderr
    );
    // The app never answers; the run ends: the list and the keeper lock go with it.
    run.stop();
    run.join().expect("ended");
    assert!(!list.exists(), "the list outlived the run");
    assert_eq!(keeper_lock(&rig), Lock::Free);
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(events.contains(LIST_AT_END), "{events}");
    rig.assert_ended("a link start whose app never answered");
}

/// THE FILE'S LIFE (the mutant's test): the list goes at the app's FIRST dev-control answer, not at
/// the run's end: once the supervisor says the app answered, the file is gone and the keeper lock is
/// free, while the app still runs.
#[test]
fn the_list_goes_at_the_apps_first_answer_and_the_keeper_lock_with_it() {
    let rig = Rig::new("p-life");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let id = id_shape();
    let seen = run_id(&rig, &[&id, &id]);
    assert!(seen.ok, "{} {}", seen.stdout, seen.stderr);
    let info = run.wait_for("the app's first answer", |i| i["answered"] == json!(true));
    assert!(
        !rig.dir.allow_file().exists(),
        "the list stands after the app's first answer"
    );
    assert_eq!(info["allow_list"], json!(LIST_TAKEN));
    assert_eq!(
        keeper_lock(&rig),
        Lock::Free,
        "the keeper lock outlived the list"
    );
    assert!(
        info["app_pid"].is_u64() && info["exit"].is_null(),
        "the app still runs"
    );
    run.stop();
    run.join().expect("ended");
    rig.assert_ended("a link start");
}

/// A refused entry sends nothing, says that the command takes no value, and shows nothing of what
/// was typed: two different entries, seven characters, nine, a Gateway ID's `*` and another key.
#[test]
fn id_refuses_a_mismatch_a_bad_shape_and_another_key_and_takes_nothing() {
    let rig = Rig::new("p-bad");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let (id, other) = (id_shape(), other_id());
    let cases: [(Vec<String>, &str); 5] = [
        (vec![id.clone(), other.clone()], "differ"),
        (vec![id[..7].to_owned()], "exactly 8"),
        (vec![format!("{id}X")], "exactly 8"),
        (vec![format!("*{}", &id[1..])], "Gateway ID"),
        (
            vec![format!("{}-{}", &id[..4], &id[4..])],
            "only letters and digits",
        ),
    ];
    for (entries, words) in cases {
        let entries: Vec<&str> = entries.iter().map(String::as_str).collect();
        let seen = run_id(&rig, &entries);
        assert!(!seen.ok, "taken: {words}");
        assert!(
            seen.stderr.contains(words) && seen.stderr.contains("takes no value"),
            "{words}: {}",
            seen.stderr
        );
        for entry in &entries {
            assert!(!seen.holds(entry), "{words}: the entry was shown");
        }
    }
    assert!(!rig.dir.allow_file().exists(), "a list was written");
    let info = run.info().expect("info");
    assert_eq!(info["id"], json!("waiting"), "the start still waits");
    assert_eq!(info["allow_list"], json!("none"));
    run.stop();
    run.join().expect("ended");
    rig.assert_ended("refused entries");
}

/// `id` asks nothing without a terminal, from a background group of one, while the question of a
/// failed link stands in the keeper root, and when the start does not wait for the ID.
#[test]
fn id_refuses_before_it_asks_without_a_foreground_terminal_a_waiting_start_or_with_a_question() {
    let rig = Rig::new("p-ref");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let out = spawn_child("id", &rig, None).finish(60);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("terminal of its own"),
        "{}",
        text(&out.stderr)
    );
    let pty = open_pty();
    let out = spawn_child("id-in-a-background-group", &rig, Some(&pty)).finish(60);
    let mut screen = Vec::new();
    drain(&pty.master, &mut screen);
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("background process group"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        !text(&screen).contains("TEST ID"),
        "it asked from a background group"
    );
    open_question(&rig.dir, 1).expect("a question");
    let seen = run_id(&rig, &[]);
    assert!(
        !seen.ok && seen.stderr.contains("question of a failed link is open"),
        "{}",
        seen.stderr
    );
    assert!(
        !seen.screen.contains("TEST ID"),
        "it asked while the question stands"
    );
    std::fs::remove_file(rig.dir.open_question_file()).expect("the question goes");
    assert_eq!(run.info().expect("info")["id"], json!("waiting"));
    run.stop();
    run.join().expect("ended");
    // A start that is not for a link takes no ID.
    let rig = Rig::new("p-ref2");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    let seen = run_id(&rig, &[]);
    assert!(
        !seen.ok && seen.stderr.contains("does not wait for the ID"),
        "{}",
        seen.stderr
    );
    run.stop();
    run.join().expect("ended");
    assert!(!rig.dir.allow_file().exists());
}

/// Whoever else can write to the socket: the digest and an answer are taken only from a process in
/// the FOREGROUND of a terminal (no terminal, or a background group of one, is refused) and the
/// refusal echoes nothing.
#[test]
fn the_supervisor_takes_a_digest_or_an_answer_only_from_a_foreground_terminal() {
    let rig = Rig::new("p-term");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "ask", "name": "test-phone-in-hand"}))
        .expect("asked");
    let allow = json!({"op": "allow", "digest": digest_shape()});
    let answer = json!({"op": "answer", "name": "test-phone-in-hand", "yes": true});
    for op in [&allow, &answer] {
        let line = raw_op(&rig, op, "raw-op", None);
        assert!(line.contains("no-terminal"), "{line}");
        assert!(!line.contains(&digest_shape()), "echoed: {line}");
        let pty = open_pty();
        let line = raw_op(&rig, op, "raw-op-in-a-background-group", Some(&pty));
        assert!(line.contains("no-terminal"), "{line}");
    }
    assert!(!rig.dir.allow_file().exists(), "a list was written");
    let info = run.info().expect("info");
    assert_eq!(info["id"], json!("waiting"));
    assert_eq!(
        info["question"],
        json!("test-phone-in-hand"),
        "the question stays open"
    );
    run.stop();
    run.join().expect("ended");
}

// ---- `answer` ------------------------------------------------------------------------------------------

/// A step's question takes ONE key, y or n, from his terminal; any other key answers nothing. A
/// step cannot put the question of a failed link (that comes from the keeper root only).
#[test]
fn answer_takes_one_key_for_a_steps_question() {
    let rig = Rig::new("p-ans");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    for bad in [QUESTION_KIND, "no-such-question"] {
        let error = run
            .call(&json!({"op": "ask", "name": bad}))
            .expect_err("refused");
        assert!(format!("{error:#}").contains("badRequest"), "{error:#}");
    }
    let ask = || {
        run.call(&json!({"op": "ask", "name": "test-phone-in-hand"}))
            .expect("asked");
    };
    let answer_of = || {
        run.call(&json!({"op": "answer_of", "name": "test-phone-in-hand"}))
            .expect("answer_of")["answer"]
            .clone()
    };
    ask();
    let seen = run_answer(&rig, Some(b'y'));
    assert!(seen.ok, "{}", seen.stderr);
    assert!(seen.screen.contains("TEST phone"), "{}", seen.screen);
    assert!(seen.stdout.contains("taken (yes)"), "{}", seen.stdout);
    assert_eq!(answer_of(), json!(true));
    ask();
    assert_eq!(answer_of(), Value::Null, "a new question has no answer yet");
    let seen = run_answer(&rig, Some(b'x'));
    assert!(
        !seen.ok && seen.stderr.contains("only y or n"),
        "{}",
        seen.stderr
    );
    assert_eq!(answer_of(), Value::Null);
    let seen = run_answer(&rig, Some(b'N'));
    assert!(
        seen.ok && seen.stdout.contains("taken (no)"),
        "{}",
        seen.stderr
    );
    assert_eq!(answer_of(), json!(false));
    let seen = run_answer(&rig, None);
    assert!(
        !seen.ok && seen.stderr.contains("no question is open"),
        "{}",
        seen.stderr
    );
    run.stop();
    run.join().expect("ended");
}

/// The question of a failed link with NO supervisor (a start that died before its supervisor came
/// up): `answer` takes the profile's lock before it asks, spends `link_at`, records the answer and
/// removes the keeper root's file; a supervisor that holds the lock and does not answer is refused
/// before anything is asked.
#[test]
fn answer_closes_the_open_question_alone_and_spends_link_at() {
    let rig = Rig::new("p-alone");
    let mut state = State::load(&rig.dir).expect("readable").expect("a state");
    state.set("link_at", json!(5));
    state.save(&rig.dir).expect("saved");
    open_question(&rig.dir, 7).expect("a question");
    let seen = run_answer(&rig, Some(b'n'));
    assert!(seen.ok, "{}", seen.stderr);
    assert!(
        seen.screen.contains("linked devices") && seen.screen.contains("never opens"),
        "{}",
        seen.screen
    );
    assert!(seen.stdout.contains("taken (no)"), "{}", seen.stdout);
    assert!(
        !rig.dir.open_question_file().exists(),
        "the question stands"
    );
    let state = rig.state();
    assert_eq!(state["link_at"], Value::Null);
    assert_eq!(state["last_link_question"]["answer"], json!("no"));
    // A supervisor that holds the profile and does not answer: nothing is asked, the question stays.
    open_question(&rig.dir, 8).expect("a question");
    let lock = open_lock_file(&rig.dir.lock_file()).expect("the lock file");
    lock.lock().expect("held");
    let seen = run_answer(&rig, None);
    assert!(
        !seen.ok && seen.stderr.contains("does not answer"),
        "{}",
        seen.stderr
    );
    assert!(!seen.screen.contains("[y/n]"), "it asked");
    assert!(rig.dir.open_question_file().exists());
    drop(lock);
}

/// T3a's flow: a start that found `link_at` opened the question and went on; its supervisor waits
/// for the ID, `id` is refused until `answer` closes the question through the supervisor (the
/// state records it), then `id` is taken and the list lives to the app's first answer.
#[test]
fn answer_closes_the_question_through_a_live_supervisor_and_id_follows() {
    let rig = Rig::new("p-live");
    open_question(&rig.dir, 1).expect("the start's question");
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let seen = run_id(&rig, &[]);
    assert!(
        !seen.ok && seen.stderr.contains("question"),
        "{}",
        seen.stderr
    );
    let seen = run_answer(&rig, Some(b'y'));
    assert!(seen.ok, "{}", seen.stderr);
    assert!(seen.stdout.contains("taken (yes)"), "{}", seen.stdout);
    assert!(!rig.dir.open_question_file().exists());
    assert_eq!(rig.state()["last_link_question"]["answer"], json!("yes"));
    let id = id_shape();
    let seen = run_id(&rig, &[&id, &id]);
    assert!(seen.ok, "{}", seen.stderr);
    let info = run.wait_for("the app's first answer", |i| i["answered"] == json!(true));
    assert_eq!(info["allow_list"], json!(LIST_TAKEN));
    assert!(!rig.dir.allow_file().exists());
    run.stop();
    run.join().expect("ended");
    rig.assert_ended("a link after the question");
}

/// The keeper root keeps `linking` (critic M4): written when the login reaches it, and while it
/// stands it is this run's own mark (not a question `answer` would take); a run that ends with the
/// login at `linking` leaves the question; `done` and a refused ID take it away.
#[test]
fn linking_is_kept_in_the_keeper_root_until_its_end_is_seen() {
    let rig = Rig::new("p-lk");
    let mut run = rig.run("link-wait", false, quick());
    run.wait_for("linking", |i| i["login"]["step"] == json!("linking"));
    assert_eq!(question_kind(&rig.dir).as_deref(), Some(LINKING_KIND));
    let seen = run_answer(&rig, None);
    assert!(
        !seen.ok && seen.stderr.contains("no question is open"),
        "{}",
        seen.stderr
    );
    run.stop();
    run.join().expect("ended");
    assert_eq!(question_kind(&rig.dir).as_deref(), Some(QUESTION_KIND));
    assert!(rig.state()["link_at"].is_u64());
    for case in ["link", "link-refused"] {
        let rig = Rig::new(&format!("p-lk-{case}"));
        let mut run = rig.run(case, false, quick());
        if case == "link" {
            run.wait_for("the login's end", |i| {
                i["login"]["step"] == json!("done") && i["link_at"] == json!(false)
            });
            run.stop();
        }
        // A refused ID is S1a (T5): the run halts and ends by itself.
        run.join().expect("ended");
        assert_eq!(question_kind(&rig.dir), None, "{case}");
    }
}
