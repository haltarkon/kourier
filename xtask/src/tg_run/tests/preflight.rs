//! The preflight of `connect` (X2-X7, X11): every refusal made on its own, with its own text, over a
//! world of canned answers and scratch places.

use std::cell::Cell;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

use super::{Scratch, args, error_of};
use crate::lock;
use crate::tg_run::preflight::{
    Cleared, Ctx, Git, RANGES_RS, RealGit, check_source, list_sha256, preflight,
};
use crate::tg_run::profile::State;
use crate::tg_run::stage::CheckedLibrary;
use crate::tg_run::{API_FILE, Args, CONNECT_COMMAND, KEEPER_DIR, REHEARSAL_DIR};

pub(super) const HEAD: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
const SLICE: &str = "0::/user.slice/user-1000.slice/user@1000.service/kourier.slice/run-r1.scope\n";
pub(super) const LO_ONLY: &str = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n";

/// A `ranges.rs` text of the shape the tool reads: the two blocks, any prose around them.
pub(super) fn ranges_body() -> String {
    "//! The allow-list.\n\nuse std::net::{Ipv4Addr, Ipv6Addr};\n\n/// Telegram's IPv4 blocks.\npub(crate) const TELEGRAM_V4: &[(Ipv4Addr, u8)] = &[\n    (Ipv4Addr::new(10, 0, 0, 0), 8),\n    (Ipv4Addr::new(10, 1, 0, 0), 16),\n];\n\n/// Telegram's IPv6 blocks.\npub(crate) const TELEGRAM_V6: &[(Ipv6Addr, u8)] = &[\n    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),\n];\n\npub(crate) const PORTS: [u16; 3] = [443, 80, 5222];\n".to_owned()
}

/// A text of `ranges_body` with a VERIFIED line that matches its blocks.
pub(super) fn verified_ranges() -> String {
    let body = ranges_body();
    let hash = list_sha256(&body).expect("hash");
    format!(
        "//! VERIFIED 2026-11-01 sha256:{} list-sha256:{hash} the published list, fetched with his go\n{body}",
        "ab".repeat(32)
    )
}

/// A git runner with canned answers; it counts its calls.
pub(super) struct FakeGit {
    pub head: String,
    pub main: String,
    pub status: String,
    pub ranges: String,
    pub calls: Cell<u32>,
}

impl FakeGit {
    fn good() -> Self {
        Self {
            head: HEAD.to_owned(),
            main: HEAD.to_owned(),
            status: String::new(),
            ranges: verified_ranges(),
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
            ["show", spec] if *spec == format!("HEAD:{RANGES_RS}") => Ok(self.ranges.clone()),
            ["worktree", "list", "--porcelain"] => Ok(
                "worktree /fake/repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /fake/other\nHEAD abc\ndetached\n"
                    .to_owned(),
            ),
            other => bail!("unexpected git call {other:?}"),
        }
    }
}

/// Everything a `connect` reads, below a scratch directory: the places, the environment, git, the
/// cgroup, the disk, the test lock, the API file and the library check.
pub(super) struct World {
    pub scratch: Scratch,
    pub git: FakeGit,
    pub cgroup: String,
    pub disk: Option<String>,
    pub env: Vec<(&'static str, String)>,
    pub busy: bool,
    /// The text of /proc/self/net/dev the connect sees (a namespace with `lo` alone).
    pub net_dev: String,
    pub uid: u32,
    /// What the library check says.
    pub library_ok: bool,
    pub library_calls: Cell<u32>,
}

pub(super) fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// An API file of `content` with the mode `mode`, made by the test (synthetic values only).
pub(super) fn write_api_file(path: &Path, content: &str, mode: u32) {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, content).expect("api file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

impl World {
    pub fn new(name: &str) -> Self {
        let w = Self {
            scratch: Scratch::new(name),
            git: FakeGit::good(),
            cgroup: SLICE.to_owned(),
            disk: Some("dm-crypt".to_owned()),
            env: Vec::new(),
            busy: false,
            net_dev: LO_ONLY.to_owned(),
            uid: current_uid(),
            library_ok: true,
            library_calls: Cell::new(0),
        };
        w.make_keeper_and_api_file();
        w
    }

    pub fn data(&self) -> PathBuf {
        self.scratch.0.join("data")
    }

    pub fn keeper(&self) -> PathBuf {
        self.data().join(KEEPER_DIR)
    }

    pub fn api_file(&self) -> PathBuf {
        self.keeper().join(API_FILE)
    }

    pub fn target(&self) -> PathBuf {
        self.scratch.0.join("target")
    }

    pub fn profile(&self) -> PathBuf {
        self.keeper().join("t")
    }

    /// The keeper root (0700) and a private API file of synthetic content.
    pub fn make_keeper_and_api_file(&self) {
        std::fs::create_dir_all(self.keeper()).expect("keeper");
        std::fs::set_permissions(self.keeper(), std::fs::Permissions::from_mode(0o700))
            .expect("mode");
        let content = format!("id={}\nhash={}\n", 1, "0".repeat(32));
        write_api_file(&self.api_file(), &content, 0o600);
    }

    /// `connect --confirmed-by-user --env test --profile t` plus `extra`.
    pub fn args(&self, extra: &[&str]) -> Args {
        let mut line = vec![
            "connect",
            "--confirmed-by-user",
            "--env",
            "test",
            "--profile",
            "t",
        ];
        line.extend(extra);
        args(&line).expect("parses")
    }

    /// The rehearsal's `connect`: no go, a place below `target/tgr/`.
    pub fn rehearsal_args(&self) -> Args {
        let place = self
            .target()
            .join(REHEARSAL_DIR)
            .join("1/x")
            .join(KEEPER_DIR)
            .join("p");
        args(&[
            "connect",
            "--env",
            "test",
            "--rehearsal-place",
            place.to_str().expect("utf-8"),
        ])
        .expect("parses")
    }

    /// Runs `f` with the context a connect reads.
    pub fn with_ctx<T>(&self, f: impl FnOnce(&Ctx<'_>) -> T) -> T {
        let data = self.data().to_str().expect("utf-8").to_owned();
        let home = self
            .scratch
            .0
            .join("home")
            .to_str()
            .expect("utf-8")
            .to_owned();
        let vars = |key: &str| match key {
            "XDG_DATA_HOME" => Some(data.clone()),
            "HOME" => Some(home.clone()),
            other => self
                .env
                .iter()
                .find(|(k, _)| *k == other)
                .map(|(_, v)| v.clone()),
        };
        let disk = |_: &Path| self.disk.clone();
        let busy = || self.busy;
        let library = || {
            self.library_calls.set(self.library_calls.get() + 1);
            if self.library_ok {
                Ok(CheckedLibrary {
                    path: PathBuf::from("library"),
                    sha256: "0".repeat(64),
                })
            } else {
                bail!("the TDLib library fails 1 check(s)")
            }
        };
        let (root, target) = (PathBuf::from("/fake/repo"), self.target());
        let ctx = Ctx {
            vars: &vars,
            root: &root,
            target: &target,
            git: &self.git,
            cgroup: &self.cgroup,
            disk: &disk,
            test_lock_busy: &busy,
            net_dev: &self.net_dev,
            uid: self.uid,
            library: &library,
        };
        f(&ctx)
    }

    pub fn run(&self, a: &Args) -> Result<Cleared> {
        self.with_ctx(|ctx| preflight(a, ctx))
    }

    pub fn git_calls(&self) -> u32 {
        self.git.calls.get()
    }

    /// Writes the profile's state file with `env`.
    pub fn write_state(&self, env: &str) {
        let dir = self.profile().join("tg-run");
        std::fs::create_dir_all(&dir).expect("tool dir");
        std::fs::write(
            dir.join("state.json"),
            format!("{{\"version\":1,\"env\":\"{env}\",\"connects\":[]}}"),
        )
        .expect("state");
    }

    pub fn write_config(&self, text: &str) {
        self.write_state("test");
        let file = self.profile().join("kourier/config/kourier.toml");
        std::fs::create_dir_all(file.parent().expect("dir")).expect("config dir");
        std::fs::write(file, text).expect("config");
    }
}

// ---- the good start -------------------------------------------------------------------------------

#[test]
fn a_good_connect_passes_every_check_and_makes_nothing() {
    let w = World::new("good");
    let cleared = w.run(&w.args(&[])).expect("passes");
    assert_eq!(cleared.head.as_deref(), Some(HEAD));
    assert_eq!(cleared.ranges_verified, Some(true));
    assert!(cleared.encrypted == Some(true) && cleared.library.is_some());
    assert!(cleared.warnings.is_empty(), "{:?}", cleared.warnings);
    assert_eq!(cleared.state.env(), Some("test"));
    assert_eq!(cleared.dir.label(), "t");
    assert_eq!(w.library_calls.get(), 1);
    // The preflight made nothing: the profile is not there, only the keeper root and HIS file.
    assert!(!w.profile().exists());
    let made: Vec<_> = std::fs::read_dir(w.keeper())
        .expect("keeper")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(made, vec![std::ffi::OsString::from(API_FILE)]);
    // A busy test lock is a warning, not a refusal.
    let mut busy = World::new("busy");
    busy.busy = true;
    let cleared = busy.run(&busy.args(&[])).expect("passes");
    assert_eq!(cleared.warnings.len(), 1);
    assert!(
        cleared.warnings[0].contains("test lock"),
        "{:?}",
        cleared.warnings
    );
    // A library that fails its check refuses the connect with the library's own words.
    let mut bad = World::new("library");
    bad.library_ok = false;
    assert!(error_of(bad.run(&bad.args(&[]))).contains("fails 1 check"));
}

// ---- X2 -------------------------------------------------------------------------------------------

/// X2: no go, nothing happens. The check comes before anything is read: no git call, no library
/// check, nothing made. Mutant: the go check skipped.
#[test]
fn a_connect_without_the_go_makes_nothing() {
    let w = World::new("go");
    let mut line = w.args(&[]);
    line.confirmed = false;
    let error = error_of(w.run(&line));
    assert!(error.contains("needs the user's go"), "{error}");
    assert!(error.contains("Nothing was started"), "{error}");
    assert_eq!(w.git_calls(), 0, "git was asked before the go was checked");
    assert_eq!(w.library_calls.get(), 0);
    assert!(!w.profile().exists());
}

// ---- X3 -------------------------------------------------------------------------------------------

/// X3: a real run is never a test. Each variable on its own. Mutant: one variable dropped.
#[test]
fn a_real_run_is_never_a_test() {
    for name in [
        lock::HELD_ENV,
        "NEXTEST",
        "NEXTEST_RUN_ID",
        "NEXTEST_EXECUTION_MODE",
    ] {
        let mut w = World::new("testlock");
        w.env = vec![(name, "1".to_owned())];
        let error = error_of(w.run(&w.args(&[])));
        assert!(error.contains("never a test"), "{name}: {error}");
        assert!(error.contains(name), "{name}: {error}");
        assert_eq!(w.git_calls(), 0, "{name}");
    }
    // kcap's own variable says nothing about a test.
    let mut w = World::new("kcap");
    w.env = vec![("NEXTEST_TEST_THREADS", "2".to_owned())];
    w.run(&w.args(&[])).expect("kcap's variable is no test");
}

// ---- X4 -------------------------------------------------------------------------------------------

/// X4: `connect` runs in `kourier.slice` only; a container's word is a rehearsal's. Mutant: a
/// container accepted for `connect`.
#[test]
fn connect_needs_the_slice_and_rehearse_may_use_the_container() {
    for cgroup in [
        "0::/user.slice/user-1000.slice/session-2.scope\n",
        "0::/user.slice/user-1000.slice/user@1000.service/notkourier.slice/run-r1.scope\n",
        "",
    ] {
        let mut w = World::new("slice");
        w.cgroup = cgroup.to_owned();
        let error = error_of(w.run(&w.args(&[])));
        assert!(
            error.contains("kourier.slice") && error.contains("kcap"),
            "{error}"
        );
        // D-4: the refusal tells the runbook's command line, from the one constant.
        assert!(
            error.contains(&format!("`{CONNECT_COMMAND}`")),
            "the refusal does not name the connect command line: {error}"
        );
        assert_eq!(w.git_calls(), 0);
    }
    // A container's word is a rehearsal's only: a real run in one is refused.
    let mut w = World::new("container");
    w.cgroup = "0::/.lxc\n".to_owned();
    w.env = vec![("container", "lxc".to_owned())];
    let error = error_of(w.run(&w.args(&[])));
    assert!(
        error.contains("tg-run connect must run inside kourier.slice"),
        "{error}"
    );
    assert_eq!(w.git_calls(), 0);
    // The rehearsal may: in the container (`container` set and not empty), not elsewhere.
    let rehearsal = w.rehearsal_args();
    w.run(&rehearsal)
        .expect("the rehearsal runs in the build container");
    w.env = vec![("container", String::new())];
    assert!(error_of(w.run(&rehearsal)).contains("kourier.slice"));
    w.env = Vec::new();
    assert!(error_of(w.run(&rehearsal)).contains("kourier.slice"));
}

// ---- X5 -------------------------------------------------------------------------------------------

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// X5: the build is main's HEAD with a clean tree: a repository of its own through the real runner,
/// and the same two refusals through a whole connect. Mutant: the dirty check skipped.
#[test]
fn the_build_is_main_with_a_clean_tree() {
    let scratch = Scratch::new("repo");
    let repo = scratch.0.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git_in(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "one\n").expect("file");
    git_in(&repo, &["add", "a.txt"]);
    git_in(&repo, &["commit", "-q", "-m", "one"]);
    let git = RealGit { root: &repo };
    let head = check_source(&git).expect("clean main passes");
    assert_eq!(head.len(), 40);
    std::fs::write(repo.join("b.txt"), "untracked\n").expect("file");
    let error = error_of(check_source(&git));
    assert!(
        error.contains("not clean") && error.contains("1 changed"),
        "{error}"
    );
    std::fs::remove_file(repo.join("b.txt")).expect("remove");
    std::fs::write(repo.join("a.txt"), "two\n").expect("file");
    assert!(error_of(check_source(&git)).contains("not clean"));
    git_in(&repo, &["checkout", "-q", "--", "a.txt"]);
    check_source(&git).expect("clean again");
    git_in(&repo, &["checkout", "-q", "-b", "other"]);
    std::fs::write(repo.join("a.txt"), "three\n").expect("file");
    git_in(&repo, &["commit", "-q", "-a", "-m", "three"]);
    let error = error_of(check_source(&git));
    assert!(error.contains("is not main"), "{error}");
    // The same two refusals through a whole connect (a fake git); nothing after them is asked.
    let mut w = World::new("source");
    w.git.main = "ffffffffffffffffffffffffffffffffffffffff".to_owned();
    assert!(error_of(w.run(&w.args(&[]))).contains("is not main"));
    assert_eq!(
        w.library_calls.get(),
        0,
        "the library was checked on a wrong HEAD"
    );
    w.git.main = HEAD.to_owned();
    w.git.status = "?? crates/x.rs\n M a\n".to_owned();
    assert!(error_of(w.run(&w.args(&[]))).contains("2 changed"));
    assert_eq!(w.library_calls.get(), 0);
    // A rehearsal builds a tree that is being edited: it asks git nothing about main.
    w.git.status = " M xtask/src/tg_run/mod.rs\n".to_owned();
    let calls = w.git_calls();
    let cleared = w
        .run(&w.rehearsal_args())
        .expect("the rehearsal passes on a dirty tree");
    assert!(
        cleared.head.is_none() && cleared.library.is_none() && cleared.ranges_verified.is_none()
    );
    assert_eq!(w.git_calls(), calls, "a rehearsal asked git");
}

// ---- X6 -------------------------------------------------------------------------------------------

/// X6 (+ the API variables, critic 22): the tool's environment holds no library path and no API
/// pair; they are refused, not stripped, before anything else is read, by a real run and a
/// rehearsal alike. Mutant: the library variable's refusal dropped.
#[test]
fn a_connect_refuses_kourier_tdjson_and_the_api_variables() {
    let secret = "synthetic-value-5309";
    for name in ["KOURIER_TDJSON", "KOURIER_TG_API_ID", "KOURIER_TG_API_HASH"] {
        let mut w = World::new("environment");
        w.env = vec![(name, secret.to_owned())];
        let error = error_of(w.run(&w.args(&[])));
        assert!(
            error.contains(name) && error.contains("Unset it"),
            "{name}: {error}"
        );
        assert!(!error.contains(secret), "{name} echoed its value: {error}");
        assert_eq!(w.git_calls(), 0, "{name}: git was asked");
        assert_eq!(w.library_calls.get(), 0, "{name}: the library was checked");
        // The rehearsal refuses them too.
        w.env.push(("container", "lxc".to_owned()));
        w.cgroup = "0::/.lxc\n".to_owned();
        assert!(
            error_of(w.run(&w.rehearsal_args())).contains(name),
            "{name}"
        );
    }
}

// ---- X7 -------------------------------------------------------------------------------------------

/// X7: the servers. `--env test` only; production says it comes with R2; missing or other is
/// refused; none of them echoes the value; a profile never changes servers. Mutants: production
/// accepted; the binding compare skipped.
#[test]
fn the_profile_keeps_its_servers() {
    let w = World::new("env");
    let with = |env: Option<&str>| {
        let mut a = w.args(&[]);
        a.env = env.map(str::to_owned);
        a
    };
    let error = error_of(w.run(&with(Some("production"))));
    assert!(error.contains("production comes with R2"), "{error}");
    let error = error_of(w.run(&with(None)));
    assert!(error.contains("--env is required"), "{error}");
    for other in ["staging", "", "Test", "test ", "local:/x"] {
        let error = error_of(w.run(&with(Some(other))));
        assert!(error.contains("--env must be test"), "{other:?}: {error}");
        if matches!(other, "staging" | "local:/x") {
            assert!(!error.contains(other), "{other:?} echoed: {error}");
        }
    }
    assert_eq!(w.library_calls.get(), 0);
    w.run(&with(Some("test"))).expect("test passes");
    // A profile bound to other servers (a hand-edited state) is refused for `test`.
    w.write_state("production");
    let error = error_of(w.run(&with(Some("test"))));
    assert!(error.contains("bound to the servers"), "{error}");
    w.write_state("test");
    w.run(&with(Some("test"))).expect("a test profile");
}

// ---- X11 ------------------------------------------------------------------------------------------

/// X11: a login is kept on an encrypted disk, or the user accepts that it is not. Mutant: the flag
/// inverted.
#[test]
fn an_unencrypted_disk_needs_its_flag() {
    let mut w = World::new("disk");
    w.disk = None;
    let error = error_of(w.run(&w.args(&[])));
    assert!(error.contains("no disk encryption"), "{error}");
    assert!(error.contains("--allow-unencrypted-disk"), "{error}");
    assert_eq!(w.library_calls.get(), 0);
    let cleared = w
        .run(&w.args(&["--allow-unencrypted-disk"]))
        .expect("the flag accepts it");
    assert_eq!(cleared.encrypted, Some(false));
    let text = crate::tg_run::summary_text(&w.args(&[]), &cleared);
    assert!(text.contains("; disk not encrypted;"), "{text}");
    assert!(
        cleared
            .warnings
            .iter()
            .any(|m| m.contains("without encryption")),
        "{:?}",
        cleared.warnings
    );
    // An encrypted disk needs no flag and says nothing with it.
    w.disk = Some("dm-crypt".to_owned());
    let cleared = w
        .run(&w.args(&["--allow-unencrypted-disk"]))
        .expect("passes");
    assert!(cleared.encrypted == Some(true) && cleared.warnings.is_empty());
    let text = crate::tg_run::summary_text(&w.args(&[]), &cleared);
    assert!(text.contains("; disk encrypted;"), "{text}");
}

// ---- the profile and its state ---------------------------------------------------------------------

/// A directory with Kourier data and no state file is not a profile of this tool and is not adopted;
/// a state file that is not tg-run's is not adopted either.
#[test]
fn a_directory_without_a_state_file_is_not_adopted() {
    let w = World::new("adopt");
    std::fs::create_dir_all(w.profile().join("kourier/data")).expect("data");
    let error = error_of(w.run(&w.args(&[])));
    assert!(
        error.contains("not adopted") && error.contains("no tg-run state file"),
        "{error}"
    );
    assert!(
        !error.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path: {error}"
    );
    for bad in [
        "{}",
        "[1]",
        "not json",
        "{\"version\":2,\"env\":\"test\",\"connects\":[]}",
        "{\"version\":1,\"connects\":[]}",
        "{\"version\":1,\"env\":\"test\"}",
    ] {
        let dir = w.profile().join("tg-run");
        std::fs::create_dir_all(&dir).expect("tool dir");
        std::fs::write(dir.join("state.json"), bad).expect("state");
        assert!(w.run(&w.args(&[])).is_err(), "{bad}");
    }
    // A profile that does not exist yet is a fresh one, bound to the servers asked for.
    let fresh = World::new("fresh");
    let cleared = fresh
        .run(&fresh.args(&[]))
        .expect("a profile that does not exist yet");
    assert_eq!(cleared.state, State::new("test"));
}

/// The profile's own `kourier.toml` (critic 25): `networks-on` names Telegram and nothing else when
/// it is there; a `pending-link` is Telegram's; other keys are none of the preflight's business.
#[test]
fn the_profile_registers_telegram_alone() {
    let w = World::new("switches");
    for ok in [
        "networks-on = [\"Telegram\"]\n",
        "networks-on = [ \"Telegram\" ]\npending-link = \"Telegram\"\n",
        "theme = \"dark\"\n",
        "# nothing but a comment\n",
        "pending-link = \"Telegram\"\n",
    ] {
        w.write_config(ok);
        w.run(&w.args(&[]))
            .unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
    }
    for bad in [
        "networks-on = [\"Telegram\", \"WhatsApp\"]\n",
        "networks-on = [\"WhatsApp\"]\n",
        "networks-on = []\n",
        "networks-on = Telegram\n",
        "networks-on = [\"Telegram\"]\npending-link = \"WhatsApp\"\n",
        "pending-link = \"\"\n",
    ] {
        w.write_config(bad);
        let error = error_of(w.run(&w.args(&[])));
        assert!(
            error.contains("networks-on") || error.contains("pending-link"),
            "{bad:?}: {error}"
        );
        assert!(error.contains("Nothing was started"), "{error}");
    }
}

// ---- the rehearsal's place ---------------------------------------------------------------------------

/// A rehearsal place skips the real run's go, test-lock, place, disk, main, list, API file and library
/// checks (and asks for none of them), keeps the cap, the environment and the servers, and runs only
/// where nothing can be reached.
#[test]
fn a_rehearsal_place_skips_the_real_checks_but_keeps_the_cap_and_the_net() {
    let mut w = World::new("rehearsal");
    w.env = vec![(lock::HELD_ENV, "1".to_owned())];
    w.git.status = " M xtask/src/tg_run/mod.rs\n".to_owned();
    w.git.ranges = ranges_body();
    w.disk = None;
    std::fs::remove_file(w.api_file()).expect("remove");
    let rehearsal = w.rehearsal_args();
    assert!(!rehearsal.confirmed);
    let cleared = w.run(&rehearsal).expect("the rehearsal passes");
    assert!(cleared.dir.rehearsal && cleared.dir.label() == "p (rehearsal)");
    assert!(
        cleared.head.is_none() && cleared.library.is_none() && cleared.ranges_verified.is_none()
    );
    assert_eq!(w.library_calls.get(), 0, "a rehearsal checks no library");
    assert_eq!(w.git_calls(), 0, "a rehearsal asked git");
    assert!(cleared.warnings.is_empty());
    // H-8: a rehearsal looks at no disk, so it does not claim one (here the disk lookup would have said
    // "not encrypted"). Mutant: the field back to a bool.
    assert_eq!(cleared.encrypted, None);
    let text = crate::tg_run::summary_text(&rehearsal, &cleared);
    assert!(text.contains("; disk not checked;"), "{text}");
    assert!(!text.contains("encrypted"), "{text}");
    // The cap is on for it too; the container is the cap on the server.
    w.cgroup = "0::/user.slice/session-2.scope\n".to_owned();
    assert!(error_of(w.run(&rehearsal)).contains("kourier.slice"));
    // Nothing may be reachable: an interface beside `lo`, an unreadable list, a header alone.
    w.cgroup = SLICE.to_owned();
    for net in [
        format!("{LO_ONLY}  eth0: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n"),
        String::new(),
        "garbage\n".to_owned(),
        LO_ONLY.lines().next().expect("line").to_owned(),
    ] {
        w.net_dev = net;
        let error = error_of(w.run(&rehearsal));
        assert!(error.contains("Nothing was started"), "{error}");
    }
    w.net_dev = format!("{LO_ONLY}  eth0: 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16\n");
    assert!(error_of(w.run(&rehearsal)).contains("eth0"));
    // A real run does not look at the interfaces (it needs the network).
    let real = World::new("rehearsal-real");
    let mut real = real;
    real.net_dev = format!("{LO_ONLY}  eth0: 1\n");
    real.run(&real.args(&[]))
        .expect("a real run sees the network");
    // The servers are bound in a rehearsal too.
    w.net_dev = LO_ONLY.to_owned();
    let mut production = w.rehearsal_args();
    production.env = Some("production".to_owned());
    assert!(error_of(w.run(&production)).contains("production comes with R2"));
}
