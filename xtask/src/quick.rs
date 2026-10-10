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
}

pub fn parse_args(args: &[String]) -> Result<CiArgs> {
    let mut parsed = CiArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quick" => parsed.quick = true,
            "--since" => {
                let rev = it.next().context("--since needs a revision")?;
                parsed.since = Some(rev.clone());
            }
            _ if arg.starts_with("--since=") => {
                parsed.since = Some(arg["--since=".len()..].to_owned());
            }
            _ => bail!("ci: unknown argument {arg:?} (usage: ci [--quick [--since <rev>]])"),
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
const READERS: [(&str, &[&str]); 3] = [
    // The smoke tool's tests check the runbook against the tool.
    ("docs/smoke-run.md", &["xtask"]),
    // `kourier`'s single-instance test reads the desktop file; the installer's tests all three.
    ("packaging/", &["kourier", "xtask"]),
    // The Signal mock the two mock suites run against.
    ("tests/mock-harness/", &["kourier-signal", "kourier-e2e"]),
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
/// network and these are the Signal backend and the two test-only crates (the same list as the
/// digest of `cargo xtask shots`).
const NO_PICTURE_CRATES: [&str; 3] = ["kourier-signal", "kourier-e2e", "kourier-coexist"];

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
    if path.ends_with(".md")
        || NEUTRAL_FILES.contains(&path)
        || NEUTRAL_DIRS.iter().any(|d| path.starts_with(d))
    {
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
fn needs_offscreen(package: &str) -> bool {
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

pub fn run(since: Option<&str>) -> Result<()> {
    let started = Instant::now();
    let ws = Workspace::load(&crate::root())?;
    let files = changed_files(since)?;
    let plan = plan(&ws, &files);
    if !plan.full.is_empty() {
        eprintln!(
            "\nquick: {} can change every build: this is a full run (cargo xtask ci)",
            list(&plan.full, 4)
        );
        return crate::ci_full();
    }
    let reached = affected(&ws, plan.changed.keys());
    describe(&files, since, &plan, &reached);

    crate::lint_steps()?;
    crate::run(
        "unit tests of every package (non-Qt)",
        crate::cargo().args([
            "test",
            "--workspace",
            "--exclude",
            "kourier-qt",
            "--lib",
            "--bins",
        ]),
    )?;
    crate::run(
        "unit tests of kourier-qt (offscreen)",
        crate::cargo()
            .args(["test", "-p", "kourier-qt", "--lib", "--bins"])
            .env("QT_QPA_PLATFORM", "offscreen"),
    )?;
    let selected = integration_tests(&ws, &reached);
    for (package, tests) in &selected {
        let mut cmd = crate::cargo();
        cmd.args(test_args(package, tests));
        if needs_offscreen(package) {
            cmd.env("QT_QPA_PLATFORM", "offscreen");
        }
        crate::run(&format!("integration tests of {package}"), &mut cmd)?;
    }
    if !plan.qml.is_empty() {
        crate::qmllint()?;
    }

    let (summary, pictures) = closing(&ws, &plan, &reached, &selected, started.elapsed());
    eprintln!("\n{summary}");
    if let Some(line) = pictures {
        eprintln!("{line}");
    }
    Ok(())
}

/// The closing report of a passed run: what ran, what did not and why, the reminder of the full
/// run; and the line about pictures, when a changed file can have changed one.
fn closing(
    ws: &Workspace,
    plan: &Plan,
    reached: &BTreeMap<String, Option<String>>,
    selected: &[(String, Vec<String>)],
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
            "kourier-app",
            &[
                "kourier-core",
                "kourier-backend-api",
                "kourier-model",
                "kourier-desktop",
                "kourier-blob",
                "kourier-signal",
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
                "kourier-viewmodel",
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
                since: None
            }
        );
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
        let (summary, pictures) =
            closing(&ws, &plan, &reached, &selected, Duration::from_secs(151));
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
        let (summary, pictures) = closing(&ws, &plan, &reached, &selected, Duration::from_secs(9));
        assert!(pictures.is_none());
        assert!(summary.contains("qmllint (no .qml file changed)"));
        assert!(summary.contains(
            "integration tests of kourier, kourier-app, kourier-e2e, kourier-qt, kourier-signal"
        ));
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
