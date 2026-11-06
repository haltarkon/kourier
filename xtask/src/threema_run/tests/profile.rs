//! The profile: where it may live, its names, its state, the private files and what `wipe` deletes.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::{Scratch, World, args, error_of, id_shape, mode_of};
use crate::threema_run::profile::{
    Places, ProfileDir, State, Where, append_line, check_canonical, crypt_below, ensure,
    keeper_root, open_lock_file, private_dir, read_json, remove_synced, unsafe_place,
    valid_profile_name, wipe_in, write_private,
};
use crate::threema_run::{
    ALLOW_FILE, BACKUPS_DIR, DEFAULT_PROFILE, KEEPER_DIR, KEEPER_LOCK, OPEN_QUESTION_FILE,
    REHEARSAL_DIR, holds_a_digest,
};

fn profile_dir(w: &World) -> ProfileDir {
    ProfileDir {
        dir: w.profile(),
        rehearsal: false,
    }
}

/// A login never lives in a neighbour's place: the repository, a worktree, the build directory, the
/// projects tree, the app's own data directory and every other run tool's kept profiles, a base
/// directory, a place that already holds a normal profile. Its own keeper root is the place for it.
/// Mutants: a run tool's row dropped; the projects tree's row dropped.
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
    assert_eq!(refused(&data.join(KEEPER_DIR).join("rt1")), None);
    for (what, dir) in [
        ("app's own data", data.join("kourier")),
        ("app's own data", data.join("kourier").join("x")),
        ("Signal smoke", data.join("kourier-smoke").join("default")),
        ("WhatsApp run", data.join("kourier-wa-run").join("default")),
        ("WhatsApp run", data.join("kourier-wa-run")),
        ("Telegram run", data.join("kourier-tg-run").join("r0")),
        ("Telegram run", data.join("kourier-tg-run")),
        (
            "projects tree",
            home.join("projects").join("study").join("x"),
        ),
        ("projects tree", home.join("projects")),
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
    // Without a home directory the rows that need one are not made up.
    let nothing = |_: &str| None;
    assert_eq!(
        unsafe_place(Path::new("/x/y"), &nothing, &places),
        None,
        "nothing known, nothing refused"
    );
}

#[test]
fn a_profile_name_is_a_name_and_never_an_id_or_a_number() {
    let long = "a".repeat(32);
    for good in [
        "rt1",
        "p",
        "test-1",
        "a_b",
        "x12345",
        "rehearsal",
        long.as_str(),
    ] {
        assert!(valid_profile_name(good), "{good}");
    }
    let too_long = "a".repeat(33);
    let id = id_shape();
    let lower = id.to_lowercase();
    let gateway_shape = ["12345", "678"].concat();
    for bad in [
        "",
        too_long.as_str(),
        "a/b",
        "..",
        ".hidden",
        "a b",
        "\u{e9}",
        "58301947",
        "r12345678",
        "p-1234567-x",
        // The ID's shape, in either case, and eight digits.
        id.as_str(),
        lower.as_str(),
        gateway_shape.as_str(),
        // The list's file name and the backups' directory are reserved.
        ALLOW_FILE,
        BACKUPS_DIR,
    ] {
        assert!(!valid_profile_name(bad), "{bad}");
    }
    // Six digits are a number of days, not a number; seven letters are a word.
    assert!(valid_profile_name("p123456"));
    assert!(valid_profile_name("profile"));
    let w = Where {
        profile: Some(id.clone()),
        rehearsal_place: None,
    };
    let error = error_of(w.resolve(&|_| None, Path::new("/t")));
    assert!(
        error.contains("invalid profile name") && !error.contains(&id),
        "{error}"
    );
}

/// No profile is named after the list's file or `backups`, and a name that stands in the keeper root
/// as anything but a directory (a file, a link) is no profile. `resolve` refuses both in fixed
/// sentences that echo no name and writes nothing; `wipe` refuses the reserved names before it looks
/// at or writes anything. The list and the keeper root are the same after all of it. Mutants: a
/// reserved name allowed; the non-directory check of `resolve` removed; `wipe`'s name check removed.
#[test]
fn no_profile_is_named_after_the_list_or_a_file_of_the_keeper_root() {
    let w = World::new("profile-names");
    std::fs::write(w.allow_file(), "synthetic").expect("list");
    let list_before = std::fs::read(w.allow_file()).expect("the list");
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
    for reserved in [ALLOW_FILE, BACKUPS_DIR] {
        let error = error_of(resolve(reserved));
        assert!(
            error.contains("invalid profile name") && error.contains("never the list's file name"),
            "{reserved}: {error}"
        );
    }
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
    assert_eq!(listing(), ["real", ALLOW_FILE, "x", "y"]);
    // `wipe` over a reserved name: refused before anything is looked at or written.
    let list_as_profile = ProfileDir {
        dir: w.allow_file(),
        rehearsal: false,
    };
    std::fs::create_dir(w.keeper().join(BACKUPS_DIR)).expect("backups");
    std::fs::write(w.keeper().join(BACKUPS_DIR).join("b"), "x").expect("a backup");
    let backups_as_profile = ProfileDir {
        dir: w.keeper().join(BACKUPS_DIR),
        rehearsal: false,
    };
    for dir in [&list_as_profile, &backups_as_profile] {
        for yes in [false, true] {
            let error = error_of(wipe_in(dir, yes, Duration::ZERO));
            assert!(
                error.contains("not a profile name") && error.contains("nothing is deleted"),
                "{error}"
            );
        }
    }
    assert_eq!(std::fs::read(w.allow_file()).expect("list"), list_before);
    assert!(w.keeper().join(BACKUPS_DIR).join("b").exists());
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

/// The default profile is `rt1`, in the keeper root, and a kept profile is never a rehearsal; the
/// files the tool and the app expect are where the runbook says.
#[test]
fn the_default_profile_is_rt1_and_its_files_are_where_they_are_expected() {
    let vars = |k: &str| (k == "XDG_DATA_HOME").then(|| "/d".to_owned());
    let dir = Where::default()
        .resolve(&vars, Path::new("/t"))
        .expect("resolves");
    assert_eq!(DEFAULT_PROFILE, "rt1");
    assert_eq!(dir.dir, Path::new("/d").join(KEEPER_DIR).join("rt1"));
    assert!(!dir.rehearsal && dir.label() == "rt1");
    assert_eq!(dir.keeper(), Path::new("/d").join(KEEPER_DIR));
    assert_eq!(
        dir.config_file(),
        dir.dir.join("kourier/config/kourier.toml")
    );
    assert_eq!(dir.lock_file(), dir.dir.join("threema-run/lock"));
    assert_eq!(dir.state_file(), dir.dir.join("threema-run/state.json"));
    // The keeper root's own files are beside the profiles, never inside one.
    assert_eq!(dir.allow_file(), dir.keeper().join(ALLOW_FILE));
    assert_eq!(dir.keeper_lock_file(), dir.keeper().join(KEEPER_LOCK));
    assert_eq!(
        dir.open_question_file(),
        dir.keeper().join(OPEN_QUESTION_FILE)
    );
    assert_eq!(dir.backups_dir(), dir.keeper().join(BACKUPS_DIR));
    // The flags always name the place, the default profile too.
    assert_eq!(Where::default().flags(), ["--profile", "rt1"]);
    let named = Where {
        profile: Some("x".to_owned()),
        rehearsal_place: None,
    };
    assert_eq!(named.flags(), ["--profile", "x"]);
    let place = Where {
        profile: None,
        rehearsal_place: Some(PathBuf::from("/a/b/p")),
    };
    assert_eq!(place.flags(), ["--rehearsal-place", "/a/b/p"]);
}

/// The rehearsal's place is a profile directory below `<target>/thr/`, inside a keeper root of the
/// tool's own name: a kept profile, `..`, the base itself, another depth of directory and a bad name
/// are refused.
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
    assert_eq!(dir.label(), "p (rehearsal)");
    let base = target.join(REHEARSAL_DIR).join("1/x");
    for bad in [
        target.join(REHEARSAL_DIR),
        base.join(KEEPER_DIR),
        target.join("elsewhere/x").join(KEEPER_DIR).join("p"),
        base.join("other").join("p"),
        target
            .join(REHEARSAL_DIR)
            .join("1/x/../../..")
            .join(KEEPER_DIR)
            .join("p"),
        base.join(KEEPER_DIR).join("58301947"),
        base.join(KEEPER_DIR).join(ALLOW_FILE),
        base.join(KEEPER_DIR).join(BACKUPS_DIR),
        PathBuf::from("/home/someone/.local/share")
            .join(KEEPER_DIR)
            .join("rt1"),
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
        profile: Some("rt1".to_owned()),
        rehearsal_place: Some(ok),
    };
    assert!(error_of(both.resolve(&|_| None, target)).contains("give one of"));
}

#[test]
fn the_state_file_is_threema_runs_and_private() {
    let w = World::new("state");
    let dir = profile_dir(&w);
    assert!(State::load(&dir).expect("no file").is_none());
    let state = State::new(1234);
    ensure(&dir, &state).expect("ensure");
    assert_eq!(mode_of(&dir.dir), 0o700);
    assert_eq!(mode_of(&dir.tool()), 0o700);
    assert_eq!(mode_of(&dir.state_file()), 0o600);
    assert_eq!(mode_of(&w.keeper()), 0o700);
    assert_eq!(State::load(&dir).expect("loads"), Some(state.clone()));
    let text = std::fs::read_to_string(dir.state_file()).expect("text");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        value,
        json!({
            "version": 1, "created_ms": 1234, "starts": 0, "build_commit": null,
            "migrations": {}, "core_schema": null, "linked_since_ms": null,
            "core_schema_at_link": null, "last_online_ms": null, "link_attempts": [],
            "link_at": null, "halted": null, "last_halt": null
        })
    );
    // Nothing in it is a digest or an ID's shape.
    assert!(!holds_a_digest(&text) && !text.contains(&id_shape()));
    // A fresh state is nothing linked, nothing halted, no link that stood.
    assert!(!state.linked() && !state.link_stood() && state.halt_kind().is_none());
    assert!(state.migrations().is_empty());
    // The setters and readers.
    let mut state = state;
    state.set("linked_since_ms", json!(5));
    state.set("link_at", json!(6));
    state.set("halted", json!({"kind": "link-refused", "at_ms": 7}));
    state.set("migrations", json!({"core/1.sql": "abc"}));
    assert!(state.linked() && state.link_stood());
    assert_eq!(state.num("linked_since_ms"), Some(5));
    assert_eq!(state.halt_kind(), Some("link-refused"));
    assert_eq!(
        state.migrations().get("core/1.sql").map(String::as_str),
        Some("abc")
    );
    state.save(&dir).expect("saves");
    assert_eq!(State::load(&dir).expect("loads"), Some(state));
    // `ensure` tightens a keeper root that was too open.
    std::fs::set_permissions(w.keeper(), std::fs::Permissions::from_mode(0o755)).expect("mode");
    ensure(&dir, &State::new(1)).expect("ensure");
    assert_eq!(mode_of(&w.keeper()), 0o700);
}

/// A state file that is not the tool's is refused, never adopted.
#[test]
fn a_state_file_that_is_not_the_tools_is_refused() {
    let w = World::new("state-foreign");
    let dir = profile_dir(&w);
    private_dir(&dir.tool()).expect("tool dir");
    for text in [
        "{\"version\":2,\"created_ms\":1,\"starts\":0,\"migrations\":{},\"link_attempts\":[]}",
        "{\"version\":1,\"starts\":0,\"migrations\":{},\"link_attempts\":[]}",
        "{\"version\":1,\"created_ms\":1,\"migrations\":{},\"link_attempts\":[]}",
        "{\"version\":1,\"created_ms\":1,\"starts\":0,\"migrations\":[],\"link_attempts\":[]}",
        "{\"version\":1,\"created_ms\":1,\"starts\":0,\"migrations\":{}}",
        "[]",
        "not json",
    ] {
        std::fs::write(dir.state_file(), text).expect("state");
        let error = error_of(State::load(&dir));
        assert!(!error.is_empty(), "{text}");
    }
}

/// The app takes a run profile only when its home is canonical and a direct child of the canonical
/// keeper root, so a place with a link on its way is refused before anything is made, and a place
/// that does not exist yet (canonical so far) is fine.
#[test]
fn a_place_with_a_link_on_its_way_is_refused_before_anything_is_made() {
    let w = World::new("canonical");
    check_canonical(&w.profile()).expect("a canonical place that does not exist yet");
    let link = w.scratch.0.join("data-link");
    std::os::unix::fs::symlink(w.data(), &link).expect("link");
    let dir = ProfileDir {
        dir: link.join(KEEPER_DIR).join("p"),
        rehearsal: false,
    };
    let error = error_of(ensure(&dir, &State::new(1)));
    assert!(
        error.contains("not canonical") && error.contains("Nothing was started"),
        "{error}"
    );
    assert!(
        !error.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path: {error}"
    );
    assert!(!w.profile().exists(), "nothing was made");
    // A `..` on the way is a place that is not canonical either.
    let dotted = ProfileDir {
        dir: w.keeper().join("..").join(KEEPER_DIR).join("p"),
        rehearsal: false,
    };
    assert!(error_of(ensure(&dotted, &State::new(1))).contains("not canonical"));
    assert!(!w.profile().exists());
    // The canonical one works.
    ensure(&profile_dir(&w), &State::new(1)).expect("ensure");
    assert!(w.profile().join("threema-run/state.json").exists());
}

/// The private writers: 0600 files through a rename (no temporary file is left), appended lines,
/// durable removal, JSON that is not there is `None`.
#[test]
fn the_private_writers_make_private_files() {
    let w = World::new("writers");
    let file = w.keeper().join("a.json");
    write_private(&file, "{\"a\":1}\n").expect("write");
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(read_json(&file).expect("json"), Some(json!({"a": 1})));
    write_private(&file, "{\"a\":2}\n").expect("again");
    assert_eq!(read_json(&file).expect("json"), Some(json!({"a": 2})));
    assert!(!w.keeper().join("a.tmp").exists());
    assert_eq!(
        read_json(&w.keeper().join("none.json")).expect("none"),
        None
    );
    std::fs::write(w.keeper().join("bad.json"), "{").expect("bad");
    assert!(read_json(&w.keeper().join("bad.json")).is_err());
    let log = w.keeper().join("log");
    append_line(&log, "one").expect("append");
    append_line(&log, "two").expect("append");
    assert_eq!(std::fs::read_to_string(&log).expect("log"), "one\ntwo\n");
    assert_eq!(mode_of(&log), 0o600);
    // Removal says whether there was a file; a directory is an error and stays.
    assert!(remove_synced(&log).expect("removed"));
    assert!(!log.exists());
    assert!(!remove_synced(&log).expect("nothing to remove"));
    std::fs::create_dir(w.keeper().join("dir")).expect("dir");
    assert!(remove_synced(&w.keeper().join("dir")).is_err());
    assert!(w.keeper().join("dir").exists());
    // `private_dir` tightens a directory that was open.
    std::fs::set_permissions(
        w.keeper().join("dir"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("mode");
    private_dir(&w.keeper().join("dir")).expect("tightened");
    assert_eq!(mode_of(&w.keeper().join("dir")), 0o700);
}

/// A dm-crypt device below the file system, found through the block device's own entries and the
/// devices it sits on (a fake sysfs tree), and nothing else.
#[test]
fn a_crypt_device_is_found_below_a_block_device() {
    let scratch = Scratch::new("sysfs");
    let sys = scratch.0.join("sys");
    let crypt = sys.join("slaves/dm-0");
    std::fs::create_dir_all(crypt.join("dm")).expect("dirs");
    std::fs::write(crypt.join("dm/uuid"), "CRYPT-LUKS2-0000-name\n").expect("uuid");
    assert_eq!(crypt_below(&sys, 0).as_deref(), Some("dm-crypt"));
    // A device mapper device that is no crypt one, and a plain disk.
    let lvm = scratch.0.join("lvm");
    std::fs::create_dir_all(lvm.join("dm")).expect("dirs");
    std::fs::write(lvm.join("dm/uuid"), "LVM-0000\n").expect("uuid");
    assert_eq!(crypt_below(&lvm, 0), None);
    assert_eq!(crypt_below(&scratch.0.join("nowhere"), 0), None);
    // The device's name is not in the answer.
    assert!(!crypt_below(&sys, 0).is_some_and(|s| s.contains("name")));
}

// ---- wipe ----------------------------------------------------------------------------------------------

/// Wipe deletes the profile directory and nothing else, and only with its flag. Without it nothing is
/// deleted (the list of test IDs included) and it says so; with it the keeper root, the backups and
/// the other profiles stay, and a list left in the keeper root goes with a warning.
#[test]
fn wipe_deletes_the_profile_and_only_with_its_flag() {
    let w = World::new("wipe");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    std::fs::write(dir.dir.join("kourier-data"), "x").expect("data");
    let sibling = w.keeper().join("other");
    std::fs::create_dir_all(&sibling).expect("sibling");
    std::fs::create_dir_all(dir.backups_dir()).expect("backups");
    std::fs::write(dir.backups_dir().join("copy"), "x").expect("backup");
    std::fs::write(w.allow_file(), "synthetic").expect("a leftover list");
    // Without the flag: nothing is deleted, in words, status 1.
    let (lines, status) = wipe_in(&dir, false, Duration::ZERO).expect("wipe");
    assert_eq!(status, 1);
    let text = lines.join("\n");
    assert!(
        text.contains("Nothing was deleted")
            && text.contains("--yes-delete-the-login")
            && text.contains("TEST phone")
            && text.contains("no account was linked"),
        "{text}"
    );
    assert!(
        !text.contains(w.scratch.0.to_str().expect("utf-8")),
        "a path: {text}"
    );
    assert!(dir.dir.join("kourier-data").exists() && dir.state_file().exists());
    assert!(w.allow_file().exists(), "the list stays without the flag");
    // With it: the profile is gone; the keeper root, the backups and the sibling are not; the list is.
    let (lines, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(lines.last().is_some_and(|l| l == "deleted."), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("left in the keeper root") && l.contains("removed")),
        "{lines:?}"
    );
    assert!(!dir.dir.exists() && !w.allow_file().exists());
    assert!(w.keeper().exists() && sibling.exists());
    assert!(dir.backups_dir().join("copy").exists());
    // A profile that is not there is an error, not a success.
    assert!(error_of(wipe_in(&dir, true, Duration::ZERO)).contains("nothing is deleted"));
}

/// With no list in the keeper root, `wipe` says nothing about one.
#[test]
fn wipe_with_no_list_says_nothing_of_a_list() {
    let w = World::new("wipe-nolist");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    let (lines, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(!lines.iter().any(|l| l.contains("test IDs")), "{lines:?}");
    assert!(!dir.dir.exists());
}

/// A list under a live keeper lock is not left over: another run owns it, and `wipe` leaves it.
#[test]
fn wipe_leaves_a_list_that_another_run_holds() {
    let w = World::new("wipe-held-list");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    std::fs::write(w.allow_file(), "synthetic").expect("a list");
    // Another run holds the keeper lock for as long as its list stands.
    let held = open_lock_file(&dir.keeper_lock_file()).expect("lock file");
    held.try_lock().expect("the test takes the lock");
    let (lines, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("another run holds the keeper lock")),
        "{lines:?}"
    );
    assert!(!dir.dir.exists());
    assert!(w.allow_file().exists(), "the other run's list stays");
    drop(held);
}

#[test]
fn wipe_never_deletes_a_directory_that_is_no_profile_of_this_tool() {
    let w = World::new("wipe-foreign");
    let dir = profile_dir(&w);
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
        &State::new(1),
    )
    .expect("ensure");
    let link = ProfileDir {
        dir: w.keeper().join("linked"),
        rehearsal: false,
    };
    std::os::unix::fs::symlink(&real, &link.dir).expect("link");
    assert!(error_of(wipe_in(&link, true, Duration::ZERO)).contains("nothing is deleted"));
    assert!(real.join("threema-run/state.json").exists());
}

#[test]
fn wipe_waits_for_nothing_and_refuses_a_profile_in_use() {
    let w = World::new("wipe-busy");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    // The supervisor holds the profile's lock for as long as it runs.
    let held = open_lock_file(&dir.lock_file()).expect("lock file");
    held.try_lock().expect("the test takes the lock");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("the app is running") && error.contains("Nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.exists());
    drop(held);
    wipe_in(&dir, true, Duration::ZERO).expect("free now");
    assert!(!dir.dir.exists());
}

/// LOW-5: `wipe` looks at the app's OWN lock too (as `backup` does): the user may have started the
/// kept profile by hand to read its chats, or an app outlives a supervisor that was killed. Its
/// databases and keys must not go under it.
#[test]
fn wipe_refuses_while_the_apps_own_lock_is_held() {
    let w = World::new("wipe-app-lock");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    let lock_file = crate::threema_run::preflight::app_lock_file(&dir);
    std::fs::create_dir_all(lock_file.parent().expect("a parent")).expect("the app's data");
    // The app holds its lock; the supervisor's own lock is free (the supervisor was killed).
    let held = open_lock_file(&lock_file).expect("lock file");
    held.try_lock().expect("the test is the app");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("still holds its own lock") && error.contains("Nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.exists() && dir.state_file().exists());
    drop(held);
    wipe_in(&dir, true, Duration::ZERO).expect("free now");
    assert!(!dir.dir.exists());
}

/// The open question of a failed link lives in the keeper root and a `wipe` cannot lose it: while it
/// stands, and while the state says a login stood at `linking`, the flag deletes nothing. Without the
/// flag the words come all the same.
#[test]
fn wipe_refuses_while_a_failed_links_question_is_open() {
    let w = World::new("wipe-question");
    let dir = profile_dir(&w);
    ensure(&dir, &State::new(1)).expect("ensure");
    std::fs::write(
        dir.open_question_file(),
        "{\"kind\":\"link-failed-after-hello\"}",
    )
    .expect("the question");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("question of a failed link is still open")
            && error.contains("Nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.exists() && dir.open_question_file().exists());
    let (_, status) = wipe_in(&dir, false, Duration::ZERO).expect("words");
    assert_eq!(status, 1);
    // Answered (the file is gone): a login that stood at `linking` with no end seen holds it too.
    std::fs::remove_file(dir.open_question_file()).expect("answered");
    let mut state = State::new(1);
    state.set("link_at", json!(5));
    state.save(&dir).expect("state");
    let error = error_of(wipe_in(&dir, true, Duration::ZERO));
    assert!(
        error.contains("stood at `linking`") && error.contains("Nothing is deleted"),
        "{error}"
    );
    assert!(dir.dir.exists());
    // Cleared at the link's end: the wipe goes through.
    state.set("link_at", json!(null));
    state.save(&dir).expect("state");
    let (_, status) = wipe_in(&dir, true, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(!dir.dir.exists());
}

/// A linked profile says so, and that the TEST phone still lists the device.
#[test]
fn wipe_of_a_linked_profile_says_what_the_phone_still_holds() {
    let w = World::new("wipe-linked");
    let dir = profile_dir(&w);
    let mut state = State::new(1);
    state.set("linked_since_ms", json!(2));
    ensure(&dir, &state).expect("ensure");
    let (lines, status) = wipe_in(&dir, false, Duration::ZERO).expect("wipe");
    assert_eq!(status, 1);
    let text = lines.join("\n");
    assert!(
        text.contains("an account was linked") && text.contains("keeps listing the linked device"),
        "{text}"
    );
}

/// `wipe` is a command line like the others: its flag is parsed, its profile resolved, and a
/// rehearsal's place can be wiped too (the rehearsal's wipe check); the keeper root stays.
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
    ensure(&dir, &State::new(1)).expect("ensure");
    let keeper = dir.keeper().to_owned();
    std::fs::write(keeper.join("sibling-file"), "synthetic").expect("file");
    let (_, status) = wipe_in(&dir, line.yes_delete, Duration::ZERO).expect("wipe");
    assert_eq!(status, 0);
    assert!(!dir.dir.exists() && keeper.join("sibling-file").exists());
}
