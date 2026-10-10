//! `cargo xtask dist`: the archives of a release, made on this machine from a commit, and the
//! proof that they are fit to be made public.
//!
//! ```text
//! target/dist/kourier-<version>-linux-<arch>[-suffix].tar.gz   the installed tree (one top directory)
//! target/dist/kourier-<version>-source[-suffix].tar.gz         `git archive <the commit>`
//! target/dist/SHA256SUMS                                       over both
//! target/dist/NOT-READY.txt                                    what still blocks a release
//! ```
//!
//! Nothing here pushes, uploads or publishes: the archives stay on this disk and the
//! maintainer attaches them to a release by hand (`RELEASING.md`).
//!
//! The suffix is the grade ([`Grade`]): only a run with `--release` that found nothing in the
//! way writes the bare names; every other archive ends in `-unreleased` (a working tree with
//! changes: `-dirty`), in its file name and in its top directory.
//!
//! What it does, in order:
//!
//! 1. Removes what an earlier run left in the output directory ([`clear_outputs`]): every
//!    `kourier-*.tar.gz`, `SHA256SUMS`, `NOT-READY.txt` and the stage directory, and nothing
//!    else. This is the first act, before the options are judged.
//! 2. Refuses a working tree with changes (`--allow-dirty` lets it through and marks every
//!    name `-dirty`). The time stamp of everything packed is the commit's time.
//! 3. Writes `NOT-READY.txt` ([`NotReady`]): blanks the maintainer has to fill, the placeholder
//!    application id, tracked files that name a private path, a missing repository address.
//!    With `--release` each is an error and nothing is built.
//! 4. Builds the release binary into its own target directory (`target/dist-build`, which the
//!    development builds never touch) with every absolute path mapped away ([`BuildPlan`]),
//!    without the test network (`--no-default-features --features signal`), and with the
//!    variables that would change the output cleared ([`build_env_removals`]). A binary holds
//!    the paths of its sources in its panic messages and C and C++ code holds them through
//!    `__FILE__`. When the build is done the commit and the state of the tree are read again
//!    and must be the ones of the start, and a stamp ([`Stamp`]) is written beside the binary:
//!    the commit, the dirty flag, the features, the Qt, KDE Frameworks and rustc versions and
//!    the binary's checksum. Packing refuses a binary whose stamp does not name this commit
//!    and state (`--no-build` too). The binary is searched for private names ([`private_needles`],
//!    [`extra_needles`], [`scan_bytes`], [`zstd_frames`]); what remains is an error unless
//!    `--allow-paths` is given.
//! 5. Stages the installed tree with the code of `cargo xtask install` (without the link
//!    handlers: the public archive does not compete with an installed Signal Desktop), adds the
//!    licence texts (`cargo about`, then the libraries that are linked in but are no crate:
//!    [`bundled_libraries`]), a generated `README.txt` and the requirements read from the
//!    binary ([`Requirements`]).
//! 6. Packs it deterministically ([`pack_tree`]), packs `git archive <the commit>` and writes
//!    `SHA256SUMS`.
//! 7. Unpacks what it packed and checks it ([`verify`]): the tree holds exactly the expected
//!    paths, `bin/kourier --version` prints the version and `--fixture` is refused, the binary
//!    is no measuring build, the desktop file claims no link scheme, the notices name SQLCipher
//!    and BoringSSL, `desktop-file-validate` and `appstreamcli validate` pass, no file holds a
//!    private name, and the source archive is the commit with plain modes.
//!
//! The archives are byte for byte the same when the same stage is packed again (tested); the
//! BUILD is not reproducible in that sense, and nothing here claims it is.

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::install::{self, APP_ID, BINARY, Sources};
use super::{cargo, root, run, target_dir};

/// The application id while it is a placeholder (see CLAUDE.md, Rules).
const PLACEHOLDER_APP_ID: &str = "app.kourier.Kourier";

/// The URI of Kourier's own QML module: not a system requirement.
const OWN_QML_MODULE: &str = "app.kourier";

/// The mark of a blank the maintainer has to fill (spelled in two pieces so that this file
/// does not count itself).
const BLANK_MARK: &str = concat!("TODO", "(maintainer)");

/// The build directory of `dist`, below the workspace (a link to the hard disk on the
/// maintainer's machine: a cold release build is 15 GB or more).
const BUILD_DIR: &str = "target/dist-build";

/// The default output directory, below the workspace.
const DEFAULT_OUT: &str = "target/dist";

/// Where the mapped paths point in the binary.
const MAPPED_WORKSPACE: &str = "/kourier";
const MAPPED_CARGO_HOME: &str = "/cargo";
const MAPPED_BUILD_DIR: &str = "/kourier-build";
const MAPPED_TOOLCHAIN: &str = "/rust-toolchain";

/// Where `dist` stages the tree and unpacks it again, below the output directory. Hidden and
/// named for the tool, because both are removed at the start of a run and at its end.
const STAGE_DIR: &str = ".dist-stage";
const VERIFY_DIR: &str = ".dist-verify";

/// The cargo arguments that choose the features of the public binary: the Signal backend and
/// not the scripted test network (`--fixture`, `--fixture-link`, the "Test network" on the
/// welcome page; `crates/kourier/Cargo.toml`).
const BUILD_FEATURE_ARGS: [&str; 3] = ["--no-default-features", "--features", "signal"];

/// What the stamp records for [`BUILD_FEATURE_ARGS`].
fn build_features() -> String {
    BUILD_FEATURE_ARGS.join(" ")
}

/// The file beside the binary that says what the binary was built from.
const STAMP_FILE: &str = "kourier.dist-stamp";

/// What `bin/kourier --fixture` says when the test network is not built in
/// (`AppError::FixtureUnavailable` in `kourier-app`; a test compares).
const FIXTURE_REFUSAL: &str = "fixture mode needs a build with the `fake` feature";

/// The notices the packed `THIRD-PARTY.html` must hold for what is linked in but is no crate.
const REQUIRED_NOTICE_WORDS: [&str; 2] = ["Zetetic", "BoringSSL"];

/// The former name of the project, in pieces so that this file does not hold it; it must not
/// be in the binary or in any packed file (compared without regard to case).
const FORMER_NAME: [&str; 6] = ["a", "p", "a", "c", "h", "i"];

/// Where the About data sit (the repository address is read from them).
const ABOUT_SOURCE: &str = "crates/kourier-app/src/about.rs";

/// The Qt Quick Controls style the application runs in.
const DESKTOP_STYLE_MODULE: &str = "org.kde.desktop";

/// The first KDE Frameworks library: its version is the Frameworks' version.
const KF6_PROBE: &str = "KF6CoreAddons";

/// How long `bin/kourier --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// User names that fixtures and examples use; `/home/<one of them>` names nobody. A name of
/// one character is one as well.
const PLACEHOLDER_USERS: [&str; 11] = [
    "user", "me", "alice", "bob", "anna", "you", "name", "username", "example", "runner", "someone",
];

/// Roots below a home directory that hold the maintainer's own work: a tracked file that
/// names one of them tells where the maintainer keeps it (spelled in two pieces so that this
/// file does not name it).
const PRIVATE_TILDE_ROOTS: [&str; 1] = [concat!("~/", "projects")];

const USAGE: &str =
    "usage: cargo xtask dist [--out <dir>] [--release] [--allow-dirty] [--allow-paths] [--no-build]

  --out <dir>     write the archives below <dir> (default: target/dist)
  --release       the real thing: every finding in NOT-READY.txt is an error and nothing is
                  built or left behind; only this writes names without a suffix; refuses
                  --allow-dirty, --allow-paths and --no-build
  --allow-dirty   pack a working tree with uncommitted changes (every name ends in -dirty;
                  the source archive is still the last commit)
  --allow-paths   keep going when the binary still holds a private name (the home directory,
                  the user name, /media/, the host name, git's name and address)
  --no-build      pack the binary that target/dist-build holds, if its stamp names this
                  commit and state

Without --release the names end in -unreleased. The output directory is emptied of earlier
archives (kourier-*.tar.gz, SHA256SUMS, NOT-READY.txt) first.";

// ---------------------------------------------------------------------------------------
// Options and names

/// What `dist` was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Where the archives go.
    pub out: PathBuf,
    /// Every finding of `NOT-READY.txt` is an error.
    pub release: bool,
    /// Pack a tree with uncommitted changes.
    pub allow_dirty: bool,
    /// Keep going when private paths remain in the binary.
    pub allow_paths: bool,
    /// Pack the binary that is there.
    pub no_build: bool,
}

/// Parses the arguments of `dist`; a relative `--out` is relative to `cwd`.
pub fn parse_options(args: &[String], cwd: &Path) -> Result<Options> {
    let opts = parse_flags(args, cwd)?;
    opts.check_conflicts()?;
    Ok(opts)
}

/// The output directory the arguments name (the last `--out`, else the default), found without
/// judging anything else: `dist` empties it before the options are checked.
pub fn out_dir_of(args: &[String], cwd: &Path) -> Result<PathBuf> {
    let mut out = root().join(DEFAULT_OUT);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--out" {
            out = out_dir(it.next().context("--out needs a directory")?, cwd)?;
        } else if let Some(dir) = arg.strip_prefix("--out=") {
            out = out_dir(dir, cwd)?;
        }
    }
    Ok(out)
}

/// The flags, without the check of which exclude each other ([`Options::check_conflicts`]).
fn parse_flags(args: &[String], cwd: &Path) -> Result<Options> {
    let mut opts = Options {
        out: root().join(DEFAULT_OUT),
        release: false,
        allow_dirty: false,
        allow_paths: false,
        no_build: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--release" => opts.release = true,
            "--allow-dirty" => opts.allow_dirty = true,
            "--allow-paths" => opts.allow_paths = true,
            "--no-build" => opts.no_build = true,
            "--out" => {
                let dir = it.next().context("--out needs a directory")?;
                opts.out = out_dir(dir, cwd)?;
            }
            other => match other.strip_prefix("--out=") {
                Some(dir) => opts.out = out_dir(dir, cwd)?,
                None => bail!("unknown argument {other:?}\n{USAGE}"),
            },
        }
    }
    Ok(opts)
}

impl Options {
    /// `--release` excludes the flags that let something through.
    pub fn check_conflicts(&self) -> Result<()> {
        if self.release {
            for (flag, set) in [
                ("--allow-dirty", self.allow_dirty),
                ("--allow-paths", self.allow_paths),
                ("--no-build", self.no_build),
            ] {
                if set {
                    bail!(
                        "--release and {flag} exclude each other: a release comes from a clean commit, built now, without private paths\n{USAGE}"
                    );
                }
            }
        }
        Ok(())
    }
}

fn out_dir(dir: &str, cwd: &Path) -> Result<PathBuf> {
    if dir.is_empty() {
        bail!("--out needs a directory\n{USAGE}");
    }
    Ok(std::path::absolute(cwd.join(dir))?)
}

/// What the archives of this run are: only a passed `--release` makes a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grade {
    /// `--release`, a clean tree, no finding: the bare names.
    Release,
    /// Everything else from a clean tree: `-unreleased`.
    Unreleased,
    /// A working tree with changes (`--allow-dirty`): `-dirty`.
    Dirty,
}

impl Grade {
    /// The suffix of every name.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Release => "",
            Self::Unreleased => "-unreleased",
            Self::Dirty => "-dirty",
        }
    }
}

/// The grade of a run: `release_passed` is a `--release` run that found nothing in the way.
pub fn grade_of(dirty: bool, release_passed: bool) -> Grade {
    if dirty {
        Grade::Dirty
    } else if release_passed {
        Grade::Release
    } else {
        Grade::Unreleased
    }
}

/// The names of what is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    /// `kourier-<version>-linux-<arch>[-suffix]`: the top directory of the binary archive.
    pub stage: String,
    /// `<stage>.tar.gz`.
    pub binary_archive: String,
    /// `kourier-<version>[-suffix]`: the top directory of the source archive.
    pub source_dir: String,
    /// `kourier-<version>-source[-suffix].tar.gz`.
    pub source_archive: String,
}

/// The names for `version` on `arch`: the grade's suffix is in the file name and in the top
/// directory of both archives.
pub fn archive_names(version: &str, arch: &str, grade: Grade) -> Names {
    let suffix = grade.suffix();
    let stage = format!("kourier-{version}-linux-{arch}{suffix}");
    Names {
        binary_archive: format!("{stage}.tar.gz"),
        stage,
        source_dir: format!("kourier-{version}{suffix}"),
        source_archive: format!("kourier-{version}-source{suffix}.tar.gz"),
    }
}

/// `[workspace.package] version` of the root `Cargo.toml`.
pub fn workspace_version(cargo_toml: &str) -> Result<String> {
    let mut in_table = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_table = line == "[workspace.package]";
        } else if in_table && let Some(rest) = line.strip_prefix("version") {
            let value = rest.trim_start().strip_prefix('=').map(str::trim);
            if let Some(v) = value.and_then(|v| v.strip_prefix('"')?.split_once('"')) {
                return Ok(v.0.to_owned());
            }
        }
    }
    bail!("no version in [workspace.package] of Cargo.toml")
}

// ---------------------------------------------------------------------------------------
// Git

/// The commit being packed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Commit {
    hash: String,
    /// Seconds since the epoch: the time stamp of everything packed.
    time: i64,
    /// `YYYY-MM-DD`.
    date: String,
    /// `git status --porcelain`, one entry per line.
    changes: Vec<String>,
}

fn git(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root()).args(args);
    stdout_of(&mut cmd, &format!("git {}", args.join(" ")))
}

/// The script of the source archive that fetches the dependencies' sources: README.txt of the
/// binary archive names it, so it must be in the tree.
const FETCH_SCRIPT: &str = "scripts/fetch-dependency-sources.sh";

/// Splits the tracked files into those a source archive holds and those it leaves out: the
/// files with the attribute `export-ignore` (`.gitattributes`: the working notes that stay
/// private). `git archive` leaves the same files out by itself.
fn exported_files(tracked: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut hidden = Vec::new();
    // (In pieces: a command line has a length limit.)
    for part in tracked.chunks(200) {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(root())
            .args(["check-attr", "-z", "export-ignore", "--"])
            .args(part);
        hidden.extend(export_ignored(&stdout_of(&mut cmd, "git check-attr")?));
    }
    let exported = tracked
        .iter()
        .filter(|f| !hidden.contains(f))
        .cloned()
        .collect();
    Ok((exported, hidden))
}

/// The paths that `git check-attr -z export-ignore` says are set: its output is
/// `path NUL attribute NUL value NUL` for every path asked about.
fn export_ignored(output: &str) -> Vec<String> {
    let fields: Vec<&str> = output.split('\0').collect();
    fields
        .chunks(3)
        .filter(|f| f.len() == 3 && f[1] == "export-ignore" && f[2] == "set")
        .map(|f| f[0].to_owned())
        .collect()
}

/// Runs `cmd` and returns its standard output; a failure carries its standard error.
fn stdout_of(cmd: &mut Command, what: &str) -> Result<String> {
    let out = cmd.output().with_context(|| format!("run {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git rev-parse HEAD` and `git status --porcelain` now.
fn git_state() -> Result<(String, Vec<String>)> {
    let hash = git(&["rev-parse", "HEAD"])?.trim().to_owned();
    let changes = git(&["status", "--porcelain"])?
        .lines()
        .map(str::to_owned)
        .collect();
    Ok((hash, changes))
}

/// Fails when the commit or the changes of the working tree are not the ones `dist` started
/// with: a binary built while they moved belongs to no commit.
fn same_state(start: &Commit, hash: &str, changes: &[String]) -> Result<()> {
    if start.hash != hash {
        bail!(
            "HEAD moved while the binary was built: {} at the start, {} now",
            &start.hash[..start.hash.len().min(12)],
            &hash[..hash.len().min(12)]
        );
    }
    if start.changes != changes {
        bail!(
            "the working tree changed while the binary was built (git status --porcelain was {} entries at the start, {} now); the binary may hold sources that are not the commit's",
            start.changes.len(),
            changes.len()
        );
    }
    Ok(())
}

/// Whether `text` is a full commit hash (40 or 64 hex digits).
fn is_commit_hash(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit())
}

fn read_commit() -> Result<Commit> {
    let info = git(&["log", "-1", "--format=%H%n%ct%n%cs", "HEAD"])?;
    let mut lines = info.lines();
    let hash = lines.next().context("git log: no hash")?.to_owned();
    let time = lines
        .next()
        .and_then(|t| t.parse().ok())
        .context("git log: no commit time")?;
    let date = lines.next().context("git log: no date")?.to_owned();
    let changes = git(&["status", "--porcelain"])?
        .lines()
        .map(str::to_owned)
        .collect();
    Ok(Commit {
        hash,
        time,
        date,
        changes,
    })
}

// ---------------------------------------------------------------------------------------
// Who is asking: the names a binary must not hold

/// The home directory and the user name of the account that builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who {
    /// `$HOME`.
    pub home: String,
    /// The user name.
    pub user: String,
}

impl Who {
    /// From `$HOME` and `$USER` (else `$LOGNAME`, else the last part of `$HOME`).
    pub fn current() -> Result<Self> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .filter(|u| !u.is_empty())
            .or_else(|| {
                Path::new(&home)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_owned)
            })
            .context("the user name is unknown (USER, LOGNAME)")?;
        Ok(Self { home, user })
    }
}

/// One thing to look for in bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needle {
    /// What it is, for the report.
    pub label: &'static str,
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Counts only where it starts a path: the byte before it is not part of a name. For
    /// `/media/`, which the application's own `kourier/media/…` strings also contain.
    pub rooted: bool,
    /// Compared without regard to the case of ASCII letters.
    pub fold_case: bool,
}

/// The needles for `who`: the home directory, the user name and `/media/` at the start of a
/// path. A user name shorter than four bytes is searched as `/<name>/` (the name `u` is in
/// every binary).
pub fn private_needles(who: &Who) -> Vec<Needle> {
    let mut out = Vec::new();
    let home = who.home.trim_end_matches('/');
    if home.starts_with('/') && home.len() > 1 {
        out.push(Needle {
            label: "the home directory",
            bytes: home.as_bytes().to_vec(),
            rooted: false,
            fold_case: false,
        });
    }
    if !who.user.is_empty() {
        let bytes = if who.user.len() >= 4 {
            who.user.as_bytes().to_vec()
        } else {
            format!("/{}/", who.user).into_bytes()
        };
        out.push(Needle {
            label: "the user name",
            bytes,
            rooted: false,
            fold_case: false,
        });
    }
    out.push(Needle {
        label: "/media/",
        bytes: b"/media/".to_vec(),
        rooted: true,
        fold_case: false,
    });
    out
}

/// What else names the maintainer or the machine: the host name and what git signs commits
/// with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    /// The host name.
    pub host: Option<String>,
    /// git's `user.name`.
    pub git_name: Option<String>,
    /// git's `user.email`.
    pub git_email: Option<String>,
}

/// The shortest value that is searched: a shorter one is in every binary.
const MIN_NEEDLE_LEN: usize = 4;

impl Extra {
    /// This machine's host name and the repository's git identity (each `None` when unset).
    pub fn current() -> Self {
        let git_value = |key: &str| {
            let mut cmd = Command::new("git");
            cmd.arg("-C").arg(root()).args(["config", key]);
            stdout_of(&mut cmd, "git config")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        Self {
            host: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty() && h != "localhost" && h != "(none)"),
            git_name: git_value("user.name"),
            git_email: git_value("user.email"),
        }
    }
}

/// The needles for `extra` and for the project's former name. A value shorter than
/// [`MIN_NEEDLE_LEN`] bytes is left out (it would match everything); the label of every needle
/// that is searched is printed by `dist`, so what was left out shows.
pub fn extra_needles(extra: &Extra) -> Vec<Needle> {
    let mut out = Vec::new();
    for (label, value) in [
        ("the host name", &extra.host),
        ("git's user.name", &extra.git_name),
        ("git's user.email", &extra.git_email),
    ] {
        if let Some(v) = value.as_deref().filter(|v| v.len() >= MIN_NEEDLE_LEN) {
            out.push(Needle {
                label,
                bytes: v.as_bytes().to_vec(),
                rooted: false,
                fold_case: true,
            });
        }
    }
    out.push(Needle {
        label: "the former project name",
        bytes: FORMER_NAME.concat().into_bytes(),
        rooted: false,
        fold_case: true,
    });
    out
}

/// The standard needles for `who` and `extra`.
pub fn all_needles(who: &Who, extra: &Extra) -> Vec<Needle> {
    let mut needles = private_needles(who);
    needles.extend(extra_needles(extra));
    needles
}

/// What [`scan_bytes`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// Matches per needle label, in the order of the needles.
    pub counts: Vec<(&'static str, usize)>,
    /// The printable strings around the first matches, with the offset of the first match in
    /// each (a string with several matches is listed once).
    pub strings: Vec<(usize, String)>,
    /// How many distinct strings hold a match.
    pub string_total: usize,
}

impl Scan {
    /// Matches over all needles.
    pub fn total(&self) -> usize {
        self.counts.iter().map(|(_, n)| n).sum()
    }

    /// `home directory 0, user name 0, /media/ 0`.
    pub fn summary(&self) -> String {
        self.counts
            .iter()
            .map(|(label, n)| format!("{label}: {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Offsets where `needle` starts in `hay`.
fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let Some((&first, _)) = needle.split_first() else {
        return out;
    };
    let mut at = 0;
    while let Some(i) = hay[at..].iter().position(|&b| b == first) {
        let start = at + i;
        if hay[start..].starts_with(needle) {
            out.push(start);
        }
        at = start + 1;
    }
    out
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

fn is_printable(b: u8) -> bool {
    (0x20..0x7f).contains(&b)
}

/// The run of printable ASCII around `at`, cut to 100 bytes on each side.
fn printable_around(bytes: &[u8], at: usize, len: usize) -> (usize, String) {
    let mut start = at;
    while start > 0 && at - start < 100 && is_printable(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = at + len;
    while end < bytes.len() && end - at - len < 100 && is_printable(bytes[end]) {
        end += 1;
    }
    (
        start,
        String::from_utf8_lossy(&bytes[start..end]).into_owned(),
    )
}

/// Looks for the needles in `bytes`, as `strings` would. At most `keep` strings are returned.
pub fn scan_bytes(bytes: &[u8], needles: &[Needle], keep: usize) -> Scan {
    let mut scan = Scan::default();
    let mut seen: BTreeMap<usize, String> = BTreeMap::new();
    let lowered = needles
        .iter()
        .any(|n| n.fold_case)
        .then(|| bytes.to_ascii_lowercase());
    for needle in needles {
        let mut count = 0;
        let found = match (&lowered, needle.fold_case) {
            (Some(low), true) => find_all(low, &needle.bytes.to_ascii_lowercase()),
            _ => find_all(bytes, &needle.bytes),
        };
        for at in found {
            if needle.rooted && at > 0 && is_name_byte(bytes[at - 1]) {
                continue;
            }
            count += 1;
            let (start, text) = printable_around(bytes, at, needle.bytes.len());
            seen.entry(start).or_insert(text);
        }
        scan.counts.push((needle.label, count));
    }
    scan.string_total = seen.len();
    scan.strings = seen.into_iter().take(keep).collect();
    scan
}

/// The first four bytes of a zstd frame.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The most one zstd frame may give back (a safeguard, not a limit anyone meets).
const ZSTD_MAX_OUTPUT: u64 = 256 * 1024 * 1024;

/// The decompressed contents of the zstd frames in `bytes` (Qt compresses the files of its
/// resources, QML among them, with zstd), joined with a NUL between them. Each frame starts at
/// a zstd magic number and runs to the next one; what is not a frame, or ends badly, gives
/// what it gave before it failed. `None` when the `zstd` tool is not installed.
pub fn zstd_frames(bytes: &[u8]) -> Result<Option<(usize, Vec<u8>)>> {
    let starts = find_all(bytes, &ZSTD_MAGIC);
    let (mut frames, mut joined) = (0, Vec::new());
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(bytes.len());
        let mut child = match Command::new("zstd")
            .args(["--decompress", "--stdout", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("run zstd"),
        };
        let mut stdin = child.stdin.take().context("zstd's input")?;
        let frame = bytes[start..end].to_vec();
        // Written on the side: zstd answers while it is still being fed.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&frame);
        });
        let mut out = Vec::new();
        child
            .stdout
            .take()
            .context("zstd's output")?
            .take(ZSTD_MAX_OUTPUT)
            .read_to_end(&mut out)?;
        let _ = child.wait();
        let _ = writer.join();
        if !out.is_empty() {
            frames += 1;
            joined.extend_from_slice(&out);
            joined.push(0);
        }
    }
    Ok(Some((frames, joined)))
}

// ---------------------------------------------------------------------------------------
// The build

/// What the build maps away and how it is invoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildPlan {
    /// Absolute path and what it becomes, the later rules winning where they overlap.
    pub prefixes: Vec<(PathBuf, String)>,
    /// The target triple (the host's).
    pub triple: String,
}

impl BuildPlan {
    /// The workspace, cargo's home, the real path of the build directory and the toolchain.
    ///
    /// The toolchain needs two rules. With the `rust-src` component installed, rustc points
    /// the standard library's panic locations at the real files below the toolchain
    /// (`<sysroot>/lib/rustlib/src/rust/library/…`) instead of the `/rustc/<commit>/library/…`
    /// they are compiled with, so those files go back to the name they have in a build
    /// without `rust-src`, and the rest of the toolchain directory is mapped as well.
    pub fn new(
        workspace: &Path,
        cargo_home: &Path,
        build_real: &Path,
        sysroot: &Path,
        rustc_commit: Option<&str>,
        triple: &str,
    ) -> Self {
        let mut prefixes = vec![
            (workspace.to_path_buf(), MAPPED_WORKSPACE.to_owned()),
            (cargo_home.to_path_buf(), MAPPED_CARGO_HOME.to_owned()),
            (build_real.to_path_buf(), MAPPED_BUILD_DIR.to_owned()),
            (sysroot.to_path_buf(), MAPPED_TOOLCHAIN.to_owned()),
        ];
        if let Some(commit) = rustc_commit {
            prefixes.push((
                sysroot.join("lib/rustlib/src/rust"),
                format!("/rustc/{commit}"),
            ));
        }
        Self {
            prefixes,
            triple: triple.to_owned(),
        }
    }

    /// A path that cannot be written into the flags: a flag is one word, and `=` splits it.
    pub fn check(&self) -> Result<()> {
        for (path, _) in &self.prefixes {
            let text = path
                .to_str()
                .with_context(|| format!("{} is not UTF-8", path.display()))?;
            if text
                .chars()
                .any(|c| c.is_whitespace() || c == '=' || c == '"' || c == '\\')
            {
                bail!(
                    "{text} has a space, '=', a quote or a backslash in it; the path flags of the compilers cannot carry that"
                );
            }
        }
        Ok(())
    }

    /// The value of `--config`: the target's rustflags, which cargo APPENDS to the array of
    /// `.cargo/config.toml` (the linker flags stay).
    pub fn rustflags_config(&self) -> String {
        let flags: Vec<String> = self
            .prefixes
            .iter()
            .map(|(from, to)| format!("\"--remap-path-prefix={}={to}\"", from.display()))
            .collect();
        format!("target.{}.rustflags=[{}]", self.triple, flags.join(","))
    }

    /// The flags of the C and C++ compilers (`CFLAGS`, `CXXFLAGS`).
    pub fn file_prefix_flags(&self) -> String {
        self.prefixes
            .iter()
            .map(|(from, to)| format!("-ffile-prefix-map={}={to}", from.display()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// `existing` (the environment's own flags) followed by ours.
    pub fn compiler_flags(&self, existing: Option<&str>) -> String {
        match existing.map(str::trim).filter(|e| !e.is_empty()) {
            Some(e) => format!("{e} {}", self.file_prefix_flags()),
            None => self.file_prefix_flags(),
        }
    }
}

/// The host triple from `rustc -vV`.
pub fn host_triple(rustc_vv: &str) -> Result<String> {
    rustc_vv
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|t| t.trim().to_owned())
        .context("rustc -vV names no host")
}

/// The commit `rustc -vV` names (a distribution's rustc may say `unknown`).
pub fn rustc_commit(rustc_vv: &str) -> Option<String> {
    rustc_vv
        .lines()
        .find_map(|l| l.strip_prefix("commit-hash: "))
        .map(str::trim)
        .filter(|h| h.len() >= 8 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_owned)
}

fn cargo_home(who: &Who) -> PathBuf {
    match std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => Path::new(&who.home).join(".cargo"),
    }
}

/// The names of the environment variables that would change what the build makes or where it
/// puts it, among `vars`: the target triple and directory, the rustc and its wrappers, the
/// flags (the paths are mapped away through the configuration, and a variable would replace
/// it), and every other `CARGO_BUILD_*`, `CARGO_PROFILE_*` and `CARGO_TARGET_*` setting.
pub fn build_env_removals<'a>(vars: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    const EXACT: [&str; 7] = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_INCREMENTAL",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_BOOTSTRAP",
    ];
    const PREFIXES: [&str; 3] = ["CARGO_BUILD_", "CARGO_PROFILE_", "CARGO_TARGET_"];
    let mut out: Vec<String> = vars
        .into_iter()
        .filter(|v| EXACT.contains(v) || PREFIXES.iter().any(|p| v.starts_with(p)))
        .map(str::to_owned)
        .collect();
    // Always cleared, set or not: they are the ones that move the output.
    for always in [
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_TARGET_DIR",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ] {
        if !out.iter().any(|v| v == always) {
            out.push(always.to_owned());
        }
    }
    out.sort();
    out
}

/// Builds the release binary into [`BUILD_DIR`] and returns the build directory.
fn build_binary(who: &Who) -> Result<PathBuf> {
    let build = root().join(BUILD_DIR);
    std::fs::create_dir_all(&build)
        .with_context(|| format!("create {} (a dangling link?)", build.display()))?;
    let real = std::fs::canonicalize(&build)?;
    let vv = stdout_of(Command::new("rustc").arg("-vV"), "rustc -vV")?;
    let sysroot = stdout_of(
        Command::new("rustc").args(["--print", "sysroot"]),
        "rustc --print sysroot",
    )?;
    let plan = BuildPlan::new(
        &root(),
        &cargo_home(who),
        &real,
        Path::new(sysroot.trim()),
        rustc_commit(&vv).as_deref(),
        &host_triple(&vv)?,
    );
    plan.check()?;
    let mut cmd = cargo();
    cmd.args([
        "build",
        "--release",
        "--locked",
        "-p",
        "kourier",
        "--target-dir",
    ])
    .arg(&build)
    .args(BUILD_FEATURE_ARGS)
    .arg("--config")
    .arg(plan.rustflags_config())
    .env(
        "CFLAGS",
        plan.compiler_flags(std::env::var("CFLAGS").ok().as_deref()),
    )
    .env(
        "CXXFLAGS",
        plan.compiler_flags(std::env::var("CXXFLAGS").ok().as_deref()),
    );
    // The environment's own flags would replace the config file's (the linker flags), a
    // target directory or triple from the environment is not ours, and a profile or a rustc
    // from it would build something else than the manifest says.
    let present: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .collect();
    for var in build_env_removals(present.iter().map(String::as_str)) {
        cmd.env_remove(var);
    }
    run(
        &format!(
            "build the release binary with the paths mapped away ({})",
            build_features()
        ),
        &mut cmd,
    )?;
    Ok(build)
}

// ---------------------------------------------------------------------------------------
// The stamp: what the binary was built from

/// The first line of a stamp file.
const STAMP_HEADER: &str = "kourier-dist-stamp 1";

/// What the build step records beside the binary, and packing reads back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// The commit the build started from.
    pub commit: String,
    /// Whether the working tree had changes.
    pub dirty: bool,
    /// The cargo feature arguments of the build.
    pub cargo_features: String,
    /// `qmake -query QT_VERSION` at build time.
    pub qt: String,
    /// The version of KDE Frameworks at build time.
    pub kf6: String,
    /// `rustc -V` at build time.
    pub rustc: String,
    /// The SHA-256 of the binary.
    pub binary_sha256: String,
}

impl Stamp {
    /// The text of the stamp file: a header and `key=value` lines.
    pub fn render(&self) -> String {
        let one_line = |v: &str| v.replace(['\n', '\r'], " ");
        format!(
            "{STAMP_HEADER}\ncommit={}\ndirty={}\nfeatures={}\nqt={}\nkf6={}\nrustc={}\nbinary-sha256={}\n",
            one_line(&self.commit),
            self.dirty,
            one_line(&self.cargo_features),
            one_line(&self.qt),
            one_line(&self.kf6),
            one_line(&self.rustc),
            one_line(&self.binary_sha256),
        )
    }

    /// Reads a stamp file; every key must be there once.
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some(STAMP_HEADER) {
            bail!("the stamp does not start with {STAMP_HEADER:?}");
        }
        let mut values: BTreeMap<&str, &str> = BTreeMap::new();
        for line in lines.filter(|l| !l.trim().is_empty()) {
            let (key, value) = line
                .split_once('=')
                .with_context(|| format!("a stamp line without '=': {line:?}"))?;
            if values.insert(key, value).is_some() {
                bail!("the stamp has {key} twice");
            }
        }
        let get = |key: &str| -> Result<String> {
            values
                .get(key)
                .map(|v| (*v).to_owned())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("the stamp has no {key}"))
        };
        let dirty = match get("dirty")?.as_str() {
            "true" => true,
            "false" => false,
            other => bail!("the stamp says dirty={other:?}"),
        };
        Ok(Self {
            commit: get("commit")?,
            dirty,
            cargo_features: get("features")?,
            qt: get("qt")?,
            kf6: get("kf6")?,
            rustc: get("rustc")?,
            binary_sha256: get("binary-sha256")?,
        })
    }

    /// Fails unless the stamp names this commit, this dirty state, this feature set and this
    /// binary.
    pub fn check(
        &self,
        commit: &str,
        dirty: bool,
        cargo_features: &str,
        binary_sha256: &str,
    ) -> Result<()> {
        if self.commit != commit {
            bail!(
                "the binary was built from commit {}, not from {} (build again: drop --no-build)",
                self.commit.get(..12).unwrap_or(&self.commit),
                commit.get(..12).unwrap_or(commit)
            );
        }
        if self.dirty != dirty {
            bail!(
                "the binary was built from a tree {}, and this tree is {} (build again: drop --no-build)",
                if self.dirty {
                    "with changes"
                } else {
                    "without changes"
                },
                if dirty {
                    "with changes"
                } else {
                    "without changes"
                },
            );
        }
        if self.cargo_features != cargo_features {
            bail!(
                "the binary was built with `{}`, not `{cargo_features}` (build again: drop --no-build)",
                self.cargo_features
            );
        }
        if self.binary_sha256 != binary_sha256 {
            bail!(
                "the binary is not the one the stamp was written for (checksum {}, the stamp has {}): another build replaced it (build again: drop --no-build)",
                binary_sha256.get(..12).unwrap_or(binary_sha256),
                self.binary_sha256.get(..12).unwrap_or(&self.binary_sha256)
            );
        }
        Ok(())
    }
}

/// `<build>/release/kourier.dist-stamp`, beside the binary.
fn stamp_path(build: &Path) -> PathBuf {
    build.join("release").join(STAMP_FILE)
}

/// The SHA-256 of a file, by `sha256sum`.
fn sha256_of(path: &Path) -> Result<String> {
    let out = stdout_of(Command::new("sha256sum").arg(path), "sha256sum")?;
    out.split_whitespace()
        .next()
        .filter(|h| h.len() == 64)
        .map(str::to_owned)
        .context("sha256sum printed no checksum")
}

/// The version in `set(PACKAGE_VERSION "6.30.0")` of a CMake `…ConfigVersion.cmake`.
pub fn cmake_package_version(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("set(PACKAGE_VERSION")?;
        let rest = rest.trim_start().strip_prefix('"')?;
        rest.split_once('"').map(|(v, _)| v.to_owned())
    })
}

/// The KDE Frameworks version on this system: pkg-config's answer for [`KF6_PROBE`], else the
/// version file of its CMake package.
fn kf6_version() -> Result<String> {
    let asked = Command::new("pkg-config")
        .args(["--modversion", KF6_PROBE])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|v| !v.is_empty());
    if let Some(v) = asked {
        return Ok(v);
    }
    let arch = std::env::consts::ARCH;
    let file =
        format!("/usr/lib/{arch}-linux-gnu/cmake/{KF6_PROBE}/{KF6_PROBE}ConfigVersion.cmake");
    std::fs::read_to_string(&file)
        .ok()
        .and_then(|t| cmake_package_version(&t))
        .with_context(|| {
            format!(
                "the KDE Frameworks version is unknown: pkg-config --modversion {KF6_PROBE} fails and {file} is missing (install the KF6 development packages, see CLAUDE.md)"
            )
        })
}

/// The versions of the tools the build uses, asked of the system before it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolVersions {
    qt: String,
    kf6: String,
    rustc: String,
}

impl ToolVersions {
    fn gather() -> Result<Self> {
        Ok(Self {
            qt: qmake_query("QT_VERSION")
                .context("the Qt version is unknown: qmake -query QT_VERSION failed")?,
            kf6: kf6_version()?,
            rustc: stdout_of(Command::new("rustc").arg("-V"), "rustc -V")?
                .trim()
                .to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------------------
// Requirements

/// A version as its numbers: `2.34` is `[2, 34]`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub Vec<u32>);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let parts: Option<Vec<u32>> = text.split('.').map(|p| p.parse().ok()).collect();
        parts.filter(|p| !p.is_empty()).map(Self)
    }
}

/// The libraries and the library paths in `objdump -p` output.
pub fn parse_dynamic_section(objdump_p: &str) -> (Vec<String>, Vec<String>) {
    let (mut needed, mut paths) = (Vec::new(), Vec::new());
    for line in objdump_p.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("NEEDED"), Some(lib)) => needed.push(lib.to_owned()),
            (Some("RPATH" | "RUNPATH"), Some(path)) => paths.push(path.to_owned()),
            _ => {}
        }
    }
    (needed, paths)
}

/// The highest version tags in the undefined symbols of `objdump -T` output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VersionTags {
    /// Highest `GLIBC_x.y`.
    pub glibc: Option<Version>,
    /// Highest `GLIBCXX_x.y.z`.
    pub glibcxx: Option<Version>,
    /// Highest `Qt_x`.
    pub qt: Option<Version>,
    /// How many undefined symbols carry `Qt_<n>_PRIVATE_API`.
    pub qt_private_symbols: usize,
}

/// Whether the flag columns of an `objdump -T` line (what stands between the address and
/// `*UND*`) mark the symbol weak: a weak undefined symbol is optional, the loader does not
/// fail on it, so its version tag is no requirement.
fn is_weak_symbol(line_before_section: &str) -> bool {
    line_before_section
        .split_whitespace()
        .skip(1)
        .any(|flags| flags.contains('w'))
}

/// Reads the version of every undefined, non-weak symbol: `… *UND* <size> (GLIBC_2.34) name`.
pub fn parse_version_tags(objdump_t: &str) -> VersionTags {
    fn raise(slot: &mut Option<Version>, v: Version) {
        if slot.as_ref().is_none_or(|old| v > *old) {
            *slot = Some(v);
        }
    }
    let mut tags = VersionTags::default();
    for line in objdump_t.lines().filter(|l| l.contains("*UND*")) {
        if is_weak_symbol(line.split("*UND*").next().unwrap_or("")) {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(tag) = words
            .len()
            .checked_sub(2)
            .map(|i| words[i].trim_matches(|c| c == '(' || c == ')'))
        else {
            continue;
        };
        if let Some(v) = tag.strip_prefix("GLIBCXX_").and_then(Version::parse) {
            raise(&mut tags.glibcxx, v);
        } else if let Some(v) = tag.strip_prefix("GLIBC_").and_then(Version::parse) {
            raise(&mut tags.glibc, v);
        } else if tag.starts_with("Qt_") && tag.ends_with("_PRIVATE_API") {
            tags.qt_private_symbols += 1;
        } else if let Some(v) = tag.strip_prefix("Qt_").and_then(Version::parse) {
            raise(&mut tags.qt, v);
        }
    }
    tags
}

/// The module names imported by the QML `text`, without Kourier's own module and without
/// imports of a directory or a file (`import "components"`).
pub fn qml_imports(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("import "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|m| !m.starts_with('"') && !m.starts_with('\''))
        .filter(|m| *m != OWN_QML_MODULE && !m.starts_with(&format!("{OWN_QML_MODULE}.")))
        .map(str::to_owned)
        .collect()
}

/// The imports of every `.qml` file below `dir`.
pub fn qml_modules_in(dir: &Path) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(qml_modules_in(&path)?);
        } else if path.extension().is_some_and(|e| e == "qml") {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            out.extend(qml_imports(&text));
        }
    }
    Ok(out)
}

/// The package names in the output of `dpkg -S <path>` (`pkg, pkg2:amd64: /path`).
pub fn parse_dpkg_search(output: &str) -> Vec<String> {
    let mut out: Vec<String> = output
        .lines()
        .filter_map(|l| l.rsplit_once(": ").map(|(names, _)| names))
        .flat_map(|names| names.split(", "))
        .map(|n| n.split(':').next().unwrap_or(n).trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The packages that own `path` on this system (none when dpkg is missing or knows no owner).
fn dpkg_owner(path: &Path) -> Vec<String> {
    Command::new("dpkg")
        .arg("-S")
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_dpkg_search(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// A QML module the application imports at run time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QmlModule {
    /// `org.kde.kirigami`.
    pub name: String,
    /// The Debian packages that hold it on the building system (empty: unknown).
    pub packages: Vec<String>,
}

/// A shared library the binary needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    /// `libQt6Core.so.6`.
    pub soname: String,
    /// The Debian packages that hold it on the building system (empty: unknown).
    pub packages: Vec<String>,
}

/// What the system must have for the binary to run, computed from the binary itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    /// `NEEDED` entries.
    pub libraries: Vec<Library>,
    /// `RPATH` and `RUNPATH` entries (none expected).
    pub library_paths: Vec<String>,
    /// Highest version tags.
    pub tags: VersionTags,
    /// The Qt the binary was built against (`qmake6 -query QT_VERSION`, from the stamp).
    pub built_with_qt: Option<String>,
    /// The KDE Frameworks it was built against (from the stamp).
    pub built_with_kf6: Option<String>,
    /// QML modules imported at run time.
    pub qml_modules: Vec<QmlModule>,
    /// The Qt Quick Controls style the application runs in ([`DESKTOP_STYLE_MODULE`]); the
    /// QML does not import it, a style is chosen by name.
    pub desktop_style: QmlModule,
}

/// `6.11` of `6.11.1`.
fn minor_of(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The text of the requirements, for the terminal and for `README.txt`.
pub fn render_requirements(r: &Requirements) -> String {
    let mut out = String::new();
    out.push_str("Shared libraries (NEEDED):\n");
    for lib in &r.libraries {
        out.push_str(&format!("  {}", lib.soname));
        if !lib.packages.is_empty() {
            out.push_str(&format!("   [{}]", lib.packages.join(", ")));
        }
        out.push('\n');
    }
    for path in &r.library_paths {
        out.push_str(&format!("  (library search path in the binary: {path})\n"));
    }
    out.push_str("\nVersions it uses:\n");
    let tag = |v: &Option<Version>| v.as_ref().map_or("none".to_owned(), ToString::to_string);
    out.push_str(&format!("  glibc      {} or newer\n", tag(&r.tags.glibc)));
    out.push_str(&format!(
        "  libstdc++  GLIBCXX_{} or newer\n",
        tag(&r.tags.glibcxx)
    ));
    let built = r.built_with_qt.as_deref();
    let n = r.tags.qt_private_symbols;
    match (n > 0, built) {
        (true, Some(full)) => out.push_str(&format!(
            "  Qt         {minor} (built with {full}). It uses {n} symbol{s} of Qt's private API\n             (Qt_6_PRIVATE_API), which Qt keeps for one minor version only: run it\n             with Qt {minor}, not with an older or a newer minor version.\n",
            minor = minor_of(full),
            s = if n == 1 { "" } else { "s" },
        )),
        (true, None) => out.push_str(&format!(
            "  Qt         the minor version it was built with. It uses {n} symbol{s} of Qt's private\n             API (Qt_6_PRIVATE_API), which Qt keeps for one minor version only.\n",
            s = if n == 1 { "" } else { "s" },
        )),
        (false, Some(full)) => out.push_str(&format!(
            "  Qt         6 (built with {full}); it uses no private API of Qt, but only {} was tried.\n",
            minor_of(full)
        )),
        (false, None) => out.push_str("  Qt         6; it uses no private API of Qt.\n"),
    }
    out.push_str("\nQML modules it imports at run time (install the packages with them):\n");
    for m in &r.qml_modules {
        out.push_str(&format!("  {}", m.name));
        if !m.packages.is_empty() {
            out.push_str(&format!("   [{}]", m.packages.join(", ")));
        }
        out.push('\n');
    }
    out.push_str("\nAlso needed at run time (the binary cannot show these):\n");
    out.push_str(&format!(
        "  - the {DESKTOP_STYLE_MODULE} Qt Quick Controls style, which the application selects\n    by name"
    ));
    if !r.desktop_style.packages.is_empty() {
        out.push_str(&format!("   [{}]", r.desktop_style.packages.join(", ")));
    }
    out.push_str(
        "\n  - a Secret Service provider, KWallet or GNOME Keyring, running and unlocked:\n    Kourier keeps its database key there and does not start without one\n",
    );
    match r.built_with_kf6.as_deref() {
        Some(v) => out.push_str(&format!(
            "  - KDE Frameworks 6: built and tried with {v}; an older release may lack a symbol\n    it links\n"
        )),
        None => out.push_str("  - KDE Frameworks 6 (the libraries above)\n"),
    }
    out
}

/// Where the shared libraries are searched on this machine.
fn library_dirs(arch: &str) -> [PathBuf; 2] {
    [
        PathBuf::from(format!("/usr/lib/{arch}-linux-gnu")),
        PathBuf::from(format!("/lib/{arch}-linux-gnu")),
    ]
}

fn qmake_query(variable: &str) -> Option<String> {
    let qmake = std::env::var("QMAKE").unwrap_or_else(|_| "/usr/bin/qmake6".to_owned());
    Command::new(qmake)
        .args(["-query", variable])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Reads the requirements from `binary` (`objdump`), and the QML modules from `qml_dir`; the
/// Qt and KDE Frameworks versions it was built with are the stamp's.
fn read_requirements(binary: &Path, qml_dir: &Path, stamp: &Stamp) -> Result<Requirements> {
    let dynamic = stdout_of(Command::new("objdump").arg("-p").arg(binary), "objdump -p")?;
    let symbols = stdout_of(Command::new("objdump").arg("-T").arg(binary), "objdump -T")?;
    let (needed, library_paths) = parse_dynamic_section(&dynamic);
    let arch = std::env::consts::ARCH;
    let libraries = needed
        .into_iter()
        .map(|soname| {
            let packages = library_dirs(arch)
                .iter()
                .map(|d| d.join(&soname))
                .find(|p| p.exists())
                .map(|p| dpkg_owner(&p))
                .unwrap_or_default();
            Library { soname, packages }
        })
        .collect();
    let qml_root = qmake_query("QT_INSTALL_QML").map(PathBuf::from);
    let module = |name: String| {
        let packages = qml_root
            .as_ref()
            .map(|r| r.join(name.replace('.', "/")))
            .filter(|dir| dir.is_dir())
            // The package that holds the module is the one that owns its `qmldir`; many
            // packages own the directory itself (`QtQuick` is shared by several).
            .map(|dir| {
                let qmldir = dir.join("qmldir");
                dpkg_owner(if qmldir.is_file() { &qmldir } else { &dir })
            })
            .unwrap_or_default();
        QmlModule { name, packages }
    };
    let qml_modules = qml_modules_in(qml_dir)?.into_iter().map(module).collect();
    Ok(Requirements {
        libraries,
        library_paths,
        tags: parse_version_tags(&symbols),
        built_with_qt: Some(stamp.qt.clone()),
        built_with_kf6: Some(stamp.kf6.clone()),
        qml_modules,
        desktop_style: module(DESKTOP_STYLE_MODULE.to_owned()),
    })
}

// ---------------------------------------------------------------------------------------
// The tree

/// The files of the binary archive, relative to its top directory, for the catalogs of
/// `languages`.
pub fn expected_files(languages: &[String]) -> Vec<String> {
    let layout = install::Layout::new(Path::new(""));
    let rel = |p: &Path| p.to_string_lossy().into_owned();
    let mut files = vec![
        "README.txt".to_owned(),
        rel(&layout.binary),
        rel(&layout.desktop),
        rel(&layout.icon),
        rel(&layout.metainfo),
        "share/licenses/kourier/LICENSE".to_owned(),
        "share/licenses/kourier/THIRD-PARTY.html".to_owned(),
    ];
    for lang in languages {
        files.push(format!("share/locale/{lang}/LC_MESSAGES/kourier.mo"));
    }
    files.sort();
    files
}

/// Every directory above the files, relative to the top directory.
pub fn expected_directories(files: &[String]) -> BTreeSet<String> {
    let mut dirs = BTreeSet::new();
    for file in files {
        let mut path = Path::new(file).parent();
        while let Some(p) = path.filter(|p| !p.as_os_str().is_empty()) {
            dirs.insert(p.to_string_lossy().into_owned());
            path = p.parent();
        }
    }
    dirs
}

/// What is below `dir`: files, directories, and anything else (links, devices), as paths
/// relative to `dir`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    /// Regular files.
    pub files: BTreeSet<String>,
    /// Directories.
    pub directories: BTreeSet<String>,
    /// Everything that is neither.
    pub others: BTreeSet<String>,
}

/// Walks `dir` without following links.
pub fn read_tree(dir: &Path) -> Result<Tree> {
    fn walk(base: &Path, dir: &Path, tree: &mut Tree) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            let rel = path
                .strip_prefix(base)
                .context("a path below its base")?
                .to_string_lossy()
                .into_owned();
            let kind = std::fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() {
                tree.directories.insert(rel);
                walk(base, &path, tree)?;
            } else if kind.is_file() {
                tree.files.insert(rel);
            } else {
                tree.others.insert(rel);
            }
        }
        Ok(())
    }
    let mut tree = Tree::default();
    walk(dir, dir, &mut tree)?;
    Ok(tree)
}

/// What differs between the tree and the expected files; empty when they match exactly.
pub fn tree_problems(tree: &Tree, expected: &[String]) -> Vec<String> {
    let mut problems = Vec::new();
    let want: BTreeSet<&String> = expected.iter().collect();
    for f in tree.files.iter().filter(|f| !want.contains(f)) {
        problems.push(format!("unexpected file {f}"));
    }
    for f in expected.iter().filter(|f| !tree.files.contains(*f)) {
        problems.push(format!("missing file {f}"));
    }
    let dirs = expected_directories(expected);
    for d in tree.directories.difference(&dirs) {
        problems.push(format!("unexpected directory {d}"));
    }
    for d in dirs.difference(&tree.directories) {
        problems.push(format!("missing directory {d}"));
    }
    for o in &tree.others {
        problems.push(format!("{o} is not a regular file or a directory"));
    }
    problems
}

/// Directories 0755, the binary 0755, every other file 0644 (and nothing else: no group or
/// other write, no sticky or setuid bits).
fn normalize_modes(dir: &Path, binary_rel: &Path) -> Result<()> {
    fn walk(base: &Path, dir: &Path, binary_rel: &Path) -> Result<()> {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(base, &path, binary_rel)?;
            } else {
                let mode = if path.strip_prefix(base).is_ok_and(|r| r == binary_rel) {
                    0o755
                } else {
                    0o644
                };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
            }
        }
        Ok(())
    }
    walk(dir, dir, binary_rel)
}

// ---------------------------------------------------------------------------------------
// README.txt

/// What `README.txt` says about the archive.
#[derive(Clone, Debug)]
pub struct Readme<'a> {
    /// The workspace version.
    pub version: &'a str,
    /// The commit and its date.
    pub commit: &'a str,
    pub date: &'a str,
    /// The names.
    pub names: &'a Names,
    /// What the archive is ([`Grade`]).
    pub grade: Grade,
    /// Where the source lives, when the About data say.
    pub repository: Option<&'a str>,
    /// The requirements of the binary.
    pub requirements: &'a Requirements,
    /// The files of the archive.
    pub files: &'a [String],
}

/// The generated `README.txt` (plain text; nothing in it depends on when or where it was made).
pub fn render_readme(r: &Readme) -> String {
    let short = r.commit.get(..12).unwrap_or(r.commit);
    let mut t = String::new();
    t.push_str(&format!(
        "Kourier {}  (linux, {})\n",
        r.version, r.names.stage
    ));
    t.push_str("====================================================================\n\n");
    t.push_str(&format!(
        "Kourier is an unofficial, native Linux messenger compatible with Signal: a Qt 6\nQML + Kirigami application on a protocol-agnostic core, free software under the\nGNU AGPL version 3 only. This archive holds the program as `cargo xtask install`\nputs it on a disk, built from commit {short} ({}).\n",
        r.date
    ));
    match r.grade {
        Grade::Release => {}
        Grade::Unreleased => t.push_str(
            "\nTHIS IS NOT A RELEASE. It was made without the release gate (`cargo xtask dist\n--release`) and is named -unreleased for that. Do not pass it on.\n",
        ),
        Grade::Dirty => t.push_str(
            "\nTHIS IS A DEVELOPMENT BUILD from a working tree with uncommitted changes; the\nsource archive beside it is the last commit WITHOUT those changes. Do not pass it on.\n",
        ),
    }
    t.push_str("\nWhat the system must have\n-------------------------\n");
    t.push_str(
        "The program is built for this system and is not self-contained: it uses the\nlibraries and QML modules of the distribution (a KDE Plasma 6 desktop has them).\nThe lists below were read from the binary when the archive was made. The names in\nbrackets are the packages that hold them on the machine that built it (KDE neon;\nother distributions name them differently).\n\n",
    );
    t.push_str(&render_requirements(r.requirements));
    t.push_str("\nInstall\n-------\n");
    t.push_str(
        "For one user (no root needed), from the directory that holds this file:\n\n    cp -r bin share ~/.local/\n\nbin/kourier must be on your PATH (~/.local/bin is on it on most systems): the\ndesktop entry starts `kourier` by name. System-wide, copy the same two\ndirectories into /usr/local (as root).\n\n",
    );
    let schemes: Vec<String> = install::SCHEMES
        .iter()
        .map(|s| format!("x-scheme-handler/{s};"))
        .collect();
    t.push_str(&format!(
        "The desktop entry has no MimeType= line, so it does not take sgnl:// and signalcaptcha:// links from Signal Desktop.\nTo open them with Kourier, add to its [Desktop Entry] group:  MimeType={}\n\n",
        schemes.concat()
    ));
    t.push_str(
        "Optionally refresh the desktop's caches:\n\n    update-desktop-database ~/.local/share/applications\n\n",
    );
    t.push_str("Remove\n------\nDelete the files this archive installed (shown for ~/.local):\n\n    cd ~/.local && rm -f \\\n");
    let installed: Vec<&String> = r.files.iter().filter(|f| *f != "README.txt").collect();
    for (i, file) in installed.iter().enumerate() {
        let end = if i + 1 == installed.len() { "" } else { " \\" };
        t.push_str(&format!("        {file}{end}\n"));
    }
    t.push_str(
        "\nYour accounts and messages are not part of the program and stay where they are:\n~/.local/share/kourier, ~/.config/kourier, ~/.local/state/kourier and\n~/.cache/kourier (see PRIVACY.md in the source). Delete them yourself if you want\nthem gone.\n\n",
    );
    t.push_str("Source and licences\n-------------------\n");
    t.push_str(&format!(
        "The source of exactly this version is the archive beside this one,\n{}, and its\nchecksum is in SHA256SUMS. ",
        r.names.source_archive
    ));
    if let Some(address) = r.repository {
        t.push_str(&format!("The repository is {address}\n"));
    }
    t.push_str(
        &format!(
            "\nThe program is built from that source and from the libraries Cargo.lock in it\nnames. Their sources are not in the archive: {FETCH_SCRIPT}\nin the source archive fetches exactly them (versions and git revisions as\nlocked).\n"
        ),
    );
    t.push_str(
        "Kourier is licensed under AGPL-3.0-only\n(share/licenses/kourier/LICENSE); the libraries built into the program and their\nlicences, with the C libraries and the Rust standard library that are linked in\nbut are no crates, are in share/licenses/kourier/THIRD-PARTY.html.\n",
    );
    t
}

// ---------------------------------------------------------------------------------------
// Packing

/// Packs `parent/name` into the gzip-compressed tar `dest`, byte for byte the same every
/// time for the same tree: names sorted, every time stamp `mtime`, owner and group 0 with
/// numeric ids, no extended attributes, gzip without a name or a time stamp. The modes are
/// the ones of the files, which [`normalize_modes`] has set.
pub fn pack_tree(parent: &Path, name: &str, mtime: i64, dest: &Path) -> Result<()> {
    let mut tar = Command::new("tar");
    tar.env("LC_ALL", "C")
        .args([
            "--create",
            "--format=gnu",
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--no-xattrs",
            "--no-acls",
            "--no-selinux",
        ])
        .arg(format!("--mtime=@{mtime}"))
        .args(["--file", "-", "-C"])
        .arg(parent)
        .arg(name);
    gzip_into(tar, dest)
}

/// `git archive` of the commit `commit` (a hash, not `HEAD`: the branch may have moved since
/// the run started) of the repository at `repo`, with `prefix` as the top directory, packed like
/// [`pack_tree`]. `tar.umask=0022` gives the files the modes 0644 and 0755 of the binary
/// archive (git's own default is 0002).
fn pack_source(repo: &Path, prefix: &str, commit: &str, dest: &Path) -> Result<()> {
    if !is_commit_hash(commit) {
        bail!("{commit:?} is not a commit hash");
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["-c", "tar.umask=0022", "archive", "--format=tar"])
        .arg(format!("--prefix={prefix}/"))
        .arg(commit);
    gzip_into(cmd, dest)
}

/// Runs `producer` (it writes a tar to its standard output) through `gzip -n -9` into `dest`,
/// written under a temporary name and renamed.
fn gzip_into(mut producer: Command, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("gz.partial");
    let file = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut tar = producer
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start the archiver")?;
    let gzip_status = Command::new("gzip")
        .args(["-n", "-9", "-c"])
        .stdin(tar.stdout.take().context("the archiver's output")?)
        .stdout(file)
        .status()
        .context("run gzip")?;
    let tar_status = tar.wait()?;
    if !tar_status.success() || !gzip_status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "packing {} failed (archiver {tar_status}, gzip {gzip_status})",
            dest.display()
        );
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("rename to {}", dest.display()))
}

/// `SHA256SUMS` for `names` in `dir` (`<hash>  <name>`, the format `sha256sum -c` reads).
pub fn checksums(dir: &Path, names: &[&str]) -> Result<String> {
    let mut sorted = names.to_vec();
    sorted.sort_unstable();
    stdout_of(
        Command::new("sha256sum").current_dir(dir).args(sorted),
        "sha256sum",
    )
}

// ---------------------------------------------------------------------------------------
// Verifying what was packed

/// Runs `cmd` for at most `limit`; kills it (it is our own child) when that is over.
fn run_limited(mut cmd: Command, limit: Duration) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A file that was just written can be "busy" for a moment: another thread that starts a
    // program has forked a child that holds the descriptor it was written through until its
    // own exec (seen in the tests, which write a script and run it at once).
    let began = Instant::now();
    let mut child = loop {
        match cmd.spawn() {
            Ok(child) => break child,
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && began.elapsed() < Duration::from_secs(2) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).context("start the program"),
        }
    };
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            if let Some(mut s) = child.stdout.take() {
                s.read_to_end(&mut stdout)?;
            }
            if let Some(mut s) = child.stderr.take() {
                s.read_to_end(&mut stderr)?;
            }
            return Ok(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("it did not finish within {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Runs `binary --version` with an empty environment and a private `HOME`, and checks that it
/// prints `Kourier <version>` and exits 0.
pub fn check_version_output(
    binary: &Path,
    version: &str,
    home: &Path,
    limit: Duration,
) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home);
    let out =
        run_limited(cmd, limit).with_context(|| format!("run {} --version", binary.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(
            "{} --version exited with {}: {}",
            binary.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let want = format!("Kourier {version}\n");
    if stdout != want {
        bail!("--version printed {stdout:?}, expected {want:?}");
    }
    Ok(())
}

/// Runs a validator and fails with its output when it does.
fn validate(program: &str, args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = Command::new(program).args(args).output().with_context(|| {
        format!("run {program} (install desktop-file-utils and appstream to make a release)")
    })?;
    if !out.status.success() {
        bail!(
            "{program} failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Runs `binary --fixture` as a user would (an empty environment, a private home) and checks
/// that the program refuses it with the message of a build without the test network. The
/// message is what tells the refusal from any other failure of a program that has no display.
pub fn check_fixture_refused(binary: &Path, home: &Path, limit: Duration) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("--fixture")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home);
    let out =
        run_limited(cmd, limit).with_context(|| format!("run {} --fixture", binary.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() || !stderr.contains(FIXTURE_REFUSAL) {
        bail!(
            "{} --fixture must be refused with \"{FIXTURE_REFUSAL}\" (the public build has no test network); it exited with {} and said {:?}",
            binary.display(),
            out.status,
            stderr.trim()
        );
    }
    Ok(())
}

/// What the packed notices must hold for the libraries linked in that are no crate.
pub fn notice_problems(html: &str) -> Vec<String> {
    REQUIRED_NOTICE_WORDS
        .iter()
        .filter(|w| !html.contains(*w))
        .map(|w| format!("THIRD-PARTY.html does not mention {w}"))
        .collect()
}

/// What [`verify`] checked.
#[derive(Debug)]
struct Verified {
    scans: Vec<(String, Scan)>,
    /// The unpacked top directory.
    root_dir: PathBuf,
}

/// [`verify_unpacked`] and then the two validators of the desktop file and the metainfo.
fn verify(
    archive: &Path,
    work: &Path,
    names: &Names,
    version: &str,
    languages: &[String],
    built: &[u8],
    needles: &[Needle],
) -> Result<Verified> {
    let verified = verify_unpacked(archive, work, names, version, languages, built, needles)?;
    let layout = install::Layout::new(&verified.root_dir);
    validate("desktop-file-validate", &[layout.desktop.as_os_str()])?;
    validate(
        "appstreamcli",
        &[
            "validate".as_ref(),
            "--no-net".as_ref(),
            // The project has no site or domain of its own yet, so the metainfo names no
            // homepage; every other finding still fails (as in the install tests).
            "--override=url-homepage-missing=info".as_ref(),
            layout.metainfo.as_os_str(),
        ],
    )?;
    Ok(verified)
}

/// Unpacks `archive` into `work` and checks it, except for the validators. `built` is the
/// binary that was staged.
fn verify_unpacked(
    archive: &Path,
    work: &Path,
    names: &Names,
    version: &str,
    languages: &[String],
    built: &[u8],
    needles: &[Needle],
) -> Result<Verified> {
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work)?;
    let status = Command::new("tar")
        .arg("--extract")
        .arg("--gzip")
        .arg("--file")
        .arg(archive)
        .arg("-C")
        .arg(work)
        .status()
        .context("run tar to unpack")?;
    if !status.success() {
        bail!("tar could not unpack {}", archive.display());
    }
    // Exactly one top directory, named like the archive, and nothing beside it.
    let top = read_tree(work)?;
    let top_level: Vec<&String> = top
        .files
        .iter()
        .chain(&top.directories)
        .chain(&top.others)
        .filter(|p| !p.contains('/'))
        .collect();
    if top_level != [&names.stage] || !top.directories.contains(&names.stage) {
        bail!(
            "the archive must hold one top directory, {}/, and nothing else; it holds {top_level:?}",
            names.stage
        );
    }
    let root_dir = work.join(&names.stage);
    let tree = read_tree(&root_dir)?;
    let expected = expected_files(languages);
    let problems = tree_problems(&tree, &expected);
    if !problems.is_empty() {
        bail!(
            "the unpacked tree is not the expected one:\n  {}",
            problems.join("\n  ")
        );
    }
    let binary = root_dir.join("bin").join(BINARY);
    let unpacked = std::fs::read(&binary)?;
    if unpacked != built {
        bail!("bin/kourier in the archive is not the binary that was built");
    }
    if install::is_perf_build(&unpacked) {
        bail!("bin/kourier is a measuring build (cargo feature perf or dumpable)");
    }
    let mode = std::fs::metadata(&binary)?.permissions().mode() & 0o7777;
    if mode != 0o755 {
        bail!("bin/kourier has mode {mode:o}, expected 755");
    }
    check_version_output(&binary, version, work, VERSION_TIMEOUT)?;
    check_fixture_refused(&binary, work, VERSION_TIMEOUT)?;
    // The public archive claims no link scheme (it would compete with a Signal Desktop).
    let layout = install::Layout::new(&root_dir);
    let desktop = std::fs::read_to_string(&layout.desktop)?;
    for scheme in install::SCHEMES {
        if desktop.contains(&format!("x-scheme-handler/{scheme}")) {
            bail!("the desktop file claims x-scheme-handler/{scheme}: the public archive must not");
        }
    }
    // The licence notices of what is linked in but is no crate.
    let notices =
        std::fs::read_to_string(root_dir.join("share/licenses/kourier/THIRD-PARTY.html"))?;
    let problems = notice_problems(&notices);
    if !problems.is_empty() {
        bail!("{}", problems.join("; "));
    }
    // No file of the archive holds a private name.
    let mut scans = Vec::new();
    for file in &tree.files {
        let scan = scan_bytes(&std::fs::read(root_dir.join(file))?, needles, 5);
        if scan.total() > 0 {
            scans.push((file.clone(), scan));
        }
    }
    Ok(Verified { scans, root_dir })
}

/// The problems in a `tar --list --verbose` listing of a source archive: every path is below
/// `top`, and the modes are the plain ones (0644, 0755; a link is 0777).
pub fn source_listing_problems(listing: &str, top: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut entries = 0;
    for line in listing.lines().filter(|l| !l.trim().is_empty()) {
        entries += 1;
        let mut words = line.split_whitespace();
        let mode = words.next().unwrap_or("");
        let name = words.skip(4).collect::<Vec<_>>().join(" ");
        let name = name.split(" -> ").next().unwrap_or("");
        if name != format!("{top}/") && !name.starts_with(&format!("{top}/")) {
            problems.push(format!("{name} is not below {top}/"));
        }
        let ok = match mode.as_bytes().first() {
            Some(b'd') => mode == "drwxr-xr-x",
            Some(b'-') => mode == "-rw-r--r--" || mode == "-rwxr-xr-x",
            Some(b'l') => mode == "lrwxrwxrwx",
            _ => false,
        };
        if !ok {
            problems.push(format!("{name} has the mode {mode}"));
        }
    }
    if entries == 0 {
        problems.push("the source archive is empty".to_owned());
    }
    problems
}

/// Checks the source archive: it is the commit `commit` (git writes the hash into the archive),
/// it has one top directory named `names.source_dir`, and the modes are plain.
fn verify_source(archive: &Path, names: &Names, commit: &str, hidden: &[String]) -> Result<()> {
    let mut gzip = Command::new("gzip")
        .args(["-dc"])
        .arg(archive)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("run gzip")?;
    let id = Command::new("git")
        .arg("get-tar-commit-id")
        .stdin(gzip.stdout.take().context("gzip's output")?)
        .output()
        .context("run git get-tar-commit-id")?;
    let _ = gzip.wait();
    let id = String::from_utf8_lossy(&id.stdout).trim().to_owned();
    if id != commit {
        bail!(
            "the source archive is the commit {id:?}, not {commit}: it was not made from the commit the run started with"
        );
    }
    let listing = stdout_of(
        Command::new("tar")
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .args(["--list", "--verbose", "--numeric-owner", "--gzip", "--file"])
            .arg(archive),
        "tar --list",
    )?;
    let mut problems = source_listing_problems(&listing, &names.source_dir);
    // A file that is hidden from the export must not be in it (git reads the attribute from
    // the commit, this run from the working tree: on a tree with changes they can differ).
    for path in hidden {
        let member = format!("{}/{path}", names.source_dir);
        if listing.lines().any(|l| l.ends_with(&format!(" {member}"))) {
            problems.push(format!(
                "{member} is in the archive and must not be (export-ignore)"
            ));
        }
    }
    if !problems.is_empty() {
        bail!(
            "the source archive is not as expected:\n  {}",
            problems
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Not ready to publish

/// A tracked file that names a private path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateFile {
    /// Relative to the repository.
    pub path: String,
    /// Line number and the text that matched, a few per file.
    pub hits: Vec<(usize, String)>,
    /// All the lines with a match.
    pub lines: usize,
}

/// What stands between this tree and a release.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotReady {
    /// Files with blanks for the maintainer, and how many each.
    pub blanks: Vec<(String, usize)>,
    /// The application id is still the placeholder.
    pub placeholder_app_id: bool,
    /// Tracked files that name a home directory or a private path.
    pub private_files: Vec<PrivateFile>,
    /// The About data name no repository address, so `README.txt` of a binary archive that
    /// is passed on alone cannot say where its source is.
    pub no_repository: bool,
}

/// The repository address the About data carry (`homepage: Some("…")` in [`ABOUT_SOURCE`]);
/// `None` when the field is `None` or not there.
pub fn about_homepage(about_rs: &str) -> Option<String> {
    about_rs.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("homepage:")?.trim_start();
        let rest = rest.strip_prefix("Some(\"")?;
        rest.split_once('"').map(|(address, _)| address.to_owned())
    })
}

impl NotReady {
    /// Blanks in all.
    pub fn blank_total(&self) -> usize {
        self.blanks.iter().map(|(_, n)| n).sum()
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.blanks.is_empty()
            && !self.placeholder_app_id
            && self.private_files.is_empty()
            && !self.no_repository
    }

    /// How many findings there are (blanks, the id and the private files each count once as
    /// a kind of finding).
    pub fn kinds(&self) -> usize {
        usize::from(!self.blanks.is_empty())
            + usize::from(self.placeholder_app_id)
            + usize::from(!self.private_files.is_empty())
            + usize::from(self.no_repository)
    }

    /// The text of `NOT-READY.txt`.
    pub fn render(&self) -> String {
        let mut t = String::from("NOT READY TO PUBLISH\n====================\n");
        let mut n = 0;
        if !self.blanks.is_empty() {
            n += 1;
            t.push_str(&format!(
                "\n{n}. {} blanks marked {BLANK_MARK} in {} tracked files (fill them):\n",
                self.blank_total(),
                self.blanks.len()
            ));
            for (path, count) in &self.blanks {
                t.push_str(&format!("     {path}: {count}\n"));
            }
        }
        if self.placeholder_app_id {
            n += 1;
            t.push_str(&format!(
                "\n{n}. The application id is still the placeholder {PLACEHOLDER_APP_ID}.\n   It changes with the project's final name (CLAUDE.md, Rules) and is in the desktop\n   file, the icon and metainfo file names, the metainfo and the code.\n"
            ));
        }
        if !self.private_files.is_empty() {
            n += 1;
            t.push_str(&format!(
                "\n{n}. {} tracked files name a home directory or a private path; they would be in\n   the source archive:\n",
                self.private_files.len()
            ));
            for file in &self.private_files {
                t.push_str(&format!("     {} ({} lines)\n", file.path, file.lines));
                for (line, text) in &file.hits {
                    t.push_str(&format!("       line {line}: {text}\n"));
                }
            }
        }
        if self.no_repository {
            n += 1;
            t.push_str(&format!(
                "\n{n}. There is no repository address. The About data ({ABOUT_SOURCE}, `homepage`)\n   name none, so the README.txt of the binary archive cannot say where its source is,\n   and a binary archive that is passed on alone must.\n"
            ));
        }
        t
    }
}

/// The private paths on one line of text: a home directory of the account that builds, the
/// home of anybody who is not a placeholder user, a mount below `/media/<user>` and the
/// maintainer's project tree. Paths of fixtures (`/home/u/…`) are not private.
pub fn private_path_hits(line: &str, who: &Who) -> Vec<String> {
    let mut hits = Vec::new();
    let home = who.home.trim_end_matches('/');
    if home.starts_with('/') && home.len() > 1 && line.contains(home) {
        hits.push(home.to_owned());
    }
    // The second item: whoever's home it is counts, not only the user's own.
    for (root, anyone) in [
        ("/home/", true),
        ("/Users/", true),
        ("/media/", false),
        ("/run/media/", false),
    ] {
        let mut rest = line;
        while let Some(i) = rest.find(root) {
            rest = &rest[i + root.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .collect();
            let starts_well = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric());
            let own = name == who.user;
            if starts_well
                && (own
                    || (anyone && name.len() > 1 && !PLACEHOLDER_USERS.contains(&name.as_str())))
            {
                let hit = format!("{root}{name}");
                if !hits.iter().any(|h: &String| hit.starts_with(h.as_str())) {
                    hits.push(hit);
                }
            }
        }
    }
    for root in PRIVATE_TILDE_ROOTS {
        if line.contains(root) {
            hits.push(root.to_owned());
        }
    }
    hits
}

/// The findings for the tracked files `tracked` below `root`.
pub fn not_ready_findings(
    root: &Path,
    tracked: &[String],
    app_id: &str,
    who: &Who,
    homepage: Option<&str>,
) -> Result<NotReady> {
    let mut found = NotReady {
        placeholder_app_id: app_id == PLACEHOLDER_APP_ID,
        no_repository: homepage.is_none_or(|h| h.trim().is_empty()),
        ..NotReady::default()
    };
    for rel in tracked {
        let bytes = match std::fs::read(root.join(rel)) {
            Ok(b) => b,
            // A tracked file that is deleted in the working tree is a change, caught
            // by the clean-tree check.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {rel}")),
        };
        // Binary files (pictures, certificates) name no path in text form.
        if bytes.iter().take(8192).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let blanks = text.matches(BLANK_MARK).count();
        if blanks > 0 {
            found.blanks.push((rel.clone(), blanks));
        }
        let mut file = PrivateFile {
            path: rel.clone(),
            hits: Vec::new(),
            lines: 0,
        };
        for (i, line) in text.lines().enumerate() {
            let hits = private_path_hits(line, who);
            if hits.is_empty() {
                continue;
            }
            file.lines += 1;
            if file.hits.len() < 3 {
                file.hits.push((i + 1, hits.join(", ")));
            }
        }
        if file.lines > 0 {
            found.private_files.push(file);
        }
    }
    found.blanks.sort();
    found.private_files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

// ---------------------------------------------------------------------------------------
// The command

fn human(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

fn minutes(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}m{:02}s", s / 60, s % 60)
}

fn remove_if_there(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Prints what a scan found, for the terminal.
fn print_scan(what: &str, scan: &Scan) {
    eprintln!(
        "dist: private paths in {what}: {} matches ({}) in {} distinct strings",
        scan.total(),
        scan.summary(),
        scan.string_total
    );
    for (offset, text) in &scan.strings {
        eprintln!("dist:   at byte {offset}: {text}");
    }
}

/// Whether `name` is a file `dist` writes into its output directory: an archive, the
/// checksums or the findings (and the half-written archive of an interrupted run). Archives
/// of every version, suffix and architecture count: none of an earlier run may be taken for
/// this run's result.
pub fn is_dist_output(name: &str) -> bool {
    name == "SHA256SUMS"
        || name == "NOT-READY.txt"
        || (name.starts_with("kourier-")
            && (name.ends_with(".tar.gz") || name.ends_with(".tar.gz.partial")))
}

/// Removes from `out` what an earlier run of `dist` left ([`is_dist_output`], and the stage and
/// unpacking directories), and nothing else: the directory may be the user's. A directory with
/// an archive's name is not touched, a link is removed as a link. Returns the names removed.
/// This is the first act of [`dist`].
pub fn clear_outputs(out: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(out) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", out.display())),
    };
    let mut removed = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let kind = std::fs::symlink_metadata(&path)?.file_type();
        if name == STAGE_DIR || name == VERIFY_DIR {
            if kind.is_dir() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
            } else {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
            }
            removed.push(format!("{name}/"));
        } else if is_dist_output(&name) && !kind.is_dir() {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            removed.push(name);
        }
    }
    removed.sort();
    Ok(removed)
}

/// Removes its directories when dropped: the stage and the unpacking of a run, on every way
/// out of it.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Writes `NOT-READY.txt` and prints it when there are findings. With `release` they are an
/// error (and, as [`clear_outputs`] has run, no archive is left behind); otherwise they are
/// warnings.
fn report_findings(findings: &NotReady, release: bool, out: &Path) -> Result<()> {
    if findings.is_empty() {
        eprintln!("dist: nothing in the tree stands in the way of a release");
        return Ok(());
    }
    let file = out.join("NOT-READY.txt");
    let text = findings.render();
    std::fs::write(&file, &text)?;
    eprintln!("\n{text}");
    if release {
        bail!(
            "--release: {} kinds of findings above (also in {}); nothing was built",
            findings.kinds(),
            file.display()
        );
    }
    eprintln!(
        "dist: warnings only (no --release); written to {}",
        file.display()
    );
    Ok(())
}

/// `cargo xtask dist`.
pub fn dist(args: &[String]) -> Result<()> {
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    // The first act: nothing an earlier run left may survive this one, whatever happens to
    // it next (an option that does not go together, a dirty tree, a finding).
    let out = out_dir_of(args, &cwd)?;
    let removed = clear_outputs(&out)?;
    if !removed.is_empty() {
        eprintln!(
            "dist: removed what an earlier run left in {}: {}",
            out.display(),
            removed.join(", ")
        );
    }
    let opts = parse_options(args, &cwd)?;
    let who = Who::current()?;
    let extra = Extra::current();
    let commit = read_commit()?;
    let dirty = !commit.changes.is_empty();
    if dirty && !opts.allow_dirty {
        let shown: Vec<&str> = commit.changes.iter().take(8).map(String::as_str).collect();
        bail!(
            "the working tree has {} changes (git status --porcelain); commit them, or give --allow-dirty for a build that is only to look at:\n  {}",
            commit.changes.len(),
            shown.join("\n  ")
        );
    }
    let version = workspace_version(&std::fs::read_to_string(root().join("Cargo.toml"))?)?;
    std::fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
    let _cleanup = Cleanup(vec![out.join(STAGE_DIR), out.join(VERIFY_DIR)]);

    // The findings first: it is cheap, and a release with findings builds nothing.
    let tracked: Vec<String> = git(&["ls-files", "-z"])?
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    let homepage = about_homepage(
        &std::fs::read_to_string(root().join(ABOUT_SOURCE))
            .with_context(|| format!("read {ABOUT_SOURCE}"))?,
    );
    // What a source archive holds: a file hidden from it (`export-ignore`) is not published
    // and is judged by nothing below.
    let (tracked, hidden) = exported_files(&tracked)?;
    if !hidden.is_empty() {
        eprintln!(
            "dist: not in the source archive (export-ignore): {}",
            hidden.join(", ")
        );
    }
    if !tracked.iter().any(|f| f == FETCH_SCRIPT) {
        bail!(
            "{FETCH_SCRIPT} is not in the source archive, and README.txt names it as the way to the dependencies' sources"
        );
    }
    let findings = not_ready_findings(&root(), &tracked, APP_ID, &who, homepage.as_deref())?;
    report_findings(&findings, opts.release, &out)?;

    // Past this point a `--release` run has no finding and no tree with changes.
    let grade = grade_of(dirty, opts.release);
    let names = archive_names(&version, std::env::consts::ARCH, grade);
    eprintln!(
        "dist: Kourier {version} from commit {} ({}){}",
        &commit.hash[..12],
        commit.date,
        match grade {
            Grade::Release => ", RELEASE".to_owned(),
            Grade::Unreleased => ", UNRELEASED (names end in -unreleased)".to_owned(),
            Grade::Dirty => ", WORKING TREE WITH CHANGES (names end in -dirty)".to_owned(),
        }
    );

    // The binary.
    let build_started = Instant::now();
    let build = root().join(BUILD_DIR);
    let binary_path = build.join("release").join(BINARY);
    let stamp_file = stamp_path(&build);
    if opts.no_build {
        // Read below; what it says must be this commit and state.
    } else {
        // An old stamp must not outlive a build that fails half way.
        match std::fs::remove_file(&stamp_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", stamp_file.display())),
        }
        let tools = ToolVersions::gather()?;
        build_binary(&who)?;
        let (hash, changes) = git_state()?;
        same_state(&commit, &hash, &changes)?;
        let stamp = Stamp {
            commit: commit.hash.clone(),
            dirty,
            cargo_features: build_features(),
            qt: tools.qt,
            kf6: tools.kf6,
            rustc: tools.rustc,
            binary_sha256: sha256_of(&binary_path)
                .with_context(|| format!("checksum of {}", binary_path.display()))?,
        };
        std::fs::write(&stamp_file, stamp.render())
            .with_context(|| format!("write {}", stamp_file.display()))?;
    }
    let build_time = build_started.elapsed();
    let binary = std::fs::read(&binary_path)
        .with_context(|| format!("read {} (build it: drop --no-build)", binary_path.display()))?;
    let stamp_text = std::fs::read_to_string(&stamp_file).with_context(|| {
        format!(
            "read {}: no stamp, so nothing says what this binary was built from (build it: drop --no-build)",
            stamp_file.display()
        )
    })?;
    let stamp =
        Stamp::parse(&stamp_text).with_context(|| format!("read {}", stamp_file.display()))?;
    stamp.check(
        &commit.hash,
        dirty,
        &build_features(),
        &sha256_of(&binary_path)?,
    )?;
    eprintln!(
        "dist: the binary was built from this commit{} with `{}`; Qt {}, KDE Frameworks {}, {}",
        if dirty { " and this dirty state" } else { "" },
        stamp.cargo_features,
        stamp.qt,
        stamp.kf6,
        stamp.rustc
    );
    if install::is_perf_build(&binary) {
        bail!("{} is a measuring build", binary_path.display());
    }
    let needles = all_needles(&who, &extra);
    eprintln!(
        "dist: searched for: {}",
        needles
            .iter()
            .map(|n| n.label)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let scan = scan_bytes(&binary, &needles, 8);
    print_scan(
        &format!("{} ({})", binary_path.display(), human(binary.len() as u64)),
        &scan,
    );
    // The Qt resources hold the QML compressed.
    let mut private_total = scan.total();
    match zstd_frames(&binary)? {
        Some((frames, text)) => {
            let inner = scan_bytes(&text, &needles, 8);
            print_scan(
                &format!(
                    "the {frames} zstd frames of the binary, decompressed ({})",
                    human(text.len() as u64)
                ),
                &inner,
            );
            private_total += inner.total();
        }
        None => eprintln!(
            "dist: the zstd tool is not installed: the compressed Qt resources were NOT searched"
        ),
    }
    if private_total > 0 && !opts.allow_paths {
        bail!(
            "the binary still holds {private_total} private names (above); find what put them there, or give --allow-paths for a build that stays on this machine"
        );
    }
    // The test network is not linked in (its code has panic locations; none must be there),
    // and the Signal backend is.
    if !find_all(&binary, b"crates/kourier-fake/").is_empty() {
        bail!(
            "the binary holds code of crates/kourier-fake: the test network is built in, and the public binary must not have it"
        );
    }
    if find_all(&binary, b"crates/kourier-signal/").is_empty() {
        bail!(
            "the binary holds no code of crates/kourier-signal: the Signal backend is not built in"
        );
    }
    let test_network = find_all(&binary, b"Test network").len();
    eprintln!(
        "dist: no code of the test network is linked in (the crate kourier-fake is not in the binary); the label \"Test network\" is in it {test_network} times: crates/kourier-viewmodel/src/conversations.rs names it for `NetworkKind::Fake` in every build, and nothing can open such a network"
    );

    // Requirements, from the binary and the stamp.
    let requirements =
        read_requirements(&binary_path, &root().join("crates/kourier-qt/qml"), &stamp)?;
    let rendered = render_requirements(&requirements);
    eprintln!("\ndist: what the binary needs\n{rendered}");

    // Stage.
    let stage_started = Instant::now();
    let stage_parent = out.join(STAGE_DIR);
    let stage = stage_parent.join(&names.stage);
    std::fs::create_dir_all(&stage)?;
    let sources = Sources::in_repo(&root(), &build);
    let locale = target_dir().join("locale");
    crate::i18n::build(&locale)?;
    let languages = crate::i18n::linguas()?;
    stage_files(&stage, &sources, &locale, &languages)?;
    third_party_notices(&stage.join("share/licenses/kourier/THIRD-PARTY.html"))?;
    let files = expected_files(&languages);
    std::fs::write(
        stage.join("README.txt"),
        render_readme(&Readme {
            version: &version,
            commit: &commit.hash,
            date: &commit.date,
            names: &names,
            grade,
            repository: homepage.as_deref(),
            requirements: &requirements,
            files: &files,
        }),
    )?;
    normalize_modes(&stage, &Path::new("bin").join(BINARY))?;
    let stage_time = stage_started.elapsed();

    // Pack, and verify what was packed. Nothing is left behind when either fails.
    let pack_started = Instant::now();
    let archives = [
        out.join(&names.binary_archive),
        out.join(&names.source_archive),
    ];
    let remove_archives = || {
        for archive in &archives {
            remove_if_there(archive);
        }
        remove_if_there(&out.join("SHA256SUMS"));
    };
    let result = (|| -> Result<Verified> {
        pack_tree(
            &stage_parent,
            &names.stage,
            commit.time,
            &out.join(&names.binary_archive),
        )?;
        pack_source(
            &root(),
            &names.source_dir,
            &commit.hash,
            &out.join(&names.source_archive),
        )?;
        let sums = checksums(&out, &[&names.binary_archive, &names.source_archive])?;
        std::fs::write(out.join("SHA256SUMS"), &sums)?;
        let work = out.join(VERIFY_DIR);
        let verified = verify(
            &out.join(&names.binary_archive),
            &work,
            &names,
            &version,
            &languages,
            &binary,
            &needles,
        )?;
        let _ = std::fs::remove_dir_all(&work);
        verify_source(
            &out.join(&names.source_archive),
            &names,
            &commit.hash,
            &hidden,
        )?;
        let check = Command::new("sha256sum")
            .current_dir(&out)
            .args(["--check", "--strict", "--quiet", "SHA256SUMS"])
            .status()
            .context("run sha256sum --check")?;
        if !check.success() {
            bail!("sha256sum --check SHA256SUMS failed");
        }
        Ok(verified)
    })();
    let verified = match result {
        Ok(v) => v,
        Err(e) => {
            remove_archives();
            return Err(e.context("the archives did not pass their check; they were removed"));
        }
    };
    let leaks: usize = verified.scans.iter().map(|(_, s)| s.total()).sum();
    if leaks > 0 {
        for (file, scan) in &verified.scans {
            print_scan(&format!("the archive's {file}"), scan);
        }
        if !opts.allow_paths {
            remove_archives();
            bail!("{leaks} private names in the packed files (above); the archives were removed");
        }
    }
    let pack_time = pack_started.elapsed();

    eprintln!(
        "\ndist: verified: the tree is exactly the expected one, bin/kourier --version prints \"Kourier {version}\" and --fixture is refused, the binary is no measuring build, the desktop file claims no link scheme, the notices name SQLCipher and BoringSSL, desktop-file-validate and appstreamcli pass; the source archive is the commit {} with plain modes",
        &commit.hash[..12]
    );
    eprintln!("dist: files in {}:", out.display());
    for name in [
        names.binary_archive.as_str(),
        names.source_archive.as_str(),
        "SHA256SUMS",
    ] {
        let size = std::fs::metadata(out.join(name))?.len();
        eprintln!("dist:   {name}  {} ({size} bytes)", human(size));
    }
    eprint!("{}", std::fs::read_to_string(out.join("SHA256SUMS"))?);
    eprintln!(
        "dist: time: build {}, stage {}, pack and verify {}, in all {}",
        minutes(build_time),
        minutes(stage_time),
        minutes(pack_time),
        minutes(started.elapsed())
    );
    match grade {
        Grade::Release => {}
        Grade::Unreleased => eprintln!(
            "dist: these are NOT release archives: no --release gate was passed, so every name ends in -unreleased"
        ),
        Grade::Dirty => eprintln!(
            "dist: these are NOT release archives: the working tree had changes, so every name ends in -dirty"
        ),
    }
    if !findings.is_empty() {
        eprintln!("\n{}", findings.render());
        eprintln!(
            "dist: NOT READY TO PUBLISH (also in {})",
            out.join("NOT-READY.txt").display()
        );
    }
    Ok(())
}

/// The installed tree below `stage`, by the code of `cargo xtask install` without the link
/// handlers: the binary, the desktop file, the icon, the metainfo and the catalogs of
/// `languages` compiled in `built_locale`, plus the licence text. (`README.txt` and the
/// third-party notices are added by the caller.)
fn stage_files(
    stage: &Path,
    sources: &Sources,
    built_locale: &Path,
    languages: &[String],
) -> Result<()> {
    // No `MimeType=` for sgnl:// and signalcaptcha://: the public archive must not compete
    // with an installed Signal Desktop for those links (README.txt says how to add the line).
    install::install_files(sources, stage, false)?;
    // The desktop file of the archive starts `kourier` by name, wherever the user copies the
    // tree; `install_files` wrote the path of the stage into it.
    let layout = install::Layout::new(stage);
    let desktop_source = std::fs::read_to_string(&sources.desktop)?;
    std::fs::write(
        &layout.desktop,
        install::desktop_entry(&desktop_source, Path::new(BINARY), false)?,
    )?;
    install::install_catalogs(built_locale, languages, stage)?;
    let licenses = stage.join("share/licenses/kourier");
    std::fs::create_dir_all(&licenses)?;
    std::fs::copy(root().join("LICENSE"), licenses.join("LICENSE"))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Libraries that are linked in but are no crate

/// One text of a bundled library: where it comes from and what it says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledText {
    /// What the text is.
    pub title: String,
    /// The file it was read from, below the package or the toolchain (for the page).
    pub source: String,
    /// The text.
    pub text: String,
}

/// A library inside the binary that `cargo about` does not list because it is no crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledLibrary {
    pub name: String,
    pub licence: String,
    /// Why it is in the binary.
    pub note: String,
    pub texts: Vec<BundledText>,
}

/// The directory of the manifest of package `name` in the output of `cargo metadata`.
fn package_dir(metadata: &serde_json::Value, name: &str) -> Result<PathBuf> {
    metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["name"] == name)
        .and_then(|p| p["manifest_path"].as_str())
        .and_then(|m| Path::new(m).parent().map(Path::to_path_buf))
        .with_context(|| format!("cargo metadata knows no package {name} (the notices of the library inside it cannot be found)"))
}

/// Reads `dir/rel` as the text of a licence; a missing file is an error that names it.
fn licence_file(what: &str, dir: &Path, rel: &str) -> Result<BundledText> {
    let path = dir.join(rel);
    let text = std::fs::read(&path).with_context(|| {
        format!(
            "the licence text of {what} is not there: {} (the notice cannot be made)",
            path.display()
        )
    })?;
    Ok(BundledText {
        title: format!("{what}: {rel}"),
        source: rel.to_owned(),
        text: String::from_utf8_lossy(&text).into_owned(),
    })
}

/// The first of `candidates` that exists below `dir`; else an error that names them all.
fn first_licence_file(what: &str, dir: &Path, candidates: &[&str]) -> Result<BundledText> {
    for rel in candidates {
        if dir.join(rel).is_file() {
            return licence_file(what, dir, rel);
        }
    }
    bail!(
        "the licence text of {what} is not there: none of {} below {} (the toolchain's documentation component is not installed?)",
        candidates.join(", "),
        dir.display()
    )
}

/// The libraries inside the binary that are no crate: SQLCipher (and SQLite in it) from the
/// `libsqlite3-sys` checkout cargo uses, BoringSSL from `boring-sys`, and the Rust standard
/// library with the libraries it bundles, from the licence files of the toolchain at
/// `sysroot`. `metadata` is the output of `cargo metadata`.
pub fn bundled_libraries(
    metadata: &serde_json::Value,
    sysroot: &Path,
    rustc_version: &str,
) -> Result<Vec<BundledLibrary>> {
    let sqlite = package_dir(metadata, "libsqlite3-sys")?;
    let boring = package_dir(metadata, "boring-sys")?;
    let rust_doc = sysroot.join("share/doc/rust");
    Ok(vec![
        BundledLibrary {
            name: "SQLCipher".to_owned(),
            licence: "BSD-3-Clause".to_owned(),
            note: "The message database is encrypted with SQLCipher by Zetetic LLC, which is built into the program (the C source is inside the libsqlite3-sys crate). Its licence requires this notice in the documentation of a binary distribution.".to_owned(),
            texts: vec![licence_file("SQLCipher (Zetetic LLC)", &sqlite, "sqlcipher/LICENSE")?],
        },
        BundledLibrary {
            name: "SQLite".to_owned(),
            licence: "public domain".to_owned(),
            note: "SQLite, the database engine inside SQLCipher, is in the public domain: its authors have dedicated the code to the public domain (https://sqlite.org/copyright.html).".to_owned(),
            texts: Vec::new(),
        },
        BundledLibrary {
            name: "BoringSSL".to_owned(),
            licence: "Apache-2.0".to_owned(),
            note: "The connections to Signal's servers use BoringSSL, built into the program through libsignal (the C source is inside the boring-sys crate). Its LICENSE file follows as it is: the Apache License 2.0, and the Go Authors' BSD-3-Clause text for test support code that is not part of the library.".to_owned(),
            texts: vec![licence_file(
                "BoringSSL",
                &boring,
                "deps/boringssl/LICENSE",
            )?],
        },
        BundledLibrary {
            name: "The Rust standard library".to_owned(),
            licence: "Apache-2.0 OR MIT".to_owned(),
            note: format!(
                "The program contains the Rust standard library and the libraries it bundles, compiled by {rustc_version}. The licence files are those of the toolchain that built it."
            ),
            texts: vec![
                first_licence_file(
                    "the Rust standard library, copyright notices",
                    &rust_doc,
                    &["COPYRIGHT-library.html", "COPYRIGHT-library", "COPYRIGHT"],
                )?,
                first_licence_file(
                    "the Rust standard library, Apache-2.0",
                    &rust_doc,
                    &["LICENSE-APACHE", "licenses/Apache-2.0.txt"],
                )?,
                first_licence_file(
                    "the Rust standard library, MIT",
                    &rust_doc,
                    &["LICENSE-MIT", "licenses/MIT.txt"],
                )?,
            ],
        },
    ])
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The section "Bundled libraries" of `THIRD-PARTY.html`.
pub fn render_bundled_section(libs: &[BundledLibrary]) -> String {
    let mut h = String::from(
        "<section id=\"bundled-libraries\">\n<h2>Bundled libraries</h2>\n<p>These libraries are built into the program but are no Rust crates, so the list above does not have them.</p>\n<ul>\n",
    );
    for lib in libs {
        h.push_str(&format!(
            "  <li><a href=\"#bundled-{}\">{}</a> ({})</li>\n",
            html_escape(&lib.name.to_ascii_lowercase().replace(' ', "-")),
            html_escape(&lib.name),
            html_escape(&lib.licence)
        ));
    }
    h.push_str("</ul>\n");
    for lib in libs {
        h.push_str(&format!(
            "<section>\n  <h3 id=\"bundled-{}\">{}</h3>\n  <p>{}</p>\n",
            html_escape(&lib.name.to_ascii_lowercase().replace(' ', "-")),
            html_escape(&lib.name),
            html_escape(&lib.note)
        ));
        for t in &lib.texts {
            h.push_str(&format!(
                "  <details>\n    <summary>{}</summary>\n    <pre class=\"license-text\">{}</pre>\n  </details>\n",
                html_escape(&t.title),
                html_escape(&t.text)
            ));
        }
        h.push_str("</section>\n");
    }
    h.push_str("</section>\n");
    h
}

/// `html` with `section` put in before the end of its `<main>` (else its `<body>`).
pub fn insert_section(html: &str, section: &str) -> Result<String> {
    let at = html
        .rfind("</main>")
        .or_else(|| html.rfind("</body>"))
        .context(
            "THIRD-PARTY.html has no </main> or </body> to put the bundled libraries before",
        )?;
    Ok(format!("{}{}{}", &html[..at], section, &html[at..]))
}

/// The libraries of [`bundled_libraries`] for this checkout and toolchain, found through
/// `cargo metadata` (offline) and `rustc --print sysroot`.
fn bundled_libraries_here() -> Result<Vec<BundledLibrary>> {
    let mut cmd = cargo();
    cmd.args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(root());
    let metadata: serde_json::Value = serde_json::from_str(&stdout_of(&mut cmd, "cargo metadata")?)
        .context("read the output of cargo metadata")?;
    let sysroot = stdout_of(
        Command::new("rustc").args(["--print", "sysroot"]),
        "rustc --print sysroot",
    )?;
    let version = stdout_of(Command::new("rustc").arg("-V"), "rustc -V")?;
    bundled_libraries(&metadata, Path::new(sysroot.trim()), version.trim())
}

/// `THIRD-PARTY.html`: `cargo about` for the crates (no network: the notices come from the
/// sources in cargo's own cache), then the section of the libraries that are no crates.
fn third_party_notices(dest: &Path) -> Result<()> {
    let found = cargo().args(["about", "--version"]).output();
    if !matches!(&found, Ok(o) if o.status.success()) {
        bail!(
            "cargo-about is not installed; the third-party notices cannot be made. Install it: cargo install cargo-about --locked"
        );
    }
    let bundled = bundled_libraries_here()?;
    let mut cmd = cargo();
    cmd.args([
        "about",
        "generate",
        "--locked",
        "--offline",
        "--all-features",
        "about.hbs",
        "-o",
    ])
    .arg(dest);
    run("third-party licence notices (cargo about)", &mut cmd)?;
    let html = std::fs::read_to_string(dest)?;
    std::fs::write(
        dest,
        insert_section(&html, &render_bundled_section(&bundled))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A home directory path for `user`, spelled in pieces: this file must not name one.
    fn home_of(user: &str) -> String {
        ["/", "home", "/", user].concat()
    }

    fn who(user: &str) -> Who {
        Who {
            home: home_of(user),
            user: user.to_owned(),
        }
    }

    /// A scratch directory below the target directory (never the user's home), removed on
    /// drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("tmp/xtask-dist")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().expect("a directory")).expect("directory");
            std::fs::write(&path, bytes).expect("file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    fn tool_exists(name: &str) -> bool {
        Command::new(name).arg("--version").output().is_ok()
    }

    #[test]
    fn the_files_hidden_from_the_export_are_read_from_gits_answer() {
        let out = "CLAUDE.md\0export-ignore\0set\0README.md\0export-ignore\0unspecified\0\
docs/a b.md\0export-ignore\0set\0x\0export-ignore\0unset\0";
        assert_eq!(export_ignored(out), ["CLAUDE.md", "docs/a b.md"]);
        assert!(export_ignored("").is_empty());
        // The real tree: the working notes are hidden, the code is not.
        let (exported, hidden) =
            exported_files(&["CLAUDE.md".to_owned(), "Cargo.toml".to_owned()]).expect("git");
        assert_eq!(hidden, ["CLAUDE.md"]);
        assert_eq!(exported, ["Cargo.toml"]);
    }

    #[test]
    fn the_names_follow_version_arch_and_grade() {
        let n = archive_names("0.1.0", "x86_64", Grade::Release);
        assert_eq!(n.stage, "kourier-0.1.0-linux-x86_64");
        assert_eq!(n.binary_archive, "kourier-0.1.0-linux-x86_64.tar.gz");
        assert_eq!(n.source_dir, "kourier-0.1.0");
        assert_eq!(n.source_archive, "kourier-0.1.0-source.tar.gz");
        // Everything that did not pass the gate says so in the file name AND the top
        // directory of both archives.
        let d = archive_names("0.1.0", "aarch64", Grade::Dirty);
        assert_eq!(d.stage, "kourier-0.1.0-linux-aarch64-dirty");
        assert_eq!(d.binary_archive, "kourier-0.1.0-linux-aarch64-dirty.tar.gz");
        assert_eq!(d.source_archive, "kourier-0.1.0-source-dirty.tar.gz");
        assert_eq!(d.source_dir, "kourier-0.1.0-dirty");
        let u = archive_names("0.1.0", "x86_64", Grade::Unreleased);
        assert_eq!(u.stage, "kourier-0.1.0-linux-x86_64-unreleased");
        assert_eq!(
            u.binary_archive,
            "kourier-0.1.0-linux-x86_64-unreleased.tar.gz"
        );
        assert_eq!(u.source_dir, "kourier-0.1.0-unreleased");
        assert_eq!(u.source_archive, "kourier-0.1.0-source-unreleased.tar.gz");
    }

    #[test]
    fn only_a_passed_release_gets_the_bare_names() {
        // dirty, release passed
        assert_eq!(grade_of(true, true), Grade::Dirty);
        assert_eq!(grade_of(true, false), Grade::Dirty);
        // clean: a plain run is unreleased, only `--release` (which stops at a finding) is not
        assert_eq!(grade_of(false, false), Grade::Unreleased);
        assert_eq!(grade_of(false, true), Grade::Release);
        let bare: Vec<Grade> = [Grade::Release, Grade::Unreleased, Grade::Dirty]
            .into_iter()
            .filter(|g| g.suffix().is_empty())
            .collect();
        assert_eq!(bare, [Grade::Release]);
    }

    #[test]
    fn the_version_is_the_workspace_one() {
        let text = "[package]\nversion = \"9.9.9\"\n\n[workspace.package]\nversion = \"1.2.3\"\nedition = \"2024\"\n\n[other]\nversion = \"4.5.6\"\n";
        assert_eq!(workspace_version(text).expect("version"), "1.2.3");
        assert!(workspace_version("[workspace]\nmembers = []\n").is_err());
        assert!(workspace_version("[workspace.package]\nversion.workspace = true\n").is_err());
        // The real one is the version of this package, which inherits it.
        let real = include_str!("../../Cargo.toml");
        assert_eq!(
            workspace_version(real).expect("real version"),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn options() {
        let cwd = Path::new("/work");
        let o = parse_options(&[], cwd).expect("defaults");
        assert_eq!(o.out, root().join("target/dist"));
        assert!(!o.release && !o.allow_dirty && !o.allow_paths && !o.no_build);
        let o = parse_options(
            &args(&["--allow-dirty", "--no-build", "--allow-paths"]),
            cwd,
        )
        .expect("flags");
        assert!(o.allow_dirty && o.no_build && o.allow_paths && !o.release);
        let o = parse_options(&args(&["--release"]), cwd).expect("release");
        assert!(o.release);
        assert_eq!(
            parse_options(&args(&["--out", "here/out"]), cwd)
                .expect("out")
                .out,
            Path::new("/work/here/out")
        );
        assert_eq!(
            parse_options(&args(&["--out=/abs/dir"]), cwd)
                .expect("out")
                .out,
            Path::new("/abs/dir")
        );
        // The output directory is known without judging the rest (it is emptied first).
        assert_eq!(
            out_dir_of(&args(&["--release", "--allow-dirty", "--out", "x"]), cwd).expect("out"),
            Path::new("/work/x")
        );
        assert_eq!(
            out_dir_of(&args(&["--out=a", "--out=/b"]), cwd).expect("out"),
            Path::new("/b")
        );
        assert_eq!(
            out_dir_of(&[], cwd).expect("out"),
            root().join("target/dist")
        );
        assert!(out_dir_of(&args(&["--out"]), cwd).is_err());
        let flags = parse_flags(&args(&["--release", "--no-build"]), cwd).expect("flags");
        assert!(flags.check_conflicts().is_err());
        for bad in [
            vec!["--out"],
            vec!["--out="],
            vec!["--push"],
            vec!["--release", "--allow-dirty"],
            vec!["--allow-paths", "--release"],
            vec!["--release", "--no-build"],
        ] {
            assert!(parse_options(&args(&bad), cwd).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_build_maps_every_private_prefix_in_rust_and_c_flags() {
        let plan = BuildPlan::new(
            Path::new("/w/kourier"),
            Path::new("/c/cargo"),
            Path::new("/d/build"),
            Path::new("/t/chain"),
            Some("abc123def"),
            "x86_64-unknown-linux-gnu",
        );
        assert_eq!(
            plan.rustflags_config(),
            "target.x86_64-unknown-linux-gnu.rustflags=[\"--remap-path-prefix=/w/kourier=/kourier\",\"--remap-path-prefix=/c/cargo=/cargo\",\"--remap-path-prefix=/d/build=/kourier-build\",\"--remap-path-prefix=/t/chain=/rust-toolchain\",\"--remap-path-prefix=/t/chain/lib/rustlib/src/rust=/rustc/abc123def\"]"
        );
        assert_eq!(
            plan.file_prefix_flags(),
            "-ffile-prefix-map=/w/kourier=/kourier -ffile-prefix-map=/c/cargo=/cargo -ffile-prefix-map=/d/build=/kourier-build -ffile-prefix-map=/t/chain=/rust-toolchain -ffile-prefix-map=/t/chain/lib/rustlib/src/rust=/rustc/abc123def"
        );
        // A rustc without a commit (a distribution's) still maps its directory.
        let plain = BuildPlan::new(
            Path::new("/w"),
            Path::new("/c"),
            Path::new("/d"),
            Path::new("/t"),
            None,
            "x86_64-unknown-linux-gnu",
        );
        assert_eq!(plain.prefixes.len(), 4);
        assert_eq!(
            plan.compiler_flags(Some(" -O2 ")),
            format!("-O2 {}", plan.file_prefix_flags())
        );
        assert_eq!(plan.compiler_flags(None), plan.file_prefix_flags());
        assert_eq!(plan.compiler_flags(Some("  ")), plan.file_prefix_flags());
        plan.check().expect("plain paths");
        for bad in ["/a b/c", "/a=b/c", "/a\"b", "/a\\b"] {
            let plan = BuildPlan::new(
                Path::new(bad),
                Path::new("/c"),
                Path::new("/d"),
                Path::new("/t"),
                None,
                "x86_64-unknown-linux-gnu",
            );
            assert!(plan.check().is_err(), "{bad}");
        }
        let vv = "rustc 1.99.0 (abc 2026-09-01)\nbinary: rustc\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.0\n";
        assert_eq!(host_triple(vv).expect("host"), "x86_64-unknown-linux-gnu");
        assert!(host_triple("rustc 1.99.0\n").is_err());
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            rustc_commit(&format!("host: x\ncommit-hash: {hash}\n")).as_deref(),
            Some(hash)
        );
        assert_eq!(rustc_commit("commit-hash: unknown\n"), None);
        assert_eq!(rustc_commit("host: x\n"), None);
    }

    #[test]
    fn the_private_path_scan_finds_a_home_path_and_the_user_name_and_not_a_clean_binary() {
        let me = who("carol");
        let needles = private_needles(&me);
        let mut dirty = Vec::new();
        dirty.extend_from_slice(b"\0\0panic at ");
        dirty.extend_from_slice(home_of("carol").as_bytes());
        dirty.extend_from_slice(
            b"/work/x.rs:1\0abc\0/media/carol/disk/y\0kourier/media/avatars\0carol\0",
        );
        let scan = scan_bytes(&dirty, &needles, 8);
        assert_eq!(
            scan.counts,
            [
                ("the home directory", 1),
                ("the user name", 3),
                ("/media/", 1)
            ]
        );
        assert_eq!(scan.total(), 5);
        assert_eq!(scan.string_total, 3);
        assert_eq!(scan.strings.len(), 3);
        assert!(
            scan.strings[0].1.starts_with("panic at "),
            "{:?}",
            scan.strings
        );
        assert!(scan.summary().contains("the user name: 3"));
        // Only the first strings are kept.
        assert_eq!(scan_bytes(&dirty, &needles, 1).strings.len(), 1);

        let clean = b"\0kourier/media/avatars\0/usr/lib/x86_64-linux-gnu/libQt6Core.so.6\0/rustc/0123abcd/library/core/src/panicking.rs:7\0/cargo/registry/src/x/tokio-1/src/lib.rs\0/kourier/crates/kourier-core/src/lib.rs\0";
        let scan = scan_bytes(clean, &needles, 8);
        assert_eq!(scan.total(), 0, "{scan:?}");
        assert!(scan.strings.is_empty());

        // `/media/` counts where a path starts, also after a scheme.
        let scan = scan_bytes(b"file:///media/x/y\0 /media/z\0a/media/b", &needles, 8);
        assert_eq!(scan.counts[2], ("/media/", 2));
    }

    #[test]
    fn a_short_user_name_is_searched_as_a_path_component() {
        let needles = private_needles(&who("u"));
        assert_eq!(needles[1].bytes, b"/u/");
        assert_eq!(scan_bytes(b"unusual use of Qt\0", &needles, 4).total(), 0);
        assert_eq!(
            scan_bytes(b"x/u/y\0", &needles, 4).counts[1],
            ("the user name", 1)
        );
        // No usable home directory: no needle for it.
        let none = private_needles(&Who {
            home: String::new(),
            user: "carol".into(),
        });
        assert!(none.iter().all(|n| n.label != "the home directory"));
        // A string longer than the context window is cut.
        let mut long = vec![b'x'; 300];
        long.extend_from_slice(b"carol");
        long.extend(vec![b'y'; 300]);
        let scan = scan_bytes(&long, &private_needles(&who("carol")), 2);
        assert!(
            scan.strings[0].1.len() <= 205,
            "{}",
            scan.strings[0].1.len()
        );
    }

    const OBJDUMP_P: &str = "\nDynamic Section:\n  NEEDED               libQt6Core.so.6\n  NEEDED               libstdc++.so.6\n  NEEDED               libc.so.6\n  RUNPATH              $ORIGIN/../lib\n  SONAME               kourier\n  INIT                 0x0000000000001000\n";

    #[test]
    fn needed_libraries_come_from_the_dynamic_section() {
        let (needed, paths) = parse_dynamic_section(OBJDUMP_P);
        assert_eq!(needed, ["libQt6Core.so.6", "libstdc++.so.6", "libc.so.6"]);
        assert_eq!(paths, ["$ORIGIN/../lib"]);
        assert_eq!(parse_dynamic_section("nothing\n"), (vec![], vec![]));
    }

    #[test]
    fn version_tags_keep_the_highest_numerically() {
        let sample = "\
DYNAMIC SYMBOL TABLE:
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.34) __libc_start_main
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.9)  pipe2
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.10) accept4
0000000000000000      DF *UND*\t0000000000000000 (GLIBCXX_3.4.29) _ZNSt3foo
0000000000000000      DF *UND*\t0000000000000000 (GLIBCXX_3.4.9) _ZNSt3bar
0000000000000000      DF *UND*\t0000000000000000 (CXXABI_1.3.9) __cxa_x
0000000000000000      DF *UND*\t0000000000000000 (Qt_6)       _ZN7QString8fromUtf8E
0000000000000000      DF *UND*\t0000000000000000 (Qt_6_PRIVATE_API) _ZN10QMetaObject
0000000000000000  w   D  *UND*\t0000000000000000              __gmon_start__
0000000000a1b2c3 g    DF .text\t0000000000000010  Base        kourier_main
";
        let tags = parse_version_tags(sample);
        assert_eq!(tags.glibc, Version::parse("2.34"));
        assert_eq!(tags.glibcxx, Version::parse("3.4.29"));
        assert_eq!(tags.qt, Version::parse("6"));
        assert_eq!(tags.qt_private_symbols, 1);
        let only_old = "0000 DF *UND*\t0 (GLIBC_2.9) a\n0000 DF *UND*\t0 (GLIBC_2.10) b\n";
        let tags = parse_version_tags(only_old);
        assert_eq!(tags.glibc.map(|v| v.to_string()), Some("2.10".to_owned()));
        assert_eq!(tags.glibcxx, None);
        assert_eq!(tags.qt_private_symbols, 0);
        assert_eq!(parse_version_tags(""), VersionTags::default());
    }

    #[test]
    fn a_weak_undefined_symbol_sets_no_glibc_requirement() {
        // objdump -T: the flag columns stand between the address and `*UND*`; `w` is weak.
        let sample = "\
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.38) strlcpy
0000000000000000  w   DF *UND*\t0000000000000000 (GLIBC_2.39) pthread_something
0000000000000000  w   DO *UND*\t0000000000000000 (GLIBCXX_3.4.99) _ZweakData
0000000000000000 gw   DF *UND*\t0000000000000000 (GLIBC_2.40) another_weak
0000000000000000      DF *UND*\t0000000000000000 (GLIBC_2.34) __libc_start_main
";
        let tags = parse_version_tags(sample);
        assert_eq!(tags.glibc, Version::parse("2.38"), "not 2.40, not 2.39");
        assert_eq!(
            tags.glibcxx, None,
            "a weak libstdc++ symbol is no requirement"
        );
        // Three private-API symbols are counted, once each.
        let private = "0 DF *UND*\t0 (Qt_6_PRIVATE_API) a\n0 DF *UND*\t0 (Qt_6_PRIVATE_API) b\n0 DF *UND*\t0 (Qt_6_PRIVATE_API) c\n0 w DF *UND*\t0 (Qt_6_PRIVATE_API) weak\n";
        assert_eq!(parse_version_tags(private).qt_private_symbols, 3);
    }

    #[test]
    fn qml_imports_are_the_modules_without_our_own() {
        let text = "import QtQuick\nimport QtQuick.Controls as QQC2\nimport org.kde.kirigami as Kirigami\nimport \"components\"\nimport app.kourier\nimport app.kourier.Sub 1.0\n  import QtMultimedia 6.0\n// import Nope\nItem { property string importance }\n";
        let found: Vec<String> = qml_imports(text).into_iter().collect();
        assert_eq!(
            found,
            [
                "QtMultimedia",
                "QtQuick",
                "QtQuick.Controls",
                "org.kde.kirigami"
            ]
        );
        // The real sources: system modules, not the application's own.
        let real = qml_modules_in(&root().join("crates/kourier-qt/qml")).expect("qml");
        assert!(
            real.contains("QtQuick") && real.contains("org.kde.kirigami"),
            "{real:?}"
        );
        assert!(
            real.iter().all(|m| !m.starts_with(OWN_QML_MODULE)),
            "{real:?}"
        );
    }

    #[test]
    fn dpkg_output_gives_package_names() {
        let out = "libqt6core6t64:amd64: /usr/lib/x86_64-linux-gnu/libQt6Core.so.6\nqml6-module-qtquick, qml6-module-qtquick-window:amd64: /usr/lib/x86_64-linux-gnu/qt6/qml/QtQuick\n";
        assert_eq!(
            parse_dpkg_search(out),
            [
                "libqt6core6t64",
                "qml6-module-qtquick",
                "qml6-module-qtquick-window"
            ]
        );
        assert!(parse_dpkg_search("").is_empty());
    }

    fn sample_requirements() -> Requirements {
        Requirements {
            libraries: vec![
                Library {
                    soname: "libQt6Core.so.6".into(),
                    packages: vec!["libqt6core6t64".into()],
                },
                Library {
                    soname: "libm.so.6".into(),
                    packages: vec![],
                },
            ],
            library_paths: vec![],
            tags: VersionTags {
                glibc: Version::parse("2.38"),
                glibcxx: Version::parse("3.4.32"),
                qt: Version::parse("6"),
                qt_private_symbols: 3,
            },
            built_with_qt: Some("6.11.1".into()),
            built_with_kf6: Some("6.30.0".into()),
            qml_modules: vec![QmlModule {
                name: "org.kde.kirigami".into(),
                packages: vec!["qml6-module-org-kde-kirigami".into()],
            }],
            desktop_style: QmlModule {
                name: DESKTOP_STYLE_MODULE.into(),
                packages: vec!["qqc2-desktop-style".into()],
            },
        }
    }

    #[test]
    fn the_requirements_text_names_versions_libraries_and_modules() {
        let text = render_requirements(&sample_requirements());
        for want in [
            "libQt6Core.so.6   [libqt6core6t64]",
            "libm.so.6\n",
            "glibc      2.38 or newer",
            "GLIBCXX_3.4.32 or newer",
            "Qt         6.11 (built with 6.11.1). It uses 3 symbols of Qt's private API",
            "(Qt_6_PRIVATE_API)",
            "run it\n             with Qt 6.11, not with an older or a newer minor version",
            "org.kde.kirigami   [qml6-module-org-kde-kirigami]",
            "Also needed at run time",
            "the org.kde.desktop Qt Quick Controls style",
            "[qqc2-desktop-style]",
            "KWallet or GNOME Keyring",
            "built and tried with 6.30.0",
        ] {
            assert!(text.contains(want), "{want:?} not in\n{text}");
        }
        // No tag after which something is claimed about all of Qt 6.
        assert!(
            !text.contains("all of Qt 6") && !text.contains("tag Qt_"),
            "{text}"
        );
        let mut open = sample_requirements();
        open.tags.qt_private_symbols = 0;
        let open = render_requirements(&open);
        assert!(open.contains("it uses no private API of Qt"), "{open}");
        let mut one = sample_requirements();
        one.tags.qt_private_symbols = 1;
        assert!(render_requirements(&one).contains("1 symbol of Qt's private API"));
        let mut unknown = sample_requirements();
        unknown.built_with_qt = None;
        unknown.built_with_kf6 = None;
        let unknown = render_requirements(&unknown);
        assert!(
            unknown.contains("the minor version it was built with"),
            "{unknown}"
        );
        assert!(
            unknown.contains("KDE Frameworks 6 (the libraries above)"),
            "{unknown}"
        );
    }

    #[test]
    fn the_readme_says_what_it_is_what_it_needs_how_to_install_and_remove() {
        let names = archive_names("1.2.3", "x86_64", Grade::Release);
        let files = expected_files(&args(&["uk"]));
        let requirements = sample_requirements();
        let readme_at = |grade, repository| {
            render_readme(&Readme {
                version: "1.2.3",
                commit: "0123456789abcdef0123",
                date: "2026-10-08",
                names: &names,
                grade,
                repository,
                requirements: &requirements,
                files: &files,
            })
        };
        let readme = |grade| readme_at(grade, None);
        let text = readme(Grade::Release);
        for want in [
            "Kourier 1.2.3  (linux, kourier-1.2.3-linux-x86_64)",
            "commit 0123456789ab (2026-10-08)",
            "libQt6Core.so.6",
            "org.kde.kirigami",
            "cp -r bin share ~/.local/",
            "/usr/local",
            "update-desktop-database",
            "share/locale/uk/LC_MESSAGES/kourier.mo",
            "bin/kourier",
            "kourier-1.2.3-source.tar.gz",
            "SHA256SUMS",
            "AGPL-3.0-only",
            "THIRD-PARTY.html",
            "Also needed at run time",
            "built and tried with 6.30.0",
        ] {
            assert!(text.contains(want), "{want:?} not in\n{text}");
        }
        // The link handlers: what the line is and how to add it, in two lines.
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("The desktop entry has no MimeType= line"))
            .expect("the paragraph about the link handlers");
        assert!(
            lines[at].contains("sgnl:// and signalcaptcha://"),
            "{}",
            lines[at]
        );
        assert!(
            lines[at + 1]
                .contains("MimeType=x-scheme-handler/sgnl;x-scheme-handler/signalcaptcha;"),
            "{}",
            lines[at + 1]
        );
        assert!(lines[at + 2].is_empty(), "two lines, then a blank one");
        assert!(!text.contains("delete the line"), "{text}");
        // No grade, no warning; the others say what they are.
        // Where the dependencies' sources come from: the script of the source archive.
        assert!(text.contains(FETCH_SCRIPT), "{text}");
        assert!(
            root().join(FETCH_SCRIPT).is_file(),
            "the script README.txt names"
        );
        assert!(!text.contains("DEVELOPMENT BUILD") && !text.contains("NOT A RELEASE"));
        assert!(readme(Grade::Dirty).contains("DEVELOPMENT BUILD"));
        assert!(readme(Grade::Unreleased).contains("THIS IS NOT A RELEASE"));
        // The repository, when the About data have one; nothing invented when not.
        assert!(!text.contains("The repository is"));
        let with = readme_at(Grade::Release, Some("https://example.org/kourier"));
        assert!(
            with.contains("The repository is https://example.org/kourier"),
            "{with}"
        );
        // The remove list ends without a continuation and does not name the README itself.
        assert!(!text.contains("README.txt \\"), "{text}");
        assert!(text.contains("share/metainfo/app.kourier.Kourier.metainfo.xml\n"));
        // Nothing depends on when or where it was made.
        assert_eq!(text, readme(Grade::Release));
        assert!(!text.contains(&home_of("")), "{text}");
    }

    #[test]
    fn the_expected_tree_is_the_installed_one_plus_licences_readme_and_catalogs() {
        let files = expected_files(&args(&["uk", "de"]));
        assert_eq!(
            files,
            [
                "README.txt",
                "bin/kourier",
                "share/applications/app.kourier.Kourier.desktop",
                "share/icons/hicolor/scalable/apps/app.kourier.Kourier.svg",
                "share/licenses/kourier/LICENSE",
                "share/licenses/kourier/THIRD-PARTY.html",
                "share/locale/de/LC_MESSAGES/kourier.mo",
                "share/locale/uk/LC_MESSAGES/kourier.mo",
                "share/metainfo/app.kourier.Kourier.metainfo.xml",
            ]
        );
        let dirs = expected_directories(&files);
        assert!(dirs.contains("share/icons/hicolor/scalable/apps"));
        assert!(dirs.contains("share/locale/uk/LC_MESSAGES"));
        assert!(dirs.contains("bin") && !dirs.contains(""));
        assert_eq!(dirs.len(), 15);
    }

    #[test]
    fn a_tree_must_match_the_expected_one_exactly() {
        let scratch = Scratch::new("tree");
        let files = expected_files(&args(&["uk"]));
        for f in &files {
            scratch.write(f, b"x");
        }
        let tree = read_tree(&scratch.0).expect("tree");
        assert_eq!(tree_problems(&tree, &files), Vec::<String>::new());
        scratch.write("share/extra.txt", b"x");
        std::fs::create_dir_all(scratch.0.join("empty-dir")).expect("dir");
        std::os::unix::fs::symlink("bin/kourier", scratch.0.join("link")).expect("link");
        std::fs::remove_file(scratch.0.join("README.txt")).expect("remove");
        let problems = tree_problems(&read_tree(&scratch.0).expect("tree"), &files);
        for want in [
            "unexpected file share/extra.txt",
            "missing file README.txt",
            "unexpected directory empty-dir",
            "link is not a regular file or a directory",
        ] {
            assert!(problems.iter().any(|p| p == want), "{want}: {problems:?}");
        }
    }

    #[test]
    fn modes_are_normalised() {
        let scratch = Scratch::new("modes");
        let bin = scratch.write("bin/kourier", b"x");
        let other = scratch.write("share/a/file", b"x");
        for (path, m) in [(&bin, 0o700), (&other, 0o664)] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(m)).expect("mode");
        }
        std::fs::set_permissions(
            scratch.0.join("share"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("mode");
        normalize_modes(&scratch.0, Path::new("bin/kourier")).expect("normalise");
        assert_eq!(mode(&bin), 0o755);
        assert_eq!(mode(&other), 0o644);
        assert_eq!(mode(&scratch.0.join("share")), 0o755);
        assert_eq!(mode(&scratch.0.join("share/a")), 0o755);
        assert_eq!(mode(&scratch.0), 0o755);
    }

    /// The same tree made twice, in a different order and at different times, with other
    /// modes before they are set.
    fn make_tree(parent: &Path, order_reversed: bool) {
        let mut files = vec![
            ("top/README.txt", 0o600),
            ("top/bin/kourier", 0o775),
            ("top/share/z/last", 0o640),
            ("top/share/a/first", 0o664),
        ];
        if order_reversed {
            files.reverse();
        }
        for (rel, m) in files {
            let path = parent.join(rel);
            std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
            std::fs::write(&path, format!("content of {rel}")).expect("file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).expect("mode");
            // Another modification time every time.
            let when = std::time::SystemTime::now() - Duration::from_secs(1000 * (m as u64));
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(when)
                .expect("mtime");
        }
        normalize_modes(&parent.join("top"), Path::new("bin/kourier")).expect("modes");
    }

    #[test]
    fn packing_is_deterministic_and_the_archive_is_clean() {
        let scratch = Scratch::new("pack");
        let (a, b) = (scratch.0.join("a"), scratch.0.join("b"));
        make_tree(&a, false);
        make_tree(&b, true);
        let mtime = 31_536_000; // 1971-01-01 00:00:00 UTC
        let (x, y) = (scratch.0.join("x.tar.gz"), scratch.0.join("y.tar.gz"));
        pack_tree(&a, "top", mtime, &x).expect("pack x");
        pack_tree(&b, "top", mtime, &y).expect("pack y");
        let bytes = std::fs::read(&x).expect("x");
        assert_eq!(bytes, std::fs::read(&y).expect("y"), "twice the same bytes");
        // Packed again from the same stage, later: the same bytes.
        std::thread::sleep(Duration::from_millis(1100));
        pack_tree(&a, "top", mtime, &x).expect("pack x again");
        assert_eq!(bytes, std::fs::read(&x).expect("x"));
        assert_ne!(
            bytes,
            {
                pack_tree(&a, "top", mtime + 1, &y).expect("pack y");
                std::fs::read(&y).expect("y")
            },
            "the time stamp is part of the bytes"
        );
        // gzip: no file name (flag 3, bit 3) and no time stamp (bytes 4 to 7).
        assert_eq!(&bytes[..2], &[0x1f, 0x8b]);
        assert_eq!(bytes[3] & 0x08, 0, "no name in the gzip header");
        assert_eq!(&bytes[4..8], &[0, 0, 0, 0], "no time in the gzip header");

        let listing = stdout_of(
            Command::new("tar")
                .env("TZ", "UTC")
                .env("LC_ALL", "C")
                .args([
                    "--list",
                    "--verbose",
                    "--numeric-owner",
                    "--full-time",
                    "--gzip",
                    "--file",
                ])
                .arg(&x),
            "tar --list",
        )
        .expect("listing");
        let lines: Vec<Vec<&str>> = listing
            .lines()
            .map(|l| l.split_whitespace().collect())
            .collect();
        let names: Vec<&str> = lines.iter().map(|w| *w.last().expect("name")).collect();
        assert_eq!(
            names,
            [
                "top/",
                "top/README.txt",
                "top/bin/",
                "top/bin/kourier",
                "top/share/",
                "top/share/a/",
                "top/share/a/first",
                "top/share/z/",
                "top/share/z/last",
            ],
            "sorted by name"
        );
        for w in &lines {
            let name = w.last().expect("name");
            assert!(
                !name.starts_with('/') && !name.split('/').any(|p| p == ".."),
                "{name}"
            );
            assert_eq!(w[1], "0/0", "owner and group 0: {w:?}");
            assert_eq!(&w[3..5], ["1971-01-01", "00:00:00"], "time stamp: {w:?}");
            let expected = if name.ends_with('/') || *name == "top/bin/kourier" {
                "755"
            } else {
                "644"
            };
            let bits: u32 = w[0][1..]
                .chars()
                .enumerate()
                .map(|(i, c)| if c == '-' { 0 } else { 1 << (8 - i) })
                .sum();
            assert_eq!(format!("{bits:o}"), expected, "mode of {name}: {w:?}");
        }
    }

    #[test]
    fn checksums_are_sorted_and_checkable() {
        let scratch = Scratch::new("sums");
        scratch.write("b.txt", b"abc");
        scratch.write("a.txt", b"");
        let sums = checksums(&scratch.0, &["b.txt", "a.txt"]).expect("sums");
        assert_eq!(
            sums,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  a.txt\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  b.txt\n"
        );
        scratch.write("SHA256SUMS", sums.as_bytes());
        let ok = Command::new("sha256sum")
            .current_dir(&scratch.0)
            .args(["--check", "--strict", "--quiet", "SHA256SUMS"])
            .status()
            .expect("sha256sum");
        assert!(ok.success());
    }

    fn script(scratch: &Scratch, body: &str) -> PathBuf {
        let path = scratch.write("kourier", format!("#!/bin/sh\n{body}\n").as_bytes());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        path
    }

    #[test]
    fn the_version_output_is_checked_with_a_time_limit() {
        let scratch = Scratch::new("version");
        let limit = Duration::from_secs(10);
        let good = script(&scratch, "echo 'Kourier 1.2.3'");
        check_version_output(&good, "1.2.3", &scratch.0, limit).expect("good");
        let err = check_version_output(&good, "1.2.4", &scratch.0, limit).expect_err("wrong");
        assert!(err.to_string().contains("printed"), "{err}");
        let failing = script(&scratch, "echo 'Kourier 1.2.3'; exit 3");
        assert!(check_version_output(&failing, "1.2.3", &scratch.0, limit).is_err());
        let noisy = script(&scratch, "echo 'Kourier 1.2.3 and more'");
        assert!(check_version_output(&noisy, "1.2.3", &scratch.0, limit).is_err());
        // The program gets an empty environment and the given home.
        let env = script(&scratch, "echo \"Kourier $HOME $USER $QT_QPA_PLATFORM\"");
        let err = check_version_output(&env, "x", &scratch.0, limit).expect_err("env");
        assert!(
            err.to_string()
                .contains(&format!("Kourier {}", scratch.0.display())),
            "{err}"
        );
        let slow = script(&scratch, "sleep 20");
        let started = Instant::now();
        let err = check_version_output(&slow, "1.2.3", &scratch.0, Duration::from_millis(300))
            .expect_err("slow");
        assert!(format!("{err:#}").contains("did not finish"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(check_version_output(&scratch.0.join("missing"), "1", &scratch.0, limit).is_err());
    }

    #[test]
    fn findings_are_blanks_the_placeholder_id_and_private_paths_in_tracked_files() {
        let scratch = Scratch::new("notready");
        let me = who("carol");
        let blank = format!("a {BLANK_MARK}: name\nb {BLANK_MARK}\n");
        scratch.write("a.md", blank.as_bytes());
        scratch.write(
            "b.md",
            format!("see {}/notes\nfine\n", home_of("carol")).as_bytes(),
        );
        scratch.write(
            "c.rs",
            [
                format!("let p = \"{}/Folder\";\n", home_of("u")),
                format!("let q = \"{}/pic.png\";\n", home_of("alice")),
                "let r = \"file:///media/ab/cd\";\n".to_owned(),
                format!("let s = \"{}/.cargo/registry\";\n", home_of("")),
                "let t = \"$XDG_RUNTIME_DIR/kourier/media/x\";\n".to_owned(),
            ]
            .concat()
            .as_bytes(),
        );
        scratch.write("d.md", ["go to ", "~/", "projects/x\n"].concat().as_bytes());
        scratch.write("e.md", format!("{}/code\n", home_of("dave")).as_bytes());
        scratch.write("f.md", "on /media/carol/disk and /media/ab/cd\n".as_bytes());
        let mut binary = vec![0u8; 4];
        binary.extend_from_slice(home_of("carol").as_bytes());
        scratch.write("bin.dat", &binary);
        let tracked = args(&[
            "a.md", "b.md", "c.rs", "d.md", "e.md", "f.md", "bin.dat", "gone.md",
        ]);
        let found =
            not_ready_findings(&scratch.0, &tracked, PLACEHOLDER_APP_ID, &me, None).expect("scan");
        assert_eq!(found.blanks, [("a.md".to_owned(), 2)]);
        assert_eq!(found.blank_total(), 2);
        assert!(found.placeholder_app_id);
        let paths: Vec<&str> = found
            .private_files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(paths, ["b.md", "d.md", "e.md", "f.md"], "{found:#?}");
        assert_eq!(found.private_files[0].hits[0].0, 1, "the line number");
        assert!(found.private_files[0].hits[0].1.contains(&home_of("carol")));
        assert!(found.no_repository, "no address was given");
        assert_eq!(found.kinds(), 4);
        assert!(!found.is_empty());
        let text = found.render();
        for want in [
            "NOT READY TO PUBLISH",
            "1. 2 blanks marked",
            "2. The application id is still the placeholder",
            "3. 4 tracked files name a home directory",
            "     a.md: 2",
            "     b.md (1 lines)",
            "4. There is no repository address",
            "README.txt of the binary archive cannot say where its source is",
        ] {
            assert!(text.contains(want), "{want:?} not in\n{text}");
        }
        // Another id, no blanks, no paths, an address: nothing found.
        let clean = not_ready_findings(
            &scratch.0,
            &args(&["c.rs", "bin.dat"]),
            "org.example.App",
            &me,
            Some("https://example.org/kourier"),
        )
        .expect("scan");
        assert!(clean.is_empty() && clean.kinds() == 0, "{clean:#?}");
        assert!(NotReady::default().is_empty());
        // Only the address is missing: that alone is a finding, and an empty one counts.
        for homepage in [None, Some(""), Some("  ")] {
            let only = not_ready_findings(
                &scratch.0,
                &args(&["c.rs"]),
                "org.example.App",
                &me,
                homepage,
            )
            .expect("scan");
            assert_eq!(
                (only.kinds(), only.no_repository),
                (1, true),
                "{homepage:?}"
            );
        }
    }

    #[test]
    fn the_repository_address_is_read_from_the_about_data() {
        let none = "pub const fn info() -> AboutInfo {\n    AboutInfo {\n        homepage: None,\n        bug_report: None,\n    }\n}\n";
        assert_eq!(about_homepage(none), None);
        let some = "    pub homepage: Option<&'static str>,\n        homepage: Some(\"https://example.org/kourier\"),\n";
        assert_eq!(
            about_homepage(some).as_deref(),
            Some("https://example.org/kourier")
        );
        // The real file: the reading follows what it says, whether it has an address or not.
        let real = std::fs::read_to_string(root().join(ABOUT_SOURCE)).expect("about.rs");
        assert!(real.lines().any(|l| l.trim().starts_with("homepage:")));
        assert_eq!(
            about_homepage(&real).is_some(),
            real.lines()
                .any(|l| l.trim().starts_with("homepage: Some(\""))
        );
    }

    #[test]
    fn a_release_with_findings_is_an_error_and_leaves_no_archive() {
        let scratch = Scratch::new("gate");
        let out = scratch.0.join("out");
        let names = archive_names("0.1.0", "x86_64", Grade::Release);
        let findings = NotReady {
            placeholder_app_id: true,
            ..NotReady::default()
        };
        let leftovers = |out: &Path| {
            std::fs::create_dir_all(out).expect("out");
            for name in [
                &names.binary_archive,
                &names.source_archive,
                "SHA256SUMS",
                "NOT-READY.txt",
            ] {
                std::fs::write(out.join(name), b"from an earlier run").expect("file");
            }
        };
        let names_in = |out: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(out)
                .expect("read")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        // --release: an error, the findings written, the old archives gone.
        leftovers(&out);
        clear_outputs(&out).expect("clear");
        let err = report_findings(&findings, true, &out).expect_err("release");
        assert!(err.to_string().contains("nothing was built"), "{err}");
        assert_eq!(names_in(&out), ["NOT-READY.txt"]);
        let text = std::fs::read_to_string(out.join("NOT-READY.txt")).expect("file");
        assert!(text.contains("placeholder"), "{text}");
        // Without --release: a warning; the file is there.
        clear_outputs(&out).expect("clear");
        report_findings(&findings, false, &out).expect("warning");
        assert_eq!(names_in(&out), ["NOT-READY.txt"]);
        // Nothing found: no file, even when an earlier run left one.
        leftovers(&out);
        clear_outputs(&out).expect("clear");
        report_findings(&NotReady::default(), true, &out).expect("nothing to report");
        assert!(names_in(&out).is_empty());
    }

    #[test]
    fn this_file_names_no_blank_and_no_private_path() {
        // Nobody of this name is in the fixtures of the tests.
        let me = who("nobody-at-all");
        let text = include_str!("dist.rs");
        assert_eq!(text.matches(BLANK_MARK).count(), 0);
        // Nor the project's former name, in any case.
        let former = FORMER_NAME.concat();
        assert!(!text.to_ascii_lowercase().contains(&former));
        for (i, line) in text.lines().enumerate() {
            assert!(
                private_path_hits(line, &me).is_empty(),
                "line {}: {line}",
                i + 1
            );
        }
    }

    /// The environment variable that lets a test run without a tool it needs.
    const ALLOW_MISSING_TOOLS: &str = "KOURIER_XTASK_ALLOW_MISSING_TOOLS";

    /// Whether the tool a test needs is installed. When it is not, the test FAILS (a test that
    /// passes without its validator checks nothing), unless the environment variable above
    /// says that is known; then this says so and the test goes on without the part that
    /// needs the tool.
    fn have_tool(name: &str) -> bool {
        if tool_exists(name) {
            return true;
        }
        assert!(
            std::env::var_os(ALLOW_MISSING_TOOLS).is_some(),
            "{name} is not installed, so this test would check nothing: install it, or set {ALLOW_MISSING_TOOLS}=1 to skip that part knowingly"
        );
        eprintln!(
            "{name} is not installed; {ALLOW_MISSING_TOOLS} is set: that part is NOT checked"
        );
        false
    }

    /// The program of a fake release: refuses `--fixture` like a build without the test
    /// network, else runs `body`.
    fn fake_program(body: &str) -> String {
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--fixture\" ]; then echo 'kourier: {FIXTURE_REFUSAL}' >&2; exit 1; fi\n{body}\n"
        )
    }

    /// Notices as the real page has them: the section of the bundled libraries inside `<main>`.
    fn sample_notices() -> String {
        insert_section(
            "<html><body><main><h1>Third-party licenses</h1></main></body></html>",
            &render_bundled_section(&sample_bundled()),
        )
        .expect("notices")
    }

    fn sample_bundled() -> Vec<BundledLibrary> {
        let text = |title: &str, text: &str| BundledText {
            title: title.to_owned(),
            source: title.to_owned(),
            text: text.to_owned(),
        };
        vec![
            BundledLibrary {
                name: "SQLCipher".into(),
                licence: "BSD-3-Clause".into(),
                note: "by Zetetic LLC".into(),
                texts: vec![text(
                    "SQLCipher (Zetetic LLC): sqlcipher/LICENSE",
                    "Copyright (c) Zetetic LLC <&>",
                )],
            },
            BundledLibrary {
                name: "BoringSSL".into(),
                licence: "Apache-2.0".into(),
                note: "BoringSSL is inside libsignal".into(),
                texts: vec![text("BoringSSL: LICENSE", "Apache License")],
            },
        ]
    }

    /// A stage made of the real packaging files and a script as the binary, packed and
    /// unpacked as `dist` does it, but without a build.
    fn fake_release(
        scratch: &Scratch,
        binary_body: &str,
        readme: &str,
        tamper: &dyn Fn(&Path),
    ) -> (PathBuf, Names, Vec<u8>) {
        let names = archive_names("0.1.0", "x86_64", Grade::Release);
        let target = scratch.0.join("build");
        std::fs::create_dir_all(target.join("release")).expect("dir");
        let bin = target.join("release").join(BINARY);
        std::fs::write(&bin, fake_program(binary_body)).expect("binary");
        let locale = scratch.0.join("locale");
        std::fs::create_dir_all(locale.join("uk/LC_MESSAGES")).expect("dir");
        std::fs::write(locale.join("uk/LC_MESSAGES/kourier.mo"), b"mo").expect("mo");
        let parent = scratch.0.join("stage");
        let _ = std::fs::remove_dir_all(&parent);
        let stage = parent.join(&names.stage);
        std::fs::create_dir_all(&stage).expect("stage");
        let sources = Sources::in_repo(&root(), &target);
        stage_files(&stage, &sources, &locale, &args(&["uk"])).expect("stage");
        std::fs::write(
            stage.join("share/licenses/kourier/THIRD-PARTY.html"),
            sample_notices(),
        )
        .expect("tp");
        std::fs::write(stage.join("README.txt"), readme).expect("readme");
        tamper(&stage);
        let built = std::fs::read(stage.join("bin").join(BINARY)).expect("read");
        normalize_modes(&stage, &Path::new("bin").join(BINARY)).expect("modes");
        let archive = scratch.0.join(&names.binary_archive);
        pack_tree(&parent, &names.stage, 1_791_476_348, &archive).expect("pack");
        (archive, names, built)
    }

    /// `verify_unpacked` on a fake release made with these parts.
    fn check_fake(
        scratch: &Scratch,
        body: &str,
        readme: &str,
        languages: &[&str],
        tamper: &dyn Fn(&Path),
    ) -> Result<Verified> {
        let needles = private_needles(&who("carol"));
        let (archive, names, built) = fake_release(scratch, body, readme, tamper);
        verify_unpacked(
            &archive,
            &scratch.0.join("work"),
            &names,
            "0.1.0",
            &args(languages),
            &built,
            &needles,
        )
    }

    /// Everything `verify` checks that needs no validator: this runs on every machine.
    #[test]
    fn what_was_packed_is_unpacked_and_checked_without_validators() {
        let scratch = Scratch::new("verify-plain");
        let verified = check_fake(
            &scratch,
            "echo 'Kourier 0.1.0'",
            "readme\n",
            &["uk"],
            &|_| {},
        )
        .expect("a good release");
        assert!(verified.scans.is_empty());
        // The desktop file of the archive starts the program by name, not by a path, and
        // claims no link scheme.
        let desktop = std::fs::read_to_string(
            verified
                .root_dir
                .join("share/applications/app.kourier.Kourier.desktop"),
        )
        .expect("desktop");
        assert!(desktop.lines().any(|l| l == "Exec=kourier %U"), "{desktop}");
        assert!(desktop.lines().any(|l| l == "TryExec=kourier"), "{desktop}");
        assert!(!desktop.contains("MimeType"), "{desktop}");
        assert!(
            !desktop.contains("sgnl") && !desktop.contains("signalcaptcha"),
            "{desktop}"
        );

        // A binary that prints another version fails.
        let err = check_fake(
            &scratch,
            "echo 'Kourier 9.9.9'",
            "readme\n",
            &["uk"],
            &|_| {},
        )
        .expect_err("wrong version");
        assert!(format!("{err:#}").contains("printed"), "{err:#}");
        // So does a catalog nobody asked for.
        let err = check_fake(
            &scratch,
            "echo 'Kourier 0.1.0'",
            "readme\n",
            &["uk", "de"],
            &|_| {},
        )
        .expect_err("missing catalog");
        assert!(
            format!("{err:#}").contains("missing file share/locale/de"),
            "{err:#}"
        );
        // A measuring build is refused.
        let marker = String::from_utf8_lossy(install_marker()).into_owned();
        let err = check_fake(
            &scratch,
            "echo 'Kourier 0.1.0'",
            "readme\n",
            &["uk"],
            &|stage| {
                let bin = stage.join("bin").join(BINARY);
                std::fs::write(
                    &bin,
                    fake_program(&format!("echo 'Kourier 0.1.0' # {marker}")),
                )
                .expect("tamper");
            },
        )
        .expect_err("measuring build");
        assert!(format!("{err:#}").contains("measuring build"), "{err:#}");
        // A private path in any packed file is reported.
        let leaky = format!("built in {}/src\n", home_of("carol"));
        let verified = check_fake(&scratch, "echo 'Kourier 0.1.0'", &leaky, &["uk"], &|_| {})
            .expect("still a valid tree");
        assert_eq!(verified.scans.len(), 1);
        assert_eq!(verified.scans[0].0, "README.txt");
        assert!(verified.scans[0].1.total() >= 2);
    }

    #[test]
    fn the_public_archive_must_refuse_the_test_network_claim_no_scheme_and_name_what_it_bundles() {
        let scratch = Scratch::new("verify-gates");
        // A binary that accepts `--fixture` (the test network is built in) is refused.
        let err = check_fake(&scratch, "echo 'Kourier 0.1.0'", "r\n", &["uk"], &|stage| {
            std::fs::write(
                stage.join("bin").join(BINARY),
                "#!/bin/sh\necho 'Kourier 0.1.0'\n",
            )
            .expect("tamper");
        })
        .expect_err("fixture accepted");
        assert!(
            format!("{err:#}").contains("--fixture must be refused"),
            "{err:#}"
        );
        // A refusal for another reason (here: no display) is not the refusal.
        let err = check_fake(&scratch, "echo 'Kourier 0.1.0'", "r\n", &["uk"], &|stage| {
            std::fs::write(
                stage.join("bin").join(BINARY),
                "#!/bin/sh\nif [ \"$1\" = \"--fixture\" ]; then echo 'no display' >&2; exit 1; fi\necho 'Kourier 0.1.0'\n",
            )
            .expect("tamper");
        })
        .expect_err("other failure");
        assert!(
            format!("{err:#}").contains("--fixture must be refused"),
            "{err:#}"
        );
        // A desktop file that claims a scheme.
        for scheme in install::SCHEMES {
            let err = check_fake(&scratch, "echo 'Kourier 0.1.0'", "r\n", &["uk"], &|stage| {
                let layout = install::Layout::new(stage);
                let mut text = std::fs::read_to_string(&layout.desktop).expect("desktop");
                text.push_str(&format!("MimeType=x-scheme-handler/{scheme};\n"));
                std::fs::write(&layout.desktop, text).expect("tamper");
            })
            .expect_err("claims a scheme");
            assert!(
                format!("{err:#}").contains(&format!("x-scheme-handler/{scheme}")),
                "{err:#}"
            );
        }
        // Notices without SQLCipher's or BoringSSL's names.
        for missing in REQUIRED_NOTICE_WORDS {
            let err = check_fake(&scratch, "echo 'Kourier 0.1.0'", "r\n", &["uk"], &|stage| {
                let page = stage.join("share/licenses/kourier/THIRD-PARTY.html");
                let text = std::fs::read_to_string(&page).expect("page");
                std::fs::write(&page, text.replace(missing, "X")).expect("tamper");
            })
            .expect_err("a notice is missing");
            assert!(
                format!("{err:#}").contains(&format!("does not mention {missing}")),
                "{err:#}"
            );
        }
        assert!(notice_problems(&sample_notices()).is_empty());
        assert_eq!(notice_problems("<html></html>").len(), 2);
    }

    /// The validators run on the same fake release: this needs `desktop-file-validate` and
    /// `appstreamcli`, and fails without them unless the opt-out is set.
    #[test]
    fn what_was_packed_is_unpacked_and_checked() {
        let have = have_tool("desktop-file-validate") & have_tool("appstreamcli");
        if !have {
            return;
        }
        let scratch = Scratch::new("verify");
        let needles = private_needles(&who("carol"));
        let (archive, names, built) =
            fake_release(&scratch, "echo 'Kourier 0.1.0'", "readme\n", &|_| {});
        let work = scratch.0.join("work");
        verify(
            &archive,
            &work,
            &names,
            "0.1.0",
            &args(&["uk"]),
            &built,
            &needles,
        )
        .expect("a good release passes both validators");
        // A desktop file the validator rejects fails the check.
        let (archive, names, built) =
            fake_release(&scratch, "echo 'Kourier 0.1.0'", "readme\n", &|stage| {
                let layout = install::Layout::new(stage);
                let text = std::fs::read_to_string(&layout.desktop).expect("desktop");
                std::fs::write(
                    &layout.desktop,
                    text.replace("Type=Application", "Type=Nonsense"),
                )
                .expect("tamper");
            });
        let err = verify(
            &archive,
            &work,
            &names,
            "0.1.0",
            &args(&["uk"]),
            &built,
            &needles,
        )
        .expect_err("a bad desktop file");
        assert!(
            format!("{err:#}").contains("desktop-file-validate"),
            "{err:#}"
        );
        // And a metainfo file it rejects.
        let (archive, names, built) =
            fake_release(&scratch, "echo 'Kourier 0.1.0'", "readme\n", &|stage| {
                let layout = install::Layout::new(stage);
                std::fs::write(&layout.metainfo, "<component><id>x</id>").expect("tamper");
            });
        let err = verify(
            &archive,
            &work,
            &names,
            "0.1.0",
            &args(&["uk"]),
            &built,
            &needles,
        )
        .expect_err("a bad metainfo");
        assert!(format!("{err:#}").contains("appstreamcli"), "{err:#}");
    }

    // ---- the output directory ------------------------------------------------------------

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// Leftovers of earlier runs under names this run would not write (another version, a
    /// dirty one, an unreleased one), and files that are not the tool's.
    fn leftovers_and_bystanders(out: &Path) {
        std::fs::create_dir_all(out).expect("out");
        for ours in [
            "kourier-0.0.9-linux-x86_64.tar.gz",
            "kourier-0.1.0-linux-x86_64-dirty.tar.gz",
            "kourier-0.1.0-source-dirty.tar.gz",
            "kourier-0.1.0-linux-x86_64-unreleased.tar.gz",
            "kourier-0.1.0-source.tar.gz",
            "kourier-0.1.0-source.tar.gz.partial",
            "SHA256SUMS",
            "NOT-READY.txt",
        ] {
            std::fs::write(out.join(ours), b"from an earlier run").expect("file");
        }
        for dir in [STAGE_DIR, VERIFY_DIR] {
            std::fs::create_dir_all(out.join(dir).join("deep/er")).expect("dir");
            std::fs::write(out.join(dir).join("deep/er/file"), b"x").expect("file");
        }
        for theirs in [
            "notes.txt",
            "kourier-notes.txt",
            "other.tar.gz",
            "SHA256SUMS.asc",
            "NOT-READY.md",
            "my-kourier-1.tar.gz",
        ] {
            std::fs::write(out.join(theirs), b"the user's").expect("file");
        }
        // A directory with an archive's name is not a file of ours.
        std::fs::create_dir_all(out.join("kourier-keep.tar.gz")).expect("dir");
        std::fs::write(out.join("kourier-keep.tar.gz/inside"), b"x").expect("file");
        std::fs::create_dir_all(out.join("sub")).expect("dir");
        std::fs::write(out.join("sub/kourier-1.0.tar.gz"), b"x").expect("file");
    }

    const BYSTANDERS: [&str; 8] = [
        "NOT-READY.md",
        "SHA256SUMS.asc",
        "kourier-keep.tar.gz",
        "kourier-notes.txt",
        "my-kourier-1.tar.gz",
        "notes.txt",
        "other.tar.gz",
        "sub",
    ];

    #[test]
    fn what_an_earlier_run_left_goes_whatever_its_names_and_nothing_else_does() {
        assert!(is_dist_output("kourier-9.9.9-linux-aarch64-dirty.tar.gz"));
        assert!(is_dist_output("kourier-0.1.0-source-unreleased.tar.gz"));
        assert!(is_dist_output("SHA256SUMS") && is_dist_output("NOT-READY.txt"));
        for not in [
            "other.tar.gz",
            "kourier-x.zip",
            "SHA256SUMS.asc",
            "xkourier-1.tar.gz",
        ] {
            assert!(!is_dist_output(not), "{not}");
        }
        let scratch = Scratch::new("clear");
        let out = scratch.0.join("out");
        leftovers_and_bystanders(&out);
        let removed = clear_outputs(&out).expect("cleared");
        assert!(removed.contains(&"kourier-0.0.9-linux-x86_64.tar.gz".to_owned()));
        assert!(removed.contains(&".dist-stage/".to_owned()));
        assert_eq!(removed.len(), 10, "{removed:?}");
        assert_eq!(names_in(&out), BYSTANDERS);
        assert!(out.join("kourier-keep.tar.gz/inside").exists());
        assert!(out.join("sub/kourier-1.0.tar.gz").exists());
        // A second time there is nothing to do; a directory that is not there is no error.
        assert!(clear_outputs(&out).expect("again").is_empty());
        assert!(
            clear_outputs(&scratch.0.join("nowhere"))
                .expect("none")
                .is_empty()
        );
        // A link is removed as a link, whatever it points at.
        let target = scratch.0.join("precious");
        std::fs::write(&target, b"keep").expect("file");
        std::os::unix::fs::symlink(&target, out.join("kourier-link.tar.gz")).expect("link");
        std::os::unix::fs::symlink(scratch.0.join("out"), out.join(STAGE_DIR)).expect("link");
        clear_outputs(&out).expect("links");
        assert!(target.exists() && out.join("notes.txt").exists());
        assert!(!out.join("kourier-link.tar.gz").exists() && !out.join(STAGE_DIR).exists());
    }

    /// `dist` itself: the output directory is emptied before an option that does not go
    /// together, or an argument nobody knows, can return.
    #[test]
    fn dist_empties_the_output_before_it_judges_its_options() {
        let scratch = Scratch::new("first-act");
        let out = scratch.0.join("out");
        let out_arg = out.to_string_lossy().into_owned();
        for bad in [
            vec!["--release", "--allow-dirty"],
            vec!["--release", "--no-build"],
            vec!["--allow-paths", "--release"],
            vec!["--push"],
        ] {
            leftovers_and_bystanders(&out);
            let mut a = vec!["--out", out_arg.as_str()];
            a.extend(bad.iter());
            let err = dist(&args(&a)).expect_err("a refusal");
            let text = format!("{err:#}");
            assert!(
                text.contains("exclude each other") || text.contains("unknown argument"),
                "{bad:?}: {text}"
            );
            assert_eq!(names_in(&out), BYSTANDERS, "{bad:?}");
        }
    }

    // ---- the stamp -----------------------------------------------------------------------

    fn sample_stamp() -> Stamp {
        Stamp {
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            dirty: false,
            cargo_features: build_features(),
            qt: "6.11.1".into(),
            kf6: "6.30.0".into(),
            rustc: "rustc 1.99.0 (abc 2026-09-01)".into(),
            binary_sha256: "ab".repeat(32),
        }
    }

    #[test]
    fn the_stamp_names_the_commit_the_state_the_features_and_the_binary() {
        let stamp = sample_stamp();
        let text = stamp.render();
        assert_eq!(Stamp::parse(&text).expect("round trip"), stamp);
        assert!(
            text.contains("features=--no-default-features --features signal"),
            "{text}"
        );
        let (c, f, h) = (stamp.commit.clone(), build_features(), "ab".repeat(32));
        stamp.check(&c, false, &f, &h).expect("matches");
        for (what, result) in [
            ("commit", stamp.check(&"f".repeat(40), false, &f, &h)),
            ("tree with changes", stamp.check(&c, true, &f, &h)),
            (
                "--features",
                stamp.check(&c, false, "--features signal,fake", &h),
            ),
            ("checksum", stamp.check(&c, false, &f, &"cd".repeat(32))),
        ] {
            let err = result.expect_err(what);
            let text = format!("{err}");
            assert!(
                text.contains(what) || text.contains("built"),
                "{what}: {text}"
            );
            assert!(text.contains("drop --no-build"), "{text}");
        }
        let mut dirty = sample_stamp();
        dirty.dirty = true;
        dirty.check(&c, true, &f, &h).expect("dirty matches dirty");
        assert!(dirty.check(&c, false, &f, &h).is_err());
        // A stamp that is not whole, doubled or foreign is no stamp.
        for bad in [
            "".to_owned(),
            "something else\ncommit=x\n".to_owned(),
            text.replace("kf6=6.30.0\n", ""),
            text.replace("dirty=false", "dirty=maybe"),
            format!("{text}qt=6.11.2\n"),
            text.replace("qt=6.11.1", "qt="),
            format!("{text}no equals sign\n"),
        ] {
            assert!(Stamp::parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_stamp_sits_beside_the_binary() {
        assert_eq!(
            stamp_path(Path::new("/b")),
            Path::new("/b/release/kourier.dist-stamp")
        );
        let scratch = Scratch::new("stamp-hash");
        let file = scratch.write("bin", b"abc");
        assert_eq!(
            sha256_of(&file).expect("sum"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_build_that_moved_the_commit_or_the_tree_is_refused() {
        let start = Commit {
            hash: "a".repeat(40),
            time: 0,
            date: "2026-10-08".into(),
            changes: vec![" M src/a.rs".into()],
        };
        same_state(&start, &"a".repeat(40), &[" M src/a.rs".to_owned()]).expect("same");
        let err = same_state(&start, &"b".repeat(40), &[" M src/a.rs".to_owned()])
            .expect_err("another commit");
        assert!(format!("{err}").contains("HEAD moved"), "{err}");
        for now in [
            vec![],
            vec![" M src/a.rs".to_owned(), "?? new".to_owned()],
            vec![" M src/b.rs".to_owned()],
        ] {
            let err = same_state(&start, &"a".repeat(40), &now).expect_err("changed tree");
            assert!(format!("{err}").contains("working tree changed"), "{err}");
        }
        assert!(is_commit_hash(
            &"0123456789abcdef"
                .repeat(2)
                .chars()
                .chain("01234567".chars())
                .collect::<String>()
        ));
        for bad in ["HEAD", "", "--output=x", "0123"] {
            assert!(!is_commit_hash(bad), "{bad}");
        }
    }

    #[test]
    fn the_variables_that_would_move_or_change_the_build_are_cleared() {
        let present = [
            "PATH",
            "HOME",
            "CARGO",
            "CARGO_HOME",
            "QMAKE",
            "CFLAGS",
            "RUSTUP_TOOLCHAIN",
            "RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_BUILD_JOBS",
            "CARGO_PROFILE_RELEASE_LTO",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC",
        ];
        let removed = build_env_removals(present);
        for gone in [
            "RUSTFLAGS",
            "CARGO_BUILD_TARGET",
            "CARGO_BUILD_JOBS",
            "CARGO_PROFILE_RELEASE_LTO",
            "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC",
        ] {
            assert!(
                removed.iter().any(|v| v == gone),
                "{gone} not in {removed:?}"
            );
        }
        for kept in [
            "PATH",
            "HOME",
            "CARGO",
            "CARGO_HOME",
            "QMAKE",
            "CFLAGS",
            "RUSTUP_TOOLCHAIN",
        ] {
            assert!(!removed.iter().any(|v| v == kept), "{kept} in {removed:?}");
        }
        // The ones that move the output are named even when the environment lacks them.
        let none = build_env_removals([]);
        for always in [
            "CARGO_BUILD_TARGET",
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "RUSTFLAGS",
        ] {
            assert!(none.iter().any(|v| v == always), "{always}");
        }
    }

    #[test]
    fn the_public_build_has_the_signal_backend_and_not_the_test_network() {
        let features = build_features();
        assert!(features.contains("--no-default-features"), "{features}");
        assert_eq!(BUILD_FEATURE_ARGS[1], "--features");
        assert_eq!(BUILD_FEATURE_ARGS[2], "signal");
        let manifest =
            std::fs::read_to_string(root().join("crates/kourier/Cargo.toml")).expect("manifest");
        assert!(manifest.lines().any(|l| l.starts_with("signal = ")));
        assert!(manifest.lines().any(|l| l.starts_with("fake = ")));
        // What the program says to `--fixture` when `fake` is left out is what the check wants.
        let app =
            std::fs::read_to_string(root().join("crates/kourier-app/src/app.rs")).expect("app.rs");
        assert!(app.contains(FIXTURE_REFUSAL), "{FIXTURE_REFUSAL}");
        let main =
            std::fs::read_to_string(root().join("crates/kourier/src/main.rs")).expect("main.rs");
        assert!(
            main.contains("kourier: {e}"),
            "the refusal reaches stderr with this prefix"
        );
    }

    #[test]
    fn the_kde_frameworks_version_is_read_from_the_cmake_package() {
        let cmake = "# comment\nset(PACKAGE_VERSION \"6.30.0\")\nif(PACKAGE_VERSION VERSION_LESS PACKAGE_FIND_VERSION)\n";
        assert_eq!(cmake_package_version(cmake).as_deref(), Some("6.30.0"));
        assert_eq!(cmake_package_version("nothing\n"), None);
        assert_eq!(cmake_package_version("set(PACKAGE_VERSION 6.30.0)"), None);
        assert_eq!(minor_of("6.11.1"), "6.11");
        assert_eq!(minor_of("6"), "6");
    }

    // ---- the wider search ----------------------------------------------------------------

    #[test]
    fn the_wider_search_finds_host_git_identity_and_the_former_name_without_regard_to_case() {
        let extra = Extra {
            host: Some("build-box".into()),
            git_name: Some("Ada Lovelace".into()),
            git_email: Some("ada@example.org".into()),
        };
        let needles = extra_needles(&extra);
        let labels: Vec<&str> = needles.iter().map(|n| n.label).collect();
        assert_eq!(
            labels,
            [
                "the host name",
                "git's user.name",
                "git's user.email",
                "the former project name"
            ]
        );
        let former = FORMER_NAME.concat();
        let mut title = former.clone();
        title[..1].make_ascii_uppercase();
        let hay = format!(
            "\0at BUILD-Box now\0by ada LOVELACE\0mail ADA@example.ORG\0{}\0{title}\0{}\0clean text\0",
            former.to_uppercase(),
            former
        );
        let scan = scan_bytes(hay.as_bytes(), &needles, 8);
        assert_eq!(
            scan.counts,
            [
                ("the host name", 1),
                ("git's user.name", 1),
                ("git's user.email", 1),
                ("the former project name", 3)
            ]
        );
        assert_eq!(scan.total(), 6);
        // A clean binary finds nothing, and the base needles still count exactly.
        let clean = scan_bytes(b"\0/kourier/crates/x.rs\0Qt\0", &needles, 8);
        assert_eq!(clean.total(), 0);
        let both = all_needles(&who("carol"), &extra);
        assert_eq!(both.len(), 3 + 4);
        assert!(both.iter().take(3).all(|n| !n.fold_case));
        // Values too short to be told from other text are left out (the label list shows it).
        let short = extra_needles(&Extra {
            host: Some("pc".into()),
            git_name: Some("Al".into()),
            git_email: None,
        });
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].label, "the former project name");
        assert!(extra_needles(&Extra::default()).len() == 1);
    }

    fn zstd_compress(bytes: &[u8]) -> Vec<u8> {
        let mut child = Command::new("zstd")
            .args(["--quiet", "--stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("zstd");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write");
        let out = child.wait_with_output().expect("zstd output");
        assert!(out.status.success());
        out.stdout
    }

    #[test]
    fn what_is_inside_a_zstd_frame_is_searched_too() {
        if !have_tool("zstd") {
            return;
        }
        // Text with a skewed alphabet and no repeats, so zstd Huffman-codes its literals
        // (a short text is stored as it is, and the plain search would find it).
        let mut x: u32 = 7;
        let mut filler = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    b"eeeetttaaoinsh"[(x >> 16) as usize % 14]
                })
                .collect()
        };
        let secret = format!("import \"{}/code/Main.qml\"", home_of("carol"));
        let mut source = filler(3000);
        source.extend(secret.as_bytes());
        source.extend(filler(3000));
        let frame = zstd_compress(&source);
        assert!(
            find_all(&frame, secret.as_bytes()).is_empty(),
            "the test needs a frame that hides the text"
        );
        let mut binary = b"\0junk bytes\0".to_vec();
        binary.extend(frame);
        binary.extend(b"\0between\0");
        binary.extend(zstd_compress(b"Item { }  // a clean file"));
        binary.extend(b"\0the end\0");
        let needles = private_needles(&who("carol"));
        // The compressed bytes hide it from the plain search ...
        assert_eq!(scan_bytes(&binary, &needles, 4).total(), 0);
        // ... and the frames give it back.
        let (frames, text) = zstd_frames(&binary).expect("run").expect("zstd is there");
        assert_eq!(frames, 2);
        let scan = scan_bytes(&text, &needles, 4);
        assert!(scan.total() >= 2, "{scan:?}");
        assert!(scan.strings[0].1.contains("Main.qml"), "{scan:?}");
        // Bytes that only look like a frame give nothing and no error.
        let mut fake = b"x".to_vec();
        fake.extend(ZSTD_MAGIC);
        fake.extend(b"not a frame at all");
        let (frames, text) = zstd_frames(&fake).expect("run").expect("zstd is there");
        assert_eq!((frames, text.len()), (0, 0));
        assert_eq!(
            zstd_frames(b"no frames").expect("run").map(|(n, _)| n),
            Some(0)
        );
    }

    // ---- the licences of what is linked in -----------------------------------------------

    fn fake_bundled_tree(scratch: &Scratch) -> (serde_json::Value, PathBuf) {
        scratch.write("sqlite/Cargo.toml", b"");
        scratch.write("sqlite/sqlcipher/LICENSE", b"Copyright (c) 2008-2020 Zetetic LLC <all>\nRedistributions in binary form must reproduce the notice\n");
        scratch.write("boring/Cargo.toml", b"");
        scratch.write("boring/deps/boringssl/LICENSE", b"Apache License & others");
        let sysroot = scratch.0.join("sysroot");
        scratch.write(
            "sysroot/share/doc/rust/COPYRIGHT-library.html",
            b"<h1>Copyright notices</h1>",
        );
        scratch.write(
            "sysroot/share/doc/rust/licenses/Apache-2.0.txt",
            b"Apache text",
        );
        scratch.write("sysroot/share/doc/rust/licenses/MIT.txt", b"MIT text");
        let meta = serde_json::json!({"packages": [
            {"name": "other", "manifest_path": scratch.0.join("other/Cargo.toml")},
            {"name": "libsqlite3-sys", "manifest_path": scratch.0.join("sqlite/Cargo.toml")},
            {"name": "boring-sys", "manifest_path": scratch.0.join("boring/Cargo.toml")},
        ]});
        (meta, sysroot)
    }

    #[test]
    fn the_notices_of_what_is_linked_in_but_is_no_crate_are_found_and_a_missing_file_is_named() {
        let scratch = Scratch::new("bundled");
        let (meta, sysroot) = fake_bundled_tree(&scratch);
        let libs = bundled_libraries(&meta, &sysroot, "rustc 1.99.0").expect("found");
        let names: Vec<&str> = libs.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "SQLCipher",
                "SQLite",
                "BoringSSL",
                "The Rust standard library"
            ]
        );
        let html = render_bundled_section(&libs);
        for want in [
            "Bundled libraries",
            "Zetetic LLC &lt;all&gt;",
            "Apache License &amp; others",
            "public domain",
            "Copyright notices",
            "Apache text",
            "MIT text",
            "rustc 1.99.0",
            "SQLCipher (Zetetic LLC): sqlcipher/LICENSE",
        ] {
            assert!(html.contains(want), "{want:?} not in\n{html}");
        }
        assert!(notice_problems(&html).is_empty());
        // Put into the page of `cargo about`, before the end of <main>.
        let page = insert_section("<html><main><h1>x</h1></main></html>", &html).expect("page");
        assert!(page.find("Bundled libraries").expect("in") < page.find("</main>").expect("end"));
        assert!(insert_section("<html></html>", &html).is_err());
        assert_eq!(
            insert_section("<body>x</body>", "S").expect("body"),
            "<body>xS</body>"
        );
        // The older toolchain layout is found as well.
        std::fs::remove_dir_all(sysroot.join("share/doc/rust")).expect("remove");
        scratch.write("sysroot/share/doc/rust/COPYRIGHT", b"old notices");
        scratch.write("sysroot/share/doc/rust/LICENSE-APACHE", b"old apache");
        scratch.write("sysroot/share/doc/rust/LICENSE-MIT", b"old mit");
        let old = bundled_libraries(&meta, &sysroot, "rustc").expect("old layout");
        assert_eq!(old[3].texts[0].source, "COPYRIGHT");
        assert_eq!(old[3].texts[1].source, "LICENSE-APACHE");
        // A missing file is an error that names it.
        for (gone, named) in [
            ("sqlite/sqlcipher/LICENSE", "sqlcipher/LICENSE"),
            ("boring/deps/boringssl/LICENSE", "boringssl/LICENSE"),
            ("sysroot/share/doc/rust/COPYRIGHT", "COPYRIGHT"),
        ] {
            let moved = scratch.0.join(format!("{gone}.moved"));
            std::fs::rename(scratch.0.join(gone), &moved).expect("move");
            let err = bundled_libraries(&meta, &sysroot, "rustc").expect_err(gone);
            assert!(format!("{err:#}").contains(named), "{gone}: {err:#}");
            std::fs::rename(&moved, scratch.0.join(gone)).expect("restore");
        }
        let none = serde_json::json!({"packages": []});
        let err = bundled_libraries(&none, &sysroot, "rustc").expect_err("no package");
        assert!(format!("{err:#}").contains("libsqlite3-sys"), "{err:#}");
    }

    /// The real checkout and toolchain: the files exist where `cargo metadata` and the sysroot
    /// say, and the page names the authors the licences require.
    #[test]
    fn the_real_checkout_has_the_licence_files_of_what_is_linked_in() {
        let libs = bundled_libraries_here().expect("the licence files of this machine");
        let html = render_bundled_section(&libs);
        assert!(
            notice_problems(&html).is_empty(),
            "{:?}",
            notice_problems(&html)
        );
        assert!(html.contains("Redistributions in binary form must reproduce"));
        assert!(html.contains("Zetetic LLC"));
        let std_texts = &libs[3].texts;
        assert_eq!(std_texts.len(), 3);
        assert!(std_texts.iter().all(|t| !t.text.trim().is_empty()));
    }

    // ---- the source archive --------------------------------------------------------------

    fn git_in(repo: &Path, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.org"])
            .args(args);
        stdout_of(&mut cmd, "git").expect("git")
    }

    #[test]
    fn the_source_archive_is_the_commit_the_run_started_with_with_plain_modes() {
        let scratch = Scratch::new("source");
        let repo = scratch.0.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        git_in(&repo, &["init", "--quiet"]);
        let first = scratch.write("repo/a.txt", b"a");
        scratch.write("repo/dir/b.txt", b"b");
        let run = scratch.write("repo/run.sh", b"#!/bin/sh\n");
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o664)).expect("mode");
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o775)).expect("mode");
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "--quiet", "-m", "one"]);
        let first_hash = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        // The branch moves on after the run started.
        scratch.write("repo/later.txt", b"later");
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "--quiet", "-m", "two"]);
        let second_hash = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
        assert_ne!(first_hash, second_hash);

        let names = archive_names("0.1.0", "x86_64", Grade::Unreleased);
        let dest = scratch.0.join(&names.source_archive);
        pack_source(&repo, &names.source_dir, &first_hash, &dest).expect("pack");
        verify_source(&dest, &names, &first_hash, &[]).expect("the commit it was asked for");
        // A file that must stay out of the archive and is in it.
        let err = verify_source(&dest, &names, &first_hash, &["dir/b.txt".to_owned()])
            .expect_err("a hidden file in the archive");
        assert!(format!("{err:#}").contains("export-ignore"), "{err:#}");
        // (A name that only ends like a member is not that member.)
        verify_source(&dest, &names, &first_hash, &["b.txt".to_owned()]).expect("not a member");
        let listing = stdout_of(
            Command::new("tar")
                .args(["--list", "--verbose", "--gzip", "--file"])
                .arg(&dest),
            "tar",
        )
        .expect("listing");
        assert!(
            listing.contains("kourier-0.1.0-unreleased/run.sh"),
            "{listing}"
        );
        assert!(
            !listing.contains("later.txt"),
            "HEAD must not be packed:\n{listing}"
        );
        assert!(
            listing.contains("-rwxr-xr-x") && listing.contains("-rw-r--r--"),
            "{listing}"
        );
        assert!(
            !listing.contains("rwxrwxr") && !listing.contains("rw-rw-r"),
            "{listing}"
        );
        // It is the first commit and not the second.
        let err = verify_source(&dest, &names, &second_hash, &[]).expect_err("another commit");
        assert!(
            format!("{err:#}").contains("not made from the commit"),
            "{err:#}"
        );
        // Packed from HEAD with git's own umask, the modes would not be plain: the check sees it.
        let raw = scratch.0.join("raw.tar");
        let tar = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "archive",
                "--format=tar",
                "--prefix=kourier-0.1.0-unreleased/",
            ])
            .arg(&first_hash)
            .output()
            .expect("archive");
        std::fs::write(&raw, &tar.stdout).expect("tar");
        let plain = stdout_of(
            Command::new("tar")
                .args(["--list", "--verbose", "--file"])
                .arg(&raw),
            "tar",
        )
        .expect("listing");
        assert!(
            !source_listing_problems(&plain, &names.source_dir).is_empty(),
            "git's default modes are 0664/0775: {plain}"
        );
        // A hash is required: a branch name could move, an option could be an option.
        for bad in ["HEAD", "main", "--output=/x"] {
            assert!(pack_source(&repo, "p", bad, &dest).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_source_listing_must_be_below_one_directory_with_plain_modes() {
        let ok = "drwxr-xr-x 0/0 0 2026-10-08 20:15 top/\n-rw-r--r-- 0/0 12 2026-10-08 20:15 top/a b.txt\n-rwxr-xr-x 0/0 12 2026-10-08 20:15 top/run.sh\nlrwxrwxrwx 0/0 0 2026-10-08 20:15 top/link -> a\n";
        assert!(source_listing_problems(ok, "top").is_empty());
        let bad = "drwxrwxr-x 0/0 0 2026-10-08 20:15 top/\n-rw-r--r-- 0/0 12 2026-10-08 20:15 elsewhere/a\n-rw-rw-r-- 0/0 12 2026-10-08 20:15 top/b\n";
        let problems = source_listing_problems(bad, "top");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(source_listing_problems("", "top")[0].contains("empty"));
        assert!(
            !source_listing_problems("-rw-r--r-- 0/0 1 2026-10-08 20:15 topless/a\n", "top")
                .is_empty()
        );
    }

    /// The text only a measuring build holds (`install.rs` keeps the constant private; the
    /// check in `verify` is `install::is_perf_build`).
    fn install_marker() -> &'static [u8] {
        b"kourier: measuring build (cargo feature dumpable)"
    }
}
