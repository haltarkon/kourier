//! How the test steps of both CI tiers run: through cargo-nextest, or through `cargo test` when
//! nextest is not installed.
//!
//! `cargo test` runs the test binaries one after another (106 of them, ~11 minutes); nextest runs
//! every test in a process of its own, several at a time across all binaries, and holds back
//! what cannot run side by side by the test groups of `.config/nextest.toml`. Without nextest
//! the steps are what they always were, and a warning says so and says how to install it:
//! never a silent skip. `KOURIER_NO_NEXTEST=1` asks for `cargo test` on purpose (to compare,
//! or when nextest itself is suspected).
//!
//! nextest does not run doc tests: the full tier has a `cargo test --doc` step after it. The
//! quick tier never ran doc tests and still does not. Test binaries built with
//! `harness = false` answer nextest's `--list` themselves (`crates/kourier-qt/tests/support`,
//! `crates/kourier-desktop/tests/dbus.rs`); each is one nextest test.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// The oldest nextest the configuration is written for (`.config/nextest.toml`:
/// `nextest-version`; test groups and `--message-format json` are older than this).
pub const MIN_VERSION: (u32, u32, u32) = (0, 9, 100);

/// Installs the release this was tried with or a newer one.
pub const INSTALL_HINT: &str = "cargo install cargo-nextest --locked";

/// Set to anything but `0` or the empty string to use `cargo test` although nextest is there.
pub const OPT_OUT_ENV: &str = "KOURIER_NO_NEXTEST";

/// What runs the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Nextest,
    CargoTest,
}

/// What `cargo nextest --version` and the environment say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Installed and new enough, with its version.
    Ready((u32, u32, u32)),
    /// `cargo nextest` is not a command.
    Missing,
    /// Installed but older than [`MIN_VERSION`], or its version could not be read.
    TooOld(String),
    /// [`OPT_OUT_ENV`] is set.
    OptedOut,
}

/// `cargo-nextest 0.9.146 (8af696ddc 2026-09-21)` → `(0, 9, 146)`.
pub fn parse_version(output: &str) -> Option<(u32, u32, u32)> {
    let word = output.lines().next()?.split_whitespace().nth(1)?;
    let mut parts = word.split(['.', '-']);
    let mut next = || parts.next()?.parse::<u32>().ok();
    Some((next()?, next()?, next()?))
}

/// The decision from what was found: `version_output` is the output of `cargo nextest
/// --version`, or `None` when the command failed.
pub fn classify(version_output: Option<&str>, opted_out: bool) -> Probe {
    if opted_out {
        return Probe::OptedOut;
    }
    let Some(out) = version_output else {
        return Probe::Missing;
    };
    match parse_version(out) {
        Some(v) if v >= MIN_VERSION => Probe::Ready(v),
        Some((a, b, c)) => Probe::TooOld(format!("{a}.{b}.{c}")),
        None => Probe::TooOld(format!(
            "an unreadable version ({:?})",
            out.lines().next().unwrap_or("")
        )),
    }
}

fn probe() -> Probe {
    let opted_out = std::env::var(OPT_OUT_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
    let output = crate::cargo()
        .args(["nextest", "--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    classify(output.as_deref(), opted_out)
}

/// What to tell the user when the tests will not run through nextest, and why; `None` when
/// they will.
pub fn warning(probe: &Probe) -> Option<String> {
    match probe {
        Probe::Ready(_) => None,
        Probe::OptedOut => Some(format!(
            "{OPT_OUT_ENV} is set: the tests run with `cargo test`, one test binary after another."
        )),
        Probe::Missing => Some(format!(
            "cargo-nextest is not installed, so the tests run with `cargo test`, one test \
             binary after another (about three times slower). Install it once: `{INSTALL_HINT}`"
        )),
        Probe::TooOld(found) => Some(format!(
            "cargo-nextest {found} is older than {}.{}.{}, so the tests run with `cargo test`, \
             one test binary after another (about three times slower). Update it: \
             `{INSTALL_HINT}`",
            MIN_VERSION.0, MIN_VERSION.1, MIN_VERSION.2
        )),
    }
}

/// The engine for this run; says on stderr which, and why when it is `cargo test`.
pub fn engine() -> Engine {
    let probe = probe();
    match (warning(&probe), &probe) {
        (None, Probe::Ready((a, b, c))) => {
            eprintln!("\ntests: cargo-nextest {a}.{b}.{c} (test groups: .config/nextest.toml)");
            Engine::Nextest
        }
        (note, _) => {
            eprintln!("\ntests: WARNING: {}", note.unwrap_or_default());
            Engine::CargoTest
        }
    }
}

/// The Qt crate's tests need a platform plugin; they get the offscreen one.
fn offscreen(cmd: &mut Command) -> &mut Command {
    cmd.env("QT_QPA_PLATFORM", "offscreen")
}

/// The package selection of the full tier's non-Qt test step. EVERY other step that builds
/// tests (the quick tier's unit and integration steps) starts from this same selection, because
/// a different one (`-p a -p b`) unifies features differently and is a second variant of every
/// dependency and test binary (T3 review F1: hundreds of units built again after each full run).
pub const NON_QT: [&str; 3] = ["--workspace", "--exclude", "kourier-qt"];

/// The package selection of the Qt crate's test step, in the same way.
pub const QT: [&str; 2] = ["-p", "kourier-qt"];

/// Set to `1` in every test step: a test that needs `unshare -rn` (the WhatsApp crate's offline
/// children) FAILS where user namespaces are unavailable instead of skipping, so a green run has run
/// them (`kourier-whatsapp/tests/support/offline.rs`).
pub const REQUIRE_UNSHARE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";

/// Set (to `1`; the tests read "set to anything") in every test step: the Signal mock-server
/// tests FAIL when Node 24 or the harness's `node_modules` is missing instead of printing
/// "skipping" and passing, so a green run, and the record `green` keeps of it, has run them (T5
/// review F1: otherwise a run with no Node found was recorded green and replayed as "green (same
/// tree)" in the environment that has it). `crates/kourier-signal/tests/support/harness.rs`.
pub const REQUIRE_MOCK_HARNESS_ENV: &str = "KOURIER_REQUIRE_MOCK_HARNESS";

/// The temporary directory of every test step, given as `TMPDIR`: `/dev/shm/kourier-test-<uid>`,
/// a RAM tmpfs of its own beside `/tmp` (the system temporary directory where there is no
/// `/dev/shm`). This machine's `/tmp` is a RAM tmpfs that other sessions fill: on 2026-10-19 a
/// Signal mock test's link failed with "database or disk is full" in the very second another
/// session's copy of a disk image filled `/tmp` (`target/flake/`). The disk is no place for it:
/// with `target/test-tmp` on the encrypted SSD every fsync of the tests' databases cost
/// milliseconds and the fsync-heavy tests took 6 to 35 times as long (`calls_attack` exhaustive
/// 3.6 s -> 127 s, the W4 kill loop 19 s -> over its 300 s), 2026-10-19 (`target/wa/ci-t1b.log`).
/// One test run at a time holds the test lock, so the two checkouts never share it at once. The
/// green records do not hash `TMPDIR`.
pub fn test_tmp() -> PathBuf {
    let shm = std::path::Path::new("/dev/shm");
    let base = if shm.is_dir() {
        shm.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    base.join(format!("kourier-test-{uid}"))
}

/// Empties [`test_tmp`] before a tier's test steps: tests killed on purpose (the kill loops)
/// leave their directories behind. Only the ci's own step functions call it, never a unit test,
/// because a unit test runs inside a test step whose tests use the directory.
fn prepare_test_tmp() -> Result<()> {
    let dir = test_tmp();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("emptying {}", dir.display())),
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    // Only this user's tests: `/dev/shm` is shared by every user of the machine.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} private", dir.display()))
}

/// Gives `cmd` the test steps' temporary directory.
fn with_test_tmp(cmd: &mut Command) -> &mut Command {
    cmd.env("TMPDIR", test_tmp())
}

/// `cargo nextest run <args>` or `cargo test <args>`: the arguments that select what runs are
/// the same cargo flags for both.
fn test_command(engine: Engine, select: &[&str]) -> Command {
    let mut cmd = crate::cargo();
    match engine {
        Engine::Nextest => cmd.args(["nextest", "run"]),
        Engine::CargoTest => cmd.arg("test"),
    };
    cmd.args(select);
    cmd.env(REQUIRE_UNSHARE_ENV, "1");
    cmd.env(REQUIRE_MOCK_HARNESS_ENV, "1");
    with_test_tmp(&mut cmd);
    cmd
}

/// What the engine is called in a record of a green run.
pub fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Nextest => "nextest",
        Engine::CargoTest => "cargo-test",
    }
}

/// The full tier's test steps: every test of the workspace, the Qt crate's with the offscreen
/// platform, and the doc tests.
pub fn full(engine: Engine) -> Result<()> {
    prepare_test_tmp()?;
    let how = match engine {
        Engine::Nextest => "nextest",
        Engine::CargoTest => "cargo test",
    };
    crate::run(
        &format!("tests (non-Qt, {how})"),
        &mut test_command(engine, &NON_QT),
    )?;
    crate::run(
        &format!("tests (Qt, offscreen, {how})"),
        offscreen(&mut test_command(engine, &QT)),
    )?;
    if engine == Engine::Nextest {
        // `cargo test` ran them in the steps above.
        crate::run(
            "doc tests",
            with_test_tmp(crate::cargo().args(["test", "--workspace", "--doc"])),
        )?;
    }
    Ok(())
}

/// The arguments of the non-Qt step of the full tier after a green quick run on the same tree
/// (T5b): the full tier's package selection ([`NON_QT`]) and the filter of what quick did not
/// run ([`crate::green::Remainder::non_qt_filter`]).
pub fn remainder_select(rest: &crate::green::Remainder) -> Vec<String> {
    let mut args: Vec<String> = NON_QT.iter().map(|a| (*a).to_owned()).collect();
    args.push("-E".to_owned());
    args.push(rest.non_qt_filter.clone());
    args
}

/// The full tier after a green quick run on the same tree: the integration tests quick did not
/// run and the slow set, the Qt window tests when quick left them out, and the doc tests. The
/// unit tests of every package all ran in quick, in both package selections.
pub fn full_remainder(rest: &crate::green::Remainder) -> Result<()> {
    prepare_test_tmp()?;
    let select = remainder_select(rest);
    let select: Vec<&str> = select.iter().map(String::as_str).collect();
    crate::run(
        "tests (non-Qt, the rest after quick, nextest)",
        &mut test_command(Engine::Nextest, &select),
    )?;
    if rest.qt_window_tests {
        let select = window_select_all();
        let select: Vec<&str> = select.iter().map(String::as_str).collect();
        crate::run(
            "tests (Qt window tests, offscreen, nextest)",
            offscreen(&mut test_command(Engine::Nextest, &select)),
        )?;
    } else if let Some(select) = window_select_slow() {
        // Quick ran the other window tests; the slow ones are the full run's (SE6).
        let select: Vec<&str> = select.iter().map(String::as_str).collect();
        crate::run(
            "tests (Qt window tests, the slow set, offscreen, nextest)",
            offscreen(&mut test_command(Engine::Nextest, &select)),
        )?;
    }
    crate::run(
        "doc tests",
        with_test_tmp(crate::cargo().args(["test", "--workspace", "--doc"])),
    )
}

/// The quick tier's unit tests: the libraries and binaries of every package.
pub fn quick_unit(engine: Engine) -> Result<()> {
    prepare_test_tmp()?;
    crate::run(
        "unit tests of every package (non-Qt)",
        &mut test_command(engine, &[&NON_QT[..], &["--lib", "--bins"]].concat()),
    )?;
    crate::run(
        "unit tests of kourier-qt (offscreen)",
        offscreen(&mut test_command(
            engine,
            &[&QT[..], &["--lib", "--bins"]].concat(),
        )),
    )
}

/// The arguments of one nextest run over the integration tests of the non-Qt `packages`: the
/// full tier's own package selection ([`NON_QT`]) and a filter set that picks the integration
/// test binaries ("kind(test)") of these packages out of what it builds. So the quick tier
/// builds the very units the full tier builds (ONE variant of every dependency and test binary:
/// after a full run a quick one compiles nothing it does not have to), runs the tests of all
/// reached packages in one run so one scheduler fills the cores, and has the features one
/// package asks of another (`kourier-desktop`'s `testing`, which `kourier-app` turns on for its
/// tests) on, as in the full run. `kind(test)` is every integration test target and nothing
/// else: the unit tests of these packages ran in `quick_unit`. Only the nextest engine takes
/// these arguments; the cargo-test fallback runs a package after another (`quick::test_args`).
/// A target whose `required-features` are off makes cargo stop with "requires the features"
/// (loud, never a silent skip); that cannot happen today because `kourier-desktop` is never
/// reached without `kourier-app`, whose dev dependency turns the feature on (test
/// `the_feature_a_gated_test_needs_is_turned_on_by_a_dependent`).
pub fn integration_select(packages: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = NON_QT.iter().map(|a| (*a).to_owned()).collect();
    args.push("-E".to_owned());
    // Without the slow set (T5d, `slow.rs`): the full run and the remainder after quick take it.
    args.push(crate::slow::integration_filter(packages));
    args
}

/// The same for the Qt crate: the full tier's `-p kourier-qt` ([`QT`]) and `kind(test)` without the
/// slow entries of the Qt crate (SE6, `slow::window_filter`: the full run and the remainder after
/// quick take them).
pub fn integration_select_qt() -> Vec<String> {
    let mut args: Vec<String> = QT.iter().map(|a| (*a).to_owned()).collect();
    args.push("-E".to_owned());
    args.push(crate::slow::window_filter());
    args
}

/// Every integration (window) test of the Qt crate, the slow ones too: the full run after a quick
/// run that left the window tests out.
fn window_select_all() -> Vec<String> {
    let mut args: Vec<String> = QT.iter().map(|a| (*a).to_owned()).collect();
    args.push("-E".to_owned());
    args.push("kind(test)".to_owned());
    args
}

/// The slow entries of the Qt crate alone (SE6): the full run after a quick run that did the other
/// window tests. `None` when the list has none.
fn window_select_slow() -> Option<Vec<String>> {
    let filter = crate::slow::window_slow_filter()?;
    let mut args: Vec<String> = QT.iter().map(|a| (*a).to_owned()).collect();
    args.push("-E".to_owned());
    args.push(filter);
    Some(args)
}

/// The quick tier's integration tests of the reached packages `selected` (package, its test
/// targets): with nextest in one run for the non-Qt packages and one for the Qt crate; with
/// `cargo test` one package after another, each by its target names.
pub fn quick_integration(engine: Engine, selected: &[(String, Vec<String>)]) -> Result<()> {
    prepare_test_tmp()?;
    match engine {
        Engine::CargoTest => {
            for (package, tests) in selected {
                let mut cmd = crate::cargo();
                cmd.args(crate::quick::test_args(package, tests));
                with_test_tmp(&mut cmd);
                if crate::quick::needs_offscreen(package) {
                    offscreen(&mut cmd);
                }
                crate::run(&format!("integration tests of {package}"), &mut cmd)?;
            }
        }
        Engine::Nextest => {
            let (qt, other): (Vec<&str>, Vec<&str>) = selected
                .iter()
                .map(|(p, _)| p.as_str())
                .partition(|p| crate::quick::needs_offscreen(p));
            if !other.is_empty() {
                let select = integration_select(&other);
                let select: Vec<&str> = select.iter().map(String::as_str).collect();
                crate::run(
                    &format!("integration tests of {} (nextest)", other.join(", ")),
                    &mut test_command(engine, &select),
                )?;
            }
            if !qt.is_empty() {
                let select = integration_select_qt();
                let select: Vec<&str> = select.iter().map(String::as_str).collect();
                crate::run(
                    &format!(
                        "integration tests of {} (offscreen, nextest)",
                        qt.join(", ")
                    ),
                    offscreen(&mut test_command(engine, &select)),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// T5 review F1: a green run has RUN the mock-server tests. They skip (and pass) where Node 24
    /// is not found unless this variable is set, and `green` would record that skip as green.
    #[test]
    fn every_test_step_requires_the_mock_harness_and_unshare() {
        for engine in [Engine::Nextest, Engine::CargoTest] {
            let cmd = test_command(engine, &NON_QT);
            let env: BTreeMap<_, _> = cmd
                .get_envs()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_owned())))
                .collect();
            for name in [REQUIRE_MOCK_HARNESS_ENV, REQUIRE_UNSHARE_ENV] {
                assert_eq!(
                    env.get(name).and_then(|v| v.as_deref()),
                    Some(std::ffi::OsStr::new("1")),
                    "{name} for {engine:?}"
                );
            }
            // The tests' temporary files go to the disk, never to the RAM `/tmp` other sessions fill.
            assert_eq!(
                env.get("TMPDIR").and_then(|v| v.as_deref()),
                Some(test_tmp().as_os_str()),
                "TMPDIR for {engine:?}"
            );
            if std::path::Path::new("/dev/shm").is_dir() {
                assert!(
                    test_tmp().starts_with("/dev/shm"),
                    "the tests' temporary files go to a tmpfs of their own, never to the /tmp other sessions fill"
                );
            }
        }
        assert_eq!(REQUIRE_MOCK_HARNESS_ENV, "KOURIER_REQUIRE_MOCK_HARNESS");
        // The name is the one the tests read.
        let harness = std::fs::read_to_string(
            crate::root().join("crates/kourier-signal/tests/support/harness.rs"),
        )
        .expect("the harness driver");
        assert!(
            harness.contains(&format!(
                "REQUIRE_ENV: &str = \"{REQUIRE_MOCK_HARNESS_ENV}\""
            )),
            "the harness reads another variable"
        );
    }

    #[test]
    fn the_version_is_read_from_the_first_line() {
        assert_eq!(
            parse_version("cargo-nextest 0.9.146 (8af696ddc 2026-09-21)\nrelease: 0.9.146\n"),
            Some((0, 9, 146))
        );
        assert_eq!(
            parse_version("cargo-nextest 0.9.100-b.2"),
            Some((0, 9, 100))
        );
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("cargo-nextest"), None);
        assert_eq!(parse_version("cargo-nextest x.y.z"), None);
    }

    #[test]
    fn the_engine_follows_what_is_installed() {
        let ok = "cargo-nextest 0.9.146 (x 2026-09-21)\n";
        assert_eq!(classify(Some(ok), false), Probe::Ready((0, 9, 146)));
        assert_eq!(
            classify(Some("cargo-nextest 0.9.100 (x)"), false),
            Probe::Ready((0, 9, 100)),
            "the minimum itself is enough"
        );
        assert_eq!(
            classify(Some("cargo-nextest 0.9.99 (x)"), false),
            Probe::TooOld("0.9.99".to_owned())
        );
        assert_eq!(
            classify(Some("cargo-nextest 0.8.200 (x)"), false),
            Probe::TooOld("0.8.200".to_owned())
        );
        assert_eq!(
            classify(Some("cargo-nextest 1.0.0 (x)"), false),
            Probe::Ready((1, 0, 0))
        );
        assert!(matches!(classify(Some("what?"), false), Probe::TooOld(_)));
        assert_eq!(classify(None, false), Probe::Missing);
        assert_eq!(classify(Some(ok), true), Probe::OptedOut);
        assert_eq!(classify(None, true), Probe::OptedOut);
    }

    #[test]
    fn without_nextest_the_warning_says_why_and_how_to_install_it() {
        assert_eq!(warning(&Probe::Ready((0, 9, 146))), None);
        let missing = warning(&Probe::Missing).expect("a warning");
        assert!(missing.contains("not installed"), "{missing}");
        assert!(
            missing.contains("cargo install cargo-nextest --locked"),
            "{missing}"
        );
        let old = warning(&Probe::TooOld("0.9.99".into())).expect("a warning");
        assert!(old.contains("0.9.99") && old.contains("0.9.100"), "{old}");
        assert!(old.contains(INSTALL_HINT), "{old}");
        let opted = warning(&Probe::OptedOut).expect("a warning");
        assert!(opted.contains(OPT_OUT_ENV), "{opted}");
    }

    #[test]
    fn the_minimum_is_the_one_the_configuration_asks_for() {
        let config = include_str!("../../.config/nextest.toml");
        let line = config
            .lines()
            .find(|l| l.starts_with("nextest-version"))
            .expect("nextest-version in .config/nextest.toml");
        let version = line
            .split('"')
            .nth(1)
            .and_then(|v| parse_version(&format!("x {v}")))
            .expect("a version");
        assert_eq!(version, MIN_VERSION);
    }

    #[test]
    fn the_integration_tests_of_all_reached_packages_are_one_run() {
        let slow = crate::slow::filter();
        assert_eq!(
            integration_select(&["kourier-app", "kourier-desktop"]),
            [
                "--workspace".to_owned(),
                "--exclude".to_owned(),
                "kourier-qt".to_owned(),
                "-E".to_owned(),
                format!(
                    "kind(test) & (package(=kourier-app) | package(=kourier-desktop)) & !({slow})"
                )
            ]
        );
        assert_eq!(
            integration_select(&["kourier"]),
            [
                "--workspace".to_owned(),
                "--exclude".to_owned(),
                "kourier-qt".to_owned(),
                "-E".to_owned(),
                format!("kind(test) & (package(=kourier)) & !({slow})")
            ],
            "every integration test target (`kind(test)`), not the unit tests again"
        );
        // The Qt crate's step: the window tests without its slow entries (SE6), which the full run takes.
        assert_eq!(
            integration_select_qt(),
            [
                "-p".to_owned(),
                "kourier-qt".to_owned(),
                "-E".to_owned(),
                crate::slow::window_filter()
            ]
        );
        assert!(
            integration_select_qt()[3].starts_with("kind(test) & !((package(=kourier-qt) & test(="),
            "{:?}",
            integration_select_qt()
        );
        // What quick left out of them, and everything when quick left the window tests out.
        assert_eq!(
            window_select_slow().expect("the Qt crate has a slow entry")[3],
            crate::slow::window_slow_filter().expect("the same")
        );
        assert_eq!(window_select_all()[2..], ["-E", "kind(test)"]);
    }

    /// T3 review F1: the quick tier's steps select packages the way the full tier's do, so one
    /// `cargo` feature set, one hash for every dependency and test binary. A `-p a -p b` in
    /// the quick tier is a second variant that the next full run's prune deletes.
    #[test]
    fn every_step_that_builds_tests_starts_from_the_full_tiers_selection() {
        let select = integration_select(&["kourier-core", "kourier-e2e"]);
        assert_eq!(select[..3], NON_QT, "non-Qt: {select:?}");
        assert!(
            select.iter().all(|a| a != "-p" && a != "--test"),
            "a `-p`/`--test` selection is another variant: {select:?}"
        );
        let qt = integration_select_qt();
        assert_eq!(qt[..2], QT, "Qt: {qt:?}");
        // The quick tier's unit step and the full tier take the very same arrays: the arrays
        // are the only place that spells the selection.
        let source = include_str!("runner.rs");
        let code = source.split("#[cfg(test)]").next().expect("the code");
        assert_eq!(
            code.matches("\"--exclude\"").count(),
            1,
            "a second spelling of the non-Qt selection can drift from `NON_QT`"
        );
        assert_eq!(
            code.matches("\"kourier-qt\"").count(),
            2,
            "the Qt crate's name stands in `NON_QT` and `QT` only"
        );
    }

    #[test]
    fn the_rest_after_quick_takes_the_full_tiers_selection_and_the_remainders_filter() {
        let rest = crate::green::Remainder {
            quick_at: 0,
            non_qt_filter: "kind(test) & !(x)".to_owned(),
            qt_window_tests: false,
            qmllint: false,
        };
        let select = remainder_select(&rest);
        assert_eq!(select[..3], NON_QT);
        assert_eq!(select[3..], ["-E", "kind(test) & !(x)"]);
        assert_eq!(engine_name(Engine::Nextest), "nextest");
        assert_eq!(engine_name(Engine::CargoTest), "cargo-test");
    }

    #[test]
    fn the_filter_set_is_the_same_filter_of_the_replay_and_of_the_run() {
        // The filter set only picks what runs: a `cargo test --no-run` replay (the prune's
        // query) must not see it. See `prune::query_for`.
        let q = crate::prune::query_for(
            &["nextest", "run"]
                .into_iter()
                .map(str::to_owned)
                .chain(integration_select(&["kourier-core"]))
                .collect::<Vec<_>>(),
        )
        .expect("a query");
        assert!(
            q.iter().all(|a| a != "-E" && !a.contains("kind(test)")),
            "{q:?}"
        );
    }

    #[test]
    fn the_feature_a_gated_test_needs_is_turned_on_by_a_dependent() {
        // `kourier-desktop`'s test targets `dbus` and `hints` need its feature `testing`. A
        // workspace run gets it from `kourier-app`'s dev dependency (features unify across the
        // selection), and `kourier-app` depends on `kourier-desktop`, so the quick tier's walk
        // over dependents reaches it whenever `kourier-desktop` is reached. Remove the feature
        // there and the quick tier stops with cargo's "requires the features" the first time
        // `kourier-desktop` changes.
        let app = include_str!("../../crates/kourier-app/Cargo.toml");
        let dev = app
            .split("[dev-dependencies]")
            .nth(1)
            .expect("[dev-dependencies] in kourier-app");
        assert!(
            dev.lines()
                .any(|l| l.starts_with("kourier-desktop") && l.contains("\"testing\"")),
            "kourier-app must turn on kourier-desktop's `testing` for its tests"
        );
        let desktop = include_str!("../../crates/kourier-desktop/Cargo.toml");
        assert!(
            desktop.contains("required-features = [\"testing\"]"),
            "the gated tests of kourier-desktop moved: update this test and the doc of \
             `integration_select`"
        );
    }

    #[test]
    fn both_engines_take_the_same_selection() {
        let select = ["--workspace", "--exclude", "kourier-qt"];
        let args = |cmd: &Command| -> Vec<String> {
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args(&test_command(Engine::Nextest, &select)),
            ["nextest", "run", "--workspace", "--exclude", "kourier-qt"]
        );
        assert_eq!(
            args(&test_command(Engine::CargoTest, &select)),
            ["test", "--workspace", "--exclude", "kourier-qt"]
        );
    }

    /// Fix F12: every test step asks for a failure, not a skip, where `unshare -rn` is missing.
    #[test]
    fn every_test_step_requires_the_offline_namespace() {
        for engine in [Engine::Nextest, Engine::CargoTest] {
            let cmd = test_command(engine, &["--workspace"]);
            let set = cmd
                .get_envs()
                .find(|(k, _)| *k == REQUIRE_UNSHARE_ENV)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned());
            assert_eq!(set.as_deref(), Some("1"), "{engine:?}");
        }
    }
}
