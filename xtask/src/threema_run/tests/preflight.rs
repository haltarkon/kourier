//! The start's checks (§2.6 with the critic's changes): every refusal made on its own, with its own
//! text, over a world of canned answers and scratch places; the order of the checks; and that the
//! preflight makes nothing. No mutant: these are checks of the run, not the account's gate.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::json;

use super::{Scratch, args, error_of};
use crate::threema_run::lines::Level;
use crate::threema_run::preflight::{
    Cleared, Clock, Ctx, Git, LINES, LineRow, Lock, MAXERROR_LIMIT_US, MIGRATION_DIRS,
    SOCKET_NAME_LEN, SOCKET_PATH_MAX, TEST_ENV, TLS_MINIMUMS, at_least, check_clock, check_halt,
    check_lines, check_migrations, check_rehearsal_offline, check_slice, check_socket_place,
    check_source, check_tls, plan_switch, preflight, probe_lock, profiles_of, read_clock,
    switch_text, tracked_migrations, utc, with_real_ctx,
};
use crate::threema_run::profile::{ProfileDir, State, private_dir};
use crate::threema_run::{Args, KEEPER_DIR, KEEPER_LOCK, OPEN_QUESTION_FILE};

/// A list of `(name, version)` or `(file, blob)` pairs of a table row.
type Entries<'a> = &'a [(&'a str, &'a str)];

pub(super) const HEAD: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
pub(super) const SLICE: &str =
    "0::/user.slice/user-1000.slice/user@1000.service/kourier.slice/run-r1.scope\n";
pub(super) const NO_SLICE: &str = "0::/user.slice/user-1000.slice/session-3.scope\n";
pub(super) const LO_ONLY: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n";
const WITH_ETH: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n  eth0:       5       1    0    0    0     0          0         0        5       1    0    0    0     0       0          0\n";

/// A synchronized clock: `TIME_OK`, no `STA_UNSYNC`, an error estimate of 1 ms.
fn synced() -> Clock {
    Clock {
        state: 0,
        status: 0,
        maxerror_us: 1_000,
    }
}

/// A `Cargo.lock` text with these packages (nothing else of the real file's shape is read).
fn lock_with(entries: &[(&str, &str)]) -> String {
    entries
        .iter()
        .map(|(n, v)| format!("[[package]]\nname = \"{n}\"\nversion = \"{v}\"\n\n"))
        .collect()
}

/// A lock with the bump of main and a few other crates, and an OLD `webpki-root-certs` (not read).
fn good_lock() -> String {
    lock_with(&[
        ("ring", "0.17.14"),
        ("rustls", "0.23.46"),
        ("rustls-webpki", "0.103.16"),
        ("webpki-root-certs", "1.0.9"),
        ("webpki-roots", "1.0.11"),
    ])
}

/// What HEAD's source says for the lines of `table`: one declaration or one literal each.
fn source_of(table: &[LineRow]) -> BTreeMap<String, String> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for row in table {
        let text = files.entry(row.file.to_owned()).or_default();
        match row.name {
            Some(name) => text.push_str(&format!(
                "/// doc\npub(crate) const {name}: &str = \"{}\";\n",
                row.text
            )),
            None => text.push_str(&format!(
                "    tracing::warn!(%problem, \"{}\");\n",
                row.text
            )),
        }
    }
    files
}

/// A git runner with canned answers; it counts its calls.
pub(super) struct FakeGit {
    pub(super) head: String,
    pub(super) main: String,
    pub(super) status: String,
    /// `git show HEAD:<path>`.
    pub(super) files: BTreeMap<String, String>,
    /// `git ls-tree -r HEAD -- <dir>`: the migration files by directory, `(name, blob)`.
    migrations: BTreeMap<String, Vec<(String, String)>>,
    worktrees: Vec<PathBuf>,
    pub(super) calls: Cell<u32>,
}

impl FakeGit {
    fn good() -> Self {
        let mut files = source_of(LINES);
        files.insert("Cargo.lock".to_owned(), good_lock());
        let migrations = MIGRATION_DIRS
            .iter()
            .map(|(_, dir)| {
                (
                    (*dir).to_owned(),
                    vec![
                        ("0001_init.sql".to_owned(), "b1".to_owned()),
                        ("0002_more.sql".to_owned(), "b2".to_owned()),
                    ],
                )
            })
            .collect();
        Self {
            head: HEAD.to_owned(),
            main: HEAD.to_owned(),
            status: String::new(),
            files,
            migrations,
            worktrees: vec![PathBuf::from("/fake/repo"), PathBuf::from("/fake/other")],
            calls: Cell::new(0),
        }
    }
}

impl Git for FakeGit {
    fn run(&self, args: &[&str]) -> Result<String> {
        self.calls.set(self.calls.get() + 1);
        match args {
            ["rev-parse", "HEAD"] => Ok(format!("{}\n", self.head)),
            ["rev-parse", "refs/heads/main"] => Ok(format!("{}\n", self.main)),
            ["status", "--porcelain=v1", "-uall"] => Ok(self.status.clone()),
            ["worktree", "list", "--porcelain"] => Ok(self
                .worktrees
                .iter()
                .map(|w| {
                    format!(
                        "worktree {}\nHEAD abc\nbranch refs/heads/x\n\n",
                        w.display()
                    )
                })
                .collect()),
            ["ls-tree", "-r", "HEAD", "--", dir] => {
                let Some(files) = self.migrations.get(*dir) else {
                    return Ok(String::new());
                };
                let mut out = format!("100644 blob c0ffee\t{dir}/README.md\n");
                for (name, blob) in files {
                    out.push_str(&format!("100644 blob {blob}\t{dir}/{name}\n"));
                }
                Ok(out)
            }
            ["show", spec] => match spec.strip_prefix("HEAD:").and_then(|p| self.files.get(p)) {
                Some(text) => Ok(text.clone()),
                None => bail!("fatal: path does not exist in HEAD"),
            },
            other => bail!("unexpected git call {other:?}"),
        }
    }
}

pub(super) fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Everything a start reads, below a scratch directory: the places, the environment, git, the
/// cgroup, the disk, the clock, the locks.
pub(super) struct World {
    pub(super) scratch: Scratch,
    pub(super) git: FakeGit,
    pub(super) cgroup: String,
    pub(super) disk: Option<String>,
    /// Environment overrides: `None` takes a variable out.
    pub(super) overrides: Vec<(&'static str, Option<String>)>,
    pub(super) busy: bool,
    pub(super) net_dev: String,
    pub(super) uid: u32,
    pub(super) clock: Option<Clock>,
    /// The lock files a process holds.
    pub(super) held: Vec<PathBuf>,
    /// Every probe fails.
    pub(super) lock_error: bool,
    pub(super) root: PathBuf,
}

impl World {
    pub(super) fn new(name: &str) -> Self {
        let scratch = Scratch::new(name);
        let root = scratch.0.join("repo");
        let w = Self {
            scratch,
            git: FakeGit::good(),
            cgroup: SLICE.to_owned(),
            disk: Some("dm-crypt".to_owned()),
            overrides: Vec::new(),
            busy: false,
            net_dev: LO_ONLY.to_owned(),
            uid: current_uid(),
            clock: Some(synced()),
            held: Vec::new(),
            lock_error: false,
            root,
        };
        private_dir(&w.keeper()).expect("the keeper root");
        private_dir(&w.run_dir()).expect("the runtime directory");
        w
    }

    pub(super) fn data(&self) -> PathBuf {
        self.scratch.0.join("data")
    }

    pub(super) fn keeper(&self) -> PathBuf {
        self.data().join(KEEPER_DIR)
    }

    pub(super) fn run_dir(&self) -> PathBuf {
        self.scratch.0.join("r")
    }

    pub(super) fn target(&self) -> PathBuf {
        self.scratch.0.join("target")
    }

    pub(super) fn dir(&self, name: &str) -> ProfileDir {
        ProfileDir {
            dir: self.keeper().join(name),
            rehearsal: false,
        }
    }

    /// The profile `p` of the tests' command lines.
    pub(super) fn profile(&self) -> ProfileDir {
        self.dir("p")
    }

    /// Writes the state file of `p` (made 0700 on the way), after `f` changed a fresh one.
    pub(super) fn save_state(&self, f: impl FnOnce(&mut State)) {
        let mut state = State::new(5);
        f(&mut state);
        let dir = self.profile();
        private_dir(&dir.tool()).expect("the tool directory");
        state.save(&dir).expect("state");
    }

    /// `p` holds an account.
    pub(super) fn link_account(&self) {
        self.save_state(|s| s.set("linked_since_ms", json!(5)));
    }

    pub(super) fn write_config(&self, text: &str) {
        let config = self.profile().config_file();
        std::fs::create_dir_all(config.parent().expect("parent")).expect("config dir");
        std::fs::write(config, text).expect("config");
    }

    pub(super) fn set(&mut self, key: &'static str, value: Option<&str>) {
        self.overrides.retain(|(k, _)| *k != key);
        self.overrides.push((key, value.map(str::to_owned)));
    }

    pub(super) fn vars(&self, key: &str) -> Option<String> {
        if let Some((_, v)) = self.overrides.iter().find(|(k, _)| *k == key) {
            return v.clone();
        }
        let path = |p: PathBuf| Some(p.to_str().expect("utf-8").to_owned());
        match key {
            "HOME" => path(self.scratch.0.join("home")),
            "XDG_DATA_HOME" => path(self.data()),
            "XDG_RUNTIME_DIR" => path(self.run_dir()),
            _ => None,
        }
    }

    pub(super) fn run_with(&self, a: &Args, lines: &[LineRow]) -> Result<Cleared> {
        self.with_ctx(lines, |ctx| preflight(a, ctx))
    }

    /// Runs `f` over this world's context (the launcher's tests run `start_in` over it).
    pub(super) fn with_ctx<R>(&self, lines: &[LineRow], f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let vars = |k: &str| self.vars(k);
        let disk = |_: &Path| self.disk.clone();
        let busy = || self.busy;
        let now = || 1_700_000_000_000;
        let clock = || match self.clock {
            Some(clock) => Ok(clock),
            None => bail!("adjtimex refused"),
        };
        let lock = |path: &Path| -> Result<Lock> {
            if self.lock_error {
                bail!("cannot open");
            }
            Ok(if self.held.iter().any(|h| h == path) {
                Lock::Held
            } else {
                Lock::Free
            })
        };
        let target = self.target();
        let ctx = Ctx {
            vars: &vars,
            root: &self.root,
            target: &target,
            git: &self.git,
            cgroup: &self.cgroup,
            disk: &disk,
            test_lock_busy: &busy,
            net_dev: &self.net_dev,
            uid: self.uid,
            now_ms: &now,
            clock: &clock,
            lock: &lock,
            lines,
        };
        f(&ctx)
    }

    pub(super) fn run(&self, a: &Args) -> Result<Cleared> {
        self.run_with(a, LINES)
    }
}

/// `start --confirmed-by-user --profile p` with more flags.
pub(super) fn start(more: &[&str]) -> Args {
    let mut all = vec!["start", "--confirmed-by-user", "--profile", "p"];
    all.extend_from_slice(more);
    args(&all).expect("a command line")
}

/// Every path below `dir`, relative and sorted (the keeper root before and after a preflight).
pub(super) fn listing(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .expect("below")
                .display()
                .to_string();
            let meta = std::fs::symlink_metadata(&path).expect("meta");
            out.push(format!("{rel} {} {:o}", meta.len(), meta.mode()));
            if meta.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

// ---- The whole ------------------------------------------------------------------------------------

/// A world in which every check passes, for a first link and for a reconnect; the preflight reports
/// what the launcher needs and MAKES NOTHING (no directory, no file, no switch).
#[test]
fn a_good_world_passes_every_check_and_makes_nothing() {
    let w = World::new("good");
    let before = listing(&w.scratch.0);
    let cleared = w
        .run(&start(&["--for-link"]))
        .expect("a first link may start");
    assert!(cleared.warnings.is_empty(), "{:?}", cleared.warnings);
    assert_eq!(cleared.head.as_deref(), Some(HEAD));
    assert_eq!(cleared.encrypted, Some(true));
    assert_eq!(cleared.socket_dir, Some(w.run_dir()));
    assert!(!cleared.leftover_list && !cleared.link_stood);
    assert_eq!(
        cleared.switch.on_text, "networks-on = [\"Threema\"]\n",
        "a missing file gets just that line"
    );
    let migrations = cleared.migrations.expect("HEAD's migrations");
    assert_eq!(migrations.len(), 2 * MIGRATION_DIRS.len());
    assert_eq!(migrations["threema/0002_more.sql"], "b2");
    assert!(!w.profile().dir.exists(), "nothing is made");
    assert_eq!(listing(&w.scratch.0), before, "nothing is made");

    w.link_account();
    let before = listing(&w.scratch.0);
    let cleared = w.run(&start(&[])).expect("a reconnect may start");
    assert!(cleared.warnings.is_empty(), "{:?}", cleared.warnings);
    assert!(cleared.state.linked());
    assert_eq!(listing(&w.scratch.0), before, "nothing is made");
}

/// The checks run in the brief's order: a world that fails them all is fixed one fault at a time,
/// and each refusal is the NEXT check's, never a later one's; the preflight makes nothing on the
/// way.
#[test]
fn the_checks_run_in_the_briefs_order() {
    let mut w = World::new("order");
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set(
            "halted",
            json!({"kind": "s5a", "at_ms": 1_700_000_000_000u64}),
        );
        s.set("migrations", json!({"core/0001_init.sql": "old"}));
    });
    w.set("NEXTEST", Some("1"));
    w.cgroup = NO_SLICE.to_owned();
    w.disk = None;
    w.git.status = " M a.rs\n".to_owned();
    w.clock = None;
    w.git
        .files
        .insert("Cargo.lock".to_owned(), lock_with(&[("rustls", "0.23.1")]));
    w.git.files.insert(
        "crates/kourier-threema/src/conn/lines.rs".to_owned(),
        String::new(),
    );
    w.held.push(w.profile().keeper_lock_file());
    w.write_config("pending-link = \"Threema\"\n");
    w.set("XDG_RUNTIME_DIR", None);

    // `--for-link` on a profile that holds an account, with every fault at once.
    let a = |confirmed: bool| {
        let mut all = vec!["start", "--profile", "p", "--for-link"];
        if confirmed {
            all.push("--confirmed-by-user");
        }
        args(&all).expect("a command line")
    };
    let steps: &[&str] = &[
        "is set",                       // 1: not a test
        "user's go",                    // 2: the go
        "kourier.slice",                // 3: the cap
        "no disk encryption",           // 4: the place and the disk
        "tree is not clean",            // 5: the source
        "was halted",                   // 6: the halt
        "already has a login",          // 7: for-link on a login
        "migration core/0001_init.sql", // 8: a changed migration
        "clock state",                  // 9: the clock
        "TLS bump",                     // 10: Cargo.lock
        "not in HEAD's source",         // 11: the lines
        "keeper lock",                  // 12: the keeper root
        "pending-link",                 // 13: the switch
        "XDG_RUNTIME_DIR is not set",   // 14: the socket's place
    ];
    let mut confirmed = false;
    for (i, snippet) in steps.iter().enumerate() {
        // Every fault of a later check is still there: this must be the first left.
        let before = listing(&w.keeper());
        let err = error_of(w.run(&a(confirmed)));
        assert!(
            err.contains(snippet),
            "check {}: expected {snippet:?}, got: {err}",
            i + 1
        );
        assert_eq!(
            listing(&w.keeper()),
            before,
            "check {} made something",
            i + 1
        );
        // The fix of this step.
        match i {
            0 => w.overrides.retain(|(k, _)| *k != "NEXTEST"),
            1 => confirmed = true,
            2 => w.cgroup = SLICE.to_owned(),
            3 => w.disk = Some("dm-crypt".to_owned()),
            4 => w.git.status = String::new(),
            5 => w.save_state(|s| {
                s.set("linked_since_ms", json!(5));
                s.set("migrations", json!({"core/0001_init.sql": "old"}));
            }),
            6 => w.save_state(|s| s.set("migrations", json!({"core/0001_init.sql": "old"}))),
            7 => w.save_state(|_| ()),
            8 => w.clock = Some(synced()),
            9 => {
                w.git.files.insert("Cargo.lock".to_owned(), good_lock());
            }
            10 => w.git.files = FakeGit::good().files,
            11 => w.held.clear(),
            12 => w.write_config(""),
            13 => w.set(
                "XDG_RUNTIME_DIR",
                Some(w.run_dir().to_str().expect("utf-8")),
            ),
            _ => unreachable!(),
        }
    }
    w.run(&a(true)).expect("every fault fixed");
}

// ---- Checks 1-3 -----------------------------------------------------------------------------------

/// Check 1: a start inside a test run is refused, whichever variable says so; nothing is looked at.
#[test]
fn a_start_inside_a_test_run_is_refused() {
    for name in TEST_ENV {
        let mut w = World::new("nottest");
        let name: &'static str = name;
        w.set(name, Some("1"));
        let err = error_of(w.run(&start(&["--for-link"])));
        assert!(err.contains("never a test") && err.contains(name), "{err}");
        assert_eq!(w.git.calls.get(), 0, "git was asked before the first check");
    }
    let w = World::new("nottest-ok");
    w.run(&start(&["--for-link"])).expect("no test variable");
}

/// Check 2: his go, for every start, before git is asked or anything else is read.
#[test]
fn a_start_needs_his_go_every_time() {
    let mut w = World::new("go");
    w.cgroup = NO_SLICE.to_owned();
    let a = args(&["start", "--profile", "p", "--for-link"]).expect("a command line");
    let err = error_of(w.run(&a));
    assert!(
        err.contains("user's go") && err.contains("--confirmed-by-user"),
        "{err}"
    );
    assert_eq!(w.git.calls.get(), 0);
    // A linked profile needs it just the same: a go is not remembered.
    w.link_account();
    let a = args(&["start", "--profile", "p"]).expect("a command line");
    assert!(error_of(w.run(&a)).contains("user's go"));
}

/// Check 3: the cap. A real start needs `kourier.slice` and a container does not count; a rehearsal
/// may also run where `container` is set; the rehearsal sees no interface but `lo`.
#[test]
fn the_cap_and_the_rehearsals_offline_rule() {
    // (cgroup, container variable, rehearsal, refused with)
    let rows: &[(&str, Option<&str>, bool, Option<&str>)] = &[
        (SLICE, None, false, None),
        (
            NO_SLICE,
            None,
            false,
            Some("never runs on the agents server"),
        ),
        (NO_SLICE, Some("podman"), false, Some("kourier.slice")),
        (SLICE, None, true, None),
        (NO_SLICE, Some("podman"), true, None),
        (NO_SLICE, Some(""), true, Some("agents server")),
        (
            NO_SLICE,
            None,
            true,
            Some("rehearse must run inside kourier.slice"),
        ),
    ];
    for (cgroup, container, rehearsal, refused) in rows {
        match (check_slice(cgroup, *container, *rehearsal), refused) {
            (Ok(()), None) => {}
            (Err(e), Some(words)) => {
                let e = format!("{e:#}");
                assert!(
                    e.contains(words) && e.contains("Nothing was started"),
                    "{e}"
                );
            }
            (got, want) => panic!("{cgroup:?} {container:?} {rehearsal}: {got:?} vs {want:?}"),
        }
    }
    check_rehearsal_offline(LO_ONLY).expect("lo alone");
    let e = error_of(check_rehearsal_offline(WITH_ETH));
    assert!(e.contains("eth0") && e.contains("unshare -rn"), "{e}");
    for unreadable in ["", "Inter-|x\n", "garbage\nmore\n"] {
        let e = error_of(check_rehearsal_offline(unreadable));
        assert!(e.contains("cannot tell"), "{unreadable:?}: {e}");
    }
}

// ---- Checks 4-5 -----------------------------------------------------------------------------------

/// Check 4: the place (the repository, a worktree) and the disk, with the user's word for a plain
/// disk as a warning.
#[test]
fn the_place_and_the_disk() {
    let mut w = World::new("place");
    w.root = w.scratch.0.clone();
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(
        e.contains("inside the repository") && e.contains("profile p"),
        "{e}"
    );
    assert!(
        !e.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path in {e}"
    );

    let mut w = World::new("place2");
    w.git.worktrees.push(w.data());
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(e.contains("a worktree of the repository"), "{e}");

    let mut w = World::new("disk");
    w.disk = None;
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(
        e.contains("no disk encryption") && e.contains("--allow-unencrypted-disk"),
        "{e}"
    );
    let cleared = w
        .run(&start(&["--for-link", "--allow-unencrypted-disk"]))
        .expect("the user's word");
    assert_eq!(cleared.encrypted, Some(false));
    assert_eq!(cleared.warnings.len(), 1);
    assert!(cleared.warnings[0].contains("without encryption"));
}

/// Check 5: HEAD is main's and the tree is clean.
#[test]
fn the_source_is_main_and_clean() {
    let mut git = FakeGit::good();
    assert_eq!(check_source(&git).expect("good"), HEAD);
    git.main = "9999999999999999999999999999999999999999".to_owned();
    let e = error_of(check_source(&git));
    assert!(
        e.contains("is not main") && e.contains("a1b2c3d4e5") && e.contains("9999999999"),
        "{e}"
    );
    git.main = HEAD.to_owned();
    git.status = " M a.rs\n?? b.rs\n\n".to_owned();
    let e = error_of(check_source(&git));
    assert!(e.contains("not clean") && e.contains("2 changed"), "{e}");
}

// ---- Checks 6-8 -----------------------------------------------------------------------------------

/// Check 6: a halted profile needs `--after-halt`; the halt moves to `last_halt` in memory only.
#[test]
fn a_halted_profile_needs_the_users_word() {
    let w = World::new("halt");
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set(
            "halted",
            json!({"kind": "s5a", "code": "follower", "at_ms": 1_700_000_000_000u64}),
        );
    });
    let e = error_of(w.run(&start(&[])));
    assert!(
        e.contains("halted (s5a follower, 2023-11-14 22:13:20 UTC)")
            && e.contains("--after-halt")
            && e.contains("read threema-run/app.log"),
        "{e}"
    );
    let on_disk = std::fs::read_to_string(w.profile().state_file()).expect("state");
    let cleared = w.run(&start(&["--after-halt"])).expect("his word");
    assert!(cleared.state.get("halted").is_null());
    assert_eq!(cleared.state.get("last_halt")["kind"], "s5a");
    assert_eq!(
        std::fs::read_to_string(w.profile().state_file()).expect("state"),
        on_disk,
        "the state file is the launcher's to save"
    );
    // A state with no halt is no halt; a halt with no time says so.
    let mut state = State::new(1);
    check_halt(&mut state, false).expect("none");
    state.set("halted", json!({"kind": "s2"}));
    assert!(error_of(check_halt(&mut state, false)).contains("at an unknown time"));
}

/// Check 7: a link start is for a profile without an account and without an open question; any other
/// start is for a profile with one. A login that stood at `linking` is no refusal: the launcher asks.
#[test]
fn a_link_start_and_a_plain_start_each_fit_their_own_profile() {
    // (holds an account, for-link, a question is open, refused with)
    let rows: &[(bool, bool, bool, Option<&str>)] = &[
        (false, true, false, None),
        (true, false, false, None),
        (true, true, false, Some("already has a login")),
        (false, false, false, Some("nothing to connect")),
        (false, true, true, Some("open question")),
        (true, false, true, None),
        (true, true, true, Some("already has a login")),
    ];
    for (linked, for_link, question, refused) in rows {
        let w = World::new("forlink");
        if *linked {
            w.link_account();
        }
        if *question {
            std::fs::write(w.keeper().join(OPEN_QUESTION_FILE), "{}").expect("question");
        }
        let a = if *for_link {
            start(&["--for-link"])
        } else {
            start(&[])
        };
        match (w.run(&a), refused) {
            (Ok(_), None) => {}
            (Err(e), Some(words)) => {
                let e = format!("{e:#}");
                assert!(
                    e.contains(words) && e.contains("Nothing was started"),
                    "{e}"
                );
            }
            (got, want) => panic!("{linked} {for_link} {question}: {got:?} vs {want:?}"),
        }
    }
    // A failed link that left `link_at`: not a refusal, but told and handed on.
    let w = World::new("linkstood");
    w.save_state(|s| s.set("link_at", json!(7)));
    let cleared = w.run(&start(&["--for-link"])).expect("the launcher asks");
    assert!(cleared.link_stood);
    assert!(
        cleared
            .warnings
            .iter()
            .any(|m| m.contains("reached linking"))
    );
}

/// Check 8: the migrations of HEAD against the profile's.
#[test]
fn the_migrations_of_head_against_the_profiles() {
    let map = |rows: &[(&str, &str)]| -> BTreeMap<String, String> {
        rows.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let recorded = map(&[("core/0001.sql", "a"), ("threema/0001.sql", "b")]);
    // (current, allow, refused with)
    let rows: &[(Entries, bool, Option<&str>)] = &[
        (
            &[("core/0001.sql", "a"), ("threema/0001.sql", "b")],
            false,
            None,
        ),
        (
            &[("core/0001.sql", "a"), ("threema/0001.sql", "X")],
            false,
            Some("changed after it ran"),
        ),
        (
            &[("core/0001.sql", "a")],
            false,
            Some("is gone from this build"),
        ),
        (
            &[
                ("core/0001.sql", "a"),
                ("threema/0001.sql", "b"),
                ("threema/0002.sql", "c"),
            ],
            false,
            Some("threema-run backup"),
        ),
        (
            &[
                ("core/0001.sql", "a"),
                ("threema/0001.sql", "b"),
                ("threema/0002.sql", "c"),
            ],
            true,
            None,
        ),
        // A change is refused even with the user's word: it is a new number or nothing.
        (
            &[("core/0001.sql", "Y"), ("threema/0001.sql", "b")],
            true,
            Some("changed after it ran"),
        ),
    ];
    for (current, allow, refused) in rows {
        match (check_migrations(&recorded, &map(current), *allow), refused) {
            (Ok(()), None) => {}
            (Err(e), Some(words)) => {
                let e = format!("{e:#}");
                assert!(
                    e.contains(words) && e.contains("Nothing was started"),
                    "{e}"
                );
            }
            (got, want) => panic!("{current:?} {allow}: {got:?} vs {want:?}"),
        }
    }
    // A profile that has run nothing takes any set (a first start).
    check_migrations(&BTreeMap::new(), &map(&[("core/0001.sql", "a")]), false).expect("fresh");

    // The listing of HEAD: the `.sql` files by label and name, the rest ignored, a missing
    // directory refused.
    let mut git = FakeGit::good();
    let all = tracked_migrations(&git).expect("listing");
    assert!(all.contains_key("core/0001_init.sql") && !all.keys().any(|k| k.ends_with(".md")));
    git.migrations.remove("crates/kourier-threema/migrations");
    let e = error_of(tracked_migrations(&git));
    assert!(e.contains("crates/kourier-threema/migrations"), "{e}");

    // Through the preflight: a migration of the profile that HEAD changed.
    let w = World::new("migr");
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set("migrations", json!({"core/0001_init.sql": "not-b1"}));
    });
    let e = error_of(w.run(&start(&[])));
    assert!(e.contains("migration core/0001_init.sql changed"), "{e}");
    let full = json!(tracked_migrations(&FakeGit::good()).expect("listing"));
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set("migrations", full.clone());
    });
    w.run(&start(&[])).expect("the same blobs");
    let mut w = World::new("migr2");
    w.save_state(|s| {
        s.set("linked_since_ms", json!(5));
        s.set("migrations", full.clone());
    });
    w.git
        .migrations
        .get_mut("crates/kourier-core/migrations")
        .expect("dir")
        .push(("0003_new.sql".to_owned(), "b3".to_owned()));
    let e = error_of(w.run(&start(&[])));
    assert!(
        e.contains("not run yet") && e.contains("core/0003_new.sql"),
        "{e}"
    );
    w.run(&start(&["--allow-schema-change"]))
        .expect("after a backup");
}

// ---- Checks 9-11 ----------------------------------------------------------------------------------

/// Check 9: the rule `timedatectl` uses (critic R-L1): `STA_UNSYNC` clear, `adjtimex` not
/// `TIME_ERROR`, `maxerror` under 16 s; a clock that cannot be read refuses.
#[test]
fn the_clock_must_be_synchronized() {
    let sec = 1_000_000;
    // (state, status, maxerror in us, refused with)
    let rows: &[(i32, i32, i64, Option<&str>)] = &[
        (0, 0, 0, None),
        (0, 0, 500_000, None),
        // Under the old bound of 1 s a synchronized PC was refused about once in 45 starts.
        (0, 0, 2 * sec, None),
        (0, 0, 16 * sec - 1, None),
        (
            0,
            0,
            16 * sec,
            Some("estimate of the clock's error is 16 s"),
        ),
        (0, 0, 40 * sec, Some("is 40 s")),
        (libc::TIME_ERROR, 0, 0, Some("not synchronized")),
        (0, libc::STA_UNSYNC, 0, Some("not synchronized")),
        (
            libc::TIME_ERROR,
            libc::STA_UNSYNC,
            17 * sec,
            Some("not synchronized"),
        ),
        // Other status bits (PLL, NANO ...) are none of its business.
        (1, 0x0001 | 0x2000, 3_000, None),
    ];
    for (state, status, maxerror_us, refused) in rows {
        let clock = Clock {
            state: *state,
            status: *status,
            maxerror_us: *maxerror_us,
        };
        match (check_clock(Ok(clock)), refused) {
            (Ok(()), None) => {}
            (Err(e), Some(words)) => {
                let e = format!("{e:#}");
                assert!(
                    e.contains(words) && e.contains("Nothing was started"),
                    "{e}"
                );
            }
            (got, want) => panic!("{clock:?}: {got:?} vs {want:?}"),
        }
    }
    assert_eq!(MAXERROR_LIMIT_US, 16 * sec);
    let e = error_of(check_clock(Err(anyhow::anyhow!("EPERM"))));
    assert!(e.contains("cannot read the kernel's clock state"), "{e}");
    // Through the preflight, a clock that cannot be read refuses the start.
    let mut w = World::new("clock");
    w.clock = None;
    assert!(error_of(w.run(&start(&["--for-link"]))).contains("clock state"));
}

/// The real reader answers or fails with its own words (a container may deny `adjtimex`): it never
/// panics, and what it says is coherent.
#[test]
fn the_real_clock_reader_answers_or_says_why() {
    match read_clock() {
        Ok(clock) => assert!(clock.state >= 0 && clock.maxerror_us >= 0, "{clock:?}"),
        Err(e) => assert!(format!("{e:#}").contains("adjtimex"), "{e:#}"),
    }
}

/// Versions are compared as numbers, a pre-release is below its release.
#[test]
fn versions_compare_as_numbers() {
    let rows: &[(&str, &str, bool)] = &[
        ("0.23.46", "0.23.46", true),
        ("0.23.47", "0.23.46", true),
        ("0.23.45", "0.23.46", false),
        ("0.103.9", "0.103.16", false),
        ("0.103.100", "0.103.16", true),
        ("1.0.11", "1.0.11", true),
        ("1.0.9", "1.0.11", false),
        ("1.1.0", "1.0.11", true),
        ("2.0.0", "1.0.11", true),
        ("0.24.0", "0.23.46", true),
        ("1.0.11-rc.1", "1.0.11", false),
        ("1.0.12-rc.1", "1.0.11", true),
        ("1.0.11+build5", "1.0.11", true),
        ("garbage", "1.0.11", false),
        ("1.0", "1.0.11", false),
        ("1.0.11.1", "1.0.11", false),
    ];
    for (found, needed, ok) in rows {
        assert_eq!(at_least(found, needed), *ok, "{found} vs {needed}");
    }
}

/// Check 10 (critic R-L6): every `[[package]]` of each name holds main's bump; a name that is
/// missing, or one entry of two that is old, is refused by name; `webpki-root-certs` is not read.
#[test]
fn the_lock_holds_mains_tls_bump_for_every_entry() {
    check_tls(&good_lock()).expect("the bump");
    assert_eq!(TLS_MINIMUMS.len(), 3);
    // (the entries, refused with)
    let rows: &[(Entries, &[&str])] = &[
        (
            &[
                ("rustls", "0.23.45"),
                ("rustls-webpki", "0.103.16"),
                ("webpki-roots", "1.0.11"),
            ],
            &["rustls 0.23.45 (needs 0.23.46"],
        ),
        (
            &[
                ("rustls", "0.23.46"),
                ("rustls-webpki", "0.103.15"),
                ("webpki-roots", "1.0.11"),
            ],
            &["rustls-webpki 0.103.15 (needs 0.103.16"],
        ),
        (
            &[
                ("rustls", "0.23.46"),
                ("rustls-webpki", "0.103.16"),
                ("webpki-roots", "1.0.9"),
            ],
            &["webpki-roots 1.0.9 (needs 1.0.11"],
        ),
        // Two versions of one crate: the old one counts.
        (
            &[
                ("rustls", "0.23.46"),
                ("rustls", "0.21.12"),
                ("rustls-webpki", "0.103.16"),
                ("webpki-roots", "1.0.11"),
            ],
            &["rustls 0.21.12"],
        ),
        // Missing names.
        (
            &[("rustls", "0.23.46"), ("rustls-webpki", "0.103.16")],
            &["webpki-roots is not in the lock"],
        ),
        (
            &[],
            &[
                "rustls is not in the lock",
                "rustls-webpki is not in the lock",
                "webpki-roots is not in the lock",
            ],
        ),
        // Every shortfall is named at once.
        (
            &[
                ("rustls", "0.23.45"),
                ("rustls-webpki", "0.103.15"),
                ("webpki-roots", "1.0.9"),
            ],
            &[
                "rustls 0.23.45",
                "rustls-webpki 0.103.15",
                "webpki-roots 1.0.9",
            ],
        ),
    ];
    for (entries, words) in rows {
        let e = error_of(check_tls(&lock_with(entries)));
        for word in *words {
            assert!(e.contains(word), "{entries:?}: want {word:?} in {e}");
        }
        assert!(
            e.contains("TLS bump")
                && e.contains("webpki-root-certs")
                && e.contains("Nothing was started"),
            "{e}"
        );
    }
    // A real lock's shape: other keys, a dependency list, entries with a `source` line.
    let real = "# lock\nversion = 4\n\n[[package]]\nname = \"rustls\"\nversion = \"0.23.46\"\nsource = \"registry+x\"\nchecksum = \"ab\"\ndependencies = [\n \"ring\",\n]\n\n[[package]]\nname = \"rustls-webpki\"\nversion = \"0.103.16\"\n\n[[package]]\nname = \"webpki-roots\"\nversion = \"1.0.11\"\n";
    check_tls(real).expect("a real lock's shape");
}

/// Check 11: every line the tool reads stands in HEAD's source in its words; a renamed or reworded
/// line, a missing file, a missing literal are each named; the table is data.
#[test]
fn every_line_the_tool_reads_stands_in_heads_source() {
    const F: &str = "crates/x/src/lines.rs";
    let table = [
        LineRow {
            file: F,
            name: Some("L_ONE"),
            text: "the first line",
            level: Level::Info,
            fields: &[],
        },
        LineRow {
            file: F,
            name: Some("L_WRAPPED"),
            text: "a long line that wraps after its equals sign",
            level: Level::Info,
            fields: &[],
        },
        LineRow {
            file: "crates/y/src/app.rs",
            name: None,
            text: "a literal in a log call",
            level: Level::Info,
            fields: &[],
        },
    ];
    let mut git = FakeGit::good();
    git.files.insert(
        F.to_owned(),
        "pub(crate) const L_ONE: &str = \"the first line\";\n/// d\npub(crate) const L_WRAPPED: &str =\n    \"a long line that wraps after its equals sign\";\n".to_owned(),
    );
    git.files.insert(
        "crates/y/src/app.rs".to_owned(),
        "tracing::warn!(%problem, \"a literal in a log call\");\n".to_owned(),
    );
    check_lines(&git, &table).expect("all three");

    // (the file F's text, the other file's text, refused with)
    let wrapped = "pub(crate) const L_WRAPPED: &str =\n    \"a long line that wraps after its equals sign\";\n";
    let one = "pub(crate) const L_ONE: &str = \"the first line\";\n";
    let literal = "tracing::warn!(%problem, \"a literal in a log call\");\n";
    let rows: &[(String, &str, &[&str])] = &[
        // Renamed.
        (
            format!("pub(crate) const L_UNO: &str = \"the first line\";\n{wrapped}"),
            literal,
            &["1 line(s)", "crates/x/src/lines.rs: L_ONE (not there)"],
        ),
        // Reworded.
        (
            format!("pub(crate) const L_ONE: &str = \"the 1st line\";\n{wrapped}"),
            literal,
            &["L_ONE (its words changed)"],
        ),
        // The words on a line that is neither the declaration nor the next.
        (
            format!(
                "{one}pub(crate) const L_WRAPPED: &str =\n    EXTRA;\n    \"a long line that wraps after its equals sign\";\n"
            ),
            literal,
            &["L_WRAPPED (its words changed)"],
        ),
        // The literal gone.
        (
            format!("{one}{wrapped}"),
            "tracing::warn!(%problem, \"another\");\n",
            &["a literal (not there)"],
        ),
        // Everything gone.
        (String::new(), "", &["3 line(s)"]),
    ];
    for (f_text, other, words) in rows {
        let mut git = FakeGit::good();
        git.files.insert(F.to_owned(), f_text.clone());
        git.files
            .insert("crates/y/src/app.rs".to_owned(), (*other).to_owned());
        let e = error_of(check_lines(&git, &table));
        for word in *words {
            assert!(e.contains(word), "want {word:?} in {e}");
        }
        assert!(
            e.contains("blind") && e.contains("Nothing was started"),
            "{e}"
        );
    }
    // A file HEAD does not have.
    let mut git = FakeGit::good();
    git.files.insert(F.to_owned(), format!("{one}{wrapped}"));
    let e = error_of(check_lines(&git, &table));
    assert!(
        e.contains("crates/y/src/app.rs: a literal (the file is not in HEAD)"),
        "{e}"
    );
    // An empty table passes; the tool's own table must not be empty.
    check_lines(&git, &[]).expect("nothing to look for");
}

/// Git over the working tree: `show HEAD:<path>` reads the file below the root.
struct TreeGit<'a>(&'a Path);

impl Git for TreeGit<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        match args {
            ["show", spec] => {
                let path = spec.strip_prefix("HEAD:").expect("a HEAD spec");
                Ok(std::fs::read_to_string(self.0.join(path))?)
            }
            other => bail!("unexpected git call {other:?}"),
        }
    }
}

/// Every row of the tool's table stands in THIS checkout's source in its words (TH6's `lines.rs`,
/// and E6 in the app's): a word TH6 changes without TR's table fails here, the day it changes.
#[test]
fn the_shipped_table_stands_in_this_checkouts_source() {
    let root = crate::root();
    check_lines(&TreeGit(&root), LINES).expect("the table's rows stand in the working tree");
}

/// The tool's own table: not empty, well formed (a repository path of a source file, a SCREAMING
/// name or none, words that are fixed text with no field and no quote), no row twice, and HEAD's
/// source of the table's own rows passes (the world's git is made from it).
#[test]
fn the_shipped_table_is_not_empty_and_is_well_formed() {
    assert!(LINES.len() >= 15, "the table of `lines.rs` is never empty");
    let mut seen = std::collections::BTreeSet::new();
    for row in LINES {
        assert!(
            row.file.starts_with("crates/") && row.file.ends_with(".rs"),
            "{}",
            row.file
        );
        if let Some(name) = row.name {
            assert!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{name}"
            );
        }
        assert!(
            !row.text.is_empty() && !row.text.contains(['"', '=', '\n', '\\']),
            "{}",
            row.text
        );
        assert!(
            seen.insert((row.file, row.name, row.text)),
            "{} twice",
            row.text
        );
    }
    check_lines(&FakeGit::good(), LINES).expect("a source that has them all");
}

// ---- Check 12 -------------------------------------------------------------------------------------

/// Check 12: the locks of the keeper root. A link start looks at EVERY profile (the list is read by
/// the app of any of them); any other start at its own; the keeper lock refuses both; a probe that
/// fails refuses.
#[test]
fn the_keeper_root_has_no_other_run() {
    let other = |w: &World| {
        let q = w.dir("q");
        private_dir(&q.dir).expect("profile q");
        q
    };
    // The keeper lock: every start waits.
    for linked in [true, false] {
        let mut w = World::new("keeper-lock");
        if linked {
            w.link_account();
        }
        w.held.push(w.profile().keeper_lock_file());
        // A reconnect on a profile with an account, a first link on one without.
        let a = if linked {
            start(&[])
        } else {
            start(&["--for-link"])
        };
        let e = error_of(w.run(&a));
        assert!(
            e.contains("keeper lock") && e.contains("Nothing was started"),
            "{e}"
        );
    }
    let w = World::new("keeper-lock-name");
    assert_eq!(w.profile().keeper_lock_file(), w.keeper().join(KEEPER_LOCK));

    // Another profile's Kourier or supervisor: a link start refuses, naming the profile only.
    let mut w = World::new("keeper-other");
    let q = other(&w);
    w.held
        .push(crate::threema_run::preflight::app_lock_file(&q));
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(
        e.contains("Kourier is running on profile q") && e.contains("list of test IDs"),
        "{e}"
    );
    assert!(
        !e.contains(w.keeper().to_str().expect("utf-8")),
        "a path in {e}"
    );
    w.held = vec![q.lock_file()];
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(e.contains("supervisor holds profile q"), "{e}");
    // ... and a plain start of a linked profile does not mind a neighbour.
    w.link_account();
    w.run(&start(&[])).expect("only its own locks count");

    // Its own app, its own supervisor: both start kinds refuse.
    for (is_app, a) in [
        (true, start(&[])),
        (false, start(&[])),
        (true, start(&["--for-link"])),
        (false, start(&["--for-link"])),
    ] {
        let mut w = World::new("keeper-own");
        let own = w.profile();
        w.link_account();
        if a.for_link {
            // A link start is for a profile without an account.
            w.save_state(|_| ());
        }
        w.held = vec![if is_app {
            crate::threema_run::preflight::app_lock_file(&own)
        } else {
            own.lock_file()
        }];
        let e = error_of(w.run(&a));
        let want = if is_app {
            "Kourier is running on profile p"
        } else {
            "supervisor holds profile p"
        };
        assert!(e.contains(want), "{e}");
    }

    // A probe that fails is a refusal: what cannot be seen is not free.
    let mut w = World::new("keeper-err");
    w.lock_error = true;
    let e = error_of(w.run(&start(&["--for-link"])));
    assert!(e.contains("cannot tell whether a run holds a lock"), "{e}");
}

/// Check 12: a list of test IDs that stands while no run holds the keeper lock is a leftover: the
/// preflight tells (and the launcher removes it); it removes nothing itself.
#[test]
fn a_leftover_list_is_told_and_left_for_the_launcher() {
    let w = World::new("leftover");
    let list = w.keeper().join(crate::threema_run::ALLOW_FILE);
    std::fs::write(&list, "x\n").expect("list");
    let cleared = w
        .run(&start(&["--for-link"]))
        .expect("a leftover is no refusal");
    assert!(cleared.leftover_list);
    assert!(
        cleared
            .warnings
            .iter()
            .any(|m| m.contains("did not end cleanly") && m.contains("removes it")),
        "{:?}",
        cleared.warnings
    );
    assert!(list.exists(), "the preflight removed the file");
    // A list under a live run's keeper lock is not left over: the start waits instead.
    let mut w = World::new("leftover2");
    std::fs::write(w.keeper().join(crate::threema_run::ALLOW_FILE), "x\n").expect("list");
    w.held.push(w.profile().keeper_lock_file());
    assert!(error_of(w.run(&start(&["--for-link"]))).contains("keeper lock"));
}

/// The profiles of a keeper root: directories with a profile's name; the list's file, `backups`,
/// the lock, a stray file, a link and a dotted name are none.
#[test]
fn the_keeper_roots_profiles() {
    let w = World::new("profiles");
    assert!(profiles_of(&w.scratch.0.join("nowhere")).is_empty());
    for dir in ["b", "a", "backups", ".hidden", "bad name"] {
        std::fs::create_dir(w.keeper().join(dir)).expect("dir");
    }
    std::fs::write(w.keeper().join(crate::threema_run::ALLOW_FILE), "x").expect("file");
    std::fs::write(w.keeper().join(KEEPER_LOCK), "").expect("lock");
    std::fs::write(w.keeper().join("stray"), "").expect("file");
    std::os::unix::fs::symlink(w.keeper().join("a"), w.keeper().join("linked")).expect("link");
    assert_eq!(profiles_of(&w.keeper()), ["a", "b"]);
}

/// The real probe: a held lock is held, a free file and a missing file are free, a directory that is
/// no lock file errors or answers, never panics.
#[test]
fn the_real_lock_probe() {
    use std::time::Duration;
    let scratch = Scratch::new("probe");
    let path = scratch.0.join("lock");
    assert_eq!(
        probe_lock(&path, Duration::ZERO).expect("absent"),
        Lock::Free
    );
    let file = std::fs::File::create(&path).expect("file");
    assert_eq!(
        probe_lock(&path, Duration::from_millis(400)).expect("free"),
        Lock::Free
    );
    file.try_lock().expect("exclusive");
    assert_eq!(probe_lock(&path, Duration::ZERO).expect("held"), Lock::Held);
    drop(file);
    assert_eq!(
        probe_lock(&path, Duration::from_millis(400)).expect("let go"),
        Lock::Free
    );
}

// ---- Check 13 -------------------------------------------------------------------------------------

/// Check 13: the switch plan. The start's text changes only the `networks-on` entry (all the lines
/// it spans), puts a missing one in front of any table, and never touches a key in a table; the end's
/// text is the same with the list empty.
#[test]
fn the_switch_is_planned_without_touching_anything_else() {
    let on = "networks-on = [\"Threema\"]";
    // (existing, the start's text, found on)
    let rows: &[(Option<&str>, String, bool)] = &[
        (None, format!("{on}\n"), false),
        (Some(""), format!("{on}\n"), false),
        (Some("tray = true\n"), format!("{on}\ntray = true\n"), false),
        (
            Some("networks-on = []\ntray = true\n"),
            format!("{on}\ntray = true\n"),
            false,
        ),
        // The case of the name does not matter to the app, so it does not to the plan.
        (
            Some("networks-on = [\"threema\"]\n"),
            format!("{on}\n"),
            true,
        ),
        (
            Some("networks-on = [ 'Threema' , ]  # mine\n"),
            format!("{on}\n"),
            true,
        ),
        (
            Some("tray = true\nnetworks-on = [\n  \"Threema\",\n]\nlocation-files = \"ask\"\n"),
            format!("tray = true\n{on}\nlocation-files = \"ask\"\n"),
            true,
        ),
        // A missing entry goes in front of a table, where a top-level key must be.
        (Some("[ui]\nx = 1\n"), format!("{on}\n[ui]\nx = 1\n"), false),
        // An entry inside a table is no switch of the app's.
        (
            Some("[ui]\nnetworks-on = [\"WhatsApp\"]\n"),
            format!("{on}\n[ui]\nnetworks-on = [\"WhatsApp\"]\n"),
            false,
        ),
        (Some("tray = true"), format!("{on}\ntray = true\n"), false),
    ];
    for (existing, text, found) in rows {
        let plan = plan_switch(*existing).unwrap_or_else(|e| panic!("{existing:?}: {e:#}"));
        assert_eq!(&plan.on_text, text, "{existing:?}");
        assert_eq!(plan.found_on, *found, "{existing:?}");
        assert_eq!(plan.existing.as_deref(), *existing);
        // The end's text: off, with every other line as it was, and the start's text again after it.
        let off = switch_text(Some(&plan.on_text), false).expect("off");
        assert!(!off.contains("Threema"), "{off}");
        assert!(off.contains("networks-on = []"), "{off}");
        assert_eq!(
            switch_text(Some(&off), true).expect("on again"),
            plan.on_text
        );
    }
    assert_eq!(switch_text(None, false).expect("off"), "networks-on = []\n");

    // Refusals.
    let rows: &[(&str, &str)] = &[
        ("networks-on = [\"WhatsApp\"]\n", "names another network"),
        (
            "networks-on = [\"Threema\", \"Telegram\"]\n",
            "names another network",
        ),
        ("pending-link = \"Threema\"\n", "link page by itself"),
        (
            "tray = true\npending-link = \"WhatsApp\"\n",
            "link page by itself",
        ),
        ("[ui]\npending-link = \"x\"\n", "link page by itself"),
        ("networks-on = true\n", "does not read"),
        ("networks-on = [\"Threema\"] junk\n", "does not read"),
        ("networks-on = [\"Threema\"\n", "does not read"),
        ("networks-on = []\nnetworks-on = []\n", "does not read"),
    ];
    for (text, words) in rows {
        let e = error_of(plan_switch(Some(text)));
        assert!(
            e.contains(words) && e.contains("Nothing was started"),
            "{text:?}: {e}"
        );
        if words.contains("does not read") {
            assert!(switch_text(Some(text), true).is_err(), "{text:?}");
        }
    }

    // Through the preflight: a switch found on tells the last run did not end cleanly (check 12
    // proved nobody is alive on the profile); the file is not written.
    let w = World::new("switch");
    w.link_account();
    w.write_config("networks-on = [\"Threema\"]\ntray = true\n");
    let cleared = w.run(&start(&[])).expect("on, nobody alive");
    assert!(cleared.switch.found_on);
    assert!(
        cleared
            .warnings
            .iter()
            .any(|m| m.contains("did not end cleanly")),
        "{:?}",
        cleared.warnings
    );
    assert_eq!(
        std::fs::read_to_string(w.profile().config_file()).expect("config"),
        "networks-on = [\"Threema\"]\ntray = true\n"
    );
    w.write_config("networks-on = [\"WhatsApp\"]\n");
    assert!(error_of(w.run(&start(&[]))).contains("another network"));
    w.write_config("pending-link = \"Threema\"\n");
    assert!(error_of(w.run(&start(&[]))).contains("pending-link"));
}

// ---- Check 14 -------------------------------------------------------------------------------------

/// Check 14: the control socket's place: `XDG_RUNTIME_DIR` set, absolute, a directory (no link) of
/// this user's with mode 0700, short enough for the socket's name.
#[test]
fn the_control_sockets_place() {
    let uid = current_uid();
    let scratch = Scratch::new("sock");
    let make = |name: &str, mode: u32| {
        let dir = scratch.0.join(name);
        std::fs::create_dir(&dir).expect("dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("mode");
        dir
    };
    let run = |dir: &Path, uid: u32| {
        let value = dir.to_str().expect("utf-8").to_owned();
        check_socket_place(
            &|k: &str| (k == "XDG_RUNTIME_DIR").then(|| value.clone()),
            uid,
        )
    };
    let good = make("good", 0o700);
    assert_eq!(run(&good, uid).expect("good"), good);
    // The mode.
    for mode in [0o755, 0o750, 0o770, 0o500] {
        let dir = make(&format!("m{mode:o}"), mode);
        assert!(
            error_of(run(&dir, uid)).contains("not your private directory"),
            "{mode:o}"
        );
    }
    // Another owner (the arm a plain user cannot reach otherwise).
    assert!(error_of(run(&good, uid + 1)).contains("not your private directory"));
    // A file, a link, a missing directory.
    let file = scratch.0.join("file");
    std::fs::write(&file, "").expect("file");
    assert!(error_of(run(&file, uid)).contains("not your private directory"));
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(&good, &link).expect("link");
    assert!(error_of(run(&link, uid)).contains("not your private directory"));
    assert!(error_of(run(&scratch.0.join("nowhere"), uid)).contains("cannot be looked at"));
    // Unset, empty, relative.
    let none = |value: Option<&str>| {
        let value = value.map(str::to_owned);
        check_socket_place(&|_: &str| value.clone(), uid)
    };
    assert!(error_of(none(None)).contains("XDG_RUNTIME_DIR is not set"));
    assert!(error_of(none(Some(""))).contains("XDG_RUNTIME_DIR is not set"));
    assert!(error_of(none(Some("run/user/1"))).contains("not an absolute path"));
    // The length: the directory plus the socket's name must fit 107 bytes (80 + 27).
    assert_eq!(SOCKET_NAME_LEN, 27);
    let base = scratch.0.to_str().expect("utf-8").len() + 1;
    assert!(
        base < SOCKET_PATH_MAX - SOCKET_NAME_LEN,
        "the scratch place is too long for this test"
    );
    let fits = make(&"f".repeat(SOCKET_PATH_MAX - SOCKET_NAME_LEN - base), 0o700);
    assert_eq!(fits.as_os_str().len() + SOCKET_NAME_LEN, SOCKET_PATH_MAX);
    run(&fits, uid).expect("exactly the limit");
    let long = make(
        &"l".repeat(SOCKET_PATH_MAX - SOCKET_NAME_LEN - base + 1),
        0o700,
    );
    let e = error_of(run(&long, uid));
    assert!(
        e.contains("would not fit a socket path") && e.contains("107"),
        "{e}"
    );
}

// ---- The rehearsal --------------------------------------------------------------------------------

/// A rehearsal keeps the cap (or the container), the offline rule, the halt, the link rule, the
/// keeper root and the switch, and skips the go, the place and disk, the source, the migrations, the
/// clock, the TLS bump, the lines and the socket's place.
#[test]
fn a_rehearsal_skips_what_only_a_real_start_needs() {
    let mut w = World::new("rehearsal");
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
    // Every real-only check would fail here: no go, a plain disk, a dirty tree, HEAD not main, a
    // clock that cannot be read, an old lock, a source without the lines, no runtime directory.
    w.disk = None;
    w.git.status = " M a.rs\n".to_owned();
    w.git.main = "9999999999999999999999999999999999999999".to_owned();
    w.clock = None;
    w.git.files.clear();
    w.set("XDG_RUNTIME_DIR", None);
    w.root = w.scratch.0.clone();
    w.cgroup = NO_SLICE.to_owned();
    w.set("container", Some("podman"));
    let calls_before = w.git.calls.get();
    let cleared = w.run(&a).expect("a rehearsal passes the checks it keeps");
    assert_eq!(
        w.git.calls.get(),
        calls_before,
        "a rehearsal asks git nothing"
    );
    assert!(cleared.dir.rehearsal);
    assert_eq!(
        (
            cleared.encrypted,
            cleared.head,
            cleared.migrations,
            cleared.socket_dir
        ),
        (None, None, None, None)
    );
    assert_eq!(cleared.switch.on_text, "networks-on = [\"Threema\"]\n");

    // What it keeps.
    let mut kept = World::new("rehearsal2");
    let place = kept
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
    kept.net_dev = WITH_ETH.to_owned();
    assert!(error_of(kept.run(&a)).contains("eth0"));
    kept.net_dev = LO_ONLY.to_owned();
    kept.cgroup = NO_SLICE.to_owned();
    assert!(error_of(kept.run(&a)).contains("rehearse must run inside kourier.slice"));
    kept.cgroup = SLICE.to_owned();
    // The link rule: a rehearsal's plain start of a profile without an account is refused too.
    let plain = args(&["start", "--rehearsal-place", place.to_str().expect("utf-8")])
        .expect("a command line");
    assert!(error_of(kept.run(&plain)).contains("nothing to connect"));
    kept.run(&a).expect("a rehearsal's link start");
}

// ---- The words --------------------------------------------------------------------------------------

/// The time helper the later chunks share.
#[test]
fn the_time_helper_says_utc() {
    assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(utc(1_700_000_000_000), "2023-11-14 22:13:20 UTC");
    assert_eq!(utc(1_709_164_800_000), "2024-02-29 00:00:00 UTC");
}

/// A busy test lock is a warning, never a refusal; every refusal ends in the sentence that nothing
/// was started and names a profile by its name, never a path.
#[test]
fn a_busy_test_lock_only_warns_and_no_refusal_names_a_path() {
    let mut w = World::new("busy");
    w.busy = true;
    let cleared = w.run(&start(&["--for-link"])).expect("a warning only");
    assert_eq!(cleared.warnings.len(), 1);
    assert!(
        cleared.warnings[0].contains("test lock"),
        "{:?}",
        cleared.warnings
    );
    // A refusal of each kind in one world: no path of the scratch place (which holds the user's
    // home in a real run) reaches a message.
    let scratch = w.scratch.0.to_str().expect("utf-8").to_owned();
    let mut bad = World::new("nopath");
    bad.git.status = " M a.rs\n".to_owned();
    bad.clock = None;
    bad.disk = None;
    let place = bad.scratch.0.to_str().expect("utf-8").to_owned();
    for a in [
        start(&["--for-link"]),
        start(&["--for-link", "--allow-unencrypted-disk"]),
    ] {
        let e = error_of(bad.run(&a));
        assert!(!e.contains(&place) && !e.contains(&scratch), "{e}");
    }
}

/// The production side of the seam reads this machine and makes nothing: the checkout, the user, a
/// clock in milliseconds, free locks for files that are not there, the tool's own table.
#[test]
fn the_real_context_reads_this_machine() {
    with_real_ctx(|ctx| {
        assert!(ctx.root.join("Cargo.toml").is_file());
        assert!(ctx.target.is_absolute());
        assert_eq!(ctx.uid, current_uid());
        assert!((ctx.now_ms)() > 1_700_000_000_000);
        assert_eq!(ctx.lines.len(), LINES.len());
        assert_eq!(
            (ctx.lock)(&ctx.root.join("no/such/lock")).expect("a missing file"),
            Lock::Free
        );
        Ok(())
    })
    .expect("a real context");
}
