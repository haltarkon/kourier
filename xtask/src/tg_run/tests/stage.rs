//! Staging (X12 and the rest of the stage): the stamped copies, the build command, where the app finds
//! its helper and library, the switch file and the probe.

use std::cell::Cell;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::preflight::HEAD;
use super::{POLL, Scratch, StepClock, error_of, executable, mode_of, profile_in};
use crate::tg_run::preflight::{check_switches, sha256_bytes};
use crate::tg_run::profile::{ProfileDir, private_dir, read_switches};
use crate::tg_run::stage::{
    CheckedLibrary, LIBRARY_MODE, LIBRARY_NOT_CHECKED, PROGRAM_MODE, Sources, StageSeams, Wait,
    build_args, build_command, check_stamped, judge_probe, probe, probe_command, stage,
    stamp_copies, switch_text, switches_set, write_switch,
};

/// A synthetic library name of the shape the stage accepts (`libtdjson.so.<version>`); not the pin's, so a TDLib
/// bump edits nothing here (the pin's own soname is read from the pin file in `tests/library.rs`).
const SONAME: &str = "libtdjson.so.1.8.0";

/// Build outputs of synthetic bytes below `scratch/build`.
struct Outputs {
    app: PathBuf,
    helper: PathBuf,
    /// The library as the preflight checked it: the file and the sum of its bytes.
    library: CheckedLibrary,
}

fn outputs(scratch: &Scratch) -> Outputs {
    let dir = scratch.0.join("build");
    std::fs::create_dir_all(&dir).expect("build dir");
    let write = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("an output");
        path
    };
    Outputs {
        app: write("kourier", b"app v1"),
        helper: write("kourier-tdlib", b"helper v1"),
        library: CheckedLibrary {
            path: write(SONAME, b"library v1"),
            sha256: sha256_bytes(b"library v1").expect("sum"),
        },
    }
}

fn sources(o: &Outputs) -> Sources<'_> {
    Sources {
        app: &o.app,
        helper: Some(&o.helper),
        library: Some(&o.library),
    }
}

/// Changes the bytes of an execute-only or read-only staged file in place, the way something that is
/// not this tool would, and puts its mode back.
fn tamper(path: &Path, bytes: &[u8]) {
    let mode = mode_of(path);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    std::fs::write(path, bytes).expect("tamper");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
}

/// X12: the copies are stamped and private: the programs execute-only (0100), the library read-only
/// (0400), the sums the stamp holds are the files', `--no-build` takes only a stamp at HEAD whose files
/// are what it says. Mutant: the library's mode 0444.
#[test]
fn the_copies_are_stamped_and_private() {
    let scratch = Scratch::new("stamp");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    let stamp = stamp_copies(&dir, &sources(&o), Some(HEAD), 1_000).expect("stamped");
    assert_eq!(mode_of(&dir.app_copy()), PROGRAM_MODE);
    assert_eq!(mode_of(&dir.helper_copy()), 0o100);
    assert_eq!(mode_of(&dir.tdlib_dir().join(SONAME)), 0o400);
    assert_eq!(LIBRARY_MODE, 0o400);
    for private in [dir.bin_dir(), dir.tdlib_dir()] {
        assert_eq!(mode_of(&private), 0o700, "{}", private.display());
    }
    assert_eq!(mode_of(&dir.stamp_file()), 0o600);
    // No half-made copy stays behind.
    for leftover in ["kourier.new", "kourier-tdlib.new"] {
        assert!(!dir.bin_dir().join(leftover).exists());
    }
    assert!(!dir.tdlib_dir().join(format!("{SONAME}.new")).exists());
    // The stamp holds HEAD and the three sums, and they are the sums of the bytes.
    assert_eq!(stamp.commit.as_deref(), Some(HEAD));
    assert_eq!(stamp.app_sha256, sha256_bytes(b"app v1").expect("sum"));
    assert_eq!(
        stamp.helper_sha256,
        Some(sha256_bytes(b"helper v1").expect("sum"))
    );
    assert_eq!(
        stamp.library,
        Some((SONAME.to_owned(), sha256_bytes(b"library v1").expect("sum")))
    );
    let text = std::fs::read_to_string(dir.stamp_file()).expect("the stamp");
    for needle in [HEAD, stamp.app_sha256.as_str()] {
        assert!(text.contains(needle), "{text}");
    }
    // The tool's check of the files leaves their modes as they were, also when it fails.
    assert_eq!(check_stamped(&dir, Some(HEAD)).expect("intact"), stamp);
    assert_eq!(mode_of(&dir.app_copy()), 0o100);
    assert_eq!(mode_of(&dir.helper_copy()), 0o100);

    // `--no-build` needs a stamp at the commit it runs at; a rehearsal's (none) is another thing.
    let other = "0".repeat(40);
    assert!(error_of(check_stamped(&dir, Some(&other))).contains("built at"));
    assert!(error_of(check_stamped(&dir, None)).contains("no commit"));

    // Each file against its sum.
    let sums_differ = "its sum differs";
    tamper(&dir.app_copy(), b"app v2");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains(sums_differ));
    assert_eq!(mode_of(&dir.app_copy()), 0o100, "restored after a failure");
    tamper(&dir.app_copy(), b"app v1");
    assert!(check_stamped(&dir, Some(HEAD)).is_ok());
    tamper(&dir.helper_copy(), b"helper v2");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains(sums_differ));
    assert_eq!(mode_of(&dir.helper_copy()), 0o100);
    tamper(&dir.helper_copy(), b"helper v1");
    tamper(&dir.tdlib_dir().join(SONAME), b"library v2");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains(sums_differ));
    tamper(&dir.tdlib_dir().join(SONAME), b"library v1");
    assert!(check_stamped(&dir, Some(HEAD)).is_ok());

    // A mode that is not the stamped one, a link, a missing file, a missing or damaged stamp.
    std::fs::set_permissions(dir.app_copy(), std::fs::Permissions::from_mode(0o755)).expect("mode");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("mode 755, not 100"));
    std::fs::set_permissions(dir.app_copy(), std::fs::Permissions::from_mode(0o100)).expect("mode");
    let lib = dir.tdlib_dir().join(SONAME);
    std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o444)).expect("mode");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("mode 444, not 400"));
    std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o400)).expect("mode");
    let helper = dir.helper_copy();
    let moved = dir.bin_dir().join("moved");
    std::fs::rename(&helper, &moved).expect("move");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("missing"));
    std::os::unix::fs::symlink(&moved, &helper).expect("link");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("not a regular file"));
    std::fs::remove_file(&helper).expect("unlink");
    std::fs::rename(&moved, &helper).expect("back");
    assert!(check_stamped(&dir, Some(HEAD)).is_ok());
    let good = std::fs::read_to_string(dir.stamp_file()).expect("stamp");
    std::fs::write(dir.stamp_file(), "not json").expect("damage");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no stamped copies"));
    // A stamp that names a library outside the tdlib directory is not this tool's.
    std::fs::write(
        dir.stamp_file(),
        good.replace(SONAME, "../../elsewhere/libtdjson.so.1"),
    )
    .expect("edit");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no stamped copies"));
    // A real stamp without its helper is not a stamp.
    std::fs::write(dir.stamp_file(), good.replace("helper_sha256", "x_sha256")).expect("edit");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no stamped copies"));
    std::fs::remove_file(dir.stamp_file()).expect("remove");
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no stamped copies"));
}

/// The rehearsal stages the app alone (the fake replaces the backend: no helper, no library) and its
/// stamp has no commit: the library and API items are skipped in a rehearsal, and nothing of them is
/// asked for or made.
#[test]
fn a_rehearsal_stages_the_app_alone() {
    let scratch = Scratch::new("stamp-rehearsal");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    let only_app = Sources {
        app: &o.app,
        helper: None,
        library: None,
    };
    let stamp = stamp_copies(&dir, &only_app, None, 5).expect("stamped");
    assert!(stamp.commit.is_none() && stamp.helper_sha256.is_none() && stamp.library.is_none());
    assert!(dir.app_copy().exists());
    assert!(!dir.helper_copy().exists() && !dir.tdlib_dir().exists());
    assert!(check_stamped(&dir, None).is_ok());
    // A real run's expectation of a rehearsal's copies is refused, and the other way round.
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no commit"));
    // A stage mixes neither: a real one needs the helper and the library, a rehearsal's has none.
    assert!(error_of(stamp_copies(&dir, &only_app, Some(HEAD), 5)).contains("a real stage has"));
    assert!(error_of(stamp_copies(&dir, &sources(&o), None, 5)).contains("a real stage has"));
    // A library that is no library of the pin's name is not staged.
    let wrong = scratch.0.join("build").join("libother.so");
    std::fs::write(&wrong, b"x").expect("file");
    let wrong = CheckedLibrary {
        path: wrong,
        sha256: sha256_bytes(b"x").expect("sum"),
    };
    let bad = Sources {
        app: &o.app,
        helper: Some(&o.helper),
        library: Some(&wrong),
    };
    assert!(error_of(stamp_copies(&dir, &bad, Some(HEAD), 5)).contains("libtdjson.so.<version>"));
}

/// `stage`: builds (the rehearsal's build for a rehearsal), checks that main did not move while it
/// built, stamps; `--no-build` builds nothing and reuses only the profile's own stamped copies.
#[test]
fn the_stage_builds_once_checks_main_and_reuses_only_its_own_copies() {
    let scratch = Scratch::new("stage");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    let builds = Cell::new(Vec::new());
    let build = |rehearsal: bool| {
        let mut b = builds.take();
        b.push(rehearsal);
        builds.set(b);
        Ok(())
    };
    let seams = |head_now: &'static dyn Fn() -> anyhow::Result<String>| StageSeams {
        build: &build,
        app: o.app.clone(),
        helper: o.helper.clone(),
        library: Some(o.library.clone()),
        head_now,
        now_ms: 7,
    };
    // `--no-build` with nothing staged: refused, nothing built.
    let no_head: &'static dyn Fn() -> anyhow::Result<String> = &|| Ok(HEAD.to_owned());
    let err = error_of(stage(&dir, Some(HEAD), true, &seams(no_head)));
    assert!(err.contains("no stamped copies"), "{err}");
    assert!(builds.take().is_empty());
    // A build, then a stamp at HEAD.
    stage(&dir, Some(HEAD), false, &seams(no_head)).expect("staged");
    assert_eq!(builds.take(), [false]);
    // `--no-build` now reuses them and builds nothing.
    stage(&dir, Some(HEAD), true, &seams(no_head)).expect("reused");
    assert!(builds.take().is_empty());
    // Main moved while the app was built: nothing is stamped, and the old stamp is gone.
    let moved_head: &'static dyn Fn() -> anyhow::Result<String> = &|| Ok("f".repeat(40));
    let err = error_of(stage(&dir, Some(HEAD), false, &seams(moved_head)));
    assert!(err.contains("main moved while the app was built"), "{err}");
    assert_eq!(builds.take(), [false]);
    // A rehearsal's stage asks for the rehearsal's build.
    let scratch2 = Scratch::new("stage2");
    let dir2 = profile_in(&scratch2);
    stage(&dir2, None, false, &seams(no_head)).expect("rehearsal staged");
    assert_eq!(builds.take(), [true]);
    assert!(!dir2.helper_copy().exists());
}

/// The seams of a stage over the outputs `o`, with `library` as the preflight checked it.
fn seams_of<'a>(
    o: &Outputs,
    build: &'a dyn Fn(bool) -> anyhow::Result<()>,
    library: Option<CheckedLibrary>,
    head_now: &'a dyn Fn() -> anyhow::Result<String>,
) -> StageSeams<'a> {
    StageSeams {
        build,
        app: o.app.clone(),
        helper: o.helper.clone(),
        library,
        head_now,
        now_ms: 7,
    }
}

/// H-3 (build run): the preflight checked a library and the build ran for minutes; the library that is
/// copied now must be that one. Another session's `srv -f target/tdlib` (an unpatched or test-only
/// build) replaces the file meanwhile: the stage refuses in the one sentence, stamps nothing and leaves
/// no copy of the file that was not checked. Mutant: the compare in `stamp_copies` dropped.
#[test]
fn a_library_that_changed_after_its_check_is_not_staged() {
    let scratch = Scratch::new("stage-swapped");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    let head_now = || Ok(HEAD.to_owned());
    let quiet = |_: bool| Ok(());
    // The control: the file is what was checked, and the stage is made.
    let control = seams_of(&o, &quiet, Some(o.library.clone()), &head_now);
    stage(&dir, Some(HEAD), false, &control).expect("a checked library is staged");
    assert!(dir.stamp_file().exists() && dir.tdlib_dir().join(SONAME).exists());
    // The swap, while the build runs: refused, no stamp, no unchecked copy.
    let swap = |_: bool| {
        std::fs::write(&o.library.path, b"library v2").expect("another build wrote the file");
        Ok(())
    };
    let swapped = seams_of(&o, &swap, Some(o.library.clone()), &head_now);
    let err = error_of(stage(&dir, Some(HEAD), false, &swapped));
    assert!(err.contains(LIBRARY_NOT_CHECKED), "{err}");
    assert!(err.contains("Nothing was stamped"), "{err}");
    assert!(
        !dir.stamp_file().exists(),
        "a stamp stands for copies that were not checked"
    );
    assert!(
        !dir.tdlib_dir().join(SONAME).exists(),
        "the unchecked copy stays"
    );
    assert!(error_of(check_stamped(&dir, Some(HEAD))).contains("no stamped copies"));
}

/// H-3 (`--no-build`): the copies a real run reuses hold the library the preflight has JUST checked, or
/// the run is refused in the same sentence: a stamp whose library sum differs from the fresh check's
/// (the library was fetched again since), and a real run with no checked library at all. A rehearsal
/// has no library and asks for none. Mutant: the compare in `stage`'s `--no-build` arm dropped.
#[test]
fn a_reused_stage_must_hold_the_library_that_was_just_checked() {
    let scratch = Scratch::new("stage-reuse");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    let head_now = || Ok(HEAD.to_owned());
    let quiet = |_: bool| Ok(());
    let with = |library: Option<CheckedLibrary>| seams_of(&o, &quiet, library, &head_now);
    stage(&dir, Some(HEAD), false, &with(Some(o.library.clone()))).expect("staged");
    // The library that was staged is the one just checked: reused.
    stage(&dir, Some(HEAD), true, &with(Some(o.library.clone()))).expect("reused");
    // A fresh check that names another sum.
    let other = CheckedLibrary {
        path: o.library.path.clone(),
        sha256: sha256_bytes(b"library v2").expect("sum"),
    };
    let err = error_of(stage(&dir, Some(HEAD), true, &with(Some(other))));
    assert!(err.contains(LIBRARY_NOT_CHECKED), "{err}");
    assert!(err.contains("Nothing was started"), "{err}");
    // No checked library to compare with: the same refusal.
    let err = error_of(stage(&dir, Some(HEAD), true, &with(None)));
    assert!(err.contains(LIBRARY_NOT_CHECKED), "{err}");
    // The stamp is as it was after all of it, and a rehearsal's reuse asks for no library.
    assert!(check_stamped(&dir, Some(HEAD)).is_ok());
    let scratch2 = Scratch::new("stage-reuse-rehearsal");
    let dir2 = profile_in(&scratch2);
    stage(&dir2, None, false, &with(None)).expect("a rehearsal is staged");
    stage(&dir2, None, true, &with(None)).expect("and reused");
}

/// B-1: the staged app is ALWAYS the tool's own `cargo build -p kourier -p kourier-tdlib` (the
/// rehearsal's `-p kourier`): exactly those arguments, no `--tests`, `--all-targets`, `--workspace`,
/// feature flag or profile. The API variables are scrubbed by the shared cargo command.
#[test]
fn the_build_command_is_pinned() {
    assert_eq!(
        build_args(false),
        ["build", "-p", "kourier", "-p", "kourier-tdlib"]
    );
    assert_eq!(build_args(true), ["build", "-p", "kourier"]);
    for rehearsal in [false, true] {
        let cmd = build_command(rehearsal);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, build_args(rehearsal));
        for forbidden in [
            "--tests",
            "--all-targets",
            "--workspace",
            "--all",
            "--release",
            "--features",
            "--no-default-features",
            "--all-features",
            "--lib",
            "--test",
        ] {
            assert!(!args.iter().any(|a| a == forbidden), "{forbidden}");
        }
        for name in ["KOURIER_TG_API_ID", "KOURIER_TG_API_HASH"] {
            let removed = cmd
                .get_envs()
                .any(|(key, value)| key == name && value.is_none());
            assert!(removed, "{name} is not scrubbed from the build");
        }
    }
}

/// The app finds the helper and the library BESIDE the stamped copy: `<exe dir>/kourier-tdlib` and
/// `<exe dir>/../tdlib/<soname>` (`kourier-app/src/app.rs` `telegram_helper_files`). The rule is run
/// over the staged tree, and its text in the app is pinned so a change of the app's rule is noticed
/// here.
#[test]
fn the_app_finds_the_helper_and_the_library_beside_the_stamped_copy() {
    let scratch = Scratch::new("beside");
    let dir = profile_in(&scratch);
    let o = outputs(&scratch);
    stamp_copies(&dir, &sources(&o), Some(HEAD), 1).expect("stamped");
    // The app's rule, as it reads: `dir` = the directory of the executable.
    let exe = dir.app_copy();
    let exe_dir = exe.parent().expect("a directory");
    let beside = exe_dir.join("kourier-tdlib");
    let built = exe_dir.join("../tdlib").join(SONAME);
    assert!(beside.is_file(), "the helper is not beside the app");
    assert!(built.is_file(), "the library is not at <exe dir>/../tdlib");
    // The files are those the tool staged, at the places the stamp names.
    assert_eq!(
        std::fs::canonicalize(&built).expect("canonical"),
        std::fs::canonicalize(dir.tdlib_dir().join(SONAME)).expect("canonical")
    );
    let app = std::fs::read_to_string(crate::root().join("crates/kourier-app/src/app.rs"))
        .expect("the app's source");
    for rule in [
        "let beside = dir.join(\"kourier-tdlib\");",
        "let built = dir.join(\"../tdlib\").join(&name);",
        "let name = format!(\"libtdjson.so.{}\", kourier_telegram::PIN.version);",
    ] {
        assert!(
            app.contains(rule),
            "the app's rule for the helper files changed: {rule}"
        );
    }
}

// ---- The switch file --------------------------------------------------------------------------------

const EXISTING: &str = "# Kourier preferences (written by the app).\ncompact = true\nnetworks-on = [\"Signal\",\n  \"WhatsApp\"]\npending-link = \"WhatsApp\"\nround-avatars = false\n[extra]\nnetworks-on-ish = 1\n";

/// D-8: the switch file carries `networks-on = ["Telegram"]` and the one-shot `pending-link`, written
/// before EVERY start (the app clears the second at its start), keeping every other key and the
/// tables.
#[test]
fn the_switch_file_keeps_other_keys_and_is_written_before_every_start() {
    // From nothing.
    let fresh = switch_text(None);
    assert_eq!(
        fresh,
        "networks-on = [\"Telegram\"]\npending-link = \"Telegram\"\n"
    );
    let (names, pending) = read_switches(&fresh);
    assert_eq!(names.as_deref(), Some(&["Telegram".to_owned()][..]));
    assert_eq!(pending.as_deref(), Some("Telegram"));
    // Over a file with other keys, an old switch (a two-line array) and a table.
    let text = switch_text(Some(EXISTING));
    assert!(text.starts_with(&fresh), "{text}");
    for kept in [
        "# Kourier preferences (written by the app).",
        "compact = true",
        "round-avatars = false",
        "[extra]",
        "networks-on-ish = 1",
    ] {
        assert!(text.lines().any(|l| l == kept), "{kept} is gone: {text}");
    }
    for dropped in ["Signal", "WhatsApp", "  \"WhatsApp\"]"] {
        assert!(!text.contains(dropped), "{dropped} stayed: {text}");
    }
    assert_eq!(text.matches("networks-on = ").count(), 1);
    assert_eq!(text.matches("pending-link").count(), 1);
    // Both keys stand before the first table, where a key belongs to the file and not to a table.
    let table_at = text.find("[extra]").expect("a table");
    assert!(text.find("pending-link").expect("key") < table_at);
    // Writing it again changes nothing.
    assert_eq!(switch_text(Some(&text)), text);
    // The preflight's reading of the same text accepts it.
    check_switches(Some(&text)).expect("accepted");

    // Written to the profile (0600), and written AGAIN after the app cleared the one-shot.
    let scratch = Scratch::new("switch");
    let dir = profile_in(&scratch);
    assert!(!switches_set(&dir));
    write_switch(&dir).expect("written");
    assert!(switches_set(&dir));
    assert_eq!(mode_of(&dir.config_file()), 0o600);
    let kept = format!(
        "{}extra = 1\n",
        std::fs::read_to_string(dir.config_file()).expect("file")
    );
    std::fs::write(
        dir.config_file(),
        kept.replace("pending-link = \"Telegram\"\n", ""),
    )
    .expect("the app's start clears it");
    assert!(
        !switches_set(&dir),
        "a cleared one-shot is not a set switch"
    );
    write_switch(&dir).expect("written again");
    assert!(switches_set(&dir));
    let now = std::fs::read_to_string(dir.config_file()).expect("file");
    assert!(now.contains("extra = 1"), "{now}");
}

// ---- The probe --------------------------------------------------------------------------------------

fn env_of(cmd: &std::process::Command, key: &str) -> Option<Option<OsString>> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.map(OsString::from))
}

/// B-2, X14 for the probe: the probe's command is the staged app over `--print-networks` with the
/// profile, the test servers and no other `KOURIER_*`; every inherited `KOURIER_*` (a dial variable
/// among them) is removed; only a rehearsal carries the fake's switch.
#[test]
fn the_probe_carries_no_dial_variable_and_no_other_kourier_variable() {
    let bin = Path::new("/staged/kourier");
    let home = Path::new("/data/kourier-tg-run/p");
    let data = Path::new("/data");
    let inherited: Vec<OsString> = [
        "KOURIER_DEV_TELEGRAM_DIAL",
        "KOURIER_DEV_REHEARSAL",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_EXPECT_NUMBER_TAIL",
        "PATH",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let real = probe_command(bin, home, data, false, &inherited);
    assert_eq!(real.get_program(), bin.as_os_str());
    assert_eq!(real.get_args().collect::<Vec<_>>(), ["--print-networks"]);
    let set: Vec<String> = real
        .get_envs()
        .filter(|(k, v)| k.to_string_lossy().starts_with("KOURIER_") && v.is_some())
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        {
            let mut s = set.clone();
            s.sort();
            s
        },
        ["KOURIER_DEV_TELEGRAM_ENV", "KOURIER_HOME"]
    );
    assert_eq!(
        env_of(&real, "KOURIER_DEV_TELEGRAM_ENV"),
        Some(Some("test".into()))
    );
    assert_eq!(env_of(&real, "KOURIER_HOME"), Some(Some(home.into())));
    assert_eq!(env_of(&real, "XDG_DATA_HOME"), Some(Some(data.into())));
    for gone in [
        "KOURIER_DEV_TELEGRAM_DIAL",
        "KOURIER_DEV_REHEARSAL",
        "KOURIER_FAILPOINT",
        "KOURIER_DEV_EXPECT_NUMBER_TAIL",
    ] {
        assert_eq!(env_of(&real, gone), Some(None), "{gone} must be removed");
    }
    assert_eq!(env_of(&real, "PATH"), None, "PATH is left alone");
    // Silent and offscreen, like every start of the tool.
    assert_eq!(
        env_of(&real, "QT_QPA_PLATFORM"),
        Some(Some("offscreen".into()))
    );
    assert!(
        env_of(&real, "PULSE_SERVER")
            .flatten()
            .is_some_and(|v| v.to_string_lossy().contains("/nonexistent/"))
    );
    assert_eq!(env_of(&real, "PULSE_RUNTIME_PATH"), Some(None));
    // In a rehearsal the probe carries the fake's switch (critic 9) and still no dial variable.
    let rehearsal = probe_command(bin, home, data, true, &inherited);
    assert_eq!(
        env_of(&rehearsal, "KOURIER_DEV_REHEARSAL"),
        Some(Some("telegram".into()))
    );
    assert_eq!(env_of(&rehearsal, "KOURIER_DEV_TELEGRAM_DIAL"), Some(None));
}

/// The probe's answer must name Telegram in `registered=`; else it is refused with what the line said.
#[test]
fn the_probe_must_name_telegram_registered() {
    judge_probe("networks registered=Telegram off= on=Telegram\n").expect("registered");
    judge_probe("noise\nnetworks registered=Signal,Telegram off= on=Telegram\n").expect("a list");
    for line in [
        "networks registered=Signal off=Telegram on=",
        "networks registered= off= on=",
        "networks registered=TelegramX off= on=",
        "networks off=Telegram registered=Fake,Threema on=",
    ] {
        let err = error_of(judge_probe(line));
        assert!(err.contains("would not register Telegram"), "{err}");
        assert!(
            err.contains(line.trim()),
            "the refusal says what the line said: {err}"
        );
    }
    let err = error_of(judge_probe("something else\n"));
    assert!(err.contains("printed no `networks` line"), "{err}");
    assert!(error_of(judge_probe("")).contains("printed no `networks` line"));
}

/// The probe, run: a staged program that prints the line passes; one that fails says its status and
/// how many lines it wrote (the lines, redacted, are in `app.log`, never in the message); one that
/// does not end is killed at its bound on the clock.
#[test]
fn the_probe_runs_the_staged_app_bounded() {
    let scratch = Scratch::new("probe");
    let dir = profile_in(&scratch);
    let tools = scratch.0.join("tools");
    private_dir(&tools).expect("tools");
    let clock = StepClock::new(POLL);
    let home = dir.dir.clone();
    let data = scratch.0.join("data");
    // The bound counts idle polls (H-10): a probe that is slow but prints its line has 30 s of silence.
    let run_for = |program: &Path, bound: Duration| {
        let wait = Wait {
            bound,
            poll: POLL,
            clock: &clock,
        };
        probe(&dir, program, &home, &data, false, &[], &wait)
    };
    let run = |program: &Path| run_for(program, Duration::from_secs(30));
    let good = executable(
        &tools,
        "good",
        "#!/bin/sh\necho 'networks registered=Telegram off= on=Telegram'\n",
    );
    run(&good).expect("the probe passes");
    let wrong = executable(
        &tools,
        "wrong",
        "#!/bin/sh\necho 'networks registered=Signal off= on='\n",
    );
    assert!(error_of(run(&wrong)).contains("would not register Telegram"));
    let secret = ["tg:", &"7".repeat(10)].concat();
    let failing = executable(
        &tools,
        "failing",
        &format!("#!/bin/sh\necho 'boom {secret}' >&2\necho 'second' >&2\nexit 3\n"),
    );
    let err = error_of(run(&failing));
    assert!(
        err.contains("exit status 3") && err.contains("2 line(s)"),
        "{err}"
    );
    assert!(!err.contains("boom") && !err.contains("7777"), "{err}");
    let log = std::fs::read_to_string(dir.app_log()).expect("the app log");
    assert!(
        log.contains("boom tg:[digits]") && !log.contains("7777777"),
        "{log}"
    );
    assert_eq!(mode_of(&dir.app_log()), 0o600);
    // A probe that does not end: killed at its bound, on the clock (1 s of silence: ten idle polls).
    let hung = executable(&tools, "hung", "#!/bin/sh\nexec sleep 600\n");
    let err = error_of(run_for(&hung, Duration::from_secs(1)));
    assert!(err.contains("did not finish its probe in time"), "{err}");
}

/// Rehearsal places are below `target/tgr/` and a real profile is not: the stage never mixes them (the
/// directory arithmetic of the staged paths is the profile's, whichever kind it is).
#[test]
fn the_staged_paths_are_below_the_profiles_tool_directory() {
    let dir = ProfileDir {
        dir: PathBuf::from("/data/kourier-tg-run/r0"),
        rehearsal: false,
    };
    let base = Path::new("/data/kourier-tg-run/r0/tg-run");
    assert_eq!(dir.app_copy(), base.join("bin/kourier"));
    assert_eq!(dir.helper_copy(), base.join("bin/kourier-tdlib"));
    assert_eq!(dir.stamp_file(), base.join("bin/stamp.json"));
    assert_eq!(dir.tdlib_dir(), base.join("tdlib"));
    assert_eq!(dir.app_log(), base.join("app.log"));
    assert_eq!(dir.report_file(), base.join("report.md"));
    assert_eq!(dir.dial_file(), base.join("dial-go.json"));
}
