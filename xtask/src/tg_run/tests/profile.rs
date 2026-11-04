//! The profile (X10, wipe): where it may live, its names, its state, and what `wipe` deletes.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::preflight::World;
use super::{Scratch, args, error_of};
use crate::tg_run::profile::{
    Places, ProfileDir, State, Where, ensure, keeper_root, read_switches, unsafe_place,
    valid_profile_name, wipe_in,
};
use crate::tg_run::{API_FILE, KEEPER_DIR, REHEARSAL_DIR};

/// X10: a login never lives in a neighbour's place: the repository, a worktree, the build directory,
/// the app's own data directory and every other run tool's kept profiles, a base directory, a place
/// that already holds a normal profile. Mutant: the WhatsApp run tool's row dropped.
#[test]
fn a_login_never_lives_in_a_neighbours_place() {
    let scratch = Scratch::new("places");
    let home = scratch.0.join("home");
    let data = home.join(".local/share");
    let vars = |key: &str| match key {
        "HOME" => Some(home.to_str().expect("utf-8").to_owned()),
        "XDG_DATA_HOME" => Some(data.to_str().expect("utf-8").to_owned()),
        _ => None,
    };
    let worktrees = [PathBuf::from("/elsewhere/kourier-wt")];
    let places = Places {
        root: Path::new("/elsewhere/kourier"),
        target: Path::new("/elsewhere/kourier/target"),
        worktrees: &worktrees,
    };
    let refused = |dir: &Path| unsafe_place(dir, &vars, &places);
    // Its own keeper root is the place for a login.
    assert_eq!(refused(&data.join(KEEPER_DIR).join("r0")), None);
    for (what, dir) in [
        ("app's own data", data.join("kourier")),
        ("app's own data", data.join("kourier").join("x")),
        ("Signal smoke", data.join("kourier-smoke").join("default")),
        ("Signal smoke", data.join("kourier-smoke")),
        ("WhatsApp run", data.join("kourier-wa-run").join("default")),
        ("WhatsApp run", data.join("kourier-wa-run")),
        ("Threema run", data.join("kourier-threema-run").join("p")),
        ("repository", PathBuf::from("/elsewhere/kourier/x")),
        ("repository", PathBuf::from("/elsewhere/kourier/target/x")),
        ("worktree", PathBuf::from("/elsewhere/kourier-wt/sub")),
    ] {
        let why = refused(&dir).unwrap_or_else(|| panic!("{} is accepted", dir.display()));
        assert!(why.contains(what), "{}: {why}", dir.display());
        assert!(
            !why.contains(scratch.0.to_str().expect("utf-8")),
            "a path: {why}"
        );
    }
    for base in [
        &data,
        &home,
        &home.join(".config"),
        &home.join(".cache"),
        &home.join(".local/state"),
    ] {
        assert!(
            refused(base).is_some_and(|w| w.contains("base directory")),
            "{}",
            base.display()
        );
    }
    // A directory that holds a normal profile.
    let normal = scratch.0.join("normal");
    std::fs::create_dir_all(normal.join("kourier/accounts")).expect("dirs");
    assert!(refused(&normal).is_some_and(|w| w.contains("normal Kourier profile")));
}

#[test]
fn a_profile_name_is_a_name_and_never_a_number() {
    let long = "a".repeat(32);
    for good in ["r0", "p", "test-1", "a_b", "x12345", long.as_str()] {
        assert!(valid_profile_name(good), "{good}");
    }
    let too_long = "a".repeat(33);
    for bad in [
        "",
        too_long.as_str(),
        "a/b",
        "..",
        "a b",
        "\u{e9}",
        "58301947",
        "r12345678",
        "p-1234567-x",
        "380501234567",
    ] {
        assert!(!valid_profile_name(bad), "{bad}");
    }
    // Six digits are a number of days, not a number.
    assert!(valid_profile_name("p123456"));
    let w = Where {
        profile: Some("58301947".to_owned()),
        rehearsal_place: None,
    };
    let error = error_of(w.resolve(&|_| None, Path::new("/t")));
    assert!(
        error.contains("invalid profile name") && !error.contains("58301947"),
        "{error}"
    );
}

/// H-6: no profile is named after the API file, and a name that stands in the keeper root as anything
/// but a directory (a file, a link) is no profile. `resolve` refuses both in fixed sentences that echo
/// no name, and writes nothing; `wipe` refuses the API file's name before it looks at or writes
/// anything. The API file and the keeper root are the same after all of it. Mutants: the API file's
/// name allowed; the non-directory check of `resolve` removed; `wipe`'s name check removed.
#[test]
fn no_profile_is_named_after_the_api_file_or_a_file_of_the_keeper_root() {
    let w = World::new("profile-api-name");
    let api_before = std::fs::read(w.api_file()).expect("the api file");
    let data = w.data().to_str().expect("utf-8").to_owned();
    let vars = |k: &str| (k == "XDG_DATA_HOME").then(|| data.clone());
    let listing = || {
        let mut names: Vec<String> = std::fs::read_dir(w.keeper())
            .expect("keeper")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let resolve = |name: &str| {
        Where {
            profile: Some(name.to_owned()),
            rehearsal_place: None,
        }
        .resolve(&vars, &w.target())
    };
    // The API file's own name, as a profile.
    assert!(!valid_profile_name(API_FILE));
    let error = error_of(resolve(API_FILE));
    assert!(
        error.contains("invalid profile name") && error.contains("never the API file's name"),
        "{error}"
    );
    // A regular file of that name, and a link (even to a directory), are no profiles; a directory is.
    std::fs::write(w.keeper().join("x"), "a file").expect("file");
    std::fs::create_dir(w.keeper().join("real")).expect("dir");
    std::os::unix::fs::symlink(w.keeper().join("real"), w.keeper().join("y")).expect("link");
    for name in ["x", "y"] {
        let error = error_of(resolve(name));
        assert!(
            error.contains("a file of that name stands in the keeper root")
                && error.contains("Nothing was started")
                && !error.contains(w.scratch.0.to_str().expect("utf-8")),
            "{name}: {error}"
        );
    }
    assert_eq!(
        resolve("real").expect("a directory is a profile").dir,
        w.keeper().join("real")
    );
    assert_eq!(
        resolve("new").expect("a free name is a profile").dir,
        w.keeper().join("new")
    );
    // Nothing was made by any of it.
    assert_eq!(listing(), ["real", "telegram-api", "x", "y"]);
    // `wipe` over the API file's name: refused before anything is looked at or written.
    let api = ProfileDir {
        dir: w.api_file(),
        rehearsal: false,
    };
    for yes in [false, true] {
        let error = error_of(wipe_in(&api, yes, Duration::ZERO));
        assert!(
            error.contains("not a profile name") && error.contains("nothing is deleted"),
            "{error}"
        );
    }
    assert_eq!(std::fs::read(w.api_file()).expect("api file"), api_before);
    assert_eq!(listing(), ["real", "telegram-api", "x", "y"]);
}

#[test]
fn the_keeper_root_follows_the_apps_rule() {
    let with = |xdg: Option<&str>, home: Option<&str>| {
        let (xdg, home) = (xdg.map(str::to_owned), home.map(str::to_owned));
        keeper_root(&move |k| match k {
            "XDG_DATA_HOME" => xdg.clone(),
            "HOME" => home.clone(),
            _ => None,
        })
    };
    assert_eq!(
        with(Some("/x"), Some("/h")).expect("root"),
        Path::new("/x").join(KEEPER_DIR)
    );
    assert_eq!(
        with(None, Some("/h")).expect("root"),
        Path::new("/h/.local/share").join(KEEPER_DIR)
    );
    // A relative or empty value is ignored, as the app does.
    assert_eq!(
        with(Some("rel"), Some("/h")).expect("root"),
        Path::new("/h/.local/share").join(KEEPER_DIR)
    );
    assert_eq!(
        with(Some(""), Some("/h")).expect("root"),
        Path::new("/h/.local/share").join(KEEPER_DIR)
    );
    assert!(with(None, None).is_err());
    assert!(with(Some("rel"), Some("rel")).is_err());
}

/// The default profile is `r0`, in the keeper root, and a kept profile is never a rehearsal.
#[test]
fn the_default_profile_is_r0() {
    let vars = |k: &str| (k == "XDG_DATA_HOME").then(|| "/d".to_owned());
    let dir = Where::default()
        .resolve(&vars, Path::new("/t"))
        .expect("resolves");
    assert_eq!(dir.dir, Path::new("/d").join(KEEPER_DIR).join("r0"));
    assert!(!dir.rehearsal && dir.label() == "r0");
    assert_eq!(dir.keeper(), Path::new("/d").join(KEEPER_DIR));
}

/// The rehearsal's place is a profile directory below `<target>/tgr/`, inside a keeper root of the
/// tool's own name: a kept profile, `..`, the base itself, another depth of directory and a bad
/// name are refused.
#[test]
fn a_rehearsal_place_is_a_profile_below_the_rehearsal_base() {
    let target = Path::new("/co/target");
    let ok = target
        .join(REHEARSAL_DIR)
        .join("1761700000/x")
        .join(KEEPER_DIR)
        .join("p");
    let resolve = |p: &Path| {
        Where {
            profile: None,
            rehearsal_place: Some(p.to_owned()),
        }
        .resolve(&|_| None, target)
    };
    let dir = resolve(&ok).expect("resolves");
    assert!(dir.rehearsal && dir.dir == ok);
    assert_eq!(dir.keeper(), ok.parent().expect("parent"));
    for bad in [
        target.join(REHEARSAL_DIR),
        target.join(REHEARSAL_DIR).join("1/x").join(KEEPER_DIR),
        target.join("elsewhere/x").join(KEEPER_DIR).join("p"),
        target
            .join(REHEARSAL_DIR)
            .join("1/x")
            .join("other")
            .join("p"),
        target
            .join(REHEARSAL_DIR)
            .join("1/x/../../..")
            .join(KEEPER_DIR)
            .join("p"),
        target
            .join(REHEARSAL_DIR)
            .join("1/x")
            .join(KEEPER_DIR)
            .join("58301947"),
        PathBuf::from("/home/someone/.local/share")
            .join(KEEPER_DIR)
            .join("r0"),
    ] {
        let error = error_of(resolve(&bad));
        assert!(
            error.contains("--rehearsal-place must be"),
            "{}: {error}",
            bad.display()
        );
    }
    // Both a profile and a place is one too many.
    let both = Where {
        profile: Some("r0".to_owned()),
        rehearsal_place: Some(ok),
    };
    assert!(error_of(both.resolve(&|_| None, target)).contains("give one of"));
}

#[test]
fn the_state_file_is_tg_runs_and_private() {
    let w = World::new("state");
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    assert!(State::load(&dir).expect("no file").is_none());
    let state = State::new("test");
    ensure(&dir, &state).expect("ensure");
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&dir.dir), 0o700);
    assert_eq!(mode(&dir.tool()), 0o700);
    assert_eq!(mode(&dir.state_file()), 0o600);
    assert_eq!(mode(&w.keeper()), 0o700);
    assert_eq!(State::load(&dir).expect("loads"), Some(state.clone()));
    let text = std::fs::read_to_string(dir.state_file()).expect("text");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        json,
        serde_json::json!({"version": 1, "env": "test", "connects": []})
    );
    // The tool's files are where the app and the runbook expect them.
    assert_eq!(
        dir.config_file(),
        dir.dir.join("kourier/config/kourier.toml")
    );
    assert_eq!(dir.lock_file(), dir.dir.join("tg-run/lock"));
    // `ensure` tightens a keeper root that was too open (HIS API file's directory).
    std::fs::set_permissions(w.keeper(), std::fs::Permissions::from_mode(0o755)).expect("mode");
    ensure(&dir, &state).expect("ensure");
    assert_eq!(mode(&w.keeper()), 0o700);
}

#[test]
fn the_switches_of_a_kourier_toml_are_read_as_written() {
    type Case = (
        &'static str,
        Option<&'static [&'static str]>,
        Option<&'static str>,
    );
    let cases: [Case; 7] = [
        ("", None, None),
        ("networks-on = [\"Telegram\"]\n", Some(&["Telegram"]), None),
        (
            "networks-on=['Telegram','WhatsApp']\n",
            Some(&["Telegram", "WhatsApp"]),
            None,
        ),
        (
            "networks-on = []\npending-link = \"Telegram\"\n",
            Some(&[]),
            Some("Telegram"),
        ),
        ("pending-link=\"WhatsApp\"\n", None, Some("WhatsApp")),
        ("networks-on = Telegram\n", Some(&[]), None),
        ("# networks-on = [\"Telegram\"]\ntheme = 1\n", None, None),
    ];
    for (text, names, pending) in cases {
        let (got_names, got_pending) = read_switches(text);
        let want_names = names.map(|n| n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(got_names, want_names, "{text:?}");
        assert_eq!(got_pending.as_deref(), pending, "{text:?}");
    }
}

// ---- wipe ----------------------------------------------------------------------------------------------

/// Wipe deletes the profile directory and nothing else, and only with its flag. Without it nothing
/// is deleted and it says so; with it HIS API file, the keeper root and the other profiles stay; a
/// directory that is no profile of this tool, a link and a profile in use are never deleted.
#[test]
fn wipe_deletes_the_profile_and_only_with_its_flag() {
    let w = World::new("wipe");
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    ensure(&dir, &State::new("test")).expect("ensure");
    std::fs::write(dir.dir.join("kourier-data"), "x").expect("data");
    let sibling = w.keeper().join("other");
    std::fs::create_dir_all(&sibling).expect("sibling");
    // Without the flag: nothing is deleted, in words, status 1.
    let (lines, status) = wipe_in(&dir, false, Duration::ZERO).expect("wipe");
    assert_eq!(status, 1);
    let text = lines.join("\n");
    assert!(
        text.contains("Nothing was deleted") && text.contains("--yes-delete-the-login"),
        "{text}"
    );
    assert!(
        !text.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path: {text}"
    );
    assert!(dir.dir.join("kourier-data").exists() && dir.state_file().exists());
    // With it: the profile is gone; the keeper root, HIS file and the sibling are not.
    let (lines, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(lines.last().is_some_and(|l| l == "deleted."), "{lines:?}");
    assert!(!dir.dir.exists());
    assert!(w.api_file().exists() && w.keeper().exists() && sibling.exists());
    // A profile that is not there is an error, not a success.
    assert!(error_of(wipe_in(&dir, true, Duration::ZERO)).contains("nothing is deleted"));
}

#[test]
fn wipe_never_deletes_a_directory_that_is_no_profile_of_this_tool() {
    let w = World::new("wipe-foreign");
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    // No state file.
    std::fs::create_dir_all(dir.dir.join("kourier")).expect("data");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("no state file") && error.contains("nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.join("kourier").exists());
    // A state file that is not the tool's.
    std::fs::create_dir_all(dir.tool()).expect("tool dir");
    std::fs::write(dir.state_file(), "{\"version\":2}").expect("state");
    assert!(wipe_in(&dir, true, Duration::ZERO).is_err());
    assert!(dir.dir.exists());
    // A link to a profile: not followed, not deleted, the target stays.
    let real = w.keeper().join("real");
    ensure(
        &ProfileDir {
            dir: real.clone(),
            rehearsal: false,
        },
        &State::new("test"),
    )
    .expect("ensure");
    let link = ProfileDir {
        dir: w.keeper().join("linked"),
        rehearsal: false,
    };
    std::os::unix::fs::symlink(&real, &link.dir).expect("link");
    assert!(error_of(wipe_in(&link, true, Duration::ZERO)).contains("nothing is deleted"));
    assert!(real.join("tg-run/state.json").exists());
}

#[test]
fn wipe_waits_for_nothing_and_refuses_a_profile_in_use() {
    let w = World::new("wipe-busy");
    let dir = ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    };
    ensure(&dir, &State::new("test")).expect("ensure");
    // A connect holds the profile's lock for as long as it runs.
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.lock_file())
        .expect("lock file");
    held.try_lock().expect("the test takes the lock");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("a connect is running") && error.contains("nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.exists());
    drop(held);
    wipe_in(&dir, true, Duration::ZERO).expect("free now");
    assert!(!dir.dir.exists());
}

/// `wipe` is a command line like the others: its flag is parsed, its profile resolved, and a
/// rehearsal's place can be wiped too (the rehearsal's check 7).
#[test]
fn wipe_of_a_rehearsal_place_leaves_the_keeper_root() {
    let w = World::new("wipe-place");
    let line = args(&[
        "wipe",
        "--rehearsal-place",
        w.target()
            .join(REHEARSAL_DIR)
            .join("1/x")
            .join(KEEPER_DIR)
            .join("p")
            .to_str()
            .expect("utf-8"),
        "--yes-delete-the-login",
    ])
    .expect("parses");
    let dir = line
        .place
        .resolve(&|_| None, &w.target())
        .expect("resolves");
    ensure(&dir, &State::new("test")).expect("ensure");
    let keeper = dir.keeper().to_owned();
    std::fs::write(keeper.join("telegram-api"), "synthetic").expect("file");
    let (_, status) = wipe_in(&dir, line.yes_delete, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(!dir.dir.exists() && keeper.join("telegram-api").exists());
}
