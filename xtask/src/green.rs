//! T5b: the same tree gives the same result, so a second run on it says so and ends.
//!
//! A GREEN `cargo xtask ci` (each tier) writes `target/.ci-green-<tier>.json`: a digest of
//! everything a test result can depend on, and what the run covered. A later run first takes
//! the digest of the tree it stands on; when a record of the same digest covers what this run
//! would do, it prints `ci: green at hh:mm (same tree)` and ends with status 0 before it
//! builds anything (`--force` runs anyway). The full tier after a green QUICK run on the same
//! digest runs only what quick did not ([`remainder`]). Why: 59 % of the test time of two days
//! was `ci --quick` runs, and many were repeats on a tree that had not changed (his words
//! 2026-10-15: "too much, run too often"; `target/test-load/options.md` §2 row 7).
//!
//! The digest is made of PARTS kept apart, so a miss can say what moved and a test can change
//! exactly one input: the tracked files and the untracked ones git does not ignore (path, mode's
//! exec bits and content; a tracked file that is gone from the disk hashes as gone), `Cargo.lock`,
//! `rust-toolchain.toml`, `.cargo/`, `tests/mock-harness/package-lock.json`, the mock harness's
//! surroundings (the Node major its search finds and the content of
//! `tests/mock-harness/node_modules/.package-lock.json`: F1), `rustc -vV`, the system Qt
//! (`qmake6 -query QT_VERSION`), the Breeze, KF6, QML-module and Qt packages and the two colour
//! scheme files the screenshot stamp reads, and every `KOURIER_*` and `RUST*` variable of the
//! environment (a `RUST_LOG`, a `KOURIER_NO_NEXTEST`, a `RUSTFLAGS` change a result). Left out
//! on purpose: [`crate::lock::HELD_ENV`] (it differs per holder), `RUSTUP_*` and
//! `RUST_RECURSION_COUNT` (rustup's own bookkeeping; `rustc -vV` and `rust-toolchain.toml`
//! say which toolchain) and `RUST_BACKTRACE` (it changes how a failure prints, no result).
//! The files that change no test result (`quick::is_neutral`: Markdown, `docs/` but the smoke
//! runbook, `.github/`, `LICENSES/`) have a part of their own that a miss names and the total
//! leaves out (F9): a docs-only edit after a green run does not run the tests again. Hashing is
//! `DefaultHasher` (SipHash, fixed keys, no new dependency: a dependency of xtask would change
//! `Cargo.lock`); a new toolchain may change it, and the only effect is one run that does not
//! skip.
//!
//! Only a run that PASSED writes a record, and only when the digest taken again at its end
//! is the one it started with (the tree did not move under it). A run that fails removes the
//! records of its digest: the tree is no longer known green. A record says what it covered
//! ([`Ran`]): `everything` for the full tier, else the non-Qt packages whose integration tests
//! ran, whether the Qt window tests and qmllint ran, whether the slow set ran. A request is
//! covered when a record of the same digest ran at least as much ([`Ran::covered_by`]).

use crate::lock;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which tier a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Quick,
    Full,
}

impl Tier {
    pub const ALL: [Tier; 2] = [Tier::Quick, Tier::Full];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Quick => "quick",
            Tier::Full => "full",
        }
    }
}

/// The part of the digest the neutral files go into (F9): `*.md`, `docs/` (not the runbook the
/// smoke tool's tests read), `.github/`, `LICENSES/` and the other files `quick::owner` takes as
/// changing no test result. It is shown when a run misses, but it is NOT one of the inputs that
/// make the [`Digest::total`]: a record covers a request whose tree differs from it only there.
pub const NEUTRAL_PART: &str = "neutral";

/// The parts of a digest, in the order they are combined.
pub const PART_NAMES: [&str; 12] = [
    "files",
    NEUTRAL_PART,
    "cargo-lock",
    "toolchain-file",
    "dot-cargo",
    "harness-lock",
    "harness",
    "rustc",
    "qt",
    "packages",
    "schemes",
    "env",
];

/// What the machine says about itself. Behind a struct so a test can change one answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    /// `rustc -vV`.
    pub rustc: String,
    /// `qmake6 -query QT_VERSION`, or "unavailable".
    pub qt: String,
    /// The installed Breeze, KF6 and Qt packages with their versions, sorted, or "unavailable".
    pub packages: String,
    /// What the Signal mock-server tests run on that git does not carry ([`harness_state`]).
    pub harness: String,
    /// The colour scheme files the screenshots read: (path as text, content).
    pub schemes: Vec<(String, Vec<u8>)>,
    /// `KOURIER_*` and `RUST*` variables ([`is_input_var`]), sorted.
    pub env: Vec<(String, String)>,
}

/// The packages whose version a result can depend on: the look (Breeze, KF6, the QML modules) and
/// Qt itself (a neon rebuild keeps `QT_VERSION` and changes the package; F7).
const PACKAGE_PATTERNS: [&str; 6] = [
    "breeze*",
    "kf6-*",
    "libkf6*",
    "qml6-module-*",
    "libqt6*",
    "qt6-*",
];

/// Whether an environment variable is an input of a test result (see the module doc).
pub fn is_input_var(name: &str) -> bool {
    (name.starts_with("KOURIER_") || name.starts_with("RUST"))
        && name != lock::HELD_ENV
        && name != "RUST_RECURSION_COUNT"
        // Changes how a failure is printed, not whether a test passes (F7).
        && name != "RUST_BACKTRACE"
        && !name.starts_with("RUSTUP_")
}

/// The input variables of an environment, sorted by name.
pub fn env_inputs(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut inputs: Vec<(String, String)> =
        vars.into_iter().filter(|(k, _)| is_input_var(k)).collect();
    inputs.sort();
    inputs
}

/// The stdout of a command that ran, whatever its status says.
fn stdout_of(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The Node major version of `node` (`--version` prints `v24.21.0`), or `None` when it does not
/// run.
fn node_major_of(node: &Path) -> Option<u32> {
    let out = Command::new(node)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// What the mock-server tests of `kourier-signal` and `kourier-e2e` need that no tracked file
/// says (F1): the Node major version the harness search finds (`KOURIER_NODE`, else
/// `$HOME/.nvm/versions/node/v24.21.0/bin/node`, else `node` on `PATH`, the first of Node 24 or
/// newer, as `crates/kourier-signal/tests/support/harness.rs` searches) and the content of
/// `tests/mock-harness/node_modules/.package-lock.json` (`node_modules` is git-ignored; a fresh
/// checkout has none). `node_major` answers for a Node binary; a test gives its own.
pub fn harness_state(
    root: &Path,
    explicit: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    node_major: &dyn Fn(&Path) -> Option<u32>,
) -> String {
    const MIN_NODE_MAJOR: u32 = 24;
    let candidates: Vec<PathBuf> = match explicit {
        Some(explicit) => vec![PathBuf::from(explicit)],
        None => home
            .map(|h| Path::new(h).join(".nvm/versions/node/v24.21.0/bin/node"))
            .into_iter()
            .chain([PathBuf::from("node")])
            .collect(),
    };
    let node = candidates
        .iter()
        .find_map(|c| node_major(c).filter(|m| *m >= MIN_NODE_MAJOR))
        .map_or_else(|| "none".to_owned(), |m| m.to_string());
    let modules =
        match std::fs::read(root.join("tests/mock-harness/node_modules/.package-lock.json")) {
            Ok(bytes) => {
                let mut hasher = DefaultHasher::new();
                bytes.hash(&mut hasher);
                format!("{:016x}", hasher.finish())
            }
            Err(_) => "absent".to_owned(),
        };
    format!("node={node} modules={modules}")
}

impl System {
    /// Asks the machine. `rustc` must answer (a run without it cannot build anyway); the
    /// others fall back to a fixed word so that a machine without them still has a stable
    /// digest. `scheme_files` are the colour scheme files the screenshots read.
    pub fn query(root: &Path, scheme_files: &[PathBuf]) -> Result<System> {
        let rustc = Command::new("rustc")
            .arg("-vV")
            .current_dir(root)
            .output()
            .context("run rustc -vV")?;
        if !rustc.status.success() {
            bail!("rustc -vV failed ({})", rustc.status);
        }
        let qt = Command::new("/usr/bin/qmake6")
            .args(["-query", "QT_VERSION"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map_or_else(
                || "unavailable".to_owned(),
                |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            );
        // dpkg-query exits 1 when one pattern matches nothing and still prints the others.
        let packages = stdout_of(
            Command::new("dpkg-query")
                .args(["-W", "-f", "${Package}=${Version}\n"])
                .args(PACKAGE_PATTERNS),
        )
        .map(|text| {
            let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            lines.sort_unstable();
            lines.join("\n")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unavailable".to_owned());
        let schemes = scheme_files
            .iter()
            .map(|path| {
                (
                    path.display().to_string(),
                    std::fs::read(path).unwrap_or_else(|_| b"<missing>".to_vec()),
                )
            })
            .collect();
        // `vars_os` and a lossy text: `vars` panics on a variable that is not Unicode (F6).
        let env = env_inputs(std::env::vars_os().map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        }));
        let harness = harness_state(
            root,
            std::env::var_os("KOURIER_NODE").as_deref(),
            std::env::var_os("HOME").as_deref(),
            &node_major_of,
        );
        Ok(System {
            rustc: String::from_utf8_lossy(&rustc.stdout).into_owned(),
            qt,
            packages,
            harness,
            schemes,
            env,
        })
    }
}

/// The digest of a tree and its surroundings: one number per part, and the one that combines
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub parts: BTreeMap<String, u64>,
    pub total: u64,
}

impl Digest {
    pub fn hex(&self) -> String {
        format!("{:016x}", self.total)
    }

    /// The parts of `self` that differ from `before` (or are missing there).
    pub fn differing(&self, before: &BTreeMap<String, u64>) -> Vec<String> {
        self.parts
            .iter()
            .filter(|(name, value)| before.get(*name) != Some(value))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// The part of the digest a path has of its own, else it belongs to "files".
fn own_part(path: &str) -> Option<&'static str> {
    match path {
        "Cargo.lock" => Some("cargo-lock"),
        "rust-toolchain.toml" => Some("toolchain-file"),
        "tests/mock-harness/package-lock.json" => Some("harness-lock"),
        _ if path.starts_with(".cargo/") => Some("dot-cargo"),
        _ => None,
    }
}

/// Every file git knows or would add (tracked, and untracked but not ignored), sorted.
fn listed_files(root: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .context("run git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Puts what a path holds into the hasher: its bytes, its link target, or "gone" (a tracked
/// file removed from the disk, or a directory).
fn hash_file(path: &Path, hasher: &mut DefaultHasher) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            1u8.hash(hasher);
            std::fs::read_link(path)
                .ok()
                .map(|target| target.to_string_lossy().into_owned())
                .hash(hasher);
        }
        Ok(meta) if meta.is_file() => {
            2u8.hash(hasher);
            // `chmod +x` of a script or fixture a test runs changes a result (F7).
            (meta.permissions().mode() & 0o111).hash(hasher);
            match std::fs::read(path) {
                Ok(bytes) => bytes.hash(hasher),
                Err(_) => 0xffu8.hash(hasher),
            }
        }
        _ => 0u8.hash(hasher),
    }
}

/// The digest of the tree under `root` and of `sys`.
pub fn digest(root: &Path, sys: &System) -> Result<Digest> {
    let mut hashers: BTreeMap<&'static str, DefaultHasher> = PART_NAMES
        .iter()
        .map(|name| (*name, DefaultHasher::new()))
        .collect();
    for rel in listed_files(root)? {
        let part = own_part(&rel).unwrap_or(if crate::quick::is_neutral(&rel) {
            NEUTRAL_PART
        } else {
            "files"
        });
        let hasher = hashers.get_mut(part).expect("a known part");
        rel.hash(hasher);
        hash_file(&root.join(&rel), hasher);
    }
    let mut put = |name: &'static str, write: &dyn Fn(&mut DefaultHasher)| {
        write(hashers.get_mut(name).expect("a known part"));
    };
    put("rustc", &|h| sys.rustc.hash(h));
    put("qt", &|h| sys.qt.hash(h));
    put("packages", &|h| sys.packages.hash(h));
    put("harness", &|h| sys.harness.hash(h));
    put("schemes", &|h| sys.schemes.hash(h));
    put("env", &|h| sys.env.hash(h));
    let parts: BTreeMap<String, u64> = hashers
        .into_iter()
        .map(|(name, hasher)| (name.to_owned(), hasher.finish()))
        .collect();
    // The neutral files are in `parts` (a miss names them) and out of the total (F9).
    let mut total = DefaultHasher::new();
    for name in PART_NAMES.into_iter().filter(|n| *n != NEUTRAL_PART) {
        name.hash(&mut total);
        parts[name].hash(&mut total);
    }
    Ok(Digest {
        parts,
        total: total.finish(),
    })
}

/// What a run covered (or, for a request, what it would cover).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ran {
    /// The full tier: every test, doc tests, qmllint, the slow set, the Qt window tests.
    pub everything: bool,
    /// The non-Qt packages whose integration tests ran (the slow set left out unless `slow`).
    pub integration: BTreeSet<String>,
    /// The Qt crate's integration (window) tests ran.
    pub qt_window_tests: bool,
    pub qmllint: bool,
    /// The slow set ran.
    pub slow: bool,
}

impl Ran {
    /// The full tier.
    pub fn everything() -> Ran {
        Ran {
            everything: true,
            integration: BTreeSet::new(),
            qt_window_tests: true,
            qmllint: true,
            slow: true,
        }
    }

    /// Whether a run that covered `have` has done at least what `self` asks for.
    pub fn covered_by(&self, have: &Ran) -> bool {
        have.everything
            || (!self.everything
                && self.integration.is_subset(&have.integration)
                && (!self.qt_window_tests || have.qt_window_tests)
                && (!self.qmllint || have.qmllint)
                && (!self.slow || have.slow))
    }

    /// Both runs together (two green runs on one digest).
    pub fn union(&self, other: &Ran) -> Ran {
        Ran {
            everything: self.everything || other.everything,
            integration: self
                .integration
                .union(&other.integration)
                .cloned()
                .collect(),
            qt_window_tests: self.qt_window_tests || other.qt_window_tests,
            qmllint: self.qmllint || other.qmllint,
            slow: self.slow || other.slow,
        }
    }
}

/// `.ci-green-<tier>.json` in cargo's target directory (`target/`).
pub fn record_path(target: &Path, tier: Tier) -> PathBuf {
    target.join(format!(".ci-green-{}.json", tier.name()))
}

/// What a green run left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub digest: u64,
    pub parts: BTreeMap<String, u64>,
    /// Unix seconds.
    pub at: u64,
    /// "nextest" or "cargo-test".
    pub engine: String,
    pub ran: Ran,
}

/// The layout of the file; a record of another one is read as none.
const FORMAT: u64 = 1;

fn hex(value: u64) -> String {
    format!("{value:016x}")
}

impl Record {
    pub fn to_json(&self) -> String {
        let parts: serde_json::Map<String, Value> = self
            .parts
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(hex(*value))))
            .collect();
        serde_json::to_string_pretty(&json!({
            "format": FORMAT,
            "digest": hex(self.digest),
            "parts": parts,
            "at": self.at,
            "engine": self.engine,
            "ran": {
                "everything": self.ran.everything,
                "integration": self.ran.integration,
                "qt_window_tests": self.ran.qt_window_tests,
                "qmllint": self.ran.qmllint,
                "slow": self.ran.slow,
            },
        }))
        .expect("a record is JSON")
    }

    pub fn parse(text: &str) -> Option<Record> {
        let v: Value = serde_json::from_str(text).ok()?;
        if v["format"].as_u64()? != FORMAT {
            return None;
        }
        let from_hex = |value: &Value| u64::from_str_radix(value.as_str()?, 16).ok();
        let parts = v["parts"]
            .as_object()?
            .iter()
            .map(|(name, value)| Some((name.clone(), from_hex(value)?)))
            .collect::<Option<BTreeMap<_, _>>>()?;
        let ran = &v["ran"];
        Some(Record {
            digest: from_hex(&v["digest"])?,
            parts,
            at: v["at"].as_u64()?,
            engine: v["engine"].as_str()?.to_owned(),
            ran: Ran {
                everything: ran["everything"].as_bool()?,
                integration: ran["integration"]
                    .as_array()?
                    .iter()
                    .map(|p| p.as_str().map(str::to_owned))
                    .collect::<Option<BTreeSet<_>>>()?,
                qt_window_tests: ran["qt_window_tests"].as_bool()?,
                qmllint: ran["qmllint"].as_bool()?,
                slow: ran["slow"].as_bool()?,
            },
        })
    }
}

/// The records on disk that can be read.
pub fn load(target: &Path) -> Vec<(Tier, Record)> {
    Tier::ALL
        .into_iter()
        .filter_map(|tier| {
            let text = std::fs::read_to_string(record_path(target, tier)).ok()?;
            Some((tier, Record::parse(&text)?))
        })
        .collect()
}

/// Writes a record (a temporary file, then a rename: a reader never sees half of one).
pub fn store(target: &Path, tier: Tier, record: &Record) -> Result<()> {
    let path = record_path(target, tier);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let temporary = path.with_extension("json.new");
    std::fs::write(&temporary, record.to_json())
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("write {}", path.display()))
}

/// Removes the records of this digest (the run on it failed: it is not known green any more).
/// Returns the tiers whose record went.
pub fn forget(target: &Path, digest: &Digest) -> Vec<Tier> {
    let mut gone = Vec::new();
    for (tier, record) in load(target) {
        if record.digest == digest.total && std::fs::remove_file(record_path(target, tier)).is_ok()
        {
            gone.push(tier);
        }
    }
    gone
}

/// The record of a green run, for the digest taken at its start (`before`) and again at its
/// end (`after`). `None` when the tree moved under the run: it proved nothing about either.
/// A record of the same tier and digest that is already there is merged in: two green runs
/// of one tree together covered both.
pub fn record_green(
    target: &Path,
    tier: Tier,
    before: &Digest,
    after: &Digest,
    engine: &str,
    ran: Ran,
    now: u64,
) -> Result<Option<Record>> {
    if before.total != after.total {
        return Ok(None);
    }
    let ran = load(target)
        .into_iter()
        .find(|(t, r)| *t == tier && r.digest == before.total && r.engine == engine)
        .map_or(ran.clone(), |(_, old)| old.ran.union(&ran));
    let record = Record {
        digest: before.total,
        parts: before.parts.clone(),
        at: now,
        engine: engine.to_owned(),
        ran,
    };
    store(target, tier, &record)?;
    Ok(Some(record))
}

/// A record that covers a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skip {
    pub tier: Tier,
    pub at: u64,
}

/// The newest record of this digest that covers `want`.
pub fn covering(records: &[(Tier, Record)], digest: &Digest, want: &Ran) -> Option<Skip> {
    records
        .iter()
        .filter(|(_, r)| r.digest == digest.total && want.covered_by(&r.ran))
        .max_by_key(|(_, r)| r.at)
        .map(|(tier, r)| Skip {
            tier: *tier,
            at: r.at,
        })
}

/// What a run says when it ends because of a record.
pub fn skip_message(skip: &Skip) -> String {
    format!(
        "ci: green at {} (same tree): the {} run on exactly this tree passed and covers what \
         this one would do; nothing to run. `--force` runs it anyway.",
        lock::clock_time(skip.at),
        skip.tier.name()
    )
}

/// Why the run is not skipped, when there is a record that is only of another tree: the parts
/// that moved since the newest green run.
pub fn why_not_same(records: &[(Tier, Record)], digest: &Digest) -> Option<String> {
    let (tier, newest) = records.iter().max_by_key(|(_, r)| r.at)?;
    if newest.digest == digest.total {
        return None;
    }
    let moved = digest.differing(&newest.parts);
    Some(format!(
        "ci: the tree is not the one of the last green run ({}, {}): {} changed",
        tier.name(),
        lock::clock_time(newest.at),
        if moved.is_empty() {
            "nothing a part names".to_owned()
        } else {
            moved.join(", ")
        }
    ))
}

/// What the full tier still has to do after a green quick run on the same tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remainder {
    /// When the quick run passed.
    pub quick_at: u64,
    /// The nextest filter of the non-Qt step (with ci's package selection): the integration
    /// tests quick did not run, and the slow ones of the packages it did.
    pub non_qt_filter: String,
    /// The Qt window tests must run (quick left them out).
    pub qt_window_tests: bool,
    /// qmllint must run (quick did not).
    pub qmllint: bool,
}

/// The part of the full tier a quick record of the same digest does not cover. `quick_filter`
/// builds the very filter quick's non-Qt integration step used for its packages, so the
/// remainder is exactly the rest of `kind(test)`. `None` when there is no such record, or the
/// engine is not nextest on both sides (the `cargo test` fallback selects by binary, not by
/// filter): the full tier then runs as it always does. The unit tests (libraries and
/// binaries) all ran in quick, in both selections.
pub fn remainder(
    records: &[(Tier, Record)],
    digest: &Digest,
    engine: &str,
    quick_filter: &dyn Fn(&[&str]) -> String,
) -> Option<Remainder> {
    let (_, quick) = records
        .iter()
        .filter(|(tier, r)| *tier == Tier::Quick && r.digest == digest.total)
        .max_by_key(|(_, r)| r.at)?;
    if quick.ran.everything || quick.engine != "nextest" || engine != "nextest" {
        return None;
    }
    let packages: Vec<&str> = quick.ran.integration.iter().map(String::as_str).collect();
    let non_qt_filter = if packages.is_empty() {
        "kind(test)".to_owned()
    } else {
        format!("kind(test) & !({})", quick_filter(&packages))
    };
    Some(Remainder {
        quick_at: quick.at,
        non_qt_filter,
        qt_window_tests: !quick.ran.qt_window_tests,
        qmllint: !quick.ran.qmllint,
    })
}

impl Remainder {
    /// What the full run says before it starts.
    pub fn message(&self) -> String {
        let mut todo = vec![
            "the integration tests quick did not run and the slow set".to_owned(),
            "doc tests".to_owned(),
        ];
        if self.qt_window_tests {
            todo.push("the Qt window tests".to_owned());
        }
        if self.qmllint {
            todo.push("qmllint".to_owned());
        }
        todo.push("screenshots".to_owned());
        format!(
            "ci: quick was green at {} on this very tree; rustfmt, clippy, deps-check, i18n and \
             the unit tests do not run again. Running only what it did not: {}.",
            lock::clock_time(self.quick_at),
            todo.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_model::{Item, matches};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory below `target/` (the RAM disk `/tmp` is not for tests).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static COUNT: AtomicU32 = AtomicU32::new(0);
            let dir = crate::root().join("target/tmp").join(format!(
                "xtask-green-{name}-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Scratch(dir)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .arg("-C")
                .arg(&self.0)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A git repository with a file in each place the digest reads, all added.
    fn repo(name: &str) -> Scratch {
        let dir = Scratch::new(name);
        dir.git(&["init", "-q"]);
        dir.write(".gitignore", "/target\n");
        dir.write("crates/a/src/lib.rs", "// a");
        dir.write("README.md", "# readme");
        dir.write("Cargo.lock", "# lock");
        dir.write("rust-toolchain.toml", "[toolchain]\nchannel = \"1.99\"");
        dir.write(".cargo/config.toml", "[env]");
        dir.write("tests/mock-harness/package-lock.json", "{}");
        dir.git(&["add", "-A"]);
        dir
    }

    fn system() -> System {
        System {
            rustc: "rustc 1.99.0\nbinary: rustc\n".to_owned(),
            qt: "6.11.1".to_owned(),
            packages: "breeze=6.8\nkf6-ki18n-dev=6.31".to_owned(),
            harness: "node=24 modules=0123456789abcdef".to_owned(),
            schemes: vec![("BreezeLight.colors".to_owned(), b"[Colors]".to_vec())],
            env: vec![("KOURIER_X".to_owned(), "1".to_owned())],
        }
    }

    fn moved(a: &Digest, b: &Digest) -> Vec<String> {
        b.differing(&a.parts)
    }

    /// Changes exactly one part, and the total.
    fn assert_moves_only(part: &str, a: &Digest, b: &Digest) {
        assert_eq!(moved(a, b), [part], "{part}");
        assert_ne!(a.total, b.total, "{part}: the total follows");
    }

    #[test]
    fn the_same_tree_has_the_same_digest() {
        let dir = repo("same");
        let a = digest(&dir.0, &system()).unwrap();
        let b = digest(&dir.0, &system()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.parts.len(), PART_NAMES.len());
        assert_eq!(a.hex().len(), 16);
    }

    #[test]
    fn a_changed_tracked_file_changes_the_files_part() {
        let dir = repo("tracked");
        let a = digest(&dir.0, &system()).unwrap();
        dir.write("crates/a/src/lib.rs", "// a, edited");
        assert_moves_only("files", &a, &digest(&dir.0, &system()).unwrap());
    }

    #[test]
    fn an_untracked_file_counts_and_an_ignored_one_does_not() {
        let dir = repo("untracked");
        let a = digest(&dir.0, &system()).unwrap();
        dir.write("target/debug/junk", "build output");
        assert_eq!(a, digest(&dir.0, &system()).unwrap(), "ignored: no change");
        dir.write("crates/a/src/new.rs", "// new, never added");
        assert_moves_only("files", &a, &digest(&dir.0, &system()).unwrap());
    }

    #[test]
    fn a_tracked_file_gone_from_the_disk_changes_the_files_part() {
        let dir = repo("gone");
        let a = digest(&dir.0, &system()).unwrap();
        std::fs::remove_file(dir.0.join("crates/a/src/lib.rs")).unwrap();
        assert_moves_only("files", &a, &digest(&dir.0, &system()).unwrap());
    }

    #[test]
    fn the_lock_file_the_toolchain_file_cargo_and_the_harness_lock_have_a_part_each() {
        for (file, part) in [
            ("Cargo.lock", "cargo-lock"),
            ("rust-toolchain.toml", "toolchain-file"),
            (".cargo/config.toml", "dot-cargo"),
            ("tests/mock-harness/package-lock.json", "harness-lock"),
        ] {
            let dir = repo("own");
            let a = digest(&dir.0, &system()).unwrap();
            dir.write(file, "changed");
            assert_moves_only(part, &a, &digest(&dir.0, &system()).unwrap());
        }
    }

    #[test]
    fn the_toolchain_qt_packages_and_schemes_have_a_part_each() {
        let dir = repo("system");
        let base = system();
        let a = digest(&dir.0, &base).unwrap();
        let mut sys = base.clone();
        sys.rustc.push_str("commit-hash: abc\n");
        assert_moves_only("rustc", &a, &digest(&dir.0, &sys).unwrap());
        let mut sys = base.clone();
        sys.qt = "6.12.0".to_owned();
        assert_moves_only("qt", &a, &digest(&dir.0, &sys).unwrap());
        let mut sys = base.clone();
        sys.packages.push_str("\nbreeze-icon-theme=6.9");
        assert_moves_only("packages", &a, &digest(&dir.0, &sys).unwrap());
        let mut sys = base.clone();
        sys.harness = "node=none modules=absent".to_owned();
        assert_moves_only("harness", &a, &digest(&dir.0, &sys).unwrap());
        let mut sys = base;
        sys.schemes[0].1 = b"[Colors:View]".to_vec();
        assert_moves_only("schemes", &a, &digest(&dir.0, &sys).unwrap());
    }

    #[test]
    fn the_environment_counts_by_its_kourier_and_rust_variables_only() {
        assert!(is_input_var("KOURIER_NO_NEXTEST"));
        assert!(is_input_var("RUSTFLAGS"));
        assert!(is_input_var("RUST_LOG"));
        assert!(!is_input_var("PATH"));
        assert!(!is_input_var("HOME"));
        assert!(!is_input_var(lock::HELD_ENV), "it differs per holder");
        assert!(!is_input_var("RUST_RECURSION_COUNT"));
        assert!(!is_input_var("RUSTUP_TOOLCHAIN"));
        let vars = [
            ("PATH", "/bin"),
            ("RUST_LOG", "debug"),
            ("KOURIER_B", "2"),
            ("KOURIER_A", "1"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        assert_eq!(
            env_inputs(vars),
            [
                ("KOURIER_A".to_owned(), "1".to_owned()),
                ("KOURIER_B".to_owned(), "2".to_owned()),
                ("RUST_LOG".to_owned(), "debug".to_owned()),
            ]
        );
        let dir = repo("env");
        let base = system();
        let a = digest(&dir.0, &base).unwrap();
        let mut sys = base.clone();
        sys.env.push(("RUST_LOG".to_owned(), "debug".to_owned()));
        assert_moves_only("env", &a, &digest(&dir.0, &sys).unwrap());
        let mut sys = base;
        sys.env[0].1 = "2".to_owned();
        assert_moves_only("env", &a, &digest(&dir.0, &sys).unwrap());
    }

    /// F9: a docs-only edit moves the part of the neutral files and not the total, so a record
    /// of the tree before it still covers the tree after it; the runbook the smoke tool's tests
    /// read, the packaging files the installer's tests read and the harness are not neutral.
    #[test]
    fn a_docs_only_edit_moves_the_neutral_part_and_not_the_total() {
        let dir = repo("neutral");
        dir.write("docs/architecture.md", "# a");
        dir.write("docs/smoke-run.md", "# runbook");
        dir.write("docs/whatsapp-run.md", "# runbook");
        dir.write("docs/telegram-run.md", "# runbook");
        dir.write("packaging/app.desktop", "[Desktop Entry]");
        dir.write("tests/mock-harness/README.md", "# harness");
        dir.git(&["add", "-A"]);
        let a = digest(&dir.0, &system()).unwrap();
        for file in ["README.md", "docs/architecture.md", ".github/ci.md"] {
            dir.write(file, "# edited");
            let b = digest(&dir.0, &system()).unwrap();
            assert_eq!(moved(&a, &b), [NEUTRAL_PART], "{file}");
            assert_eq!(a.total, b.total, "{file}: the same tree for a test");
            // A record of the first tree covers the second.
            let rec = Record {
                digest: a.total,
                parts: a.parts.clone(),
                at: 5,
                engine: "nextest".to_owned(),
                ran: Ran::everything(),
            };
            assert!(
                covering(&[(Tier::Full, rec)], &b, &Ran::everything()).is_some(),
                "{file}"
            );
        }
        // These files are read by tests: they are inputs.
        for file in [
            "docs/smoke-run.md",
            "docs/whatsapp-run.md",
            "docs/telegram-run.md",
            "packaging/app.desktop",
            "tests/mock-harness/README.md",
        ] {
            let before = digest(&dir.0, &system()).unwrap();
            dir.write(file, "# edited, and a test reads it");
            let after = digest(&dir.0, &system()).unwrap();
            assert_moves_only("files", &before, &after);
        }
    }

    #[test]
    fn a_script_made_executable_moves_the_files_part() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo("mode");
        dir.write("crates/a/run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(
            dir.0.join("crates/a/run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let a = digest(&dir.0, &system()).unwrap();
        std::fs::set_permissions(
            dir.0.join("crates/a/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_moves_only("files", &a, &digest(&dir.0, &system()).unwrap());
    }

    #[test]
    fn a_backtrace_setting_is_no_input_but_a_log_filter_is() {
        assert!(!is_input_var("RUST_BACKTRACE"));
        assert!(is_input_var("RUST_LOG"));
        assert!(is_input_var("RUST_MIN_STACK"));
    }

    /// F1: the harness part says which Node the harness search finds and what `npm ci` installed.
    #[test]
    fn the_harness_state_names_the_node_found_and_the_installed_modules() {
        use std::ffi::OsStr;
        let dir = Scratch::new("harness");
        const NVM: &str = "/home/x/.nvm/versions/node/v24.21.0/bin/node";
        let answers = |known: &'static [(&'static str, u32)]| {
            move |p: &Path| {
                known
                    .iter()
                    .find(|(path, _)| Path::new(path) == p)
                    .map(|(_, major)| *major)
            }
        };
        let home = Some(OsStr::new("/home/x"));
        assert_eq!(
            harness_state(&dir.0, None, home, &answers(&[(NVM, 24)])),
            "node=24 modules=absent"
        );
        // The search takes the first Node of 24 or newer: an old nvm Node is passed over.
        assert_eq!(
            harness_state(&dir.0, None, home, &answers(&[(NVM, 22), ("node", 25)])),
            "node=25 modules=absent"
        );
        // No Node 24 anywhere: the mock tests cannot run (and now fail instead of skipping).
        assert_eq!(
            harness_state(&dir.0, None, home, &answers(&[("node", 22)])),
            "node=none modules=absent"
        );
        // KOURIER_NODE is the only one asked, as the harness does.
        assert_eq!(
            harness_state(
                &dir.0,
                Some(OsStr::new("/opt/node")),
                home,
                &answers(&[(NVM, 24), ("/opt/node", 22)])
            ),
            "node=none modules=absent"
        );
        dir.write(
            "tests/mock-harness/node_modules/.package-lock.json",
            "{\"a\": 1}",
        );
        let first = harness_state(&dir.0, None, home, &answers(&[(NVM, 24)]));
        assert!(
            first.starts_with("node=24 modules=") && !first.ends_with("absent"),
            "{first}"
        );
        dir.write(
            "tests/mock-harness/node_modules/.package-lock.json",
            "{\"a\": 2}",
        );
        assert_ne!(
            first,
            harness_state(&dir.0, None, home, &answers(&[(NVM, 24)]))
        );
    }

    /// Not a test: asks the machine the way `ci` does, in a child whose environment the test
    /// below chose, and prints the answer.
    #[test]
    fn query_child() {
        if std::env::var_os("KOURIER_T5_PROBE").is_none() {
            return;
        }
        let sys = System::query(&crate::root(), &[]).expect("the query answers");
        println!();
        println!("probe-env {}", serde_json::to_string(&sys.env).unwrap());
        println!("probe-qt {}", sys.qt);
        println!(
            "probe-packages {}",
            serde_json::to_string(&sys.packages).unwrap()
        );
        println!("probe-harness {}", sys.harness);
    }

    /// F5 and F6: `System::query` itself, run for real: a `KOURIER_*` variable is in the digest's
    /// inputs, `RUST_BACKTRACE` is not, a variable that is not Unicode does not abort `ci`, and
    /// Qt, the packages (Qt's among them) and the harness are really asked.
    #[test]
    fn the_real_query_reads_the_environment_and_the_machine() {
        use std::os::unix::ffi::OsStrExt;
        let out = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                "green::tests::query_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("KOURIER_T5_PROBE", "1")
            .env("KOURIER_T5_BAD", std::ffi::OsStr::from_bytes(b"a\xffb"))
            .env("RUST_BACKTRACE", "1")
            .output()
            .expect("run the child");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "the query died (a non-UTF-8 variable?): {text} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} line in {text}"))
                .trim()
                .to_owned()
        };
        let env: Vec<(String, String)> = serde_json::from_str(&line("probe-env ")).unwrap();
        assert!(
            env.contains(&("KOURIER_T5_PROBE".to_owned(), "1".to_owned())),
            "{env:?}"
        );
        assert!(
            env.contains(&("KOURIER_T5_BAD".to_owned(), "a\u{fffd}b".to_owned())),
            "{env:?}"
        );
        assert!(env.iter().all(|(k, _)| k != "RUST_BACKTRACE"), "{env:?}");
        assert!(env.iter().all(|(k, _)| k != lock::HELD_ENV), "{env:?}");
        let qt = line("probe-qt ");
        assert!(qt.starts_with(|c: char| c.is_ascii_digit()), "Qt: {qt}");
        let packages: String = serde_json::from_str(&line("probe-packages ")).unwrap();
        assert_ne!(
            packages, "unavailable",
            "dpkg-query answers on this machine"
        );
        for prefix in ["libqt6", "qt6-"] {
            assert!(
                packages.lines().any(|l| l.starts_with(prefix)),
                "no {prefix}* package in {packages}"
            );
        }
        assert!(
            packages
                .lines()
                .any(|l| l.starts_with("kf6-") || l.starts_with("libkf6")),
            "no KF6 package in {packages}"
        );
        assert!(line("probe-harness ").starts_with("node="));
    }

    fn ran(packages: &[&str], window: bool, qmllint: bool) -> Ran {
        Ran {
            everything: false,
            integration: packages.iter().map(|p| (*p).to_owned()).collect(),
            qt_window_tests: window,
            qmllint,
            slow: false,
        }
    }

    fn digest_of(total: u64) -> Digest {
        Digest {
            parts: BTreeMap::from([("files".to_owned(), total)]),
            total,
        }
    }

    fn record(total: u64, at: u64, engine: &str, ran: Ran) -> Record {
        Record {
            digest: total,
            parts: digest_of(total).parts,
            at,
            engine: engine.to_owned(),
            ran,
        }
    }

    #[test]
    fn a_record_covers_a_request_only_when_it_ran_at_least_as_much() {
        let quick = ran(&["kourier-core", "kourier-app"], false, true);
        // The same or less is covered.
        assert!(ran(&["kourier-core"], false, false).covered_by(&quick));
        assert!(ran(&["kourier-core", "kourier-app"], false, true).covered_by(&quick));
        assert!(ran(&[], false, false).covered_by(&quick));
        // More is not: another package, the window tests, the slow set.
        assert!(!ran(&["kourier-signal"], false, false).covered_by(&quick));
        assert!(!ran(&["kourier-core"], true, false).covered_by(&quick));
        // qmllint asked for and not run.
        assert!(!ran(&["kourier-core"], false, true).covered_by(&ran(
            &["kourier-core"],
            false,
            false
        )));
        let mut slow = ran(&["kourier-core"], false, false);
        slow.slow = true;
        assert!(!slow.covered_by(&quick));
        // The full tier is covered only by the full tier; a full record covers everything.
        assert!(!Ran::everything().covered_by(&quick));
        assert!(Ran::everything().covered_by(&Ran::everything()));
        assert!(quick.covered_by(&Ran::everything()));
        assert!(slow.covered_by(&Ran::everything()));
    }

    #[test]
    fn a_repeat_on_the_same_digest_is_covered_and_another_digest_is_not() {
        let records = vec![
            (
                Tier::Quick,
                record(7, 100, "nextest", ran(&["a"], false, false)),
            ),
            (Tier::Full, record(7, 50, "nextest", Ran::everything())),
        ];
        let same = digest_of(7);
        // Quick for `a`: both cover it; the newer wins.
        assert_eq!(
            covering(&records, &same, &ran(&["a"], false, false)),
            Some(Skip {
                tier: Tier::Quick,
                at: 100
            })
        );
        // Quick for `b`: only the full record covers it.
        assert_eq!(
            covering(&records, &same, &ran(&["b"], false, false)),
            Some(Skip {
                tier: Tier::Full,
                at: 50
            })
        );
        // The full tier: only the full record.
        assert_eq!(
            covering(&records, &same, &Ran::everything()),
            Some(Skip {
                tier: Tier::Full,
                at: 50
            })
        );
        // Another tree: nothing.
        assert_eq!(
            covering(&records, &digest_of(8), &ran(&["a"], false, false)),
            None
        );
        // A quick record alone never covers the full tier.
        assert_eq!(covering(&records[..1], &same, &Ran::everything()), None);
    }

    #[test]
    fn the_skip_says_when_and_how_to_force_it() {
        let text = skip_message(&Skip {
            tier: Tier::Quick,
            at: 1_700_000_000,
        });
        assert!(text.starts_with("ci: green at "), "{text}");
        assert!(text.contains("(same tree)"), "{text}");
        assert!(text.contains("--force"), "{text}");
        assert!(text.contains("quick"), "{text}");
    }

    #[test]
    fn a_miss_names_the_parts_that_moved() {
        let mut old = digest_of(1);
        old.parts.insert("env".to_owned(), 5);
        let records = vec![(Tier::Quick, record(1, 100, "nextest", Ran::default()))];
        let mut new = digest_of(2);
        new.parts.insert("env".to_owned(), 6);
        let why = why_not_same(&records, &new).unwrap();
        assert!(why.contains("files") && why.contains("env"), "{why}");
        assert_eq!(why_not_same(&records, &digest_of(1)), None, "the same tree");
        assert_eq!(
            why_not_same(&[], &new),
            None,
            "no record, nothing to compare"
        );
    }

    #[test]
    fn a_record_is_read_back_as_written() {
        let rec = record(
            0xabc,
            1_700_000_000,
            "nextest",
            ran(&["a", "b"], true, false),
        );
        assert_eq!(Record::parse(&rec.to_json()), Some(rec.clone()));
        let mut full = rec;
        full.ran = Ran::everything();
        assert_eq!(Record::parse(&full.to_json()), Some(full));
        // Anything else is no record.
        assert_eq!(Record::parse(""), None);
        assert_eq!(Record::parse("{}"), None);
        let other = Record::parse(
            &record(1, 1, "x", Ran::default())
                .to_json()
                .replace("\"format\": 1", "\"format\": 2"),
        );
        assert_eq!(other, None, "another layout is not read");
    }

    #[test]
    fn only_a_green_run_on_an_unmoved_tree_writes_a_record() {
        let dir = Scratch::new("write");
        let a = digest_of(1);
        let rec = record_green(
            &dir.0,
            Tier::Quick,
            &a,
            &digest_of(1),
            "nextest",
            ran(&["a"], false, false),
            10,
        )
        .unwrap()
        .expect("written");
        assert_eq!(load(&dir.0), [(Tier::Quick, rec)]);
        // The tree moved under the run: nothing is written, the old record stays.
        let moved = record_green(
            &dir.0,
            Tier::Quick,
            &digest_of(2),
            &digest_of(3),
            "nextest",
            Ran::everything(),
            20,
        )
        .unwrap();
        assert_eq!(moved, None);
        assert_eq!(load(&dir.0).len(), 1);
        assert_eq!(load(&dir.0)[0].1.digest, 1);
        assert_eq!(load(&dir.0)[0].1.at, 10);
    }

    #[test]
    fn two_green_runs_on_one_tree_are_merged() {
        let dir = Scratch::new("merge");
        let d = digest_of(4);
        record_green(
            &dir.0,
            Tier::Quick,
            &d,
            &d,
            "nextest",
            ran(&["a"], false, false),
            10,
        )
        .unwrap();
        let merged = record_green(
            &dir.0,
            Tier::Quick,
            &d,
            &d,
            "nextest",
            ran(&["b"], true, true),
            20,
        )
        .unwrap()
        .unwrap();
        assert_eq!(merged.ran, ran(&["a", "b"], true, true));
        assert_eq!(merged.at, 20);
        // Another engine does not merge: what one engine ran says nothing of the other.
        let other = record_green(
            &dir.0,
            Tier::Quick,
            &d,
            &d,
            "cargo-test",
            ran(&["c"], false, false),
            30,
        )
        .unwrap()
        .unwrap();
        assert_eq!(other.ran, ran(&["c"], false, false));
    }

    #[test]
    fn a_failed_run_takes_the_records_of_its_tree_away_and_only_those() {
        let dir = Scratch::new("forget");
        let (a, b) = (digest_of(1), digest_of(2));
        record_green(
            &dir.0,
            Tier::Quick,
            &a,
            &a,
            "nextest",
            ran(&["x"], false, false),
            10,
        )
        .unwrap();
        record_green(&dir.0, Tier::Full, &b, &b, "nextest", Ran::everything(), 11).unwrap();
        assert_eq!(forget(&dir.0, &a), [Tier::Quick]);
        let left = load(&dir.0);
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].0, left[0].1.digest), (Tier::Full, 2));
        assert_eq!(forget(&dir.0, &a), [], "nothing left to forget");
    }

    /// Quick's non-Qt integration filter for `packages` over a made-up slow set (what T5d puts
    /// into `runner::integration_select`).
    fn quick_filter(packages: &[&str]) -> String {
        let any: Vec<String> = packages.iter().map(|p| format!("package(={p})")).collect();
        format!(
            "kind(test) & ({}) & !((package(=a) & test(=crash::every_point)) | \
             (package(=c) & test(=transfer::deadline)))",
            any.join(" | ")
        )
    }

    fn universe() -> Vec<Item> {
        let item = |package, name, kind| Item {
            package,
            name,
            kind,
        };
        vec![
            item("a", "unit_one", "lib"),
            item("a", "crash::every_point", "test"),
            item("a", "crash::one", "test"),
            item("b", "unit_two", "lib"),
            item("b", "plain::one", "test"),
            item("b", "plain::two", "test"),
            item("c", "unit_three", "bin"),
            item("c", "transfer::deadline", "test"),
            item("c", "transfer::fast", "test"),
            item("d", "other::one", "test"),
            // Same test name in two packages: the slow name belongs to `a` only.
            item("d", "crash::every_point", "test"),
        ]
    }

    #[test]
    fn the_remainder_is_exactly_the_integration_tests_quick_did_not_run() {
        let records = vec![(
            Tier::Quick,
            record(9, 100, "nextest", ran(&["a", "b"], true, true)),
        )];
        let rem =
            remainder(&records, &digest_of(9), "nextest", &quick_filter).expect("a remainder");
        let all: Vec<Item> = universe();
        let ran_in_quick: Vec<&Item> = all
            .iter()
            .filter(|i| matches(&quick_filter(&["a", "b"]), i))
            .collect();
        let left: Vec<&Item> = all
            .iter()
            .filter(|i| matches(&rem.non_qt_filter, i))
            .collect();
        let names = |items: &[&Item]| -> Vec<String> {
            items
                .iter()
                .map(|i| format!("{}::{}", i.package, i.name))
                .collect()
        };
        // Quick ran the integration tests of a and b without the slow one of a.
        assert_eq!(
            names(&ran_in_quick),
            ["a::crash::one", "b::plain::one", "b::plain::two"]
        );
        // The remainder: the slow one of a, every test of c and d.
        assert_eq!(
            names(&left),
            [
                "a::crash::every_point",
                "c::transfer::deadline",
                "c::transfer::fast",
                "d::other::one",
                "d::crash::every_point",
            ]
        );
        // Together exactly `kind(test)`, with nothing twice.
        for item in &all {
            let integration = item.kind == "test";
            let in_quick = matches(&quick_filter(&["a", "b"]), item);
            let in_rest = matches(&rem.non_qt_filter, item);
            assert_eq!(in_quick || in_rest, integration, "{item:?}");
            assert!(!(in_quick && in_rest), "{item:?} would run twice");
        }
        assert!(!rem.qt_window_tests, "quick ran them");
        assert!(!rem.qmllint, "quick ran it");
        assert_eq!(rem.quick_at, 100);
    }

    #[test]
    fn a_quick_run_that_ran_no_integration_tests_leaves_all_of_them() {
        let records = vec![(
            Tier::Quick,
            record(9, 100, "nextest", ran(&[], false, false)),
        )];
        let rem = remainder(&records, &digest_of(9), "nextest", &quick_filter).unwrap();
        assert_eq!(rem.non_qt_filter, "kind(test)");
        assert!(rem.qt_window_tests && rem.qmllint);
    }

    #[test]
    fn no_remainder_without_a_quick_record_of_this_tree_on_nextest() {
        let quick = |engine: &str, total| {
            vec![(
                Tier::Quick,
                record(total, 1, engine, ran(&["a"], false, false)),
            )]
        };
        // Another tree, another engine on either side, a full record, no record: no shortcut.
        assert_eq!(
            remainder(
                &quick("nextest", 1),
                &digest_of(2),
                "nextest",
                &quick_filter
            ),
            None
        );
        assert_eq!(
            remainder(
                &quick("cargo-test", 1),
                &digest_of(1),
                "nextest",
                &quick_filter
            ),
            None
        );
        assert_eq!(
            remainder(
                &quick("nextest", 1),
                &digest_of(1),
                "cargo-test",
                &quick_filter
            ),
            None
        );
        let full = vec![(Tier::Full, record(1, 1, "nextest", Ran::everything()))];
        assert_eq!(
            remainder(&full, &digest_of(1), "nextest", &quick_filter),
            None
        );
        assert_eq!(
            remainder(&[], &digest_of(1), "nextest", &quick_filter),
            None
        );
        // A quick record that turned into a full run covers everything: nothing is left, and
        // `covering` (not `remainder`) is what ends such a run.
        let turned = vec![(Tier::Quick, record(1, 1, "nextest", Ran::everything()))];
        assert_eq!(
            remainder(&turned, &digest_of(1), "nextest", &quick_filter),
            None
        );
    }

    #[test]
    fn the_full_run_after_quick_says_what_it_runs() {
        let rem = Remainder {
            quick_at: 1_700_000_000,
            non_qt_filter: "kind(test)".to_owned(),
            qt_window_tests: true,
            qmllint: false,
        };
        let text = rem.message();
        assert!(text.contains("quick was green at"), "{text}");
        assert!(
            text.contains("slow set") && text.contains("doc tests"),
            "{text}"
        );
        assert!(text.contains("Qt window tests"), "{text}");
        assert!(!text.contains("qmllint"), "quick ran it: {text}");
        assert!(text.contains("screenshots"), "{text}");
    }
}
