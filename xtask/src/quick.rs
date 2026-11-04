//! `cargo xtask ci --quick [--since <rev>]`: the CI tier for use while working.
//!
//! It runs rustfmt, clippy and deps-check as the full run does, the unit tests of every
//! package (seconds), and the integration tests of the packages the change reaches: the
//! packages that own a changed file and every workspace package that depends on one of them,
//! through normal, dev or build dependencies. A change that can alter any build (the root
//! manifest, the lock file, the toolchain, `.cargo/`) makes it the full `ci`. Screenshots,
//! doc tests and the integration tests of unreached packages are left to the full run.

use crate::workspace::Workspace;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::{Duration, Instant};

/// The arguments of `cargo xtask ci`.
#[derive(Debug, Default, PartialEq)]
pub struct CiArgs {
    pub quick: bool,
    pub since: Option<String>,
    /// `--keep-target`: leave `target/debug` as it is at the end (no [`crate::prune`]).
    pub keep_target: bool,
    /// `--no-wait`: when another test run holds the lock ([`crate::lock`]), end with status 75
    /// instead of waiting for it.
    pub no_wait: bool,
    /// `--force`: run although a green run on the very same tree is on record ([`crate::green`]).
    pub force: bool,
}

pub fn parse_args(args: &[String]) -> Result<CiArgs> {
    let mut parsed = CiArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quick" => parsed.quick = true,
            "--keep-target" => parsed.keep_target = true,
            "--no-wait" => parsed.no_wait = true,
            "--force" => parsed.force = true,
            "--since" => {
                let rev = it.next().context("--since needs a revision")?;
                parsed.since = Some(rev.clone());
            }
            _ if arg.starts_with("--since=") => {
                parsed.since = Some(arg["--since=".len()..].to_owned());
            }
            _ => bail!(
                "ci: unknown argument {arg:?} (usage: ci [--quick [--since <rev>]] [--keep-target] [--no-wait] [--force])"
            ),
        }
    }
    if let Some(rev) = &parsed.since {
        if !parsed.quick {
            bail!("ci: --since goes with --quick");
        }
        if rev.is_empty() || rev.starts_with('-') {
            bail!("ci: --since needs a revision, not {rev:?}");
        }
    }
    Ok(parsed)
}

/// Files whose change can alter any build: the run is the full one.
const FULL_FILES: [&str; 3] = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];
const FULL_PREFIXES: [&str; 1] = [".cargo/"];

/// Files outside every package that a test reads: (path, or directory with a trailing `/`,
/// the packages whose tests read it).
const READERS: [(&str, &[&str]); 6] = [
    // The smoke tool's tests check the runbook against the tool.
    ("docs/smoke-run.md", &["xtask"]),
    // The same for the WhatsApp run tool and its runbook.
    ("docs/whatsapp-run.md", &["xtask"]),
    // The same for the Telegram run tool and its runbook.
    ("docs/telegram-run.md", &["xtask"]),
    // `kourier`'s single-instance test reads the desktop file; the installer's tests all three.
    ("packaging/", &["kourier", "xtask"]),
    // The Signal mock the two mock suites run against.
    ("tests/mock-harness/", &["kourier-signal", "kourier-e2e"]),
    // The vendored libthreema: a path dependency of the Threema backend only, outside every
    // package directory of this workspace (its own suite runs by hand, VENDORED.md).
    ("third_party/libthreema/", &["kourier-threema"]),
];

/// Files that cannot change a test result.
const NEUTRAL_FILES: [&str; 9] = [
    "LICENSE",
    "REUSE.toml",
    "deny.toml",
    "about.toml",
    "about.hbs",
    "cliff.toml",
    ".gitignore",
    ".gitattributes",
    "CODEOWNERS",
];
const NEUTRAL_DIRS: [&str; 3] = ["docs/", ".github/", "LICENSES/"];

/// Crates below `crates/` that cannot change a picture: the screenshots render on the fake
/// network and these are the Signal, WhatsApp, Telegram and Threema backends, the TDLib helper crate and the two test-only crates (the same
/// list as the digest of `cargo xtask shots`).
const NO_PICTURE_CRATES: [&str; 7] = [
    "kourier-signal",
    "kourier-whatsapp",
    "kourier-telegram",
    "kourier-tdlib",
    "kourier-threema",
    "kourier-e2e",
    "kourier-coexist",
];

/// What a changed path belongs to.
#[derive(Debug, PartialEq)]
pub enum Owner {
    /// Can alter any build.
    Full,
    /// Cannot change a test result.
    Neutral,
    /// These workspace packages.
    Packages(Vec<String>),
    /// Outside every package and not known to matter.
    Unmapped,
}

/// Markdown, the neutral files and directories (and nothing of what a test reads: see
/// [`is_neutral`]).
fn is_neutral_by_name(path: &str) -> bool {
    path.ends_with(".md")
        || NEUTRAL_FILES.contains(&path)
        || NEUTRAL_DIRS.iter().any(|d| path.starts_with(d))
}

/// Whether a path cannot change a test result: [`owner`] says `Neutral` for it, whatever the
/// workspace is (no file that can alter a build, none a test reads: `docs/smoke-run.md`,
/// `packaging/`, `tests/mock-harness/`). The digest of a green run keeps these files apart
/// (`green::NEUTRAL_PART`), so that a docs-only edit does not run the tests again (T5 review F9).
pub fn is_neutral(path: &str) -> bool {
    !(FULL_FILES.contains(&path)
        || FULL_PREFIXES.iter().any(|p| path.starts_with(p))
        || READERS.iter().any(|(prefix, _)| {
            path == *prefix || (prefix.ends_with('/') && path.starts_with(prefix))
        }))
        && is_neutral_by_name(path)
}

pub fn owner(ws: &Workspace, path: &str) -> Owner {
    if FULL_FILES.contains(&path) || FULL_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return Owner::Full;
    }
    for (prefix, packages) in READERS {
        if path == prefix || (prefix.ends_with('/') && path.starts_with(prefix)) {
            let packages: Vec<String> = packages
                .iter()
                .filter(|p| ws.is_member(p))
                .map(|p| (*p).to_owned())
                .collect();
            return Owner::Packages(packages);
        }
    }
    if is_neutral_by_name(path) {
        return Owner::Neutral;
    }
    match ws
        .packages
        .iter()
        .find(|p| path.starts_with(&format!("{}/", p.dir)))
    {
        Some(p) => Owner::Packages(vec![p.name.clone()]),
        None => Owner::Unmapped,
    }
}

/// Whether the path is one the digest of `cargo xtask shots` hashes, so a picture may differ.
fn can_change_a_picture(path: &str) -> bool {
    if path == "Cargo.lock"
        || path.starts_with("packaging/")
        || path == "xtask/src/main.rs"
        || path == "xtask/src/shots.rs"
    {
        return true;
    }
    let mut parts = path.split('/');
    parts.next() == Some("crates")
        && parts
            .next()
            .is_some_and(|krate| !NO_PICTURE_CRATES.contains(&krate))
        && !path
            .split('/')
            .any(|part| part == "tests" || part == "target")
}

/// The changed files sorted into what they mean for the run.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Changed packages and the files that changed them.
    pub changed: BTreeMap<String, Vec<String>>,
    /// Files that cannot change a test result.
    pub neutral: Vec<String>,
    /// Files outside every package that are not known to matter.
    pub unmapped: Vec<String>,
    /// Files that make this a full run.
    pub full: Vec<String>,
    /// Changed QML files.
    pub qml: Vec<String>,
    /// Changed files that can change a picture.
    pub pictures: Vec<String>,
}

pub fn plan(ws: &Workspace, files: &[String]) -> Plan {
    let mut plan = Plan::default();
    for file in files {
        let owner = owner(ws, file);
        if file.ends_with(".qml") {
            plan.qml.push(file.clone());
        }
        if owner != Owner::Neutral && can_change_a_picture(file) {
            plan.pictures.push(file.clone());
        }
        match owner {
            Owner::Full => plan.full.push(file.clone()),
            Owner::Neutral => plan.neutral.push(file.clone()),
            Owner::Unmapped => plan.unmapped.push(file.clone()),
            Owner::Packages(packages) => {
                for package in packages {
                    plan.changed.entry(package).or_default().push(file.clone());
                }
            }
        }
    }
    plan
}

/// What a change of the root `Cargo.toml` and `Cargo.lock` means (T5e, [`crate::manifest`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootChange {
    /// Neither file is among the changed ones.
    Untouched,
    /// Something every package can feel (the reason is for the report): the full run.
    Shared(String),
    /// Only a new member and its own lines: a change of these packages (none when the files
    /// differ by comments or white space only).
    NewMembers {
        members: Vec<String>,
        /// What the change adds, for the report ([`crate::manifest::Added::describe`]).
        note: String,
    },
}

/// Like [`plan`], with the verdict on the root manifest and lock file: when they changed only by
/// a new member's lines, they are changes of that member and not of every build.
pub fn plan_with_root(ws: &Workspace, files: &[String], root: &RootChange) -> Plan {
    let RootChange::NewMembers { members, .. } = root else {
        return plan(ws, files);
    };
    let (root_files, others): (Vec<String>, Vec<String>) = files
        .iter()
        .cloned()
        .partition(|f| f == "Cargo.toml" || f == "Cargo.lock");
    let mut plan = plan(ws, &others);
    for file in root_files {
        if can_change_a_picture(&file) {
            plan.pictures.push(file.clone());
        }
        if members.is_empty() {
            plan.neutral.push(file);
        } else {
            for member in members {
                plan.changed
                    .entry(member.clone())
                    .or_default()
                    .push(file.clone());
            }
        }
    }
    plan
}

/// Reads the verdict from git and the working tree. `base` is the revision the changed files
/// are counted from (`--since`, else `HEAD`). A file git cannot show at the base is a shared
/// change (the safe side).
fn root_change(ws: &Workspace, files: &[String], base: &str) -> RootChange {
    if !files.iter().any(|f| f == "Cargo.toml" || f == "Cargo.lock") {
        return RootChange::Untouched;
    }
    let read = || -> Result<RootChange> {
        let show = |path: &str| -> Result<String> {
            Ok(String::from_utf8_lossy(&git(&["show", &format!("{base}:{path}")])?).into_owned())
        };
        let now = |path: &str| -> Result<String> {
            std::fs::read_to_string(crate::root().join(path))
                .with_context(|| format!("read {path}"))
        };
        // The base's files: a member is new when the base has no manifest of its directory.
        let known = String::from_utf8_lossy(&git(&["ls-tree", "-r", "--name-only", base])?)
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let new_members: Vec<&crate::workspace::Package> = ws
            .packages
            .iter()
            .filter(|p| !known.contains(&format!("{}/Cargo.toml", p.dir)))
            .collect();
        // Their own manifests: a feature they turn on for a crate the base builds reaches every
        // build (T5 review F4).
        let manifests: Vec<(String, String)> = new_members
            .iter()
            .filter_map(|p| Some((p.name.clone(), now(&format!("{}/Cargo.toml", p.dir)).ok()?)))
            .collect();
        let verdict = crate::manifest::classify_with_members(
            &show("Cargo.toml")?,
            &now("Cargo.toml")?,
            &show("Cargo.lock")?,
            &now("Cargo.lock")?,
            &manifests,
            &|file| known.contains(file),
        );
        Ok(match verdict {
            crate::manifest::Root::Shared(reason) => RootChange::Shared(reason),
            crate::manifest::Root::Same => RootChange::NewMembers {
                members: Vec::new(),
                note: String::new(),
            },
            crate::manifest::Root::Additive(added) => RootChange::NewMembers {
                members: new_members.iter().map(|p| p.name.clone()).collect(),
                note: added.describe(),
            },
        })
    };
    read().unwrap_or_else(|e| RootChange::Shared(format!("cannot compare with {base}: {e:#}")))
}

/// The packages a change reaches: the changed ones (`None`) and every member that depends on a
/// reached package, with the dependency it was reached through.
pub fn affected<'a>(
    ws: &'a Workspace,
    changed: impl IntoIterator<Item = &'a String>,
) -> BTreeMap<String, Option<String>> {
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for pkg in &ws.packages {
        for (to, _) in ws.deps_of(&pkg.name) {
            if ws.is_member(to) {
                dependents.entry(to).or_default().insert(&pkg.name);
            }
        }
    }
    let mut reached: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut queue = std::collections::VecDeque::new();
    for name in changed {
        reached.insert(name.clone(), None);
        queue.push_back(name.as_str());
    }
    while let Some(current) = queue.pop_front() {
        for &dependent in dependents.get(current).into_iter().flatten() {
            if !reached.contains_key(dependent) {
                reached.insert(dependent.to_owned(), Some(current.to_owned()));
                queue.push_back(dependent);
            }
        }
    }
    reached
}

/// The integration test targets to run: the reached packages that have some, sorted by name.
pub fn integration_tests(
    ws: &Workspace,
    reached: &BTreeMap<String, Option<String>>,
) -> Vec<(String, Vec<String>)> {
    let mut selected: Vec<(String, Vec<String>)> = ws
        .packages
        .iter()
        .filter(|p| reached.contains_key(&p.name) && !p.tests.is_empty())
        .map(|p| (p.name.clone(), p.tests.clone()))
        .collect();
    selected.sort();
    selected
}

/// The packages whose own changes bring the Qt window tests (the integration tests of
/// `kourier-qt`: 26 binaries, each an app or a window on the offscreen platform) into the quick
/// tier (T5c, 2026-10-16; his choice "the three small ones"): the Qt crate and its QML (the QML
/// lives in it), the viewmodel that feeds it and the composition root it runs on. A change
/// below them (the core, the model, a backend, the fake) still reaches `kourier-qt` through the
/// dependency walk, but its window tests are left to the full run: they cost 77-87 s to run
/// and 34-42 s to link, and a break there shows in the viewmodel's and the core's own tests
/// first (the one that did not, `actions_ui` "[refusedfiles]", the full run caught). The
/// crate's lib unit tests always run (`runner::quick_unit`).
pub const UI_SIDE: [&str; 3] = ["kourier-qt", "kourier-viewmodel", "kourier-app"];

/// Whether a change of one of the `changed` packages brings the Qt window tests along.
pub fn ui_side_changed<'a>(changed: impl IntoIterator<Item = &'a String>) -> bool {
    changed.into_iter().any(|p| UI_SIDE.contains(&p.as_str()))
}

/// The package of the window tests: `kourier-qt` (`needs_offscreen`).
fn is_window_tests(package: &str) -> bool {
    needs_offscreen(package)
}

/// `selected` without the Qt window tests unless `ui_changed`, and how many test binaries were
/// left out (none when they stay or were not reached).
pub fn without_window_tests(
    selected: Vec<(String, Vec<String>)>,
    ui_changed: bool,
) -> (Vec<(String, Vec<String>)>, Option<usize>) {
    if ui_changed {
        return (selected, None);
    }
    let (window, rest): (Vec<_>, Vec<_>) = selected
        .into_iter()
        .partition(|(package, _)| is_window_tests(package));
    let left_out = window.first().map(|(_, tests)| tests.len());
    (rest, left_out)
}

/// What the run says about the window tests it left out (T5c).
pub fn window_tests_note(binaries: usize) -> String {
    format!(
        "  left out: the Qt window tests of kourier-qt ({binaries} test binaries): nothing of \
         kourier-qt, its QML, kourier-viewmodel or kourier-app changed; a change below them \
         reaches them in the full run (`cargo xtask ci`)"
    )
}

/// What the integration step leaves out of the slow set (T5d, T5 review F3): the slow tests of
/// the packages it runs (the Qt crate's window tests too, SE6, when they ran), in words; `None`
/// when none of them has any.
fn slow_left_out(selected: &[(String, Vec<String>)]) -> Option<String> {
    let packages = selected.iter().map(|(package, _)| package.as_str());
    crate::slow::left_out_note(&crate::slow::left_out(packages))
}

/// `cargo test` arguments for the integration tests of one package: its test targets by name,
/// so its unit tests (which the first test step ran) do not run twice.
pub fn test_args(package: &str, tests: &[String]) -> Vec<String> {
    let mut args = vec!["test".to_owned(), "-p".to_owned(), package.to_owned()];
    for test in tests {
        args.push("--test".to_owned());
        args.push(test.clone());
    }
    args
}

/// The Qt crate's tests need a platform plugin; the full run gives them the offscreen one.
pub fn needs_offscreen(package: &str) -> bool {
    package == "kourier-qt"
}

/// Paths from `git status --porcelain=v1 -z`: the path of every entry, and the old path of a
/// rename or copy.
pub fn parse_porcelain(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let mut entries = text.split('\0').filter(|e| !e.is_empty());
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        let (Some(status), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        paths.push(path.to_owned());
        if status.contains(['R', 'C']) {
            paths.extend(entries.next().map(str::to_owned));
        }
    }
    paths
}

/// Paths from `git diff --name-only -z`.
pub fn parse_names(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

fn git(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(crate::root())
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// The files that differ from HEAD (staged, unstaged, untracked) and, with `since`, those
/// changed by the commits after it, as paths from the workspace root.
fn changed_files(since: Option<&str>) -> Result<Vec<String>> {
    let mut files = parse_porcelain(&git(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
    ])?);
    if let Some(rev) = since {
        let range = format!("{rev}..HEAD");
        files.extend(parse_names(&git(&[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &range,
            "--",
        ])?));
    }
    files.sort();
    files.dedup();
    Ok(files)
}

pub fn run(since: Option<&str>, start: &crate::Start) -> Result<crate::Outcome> {
    let started = Instant::now();
    let ws = Workspace::load(&crate::root())?;
    let files = changed_files(since)?;
    let root = root_change(&ws, &files, since.unwrap_or("HEAD"));
    let plan = plan_with_root(&ws, &files, &root);
    if !plan.full.is_empty() {
        eprintln!(
            "\nquick: {} can change every build: this is a full run (cargo xtask ci)",
            list(&plan.full, 4)
        );
        if let RootChange::Shared(reason) = &root {
            eprintln!("quick: {reason}");
        }
        return crate::ci_full(start);
    }
    let reached = affected(&ws, plan.changed.keys());
    describe(&files, since, &plan, &reached, &root);

    let (selected, window_left_out) = without_window_tests(
        integration_tests(&ws, &reached),
        ui_side_changed(plan.changed.keys()),
    );
    // What this run covers. A record of this very tree that covers it ends the run (T5b).
    let ran = crate::green::Ran {
        everything: false,
        integration: selected
            .iter()
            .filter(|(package, _)| !needs_offscreen(package))
            .map(|(package, _)| package.clone())
            .collect(),
        qt_window_tests: selected.iter().any(|(package, _)| needs_offscreen(package)),
        qmllint: !plan.qml.is_empty(),
        slow: false,
    };
    if start.covered(&ran) {
        return Ok(crate::Outcome::Skipped);
    }

    crate::lint_steps()?;
    let engine = crate::runner::engine();
    crate::runner::quick_unit(engine)?;
    if let Some(binaries) = window_left_out {
        eprintln!("\nquick:\n{}", window_tests_note(binaries));
    }
    if let Some(note) = slow_left_out(&selected) {
        eprintln!("\nquick:\n  left out: {note}");
    }
    crate::runner::quick_integration(engine, &selected)?;
    if !plan.qml.is_empty() {
        crate::qmllint()?;
    }

    let (summary, pictures) = closing(
        &ws,
        &plan,
        &reached,
        &selected,
        window_left_out,
        started.elapsed(),
    );
    eprintln!("\n{summary}");
    if let Some(line) = pictures {
        eprintln!("{line}");
    }
    Ok(crate::Outcome::Ran {
        tier: crate::green::Tier::Quick,
        ran,
        engine,
    })
}

/// The closing report of a passed run: what ran, what did not and why, the reminder of the full
/// run; and the line about pictures, when a changed file can have changed one.
fn closing(
    ws: &Workspace,
    plan: &Plan,
    reached: &BTreeMap<String, Option<String>>,
    selected: &[(String, Vec<String>)],
    window_left_out: Option<usize>,
    elapsed: Duration,
) -> (String, Option<String>) {
    let mut ran = vec![
        "rustfmt".to_owned(),
        "clippy".to_owned(),
        "deps-check".to_owned(),
        "unit tests of every package".to_owned(),
    ];
    let names: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
    if !names.is_empty() {
        ran.push(format!("integration tests of {}", names.join(", ")));
    }
    let mut skipped: Vec<String> = Vec::new();
    let mut others: Vec<&str> = ws
        .packages
        .iter()
        .filter(|p| !p.tests.is_empty() && !reached.contains_key(&p.name))
        .map(|p| p.name.as_str())
        .collect();
    others.sort();
    if !others.is_empty() {
        skipped.push(format!(
            "integration tests of {} (not reached by the change)",
            others.join(", ")
        ));
    }
    if let Some(binaries) = window_left_out {
        skipped.push(format!(
            "the Qt window tests of kourier-qt ({binaries} test binaries; nothing of kourier-qt, \
             its QML, kourier-viewmodel or kourier-app changed)"
        ));
    }
    if let Some(note) = slow_left_out(selected) {
        skipped.push(note);
    }
    skipped.push("doc tests".to_owned());
    if plan.qml.is_empty() {
        skipped.push("qmllint (no .qml file changed)".to_owned());
    } else {
        ran.push("qmllint".to_owned());
    }
    skipped.push("screenshots".to_owned());
    let summary = format!(
        "quick: all steps passed in {}\n  ran: {}\n  not run: {}\n  \
         The full run is `cargo xtask ci`: before a commit that touches the core, the storage, \
         a backend or more than one area, and before any run against real servers.",
        duration(elapsed),
        ran.join(", "),
        skipped.join("; "),
    );
    let pictures = (!plan.pictures.is_empty()).then(|| {
        format!(
            "quick: pictures may have changed ({}) and were not rendered: `cargo xtask shots` \
             compares them",
            list(&plan.pictures, 3)
        )
    });
    (summary, pictures)
}

/// What the run takes as changed and affected, and why.
fn describe(
    files: &[String],
    since: Option<&str>,
    plan: &Plan,
    reached: &BTreeMap<String, Option<String>>,
    root: &RootChange,
) {
    let source = match since {
        Some(rev) => format!("git status and {rev}..HEAD"),
        None => "git status".to_owned(),
    };
    eprintln!("\nquick: {} changed paths ({source})", files.len());
    if plan.changed.is_empty() {
        eprintln!("  changed packages: none");
    }
    for (package, files) in &plan.changed {
        eprintln!("  changed: {package} ({})", list(files, 3));
    }
    for (package, via) in reached {
        if let Some(via) = via {
            eprintln!("  affected: {package} (depends on {via})");
        }
    }
    match root {
        RootChange::NewMembers { members, note } if !members.is_empty() => eprintln!(
            "  root Cargo.toml and Cargo.lock: only the lines of a new member ({}), read as a \
             change of that member ({note})",
            members.join(", ")
        ),
        RootChange::NewMembers { note, .. } => eprintln!(
            "  root Cargo.toml and Cargo.lock: nothing that reaches a package ({})",
            if note.is_empty() {
                "comments or white space only"
            } else {
                note
            }
        ),
        RootChange::Shared(_) | RootChange::Untouched => {}
    }
    if !plan.neutral.is_empty() {
        eprintln!(
            "  no effect on any test: {} ({} files)",
            list(&plan.neutral, 3),
            plan.neutral.len()
        );
    }
    if !plan.unmapped.is_empty() {
        eprintln!(
            "  in no package, assumed to change no test: {}",
            list(&plan.unmapped, 3)
        );
    }
}

/// The first `n` entries joined, and how many more there are.
fn list(items: &[String], n: usize) -> String {
    let mut text = items.iter().take(n).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > n {
        text.push_str(&format!(" and {} more", items.len() - n));
    }
    text
}

fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{} min {} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{Kind, tests::made_up};

    /// The crate layout of the repository as a made-up graph (the edges of architecture §2.2).
    fn repo() -> Workspace {
        let dirs = [
            "kourier-model",
            "kourier-backend-api",
            "kourier-db",
            "kourier-blob",
            "kourier-media",
            "kourier-core",
            "kourier-fake",
            "kourier-signal",
            "kourier-whatsapp",
            "kourier-threema",
            "kourier-desktop",
            "kourier-app",
            "kourier-viewmodel",
            "kourier-qt",
            "kourier",
            "kourier-coexist",
            "kourier-e2e",
        ];
        let dirs: Vec<(String, String)> = dirs
            .iter()
            .map(|n| ((*n).to_owned(), format!("crates/{n}")))
            .collect();
        // Every crate has an integration test target but xtask.
        const IT: &[&str] = &["it"];
        const NONE: &[&str] = &[];
        let mut members: Vec<(&str, &str, &[&str])> = dirs
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_str(), IT))
            .collect();
        members.push(("xtask", "xtask", NONE));
        fn normal(
            from: &'static str,
            to: &[&'static str],
        ) -> Vec<(&'static str, &'static str, Kind)> {
            to.iter().map(|t| (from, *t, Kind::Normal)).collect()
        }
        let mut edges = Vec::new();
        edges.extend(normal("kourier-backend-api", &["kourier-model"]));
        edges.extend(normal(
            "kourier-core",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ],
        ));
        edges.extend(normal(
            "kourier-fake",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ],
        ));
        edges.extend(normal(
            "kourier-signal",
            &[
                "kourier-model",
                "kourier-backend-api",
                "kourier-db",
                "kourier-blob",
                "kourier-media",
            ],
        ));
        edges.extend(normal(
            "kourier-whatsapp",
            &["kourier-model", "kourier-backend-api", "kourier-db"],
        ));
        edges.extend(normal(
            "kourier-threema",
            &["kourier-model", "kourier-backend-api", "kourier-db"],
        ));
        edges.extend(normal(
            "kourier-app",
            &[
                "kourier-core",
                "kourier-backend-api",
                "kourier-model",
                "kourier-desktop",
                "kourier-blob",
                "kourier-signal",
                "kourier-whatsapp",
                "kourier-threema",
                "kourier-fake",
                "kourier-db",
            ],
        ));
        edges.extend(normal(
            "kourier-viewmodel",
            &["kourier-core", "kourier-model"],
        ));
        edges.extend(normal(
            "kourier-qt",
            &["kourier-app", "kourier-viewmodel", "kourier-model"],
        ));
        edges.extend(normal("kourier", &["kourier-qt", "kourier-app"]));
        edges.extend(normal("kourier-coexist", &["kourier-db"]));
        edges.extend(normal(
            "kourier-e2e",
            &[
                "kourier-core",
                "kourier-signal",
                "kourier-fake",
                "kourier-db",
                "kourier-backend-api",
                "kourier-model",
                "kourier-media",
            ],
        ));
        // A dev dependency reaches its user's tests as well.
        edges.push(("kourier-qt", "kourier-fake", Kind::Dev));
        // An external crate is not a workspace package and takes no part.
        edges.push(("kourier-core", "tokio", Kind::Normal));
        made_up(&members, &edges)
    }

    fn reach(ws: &Workspace, changed: &[&str]) -> Vec<String> {
        let changed: Vec<String> = changed.iter().map(|c| (*c).to_owned()).collect();
        affected(ws, changed.iter()).into_keys().collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_leaf_reaches_itself_and_the_binary() {
        assert_eq!(
            reach(&repo(), &["kourier-qt"]),
            strings(&["kourier", "kourier-qt"])
        );
    }

    #[test]
    fn the_viewmodel_reaches_the_front_end_and_the_binary() {
        assert_eq!(
            reach(&repo(), &["kourier-viewmodel"]),
            strings(&["kourier", "kourier-qt", "kourier-viewmodel"])
        );
    }

    #[test]
    fn the_model_reaches_nearly_everything() {
        let reached = reach(&repo(), &["kourier-model"]);
        assert_eq!(
            reached,
            strings(&[
                "kourier",
                "kourier-app",
                "kourier-backend-api",
                "kourier-core",
                "kourier-e2e",
                "kourier-fake",
                "kourier-model",
                "kourier-qt",
                "kourier-signal",
                "kourier-threema",
                "kourier-viewmodel",
                "kourier-whatsapp",
            ])
        );
        // What does not depend on the model: storage, media, desktop, the coexist test, xtask.
        for name in [
            "kourier-db",
            "kourier-blob",
            "kourier-media",
            "kourier-desktop",
            "kourier-coexist",
            "xtask",
        ] {
            assert!(!reached.contains(&name.to_owned()), "{name}");
        }
    }

    #[test]
    fn a_dev_dependency_carries_the_change() {
        let reached = reach(&repo(), &["kourier-fake"]);
        assert!(reached.contains(&"kourier-qt".to_owned()));
        assert!(reached.contains(&"kourier-e2e".to_owned()));
    }

    #[test]
    fn xtask_reaches_only_xtask() {
        let ws = repo();
        let plan = plan(&ws, &strings(&["xtask/src/quick.rs", "xtask/Cargo.toml"]));
        assert_eq!(plan.changed.keys().collect::<Vec<_>>(), ["xtask"]);
        assert_eq!(reach(&ws, &["xtask"]), strings(&["xtask"]));
    }

    #[test]
    fn the_reason_is_the_dependency_it_came_through() {
        let ws = repo();
        let changed = strings(&["kourier-viewmodel"]);
        let reached = affected(&ws, changed.iter());
        assert_eq!(reached["kourier-viewmodel"], None);
        assert_eq!(reached["kourier-qt"], Some("kourier-viewmodel".to_owned()));
        assert_eq!(reached["kourier"], Some("kourier-qt".to_owned()));
    }

    #[test]
    fn markdown_and_docs_reach_nothing() {
        let ws = repo();
        let plan = plan(
            &ws,
            &strings(&[
                "README.md",
                "docs/architecture.md",
                "crates/kourier-core/README.md",
                ".github/workflows/ci.yml",
                "LICENSES/AGPL-3.0-only.txt",
                "deny.toml",
                "CHANGELOG.md",
            ]),
        );
        assert!(plan.changed.is_empty(), "{plan:?}");
        assert!(plan.full.is_empty());
        assert!(plan.unmapped.is_empty());
        assert_eq!(plan.neutral.len(), 7);
        assert!(plan.pictures.is_empty());
        assert!(affected(&ws, plan.changed.keys()).is_empty());
    }

    #[test]
    fn the_lock_file_and_the_toolchain_make_a_full_run() {
        let ws = repo();
        for file in [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ] {
            let plan = plan(&ws, &strings(&[file]));
            assert_eq!(plan.full, strings(&[file]), "{file}");
            assert!(plan.changed.is_empty());
        }
        // A package's own manifest is that package's change.
        let plan = plan(&ws, &strings(&["crates/kourier-qt/Cargo.toml"]));
        assert!(plan.full.is_empty());
        assert!(plan.changed.contains_key("kourier-qt"));
    }

    #[test]
    fn a_path_belongs_to_the_package_whose_directory_it_is_in() {
        let ws = repo();
        let owns = |path: &str| owner(&ws, path);
        assert_eq!(
            owns("crates/kourier/src/main.rs"),
            Owner::Packages(strings(&["kourier"]))
        );
        assert_eq!(
            owns("crates/kourier-app/src/lib.rs"),
            Owner::Packages(strings(&["kourier-app"]))
        );
        assert_eq!(
            owns("crates/kourier-qt/qml/Main.qml"),
            Owner::Packages(strings(&["kourier-qt"]))
        );
        assert_eq!(owns("crates/kourier-new/src/lib.rs"), Owner::Unmapped);
        assert_eq!(owns("scripts/run.sh"), Owner::Unmapped);
    }

    #[test]
    fn files_that_tests_read_belong_to_those_tests() {
        let ws = repo();
        let owns = |path: &str| owner(&ws, path);
        assert_eq!(
            owns("tests/mock-harness/client.mjs"),
            Owner::Packages(strings(&["kourier-signal", "kourier-e2e"]))
        );
        assert_eq!(
            owns("packaging/app.kourier.Kourier.desktop"),
            Owner::Packages(strings(&["kourier", "xtask"]))
        );
        assert_eq!(
            owns("docs/smoke-run.md"),
            Owner::Packages(strings(&["xtask"]))
        );
        assert_eq!(
            owns("docs/whatsapp-run.md"),
            Owner::Packages(strings(&["xtask"]))
        );
        assert_eq!(
            owns("docs/telegram-run.md"),
            Owner::Packages(strings(&["xtask"]))
        );
        // The vendored libthreema is read by the Threema backend's build only.
        assert_eq!(
            owns("third_party/libthreema/lib/src/lib.rs"),
            Owner::Packages(strings(&["kourier-threema"]))
        );
        assert!(!is_neutral("third_party/libthreema/VENDORED.md"));
        assert_eq!(owns("docs/architecture.md"), Owner::Neutral);
    }

    #[test]
    fn qml_and_pictures_are_noticed() {
        let ws = repo();
        let plan = plan(
            &ws,
            &strings(&[
                "crates/kourier-qt/qml/Main.qml",
                "crates/kourier-viewmodel/src/lib.rs",
                "crates/kourier-signal/src/lib.rs",
                "crates/kourier-whatsapp/src/lib.rs",
                "crates/kourier-threema/src/lib.rs",
                "crates/kourier-qt/tests/chat_ui.rs",
                "crates/kourier-e2e/src/lib.rs",
                "xtask/src/quick.rs",
                "xtask/src/shots.rs",
            ]),
        );
        assert_eq!(plan.qml, strings(&["crates/kourier-qt/qml/Main.qml"]));
        assert_eq!(
            plan.pictures,
            strings(&[
                "crates/kourier-qt/qml/Main.qml",
                "crates/kourier-viewmodel/src/lib.rs",
                "xtask/src/shots.rs",
            ])
        );
        assert!(can_change_a_picture("packaging/app.kourier.Kourier.svg"));
        assert!(!can_change_a_picture("crates/kourier-coexist/src/main.rs"));
        assert!(!can_change_a_picture("crates/kourier-whatsapp/src/lib.rs"));
        assert!(!can_change_a_picture(
            "crates/kourier-whatsapp/migrations/0001_init.sql"
        ));
        assert!(!can_change_a_picture("crates/kourier-threema/src/lib.rs"));
        assert!(!can_change_a_picture(
            "third_party/libthreema/lib/src/lib.rs"
        ));
    }

    #[test]
    fn integration_tests_are_those_of_the_reached_packages() {
        let ws = repo();
        let changed = strings(&["kourier-viewmodel", "xtask"]);
        let reached = affected(&ws, changed.iter());
        let selected = integration_tests(&ws, &reached);
        let names: Vec<&str> = selected.iter().map(|(p, _)| p.as_str()).collect();
        // xtask has no integration test target.
        assert_eq!(names, ["kourier", "kourier-qt", "kourier-viewmodel"]);
        assert_eq!(
            test_args("kourier-qt", &strings(&["chat_ui", "models"])),
            strings(&[
                "test",
                "-p",
                "kourier-qt",
                "--test",
                "chat_ui",
                "--test",
                "models"
            ])
        );
        assert!(needs_offscreen("kourier-qt"));
        assert!(!needs_offscreen("kourier"));
    }

    #[test]
    fn git_output_is_read_with_renames_and_untracked_files() {
        let raw = b" M crates/a/src/lib.rs\0?? new file.rs\0R  new/name.rs\0old/name.rs\0 D gone.rs\0A  added.rs\0";
        assert_eq!(
            parse_porcelain(raw),
            strings(&[
                "crates/a/src/lib.rs",
                "new file.rs",
                "new/name.rs",
                "old/name.rs",
                "gone.rs",
                "added.rs",
            ])
        );
        assert_eq!(parse_porcelain(b""), Vec::<String>::new());
        assert_eq!(parse_names(b"a/b.rs\0c.rs\0"), strings(&["a/b.rs", "c.rs"]));
    }

    #[test]
    fn the_arguments() {
        let args = |list: &[&str]| parse_args(&strings(list));
        assert_eq!(args(&[]).unwrap(), CiArgs::default());
        assert_eq!(
            args(&["--quick"]).unwrap(),
            CiArgs {
                quick: true,
                since: None,
                keep_target: false,
                no_wait: false,
                force: false
            }
        );
        // `--force` belongs to both tiers: a record of the very same tree does not end the run.
        assert!(args(&["--force"]).unwrap().force);
        assert!(args(&["--quick", "--force"]).unwrap().force);
        assert!(!args(&["--quick"]).unwrap().force);
        assert!(args(&["--force=1"]).is_err());
        // `--no-wait` belongs to both tiers: the test lock ends the run with 75 instead of waiting.
        assert!(args(&["--no-wait"]).unwrap().no_wait);
        assert!(args(&["--quick", "--no-wait"]).unwrap().no_wait);
        assert!(!args(&["--quick"]).unwrap().no_wait);
        assert!(args(&["--no-wait=1"]).is_err());
        // The flag belongs to both tiers and changes nothing else.
        assert!(args(&["--keep-target"]).unwrap().keep_target);
        let quick_kept = args(&["--quick", "--keep-target", "--since", "abc"]).unwrap();
        assert!(quick_kept.quick && quick_kept.keep_target);
        assert_eq!(quick_kept.since.as_deref(), Some("abc"));
        assert!(!args(&["--quick"]).unwrap().keep_target);
        assert!(args(&["--keep-target=1"]).is_err());
        assert_eq!(
            args(&["--quick", "--since", "abc123"]).unwrap().since,
            Some("abc123".to_owned())
        );
        assert_eq!(
            args(&["--since=HEAD~3", "--quick"]).unwrap().since,
            Some("HEAD~3".to_owned())
        );
        assert!(args(&["--since", "abc"]).is_err(), "--since needs --quick");
        assert!(args(&["--quick", "--since"]).is_err());
        assert!(args(&["--quick", "--since", "--output=x"]).is_err());
        assert!(args(&["--fast"]).is_err());
    }

    /// T5e: a root manifest and lock file that changed only by a new member's lines are changes of
    /// that member; anything shared stays the full run.
    #[test]
    fn a_root_change_that_is_only_a_new_member_is_a_change_of_that_member() {
        let ws = made_up(
            &[
                ("kourier-model", "crates/kourier-model", &[][..]),
                ("kourier-new", "crates/kourier-new", &[][..]),
            ],
            &[("kourier-new", "kourier-model", Kind::Normal)],
        );
        let files = strings(&[
            "Cargo.toml",
            "Cargo.lock",
            "crates/kourier-new/Cargo.toml",
            "crates/kourier-new/src/lib.rs",
        ]);
        let added = RootChange::NewMembers {
            members: vec!["kourier-new".to_owned()],
            note: String::new(),
        };
        let plan = plan_with_root(&ws, &files, &added);
        assert!(plan.full.is_empty(), "{plan:?}");
        assert_eq!(
            plan.changed["kourier-new"],
            strings(&[
                "crates/kourier-new/Cargo.toml",
                "crates/kourier-new/src/lib.rs",
                "Cargo.toml",
                "Cargo.lock"
            ])
        );
        assert!(plan.pictures.contains(&"Cargo.lock".to_owned()));
        // The same files with a shared change, or with no verdict: the full run, as before.
        for root in [
            RootChange::Shared("a feature of a shared crate".to_owned()),
            RootChange::Untouched,
        ] {
            let plan = plan_with_root(&ws, &files, &root);
            assert_eq!(
                plan.full,
                strings(&["Cargo.toml", "Cargo.lock"]),
                "{root:?}"
            );
        }
        // Comments only: the files change no package.
        let quiet = plan_with_root(
            &ws,
            &strings(&["Cargo.toml"]),
            &RootChange::NewMembers {
                members: Vec::new(),
                note: String::new(),
            },
        );
        assert!(quiet.full.is_empty() && quiet.changed.is_empty());
        assert_eq!(quiet.neutral, strings(&["Cargo.toml"]));
        // The toolchain and `.cargo/` are never read this way.
        let plan = plan_with_root(
            &ws,
            &strings(&["rust-toolchain.toml", ".cargo/config.toml"]),
            &added,
        );
        assert_eq!(plan.full.len(), 2);
    }

    #[test]
    fn the_closing_report_says_what_ran_and_what_did_not() {
        let ws = repo();
        let plan = plan(
            &ws,
            &strings(&[
                "crates/kourier-qt/qml/Main.qml",
                "crates/kourier-qt/src/lib.rs",
            ]),
        );
        let reached = affected(&ws, plan.changed.keys());
        let selected = integration_tests(&ws, &reached);
        let (summary, pictures) = closing(
            &ws,
            &plan,
            &reached,
            &selected,
            None,
            Duration::from_secs(151),
        );
        assert!(summary.starts_with("quick: all steps passed in 2 min 31 s"));
        assert!(summary.contains("ran: rustfmt, clippy, deps-check, unit tests of every package, integration tests of kourier, kourier-qt, qmllint"));
        assert!(summary.contains("not run: integration tests of kourier-app, kourier-backend-api"));
        assert!(!summary.contains("kourier-qt (not reached"));
        assert!(summary.contains("doc tests; screenshots"));
        assert!(!summary.contains("qmllint (no .qml"));
        assert!(summary.contains("`cargo xtask ci`"));
        let pictures = pictures.expect("a picture may differ");
        assert!(pictures.contains("`cargo xtask shots`"));
        assert!(pictures.contains("crates/kourier-qt/qml/Main.qml"));

        // Nothing that draws changed, no QML: no pictures line, qmllint reported as skipped.
        let plan = super::plan(&ws, &strings(&["crates/kourier-signal/src/lib.rs"]));
        let reached = affected(&ws, plan.changed.keys());
        let selected = integration_tests(&ws, &reached);
        let (summary, pictures) = closing(
            &ws,
            &plan,
            &reached,
            &selected,
            None,
            Duration::from_secs(9),
        );
        assert!(pictures.is_none());
        assert!(summary.contains("qmllint (no .qml file changed)"));
        assert!(summary.contains(
            "integration tests of kourier, kourier-app, kourier-e2e, kourier-qt, kourier-signal"
        ));
    }

    /// What a change reaches and what the quick tier runs of it (T5c), by file.
    fn quick_selection(file: &str) -> (Vec<String>, Option<usize>) {
        let ws = repo();
        let plan = plan(&ws, &strings(&[file]));
        let reached = affected(&ws, plan.changed.keys());
        let (selected, left_out) = without_window_tests(
            integration_tests(&ws, &reached),
            ui_side_changed(plan.changed.keys()),
        );
        (selected.into_iter().map(|(p, _)| p).collect(), left_out)
    }

    #[test]
    fn a_change_below_the_ui_leaves_the_window_tests_out() {
        for file in [
            "crates/kourier-core/src/lib.rs",
            "crates/kourier-model/src/lib.rs",
            "crates/kourier-backend-api/src/lib.rs",
            "crates/kourier-fake/src/lib.rs",
            "crates/kourier-db/src/lib.rs",
            "crates/kourier-desktop/src/lib.rs",
        ] {
            // The dependency walk reaches the Qt crate ...
            let ws = repo();
            let plan = plan(&ws, &strings(&[file]));
            let reached = affected(&ws, plan.changed.keys());
            assert!(reached.contains_key("kourier-qt"), "{file}: {reached:?}");
            // ... and its window tests are the one thing quick leaves out.
            let (run, left_out) = quick_selection(file);
            assert_eq!(left_out, Some(1), "{file}");
            assert!(!run.contains(&"kourier-qt".to_owned()), "{file}: {run:?}");
        }
        // Everything else the walk reached still runs: the core's change keeps its dependents.
        let (run, _) = quick_selection("crates/kourier-core/src/lib.rs");
        for package in [
            "kourier-core",
            "kourier-e2e",
            "kourier-viewmodel",
            "kourier-app",
        ] {
            assert!(run.contains(&package.to_owned()), "{package}: {run:?}");
        }
        // A signal change reaches no window test to leave out of the Qt crate's walk.
        let (run, left_out) = quick_selection("crates/kourier-signal/src/lib.rs");
        assert!(
            left_out.is_some() && !run.contains(&"kourier-qt".to_owned()),
            "{run:?}"
        );
    }

    #[test]
    fn a_change_on_the_ui_side_keeps_the_window_tests() {
        for file in [
            "crates/kourier-qt/qml/Main.qml",
            "crates/kourier-qt/src/lib.rs",
            "crates/kourier-qt/tests/chat_ui.rs",
            "crates/kourier-qt/Cargo.toml",
            "crates/kourier-viewmodel/src/lib.rs",
            "crates/kourier-app/src/lib.rs",
        ] {
            let (run, left_out) = quick_selection(file);
            assert_eq!(left_out, None, "{file}");
            assert!(run.contains(&"kourier-qt".to_owned()), "{file}: {run:?}");
        }
        // The rule names exactly these three.
        assert_eq!(UI_SIDE, ["kourier-qt", "kourier-viewmodel", "kourier-app"]);
        assert!(ui_side_changed(&strings(&["kourier-core", "kourier-app"])));
        assert!(!ui_side_changed(&strings(&["kourier-core", "kourier"])));
        assert!(!ui_side_changed(&strings(&[])));
    }

    #[test]
    fn the_closing_report_says_the_window_tests_were_left_out_and_why() {
        let ws = repo();
        let plan = plan(&ws, &strings(&["crates/kourier-core/src/lib.rs"]));
        let reached = affected(&ws, plan.changed.keys());
        let (selected, left_out) = without_window_tests(
            integration_tests(&ws, &reached),
            ui_side_changed(plan.changed.keys()),
        );
        let (summary, _) = closing(
            &ws,
            &plan,
            &reached,
            &selected,
            left_out,
            Duration::from_secs(9),
        );
        assert!(
            summary.contains("the Qt window tests of kourier-qt (1 test binaries; nothing of kourier-qt, its QML, kourier-viewmodel or kourier-app changed)"),
            "{summary}"
        );
        assert!(!summary.contains("integration tests of kourier, kourier-app, kourier-backend-api, kourier-core, kourier-e2e, kourier-fake, kourier-qt"));
        assert!(window_tests_note(26).contains("26 test binaries"));
        // On the UI side nothing is left out and the report does not say so.
        let plan = super::plan(&ws, &strings(&["crates/kourier-qt/qml/Main.qml"]));
        let reached = affected(&ws, plan.changed.keys());
        let selected = integration_tests(&ws, &reached);
        let (summary, _) = closing(
            &ws,
            &plan,
            &reached,
            &selected,
            None,
            Duration::from_secs(9),
        );
        assert!(!summary.contains("window tests"), "{summary}");
    }

    /// T5 review F3: the integration tests of a package are reported as run, so the report must
    /// also say that its slow tests were not (the guards of a failpoint are exactly those).
    #[test]
    fn the_closing_report_says_the_slow_set_was_left_out_and_who_runs_it() {
        let ws = repo();
        let plan = plan(&ws, &strings(&["crates/kourier-core/src/lib.rs"]));
        let reached = affected(&ws, plan.changed.keys());
        let (selected, left_out) = without_window_tests(
            integration_tests(&ws, &reached),
            ui_side_changed(plan.changed.keys()),
        );
        let (summary, _) = closing(
            &ws,
            &plan,
            &reached,
            &selected,
            left_out,
            Duration::from_secs(9),
        );
        let note = slow_left_out(&selected).expect("kourier-core and kourier-e2e have slow tests");
        assert!(summary.contains(&note), "{summary}");
        // The count is the number of the list's tests of the packages that ran (the core and the
        // e2e crate that depends on it; the Signal crate does not depend on the core).
        let wanted: usize = ["kourier-core", "kourier-e2e"]
            .iter()
            .map(|p| {
                crate::slow::SLOW_TESTS
                    .iter()
                    .filter(|t| t.package == *p)
                    .count()
            })
            .sum();
        assert!(note.contains(&format!("{wanted} tests")), "{note}");
        assert!(
            note.contains("only the full `cargo xtask ci` runs them"),
            "{note}"
        );
        let core = crate::slow::SLOW_TESTS
            .iter()
            .filter(|t| t.package == "kourier-core")
            .count();
        assert!(note.contains(&format!("kourier-core {core}")), "{note}");
        // The slow set comes before the doc tests, after the other things left out.
        let (slow, docs) = (summary.find("the slow set"), summary.find("; doc tests"));
        assert!(slow.is_some() && slow < docs, "{summary}");
        // A package without slow tests: nothing to say.
        let none = vec![("kourier-model".to_owned(), vec!["it".to_owned()])];
        assert_eq!(slow_left_out(&none), None);
        assert_eq!(slow_left_out(&[]), None);
    }

    #[test]
    fn what_changes_no_test_result_is_neutral_by_its_name_alone() {
        for path in [
            "README.md",
            "CLAUDE.md",
            "docs/architecture.md",
            ".github/workflows/ci.yml",
            "LICENSES/AGPL-3.0-only.txt",
            "crates/kourier-core/README.md",
            "LICENSE",
        ] {
            assert!(is_neutral(path), "{path}");
        }
        // Read by a test (the runbook, the desktop file, the harness), or able to alter a build.
        for path in [
            "docs/smoke-run.md",
            "docs/whatsapp-run.md",
            "docs/telegram-run.md",
            "packaging/app.kourier.Kourier.desktop",
            "packaging/README.md",
            "tests/mock-harness/README.md",
            "tests/mock-harness/harness.mjs",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            "crates/kourier-core/src/lib.rs",
            "xtask/src/main.rs",
        ] {
            assert!(!is_neutral(path), "{path}");
        }
        // The same set as the plan's: `owner` says Neutral exactly for these.
        let ws = repo();
        for path in [
            "README.md",
            "docs/smoke-run.md",
            "docs/whatsapp-run.md",
            "docs/telegram-run.md",
            "packaging/README.md",
            "docs/architecture.md",
            "LICENSE",
            "tests/mock-harness/README.md",
        ] {
            assert_eq!(
                is_neutral(path),
                owner(&ws, path) == Owner::Neutral,
                "{path}"
            );
        }
    }

    #[test]
    fn lists_are_cut() {
        assert_eq!(list(&strings(&["a", "b"]), 3), "a, b");
        assert_eq!(
            list(&strings(&["a", "b", "c", "d", "e"]), 3),
            "a, b, c and 2 more"
        );
        assert_eq!(duration(Duration::from_secs(151)), "2 min 31 s");
        assert_eq!(duration(Duration::from_secs(9)), "9 s");
    }
}
