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

use anyhow::Result;
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

/// `cargo nextest run <args>` or `cargo test <args>`: the arguments that select what runs are
/// the same cargo flags for both.
fn test_command(engine: Engine, select: &[&str]) -> Command {
    let mut cmd = crate::cargo();
    match engine {
        Engine::Nextest => cmd.args(["nextest", "run"]),
        Engine::CargoTest => cmd.arg("test"),
    };
    cmd.args(select);
    cmd
}

/// The full tier's test steps: every test of the workspace, the Qt crate's with the offscreen
/// platform, and the doc tests.
pub fn full(engine: Engine) -> Result<()> {
    let how = match engine {
        Engine::Nextest => "nextest",
        Engine::CargoTest => "cargo test",
    };
    crate::run(
        &format!("tests (non-Qt, {how})"),
        &mut test_command(engine, &["--workspace", "--exclude", "kourier-qt"]),
    )?;
    crate::run(
        &format!("tests (Qt, offscreen, {how})"),
        offscreen(&mut test_command(engine, &["-p", "kourier-qt"])),
    )?;
    if engine == Engine::Nextest {
        // `cargo test` ran them in the steps above.
        crate::run(
            "doc tests",
            crate::cargo().args(["test", "--workspace", "--doc"]),
        )?;
    }
    Ok(())
}

/// The quick tier's unit tests: the libraries and binaries of every package.
pub fn quick_unit(engine: Engine) -> Result<()> {
    crate::run(
        "unit tests of every package (non-Qt)",
        &mut test_command(
            engine,
            &["--workspace", "--exclude", "kourier-qt", "--lib", "--bins"],
        ),
    )?;
    crate::run(
        "unit tests of kourier-qt (offscreen)",
        offscreen(&mut test_command(
            engine,
            &["-p", "kourier-qt", "--lib", "--bins"],
        )),
    )
}

/// The arguments of one nextest run over the integration tests of `packages`: all of them in
/// one run, so one scheduler fills the cores with tests of different packages, and so the
/// features one of them asks of another (`kourier-desktop`'s `testing`, which `kourier-app`
/// turns on for its tests) are on, as they are in the full run. `--test '*'` is every
/// integration test target and nothing else: `--tests` would run the unit tests of these
/// packages a second time (`quick_unit` ran them). Every package passed must have at least
/// one such target (cargo refuses a pattern that matches none). A target whose
/// `required-features` are off makes cargo stop with "requires the features" (loud, never a
/// silent skip); that cannot happen today because `kourier-desktop` is never reached without
/// `kourier-app`, whose dev dependency turns the feature on (test
/// `the_feature_a_gated_test_needs_is_turned_on_by_a_dependent`).
pub fn integration_select(packages: &[&str]) -> Vec<String> {
    let mut args = Vec::new();
    for package in packages {
        args.push("-p".to_owned());
        args.push((*package).to_owned());
    }
    args.push("--test".to_owned());
    args.push("*".to_owned());
    args
}

/// The quick tier's integration tests of the reached packages `selected` (package, its test
/// targets): with nextest in one run for the non-Qt packages and one for the Qt crate; with
/// `cargo test` one package after another, each by its target names.
pub fn quick_integration(engine: Engine, selected: &[(String, Vec<String>)]) -> Result<()> {
    match engine {
        Engine::CargoTest => {
            for (package, tests) in selected {
                let mut cmd = crate::cargo();
                cmd.args(crate::quick::test_args(package, tests));
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
                let select = integration_select(&qt);
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
        assert_eq!(
            integration_select(&["kourier-app", "kourier-desktop"]),
            ["-p", "kourier-app", "-p", "kourier-desktop", "--test", "*"]
        );
        assert_eq!(
            integration_select(&["kourier"]),
            ["-p", "kourier", "--test", "*"],
            "every integration test target, not `--tests` (which adds the unit tests again)"
        );
    }

    #[test]
    fn the_feature_a_gated_test_needs_is_turned_on_by_a_dependent() {
        // `kourier-desktop`'s test targets `dbus` and `hints` need its feature `testing`. A
        // `-p` run gets it from `kourier-app`'s dev dependency, and `kourier-app` depends on
        // `kourier-desktop`, so the quick tier's walk over dependents reaches it whenever
        // `kourier-desktop` is reached. Remove the feature there and the quick tier stops
        // with cargo's "requires the features" the first time `kourier-desktop` changes.
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
}
